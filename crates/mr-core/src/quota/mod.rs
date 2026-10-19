//! Provider quota: reading what an upstream says about an account's remaining allowance.
//!
//! This module is the Rust rewrite of the per-provider usage fetchers in OmniRoute
//! (`open-sse/services/usage/`, MIT, © 2026 diegosouzapw). OmniRoute's dispatcher
//! (`getUsageForProvider`) is a `switch` over ~45 provider ids returning one normalized
//! `UsageQuota` shape; this module keeps the *shape* and the *normalisation rules* and replaces the
//! dispatcher with a registry of [`registry::QuotaFetcher`] implementations.
//!
//! Three rules carried over deliberately, because they are the difference between a quota panel
//! that is useful and one that lies:
//!
//! * **`0` is not always exhausted.** When an upstream omits the remaining fraction, the reported
//!   zero is a sentinel; [`types::UsageQuota::fraction_reported`] records which it is.
//! * **A window is a duration, not a name.** `session` may mean five hours or thirty days; the
//!   label follows the real window length ([`window::WindowFamily`]) so a 30-day window is never
//!   presented as a 5-hour one.
//! * **Reset instants are parsed per family, never guessed.** [`types::parse_reset_time`] handles
//!   the several shapes upstreams use and returns `None` rather than inventing a time.
//!
//! Nothing here runs on the request path: fetching a quota is a control-plane operation that
//! reaches the network, and the relay never calls it.

pub mod http;
pub mod registry;
pub mod types;
pub mod window;

// Phase C provider fetchers. One file per provider family, so each is written by exactly
// one worker; `registry` decides which of them answers for a given account.
pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod commandcode;
pub mod cursor;
pub mod deepseek;
pub mod github;
pub mod glm;
pub mod grok;
pub mod kimi;
pub mod minimax;
pub mod moonshot;
pub mod openai_compat;
pub mod openrouter;
pub mod qwen;
