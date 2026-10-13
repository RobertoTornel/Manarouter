//! `mr vault` — the encrypted secret store alongside config.json.
//!
//! The vault holds every provider secret; config.json holds only `secret_ref` ids. Reading the
//! vault shows fingerprints (`mask`), never values.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum VaultCmd {
    /// List stored secret ids with fingerprints. Values are never printed.
    List,
    /// Decrypt everything, re-encrypt with a new random key, and rotate vault.key.
    Rekey,
}

/// Run a vault subcommand.
pub async fn run(cmd: VaultCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr vault` is not implemented yet"))
}
