# OAuth: Claude Code (Claude subscription)

The `claude` OAuth platform is how a **Claude subscription** reaches Anthropic's API. It is not the
Console API-key login: the credential is an `sk-ant-oat01-…` subscription token, it is presented as
`Authorization: Bearer` rather than `x-api-key`, and the request must look like the Claude Code CLI's.
A request that carries the right token but an ordinary Messages shape is refused with
`invalid_request_error`, which is why the credential and the [`claude-cli` dialect](24-provider-dialects.md)
are two halves of one feature.

The shared machinery — PKCE, `state`, the session registry, single-flight refresh, the 401 repair
path — is in [the OAuth overview](10-oauth-overview.md). This document is the platform-specific
facts: the client identity, the pasted-code flow, the JSON exchange, the refresh window, the account
identity, and the inference fingerprint.

---

## 1. Client identity

| Item | Value |
|---|---|
| `client_id` | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` |
| Provenance | Anthropic's published Claude Code client id, as shipped in `anthropics/claude-code` and used by its login command |
| Classification | **PUBLIC (not a secret)** — a native public client; PKCE and the fixed redirect are the protection. Listed in `ALLOWLIST` in `scripts/check-secrets.sh` |
| Authorize | `https://claude.ai/oauth/authorize` |
| Token | `https://api.anthropic.com/v1/oauth/token` |
| Refresh | `https://api.anthropic.com/v1/oauth/token` — the same URL and the **same JSON encoding** (§3) |
| Profile | `https://api.anthropic.com/api/oauth/profile` |
| Usage | `https://api.anthropic.com/api/oauth/usage` — undocumented, used by the quota reader, not by login |
| Redirect | `https://console.anthropic.com/oauth/code/callback` — **fixed, on Anthropic's origin, not on this host** |
| Scopes | `org:create_api_key user:profile user:inference` |
| Client secret | **none** |
| PKCE | required, `S256` |
| Device flow | **none** |
| Loopback redirect | **none** |

There is no staging override in this build: `crates/mr-core/src/oauth/claude.rs` reads the fixed
constants from `crates/mr-core/src/published.rs`, and no environment variable changes them.

The client id is the same one the Claude Code CLI sends. Presenting it is not impersonation in any
meaningful sense: it is the public identifier of an OAuth client, it carries no secret, and the
provider's authorization server decides what the resulting grant may do.

---

## 2. Flow: authorization code with PKCE, finished by pasting

Anthropic's authorization server requires `redirect_uri` to match a registered value, and this client
has exactly one: `https://console.anthropic.com/oauth/code/callback`. Nothing on `127.0.0.1` is
registered for it, so a loopback listener would be answered with `redirect_uri_mismatch` before the
person ever saw a consent screen. The CLI's own answer is the same: let the browser land on Anthropic's
page and ask the person to copy what it shows.

So the platform declares `paste_redirect_uri() = Some(CODE_CALLBACK)` and
`begins_with_loopback() = false`. `OAuthSessions::start_paste` binds **no port and no listener**: it
mints `state` and the PKCE verifier, records them in a `PendingKind::Paste`, parks one `ManualSlot`
under the session id, and returns the authorization URL the panel opens. `POST /complete` then blocks
until a code arrives or the ten-minute session TTL expires.

### What the browser shows

The consent screen runs on `claude.ai`. On approval, Anthropic's callback page prints a single line in
the form

```
<authorization-code>#<state>
```

The person copies that whole line into the panel's box, which is labelled **"Use this code"** and
whose placeholder is `code#state`. The panel POSTs it to
`/api/oauth/sessions/{id}/redirect` as `{"provider_id": …, "url": "…"}` — the same route every paste
flow uses, so there is one parser and one exchange path rather than two.

### What the parser accepts

`flow::parse_pasted_redirect` handles the forms a person might actually produce:

| Pasted | Treated as |
|---|---|
| `https://console.anthropic.com/oauth/code/callback?code=…&state=…` | a full callback URL → code + state |
| `code=…&state=…` | a query string → code + state |
| `code#state` | the page's own line → code + state |
| `code` alone | a bare code — **accepted**: PKCE already binds the code to this session's verifier, so the session's own `state` is used |

