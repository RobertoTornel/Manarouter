# API reference

If you only need to call the API, [`docs/31-endpoints-in-use.md`](31-endpoints-in-use.md) lists the
endpoints this project actually uses, with a runnable request for each family. This document is the
full reference; the agent-facing contract is
[`docs/30-openai-compatible-api-spec.md`](30-openai-compatible-api-spec.md).

Two surfaces on two ports:

| Surface | Port | Purpose | Auth |
|---|---|---|---|
| **Relay** | 20900 | Client requests. Everything that touches a model. | A client key, `Authorization: Bearer` or `x-api-key` |
| **Control plane** (conductor) | 20901 | Admin API and the panel. Never on the request path. | A session cookie, or nothing when bound to loopback |

Both ports are configurable and both changes apply on **next restart**
([Web panel §ports](15-web-panel.md#ports-restart-and-the-amber-banner)).

---

## Part 1 — The relay

### Routes

Every path the relay mounts is listed here. A test asks each one of the mounted router and fails
if a documented path answers axum's empty 404, so a row in this table is a route that exists.

**Text and models.**

| Method | Path | Shape |
|---|---|---|
| `POST` | `/v1/chat/completions` | OpenAI Chat Completions |
| `POST` | `/v1/responses` | OpenAI Responses |
| `GET` | `/v1/responses/{id}` | Retrieve a stored response |
| `POST` | `/v1/responses/{id}/cancel` | Cancel a background response |
| `DELETE` | `/v1/responses/{id}` | Delete a stored response |
| `GET` | `/v1/responses/{id}/input_items` | The input items a stored response was built from |
| `POST` | `/v1/messages` | **Anthropic** Messages |
| `POST` | `/v1/messages/count_tokens` | **Anthropic** token counting — counted upstream for an Anthropic provider, estimated locally (and marked with `x-manarouter-count-tokens-estimate: true`) for any other |
| `GET` | `/v1/models` | OpenAI Models, served from the compiled snapshot |
| `GET` | `/v1/models/{model}` | One model, by the same name a request would use (the id may contain slashes, so it is a wildcard path) |
| `DELETE` | `/v1/models/{model}` | OpenAI's delete: local configuration is not a provider resource, so this removes nothing and says so |

**Modalities.**

| Method | Path | Shape |
|---|---|---|
| `POST` | `/v1/embeddings` | OpenAI Embeddings |
| `POST` | `/v1/moderations` | OpenAI Moderations |
| `POST` | `/v1/audio/speech` | OpenAI speech, returns audio |
| `POST` | `/v1/audio/transcriptions` | OpenAI transcription, multipart |
| `POST` | `/v1/audio/translations` | OpenAI translation, multipart |
| `GET` | `/v1/realtime` | OpenAI Realtime over a WebSocket: the socket is accepted and **pumped**, so frames cross the relay as they are rather than being buffered |
| `POST` | `/v1/realtime/client_secrets` | OpenAI Realtime ephemeral credentials |
| `POST` | `/v1/realtime/calls` | OpenAI Realtime calls, including the SDP offer form |
| `POST` | `/v1/images/generations` | OpenAI Images |
| `POST` | `/v1/images/edits` | OpenAI Images, multipart |
| `POST` | `/v1/images/variations` | OpenAI Images, multipart |
| `POST` | `/v1/videos` | OpenAI video creation |
| `GET` | `/v1/videos/{id}` | Video status |
| `GET` | `/v1/videos/{id}/content` | The rendered video bytes |

**Files, uploads and batches.**

| Method | Path | Shape |
|---|---|---|
| `POST` `GET` | `/v1/files` | OpenAI Files (upload, list) — the batch lane's input files and fine-tuning's training data |
| `GET` `DELETE` | `/v1/files/{id}` | OpenAI Files (retrieve, delete) |
| `GET` | `/v1/files/{id}/content` | The stored bytes |
| `POST` `GET` | `/v1/uploads` | OpenAI resumable uploads (create, list) |
| `POST` | `/v1/uploads/{id}/parts` | One part of a resumable upload |
| `POST` | `/v1/uploads/{id}/complete` | Assemble the parts into a file |
| `POST` | `/v1/uploads/{id}/cancel` | Abandon the upload |
| `POST` `GET` | `/v1/batches` | OpenAI Batches (submit, list) |
| `GET` | `/v1/batches/{id}` | OpenAI Batches (retrieve, with `usage`) |
| `POST` | `/v1/batches/{id}/cancel` | OpenAI Batches (cancel) |

**Retrieval.** Vector stores keep their contents in the relay's own state, and `/v1/search` answers
either from a vector store or by sending the query to a model, depending on what the body names.

| Method | Path | Shape |
|---|---|---|
| `POST` `GET` | `/v1/vector_stores` | Create, list |
| `POST` `GET` `DELETE` | `/v1/vector_stores/{id}` | Retrieve, modify, delete |
| `POST` `GET` | `/v1/vector_stores/{id}/files` | Attach a file, list the attached ones |
| `GET` `DELETE` | `/v1/vector_stores/{id}/files/{file_id}` | One attached file |
| `POST` `GET` | `/v1/vector_stores/{id}/file_batches` | Batch attachment (add, list) |
| `GET` | `/v1/vector_stores/{id}/file_batches/{batch_id}` | One attachment batch |
| `POST` | `/v1/vector_stores/{id}/search` | Search inside one store |
| `POST` | `/v1/search` | Search across stores, or a model-backed search when the body carries `model` |

**Jobs, evaluations and containers.**

| Method | Path | Shape |
|---|---|---|
| `POST` `GET` | `/v1/fine_tuning/jobs` | Create, list |
| `GET` `POST` | `/v1/fine_tuning/jobs/{id}` and `/{id}/cancel`, `/{id}/pause`, `/{id}/resume` | The job by id, and the three transitions |
| `GET` | `/v1/fine_tuning/jobs/{id}/events`, `/{id}/checkpoints` | The job's event log and its checkpoints |
| `POST` `GET` | `/v1/containers` | Create, list — **refused locally**, see below |
| `GET` `DELETE` | `/v1/containers/{id}` | Retrieve, delete |
| `POST` `GET` | `/v1/evals` | Create, list |
| `GET` `POST` `DELETE` | `/v1/evals/{id}` | Retrieve, update, delete |
| `POST` `GET` | `/v1/evals/{id}/runs` | Create, list runs |
| `GET` `POST` | `/v1/evals/{id}/runs/{run_id}` | One run, and its `cancel` |

Job creation needs an account with more than $15 of known credit, and takes the richest that
qualifies; the job's other routes are not gated. See
[key selection](05-key-selection.md#the-declared-balance-and-the-one-route-ranked-by-it).

**Liveness and control.**

| Method | Path | Shape |
|---|---|---|
| `GET` | `/healthz` | Liveness. **Never routed upstream.** |
| `GET` | `/control/probe` | Diagnostic probe of the routing table |
| `GET` | `/control/activity` | The last few combo decisions, for the panel's live view |

Three of these families need a sentence more than a row. A **container** is not something the relay
can host: it refuses `POST /v1/containers` with `501 unsupported_endpoint` and proxies the family only
when a configured provider declares the `containers` capability. **Vector stores, files, uploads,
batches, evaluations and stored responses** are served from local state, so their capabilities are
never consulted on the request path. **Every other route** is gated: the resolved step's provider must
declare the capability the route needs, and a provider that declares nothing but chat and models
answers `404 unsupported_endpoint` naming the missing capability. Small providers are the reason the
gate exists — `fish.audio` speaks audio and nothing else — and
[29-openai-surface.md](29-openai-surface.md) is the classification of all of it, family by family.

`/healthz` is deliberately not a model route. A health check that can be satisfied by a model
provider is a health check that reports the provider's health, not the router's — and when the
provider is down, the router looks down while it is working perfectly.

`GET /v1/models` is the one route that answers without touching a provider; what it lists is described
under [The `/v1/models` listing](#the-v1models-listing).

### Control routes (loopback only)

Two routes exist for the panel's own use and are not part of the API surface: they refuse any caller
that is not on loopback, because the instance may bind a LAN or tailnet interface and these skip the
client-key check. A local process already runs as the operator and can read the vault, so the loopback
check is the whole authorisation for them.

| Method | Path | Answers |
|---|---|---|
| `POST` | `/control/probe` | Runs one small completion for a model or combo through the ordinary dispatch path — routing table, dialect translation, account selection, cooldowns, fallbacks — and returns the ordinary OpenAI response. This is what "test this model" and the combo health check use, which is why they work for every dialect rather than only the OpenAI shape. It sends **no output cap**, so the model answers inside the budget a real client would get; the earlier fixed 32-token cap let a reasoning model spend its whole allowance on thoughts and return an empty answer. It also speaks **the surface the route declares**: chat completions when a step declares `chat`, the Responses surface when the steps declare only `responses` (Codex). Probing a responses-only provider through chat completions could only ever answer `unsupported_endpoint` at the capability gate, so the test failed without ever dialling a provider that works. |
| `GET` | `/control/activity` | The combo decisions this process has made, newest first, and the account cooldowns it currently holds. It is answered from memory, by the process that made the decisions, because only that process knows which step a combo committed to. |

The panel's "test" button goes through the conductor (`POST /api/models/test`), which asks this probe
and then insists on a non-empty answer: a response with no text is reported as a failure naming the
upstream `finish_reason` (or the Responses `status`), never as a pass. A Responses-shaped answer is
read from `output_text`, falling back to the `output` text parts. When the route refuses, the
conductor reports the upstream's own sentence from whichever envelope carried it — `error.message`,
`error.detail`, a top-level `detail` or `message`, or `error` as a string — so a provider whose
exhausted-balance refusal arrives as `{"detail": "…"}` is reported as exactly that, not as a generic
relay refusal. The conductor allows the probe two minutes — a reasoning
model can legitimately think for over a minute — and a failure to reach the relay is reported as a
timeout or a transport error, not as "the relay is not running" unless the connection was actually
refused.

`GET /control/activity` returns at most 24 entries and forgets them on restart, which is the honest
scope of a fact about in-flight work:

```json
{
  "entries": [
    {
      "combo": "expensivesmart",
      "requested": "expensivesmart",
      "step": "commandcode/deepseek/deepseek-v4.1-flash",
      "started_at": 1793707608231,
      "finished_at": null,
      "outcome": null
    }
  ],
  "cooldowns": {
    "mig-a2fefbe19a6cf4c643b545b613b2e701": {
      "until_ms": 1793707688231,
      "reason": "rate_limited",
      "failures": 1
    },
    "01a1bbd3-8335-7624-9eed-d8d9623086f3": {
      "until_ms": 1793707700000,
      "reason": "quota_exhausted",
      "failures": 2
    }
  }
}
```

`finished_at` and `outcome` are `null` while the client is still receiving the answer; the entry is
finished when the client-facing response body is dropped, which is when the relay has finished writing
or the client stopped reading. `outcome` is `ok` then, or `failed` when no step answered and the relay
gave up. Only a request whose model name resolved to a **combo** is recorded: a plain
`provider/model` request is not a combo decision.

`cooldowns` is the relay's own account-health map, expired-but-not-yet-cleared entries included, so
this view cannot disagree with the selection it is meant to explain. A key is the account id when the
provider's `cooldown_scope` is `account` (the default); a narrower or wider scope appears as a composed
key naming the model as well, and a provider-scoped key is shared by every account of that provider.
`reason` is one of `rate_limited`, `quota_exhausted`, `auth_failed`, `backoff`. No label, account
secret, request id or body appears anywhere in the document.

### Model addressing

A `model` field is a name, and a name can carry a **reasoning level** as a `-<level>` suffix
(`anthropic/claude-sonnet-4-5-high`, `coding-high`). Resolution order is compiled into the snapshot
and the **first match wins** (the rank numbers are the snapshot's own `RANK_*` constants):

| Rank | Form | Example |
|---|---|---|
| 0 | Alias | `fast` → the combo `coding` |
| 1 | Combo id | `01924f3a-...` |
| 2 | Combo name, with or without a level suffix | `coding`, `coding-high` |
| 3 | `provider/<exact model id>` | `anthropic/claude-sonnet-4-5` |
| 5 | `provider/<base model>-<level>` | `anthropic/claude-sonnet-4-5-high` |

Rank 4 is unused: the bare-model fallback is gone, so an unqualified id such as `gpt-5` is not an
address and answers 404.

A level-suffixed combo name shares rank 2 with the plain combo name, which is what makes
`coding-high` a way to pin an existing route to a tier without editing it. A rank-5 key, by contrast,
is derived by appending a suffix to a model id, so it is deliberately the **lowest** precedence there
is: an exact model id (rank 3) outranks it. Requesting a level is therefore
always possible, but a derived name can never shadow a model that genuinely exists under that name.

Two properties worth knowing before you debug a 404:

- **Alias chains are flattened at compile time, so the request path takes at most one hop.** An alias
  may point at another alias; the compiler follows the chain to its final target and stores that, and
  rejects a chain that loops with an `Error::Snapshot` naming the alias. Resolution stays a flat lookup
  rather than a graph walk, but a cycle is a compile-time error, not something the shape of the data
  makes impossible.
- **Matching is case-insensitive, and a hash collision is never trusted.** The snapshot indexes names
  by `fnv1a64` for speed; on a hit the candidate is compared with a full `eq_ignore_ascii_case` before
  use. Two names that collide cannot silently route to the wrong target.

#### An exact model id always beats a `base-level` reading

This is the rule most likely to surprise you, so it is worth stating on its own.
`antigravity/gemini-3.8-flash-low` is a **real model id**; it is not read as the model
`gemini-3.8-flash` at level `low`. The rank-3 entry for the exact id wins over the rank-5 entry
derived by stripping a suffix, and the derived entry is only consulted when no model by the longer
name exists. The corollary is that a provider's models named this way never get `-<level>` variants
listed for them: `...-low-high` would name nothing, because the exact-id rule wins again.

#### Levels

The canonical ladder, ascending, is:

```text
none < minimal < low < medium < high < xhigh < ultra < max
```

`xhigh` also accepts the spellings `x-high`, `extra-high`, `extra_high` and `extrahigh`, because that
is what a human writing a config by hand types; the canonical spelling is what appears on the wire and
in a snapshot.

**Which levels a model offers** is decided when the snapshot is compiled, in a fixed order: the
operator's explicit `reasoning_levels` on the model entry wins outright; otherwise the bundled table
`catalog/model_meta.json`, keyed `provider/model` and falling back to a provider-agnostic `model` key;
otherwise, when the model or its provider declares the `reasoning` capability — or the dialect serves
reasoning models only, currently `codex` — the dialect's own vocabulary
(`Dialect::expressible_reasoning_levels`). A model that reaches none of those has **no** levels: its
name gets no `-<level>` suffix, and it runs at `none`, which emits no reasoning field at all.

The vocabulary is per dialect, and it is the same list validation accepts: named-effort dialects
(`openai`, `codex`, `copilot`, `command-code-go`) can send `none, minimal, low, medium, high, xhigh`,
while the level-and-budget dialects (`anthropic`, `google`, `google-ai-studio`,
`antigravity`) express every tier, `ultra` and `max` included — on `google-ai-studio` the level
alone is what goes out, and its upper tiers share that surface's `high`. Any source — declared, catalogue or vocabulary — is filtered through that list, so the
listing never offers a tier the dialect would rewrite or drop. A configured level outside the list
is refused by the configuration validator with its name and the vocabulary quoted, so the panel
cannot save it and startup will not publish it; it is never silently lowered.

**An out-of-range level does not fail.** Asking for `-max` on a model that tops out at `xhigh` succeeds
and runs at that model's **highest available** level, so the request simply reasons less than it asked
for. The substitution is recorded (the relay logs it, and the panel shows it) rather than being
silently absorbed. A model that offers no levels at all reroutes to `none`.

**The default level** applies when a name carries no suffix: the model's own
`default_reasoning_level` when set, else `routing.default_reasoning_level` in the configuration, which
defaults to `high`.

Two combo nuances follow from the same rule:

- A combo named **without** a suffix runs each step at **that step's own default level**. Forcing the
  configuration default onto every step would override a per-model `default_reasoning_level` the
  operator set deliberately.
- A combo named **with** a suffix (`coding-high`) forces that tier onto every step, and a step whose
  model does not offer that tier falls back to that model's highest — the same substitution rule as a
  direct name.

#### What the level puts on the wire

The relay merges a fragment into the upstream body, per dialect. Level `none` emits **no reasoning
field at all, on every dialect** — sending `{"reasoning_effort":"none"}` to a provider that does not
recognise the value is a 400, and omitting the field is how every provider expresses "do not reason".

| Dialect | Fragment |
|---|---|
| openai, copilot, command-code-go | `{"reasoning_effort":"<level>"}` — `minimal`, `low`, `medium` and `high` as themselves; `xhigh`, `ultra` and `max` all normalised to `high` |
| codex (Responses) | `{"reasoning":{"effort":"<level>","summary":"auto"}}` — `xhigh` as `xhigh`; `ultra` and `max` normalised to `xhigh` |
| anthropic | `{"thinking":{"type":"enabled","budget_tokens":N}}` with `N` = 1024 for `minimal`/`low`, 10240 for `medium`, 131072 for `high` and above, then fitted (below) |
| google, antigravity | `{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"\|"high","thinkingBudget":N}}}`, where `thinkingLevel` is `high` from `medium` upward and `low` below it, and `N` is the budget above |
| google-ai-studio | `{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"\|"high"}}}` — the level alone, because the public API refuses `thinkingLevel` and `thinkingBudget` together and caps a budget at 65535 |

A per-model `reasoning_wire` map in the configuration **replaces the dialect default wholesale** for
the levels it names — a fragment has to be internally consistent, so merging one field-by-field is not
possible. `null` at a level means "send nothing at that level", which is a different statement from
"no opinion".

**A thinking budget is fitted, not asserted.** The budgets above are the table's ceiling for a
dialect, not a promise about any one model. Anthropic rejects `budget_tokens` at or above the
request's `max_tokens` (`400`, "`max_tokens` must be greater than `thinking.budget_tokens`"), and a
Gemini/Antigravity budget is spent out of the same candidate allowance as the answer, so a budget
larger than the allowance produces a successful response whose `content` is empty — the thinking
used the whole allowance and the upstream stopped with `MAX_TOKENS`. So the compiled fragment is
fitted twice: at compile time to the model's declared `max_output_tokens`, and at request time to the
`max_tokens` (or `max_completion_tokens`, or Gemini's `generationConfig.maxOutputTokens`) the body
actually states. Fitting means *lowering* to leave 4096 tokens of room for the answer; if that leaves
less than 1024 the fragment is dropped entirely rather than sent as a number the upstream would
refuse. A model that declares no output limit keeps the table's budget, because there is nothing to
fit it to.

Two more properties of the rewrite:

- **The relay only rewrites the body when it has to** — when there is a fragment to merge, or when the
  `model` field must be retargeted on the passthrough path. A model name resolved at its default level
  on a same-dialect provider is forwarded byte-for-byte, which is the whole latency argument. The same
  pass retargets `model` to the real upstream model id, because `...-high` is not a model the upstream
  has.
- **A client's own explicit reasoning field is never overwritten.** A client that sends
  `reasoning_effort` is being more specific than a level suffix can be, and quietly replacing it would
  make the same request mean different things depending on whether it arrived through a model name.

#### When a name matches nothing

The answer is a `404` whose message is chosen to be actionable, not a generic "no match" and certainly
not a silent fallback to "some model". The three cases need different fixes, so they read differently:

| What you sent | What the message says |
|---|---|
| A bare name that matches no combo or model | That a name with no provider prefix must be a combo or an alias, and that a model is addressed as `provider/model`, optionally with a `-<level>` suffix |
| An unknown provider prefix | The unknown provider, and the list of configured providers |
| A known provider and an unmatched model | That the provider exists but offers no matching model — it does not guess at a closest candidate |

On `/v1/messages` the same failure arrives in the Anthropic envelope with type `not_found_error`; see
[Error envelopes](#error-envelopes).

> **Status.** Bare model ids do **not** resolve: there is no bare-model pass in the index, rank 4 is
> unassigned, and a name like `gpt-5` fails with the actionable 404 above. Address a model as
> `provider/model`. The generated-alias migration (`generated: true` aliases are deleted on config
> load, with `config.json.pre-migration.bak` written as a backup) is written on that assumption, and
> it now matches the code.

### The `/v1/models` listing

`GET /v1/models` is served **entirely from the compiled snapshot**. No provider is contacted, so it
cannot be slow and cannot fail because a provider is down. It is the intended first check when a
request 404s, because it lists exactly the names that will resolve. It is authenticated by the same
rule as every other relay route (see [Authentication](#authentication)).

| Listed | Notes |
|---|---|
| Each combo name | `owned_by` is `manarouter`; the `manarouter` object carries `kind: "combo"`, the `strategy` and the `steps` count |
| Each combo name with a level suffix | One entry per level a step of that combo offers; the `manarouter` object additionally carries `reasoning_level` |
| `provider/model` for every visible model | `owned_by` is the provider id |
| `provider/model-<level>` | **Only** for a level that model genuinely offers, and never for a model whose own id already ends in a level word |

A model that advertises no reasoning levels is listed exactly once, under its own id — which is what
stops a client choosing `...-high` and discovering at request time that the suffix was part of the
model's name. The reverse is deliberate too: a level a model does **not** offer is not listed even
though requesting it succeeds by rerouting to that model's highest, because advertising it would be
advertising a tier that is not this model's.

Each provider entry's `manarouter` object carries:

| Field | Meaning |
|---|---|
| `kind` | `provider` |
| `dialect` | The API shape the provider speaks |
| `default_reasoning_level` | The level a request naming this model with no suffix gets |
| `reasoning_levels` | Array of the levels this model offers, ascending |
| `input_modalities` | Array of the input kinds this model accepts |

A level-qualified entry additionally carries `reasoning_level`. The response is the standard OpenAI
shape — `{"object":"list","data":[...]}` — with these `manarouter` objects alongside.

### Authentication

```http
Authorization: Bearer mr-xxxxxxxxxxxxxxxx
```

or, for Anthropic-shaped clients:

```http
x-api-key: mr-xxxxxxxxxxxxxxxx
```

Both are accepted on every route, because clients differ in which they send and refusing the "wrong"
one for a given path would be gratuitous.

| Situation | Response |
|---|---|
| A valid key | Proceed |
| An invalid key | `401` |
| No key, and **no keys are configured** | Proceed. An unconfigured router on loopback is a development setup, and requiring a key before it can be used at all would be hostile. |
| No key, and keys **are** configured | `401` |

Client keys are stored as SHA-256 hashes, and comparison is constant-time. The plaintext is shown
once at creation and is not recoverable.

### The 429 contract

Two different things produce a 429, and they mean different things:

| Source | How to tell | What it means |
|---|---|---|
| **The relay** | The body is the router's error envelope, and it names every exhausted step | Every account for every step is cooling down. The request never reached a provider. |
| **A provider, forwarded** | The body is the provider's own, passed through | That provider is throttling. Another step may still be usable. |

The relay's own 429 includes a `Retry-After` derived from the **earliest** known reset across the
steps it tried, so a client that honours it will not come back before something can answer.

### Streaming

`stream: true` returns `text/event-stream`.

```
data: {"id":"...","choices":[{"delta":{"content":"Hello"}}]}

: keepalive

data: {"id":"...","choices":[{"delta":{"content":" world"}}]}

data: [DONE]
```

- **Keepalive comments** (`: keepalive`) from an upstream are passed through unchanged. The relay does
  not generate its own, so a long gap in a stream it is translating is a long gap on the wire.
- **`data: [DONE]`** terminates a completed translated stream. It is not appended after an error: a
  stream that breaks mid-body simply ends. A Gemini stream that reaches a clean end of body without
  a `finishReason` is completed with a synthesized `finish_reason: "stop"` first, because a strict
  Chat client rejects an unterminated stream even when all the text arrived.
- **`manarouter.retry`** is the router-specific event emitted when the winning model failed
  mid-stream, bytes were already on the wire, and the combo's policy is `restart`:

  ```
  event: manarouter.retry
  data: {"reason":"upstream_failed","from":"anthropic/claude-sonnet-4-5","to":"openai/gpt-5"}
  ```

  It is followed by a terminator in the client's dialect — `data: {"choices":[{"delta":{},
  "finish_reason":"stop"}]}` for an OpenAI-shaped client, `event: message_stop` with
  `data: {"type":"message_stop"}` for an Anthropic-shaped one — and then the replacement model's
  frames.

  **A client that ignores unknown SSE events — as the specification requires — still receives a
  valid, terminated response.** The retry event is informational. This is a compatibility
  requirement, not a nicety: any client that broke on an unknown event would break on the first
  provider that added one.

### Mid-stream failure

> **Status.** Implemented. The relay wraps the committed winner's body, so it sees the failure, and
> the combo's `midstream_policy` decides what happens. The replacement is a participant that
> **passed** the race's eligibility gate — nothing is re-dispatched.

Once bytes have reached the client, the router cannot pretend the request did not happen.
`midstream_policy` decides ([Combos §midstream policy](06-combos.md#7-midstream_policy)):

| Policy | What the client sees |
|---|---|
| `restart` (default) | The `manarouter.retry` boundary, then the replacement model's frames |
| `terminate` | The partial stream, ended with an error |

**Hard guard under both:** once any `tool_calls` delta has been emitted, the relay refuses to splice.
A tool call is a structured object the client is already assembling; half of one model's call plus
half of another's produces a call that is either invalid or, worse, valid and wrong. A truncated
stream is the honest outcome.

Before anything has reached the client, both policies switch silently: the client never saw the dead
model's answer, so there is nothing to un-send and nothing to announce.

### The Anthropic surface

`POST /v1/messages` accepts the Anthropic Messages shape and returns it. Differences that matter:

- **`max_tokens` is required by Anthropic.** A `/v1/messages` request that omits it and is translated
  to an OpenAI-shaped provider has the field omitted rather than invented; passed through to an
  Anthropic-shaped provider, the upstream's own required-field error comes back. The **8192** default
  applies on the *opposite* hop: an OpenAI Chat Completions client that omits `max_tokens` while
  targeting an Anthropic-shaped provider gets `max_tokens: 8192` (`DEFAULT_MAX_TOKENS`), because
  Anthropic rejects the request without the field. Never silently unlimited — a missing `max_tokens`
  that becomes an unbounded request is how a client discovers its bill.
- **`anthropic-version`** is accepted and forwarded where the provider is Anthropic-shaped.
- **Content blocks** (`tool_use`, `tool_result`, `image`) are translated rather than passed through
  when the target is an OpenAI-shaped provider, and passed through verbatim when it is not.
- **The error envelope differs.** See below.

### The batch lane

The routes above answer one request with one answer. `/v1/files` and `/v1/batches` are the second
lane: submit a file of prompts, get an id back, poll it, read the results in order. The OpenAI shapes
are used so that a client already written for OpenAI's batch API works unchanged; the lane behind them
is a durable queue with per-item isolation, retry of transient failures only, and resume from disk
after a restart. The full design is in [25-gpu-rentals.md](25-gpu-rentals.md) §2, §10, §11 and §12; this
section is the wire contract.

**Upload.** `POST /v1/files` accepts `multipart/form-data` (`purpose=batch`, `file=@input.jsonl`) and,
for a shell client that would rather not build an envelope, a raw body with `?filename=…&purpose=batch`
in the query string. `purpose` is `batch` or `fine-tune`, or absent, which means `batch`; any other
value is refused with the purpose it got. A `fine-tune` file is stored exactly like a batch file and
is staged to the provider by the fine-tuning route below. The filename is metadata and never becomes
a path. The stored bytes are returned by `GET /v1/files/{id}/content`, which is how an operator checks
what the lane actually received — and the cap is 32 MiB per file, the lane's own limit rather than
OpenAI's 512 MB, because a file is held in memory while it is validated and written.

**Submit.** `POST /v1/batches` takes `input_file_id`, `endpoint` (`/v1/chat/completions` is
the only one this lane runs; any other is refused with `unsupported_endpoint`), `completion_window` (`24h` is the only accepted value, and it is
not enforced — see below), optional `metadata`, and the optional `max_cost_usd` extension. It validates the file, reads
every line as an independent request body, writes the job, and returns with status `validating`. Each
input line is the usual OpenAI shape:

```json
{"custom_id": "item-1", "method": "POST", "url": "/v1/chat/completions",
 "body": {"model": "modal-vllm/qwen", "messages": [{"role": "user", "content": "…"}]}}
```

`custom_id` is optional and, when present, comes back on the matching result line. Results are written
in **input order**, not completion order, because a batch's consumer joins on position.

**Status.** The set is OpenAI's: `validating`, `in_progress`, `finalizing`, `completed`, `failed`,
`cancelled`, `expired`, `cancelling`. `request_counts` carries `total`, `completed` and `failed`. A batch
whose items all failed still finishes `completed` — the batch did its job, and the failures are in the
error file, which is what the error file is for. `expires_at` is reported as `created_at + 24h` and
nothing enforces it: an item waits for admission at most `ADMISSION_WAIT` and every attempt has its own
upstream timeout, so a batch always ends, but a stalled batch does not become `expired`. The status
exists because the API shape needs it, not because a timer writes it.

**Results and errors.** Two files, both readable through `/v1/files/{id}/content`: `output_file_id`
holds one JSONL line per successful item, `error_file_id` one per failed item with the provider's code
and message (never a credential, never the request body). An item that never ran appears in neither —
see the cap below.

**The cost report.** The batch object carries a `usage` block, an extension to OpenAI's schema that a
client which ignores unknown fields is unaffected by. It is a computation over the GPU-second ledger,
never over token counts: `gpu_seconds` (wall clock inside item attempts, cold-start and admission waits
included), `cold_starts`, and then the money — `cost_usd` (`gpu_seconds × price_per_second`),
`boot_cost_usd` (`cold_starts × boot_seconds × price`, kept separate because the boot fee is the price
of the card being cold and is not proportional to anything the batch did), `boot_share_pct`, and
`cold_start_verdict` (`boot_dominates` above half the bill, `work_dominates` below, `unknown` when there
is no price to divide). `usd_per_million_output_tokens` is the whole bill over the output tokens the
provider counted; `serving_usd_per_million_output_tokens` is the same with the boot fee removed, which
is the figure a card-to-card comparison needs. Every cost field is `null` rather than estimated when the
provider declares no `price_per_second`, and `card` is non-null only when the declared price matches a
measured profile within two cents. `provider` and `model` name the settled items' endpoint, or `null`
when they disagreed or when the route never resolved to one endpoint (a combo, an unknown model, or a
provider the planner does not price).

**A ceiling.** `max_cost_usd` is an optional extension: dollars, strictly positive, at most 1000. It is
checked against money already settled **before each item is dispatched**, so it stops the queue rather
than killing a request — an in-flight item is never interrupted, because that would waste the GPU
seconds already billed for it and produce neither a result nor an error. Items that never ran are
retired with no result file, exactly as a cancellation retires them: not failures, since nothing about
them failed. The batch ends `cancelled` (OpenAI has no "stopped early" status and inventing one would
break clients that switch on the documented set) with `budget_exceeded: true` and `stopped_reason:
"max_cost_usd_exceeded"` — or `"cancelled"` when an operator cancelled it — so the reason is never
inferred from the status. `max_cost_usd` is echoed even when it is `null`, so a client polling a batch
it did not submit can see that nothing was capped. An endpoint with no declared price adds `0.0` to the
ledger, which makes a capped batch able to overspend by the unpriced share but never to under-report.

**Cancellation.** `POST /v1/batches/{id}/cancel` moves a batch to `cancelling` and then `cancelled`:
items not yet dispatched are retired, items in flight finish. It is the same machinery the cap uses,
which is why the two share a terminal status.

**This lane is not the deployed endpoint's batching.** A deployment may itself be applied in `batch`
mode with a fill policy — `full` makes its batch wait until it holds `batch.size` items, `timeout`
ships a short batch after `batch_timeout_secs` — and that is recorded with the deployment
([25-gpu-rentals.md](25-gpu-rentals.md) §2). The relay lane described in this section is a different
thing: it dispatches every item through the ordinary request path and never waits for a batch to fill.
Confusing the two is the usual reason someone expects a submitted file to sit still until more lines
arrive; it will not.

**Priority.** A batch and an interactive request share the endpoint, and the lane has no priority
class: an interactive request can be delayed by an item already in flight, but never by one that is
waiting — an item holds no KV reservation between attempts. The knob is `MANAROUTER_BATCH_WORKERS`
(default 4, clamped to 1–64): set it to `1` and a batch occupies one slot so interactive traffic
always finds room. It is not a pre-emption mechanism, because cancelling an in-flight generation is
billed the same as finishing it — the relay does not do that to save a queue position.

**What the lane deliberately does not do.** It does not re-apply per-client-key quota to individual
items. A batch is authorized once, as a whole, when it is submitted, so an operator who meters client
keys should treat one batch as one metered submission rather than N metered calls. It does not price
tokens (see the cost report above), it does not deploy anything — a rented GPU is woken and torn down
by the operator, per [25-gpu-rentals.md](25-gpu-rentals.md) §1 — and it does not read a deployment's
`mode` or fill policy, because those describe the endpoint rather than the queue.

### Error envelopes

**OpenAI-shaped** (routes 1–7, and any translated target):

```json
{ "error": { "message": "...", "type": "invalid_request_error", "code": "model_not_found", "param": "model" } }
```

**Anthropic-shaped** (`/v1/messages`, `/v1/messages/count_tokens`):

```json
{ "type": "error", "error": { "type": "not_found_error", "message": "..." } }
```

A client debugging a `/v1/messages` 404 should not expect OpenAI's fields. The shim answers in the
dialect it was asked in.

### Worked example

```bash
# 1. A provider, prefilled from the catalogue.
mr provider add anthropic --from-catalog anthropic

# 2. An account. The key comes from stdin, never argv -- a command line is visible in `ps`.
printf '%s' "$ANTHROPIC_API_KEY" | mr account add anthropic --label main --secret-stdin

# 3. Discover its models.
mr models sync anthropic

# 4. A combo with a fallback.
mr combo add coding \
  --step anthropic/claude-sonnet-4-5 \
  --step openai/gpt-5

# 5. A client key. Printed once; copy it now.
mr key create --label laptop --print

# 6. Run it.
mr serve
```

```bash
curl -sS http://127.0.0.1:20900/v1/chat/completions \
  -H 'Authorization: Bearer mr-...' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "coding",
    "messages": [{"role": "user", "content": "Say hi in five words."}],
    "stream": false
  }'
```

```bash
# The Anthropic shape, against the same combo.
curl -sS http://127.0.0.1:20900/v1/messages \
  -H 'x-api-key: mr-...' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "coding",
    "max_tokens": 64,
    "messages": [{"role": "user", "content": "Say hi in five words."}]
  }'
```

```bash
# Pin the tier: the same combo at `high`. Asking for a tier a step's model does not
# offer is not an error -- that step falls back to its own highest.
curl -sS http://127.0.0.1:20900/v1/chat/completions \
  -H 'Authorization: Bearer mr-...' \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "coding-high",
    "messages": [{"role": "user", "content": "Say hi in five words."}]
  }'
```

```bash
# What this instance will actually answer to. No provider is contacted.
curl -sS http://127.0.0.1:20900/v1/models \
  -H 'Authorization: Bearer mr-...'
```

---

## Part 2 — The control plane

Base `http://127.0.0.1:20901/api`. **A `GET` never mutates.** Every mutation requires a valid session
**and** an acceptable `Origin` header, checked before the handler runs — a cross-origin form post from
a page the operator is merely visiting is the realistic attack on a loopback admin interface.

### Session

| Method | Path | Body | Notes |
|---|---|---|---|
| `GET` | `/api/health` | — | Unauthenticated. Version and readiness only. |
| `GET` | `/api/session` | — | Whether the caller has a session, and whether a password is required. |
| `POST` | `/api/login` | `{"password":"..."}` | Sets the session cookie. |
| `POST` | `/api/logout` | — | Revokes this session. |
| `POST` | `/api/password` | `{"current":"...","new":"..."}` | **Revokes every session**, including this one. |

### Configuration

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/config` | Redacted: no credential values, ever. |
| `PUT` | `/api/config` | Validate → save atomically → recompile the snapshot → return the new digest. |
| `GET` | `/api/config/validate` | Returns **every** problem, not the first. |
| `GET` `PUT` | `/api/config/raw` | The file as text, for the editor view. Credentials are ids, so nothing is exposed. |
| `GET` | `/api/ports` | Current and pending ports. |
| `PUT` | `/api/ports` | Persists a change. **Applies on next restart.** |
| `POST` | `/api/restart` | Performs it. Drops in-flight connections, which is why it is explicit. |

The API writes are `PUT` rather than `POST`, matching the rest of the entity surface: these are
replacements of a whole resource, not submissions that create a new one.

### Entities

| Method | Path |
|---|---|
| `GET` `POST` | `/api/providers` |
| `PUT` `DELETE` | `/api/providers/{id}` |
| `POST` | `/api/providers/{id}/accounts` |
| `PUT` `DELETE` | `/api/providers/{id}/accounts/{account_id}` |
| `POST` | `/api/providers/{id}/models` |
| `POST` | `/api/providers/{id}/models/sync` |
| `DELETE` | `/api/providers/{id}/models/{model}` |
| `GET` `POST` | `/api/combos` |
| `PUT` `DELETE` | `/api/combos/{id}` |
| `GET` `POST` | `/api/aliases` |
| `DELETE` | `/api/aliases/{name}` |
| `GET` `POST` | `/api/keys` |
| `PUT` `DELETE` | `/api/keys/{id}` |
| `POST` | `/api/keys/{id}/rotate`, `/api/keys/{id}/regenerate` |
| `GET` | `/api/docs`, `/api/docs/{name}` |
| `GET` | `/api/activity` | The relay's combo activity and cooldown map, proxied from `GET /control/activity` — see [Control routes](#control-routes-loopback-only) in Part 1. When the relay is not answering it returns `502` naming the port rather than an empty list, because "nothing is running" and "I cannot see" are different facts |

`/api/providers/{id}` has no `GET`: the list route returns every provider in full, and a single
provider is read from there. `/api/providers/{id}/models` is a `POST` that upserts one model rather
than a `GET` listing, precisely because the list route already carries the models.
`/api/keys/{id}/rotate` and `/api/keys/{id}/regenerate` reach the same handler — the second spelling
is an alias kept for the panel.

**axum 0.8 note for contributors:** the path parameter syntax is `/{id}`, not `/:id`. The latter
panics at runtime rather than failing to compile, so it is caught only by starting the server.

### The one place a secret is returned

`POST /api/keys` returns the new client key's plaintext **exactly once**:

```json
{
  "id": "01924f...", "label": "laptop", "prefix": "mr-3f9a", "enabled": true,
  "allow_models": [], "allow_routes": [], "limit_requests": 0,
  "key": "mr-..."
}
```

There is no `warning` field: the response is the stored key with `key_hash` removed (`key_view`) plus
the plaintext `key`, and the status is `201 Created`. The obligation to save the key is the operator's,
not something the payload restates. It is stored as a SHA-256 hash, so there is nothing to show on a
later request. **No other endpoint returns a credential value** — not `/api/config`, not
`/api/providers`, not the migration. Credentials appear as ids or masked fingerprints.

### Quota and OAuth

| Method | Path | Notes |
|---|---|---|
| `GET` | `/api/quota` | Cached provider quota state for every account, plus its declared windows; `POST /api/quota/refresh` refreshes it. It does not read the request ledger — that is `/api/usage` below. See [Quota tracking](08-quota-tracking.md). |
| `GET` | `/api/usage` | The per-request usage ledger, with the token count reported by the provider or estimated by the router — see below. |
| `GET` | `/api/oauth` | The OAuth platforms this build supports. |
| `POST` | `/api/oauth/{platform}/authorize` | Starts an interactive login — a loopback listener, a page login, or (for Claude Code) a paste-only session. |
| `POST` | `/api/oauth/{platform}/device-code` | Starts a device-code login. |
| `POST` | `/api/oauth/{platform}/screen` | Starts a page login the conductor observes by polling (Cursor). |
| `POST` | `/api/oauth/{platform}/paste` | Starts a login that binds nothing and finishes when the person pastes the code the provider's page shows (Claude Code). The platform must declare `paste_redirect_uri()`; any other platform answers `400`. |
| `POST` | `/api/oauth/sessions/{id}/poll` | Polls a device-code session, or a page-login session. |
| `POST` | `/api/oauth/sessions/{id}/complete` | Completes an interactive login; blocks until the browser reaches the loopback redirect, or until the pasted code arrives. |
| `POST` | `/api/oauth/sessions/{id}/redirect` | Hands a pasted redirect URL (or its `code`/`state` query, or `code#state`) to a login that is already waiting. Answers `202`; the exchange still happens in the waiting `complete` call. |

The seven OAuth `POST`s are how a credential enters the vault without ever passing through
`config.json`; they are documented in [OAuth overview](10-oauth-overview.md).

### `GET /api/usage` — the token count, by the provider or by the router

The ledger of what the keys spent, one row per request, newest first. `?limit=` (default 50, max 500)
and `?key=<client-key-id>` narrow it.

```json
{
  "rows": [
    {
      "id": "0193f0c2-8a71-7c3e-9b02-5f1c6d4e7a10",
      "client_key_id": "key-a",
      "occurred_at": 1760000000000,
      "prompt_tokens": 120,
      "completion_tokens": 45,
      "provider_prompt_tokens": null,
      "provider_completion_tokens": null,
      "provider_reported": false,
      "estimated": true,
      "request_bytes": 480,
      "response_bytes": 180
    }
  ],
  "totals": {
    "requests": 1,
    "prompt_tokens": 120,
    "completion_tokens": 45,
    "estimated_requests": 1
  }
}
```

The token count of a request is reported **either by the provider** — the upstream sent a `usage`
block, and those numbers are in `provider_prompt_tokens` / `provider_completion_tokens` — **or by the
router**, in which case `estimated` is `true` and the numbers were derived from the byte counts the
relay measured (`request_bytes`, `response_bytes`) with the project's four-bytes-per-token rule.

**The estimate is computed in the panel process, not on the router's request path.** The relay only
counts bytes as they stream past; the arithmetic happens here, in the conductor. A row with neither
provider counts nor measured bytes keeps `null` numbers and `estimated: false` — unknown stays
unknown, and never becomes zero.

### The error envelope

```json
{
  "error": {
    "code": "validation_failed",
    "message": "The configuration has 3 problems.",
    "details": [
      { "field": "providers[2].base_url", "message": "must start with http:// or https://" },
      { "field": "combos[0].steps[1].provider", "message": "unknown provider \"openaii\"" },
      { "field": "quota.default_cooldown_secs", "message": "must be greater than 0" }
    ]
  }
}
```

**Validation returns every problem at once, deliberately.** Returning the first failure turns fixing a
configuration into a loop of save-and-retry, one error at a time. An operator editing JSON by hand
gets to fix everything in one pass.

### Bind policy

| Bind | Password | Result |
|---|---|---|
| Loopback | Not required | Starts |
| Non-loopback | Set | Starts |
| Non-loopback | Not set | **Refused at startup**, with a message saying how to set one |

It is refused rather than warned about. A panel that silently exposes provider credentials and client
keys to the network because the operator did not read a log line is not a defensible default.

## See also

- [Web panel](15-web-panel.md) — the interface over the control plane.
- [Configuration](03-configuration.md) — the schema both surfaces edit, including `reasoning_levels`
  and `routing.default_reasoning_level`.
- [Combos](06-combos.md) — model addressing in depth.
- [Routing snapshot](04-routing-snapshot.md) — how a name and its level are compiled into the table
  the relay reads.
- [Security](21-security.md) — sessions, origin checking and the vault.
- [Troubleshooting](20-troubleshooting.md) — when a response is not what you expected.
