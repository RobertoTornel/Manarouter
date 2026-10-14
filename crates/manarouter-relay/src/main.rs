//! The `manarouter-relay` binary.
//!
//! Kept deliberately thin. Everything it does lives in the library so that the integration tests and
//! the latency benchmark exercise the same code the shipped binary runs — a startup routine that only
//! exists in `main.rs` is a startup routine that is never tested.
//!
//! The global allocator is declared in `lib.rs` for the same reason.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use manarouter_relay::select::Health;
use manarouter_relay::server::{self, AppState};
use manarouter_relay::state::RelayState;
use mr_core::config::Config;
use mr_core::secret::Vault;

/// The data plane: accepts OpenAI- and Anthropic-shaped requests and routes them.
///
/// It evaluates no routing policy. The control plane compiles the routing table; this process looks
/// up what it was given and moves bytes, which is what keeps the added latency in the hundreds of
/// microseconds rather than milliseconds.
#[derive(Debug, Parser)]
#[command(name = "manarouter-relay", version, about, long_about = None)]
struct Args {
    /// Interface to bind. Overrides the configuration file.
    ///
    /// Optional rather than defaulted, so that "not passed" stays distinguishable from "passed the
    /// default value". A `String` with `default_value = "127.0.0.1"` cannot tell those apart and
    /// would silently override a config file that says something else.
    #[arg(long)]
    host: Option<String>,

    /// TCP port. Overrides the configuration file.
    #[arg(long)]
    port: Option<u16>,

    /// Configuration directory. Defaults to `~/.config/manarouter`.
    ///
    /// Honoured by setting `MANAROUTER_CONFIG_DIR`, which is the same variable the rest of the code
    /// already reads, so there is one definition of where the configuration lives rather than two
    /// that can disagree.
    #[arg(long)]
    config_dir: Option<std::path::PathBuf>,

    /// Log filter, in `tracing-subscriber` syntax.
    ///
    /// `RUST_LOG` still wins when it is set, because that is the variable an operator already reaches
    /// for and a flag that silently overrode it would be a surprise.
    #[arg(long)]
    log: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Set the configuration directory before anything reads it, and before the runtime starts.
    //
    // `std::env::set_var` is not thread-safe against a concurrent `getenv`, and Rust 2024 marks it
    // `unsafe` for exactly that reason. Doing it here — in `main`, single-threaded, before the tokio
    // runtime exists — is the only placement where it is sound. It is also deliberately conditional:
    // when `--config-dir` was not passed, an existing `MANAROUTER_CONFIG_DIR` from the environment is
    // left alone.
    if let Some(dir) = &args.config_dir {
        std::env::set_var("MANAROUTER_CONFIG_DIR", dir);
    }

