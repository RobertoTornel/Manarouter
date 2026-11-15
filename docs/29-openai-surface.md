# The OpenAI-compatible surface

Manarouter answers the OpenAI HTTP API as it stands today: Chat Completions and Responses for text,
Embeddings and Moderations, the audio family, Images, Videos, Realtime, Files, Uploads, Batches,
Vector Stores, Containers, Fine-tuning and Evals. It answers nothing older. `/v1/completions`,
`/v1/edits`, `/v1/engines/*`, the `fine-tunes` family and the Assistants tree are absent by choice:
each has a successor in the current spec, and carrying a retired shape forward would mean testing
every provider against an API that no new client should be written against.

The contract an agent implements against — exact paths, capability gating, error strings, limits and
change recipes — is
[`docs/30-openai-compatible-api-spec.md`](30-openai-compatible-api-spec.md). This document argues why
the surface is shaped as it is; that one states what must remain true.

One process serves one port, so which shape an endpoint answers in cannot be left to whichever
upstream happens to be configured. Every route is classified before it is written, and the class is
the same in the code, in the route table, and here:

| Class | Who answers | Routes |
|---|---|---|
| **Proxied** | A provider, after auth and translation | chat, responses, embeddings, audio speech/transcription/translation, images generations/edits/variations, moderations, videos, realtime (the websocket upgrade and the two client-secret and call POSTs), fine-tuning, containers |
| **Served here** | The relay, from its own state | `/v1/models` and `/v1/models/{model}`, files, uploads, batches, stored responses, vector stores and search, evals |
| **Refused** | Nobody — a deliberate error | containers, when no provider declares them |

A route's class is fixed. Which class a **request** lands in is not: a request is refused whenever no
step the router would use declares the capability that route requires. That is the gate below, and it
is the reason a provider that implements one family can be configured without breaking the rest.

## The capability gate

A provider declares which client surfaces it serves (`Capability`, `crates/mr-core/src/dialect.rs`),
and the declaration is what the panel's checkboxes write and what `catalog/providers.toml` carries per
template. Twenty-two capabilities exist today; most name one route, the video family names three and
realtime names two, because a provider either accepts video jobs or it does not — polling and
downloading a job it accepted are not separately optional:

| Capability | Client routes it covers |
|---|---|
| `chat` | `POST /v1/chat/completions` |
| `responses` | `POST /v1/responses` |
| `embeddings` | `POST /v1/embeddings` |
| `audio-transcription` | `POST /v1/audio/transcriptions` |
| `audio-translation` | `POST /v1/audio/translations` |
| `audio-speech` | `POST /v1/audio/speech` |
| `images-generations` | `POST /v1/images/generations` |
| `images-edits` | `POST /v1/images/edits` |
| `images-variations` | `POST /v1/images/variations` |
| `moderations` | `POST /v1/moderations` |
| `video` | `POST /v1/videos`, `GET /v1/videos/{id}`, `GET /v1/videos/{id}/content` |
| `realtime` | `POST /v1/realtime/client_secrets`, `POST /v1/realtime/calls`, and the `/v1/realtime` websocket |
| `models` | `GET /v1/models` |
| `messages` | `POST /v1/messages` (the Anthropic surface) and `POST /v1/messages/count_tokens` |
| `search` | `POST /v1/search` |
| `vector-stores` | the `/v1/vector_stores` management family, served here |
| `containers` | the `/v1/containers` family |
| `fine-tuning` | the `/v1/fine_tuning/jobs` family |
| `evals` | the `/v1/evals` family, served here |
| `tool-calling`, `reasoning`, `vision` | no route — model features, recorded so the panel can say a model supports them |

The last three are the exception that proves the rule: they are model features, not routes, so they
are never consulted when a request arrives. Everything above them is a route, and
`RouteKind::capability` is the single mapping from route to capability — `Capability::for_route`
delegates to it and `Capability::routes` is the readable inverse, which is how the two directions are
kept from disagreeing (`dialect.rs`, and the test `routes_and_capabilities_agree_in_both_directions`
walks both ways).

The gate runs after the snapshot has resolved a model to steps and before any step is dialled. Each
step whose provider does not declare the route's capability is dropped:

