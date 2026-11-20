//! The relay's shared state: the compiled routing table, the upstream client, and client keys.
//!
//! # What is here, and what is deliberately not
//!
//! Mutable request-path state is limited to relaxed atomics: an `AtomicU64` per rotating pool — a
//! combo's steps, a step's accounts — plus a global counter and per-proxy in-flight counts for
//! proxy choice.
//! Everything else — the routing table, the providers, the accounts — is loaded once, behind an
//! `ArcSwap`, and read without a lock.
//!
//! That is not an optimisation for its own sake. A mutex on this path is a queue, and a queue is
//! latency that grows with load precisely when latency matters most. The compiled snapshot exists so
//! that a request needs no coordination at all: it loads an `Arc`, walks a slice, and sends bytes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use mr_core::config::Config;
use mr_core::ids::ComboId;
use mr_core::snapshot::{RoutingSnapshot, StepPlan};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::upstream::Upstream;

/// Mutable proxy-selection counters, separate from the hot-swapped routing table.
///
/// The counters are keyed by proxy **id**, not by position in a list. A position has to be resolved
/// against one particular generation of the proxy list, and the request path reads the *compiled*
/// snapshot: a proxy added or removed between two requests would make a position refer to a
/// different proxy, which is exactly the kind of near-miss that becomes a wrong egress decision.
/// An id names one proxy and cannot be reused for another, so a counter that outlives a
/// configuration change still counts the proxy it was counting.
///
/// The round-robin ticket is global and survives every change, because fairness is a property of
/// the sequence of requests, not of the list they are chosen from.
///
/// Selection itself remains independent of `RelayState`; the caller holds this beside the routing
/// table and passes it to `proxy::choose`.
pub struct ProxySelectionState {
    round_robin: AtomicU64,
    inflight: Mutex<HashMap<String, Arc<AtomicUsize>>>,
}

impl ProxySelectionState {
    /// Construct counters for a config's proxy list.
    ///
    /// The list is only used to pre-register the ids, so that the first request for a proxy does
    /// not have to grow the map. A proxy that is not in the list is still counted correctly when it
    /// is used: an id that is unknown simply starts at zero.
    pub fn new(proxies: &[mr_core::config::ProxyConfig]) -> Self {
        let state = Self::empty();
        {
            let mut inflight = state.inflight.lock().unwrap_or_else(|e| e.into_inner());
            for proxy in proxies {
                inflight
                    .entry(proxy.id.clone())
                    .or_insert_with(|| Arc::new(AtomicUsize::new(0)));
            }
        }
        state
    }

    /// Construct counters for no proxies at all.
    pub fn empty() -> Self {
        Self {
            round_robin: AtomicU64::new(0),
            inflight: Mutex::new(HashMap::new()),
        }
    }

    /// Increment the shared round-robin ticket and return the previous value.
    pub fn next_round_robin(&self) -> u64 {
        self.round_robin.fetch_add(1, Ordering::Relaxed)
    }

    /// Current number of leases for the proxy with this id.
    ///
    /// An id nobody has leased yet has zero leases. That is not a stale generation being mistaken
    /// for an idle proxy: the counters are keyed by id, so a counter that exists is always the
    /// counter of the proxy it names.
    pub fn inflight(&self, id: &str) -> usize {
        self.counter(id).load(Ordering::Relaxed)
    }

    /// Mark the proxy with this id in flight until the returned lease is dropped.
    ///
    /// The call site should create the lease immediately after selection, then keep it alive until
    /// the upstream request ends (including TLS setup and response streaming). If connect fails, the
    /// same drop releases the count; no hand-written decrement path can be forgotten.
    pub fn lease(&self, id: &str) -> ProxyLease {
        let counter = self.counter(id);
        counter.fetch_add(1, Ordering::Relaxed);
        ProxyLease { counter }
    }

    /// The counter for a proxy id, creating it the first time it is used.
    fn counter(&self, id: &str) -> Arc<AtomicUsize> {
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            inflight
                .entry(id.to_owned())
                .or_insert_with(|| Arc::new(AtomicUsize::new(0))),
        )
    }
}

/// An RAII count of one request using a proxy.
///
/// The counter is atomic and relaxed because it chooses a tie-breaker, not a synchronization
/// boundary. A stale count can only change which equally eligible proxy the next request uses; it
/// cannot affect the request currently holding this lease.
pub struct ProxyLease {
    counter: Arc<AtomicUsize>,
}

impl Drop for ProxyLease {
    fn drop(&mut self) {
        let previous = self.counter.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "proxy lease counter underflowed");
    }
}

