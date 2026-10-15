//! Proof that an imported grant belongs to *this* installation, not to the reference.
//!
//! `docs/17-migrating-from-omniroute.md` claimed OAuth tokens could not be migrated. The migration
//! refutes that by construction: it never copies a stored access token, it hands the reference's
//! *refresh* token to the platform's own refresh implementation and stores whatever comes back.
//! After that the credential in Manarouter's vault is one Manarouter obtained for itself, so the
//! reference's copy is irrelevant and the reference need not be running.
//!
//! This test is the executable form of that argument. It reads Manarouter's own vault and store —
//! it never opens the reference database — and requires every imported credential to be usable.
//!
//! ## Why it goes through `Refresher`, and not the platform directly
//!
//! **Calling `refresh_with_context` directly is destructive**, and an earlier version of this test
//! did exactly that and burned a live credential. Codex and Antigravity issue *single-use* refresh
//! tokens: presenting one returns a new token and invalidates the old. The platform method returns
//! the rotated set but persists nothing, so the vault keeps the now-dead token and the account is
//! broken from then on — with the failure surfacing later, on a real request, as a 401 that looks
//! like a provider outage rather than a test's side effect.
//!
//! [`Refresher::ensure_fresh`] is the production path, and it persists the rotation through the
//! credential store. It is also single-flight per credential and honours the platform's refresh
//! lead, so a credential that is not yet due is served from the vault without a network call at
//! all. That makes this test safe to re-run: it exercises the real code path, and it cannot leave a
//! stored token stale. It asserts the *observable outcome* — a usable access token for every
//! imported account — rather than assuming a particular account needed a network round trip.
//!
//! Ignored by default: it needs a real installation and may make network calls, so it must never
//! run in CI. Run it deliberately, and set `MANAROUTER_FORCE_REFRESH=1` for the stronger claim:
//!
//! ```text
//! # cheap: proves every imported credential is present and usable
//! cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture
//!
//! # strong: forces a real rotation from every provider, proving the grants are independently
//! # refreshable. Spends a single-use refresh token per account, so do not run it in a loop.
//! MANAROUTER_FORCE_REFRESH=1 \
//!   cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture
//! ```

use std::sync::Arc;

use mr_core::oauth::refresh::{CredentialStore, Refresher, VaultStore};
use mr_core::oauth::{platform, TokenSet};
use mr_core::secret::Vault;
use mr_core::store::Store;

/// A thin wrapper so the refresher owns the write path while the test keeps its own handle.
///
/// `Refresher` takes an `Arc<dyn CredentialStore>`; `VaultStore` already implements the trait, so
/// this only exists to make the delegation explicit at the call site.
struct ReportingStore {
    inner: VaultStore,
}

impl CredentialStore for ReportingStore {
    fn load(&self, key: &str) -> mr_core::error::Result<Option<TokenSet>> {
        self.inner.load(key)
    }

    fn save(&self, key: &str, tokens: &TokenSet) -> mr_core::error::Result<()> {
        self.inner.save(key, tokens)
    }
}

#[tokio::test]
#[ignore = "requires a real installation and may make network calls"]
async fn imported_grants_refresh_without_the_reference() {
    let store = Store::open(&mr_core::config::state_dir().join("state.sqlite"))
        .expect("open the state database");
    // Opened once up front purely to prove the vault is readable; each refresher then takes its
    // own handle, because `Vault` owns a file lock and is not `Clone`.
    Vault::open_default().expect("open the vault");

    let rows = store
        .list_oauth_credentials(false)
        .expect("list credentials");
    assert!(
        !rows.is_empty(),
        "no OAuth credentials are installed; run `mr migrate omniroute --apply` first"
    );

    // `MANAROUTER_FORCE_REFRESH=1` additionally demands a real network rotation from every
    // credential, which is a strictly stronger claim than "usable": with `force = false` the
    // refresher returns the stored token unchanged whenever it is not yet due, so the test can pass
    // in milliseconds without proving the grant can still be *refreshed*. Both modes are useful —
    // the default is cheap and safe to run often, the forced mode is the one that actually exercises
    // the reference-independence claim, at the cost of spending a single-use token per account.
    //
    // Forced mode is opt-in because on a single-use platform it is mildly destructive even when it
    // succeeds: the run rotates every token and persists the replacements, so any *other* copy of
    // those credentials (the reference's) is left dead. It persists what it rotates, which is what
    // makes that acceptable rather than fatal.
    let force = std::env::var("MANAROUTER_FORCE_REFRESH").is_ok_and(|v| v == "1");

    let mut usable = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for row in &rows {
        // The label is only ever used in reporting; no credential is ever printed.
        let label = if row.label.is_empty() {
            row.id.as_str().to_owned()
        } else {
            row.label.clone()
        };

        // The record states where its credential lives, and the platform its own id. Neither is
        // re-derived here, because the id spelling is the record's business.
        let Some(platform_id) = platform(&row.platform) else {
            failures.push(format!(
                "{label}: {} is not a platform in this build",
                row.platform
            ));
            continue;
        };

        // The production refresher, so any rotation is persisted rather than lost.
        let refresher = Refresher::new(
            platform_id,
            Arc::new(ReportingStore {
                inner: VaultStore::open_default().expect("open the vault"),
            }),
        );

        match refresher.ensure_fresh(&row.vault_ref, force).await {
            Ok(tokens) if !tokens.access_token.is_empty() => {
                println!("{}  {label}", if force { "refreshed" } else { "usable   " });
                usable += 1;
            }
            Ok(_) => failures.push(format!(
                "{label}: the refresh returned an empty access token"
            )),
            Err(e) => failures.push(format!("{label}: refresh failed: {e}")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} imported grants are not usable:\n  {}",
        failures.len(),
        rows.len(),
        failures.join("\n  ")
    );
    let verb = if force {
        "refreshed through the provider"
    } else {
        "usable without the reference"
    };
    println!("{usable} imported grants are {verb}");
}
