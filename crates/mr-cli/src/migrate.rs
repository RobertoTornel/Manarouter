//! `mr migrate` — import a previous installation.
//!
//! The rules the migration must keep, from docs/17 and plan §13: it reads a *copy* of the
//! reference database and never the original, and it does nothing until `--apply` is given.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use std::path::PathBuf;

use crate::output::CliError;
use crate::Ctx;

#[derive(clap::Args, Debug)]
pub struct MigrateCmd {
    /// Which system to import from.
    #[arg(value_enum)]
    pub source: Source,
    /// Perform the import. Without it the command only reports what it would do.
    #[arg(long)]
    pub apply: bool,
    /// Path to the reference database (default: ~/.omniroute/storage.sqlite).
    #[arg(long, value_name = "PATH")]
    pub db: Option<PathBuf>,
}

/// Supported sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Source {
    /// Import an OmniRoute installation.
    Omniroute,
}

/// Run a migrate subcommand.
pub async fn run(cmd: MigrateCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr migrate` is not implemented yet"))
}
