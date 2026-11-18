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
    /// `:streamGenerateContent?alt=sse`, with the permissive `thinkingConfig` the Cloud Code
    /// surface accepts — a level and a budget together, and a budget up to `131_072`.
    ///
    /// That permissiveness is why the public API-key surface has its own dialect rather than a
    /// note here: [`Dialect::GoogleAiStudio`] is the same shape with the stricter thinking rules
    /// `generativelanguage.googleapis.com/v1beta` actually enforces. Pick this one only for a
    /// surface known to accept both fields at once (Cloud Code without the `v1internal` envelope).
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
    ///
    /// The `-go` suffix names the Go bridge this dialect targets. The pre-rename spellings
    /// (`commandcode`, `command-code`, `cmd`) are deliberately **not** accepted by
    /// [`Dialect::parse`]: a configuration file that still contains one is old data, and the
    /// migration that rewrites such files is a separate, explicit step. Failing loudly here is
    /// what keeps a stale file from silently selecting this dialect.
    CommandCodeGo = 6,
    /// The Gemini API as Google AI Studio serves it to an API key: the same
    /// `generativelanguage.googleapis.com` body, path and `x-goog-api-key` auth as
    /// [`Dialect::Google`], with the public surface's stricter `thinkingConfig`.
    ///
    /// The body shape is shared; only the thinking control differs, and it differs in both
    /// directions at once:
    ///
    /// - `thinkingConfig` takes a `thinkingLevel` **or** a `thinkingBudget`, never both. Sending
    ///   both is a `400`: "You can only set only one of thinking budget and thinking level."
    /// - `thinkingBudget` must lie within `[-1, 65535]`. The `131_072` ceiling Cloud Code accepts
    ///   is refused: "thinking_budget must be in the range [-1, 65535]".
    /// - `thinkingLevel` is `low` or `high` on the Gemini 3 models this surface serves; those
    ///   models refuse `minimal`.
    ///
    /// Since the budget a model will accept cannot be read from this install's model metadata
    /// (a synced AI Studio model carries neither `levels` nor `max_output_tokens`), this dialect
    /// puts a *level* on the wire and never a budget: a dimensionless value cannot fall outside a
    /// range, and a level sent alone cannot collide with the mutual-exclusion rule. The exact
    /// fragments are built in [`crate::snapshot`].
    ///
    /// Measured against `v1beta` with an AI Studio key on 2026-11-11; the three sentences quoted
    /// above are the upstream's own. `Dialect::Google`'s permissive pair is what produced
    /// "thinking_budget must be in the range [-1, 65535]" for `gemini/gemini-3.8-flash`.
    GoogleAiStudio = 7,
    /// Modal-hosted vLLM endpoint: OpenAI-compatible, but with `Modal-Key` and `Modal-Secret`
    /// headers.
    ModalVllm = 8,
    /// Cursor's `agent.v1.AgentService` — a bidirectional Connect-RPC stream of protobuf frames,
    /// not an HTTP request/response pair.
    ///
    /// It has its own value rather than sharing [`Dialect::OpenAi`] because nothing about the wire
    /// is shared: the body is a length-prefixed protobuf frame written without `END_STREAM`, the
    /// answer is a stream of protobuf frames that must be answered in turn (request-context, blob
    /// store, interaction queries), and the host is discovered at runtime rather than configured.
    /// Treating it as OpenAI-compatible would send a JSON body to a service that only decodes
    /// protobuf.
    ///
    /// **Chat transport only.** The OAuth login that mints the token this dialect sends is a
    /// separate concern owned by `mr-core::oauth`; a provider using this dialect still needs a
    /// credential stored the ordinary way.
    ///
    /// The exact bytes are implemented in `mr-dialects::cursor` (frames and protobuf) and
    /// `manarouter-relay::cursor_transport` (the HTTP/2 exchange).
    Cursor = 9,
    /// The ZCode desktop app's plan surface: Anthropic Messages, at
    /// `zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages`.
    ///
    /// The body and the path are Anthropic's, unchanged — the app builds them with the AI SDK's
    /// Anthropic client, so `thinking`, cache markers, `tool_use` blocks and the SSE event names
    /// are all the ones [`Dialect::Anthropic`] already speaks. What is *not* Anthropic's is
    /// everything around the body:
    ///
    /// - The credential is a ZCode **business** token minted from a Z.ai or BigModel login, not an
    ///   Anthropic key. It goes in `x-api-key`, exactly as the AI SDK puts it there.
    /// - The request has to *look* like the app. `User-Agent: ZCode/<version>`, `X-ZCode-App-Version`,
    ///   `X-Title`, `X-Platform`, `X-Os-Category`, `X-Client-Language`, `X-Client-Timezone` and
    ///   `HTTP-Referer` are the installer-visible identity, and `X-Session-Id`,
    ///   `x-zcode-query-source` and `x-zcode-session-type` are the per-request attribution the
    ///   endpoint expects. They live in [`crate::dialect::zcode`]'s constants and are added by the
    ///   relay, per the same split [`Dialect::CommandCodeGo`] documents.
    /// - The app *also* signs its model requests (`X-Client-Sig`, `X-Client-Pow`, `X-App-Id`) when
    ///   the provider is an API key or an individual/team coding plan. That layer is not
    ///   implemented here; `docs/providers/zcode.md` records the gap and the handshake it would
    ///   need. The account (start-plan/off-peak/highspeed) path the OAuth platform here targets is
    ///   the one the app itself sends **unsigned**.
    ///
    /// Because the shape is Anthropic's, the translator forwards it byte-for-byte from an
    /// Anthropic client rather than re-serialising, the way [`Dialect::Copilot`] is verbatim for an
    /// OpenAI client. It is not in [`Dialect::is_passthrough_to`]: that predicate means "the same
    /// dialect twice", and a request to this one still needs a different set of headers.
    Zcode = 10,
    /// Anthropic Messages as **Claude Code itself** sends them: `api.anthropic.com/v1/messages`
    /// with a Claude subscription OAuth token, where the surface validates that the request comes
    /// from its own CLI.
    ///
    /// The body is Anthropic's, with one requirement [`Dialect::Anthropic`] has no reason to know:
    /// when the credential is an `sk-ant-oat-…` subscription token rather than an API key, the
    /// `system` field must begin with the CLI's own identity —
    ///
    /// > `You are Claude Code, Anthropic's official CLI for Claude.`
    ///
    /// — as the **first element** of the `system` array. Not concatenated into the caller's own
    /// text, not absent, and not merely present somewhere: a separate first array entry is answered
    /// `200`, while a request whose `system` omits it is answered
    /// `400 {"type":"error","error":{"type":"invalid_request_error","message":"Error"}}`.
    /// `claude-haiku-4-5` is exempt; the Opus and Sonnet models are not. `mr-dialects::claudecli`
    /// performs that rewrite, and it is a body rewrite rather than a header one, which is exactly
    /// why this is a dialect and not an auth method on [`Dialect::Anthropic`].
    ///
    /// The rest of the fingerprint is headers: `Authorization: Bearer` instead of `x-api-key`,
    /// `anthropic-beta: claude-code-20250219,oauth-2025-04-20`, `anthropic-version: 2023-06-01`,
    /// `user-agent: claude-cli/<version> (external, cli)` and `x-app: cli`. The relay applies them
    /// from [`crate::published`] and the provider row; a subscription token sent without them is
    /// refused before the model ever runs.
    ///
    /// The login that mints the token is a paste flow, not a loopback one: Claude Code's registered
    /// redirect is `https://console.anthropic.com/oauth/code/callback`, a page nobody's browser can
    /// reach on *this* host, so `mr-core::oauth::claude` exchanges the `code#state` the operator
    /// copies out of it. That is also why this is not in [`Dialect::is_passthrough_to`]: a request
    /// needs a different body and a different set of headers.
    ClaudeCli = 11,
    /// Mistral Vibe: the OpenAI-compatible surface at `api.mistral.ai/v1`, authenticated with the
    /// bearer the Vibe desktop login (Ory PKCE at `auth.mistral.ai`) mints.
    ///
    /// The body is OpenAI-shaped and is forwarded byte-for-byte from an OpenAI client; the dialect
    /// exists because the credential is OAuth-only and the surface is Mistral's own.
    MistralVibe = 12,
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
            6 => Self::CommandCodeGo,
            7 => Self::GoogleAiStudio,
            8 => Self::ModalVllm,
            9 => Self::Cursor,
            10 => Self::Zcode,
            11 => Self::ClaudeCli,
            12 => Self::MistralVibe,
            _ => return None,
        })
    }

    /// The canonical, *normalised* identifier: what [`Dialect::parse`] returns for, and what a
    /// diagnostic or CLI table should print.
    ///
    /// This is **not** the config-file spelling. Config files and the panel go through `serde`,
    /// whose `kebab-case` rename produces `open-ai` where this returns `openai`. The two differ
    /// exactly once, for [`Dialect::OpenAi`], and the divergence is deliberate: `parse` is the
    /// permissive human-facing parser, serde is the strict wire form. A cross-language test
    /// (`the_panel_offers_only_dialects_this_binary_deserialises`) pins the panel's list to the
    /// serde form so the stricter of the two cannot be forgotten.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
            Self::Antigravity => "antigravity",
            Self::CommandCodeGo => "command-code-go",
            Self::GoogleAiStudio => "google-ai-studio",
            Self::ModalVllm => "modal-vllm",
            Self::Cursor => "cursor",
            Self::Zcode => "zcode",
            Self::ClaudeCli => "claude-cli",
            Self::MistralVibe => "mistral-vibe",
        }
    }

    /// Parse the config identifier. Case-insensitive, and accepts common aliases so that both
    /// hand-written config and migrated OmniRoute data load.
    ///
    /// The two Gemini shapes are separated by *what the alias names*. `google` and
    /// `generativelanguage` name the protocol shape and keep [`Dialect::Google`]; `gemini`,
    /// `google-ai-studio`, `ai-studio` and `aistudio` name the vendor's public API-key surface and
    /// select [`Dialect::GoogleAiStudio`], whose thinking rules are the ones such a key is actually
    /// held to. `gemini` is the alias OmniRoute data carries, so migrated data picks up the
    /// stricter — and correct — control instead of keeping a fragment the surface rejects.
    ///
    /// The three `claude*` spellings are separated by *which credential they authenticate*:
    /// `claude` and `messages` name the Messages API with an API key and keep
    /// [`Dialect::Anthropic`]; `claude-cli`, `claudecli`, `claude-code` and `claudecode` name the
    /// CLI surface, which a subscription token authenticates only when the request also looks
    /// like the CLI.
    ///
    /// [`Dialect::CommandCodeGo`] is the deliberate exception: it accepts exactly
    /// `command-code-go` and the short alias `cmdgo`, and nothing else. The pre-rename spellings
    /// are not aliases — accepting them here would defeat the rename and hide stale config, so a
    /// file that still uses one fails loudly and the config migration rewrites it.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "openai" | "openai-compatible" | "openai_compatible" | "oa" | "compatible" => {
                Self::OpenAi
            }
            "anthropic" | "anthropic-compatible" | "claude" | "messages" => Self::Anthropic,
            // `claude` above is the *API* shape and stays [`Dialect::Anthropic`]; these name the
            // CLI's own surface, whose body carries the CLI identity block and whose credential is
            // a subscription token. Accepting `claude` here too would silently move every existing
            // configuration onto a surface an API key cannot authenticate.
            "claude-cli" | "claudecli" | "claude-code" | "claudecode" => Self::ClaudeCli,
            "google" | "generativelanguage" => Self::Google,
            "gemini" | "google-ai-studio" | "googleaistudio" | "ai-studio" | "aistudio" => {
                Self::GoogleAiStudio
            }
            "codex" | "chatgpt" | "chatgpt-codex" => Self::Codex,
            "copilot" | "github-copilot" | "githubcopilot" => Self::Copilot,
            "antigravity" | "agy" | "gemini-code-assist" | "cloudcode" => Self::Antigravity,
            "command-code-go" | "cmdgo" => Self::CommandCodeGo,
            "modal-vllm" | "modal" | "modalvllm" => Self::ModalVllm,
            // `cursor-agent` is the installed CLI's own name, so it is the spelling an operator
            // copying from `cursor-agent --version` is most likely to write.
            "cursor" | "cursor-agent" | "cursoragent" => Self::Cursor,
            "zcode" | "z-code" | "zcode-plan" => Self::Zcode,
            "mistral-vibe" | "vibe" | "mistralvibe" => Self::MistralVibe,
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
            (Self::OpenAi, Self::OpenAi)
                | (Self::OpenAi, Self::MistralVibe)
                | (Self::MistralVibe, Self::MistralVibe)
                | (Self::Anthropic, Self::Anthropic)
        )
    }

    /// Whether this dialect always answers with `text/event-stream` even when the client asked
    /// for a single JSON document.
    ///
    /// Codex, Antigravity and Cursor do, so the relay must synthesize a non-streaming response for
    /// a non-streaming client rather than forwarding the SSE body. Cursor is the extreme case: the
    /// upstream exchange is a bidirectional frame stream with no request/response mode at all, so
    /// the relay's own OpenAI-SSE rendering is always the source of the client's bytes.
    #[inline]
    pub const fn force_stream(self) -> bool {
        matches!(self, Self::Codex | Self::Antigravity | Self::Cursor)
    }

    /// Every dialect, in snapshot order. Useful for exhaustive tests.
    pub const ALL: [Dialect; 13] = [
        Self::OpenAi,
        Self::Anthropic,
        Self::Google,
        Self::Codex,
        Self::Copilot,
        Self::Antigravity,
        Self::CommandCodeGo,
        Self::GoogleAiStudio,
        Self::ModalVllm,
        Self::Cursor,
        Self::Zcode,
        Self::ClaudeCli,
        Self::MistralVibe,
    ];

    /// The reasoning levels this dialect can put on the wire as *themselves*, ascending.
    ///
    /// This is the source of truth the compiler, the validator and the panel all read. Two
    /// families exist because two kinds of parameter exist:
    ///
    /// - The **named-effort** dialects (OpenAI, Copilot, Codex, Command Code) send an effort
    ///   string, and their APIs define `none`, `minimal`, `low`, `medium`, `high` and `xhigh`.
    ///   `ultra` and `max` are deliberately absent: the compiler rewrites both to `xhigh` for
    ///   these dialects, so a model advertising `max` would be claiming a tier the upstream never
    ///   receives. A request for such a name reroutes to the model's highest offered tier through
    ///   the ordinary `-<level>` rule instead of being silently lowered.
    /// - The **budget** dialects (Anthropic, Google, Antigravity) send a number rather than a
    ///   name. Every tier on the ladder has a budget, so all eight are expressible even though
    ///   the top few share the provider's ceiling.
    ///
    /// Keeping the list here rather than in the compiler is what makes "can this provider send
    /// this level" one question with one answer: a per-model list, the panel's picker and the
    /// configuration validator cannot disagree about the vocabulary of a dialect.
    pub const fn expressible_reasoning_levels(self) -> &'static [ReasoningLevel] {
        match self {
            Self::OpenAi
            | Self::MistralVibe
            | Self::Copilot
            | Self::CommandCodeGo
            | Self::Codex
            | Self::ModalVllm => &NAMED_EFFORT_LEVELS,
            // The budget dialects and AI Studio take a level or budget into their own fields.
            // Every tier is expressible for the same reason — the upper tiers share a ceiling.
            Self::Anthropic
            | Self::Google
            | Self::Antigravity
            | Self::GoogleAiStudio
            | Self::Zcode
            | Self::ClaudeCli => &ReasoningLevel::ALL,
            // The agent wire format has no reasoning-effort field and no thinking budget: the model
            // is chosen by id and the server decides how much to think. Offering `none` alone is
            // the honest answer — every other tier would be a promise this dialect cannot keep, and
            // the compiler would happily advertise it. A model routed here keeps its default
            // behaviour; a request for a named tier is rerouted by the ordinary `-<level>` rule
            // rather than silently sent as something else.
            Self::Cursor => &NO_REASONING_LEVELS,
        }
    }

    /// Whether `level` is one this dialect can send without rewriting it into another tier.
    ///
    /// The validator refuses configuration that says otherwise, so this predicate is the one
    /// place "accepted level" and "offered level" are decided.
    pub fn supports_reasoning_level(self, level: ReasoningLevel) -> bool {
        self.expressible_reasoning_levels().contains(&level)
    }

    /// Whether every model this dialect serves reasons by default.
    ///
    /// Codex is Responses-API-only and serves reasoning models exclusively, so a model listed
    /// there with no metadata still gets a level set. The other dialects also serve plain chat
    /// models, which must not sprout level-qualified names nobody asked for; their providers turn
    /// levels on by declaring the `Reasoning` capability.
    #[inline]
    pub const fn always_reasons(self) -> bool {
        matches!(self, Self::Codex)
    }
}