/// The routing table, with the one piece of mutable routing state attached.
///
/// The counters live inside the same swapped value as the snapshot rather than beside it, so a
/// request that loads the table sees a combo list and its counters from the same generation. Two
/// separate swaps would allow a request to read a new combo list with old counters, which is
/// survivable — a round-robin counter starting over is not a correctness problem — but it is the
/// kind of near-miss that makes an eventual real bug unreadable.
pub struct RouteTable {
    /// The compiled snapshot. Immutable for the life of this table.
    pub snapshot: RoutingSnapshot,
    /// One relaxed counter per rotating pool, indexed by position in `counter_pos`.
    ///
    /// Relaxed ordering is correct here and not a shortcut. The counter orders *nothing*: no thread
    /// synchronises with another through it, and a lost update means one request goes to the next
    /// step instead of this one. Paying for `SeqCst` to order a decision that nothing observes would
    /// be buying a fence with no buyer.
    rr: Vec<AtomicU64>,
    /// Every key a counter is held under, to its position in `rr`.
    ///
    /// Two kinds of pool share the vector, because both are the same thing: a rotation that has to
    /// remember where it got to. A combo takes its steps in turn, and a step takes its accounts in
    /// turn. Both are allocated in [`new`](Self::new), from the snapshot itself.
    counter_pos: HashMap<ComboId, usize>,
}

impl RouteTable {
    /// Build a table around a snapshot, allocating one counter per rotating pool.
    pub fn new(snapshot: RoutingSnapshot) -> Self {
        // Every pool a request can advance, in a fixed order so that two tables built from one
        // snapshot hand out the same slots: each combo's step rotation first, then each step's
        // account rotation.
        //
        // The step keys are the half that matters. A step's accounts are taken in turn however the
        // step was reached — a direct `provider/model` request never sees a combo — so every
        // compiled step is allocated here. Leaving one out does not fail loudly: a lookup that misses
        // answers `0`, which is a perfectly good counter, so the rotation would never advance and
        // the provider would serve from its first account for as long as the process lived.
        let mut keys: Vec<ComboId> = snapshot.combos().iter().map(|c| c.id.clone()).collect();
        for provider in snapshot.providers() {
            keys.extend(provider.steps.iter().map(step_counter_key));
        }
        for combo in snapshot.combos() {
            keys.extend(combo.steps.iter().map(step_counter_key));
        }
        let mut counter_pos = HashMap::with_capacity(keys.len());
        for key in keys {
            // The length is the next free slot while the keys arrive in order. A step named twice —
            // by two combos, or by a combo and its provider's own model list — is one pool, so it
            // keeps the slot it already has and both callers advance the same counter.
            let next = counter_pos.len();
            counter_pos.entry(key).or_insert(next);
        }
        let rr = (0..counter_pos.len()).map(|_| AtomicU64::new(0)).collect();
        Self {
            snapshot,
            rr,
            counter_pos,
        }
    }

    /// Advance and read the round-robin counter that orders a combo's steps.
    pub fn advance_combo_rr(&self, combo: &ComboId) -> u64 {
        self.advance(combo)
    }

    /// Advance and read the round-robin counter that rotates one step's accounts.
    ///
    /// Keyed by the step rather than by the combo that names it: two combos sharing a step are
    /// choosing from the same account pool, and so is a direct `provider/model` request, so all
    /// three must advance one counter between them.
    pub fn advance_account_rr(&self, step: &StepPlan) -> u64 {
        self.advance(&step_counter_key(step))
    }

    /// Advance one counter, or answer `0` for a key this table never allocated.
    ///
    /// Unreachable by construction — the two callers above only build keys [`new`](Self::new)
    /// allocated — and it answers `0` rather than panicking, because one request must not take the
    /// process down.
    fn advance(&self, key: &ComboId) -> u64 {
        match self.counter_pos.get(key) {
            Some(&i) => match self.rr.get(i) {
                Some(counter) => counter.fetch_add(1, Ordering::Relaxed),
                None => 0,
            },
            None => 0,
        }
    }

    /// The number of combos in this table.
    pub fn combo_count(&self) -> usize {
        self.snapshot.combos().len()
    }
}

/// The counter key for one step's account rotation.
///
/// Provider and model rather than the combo: the accounts a step draws from are a property of the
/// step, not of the route that named it. `#` is punctuation no provider id is likely to contain; a
/// model string that did contain it could collide with another step's key, and the cost of a
/// collision is two steps sharing one rotation counter — faster rotation, never a wrong route.
fn step_counter_key(step: &StepPlan) -> ComboId {
    ComboId::from_raw(format!("{}#{}", step.provider_id, step.model))
}

