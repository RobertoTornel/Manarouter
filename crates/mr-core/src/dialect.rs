//! Wire dialects, capabilities, and the small closed vocabularies that configure routing.
//!
//! Everything in this module is a closed set that appears in the routing snapshot. Keeping them
//! as fieldless enums with explicit `repr` values is deliberate: the relay matches on a `u8`
//! loaded straight out of the memory-mapped snapshot rather than deserializing a string, so a
//! routing decision never touches a parser.

use serde::{Deserialize, Serialize};

/// A dialect is a *shape of HTTP API*, not a vendor.
///
/// Providers are grouped by dialect because translation is only required when the client's
/// dialect differs from the upstream's. Two providers that share a dialect share every code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum Dialect {
    /// OpenAI's public API surface: `/v1/chat/completions`, `/v1/responses`,
    /// `/v1/embeddings`, `/v1/audio/*`, `/v1/images/generations`, `/v1/models`.
    ///
    /// This is the baseline: by far the largest group of providers speak it.
    OpenAi = 0,
    /// Anthropic's Messages API: `/v1/messages`, `/v1/messages/count_tokens`.
    Anthropic = 1,
    /// Google's `generativelanguage` API: `models/{model}:generateContent` and
    /// `:streamGenerateContent?alt=sse`. Used by plain Gemini API-key access.
    Google = 2,
    /// OpenAI Codex on a ChatGPT subscription: the Responses API at
    /// `chatgpt.com/backend-api/codex/responses`, SSE-first, `store` must be `false`.
    Codex = 3,
    /// GitHub Copilot: `api.githubcopilot.com`, bearer minted from a GitHub token.
    Copilot = 4,
    /// Google Antigravity / Gemini Code Assist: `cloudcode-pa.googleapis.com`
    /// `v1internal:*` with a project/tier onboarding envelope.
    Antigravity = 5,
    /// Command Code's CLI envelope at `api.commandcode.ai/alpha/generate`, which returns its
    /// own event stream rather than OpenAI SSE.
    CommandCode = 6,
}

impl Dialect {
    /// The `u8` stored in the snapshot.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a dialect from its snapshot `u8`, rejecting unknown values rather than guessing.
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::OpenAi,
            1 => Self::Anthropic,
            2 => Self::Google,
            3 => Self::Codex,
            4 => Self::Copilot,
            5 => Self::Antigravity,
            6 => Self::CommandCode,
            _ => return None,
        })
    }

    /// The stable identifier used in config files, the panel and documentation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Antigravity => "antigravity",
            Self::CommandCode => "commandcode",
        }
    }

    /// Parse the config identifier. Case-insensitive, and accepts common aliases so that both
    /// hand-written config and migrated OmniRoute data load.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "openai" | "openai-compatible" | "openai_compatible" | "oa" | "compatible" => {
                Self::OpenAi
            }
            "anthropic" | "anthropic-compatible" | "claude" | "messages" => Self::Anthropic,
            "google" | "gemini" | "generativelanguage" => Self::Google,
            "codex" | "chatgpt" | "chatgpt-codex" => Self::Codex,
            "copilot" | "github-copilot" | "githubcopilot" => Self::Copilot,
            "antigravity" | "agy" | "gemini-code-assist" | "cloudcode" => Self::Antigravity,
            "commandcode" | "command-code" | "cmd" => Self::CommandCode,
            _ => return None,
        })
    }

    /// Whether a client speaking `self` can be forwarded byte-for-byte to a provider speaking
    /// `other`, with only header changes.
    ///
    /// This is the single most latency-relevant predicate in the project: when it returns `true`
    /// the relay never parses or re-serializes a body.
    #[inline]
    pub const fn is_passthrough_to(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::OpenAi, Self::OpenAi) | (Self::Anthropic, Self::Anthropic)
        )
    }

    /// Whether this dialect always answers with `text/event-stream` even when the client asked
    /// for a single JSON document.
    ///
    /// Codex and Antigravity do, so the relay must synthesize a non-streaming response for a
    /// non-streaming client rather than forwarding the SSE body.
    #[inline]
    pub const fn force_stream(self) -> bool {
        matches!(self, Self::Codex | Self::Antigravity)
    }

    /// Every dialect, in snapshot order. Useful for exhaustive tests.
    pub const ALL: [Dialect; 7] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Google,
        Self::Codex,
        Self::Copilot,
        Self::Antigravity,
        Self::CommandCode,
    ];
}

