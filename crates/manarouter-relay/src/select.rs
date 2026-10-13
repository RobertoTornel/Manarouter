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
use mr_core::snapshot::{AccountPlan, ComboPlan, ProviderPlan, StepPlan};

/// Why an account is temporarily unusable.
///
/// The distinction drives the cooldown length and, more importantly, what the panel tells the
/// operator. A rate limit resolves itself; a rejected credential does not, and presenting the two
/// the same way sends someone to wait for a fix that requires their action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CooldownReason {
    /// The provider throttled us. Resolves on its own.
    RateLimited,
    /// A declared quota window is full. Resolves on its own.
    QuotaExhausted,
    /// The credential was rejected. **Never resolves without operator action.**
    AuthFailed,
    /// Repeated transient failures. The cooldown grows with each one.
    Backoff,
}

impl CooldownReason {
    /// Whether waiting can fix this.
    pub fn self_healing(self) -> bool {
        !matches!(self, CooldownReason::AuthFailed)
    }

    /// The stable identifier, for logs and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate-limited",
            Self::QuotaExhausted => "quota-exhausted",
            Self::AuthFailed => "auth-failed",
            Self::Backoff => "backoff",
        }
    }
}

/// A temporary unavailability, with the instant it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

    /// Take an account out of rotation until `until_ms`.
    ///
    /// The failure count **carries forward** across a cooldown rather than resetting. That is what
    /// stops the flap described in `docs/09-reset-aware-priority.md`: an account that fails, cools
    /// down, and fails again is at two consecutive failures, so three strikes really do mean three.
    /// Resetting here would let a permanently throttled account retry forever at full rate.
    pub fn cool_down(&self, account_id: &str, until_ms: u64, reason: CooldownReason) {
        self.map.rcu(|current| {
            let mut next = HashMap::with_capacity(current.len() + 1);
            next.extend(current.iter().map(|(k, v)| (k.clone(), *v)));
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

    /// Record a success, clearing any cooldown and the failure count.
    pub fn record_success(&self, account_id: &str) {
        self.map.rcu(|current| {
            if !current.contains_key(account_id) {
                return current.clone();
            }
            let mut next = HashMap::with_capacity(current.len());
            next.extend(current.iter().map(|(k, v)| (k.clone(), *v)));
            next.remove(account_id);
            std::sync::Arc::new(next)
        });
    }

    /// Clear one account's cooldown, for the operator's "try it now" action.
    pub fn clear(&self, account_id: &str) {
        self.record_success(account_id);
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
        self.map.load().values().map(|c| c.until_ms).min()
    }
}

/// Whether an account is usable right now.
///
/// Takes the map by reference rather than loading it itself, so one request resolves every account
/// against a single consistent view.
pub fn account_available(
    account_id: &str,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
) -> bool {
    match health.get(account_id) {
        None => true,
        Some(c) => c.is_over(now_ms),
    }
}

/// Whether a step has at least one usable account.
pub fn step_available(
    provider: &ProviderPlan,
    health: &HashMap<String, Cooldown>,
    now_ms: u64,
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
        .any(|a| account_available(a.id.as_str(), health, now_ms))
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
    let available: Vec<bool> = (0..n)
        .map(
            |i| match providers.get(combo.steps[i].provider_index as usize) {
                Some(p) => step_available(p, health, now_ms),
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
    provider
        .accounts
        .iter()
        .filter_map(|a| health.get(a.id.as_str()).map(|c| c.until_ms))
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

/// Choose an account for a step.
///
/// `counter` is the step's own rotation counter, so that two steps on the same provider do not
/// advance each other's position.
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
    // Candidates in the operator's order. `priority` is expressed by the order of the account list
    // in the configuration, so nothing here needs to sort for that strategy.
    let mut candidates: Vec<&AccountPlan> = provider
        .accounts
        .iter()
        .filter(|a| account_available(a.id.as_str(), health, now_ms))
        .filter(|a| account_allows_model(a, model))
        .collect();

    if candidates.is_empty() {
        return None;
    }

    match strategy {
        KeyStrategy::Priority => candidates.first().copied(),
        KeyStrategy::RoundRobin => {
            let i = (counter % candidates.len() as u64) as usize;
            candidates.get(i).copied()
        }
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
            let i = (x % candidates.len() as u64) as usize;
            candidates.get(i).copied()
        }
        KeyStrategy::LeastUsed => {
            // `max_concurrent` is the only load signal the snapshot carries, and it is a *capacity*,
            // not a count of in-flight requests. Using it as a tie-break keeps the pool fresh when
            // capacities differ, and it is honest about being a preference rather than a
            // measurement — a real least-used needs a counter the relay does not keep per account.
            candidates.sort_by_key(|a| std::cmp::Reverse(a.max_concurrent));
            candidates.first().copied()
        }
    }
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
    let strip = |s: &str| match s.rsplit_once('/') {
        Some((_, tail)) => tail.to_owned(),
        None => s.to_owned(),
    };
    a.eq_ignore_ascii_case(b) || strip(a).eq_ignore_ascii_case(&strip(b))
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
        CooldownReason::RateLimited | CooldownReason::QuotaExhausted | CooldownReason::Backoff => {
            config.default_cooldown_secs.saturating_mul(1000)
        }
    };

    // A provider's own `retry-after` is knowledge and outranks the configured guess.
    let chosen = if config.live_signals_win {
        retry_after_ms.unwrap_or(base)
    } else {
        base
    };

    // The backoff grows with repetition, because a provider that states the same reset three times
    // and rejects the request each time is not honouring it. Doubling is bounded by two doublings so
    // that a long-running account cannot end up cooling for a week.
    let growth = 1u64 << previous_failures.min(2);
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
            accounts,
            capabilities: Vec::new(),
            connect_timeout_ms: 1000,
            ttfb_timeout_ms: 1000,
            idle_timeout_ms: 1000,
            total_timeout_ms: 1000,
            headers: Default::default(),
            route_overrides: Default::default(),
            http2: Default::default(),
            prewarm: false,
            steps: Vec::new(),
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
    fn a_success_clears_the_failure_count() {
        let h = Health::new();
        h.cool_down("a", 1_000, CooldownReason::RateLimited);
        h.record_success("a");
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
}
