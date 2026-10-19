# OAuth: OpenAI Codex (ChatGPT subscription)

The `openai-codex` provider is not the OpenAI API. It is the ChatGPT backend that the Codex CLI
talks to, and it is reached with a ChatGPT-plan login rather than an API key. Everything in this
document is about that surface specifically: a request sent to it with the wrong headers, or with a
refresh token encoded the wrong way, fails in ways that look like an outage rather than a mistake.

The shared machinery — single-flight refresh, the 401 repair path, the loopback listener — is in
`docs/10-oauth-overview.md`. This document is the platform-specific facts.

---

## 1. Client identity

| Item | Value |
|---|---|
| `client_id` | `app_EMoamEEZ73f0CkXaXp7hrann` |
| Provenance | OpenAI's own Codex CLI. `openai/codex`, `login/src/auth/manager.rs` (`pub const CLIENT_ID`) |
| Classification | **PUBLIC (not a secret)** — a native public client; PKCE and redirect validation are the protection |
| Authorize | `https://auth.openai.com/oauth/authorize` |
| Token | `https://auth.openai.com/oauth/token` |
| Refresh | `https://auth.openai.com/oauth/token` — the same URL, **different encoding** (§3) |
| Revoke | `https://auth.openai.com/oauth/revoke` — a first-party-client fact recorded for completeness; **no code path in this build calls it** |
| Issuer | `https://auth.openai.com` (`DEFAULT_ISSUER`) |

Do not use the second literal that appears in the Codex binary, `app_69a1d78e929881919bba0dbda1f6436d`.
It is for **agent identity / workload-identity registration**, not the interactive login flow, and
presenting it at the authorize endpoint will not produce a ChatGPT credential.

Three environment overrides exist in the first-party client, for testing against a staging issuer
without recompiling: `CODEX_APP_SERVER_LOGIN_CLIENT_ID`, `CODEX_REFRESH_TOKEN_URL_OVERRIDE` and
`CODEX_REVOKE_TOKEN_URL_OVERRIDE`. **This build does not read any of them** — `codex.rs` uses the
fixed constants in `crates/mr-core/src/published.rs`, so there is no staging affordance here.

---

## 2. Flow: authorization code with PKCE

The default flow is authorization code + PKCE (`S256` only), loopback redirect, no client secret.
Codex also supports a **device-code** flow (§2a), which needs no inbound connection at all.

### Redirect

| Item | Value |
|---|---|
| Primary | `http://127.0.0.1:1455/auth/callback` |
| Fallback | `http://127.0.0.1:1457/auth/callback` |
| Bind address | `127.0.0.1` only |

Both ports are registered with the provider, which is what makes the fallback work. The URI is built
from the port actually bound — `format!("http://127.0.0.1:{actual_port}/auth/callback")` — so the
value sent at authorize time and the value sent at token time always agree. If they disagree, the
token exchange is rejected even though the authorize step succeeded; see §6 of
`docs/10-oauth-overview.md` for why neither the host nor the port may be normalised.

### PKCE and `state`

- `code_challenge_method=S256` is always sent. The verifier is generated with a CSPRNG and encoded
  base64url without padding.
- `state` is 32 random bytes encoded base64url without padding, and the callback's `state` must match
  it exactly.

The challenge is the SHA-256 of the verifier, base64url, no padding. The most common implementation
mistake is sending the raw verifier as the challenge, or using standard base64 (`+`/`/`) instead of
the URL-safe alphabet. Both produce a challenge the provider will not match at token time, and the
failure is `invalid_grant` at the exchange — after the user has already completed the browser step,
which is the most expensive place to discover it.

### 2a. Device-code flow (headless and remote)

The loopback flow requires the browser to reach a listener on **the machine running the router**. A
headless server, a container, or a browser on another host cannot do that, so those deployments need a
flow with no inbound connection. Codex offers one — the same flow the Codex CLI uses — and the panel
offers it as "Use a device code instead".

This is **not** RFC 8628, and the differences are the whole difficulty:

| Step | Endpoint | Notes |
|---|---|---|
| 1. Start | `POST https://auth.openai.com/api/accounts/deviceauth/usercode` | JSON body `{"client_id":…}`. Returns `device_auth_id`, `user_code`, `interval`. |
| 2. Poll | `POST …/deviceauth/token` | JSON body `{"device_auth_id":…, "user_code":…}`. On approval returns `authorization_code` **and** `code_verifier`. |
| 3. Exchange | `POST https://auth.openai.com/oauth/token` | Form-encoded `authorization_code` grant with `redirect_uri = https://auth.openai.com/deviceauth/callback`. |