/// The effort values a named-effort dialect defines.
///
/// A module-level constant rather than an inline array so
/// [`Dialect::expressible_reasoning_levels`] can return a stable slice from a `const fn`; the
/// order matches [`ReasoningLevel::ALL`] up to `xhigh`, which is what lets the compiler fold the
/// list into an ascending bitmask without sorting it.
/// The only level a dialect with no level field can put on the wire.
///
/// [`ReasoningLevel::None`] is present because it means "send no fragment at all", which is exactly
/// what such a dialect does — and because the compiler requires every dialect's list to contain it
/// so that an unaugmented request is always expressible.
const NO_REASONING_LEVELS: [ReasoningLevel; 1] = [ReasoningLevel::None];

const NAMED_EFFORT_LEVELS: [ReasoningLevel; 6] = [
    ReasoningLevel::None,
    ReasoningLevel::Minimal,
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
];

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
    /// `POST /v1/audio/translations`
    ///
    /// Translating speech into English text is a separate route from transcribing it in its own
    /// language: a provider may implement one without the other, and the distinction is what
    /// stops a transcription-only provider from being offered a translation request it will
    /// reject only after the audio has been uploaded.
    AudioTranslation = 11,
    /// `POST /v1/images/edits`
    ///
    /// Editing takes an existing image plus a prompt (and optionally a mask). It is separate from
    /// generation because the request body is multipart and the upstream may support one route
    /// without the other.
    ImagesEdits = 12,
    /// `POST /v1/images/variations`
    ///
    /// Variations take one image and produce alternatives. Providers that edit images do not
    /// necessarily offer variations, so the panel must be able to tell an operator which of the
    /// two a given upstream actually serves.
    ImagesVariations = 13,
    /// `POST /v1/moderations`
    Moderations = 14,
    /// The video job surface: `POST /v1/videos` to submit, `GET /v1/videos/{id}` for status and
    /// `GET /v1/videos/{id}/content` for the rendered bytes.
    ///
    /// One capability covers all three routes because a provider either accepts video jobs or it
    /// does not; polling and downloading a job it accepted are not separately optional.
    Video = 15,
    /// The Realtime API: the `GET /v1/realtime` websocket upgrade, which names its route in
    /// `?model=`, and the two WebRTC handshake POSTs, `POST /v1/realtime/client_secrets` and
    /// `POST /v1/realtime/calls`.
    ///
    /// The upgrade is proxied — `crates/manarouter-relay/src/realtime.rs` selects a provider that
    /// declares this capability and pumps frames between the client and it. It is not a
    /// [`RouteKind`] because it carries no JSON body and cannot be re-dispatched the way an HTTP
    /// request can, so it has a selection path of its own. The two POSTs are [`RouteKind`]s and go
    /// through the ordinary capability-addressed path.
    Realtime = 16,
    /// `/v1/vector_stores` and its files and search sub-resources.
    ///
    /// The relay answers this family itself, from its own store
    /// (`crates/manarouter-relay/src/vector_stores.rs`), so it has no [`RouteKind`] and never
    /// reaches a provider: retrieval is local and only the embedding call inside it is proxied. The
    /// capability is therefore a catalogue statement — "this upstream also hosts a vector-store
    /// family of its own" — and declaring it changes nothing on this relay's request path.
    VectorStores = 17,
    /// `/v1/containers` and its files sub-resource.
    ///
    /// A management surface like [`Capability::VectorStores`]: the relay refuses the family rather
    /// than pretending to run a sandbox, so the capability records whether an upstream offers one,
    /// and a request is proxied only where a step declares it.
    Containers = 18,
    /// `/v1/fine_tuning/jobs` and its events and checkpoints sub-resources.
    ///
    /// A management surface like [`Capability::VectorStores`], with one difference that matters: a
    /// provider that declares it really does run the training, and the relay stages the training
    /// file into that provider's own file namespace before it creates the job.
    FineTuning = 19,
    /// `/v1/evals` and its runs and output-items sub-resources.
    ///
    /// Served here, like [`Capability::VectorStores`]: an eval is a local record of requests this
    /// relay made, so there is nothing for a provider to declare and nothing to gate.
    Evals = 20,
    /// `POST /v1/search`
    ///
    /// Two shapes under one path: a body naming a `model` is proxied to a provider that hosts a
    /// search endpoint, and a body naming a vector store is answered locally by retrieval. It is
    /// OpenAI-only as a proxied route in this build: no other dialect here has a search surface, so
    /// declaring it on one is a configuration error rather than a guess the validator lets through.
    Search = 21,
}

