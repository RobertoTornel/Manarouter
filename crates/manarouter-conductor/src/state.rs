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
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Instant, SystemTime};

use mr_core::config::Config;
use mr_core::secret::Vault;
use mr_core::snapshot::{RoutingSnapshot, SnapshotHandle};
use tokio::sync::{Mutex, Notify, RwLock};

use crate::auth::PanelAuth;

/// Where the two directory-backed assets of the panel live.
///
/// The compiler cannot embed them: the spec preferred a compile-time embed (`include_dir`), but
/// the manifest for this crate is owned elsewhere, so the panel is served from disk instead. Both
/// directories are resolved **once, at startup** — a later `cd` or chdir cannot silently change
/// which `panel/` the process serves — and are `None` when nothing was found, which is what makes
/// "panel directory missing" a graceful placeholder page rather than a crash.
#[derive(Debug, Clone, Default)]
pub struct ServeDirs {
    /// Directory holding the panel's static shell (`index.html` and friends).
    pub panel: Option<PathBuf>,
    /// Directory holding the markdown documentation browsed at `GET /api/docs`.
    pub docs: Option<PathBuf>,
}

impl ServeDirs {
    /// Look for `panel/` and `docs/` next to the usual places an operator runs from: the working
    /// directory, its parents (for `cargo run` from a crate directory), and the executable's
    /// location (for a release binary under `target/release/`, and for `cargo test`, whose cwd is
    /// the crate root while the executable sits three levels up in the workspace).
    pub fn discover() -> Self {
        let mut bases: Vec<PathBuf> = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            bases.push(cwd.clone());
            let mut up = cwd;
            for _ in 0..2 {
                match up.parent() {
                    Some(p) => {
                        up = p.to_path_buf();
                        bases.push(up.clone());
                    }
                    None => break,
                }
            }
        }
        if let Ok(exe) = std::env::current_exe() {
            let mut up = exe.parent().map(Path::to_path_buf);
            for _ in 0..5 {
                match up {
                    Some(p) => {
                        up = p.parent().map(Path::to_path_buf);
                        bases.push(p);
                    }
                    None => break,
                }
            }
        }
        let panel = bases
            .iter()
            .map(|b| b.join("panel"))
            .find(|p| p.join("index.html").is_file());
        let docs = bases.iter().map(|b| b.join("docs")).find(|p| p.is_dir());
        Self { panel, docs }
    }
}

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
    /// Ephemeral provider authorization sessions. They never contain a completed token response.
    pub oauth_sessions: mr_core::oauth::session::OAuthSessions,
    /// Serialize concurrent configuration mutations so overlapping OAuth logins can't lose accounts.
    pub mutation_lock: Arc<Mutex<()>>,
    /// The usage database: what the relay recorded about traffic that has already happened.
    ///
    /// Read-only from here. The relay is the only writer — it owns the request path, and the
    /// conductor must never contend with it for a write lock on the hot path.
    pub store: Arc<mr_core::store::Store>,
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
    /// Where `panel/` and `docs/` were found at startup.
    pub dirs: ServeDirs,
    /// For uptime in `GET /api/health`.
    pub started_at: Instant,
    /// `(modified, length)` of `config.json` as of the last time this process read or wrote it.
    ///
    /// Used by [`Self::reload_from_disk`] to decide whether re-parsing the file could possibly
    /// discover anything. A `stat` is a few microseconds against a parse of a document with dozens of
    /// providers, so the endpoint layer can call the reload on every read without the panel paying
    /// for it. Length is checked as well as mtime because filesystem timestamp granularity is coarse
    /// enough (and the fast path cheap enough) that an edit within the same tick should still be
    /// noticed.
    pub config_seen: Arc<StdMutex<Option<(SystemTime, u64)>>>,
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
            Self::Validation(problems) => problems
                .iter()
                .cloned()
                .map(serde_json::Value::String)
                .collect(),
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