/// A capability is a *route*, not a model feature. It decides which client endpoints a provider
/// can serve, and lets the panel grey out combinations that cannot work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum Capability {
    /// `POST /v1/chat/completions`
    Chat = 0,
    /// `POST /v1/responses`
    Responses = 1,
    /// `POST /v1/embeddings`
    Embeddings = 2,
    /// `POST /v1/audio/transcriptions`
    AudioTranscription = 3,
    /// `POST /v1/audio/speech`
    AudioSpeech = 4,
    /// `POST /v1/images/generations`
    ImagesGenerations = 5,
    /// `GET /v1/models`
    Models = 6,
    /// `POST /v1/messages` (Anthropic Messages)
    Messages = 7,
    /// Tool / function calling is honoured on this route.
    ToolCalling = 8,
    /// The upstream can emit reasoning/thinking content.
    Reasoning = 9,
    /// Image input is accepted (vision).
    Vision = 10,
}

impl Capability {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Responses => "responses",
            Self::Embeddings => "embeddings",
            Self::AudioTranscription => "audio.transcription",
            Self::AudioSpeech => "audio.speech",
            Self::ImagesGenerations => "images.generations",
            Self::Models => "models",
            Self::Messages => "messages",
            Self::ToolCalling => "tool-calling",
            Self::Reasoning => "reasoning",
            Self::Vision => "vision",
        }
    }

    /// Parse the config identifier, accepting both dotted and hyphenated spellings.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "chat" | "chat-completions" | "chat_completions" => Self::Chat,
            "responses" | "response" | "responses-api" => Self::Responses,
            "embeddings" | "embedding" => Self::Embeddings,
            "audio.transcription" | "audio-transcription" | "transcription" | "stt" => {
                Self::AudioTranscription
            }
            "audio.speech" | "audio-speech" | "speech" | "tts" => Self::AudioSpeech,
            "images.generations" | "images-generations" | "images" | "image-generation" => {
                Self::ImagesGenerations
            }
            "models" | "model-list" => Self::Models,
            "messages" | "anthropic-messages" => Self::Messages,
            "tool-calling" | "tools" | "function-calling" => Self::ToolCalling,
            "reasoning" | "thinking" => Self::Reasoning,
            "vision" | "image-input" => Self::Vision,
            _ => return None,
        })
    }

    /// The capability that a given client route requires.
    ///
    /// Used by the relay to reject a route before dialling upstream, and by the panel to warn
    /// that a provider cannot serve a combo step.
    pub const fn for_route(route: RouteKind) -> Self {
        match route {
            RouteKind::ChatCompletions => Self::Chat,
            RouteKind::Responses => Self::Responses,
            RouteKind::Embeddings => Self::Embeddings,
            RouteKind::AudioTranscription => Self::AudioTranscription,
            RouteKind::AudioSpeech => Self::AudioSpeech,
            RouteKind::ImagesGenerations => Self::ImagesGenerations,
            RouteKind::Models => Self::Models,
            RouteKind::Messages | RouteKind::CountTokens => Self::Messages,
        }
    }

    /// The capabilities every provider is assumed to have unless configured otherwise.
    pub const BASELINE: [Capability; 1] = [Capability::Chat];
}

/// Which client-facing endpoint a request arrived on.
///
/// The relay extracts this from the path with a match on the first few bytes; nothing about it
/// comes from parsing a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum RouteKind {
    /// `POST /v1/chat/completions`
    ChatCompletions = 0,
    /// `POST /v1/responses`
    Responses = 1,
    /// `POST /v1/embeddings`
    Embeddings = 2,
    /// `POST /v1/audio/transcriptions`
    AudioTranscription = 3,
    /// `POST /v1/audio/speech`
    AudioSpeech = 4,
    /// `POST /v1/images/generations`
    ImagesGenerations = 5,
    /// `GET /v1/models`
    Models = 6,
    /// `POST /v1/messages`
    Messages = 7,
    /// `POST /v1/messages/count_tokens`
    CountTokens = 8,
}

