//! The `manarouter-relay` binary.
//!
//! Kept deliberately thin. Everything it does lives in the library so that the integration tests
//! and the latency benchmark exercise the same code the shipped binary runs — a startup routine
//! that only exists in `main.rs` is a startup routine that is never tested.
//!
//! The global allocator is declared in `lib.rs` for the same reason.

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "manarouter_relay=info".into()),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "manarouter-relay starting"
    );

    // The listener, snapshot loading and the request loop are built in the next stage; see
    // `docs/01-architecture.md` and the module map in `lib.rs`.
    tracing::warn!("data plane not yet wired up: no listener is bound");

    Ok(())
}
