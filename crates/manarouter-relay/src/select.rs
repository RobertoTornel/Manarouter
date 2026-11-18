//! Step and account selection, and the hedged-race policy.
//!
//! # Why this is a separate module of pure functions
//!
//! Everything in this file is a decision: given a combo, a health map, a clock and a counter, which
//! step should be tried, which account should pay for it, and who won a race. None of it performs
//! I/O.
//!
//! That split is deliberate and it is the reason these decisions can be tested at all. Selection is
//! where routing bugs live — a demoted step coming back too early, a round-robin counter that skips,
//! a hedge that resolves before the higher-priority participant has had its chance — and every one of
//! those is a pure input-output question. Testing them through a live HTTP path would mean a network
//! and a flaky test; testing them here means a table of inputs and expected outputs.

use std::collections::HashMap;

use mr_core::dialect::{ComboStrategy, KeyStrategy};
use mr_core::ids::now_millis;
use mr_core::model::CooldownScope;
use mr_core::snapshot::{AccountPlan, ComboPlan, ProviderPlan, StepPlan};

/// Why an account is temporarily unusable.
///
/// The distinction drives the cooldown length and, more importantly, what the panel tells the
/// operator. A rate limit resolves itself; a rejected credential does not, and presenting the two
/// the same way sends someone to wait for a fix that requires their action.
///
/// `Serialize` spells this in snake_case for `GET /control/activity`, which is a machine reading
/// the relay's state. `as_str` keeps the hyphenated spelling the logs and the panel already use;
/// the two spellings are deliberate rather than accidental, so they are two methods rather than
/// one shared string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CooldownReason {
    /// The provider throttled us. Resolves on its own.
    RateLimited,
    /// A declared quota window is full. Resolves on its own.
    QuotaExhausted,
    /// The credential was rejected. **Never resolves without operator action.**
    AuthFailed,
    /// Repeated transient failures. The cooldown grows with each one.
    Backoff,
    /// The client's request is malformed (400, 404, 422…). No cooldown — the account is fine.
    InvalidRequest,
    /// A self-hosted backend is cold and starting up.
    ///
    /// This is the one reason whose wait does **not** grow: a container that is still booting is
    /// expected to reject the next probe, and doubling the wait on each rejection would schedule the
    /// retry after the container is already warm. See [`COLD_START_COOLDOWN_MS`].
    ColdStart,
    /// Local capacity for the target endpoint is exhausted.
    KvBudgetExceeded,
}

impl CooldownReason {
    /// Whether waiting can fix this.
    pub fn self_healing(self) -> bool {
        !matches!(
            self,
            CooldownReason::AuthFailed | CooldownReason::InvalidRequest
        )
    }

    /// The stable identifier, for logs and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate-limited",
            Self::QuotaExhausted => "quota-exhausted",
            Self::AuthFailed => "auth-failed",
            Self::Backoff => "backoff",
            Self::InvalidRequest => "invalid-request",
            Self::ColdStart => "cold-start",
            Self::KvBudgetExceeded => "kv-budget-exceeded",
        }
    }
}

/// A temporary unavailability, with the instant it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Cooldown {
    /// Wall-clock milliseconds at which the account becomes usable again.
    pub until_ms: u64,
    /// Why it was taken out of rotation.
    pub reason: CooldownReason,
    /// Consecutive failures so far, used to grow the next cooldown.
    pub failures: u32,
}

impl Cooldown {
    /// Whether this cooldown has elapsed.
    pub fn is_over(&self, now_ms: u64) -> bool {
        now_ms >= self.until_ms
    }
}

/// Account health, as the relay observes it.
///
/// Read-mostly and swapped whole. A request loads the map once and passes the same view down through
/// selection, so a cooldown that lands mid-request cannot make one step see two different answers.
#[derive(Debug, Default)]
pub struct Health {
    map: arc_swap::ArcSwap<HashMap<String, Cooldown>>,
}

impl Health {
    /// An empty health map: everything available.
    pub fn new() -> Self {
        Self::default()
    }

    /// A consistent view, held for the duration of a request.
    pub fn view(&self) -> arc_swap::Guard<std::sync::Arc<HashMap<String, Cooldown>>> {
        self.map.load()
    }

    /// A copy of the cooldown map, for the control endpoint.
    ///
    /// A copy rather than the guard because the caller serialises it: holding the guard across a
    /// JSON document would keep a whole generation alive for the length of a panel request. The
    /// map is small — one entry per account currently out of rotation — so this is a handful of
    /// strings, not a scan of the routing table.
    pub fn snapshot(&self) -> HashMap<String, Cooldown> {
        self.map.load().as_ref().clone()
    }

    /// Take an account out of rotation until `until_ms`.
    ///
    /// The failure count **carries forward** across a cooldown rather than resetting. That is what
    /// stops the flap described in `docs/09-reset-aware-priority.md`: an account that fails, cools
    /// down, and fails again is at two consecutive failures, so three strikes really do mean three.
    /// Resetting here would let a permanently throttled account retry forever at full rate.
    pub fn cool_down(&self, account_id: &str, until_ms: u64, reason: CooldownReason) {
        let now_ms = now_millis().max(0) as u64;
        self.map.rcu(|current| {
            // A cooldown that has elapsed is information the map no longer needs: it changes no
            // decision, and keeping it would grow the map without bound. A provider with many
            // models and accounts parks a model-shaped key per refusal, and those entries never
            // get cleared by a success on a sibling route — so without pruning, a long-running
            // relay accumulates thousands of dead entries. Expired entries are dropped once the
            // map has grown past [`COOLDOWN_MAP_SAFE_SIZE`], which bounds the map while leaving a
            // small one alone (and with it the failure counts that older tests and the flap
            // guard rely on carrying across expired parks).
            let prune = current.len() >= COOLDOWN_MAP_SAFE_SIZE;
            let mut next = HashMap::with_capacity(current.len() + 1);
            for (key, value) in current.iter() {
                if !prune || !value.is_over(now_ms) {
                    next.insert(key.clone(), *value);
                }
            }
            let failures = next.get(account_id).map_or(1, |c| c.failures + 1);
            next.insert(
                account_id.to_owned(),
                Cooldown {
                    until_ms,
                    reason,
                    failures,
                },
            );
            // `rcu` swaps whole values, so the closure returns a fresh `Arc`. Building a new map
            // rather than mutating in place is what makes a reader holding the old view keep a
            // consistent one for the length of its request.
            std::sync::Arc::new(next)
        });
    }

    /// Record a success on one route, clearing the cooldown and failure count for every shape that
    /// could have parked it.
    ///
    /// Every shape, not only the configured one: the request proves the credential answers **and**
    /// that this model answers on it, so leaving any entry in place would keep skipping a route that
    /// has just demonstrated it works. The keys here must stay the same set
    /// [`account_available_for`] reads — a success that cleared only the configured shape would
    /// leave a narrower park in place, and the route would keep failing with nothing left to learn.
    pub fn record_success(&self, provider_id: &str, account_id: &str, model: &str) {
        let keys = [
            account_id.to_owned(),
            model_key(account_id, model),
            provider_key(provider_id, model),
        ];
        self.map.rcu(|current| {
            if keys.iter().all(|key| !current.contains_key(key)) {
                return current.clone();
            }
            let mut next = HashMap::with_capacity(current.len());
            next.extend(current.iter().map(|(k, v)| (k.clone(), *v)));
            for key in &keys {
                next.remove(key);
            }
            std::sync::Arc::new(next)
        });
    }

    /// Clear everything recorded against one account, for the operator's "try it now" action.
    ///
    /// The prefix covers the model-shaped keys as well: "try this account now" means every route on
    /// it. A provider-shaped key is deliberately left alone — it belongs to the provider's quota
    /// rather than to this credential, and clearing it would be claiming a recovery that this
    /// account cannot demonstrate.
    pub fn clear(&self, account_id: &str) {
        let prefix = format!("{account_id}{SCOPE_SEPARATOR}");
        self.map.rcu(|current| {
            let keep = current.len();
            let mut next = HashMap::with_capacity(keep);
            next.extend(
                current
                    .iter()
                    .filter(|(key, _)| key.as_str() != account_id && !key.starts_with(&prefix))
                    .map(|(k, v)| (k.clone(), *v)),
            );
            if next.len() == keep {
                return current.clone();
            }
            std::sync::Arc::new(next)
        });
    }

    /// How many consecutive failures this route has recorded: the worst of the shapes that can park
    /// it, so a model-scoped failure keeps growing rather than restarting at one each time.
    ///
    /// The count carries forward across cooldowns and resets only on a success, so it is the
    /// honest input to `cooldown_until`'s growth. Callers used to pass a hard-coded zero, which
    /// silently disabled the growth: an account that failed fifteen times in a row was parked
    /// for the same minute as one that failed once, and the combo re-walked it on every request.
    pub fn failures(
        &self,
        scope: CooldownScope,
        provider_id: &str,
        account_id: &str,
        model: &str,
    ) -> u32 {
        let map = self.map.load();
        let own = map.get(account_id).map_or(0, |c| c.failures);
        match wider_key(scope, provider_id, account_id, model)
            .and_then(|key| map.get(&key).map(|c| c.failures))
        {
            Some(wider) => own.max(wider),
            None => own,
        }
    }

    /// The number of accounts currently cooling down.
    pub fn cooling_count(&self) -> usize {
        self.map.load().len()
    }

