//! The domain model: the entities the operator edits in the panel and the control plane compiles
//! into a routing snapshot.
//!
//! These types are the *authored* form. They are rich, self-describing and JSON/TOML friendly,
//! because they are written by hand and by the panel. The relay never sees them: the compiler in
//! [`crate::snapshot`] flattens them into a layout that can be read out of shared memory without a
//! parser.
//!
//! Keeping the two representations separate is the whole reason combo logic can be arbitrarily
//! clever without costing anything at request time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::dialect::{
    AccountState, AuthKind, Capability, ComboStrategy, Dialect, KeyStrategy, MidstreamPolicy,
    ProviderHealth, RouteKind, WindowKind, WinnerFailMode, WinnerMetric,
};
use crate::ids::{fnv1a64, AccountId, ComboId, Millis, OAuthCredentialId, ProviderId};

/// One selectable model on a provider.
///
/// Models come from two sources and both are first class: discovered from the provider's
/// `GET /models`, or entered by hand. Hand-entered metadata always wins over discovered metadata,
/// because a discovery response knows nothing about context length or which routes a model
/// actually serves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelEntry {
    /// The identifier the upstream expects in the `model` field.
    pub id: String,
    /// Where this entry came from.
    #[serde(default)]
    pub source: ModelSource,
    /// Human label shown in the panel. Falls back to `id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Context window in tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Maximum output tokens, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Routes this specific model can serve, when narrower than the provider's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<Capability>,
    /// Model-specific input price per million tokens, for the panel's cost column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_mtok: Option<f64>,
    /// Model-specific output price per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_mtok: Option<f64>,
    /// When this entry was last reconciled with the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<Millis>,
    /// Whether the operator has hidden it from pickers without deleting it.
    #[serde(default)]
    pub hidden: bool,
}

/// Where a [`ModelEntry`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelSource {
    /// Discovered from the provider's `/models` endpoint.
    #[default]
    Synced,
    /// Entered by the operator. Never overwritten by a sync.
    Manual,
    /// Synthesized by the relay because a provider does not expose a model list.
    Inferred,
}

impl ModelSource {
    /// Whether a discovery run is allowed to overwrite fields on this entry.
    ///
    /// Manual entries are authoritative: an operator who typed a context length for a model the
    /// provider omits did so deliberately, and a sync silently reverting it would be a bug.
    #[inline]
    pub const fn is_overwritable_by_sync(self) -> bool {
        matches!(self, Self::Synced | Self::Inferred)
    }
}

/// One credential belonging to a provider: a single API key, or a single OAuth account.
///
/// Accounts are the unit of quota, rate limiting, health and rotation. A provider with six Codex
/// accounts is six accounts; the provider is only unusable when all six are.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    /// Stable identifier. Never regenerated, so a re-run migration stays idempotent.
    pub id: AccountId,
    /// Human label, for example the account email or `key-3`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Email or login associated with the credential, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Reference into the secret vault. The secret itself is **never** in the config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    /// Reference to a stored OAuth credential set, for OAuth accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_ref: Option<OAuthCredentialId>,
    /// Lower is tried first under [`KeyStrategy::Priority`].
    #[serde(default)]
    pub priority: i32,
    /// Relative weight, reserved for weighted selection. Zero means unweighted.
    #[serde(default)]
    pub weight: u32,
    /// Whether the operator has this account enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum simultaneous in-flight requests on this account. Zero means unlimited.
    #[serde(default)]
    pub max_concurrent: u32,
    /// If non-empty, only these model ids may use this account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_allow: Vec<String>,
    /// If non-empty, these model ids may never use this account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_deny: Vec<String>,
    /// Per-account quota windows, when they differ from the provider's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<QuotaWindow>,
    /// Free-form provider-specific fields, for example a Copilot session or a Codex workspace id.
    ///
    /// Kept as an open map so a new provider needs no schema change, and preserved on round-trip
    /// so an older instance never drops a newer instance's fields.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_true() -> bool {
    true
}

impl Account {
    /// A fresh, enabled account with default scheduling.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            id: AccountId::generate(),
            label: Some(label.into()),
            identity: None,
            secret_ref: None,
            oauth_ref: None,
            priority: 0,
            weight: 0,
            enabled: true,
            max_concurrent: 0,
            models_allow: Vec::new(),
            models_deny: Vec::new(),
            quota: Vec::new(),
            extra: BTreeMap::new(),
        }
    }

    /// Whether this account is allowed to serve `model`.
    ///
    /// A deny entry always wins over an allow entry, so an operator can allow `*` and then carve
    /// out an exception without having to enumerate everything else.
    pub fn allows_model(&self, model: &str) -> bool {
        if self.models_deny.iter().any(|m| m == model || m == "*") {
            return false;
        }
        if self.models_allow.is_empty() {
            return true;
        }
        self.models_allow
            .iter()
            .any(|m| m == model || m == "*" || glob_match(m, model))
    }

    /// The live scheduling state implied by configuration alone, before any traffic is seen.
    pub fn configured_state(&self) -> AccountState {
        if self.enabled {
            AccountState::Ready
        } else {
            AccountState::Disabled
        }
    }
}

/// A minimal glob matcher supporting a single trailing `*`.
///
/// Deliberately not a regex: this runs while compiling a snapshot, and a full regex engine would be
/// a large dependency for what is always a prefix match in practice (`claude-*`, `gpt-*`). A
/// mid-string `*` is treated literally.
fn glob_match(pattern: &str, value: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => value.starts_with(prefix),
        None => pattern == value,
    }
}

