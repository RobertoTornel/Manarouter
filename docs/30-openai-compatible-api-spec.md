# 30 — The OpenAI-compatible API: an implementation spec for agents

This is the normative specification for the OpenAI-shaped HTTP surface of the Manarouter relay. It is
written for **agents**: an agent writing a client against this relay, an agent adding a route to it,
or an agent wiring a new provider into it. Every rule below is either enforced by code or pinned by a
test, and each section names the code that owns it so an agent can go straight to the thing it must
change rather than searching.

The human-facing classification of the surface — which families are proxied, which are served here,
and which are refused — is in [docs/29-openai-surface.md](29-openai-surface.md). The wire examples for
individual endpoints are in [docs/18-api-reference.md](18-api-reference.md). The dialect and
capability model is in [docs/24-provider-dialects.md](24-provider-dialects.md). This document is the
contract those three describe from different angles, stated once and in one voice.

## 0. How to use this document

### 0.1 Authority order

When two sources disagree, the earlier entry wins:

1. **The code**, as it exists on the branch you are editing. A specification that has drifted from
   the code is a bug in the specification, and the correct fix is to correct the document in the same
   change that moved the behaviour.
2. **This document**, for the client-visible contract: paths, status codes, envelopes, admission
   rules, and the invariants in §5.
3. **`docs/18`, `docs/24`, `docs/29`**, for per-endpoint wire shapes and the dialect/capability model.
4. **`docs/23-implementation-status.md`**, for what has been verified against a live vendor and what
   has only been verified against a fake upstream. Read it before claiming a family works in
   production.

An agent that finds this document describing behaviour the code does not have must not "fix" the code
to match the document without checking the tests. Most of the surprising rules here are pinned by a
test whose name states the reason, and Appendix D lists the ones worth reading first.

### 0.2 Vocabulary

| Term | Meaning in this document |
| --- | --- |
| **client** | The process calling the relay. It presents a *client key*, never a provider credential. |
| **provider** | An upstream service configured by the operator (OpenAI, Anthropic, a local vLLM, `fish.audio`, …). |
| **account** | One credential belonging to a provider. A provider may have several, and the relay chooses between them. |
| **model** | A model id as the provider names it, plus the relay's routing metadata for it. |
| **combo** | An operator-created route that tries several models in order. A combo is addressable exactly like a model. |
| **step** | One participant of a combo: a provider, a model, and per-step timeouts. A plain model is a one-step route. |
| **route** | A client-visible path plus the resolution it performs. `/v1/chat/completions` is one route regardless of how many combos it can reach. |
| **capability** | A declaration on a provider that it can serve a family (`chat`, `video`, `fine-tuning`, …). The relay refuses a route when no reachable step declares the capability the route requires. |
| **capability-addressed** | A path whose provider is chosen by *capability* rather than by a model named in the body. Fine-tuning, containers and the model-optional routes work this way. |
| **snapshot** | The compiled, immutable routing table the relay serves from. It is versioned (`SNAPSHOT_VERSION`) and replaced atomically. |

### 0.3 Verifying a claim before you rely on it

Run the narrow tests for the area you are touching, then the workspace gate. The gate is the same one
CI runs, and it must end with the marker rather than merely printing a passing count:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The panel suites are separate and use `node --test`:

```bash
cd panel && node --test
cd panel-lab && node --test
```

`node --test` prints `ℹ pass` and `ℹ fail`; grep for those exact prefixes when scripting a check. A
compound command that ends with a passing suite can still exit non-zero, so a gate script must use
`set -e` and record the exit code explicitly.

### 0.4 What this build is not

The relay is a **router and a shim**, not a provider. It does not implement inference, does not run
fine-tuning, does not grade evals with a model it hosts, and does not pretend to. Where it cannot do
something honestly it refuses with a specific code (§1.8) rather than returning a plausible-looking
empty result. That rule is the reason several endpoints in §2 are marked *refused* instead of
*proxied*.

## 1. The client contract

Everything in this section is what a client may rely on. "MUST" is a requirement on the client or on
an implementation claiming conformance; "MUST NOT" is a prohibition; "SHOULD" is expected behaviour
whose absence needs a reason.

### 1.1 One listener, one prefix

The relay serves one HTTP listener (`127.0.0.1:20900` by default). Every OpenAI-shaped path begins
with `/v1/`. A client's base URL MUST therefore be `http://<host>:<port>/v1`, and a client library
that appends `/chat/completions` to the base URL will reach the right handler. A base URL without
`/v1` does not work: `/chat/completions` is not registered.

The relay does not add a router fallback. An unregistered path is answered by the HTTP framework with
`404 Not Found`, `content-length: 0`, and **no JSON body at all**; a registered path called with a
method it does not mount is answered the same way with `405`. This is the one place where the surface
is not OpenAI-shaped, and it is deliberate: a typo should look like a typo, not like a provider error.
A client MUST NOT parse an empty 404 or 405 as an error envelope; it should treat it as "wrong path,
wrong base URL, or wrong method". Verified directly:

```
$ curl -s -D- -o /dev/null http://127.0.0.1:20900/v1/nope
HTTP/1.1 404 Not Found
content-length: 0
```

`GET /healthz` answers `{"status":"ok"}` and consults no provider. `POST /control/probe` and
`GET /control/activity` are the operator control plane; they are not part of the OpenAI contract and
are documented in `docs/18`.

Method handling is declared per path and is not uniform. Proxied paths use `any(...)`, so the
client's method reaches the handler and is forwarded verbatim; the upstream decides whether it is
allowed. Locally served paths declare their methods precisely (`get(...).delete(...)`,
`post(...).get(...)`), and anything else is the framework's empty `405`. The exception is
`/v1/models`, which is mounted with `any(...)` and answers the listing for every method, because the
listing is read-only and answering it is more useful than a `405`. A client SHOULD use the method
named for each path in §2; an agent adding a route MUST choose deliberately between `any` (proxied)
and an explicit method set (local), and MUST NOT rely on the framework to reject a method on a proxied
path.

### 1.2 Credentials

A client authenticates with a **client key**, which is not a provider credential and grants no access
to any provider directly. Two header forms are accepted on every route:

```
Authorization: Bearer <client-key>
x-api-key: <client-key>
```

The scheme is matched case-insensitively and the space after it is optional, because header
normalisation can strip trailing whitespace and `Authorization: Bearer ` otherwise arrives as the
literal word `Bearer`. When both headers are present, `x-api-key` wins. This precedence is fixed in
`presented_key` (`crates/manarouter-relay/src/server.rs:4617`) and is not configurable.

Failures are `401` with the OpenAI error envelope, code `invalid_api_key`, and a `WWW-Authenticate:
Bearer` header. The message distinguishes the two cases an operator actually needs to tell apart:

- No credential at all: `no client key was presented. Send one as \`Authorization: Bearer <key>\` or
  \`x-api-key: <key>\`.`
- A credential that did not validate: `the client key is not valid`.

Open mode (no client keys configured) accepts only a **genuinely absent** credential. A key that is
presented when none exist is rejected, because there is nothing it could have been validated against.
An implementation MUST NOT treat "no keys configured" as "any key is valid".

Credentials are never placed in a URL. `AuthKind::QueryParam` is recognised in the provider model and
is deliberately never applied by `apply_auth` (`server.rs:5561`): a provider that requires a query
parameter credential cannot be used through this relay, and the relay does not leak the secret into
request lines or logs to support it. There is no query-parameter client authentication either.

### 1.3 Bodies: what the relay reads and what it forwards

The relay's central promise is that **bytes are the contract**. It forwards the client's body
unchanged unless it must translate dialects, resolve a qualified model name, apply a reasoning level,
retarget a model for modality support, or stage a fine-tuning file. It does not validate request
schemas, does not reject unknown fields, does not fill defaults, and does not re-serialise JSON it did
not have to touch. A client MAY therefore send any field its upstream understands, including fields
newer than this document.

To route, the relay reads exactly three top-level fields from the body, using a linear scanner rather
than a JSON parse (`crates/manarouter-relay/src/model_scan.rs:113`):

| Field | Use |
| --- | --- |
| `model` | Chooses the route. First occurrence wins, matching `serde_json` and therefore matching the upstream's interpretation. |
| `stream` | Chooses streaming behaviour and the upstream `accept` header. Only a literal `true` counts. |
| `n` | Lets quota count a request that asks for several completions. |

