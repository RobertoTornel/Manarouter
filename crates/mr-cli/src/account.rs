//! `mr account` — credentials attached to a provider.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// Attach a credential to a provider (stored in the vault; only its id reaches config.json).
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
        /// Account id (from `account list`).
        account: String,
        /// Required when the account still exists in the vault or is referenced elsewhere.
        #[arg(long)]
        force: bool,
    },
    /// Re-enable a disabled account.
    Enable {
        /// Provider id.
        provider: String,
        /// Account id.
        account: String,
    },
    /// Disable an account without deleting its credential.
    Disable {
        /// Provider id.
        provider: String,
        /// Account id.
        account: String,
    },
    /// Set an account's selection priority (higher wins first).
    SetPriority {
        /// Provider id.
        provider: String,
        /// Account id.
        account: String,
        /// New priority.
        priority: i32,
    },
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Provider the credential belongs to.
    pub provider: String,
    /// Human label, for example `personal`.
    #[arg(long)]
    pub label: Option<String>,
    /// The credential itself. Prefer --secret-stdin or --secret-env: this form is visible in the
    /// shell history and to other processes.
    #[arg(long, visible_alias = "key")]
    pub secret: Option<String>,
    /// Read the credential from stdin (this is the form to use in scripts and pipes).
    #[arg(long)]
    pub secret_stdin: bool,
    /// Read the credential from this environment variable.
    #[arg(long, value_name = "VAR")]
    pub secret_env: Option<String>,
    /// Selection priority (higher wins first).
    #[arg(long, value_name = "N")]
    pub priority: Option<i32>,
    /// Relative weight for round-robin selection.
    #[arg(long, value_name = "N")]
    pub weight: Option<u32>,
    /// Stable identity (email, account name) for display and deduplication.
    #[arg(long, value_name = "TEXT")]
    pub identity: Option<String>,
}

/// Run an account subcommand.
pub async fn run(cmd: AccountCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr account` is not implemented yet"))
}