impl RouteKind {
    /// The client dialect implied by this route.
    #[inline]
    pub const fn client_dialect(self) -> Dialect {
        match self {
            Self::ChatCompletions
            | Self::Responses
            | Self::Embeddings
            | Self::AudioTranscription
            | Self::AudioSpeech
            | Self::ImagesGenerations
            | Self::Models => Dialect::OpenAi,
            Self::Messages | Self::CountTokens => Dialect::Anthropic,
        }
    }

    /// Whether this route carries a request body at all.
    ///
    /// `GET /v1/models` does not, which lets the relay skip body handling entirely.
    #[inline]
    pub const fn has_request_body(self) -> bool {
        !matches!(self, Self::Models)
    }

    /// Whether this route can be usefully raced between providers.
    ///
    /// Hedging only makes sense for generative, streaming, text-producing routes. Hedging an
    /// embedding or an image generation would duplicate work and burn quota for no benefit, so
    /// the combo compiler refuses to build a hedged plan for them.
    #[inline]
    pub const fn is_hedgeable(self) -> bool {
        matches!(
            self,
            Self::ChatCompletions | Self::Responses | Self::Messages
        )
    }

    /// Whether a response to this route is expected to be `text/event-stream` when streaming.
    #[inline]
    pub const fn is_sse_capable(self) -> bool {
        !matches!(self, Self::Models | Self::CountTokens)
    }

    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat.completions",
            Self::Responses => "responses",
            Self::Embeddings => "embeddings",
            Self::AudioTranscription => "audio.transcriptions",
            Self::AudioSpeech => "audio.speech",
            Self::ImagesGenerations => "images.generations",
            Self::Models => "models",
            Self::Messages => "messages",
            Self::CountTokens => "messages.count-tokens",
        }
    }

    /// Every route, for exhaustive tests and the panel's routing table.
    pub const ALL: [RouteKind; 9] = [
        Self::ChatCompletions,
        Self::Responses,
        Self::Embeddings,
        Self::AudioTranscription,
        Self::AudioSpeech,
        Self::ImagesGenerations,
        Self::Models,
        Self::Messages,
        Self::CountTokens,
    ];
}

/// How the credential is attached to an upstream request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type")]
#[derive(Default)]
pub enum AuthKind {
    /// `Authorization: Bearer <secret>` — the OpenAI convention and the most common.
    #[default]
    Bearer,
    /// `x-api-key: <secret>` — the Anthropic convention.
    ApiKeyHeader {
        /// Header name, defaulting to `x-api-key`.
        #[serde(default = "default_api_key_header")]
        header: String,
    },
    /// An arbitrary header, for providers that invent their own.
    Header {
        /// Header name.
        name: String,
        /// Optional static prefix, for example `Token `.
        #[serde(default)]
        prefix: String,
    },
    /// A query-string parameter, for the Google-style endpoints that want `?key=`.
    QueryParam {
        /// Parameter name, defaulting to `key`.
        #[serde(default = "default_query_param")]
        param: String,
    },
    /// The credential needs an OAuth platform to turn it into live request headers, and may need
    /// a short-lived session token minted from a long-lived one (Copilot).
    ///
    /// The tag is pinned with an explicit `rename` rather than left to the container's
    /// `kebab-case`. Kebab-casing an acronym produces `o-auth`, which is not a word anybody would
    /// guess and reads as a typo in a hand-edited configuration. Pinning it also means the wire
    /// name is stated here rather than being an emergent property of the rename rule.
    #[serde(rename = "oauth")]
    OAuth {
        /// Which registered OAuth platform handles this account.
        platform: String,
    },
}

fn default_api_key_header() -> String {
    "x-api-key".to_string()
}

fn default_query_param() -> String {
    "key".to_string()
}

impl AuthKind {
    /// Whether this auth kind requires the OAuth subsystem rather than a static secret.
    #[inline]
    pub const fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth { .. })
    }
}

