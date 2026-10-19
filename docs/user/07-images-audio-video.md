# Images, audio, and video

Use the relay's supported API routes for media requests, and check the selected provider's capabilities and wire format before sending real media; route availability does not mean every provider accepts every media type.

## Available request routes

The relay exposes OpenAI-compatible chat completions, Responses, image generation, audio transcription, and audio speech routes, plus Anthropic Messages. See the [API reference](../18-api-reference.md) for the available request routes and authentication. Providers differ in which routes they can serve, and a request can incur provider charges.

## Image input

OpenAI-style `image_url` content can be translated for some cross-dialect routes, including Anthropic Messages and Gemini-compatible providers. Depending on the destination, a data URL is carried as base64 image data; a plain URL is passed as a URL or provider file reference. Anthropic image blocks also have a translation path to OpenAI-style data URLs.

This does not make every provider support image input. Use a model whose provider accepts the image format and route you send, and consult the [provider guides](../providers/README.md) when selecting a route.

## Audio, video, and automatic fallback

The relay exposes audio transcription and speech-generation endpoints, but support for those routes depends on the selected provider. Chat-style media payloads are not universally converted between provider formats. For example, the Gemini chat translator currently handles text and image parts; it drops unsupported part types such as audio rather than translating them. Check the provider's documented API shape and capabilities; do not assume audio can be sent unchanged to a different dialect.

Video is not a generally supported cross-provider feature. The relay detects some media payloads and can identify a configured default, but it currently returns HTTP 501 when an unsupported modality would require fallback: it cannot yet reroute the whole request to a resolved provider/account with matching credentials and URL. Setting a modality default therefore does not make an unsupported request work today. No model splitting occurs for a request containing multiple media types. Use a provider and model that explicitly support the exact route and payload format.

For errors, verify the route and payload first, then use [Troubleshooting](10-troubleshooting.md). Keep provider credentials and client keys out of requests shared for support; see [Security and credentials](11-security.md).
