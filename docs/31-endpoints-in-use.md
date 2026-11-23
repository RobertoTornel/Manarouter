# Endpoints in use

The HTTP contract this project uses, written so that any serving stack can implement it. It says
what a client sends and what it gets back; it says nothing about routing, upstream selection, or
which process answers. Those are the implementing stack's business.

Paths are written from the version prefix. An OpenAI-compatible client's base URL is
`<origin>/v1`, so the first row below is called as `<origin>/v1/chat/completions`. The Anthropic
paths in this document already carry their `/v1`.

## Authentication

Every path accepts either header:

| Header | Value |
|---|---|
| `Authorization` | `Bearer <key>` |
| `x-api-key` | `<key>` |

`x-api-key` is checked first when both are present. The key is opaque to the client: the stack
hashes it and compares the hash, so a key is shown once at creation and never again. A missing,
malformed, or unknown key is `401` with the OpenAI error envelope and `WWW-Authenticate: Bearer`,
except on `/v1/messages*`, which uses the Anthropic envelope. The `WWW-Authenticate` header carries
`error="invalid_token"`, so a client can tell a rejected key from an absent one without parsing
prose.

Keep the key out of shell history and `ps`. A 0600 curl config file is the least awkward way:

```bash
umask 077
printf 'header = "Authorization: Bearer %s"\n' "$MR_KEY" > /tmp/mr.curl
curl -K /tmp/mr.curl https://example.invalid/v1/models
```

`GET /healthz` is the one path that needs no key: a liveness probe cannot be expected to hold one.

This surface sends no CORS headers and does not answer `OPTIONS`, so a browser page cannot call it
directly. A browser client needs its own server in front, or the deployment has to add CORS on
purpose.

## Endpoint table

`Body` names the request encoding; `JSON` means `application/json`.

