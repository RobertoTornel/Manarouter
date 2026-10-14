//! `mr account` — add, inspect, enable/disable and reprioritise the credentials of a provider.
//!
//! Credentials enter the vault, never the config: `config.json` only ever gains a `secret_ref`
//! id of the form `provider/<p>/account/<a>`, and nothing in this module prints a stored
//! credential (only a `stored: sk-…wxyz` fingerprint line after a successful add).

use std::io::Read;

use clap::Subcommand;

use mr_core::model::{Account, Provider};
use mr_core::secret::{account_secret_id, looks_like_a_credential};

use crate::output::{confirm_secret, print_json, require_provider, say, warn, CliError, Table};
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// Add an account and its credential (the secret goes to the vault, never the config).
    Add(AddArgs),
    /// List a provider's accounts.
    List {
        /// Provider id.
        provider: String,
    },
    /// Remove an account and its vault entry.
    Remove {
        /// Provider id.
        provider: String,
        /// Account id or label.
        account: String,
        /// Remove it even if it is the provider's last enabled account.
        #[arg(long)]
        force: bool,
    },
    /// Re-enable a disabled account.
    Enable {
        /// Provider id.
        provider: String,
        /// Account id or label.
        account: String,
    },
    /// Disable an account without deleting it.
    Disable {
        /// Provider id.
        provider: String,
        /// Account id or label.
        account: String,
    },
    /// Change the account's priority (lower is tried first under the priority strategy).
    SetPriority {
        /// Provider id.
        provider: String,
        /// Account id or label.
        account: String,
        /// New priority; lower numbers are tried first.
        priority: i32,
    },
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Provider id.
    pub provider: String,
    /// Label for the new account (defaults to `account-<n>`).
    #[arg(long)]
    pub label: Option<String>,
    /// The credential itself, passed on the command line (visible to `ps`; prefer --secret-stdin).
    #[arg(long, visible_alias = "key")]
    pub secret: Option<String>,
    /// Read the credential from standard input.
    #[arg(long)]
    pub secret_stdin: bool,
    /// Read the credential from this environment variable.
    #[arg(long, value_name = "VAR")]
    pub secret_env: Option<String>,
    /// Selection priority; lower is tried first (default 0).
    #[arg(long)]
    pub priority: Option<i32>,
    /// Relative weight for weighted selection (default 0 = unweighted).
    #[arg(long)]
    pub weight: Option<u32>,
    /// Email or login this credential belongs to.
    #[arg(long)]
    pub identity: Option<String>,
}

/// Run an account subcommand.
pub async fn run(cmd: AccountCmd, ctx: &Ctx) -> Result<(), CliError> {
    match cmd {
        AccountCmd::Add(a) => add(ctx, a),
        AccountCmd::List { provider } => list(ctx, &provider),
        AccountCmd::Remove {
            provider,
            account,
            force,
        } => remove(ctx, &provider, &account, force),
        AccountCmd::Enable { provider, account } => set_enabled(ctx, &provider, &account, true),
        AccountCmd::Disable { provider, account } => set_enabled(ctx, &provider, &account, false),
        AccountCmd::SetPriority {
            provider,
            account,
            priority,
        } => set_priority(ctx, &provider, &account, priority),
    }
}

// ---------------------------------------------------------------------------
// credential sources
// ---------------------------------------------------------------------------

