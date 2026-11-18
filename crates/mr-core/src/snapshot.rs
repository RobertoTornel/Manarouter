//! The compiled routing snapshot: the flat, pre-resolved routing table the relay reads on every
//! request.
//!
//! ## Why there are two representations
//!
//! The authored configuration in [`crate::config`] is rich and hand-editable: it may hold disabled
//! providers, disabled accounts, hidden models, aliases that chain, hedges whose participant list
//! is implicit, and steps that point at providers which no longer exist. None of that should be
//! interpreted while a request is in flight. The compiler in this module flattens the authored
//! form once, at the moment the operator saves, into the layout below.
//!
//! The relay's contract is deliberately narrow: it loads a snapshot, swaps it in through a
//! [`SnapshotHandle`], and from then on asks one question per request — "what does `model` mean?"
//! — which is one hash lookup and one string comparison.
//!
//! ## What "no per-request parse" does and does not buy
//!
//! It **does** buy that every interpretation decision is made here and never again. Account order
//! is resolved (disabled accounts dropped, the rest sorted by priority), hidden models are gone,
//! hedge participant indices are remapped onto the surviving steps, alias chains are followed to a
//! final target, and the lookup index that answers `provider/model`, bare model ids and combo names
//! is built once. The request path is an atomic load of an `Arc` plus a hash lookup; it never
//! walks a `Config`, sorts anything, or resolves an alias chain.
//!
//! It does **not** buy literal zero-copy reads out of a memory-mapped file. [`RoutingSnapshot::decode`]
//! validates the frame and then builds owned `String`s and `Vec`s, so a decoded snapshot lives on
//! the ordinary heap. The on-disk form is still a versioned, checksummed, string-deduplicated
//! binary frame, because the same bytes have to cross a process boundary and must fail closed when
//! truncated or corrupted; but a reader should not expect the in-memory snapshot to borrow from
//! the file it came from. Truly zero-copy would require a layout-stable `repr(C)` schema and an
//! mmap whose lifetime is tied to the file, and the portability and validation costs of that are
//! not worth paying for a table that is read once per reload. This is an honest trade, not an
//! accident: the earlier ambition of reading fields straight out of shared memory was given up in
//! favour of a format that can be validated and rejected cleanly.
//!
//! ## The on-disk frame
//!
//! ```text
//! magic[8] | version u32 | digest u64 | payload_len u32 | checksum u64 | payload[..]
//! ```
//!
//! `checksum` is FNV-1a over `version || digest || payload`, so a single flipped byte anywhere
//! meaningful is caught by [`verify_header`] before any structure is built. `payload_len` is
//! separately checked against the actual byte count, which is what turns a truncated file into a
//! precise "truncated" error rather than a successful parse of a half-valid routing table.
//!
//! The payload begins with a string table and refers to every string by index. Provider ids, base
//! URLs, header names, model ids and auth-kind parameter names repeat heavily across the table, and
//! the string table is what keeps the frame from being dominated by the same slug copied a hundred
//! times. A step therefore stores only its provider index and model; every other per-step field is
//! reconstructed from the provider it points at, so there is exactly one copy of each base URL and
//! timeout in the compiled form.
//!
//! ## Precedence and the lookup index
//!
//! One requested string can match several things at once: an alias `from`, a combo id, a combo
//! name, a `provider/model` pair, and a bare model id might all be the same text. The index is a
//! `HashMap<u64, u32>` keyed on `fnv1a64` of the ASCII-lowercased key, where the value is the head
//! of a chain of [`IndexEntry`] records. Every entry carries its original (folded) key and a
//! precedence rank, and a lookup walks the chain comparing the **full string** — a hash collision
//! therefore costs one extra comparison, it never routes to the wrong combo. When several entries
//! genuinely share a key, the lowest rank wins: alias, then combo id, then combo name, then
//! `provider/model`, then bare model id.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::{Config, ProxyConfig, ProxyStrategy};
use crate::dialect::{
    AuthKind, Capability, ComboStrategy, Dialect, InputModality, KeyStrategy, MidstreamPolicy,
    ReasoningLevel, WinnerFailMode, WinnerMetric,
};
use crate::error::{Error, Result};
use crate::ids::{fnv1a64, AccountId, ClientKeyId, ComboId, Millis, ProviderId};
use crate::model::{
    Account, ClientKey, ComboStep, CompressionMode, CompressionSpec, CooldownScope,
    Http2Preference, KeyLimit, KeyLimits, ModelEntry, PricingPlan, PricingSource, Provider,
};
use crate::secret::account_secret_id;
use crate::{SNAPSHOT_MAGIC, SNAPSHOT_VERSION};

/// Bytes before the payload in every frame: magic, version, digest, payload length and checksum.
const HEADER_LEN: usize = 32;

/// Sentinel used in the payload for "no string", since a real string index is never `u32::MAX`.
const NO_STRING: u32 = u32::MAX;

/// How many alias hops [`RoutingSnapshot::resolve`] will follow before giving up.
///
/// Alias chains are flattened at compile time, so a runtime chain longer than one hop means the
/// snapshot was built by something else or is corrupt. The bound exists so a hand-edited or
/// malicious file degrades to `NotFound` instead of hanging the relay.
const MAX_ALIAS_HOPS: usize = 8;

const RANK_ALIAS: u8 = 0;
const RANK_COMBO_ID: u8 = 1;
const RANK_COMBO_NAME: u8 = 2;
const RANK_QUALIFIED: u8 = 3;
// Ranks 4 and below are unused since Feature 3 removed the bare-model fallback. They are left
// unassigned rather than renumbered so a value written by an older snapshot is *rejected* by
// `link_entries` instead of silently acquiring a new meaning.

/// The lookup keys that are *derived* from a model id by appending a level suffix.
///
/// They rank between a combo name and the model's own id, which is exactly "a real model id wins
/// over a stripped suffix": `antigravity/gemini-3.8-flash-low` is a model in its own right, so the
/// rank [`RANK_QUALIFIED`] entry for it is what answers, and the entry derived from
/// `gemini-3.8-flash` plus the level `low` is only consulted when there is no such model.
const RANK_QUALIFIED_LEVEL: u8 = 5;
/// The highest rank any index entry may carry; `link_entries` rejects anything above it.
const RANK_MAX: u8 = RANK_QUALIFIED_LEVEL;

/// The level-name spellings that may appear as a `-<suffix>` on a model name.
///
/// Only the canonical spellings [`ReasoningLevel::parse`] accepts *and* that this table lists. The
/// table is what makes suffix detection allocation-free and, more importantly, predictable: a model
/// called `foo-bar` must never be read as a model `foo` at a level `bar`, and it is not, because
/// `bar` is not in this list.
const LEVEL_SUFFIXES: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "ultra", "max",
];

/// The level named by a `-<suffix>` at the end of `name`, if there is one.
///
/// `name` is a model id, not a whole request name, so `deepseek/deepseek-v4.1-flash-max` yields
/// `max` and `some/deepseek` yields `None`.
fn level_suffix(name: &str) -> Option<ReasoningLevel> {
    let (_, suffix) = name.rsplit_once('-')?;
    LEVEL_SUFFIXES
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(suffix))
        .and_then(|candidate| ReasoningLevel::parse(candidate))
}

/// A compiled, immutable routing table.
///
/// Everything the relay needs to answer "what does this model name mean" is here, already
/// resolved. The snapshot is normally read through a [`SnapshotHandle`] so that a recompile can be
/// swapped in without stopping traffic.
///
/// The fields are private on purpose: the lookup index and the entry chains have invariants that
/// only the compiler and the decoder know how to establish, and the relay should read plans
/// through the accessors rather than mutate the table around them.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutingSnapshot {
    /// Copied from [`Config::routing_digest`] so a relay can tell "the file changed" from "the
    /// bytes changed" without comparing whole snapshots.
    digest: u64,
    /// When the compiler ran. Kept for the panel and for reload logging; never used for routing.
    built_at: Millis,
    /// Defaults used to reroute inputs unsupported by the initially selected model.
    routing_defaults: crate::config::RoutingDefaults,
    /// Providers in configured order, disabled ones removed. This order is also the order of a
    /// bare-model lookup's tie-break.
    providers: Vec<ProviderPlan>,
    /// Enabled combos only, in configured order.
    combos: Vec<ComboPlan>,
    /// Aliases with their chains already flattened to a final target.
    aliases: Vec<AliasPlan>,
    /// Enabled client keys with their rolling quota windows, so the relay enforces a key's budget
    /// without re-reading the authored configuration on every request.
    keys: Vec<KeyPlan>,
    /// Usable proxies, in configured order. Disabled proxies are dropped here rather than skipped
    /// at dial time, because "configured but never dialled" is a decision, not a runtime check.
    proxies: Vec<ProxyPlan>,
    /// How a provider or account with no explicit proxy override picks one.
    proxy_strategy: ProxyStrategy,
    /// Hash bucket heads: `fnv1a64(folded key)` to an index in `index_entries`.
    index: HashMap<u64, u32>,
    /// Every lookup key, in insertion order. `next` chains entries that share a bucket.
    index_entries: Vec<IndexEntry>,
}

impl Default for RoutingSnapshot {
    /// An empty snapshot with a zero digest and no routes.
    ///
    /// A default is never a routing decision: it is a placeholder for a handle that has not been
    /// given a compiled table yet, so every lookup on it returns `Resolution::NotFound`.
    fn default() -> Self {
        Self {
            digest: 0,
            built_at: Millis::from_millis(0),
            routing_defaults: crate::config::RoutingDefaults::default(),
            providers: Vec::new(),
            combos: Vec::new(),
            aliases: Vec::new(),
            keys: Vec::new(),
            proxies: Vec::new(),
            proxy_strategy: ProxyStrategy::default(),
            index: HashMap::new(),
            index_entries: Vec::new(),
        }
    }
}

/// One lookup key and what it resolves to.
///
/// `key` is stored on the entry rather than being implied by the hash because a hash collision must
/// be verified by comparing the string. `rank` decides precedence when several entries share the
/// same key, and `next` walks the bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    /// The lookup key, ASCII-lowercased at compile time.
    key: String,
    /// Lower wins. See the `RANK_*` constants.
    rank: u8,
    /// What this key resolves to.
    target: IndexTarget,
    /// The next entry in the same hash bucket, if any.
    next: Option<u32>,
}

/// The destination of an index entry.
#[derive(Debug, Clone, PartialEq, Eq)]
enum IndexTarget {
    /// Another alias hop. Should not occur after compile-time flattening, but is handled.
    Alias { alias: u32 },
    /// A combo, by index into the combo list. `level` is `Some` only when the client named a tier.
    Combo { combo: u32, level: Option<u8> },
    /// A provider model, by provider index and index into that provider's step list.
    ///
    /// `level` is what the request will actually run at; `requested` is what the name asked for.
    /// When they differ, the requested level was not one the model offers and the model's highest
    /// available level was substituted, which is the one substitution the design allows instead of
    /// failing. Both are stored so that comparison is a field read rather than a re-parse of the
    /// name that produced the entry.
    Step {
        provider: u32,
        step: u32,
        level: u8,
        requested: u8,
    },
}

/// A placeholder used when a combo names a model the provider no longer carries.
///
/// `Config::validate` reports the dangling step, but the compiler must still produce *a* step: it
/// is a static placeholder so the combo keeps working if the model comes back, and it carries no
/// reasoning levels and no modalities, which is the honest answer for a model that does not exist.
static UNKNOWN_MODEL: std::sync::LazyLock<ModelEntry> = std::sync::LazyLock::new(|| ModelEntry {
    id: String::new(),
    source: crate::model::ModelSource::Manual,
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

/// The highest rank set in a level bitmask, or `None` when it is empty.
fn highest_rank(mask: u32) -> u8 {
    ReasoningLevel::ALL
        .into_iter()
        .rev()
        .find(|level| mask & (1u32 << level.rank()) != 0)
        .map(|level| level.rank())
        .unwrap_or(ReasoningLevel::None.rank())
}

/// A compiled alias: the name a client sends mapped to its final target.
///
/// The `to` stored here is the end of the chain, not the next hop. Flattening at compile time means
/// the relay never pays for a chain walk, and a chain that loops is rejected while compiling rather
/// than bounded at request time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasPlan {
    /// The name the client sends.
    pub from: String,
    /// The final target, after following any alias-to-alias hops.
    pub to: String,
    /// Whether this alias was generated from a naming convention rather than authored by hand.
    pub generated: bool,
}

/// A compiled combo: an ordered fallback chain with all of its policy pre-resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct ComboPlan {
    /// Stable identifier.
    pub id: ComboId,
    /// The name clients call. Indexed alongside the id at a lower precedence.
    pub name: String,
    /// How the steps are traversed.
    pub strategy: ComboStrategy,
    /// The survivable, enabled steps in the operator's order. Disabled steps and steps pointing at
    /// disabled providers never reach the snapshot.
    pub steps: Vec<StepPlan>,
    /// The hedged race, with participant indices already remapped onto `steps`.
    pub hedge: HedgePlan,
    /// Sticky-success expiry after inactivity, in seconds; zero disables it.
    pub sticky_success_window_secs: u32,
    /// What to do when the committed model dies after bytes have been sent.
    pub midstream_policy: MidstreamPolicy,
    /// Attempts allowed before the combo gives up.
    pub max_retries: u8,
    /// Delay between attempts of the same step.
    pub retry_delay_ms: u64,
    /// Whether a winning step stays sticky for a conversation.
    pub affinity: bool,
    /// Off-path compression configuration. Carried here so the relay never reads the authored
    /// config to find out whether to run a transform.
    pub compression: CompressionSpec,
    /// `fnv1a64` over the routing-relevant fields of this plan.
    ///
    /// This is the cheap "did anything I care about change" check. It is computed from the compiled
    /// plan, not from [`crate::model::Combo::signature`], so a change to affinity or compression —
    /// which the authored signature does not cover — still moves it.
    pub signature: u64,
}

/// One attemptable upstream: a provider and a model, with everything needed to issue the request
/// already copied in.
///
/// The step is self-contained on purpose. The relay holds a `&StepPlan` and a credential it looked
/// up by [`AccountPlan`] reference; it never has to chase back into the provider to find a timeout
/// or a base URL.
#[derive(Debug, Clone, PartialEq)]
pub struct StepPlan {
    /// Index into [`RoutingSnapshot::providers`].
    pub provider_index: u32,
    /// The provider's slug, copied here so a step can be logged without another lookup.
    pub provider_id: ProviderId,
    /// The model id as the upstream expects it. **Without** any `-<level>` suffix: the suffix
    /// selects [`StepPlan::level`], and sending it upstream would name a model that does not exist.
    pub model: String,
    /// The API shape this provider speaks.
    pub dialect: Dialect,
    /// How the credential is attached.
    pub auth: AuthKind,
    /// Base URL including any version path.
    pub base_url: String,
    /// How long to wait for the upstream connection to establish.
    pub connect_timeout_ms: u64,
    /// How long to wait for the first response byte.
    pub ttfb_timeout_ms: u64,
    /// How long the stream may go silent before it is treated as stalled.
    pub idle_timeout_ms: u64,
    /// Total time allowed for the whole request.
    pub total_timeout_ms: u64,
    /// The reasoning level this resolution was asked for, as `ReasoningLevel::rank`.
    pub level: u8,
    /// Which levels this model offers, as a bitmask over [`ReasoningLevel::rank`].
    ///
    /// Zero means the model advertises none, which is a real answer: it is why a `-high` suffix on
    /// such a model is not a `-<level>` at all but part of the model's own id.
    pub levels: u32,
    /// The level used when a request names this model with no suffix.
    pub default_level: u8,
    /// The JSON fragment to merge into the upstream body at each level, indexed by level rank.
    ///
    /// `None` means "send nothing for this level" — which is the correct answer for `none`, for a
    /// dialect that has nothing to say, and for a level the model does not offer (that combination
    /// cannot be resolved to, but the encoding stays honest rather than fabricating a fragment).
    pub reasoning_json: [Option<String>; 8],
    /// Which input types this model accepts, as a bitmask over
    /// [`InputModality`](crate::dialect::InputModality)'s discriminant.
    pub input_modalities: u32,
    /// How this model is priced, when anything is known. Compiled in so a cost estimate on the
    /// request path never reads configuration or the bundled table.
    pub pricing: Option<PricingPlan>,
    /// Declared or discovered context window. Provider-level max_context overrides this value.
    pub context_length: Option<u32>,
    /// Which of the provider's accounts this step may use, resolved from the authored selectors.
    ///
    /// `None` means every account, which is what a step that names no selection means and what
    /// every direct `provider/model` resolution gets. `Some` carries account ids in the provider's
    /// own order, so the relay filters candidates by identity and never parses a tag at request
    /// time. `Some(vec![])` is a real state: the selectors matched only accounts that are disabled,
    /// and the step is unusable rather than silently widened back to all of them.
    pub accounts: Option<Vec<AccountId>>,
}

impl StepPlan {
    /// Whether `level` is one this model offers.
    #[inline]
    pub fn supports_level(&self, level: ReasoningLevel) -> bool {
        self.levels & (1u32 << level.rank()) != 0
    }

    /// The reasoning levels this model offers, ascending.
    pub fn levels(&self) -> Vec<ReasoningLevel> {
        ReasoningLevel::ALL
            .into_iter()
            .filter(|level| self.supports_level(*level))
            .collect()
    }

    /// Whether this model accepts `modality`.
    #[inline]
    pub fn accepts_modality(&self, modality: crate::dialect::InputModality) -> bool {
        self.input_modalities & (1u32 << modality.as_u8()) != 0
    }

    /// The reasoning fragment to send at `level`, if any.
    ///
    /// `None` means "send no reasoning field", which is the correct answer for `none`, for a
    /// dialect with nothing to say, and for a level this model does not offer. The level is passed
    /// in rather than read from [`StepPlan::level`] because that field is only the *default*: a
    /// request that named a tier resolves to a different level, and reading the default here would
    /// silently send the wrong one.
    pub fn reasoning_fragment_for(&self, level: ReasoningLevel) -> Option<&str> {
        self.reasoning_json[level.rank() as usize].as_deref()
    }

    /// The reasoning fragment to send at this step's default level, if any.
    pub fn reasoning_fragment(&self) -> Option<&str> {
        ReasoningLevel::from_u8(self.level).and_then(|level| self.reasoning_fragment_for(level))
    }
}

/// A compiled provider: dialect, credential attachment, ordered accounts and per-route settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderPlan {
    /// The operator-chosen slug.
    pub id: ProviderId,
    /// Human label for the panel.
    pub label: Option<String>,
    /// Which API shape this provider speaks.
    pub dialect: Dialect,
    /// Base URL including any version path.
    pub base_url: String,
    /// How to attach a credential.
    pub auth: AuthKind,
    /// How to choose among `accounts`.
    pub key_strategy: KeyStrategy,
    /// What a refusal takes out of rotation: this credential, this model on it, or this model across
    /// every credential of the provider. See [`CooldownScope`].
    pub cooldown_scope: CooldownScope,
    /// Enabled accounts, ordered by ascending priority (ties keep their authored order).
    ///
    /// The relay reads this order directly; it must not re-sort, because the authored array order
    /// is the tie-break the operator sees in the panel.
    pub accounts: Vec<AccountPlan>,
    /// The [`ProxyPlan::id`] this provider's traffic must use, or `None` when the operator set no
    /// override.
    ///
    /// An account-level override takes precedence; when neither is set the snapshot's
    /// [`ProxyStrategy`] chooses. The id is kept **verbatim** rather than resolved to the set of
    /// dialable proxies, because "no override" and "an override that names a proxy which is
    /// currently disabled" are different situations and must not compile to the same value: the
    /// first is answered by the strategy, the second by a direct connection.
    ///
    /// A reference to a proxy that does not exist at all is a configuration error, reported by
    /// [`Config::validate`] before the relay ever starts.
    pub proxy: Option<String>,
    /// Routes this provider can serve.
    pub capabilities: Vec<Capability>,
    /// Connection-establishment timeout.
    pub connect_timeout_ms: u64,
    /// First-byte timeout.
    pub ttfb_timeout_ms: u64,
    /// Stream-idle timeout.
    pub idle_timeout_ms: u64,
    /// Whole-request timeout.
    pub total_timeout_ms: u64,
    /// Extra headers for every upstream request.
    pub headers: BTreeMap<String, String>,
    /// Per-route path overrides.
    pub route_overrides: BTreeMap<String, String>,
    /// Whether to negotiate HTTP/2.
    pub http2: Http2Preference,
    /// Whether to keep idle connections warm.
    pub prewarm: bool,
    /// KV-pool budget for self-hosted providers.
    pub kv_pool_tokens: Option<u64>,
    /// Maximum declared context tokens.
    pub max_context: Option<u64>,
    /// Maximum concurrent requests accepted by the endpoint.
    pub max_concurrent: Option<u32>,
    /// Price per second in USD. When set, GPU-second billing is enabled.
    pub price_per_second: Option<f64>,
    /// Request timeout used to accommodate cold starts.
    pub cold_start_ttfb_timeout_ms: Option<u64>,
    /// One step per visible model, so a `provider/model` or bare-model request can be served
    /// without a combo. Hidden models are absent.
    pub steps: Vec<StepPlan>,
    /// Model ids the operator marked hidden: present on the provider, absent from `steps`, and
    /// never routed against.
    ///
    /// The list exists only so a miss can distinguish "no such model" from "that model is hidden",
    /// which are fixed differently — a typo versus a checkbox. It carries no credential and no
    /// routing decision.
    pub hidden_models: Vec<String>,
}

/// A compiled account: scheduling metadata plus a reference to the credential, never its value.
///
/// `secret_ref` names vault material; `oauth_ref` names an OAuth metadata record in the Store.
/// The compiler resolves the conventional [`account_secret_id`] fallback for static credentials;
/// the relay resolves OAuth metadata to its vault reference at request time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountPlan {
    /// Stable identifier.
    pub id: AccountId,
    /// Human label.
    pub label: Option<String>,
    /// Vault id for a static credential, or `None` for a purely OAuth account.
    pub secret_ref: Option<String>,
    /// Vault id for a stored OAuth credential set, if this account uses one.
    pub oauth_ref: Option<String>,
    /// Lower is tried first under [`KeyStrategy::Priority`].
    pub priority: i32,
    /// Relative weight, reserved for weighted selection.
    pub weight: u32,
    /// Maximum simultaneous in-flight requests. Zero means unlimited.
    pub max_concurrent: u32,
    /// If non-empty, only these model ids may use this account.
    pub models_allow: Vec<String>,
    /// If non-empty, these model ids may not use this account. Always wins over `models_allow`.
    pub models_deny: Vec<String>,
    /// The [`ProxyPlan::id`] this account's traffic must use, or `None` when the operator set no
    /// override here.
    ///
    /// The override is per *account* as well as per provider because a single credential may be
    /// region-locked: moving just that account behind a proxy must not move the whole provider.
    /// `None` is not "direct": it means the provider's override applies next, and if that is also
    /// absent the snapshot's [`ProxyStrategy`] decides.
    ///
    /// The id is kept verbatim, for the same reason as [`ProviderPlan::proxy`].
    pub proxy: Option<String>,
    /// Values for the named placeholders in the provider's base URL.
    ///
    /// Compiled from [`crate::model::Account::placeholders`] and carried here because the relay
    /// fills the placeholders in at request time, once this account has been chosen. Keeping it
    /// per account is the whole point: one provider, several accounts, several values.
    pub placeholders: std::collections::BTreeMap<String, String>,
    /// Declared remaining credit in **US cents**, rounded from
    /// [`crate::model::Account::credit_usd`].
    ///
    /// Cents rather than dollars so the fine-tuning floor is an exact integer comparison and this
    /// plan stays `Eq`: a `f64` in a compiled snapshot would make `AccountPlan` only `PartialEq`
    /// for a number the router never spends. `None` means the operator declared nothing; it is not
    /// a zero balance. A declared zero is `Some(0)`.
    pub credit_cents: Option<u64>,
    /// The declared quota windows that can stop this account, already resolved against the
    /// provider's defaults.
    ///
    /// Resolved, not authored: an account with windows of its own uses those, and an account
    /// without them inherits the provider's. Doing it here means the request path never has to know
    /// which level a window came from, and the two levels cannot be applied inconsistently by
    /// different callers.
    ///
    /// Only windows that constrain something are kept — a window with every limit at zero, or one
    /// marked [`crate::model::QuotaWindow::reset_only`], is a reset clock rather than a limit and
    /// is carried by the configuration, not by the data plane. A window whose length is not
    /// positive is dropped too: there is no interval to count over, and enforcement cannot be
    /// derived from it.
    pub quota: Vec<crate::model::QuotaWindow>,
}

/// A compiled client key: everything the relay needs to authenticate a request and charge it to
/// the right rolling window.
///
/// The key itself is never here — [`Self::key_hash`] is the stored SHA-256, exactly as the config
/// holds it, so a snapshot file is no more sensitive than the config file it was compiled from.
///
/// The quota windows are compiled in because enforcement is a per-request hot path: reading them
/// out of the authored configuration would mean either re-parsing JSON per request or keeping a
/// second copy of the config alive in the data plane, and both defeat the point of a snapshot. Only
/// enabled keys appear; every optional window is preserved exactly as authored, so the relay can
/// distinguish an absent window from an explicitly configured uncapped window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPlan {
    /// Stable identifier, used for usage rows and log fields.
    pub id: ClientKeyId,
    /// Human label, for the panel and for logs.
    pub label: String,
    /// Lowercase hex SHA-256 of the key. Never the key.
    pub key_hash: String,
    /// Short non-secret prefix kept for display, for example `mr-3f9a`.
    pub prefix: Option<String>,
    /// If non-empty, the key may only call these model ids (globs allowed).
    pub allow_models: Vec<String>,
    /// If non-empty, the key may only call these routes.
    pub allow_routes: Vec<String>,
    /// Flat, non-rolling request budget. Zero means unlimited.
    ///
    /// Kept alongside [`Self::limits`] because the two are both enforced when both are set; the
    /// flat budget predates the rolling windows and existing keys still carry it.
    pub limit_requests: u64,
    /// Rolling token and request caps, by window.
    pub limits: KeyLimits,
}

/// A compiled upstream proxy: where to dial, and the vault reference for its password.
///
/// The password is a [`Self::secret_ref`] for the same reason an account credential is: a snapshot
/// file that leaks must cost the operator a hostname, not a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyPlan {
    /// Stable id, referenced by [`ProviderPlan::proxy`] and [`AccountPlan::proxy`].
    pub id: String,
    /// Human label for the panel.
    pub label: Option<String>,
    /// The endpoint, including its scheme.
    pub url: String,
    /// Username, when the proxy authenticates. Not a secret.
    pub username: Option<String>,
    /// Reference into the secret vault for the password, when the proxy authenticates.
    pub secret_ref: Option<String>,
    /// Lower sorts first under [`ProxyStrategy::Priority`].
    pub priority: i32,
}