impl Capability {
    /// A spelling for operator-facing diagnostics.
    ///
    /// This is **not** the configuration or panel wire form. Those go through `serde`, whose
    /// `kebab-case` rename produces `audio-transcription`, `audio-speech` and
    /// `images-generations`. The diagnostic spellings returned here are dotted where the route
    /// family has a dotted form and hyphenated otherwise; both spellings are accepted on input by
    /// [`Capability::parse`], but the serde form is what gets written to `config.json`.
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
            Self::AudioTranslation => "audio.translation",
            Self::ImagesEdits => "images.edits",
            Self::ImagesVariations => "images.variations",
            Self::Moderations => "moderations",
            Self::Video => "video",
            Self::Realtime => "realtime",
            Self::VectorStores => "vector-stores",
            Self::Containers => "containers",
            Self::FineTuning => "fine-tuning",
            Self::Evals => "evals",
            Self::Search => "search",
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
            "audio.translation" | "audio-translation" | "translation" => Self::AudioTranslation,
            "images.edits" | "images-edits" | "image-edit" => Self::ImagesEdits,
            "images.variations" | "images-variations" | "image-variation" => Self::ImagesVariations,
            "moderations" | "moderation" => Self::Moderations,
            "video" | "videos" | "video-generation" => Self::Video,
            "realtime" | "real-time" | "realtime-api" => Self::Realtime,
            "vector-stores" | "vector.stores" | "vector-store" => Self::VectorStores,
            "containers" | "container" => Self::Containers,
            "fine-tuning" | "fine.tuning" | "finetuning" => Self::FineTuning,
            "evals" | "eval" | "evaluations" => Self::Evals,
            "search" | "web-search" => Self::Search,
            _ => return None,
        })
    }

    /// The client routes this capability lets a provider serve.
    ///
    /// The inverse of [`RouteKind::capability`]. Most capabilities map to exactly one route; the
    /// video and realtime families map to three and two respectively because the capability is
    /// what a provider declares while the routes are what a client calls. Three kinds of
    /// capability return an empty slice: the model features that are not routes at all
    /// ([`Capability::ToolCalling`], [`Capability::Reasoning`], [`Capability::Vision`]), and the two
    /// families this relay serves itself from local state ([`Capability::VectorStores`],
    /// [`Capability::Evals`]).
    pub const fn routes(self) -> &'static [RouteKind] {
        match self {
            Self::Chat => &[RouteKind::ChatCompletions],
            Self::Responses => &[RouteKind::Responses],
            Self::Embeddings => &[RouteKind::Embeddings],
            Self::AudioTranscription => &[RouteKind::AudioTranscription],
            Self::AudioSpeech => &[RouteKind::AudioSpeech],
            Self::ImagesGenerations => &[RouteKind::ImagesGenerations],
            Self::Models => &[RouteKind::Models],
            Self::Messages => &[RouteKind::Messages, RouteKind::CountTokens],
            Self::AudioTranslation => &[RouteKind::AudioTranslation],
            Self::ImagesEdits => &[RouteKind::ImagesEdits],
            Self::ImagesVariations => &[RouteKind::ImagesVariations],
            Self::Moderations => &[RouteKind::Moderations],
            Self::Video => &[
                RouteKind::VideoCreate,
                RouteKind::VideoRetrieve,
                RouteKind::VideoContent,
            ],
            Self::Realtime => &[RouteKind::RealtimeClientSecrets, RouteKind::RealtimeCalls],
            Self::ToolCalling | Self::Reasoning | Self::Vision => &[],
            Self::VectorStores | Self::Containers | Self::FineTuning | Self::Evals => &[],
            Self::Search => &[RouteKind::Search],
        }
    }

    /// The capability that a given client route requires.
    ///
    /// Used by the relay to reject a route before dialling upstream, and by the panel to warn
    /// that a provider cannot serve a combo step. It delegates to [`RouteKind::capability`] so
    /// the two directions cannot disagree; [`Capability::routes`] is the readable inverse.
    pub const fn for_route(route: RouteKind) -> Self {
        route.capability()
    }

    /// The capabilities every provider is assumed to have unless configured otherwise.
    pub const BASELINE: [Capability; 1] = [Capability::Chat];

    /// Every capability, in discriminant order.
    ///
    /// The snapshot stores a capability as its discriminant cast to `u8`, so this array is also
    /// the tag order. Tests walk it instead of a hand-written list, which is what makes a new
    /// variant impossible to add without the tag decoder and the dialect matrix seeing it.
    pub const ALL: [Capability; 22] = [
        Self::Chat,
        Self::Responses,
        Self::Embeddings,
        Self::AudioTranscription,
        Self::AudioSpeech,
        Self::ImagesGenerations,
        Self::Models,
        Self::Messages,
        Self::ToolCalling,
        Self::Reasoning,
        Self::Vision,
        Self::AudioTranslation,
        Self::ImagesEdits,
        Self::ImagesVariations,
        Self::Moderations,
        Self::Video,
        Self::Realtime,
        Self::VectorStores,
        Self::Containers,
        Self::FineTuning,
        Self::Evals,
        Self::Search,
    ];
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
    /// `POST /v1/audio/translations`
    AudioTranslation = 9,
    /// `POST /v1/images/edits`
    ImagesEdits = 10,
    /// `POST /v1/images/variations`
    ImagesVariations = 11,
    /// `POST /v1/moderations`
    Moderations = 12,
    /// `POST /v1/videos`
    VideoCreate = 13,
    /// `GET /v1/videos/{id}`
    VideoRetrieve = 14,
    /// `GET /v1/videos/{id}/content`
    VideoContent = 15,
    /// `POST /v1/realtime/client_secrets`
    RealtimeClientSecrets = 16,
    /// `POST /v1/realtime/calls`
    RealtimeCalls = 17,
    /// `POST /v1/search`
    Search = 18,
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
            | Self::Models
            | Self::AudioTranslation
            | Self::ImagesEdits
            | Self::ImagesVariations
            | Self::Moderations
            | Self::VideoCreate
            | Self::VideoRetrieve
            | Self::VideoContent
            | Self::RealtimeClientSecrets
            | Self::RealtimeCalls
            | Self::Search => Dialect::OpenAi,
            Self::Messages | Self::CountTokens => Dialect::Anthropic,
        }
    }

    /// The capability a provider must declare to serve this route.
    ///
    /// This is the single source of truth for the route-to-capability direction: the older
    /// [`Capability::for_route`] spelling delegates here, and [`Capability::routes`] is the
    /// readable inverse. Keeping one match instead of two is what makes it impossible for the
    /// relay's admission check and the panel's warning to disagree about, say, whether a video
    /// status poll needs the video capability.
    pub const fn capability(self) -> Capability {
        match self {
            Self::ChatCompletions => Capability::Chat,
            Self::Responses => Capability::Responses,
            Self::Embeddings => Capability::Embeddings,
            Self::AudioTranscription => Capability::AudioTranscription,
            Self::AudioSpeech => Capability::AudioSpeech,
            Self::ImagesGenerations => Capability::ImagesGenerations,
            Self::Models => Capability::Models,
            Self::Messages | Self::CountTokens => Capability::Messages,
            Self::AudioTranslation => Capability::AudioTranslation,
            Self::ImagesEdits => Capability::ImagesEdits,
            Self::ImagesVariations => Capability::ImagesVariations,
            Self::Moderations => Capability::Moderations,
            Self::VideoCreate | Self::VideoRetrieve | Self::VideoContent => Capability::Video,
            Self::RealtimeClientSecrets | Self::RealtimeCalls => Capability::Realtime,
            Self::Search => Capability::Search,
        }
    }

    /// Whether this route carries a request body at all.
    ///
    /// `GET /v1/models` does not, and neither do the two video reads: retrieving a job and
    /// downloading its content are `GET`s with the id in the path. Skipping body handling for
    /// them matters because `/v1/videos/{id}/content` answers with video bytes, not JSON.
    #[inline]
    pub const fn has_request_body(self) -> bool {
        !matches!(
            self,
            Self::Models | Self::VideoRetrieve | Self::VideoContent
        )
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

    /// Whether a response to this route can be `text/event-stream` when the client asks to stream.
    ///
    /// The three generative text routes are the only ones: everything else answers with one JSON
    /// object (embeddings, models, moderations, a video job), audio bytes, image bytes or a video
    /// file, none of which a client reads as an event stream. This is a narrower set than
    /// [`Self::has_request_body`] and, today, the same set as [`Self::is_hedgeable`] — the two are
    /// kept separate because a route can be hedgeable without being streamable and vice versa, and
    /// a future route is likely to be one and not the other.
    #[inline]
    pub const fn is_sse_capable(self) -> bool {
        matches!(
            self,
            Self::ChatCompletions | Self::Responses | Self::Messages
        )
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
            Self::AudioTranslation => "audio.translations",
            Self::ImagesEdits => "images.edits",
            Self::ImagesVariations => "images.variations",
            Self::Moderations => "moderations",
            Self::VideoCreate => "video.create",
            Self::VideoRetrieve => "video.retrieve",
            Self::VideoContent => "video.content",
            Self::RealtimeClientSecrets => "realtime.client-secrets",
            Self::RealtimeCalls => "realtime.calls",
            Self::Search => "search",
        }
    }

    /// The configuration key for this route, as `route_overrides` is keyed.
    ///
    /// Kebab-case, matching this enum's serde representation: "chat-completions",
    /// "responses", "embeddings", "audio-transcription", "audio-speech",
    /// "images-generations", "models", "messages", "count-tokens". `as_str` is the
    /// display spelling and is dotted; this one is the configuration spelling.
    pub fn as_config_key(&self) -> &'static str {
        match self {
            Self::ChatCompletions => "chat-completions",
            Self::Responses => "responses",
            Self::Embeddings => "embeddings",
            Self::AudioTranscription => "audio-transcription",
            Self::AudioSpeech => "audio-speech",
            Self::ImagesGenerations => "images-generations",
            Self::Models => "models",
            Self::Messages => "messages",
            Self::CountTokens => "count-tokens",
            Self::AudioTranslation => "audio-translation",
            Self::ImagesEdits => "images-edits",
            Self::ImagesVariations => "images-variations",
            Self::Moderations => "moderations",
            Self::VideoCreate => "video-create",
            Self::VideoRetrieve => "video-retrieve",
            Self::VideoContent => "video-content",
            Self::RealtimeClientSecrets => "realtime-client-secrets",
            Self::RealtimeCalls => "realtime-calls",
            Self::Search => "search",
        }
    }

    /// Every route, for exhaustive tests and the panel's routing table.
    pub const ALL: [RouteKind; 19] = [
        Self::ChatCompletions,
        Self::Responses,
        Self::Embeddings,
        Self::AudioTranscription,
        Self::AudioSpeech,
        Self::ImagesGenerations,
        Self::Models,
        Self::Messages,
        Self::CountTokens,
        Self::AudioTranslation,
        Self::ImagesEdits,
        Self::ImagesVariations,
        Self::Moderations,
        Self::VideoCreate,
        Self::VideoRetrieve,
        Self::VideoContent,
        Self::RealtimeClientSecrets,
        Self::RealtimeCalls,
        Self::Search,
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
    /// Modal-style authentication, using `Modal-Key` and `Modal-Secret` headers.
    Modal,
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
        ///
        /// `#[serde(default)]` so that an empty platform is *representable*. Refusing it at
        /// deserialisation time turns one bad provider into a configuration that will not load at
        /// all — the panel and the relay both come up empty with a serde error, which is harder to
        /// repair than the provider is worth. Validation names the missing id instead, and the
        /// control plane refuses to save one.
        #[serde(default)]
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
    /// The operator's rule: **the winner is the highest-priority participant that passed.**
    ///
    /// It is also the cheapest of the four modes: the participant that passed the race is already in
    /// hand, so a combo that says nothing about `on_winner_fail` pays nothing extra for its
    /// recovery. `PickHighestPassedDifferentProvider` shares that zero-cost shape; `RerunExcluding`
    /// and `StableFallback` spend new upstream requests, which is why they are an explicit choice.
    fn default() -> Self {
        Self::PickHighestPassed
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

/// How much reasoning a client asked a model to spend, on one closed ladder.
///
/// The rule this type exists for: a client asks for `provider/model-level`, and a level the model
/// does not offer must **never** fail the request. The caller reroutes to the model's highest
/// available level instead. That rerouting lives with the router, not in this file; what lives here
/// is the ordered ladder that makes "highest available" a single comparison on
/// [`ReasoningLevel::rank`] rather than a per-vendor string table, and one spelling per level so a
/// request, a config file and a snapshot byte all agree.
///
/// The canonical spellings are lowercase and match what appears on the wire. `xhigh` is canonical
/// because that is what OmniRoute writes; the `x-high`/`extra-high` spellings are accepted aliases
/// in [`ReasoningLevel::parse`] because that is what a human writing a config by hand types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum ReasoningLevel {
    /// No reasoning at all: the model answers directly.
    None = 0,
    /// As little reasoning as the provider can express.
    Minimal = 1,
    /// A small, cheap reasoning budget.
    Low = 2,
    /// The provider's default for most models.
    Medium = 3,
    /// More reasoning than the default, for models that can spend it.
    High = 4,
    /// The tier above `high` on the models that offer one.
    ///
    /// Pinned with an explicit `rename`: the container's `kebab-case` would derive `x-high`, but
    /// the value that appears on the wire and in migrated OmniRoute data is `xhigh`.
    #[serde(rename = "xhigh")]
    XHigh = 5,
    /// The provider's top tier below a hard maximum (`max`).
    Ultra = 6,
    /// The model's absolute maximum.
    Max = 7,
}

impl ReasoningLevel {
    /// Every level, in ascending order, for the panel and exhaustive tests.
    pub const ALL: [ReasoningLevel; 8] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Ultra,
        Self::Max,
    ];

    /// Where the level sits on the ladder, so "highest available" is a comparison.
    ///
    /// This is deliberately the discriminant: the declaration order *is* the ladder, so ranking and
    /// the snapshot value cannot drift apart.
    #[inline]
    pub const fn rank(self) -> u8 {
        self as u8
    }

    /// The stable identifier used in config, the request and the panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Ultra => "ultra",
            Self::Max => "max",
        }
    }

    /// Parse a level. Case-insensitive, and accepts the human spellings of `xhigh`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase().replace(['_', ' '], "-");
        Some(match s.as_str() {
            "none" => Self::None,
            "minimal" => Self::Minimal,
            "low" => Self::Low,
            "medium" => Self::Medium,
            "high" => Self::High,
            // OmniRoute writes `xhigh`; a hand-written config is more likely to write the others.
            "xhigh" | "x-high" | "extra-high" | "extrahigh" => Self::XHigh,
            "ultra" => Self::Ultra,
            "max" => Self::Max,
            _ => return None,
        })
    }

    /// The `u8` stored in the snapshot.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a level from its snapshot `u8`, rejecting unknown values rather than guessing.
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::None,
            1 => Self::Minimal,
            2 => Self::Low,
            3 => Self::Medium,
            4 => Self::High,
            5 => Self::XHigh,
            6 => Self::Ultra,
            7 => Self::Max,
            _ => return None,
        })
    }
}