| Method | Path | Purpose | Body |
|---|---|---|---|
| POST | `/v1/chat/completions` | Chat, streamed or not | JSON |
| POST | `/v1/responses` | Responses API, streamed or not | JSON |
| GET | `/v1/responses/{id}` | Retrieve a stored response | none |
| DELETE | `/v1/responses/{id}` | Delete a stored response | none |
| POST | `/v1/responses/{id}/cancel` | Cancel a running response | JSON, may be empty |
| GET | `/v1/responses/{id}/input_items` | List a stored response's input items | none |
| POST | `/v1/messages` | Anthropic Messages | JSON |
| POST | `/v1/messages/count_tokens` | Count input tokens | JSON |
| GET | `/v1/models` | List model names the deployment serves | none |
| GET | `/v1/models/{model}` | Retrieve one entry | none |
| POST | `/v1/embeddings` | Embeddings | JSON |
| POST | `/v1/moderations` | Moderation classification | JSON |
| POST | `/v1/audio/transcriptions` | Speech to text | multipart |
| POST | `/v1/audio/translations` | Speech to English text | multipart |
| POST | `/v1/audio/speech` | Text to speech | JSON → audio bytes |
| POST | `/v1/images/generations` | Generate images | JSON |
| POST | `/v1/images/edits` | Edit an image | multipart |
| POST | `/v1/images/variations` | Variations of an image | multipart |
| POST | `/v1/videos` | Start a video job | JSON |
| GET | `/v1/videos` | List video jobs | none |
| GET | `/v1/videos/{id}` | Poll a video job | none |
| DELETE | `/v1/videos/{id}` | Delete a video job | none |
| GET | `/v1/videos/{id}/content` | Download the rendered video | none |
| GET | `/v1/realtime` | Realtime websocket upgrade | websocket |
| POST | `/v1/realtime/client_secrets` | Mint an ephemeral realtime key | JSON |
| POST | `/v1/realtime/calls` | WebRTC SDP handshake | `application/sdp` |
| POST | `/v1/files` | Upload a file | multipart or raw bytes |
| GET | `/v1/files` | List files | none |
| GET | `/v1/files/{id}` | Retrieve file metadata | none |
| GET | `/v1/files/{id}/content` | Download file bytes | none |
| DELETE | `/v1/files/{id}` | Delete a file | none |
| POST | `/v1/uploads` | Create a resumable upload | JSON |
| POST | `/v1/uploads/{id}/parts` | Add one part | multipart |
| POST | `/v1/uploads/{id}/complete` | Finish the upload | JSON |
| POST | `/v1/uploads/{id}/cancel` | Abandon the upload | JSON, may be empty |
| POST | `/v1/batches` | Submit a batch | JSON |
| GET | `/v1/batches` | List batches | none |
| GET | `/v1/batches/{id}` | Retrieve a batch | none |
| POST | `/v1/batches/{id}/cancel` | Cancel a batch | JSON, may be empty |
| POST | `/v1/jobs` | Submit a GPU job | JSON |
| GET | `/v1/jobs` | List jobs | none |
| GET | `/v1/jobs/{id}` | Retrieve a job | none |
| POST | `/v1/jobs/{id}/cancel` | Cancel a job | JSON, may be empty |
| GET | `/v1/jobs/{id}/artifacts/{name}` | Download a job artifact | none |
| POST | `/v1/vector_stores` | Create a vector store | JSON |
| GET | `/v1/vector_stores` | List vector stores | none |
| GET | `/v1/vector_stores/{id}` | Retrieve a vector store | none |
| POST | `/v1/vector_stores/{id}` | Update a vector store | JSON |
| DELETE | `/v1/vector_stores/{id}` | Delete a vector store | none |
| POST | `/v1/vector_stores/{id}/files` | Attach a file | JSON |
| GET | `/v1/vector_stores/{id}/files` | List attached files | none |
| GET | `/v1/vector_stores/{id}/files/{file_id}` | Retrieve one attachment | none |
| DELETE | `/v1/vector_stores/{id}/files/{file_id}` | Detach a file | none |
| POST | `/v1/vector_stores/{id}/file_batches` | Attach several files | JSON |
| GET | `/v1/vector_stores/{id}/file_batches/{batch_id}` | Poll an attach batch | none |
| POST | `/v1/vector_stores/{id}/search` | Search one store | JSON |
| POST | `/v1/search` | Search one or more stores, or call a model | JSON |
| POST | `/v1/evals` | Create an eval | JSON |
| GET | `/v1/evals/{id}` | Retrieve an eval | none |
| POST | `/v1/evals/{id}/runs` | Start a run | JSON |
| GET | `/v1/evals/{id}/runs/{run_id}` | Poll a run | none |
| POST | `/v1/fine_tuning/jobs` | Start a fine-tuning job | JSON |
| GET | `/v1/fine_tuning/jobs` | List jobs | none |
| GET | `/v1/fine_tuning/jobs/{id}` | Retrieve a job | none |
| POST | `/v1/fine_tuning/jobs/{id}/cancel` | Cancel a job | JSON, may be empty |
| GET | `/v1/fine_tuning/jobs/{id}/events` | List job events | none |
| POST | `/v1/fine_tuning/jobs/{id}/pause` | Pause a job | JSON, may be empty |
| POST | `/v1/fine_tuning/jobs/{id}/resume` | Resume a job | JSON, may be empty |
| GET | `/v1/fine_tuning/jobs/{id}/checkpoints` | List checkpoints | none |
| POST | `/v1/containers` | Create a container | JSON |
| GET | `/v1/containers` | List containers | none |
| GET | `/v1/containers/{id}` | Retrieve a container | none |
| DELETE | `/v1/containers/{id}` | Delete a container | none |
| GET | `/healthz` | Liveness | none |

An unregistered path answers `404` with an empty body; a wrong method on a registered path answers
`405` with an empty body and an `Allow` header naming the methods that path does support. Every `GET`
route also answers `HEAD`. Neither the `404` nor the `405` uses the error envelope.

Model ids contain `/`, so `GET /v1/models/{model}` takes the id percent-encoded or not; the stack
decodes path segments before matching. `created` on a model row is always `0`: the field exists
because clients require it, not because the deployment knows when the model appeared.

List endpoints take `limit` and `after` query parameters. `limit` defaults to 20 and is capped at
100; `after` is the id to continue from. Pages are
`{"object":"list","data":[…],"first_id":…,"last_id":…,"has_more":…}`, with `null` ids on an empty
page. `GET /v1/models` is the exception: it returns the whole catalogue in one page and takes no
parameters.

## Chat

