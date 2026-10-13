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

**It depends on the provider, and the catalogue records which.** There is no rule to infer.

The relay appends the route path to the base URL, so:

| `base_url` | Request goes to |
|---|---|
| `https://api.example.com/v1` | `https://api.example.com/v1/chat/completions` |
| `https://api.example.com` | `https://api.example.com/chat/completions` |

The first is the more common convention, and it is what the shipped templates use. A provider with the
second will return a 404 on `/chat/completions`, which is at least an unambiguous symptom.

Some providers put the version elsewhere — `https://api.example.com/openai/v1`, or with an account id
in the path. The base URL is whatever precedes the route.

## Choosing the auth kind

| Kind | What it sends | Use when |
|---|---|---|
| `bearer` | `Authorization: Bearer <key>` | The OpenAI convention. Most providers. |
| `api-key-header` | `api-key: <key>` | Azure OpenAI and friends. |
| `header` | A header of your choosing | Providers with a bespoke header. |
| `query-param` | `?key=<key>` | Rare, and worth avoiding — a key in a URL ends up in logs, proxies and browser history. |

A provider that wants `api-key` and receives `Authorization` returns 401, not an error explaining the
mix-up. That is why the template records it rather than the router trying both: sending a credential
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