The scanner tracks string boundaries and nesting depth, so a `"model"` inside a tool schema or a
conversation is not matched, and only the top-level object's field is used. It is not a validator: a
malformed body that yields a readable model is forwarded and rejected upstream, which is the correct
outcome. One consequence to know: the scanner returns the **raw** value bytes, so a model name written
with JSON escapes (`"gpt\u002D5"`) is not decoded for routing even though the upstream will decode it.
Clients SHOULD write model names plainly.

Multipart bodies are handled by locating the `model` field's text part when the route needs one
(`model_scan.rs` exposes `multipart_boundary` for this). Audio and image uploads are otherwise
forwarded byte for byte with the client's `Content-Type` and boundary intact.

The whole body is buffered with a hard cap of **32 MiB** (`MAX_BODY_BYTES`, `server.rs:67`; the bound
is asserted at compile time between 16 and 64 MiB). Exceeding it produces `413` with code
`payload_too_large` and message `the request body exceeds 33554432 bytes`. A body that cannot be read
produces `400` with code `invalid_request_error` and message `the request body could not be read:
<reason>`. Both are generated **before** dialect dispatch, so both use the OpenAI envelope even on
`/v1/messages`; see §5.2, deviation 3.

A route that needs a model and does not find one returns `400 invalid_request_error` with message
`the request body has no readable "model" field`. Exactly three routes are allowed to omit the model
because they have a documented upstream default: `POST /v1/moderations`, `POST /v1/images/edits`, and
`POST /v1/images/variations`. Those three become capability-addressed (§1.10) and are pinned by
`only_the_three_routes_with_a_documented_default_accept_a_missing_model`.

### 1.4 Addressing a route

The `model` string is resolved against the snapshot in a fixed order (`docs/18` §"Model resolution").
Rank 0 wins, then 1, and so on:

| Rank | Form | Example |
| --- | --- | --- |
| 0 | Alias | `fast` (an alias the operator defined) |
| 1 | Combo id | `combo_7f3a…` |
| 2 | Combo name, optionally with a level | `coding`, `coding-high` |
| 3 | `provider/<exact model id>` | `open-ai/gpt-4o-mini` |
| 5 | `provider/<base model>-<level>` | `open-ai/gpt-5-high` |

Rank 4 is deliberately unassigned: a **bare** model id such as `gpt-5` does not resolve, even if some
provider serves it, because two providers may serve the same id and the relay will not guess. The
error is `404 model_not_found`. Clients SHOULD discover names from `GET /v1/models` rather than
constructing them.

Resolution is case-insensitive, with a full comparison after the hash match, so `Open-AI/GPT-4O-Mini`
resolves like `open-ai/gpt-4o-mini` while two names that merely collide in the hash do not. Alias
chains are flattened when the snapshot is compiled, so an alias of an alias costs one lookup.

A level suffix selects a reasoning level. The ladder is:

```
none < minimal < low < medium < high < xhigh < ultra < max
```

`xhigh` also accepts `x-high`, `extra-high`, `extra_high`, and `extrahigh`. A level the model does not
offer reroutes to that model's **highest** offered level rather than failing, because a client asking
for more reasoning than a model has should get the most it has. A qualified name that does not parse
as a level falls through to the rank-3 exact-model lookup, so a model whose id genuinely ends in
`-high` is still addressable.

Two request-shaping rules apply after resolution:

- **The client's explicit reasoning field wins.** If the client sends `reasoning_effort` (or the
  dialect's equivalent), a level suffix does not overwrite it. The level only supplies a value the
  client omitted.
- **A provider that rejects the reasoning parameter is retried once.** The relay reconciles an
  `unsupported_reasoning_effort` refusal to the nearest value the provider names and retries once; if
  the model does not support the parameter at all, the field is dropped and the request is retried
  once. This is the only case where the relay rewrites a semantic field the client sent, and it is
  bounded to a single retry.

An optional `?provider=<name>` query parameter narrows a model-bound route to one provider. It is
honoured on capability-addressed routes (§1.10) and is the documented way to pin a request when a
combo would otherwise choose. An unknown provider produces `404` naming the provider.

### 1.5 Discovery: `GET /v1/models`

`GET /v1/models` is served entirely from the compiled snapshot; it never contacts a provider
(`list_models`, `server.rs:550`; the rows are built by `model_listing`, `server.rs:410`). It requires
the same client key as every other route and returns `{"object":"list","data":[…]}` with no
pagination.

The listing contains, for each route the operator has made available:

- every combo, as a name and as an id;
- every combo qualified by each level it offers;
- every `provider/<model>` pair;
- every `provider/<model>-<level>` where the model genuinely offers that level.

Each entry carries a `manarouter` object describing what the name is. A base entry:

```json
{
  "id": "open-ai/gpt-5",
  "object": "model",
  "created": 0,
  "owned_by": "open-ai",
  "manarouter": {
    "kind": "provider",
    "dialect": "open-ai",
    "default_reasoning_level": "medium",
    "reasoning_levels": ["minimal", "low", "medium", "high"],
    "input_modalities": ["text", "image"]
  }
}
```

`kind` is `provider` or `combo`, never `model`. A combo entry carries `strategy` and `steps` instead
of `dialect` and the reasoning fields. A level-qualified entry carries only `kind`, `dialect` (for a
provider) and the chosen `reasoning_level` — it repeats no modality or level list, because the
qualification is a name for the base entry and not a different model:

```json
{
  "id": "open-ai/gpt-5-high",
  "object": "model",
  "created": 0,
  "owned_by": "open-ai",
  "manarouter": {"kind": "provider", "dialect": "open-ai", "reasoning_level": "high"}
}
```

`created` is always `0`: nothing in the snapshot records a creation time, and inventing one would be a
fact the relay does not have. The listing does **not** advertise capabilities. An agent MUST NOT infer that a route works because a model appears
here: availability of, say, `/v1/videos` depends on the provider declaring the `Videos` capability,
and
the only way to learn that from the surface is to make the request and read the `404` (§1.10). This is
a known limitation, stated again in §5.3.

`GET /v1/models/{model}` reads the same listing and returns the single entry, or `404
model_not_found`. The path is a wildcard, so a model id containing `/` is addressable without
percent-encoding the slash. `DELETE /v1/models/{model}` is **always** refused with `400
invalid_request_error` and the message `deleting a model is only possible for a fine-tuned model on a
provider that serves fine-tuning, and this model is not one`. Nothing is forwarded, even when a
provider would accept the delete. Clients that manage fine-tuned models must delete them at the
provider.

### 1.6 Response ownership and headers

On a proxied request the upstream's status code and body are returned to the client unchanged. The
relay does not rewrite a successful provider body, does not add fields, and does not normalise an
error body it did not generate. A client MUST therefore be prepared for the provider's own error
shape when the provider answered — a provider `401`, `404` or `429` can reach the client verbatim
after failover is exhausted (§1.9).

Response headers are the upstream's, minus hop-by-hop headers. `HOP_BY_HOP` (`server.rs:5965`) is
`connection`, `keep-alive`, `proxy-authenticate`, `proxy-authorization`, `te`, `trailer`,
`transfer-encoding`, `upgrade`, and `content-length`. Dropping `content-length` is what lets a
translated or re-framed body be sent with the correct length instead of the upstream's. Everything
else, including provider request ids and rate-limit headers, passes through.

`content-type` is owned differently on the two paths:

- **Untranslated path:** the upstream's `content-type` is forwarded untouched (`build_response`,
  `server.rs:5997`).
- **Translated path:** the relay forces the type the client expects (`server.rs:5777`):
  `text/event-stream; charset=utf-8` when streaming, otherwise `application/json; charset=utf-8`.

The relay adds exactly one header of its own: `x-manarouter-count-tokens-estimate: true` on a locally
estimated `POST /v1/messages/count_tokens`. It does not add a request id, a server header, or a
timing header. Agents SHOULD NOT depend on one.

Request headers are built from the step's configured headers, the provider credential, OAuth where
configured, and `content-type`. The client's `content-type` is used only when the body is forwarded
untranslated (it must be, or a multipart boundary would break). Other client headers are not
forwarded, with one exception: when the request streams, the relay sets `accept: text/event-stream`
on the upstream request. A client cannot smuggle a header through to the provider; if a provider
needs one, it belongs in the provider configuration.