/// Backup journal for the vault writes performed while assembling an edit that may still fail.
///
/// See [`AppState::tx_put`]. Zero fields besides the writes themselves; `Default` is all a
/// handler needs.
#[derive(Debug, Default)]
pub struct VaultTx {
    /// `(vault id, value that was stored before this edit)`.
    writes: Vec<(String, Option<String>)>,
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

/// File-backed inputs required to construct the shared application state.
#[derive(Debug, Clone)]
pub struct StateFiles {
    pub config_path: PathBuf,
    pub snapshot_path: PathBuf,
    pub database_path: PathBuf,
    pub dirs: ServeDirs,
}

impl AppState {
    /// Build state from an already-loaded configuration and its file locations.
    ///
    /// `running_relay_port` should be the relay port this process started with;
    /// `running_control_port` the port actually bound (0 before the listener exists).
    pub fn new(
        config: Config,
        files: StateFiles,
        auth: PanelAuth,
        running_relay_port: u16,
        running_control_port: u16,
    ) -> Result<Self, String> {
        let StateFiles {
            config_path,
            snapshot_path,
            database_path,
            dirs,
        } = files;
        let dir = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .ok_or_else(|| format!("{} has no parent directory", config_path.display()))?
            .to_path_buf();
        // Creating the vault is allowed to fail here (an unreadable directory must stop startup)
        // but never inside a handler: the API should not discover at first save that the vault
        // could not be opened.
        let vault = Vault::open(&dir)
            .map_err(|e| format!("cannot open the vault in {}: {e}", dir.display()))?;
        let snapshot = compile_initial(&config, &snapshot_path)?;
        // Opened here for the same reason the vault is: a handler must not discover at its first
        // read that the database could not be opened. `Store::open` creates the schema when the
        // file is new, so a fresh install gets an empty usage history rather than an error.
        let store = mr_core::store::Store::open(&database_path)
            .map_err(|e| format!("cannot open {}: {e}", database_path.display()))?;
        let state = Self {
            config: Arc::new(RwLock::new(config)),
            config_path,
            snapshot_path,
            vault: Arc::new(Mutex::new(vault)),
            oauth_sessions: mr_core::oauth::session::OAuthSessions::new(),
            mutation_lock: Arc::new(Mutex::new(())),
            store: Arc::new(store),
            auth: Arc::new(RwLock::new(auth)),
            snapshot: SnapshotHandle::new(snapshot),
            restart: RestartSignal::default(),
            running_ports: Arc::new(RunningPorts {
                relay: AtomicU16::new(running_relay_port),
                control: AtomicU16::new(running_control_port),
            }),
            dirs,
            started_at: Instant::now(),
            config_seen: Arc::new(StdMutex::new(None)),
        };
        // Seed the fingerprint with what we just loaded, so the first read after startup does not
        // mistake the file we started from for an external edit.
        state.mark_config_seen();
        Ok(state)
    }

    /// Clone the current configuration.
    pub async fn current_config(&self) -> Config {
        self.config.read().await.clone()
    }

