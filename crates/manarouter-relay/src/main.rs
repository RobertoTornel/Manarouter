//! The `manarouter-relay` binary.
//!
//! Kept deliberately thin. Everything it does lives in the library so that the integration tests and
//! the latency benchmark exercise the same code the shipped binary runs — a startup routine that only
//! exists in `main.rs` is a startup routine that is never tested.
//!
//! The global allocator is declared in `lib.rs` for the same reason.

use clap::Parser;

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

    // The startup sequence lives in `manarouter_relay::startup` so `mr serve` composes exactly the
    // same code path. `main` only owns the two things a library must not: the process-wide log
    // subscriber and the environment, both set above, plus the exit code below.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("could not start the async runtime: {e}"))?;

    let outcome = runtime.block_on(async move {
        manarouter_relay::startup::serve(manarouter_relay::startup::RelayOptions {
            host: args.host.clone(),
            port: args.port,
        })
        .await
    });

    // A startup refusal is **exit code 2**, and that is documented: `docs/23-implementation-status.md`
    // says the keyless-bind refusal exits 2. It is also the code an operator's script can distinguish
    // from a crash. Returning `Err` here would make it 1 through the `anyhow` path, which is why this
    // is a return code rather than a propagated error.
    //
    // The prefix is added here rather than in the library so the message reads as coming from this
    // binary, which is what it says in a log or a systemd journal.
    match outcome {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("manarouter-relay: {e}");
            std::process::exit(2);
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