/// One quota window on an account.
///
/// Windows are the input to reset-aware scheduling: when every window on every account of a step is
/// exhausted, the step is demoted rather than removed, and a timer restores it at the earliest
/// reset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// Operator-facing name, for example `5h` or `monthly`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// How the window's boundaries are determined.
    #[serde(default = "default_window_kind")]
    pub kind: WindowKind,
    /// Window length in seconds.
    pub seconds: i64,
    /// Maximum requests in the window. Zero means unlimited.
    #[serde(default)]
    pub limit_requests: u64,
    /// Maximum total tokens in the window. Zero means unlimited.
    #[serde(default)]
    pub limit_tokens: u64,
    /// Maximum input tokens in the window. Zero means unlimited.
    #[serde(default)]
    pub limit_input_tokens: u64,
    /// Maximum output tokens in the window. Zero means unlimited.
    #[serde(default)]
    pub limit_output_tokens: u64,
    /// Operators frequently know the reset clock of a plan (a 5-hour rolling window, a monthly
    /// calendar) without knowing the limits. Setting this marks the window as
    /// [`WindowKind::ProviderDeclared`] so only the reset timing is used for demotion.
    #[serde(default)]
    pub reset_only: bool,
}

fn default_window_kind() -> WindowKind {
    WindowKind::Rolling
}

impl Default for QuotaWindow {
    fn default() -> Self {
        Self {
            name: None,
            kind: WindowKind::Rolling,
            seconds: 3600,
            limit_requests: 0,
            limit_tokens: 0,
            limit_input_tokens: 0,
            limit_output_tokens: 0,
            reset_only: false,
        }
    }
}

impl QuotaWindow {
    /// A request-count window, the simplest useful configuration.
    pub fn requests_per(seconds: i64, limit: u64) -> Self {
        Self {
            seconds,
            limit_requests: limit,
            ..Self::default()
        }
    }

    /// Whether this window constrains anything at all.
    ///
    /// A window with no limits and no reset tracking is inert, and the compiler drops it rather
    /// than carrying dead weight into the snapshot.
    #[inline]
    pub const fn is_effective(&self) -> bool {
        self.reset_only
            || self.limit_requests > 0
            || self.limit_tokens > 0
            || self.limit_input_tokens > 0
            || self.limit_output_tokens > 0
    }
}

/// A provider: an upstream that speaks one dialect and holds one or more accounts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    /// The operator-chosen slug, used in model names. Lower-case, no spaces.
    pub id: ProviderId,
    /// Human label for the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Which API shape this provider speaks.
    pub dialect: Dialect,
    /// Base URL including any version path, for example `https://openrouter.ai/api/v1`.
    pub base_url: String,
    /// How to attach a credential.
    #[serde(default)]
    pub auth: AuthKind,
    /// Routes this provider can serve.
    #[serde(default = "default_capabilities")]
    pub capabilities: Vec<Capability>,
    /// Accounts. At least one is required for the provider to route anything.
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// How to choose among [`Provider::accounts`].
    #[serde(default)]
    pub key_strategy: KeyStrategy,
    /// Models discovered from `/models`, plus hand-entered ones.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// When the last discovery run completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_synced_at: Option<Millis>,
    /// Provider-wide quota windows, applied to any account that does not override them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quota: Vec<QuotaWindow>,
    /// Whether the operator has this provider enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra headers to attach to every upstream request.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// Extra path segments appended to `base_url` per route, when a provider deviates.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub route_overrides: BTreeMap<String, String>,
    /// How long to wait for the upstream connection to establish.
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// How long to wait for the first response byte before treating the attempt as failed.
    ///
    /// This is deliberately a separate knob from the total timeout: it is the number the hedged
    /// race cares about, and a slow model that is going to answer should not be killed just
    /// because it is thinking.
    #[serde(default = "default_ttfb_timeout_ms")]
    pub ttfb_timeout_ms: u64,
    /// How long the stream may go silent before it is treated as stalled. Zero disables.
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
    /// Total time allowed for the whole request. Zero disables.
    #[serde(default)]
    pub total_timeout_ms: u64,
    /// Whether to keep idle connections warm to this provider.
    #[serde(default = "default_true")]
    pub prewarm: bool,
    /// Whether this provider prefers HTTP/2 when the host supports it.
    #[serde(default = "default_http2")]
    pub http2: Http2Preference,
    /// Operator note, shown in the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Open escape hatch for provider-specific configuration, preserved on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

fn default_capabilities() -> Vec<Capability> {
    vec![Capability::Chat, Capability::Models]
}
fn default_connect_timeout_ms() -> u64 {
    10_000
}
fn default_ttfb_timeout_ms() -> u64 {
    90_000
}
fn default_idle_timeout_ms() -> u64 {
    120_000
}
fn default_http2() -> Http2Preference {
    Http2Preference::Auto
}

/// Whether to negotiate HTTP/2 with an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Http2Preference {
    /// Use HTTP/2 when ALPN offers it, otherwise HTTP/1.1. Cached per host after the first
    /// connection so the hot path never re-negotiates blindly.
    #[default]
    Auto,
    /// Force HTTP/2 with prior knowledge. For providers known to require it.
    Required,
    /// Never use HTTP/2. For providers with broken h2 implementations.
    Disabled,
}

