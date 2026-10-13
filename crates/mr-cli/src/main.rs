//! `mr` — the Manarouter control CLI.
//!
//! One binary, thirteen subcommands, four exit codes beyond success. The shape of every flag is
//! documented in `docs/02-quickstart.md` and the exit-code contract below is part of `--help`;
//! both are held by tests in this file.
//!
//! Two invariants shape everything here:
//!
//! 1. **A credential is never printed in full** except the one-time plaintext of `key create`
//!    (see [`output`]). `config.json` holds only id references and hashes.
//! 2. **An invalid configuration never reaches disk.** Every mutation goes through [`Ctx::save`],
//!    which validates, compiles the routing snapshot in memory, and only then writes; a refusal
//!    leaves the previous files untouched and exits 3.

mod account;
mod alias;
mod combo;
mod config;
mod doctor;
mod key;
mod migrate;
mod models;
mod output;
mod provider;
mod serve;
mod vault;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use mr_core::config::Config;
use mr_core::snapshot::RoutingSnapshot;
use output::{say, warn, CliError, Exit};

/// Documented in `mr --help`; asserted by tests.
const EXIT_CODES: &str = "\
EXIT CODES
  0  success
  1  general failure (I/O, network, vault, a child process that died)
  2  usage error (unknown command or flag, missing argument, bad enum value, out-of-range index)
  3  validation failure (the change would put an invalid configuration on disk, or the existing
     configuration is already invalid)
  4  not found (provider, combo, account, client key or alias)
";

#[derive(Parser, Debug)]
#[command(
    name = "mr",
    version,
    about = "Manarouter control CLI: providers, accounts, combos, keys, migration, serve",
    after_help = EXIT_CODES,
    subcommand_required = true,
    arg_required_else_help = true
)]
pub struct Cli {
    /// Operate on this configuration directory instead of the default. Also honoured when set as
    /// the MANAROUTER_CONFIG_DIR environment variable; the flag wins.
    #[arg(long, global = true, value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Print machine-readable JSON from read commands instead of tables.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Add, inspect, test, enable and remove providers.
    #[command(subcommand)]
    Provider(provider::ProviderCmd),
    /// Add, list, remove and re-prioritise accounts (credentials) on a provider.
    #[command(subcommand)]
    Account(account::AccountCmd),
    /// Named model routes: add, list, show, remove, and edit their steps.
    #[command(subcommand)]
    Combo(combo::ComboCmd),
    /// List, add and remove alternative model spellings.
    #[command(subcommand)]
    Alias(alias::AliasCmd),
    /// Client keys the relay accepts from callers.
    #[command(subcommand)]
    Key(key::KeyCmd),
    /// Sync and list a provider's model catalogue.
    #[command(subcommand)]
    Models(models::ModelsCmd),
    /// Inspect and rekey the encrypted secret vault.
    #[command(subcommand)]
    Vault(vault::VaultCmd),
    /// Show, validate and edit the configuration file.
    #[command(subcommand)]
    Config(config::ConfigCmd),
    /// Run the relay (data plane) and conductor (control plane) together.
    Serve(serve::ServeCmd),
    /// Import a previous OmniRoute installation (reads a copy, dry-run by default).
    Migrate(migrate::MigrateCmd),
    /// Check the installation: config, credentials, routing snapshot, ports.
    Doctor,
    /// Print version and configuration locations.
    Version,
}

/// Everything a subcommand needs from the invocation: the output mode. The configuration
/// directory resolves through mr-core (flag > `MANAROUTER_CONFIG_DIR` > platform default) and is
/// set before anything asks for a path.
#[derive(Debug, Clone, Copy)]
pub struct Ctx {
    pub json: bool,
}

impl Ctx {
    /// The configuration directory in effect.
    pub fn config_dir(&self) -> PathBuf {
        mr_core::config::config_dir()
    }

    /// The configuration file path.
    pub fn config_path(&self) -> PathBuf {
        mr_core::config::config_path()
    }

