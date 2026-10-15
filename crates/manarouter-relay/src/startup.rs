//! Startup for the relay, callable from more than one binary.
//!
//! The `manarouter-relay` binary and `mr serve` need the same startup sequence — load and validate the
//! configuration, resolve the bind address, refuse a keyless non-loopback bind, open the vault, build
//! the `AppState`, then serve. Keeping it here rather than in the binary is what lets `mr serve`
//! compose the relay without re-implementing any of it, and it means the integration tests and the
//! latency benchmark exercise the same code the shipped binary runs.
//!
//! The functions return errors instead of calling `std::process::exit`. A process that exits inside a
//! helper cannot be composed: `mr serve` has two services in one process, and the relay's decision to
//! terminate has to become the whole process's decision at one place, not a `exit(2)` from a library
//! function.

use std::net::SocketAddr;
use std::sync::Arc;

use crate::select::Health;
use crate::server::{self, AppState};
use crate::state::RelayState;
use mr_core::config::Config;
use mr_core::secret::Vault;

/// What the relay needs before it can serve.
///
/// `host`/`port` are `Option` rather than defaulted so "not passed" stays distinguishable from
/// "passed the default": a value with a default cannot tell those apart and would silently override a
/// configuration file that says something else.
#[derive(Debug, Default, Clone)]
pub struct RelayOptions {
    /// Interface to bind, overriding the configuration file.
    pub host: Option<String>,
    /// TCP port, overriding the configuration file.
    pub port: Option<u16>,
}

/// Why a relay could not start.
///
/// A plain message rather than an error type: every one of these is terminal, printed for a human, and
/// nothing downstream branches on which it was.
#[derive(Debug)]
pub struct StartupError(pub String);

impl std::fmt::Display for StartupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StartupError {}

impl StartupError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// Resolve the bind address from the options, then the configuration, then the default.
///
/// Precedence is the flag, then the configuration file, then the struct default. The flag has to win
/// or the option is a lie; the file has to beat the default or the setting is a decoration.
pub fn resolve_addr(options: &RelayOptions, config: &Config) -> Result<SocketAddr, StartupError> {
    let host = options
        .host
        .clone()
        .unwrap_or_else(|| config.relay.host.clone());
    let port = options.port.unwrap_or(config.relay.port);

    // An IPv6 literal contains colons, so `{host}:{port}` is ambiguous and does not parse:
    // `::1:20900` is read as a malformed address rather than as port 20900 on `::1`. Bracketing is
    // how a socket address disambiguates, and an operator typing `--host ::1` should not have to know
    // that — the error message would send them looking for a networking problem instead of a syntax
    // one.
    let rendered = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };

    rendered.parse().map_err(|e| {
        StartupError::new(format!(
            "could not interpret \"{host}\" and port {port} as an address: {e}"
        ))
    })
}

/// Whether a keyless relay may bind this address.
///
/// Everything that decides to terminate the process is untestable from outside, and this is exactly
/// the kind of safety rule that should have a test rather than a comment.
pub fn keyless_bind_is_allowed(addr: SocketAddr, keys_configured: bool) -> bool {
    keys_configured || addr.ip().is_loopback()
}

/// Load and validate the configuration, reporting every problem rather than the first.
///
/// A missing file is the normal first run and `Config::load_default` already treats it as such;
/// anything else is a real problem for the operator to fix, and it is returned rather than panicked
/// because release builds set `panic = "abort"`.
pub fn load_config() -> Result<Config, StartupError> {
    let config = Config::load_default().map_err(|e| {
        StartupError::new(format!(
            "could not read the configuration: {e}\nlooked in {}",
            mr_core::config::config_path().display()
        ))
    })?;

    let problems = config.validate();
    if !problems.is_empty() {
        let mut message = String::from("the configuration is not valid:");
        for problem in &problems {
            message.push_str("\n  - ");
            message.push_str(problem);
        }
        return Err(StartupError::new(message));
    }
    Ok(config)
}

