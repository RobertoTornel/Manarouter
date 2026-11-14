//! `mr account` — add, inspect, enable/disable and reprioritise the credentials of a provider.
//!
//! Credentials enter the vault, never the config: `config.json` only ever gains a `secret_ref`
//! id of the form `provider/<p>/account/<a>`, and nothing in this module prints a stored
//! credential (only a `stored: sk-…wxyz` fingerprint line after a successful add).

use std::io::{IsTerminal, Read};

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
    /// Declare the account's remaining credit, in US dollars.
    ///
    /// Fine-tuning only starts a job on an account known to hold more than $15, and most
    /// providers do not publish a dollar balance the router can read. Declaring it here is how an
    /// OpenAI-style account becomes eligible; `--clear` returns it to "unknown".
    SetCredit {
        /// Provider id.
        provider: String,
        /// Account id or label.
        account: String,
        /// Remaining balance in US dollars, for example `25` or `12.50`.
        #[arg(long)]
        usd: Option<f64>,
        /// Forget the declared balance instead of setting one.
        #[arg(long, conflicts_with = "usd")]
        clear: bool,
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
    /// Remaining credit in US dollars; fine-tuning only spends an account known to hold more
    /// than $15.
    #[arg(long, value_name = "USD")]
    pub credit_usd: Option<f64>,
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
        AccountCmd::SetCredit {
            provider,
            account,
            usd,
            clear,
        } => set_credit(ctx, &provider, &account, usd, clear),
    }
}

// ---------------------------------------------------------------------------
// credential sources
// ---------------------------------------------------------------------------

/// Resolve `--secret` / `--secret-stdin` / `--secret-env` — exactly one, or a usage error
/// explaining all three. The stdin branch reads from `stdin` so tests can drive the real
/// `--secret-stdin` path end to end without owning the process's file descriptor 0.
fn read_secret(
    args: &AddArgs,
    mut stdin: impl Read,
    interactive: bool,
) -> Result<String, CliError> {
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
        read_secret_from_stdin(&mut stdin, interactive)?
    };

    let secret = secret.trim();
    if secret.is_empty() {
        return Err(CliError::validation("the credential is empty"));
    }
    // A paste that carried more than one line would otherwise be stored whole and fail later as an
    // opaque upstream 401. Control characters cannot appear in a credential; neither can the tabs
    // and newlines a wrapped paste drags in.
    if secret.chars().any(char::is_control) {
        return Err(CliError::validation(
            "the credential contains a control character (a newline or tab), so more than one \
             line was pasted; nothing was written",
        )
        .with_detail("paste only the key itself, then press Enter once"));
    }
    Ok(secret.to_owned())
}

/// Read a credential from `--secret-stdin`.
///
/// On a terminal the echo is turned off first: `--secret-stdin` exists so the key never appears in
/// `ps`, and leaving echo on would undo that by putting it in scrollback, in the shell's history
/// buffer as rendered text, in `tmux` capture, and in anything recording the session. A pipe is
/// read to EOF exactly as before.
fn read_secret_from_stdin(stdin: &mut impl Read, interactive: bool) -> Result<String, CliError> {
    if !interactive {
        let mut buf = String::new();
        std::io::Read::read_to_string(stdin, &mut buf).map_err(|e| {
            CliError::failure(format!("cannot read the credential from stdin: {e}"))
        })?;
        // Trim only the line ending a paste adds; interior text is checked by the caller.
        return Ok(buf.trim_end_matches(['\n', '\r']).to_owned());
    }

    #[cfg(unix)]
    {
        // File descriptor 0, not the generic reader: the interactive branch only runs when stdin
        // really is the terminal, which is the descriptor the termios calls must act on.
        let fd = libc::STDIN_FILENO;
        let mut term: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut term) } == 0 {
            let original = term;
            term.c_lflag &= !libc::ECHO;
            // TCSANOW: the change has to be in effect before the first keystroke is echoed.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
            eprint!("Paste the credential, then press Enter (input is hidden): ");
            let mut line = String::new();
            let read = std::io::BufRead::read_line(
                &mut std::io::BufReader::new(std::io::stdin()),
                &mut line,
            );
            // Restored unconditionally, before the result is inspected, so an error can never
            // leave the operator's terminal with echo off.
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &original) };
            eprintln!();
            read.map_err(|e| {
                CliError::failure(format!("cannot read the credential from the terminal: {e}"))
            })?;
            return Ok(line.trim_end_matches(['\n', '\r']).to_owned());
        }
    }

    // A terminal that refused the termios calls (rare, non-POSIX): still accept the input, but say
    // plainly that it was visible.
    eprintln!("warning: this terminal cannot hide input, so the credential was echoed as typed");
    let mut buf = String::new();
    std::io::Read::read_to_string(stdin, &mut buf)
        .map_err(|e| CliError::failure(format!("cannot read the credential from stdin: {e}")))?;
    Ok(buf.trim_end_matches(['\n', '\r']).to_owned())
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
    let secret = read_secret(&args, stdin, std::io::stdin().is_terminal())?;

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
    if let Some(usd) = args.credit_usd {
        check_credit_usd(usd)?;
        account.credit_usd = Some(usd);
    }

    let secret_id = account_secret_id(&provider_id, account.id.as_str());
    let mut vault = ctx.vault()?;
    vault.put(&secret_id, &secret).map_err(CliError::from)?;
    vault.flush().map_err(CliError::from)?;
    account.secret_ref = Some(secret_id.clone());

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
    // The secret is in the vault before the config that references it exists. If the save is
    // refused, that entry would be an orphan: no account points at it, so nothing the operator can
    // run would show or remove it. Delete it on the way out rather than leaving ciphertext behind.
    let out = match ctx.save(&cfg) {
        Ok(out) => out,
        Err(e) => {
            if let Ok(mut vault) = ctx.vault() {
                if vault.delete(&secret_id) {
                    let _ = vault.flush();
                }
            }
            return Err(e);
        }
    };
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

    // Open the vault before anything is written, so an unreadable vault fails without changing
    // the configuration.
    let mut vault = if secret_ref.is_some() {
        Some(ctx.vault()?)
    } else {
        None
    };

    // The configuration write is what makes the removal real: `ctx.save` validates before it
    // writes and refuses, for example, a provider that would be left with no accounts at all.
    // Deleting the credential first would destroy it while the account is still in the config.
    let out = ctx.save(&cfg)?;

    let mut deleted_vault_entry = false;
    if let (Some(sid), Some(v)) = (&secret_ref, vault.as_mut()) {
        deleted_vault_entry = v.delete(sid);
        if deleted_vault_entry {
            v.flush().map_err(CliError::from)?;
        }
    }

    // Defensive: someone may have used the secret itself as the label, so the removal message
    // must not repeat it verbatim.
    let label_is_secret = looks_like_a_credential(&label);
    let shown_label = if label_is_secret {
        "<hidden>"
    } else {
        label.as_str()
    };
    let mut human = vec![format!(
        "account `{shown_label}` removed from `{provider_id}`"
    )];
    if deleted_vault_entry {
        human.push("its vault entry was deleted with it".to_owned());
    }
    if label_is_secret {
        human.push("note: the label looked credential-shaped and is not repeated here".to_owned());
    }
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