- **A step whose provider is missing from the snapshot is kept**, so the mid-flight skip can report a
  stale snapshot instead of disguising it as a missing capability. A stale snapshot is a different
  problem with a different fix, and the error should say which one it is.
- **If steps existed but all were dropped**, the request is refused with `404` and the code
  `unsupported_endpoint`, naming the route and the capability it needed. Nothing is dialled: the
  upstream would answer with a 404 that blames the model for a feature the operator never configured,
  and falling back to chat would answer in a shape the client cannot read.
- **If the model resolved to no steps at all**, that is `model_not_found` — a different mistake, with
  a different message.

The refusal looks like this, in the dialect the client asked in (`error_response` in
`crates/manarouter-relay/src/server.rs`):

```json
{"error": {"message": "the route audio.speech requires the audio.speech capability, and no step's provider declares it", "type": "unsupported_endpoint", "code": "unsupported_endpoint"}}
```

An Anthropic client gets `{"type": "error", "error": {"type": "unsupported_endpoint", "message": …}}`
with the same status, because the envelope is the only part of that response the client's own error
handling will read.

This is what makes a one-family provider a first-class citizen. `fish-audio` in
`catalog/providers.toml` declares `audio-speech` and nothing else: a chat request routed to it is
refused with the message above instead of being forwarded as an OpenAI chat body the upstream would
reject after the connection was already made. The same holds for `deepgram` (transcription only),
`stability-ai` (image generation only) and `voyage`/`jina` (embeddings only).

## Configuration hygiene

A capability that the provider's dialect cannot express is a configuration error, not a runtime
surprise, so it is refused when the provider is saved (`Provider::dialect_capability_mismatches`,
`crates/mr-core/src/model.rs`, and the table test that walks every dialect against every capability).
`open-ai` accepts all of them because the OpenAI dialect is the superset; `anthropic` rejects the
audio, image, video and realtime families it has no shape for; `google` and the OAuth dialects reject
the families their envelopes cannot carry. The panel does not hide the combinations a dialect cannot
express — all twenty-two boxes are tickable — so the refusal is the feedback: it is returned on save,
in the provider drawer, and names the capability and the dialect that cannot carry it.

Declaring *fewer* capabilities than a provider can serve is always allowed — it narrows what the
router will send there. Declaring one it cannot serve is what the validator exists to stop.

## The families

### Audio

Three routes and three separate capabilities: `POST /v1/audio/speech` (JSON in, audio bytes out),
`POST /v1/audio/transcriptions` and `POST /v1/audio/translations` (multipart in, JSON out). They are
separate because vendors really do ship one and not the others — `fish-audio` and `elevenlabs`
declare `audio-speech` alone, `deepgram` declares `audio-transcription` alone — and a provider that
transcribes a recording has not thereby promised to synthesise one.

Speech is the family's binary response: the provider's status, content type and bytes pass through,
streamed rather than re-wrapped, so a client receives audio where it expected audio and an upstream
error where the upstream failed. The two multipart routes are forwarded **byte for byte** — boundary,
audio payload and filename intact — with one exception: the `model` form part, whose value is
replaced with the upstream's model id so a request addressed to `provider/whisper-large-v3` reaches
the provider as `whisper-large-v3`. That replacement is a byte-range splice, not a re-encode, which
is why an unknown form field a provider added last week survives the trip.

### Images

`POST /v1/images/generations` is JSON; `POST /v1/images/edits` and `POST /v1/images/variations` are
multipart; the three carry separate capabilities. Generation and editing diverge in practice — many
providers generate and refuse to edit — and variations are a third thing again, so the declaration is
three checkboxes rather than one. The multipart pair uses the same byte-range `model` splice the audio
routes use.

In the spec `model` is optional on all three of these image routes and on moderations (it defaults to
`dall-e-2`, or to `omni-moderation-latest` for moderations). A request that omits it is not an error
here either: with no model to resolve, the route becomes capability-addressed and the first provider
declaring the capability answers, which is the only reading available when the client named no model.
Every other route still refuses a body with no readable `model`, because on those the model is
required and inventing one would misreport what ran.

### Video

