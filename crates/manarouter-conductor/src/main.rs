//! # manarouter-conductor — the control plane
//!
//! Owns the config, the panel, combo authoring, OAuth and quota, and compiles the routing
//! snapshot the relay reads. It is never on the request path.
//!
//! Startup order matters and is mirrored by `state::AppState::new`:
//! load the configuration → resolve panel authentication (refusing a remotely-bound panel with
//! no password) → open the vault and compile the initial snapshot → bind and serve → wait for
//! a restart request, then exit cleanly so a supervisor can bring us back.

use manarouter_conductor::auth::PanelAuth;
use manarouter_conductor::serve;
use manarouter_conductor::state::{AppState, ServeDirs};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "manarouter_conductor=info".into()),
        )
        .init();
    tracing::info!("manarouter-conductor {}", env!("CARGO_PKG_VERSION"));

    let config = mr_core::config::Config::load_default().map_err(|e| {
        anyhow::anyhow!(
            "could not load the configuration: {e}. Fix the file it names, or delete it to \
             start from defaults."
        )
    })?;

    // Authentication is resolved *before* anything is opened or bound: a panel that would be
    // exposed without a password must fail here, with the reason, rather than after the socket
    // is already listening.
    let auth = PanelAuth::resolve(
        &config.control.host,
        config.control.password_hash.clone(),
        config.control.allow_remote_without_password,
    )
    .map_err(|e| {
        anyhow::anyhow!(
            "refusing to start the control plane: {e} — control.host is {} (a non-loopback \
             address) and control.password_hash is not set. Set a panel password, or bind \
             control.host to 127.0.0.1.",
            config.control.host
        )
    })?;

    // Free functions, not methods: the paths derive from $XDG_CONFIG_HOME, not from the loaded
    // document, so they are read the same way `Config::load_default` reads them.
    let config_path = mr_core::config::config_path();
    let snapshot_path = mr_core::config::snapshot_path();
    let running_relay_port = config.relay.port;

    let state = AppState::new(
        config,
        config_path,
        snapshot_path,
        auth,
        running_relay_port,
        0, // the control port is stored once the listener exists (serve.rs)
        ServeDirs::discover(),
    )
    .map_err(|e| anyhow::anyhow!("the control plane could not start: {e}"))?;

    serve::serve(state.clone())
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // Only reachable through the restart endpoint's graceful shutdown: exit cleanly so a
    // supervisor (systemd `Restart=`, docker `--restart`, an init script) starts us again.
    tracing::info!("restart requested; exiting cleanly for the supervisor to restart the process");
    Ok(())
}
