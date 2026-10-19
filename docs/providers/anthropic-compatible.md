# Anthropic-compatible providers

The Anthropic Messages API is a different shape from OpenAI's, and several providers offer it
natively — Anthropic itself, and a growing number of gateways and self-hosted servers.

```bash
mr provider add anthropic --from-catalog anthropic
printf '%s' "$ANTHROPIC_API_KEY" | mr account add anthropic --label main --secret-stdin
mr models sync anthropic
```

## Native `/v1/messages` beats a translated one

There are two ways to reach an Anthropic model:

1. **A provider that serves `/v1/messages` natively.** The relay forwards the request almost verbatim.
2. **An OpenAI-shaped gateway in front of an Anthropic model.** The relay translates Messages →
   Chat Completions, then the gateway translates back.

**Prefer the first, always.** The second round trip loses information at every hop, and the losses are
not symmetric:

- **Reasoning blocks.** Anthropic returns `thinking` blocks with a signature. A translation that
  flattens them to text loses the signature, and the next turn that echoes them back is rejected.
- **Cache control.** `cache_control` breakpoints are Anthropic-specific and have no OpenAI equivalent.
  A translated request silently loses prompt caching, which changes the bill without changing the
  output.
- **`tool_use` / `tool_result` semantics.** Anthropic's blocks are a structured conversation, not a
  message with a `tool_calls` array. The mapping is lossy in edge cases, particularly with parallel
  tool calls.
- **Stop reasons.** `stop_sequence` and `max_tokens` are distinct, and a translator that maps both to
  `stop` loses the ability to tell a truncation from a completion.

The router prefers a native endpoint wherever the catalogue knows of one, and
[the catalogue](catalog.md) records which providers have one.

## Headers

```http
x-api-key: sk-ant-...
anthropic-version: 2023-06-01
```

- **`x-api-key`**, not `Authorization: Bearer`. The relay accepts either *from clients* on any route,
  but sends the provider what the provider expects. Getting this backwards produces a 401 that looks
  like a bad key.
- **`anthropic-version`** is required by the real API. The relay sends the version the catalogue
  records and forwards a client's own value where the target is Anthropic-shaped.

## `max_tokens` is required, and defaults to 8192

Anthropic requires `max_tokens`. OpenAI does not. A client written against OpenAI that omits it and
is pointed at `/v1/messages` would otherwise get a 400 that says nothing useful about why.

So the shim **supplies 8192** when a client omits it. Chosen to be large enough for a real answer and
small enough that a runaway generation is bounded — and, critically, **never unbounded**. A missing
`max_tokens` that becomes an unlimited request is how a client discovers the problem from its bill
rather than from an error.

## Blocks versus messages

The two shapes differ structurally, not just in field names:

| | OpenAI | Anthropic |
|---|---|---|
| Message content | A string, or an array of parts | **Always an array of typed blocks** |
| System prompt | A message with `role: "system"` | A separate top-level `system` field |
| Tool call | `tool_calls` array on the assistant message | A `tool_use` block in the content array |
| Tool result | A message with `role: "tool"` | A `tool_result` block in a **user** message |
| Images | `image_url` content part | `image` block with a `source` object |

The `tool_result` row is the one that surprises people. In Anthropic's model a tool result is not a
message from a distinct role; it is content in a user turn. A translation that emits a
`role: "tool"` message and then converts it back produces a conversation the API rejects, usually with
a confusing error about message alternation.

## Streaming

Anthropic's streaming events differ from OpenAI's, though both are `text/event-stream`:

| Anthropic | Meaning |
|---|---|
| `message_start` | The message envelope, including usage |
| `content_block_start` | A new block (text or `tool_use`) |
| `content_block_delta` | Text or a tool input fragment |
| `content_block_stop` | The block is complete |
| `message_delta` | `stop_reason` and final usage |
| `message_stop` | End |

The relay translates these to `chat.completion.chunk` frames when the client asked in OpenAI's
dialect, and passes them through when the client is Anthropic-shaped.

**`content_block_delta` for a tool call carries partial JSON**, not an object. It is assembled across
deltas, which is why the relay refuses to splice a different model's output once a `tool_calls` delta
has been emitted ([Combos](../06-combos.md#7-midstream_policy)): a half-assembled tool input from one
model plus another's is a function call that may be valid and wrong.

## Count tokens

`POST /v1/messages/count_tokens` asks a provider how many tokens a request would cost. Only an
Anthropic-shaped upstream can answer that question, so the relay splits the route in two.

**Against an Anthropic provider** the request is forwarded to the upstream's own
`/v1/messages/count_tokens`, untranslated, and the upstream's answer is returned as it arrived.

**Against any other provider** the upstream is **not called at all**. Asking a provider that has no
counting endpoint would mean asking for a *generation* instead — a billed answer, in the wrong
shape, to a question that was not a question. So the relay answers locally:

```json
{ "input_tokens": 42 }
```

and says so in the response header:

```
x-manarouter-count-tokens-estimate: true
```

The number is a **character-based approximation, not the provider's tokenizer**: the UTF-8 byte
length of every string value in the request body, the top-level `model` excluded, divided by four
and rounded up, never less than one. A body that is not JSON falls back to its raw length the same
way. An estimate presented as an authoritative count would be worse than no endpoint at all, which
is why the header is there: a client that needs the provider's exact number can tell the two apart,
and can point itself at an Anthropic upstream when the difference matters.

The local answer costs nothing — no account is selected, no credential is read, and no upstream is
billed.

## See also

- [The catalogue](catalog.md) — which providers serve Messages natively.
- [OpenAI-compatible](openai-compatible.md) — the other large family, and the passthrough rule.
- [API reference](../18-api-reference.md) — the relay's Anthropic surface.
- [Combos](../06-combos.md) — midstream policy and the tool-call splice guard.
