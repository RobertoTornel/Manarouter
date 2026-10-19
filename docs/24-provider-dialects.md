# 24 — Provider dialects

A **dialect** is the shape of HTTP API an upstream speaks. This document says what each of the seven
is on the wire, which client routes can reach it, what the relay rewrites on the way in and on the
way back, and where the edges are. It is one level above the per-provider guides in
[providers/](providers/README.md): those describe a *vendor*, this describes a *protocol*.

---

## What a dialect is

A dialect is a shape of API, not a vendor. The comment on the enum says so directly: providers are
grouped by dialect because translation is only required when the client's dialect differs from the
upstream's, and two providers that share a dialect share every code path
(`crates/mr-core/src/dialect.rs:10-13`).

The set is closed and small — seven members (`dialect.rs:17-45`, and `Dialect::ALL` at
`dialect.rs:137-145`):

| `Dialect` | Canonical id (`Dialect::as_str`, `dialect.rs:78-88`) | Serde id (what a `config.json` or `catalog/providers.toml` entry carries) | Templates in `catalog/providers.toml` |
|---|---|---|---|
| `OpenAi` | `openai` | `open-ai` | 49 |
| `Anthropic` | `anthropic` | `anthropic` | 1 |
| `Google` | `google` | `google` | 1 |
| `Codex` | `codex` | `codex` | 1 |
| `Copilot` | `copilot` | `copilot` | 1 |
| `Antigravity` | `antigravity` | `antigravity` | 1 |
| `CommandCodeGo` | `command-code-go` | `command-code-go` | 1 |

Two spellings exist for a reason, and they differ for only one member. `as_str` is the permissive,
human-facing id; `serde`'s `kebab-case` rename is the strict wire form, and the two differ exactly
once — serde produces `open-ai` where `as_str` returns `openai` (`dialect.rs:69-77`). The panel's
list is pinned to the serde form by a cross-language test so the stricter of the two cannot be
forgotten.

`Dialect::parse` (`dialect.rs:97-111`) is the permissive parser used for hand-written config and for
migrated OmniRoute data: `openai`, `openai-compatible`, `oa` and `compatible` all mean `OpenAi`;
`claude` and `messages` mean `Anthropic`; `gemini` means `Google`; `chatgpt` means `Codex`;
`agy` and `cloudcode` mean `Antigravity`. `CommandCodeGo` is the deliberate exception: it accepts
exactly `command-code-go` and `cmdgo`. The pre-rename spellings (`commandcode`, `command-code`,
`cmd`) are **not** aliases, because accepting them would defeat the rename and hide stale
configuration — a file that still contains one fails loudly (`dialect.rs:36-44`, `dialect.rs:92-96`).

### A dialect is not a capability, and not a provider

A **capability** is a route, not a model feature: `chat`, `responses`, `embeddings`,
`audio-transcription`, `audio-speech`, `images-generations`, `models`, `messages`, `tool-calling`,
`reasoning`, `vision` (`dialect.rs:153-176`). Capabilities are declared per provider and per model
and are what the panel uses to grey out combinations that cannot work (`dialect.rs:148-149`). They
do **not** gate the relay's request path — nothing in the relay's selection or dispatch reads a
provider's capability list. What actually decides whether a request can be served is the dialect
pairing, and it refuses with a `400` that names both dialects rather than forwarding a body the
upstream will reject with a confusing error (`crates/mr-dialects/src/lib.rs:144-156`).

### The pivot

Translation is **hub-and-spoke with Chat Completions as the pivot** — the same convention both
reference routers use (`crates/mr-dialects/src/lib.rs:25-28`). The pivot representation is
`openai::PivotRequest`: the Chat Completions request object plus a `stream` flag
(`crates/mr-dialects/src/openai.rs:31-38`).

The pivot exists because Chat Completions has the most providers and the simplest content model, so
every provider needs at most one hop from a familiar shape rather than one bridge per pair
(`openai.rs:3-5`). But **pivoting twice loses information**, so a direct pair is preferred wherever
it can be done exactly (`lib.rs:25-28`): an Anthropic `thinking` block, a `cache_control`
breakpoint and a `redacted_thinking` block have no Chat Completions equivalent and a round trip
through the pivot destroys them (`openai.rs:9-13`). Anthropic-to-Anthropic traffic is therefore
forwarded verbatim and never reaches the pivot at all.

Three functions make up the whole interface (`lib.rs:171`, `lib.rs:233`, `lib.rs:277`):

| Function | Direction | Returns |
|---|---|---|
| `translate_request` | client body → upstream body | `Ok(None)` means *forward the original bytes* |
| `translate_response_body` | upstream body → client body | only ever called when the dialects differ |
| `translate_sse_data` | one SSE frame → zero or more client frames | same |

