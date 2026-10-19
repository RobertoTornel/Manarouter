# API reference

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

| Method | Path | Shape |
|---|---|---|
| `POST` | `/v1/chat/completions` | OpenAI Chat Completions |
| `POST` | `/v1/responses` | OpenAI Responses |
| `POST` | `/v1/embeddings` | OpenAI Embeddings |
| `POST` | `/v1/audio/transcriptions` | OpenAI, multipart |
| `POST` | `/v1/audio/speech` | OpenAI, returns audio |
| `POST` | `/v1/images/generations` | OpenAI Images |
| `GET` | `/v1/models` | OpenAI Models, served from the compiled snapshot |
| `POST` | `/v1/messages` | **Anthropic** Messages |
| `POST` | `/v1/messages/count_tokens` | **Anthropic** token counting — counted upstream for an Anthropic provider, estimated locally (and marked with `x-manarouter-count-tokens-estimate: true`) for any other |
| `GET` | `/healthz` | Liveness. **Never routed upstream.** |

`/healthz` is deliberately not a model route. A health check that can be satisfied by a model
provider is a health check that reports the provider's health, not the router's — and when the
provider is down, the router looks down while it is working perfectly.

`GET /v1/models` is the one route that answers without touching a provider; what it lists is described
under [The `/v1/models` listing](#the-v1models-listing).

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
| 4 | Bare model id | `gpt-5`, if a visible model has exactly that id |
| 5 | `provider/<base model>-<level>` | `anthropic/claude-sonnet-4-5-high` |

A level-suffixed combo name shares rank 2 with the plain combo name, which is what makes
`coding-high` a way to pin an existing route to a tier without editing it. A rank-5 key, by contrast,
is derived by appending a suffix to a model id, so it is deliberately the **lowest** precedence there
is: an exact model id (rank 3) and even a bare id (rank 4) outrank it. Requesting a level is therefore
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
reasoning models only, currently `codex` — the generic set `none, low, medium, high, xhigh, max`. A
model that reaches none of those has **no** levels: its name gets no `-<level>` suffix, and it runs at
`none`, which emits no reasoning field at all. The generic set deliberately omits `minimal`, `ultra`
and `max`, which are provider-specific tiers; advertising one a provider rejects turns a working
request into a 400.

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
| anthropic | `{"thinking":{"type":"enabled","budget_tokens":N}}` with `N` = 1024 for `minimal`/`low`, 10240 for `medium`, 131072 for `high` and above |
| google, antigravity | `{"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"\|"high","thinkingBudget":N}}}`, where `thinkingLevel` is `high` from `medium` upward and `low` below it, and `N` is the budget above |

A per-model `reasoning_wire` map in the configuration **replaces the dialect default wholesale** for
the levels it names — a fragment has to be internally consistent, so merging one field-by-field is not
possible. `null` at a level means "send nothing at that level", which is a different statement from
"no opinion".

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

> **Status.** Bare model ids still resolve at rank 4 above when a visible model has exactly that id,
> so a name like `gpt-5` does **not** fail as long as one enabled provider offers it.
> Feature 3's design intended the bare fallback to be removed, and the generated-alias migration
> (`generated: true` aliases are deleted on config load, with `config.json.pre-migration.bak` written
> as a backup) is written on that assumption — but the bare index pass is still compiled, so the
> fallback remains. Treat a bare model id as *working but shadowed by design*: a combo or alias of the
> same name wins. The replacement the design intends is for the operator to create a combo named after
> the bare id.

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
  stream that breaks mid-body simply ends.
- **`manarouter.retry`** is the router-specific event that is *intended* to be emitted when the winning
  model failed mid-stream and the policy is `restart`:

  ```
  event: manarouter.retry
  data: {"from":"anthropic/claude-sonnet-4-5","to":"openai/gpt-5","reason":"upstream_disconnect"}
  ```

  **A client that ignores unknown SSE events — as the specification requires — still receives a
  valid, terminated response.** The retry event is informational. This is a compatibility
  requirement, not a nicety: any client that broke on an unknown event would break on the first
  provider that added one.

### Mid-stream failure

> **Status.** The behaviour in this section is the design. `midstream_policy` is compiled into the plan
> and accepted by `mr combo add --midstream-policy`, but the relay never reads it and never emits
> `manarouter.retry`: a step that dies after bytes are on the wire currently ends the client stream
> as-is, and there is no tool-call guard.

Once bytes have reached the client, the router cannot pretend the request did not happen.
`midstream_policy` decides ([Combos §midstream policy](06-combos.md#7-midstream_policy)):

| Policy | What the client sees |
|---|---|
| `restart` (default) | One coherent response, from the replacement model |
| `terminate` | A truncated stream, ended cleanly with `[DONE]` |

**Hard guard under both:** once any `tool_calls` delta has been emitted, the relay refuses to splice.
A tool call is a structured object the client is already assembling; half of one model's call plus
half of another's produces a call that is either invalid or, worse, valid and wrong. A truncated
stream is the honest outcome.

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
| `GET` | `/api/quota` | Aggregates the `usage` table per account. **Nothing writes that table today**, so it returns empty regardless of traffic — see [Quota tracking](08-quota-tracking.md). |
| `GET` | `/api/oauth` | The OAuth platforms this build supports. |
| `POST` | `/api/oauth/{platform}/authorize` | Starts an interactive login. |
| `POST` | `/api/oauth/{platform}/device-code` | Starts a device-code login. |
| `POST` | `/api/oauth/sessions/{id}/poll` | Polls a device-code session. |
| `POST` | `/api/oauth/sessions/{id}/complete` | Completes an interactive login. |

The four OAuth `POST`s are how a credential enters the vault without ever passing through
`config.json`; they are documented in [OAuth overview](10-oauth-overview.md).

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