/// A compiled hedge: who races, and how the race is resolved.
///
/// Participant indices have been remapped from the authored step list onto
/// [`ComboPlan::steps`](ComboPlan), and a race with fewer than two surviving participants is
/// reported as disabled rather than being executed with the machinery and none of the benefit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HedgePlan {
    /// Whether the race runs at all.
    pub enabled: bool,
    /// Indices into the owning combo's `steps`, in priority order.
    pub participants: Vec<u16>,
    /// The eligibility window in milliseconds. Not a timeout.
    pub window_ms: u16,
    /// What counts as having replied.
    pub winner_metric: WinnerMetric,
    /// What to do when the winner dies.
    pub on_winner_fail: WinnerFailMode,
    /// Whether losers are cancelled once a winner is committed.
    pub cancel_losers: bool,
    /// Extra grace for a lower-priority participant before it is abandoned.
    pub loser_grace_ms: u16,
}

/// What a requested model name resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution<'a> {
    /// A combo matched; the relay should walk its steps.
    ///
    /// `level` is `Some` only when the client asked for a tier **by name** (`combo-name-high`).
    /// `None` means the name carried no suffix, and each step then runs at its own model's default
    /// level — forcing the configuration default onto every step would override a per-model
    /// `default_reasoning_level` the operator set deliberately.
    Combo {
        plan: &'a ComboPlan,
        level: Option<u8>,
    },
    /// A concrete provider/model matched; the relay should call it directly, at `level`.
    ///
    /// `substituted` is `true` when the level asked for was not one the model offers and the
    /// model's highest available level was used instead. The relay logs it and the panel shows it;
    /// nothing else about the resolution changes.
    Direct {
        step: &'a StepPlan,
        level: u8,
        substituted: bool,
    },
    /// Nothing matched, with a reason specific enough to hand to the operator.
    ///
    /// The reason is what makes a 404 actionable: "no combo, alias or model matches" is true but
    /// useless when what the operator actually typed was a bare model id that used to work, and the
    /// answer they need is "names must be `provider/model`, or a combo or alias".
    NotFound { reason: std::borrow::Cow<'a, str> },
}

impl<'a> Resolution<'a> {
    /// A `NotFound` with a reason that is already a `String`.
    fn not_found(reason: String) -> Self {
        Resolution::NotFound {
            reason: reason.into(),
        }
    }
}

/// The relay's hot-swap handle for the compiled snapshot.
///
/// The handle is `Clone` and shares one [`ArcSwap`], so every worker in the process observes the
/// same table and sees an install at the same moment.
#[derive(Clone)]
pub struct SnapshotHandle {
    inner: Arc<ArcSwap<RoutingSnapshot>>,
}

impl SnapshotHandle {
    /// Wrap a freshly compiled snapshot in a swappable handle.
    pub fn new(initial: RoutingSnapshot) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(initial)),
        }
    }

    /// The snapshot currently installed.
    ///
    /// This is the hot path: one atomic load plus a refcount increment. It performs **no**
    /// allocation and clones no routing data, and it must stay that way. If a future change finds
    /// itself wanting to transform or filter here, that work belongs in the compiler instead.
    pub fn current(&self) -> Arc<RoutingSnapshot> {
        self.inner.load_full()
    }

    /// Install a newly compiled snapshot, visible to every worker immediately.
    ///
    /// Readers already holding a reference to the previous snapshot keep it until they drop it, so
    /// an in-flight request always finishes against a consistent table.
    pub fn install(&self, next: RoutingSnapshot) {
        self.inner.store(Arc::new(next));
    }

    /// Load a snapshot from disk, install it, and return its digest.
    ///
    /// Nothing is installed if the file cannot be read or decoded, so a corrupt reload leaves the
    /// relay serving the previous table rather than failing.
    pub fn reload_from(&self, path: &Path) -> Result<u64> {
        let next = RoutingSnapshot::load_from(path)?;
        let digest = next.digest();
        self.install(next);
        Ok(digest)
    }
}

impl RoutingSnapshot {
    /// Compile an authored configuration into a snapshot.
    ///
    /// This is the only place that interprets routing configuration. It removes disabled providers,
    /// accounts and combos, drops hidden models, orders accounts by priority, flattens alias
    /// chains, remaps hedge participants and builds the lookup index.
    ///
    /// Failure modes are deliberate:
    ///
    /// * A combo whose routable steps all name a provider that does not exist is an
    ///   `Error::Snapshot` naming the combo. That is an operator typo which would otherwise fail at
    ///   request time.
    /// * A combo whose steps are merely disabled, or point only at providers the operator has
    ///   disabled, is silently omitted. Disabling a provider is a deliberate act and must not make
    ///   the whole snapshot fail to compile.
    /// * An alias cycle is an `Error::Snapshot` naming the alias, because a chain that never
    ///   terminates has no correct flattened target.
    pub fn compile(config: &Config) -> Result<Self> {
        let built_at = Millis::now();
        let digest = config.routing_digest();

        // Proxies and keys are compiled before providers so that a provider or account override can
        // be tested against the list of proxies the relay is actually allowed to dial. Doing it the
        // other way round would leave the relay reconciling a dangling id per request.
        let proxies: Vec<ProxyPlan> = config
            .proxies
            .iter()
            .filter(|proxy| proxy.enabled)
            .map(proxy_plan)
            .collect();

        let keys: Vec<KeyPlan> = config
            .client_keys
            .iter()
            .filter(|key| key.enabled)
            .map(key_plan)
            .collect();

        let mut providers: Vec<ProviderPlan> = Vec::with_capacity(config.providers.len());
        let mut provider_index: HashMap<&str, u32> = HashMap::with_capacity(config.providers.len());
        let mut disabled_providers: HashSet<&str> = HashSet::new();

        for provider in &config.providers {
            if !provider.enabled {
                disabled_providers.insert(provider.id.as_str());
                continue;
            }
            let index = providers.len() as u32;
            provider_index.insert(provider.id.as_str(), index);

            let mut accounts: Vec<&Account> =
                provider.accounts.iter().filter(|a| a.enabled).collect();
            // A stable sort: priority is the primary key and the authored array order is the
            // tie-break, which is exactly what the panel shows the operator.
            accounts.sort_by_key(|a| a.priority);
            let accounts = accounts
                .into_iter()
                .map(|a| {
                    let person = config.person_for_account(a).map(|p| p.proxy.as_str());
                    account_plan(
                        provider.id.as_str(),
                        a,
                        person,
                        provider.effective_windows(a),
                    )
                })
                .collect();

            let mut plan = ProviderPlan {
                id: provider.id.clone(),
                label: provider.label.clone(),
                dialect: provider.dialect,
                base_url: provider.base_url.clone(),
                auth: provider.auth.clone(),
                key_strategy: provider.key_strategy,
                cooldown_scope: provider.cooldown_scope,
                accounts,
                proxy: provider.proxy.as_deref().map(str::to_owned),
                capabilities: provider.capabilities.clone(),
                connect_timeout_ms: provider.connect_timeout_ms,
                ttfb_timeout_ms: provider.ttfb_timeout_ms,
                idle_timeout_ms: provider.idle_timeout_ms,
                total_timeout_ms: provider.total_timeout_ms,
                headers: provider.headers.clone(),
                route_overrides: provider.route_overrides.clone(),
                http2: provider.http2,
                prewarm: provider.prewarm,
                kv_pool_tokens: provider.kv_pool_tokens,
                max_context: provider.max_context,
                max_concurrent: provider.max_concurrent,
                price_per_second: provider.price_per_second,
                cold_start_ttfb_timeout_ms: provider.cold_start_ttfb_timeout_ms,
                steps: Vec::new(),
                hidden_models: provider
                    .models
                    .iter()
                    .filter(|m| m.hidden)
                    .map(|m| m.id.clone())
                    .collect(),
            };
            let visible: Vec<&ModelEntry> = provider.models.iter().filter(|m| !m.hidden).collect();
            let mut steps = Vec::with_capacity(visible.len());
            for model in &visible {
                steps.push(plan_step(
                    index,
                    &plan,
                    model,
                    config.routing.default_reasoning_level,
                ));
            }
            plan.steps = steps;
            providers.push(plan);
        }

        // Authored providers by id, so a combo's step can find the model entry the operator
        // actually wrote (including its `reasoning_wire` and pricing) without a second lookup
        // structure that could disagree with the one the index was built from.
        let entries: HashMap<&str, &crate::model::Provider> = config
            .providers
            .iter()
            .map(|p| (p.id.as_str(), p))
            .collect();

        let mut combos: Vec<ComboPlan> = Vec::with_capacity(config.combos.len());
        for combo in &config.combos {
            if !combo.enabled {
                continue;
            }
            if combo.steps.len() > u16::MAX as usize {
                return Err(Error::Snapshot(format!(
                    "combo {:?} has {} steps, more than the {} a compiled plan can index",
                    combo.name,
                    combo.steps.len(),
                    u16::MAX
                )));
            }

            // `remap[i]` is where authored step `i` ended up, so hedge participants and
            // stable-fallback references can be rewritten onto the surviving steps.
            let mut remap: Vec<Option<u16>> = vec![None; combo.steps.len()];
            let mut steps: Vec<StepPlan> = Vec::with_capacity(combo.steps.len());
            let mut unknown: Vec<String> = Vec::new();
            let mut expander = ComboExpander {
                config,
                entries: &entries,
                providers: &providers,
                provider_index: &provider_index,
                disabled_providers: &disabled_providers,
                default_level: config.routing.default_reasoning_level,
                chain: vec![combo.name.clone()],
            };

            for (authored_index, step) in combo.steps.iter().enumerate() {
                let before = steps.len();
                expander.add_authored(step, &mut steps, &mut unknown)?;
                if steps.len() > before {
                    // A nested combo can expand to several steps; a hedge participant or stable
                    // fallback that named this authored step follows its first expanded step.
                    remap[authored_index] = Some(before as u16);
                }
            }
            if steps.len() > u16::MAX as usize {
                return Err(Error::Snapshot(format!(
                    "combo {:?} expands to {} steps, more than the {} a compiled plan can index",
                    combo.name,
                    steps.len(),
                    u16::MAX
                )));
            }

            if steps.is_empty() {
                if !unknown.is_empty() {
                    return Err(Error::Snapshot(format!(
                        "combo {:?} has no routable steps: every enabled step references an unknown \
                         provider ({})",
                        combo.name,
                        unknown.join(", ")
                    )));
                }
                // Every step was disabled, or every provider it named is currently disabled.
                // Omit the combo rather than failing the whole compile.
                continue;
            }
            // Some steps are routable, so the combo still works; unknown ones have been dropped and
            // `Config::validate` is the place that reports them to the operator.

            let participants: Vec<u16> = combo
                .hedge
                .resolved_participants(combo.steps.len())
                .into_iter()
                .filter_map(|i| remap.get(i as usize).copied().flatten())
                .collect();
            let on_winner_fail = match &combo.hedge.on_winner_fail {
                WinnerFailMode::StableFallback { step } => {
                    match remap.get(*step as usize).copied().flatten() {
                        Some(remapped) => WinnerFailMode::StableFallback { step: remapped },
                        // The step the fallback pointed at is no longer routable, so the mode
                        // cannot be honoured; fall back to the default rather than dangling.
                        None => WinnerFailMode::default(),
                    }
                }
                other => other.clone(),
            };
            let hedge = HedgePlan {
                enabled: combo.hedge.enabled && participants.len() >= 2,
                participants,
                window_ms: combo.hedge.window_ms,
                winner_metric: combo.hedge.winner_metric,
                on_winner_fail,
                cancel_losers: combo.hedge.cancel_losers,
                loser_grace_ms: combo.hedge.loser_grace_ms,
            };

            let mut plan = ComboPlan {
                id: combo.id.clone(),
                name: combo.name.clone(),
                strategy: combo.strategy,
                steps,
                hedge,
                sticky_success_window_secs: combo.sticky_success_window_secs,
                midstream_policy: combo.midstream_policy,
                max_retries: combo.max_retries,
                retry_delay_ms: combo.retry_delay_ms,
                affinity: combo.affinity,
                compression: combo.compression.clone(),
                signature: 0,
            };
            plan.signature = plan_signature(&plan);
            combos.push(plan);
        }

        let aliases = compile_aliases(config)?;

        let mut index_entries: Vec<IndexEntry> = Vec::new();
        for (i, alias) in aliases.iter().enumerate() {
            index_entries.push(IndexEntry {
                key: alias.from.to_ascii_lowercase(),
                rank: RANK_ALIAS,
                target: IndexTarget::Alias { alias: i as u32 },
                next: None,
            });
        }
        for (i, combo) in combos.iter().enumerate() {
            let combo_index = i as u32;
            if !combo.id.as_str().is_empty() {
                index_entries.push(IndexEntry {
                    key: combo.id.as_str().to_ascii_lowercase(),
                    rank: RANK_COMBO_ID,
                    target: IndexTarget::Combo {
                        combo: combo_index,
                        level: None,
                    },
                    next: None,
                });
            }
            if !combo.name.is_empty() {
                index_entries.push(IndexEntry {
                    key: combo.name.to_ascii_lowercase(),
                    rank: RANK_COMBO_NAME,
                    target: IndexTarget::Combo {
                        combo: combo_index,
                        level: None,
                    },
                    next: None,
                });
                // A combo can carry the same `-<level>` suffix a model can, which is how a route
                // is pinned to a tier without editing it: `coding-high` runs `coding` at `high`.
                // A suffix that is not a level name produces nothing here, so an ordinary combo
                // name containing a dash is left exactly as it was.
                for level in LEVEL_SUFFIXES {
                    let Some(level) = ReasoningLevel::parse(level) else {
                        continue;
                    };
                    index_entries.push(IndexEntry {
                        key: format!("{}-{}", combo.name, level.as_str()).to_ascii_lowercase(),
                        rank: RANK_COMBO_NAME,
                        target: IndexTarget::Combo {
                            combo: combo_index,
                            level: Some(level.rank()),
                        },
                        next: None,
                    });
                }
            }
        }
        // Qualified `provider/model` keys, then the level-qualified forms.
        //
        // Order is what encodes precedence, and `link_entries` compares rank rather than insertion
        // order, so these two passes are independent: a model's own id always beats a
        // `base-level` reading of it.
        for (provider_index, provider) in providers.iter().enumerate() {
            for (step_index, step) in provider.steps.iter().enumerate() {
                // An exact model id runs at the tier its own name ends in when that tier is one it
                // offers, and otherwise at its default. The distinction matters for a model called
                // `gemini-3.8-flash-low`: the trailing `low` is part of the id *and* names the tier
                // it should run at, and resolving it must not be reported as a substitution.
                let level = level_suffix(&step.model)
                    .filter(|level| step.supports_level(*level))
                    .unwrap_or_else(|| {
                        ReasoningLevel::from_u8(step.default_level).unwrap_or(ReasoningLevel::None)
                    });
                index_entries.push(IndexEntry {
                    key: format!("{}/{}", provider.id.as_str(), step.model).to_ascii_lowercase(),
                    rank: RANK_QUALIFIED,
                    target: IndexTarget::Step {
                        provider: provider_index as u32,
                        step: step_index as u32,
                        level: level.rank(),
                        requested: level.rank(),
                    },
                    next: None,
                });
            }
        }
        for (provider_index, provider) in providers.iter().enumerate() {
            for (step_index, step) in provider.steps.iter().enumerate() {
                let prefix = format!("{}/", provider.id.as_str());
                // A real model id that merely *ends* in a level word is not also a `base-level`
                // name for a shorter model. `antigravity/gemini-3.8-flash-low` is a model; it must
                // not shadow anything, and the model it would otherwise be read as
                // (`gemini-3.8-flash`) is a different, real model that has its own entries.
                if level_suffix(&step.model).is_some() {
                    continue;
                }
                for level in LEVEL_SUFFIXES {
                    let Some(level) = ReasoningLevel::parse(level) else {
                        continue;
                    };
                    let offered = step.supports_level(level);
                    index_entries.push(IndexEntry {
                        key: format!("{prefix}{}-{}", step.model, level.as_str())
                            .to_ascii_lowercase(),
                        rank: RANK_QUALIFIED_LEVEL,
                        // An out-of-range level reroutes to the model's highest available one
                        // rather than failing; the flag the relay reports comes from comparing
                        // `target.level` with what the name asked for, so nothing extra is stored.
                        target: IndexTarget::Step {
                            provider: provider_index as u32,
                            step: step_index as u32,
                            level: if offered {
                                level.rank()
                            } else {
                                // Out of range reroutes to the highest available level. A model
                                // reporting no levels at all reroutes to `none`, which sends no
                                // reasoning fragment rather than inventing support.
                                highest_rank(step.levels)
                            },
                            requested: level.rank(),
                        },
                        next: None,
                    });
                }
            }
        }
        // There is deliberately **no** bare-model pass. Feature 3 removed the bare-id fallback:
        // a name with no provider prefix resolves only as an alias or a combo, because the old
        // fallback made two providers offering one model id resolve by configured order — a
        // routing decision nobody wrote down. The error a miss returns says exactly this.

        let mut snapshot = RoutingSnapshot {
            digest,
            built_at,
            routing_defaults: config.routing.defaults.clone(),
            providers,
            combos,
            aliases,
            keys,
            proxies,
            proxy_strategy: config.proxy_strategy,
            index: HashMap::new(),
            index_entries,
        };
        snapshot.link_entries()?;
        Ok(snapshot)
    }

    /// The routing digest this snapshot was compiled from.
    ///
    /// Equal to [`Config::routing_digest`] of the configuration that produced it.
    pub fn digest(&self) -> u64 {
        self.digest
    }

    /// When the compiler ran.
    pub fn built_at(&self) -> Millis {
        self.built_at
    }

    /// The fallback model defaults for unsupported request modalities.
    pub fn routing_defaults(&self) -> &crate::config::RoutingDefaults {
        &self.routing_defaults
    }

    /// The compiled providers, in configured order.
    pub fn providers(&self) -> &[ProviderPlan] {
        &self.providers
    }

    /// The compiled combos, in configured order.
    pub fn combos(&self) -> &[ComboPlan] {
        &self.combos
    }

    /// The compiled aliases, each already flattened to its final target.
    pub fn aliases(&self) -> &[AliasPlan] {
        &self.aliases
    }

    /// The enabled client keys with their rolling quota windows compiled in.
    ///
    /// The relay matches a presented key by comparing its hash against these plans, so the order is
    /// the configured order and nothing here is sorted: key counts are small and a linear scan over
    /// a handful of constant-time comparisons is cheaper than a second hash index maintained in two
    /// places.
    pub fn keys(&self) -> &[KeyPlan] {
        &self.keys
    }

    /// The proxies the relay may dial, in configured order.
    pub fn proxies(&self) -> &[ProxyPlan] {
        &self.proxies
    }

    /// How a provider or account with no override picks a proxy.
    pub fn proxy_strategy(&self) -> ProxyStrategy {
        self.proxy_strategy
    }

    /// A proxy by id, for a caller holding a compiled [`ProviderPlan::proxy`] or
    /// [`AccountPlan::proxy`].
    ///
    /// This is a linear scan on purpose: the proxy list is a handful of entries, and an id index
    /// would be one more thing to keep consistent across encode and decode for no measurable gain.
    pub fn proxy(&self, id: &str) -> Option<&ProxyPlan> {
        self.proxies.iter().find(|proxy| proxy.id == id)
    }

    /// Resolve a requested model name to a combo, if one matches.
    ///
    /// This is [`RoutingSnapshot::resolve`] restricted to the combo case, for callers that
    /// genuinely only handle combos. A direct provider/model hit returns `None` here; use `resolve`
    /// to see it.
    pub fn lookup(&self, requested: &str) -> Option<&ComboPlan> {
        match self.resolve(requested) {
            Resolution::Combo { plan, .. } => Some(plan),
            _ => None,
        }
    }