When the paste carries a `state`, it is compared against the session's own before anything is sent
upstream; a mismatch is a `400` and the session keeps waiting. A bare code skips that comparison
because PKCE has already bound it — a code from another sign-in cannot be redeemed with this verifier
— and the session supplies its own `state` for the JSON exchange. A second paste for one session is
refused (`a redirect was already submitted for this sign-in`), and a paste that arrives before
`/complete` starts is still found, because the slot is keyed by session id and raced against the wait
rather than living inside it.

### PKCE and `state`

Both are minted per session and never reused. The verifier is 43 characters of base64url from the
platform RNG; the challenge is its SHA-256, and `state` is 43 characters from the same source. The
authorization URL carries them as:

```
https://claude.ai/oauth/authorize?code=true
  &client_id=…
  &response_type=code
  &redirect_uri=https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback
  &scope=org%3Acreate_api_key%20user%3Aprofile%20user%3Ainference
  &code_challenge=…
  &code_challenge_method=S256
  &state=…
```

`code=true` is first and deliberate: it is what tells Anthropic's page to render the pasteable code
rather than redirect with it. A test asserts the whole URL shape, including the percent-encoded
redirect.

---

## 3. The exchange is JSON and repeats the `state`

Unlike Codex, whose exchange is form-encoded and whose refresh is JSON, Claude is JSON in both
directions. The exchange posts:

```json
{
  "grant_type": "authorization_code",
  "code": "<code from the paste>",
  "state": "<the session's state, echoed back>",
  "client_id": "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
  "redirect_uri": "https://console.anthropic.com/oauth/code/callback",
  "code_verifier": "<the PKCE verifier>"
}
```

with `Accept: application/json` and `anthropic-beta: oauth-2025-04-20`. Echoing `state` is unusual in
an RFC 6749 exchange — the code is normally bound to it server-side — and it is why this platform
overrides `exchange_code_with_state` and leaves plain `exchange_code` as an error: a stateless call
cannot supply the value. The session layer routes every Claude login through the state-carrying tail,
and the same tail completes the vehicle, the account identity and the vault write for every platform.

A successful body looks like:

```json
{
  "access_token": "sk-ant-oat01-…",
  "refresh_token": "sk-ant-ort01-…",
  "expires_in": 28800,
  "scope": "user:inference user:profile",
  "token_type": "Bearer",
  "account": {
    "uuid": "…",
    "email_address": "…",
    "organization_uuid": "…"
  }
}
```

`account` is the useful part: it names the subscription before any profile call. The token row keeps
`account_uuid`, `account_email` and `organization_uuid` in the token set's `extra` map, and
`refresh_with_context` copies them forward on every renewal so a refresh does not spend a profile
round trip. A response with no `refresh_token` keeps the presented one rather than stranding the
account.

---

## 4. Tokens, expiry and the four-hour refresh window

`expires_in` is honoured as given (eight hours in the captured shape above); `DEFAULT_TTL_SECS =
3600` is only the fallback for a response that omits it. The vault stores the whole `TokenSet` —
access token, refresh token, expiry, scopes, `extra` — encrypted, and the relay resolves it by
reference; no token is ever written to `config.json` or the snapshot.

`Claude::refresh_lead()` is **4 hours** (`REFRESH_LEAD_SECS = 14 400`). The lead is how long before
expiry the refresher starts renewing proactively, so with an eight-hour token the router renews at
roughly the halfway point.

> **UNCERTAIN — the 4-hour lead is a chosen value, not a measured one.** The CLI refreshes on its own
> schedule and the provider does not document a required lead; the value was picked to be half a
> typical subscription token's life, so a renewal happens well before any request could see an expired
> token, without spending a refresh on every request. If live behaviour shows tokens being refused
> earlier, change `REFRESH_LEAD_SECS` in `crates/mr-core/src/oauth/claude.rs`; nothing else depends on
> the number.

The refresh token is **single-use and rotating**. The refresh posts
`{"grant_type": "refresh_token", "client_id": …, "refresh_token": …}` as JSON, and a response that
includes a new `refresh_token` replaces the old one atomically with the new expiry. Reusing a
consumed refresh token is treated by the provider as token theft and can revoke the grant, which is
why the refresher is single-flight: concurrent requests that both see an expiring token share one
refresh rather than racing two uses of the same secret.

Failure classification is shared with every other platform (`refresh.rs`):

