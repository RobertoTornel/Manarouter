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
//!
//! ## GPU rentals
//!
//! [`gpu`] holds the pure logic behind renting a GPU: the verified card catalog, the HuggingFace
//! architecture reader, the KV-cache sizing maths and the re-quantisation recipe. It is deliberately
//! free of I/O and of the clock, and [`rental`] holds the operator's authored rental accounts and
//! deployments, so a sizing formula or a price can be checked in a test rather than against an
//! invoice.

pub mod catalog;
pub mod config;
pub mod dialect;
pub mod error;
pub mod gpu;
pub mod ids;
pub mod lane;
pub mod model;
pub mod oauth;
pub mod placeholders;
pub mod published;
pub mod quota;
pub mod rental;
pub mod secret;
pub mod snapshot;
pub mod store;
pub mod usage;

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
    Account, Alias, ClientKey, Combo, ComboStep, CompressionMode, CompressionSpec, CooldownScope,
    HedgeSpec, Http2Preference, ModelEntry, ModelSource, Provider, QuotaWindow,
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
///
/// Version 3 added the per-step reasoning-level fields (`levels`, `default_level`,
/// `reasoning_json`), `input_modalities` and `pricing`, plus the proxy pool and the per-key limit
/// table. A version-2 snapshot decodes *without* those, so this bump is what stops an old file
/// being read as though it carried them.
///
/// Version 4 completed Feature 3: a step's level metadata is now **encoded**, and every lookup
/// entry carries the level it resolves to. Version 3 declared the fields but never wrote them, so
/// a v3 frame would decode with empty level tables.
///
/// Version 5 adds compiled client-key limits and proxy assignments/settings to the payload; a v4
/// frame has neither, and cannot safely stand in for a complete routing plan.
///
/// Version 6 adds modality fallback defaults to the compiled routing plan. A v5 frame has no such
/// defaults, so it must be rejected and rebuilt from authored config.
///
/// Version 7 adds each provider's **cooldown scope** to the compiled plan. A v6 frame has no such
/// field, and its provider records are one byte shorter, so reading one would shift every following
/// byte of the record — it must be rejected rather than reinterpreted.
///
/// Version 8 carries provider KV capacity, cost-per-second and cold-start timeout configuration.
///
/// Version 9 adds each account's **named placeholder values** — the account id a Cloudflare base
/// URL asks for, the account name a Snowflake hostname asks for. A v8 account record carries no
/// such map, so a base URL with braces would reach the upstream literally; the frame must be
/// rebuilt from authored config instead of reinterpreted.
///
/// Version 10 adds each provider's **hidden model ids**, so a miss can say "that model is hidden"
/// instead of "no such model" — a typo and a checkbox need different fixes. A v9 frame carries no
/// such list, so a hidden model would be reported as nonexistent; rebuilding from authored config
/// is the only way to recover it.
///
/// Version 22 adds the capability tags for audio translation, image editing and variations,
/// moderation, video, realtime, the vector-store/container/fine-tuning/eval management surfaces and
/// Search (tags 11 through 21). A v10 reader that reached one of those records would fail inside
/// the tag decoder with only a byte offset to show for it; the version mismatch at the frame header
/// makes it reject the whole snapshot and rebuild from authored config instead.
///
/// The constant is deliberately kept **greater than every capability tag**, not merely one past the
/// previous version. That keeps the header check a complete guard for readers of this build too: a
/// capability appended later without a version bump would otherwise pass the header and fail only
/// when the decoder reached its tag. The test `the_snapshot_version_stays_ahead_of_every_capability_tag`
/// enforces the invariant.
pub const SNAPSHOT_VERSION: u32 = 22;

/// Current on-disk database schema version.
///
/// Migrations are numbered and applied in order, but they live *in code* — the ordered `MIGRATIONS`
/// table in [`store`], applied through `PRAGMA user_version`. The `migrations/` directory at the
/// repository root is not used; there are no `.sql` files to apply.
///
/// Version 3 added the `usage_tokens` side table (cached tokens, cache-write tokens, and the
/// generation window that tokens-per-second divides by). Version 4 added the `key_usage_bytes`
/// side table: the request and response byte counts the relay observes, which the panel process
/// turns into a token estimate when the provider reported no counts of its own. Version 5 adds
/// `gpu_usage`, the append-only billing record for self-hosted provider GPU seconds.
pub const SCHEMA_VERSION: u32 = 5;
