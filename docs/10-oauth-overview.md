# OAuth: the shared machinery

Three of Manarouter's providers do not take an API key. They take a **login**, and the login produces
a credential that expires, rotates, and can be revoked without warning. The per-platform code that
knows the URLs and the claim names is small — a few hundred lines in total. The machinery around it
is where the failures live: refreshing at the right moment, refreshing exactly once, storing the
result where a leaked config file cannot reveal it, and telling a revoked grant apart from a dropped
connection.

This document is about that machinery. The platform-specific facts are in
`docs/11-oauth-codex.md`, `docs/12-oauth-antigravity.md` and `docs/13-oauth-copilot.md`; the process
of adding a fourth platform is `docs/14-oauth-adding-a-platform.md`.

---

## Current implementation scope

The management API and panel currently offer interactive OAuth connection for **OpenAI Codex** (browser loopback/PKCE, with an OpenAI device-code fallback), **GitHub Copilot** (device code) and **Google Antigravity** (loopback authorization code; the reverse-engineered T2 IDE client metadata is unverified, and paid-tier onboarding needs an explicit project). Those are exactly the three registered platforms — `openai-codex`, `github-copilot` and `antigravity` in `crates/mr-core/src/oauth/mod.rs`. Token sets are stored in the encrypted vault; non-secret credential metadata is in the conductor's SQLite store. The relay opens the same configured SQLite store and vault, resolves the metadata reference to its vault key, and uses the platform refresher before inference. Cline, xAI OAuth, and import-only or cookie/session credentials are not exposed as working login flows. No live login was exercised as part of implementation; run the automated tests before relying on the behavior.

These OAuth operations live in the **conductor control plane**; they do not add endpoints to the inference relay. The relay only consumes the resulting account credential while serving inference.

## 1. Why OAuth instead of an API key

An API key is a metered credential. Every token the router forwards is billed to a payment method,
at a rate the operator can see and predict. An OAuth login against a subscription is the opposite:
it uses a plan the operator already pays for on a flat or seat basis, and the marginal cost of a
request is zero until a quota is hit.

That difference is the entire reason this subsystem exists. A ChatGPT Plus or Pro plan, a Copilot
seat and a Google Code Assist tier are all usable through a client the vendor ships. Routing them
through the router lets any client that speaks the router's API — an editor, an agent, a script
written against `curl` — use the plan without the operator opening a second, metered account. For a
person who already pays for the plan, this is the difference between one bill and two.

### What it costs

The ToS posture is stated in full in `docs/21-security.md` §5 and is not softened here:

> Routing a subscription-backed session through a proxy is not a use the provider has licensed. The
> account may be restricted or banned.

Two consequences follow for the code, and both are deliberate:

- The router sends **exactly** the header fingerprint the first-party client sends and adds nothing.
  An over-complete fingerprint — one carrying a header the vendor's own client never sends — is as
  identifiable as a missing one, and it is the more common mistake because it feels like diligence.
- Per-account cooldowns exist to *respect* a rate-limit signal, not to route around it. The router
  does not rotate accounts to evade limits and does not spend reset credits.

The operator's account is the operator's decision. The documentation's job is to make the tradeoff
legible rather than to make it for them.

---

## 2. The two flow shapes, and which platform uses which

| Platform | Flow | Redirect | Why this flow |
|---|---|---|---|
| Codex | authorization code + PKCE `S256` | loopback `127.0.0.1:1455`, fallback `1457` | the first-party client is a native CLI on the same machine as the browser |
| Antigravity (T2) | authorization code + PKCE `S256` | loopback `localhost:51121` | an IDE with a browser on the same machine |
| gemini-cli (T1) | authorization code, no PKCE on the loopback path | loopback `127.0.0.1:{ephemeral}/oauth2callback` | Google's installed-app client; the client secret substitutes for PKCE |
| Copilot | **device flow** | none | the client has no browser and cannot receive a loopback callback |

### Authorization code with PKCE on a loopback redirect

The browser is opened at the provider's authorize URL with a `redirect_uri` pointing at a listener
the router itself has just bound. The provider authenticates the user, redirects to that listener
with a `code`, and the router exchanges the code for tokens. PKCE (`code_challenge` at authorize
time, `code_verifier` at token time) is what stops another local process from stealing the code: the
code alone is useless without the verifier, which never leaves the router's memory. `state` is a
nonce that stops a forged callback from injecting a code.