| Condition | Class | What happens |
|---|---|---|
| `invalid_grant`, `invalid_client`, `unauthorized_client`, `access_denied` | **permanent** | the account is marked `auth-failed` and the panel asks for a new login; the stored tokens are kept for inspection but never presented again |
| HTTP 401 without a code | **permanent** | same |
| 408 / 429 / 5xx / network / unknown code | **transient** | the stored token set is untouched and the refresh is retried later; one provider outage does not sign the account out |

The relay's 401 repair path applies as well: a request refused with `401` is retried once after a
forced refresh, which is the case a clock skew or an early revocation produces. See
[the overview](10-oauth-overview.md) §5–6.

---

## 5. Account identity comes from the profile

The token response already carries an `account`, and when it does, `account_identity` answers from it
without a network call: `id = account.uuid`, `email = account.email_address`, label = the email (or
`"Claude"` when the response has none). That is the common path, because a fresh exchange returns the
`account` object.

When it does not — a restored token set from an older shape, or a refresh that carried nothing — the
platform reads `GET https://api.anthropic.com/api/oauth/profile` with:

- `Authorization: Bearer <access token>` — **not** `x-api-key`,
- `anthropic-beta: oauth-2025-04-20`,
- `User-Agent: claude-code/2.1.280`.

The profile's `account.uuid` (or `account.id`) is the stable id; `account.email_address` (or
`account.email`) is the email; `account.full_name` is the preferred label. The plan is read from
`organization.rate_limit_tier`, then `organization.organization_type`, then `organization.plan`, then
a top-level `plan` — Anthropic has shipped more than one spelling, and a missing value is `None`
rather than a guess.

An account id is mandatory: a profile without one is an error, not an unnamed account. Duplicate
logins for the same subscription therefore land on the same account row rather than creating a second
one.

---

## 6. Inference: the dialect, the identity and the fingerprint

Login produces a credential; the [`claude-cli` dialect](24-provider-dialects.md) is what makes the
credential usable. The catalogue template is `claude-code` (`catalog/providers.toml`):

| Item | Value |
|---|---|
| Dialect | `claude-cli` |
| Base URL | `https://api.anthropic.com/v1` |
| Path | `/v1/messages`, and `/v1/messages/count_tokens` for the counting route |
| Credential | `Authorization: Bearer <sk-ant-oat01-…>`, from this platform's `apply_credential` |
| Model names | the API's own `claude-*` ids, from the model list |

### The fingerprint

| Header | Value |
|---|---|
| `Authorization` | `Bearer …` |
| `anthropic-version` | `2023-06-01` |
| `anthropic-beta` | `claude-code-20250219,oauth-2025-04-20` |
| `x-app` | `cli` |
| `user-agent` | `claude-cli/2.1.280 (external, cli)` |

`oauth-2025-04-20` is what accepts an `sk-ant-oat*` bearer at all; `claude-code-20250219` selects the
subscription behaviour. Neither is optional. `x-api-key` is **never** sent — a subscription token on
that header is refused, and the catalogue template has no place to put one. The user agent is built by
`mr_dialects::claudecli::user_agent()` from `quota::claude::CLAUDE_CODE_VERSION`, so the wire string
and the quota reader's pinned release cannot drift apart.

### The body

