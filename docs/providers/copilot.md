# GitHub Copilot

Copilot is reached through a GitHub account rather than an API key, and it serves several API shapes
from one endpoint. It is the most header-sensitive provider in the catalogue.

Login is documented in [OAuth: Copilot](../13-oauth-copilot.md). This page covers the provider.

## Why it is unusual

Three things distinguish it from every other provider:

1. **A two-stage credential.** A long-lived GitHub token is exchanged for a **short-lived Copilot
   session token**, and the session token is what the API accepts. There is no OAuth refresh: the
   session token is re-minted.
2. **`Editor-Version` is mandatory.** Its absence is a hard 400.
3. **`Copilot-Integration-Id` selects the model policy.** The wrong value does not fail — it silently
   changes which models you get.

## Minting a session token

```http
GET https://api.github.com/copilot_internal/v2/token
```

Response fields that matter:

| Field | Meaning | Trap |
|---|---|---|
| `token` | The session token | **Opaque.** See below. |
| `expires_at` | Expiry | **Unix seconds, not milliseconds.** |
| `refresh_in` | Seconds until a refresh is advisable (~1500) | Not an expiry. |
| `endpoints.api` | The API host to use | Preferred source for the host. |
| `sku` | The subscription tier | The basis for a plan label. |

### `expires_at` is in seconds

A values-in-seconds field treated as milliseconds produces an expiry roughly 50 000 years in the
future, so the token is never refreshed and the account fails at a random later moment with a 401 that
looks unrelated. The reverse mistake expires instantly and every request refreshes.

The adapter converts explicitly and the conversion is tested, because this is a bug that survives
casual testing in one direction and is invisible in the other.

### The token is opaque

The session token has the shape `k=v;k=v;…:<hmac>`. It looks parseable, and it is a mistake to parse
it: the key-value pairs are not a documented interface, some are duplicated, and the trailing segment
is a signature. **The adapter treats the whole string as opaque** and passes it through unchanged.

Parsing it would work until GitHub changed a field, at which point the router would be extracting a
hostname from a string whose format it assumed.

### `copilot_plan` is not a real field

It is **synthesised from `sku`**. Anyone "fixing" the code by reading a nonexistent response field will
find it always empty. The catalogue records this so the next person does not try.

## Host selection

```
use endpoints.api if present
otherwise derive from proxy-ep, replacing a leading "proxy." with "api."
```

The fallback exists because `endpoints.api` is not always present. The `proxy.` → `api.` rewrite is
the documented transformation; note it is a **prefix** replacement, not a substring one, so a host
that already begins with `api.` is left alone.

## The mandatory headers

```http
Editor-Version: <as recorded in the catalogue>
Copilot-Integration-Id: <as recorded in the catalogue>
```

- **`Editor-Version` is mandatory.** Omitting it is a hard 400 with no useful explanation. This is the
  single most likely reason a hand-rolled integration fails.
- **`Copilot-Integration-Id` selects the model policy.** Critically, it must **not** be
  `copilot-language-server`: that value is associated with a policy that does not grant the intended
  model access. The wrong value produces a working connection with the wrong models available, which
  is far harder to diagnose than a refusal.

Both are fingerprint headers, declared as data in the catalogue rather than buried in code, because
they are version-dependent and are the first thing to check when Copilot breaks.

## Dialects served

One endpoint serves several shapes, and the adapter picks the right path per capability:

| Path | Shape |
|---|---|
| `/chat/completions` | OpenAI Chat Completions |
| `/responses` | OpenAI Responses |
| `/v1/messages` | **Anthropic Messages, natively** |
| `/models` | Model list |

The native `/v1/messages` is worth preferring
([Anthropic-compatible §native beats translated](anthropic-compatible.md#native-v1messages-beats-a-translated-one)).

## The account model

One GitHub account per credential. The **long-lived GitHub token** is what persists in the vault; the
Copilot session token is derived and short-lived, and is never written to disk as though it were the
credential.

Several accounts attach to one provider entry as usual. A Copilot subscription has its own rate
limits, so `reset-aware-priority` and a multi-account setup are both useful here.

### Is the GitHub token single-use?

The research dossier records that Copilot has **no OAuth refresh at all** and that the underlying
GitHub token is long-lived and does not rotate. An earlier summary claimed the refresh token was
single-use.

Both may be true of different flows. What the implementation does: **the single-flight refresh guard
is applied unconditionally.** It is load-bearing for Codex, where a double refresh permanently breaks
the account, and it is defensive here, where non-rotation is reported but not something the router
should stake an account on.

## Capabilities

`chat`, `responses`, `messages`, `tool-calling`, `vision`, `models`.

## See also

- [OAuth: Copilot](../13-oauth-copilot.md) — the device flow and the token exchange.
- [The catalogue](catalog.md) — the `github-copilot` entry.
- [Anthropic-compatible](anthropic-compatible.md) — the native Messages path.
- [Quota tracking](../08-quota-tracking.md) — subscription windows.