/// Per-account in-flight request counts, for spreading concurrent load across a provider's accounts.
///
/// A provider that rate-limits under concurrency is usually throttling *one key*, not the whole
/// account pool: if every concurrent request lands on the same account, that account absorbs the
/// whole burst and 429s while its siblings sit idle. Account selection reads these counts and
/// prefers the least-busy account, so N concurrent requests across an N-account provider spread one
/// per account.
#[derive(Debug, Default)]
pub struct AccountInflight {
    counts: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl AccountInflight {
    /// A consistent snapshot of in-flight counts, keyed by account id.
    pub fn snapshot(&self) -> std::collections::HashMap<String, u64> {
        self.counts.lock().expect("not poisoned").clone()
    }

    /// Mark one more request as in flight on `account_id`; the returned lease releases it when it
    /// drops.
    pub fn begin(&self, account_id: &str) -> InflightLease<'_> {
        let mut counts = self.counts.lock().expect("not poisoned");
        *counts.entry(account_id.to_owned()).or_insert(0) += 1;
        InflightLease {
            inflight: self,
            account_id: account_id.to_owned(),
        }
    }

    fn end(&self, account_id: &str) {
        let mut counts = self.counts.lock().expect("not poisoned");
        if let Some(count) = counts.get_mut(account_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(account_id);
            }
        }
    }
}

/// Releases an in-flight count when the request it belongs to finishes.
#[derive(Debug)]
pub struct InflightLease<'a> {
    inflight: &'a AccountInflight,
    account_id: String,
}

impl Drop for InflightLease<'_> {
    fn drop(&mut self) {
        self.inflight.end(&self.account_id);
    }
}

/// A client key, reduced to what authentication actually needs.
///
/// The plaintext is not stored, in memory or on disk. A key is verified by hashing what the client
/// presented and comparing that against the stored hash in constant time. Keeping the plaintext
/// would let the relay compare strings, which is faster and leaks the key through timing.
#[derive(Debug, Clone)]
pub struct KeyEntry {
    /// The key's id, for logs. Never the key.
    pub id: String,
    /// Display label.
    pub label: String,
    /// Lowercase hex SHA-256 of the key.
    pub hash: [u8; 32],
    /// Models this key may address. Empty means any.
    pub allow_models: Vec<String>,
    /// Routes this key may use. Empty means any.
    pub allow_routes: Vec<String>,
    /// Combos this key may use, by id or name. Empty means any.
    pub allow_combos: Vec<String>,
    /// The flat, non-rolling request budget. Zero means unlimited.
    ///
    /// It travels with the key because the relay has to be able to answer "may this key make this
    /// request?" without reading the configuration file on the request path.
    pub limit_requests: u64,
    /// The rolling token and request caps, by window.
    ///
    /// Behind an `Arc` because the entry is copied on every authenticated request, and the three
    /// windows are 72 bytes that would otherwise be copied with it. The `Arc` is cloned instead,
    /// which is a single atomic increment rather than a memcpy — and it keeps
    /// [`AuthOutcome`] small enough that the enum is not dominated by one variant.
    pub limits: Arc<mr_core::model::KeyLimits>,
}

impl KeyEntry {
    /// Parse the stored hex hash into bytes.
    ///
    /// A stored hash that is not 64 hex characters is **skipped with a warning** rather than treated
    /// as a match or as a fatal error. A malformed row is a configuration problem, and the safe
    /// direction is to refuse the key it belongs to; a fatal error would take the whole relay down
    /// for one bad row, which is a worse outcome for the operator.
    pub fn from_config(key: &mr_core::model::ClientKey) -> Option<Self> {
        if !key.enabled {
            return None;
        }
        let raw = key.key_hash.trim();
        if raw.len() != 64 {
            tracing::warn!(
                key_id = %key.id,
                "client key hash is not a 64-character SHA-256; the key will not authenticate"
            );
            return None;
        }
        let mut hash = [0u8; 32];
        for (i, byte) in hash.iter_mut().enumerate() {
            let pair = raw.get(i * 2..i * 2 + 2)?;
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self {
            id: key.id.to_string(),
            label: key.label.clone(),
            hash,
            allow_models: key.allow_models.clone(),
            allow_routes: key.allow_routes.clone(),
            allow_combos: key.allow_combos.clone(),
            limit_requests: key.limit_requests,
            limits: Arc::new(key.limits),
        })
    }