impl Provider {
    /// A minimal provider with sensible defaults, used as the base when the panel creates one.
    pub fn new(id: impl Into<String>, dialect: Dialect, base_url: impl Into<String>) -> Self {
        Self {
            id: ProviderId::from(id.into()),
            label: None,
            dialect,
            base_url: base_url.into(),
            auth: AuthKind::default(),
            capabilities: default_capabilities(),
            accounts: Vec::new(),
            key_strategy: KeyStrategy::default(),
            models: Vec::new(),
            models_synced_at: None,
            quota: Vec::new(),
            enabled: true,
            headers: BTreeMap::new(),
            route_overrides: BTreeMap::new(),
            connect_timeout_ms: default_connect_timeout_ms(),
            ttfb_timeout_ms: default_ttfb_timeout_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
            total_timeout_ms: 0,
            prewarm: true,
            http2: Http2Preference::Auto,
            note: None,
            extra: BTreeMap::new(),
        }
    }

    /// Whether the provider can serve a given client route.
    pub fn supports(&self, route: RouteKind) -> bool {
        let needed = Capability::for_route(route);
        if self.capabilities.contains(&needed) {
            return true;
        }
        // A provider that serves Anthropic Messages also serves count_tokens, and vice versa:
        // they are the same upstream endpoint family and no provider implements one without the
        // other. Encoding that here avoids making the operator declare both.
        needed == Capability::Messages && self.capabilities.contains(&Capability::Messages)
    }

    /// Accounts that are enabled and permitted to serve `model`.
    ///
    /// This is the candidate set the relay selects from. It is computed at compile time and stored
    /// in the snapshot, so the hot path never re-filters by model.
    pub fn candidate_accounts<'a>(&'a self, model: &str) -> Vec<&'a Account> {
        self.accounts
            .iter()
            .filter(|a| a.enabled && a.allows_model(model))
            .collect()
    }

    /// The rolled-up health of this provider given a per-account state lookup.
    ///
    /// Note the shape of this function: it takes the states as an input rather than reading them
    /// from live counters, so the same rollup is used by the panel, the compiler and the tests and
    /// cannot drift.
    pub fn health(&self, state_of: impl Fn(&AccountId) -> AccountState) -> ProviderHealth {
        let total = self.accounts.iter().filter(|a| a.enabled).count();
        let usable = self
            .accounts
            .iter()
            .filter(|a| a.enabled && state_of(&a.id).is_usable())
            .count();
        ProviderHealth::from_counts(total, usable)
    }

    /// The effective quota windows for one account: the account's own if it has any, otherwise the
    /// provider's.
    pub fn effective_windows<'a>(&'a self, account: &'a Account) -> &'a [QuotaWindow] {
        if account.quota.is_empty() {
            &self.quota
        } else {
            &account.quota
        }
    }

    /// All quota windows that will actually constrain anything, across accounts and provider
    /// defaults. Used by the compiler to decide which counters to allocate.
    pub fn effective_window_count(&self) -> usize {
        let provider_level = self.quota.iter().filter(|w| w.is_effective()).count();
        if self.accounts.iter().any(|a| !a.quota.is_empty()) {
            let per_account: usize = self
                .accounts
                .iter()
                .map(|a| a.quota.iter().filter(|w| w.is_effective()).count())
                .sum();
            per_account + provider_level
        } else {
            provider_level
        }
    }

    /// The full model id a client would use to address a model on this provider directly.
    pub fn qualified_model(&self, model: &str) -> String {
        format!("{}/{}", self.id, model)
    }

    /// Validate internal consistency, returning every problem found rather than the first.
    ///
    /// The panel calls this before saving and the compiler calls it before publishing, so an
    /// invalid provider can never reach the relay.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.id.as_str().is_empty() {
            problems.push("provider id must not be empty".into());
        }
        if self.id.as_str().contains('/') || self.id.as_str().contains(char::is_whitespace) {
            problems.push(format!(
                "provider id {:?} must not contain '/' or whitespace, because it is used as a \
                 model-name prefix",
                self.id.as_str()
            ));
        }
        if !self.base_url.starts_with("http://") && !self.base_url.starts_with("https://") {
            problems.push(format!(
                "base_url {:?} must start with http:// or https://",
                self.base_url
            ));
        }
        if self.accounts.is_empty() {
            problems.push("provider has no accounts, so it can never serve a request".into());
        }
        for a in &self.accounts {
            let has_secret = a.secret_ref.is_some() || a.oauth_ref.is_some();
            if !has_secret && !matches!(self.auth, AuthKind::Bearer) && a.enabled {
                // Not fatal: some providers genuinely need no credential, but the operator almost
                // certainly meant to attach one, so it is worth surfacing.
                problems.push(format!(
                    "account {:?} has no credential attached; it will only work if the upstream \
                     needs none",
                    a.label.as_deref().unwrap_or(a.id.as_str())
                ));
            }
            if a.max_concurrent > 0 && a.max_concurrent < 1 {
                problems.push(format!("account {} has an impossible max_concurrent", a.id));
            }
            for w in &a.quota {
                if w.seconds <= 0 {
                    problems.push(format!(
                        "account {} has a quota window with a non-positive length",
                        a.id
                    ));
                }
            }
        }
        if matches!(self.auth, AuthKind::OAuth { .. }) && self.accounts.is_empty() {
            problems.push("OAuth provider has no accounts to log in".into());
        }
        if self.ttfb_timeout_ms == 0 && self.total_timeout_ms == 0 {
            problems.push(
                "both ttfb_timeout_ms and total_timeout_ms are zero, so a hung upstream would \
                 hold a request forever"
                    .into(),
            );
        }
        for m in &self.models {
            if m.id.is_empty() {
                problems.push("a model entry has an empty id".into());
            }
        }
        problems
    }

    /// Which of the client routes this provider claims but cannot actually serve, given its
    /// dialect. Catches the common misconfiguration of attaching an Anthropic base URL to a
    /// provider still declaring OpenAI capabilities.
    pub fn dialect_capability_mismatches(&self) -> Vec<String> {
        let mut out = Vec::new();
        let wants = |c: Capability| self.capabilities.contains(&c);
        match self.dialect {
            Dialect::Anthropic => {
                if wants(Capability::Embeddings) {
                    out.push(
                        "the Anthropic Messages dialect has no embeddings endpoint".to_string(),
                    );
                }
                if wants(Capability::AudioTranscription) || wants(Capability::AudioSpeech) {
                    out.push("the Anthropic Messages dialect has no audio endpoints".to_string());
                }
            }
            Dialect::Codex => {
                for c in [
                    Capability::Embeddings,
                    Capability::AudioTranscription,
                    Capability::AudioSpeech,
                    Capability::ImagesGenerations,
                ] {
                    if wants(c) {
                        out.push(format!(
                            "the Codex dialect only serves the Responses API, not {}",
                            c.as_str()
                        ));
                    }
                }
            }
            Dialect::CommandCode if wants(Capability::Embeddings) => {
                out.push("the Command Code dialect has no embeddings endpoint".to_string());
            }
            _ => {}
        }
        out
    }
}