    /// Load the configuration, defaulting to an empty one on first run.
    pub fn load(&self) -> Result<Config, CliError> {
        Config::load_default().map_err(CliError::from)
    }

    /// Open (and, if needed, create) the secret vault in the configuration directory.
    pub fn vault(&self) -> Result<mr_core::secret::Vault, CliError> {
        mr_core::secret::Vault::open_default().map_err(CliError::from)
    }

    /// Persist a changed configuration — the only write path in this CLI.
    ///
    /// Order matters and is tested: validate (advisory problems warn, real problems refuse with
    /// exit 3 and write nothing), compile the routing snapshot in memory (a snapshot that will not
    /// compile means the router could not serve this config, so nothing is written), save
    /// `config.json`, then write the snapshot. If the snapshot write fails after the config write,
    /// the error says exactly that, and `mr doctor` will report the mismatch.
    ///
    /// Printing is deliberately *not* done here: [`Ctx::finish`] merges this outcome into the
    /// command's own report so `--json` emits exactly one document.
    pub fn save(&self, cfg: &Config) -> Result<SaveOutcome, CliError> {
        let (problems, advisories) = partition_problems(cfg.validate());
        for a in &advisories {
            warn(a);
        }
        if !problems.is_empty() {
            return Err(CliError::validation(
                "the updated configuration is not valid; nothing was written",
            )
            .with_details(problems));
        }
        let snapshot = RoutingSnapshot::compile(cfg).map_err(|e| {
            CliError::validation(format!(
                "the updated configuration cannot be compiled into a routing snapshot; \
                 nothing was written: {e}"
            ))
        })?;
        let digest = snapshot.digest();

        let config_path = self.config_path();
        // `Config::save` writes a caller-chosen path; `save_default` targets config_path(), which
        // is the same directory --config-dir / MANAROUTER_CONFIG_DIR resolved above.
        cfg.save_default().map_err(CliError::from)?;

        let snapshot_path = mr_core::config::snapshot_path();
        snapshot.write_to(&snapshot_path).map_err(|e| {
            CliError::failure(format!(
                "configuration was written to {}, but the routing snapshot could not be updated \
                 at {}: {e}; run `mr doctor` to see the mismatch",
                config_path.display(),
                snapshot_path.display()
            ))
        })?;

        Ok(SaveOutcome {
            config: config_path,
            snapshot: snapshot_path,
            digest,
        })
    }

    /// Report a completed [`Ctx::save`] alongside a command's own result.
    ///
    /// `human` lines print first, then the two write confirmations. `extra_json` must be a JSON
    /// object; it is merged with `config`, `snapshot` and `digest` and printed as the single
    /// document `--json` readers get.
    pub fn finish(
        &self,
        out: &SaveOutcome,
        human: Vec<String>,
        extra_json: serde_json::Value,
    ) -> Result<(), CliError> {
        if self.json {
            let mut doc = extra_json;
            let obj = doc.as_object_mut().ok_or_else(|| {
                CliError::failure("internal: finish() extra_json must be an object")
            })?;
            obj.insert(
                "config".into(),
                serde_json::Value::String(out.config.display().to_string()),
            );
            obj.insert(
                "snapshot".into(),
                serde_json::Value::String(out.snapshot.display().to_string()),
            );
            obj.insert(
                "digest".into(),
                serde_json::Value::String(format!("{:#x}", out.digest)),
            );
            output::print_json(&doc)
        } else {
            for line in human {
                say(line);
            }
            say(format!("configuration written to {}", out.config.display()));
            say(format!(
                "routing snapshot written to {} (digest {:#x})",
                out.snapshot.display(),
                out.digest
            ));
            Ok(())
        }
    }
}

/// What a completed [`Ctx::save`] wrote, for [`Ctx::finish`].
#[derive(Debug, Clone)]
pub struct SaveOutcome {
    pub config: PathBuf,
    pub snapshot: PathBuf,
    pub digest: u64,
}

