//! The relay's shared state: the compiled routing table, the upstream client, and client keys.
//!
//! # What is here, and what is deliberately not
//!
//! There is exactly **one** piece of mutable state on the request path: a relaxed `AtomicU64` per
//! combo, used by the `round-robin` strategy. Everything else — the routing table, the providers,
//! the accounts — is loaded once, behind an `ArcSwap`, and read without a lock.
//!
//! That is not an optimisation for its own sake. A mutex on this path is a queue, and a queue is
//! latency that grows with load precisely when latency matters most. The compiled snapshot exists so
//! that a request needs no coordination at all: it loads an `Arc`, walks a slice, and sends bytes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use mr_core::config::Config;
use mr_core::ids::ComboId;
use mr_core::snapshot::RoutingSnapshot;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::upstream::Upstream;

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
    /// One relaxed counter per combo, indexed by position in `snapshot.combos`.
    ///
    /// Relaxed ordering is correct here and not a shortcut. The counter orders *nothing*: no thread
    /// synchronises with another through it, and a lost update means one request goes to the next
    /// step instead of this one. Paying for `SeqCst` to order a decision that nothing observes would
    /// be buying a fence with no buyer.
    rr: Vec<AtomicU64>,
    /// Combo id to its position in `snapshot.combos`, for the counter lookup.
    combo_pos: HashMap<ComboId, usize>,
}

impl RouteTable {
    /// Build a table around a snapshot, allocating one counter per combo.
    pub fn new(snapshot: RoutingSnapshot) -> Self {
        let rr = (0..snapshot.combos().len())
            .map(|_| AtomicU64::new(0))
            .collect();
        let combo_pos = snapshot
            .combos()
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id.clone(), i))
            .collect();
        Self {
            snapshot,
            rr,
            combo_pos,
        }
    }

    /// Advance and read the round-robin counter for a combo.
    ///
    /// Returns `0` for a combo with no counter, which can only happen if the caller's combo came
    /// from a different table generation — impossible while a request holds one guard, but a `0`
    /// makes that harmless rather than a panic.
    pub fn advance_rr(&self, combo: &ComboId) -> u64 {
        match self.combo_pos.get(combo) {
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
        })
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
    keys: Vec<KeyEntry>,
    /// The configuration directory, so a reload knows where to look.
    config_dir: std::path::PathBuf,
}

impl RelayState {
    /// Build the state from a configuration and its compiled snapshot.
    pub fn new(config: &Config, snapshot: RoutingSnapshot, upstream: Upstream) -> Self {
        let keys = config
            .client_keys
            .iter()
            .filter_map(KeyEntry::from_config)
            .collect();
        Self {
            table: ArcSwap::from_pointee(RouteTable::new(snapshot)),
            upstream,
            keys,
            config_dir: mr_core::config::config_dir(),
        }
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

        let snapshot = match RoutingSnapshot::load_from(&path) {
            Ok(s) if s.digest() == want => s,
            Ok(stale) => {
                tracing::info!(
                    snapshot = stale.digest(),
                    config = want,
                    "compiled snapshot is stale; recompiling from the configuration"
                );
                let fresh = RoutingSnapshot::compile(&config)?;
                // A failure to persist is not fatal: the relay can serve correctly from memory, and
                // refusing to start because a cache file could not be written would be the tail
                // wagging the dog.
                if let Err(e) = fresh.write_to(&path) {
                    tracing::warn!(error = %e, "could not persist the recompiled snapshot");
                }
                fresh
            }
            Err(mr_core::error::Error::NotFound(_)) => {
                tracing::info!("no compiled snapshot; compiling from the configuration");
                let fresh = RoutingSnapshot::compile(&config)?;
                if let Err(e) = fresh.write_to(&path) {
                    tracing::warn!(error = %e, "could not persist the compiled snapshot");
                }
                fresh
            }
            Err(e) => return Err(e),
        };

        let upstream = Upstream::default_pool().map_err(|e| {
            // The client is built from a static root store and ALPN list, so this cannot fail for
            // anything the operator did. Surfacing it as a configuration error would send them
            // looking in `config.json` for a problem that is not there.
            mr_core::error::Error::InvalidState(format!("could not build the upstream client: {e}"))
        })?;
        Ok(Self::new(&config, snapshot, upstream))
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
        let snapshot = RoutingSnapshot::compile(&config)?;
        let digest = snapshot.digest();
        if let Err(e) = snapshot.write_to(&mr_core::config::snapshot_path()) {
            tracing::warn!(error = %e, "could not persist the recompiled snapshot");
        }
        self.install(snapshot);
        Ok(digest)
    }

    /// The shared upstream client.
    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    /// The configuration directory this instance was started against.
    pub fn config_dir(&self) -> &std::path::Path {
        &self.config_dir
    }

    /// Whether any client keys are configured.
    pub fn keys_are_configured(&self) -> bool {
        !self.keys.is_empty()
    }

    /// The number of configured keys.
    pub fn key_count(&self) -> usize {
        self.keys.len()
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
    pub fn authenticate(&self, presented: Option<&str>) -> AuthOutcome {
        if self.keys.is_empty() {
            return AuthOutcome::Open;
        }
        let Some(key) = presented else {
            return AuthOutcome::Missing;
        };
        let hash = Self::hash_presented(key);
        // Every key is compared, and the loop does not break on a match. Breaking early would make
        // the response time depend on which key matched, which tells an observer the position of a
        // valid key in the list.
        let mut matched: Option<&KeyEntry> = None;
        for entry in &self.keys {
            if entry.matches(&hash) {
                matched = Some(entry);
            }
        }
        match matched {
            Some(entry) => AuthOutcome::Ok(entry.clone()),
            None => AuthOutcome::Rejected,
        }
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
            limit_requests: 0,
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
            keys,
            config_dir: std::path::PathBuf::from("/tmp"),
        }
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
}