    /// The soonest instant at which any cooling account becomes usable, if any are.
    ///
    /// Used to derive a `Retry-After` for the relay's own 429. Answering with the earliest possible
    /// recovery is the honest value: a client that waits that long will not come back before
    /// something can serve it.
    pub fn earliest_reset(&self) -> Option<u64> {
        // Only self-healing cooldowns count. An auth failure ends when the operator fixes the
        // credential, not when the clock passes; advertising its instant as a `Retry-After` would
        // promise a recovery that waiting cannot deliver.
        self.map
            .load()
            .values()
            .filter(|c| c.reason.self_healing())
            .map(|c| c.until_ms)
            .min()
    }
}

/// The size past which the cooldown map prunes expired entries on write.
///
/// See [`Health::cool_down`]: below this, entries (expired or not) are kept so failure counts
/// carry across parks; above it, expired entries are dropped to bound memory.
const COOLDOWN_MAP_SAFE_SIZE: usize = 1_024;

/// The separator between the parts of a scoped cooldown key.
///
/// `\u{1f}` (unit separator): neither an account id (a UUID, or a `mig-…` migration marker) nor a
/// model id contains a control character, so splitting on it is unambiguous — and an account-scoped
/// key stays the bare account id it has always been, which keeps existing entries, log lines and the
/// panel's `cooldowns` object readable across the change.
const SCOPE_SEPARATOR: char = '\u{1f}';

/// The prefix that marks a cooldown recorded against a whole provider rather than one credential.
const PROVIDER_PREFIX: char = '\u{1e}';

/// The model-shaped key: this credential, for this one model.
fn model_key(account_id: &str, model: &str) -> String {
    format!("{account_id}{SCOPE_SEPARATOR}{model}")
}

/// The provider-shaped key: every credential of this provider, for this one model.
fn provider_key(provider_id: &str, model: &str) -> String {
    format!("{PROVIDER_PREFIX}{provider_id}{SCOPE_SEPARATOR}{model}")
}

/// Whether the cooldown recorded under `key` has elapsed, or there is none.
fn cooldown_over(health: &HashMap<String, Cooldown>, key: &str, now_ms: u64) -> bool {
    match health.get(key) {
        None => true,
        Some(c) => c.is_over(now_ms),
    }
}

/// The extra key `scope` adds on top of the bare account id, if it adds one.
///
/// The account shape needs no entry here: it is the shape an authentication failure is recorded under
/// whatever the scope says, because a rejected credential is a fact about the account and not about
/// one model. Returning `None` for `account` is what keeps the default scope's lookups to one
/// borrowed `&str` and no allocation on the request path.
fn wider_key(
    scope: CooldownScope,
    provider_id: &str,
    account_id: &str,
    model: &str,
) -> Option<String> {
    match scope {
        CooldownScope::Account => None,
        CooldownScope::Model => Some(model_key(account_id, model)),
        CooldownScope::Provider => Some(provider_key(provider_id, model)),
    }
}

/// The key a refusal on one route is recorded under.
///
/// Two cases ignore the scope, because they describe the credential rather than its limits: an
/// authentication failure is recorded against the account — parking one model would leave the same
/// rejection waiting on every other one — and a malformed request is not recorded at all, which the
/// caller enforces by returning before it reaches here.
pub fn cooldown_key(
    scope: CooldownScope,
    provider_id: &str,
    account_id: &str,
    model: &str,
    reason: CooldownReason,
) -> String {
    match wider_key(scope, provider_id, account_id, model) {
        Some(key) if reason != CooldownReason::AuthFailed => key,
        _ => account_id.to_owned(),
    }
}

/// Whether an account is usable right now, considering only the account-shaped key.
///
/// Takes the map by reference rather than loading it itself, so one request resolves every account
/// against a single consistent view.
pub fn account_available(
    account_id: &str,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
) -> bool {
    cooldown_over(health, account_id, now_ms)
}

/// Whether an account can serve one model right now.
///
/// Every shape that can park this route is consulted — the credential, the model on that credential,
/// and the model across the provider — rather than only the provider's configured `cooldown_scope`.
/// The configured scope is a *ceiling* for failures whose cause is unknown; a refusal that plainly
/// concerns one model is recorded at the narrower shape, whatever that ceiling is. A reader that
/// consulted only the ceiling would therefore miss the entry it had just written and send the next
/// request to an upstream that has already refused it, which is how a plan refusal became an
/// endless stream of `429`s instead of a parked model. Consulting every shape cannot widen a park:
/// it only makes a narrow one visible.
pub fn account_available_for(
    provider_id: &str,
    account_id: &str,
    model: &str,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
) -> bool {
    // Nothing is parked, which is the ordinary case: skipping the composed keys keeps the request
    // path free of allocation while the map is empty.
    if health.is_empty() {
        return true;
    }
    cooldown_over(health, account_id, now_ms)
        && cooldown_over(health, &model_key(account_id, model), now_ms)
        && cooldown_over(health, &provider_key(provider_id, model), now_ms)
}

/// The instant at which a route's own cooldowns have all elapsed, or `None` when it has none.
///
/// The same three shapes [`account_available_for`] consults, for the same reason, and the latest of
/// them rather than the earliest: a route is usable only when nothing about it is still cooling, and
/// reporting the soonest expiry of two would promise a service that the other one still refuses.
pub fn route_reset_instant(
    provider_id: &str,
    account_id: &str,
    model: &str,
    health: &HashMap<String, Cooldown>,
) -> Option<u64> {
    [
        account_id.to_owned(),
        model_key(account_id, model),
        provider_key(provider_id, model),
    ]
    .iter()
    .filter_map(|key| health.get(key).map(|c| c.until_ms))
    .max()
}

/// Whether a step has at least one usable account for `model`.
pub fn step_available(
    provider: &ProviderPlan,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
    model: &str,
) -> bool {
    // A step whose provider exists but has no accounts at all is unavailable. That case is real: an
    // operator adds a provider, adds a combo step for it, and has not yet attached a credential.
    // Treating it as available would send a request with no credential and return the provider's
    // 401, which reads as an authentication bug rather than as a missing setup step.
    if provider.accounts.is_empty() {
        return false;
    }
    provider
        .accounts
        .iter()
        .any(|a| account_available_for(provider.id.as_str(), a.id.as_str(), model, health, now_ms))
}

/// Whether `step` may use `account`.
///
/// A step that names no selection may use every account, which is the default and what every direct
/// `provider/model` resolution carries. A step that names one may use only those accounts, and a
/// selection that resolved to nothing may use none — the operator asked for accounts that are not
/// available, and widening back to everyone would send the request from an account they excluded.
pub fn step_allows_account(step: &StepPlan, account: &AccountPlan) -> bool {
    match &step.accounts {
        None => true,
        Some(selected) => selected.iter().any(|id| id == &account.id),
    }
}

/// Whether a step has at least one usable account that allows the requested model.
pub fn step_has_usable_account(
    provider: &ProviderPlan,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
    step: &StepPlan,
) -> bool {
    provider.accounts.iter().any(|a| {
        step_allows_account(step, a)
            && account_allows_model(a, &step.model)
            && account_available_for(
                provider.id.as_str(),
                a.id.as_str(),
                &step.model,
                health,
                now_ms,
            )
    })
}

/// The order in which a combo's steps should be tried, as indices into `combo.steps`.
///
/// # The strategy rules
///
/// * `priority` — the operator's order, with steps that have no usable account moved to the back.
/// * `round-robin` — the operator's order, rotated by the combo's counter.
/// * `reset-aware-priority` — the operator's order, stable-partitioned so that unavailable steps keep
///   their relative order and go after every healthy one, ordered by earliest reset.
///
/// A step whose provider cannot be resolved at all is treated as unavailable. It cannot serve a
/// request, and leaving it in place would mean every request pays a failed lookup before moving on.
pub fn order_steps(
    combo: &ComboPlan,
    providers: &[ProviderPlan],
    health: &HashMap<String, Cooldown>,
    counter: u64,
    now_ms: u64,
) -> Vec<usize> {
    let n = combo.steps.len();
    // Proxy eligibility belongs here: once proxy selection is available, combine it with account
    // health before applying priority/round-robin, but keep each candidate as its resolved step
    // index. A `(provider, model, level)` sibling is a distinct route and must never be deduped by
    // the bare upstream model id.
    let available: Vec<bool> = (0..n)
        .map(
            |i| match providers.get(combo.steps[i].provider_index as usize) {
                Some(p) => step_available(p, health, now_ms, &combo.steps[i].model),
                None => false,
            },
        )
        .collect();

    match combo.strategy {
        ComboStrategy::Priority => stable_partition_by(&available, true),
        ComboStrategy::RoundRobin => rotate_by(&stable_partition_by(&available, true), counter),
        ComboStrategy::ResetAwarePriority => {
            // Healthy steps keep the operator's order; unavailable steps follow, soonest reset first.
            // The sort is stable and only ever reorders *within* the unavailable group, so the
            // operator's preference survives inside both groups.
            let mut healthy = Vec::with_capacity(n);
            let mut unavailable = Vec::with_capacity(n);
            for (i, ok) in available.iter().enumerate() {
                if *ok {
                    healthy.push(i);
                } else {
                    unavailable.push(i);
                }
            }
            unavailable.sort_by_key(|&i| step_earliest_reset(&combo.steps[i], providers, health));
            healthy.extend(unavailable);
            healthy
        }
    }
}

