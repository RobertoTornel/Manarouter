//! Shared state for the control-plane API, and the one code path that commits a change.
//!
//! Every mutation in [`crate::api`] funnels through [`AppState::commit`]. That is deliberate:
//! there is exactly one place where an edited configuration reaches disk and exactly one place
//! where a compiled snapshot replaces the previous one, so "validate, save, compile, install"
//! cannot be forgotten in one handler and remembered in another.
//!
//! The ordering inside `commit` is the load-bearing part:
//!
//! 1. `Config::validate()` — an invalid configuration never reaches disk.
//! 2. `RoutingSnapshot::compile()` — a configuration the compiler rejects is reported as
//!    *validation* failure while the file is still untouched. Compilation is a stricter check
//!    than validation in a few places (a combo whose steps are all unreachable, for example),
//!    and those must behave like any other rejected edit: nothing written, old snapshot still
//!    serving.
//! 3. `Config::save()` — atomic temp-file + rename.
//! 4. `RoutingSnapshot::write_to()` — atomic as well, *after* the config is on disk.
//! 5. `SnapshotHandle::install()` — only after the bytes are durable.
//!
//! A failure at (3) changes nothing anywhere. A failure at (4) leaves the new configuration
//! saved but the previous snapshot serving: stale routing, not an outage, and an honest error
//! tells the operator exactly that. Installing before writing would invert the failure mode
//! into "the running snapshot came from a configuration this machine cannot reload", which is
//! the worse of the two.
//!
//! [`ArcSwap`]: mr_core::snapshot::SnapshotHandle

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Instant;

use mr_core::config::Config;
use mr_core::secret::Vault;
use mr_core::snapshot::{RoutingSnapshot, SnapshotHandle};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::auth::PanelAuth;

/// Everything the request handlers share.
///
/// Cheap to clone: every field is an `Arc` (or a handle that wraps one), so handlers clone the
/// state, never the data.
#[derive(Clone)]
pub struct AppState {
    /// The authored configuration. Read constantly, written only through [`Self::commit`].
    pub config: Arc<RwLock<Config>>,
    /// Where the configuration is saved. Held rather than re-derived so a test (or a second
    /// instance pointed at another directory) never has to touch the process environment.
    pub config_path: PathBuf,
    /// Where the compiled snapshot is written next to it.
    pub snapshot_path: PathBuf,
    /// The encrypted secret store. `tokio::sync::Mutex` because every mutation awaits on it.
    pub vault: Arc<Mutex<Vault>>,
    /// Panel authentication policy. Behind a lock because changing the panel password rebuilds
    /// it (and thereby revokes every live session — see [`AppState::replace_password_hash`]).
    pub auth: Arc<RwLock<PanelAuth>>,
    /// The compiled routing snapshot, swappable atomically.
    pub snapshot: SnapshotHandle,
    /// Cross-task signal: POST /api/restart sets it, `main` awaits it and exits cleanly.
    pub restart: RestartSignal,
    /// What the *running process* bound, captured at startup.
    ///
    /// `control` is the real bound port (which matters when the config says `0`).
    /// `relay` is the port present in the configuration when this process started: the relay is a
    /// separate process, so the conductor cannot observe its socket — it can only report the value
    /// the relay was last started from. The panel needs both to render "configured vs running".
    pub running_ports: Arc<RunningPorts>,
    /// For uptime in `GET /api/health`.
    pub started_at: Instant,
}

/// The ports this process believes are live.
#[derive(Debug, Default)]
pub struct RunningPorts {
    /// Port the relay was configured with at startup.
    pub relay: AtomicU16,
    /// Port the control plane actually bound.
    pub control: AtomicU16,
}

/// Why a commit was refused, after validation and compilation.
#[derive(Debug)]
pub enum CommitError {
    /// The configuration is invalid or will not compile. Nothing was written.
    Validation(Vec<String>),
    /// The configuration file could not be saved. Atomic save means the old file is intact.
    Save(String),
    /// The configuration was saved, but the snapshot could not be rewritten. The previous
    /// snapshot keeps serving — stale routing rather than an outage.
    SnapshotWrite(String),
}