/// Split `Config::validate`'s output into real problems (refuse) and advisories (warn).
///
/// Two advisory classes exist, both spelled by mr-core itself:
/// - a provider with no credential yet ("has no accounts") — the normal state between
///   `mr provider add` and `mr account add`, and refusing it would make the README's own
///   quickstart impossible;
/// - an account with no credential attached when the upstream may need none ("has no credential
///   attached") — mr-core's own comment on that one is "Not fatal".
///
/// Everything else — unknown providers, duplicate ids, unreachable aliases, a combo with no
/// enabled steps — is a reason not to write.
pub fn partition_problems(all: Vec<String>) -> (Vec<String>, Vec<String>) {
    let (advisories, problems) = all
        .into_iter()
        .partition(|p| p.contains("has no accounts") || p.contains("has no credential attached"));
    (problems, advisories)
}

async fn dispatch(command: Command, ctx: &Ctx) -> Result<(), CliError> {
    match command {
        Command::Provider(c) => provider::run(c, ctx).await,
        Command::Account(c) => account::run(c, ctx).await,
        Command::Combo(c) => combo::run(c, ctx).await,
        Command::Alias(c) => alias::run(c, ctx).await,
        Command::Key(c) => key::run(c, ctx).await,
        Command::Models(c) => models::run(c, ctx).await,
        Command::Vault(c) => vault::run(c, ctx).await,
        Command::Config(c) => config::run(c, ctx).await,
        Command::Serve(c) => serve::run(c, ctx).await,
        Command::Migrate(c) => migrate::run(c, ctx).await,
        Command::Doctor => doctor::run(ctx).await,
        Command::Version => version(ctx),
    }
}

/// `mr version`: identity plus where this invocation would read and write.
fn version(ctx: &Ctx) -> Result<(), CliError> {
    if ctx.json {
        output::print_json(&serde_json::json!({
            "name": "mr",
            "version": env!("CARGO_PKG_VERSION"),
            "config_dir": ctx.config_dir().display().to_string(),
            "config_path": ctx.config_path().display().to_string(),
        }))
    } else {
        say(format!("mr {}", env!("CARGO_PKG_VERSION")));
        say(format!("config dir: {}", ctx.config_dir().display()));
        say(format!("config file: {}", ctx.config_path().display()));
        Ok(())
    }
}

fn main() -> ExitCode {
    // `--config-dir` must reach the environment before anything resolves a path, so mr-core's
    // config_dir() sees it. Edition 2021: set_var is a plain, safe function here.
    let cli = Cli::parse();
    if let Some(dir) = &cli.config_dir {
        std::env::set_var("MANAROUTER_CONFIG_DIR", dir);
    }
    // Logging goes to stderr and never replaces a command's own output on stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mr=info".into()),
        )
        .init();

    let ctx = Ctx { json: cli.json };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::from(Exit::Failure.code());
        }
    };
    match runtime.block_on(dispatch(cli.command, &ctx)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e.render());
            ExitCode::from(e.exit.code())
        }
    }
}

#[cfg(test)]
mod tests {
    //! Argument-parsing tests: one per subcommand, plus the exit-code contract.
    use super::*;
    use crate::output::Exit;
    use clap::{error::ErrorKind, CommandFactory};

    fn parse(args: &[&str]) -> Result<Cli, clap::error::Error> {
        Cli::try_parse_from(args)
    }

    // ---- global flags ----------------------------------------------------