/// The earliest instant at which a step could become usable.
///
/// `u64::MAX` when nothing is known, which sorts a step with no known reset last. That is the honest
/// position: an account with no known reset is less likely to recover soon than one with a stated
/// instant, and it still stays in the list either way, so the ordering degrades but correctness does
/// not.
fn step_earliest_reset(
    step: &StepPlan,
    providers: &[ProviderPlan],
    health: &HashMap<String, Cooldown>,
) -> u64 {
    let Some(provider) = providers.get(step.provider_index as usize) else {
        return u64::MAX;
    };
    // Accounts that forbid this model are not candidates for it, so their cooldowns say nothing
    // about when the step can serve it.
    provider
        .accounts
        .iter()
        .filter(|a| account_allows_model(a, &step.model))
        .filter_map(|a| {
            route_reset_instant(provider.id.as_str(), a.id.as_str(), &step.model, health)
        })
        .min()
        .unwrap_or(u64::MAX)
}

/// Indices where `keep[i]` is true, in order, followed by those where it is false.
///
/// Written by hand rather than as a `sort_by_key`, because a sort is only stable by convention and a
/// future change to `sort_unstable_by` would silently permute the operator's priority order. Here the
/// property is in the loop rather than in the choice of function.
fn stable_partition_by(keep: &[bool], first: bool) -> Vec<usize> {
    let mut yes = Vec::with_capacity(keep.len());
    let mut no = Vec::with_capacity(keep.len());
    for (i, k) in keep.iter().enumerate() {
        if *k == first {
            yes.push(i);
        } else {
            no.push(i);
        }
    }
    yes.extend(no);
    yes
}

/// Rotate a list left by `by`, leaving an empty list alone.
fn rotate_by(items: &[usize], by: u64) -> Vec<usize> {
    if items.is_empty() {
        return Vec::new();
    }
    let shift = (by % items.len() as u64) as usize;
    let mut out = Vec::with_capacity(items.len());
    out.extend_from_slice(&items[shift..]);
    out.extend_from_slice(&items[..shift]);
    out
}

/// Every account a step may use, in the order the key strategy wants them tried.
///
/// `counter` is the step's own rotation counter, so that two steps on the same provider do not
/// advance each other's position.
///
/// Health and the model lists filter here; the strategy orders what remains, and the first entry is
/// [`pick_account`]'s choice. The whole list is returned because health is not the only reason an
/// account can be unusable: an account whose credential is absent from the vault, or whose OAuth
/// refresh was rejected, cannot serve this request either, and skipping it to the next candidate is
/// the same decision with the same ordering. A caller that discovers such a reason must walk this
/// list rather than invent a second ordering that could disagree with it.
///
/// An empty list means every account is cooling down or forbidden this model, which the caller must
/// treat as "this step cannot serve" rather than as an error — the next step in the order is the
/// right answer.
pub fn account_candidates<'a>(
    provider: &'a ProviderPlan,
    strategy: KeyStrategy,
    counter: u64,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
    model: &str,
) -> Vec<&'a AccountPlan> {
    account_candidates_with(provider, strategy, counter, health, now_ms, model, true)
}

/// The same ordered list, but with the cooldown filter left out.
///
/// A cooldown is a prediction about the near future, not a fact about now: an account that was
/// throttled a minute ago may answer perfectly well. When every step of a combo is cooling down,
/// refusing the request without trying anything is the worst of the available answers, so the
/// caller uses this to make one last attempt on the step that recovers soonest. The model filter
/// still applies — an account that is not allowed to serve this model will not start being allowed
/// because the request was desperate.
///
/// The order is the recovery order, not the operator's: the caller is about to fail over across
/// these accounts one by one, and the one that recovers soonest is the likeliest to answer —
/// operator order would spend the first attempt on whichever account happens to be configured
/// first, every time, and the retry loop would only reach a sibling the operator had already
/// fixed once that account's park expired. An account with no cooldown entry recovers "now", and
/// goes first.
pub fn account_candidates_ignoring_cooldowns<'a>(
    provider: &'a ProviderPlan,
    strategy: KeyStrategy,
    counter: u64,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
    model: &str,
) -> Vec<&'a AccountPlan> {
    let mut candidates =
        account_candidates_with(provider, strategy, counter, health, now_ms, model, false);
    candidates.sort_by_key(|account| {
        route_reset_instant(provider.id.as_str(), account.id.as_str(), model, health).unwrap_or(0)
    });
    candidates
}

fn account_candidates_with<'a>(
    provider: &'a ProviderPlan,
    strategy: KeyStrategy,
    counter: u64,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
    model: &str,
    respect_cooldowns: bool,
) -> Vec<&'a AccountPlan> {
    // Candidates in the operator's order. `priority` is expressed by the order of the account list
    // in the configuration, so nothing here needs to sort for that strategy.
    let mut candidates: Vec<&AccountPlan> = provider
        .accounts
        .iter()
        .filter(|a| {
            !respect_cooldowns
                || account_available_for(provider.id.as_str(), a.id.as_str(), model, health, now_ms)
        })
        .filter(|a| account_allows_model(a, model))
        .collect();

    match strategy {
        KeyStrategy::Priority => {}
        KeyStrategy::RoundRobin => rotate_candidates(&mut candidates, counter),
        KeyStrategy::Random => {
            // Seeded from the counter rather than from the OS, so a test can be deterministic and a
            // production call is still unpredictable. Pulling entropy on the request path would be a
            // syscall per request for a decision that does not need it.
            let mut x = counter
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(now_ms);
            x ^= x >> 30;
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            x ^= x >> 27;
            rotate_candidates(&mut candidates, x);
        }
        KeyStrategy::LeastUsed => {
            // `max_concurrent` is the only load signal the snapshot carries, and it is a *capacity*,
            // not a count of in-flight requests. Using it as a tie-break keeps the pool fresh when
            // capacities differ, and it is honest about being a preference rather than a
            // measurement — a real least-used needs a counter the relay does not keep per account.
            candidates.sort_by_key(|a| std::cmp::Reverse(a.max_concurrent));
        }
    }
    candidates
}

/// Rotate a candidate list so the strategy's next choice is first, leaving an empty list alone.
fn rotate_candidates(accounts: &mut [&AccountPlan], by: u64) {
    if accounts.is_empty() {
        return;
    }
    let shift = (by % accounts.len() as u64) as usize;
    accounts.rotate_left(shift);
}

/// Choose an account for a step.
///
/// This is the first of [`account_candidates`]'s ordered list: the same filtering and the same
/// strategy ordering, so a caller that needs to fall through to a later candidate is reading the
/// list this function read.
///
/// Returns `None` when every account is cooling down, which the caller must treat as "this step
/// cannot serve" rather than as an error — the next step in the order is the right answer.
pub fn pick_account<'a>(
    provider: &'a ProviderPlan,
    strategy: KeyStrategy,
    counter: u64,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
    model: &str,
) -> Option<&'a AccountPlan> {
    account_candidates(provider, strategy, counter, health, now_ms, model)
        .into_iter()
        .next()
}

/// Whether an account's allow/deny lists permit a model.
///
/// Deny wins over allow. An operator who adds a model to both lists has made a contradiction, and the
/// safe reading of a contradiction about spending money is the restrictive one.
pub fn account_allows_model(account: &AccountPlan, model: &str) -> bool {
    if account.models_deny.iter().any(|m| eq_model(m, model)) {
        return false;
    }
    if account.models_allow.is_empty() {
        return true;
    }
    account.models_allow.iter().any(|m| eq_model(m, model))
}

/// Model-id comparison, tolerant of a provider prefix on either side.
///
/// An operator writes `claude-sonnet-4-5` in one place and `anthropic/claude-sonnet-4-5` in another,
/// and both readings are meant. Comparing raw strings would make the allow-list silently not match,
/// which reads as the router ignoring the list rather than as a formatting detail.
fn eq_model(a: &str, b: &str) -> bool {
    // Borrowed, not owned: this runs once per allow/deny entry for every account considered on the
    // request path, and looking at the tail of a model id never needed an allocation.
    fn strip(s: &str) -> &str {
        match s.rsplit_once('/') {
            Some((_, tail)) => tail,
            None => s,
        }
    }
    a.eq_ignore_ascii_case(b) || strip(a).eq_ignore_ascii_case(strip(b))
}

/// What a hedge should do now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HedgeDecision {
    /// Participant `index` (an index into the race's own order) has won.
    Resolved(usize),
    /// Keep waiting; a higher-priority participant is still unresolved.
    Waiting,
    /// The window has expired and nobody replied inside it.
    NoWinner,
}

/// The state of one race participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Participant {
    /// Milliseconds since the race started, when the response arrived. `None` while unresolved.
    pub arrived_at_ms: Option<u64>,
    /// Whether the participant has failed and will never arrive.
    pub failed: bool,
}

impl Participant {
    /// A participant that has not yet replied.
    pub const PENDING: Self = Self {
        arrived_at_ms: None,
        failed: false,
    };

    /// A participant that replied after `ms`.
    pub const fn arrived(ms: u64) -> Self {
        Self {
            arrived_at_ms: Some(ms),
            failed: false,
        }
    }

    /// A participant that will never reply.
    pub const FAILED: Self = Self {
        arrived_at_ms: None,
        failed: true,
    };

    /// Whether this participant's outcome is settled.
    pub fn is_resolved(&self) -> bool {
        self.failed || self.arrived_at_ms.is_some()
    }

    /// Whether it replied inside the eligibility window.
    pub fn replied_in_window(&self, window_ms: u64) -> bool {
        matches!(self.arrived_at_ms, Some(t) if t <= window_ms)
    }
}