### 1.7 Streaming

Streaming is selected by a top-level `"stream": true` in the request body, not by the `Accept` header.
The relay sets `accept: text/event-stream` upstream, forwards the response as it arrives, and does not
generate keepalives of its own: comments the provider sends are passed through, and a stream with no
events simply stays open for as long as the provider holds it.

The contract for a translated stream (a dialect change, §3.5) is:

- Each upstream event is translated and emitted as one `data:` frame.
- `data: [DONE]` terminates a completed stream. It is emitted once, by the relay, after the final
  translated event of a stream that completed.
- A mid-stream failure follows the combo's `midstream_policy`. With `restart`, the relay emits an
  `event: manarouter.retry` boundary and starts the replacement attempt; with `terminate`, the stream
  ends with an error event. The relay never silently splices a second attempt's tokens into a stream
  that already emitted `tool_calls` deltas, because a client would then assemble a tool call from two
  different completions.
- A frame is buffered to an event boundary before it is written, so a mid-stream death never hands the
  client half an SSE frame.

An untranslated stream is forwarded byte for byte, including any `event:` lines and comments the
provider sends. A client that needs the retry boundary must use a translated stream, or a combo whose
policy it can observe through the provider's own framing.

### 1.8 Errors

The relay generates errors in the dialect the client asked in (`error_response`, `server.rs:6041`):

```json
{"error": {"message": "…", "type": "…", "code": "…"}}
```

```json
{"type": "error", "error": {"type": "…", "message": "…"}}
```

The OpenAI form is used for every `/v1/*` OpenAI route; the Anthropic form is used for `/v1/messages`
and `/v1/messages/count_tokens`. `param` is not emitted. Appendix B lists every local code and the
status it arrives with. The codes an agent is most likely to meet:

| Status | Code | Meaning |
| --- | --- | --- |
| 400 | `invalid_request_error` | The body could not be read, had no model, or named something malformed. |
| 401 | `invalid_api_key` | Missing or invalid client key. |
| 404 | `model_not_found` | The model/alias/combo did not resolve. |
| 404 | `unsupported_endpoint` | The route's capability is not declared by any reachable step. |
| 413 | `payload_too_large` | Body over 32 MiB. |
| 429 | `rate_limit_error` | Quota exhausted, or every account is cooling down. |
| 429 | `no_usable_accounts` | No account for the step can be selected at all. |
| 402/403 | `insufficient_credit` | A fine-tuning job was refused before creation. |
| 501 | `modality_fallback_unavailable` | No step carries the input modality and the default cannot be used. |
| 501 | `unsupported_endpoint` | Containers with no declaring provider. |
| 502 | `proxy_error` | The relay could not reach the upstream. Never retried on another account. |

A provider error is **not** translated into a local code. If the provider says `429` with its own
body, that is what the client receives once the relay has finished trying. This is intentional: an
operator debugging a provider needs the provider's words, not a paraphrase.

The relay's own `429` is the quota admission refusal (`quota_error`, `server.rs:6064`), not an
exhausted-failover summary: the message names the key's spent budget ("this key has used its weekly
budget of N token(s)") and `Retry-After` is the length of the window that was exceeded, rounded up to
whole seconds. The flat `limit_requests` budget has no window, so it carries no `Retry-After`. A
provider's `429` is forwarded with the provider's own `Retry-After` untouched, and an exhausted
failover chain forwards the last provider response rather than synthesizing a relay `429`.

### 1.9 Retry, failover, and the absence of idempotency

A request is attempted against each step of the resolved route in order. Within a step, accounts are
tried according to the selection policy. The classification of a failed attempt decides what happens
next (`attempt_step`, `server.rs:3061`, and the classification helpers around `:3820`):

| Upstream result | What the relay does |
| --- | --- |
| `2xx` | Returns it, records health, and honours stickiness. |
| `400`, `404`, `405`, `413`, `415`, `422` (a request problem) | Returns the provider's body; does **not** try another account for the same step; advances to the next combo step, carrying the answer. |
| `401`, `403` | Cools the account down as `AuthFailed` and tries another account, then the next step. |
| `402` | Cools the account down as `QuotaExhausted` and fails over silently. |
| `429` | Cools the account down as `RateLimited` (for the reset the body names, if it names one) and fails over silently. |
| `503` from a Modal/vLLM step | Cools the account down as `ColdStart` and fails over. |
| Any other status | Cools the account down as `Backoff` and fails over. |
| Connection failure / timeout | Cools the account with a backoff and fails over. |
| Proxy failure after the request was sent | `502 proxy_error`; never rerouted, because the upstream may have performed the work. |

When every step has failed, the **last provider response is forwarded verbatim**, so a client can see
a provider `401`, `404` or `429` even though other accounts were tried first. An agent MUST NOT assume
that a `429` from this relay always means the relay's own quota; check the body's `code`.

**There is no idempotency support.** The relay honours no `Idempotency-Key`, `X-Request-Id`, or
similar header, and it does not deduplicate. A client that retries a `POST` after a timeout may create
a second completion, a second batch, or — worst — a second fine-tuning job, which costs real money.
Clients MUST make retries safe themselves: retry only on a response the relay generated (a `502`, a
`503`, or a `429` with a `retry-after`), never blindly after a transport error on a mutating request.
This is a deliberate omission, not an oversight; adding it would require a durable request log keyed
by a client-supplied value, and the relay has no such log.

### 1.10 Capability admission

Every route requires a capability. The requirement is defined once, in `RouteKind::capability()`
(`crates/mr-core/src/dialect.rs:658`), and the relay consults it in two places:

- **Model-bound routes** (`server.rs:2235`): the request is refused with `404 unsupported_endpoint`
  and the message `the route <path> requires the <capability> capability, and no step's provider
  declares it` when no step of the resolved route has a provider declaring it. The gate runs **per
  step in step order**: the earliest step with a funded account wins, and an exhausted step is
  skipped rather than failing the request. A combo whose first participant cannot do video will
  therefore still serve a video request from a later participant that can.
- **Capability-addressed routes** (`forward_to_capability`, `server.rs:1055`): there is no model to
  resolve, so the provider is the first one declaring the capability, or the one named by
  `?provider=`. With none, the answer is `404 unsupported_endpoint` and the message `the route <path>
  requires the <capability> capability, and no provider declares it` (or `no provider named "<name>"
  declares the <capability> capability` when a provider was named).

Containers are the exception: with no declaring provider the answer is `501`, because the relay does
not run containers itself and `501` states that more honestly than `404` would.

Capability declarations are validated when a provider is configured and when a catalog is loaded
(`Provider::validate` at `model.rs:892`, `dialect_capability_mismatches` at `:1014`, called from
`catalog.rs:240`). A provider whose dialect cannot speak an OpenAI-only capability is rejected at load
time rather than silently gated at request time. An agent adding a capability MUST add it to that
validation, not only to the runtime gate.

### 1.11 Partial providers, and why the surface is not uniform

A provider may implement only one family. `fish.audio` is the canonical example: it speaks an
OpenAI-shaped audio API and nothing else. The model supports this without special cases:

1. The provider template declares only the capabilities it has (`audio.speech`,
   `audio.transcription`, …).
2. The provider's models are listed normally, so `GET /v1/models` advertises them.
3. A request for a family the provider does not declare is refused by the gate with a specific
   `404 unsupported_endpoint`, not a confusing upstream error.
4. The panel shows the declaration as a warning when a combo mixes providers that cannot all serve the
   same route.

An agent adding such a provider MUST declare exactly the families it serves, MUST use the provider's
real dialect for them, and MUST NOT add a fake route to make the provider look complete. The
capability list is the documentation; there is no second list to update. Appendix A is the mapping.

### 1.12 Deliberately absent

The following are **not** part of the surface and MUST NOT be added:

- `/v1/completions`, `/v1/edits`, `/v1/engines/*`, `/v1/fine-tunes` — the legacy OpenAI API.
- Assistants, threads, runs, and the old files-in-assistants model.
- `/v1/organization/*`, `/v1/projects/*`, `/v1/users/*` — account administration belongs to the
  provider, not to a router.
- `/v1/audio/voices`, `/v1/audio/voice_consents` and the other newer audio-management paths. They are
  not in the 51-path surface; adding one requires a provider that serves it and a capability to gate
  it, which is a product decision rather than a mechanical addition.

