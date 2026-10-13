//! `mr models` — a provider's model catalogue.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum ModelsCmd {
    /// List models (`models list` alone lists every provider's, as qualified ids).
    List {
        /// Provider id (positional).
        provider: Option<String>,
        /// Provider id (`--provider`, the same value as the positional).
        #[arg(long = "provider")]
        provider_flag: Option<String>,
        /// Only entries with this source: synced, manual, inferred.
        #[arg(long, value_name = "SOURCE")]
        source: Option<String>,
    },
    /// Fetch the provider's model list over the network and store it.
    Sync {
        /// Provider id (positional).
        provider: Option<String>,
        /// Sync every configured provider.
        #[arg(long)]
        all: bool,
    },
}

impl ModelsCmd {
    /// The provider a `List` invocation should operate on, from either spelling.
    pub fn list_provider<'a>(
        provider: &'a Option<String>,
        provider_flag: &'a Option<String>,
    ) -> Option<&'a str> {
        provider.as_deref().or(provider_flag.as_deref())
    }
}

/// Run a models subcommand.
pub async fn run(cmd: ModelsCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr models` is not implemented yet"))
}