/// Decide a race.
///
/// `participants` is in **priority order**, highest priority first. `window_ms` is the eligibility
/// window, and `elapsed_ms` is how long the race has been running.
///
/// # The rules, and why they are these rules
///
/// * The winner is the **highest-priority** participant that replied inside the window — not the
///   fastest. A race that rewarded raw speed would make the operator's ordering decorative.
/// * If the top participant replies inside the window, the race resolves **immediately**. Waiting for
///   the others would be waiting for a decision already made, and the user is waiting with it.
/// * The window is an **eligibility gate, not a timeout.** Its expiry never fails the request; it
///   only means nobody won on priority, and the caller falls back to its failure mode.
/// * A decision commits once every higher-priority participant is **resolved** — either because it
///   replied or because it failed. A lower-priority participant cannot win while a higher-priority
///   one might still reply in time.
pub fn hedge_decide(
    participants: &[Participant],
    window_ms: u64,
    elapsed_ms: u64,
) -> HedgeDecision {
    if participants.is_empty() {
        return HedgeDecision::NoWinner;
    }

    // The best candidate: the first participant, in priority order, that replied inside the window.
    let candidate = participants
        .iter()
        .position(|p| p.replied_in_window(window_ms));

    if let Some(index) = candidate {
        // Everyone above the candidate must be settled before we commit. A participant above it that
        // is still pending could yet reply inside the window and would then outrank it.
        let settled_above = participants[..index].iter().all(|p| p.is_resolved());
        if settled_above {
            return HedgeDecision::Resolved(index);
        }
    }

    // Window expired with no eligible reply: the race has no priority winner. Note this is checked
    // only after the candidate search, so a participant that replied inside the window is never
    // discarded because time passed while a higher-priority one was still pending.
    if elapsed_ms >= window_ms {
        // A reply that arrived after the window is still a reply. If nothing eligible exists, the
        // best available answer is the highest-priority participant that replied at all, because the
        // alternative — failing the request — throws away a working response.
        if let Some(index) = participants.iter().position(|p| p.arrived_at_ms.is_some()) {
            let settled_above = participants[..index].iter().all(|p| p.is_resolved());
            if settled_above {
                return HedgeDecision::Resolved(index);
            }
        }
        // Everyone above still pending and the window is gone, so nothing above can win on
        // eligibility any more; if some participant has replied, take the best of them.
        if participants.iter().any(|p| p.arrived_at_ms.is_some()) {
            if let Some(index) = participants.iter().position(|p| p.arrived_at_ms.is_some()) {
                return HedgeDecision::Resolved(index);
            }
        }
        if participants.iter().all(|p| p.is_resolved()) {
            return HedgeDecision::NoWinner;
        }
        return HedgeDecision::Waiting;
    }

    HedgeDecision::Waiting
}

/// How long an account should be cooled down after a failure.
///
/// Returns the instant, so callers do not each reinvent the arithmetic, and so the growing backoff
/// lives in one place.
/// How long a cold self-hosted backend is left alone before the next attempt.
///
/// A calibrated default, not a measurement. Modal's `scaledown_window` default is 60 s and a vLLM
/// container that must load weights commonly takes 60-120 s to answer its first request, while the
/// relay's own ceiling for a `modal_vllm` first byte is 300 s (`cold_start_ttfb_timeout_ms`). 90 s is
/// the shortest window that outlasts the common boot without spending most of that ceiling asleep.
pub const COLD_START_COOLDOWN_MS: u64 = 90_000;

