# Codex (OpenAI subscription)

The Codex dialect reaches OpenAI models through a **ChatGPT subscription** rather than a metered API
key. It is the difference between a flat monthly cost and a per-token bill, which is why operators want
it first in a combo.

Login is documented separately in [OAuth: Codex](../11-oauth-codex.md). This page covers the provider
itself.

## Why it needs its own dialect

It is not `/v1/chat/completions` with a different host. Four things differ, and each one breaks a
plain OpenAI client:

| | Standard OpenAI | Codex |
|---|---|---|
| Endpoint | `POST api.openai.com/v1/chat/completions` | `POST chatgpt.com/backend-api/codex/responses` |
| Account routing | An `OpenAI-Organization` header | **`chatgpt-account-id`**, from a JWT claim |
| Originator | — | **`originator: codex_cli_rs`** |
| Body constraints | Free | `store: false` and `stream: true` are **hard-coded** |

The host is not `api.openai.com`, which is the first thing people get wrong.

## The inference endpoint

```http
POST https://chatgpt.com/backend-api/codex/responses
chatgpt-account-id: <from the JWT>
originator: codex_cli_rs
User-Agent: codex_cli_rs/<version>
```

### `chatgpt-account-id` comes from the token, not from configuration

The account id is a claim inside the access token's JWT, at
`https://api.openai.com/auth` → `chatgpt_account_id`.

**Sending a literal value is a hard failure.** It is not defaulted, not guessed, and not optional —
so an operator who tries to set it by hand gets an error rather than a request routed to the wrong
account.

The claim is read without verifying the JWT signature, and that is correct here. The token was
obtained from OpenAI over TLS in the same process; verifying it would require fetching and caching
JWKS purely to confirm something already established, and would introduce a new network dependency
on the request path. The claim is read for routing, not for trust.

### `originator` is mandatory

`originator: codex_cli_rs` identifies the caller as the official CLI. Omitting it, or sending anything
else, is refused — and the refusal does not say "you are missing originator", so it presents as an
authentication problem.

### The body constraints are not suggestions

- **`store: false`** — the request must not be retained server-side. Setting `true` is refused.
- **`stream: true`** — the Responses endpoint here is stream-only. A client asking for a
  non-streamed response gets one synthesised by the relay, which consumes the SSE stream and returns a
  single JSON body.

## The `instructions` question — UNCERTAIN

**This is the highest-risk uncertainty in the integration.**

Historically, base instructions were a top-level `instructions` field alongside `input`. Current
versions of the first-party client appear to have moved them **inside `input` as a `BaseInstructions`
item** instead.

| If the implementation is… | Then… |
|---|---|
| **Right** (instructions in `input`) | Everything works. |
| **Wrong** (a top-level field is still required) | The request is rejected, or instructions are silently ignored — which is worse, because the model answers without its system prompt and the output is subtly wrong rather than obviously broken. |

The implementation here does **not** do that: `mr-dialects/src/responses.rs` folds the system
message into a top-level `instructions` field and emits no `BaseInstructions` item at all. That is
what the code sends today, and it is not flagged UNCERTAIN at that site; only a live request against
a real subscription settles which form the endpoint accepts.

## Fingerprint headers

| Header | Value | Note |
|---|---|---|
| `originator` | `codex_cli_rs` | **Mandatory.** |
| `chatgpt-account-id` | From the JWT | **Mandatory**, and never configured by hand. |
| `User-Agent` | `codex_cli_rs/<version>` | Sent by the first-party client. |
| `OpenAI-Beta` | `responses=experimental` | **UNCERTAIN.** Needed by older servers; sent for compatibility. A server that rejects an unknown beta header would refuse the request. |

## The account model

One OAuth credential per ChatGPT account. Several accounts can be attached to one provider entry, and
[account selection](../05-key-selection.md) applies as usual — which matters here, because a
subscription has its own usage windows and one account's window filling up is exactly the case
`reset-aware-priority` handles well.

Refresh tokens are **single-use**, and a refresh failure is permanent: the operator must log in again.
See [OAuth: Codex](../11-oauth-codex.md) for the flow and the refresh window.

## Capabilities

`responses`, `tool-calling`, `reasoning`, `vision`.

Note there is **no `chat`** capability. This provider speaks the Responses shape, not Chat
Completions, so a client asking for `/v1/chat/completions` against a Codex step is translated rather
than passed through.

## See also

- [OAuth: Codex](../11-oauth-codex.md) — the login flow, refresh, and the single-use token trap.
- [The catalogue](catalog.md) — the `openai-codex` entry.
- [Combos](../06-combos.md) — putting a subscription first.
- [Quota tracking](../08-quota-tracking.md) — subscription windows.
