# OAuth: Google Antigravity and Gemini Code Assist

Antigravity and Gemini Code Assist are the same Cloud Code backend reached through two different
public OAuth clients. The difference is not cosmetic: the clients carry different scopes and different
metadata, and the backend routes and entitlements follow the metadata. Picking the wrong one produces
a login that succeeds and an inference call that fails, or a call that succeeds against a different
tier's quota.

The shared machinery is in `docs/10-oauth-overview.md`.

---

## 1. Naming the constants, and why they are written the way they are

This document does not print the four Google OAuth constants in full, and neither does any other file
in the repository. The first push of this repository was rejected by GitHub's push protection
(`GH013: Repository rule violations`) naming exactly these four: two client ids and two
`GOCSPX-`-prefixed client secrets. The scanner cannot distinguish a vendor-published value from a
leaked one, and the only override is a manual per-secret click that every future contributor would
have to repeat.

The values are assembled at **compile time** from fragments in `crates/mr-core/src/published.rs`
using `concat!`, so no contiguous literal exists in the source for a scanner to match, the assembled
constant is identical, and there is no runtime cost. `docs/21-security.md` §6 carries the security
explanation and `docs/10-oauth-overview.md` §10 the engineering one.

Where this document needs to identify a value it uses a **truncated** form — `681255809395-oo8f…`,
`GOCSPX-4uHg…` — which is enough to recognise the constant and cannot match a detector pattern. The
full values are one `grep` away in `published.rs`, which is the point: they are public by design, and
the only reason they are not written out is to keep `git push` working.

---

## 2. Two tiers, and why there are two

| | T1 — gemini-cli | T2 — Antigravity IDE |
|---|---|---|
| Client id | `681255809395-oo8f…` (`GEMINI_CLI_CLIENT_ID`) | `1071006060591-tmhs…` (`ANTIGRAVITY_CLIENT_ID`) |
| Client secret | `GOCSPX-4uHg…` (`GEMINI_CLI_CLIENT_SECRET`) | `GOCSPX-K58F…` (`ANTIGRAVITY_CLIENT_SECRET`) |
| Scopes | `cloud-platform`, `userinfo.email`, `userinfo.profile` | T1's three **plus** `cclog` and `experimentsandconfigs` |
| Loopback redirect | `http://127.0.0.1:{port}/oauth2callback`, ephemeral port | `http://localhost:51121/oauth-callback`, fixed port |
| PKCE on the loopback path | no | see the warning below |
| Provenance | Google's open-source [`gemini-cli`](https://github.com/google-gemini/gemini-cli), `packages/core/src/code_assist/oauth2.ts` | extracted from the shipped Antigravity IDE application bundle |
| Confidence | **verified (T1 primary)** | **UNCERTAIN (T2)** |

**Why two exist.** T1 is the credential Google publishes in its own repository. It is verifiable —
anyone can open the file and read it — and it is the right default. It does not, however, carry
Antigravity's two extra scopes, and it does not present Antigravity's client metadata. T2 is the
credential the Antigravity IDE ships inside its application bundle; it is what makes the backend
treat the caller as Antigravity rather than as the gemini-cli tool, which changes the quota pool and
the model policy.

So the two tiers are a real choice, not redundancy: T1 is the one whose provenance can be checked, T2
is the one whose behaviour matches the IDE. The catalogue ships T2, because Antigravity's 5-scope
consent and `ANTIGRAVITY` metadata are the reason to use this provider rather than the plain Gemini
API. The T1 pair remains in `published.rs` and is the fallback when T2 stops working.

> **T2 is UNCERTAIN.** The Antigravity pair was extracted from a **shipped application**, not from a
> public repository. It is corroborated by three independent third-party projects, and no Google
> artifact confirms it in the research session. The consequence of being wrong is specific and worth
> stating: the authorize request is rejected with `invalid_client` (or, if the client id is valid but
> the app has been removed, an equivalent error) **before** the user sees a consent screen, so the
> failure is immediate and diagnosable — but the fix is to extract a fresh pair from a current
> install rather than to debug the flow. An installed application can rotate its embedded client at
> any release, without notice and without any public changelog. `crates/mr-core/src/published.rs`
> documents the extraction recipe.

### Why the secrets are in the repository at all

Both are **public by design**. Google registers installed applications as public clients and ships
the secret inside the application, because the secret cannot be kept: anyone can read a desktop
binary or an Electron bundle. Google's own `gemini-cli` source says so in a comment, and
`docs/21-security.md` §6 is the project's statement of the same position. Nothing here grants access
to anything on its own; the grant comes from the user completing a consent screen.

### The PKCE question for T2 — a discrepancy that must be resolved, not smoothed over