Four things that cost time to rediscover, all confirmed against the live endpoint:

- **The poll needs the `user_code` as well as the device id.** Omitting it is
  `400 … user_code: Field required`, not a pending status. A wrong code is
  `400 deviceauth_user_code_mismatch`. This is why `device_poll` takes the whole
  `DeviceAuthorization` rather than just `device_code` — GitHub's RFC 8628 flow needs only the device
  code, and a signature passing one string cannot serve both.
- **The pending state is `403`** with `code: deviceauth_authorization_pending`.
- **PKCE is generated server-side** and returned with the authorization code. This flow neither sends
  a `code_challenge` nor stores a verifier; step 2's `code_verifier` is used verbatim at step 3.
- **`interval` is a string** (`"5"`), though the CLI types it as a number. Parse both.

A `404` at step 1 means device login is disabled for the account or workspace by an administrator.
That is reported as a configuration outcome with the browser sign-in suggested, not as a failure.

The step-3 response is turned into the same `TokenSet` the browser flow produces, including
`chatgpt_account_id` in `extra` — without it every relay request is a hard `400`, so `device_poll`
refuses to report success when the claim is missing rather than storing a credential that cannot work.

### Scopes

```
openid profile email offline_access api.connectors.read api.connectors.invoke
```

| Scope | Why it is there |
|---|---|
| `openid` | required for an OIDC id_token |
| `profile`, `email` | account display and identity |
| `offline_access` | **this is what yields a refresh token** |
| `api.connectors.read`, `api.connectors.invoke` | the connectors feature |

Two notes on the last three.

`offline_access` is not optional decoration. Without it the token endpoint returns an access token
with no refresh token, and the account silently requires a fresh interactive login every hour. The
symptom is not an error; it is an account that works until it doesn't.

The `api.connectors.*` scopes are **version-dependent and UNCERTAIN**. Current Codex `main` sends
them; older Codex shipped only `openid profile email offline_access`. Whether they are accepted for
all accounts is unverified — they may be gated by plan. The consequence of being wrong in either
direction is asymmetric: omitting them breaks the connectors feature, while sending them to a server
that rejects unknown scopes would break login entirely. The implementation sends them, matching
current first-party behaviour, and this is the point to revisit if a login starts failing at the
authorize step with an `invalid_scope` error.

### Extra authorize parameters

| Parameter | Value | What it does |
|---|---|---|
| `id_token_add_organizations` | `true` | asks the id_token to carry organization/workspace claims |
| `codex_cli_simplified_flow` | `true` | selects the simplified consent UI the CLI uses |
| `originator` | `codex_cli_rs` | identifies the first-party client; also sent as an inference header (§5) |

`originator`'s default is `codex_cli_rs` in the first-party client, overridable there via
`CODEX_INTERNAL_ORIGINATOR_OVERRIDE`. The router uses the default value and offers no override: the
value is part of the fingerprint, and a configurable fingerprint is a fingerprint that will be
configured wrong.

`resource` is **not** used for the ChatGPT flow, and `allowed_workspace_id` is added only when a
workspace is forced.

---

## 3. The exchange and the refresh use different encodings

This is the trap in this platform, and it is worth stating before anything else about tokens.

| Call | URL | Encoding | Body |
|---|---|---|---|
| Code exchange | `https://auth.openai.com/oauth/token` | `application/x-www-form-urlencoded` | `grant_type=authorization_code`, `client_id`, `code`, `redirect_uri`, `code_verifier` |
| Refresh | `https://auth.openai.com/oauth/token` | `application/json` | `grant_type=refresh_token`, `client_id`, `refresh_token` |

The same endpoint accepts form encoding for the authorization-code grant and **JSON** for the
refresh grant. It is not a fallback: the first-party client has an explicit `TokenEncoding` switch
with `Form` for the exchange and `Json` for the refresh, and a source comment states the split is
deliberate.

If you get it wrong and send the refresh form-encoded, the server rejects it. The damage is worse
than a failed call. A refresh failure moves the account to `auth-failed` and a long cooldown
(`docs/10-oauth-overview.md` §7), so a purely local encoding mistake takes a working account out of
service and asks the operator to re-authenticate — and if they do, the new login works and the cause
is never seen. A form-encoded refresh is the single most likely way to lose an afternoon to this
integration.