pub fn cooldown_until(
    reason: CooldownReason,
    previous_failures: u32,
    retry_after_ms: Option<u64>,
    config: &mr_core::config::QuotaConfig,
) -> u64 {
    // `now_millis` is signed because it is a wall clock; the cooldown arithmetic is not, and a
    // negative clock would otherwise saturate into a cooldown that never ends.
    let now = now_millis().max(0) as u64;
    let base = match reason {
        CooldownReason::AuthFailed => config.auth_failure_cooldown_secs.saturating_mul(1000),
        CooldownReason::InvalidRequest => 0,
        CooldownReason::ColdStart => COLD_START_COOLDOWN_MS,
        CooldownReason::RateLimited
        | CooldownReason::QuotaExhausted
        | CooldownReason::Backoff
        | CooldownReason::KvBudgetExceeded => config.default_cooldown_secs.saturating_mul(1000),
    };

    // A provider's own `retry-after` is knowledge and outranks the configured guess.
    let stated = config.live_signals_win.then_some(retry_after_ms).flatten();
    let chosen = stated.unwrap_or(base);

    // The backoff grows with repetition, because a provider that states nothing and rejects three
    // times running is not going to honour a fourth guess. Doubling is bounded by two doublings so
    // that a long-running account cannot end up cooling for a week.
    //
    // Two reasons are exempt. A cold start's rejections are one container still loading weights, so
    // growing the wait moves the retry past the moment it becomes usable. And a *stated* recovery
    // instant is an answer, not a guess: multiplying it turns a provider's "resets in 13h" into two
    // days of a healthy account sitting idle, which is precisely the opposite of what the statement
    // is for. If the provider breaks its own promise, the next refusal is free to cool it again.
    let growth = match reason {
        CooldownReason::ColdStart => 1,
        _ if stated.is_some() => 1,
        _ => 1u64 << previous_failures.min(2),
    };
    now.saturating_add(chosen.saturating_mul(growth))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::ids::ComboId;

    fn account(id: &str) -> AccountPlan {
        AccountPlan {
            id: mr_core::ids::AccountId::from_raw(id),
            label: None,
            secret_ref: Some(format!("sec-{id}")),
            oauth_ref: None,
            priority: 0,
            weight: 0,
            max_concurrent: 1,
            models_allow: Vec::new(),
            models_deny: Vec::new(),
            proxy: None,
            placeholders: Default::default(),
            credit_cents: None,
            quota: Vec::new(),
        }
    }

    fn provider(id: &str, accounts: Vec<AccountPlan>) -> ProviderPlan {
        ProviderPlan {
            id: mr_core::ids::ProviderId::from_raw(id),
            label: None,
            dialect: mr_core::dialect::Dialect::OpenAi,
            base_url: "https://example.com".into(),
            auth: mr_core::dialect::AuthKind::Bearer,
            key_strategy: KeyStrategy::Priority,
            cooldown_scope: CooldownScope::Account,
            accounts,
            proxy: None,
            capabilities: Vec::new(),
            connect_timeout_ms: 1000,
            ttfb_timeout_ms: 1000,
            idle_timeout_ms: 1000,
            total_timeout_ms: 1000,
            headers: Default::default(),
            route_overrides: Default::default(),
            http2: Default::default(),
            prewarm: false,
            kv_pool_tokens: None,
            max_context: None,
            max_concurrent: None,
            price_per_second: None,
            cold_start_ttfb_timeout_ms: None,
            steps: Vec::new(),
            hidden_models: Vec::new(),
        }
    }

    fn step(provider_index: u32, model: &str) -> StepPlan {
        StepPlan {
            provider_index,
            provider_id: mr_core::ids::ProviderId::from_raw(format!("p{provider_index}")),
            model: model.into(),
            dialect: mr_core::dialect::Dialect::OpenAi,
            auth: mr_core::dialect::AuthKind::Bearer,
            base_url: "https://example.com".into(),
            connect_timeout_ms: 1000,
            ttfb_timeout_ms: 1000,
            idle_timeout_ms: 1000,
            total_timeout_ms: 1000,
            level: mr_core::dialect::ReasoningLevel::High.rank(),
            levels: 0,
            default_level: mr_core::dialect::ReasoningLevel::High.rank(),
            reasoning_json: Default::default(),
            input_modalities: 1,
            pricing: None,
            context_length: None,
            accounts: None,
        }
    }

    fn combo(strategy: ComboStrategy, steps: Vec<StepPlan>) -> ComboPlan {
        ComboPlan {
            id: ComboId::generate(),
            name: "test".into(),
            strategy,
            steps,
            hedge: mr_core::snapshot::HedgePlan {
                enabled: false,
                participants: Vec::new(),
                window_ms: 0,
                winner_metric: Default::default(),
                on_winner_fail: Default::default(),
                cancel_losers: true,
                loser_grace_ms: 0,
            },
            midstream_policy: Default::default(),
            sticky_success_window_secs: 1800,
            max_retries: 0,
            retry_delay_ms: 0,
            affinity: false,
            compression: Default::default(),
            signature: 0,
        }
    }

    fn healthy() -> HashMap<String, Cooldown> {
        HashMap::new()
    }

    #[test]
    fn two_steps_for_the_same_model_at_distinct_levels_remain_independently_selectable() {
        let provider = provider("p0", vec![account("a0")]);
        let mut low = step(0, "reasoning-model");
        low.level = mr_core::dialect::ReasoningLevel::Low.rank();
        let mut high = step(0, "reasoning-model");
        high.level = mr_core::dialect::ReasoningLevel::High.rank();
        let plan = combo(ComboStrategy::Priority, vec![low, high]);

        // Candidate identity is its resolved step position, not the bare upstream model id. The
        // order is used by the caller to index `combo.steps`, which preserves each level's wire
        // fragment and retry/cooldown identity even when the provider/model pair is shared.
        assert_eq!(
            order_steps(&plan, &[provider], &healthy(), 0, 100),
            vec![0, 1]
        );
        assert_eq!(plan.steps[0].model, plan.steps[1].model);
        assert_ne!(plan.steps[0].level, plan.steps[1].level);
    }

    #[test]
    fn priority_keeps_the_operator_order_when_everything_is_healthy() {
        let c = combo(
            ComboStrategy::Priority,
            vec![step(0, "a"), step(1, "b"), step(2, "c")],
        );
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
            provider("p2", vec![account("a2")]),
        ];
        assert_eq!(order_steps(&c, &ps, &healthy(), 0, 0), vec![0, 1, 2]);
    }

    #[test]
    fn priority_moves_a_step_with_no_usable_account_to_the_back() {
        let c = combo(ComboStrategy::Priority, vec![step(0, "a"), step(1, "b")]);
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
        ];
        let mut h = HashMap::new();
        h.insert(
            "a0".to_owned(),
            Cooldown {
                until_ms: 5_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        assert_eq!(order_steps(&c, &ps, &h, 0, 1_000), vec![1, 0]);
    }

    #[test]
    fn reset_aware_is_identical_to_priority_when_everything_is_healthy() {
        // The property that makes the strategy safe to enable: with nothing cooling down it must be
        // indistinguishable from the simpler one.
        let steps = vec![step(0, "a"), step(1, "b"), step(2, "c")];
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
            provider("p2", vec![account("a2")]),
        ];
        let p = combo(ComboStrategy::Priority, steps.clone());
        let r = combo(ComboStrategy::ResetAwarePriority, steps);
        assert_eq!(
            order_steps(&p, &ps, &healthy(), 0, 0),
            order_steps(&r, &ps, &healthy(), 0, 0)
        );
    }

    #[test]
    fn reset_aware_demotes_instead_of_removing() {
        // The core rule: the demoted step is still in the list, so it is reachable when everything
        // healthy has failed.
        let c = combo(
            ComboStrategy::ResetAwarePriority,
            vec![step(0, "a"), step(1, "b")],
        );
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
        ];
        let mut h = HashMap::new();
        h.insert(
            "a0".to_owned(),
            Cooldown {
                until_ms: 5_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        let order = order_steps(&c, &ps, &h, 0, 1_000);
        assert_eq!(
            order,
            vec![1, 0],
            "the demoted step must still be present, last"
        );
        assert_eq!(order.len(), 2, "demotion is not removal");
    }

    #[test]
    fn reset_aware_orders_the_unavailable_group_by_earliest_reset() {
        let c = combo(
            ComboStrategy::ResetAwarePriority,
            vec![step(0, "a"), step(1, "b"), step(2, "c"), step(3, "d")],
        );
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
            provider("p2", vec![account("a2")]),
            provider("p3", vec![account("a3")]),
        ];
        let mut h = HashMap::new();
        // Step 0 resets late, step 1 resets soon, and step 3 is healthy.
        h.insert(
            "a0".to_owned(),
            Cooldown {
                until_ms: 9_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        h.insert(
            "a1".to_owned(),
            Cooldown {
                until_ms: 2_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        let order = order_steps(&c, &ps, &h, 0, 1_000);
        assert_eq!(order, vec![2, 3, 1, 0], "healthy first, then soonest reset");
    }

    #[test]
    fn a_step_whose_provider_has_no_accounts_is_unavailable() {
        // An operator who added a step before attaching a credential should not get a 401 from the
        // provider; the step should simply not be chosen.
        let c = combo(ComboStrategy::Priority, vec![step(0, "a"), step(1, "b")]);
        let ps = vec![
            provider("p0", Vec::new()),
            provider("p1", vec![account("a1")]),
        ];
        assert_eq!(order_steps(&c, &ps, &healthy(), 0, 0), vec![1, 0]);
    }

    #[test]
    fn round_robin_rotates_and_wraps() {
        let c = combo(
            ComboStrategy::RoundRobin,
            vec![step(0, "a"), step(1, "b"), step(2, "c")],
        );
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
            provider("p2", vec![account("a2")]),
        ];
        assert_eq!(order_steps(&c, &ps, &healthy(), 0, 0), vec![0, 1, 2]);
        assert_eq!(order_steps(&c, &ps, &healthy(), 1, 0), vec![1, 2, 0]);
        assert_eq!(order_steps(&c, &ps, &healthy(), 3, 0), vec![0, 1, 2]);
        assert_eq!(order_steps(&c, &ps, &healthy(), 7, 0), vec![1, 2, 0]);
    }

    #[test]
    fn an_expired_cooldown_no_longer_demotes() {
        let c = combo(
            ComboStrategy::ResetAwarePriority,
            vec![step(0, "a"), step(1, "b")],
        );
        let ps = vec![
            provider("p0", vec![account("a0")]),
            provider("p1", vec![account("a1")]),
        ];
        let mut h = HashMap::new();
        h.insert(
            "a0".to_owned(),
            Cooldown {
                until_ms: 5_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        assert_eq!(order_steps(&c, &ps, &h, 0, 4_999), vec![1, 0]);
        assert_eq!(order_steps(&c, &ps, &h, 0, 5_000), vec![0, 1]);
    }

    #[test]
    fn expired_cooldowns_are_pruned_on_the_next_write() {
        // A model-scoped park is cleared only by a success on that exact route, so a long-running
        // relay accumulates expired entries. Below the safe size they are kept (their failure
        // counts are still useful), and past it they are dropped when something is written, so
        // the map stays bounded by the number of *live* parks plus the threshold.
        let h = Health::new();
        for i in 0..COOLDOWN_MAP_SAFE_SIZE as u64 {
            h.cool_down(&format!("a{i}"), 1_000, CooldownReason::RateLimited);
        }
        // Every seed has expired; the next write crosses the threshold and prunes them.
        h.cool_down("live", 99_000_000, CooldownReason::RateLimited);
        let view = h.view();
        assert_eq!(
            view.len(),
            1,
            "expired entries must be pruned once the map is large"
        );
        assert!(view.contains_key("live"));
        assert!(view.get("live").unwrap().until_ms >= 99_000_000);
    }

    #[test]
    fn a_failure_count_carries_forward_across_a_cooldown() {
        // This is what stops the flap: three strikes must mean three, not one per cooldown.
        let h = Health::new();
        h.cool_down("a", 1_000, CooldownReason::RateLimited);
        assert_eq!(h.view().get("a").unwrap().failures, 1);
        h.cool_down("a", 2_000, CooldownReason::RateLimited);
        assert_eq!(h.view().get("a").unwrap().failures, 2);
        h.cool_down("a", 3_000, CooldownReason::RateLimited);
        assert_eq!(h.view().get("a").unwrap().failures, 3);
    }

    #[test]
    fn the_failure_count_is_readable_so_the_cooldown_can_grow_with_it() {
        // The relay used to pass a hard-coded zero to `cooldown_until`, which disabled the
        // growth entirely: a permanently dead account was parked for the same minute as one
        // that had failed once, and every request re-walked it. The count has to be readable
        // for the growth to be reachable at all.
        let h = Health::new();
        assert_eq!(
            h.failures(CooldownScope::Account, "p", "a", "m"),
            0,
            "an account never seen has no failures"
        );
        h.cool_down("a", 1_000, CooldownReason::RateLimited);
        assert_eq!(h.failures(CooldownScope::Account, "p", "a", "m"), 1);
        h.cool_down("a", 2_000, CooldownReason::RateLimited);
        assert_eq!(h.failures(CooldownScope::Account, "p", "a", "m"), 2);
        h.record_success("p", "a", "m");
        assert_eq!(h.failures(CooldownScope::Account, "p", "a", "m"), 0);
    }

    #[test]
    fn a_success_clears_the_failure_count() {
        let h = Health::new();
        h.cool_down("a", 1_000, CooldownReason::RateLimited);
        h.record_success("p", "a", "m");
        assert_eq!(h.cooling_count(), 0);
        h.cool_down("a", 2_000, CooldownReason::RateLimited);
        assert_eq!(
            h.view().get("a").unwrap().failures,
            1,
            "a success resets the count, which is what makes it *consecutive* failures"
        );
    }

    #[test]
    fn earliest_reset_is_the_minimum_over_cooling_accounts() {
        let h = Health::new();
        h.cool_down("a", 5_000, CooldownReason::RateLimited);
        h.cool_down("b", 2_000, CooldownReason::RateLimited);
        h.cool_down("c", 9_000, CooldownReason::RateLimited);
        assert_eq!(h.earliest_reset(), Some(2_000));
    }

    #[test]
    fn an_auth_failure_is_not_self_healing() {
        assert!(!CooldownReason::AuthFailed.self_healing());
        assert!(CooldownReason::RateLimited.self_healing());
        assert!(CooldownReason::Backoff.self_healing());
        assert!(CooldownReason::QuotaExhausted.self_healing());
    }

    #[test]
    fn an_auth_failure_and_a_rate_limit_produce_very_different_cooldowns() {
        let config = mr_core::config::QuotaConfig {
            default_cooldown_secs: 60,
            auth_failure_cooldown_secs: 900,
            ..Default::default()
        };
        // `now_millis` is signed because it is a wall clock; the cooldown arithmetic is not, and a
        // negative clock would otherwise saturate into a cooldown that never ends.
        let now = now_millis().max(0) as u64;
        let rate = cooldown_until(CooldownReason::RateLimited, 0, None, &config);
        let auth = cooldown_until(CooldownReason::AuthFailed, 0, None, &config);
        assert!(rate <= now + 60_000 + 50);
        assert!(auth >= now + 900_000 - 50);
    }

    #[test]
    fn a_provider_retry_after_overrides_the_configured_guess() {
        let config = mr_core::config::QuotaConfig {
            default_cooldown_secs: 60,
            live_signals_win: true,
            ..Default::default()
        };
        // `now_millis` is signed because it is a wall clock; the cooldown arithmetic is not, and a
        // negative clock would otherwise saturate into a cooldown that never ends.
        let now = now_millis().max(0) as u64;
        let until = cooldown_until(CooldownReason::RateLimited, 0, Some(5_000), &config);
        assert!(
            until <= now + 5_000 + 50,
            "the measurement wins over the guess"
        );
    }

    #[test]
    fn a_stated_recovery_instant_is_not_multiplied_by_repetition() {
        let config = mr_core::config::QuotaConfig {
            default_cooldown_secs: 60,
            live_signals_win: true,
            ..Default::default()
        };
        let now = now_millis().max(0) as u64;
        let wait = 13 * 3_600_000u64 + 24 * 60_000;
        for failures in [0, 1, 2, 9] {
            let until = cooldown_until(
                CooldownReason::QuotaExhausted,
                failures,
                Some(wait),
                &config,
            );
            assert!(
                until <= now + wait + 50,
                "the provider said {wait} ms; growth on a stated instant parks a healthy account"
            );
        }
    }

    #[test]
    fn the_backoff_grows_with_repetition_and_is_bounded() {
        let config = mr_core::config::QuotaConfig {
            default_cooldown_secs: 60,
            ..Default::default()
        };
        // `now_millis` is signed because it is a wall clock; the cooldown arithmetic is not, and a
        // negative clock would otherwise saturate into a cooldown that never ends.
        let now = now_millis().max(0) as u64;
        let first = cooldown_until(CooldownReason::RateLimited, 0, None, &config) - now;
        let second = cooldown_until(CooldownReason::RateLimited, 1, None, &config) - now;
        let third = cooldown_until(CooldownReason::RateLimited, 2, None, &config) - now;
        let tenth = cooldown_until(CooldownReason::RateLimited, 9, None, &config) - now;
        assert!(second > first);
        assert!(third > second);
        assert_eq!(third, tenth, "growth must saturate rather than run away");
        assert!(tenth <= 240_000, "a bounded backoff, not a week");
    }

    #[test]
    fn a_cold_start_waits_a_fixed_window_and_never_grows() {
        let config = mr_core::config::QuotaConfig {
            default_cooldown_secs: 60,
            ..Default::default()
        };
        // `now_millis` is signed because it is a wall clock; the cooldown arithmetic is not, and a
        // negative clock would otherwise saturate into a cooldown that never ends.
        let now = now_millis().max(0) as u64;
        let first = cooldown_until(CooldownReason::ColdStart, 0, None, &config) - now;
        let fourth = cooldown_until(CooldownReason::ColdStart, 3, None, &config) - now;
        // Wall clock moves between the two calls, so the equality is checked with a tolerance that
        // is small next to the window itself.
        assert!(
            first.abs_diff(fourth) < 1_000,
            "a cold start must not double: {first} vs {fourth}"
        );
        assert!(
            (COLD_START_COOLDOWN_MS as i64 - first as i64).abs() < 1_000,
            "the cold-start wait is the documented window, got {first}"
        );
    }

    #[test]
    fn a_cold_start_is_not_a_rate_limit() {
        let config = mr_core::config::QuotaConfig {
            default_cooldown_secs: 60,
            ..Default::default()
        };
        // `now_millis` is signed because it is a wall clock; the cooldown arithmetic is not, and a
        // negative clock would otherwise saturate into a cooldown that never ends.
        let now = now_millis().max(0) as u64;
        let cold = cooldown_until(CooldownReason::ColdStart, 0, None, &config) - now;
        let rate = cooldown_until(CooldownReason::RateLimited, 0, None, &config) - now;
        assert!(
            cold > rate,
            "a cold start is not a throttle: cold {cold} vs rate {rate}"
        );
    }

    #[test]
    fn priority_account_selection_takes_the_first_available() {
        let p = provider("p", vec![account("a"), account("b")]);
        let chosen = pick_account(&p, KeyStrategy::Priority, 0, &healthy(), 0, "m").unwrap();
        assert_eq!(chosen.id.as_str(), "a");
    }

    #[test]
    fn account_selection_skips_a_cooling_account() {
        let p = provider("p", vec![account("a"), account("b")]);
        let mut h = HashMap::new();
        h.insert(
            "a".to_owned(),
            Cooldown {
                until_ms: 9_000,
                reason: CooldownReason::AuthFailed,
                failures: 1,
            },
        );
        let chosen = pick_account(&p, KeyStrategy::Priority, 0, &h, 1_000, "m").unwrap();
        assert_eq!(chosen.id.as_str(), "b");
    }

    #[test]
    fn account_selection_returns_none_when_everything_is_cooling() {
        let p = provider("p", vec![account("a"), account("b")]);
        let mut h = HashMap::new();
        h.insert(
            "a".to_owned(),
            Cooldown {
                until_ms: 9_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        h.insert(
            "b".to_owned(),
            Cooldown {
                until_ms: 9_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        assert!(pick_account(&p, KeyStrategy::Priority, 0, &h, 1_000, "m").is_none());
    }

    #[test]
    fn round_robin_account_selection_advances_and_wraps() {
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        let got: Vec<String> = (0..7)
            .map(|i| {
                pick_account(&p, KeyStrategy::RoundRobin, i, &healthy(), 0, "m")
                    .unwrap()
                    .id
                    .as_str()
                    .to_owned()
            })
            .collect();
        assert_eq!(got, vec!["a", "b", "c", "a", "b", "c", "a"]);
    }

    #[test]
    fn random_account_selection_only_ever_returns_a_candidate() {
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        for i in 0..200 {
            let chosen = pick_account(&p, KeyStrategy::Random, i, &healthy(), i, "m").unwrap();
            assert!(["a", "b", "c"].contains(&chosen.id.as_str()));
        }
    }

    #[test]
    fn random_account_selection_reaches_every_account() {
        // Not a distribution test — a check that the mixing is not degenerate. A generator whose low
        // bits are constant would always pick the same account, which is a bug that a
        // "returns a candidate" assertion would happily pass.
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        let mut seen = std::collections::BTreeSet::new();
        for i in 0..300 {
            seen.insert(
                pick_account(&p, KeyStrategy::Random, i, &healthy(), i * 977, "m")
                    .unwrap()
                    .id
                    .as_str()
                    .to_owned(),
            );
        }
        assert_eq!(
            seen.len(),
            3,
            "a degenerate mix would never reach all three"
        );
    }

    /// The ids of a candidate list, in order.
    fn candidate_ids<'a>(candidates: &[&'a AccountPlan]) -> Vec<&'a str> {
        candidates.iter().map(|a| a.id.as_str()).collect()
    }

    #[test]
    fn account_candidates_lists_every_eligible_account_in_the_operator_order() {
        // This is the list a caller walks when it has to skip an account for a reason health does
        // not model. It must be the same set and the same order `pick_account` chose from, or the
        // fall-through would be a second opinion about which account is next.
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        let candidates = account_candidates(&p, KeyStrategy::Priority, 0, &healthy(), 0, "m");
        assert_eq!(candidate_ids(&candidates), vec!["a", "b", "c"]);
        assert_eq!(
            pick_account(&p, KeyStrategy::Priority, 0, &healthy(), 0, "m").unwrap(),
            candidates[0]
        );
    }

    /// The last-resort order is the recovery order, not the operator's.
    ///
    /// When every account is cooling, the caller fails over across the cooling accounts one by one,
    /// and the first attempt decides which one gets the request. Operator order spent that attempt
    /// on the config-first account every time — the account that absorbed every last-resort request
    /// while a sibling the operator had already fixed sat untried. Soonest recovery first, and an
    /// account with no cooldown entry (recovered "now") ahead of every parked one.
    #[test]
    fn account_candidates_ignoring_cooldowns_are_in_recovery_order() {
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        let mut h = HashMap::new();
        h.insert(
            "a".to_owned(),
            Cooldown {
                until_ms: 9_000,
                reason: CooldownReason::QuotaExhausted,
                failures: 1,
            },
        );
        h.insert(
            "b".to_owned(),
            Cooldown {
                until_ms: 5_000,
                reason: CooldownReason::QuotaExhausted,
                failures: 1,
            },
        );
        // `c` has no entry at all: it recovers now, ahead of both parked accounts.
        let candidates =
            account_candidates_ignoring_cooldowns(&p, KeyStrategy::Priority, 0, &h, 1_000, "m");
        assert_eq!(candidate_ids(&candidates), vec!["c", "b", "a"]);

        // The model filter still applies in this mode.
        let mut limited = account("b");
        limited.models_allow = vec!["only-this".into()];
        let p = provider("p", vec![account("a"), limited]);
        let candidates =
            account_candidates_ignoring_cooldowns(&p, KeyStrategy::Priority, 0, &h, 1_000, "m");
        assert_eq!(candidate_ids(&candidates), vec!["a"]);
    }

    #[test]
    fn account_candidates_leave_out_the_ineligible_without_reordering_the_rest() {
        let mut limited = account("b");
        limited.models_allow = vec!["only-this".into()];
        let p = provider("p", vec![account("a"), limited, account("c")]);
        let mut h = HashMap::new();
        h.insert(
            "a".to_owned(),
            Cooldown {
                until_ms: 9_000,
                reason: CooldownReason::RateLimited,
                failures: 1,
            },
        );
        let candidates = account_candidates(&p, KeyStrategy::Priority, 0, &h, 1_000, "m");
        assert_eq!(candidate_ids(&candidates), vec!["c"]);
    }

    #[test]
    fn round_robin_candidates_are_a_rotation_of_the_same_set() {
        // As the counter advances, the fall-through order rotates with the first choice. Losing or
        // duplicating an account would make "try the next candidate" mean something else entirely.
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        let expected = [
            vec!["a", "b", "c"],
            vec!["b", "c", "a"],
            vec!["c", "a", "b"],
        ];
        for (counter, order) in expected.iter().enumerate() {
            let candidates = account_candidates(
                &p,
                KeyStrategy::RoundRobin,
                counter as u64,
                &healthy(),
                0,
                "m",
            );
            assert_eq!(&candidate_ids(&candidates), order);
            assert_eq!(
                pick_account(
                    &p,
                    KeyStrategy::RoundRobin,
                    counter as u64,
                    &healthy(),
                    0,
                    "m"
                )
                .unwrap(),
                candidates[0]
            );
        }
    }

    #[test]
    fn random_candidates_start_at_the_account_pick_account_would_have_chosen() {
        // The random strategy's first pick is unchanged by walking the rest of the list.
        let p = provider("p", vec![account("a"), account("b"), account("c")]);
        for i in 0..100 {
            let candidates =
                account_candidates(&p, KeyStrategy::Random, i, &healthy(), i * 977, "m");
            assert_eq!(
                pick_account(&p, KeyStrategy::Random, i, &healthy(), i * 977, "m").unwrap(),
                candidates[0]
            );
            // ...and the rotation still holds every account exactly once.
            let mut ids = candidate_ids(&candidates);
            ids.sort_unstable();
            assert_eq!(ids, vec!["a", "b", "c"]);
        }
    }

    #[test]
    fn least_used_candidates_are_ordered_by_capacity_as_a_whole() {
        let mut a = account("a");
        a.max_concurrent = 1;
        let mut b = account("b");
        b.max_concurrent = 4;
        let p = provider("p", vec![a, b]);
        let candidates = account_candidates(&p, KeyStrategy::LeastUsed, 0, &healthy(), 0, "m");
        assert_eq!(candidate_ids(&candidates), vec!["b", "a"]);
    }

    #[test]
    fn an_account_deny_list_beats_its_allow_list() {
        // A contradiction about spending money is read restrictively.
        let mut a = account("a");
        a.models_allow = vec!["expensive".into()];
        a.models_deny = vec!["expensive".into()];
        assert!(!account_allows_model(&a, "expensive"));
    }

    #[test]
    fn an_empty_allow_list_permits_everything() {
        let a = account("a");
        assert!(account_allows_model(&a, "anything"));
    }

    #[test]
    fn a_provider_prefix_on_either_side_still_matches() {
        let mut a = account("a");
        a.models_allow = vec!["anthropic/claude-sonnet-4-5".into()];
        assert!(account_allows_model(&a, "claude-sonnet-4-5"));
        assert!(account_allows_model(&a, "anthropic/claude-sonnet-4-5"));

        let mut b = account("b");
        b.models_allow = vec!["claude-sonnet-4-5".into()];
        assert!(account_allows_model(&b, "anthropic/claude-sonnet-4-5"));
    }

    #[test]
    fn a_model_outside_the_allow_list_is_refused() {
        let mut a = account("a");
        a.models_allow = vec!["cheap".into()];
        assert!(!account_allows_model(&a, "expensive"));
    }

    #[test]
    fn a_disallowed_account_is_not_a_candidate() {
        let mut limited = account("limited");
        limited.models_allow = vec!["only-this".into()];
        let p = provider("p", vec![limited, account("open")]);
        let chosen =
            pick_account(&p, KeyStrategy::Priority, 0, &healthy(), 0, "other-model").unwrap();
        assert_eq!(chosen.id.as_str(), "open");
    }

    #[test]
    fn the_hedge_resolves_immediately_when_the_top_participant_replies_in_window() {
        // The property that keeps the race from adding latency: a decision already made is not
        // waited on.
        let ps = [
            Participant::arrived(40),
            Participant::PENDING,
            Participant::PENDING,
        ];
        assert_eq!(hedge_decide(&ps, 250, 40), HedgeDecision::Resolved(0));
    }

    #[test]
    fn a_lower_participant_cannot_win_while_a_higher_one_may_still_reply() {
        let ps = [Participant::PENDING, Participant::arrived(50)];
        assert_eq!(hedge_decide(&ps, 250, 50), HedgeDecision::Waiting);
    }

    #[test]
    fn a_lower_participant_wins_once_every_higher_one_has_failed() {
        let ps = [Participant::FAILED, Participant::arrived(50)];
        assert_eq!(hedge_decide(&ps, 250, 50), HedgeDecision::Resolved(1));
    }

    #[test]
    fn the_highest_priority_eligible_participant_wins_even_if_it_was_slower() {
        // Priority, not speed: the operator's ordering is the point of the race.
        let ps = [
            Participant::arrived(200),
            Participant::arrived(20),
            Participant::PENDING,
        ];
        assert_eq!(hedge_decide(&ps, 250, 200), HedgeDecision::Resolved(0));
    }

    #[test]
    fn a_participant_that_replied_after_the_window_does_not_win_on_priority() {
        // Step 1 was slow — 400 ms, well outside the 250 ms window — so it has no priority
        // protection even though it is first in the operator's order. Step 2 replied in time and
        // wins.
        //
        // Note that step 1's late arrival still *resolves* it: it no longer blocks the race, so the
        // decision is immediate rather than waiting out a window that step 1 has already lost. A
        // participant that has replied can never block a lower one, however late it was.
        let ps = [Participant::arrived(400), Participant::arrived(30)];
        assert_eq!(hedge_decide(&ps, 250, 400), HedgeDecision::Resolved(1));
    }

    #[test]
    fn a_slow_higher_priority_participant_blocks_only_while_it_is_still_pending() {
        // The same race, earlier: step 1 has not replied yet, so it might still make the window, and
        // step 2 must wait. This is the pair of assertions that pins the rule — the only difference
        // between them is whether the higher-priority participant is resolved.
        let pending = [Participant::PENDING, Participant::arrived(30)];
        assert_eq!(hedge_decide(&pending, 250, 30), HedgeDecision::Waiting);

        let landed_late = [Participant::arrived(400), Participant::arrived(30)];
        assert_eq!(
            hedge_decide(&landed_late, 250, 400),
            HedgeDecision::Resolved(1)
        );
    }

    #[test]
    fn a_late_arrival_is_still_used_when_nothing_was_eligible() {
        // Every participant was too slow. Failing the request would throw away a working response,
        // so the best available answer is the highest-priority participant that replied at all.
        let ps = [Participant::arrived(900), Participant::arrived(800)];
        assert_eq!(hedge_decide(&ps, 250, 1_000), HedgeDecision::Resolved(0));
    }

    #[test]
    fn window_expiry_never_fails_a_race_that_has_a_reply() {
        // The window is an eligibility gate, not a timeout. Throwing away a working response because
        // a clock passed would be the opposite of the feature's purpose.
        let ps = [Participant::arrived(900)];
        assert_eq!(hedge_decide(&ps, 250, 1_000), HedgeDecision::Resolved(0));
    }

    #[test]
    fn a_race_with_nothing_resolved_and_no_window_left_keeps_waiting() {
        let ps = [Participant::PENDING, Participant::PENDING];
        assert_eq!(hedge_decide(&ps, 250, 300), HedgeDecision::Waiting);
    }

    #[test]
    fn a_race_everyone_failed_has_no_winner() {
        let ps = [Participant::FAILED, Participant::FAILED];
        assert_eq!(hedge_decide(&ps, 250, 250), HedgeDecision::NoWinner);
    }

    #[test]
    fn an_empty_race_has_no_winner() {
        assert_eq!(hedge_decide(&[], 250, 0), HedgeDecision::NoWinner);
    }

    #[test]
    fn the_top_participant_reply_is_immediate_even_with_others_pending() {
        // The immediate-resolution rule stated as a property: no waiting for peers.
        let ps = [
            Participant::arrived(1),
            Participant::PENDING,
            Participant::PENDING,
        ];
        assert_eq!(hedge_decide(&ps, 5_000, 1), HedgeDecision::Resolved(0));
    }

    #[test]
    fn rotate_by_leaves_an_empty_list_alone() {
        assert_eq!(rotate_by(&[], 5), Vec::<usize>::new());
    }

    #[test]
    fn stable_partition_preserves_order_inside_both_groups() {
        // The property the operator's priority order depends on.
        assert_eq!(
            stable_partition_by(&[true, false, true, false, true], true),
            vec![0, 2, 4, 1, 3]
        );
    }

    #[test]
    fn account_availability_treats_an_unknown_account_as_available() {
        // An account the relay has never seen a failure for is usable. Defaulting the other way
        // would make a fresh install route nothing.
        assert!(account_available("never-seen", &healthy(), 0));
    }

    /// A cooldown entry for the fake-clock tests: parked until `until_ms`, first strike.
    fn parked(until_ms: u64, reason: CooldownReason) -> Cooldown {
        Cooldown {
            until_ms,
            reason,
            failures: 1,
        }
    }

    #[test]
    fn the_default_scope_parks_the_whole_account() {
        // An operator who never sets a scope gets the historical behaviour exactly: one refusal
        // takes the credential out for every model, under the bare account id as the key.
        let p = provider("p", vec![account("a0")]);
        assert_eq!(p.cooldown_scope, CooldownScope::Account);
        let key = cooldown_key(
            p.cooldown_scope,
            "p",
            "a0",
            "pro",
            CooldownReason::RateLimited,
        );
        assert_eq!(key, "a0");
        let mut h = HashMap::new();
        h.insert(key, parked(9_000, CooldownReason::RateLimited));
        assert!(!step_available(&p, &h, 0, "pro"));
        assert!(!step_available(&p, &h, 0, "lite"));
    }

    #[test]
    fn a_model_specific_park_is_visible_under_the_account_scope() {
        // The live failure this guards. A provider configured with the wide `account` scope — the
        // default, and the right reading of a spent balance — can still record a *plan* refusal
        // against one model, because the credential is perfectly good for every other one. The
        // reader must therefore consult the narrow shape the writer chose, not the provider's
        // ceiling: with the ceiling as its only key, the entry was written, logged, and then ignored
        // on the very next request, which dialled the upstream that had just refused it. Three
        // attempts in a row, ~1.3 s of upstream each, for a model the account cannot have today.
        let p = provider("p", vec![account("a0")]);
        assert_eq!(p.cooldown_scope, CooldownScope::Account);
        let mut h = HashMap::new();
        h.insert(
            cooldown_key(
                CooldownScope::Model,
                "p",
                "a0",
                "pro",
                CooldownReason::QuotaExhausted,
            ),
            parked(9_000, CooldownReason::QuotaExhausted),
        );
        // The parked model is out; its siblings on the same credential are untouched. That is the
        // whole point of recording it narrowly, so reading it must not widen it either.
        assert!(!step_available(&p, &h, 0, "pro"));
        assert!(step_available(&p, &h, 0, "lite"));
        assert_eq!(route_reset_instant("p", "a0", "pro", &h), Some(9_000));
        assert_eq!(route_reset_instant("p", "a0", "lite", &h), None);

        // And the matching success clears it, rather than leaving an entry nothing would remove.
        let health = Health::new();
        health.cool_down(
            &model_key("a0", "pro"),
            9_000,
            CooldownReason::QuotaExhausted,
        );
        assert!(!account_available_for("p", "a0", "pro", &health.view(), 0));
        health.record_success("p", "a0", "pro");
        assert_eq!(health.cooling_count(), 0);
    }

    #[test]
    fn a_model_scope_parks_only_the_refused_model_on_that_credential() {
        // The OpenAdapter shape: a spent Pro allowance says nothing about Lite on the same
        // credential, so the combo's Lite step must still be allowed to use it.
        let mut solo = provider("p", vec![account("a0")]);
        solo.cooldown_scope = CooldownScope::Model;
        let key = cooldown_key(
            CooldownScope::Model,
            "p",
            "a0",
            "pro",
            CooldownReason::QuotaExhausted,
        );
        assert_ne!(key, "a0", "the model scope records under a composed key");
        let mut h = HashMap::new();
        h.insert(key, parked(9_000, CooldownReason::QuotaExhausted));

        assert!(!account_available_for("p", "a0", "pro", &h, 0));
        assert!(account_available_for("p", "a0", "lite", &h, 0));
        assert!(!step_available(&solo, &h, 0, "pro"));
        assert!(step_available(&solo, &h, 0, "lite"));

        // And it is scoped to the credential, not to the provider: an untouched account can still
        // serve the parked model.
        let mut pair = provider("p", vec![account("a0"), account("a1")]);
        pair.cooldown_scope = CooldownScope::Model;
        assert!(account_available_for("p", "a1", "pro", &h, 0));
        assert!(step_available(&pair, &h, 0, "pro"));
    }

    #[test]
    fn a_provider_scope_parks_the_model_for_every_credential() {
        // The opposite reading of the same refusal: the bucket belongs to the provider, so a
        // credential that has not been tried yet would be refused for the same reason.
        let mut p = provider("p", vec![account("a0"), account("a1")]);
        p.cooldown_scope = CooldownScope::Provider;
        let mut h = HashMap::new();
        h.insert(
            cooldown_key(
                CooldownScope::Provider,
                "p",
                "a0",
                "pro",
                CooldownReason::RateLimited,
            ),
            parked(9_000, CooldownReason::RateLimited),
        );
        assert!(!account_available_for("p", "a1", "pro", &h, 0));
        assert!(account_available_for("p", "a1", "lite", &h, 0));
        assert!(!step_available(&p, &h, 0, "pro"));
        assert!(step_available(&p, &h, 0, "lite"));
    }

    #[test]
    fn an_auth_failure_is_account_wide_under_every_scope() {
        // A rejected credential is a fact about the credential. Widening it would silence accounts
        // whose keys were never sent, so the scope must not apply to it.
        for scope in [
            CooldownScope::Account,
            CooldownScope::Model,
            CooldownScope::Provider,
        ] {
            assert_eq!(
                cooldown_key(scope, "p", "a0", "pro", CooldownReason::AuthFailed),
                "a0"
            );
            let mut h = HashMap::new();
            h.insert("a0".to_owned(), parked(9_000, CooldownReason::AuthFailed));
            assert!(!account_available_for("p", "a0", "any-model", &h, 0));
        }
    }

    #[test]
    fn a_route_resets_when_the_last_shape_parking_it_expires() {
        // The maximum, not the minimum: the account entry expiring does not make the route usable
        // while the model-shaped entry still holds it. Reading the minimum here would let a step
        // back in as soon as either half lifted.
        let h = Health::new();
        h.cool_down("a0", 1_000, CooldownReason::AuthFailed);
        h.cool_down(
            &model_key("a0", "pro"),
            5_000,
            CooldownReason::QuotaExhausted,
        );
        assert_eq!(
            route_reset_instant("p", "a0", "pro", &h.view()),
            Some(5_000)
        );
        assert_eq!(
            route_reset_instant("p", "a0", "lite", &h.view()),
            Some(1_000)
        );
    }

    #[test]
    fn a_success_clears_every_shape_that_could_have_parked_the_route() {
        // A request that worked proves both halves: the credential answers and this model answers
        // on it. Leaving the wider entry behind would keep skipping a route just shown to work.
        let h = Health::new();
        h.cool_down("a0", 1_000, CooldownReason::RateLimited);
        h.cool_down(
            &model_key("a0", "pro"),
            2_000,
            CooldownReason::QuotaExhausted,
        );
        h.record_success("p", "a0", "pro");
        assert_eq!(h.cooling_count(), 0);
        assert_eq!(h.failures(CooldownScope::Model, "p", "a0", "pro"), 0);
    }

    #[test]
    fn clearing_an_account_takes_its_model_keys_but_leaves_a_provider_bucket() {
        // "Try this account now" means every route on this credential. A provider-scoped entry is
        // not this account's to clear: it belongs to the provider's quota.
        let h = Health::new();
        h.cool_down("a0", 1_000, CooldownReason::RateLimited);
        h.cool_down(
            &model_key("a0", "pro"),
            2_000,
            CooldownReason::QuotaExhausted,
        );
        h.cool_down(
            &provider_key("p", "pro"),
            3_000,
            CooldownReason::QuotaExhausted,
        );
        h.clear("a0");
        assert_eq!(h.cooling_count(), 1);
        assert!(h.view().contains_key(&provider_key("p", "pro")));
    }

    #[test]
    fn order_steps_demotes_a_parked_model_and_keeps_its_sibling_step() {
        // The combo-level regression for the live failure this scope exists for: step 1's model is
        // out of quota, step 2 is a different model on the same credential and must stay first.
        let mut p = provider("p0", vec![account("a0")]);
        p.cooldown_scope = CooldownScope::Model;
        let c = combo(
            ComboStrategy::Priority,
            vec![step(0, "pro"), step(0, "lite")],
        );
        let mut h = HashMap::new();
        h.insert(
            cooldown_key(
                CooldownScope::Model,
                "p0",
                "a0",
                "pro",
                CooldownReason::QuotaExhausted,
            ),
            parked(9_000, CooldownReason::QuotaExhausted),
        );
        assert_eq!(order_steps(&c, &[p], &h, 0, 0), vec![1, 0]);
    }

    // -----------------------------------------------------------------------
    // per-step account selection
    // -----------------------------------------------------------------------

    /// The default is every account, so a step written before selection existed is unchanged.
    #[test]
    fn a_step_without_a_selection_allows_every_account() {
        let step = step(0, "m");
        assert!(step_allows_account(&step, &account("a1")));
        assert!(step_allows_account(&step, &account("a2")));
    }

    #[test]
    fn a_step_selection_allows_only_the_named_accounts() {
        let mut step = step(0, "m");
        step.accounts = Some(vec![mr_core::ids::AccountId::from_raw("a2")]);
        assert!(!step_allows_account(&step, &account("a1")));
        assert!(step_allows_account(&step, &account("a2")));
    }

    /// An empty selection is a step that can run nowhere. It must not be read as "no restriction",
    /// which would send traffic through accounts the operator excluded.
    #[test]
    fn an_empty_selection_allows_nothing() {
        let mut step = step(0, "m");
        step.accounts = Some(Vec::new());
        assert!(!step_allows_account(&step, &account("a1")));
        assert!(!step_allows_account(&step, &account("a2")));
    }

    /// The selection is applied before cooldowns are consulted, so a step whose only account is
    /// parked reports itself unusable rather than falling through to an account it may not use.
    #[test]
    fn a_selected_account_that_is_parked_makes_the_step_unusable() {
        let p = provider("p0", vec![account("a1"), account("a2")]);
        let mut step = step(0, "m");
        step.accounts = Some(vec![mr_core::ids::AccountId::from_raw("a1")]);
        let mut health = HashMap::new();
        health.insert(
            cooldown_key(
                CooldownScope::Account,
                "p0",
                "a1",
                "m",
                CooldownReason::AuthFailed,
            ),
            parked(9_000, CooldownReason::AuthFailed),
        );
        assert!(!step_has_usable_account(&p, &health, 0, &step));
        // The unselected account is healthy, and that must not rescue the step.
        assert!(account_available_for("p0", "a2", "m", &health, 0));
    }
}
