//! # mr — the Manarouter CLI

use clap::Parser;

#[derive(Parser)]
#[command(name = "mr", version, about = "Manarouter control CLI")]
struct Cli {
    /// Path to the Manarouter config directory (defaults to ~/.config/manarouter)
    #[arg(long, global = true)]
    config_dir: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mr=info".into()),
        )
        .init();
    let _cli = Cli::parse();
    tracing::info!("mr {} (skeleton)", env!("CARGO_PKG_VERSION"));
    Ok(())
}