`POST /v1/videos` names its model in the body and takes the ordinary model-bound path. The two reads,
`GET /v1/videos/{id}` and `GET /v1/videos/{id}/content`, name a render by id and no model at all, so
they are capability-addressed: the first provider declaring `video` is asked, or the one the client
names with `?provider=`. One capability covers all three because a provider that accepts a job has
already promised to let it be polled and downloaded — splitting the reads out would let a
configuration accept work it could not finish.

The content read is binary. Status, content type and bytes are the provider's, streamed through
rather than buffered, which matters when the render is a tens-of-megabytes MP4.

### Moderations

`POST /v1/moderations` takes JSON and answers in OpenAI's moderation shape. Nothing is translated in
either direction, because no other dialect in this build has a moderation endpoint at all: a provider
that declares `moderations` is asserting it speaks this exact shape. The route is proxied, never
computed — this process has no classifier of its own and does not pretend to.

### Realtime

Two shapes share one capability. `POST /v1/realtime/client_secrets` and `POST /v1/realtime/calls` are
ordinary HTTP requests whose model lives inside the `session` object; the body scanner reads the
top-level fields and deliberately does not descend into nested objects, so these are
capability-addressed instead of model-bound. A secret bundle or an SDP answer is forwarded unchanged.

`GET /v1/realtime?model=provider/model` is the socket. The relay accepts the client's WebSocket,
resolves the model exactly as a chat request would — same routing table, same account selection, same
capability gate — then opens its own WebSocket to the provider and pumps frames both ways. Frames are
forwarded verbatim: the relay does not parse audio, does not re-frame, and does not time out an idle
session, because a person thinking before they speak is not a fault. A close from either side
propagates with its code and reason, and an upstream failure after the upgrade closes the client
socket with `1011`.

Every refusal happens **before** the upgrade, so a client never receives a socket that then dies. A
request with no `model` query parameter is a `400`: nothing else in a socket request names a
provider, and guessing would be worse than asking. A model no provider declares `realtime` for is a
`404 unsupported_endpoint` naming the capability, and a provider that serves the model over a
non-OpenAI dialect is a `501` with the dialect in the message, because retrying cannot change either
answer.

Two limits are worth knowing. The relay forwards the provider's headers on the upstream handshake but
not the client's, so a provider that wanted a client-supplied header will not see it. And a provider's
`proxy` setting is not honoured for a WebSocket connection.

### Fine-tuning

The job family is proxied whole: `POST`/`GET /v1/fine_tuning/jobs`, the job by id, its `events` and
`checkpoints`, and `pause`, `resume` and `cancel`. None of those paths names a model, so the provider
is chosen by the `fine-tuning` capability — or by `?provider=` — and the job stays the provider's own:
its id, its state machine, its checkpoints, its bill. The relay adds no queue and reports no progress
it did not receive.

Creation is the one request here that costs money before it can succeed, so it is the one request
whose account is chosen by balance: the relay starts a job only on an account whose known credit is
more than $15, taking the richest among those that qualify. Known means declared with
`mr account set-credit … --usd`, or a USD balance cached from the provider within the last 24 hours;
anything else is unknown and disqualifies the account rather than being guessed at. A `402` — or a
`400` whose body says the credit ran out — moves the creation to the next funded account silently,
and cools the one that refused. The rest of the family is not gated: `GET`, `events`, `checkpoints`,
`pause`, `resume` and `cancel` name a job that already exists, so they are forwarded as ordinary
capability-addressed requests. The relay does not record which account created a job, so no later
request is pinned to it: under a `RoundRobin` policy the next read can reach a sibling account of the
same provider, and that provider then answers `404`, which the relay forwards verbatim. A created job
cannot be moved, because no other account can continue it, and it is never resubmitted elsewhere.

`training_file` and `validation_file` are the one place the relay must intervene. A client uploads its
JSONL to the relay's file store, so the id it holds — `file_…` — is the relay's; the provider needs an
id of its own or the job fails on a file it cannot see. The relay stages those bytes into the
provider's `POST /v1/files` as multipart with `purpose=fine-tune`, under the filename the operator
uploaded, using the resolved step's base URL and its own credential; it substitutes the id the
provider returns and forwards the request otherwise untouched — every other field reaches the
provider as the client wrote it.