/// Build the relay's state, checking the configuration and the keyless-bind rule on the way.
///
/// Split from [`serve`] so `mr serve` can build the state, start listening, and only then report
/// success — a caller that has to await the whole serve loop cannot tell "listening" from "started and
/// already gone".
pub fn build_state(
    options: &RelayOptions,
    config: &Config,
) -> Result<(AppState, SocketAddr), StartupError> {
    let addr = resolve_addr(options, config)?;

    let relay = Arc::new(
        RelayState::load()
            .map_err(|e| StartupError::new(format!("could not load the routing table: {e}")))?,
    );

    // With no client keys configured the relay serves **everyone**: `authenticate` answers `Open`,
    // because a fresh install that rejected all requests would be unusable and the operator would
    // have no way to reach the panel and create a key.
    //
    // That is a reasonable trade on loopback and an unacceptable one anywhere else, so the two
    // conditions are checked together. Binding a keyless relay to a reachable interface would hand
    // this machine's provider credentials to anyone who can route to it, and no warning in a log is a
    // sufficient answer to that.
    if !relay.keys_are_configured() {
        if !keyless_bind_is_allowed(addr, false) {
            return Err(StartupError::new(format!(
                "refusing to bind {addr} with no client keys configured.\n\
                 a relay with no keys serves every request to anyone who can reach it,\n\
                 and it holds this machine's provider credentials.\n\
                 create a key with `mr key create`, or bind 127.0.0.1."
            )));
        }
        tracing::warn!(
            bind = %addr,
            "no client keys are configured, so every request is accepted on this loopback \
             address. Create one with `mr key create`."
        );
    }
    tracing::info!(
        keys = relay.key_count(),
        combos = relay.table().snapshot.combos().len(),
        providers = relay.table().snapshot.providers().len(),
        "routing table loaded"
    );

    let vault = Arc::new(
        Vault::open_default()
            .map_err(|e| StartupError::new(format!("could not open the credential vault: {e}")))?,
    );

    let refresh_vault =
        Vault::open(mr_core::config::config_dir()).expect("vault was just opened successfully");
    let refresh_store = Arc::new(mr_core::oauth::refresh::VaultStore::new(refresh_vault));
    let oauth_refreshers = mr_core::oauth::platforms()
        .iter()
        .map(|platform| {
            (
                platform.id().to_owned(),
                Arc::new(mr_core::oauth::refresh::Refresher::new(
                    *platform,
                    refresh_store.clone(),
                )),
            )
        })
        .collect();
    let oauth_store = Arc::new(
        mr_core::store::Store::open(&mr_core::config::database_path())
            .map_err(|e| StartupError::new(format!("could not open OAuth metadata store: {e}")))?,
    );

    // The reloader needs its own handle, taken before `relay` is moved into the state.
    let reload_handle = Arc::clone(&relay);

    let state = AppState {
        relay,
        health: Arc::new(Health::new()),
        vault,
        oauth_refreshers: Arc::new(oauth_refreshers),
        oauth_store,
        sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
    };

    // Adopt routing changes written by the control plane, without a restart.
    //
    // The control plane compiles the snapshot and writes `config.json`, but the relay holds its own
    // copy in memory from startup. Until this existed, every import or model addition required
    // stopping and starting the relay — and the failure was quiet: `/v1/models` listed the old set and
    // a request for a new model was a 404 that looked like a typo.
    //
    // A polling task rather than an inotify watcher: the file is small, this is off the request path,
    // and a poll cannot miss an event that an editor's rename dance would confuse. One `stat` per
    // interval is cheaper than the machinery that would replace it.
    spawn_config_reloader(reload_handle);

    Ok((state, addr))
}

/// Build the state and serve until the process is asked to stop.
pub async fn serve(options: RelayOptions) -> Result<(), StartupError> {
    let config = load_config()?;
    let (state, addr) = build_state(&options, &config)?;
    server::serve(state, addr)
        .await
        .map_err(|e| StartupError::new(format!("the listener failed: {e}")))
}