/// An input a model accepts, as a closed vocabulary.
///
/// Routing cares about the *kind* of content, not the MIME type: a request carrying an image or a
/// PDF may need a different model than one carrying only text, and the panel needs a fixed set to
/// render. `Pdf` is separate from `File` because providers that accept documents usually treat an
/// inline PDF specially, while `File` is the catch-all for every other attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[repr(u8)]
pub enum InputModality {
    /// Plain text, including markdown and code. Every chat model accepts this.
    Text = 0,
    /// Image input (vision).
    Image = 1,
    /// Audio input, such as a recorded prompt or a transcription.
    Audio = 2,
    /// Video input.
    Video = 3,
    /// A PDF document, usually inline base64 or a provider-side file id.
    Pdf = 4,
    /// Any other attachment the provider accepts.
    File = 5,
}

impl InputModality {
    /// Every modality, for the panel and exhaustive tests.
    pub const ALL: [InputModality; 6] = [
        Self::Text,
        Self::Image,
        Self::Audio,
        Self::Video,
        Self::Pdf,
        Self::File,
    ];

    /// The stable identifier used in config, the panel and on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Pdf => "pdf",
            Self::File => "file",
        }
    }

    /// Parse a modality. Case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().to_ascii_lowercase();
        Some(match s.as_str() {
            "text" => Self::Text,
            "image" => Self::Image,
            "audio" => Self::Audio,
            "video" => Self::Video,
            "pdf" => Self::Pdf,
            "file" => Self::File,
            _ => return None,
        })
    }

    /// The `u8` stored in the snapshot.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Recover a modality from its snapshot `u8`, rejecting unknown values rather than guessing.
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Text,
            1 => Self::Image,
            2 => Self::Audio,
            3 => Self::Video,
            4 => Self::Pdf,
            5 => Self::File,
            _ => return None,
        })
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
        // misread snapshot byte would otherwise route traffic to an unrelated provider. The value
        // after the last dialect is the one a new variant would take, so this line moves with the
        // enum: `ClaudeCli` is 11, and 12 is still unassigned.
        assert_eq!(Dialect::from_u8(13), None);
        assert_eq!(Dialect::from_u8(255), None);
    }

    #[test]
    fn only_like_dialects_are_passthrough() {
        for a in Dialect::ALL {
            for b in Dialect::ALL {
                let expected = matches!(
                    (a, b),
                    (Dialect::OpenAi, Dialect::OpenAi)
                        | (Dialect::OpenAi, Dialect::MistralVibe)
                        | (Dialect::MistralVibe, Dialect::MistralVibe)
                        | (Dialect::Anthropic, Dialect::Anthropic)
                );
                assert_eq!(a.is_passthrough_to(b), expected, "{a:?} -> {b:?}");
            }
        }
    }

    #[test]
    fn on_winner_fail_uses_the_internally_tagged_wire_form() {
        // The config file format for `on_winner_fail` is internally tagged on `mode`. This test
        // pins it, because the panel writes this field: `parseWinnerFail`/`buildWinnerFail` in
        // `panel/format.js` now emit and accept exactly this shape (and still *read* the legacy
        // externally tagged documents written before the fix).
        let rerun = WinnerFailMode::RerunExcluding { max_rounds: 3 };
        let json = serde_json::to_value(&rerun).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({ "mode": "rerun-excluding", "max_rounds": 3 })
        );
        assert_eq!(
            serde_json::from_value::<WinnerFailMode>(json).expect("deserialize"),
            rerun
        );

        let stable = WinnerFailMode::StableFallback { step: 2 };
        assert_eq!(
            serde_json::to_value(stable).expect("serialize"),
            serde_json::json!({ "mode": "stable-fallback", "step": 2 })
        );

        // The legacy externally tagged shape is not accepted by this type, and the panel's reader
        // is the only place that still understands it.
        assert!(serde_json::from_value::<WinnerFailMode>(
            serde_json::json!({ "rerun-excluding": { "max_rounds": 3 } })
        )
        .is_err());
    }

    #[test]
    fn the_default_winner_fail_mode_is_the_operator_s_rule() {
        // The operator's rule: the winner is the highest-priority participant that passed. A combo
        // that says nothing about `on_winner_fail` gets that rule, not the expensive one.
        assert_eq!(WinnerFailMode::default(), WinnerFailMode::PickHighestPassed);
        assert_eq!(WinnerFailMode::default().as_str(), "pick-highest-passed");

        // The other three remain accepted configuration options — they deserialize, they keep
        // their wire shape, and they are simply not implemented yet.
        for mode in [
            WinnerFailMode::RerunExcluding { max_rounds: 2 },
            WinnerFailMode::PickHighestPassed,
            WinnerFailMode::PickHighestPassedDifferentProvider,
            WinnerFailMode::StableFallback { step: 1 },
        ] {
            let json = serde_json::to_value(&mode).expect("serialize");
            assert_eq!(
                serde_json::from_value::<WinnerFailMode>(json).expect("deserialize"),
                mode
            );
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
        for route in [
            RouteKind::AudioTranslation,
            RouteKind::ImagesEdits,
            RouteKind::ImagesVariations,
            RouteKind::Moderations,
            RouteKind::VideoCreate,
            RouteKind::VideoRetrieve,
            RouteKind::VideoContent,
            RouteKind::RealtimeClientSecrets,
            RouteKind::RealtimeCalls,
            RouteKind::Search,
        ] {
            assert!(
                !route.is_hedgeable(),
                "new route {route:?} must not be hedged"
            );
        }
    }

    #[test]
    fn only_text_routes_can_answer_with_an_event_stream() {
        // `is_sse_capable` used to be "everything except the two body-less routes", which claimed
        // that an audio clip, an image, a video file and a moderation verdict could arrive as
        // `text/event-stream`. They cannot: each is one body of its own media type. The three text
        // routes are the only ones a client may stream, and the assertion is written as an
        // exhaustive walk so a new variant has to make a decision here.
        assert!(RouteKind::ChatCompletions.is_sse_capable());
        assert!(RouteKind::Responses.is_sse_capable());
        assert!(RouteKind::Messages.is_sse_capable());
        for route in RouteKind::ALL {
            if matches!(
                route,
                RouteKind::ChatCompletions | RouteKind::Responses | RouteKind::Messages
            ) {
                continue;
            }
            assert!(
                !route.is_sse_capable(),
                "{route:?} does not answer with an event stream, whatever the client asked for"
            );
        }
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
        // The two video reads carry the job id in the path and answer with JSON and video bytes
        // respectively; only the submit route has a body.
        for route in [RouteKind::VideoRetrieve, RouteKind::VideoContent] {
            assert!(!route.has_request_body(), "{route:?}");
        }
        for route in [
            RouteKind::AudioTranslation,
            RouteKind::ImagesEdits,
            RouteKind::ImagesVariations,
            RouteKind::Moderations,
            RouteKind::VideoCreate,
            RouteKind::RealtimeClientSecrets,
            RouteKind::RealtimeCalls,
            RouteKind::Search,
        ] {
            assert!(route.has_request_body(), "{route:?}");
        }
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

    #[test]
    fn command_code_go_serde_uses_the_renamed_identifier() {
        // The `kebab-case` rename must be verified, not assumed: an acronym-heavy variant is
        // exactly where the derived spelling and the intended one diverge.
        let json = serde_json::to_string(&Dialect::CommandCodeGo).unwrap();
        assert_eq!(json, "\"command-code-go\"");
        let back: Dialect = serde_json::from_str("\"command-code-go\"").unwrap();
        assert_eq!(back, Dialect::CommandCodeGo);
        // The pre-rename spellings are not accepted on input either, so a stale config fails
        // rather than silently selecting this dialect.
        assert!(serde_json::from_str::<Dialect>("\"commandcode\"").is_err());
        assert!(serde_json::from_str::<Dialect>("\"command-code\"").is_err());
        assert!(serde_json::from_str::<Dialect>("\"cmd\"").is_err());
    }

    #[test]
    fn command_code_go_parses_only_the_new_identifier() {
        assert_eq!(
            Dialect::parse("command-code-go"),
            Some(Dialect::CommandCodeGo)
        );
        assert_eq!(Dialect::parse("cmdgo"), Some(Dialect::CommandCodeGo));
        // Case-insensitive like every other dialect identifier.
        assert_eq!(
            Dialect::parse("Command-Code-Go"),
            Some(Dialect::CommandCodeGo)
        );
        assert_eq!(Dialect::parse("command-code"), None);
        assert_eq!(Dialect::parse("commandcode"), None);
        assert_eq!(Dialect::parse("cmd"), None);
    }

    #[test]
    fn cursor_parses_its_cli_spelling_and_offers_only_the_absent_level() {
        assert_eq!(Dialect::parse("cursor"), Some(Dialect::Cursor));
        assert_eq!(Dialect::parse("Cursor-Agent"), Some(Dialect::Cursor));
        assert_eq!(Dialect::parse("cursor-cli"), None);
        // The agent wire format has no effort field. Advertising another tier would make the
        // panel and `/v1/models` promise a level the upstream never receives.
        assert_eq!(
            Dialect::Cursor.expressible_reasoning_levels(),
            [ReasoningLevel::None]
        );
        for level in ReasoningLevel::ALL {
            assert_eq!(
                Dialect::Cursor.supports_reasoning_level(level),
                level == ReasoningLevel::None,
                "{level:?}"
            );
        }
        assert!(Dialect::Cursor.force_stream());
        assert!(!Dialect::Cursor.always_reasons());
    }

    #[test]
    fn every_dialect_has_its_own_stable_identifier() {
        let expected = [
            (Dialect::OpenAi, "openai"),
            (Dialect::Anthropic, "anthropic"),
            (Dialect::Google, "google"),
            (Dialect::Codex, "codex"),
            (Dialect::Copilot, "copilot"),
            (Dialect::Antigravity, "antigravity"),
            (Dialect::CommandCodeGo, "command-code-go"),
            (Dialect::GoogleAiStudio, "google-ai-studio"),
            (Dialect::ModalVllm, "modal-vllm"),
            (Dialect::Cursor, "cursor"),
            (Dialect::Zcode, "zcode"),
            (Dialect::ClaudeCli, "claude-cli"),
            (Dialect::MistralVibe, "mistral-vibe"),
        ];
        assert_eq!(expected.len(), Dialect::ALL.len());
        for (dialect, id) in expected {
            assert_eq!(dialect.as_str(), id, "{dialect:?}");
            assert_eq!(Dialect::parse(id), Some(dialect), "{id}");
        }
    }

    /// The two Gemini shapes are selected by *what the alias names*, and `google` keeps the
    /// permissive protocol shape.
    #[test]
    fn the_gemini_aliases_select_the_public_surface_not_the_protocol_shape() {
        assert_eq!(Dialect::parse("gemini"), Some(Dialect::GoogleAiStudio));
        assert_eq!(Dialect::parse("ai-studio"), Some(Dialect::GoogleAiStudio));
        assert_eq!(Dialect::parse("aistudio"), Some(Dialect::GoogleAiStudio));
        assert_eq!(
            Dialect::parse("AI Studio"),
            None,
            "aliases are exact, not fuzzy"
        );
        assert_eq!(Dialect::parse("google"), Some(Dialect::Google));
        assert_eq!(
            Dialect::parse("generativelanguage"),
            Some(Dialect::Google),
            "the host name names the protocol shape"
        );
    }

    /// A subscription token authenticates a request only when the body carries the CLI identity
    /// block, so the alias must not fall through to the API-key shape it resembles.
    #[test]
    fn the_claude_aliases_select_the_cli_surface_and_keep_the_api_one_apart() {
        assert_eq!(Dialect::parse("claude"), Some(Dialect::Anthropic));
        assert_eq!(Dialect::parse("messages"), Some(Dialect::Anthropic));
        assert_eq!(Dialect::parse("claude-cli"), Some(Dialect::ClaudeCli));
        assert_eq!(Dialect::parse("Claude-Code"), Some(Dialect::ClaudeCli));
        assert_eq!(Dialect::parse("claudecode"), Some(Dialect::ClaudeCli));
        assert_eq!(Dialect::parse("claudecli"), Some(Dialect::ClaudeCli));
        assert_eq!(
            Dialect::parse("claude code"),
            None,
            "aliases are exact, not fuzzy"
        );
        // The serde spelling the panel and config files use, which `as_str` also returns.
        let value = serde_json::to_value(Dialect::ClaudeCli).expect("serialize");
        assert_eq!(value, serde_json::json!("claude-cli"));
    }

    #[test]
    fn the_panel_offers_only_dialects_this_binary_deserialises() {
        // Cross-language guard. The panel writes the *serde* spelling, which is not always
        // `as_str()`: `OpenAi` is `open-ai` in a config file and `openai` to `parse`. A dropdown
        // entry the backend rejects turns "Add provider" into a save-time error, which is exactly
        // how the pre-rename `command-code` entry was caught.
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../panel/view-providers.js");
        let Ok(source) = std::fs::read_to_string(&path) else {
            return; // the panel is not part of a published crate checkout; nothing to pin
        };
        let line = source
            .lines()
            .find(|line| line.trim_start().starts_with("const DIALECTS ="))
            .expect("panel/view-providers.js declares `const DIALECTS`");
        let list = line.split_once('[').expect("DIALECTS is an array").1;
        let list = list.split_once(']').expect("DIALECTS is closed").0;
        let offered: Vec<&str> = list
            .split(',')
            .map(|entry| entry.trim().trim_matches('\''))
            .filter(|entry| !entry.is_empty())
            .collect();

        // Every offered spelling must deserialise, and to a dialect that exists.
        for spelling in &offered {
            let quoted = format!("\"{spelling}\"");
            let parsed: Dialect = serde_json::from_str(&quoted).unwrap_or_else(|e| {
                panic!("the panel offers `{spelling}`, which the config cannot deserialise: {e}")
            });
            assert!(Dialect::ALL.contains(&parsed), "{spelling} -> {parsed:?}");
        }
        // And every dialect must be reachable from the panel.
        for dialect in Dialect::ALL {
            let spelling = serde_json::to_value(dialect).unwrap();
            let spelling = spelling.as_str().unwrap().to_string();
            assert!(
                offered.contains(&spelling.as_str()),
                "{dialect:?} ({spelling}) is missing from the panel's DIALECTS"
            );
        }
        assert_eq!(
            offered.len(),
            Dialect::ALL.len(),
            "DIALECTS has a duplicate: {line}"
        );
    }

    #[test]
    fn reasoning_levels_are_ordered_from_none_to_max() {
        // The real ladder. Getting the top three out of order would silently reroute a request to
        // a weaker level, so the order is asserted directly rather than only through `rank`.
        assert!(ReasoningLevel::None < ReasoningLevel::Minimal);
        assert!(ReasoningLevel::Minimal < ReasoningLevel::Low);
        assert!(ReasoningLevel::Low < ReasoningLevel::Medium);
        assert!(ReasoningLevel::Medium < ReasoningLevel::High);
        assert!(ReasoningLevel::High < ReasoningLevel::XHigh);
        assert!(ReasoningLevel::XHigh < ReasoningLevel::Ultra);
        assert!(ReasoningLevel::Ultra < ReasoningLevel::Max);
        for (i, level) in ReasoningLevel::ALL.into_iter().enumerate() {
            assert_eq!(level.rank(), i as u8, "{level:?}");
            assert_eq!(level.as_u8(), level.rank(), "{level:?}");
            assert_eq!(ReasoningLevel::from_u8(level.as_u8()), Some(level));
        }
        assert_eq!(ReasoningLevel::from_u8(8), None);
        assert_eq!(ReasoningLevel::from_u8(255), None);
    }

    #[test]
    fn reasoning_level_aliases_and_roundtrips() {
        for level in ReasoningLevel::ALL {
            assert_eq!(ReasoningLevel::parse(level.as_str()), Some(level));
            assert_eq!(
                ReasoningLevel::parse(&level.as_str().to_ascii_uppercase()),
                Some(level)
            );
            assert_eq!(
                serde_json::from_str::<ReasoningLevel>(&format!("\"{}\"", level.as_str())).unwrap(),
                level
            );
            assert_eq!(
                serde_json::to_string(&level).unwrap(),
                format!("\"{}\"", level.as_str())
            );
        }
        // `xhigh` is canonical; humans write the other spellings.
        for alias in [
            "xhigh",
            "x-high",
            "extra-high",
            "extra_high",
            "extrahigh",
            "XHigh",
        ] {
            assert_eq!(
                ReasoningLevel::parse(alias),
                Some(ReasoningLevel::XHigh),
                "{alias}"
            );
        }
        assert_eq!(ReasoningLevel::parse("nope"), None);
        assert_eq!(ReasoningLevel::parse(""), None);
    }

    #[test]
    fn input_modalities_roundtrip() {
        for m in InputModality::ALL {
            assert_eq!(InputModality::parse(m.as_str()), Some(m), "{m:?}");
            assert_eq!(
                InputModality::parse(&m.as_str().to_ascii_uppercase()),
                Some(m)
            );
            assert_eq!(InputModality::from_u8(m.as_u8()), Some(m), "{m:?}");
            assert_eq!(
                serde_json::from_str::<InputModality>(&format!("\"{}\"", m.as_str())).unwrap(),
                m
            );
            assert_eq!(
                serde_json::to_string(&m).unwrap(),
                format!("\"{}\"", m.as_str())
            );
        }
        assert_eq!(InputModality::from_u8(6), None);
        assert_eq!(InputModality::parse("hologram"), None);
    }

    #[test]
    fn route_config_key_matches_the_serde_spelling_for_every_variant() {
        // `route_overrides` is keyed by the route's configuration spelling. If the serde rename
        // ever changes, this accessor must change with it — otherwise the two drift apart and a
        // configured override silently stops matching.
        for route in RouteKind::ALL {
            let serde_name = serde_json::to_value(route).expect("serialize route");
            assert_eq!(
                serde_name,
                serde_json::Value::String(route.as_config_key().to_string()),
                "{route:?}"
            );
        }
    }

    #[test]
    fn route_config_key_spellings_are_pinned() {
        // The literal spellings, so a wholesale rename of the serde attribute is caught here
        // rather than only showing up as a broken configuration file in the field.
        let expected = [
            (RouteKind::ChatCompletions, "chat-completions"),
            (RouteKind::Responses, "responses"),
            (RouteKind::Embeddings, "embeddings"),
            (RouteKind::AudioTranscription, "audio-transcription"),
            (RouteKind::AudioSpeech, "audio-speech"),
            (RouteKind::ImagesGenerations, "images-generations"),
            (RouteKind::Models, "models"),
            (RouteKind::Messages, "messages"),
            (RouteKind::CountTokens, "count-tokens"),
            (RouteKind::AudioTranslation, "audio-translation"),
            (RouteKind::ImagesEdits, "images-edits"),
            (RouteKind::ImagesVariations, "images-variations"),
            (RouteKind::Moderations, "moderations"),
            (RouteKind::VideoCreate, "video-create"),
            (RouteKind::VideoRetrieve, "video-retrieve"),
            (RouteKind::VideoContent, "video-content"),
            (RouteKind::RealtimeClientSecrets, "realtime-client-secrets"),
            (RouteKind::RealtimeCalls, "realtime-calls"),
            (RouteKind::Search, "search"),
        ];
        assert_eq!(RouteKind::ALL.len(), 19);
        assert_eq!(
            expected.len(),
            RouteKind::ALL.len(),
            "every variant must be listed"
        );
        for (route, key) in expected {
            assert_eq!(route.as_config_key(), key, "{route:?}");
        }
    }

    #[test]
    fn every_capability_round_trips_through_both_spellings() {
        // `ALL` is the exhaustive list, so this covers every variant rather than the handful a
        // hand-written test would name. A tag renumbering or a parser drift shows up here for the
        // variant that caused it.
        assert_eq!(Capability::ALL.len(), 22);
        for capability in Capability::ALL {
            assert_eq!(
                Capability::parse(capability.as_str()),
                Some(capability),
                "{capability:?}"
            );
            let wire = serde_json::to_value(capability).expect("serialize capability");
            let wire = wire.as_str().expect("capabilities serialise as strings");
            assert_eq!(
                Capability::parse(wire),
                Some(capability),
                "{capability:?} does not accept its serde spelling `{wire}`"
            );
            assert_eq!(
                serde_json::from_str::<Capability>(&format!("\"{wire}\"")).expect("deserialize"),
                capability
            );
        }
        // The append-only spellings are pinned literally: a rename here would silently invalidate
        // every provider entry and saved config that uses one.
        for (text, expected) in [
            ("audio.translation", Capability::AudioTranslation),
            ("audio-translation", Capability::AudioTranslation),
            ("images.edits", Capability::ImagesEdits),
            ("images-edits", Capability::ImagesEdits),
            ("images.variations", Capability::ImagesVariations),
            ("images-variations", Capability::ImagesVariations),
            ("moderations", Capability::Moderations),
            ("video", Capability::Video),
            ("realtime", Capability::Realtime),
            ("vector-stores", Capability::VectorStores),
            ("containers", Capability::Containers),
            ("fine-tuning", Capability::FineTuning),
            ("evals", Capability::Evals),
            ("search", Capability::Search),
        ] {
            assert_eq!(Capability::parse(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn routes_and_capabilities_agree_in_both_directions() {
        // The two directions are maintained for different callers — route -> capability for the
        // relay's admission check, capability -> routes for the catalogue — so they are walked
        // against each other instead of trusted. `ALL` keeps this exhaustive.
        for capability in Capability::ALL {
            for route in capability.routes() {
                assert_eq!(
                    Capability::for_route(*route),
                    capability,
                    "{route:?} claims to need {capability:?}"
                );
            }
        }
        for route in RouteKind::ALL {
            assert!(
                route.capability().routes().contains(&route),
                "{route:?} is missing from {:?}.routes()",
                route.capability()
            );
        }
        // Video and realtime are the only multi-route capabilities; pin their size so a route
        // added to either family later has to make a deliberate decision about the mapping.
        assert_eq!(Capability::Video.routes().len(), 3);
        assert_eq!(Capability::Realtime.routes().len(), 2);
    }

    #[test]
    fn a_named_effort_dialect_does_not_offer_a_tier_it_would_rewrite() {
        // `ultra` and `max` are not effort strings these APIs define; the compiler lowers both to
        // `xhigh`. Offering them would mean the panel and `/v1/models` advertise a tier the
        // upstream never sees, so they are excluded from the authoritative set.
        for dialect in [
            Dialect::OpenAi,
            Dialect::Copilot,
            Dialect::Codex,
            Dialect::CommandCodeGo,
        ] {
            let levels = dialect.expressible_reasoning_levels();
            assert!(!levels.contains(&ReasoningLevel::Ultra), "{dialect:?}");
            assert!(!levels.contains(&ReasoningLevel::Max), "{dialect:?}");
            assert!(levels.contains(&ReasoningLevel::Minimal), "{dialect:?}");
            assert!(levels.contains(&ReasoningLevel::XHigh), "{dialect:?}");
            assert!(!dialect.supports_reasoning_level(ReasoningLevel::Max));
        }
    }

    #[test]
    fn a_budget_dialect_can_express_every_tier() {
        // These dialects send a number, and the ladder has a budget for every tier, so nothing on
        // the ladder is unsendable even where the top budgets coincide.
        for dialect in [Dialect::Anthropic, Dialect::Google, Dialect::Antigravity] {
            assert_eq!(
                dialect.expressible_reasoning_levels(),
                &ReasoningLevel::ALL,
                "{dialect:?}"
            );
            for level in ReasoningLevel::ALL {
                assert!(
                    dialect.supports_reasoning_level(level),
                    "{dialect:?} {level:?}"
                );
            }
        }
    }

    #[test]
    fn every_dialects_level_list_is_ascending_and_unique() {
        for dialect in Dialect::ALL {
            let levels = dialect.expressible_reasoning_levels();
            assert!(!levels.is_empty(), "{dialect:?}");
            for pair in levels.windows(2) {
                assert!(
                    pair[0].rank() < pair[1].rank(),
                    "{dialect:?} is not ascending at {pair:?}"
                );
            }
            // Ascending and unique are the same assertion here because the ladder is total, so a
            // duplicate would have to be non-ascending. This pins the `const` slice order the
            // compiler relies on instead of sorting at run time.
            assert!(levels.contains(&ReasoningLevel::None), "{dialect:?}");
        }
    }
}