Two cases are deliberately left alone. An id the relay does not hold is treated as a file the
provider already knows, so a client that uploaded straight to the provider keeps working; rewriting
it would break a request that is fine. An account whose credential is not a header (`OAuth`, or a
key carried in the query string) is not staged either, because an upload needs the same
authentication the job does and inventing a second authentication path here would not be the
credential the operator configured. In both cases the job request is forwarded as it arrived.

A failed upload cannot turn into a relay error: the staging step's answer is the body to send and
nothing else, so it logs the reason and forwards the request still naming the local id — the
provider then refuses the job on a file it cannot resolve, and that error names the id that failed.

`deploy/modal-finetune/` is a reference server for this surface: it serves `POST /v1/files`, the job
routes above, and a LoRA trainer on one RTX PRO 6000, so a fine-tune can be pointed at a GPU the
operator rents instead of at a vendor. It exists because this family is the one place where proxying
is not enough to be useful — the route only means something if something on the other end actually
trains.

### Files, uploads and batches

Files live in the relay's own namespace, not the provider's: `POST /v1/files` takes a multipart body
with a `purpose`, `GET /v1/files` lists, `GET`/`DELETE /v1/files/{id}` fetches or removes one entry
and `GET /v1/files/{id}/content` returns the bytes. The lane that owns them is
`crates/manarouter-relay/src/batch.rs`, and a file's id is minted by the relay, so a batch can be
inspected after the provider that would have run it is long gone.

`/v1/uploads` is the resumable path to the same namespace: create an upload, `POST` parts to it in as
many requests as the client likes, then `POST …/complete` and the relay assembles the parts and hands
the bytes to the same store `POST /v1/files` writes to. Parts are named by their sequence, not by the
client's filename, because a filename is attacker-controlled text and the part files are paths;
completing is the only transition that creates a `file_…` id, and `cancel` removes the parts and
freezes the upload. An upload that is never completed expires after 24 hours, and the next request
that names it deletes what is left and says so, rather than resurrecting a half-written file.

**The cap is 32 MiB, not OpenAI's 512.** `MAX_FILE_BYTES` and `MAX_BODY_BYTES` are both 32 MiB
because a file is held in memory while it is validated and written; raising the advertised cap
without streaming to disk would only move the failure to a less informative place. The refusal names
the cap it enforced, so a client learns the real number instead of an upstream 413.

A batch is `POST /v1/batches` with a JSONL file of items and `completion_window` — only `"24h"` is
accepted, and it is recorded rather than enforced, because the lane finishes when the items finish.
Validation is synchronous; execution is not. Each item is dispatched through the ordinary pipeline —
routing table, dialect translation, account selection, cooldowns, fallbacks — so a batch item is
subject to the same rules as a live request, including the capability gate. An item that fails is
retried up to three times before it is written to the error file; the batch's `request_counts` says
how many succeeded, and `output_file_id`/`error_file_id` point at the two result files in the same
namespace. A single item's body is capped at 16 MiB and a batch at 50 000 items, and the batch's
`metadata` may carry a cost ceiling which no run can exceed — the lane stops rather than spending
past it.

### Vector stores and search

The vector-store family is served here, not proxied: `POST`/`GET /v1/vector_stores`, a store by id
with update and delete, `/files` and `/files/{file_id}`, `/file_batches`, and `/search`. A store is a
directory under `<state_dir>/vector_stores/<id>/` holding `store.json` and `chunks.jsonl`, so it
survives a restart and deleting the store deletes its chunks. Because nothing leaves the process
except the embedding call, no capability gates the management routes — a capability declares what a
*provider* can do, and there is no provider here.

Attaching a file reads the bytes from the batch lane's file store, splits them into chunks and embeds
them through the relay's own `/v1/embeddings` route. Four properties of that are worth stating,
because they are visible in the responses:

- **Chunking is character-based.** The chunker targets 800 characters with 100 of overlap, split at a
  word boundary, so a code point is never cut. `chunking_strategy` on create is validated but does not
  change the chunker, and the strategy reported back on an attached file is the character strategy
  that actually ran.
- **Scores are cosine similarity in [-1, 1].** OpenAI's vector-store scores use a different scale and
  are not rescaled here, because a rescale would be a guess presented as a number.
