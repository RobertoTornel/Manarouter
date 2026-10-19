//! `mr serve` — run the relay and the conductor together, for a foreground session.
//!
//! # Why this supervises two child processes
//!
//! The obvious implementation is one process with both services on a shared runtime, and this project
//! rejected that in writing before either service existed. `docs/01-architecture.md` calls it "Wrong
//! option 1": a request path and an admin panel have incompatible cost profiles, and a shared event
//! loop means the slow thing eventually blocks the fast thing. The reference router that motivated the
//! project measured 5.2–9.2 seconds for a purely local `/v1/models` request with its panel on the same
//! runtime.
//!
//! Two properties make that binding here rather than philosophical:
//!
//! - The workspace builds release binaries with `panic = "abort"`. In one process, a panic on a panel
//!   route would abort the process that is mid-completion for somebody's paid request. In two, the
//!   worst a broken panel can do is fail to render.
//! - The two services are separate binaries, each owning its own runtime, signal handling and
//!   shutdown. Running them as children costs one `fork`/`exec` and nothing per request.
//!
//! # What "supervise" means here, and what it does not
//!
//! This is a *foreground convenience*, not an init system. It does the three things a parent must do to
//! be safe to press Ctrl-C on, and deliberately not the rest:
//!
//! 1. Each child is put in **its own process group**, and `libc::killpg` signals that group so the
//!    signal reaches grandchildren too. Signalling only the direct child can leave a grandchild
//!    holding the listening port. (`libc` is already a dependency of this crate for exactly this.)
//! 2. `SIGINT`/`SIGTERM` are forwarded to both groups, then both children are waited for, so a Ctrl-C
//!    drains in-flight requests rather than orphaning them.
//! 3. If **either** child exits, the other is stopped and the exit status becomes this process's
//!    status. A half-running Manarouter is a trap: the panel would still list providers and answer the
//!    API while nothing could serve a request, and that reads as a provider outage rather than a dead
//!    process.
//!
//! It does **not** restart a crashed child, back off, or track health. `Restart=on-failure` in the
//! systemd units the installer writes is the supported way to run this unattended; a second,
//! half-implemented restart policy here would be worse than none.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use crate::output::{say, warn, CliError};
use crate::Ctx;

/// Environment variable that overrides the conductor's bind port for one run.
///
/// The conductor parses no command-line arguments of its own, so `--control-port` needs a channel it
/// already understands. An environment variable rather than a rewritten `config.json` is deliberate:
/// `mr serve --control-port` is a temporary override, and editing the operator's configuration as a
/// side effect of starting a process would persist a change they never asked to keep.
const CONTROL_PORT_ENV: &str = "MANAROUTER_CONTROL_PORT";

/// `mr serve` is a leaf command: it carries its own argument struct, which `main`'s `Command` holds as
/// a payload (a type implementing `clap::Args`).
#[derive(clap::Args, Debug)]
pub struct ServeCmd {
    /// Relay (data plane) port; defaults to config.json's `relay.port` (20900).
    #[arg(long, value_name = "PORT")]
    pub relay_port: Option<u16>,
    /// Conductor + panel port; defaults to config.json's `control.port` (20901).
    #[arg(long = "control-port", value_name = "PORT")]
    pub control_port: Option<u16>,
}

/// Where a service binary was found.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Located {
    /// Beside the running `mr` executable — a build tree or an install directory.
    Sibling(PathBuf),
    /// On `PATH`.
    OnPath(PathBuf),
}

impl Located {
    fn path(&self) -> &Path {
        match self {
            Located::Sibling(p) | Located::OnPath(p) => p,
        }
    }

    fn where_(&self) -> &'static str {
        match self {
            Located::Sibling(_) => "beside mr",
            Located::OnPath(_) => "on PATH",
        }
    }
}

/// Find one of the service binaries.
///
/// The running executable's directory is checked first, then `PATH`. That order is deliberate: a build
/// tree's `target/release/` holds a matching set of all three binaries, so `mr serve` run from a
/// checkout uses the build that was just made rather than whatever an older installer left in
/// `~/.local/bin` — which is exactly the mismatch that produces "my fix isn't running".
///
/// The error names both places it looked: a bare "not found" leaves the operator guessing which of two
/// installations is at fault.
fn locate(binary: &str) -> Result<Located, CliError> {
    let exe = std::env::current_exe().ok();

    if let Some(dir) = exe.as_deref().and_then(Path::parent) {
        let candidate = dir.join(binary);
        if is_executable(&candidate) {
            return Ok(Located::Sibling(candidate));
        }
    }

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if is_executable(&candidate) {
                return Ok(Located::OnPath(candidate));
            }
        }
    }

    Err(CliError::failure(format!(
        "could not find `{binary}`. Looked beside {} and on PATH. Build it with \
         `cargo build --release`, or install the services with `installer.sh --systemd-user`.",
        exe.map(|p| p.display().to_string())
            .unwrap_or_else(|| "this executable".into())
    )))
}

