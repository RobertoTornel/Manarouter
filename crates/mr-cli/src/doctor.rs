//! `mr doctor` — check the installation end to end.
//!
//! Checks: configuration loads and validates, every combo step resolves to a real provider and a
//! usable account, the vault opens, the routing snapshot matches the configuration digest, and the
//! configured ports are either free or already spoken for. Exit 1 on any FAIL, 0 when only
//! warnings.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use crate::output::CliError;
use crate::Ctx;

/// Run `mr doctor`.
pub async fn run(ctx: &Ctx) -> Result<(), CliError> {
    let _ = ctx;
    Err(CliError::failure("`mr doctor` is not implemented yet"))
}