The inverse mistake — sending a JSON code exchange — fails immediately at login, which is at least
easy to diagnose.

---

## 4. Tokens, expiry and refresh

### What the token endpoint returns

The response carries `access_token`, `refresh_token`, `id_token` and `expires_in`. The first-party
client persists the raw response plus the extracted `account_id` to `auth.json`, and that is the
shape to store here too — the raw tokens and the parsed identity, in the vault, never in
`config.json` or the database (`docs/10-oauth-overview.md` §6).

The access token and the id_token are JWTs. The expiry is read from the standard `exp` claim rather
than trusted from `expires_in`, because `exp` is what the upstream will actually enforce.

### The refresh window is 5 minutes

`CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5`. The credential is refreshed when the access token
has less than five minutes left. The first-party client also runs a periodic refresh check
(`TOKEN_REFRESH_INTERVAL = 8`); the dossier does not resolve whether that constant is seconds or
minutes, and it does not matter here — the tick is a scheduling detail and the window is the
substantive number.

Five minutes is short. Why that is acceptable here, and why the single-flight guard is mandatory
rather than defensive, is the subject of §4 and §5 of `docs/10-oauth-overview.md`. The short version:
with a five-minute budget one transient failure consumes the window and the next refresh is
triggered by a user-visible `401`, which is exactly when concurrent requests are most likely to all
try to refresh at once.

### The refresh token is single-use

A successful refresh returns a **new** refresh token and invalidates the one presented. Every one of
the following is a permanent failure that requires a fresh interactive login:

- presenting an already-used refresh token (reuse);
- presenting an expired refresh token;
- presenting one the server has invalidated;
- a refresh response whose account does not match the credential being refreshed.

The first-party client classifies these explicitly, along with `invalid_grant` on HTTP 400 and any
HTTP 401, as permanent; every other error is retryable. The router keeps its own classification —
`auth-failed` plus a long cooldown rather than deletion — because a transient upstream fault and a
revoked grant are indistinguishable from the client, and deleting the account would be the
destructive reading of an ambiguous signal.

The practical rules that follow:

1. Never send two refreshes concurrently with the same token. The single-flight guard is not
   optional.
2. Persist the new refresh token **before** treating the old one as spent. If the process dies
   between rotation and persistence, the stored token is dead and only a re-login recovers it.
3. Never retry a refresh with the token that just failed. A retry after a lost response presents a
   token the server has already rotated, which is indistinguishable from theft.
4. Do not treat a refresh failure as transient without knowing which error it was. The four cases
   above are permanent; classifying them as retryable produces a refresh loop that burns the window
   and never succeeds.

---

## 5. Account identity comes from the JWT

The account identity is not an optional field to fill in later. A Codex inference request must carry
`chatgpt-account-id`, and the only correct source for it is the JWT the login returned.

| Claim | Location | Use |
|---|---|---|
| `chatgpt_account_id` | namespace claim `https://api.openai.com/auth` | **becomes the `chatgpt-account-id` header** |
| `chatgpt_account_user_id` | same namespace | account-scoped user id |
| `email` | id_token, flat | display |
| `chatgpt_plan_type` | id_token, flat | `free` / `plus` / `pro` / `business` / `enterprise` / `edu`, possibly other values |
| `chatgpt_user_id` | id_token, flat | user id |
| `chatgpt_account_is_fedramp` | id_token, flat | deployment flag |
| `exp` | either JWT | expiry |

Decode the payload with `base64url` **without padding**. A padded decoder will reject the segment; a
standard-base64 decoder will mis-handle `-` and `_`.

The account id is also the **multi-account key** for this platform: two logins with different
`chatgpt_account_id` values are two accounts, and re-authenticating the same value replaces the
credential in place. Storing the id from the wrong claim is not cosmetic — it produces a header the
backend rejects, and it also makes re-login create duplicate accounts instead of refreshing one.

---

## 6. Inference: the endpoint and the fingerprint

```
POST https://chatgpt.com/backend-api/codex/responses
```

The catalogue records this as `base_url` for `openai-codex` with dialect `codex`
(`catalog/providers.toml`). The route is the Responses API, not Chat Completions.

### Headers

