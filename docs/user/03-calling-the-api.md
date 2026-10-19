# Calling the API

Use this guide to send requests from your application to the local Manarouter relay and diagnose the first response.

## Choose the relay, not the panel

The relay is the API listener; the web panel is a separate control listener. With the default ports, use:

- **OpenAI-compatible base URL:** `http://127.0.0.1:20900/v1`
- **Anthropic Messages base URL:** `http://127.0.0.1:20900` — do not add `/v1` to this base URL.
- **Panel:** `http://127.0.0.1:20901` — not an API endpoint.

Your installation may use different ports or bind addresses. Check the local configuration or [Troubleshooting](10-troubleshooting.md); do not expose the relay or panel to an untrusted network as a shortcut. See [Security and credentials](11-security.md) before using the relay beyond loopback.

Manarouter uses two separate credentials: your application presents a **Manarouter client key** to the relay, and Manarouter uses the configured provider credential to contact the upstream. Never use a provider credential as your client key. When key authentication is enabled, send the client key as either `Authorization: Bearer <client-key>` or `x-api-key: <client-key>`; both header forms are accepted on the relay routes. Keep real keys out of shell history, commands, logs, and conversations.

## Make an OpenAI-compatible chat request

Replace `PROVIDER/MODEL` with a model ID from `GET /v1/models`, such as a configured `provider/model` name, or with a combo or alias name you created. The API listing includes provider models and combos, not aliases; inspect aliases with `mr alias list`. The `MR_CLIENT_KEY` value is the secret you just entered interactively; it is not echoed. This Bash example passes the header to curl through its standard-input configuration:

```bash
read -rsp 'Manarouter client key: ' MR_CLIENT_KEY
printf '\n'
curl --config - <<CURL
url = "http://127.0.0.1:20900/v1/chat/completions"
request = "POST"
header = "Authorization: Bearer $MR_CLIENT_KEY"
header = "Content-Type: application/json"
data = "{\"model\":\"PROVIDER/MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}]}"
CURL
unset MR_CLIENT_KEY
```

For successful routing, the selected provider/model must be configured and visible, or the combo/alias must exist. A model supplied without a provider prefix does not resolve by itself: an unqualified name must be an alias or combo. Check provider models and combos with `GET /v1/models` and check aliases with `mr alias list` before changing your application. The listing comes from Manarouter's current routing configuration and does not query providers; it does not include aliases. Some registered endpoints, such as audio transcription and speech, have specialized request/response shapes and may be forwarded unchanged when cross-dialect translation does not apply. Follow the client API's required payload format.

To opt into server-sent event streaming, add `stream: true` to the JSON body. This complete curl example prompts for the client key without echoing it:

```bash
read -rsp 'Manarouter client key: ' MR_CLIENT_KEY
printf '\n'
curl --config - <<CURL
url = "http://127.0.0.1:20900/v1/chat/completions"
request = "POST"
header = "Authorization: Bearer $MR_CLIENT_KEY"
header = "Content-Type: application/json"
data = "{\"model\":\"PROVIDER/MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}],\"stream\":true}"
CURL
unset MR_CLIENT_KEY
```

Without `stream: true`, request a regular JSON response. Some upstream dialects stream regardless; when Manarouter translates such an upstream response for a non-streaming client, it folds the stream into a single document.

## Call the Anthropic Messages surface

Use `/v1/messages` on the relay root (not the OpenAI `/v1` base URL). This request includes Anthropic's required `max_tokens` field. The client key is still a Manarouter key; do not put an upstream provider secret here.

```bash
read -rsp 'Manarouter client key: ' MR_CLIENT_KEY
printf '\n'
curl --config - <<CURL
url = "http://127.0.0.1:20900/v1/messages"
request = "POST"
header = "x-api-key: $MR_CLIENT_KEY"
header = "anthropic-version: 2023-06-01"
header = "Content-Type: application/json"
data = "{\"model\":\"PROVIDER/MODEL\",\"max_tokens\":512,\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}]}"
CURL
unset MR_CLIENT_KEY
```

The Anthropic-shaped request is translated when the selected upstream uses a different dialect. Translation and route support depend on the configured upstream; the relay cannot make a provider support an API it does not offer.