- **`filters` are refused when non-empty.** No per-chunk attributes are stored, so a filtered search
  would quietly return the wrong set instead of saying it cannot filter.
- **An embedding model is needed to attach or to search with text.** It comes from the store's own
  `embedding_model` field — a local extension OpenAI does not have — then
  `MANAROUTER_EMBEDDING_MODEL`, then the first provider in the routing snapshot declaring
  `embeddings`. With none of those the store still exists and still lists, while attaching and text
  search answer `409`. A numeric query vector needs no embedder at all.

Attaching is bounded at 200 000 chunks and 100 MiB of chunk text. The bound is a denial-of-service
bound rather than a product limit, and exceeding it is a `413` that names which bound was crossed. The
embedding call happens before anything is recorded, so a failed embed leaves no half-attached file.

`POST /v1/search` has two natures in one path. A body carrying `model` is a provider search: its bytes
go to the ordinary dispatcher, so the provider — and the capability gate — answers, and a provider
that does not declare `search` produces the same `404 unsupported_endpoint` any other unconfigured
family would. A body carrying `vector_store_id`, or `vector_store_ids` to merge several stores by
score, is local retrieval. When both are present `model` wins, because the client asked the provider
and a local answer would be answering a different question. A merged local search requires every named
store to resolve to the same embedding model: cosine scores from different embedding spaces are not
comparable, and adding them would invent a ranking. Every result on both search routes carries
`vector_store_id`, so a merged page says which store each hit came from.

### Stored responses

`POST /v1/responses` is proxied like any other text route, and the relay keeps a copy when the client
asks it to: `"store": true` and a 2xx response are both required, and a body over 8 MiB is skipped
with a warning rather than written. The record lives at `<state_dir>/responses/<id>.json` and holds
the upstream response verbatim beside the request the client sent, because a stored response that
cannot be replayed against the request that produced it is only half a record.

The four reads are `GET /v1/responses/{id}`, `DELETE /v1/responses/{id}`, `POST
/v1/responses/{id}/cancel` and `GET /v1/responses/{id}/input_items`. Everything about them is local:
nothing is re-dialled. Cancel on a response that already finished returns it untouched rather than
inventing a cancellation the provider never performed — the relay cannot cancel work it did not
start, and pretending otherwise would make the client's state disagree with the upstream's.
`input_items` renders the request's `input` in the items shape, replacing any client-supplied `type`
or `id` with ones this relay minted, and gives an element that is not an object an item with no
invented role. An id that is malformed and an id that is unknown get the same 404, so the endpoint
cannot be used to probe which ids exist; ids are restricted to ASCII letters, digits, `.`, `_` and
`-` because they become file paths.

### Evals

An eval is a saved question plus a set of items to ask it of. `POST /v1/evals` records the question,
`POST /v1/evals/{id}/runs` executes it, `GET /v1/evals/{id}` and `GET /v1/evals/{id}/runs/{run_id}`
read the two halves back. Execution is synchronous: the run's response is the finished run, because a
run that returned an id before doing any work would need a poll loop, a resumed state machine and a
cancel path for something that takes as long as the items it grades.

The items come either inline as `file_content` or from a JSONL file in the batch lane's namespace by
`file_id`, which is the same file `POST /v1/files` accepted, so an eval can grade a batch's input
without copying it. Every item's model is resolved before the first upstream call — `item.model`,
else the eval's `metadata.model`, and a run that names neither is refused with both places in the
message. The body for every item is built and validated in the same preflight, so a malformed item is
a `400` before any request is paid for, not a half-finished run.

Two criteria are implemented. `string_check` substitutes `{{sample.output_text}}` and
`{{item.<field>}}` and compares with `eq`, `ne`, `like` (substring) or `ilike` (case-insensitive
substring); an unknown placeholder is a `400`. `label_model` asks a grader model the client names,
through the same dispatch path as the items, and matches its answer against the labels
case-insensitively, longest label first, with `passing_labels` defaulting to `["pass"]`. A criterion
type the relay does not implement is refused at run time with a message naming the type and both it
does implement, rather than being skipped as if it had passed.

