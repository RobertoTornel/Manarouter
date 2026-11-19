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
    AccountState, AuthKind, Capability, ComboStrategy, Dialect, InputModality, KeyStrategy,
    MidstreamPolicy, ProviderHealth, ReasoningLevel, RouteKind, WindowKind, WinnerFailMode,
    WinnerMetric,
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
    /// Input types this model accepts, when the operator has declared them.
    ///
    /// Empty is the normal case and means *derive*: the compiler seeds it from the bundled
    /// capability table (`catalog/model_meta.json`) and falls back to text-only. An explicit list
    /// is how an operator corrects the seed, and hand-entered metadata always wins over discovered
    /// metadata — the same rule [`ModelEntry::source`] documents.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<InputModality>,
    /// Reasoning tiers this model offers, when the operator has declared them.
    ///
    /// Empty means *derive*, in the same way and from the same table as [`Self::input_modalities`]:
    /// the bundled table first, then the dialect's default set when the provider advertises
    /// reasoning. Ascending order is not required — it is normalised when compiled.
    ///
    /// The canonical vocabulary lives in [`crate::dialect::ReasoningLevel`], including the
    /// deliberate `minimal` tier, so migrated OmniRoute tier data maps faithfully.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_levels: Vec<ReasoningLevel>,
    /// The tier used when a request names the model without a `-<level>` suffix.
    ///
    /// `None` falls through to `routing.default_reasoning_level` in the configuration, which
    /// itself defaults to `high`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_level: Option<ReasoningLevel>,
    /// Per-model override of the dialect's reasoning fragment, keyed by level.
    ///
    /// This is a **wholesale replacement**, not a merge: a dialect's default fragment has to be
    /// internally consistent (`{"thinking":{"type":"enabled","budget_tokens":N}}` cannot be merged
    /// field-by-field with an operator's), so when a level is present here its value is what the
    /// relay puts in the upstream body, and when a level is absent the dialect default is used.
    /// `None` at a level means "send nothing for this level".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_wire: Option<BTreeMap<ReasoningLevel, serde_json::Value>>,
    /// Model-specific input price per million tokens, for the panel's cost column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_price_per_mtok: Option<f64>,
    /// Model-specific output price per million tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_price_per_mtok: Option<f64>,
    /// The full price picture, when known: both directions plus cache rates and where the numbers
    /// came from.
    ///
    /// [`Self::input_price_per_mtok`] and [`Self::output_price_per_mtok`] predate this field and
    /// are kept because they are what the panel's simpler cost column reads. When both are set they
    /// must agree; [`PricingPlan`] is the authoritative record, because it also carries the cache
    /// rates a cost estimate needs and the provenance that decides which source wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<PricingPlan>,
    /// When this entry was last reconciled with the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synced_at: Option<Millis>,
    /// Whether the operator has hidden it from pickers without deleting it.
    #[serde(default)]
    pub hidden: bool,
}

impl ModelEntry {
    /// The level a request gets when it names this model without a `-<level>` suffix.
    ///
    /// Falls through to the configuration's `routing.default_reasoning_level`, which the caller
    /// passes in, so this stays a pure function with no configuration dependency.
    #[inline]
    pub fn default_level(&self, config_default: ReasoningLevel) -> ReasoningLevel {
        self.default_reasoning_level.unwrap_or(config_default)
    }
}

/// The highest level in `levels`, or `None` when the list is empty.
///
/// This is the one substitution the design allows instead of failing a request: asking for a level
/// a model does not offer (`-max` on a model that tops out at `xhigh`) reroutes to that model's
/// highest available tier. Ranking through [`ReasoningLevel::rank`] rather than relying on enum
/// comparison keeps it identical to the `u8` the snapshot stores.
pub fn highest_level(levels: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    levels.iter().copied().max_by_key(|level| level.rank())
}

/// The reasoning levels a model actually offers, ascending.
///
/// This is the derivation the compiler, the panel and the validator share, so "what levels does
/// this model have" has one answer. The precedence is fixed:
///
/// 1. the operator's explicit `reasoning_levels` wins outright, because a hand-entered list is the
///    only source that knows about a provider's non-standard tiers;
/// 2. otherwise the bundled per-model capability table, which is provider catalogue metadata and
///    is more specific than a dialect-wide guess;
/// 3. otherwise the dialect's own list, but **only** when the model or its provider declares the
///    [`Capability::Reasoning`] capability or the dialect serves reasoning models exclusively.
///
/// The result is always intersected with [`Dialect::expressible_reasoning_levels`]: a stale list or
/// a catalogue row naming a tier the dialect lowers or rewrites must not become an offered level,
/// because every offered level is also an accepted `-<level>` name. [`Provider::validate`] reports
/// such an entry as a problem rather than letting this silently clean it up.
pub fn effective_reasoning_levels(
    provider_id: &str,
    dialect: Dialect,
    provider_capabilities: &[Capability],
    model: &ModelEntry,
) -> Vec<ReasoningLevel> {
    let seeded: &[ReasoningLevel] = if model.capabilities.contains(&Capability::Reasoning)
        || provider_capabilities.contains(&Capability::Reasoning)
        || dialect.always_reasons()
    {
        dialect.expressible_reasoning_levels()
    } else {
        &[]
    };
    let candidate: &[ReasoningLevel] = if !model.reasoning_levels.is_empty() {
        &model.reasoning_levels
    } else {
        match crate::catalog::catalog().model_meta(provider_id, &model.id) {
            Some(meta) if !meta.levels.is_empty() => meta.levels,
            _ => seeded,
        }
    };

    let mut levels: Vec<ReasoningLevel> = candidate
        .iter()
        .copied()
        .filter(|level| dialect.supports_reasoning_level(*level))
        .collect();
    levels.sort_unstable_by_key(|level| level.rank());
    levels.dedup();
    levels
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

/// What a model costs, and where the numbers came from.
///
/// Rates are US dollars per million tokens. Every rate is optional because providers publish
/// different subsets: a model that bills no cache reads has *no* cache-read rate, which is not the
/// same as a rate of zero, and the panel must be able to tell those apart.
///
/// `Default` is the honest "nothing is known" price, which is what makes it usable as the base a
/// merge starts from; it is deliberately *not* an all-zero price.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PricingPlan {
    /// Prompt tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok: Option<f64>,
    /// Completion tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok: Option<f64>,
    /// Tokens served from a provider-side prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_per_mtok: Option<f64>,
    /// Tokens written to a provider-side prompt cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_per_mtok: Option<f64>,
    /// Which source these numbers came from.
    #[serde(default)]
    pub source: PricingSource,
}

impl PricingPlan {
    /// Whether any rate at all is known.
    ///
    /// A plan with no rates renders as `N/A`, never as free: an unknown price is not a zero price,
    /// and a cost estimate that silently reads $0.00 for an unknown model is worse than no estimate.
    #[inline]
    pub const fn has_any_rate(&self) -> bool {
        self.input_per_mtok.is_some()
            || self.output_per_mtok.is_some()
            || self.cache_read_per_mtok.is_some()
            || self.cache_write_per_mtok.is_some()
    }
}