/// Which steps compete in a hedged race, and how the race resolves.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HedgeSpec {
    /// Whether the race is enabled for this combo.
    #[serde(default)]
    pub enabled: bool,
    /// Explicit indices into the combo's `steps` that enter the competition, in priority order.
    ///
    /// An operator uses this to keep a model in the fallback chain without ever hedging it, which
    /// matters for expensive models and for models with tiny quotas.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub participants: Vec<u16>,
    /// When [`HedgeSpec::participants`] is empty, the first `count` eligible steps compete.
    #[serde(default = "default_hedge_count")]
    pub count: u8,
    /// The eligibility window in milliseconds.
    ///
    /// This is **not** a timeout. It is the window inside which a participant can win on priority;
    /// a participant that replies after it simply loses its priority protection rather than
    /// failing the request.
    #[serde(default = "default_window_ms")]
    pub window_ms: u16,
    /// What counts as having replied.
    #[serde(default)]
    pub winner_metric: WinnerMetric,
    /// What to do when the winner dies.
    #[serde(default)]
    pub on_winner_fail: WinnerFailMode,
    /// Whether the losers are cancelled as soon as the winner is committed.
    ///
    /// On by default and almost always correct: dropping a loser limits local resource use, but does
    /// not guarantee that the provider stops generating or billing. Turning it off is only useful
    /// for debugging the race.
    #[serde(default = "default_true")]
    pub cancel_losers: bool,
    /// Extra grace given to a lower-priority participant before it is abandoned once the winner is
    /// committed. Zero is the right answer for streaming; it exists for providers whose first byte
    /// arrives in bursts.
    #[serde(default)]
    pub loser_grace_ms: u16,
}

fn default_hedge_count() -> u8 {
    3
}
fn default_window_ms() -> u16 {
    800
}

impl Default for HedgeSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            participants: Vec::new(),
            count: default_hedge_count(),
            window_ms: default_window_ms(),
            winner_metric: WinnerMetric::default(),
            on_winner_fail: WinnerFailMode::default(),
            cancel_losers: true,
            loser_grace_ms: 0,
        }
    }
}

impl HedgeSpec {
    /// The participants implied by `count` when no explicit list is given.
    ///
    /// Returns indices into the step list, skipping disabled steps, capped at the number of steps.
    pub fn implicit_participants(&self, step_count: usize) -> Vec<u16> {
        let n = (self.count as usize).min(step_count);
        (0..n).map(|i| i as u16).collect()
    }

    /// The effective participant list, resolving the implicit form.
    pub fn resolved_participants(&self, step_count: usize) -> Vec<u16> {
        if self.participants.is_empty() {
            self.implicit_participants(step_count)
        } else {
            self.participants
                .iter()
                .copied()
                .filter(|i| (*i as usize) < step_count)
                .collect()
        }
    }

    /// Whether this spec can produce a meaningful race: at least two participants.
    ///
    /// A "race" of one is just a normal request with extra bookkeeping, so the compiler downgrades
    /// it rather than paying for the machinery.
    pub fn is_effective(&self, step_count: usize) -> bool {
        self.enabled && self.resolved_participants(step_count).len() >= 2
    }
}

/// One ordered step of a combo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComboStep {
    /// The provider that serves this step.
    pub provider: ProviderId,
    /// The model id as the upstream expects it.
    pub model: String,
    /// Whether the operator has this step enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Relative weight, reserved for future weighted strategies.
    #[serde(default)]
    pub weight: u32,
    /// Free-form operator note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ComboStep {
    /// A new enabled step.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: ProviderId::from(provider.into()),
            model: model.into(),
            enabled: true,
            weight: 0,
            note: None,
        }
    }
}

