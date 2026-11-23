## Mistral Vibe API (`mistral-vibe-api`)

**OAuth (recommended).** The panel signs a Mistral account in through the same login the Mistral CLI
performs: a loopback PKCE flow against Mistral's Ory deployment (`auth.mistral.ai`), minting a vibe
token for `https://api.mistral.ai/v1`. Add an account and choose **Sign in**; a browser opens, you
approve, and the token lands in the encrypted vault with automatic refresh. The provider needs no
contact with Mistral's consumer website — the wire is the OpenAI-compatible platform API, forwarded
byte-for-byte from an OpenAI client.

**API key (still supported).** Existing bearer accounts keep working after the flip: an account with
no OAuth grant falls back to its vault secret (the relay and conductor both fall through to
`secret_ref` when `oauth_ref` is absent). A new account can still be added with an API key via
`mr account add mistral-vibe-api --secret-stdin` (the key itself goes in through your terminal, never
in this chat).

**Known unknown.** Whether `api.mistral.ai` accepts the minted Ory token is UNVERIFIED in this build:
an earlier revision of this entry recorded the API rejecting both Ory tokens with `Invalid API Key`,
while the operator's direction for this revision is the CLI-mimicking login that is now implemented.
The first live login (panel **Sign in**) is the verification step. If the login completes but
requests then fail with `401`, the first knob is the bearer in
`crates/mr-core/src/oauth/mistral_api.rs` (`apply_credential`): it sends the audience-scoped access
token, and the JWT `id_token` is preserved in the token set's `extra` as the documented alternative.
No provider migration is needed either way — bearer accounts are not affected.

The two surfaces use different credentials, so do not paste a platform API key into a consumer-surface
prompt or vice versa. The relay forwards chat-completions requests on the **OpenAI wire**: `model`,
`messages`, `stream` pass through and the response is streamed back as OpenAI SSE, preserving usage
(and converting any Mistral streaming deltas into the same shape). `developer` messages are rewritten
to `system` for this upstream.

The dialect id is `mistral-vibe-api`, so a configured model is selected as
`mistral-vibe-api/<model>`.
