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
| `GET` | `/v1/models` | OpenAI Models |
| `POST` | `/v1/messages` | **Anthropic** Messages |
| `POST` | `/v1/messages/count_tokens` | **Anthropic** token counting |
| `GET` | `/healthz` | Liveness. **Never routed upstream.** |

`/healthz` is deliberately not a model route. A health check that can be satisfied by a model
provider is a health check that reports the provider's health, not the router's — and when the
provider is down, the router looks down while it is working perfectly.

### Model addressing

The `model` field is resolved in this order, and the **first match wins**:

| # | Form | Example |
|---|---|---|
| 1 | Alias | `fast` → the combo `coding` |
| 2 | Combo id | `01924f3a-...` |
| 3 | Combo name | `coding` |
| 4 | `provider/model` | `anthropic/claude-sonnet-4-5` |
| 5 | Bare model id | `gpt-5`, if exactly one enabled provider offers it |

Two properties worth knowing before you debug a 404:

- **Aliases resolve in a single hop and are not transitive.** An alias pointing at another alias does
  not chain. This keeps resolution a flat lookup instead of a graph traversal on the request path, and
  makes a cycle impossible by construction.
- **Matching is case-insensitive, and a hash collision is never trusted.** The snapshot indexes names
  by `fnv1a64` for speed; on a hit the candidate is compared with a full `eq_ignore_ascii_case` before
  use. Two names that collide cannot silently route to the wrong target.

If a name matches none of the five, the answer is a 404 naming the model — not a silent fallback to
"some model", which would be worse than a failure.

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

- **Keepalive comments** (`: keepalive`) are sent during long gaps. Without them, an intermediary
  that times out idle connections kills a stream that is working perfectly.
- **`data: [DONE]`** terminates the stream. Always sent, including after an error.
- **`manarouter.retry`** is a router-specific event, emitted when the winning model failed mid-stream
  and the policy is `restart`:

  ```
  event: manarouter.retry
  data: {"from":"anthropic/claude-sonnet-4-5","to":"openai/gpt-5","reason":"upstream_disconnect"}
  ```

  **A client that ignores unknown SSE events — as the specification requires — still receives a
  valid, terminated response.** The retry event is informational. This is a compatibility
  requirement, not a nicety: any client that broke on an unknown event would break on the first
  provider that added one.

### Mid-stream failure

Once bytes have reached the client, the router cannot pretend the request did not happen.
`midstream_policy` decides ([Combos §midstream policy](06-combos.md#midstream-policy)):

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

- **`max_tokens` is required by Anthropic**, and this shim defaults it to **8192** when a client omits
  it. Never silently unlimited — a missing `max_tokens` that becomes an unbounded request is how a
  client discovers its bill.
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
| `POST` | `/api/config` | Validate → save atomically → recompile the snapshot → return the new digest. |
| `POST` | `/api/config/validate` | Returns **every** problem, not the first. |
| `GET` | `/api/config/raw` | The file as text, for the editor view. Credentials are ids, so nothing is exposed. |
| `GET` | `/api/ports` | Current and pending ports. |
| `POST` | `/api/ports` | Persists a change. **Applies on next restart.** |
| `POST` | `/api/restart` | Performs it. Drops in-flight connections, which is why it is explicit. |

### Entities

| Method | Path |
|---|---|
| `GET` `POST` | `/api/providers` |
| `GET` `PUT` `DELETE` | `/api/providers/{id}` |
| `POST` | `/api/providers/{id}/accounts` |
| `PUT` `DELETE` | `/api/providers/{id}/accounts/{account_id}` |
| `GET` | `/api/providers/{id}/models` |
| `POST` | `/api/providers/{id}/models/sync` |
| `GET` `POST` | `/api/combos` |
| `GET` `PUT` `DELETE` | `/api/combos/{id}` |
| `GET` `POST` | `/api/aliases` |
| `DELETE` | `/api/aliases/{name}` |
| `GET` `POST` | `/api/keys` |
| `DELETE` | `/api/keys/{id}` |
| `GET` | `/api/docs`, `/api/docs/{name}` |

**axum 0.8 note for contributors:** the path parameter syntax is `/{id}`, not `/:id`. The latter
panics at runtime rather than failing to compile, so it is caught only by starting the server.

### The one place a secret is returned

`POST /api/keys` returns the new client key's plaintext **exactly once**:

```json
{ "id": "01924f...", "label": "laptop", "key": "mr-...", "warning": "This key is not recoverable." }
```

It is stored as a SHA-256 hash, so there is nothing to show on a later request. **No other endpoint
returns a credential value** — not `/api/config`, not `/api/providers`, not the migration. Credentials
appear as ids or masked fingerprints.

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
- [Configuration](03-configuration.md) — the schema both surfaces edit.
- [Combos](06-combos.md) — model addressing in depth.
- [Security](21-security.md) — sessions, origin checking and the vault.
- [Troubleshooting](20-troubleshooting.md) — when a response is not what you expected.