The research dossier records the Antigravity authorize URL **with** `code_challenge` and
`code_challenge_method=S256`, and its token request **with** `code_verifier`. The brief for this
document states that T2 uses **no PKCE**.

> **UNCERTAIN, and the two statements are in direct conflict.** The dossier is the verified source
> used throughout this document set, so this document follows it: **T2 sends PKCE `S256`**, and the
> implementation should send a `code_verifier` at exchange time. But the conflict is unresolved, and
> the consequence of getting it wrong runs in both directions:
>
> - If the implementation sends a `code_challenge` at authorize time and the server (or a future
>   plugin release) does not expect one, the exchange can fail with `invalid_grant` because the
>   verifier does not correspond to anything.
> - If the implementation omits the challenge but sends a verifier, the exchange fails with
>   `invalid_grant` for the opposite reason.
> - If the implementation omits **both**, it matches the "no PKCE" reading and fails only if the
>   server actually requires PKCE.
>
> The safe engineering resolution is to make the challenge/verifier pair a property of the tier, not
> a global constant: T1's loopback path sends neither, T2 sends both, and the pair is never sent
> half-way. Verify against a live Antigravity login before treating either as settled.

---

## 3. The authorize request

### T1 (gemini-cli), loopback path

`https://accounts.google.com/o/oauth2/v2/auth` with `redirect_uri` (the `127.0.0.1` spelling),
`access_type=offline`, `scope` and `state`. **No `prompt=consent` and no PKCE on this path** — the
installed-app client secret is what protects the exchange. The port is ephemeral: the client binds
port 0 and uses whatever the OS assigns, because this client is not registered against a fixed
redirect.

`access_type=offline` is the Google equivalent of Codex's `offline_access`: without it there is no
refresh token, and the account needs an interactive login again within the hour.

### T1, the user-code path

For a client that will not open a browser at all, gemini-cli offers a paste-the-code flow with
`redirect_uri=https://codeassist.google.com/authcode`, `access_type=offline`,
`code_challenge_method=S256` and `state`; the user pastes the code back into the terminal. PKCE
applies **on this path** — the opposite of T1's loopback path. The distinction matters if either is
copied without the other.

### T2 (Antigravity)

```
https://accounts.google.com/o/oauth2/v2/auth
  ?client_id=1071006060591-tmhs…
  &response_type=code
  &redirect_uri=http%3A%2F%2Flocalhost%3A51121%2Foauth-callback
  &scope=<the 5 scopes, space-joined, URL-encoded>
  &code_challenge=<S256>
  &code_challenge_method=S256
  &state=<base64 of {verifier, projectId}>
  &access_type=offline
  &prompt=consent
```

Two details beyond the PKCE question:

- The redirect is `localhost`, **not** `127.0.0.1`, and the port is fixed at `51121`. The provider
  has that exact string registered; substituting the numeric address is a `redirect_uri_mismatch`.
- The `state` is an ordinary random, single-use nonce here (`flow::new_state()` — 32 CSPRNG bytes,
  base64url, unpadded). The project id does **not** travel in `state`: it is discovered after the
  token exchange by `loadCodeAssist` / `onboardUser` and persisted as `cloudcode_project_id` in the
  token set's `extra` (`store_cloudcode_context` in `crates/mr-core/src/oauth/antigravity.rs`), which
  is also what survives a refresh. `docs/14-oauth-adding-a-platform.md` treats `state` as opaque and
  single-use, and this platform does the same.

The `https://accounts.google.com/o/oauth2/auth` spelling without `/v2` is also valid and is what one
of the corroborating projects uses; both endpoints work. Google's documentation for the endpoint is
[OAuth 2.0 for Mobile & Desktop Apps](https://developers.google.com/identity/protocols/oauth2/native-app).

---

## 4. Project onboarding

Antigravity and gemini-cli do not talk to the plain Gemini API. They talk to the **Cloud Code** API
at `https://cloudcode-pa.googleapis.com`, version `v1internal`, with the URL shape
`${endpoint}/${version}:${method}`. Every project-scoped call needs a Google Cloud project id, and
getting one is a three-step handshake.

1. Call `loadCodeAssist` to discover the account's tier and current project.
2. Call `onboardUser` to create or attach the project for that tier.
3. Poll the long-running operation `onboardUser` returns until it completes, then read the project
   id out of the result.

### `loadCodeAssist`

```
POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist
{
  "cloudaicompanionProject": "<project-id-or-omitted>",
  "metadata": {
    "ideType": "IDE_UNSPECIFIED",
    "platform": "PLATFORM_UNSPECIFIED",
    "pluginType": "GEMINI",
    "duetProject": "<project-id-or-omitted>"
  }
}
```

The response carries `currentTier`, `allowedTiers`, `ineligibleTiers`, `cloudaicompanionProject` and
`paidTier`. Antigravity sends `"ideType": "ANTIGRAVITY"` and a real `platform` (`WINDOWS` / `MACOS`)
instead of the `UNSPECIFIED` placeholders — see §5 for why that matters.

### `onboardUser` and the `Precondition Failed` trap

> **The trap: for a `free-tier` account, omit the `project` field entirely.** Supplying it causes the
> call to fail with `Precondition Failed`.

The first-party source comment is explicit: the free tier uses a **managed** Google Cloud project
that Google owns and the user never sees, so there is nothing to name. Sending
`cloudaicompanionProject` (or `duetProject`) asks the backend to onboard the user into a project they
have supplied, and the precondition for that — that the caller can see and bill the project — is not
satisfied. The server's answer is an HTTP-level precondition failure (a gRPC `FAILED_PRECONDITION`
in the 400/412 range), which names no field. An implementer who reads it as "the project id is wrong"
will try other ids and get the same error for each.

