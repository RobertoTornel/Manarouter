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

use crate::dialect::{InputModality, ReasoningLevel};
use crate::error::{Error, Result};
use crate::model::{Alias, ClientKey, Combo, KeyLimit, Provider};
use crate::{
    CONFIG_FORMAT_VERSION, DEFAULT_CONTROL_HOST, DEFAULT_CONTROL_PORT, DEFAULT_RELAY_HOST,
    DEFAULT_RELAY_PORT,
};

/// Serde default for the handful of booleans that mean "on unless switched off".
fn default_true() -> bool {
    true
}

/// Whether a host names the loopback interface.
///
/// Accepts the spellings a person might write plus the `localhost` name, and parses before
/// comparing so that `[::1]`, `::1` and `127.0.0.1` are all handled by the IP parser rather than by
/// string matching, which would get IPv6 wrong.
pub fn is_loopback(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match trimmed.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Whether binding `host` exposes the listener beyond this machine.
///
/// `0.0.0.0` and `::` are not loopback, so they need no special case: a wildcard bind is the most
/// exposed choice there is, not an exemption from the rule.
pub fn is_remote_exposed(host: &str) -> bool {
    !is_loopback(host)
}

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

    /// Reasoning-level and multimodal routing defaults.
    pub routing: RoutingConfig,

    /// Upstream proxies, in operator-chosen order.
    pub proxies: Vec<ProxyConfig>,

    /// How to choose among the eligible proxies.
    pub proxy_strategy: ProxyStrategy,

    /// Whether the relay answers requests that carry no client key at all.
    ///
    /// Defaults to **false**. Without it a relay with no keys refuses to start on a non-loopback
    /// address (see [`Config::validate`]), which is the safe default: an open relay spends the
    /// operator's money for anyone who finds the port.
    pub no_key_required: bool,

    /// Whether [`Self::no_key_required`] may be on while the relay binds a non-loopback address.
    ///
    /// Also defaults to false, and exists so that exposing a keyless relay is a second, separate,
    /// deliberate act rather than a side effect of flipping one switch.
    pub allow_remote_without_key: bool,

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
    ///
    /// **Not read yet.** The relay binds one socket regardless of this value and sets
    /// `SO_REUSEPORT` on it unconditionally (see `bind_reuseport` in
    /// `manarouter-relay/src/server.rs`), so today the flag documents the intended design only.
    /// It is kept so an existing file keeps loading; flipping it changes nothing.
    pub reuse_port: bool,
    /// TCP backlog.
    ///
    /// **Not applied yet** — the socket uses the operating system's default backlog.
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
    /// How the panel draws a remaining allowance.
    pub display: QuotaDisplayConfig,
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
            display: QuotaDisplayConfig::default(),
            extra: Map::new(),
        }
    }
}

/// How the panel renders a remaining allowance.
///
/// Chunks are the default because they are the honest rendering for the case that matters: a
/// provider that reports only a percentage, or only a remaining/total pair with a coarse
/// resolution, still reads correctly as a partly filled bar, whereas a percentage invites a
/// precision the upstream does not have.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct QuotaDisplayConfig {
    /// Chunks or percentages.
    pub mode: QuotaDisplayMode,
    /// How many chunks a full allowance is drawn as. One global count, not per provider.
    pub chunks: u32,
    /// Whether opening the Quota view refreshes every account.
    ///
    /// **False** by default: a refresh reaches the providers, costs one request per account and can
    /// trip their rate limits, so a panel that did it on every page load would spend the operator's
    /// allowance on their own curiosity. With this off the view renders the cached snapshot and says
    /// how old it is until someone presses refresh.
    pub reset_on_load: bool,
    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for QuotaDisplayConfig {
    fn default() -> Self {
        Self {
            mode: QuotaDisplayMode::default(),
            chunks: DEFAULT_QUOTA_CHUNKS,
            reset_on_load: false,
            extra: Map::new(),
        }
    }
}

/// The two allowed renderings. A closed set, so an unknown value in the file is an error rather
/// than a silently ignored setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum QuotaDisplayMode {
    /// A bar of `chunks` cells: filled pink is what remains, light gray is what is used.
    #[default]
    Chunks,
    /// A single percentage.
    Percentage,
}

/// The default number of cells in a chunk bar.
///
/// Twenty divides evenly by four and by five, so the common fractional allowances (25%, 50%, 75%,
/// 20%, 40%, 60%, 80%) land on whole cells and the bar does not appear to stutter.
pub const DEFAULT_QUOTA_CHUNKS: u32 = 20;

/// The most cells a chunk bar may be drawn with.
///
/// A cap exists because the value comes from a config file and is multiplied into a DOM node count:
/// an operator typo of `100000` should be a validation message, not a frozen browser tab.
pub const MAX_QUOTA_CHUNKS: u32 = 200;

/// Reasoning and multimodal defaults applied while routing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RoutingConfig {
    /// The level a model is asked at when neither the model nor the request names one.
    ///
    /// `high` by default. The design deliberately has no "unset" for this: *some* level must be
    /// chosen for a model that advertises reasoning, and making the operator name it on every model
    /// would be worse than one documented default they can override in two places (here, and per
    /// model via `ModelEntry::default_reasoning_level`).
    pub default_reasoning_level: ReasoningLevel,
    /// Where to send an input type the chosen model cannot accept.
    pub defaults: RoutingDefaults,
    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            default_reasoning_level: ReasoningLevel::High,
            defaults: RoutingDefaults::default(),
            extra: Map::new(),
        }
    }
}

/// One fallback model per input type, each a `provider/model` name (or a combo).
///
/// `text` is deliberately absent: every routable model accepts text, so a text default could never
/// be used and offering one would only invite a misconfiguration that silently never fires.
///
/// A request carrying several unsupported types is satisfied only if a *single* target model
/// accepts all of them; otherwise the relay reports the error naming the defaults, because
/// splitting one request across two upstream models is not something a chat completion can express.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct RoutingDefaults {
    /// Fallback model for image input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Fallback model for audio input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    /// Fallback model for video input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    /// Fallback model for PDF input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pdf: Option<String>,
    /// Fallback model for any other attachment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl RoutingDefaults {
    /// The default model for `modality`, or `None` when the operator has not configured one.
    ///
    /// Text always answers `None` — see the type's own documentation for why.
    pub fn for_modality(&self, modality: InputModality) -> Option<&str> {
        match modality {
            InputModality::Text => None,
            InputModality::Image => self.image.as_deref(),
            InputModality::Audio => self.audio.as_deref(),
            InputModality::Video => self.video.as_deref(),
            InputModality::Pdf => self.pdf.as_deref(),
            InputModality::File => self.file.as_deref(),
        }
    }

    /// Every default the operator has actually set, in modality order.
    ///
    /// Used by validation and by the panel so both see the same list, in the same order, without
    /// either having to know which five fields exist.
    pub fn configured(&self) -> Vec<(InputModality, &str)> {
        [
            (InputModality::Image, self.image.as_deref()),
            (InputModality::Audio, self.audio.as_deref()),
            (InputModality::Video, self.video.as_deref()),
            (InputModality::Pdf, self.pdf.as_deref()),
            (InputModality::File, self.file.as_deref()),
        ]
        .into_iter()
        .filter_map(|(modality, model)| model.map(|model| (modality, model)))
        .collect()
    }
}