/// Where a [`PricingPlan`]'s numbers came from, least authoritative first.
///
/// The precedence matters because the three sources genuinely disagree: the bundled LiteLLM table
/// lags a provider's price change, a provider's live response reflects it first, and an operator
/// who has typed a number has the final word. Deriving `Ord` in this declaration order makes
/// "which source wins" a `max` rather than a hand-written match that can drift from this comment.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PricingSource {
    /// The bundled offline table generated by `tools/gen-pricing.py`.
    #[default]
    Bundled,
    /// Live provider pricing, where the provider exposes it (OpenRouter-style).
    Synced,
    /// Typed into the panel by the operator.
    Manual,
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
    ///
    /// `windows` is accepted as an alias: the quota documentation has always shown the field
    /// under that name, and rejecting it would make the documented example a parse error for the
    /// operator who copied it.
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "windows")]
    pub quota: Vec<QuotaWindow>,
    /// Free-form tags naming who the account belongs to, or what it is for.
    ///
    /// A tag is a small note, not a security boundary. It exists for two jobs that are the same
    /// job seen from two ends: a combo step can select the accounts it may use by tag
    /// (`accounts: ["tag:alice"]`), and a tag that names a configured [`crate::config::PersonConfig`]
    /// decides which machine the account's traffic leaves from — that person's Tailscale exit node,
    /// expressed as a proxy. Matching is case-insensitive and surrounding whitespace is ignored, so
    /// `Alice` and `alice` are one tag.
    ///
    /// The first tag that names a person wins, in the order the tags are written. An account with
    /// no tags is nobody's in particular and egresses the way its provider does.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Which configured proxy this account's traffic goes through, by [`ProxyConfig::id`].
    ///
    /// `None` falls back to the person named by [`Self::tags`], then to the provider's
    /// [`Provider::proxy`], then to a direct connection. The account level exists because one
    /// account can be the one that is rate-limited or region-locked, and moving *just it* behind a
    /// proxy must not move the whole provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Values for the named placeholders in the provider's base URL.
    ///
    /// A catalogue base URL may name a value only the account knows — Cloudflare Workers AI
    /// addresses `…/accounts/{account_id}/…`, Snowflake addresses `{account}.snowflakecomputing.com`
    /// — so the name lives in the catalogue and the value lives here. The map is per *account*
    /// because one provider can be used by several accounts, each with its own id.
    ///
    /// Substitution happens in the relay, at request time, once the account is chosen; see
    /// [`crate::placeholders`] for the rule and for why a value must be URL-safe. An empty map is
    /// the normal case: a provider whose base URL has no braces never reads this.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub placeholders: BTreeMap<String, String>,
    /// Remaining credit in **US dollars**, when the operator declares it.
    ///
    /// This is an input to the fine-tuning credit floor, never a spend meter: the router does not
    /// decrement it, because only the upstream knows what a job cost. When it is absent the relay
    /// falls back to a **freshly cached** USD balance from quota tracking, so an operator whose
    /// provider exposes a balance endpoint does not have to repeat the number here. A declared
    /// value wins over the cache: it is the operator's assertion about an account the router
    /// cannot otherwise price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credit_usd: Option<f64>,
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
            tags: Vec::new(),
            proxy: None,
            placeholders: BTreeMap::new(),
            credit_usd: None,
            extra: BTreeMap::new(),
        }
    }

    /// Whether this account is allowed to serve `model`.
    ///
    /// A deny entry always wins over an allow entry, so an operator can allow `*` and then carve
    /// out an exception without having to enumerate everything else.
    pub fn allows_model(&self, model: &str) -> bool {
        if self
            .models_deny
            .iter()
            .any(|m| m == model || m == "*" || glob_match(m, model))
        {
            return false;
        }
        if self.models_allow.is_empty() {
            return true;
        }
        self.models_allow
            .iter()
            .any(|m| m == model || m == "*" || glob_match(m, model))
    }

    /// Whether this account carries `tag`, ignoring case and surrounding whitespace.
    ///
    /// The comparison is deliberately forgiving: a tag is typed by a person into a CLI flag or a
    /// panel field, and `Alice ` failing to match `alice` would be a bug report, not a feature.
    pub fn has_tag(&self, tag: &str) -> bool {
        let wanted = tag.trim();
        !wanted.is_empty()
            && self
                .tags
                .iter()
                .any(|own| own.trim().eq_ignore_ascii_case(wanted))
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

/// Whether `selector` names `account`.
///
/// A selector is an account id, or a tag with an optional `tag:` prefix — `alice` and `tag:alice`
/// are the same selector. The id is checked first, so an account whose id happens to equal a tag
/// stays addressable; the tag comparison ignores case and surrounding whitespace, the same rule
/// [`Account::has_tag`] applies. This is the one definition of a selector: configuration
/// validation, snapshot compilation and the CLI all read it from here.
pub fn account_selector_matches(selector: &str, account: &Account) -> bool {
    let trimmed = selector.trim();
    if trimmed.is_empty() {
        return false;
    }
    match trimmed.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("tag:") => account.has_tag(&trimmed[4..]),
        _ => account.id.as_str() == trimmed || account.has_tag(trimmed),
    }
}

/// A minimal glob matcher supporting a single trailing `*`.
///
/// Deliberately not a regex: this runs while compiling a snapshot, and a full regex engine would be
/// a large dependency for what is always a prefix match in practice (`claude-*`, `gpt-*`). A
/// mid-string `*` is treated literally.
pub fn glob_match(pattern: &str, value: &str) -> bool {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Maximum upstream-reported spend in the window, in US cents. Zero means
    /// unlimited. This is the generic money dimension: the upstream can report
    /// it directly, or the relay can settle an estimated/model-priced amount.
    #[serde(
        default,
        skip_serializing_if = "is_zero_u64",
        alias = "limit_cost_cents"
    )]
    pub limit_usd_cents: u64,
    /// Operators frequently know the reset clock of a plan (a 5-hour rolling window, a monthly
    /// calendar) without knowing the limits. Setting this marks the window as
    /// [`WindowKind::ProviderDeclared`] so only the reset timing is used for demotion.
    #[serde(default)]
    pub reset_only: bool,
}

fn default_window_kind() -> WindowKind {
    WindowKind::Rolling
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
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
            limit_usd_cents: 0,
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

    /// A spend window in US cents. The caller supplies a provider-agnostic
    /// cap; the relay settles whatever the upstream reported, or an estimate,
    /// and the account quota module applies the same window arithmetic used
    /// for requests and tokens.
    pub fn usd_cents_per(seconds: i64, limit_usd_cents: u64) -> Self {
        Self {
            seconds,
            limit_usd_cents,
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
            || self.limit_usd_cents > 0
    }

    /// Whether this window can make an account unusable.
    ///
    /// Stricter than [`Self::is_effective`]: a `reset_only` window is effective — it is shown to the
    /// operator and its reset instant is tracked — but it constrains nothing, because the operator
    /// wrote it to say "watch this clock" rather than "stop here". Counting it as a limit would
    /// turn a monitoring declaration into a refusal, which is the opposite of what was asked.
    #[inline]
    pub const fn constrains(&self) -> bool {
        self.limit_requests > 0
            || self.limit_tokens > 0
            || self.limit_input_tokens > 0
            || self.limit_output_tokens > 0
            || self.limit_usd_cents > 0
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
    /// What a refusal takes out of rotation. See [`CooldownScope`].
    #[serde(default)]
    pub cooldown_scope: CooldownScope,
    /// Models discovered from `/models`, plus hand-entered ones.
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    /// When the last discovery run completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_synced_at: Option<Millis>,
    /// Provider-wide quota windows, applied to any account that does not override them.
    ///
    /// `windows` is accepted as an alias for the same reason as on [`Account`].
    #[serde(default, skip_serializing_if = "Vec::is_empty", alias = "windows")]
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
    ///
    /// Defaults to **false**, making prewarm a per-provider opt-in: adding a provider opens no
    /// connection until it is used, so a provider that is misconfigured, rate-limited or simply
    /// unwanted costs nothing on start.
    ///
    /// The global `latency.prewarm` is *also* required, and acts as the master switch
    /// (`effective = latency.prewarm && provider.prewarm`). It is an `&&`, not an `||`, because the
    /// alternative is self-defeating: `latency.prewarm` defaults to true, so an `||` would re-enable
    /// every provider the moment the file was loaded and "adding a provider opens no connection"
    /// would be unobservable.
    #[serde(default)]
    pub prewarm: bool,
    /// Whether this provider prefers HTTP/2 when the host supports it.
    #[serde(default = "default_http2")]
    pub http2: Http2Preference,
    /// KV-token budget configuration for self-hosted providers (e.g. Modal vLLM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kv_pool_tokens: Option<u64>,
    /// Maximum declared context tokens; overrides discovered model metadata when provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_context: Option<u64>,
    /// Maximum simultaneous requests accepted by the endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// Cost per second in USD. When set, GPU-second billing is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_per_second: Option<f64>,
    /// A self-hosted Modal/vLLM endpoint reports 503 while scaling from cold; use a long TTFB
    /// budget to allow one boot and prefill.
    #[serde(
        default = "default_cold_start_ttfb_timeout_ms",
        skip_serializing_if = "Option::is_none"
    )]
    pub cold_start_ttfb_timeout_ms: Option<u64>,
    /// Which configured proxy this provider's traffic goes through, by [`ProxyConfig::id`].
    ///
    /// `None` dials the upstream directly. An account may override this, which is the case the
    /// feature exists for: several accounts on one provider where only one needs to egress
    /// elsewhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    /// Operator note, shown in the panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Open escape hatch for provider-specific configuration, preserved on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// What a refusal takes out of rotation: one credential, one model on it, or one model everywhere.
///
/// A provider's limits are not always a property of the credential. Some meter **one model** apart
/// from the rest of the catalogue — OpenAdapter's free plan counts Pro models separately from Lite
/// ones, and Antigravity publishes a quota per model — and some meter **every credential of the
/// provider** in one bucket. Recording every refusal against the account, which is the historical
/// behaviour and still the default, makes a spent Pro allowance look like a dead credential: the
/// combo's Lite step on that same account is skipped for a reason that never applied to it.
///
/// The scope decides the key a refusal is recorded under. A lookup consults every shape that applies
/// to the request, so a narrow refusal is never widened by the routing code. Two things are fixed
/// whatever the scope says, because they describe the credential rather than its limits: an
/// authentication failure is always recorded against the account, and a malformed request records
/// nothing at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum CooldownScope {
    /// The credential, for every model it can serve. The default, and the behaviour before scopes
    /// existed.
    #[default]
    Account = 0,
    /// The credential, for the one model that was refused. Right when the upstream meters each model
    /// separately, so its sibling models are still worth trying.
    Model = 1,
    /// Every credential of the provider, for the one model that was refused. Right when the quota is
    /// shared by the whole provider, so an untried account would be refused for the same reason.
    Provider = 2,
}