/// Refuse a credit figure that is not money, before anything is written.
fn check_credit_usd(usd: f64) -> Result<(), CliError> {
    if !usd.is_finite() || usd < 0.0 {
        return Err(CliError::validation(
            "--credit-usd must be a finite, non-negative dollar amount".to_owned(),
        )
        .with_detail("for example `--credit-usd 25` or `--credit-usd 12.50`"));
    }
    Ok(())
}

fn set_credit(
    ctx: &Ctx,
    provider_id: &str,
    account_key: &str,
    usd: Option<f64>,
    clear: bool,
) -> Result<(), CliError> {
    if usd.is_none() && !clear {
        return Err(CliError::usage(
            "set-credit needs --usd <amount>, or --clear to forget a declared balance".to_owned(),
        ));
    }
    if let Some(usd) = usd {
        check_credit_usd(usd)?;
    }
    let mut cfg = ctx.load()?;
    let p = cfg
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == provider_id)
        .ok_or_else(|| CliError::not_found(format!("no provider with id `{provider_id}`")))?;
    let a = find_account_mut(p, account_key)?;
    let label = a.label.clone().unwrap_or_else(|| a.id.as_str().to_owned());
    a.credit_usd = usd;
    let out = ctx.save(&cfg)?;
    let human = match usd {
        Some(usd) => {
            format!("account `{label}` on `{provider_id}` declares a ${usd:.2} credit balance")
        }
        None => format!("account `{label}` on `{provider_id}` no longer declares a credit balance"),
    };
    ctx.finish(
        &out,
        vec![human],
        serde_json::json!({ "provider": provider_id, "account": label, "credit_usd": usd }),
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
            credit_usd: None,
        }
    }

    #[test]
    fn no_secret_source_is_a_usage_error_naming_all_three() {
        let err = read_secret(&args(), std::io::empty(), false).unwrap_err();
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
        let err = read_secret(&a, std::io::empty(), false).unwrap_err();
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
        assert_eq!(read_secret(&a, std::io::empty(), false).unwrap(), "sk-test");
    }

    #[test]
    fn env_source_missing_is_a_validation_error() {
        let mut a = args();
        a.secret_env = Some("MR_TEST_DEFINITELY_NOT_SET_12345".into());
        let err = read_secret(&a, std::io::empty(), false).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Validation);
        assert!(err.message.contains("MR_TEST_DEFINITELY_NOT_SET_12345"));
    }

    #[test]
    fn empty_credentials_are_refused_with_exit_three() {
        let mut a = args();
        a.secret = Some("   ".into());
        let err = read_secret(&a, std::io::empty(), false).unwrap_err();
        assert_eq!(err.exit, crate::output::Exit::Validation);
    }

    #[test]
    fn a_credit_amount_is_a_finite_non_negative_dollar_figure() {
        assert!(check_credit_usd(0.0).is_ok());
        assert!(check_credit_usd(15.01).is_ok());
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let err = check_credit_usd(bad).unwrap_err();
            assert_eq!(err.exit, crate::output::Exit::Validation);
            assert!(err.message.contains("credit"), "{}", err.message);
        }
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