    /// Whether this key is metered at all.
    ///
    /// A key with no caps pays nothing on the request path: no ledger read, no row written. That
    /// is what keeps quota enforcement free for the keys that never asked for it.
    pub fn is_metered(&self) -> bool {
        crate::quota::QuotaManager::is_metered(&self.limits, self.limit_requests)
    }

    /// Whether a presented key matches, in constant time.
    ///
    /// Constant time matters here even though the input is a hash. A byte-by-byte comparison returns
    /// early on the first difference, and the timing of that early return tells an attacker how many
    /// leading bytes of a *candidate hash* are correct. That does not directly reveal the key, but it
    /// turns a 256-bit search into a byte-at-a-time one, and there is no reason to accept that when
    /// the fix is one comparison.
    pub fn matches(&self, presented_hash: &[u8; 32]) -> bool {
        // Slices, because `ConstantTimeEq` is implemented for `[u8]` and using the array
        // form would need an explicit length check that the slice version already has.
        self.hash.as_slice().ct_eq(presented_hash.as_slice()).into()
    }
}

/// Everything a request needs, loaded once.
pub struct RelayState {
    /// The routing table and counters, hot-swappable.
    table: ArcSwap<RouteTable>,
    /// The pooled upstream client, shared by every worker so the connection pool is not siloed.
    upstream: Upstream,
    /// Configured client keys, or empty when the instance is open.
    keys: ArcSwap<Vec<KeyEntry>>,
    /// Whether `config.json` declares any client keys at all, even when none of them could be
    /// loaded. Kept separate from the ring length so that a configuration whose hashes are all
    /// malformed fails closed instead of silently reopening the relay.
    keys_declared: std::sync::atomic::AtomicBool,
    /// The configuration directory, so a reload knows where to look.
    config_dir: std::path::PathBuf,
    /// Proxy-selection counters: the round-robin ticket and the per-proxy in-flight counts.
    ///
    /// They live here rather than in the routing table because they are *not* a property of a
    /// snapshot: a ticket that restarted on every reload would make the round-robin strategy
    /// unfair, and an in-flight count that restarted would make `least-used` send a burst of
    /// requests through one proxy.
    proxy_selection: ProxySelectionState,
    /// Quota cooldown configuration, from config.json.
    ///
    /// `default_cooldown_secs`, `auth_failure_cooldown_secs` and `live_signals_win` are read here
    /// so that an operator's configured values actually take effect on the request path, instead
    /// of the hard-coded defaults.
    ///
    /// Swapped whole on reload. A plain field would be captured once at startup, so an operator
    /// who hand-edits `quota` in config.json and sees "routing table reloaded" would reasonably
    /// expect the new cooldown to apply — and would silently keep the old one until a restart.
    quota_config: ArcSwap<mr_core::config::QuotaConfig>,
    /// Silent rate-limit retry configuration, from config.json (`rateLimitRetry`).
    ///
    /// Swapped whole on reload for the same reason as `quota_config`: a hand edit must take
    /// effect without a restart.
    rate_limit_retry: ArcSwap<mr_core::config::RateLimitRetryConfig>,
    /// Debug logging of admitted request payloads (`debugLogMessages`).
    ///
    /// Swapped whole on reload so a hand edit applies without a restart.
    debug_log_messages: ArcSwap<bool>,
}

impl RelayState {
    /// Build the state from a configuration and its compiled snapshot.
    pub fn new(config: &Config, snapshot: RoutingSnapshot, upstream: Upstream) -> Self {
        let keys = key_ring(config);
        Self {
            table: ArcSwap::from_pointee(RouteTable::new(snapshot)),
            upstream,
            keys: ArcSwap::from_pointee(keys),
            keys_declared: std::sync::atomic::AtomicBool::new(!config.client_keys.is_empty()),
            config_dir: mr_core::config::config_dir(),
            proxy_selection: ProxySelectionState::new(&config.proxies),
            quota_config: ArcSwap::from_pointee(config.quota.clone()),
            rate_limit_retry: ArcSwap::from_pointee(config.rate_limit_retry.clone()),
            debug_log_messages: ArcSwap::from_pointee(config.debug_log_messages),
        }
    }

    /// The configured quota cooldown settings.
    ///
    /// Read once per request, so a reload that lands mid-request cannot make one step cool an
    /// account down for a different length than another.
    pub fn quota_config(&self) -> std::sync::Arc<mr_core::config::QuotaConfig> {
        self.quota_config.load_full()
    }

    /// The configured silent rate-limit retry settings.
    ///
    /// Read once per request so one request does not observe two different settings.
    pub fn rate_limit_retry_config(&self) -> std::sync::Arc<mr_core::config::RateLimitRetryConfig> {
        self.rate_limit_retry.load_full()
    }