impl CooldownScope {
    /// Every scope, from the narrowest to the widest, for command-line spellings and listings.
    pub const ALL: [Self; 3] = [Self::Account, Self::Model, Self::Provider];

    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Account => "account",
            Self::Model => "model",
            Self::Provider => "provider",
        }
    }

    /// Parse the config identifier, tolerating underscores and spacing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "account" => Some(Self::Account),
            "model" => Some(Self::Model),
            "provider" => Some(Self::Provider),
            _ => None,
        }
    }
}

fn default_capabilities() -> Vec<Capability> {
    // Every capability on by default: a new provider is a blank best-effort surface, and the
    // operator can uncheck what the upstream does not serve. The panel mirrors this exact list.
    Capability::ALL.to_vec()
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

fn default_cold_start_ttfb_timeout_ms() -> Option<u64> {
    None
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
        let modal_vllm = dialect == Dialect::ModalVllm;
        Self {
            id: ProviderId::from(id.into()),
            label: None,
            dialect,
            base_url: base_url.into(),
            auth: AuthKind::default(),
            capabilities: default_capabilities(),
            accounts: Vec::new(),
            key_strategy: KeyStrategy::default(),
            cooldown_scope: CooldownScope::default(),
            models: Vec::new(),
            models_synced_at: None,
            quota: Vec::new(),
            enabled: true,
            headers: BTreeMap::new(),
            route_overrides: BTreeMap::new(),
            connect_timeout_ms: if modal_vllm {
                300_000
            } else {
                default_connect_timeout_ms()
            },
            ttfb_timeout_ms: default_ttfb_timeout_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
            total_timeout_ms: 0,
            prewarm: false,
            http2: Http2Preference::Auto,
            kv_pool_tokens: None,
            max_context: None,
            max_concurrent: None,
            price_per_second: None,
            cold_start_ttfb_timeout_ms: modal_vllm.then_some(300_000),
            proxy: None,
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

    /// The reasoning levels this provider's dialect can send, ascending.
    ///
    /// The panel's pickers read the same list, so a level offered in the UI is one the validator
    /// and the compiler will also accept for this provider.
    #[inline]
    pub fn dialect_reasoning_levels(&self) -> &'static [ReasoningLevel] {
        self.dialect.expressible_reasoning_levels()
    }

    /// The reasoning levels `model` actually offers, ascending.
    #[inline]
    pub fn effective_reasoning_levels(&self, model: &ModelEntry) -> Vec<ReasoningLevel> {
        effective_reasoning_levels(self.id.as_str(), self.dialect, &self.capabilities, model)
    }

    /// Why a configured level cannot reach this provider, or `None` when it can.
    ///
    /// The message names the level, the dialect and the levels that dialect *can* send, because a
    /// bare "invalid level" leaves the operator guessing which spelling the provider wanted. The
    /// same wording is used for a model entry, a model default and a combo step so the three
    /// surfaces do not read as three different rules.
    pub fn unsendable_level_reason(&self, level: ReasoningLevel) -> Option<String> {
        if self.dialect.supports_reasoning_level(level) {
            return None;
        }
        let allowed: Vec<&str> = self
            .dialect
            .expressible_reasoning_levels()
            .iter()
            .map(|level| level.as_str())
            .collect();
        Some(format!(
            "the {} dialect cannot send reasoning level {:?}; it can express {}",
            self.dialect.as_str(),
            level.as_str(),
            allowed.join(", "),
        ))
    }

    /// Every reasoning-level problem on this provider's model entries.
    ///
    /// Validation is deliberately stricter than the compiler. The compiler caps an offered set to
    /// what the dialect can send so a stale file still routes, but that cap is a silent loss: the
    /// operator asked for `max` and would get `xhigh`. Reporting it here is what makes the loss
    /// visible before a request is spent.
    pub fn reasoning_level_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for model in &self.models {
            for level in &model.reasoning_levels {
                if let Some(reason) = self.unsendable_level_reason(*level) {
                    problems.push(format!(
                        "model {:?} declares reasoning level {:?}, but {reason}",
                        model.id,
                        level.as_str(),
                    ));
                }
            }
            if let Some(default) = model.default_reasoning_level {
                if let Some(reason) = self.unsendable_level_reason(default) {
                    problems.push(format!(
                        "model {:?} defaults to reasoning level {:?}, but {reason}",
                        model.id,
                        default.as_str(),
                    ));
                } else if !model.reasoning_levels.is_empty()
                    && !model.reasoning_levels.contains(&default)
                {
                    let declared: Vec<&str> = model
                        .reasoning_levels
                        .iter()
                        .map(|level| level.as_str())
                        .collect();
                    problems.push(format!(
                        "model {:?} defaults to reasoning level {:?}, but its declared \
                         reasoning_levels do not include it ({})",
                        model.id,
                        default.as_str(),
                        declared.join(", "),
                    ));
                }
            }
            if let Some(wire) = &model.reasoning_wire {
                for level in wire.keys() {
                    if let Some(reason) = self.unsendable_level_reason(*level) {
                        problems.push(format!(
                            "model {:?} overrides the reasoning wire for level {:?}, but {reason}",
                            model.id,
                            level.as_str(),
                        ));
                    }
                }
            }
        }
        problems
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
            // Bearer is exempt on purpose: a local llama.cpp or Ollama server accepts requests with
            // no credential at all, and an account with no secret is exactly how that is expressed.
            // For every other auth kind the upstream would certainly reject the request, so an
            // enabled account with nothing attached is a configuration error rather than a choice.
            if !has_secret && a.enabled && !matches!(self.auth, AuthKind::Bearer) {
                problems.push(format!(
                    "account {:?} has no credential attached, but this provider does not \
                     authenticate with a bearer token; add the account's secret",
                    a.label.as_deref().unwrap_or(a.id.as_str()),
                ));
            }
            for w in &a.quota {
                if w.seconds <= 0 {
                    problems.push(format!(
                        "account {} has a quota window with a non-positive length",
                        a.id
                    ));
                }
            }
            // A negative or non-finite credit would quietly disqualify an account from the
            // fine-tuning floor, or worse compare as "richer" under a NaN. Refuse it at save time.
            if a.credit_usd
                .is_some_and(|credit| !credit.is_finite() || credit < 0.0)
            {
                problems.push(format!(
                    "account {} declares a credit_usd that is not a finite, non-negative number",
                    a.id
                ));
            }
        }
        if self.ttfb_timeout_ms == 0 && self.total_timeout_ms == 0 {
            problems.push(
                "both ttfb_timeout_ms and total_timeout_ms are zero, so a hung upstream would \
                 hold a request forever"
                    .into(),
            );
        }
        if self.dialect == Dialect::ModalVllm {
            if self
                .price_per_second
                .is_some_and(|price| !price.is_finite() || price <= 0.0)
            {
                problems.push("price_per_second must be a finite positive number".into());
            }
            if self
                .cold_start_ttfb_timeout_ms
                .is_some_and(|timeout| timeout == 0)
            {
                problems.push("cold_start_ttfb_timeout_ms must be greater than zero".into());
            }
        }
        for m in &self.models {
            if m.id.is_empty() {
                problems.push("a model entry has an empty id".into());
            }
        }
        problems.extend(self.reasoning_level_problems());
        // A capability the dialect cannot serve only surfaces as a failed request otherwise:
        // correct-looking configuration, opaque upstream error.
        problems.extend(self.dialect_capability_mismatches());
        problems.extend(self.oauth_platform_problems());
        problems
    }

    /// Problems with the OAuth platform this provider names, if it asks for OAuth at all.
    ///
    /// Only an *empty* platform is reported here. An `oauth` provider with no platform has no
    /// login to start, so it can never hold a credential: the panel draws a Connect button whose
    /// only outcome is an error. The control plane refuses to *save* that; this reports it for a
    /// configuration that acquired the shape some other way (a hand edit, an import, an older
    /// panel), because a file in that state must still **load** — the operator has to be able to
    /// open the panel to repair it, so the problem is advisory rather than fatal.
    ///
    /// An *unregistered* platform is deliberately not reported. The catalogue ships entries whose
    /// platform this build does not implement yet and says so in their note (`cline`), so a
    /// provider module cannot distinguish a documented migration target from a typo. The
    /// control plane, which is where an operator types an id by hand, refuses those instead.
    pub fn oauth_platform_problems(&self) -> Vec<String> {
        let AuthKind::OAuth { platform } = &self.auth else {
            return Vec::new();
        };
        if !platform.trim().is_empty() {
            return Vec::new();
        }
        vec![format!(
            "provider {:?} has no OAuth platform, so it has no login to start and can never \
             hold a credential; set auth.platform to one of: {}",
            self.id.as_str(),
            crate::oauth::platforms()
                .iter()
                .map(|p| p.id())
                .collect::<Vec<_>>()
                .join(", ")
        )]
    }

    /// Which of the client routes this provider claims but cannot actually serve, given its
    /// dialect. Catches the common misconfiguration of attaching an Anthropic base URL to a
    /// provider still declaring OpenAI capabilities.
    pub fn dialect_capability_mismatches(&self) -> Vec<String> {
        let mut out = Vec::new();
        let wants = |c: Capability| self.capabilities.contains(&c);
        match self.dialect {
            // OpenAI is the reference surface: every capability in this build, Search included, is
            // one of its routes, so there is nothing it can claim that it cannot serve.
            Dialect::OpenAi | Dialect::MistralVibe => {}
            // ZCode's plan surface is the same Messages endpoint and nothing else, so the same
            // complaints apply. Its count-tokens route is answered locally rather than proxied, so
            // `messages` covers a client that counts through it.
            //
            // "Text" here means an endpoint that takes a prompt and returns tokens. Vision is text
            // on the wire (image input, token output), while every other non-Messages family —
            // embeddings, audio, images, moderation, video, realtime and the management surfaces
            // appended in this build — has no equivalent on the Messages API at all.
            //
            // Claude Code's surface is the same Messages endpoint: the identity sentence and the
            // subscription headers change how a request is accepted, not which families exist.
            Dialect::Anthropic | Dialect::Zcode | Dialect::ClaudeCli => {
                for c in Capability::ALL {
                    if wants(c) && !is_text_capability(c) {
                        out.push(format!(
                            "the Anthropic Messages dialect has no {} endpoint",
                            c.as_str()
                        ));
                    }
                }
            }
            // Codex serves the Responses API. It shares OpenAI's text-generation vocabulary but
            // none of the endpoint families around it, including the ones appended in this build.
            Dialect::Codex => {
                for c in Capability::ALL {
                    if wants(c) && !is_text_capability(c) {
                        out.push(format!(
                            "the Codex dialect only serves the Responses API, not {}",
                            c.as_str()
                        ));
                    }
                }
            }
            // Command Code's alpha/generate envelope carries a prompt and nothing else. It had no
            // embeddings endpoint before this build and gained no audio, image, video or management
            // surface with it.
            Dialect::CommandCodeGo => {
                if wants(Capability::Embeddings) {
                    out.push("the Command Code dialect has no embeddings endpoint".to_string());
                }
                push_openai_only_mismatches(&mut out, self);
            }
            Dialect::ModalVllm => {
                for c in [
                    Capability::Responses,
                    Capability::Embeddings,
                    Capability::AudioTranscription,
                    Capability::AudioSpeech,
                    Capability::ImagesGenerations,
                ] {
                    if wants(c) {
                        out.push(format!(
                            "the Modal vLLM dialect only serves OpenAI-compatible chat completions, not {}",
                            c.as_str()
                        ));
                    }
                }
                push_openai_only_mismatches(&mut out, self);
            }
            // These dialects speak their vendor's own protocol (or, for Copilot and Cursor, the
            // agent's). None of the endpoint families appended in this build has an equivalent
            // there, so they are refused while the families each dialect already served keep
            // whatever rule they had — that is a separate, older question.
            Dialect::Google
            | Dialect::GoogleAiStudio
            | Dialect::Antigravity
            | Dialect::Copilot
            | Dialect::Cursor => {
                push_openai_only_mismatches(&mut out, self);
            }
        }
        out
    }
}