`Ok(None)` is the fast path and the common case: an OpenAI client talking to an OpenAI provider, or
an Anthropic client talking to an Anthropic provider, does no parsing at all
(`lib.rs:5-14`, `dialect.rs:113-124`). The relay forwards the original request bytes and pipes the
upstream response straight back.

---

## The seven dialects at a glance

| Dialect | Upstream path | Body passthrough possible | Always answers as SSE | Upstream shape |
|---|---|---|---|---|
| `open-ai` | `/v1/chat/completions` | yes, from an OpenAI client | no | OpenAI Chat Completions |
| `anthropic` | `/v1/messages` | yes, from an Anthropic client | no | Anthropic Messages |
| `google` | `models/{model}:generateContent` | no | no | Gemini `generateContent` |
| `codex` | `/responses` | no | yes | OpenAI Responses |
| `copilot` | `/chat/completions` | yes, from an OpenAI client | no | OpenAI Chat Completions, behind a Copilot bearer |
| `antigravity` | `v1internal:generateContent` | no | effectively yes | Gemini `generateContent` inside a Cloud Code envelope |
| `command-code-go` | `alpha/generate` | no | yes | Command Code CLI envelope and private event stream |

The path column is the path a request ends up at: the **dialect default** for the client's route,
unless the provider's `route_overrides` names one or a translator supplies one. Google and
Antigravity are always translator-named, because their path carries the model id or the Cloud Code
method. Two things about the defaults need care.

### How the upstream path is chosen

The upstream URL is `base_url` + a path, and the path is chosen in this order
(`crates/manarouter-relay/src/server.rs`, `upstream_path` and `build_url`):

1. the provider's `route_overrides` entry for the route the **client** called;
2. otherwise a path the **translator** supplied (`TranslatedRequest::path_override`);
3. otherwise the **dialect default** for that route (`default_path`).

The two sources do not use the same convention:

| Source | Form | Examples |
|---|---|---|
| Translator `path_override` | relative to the base URL | `messages`, `chat/completions`, `models/{model}:generateContent`, `alpha/generate` |
| Dialect default | absolute, version included | `/v1/chat/completions`, `/v1/messages`, `/v1/embeddings` |

The Anthropic translator states the translator's convention in a comment: "Anthropic's Messages
endpoint is not the OpenAI path, so the translator names it. The provider's base URL is expected to
already end in `/v1`" (`crates/mr-dialects/src/anthropic.rs:43-50`).

**The version segment appears exactly once.** The base URL decides whether there is one:

- a base that ends in a version segment (`/v1`, `/v4`, `/v1beta`) supplies it, and a path that
  carries a version segment of its own gives it up — so `https://api.openai.com/v1` plus
  `/v1/chat/completions` is `https://api.openai.com/v1/chat/completions`, not `…/v1/v1/…`;
- a base that carries none gets the dialect's version added to a translator-supplied path, so
  `https://api.deepseek.com` plus `chat/completions` is
  `https://api.deepseek.com/v1/chat/completions`;
- a path with no version at all and a base with no version at all is the only case where a version
  is genuinely absent, and the dialect's version is added there too.

So **a request that is translated and the same request passed through now reach the same upstream
at the same URL**, which is what the two conventions were always trying to say.

**An operator's path is used as written.** A `route_overrides` value is the operator's explicit
statement of where that provider serves that route; the relay never adds a version to it and never
replaces the version in it. The one thing it does is drop a version segment that is *exactly* the
base's, so a base ending in `/v1` and a value of `v1/responses` give `…/v1/responses` rather than
`…/v1/v1/responses`. A provider whose endpoint is versionless says so in its overrides: the
`deepseek` and `perplexity` entries both record `chat-completions = "chat/completions"`.

The key is the route the **client** called, in its kebab-case spelling (`chat-completions`,
`responses`, `embeddings`, `audio-transcription`, `audio-speech`, `images-generations`, `models`,
`messages`, `count-tokens`) — `RouteKind::as_config_key()`. A key of `/v1/chat/completions` never
matches, and the relay logs a warning naming the provider when it finds one.

### The default path is per dialect **and** per client route

`default_path` takes a `Dialect` and the client's `RouteKind`, so each OpenAI-shaped route reaches
the endpoint that belongs to it:

| Client route | OpenAI dialect default |
|---|---|
| `/v1/chat/completions` | `/v1/chat/completions` |
| `/v1/responses` | `/v1/responses` |
| `/v1/embeddings` | `/v1/embeddings` |
| `/v1/audio/transcriptions` | `/v1/audio/transcriptions` |
| `/v1/audio/speech` | `/v1/audio/speech` |
| `/v1/images/generations` | `/v1/images/generations` |

