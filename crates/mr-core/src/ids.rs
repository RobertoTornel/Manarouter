//! Typed identifiers.
//!
//! Every identifier is a UUIDv7 rendered as a string. Two properties matter and both are
//! requirements of the v2 cross-instance sync groundwork described in `docs/22-sync-design.md`:
//!
//! 1. **Globally unique and time-ordered.** UUIDv7 embeds a millisecond timestamp, so ids sort by
//!    creation time while remaining collision-free across machines. An autoincrement integer would
//!    collide the moment two instances merged.
//! 2. **Stable across export and import.** The store never regenerates an id, so a record that is
//!    copied to another machine keeps its identity and a merge can tell "the same record changed"
//!    from "a new record appeared".
//!
//! The newtypes exist so that a provider id can never be passed where an account id is expected.

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Generate a new time-ordered identifier.
///
/// UUIDv7 is used rather than v4 specifically so that ids created in sequence sort in creation
/// order, which makes the oplog and any future merge deterministic to reason about.
pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

/// Monotonic per-record revision counter.
///
/// Incremented on every write. A future sync layer compares revisions to decide which side of a
/// conflict wins, and the oplog records them so nothing is silently discarded.
pub type Revision = u64;

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an existing identifier string.
            ///
            /// Used when loading from storage or a migration rather than when creating a record,
            /// so it deliberately does not validate the format: a migrated id from another system
            /// may legitimately not be a UUID, and rejecting it would lose data.
            pub fn from_raw(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            /// Mint a fresh identifier.
            pub fn generate() -> Self {
                Self(new_id())
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume into the underlying string.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

typed_id!(
    /// Identifies a provider entry, for example `openadapter` or `command-code`.
    ///
    /// This is the human-facing slug and is used in model names (`openadapter/DeepSeek-V4.1-Flash`),
    /// so unlike every other identifier it is chosen by the operator and is not a UUID.
    ProviderId
);

typed_id!(
    /// Identifies one credential within a provider: one API key, or one OAuth account.
    AccountId
);

typed_id!(
    /// Identifies a combo.
    ComboId
);

typed_id!(
    /// Identifies a client-facing API key that callers use to authenticate to the relay.
    ClientKeyId
);

typed_id!(
    /// Identifies a provider connection in the panel that does not correspond to a real provider,
    /// used for user-defined OpenAI-compatible endpoints.
    NodeId
);

typed_id!(
    /// Identifies a stored OAuth credential set.
    OAuthCredentialId
);

/// A wall-clock instant, stored as milliseconds since the Unix epoch.
///
/// Milliseconds rather than seconds because quota windows are frequently only minutes long, and
/// because latency reporting needs sub-second resolution. A signed integer is used so that a clock
/// skew into the past is representable rather than wrapping to a huge future timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Millis(pub i64);

impl Millis {
    /// The current instant.
    pub fn now() -> Self {
        Self(now_millis())
    }

    /// Construct from a raw millisecond count.
    pub const fn from_millis(v: i64) -> Self {
        Self(v)
    }

    /// The raw millisecond count.
    pub const fn as_millis(self) -> i64 {
        self.0
    }

    /// Whether this instant is strictly before `other`.
    pub const fn is_before(self, other: Self) -> bool {
        self.0 < other.0
    }

    /// This instant moved forward by `secs` seconds.
    pub const fn plus_secs(self, secs: i64) -> Self {
        Self(self.0.saturating_add(secs.saturating_mul(1000)))
    }

    /// This instant moved forward by `ms` milliseconds.
    pub const fn plus_millis(self, ms: i64) -> Self {
        Self(self.0.saturating_add(ms))
    }

    /// How many milliseconds until `self`, or zero if it is already in the past.
    pub fn remaining_ms_until(self, now: Millis) -> i64 {
        (self.0 - now.0).max(0)
    }

    /// Render as an RFC 3339 timestamp for the panel and logs.
    pub fn to_rfc3339(self) -> String {
        use time::format_description::well_known::Rfc3339;
        time::OffsetDateTime::from_unix_timestamp_nanos((self.0 as i128).saturating_mul(1_000_000))
            .ok()
            .and_then(|dt| dt.format(&Rfc3339).ok())
            .unwrap_or_else(|| self.0.to_string())
    }
}

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

/// Milliseconds since the Unix epoch, or zero if the clock is before the epoch.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A 64-bit content hash used for model-name lookup in the routing snapshot and for deriving
/// session-affinity keys.
///
/// FNV-1a is chosen over SipHash because it is evaluated on the hot path on a handful of bytes and
/// is not exposed to adversarial input: the model name comes from the operator's own configuration
/// and the session key is never used for a security decision. It is also trivially reproducible in
/// tests, so a snapshot compiled on one machine hashes identically on another.
#[inline]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_unique_and_time_ordered() {
        let a = ProviderId::generate();
        let b = ProviderId::generate();
        assert_ne!(a, b);
        // UUIDv7 embeds a millisecond timestamp, so ids minted in order sort in order. This is
        // what makes the oplog deterministic to replay.
        assert!(a.as_str() < b.as_str(), "{a} should sort before {b}");
    }

    #[test]
    fn ids_are_transparent_in_json() {
        // Ids appear as bare strings on the wire; wrapping them in an object would leak the
        // newtype into the panel's JSON for no benefit.
        let id = AccountId::from_raw("acc-1");
        let s = serde_json::to_string(&id).unwrap();
        assert_eq!(s, "\"acc-1\"");
        let back: AccountId = serde_json::from_str(&s).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn ids_accept_non_uuid_values_from_migration() {
        // A migrated OmniRoute connection id is not a UUID, and losing it would break the
        // idempotency of a re-run migration.
        let id = AccountId::from_raw("74f1c2a0-legacy");
        assert_eq!(id.as_str(), "74f1c2a0-legacy");
    }

    #[test]
    fn millis_arithmetic_and_countdown_never_go_negative() {
        let now = Millis::from_millis(1_000_000);
        let later = now.plus_secs(5);
        assert_eq!(later.as_millis(), 1_005_000);
        assert_eq!(later.remaining_ms_until(now), 5_000);
        // A reset that has already passed must report zero remaining, not a negative countdown,
        // otherwise the scheduler's "is it available yet" test inverts.
        assert_eq!(now.remaining_ms_until(later), 0);
        assert_eq!(now.plus_millis(-100_000).remaining_ms_until(now), 0);
    }

    #[test]
    fn millis_survives_a_clock_before_the_epoch_without_wrapping() {
        // A machine with a wrong clock must not produce a reset timestamp that looks like it is
        // decades in the future.
        let past = Millis::from_millis(-5_000);
        assert_eq!(past.as_millis(), -5_000);
        assert!(past.is_before(Millis::from_millis(0)));
        // `remaining_ms_until(now)` is the time from `now` to `self`, so a deadline already gone
        // must report zero rather than a nonsensical positive wait.
        assert_eq!(past.remaining_ms_until(Millis::from_millis(0)), 0);
        assert_eq!(Millis::from_millis(0).remaining_ms_until(past), 5_000);
        assert_eq!(
            Millis::from_millis(3_000).remaining_ms_until(Millis::from_millis(1_000)),
            2_000
        );
    }

    #[test]
    fn millis_formats_as_rfc3339() {
        let m = Millis::from_millis(1_700_000_000_000);
        let s = m.to_rfc3339();
        assert!(s.starts_with("2023-11-14T"), "unexpected rendering: {s}");
    }

    #[test]
    fn fnv1a64_is_stable_across_runs_and_sensitive_to_input() {
        // A golden value: if this ever changes, every previously compiled snapshot becomes
        // unreadable, so the change must be accompanied by a snapshot version bump.
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        // A real model id, so the vector covers a realistic string length and character set rather
        // than only the trivial empty input.
        assert_eq!(fnv1a64(b"oad/DeepSeek-V4.1-Flash"), 0x5276_9e22_79cd_3e45);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"b"), 0xaf63_df4c_8601_f1a5);
        assert_eq!(fnv1a64(b"ab"), 0x089c_4407_b545_986a);
        assert_eq!(fnv1a64(b"ba"), 0x08a6_3307_b54d_d00c);
        // Order sensitivity is the property that makes this safe to key a routing table on: if the
        // hash were commutative, two models with permuted names would collide.
        assert_ne!(fnv1a64(b"a"), fnv1a64(b"b"));
        assert_ne!(fnv1a64(b"ab"), fnv1a64(b"ba"));
    }

    #[test]
    fn fnv1a64_handles_a_long_input_without_wrapping_badly() {
        // A long model id must still produce a well-distributed value; a hash that collapses long
        // inputs would silently merge distinct routes.
        let a = vec![b'x'; 4096];
        let mut b = a.clone();
        *b.last_mut().unwrap() = b'y';
        assert_ne!(fnv1a64(&a), fnv1a64(&b));
    }

    #[test]
    fn fnv1a64_is_not_identity_on_single_bytes() {
        // Guards against a transcription error in the prime or offset basis, which would leave the
        // function looking plausible while being useless.
        assert_ne!(fnv1a64(b"a"), 0);
        assert_ne!(fnv1a64(b"a"), 0xcbf2_9ce4_8422_2325);
    }
}