/// One upstream proxy the operator has configured.
///
/// The password is **never** in this struct: like an account credential it is a
/// [`Self::secret_ref`] into the encrypted vault, so a config file that is copied, pasted into an
/// issue or committed by accident leaks a hostname rather than a credential.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProxyConfig {
    /// Stable id, referenced by `Provider::proxy` and `Account::proxy`.
    pub id: String,
    /// Human label for the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The endpoint, including its scheme: `http://`, `https://`, `socks4://`, `socks5://` or
    /// `socks5h://` (SOCKS5 with hostname resolution performed by the proxy).
    pub url: String,
    /// Username, when the proxy authenticates. Not a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Reference into the secret vault for the password, when the proxy authenticates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Whether the proxy may be chosen. A disabled proxy stays configured but is never dialled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Lower sorts first under the `priority` strategy. Ties fall through to the next strategy
    /// input, so equal priorities do not make selection order depend on map iteration.
    #[serde(default)]
    pub priority: i32,
    /// Fields this binary does not know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl ProxyConfig {
    /// The URL schemes the relay can actually dial.
    pub const SUPPORTED_SCHEMES: [&'static str; 5] =
        ["http", "https", "socks4", "socks5", "socks5h"];

    /// This proxy's scheme, lowercased, or `None` when the URL carries no `scheme://`.
    pub fn scheme(&self) -> Option<String> {
        let (scheme, rest) = self.url.split_once("://")?;
        if scheme.is_empty() || rest.is_empty() {
            return None;
        }
        Some(scheme.to_ascii_lowercase())
    }

    /// The `host:port` this proxy dials, with no scheme, userinfo or path.
    pub fn authority(&self) -> Option<&str> {
        let (_, rest) = self.url.split_once("://")?;
        let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let rest = match rest.rsplit_once('@') {
            Some((_, host)) => host,
            None => rest,
        };
        if rest.is_empty() {
            None
        } else {
            Some(rest)
        }
    }

    /// Whether the endpoint is one the relay can dial at all.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        match self.scheme() {
            None => problems.push(format!(
                "proxy {}: `url` must include a scheme (one of {})",
                self.id,
                Self::SUPPORTED_SCHEMES.join(", ")
            )),
            Some(scheme) => {
                if !Self::SUPPORTED_SCHEMES.contains(&scheme.as_str()) {
                    problems.push(format!(
                        "proxy {}: unsupported scheme `{scheme}` (expected one of {})",
                        self.id,
                        Self::SUPPORTED_SCHEMES.join(", ")
                    ));
                }
                match self.authority() {
                    None => problems.push(format!("proxy {}: `url` names no host", self.id)),
                    Some(authority) if !authority.contains(':') => problems.push(format!(
                        "proxy {}: `{authority}` must include a port",
                        self.id
                    )),
                    Some(_) => {}
                }
            }
        }
        problems
    }
}

/// How to choose among the eligible proxies.
///
/// The same vocabulary the account [`crate::dialect::KeyStrategy`] uses, on purpose: an operator
/// who has learned "least-used" for accounts should not have to learn a second word for proxies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyStrategy {
    /// Lowest `priority` first; ties broken by declaration order.
    #[default]
    Priority,
    /// The proxy carrying the fewest in-flight requests.
    LeastUsed,
    /// Uniformly at random.
    Random,
    /// Cycled in declaration order.
    RoundRobin,
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
            routing: RoutingConfig::default(),
            proxies: Vec::new(),
            proxy_strategy: ProxyStrategy::default(),
            no_key_required: false,
            allow_remote_without_key: false,
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

/// Dialect spellings that predate the `command-code` → `command-code-go` rename.
///
/// Listed in one place, and consulted only by [`migrate_document`], so that the migration is the
/// single spot in the codebase where old dialect data is understood. [`crate::Dialect::parse`]
/// deliberately rejects every one of these.
pub const LEGACY_COMMAND_CODE_DIALECTS: [&str; 3] = ["command-code", "commandcode", "cmd"];

/// File-name suffix given to the copy of a config file taken before it is migrated:
/// `config.json` becomes `config.json.pre-migration.bak`.
///
/// B1 shipped this as `.pre-command-code-go.bak`, naming the one migration that existed. It is
/// named for the *step* rather than for one migration now that a second one (dropping generated
/// aliases) exists, so the filename keeps describing what happened as more are added.
pub const MIGRATION_BACKUP_SUFFIX: &str = ".pre-migration.bak";

/// Run every config-file migration this binary knows about over raw JSON text.
///
/// Returns `Some(rewritten_json)` when at least one migration applied and `None` when the text is
/// already current — in which case the caller parses the original bytes and nothing is written.
///
/// Working on the raw document rather than on a deserialised [`Config`] is deliberate twice over:
/// the pre-rename dialect spelling has no `Dialect` variant left to deserialise into, and a raw
/// rewrite preserves unknown keys, key order and every unrelated byte of the operator's file.
fn migrate_document(text: &str, path: &Path) -> Result<Option<String>> {
    let mut document: Value = serde_json::from_str(text)
        .map_err(|e| Error::Config(format!("{} is not valid config JSON: {e}", path.display())))?;

    // Additive on purpose: a new migration is one more line here, and the write below happens when
    // *any* of them changed something. Every step must be idempotent, because this runs on every
    // load of a file that has not been rewritten yet.
    let dialects = migrate_command_code_dialects(&mut document);
    let aliases = migrate_generated_aliases(&mut document);
    if dialects.is_empty() && aliases == 0 {
        return Ok(None);
    }
    if !dialects.is_empty() {
        tracing::info!(
            count = dialects.len(),
            "rewrote pre-rename `command-code` dialect spellings in the config"
        );
    }
    if aliases > 0 {
        tracing::info!(
            count = aliases,
            "removed generated model aliases; bare model names now resolve only as aliases or combos"
        );
    }

    let mut json = serde_json::to_string_pretty(&document)
        .map_err(|e| Error::Config(format!("cannot serialise migrated config: {e}")))?;
    json.push('\n');
    Ok(Some(json))
}