The Anthropic dialect has two: `/v1/messages` for the Messages route and
`/v1/messages/count_tokens` for the token-counting route. The other dialects have one each
(`/v1beta/models:generateContent`, `/v1internal:generateContent`, `/responses`, `/chat/completions`,
`/alpha/generate`), and Google and Antigravity are always translator-named, because their path
carries the model id or the Cloud Code method.

A provider that serves a route the convention does not name still has to say so in its
`route_overrides` — for example an OpenAI-compatible service whose responses endpoint is not
`/v1/responses`.

### `force_stream` is a dialect-level statement; the behaviour is per request

`Dialect::force_stream` returns true for `Codex` and `Antigravity` (`dialect.rs:126-134`), but the
flag the relay actually acts on is the one on the translated request (`lib.rs:99-103`). Codex forces
it unconditionally (`crates/mr-dialects/src/responses.rs:71-76`) and Command Code does too
(`crates/mr-dialects/src/commandcode.rs:80-83`); Antigravity sets it from the *request's* stream
intent and chooses `v1internal:streamGenerateContent?alt=sse` or `v1internal:generateContent` to
match (`crates/mr-dialects/src/google.rs:67-93`).

---

## `open-ai` — Chat Completions

**On the wire.** OpenAI's public API surface: `/v1/chat/completions`, `/v1/responses`,
`/v1/embeddings`, `/v1/audio/*`, `/v1/images/generations`, `/v1/models` (`dialect.rs:18-22`). This is
the baseline — by far the largest group of providers speak it, and 49 of the 55 shipped templates
use it.

**Who can reach it.** Every client route. The OpenAI dialect is the client dialect of every route
except the two Anthropic ones (`dialect.rs:277-288`), and an Anthropic client reaches it through
the direct `(Anthropic, OpenAi)` pair (`lib.rs:198-200`).

**What the relay does.** Nothing to the body. An OpenAI client body is forwarded verbatim
(`lib.rs:174`, `dialect.rs:119-124`); the response is piped back byte for byte. The only field the
relay touches on the passthrough path is `model`, and only when it differs from the resolved
upstream model — rewriting it would mean re-serialising a body the relay had no other reason to
touch (`crates/manarouter-relay/src/server.rs`, `apply_reasoning_level`).

**Quirks.**

- The Anthropic→OpenAI direction is a *direct* pair, not a pivot hop: `openai::from_anthropic_request`
  converts a Messages body into a Chat Completions body in one step (`openai.rs:93-99`). Four
  things actually differ — the system prompt is a top-level field rather than a message, content is
  a list of typed blocks rather than a string, `tool_use`/`tool_result` blocks become `tool_calls`
  and `tool` role messages, and `max_tokens` is mandatory upstream but optional here.
- That direct pair also names the upstream path — `chat/completions`, relative to the base URL
  (`openai.rs:366-378`) — while the OpenAI client reaching an OpenAI provider uses the dialect
  default `/v1/chat/completions`. See "Two path conventions" above.
- A field the translator does not understand is **preserved in the request** rather than dropped,
  wherever the upstream might accept it, and dropped only where forwarding it would cause a
  rejection. Silently discarding an unknown tool parameter or a `response_format` would produce a
  subtly wrong answer rather than a clear error, which is the worse failure (`openai.rs:15-22`).

**Read next.** [OpenAI-compatible providers](providers/openai-compatible.md) for the base-URL
convention and the auth kinds; [the catalogue](providers/catalog.md) for the 49 templates.

---

## `anthropic` — Anthropic Messages

**On the wire.** Anthropic's Messages API: `/v1/messages` and `/v1/messages/count_tokens`
(`dialect.rs:23-24`). The body is a Messages object: a top-level `system`, a `messages` array of
typed content blocks, `max_tokens` as a **mandatory** field, `tools`, `tool_choice`,
`stop_sequences`, `temperature`, `top_p`, `stream`, and a `thinking` block for extended reasoning.
The credential travels as `x-api-key`, and `anthropic-version` is required — the shipped template
pins `2023-06-01` (`catalog/providers.toml`).

**Who can reach it.** Both client dialects. An Anthropic client reaches it as a passthrough
(`lib.rs:174`); an OpenAI client reaches it through `anthropic::from_openai_request`
(`crates/mr-dialects/src/anthropic.rs:22-50`), which is the pairing a Codex CLI or Cursor client
needs when it is routed to a Claude model (`anthropic.rs:1-4`).

**What the relay does.** The request half is genuinely different work from the other direction and
lives in `anthropic.rs`; the response half is the exact inverse of what `openai.rs` already does and
is therefore delegated rather than reimplemented, because duplicating an inverse is how the two
halves drift apart (`anthropic.rs:5-9`).

**Quirks.** Anthropic is stricter than Chat Completions in three ways a naive translation gets
wrong (`anthropic.rs:11-13`):