"Newest spec" is the rule: when a family is added, add the current shape, not a deprecated one. The
51 paths in §2 are the whole surface.

## 2. The surface map

Every path below is registered exactly once. `the_documented_route_list_is_the_one_registered`
(`server.rs:9023`) asserts the list has 51 entries and that each reaches a handler; `no_path_is_registered_twice`
(`server.rs:9115`) mounts the router, which panics on a duplicate. `/v1/search` in particular is
registered in `vector_stores.rs` only, because `Router::merge` panics on a duplicate path and a
startup panic is how that mistake would present.

The *owner* column is the module that serves the path; *provider?* says whether a request can reach an
upstream; *gate* names the capability that must be declared when it can.

### 2.1 Liveness and control plane

| Path | Methods | Owner | Provider? | Gate |
| --- | --- | --- | --- | --- |
| `/healthz` | GET | `server.rs` | no | — |
| `/control/probe` | POST | `server.rs` | yes, one real chat request | model-bound |
| `/control/activity` | GET | `server.rs` | no | — |

The two control paths are loopback-only and **skip the client key**, because they are called by the
panel on the same host. `POST /control/probe` takes `{"model": "<name>", "prompt": "…"}` and runs one
real `/v1/chat/completions` request through the ordinary dispatch, so it exercises the same routing,
translation, account selection and cooldowns a client would; it is the supported way to answer "does
this route work right now". A non-loopback caller gets `403` with code `probe_not_local` (or
`activity_not_local`) before the body is read. An agent MUST NOT relax the loopback check or add the
client-key check *instead* of it; both are load-bearing.

### 2.2 Text and models

| Path | Methods | Owner | Provider? | Gate |
| --- | --- | --- | --- | --- |
| `/v1/chat/completions` | POST | `server.rs` | yes | `chat` |
| `/v1/responses` | POST | `server.rs` → `responses.rs` for storage | yes | `responses` |
| `/v1/responses/{id}` | GET, DELETE | `responses.rs` | no | — |
| `/v1/responses/{id}/cancel` | POST | `responses.rs` | no | — |
| `/v1/responses/{id}/input_items` | GET | `responses.rs` | no | — |
| `/v1/messages` | POST | `server.rs` | yes | `messages` |
| `/v1/messages/count_tokens` | POST | `server.rs` | yes, or estimated locally | `messages` |
| `/v1/models` | any | `server.rs` | no | — |
| `/v1/models/{*model}` | GET, DELETE | `server.rs` | no | — |

`/v1/responses` is proxied for creation; the retrieval, cancel and input-item reads are answered from
the relay's own store and only see responses that were created with `store: true` (§3.8). A stored
response is written only when the whole body is a single JSON object; a streamed response is not
stored, because the relay would have to reassemble it and would then be storing something the client
never received.

### 2.3 Modalities

| Path | Methods | Owner | Provider? | Gate |
| --- | --- | --- | --- | --- |
| `/v1/embeddings` | POST | `server.rs` | yes | `embeddings` |
| `/v1/moderations` | POST | `server.rs` | yes | `moderations` |
| `/v1/audio/transcriptions` | POST | `server.rs` | yes | `audio.transcription` |
| `/v1/audio/translations` | POST | `server.rs` | yes | `audio.translation` |
| `/v1/audio/speech` | POST | `server.rs` | yes | `audio.speech` |
| `/v1/images/generations` | POST | `server.rs` | yes | `images.generations` |
| `/v1/images/edits` | POST | `server.rs` | yes | `images.edits` |
| `/v1/images/variations` | POST | `server.rs` | yes | `images.variations` |
| `/v1/videos` | POST | `server.rs` | yes | `video` |
| `/v1/videos/{*rest}` | GET | `server.rs` | yes | `video` |
| `/v1/realtime` | GET (websocket) | `realtime.rs` | yes, pumped | `realtime` |
| `/v1/realtime/client_secrets` | POST | `server.rs` | yes | `realtime` |
| `/v1/realtime/calls` | POST | `server.rs` | yes | `realtime` |

`/v1/videos/{*rest}` covers both `GET /v1/videos/{id}` (status) and `GET /v1/videos/{id}/content` (the
rendered bytes). The content response is not JSON and is streamed through untouched; the relay must
not buffer it as an error body. `route_kind_for` (`server.rs:5512`) has the suffix logic, and it is
the only place that distinguishes the three video routes.

### 2.4 Files, uploads and batches

| Path | Methods | Owner | Provider? | Gate |
| --- | --- | --- | --- | --- |
| `/v1/files` | POST, GET | `batch.rs` | no | — |
| `/v1/files/{id}` | GET, DELETE | `batch.rs` | no | — |
| `/v1/files/{id}/content` | GET | `batch.rs` | no | — |
| `/v1/uploads` | POST | `uploads.rs` | no | — |
| `/v1/uploads/{id}/parts` | POST | `uploads.rs` | no | — |
| `/v1/uploads/{id}/complete` | POST | `uploads.rs` | no | — |
| `/v1/uploads/{id}/cancel` | POST | `uploads.rs` | no | — |
| `/v1/batches` | POST, GET | `batch.rs` | items are proxied | per item |
| `/v1/batches/{id}` | GET | `batch.rs` | no | — |
| `/v1/batches/{id}/cancel` | POST | `batch.rs` | no | — |

Files and resumable uploads are local storage. A batch is a local job whose **items** are dispatched
through the ordinary request pipeline, so every item passes the same model resolution, capability
gate, account selection and quota admission a direct request would. That is the guarantee that a
batch cannot be used to reach a route the direct surface refuses.

### 2.5 Vector stores and retrieval

| Path | Methods | Owner | Provider? | Gate |
| --- | --- | --- | --- | --- |
| `/v1/vector_stores` | POST, GET | `vector_stores.rs` | no | — |
| `/v1/vector_stores/{id}` | GET, POST, DELETE | `vector_stores.rs` | no | — |
| `/v1/vector_stores/{id}/files` | POST, GET | `vector_stores.rs` | no | — |
| `/v1/vector_stores/{id}/files/{file_id}` | GET, DELETE | `vector_stores.rs` | no | — |
| `/v1/vector_stores/{id}/file_batches` | POST | `vector_stores.rs` | no | — |
| `/v1/vector_stores/{id}/file_batches/{batch_id}` | GET | `vector_stores.rs` | no | — |
| `/v1/vector_stores/{id}/search` | POST | `vector_stores.rs` | embedding call only | `embeddings` for that call |
| `/v1/search` | POST | `vector_stores.rs` | conditionally | `search` when a model is named |

`POST /v1/search` is dual-natured and the branch is decided from the body **before** any lookup:

- A body containing `model` is dispatched byte for byte through `dispatch_internal`, so it obeys the
  ordinary capability gate and can be served by a provider that hosts a search endpoint. `model` wins
  when the body also names a store; a client that names both is asking the provider.
- A body without `model` is local retrieval over the relay's stores and MUST name
  `vector_store_id` or `vector_store_ids`, but not both. The query MUST be a non-empty string on this
  path; a numeric query vector is only accepted on the per-store search route.

### 2.6 Jobs

| Path | Methods | Owner | Provider? | Gate |
| --- | --- | --- | --- | --- |
| `/v1/fine_tuning/jobs` | POST, GET | `server.rs` | yes | `fine-tuning` |
| `/v1/fine_tuning/jobs/{*rest}` | GET, POST | `server.rs` | yes | `fine-tuning` |
| `/v1/containers` | any | `server.rs` | yes | `containers` (else 501) |
| `/v1/containers/{*rest}` | any | `server.rs` | yes | `containers` (else 501) |
| `/v1/evals` | POST | `evals.rs` | no, runs call the pipeline | per run |
| `/v1/evals/{id}` | GET | `evals.rs` | no | — |
| `/v1/evals/{id}/runs` | POST | `evals.rs` | yes, as ordinary requests | per request |
| `/v1/evals/{id}/runs/{run_id}` | GET | `evals.rs` | no | — |

Fine-tuning is a pure proxy with one relay-side seam: the training file (§3.9). The credit gate
(§3.6) applies to **creation only**; a status poll, an event listing, a pause, or a cancel is an
ordinary capability-addressed request and is not re-gated. Containers are proxied when a provider
declares them and refused with `501` otherwise. Evals are a local record: a run issues real requests
through the pipeline, so its cost is real and its failures are the pipeline's failures.