/// Whether a capability is a prompt-in, tokens-out endpoint.
///
/// This is the boundary the Messages dialects and Codex are described by: everything that is not
/// one of these has a request or response body the dialect cannot build, so claiming it would only
/// surface as an opaque upstream error. The match is exhaustive on purpose — appending a capability
/// forces a decision about which side of the line it falls on rather than defaulting to "text".
fn is_text_capability(capability: Capability) -> bool {
    match capability {
        Capability::Chat
        | Capability::Responses
        | Capability::Models
        | Capability::Messages
        | Capability::ToolCalling
        | Capability::Reasoning
        | Capability::Vision => true,
        Capability::Embeddings
        | Capability::AudioTranscription
        | Capability::AudioSpeech
        | Capability::ImagesGenerations
        | Capability::AudioTranslation
        | Capability::ImagesEdits
        | Capability::ImagesVariations
        | Capability::Moderations
        | Capability::Video
        | Capability::Realtime
        | Capability::VectorStores
        | Capability::Containers
        | Capability::FineTuning
        | Capability::Evals
        | Capability::Search => false,
    }
}

/// Whether a capability belongs to an endpoint family only the OpenAI dialect serves.
///
/// These are the capabilities appended in the build that introduced the audio-translation,
/// image-edit, moderation, video, realtime and management routes, plus Search. No other dialect
/// compiled in here has a route for any of them. The match is exhaustive so that appending another
/// capability cannot silently become "accepted by every dialect": it has to be classified here.
fn is_openai_only_capability(capability: Capability) -> bool {
    match capability {
        Capability::AudioTranslation
        | Capability::ImagesEdits
        | Capability::ImagesVariations
        | Capability::Moderations
        | Capability::Video
        | Capability::Realtime
        | Capability::VectorStores
        | Capability::Containers
        | Capability::FineTuning
        | Capability::Evals
        | Capability::Search => true,
        Capability::Chat
        | Capability::Responses
        | Capability::Embeddings
        | Capability::AudioTranscription
        | Capability::AudioSpeech
        | Capability::ImagesGenerations
        | Capability::Models
        | Capability::Messages
        | Capability::ToolCalling
        | Capability::Reasoning
        | Capability::Vision => false,
    }
}