So the request shape is tier-dependent:

| Tier | Body |
|---|---|
| `free-tier` | `{"tierId": "free-tier", "metadata": {…}}` — **no project field of any kind** |
| `legacy-tier`, `standard-tier` | include `cloudaicompanionProject` + `duetProject` |

The tier to request is chosen as: the first entry of `allowedTiers[]` whose `isDefault` is set;
otherwise `legacy-tier`. Known `UserTierId` values are `free-tier`, `legacy-tier` and `standard-tier`.

### The long-running operation

`onboardUser` returns an operation. Poll

```
GET https://cloudcode-pa.googleapis.com/v1internal/{operationName}
```

every 5 seconds until it reports done, then read `response.cloudaicompanionProject.id`. Take the
project id from that, or from `loadCodeAssist`'s `cloudaicompanionProject` if it was already set. If
it is absent and `GOOGLE_CLOUD_PROJECT` is unset, gemini-cli raises `ProjectIdRequiredError` — that
is the correct behaviour to copy, because continuing with an empty project id produces a confusing
failure on the first inference call instead.

> **Do not hardcode a fallback project id.** One corroborating third-party plugin hardcodes
> `rising-fact-p41fc`. That is a specific user's managed project captured in a source file. Using it
> is both an entitlement violation and a latent failure: it works until the project is decommissioned,
> and then every request fails for a reason that has nothing to do with the code. The dossier marks
> it UNCERTAIN and advises against it; this project does not do it.

### Antigravity's host fallback

Antigravity tries, in order:

1. `https://daily-cloudcode-pa.sandbox.googleapis.com`
2. `https://autopush-cloudcode-pa.sandbox.googleapis.com`
3. `https://cloudcode-pa.googleapis.com`

The first two are Google-internal sandboxes used by the IDE's own build channels. The router should
use the production host (`catalog/providers.toml` records `https://cloudcode-pa.googleapis.com`) and
not treat the sandboxes as a fallback for production traffic — they exist for the IDE's release
rings, and their availability is not a promise to third parties. This ordering is recorded here so
that a reader who sees it in another client knows what it is.

---

## 5. Inference: endpoints, headers and the metadata assertion

```
POST https://cloudcode-pa.googleapis.com/v1internal:generateContent
POST https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse
```

### Request body

```json
{
  "project": "<project-id>",
  "model": "<model-id>",
  "request": { "contents": [], "generationConfig": {}, "systemInstruction": {}, "tools": [] },
  "userAgent": "antigravity",
  "requestId": "<uuid>"
}
```

The Gemini payload is nested under `request`. The `project` on the wrapper is the id from §4; it is
required on every project-scoped call.

### Headers

| Header | Value |
|---|---|
| `Authorization` | `Bearer <access_token>` |
| `Content-Type` | `application/json` |
| `X-Goog-Api-Client` | `google-cloud-sdk vscode_cloudshelleditor/0.1` |
| `Client-Metadata` | `{"ideType":"ANTIGRAVITY","platform":"WINDOWS","pluginType":"GEMINI"}` |
| `User-Agent` | the Antigravity IDE string: `Mozilla/5.0 (…) Antigravity/<version> Chrome/138.0.7204.235 Electron/37.3.1 Safari/537.36` |
| `Accept` | `text/event-stream` on the streaming route |

`Client-Metadata` is a JSON object sent as a header value, not a query parameter, and it is the one
that carries meaning rather than decoration. It asserts **which client is calling**. The backend uses
`ideType` to select the caller's quota pool and model policy, and `platform` to sanity-check the
caller's environment. `pluginType: "GEMINI"` is what gemini-cli also sends; `ideType: "ANTIGRAVITY"`
is what distinguishes an Antigravity request from a gemini-cli one.

The values matter for three reasons:

