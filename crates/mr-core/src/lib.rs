//! # mr-core
//!
//! Shared foundation for Manarouter: the configuration model, the compiled routing snapshot that
//! the data plane reads, the on-disk store, and the secret vault.
//!
//! With the single exception of the [`oauth`] login flows (which must reach each provider's
//! token endpoint to exchange and refresh credentials), nothing in this crate talks to the
//! network. Everything else is pure data, so both the control plane (which authors and compiles)
//! and the data plane (which reads a compiled snapshot) depend on it without pulling in an HTTP
//! stack of their own.
//!
//! ## The two representations
//!
//! Manarouter keeps two deliberately separate descriptions of the same routing configuration:
//!
//! * The **authored** form in [`model`], [`config`] and [`store`]. Rich, self-describing, friendly
//!   to hand-editing and to the panel. This is what the operator changes.
//! * The **compiled** form in [`snapshot`]. A flat, `repr(C)`-ish layout that the relay reads out
//!   of shared memory without invoking a parser.
//!
//! The compiler lives in [`snapshot`] and is the only thing that bridges them. This is what lets
//! combo logic be arbitrarily sophisticated while the request path stays a pointer load, a hash
//! lookup and a couple of atomic operations.

pub mod catalog;
pub mod config;
pub mod dialect;
pub mod error;
pub mod ids;
pub mod model;
pub mod oauth;
pub mod published;
pub mod secret;
pub mod snapshot;
pub mod store;

pub use error::{Error, Result};

// The vocabulary is re-exported at the crate root because it is referenced by nearly every module
// and by both binaries; requiring a fully qualified path everywhere would add noise without adding
// clarity.
pub use dialect::{
    AccountState, AffinityMode, AuthKind, Capability, ComboStrategy, Dialect, KeyStrategy,
    MidstreamPolicy, ProviderHealth, RouteKind, WindowKind, WinnerFailMode, WinnerMetric,
};
pub use ids::{
    fnv1a64, now_millis, AccountId, ClientKeyId, ComboId, Millis, NodeId, OAuthCredentialId,
    ProviderId, Revision,
};
pub use model::{
    Account, Alias, ClientKey, Combo, ComboStep, CompressionMode, CompressionSpec, HedgeSpec,
    Http2Preference, ModelEntry, ModelSource, Provider, QuotaWindow,
};

/// Default host for the data plane (the relay).
pub const DEFAULT_RELAY_HOST: &str = "127.0.0.1";

/// Default port for the data plane.
///
/// Chosen to avoid OmniRoute's 20129 and 9router's 20127/20128, so all three can run side by side
/// during a migration.
pub const DEFAULT_RELAY_PORT: u16 = 20900;

/// Default host for the control plane (the conductor and its panel).
pub const DEFAULT_CONTROL_HOST: &str = "127.0.0.1";

/// Default port for the control plane.
pub const DEFAULT_CONTROL_PORT: u16 = 20901;

/// The config format version written into every config file.
///
/// Unknown keys in a newer file are preserved on round-trip rather than dropped, so an older
/// instance never destroys a newer instance's fields. See `docs/22-sync-design.md`.
pub const CONFIG_FORMAT_VERSION: u32 = 1;

/// Magic bytes at the head of every compiled routing snapshot.
///
/// The relay refuses to load a file that does not start with these bytes, rather than
/// misinterpreting an unrelated file as routing data.
pub const SNAPSHOT_MAGIC: [u8; 8] = *b"MROUTES1";

/// Current snapshot format version.
///
/// Bumped whenever the compiled layout changes. The relay keeps serving its previous snapshot
/// rather than loading one it does not understand, so a version skew degrades to stale routing
/// instead of to a crash.
pub const SNAPSHOT_VERSION: u32 = 2;

/// Current on-disk database schema version.
///
/// Migrations are numbered and applied in order; see `migrations/` and [`store`].
pub const SCHEMA_VERSION: u32 = 1;
