//! `mr key` — client keys the relay accepts from callers.
//!
//! The plaintext of a key exists for exactly one moment: the output of `key create`. It is hashed
//! (SHA-256) into config.json and never stored or printed again — `key list` shows id, label,
//! prefix and permissions only.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum KeyCmd {
    /// Create a client key. The plaintext is printed once (bare on stdout with --print).
    Create(CreateArgs),
    /// List keys: id, label, prefix, permissions. Never a key or its hash.
    List,
    /// Re-enable a disabled key.
    Enable {
        /// Key id.
        id: String,
    },
    /// Disable a key (rejected at authentication, before routing).
    Disable {
        /// Key id.
        id: String,
    },
    /// Delete a key from the configuration.
    Remove {
        /// Key id.
        id: String,
    },
}

#[derive(clap::Args, Debug)]
pub struct CreateArgs {
    /// Human label, for example `my-editor`.
    #[arg(long)]
    pub label: Option<String>,
    /// Print only the bare key on stdout (for `$(...)` capture); the confirmation text moves to
    /// stderr. Without it the key is printed once inside a human-readable block.
    #[arg(long)]
    pub print: bool,
    /// Restrict to these model ids/patterns (comma-separated or repeated).
    #[arg(long = "allow-models", value_name = "PATTERNS")]
    pub allow_models: Vec<String>,
    /// Restrict to these routes (`chat.completions`, ...).
    #[arg(long = "allow-routes", value_name = "ROUTES")]
    pub allow_routes: Vec<String>,
    /// Per-key request budget (0 or absent = unlimited).
    #[arg(long = "limit-requests", value_name = "N")]
    pub limit_requests: Option<u64>,
    /// Descriptive note.
    #[arg(long)]
    pub note: Option<String>,
}

/// Run a key subcommand.
pub async fn run(cmd: KeyCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr key` is not implemented yet"))
}