## Routes and model discovery

The relay registers these API paths; whether a selected upstream can serve a route depends on that provider's capabilities and configuration.

| Client API shape | Method and path | Base URL |
|---|---|---|
| OpenAI Chat Completions | `POST /v1/chat/completions` | `http://127.0.0.1:20900` |
| OpenAI Responses | `POST /v1/responses` | `http://127.0.0.1:20900` |
| OpenAI embeddings | `POST /v1/embeddings` | `http://127.0.0.1:20900` |
| OpenAI audio transcription / speech | `POST /v1/audio/transcriptions`, `POST /v1/audio/speech` | `http://127.0.0.1:20900` |
| OpenAI image generation | `POST /v1/images/generations` | `http://127.0.0.1:20900` |
| Anthropic Messages / token counting | `POST /v1/messages`, `POST /v1/messages/count_tokens` | `http://127.0.0.1:20900` |
| OpenAI-compatible model list | `GET /v1/models` | `http://127.0.0.1:20900` |
| Liveness | `GET /healthz` | `http://127.0.0.1:20900` |

For OpenAI-compatible clients, the base URL therefore ends in `/v1`; for Anthropic clients, configure the relay root without `/v1` as the base URL and use the `/v1/messages` path. Use the model-list endpoint to inspect addressable provider models and combos; aliases are not included in the listing, so use `mr alias list` to inspect those. Provider entries include Manarouter metadata such as default reasoning level, supported levels, and input modalities. Authentication applies to `/v1/models` too when client-key authentication is enabled. `/healthz` only reports relay liveness; it does not test a provider or model.

For the syntax and behavior of reasoning suffixes such as `PROVIDER/MODEL-high`, see [Models and reasoning levels](04-models-and-reasoning-levels.md). For image/audio/video input and fallbacks, see [Images, audio, and video](07-images-audio-video.md).

## Understand common HTTP responses

| Status | Meaning and next step |
|---|---|
| `400 Bad Request` | The relay could not read a usable `model` field, or rejected a malformed request during dialect translation. Check the path's request shape and ensure the body contains a string `model`. |
| `401 Unauthorized` | A client key is missing or invalid when authentication is required. Check that you used a Manarouter client key from this instance, and used `Authorization: Bearer …` or `x-api-key`. A provider key is not interchangeable. |
| `404 Not Found` | A wrong path, unknown model, unknown provider prefix, or unresolved combo/alias can produce a 404. Verify the relay port and path, then compare the name to `GET /v1/models`, `mr combo list`, or `mr alias list`. A name-resolution miss is returned as 404, not 400. |
| `413 Payload Too Large` | The relay refuses request bodies larger than 32 MiB. Reduce the upload or split it if the client and provider permit that. |
| `429 Too Many Requests` | The relay's `no_usable_accounts` response means every account for the selected route steps is unavailable or cooling down. A provider's own rate-limit response may also be forwarded. Read the body and `Retry-After`; see [Troubleshooting](10-troubleshooting.md). |

If you send an API request to the panel listener, its `/v1/*` response identifies that it is the panel port and points you to the relay port. Change your API base URL; do not troubleshoot it as a model error.

## Not yet implemented

- **Client-key quotas and usage metering:** per-key rolling token/request caps are not enforced by the relay yet. The quota/usage view is not a reliable record of calls yet because the request path does not write usage rows. A configured limit must not be treated as an active safeguard.
- **Client-key request-count and model/route restrictions:** the legacy `--limit-requests` field and model/route allow-list fields are not enforced on API requests yet. Do not use these fields as access controls.
- **Automatic modality fallback:** the relay detects an unsupported input modality and retries the whole request against the configured `routing.defaults` entry for that modality, with that model's own provider, account, credential and upstream URL. The budget is one fallback: if the default cannot carry the modality, or names no known model, the request fails with HTTP 501 (`modality_fallback_unavailable`). A default makes a request work only when it names a model that really accepts the modality; see [Images, audio, and video](07-images-audio-video.md).

For more request and response details, consult the [API reference](../18-api-reference.md). For command syntax, see the [CLI reference](12-cli-reference.md).