## 3. Implementation

This part is for an agent changing the relay. It describes the order of operations and the seams,
with the code that owns each one.

### 3.1 Crates and modules

| Crate | Responsibility |
| --- | --- |
| `mr-core` | The provider/model/combo model, dialects and capabilities, the snapshot, the store, the catalog, quota accounting, ids. No HTTP. |
| `manarouter-relay` | The HTTP listener, routing, translation, account selection, upstream transport, and the locally served families (`batch`, `uploads`, `responses`, `vector_stores`, `evals`, `realtime`, `quota`, `recovery`). |
| `manarouter-conductor` | The control plane and panel backend: configuration, credentials, provider sync, snapshot publication. |
| `mr-cli` | `mr`, the operator CLI. |
| `mr-dialects` | Translation tables and their tests. |

The relay never reads configuration files directly. It serves a **snapshot** published by the
conductor; the conductor is the only writer. An agent adding a request-path feature that needs new
configuration MUST add it to the snapshot (with a version bump, §4.5) rather than reading a file from
the relay.

### 3.2 The request pipeline, in order

A request to a model-bound route passes through these stages. The order matters, and several tests
pin it.

1. **Mount.** `router()` (`server.rs:165`) merges the family routers, then the OpenAI and Anthropic
   dispatch entry points. A duplicate path panics here.
2. **Body read.** `read_body` buffers up to `MAX_BODY_BYTES` while the body arrives, so an oversized
   upload is refused before it is held in memory. Failure returns the `413`/`400` from §1.3.
3. **Head scan.** `scan_request_head` extracts `model`, `stream`, and `n`. No JSON parse.
4. **Authentication.** `authenticate_request` with the presented key (§1.2). Authentication happens
   before resolution, so an unauthenticated request cannot learn which models exist.
5. **Quota admission.** For a capped key, the request reserves one request plus a prompt-token
   estimate (`CHARS_PER_TOKEN` = 4). This happens **before any upstream side effect**, and the
   reservation is inherited by a fallback re-entry rather than taken twice. A refused request writes
   nothing.
6. **Resolution.** The model string is resolved against the pinned snapshot (§1.4). A pinned snapshot
   is used for the whole request, including a modality fallback, so a configuration change mid-flight
   cannot make one request see two different tables.
7. **Model-optional routing.** If the route may omit the model and none was found, the request becomes
   capability-addressed (§3.4). Otherwise a missing model is the `400` from §1.3.
8. **Capability gate.** Per step, in step order (§1.10).
9. **Combo selection and account selection.** The step order is walked; within a step,
   `select::account_candidates` ranks usable accounts by the configured policy. Sticky sessions and
   round-robin are both implemented here; `select.rs` is pure and its tests are the specification.
10. **Body shaping.** Translation, reasoning-level merge, modality retarget, fine-tuning staging. The
    shaping is a closed set of function pointers (`BodyShaper`, `server.rs:687`) rather than a runtime
    closure, so the behaviours are visible at the call sites.
11. **Upstream request.** Built from the step's base URL, dialect, auth, headers and timeouts.
12. **Attempt.** `attempt_step` (`server.rs:3061`) sends it, classifies the result (§1.9), and either
    returns the response or records a failure and moves on.
13. **Response.** Verbatim on the untranslated path; translated and re-framed on the other (§1.6).
14. **Accounting.** Usage is recorded for a metered request; health and stickiness are updated on
    success; the quota reservation is settled.

A locally served family (files, batches, vector stores, evals, stored responses) enters at step 4 and
then runs its own logic. It MUST still authenticate and MUST still apply quota where it makes
upstream calls. A local family MUST NOT bypass the pipeline for anything that reaches a provider:
batches and evals both go back through `dispatch_internal` for exactly this reason.

### 3.3 Failure classification

The relay's behaviour on failure is a table, not a heuristic. `classify_status` (`server.rs:5161`)
maps the HTTP status to a class, and `exhausted_by_body` (`server.rs:4794`) refines it from the body,
returning a `BodyRefusal` (`server.rs:4756`) of reason, wait and an optional scope.

The sentence itself is read by `refusal_text` (`server.rs:4778`) from `error.message`, `error.detail`,
a top-level `detail`, a top-level `message`, or an `error` that is a bare string, because providers
use all five envelopes; reading only OpenAI's shape once classified a
`403 {"detail":"User is locked. Reason: Exhausted balance. …"}` as a broken credential. Two body
statements narrow the classification further:

- `names_a_plan_limit` (`server.rs:4931`) — a plan word **and** a limit word — means the plan does
  not cover this model, so the refusal is `QuotaExhausted` scoped with
  `CooldownScope::Model` (`server.rs:4848`): the account keeps serving the models its plan does cover,
  and the combo moves to the next step at once.
- `mentions_a_spent_account` (`server.rs:4876`) means the account cannot pay. That is the reason a
  provider that says "you do not have enough credits" fails over silently instead of surfacing a
  `400`, and the refusal is parked for `SPENT_BALANCE_PARK_MS` (`server.rs:4769`, one hour) when the
  body states no reset, because re-probing a balance every minute only learns it is still gone.

An agent adding a provider whose "out of money" or "your plan does not include this model" wording is
new MUST add the phrase to `names_a_spent_balance` (`server.rs:4896`) or `names_a_plan_limit` and add a
test in the shape of `money_wording_from_other_providers_still_cools_the_account` or
`a_plan_that_excludes_the_model_cools_only_that_model`; a phrase that does not match is a request that
fails while a funded sibling account sits idle.

A cooldown is a prediction about the near future, not a fact about now. When every step is cooling
down and nothing was actually tried, the step that recovers soonest is tried anyway
(`last_resort_step`), and its real answer is what the client gets. Refusing without trying is the
worst of the available answers, because the operator sees a route that died instantly while the
provider was healthy.

### 3.4 The capability-addressed path

`forward_to_capability` (`server.rs:1055`) and `forward_with_bytes` (`server.rs:1120`) serve routes
that have no model to resolve: the fine-tuning family, containers, and the three model-optional
routes. Their rules are different from the model-bound path in ways an agent must not blur:

- The provider is the **first** one declaring the capability, or the one named by `?provider=`. There
  is no combo and no resolution rank.
- The base URL, dialect, auth and timeouts come from `provider.steps.first()`. A provider with several
  steps is not load-balanced on this path; the first step is the provider's identity.
- The body is **never translated**. These routes are OpenAI-shaped by definition; a provider that
  cannot speak them should not declare the capability, and the validator enforces the obvious cases.
- The account is chosen by the same `select::account_candidates` as any other request, so the
  provider's account policy still applies.
- The upstream status and body are returned as-is. There is no local envelope around a provider
  answer.
- A connect failure does not cool the account here, because there is no per-account health record for
  a route that may not have one. This is a known asymmetry (§5.3).

The fine-tuning creation path adds the credit gate before it calls the forwarder (§3.6), and the body
shaper stages the training file (§3.9). Nothing else on this path is special.

### 3.5 Dialect translation

Translation is hub-and-spoke with Chat Completions as the pivot. `translate_request` returns
`Ok(None)` when no translation is needed, which is the signal to forward the original bytes; a direct
pair (`open-ai → open-ai`, `anthropic → anthropic`) is always verbatim. Responses are translated by
`translate_response_body`, and streamed events by `translate_sse_data`.

An agent adding a dialect MUST implement the pair through the pivot rather than adding a direct
translation for each pair, or the number of translations grows quadratically. The existing dialects
and their capabilities are in `docs/24`; the trait surface is `mr-core/src/dialect.rs`. A dialect that
cannot express a capability MUST NOT claim it: `dialect_capability_mismatches` rejects the
configuration at load time, which is earlier and more legible than a request-time failure.

Translation is not a normalisation layer. A field the target dialect cannot express is dropped rather
than emulated with something that means something else, and a request that cannot be expressed is
refused with a specific error instead of being sent in a shape the provider will misinterpret.

### 3.6 Account selection and the fine-tuning credit gate

Account selection is `select.rs`: pure functions over the snapshot, with the ranking policy applied
only at creation time. `ProviderPlan.accounts` order MUST NOT be re-sorted; only the local `usable`
list is sorted, and the ranking applies to job creation. This is what makes a `RoundRobin` policy
stable across requests instead of depending on a sort's tie-breaking.