/// A named route: a custom model name that resolves to an ordered list of steps.
///
/// The combo name is what clients pass in the `model` field, so it lives in the same namespace as
/// real model ids and is matched the same way.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Combo {
    /// Stable identifier.
    pub id: ComboId,
    /// The model name clients call. This is the operator's custom name.
    pub name: String,
    /// Optional display label distinct from the callable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The ordered steps. The operator's order is authoritative and is never silently reordered;
    /// only [`ComboStrategy::ResetAwarePriority`] changes the traversal order at runtime, and then
    /// only by demoting unavailable steps.
    #[serde(default)]
    pub steps: Vec<ComboStep>,
    /// How the steps are traversed.
    #[serde(default)]
    pub strategy: ComboStrategy,
    /// The hedged race configuration.
    #[serde(default)]
    pub hedge: HedgeSpec,
    /// What to do when the committed model dies after bytes have been sent.
    #[serde(default)]
    pub midstream_policy: MidstreamPolicy,
    /// How many times a whole attempt may be retried before the combo gives up.
    #[serde(default = "default_combo_max_retries")]
    pub max_retries: u8,
    /// Delay between retries of the same step.
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
    /// Whether the relay keeps the winning step sticky for a conversation.
    #[serde(default = "default_true")]
    pub affinity: bool,
    /// Optional context length advertised on `GET /v1/models` for this combo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// Whether the combo is hidden from pickers while remaining callable.
    #[serde(default)]
    pub hidden: bool,
    /// Whether the combo is disabled entirely.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Off-path content transformer configuration. Default off.
    #[serde(default)]
    pub compression: CompressionSpec,
    /// Optional system message the relay prepends for this combo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,
    /// Free-form operator note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Sort order in the panel.
    #[serde(default)]
    pub sort_order: i32,
}

fn default_combo_max_retries() -> u8 {
    1
}
fn default_retry_delay_ms() -> u64 {
    2_000
}

impl Combo {
    /// A new combo with one step.
    pub fn new(name: impl Into<String>, first: ComboStep) -> Self {
        Self {
            id: ComboId::generate(),
            name: name.into(),
            label: None,
            steps: vec![first],
            strategy: ComboStrategy::default(),
            hedge: HedgeSpec::default(),
            midstream_policy: MidstreamPolicy::default(),
            max_retries: default_combo_max_retries(),
            retry_delay_ms: default_retry_delay_ms(),
            affinity: true,
            context_length: None,
            hidden: false,
            enabled: true,
            compression: CompressionSpec::default(),
            system_message: None,
            note: None,
            sort_order: 0,
        }
    }

    /// Whether the combo has any step the relay could actually attempt.
    pub fn has_routable_step(&self) -> bool {
        self.enabled && self.steps.iter().any(|s| s.enabled)
    }

    /// The hedged race in effect after downgrades.
    ///
    /// A hedge over fewer than two participants is not a race, so it is reported as disabled
    /// rather than being executed with all the machinery and none of the benefit.
    pub fn effective_hedge(&self) -> HedgeSpec {
        let mut h = self.hedge.clone();
        if !h.is_effective(self.steps.len()) {
            h.enabled = false;
        }
        h
    }

    /// Validate the combo, returning every problem found.
    pub fn validate(&self, known_providers: &[ProviderId]) -> Vec<String> {
        let mut problems = Vec::new();
        if self.name.trim().is_empty() {
            problems.push(
                "combo name must not be empty, because it is the model id clients call".to_string(),
            );
        }
        if self.name.contains(char::is_whitespace) {
            problems.push(format!(
                "combo name {:?} contains whitespace; most clients will not send it as a model id \
                 without quoting",
                self.name
            ));
        }
        if self.steps.is_empty() {
            problems.push("combo has no steps".to_string());
        }
        if !self.steps.iter().any(|s| s.enabled) {
            problems.push("combo has no enabled steps".to_string());
        }
        for (i, s) in self.steps.iter().enumerate() {
            if !known_providers.contains(&s.provider) {
                problems.push(format!(
                    "step {i} references unknown provider {:?}",
                    s.provider.as_str()
                ));
            }
            if s.model.trim().is_empty() {
                problems.push(format!("step {i} has an empty model id"));
            }
        }
        if self.retry_delay_ms > 60_000 {
            problems.push(format!(
                "retry_delay_ms is {}ms, which will look like a hang to most clients",
                self.retry_delay_ms
            ));
        }
        if self.max_retries > 10 {
            problems.push(format!(
                "max_retries is {}, which risks amplifying an upstream outage",
                self.max_retries
            ));
        }
        if let WinnerFailMode::StableFallback { step } = &self.hedge.on_winner_fail {
            if (*step as usize) >= self.steps.len() {
                problems.push(format!(
                    "hedge.on_winner_fail names stable step {step}, but the combo only has {} steps",
                    self.steps.len()
                ));
            } else if !self.steps[*step as usize].enabled {
                problems.push(format!(
                    "hedge.on_winner_fail names stable step {step}, which is disabled"
                ));
            }
        }
        if self.hedge.enabled {
            let participants = self.hedge.resolved_participants(self.steps.len());
            if participants.len() < 2 {
                problems.push(
                    "hedge is enabled but fewer than two participants resolve, so no race would \
                     happen"
                        .to_string(),
                );
            }
            for p in &participants {
                match self.steps.get(*p as usize) {
                    None => problems.push(format!(
                        "hedge participant {p} is out of range (combo has {} steps)",
                        self.steps.len()
                    )),
                    Some(s) if !s.enabled => problems.push(format!(
                        "hedge participant {p} names a disabled step, so it can never win"
                    )),
                    Some(_) => {}
                }
            }
            if self.hedge.window_ms == 0 {
                problems.push(
                    "hedge window_ms is zero, which makes the window an instant and removes the \
                     priority protection it exists to provide"
                        .to_string(),
                );
            }
        }
        problems
    }