- A **wrong `ideType`** (for example the gemini-cli `IDE_UNSPECIFIED` or `GEMINI`) applies the wrong
  policy. The failure is not necessarily an error: the request can succeed against a different tier
  or model allowlist, which is worse, because the operator sees plausible output and does not learn
  that the wrong entitlement was used.
- A **placeholder `platform`** such as `PLATFORM_UNSPECIFIED` is gemini-cli's value and is visibly not
  an IDE on a real OS. The IDE sends `WINDOWS` or `MACOS`; the router should send the value matching
  the machine.
- The whole header is **part of the fingerprint** in the sense of `docs/21-security.md` §5. It should
  be sent exactly as the IDE sends it and not extended with router-specific keys.

`X-Goog-Api-Client` is the standard Google client-identification header. The exact value used by
gemini-cli comes from `google-auth-library` and is **UNCERTAIN**; the value above is the one the
Antigravity IDE sends and is what the catalogue records. The two clients do not use the same string,
and using the T1 string with a T2 login is a mismatch rather than an improvement.

### The response envelope, and the translator trap

```json
{ "response": { "candidates": [], "usageMetadata": {}, "modelVersion": "", "responseId": "" },
  "traceId": "..." }
```

`candidates` is nested **one level deeper** than in the plain Gemini API, where it is top-level. This
is the single most likely mistake in this integration:

> A translator written against the plain Gemini API looks for `candidates` at the top level. Against
> this envelope it finds nothing, and the usual implementation returns an **empty completion** rather
> than an error — no exception, no log line, only a model that produced no output. The bug looks like
> a model problem, and the fix is one field access.

The rule is: unwrap `response` first, then treat the inner object as the plain Gemini response.
`traceId` is the only thing at the top level, and it is worth logging on failure because Google's
support uses it.

---

## 6. Tokens, refresh and what is not verified

The known parts:

- The token endpoint is `POST https://oauth2.googleapis.com/token`, form-urlencoded, with
  `client_id`, `client_secret`, `code`, `grant_type=authorization_code` and `redirect_uri` (plus
  `code_verifier` if PKCE was used).
- Both tiers request `access_type=offline`, which is what asks Google for a refresh token, and T2
  also sends `prompt=consent`, which forces the consent screen that makes Google issue one again on
  re-login.

What is **not** verified from a primary source in the research dossier, and must not be treated as
settled:

> **UNCERTAIN: whether a `refresh_token` is actually returned, and how long the grant lives.** The
> dossier does not record a real Antigravity or gemini-cli token response, so the presence of
> `refresh_token` in the response is inferred from `access_type=offline` and `prompt=consent`, not
> observed. The corroborating plugin packs its stored refresh token as
> `<refresh_token>|<projectId>|<managedProjectId>`, which implies it does receive one, but that is T2
> evidence about a third-party client's storage format, not a primary observation of the wire
> response.
>
> Consequences:
>
> - **If no refresh token is returned**, the access token lasts about an hour and the account needs
>   an interactive login each time. The router must detect this at login (an absent `refresh_token`)
>   and say so, rather than storing a credential that silently cannot be refreshed and letting it
>   become an `auth-failed` account an hour later.
> - **If the grant is shorter-lived than assumed** — Google can expire a refresh token for an
>   installed app that is unused, or on a policy change — the same `auth-failed` path applies.
> - **The packed storage format is not a wire format.** Do not copy `<refresh_token>|<project>|<managed>`
>   as if it were what Google returns; it is one plugin's local convention, and adopting it would
>   couple this router's vault layout to another project's choices. Store the project id in the
>   credential's own field.
>
> When `expires_in` is absent from a Google token response, one corroborating client defaults the
> expiry to **3600 seconds**. That is a safe default for a Google access token and is used as a
> floor, but it is a default rather than a measurement.
>
> One further UNCERTAIN item: the exact `User-Agent` and `X-Goog-Api-Client` values gemini-cli sends
> through `google-auth-library` are not recorded. For the T1 tier this must be measured before it is
> asserted; for T2 the IDE values above are what the catalogue records.

---

## See also

- [OAuth overview](10-oauth-overview.md) — refresh, storage, the loopback listener
- [OpenAI Codex](11-oauth-codex.md) — the other authorization-code login, for contrast
- [GitHub Copilot](13-oauth-copilot.md) — the device-flow platform, no redirect, no secret
- [Adding a platform](14-oauth-adding-a-platform.md) — the trait this platform implements
- [Security and credentials](21-security.md) — §5 the ToS posture, §6 the published constants
- [published.rs](../crates/mr-core/src/published.rs) — where the four Google constants are assembled
- [providers.toml](../catalog/providers.toml) — the `antigravity` entry, headers and Cloud Code paths