    /// Whether debug logging of admitted request payloads is enabled (`debugLogMessages`).
    pub fn debug_log_messages(&self) -> bool {
        *self.debug_log_messages.load_full()
    }

    /// Load the snapshot and configuration from disk.
    ///
    /// If the snapshot is **missing or stale** it is recompiled from the configuration rather than
    /// treated as an error. A stale snapshot is the normal state after a hand edit to `config.json`,
    /// and refusing to start would mean an operator has to run a second command before the relay
    /// works — which is a footgun, not a safety feature. Recompiling is cheap and idempotent.
    pub fn load() -> mr_core::error::Result<Self> {
        let config = Config::load_default()?;
        let path = mr_core::config::snapshot_path();
        let want = config.routing_digest();

        // The snapshot is a **derived cache**: every byte of it comes from the configuration, so any
        // problem reading it is answered by compiling a fresh one. That is deliberate and it is why
        // this does not have an error arm for a corrupt or truncated file.
        //
        // An earlier version returned the error for anything that was not `NotFound`, which meant a
        // relay could not start on a fresh install at all: `load_from` wrapped every read failure in
        // one variant, so the "compile me a fresh one" arm was unreachable and the process exited
        // instead. A cache that can be rebuilt must never be able to stop the program.
        let snapshot = match RoutingSnapshot::load_from(&path) {
            Ok(s) if s.digest() == want => s,
            Ok(stale) => {
                tracing::info!(
                    snapshot = stale.digest(),
                    config = want,
                    "compiled snapshot is stale; recompiling from the configuration"
                );
                Self::recompile(&config, &path)?
            }
            Err(e) => {
                tracing::info!(
                    error = %e,
                    path = %path.display(),
                    "no usable compiled snapshot; compiling from the configuration"
                );
                Self::recompile(&config, &path)?
            }
        };

        let upstream = Upstream::from_latency(&config.latency).map_err(|e| {
            // The client is built from a static root store and ALPN list, so this cannot fail for
            // anything the operator did. Surfacing it as a configuration error would send them
            // looking in `config.json` for a problem that is not there.
            mr_core::error::Error::InvalidState(format!("could not build the upstream client: {e}"))
        })?;
        Ok(Self::new(&config, snapshot, upstream))
    }

    /// Compile the configuration into a snapshot and cache it, without making the cache load-bearing.
    ///
    /// A failure to write is logged and ignored. The relay can serve correctly from the value it holds
    /// in memory, and refusing to start because a cache file could not be written would be the tail
    /// wagging the dog — particularly since the next start would simply recompile again.
    fn recompile(
        config: &Config,
        path: &std::path::Path,
    ) -> mr_core::error::Result<RoutingSnapshot> {
        let fresh = RoutingSnapshot::compile(config)?;
        if let Err(e) = fresh.write_to(path) {
            tracing::warn!(error = %e, "could not persist the compiled snapshot");
        }
        Ok(fresh)
    }

    /// The current routing table.
    ///
    /// The guard must be held for the duration of a request. Dropping it early and re-loading would
    /// allow a `reload` to swap the table mid-request, so a request could pick a step from one
    /// snapshot and resolve its provider from another.
    pub fn table(&self) -> arc_swap::Guard<Arc<RouteTable>> {
        self.table.load()
    }

    /// Install a freshly compiled snapshot, keeping the process serving throughout.
    pub fn install(&self, snapshot: RoutingSnapshot) {
        self.table.store(Arc::new(RouteTable::new(snapshot)));
    }

    /// Re-read the configuration and recompile.
    pub fn reload(&self) -> mr_core::error::Result<u64> {
        let config = Config::load_default()?;
        // A reload must not install what startup would refuse, or the same file would work until
        // the next restart and then brick the gateway. Advisories are logged and tolerated, exactly
        // as at startup.
        for advisory in config
            .validate()
            .into_iter()
            .filter(|p| mr_core::config::is_advisory(p))
        {
            tracing::warn!(problem = %advisory, "configuration advisory");
        }
        let problems = config.validate_fatal();
        if !problems.is_empty() {
            return Err(mr_core::error::Error::Config(format!(
                "refusing to reload an invalid configuration: {}",
                problems.join("; ")
            )));
        }
        let snapshot = RoutingSnapshot::compile(&config)?;
        let digest = snapshot.digest();
        if let Err(e) = snapshot.write_to(&mr_core::config::snapshot_path()) {
            tracing::warn!(error = %e, "could not persist the recompiled snapshot");
        }
        self.install_reloaded(&config, snapshot);
        Ok(digest)
    }

