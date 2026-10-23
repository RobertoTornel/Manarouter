# Command Code

Command Code is a coding-model provider whose API is unlike any of the standard shapes. The adapter
exists because the shape cannot be expressed by translation alone, and because several of its
behaviours are actively hostile to a naive client.

Ground truth for this document: `.research/commandcode-bridge.md` and the reference implementation
in the sibling `fornai/commandcode-bridge` checkout, outside this repository.

## The endpoint

```http
POST {base}/alpha/generate
```

with these headers, all of which are required:

| Header | Value | Why |
|---|---|---|
| `authorization` | `Bearer <key>` | Standard. |
| `content-type` | `application/json` | Standard. |
| `accept` | `text/event-stream` | The endpoint is always SSE. |
| `x-command-code-version` | `1.82.1` | CLI compatibility fingerprint used by the working bridge. |
| `x-cli-environment` | `production` | Same. |

The version and environment headers are **fingerprints**, not authentication. They identify the
caller as the official CLI, and they are the kind of header that is easy to omit in an adapter and
then spend an afternoon debugging.

The version is **pinned by the relay**, not by the provider's configuration: the constant
`COMMAND_CODE_VERSION` in `crates/mr-dialects/src/commandcode.rs` holds `1.82.1`, and the relay
overwrites whatever a provider's `headers` map says. The catalogue entry records the same value so
the data and the code agree; change the constant, and the catalogue entry with it.

## Always SSE

The response is `text/event-stream` **regardless of what the client asked for**. There is no
non-streaming mode.

So the adapter forces `stream: true` in `params` and synthesises a non-streamed response for a client
that asked for one: it consumes the whole event stream server-side and returns a single JSON body.
The client sees the shape it asked for; the upstream sees what it requires. The cost is that a
non-streaming client gets no output until the model has finished, which is inherent — there is nothing
to stream from.

## The request envelope

```json
{
  "config": { "...": "10 fields" },
  "memory": "",
  "taste": "",
  "skills": "",
  "params": { "model": "...", "messages": [...], "stream": true },
  "threadId": "01924f3a-..."
}
```

Five fields are not what an OpenAI-shaped request would suggest:

- **`config`** carries the CLI's own settings. The 9 fields are the first-party client's; the adapter
  populates the ones that are required and leaves the rest at the defaults the CLI uses.
- **`memory`, `taste`, `skills`** are personalisation slots. They are sent empty, because inventing
  content for them would put words in the operator's mouth.
- **`params`** is where the actual completion parameters live, including the model and the messages.
  It is a nested object rather than the request body being the parameters.
- **`threadId`** is a **fresh UUID per request**, not a conversation identifier. Sending a stable one
  would make the provider treat successive requests as one thread, which is not what a stateless
  router should do.

## Tools

Command Code's tool representation uses `input_schema` with **no `{type: "function"}` wrapper**:

```json
{
  "name": "get_weather",
  "description": "Get the weather",
  "input_schema": { "type": "object", "properties": { "city": { "type": "string" } } }
}
```

not:

```json
{ "type": "function", "function": { "name": "...", "parameters": {...} } }
```

An adapter that sends the OpenAI wrapper produces a request the provider accepts and then behaves
strangely with — which is worse than an outright rejection.

## Events

| Event | Meaning |
|---|---|
| `text-delta` | A fragment of assistant text. |
| `tool-call` | A complete tool call. |
| `tool-use` / `tool-input-start` | The beginning of a tool call's input. |
| `tool-delta` / `tool-input-delta` | A fragment of a tool call's input. |
| `finish` | The response is complete. |
| `error` | Something went wrong. |

There are two names for each tool-input event. The adapter accepts both, because which one arrives
depends on the server's version and there is no negotiation.

## The gotchas

Each of these will produce a bug that looks like something else.

### There is no upstream timeout

The provider will hold a connection open indefinitely. A request that stalls stalls forever unless the
relay imposes its own deadline. The per-provider fields are `ttfb_timeout_ms` and `total_timeout_ms`,
and **both default to zero, which disables them**; the catalogue entry sets neither. Set
`total_timeout_ms` explicitly on this provider (for example `300000` for five minutes) or one dead
request occupies a connection until the process restarts. The provider loader flags a config where both are zero.

### `stream: true` is forced

Already covered, but worth restating as a gotcha rather than a design note: a client asking for a
non-streamed response does not change what the upstream sends.

### Streaming reports no usage

Token counts are absent from the event stream. An adapter that reports zero tokens is reporting a
measurement it never received; the honest thing is to record that usage was not available. See
[Quota tracking §what is recorded per request](../08-quota-tracking.md#what-is-recorded-per-request).

### `content` is `null` when tool calls are present

A response containing tool calls carries `"content": null`, not `""`. A client that does `content.length`
crashes. The adapter **preserves** the null (an OpenAI-shaped response with tool calls also carries
`content: null`), so clients must read it defensively rather than assume a string.

### An unknown-id `tool-delta` falls back to the last tool call

**This is a real correctness hazard, and the most dangerous item on this list.**

When a `tool-delta` arrives with an id the adapter has not seen (or with no id at all), the adapter
does what the reference bridge does: it attaches the fragment to the **most recently opened** tool
call rather than failing. With parallel tool calls, a fragment intended for the first call can
therefore be merged into the second, producing a tool call with arguments from two different calls.
The fragment is ignored only when no tool call is open at all.

That is a malformed result at best and a valid-but-wrong function invocation at worst. The adapter
cannot detect the mis-attribution, because it has no way to recover the intended id: treat parallel
tool calls on this provider as suspect when the arguments look stitched together.

### `role: "assistant"` is emitted exactly once

Not once per delta. An adapter that emits a role on every text fragment produces a stream that most
clients tolerate and some reject, so the bug survives testing.

### `threadId` is fresh per request

Covered above, repeated here because the consequence is subtle: reusing a thread id across requests
makes the provider carry context the client never sent.

### Upstream errors are not OpenAI-shaped

```json
{ "success": false, "error": { "code": "...", "status": 429, "message": "...", "docs": "..." } }
```

A client expecting OpenAI's `{"error": {"message", "type", "code"}}` will not find the message where
it looks. **The adapter translates this envelope**, including the `docs` link, which is often the only
thing that explains the failure. Anthropic-shaped clients receive only `type` and `message`; `code`,
`status` and `docs` are preserved on the OpenAI-compatible surface only.

## The GO plan

The bridge exists because Command Code's GO plan is a subscription, not a metered API. The adapter's
job is to make a subscription-only provider usable through an OpenAI-compatible surface, which
requires exactly the translation described here: a different envelope, a different tool shape, a
forced stream, and no usage reporting.

## See also

- [The catalogue](catalog.md) — the `command-code` entry and its dialect.
- [OpenAI-compatible](openai-compatible.md) — the surface clients actually see.
- [API reference](../18-api-reference.md) — error envelopes.
- [Quota tracking](../08-quota-tracking.md) — recording usage a provider does not report.
