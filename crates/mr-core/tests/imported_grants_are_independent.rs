//! Proof that an imported grant belongs to *this* installation, not to the reference.
//!
//! `docs/17-migrating-from-omniroute.md` claimed OAuth tokens could not be migrated. The migration
//! refutes that by construction: it never copies a stored access token, it hands the reference's
//! *refresh* token to the platform's own `refresh_with_context` and stores whatever comes back.
//! After that the credential in Manarouter's vault is one Manarouter obtained for itself, so the
//! reference's copy is irrelevant and the reference need not be running.
//!
//! This test is the executable form of that argument. It reads Manarouter's own vault and store —
//! it never opens the reference database — refreshes every imported credential with Manarouter's
//! own platform implementation, and requires all of them to succeed.
//!
//! Ignored by default: it needs a real installation and makes network calls, so it must never run
//! in CI. Run it deliberately:
//!
//! ```text
//! cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture
//! ```

use mr_core::oauth::{platform, TokenSet};
use mr_core::secret::Vault;
use mr_core::store::Store;

#[tokio::test]
#[ignore = "requires a real installation and makes network calls"]
async fn imported_grants_refresh_without_the_reference() {
    let store = Store::open(&mr_core::config::state_dir().join("state.sqlite"))
        .expect("open the state database");
    let vault = Vault::open_default().expect("open the vault");

    let rows = store
        .list_oauth_credentials(false)
        .expect("list credentials");
    assert!(
        !rows.is_empty(),
        "no OAuth credentials are installed; run `mr migrate omniroute --apply` first"
    );

    let mut refreshed = 0usize;
    let mut failures: Vec<String> = Vec::new();

    for row in &rows {
        // The label is only ever used in reporting; no credential is ever printed.
        let label = if row.label.is_empty() {
            row.id.as_str().to_owned()
        } else {
            row.label.clone()
        };
        let Some(platform_id) = platform(&row.platform) else {
            failures.push(format!(
                "{label}: {} is not a platform in this build",
                row.platform
            ));
            continue;
        };
        // The record states where its credential lives; the id is not re-derived here, because
        // two conventions exist in this codebase and only the record knows which one it used.
        let Some(raw) = vault.get(&row.vault_ref).expect("read the vault") else {
            failures.push(format!("{label}: no vault entry at {}", row.vault_ref));
            continue;
        };
        let tokens: TokenSet = match serde_json::from_str(&raw) {
            Ok(t) => t,
            Err(e) => {
                failures.push(format!("{label}: the stored token set is unreadable ({e})"));
                continue;
            }
        };
        let Some(refresh_token) = tokens.refresh_token.clone() else {
            failures.push(format!("{label}: no refresh token is stored"));
            continue;
        };

        // The claim under test: this succeeds using only Manarouter's credential and the
        // platform's own public client identity. Nothing here opens the reference.
        match platform_id
            .refresh_with_context(&refresh_token, &tokens)
            .await
        {
            Ok(next) => {
                assert!(
                    !next.access_token.is_empty(),
                    "{label}: the refresh returned an empty access token"
                );
                println!("refreshed  {label}");
                refreshed += 1;
            }
            Err(e) => failures.push(format!("{label}: refresh failed: {e}")),
        }
    }

    assert!(
        failures.is_empty(),
        "{} of {} imported grants did not refresh:\n  {}",
        failures.len(),
        rows.len(),
        failures.join("\n  ")
    );
    println!("{refreshed} imported grants refreshed without the reference");
}