- `max_tokens` is mandatory. There is no correct way to guess the client's intent, so a deliberately
  generous default is substituted and the value is always sent; getting this wrong is the most
  common cause of a hard 400 on this hop (`anthropic.rs:62-70`).
- `messages` must alternate user/assistant.
- The system prompt is a top-level field rather than a message.

A Chat Completions `stop` may be a bare string; Anthropic requires an array, so the translator
promotes it to `stop_sequences` and omits the field when the array would be empty
(`anthropic.rs:80-90`).

The translated hop names the path `messages`, relative to the base URL, with the comment that "the
provider's base URL is expected to already end in `/v1`" (`anthropic.rs:43-50`).

**Read next.** [Anthropic-compatible providers](providers/anthropic-compatible.md) for the native
`/v1/messages` argument and the block-versus-message difference.

---

## `google` — Gemini `generateContent`

**On the wire.** Google's `generativelanguage` API: `models/{model}:generateContent` and
`models/{model}:streamGenerateContent?alt=sse` (`dialect.rs:25-27`). The body is `contents` (an
array of turns with typed `parts`), an optional top-level `systemInstruction`, and a
`generationConfig` holding `temperature`, `topP`, `maxOutputTokens` and `stopSequences`
(`crates/mr-dialects/src/google.rs:461-499`).

**Who can reach it.** Both client dialects. `google::from_pivot_request` picks the path or the
`v1internal` envelope off the upstream dialect, so the Google and Antigravity pairings land on the
same translator (`lib.rs:202-211`).

**What the relay does.** The request half turns a Chat Completions body into Gemini's
`contents`/`parts` document and names the URL; the response half turns Gemini's candidates back
into pivot JSON (`google.rs:1-6`).

**Quirks.** Three shapes make this dialect different from every other one, and each is a place a
naive translation breaks (`google.rs:8-19`):

- **The model id is not in the body.** Gemini addresses models through the URL, so the model is
  written into `path_override` and the body carries no `model` field at all. The most common
  integration mistake is putting the id in the body and letting the URL fall back to a default
  model (`google.rs:48-55`).
- **A conversation is one array of turns with typed parts,** not alternating messages: the system
  prompt is hoisted to `systemInstruction`, assistant turns become `model` turns, and a tool result
  is a *user* turn holding a `functionResponse` keyed by function name — Gemini has never heard of
  `tool_call_id` (`google.rs:12-16`).
- **An empty `contents` array is illegal** (`Content.parts must not be empty`), so a request whose
  messages contribute nothing still emits one empty user turn (`google.rs:21-22`).

Two further rules are pinned by tests: malformed frames on the stream are skipped, never fatal — one
bad frame must not kill a response already half-delivered — and what is *emitted* is Gemini's
documented camelCase (`functionCall`, `usageMetadata`, …) while what is *read* accepts camelCase
first and snake_case as a fallback (`google.rs:21-28`).

**A caveat about the shipped template.** The only `google` template is `gemini`, and it authenticates
with `?key=…` (`catalog/providers.toml`). The relay explicitly does not apply query-parameter
authentication: it logs "query-parameter authentication is not applied; the credential will be
missing" and sends nothing (`crates/manarouter-relay/src/server.rs`, `apply_auth`). The same
catalogue note records that the key may also be sent as `x-goog-api-key`, which is the shape the
relay can send.

**Read next.** [The Antigravity guide](providers/antigravity.md) for the Cloud Code envelope that
wraps this shape, and [OAuth: Antigravity](12-oauth-antigravity.md) for the login.

---

## `codex` — the Responses API on a ChatGPT subscription

**On the wire.** OpenAI Codex on a ChatGPT subscription: the Responses API at
`chatgpt.com/backend-api/codex/responses`, SSE-first, `store` must be `false` (`dialect.rs:28-30`).
The shipped template's base URL is `https://chatgpt.com/backend-api/codex`, so the default path
`/responses` lands on the documented URL (`catalog/providers.toml`).

The body is a Responses object: a typed `input` item array rather than free-form `messages`, with
`instructions` as the top-level system slot (`crates/mr-dialects/src/responses.rs:7-9`). The
translator writes `instructions`, `input`, `tools`, `tool_choice`, `temperature`, `top_p`,
`parallel_tool_calls`, `max_output_tokens`, `reasoning`, `text` (from `response_format`) and
`prompt_cache_key` (`responses.rs:109-160`).

**Who can reach it.** Both client dialects (`lib.rs:217-220`).

**What the relay does.** Two client shapes arrive, and they are told apart by the body rather than
by the context (`responses.rs:43-53`):

- a **Chat Completions pivot** request (`messages`), which is a structured rewrite into
  `instructions` + `input`;
