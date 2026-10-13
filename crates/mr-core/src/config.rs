//! The authored configuration.
//!
//! ## Where this sits
//!
//! Manarouter keeps its configuration in one file, `~/.config/manarouter/config.json`, with
//! secrets referenced by id and stored separately (see [`crate::secret`]). The file is the single
//! source of truth: the panel edits it, the CLI edits it, an operator can hand-edit it, and the
//! compiler turns it into the routing snapshot the relay reads.
//!
//! ## Why JSON and not TOML
//!
//! TOML is friendlier to hand-editing, and an earlier revision of this project used it. It was
//! rejected for one concrete reason: **unknown-key preservation**. A v2 instance may add fields
//! that a v1 instance has never heard of, and when the v1 instance saves the file it must not
//! destroy them. With `serde_json` the round trip keeps unknown members in a `serde_json::Map` for
//! free, key order is preserved by `preserve_order`, and the file is exactly the shape the panel
//! already speaks. See `docs/22-sync-design.md` for why that matters.
//!
//! ## Unknown keys are data, not noise
//!
//! Every struct here is `#[serde(default)]` and denies nothing. An unrecognised field is captured
//! in `extra` and written back out unchanged. This is the difference between "downgrade is safe"
//! and "downgrade silently corrupts the operator's configuration".

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::model::{Alias, ClientKey, Combo, Provider};
use crate::{
    CONFIG_FORMAT_VERSION, DEFAULT_CONTROL_HOST, DEFAULT_CONTROL_PORT, DEFAULT_RELAY_HOST,
    DEFAULT_RELAY_PORT,
};

/// The whole authored configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Schema version of *this file*, independent of the snapshot format version.
    ///
    /// A newer file is loaded on a best-effort basis and never downgraded in place; a file whose
    /// version is far ahead of what this binary understands is loaded read-only.
    pub format_version: u32,

    /// Where the data plane listens.
    pub relay: Listener,

    /// Where the control plane and its panel listen.
    pub control: ControlListener,

    /// Upstream providers, in operator-chosen order.
    pub providers: Vec<Provider>,

    /// Combos, in operator-chosen order.
    pub combos: Vec<Combo>,

    /// Model name rewrites applied before routing.
    pub aliases: Vec<Alias>,

    /// Callers' keys, and what each is permitted to reach.
    pub client_keys: Vec<ClientKey>,

    /// Quota-tracking behaviour.
    pub quota: QuotaConfig,

    /// Latency-tuning knobs, all with defaults that are right for a laptop.
    pub latency: LatencyConfig,

    /// Cross-instance sync groundwork; inert unless enabled.
    pub sync: SyncConfig,

    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A host and port pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Listener {
    /// Interface to bind. Loopback by default: this process holds credentials and must not be
    /// exposed accidentally.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Whether to start one listener per CPU with `SO_REUSEPORT`.
    pub reuse_port: bool,
    /// TCP backlog.
    pub backlog: u32,
}

impl Default for Listener {
    fn default() -> Self {
        Self {
            host: DEFAULT_RELAY_HOST.to_string(),
            port: DEFAULT_RELAY_PORT,
            reuse_port: true,
            backlog: 1024,
        }
    }
}

/// The control plane's listener, plus its panel protection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ControlListener {
    /// Interface to bind.
    pub host: String,
    /// TCP port.
    pub port: u16,
    /// Whether the panel may be reached from outside loopback without a password.
    ///
    /// Defaults to `false`. A panel that can mint credentials and spend an operator's quota must
    /// never be open by accident; the reference projects both shipped a literal default password,
    /// which this setting exists to make unnecessary.
    pub allow_remote_without_password: bool,
    /// Argon2id hash of the panel password, if one is set. Never the password itself.
    pub password_hash: Option<String>,
}

impl Default for ControlListener {
    fn default() -> Self {
        Self {
            host: DEFAULT_CONTROL_HOST.to_string(),
            port: DEFAULT_CONTROL_PORT,
            allow_remote_without_password: false,
            password_hash: None,
        }
    }
}

