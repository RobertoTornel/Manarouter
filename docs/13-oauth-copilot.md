# OAuth: GitHub Copilot

Copilot is the odd one of the three. It has **two** credentials where the others have one: a
long-lived GitHub OAuth token, obtained by device flow, and a short-lived Copilot session token
minted from it. Almost every mistake in this integration is a mistake about which of the two belongs
in a given header.

The shared machinery is in `docs/10-oauth-overview.md`.

---

## 1. The device flow

The device flow exists because the client cannot host a redirect. It is defined for public clients,
so there is **no client secret** — only a `client_id`. `docs/10-oauth-overview.md` §2 explains why
that is the right shape for a headless router; this section is the wire detail.

### Step 1: request a device code

```
POST https://github.com/login/device/code
Accept: application/json
Content-Type: application/json

{"client_id":"Iv1.b507a08c87ecfe98","scope":"read:user"}
```

The response carries `device_code`, `user_code`, `verification_uri`, `expires_in` and `interval`.
Show `user_code` and `verification_uri` to the operator; the `device_code` is the router's secret for
the next step and is never shown.

`read:user` is the scope the Copilot app needs. It is deliberately narrow: the flow needs to learn
which GitHub account the operator is, and nothing else. The VS Code app variant requests `repo`
instead (§2) — a much broader grant that a router has no reason to ask for.

Send `Accept: application/json`. GitHub's OAuth endpoints default to form-encoded responses in some
contexts; without the header the parse fails on a body that looks like a query string, which reads as
a malformed response rather than a missing header.

### Step 2: poll for the token

```
POST https://github.com/login/oauth/access_token
Accept: application/json

{
  "client_id": "Iv1.b507a08c87ecfe98",
  "device_code": "<device_code>",
  "grant_type": "urn:ietf:params:oauth:grant-type:device_code"
}
```