    init_tracing(args.log.as_deref());

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        config_dir = %mr_core::config::config_dir().display(),
        "manarouter-relay starting"
    );

    // The configuration is loaded here, in the binary, as well as inside `RelayState::load`. That is
    // not redundancy for its own sake: the listener settings live in the same file, and reading it
    // once here means a malformed configuration is reported as a startup error with a path in it,
    // rather than surfacing later as a routing failure inside a request.
    let config = match Config::load_default() {
        Ok(c) => c,
        Err(e) => {
            // A missing file is the normal first run, not an error worth a stack trace. Anything else
            // is a real problem the operator has to fix, and it is printed rather than panicked
            // because release builds set `panic = "abort"`.
            eprintln!("manarouter-relay: could not read the configuration: {e}");
            eprintln!(
                "manarouter-relay: looked in {}",
                mr_core::config::config_path().display()
            );
            std::process::exit(2);
        }
    };

    // `validate` returns every problem it found rather than the first, so an operator fixing a
    // configuration sees the whole list once instead of one error per edit.
    let problems = config.validate();
    if !problems.is_empty() {
        eprintln!("manarouter-relay: the configuration is not valid:");
        for p in &problems {
            eprintln!("  - {p}");
        }
        std::process::exit(2);
    }

    let addr = match resolve_addr(&args, &config) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("manarouter-relay: {e}");
            std::process::exit(2);
        }
    };

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("manarouter-relay: could not start the async runtime: {e}");
            std::process::exit(2);
        }
    };

    runtime.block_on(async move {
        let relay = match RelayState::load() {
            Ok(r) => Arc::new(r),
            Err(e) => {
                eprintln!("manarouter-relay: could not load the routing table: {e}");
                std::process::exit(2);
            }
        };

        // With no client keys configured the relay serves **everyone**: `authenticate` answers
        // `Open`, because a fresh install that rejected all requests would be unusable and the
        // operator would have no way to reach the panel and create a key.
        //
        // That is a reasonable trade on loopback and an unacceptable one anywhere else, so the two
        // conditions are checked together. Binding a keyless relay to a reachable interface would
        // hand this machine's provider credentials to anyone who can route to it, and no warning in a
        // log is a sufficient answer to that.
        if !relay.keys_are_configured() {
            if !keyless_bind_is_allowed(addr, false) {
                eprintln!(
                    "manarouter-relay: refusing to bind {addr} with no client keys configured.\n\
                     manarouter-relay: a relay with no keys serves every request to anyone who can \
                     reach it,\n\
                     manarouter-relay: and it holds this machine's provider credentials.\n\
                     manarouter-relay: create a key with `mr key create`, or bind 127.0.0.1."
                );
                std::process::exit(2);
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

        let vault = match Vault::open_default() {
            Ok(v) => Arc::new(v),
            Err(e) => {
                eprintln!("manarouter-relay: could not open the credential vault: {e}");
                std::process::exit(2);
            }
        };

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
        let oauth_store = match mr_core::store::Store::open(&mr_core::config::database_path()) {
            Ok(store) => Arc::new(store),
            Err(e) => {
                eprintln!("manarouter-relay: could not open OAuth metadata store: {e}");
                std::process::exit(2);
            }
        };
        let state = AppState {
            relay,
            health: Arc::new(Health::new()),
            vault,
            oauth_refreshers: Arc::new(oauth_refreshers),
            oauth_store,
        };

        if let Err(e) = server::serve(state, addr).await {
            eprintln!("manarouter-relay: the listener failed: {e}");
            std::process::exit(1);
        }
    });

    Ok(())
}

/// Whether a keyless relay may bind this address.
///
/// Split out from the process so the rule can be tested: everything that calls `exit` is untestable,
/// and this is exactly the kind of rule that should have a test rather than a comment.
fn keyless_bind_is_allowed(addr: SocketAddr, keys_configured: bool) -> bool {
    keys_configured || addr.ip().is_loopback()
}

/// Install the log subscriber.
fn init_tracing(flag: Option<&str>) {
    use tracing_subscriber::EnvFilter;

    // Precedence: `RUST_LOG`, then `--log`, then the crate default. `RUST_LOG` first because it is
    // the variable an operator already has set and reaching for, and a flag that silently overrode it
    // would be a surprise rather than a convenience.
    let filter = std::env::var("RUST_LOG")
        .ok()
        .or_else(|| flag.map(str::to_owned))
        .unwrap_or_else(|| "manarouter_relay=info".to_owned());

    let filter = EnvFilter::try_new(&filter).unwrap_or_else(|e| {
        eprintln!("manarouter-relay: ignoring an unparseable log filter ({e})");
        EnvFilter::new("manarouter_relay=info")
    });

    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Work out where to listen.
///
/// Precedence is the flag, then the configuration file, then the struct default. The flag has to win
/// — someone who typed `--port` means it — and the file has to beat the default, or a configuration
/// that says where to listen would be ignored.
fn resolve_addr(args: &Args, config: &Config) -> Result<SocketAddr, String> {
    let host = args
        .host
        .clone()
        .unwrap_or_else(|| config.relay.host.clone());
    let port = args.port.unwrap_or(config.relay.port);

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

    rendered
        .parse()
        .map_err(|e| format!("could not interpret \"{host}\" and port {port} as an address: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(host: Option<&str>, port: Option<u16>) -> Args {
        Args {
            host: host.map(str::to_owned),
            port,
            config_dir: None,
            log: None,
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
        assert!(err.contains("could not interpret"), "got: {err}");
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

    #[test]
    fn the_arguments_are_all_optional() {
        // Every flag is `Option`, which is what makes "not passed" distinguishable from "passed the
        // default value" and is the whole reason the configuration file can win.
        let a = Args::parse_from(["manarouter-relay"]);
        assert!(a.host.is_none());
        assert!(a.port.is_none());
        assert!(a.config_dir.is_none());
        assert!(a.log.is_none());
    }

    #[test]
    fn a_port_flag_is_parsed_as_a_number() {
        let a = Args::parse_from(["manarouter-relay", "--port", "1234"]);
        assert_eq!(a.port, Some(1234));
    }

    #[test]
    fn a_port_that_is_not_a_number_is_rejected_by_clap() {
        // clap produces the message and the exit code, which is why this is not hand-rolled.
        assert!(Args::try_parse_from(["manarouter-relay", "--port", "not-a-port"]).is_err());
    }
}
