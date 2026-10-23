# Antigravity (Google Cloud Code Assist)

Antigravity reaches Gemini models through Google's **Cloud Code Assist** API using an OAuth account
rather than a metered API key. The inference shape is Gemini's `generateContent`, wrapped one level
deeper.

Login is documented in [OAuth: Antigravity](../12-oauth-antigravity.md). This page covers the provider.

## Two tiers, and which one is default

| Tier | Client | Provenance | Status |
|---|---|---|---|
| **T1** | gemini-cli | Google's own open-source repository | Code-level constructor only; not runtime registered. |
| **T2** | Antigravity IDE | Extracted from the shipped application | Runtime registry default; **UNCERTAIN / experimental.** |

The OAuth registry defaults to T2 because the provider catalogue is specifically an Antigravity IDE
client. The Google-published T1 client is available as an explicit code-level alternative, but is not
a runtime panel choice and its account identity/quota behavior does not impersonate Antigravity. T2's
client came from a shipped bundle, so its longevity is not guaranteed and its use is a judgement call the operator
makes explicitly.

**UNCERTAIN (T2):** because it was reverse-engineered rather than published, a change in the shipped
app can invalidate it without notice. The failure is `invalid_client` at the token endpoint, which is
at least unambiguous.

## The inference shape

```http
POST https://cloudcode-pa.googleapis.com/v1internal:generateContent
POST https://cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse
```

with `X-Goog-Api-Client` and a `Client-Metadata` header carrying:

```json
{ "ideType": "ANTIGRAVITY", "...": "..." }
```

Two things differ from plain Gemini:

1. **The path is `v1internal:`-prefixed**, not the public `v1beta/models/{model}:generateContent`.
2. **The response is wrapped:**

   ```json
   { "response": { "candidates": [ ... ] }, "traceId": "..." }
   ```

   The candidates are **one level deeper** than plain Gemini. A parser written for the public API
   finds nothing at `candidates` and reports an empty response rather than an error — which is why
   [the google dialect](../01-architecture.md) unwraps the wrapper before parsing.

`traceId` is not in the candidates and is worth logging on failure: it is what Google's support asks
for.

## Project onboarding

Before inference works, the account must be onboarded, and this is where the documented trap lives.

```
loadCodeAssist   ->  which tier is this account on?
onboardUser      ->  create or select the project
poll the long-running operation until it completes
```

### The trap: omit `project` for a free-tier account

**For a `free-tier` account the `project` field must be omitted.** Sending one — even the account's own
default project — fails with `Precondition Failed`.

A paid-tier account is not supported by the current onboarding path: it fails closed rather than sending a project. The rule is a property of the tier, not of the
account, which is why the adapter decides it from the `loadCodeAssist` result rather than from
configuration. The decision is a pure function and is tested without a network.

An `invalid_client` failure during onboarding is a T2-tier problem; a `Precondition Failed` is almost
always this rule.

## Scopes

The implementation currently supports no paid-tier selection: without an explicitly supplied project it fails closed. Free-tier onboarding omits all project fields and polls a bounded number of times for the managed project. T2's client metadata remains experimental; do not treat the reverse-engineered flow as provider-verified.

The gemini-cli tier requests:

```
https://www.googleapis.com/auth/cloud-platform
https://www.googleapis.com/auth/userinfo.email
https://www.googleapis.com/auth/userinfo.profile
```

The Antigravity tier adds:

```
https://www.googleapis.com/auth/cclog
https://www.googleapis.com/auth/experimentsandconfigs
```

The extra two are what the IDE requests. Requesting more than necessary is visible to the user on the
consent screen, which is why the extra two sit on the default T2 client while gemini-cli (T1) keeps the smaller set.

## PKCE, and a contradiction worth recording

An earlier internal summary claimed the Antigravity flow uses **no PKCE**. The research dossier shows
its authorization URL carrying `code_challenge` and `code_challenge_method=S256`, and its token
exchange carrying `code_verifier`.

**Implementation assumption, not verification:** this build sends S256 PKCE, following the dossier despite conflicting evidence. The distinction is not academic: a token exchange that omits
`code_verifier` against a flow that set a challenge is rejected with `invalid_grant`, which is
indistinguishable from a wrong or expired code and sends you looking in the wrong place.

Note also that gemini-cli's own **loopback** path uses no PKCE while its **user-code** path does.
Both are in the same client. This is why the trait allows PKCE to be absent per platform rather than
assuming one convention.

## Redirect URI

```
http://localhost:51121/oauth-callback
```

`localhost`, not `127.0.0.1`, and a **fixed** port. This is the opposite of the gemini-cli loopback,
which uses `127.0.0.1` and an ephemeral port. The provider compares the redirect byte for byte, so a
shared helper that normalised the two hosts would break one of the two registrations.

## Fingerprint headers

| Header | Note |
|---|---|
| `X-Goog-Api-Client` | The exact value gemini-cli sends. **UNCERTAIN** — recorded from research rather than from a published specification. |
| `User-Agent` | **UNCERTAIN**, same provenance. |
| `Client-Metadata` | Carries `ideType: ANTIGRAVITY`. Required by the internal endpoint. |

## Refresh tokens

**UNCERTAIN:** whether the flow returns a `refresh_token` at all, and how long the grant lasts, is not
established from a primary source. If no refresh token is returned, the account must be re-authorised
when the access token expires, and the panel says so rather than showing an account that will fail.

The storage format `<refresh_token>|<projectId>|<managedProjectId>` seen in one plugin is that
plugin's local convention, **not** a wire format, and is not reproduced.

## Capabilities

`chat`, `tool-calling`, `reasoning`, `vision`, `models`.

## Legal and ToS note

T1 uses a client Google publishes for this purpose. T2 uses credentials extracted from a shipped
application. That difference is a real one and the operator choosing T2 should understand it. See
[Security](../21-security.md).

## See also

- [OAuth: Antigravity](../12-oauth-antigravity.md) — the flow, the two tiers, and PKCE.
- [The catalogue](catalog.md) — the Antigravity entry.
- [Architecture](../01-architecture.md) — the wrapped response and the dialect layer.
- [Security](../21-security.md) — the published-constant posture.