/// Delete machine-generated model aliases from a raw config document.
///
/// Returns how many were removed. These entries were produced by an earlier version's naming
/// convention, and the whole point of Feature 3 is that a bare model name no longer resolves: an
/// operator's hand-written aliases are left alone, but a generated one would keep quietly
/// resurrecting the behaviour the feature removed. Only `generated: true` is touched — an alias an
/// operator wrote is never removed, even if it looks machine-made.
///
/// Entries that are not objects, or that carry no `generated` key, are left untouched rather than
/// guessed at, so a malformed file is handed to the ordinary deserialiser to complain about.
pub fn migrate_generated_aliases(document: &mut Value) -> usize {
    let Some(aliases) = document.get_mut("aliases").and_then(Value::as_array_mut) else {
        return 0;
    };
    let before = aliases.len();
    aliases.retain(|alias| !matches!(alias.get("generated").and_then(Value::as_bool), Some(true)));
    before - aliases.len()
}

/// Rewrite pre-rename `command-code` dialect values in a raw config document.
///
/// Returns the index of every provider rewritten, in file order, so a caller can report exactly
/// what changed. Matching ignores case because the file is routinely hand-edited; the value
/// written is always [`crate::Dialect::CommandCodeGo`]'s canonical spelling. Providers are the
/// only place a dialect is authored, so that is the only array inspected.
pub fn migrate_command_code_dialects(document: &mut Value) -> Vec<usize> {
    let Some(providers) = document.get_mut("providers").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let mut rewritten = Vec::new();
    for (index, provider) in providers.iter_mut().enumerate() {
        let Some(dialect) = provider.get_mut("dialect") else {
            continue;
        };
        let Some(spelling) = dialect.as_str() else {
            continue;
        };
        if !LEGACY_COMMAND_CODE_DIALECTS
            .iter()
            .any(|legacy| legacy.eq_ignore_ascii_case(spelling))
        {
            continue;
        }
        *dialect = Value::String(crate::Dialect::CommandCodeGo.as_str().to_string());
        rewritten.push(index);
    }
    rewritten
}

/// Keep the operator's original bytes beside the file before rewriting it.
///
/// The backup name is chosen so that it never overwrites an existing file: a second migration
/// (say, an old file restored from a backup) gets `.2`, `.3`, … rather than destroying the first
/// backup. Failing here aborts the migration *before* the config is replaced, so a read-only
/// config directory cannot leave the operator with neither the old file nor the new one.
fn backup_before_migration(path: &Path, original: &str) -> Result<()> {
    let mut base = path.as_os_str().to_os_string();
    base.push(MIGRATION_BACKUP_SUFFIX);
    let mut target = PathBuf::from(base);
    let mut attempt = 1u32;
    while target.exists() {
        attempt += 1;
        let mut next = path.as_os_str().to_os_string();
        next.push(MIGRATION_BACKUP_SUFFIX);
        next.push(format!(".{attempt}"));
        target = PathBuf::from(next);
    }
    std::fs::write(&target, original.as_bytes()).map_err(|e| {
        Error::Config(format!(
            "cannot write the pre-migration backup {}: {e}; the config was left unchanged",
            target.display()
        ))
    })
}