/// How to choose among the accounts of one provider, or among the steps of a combo.
///
/// The same vocabulary is reused for both so the panel presents one selector and the relay's
/// selection code is a single match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum KeyStrategy {
    /// Always the first available account in priority order. Deterministic and sticky, which
    /// keeps prompt caches warm on a single account.
    #[default]
    Priority = 0,
    /// Cycle through available accounts. Spreads load and rate-limit headroom evenly.
    RoundRobin = 1,
    /// Pick uniformly at random. Useful when upstream shards are unbalanced.
    Random = 2,
    /// Pick the account with the fewest requests so far. Best for keeping a pool fresh when
    /// requests have wildly different costs.
    LeastUsed = 3,
}

impl KeyStrategy {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round-robin",
            Self::Random => "random",
            Self::LeastUsed => "least-used",
        }
    }

    /// Parse the config identifier, tolerating underscores and spacing, and accepting the
    /// OmniRoute spelling `fill-first` for priority.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "priority" | "first" | "fill-first" => Self::Priority,
            "round-robin" | "roundrobin" | "rr" | "rotate" => Self::RoundRobin,
            "random" | "rand" => Self::Random,
            "least-used" | "leastused" | "least" | "lru" => Self::LeastUsed,
            _ => return None,
        })
    }

    /// Every strategy, for the panel and exhaustive tests.
    pub const ALL: [KeyStrategy; 4] = [
        Self::Priority,
        Self::RoundRobin,
        Self::Random,
        Self::LeastUsed,
    ];
}

/// How a combo walks its ordered steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum ComboStrategy {
    /// Always start at step 0 and fall forward on failure. The operator's order is absolute.
    #[default]
    Priority = 0,
    /// Start at a rotating index, then fall forward from there.
    RoundRobin = 1,
    /// Priority order, but a step with no available account is demoted below every healthy step
    /// rather than removed, and is restored by a timer when its reset passes.
    ResetAwarePriority = 2,
}

impl ComboStrategy {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::RoundRobin => "round-robin",
            Self::ResetAwarePriority => "reset-aware-priority",
        }
    }

    /// Parse the config identifier.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "priority" | "fill-first" | "ordered" => Self::Priority,
            "round-robin" | "roundrobin" | "rr" => Self::RoundRobin,
            "reset-aware-priority" | "reset-aware" | "resetaward" | "quota-aware" => {
                Self::ResetAwarePriority
            }
            _ => return None,
        })
    }

    /// Whether this strategy needs per-step availability stamps to be maintained.
    #[inline]
    pub const fn needs_availability(self) -> bool {
        matches!(self, Self::ResetAwarePriority)
    }

    /// Every strategy, for the panel and exhaustive tests.
    pub const ALL: [ComboStrategy; 3] =
        [Self::Priority, Self::RoundRobin, Self::ResetAwarePriority];
}

/// What counts as a participant having "replied" during a hedged race.
///
/// The operator chooses this because it is a direct trade between how quickly a race resolves and
/// how much quota a losing hedge costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum WinnerMetric {
    /// The first non-error upstream response **body** byte.
    ///
    /// Local cancellation happens after a body frame arrives for the winner; upstream providers may
    /// continue generation or bill for losing requests after the relay drops their connections.
    #[default]
    FirstResponseByte = 0,
    /// The first fully-parsed content delta, or the complete document when the client asked for a
    /// non-streaming response.
    ///
    /// More faithful to "the model actually answered", but every loser pays for one token.
    FirstCompleteToken = 1,
}

impl WinnerMetric {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstResponseByte => "first-response-byte",
            Self::FirstCompleteToken => "first-complete-token",
        }
    }

    /// Parse the config identifier.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "first-response-byte" | "first-byte" | "ttfb" | "byte" => Self::FirstResponseByte,
            "first-complete-token" | "first-token" | "token" | "delta" => Self::FirstCompleteToken,
            _ => return None,
        })
    }
}