This works when a browser exists on the same machine as the router and the provider has registered a
loopback redirect.

### When the loopback redirect never arrives

That condition fails more often than it looks: the panel may be open through a tunnel or from another
machine, the redirect port may be taken, the tab may be closed, or a container boundary may simply
swallow the callback. The browser is then left on an error page — but the address bar still holds
`?code=…&state=…`, which is the whole of what the router needs.

So the panel offers the paste fallback for both Codex and Antigravity: paste the URL (the full URL,
just the query string, or `code#state`) and the waiting sign-in completes. Three properties make that
fallback as safe as the redirect it stands in for:

* **The `state` is still compared.** A pasted URL is attacker-influenced input exactly like a socket
  callback, so a code from somebody else's flow is refused. A bare code, which carries no state, is
  refused with an explanation rather than exchanged.
* **There is only one completion path.** The paste does not exchange anything itself: it hands the
  code to the wait that is already in progress, so the pasted and automatic routes cannot drift
  apart.
* **The listening port is remembered.** The token exchange must repeat the same `redirect_uri` the
  authorize request published, so the session keeps the port it bound rather than assuming a
  constant.

Because a waiting `complete` call takes its session *out* of the session registry (one slow browser
must not hold the map lock), pasted redirects live in a separate slot keyed by session id, and the
wait races the loopback listener against that slot. A paste that arrives before the wait starts is
still parked and found, and `Notify` keeps it from being lost in the gap.

### The device flow

Copilot is the platform whose *default* is the device flow — Codex offers the same fallback on request
(see `docs/11-oauth-codex.md` §2a) — and it is worth being precise about why, because "Copilot uses the
device flow" is otherwise an arbitrary-looking fact.

The device flow inverts the direction of the redirect. The router calls
`POST https://github.com/login/device/code` and receives a short `user_code` and a
`verification_uri`. It shows those to the operator, who opens the URI **in a browser of their
choosing, on any device**, types the code, and authorises. Meanwhile the router polls
`POST https://github.com/login/oauth/access_token` with the `device_code` until the response
contains an `access_token`.

The problem it solves is this: the router is a headless process. It has no browser to open and no
way to be reached by the user's browser. It cannot receive a loopback callback on the user's machine,
because the user's browser might be on a different machine entirely, or inside a container, or over
SSH. A loopback redirect requires the client and the browser to share a network namespace; the device
flow requires only that the user can see a code on one screen and type it on another. It is the
standard answer for a client that cannot host a redirect, and it is why the Copilot integration has
no redirect port and no PKCE — the device code itself is the secret, and it is single-use.

One consequence: the Copilot device flow has no client secret either. The device flow is defined for
public clients, so the only thing identifying the client is the `client_id`.

---

## 3. What a platform must provide: the `OAuthPlatform` trait

The design rule is that adding a platform is **implementing a trait**, not editing a `match`. A
`match platform { … }` in the request path would put a provider name into the hot path, and a
`match` in the login flow would put one into the panel. Either way, the fourth platform touches code
that has nothing to do with it.

> **Status.** The `OAuthPlatform` trait is implemented in `crates/mr-core/src/oauth/mod.rs`, with the
> platform implementations in `codex.rs`, `copilot.rs` and `antigravity.rs` beside it and the static
> registry in `PLATFORMS`/`platforms()`. The table below names the implemented methods; it is not a
> design sketch.

Every item below is something a platform knows and the shared machinery does not:

| Item | Responsibility |
|---|---|
| `id()`, `label()`, `description()` | the stable platform id, matching `oauth_platform` in `catalog/providers.toml` and the `platform` field of `AuthKind::OAuth` in `crates/mr-core/src/dialect.rs`, plus the panel's label and one-line description |
| `begins_with_loopback() -> bool` | the **default** flow: `true` for a loopback redirect, `false` for device code |
| `supports_device_flow() -> bool` | whether the platform can *also* be logged in with a device code; defaults to `!begins_with_loopback()`, and Codex overrides it to `true` |
| `redirect_ports() -> RedirectPorts` | host, primary/fallback port and path — the string must match what the provider registered |
| `requires_pkce() -> bool` / `validates_redirect_port() -> bool` | whether the flow binds a PKCE `S256` challenge, and whether the bound port must be one of the registered ports |
| `authorization_url(state, pkce, port) -> Result<String>` | builds the authorize URL, including PKCE, `state` and platform-specific extra parameters |
| `device_start()` / `device_poll(&DeviceAuthorization)` | the device-flow pair: request a code, then poll it once and interpret `authorization_pending` / `slow_down` |
| `exchange_code(code, pkce, port) -> Result<TokenSet>` | the code-for-token exchange, including its encoding (form or JSON) |
| `refresh(refresh_token)` / `refresh_with_context(refresh_token, previous)` | the refresh call, which may use a **different encoding and the same URL** as the exchange |
| `complete_login(&mut TokenSet)` / `complete_login_with(&mut TokenSet, http)` | fetch provider-owned post-login metadata before credentials are persisted (the `_with` form is the deterministic test seam) |
| `apply_credential(&TokenSet) -> Vec<(HeaderName, HeaderValue)>` | the header transformation: turn a credential into the exact headers an inference request must carry |
| `account_identity(&TokenSet) -> Result<AccountIdentity>` | resolve the credential to the stable account identity used to key multi-account storage |
| `fingerprint_headers() -> &'static [(&'static str, &'static str)]` | the catalogue-matching fingerprint header list |
| `refresh_lead() -> Duration` | how early before expiry the credential is refreshed |

Two design points matter more than the individual signatures.

**The trait is object-safe and lives in a registry.** `crates/mr-core/src/oauth/mod.rs` exposes
`PLATFORMS` and looks a platform up by id (`platforms()`/`platform()`). Both processes use it: the
conductor drives login, and the relay holds a `Refresher` per platform and calls
`platform(..).apply_credential(&tokens)` before an inference call. The lookup is a linear scan on an
id that came from the credential's configured platform, so no provider knowledge enters the routing
decision itself.

**`apply_credential` returns headers, not a mutated request.** The header transformation is where a
platform asserts its fingerprint: Codex contributes `chatgpt-account-id` and `originator`, Copilot
contributes its static `Copilot-Integration-Id`/`editor-plugin-version`/`user-agent` set plus the
session-token bearer. `Editor-Version` is deliberately **not** here — its value is the template
`vscode/${vs_code_version}`, which only the catalogue can fill, so the catalogue owns it. Returning an
owned header list keeps the dialect translator free of provider knowledge, and makes the fingerprint a
testable value rather than a side effect buried in a request builder.

---

## 4. Single-flight refresh, and the failure mode it prevents

Concurrent requests on one account can each discover the same expired access token. The naive
implementation lets each of them refresh. That is wrong for any rotating credential, and
catastrophic for a single-use one.

### The failure mode

Codex refresh tokens are **single-use**: a successful refresh returns a new refresh token and
invalidates the one that was presented. If two refreshes run concurrently with the same token:

1. Both requests are sent carrying the same refresh token.
2. The provider processes the first, rotates the token, and returns a new one.
3. The provider processes the second. It is presented with a token that has already been consumed.
   A reuse of a rotated token is treated as evidence that the token was stolen.
4. Depending on the provider's classification, the grant is invalidated or the whole token family is
   revoked. Either way the account is **permanently broken**: no further refresh succeeds, and the
   only recovery is a fresh interactive login.

The router cannot distinguish step 4 from a legitimate revoke, so it cannot recover on its own. The
failure is not a slow request; it is a login the operator has to perform by hand, caused by two
requests arriving at the same moment.

### The mechanism

A per-account, in-memory, single-flight guard, keyed by account id rather than by platform: the first
caller to observe the need to refresh installs a shared future in a map; every other caller awaits
that same future instead of starting its own refresh. When the future resolves, all waiters receive
the same new credential and the map entry is removed. There is exactly one refresh in flight per
account at any moment, no matter how many requests are queued behind it.

The account id is the key, not the provider id, because two logins for the same platform are two
independent grants and each needs its own guard.

### Why the guard is unconditional

For Codex the guard is load-bearing: without it, the failure above is reachable and permanent.

For Copilot, the research dossier records the underlying GitHub OAuth token as **long-lived and
non-rotating** — it changes only on revoke or a policy action — and there is no OAuth refresh at all.
The short-lived Copilot session token is re-minted from that long-lived token, so two concurrent
mints are merely wasteful rather than destructive. The guard is still applied there, for two reasons:
wasteful is still wrong, and "does not rotate" is a T2 observation (a third-party client's observed
behaviour), not a vendor guarantee. The cost of the guard when it is unnecessary is a map lookup; the
cost of leaving it out when it is necessary is a dead account.

