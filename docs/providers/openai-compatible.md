# OpenAI-compatible providers

Most providers offer an OpenAI-shaped `/v1/chat/completions`. Adding one is three fields and one
command.

```bash
mr provider add myprov \
  --dialect open-ai \
  --base-url https://api.myprov.com/v1 \
  --auth bearer \
  --capability chat --capability tool-calling
printf '%s' "$MYPROV_KEY" | mr account add myprov --label main --secret-stdin
mr provider test myprov
```

## Does `/v1` belong in the base URL?

**Usually, and the relay will not guess twice.** The base URL is whatever precedes the route, and
the version segment is handled:

| `base_url` | Request goes to |
|---|---|
| `https://api.example.com/v1` | `https://api.example.com/v1/chat/completions` |
| `https://api.example.com` | `https://api.example.com/v1/chat/completions` |

A base URL that already ends in a version segment (`/v1`, `/v4`, `/v1beta`) supplies it, and the
relay does not add another. A base URL that carries none gets the dialect's version, so the same
provider is reached at the same URL whether the request was passed through or translated — which
is the point: the two used to differ, and a provider that only serves one of them failed half the
time.

**The exception is a provider whose endpoint has no version at all** — DeepSeek's chat path and
Perplexity's are both at the origin root. Those are recorded in the catalogue as `route_overrides`,
and the relay uses them as written:

```jsonc
{
  "id": "perplexity",
  "base_url": "https://api.perplexity.ai",
  "route_overrides": { "chat-completions": "chat/completions" }
}
```

The same mechanism is the escape hatch for any other deviation — a different responses path, a
model-scoped prefix, an account id in the path. The key is the route the **client** called, in its
kebab-case spelling (`chat-completions`, `responses`, `embeddings`, `messages`, …), and the value is
the path appended to the base URL. The full contract is in
[the configuration guide](../03-configuration.md#route_overrides).

Some providers put the version elsewhere — `https://api.example.com/openai/v1`, or with an account id
in the path. The base URL is whatever precedes the route, and the relay takes the version segment
from it wherever it sits.

## Choosing the auth kind

| Kind | What it sends | Use when |
|---|---|---|
| `bearer` | `Authorization: Bearer <key>` | The OpenAI convention. Most providers. |
| `api-key-header` | `x-api-key: <key>` (the header name is configurable) | Anthropic, Google, and others that name their own header. |
| `header` | A header of your choosing | Providers with a bespoke header. |
| `query-param` | **not sent** | The relay never puts a credential in a URL — a key in a URL ends up in logs, proxies and browser history. A provider that only accepts `?key=` cannot be used as it stands; use a header the provider also accepts. |

A provider that wants `x-api-key` and receives `Authorization` returns 401, not an error explaining
the mix-up. That is why the template records it rather than the router trying both: sending a credential
in a header the provider does not expect is a credential leaked to a log for no benefit.

## Why discovery can lie

`mr models sync <provider>` calls `/v1/models` and records what comes back. Treat it as a starting
point, not as truth:

- **It lists models you cannot use.** A `/models` endpoint frequently returns the provider's entire
  catalogue, including models your account tier cannot call, models retired for new customers, and
  models that only exist as aliases for a newer version.
- **It omits models you can.** Some providers gate `/models` behind a scope, or list only a curated
  subset.
- **It says nothing about capabilities.** There is no standard field for "supports tool calling" or
  "supports vision". A model that appears in `/models` and rejects `tools` is a runtime discovery.
- **It changes without warning.** A provider can add, remove or rename a model at any time.

The catalogue has a `capabilities` list per provider, and `mr models list --source` shows which
entries were discovered and which were entered by hand. **A sync never overwrites hand-entered
metadata** — if it did, an operator's correction would silently revert on the next sync.

## Passthrough: why an OpenAI client to an OpenAI provider is verbatim

When the incoming request's dialect and the target provider's dialect both resolve to `open-ai`, the
relay **forwards the original bytes unchanged**. It does not parse, translate, or re-serialise.

Two consequences, and both are the reason for the rule:

1. **It is the fastest path.** No JSON parse, no re-encode, no allocation for the body. The relay's
   only work is a targeted scan for the `model` field — see
   [Latency engineering](../16-latency-engineering.md).
2. **It preserves fields the router does not understand.** Providers add request fields faster than
   any router can track: `reasoning_effort`, `logprobs`, vendor-specific caching hints, a new
   `response_format` variant. A router that parses and re-serialises a body it half-understands
   **silently deletes** those fields, and the client's feature stops working with no error. Verbatim
   forwarding means an unknown field reaches the provider exactly as sent.

The dialect layer's job is therefore to decide **whether** to translate, not to translate
unconditionally. An OpenAI-shaped client hitting an Anthropic provider is translated; hitting an
OpenAI provider is not.

## Capabilities to declare honestly

| Capability | Declare when |
|---|---|
| `chat` | `/v1/chat/completions` works. |
| `responses` | `/v1/responses` works. Separate from `chat`, because many providers have one and not the other. |
| `embeddings` | `/v1/embeddings` works. |
| `tool-calling` | The provider accepts a `tools` array **and** returns valid `tool_calls`. |
| `vision` | The provider accepts image content parts. |
| `reasoning` | The provider exposes reasoning output or an effort control. |

Declaring a capability the provider does not have does not make it work; it makes the router attempt
something that fails. Declaring too few is the safer error, because the router simply does not try.

## Fingerprint headers

A `[providers.headers]` table sends fixed headers on every request:

```toml
[providers.headers]
"HTTP-Referer" = "https://github.com/your/repo"
"X-Title" = "Manarouter"
```

This is how OpenRouter's attribution headers are configured, and it is also how a provider that
requires a specific `User-Agent` or an organisation header is satisfied. Nothing secret belongs here —
these are in `config.json` in plaintext.

## See also

- [The catalogue](catalog.md) — which providers are already covered.
- [Anthropic-compatible](anthropic-compatible.md) — the other large family.
- [Configuration](../03-configuration.md) — the provider schema in full.
- [Key selection](../05-key-selection.md) — several keys on one provider.