/// What to do when the model a combo committed to dies.
///
/// These are the four modes the operator asked for. They are exhaustive on purpose: every failure
/// path in the combo engine routes through exactly one of them, so behaviour is auditable rather
/// than emergent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode")]
pub enum WinnerFailMode {
    /// Re-run the entire competition over the remaining participants, excluding every model that
    /// has already failed, bounded by `max_rounds`.
    ///
    /// This is the requested "run this again" branch.
    RerunExcluding {
        /// How many times the competition may be re-run before giving up.
        #[serde(default = "default_max_rounds")]
        max_rounds: u8,
    },
    /// Do not re-dispatch at all: take the highest-priority participant that already passed in the
    /// original competition.
    ///
    /// Costs zero added latency, because the answer is already in the race's result set.
    PickHighestPassed,
    /// As `PickHighestPassed`, but restricted to a participant backed by a **different provider**
    /// than the one that died.
    ///
    /// This is the mode that survives a whole-provider outage, including a provider-wide quota
    /// reset, which is exactly the case a same-provider fallback cannot help with.
    PickHighestPassedDifferentProvider,
    /// Jump straight to an operator-chosen model that need not be part of the competition.
    StableFallback {
        /// Index into the combo's full step list identifying the stable model.
        step: u16,
    },
}

fn default_max_rounds() -> u8 {
    2
}

impl Default for WinnerFailMode {
    fn default() -> Self {
        Self::RerunExcluding {
            max_rounds: default_max_rounds(),
        }
    }
}

impl WinnerFailMode {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RerunExcluding { .. } => "rerun-excluding",
            Self::PickHighestPassed => "pick-highest-passed",
            Self::PickHighestPassedDifferentProvider => "pick-highest-passed-different-provider",
            Self::StableFallback { .. } => "stable-fallback",
        }
    }
}

/// What the relay does when the committed model fails **after** bytes have reached the client.
///
/// Splitting the otherwise-ambiguous case out into one explicit switch is what keeps the streaming
/// path honest: there is no hidden third behaviour, and the operator can see which one is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum MidstreamPolicy {
    /// Retry with the next best model according to the combo's rules.
    ///
    /// If nothing has been flushed to the client the switch is invisible. If bytes are already on
    /// the wire the relay emits an explicit `manarouter.retry` SSE boundary and then continues
    /// with the new model, so a client that ignores unknown events still sees a complete,
    /// terminated, non-overlapping response.
    ///
    /// The one hard guard: if any `tool_calls` delta has already been emitted the relay refuses to
    /// splice and terminates with an error, because a duplicated or half-emitted tool call
    /// corrupts the client's state machine far worse than a clean failure.
    #[default]
    Restart = 0,
    /// Never retry once bytes are on the wire. Close the stream with an error and let the client
    /// decide what to do.
    Terminate = 1,
}

impl MidstreamPolicy {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::Terminate => "terminate",
        }
    }

    /// Parse the config identifier.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "restart" | "retry" | "resume" => Self::Restart,
            "terminate" | "fail" | "error" | "close" => Self::Terminate,
            _ => return None,
        })
    }
}

/// How a request's conversation is identified, so the relay can keep a combo's winner sticky.
///
/// Sticky winners mean a hedged race runs **once per conversation** instead of once per turn,
/// which removes the hedging cost from every subsequent turn and keeps upstream prompt caches hot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum AffinityMode {
    /// Derive the key from a hash of the leading system message plus the first user message.
    /// Works with any client and is stable across turns of a growing conversation.
    #[default]
    FirstSystemAndUser = 0,
    /// Require a client-supplied `x-manarouter-session` header. Precise, but only clients that
    /// send it benefit.
    HeaderOnly = 1,
    /// Prefer the header, and fall back to content hashing when it is absent.
    HeaderOrContent = 2,
    /// No affinity at all: every turn re-runs the strategy from scratch.
    Off = 3,
}

impl AffinityMode {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FirstSystemAndUser => "first-system-and-user",
            Self::HeaderOnly => "header-only",
            Self::HeaderOrContent => "header-or-content",
            Self::Off => "off",
        }
    }

    /// Parse the config identifier.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "first-system-and-user" | "content" | "hash" => Self::FirstSystemAndUser,
            "header-only" | "header" => Self::HeaderOnly,
            "header-or-content" | "both" => Self::HeaderOrContent,
            "off" | "none" | "stateless" => Self::Off,
            _ => return None,
        })
    }
}