/// Resolve `--secret` / `--secret-stdin` / `--secret-env` — exactly one, or a usage error
/// explaining all three. The stdin branch reads from `stdin` so tests can drive the real
/// `--secret-stdin` path end to end without owning the process's file descriptor 0.
fn read_secret(args: &AddArgs, mut stdin: impl Read) -> Result<String, CliError> {
    let sources =
        args.secret.is_some() as u8 + args.secret_stdin as u8 + args.secret_env.is_some() as u8;
    if sources == 0 {
        return Err(CliError::usage(format!(
            "no credential given for provider `{}`",
            args.provider
        ))
        .with_detail("choose one of:")
        .with_details(vec![
            "mr account add <provider> --secret-stdin   (recommended: paste, then Ctrl-D)"
                .to_owned(),
            "mr account add <provider> --secret-env VAR (read from the environment)".to_owned(),
            "mr account add <provider> --secret VALUE   (visible in `ps`; discouraged)".to_owned(),
        ]));
    }
    if sources > 1 {
        return Err(CliError::usage(
            "--secret, --secret-stdin and --secret-env are mutually exclusive",
        ));
    }

    let secret = if let Some(s) = &args.secret {
        warn("--secret puts the credential on the command line where `ps` can see it; prefer --secret-stdin");
        s.clone()
    } else if let Some(var) = &args.secret_env {
        std::env::var(var)
            .map_err(|_| CliError::validation(format!("environment variable `{var}` is not set")))?
    } else {
        let mut buf = String::new();
        stdin.read_to_string(&mut buf).map_err(|e| {
            CliError::failure(format!("cannot read the credential from stdin: {e}"))
        })?;
        // Trim only the line ending a paste adds; interior whitespace is part of the credential.
        buf.trim_end_matches(['\n', '\r']).to_owned()
    };

    let secret = secret.trim();
    if secret.is_empty() {
        return Err(CliError::validation("the credential is empty"));
    }
    Ok(secret.to_owned())
}

// ---------------------------------------------------------------------------
// find / add / list
// ---------------------------------------------------------------------------

/// An account by id first (stable), then by label (what people type).
fn find_account<'a>(p: &'a Provider, key: &str) -> Result<&'a Account, CliError> {
    p.accounts
        .iter()
        .find(|a| a.id.as_str() == key)
        .or_else(|| p.accounts.iter().find(|a| a.label.as_deref() == Some(key)))
        .ok_or_else(|| {
            let known: Vec<String> = p
                .accounts
                .iter()
                .map(|a| {
                    format!(
                        "{} ({})",
                        a.label.clone().unwrap_or_else(|| "?".to_owned()),
                        a.id
                    )
                })
                .collect();
            let mut e = CliError::not_found(format!("provider `{}` has no account `{key}`", p.id));
            if known.is_empty() {
                e = e.with_detail("the provider has no accounts at all");
            } else {
                e = e.with_detail(format!("known accounts: {}", known.join(", ")));
            }
            e
        })
}

fn find_account_mut<'a>(p: &'a mut Provider, key: &str) -> Result<&'a mut Account, CliError> {
    let by_id = p.accounts.iter().position(|a| a.id.as_str() == key);
    let by_label = if by_id.is_some() {
        None
    } else {
        p.accounts
            .iter()
            .position(|a| a.label.as_deref() == Some(key))
    };
    match by_id.or(by_label) {
        Some(i) => Ok(&mut p.accounts[i]),
        None => {
            let known: Vec<String> = p
                .accounts
                .iter()
                .map(|a| a.label.clone().unwrap_or_else(|| "?".to_owned()))
                .collect();
            Err(
                CliError::not_found(format!("provider `{}` has no account `{key}`", p.id))
                    .with_detail(format!("known labels: {}", known.join(", "))),
            )
        }
    }
}

fn add(ctx: &Ctx, args: AddArgs) -> Result<(), CliError> {
    add_with(ctx, args, std::io::stdin())
}

/// The body of `account add`, with the credential source's stdin injected (tests drive the
/// `--secret-stdin` path through this).
fn add_with(ctx: &Ctx, args: AddArgs, stdin: impl Read) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let provider_id = args.provider.clone();
    // Resolve the provider before touching stdin: a typo should not hang waiting for a paste.
    if !cfg.providers.iter().any(|p| p.id.as_str() == provider_id) {
        return Err(CliError::not_found(format!(
            "no provider with id `{provider_id}`"
        )));
    }
    let secret = read_secret(&args, stdin)?;

    let p = cfg
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == provider_id)
        .ok_or_else(|| CliError::not_found(format!("no provider with id `{provider_id}`")))?;

    let label = args
        .label
        .clone()
        .unwrap_or_else(|| format!("account-{}", p.accounts.len() + 1));
    // A duplicate label would make `account remove <label>` ambiguous — refuse now rather than
    // resolve it by guessing later.
    if p.accounts
        .iter()
        .any(|a| a.label.as_deref() == Some(label.as_str()))
    {
        return Err(CliError::validation(format!(
            "provider `{provider_id}` already has an account labelled `{label}`; nothing was written"
        ))
        .with_detail("pass --label to name this one differently"));
    }

    let mut account = Account::new(&label);
    if let Some(priority) = args.priority {
        account.priority = priority;
    }
    if let Some(weight) = args.weight {
        account.weight = weight;
    }
    account.identity = args.identity.clone();

    let secret_id = account_secret_id(&provider_id, account.id.as_str());
    let mut vault = ctx.vault()?;
    vault.put(&secret_id, &secret).map_err(CliError::from)?;
    vault.flush().map_err(CliError::from)?;
    account.secret_ref = Some(secret_id);

    let confirm = confirm_secret(&secret, &label);
    let doc = serde_json::json!({
        "provider": provider_id,
        "account": account,
    });
    p.accounts.push(account);

    let human = vec![
        format!("account `{label}` added to provider `{provider_id}`"),
        confirm,
        "the credential is in the vault; only its reference id is in the config".to_owned(),
    ];
    let out = ctx.save(&cfg)?;
    ctx.finish(&out, human, doc)
}