    /// Re-read `config.json` from disk and adopt it if it changed underneath us.
    ///
    /// # Why this exists
    ///
    /// The conductor holds the authored configuration in memory, and until now the only way it
    /// learned about an edit made by *another* process was to be restarted. `mr migrate
    /// omniroute`, `mr account add`, `mr provider add` and `mr key create` all write the same file
    /// the conductor is serving from, so an operator who imported accounts with the CLI saw a panel
    /// that did not list them — the providers and their models were on disk the whole time, and the
    /// panel was faithfully rendering a snapshot from before the import. Restarting the process
    /// "fixed" it, which is the tell that this is a cache-coherence bug and not a data problem.
    ///
    /// This is the read half of a watch: the endpoint layer calls it before serving a view of the
    /// configuration, so any API read reflects what is on disk. It is deliberately *not* a file
    /// watcher thread, because a watcher can only make reads faster, never more correct, and the
    /// cheap version has no background task, no debounce window and no events to lose.
    ///
    /// # What it will not do
    ///
    /// * **It never writes.** No save, no snapshot rewrite, no vault touch — those belong to
    ///   [`Self::commit`]. Reloading is not a way to persist an in-memory edit; that still requires
    ///   an explicit commit.
    /// * **It refuses a configuration that does not compile.** A file that was hand-edited into an
    ///   invalid state is reported and the previous configuration keeps serving, exactly as a
    ///   rejected commit behaves. A broken file must not take the panel down with it, because the
    ///   panel is how the operator would fix it.
    /// * **It does not fight a concurrent mutation.** The mutation lock is taken for the swap, so a
    ///   reload cannot interleave with a handler that is mid-edit and resurrect a stale view.
    ///
    /// Returns `true` when a different configuration was adopted.
    pub async fn reload_from_disk(&self) -> bool {
        // Fast path: unchanged since we last looked. This is what makes it affordable to call on
        // every panel read rather than only on a heuristic schedule.
        if !self.config_file_changed() {
            return false;
        }

        // Take the lock first: a mutation in flight is about to write the file *and* the memory
        // copy, so reading between its write and its publish would install a stale config.
        let _guard = self.mutation_lock.lock().await;

        // Re-check under the lock. A mutation that completed while we waited has already published
        // the new configuration, so re-reading here would be redundant rather than wrong.
        if !self.config_file_changed() {
            return false;
        }

        // Capture the fingerprint *before* the read: the bytes installed below must be the ones
        // remembered, or an edit that lands during the read is recorded as seen while the older
        // content is what got loaded, and it would never be picked up until the file changes again.
        let seen = self.config_fingerprint();
        let on_disk = match Config::load(&self.config_path) {
            Ok(config) => config,
            // A missing file is the normal "never edited by hand" case, not an error worth
            // logging on every read.
            Err(_) if !self.config_path.exists() => {
                tracing::debug!(path = %self.config_path.display(), "no configuration file to reload");
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    path = %self.config_path.display(),
                    error = %e,
                    "configuration could not be re-read; keeping the running copy"
                );
                return false;
            }
        };
        // Record what we read *before* comparing, so a later edit is still detected even when this
        // one turns out to be a no-op. A hand-edit that reorders keys, or a rewrite with identical
        // content, should not cause the same file to be re-parsed on every subsequent request.
        *self
            .config_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = seen;

        {
            let current = self.config.read().await;
            if *current == on_disk {
                return false;
            }
        }

        // Compile before adopting: an uncompilable file must not replace a working snapshot.
        let snapshot = match RoutingSnapshot::compile(&on_disk) {
            Ok(snapshot) => snapshot,
            Err(e) => {
                tracing::warn!(
                    path = %self.config_path.display(),
                    error = %e,
                    "the configuration on disk would not compile; keeping the running copy"
                );
                return false;
            }
        };

        // Install the snapshot first, so the relay-facing table and the panel's view never
        // disagree about which configuration is live.
        self.snapshot.install(snapshot);
        *self.config.write().await = on_disk;
        tracing::info!("configuration reloaded from disk");
        true
    }

    /// `(mtime, length)` of the configuration file, or `None` when it cannot be stat'd.
    fn config_fingerprint(&self) -> Option<(SystemTime, u64)> {
        let meta = std::fs::metadata(&self.config_path).ok()?;
        Some((meta.modified().ok()?, meta.len()))
    }

    /// Whether the file differs from the last fingerprint this process recorded.
    ///
    /// An unreadable or absent file reports `false`: there is nothing to reload, and treating a
    /// transient stat failure as a change would mean re-parsing (and possibly re-publishing) on
    /// every read.
    fn config_file_changed(&self) -> bool {
        let Some(now) = self.config_fingerprint() else {
            return false;
        };
        let seen = self
            .config_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *seen != Some(now)
    }

    /// Record the file's current fingerprint as seen.
    fn mark_config_seen(&self) {
        let Some(now) = self.config_fingerprint() else {
            return;
        };
        let mut seen = self
            .config_seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *seen = Some(now);
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
        self.commit_with(next, |cfg| cfg.validate()).await
    }

