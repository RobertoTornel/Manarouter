//! `mr serve` — run the relay and the conductor together.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use crate::output::CliError;
use crate::Ctx;

/// `mr serve` is a leaf command: it carries its own argument struct, which `main`'s `Command`
/// holds as a payload (a type implementing `clap::Args`).
#[derive(clap::Args, Debug)]
pub struct ServeCmd {
    /// Relay (data plane) port; defaults to config.json's `relay.port` (20900).
    #[arg(long, value_name = "PORT")]
    pub relay_port: Option<u16>,
    /// Conductor + panel port; defaults to config.json's `control.port` (20901).
    #[arg(long = "control-port", value_name = "PORT")]
    pub control_port: Option<u16>,
}

/// Run `mr serve`.
pub async fn run(cmd: ServeCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr serve` is not implemented yet"))
}