    /// Resolve a requested model name.
    ///
    /// The order is fixed and deliberate and lives entirely in the compiled index: a user alias,
    /// then a combo id, then a combo name (optionally with a `-<level>` suffix), then
    /// `provider/model` (optionally with a `-<level>` suffix, with the model's own id winning over
    /// the suffix reading), and finally a bare model id. The alias step is flattened at compile
    /// time, so at most one extra hop is ever taken here; the loop bound is purely a guard against
    /// a snapshot built by something that did not flatten.
    ///
    /// Matching is ASCII-case-insensitive. Model ids and provider slugs are ASCII by construction,
    /// and folding only ASCII keeps this allocation-free on the hot path.
    ///
    /// A miss carries a reason (see [`Resolution::NotFound`]), because "nothing matched" is not
    /// actionable when the operator typed a bare model id that used to work.
    pub fn resolve(&self, requested: &str) -> Resolution<'_> {
        let mut key = requested;
        for _ in 0..=MAX_ALIAS_HOPS {
            match self.lookup_target(key) {
                Some(IndexTarget::Alias { alias }) => match self.aliases.get(*alias as usize) {
                    Some(plan) => key = plan.to.as_str(),
                    None => {
                        return Resolution::not_found(format!(
                            "alias {requested:?} points at a target this snapshot does not carry"
                        ))
                    }
                },
                Some(IndexTarget::Combo { combo, level }) => {
                    return match self.combos.get(*combo as usize) {
                        Some(plan) => Resolution::Combo {
                            plan,
                            level: *level,
                        },
                        None => Resolution::not_found(format!(
                            "combo index {combo} is out of range for this snapshot"
                        )),
                    }
                }
                Some(IndexTarget::Step {
                    provider,
                    step,
                    level,
                    requested: asked,
                }) => {
                    return match self
                        .providers
                        .get(*provider as usize)
                        .and_then(|p| p.steps.get(*step as usize))
                    {
                        Some(plan) => Resolution::Direct {
                            step: plan,
                            level: *level,
                            substituted: level != asked,
                        },
                        None => Resolution::not_found(format!(
                            "the model table moved under {requested:?}; reload the snapshot"
                        )),
                    }
                }
                None => return Resolution::not_found(self.explain_miss(requested)),
            }
        }
        Resolution::not_found(format!(
            "alias {requested:?} does not resolve: its chain is longer than the {MAX_ALIAS_HOPS} \
             hops a compiled snapshot allows"
        ))
    }

    /// Why a name matched nothing, phrased for the operator who typed it.
    ///
    /// The distinctions matter because the three cases need different fixes: a name with no
    /// provider prefix is now an error by design (Feature 3 removed the bare-id fallback), an
    /// unknown provider is a typo in the prefix, and a known provider with an unknown model is a
    /// typo in the model or a model that was never synced.
    fn explain_miss(&self, requested: &str) -> String {
        match requested.split_once('/') {
            None => format!(
                "no combo, alias or model matches {requested:?}. A name with no provider prefix \
                 must be a combo or an alias; a model is addressed as provider/model, optionally \
                 with a -<level> suffix such as {requested}-high."
            ),
            Some((provider, rest)) => {
                if let Some(hidden) = self.hidden_model_named(provider, rest) {
                    // A hidden model is not a missing one, and telling the operator it does not
                    // exist sends them to sync models that are already there. The fix is one
                    // checkbox, so say that instead.
                    format!(
                        "provider {provider:?} has a model {hidden:?}, but it is hidden: show it in \
                         the provider's model list to route to it"
                    )
                } else if self.providers.iter().any(|p| p.id.as_str() == provider) {
                    // Naming a *level* that the model does not offer is not a miss: those keys are
                    // indexed against the model's highest level. So the only way to get here with a
                    // known provider is a genuine model miss, and naming the closest candidate
                    // would be a guess this code deliberately does not make.
                    format!(
                        "provider {provider:?} exists but does not offer a model matching {rest:?}"
                    )
                } else {
                    format!(
                        "unknown provider {provider:?} in {requested:?}; configured providers are {}",
                        self.provider_ids()
                    )
                }
            }
        }
    }

    /// The hidden model id `name` would have matched, if any.
    ///
    /// `name` may carry a `-<level>` suffix, because that is how a request pins a tier, so the
    /// suffix is stripped before the comparison when it names a level. Matching is exact and
    /// case-sensitive, like the routing index: a near miss is still a miss and must say so.
    fn hidden_model_named(&self, provider: &str, name: &str) -> Option<String> {
        let plan = self.providers.iter().find(|p| p.id.as_str() == provider)?;
        let base = match level_suffix(name) {
            Some(_) => name.rsplit_once('-').map(|(base, _)| base).unwrap_or(name),
            None => name,
        };
        plan.hidden_models
            .iter()
            .find(|hidden| hidden.as_str() == base)
            .cloned()
    }

    /// The configured provider ids, comma-separated, for a miss message.
    fn provider_ids(&self) -> String {
        if self.providers.is_empty() {
            return "none (no provider is enabled)".to_string();
        }
        self.providers
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Encode to the on-disk frame.
    ///
    /// Encoding is deterministic for a given snapshot: the same table produces the same bytes, so
    /// a snapshot can be compared byte-for-byte across machines.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let (strings, body) = self.encode_body();
        let mut payload: Vec<u8> = Vec::with_capacity(body.len() + 64);
        payload.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        for string in &strings {
            let length = u32::try_from(string.len()).map_err(|_| {
                Error::Snapshot("snapshot cannot encode a string longer than 4 GiB".into())
            })?;
            payload.extend_from_slice(&length.to_le_bytes());
            payload.extend_from_slice(string.as_bytes());
        }
        payload.extend_from_slice(&body);
        if u32::try_from(payload.len()).is_err() {
            return Err(Error::Snapshot(
                "snapshot payload exceeds the 4 GiB the frame can address".into(),
            ));
        }
        Ok(assemble(self.digest, &payload))
    }

    /// Decode from the on-disk frame.
    ///
    /// Every read is bounds-checked, so malformed input produces an `Error::Snapshot` and never a
    /// panic: a wrong magic, an unknown version, a short read, a bad checksum, an out-of-range
    /// string or structure index, or trailing bytes are each reported distinctly.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let (_version, digest) = verify_header(bytes)?;
        let payload_len = read_u32_at(bytes, 20)? as usize;
        let payload = bytes
            .get(HEADER_LEN..HEADER_LEN + payload_len)
            .ok_or_else(|| {
                Error::Snapshot(
                    "snapshot is truncated: the header declares more payload than is present"
                        .into(),
                )
            })?;

        let mut reader = Reader::new(payload);
        let strings = decode_string_table(&mut reader)?;
        let built_at = Millis::from_millis(reader.i64()?);
        let routing_defaults = crate::config::RoutingDefaults {
            image: reader.opt_string(&strings)?,
            audio: reader.opt_string(&strings)?,
            video: reader.opt_string(&strings)?,
            pdf: reader.opt_string(&strings)?,
            file: reader.opt_string(&strings)?,
            extra: Default::default(),
        };
        let providers = decode_providers(&mut reader, &strings)?;
        let combos = decode_combos(&mut reader, &strings, &providers)?;
        let aliases = decode_aliases(&mut reader, &strings)?;
        let keys = decode_keys(&mut reader, &strings)?;
        let proxies = decode_proxies(&mut reader, &strings)?;
        let proxy_strategy = proxy_strategy_from_tag(reader.u8()?)?;
        let index_entries = decode_index_entries(&mut reader, &strings)?;
        if reader.remaining() != 0 {
            return Err(Error::Snapshot(format!(
                "snapshot payload has {} unread trailing bytes",
                reader.remaining()
            )));
        }

        let mut snapshot = RoutingSnapshot {
            digest,
            built_at,
            routing_defaults,
            providers,
            combos,
            aliases,
            keys,
            proxies,
            proxy_strategy,
            index: HashMap::new(),
            index_entries,
        };
        snapshot.link_entries()?;
        Ok(snapshot)
    }

    /// Write the frame atomically: a sibling temporary file, `fsync`, then rename.
    ///
    /// The relay may be reading this path at any moment, so a partially written file is never
    /// allowed to be visible. On any failure the temporary file is removed rather than left for the
    /// next start to trip over.
    pub fn write_to(&self, path: &Path) -> Result<()> {
        let bytes = self.encode()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    Error::Snapshot(format!("cannot create {}: {e}", parent.display()))
                })?;
            }
        }
        let temp = temp_sibling(path);
        let result = (|| -> std::io::Result<()> {
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            std::fs::rename(&temp, path)
        })();
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&temp);
                Err(Error::Snapshot(format!(
                    "cannot write snapshot {}: {e}",
                    path.display()
                )))
            }
        }
    }

    /// Load and decode a snapshot from a file.
    pub fn load_from(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path).map_err(|e| {
            // An absent file is a distinct case from an unreadable one, and callers act on the
            // difference: the first run of a process that has never compiled a snapshot is not an
            // error, while a file that exists and cannot be read is something an operator should
            // know about. Collapsing both into one variant made "compile me a fresh one" impossible
            // to express, which is how a relay ends up unable to start on a fresh install.
            if e.kind() == std::io::ErrorKind::NotFound {
                Error::NotFound(format!("no snapshot at {}", path.display()))
            } else {
                Error::Snapshot(format!("cannot read snapshot {}: {e}", path.display()))
            }
        })?;
        Self::decode(&bytes)
    }

    /// Look up one key in the index, applying precedence and verifying collisions by full string.
    ///
    /// The chain walk is bounded by the number of entries, so a snapshot whose `next` pointers were
    /// built wrongly (or a hand-edited in-memory table) cannot hang a request.
    fn lookup_target(&self, requested: &str) -> Option<&IndexTarget> {
        let hash = fnv1a64_lower_ascii(requested.as_bytes());
        let mut cursor = *self.index.get(&hash)?;
        let mut best: Option<&IndexEntry> = None;
        let mut budget = self.index_entries.len() + 1;
        while budget > 0 {
            budget -= 1;
            let entry = match self.index_entries.get(cursor as usize) {
                Some(entry) => entry,
                None => break,
            };
            if entry.key.eq_ignore_ascii_case(requested) {
                let better = match best {
                    Some(current) => entry.rank < current.rank,
                    None => true,
                };
                if better {
                    best = Some(entry);
                }
            }
            match entry.next {
                Some(next) if (next as usize) < self.index_entries.len() => cursor = next,
                _ => break,
            }
        }
        best.map(|entry| &entry.target)
    }

    /// Rebuild the hash bucket heads and `next` chains from `index_entries`.
    ///
    /// Entries are linked in vector order, each becoming the new head of its bucket. This is the
    /// single place the index invariant is established, which is why `encode` need not persist the
    /// chain: decoding the same entry order reproduces it exactly.
    fn link_entries(&mut self) -> Result<()> {
        self.index.clear();
        for i in 0..self.index_entries.len() {
            let target = self.index_entries[i].target.clone();
            self.validate_target(&target)?;
            if self.index_entries[i].rank > RANK_MAX {
                return Err(Error::Snapshot(format!(
                    "snapshot lookup entry {i} has unknown precedence rank {}",
                    self.index_entries[i].rank
                )));
            }
            let hash = fnv1a64(self.index_entries[i].key.as_bytes());
            let next = self.index.get(&hash).copied();
            self.index_entries[i].next = next;
            self.index.insert(hash, i as u32);
        }
        Ok(())
    }

    /// Reject an index target that points at a structure which does not exist.
    fn validate_target(&self, target: &IndexTarget) -> Result<()> {
        match target {
            IndexTarget::Alias { alias } => {
                if *alias as usize >= self.aliases.len() {
                    return Err(Error::Snapshot(format!(
                        "snapshot lookup entry points at alias {alias}, but only {} aliases exist",
                        self.aliases.len()
                    )));
                }
            }
            IndexTarget::Combo { combo, .. } => {
                if *combo as usize >= self.combos.len() {
                    return Err(Error::Snapshot(format!(
                        "snapshot lookup entry points at combo {combo}, but only {} combos exist",
                        self.combos.len()
                    )));
                }
            }
            IndexTarget::Step { provider, step, .. } => {
                let plan = self.providers.get(*provider as usize).ok_or_else(|| {
                    Error::Snapshot(format!(
                        "snapshot lookup entry points at provider {provider}, but only {} providers \
                         exist",
                        self.providers.len()
                    ))
                })?;
                if *step as usize >= plan.steps.len() {
                    return Err(Error::Snapshot(format!(
                        "snapshot lookup entry points at step {step} of provider {provider}, which \
                         has only {} steps",
                        plan.steps.len()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Serialise the body into `(string table, body bytes)`.
    ///
    /// The table has to precede the body it describes, but strings are only discovered while
    /// writing it, so the body is built first and the two are concatenated by `encode`.
    fn encode_body(&self) -> (Vec<String>, Vec<u8>) {
        let mut encoder = Encoder::new();
        encoder.put_i64(self.built_at.as_millis());
        encoder.put_opt_str(self.routing_defaults.image.as_deref());
        encoder.put_opt_str(self.routing_defaults.audio.as_deref());
        encoder.put_opt_str(self.routing_defaults.video.as_deref());
        encoder.put_opt_str(self.routing_defaults.pdf.as_deref());
        encoder.put_opt_str(self.routing_defaults.file.as_deref());
        encoder.put_u32(self.providers.len() as u32);
        for provider in &self.providers {
            encode_provider(&mut encoder, provider);
        }
        encoder.put_u32(self.combos.len() as u32);
        for combo in &self.combos {
            encode_combo(&mut encoder, combo);
        }
        encoder.put_u32(self.aliases.len() as u32);
        for alias in &self.aliases {
            encode_alias(&mut encoder, alias);
        }
        encoder.put_u32(self.keys.len() as u32);
        for key in &self.keys {
            encode_key(&mut encoder, key);
        }
        encoder.put_u32(self.proxies.len() as u32);
        for proxy in &self.proxies {
            encode_proxy(&mut encoder, proxy);
        }
        encoder.put_u8(proxy_strategy_tag(self.proxy_strategy));
        encoder.put_u32(self.index_entries.len() as u32);
        for entry in &self.index_entries {
            encode_index_entry(&mut encoder, entry);
        }
        (encoder.strings, encoder.body)
    }
}

/// Validate a frame's magic, version, length and checksum and return `(version, digest)`.
///
/// This exists so a relay can decide whether a file is worth [`RoutingSnapshot::decode`]ing at all
/// — for example to skip a reload whose digest is already installed — without building the
/// snapshot. It does hash the payload, because the checksum is the only thing that distinguishes a
/// truncated file from a valid one; what it avoids is allocating the snapshot's owned structures.
pub fn verify_header(bytes: &[u8]) -> Result<(u32, u64)> {
    if bytes.len() < SNAPSHOT_MAGIC.len() {
        return Err(Error::Snapshot(format!(
            "snapshot is truncated: {} bytes, too few for the {} magic bytes",
            bytes.len(),
            SNAPSHOT_MAGIC.len()
        )));
    }
    if bytes[..SNAPSHOT_MAGIC.len()] != SNAPSHOT_MAGIC {
        return Err(Error::Snapshot(
            "snapshot magic bytes do not match: this is not a compiled routing snapshot".into(),
        ));
    }
    if bytes.len() < HEADER_LEN {
        return Err(Error::Snapshot(format!(
            "snapshot is truncated: {} bytes, too few for the {HEADER_LEN}-byte header",
            bytes.len()
        )));
    }

    let version = read_u32_at(bytes, 8)?;
    if version != SNAPSHOT_VERSION {
        return Err(Error::Snapshot(format!(
            "snapshot version {version} is not understood; this binary speaks version \
             {SNAPSHOT_VERSION}"
        )));
    }
    let digest = read_u64_at(bytes, 12)?;
    let payload_len = read_u32_at(bytes, 20)? as usize;
    let expected = HEADER_LEN.checked_add(payload_len).ok_or_else(|| {
        Error::Snapshot("snapshot payload length overflows the address space".into())
    })?;
    if bytes.len() < expected {
        return Err(Error::Snapshot(format!(
            "snapshot is truncated: the header declares a {payload_len}-byte payload ({expected} \
             bytes total) but only {} bytes are present",
            bytes.len()
        )));
    }
    if bytes.len() > expected {
        return Err(Error::Snapshot(format!(
            "snapshot has {} trailing bytes after its {payload_len}-byte payload",
            bytes.len() - expected
        )));
    }

    let stored = read_u64_at(bytes, 24)?;
    let actual = checksum(version, digest, &bytes[HEADER_LEN..expected]);
    if stored != actual {
        return Err(Error::Snapshot(format!(
            "snapshot checksum mismatch: the header claims {stored:#018x} but the payload hashes to \
             {actual:#018x}"
        )));
    }
    Ok((version, digest))
}

/// Build a complete frame around an already-encoded payload.
fn assemble(digest: u64, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&SNAPSHOT_MAGIC);
    out.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    out.extend_from_slice(&digest.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&checksum(SNAPSHOT_VERSION, digest, payload).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// FNV-1a over `version || digest || payload`.
///
/// Folding the version and digest in means a corrupted header is caught by the same check that
/// catches a corrupted payload, so `verify_header` can trust everything it returns.
fn checksum(version: u32, digest: u64, payload: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let version_bytes = version.to_le_bytes();
    let digest_bytes = digest.to_le_bytes();
    let mut hash = OFFSET;
    for chunk in [&version_bytes[..], &digest_bytes[..], payload] {
        for &byte in chunk {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

/// `fnv1a64` of the ASCII-lowercased bytes, without allocating a folded copy.
///
/// The hot path calls this with the requested key, and compile/decode fold keys with the same ASCII
/// rule, so the hashes agree. Full Unicode case folding is deliberately not used: model ids and
/// provider slugs are ASCII, and allocating a `String` per request to handle a case that cannot
/// occur would defeat the point of a compiled table.
fn fnv1a64_lower_ascii(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for &byte in bytes {
        hash ^= byte.to_ascii_lowercase() as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Read a little-endian `u32` at a fixed offset without ever panicking.
fn read_u32_at(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| Error::Snapshot("snapshot offset overflow".into()))?;
    let slice = bytes.get(offset..end).ok_or_else(|| {
        Error::Snapshot(format!(
            "snapshot is truncated: {end} bytes are needed to read a u32 at offset {offset}, but \
             only {} are present",
            bytes.len()
        ))
    })?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

/// Read a little-endian `u64` at a fixed offset without ever panicking.
fn read_u64_at(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| Error::Snapshot("snapshot offset overflow".into()))?;
    let slice = bytes.get(offset..end).ok_or_else(|| {
        Error::Snapshot(format!(
            "snapshot is truncated: {end} bytes are needed to read a u64 at offset {offset}, but \
             only {} are present",
            bytes.len()
        ))
    })?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}

/// A sibling path used for the atomic write, unique per process.
fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "snapshot".to_string());
    let temp_name = format!(".{name}.{}.tmp", std::process::id());
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(temp_name),
        _ => PathBuf::from(temp_name),
    }
}

/// Build a compiled account from the authored one.
///
/// The credential reference is resolved here, once: an explicit `secret_ref` is kept, an account
/// with no OAuth reference falls back to the conventional [`account_secret_id`] vault path, and an
/// OAuth account keeps only its metadata reference. The relay opens the vault after resolving its
/// metadata record; this module only handles ids.
///
/// `windows` is the account's **effective** quota declaration — its own if it wrote any, otherwise
/// the provider's — so that resolution happens once, here, rather than on every request.
fn account_plan(
    provider_id: &str,
    account: &Account,
    person_proxy: Option<&str>,
    windows: &[crate::model::QuotaWindow],
) -> AccountPlan {
    let secret_ref = match &account.secret_ref {
        Some(explicit) => Some(explicit.clone()),
        None if account.oauth_ref.is_none() => {
            Some(account_secret_id(provider_id, account.id.as_str()))
        }
        None => None,
    };
    let oauth_ref = account.oauth_ref.as_ref().map(|id| id.as_str().to_string());
    AccountPlan {
        id: account.id.clone(),
        label: account.label.clone(),
        secret_ref,
        oauth_ref,
        priority: account.priority,
        weight: account.weight,
        max_concurrent: account.max_concurrent,
        models_allow: account.models_allow.clone(),
        models_deny: account.models_deny.clone(),
        // The account's own choice wins; otherwise the person it is tagged for decides, which is
        // what makes tagging an account enough to send it through that person's machine. A proxy
        // id that is missing or disabled resolves to a direct connection at request time, so a
        // person whose machine is off costs their accounts their egress, not their requests.
        proxy: account
            .proxy
            .clone()
            .or_else(|| person_proxy.map(str::to_owned)),
        placeholders: account
            .placeholders
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        credit_cents: account.credit_usd.map(cents_from_usd),
        // Only windows that can actually stop an account are compiled: an inert window is dead
        // weight on the hot path, and a window with no positive length has no interval to count
        // over — the configuration validator already refuses it, but a hand-built `Config` gets
        // the same treatment here rather than an infinite one.
        quota: windows
            .iter()
            .filter(|window| window.seconds > 0 && window.constrains())
            .cloned()
            .collect(),
    }
}

/// Round a dollar amount to whole cents.
///
/// `as u64` saturates and a negative value was already refused by validation, but `.max(0.0)`
/// keeps a plan compiled from a hand-built config from wrapping; the relay treats the result as
/// "unknown" only when the operator declared nothing at all.
fn cents_from_usd(usd: f64) -> u64 {
    (usd * 100.0).round().max(0.0) as u64
}

/// Build a proxy plan without its disabled configuration metadata.
fn proxy_plan(proxy: &ProxyConfig) -> ProxyPlan {
    ProxyPlan {
        id: proxy.id.clone(),
        label: proxy.label.clone(),
        url: proxy.url.clone(),
        username: proxy.username.clone(),
        secret_ref: proxy.secret_ref.clone(),
        priority: proxy.priority,
    }
}

/// Build a client-key plan, preserving the three authored optional windows verbatim.
///
/// In particular, an explicit all-zero window is retained rather than collapsed to `None`: the
/// authored config distinguishes an absent window from an explicit window whose dimensions are
/// uncapped, and a compiled representation should not erase that distinction.
fn key_plan(key: &ClientKey) -> KeyPlan {
    KeyPlan {
        id: key.id.clone(),
        label: key.label.clone(),
        key_hash: key.key_hash.clone(),
        prefix: key.prefix.clone(),
        allow_models: key.allow_models.clone(),
        allow_routes: key.allow_routes.clone(),
        limit_requests: key.limit_requests,
        limits: key.limits,
    }
}

/// Expands one combo's authored steps into the flat step list the relay executes.
///
/// A step may name a provider/model pair or another combo. A combo target is spliced in place: the
/// nested combo contributes its enabled steps, in its own order, and the outer combo keeps its
/// strategy, hedge, retries and compression. Expansion is recursive, so a combo can be assembled
/// from smaller routes; [`Self::chain`] makes a loop an error naming the chain instead of a
/// request that never terminates.
struct ComboExpander<'a> {
    config: &'a Config,
    entries: &'a HashMap<&'a str, &'a Provider>,
    providers: &'a [ProviderPlan],
    provider_index: &'a HashMap<&'a str, u32>,
    disabled_providers: &'a HashSet<&'a str>,
    default_level: ReasoningLevel,
    /// Names of the combos currently being expanded, outermost first.
    chain: Vec<String>,
}

impl ComboExpander<'_> {
    /// Append the steps `step` contributes, returning whether it contributed any.
    fn add_authored(
        &mut self,
        step: &ComboStep,
        out: &mut Vec<StepPlan>,
        unknown: &mut Vec<String>,
    ) -> Result<bool> {
        if !step.enabled {
            return Ok(false);
        }
        if let Some(target) = &step.combo {
            return self.add_combo_target(target, out, unknown);
        }
        match self.provider_index.get(step.provider.as_str()) {
            Some(&provider) => {
                let plan = &self.providers[provider as usize];
                // The entry comes from the authored configuration rather than from the compiled
                // plan, so a combo step is built from exactly the same metadata a direct
                // `provider/model` request is, with no second derivation to drift.
                let entry = self
                    .entries
                    .get(step.provider.as_str())
                    .and_then(|source| {
                        source
                            .models
                            .iter()
                            .find(|m| m.id.eq_ignore_ascii_case(&step.model))
                    })
                    .unwrap_or(&UNKNOWN_MODEL);
                // A step may pin its own tier; otherwise the routing default applies.
                // `plan_step` clamps a level the model does not offer to the model's highest one,
                // so an over-asked level degrades instead of failing.
                let mut planned = plan_step(
                    provider,
                    plan,
                    entry,
                    step.level.unwrap_or(self.default_level),
                );
                planned.accounts = self.resolve_accounts(step, plan);
                out.push(planned);
                Ok(true)
            }
            None => {
                if !self.disabled_providers.contains(step.provider.as_str()) {
                    unknown.push(step.provider.as_str().to_string());
                }
                Ok(false)
            }
        }
    }

    /// Resolve a step's authored account selectors against the provider's compiled accounts.
    ///
    /// `None` means the step names no selection and every account may serve it. Otherwise the
    /// result is the ids of the accounts the selectors name, in the provider's own order — which is
    /// priority order, so the relay's existing ranking still decides among them.
    ///
    /// The selectors are matched against the *authored* accounts rather than the compiled ones
    /// because a tag lives on the account and the compiled plan carries only what the request path
    /// needs. A selector that names only disabled accounts therefore resolves to an empty list: the
    /// operator asked for those accounts and they are not available, which is a step that cannot
    /// serve rather than a step that quietly serves from everyone.
    fn resolve_accounts(&self, step: &ComboStep, plan: &ProviderPlan) -> Option<Vec<AccountId>> {
        if step.accounts.is_empty() {
            return None;
        }
        let authored = self.config.provider(step.provider.as_str());
        let mut selected = Vec::new();
        for account in &plan.accounts {
            let source = authored.and_then(|provider| {
                provider
                    .accounts
                    .iter()
                    .find(|candidate| candidate.id == account.id)
            });
            let named = step.accounts.iter().any(|selector| match source {
                Some(source) => crate::model::account_selector_matches(selector, source),
                // A compiled account with no authored counterpart cannot happen, but matching on
                // the id alone keeps the resolution total rather than silently selecting nothing.
                None => account.id.as_str() == selector.trim(),
            });
            if named {
                selected.push(account.id.clone());
            }
        }
        Some(selected)
    }

    fn add_combo_target(
        &mut self,
        target: &str,
        out: &mut Vec<StepPlan>,
        unknown: &mut Vec<String>,
    ) -> Result<bool> {
        let Some(nested) = self.config.combo(target) else {
            return Err(Error::Snapshot(format!(
                "combo step routes to unknown combo {target:?}"
            )));
        };
        // A disabled combo is not routable at all, so it contributes nothing; the outer combo
        // either keeps its other steps or is omitted by the caller as unroutable.
        if !nested.enabled {
            return Ok(false);
        }
        if self.chain.iter().any(|name| name == &nested.name) {
            let mut chain = self.chain.clone();
            chain.push(nested.name.clone());
            return Err(Error::Snapshot(format!(
                "combo routing loop: {}",
                chain.join(" -> ")
            )));
        }
        self.chain.push(nested.name.clone());
        let before = out.len();
        for step in &nested.steps {
            self.add_authored(step, out, unknown)?;
        }
        self.chain.pop();
        Ok(out.len() > before)
    }
}

/// Build a step from an already-compiled provider.
///
/// Everything except the model's own metadata and the provider index is copied from the provider,
/// so the encoded frame carries one copy of each base URL and timeout rather than one per model.
fn plan_step(
    index: u32,
    provider: &ProviderPlan,
    model: &ModelEntry,
    default_reasoning_level: ReasoningLevel,
) -> StepPlan {
    let levels = effective_levels(model, provider);
    let modalities = effective_modalities(model, provider);
    // The default has to be a level the model actually offers, or the snapshot would carry a step
    // whose own default it does not support — and the decoder rejects exactly that, because it
    // would be a step that emits a fragment the model never agreed to.
    //
    // Two cases are worth naming. A model with **no** levels at all gets `none`, which emits no
    // fragment: a fabricated `high` on a model that never advertised reasoning is the kind of
    // invented capability this project refuses to show. A model that offers levels but not the
    // configured default gets its **highest** one, which is the same substitution rule an
    // out-of-range `-<level>` suffix follows.
    let default_level = if levels == 0 {
        ReasoningLevel::None
    } else {
        let configured = model.default_level(default_reasoning_level);
        if levels & (1u32 << configured.rank()) != 0 {
            configured
        } else {
            ReasoningLevel::from_u8(highest_rank(levels)).unwrap_or(ReasoningLevel::None)
        }
    };
    StepPlan {
        provider_index: index,
        provider_id: provider.id.clone(),
        model: model.id.clone(),
        dialect: provider.dialect,
        auth: provider.auth.clone(),
        base_url: provider.base_url.clone(),
        connect_timeout_ms: provider.connect_timeout_ms,
        ttfb_timeout_ms: provider.ttfb_timeout_ms,
        idle_timeout_ms: provider.idle_timeout_ms,
        total_timeout_ms: provider.total_timeout_ms,
        level: default_level.rank(),
        levels,
        default_level: default_level.rank(),
        reasoning_json: reasoning_fragments(provider.dialect, model, levels),
        input_modalities: modalities,
        pricing: model.pricing.clone(),
        // A direct `provider/model` resolution has no authored step and therefore no selection;
        // `add_authored` fills this in for a combo step.
        accounts: None,
        context_length: model.context_length.or_else(|| {
            provider
                .max_context
                .and_then(|context| u32::try_from(context).ok())
        }),
    }
}

/// The levels a model offers, as a bitmask.
///
/// The precedence lives in [`crate::model::effective_reasoning_levels`] so the panel, the validator
/// and this compiler cannot disagree about it: the operator's explicit list wins, then the bundled
/// provider catalogue, then the dialect's own list when the model or provider declares reasoning
/// support. That function also caps the result to what the dialect can put on the wire, which is
/// why a stale `max` on an OpenAI-shaped provider never becomes an offered tier here even before
/// validation has had a chance to report it.
fn effective_levels(model: &ModelEntry, provider: &ProviderPlan) -> u32 {
    crate::model::effective_reasoning_levels(
        provider.id.as_str(),
        provider.dialect,
        &provider.capabilities,
        model,
    )
    .iter()
    .fold(0u32, |mask, level| mask | (1u32 << level.rank()))
}

/// The input modalities a model accepts, as a bitmask over `InputModality`'s discriminant.
///
/// Text is always accepted: a model that could not take text is not a chat model, and the
/// rewritten-on-unsupported-input path depends on there being one type that always works.
fn effective_modalities(model: &ModelEntry, provider: &ProviderPlan) -> u32 {
    let declared = |modalities: &[InputModality]| {
        modalities
            .iter()
            .fold(1u32 << InputModality::Text.as_u8(), |mask, modality| {
                mask | (1u32 << modality.as_u8())
            })
    };
    if !model.input_modalities.is_empty() {
        return declared(&model.input_modalities);
    }
    if let Some(meta) = crate::catalog::catalog().model_meta(provider.id.as_str(), &model.id) {
        if !meta.modalities.is_empty() {
            return declared(meta.modalities);
        }
    }
    // The provider's own `Vision` capability is the weakest evidence there is, and it is still
    // better than nothing: it is the operator saying "this provider takes images".
    if provider.capabilities.contains(&Capability::Vision) {
        return declared(&[InputModality::Text, InputModality::Image]);
    }
    declared(&[InputModality::Text])
}

/// The per-level reasoning fragment for a step, indexed by level rank.
///
/// A per-model `reasoning_wire` entry replaces the dialect default **wholesale** for that level;
/// this is why the result is a table of encoded JSON strings rather than a merge performed later.
/// Encoding is fallible in principle (a serialiser can fail on an unrepresentable float) and
/// infallible for the fragments this module can produce, so a failure is reported by sending
/// nothing for that level rather than by failing the whole compile — the same "never invent a
/// capability" rule applies in reverse: never invent a fragment either.
fn reasoning_fragments(dialect: Dialect, model: &ModelEntry, levels: u32) -> [Option<String>; 8] {
    let mut out: [Option<String>; 8] = Default::default();
    for level in ReasoningLevel::ALL {
        if levels & (1u32 << level.rank()) == 0 {
            continue;
        }
        let mut fragment = match model
            .reasoning_wire
            .as_ref()
            .and_then(|map| map.get(&level))
        {
            // An explicit `null` in `reasoning_wire` is the operator saying "send nothing here",
            // which is a different statement from "no opinion" and must survive as one.
            Some(value) if value.is_null() => None,
            Some(value) => Some(value.clone()),
            None => dialect_reasoning_fragment(dialect, level),
        };
        // A budget larger than the model can emit is a request the upstream refuses (Anthropic)
        // or an answer that never gets written (Gemini spends it all thinking). The model's own
        // `max_output_tokens` is the only honest ceiling, and clamping only lowers a number, so an
        // authored `reasoning_wire` is fitted by the same rule instead of being trusted blindly.
        if let Some(value) = fragment.as_mut() {
            if !fit_thinking_budget(dialect, value, model.max_output_tokens) {
                fragment = None;
            } else {
                state_thinking_output_cap(dialect, value, model);
            }
        }
        out[level.rank() as usize] = fragment.and_then(|value| serde_json::to_string(&value).ok());
    }
    out
}

/// State the output allowance a thinking budget has to fit inside.
///
/// A budget is not a free-standing number: it is carved out of the same allowance the answer comes
/// from, and an upstream that reads both from one body compares them before it does any work.
/// Cloud Code answers Anthropic's `max_tokens` must be greater than `thinking.budget_tokens` when a
/// budget arrives, as antigravity bodies do, without the `maxOutputTokens` it is measured against —
/// the model then reasons under a ceiling it was never told about.
///
/// So the ceiling travels with the budget, and only for the dialects that carry thinking inside
/// `generationConfig`: Anthropic is handed `max_tokens` by its own translator, and the
/// `reasoning_effort` dialects have no budget to pair. The relay still lets a client's stated cap
/// win, because it refits the budget to that cap before merging and never overwrites a field the
/// request already set.
fn state_thinking_output_cap(
    dialect: Dialect,
    fragment: &mut serde_json::Value,
    model: &ModelEntry,
) {
    if !matches!(
        dialect,
        Dialect::Google | Dialect::Antigravity | Dialect::GoogleAiStudio
    ) {
        return;
    }
    let Some(ceiling) = model.max_output_tokens else {
        // Nothing declared is nothing to state: inventing a ceiling would be inventing a limit
        // the model may not have.
        return;
    };
    if !mentions_thinking_budget(fragment) {
        return;
    }
    // The fragment is shaped exactly as deep as the field it has to reach: `generationConfig` is
    // at the top level for Gemini, and under the envelope's `request` for Antigravity.
    let generation = match fragment.get_mut("generationConfig") {
        Some(generation) => Some(generation),
        None => fragment
            .get_mut("request")
            .and_then(|envelope| envelope.get_mut("generationConfig")),
    };
    if let Some(serde_json::Value::Object(generation)) = generation {
        if !generation.contains_key("maxOutputTokens") {
            generation.insert("maxOutputTokens".into(), serde_json::Value::from(ceiling));
        }
    }
}
/// The level→fragment table for one dialect.
///
/// These are the documented defaults from `plan.md` §4.5. The relay merges the selected fragment
/// into the upstream body; Google, Antigravity and Google AI Studio take thinking inside a nested
/// `generationConfig`, so their fragments are shaped to reach that field and are fitted to the
/// budget range the surface accepts.
fn dialect_reasoning_fragment(
    dialect: Dialect,
    level: ReasoningLevel,
) -> Option<serde_json::Value> {
    if level == ReasoningLevel::None {
        // `none` emits no reasoning fragment at all, on every dialect. Sending
        // `{"reasoning_effort":"none"}` to a provider that does not recognise the value is a 400,
        // and omitting the field is how every one of them expresses "do not reason".
        return None;
    }
    if dialect == Dialect::Cursor {
        // The Cursor agent frame carries no reasoning field of any kind: the model is named by id
        // and the server decides how much to think. `expressible_reasoning_levels` therefore offers
        // only `none`, and a non-`none` level reaching this point means a hand-built step bypassed
        // the compiler. Emitting a fragment would be pointless — the relay's Cursor encoder reads
        // only the model id from this body — so the honest answer is the same one `none` gets.
        return None;
    }
    let effort = effort_spelling(dialect, level);
    Some(match dialect {
        // Cursor is grouped here only because the match must be total: the early return above
        // answers for it before this table is reached, and reading the effort spelling the other
        // dialects use is a truer summary of "no field on the wire" than inventing a shape.
        Dialect::OpenAi | Dialect::Copilot | Dialect::CommandCodeGo | Dialect::Cursor => {
            serde_json::json!({ "reasoning_effort": effort })
        }
        Dialect::Codex => {
            serde_json::json!({ "reasoning": { "effort": effort, "summary": "auto" } })
        }
        // ZCode is the same body shape as Anthropic — the app builds it with the AI SDK's
        // Anthropic client — so it takes the same `thinking` fragment, budget and all. Claude
        // Code's surface is Anthropic Messages unchanged apart from the identity block, so the
        // same `thinking` fragment is what its models accept.
        Dialect::Anthropic | Dialect::Zcode | Dialect::ClaudeCli => serde_json::json!({
            "thinking": { "type": "enabled", "budget_tokens": thinking_budget(level) }
        }),
        // The AI Studio surface takes a level **or** a budget and refuses a body carrying both —
        // "You can only set only one of thinking budget and thinking level." — and its budget is
        // capped at 65535 where Cloud Code takes 131072. So this dialect is sent the level alone:
        // a dimensionless switch is the only thinking control that cannot fall outside a range,
        // and sending one field cannot collide with a rule about sending two. Everything from
        // `medium` up is the surface's `high`, exactly as on the dialects above, because that is
        // the vocabulary the Gemini 3 models publish (`low`, `high`; not `minimal`).
        Dialect::GoogleAiStudio => serde_json::json!({
            "generationConfig": {
                "thinkingConfig": {
                    "thinkingLevel": if level.rank() >= ReasoningLevel::High.rank() { "high" } else { "low" },
                }
            }
        }),
        Dialect::ModalVllm => serde_json::json!({ "reasoning_effort": effort }),
        // Google and Antigravity take thinking inside `generationConfig`. The relay's merge is
        // recursive, so the fragment is shaped exactly as deep as the field it has to reach.
        // `thinkingLevel` is a two-value switch in the Gemini API, so everything from `medium` up
        // is `high` with the corresponding budget.
        Dialect::Google | Dialect::Antigravity => {
            let generation = serde_json::json!({
                "generationConfig": {
                    "thinkingConfig": {
                        "thinkingLevel": if level.rank() >= ReasoningLevel::High.rank() { "high" } else { "low" },
                        "thinkingBudget": thinking_budget(level),
                    }
                }
            });
            if dialect == Dialect::Antigravity {
                // Antigravity's body is an envelope (`{project, model, request, userAgent}`), so a
                // fragment aimed at the Gemini payload has to sit under `request`. At the envelope's
                // top level it is not a field of anything: the upstream answers 400
                // `Unknown name "generationConfig": Cannot find field.`
                serde_json::json!({ "request": generation })
            } else {
                generation
            }
        }
    })
}

/// The `reasoning_effort` string a provider accepts for a level.
///
/// These dialects name the effort, so their vocabulary is the six strings listed in
/// [`Dialect::expressible_reasoning_levels`]. `ultra` and `max` are not among them and a compiled
/// step never offers either: the request reroutes to the model's highest offered tier through the
/// ordinary `-<level>` rule. The arms below remain as a last-resort mapping so a malformed or
/// hand-built step cannot put a string the upstream has never heard of into a body; reaching them
/// would mean the compiled level mask was bypassed.
fn effort_spelling(dialect: Dialect, level: ReasoningLevel) -> &'static str {
    match dialect {
        // `xhigh` is the ceiling for the OpenAI-shaped providers, so the two tiers above it land
        // there rather than being passed through.
        Dialect::OpenAi | Dialect::Copilot | Dialect::CommandCodeGo | Dialect::Codex => match level
        {
            ReasoningLevel::Minimal => "minimal",
            ReasoningLevel::Low => "low",
            ReasoningLevel::Medium => "medium",
            ReasoningLevel::High => "high",
            ReasoningLevel::XHigh | ReasoningLevel::Ultra | ReasoningLevel::Max => "xhigh",
            ReasoningLevel::None => "none",
        },
        // Anthropic's `thinking.budget_tokens` is written by the fragment, not by this spelling,
        // so the canonical name is the honest answer for any dialect this table does not know.
        _ => level.as_str(),
    }
}

/// The thinking-token budget for a level, ported from OmniRoute's `EFFORT_BUDGETS` (MIT).
///
/// Claude's `thinking.budget_tokens` has to be a real number, so unlike `reasoning_effort` there is
/// nothing to omit. `high` is deliberately the provider's own maximum rather than a computed guess,
/// and it is only a starting point: what a model can actually emit is the *model's* ceiling, not
/// this table's, so [`fit_thinking_budget`] fits the number to the ceiling a model declares and the
/// relay fits it again to the `max_tokens` a particular request states. Sending 131072 to a model
/// that can emit 64000 is not merely wasteful — Anthropic rejects the pair outright with
/// `max_tokens must be greater than thinking.budget_tokens` — so this table alone must never reach
/// a body.
const fn thinking_budget(level: ReasoningLevel) -> u32 {
    match level {
        ReasoningLevel::None => 0,
        ReasoningLevel::Minimal | ReasoningLevel::Low => 1024,
        ReasoningLevel::Medium => 10240,
        ReasoningLevel::High
        | ReasoningLevel::XHigh
        | ReasoningLevel::Ultra
        | ReasoningLevel::Max => 131_072,
    }
}

/// Room left under a ceiling so an answer can follow the thinking that was paid for.
///
/// A budget that consumes the whole output allowance is the empty-answer failure: the model thinks
/// until the candidate is exhausted, the upstream reports `MAX_TOKENS`, and a client that does not
/// render reasoning sees a successful reply with no text in it.
pub const THINKING_HEADROOM_TOKENS: u32 = 4096;

/// The smallest thinking budget worth sending.
///
/// Anthropic refuses `budget_tokens` below 1024, and below this there is no room to think *and*
/// answer anyway, so a budget squeezed under it is dropped by omitting the field entirely rather
/// than by sending a number the upstream will reject.
pub const MIN_THINKING_BUDGET: u32 = 1024;

/// The spellings a dialect may use for a thinking budget.
const BUDGET_KEYS: [&str; 4] = [
    "budget_tokens",
    "budgetTokens",
    "thinking_budget",
    "thinkingBudget",
];

/// Does anything in `value` name a thinking budget?
fn mentions_thinking_budget(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => map.iter().any(|(key, inner)| {
            BUDGET_KEYS.contains(&key.as_str()) || mentions_thinking_budget(inner)
        }),
        serde_json::Value::Array(items) => items.iter().any(mentions_thinking_budget),
        _ => false,
    }
}

/// Lower every thinking budget inside `value` to at most `cap`, in place.
fn lower_thinking_budgets(value: &mut serde_json::Value, cap: u32) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map.iter_mut() {
                if BUDGET_KEYS.contains(&key.as_str()) {
                    if let Some(number) = inner.as_u64() {
                        if number > u64::from(cap) {
                            *inner = serde_json::Value::from(cap);
                        }
                        continue;
                    }
                }
                lower_thinking_budgets(inner, cap);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                lower_thinking_budgets(item, cap);
            }
        }
        _ => {}
    }
}

/// The thinking-budget range a dialect accepts, when the dialect publishes one.
///
/// Only the AI Studio surface states a numeric range for the budget — and it states it as an
/// error, "thinking_budget must be in the range [-1, 65535]", which is a bound a request has to
/// respect just as much as a schema would be. Cloud Code and Anthropic publish no such range: for
/// them the honest bound is the model's own output allowance, which is what a ceiling is for.
///
/// `-1` is inside the range and means "let the model decide", so the lower bound needs no code:
/// it is not a budget this module writes, and [`lower_thinking_budgets`] only lowers a number.
pub const fn dialect_budget_ceiling(dialect: Dialect) -> Option<u32> {
    match dialect {
        Dialect::GoogleAiStudio => Some(65_535),
        _ => None,
    }
}

/// Fit the thinking budget in `fragment` under `ceiling` output tokens, in place.
///
/// Returns whether the fragment still says anything worth sending. `false` means the ceiling leaves
/// no room to both think and answer, and the caller must drop the fragment rather than send a
/// thinking level the model cannot honour.
///
/// Two bounds can apply and the tighter one wins: the model's output allowance, which the budget is
/// carved out of, and the dialect's own accepted range. A fragment that names no budget is left
/// untouched, and so is one whose bounds are both unknown — a model that declares no output limit
/// and a dialect that states no range have said nothing to fit against, and inventing a limit would
/// be inventing a capability. Clamping only ever lowers a number, so it is safe to apply to a
/// fragment the operator authored as well as to a dialect default.
pub fn fit_thinking_budget(
    dialect: Dialect,
    fragment: &mut serde_json::Value,
    ceiling: Option<u32>,
) -> bool {
    if !mentions_thinking_budget(fragment) {
        return true;
    }
    let model_cap = ceiling.map(|ceiling| ceiling.saturating_sub(THINKING_HEADROOM_TOKENS));
    let Some(cap) = model_cap
        .into_iter()
        .chain(dialect_budget_ceiling(dialect))
        .min()
    else {
        return true;
    };
    if cap < MIN_THINKING_BUDGET {
        return false;
    }
    lower_thinking_budgets(fragment, cap);
    true
}

/// `fnv1a64` over the routing-relevant fields of a compiled combo.
///
/// Fields that cannot affect routing — the label, the note, the panel sort order — are excluded on
/// purpose, so cosmetic edits do not invalidate affinity or force a reload.
fn plan_signature(plan: &ComboPlan) -> u64 {
    let mut hash = fnv1a64(plan.id.as_str().as_bytes());
    hash ^= fnv1a64(plan.name.as_bytes());
    hash ^= fnv1a64(&[plan.strategy as u8]);
    hash ^= fnv1a64(&[plan.midstream_policy as u8]);
    hash ^= fnv1a64(&plan.max_retries.to_le_bytes());
    hash ^= fnv1a64(&plan.retry_delay_ms.to_le_bytes());
    hash ^= fnv1a64(&[plan.affinity as u8]);
    hash ^= fnv1a64(&plan.sticky_success_window_secs.to_le_bytes());
    hash ^= fnv1a64(&[plan.compression.enabled as u8]);
    hash ^= fnv1a64(&[compression_mode_tag(plan.compression.mode)]);
    hash ^= fnv1a64(&(plan.compression.min_bytes as u64).to_le_bytes());
    hash ^= fnv1a64(&[plan.hedge.enabled as u8]);
    hash ^= fnv1a64(&plan.hedge.window_ms.to_le_bytes());
    hash ^= fnv1a64(&plan.hedge.loser_grace_ms.to_le_bytes());
    hash ^= fnv1a64(&[plan.hedge.cancel_losers as u8]);
    hash ^= fnv1a64(plan.hedge.winner_metric.as_str().as_bytes());
    hash ^= fnv1a64(&[winner_fail_tag(&plan.hedge.on_winner_fail)]);
    match &plan.hedge.on_winner_fail {
        WinnerFailMode::RerunExcluding { max_rounds } => hash ^= fnv1a64(&[*max_rounds]),
        WinnerFailMode::StableFallback { step } => hash ^= fnv1a64(&step.to_le_bytes()),
        WinnerFailMode::PickHighestPassed | WinnerFailMode::PickHighestPassedDifferentProvider => {}
    }
    for participant in &plan.hedge.participants {
        hash ^= fnv1a64(&participant.to_le_bytes());
    }
    for step in &plan.steps {
        hash ^= fnv1a64(&step.provider_index.to_le_bytes());
        hash ^= fnv1a64(step.provider_id.as_str().as_bytes());
        hash ^= fnv1a64(step.model.as_bytes());
        hash ^= fnv1a64(&[step.dialect as u8]);
        hash ^= fnv1a64(step.base_url.as_bytes());
        hash ^= fnv1a64(&step.connect_timeout_ms.to_le_bytes());
        hash ^= fnv1a64(&step.ttfb_timeout_ms.to_le_bytes());
        hash ^= fnv1a64(&step.idle_timeout_ms.to_le_bytes());
        hash ^= fnv1a64(&step.total_timeout_ms.to_le_bytes());
    }
    hash
}

/// Flatten authored aliases to their final targets, rejecting cycles.
///
/// A cycle has no correct flattened target, so it is a compile error naming the alias rather than
/// something the relay has to detect at request time. An alias whose target does not exist is kept
/// as-is: a dangling alias simply never resolves, which is the least surprising behaviour while a
/// provider is being added.
fn compile_aliases(config: &Config) -> Result<Vec<AliasPlan>> {
    let mut authored: Vec<(String, String, bool)> = Vec::with_capacity(config.aliases.len());
    let mut seen: HashSet<String> = HashSet::new();
    for alias in &config.aliases {
        if alias.generated {
            // These aliases were created by a now-retired naming convention. The config loader
            // removes them with a backup, but filtering here also protects callers that compile an
            // in-memory Config directly (tests, embedders and future control-plane code) from
            // quietly reintroducing the old bare-model route.
            continue;
        }
        let from = alias.from.trim();
        let to = alias.to.trim();
        if from.is_empty() || to.is_empty() {
            continue;
        }
        if !seen.insert(from.to_ascii_lowercase()) {
            // First definition wins, matching the index's first-inserted-head rule.
            continue;
        }
        authored.push((from.to_string(), to.to_string(), alias.generated));
    }

    let lookup: HashMap<String, String> = authored
        .iter()
        .map(|(from, to, _)| (from.to_ascii_lowercase(), to.clone()))
        .collect();

    let mut out = Vec::with_capacity(authored.len());
    for (from, to, generated) in authored {
        let mut target = to;
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(from.to_ascii_lowercase());
        loop {
            let folded = target.to_ascii_lowercase();
            if !visited.insert(folded.clone()) {
                return Err(Error::Snapshot(format!(
                    "alias {from:?} forms a cycle: {from:?} -> ... -> {target:?} -> ... back to an \
                     earlier alias"
                )));
            }
            match lookup.get(&folded) {
                Some(next) => target = next.clone(),
                None => break,
            }
        }
        out.push(AliasPlan {
            from,
            to: target,
            generated,
        });
    }
    Ok(out)
}

/// Encode a provider.
fn encode_provider(encoder: &mut Encoder, provider: &ProviderPlan) {
    encoder.put_str(provider.id.as_str());
    let label = encoder.opt_str_id(provider.label.as_deref());
    encoder.put_u32(label);
    encoder.put_u8(provider.dialect as u8);
    encoder.put_str(&provider.base_url);
    encode_auth(encoder, &provider.auth);
    encoder.put_u8(provider.key_strategy as u8);
    encoder.put_u8(provider.cooldown_scope as u8);
    let proxy = encoder.opt_str_id(provider.proxy.as_deref());
    encoder.put_u32(proxy);
    encoder.put_u32(provider.capabilities.len() as u32);
    for capability in &provider.capabilities {
        encoder.put_u8(*capability as u8);
    }
    encoder.put_u32(provider.accounts.len() as u32);
    for account in &provider.accounts {
        encode_account(encoder, account);
    }
    encoder.put_u64(provider.connect_timeout_ms);
    encoder.put_u64(provider.ttfb_timeout_ms);
    encoder.put_u64(provider.idle_timeout_ms);
    encoder.put_u64(provider.total_timeout_ms);
    encode_string_map(encoder, &provider.headers);
    encode_string_map(encoder, &provider.route_overrides);
    encoder.put_u8(http2_tag(provider.http2));
    encoder.put_bool(provider.prewarm);
    encoder.put_u64(provider.kv_pool_tokens.unwrap_or(0));
    encoder.put_u64(provider.max_context.unwrap_or(0));
    encoder.put_u32(provider.max_concurrent.unwrap_or(0));
    encoder.put_u64(provider.price_per_second.map(f64::to_bits).unwrap_or(0));
    encoder.put_u64(provider.cold_start_ttfb_timeout_ms.unwrap_or(0));
    encoder.put_u32(provider.steps.len() as u32);
    for step in &provider.steps {
        encode_step(encoder, step);
    }
    encode_string_list(encoder, &provider.hidden_models);
}

/// Encode an account.
fn encode_account(encoder: &mut Encoder, account: &AccountPlan) {
    encoder.put_str(account.id.as_str());
    let label = encoder.opt_str_id(account.label.as_deref());
    encoder.put_u32(label);
    let secret = encoder.opt_str_id(account.secret_ref.as_deref());
    encoder.put_u32(secret);
    let oauth = encoder.opt_str_id(account.oauth_ref.as_deref());
    encoder.put_u32(oauth);
    encoder.put_i32(account.priority);
    encoder.put_u32(account.weight);
    encoder.put_u32(account.max_concurrent);
    encode_string_list(encoder, &account.models_allow);
    encode_string_list(encoder, &account.models_deny);
    let proxy = encoder.opt_str_id(account.proxy.as_deref());
    encoder.put_u32(proxy);
    // Named placeholder values, as name/value pairs. A map rather than a fixed pair of fields
    // because the names come from the catalogue: Cloudflare asks for `{account_id}`, Snowflake for
    // `{account}`, and a third provider will ask for something else.
    encoder.put_u32(u32::try_from(account.placeholders.len()).unwrap_or(u32::MAX));
    for (name, value) in &account.placeholders {
        encoder.put_str(name);
        encoder.put_str(value);
    }
    // Declared credit in whole cents: `u64::MAX` is "not declared", every other value is a
    // balance. A sentinel rather than a separate presence byte because no real balance is within
    // 18 quintillion dollars of the maximum.
    encoder.put_u64(account.credit_cents.unwrap_or(u64::MAX));
    // The account's effective quota windows — the ones that can stop it. Encoded so the data plane
    // enforces exactly what the operator declared without re-reading the configuration on the
    // request path, which is the same reason a client key's windows are compiled in.
    encoder.put_u32(u32::try_from(account.quota.len()).unwrap_or(u32::MAX));
    for window in &account.quota {
        encode_quota_window(encoder, window);
    }
}

/// Encode one declared quota window.
///
/// Every dimension is written, including the ones at zero, because "unlimited tokens" and "no
/// token dimension at all" are the same statement here — the window says which dimensions it
/// limits by setting them above zero, and a decoder that guessed would be inventing a limit.
fn encode_quota_window(encoder: &mut Encoder, window: &crate::model::QuotaWindow) {
    let name = encoder.opt_str_id(window.name.as_deref());
    encoder.put_u32(name);
    encoder.put_u8(window.kind as u8);
    encoder.put_i64(window.seconds);
    encoder.put_u64(window.limit_requests);
    encoder.put_u64(window.limit_tokens);
    encoder.put_u64(window.limit_input_tokens);
    encoder.put_u64(window.limit_output_tokens);
    encoder.put_u64(window.limit_usd_cents);
    encoder.put_bool(window.reset_only);
}

/// Encode an auth kind.
fn encode_auth(encoder: &mut Encoder, auth: &AuthKind) {
    match auth {
        AuthKind::Bearer => encoder.put_u8(0),
        AuthKind::ApiKeyHeader { header } => {
            encoder.put_u8(1);
            let id = encoder.str_id(header);
            encoder.put_u32(id);
        }
        AuthKind::Header { name, prefix } => {
            encoder.put_u8(2);
            let name_id = encoder.str_id(name);
            let prefix_id = encoder.str_id(prefix);
            encoder.put_u32(name_id);
            encoder.put_u32(prefix_id);
        }
        AuthKind::QueryParam { param } => {
            encoder.put_u8(3);
            let id = encoder.str_id(param);
            encoder.put_u32(id);
        }
        AuthKind::Modal => encoder.put_u8(5),
        AuthKind::OAuth { platform } => {
            encoder.put_u8(4);
            let id = encoder.str_id(platform);
            encoder.put_u32(id);
        }
    }
}

/// Encode every field of a step.
///
/// The base URL, timeouts and auth are copied from the provider when the step is compiled, and are
/// deliberately **not** repeated here: the provider each step names is encoded once, and repeating
/// a base URL per model would multiply the frame by the size of the model list. What *is* per-step
/// is the reasoning metadata — a level, the set the model offers, the per-level fragments and the
/// modality mask — because that depends on the model entry, which the frame does not carry
/// otherwise. `decode` reconstructs the provider-owned fields from the provider the index names.
fn encode_step(encoder: &mut Encoder, step: &StepPlan) {
    encoder.put_u32(step.provider_index);
    encoder.put_str(&step.model);
    encoder.put_u8(step.level);
    encoder.put_u32(step.levels);
    encoder.put_u8(step.default_level);
    for fragment in &step.reasoning_json {
        let id = encoder.opt_str_id(fragment.as_deref());
        encoder.put_u32(id);
    }
    encoder.put_u32(step.input_modalities);
    encode_pricing(encoder, step.pricing.as_ref());
    encoder.put_u32(step.context_length.unwrap_or(0));
    // A presence byte rather than a count, because "no selection" and "a selection that matched
    // nothing" are different answers: the first means every account, the second means none.
    match &step.accounts {
        None => encoder.put_bool(false),
        Some(accounts) => {
            encoder.put_bool(true);
            encoder.put_u32(accounts.len() as u32);
            for account in accounts {
                encoder.put_str(account.as_str());
            }
        }
    }
}

/// Encode an optional price table.
///
/// Rates are `f64` and go out as their IEEE-754 bit patterns: JSON's decimal round-trip is lossy
/// enough that a price could come back a fraction of a cent different, and a cost estimate that
/// drifts on every reload is worse than one that is merely stale.
fn encode_pricing(encoder: &mut Encoder, pricing: Option<&PricingPlan>) {
    match pricing {
        None => encoder.put_bool(false),
        Some(pricing) => {
            encoder.put_bool(true);
            for rate in [
                pricing.input_per_mtok,
                pricing.output_per_mtok,
                pricing.cache_read_per_mtok,
                pricing.cache_write_per_mtok,
            ] {
                match rate {
                    Some(value) => {
                        encoder.put_bool(true);
                        encoder.put_u64(value.to_bits());
                    }
                    None => encoder.put_bool(false),
                }
            }
            encoder.put_u8(pricing_source_tag(pricing.source));
        }
    }
}

/// Encode a pricing source.
fn pricing_source_tag(source: PricingSource) -> u8 {
    match source {
        PricingSource::Bundled => 0,
        PricingSource::Synced => 1,
        PricingSource::Manual => 2,
    }
}

/// Decode a pricing source.
fn pricing_source_from_tag(tag: u8) -> Result<PricingSource> {
    Ok(match tag {
        0 => PricingSource::Bundled,
        1 => PricingSource::Synced,
        2 => PricingSource::Manual,
        other => {
            return Err(Error::Snapshot(format!(
                "snapshot pricing source tag {other} is not one this binary knows"
            )))
        }
    })
}

/// Decode an optional price table written by [`encode_pricing`].
fn decode_pricing(reader: &mut Reader) -> Result<Option<PricingPlan>> {
    if !reader.bool()? {
        return Ok(None);
    }
    let mut rates = [None; 4];
    for slot in rates.iter_mut() {
        if reader.bool()? {
            *slot = Some(f64::from_bits(reader.u64()?));
        }
    }
    Ok(Some(PricingPlan {
        input_per_mtok: rates[0],
        output_per_mtok: rates[1],
        cache_read_per_mtok: rates[2],
        cache_write_per_mtok: rates[3],
        source: pricing_source_from_tag(reader.u8()?)?,
    }))
}

/// Decode the per-step reasoning metadata, then fill in the provider-owned fields.
///
/// `provider` is the provider this step belongs to; `any_model` stands in for a combo step that
/// names a model the provider no longer carries, which `Config::validate` reports and the frame
/// must still encode.
fn decode_step(
    reader: &mut Reader,
    strings: &[String],
    provider_index: u32,
    provider: &ProviderPlan,
) -> Result<StepPlan> {
    let model = reader.string(strings)?;
    let level = read_level(reader)?;
    let levels = reader.u32()?;
    let default_level = read_level(reader)?;
    let mut reasoning_json: [Option<String>; 8] = Default::default();
    for slot in reasoning_json.iter_mut() {
        *slot = reader.opt_string(strings)?;
    }
    let input_modalities = reader.u32()?;
    let pricing = decode_pricing(reader)?;
    let context_length = match reader.u32()? {
        0 => None,
        value => Some(value),
    };
    let accounts = if reader.bool()? {
        let count = reader.u32()? as usize;
        let mut accounts = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            let id = AccountId::from(reader.string(strings)?);
            // A selection naming an account the provider does not have would be a step that can
            // never serve, and the frame is the last place that can be caught cheaply.
            if !provider.accounts.iter().any(|account| account.id == id) {
                return Err(Error::Snapshot(format!(
                    "step for provider {} selects unknown account {id}",
                    provider.id
                )));
            }
            accounts.push(id);
        }
        Some(accounts)
    } else {
        None
    };
    Ok(StepPlan {
        provider_index,
        provider_id: provider.id.clone(),
        model,
        dialect: provider.dialect,
        auth: provider.auth.clone(),
        base_url: provider.base_url.clone(),
        connect_timeout_ms: provider.connect_timeout_ms,
        ttfb_timeout_ms: provider.ttfb_timeout_ms,
        idle_timeout_ms: provider.idle_timeout_ms,
        total_timeout_ms: provider.total_timeout_ms,
        level,
        levels,
        default_level,
        reasoning_json,
        input_modalities,
        pricing,
        context_length,
        accounts,
    })
}