| Header | Value | Required? |
|---|---|---|
| `Authorization` | `Bearer <access_token>` | yes |
| `chatgpt-account-id` | the `chatgpt_account_id` claim | yes — a literal or wrong value is a hard failure |
| `originator` | `codex_cli_rs` | yes for the official endpoint |
| `User-Agent` | `codex_cli_rs/<version> (<OsType> <os version>; <arch>) <terminal-ua>` | yes |
| `Content-Type` | `application/json` | yes |
| `Accept` | `text/event-stream` | yes for the streaming response |
| `OpenAI-Beta` | `responses=experimental` | **UNCERTAIN** — see below |
| `x-codex-installation-id` | a per-install UUID | optional; also carried in `client_metadata` |
| `x-codex-turn-state` | replayed from the previous response | optional; sticky routing only |
| `x-codex-window-id`, `x-codex-routing-hint`, `x-codex-turn-metadata`, `x-codex-parent-thread-id` | Codex telemetry/routing | optional |
| `x-openai-subagent`, `x-openai-memgen-request` | internal sub-agent/memory flows | optional |

> **UNCERTAIN: `OpenAI-Beta`.** Older Codex releases sent `OpenAI-Beta: responses=experimental` on
> the HTTP responses call. Current `main` only sets an `OpenAI-Beta` value on the **WebSocket** path
> (`responses_websockets=2026-02-06`), and the HTTP path's requirement is not confirmed. The
> catalogue ships `responses=experimental` "for compatibility", which means the implementation
> currently sends a header the current first-party client may not send. If that is wrong, it is the
> exact thing §5 of `docs/21-security.md` warns about: an over-complete fingerprint is as detectable
> as a missing one. The safe resolution is empirical — remove the header, and if the endpoint still
> answers, leave it out. Do not assume either way.

### Why the fingerprint must be exact

This is the ChatGPT subscription backend, not the public API. It is reached by one first-party client,
and it is entitled to assume the request came from that client. Two directions of error:

- **A missing header** that the client always sends makes the request visibly not-first-party.
- **An extra header** that the client never sends does the same, and is easier to introduce by
  accident — a generic `x-request-id`, a router-specific `x-manarouter-account`, a tracing header
  injected by an HTTP library.

So the header set is not "whatever is needed to make it work". It is the fingerprint the first-party
client presents, and the rule is to send exactly that. The catalogue records the required names and
`crates/mr-core/src/catalog.rs` has a test that asserts they are present
(`the_three_self_implemented_oauth_platforms_carry_their_fingerprints`); the header values that are
templated from the credential are written `${name}` in the catalogue and filled by the OAuth layer at
request time.

A related trap: `chatgpt-account-id` is listed as a template, `${chatgpt_account_id}`, not a literal.
Sending the literal string, or the account id from a different account, is a hard failure rather than
a fallback to a default.

---

## 7. Request body constraints

The body is the Responses shape, with two fields hard-coded:

| Field | Value | Why |
|---|---|---|
| `store` | `false`, always | the ChatGPT-subscription privacy contract; the router hard-codes it |
| `stream` | `true`, always | the backend streams, and the client does not get to ask for a single JSON document |

Other fields are capability-dependent: `reasoning` (`{effort, summary}`), `text`, `include`,
`prompt_cache_key`, `service_tier` and `client_metadata` are set per model. `parallel_tool_calls` is
forced to `false` when the model is flagged `use_responses_lite`, because that backend cannot run
parallel tool calls.

### Non-streaming clients are served by a shim

Because `stream: true` is not negotiable, a client that asked for a single JSON response cannot be
served by passing the upstream response through. `crates/mr-dialects/src/lib.rs` models this as
`upstream_forces_stream()`: the Codex dialect always streams, so when the client is not streaming the
relay synthesises a complete JSON document from the SSE events and returns that. The client sees an
ordinary non-streaming response and never learns that the upstream streamed.

This is a correctness requirement, not an optimisation. Returning raw SSE to a client that expected
one JSON object produces a parse error at the client; the shim is what keeps a non-streaming client
working against a streaming-only upstream.

### `instructions` — UNCERTAIN, and the highest-risk guess in this integration

The ChatGPT backend requires non-empty base instructions. If you send none, the request fails — this
is the error that third-party harnesses hit. The disagreement is *where* they go.

- The **current** Codex client has no top-level `instructions` field. Base instructions are injected
  as a `BaseInstructionsFragment` **item inside `input`**.