/// Re-read the configuration periodically and install a freshly compiled snapshot when it changed.
///
/// Failures are logged and the loop continues: a configuration that momentarily will not parse (a
/// half-written file, a hand-edit in progress) must not take down a relay that is serving traffic. The
/// previously installed snapshot keeps working, which is the whole point of keeping routing data in an
/// atomically swappable handle.
fn spawn_config_reloader(relay: Arc<RelayState>) {
    tokio::spawn(async move {
        let path = mr_core::config::config_path();
        let mut last_seen = config_fingerprint(&path);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(2));
        // The first tick fires immediately; the initial state was just loaded, so skip it.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let now = config_fingerprint(&path);
            if now == last_seen {
                continue;
            }
            // Record before attempting the reload: a file that will not compile must not be retried on
            // every tick forever, and once it *does* compile the mtime changes again.
            last_seen = now;
            match relay.reload() {
                Ok(digest) => tracing::info!(
                    digest,
                    providers = relay.table().snapshot.providers().len(),
                    "routing table reloaded from an external configuration change"
                ),
                Err(e) => tracing::warn!(
                    error = %e,
                    "a configuration change was detected but could not be applied; keeping the \
                     current routing table"
                ),
            }
        }
    });
}

/// `(mtime, length)` of a file, or `None` when it cannot be read.
///
/// Length is included because filesystem timestamps can be coarse here: an edit landing in the same
/// timestamp tick as the previous read would otherwise be invisible. A path is taken rather than read
/// from the global config location so the rule can be tested against a temp file.
fn config_fingerprint(path: &std::path::Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address-resolution tests only care about host and port; the other flags are not part of
    /// `RelayOptions`, which is what lets this module be tested without the binary's clap surface.
    fn args(host: Option<&str>, port: Option<u16>) -> RelayOptions {
        RelayOptions {
            host: host.map(str::to_owned),
            port,
        }
    }

    fn config(host: &str, port: u16) -> Config {
        let mut c = Config::default();
        c.relay.host = host.to_owned();
        c.relay.port = port;
        c
    }

    #[test]
    fn with_no_flags_the_configuration_file_decides() {
        // Otherwise a configuration that says where to listen would be ignored, which makes the file
        // a decoration.
        let a = args(None, None);
        let c = config("0.0.0.0", 12345);
        assert_eq!(
            resolve_addr(&a, &c).unwrap(),
            "0.0.0.0:12345".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn a_port_flag_beats_the_configuration_file() {
        // Someone who typed `--port` means it.
        let a = args(None, Some(9999));
        let c = config("127.0.0.1", 20900);
        assert_eq!(
            resolve_addr(&a, &c).unwrap(),
            "127.0.0.1:9999".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn a_host_flag_beats_the_configuration_file() {
        let a = args(Some("10.0.0.7"), None);
        let c = config("127.0.0.1", 20900);
        assert_eq!(
            resolve_addr(&a, &c).unwrap(),
            "10.0.0.7:20900".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn both_flags_together_beat_the_configuration_file() {
        let a = args(Some("10.0.0.7"), Some(9999));
        let c = config("127.0.0.1", 20900);
        assert_eq!(
            resolve_addr(&a, &c).unwrap(),
            "10.0.0.7:9999".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn the_default_configuration_binds_loopback() {
        // The process holds credentials. A default that bound every interface would expose them on
        // any network the machine happens to be on, which is not a mistake worth allowing by
        // accident.
        let a = args(None, None);
        let c = Config::default();
        let addr = resolve_addr(&a, &c).unwrap();
        assert!(
            addr.ip().is_loopback(),
            "the default must not be a public bind"
        );
        assert_eq!(addr.port(), 20900);
    }

    #[test]
    fn a_bindable_default_is_chosen_over_a_free_port() {
        // Not a port-availability test: a documented port is worth more than a random one, because a
        // client has to be told where to connect. This pins the documented value.
        assert_eq!(mr_core::DEFAULT_RELAY_PORT, 20900);
    }

    #[test]
    fn a_bare_ipv6_literal_is_bracketed_for_the_operator() {
        // `--host ::1` is what somebody types. Without bracketing it does not parse, and the error
        // sends them looking for a networking problem instead of a syntax one.
        let a = args(Some("::1"), Some(20900));
        let c = Config::default();
        let addr = resolve_addr(&a, &c).unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 20900);
    }

    #[test]
    fn an_already_bracketed_ipv6_literal_is_not_bracketed_twice() {
        let a = args(Some("[::1]"), Some(20900));
        let c = Config::default();
        assert_eq!(resolve_addr(&a, &c).unwrap().port(), 20900);
    }

    #[test]
    fn the_ipv6_any_address_is_recognised_as_reachable() {
        // `::` is IPv6's `0.0.0.0` and must be treated as the exposure it is.
        let a = args(Some("::"), Some(20900));
        let c = Config::default();
        let addr = resolve_addr(&a, &c).unwrap();
        assert!(!addr.ip().is_loopback());
        assert!(!keyless_bind_is_allowed(addr, false));
    }

    #[test]
    fn a_hostname_that_does_not_resolve_is_a_clean_error() {
        // It must not panic: release builds set `panic = "abort"`, so a panic here is a process that
        // dies without telling anyone why.
        let a = args(Some("this is not an address"), Some(1));
        let c = Config::default();
        let err = resolve_addr(&a, &c).unwrap_err();
        assert!(
            err.to_string().contains("could not interpret"),
            "got: {err}"
        );
    }

    #[test]
    fn an_unparseable_log_filter_falls_back_rather_than_dying() {
        // A typo in a log filter must not be the reason a router will not start.
        let filter = tracing_subscriber::EnvFilter::try_new("[[[not a filter");
        assert!(filter.is_err());
        let fallback = tracing_subscriber::EnvFilter::new("manarouter_relay=info");
        assert!(!fallback.to_string().is_empty());
    }

    #[test]
    fn a_keyless_relay_may_bind_loopback() {
        // The bring-up order has to stay possible: start the relay, reach the panel, create a key.
        // IPv6 literals need brackets in a socket address, which is the form the flag has to accept
        // too — `--host ::1` is what an operator will type, and `update` below is not the only place
        // that has to cope with it.
        for host in ["127.0.0.1", "[::1]"] {
            let addr: SocketAddr = format!("{host}:20900").parse().unwrap();
            assert!(
                keyless_bind_is_allowed(addr, false),
                "{host} must be allowed without keys"
            );
        }
    }

    #[test]
    fn a_keyless_relay_may_not_bind_a_reachable_address() {
        // It holds this machine's provider credentials, and with no keys it serves every request to
        // anyone who can route to it. No log line is a sufficient answer to that.
        for host in ["0.0.0.0", "10.0.0.7", "192.168.1.10", "[::]"] {
            let addr: SocketAddr = format!("{host}:20900").parse().unwrap();
            assert!(
                !keyless_bind_is_allowed(addr, false),
                "{host} must be refused without keys"
            );
        }
    }

    #[test]
    fn a_relay_with_keys_may_bind_anywhere() {
        // Once a key exists the operator has made an explicit choice, and binding a reachable
        // interface is how the router is used from another machine.
        let addr: SocketAddr = "0.0.0.0:20900".parse().unwrap();
        assert!(keyless_bind_is_allowed(addr, true));
    }

    // ----- configuration fingerprint ----------------------------------------

    #[test]
    fn a_missing_configuration_has_no_fingerprint() {
        // `None` rather than an error: on a first run the file does not exist yet, and the reloader
        // must treat that as "nothing changed" instead of retrying forever.
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(config_fingerprint(&dir.path().join("nope.json")), None);
    }

    #[test]
    fn a_growing_file_changes_its_fingerprint() {
        // The length component is what makes this work: an edit that lands within the same
        // filesystem timestamp tick as the previous read would otherwise look unchanged.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{}").expect("write");
        let before = config_fingerprint(&path).expect("fingerprint");
        std::fs::write(&path, b"{\"providers\":[]}").expect("write");
        let after = config_fingerprint(&path).expect("fingerprint");
        assert_ne!(
            before, after,
            "a length change must be visible even when the timestamp tick is the same"
        );
    }

    #[test]
    fn an_unchanged_file_keeps_its_fingerprint() {
        // The common case, and the one the reloader's cost depends on: an untouched configuration
        // must compare equal so no parse happens at all.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("config.json");
        std::fs::write(&path, b"{\"a\":1}").expect("write");
        assert_eq!(config_fingerprint(&path), config_fingerprint(&path));
    }
}