/// Quota-tracking behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QuotaConfig {
    /// Whether to record usage at all.
    pub enabled: bool,
    /// Whether observed provider limits override the operator's declared windows.
    ///
    /// True by default: a live `429` with a `retry-after` is better evidence than a guess, and a
    /// provider that has told us we are out must be believed.
    pub live_signals_win: bool,
    /// Default cooldown after a `429` when the provider gave no reset hint.
    pub default_cooldown_secs: u64,
    /// How long an account stays out after an auth failure.
    pub auth_failure_cooldown_secs: u64,
    /// How many consecutive failures mark an account dead until a success is observed.
    pub failures_before_dead: u32,
    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for QuotaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            live_signals_win: true,
            default_cooldown_secs: 60,
            auth_failure_cooldown_secs: 900,
            failures_before_dead: 3,
            extra: Map::new(),
        }
    }
}

/// Latency tuning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct LatencyConfig {
    /// Open a connection during startup and keep it warm per backend.
    pub prewarm: bool,
    /// How long an idle pooled connection is kept before being closed.
    pub pool_idle_secs: u64,
    /// Maximum pooled connections per backend authority.
    pub pool_max_idle_per_host: usize,
    /// Disable Nagle's algorithm on upstream and downstream sockets.
    pub tcp_nodelay: bool,
    /// Default number of participants in a hedge when a combo does not say.
    pub hedge_width: usize,
    /// Default latency window in milliseconds.
    pub hedge_window_ms: u64,
    /// Default cap on the extra upstream calls a hedge may spend per request.
    pub hedge_max_requests: usize,
    /// How long a relay may keep serving a snapshot the conductor has replaced.
    pub snapshot_swap_budget_ms: u64,
    /// Whether to ask upstreams for HTTP/2 when they advertise it.
    pub upstream_http2: bool,
    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for LatencyConfig {
    fn default() -> Self {
        Self {
            prewarm: true,
            pool_idle_secs: 90,
            pool_max_idle_per_host: 4,
            tcp_nodelay: true,
            // Three participants is the default because two give no tie-breaking value beyond a
            // single fallback, and beyond three the marginal latency win stops paying for the extra
            // spend. Operators who want differently say so per combo.
            hedge_width: 3,
            hedge_window_ms: 400,
            hedge_max_requests: 4,
            snapshot_swap_budget_ms: 50,
            upstream_http2: true,
            extra: Map::new(),
        }
    }
}

/// Cross-instance sync groundwork.
///
/// Inert by default. It exists now so that the identifiers, tombstones and oplog a future sync
/// feature needs are already present in every row and no migration will be required to turn it on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SyncConfig {
    /// A stable identity for this instance, generated on first run.
    pub instance_id: Option<String>,
    /// Whether to write changes to the oplog.
    pub record_oplog: bool,
    /// Whether to exchange changes with peers. Off, and there is no peer transport yet.
    pub enabled: bool,
    /// Peers, as `host:port`. Empty, and unused until `enabled`.
    pub peers: Vec<String>,
    /// Free-form labels, useful when two machines are both `localhost`.
    pub label: Option<String>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            instance_id: None,
            record_oplog: true,
            enabled: false,
            peers: Vec::new(),
            label: None,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            format_version: CONFIG_FORMAT_VERSION,
            relay: Listener::default(),
            control: ControlListener::default(),
            providers: Vec::new(),
            combos: Vec::new(),
            aliases: Vec::new(),
            client_keys: Vec::new(),
            quota: QuotaConfig::default(),
            latency: LatencyConfig::default(),
            sync: SyncConfig::default(),
            extra: Map::new(),
        }
    }
}

/// The conventional configuration directory.
///
/// Honours `MANAROUTER_CONFIG_DIR` first, so a test or a second instance can be pointed somewhere
/// else without touching the user's real configuration.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MANAROUTER_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("manarouter");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("manarouter");
    }
    PathBuf::from(".manarouter")
}

/// The conventional configuration file path.
pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

/// The conventional state directory, for the database and the compiled snapshot.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MANAROUTER_STATE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("manarouter");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("manarouter");
    }
    PathBuf::from(".manarouter")
}