/// Whether a quota window is anchored to a fixed clock boundary or slides with traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum WindowKind {
    /// A window of `seconds` that starts when the first request in it arrives and resets
    /// `seconds` later.
    Rolling = 0,
    /// A window aligned to wall-clock boundaries of `seconds` (hourly, or daily when `seconds` is
    /// 86400).
    Fixed = 1,
    /// A window whose length the upstream declares through its own reset headers. The relay learns
    /// the boundary from traffic rather than assuming it.
    ProviderDeclared = 2,
}

impl WindowKind {
    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rolling => "rolling",
            Self::Fixed => "fixed",
            Self::ProviderDeclared => "provider-declared",
        }
    }

    /// Parse the config identifier.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "rolling" | "sliding" => Self::Rolling,
            "fixed" | "aligned" | "calendar" => Self::Fixed,
            "provider-declared" | "declared" | "upstream" => Self::ProviderDeclared,
            _ => return None,
        })
    }
}

/// Why an account is currently unusable. Recorded so the panel can explain itself and so the
/// scheduler can decide when to try again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum AccountState {
    /// Usable.
    Ready = 0,
    /// The operator disabled it.
    Disabled = 1,
    /// The upstream returned 429 or a quota signal; retry after `available_after`.
    RateLimited = 2,
    /// Repeated failures put it in backoff; retry after `available_after`.
    Backoff = 3,
    /// Its token expired and could not be refreshed.
    AuthFailed = 4,
    /// Every configured quota window is exhausted; retry after the next reset.
    QuotaExhausted = 5,
}

impl AccountState {
    /// Whether an account in this state may be used right now.
    #[inline]
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Disabled => "disabled",
            Self::RateLimited => "rate-limited",
            Self::Backoff => "backoff",
            Self::AuthFailed => "auth-failed",
            Self::QuotaExhausted => "quota-exhausted",
        }
    }
}

/// A provider's rolled-up health.
///
/// This is deliberately a **derived** value and never a routing input on its own: a provider with
/// one throttled account out of six is `Degraded`, not dead, and account selection routes around
/// the throttled account rather than skipping the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum ProviderHealth {
    /// No accounts are configured.
    Unconfigured = 0,
    /// At least one account is usable.
    Healthy = 1,
    /// Some, but not all, accounts are usable.
    Degraded = 2,
    /// Accounts exist but none is usable.
    Dead = 3,
}

impl ProviderHealth {
    /// Derive the rollup from account counts.
    ///
    /// This is the single place the rule "a provider is only dead when every account is dead" is
    /// expressed, so it cannot drift between the panel, the compiler and the relay.
    #[inline]
    pub const fn from_counts(total: usize, usable: usize) -> Self {
        if total == 0 {
            Self::Unconfigured
        } else if usable == 0 {
            Self::Dead
        } else if usable == total {
            Self::Healthy
        } else {
            Self::Degraded
        }
    }

    /// Whether traffic can be sent at all.
    #[inline]
    pub const fn is_serviceable(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }

    /// The stable identifier used in config and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Dead => "dead",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialect_u8_roundtrip_is_total_and_faithful() {
        for d in Dialect::ALL {
            assert_eq!(Dialect::from_u8(d.as_u8()), Some(d), "{d:?}");
            assert_eq!(Dialect::parse(d.as_str()), Some(d), "{d:?}");
        }
        // Unknown values must be rejected rather than silently mapping onto a dialect, because a
        // misread snapshot byte would otherwise route traffic to an unrelated provider.
        assert_eq!(Dialect::from_u8(7), None);
        assert_eq!(Dialect::from_u8(255), None);
    }

    #[test]
    fn only_like_dialects_are_passthrough() {
        for a in Dialect::ALL {
            for b in Dialect::ALL {
                let expected = matches!(
                    (a, b),
                    (Dialect::OpenAi, Dialect::OpenAi) | (Dialect::Anthropic, Dialect::Anthropic)
                );
                assert_eq!(a.is_passthrough_to(b), expected, "{a:?} -> {b:?}");
            }
        }
    }

    #[test]
    fn codex_and_antigravity_always_stream() {
        // Both answer with SSE even when asked for one JSON document, so the relay has to
        // synthesize a non-streaming response for a non-streaming client.
        assert!(Dialect::Codex.force_stream());
        assert!(Dialect::Antigravity.force_stream());
        assert!(!Dialect::OpenAi.force_stream());
        assert!(!Dialect::Copilot.force_stream());
    }

    #[test]
    fn only_generative_text_routes_are_hedgeable() {
        // Hedging an embedding or an image generation would duplicate work and burn quota for no
        // benefit, so the compiler must refuse to build a hedged plan for them.
        assert!(RouteKind::ChatCompletions.is_hedgeable());
        assert!(RouteKind::Responses.is_hedgeable());
        assert!(RouteKind::Messages.is_hedgeable());
        assert!(!RouteKind::Embeddings.is_hedgeable());
        assert!(!RouteKind::ImagesGenerations.is_hedgeable());
        assert!(!RouteKind::AudioSpeech.is_hedgeable());
    }

    #[test]
    fn strategy_vocabularies_accept_migrated_omniroute_spellings() {
        // OmniRoute writes `fill-first` for what we call priority; migrated data must load.
        assert_eq!(
            KeyStrategy::parse("fill-first"),
            Some(KeyStrategy::Priority)
        );
        assert_eq!(
            ComboStrategy::parse("fill-first"),
            Some(ComboStrategy::Priority)
        );
        assert_eq!(
            ComboStrategy::parse("round_robin"),
            Some(ComboStrategy::RoundRobin)
        );
        assert_eq!(
            WinnerMetric::parse("ttfb"),
            Some(WinnerMetric::FirstResponseByte)
        );
        assert_eq!(
            MidstreamPolicy::parse("close"),
            Some(MidstreamPolicy::Terminate)
        );
        assert_eq!(AffinityMode::parse("stateless"), Some(AffinityMode::Off));
    }

    #[test]
    fn strategy_panels_are_complete() {
        for s in KeyStrategy::ALL {
            assert_eq!(KeyStrategy::parse(s.as_str()), Some(s), "{s:?}");
        }
        for s in ComboStrategy::ALL {
            assert_eq!(ComboStrategy::parse(s.as_str()), Some(s), "{s:?}");
        }
        for r in RouteKind::ALL {
            // Every route must map onto a capability, and every route's client dialect must be
            // one of the two dialects the relay accepts from clients.
            let _ = Capability::for_route(r);
            assert!(matches!(
                r.client_dialect(),
                Dialect::OpenAi | Dialect::Anthropic
            ));
        }
    }

    #[test]
    fn models_route_needs_no_request_body() {
        assert!(!RouteKind::Models.has_request_body());
        assert!(RouteKind::ChatCompletions.has_request_body());
        assert!(RouteKind::CountTokens.has_request_body());
    }

    #[test]
    fn provider_is_only_dead_when_every_account_is_dead() {
        // The ratified Q4 rule, stated once and enforced everywhere through this constructor.
        assert_eq!(ProviderHealth::from_counts(6, 1), ProviderHealth::Degraded);
        assert_eq!(ProviderHealth::from_counts(6, 5), ProviderHealth::Degraded);
        assert_eq!(ProviderHealth::from_counts(6, 6), ProviderHealth::Healthy);
        assert_eq!(ProviderHealth::from_counts(6, 0), ProviderHealth::Dead);
        assert_eq!(
            ProviderHealth::from_counts(0, 0),
            ProviderHealth::Unconfigured
        );
        assert!(ProviderHealth::from_counts(6, 1).is_serviceable());
        assert!(!ProviderHealth::from_counts(6, 0).is_serviceable());
    }

    #[test]
    fn account_state_usable_is_only_ready() {
        for s in [
            AccountState::Disabled,
            AccountState::RateLimited,
            AccountState::Backoff,
            AccountState::AuthFailed,
            AccountState::QuotaExhausted,
        ] {
            assert!(!s.is_usable(), "{s:?} must not be usable");
        }
        assert!(AccountState::Ready.is_usable());
    }
}