impl CommitError {
    /// The problems, for the error envelope's `details` list.
    pub fn details(&self) -> Vec<serde_json::Value> {
        match self {
            Self::Validation(problems) => {
                problems.iter().map(serde_json::Value::from).collect()
            }
            other => vec![serde_json::Value::from(other.message())],
        }
    }

    /// A single-line description for the error message.
    pub fn message(&self) -> String {
        match self {
            Self::Validation(_) => "the configuration is not valid".to_string(),
            Self::Save(m) => format!("could not save the configuration: {m}"),
            Self::SnapshotWrite(m) => format!(
                "the configuration was saved, but the routing snapshot could not be rewritten; \
                 the previous snapshot keeps serving until the next successful save: {m}"
            ),
        }
    }

    /// Whether this is a client-fixable validation problem (400) rather than an I/O failure (500).
    pub fn is_validation(&self) -> bool {
        matches!(self, Self::Validation(_))
    }
}

/// A one-shot "please restart" signal from the API to `main`.
///
/// `Notify::notify_one` rather than `notify_waiters`: a permit is stored even when nobody is
/// waiting yet, so a signal raised before `main` reaches its await point is not lost.
#[derive(Clone, Default)]
pub struct RestartSignal {
    notify: Arc<Notify>,
    pending: Arc<AtomicBool>,
}