`model` and `messages` are required. Anything else is the ordinary Chat Completions field set
(`temperature`, `max_tokens`, `tools`, `response_format`, `reasoning_effort`, …) and is passed
through to the model.

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"provider/model","messages":[{"role":"user","content":"Hello"}]}'
```

With `"stream": true` the response is `text/event-stream`, one `data: {…}` frame per chunk, ended by
`data: [DONE]`.

## Responses

`model` and `input` are required; `input` is a string or an array of input items. Pass
`"store": true` to make the response retrievable afterwards at `GET /v1/responses/{id}`; without it
nothing is kept and that GET answers `404`.

Streaming uses SSE with named events: `event: response.created`, then deltas, then the terminal
`event: response.completed` (or `response.failed`, `response.incomplete`). Each event's `data:` is
one JSON object.

`GET /v1/responses/{id}` returns the object that was sent back when it was created — `id`,
`object: "response"`, `status`, `output`, `usage` — unchanged, and `DELETE` removes it.
`GET /v1/responses/{id}/input_items` returns the stored request's items as a list page. A response
is stored only when the whole body was JSON, so a streamed response is never retrievable.

## Messages

`model`, `max_tokens` and `messages` are required. Send `anthropic-version` the way Anthropic
clients do; the stack does not add it for you. Streaming is SSE with Anthropic's event names, ended
by `event: message_stop`.

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/messages \
  -H 'Content-Type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"provider/model","max_tokens":256,"messages":[{"role":"user","content":"Hello"}]}'
```

`POST /v1/messages/count_tokens` takes the same envelope and answers `{"input_tokens": 42}`.

## Embeddings and moderations

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/embeddings \
  -H 'Content-Type: application/json' \
  -d '{"model":"provider/model","input":["one","two"]}'
```

The answer is `{"object":"list","data":[{"object":"embedding","index":0,"embedding":[…]}],"model":…,"usage":{…}}`.

Moderations take `{"model":…,"input":…}` and answer
`{"id":…,"model":…,"results":[{"flagged":false,"categories":{…},"category_scores":{…}}]}`. `model`
may be omitted, in which case the deployment's default is used.

## Audio

Transcriptions and translations are `multipart/form-data` with a `file` part and a `model` field;
`language`, `prompt` and `response_format` are optional. The answer is `{"text": "…"}` for the default
JSON, plain text for `text`, and the corresponding subtitle or JSON shape otherwise.

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/audio/transcriptions \
  -F file=@clip.mp3 -F model=provider/model
```

Speech is JSON and answers audio bytes with the format's content type. `model` selects the engine
and `voice` is opaque to this contract: the deployment maps it to whatever its engine accepts, which
may be a preset name or a reference id.

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/audio/speech \
  -H 'Content-Type: application/json' \
  -d '{"model":"provider/model","input":"Hello","voice":"alloy"}' \
  --output hello.mp3
```

## Images

Generations take `{"model":…,"prompt":…,"n":…,"size":…}` and answer
`{"created":…,"data":[{"url":…}|{"b64_json":…}]}`. Edits take `image` (and optionally `mask`) as file
parts plus `prompt`; variations take `image` plus optional `n` and `size`.

## Videos

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/videos \
  -H 'Content-Type: application/json' \
  -d '{"model":"provider/model","prompt":"A kite over a cold sea"}'
```

The answer is a job object with `id`, `object`, `status`, `progress` and `created_at`. Poll
`GET /v1/videos/{id}` until `status` is `completed` (or `failed`), then download
`GET /v1/videos/{id}/content`. `DELETE /v1/videos/{id}` removes the job.

## Realtime

`GET /v1/realtime?model=provider/model` is a websocket upgrade. The query parameter is mandatory;
the socket carries the realtime protocol's JSON events and audio frames unchanged, in both
directions. A request without `model` is refused with `400`.

`POST /v1/realtime/client_secrets` mints an ephemeral key for a browser:
`{"session": {…}}` → `{"value":"ek_…","expires_at":…}`. `POST /v1/realtime/calls` takes a WebRTC
offer as `application/sdp` and answers the SDP answer.

## Fine-tuning

Creation names a training file id this deployment minted and a base model:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/fine_tuning/jobs \
  -H 'Content-Type: application/json' \
  -d '{"model":"provider/model","training_file":"file-abc123","suffix":"support"}'
```

The answer is a job object with `id`, `status`, `model`, `training_file`, `created_at` and
`estimated_finish` (often `null`). The rest of the family names an existing job: retrieve it, list
its `events`, cancel it, pause or resume it, or list its `checkpoints`. Only creation spends money
up front.

## Files

Two upload shapes, same result. Multipart, which is what every client library uses:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/files \
  -F purpose=batch -F file=@input.jsonl
```

Raw bytes, with the name and purpose in the query string:

```bash
curl -K /tmp/mr.curl "https://example.invalid/v1/files?filename=input.jsonl&purpose=batch" \
  -H 'Content-Type: application/octet-stream' --data-binary @input.jsonl
```