The URL is `https://github.com/login/oauth/access_token`. It is easy to mistype as a shorter
`login/oauth/access_token` path on a different host, or to drop the `/login` segment; both produce a
404 that looks like a removed endpoint rather than a typo. The contract details are in GitHub's
[device flow documentation](https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow)
and in [RFC 8628](https://www.rfc-editor.org/rfc/rfc8628).

The `grant_type` value is `urn:ietf:params:oauth:grant-type:device_code`. It is a URN, not the short
`device_code`, and GitHub rejects the short form.

### Step 3: honour the polling contract

| Response | Meaning | Action |
|---|---|---|
| `authorization_pending` | the user has not finished | wait `interval` seconds, poll again |
| `slow_down` | polling too fast | **increase the interval by 5 seconds**, then poll again |
| `expired_token` | the device code's `expires_in` elapsed | restart the flow with a fresh code |
| `access_denied` | the user refused | stop; report the refusal |
| an `access_token` | success | proceed to §3 |

The `interval` from step 1 is a **minimum**, not a suggestion. Polling faster does not shorten the
login; it triggers `slow_down`, which lengthens it. `slow_down` is a permanent increase for the
remainder of this flow — the interval grows, it does not reset after one polite pause. An
implementation that polls on a fixed short timer will spend the whole flow being told to slow down
and can exhaust `expires_in` before the user finishes typing.

The `access_token` this returns is the **GitHub** token — long-lived and `gho_`-prefixed. It is not
the credential to send to the inference endpoint. That is the next section's job.

---

## 2. Client ids, and one documented anti-example

| Client id | Identity | Confidence | Use it? |
|---|---|---|---|
| `Iv1.b507a08c87ecfe98` | the Copilot plugin GitHub App; found literally in Microsoft's bundled `copilot-language-server/dist/main.js` | **verified (T1)** | **yes — this is the default** |
| `01ab8ac9400c4e429b23` | the VS Code OAuth app, per Microsoft's own `script/setup/getToken.mts`; device flow works, but scope is `repo` | **verified (T1)** | only if the plugin app is unavailable, and note the broader scope |
| `Ov23li8tweQw6odWQebz` | a third-party client's app | **anti-example** | **no — see below** |
| `Ov23liV9UpD7Rnfnskm3` | also present in the language-server bundle, role unknown | **UNCERTAIN** | no; never observed serving device flow |

### The anti-example, and why it is written down

`Ov23li8tweQw6odWQebz` completes the device flow successfully. The GitHub token it returns is
`gho_`-prefixed, looks correct, and is accepted by the GitHub API. It then **404s on the Copilot
token exchange** — `GET https://api.github.com/copilot_internal/v2/token` returns 404, not 401 and
not 403.

That symptom is what makes this worth documenting. A 404 from an endpoint that is otherwise working
reads as one of two things: the endpoint has moved, or the account is not Copilot-entitled.
Neither is true. The token exchange is scoped to the OAuth app that minted the GitHub token, and a
token from an app that is not the Copilot app cannot be exchanged, so the route is not found for that
credential. An implementer who starts by trusting an appealing-looking client id from a blog post
will spend the session debugging the wrong layer.

The rule: the device flow is not enough; the token must come from an app that the Copilot token
exchange recognises. `Iv1.b507a08c87ecfe98` is verified in first-party code; use that.

> The language-server bundle also hardcodes a paired **client secret** for the plugin app. The
> device flow does not use a client secret, and it must not be sent — adding one turns a public-client
> flow into something the server has no reason to expect.

---

## 3. Minting the Copilot session token

```
GET https://api.github.com/copilot_internal/v2/token
Authorization: Bearer <github_access_token>
X-GitHub-Api-Version: 2024-12-15
Accept: application/json
```

Two auth spellings are accepted and both are observed in first-party code:
`Authorization: Bearer <github_token>` with `X-GitHub-Api-Version: 2024-12-15`, or
`Authorization: token <github_token>`. Use the `Bearer` form.

### The response

| Field | Meaning |
|---|---|
| `token` | the Copilot session token — the credential for inference (§4) |
| `expires_at` | expiry, **Unix seconds** — see the trap below |
| `refresh_in` | seconds until the client should re-mint; about `1500` (25 minutes) |
| `endpoints.api` | the SKU-isolated API base — **prefer this** |
| `endpoints.proxy`, `endpoints.origin-tracker`, `endpoints.telemetry` | related hosts |
| `sku` | access type: `free_limited_copilot`, `free_educational_quota`, `plus_monthly_subscriber_quota`, … |
| `individual` | Copilot Individual subscription flag |
| `organization_list`, `enterprise_list` | organisation / enterprise tracking ids |
| `limited_user_quotas` | `{chat, completions}` for free accounts, `null` otherwise |

> **`expires_at` is Unix SECONDS, not milliseconds.** This is a real trap and the evidence is
> unambiguous: in the recorded response `expires_at = 1775492517` and the session token's own `exp`
> field is the same number, and both decode to exactly 1800 seconds after the request. Interpreting
> it as milliseconds yields a timestamp in January 1970, so the token is judged expired the instant it
> is minted. The failure is a **re-mint loop**: every request mints a fresh token, judges it expired,
> mints again, and the account appears to work while burning an exchange per request and gaining a
> fresh chance to trip GitHub's rate limiting. The same warning applies to the `exp` field inside the
> session token (§4).

### `copilot_plan` is not a real response field

The mint response does **not** contain `copilot_plan`. The Copilot language server synthesises it as
`copilot_plan: r.getTokenValue("sku") ?? ""` — it is a display alias derived from `sku`, not a value
the server sends.

So: do not parse `copilot_plan`, do not write a deserialiser that requires it, and do not add it to a
response model as an optional field "for completeness". It will always be absent. The panel should display
the **`sku`** value (for example `plus_monthly_subscriber_quota`) under whatever human label the UI
uses for a plan; that is the real datum, and the alias exists only so the UI has a friendlier name for
it.

---

## 4. The session token is opaque

The `token` field has the shape:

```
k=v;k=v;…:<hmac>
```

That is: semicolon-separated key/value pairs, then a colon, then a MAC over the preceding string.
Observed fields include `tid`, `exp` (Unix seconds, equal to `expires_at`), `sku`, `proxy-ep`, `st`,
`ip`, `asn`, `cq`, `rd`, and a set of `0`/`1` feature flags (`chat`, `cit`, `mcp`, `agent_mode`, …).

**Treat it as opaque.** Pass it through unchanged as `Authorization: Bearer <token>`. `proxy-ep` is
read to derive a host (§5) — reading a field is not the same as rewriting one.

The risk of trying to parse and reconstruct it is concrete: the MAC covers the field string. Change,
drop or reorder a single field and the token no longer verifies — at best the upstream returns `401`,
and at worst the request is routed by a field you did not intend to alter. The fields are also
**server-side accounting**: `ip` and `asn` are the address the server saw, and rewriting them is the
machine-readable equivalent of a forged credential. There is no feature that requires rebuilding this
string, so the only correct handling is to move it from the mint response to the request header
without touching it.

---

## 5. Choosing the host

The inference base is selected in this order:

1. **`endpoints.api`** from the mint response, when present. This is what the first-party language
   server uses.
2. Otherwise derive from the session token's `proxy-ep` field: replace a **leading** `proxy.` with
   `api.` and use `https://`. So `proxy.individual.githubcopilot.com` becomes
   `https://api.individual.githubcopilot.com`, and `proxy.enterprise.githubcopilot.com` becomes
   `https://api.enterprise.githubcopilot.com`. Only the leading label is replaced; a `proxy.` later
   in the name is not a prefix.
3. Otherwise the literal `https://api.githubcopilot.com`, which is the catalogue's recorded
   `base_url`.

Why guessing the host is fragile: the host is **SKU-isolated**. Individual, business and enterprise
accounts are served on different hostnames with different model allowlists on the same path. A wrong
host does not necessarily fail loudly — it can return a valid-looking response scoped to a different
entitlement, or a 400 naming a model the caller "cannot access" that is in fact available on the
correct host. The token's own `proxy-ep` is authoritative for the account it belongs to; deriving the
host from it is correct, and reading the host out of a configuration file is not.

---

## 6. The surfaces

One host, one bearer credential, several wire dialects. **The path selects the dialect;
`Copilot-Integration-Id` selects the model policy.**

| Path | Dialect |
|---|---|
| `/chat/completions` | OpenAI Chat Completions |
| `/responses` | OpenAI Responses |
| `/v1/messages` | **native Anthropic Messages passthrough** |
| `/v1/messages/count_tokens` | Anthropic token counting |
| `/embeddings` | OpenAI embeddings |
| `/models` | model list |
| `/models/session` | auto model selection |

`GET /models` returns entries carrying `data[].supported_endpoints`, a subset of
`{"/v1/messages", "/responses", "/chat/completions"}`, plus `capabilities.{family,limits,supports,tokenizer,type}`,
`model_picker_enabled`, `vendor`, `version` and optionally `policy`. **Route on
`supported_endpoints`.** Calling a model on a path it does not list is a `400`, with a message such as
`model "gpt-5.2-codex" is not accessible via the /chat/completions endpoint`. The model is present in
the list and the request is still refused; the endpoint set is the authority, not the model's
existence.

These are the **upstream** paths on `api.githubcopilot.com`, not the router's routes. The relay
exposes `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/messages`,
`/v1/messages/count_tokens` and `/v1/models` (`crates/manarouter-relay/src/server.rs`); it does not
currently call `/models/session` or read `supported_endpoints` to validate a route before sending.

**The native Anthropic surface is a latency win worth stating plainly.** An Anthropic-shaped client
can be pointed at `/v1/messages` and the router can pass the request and the response through without
translating to the OpenAI shape and back. Translation is where the latency budget is spent and where
fidelity is lost (`docs/16-latency-engineering.md`); avoiding it entirely for Claude models on a
Copilot account is the fastest path the router has to those models. It is also the only endpoint that
surfaces prompt-cache `cached_tokens` in its usage block, so a client that cares about cache
accounting gets more information here than through a translated route.

---

## 7. Required headers, and the one that is a hard failure

| Header | Value | Requirement |
|---|---|---|
| `Authorization` | `Bearer <copilot_session_token>` | **mandatory** — and it must be the session token, not the GitHub token |
| `Editor-Version` | `vscode/<vs_code_version>` | **mandatory** — absence is a hard 400 |
| `Copilot-Integration-Id` | `vscode-chat` | effectively mandatory: selects the integrator policy |
| `editor-plugin-version` | `copilot-chat/0.26.7` | fingerprint fidelity |
| `user-agent` | `GitHubCopilotChat/0.26.7` | fingerprint fidelity |
| `openai-intent` | `conversation-panel`, `conversation-inline` or `conversation-edits` by surface | fingerprint fidelity |
| `x-github-api-version` | `2025-04-01` | fingerprint fidelity |
| `x-request-id` | a UUID | fingerprint fidelity |
| `x-vscode-user-agent-library-version` | `electron-fetch` | fingerprint fidelity |
| `Copilot-Vision-Request` | `true` only when an `image_url` part is present | conditional |

`catalog/providers.toml` records the static subset it can fill — `Editor-Version`,
`Copilot-Integration-Id`, `editor-plugin-version`, `user-agent`, `openai-intent`,
`x-github-api-version` and `x-vscode-user-agent-library-version` — and the `headers` test in
`crates/mr-core/src/catalog.rs` asserts that `Editor-Version` and `Copilot-Integration-Id` are
present. Two rows above are **not** catalogue entries and are **not** implemented here: the router
sends no `x-request-id`, and it does not set `Copilot-Vision-Request` for image parts. Treat those as
part of the reference client's fingerprint, not as headers this build injects.

### `Editor-Version` is mandatory

Omitting it is a hard `400` with the server's own text:

```
bad request: missing Editor-Version header for IDE auth.
```

There is no default and no fallback. This is not a fingerprint nicety; the endpoint refuses to serve
a request that does not identify its editor. Any code path that builds the Copilot headers must
include it, including error paths and retries.

### `Copilot-Integration-Id` selects the model policy

This header tells the backend which *integrator* is calling, and the integrator determines the
**allowlist of models** available. It is not cosmetic: two requests with identical bodies and
different integration ids can receive different model lists and different access decisions.

Valid observed values include `vscode-chat`, `vscode-chat-dev`, `vscode-chat-editor`,
`vscode-chat-session`, `jetbrains-chat-dev` and `copilot-developer-cli`. The catalogue records
`vscode-chat`, which is the value the VS Code Copilot Chat client uses.

> **Do not send `copilot-language-server`.** It appears as a client-set value **zero** times in the
> official bundle. It appears only as the **server-side fallback name** in 400 errors — the string you
> are told you were treated as when the request could not be attributed to a real integrator. Sending
> it explicitly asks to be treated as the fallback, which is the least-entitled path.
>
> **UNCERTAIN:** the exact behaviour when `Copilot-Integration-Id` is omitted entirely. It is
> "effectively mandatory" rather than proven mandatory; the one certain statement is that sending
> `copilot-language-server` is wrong. Since the header selects model availability, omitting it is not
> a safe default even if it does not 400.

### The classic failure: sending the raw GitHub token

If the `Authorization` header carries the **GitHub** OAuth token instead of the minted session token,
the server routes the request to the fallback integrator `copilot-language-server` and returns:

```
400 … not available for integrator copilot-language-server
```

The error names the fallback, not the mistake, so it reads as a plan or entitlement problem. The fix
is always the same: exchange the GitHub token for a Copilot session token first (§3), send the session
token, and keep the GitHub token for the exchange only.

### `x-github-api-version`

`2025-04-01` and `2026-01-09` have both been observed working for the CAPI calls, and which one is
*required* is **UNCERTAIN**. The catalogue records `2025-04-01`, which is the value the reference
client uses. This is a header where the conservative choice is to send the recorded value rather than
to send two.

---

## 8. Refresh

The Copilot session token lasts about **1800 seconds (30 minutes)**, and `refresh_in` (about 1500
seconds) is the server's own hint about when to re-mint. Observed client margins:

| Client | Re-mints when |
|---|---|
| language server | `expires_at = now + refresh_in + 60` |
| VS Code | `expires_at < now - 300` (a 5-minute safety margin) |
| one third-party client | 120 seconds before expiry |

There is **no separate OAuth refresh** for Copilot. The session token is re-minted by repeating the
`GET https://api.github.com/copilot_internal/v2/token` exchange from §3, and that call needs only the
long-lived GitHub token. Consequences:

- **The GitHub token is what persists** in the vault. The session token is derived, short-lived, and
  need not be stored at all (`docs/10-oauth-overview.md` §6).
- **The GitHub token does not rotate on its own.** The dossier records that it changes only on revoke
  or a policy action. That is a T2 observation, so the single-flight guard from
  `docs/10-oauth-overview.md` §4 is still applied to the mint: a duplicate mint is wasted work, and a
  design that assumes non-rotation forever is one policy change away from a broken account.
- **Failures are distinguishable where it matters.** A `401` from the mint means the GitHub token is
  no longer valid and the operator must re-authenticate. A `403` carrying
  `{can_signup_for_limited, error_details: {notification_id, …}}` means the account is not
  Copilot-entitled, or an organisation policy is blocking it — a different problem with a different
  remedy, and not something a re-login fixes.

---

## See also

- [OAuth overview](10-oauth-overview.md) — the device flow, single-flight, storage
- [OpenAI Codex](11-oauth-codex.md) — the other short-lived-token platform
- [Google Antigravity](12-oauth-antigravity.md) — the two-tier Google login
- [Adding a platform](14-oauth-adding-a-platform.md) — the trait, including the device-flow pair
- [Latency engineering](16-latency-engineering.md) — the native Anthropic passthrough
- [Security and credentials](21-security.md) — §5 the ToS posture and the fingerprint rule
- [providers.toml](../catalog/providers.toml) — the `github-copilot` entry and its recorded headers
- [catalog.rs](../crates/mr-core/src/catalog.rs) — the test asserting `Editor-Version` is present