    /// A cheap content signature used to detect whether a recompiled plan differs from the last
    /// one. The conductor only republishes the snapshot when this changes, which keeps the relay
    /// from swapping pointers on every unrelated settings write.
    pub fn signature(&self) -> u64 {
        let mut h = fnv1a64(self.name.as_bytes());
        h ^= fnv1a64(&(self.strategy as u8).to_le_bytes());
        h ^= fnv1a64(&(self.hedge.enabled as u8).to_le_bytes());
        h ^= fnv1a64(&self.hedge.window_ms.to_le_bytes());
        h ^= fnv1a64(&self.hedge.count.to_le_bytes());
        h ^= fnv1a64(&(self.midstream_policy as u8).to_le_bytes());
        // `max_retries` is a `u8` while `retry_delay_ms` is a `u64`, so only the latter is a
        // no-op cast. Widening the former is deliberate: encoding it in one byte would make a
        // retry count of 1 indistinguishable from a 1 ms delay in the adjacent term.
        h ^= fnv1a64(&u64::from(self.max_retries).to_le_bytes());
        h ^= fnv1a64(&self.retry_delay_ms.to_le_bytes());
        for s in &self.steps {
            h ^= fnv1a64(s.provider.as_str().as_bytes());
            h ^= fnv1a64(s.model.as_bytes());
            h ^= fnv1a64(&(s.enabled as u8).to_le_bytes());
        }
        for p in &self.hedge.participants {
            h ^= fnv1a64(&p.to_le_bytes());
        }
        h
    }
}

/// Off-path content compression.
///
/// Defaults to off everywhere. It exists because the operator wants the capability available and
/// the groundwork laid, but a content transform is a CPU cost in the request path and is not
/// enabled unless a combo explicitly opts in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompressionSpec {
    /// Whether compression runs for this combo.
    #[serde(default)]
    pub enabled: bool,
    /// Which transformer to apply.
    #[serde(default)]
    pub mode: CompressionMode,
    /// Only compress payloads at least this large, in bytes.
    #[serde(default = "default_min_compress_bytes")]
    pub min_bytes: usize,
    /// Never grow a payload: if the transform would not shrink it, the original is used.
    #[serde(default = "default_true")]
    pub never_grow: bool,
    /// Never produce an empty payload.
    #[serde(default = "default_true")]
    pub never_empty: bool,
}

fn default_min_compress_bytes() -> usize {
    4096
}

impl Default for CompressionSpec {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: CompressionMode::Rtk,
            min_bytes: default_min_compress_bytes(),
            never_grow: true,
            never_empty: true,
        }
    }
}

/// Which compression transformer to apply to tool-result bulk text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionMode {
    /// The RTK tool-output compressor, which was itself originally a Rust project. Only tool-result
    /// bulk text is touched: diffs, logs, build output, grep results and directory listings.
    #[default]
    Rtk,
    /// Compress the system prompt only.
    SystemPrompt,
}

/// A user-defined alias from one model name to another.
///
/// Aliases exist so a client can keep using a short familiar name (`oad/DeepSeek-V4.1-Flash`) while
/// the canonical id drifts, and so the migration from OmniRoute's triple-exposed naming
/// (prefixed, short prefix, bare) is reproducible rather than magic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alias {
    /// The name the client sends.
    pub from: String,
    /// The canonical target: a model id, a qualified model id, or a combo name.
    pub to: String,
    /// Whether this alias was generated from a naming convention rather than authored by hand.
    #[serde(default)]
    pub generated: bool,
}