impl RestartSignal {
    /// Record that a restart has been requested and wake whoever is waiting.
    pub fn request(&self) {
        self.pending.store(true, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// Whether a restart has been requested but not yet performed.
    pub fn is_pending(&self) -> bool {
        self.pending.load(Ordering::SeqCst)
    }

    /// Wait for the signal. Safe to call after it was raised: the stored permit is consumed.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

impl AppState {
    /// Build state from an already-loaded configuration.
    ///
    /// `running_relay_port` should be the relay port this process started with;
    /// `running_control_port` the port actually bound (0 before the listener exists).
    pub fn new(
        config: Config,
        config_path: PathBuf,
        snapshot_path: PathBuf,
        auth: PanelAuth,
        running_relay_port: u16,
        running_control_port: u16,
    ) -> Result<Self, String> {
        let dir = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| format!("{} has no parent directory", config_path.display()))?
            .to_path_buf();
        // Creating the vault is allowed to fail here (an unreadable directory must stop startup)
        // but never inside a handler: the API should not discover at first save that the vault
        // could not be opened.
        let vault = Vault::open(&dir).map_err(|e| format!("cannot open the vault in {}: {e}", dir.display()))?;
        let snapshot = compile_initial(&config, &snapshot_path)?;
        Ok(Self {
            config: Arc::new(RwLock::new(config)),
            config_path,
            snapshot_path,
            vault: Arc::new(Mutex::new(vault)),
            auth: Arc::new(RwLock::new(auth)),
            snapshot: SnapshotHandle::new(snapshot),
            restart: RestartSignal::default(),
            running_ports: Arc::new(RunningPorts {
                relay: AtomicU16::new(running_relay_port),
                control: AtomicU16::new(running_control_port),
            }),
            started_at: Instant::now(),
        })
    }

    /// Clone the current configuration.
    pub async fn current_config(&self) -> Config {
        self.config.read().await.clone()
    }

    /// The port the control plane is actually listening on (falls back to the configured one
    /// when called before the bind).
    pub fn running_control_port(&self) -> u16 {
        let bound = self.running_ports.control.load(Ordering::SeqCst);
        if bound != 0 {
            return bound;
        }
        self.config
            .try_read()
            .map(|c| c.control.port)
            .unwrap_or_default()
    }

    /// The relay port this process started with.
    pub fn running_relay_port(&self) -> u16 {
        self.running_ports.relay.load(Ordering::SeqCst)
    }

    /// Persist `next`, compile it, and install the result. See the module docs for the ordering.
    ///
    /// On `Ok`, the file, the in-memory configuration, the snapshot on disk and the installed
    /// snapshot all describe the same configuration.
    pub async fn commit(&self, next: Config) -> Result<(), CommitError> {
        // (1) Validate. `validate` returns every problem, so the operator sees the whole list
        // in one round trip instead of fixing them one at a time.
        let problems = next.validate();
        if !problems.is_empty() {
            return Err(CommitError::Validation(problems));
        }

        // (2) Compile before writing anything: a config the compiler refuses must behave like
        // an invalid one — nothing touched, old snapshot still serving.
        let snapshot = RoutingSnapshot::compile(&next).map_err(|e| {
            CommitError::Validation(vec![format!("the routing snapshot would not compile: {e}")])
        })?;

        // (3) Save the configuration atomically.
        next.save(&self.config_path)
            .map_err(|e| CommitError::Save(format!("{}: {e}", self.config_path.display())))?;

        // (4) Write the snapshot only now that the configuration it describes is on disk.
        snapshot
            .write_to(&self.snapshot_path)
            .map_err(|e| CommitError::SnapshotWrite(format!("{}: {e}", self.snapshot_path.display())))?;

        // (5) Publish.
        self.snapshot.install(snapshot);
        *self.config.write().await = next;
        Ok(())
    }

    /// Write one secret into the vault and flush it to disk immediately.
    ///
    /// Flush-per-write rather than batched: a crash right after a save must not leave the config
    /// referencing a secret that was never durably written.
    pub async fn put_secret(&self, id: &str, value: &str) -> Result<(), String> {
        let mut vault = self.vault.lock().await;
        vault.put(id, value).map_err(|e| e.to_string())?;
        vault.flush().map_err(|e| e.to_string())
    }

    /// Remove one secret from the vault (absent ids are fine).
    pub async fn delete_secret(&self, id: &str) {
        let mut vault = self.vault.lock().await;
        if vault.delete(id) {
            let _ = vault.flush();
        }
    }

    /// Swap in a new panel-password hash.
    ///
    /// The whole `PanelAuth` is rebuilt rather than mutated: a fresh one carries a fresh session
    /// table, so changing the password revokes every existing session, which is exactly what
    /// `docs/15-web-panel.md` promises. (There is deliberately no setter on `PanelAuth` — a
    /// password change *is* a re-resolve.)
    pub async fn replace_password_hash(
        &self,
        hash: Option<String>,
        bind_host: &str,
        allow_remote_without_password: bool,
    ) -> Result<(), String> {
        let next = PanelAuth::resolve(bind_host, hash, allow_remote_without_password)
            .map_err(|e| e.to_string())?;
        *self.auth.write().await = next;
        Ok(())
    }

    /// Convenience used by the password endpoints: verify + rebuild + issue, in that order.
    pub async fn password_matches(&self, candidate: &str) -> bool {
        self.auth.read().await.login(candidate).is_some()
    }
}

/// Compile the snapshot the process starts with and write it to `path`.
///
/// A configuration that fails to compile at startup is a hard error with the compiler's own
/// message: the operator must see *why* rather than a process that starts and routes nothing.
pub fn compile_initial(config: &Config, path: &Path) -> Result<RoutingSnapshot, String> {
    let snapshot = RoutingSnapshot::compile(config)
        .map_err(|e| format!("the routing snapshot will not compile: {e}"))?;
    if let Err(e) = snapshot.write_to(path) {
        // Not fatal: the in-memory snapshot is what this process serves from, and refusing to
        // start because a state directory is unwritable would trade a working router for an
        // error message. Report it loudly instead.
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not persist the initial snapshot; it will be rewritten on the next save"
        );
    }
    Ok(snapshot)
}