The fine-tuning gate is the one place where a route refuses for money before it contacts anyone
(`server.rs:1203-1240`). Its rules:

- The floor is **strictly greater than** `$15.00` (`FINE_TUNING_CREDIT_FLOOR_CENTS = 1_500`). Exactly
  `$15.00` is refused; `$15.005` (1 501 cents) is accepted. This is deliberate: "over 15 dollars" means
  over, and a floor that admits its own boundary is a bug waiting to be reported.
- Only an explicitly declared USD balance compares. A declared `credit_usd` wins over a cached USD
  quota no older than 24 hours (`FINE_TUNING_CREDIT_MAX_AGE_MS`). A percentage, an "unlimited" marker,
  an unreported balance, a missing row, and a future-stamped row are all **unknown**, and unknown
  never qualifies. A declared `Some(0)` is a declared zero, which is different from unknown.
- The gate walks steps in order and takes the **richest qualifying account within the first funded
  step**. It does not pool balances across steps. A step with no funded account is skipped, and the
  next step is considered; when no step has one, the answer is `402`/`403 insufficient_credit` naming
  the floor.
- An OAuth fine-tuning account cannot be balance-probed by the relay (`ShapeTarget.credential ==
  None`), so only a declared or cached balance can qualify it. This is a limitation of the credential
  shape, not an invitation to guess.

A created job is never resubmitted to another account. If the provider accepted the job, the job
exists; retrying it elsewhere would double-spend. The relay therefore does not fail a created job over
for money. What it does do is stage the training file before creation (§3.9) and refuse before
creation when the account cannot pay.

### 3.7 Limits, timeouts and quota

Per-step timeouts come from the snapshot and have floors that exist for GPU cold starts:

| Setting | Floor | Behaviour at `0` |
| --- | --- | --- |
| `connect_timeout_ms` | — | framework default |
| `ttfb_timeout_ms` | 300 000 ms | — |
| `total_timeout_ms` | 600 000 ms | no total timeout |
| `idle_timeout_ms` | — | no idle timeout |

A local vLLM behind a GPU that scales from zero can take minutes to answer the first request; a
five-second TTFB would fail every cold start and look like a provider outage. The floors are
deliberate and an agent MUST NOT lower them to make a test faster.

Quota (`quota.rs`) is per client key: a rolling window of requests and tokens, plus a flat request
limit. The reservation is one request plus a prompt-token estimate of `chars / 4`; the completion is
not reserved, so a token cap can be overshot by the one request that crosses it. Only capped keys are
metered; an uncapped key writes nothing. Windows are 5 hours, 1 week, and 30 days. A refused request
writes nothing, which is what makes a `429` safe to retry.

### 3.8 Local state and durability

Persistent SQLite tables: `account_health`, `usage`, `oauth_credential`, `oplog`, `key_usage`,
`provider_quota_state`, `usage_tokens`, `key_usage_bytes`, `gpu_usage`. Everything else lives as files
under `state_dir()`: files and uploads, batches, stored responses, evals, vector stores.

Two rules an agent must preserve:

- **An id minted by the relay is never a path.** A filename from a client is never joined into a path;
  the relay mints `file_…`, `batch_…`, `vs_…`, `eval_…` ids and looks them up. This is the guarantee
  that a malicious filename cannot escape the state directory.
- **A stored response is stored only when it is whole.** `store: true` on a non-streamed response
  writes the body only if it is a single JSON object. A streamed response is not stored at all. A
  partial body is never written, because a later read would then serve a truncated answer as if it
  were complete.

Local writes are best-effort in the sense that a failed write is logged and the request still
completes; the client's answer does not depend on the record. The exception is a fine-tuning staging
upload, which is described next.

### 3.9 The fine-tuning seam

`stage_fine_tuning_files` (`server.rs:732`) is the only place a request body is rewritten for a
provider's benefit. A fine-tuning job names its data by a **provider** file id, but the relay serves
`/v1/files` itself, so a client's id is local (`file_…`) and no upstream can resolve it. Staging closes
that gap: for each of `training_file` and `validation_file`, if the id names a file this relay holds,
it is uploaded to the resolved step's `/v1/files` as multipart with `purpose=fine-tune`, and the id
that comes back replaces it. Every other byte of the body reaches the provider as the client wrote it.

The deliberate non-behaviours are as important as the behaviour:

- An id the relay does not hold is left alone. It may be a provider file id the client uploaded
  directly, which is a legitimate way to use the route; rewriting it would break a working request.
- A failed upload leaves the local id in place and logs why. The shaper's answer is the body to send
  and nothing else — it cannot turn the request into a relay error — so the alternative to forwarding
  is dropping the request. A provider's own "unknown file" `400` names the id and is at least true
  about where the problem is.
- An account whose credential is not a header (`OAuth`, `QueryParam`) is not staged: the upload needs
  the same authentication the job would, and inventing a second credential path here would diverge
  from the operator's configuration. The job is forwarded as it arrived.

An agent changing this MUST preserve the third rule in particular: staging with a credential the
relay cannot present would either fail every time or, worse, upload under a different identity.

### 3.10 Realtime

`GET /v1/realtime` is a websocket upgrade that the relay pumps between the client and the provider
(`realtime.rs`). It is not a request/response route: the relay forwards frames in both directions and
does not parse the realtime protocol. The two POSTs (`client_secrets`, `calls`) are the HTTP
handshake and are ordinary capability-addressed requests.

The pump has never carried real audio in a test; the tests use a local socket. An agent changing it
MUST test with a local websocket peer and MUST NOT claim vendor interop from a unit test. The
capability's doc comment (`dialect.rs:390`) states that the upgrade is proxied and why it has no
`RouteKind`; it said the opposite until recently, so an agent editing either the comment or the
handler MUST keep the two in agreement.

### 3.11 The batch lane

A batch is one metered submission: one file, one `completion_window`, and items that are each a
`/v1/chat/completions` request. The limits are in `batch.rs:161-300`, the cost ceiling at `:2674`, and
they are the contract:

- `MAX_FILE_BYTES` 32 MiB, `MAX_ITEMS` 50 000, `MAX_ITEM_BODY_BYTES` 16 MiB.
- `MAX_ATTEMPTS` 3; a `4xx` is never retried, because it will fail the same way.
- `DEFAULT_WORKERS` 4, overridable with `MANAROUTER_BATCH_WORKERS` (1–64).
- `ADMISSION_WAIT` 300 s, `RETRY_BASE` 250 ms, `RETRY_CAP` 8 s.
- `MAX_COST_CEILING` $1000, a guard against a misconfigured price turning a batch into a runaway.
- `completion_window` is required and only `"24h"` is accepted. It is recorded, **not enforced**: the
  relay does not expire a batch at 24 hours. An agent MUST NOT claim enforcement.

Each item goes through the ordinary pipeline, including the capability gate: `dispatch_batch_item`
(`server.rs:6327`) calls `dispatch_routed` with the item's authorization and admission already taken,
so an item gets the same routing, selection and gate as a direct request. That is what makes "a batch
item is not a bypass" true; per-item isolation is pinned by
`a_batch_answers_in_submission_order_and_isolates_its_failures` (`batch.rs:3503`).

## 4. Recipes

### 4.1 Add an OpenAI route that proxies to a provider

1. Add the path to `route_kind_for` (`server.rs:5512`) if it is model-bound, and add a `RouteKind`
   variant with its `capability()` mapping if it is new. The route kind is the single source of truth;
   do not add a second table.
2. Add the path to the router in `server.rs` (or to the owning module if it belongs to a local
   family). Remember `Router::merge` panics on a duplicate.
3. Add the path to the documented list in `the_documented_route_list_is_the_one_registered`
   (`server.rs:9023`) and update the count assertion. A path that is mounted but undocumented, or
   documented but not mounted, fails that test.
4. Add the capability to `Capability` if it is new, with `as_str`, `parse`, the serde form, and the
   dialect validation. Then add it to the provider templates that genuinely serve it.
5. Add the path to `docs/18` and `docs/29`, and to §2 of this document. The docs are part of the
   change, not a follow-up.
6. Test: a request with no declaring provider is refused with the gate's `404`; a request with one is
   forwarded to a fake upstream and its body arrives byte for byte; a streaming variant emits SSE.

### 4.2 Add a locally served family