/// Whether a path is a file this process could execute.
///
/// `is_file` alone would accept a non-executable file and defer the failure to `spawn`, where the
/// message is a bare `Permission denied` that does not say which of the two locations was wrong.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// A running service.
struct Child {
    name: &'static str,
    child: tokio::process::Child,
    /// Process-group id, which equals the child's pid because it was made its own group leader.
    pgid: libc::pid_t,
}

impl Child {
    /// Spawn one service in a new process group.
    ///
    /// `process_group(0)` makes the child a group leader, so `killpg` on it reaches the child and
    /// anything it spawns. `kill_on_drop` is the backstop for the path where this process is killed
    /// outright and never reaches its own shutdown code.
    fn spawn(
        name: &'static str,
        path: &Path,
        args: &[String],
        env: &[(&str, String)],
    ) -> Result<Self, CliError> {
        let mut command = tokio::process::Command::new(path);
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .stdin(Stdio::null())
            // Inherited, not piped: this is a foreground command, and interleaving the children's lines
            // with this process's own is the point. Piping would mean buffering them and choosing an
            // order, which loses the one thing an operator wants — that a startup failure appears
            // immediately, in the order it happened.
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0)
            .kill_on_drop(true);

        let child = command.spawn().map_err(|e| {
            CliError::failure(format!("could not start {name} ({}): {e}", path.display()))
        })?;

        let Some(pid) = child.id() else {
            return Err(CliError::failure(format!(
                "could not start {name}: it exited before it could be supervised"
            )));
        };
        Ok(Self {
            name,
            child,
            pgid: pid as libc::pid_t,
        })
    }

    /// Ask the whole group to stop, then wait for the direct child.
    async fn stop(&mut self) {
        // SAFETY: `killpg` has no memory-safety contract — it takes a process-group id and a signal.
        // The pgid came from a child this process started as its own group leader, so it names that
        // group. It is > 0 because it is a live child's pid, and the kernel never returns 0 or a
        // negative value as a pid; a negative pgid would signal a different set of processes entirely.
        unsafe {
            libc::killpg(self.pgid, libc::SIGTERM);
        }
        // Bounded: a child that ignores SIGTERM must not make Ctrl-C hang. `kill_on_drop` is the hard
        // stop that covers this future being dropped instead of completing.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), self.child.wait()).await;
        tracing::debug!(service = self.name, "stopped");
    }

    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }
}