/// Encode a combo.
fn encode_combo(encoder: &mut Encoder, combo: &ComboPlan) {
    encoder.put_str(combo.id.as_str());
    encoder.put_str(&combo.name);
    encoder.put_u8(combo.strategy as u8);
    encoder.put_u8(combo.midstream_policy as u8);
    encoder.put_u8(combo.max_retries);
    encoder.put_u64(combo.retry_delay_ms);
    encoder.put_bool(combo.affinity);
    encoder.put_bool(combo.compression.enabled);
    encoder.put_u8(compression_mode_tag(combo.compression.mode));
    encoder.put_u64(combo.compression.min_bytes as u64);
    encoder.put_bool(combo.compression.never_grow);
    encoder.put_bool(combo.compression.never_empty);
    encoder.put_bool(combo.hedge.enabled);
    encoder.put_u32(combo.sticky_success_window_secs);
    encoder.put_u16(combo.hedge.window_ms);
    encoder.put_u16(combo.hedge.loser_grace_ms);
    encoder.put_u8(combo.hedge.winner_metric as u8);
    encoder.put_bool(combo.hedge.cancel_losers);
    encoder.put_u8(winner_fail_tag(&combo.hedge.on_winner_fail));
    match &combo.hedge.on_winner_fail {
        WinnerFailMode::RerunExcluding { max_rounds } => encoder.put_u8(*max_rounds),
        WinnerFailMode::StableFallback { step } => encoder.put_u16(*step),
        WinnerFailMode::PickHighestPassed | WinnerFailMode::PickHighestPassedDifferentProvider => {}
    }
    encoder.put_u32(combo.hedge.participants.len() as u32);
    for participant in &combo.hedge.participants {
        encoder.put_u16(*participant);
    }
    encoder.put_u64(combo.signature);
    encoder.put_u32(combo.steps.len() as u32);
    for step in &combo.steps {
        encode_step(encoder, step);
    }
}

