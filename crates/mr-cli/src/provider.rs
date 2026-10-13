//! `mr provider` — add, inspect, test, enable and remove upstream providers.
//!
//! Stub: the clap surface is final, the bodies are filled in one at a time.

use clap::Subcommand;

use crate::output::CliError;
use crate::Ctx;

#[derive(Subcommand, Debug)]
pub enum ProviderCmd {
    /// Add a provider: a catalogue id, or a dialect + base URL for a custom endpoint.
    Add(AddArgs),
    /// List configured providers.
    List,
    /// Show one provider in full.
    Show {
        /// Provider id.
        id: String,
    },
    /// Remove a provider. Refuses while credentials or combos still reference it, unless --force.
    Remove {
        /// Provider id.
        id: String,
        /// Also drop the provider's vault entries and combo steps.
        #[arg(long)]
        force: bool,
    },
    /// Re-enable a disabled provider.
    Enable {
        /// Provider id.
        id: String,
    },
    /// Disable a provider without deleting it.
    Disable {
        /// Provider id.
        id: String,
    },
    /// Make one real, minimal authenticated request and report the upstream's actual response.
    Test {
        /// Provider id.
        id: String,
        /// Model to exercise (default: the provider's first model, or a catalogue probe).
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct AddArgs {
    /// Catalogue id (`openai`), a dialect spelling (`openai-compatible`), or the new provider id
    /// when --dialect and --base-url describe a custom endpoint.
    pub target: String,
    /// Name for the new provider (defaults to the catalogue id, or to `target`).
    #[arg(long)]
    pub id: Option<String>,
    /// Wire dialect for a custom endpoint (`open-ai`, `anthropic`, `google`, ...).
    #[arg(long, value_name = "DIALECT")]
    pub dialect: Option<String>,
    /// Base URL of a custom endpoint.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,
    /// Human label shown in tables and the panel.
    #[arg(long)]
    pub label: Option<String>,
    /// Seed from this catalogue template instead of the positional.
    #[arg(long = "from-catalog", value_name = "ID")]
    pub from_catalog: Option<String>,
    /// How the credential is attached: bearer, api-key, header:NAME[:PREFIX], query[:PARAM],
    /// oauth:PLATFORM.
    #[arg(long, value_name = "KIND")]
    pub auth: Option<String>,
    /// Add a capability (`--capability chat`, `--capability audio-transcription`, ...).
    #[arg(long = "capability", value_name = "CAPABILITY")]
    pub capability: Vec<String>,
    /// Add a static header (`--header "anthropic-version: 2023-06-01"`).
    #[arg(long = "header", value_name = "NAME: VALUE")]
    pub header: Vec<String>,
    /// Account selection strategy for this provider.
    #[arg(long = "key-strategy", value_name = "STRATEGY")]
    pub key_strategy: Option<String>,
    /// TCP connect timeout in milliseconds.
    #[arg(long = "connect-timeout-ms", value_name = "MS")]
    pub connect_timeout_ms: Option<u64>,
    /// Time-to-first-byte timeout in milliseconds.
    #[arg(long = "ttfb-timeout-ms", value_name = "MS")]
    pub ttfb_timeout_ms: Option<u64>,
    /// Idle-connection timeout in milliseconds.
    #[arg(long = "idle-timeout-ms", value_name = "MS")]
    pub idle_timeout_ms: Option<u64>,
    /// Whole-request timeout in milliseconds (0 = unlimited).
    #[arg(long = "total-timeout-ms", value_name = "MS")]
    pub total_timeout_ms: Option<u64>,
}

/// Run a provider subcommand.
pub async fn run(cmd: ProviderCmd, ctx: &Ctx) -> Result<(), CliError> {
    let _ = (&cmd, ctx);
    Err(CliError::failure("`mr provider` is not implemented yet"))
}