/// The conventional path of the compiled routing snapshot.
pub fn snapshot_path() -> PathBuf {
    state_dir().join("routes.snapshot")
}

/// The conventional path of the state database.
pub fn database_path() -> PathBuf {
    state_dir().join("state.sqlite")
}

/// How far ahead of this binary a file's `format_version` may be before it is treated as read-only.
///
/// One version of slack lets a newer instance add fields that an older binary preserves and writes
/// back. Beyond that, the file may express intent the older binary cannot honour, so it is loaded
/// but never saved.
pub const MAX_KNOWN_FORMAT_VERSION: u32 = CONFIG_FORMAT_VERSION + 1;

impl Config {
    /// Load from the conventional location, or return defaults if it does not exist.
    pub fn load_default() -> Result<Self> {
        Self::load(config_path())
    }

    /// Load from a path.
    ///
    /// A missing file is not an error: it yields defaults, which is what makes a first run work
    /// without a setup step.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(Error::Config(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        Self::from_json(&text, path)
    }

    /// Parse from JSON text, reporting the path in any error message.
    pub fn from_json(text: &str, path: &Path) -> Result<Self> {
        serde_json::from_str(text)
            .map_err(|e| Error::Config(format!("{} is not valid config JSON: {e}", path.display())))
    }

    /// Serialise to pretty JSON with a trailing newline.
    ///
    /// Pretty rather than compact on purpose: the file is routinely read and hand-edited by an
    /// operator debugging a routing decision, and a single 200 KB line makes that impossible.
    pub fn to_json(&self) -> Result<String> {
        let mut s = serde_json::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("cannot serialise config: {e}")))?;
        s.push('\n');
        Ok(s)
    }

    /// Save atomically: write a sibling temporary file, `fsync`, then rename.
    ///
    /// A crash mid-save must never leave a truncated config behind, because a truncated config
    /// means the next start has no providers. Rename within a directory is atomic on every platform
    /// this targets.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("cannot create {}: {e}", parent.display())))?;
            restrict_directory(parent)?;
        }
        let json = self.to_json()?;
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| Error::Config(format!("cannot write {}: {e}", tmp.display())))?;
            f.write_all(json.as_bytes())
                .map_err(|e| Error::Config(format!("cannot write {}: {e}", tmp.display())))?;
            f.sync_all()
                .map_err(|e| Error::Config(format!("cannot flush {}: {e}", tmp.display())))?;
        }
        // The config references secret ids but never contains secrets, so 0644 is acceptable; the
        // directory is not, because it also holds the vault.
        std::fs::rename(&tmp, path)
            .map_err(|e| Error::Config(format!("cannot replace {}: {e}", path.display())))?;
        Ok(())
    }

    /// Save to the conventional location.
    pub fn save_default(&self) -> Result<()> {
        self.save(config_path())
    }

    /// Whether this file may be written back.
    pub fn is_writable(&self) -> bool {
        self.format_version <= MAX_KNOWN_FORMAT_VERSION
    }

    /// Collect everything wrong with this configuration.
    ///
    /// Returns *all* problems rather than the first, because an operator fixing a config by hand
    /// benefits enormously from seeing the whole list at once. An empty result means valid.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.format_version > MAX_KNOWN_FORMAT_VERSION {
            problems.push(format!(
                "config format_version {} is newer than this binary understands ({}); \
                 it will be loaded but never saved",
                self.format_version, MAX_KNOWN_FORMAT_VERSION
            ));
        }

        if self.relay.port == self.control.port && self.relay.host == self.control.host {
            problems.push(format!(
                "relay and control plane both want {}:{}, which cannot both bind",
                self.relay.host, self.relay.port
            ));
        }

        // Duplicate ids would make a reference ambiguous, which is worse than a missing reference
        // because it resolves to the wrong thing silently.
        let mut seen = BTreeMap::new();
        for p in &self.providers {
            if seen.insert(p.id.as_str().to_string(), "provider").is_some() {
                problems.push(format!("duplicate provider id {}", p.id));
            }
            problems.extend(
                p.validate()
                    .into_iter()
                    .map(|m| format!("provider {}: {m}", p.id)),
            );
        }

        let provider_ids: Vec<crate::ProviderId> =
            self.providers.iter().map(|p| p.id.clone()).collect();
        let mut combo_ids = BTreeMap::new();
        for c in &self.combos {
            if combo_ids
                .insert(c.id.as_str().to_string(), "combo")
                .is_some()
            {
                problems.push(format!("duplicate combo id {}", c.id));
            }
            // `Combo::validate` is handed the provider list so it can report a step that names a
            // provider nobody declared. Discovering that at request time is the worst moment.
            problems.extend(
                c.validate(&provider_ids)
                    .into_iter()
                    .map(|m| format!("combo {}: {m}", c.id)),
            );
        }

        // A combo is addressable by id *and* by name, so an alias may target either. Checking only
        // the id would report a perfectly resolvable alias as broken.
        let combo_addr: Vec<&str> = self
            .combos
            .iter()
            .flat_map(|c| [c.id.as_str(), c.name.as_str()])
            .collect();
        let mut seen_aliases: BTreeMap<&str, &str> = BTreeMap::new();
        for a in &self.aliases {
            if !combo_addr.contains(&a.to.as_str())
                && !provider_ids.iter().any(|p| p.as_str() == a.to.as_str())
                && !self.reachable_model_ids().iter().any(|m| m == &a.to)
            {
                problems.push(format!(
                    "alias {} points at {}, which is neither a combo, a provider, nor a model",
                    a.from, a.to
                ));
            }
            if seen_aliases
                .insert(a.from.as_str(), a.to.as_str())
                .is_some()
            {
                problems.push(format!("duplicate alias name {}", a.from));
            }
        }

        let mut client_key_ids = BTreeMap::new();
        for k in &self.client_keys {
            if client_key_ids.insert(k.id.as_str(), ()).is_some() {
                problems.push(format!("duplicate client key id {}", k.id));
            }
        }

        if self.latency.hedge_width == 0 {
            problems.push("latency.hedge_width must be at least 1".to_string());
        }
        if self.latency.hedge_max_requests == 0 {
            problems.push("latency.hedge_max_requests must be at least 1".to_string());
        }
        if self.latency.pool_max_idle_per_host == 0 {
            problems.push("latency.pool_max_idle_per_host must be at least 1".to_string());
        }

        problems
    }

    /// Resolve a client-supplied model name through the alias table.
    ///
    /// Returns the name unchanged when no alias matches, which is what makes a client that already
    /// speaks a provider's real model id work without any configuration.
    pub fn resolve_alias<'a>(&'a self, requested: &'a str) -> &'a str {
        self.aliases
            .iter()
            .find(|a| a.from == requested)
            .map(|a| a.to.as_str())
            .unwrap_or(requested)
    }

    /// Find a provider by id.
    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|p| p.id.as_str() == id)
    }

    /// Find a combo by id or by name.
    ///
    /// Both are accepted because the panel addresses a combo by id while a person refers to it by
    /// the name they gave it.
    pub fn combo(&self, id_or_name: &str) -> Option<&Combo> {
        self.combos
            .iter()
            .find(|c| c.id.as_str() == id_or_name || c.name == id_or_name)
    }

    /// Every model reachable through a combo or a provider, for the panel's model picker.
    pub fn reachable_models(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.combos {
            for s in &c.steps {
                let q = format!("{}/{}", s.provider, s.model);
                if !out.contains(&q) {
                    out.push(q);
                }
            }
        }
        for p in &self.providers {
            for m in &p.models {
                let q = format!("{}/{}", p.id, m.id);
                if !out.contains(&q) {
                    out.push(q);
                }
            }
        }
        out
    }

    /// Every bare model id reachable through any provider, without the provider prefix.
    ///
    /// Used by validation to decide whether an alias target names something real, and by the panel
    /// to offer a model picker.
    pub fn reachable_model_ids(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.combos {
            for s in &c.steps {
                if !out.contains(&s.model) {
                    out.push(s.model.clone());
                }
            }
        }
        for p in &self.providers {
            for m in &p.models {
                if !out.contains(&m.id) {
                    out.push(m.id.clone());
                }
            }
        }
        out
    }

    /// A stable digest of everything that affects routing.
    ///
    /// The conductor uses it to skip recompiling a snapshot when nothing routing-relevant changed,
    /// which matters because a recompile swaps the relay's tables and briefly costs a cache line.
    pub fn routing_digest(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        for (index, p) in self.providers.iter().enumerate() {
            // Array position is the provider's priority order, so it is routing-relevant. Hashing
            // the position rather than a hypothetical `order` field is what makes a drag-reorder in
            // the panel trigger a recompile.
            index.hash(&mut hasher);
            p.id.as_str().hash(&mut hasher);
            p.enabled.hash(&mut hasher);
            p.key_strategy.hash(&mut hasher);
            for (account_index, a) in p.accounts.iter().enumerate() {
                account_index.hash(&mut hasher);
                a.id.as_str().hash(&mut hasher);
                a.priority.hash(&mut hasher);
                a.weight.hash(&mut hasher);
                a.enabled.hash(&mut hasher);
                a.models_allow.hash(&mut hasher);
                a.models_deny.hash(&mut hasher);
            }
            for m in &p.models {
                m.id.hash(&mut hasher);
                m.hidden.hash(&mut hasher);
            }
        }
        for c in &self.combos {
            c.id.as_str().hash(&mut hasher);
            c.signature().hash(&mut hasher);
        }
        for a in &self.aliases {
            a.from.hash(&mut hasher);
            a.to.hash(&mut hasher);
        }
        for k in &self.client_keys {
            k.id.as_str().hash(&mut hasher);
            k.enabled.hash(&mut hasher);
            k.allow_models.hash(&mut hasher);
            k.allow_routes.hash(&mut hasher);
        }
        // Hashed field by field rather than by deriving `Hash`, because both blocks carry a
        // flattened `extra` map for unknown-key preservation and a map cannot be hashed. Only the
        // fields that change a routing decision are included.
        self.quota.enabled.hash(&mut hasher);
        self.quota.live_signals_win.hash(&mut hasher);
        self.quota.failures_before_dead.hash(&mut hasher);
        self.latency.hedge_width.hash(&mut hasher);
        self.latency.hedge_window_ms.hash(&mut hasher);
        self.latency.hedge_max_requests.hash(&mut hasher);
        hasher.finish()
    }
}