/// A client-facing API key used to authenticate callers of the relay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientKey {
    /// Stable identifier.
    pub id: crate::ids::ClientKeyId,
    /// Human label, for example `claude-code`.
    pub label: String,
    /// A hash of the key, never the key itself.
    ///
    /// The relay compares hashes, so a leaked config file does not leak a usable credential.
    pub key_hash: String,
    /// A short, non-secret prefix kept for display, for example `mr-3f9a`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Whether the key is usable.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// If non-empty, the key may only call these model ids (globs allowed).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_models: Vec<String>,
    /// If non-empty, the key may only call these routes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_routes: Vec<String>,
    /// Per-key request budget. Zero means unlimited.
    #[serde(default)]
    pub limit_requests: u64,
    /// Descriptive note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ClientKey {
    /// Whether this key may address `model` on `route`.
    pub fn permits(&self, model: &str, route: RouteKind) -> bool {
        let model_ok = self.allow_models.is_empty()
            || self
                .allow_models
                .iter()
                .any(|m| m == "*" || m == model || glob_match(m, model));
        if !model_ok {
            return false;
        }
        self.allow_routes.is_empty()
            || self
                .allow_routes
                .iter()
                .any(|r| r == "*" || r == route.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_with(accounts: usize) -> Provider {
        let mut p = Provider::new("test", Dialect::OpenAi, "https://example.test/v1");
        for i in 0..accounts {
            let mut a = Account::new(format!("key-{i}"));
            a.secret_ref = Some(format!("test/key-{i}"));
            a.priority = i as i32;
            p.accounts.push(a);
        }
        p
    }

    #[test]
    fn provider_is_only_dead_when_every_account_is_dead() {
        let p = provider_with(6);
        let dead_ones = |a: &AccountId| {
            if a.as_str() == p.accounts[0].id.as_str() {
                AccountState::Ready
            } else {
                AccountState::RateLimited
            }
        };
        assert_eq!(p.health(dead_ones), ProviderHealth::Degraded);
        assert!(p.health(dead_ones).is_serviceable());

        let all_dead = |_: &AccountId| AccountState::QuotaExhausted;
        assert_eq!(p.health(all_dead), ProviderHealth::Dead);
        assert!(!p.health(all_dead).is_serviceable());
    }

    #[test]
    fn disabled_accounts_are_excluded_from_health_entirely() {
        let mut p = provider_with(2);
        p.accounts[1].enabled = false;
        // Only one account is configured as far as routing is concerned, and it is ready, so the
        // provider is Healthy rather than Degraded: a deliberately disabled account is not an
        // outage.
        assert_eq!(p.health(|_| AccountState::Ready), ProviderHealth::Healthy);

        // Disabling every account leaves the provider Unconfigured, not Dead. A deliberate operator
        // action is not an outage, and painting the panel red for something working as instructed
        // would train the operator to ignore the indicator.
        p.accounts[0].enabled = false;
        assert_eq!(
            p.health(|_| AccountState::Ready),
            ProviderHealth::Unconfigured
        );
    }

    #[test]
    fn account_model_allow_deny_rules_are_precise() {
        let mut a = Account::new("k");
        assert!(a.allows_model("anything"));
        a.models_allow = vec!["claude-*".into(), "gpt-5".into()];
        assert!(a.allows_model("claude-opus-4-7"));
        assert!(a.allows_model("gpt-5"));
        assert!(!a.allows_model("gpt-6"));
        // Deny wins over allow, so an operator can allow a family and carve out an exception
        // without enumerating everything else.
        a.models_deny = vec!["claude-opus-4-7".into()];
        assert!(!a.allows_model("claude-opus-4-7"));
        assert!(a.allows_model("claude-sonnet-4-6"));

        let mut b = Account::new("k2");
        b.models_allow = vec!["*".into()];
        b.models_deny = vec!["secret-model".into()];
        assert!(b.allows_model("public-model"));
        assert!(!b.allows_model("secret-model"));
    }

    #[test]
    fn glob_match_handles_trailing_star_and_literal_only() {
        assert!(glob_match("claude-*", "claude-x"));
        assert!(glob_match("claude-*", "claude-"));
        assert!(!glob_match("claude-*", "gpt-x"));
        assert!(glob_match("exact", "exact"));
        assert!(!glob_match("exact", "exact2"));
        // A mid-string star is treated literally rather than as a wildcard, so a model id that
        // genuinely contains one is not accidentally over-matched.
        assert!(!glob_match("a*b", "axxxb"));
        assert!(glob_match("a*b", "a*b"));
    }

    #[test]
    fn manual_models_are_never_overwritten_by_sync() {
        assert!(!ModelSource::Manual.is_overwritable_by_sync());
        assert!(ModelSource::Synced.is_overwritable_by_sync());
        assert!(ModelSource::Inferred.is_overwritable_by_sync());
    }

    #[test]
    fn hedge_of_one_participant_is_downgraded_to_no_race() {
        let mut c = Combo::new("solo", ComboStep::new("p", "m1"));
        c.hedge.enabled = true;
        c.hedge.count = 3;
        // Only one step exists, so the "race" would have a single participant: executing the race
        // machinery would cost latency and buy nothing.
        assert!(!c.effective_hedge().enabled);

        c.steps.push(ComboStep::new("p2", "m2"));
        assert!(c.effective_hedge().enabled);
        assert_eq!(c.effective_hedge().resolved_participants(2), vec![0, 1]);
    }

    #[test]
    fn hedge_participants_can_exclude_a_step_from_racing_but_keep_it_as_fallback() {
        let mut c = Combo::new("mixed", ComboStep::new("cheap", "m1"));
        c.steps.push(ComboStep::new("cheap", "m2"));
        c.steps.push(ComboStep::new("expensive", "m3"));
        c.hedge.enabled = true;
        c.hedge.participants = vec![0, 1];
        // The expensive model stays in the ordered fallback chain but never enters a race, which
        // is exactly the control the operator asked for.
        assert_eq!(c.effective_hedge().resolved_participants(3), vec![0, 1]);
        assert!(c.effective_hedge().enabled);
    }

    #[test]
    fn hedge_participants_out_of_range_are_dropped_not_fatal() {
        let mut c = Combo::new("x", ComboStep::new("p", "m"));
        c.hedge.enabled = true;
        c.hedge.participants = vec![0, 99];
        // A stale index after a step is deleted must not panic the compiler; it degrades to a
        // single participant and therefore to no race.
        assert_eq!(c.effective_hedge().resolved_participants(1), vec![0]);
        assert!(!c.effective_hedge().enabled);
    }

    #[test]
    fn stable_fallback_step_is_validated() {
        let mut c = Combo::new("x", ComboStep::new("p", "m"));
        c.steps.push(ComboStep::new("p", "m2"));
        c.hedge.on_winner_fail = WinnerFailMode::StableFallback { step: 5 };
        let problems = c.validate(&[ProviderId::from("p")]);
        assert!(
            problems.iter().any(|p| p.contains("only has 2 steps")),
            "{problems:?}"
        );

        c.hedge.on_winner_fail = WinnerFailMode::StableFallback { step: 1 };
        c.steps[1].enabled = false;
        let problems = c.validate(&[ProviderId::from("p")]);
        assert!(
            problems.iter().any(|p| p.contains("which is disabled")),
            "{problems:?}"
        );
    }

    #[test]
    fn combo_validation_catches_unknown_provider_and_empty_name() {
        let mut c = Combo::new("  ", ComboStep::new("ghost", "m"));
        let problems = c.validate(&[ProviderId::from("real")]);
        assert!(problems.iter().any(|p| p.contains("must not be empty")));
        assert!(problems.iter().any(|p| p.contains("unknown provider")));

        c.name = "has space".into();
        let problems = c.validate(&[ProviderId::from("ghost")]);
        assert!(problems.iter().any(|p| p.contains("whitespace")));
    }

    #[test]
    fn combo_signature_changes_when_routing_changes_but_not_on_unrelated_edits() {
        let c = Combo::new("a", ComboStep::new("p", "m"));
        let base = c.signature();

        let mut note_only = c.clone();
        note_only.note = Some("a comment".into());
        // The conductor must not republish the whole snapshot because someone typed a note.
        assert_eq!(note_only.signature(), base);

        let mut reordered = c.clone();
        reordered.steps[0].enabled = false;
        assert_ne!(reordered.signature(), base);

        let mut window = c.clone();
        window.hedge.window_ms = 1234;
        assert_ne!(window.signature(), base);
    }

    #[test]
    fn provider_validation_reports_every_problem_at_once() {
        let mut p = Provider::new("bad/id", Dialect::OpenAi, "ftp://nope");
        p.ttfb_timeout_ms = 1000;
        let problems = p.validate();
        // Reporting all problems lets the panel show them together instead of making the operator
        // fix one, save, and discover the next.
        assert!(problems.iter().any(|s| s.contains("must not contain '/'")));
        assert!(problems.iter().any(|s| s.contains("must start with http")));
        assert!(problems.iter().any(|s| s.contains("no accounts")));
    }

    #[test]
    fn a_provider_with_no_timeout_at_all_is_rejected() {
        let p = Provider {
            ttfb_timeout_ms: 0,
            total_timeout_ms: 0,
            idle_timeout_ms: 0,
            ..provider_with(1)
        };
        assert!(p
            .validate()
            .iter()
            .any(|s| s.contains("hold a request forever")));
    }

    #[test]
    fn dialect_capability_mismatches_are_flagged() {
        let mut p = Provider::new("a", Dialect::Anthropic, "https://api.anthropic.com/v1");
        p.capabilities = vec![Capability::Messages, Capability::Embeddings];
        assert!(p
            .dialect_capability_mismatches()
            .iter()
            .any(|s| s.contains("no embeddings endpoint")));

        let mut cx = Provider::new("c", Dialect::Codex, "https://chatgpt.com/backend-api/codex");
        cx.capabilities = vec![Capability::Responses, Capability::ImagesGenerations];
        assert!(cx
            .dialect_capability_mismatches()
            .iter()
            .any(|s| s.contains("images.generations")));
    }

    #[test]
    fn messages_capability_also_serves_count_tokens() {
        let mut p = Provider::new("a", Dialect::Anthropic, "https://api.anthropic.com/v1");
        p.capabilities = vec![Capability::Messages];
        assert!(p.supports(RouteKind::Messages));
        assert!(p.supports(RouteKind::CountTokens));
        assert!(!p.supports(RouteKind::ChatCompletions));
    }

    #[test]
    fn candidate_accounts_filter_by_enabled_and_model_permission() {
        let mut p = provider_with(3);
        p.accounts[1].enabled = false;
        p.accounts[2].models_allow = vec!["only-this".into()];
        let candidates = p.candidate_accounts("other-model");
        assert_eq!(candidates.len(), 1);
        let candidates = p.candidate_accounts("only-this");
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn account_quota_overrides_provider_quota_only_when_present() {
        let mut p = provider_with(2);
        p.quota = vec![QuotaWindow::requests_per(3600, 100)];
        assert_eq!(p.effective_windows(&p.accounts[0]).len(), 1);

        p.accounts[1].quota = vec![QuotaWindow::requests_per(60, 5)];
        assert_eq!(p.effective_windows(&p.accounts[1])[0].seconds, 60);
        // The account that declares nothing still inherits the provider default rather than
        // becoming unlimited.
        assert_eq!(p.effective_windows(&p.accounts[0])[0].seconds, 3600);
    }

    #[test]
    fn inert_quota_windows_are_detected() {
        assert!(!QuotaWindow::default().is_effective());
        assert!(QuotaWindow::requests_per(3600, 1).is_effective());
        let reset_only = QuotaWindow {
            reset_only: true,
            ..QuotaWindow::default()
        };
        // A window that only carries reset timing is still useful: it drives demotion even though
        // it constrains no counter.
        assert!(reset_only.is_effective());
    }

    #[test]
    fn client_key_permission_globs_work() {
        let mut k = ClientKey {
            id: crate::ids::ClientKeyId::generate(),
            label: "claude-code".into(),
            key_hash: "h".into(),
            prefix: None,
            enabled: true,
            allow_models: vec!["claude-*".into()],
            allow_routes: vec!["messages".into()],
            limit_requests: 0,
            note: None,
        };
        assert!(k.permits("claude-opus-4-7", RouteKind::Messages));
        assert!(!k.permits("claude-opus-4-7", RouteKind::ChatCompletions));
        assert!(!k.permits("gpt-5", RouteKind::Messages));

        k.allow_routes.clear();
        k.allow_models.clear();
        assert!(k.permits("anything", RouteKind::Embeddings));
    }

    #[test]
    fn config_round_trips_without_losing_unknown_fields() {
        // The v2 sync groundwork depends on an older instance never dropping a newer instance's
        // fields, so an unrecognised key must survive a load/save cycle.
        let json = serde_json::json!({
            "id": "p",
            "dialect": "open-ai",
            "base_url": "https://example.test/v1",
            "a_field_from_the_future": { "nested": [1, 2, 3] }
        });
        let p: Provider = serde_json::from_value(json).unwrap();
        let out = serde_json::to_value(&p).unwrap();
        assert_eq!(
            out.get("a_field_from_the_future"),
            Some(&serde_json::json!({ "nested": [1, 2, 3] })),
            "unknown top-level keys must be preserved"
        );
    }
}