/// What ended the run.
enum Outcome {
    /// `SIGINT` or `SIGTERM`, named for the message.
    Signalled(&'static str),
    /// A child exited on its own, with its status.
    ChildExited(&'static str, std::io::Result<std::process::ExitStatus>),
}

/// Run `mr serve`.
pub async fn run(cmd: ServeCmd, ctx: &Ctx) -> Result<(), CliError> {
    // Both binaries are resolved *before* anything is spawned. Failing after starting one would leave a
    // stray relay holding the relay port, so the operator's next attempt fails with "address in use" —
    // an error about the recovery rather than about the problem.
    let relay = locate("manarouter-relay")?;
    let conductor = locate("manarouter-conductor")?;

    let relay_args: Vec<String> = cmd
        .relay_port
        .map(|p| vec!["--port".to_owned(), p.to_string()])
        .unwrap_or_default();
    let conductor_env: Vec<(&str, String)> = cmd
        .control_port
        .map(|p| vec![(CONTROL_PORT_ENV, p.to_string())])
        .unwrap_or_default();

    if !ctx.json {
        say(format!(
            "relay      : {}  ({} — {})",
            relay.path().display(),
            relay.where_(),
            port_label(cmd.relay_port, "relay.port")
        ));
        say(format!(
            "conductor  : {}  ({} — {})",
            conductor.path().display(),
            conductor.where_(),
            port_label(cmd.control_port, "control.port")
        ));
        say("both started; Ctrl-C stops both");
    }

    let mut relay_child = Child::spawn("relay", relay.path(), &relay_args, &[])?;
    let mut conductor_child = Child::spawn("conductor", conductor.path(), &[], &conductor_env)?;

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .map_err(|e| CliError::failure(format!("could not listen for SIGINT: {e}")))?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| CliError::failure(format!("could not listen for SIGTERM: {e}")))?;

    // Race a signal against either child exiting. `select!` cancels the losers, which is safe here:
    // `wait()` on a `tokio::process::Child` is cancel-safe, and dropping the signal futures only stops
    // listening, which is what happens on the way out anyway.
    let outcome = tokio::select! {
        _ = sigint.recv() => Outcome::Signalled("SIGINT"),
        _ = sigterm.recv() => Outcome::Signalled("SIGTERM"),
        status = relay_child.wait() => Outcome::ChildExited("relay", status),
        status = conductor_child.wait() => Outcome::ChildExited("conductor", status),
    };

    match &outcome {
        Outcome::Signalled(signal) => {
            if !ctx.json {
                say(format!("{signal} received; stopping both services"));
            }
        }
        Outcome::ChildExited(name, _) => {
            warn(format!("{name} exited; stopping the other service"));
        }
    }

    // Whatever happened, stop both. Stopping a child that already exited is a no-op: `killpg` on a
    // group with no live members returns ESRCH harmlessly.
    relay_child.stop().await;
    conductor_child.stop().await;

    match outcome {
        // A signal is how a foreground command is *supposed* to end, so it is a success.
        Outcome::Signalled(_) => Ok(()),
        Outcome::ChildExited(name, status) => {
            let status = status.map_err(|e| {
                CliError::failure(format!("could not read {name}'s exit status: {e}"))
            })?;
            match status.code() {
                // A child that exited 0 deliberately — the conductor's restart endpoint, for example —
                // is not a failure of this command.
                Some(0) => Ok(()),
                // The child's own status is named in the message, but `mr serve` itself exits with
                // the generic failure code (1): `CliError` carries an `Exit` enum whose documented
                // codes are 0-4, so a child's code cannot be forwarded verbatim without widening
                // that type. Whatever supervises `mr serve` sees exit 1 and reads the message.
                Some(code) => Err(CliError::failure(format!(
                    "{name} exited with status {code}"
                ))),
                None => Err(CliError::failure(format!(
                    "{name} was killed by a signal; the other service has been stopped"
                ))),
            }
        }
    }
}

fn port_label(port: Option<u16>, setting: &str) -> String {
    match port {
        Some(p) => format!("port {p}"),
        None => format!("{setting} from config"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::testutil::{cleanup, temp_dir};

    #[test]
    fn a_nonexistent_binary_is_a_clear_error_naming_both_places() {
        let err = locate("manarouter-definitely-not-a-real-binary").unwrap_err();
        let message = err.message;
        assert!(message.contains("beside"), "got: {message}");
        assert!(message.contains("PATH"), "got: {message}");
        assert!(
            message.contains("cargo build --release"),
            "the error must say how to fix it; got: {message}"
        );
    }

    #[test]
    fn an_executable_beside_this_test_binary_is_found_as_a_sibling() {
        // `current_exe` in a test is the test harness under `target/*/deps/`, so a file written there is
        // exactly the sibling case. Skipped rather than failed if the directory is not writable, since
        // that is an environment property and not the behaviour under test.
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(dir) = exe.parent() else { return };
        let probe = dir.join("mr-serve-locate-probe");
        if std::fs::write(&probe, b"#!/bin/sh\n").is_err() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755));

        let found = locate("mr-serve-locate-probe");
        let _ = std::fs::remove_file(&probe);

        match found {
            Ok(Located::Sibling(p)) => assert_eq!(p.file_name().unwrap(), "mr-serve-locate-probe"),
            other => panic!("expected a sibling match, got {other:?}"),
        }
    }

    #[test]
    fn a_non_executable_file_is_not_mistaken_for_a_binary() {
        // Otherwise the failure surfaces as a bare `Permission denied` from `spawn`, with no hint that
        // the path chosen was the wrong one.
        let dir = temp_dir("serve-non-executable");
        let plain = dir.join("not-executable");
        std::fs::write(&plain, b"data").expect("write");
        let verdict = is_executable(&plain);
        cleanup(&dir);
        assert!(!verdict);
    }

    #[test]
    fn an_executable_file_is_recognised() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("serve-executable");
        let script = dir.join("script");
        std::fs::write(&script, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let verdict = is_executable(&script);
        cleanup(&dir);
        assert!(verdict);
    }

    #[test]
    fn a_directory_is_never_a_binary() {
        // `metadata().is_file()` is what stops a directory named like the binary from being "found" and
        // then failing at `spawn` with EISDIR.
        let dir = temp_dir("serve-directory");
        let verdict = is_executable(&dir);
        cleanup(&dir);
        assert!(!verdict);
    }

    #[test]
    fn the_port_label_says_where_the_value_came_from() {
        assert_eq!(port_label(Some(20900), "relay.port"), "port 20900");
        assert_eq!(port_label(None, "relay.port"), "relay.port from config");
    }
}