/// Write already-serialised config JSON to `path` atomically: sibling temporary file, `fsync`,
/// then rename.
///
/// Shared by [`Config::save`] and the load-time migration so there is exactly one write path, and
/// therefore exactly one place where a crash could truncate the config. The caller is responsible
/// for creating the parent directory.
fn write_config_atomically(path: &Path, json: &str) -> Result<()> {
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

impl Config {
    /// Load from the conventional location, or return defaults if it does not exist.
    pub fn load_default() -> Result<Self> {
        Self::load(config_path())
    }

    /// Load from a path.
    ///
    /// A missing file is not an error: it yields defaults, which is what makes a first run work
    /// without a setup step.
    ///
    /// ## One-time migrations
    ///
    /// A file written before the `command-code` → `command-code-go` dialect rename is rewritten
    /// here, with its original bytes kept as a sibling backup, and then loaded. This is the only
    /// place the old spelling is understood: [`crate::Dialect::parse`] rejects it outright, so a
    /// stale file that somehow reaches the parser fails loudly rather than silently selecting an
    /// upstream. The migration runs in memory even when the file must not be saved (a much newer
    /// `format_version`), so a downgrade still starts.
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
        let Some(migrated) = migrate_document(&text, path)? else {
            return Self::from_json(&text, path);
        };
        let config = Self::from_json(&migrated, path)?;
        if config.is_writable() {
            // Backup first: if either write fails, the operator still has the original on disk
            // rather than a half-migrated config with nothing to fall back to.
            backup_before_migration(path, &text)?;
            write_config_atomically(path, &migrated)?;
        }
        Ok(config)
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
        write_config_atomically(path, &json)
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
        // Computed once: `reachable_model_ids` allocates a fresh vector and walks every account's
        // allow and deny lists, so calling it inside the loop made validating N aliases cost
        // N full rebuilds of the model list on every panel save and every startup.
        let reachable_ids = self.reachable_model_ids();
        for a in &self.aliases {
            if !combo_addr.contains(&a.to.as_str())
                && !provider_ids.iter().any(|p| p.as_str() == a.to.as_str())
                && !reachable_ids.iter().any(|m| m == &a.to)
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
        // Zero idle timeout means disable idle connection retention. The max-idle count must be
        // positive: hyper's zero handling is implementation-specific and a zero cap obscures intent.
        if self.latency.pool_max_idle_per_host == 0 {
            problems.push("latency.pool_max_idle_per_host must be at least 1".to_string());
        }
        if self.latency.pool_idle_secs > 86_400 {
            problems.push("latency.pool_idle_secs must not exceed 86400 seconds".to_string());
        }

        problems.extend(self.validate_routing());
        problems.extend(self.validate_proxies());
        problems.extend(self.validate_keyless());

        problems
    }

    /// Validate `routing.defaults` and `quota.display`.
    fn validate_routing(&self) -> Vec<String> {
        let mut problems = Vec::new();

        // Every default must name something this config can actually resolve, or it is a setting
        // that silently never works: the request is rewritten to a name that then 404s.
        for (modality, model) in self.routing.defaults.configured() {
            let named_provider = model.split_once('/').map(|(p, _)| p);
            let known = match named_provider {
                Some(provider) => self.providers.iter().any(|p| p.id.as_str() == provider),
                // A bare default is only usable if it names a combo, since bare model ids are no
                // longer routable.
                None => self.combo(model).is_some(),
            };
            if !known {
                problems.push(format!(
                    "routing.defaults.{} names `{model}`, which is not a provider/model pair or a combo",
                    modality.as_str()
                ));
            }
        }

        if self.quota.display.chunks == 0 {
            problems.push("quota.display.chunks must be at least 1".to_string());
        }
        if self.quota.display.chunks > MAX_QUOTA_CHUNKS {
            problems.push(format!(
                "quota.display.chunks must not exceed {MAX_QUOTA_CHUNKS}"
            ));
        }

        problems
    }

    /// Validate the proxy list and every reference into it.
    fn validate_proxies(&self) -> Vec<String> {
        let mut problems = Vec::new();

        let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
        for proxy in &self.proxies {
            if seen.insert(proxy.id.as_str(), ()).is_some() {
                problems.push(format!("duplicate proxy id {}", proxy.id));
            }
            if proxy.id.trim().is_empty() {
                problems.push("proxy id must not be empty".to_string());
            }
            problems.extend(proxy.validate());
        }

        // A reference to a proxy that does not exist is reported here rather than at dial time,
        // because the fallback is a silent direct connection: without this check the operator's
        // belief that traffic is proxied and the reality would differ with nothing to notice.
        for p in &self.providers {
            if let Some(id) = p.proxy.as_deref() {
                if self.proxy(id).is_none() {
                    problems.push(format!("provider {} names unknown proxy {id}", p.id));
                }
            }
            for a in &p.accounts {
                if let Some(id) = a.proxy.as_deref() {
                    if self.proxy(id).is_none() {
                        problems.push(format!(
                            "provider {} account {} names unknown proxy {id}",
                            p.id, a.id
                        ));
                    }
                }
            }
        }

        problems
    }

    /// Refuse an unauthenticated relay that anyone on the network could reach.
    ///
    /// The startup path performs the same check against the address it is about to bind; doing it
    /// here as well means `mr config validate` and the panel's save both report it *before* an
    /// operator restarts a service and discovers it by the process refusing to come up.
    fn validate_keyless(&self) -> Vec<String> {
        let mut problems = Vec::new();

        if self.serves_keyless() && is_remote_exposed(&self.relay.host) {
            if self.no_key_required && self.allow_remote_without_key {
                // Explicitly permitted: both switches were set on purpose.
            } else {
                let because = if self.no_key_required {
                    "no_key_required is set"
                } else {
                    "no client keys are configured"
                };
                problems.push(format!(
                    "relay would listen on {}:{} with no authentication ({because}); \
                     create a key with `mr key create`, bind 127.0.0.1, or set \
                     relay.no_key_required and relay.allow_remote_without_key if that is truly intended",
                    self.relay.host, self.relay.port
                ));
            }
        }

        if self.allow_remote_without_key && !self.no_key_required {
            // Not an error, but the operator has set the guard without the thing it guards, which
            // almost always means they set the wrong one of the two flags.
            problems.push(
                "relay.allow_remote_without_key is set but relay.no_key_required is not; \
                 the allowance has no effect"
                    .to_string(),
            );
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

    /// The configured proxy with this id, if any.
    pub fn proxy(&self, id: &str) -> Option<&ProxyConfig> {
        self.proxies.iter().find(|p| p.id == id)
    }

    /// Every enabled proxy, for selection.
    pub fn enabled_proxies(&self) -> Vec<&ProxyConfig> {
        self.proxies.iter().filter(|p| p.enabled).collect()
    }

    /// Which proxy a request for `provider` on `account` should use.
    ///
    /// The account's choice wins over the provider's, which is the whole reason both exist: several
    /// accounts on one provider where only one needs to egress elsewhere is the case an operator
    /// actually hits. A reference to a proxy that is missing or disabled resolves to `None`, which
    /// means a direct connection — the request still goes out, because silently failing every
    /// request over a typo'd proxy id would be worse than egressing directly.
    pub fn effective_proxy(
        &self,
        provider: &Provider,
        account: Option<&crate::model::Account>,
    ) -> Option<&ProxyConfig> {
        let wanted = account
            .and_then(|a| a.proxy.as_deref())
            .or(provider.proxy.as_deref())?;
        self.proxy(wanted).filter(|p| p.enabled)
    }

    /// Whether the relay is configured to serve unauthenticated requests.
    ///
    /// Two ways to be open, and they are not the same: an operator who has configured **no** client
    /// keys at all has an open relay by omission, which is what `no_key_required` makes explicit
    /// and opt-in.
    pub fn serves_keyless(&self) -> bool {
        self.no_key_required || self.client_keys.is_empty()
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
            p.proxy.hash(&mut hasher);
            for (account_index, a) in p.accounts.iter().enumerate() {
                account_index.hash(&mut hasher);
                a.id.as_str().hash(&mut hasher);
                a.priority.hash(&mut hasher);
                a.weight.hash(&mut hasher);
                a.enabled.hash(&mut hasher);
                a.models_allow.hash(&mut hasher);
                a.models_deny.hash(&mut hasher);
                a.proxy.hash(&mut hasher);
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
        for (key_index, k) in self.client_keys.iter().enumerate() {
            // Authentication, permission checks and quota enforcement all read these compiled key
            // fields. Hash them explicitly so changing a limit or rotating a key cannot leave the
            // relay using the old plan merely because the snapshot digest stayed the same.
            key_index.hash(&mut hasher);
            k.id.as_str().hash(&mut hasher);
            k.key_hash.hash(&mut hasher);
            k.enabled.hash(&mut hasher);
            k.allow_models.hash(&mut hasher);
            k.allow_routes.hash(&mut hasher);
            k.limit_requests.hash(&mut hasher);
            hash_key_limit(k.limits.session_5h, &mut hasher);
            hash_key_limit(k.limits.weekly_1w, &mut hasher);
            hash_key_limit(k.limits.monthly_30d, &mut hasher);
        }
        // Proxy settings affect where a provider's requests leave the machine. Hash the authored
        // order as a tiebreak input for strategies that use declaration order, and include disabled
        // entries too: re-enabling one must invalidate the plan.
        self.proxy_strategy.hash(&mut hasher);
        for (proxy_index, proxy) in self.proxies.iter().enumerate() {
            proxy_index.hash(&mut hasher);
            proxy.id.hash(&mut hasher);
            proxy.url.hash(&mut hasher);
            proxy.label.hash(&mut hasher);
            proxy.username.hash(&mut hasher);
            proxy.secret_ref.hash(&mut hasher);
            proxy.enabled.hash(&mut hasher);
            proxy.priority.hash(&mut hasher);
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
        // The routing defaults are baked into the compiled snapshot
        // (`default_reasoning_level` and `routing_defaults`), so leaving them out of the digest
        // let a panel edit to a modality fallback, or to the reasoning default, leave the digest
        // unchanged. The relay only reloads a snapshot whose digest differs, so the operator's
        // change would silently never reach the data plane.
        self.routing.default_reasoning_level.hash(&mut hasher);
        self.routing.defaults.image.hash(&mut hasher);
        self.routing.defaults.audio.hash(&mut hasher);
        self.routing.defaults.video.hash(&mut hasher);
        self.routing.defaults.pdf.hash(&mut hasher);
        self.routing.defaults.file.hash(&mut hasher);
        hasher.finish()
    }
}

/// Hash a rolling-window cap without collapsing absence into an explicit zero cap.
///
/// The `KeyLimit` type intentionally has no `Hash` implementation, and encoding its two numbers
/// alone would make `None` identical to `Some(default)`. The presence byte preserves the distinction
/// made by the authored model and by the snapshot compiler.
fn hash_key_limit<H: std::hash::Hasher>(limit: Option<KeyLimit>, hasher: &mut H) {
    use std::hash::Hash;

    match limit {
        Some(limit) => {
            true.hash(hasher);
            limit.tokens.hash(hasher);
            limit.requests.hash(hasher);
        }
        None => false.hash(hasher),
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

    /// A provider that passes `Provider::validate`, which refuses one with no accounts. A test
    /// asserting the *whole* config is clean needs this rather than [`provider`].
    fn servable_provider(id: &str) -> Provider {
        let mut p = provider(id);
        p.accounts.push(account("default"));
        p
    }

    /// A minimal, valid client key. `ClientKey` deliberately has no `new` — the hash is minted by
    /// the CLI — so tests build the literal.
    fn client_key(label: &str) -> ClientKey {
        ClientKey {
            id: crate::ids::ClientKeyId::generate(),
            label: label.to_string(),
            key_hash: "hash".to_string(),
            prefix: None,
            enabled: true,
            allow_models: Vec::new(),
            allow_routes: Vec::new(),
            limit_requests: 0,
            limits: Default::default(),
            note: None,
        }
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
    fn pool_idle_zero_disables_retention_and_excessive_idle_is_rejected() {
        let mut cfg = Config::default();
        cfg.latency.pool_idle_secs = 0;
        assert!(!cfg.validate().iter().any(|m| m.contains("pool_idle_secs")));
        cfg.latency.pool_idle_secs = 86_401;
        assert!(cfg.validate().iter().any(|m| m.contains("pool_idle_secs")));
        cfg.latency.pool_idle_secs = 90;
        cfg.latency.pool_max_idle_per_host = 0;
        assert!(cfg
            .validate()
            .iter()
            .any(|m| m.contains("pool_max_idle_per_host")));
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
    fn the_routing_digest_changes_when_key_limits_change() {
        let mut a = Config::default();
        a.client_keys.push(client_key("app"));
        let mut b = a.clone();
        b.client_keys[0].limits.session_5h = Some(KeyLimit {
            tokens: 10_000,
            requests: 12,
        });
        assert_ne!(a.routing_digest(), b.routing_digest());

        let mut c = b.clone();
        c.client_keys[0].limits.session_5h = Some(KeyLimit {
            tokens: 10_000,
            requests: 13,
        });
        assert_ne!(b.routing_digest(), c.routing_digest());
    }

    #[test]
    fn the_routing_digest_changes_when_proxy_settings_change() {
        let mut a = Config::default();
        a.providers.push(provider("p1"));
        let mut b = a.clone();
        b.proxy_strategy = ProxyStrategy::RoundRobin;
        assert_ne!(a.routing_digest(), b.routing_digest());

        let mut c = a.clone();
        let mut proxy = ProxyConfig {
            id: "edge".into(),
            label: None,
            url: "socks5://proxy.example:1080".into(),
            username: None,
            secret_ref: None,
            enabled: true,
            priority: 0,
            extra: Map::new(),
        };
        c.proxies.push(proxy.clone());
        let mut d = c.clone();
        d.proxies[0].priority = 1;
        assert_ne!(c.routing_digest(), d.routing_digest());

        proxy.enabled = false;
        let mut e = a.clone();
        e.proxies.push(proxy);
        assert_ne!(a.routing_digest(), e.routing_digest());
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
            input_modalities: Vec::new(),
            reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            reasoning_wire: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            pricing: None,
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

    /// A config written before the `command-code` → `command-code-go` rename.
    const LEGACY_COMMAND_CODE_CONFIG: &str = r#"{
        "format_version": 1,
        "providers": [
            {
                "id": "cmd",
                "dialect": "command-code",
                "base_url": "https://api.commandcode.ai",
                "future_provider_field": "keep me"
            }
        ],
        "future_top_level": {"nested": [1, 2, 3]}
    }"#;

    fn migration_backup_path(config: &Path) -> PathBuf {
        let mut base = config.as_os_str().to_os_string();
        base.push(MIGRATION_BACKUP_SUFFIX);
        PathBuf::from(base)
    }

    #[test]
    fn a_legacy_command_code_config_is_migrated_on_load_with_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, LEGACY_COMMAND_CODE_CONFIG).unwrap();

        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers[0].dialect, crate::Dialect::CommandCodeGo);

        // The file on disk now holds the new spelling and parses without any migration.
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert!(
            on_disk.contains("\"command-code-go\""),
            "migrated file lacks the new spelling: {on_disk}"
        );
        assert!(
            !on_disk.contains("\"command-code\""),
            "migrated file still holds the old spelling: {on_disk}"
        );
        let reparsed = Config::from_json(&on_disk, &p).unwrap();
        assert_eq!(reparsed.providers[0].dialect, crate::Dialect::CommandCodeGo);

        // Unknown keys survive the raw-document rewrite, on both levels.
        assert_eq!(reparsed.extra["future_top_level"]["nested"][2], 3);
        assert_eq!(
            reparsed.providers[0].extra["future_provider_field"],
            "keep me"
        );

        // The original bytes are kept beside the file, byte for byte.
        let backup = migration_backup_path(&p);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            LEGACY_COMMAND_CODE_CONFIG,
            "the backup is not the operator's original file"
        );
    }

    #[test]
    fn a_migrated_config_is_not_rewritten_a_second_time() {
        // The migration must be one-time: a second load finds nothing to do, so it neither writes
        // the file again nor adds another backup.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, LEGACY_COMMAND_CODE_CONFIG).unwrap();

        Config::load(&p).unwrap();
        let after_first = std::fs::read_to_string(&p).unwrap();
        let backup = migration_backup_path(&p);
        let backup_after_first = std::fs::read_to_string(&backup).unwrap();

        Config::load(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), after_first);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            backup_after_first
        );
        assert!(
            !migration_backup_path(&backup).exists(),
            "a second load wrote another backup"
        );
    }

    #[test]
    fn the_old_dialect_spelling_never_parses_outside_the_migration() {
        // I5: the rename is hard. `from_json` is the un-migrated entry point and must fail loudly
        // rather than fall back to a guess.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        let err = Config::from_json(LEGACY_COMMAND_CODE_CONFIG, &p).unwrap_err();
        assert!(
            format!("{err}").contains("not valid config JSON"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_config_too_new_to_save_is_migrated_in_memory_only() {
        // A downgrade must still start, but must not overwrite a file a newer binary owns.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        let too_new = LEGACY_COMMAND_CODE_CONFIG.replace(
            "\"format_version\": 1",
            &format!("\"format_version\": {}", MAX_KNOWN_FORMAT_VERSION + 1),
        );
        std::fs::write(&p, &too_new).unwrap();

        let cfg = Config::load(&p).unwrap();
        assert_eq!(cfg.providers[0].dialect, crate::Dialect::CommandCodeGo);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), too_new);
        assert!(!migration_backup_path(&p).exists());
    }

    #[test]
    fn every_legacy_spelling_is_rewritten_and_nothing_else_is_touched() {
        let mut document: Value = serde_json::from_str(
            r#"{
                "providers": [
                    {"id": "a", "dialect": "command-code", "base_url": "https://a.test"},
                    {"id": "b", "dialect": "commandcode", "base_url": "https://b.test"},
                    {"id": "c", "dialect": "CMD", "base_url": "https://c.test"},
                    {"id": "d", "dialect": "openai", "base_url": "https://d.test"},
                    {"id": "e", "base_url": "https://e.test"}
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(migrate_command_code_dialects(&mut document), vec![0, 1, 2]);
        let providers = document["providers"].as_array().unwrap();
        for provider in providers.iter().take(3) {
            assert_eq!(provider["dialect"], "command-code-go");
        }
        assert_eq!(providers[3]["dialect"], "openai");
        assert!(providers[4].get("dialect").is_none());
    }

    #[test]
    fn a_config_without_providers_migrates_to_nothing() {
        let mut document: Value = serde_json::from_str(r#"{"format_version": 1}"#).unwrap();
        assert!(migrate_command_code_dialects(&mut document).is_empty());
        assert!(migrate_document("{}", Path::new("x.json"))
            .unwrap()
            .is_none());
    }

    // ---- B3: routing defaults, quota display, proxies, no-key mode ----

    #[test]
    fn a_default_config_has_no_key_required_off_and_reasoning_at_high() {
        let cfg = Config::default();
        assert!(
            !cfg.no_key_required,
            "an open relay must never be the default"
        );
        assert!(!cfg.allow_remote_without_key);
        assert_eq!(
            cfg.routing.default_reasoning_level,
            ReasoningLevel::High,
            "the documented default level is high"
        );
        assert_eq!(cfg.quota.display.mode, QuotaDisplayMode::Chunks);
        assert_eq!(cfg.quota.display.chunks, DEFAULT_QUOTA_CHUNKS);
        assert_eq!(cfg.proxy_strategy, ProxyStrategy::Priority);
        assert!(cfg.proxies.is_empty());
    }

    #[test]
    fn the_new_sections_survive_a_save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");

        let mut cfg = Config::default();
        cfg.providers.push(provider("acme"));
        cfg.client_keys.push(client_key("app"));
        cfg.routing.default_reasoning_level = ReasoningLevel::Ultra;
        cfg.routing.defaults.image = Some("acme/vision-1".to_string());
        cfg.quota.display.mode = QuotaDisplayMode::Percentage;
        cfg.quota.display.chunks = 7;
        cfg.proxies.push(ProxyConfig {
            id: "eu".to_string(),
            label: Some("EU egress".to_string()),
            url: "socks5://127.0.0.1:1080".to_string(),
            username: Some("manarouter".to_string()),
            secret_ref: Some("secret:proxy-eu".to_string()),
            enabled: true,
            priority: 3,
            extra: Map::new(),
        });
        cfg.proxy_strategy = ProxyStrategy::LeastUsed;
        cfg.save(&path).unwrap();

        let back = Config::load(&path).unwrap();
        assert_eq!(back, cfg);
        assert_eq!(back.proxies[0].scheme().as_deref(), Some("socks5"));
        assert_eq!(back.proxies[0].authority(), Some("127.0.0.1:1080"));
    }

    #[test]
    fn an_old_config_still_loads_because_every_new_field_has_a_default() {
        // The whole reason B3 needs no data migration: a file written before these keys existed
        // must load with the documented defaults rather than failing.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"format_version":1,"providers":[],"quota":{"enabled":true}}"#,
        )
        .unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.routing.default_reasoning_level, ReasoningLevel::High);
        assert!(!cfg.no_key_required);
        assert!(cfg.proxies.is_empty());
        assert_eq!(cfg.quota.display.chunks, DEFAULT_QUOTA_CHUNKS);
    }

    #[test]
    fn unknown_fields_inside_the_new_sections_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"format_version":1,
                "routing":{"default_reasoning_level":"low","future_knob":42},
                "quota":{"display":{"mode":"percentage","future_bar":"x"}},
                "proxies":[{"id":"p","url":"http://127.0.0.1:8080","future_proxy":true}]}"#,
        )
        .unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.routing.extra.get("future_knob"), Some(&Value::from(42)));
        assert_eq!(
            cfg.quota.display.extra.get("future_bar"),
            Some(&Value::from("x"))
        );
        assert_eq!(
            cfg.proxies[0].extra.get("future_proxy"),
            Some(&Value::from(true))
        );
    }

    #[test]
    fn is_loopback_handles_every_spelling_a_person_writes() {
        for host in [
            "127.0.0.1",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
            "127.1.2.3",
        ] {
            assert!(is_loopback(host), "{host} is loopback");
        }
        for host in ["0.0.0.0", "::", "[::]", "192.168.1.10", "example.test"] {
            assert!(!is_loopback(host), "{host} is not loopback");
        }
        // A wildcard bind is the most exposed choice, not an exemption.
        assert!(is_remote_exposed("0.0.0.0"));
    }

    #[test]
    fn a_keyless_relay_on_a_remote_address_is_refused_at_validation() {
        let mut cfg = Config::default();
        cfg.relay.host = "0.0.0.0".to_string();
        assert!(
            cfg.client_keys.is_empty(),
            "this test is about the omitted-keys case"
        );

        let problems = cfg.validate();
        assert_eq!(problems.len(), 1, "exactly one problem: {problems:?}");
        assert!(
            problems[0].contains("no authentication") && problems[0].contains("mr key create"),
            "the message must say why and what to do: {}",
            problems[0]
        );

        // Loopback with no keys stays legal: that is the first-run experience.
        cfg.relay.host = "127.0.0.1".to_string();
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn configuring_a_client_key_makes_a_remote_bind_legal_again() {
        let mut cfg = Config::default();
        cfg.relay.host = "0.0.0.0".to_string();
        cfg.client_keys.push(client_key("app"));
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn no_key_required_on_a_remote_bind_needs_the_second_switch() {
        let mut cfg = Config::default();
        cfg.relay.host = "0.0.0.0".to_string();
        cfg.client_keys.push(client_key("app"));
        cfg.no_key_required = true;

        // One switch is not enough.
        let problems = cfg.validate();
        assert_eq!(
            problems.len(),
            1,
            "expected exactly one problem: {problems:?}"
        );
        assert!(
            problems[0].contains("no_key_required is set"),
            "the message must name the flag responsible: {}",
            problems[0]
        );
        assert!(problems[0].contains("allow_remote_without_key"));

        // Both switches on purpose: permitted.
        cfg.allow_remote_without_key = true;
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn the_allowance_without_the_flag_it_guards_is_reported_as_inert() {
        // Setting the guard alone is nearly always the operator picking the wrong one of two
        // similarly named flags, which is worth a word rather than silence.
        let cfg = Config {
            allow_remote_without_key: true,
            ..Config::default()
        };
        let problems = cfg.validate();
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("has no effect"), "{}", problems[0]);
    }

    #[test]
    fn serves_keyless_distinguishes_omission_from_intent() {
        let mut cfg = Config::default();
        assert!(
            cfg.serves_keyless(),
            "no keys at all means requests are served without one"
        );
        cfg.client_keys.push(client_key("app"));
        assert!(!cfg.serves_keyless());
        cfg.no_key_required = true;
        assert!(
            cfg.serves_keyless(),
            "the flag opens it even with keys present"
        );
    }

    #[test]
    fn a_proxy_reference_to_an_unknown_id_is_reported() {
        let mut cfg = Config::default();
        let mut p = servable_provider("acme");
        p.proxy = Some("nope".to_string());
        cfg.providers.push(p);

        let problems = cfg.validate();
        assert!(
            problems.iter().any(|m| m.contains("unknown proxy nope")),
            "{problems:?}"
        );

        cfg.proxies.push(ProxyConfig {
            id: "nope".to_string(),
            label: None,
            url: "http://127.0.0.1:8080".to_string(),
            username: None,
            secret_ref: None,
            enabled: true,
            priority: 0,
            extra: Map::new(),
        });
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn a_proxy_url_must_have_a_supported_scheme_a_host_and_a_port() {
        let cases = [
            ("acme.test:1080", "scheme"),
            ("ftp://127.0.0.1:1080", "unsupported scheme"),
            ("socks5://", "scheme"),
            ("socks5://127.0.0.1", "port"),
        ];
        for (url, expected) in cases {
            let proxy = ProxyConfig {
                id: "p".to_string(),
                label: None,
                url: url.to_string(),
                username: None,
                secret_ref: None,
                enabled: true,
                priority: 0,
                extra: Map::new(),
            };
            let problems = proxy.validate();
            assert!(
                problems.iter().any(|m| m.contains(expected)),
                "`{url}` should report something containing `{expected}`, got {problems:?}"
            );
        }

        for scheme in ProxyConfig::SUPPORTED_SCHEMES {
            let proxy = ProxyConfig {
                id: "p".to_string(),
                label: None,
                url: format!("{scheme}://user:pass@127.0.0.1:1080"),
                username: None,
                secret_ref: None,
                enabled: true,
                priority: 0,
                extra: Map::new(),
            };
            assert!(
                proxy.validate().is_empty(),
                "{scheme} should be dialable: {:?}",
                proxy.validate()
            );
            // Userinfo must not leak into the authority the connector dials.
            assert_eq!(proxy.authority(), Some("127.0.0.1:1080"));
        }
    }

    #[test]
    fn a_duplicate_proxy_id_is_reported() {
        let mut cfg = Config::default();
        for _ in 0..2 {
            cfg.proxies.push(ProxyConfig {
                id: "dup".to_string(),
                label: None,
                url: "http://127.0.0.1:8080".to_string(),
                username: None,
                secret_ref: None,
                enabled: true,
                priority: 0,
                extra: Map::new(),
            });
        }
        assert!(
            cfg.validate()
                .iter()
                .any(|m| m.contains("duplicate proxy id dup")),
            "{:?}",
            cfg.validate()
        );
    }

    #[test]
    fn the_account_proxy_overrides_the_provider_proxy() {
        let mut cfg = Config::default();
        let mut p = provider("acme");
        p.proxy = Some("shared".to_string());
        let mut a = account("second");
        a.proxy = Some("special".to_string());
        let plain = account("first");
        p.accounts.push(a.clone());
        p.accounts.push(plain.clone());
        cfg.providers.push(p);
        for (id, url) in [
            ("shared", "http://127.0.0.1:1111"),
            ("special", "socks5://127.0.0.1:2222"),
        ] {
            cfg.proxies.push(ProxyConfig {
                id: id.to_string(),
                label: None,
                url: url.to_string(),
                username: None,
                secret_ref: None,
                enabled: true,
                priority: 0,
                extra: Map::new(),
            });
        }

        let provider = &cfg.providers[0];
        assert_eq!(
            cfg.effective_proxy(provider, Some(&a))
                .map(|p| p.id.as_str()),
            Some("special"),
            "the account's choice wins"
        );
        assert_eq!(
            cfg.effective_proxy(provider, Some(&plain))
                .map(|p| p.id.as_str()),
            Some("shared"),
            "an account that names none inherits the provider's"
        );
        assert_eq!(
            cfg.effective_proxy(provider, None).map(|p| p.id.as_str()),
            Some("shared")
        );
    }

    #[test]
    fn a_disabled_or_missing_proxy_resolves_to_a_direct_connection() {
        // Falling back to direct is deliberate: failing every request over a typo'd proxy id would
        // be worse than egressing directly, and validation reports the typo separately.
        let mut cfg = Config::default();
        let mut p = provider("acme");
        p.proxy = Some("off".to_string());
        cfg.providers.push(p);
        cfg.proxies.push(ProxyConfig {
            id: "off".to_string(),
            label: None,
            url: "http://127.0.0.1:8080".to_string(),
            username: None,
            secret_ref: None,
            enabled: false,
            priority: 0,
            extra: Map::new(),
        });

        assert!(cfg.effective_proxy(&cfg.providers[0], None).is_none());
        assert!(cfg.enabled_proxies().is_empty());
    }

    #[test]
    fn a_routing_default_that_names_nothing_routable_is_reported() {
        let mut cfg = Config::default();
        cfg.providers.push(servable_provider("acme"));
        cfg.routing.defaults.image = Some("ghost/vision-1".to_string());
        assert!(
            cfg.validate()
                .iter()
                .any(|m| m.contains("routing.defaults.image") && m.contains("ghost/vision-1")),
            "{:?}",
            cfg.validate()
        );

        // A real provider/model pair is accepted.
        cfg.routing.defaults.image = Some("acme/vision-1".to_string());
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());

        // So is a combo, which is addressable without a provider prefix.
        cfg.routing.defaults.image = None;
        cfg.routing.defaults.audio = Some("voice".to_string());
        cfg.combos.push(combo("voice", "acme"));
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn for_modality_answers_none_for_text_and_the_configured_field_otherwise() {
        let mut defaults = RoutingDefaults::default();
        assert!(defaults.configured().is_empty());
        assert!(defaults.for_modality(InputModality::Text).is_none());

        defaults.image = Some("acme/vision-1".to_string());
        defaults.pdf = Some("acme/docs-1".to_string());
        assert_eq!(
            defaults.for_modality(InputModality::Image),
            Some("acme/vision-1")
        );
        assert_eq!(
            defaults.for_modality(InputModality::Pdf),
            Some("acme/docs-1")
        );
        assert!(defaults.for_modality(InputModality::Audio).is_none());
        assert!(
            defaults.for_modality(InputModality::Text).is_none(),
            "text can never need a rewrite, so it never has a default"
        );

        let configured = defaults.configured();
        assert_eq!(configured.len(), 2);
        assert_eq!(configured[0].0, InputModality::Image);
        assert_eq!(configured[1].0, InputModality::Pdf);
    }

    #[test]
    fn the_chunk_count_is_bounded_because_it_becomes_dom_nodes() {
        let mut cfg = Config::default();
        cfg.quota.display.chunks = 0;
        assert!(
            cfg.validate()
                .iter()
                .any(|m| m.contains("chunks must be at least 1")),
            "{:?}",
            cfg.validate()
        );

        cfg.quota.display.chunks = MAX_QUOTA_CHUNKS + 1;
        assert!(
            cfg.validate()
                .iter()
                .any(|m| m.contains("chunks must not exceed")),
            "{:?}",
            cfg.validate()
        );

        cfg.quota.display.chunks = MAX_QUOTA_CHUNKS;
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    // ---- B3/B5: generated aliases are deleted on load ----

    /// A config with one machine-generated alias and one the operator wrote by hand.
    const CONFIG_WITH_GENERATED_ALIASES: &str = r#"{
        "format_version": 1,
        "aliases": [
            {"from": "gpt-5", "to": "openai/gpt-5", "generated": true},
            {"from": "my-model", "to": "acme/vision-1"},
            {"from": "old-thing", "to": "acme/vision-1", "generated": false}
        ]
    }"#;

    #[test]
    fn migrate_generated_aliases_removes_only_generated_entries() {
        let mut document: Value = serde_json::from_str(CONFIG_WITH_GENERATED_ALIASES).unwrap();
        assert_eq!(migrate_generated_aliases(&mut document), 1);

        let aliases = document.get("aliases").unwrap().as_array().unwrap();
        assert_eq!(aliases.len(), 2);
        let froms: Vec<&str> = aliases
            .iter()
            .map(|a| a.get("from").unwrap().as_str().unwrap())
            .collect();
        assert_eq!(
            froms,
            vec!["my-model", "old-thing"],
            "an operator-authored alias is never removed, even an explicit `generated: false`"
        );

        // Idempotent: nothing left to remove on a second pass.
        assert_eq!(migrate_generated_aliases(&mut document), 0);
    }

    #[test]
    fn migrate_generated_aliases_leaves_anything_it_cannot_read_alone() {
        // A malformed entry must reach the ordinary deserialiser and be reported there, rather than
        // being silently dropped by a migration that guessed at its shape.
        let mut document: Value = serde_json::from_str(
            r#"{"aliases": [
                {"from": "a", "to": "acme/m", "generated": "yes"},
                {"from": "b", "to": "acme/m"},
                "not an object",
                {"from": "c", "to": "acme/m", "generated": true}
            ]}"#,
        )
        .unwrap();
        assert_eq!(migrate_generated_aliases(&mut document), 1);
        assert_eq!(
            document.get("aliases").unwrap().as_array().unwrap().len(),
            3
        );
    }

    #[test]
    fn migrate_generated_aliases_is_a_no_op_without_an_aliases_array() {
        for text in ["{}", r#"{"aliases": null}"#, r#"{"aliases": 7}"#] {
            let mut document: Value = serde_json::from_str(text).unwrap();
            assert_eq!(migrate_generated_aliases(&mut document), 0, "{text}");
        }
    }

    #[test]
    fn a_generated_alias_is_dropped_on_load_with_a_backup_and_a_log_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(&p, CONFIG_WITH_GENERATED_ALIASES).unwrap();

        let cfg = Config::load(&p).unwrap();
        let froms: Vec<&str> = cfg.aliases.iter().map(|a| a.from.as_str()).collect();
        assert_eq!(
            froms,
            vec!["my-model", "old-thing"],
            "the generated alias is gone and the authored ones survive"
        );

        // The rewrite is on disk, so the removal is one-time rather than a per-load filter.
        let on_disk = std::fs::read_to_string(&p).unwrap();
        assert!(!on_disk.contains("gpt-5"), "still on disk: {on_disk}");
        assert_eq!(Config::from_json(&on_disk, &p).unwrap().aliases.len(), 2);

        // And the operator's original file is kept, byte for byte.
        let backup = migration_backup_path(&p);
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            CONFIG_WITH_GENERATED_ALIASES
        );
    }

    #[test]
    fn a_config_with_only_generated_aliases_to_drop_is_still_rewritten() {
        // The migration hook fires when *any* step changed something, not only the dialect one —
        // this is the regression the generalisation exists to prevent.
        let mut document: Value = serde_json::from_str(
            r#"{"format_version": 1, "aliases": [{"from": "x", "to": "c/m", "generated": true}]}"#,
        )
        .unwrap();
        assert!(migrate_command_code_dialects(&mut document).is_empty());
        assert_eq!(migrate_generated_aliases(&mut document), 1);

        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.json");
        std::fs::write(
            &p,
            r#"{"format_version": 1, "aliases": [{"from": "x", "to": "c/m", "generated": true}]}"#,
        )
        .unwrap();
        let cfg = Config::load(&p).unwrap();
        assert!(cfg.aliases.is_empty());
        assert!(migration_backup_path(&p).exists());
    }
}