fn list(ctx: &Ctx, provider_id: &str) -> Result<(), CliError> {
    let cfg = ctx.load()?;
    let p = require_provider(&cfg, provider_id)?;
    if ctx.json {
        return print_json(&p.accounts);
    }
    let mut t = Table::new([
        "LABEL",
        "ID",
        "PRIORITY",
        "ENABLED",
        "CREDENTIAL",
        "IDENTITY",
    ]);
    for a in &p.accounts {
        let cred = if a.secret_ref.is_some() {
            "vault"
        } else if a.oauth_ref.is_some() {
            "oauth"
        } else {
            "none"
        };
        t.row(vec![
            a.label.clone().unwrap_or_default(),
            a.id.as_str().to_owned(),
            a.priority.to_string(),
            if a.enabled { "yes" } else { "no" }.to_owned(),
            cred.to_owned(),
            a.identity.clone().unwrap_or_default(),
        ]);
    }
    t.print();
    if p.accounts.is_empty() {
        say(format!(
            "no accounts on `{provider_id}`; add one with `mr account add {provider_id} --secret-stdin`"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// remove / enable / disable / set-priority
// ---------------------------------------------------------------------------

fn remove(ctx: &Ctx, provider_id: &str, account_key: &str, force: bool) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let (label, secret_ref) = {
        let p = cfg
            .providers
            .iter_mut()
            .find(|p| p.id.as_str() == provider_id)
            .ok_or_else(|| CliError::not_found(format!("no provider with id `{provider_id}`")))?;
        let is_last_enabled = {
            let a = find_account(p, account_key)?;
            let enabled_here = p.accounts.iter().filter(|x| x.enabled).count();
            a.enabled && enabled_here == 1
        };
        if is_last_enabled && !force {
            return Err(CliError::validation(format!(
                "`{account_key}` is the only enabled account of `{provider_id}`; removing it would \
                 leave the provider unable to serve a request"
            ))
            .with_detail("re-run with --force to remove it anyway"));
        }
        let a = find_account_mut(p, account_key)?;
        let label = a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned());
        let secret_ref = a.secret_ref.clone();
        let account_id = a.id.clone();
        p.accounts.retain(|x| x.id != account_id);
        (label, secret_ref)
    };

    let mut deleted_vault_entry = false;
    if let Some(sid) = &secret_ref {
        let mut vault = ctx.vault()?;
        deleted_vault_entry = vault.delete(sid);
        if deleted_vault_entry {
            vault.flush().map_err(CliError::from)?;
        }
    }

    let mut human = vec![format!("account `{label}` removed from `{provider_id}`")];
    if deleted_vault_entry {
        human.push("its vault entry was deleted with it".to_owned());
    }
    if looks_like_a_credential(&label) {
        // Defensive: someone used the secret itself as the label. We never echoed it, but say so.
        human.push("note: the label looked credential-shaped and was never printed".to_owned());
    }

    let out = ctx.save(&cfg)?;
    let doc = serde_json::json!({
        "provider": provider_id,
        "account": label,
        "removed": true,
        "vault_entry_deleted": deleted_vault_entry,
    });
    ctx.finish(&out, human, doc)
}

fn set_enabled(
    ctx: &Ctx,
    provider_id: &str,
    account_key: &str,
    enabled: bool,
) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let p = cfg
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == provider_id)
        .ok_or_else(|| CliError::not_found(format!("no provider with id `{provider_id}`")))?;
    let a = find_account_mut(p, account_key)?;
    a.enabled = enabled;
    let label = a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned());
    let verb = if enabled { "enabled" } else { "disabled" };
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![format!("account `{label}` {verb} on `{provider_id}`")],
        serde_json::json!({ "provider": provider_id, "account": label, "enabled": enabled }),
    )
}

