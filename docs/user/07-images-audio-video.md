# Images, audio, and video

Use the relay's supported API routes for media requests, and check the selected provider's capabilities and wire format before sending real media; route availability does not mean every provider accepts every media type.

## Available request routes

The relay exposes OpenAI-compatible chat completions, Responses, image generation, audio transcription, and audio speech routes, plus Anthropic Messages. See the [API reference](../18-api-reference.md) for the available request routes and authentication. Providers differ in which routes they can serve, and a request can incur provider charges.

## Image input

OpenAI-style `image_url` content can be translated for some cross-dialect routes, including Anthropic Messages and Gemini-compatible providers. Depending on the destination, a data URL is carried as base64 image data; a plain URL is passed as a URL or provider file reference. Anthropic image blocks also have a translation path to OpenAI-style data URLs.

This does not make every provider support image input. Use a model whose provider accepts the image format and route you send, and consult the [provider guides](../providers/README.md) when selecting a route.

## Audio, video, and automatic fallback

The relay exposes audio transcription and speech-generation endpoints, but support for those routes depends on the selected provider. Chat-style media payloads are not universally converted between provider formats. For example, the Gemini chat translator currently handles text and image parts; it drops unsupported part types such as audio rather than translating them. Check the provider's documented API shape and capabilities; do not assume audio can be sent unchanged to a different dialect.

Video is not a generally supported cross-provider feature. Use a provider and model that explicitly support the exact route and payload format.

### Automatic modality fallback

When a request carries a modality the selected route cannot accept — an image sent to a text-only model, for example — the relay looks for a `routing.defaults` entry for that modality and retries the **whole request** against it: the default is resolved against the same routing snapshot, and the retry uses that model's own provider, account, credential, upstream URL and dialect. The original route is not asked to serve a model it does not have.

The rules are narrow on purpose:

- One fallback, not a chain. A default that cannot carry the modality either fails the request with HTTP 501 (`modality_fallback_unavailable`) rather than trying again.
- A default that names no known model fails the same way, with a message saying so, instead of routing to something the operator did not name.
- A request is never split: one request goes to one model. No model splitting occurs for a request containing multiple media types.

If no default is configured for the modality, the request fails with HTTP 501 as before. Setting a default makes an unsupported request work *only* when that default names a model that really accepts the modality.

For errors, verify the route and payload first, then use [Troubleshooting](10-troubleshooting.md). Keep provider credentials and client keys out of requests shared for support; see [Security and credentials](11-security.md).