1. Create `crates/manarouter-relay/src/<family>.rs` with a `routes() -> Router<AppState>` function,
   following `vector_stores.rs` or `uploads.rs`.
2. Merge it in `router()` **before** `.with_state`, like the existing families.
3. Authenticate every handler the way the rest of the surface does — `presented_key` then
   `authenticate_request` (`server.rs:4617`, `:4645`), as `vector_stores.rs::authorize` (`:2531`)
   does. A local family is not exempt from the client key.
4. Mint ids, never paths (§3.8). Store under `state_dir()`.
5. If the family makes provider calls, dispatch them through `dispatch_internal` so the gate, quota,
   selection and translation all apply. Do not call the upstream client directly.
6. Add the paths to the documented list and to §2; add the limits to Appendix C.

### 4.3 Add a provider that supports only one family

1. Add a provider template with only the capabilities it serves. Do not add a placeholder for a family
   it does not.
2. Use its real dialect. If it is OpenAI-shaped for that family, use `open-ai` and declare only the
   relevant capability; the validator allows an OpenAI dialect to declare a subset.
3. If its dialect is not OpenAI-shaped, check `dialect_capability_mismatches`: it will reject an
   OpenAI-only capability on a non-OpenAI dialect, which is the correct outcome.
4. Add a test that the family is served and a sibling family is refused with `404
   unsupported_endpoint`. That pair is the whole contract of a partial provider.
5. Document it in `docs/24` and in the provider's own entry; do not add a new document for one
   provider.

### 4.4 Add a capability

1. Add the variant to `Capability` (`dialect.rs:340`), with a doc comment that says what family it
   covers and whether it is proxied, served here, or refused.
2. Add `as_str`, `parse` (accepting dotted and hyphenated spellings), and the serde form. The serde
   form is what is written to `config.json`; the diagnostic form is for messages.
3. Add the `RouteKind::capability()` arm if a route requires it. If the capability is catalogue-only
   (no route requires it), say so in the doc comment and do not invent a route to justify it.
4. Add the dialect rule: which dialects may declare it. Add it to `dialect_capability_mismatches` if
   an OpenAI-only family.
5. Add the provider template declarations and the panel's capability list.
6. Add tests: parse round-trip, serde round-trip, gate refusal, and validation rejection for a
   mismatched dialect.

### 4.5 Change the snapshot

`SNAPSHOT_VERSION` is in `crates/mr-core/src/lib.rs`. Any change to the encoded shape requires a bump
and a matching encode/decode pair in `snapshot.rs`:

1. Bump `SNAPSHOT_VERSION`.
2. Add the field to `AccountPlan`/`ProviderPlan`/whatever plan owns it, with a doc comment.
3. Add it to `encode_*` and `decode_*` in the same position, or use a named field encoding if the
   surrounding code does. The round-trip test asserts order; a mismatch is a decode that silently
   reads the wrong field.
4. Update every construction site. A new `AccountPlan` field breaks `E0063` in the CLI, the panel
   backend and the tests; that is the compiler doing the work, not a nuisance.
5. Update `docs/LLM_INDEX.md` (it names the current version) and any doc that states it.

### 4.6 Add a client-facing error

1. Choose a code from Appendix B if one fits. A new code is a new contract; do not add one for a case
   an existing code describes.
2. Emit it with `error_response(status, dialect, code, message)`, passing the **client's** dialect, so
   an Anthropic client gets an Anthropic envelope.
3. Write the message as a full sentence that names the thing the operator must change. "unsupported"
   alone is not a message.
4. Add it to Appendix B and to the table in §1.8 if it is likely to be met.
5. Test the status, the code, and the dialect of the envelope.

### 4.7 Write the test that pins a contract

A contract test in this repository states the reason in its name and asserts the smallest observable
fact. Prefer:

- a fake upstream that records the request it received, for byte-fidelity claims;
- a local socket for streaming and websocket claims;
- a call to `router(state)` for mounting claims;
- a snapshot built in the test for resolution and gate claims.

Do not assert on a log line. Do not assert that a request "works" by checking only a `2xx`; assert the
body the upstream received, or the body the client received. A test that would pass with the feature
removed is not a test of the feature.

## 5. Invariants and known deviations

### 5.1 Invariants that must not regress

These are stated as guarantees in `docs/23` §4.2 and are each pinned by a test:

1. **A batch item is not a bypass.** It passes the same gate and resolution as a direct request.
2. **A failed fine-tuning upload stays failed.** The relay never invents a provider id or pretends the
   upload succeeded.
3. **A fine-tuning job is created only on an account that can provably pay.** Unknown balance is not
   a yes.
4. **A stored response is stored only when it is whole.** No partial body is ever written.
5. **A filename is never a path.** Ids are minted; lookups are by id.
6. **`/v1/search` is decided before any lookup.** A body naming a model goes to the provider even if
   it also names a store.

Three more that follow from the code and are easy to break:

7. **Bytes are forwarded unless there is a reason to rewrite them.** A new feature that parses and
   re-serialises every body has broken the latency argument and probably a field the provider needed.
8. **The capability gate is one table.** `RouteKind::capability()` is the only route-to-capability
   direction; the panel and the relay must not grow a second one.
9. **The relay does not guess a model.** A bare id that two providers serve is a `404`, not a
   coin flip.

### 5.2 Known deviations from OpenAI's wire

An agent writing a conformance suite should know these, and an agent fixing one should remove its row.

| # | Deviation | Where | Status |
| --- | --- | --- | --- |
| 1 | An unregistered path returns an empty `404`, and a wrong method on a local path an empty `405`; neither has a JSON envelope. | framework default, no fallback | deliberate |
| 2 | A proxied path accepts any method and forwards it; `/v1/models` answers every method. | `router()`, `any(...)` | deliberate; a local route must declare its methods |
| 3 | Body-read failures (`413`, `400`) always use the OpenAI envelope, even on `/v1/messages`. | `BodyError::into_response`, `server.rs:85` | known defect |
| 4 | No idempotency: `Idempotency-Key` is honoured nowhere. | — | deliberate (§1.9) |
| 5 | `DELETE /v1/models/{id}` always `400`s; nothing is forwarded. | `delete_model`, `server.rs:616` | deliberate |
| 6 | The model listing does not advertise capabilities. | `GET /v1/models` | known limitation (§5.3) |
| 7 | A model name written with JSON escapes is not decoded for routing. | `model_scan.rs` | known limitation |

Deviation 3 is a defect rather than a choice, and it is left as one: fixing it requires threading
the client's dialect into body reading, which happens before dialect dispatch. An agent that fixes it
MUST keep the OpenAI envelope on OpenAI routes and add a test for the Anthropic one. Deviations 6 and
7 are known limitations, argued in §5.3 rather than left as surprises.

### 5.3 Gaps stated honestly

- **No job-to-account affinity.** A created fine-tuning job lives on the account that created it, but
  the relay does not remember which one. A later `GET /v1/fine_tuning/jobs/{id}`, its events, a pause,
  or a cancel is an ordinary capability-addressed request, and with a `RoundRobin` policy the next
  request may be answered by a different account of the same provider. The provider then answers `404`
  and the relay forwards it. The fix is a job-id → account map, which is a state change and a snapshot
  change; until it exists, an operator with several accounts on one fine-tuning provider should pin
  the provider or use one account.
- **Capabilities are not discoverable from `/v1/models`.** A client learns that a route is unavailable
  by making the request and reading the `404`. The listing has the data to describe a model's
  modalities and levels, but not the provider's declared families, because a model can be served by
  several providers with different declarations.
- **A capability-addressed connect failure does not cool the account.** The model-bound path records
  health per account; the capability-addressed path has no equivalent record. A provider that is down
  is therefore retried on the next request rather than being parked, which is safe but noisier.
- **No live-vendor verification for several families.** The wire shapes are tested against fake
  upstreams. The realtime pump has never carried real audio; the fine-tuning staging path has never
  hit a real file API; no real grader model has been called by an eval. `docs/23` §5 lists these; do
  not upgrade a claim without a live test.
- **`deploy/modal-finetune/` has never been deployed.** It is a reference trainer with its own tests;
  no GPU has run it. Do not describe it as a working deployment.

## Appendix A — Route kinds and the capabilities they require

`RouteKind` has 19 variants; `Capability` has 22. The difference is not an accident: some capabilities
describe a model's behaviour rather than a route, and some families are served locally and have no
route kind.