    /// [`Self::commit`] with a caller-supplied validator.
    ///
    /// Exactly one caller uses this (the provider-create endpoint in `api.rs`, which permits a
    /// provider that is *born* without accounts because the panel adds them afterwards — see the
    /// comment there). Every other mutation calls plain [`Self::commit`]. The compile-before-save
    /// ordering is identical either way: a custom validator is not a way to write a broken
    /// configuration, only to permit one shape of a valid one.
    pub async fn commit_with<F>(&self, next: Config, validate: F) -> Result<(), CommitError>
    where
        F: FnOnce(&Config) -> Vec<String>,
    {
        // (1) Validate. `validate` returns every problem, so the operator sees the whole list
        // in one round trip instead of fixing them one at a time.
        let problems = validate(&next);
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
        snapshot.write_to(&self.snapshot_path).map_err(|e| {
            CommitError::SnapshotWrite(format!("{}: {e}", self.snapshot_path.display()))
        })?;

        // (5) Publish.
        self.snapshot.install(snapshot);
        *self.config.write().await = next;
        // (6) Record what we just wrote, so the next read does not reload our own save. Without
        // this a commit would be followed by a re-parse of the file it wrote, on every read.
        self.mark_config_seen();
        Ok(())
    }

    /// Write one secret into the vault **on behalf of a pending edit**, remembering what was
    /// there before so the edit can be undone.
    ///
    /// Why the bookkeeping: a handler writes secrets while assembling the next configuration, but
    /// that configuration may still fail validation. Without a backup, a rejected edit would leave
    /// the new secret in the vault — and if the secret replaced an existing one, the *old* secret
    /// would be lost while `config.json` still points at its id. [`Self::tx_rollback`] restores
    /// exactly the previous state in that case.
    ///
    /// Each write is flushed to disk immediately: a crash right after a successful save must not
    /// leave the config referencing a secret that was never durably written.
    pub async fn tx_put(&self, tx: &mut VaultTx, id: &str, value: &str) -> Result<(), String> {
        let mut vault = self.vault.lock().await;
        let previous = vault.get(id).map_err(|e| e.to_string())?;
        vault.put(id, value).map_err(|e| e.to_string())?;
        vault.flush().map_err(|e| e.to_string())?;
        tx.writes.push((id.to_owned(), previous));
        Ok(())
    }

    /// Undo every secret write recorded in `tx`, newest first, and flush.
    ///
    /// Failures here are logged rather than propagated: the caller is already reporting a more
    /// important error, and the worst case is an orphaned vault entry (never a lost one — a
    /// restore that fails leaves the newer value, which is what the rollback could not fix
    /// anyway).
    pub async fn tx_rollback(&self, tx: VaultTx) {
        if tx.writes.is_empty() {
            return;
        }
        let mut vault = self.vault.lock().await;
        for (id, previous) in tx.writes.iter().rev() {
            let outcome = match previous {
                Some(value) => vault.put(id, value),
                None => {
                    vault.delete(id);
                    Ok(())
                }
            };
            if let Err(e) = outcome {
                tracing::warn!(secret_id = %id, error = %e, "could not roll back a vault write");
            }
        }
        if let Err(e) = vault.flush() {
            tracing::warn!(error = %e, "could not flush the vault after a rollback");
        }
    }

    /// Remove one secret from the vault (absent ids are fine).
    pub async fn delete_secret(&self, id: &str) {
        let mut vault = self.vault.lock().await;
        if vault.delete(id) {
            if let Err(e) = vault.flush() {
                tracing::warn!(secret_id = %id, error = %e, "could not flush the vault after a delete");
            }
        }
    }