/// Encode an alias.
fn encode_alias(encoder: &mut Encoder, alias: &AliasPlan) {
    encoder.put_str(&alias.from);
    encoder.put_str(&alias.to);
    encoder.put_bool(alias.generated);
}

/// Encode a client key plan, including the three rolling windows.
///
/// A window is written as a presence byte followed by its two caps rather than as two u64s where
/// zero means absent, because `None` (not enforced) and `Some(0)` (enforced, uncapped dimension) are
/// different answers and the relay must not have to infer which one it was given.
fn encode_key(encoder: &mut Encoder, key: &KeyPlan) {
    encoder.put_str(key.id.as_str());
    encoder.put_str(&key.label);
    encoder.put_str(&key.key_hash);
    let prefix = encoder.opt_str_id(key.prefix.as_deref());
    encoder.put_u32(prefix);
    encode_string_list(encoder, &key.allow_models);
    encode_string_list(encoder, &key.allow_routes);
    encoder.put_u64(key.limit_requests);
    encode_key_limit(encoder, key.limits.session_5h.as_ref());
    encode_key_limit(encoder, key.limits.weekly_1w.as_ref());
    encode_key_limit(encoder, key.limits.monthly_30d.as_ref());
}

/// Encode one window's caps, or the absence of the window.
fn encode_key_limit(encoder: &mut Encoder, limit: Option<&KeyLimit>) {
    match limit {
        Some(limit) => {
            encoder.put_bool(true);
            encoder.put_u64(limit.tokens);
            encoder.put_u64(limit.requests);
        }
        None => encoder.put_bool(false),
    }
}

/// Encode one proxy plan.
fn encode_proxy(encoder: &mut Encoder, proxy: &ProxyPlan) {
    encoder.put_str(&proxy.id);
    let label = encoder.opt_str_id(proxy.label.as_deref());
    encoder.put_u32(label);
    encoder.put_str(&proxy.url);
    let username = encoder.opt_str_id(proxy.username.as_deref());
    encoder.put_u32(username);
    let secret = encoder.opt_str_id(proxy.secret_ref.as_deref());
    encoder.put_u32(secret);
    encoder.put_i32(proxy.priority);
}

/// The on-disk tag for a proxy strategy.
///
/// Written explicitly rather than as the discriminant so that reordering the authored enum cannot
/// silently change the meaning of an already-written snapshot.
fn proxy_strategy_tag(strategy: ProxyStrategy) -> u8 {
    match strategy {
        ProxyStrategy::Priority => 0,
        ProxyStrategy::LeastUsed => 1,
        ProxyStrategy::Random => 2,
        ProxyStrategy::RoundRobin => 3,
    }
}

/// Read a proxy strategy tag, rejecting one this binary does not know.
fn proxy_strategy_from_tag(tag: u8) -> Result<ProxyStrategy> {
    match tag {
        0 => Ok(ProxyStrategy::Priority),
        1 => Ok(ProxyStrategy::LeastUsed),
        2 => Ok(ProxyStrategy::Random),
        3 => Ok(ProxyStrategy::RoundRobin),
        other => Err(Error::Snapshot(format!(
            "snapshot names unknown proxy strategy tag {other}"
        ))),
    }
}

/// Encode one lookup entry. The `next` chain is rebuilt on decode rather than persisted.
fn encode_index_entry(encoder: &mut Encoder, entry: &IndexEntry) {
    encoder.put_str(&entry.key);
    encoder.put_u8(entry.rank);
    match &entry.target {
        IndexTarget::Alias { alias } => {
            encoder.put_u8(0);
            encoder.put_u32(*alias);
        }
        IndexTarget::Combo { combo, level } => {
            encoder.put_u8(1);
            encoder.put_u32(*combo);
            match level {
                Some(level) => {
                    encoder.put_bool(true);
                    encoder.put_u8(*level);
                }
                None => encoder.put_bool(false),
            }
        }
        IndexTarget::Step {
            provider,
            step,
            level,
            requested,
        } => {
            encoder.put_u8(2);
            encoder.put_u32(*provider);
            encoder.put_u32(*step);
            encoder.put_u8(*level);
            encoder.put_u8(*requested);
        }
    }
}

/// Encode a `BTreeMap<String, String>` deterministically.
fn encode_string_map(encoder: &mut Encoder, map: &BTreeMap<String, String>) {
    encoder.put_u32(map.len() as u32);
    for (key, value) in map {
        let key_id = encoder.str_id(key);
        let value_id = encoder.str_id(value);
        encoder.put_u32(key_id);
        encoder.put_u32(value_id);
    }
}

/// Encode a list of strings as a count followed by string-table indices.
fn encode_string_list(encoder: &mut Encoder, list: &[String]) {
    encoder.put_u32(list.len() as u32);
    for item in list {
        encoder.put_str(item);
    }
}

/// Decode the string table.
fn decode_string_table(reader: &mut Reader) -> Result<Vec<String>> {
    let count = reader.u32()? as usize;
    let mut strings = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let length = reader.u32()? as usize;
        let bytes = reader.take(length)?;
        let string = std::str::from_utf8(bytes).map_err(|e| {
            Error::Snapshot(format!(
                "snapshot string table entry is not valid UTF-8: {e}"
            ))
        })?;
        strings.push(string.to_string());
    }
    Ok(strings)
}

/// Decode the provider list.
fn decode_providers(reader: &mut Reader, strings: &[String]) -> Result<Vec<ProviderPlan>> {
    let count = reader.u32()? as usize;
    let mut providers = Vec::with_capacity(count.min(4096));
    for index in 0..count {
        let id = ProviderId::from(reader.string(strings)?);
        let label = reader.opt_string(strings)?;
        let dialect = dialect_from_tag(reader.u8()?)?;
        let base_url = reader.string(strings)?;
        let auth = decode_auth(reader, strings)?;
        let key_strategy = key_strategy_from_tag(reader.u8()?)?;
        let cooldown_scope = cooldown_scope_from_tag(reader.u8()?)?;
        let proxy = reader.opt_string(strings)?;
        let capability_count = reader.u32()? as usize;
        let mut capabilities = Vec::with_capacity(capability_count.min(64));
        for _ in 0..capability_count {
            capabilities.push(capability_from_tag(reader.u8()?)?);
        }
        let account_count = reader.u32()? as usize;
        let mut accounts = Vec::with_capacity(account_count.min(1024));
        for _ in 0..account_count {
            accounts.push(decode_account(reader, strings)?);
        }
        let connect_timeout_ms = reader.u64()?;
        let ttfb_timeout_ms = reader.u64()?;
        let idle_timeout_ms = reader.u64()?;
        let total_timeout_ms = reader.u64()?;
        let headers = decode_string_map(reader, strings)?;
        let route_overrides = decode_string_map(reader, strings)?;
        let http2 = http2_from_tag(reader.u8()?)?;
        let prewarm = reader.bool()?;
        let kv_pool_tokens = match reader.u64()? {
            0 => None,
            n => Some(n),
        };
        let max_context = match reader.u64()? {
            0 => None,
            n => Some(n),
        };
        let max_concurrent = match reader.u32()? {
            0 => None,
            n => Some(n),
        };
        let price_bits = reader.u64()?;
        let price_per_second = if price_bits == 0 {
            None
        } else {
            Some(f64::from_bits(price_bits))
        };
        let cold_start_ttfb_timeout_ms = match reader.u64()? {
            0 => None,
            n => Some(n),
        };

        let mut provider = ProviderPlan {
            id,
            label,
            dialect,
            base_url,
            auth,
            key_strategy,
            cooldown_scope,
            accounts,
            proxy,
            capabilities,
            connect_timeout_ms,
            ttfb_timeout_ms,
            idle_timeout_ms,
            total_timeout_ms,
            headers,
            route_overrides,
            http2,
            prewarm,
            kv_pool_tokens,
            max_context,
            max_concurrent,
            price_per_second,
            cold_start_ttfb_timeout_ms,
            steps: Vec::new(),
            hidden_models: Vec::new(),
        };

        let step_count = reader.u32()? as usize;
        let mut steps = Vec::with_capacity(step_count.min(4096));
        for _ in 0..step_count {
            let step_provider = reader.u32()?;
            if step_provider as usize != index {
                return Err(Error::Snapshot(format!(
                    "snapshot provider {index} declares a step owned by provider index \
                     {step_provider}"
                )));
            }
            let step = decode_step(reader, strings, index as u32, &provider)?;
            let level = ReasoningLevel::from_u8(step.level).ok_or_else(|| {
                Error::Snapshot(format!(
                    "snapshot step {:?} carries reasoning level {}",
                    step.model, step.level
                ))
            })?;
            // A step with an empty level set has no notion of a tier at all, and stores the
            // no-reasoning level so it emits no fragment. Any *other* level must be one the step
            // says it offers, or the frame is internally inconsistent and a request could be
            // served at a tier the model does not have.
            if step.levels != 0 && !step.supports_level(level) {
                return Err(Error::Snapshot(format!(
                    "snapshot step {:?} is stored at level {}, which its own level set does not \
                     contain",
                    step.model, step.level
                )));
            }
            steps.push(step);
        }
        provider.steps = steps;
        // After the steps: the hidden list is a diagnostic, not a routing input, so a frame that
        // ends here without it is read as "nothing hidden" rather than as a corrupt provider. The
        // version gate is what makes that safe.
        provider.hidden_models = decode_string_list(reader, strings)?;
        providers.push(provider);
    }
    Ok(providers)
}

/// Decode the combo list, resolving steps against the already-decoded providers.
fn decode_combos(
    reader: &mut Reader,
    strings: &[String],
    providers: &[ProviderPlan],
) -> Result<Vec<ComboPlan>> {
    let count = reader.u32()? as usize;
    let mut combos = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let id = ComboId::from(reader.string(strings)?);
        let name = reader.string(strings)?;
        let strategy = combo_strategy_from_tag(reader.u8()?)?;
        let midstream_policy = midstream_from_tag(reader.u8()?)?;
        let max_retries = reader.u8()?;
        let retry_delay_ms = reader.u64()?;
        let affinity = reader.bool()?;

        let compression = CompressionSpec {
            enabled: reader.bool()?,
            mode: compression_mode_from_tag(reader.u8()?)?,
            min_bytes: reader.u64()? as usize,
            never_grow: reader.bool()?,
            never_empty: reader.bool()?,
        };

        let hedge_enabled = reader.bool()?;
        let sticky_success_window_secs = reader.u32()?;
        let window_ms = reader.u16()?;
        let loser_grace_ms = reader.u16()?;
        let winner_metric = winner_metric_from_tag(reader.u8()?)?;
        let cancel_losers = reader.bool()?;
        let on_winner_fail = decode_winner_fail(reader)?;
        let participant_count = reader.u32()? as usize;
        let mut participants = Vec::with_capacity(participant_count.min(4096));
        for _ in 0..participant_count {
            participants.push(reader.u16()?);
        }

        let signature = reader.u64()?;
        let step_count = reader.u32()? as usize;
        let mut steps = Vec::with_capacity(step_count.min(4096));
        for _ in 0..step_count {
            let provider_index = reader.u32()?;
            let provider = providers.get(provider_index as usize).ok_or_else(|| {
                Error::Snapshot(format!(
                    "snapshot combo step references provider index {provider_index}, but only {} \
                     providers exist",
                    providers.len()
                ))
            })?;
            steps.push(decode_step(reader, strings, provider_index, provider)?);
        }

        for participant in &participants {
            if *participant as usize >= steps.len() {
                return Err(Error::Snapshot(format!(
                    "snapshot combo hedge participant {participant} is out of range for a combo \
                     with {} steps",
                    steps.len()
                )));
            }
        }
        if let WinnerFailMode::StableFallback { step } = &on_winner_fail {
            if *step as usize >= steps.len() {
                return Err(Error::Snapshot(format!(
                    "snapshot combo stable-fallback step {step} is out of range for a combo with {} \
                     steps",
                    steps.len()
                )));
            }
        }

        combos.push(ComboPlan {
            id,
            name,
            strategy,
            steps,
            hedge: HedgePlan {
                enabled: hedge_enabled,
                participants,
                window_ms,
                winner_metric,
                on_winner_fail,
                cancel_losers,
                loser_grace_ms,
            },
            sticky_success_window_secs,
            midstream_policy,
            max_retries,
            retry_delay_ms,
            affinity,
            compression,
            signature,
        });
    }
    Ok(combos)
}

/// Decode the alias list.
fn decode_aliases(reader: &mut Reader, strings: &[String]) -> Result<Vec<AliasPlan>> {
    let count = reader.u32()? as usize;
    let mut aliases = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        aliases.push(AliasPlan {
            from: reader.string(strings)?,
            to: reader.string(strings)?,
            generated: reader.bool()?,
        });
    }
    Ok(aliases)
}

/// Read one window's caps, or `None` when the frame says the window is absent.
fn decode_key_limit(reader: &mut Reader) -> Result<Option<KeyLimit>> {
    if !reader.bool()? {
        return Ok(None);
    }
    Ok(Some(KeyLimit {
        tokens: reader.u64()?,
        requests: reader.u64()?,
    }))
}

/// Read the compiled client keys.
///
/// A key whose stored hash is empty is treated as a corrupt frame rather than as an inert key: no
/// real key has one, and accepting it would let a shifted read masquerade as a valid plan.
fn decode_keys(reader: &mut Reader, strings: &[String]) -> Result<Vec<KeyPlan>> {
    let count = reader.u32()? as usize;
    let mut keys = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let id = ClientKeyId::from(reader.string(strings)?);
        let label = reader.string(strings)?;
        let key_hash = reader.string(strings)?;
        if key_hash.trim().is_empty() {
            return Err(Error::Snapshot(format!(
                "snapshot client key {id} carries an empty key hash"
            )));
        }
        let prefix = reader.opt_string(strings)?;
        let allow_models = decode_string_list(reader, strings)?;
        let allow_routes = decode_string_list(reader, strings)?;
        let limit_requests = reader.u64()?;
        keys.push(KeyPlan {
            id,
            label,
            key_hash,
            prefix,
            allow_models,
            allow_routes,
            limit_requests,
            limits: KeyLimits {
                session_5h: decode_key_limit(reader)?,
                weekly_1w: decode_key_limit(reader)?,
                monthly_30d: decode_key_limit(reader)?,
            },
        });
    }
    Ok(keys)
}

/// Read the compiled proxy list.
fn decode_proxies(reader: &mut Reader, strings: &[String]) -> Result<Vec<ProxyPlan>> {
    let count = reader.u32()? as usize;
    let mut proxies = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        proxies.push(ProxyPlan {
            id: reader.string(strings)?,
            label: reader.opt_string(strings)?,
            url: reader.string(strings)?,
            username: reader.opt_string(strings)?,
            secret_ref: reader.opt_string(strings)?,
            priority: reader.i32()?,
        });
    }
    Ok(proxies)
}

/// Read a level byte, rejecting a value the ladder does not have.
///
/// A misread byte here would silently route a request at a tier the operator never asked for, so
/// this is a hard decode error rather than a clamp.
fn read_level(reader: &mut Reader) -> Result<u8> {
    let raw = reader.u8()?;
    if ReasoningLevel::from_u8(raw).is_none() {
        return Err(Error::Snapshot(format!(
            "snapshot carries reasoning level {raw}, which is not on the ladder"
        )));
    }
    Ok(raw)
}

/// Decode the lookup entries. The `next` chains are rebuilt by `link_entries`.
fn decode_index_entries(reader: &mut Reader, strings: &[String]) -> Result<Vec<IndexEntry>> {
    let count = reader.u32()? as usize;
    let mut entries = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        let key = reader.string(strings)?;
        let rank = reader.u8()?;
        let target = match reader.u8()? {
            0 => IndexTarget::Alias {
                alias: reader.u32()?,
            },
            1 => IndexTarget::Combo {
                combo: reader.u32()?,
                level: match reader.bool()? {
                    true => Some(read_level(reader)?),
                    false => None,
                },
            },
            2 => IndexTarget::Step {
                provider: reader.u32()?,
                step: reader.u32()?,
                level: read_level(reader)?,
                requested: read_level(reader)?,
            },
            tag => {
                return Err(Error::Snapshot(format!(
                    "snapshot lookup entry has unknown target tag {tag}"
                )))
            }
        };
        entries.push(IndexEntry {
            key,
            rank,
            target,
            next: None,
        });
    }
    Ok(entries)
}

/// Decode an account.
fn decode_account(reader: &mut Reader, strings: &[String]) -> Result<AccountPlan> {
    Ok(AccountPlan {
        id: AccountId::from(reader.string(strings)?),
        label: reader.opt_string(strings)?,
        secret_ref: reader.opt_string(strings)?,
        oauth_ref: reader.opt_string(strings)?,
        priority: reader.i32()?,
        weight: reader.u32()?,
        max_concurrent: reader.u32()?,
        models_allow: decode_string_list(reader, strings)?,
        models_deny: decode_string_list(reader, strings)?,
        proxy: reader.opt_string(strings)?,
        placeholders: {
            let count = reader.u32()? as usize;
            let mut placeholders = std::collections::BTreeMap::new();
            for _ in 0..count {
                let name = reader.string(strings)?;
                let value = reader.string(strings)?;
                placeholders.insert(name, value);
            }
            placeholders
        },
        credit_cents: match reader.u64()? {
            u64::MAX => None,
            cents => Some(cents),
        },
        quota: {
            let count = reader.u32()? as usize;
            let mut quota = Vec::with_capacity(count.min(64));
            for _ in 0..count {
                quota.push(decode_quota_window(reader, strings)?);
            }
            quota
        },
    })
}

/// Decode one declared quota window written by [`encode_quota_window`].
fn decode_quota_window(
    reader: &mut Reader,
    strings: &[String],
) -> Result<crate::model::QuotaWindow> {
    Ok(crate::model::QuotaWindow {
        name: reader.opt_string(strings)?,
        kind: window_kind_from_tag(reader.u8()?)?,
        seconds: reader.i64()?,
        limit_requests: reader.u64()?,
        limit_tokens: reader.u64()?,
        limit_input_tokens: reader.u64()?,
        limit_output_tokens: reader.u64()?,
        limit_usd_cents: reader.u64()?,
        reset_only: reader.bool()?,
    })
}

/// Decode a window kind tag.
fn window_kind_from_tag(tag: u8) -> Result<crate::dialect::WindowKind> {
    // `WindowKind` is `#[repr(u8)]` with tags from zero, so the tag *is* the discriminant; the
    // order is asserted by the exhaustive match's arms rather than by a hand-copied number.
    use crate::dialect::WindowKind;
    Ok(match tag {
        0 => WindowKind::Rolling,
        1 => WindowKind::Fixed,
        2 => WindowKind::ProviderDeclared,
        other => {
            return Err(Error::Snapshot(format!(
                "snapshot quota window kind {other} is not one this binary knows"
            )))
        }
    })
}

/// Decode an auth kind.
fn decode_auth(reader: &mut Reader, strings: &[String]) -> Result<AuthKind> {
    match reader.u8()? {
        0 => Ok(AuthKind::Bearer),
        1 => Ok(AuthKind::ApiKeyHeader {
            header: reader.string(strings)?,
        }),
        2 => {
            let name = reader.string(strings)?;
            let prefix = reader.string(strings)?;
            Ok(AuthKind::Header { name, prefix })
        }
        3 => Ok(AuthKind::QueryParam {
            param: reader.string(strings)?,
        }),
        4 => Ok(AuthKind::OAuth {
            platform: reader.string(strings)?,
        }),
        5 => Ok(AuthKind::Modal),
        tag => Err(Error::Snapshot(format!(
            "snapshot contains unknown auth kind tag {tag}"
        ))),
    }
}

/// Decode a `BTreeMap<String, String>`.
fn decode_string_map(reader: &mut Reader, strings: &[String]) -> Result<BTreeMap<String, String>> {
    let count = reader.u32()? as usize;
    let mut map = BTreeMap::new();
    for _ in 0..count {
        let key = reader.string(strings)?;
        let value = reader.string(strings)?;
        map.insert(key, value);
    }
    Ok(map)
}

/// Decode a count-prefixed list of strings.
fn decode_string_list(reader: &mut Reader, strings: &[String]) -> Result<Vec<String>> {
    let count = reader.u32()? as usize;
    let mut list = Vec::with_capacity(count.min(4096));
    for _ in 0..count {
        list.push(reader.string(strings)?);
    }
    Ok(list)
}

/// Decode a winner-failure mode and its payload.
fn decode_winner_fail(reader: &mut Reader) -> Result<WinnerFailMode> {
    match reader.u8()? {
        0 => Ok(WinnerFailMode::RerunExcluding {
            max_rounds: reader.u8()?,
        }),
        1 => Ok(WinnerFailMode::PickHighestPassed),
        2 => Ok(WinnerFailMode::PickHighestPassedDifferentProvider),
        3 => Ok(WinnerFailMode::StableFallback {
            step: reader.u16()?,
        }),
        tag => Err(Error::Snapshot(format!(
            "snapshot contains unknown winner-failure tag {tag}"
        ))),
    }
}

/// Decode a dialect tag.
fn dialect_from_tag(tag: u8) -> Result<Dialect> {
    Dialect::from_u8(tag)
        .ok_or_else(|| Error::Snapshot(format!("snapshot contains unknown dialect tag {tag}")))
}

/// Decode a capability tag.
fn capability_from_tag(tag: u8) -> Result<Capability> {
    Ok(match tag {
        0 => Capability::Chat,
        1 => Capability::Responses,
        2 => Capability::Embeddings,
        3 => Capability::AudioTranscription,
        4 => Capability::AudioSpeech,
        5 => Capability::ImagesGenerations,
        6 => Capability::Models,
        7 => Capability::Messages,
        8 => Capability::ToolCalling,
        9 => Capability::Reasoning,
        10 => Capability::Vision,
        11 => Capability::AudioTranslation,
        12 => Capability::ImagesEdits,
        13 => Capability::ImagesVariations,
        14 => Capability::Moderations,
        15 => Capability::Video,
        16 => Capability::Realtime,
        17 => Capability::VectorStores,
        18 => Capability::Containers,
        19 => Capability::FineTuning,
        20 => Capability::Evals,
        21 => Capability::Search,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown capability tag {tag}"
            )))
        }
    })
}