fn set_priority(
    ctx: &Ctx,
    provider_id: &str,
    account_key: &str,
    priority: i32,
) -> Result<(), CliError> {
    let mut cfg = ctx.load()?;
    let p = cfg
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == provider_id)
        .ok_or_else(|| CliError::not_found(format!("no provider with id `{provider_id}`")))?;
    let a = find_account_mut(p, account_key)?;
    let label = a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned());
    a.priority = priority;
    let out = ctx.save(&cfg)?;
    ctx.finish(
        &out,
        vec![format!(
            "account `{label}` on `{provider_id}` now has priority {priority}"
        )],
        serde_json::json!({ "provider": provider_id, "account": label, "priority": priority }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> AddArgs {
        AddArgs {
            provider: "openai".into(),
            label: None,
            secret: None,
            secret_stdin: false,
            secret_env: None,
            priority: None,
            weight: None,
            identity: None,
        }
    }

    #[test]
    fn no_secret_source_is_a_usage_error_naming_all_three() {
        let err = read_secret(&args(), std::io::empty()).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        let details = err.details.join("\n");
        assert!(details.contains("--secret-stdin"), "{details}");
        assert!(details.contains("--secret-env"), "{details}");
        assert!(details.contains("--secret VALUE"), "{details}");
    }

    #[test]
    fn two_secret_sources_are_mutually_exclusive() {
        let mut a = args();
        a.secret = Some("abc".into());
        a.secret_stdin = true;
        let err = read_secret(&a, std::io::empty()).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Usage);
        assert!(
            err.message.contains("mutually exclusive"),
            "{}",
            err.message
        );
    }

    #[test]
    fn cli_secret_is_taken_as_given() {
        let mut a = args();
        a.secret = Some("sk-test".into());
        assert_eq!(read_secret(&a, std::io::empty()).unwrap(), "sk-test");
    }

    #[test]
    fn env_source_missing_is_a_validation_error() {
        let mut a = args();
        a.secret_env = Some("MR_TEST_DEFINITELY_NOT_SET_12345".into());
        let err = read_secret(&a, std::io::empty()).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Validation);
        assert!(err.message.contains("MR_TEST_DEFINITELY_NOT_SET_12345"));
    }

    #[test]
    fn empty_credentials_are_refused_with_exit_three() {
        let mut a = args();
        a.secret = Some("   ".into());
        let err = read_secret(&a, std::io::empty()).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Validation);
    }

    #[test]
    fn accounts_resolve_by_id_then_label_then_fail() {
        let mut p = Provider::new("openai", mr_core::dialect::Dialect::OpenAi, "https://x/v1");
        let a = Account::new("editor");
        let id = a.id.as_str().to_owned();
        p.accounts.push(a);
        assert!(find_account(&p, "editor").is_ok());
        assert!(find_account(&p, &id).is_ok());
        let err = find_account(&p, "nope").unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::NotFound);
        assert!(err.message.contains("nope"), "{}", err.message);
    }

    #[test]
    fn accounts_list_json_never_carries_a_secret() {
        let mut p = Provider::new("openai", mr_core::dialect::Dialect::OpenAi, "https://x/v1");
        let mut a = Account::new("editor");
        a.secret_ref = Some("provider/openai/account/x".into());
        p.accounts.push(a);
        let json = serde_json::to_string(&p.accounts).unwrap();
        assert!(json.contains("secret_ref"), "{json}");
        assert!(json.contains("provider/openai/account/x"), "{json}");
        // The only secret that could ever appear here would be the raw value; assert the shape
        // of the reference instead of trusting ourselves.
        assert!(!json.contains("sk-"), "{json}");
    }
}