- a **Responses-native** client (`input`, `RouteKind::Responses`), whose body is already the
  upstream shape and only needs the model and the two always-on flags normalised.

Either way `stream` is forced on and `store` is forced off, in both directions, because Codex
streams an answer regardless and rejects `store: true` (`responses.rs:71-76`).

**Quirks.**

- The endpoint is SSE-first and the stream is a typed event protocol (`response.created`,
  `output_text.delta`, `response.completed`, …) rather than bare `chat.completion.chunk` payloads,
  so a response translation has to track cross-frame state: one role chunk, stable tool indices, one
  terminator (`responses.rs:10-14`).
- The terminator is where the two client shapes diverge: a Chat Completions client needs a final
  `finish_reason` chunk and `data: [DONE]`, while a Responses client terminates on
  `response.completed` and treats `[DONE]` as protocol noise — so `[DONE]` is suppressed for
  Responses routes and re-emitted only for Chat Completions clients (`responses.rs:16-19`).
- Fields with no Responses equivalent (`n`, `stop`, `stream_options`) are dropped rather than
  smuggled through, because the upstream rejects what it does not understand
  (`responses.rs:105-108`).
- Codex is the one dialect `dialect_default_levels` treats as reasoning-only: a model listed
  under Codex with no declared metadata is still seeded with level-qualified names
  (`crates/mr-core/src/snapshot.rs:1786-1805`, and the [API reference](18-api-reference.md#model-addressing)).

**Read next.** [Codex](providers/codex.md) for the OAuth flow, the fingerprint headers and the
`chatgpt-account-id` claim.

---

## `copilot` — GitHub Copilot's OpenAI-shaped surface

**On the wire.** `api.githubcopilot.com`, with the bearer minted from a GitHub token
(`dialect.rs:31-32`). The chat path is `/chat/completions` — note that it carries no `/v1` prefix,
unlike the `open-ai` dialect's default path.

**Who can reach it.** Both client dialects.

**What the relay does.** This is the one upstream that speaks a *foreign* shape but an OpenAI-shaped
body. GitHub Copilot exposes an OpenAI-shaped chat surface once the bearer is attached, so an
OpenAI client needs no body translation for it: `(OpenAi, Copilot)` is a passthrough
(`lib.rs:176-180`). An Anthropic client reaches it through the same direct Anthropic-to-OpenAI pair
that `(Anthropic, OpenAi)` uses, not a double pivot (`lib.rs:185-193`).

**Quirks.**

- The fingerprint headers are not optional. `Editor-Version` is mandatory — omitting it is a hard
  400 — and `Copilot-Integration-Id` selects the integrator allowlist that decides model
  availability. The bearer sent upstream is the Copilot token minted from the GitHub token, never
  the raw GitHub token (`catalog/providers.toml`, and [the Copilot guide](providers/copilot.md)). No
  amount of model discovery reveals a mandatory header, which is why they live in the catalogue data
  rather than in a comment ([the catalogue](providers/README.md)).
- The base URL can be overridden per credential: the relay uses the `copilot_api_host` from the
  OAuth token metadata when it is an `https://` URL without an `@`
  (`crates/manarouter-relay/src/server.rs`).
- A Copilot provider's `route_overrides` are ignored: the relay never consults the override table
  for this dialect, because the base host comes from the OAuth token and the provider has no
  upstream path of its own for an override to replace (`crates/manarouter-relay/src/server.rs`).

**Read next.** [Copilot](providers/copilot.md) for the header set and the token exchange.

---

## `antigravity` — Google Cloud Code Assist

**On the wire.** `cloudcode-pa.googleapis.com`, `v1internal:*`, with a project/tier onboarding
envelope (`dialect.rs:33-35`). The shipped base URL is `https://cloudcode-pa.googleapis.com`, so the
default path `v1internal:generateContent` lands on the documented URL.

**Who can reach it.** Both client dialects, through the same translator as `google`
(`lib.rs:202-211`).

**What the relay does.** Antigravity is the Gemini `generateContent` shape wrapped one level deeper
(`google.rs:16-19`). The request envelope is `{model, request, userAgent: "antigravity",
requestId}`, where `request` is the ordinary Gemini body (`google.rs:67-83`). The `project` field is
**not** client input: the relay adds it from the encrypted OAuth token metadata just before
dispatch, and refuses the request with a `503` if that metadata is missing or malformed
(`crates/manarouter-relay/src/server.rs`, `inject_antigravity_project`).

The answer comes back wrapped too: `{"response": {…the real generateContent response…},
"traceId": …}` instead of the bare object the public API returns, so responses are unwrapped before
anything is read (`google.rs:16-19`).

**Quirks.**

- **Antigravity is not a body passthrough**, even though its base URL is Google's. Its credential
  does not change the wire shape: it speaks Gemini `generateContent` wrapped in the envelope above,
  and its answers come back wrapped. It routes through the Google translator and is marked neither
  passthrough-able nor body-flag streaming (`lib.rs:176-186`).
- The project id is validated before it is sent: non-empty, at most 256 characters, and only ASCII
  alphanumerics, `-`, `_` and `.` (`crates/manarouter-relay/src/server.rs`).
- The Cloud Code onboarding, the tier model and the exact `v1internal` method set are recorded as
  reverse-engineered and unverified in the provider guide; the code does not invent behaviour it
  cannot check.

**Read next.** [Antigravity](providers/antigravity.md) for the two tiers, the onboarding envelope and
the scopes; [OAuth: Antigravity](12-oauth-antigravity.md) for the login.

---

## `command-code-go` — the Command Code CLI envelope

**On the wire.** `api.commandcode.ai/alpha/generate`, which returns its own event stream rather than
OpenAI SSE (`dialect.rs:36-38`). The whole body is replaced by a fixed envelope: `config`, `memory`,
`taste`, `skills`, `params` and `threadId` (`crates/mr-dialects/src/commandcode.rs:1-8`, and the
builder at `commandcode.rs:91-253`).

**Who can reach it.** Both client dialects (`lib.rs:212-215`).

**What the relay does.** The request half replaces the client's body with the envelope; the response
half reads the private event stream back and folds it into the client's shape. There is no
non-streaming response and no `[DONE]` sentinel on the wire, so the translator is the only thing
that knows where an answer ends (`commandcode.rs:3-8`).

**Quirks.**

- `params.stream` is always `true`, whatever the client asked for. A non-streaming client still
  opens an SSE connection upstream, and `to_pivot_response` exists to fold such a stream back into
  one JSON object (`commandcode.rs:12-15`, `commandcode.rs:80-83`).
- Tool declarations use `input_schema`, not `parameters`, and are **not** wrapped in
  `{type:"function", function:{…}}` — the upstream silently ignores a tool it cannot read
  (`commandcode.rs:16-18`).
- Errors are not OpenAI-shaped. They are `{"success":false,"error":{…}}`, both as an HTTP body and as
  an `error`-typed stream event, and are rewritten into the client's error shape
  (`commandcode.rs:18-19`).
- The stream is a private event protocol: `text-delta`, `tool-call`, `tool-use`, `tool-input-start`,
  `tool-delta`, `tool-input-delta`, `finish` and `error` (`commandcode.rs:461-611`).
- Headers are not the translator's business. The relay must add `authorization: Bearer <key>`,
  `accept: text/event-stream`, `x-command-code-version` and `x-cli-environment` itself; the
  constants live next to the translator so the two cannot drift apart
  (`commandcode.rs:20-25`, `commandcode.rs:38-47`, and the relay's `upstream_headers`). The version
  the relay sends is the constant `COMMAND_CODE_VERSION`, which replaces any value the provider
  template carries.
- The two fingerprint headers are *not* authentication; they identify the CLI. There is no
  negotiation — if the upstream stops accepting the version, the request fails
  ([the Command Code guide](providers/commandcode.md)).

**One asymmetry worth knowing.** The `(Anthropic, CommandCodeGo)` arm does **not** go through the
pivot: `commandcode::from_pivot_request` parses the client's body directly and builds the envelope
from it (`lib.rs:212-215`, `commandcode.rs:59-85`), unlike the Google and Codex arms, which call
`to_pivot_request` first. An Anthropic body's top-level `system` field and its `tool_use`/
`tool_result` blocks therefore do not survive that hop, because the envelope builder looks for a
Chat Completions `system` *message* and for `tool_calls` (`commandcode.rs:114-180`).

**Read next.** [Command Code](providers/commandcode.md) for the envelope fields and the bridge.

---

## Client route × upstream dialect

### The relay decides by dialect, not by route

`RouteKind::client_dialect` maps every client route onto exactly one client dialect
(`dialect.rs:277-288`), and the translation dispatch matches on `(client_dialect, upstream_dialect)`
and nothing else (`lib.rs:171-222`). The route appears in the error message and in the pivot's
context, nowhere else.

| Client route | `RouteKind` | Client dialect | Body shape |
|---|---|---|---|
| `POST /v1/chat/completions` | `ChatCompletions` | `open-ai` | chat |
| `POST /v1/responses` | `Responses` | `open-ai` | chat or Responses-native |
| `POST /v1/embeddings` | `Embeddings` | `open-ai` | embeddings |
| `POST /v1/audio/transcriptions` | `AudioTranscription` | `open-ai` | multipart |
| `POST /v1/audio/speech` | `AudioSpeech` | `open-ai` | audio |
| `POST /v1/images/generations` | `ImagesGenerations` | `open-ai` | images |
| `GET /v1/models` | `Models` | `open-ai` | none — served from the snapshot |
| `POST /v1/messages` | `Messages` | `anthropic` | Anthropic Messages |
| `POST /v1/messages/count_tokens` | `CountTokens` | `anthropic` | Anthropic Messages |

The route list is the one in the [API reference](18-api-reference.md); `RouteKind` itself is at
`dialect.rs:253-272`.

### Client dialect × upstream dialect

| Upstream dialect | From an `open-ai` client | From an `anthropic` client |
|---|---|---|
| `open-ai` | **Passthrough** — body forwarded verbatim | **Translated** — direct Anthropic→OpenAI pair |
| `anthropic` | **Translated** — direct OpenAI→Anthropic pair | **Passthrough** — body forwarded verbatim |
| `google` | **Translated** — pivot → Gemini | **Translated** — pivot → Gemini (two hops) |
| `codex` | **Translated** — pivot → Responses | **Translated** — pivot → Responses (two hops) |
| `copilot` | **Passthrough** — Copilot is OpenAI-shaped | **Translated** — direct Anthropic→OpenAI pair |
| `antigravity` | **Translated** — pivot → Gemini + Cloud Code envelope | **Translated** — pivot → Gemini + envelope (two hops) |
| `command-code-go` | **Translated** — Chat Completions body → CLI envelope | **Translated** — envelope built from the Anthropic body directly |
| anything else | **Refused** — `400`, both dialects named | **Refused** — `400`, both dialects named |

"Two hops" means the Anthropic body is first normalised into the Chat Completions pivot and then
into the target shape, so the Anthropic-only parts of the shape do not survive. "Refused" is the
`TranslateError::Unsupported` arm, which is the only arm without a match (`lib.rs:221-225`,
`lib.rs:144-156`).

### What the matrix does not tell you

**Only chat-shaped routes have a translatable body.** The dispatch is by dialect pair, so the relay
will attempt a translation for `/v1/embeddings` or `/v1/audio/transcriptions` against a non-OpenAI
upstream even though the body is not a chat body at all. The result is not a clean refusal: an
embeddings body has no `messages`, so it is translated as though it were a chat request with
nothing in it, and whatever the upstream makes of that is what the client gets. Treat the table as
a statement about *dialects*, and the route list above as a statement about *bodies*.

**`/v1/messages/count_tokens` has two answers, and the relay says which is which.** Only an
Anthropic-shaped upstream can count tokens — the endpoint exists in Anthropic's API and nowhere
else — so:

- against an **Anthropic** step the request is forwarded to the upstream's own
  `/v1/messages/count_tokens`, untranslated, and the upstream's answer is returned as it arrived;
- against **any other** step the upstream is **not called at all**. The relay answers locally with
  an estimate and marks it: `200 OK`, the body exactly `{"input_tokens": N}`, and the header
  `x-manarouter-count-tokens-estimate: true`.

The estimate is a byte-based approximation, not a tokenizer: the request body's length in bytes,
divided by four and rounded up, never less than one. It is the same four-bytes-per-token rule the
relay uses to size a quota reservation, and it counts the JSON shell as well as the text — a
percentage or two on a long prompt, several times the text-only count on a small or schema-heavy
body. It is deliberately coarse — an estimate presented as an authoritative count would be worse than
no endpoint at all.

Two consequences worth knowing. The estimate is produced **before an account is chosen**, so it
costs nothing: no credential is read, no upstream is billed, and the request is counted with zero
tokens. And inside a **hedged race** the estimate answers instantly, so a non-Anthropic participant
can win a race an Anthropic participant would have answered exactly — the race's own rule
(highest priority that replies inside the window) decides, and the header tells the client what it
got.

**A provider's declared capabilities do not narrow the matrix.** They are catalogue metadata for
the panel; the relay's gate is the dialect pairing above.

---

## Cross-dialect rules

### Passthrough

`Dialect::is_passthrough_to` is true for exactly two pairs: `open-ai → open-ai` and
`anthropic → anthropic` (`dialect.rs:113-124`). It is the single most latency-relevant predicate in
the project: when it returns true the relay never parses or re-serializes a body. Copilot adds a
third *practical* passthrough — an OpenAI client needs no body translation for it — but that is an
arm in the dispatch rather than a property of the enum (`lib.rs:176-180`).

`is_passthrough_to` is a **necessary** condition, not a sufficient one: a dialect can match while a
route still needs header or model-name rewriting, which the relay handles without touching the body
(`lib.rs:74-82`).

### Forced streaming

Three upstreams answer only in `text/event-stream`, whatever the client asked for: Codex, Command
Code, and — in practice — Antigravity's streaming method. The relay sends
`accept: text/event-stream` whenever the client asked for a stream or the translated request forces
it (`crates/manarouter-relay/src/server.rs`, `upstream_headers`); asking for JSON there would get a
JSON *error* instead. A non-streaming client is then served by folding the stream back into one
JSON document on the way out (`crates/manarouter-relay/src/server.rs`, `build_client_response`).

### Reasoning levels

The level ladder is a closed vocabulary — `none < minimal < low < medium < high < xhigh < ultra <
max` (`dialect.rs:829-940`, and the [API reference](18-api-reference.md#model-addressing)). A level
the model does not offer never fails the request: the caller reroutes to the model's highest
available level instead.

The fragment that carries the level is **per dialect**, and the relay merges it into the upstream
body *after* translation, so it lands on the upstream's own shape — an OpenAI client routed to
Gemini gets `generationConfig.thinkingConfig`, not a field Gemini never reads
(`crates/mr-core/src/snapshot.rs`, `dialect_reasoning_fragment`):

| Dialect | Fragment for a level above `none` |
|---|---|
| `open-ai`, `copilot`, `command-code-go` | `{"reasoning_effort": "<effort>"}` |
| `codex` | `{"reasoning": {"effort": "<effort>", "summary": "auto"}}` |
| `anthropic` | `{"thinking": {"type": "enabled", "budget_tokens": <n>}}` |
| `google`, `antigravity` | nothing here — the thinking config is a nested `generationConfig` the translator writes |

Level `none` emits **no reasoning field at all, on every dialect**: sending
`{"reasoning_effort":"none"}` to a provider that does not recognise the value is a 400, and omitting
the field is how every one of them expresses "do not reason"
(`crates/mr-core/src/snapshot.rs:1875-1880`). The merge is recursive, so the Google fragment is
shaped as deep as the field it has to reach, and an existing **scalar** under a key the fragment
names is left alone — which is what keeps a client's explicit `reasoning_effort` authoritative
(`crates/manarouter-relay/src/server.rs`, `merge_reasoning`).

### Modalities

`InputModality` is a closed vocabulary of six: `text`, `image`, `audio`, `video`, `pdf`, `file`
(`dialect.rs:943-965`). Text is always accepted: a model that could not take text is not a chat
model, and the fallback path depends on there being one type that always works
(`crates/mr-core/src/snapshot.rs:1807-1810`).

A request whose modality the selected route cannot carry is retried against the configured default
entry, as a complete reroute against the same pinned snapshot, with a budget of one fallback; a
target that also lacks the modality, or names no known model, fails with HTTP `501`
([the implementation status](23-implementation-status.md)). The modality is decided from the
*request body*, after translation, so the check sees the shape the upstream will actually receive.

### The model id

A dialect decides where the model id lives, and this is the single most common integration mistake:

- `open-ai`, `anthropic`, `codex` carry it in the body, in a `model` field.
- `google` carries it in the **URL** — `models/{model}:generateContent` — and the body carries no
  `model` field at all (`google.rs:48-55`).
- `antigravity` carries it in the **envelope**, alongside the request (`google.rs:70-74`).
- `command-code-go` carries it in the envelope's `params.model`.

The relay resolves the alias before translation and writes the resolved id into whichever field the
target dialect uses, so the id in the path and the id left in the body can never disagree — the
Gemini case is visible at `google.rs:59-62`, where the body is rewritten before the path is built
from the same value.

### Error philosophy

A field the translator does not understand is **preserved in the request** rather than dropped,
wherever the upstream might accept it, and dropped only where forwarding it would cause a rejection
(`openai.rs:15-22`). A pairing that is not implemented is refused outright, with a `400` naming
both dialects, rather than forwarding a body the upstream will reject with a confusing error
(`lib.rs:144-156`).

---

## Where to read more

| Question | Read |
|---|---|
| Which providers speak which dialect | [The catalogue](providers/catalog.md) — mirrors `catalog/providers.toml`, and a test checks it does |
| How a provider is added | [The provider catalogue](providers/README.md) |
| The dialect enum itself | `crates/mr-core/src/dialect.rs` |
| The translation dispatch and the pivot | `crates/mr-dialects/src/lib.rs` |
| The translators | `openai.rs`, `anthropic.rs`, `google.rs`, `responses.rs`, `commandcode.rs` in `crates/mr-dialects/src/` |
| The client-facing routes | [API reference](18-api-reference.md) |
| How a dialect becomes a route on the wire | [The routing snapshot](04-routing-snapshot.md) |
| The relay's overall shape | [Architecture](01-architecture.md) |
| Reasoning levels and model naming | [API reference](18-api-reference.md#model-addressing) |
| OAuth for the OAuth-only dialects | [OAuth overview](10-oauth-overview.md), [Codex](11-oauth-codex.md), [Antigravity](12-oauth-antigravity.md), [Copilot](13-oauth-copilot.md) |