/// Decode a cooldown scope tag.
fn cooldown_scope_from_tag(tag: u8) -> Result<CooldownScope> {
    Ok(match tag {
        0 => CooldownScope::Account,
        1 => CooldownScope::Model,
        2 => CooldownScope::Provider,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown cooldown scope tag {tag}"
            )))
        }
    })
}

/// Decode a key strategy tag.
fn key_strategy_from_tag(tag: u8) -> Result<KeyStrategy> {
    Ok(match tag {
        0 => KeyStrategy::Priority,
        1 => KeyStrategy::RoundRobin,
        2 => KeyStrategy::Random,
        3 => KeyStrategy::LeastUsed,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown key strategy tag {tag}"
            )))
        }
    })
}

/// Decode a combo strategy tag.
fn combo_strategy_from_tag(tag: u8) -> Result<ComboStrategy> {
    Ok(match tag {
        0 => ComboStrategy::Priority,
        1 => ComboStrategy::RoundRobin,
        2 => ComboStrategy::ResetAwarePriority,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown combo strategy tag {tag}"
            )))
        }
    })
}

/// Decode a midstream policy tag.
fn midstream_from_tag(tag: u8) -> Result<MidstreamPolicy> {
    Ok(match tag {
        0 => MidstreamPolicy::Restart,
        1 => MidstreamPolicy::Terminate,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown midstream policy tag {tag}"
            )))
        }
    })
}

/// Decode a winner metric tag.
fn winner_metric_from_tag(tag: u8) -> Result<WinnerMetric> {
    Ok(match tag {
        0 => WinnerMetric::FirstResponseByte,
        1 => WinnerMetric::FirstCompleteToken,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown winner metric tag {tag}"
            )))
        }
    })
}

/// Tag an HTTP/2 preference for the wire.
fn http2_tag(preference: Http2Preference) -> u8 {
    match preference {
        Http2Preference::Auto => 0,
        Http2Preference::Required => 1,
        Http2Preference::Disabled => 2,
    }
}

/// Decode an HTTP/2 preference tag.
fn http2_from_tag(tag: u8) -> Result<Http2Preference> {
    Ok(match tag {
        0 => Http2Preference::Auto,
        1 => Http2Preference::Required,
        2 => Http2Preference::Disabled,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown http2 preference tag {tag}"
            )))
        }
    })
}

/// Tag a compression mode for the wire.
fn compression_mode_tag(mode: CompressionMode) -> u8 {
    match mode {
        CompressionMode::Rtk => 0,
        CompressionMode::SystemPrompt => 1,
    }
}

/// Decode a compression mode tag.
fn compression_mode_from_tag(tag: u8) -> Result<CompressionMode> {
    Ok(match tag {
        0 => CompressionMode::Rtk,
        1 => CompressionMode::SystemPrompt,
        _ => {
            return Err(Error::Snapshot(format!(
                "snapshot contains unknown compression mode tag {tag}"
            )))
        }
    })
}

/// Tag a winner-failure mode for the wire. The payload byte or half-word follows.
fn winner_fail_tag(mode: &WinnerFailMode) -> u8 {
    match mode {
        WinnerFailMode::RerunExcluding { .. } => 0,
        WinnerFailMode::PickHighestPassed => 1,
        WinnerFailMode::PickHighestPassedDifferentProvider => 2,
        WinnerFailMode::StableFallback { .. } => 3,
    }
}

/// Accumulates the body while assigning every distinct string a table index.
struct Encoder {
    strings: Vec<String>,
    ids: HashMap<String, u32>,
    body: Vec<u8>,
}

impl Encoder {
    /// A fresh encoder with an empty table and body.
    fn new() -> Self {
        Self {
            strings: Vec::new(),
            ids: HashMap::new(),
            body: Vec::new(),
        }
    }

    /// Intern a string, returning its table index.
    fn str_id(&mut self, value: &str) -> u32 {
        if let Some(&id) = self.ids.get(value) {
            return id;
        }
        let id = self.strings.len() as u32;
        self.strings.push(value.to_string());
        self.ids.insert(value.to_string(), id);
        id
    }

    /// Intern an optional string, using [`NO_STRING`] for `None`.
    fn opt_str_id(&mut self, value: Option<&str>) -> u32 {
        match value {
            Some(value) => self.str_id(value),
            None => NO_STRING,
        }
    }

    /// Intern an optional string and append its index to the body.
    fn put_opt_str(&mut self, value: Option<&str>) {
        let id = self.opt_str_id(value);
        self.put_u32(id);
    }

    /// Intern a string and append its index to the body.
    fn put_str(&mut self, value: &str) {
        let id = self.str_id(value);
        self.put_u32(id);
    }

    /// Append one byte.
    fn put_u8(&mut self, value: u8) {
        self.body.push(value);
    }

    /// Append a little-endian `u16`.
    fn put_u16(&mut self, value: u16) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a little-endian `u32`.
    fn put_u32(&mut self, value: u32) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a little-endian `u64`.
    fn put_u64(&mut self, value: u64) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a little-endian `i32`.
    fn put_i32(&mut self, value: i32) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a little-endian `i64`.
    fn put_i64(&mut self, value: i64) {
        self.body.extend_from_slice(&value.to_le_bytes());
    }

    /// Append a boolean as one byte.
    fn put_bool(&mut self, value: bool) {
        self.put_u8(value as u8);
    }
}

