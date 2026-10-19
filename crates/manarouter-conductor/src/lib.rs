//! # manarouter-conductor — the control plane
//!
//! This process owns everything the data plane refuses to know: the configuration, the panel, combo
//! authoring, OAuth, quota accounting, and the compilation of the routing snapshot.
//!
//! It is **never on the request path**. That is not a description of how it happens to be deployed;
//! it is the property the whole architecture exists to preserve. If a change makes the relay wait
//! for this process, the change is wrong regardless of how convenient it is — see
//! `docs/01-architecture.md` §2.
//!
//! ## Why the allocator is *not* pinned here
//!
//! The relay declares mimalloc as its global allocator because it is on the hot path. The conductor
//! deliberately does not: it is a long-lived configuration server whose dominant costs are SQLite
//! pages, JSON serialisation and TLS, none of which the allocator chooses. Adding a second allocator
//! dependency here would buy nothing measurable and would make the two processes' memory behaviour
//! harder to compare.
//!
//! ## Module map
//!
//! * [`auth`] — panel authentication: Argon2id passwords, sessions, origin checks, and the refusal
//!   to start a remotely-reachable panel with no password.
//! * [`state`] — the shared application state: the configuration behind a lock, the vault, the
//!   compiled snapshot handle, the restart signal, and the commit sequence every mutation goes
//!   through (validate → compile → save → install).
//! * [`api`] — the REST surface the panel calls, plus the guard middleware (origin check, then
//!   session) and the redaction/healing rules that keep credentials out of responses without
//!   making round trips lossy.
//! * [`serve`] — binding the control listener and serving `panel/` static assets; the place
//!   where a bind failure becomes an actionable error instead of a panic.

pub mod api;
pub mod auth;
pub mod models_api;
pub mod quota_api;
pub mod serve;
pub mod state;