/// Append a complaint for every OpenAI-only capability a non-OpenAI provider claims.
///
/// Kept as one function so every dialect that shares this rule names itself in the same place; the
/// message includes the dialect so an operator staring at a provider list knows which entry to
/// repair.
fn push_openai_only_mismatches(out: &mut Vec<String>, provider: &Provider) {
    for capability in Capability::ALL {
        if provider.capabilities.contains(&capability) && is_openai_only_capability(capability) {
            out.push(format!(
                "the {} dialect has no {} endpoint",
                provider.dialect.as_str(),
                capability.as_str()
            ));
        }
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
    ///
    /// Empty when this step routes to another combo instead; see [`Self::combo`].
    #[serde(
        default = "empty_provider_id",
        skip_serializing_if = "provider_is_empty"
    )]
    pub provider: ProviderId,
    /// The model id as the upstream expects it.
    ///
    /// Empty when this step routes to another combo instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// Route this step to another combo by id or name.
    ///
    /// A combo target is expanded where it stands: the nested combo's enabled steps are spliced
    /// into this combo's step list at compile time, in its own order. That makes a combo callable
    /// from another without copying its steps, while the caller keeps its own strategy, retries,
    /// hedge and compression. The nested combo's strategy is deliberately ignored — it contributes
    /// steps, not a second router.
    ///
    /// Cycles are rejected when the configuration is validated and again at compile time, because
    /// a loop here would be an unbounded request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub combo: Option<String>,
    /// Which of the provider's accounts may serve this step. Empty means every account.
    ///
    /// Each entry is either an account id, or `tag:<name>` for every account carrying that tag —
    /// which is how a step is written as "my accounts, and team member 1's, but not team member
    /// 2's". A bare word is read as a tag too, so `alice` and `tag:alice` mean the same thing; the
    /// prefix exists for the case where a tag happens to look like an id.
    ///
    /// The selection is resolved when the snapshot is compiled, so the relay compares account ids
    /// and never parses a selector at request time. An entry that matches no account of this
    /// provider is a configuration problem, reported by [`crate::Config::validate`]; a selector
    /// that matches only *disabled* accounts resolves to an empty set, which makes the step
    /// unusable rather than silently widening it back to every account.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<String>,
    /// Whether the operator has this step enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Relative weight, reserved for future weighted strategies.
    #[serde(default)]
    pub weight: u32,
    /// The reasoning level this step runs at, pinning it without touching the model's own default.
    ///
    /// A combo can mix tiers deliberately — a cheap model at `low` first, an expensive one at `max`
    /// when it retries. Absent means "the model's default", which is what a step has always done.
    ///
    /// A level the model does not offer is **not** an error and never reaches the wire: it is
    /// clamped to that model's highest available level at compile time (`max` on a model that tops
    /// out at `xhigh` runs at `xhigh`), the same substitution an out-of-range `-<level>` suffix
    /// follows. A model that offers no levels at all emits no reasoning field regardless.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<crate::dialect::ReasoningLevel>,
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
            combo: None,
            accounts: Vec::new(),
            enabled: true,
            weight: 0,
            level: None,
            note: None,
        }
    }

    /// A step that routes to another combo.
    pub fn from_combo(combo: impl Into<String>) -> Self {
        Self {
            provider: ProviderId::from(String::new()),
            model: String::new(),
            combo: Some(combo.into()),
            accounts: Vec::new(),
            enabled: true,
            weight: 0,
            level: None,
            note: None,
        }
    }

    /// Whether this step names another combo rather than a provider/model pair.
    pub fn is_combo_target(&self) -> bool {
        self.combo.is_some()
    }
}

fn empty_provider_id() -> ProviderId {
    ProviderId::from(String::new())
}

fn provider_is_empty(id: &ProviderId) -> bool {
    id.as_str().is_empty()
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
    /// The hedged race configuration. `enabled` also opts priority combos into sticky-success
    /// selection.
    ///
    /// The race itself runs: the relay dispatches every participant at once and commits the
    /// highest-priority participant that answers inside the eligibility window. Winner recovery
    /// (`on_winner_fail`) and the mid-stream switch are not implemented, because the winner is
    /// committed before its body streams.
    #[serde(default)]
    pub hedge: HedgeSpec,
    /// Inactivity window in seconds for combo-wide sticky-success selection. Used only when
    /// strategy is priority and hedge is enabled. Zero disables remembered preference.
    #[serde(default = "default_sticky_success_window_secs")]
    pub sticky_success_window_secs: u32,
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
fn default_sticky_success_window_secs() -> u32 {
    1_800
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
            sticky_success_window_secs: default_sticky_success_window_secs(),
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
            if let Some(target) = &s.combo {
                // Whether the target exists, and whether following it closes a loop, is a
                // cross-combo question: Config::validate owns it. Here only the step's own
                // shape is checked.
                if target.trim().is_empty() {
                    problems.push(format!("step {i} has an empty combo target"));
                }
                continue;
            }
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
        h ^= fnv1a64(&self.sticky_success_window_secs.to_le_bytes());
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
            // A step's pinned tier and its combo target both change which upstream is called, so a
            // signature that ignored them would leave the relay running the previous plan.
            if let Some(level) = s.level {
                h ^= fnv1a64(&[level.rank()]);
            }
            if let Some(target) = &s.combo {
                h ^= fnv1a64(target.as_bytes());
            }
            // Which accounts a step may use changes which upstream credential is called, so a
            // signature that ignored the selection would leave the relay running the previous plan
            // after the operator narrowed a step.
            for selector in &s.accounts {
                h ^= fnv1a64(selector.as_bytes());
            }
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
    /// If non-empty, the key may only call these combos (matched against the combo's id or name).
    ///
    /// A combo-scoped key cannot address a provider/model directly, and `GET /v1/models` shows
    /// only the models reachable through the scoped combos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_combos: Vec<String>,
    /// Per-key request budget. Zero means unlimited.
    #[serde(default)]
    pub limit_requests: u64,
    /// Rolling token and request caps per window, enforced by the relay.
    ///
    /// Separate from [`Self::limit_requests`], which is a flat, non-rolling budget kept for
    /// compatibility. The two are both enforced when both are set.
    #[serde(default, skip_serializing_if = "KeyLimits::is_empty")]
    pub limits: KeyLimits,
    /// Descriptive note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A client key's rolling caps, one per window family the quota panel already shows.
///
/// The three windows are named explicitly rather than stored as a list of durations because they
/// are the three the operator is actually offered, and because an explicit field cannot be
/// silently mistyped into a fourth window nothing enforces. Each is independently optional, and
/// `None` means *not enforced* rather than *zero allowed*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyLimits {
    /// Caps over a rolling five hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_5h: Option<KeyLimit>,
    /// Caps over a rolling seven days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly_1w: Option<KeyLimit>,
    /// Caps over a rolling thirty days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_30d: Option<KeyLimit>,
}

impl KeyLimits {
    /// Whether nothing at all is capped, which is the overwhelmingly common case and is why the
    /// field is skipped on serialisation.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.session_5h.is_none() && self.weekly_1w.is_none() && self.monthly_30d.is_none()
    }
}

/// One window's caps for a client key. Zero in either field means that dimension is uncapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KeyLimit {
    /// Prompt + completion tokens allowed in the window. Zero means uncapped.
    #[serde(default)]
    pub tokens: u64,
    /// Requests allowed in the window. Zero means uncapped.
    #[serde(default)]
    pub requests: u64,
}

impl KeyLimit {
    /// Whether this window caps nothing, in which case it is inert and the relay skips it.
    #[inline]
    pub const fn is_inert(&self) -> bool {
        self.tokens == 0 && self.requests == 0
    }
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

    /// A model entry with nothing declared, so level derivation starts from the dialect.
    fn model_entry(id: &str) -> ModelEntry {
        ModelEntry {
            id: id.into(),
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
        }
    }

    /// A provider whose only property under test is its dialect and capabilities.
    fn reasoning_provider(dialect: Dialect, capabilities: Vec<Capability>) -> Provider {
        let mut p = Provider::new("p1", dialect, "https://example.test/v1");
        p.capabilities = capabilities;
        p
    }

