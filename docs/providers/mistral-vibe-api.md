## Mistral Vibe API (`mistral-vibe-api`)

**OAuth (recommended).** The panel signs a Mistral account in through the same browser sign-in the
Mistral CLI performs: the conductor creates a sign-in process on Mistral's own console
(`https://console.mistral.ai/api/vibe/sign-in`), opens that page, polls it, and exchanges the
completed sign-in for the platform's real API key. Add an account and choose **Sign in**; approve on
Mistral's page, and the extracted API key lands in the encrypted vault. There is no Ory token and no
refresh grant: if the key is revoked, sign in again. The provider needs no contact with Mistral's
consumer website — the wire is the OpenAI-compatible platform API, forwarded byte-for-byte from an
OpenAI client.

**API key (still supported).** Existing bearer accounts keep working after the flip: an account with
no OAuth grant falls back to its vault secret (the relay and conductor both fall through to
`secret_ref` when `oauth_ref` is absent). A new account can still be added with an API key via
`mr account add mistral-vibe-api --secret-stdin` (the key itself goes in through your terminal, never
in this chat).

No provider migration is needed either way — bearer accounts are not affected.

The two surfaces use different credentials, so do not paste a platform API key into a consumer-surface
prompt or vice versa. The relay forwards chat-completions requests on the **OpenAI wire**: `model`,
`messages`, `stream` pass through and the response is streamed back as OpenAI SSE, preserving usage
(and converting any Mistral streaming deltas into the same shape). `developer` messages are rewritten
to `system` for this upstream.

The dialect id is `mistral-vibe-api`, so a configured model is selected as
`mistral-vibe-api/<model>`.
