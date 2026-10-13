//! `mr alias` — alternative spellings that resolve to a real model or combo.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum AliasCmd {
    /// List aliases: the spelling and where it points.
    List,
    /// Point a spelling at a model id, qualified model id, or combo name.
    Add {
        /// What a client sends (`gpt-5`).
        from: String,
        /// What it resolves to (`openai/gpt-5`, or a combo name).
        to: String,
    },
    /// Remove an alias.
    Remove {
        /// The spelling to remove.
        from: String,
    },
}

/// Run an alias subcommand.
pub async fn run(cmd: AliasCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr alias` is not implemented yet"))
}