    #[test]
    fn a_model_with_no_levels_exposes_the_dialects_levels_when_the_provider_reasons() {
        let p = reasoning_provider(
            Dialect::OpenAi,
            vec![Capability::Chat, Capability::Reasoning],
        );
        assert_eq!(
            p.effective_reasoning_levels(&model_entry("m1")),
            vec![
                ReasoningLevel::None,
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
            ]
        );
    }

    #[test]
    fn a_model_with_no_levels_and_no_reasoning_capability_exposes_none() {
        let p = reasoning_provider(Dialect::OpenAi, vec![Capability::Chat]);
        assert!(p.effective_reasoning_levels(&model_entry("m1")).is_empty());
    }

    #[test]
    fn a_codex_model_is_seeded_without_a_reasoning_capability() {
        // Codex serves reasoning models exclusively, so the dialect itself is the evidence.
        let p = reasoning_provider(Dialect::Codex, vec![Capability::Responses]);
        assert_eq!(
            p.effective_reasoning_levels(&model_entry("gpt-5.5-codex")),
            Dialect::Codex.expressible_reasoning_levels()
        );
    }

    #[test]
    fn a_declared_level_set_wins_over_the_dialect_default() {
        let p = reasoning_provider(
            Dialect::OpenAi,
            vec![Capability::Chat, Capability::Reasoning],
        );
        let mut m = model_entry("m1");
        m.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
        assert_eq!(
            p.effective_reasoning_levels(&m),
            vec![ReasoningLevel::Low, ReasoningLevel::High]
        );
    }

    #[test]
    fn the_compiler_cap_drops_a_catalogue_tier_the_dialect_cannot_send() {
        // The bundled table says this model offers `max`; the openai dialect lowers `max` to
        // `xhigh`, so offering it would advertise a tier the upstream never receives.
        let mut p = reasoning_provider(Dialect::OpenAi, vec![Capability::Chat]);
        p.id = crate::ProviderId::from("agnes".to_owned());
        assert_eq!(
            p.effective_reasoning_levels(&model_entry("agnes-2.0-flash")),
            vec![
                ReasoningLevel::None,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
            ]
        );
    }

    #[test]
    fn a_level_the_dialect_cannot_send_is_refused_with_its_name() {
        let mut p = reasoning_provider(Dialect::OpenAi, vec![Capability::Chat]);
        let mut m = model_entry("m1");
        m.reasoning_levels = vec![ReasoningLevel::Max];
        p.models = vec![m];

        let problems = p.reasoning_level_problems();
        assert_eq!(problems.len(), 1, "{problems:?}");
        let problem = &problems[0];
        assert!(problem.contains("\"max\""), "{problem}");
        assert!(problem.contains("openai"), "{problem}");
        assert!(
            problem.contains("none, minimal, low, medium, high, xhigh"),
            "the message must quote the levels the dialect can send: {problem}"
        );
    }

    #[test]
    fn a_budget_dialect_accepts_a_level_the_named_dialect_refuses() {
        let mut p = reasoning_provider(Dialect::Anthropic, vec![Capability::Chat]);
        let mut m = model_entry("m1");
        m.reasoning_levels = vec![ReasoningLevel::Max];
        p.models = vec![m];
        assert!(p.reasoning_level_problems().is_empty());
    }

    #[test]
    fn a_model_default_the_dialect_cannot_send_is_refused_with_its_name() {
        let mut p = reasoning_provider(Dialect::OpenAi, vec![Capability::Chat]);
        let mut m = model_entry("m1");
        m.default_reasoning_level = Some(ReasoningLevel::Ultra);
        p.models = vec![m];

        let problem = p.reasoning_level_problems().join("\n");
        assert!(problem.contains("\"ultra\""), "{problem}");
        assert!(problem.contains("defaults"), "{problem}");
    }

    #[test]
    fn a_model_default_must_be_one_of_the_levels_it_declares() {
        let mut p = reasoning_provider(Dialect::Anthropic, vec![Capability::Chat]);
        let mut m = model_entry("m1");
        m.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
        m.default_reasoning_level = Some(ReasoningLevel::Max);
        p.models = vec![m];

        let problem = p.reasoning_level_problems().join("\n");
        assert!(problem.contains("do not include it"), "{problem}");
        assert!(problem.contains("low, high"), "{problem}");
    }

    #[test]
    fn a_reasoning_wire_override_for_an_unsendable_level_is_refused() {
        let mut p = reasoning_provider(Dialect::OpenAi, vec![Capability::Chat]);
        let mut m = model_entry("m1");
        m.reasoning_wire = Some(
            [(
                ReasoningLevel::Max,
                serde_json::json!({ "reasoning_effort": "high" }),
            )]
            .into_iter()
            .collect(),
        );
        p.models = vec![m];

        let problem = p.reasoning_level_problems().join("\n");
        assert!(problem.contains("\"max\""), "{problem}");
        assert!(
            problem.contains("overrides the reasoning wire"),
            "{problem}"
        );
    }