    #[test]
    fn help_documents_every_exit_code() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("EXIT CODES"), "{help}");
        // The four documented failures plus success.
        for line in [
            "  0  success",
            "  1  general failure",
            "  2  usage error",
            "  3  validation failure",
            "  4  not found",
        ] {
            assert!(help.contains(line), "help must contain `{line}`");
        }
    }

    #[test]
    fn config_dir_and_json_are_global() {
        // After the subcommand is also accepted, because the flags are global.
        let cli = parse(&["mr", "provider", "list", "--json", "--config-dir", "/tmp/x"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/x")));
        let cli = parse(&["mr", "--config-dir", "/tmp/y", "doctor"]).unwrap();
        assert_eq!(cli.config_dir, Some(PathBuf::from("/tmp/y")));
        assert!(!cli.json);
    }

    #[test]
    fn no_subcommand_is_a_usage_error() {
        let err = parse(&["mr"]).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }

    #[test]
    fn unknown_subcommand_is_a_usage_error() {
        let err = parse(&["mr", "frobnicate"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);
    }

    // ---- provider --------------------------------------------------------

    #[test]
    fn provider_add_parses_the_readme_and_quickstart_shapes() {
        // README quickstart line 82.
        let cli = parse(&[
            "mr",
            "provider",
            "add",
            "openai",
            "--dialect",
            "open-ai",
            "--base-url",
            "https://api.openai.com/v1",
        ])
        .unwrap();
        match cli.command {
            Command::Provider(provider::ProviderCmd::Add(a)) => {
                assert_eq!(a.target, "openai");
                assert_eq!(a.dialect.as_deref(), Some("open-ai"));
                assert_eq!(a.base_url.as_deref(), Some("https://api.openai.com/v1"));
            }
            other => panic!("expected provider add, got {other:?}"),
        }

        // Quickstart §2: catalogue template renamed with --id.
        let cli = parse(&[
            "mr",
            "provider",
            "add",
            "openai-compatible",
            "--id",
            "gateway",
            "--base-url",
            "https://gateway.internal/v1",
        ])
        .unwrap();
        match cli.command {
            Command::Provider(provider::ProviderCmd::Add(a)) => {
                assert_eq!(a.target, "openai-compatible");
                assert_eq!(a.id.as_deref(), Some("gateway"));
                assert_eq!(a.base_url.as_deref(), Some("https://gateway.internal/v1"));
            }
            other => panic!("expected provider add, got {other:?}"),
        }

        // Bare catalogue add.
        let cli = parse(&["mr", "provider", "add", "anthropic"]).unwrap();
        match cli.command {
            Command::Provider(provider::ProviderCmd::Add(a)) => {
                assert_eq!(a.target, "anthropic");
                assert!(a.dialect.is_none());
                assert!(a.base_url.is_none());
            }
            other => panic!("expected provider add, got {other:?}"),
        }
    }

    #[test]
    fn provider_list_show_remove_and_test_parse() {
        match parse(&["mr", "provider", "list"]).unwrap().command {
            Command::Provider(provider::ProviderCmd::List) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "provider", "show", "openai"])
            .unwrap()
            .command
        {
            Command::Provider(provider::ProviderCmd::Show { id }) => assert_eq!(id, "openai"),
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "provider", "remove", "openai", "--force"])
            .unwrap()
            .command
        {
            Command::Provider(provider::ProviderCmd::Remove { id, force }) => {
                assert_eq!(id, "openai");
                assert!(force);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "provider", "test", "openai", "--model", "gpt-5"])
            .unwrap()
            .command
        {
            Command::Provider(provider::ProviderCmd::Test { id, model }) => {
                assert_eq!(id, "openai");
                assert_eq!(model.as_deref(), Some("gpt-5"));
            }
            other => panic!("{other:?}"),
        }
        // `provider add` without an id is a usage error (exit 2), not a validation error.
        let err = parse(&["mr", "provider", "add"]).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    // ---- account ---------------------------------------------------------

    #[test]
    fn account_add_parses_secret_sources() {
        match parse(&[
            "mr", "account", "add", "openai", "--label", "personal", "--secret", "sk-x",
        ])
        .unwrap()
        .command
        {
            Command::Account(account::AccountCmd::Add(a)) => {
                assert_eq!(a.provider, "openai");
                assert_eq!(a.label.as_deref(), Some("personal"));
                assert_eq!(a.secret.as_deref(), Some("sk-x"));
                assert!(!a.secret_stdin);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "account", "add", "openai", "--secret-stdin"])
            .unwrap()
            .command
        {
            Command::Account(account::AccountCmd::Add(a)) => {
                assert!(a.secret_stdin);
                assert!(a.secret.is_none());
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "mr",
            "account",
            "add",
            "openai",
            "--secret-env",
            "OPENAI_KEY",
        ])
        .unwrap()
        .command
        {
            Command::Account(account::AccountCmd::Add(a)) => {
                assert_eq!(a.secret_env.as_deref(), Some("OPENAI_KEY"));
            }
            other => panic!("{other:?}"),
        }
        // docs/02 §3 writes `--key`; it is a visible alias of --secret.
        match parse(&["mr", "account", "add", "openai", "--key", "sk-y"])
            .unwrap()
            .command
        {
            Command::Account(account::AccountCmd::Add(a)) => {
                assert_eq!(a.secret.as_deref(), Some("sk-y"));
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "account", "list", "openai"]).unwrap().command {
            Command::Account(account::AccountCmd::List { provider }) => {
                assert_eq!(provider, "openai");
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "account", "remove", "openai", "acct-1", "--force"])
            .unwrap()
            .command
        {
            Command::Account(account::AccountCmd::Remove {
                provider,
                account,
                force,
            }) => {
                assert_eq!(provider, "openai");
                assert_eq!(account, "acct-1");
                assert!(force);
            }
            other => panic!("{other:?}"),
        }
    }

    // ---- combo -----------------------------------------------------------

    #[test]
    fn combo_add_parses_positional_and_flag_name() {
        // README quickstart line 90.
        let cli = parse(&[
            "mr",
            "combo",
            "add",
            "Brains",
            "--step",
            "openai/gpt-4o",
            "--step",
            "anthropic/claude-sonnet-4-5",
            "--strategy",
            "priority",
        ])
        .unwrap();
        match cli.command {
            Command::Combo(combo::ComboCmd::Add(a)) => {
                assert_eq!(a.resolved_name(), Some("Brains"));
                assert_eq!(a.step, vec!["openai/gpt-4o", "anthropic/claude-sonnet-4-5"]);
                assert_eq!(a.strategy.as_deref(), Some("priority"));
            }
            other => panic!("{other:?}"),
        }
        // docs/02 §5 `combo create --name …`.
        let cli = parse(&[
            "mr",
            "combo",
            "create",
            "--name",
            "Good",
            "--step",
            "openai/gpt-5.5",
        ])
        .unwrap();
        match cli.command {
            Command::Combo(combo::ComboCmd::Add(a)) => {
                assert_eq!(a.resolved_name(), Some("Good"));
                assert_eq!(a.step, vec!["openai/gpt-5.5"]);
            }
            other => panic!("{other:?}"),
        }
        // No name at all is a usage error.
        assert!(parse(&["mr", "combo", "add", "--step", "openai/gpt"]).is_err());
    }

    #[test]
    fn combo_step_remove_and_move_parse_with_indices() {
        match parse(&["mr", "combo", "step", "remove", "Brains", "1"])
            .unwrap()
            .command
        {
            Command::Combo(combo::ComboCmd::Step(combo::StepCmd::Remove { name, index })) => {
                assert_eq!(name, "Brains");
                assert_eq!(index, 1usize);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "combo", "step", "move", "Brains", "0", "2"])
            .unwrap()
            .command
        {
            Command::Combo(combo::ComboCmd::Step(combo::StepCmd::Move { name, from, to })) => {
                assert_eq!(name, "Brains");
                assert_eq!(from, 0usize);
                assert_eq!(to, 2usize);
            }
            other => panic!("{other:?}"),
        }
        // A non-numeric index is caught by clap as a usage error.
        assert!(parse(&["mr", "combo", "step", "move", "Brains", "x", "2"]).is_err());
    }

    // ---- alias -----------------------------------------------------------

    #[test]
    fn alias_commands_parse() {
        match parse(&["mr", "alias", "list"]).unwrap().command {
            Command::Alias(alias::AliasCmd::List) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "alias", "add", "gpt-5", "openai/gpt-5"])
            .unwrap()
            .command
        {
            Command::Alias(alias::AliasCmd::Add { from, to }) => {
                assert_eq!(from, "gpt-5");
                assert_eq!(to, "openai/gpt-5");
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "alias", "remove", "gpt-5"]).unwrap().command {
            Command::Alias(alias::AliasCmd::Remove { from }) => assert_eq!(from, "gpt-5"),
            other => panic!("{other:?}"),
        }
    }

    // ---- key / models / vault / config / serve / migrate -----------------

    #[test]
    fn key_create_parses_spec_and_doc_flags() {
        match parse(&["mr", "key", "create", "--label", "my-client", "--print"])
            .unwrap()
            .command
        {
            Command::Key(key::KeyCmd::Create(a)) => {
                assert_eq!(a.label.as_deref(), Some("my-client"));
                assert!(a.print);
            }
            other => panic!("{other:?}"),
        }
        // docs/02 §6 restriction flags.
        match parse(&[
            "mr",
            "key",
            "create",
            "--label",
            "ci",
            "--allow-models",
            "gpt-5*,claude-*",
            "--allow-routes",
            "chat.completions",
        ])
        .unwrap()
        .command
        {
            Command::Key(key::KeyCmd::Create(a)) => {
                assert_eq!(a.allow_models, vec!["gpt-5*,claude-*"]);
                assert_eq!(a.allow_routes, vec!["chat.completions"]);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "key", "list"]).unwrap().command {
            Command::Key(key::KeyCmd::List) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "key", "disable", "key-1"]).unwrap().command {
            Command::Key(key::KeyCmd::Disable { id }) => assert_eq!(id, "key-1"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn models_commands_parse() {
        match parse(&["mr", "models", "sync", "openai"]).unwrap().command {
            Command::Models(models::ModelsCmd::Sync { provider, all, .. }) => {
                assert_eq!(provider.as_deref(), Some("openai"));
                assert!(!all);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "models", "sync", "--all"]).unwrap().command {
            Command::Models(models::ModelsCmd::Sync { provider, all, .. }) => {
                assert!(provider.is_none());
                assert!(all);
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "models", "list", "openai"]).unwrap().command {
            Command::Models(models::ModelsCmd::List {
                provider, source, ..
            }) => {
                assert_eq!(provider.as_deref(), Some("openai"));
                assert!(source.is_none());
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "models", "list", "--source", "manual"])
            .unwrap()
            .command
        {
            Command::Models(models::ModelsCmd::List { source, .. }) => {
                assert_eq!(source.as_deref(), Some("manual"));
            }
            other => panic!("{other:?}"),
        }
        // `--provider` (docs/02 unverified variant) is accepted too.
        match parse(&["mr", "models", "list", "--provider", "openai"])
            .unwrap()
            .command
        {
            Command::Models(models::ModelsCmd::List {
                provider,
                provider_flag,
                ..
            }) => {
                assert_eq!(
                    models::ModelsCmd::list_provider(&provider, &provider_flag),
                    Some("openai")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn vault_and_config_commands_parse() {
        match parse(&["mr", "vault", "list"]).unwrap().command {
            Command::Vault(vault::VaultCmd::List) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "vault", "rekey"]).unwrap().command {
            Command::Vault(vault::VaultCmd::Rekey) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "config", "path"]).unwrap().command {
            Command::Config(config::ConfigCmd::Path) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "config", "validate"]).unwrap().command {
            Command::Config(config::ConfigCmd::Validate) => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "config", "set-port", "relay", "21000"])
            .unwrap()
            .command
        {
            Command::Config(config::ConfigCmd::SetPort { target, port }) => {
                assert_eq!(target, config::PortTarget::Relay);
                assert_eq!(port, 21000u16);
            }
            other => panic!("{other:?}"),
        }
        // Port 70000 does not fit u16: clap rejects it as usage, not validation.
        assert!(parse(&["mr", "config", "set-port", "relay", "70000"]).is_err());
    }

    #[test]
    fn serve_migrate_doctor_version_parse() {
        match parse(&["mr", "serve"]).unwrap().command {
            Command::Serve(_) => {}
            other => panic!("{other:?}"),
        }
        match parse(&[
            "mr",
            "serve",
            "--relay-port",
            "21000",
            "--control-port",
            "21001",
        ])
        .unwrap()
        .command
        {
            Command::Serve(serve::ServeCmd {
                relay_port,
                control_port,
                ..
            }) => {
                assert_eq!(relay_port, Some(21000));
                assert_eq!(control_port, Some(21001));
            }
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "migrate", "omniroute"]).unwrap().command {
            Command::Migrate(migrate::MigrateCmd { source, apply, db }) => {
                assert_eq!(source, migrate::Source::Omniroute);
                assert!(!apply);
                assert!(db.is_none());
            }
            other => panic!("{other:?}"),
        }
        match parse(&[
            "mr",
            "migrate",
            "omniroute",
            "--db",
            "/tmp/storage.sqlite",
            "--apply",
        ])
        .unwrap()
        .command
        {
            Command::Migrate(migrate::MigrateCmd { apply, db, .. }) => {
                assert!(apply);
                assert_eq!(db, Some(PathBuf::from("/tmp/storage.sqlite")));
            }
            other => panic!("{other:?}"),
        }
        // Unknown source or missing source are usage errors.
        assert!(parse(&["mr", "migrate"]).is_err());
        assert!(parse(&["mr", "migrate", "chrome"]).is_err());
        match parse(&["mr", "doctor"]).unwrap().command {
            Command::Doctor => {}
            other => panic!("{other:?}"),
        }
        match parse(&["mr", "version"]).unwrap().command {
            Command::Version => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn json_flag_is_accepted_on_every_read_command() {
        for args in [
            vec!["mr", "provider", "list", "--json"],
            vec!["mr", "provider", "show", "openai", "--json"],
            vec!["mr", "account", "list", "openai", "--json"],
            vec!["mr", "combo", "list", "--json"],
            vec!["mr", "combo", "show", "Brains", "--json"],
            vec!["mr", "alias", "list", "--json"],
            vec!["mr", "key", "list", "--json"],
            vec!["mr", "models", "list", "openai", "--json"],
            vec!["mr", "config", "path", "--json"],
            vec!["mr", "config", "validate", "--json"],
            vec!["mr", "vault", "list", "--json"],
            vec!["mr", "doctor", "--json"],
            vec!["mr", "version", "--json"],
        ] {
            let cli = parse(&args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert!(cli.json, "{args:?}");
        }
    }

    #[test]
    fn version_json_is_serialisable() {
        let ctx = Ctx { json: true };
        version(&ctx).unwrap();
    }

    #[test]
    fn partition_keeps_accountless_provider_as_an_advisory() {
        let (bad, advisory) = partition_problems(vec![
            "provider openai has no accounts, so it can never serve a request".to_string(),
            "combo bad references unknown provider nope".to_string(),
        ]);
        assert_eq!(advisory.len(), 1);
        assert_eq!(bad.len(), 1);
        assert!(bad[0].contains("unknown provider"));
    }

    #[test]
    fn clap_usage_errors_have_exit_code_two() {
        // clap exits 2 natively for parse failures; assert the contract we document.
        let err = parse(&["mr", "provider", "add"]).unwrap_err();
        assert_eq!(err.exit_code(), Exit::Usage.code() as i32);
        let err = parse(&["mr", "combo", "step", "move", "x", "nope", "1"]).unwrap_err();
        assert_eq!(err.exit_code(), Exit::Usage.code() as i32);
    }
}