    /// Install everything a successfully loaded configuration resolved to.
    ///
    /// Split out of [`reload`](Self::reload) only so a test can drive it with a configuration it
    /// built itself: `reload` reads the real config directory, which a unit test must not write.
    ///
    /// Every section here is one an operator expects a reload to pick up. Client keys live in the
    /// same file as the routing table, so refreshing only the table would leave a revoked key
    /// working until restart. Cooldown lengths are policy rather than routing state and so are not
    /// part of the snapshot — but leaving them behind would make `quota` the one section of
    /// config.json that silently needs a restart.
    fn install_reloaded(&self, config: &Config, snapshot: RoutingSnapshot) {
        self.keys.store(Arc::new(key_ring(config)));
        self.keys_declared.store(
            !config.client_keys.is_empty(),
            std::sync::atomic::Ordering::Relaxed,
        );
        self.quota_config
            .store(std::sync::Arc::new(config.quota.clone()));
        self.install(snapshot);
    }

    /// The shared upstream client.
    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    /// The proxy-selection counters, shared by every worker.
    pub fn proxy_selection(&self) -> &ProxySelectionState {
        &self.proxy_selection
    }

    /// The configuration directory this instance was started against.
    pub fn config_dir(&self) -> &std::path::Path {
        &self.config_dir
    }

    /// Whether any client keys are configured.
    pub fn keys_are_configured(&self) -> bool {
        !self.keys.load().is_empty()
    }

    /// The number of configured keys.
    pub fn key_count(&self) -> usize {
        self.keys.load().len()
    }

    /// Hash a presented key for comparison.
    ///
    /// Hex, lowercase, matching what `mr key create` stores. One encoding for both sides: a mismatch
    /// between the two would present as every key being rejected, which is at least loud, but there
    /// is no reason to have two.
    pub fn hash_presented(key: &str) -> [u8; 32] {
        let mut h = Sha256::new();
        h.update(key.as_bytes());
        h.finalize().into()
    }

    /// Authenticate a request.
    ///
    /// Returns the matching key, or `None` when the request must be refused:
    ///
    /// * No keys configured and no credential presented — **allowed**. An unconfigured router on
    ///   loopback is a development setup, and demanding a key before it can be used at all would be
    ///   hostile to the first five minutes of use.
    /// * No keys configured but a credential presented — allowed, and ignored. The client believes
    ///   it is authenticating; refusing would break a working setup for no security gain, since
    ///   there is nothing to authenticate against.
    /// * Keys configured and none presented — refused.
    /// * Keys configured and a wrong one presented — refused.
    /// * Client keys configured but none of them loadable (every `key_hash` malformed) — refused
    ///   as well, rather than reopened: a broken configuration must not remove authentication.
    ///
    /// Hashes are checked in configuration order, returning immediately at the first match. This
    /// makes successful authentication cost proportional to the match position; it does not conceal
    /// the matching key's position from an observer able to measure the relay.
    pub fn authenticate(&self, presented: Option<&str>) -> AuthOutcome {
        let keys = self.keys.load();
        if keys.is_empty() {
            // Keys are declared but not one of them could be loaded. Refusing is the safe
            // direction: returning `Open` would publish a relay the operator believes is protected.
            if self
                .keys_declared
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                return match presented {
                    Some(_) => AuthOutcome::Rejected,
                    None => AuthOutcome::Missing,
                };
            }
            return AuthOutcome::Open;
        }
        let Some(key) = presented else {
            return AuthOutcome::Missing;
        };
        let hash = Self::hash_presented(key);
        // Key hashes are compared in configured order. Stop at the first match to keep the common
        // path proportional to the matching key's position rather than scanning all configured keys.
        for entry in keys.iter() {
            if entry.matches(&hash) {
                return AuthOutcome::Ok(entry.clone());
            }
        }
        AuthOutcome::Rejected
    }
}

/// The result of authenticating a request.
#[derive(Debug, Clone)]
pub enum AuthOutcome {
    /// A configured key matched.
    Ok(KeyEntry),
    /// No keys are configured, so the instance is open.
    Open,
    /// Keys are configured and none was presented.
    Missing,
    /// A key was presented and did not match.
    Rejected,
}

impl AuthOutcome {
    /// Whether the request may proceed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, AuthOutcome::Ok(_) | AuthOutcome::Open)
    }

    /// The key id, when a key matched. For logs.
    pub fn key_id(&self) -> Option<&str> {
        match self {
            AuthOutcome::Ok(k) => Some(&k.id),
            _ => None,
        }
    }
}