The API checks that the request came from the CLI in the **body**, not in a header: `system` must open
with the identity sentence as a separate first entry. `with_identity` enforces that idempotently, and
[the dialect document](24-provider-dialects.md) records the measurement
([anthropics/claude-code#40515](https://github.com/anthropics/claude-code/issues/40515)),
the accepted and refused forms, and the `claude-haiku-4-5` exemption. A Claude Code client pointed at
the relay already sends the sentence; it is not doubled.

### Quota

Usage is read from the undocumented `https://api.anthropic.com/api/oauth/usage` endpoint with the same
bearer and `anthropic-beta: oauth-2025-04-20`; `crates/mr-core/src/quota/claude.rs` maps its
`five_hour` / `seven_day` / `limits[]` windows. See [quota tracking](08-quota-tracking.md). Nothing in
the login path calls it.

---

## 7. Failure modes and what each one means

| Symptom | Cause | Fix |
|---|---|---|
| `redirect_uri_mismatch` on the consent screen | the authorization URL was edited to use a different redirect | the URI is fixed by the platform; do not parameterise it |
| The consent page never shows a code | the login was started from a surface that redirects instead of rendering | use the panel's Claude connect button, which asks for `code=true` |
| `the sign-in expired before a code was pasted` | the person took more than the ten-minute session TTL | start a new login |
| `400` on `/redirect`, session still waiting | the pasted `state` belongs to a different sign-in attempt | copy the line the page shows, in full; do not reuse an old code |
| `400` on `/redirect`, "already used" | a second paste for one sign-in | one paste per login; start again if the first failed |
| `invalid_grant` at the exchange | the code was reused, the PKCE verifier did not match, or the code expired | start a new login; a repeated failure means the stored verifier was corrupted, not that the account is bad |
| `platform cannot be finished by pasting a code` | a paste was attempted for a platform without `paste_redirect_uri()` | use that platform's own flow |
| `401` on inference but the login succeeded | the credential was applied as `x-api-key`, or the beta header was dropped | check the step's dialect is `claude-cli` and the template's headers survived |
| `invalid_request_error`/`Error` with a valid token | the body's `system` field does not open with the CLI identity | the dialect must be `claude-cli`, not `anthropic` |
| The account is marked `auth-failed` after a refresh | the refresh token was already consumed (permanent class) | log in again; do not retry the same refresh token |

---

## 8. A first login, step by step

1. The operator adds a `claude-code` provider and clicks Connect. The panel calls
   `POST /api/oauth/claude/paste`; the conductor creates a session, mints `state` and PKCE, parks the
   manual slot, and answers `{session_id, authorization_url}`.
2. The panel opens the authorization URL in a new tab. The person signs in to claude.ai and approves
   the request. Anthropic's callback page shows `<code>#<state>`.
3. The person pastes that line into the panel's **"Use this code"** box, which POSTs it to
   `/api/oauth/sessions/{id}/redirect`.
4. The waiting `POST /complete` receives it, checks the state, exchanges the code and the verifier for
   a JSON token set, reads the account from the response's `account` object (or the profile), writes
   the tokens to the encrypted vault, and records non-secret metadata in SQLite.
5. The panel reports the account's email and the provider is ready. `GET /api/oauth` now lists
   `claude` with `flow: "paste"` and `flows: ["paste"]`.

A refresh is invisible to the panel: the relay sees a token within its four-hour lead, refreshes it
single-flight, writes the rotated set back to the vault, and proceeds.

---

## 9. What is verified, and what is not

Verified by the suite:

- the authorization URL's exact shape, including `code=true`, the fixed redirect and `S256`;
- the JSON exchange carrying `state` and the verifier, and plain `exchange_code` refusing;
- a rotating refresh that keeps `account_*` across the renewal, and one without a replacement keeping
  the presented token;
- the profile → identity mapping, including the plan fallbacks and the missing-id error;
- the paste session binding no port, refusing a loopback-style paste, finishing with its state, and
  refusing a second paste;
- the panel's paste-only form: Connect's label, one `window.open`, the `code#state` placeholder, the
  `/redirect` body, exactly one `/complete`, and no `/authorize` or `/device-code` call;
- the dialect's header fingerprint and body rewrite, and the catalogue template matching them;
- `GET /api/oauth` reporting `flow: "paste"` for `claude`.

Not verified, and stated so the reader can weigh it:

> **No live Claude subscription login was performed for this revision.** The flow is assembled from
> the fixed client identity, the CLI's own pasted-code behaviour and the captured token/profile
> shapes, and it is pinned by tests against scripted transports. The first real login is therefore the
> thing that proves the exchange, not this document. In particular, the four-hour refresh lead (§4)
> and the exact beta set (§6) are the two values most likely to need adjustment after a live run.

---

## See also

- [The OAuth overview](10-oauth-overview.md) — the shared session, PKCE, refresh and repair machinery,
  and the pasted-code flow as a general shape.
- [Provider dialects](24-provider-dialects.md) — the `claude-cli` dialect and why the identity is a
  body rewrite.
- [Adding an OAuth platform](14-oauth-adding-a-platform.md) — the checklist a platform like this one
  follows.
- [Provider catalogue](providers/catalog.md) — the `claude-code` template.
- [Quota tracking](08-quota-tracking.md) — the usage endpoint and the plan windows it reports.
- [Security](21-security.md) — why a public client id is public, and the fingerprint discipline.
