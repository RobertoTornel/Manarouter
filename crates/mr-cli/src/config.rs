//! `mr config` — inspect, validate and adjust config.json.
//!
//! Every mutation here goes through `Ctx::save`: validate → compile snapshot → write. An invalid
//! change is refused with exit 3 and touches nothing on disk.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
    /// Print the configuration file path.
    Path,
    /// Check the configuration and report every problem (exit 3 if any).
    Validate,
    /// Print the configuration (no secrets live in it; nothing is redacted away).
    Show,
    /// Open config.json in $VISUAL/$EDITOR.
    Edit,
    /// Change a listener port: `mr config set-port relay 20900`.
    SetPort {
        /// Which listener.
        target: PortTarget,
        /// New TCP port.
        port: u16,
    },
}

/// Which listener `set-port` changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PortTarget {
    /// The relay (data plane): `relay` in config.json.
    #[value(alias = "data")]
    Relay,
    /// The conductor + panel (control plane): `control` in config.json.
    #[value(alias = "conductor", alias = "panel")]
    Control,
}

/// Run a config subcommand.
pub async fn run(cmd: ConfigCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr config` is not implemented yet"))
}