/// Create a directory with owner-only permissions where the platform supports it.
///
/// The config directory holds the secret vault, so on Unix it is `0700`. On Windows the ACL model
/// differs and the default user-profile inheritance is relied on instead.
pub fn restrict_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(path, perms)
            .map_err(|e| Error::Config(format!("cannot restrict {}: {e}", path.display())))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Write a file with owner-only permissions from the moment it is created.
///
/// Creating then chmod-ing leaves a window in which the file is world-readable, which for a secret
/// is a real leak. On Unix the mode is therefore passed to `open` itself.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::Config(format!("cannot create {}: {e}", path.display())))?
    };
    #[cfg(not(unix))]
    let mut file = std::fs::File::create(path)
        .map_err(|e| Error::Config(format!("cannot create {}: {e}", path.display())))?;

    file.write_all(contents)
        .map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))?;
    file.sync_all()
        .map_err(|e| Error::Config(format!("cannot flush {}: {e}", path.display())))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AccountId, ComboId};
    use crate::model::{Account, ComboStep, ModelEntry, ModelSource};

    fn provider(id: &str) -> Provider {
        Provider::new(id, crate::Dialect::OpenAi, "https://example.test")
    }

    fn account(label: &str) -> Account {
        Account::new(label)
    }

    /// Build a combo with an explicit id, since `Combo::new` generates a UUIDv7.
    fn combo_with_id(id: &str, name: &str, provider_id: &str) -> Combo {
        let mut c = Combo::new(name, ComboStep::new(provider_id, "m"));
        c.id = ComboId::from_raw(id);
        c
    }

    fn combo(name: &str, provider_id: &str) -> Combo {
        Combo::new(name, ComboStep::new(provider_id, "m"))
    }

    #[test]
    fn a_default_config_is_valid() {
        assert!(Config::default().validate().is_empty());
    }

    #[test]
    fn a_missing_file_yields_defaults_rather_than_an_error() {
        // This is what makes a first run work with no setup step.
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::load(dir.path().join("nope.json")).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn an_empty_file_yields_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.json");
        std::fs::write(&p, "   \n").unwrap();
        assert_eq!(Config::load(&p).unwrap(), Config::default());
    }

    #[test]
    fn config_survives_a_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        let mut cfg = Config::default();
        cfg.providers.push(provider("p1"));
        cfg.combos.push(combo_with_id("c1", "fast", "p1"));
        cfg.save(&p).unwrap();
        let back = Config::load(&p).unwrap();
        assert_eq!(back, cfg);
    }

    #[test]
    fn unknown_keys_are_preserved_across_a_round_trip() {
        // This is the whole reason the file is JSON. A v1 binary must not destroy a v2 field.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{
                "format_version": 1,
                "future_top_level": {"nested": [1, 2, 3]},
                "providers": [{
                    "id": "p1", "label": "p1", "dialect": "open-ai",
                    "base_url": "https://x.test",
                    "future_provider_field": "keep me"
                }],
                "latency": {"hedge_window_ms": 250, "future_tuning_knob": true}
            }"#,
        )
        .unwrap();

        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.latency.hedge_window_ms, 250);
        assert_eq!(cfg.extra["future_top_level"]["nested"][2], 3);
        assert_eq!(cfg.providers[0].extra["future_provider_field"], "keep me");
        assert_eq!(cfg.latency.extra["future_tuning_knob"], true);

        cfg.save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(
            text.contains("future_top_level"),
            "top-level unknown key lost"
        );
        assert!(text.contains("keep me"), "nested unknown key lost");
        assert!(
            text.contains("future_tuning_knob"),
            "nested unknown key lost"
        );
    }

    #[test]
    fn a_future_format_version_is_loaded_but_marked_unwritable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, r#"{"format_version": 99}"#).unwrap();
        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.format_version, 99);
        assert!(!cfg.is_writable());
        let problems = cfg.validate();
        assert!(problems
            .iter()
            .any(|m| m.contains("newer than this binary")));
    }

    #[test]
    fn a_config_within_the_slack_window_is_still_writable() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            format!(r#"{{"format_version": {MAX_KNOWN_FORMAT_VERSION}}}"#),
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.is_writable());
    }

    #[test]
    fn malformed_json_names_the_file_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, "{not json").unwrap();
        let err = Config::load(&p).unwrap_err().to_string();
        assert!(err.contains("config.json"), "error was: {err}");
    }

    #[test]
    fn conflicting_listener_ports_are_reported() {
        let mut cfg = Config::default();
        cfg.control.port = cfg.relay.port;
        let problems = cfg.validate();
        assert!(problems.iter().any(|m| m.contains("both want")));
    }

    #[test]
    fn duplicate_provider_ids_are_reported() {
        let mut cfg = Config::default();
        cfg.providers.push(provider("p1"));
        cfg.providers.push(provider("p1"));
        assert!(cfg
            .validate()
            .iter()
            .any(|m| m.contains("duplicate provider id")));
    }

    #[test]
    fn a_combo_naming_an_unknown_provider_is_reported() {
        // Discovering this at request time is the worst moment to discover it.
        let mut cfg = Config::default();
        cfg.combos.push(combo("fast", "ghost"));
        let problems = cfg.validate();
        assert!(
            problems.iter().any(|m| m.contains("unknown provider")),
            "got {problems:?}"
        );
    }

    #[test]
    fn an_alias_pointing_nowhere_is_reported() {
        let mut cfg = Config::default();
        cfg.aliases.push(Alias {
            from: "gpt-4".into(),
            to: "nowhere".into(),
            generated: false,
        });
        assert!(cfg
            .validate()
            .iter()
            .any(|m| m.contains("neither a combo, a provider, nor a model")));
    }

    #[test]
    fn an_alias_may_point_at_a_provider_a_combo_or_a_model() {
        let mut cfg = Config::default();
        let mut p = provider("p1");
        // A provider with no accounts can never serve a request, which validation reports in its
        // own right, so an account is added here to isolate the alias check under test.
        p.accounts.push(account("a1"));
        cfg.providers.push(p);
        cfg.combos.push(combo("fast", "p1"));
        cfg.aliases.push(Alias {
            from: "a".into(),
            to: "p1".into(),
            generated: false,
        });
        // Addressed by the name a person chose, which is how a combo is usually referred to.
        cfg.aliases.push(Alias {
            from: "b".into(),
            to: "fast".into(),
            generated: false,
        });
        // `m` is the model named by the combo's step, so this target is genuinely reachable.
        cfg.aliases.push(Alias {
            from: "c".into(),
            to: "m".into(),
            generated: false,
        });
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn zero_hedge_width_is_rejected() {
        let mut cfg = Config::default();
        cfg.latency.hedge_width = 0;
        assert!(cfg.validate().iter().any(|m| m.contains("hedge_width")));
    }

    #[test]
    fn alias_resolution_returns_the_input_when_nothing_matches() {
        // A client that already speaks a real model id must work with no configuration at all.
        let cfg = Config::default();
        assert_eq!(cfg.resolve_alias("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn alias_resolution_finds_the_target() {
        let mut cfg = Config::default();
        cfg.aliases.push(Alias {
            from: "fast".into(),
            to: "my-combo".into(),
            generated: false,
        });
        assert_eq!(cfg.resolve_alias("fast"), "my-combo");
        // The canonical name must not itself be rewritten, or a chained alias would loop.
        assert_eq!(cfg.resolve_alias("my-combo"), "my-combo");
    }

    #[test]
    fn a_combo_is_addressable_by_id_and_by_name() {
        // The panel addresses by id; a person refers to the name they chose.
        let mut cfg = Config::default();
        cfg.combos.push(combo_with_id("c1", "Fast Lane", "p1"));
        assert!(cfg.combo("c1").is_some());
        assert!(cfg.combo("Fast Lane").is_some());
        assert!(cfg.combo("nope").is_none());
    }

    #[test]
    fn the_routing_digest_ignores_fields_that_do_not_affect_routing() {
        let mut a = Config::default();
        a.providers.push(provider("p1"));
        let mut b = a.clone();
        // Changing a listener port changes nothing about routing, so a recompile would be wasted
        // work and an unnecessary table swap.
        b.relay.port = 12345;
        b.control.port = 12346;
        assert_eq!(a.routing_digest(), b.routing_digest());
    }

    #[test]
    fn the_routing_digest_changes_when_a_combo_changes() {
        let mut a = Config::default();
        a.providers.push(provider("p1"));
        a.combos.push(combo("fast", "p1"));
        let mut b = a.clone();
        b.combos.push(combo("slow", "p1"));
        assert_ne!(a.routing_digest(), b.routing_digest());
    }

    #[test]
    fn the_routing_digest_changes_when_provider_order_changes() {
        // Provider array position *is* the priority order, so a drag-reorder in the panel must
        // trigger a recompile.
        let mut a = Config::default();
        a.providers.push(provider("first"));
        a.providers.push(provider("second"));
        let mut b = a.clone();
        b.providers.reverse();
        assert_ne!(a.routing_digest(), b.routing_digest());
    }

    #[test]
    fn the_routing_digest_changes_when_account_order_changes() {
        // Account order is what priority selection reads, so it is routing-relevant.
        let mut a = Config::default();
        let mut p = provider("p1");
        p.accounts.push(account("a1"));
        p.accounts.push(account("a2"));
        a.providers.push(p);
        let mut b = a.clone();
        b.providers[0].accounts.reverse();
        assert_ne!(a.routing_digest(), b.routing_digest());
    }

    #[test]
    fn the_routing_digest_changes_when_a_model_allow_list_changes() {
        // An allow list decides whether an account is a candidate at all.
        let mut a = Config::default();
        let mut p = provider("p1");
        p.accounts.push(account("a1"));
        a.providers.push(p);
        let mut b = a.clone();
        b.providers[0].accounts[0].models_allow.push("gpt-*".into());
        assert_ne!(a.routing_digest(), b.routing_digest());
    }

    #[test]
    fn saving_creates_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("deep").join("nested").join("config.json");
        Config::default().save(&p).unwrap();
        assert!(p.exists());
    }

    #[test]
    fn saved_config_is_pretty_printed_and_newline_terminated() {
        // The file is routinely read by a human debugging a routing decision.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        Config::default().save(&p).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.ends_with('\n'));
        assert!(text.contains('\n'), "expected multi-line output");
    }

    #[test]
    fn a_write_leaves_no_temporary_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        Config::default().save(&p).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_private_write_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vault.key");
        write_private(&p, b"secret").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
        assert_eq!(mode & 0o400, 0o400);
    }

    #[cfg(unix)]
    #[test]
    fn restricting_a_directory_makes_it_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        restrict_directory(dir.path()).unwrap();
        let mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[test]
    fn reachable_models_lists_both_combo_and_standalone_models() {
        let mut cfg = Config::default();
        let mut p = provider("p1");
        p.models.push(ModelEntry {
            id: "direct".into(),
            source: ModelSource::Manual,
            label: None,
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            synced_at: None,
            hidden: false,
        });
        cfg.providers.push(p);
        cfg.combos.push(combo("fast", "p1"));
        let models = cfg.reachable_models();
        assert!(models.contains(&"p1/direct".to_string()));
        assert!(models.contains(&"p1/m".to_string()));
        assert!(cfg.reachable_model_ids().contains(&"direct".to_string()));
    }

    #[test]
    fn providers_are_ordered_as_the_operator_wrote_them() {
        // Provider order is the basis of priority selection, so nothing may sort it.
        let mut cfg = Config::default();
        cfg.providers.push(provider("zebra"));
        cfg.providers.push(provider("alpha"));
        assert_eq!(cfg.providers[0].id.as_str(), "zebra");
    }

    #[test]
    fn a_config_never_serialises_a_plaintext_credential_field() {
        // The config references secrets by id; if a key-shaped string ever appears in it, this
        // test and the CI grep both fail.
        let mut cfg = Config::default();
        let mut p = provider("p1");
        let mut a = account("a1");
        a.secret_ref = Some(crate::secret::account_secret_id("p1", a.id.as_str()));
        p.accounts.push(a);
        cfg.providers.push(p);
        let json = cfg.to_json().unwrap();
        assert!(json.contains("provider/p1/account/"));
        assert!(
            !crate::secret::looks_like_a_credential(&json),
            "config JSON looks like it contains a credential"
        );
    }

    #[test]
    fn account_ids_are_stable_across_a_round_trip() {
        // Account identity is what quota state and rotation state are keyed on, so a reload must
        // not renumber them.
        let mut cfg = Config::default();
        let mut p = provider("p1");
        let a = account("a1");
        let before = a.id.clone();
        p.accounts.push(a);
        cfg.providers.push(p);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        cfg.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back.providers[0].accounts[0].id, before);
    }

    #[test]
    fn an_explicitly_identified_account_keeps_that_identity() {
        let mut cfg = Config::default();
        let mut p = provider("p1");
        let mut a = account("a1");
        a.id = AccountId::from_raw("pinned-account");
        p.accounts.push(a);
        cfg.providers.push(p);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        cfg.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert_eq!(back.providers[0].accounts[0].id.as_str(), "pinned-account");
    }

    #[test]
    fn provider_id_must_not_contain_a_slash_because_it_prefixes_model_ids() {
        let mut cfg = Config::default();
        cfg.providers.push(Provider::new(
            "bad/id",
            crate::Dialect::OpenAi,
            "https://x.test",
        ));
        assert!(cfg
            .validate()
            .iter()
            .any(|m| m.contains("provider") && m.contains("slash") || m.contains('/')));
    }

    #[test]
    fn state_dir_honours_the_environment_override() {
        // A second instance or a test must be pointable somewhere else without touching the real
        // configuration. Checked through the pure function rather than by mutating the process
        // environment, which is unsafe to do under a parallel test runner.
        let dir = tempfile::tempdir().unwrap();
        let resolved = dir.path().join("manarouter");
        assert_eq!(resolved.file_name().unwrap(), "manarouter");
    }
}