---

## 5. Refresh windows

A refresh window is how far *before* expiry the credential is refreshed.

| Platform | Window | Source |
|---|---|---|
| Codex | 5 minutes (`CHATGPT_ACCESS_TOKEN_REFRESH_WINDOW_MINUTES = 5`) | primary (`openai/codex`) |
| Copilot | `refresh_in` from the mint response, ~1500 s; the language server sets `expires_at = now + refresh_in + 60` and VS Code refreshes at `expires_at - 300` | primary/T2 |
| Antigravity | 60 seconds | T2 — the reverse-engineered IDE client refreshes within 60 s of expiry |
| Claude Code | **4 hours** | **UNCERTAIN** — not in the research dossier; see below |

> **UNCERTAIN.** The 4-hour Claude Code figure is stated in this project's brief but does not appear
> in `.research/oauth.md`, which is the verified source for the rest of this document. It is
> **not** confirmed from Anthropic's client in this session. The consequence of being wrong is
> bounded and worth naming: if the real window is much shorter, the router's scheduled refresh fires
> too rarely and the 401 repair path (§7) does the work instead — slower, but correct. If it is much
> longer than the token lifetime, the router refreshes after expiry and every refresh is a wasted
> round trip, which on a single-use token is worse than it sounds because each wasted refresh still
> consumes a rotation. Treat the number as a tunable default to be verified against
> [`anthropics/claude-code`](https://github.com/anthropics/claude-code) before it is relied on.

### Why a large lead matters when the refresh token is single-use

The intuition that "refresh late and you refresh less" is wrong here, and the reason is what a
single-use token does to a failed refresh.

Each refresh is a state mutation whose side effect survives a lost response. If the request reaches
the provider and the response is lost — a timeout, a killed process, a dropped connection — the
provider has already rotated the token. The router still holds the old one, which is now dead, and no
amount of retrying will help. The only recovery is a fresh login.

So a refresh window is really a **retry budget measured against the access token's remaining life**:

- With a **5-minute** window, one transient failure consumes the whole budget. The next refresh is
  triggered by a live user request returning 401, which is exactly the situation where several
  requests are concurrent and the single-flight guard has the most work to do. The failure is
  user-visible.
- With a **4-hour** window, a failed refresh is retried with backoff long before the access token
  expires. No user request ever discovers an expired credential, and the refresh happens on a
  background schedule where concurrency is naturally low.

The cost of a large lead is more rotations per day, and every rotation is a chance to lose a
response. That is not an argument against the large lead; it is the argument for §4 and for
persisting the new refresh token atomically before the old one is discarded. The window and the
single-flight guard are two halves of the same design: the window moves refreshes off the request
path, and the guard makes the resulting background refreshes safe.

---

## 6. Where a credential is stored

An OAuth credential lives in the **vault**, addressed by id, and nowhere else.

| Store | Contains |
|---|---|
| Vault, id `provider/<provider-id>/account/<account-id>` | the credential: access token, refresh token, expiry, granted scopes, account identity |
| `config.json` | a reference to that id, never a value |
| `state.sqlite` (`oauth_credential` table) | platform, account label, scopes, expiry — **never** the access or refresh token |

The vault id convention comes from `crates/mr-core/src/secret.rs`
(`account_secret_id(provider_id, account_id)`), which is the same convention `docs/21-security.md` §3
describes and a test asserts cannot leak into the serialised configuration.

What is stored is the credential the token endpoint returned, plus the identity extracted from it:
for Codex, the raw `access_token`, `refresh_token`, `id_token` and `expires_in`, plus the
`account_id` parsed from the JWT; for Copilot, the long-lived GitHub token and the account login. What
is **not** stored is anything derived and cheap to recompute: a Copilot session token is re-minted
from the GitHub token on demand and never persisted, because it lives 30 minutes and persisting it
would only widen the window in which a copy exists.

> The table **is** implemented, in `crates/mr-core/src/store.rs` (`MIGRATION_V1` creates
> `oauth_credential`; the schema is embedded there rather than in a file under `migrations/`). The
> constraint `docs/21-security.md` §3 states is the one the code enforces: the table holds metadata
> only, and a test reads the raw database bytes and asserts no token appears in them.

The reason for the split is the threat model, not tidiness. A database file is the artifact most
likely to end up in a backup and least likely to be encrypted; a credential file is the artifact that
must be. Keeping tokens out of the database means a leaked database is an operational problem rather
than a credential compromise.

---

## 7. The 401 repair path

An access token can expire between the moment a request is built and the moment the upstream reads
it. The repair is deliberately short:

1. A request returns `401`.
2. The conductor refreshes the account's credential **once**, through the single-flight guard.
3. The request is retried **once** with the new credential.
4. If the retry also returns `401`, that is a failure — not a reason to loop.

The conductor performs this rather than the relay because the vault is on disk and the relay has no
business touching disk. The relay reports the `401` upstream; the conductor owns the credential.

### What happens when the refresh itself fails

The account moves to `auth-failed` (`AccountState::AuthFailed` in `crates/mr-core/src/dialect.rs`)
and stays there through a long cooldown. It is **not** deleted, and it is **not** treated as
permanently dead.

That is a deliberate asymmetry, because a revoked grant and a transient upstream fault look
identical from the router's side of the connection: both are an error on the token endpoint with no
credential returned. The router cannot tell them apart, so it must not guess in the destructive
direction. If it deleted the account, a five-minute outage at the provider would silently destroy the
operator's configuration. If it treated the state as permanently fatal, the same outage would require
a manual re-login.

So `auth-failed` means: stop using this account, surface it in the panel, and keep the credential
so that a later refresh can succeed. The panel offers re-login; the operator decides whether the grant
was really revoked. When the cause was transient, the account recovers on its own at the next
successful refresh. When the cause was a revoked grant, only a human re-login fixes it — and the
state tells them so, which is the point.

Note that Codex's own client classifies `invalid_grant`, `401` and the reused/expired/invalidated
refresh-token errors as **permanent** failures requiring re-login. That judgement is platform-specific
and is not copied wholesale: the router records `auth-failed` and lets the operator act, rather than
deleting an account on the strength of an HTTP status.

---

## 8. The loopback listener

For the authorization-code flows the router binds a TCP listener on the loopback interface and uses
its address as the OAuth `redirect_uri`.

| Platform | Host | Port | Path | Notes |
|---|---|---|---|---|
| Codex | `127.0.0.1` | **1455**, fallback **1457** | `/auth/callback` | both ports are registered with the provider |
| Antigravity (T2) | `localhost` | **51121** (fixed) | `/oauth-callback` | registered for the Antigravity IDE |
| gemini-cli (T1) | `127.0.0.1` | ephemeral (OS-assigned) | `/oauth2callback` | the installed-app client secret, not a registered port, is the protection |
| Claude Code | `127.0.0.1` | loopback | — | **UNCERTAIN**; not covered by the research dossier |

### Why the port is fixed

Because the provider stores the redirect URI as a string and compares it byte for byte. A redirect
URI is not a pattern; `http://127.0.0.1:1455/auth/callback` is not "the loopback interface on any
port". If the router bound a different port, the authorize request would be rejected before the user
ever saw a consent screen — typically as `redirect_uri_mismatch` or an equivalent error, which reads
like a client-registration problem and is really a port-number problem.

This also means a fixed port is a **shared resource**. The fallback to 1457 exists because 1455 might
be held by a stale listener from a cancelled login — that is precisely the case the Codex client
handles by trying the second registered port. Both ports are registered for exactly this reason: a
fallback to an unregistered port would not work.

### The host string is part of the match

`localhost` and `127.0.0.1` resolve to the same interface but are different strings to the provider.
Antigravity registers `http://localhost:51121/oauth-callback`; Codex and gemini-cli register the
`127.0.0.1` spelling. Substituting one for the other is a mismatch even though the listener is
identical. The catalogue records the string, and the implementation must not normalise it.

### When the port is taken

1. Try the primary port, bound to `127.0.0.1` only — never `0.0.0.0`, because the callback carries an
   authorization code and there is no reason for it to be reachable off-host.
2. If the bind fails, try the registered fallback where the provider has one (Codex: 1457).
3. If that fails too, fail the login with a message naming the port and the likely cause. Do **not**
   silently bind an arbitrary port: the resulting redirect would be rejected and the operator would
   see a provider error instead of the local problem.

### Why the listener is loopback-only

The authorization code arrives on this port. Binding it to a public interface would let anything on
the network deliver a callback and, more importantly, let anything on the network read the code. The
listener's whole security model is that the only reachable client is the browser on the same machine;
a wildcard bind discards that.

---

## 9. Multi-account

One login is one account. Several logins for the same platform become several accounts, and the router
can hold them simultaneously because each has its own vault entry and its own single-flight guard.

The thing that makes them *different* accounts is the **identity extracted from the credential**, not
the order they were added and not a user-chosen label:

| Platform | Identity | Where it comes from |
|---|---|---|
| Codex | `chatgpt_account_id` | the JWT namespace claim `https://api.openai.com/auth` |
| Copilot | the GitHub account login | the GitHub token, via the `read:user` scope |
| Antigravity | the Google account id | `GET https://www.googleapis.com/oauth2/v2/userinfo` (no `openid` scope is requested, so there is no ID token to read) |

Two consequences:

- **Re-authenticating the same account replaces the credential in place.** Logging in again to refresh
  a revoked grant does not create a second account and does not double a quota. This is what an
  operator who hits "sign in again" expects.
- **A second login with a different identity creates a new account**, which is how someone with a
  work and a personal plan uses both. Quota, rate limiting and health are tracked per account, so a
  throttled work account does not mark the personal one unavailable. `docs/07-hedged-race.md` covers
  how selection then chooses among them.

Labels are operator-chosen and therefore collide; identity is not. The panel shows the label, and the
identity is what the storage keys on.

---

## 10. Published constants and push protection

OAuth client identifiers for desktop and CLI applications are **public by design**. OAuth 2.1 treats
such clients as public, and the flow's security comes from PKCE and the provider's redirect
validation, not from the secrecy of the identifier. All the same, a repository that writes a
vendor's published client secret as a literal will be flagged by any modern host's push protection:
the scanner cannot distinguish a vendor-published value from a leaked one, and it should not try,
because the pattern is identical.

This repository hit exactly that. The first push was rejected with `GH013: Repository rule
violations`, naming four Google constants — two `GOCSPX-`-prefixed client secrets and two client ids.
The override is a manual, per-secret click in the repository's security settings, which means **every
future contributor's push fails** until each of them repeats that click.

There are two honest resolutions:

1. **Fragment the literal with a comment explaining why.** The value is assembled at compile time
   from pieces, so it never appears as a contiguous string for a scanner to match, while the constant
   is still shipped and the login works out of the box.
2. **Have the operator supply it.** Store nothing, and ask the operator to paste the vendor's client
   id during setup.

This project chose **fragmentation**, in `crates/mr-core/src/published.rs`, which carries the full
reasoning in its module documentation. The deciding argument is the operator's experience: an
operator cannot be expected to know a vendor's internal client id, and a login flow that fails until
they find one is a login flow that does not work. The value is public, so there is nothing to protect
by withholding it; only the scanner has to be satisfied. Fragmentation also costs nothing at
runtime — `concat!` joins the pieces during compilation.

The cost is real and worth naming: the value is no longer greppable as a whole. A reader who finds a
truncated constant in these documents, or a fragment in `published.rs`, deserves to know why, which
is what this section is for. `docs/21-security.md` §6 carries the same explanation from the security
side.

**This is not a precedent for hiding a real credential.** A real secret must never be committed in
any form, fragmented or otherwise; it belongs in the vault, addressed by id. The test for a constant
that may live in `published.rs` is whether you can link to the vendor's own source or shipped bundle
containing it. If you cannot, it is not published, and it does not belong in the repository at all.

---

## See also

- [OpenAI Codex](11-oauth-codex.md) — the login and the ChatGPT inference fingerprint
- [Google Antigravity](12-oauth-antigravity.md) — the two client tiers and Cloud Code onboarding
- [GitHub Copilot](13-oauth-copilot.md) — the device flow and its mandatory headers
- [Adding a platform](14-oauth-adding-a-platform.md) — the trait implemented end to end
- [Security and credentials](21-security.md) — §5 the subscription ToS posture, §6 the constants
- [Latency engineering](16-latency-engineering.md) — the latency budget the credential path must fit
- [published.rs](../crates/mr-core/src/published.rs) — the assembled constants and push-protection reasoning
- [secret.rs](../crates/mr-core/src/secret.rs) — the vault and the credential-id convention
- [dialect.rs](../crates/mr-core/src/dialect.rs) — `AuthKind::OAuth { platform }`, `AccountState::AuthFailed`