/// Build the client-key ring for a configuration.
fn key_ring(config: &Config) -> Vec<KeyEntry> {
    config
        .client_keys
        .iter()
        .filter_map(KeyEntry::from_config)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A client key with the fields the tests care about, and defaults for the rest.
    ///
    /// Written once rather than per test: eleven fields is enough that five copies would drift, and
    /// a drifted fixture is a test that passes for the wrong reason.
    fn client_key(
        id: &str,
        label: &str,
        hash_hex: &str,
        enabled: bool,
    ) -> mr_core::model::ClientKey {
        mr_core::model::ClientKey {
            id: mr_core::ids::ClientKeyId::from_raw(id),
            label: label.to_owned(),
            key_hash: hash_hex.to_owned(),
            prefix: None,
            enabled,
            allow_models: Vec::new(),
            allow_routes: Vec::new(),
            allow_combos: Vec::new(),
            limit_requests: 0,
            limits: Default::default(),
            note: None,
        }
    }

    fn entry(label: &str, key: &str) -> KeyEntry {
        KeyEntry {
            id: format!("id-{label}"),
            label: label.to_owned(),
            hash: RelayState::hash_presented(key),
            allow_models: Vec::new(),
            allow_routes: Vec::new(),
            allow_combos: Vec::new(),
            limit_requests: 0,
            limits: Arc::new(Default::default()),
        }
    }

    fn state_with(keys: Vec<KeyEntry>) -> RelayState {
        // A minimal snapshot: no providers, no combos. Authentication does not touch it, and an
        // empty table is the honest fixture for a test that is only about keys.
        let _config = Config::load_default().unwrap_or_else(|_| Config::default());
        let snapshot = RoutingSnapshot::default();
        RelayState {
            table: ArcSwap::from_pointee(RouteTable::new(snapshot)),
            upstream: Upstream::default_pool().expect("a client can always be built"),
            keys: ArcSwap::from_pointee(keys),
            keys_declared: std::sync::atomic::AtomicBool::new(false),
            config_dir: std::path::PathBuf::from("/tmp"),
            proxy_selection: ProxySelectionState::empty(),
            quota_config: ArcSwap::from_pointee(mr_core::config::QuotaConfig::default()),
            rate_limit_retry: ArcSwap::from_pointee(
                mr_core::config::RateLimitRetryConfig::default(),
            ),
            debug_log_messages: ArcSwap::from_pointee(false),
        }
    }

    /// A reload must replace the cooldown policy, not just the routing table.
    ///
    /// The regression this guards is silent by construction: the relay logs "routing table
    /// reloaded" and then keeps cooling accounts down for the old lengths, so an operator sees a
    /// successful reload and a policy that did not take effect.
    #[test]
    fn a_reload_replaces_the_quota_cooldown_policy() {
        let s = state_with(Vec::new());
        assert_eq!(
            s.quota_config().default_cooldown_secs,
            mr_core::config::QuotaConfig::default().default_cooldown_secs,
            "the fixture should start from the default policy"
        );

        let mut config = Config::default();
        config.quota.default_cooldown_secs = 7;
        config.quota.auth_failure_cooldown_secs = 11;
        s.install_reloaded(&config, RoutingSnapshot::default());

        let after = s.quota_config();
        assert_eq!(after.default_cooldown_secs, 7);
        assert_eq!(after.auth_failure_cooldown_secs, 11);
    }

    #[test]
    fn a_matching_key_authenticates() {
        let s = state_with(vec![entry("laptop", "mr-abc123")]);
        let out = s.authenticate(Some("mr-abc123"));
        assert!(out.is_allowed());
        assert_eq!(out.key_id(), Some("id-laptop"));
    }

    #[test]
    fn a_wrong_key_is_refused() {
        let s = state_with(vec![entry("laptop", "mr-abc123")]);
        assert!(matches!(
            s.authenticate(Some("mr-wrong")),
            AuthOutcome::Rejected
        ));
    }

    #[test]
    fn an_absent_key_is_refused_only_when_keys_exist() {
        // The distinction that matters for the first five minutes of use: an unconfigured instance
        // is open, and a configured one is not.
        let open = state_with(Vec::new());
        assert!(open.authenticate(None).is_allowed());
        assert!(matches!(open.authenticate(None), AuthOutcome::Open));

        let closed = state_with(vec![entry("laptop", "mr-abc123")]);
        assert!(!closed.authenticate(None).is_allowed());
        assert!(matches!(closed.authenticate(None), AuthOutcome::Missing));
    }

    #[test]
    fn the_right_key_among_several_is_found() {
        let s = state_with(vec![
            entry("a", "mr-aaa"),
            entry("b", "mr-bbb"),
            entry("c", "mr-ccc"),
        ]);
        assert_eq!(s.authenticate(Some("mr-bbb")).key_id(), Some("id-b"));
    }

    #[test]
    fn duplicate_matching_hash_uses_first_configured_key() {
        let s = state_with(vec![
            entry("first", "mr-shared"),
            entry("second", "mr-shared"),
        ]);
        assert_eq!(s.authenticate(Some("mr-shared")).key_id(), Some("id-first"));
    }

    #[test]
    fn hashing_is_stable_and_hex_comparable() {
        // The stored hash and the presented-key hash must use one encoding. Two encodings would
        // present as every key being rejected, so this pins the representation.
        let h = RelayState::hash_presented("mr-abc123");
        let hex: String = h.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex.to_lowercase());
        assert_eq!(h, RelayState::hash_presented("mr-abc123"));
        assert_ne!(h, RelayState::hash_presented("mr-abc124"));
    }

    #[test]
    fn a_disabled_key_never_authenticates() {
        let key = client_key("k-old", "old", &"a".repeat(64), false);
        assert!(KeyEntry::from_config(&key).is_none());
    }

    #[test]
    fn a_malformed_stored_hash_disables_that_key_rather_than_the_process() {
        // One bad row must not take the relay down. The safe direction is to refuse the key.
        let key = client_key("k-broken", "broken", "not-a-hash", true);
        assert!(KeyEntry::from_config(&key).is_none());
    }

    #[test]
    fn a_stored_hash_round_trips_through_hex() {
        let hash = RelayState::hash_presented("mr-secret-value");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let key = client_key("k-good", "good", &hex, true);
        let entry = KeyEntry::from_config(&key).expect("a well-formed hash parses");
        assert!(entry.matches(&hash));
    }

    /// A compiled snapshot with one provider, two models and two accounts.
    ///
    /// Authored as JSON because that is the shape an operator's file arrives in, and because it
    /// keeps the fixture to the fields these tests are about: a hand-built `ModelEntry` is twenty
    /// fields of noise around the one the counter key reads.
    fn two_model_snapshot() -> RoutingSnapshot {
        let config: Config = serde_json::from_str(
            r#"{
                "providers": [{
                    "id": "p",
                    "dialect": "open-ai",
                    "base_url": "https://example.test/v1",
                    "key_strategy": "round-robin",
                    "accounts": [{"id": "first"}, {"id": "second"}],
                    "models": [{"id": "m"}, {"id": "n"}]
                }]
            }"#,
        )
        .expect("the fixture is a valid configuration");
        RoutingSnapshot::compile(&config).expect("the fixture compiles")
    }

    /// A compiled step owns its account rotation counter, and advancing it advances the counter.
    ///
    /// This is the regression: the request path built its own `provider#model` key, the table
    /// allocated counters for combos only, and an unknown key answered `0`. Every request therefore
    /// rotated by zero — the identity — so `round-robin` selected like `priority` for every provider
    /// and every model, silently, because a served request is a served request.
    #[test]
    fn a_compiled_step_has_an_account_counter_that_advances() {
        let table = RouteTable::new(two_model_snapshot());
        let step = &table.snapshot.providers()[0].steps[0];

        assert_eq!(table.advance_account_rr(step), 0);
        assert_eq!(
            table.advance_account_rr(step),
            1,
            "the second request on a step must read a counter selection can rotate by"
        );
    }

    /// Two models on one provider rotate their accounts independently.
    ///
    /// Keying by provider alone would couple them, so traffic to one model would decide which account
    /// the other used — an operator-visible surprise, and the reason the key carries both halves of
    /// the step.
    #[test]
    fn each_model_on_a_provider_advances_its_own_account_counter() {
        let table = RouteTable::new(two_model_snapshot());
        let provider = &table.snapshot.providers()[0];
        let (first, second) = (&provider.steps[0], &provider.steps[1]);

        assert_eq!(table.advance_account_rr(first), 0);
        assert_eq!(
            table.advance_account_rr(second),
            0,
            "another model's first request is still a first request"
        );
        assert_eq!(table.advance_account_rr(first), 1);
    }

    #[test]
    fn upstream_wires_configured_latency_pool_settings() {
        let mut cfg = Config::default();
        cfg.latency.pool_idle_secs = 42;
        cfg.latency.pool_max_idle_per_host = 7;
        let client = Upstream::from_latency(&cfg.latency).unwrap();
        assert_eq!(
            client.pool_settings(),
            (std::time::Duration::from_secs(42), 7)
        );
    }
}