`purpose` is `batch` or `fine-tune`; anything else is refused. The answer is
`{"id":"file-…","object":"file","bytes":…,"created_at":…,"filename":…,"purpose":…}`. Files do not
expire, so a `file-…` id stays usable until it is deleted. `GET /v1/files/{id}/content` returns the
bytes with `application/octet-stream`.

## Uploads

Resumable uploads, for when a file is too large or too flaky to send in one request.

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/uploads \
  -H 'Content-Type: application/json' \
  -d '{"filename":"input.jsonl","purpose":"batch","bytes":1048576}'
```

`filename` and `bytes` are required and `purpose` is optional; the answer carries `id`, `status` and
`expires_at`. `bytes` is not checked against what you actually send. Send bytes as multipart parts
whose single file field is named `data`:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/uploads/upload-abc/parts \
  -F data=@part1.bin
```

Parts may arrive in any order. Finish with `POST /v1/uploads/{id}/complete` and either
`{"part_ids":["part-1","part-2"]}` or no body, which means every part received, in arrival order.
The answer is the finished file object. Parts are deleted 24 hours after the upload is created, and
completing an expired upload fails; starting a new upload is the only recovery. No path lists an
upload's parts, so a client that loses track of part ids cannot find out which parts landed.

## Batches

A batch is one JSONL file, one request per line, each with a `custom_id` and a `body` that is a
Chat Completions request:

```jsonl
{"custom_id":"1","method":"POST","url":"/v1/chat/completions","body":{"model":"provider/model","messages":[{"role":"user","content":"Hi"}]}}
```

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/batches \
  -H 'Content-Type: application/json' \
  -d '{"input_file_id":"file-abc123","endpoint":"/v1/chat/completions","completion_window":"24h"}'
```

Only `input_file_id` is required; `endpoint` defaults to `/v1/chat/completions` and must be that
value, because Chat Completions items are the only ones the lane runs; `completion_window` defaults
to `"24h"`, the only window accepted. The answer is a batch object
whose `status` moves from `validating` to `in_progress` and then `completed`, `failed`, `expired` or
`cancelled`. Results are written to `output_file_id`, failures to `error_file_id`; each line carries
the item's `custom_id` so a client can pair answers with requests. Items are answered
independently: one failing item does not fail the batch.

```json
{"id":"batch-abc","object":"batch","status":"in_progress","input_file_id":"file-abc123",
 "endpoint":"/v1/chat/completions","completion_window":"24h","created_at":1710000000,
 "request_counts":{"total":10,"completed":4,"failed":1},"output_file_id":null,"error_file_id":null}
```

`GET /v1/batches` lists with `limit` and `after`; `POST /v1/batches/{id}/cancel` stops new items and
keeps the ones already paid for.

## GPU jobs

A job runs a container with a command on a rented GPU. `image` and `backend` are required:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/jobs \
  -H 'Content-Type: application/json' \
  -d '{"backend":"demo","image":"python:3.12-slim","command":["python","render.py"],"env":{"STEPS":"50"},"gpu_count":1,"max_seconds":3600}'
```