| `RouteKind` | Capability | Path |
| --- | --- | --- |
| `ChatCompletions` | `chat` | `POST /v1/chat/completions` |
| `Responses` | `responses` | `POST /v1/responses` |
| `Embeddings` | `embeddings` | `POST /v1/embeddings` |
| `AudioTranscription` | `audio.transcription` | `POST /v1/audio/transcriptions` |
| `AudioSpeech` | `audio.speech` | `POST /v1/audio/speech` |
| `AudioTranslation` | `audio.translation` | `POST /v1/audio/translations` |
| `ImagesGenerations` | `images.generations` | `POST /v1/images/generations` |
| `ImagesEdits` | `images.edits` | `POST /v1/images/edits` |
| `ImagesVariations` | `images.variations` | `POST /v1/images/variations` |
| `Moderations` | `moderations` | `POST /v1/moderations` |
| `VideoCreate` | `video` | `POST /v1/videos` |
| `VideoRetrieve` | `video` | `GET /v1/videos/{id}` |
| `VideoContent` | `video` | `GET /v1/videos/{id}/content` |
| `RealtimeClientSecrets` | `realtime` | `POST /v1/realtime/client_secrets` |
| `RealtimeCalls` | `realtime` | `POST /v1/realtime/calls` |
| `Messages` | `messages` | `POST /v1/messages` |
| `CountTokens` | `messages` | `POST /v1/messages/count_tokens` |
| `Models` | `models` | `GET /v1/models` (served locally; the declaration is catalogue-only) |
| `Search` | `search` | `POST /v1/search` (only when the body names a model) |

Capabilities with no `RouteKind`: `tool-calling`, `reasoning`, and `vision` describe what a model can
do and are not gate-checked; `vector-stores` and `evals` are served here and never reach a provider;
`fine-tuning` and `containers` are capability-addressed families that are not route kinds because
there is no model in their path.

## Appendix B — Local error codes

Every code below is generated by the relay, not forwarded from a provider.

| Code | Typical status | Raised by |
| --- | --- | --- |
| `invalid_api_key` | 401 | `auth_error` |
| `invalid_request_error` | 400 | body read, missing model, malformed local request |
| `model_not_found` | 404 | resolution, `GET /v1/models/{id}` |
| `unsupported_endpoint` | 404, 501 | capability gate, containers |
| `payload_too_large` | 413 | `BodyError::TooLarge` |
| `no_usable_accounts` | 429 | no selectable account for a step |
| `insufficient_credit` | 402/403 | fine-tuning credit gate |
| `rate_limit_error` | 429 | quota denial |
| `proxy_error` | 502 | upstream transport failure |
| `upstream_error` | varies | upstream failure with no better classification |
| `server_error` | 500 | internal failure |
| `internal_error` | 500 | internal failure |
| `modality_fallback_unavailable` | 501 | no step carries the modality and the default is unusable |
| `account_placeholder_unfilled` | 400 | an account placeholder was not filled in configuration |
| `vault_error` | 500 | the credential vault could not be read |
| `kv_budget_exceeded` | 429 | the KV budget for a batch/route was exceeded |

A provider's own code (for example `unsupported_reasoning_effort`) is not in this table because the
relay forwards it; see §1.8.

## Appendix C — Limits

| Limit | Value | Where |
| --- | --- | --- |
| Request body | 32 MiB | `MAX_BODY_BYTES`, `server.rs:67` |
| File upload | 32 MiB | `MAX_FILE_BYTES`, `batch.rs` |
| Batch items | 50 000 | `MAX_ITEMS`, `batch.rs` |
| Batch item body | 16 MiB | `MAX_ITEM_BODY_BYTES`, `batch.rs` |
| Batch attempts | 3 | `MAX_ATTEMPTS`, `batch.rs` |
| Batch workers | 4 (1–64) | `DEFAULT_WORKERS`, `MANAROUTER_BATCH_WORKERS` |
| Batch admission wait | 300 s | `ADMISSION_WAIT`, `batch.rs` |
| Batch cost ceiling | $1000 | `MAX_COST_CEILING`, `batch.rs` |
| Upload size | 32 MiB | `MAX_UPLOAD_BYTES`, `uploads.rs` |
| Upload TTL | 86 400 s | `UPLOAD_TTL_SECONDS`, `uploads.rs` |
| Upload JSON body | 1 MiB | `MAX_JSON_BODY_BYTES`, `uploads.rs` |
| Upload filename | 1 024 B | `MAX_FILENAME_BYTES`, `uploads.rs` |
| Upload purpose | 64 B | `MAX_PURPOSE_BYTES`, `uploads.rs` |
| Stored response | 8 MiB | `MAX_STORED_BYTES`, `responses.rs` |
| Response id | 128 B | `MAX_ID_BYTES`, `responses.rs` |
| Vector chunk target | 800 chars | `CHUNK_TARGET`, `vector_stores.rs` |
| Vector chunk overlap | 100 chars | `CHUNK_OVERLAP`, `vector_stores.rs` |
| Embedding batch | 64 | `EMBED_BATCH`, `vector_stores.rs` |
| Vector chunks per store | 200 000 | `MAX_CHUNKS`, `vector_stores.rs` |
| Vector text | 100 MiB | `MAX_TEXT_BYTES`, `vector_stores.rs` |
| Search results | 50 max, 10 default | `MAX_SEARCH_RESULTS`, `DEFAULT_SEARCH_RESULTS` |
| List page | 100 max, 20 default | `MAX_LIMIT`, `DEFAULT_LIMIT` |
| Files per batch | 500 | `MAX_BATCH_FILES`, `vector_stores.rs` |
| Stores per search | 100 | `MAX_SEARCH_STORES`, `vector_stores.rs` |
| Metadata | 16 pairs, 64-char key, 512-char value | `vector_stores.rs` |
| Eval response | 8 MiB | `MAX_RESPONSE_BYTES`, `evals.rs` |
| Fine-tuning floor | > $15.00 (1 500 cents) | `FINE_TUNING_CREDIT_FLOOR_CENTS`, `server.rs:921` |
| Fine-tuning credit age | 24 h | `FINE_TUNING_CREDIT_MAX_AGE_MS`, `server.rs` |
| Quota window | 5 h / 1 week / 30 days | `quota.rs` |
| Quota prompt estimate | 4 chars/token | `CHARS_PER_TOKEN`, `quota.rs` |

## Appendix D — Tests worth reading before changing behaviour

| Test | Pins |
| --- | --- |
| `the_documented_route_list_is_the_one_registered` (`server.rs:9023`) | The 51-path surface is mounted. |
| `no_path_is_registered_twice` (`server.rs:9115`) | No duplicate mount, especially `/v1/search`. |
| `only_the_three_routes_with_a_documented_default_accept_a_missing_model` (`server.rs:14385`) | Model-optional routing. |
| `money_wording_from_other_providers_still_cools_the_account` (`server.rs:8880`) | The spent-balance phrase list. |
| `a_batch_answers_in_submission_order_and_isolates_its_failures` (`batch.rs:3503`) | Per-item isolation. Every item is dispatched by `dispatch_batch_item` (`server.rs:6327`), which calls `dispatch_routed`, so an item goes through the same routing, selection and capability gate as a direct request. |
| `fifteen_dollars_is_not_more_than_fifteen_dollars` (`server.rs:13941`) | `$15.00` refused, `$15.005` accepted. |
| `a_fine_tuning_job_is_refused_before_any_dial_when_no_account_is_funded` (`:13908`) | Refusal precedes every provider side effect. |
| `a_spent_account_is_silently_replaced_by_a_funded_sibling` (`:14011`) | Silent failover on a 402/403/429. |
| `a_cached_usd_balance_can_fund_a_job_until_it_goes_stale` (`:14098`) | The 24-hour cache rule. |
| `a_rich_snapshot_round_trips_through_encode_and_decode` (`snapshot.rs:3873`) | Snapshot encode/decode order, including the account fields. |
| `round_robin_account_selection_advances_and_wraps` (`select.rs:1363`) and `round_robin_candidates_are_a_rotation_of_the_same_set` (`:1447`) | Round-robin stability. |
| panel `node --test` suites | The panel's view of capabilities and routing. |

When a change makes one of these fail, the test is usually right and the change is the thing to
reconsider. When the change is genuinely a new contract, change the test in the same commit and say
why in its name.
