//! # manarouter-conductor — the control plane
//!
//! Owns the config, the panel, combo authoring, OAuth and quota, and compiles the routing
//! snapshot the relay reads. It is never on the request path.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "manarouter_conductor=info".into()),
        )
        .init();
    tracing::info!(
        "manarouter-conductor {} (skeleton)",
        env!("CARGO_PKG_VERSION")
    );
    Ok(())
}