`backend` is `demo` (the local simulator: costs nothing, good for wiring a client) or `vast`
(Vast.ai; needs `VAST_API_KEY` in the relay's environment). `gpu` names a model, `disk_gb` a disk
size, and `max_seconds` is the wall-clock budget after which the job fails rather than billing
forever. `env` is a string map.

The answer is a job object whose `status` moves `queued` → `running` → `succeeded` (with
`artifacts`), `failed` (with `error`), or `cancelled`. Poll with `GET /v1/jobs/{id}`; `logs` grows
as the backend reports. `POST /v1/jobs/{id}/cancel` stops a queued or running job.
`GET /v1/jobs` lists with `limit` and `after`. A named artifact downloads from
`GET /v1/jobs/{id}/artifacts/{name}` (plain file names only). See `docs/27-gpu-jobs.md`.

## Vector stores

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/vector_stores \
  -H 'Content-Type: application/json' \
  -d '{"name":"manuals"}'
```

`name` is optional, as are `metadata`, `file_ids`, `chunking_strategy`, `embedding_model` and
`expires_after`; the
answer is a store object with `id`, `object`, `status`, `file_counts`, `created_at`.

Attach a file, which is chunked and embedded by the stack:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/vector_stores/vs_abc/files \
  -H 'Content-Type: application/json' -d '{"file_id":"file-abc123"}'
```

Attaching the same file twice is refused. `POST /v1/vector_stores/{id}/file_batches` takes
`{"file_ids":[…]}` and answers a batch object to poll at
`GET /v1/vector_stores/{id}/file_batches/{batch_id}`. `POST /v1/vector_stores/{id}` updates `name`,
`metadata`, `embedding_model` or `expires_after`; `DELETE` removes the store and its chunks.

Search one store:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/vector_stores/vs_abc/search \
  -H 'Content-Type: application/json' \
  -d '{"query":"how do I reset it","max_num_results":5}'
```

The answer is `{"object":"vector_store.search_results.page","data":[{"file_id","filename","score","content":[{"type":"text","text":…}],"attributes":{…}}],"has_more":…}`.
`max_num_results` defaults to 10 and is capped at 50; `filters` narrows by metadata.

`POST /v1/search` is the same retrieval without naming a store in the path: send `query` plus
either `vector_store_id` or `vector_store_ids` (at most 100 stores, merged and ranked by score).
`max_num_results` and `filters` behave as they do on the per-store route. If the body carries
`model` instead, the request is a model call: the body is forwarded as-is and the answer is the
model's own.

## Evals

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/evals \
  -H 'Content-Type: application/json' \
  -d '{"name":"support-accuracy","data_source_config":{"type":"custom"},"testing_criteria":[{"type":"string_check","name":"mentions-refund","input":"{{sample.output_text}}","reference":"refund","operation":"like"}]}'
```

`name` is required; `data_source_config` is stored verbatim and `testing_criteria` must be an array
of objects. A run needs a data source naming a JSONL file:

```bash
curl -K /tmp/mr.curl https://example.invalid/v1/evals/eval_abc/runs \
  -H 'Content-Type: application/json' \
  -d '{"data_source":{"type":"jsonl","source":{"type":"file_id","id":"file-abc123"}}}'
```

`source` may instead be `{"type":"file_content","content":[{…},…]}` to inline the items. A run may
also carry `name` and `metadata`. Poll `GET /v1/evals/{id}/runs/{run_id}` for `status`, `result_counts`
and `per_testing_criteria_results`.

```json
{"id":"evalrun-abc","object":"eval.run","status":"completed","created_at":1710000000,
 "result_counts":{"total":2,"passed":1,"failed":1},
 "per_testing_criteria_results":[{"testing_criteria":"mentions-refund","passed":1,"failed":1}]}
```

Criteria templates interpolate `{{sample.output_text}}` and `{{item.<field>}}` only. `string_check`
needs `input`, `reference` and `operation` (`eq`, `ne`, `like`, `ilike`); `label_model` needs a
`model`, an `input`, and an optional `prompt`.

## Containers

`POST /v1/containers` takes `{"name":…,"file_ids":[…]}`, `GET /v1/containers/{id}` retrieves one and
`DELETE /v1/containers/{id}` removes it. This family is passed to a provider that serves containers;
a deployment with no such provider refuses it rather than pretending.

## Errors

OpenAI-shaped paths answer:

```json
{"error": {"message": "…", "type": "invalid_request_error", "code": "…", "param": null}}
```

`/v1/messages*` answers the Anthropic shape:

```json
{"type": "error", "error": {"type": "invalid_request_error", "message": "…"}}
```

| Status | Meaning |
|---|---|
| `400` | The request is malformed, or names a model or parameter the deployment cannot serve |
| `401` | Missing or unknown key |
| `404` | No such path, file, batch, store, eval or job |
| `413` | Body too large |
| `429` | Rate limited or out of quota; integer `Retry-After` seconds when known |
| `502` | The upstream provider failed; the provider's own error body is passed through |
| `503` | No upstream was reachable, or one is still starting |

Branch on the status first and on `code` second: `code` is stable and machine-readable, the message
is prose and may change. `429`, `502` and `503` are worth retrying; `400`, `401`, `404` and `413`
will not succeed on a second attempt with the same body.

A failure after the response has started cannot be an error status, because the client is already
reading an event stream. Streaming families end the stream without their terminal marker: no
`response.completed` for Responses, no `message_stop` for Messages, no `[DONE]` for Chat. A client
MUST treat a stream that ended without its marker as failed, not as complete. Nothing is retried
once a byte of the response body has been sent.

A body over 32 MiB is refused with `413` before it reaches a model. JSON bodies are read whole, so
`Content-Length` must be honest.