    #[test]
    fn provider_validation_carries_the_reasoning_level_problem() {
        // The panel and the compiler read `Provider::validate`; a problem that only lived on a
        // private helper would never reach either.
        let mut p = reasoning_provider(Dialect::OpenAi, vec![Capability::Chat]);
        let mut m = model_entry("m1");
        m.reasoning_levels = vec![ReasoningLevel::Max];
        p.models = vec![m];

        let problems = p.validate();
        assert!(
            problems.iter().any(|p| p.contains("reasoning level")),
            "{problems:?}"
        );
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
    fn a_combo_step_can_route_to_another_combo_without_naming_a_provider() {
        let mut c = Combo::new("outer", ComboStep::from_combo("inner"));
        c.steps.push(ComboStep::new("p", "m"));
        // The target is resolved against the other combos by `Config::validate`, so the step must
        // not be reported as referencing an unknown provider here.
        assert!(
            c.validate(&[ProviderId::from("p")]).is_empty(),
            "{:?}",
            c.validate(&[ProviderId::from("p")])
        );
        assert!(c.steps[0].is_combo_target());
        assert!(!c.steps[1].is_combo_target());

        let blank = Combo::new("bad", ComboStep::from_combo("  "));
        assert!(blank
            .validate(&[])
            .iter()
            .any(|p| p.contains("empty combo target")));
    }

    #[test]
    fn a_combo_step_that_routes_to_a_combo_serializes_without_provider_or_model() {
        // The authored JSON stays readable: a combo target is one key, not two empty strings.
        let step: ComboStep =
            serde_json::from_value(serde_json::json!({ "combo": "inner" })).unwrap();
        assert_eq!(step.provider.as_str(), "");
        assert!(step.model.is_empty());
        assert_eq!(
            serde_json::to_value(&step).unwrap(),
            serde_json::json!({ "combo": "inner", "enabled": true, "weight": 0 })
        );
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

        // A pinned tier and a combo target change the upstream a step reaches, so each must
        // invalidate the digest the conductor uses to decide whether to recompile.
        let mut levelled = c.clone();
        levelled.steps[0].level = Some(crate::dialect::ReasoningLevel::High);
        assert_ne!(levelled.signature(), base);

        let mut chained = c.clone();
        chained.steps[0] = ComboStep::from_combo("nested");
        assert_ne!(chained.signature(), base);
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
    fn a_credit_that_is_not_a_finite_non_negative_dollar_amount_is_rejected() {
        let mut p = provider_with(1);
        for bad in [f64::NAN, f64::INFINITY, -0.01] {
            p.accounts[0].credit_usd = Some(bad);
            assert!(
                p.validate().iter().any(|s| s.contains("credit_usd")),
                "{bad} was accepted as a balance"
            );
        }
        // Zero is a declaration, not an absence: it means the operator knows the account is spent.
        p.accounts[0].credit_usd = Some(0.0);
        assert!(p.validate().iter().all(|s| !s.contains("credit_usd")));
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
    fn every_dialect_rejects_exactly_its_unsupported_capabilities() {
        // A literal table rather than a walk over the implementation: the question is which
        // combinations this build promises to serve, and the test should state that promise on its
        // own. `Capability::ALL` and `Dialect::ALL` keep the walk exhaustive in both directions.
        let new_capabilities = [
            Capability::AudioTranslation,
            Capability::ImagesEdits,
            Capability::ImagesVariations,
            Capability::Moderations,
            Capability::Video,
            Capability::Realtime,
            Capability::VectorStores,
            Capability::Containers,
            Capability::FineTuning,
            Capability::Evals,
            Capability::Search,
        ];
        let with_new = |existing: &[Capability]| -> Vec<Capability> {
            existing.iter().copied().chain(new_capabilities).collect()
        };
        let text_and_audio_free = [
            Capability::Embeddings,
            Capability::AudioTranscription,
            Capability::AudioSpeech,
            Capability::ImagesGenerations,
        ];
        let rejected: Vec<(Dialect, Vec<Capability>)> = vec![
            (Dialect::OpenAi, Vec::new()),
            (Dialect::MistralVibe, Vec::new()),
            (Dialect::Anthropic, with_new(&text_and_audio_free)),
            (Dialect::Zcode, with_new(&text_and_audio_free)),
            (Dialect::ClaudeCli, with_new(&text_and_audio_free)),
            (Dialect::Codex, with_new(&text_and_audio_free)),
            (Dialect::CommandCodeGo, with_new(&[Capability::Embeddings])),
            (
                Dialect::ModalVllm,
                with_new(&[
                    Capability::Responses,
                    Capability::Embeddings,
                    Capability::AudioTranscription,
                    Capability::AudioSpeech,
                    Capability::ImagesGenerations,
                ]),
            ),
            (Dialect::Google, with_new(&[])),
            (Dialect::GoogleAiStudio, with_new(&[])),
            (Dialect::Antigravity, with_new(&[])),
            (Dialect::Copilot, with_new(&[])),
            (Dialect::Cursor, with_new(&[])),
        ];

        assert_eq!(
            rejected.len(),
            Dialect::ALL.len(),
            "every dialect needs exactly one row"
        );
        for dialect in Dialect::ALL {
            let rows: Vec<_> = rejected.iter().filter(|(row, _)| *row == dialect).collect();
            assert_eq!(rows.len(), 1, "{dialect:?} must have exactly one row");
            let (_, unsupported) = rows[0];
            for capability in Capability::ALL {
                let mut provider = Provider::new("audit", dialect, "https://example.invalid/v1");
                provider.capabilities = vec![capability];
                let mismatches = provider.dialect_capability_mismatches();
                assert_eq!(
                    !mismatches.is_empty(),
                    unsupported.contains(&capability),
                    "{dialect:?} + {capability:?} should be {}: {mismatches:?}",
                    if unsupported.contains(&capability) {
                        "rejected"
                    } else {
                        "accepted"
                    }
                );
            }
        }
    }

    #[test]
    fn an_audio_only_provider_is_valid() {
        // Endpoint-specialised providers such as fish-audio and elevenlabs declare no chat
        // capability at all. That is a complete configuration, not an error: validation must not
        // assume that a provider has to serve `/chat/completions` to be worth saving.
        let mut provider = Provider::new("fish-audio", Dialect::OpenAi, "https://api.fish.audio");
        provider.capabilities = vec![Capability::AudioSpeech];
        provider.accounts.push(Account::new("default"));
        assert!(
            provider.validate().is_empty(),
            "an audio-only provider must validate cleanly: {:?}",
            provider.validate()
        );
        assert!(provider.supports(RouteKind::AudioSpeech));
        assert!(!provider.supports(RouteKind::ChatCompletions));
        assert!(!provider.supports(RouteKind::Models));
    }

    #[test]
    fn an_oauth_provider_without_a_usable_platform_is_flagged() {
        // The shape the buggy panel used to send: `oauth` with the platform box left empty.
        let mut p = Provider::new("cursor", Dialect::Cursor, "https://api2.cursor.sh");
        p.accounts.push(Account::new("default"));
        p.auth = AuthKind::OAuth {
            platform: String::new(),
        };
        let problems = p.validate();
        assert!(
            problems.iter().any(|s| s.contains("has no OAuth platform")),
            "an OAuth provider with no platform must be flagged: {problems:?}"
        );

        // An unregistered id is *not* a provider-level problem: the catalogue ships platforms
        // this build does not implement yet (`cline`) and documents them as such, so the provider
        // cannot tell a migration target from a typo. The control plane refuses those instead.
        p.auth = AuthKind::OAuth {
            platform: "not-a-platform".into(),
        };
        assert!(
            p.validate().iter().all(|s| !s.contains("OAuth platform")),
            "an unregistered platform must not be flagged here: {:?}",
            p.validate()
        );

        // A registered platform is accepted, so the check cannot fire on every OAuth provider.
        p.auth = AuthKind::OAuth {
            platform: "cursor".into(),
        };
        assert!(
            p.validate().iter().all(|s| !s.contains("OAuth platform")),
            "a registered platform must not be flagged: {:?}",
            p.validate()
        );
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
    fn quota_windows_accept_the_documented_windows_name() {
        // docs/08 shows the account example with "windows"; rejecting it would make the
        // documented example a parse error.
        let account: Account = serde_json::from_value(serde_json::json!({
            "id": "a",
            "windows": [
                { "name": "rpm", "kind": "rolling", "seconds": 60, "limit_requests": 500 }
            ]
        }))
        .expect("alias accepted");
        assert_eq!(account.quota.len(), 1);
        assert_eq!(account.quota[0].name.as_deref(), Some("rpm"));
        assert_eq!(account.quota[0].limit_requests, 500);

        let provider: Provider = serde_json::from_value(serde_json::json!({
            "id": "p",
            "dialect": "open-ai",
            "base_url": "https://example.com/v1",
            "windows": [
                { "seconds": 3600, "limit_requests": 100 }
            ]
        }))
        .expect("provider alias accepted");
        assert_eq!(provider.quota.len(), 1);
        assert_eq!(provider.quota[0].limit_requests, 100);
    }

    #[test]
    fn window_that_only_tracks_a_reset_constrains_nothing() {
        // A reset_only window is effective — it is shown and its reset is tracked — but it must
        // never make an account unusable: the operator wrote it to say "watch this clock".
        let reset_only = QuotaWindow {
            reset_only: true,
            ..QuotaWindow::default()
        };
        assert!(reset_only.is_effective());
        assert!(!reset_only.constrains());
        assert!(QuotaWindow::requests_per(3600, 1).constrains());
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
            allow_combos: Vec::new(),
            limit_requests: 0,
            limits: KeyLimits::default(),
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

    // ---------------------------------------------------------------- B2: new model metadata

    #[test]
    fn a_config_written_before_the_new_fields_still_loads_and_does_not_gain_keys() {
        // Every field B2 added is `#[serde(default)]` + `skip_serializing_if`, so an old file must
        // load unchanged *and* a round trip must not invent keys the operator never wrote. A
        // migration that silently rewrites every model entry would be a diff on every save.
        let json = serde_json::json!({
            "id": "gpt-6",
            "source": "manual",
            "dialect": "open-ai",
            "base_url": "https://example.test/v1",
            "models": [{ "id": "gpt-6-luna" }]
        });
        let p: Provider = serde_json::from_value(json).unwrap();
        let m = &p.models[0];
        assert!(
            m.reasoning_levels.is_empty(),
            "absent levels must default empty"
        );
        assert_eq!(m.default_reasoning_level, None);
        assert!(m.reasoning_wire.is_none());
        assert!(m.input_modalities.is_empty());
        assert!(m.pricing.is_none());

        let out = serde_json::to_value(&p).unwrap();
        let model = &out["models"][0];
        for absent in [
            "reasoning_levels",
            "default_reasoning_level",
            "reasoning_wire",
            "input_modalities",
            "pricing",
        ] {
            assert!(
                model.get(absent).is_none(),
                "round trip invented the key `{absent}`: {model}"
            );
        }
    }

    #[test]
    fn a_provider_defaults_to_account_scoped_cooldowns_and_parses_every_scope() {
        // The default is the behaviour before scopes existed: an untouched config keeps the
        // historical cooldown keys, and nothing changes shape on upgrade.
        let p: Provider = serde_json::from_value(serde_json::json!({
            "id": "openadapter",
            "dialect": "open-ai",
            "base_url": "https://example.test/v1"
        }))
        .unwrap();
        assert_eq!(p.cooldown_scope, CooldownScope::Account);
        assert_eq!(CooldownScope::default(), CooldownScope::Account);

        // A provider created in code carries the same default.
        assert_eq!(
            Provider::new("fresh", Dialect::OpenAi, "https://example.test/v1").cooldown_scope,
            CooldownScope::Account
        );

        // Every variant has one stable written form, and the parser accepts that form plus the
        // tolerant spellings an operator might type on a command line.
        let scopes = [
            (CooldownScope::Account, "account"),
            (CooldownScope::Model, "model"),
            (CooldownScope::Provider, "provider"),
        ];
        for (scope, text) in scopes {
            assert_eq!(scope.as_str(), text);
            assert_eq!(CooldownScope::parse(text), Some(scope));
            assert_eq!(
                serde_json::to_value(scope).unwrap(),
                serde_json::json!(text)
            );
            assert_eq!(
                serde_json::from_value::<CooldownScope>(serde_json::json!(text)).unwrap(),
                scope
            );
            assert_eq!(CooldownScope::parse(&format!(" {text} ")), Some(scope));
            assert_eq!(
                CooldownScope::parse(&text.to_ascii_uppercase()),
                Some(scope)
            );
        }
        assert_eq!(CooldownScope::parse("model_scope"), None);
        assert_eq!(CooldownScope::parse(""), None);
    }

    #[test]
    fn the_new_model_metadata_round_trips_through_serde() {
        let m: ModelEntry = serde_json::from_value(serde_json::json!({
            "id": "gemini-3.8-flash",
            "reasoning_levels": ["low", "high", "xhigh", "max"],
            "default_reasoning_level": "high",
            "reasoning_wire": {
                "high": { "thinking": { "type": "enabled", "budget_tokens": 131072 } }
            },
            "input_modalities": ["text", "image", "video"],
            "pricing": {
                "input_per_mtok": 0.15,
                "output_per_mtok": 0.6,
                "cache_read_per_mtok": 0.0375,
                "source": "synced"
            }
        }))
        .unwrap();

        assert_eq!(m.reasoning_levels.len(), 4);
        assert_eq!(m.default_reasoning_level, Some(ReasoningLevel::High));
        assert_eq!(
            m.reasoning_wire.as_ref().unwrap()[&ReasoningLevel::High]["thinking"]["budget_tokens"],
            131072
        );
        assert_eq!(
            m.input_modalities,
            vec![
                InputModality::Text,
                InputModality::Image,
                InputModality::Video
            ]
        );
        let pricing = m.pricing.as_ref().unwrap();
        assert_eq!(pricing.input_per_mtok, Some(0.15));
        assert_eq!(pricing.cache_read_per_mtok, Some(0.0375));
        assert_eq!(pricing.cache_write_per_mtok, None);
        assert_eq!(pricing.source, PricingSource::Synced);

        let back: ModelEntry = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn reasoning_wire_keys_use_the_canonical_level_spelling() {
        // `xhigh` not `x-high`: the map is written by the panel and read by the relay, and a key
        // that does not match `ReasoningLevel`'s serde form would silently never apply.
        let m: ModelEntry = serde_json::from_value(serde_json::json!({
            "id": "m",
            "reasoning_wire": { "xhigh": { "reasoning_effort": "high" } }
        }))
        .unwrap();
        assert!(m
            .reasoning_wire
            .as_ref()
            .unwrap()
            .contains_key(&ReasoningLevel::XHigh));
        let out = serde_json::to_value(&m).unwrap();
        assert!(out["reasoning_wire"].get("xhigh").is_some(), "{out}");
    }

    #[test]
    fn highest_level_substitutes_by_rank_not_declaration_order() {
        // The substitution the design allows instead of failing: `-max` on a model that tops out at
        // `xhigh` reroutes there. The list is deliberately out of order, because an operator's list
        // need not be sorted and `xhigh` sorts below `max` numerically but above `ultra` by rank.
        let levels = [
            ReasoningLevel::XHigh,
            ReasoningLevel::Low,
            ReasoningLevel::None,
            ReasoningLevel::Ultra,
        ];
        assert_eq!(highest_level(&levels), Some(ReasoningLevel::Ultra));
        assert_eq!(
            highest_level(&[ReasoningLevel::Minimal]),
            Some(ReasoningLevel::Minimal)
        );
        assert_eq!(highest_level(&[]), None);
    }

    #[test]
    fn default_level_falls_through_to_the_configured_default() {
        let mut m: ModelEntry = serde_json::from_value(serde_json::json!({ "id": "m" })).unwrap();
        assert_eq!(m.default_level(ReasoningLevel::High), ReasoningLevel::High);
        m.default_reasoning_level = Some(ReasoningLevel::Low);
        assert_eq!(m.default_level(ReasoningLevel::High), ReasoningLevel::Low);
    }

    #[test]
    fn prewarm_is_off_for_a_new_provider_and_on_only_when_asked() {
        // I1: adding a provider must open no startup connection.
        let p = Provider::new("p", Dialect::OpenAi, "https://example.test/v1");
        assert!(!p.prewarm, "a new provider must not prewarm");

        let parsed: Provider = serde_json::from_value(serde_json::json!({
            "id": "p", "dialect": "open-ai", "base_url": "https://example.test/v1"
        }))
        .unwrap();
        assert!(!parsed.prewarm, "an absent `prewarm` must mean false");

        let asked: Provider = serde_json::from_value(serde_json::json!({
            "id": "p", "dialect": "open-ai", "base_url": "https://example.test/v1",
            "prewarm": true
        }))
        .unwrap();
        assert!(asked.prewarm, "an explicit `true` must be honoured");
    }

    #[test]
    fn client_key_limits_are_skipped_when_nothing_is_capped() {
        // The common case is no limits at all, and it must not add a key to every saved config.
        let key = ClientKey {
            id: crate::ids::ClientKeyId::from_raw("k1"),
            label: "k".into(),
            key_hash: "h".into(),
            prefix: None,
            enabled: true,
            allow_models: Vec::new(),
            allow_routes: Vec::new(),
            allow_combos: Vec::new(),
            limit_requests: 0,
            limits: KeyLimits::default(),
            note: None,
        };
        assert!(key.limits.is_empty());
        let out = serde_json::to_value(&key).unwrap();
        assert!(out.get("limits").is_none(), "{out}");

        let capped = ClientKey {
            limits: KeyLimits {
                session_5h: Some(KeyLimit {
                    tokens: 100_000,
                    requests: 0,
                }),
                ..KeyLimits::default()
            },
            ..key
        };
        assert!(!capped.limits.is_empty());
        let out = serde_json::to_value(&capped).unwrap();
        assert_eq!(out["limits"]["session_5h"]["tokens"], 100_000);
        assert_eq!(out["limits"]["session_5h"]["requests"], 0);
        assert!(out["limits"].get("weekly_1w").is_none(), "{out}");

        let back: ClientKey = serde_json::from_value(out).unwrap();
        assert_eq!(back, capped);
    }

    #[test]
    fn an_inert_limit_is_recognised_and_an_unknown_price_is_not_free() {
        assert!(KeyLimit::default().is_inert());
        assert!(!KeyLimit {
            tokens: 1,
            requests: 0
        }
        .is_inert());

        // `None` rates must never read as $0.00.
        assert!(!PricingPlan::default().has_any_rate());
        assert!(
            PricingPlan {
                output_per_mtok: Some(0.0),
                ..PricingPlan::default()
            }
            .has_any_rate(),
            "a genuine zero rate is still a known rate"
        );
    }

    #[test]
    fn pricing_sources_rank_so_a_manual_price_beats_a_synced_one_which_beats_the_bundle() {
        // "Manual override wins" (I18) is the whole reason the source is stored.
        assert!(PricingSource::Bundled < PricingSource::Synced);
        assert!(PricingSource::Synced < PricingSource::Manual);
        assert_eq!(PricingSource::default(), PricingSource::Bundled);
        let winner = [
            PricingSource::Synced,
            PricingSource::Manual,
            PricingSource::Bundled,
        ]
        .into_iter()
        .max();
        assert_eq!(winner, Some(PricingSource::Manual));
    }
}