A failed item and an errored item are different results and stay different: a `500` from upstream, a
body that is not JSON or a response with no assistant message is *errored*, while an answer that
graded false is *failed*. Both leave the run `completed`, so one bad item does not hide the other
ninety-nine, and `per_model_usage` carries a row for every model that was called, graders included.

An eval and its runs are stored at `<state_dir>/evals/<eval_id>.json` and
`<state_dir>/evals/<eval_id>/runs/<run_id>.json`, written by temp file and rename, so a run that was
interrupted leaves the previous one intact.

### Containers

Refused by default, and that is the honest answer. A container endpoint runs code in an image with a
lifecycle and a filesystem; this process has no sandbox, no cgroup and no image store, and a route
that returned a fake container would be a worse lie than a refusal. `/v1/containers` and everything
under it therefore answers `501 unsupported_endpoint` when no provider declares `containers`, with a
message that says the relay serves this surface only by proxying to one that does.

When a provider does declare it, the family is forwarded like the other capability-addressed routes:
the provider runs the container, the relay carries the requests, and nothing about the container's
lifecycle is invented on this side.

## The whole surface

| Family | Routes | Class | Capability consulted |
|---|---|---|---|
| Chat Completions | `POST /v1/chat/completions` | Proxied | `chat` |
| Responses | `POST /v1/responses` | Proxied | `responses` |
| | `GET`/`DELETE /v1/responses/{id}`, `POST …/{id}/cancel`, `GET …/{id}/input_items` | Served here | — |
| Embeddings | `POST /v1/embeddings` | Proxied | `embeddings` |
| Audio | `POST /v1/audio/speech` | Proxied | `audio-speech` |
| | `POST /v1/audio/transcriptions` | Proxied | `audio-transcription` |
| | `POST /v1/audio/translations` | Proxied | `audio-translation` |
| Images | `POST /v1/images/generations` | Proxied | `images-generations` |
| | `POST /v1/images/edits` | Proxied | `images-edits` |
| | `POST /v1/images/variations` | Proxied | `images-variations` |
| Moderations | `POST /v1/moderations` | Proxied | `moderations` |
| Models | `GET /v1/models`, `GET`/`DELETE /v1/models/{model}` | Served here | `models`, declared but never gated |
| Video | `POST /v1/videos`, `GET /v1/videos/{id}`, `GET …/{id}/content` | Proxied | `video` |
| Realtime | `GET /v1/realtime` (WebSocket), `POST /v1/realtime/client_secrets`, `POST /v1/realtime/calls` | Proxied | `realtime` |
| Files | `POST`/`GET /v1/files`, `GET`/`DELETE /v1/files/{id}`, `GET …/{id}/content` | Served here | — |
| Uploads | `POST /v1/uploads`, `POST …/{id}/parts`, `…/{id}/complete`, `…/{id}/cancel` | Served here | — |
| Batches | `POST`/`GET /v1/batches`, `GET /v1/batches/{id}`, `POST …/{id}/cancel` | Served here | — |
| Vector stores | `/v1/vector_stores`, its files, file batches and search | Served here | — |
| Search | `POST /v1/search` | Proxied when the body names a model, served here otherwise | `search` when proxied |
| Evals | `POST /v1/evals`, `GET /v1/evals/{id}`, `POST …/{id}/runs`, `GET …/runs/{run_id}` | Served here | — |
| Fine-tuning | `POST`/`GET /v1/fine_tuning/jobs` and the whole job family | Proxied | `fine-tuning` |
| Containers | `/v1/containers` and its sub-resources | Refused unless a provider declares them, then proxied | `containers` |

The route-by-route listing, with request and response shapes, is in
[the API reference](18-api-reference.md). What this page adds is the classification: every path above
is answered by exactly one of the three mechanisms, and the capability column is the difference
between a request that reaches a provider and one that is refused before a socket is opened.

## See also

- [The API reference](18-api-reference.md) — every route, with examples.
- [Provider dialects](24-provider-dialects.md) — what each upstream dialect can and cannot express.
- [GPU rentals](25-gpu-rentals.md) — the batch lane on rented hardware, and the fine-tuning
  deployment this surface can drive.
- [OpenAI-compatible providers](providers/openai-compatible.md) — declaring capabilities for a
  provider that serves only one family.
- [The web panel](15-web-panel.md) — the capability editor and the presets that populate it.