    /// Remove every secret whose id starts with `prefix`, returning how many went.
    ///
    /// Used when an entity is deleted: `provider/{pid}/…` covers the account secrets, the
    /// provider-wide secret, and any OAuth material stored under the same prefix, without this
    /// crate having to know the OAuth module's id scheme.
    pub async fn delete_secrets_with_prefix(&self, prefix: &str) -> usize {
        let mut vault = self.vault.lock().await;
        let ids: Vec<String> = vault
            .ids()
            .into_iter()
            .filter(|id| id.starts_with(prefix))
            .collect();
        let mut removed = 0;
        for id in &ids {
            if vault.delete(id) {
                removed += 1;
            }
        }
        if removed > 0 {
            if let Err(e) = vault.flush() {
                tracing::warn!(error = %e, "could not flush the vault after a cascade delete");
            }
        }
        removed
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

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::dialect::Dialect;
    use mr_core::model::Provider;

    /// A minimal state on a private temp directory, so no test touches the real installation.
    fn state_in(dir: &Path) -> (AppState, PathBuf) {
        let config_path = dir.join("config.json");
        let snapshot_path = dir.join("routes.snapshot");
        let database_path = dir.join("state.sqlite");
        let config = Config::default();
        config.save(&config_path).expect("write the initial config");
        let state = AppState::new(
            config,
            StateFiles {
                config_path: config_path.clone(),
                snapshot_path,
                database_path,
                dirs: ServeDirs {
                    panel: None,
                    docs: None,
                },
            },
            PanelAuth::resolve("127.0.0.1", None, false).expect("loopback panel needs no password"),
            20900,
            20901,
        )
        .expect("build state");
        (state, config_path)
    }

    /// A configuration with one enabled provider, so a reload has something observable to change.
    fn with_provider(label: &str) -> Config {
        let mut config = Config::default();
        let mut provider = Provider::new("example", Dialect::OpenAi, "https://api.example.test/v1");
        provider.label = Some(label.to_owned());
        // A provider with no accounts is refused by `validate`, and these tests commit one, so the
        // fixture has to be a configuration the router would actually accept.
        provider
            .accounts
            .push(mr_core::model::Account::new("test account"));
        config.providers.push(provider);
        config
    }

    #[tokio::test]
    async fn an_external_edit_is_adopted_without_a_restart() {
        // The bug this exists for: `mr migrate omniroute` writes the config the conductor is
        // serving from, and the panel kept rendering the pre-import document until the process was
        // restarted. The data was never missing — only the reader was stale.
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, path) = state_in(dir.path());

        assert!(state.current_config().await.providers.is_empty());
        assert!(
            !state.reload_from_disk().await,
            "nothing changed yet, so nothing should reload"
        );

        with_provider("written by another process")
            .save(&path)
            .expect("simulate a CLI write");

        assert!(
            state.reload_from_disk().await,
            "an external write must be adopted"
        );
        let config = state.current_config().await;
        assert_eq!(config.providers.len(), 1);
        assert_eq!(
            config.providers[0].label.as_deref(),
            Some("written by another process")
        );
    }

    #[tokio::test]
    async fn a_reload_is_idempotent() {
        // The reload runs on every authenticated GET, so a second call with no intervening write
        // must be a no-op rather than re-parsing and re-installing the same document.
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, path) = state_in(dir.path());
        with_provider("once").save(&path).expect("external write");

        assert!(state.reload_from_disk().await, "first reload adopts it");
        assert!(
            !state.reload_from_disk().await,
            "a second reload with nothing changed must not report a change"
        );
    }

    #[tokio::test]
    async fn a_commit_is_not_immediately_re_read() {
        // A commit writes the file *and* publishes the configuration. Recording the fingerprint at
        // the end of the commit is what stops the next read from re-parsing what we just wrote.
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, _path) = state_in(dir.path());

        state
            .commit(with_provider("committed"))
            .await
            .expect("commit");
        assert!(
            !state.reload_from_disk().await,
            "our own save must not look like an external edit"
        );
    }

    #[tokio::test]
    async fn a_broken_file_does_not_replace_a_working_configuration() {
        // A hand-edit that does not compile must leave the running configuration alone: the panel
        // is how the operator would repair it, so a bad file must not take the panel down.
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, path) = state_in(dir.path());
        with_provider("good").save(&path).expect("external write");
        assert!(state.reload_from_disk().await, "adopt the good revision");

        std::fs::write(&path, b"{ this is not json").expect("write a broken file");

        assert!(
            !state.reload_from_disk().await,
            "an unparseable file must be refused"
        );
        let config = state.current_config().await;
        assert_eq!(
            config.providers.len(),
            1,
            "the working revision must still be serving"
        );
        assert_eq!(config.providers[0].label.as_deref(), Some("good"));
    }

    #[tokio::test]
    async fn a_missing_file_is_not_an_error() {
        // A fresh install with no config.json is normal. Treating it as a change would mean a
        // warning on every read.
        let dir = tempfile::tempdir().expect("temp dir");
        let (state, path) = state_in(dir.path());
        std::fs::remove_file(&path).expect("remove the config");

        assert!(!state.reload_from_disk().await);
        assert!(state.current_config().await.providers.is_empty());
    }
}