/// A bounds-checked cursor over the payload.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A cursor at the start of `buf`.
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// How many bytes are left unread.
    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Consume exactly `length` bytes, or report a precise truncation.
    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(length)
            .ok_or_else(|| Error::Snapshot("snapshot payload offset overflow".into()))?;
        let slice = self.buf.get(self.pos..end).ok_or_else(|| {
            Error::Snapshot(format!(
                "snapshot payload is truncated at byte {}: {length} bytes are needed but only {} \
                 remain",
                self.pos,
                self.remaining()
            ))
        })?;
        self.pos = end;
        Ok(slice)
    }

    /// Read one byte.
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read a little-endian `u16`.
    fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Read a little-endian `u32`.
    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a little-endian `u64`.
    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Read a little-endian `i32`.
    fn i32(&mut self) -> Result<i32> {
        let bytes = self.take(4)?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a little-endian `i64`.
    fn i64(&mut self) -> Result<i64> {
        let bytes = self.take(8)?;
        Ok(i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// Read a boolean byte.
    fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(Error::Snapshot(format!(
                "snapshot boolean is {other}, which is neither 0 nor 1"
            ))),
        }
    }

    /// Read a string-table index and return the string it names.
    fn string(&mut self, table: &[String]) -> Result<String> {
        let index = self.u32()?;
        table.get(index as usize).cloned().ok_or_else(|| {
            Error::Snapshot(format!(
                "snapshot string index {index} is out of range: the table has {} entries",
                table.len()
            ))
        })
    }

    /// Read an optional string-table index.
    fn opt_string(&mut self, table: &[String]) -> Result<Option<String>> {
        let index = self.u32()?;
        if index == NO_STRING {
            return Ok(None);
        }
        table.get(index as usize).cloned().map(Some).ok_or_else(|| {
            Error::Snapshot(format!(
                "snapshot string index {index} is out of range: the table has {} entries",
                table.len()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Account, Alias, Combo, ComboStep, ModelEntry, ModelSource, Provider};

    fn provider(id: &str) -> Provider {
        let mut plan = Provider::new(id, Dialect::OpenAi, format!("https://{id}.example/v1"));
        plan.label = Some(format!("{id} label"));
        plan.headers.insert("x-test".into(), "yes".into());
        plan.route_overrides
            .insert("chat".into(), "/chat/completions".into());
        plan
    }

    fn model(id: &str) -> ModelEntry {
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

    fn account(id: &str, label: &str, priority: i32) -> Account {
        let mut account = Account::new(label);
        account.id = AccountId::from_raw(id);
        account.priority = priority;
        account
    }

    fn combo(id: &str, name: &str, steps: &[(&str, &str)]) -> Combo {
        let first = ComboStep::new(steps[0].0, steps[0].1);
        let mut combo = Combo::new(name, first);
        combo.id = ComboId::from_raw(id);
        combo.steps = steps
            .iter()
            .map(|(provider, model)| ComboStep::new(*provider, *model))
            .collect();
        combo
    }

    /// A configuration exercising every field the encoder is expected to preserve.
    fn rich_config() -> Config {
        let mut config = Config::default();

        let mut alpha = provider("alpha");
        alpha.models = vec![model("m1"), model("m2"), {
            let mut hidden = model("hidden");
            hidden.hidden = true;
            hidden
        }];
        alpha.accounts = vec![
            account("acc-second", "second", 5),
            account("acc-first", "first", 1),
            {
                let mut off = account("acc-off", "off", 0);
                off.enabled = false;
                off
            },
        ];
        alpha.key_strategy = KeyStrategy::RoundRobin;
        alpha.connect_timeout_ms = 1_234;
        alpha.ttfb_timeout_ms = 5_678;
        alpha.idle_timeout_ms = 9_012;
        alpha.total_timeout_ms = 34_567;
        alpha.http2 = Http2Preference::Required;
        alpha.prewarm = false;

        let mut beta = provider("beta");
        beta.models = vec![model("m2"), model("m3")];
        beta.accounts = vec![account("acc-beta", "beta account", 0)];
        beta.auth = AuthKind::ApiKeyHeader {
            header: "x-beta-key".into(),
        };

        alpha.proxy = Some("edge".into());
        alpha.accounts[0].proxy = Some("mobile".into());
        config.providers = vec![alpha, beta];
        config.proxies = vec![
            ProxyConfig {
                id: "edge".into(),
                label: Some("Edge".into()),
                url: "http://proxy.example:8080".into(),
                username: Some("router".into()),
                secret_ref: Some("proxy/edge".into()),
                enabled: true,
                priority: 3,
                extra: Default::default(),
            },
            ProxyConfig {
                id: "mobile".into(),
                label: None,
                url: "socks5://mobile.example:1080".into(),
                username: None,
                secret_ref: None,
                enabled: true,
                priority: 1,
                extra: Default::default(),
            },
            ProxyConfig {
                id: "disabled".into(),
                label: None,
                url: "http://disabled.example:8080".into(),
                username: None,
                secret_ref: None,
                enabled: false,
                priority: 0,
                extra: Default::default(),
            },
        ];
        config.proxy_strategy = ProxyStrategy::RoundRobin;
        let mut client_key = ClientKey {
            id: ClientKeyId::from_raw("key-one"),
            label: "desktop".into(),
            key_hash: "a".repeat(64),
            prefix: Some("mr-a".into()),
            enabled: true,
            allow_models: vec!["alpha/*".into()],
            allow_routes: vec!["chat".into()],
            allow_combos: Vec::new(),
            limit_requests: 999,
            limits: KeyLimits {
                session_5h: Some(KeyLimit {
                    tokens: 5000,
                    requests: 10,
                }),
                weekly_1w: Some(KeyLimit {
                    tokens: 0,
                    requests: 20,
                }),
                monthly_30d: Some(KeyLimit {
                    tokens: 0,
                    requests: 0,
                }),
            },
            note: None,
        };
        config.client_keys.push(client_key.clone());
        client_key.enabled = false;
        client_key.id = ClientKeyId::from_raw("disabled-key");
        config.client_keys.push(client_key);

        let mut smart = combo(
            "combo-smart",
            "smart",
            &[("alpha", "m1"), ("beta", "m2"), ("alpha", "m2")],
        );
        // An authored disabled step at index 0, so hedge participant remapping is exercised.
        smart.steps.insert(0, {
            let mut disabled = ComboStep::new("alpha", "m1");
            disabled.enabled = false;
            disabled
        });
        smart.hedge.enabled = true;
        smart.hedge.count = 2;
        smart.hedge.participants = vec![1, 3];
        smart.hedge.window_ms = 321;
        smart.hedge.loser_grace_ms = 42;
        smart.hedge.winner_metric = WinnerMetric::FirstCompleteToken;
        smart.hedge.on_winner_fail = WinnerFailMode::RerunExcluding { max_rounds: 3 };
        smart.strategy = ComboStrategy::ResetAwarePriority;
        smart.midstream_policy = MidstreamPolicy::Terminate;
        smart.max_retries = 4;
        smart.retry_delay_ms = 123;
        smart.affinity = false;
        smart.compression.enabled = true;
        smart.compression.mode = CompressionMode::SystemPrompt;
        smart.compression.min_bytes = 99;

        config.combos = vec![smart];
        config.aliases = vec![
            Alias {
                from: "fast".into(),
                to: "smart".into(),
                generated: false,
            },
            Alias {
                from: "quick".into(),
                to: "fast".into(),
                generated: false,
            },
        ];
        config
    }

    #[test]
    fn provider_context_override_is_compiled_into_the_step() {
        let mut config = Config::default();
        let mut provider = provider("modal");
        provider.dialect = Dialect::ModalVllm;
        provider.max_context = Some(262_144);
        provider.models = vec![model("qwen")];
        provider.accounts = vec![account("a", "a", 0)];
        config.providers = vec![provider];
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let step = &snapshot.providers()[0].steps[0];
        assert_eq!(step.context_length, Some(262_144));
    }

    #[test]
    fn quota_windows_are_resolved_onto_accounts_and_round_trip() {
        // No provider id appears here: the compiler carries whatever windows the operator
        // declared, which is what makes the feature work for every provider rather than one.
        let mut config = Config::default();
        let mut provider = provider("generic");
        provider.quota = vec![crate::model::QuotaWindow::requests_per(3600, 100)];
        let mut first = account("a", "a", 0);
        let mut second = account("b", "b", 1);
        // An account's own windows replace the provider's; an account with none inherits them.
        second.quota = vec![crate::model::QuotaWindow::requests_per(60, 5)];
        // A window that constrains nothing, and one with no positive length, never reach the
        // data plane: enforcement cannot be derived from either.
        second.quota.push(crate::model::QuotaWindow {
            seconds: 60,
            ..Default::default()
        });
        second.quota.push(crate::model::QuotaWindow {
            seconds: 0,
            limit_requests: 9,
            ..Default::default()
        });
        provider.accounts = vec![first.clone(), second.clone()];
        provider.models = vec![model("m")];
        config.providers = vec![provider];
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let accounts = &snapshot.providers()[0].accounts;
        assert_eq!(accounts[0].quota.len(), 1);
        assert_eq!(accounts[0].quota[0].limit_requests, 100);
        assert_eq!(accounts[1].quota.len(), 1);
        assert_eq!(accounts[1].quota[0].seconds, 60);

        let bytes = snapshot.encode().unwrap();
        let decoded = RoutingSnapshot::decode(&bytes).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(
            decoded.providers()[0].accounts[1].quota[0].limit_requests,
            5,
            "the binary frame must carry the windows the relay enforces"
        );
        // An unrelated config shape must not gain windows by accident.
        first.quota.clear();
        assert!(accounts[0].quota.len() == 1);
    }

    #[test]
    fn model_context_is_preserved_when_no_provider_override_exists() {
        let mut config = Config::default();
        let mut provider = provider("modal");
        provider.dialect = Dialect::ModalVllm;
        let mut entry = model("qwen");
        entry.context_length = Some(196_608);
        provider.models = vec![entry];
        provider.accounts = vec![account("a", "a", 0)];
        config.providers = vec![provider];
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(
            snapshot.providers()[0].steps[0].context_length,
            Some(196_608)
        );
    }

    #[test]
    fn a_rich_snapshot_round_trips_through_encode_and_decode() {
        let mut config = rich_config();
        config.routing.defaults.image = Some("vision/image".into());
        config.routing.defaults.audio = Some("voice/audio".into());
        let original = RoutingSnapshot::compile(&config).unwrap();
        let bytes = original.encode().unwrap();
        let decoded = RoutingSnapshot::decode(&bytes).unwrap();
        assert_eq!(decoded, original);
        assert_eq!(decoded.digest(), config.routing_digest());
    }

    #[test]
    fn the_cooldown_scope_is_compiled_and_survives_the_encode_decode_round_trip() {
        let mut config = rich_config();
        config.providers[0].cooldown_scope = CooldownScope::Provider;
        config.providers[1].cooldown_scope = CooldownScope::Model;
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(
            snapshot.providers()[0].cooldown_scope,
            CooldownScope::Provider
        );
        assert_eq!(snapshot.providers()[1].cooldown_scope, CooldownScope::Model);

        let bytes = snapshot.encode().unwrap();
        let decoded = RoutingSnapshot::decode(&bytes).unwrap();
        assert_eq!(
            decoded.providers()[0].cooldown_scope,
            CooldownScope::Provider
        );
        assert_eq!(decoded.providers()[1].cooldown_scope, CooldownScope::Model);

        // The scope decides which keys a refusal lands under, so changing it is routing-relevant:
        // the digest has to move or the conductor would skip the recompile that applies it.
        let before = config.routing_digest();
        config.providers[0].cooldown_scope = CooldownScope::Account;
        assert_ne!(config.routing_digest(), before);
    }

    #[test]
    fn a_declared_credit_round_trips_through_the_frame_as_cents() {
        let mut config = rich_config();
        // `acc-first` is declared second but sorts first by priority, so the compiled order is not
        // the authored order — exactly what this assertion is here to pin.
        config.providers[0].accounts[1].credit_usd = Some(1234.56);
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(
            snapshot.providers()[0].accounts[0].credit_cents,
            Some(123_456)
        );
        let decoded = RoutingSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(
            decoded.providers()[0].accounts[0].credit_cents,
            Some(123_456)
        );
        // Absence is not zero: an unknown balance must not read as a spent account.
        assert_eq!(decoded.providers()[0].accounts[1].credit_cents, None);
    }

    #[test]
    fn every_capability_tag_round_trips() {
        // The encoder writes `*capability as u8`; `capability_from_tag` is the only decoder. A
        // capability appended without a tag arm would encode fine and then fail to load, which is
        // the worst time to find out, so the whole of `ALL` is walked here.
        for (tag, capability) in (0u8..).zip(Capability::ALL) {
            assert_eq!(capability as u8, tag, "{capability:?} moved off its tag");
            assert_eq!(
                capability_from_tag(tag).expect("known tag decodes"),
                capability,
                "tag {tag}"
            );
        }
        // The first unused tag must be refused rather than guessed at: mapping it to a default
        // would route with a capability the operator never configured.
        let first_unknown = Capability::ALL.len() as u8;
        assert!(capability_from_tag(first_unknown).is_err());
        assert!(capability_from_tag(u8::MAX).is_err());
    }

    #[test]
    fn the_snapshot_version_stays_ahead_of_every_capability_tag() {
        // The version check at the frame header is the only thing that stops an older reader from
        // decoding tags it does not know. Keeping the version above the whole tag range makes that
        // check complete: a reader whose version equals a tag it might meet must never accept the
        // frame, or it would pass the header and fail mid-decode with a byte offset for a diagnosis.
        let highest = Capability::ALL
            .iter()
            .map(|capability| *capability as u8)
            .max()
            .expect("Capability::ALL is not empty");
        assert!(
            SNAPSHOT_VERSION > u32::from(highest),
            "SNAPSHOT_VERSION {SNAPSHOT_VERSION} must exceed the highest capability tag {highest}"
        );
    }

    #[test]
    fn an_unknown_cooldown_scope_tag_is_a_decode_error() {
        // A tag this build does not know cannot be guessed at: silently mapping it to the default
        // would route with a scope the operator never chose.
        assert!(cooldown_scope_from_tag(3).is_err());
        assert_eq!(cooldown_scope_from_tag(0).unwrap(), CooldownScope::Account);
        assert_eq!(cooldown_scope_from_tag(1).unwrap(), CooldownScope::Model);
        assert_eq!(cooldown_scope_from_tag(2).unwrap(), CooldownScope::Provider);
    }

    #[test]
    fn compile_orders_accounts_by_priority_and_drops_disabled_ones() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let alpha = &snapshot.providers()[0];
        let labels: Vec<_> = alpha
            .accounts
            .iter()
            .map(|a| a.label.as_deref().unwrap())
            .collect();
        assert_eq!(labels, vec!["first", "second"]);
    }

    #[test]
    fn key_limits_and_proxy_assignments_are_compiled_and_round_trip() {
        let config = rich_config();
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(snapshot.keys().len(), 1);
        let key = &snapshot.keys()[0];
        assert_eq!(key.id.as_str(), "key-one");
        assert_eq!(key.limit_requests, 999);
        assert_eq!(key.limits.session_5h.unwrap().tokens, 5000);
        assert_eq!(key.limits.session_5h.unwrap().requests, 10);
        assert_eq!(key.limits.weekly_1w.unwrap().tokens, 0);
        assert_eq!(key.limits.weekly_1w.unwrap().requests, 20);
        assert_eq!(
            key.limits.monthly_30d,
            Some(KeyLimit {
                tokens: 0,
                requests: 0
            }),
            "an explicitly configured uncapped window is preserved"
        );

        assert_eq!(snapshot.proxy_strategy(), ProxyStrategy::RoundRobin);
        assert_eq!(
            snapshot.proxies().len(),
            2,
            "disabled proxy is not dialable"
        );
        assert_eq!(snapshot.proxy("disabled"), None);
        let alpha = &snapshot.providers()[0];
        assert_eq!(alpha.proxy.as_deref(), Some("edge"));
        assert_eq!(alpha.accounts[0].proxy, None);
        assert_eq!(alpha.accounts[1].proxy.as_deref(), Some("mobile"));
        assert_eq!(
            snapshot.proxy("edge").unwrap().secret_ref.as_deref(),
            Some("proxy/edge")
        );

        let decoded = RoutingSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.keys()[0].limits.session_5h.unwrap().tokens, 5000);
        assert_eq!(decoded.keys()[0].limits.weekly_1w.unwrap().requests, 20);
        assert_eq!(
            decoded.keys()[0].limits.monthly_30d,
            Some(KeyLimit {
                tokens: 0,
                requests: 0
            })
        );
        assert_eq!(
            decoded.providers()[0].accounts[1].proxy.as_deref(),
            Some("mobile")
        );
    }

    #[test]
    fn generated_aliases_do_not_enter_a_compiled_snapshot() {
        let mut config = Config::default();
        config.providers.push(provider("p"));
        config.aliases = vec![
            Alias {
                from: "manual".into(),
                to: "p/model".into(),
                generated: false,
            },
            Alias {
                from: "old-generated".into(),
                to: "p/model".into(),
                generated: true,
            },
        ];
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(snapshot.aliases().len(), 1);
        assert_eq!(snapshot.aliases()[0].from, "manual");
        assert!(matches!(
            snapshot.resolve("old-generated"),
            Resolution::NotFound { .. }
        ));
    }

    /// A hidden model is reported as hidden, not as missing.
    ///
    /// The regression: a model the operator could see in the provider's list was answered with
    /// "does not offer a model matching", which sends them to sync models that are already synced.
    /// The two states need different fixes — an id versus a checkbox — so they get different text.
    #[test]
    fn a_hidden_model_is_reported_as_hidden_rather_than_missing() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        assert_eq!(
            snapshot.providers()[0].hidden_models,
            vec!["hidden".to_owned()]
        );

        for requested in ["alpha/hidden", "alpha/hidden-high"] {
            match snapshot.resolve(requested) {
                Resolution::NotFound { reason } => {
                    assert!(reason.contains("hidden"), "{requested}: {reason}");
                    assert!(
                        !reason.contains("does not offer"),
                        "{requested} exists but is hidden: {reason}"
                    );
                }
                other => panic!("{requested} must not resolve while hidden: {other:?}"),
            }
        }

        // A name that is genuinely absent keeps the ordinary message.
        match snapshot.resolve("alpha/nope") {
            Resolution::NotFound { reason } => {
                assert!(reason.contains("does not offer"), "{reason}");
                assert!(!reason.contains("hidden"), "{reason}");
            }
            other => panic!("a missing model must not resolve: {other:?}"),
        }
    }

    /// The hidden list survives the frame, because the relay reads it from the encoded snapshot.
    #[test]
    fn hidden_models_round_trip_through_the_frame() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let bytes = snapshot.encode().unwrap();
        let decoded = RoutingSnapshot::decode(&bytes).unwrap();
        assert_eq!(
            decoded.providers()[0].hidden_models,
            snapshot.providers()[0].hidden_models
        );
        assert!(decoded.providers()[0]
            .steps
            .iter()
            .all(|step| step.model != "hidden"));
    }

    #[test]
    fn compile_drops_hidden_models() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let models: Vec<_> = snapshot.providers()[0]
            .steps
            .iter()
            .map(|s| s.model.as_str())
            .collect();
        assert_eq!(models, vec!["m1", "m2"]);
    }

    #[test]
    fn compile_remaps_hedge_participants_onto_surviving_steps() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let smart = snapshot.lookup("smart").unwrap();
        assert_eq!(smart.steps.len(), 3);
        assert!(smart.hedge.enabled);
        assert_eq!(smart.hedge.participants, vec![0, 2]);
        assert_eq!(smart.hedge.winner_metric, WinnerMetric::FirstCompleteToken);
        assert_eq!(smart.hedge.window_ms, 321);
        assert_eq!(smart.hedge.loser_grace_ms, 42);
    }

    #[test]
    fn compile_flattens_alias_chains() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let quick = snapshot
            .aliases()
            .iter()
            .find(|a| a.from == "quick")
            .unwrap();
        assert_eq!(quick.to, "smart");
        assert!(!quick.generated);
        assert!(snapshot.lookup("quick").is_some());
    }

    #[test]
    fn compile_resolves_the_conventional_secret_reference() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let account = &snapshot.providers()[0].accounts[0];
        assert_eq!(
            account.secret_ref.as_deref(),
            Some("provider/alpha/account/acc-first")
        );
    }

    #[test]
    fn the_digest_is_stable_across_encode_and_decode() {
        let config = rich_config();
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let decoded = RoutingSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(decoded.digest(), snapshot.digest());
        assert_eq!(decoded.digest(), config.routing_digest());
        assert_eq!(decoded.built_at(), snapshot.built_at());
    }

    #[test]
    fn encoding_is_deterministic() {
        let config = rich_config();
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(snapshot.encode().unwrap(), snapshot.encode().unwrap());
    }

    #[test]
    fn malformed_inputs_are_rejected_without_panicking() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        let frame = snapshot.encode().unwrap();

        for cut in [0usize, 1, 4, 7, 8, 12, 20, 31, 32, frame.len() - 1] {
            let result = RoutingSnapshot::decode(&frame[..cut]);
            assert!(result.is_err(), "a {cut}-byte prefix must not decode");
        }

        let mut wrong_magic = frame.clone();
        wrong_magic[0] ^= 0xff;
        let error = RoutingSnapshot::decode(&wrong_magic)
            .unwrap_err()
            .to_string();
        assert!(error.contains("magic"), "{error}");

        let mut wrong_version = frame.clone();
        wrong_version[8..12].copy_from_slice(&(SNAPSHOT_VERSION + 1).to_le_bytes());
        let error = RoutingSnapshot::decode(&wrong_version)
            .unwrap_err()
            .to_string();
        assert!(error.contains("version"), "{error}");

        let mut corrupt = frame.clone();
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x01;
        let error = RoutingSnapshot::decode(&corrupt).unwrap_err().to_string();
        assert!(error.contains("checksum"), "{error}");

        let mut trailing = frame.clone();
        trailing.push(0);
        let error = RoutingSnapshot::decode(&trailing).unwrap_err().to_string();
        assert!(error.contains("trailing"), "{error}");
    }

    #[test]
    fn an_out_of_range_string_index_is_rejected() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // empty string table
        payload.extend_from_slice(&0i64.to_le_bytes()); // built_at
        payload.extend_from_slice(&1u32.to_le_bytes()); // one provider
        payload.extend_from_slice(&7u32.to_le_bytes()); // its id names string 7
        let frame = assemble(0, &payload);
        let error = RoutingSnapshot::decode(&frame).unwrap_err().to_string();
        assert!(error.contains("out of range"), "{error}");
    }

    #[test]
    fn an_out_of_range_lookup_target_is_rejected() {
        let mut snapshot = RoutingSnapshot::default();
        snapshot.index_entries.push(IndexEntry {
            key: "x".into(),
            rank: RANK_COMBO_NAME,
            target: IndexTarget::Combo {
                combo: 5,
                level: None,
            },
            next: None,
        });
        let error = snapshot.link_entries().unwrap_err().to_string();
        assert!(error.contains("only 0 combos"), "{error}");

        let mut snapshot = RoutingSnapshot::default();
        snapshot.index_entries.push(IndexEntry {
            key: "x".into(),
            rank: RANK_QUALIFIED,
            target: IndexTarget::Step {
                provider: 0,
                step: 0,
                level: 0,
                requested: 0,
            },
            next: None,
        });
        let error = snapshot.link_entries().unwrap_err().to_string();
        assert!(error.contains("provider 0"), "{error}");
    }

    #[test]
    fn an_out_of_range_rank_is_rejected() {
        let mut snapshot = RoutingSnapshot::default();
        snapshot.index_entries.push(IndexEntry {
            key: "x".into(),
            rank: RANK_MAX + 1,
            target: IndexTarget::Combo {
                combo: 0,
                level: None,
            },
            next: None,
        });
        // Give it a real combo so target validation passes first.
        snapshot.combos.push(ComboPlan {
            id: ComboId::from_raw("c"),
            name: "c".into(),
            strategy: ComboStrategy::Priority,
            steps: Vec::new(),
            hedge: HedgePlan {
                enabled: false,
                participants: Vec::new(),
                window_ms: 0,
                winner_metric: WinnerMetric::FirstResponseByte,
                on_winner_fail: WinnerFailMode::PickHighestPassed,
                cancel_losers: true,
                loser_grace_ms: 0,
            },
            sticky_success_window_secs: 1800,
            midstream_policy: MidstreamPolicy::Restart,
            max_retries: 1,
            retry_delay_ms: 0,
            affinity: true,
            compression: CompressionSpec::default(),
            signature: 0,
        });
        let error = snapshot.link_entries().unwrap_err().to_string();
        assert!(error.contains("rank"), "{error}");
    }

    #[test]
    fn verify_header_reports_version_and_digest_without_decoding() {
        let config = rich_config();
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let (version, digest) = verify_header(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(version, SNAPSHOT_VERSION);
        assert_eq!(digest, config.routing_digest());
    }

    #[test]
    fn lookup_precedence_is_alias_then_id_then_name_then_qualified_and_bare_ids_do_not_resolve() {
        let mut config = Config::default();

        let mut p1 = provider("p1");
        p1.models = vec![model("m1"), model("bare1"), model("q1"), model("onlybare")];
        p1.accounts = vec![account("a1", "a1", 0)];

        let mut p2 = provider("p2");
        p2.models = vec![model("qual1"), model("p2/qual1")];
        p2.accounts = vec![account("a2", "a2", 0)];

        config.providers = vec![p1, p2];
        config.combos = vec![
            combo("alias-target", "target-name", &[("p1", "m1")]),
            combo("aa-holder", "aa", &[("p1", "m1")]),
            combo("bb", "bb-name", &[("p1", "m1")]),
            combo("cc", "x-name", &[("p1", "m1")]),
            combo("y-id", "cc", &[("p1", "m1")]),
            combo("z-id", "p1/q1", &[("p1", "m1")]),
            combo("w-id", "bare1", &[("p1", "m1")]),
        ];
        config.aliases = vec![
            Alias {
                from: "aa".into(),
                to: "alias-target".into(),
                generated: false,
            },
            Alias {
                from: "bb".into(),
                to: "alias-target".into(),
                generated: false,
            },
        ];
        let snapshot = RoutingSnapshot::compile(&config).unwrap();

        // Alias beats both a combo name ("aa") and a combo id ("bb").
        for requested in ["aa", "bb"] {
            match snapshot.resolve(requested) {
                Resolution::Combo { plan: combo, .. } => {
                    assert_eq!(combo.id.as_str(), "alias-target")
                }
                other => panic!("{requested} resolved to {other:?}"),
            }
        }

        // A combo id beats another combo's name.
        match snapshot.resolve("cc") {
            Resolution::Combo { plan: combo, .. } => assert_eq!(combo.name, "x-name"),
            other => panic!("cc resolved to {other:?}"),
        }

        // A combo name beats a qualified provider/model.
        match snapshot.resolve("p1/q1") {
            Resolution::Combo { plan: combo, .. } => assert_eq!(combo.id.as_str(), "z-id"),
            other => panic!("p1/q1 resolved to {other:?}"),
        }

        // A combo name beats a bare model id.
        match snapshot.resolve("bare1") {
            Resolution::Combo { plan: combo, .. } => assert_eq!(combo.id.as_str(), "w-id"),
            other => panic!("bare1 resolved to {other:?}"),
        }

        // A qualified provider/model beats a bare model id with the same characters.
        match snapshot.resolve("p2/qual1") {
            Resolution::Direct { step, .. } => {
                assert_eq!(step.model, "qual1");
                assert_eq!(step.provider_id.as_str(), "p2");
            }
            other => panic!("p2/qual1 resolved to {other:?}"),
        }

        // A bare model id no longer resolves at all: Feature 3 removed the fallback, because
        // "the first provider in configured order that offers this id" was a routing decision
        // nobody wrote down. The miss has to explain the naming rule.
        let reason = miss(snapshot.resolve("onlybare"));
        assert!(
            reason.contains("provider/model") || reason.contains("provider prefix"),
            "{reason}"
        );
    }

    #[test]
    fn lookup_is_ascii_case_insensitive() {
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        assert!(snapshot.lookup("SMART").is_some());
        assert!(snapshot.lookup("Smart").is_some());
        assert!(snapshot.lookup("smart").is_some());
        match snapshot.resolve("ALPHA/M1") {
            Resolution::Direct { step, .. } => assert_eq!(step.model, "m1"),
            other => panic!("ALPHA/M1 resolved to {other:?}"),
        }
    }

    #[test]
    fn a_hash_collision_is_resolved_by_full_string_comparison() {
        // Two different keys sharing one bucket cannot be produced by brute-forcing a 64-bit hash,
        // so the situation is constructed directly: an entry with a *different* key is spliced in
        // at the head of "smart"'s bucket, with a lower precedence rank and a different target than
        // the genuine entry. If the lookup trusted the hash alone, this impostor would win; because
        // it compares the full key, it is skipped.
        let mut config = Config::default();
        let mut p1 = provider("p1");
        p1.models = vec![model("m1")];
        p1.accounts = vec![account("a", "a", 0)];
        config.providers = vec![p1];
        config.combos = vec![
            combo("smart-id", "smart", &[("p1", "m1")]),
            combo("other-id", "other", &[("p1", "m1")]),
        ];

        let mut snapshot = RoutingSnapshot::compile(&config).unwrap();
        let other = snapshot
            .combos
            .iter()
            .position(|c| c.id.as_str() == "other-id")
            .unwrap() as u32;
        let hash = fnv1a64_lower_ascii(b"smart");
        let head = *snapshot.index.get(&hash).expect("smart is indexed");
        let injected = snapshot.index_entries.len() as u32;
        snapshot.index_entries.push(IndexEntry {
            key: "impostor".into(),
            // Alias rank: without the string comparison this would outrank the real entry.
            rank: RANK_ALIAS,
            target: IndexTarget::Combo {
                combo: other,
                level: None,
            },
            next: Some(head),
        });
        snapshot.index.insert(hash, injected);
        assert_eq!(*snapshot.index.get(&hash).unwrap(), injected);

        match snapshot.resolve("smart") {
            Resolution::Combo { plan: combo, .. } => assert_eq!(combo.id.as_str(), "smart-id"),
            other => panic!("smart resolved to {other:?}"),
        }
    }

    #[test]
    fn disabled_and_hidden_things_are_excluded() {
        let mut config = Config::default();

        let mut enabled = provider("enabled");
        enabled.models = vec![model("visible"), {
            let mut hidden = model("hidden");
            hidden.hidden = true;
            hidden
        }];
        enabled.accounts = vec![account("on", "on", 0), {
            let mut off = account("off", "off", 1);
            off.enabled = false;
            off
        }];

        let mut disabled = provider("disabled");
        disabled.enabled = false;

        config.providers = vec![enabled, disabled];

        let mut off_combo = combo("off-combo", "off-combo", &[("enabled", "visible")]);
        off_combo.enabled = false;

        config.combos = vec![
            combo("on-combo", "on-combo", &[("enabled", "visible")]),
            off_combo,
            combo("gone-combo", "gone-combo", &[("disabled", "visible")]),
        ];

        let snapshot = RoutingSnapshot::compile(&config).unwrap();

        assert_eq!(snapshot.providers().len(), 1);
        assert_eq!(snapshot.providers()[0].id.as_str(), "enabled");
        assert_eq!(snapshot.providers()[0].accounts.len(), 1);
        assert_eq!(snapshot.providers()[0].accounts[0].id.as_str(), "on");
        assert_eq!(snapshot.providers()[0].steps.len(), 1);
        assert_eq!(snapshot.providers()[0].steps[0].model, "visible");

        let names: Vec<_> = snapshot.combos().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["on-combo"]);
    }

    #[test]
    fn a_combo_whose_steps_all_name_unknown_providers_fails_to_compile() {
        let config = Config {
            combos: vec![combo("broken-id", "broken", &[("ghost", "m1")])],
            ..Config::default()
        };
        let error = RoutingSnapshot::compile(&config).unwrap_err().to_string();
        assert!(error.contains("broken"), "{error}");
        assert!(error.contains("ghost"), "{error}");
    }

    #[test]
    fn unknown_steps_are_dropped_when_a_combo_still_has_a_routable_step() {
        let mut config = Config::default();
        let mut p1 = provider("p1");
        p1.models = vec![model("m1")];
        p1.accounts = vec![account("a", "a", 0)];
        config.providers = vec![p1];
        config.combos = vec![combo("mixed", "mixed", &[("ghost", "m1"), ("p1", "m1")])];

        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(snapshot.combos().len(), 1);
        assert_eq!(snapshot.combos()[0].steps.len(), 1);
        assert_eq!(snapshot.combos()[0].steps[0].provider_id.as_str(), "p1");
    }

    #[test]
    fn a_combo_pointing_only_at_disabled_providers_is_omitted_rather_than_failing() {
        let mut config = Config::default();
        let mut p1 = provider("p1");
        p1.enabled = false;
        config.providers = vec![p1];
        config.combos = vec![combo("c", "c", &[("p1", "m1")])];

        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert!(snapshot.combos().is_empty());
        assert!(snapshot.providers().is_empty());
    }

    #[test]
    fn an_alias_cycle_is_rejected_at_compile_time() {
        let config = Config {
            aliases: vec![
                Alias {
                    from: "a".into(),
                    to: "b".into(),
                    generated: false,
                },
                Alias {
                    from: "b".into(),
                    to: "a".into(),
                    generated: false,
                },
            ],
            ..Config::default()
        };
        let error = RoutingSnapshot::compile(&config).unwrap_err().to_string();
        assert!(error.contains("cycle"), "{error}");
    }

    #[test]
    fn an_empty_config_compiles_encodes_decodes_and_resolves_nothing() {
        let snapshot = RoutingSnapshot::compile(&Config::default()).unwrap();
        assert!(snapshot.providers().is_empty());
        assert!(snapshot.combos().is_empty());
        assert!(snapshot.aliases().is_empty());

        let decoded = RoutingSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(decoded, snapshot);
        match decoded.resolve("anything") {
            Resolution::NotFound { reason } => assert!(!reason.is_empty(), "a miss must say why"),
            other => panic!("an empty snapshot resolved {other:?}"),
        }
        assert!(decoded.lookup("anything").is_none());
    }

    #[test]
    fn install_is_visible_to_the_next_current() {
        let handle = SnapshotHandle::new(RoutingSnapshot::compile(&Config::default()).unwrap());
        let before = handle.current().digest();

        let config = Config {
            providers: vec![provider("alpha")],
            ..Config::default()
        };
        let next = RoutingSnapshot::compile(&config).unwrap();
        handle.install(next);

        assert_ne!(handle.current().digest(), before);
        assert_eq!(handle.current().digest(), config.routing_digest());
    }

    #[test]
    fn reload_from_installs_the_file_and_returns_its_digest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routing.snapshot");
        let handle = SnapshotHandle::new(RoutingSnapshot::compile(&Config::default()).unwrap());

        let config = Config {
            providers: vec![provider("alpha")],
            ..Config::default()
        };
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        snapshot.write_to(&path).unwrap();

        let digest = handle.reload_from(&path).unwrap();
        assert_eq!(digest, config.routing_digest());
        assert_eq!(handle.current().digest(), digest);
        assert_eq!(handle.current().providers().len(), 1);
    }

    #[test]
    fn a_failed_reload_leaves_the_previous_snapshot_installed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routing.snapshot");
        std::fs::write(&path, b"not a snapshot at all").unwrap();

        let handle = SnapshotHandle::new(RoutingSnapshot::compile(&Config::default()).unwrap());
        let before = handle.current().digest();
        assert!(handle.reload_from(&path).is_err());
        assert_eq!(handle.current().digest(), before);
    }

    #[test]
    fn write_to_is_atomic_and_leaves_no_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routing.snapshot");
        let snapshot = RoutingSnapshot::compile(&rich_config()).unwrap();
        snapshot.write_to(&path).unwrap();

        let names: Vec<String> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["routing.snapshot".to_string()]);

        let loaded = RoutingSnapshot::load_from(&path).unwrap();
        assert_eq!(loaded, snapshot);
    }

    #[test]
    fn load_from_a_missing_file_is_an_error_rather_than_a_default() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("absent.snapshot");
        assert!(RoutingSnapshot::load_from(&path).is_err());
    }

    // -----------------------------------------------------------------------------------------
    // Feature 3: reasoning levels in model names
    // -----------------------------------------------------------------------------------------

    /// A provider whose models carry the metadata the level rules read.
    ///
    /// The provider id is deliberately a family the bundled catalogue does not carry, so these
    /// tests exercise what the *operator* declared rather than whatever
    /// `catalog/model_meta.json` happens to say about a real model.
    fn level_provider(id: &str, models: Vec<ModelEntry>) -> Provider {
        let mut p = provider(id);
        p.models = models;
        p.accounts = vec![account("a", "a", 0)];
        p
    }

    fn with_levels(id: &str, levels: &[ReasoningLevel]) -> ModelEntry {
        let mut m = model(id);
        m.reasoning_levels = levels.to_vec();
        m
    }

    /// Compile a configuration whose single provider `p1` carries one model with the metadata the
    /// level rules read.
    ///
    /// A builder rather than an inline `Config::default()` mutation in every test: `clippy` rejects
    /// field assignment after `Default::default()`, and one construction means the tests cannot
    /// drift from each other in which fields they bother to set.
    fn compile_one(provider_models: Vec<ModelEntry>, combos: Vec<Combo>) -> RoutingSnapshot {
        let mut config = Config {
            providers: vec![level_provider("p1", provider_models)],
            ..Config::default()
        };
        config.combos = combos;
        RoutingSnapshot::compile(&config).unwrap()
    }

    /// As [`compile_one`], but on a dialect that can express a different slice of the ladder.
    ///
    /// A test about the top tiers has to use a budget dialect: the named-effort dialects do not
    /// offer `max`, and the compiler would correctly cap the declaration before the test could
    /// observe it.
    fn compile_one_on(
        dialect: Dialect,
        provider_models: Vec<ModelEntry>,
        combos: Vec<Combo>,
    ) -> RoutingSnapshot {
        let mut provider = level_provider("p1", provider_models);
        provider.dialect = dialect;
        let mut config = Config {
            providers: vec![provider],
            ..Config::default()
        };
        config.combos = combos;
        RoutingSnapshot::compile(&config).unwrap()
    }

    /// The `(model, level, substituted)` triple of a direct hit.
    fn direct(resolution: Resolution<'_>) -> (String, u8, bool) {
        match resolution {
            Resolution::Direct {
                step,
                level,
                substituted,
            } => (step.model.clone(), level, substituted),
            other => panic!("expected a direct hit, got {other:?}"),
        }
    }

    /// The reason a miss carried, so a test can assert the message is actionable.
    fn miss(resolution: Resolution<'_>) -> String {
        match resolution {
            Resolution::NotFound { reason } => reason.into_owned(),
            other => panic!("expected a miss, got {other:?}"),
        }
    }

    #[test]
    fn an_exact_model_id_ending_in_a_level_word_wins_over_a_stripped_suffix() {
        // The rank-2-beats-rank-2.5 rule: `gemini-3.8-flash-low` is a model, and its trailing
        // `low` names the tier it runs at rather than selecting a shorter `gemini-3.8-flash`.
        let snapshot = compile_one(
            vec![
                with_levels(
                    "gemini-3.8-flash-low",
                    &[
                        ReasoningLevel::None,
                        ReasoningLevel::Low,
                        ReasoningLevel::High,
                    ],
                ),
                with_levels(
                    "gemini-3.8-flash",
                    &[ReasoningLevel::Low, ReasoningLevel::High],
                ),
            ],
            Vec::new(),
        );

        let (model, level, substituted) = direct(snapshot.resolve("p1/gemini-3.8-flash-low"));
        assert_eq!(model, "gemini-3.8-flash-low");
        assert_eq!(level, ReasoningLevel::Low.rank());
        assert!(
            !substituted,
            "the model's own name asked for the tier it runs at, so nothing was substituted"
        );
    }

    #[test]
    fn a_slashed_model_id_resolves_with_a_level_suffix() {
        // `max` is expressed by a budget dialect; on a named-effort dialect the compiler would cap
        // it to `xhigh`, which is the behaviour the next test pins instead.
        let snapshot = compile_one_on(
            Dialect::Anthropic,
            vec![with_levels(
                "deepseek/deepseek-v4.1-flash",
                &[
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                    ReasoningLevel::Max,
                ],
            )],
            Vec::new(),
        );

        let (model, level, substituted) =
            direct(snapshot.resolve("p1/deepseek/deepseek-v4.1-flash-max"));
        assert_eq!(model, "deepseek/deepseek-v4.1-flash");
        assert_eq!(level, ReasoningLevel::Max.rank());
        assert!(!substituted);
    }

    #[test]
    fn the_configured_default_level_is_high_when_the_model_does_not_override_it() {
        let snapshot = compile_one(
            vec![with_levels(
                "m1",
                &[
                    ReasoningLevel::Low,
                    ReasoningLevel::High,
                    ReasoningLevel::Max,
                ],
            )],
            Vec::new(),
        );

        let (_, level, substituted) = direct(snapshot.resolve("p1/m1"));
        assert_eq!(level, ReasoningLevel::High.rank());
        assert!(!substituted);
    }

    #[test]
    fn a_model_can_override_the_default_level() {
        let mut declared = with_levels(
            "m1",
            &[
                ReasoningLevel::Low,
                ReasoningLevel::High,
                ReasoningLevel::Max,
            ],
        );
        declared.default_reasoning_level = Some(ReasoningLevel::Low);
        let snapshot = compile_one(vec![declared], Vec::new());

        let (_, level, _) = direct(snapshot.resolve("p1/m1"));
        assert_eq!(level, ReasoningLevel::Low.rank());
    }

    #[test]
    fn a_bare_model_name_is_an_error_that_names_the_problem() {
        // Feature 3 removed the bare-id fallback deliberately. The message has to say so, because
        // "no model matches gpt-5" reads as a sync problem when it is really a naming rule.
        let snapshot = compile_one(vec![with_levels("gpt-5", &[])], Vec::new());

        let reason = miss(snapshot.resolve("gpt-5"));
        assert!(
            reason.contains("provider/model") || reason.contains("provider prefix"),
            "the miss must explain the naming rule: {reason}"
        );
    }

    #[test]
    fn an_unknown_provider_is_an_error_that_lists_the_configured_ones() {
        let snapshot = compile_one(vec![with_levels("m1", &[])], Vec::new());

        let reason = miss(snapshot.resolve("nosuchprovider/model"));
        assert!(reason.contains("nosuchprovider"), "{reason}");
        assert!(
            reason.contains("p1"),
            "the message must list the providers that do exist: {reason}"
        );
    }

    #[test]
    fn a_known_provider_with_an_unknown_model_says_which_half_was_wrong() {
        let snapshot = compile_one(vec![with_levels("m1", &[])], Vec::new());

        let reason = miss(snapshot.resolve("p1/nope"));
        assert!(reason.contains("p1"), "{reason}");
        assert!(reason.contains("nope"), "{reason}");
        assert!(
            !reason.contains("unknown provider"),
            "the provider is known, so the message must not blame it: {reason}"
        );
    }

    #[test]
    fn a_combo_named_with_a_slash_resolves_and_carries_no_level() {
        let snapshot = compile_one(
            vec![with_levels("m1", &[])],
            vec![combo("c1", "deepseek/deepseek-v4.1-flash", &[("p1", "m1")])],
        );

        match snapshot.resolve("deepseek/deepseek-v4.1-flash") {
            Resolution::Combo { plan, level } => {
                assert_eq!(plan.id.as_str(), "c1");
                assert_eq!(
                    level, None,
                    "the name carried no tier, so no level is forced onto the steps"
                );
            }
            other => panic!("expected the combo, got {other:?}"),
        }
    }

    #[test]
    fn a_combo_name_with_a_level_suffix_forces_that_tier() {
        let snapshot = compile_one(
            vec![with_levels("m1", &[])],
            vec![combo("c1", "coding", &[("p1", "m1")])],
        );

        match snapshot.resolve("coding-low") {
            Resolution::Combo { plan, level } => {
                assert_eq!(plan.id.as_str(), "c1");
                assert_eq!(level, Some(ReasoningLevel::Low.rank()));
            }
            other => panic!("expected the combo at a tier, got {other:?}"),
        }
    }

    #[test]
    fn a_combo_step_pins_its_own_reasoning_level() {
        // Two steps, two tiers: the combo is authored to spend little on the first attempt and more
        // on the retry, without either model's own default being edited.
        let mut first = combo("c1", "coding", &[("p1", "cheap"), ("p1", "strong")]);
        first.steps[0].level = Some(ReasoningLevel::Low);
        first.steps[1].level = Some(ReasoningLevel::High);
        let snapshot = compile_one(
            vec![
                with_levels(
                    "cheap",
                    &[
                        ReasoningLevel::Low,
                        ReasoningLevel::Medium,
                        ReasoningLevel::High,
                    ],
                ),
                with_levels(
                    "strong",
                    &[
                        ReasoningLevel::Low,
                        ReasoningLevel::Medium,
                        ReasoningLevel::High,
                    ],
                ),
            ],
            vec![first],
        );

        let plan = &snapshot.combos()[0];
        assert_eq!(plan.steps[0].level, ReasoningLevel::Low.rank());
        assert_eq!(plan.steps[1].level, ReasoningLevel::High.rank());
        assert_eq!(
            plan.steps[0].reasoning_fragment(),
            Some(r#"{"reasoning_effort":"low"}"#)
        );
        assert_eq!(
            plan.steps[1].reasoning_fragment(),
            Some(r#"{"reasoning_effort":"high"}"#)
        );
    }

    #[test]
    fn a_combo_step_routing_to_another_combo_is_spliced_in_place() {
        let mut inner = combo("c-inner", "inner", &[("p1", "m1"), ("p1", "m2")]);
        // A disabled nested step is not routable, so it contributes nothing.
        inner.steps[1].enabled = false;
        inner.strategy = ComboStrategy::RoundRobin;

        let mut outer = combo("c-outer", "outer", &[("p1", "m0")]);
        outer.strategy = ComboStrategy::Priority;
        outer.steps = vec![ComboStep::new("p1", "m0"), ComboStep::from_combo("inner")];

        let snapshot = compile_one(
            vec![model("m0"), model("m1"), model("m2")],
            vec![inner, outer],
        );
        let plan = snapshot
            .combos()
            .iter()
            .find(|c| c.id.as_str() == "c-outer")
            .expect("the outer combo compiles");
        let models: Vec<&str> = plan.steps.iter().map(|s| s.model.as_str()).collect();
        assert_eq!(models, ["m0", "m1"]);
        assert_eq!(
            plan.strategy,
            ComboStrategy::Priority,
            "the caller keeps its own strategy; only the target's steps are spliced in"
        );
    }

    #[test]
    fn a_hedge_participant_that_names_a_combo_step_follows_its_first_expanded_step() {
        // Authored step 0 expands to two steps, so the direct step authored at index 1 lands at
        // compiled index 2 and the participant list has to say so.
        let inner = combo("c-inner", "inner", &[("p1", "m1"), ("p1", "m2")]);
        let mut outer = combo("c-outer", "outer", &[("p1", "m0")]);
        outer.steps = vec![ComboStep::from_combo("inner"), ComboStep::new("p1", "m0")];
        outer.hedge.enabled = true;
        outer.hedge.participants = vec![1];

        let snapshot = compile_one(
            vec![model("m0"), model("m1"), model("m2")],
            vec![inner, outer],
        );
        let plan = snapshot
            .combos()
            .iter()
            .find(|c| c.id.as_str() == "c-outer")
            .expect("the outer combo compiles");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.hedge.participants, vec![2]);
    }

    #[test]
    fn a_routing_loop_is_refused_at_compile_time_with_the_chain() {
        // `Config::validate` reports this too, but the compiler must fail closed on its own: it is
        // reachable with a config that was never validated.
        let mut a = combo("c-a", "a", &[("p1", "m1")]);
        a.steps = vec![ComboStep::from_combo("b")];
        let mut b = combo("c-b", "b", &[("p1", "m1")]);
        b.steps = vec![ComboStep::from_combo("a")];

        let mut config = Config {
            providers: vec![level_provider("p1", vec![model("m1")])],
            ..Config::default()
        };
        config.combos = vec![a, b];

        let err = RoutingSnapshot::compile(&config).expect_err("a loop must not compile");
        let message = err.to_string();
        assert!(
            message.contains("combo routing loop: a -> b -> a"),
            "got {message}"
        );
    }

    #[test]
    fn a_combo_routing_to_an_unknown_combo_fails_to_compile() {
        let mut outer = combo("c-outer", "outer", &[("p1", "m1")]);
        outer.steps = vec![ComboStep::from_combo("missing")];

        let mut config = Config {
            providers: vec![level_provider("p1", vec![model("m1")])],
            ..Config::default()
        };
        config.combos = vec![outer];

        let err =
            RoutingSnapshot::compile(&config).expect_err("a dangling target must not compile");
        assert!(
            err.to_string().contains("unknown combo \"missing\""),
            "got {err}"
        );
    }

    #[test]
    fn a_combo_step_asking_for_more_than_the_model_offers_runs_at_its_maximum() {
        // The documented substitution rule, now reachable from the configuration as well as from a
        // `-max` suffix: the request must succeed and reason at the model's ceiling. The provider
        // is a budget dialect so `max` is a level the dialect can actually send; the substitution
        // here is about the *model* topping out at `xhigh`, not about an unsendable level.
        let mut pinned = combo("c1", "coding", &[("p1", "m1")]);
        pinned.steps[0].level = Some(ReasoningLevel::Max);
        let snapshot = compile_one_on(
            Dialect::Anthropic,
            vec![with_levels(
                "m1",
                &[
                    ReasoningLevel::Low,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ],
            )],
            vec![pinned],
        );

        let step = &snapshot.combos()[0].steps[0];
        assert_eq!(step.level, ReasoningLevel::XHigh.rank());
        assert_eq!(
            step.reasoning_fragment(),
            Some(r#"{"thinking":{"type":"enabled","budget_tokens":131072}}"#),
            "the fragment must be the model's maximum, never the level that was asked for"
        );
    }

    #[test]
    fn a_combo_step_without_a_level_keeps_the_routing_default() {
        let snapshot = compile_one(
            vec![with_levels(
                "m1",
                &[
                    ReasoningLevel::Low,
                    ReasoningLevel::Medium,
                    ReasoningLevel::High,
                ],
            )],
            vec![combo("c1", "coding", &[("p1", "m1")])],
        );

        let step = &snapshot.combos()[0].steps[0];
        assert_eq!(
            step.level,
            ReasoningLevel::from_u8(snapshot.providers()[0].steps[0].default_level)
                .unwrap()
                .rank()
        );
        assert!(step.reasoning_fragment().is_some());
    }

    #[test]
    fn an_out_of_range_level_reroutes_to_the_highest_available_one() {
        // `-max` on a model that tops out at `xhigh` must succeed and reason less, and the
        // substitution must be visible rather than silent.
        let snapshot = compile_one(
            vec![with_levels(
                "m1",
                &[
                    ReasoningLevel::Low,
                    ReasoningLevel::High,
                    ReasoningLevel::XHigh,
                ],
            )],
            Vec::new(),
        );

        let (model, level, substituted) = direct(snapshot.resolve("p1/m1-max"));
        assert_eq!(model, "m1");
        assert_eq!(level, ReasoningLevel::XHigh.rank());
        assert!(
            substituted,
            "the substitution must be reported, not absorbed"
        );
    }

    #[test]
    fn every_level_name_is_indexed_even_when_the_model_lacks_it() {
        // Otherwise a client asking for a tier it cannot have would get a 404 instead of a
        // downgrade, which is the opposite of the documented rule.
        let snapshot = compile_one(
            vec![with_levels(
                "m1",
                &[ReasoningLevel::Low, ReasoningLevel::High],
            )],
            Vec::new(),
        );

        for level in ReasoningLevel::ALL {
            let expected = if level == ReasoningLevel::Low || level == ReasoningLevel::High {
                level.rank()
            } else {
                ReasoningLevel::High.rank()
            };
            let (_, actual, substituted) =
                direct(snapshot.resolve(&format!("p1/m1-{}", level.as_str())));
            assert_eq!(actual, expected, "at {}", level.as_str());
            if level != ReasoningLevel::Low && level != ReasoningLevel::High {
                assert!(substituted, "at {}", level.as_str());
            }
        }
    }

    #[test]
    fn a_none_level_step_emits_no_reasoning_fragment() {
        let snapshot = compile_one(
            vec![with_levels(
                "m1",
                &[
                    ReasoningLevel::None,
                    ReasoningLevel::Low,
                    ReasoningLevel::High,
                ],
            )],
            Vec::new(),
        );

        match snapshot.resolve("p1/m1-none") {
            Resolution::Direct { step, level, .. } => {
                assert_eq!(level, ReasoningLevel::None.rank());
                assert_eq!(
                    step.reasoning_fragment_for(ReasoningLevel::None),
                    None,
                    "`none` must send no reasoning field at all, on every dialect"
                );
                // The step's *default* level is a different thing from the level this name asked
                // for, and it still carries its own fragment.
                assert!(step.reasoning_fragment().is_some());
            }
            other => panic!("expected a direct hit, got {other:?}"),
        }
    }

    #[test]
    fn a_model_with_no_levels_gets_no_suffix_and_runs_at_none() {
        // The honest answer for a model nobody has metadata for and whose provider does not claim
        // the reasoning capability: no tiers, and no invented `reasoning_effort`.
        let snapshot = compile_one(vec![with_levels("m1", &[])], Vec::new());

        match snapshot.resolve("p1/m1") {
            Resolution::Direct { step, level, .. } => {
                assert_eq!(step.levels, 0);
                assert!(step.levels().is_empty());
                assert_eq!(level, ReasoningLevel::None.rank());
                assert_eq!(step.reasoning_fragment(), None);
            }
            other => panic!("expected a direct hit, got {other:?}"),
        }
        for level in ReasoningLevel::ALL {
            assert!(!snapshot
                .providers()
                .first()
                .unwrap()
                .steps
                .first()
                .unwrap()
                .supports_level(level));
        }
    }

    #[test]
    fn a_provider_declaring_reasoning_seeds_the_dialects_level_set() {
        let mut p = level_provider("p1", vec![with_levels("m1", &[])]);
        p.capabilities = vec![Capability::Chat, Capability::Reasoning];
        let mut config = Config {
            providers: vec![p],
            ..Config::default()
        };
        config.combos = Vec::new();
        let snapshot = RoutingSnapshot::compile(&config).unwrap();

        let step = &snapshot.providers()[0].steps[0];
        assert_eq!(
            step.levels(),
            vec![
                ReasoningLevel::None,
                ReasoningLevel::Minimal,
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
            ],
            "the seed is the openai dialect's own vocabulary, not a hand-written list"
        );
        assert!(
            !step.supports_level(ReasoningLevel::Max),
            "a tier the dialect rewrites to xhigh must not be offered"
        );
    }

    #[test]
    fn resolution_is_case_insensitive_including_the_level() {
        let snapshot = compile_one(
            vec![with_levels(
                "m1",
                &[ReasoningLevel::Low, ReasoningLevel::High],
            )],
            Vec::new(),
        );

        for requested in ["p1/m1-high", "P1/M1-HIGH", "p1/M1-High"] {
            let (_, level, substituted) = direct(snapshot.resolve(requested));
            assert_eq!(level, ReasoningLevel::High.rank(), "{requested}");
            assert!(!substituted, "{requested}");
        }
    }

    #[test]
    fn a_user_alias_beats_a_level_qualified_model_name() {
        let config = Config {
            providers: vec![level_provider(
                "p1",
                vec![with_levels(
                    "m1",
                    &[ReasoningLevel::Low, ReasoningLevel::High],
                )],
            )],
            aliases: vec![Alias {
                from: "p1/m1-high".into(),
                to: "p1/m1-low".into(),
                generated: false,
            }],
            ..Config::default()
        };
        let snapshot = RoutingSnapshot::compile(&config).unwrap();

        let (_, level, _) = direct(snapshot.resolve("p1/m1-high"));
        assert_eq!(
            level,
            ReasoningLevel::Low.rank(),
            "the alias must win over the derived level name"
        );
    }

    #[test]
    fn level_metadata_survives_the_encode_decode_round_trip() {
        // The test that catches a codec which forgets a field: a decoded snapshot is what the relay
        // actually loads, so a level lost in the frame is a level lost in production.
        let mut declared = with_levels(
            "deepseek/deepseek-v4.1-flash",
            &[
                ReasoningLevel::Low,
                ReasoningLevel::Medium,
                ReasoningLevel::High,
                ReasoningLevel::XHigh,
            ],
        );
        declared.default_reasoning_level = Some(ReasoningLevel::Medium);
        declared.input_modalities = vec![InputModality::Text, InputModality::Image];
        declared.pricing = Some(PricingPlan {
            input_per_mtok: Some(0.27),
            output_per_mtok: Some(1.1),
            cache_read_per_mtok: None,
            cache_write_per_mtok: Some(0.135),
            source: PricingSource::Synced,
        });
        declared.reasoning_wire = Some(
            [(
                ReasoningLevel::XHigh,
                serde_json::json!({ "reasoning_effort": "high" }),
            )]
            .into_iter()
            .collect(),
        );

        let config = Config {
            providers: vec![level_provider("p1", vec![declared])],
            // A combo step must carry the same metadata as the direct model it names.
            combos: vec![combo(
                "c1",
                "coding",
                &[("p1", "deepseek/deepseek-v4.1-flash")],
            )],
            ..Config::default()
        };

        let original = RoutingSnapshot::compile(&config).unwrap();
        let decoded = RoutingSnapshot::decode(&original.encode().unwrap()).unwrap();
        assert_eq!(decoded, original);

        let direct_step = &decoded.providers()[0].steps[0];
        assert_eq!(direct_step.levels, original.providers()[0].steps[0].levels);
        assert!(direct_step.supports_level(ReasoningLevel::XHigh));
        assert!(!direct_step.supports_level(ReasoningLevel::Max));
        assert_eq!(direct_step.default_level, ReasoningLevel::Medium.rank());
        assert!(direct_step.accepts_modality(InputModality::Image));
        assert!(!direct_step.accepts_modality(InputModality::Audio));
        assert_eq!(
            direct_step.pricing.as_ref().unwrap().input_per_mtok,
            Some(0.27)
        );
        // The step's default level is the model's own `medium`, so the default fragment is the
        // dialect's `medium` fragment...
        assert_eq!(
            direct_step.reasoning_fragment(),
            Some(r#"{"reasoning_effort":"medium"}"#),
            "the default fragment comes from the step's default level"
        );
        // ...while the operator's override applies at the level it names, replacing the dialect
        // default there rather than merging with it.
        assert_eq!(
            direct_step.reasoning_fragment_for(ReasoningLevel::XHigh),
            Some(r#"{"reasoning_effort":"high"}"#),
            "a per-model override replaces the dialect default wholesale for that level"
        );

        let combo_step = &decoded.combos()[0].steps[0];
        assert_eq!(combo_step.levels, direct_step.levels);
        assert_eq!(combo_step.default_level, direct_step.default_level);
        assert_eq!(
            combo_step.reasoning_json, direct_step.reasoning_json,
            "a combo step and the model it names must compile identically"
        );
    }

    #[test]
    fn a_thinking_budget_is_fitted_to_the_models_own_output_ceiling() {
        let fragment_for = |dialect: Dialect, ceiling: Option<u32>| {
            let mut config = Config::default();
            let mut p = level_provider("p1", vec![with_levels("m1", &[ReasoningLevel::High])]);
            p.dialect = dialect;
            p.models[0].max_output_tokens = ceiling;
            config.providers = vec![p];
            RoutingSnapshot::compile(&config).unwrap().providers()[0].steps[0].reasoning_json
                [ReasoningLevel::High.rank() as usize]
                .as_deref()
                .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap())
        };

        // Anthropic refuses `budget_tokens` at or above `max_tokens`, so a budget at the model's
        // own ceiling makes every request a 400. The answer needs room too, hence the headroom.
        // Unlike the Gemini-shaped dialects below, the fragment states no ceiling of its own: the
        // Anthropic translator always writes `max_tokens`, so a second opinion here would be a
        // number with nothing to override.
        assert_eq!(
            fragment_for(Dialect::Anthropic, Some(64_000)),
            Some(serde_json::json!({
                "thinking": { "type": "enabled", "budget_tokens": 59_904 }
            }))
        );

        // Gemini spends the same allowance on thinking, which is how a reply arrives carrying
        // nothing but thoughts: the candidate budget is exhausted before any text is written.
        assert_eq!(
            fragment_for(Dialect::Google, Some(65_536)),
            Some(serde_json::json!({
                "generationConfig": {
                    "maxOutputTokens": 65_536,
                    "thinkingConfig": { "thinkingLevel": "high", "thinkingBudget": 61_440 }
                }
            }))
        );
        assert_eq!(
            fragment_for(Dialect::Antigravity, Some(64_000)),
            Some(serde_json::json!({
                "request": {
                    "generationConfig": {
                        "maxOutputTokens": 64_000,
                        "thinkingConfig": { "thinkingLevel": "high", "thinkingBudget": 59_904 }
                    }
                }
            }))
        );

        // A ceiling with no room left to both think and answer drops the field entirely: a clamped
        // 1024 budget under a 4096 ceiling is a request that cannot produce an answer at all.
        assert_eq!(fragment_for(Dialect::Anthropic, Some(4_096)), None);

        // The AI Studio default has no budget to fit — it is sent a level alone — so no ceiling
        // applies to it here. What happens to a fragment that does name one, on either surface,
        // is the next test's subject.
        assert_eq!(
            fragment_for(Dialect::GoogleAiStudio, Some(4_096)),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingLevel": "high" } }
            })),
            "no budget, no clamp, and the level is not dropped: this dialect's fragment cannot \
             spend the answer's allowance"
        );

        // A model that declares no ceiling keeps the table's budget — there is nothing to fit to,
        // and inventing a ceiling would be inventing a capability.
        assert_eq!(
            fragment_for(Dialect::Anthropic, None),
            Some(serde_json::json!({
                "thinking": { "type": "enabled", "budget_tokens": 131_072 }
            }))
        );
    }
    #[test]
    fn the_ai_studio_surface_is_fitted_to_its_own_budget_range_not_only_the_models_ceiling() {
        // Measured against the public API on 2026-11-11 with an AI Studio key: a body carrying
        // `thinkingBudget: 131072` is refused with `thinking_budget must be in the range
        // [-1, 65535]`, and one carrying both a budget and a level is refused with "You can only
        // set only one of thinking budget and thinking level." The dialect default therefore
        // states a level, and the range bound is the second opinion applied to any fragment that
        // does state a budget — including one an operator authored, which is the only way this
        // dialect's fragment grows one.
        let fragment_for = |wire: Option<serde_json::Value>, ceiling: Option<u32>| {
            let mut config = Config::default();
            let mut p = level_provider("p1", vec![with_levels("m1", &[ReasoningLevel::High])]);
            p.dialect = Dialect::GoogleAiStudio;
            p.models[0].max_output_tokens = ceiling;
            p.models[0].reasoning_wire =
                wire.map(|fragment| [(ReasoningLevel::High, fragment)].into_iter().collect());
            config.providers = vec![p];
            RoutingSnapshot::compile(&config).unwrap().providers()[0].steps[0].reasoning_json
                [ReasoningLevel::High.rank() as usize]
                .as_deref()
                .map(|text| serde_json::from_str::<serde_json::Value>(text).unwrap())
        };

        // The default: a level, and nothing else. A model that declares a ceiling changes
        // nothing, because there is no budget for that ceiling to bound — `maxOutputTokens`
        // travels with a budget, and stating it here would be stating a cap the fragment is not
        // measured against.
        assert_eq!(
            fragment_for(None, Some(65_536)),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingLevel": "high" } }
            }))
        );

        // A budget larger than the surface accepts is lowered to the surface's own ceiling, not
        // to the model's: `131072` is a number this API answers 400 for, whatever the model's
        // output allowance happens to be. The ceiling is then stated, as on the Gemini dialects
        // above, because the budget is carved out of it.
        assert_eq!(
            fragment_for(
                Some(
                    serde_json::json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":131_072}}})
                ),
                None,
            ),
            Some(serde_json::json!({
                "generationConfig": { "thinkingConfig": { "thinkingBudget": 65_535 } }
            }))
        );

        // Both bounds at once: the model's allowance minus headroom is the tighter of the two, so
        // the budget is fitted to that and the ceiling it was fitted against is sent with it.
        assert_eq!(
            fragment_for(
                Some(
                    serde_json::json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":131_072}}})
                ),
                Some(65_536),
            ),
            Some(serde_json::json!({
                "generationConfig": {
                    "maxOutputTokens": 65_536,
                    "thinkingConfig": { "thinkingBudget": 61_440 }
                }
            }))
        );

        // And a ceiling with no room left to both think and answer drops a budgeted fragment
        // entirely, exactly as it does for Anthropic: this API accepts no budget below 1024, so a
        // clamped 1024 under a 4096 allowance is a request that cannot produce an answer.
        assert_eq!(
            fragment_for(
                Some(
                    serde_json::json!({"generationConfig":{"thinkingConfig":{"thinkingBudget":32_768}}})
                ),
                Some(4_096),
            ),
            None
        );
    }

    #[test]
    fn each_dialect_maps_a_level_onto_the_field_it_actually_understands() {
        let fragments = |dialect: Dialect| {
            let mut config = Config::default();
            let mut p = level_provider(
                "p1",
                vec![with_levels(
                    "m1",
                    &[
                        ReasoningLevel::None,
                        ReasoningLevel::Low,
                        ReasoningLevel::High,
                    ],
                )],
            );
            p.dialect = dialect;
            config.providers = vec![p];
            RoutingSnapshot::compile(&config).unwrap().providers()[0].steps[0]
                .reasoning_json
                .clone()
        };

        // `none` is silent everywhere: a provider that does not recognise
        // `{"reasoning_effort":"none"}` answers 400.
        assert_eq!(
            fragments(Dialect::OpenAi)[ReasoningLevel::None.rank() as usize],
            None
        );
        assert_eq!(
            fragments(Dialect::Anthropic)[ReasoningLevel::None.rank() as usize],
            None
        );

        let openai = fragments(Dialect::OpenAi);
        assert_eq!(
            openai[ReasoningLevel::Low.rank() as usize].as_deref(),
            Some(r#"{"reasoning_effort":"low"}"#)
        );

        let codex = fragments(Dialect::Codex);
        assert_eq!(
            codex[ReasoningLevel::High.rank() as usize].as_deref(),
            Some(r#"{"reasoning":{"effort":"high","summary":"auto"}}"#)
        );

        let anthropic = fragments(Dialect::Anthropic);
        assert_eq!(
            anthropic[ReasoningLevel::High.rank() as usize].as_deref(),
            Some(r#"{"thinking":{"type":"enabled","budget_tokens":131072}}"#)
        );

        let google = fragments(Dialect::Google);
        assert_eq!(
            google[ReasoningLevel::High.rank() as usize].as_deref(),
            Some(
                r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"high","thinkingBudget":131072}}}"#
            )
        );

        // The AI Studio surface refuses a body carrying both a level and a budget ("You can only
        // set only one of thinking budget and thinking level."), so it is sent the level alone —
        // the only control here that cannot be out of range. Its vocabulary is `low`/`high`, so
        // the two tiers below `high` are both `low`.
        let ai_studio = fragments(Dialect::GoogleAiStudio);
        assert_eq!(
            ai_studio[ReasoningLevel::Low.rank() as usize].as_deref(),
            Some(r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"}}}"#)
        );
        assert_eq!(
            ai_studio[ReasoningLevel::High.rank() as usize].as_deref(),
            Some(r#"{"generationConfig":{"thinkingConfig":{"thinkingLevel":"high"}}}"#)
        );

        // Antigravity sends the same field one level deeper: its body is an envelope, and a
        // `generationConfig` at the envelope's top level is a field of nothing — the upstream
        // answers 400 `Unknown name "generationConfig": Cannot find field.`
        let antigravity = fragments(Dialect::Antigravity);
        assert_eq!(
            antigravity[ReasoningLevel::High.rank() as usize].as_deref(),
            Some(
                r#"{"request":{"generationConfig":{"thinkingConfig":{"thinkingLevel":"high","thinkingBudget":131072}}}}"#
            )
        );
    }

    #[test]
    fn generated_aliases_are_deleted_while_authored_ones_survive() {
        let mut document = serde_json::json!({
            "aliases": [
                { "from": "generated-one", "to": "coding", "generated": true },
                { "from": "authored", "to": "coding", "generated": false },
                { "from": "no-flag", "to": "coding" },
                "not an object"
            ]
        });
        let removed = crate::config::migrate_generated_aliases(&mut document);
        assert_eq!(removed, 1);
        let remaining = document["aliases"].as_array().unwrap();
        assert_eq!(remaining.len(), 3);
        assert!(remaining
            .iter()
            .all(|alias| alias.get("from").and_then(|v| v.as_str()) != Some("generated-one")));
    }

    // -----------------------------------------------------------------------
    // per-step account selection and per-person egress
    // -----------------------------------------------------------------------

    /// A provider with three accounts, two of them tagged for people.
    fn selected_config() -> Config {
        let mut config = Config::default();
        let mut p = provider("alpha");
        p.models = vec![model("m")];
        let mut mine = account("a1", "mine", 0);
        mine.tags.push("alice".into());
        let mut theirs = account("a2", "theirs", 0);
        theirs.tags.push("bob".into());
        let mut spare = account("a3", "spare", 0);
        spare.enabled = false;
        p.accounts = vec![mine, theirs, spare];
        config.providers.push(p);
        config
    }

    fn proxy(id: &str) -> crate::config::ProxyConfig {
        crate::config::ProxyConfig {
            id: id.into(),
            label: None,
            url: "socks5h://100.64.0.7:1080".into(),
            username: None,
            secret_ref: None,
            enabled: true,
            priority: 0,
            extra: Default::default(),
        }
    }

    /// The default is every account, and it must stay that way: a combo written before this
    /// feature existed has to behave exactly as it did.
    #[test]
    fn a_step_without_a_selection_plans_every_account() {
        let mut config = selected_config();
        config
            .combos
            .push(combo("chain", "chain", &[("alpha", "m")]));
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let step = &snapshot.combos()[0].steps[0];
        assert_eq!(step.accounts, None);
    }

    #[test]
    fn a_step_selection_resolves_to_the_named_accounts() {
        let mut config = selected_config();
        let mut c = combo("chain", "chain", &[("alpha", "m")]);
        c.steps[0].accounts = vec!["alice".into()];
        config.combos.push(c);
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let step = &snapshot.combos()[0].steps[0];
        assert_eq!(
            step.accounts.as_ref().unwrap(),
            &vec![AccountId::from_raw("a1")]
        );

        // An id and a tag select the same account, and the order is the provider's, not the
        // selector's.
        let mut config = selected_config();
        let mut c = combo("chain", "chain", &[("alpha", "m")]);
        c.steps[0].accounts = vec!["bob".into(), "a1".into()];
        config.combos.push(c);
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(
            snapshot.combos()[0].steps[0].accounts.as_ref().unwrap(),
            &vec![AccountId::from_raw("a1"), AccountId::from_raw("a2")]
        );
    }

    /// A selection that matches only a disabled account is an empty set, not "all": widening it
    /// back would send traffic through an account the operator deliberately excluded.
    #[test]
    fn a_selection_of_only_disabled_accounts_stays_empty() {
        let mut config = selected_config();
        let mut c = combo("chain", "chain", &[("alpha", "m")]);
        c.steps[0].accounts = vec!["a3".into()];
        config.combos.push(c);
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(
            snapshot.combos()[0].steps[0].accounts.as_ref().unwrap(),
            &Vec::<AccountId>::new()
        );
    }

    /// The selection survives the snapshot, and a snapshot naming an account the provider does not
    /// have is refused rather than silently ignored.
    #[test]
    fn the_account_selection_round_trips_and_is_checked() {
        let mut config = selected_config();
        let mut c = combo("chain", "chain", &[("alpha", "m")]);
        c.steps[0].accounts = vec!["alice".into()];
        config.combos.push(c);
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let decoded = RoutingSnapshot::decode(&snapshot.encode().unwrap()).unwrap();
        assert_eq!(decoded, snapshot);
        assert_eq!(
            decoded.combos()[0].steps[0].accounts.as_ref().unwrap(),
            &vec![AccountId::from_raw("a1")]
        );

        // Corrupt the account id in the payload: the decoder must refuse it, because a step that
        // selects an account the provider does not have could never run.
        let mut bytes = snapshot.encode().unwrap();
        let needle = b"a1";
        let at = bytes
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("the account id is in the payload");
        bytes[at] = b'z';
        bytes[at + 1] = b'z';
        let error = RoutingSnapshot::decode(&bytes).unwrap_err().to_string();
        assert!(
            error.contains("selects unknown account") || error.contains("checksum"),
            "{error}"
        );
    }

    /// Tagging an account for a person is enough: the compiled plan carries that person's proxy
    /// with no per-account configuration.
    #[test]
    fn a_person_tag_puts_their_proxy_in_the_plan() {
        let mut config = selected_config();
        config.proxies.push(proxy("laptop"));
        config
            .people
            .push(crate::config::PersonConfig::new("alice", "laptop"));
        config
            .combos
            .push(combo("chain", "chain", &[("alpha", "m")]));
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        let accounts = &snapshot.providers()[0].accounts;
        assert_eq!(accounts[0].proxy.as_deref(), Some("laptop"));
        assert_eq!(accounts[1].proxy, None, "bob has no person configured");
        // The account's own choice still wins over the person's.
        let mut config = selected_config();
        config.proxies.push(proxy("laptop"));
        config.proxies.push(proxy("office"));
        config
            .people
            .push(crate::config::PersonConfig::new("alice", "laptop"));
        config.providers[0].accounts[0].proxy = Some("office".into());
        let snapshot = RoutingSnapshot::compile(&config).unwrap();
        assert_eq!(
            snapshot.providers()[0].accounts[0].proxy.as_deref(),
            Some("office")
        );
    }
}
