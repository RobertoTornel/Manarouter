//! Proactive and forced credential refresh: single-flight admission, failure classification,
//! and vault persistence ordering.
//!
//! Contents (filled in next): `CredentialStore` (the `Vault` adapter), `Refresher` with
//! per-credential single-flight, `ensure_fresh(id, force)` for 401 repair, transient-vs-permanent
//! classification, and the persistence-before-success ordering tests.
