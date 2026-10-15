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
use manarouter_conductor::state::{AppState, ServeDirs, StateFiles};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "manarouter_conductor=info".into()),
        )
        .init();
    tracing::info!("manarouter-conductor {}", env!("CARGO_PKG_VERSION"));

    let mut config = mr_core::config::Config::load_default().map_err(|e| {
        anyhow::anyhow!(
            "could not load the configuration: {e}. Fix the file it names, or delete it to \
             start from defaults."
        )
    })?;

    // A one-run override of the panel port, used by `mr serve --control-port`.
    //
    // This binary parses no command-line arguments, so an environment variable is the channel a
    // parent process can use without a flag surface here that could drift from the panel's own
    // Settings view. It is read before authentication is resolved and before anything binds, so a
    // port change cannot arrive after the listener exists.
    //
    // An unparseable value is an error rather than a silent fallback: silently binding the configured
    // port instead would make `mr serve --control-port 3000` appear to work while the panel listened
    // somewhere else.
    if let Some(raw) = std::env::var_os("MANAROUTER_CONTROL_PORT") {
        let text = raw.to_string_lossy().to_string();
        let port: u16 = text.parse().map_err(|_| {
            anyhow::anyhow!(
                "MANAROUTER_CONTROL_PORT is not a valid port: {text:?} (expected 0-65535)"
            )
        })?;
        tracing::info!(
            configured = config.control.port,
            override_port = port,
            "MANAROUTER_CONTROL_PORT overrides control.port for this run"
        );
        config.control.port = port;
    }

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
        StateFiles {
            config_path,
            snapshot_path,
            database_path: mr_core::config::database_path(),
            dirs: ServeDirs::discover(),
        },
        auth,
        running_relay_port,
        0, // the control port is stored once the listener exists (serve.rs)
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