- **Older** Codex versions sent a top-level `"instructions": "<system prompt>"` string.

> **UNCERTAIN.** It is not confirmed whether the current `/responses` HTTP endpoint still accepts a
> top-level `instructions` string. Both are recorded possibilities; neither is settled.
>
> - **What the implementation assumes:** the system prompt is placed in `input` as a `BaseInstructions`
>   item, matching the current first-party client. The catalogue does not hard-code a top-level field.
> - **What breaks if the assumption is wrong the other way** (the endpoint requires a top-level
>   `instructions` and ignores the in-`input` item): the request is accepted but runs without the
>   intended base instructions. That is worse than a hard error, because it produces a model that
>   behaves as if it has no system prompt — often subtly, sometimes not — and nothing in the response
>   says why.
> - **What breaks if the assumption is wrong in the other direction** (the endpoint rejects unknown
>   top-level fields): if the implementation defensively sends *both*, the extra field could be
>   rejected outright.
>
> The dossier's own recommendation is to put the prompt in `input` and, if it is unclear, send a
> top-level `instructions` too and verify empirically. That is the right instruction for a human
> testing against the live endpoint; it is **not** a licence to treat either form as verified. The
> reference for a working third-party translation is the local Hermes agent's
> `agent/codex_headers.py` and `agent/codex_responses_adapter.py`, which are T2 artefacts and should
> be read before changing this.

---

## 8. A first login, step by step

1. Generate a PKCE verifier (CSPRNG, base64url no-pad) and `state` (32 random bytes, base64url
   no-pad).
2. Bind `127.0.0.1:1455`; on failure try `1457`; on failure, stop and report which ports are held.
3. Build the redirect from the port actually bound: `http://127.0.0.1:{port}/auth/callback`.
4. Open `https://auth.openai.com/oauth/authorize` with `response_type=code`, `client_id`,
   `redirect_uri`, `code_challenge` (S256 of the verifier), `code_challenge_method=S256`, `state`,
   the six scopes, `id_token_add_organizations=true`, `codex_cli_simplified_flow=true` and
   `originator=codex_cli_rs`.
5. Wait for the loopback callback. Verify `state` matches; reject the request if it does not.
6. `POST https://auth.openai.com/oauth/token` as **form-urlencoded**: `grant_type=authorization_code`,
   `client_id`, `code`, `redirect_uri`, `code_verifier`.
7. Decode the returned access token's JWT payload, read `https://api.openai.com/auth` →
   `chatgpt_account_id` and `exp`.
8. Store the credential in the vault at `provider/openai-codex/account/<account-id>`, keyed by that
   account id.
9. Record the account against the provider in `config.json` — a reference to the vault id, not the
   tokens.

## 9. A refresh, step by step

1. The access token has less than five minutes left, or a request returned `401`.
2. Acquire the account's single-flight refresh slot. If another refresh is already in flight, wait for
   it and use its result; do not start a second one.
3. `POST https://auth.openai.com/oauth/token` as **JSON**: `grant_type=refresh_token`, `client_id`,
   `refresh_token`.
4. On success: read the new `access_token`, the new `refresh_token`, and `exp`; persist the new
   credential **before** releasing the slot, so no waiter can use the old token; then retry the
   request that triggered the refresh, once.
5. On failure: classify the error. Reuse, expiry, invalidation, account mismatch, `invalid_grant` on
   `400` and any `401` are permanent; everything else is retryable. Either way the account moves to
   `auth-failed`, and the permanent cases can only be cleared by an interactive re-login.

---

## See also

- [OAuth overview](10-oauth-overview.md) — single-flight refresh, the 401 path, storage
- [GitHub Copilot](13-oauth-copilot.md) — the other short-lived-token platform
- [Adding a platform](14-oauth-adding-a-platform.md) — the trait this platform implements
- [Security and credentials](21-security.md) — §5 why the fingerprint is sent exactly
- [The hedged race](07-hedged-race.md) — how a combo spends a subscription account's quota
- [providers.toml](../catalog/providers.toml) — the `openai-codex` entry and its recorded headers
- [catalog.rs](../crates/mr-core/src/catalog.rs) — the test that asserts the fingerprint headers
- [mr-dialects lib.rs](../crates/mr-dialects/src/lib.rs) — `upstream_forces_stream()` and the shim
