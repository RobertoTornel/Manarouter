# Modal fine-tuning provider (reference)

A single-file, OpenAI-compatible fine-tuning provider for Modal. It accepts JSONL files,
runs one supervised LoRA SFT job at a time on one **NVIDIA RTX-PRO-6000 (96 GB)**, reports
events and per-epoch checkpoints, supports cooperative pause/resume/cancel, and serves
non-streaming chat from the resulting adapter.

This is a reference implementation, not a production trainer. It is deliberately
single-tenant and single-job: no distributed training, no adapter merging, and no
authentication inside the app (Modal Proxy Auth is the front door).

## Cost

Modal bills resources while a container is alive, not per request. This app requests:

| Resource | Price | This app | Per hour |
|---|---|---|---|
| RTX-PRO-6000 | $0.000842/s | 1 GPU | $3.0312 |
| CPU | $0.0000131/core/s | 8 physical cores | $0.3773 |
| Memory | $0.00000222/GiB/s | 32 GiB | $0.2557 |
| **Total while alive** | | | **~$3.66/hour** |

- The GPU price is from Modal's public pricing page; CPU and memory are the standard
  on-demand rates. Volume storage is $0.09/GiB/month after the first 1 TiB.
- `scaledown_window=1200` means the container can stay alive for **20 minutes after the
  last HTTP input**, still billing. That tail is up to ~$1.22. When the container is
  scaled to zero, this deployment costs nothing.
- **A background training thread does not count as an input.** While a job is running,
  poll at least once every 15 minutes. If no request arrives for 20 minutes, Modal can
  stop the container mid-job; the training process dies, and the job is left `running`
  until a new container reconciles the active marker (after 30 minutes) or a later
  create request treats it as stale.
- A second concurrent request can cold-start another GPU container just to answer 409.
  Avoid overlapping clients and polls from several machines.

## What it provides

All paths are relative to the deployed Modal URL.

| Method | Path | Notes |
|---|---|---|
| POST | `/v1/files` | multipart `file` + optional `purpose`; returns a `file-...` object |
| GET | `/v1/files` | `limit`, `after` |
| GET | `/v1/files/{id}` | metadata |
| GET | `/v1/files/{id}/content` | `application/jsonl` |
| POST | `/v1/fine_tuning/jobs` | flat or `method.supervised.hyperparameters`; returns immediately |
| GET | `/v1/fine_tuning/jobs` | `limit` (max 100), `after` |
| GET | `/v1/fine_tuning/jobs/{id}` | current job |
| POST | `/v1/fine_tuning/jobs/{id}/cancel` | cooperative, or immediate before training |
| POST | `/v1/fine_tuning/jobs/{id}/pause` | only while `running` |
| POST | `/v1/fine_tuning/jobs/{id}/resume` | only while paused |
| GET | `/v1/fine_tuning/jobs/{id}/events` | messages + one metrics event per logged step |
| GET | `/v1/fine_tuning/jobs/{id}/checkpoints` | one per epoch |
| POST | `/v1/chat/completions` | non-streaming; base model or a succeeded adapter |
| GET | `/v1/models` | base model + succeeded adapters |
| DELETE | `/v1/models/{model}` | hides an adapter from the listing; does not delete files |
| GET | `/healthz` | no GPU work, but any HTTP request still starts the GPU container |

Statuses are `validating_files -> queued -> running -> succeeded | failed | cancelled`.
Creation returns `validating_files` immediately; validation happens in the background
thread, and a bad file produces a terminal `failed` job with a readable `error.message`
rather than an HTTP 500. `GET /docs` exposes the FastAPI schema.

### Training file

One JSON object per line. Every line needs a non-empty `messages` array; every message
needs string `role` and `content`, and at least one message must have `role: "assistant"`.
Tool calls and `prompt`/`completion` records are not supported. Blank lines are ignored.

```jsonl
{"messages":[{"role":"user","content":"What is 2+2?"},{"role":"assistant","content":"4"}]}
{"messages":[{"role":"system","content":"Answer briefly."},{"role":"user","content":"Name a color."},{"role":"assistant","content":"Blue."}]}
```

The whole file is parsed before training. A line that is not valid JSON, is not an object,
has no `messages` array, or has no assistant message fails the job with a line number in
the message. `validation_file` is accepted and stored but **not used**; there is no
validation split and `valid_loss` is always `null`.

### Hyperparameters

Defaults: `n_epochs=1`, `batch_size=1`, `learning_rate_multiplier=1.0`, which means
`learning_rate = 2e-4 * multiplier`. All three are accepted in any of these places, with
the later one winning:

1. top-level `hyperparameters`
2. top-level `n_epochs` / `batch_size` / `learning_rate_multiplier`
3. `method.supervised.hyperparameters`

`method.type` must be `supervised` when `method` is present. `n_epochs` and `batch_size`
must be whole numbers >= 1; the multiplier must be > 0.

Other training constants: LoRA `r=16`, `alpha=32`, `dropout=0.05`, target modules
`q_proj`, `k_proj`, `v_proj`, `o_proj` when present (otherwise the first linear layers
found), max sequence length 1024, default batch size 1, one optimizer step per batch,
`learning_rate` constant with no warmup. bf16 is used when the GPU supports it, otherwise
fp16. The seed comes from `seed` (default 42). `trained_tokens` is the real number of
non-padding input tokens seen by the trainer.

## Deploy

Prerequisites: a Modal account with billing enabled, Python 3.10+, and the Modal CLI.

```bash
cd deploy/modal-finetune
python3 -m venv .venv
. .venv/bin/activate
pip install modal
modal setup                 # browser login; stores a token outside this repo
modal deploy app.py
```

`modal deploy` prints the web-server URL. Copy it; do not guess the URL from the app
name. The first build installs `torch==2.8.0+cu128` and the training stack, so it takes
several minutes. Later deploys reuse the image layers.

The app creates the Volume `manarouter-finetune-state` on first deploy and mounts it at
`/state`. Job data survives redeploys. Inspect it with:

```bash
modal volume ls manarouter-finetune-state
modal volume get manarouter-finetune-state jobs/ACTIVE.json /tmp/active.json
```

### Proxy auth

The web server is deployed with `requires_proxy_auth=True`. Create a **Proxy Token** in
the Modal workspace settings, keep its ID and secret, and send them on every request as
`Modal-Key` and `Modal-Secret`. The app never sees a password and contains no token.
Requests without valid headers are rejected by Modal before reaching the app.

Do not put the token on a command line (shell history and `ps` can see it). Use a 0600
curl config file:

```bash
umask 077
cat > ~/.modal-finetune-auth <<EOF
header = "Modal-Key: $MODAL_KEY"
header = "Modal-Secret: $MODAL_SECRET"
EOF
```

Then pass `-K ~/.modal-finetune-auth` to every curl below. The variables must already
hold your Proxy Token halves; this file is a secret, so remove it when finished.

### Optional environment

| Variable | Default | Meaning |
|---|---|---|
| `BASE_MODEL` | `Qwen/Qwen2.5-1.5B-Instruct` | Hugging Face base model for every job |
| `MAX_SEQ_LENGTH` | `1024` | Maximum rendered sequence length |
| `STATE_DIR` | `/state` | State directory; also the Volume mount point |
| `HF_TOKEN` | unset | Token for gated Hugging Face models |

Shell variables at `modal deploy` time are **not** forwarded into containers. Change the
default in `app.py`, or attach a Modal secret. For a gated base model:

```bash
modal secret create huggingface HF_TOKEN=-
```

Modal opens `$EDITOR`; paste the token after `HF_TOKEN=`, save, and exit. The value never
appears in shell history or `ps`. Then add
`secrets=[modal.Secret.from_name("huggingface")]` to the `@app.cls(...)` decorator. Never
commit that secret.

The class is pinned to the exact GPU string `"RTX-PRO-6000"`. It is a 96 GB Blackwell
card; the base model, its LoRA training state, and the chat KV cache must fit in that
memory. A 1.5B model is comfortable; a 70B model is not. Chat and training are never
resident at the same time: chat is refused with 409 while a job is active, and the model
cache is evicted when a new job starts.

## Manarouter provider

The app speaks enough of the OpenAI surface for Manarouter's existing `modal-vllm`
dialect. Add the provider through the panel or CLI, or put this object in the `providers`
array of `~/.config/manarouter/config.json`:

```json
{
  "id": "modal-finetune",
  "label": "Modal fine-tuning (RTX-PRO-6000)",
  "dialect": "modal-vllm",
  "base_url": "https://<workspace>--<deployment>.modal.run",
  "auth": { "type": "modal" },
  "capabilities": ["fine-tuning", "chat", "models"],
  "price_per_second": 0.000842
}
```

- `base_url` is the deployment root, not a `/v1/...` path; the relay appends the route.
- Add the account secret as the two Proxy Token halves joined by exactly one period:
  `TOKEN_ID.TOKEN_SECRET`. Use `mr account add modal-finetune --secret-stdin` or the
  panel, never a command-line argument. The relay splits the secret and sends
  `Modal-Key`/`Modal-Secret`. Do not put either half in `base_url` or static headers.
- `max_context`, `kv_pool_tokens`, `max_concurrent`, and `price_per_second` are optional
  `modal-vllm` settings. For cost accounting, `price_per_second = 0.000842` covers the
  GPU only, not CPU, memory, or idle time.

**The `fine-tuning` capability does reach this app through the relay.** The job family —
`POST /v1/fine_tuning/jobs`, the job by id, its `events` and `checkpoints`, and `pause`,
`resume`, `cancel` — is selected by that capability (or by `?provider=modal-finetune`), and
the relay stages the training file: a file uploaded to the relay's own store with
`purpose=fine-tune` is uploaded here as multipart with the same purpose and its original
filename, and the id this app returns is substituted into `training_file` before the job is
forwarded. A file id the relay does not hold is forwarded untouched, and an upload that
fails is forwarded too — this app then refuses the job on a file it cannot resolve, which
is the truthful error. Chat goes through the existing `modal-vllm` path, and model
discovery can use `GET /v1/models` when the relay syncs this provider.

Through the relay the two curl steps below collapse into one: upload the JSONL to the
relay's file store, then create the job naming that id. Provider selection is by capability,
so `model` in the job body is the base model to train, not a route selector.

```bash
FILE_ID=$(curl -sS "$RELAY/v1/files" -H "Authorization: Bearer $CLIENT_KEY" \
  -F purpose=fine-tune -F file=@train.jsonl | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
curl -sS "$RELAY/v1/fine_tuning/jobs" -H "Authorization: Bearer $CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  -d "{\"training_file\":\"$FILE_ID\",\"model\":\"Qwen/Qwen2.5-1.5B-Instruct\",\"suffix\":\"support\"}"
```

`RELAY` is `http://127.0.0.1:20900` by default and `CLIENT_KEY` is a relay client key, not
the Modal Proxy Token — the relay holds that one.

## Curl walkthrough

Set `MODAL_URL` to the URL printed by `modal deploy`. The auth file is passed with `-K`.

### 1. Upload

```bash
curl -sS -K ~/.modal-finetune-auth \
  -F purpose=fine-tune \
  -F file=@train.jsonl \
  "$MODAL_URL/v1/files"
```

The response contains an `id` such as `file-1a2b3c4d...`. Put it in a variable:

```bash
FILE_ID=$(curl -sS -K ~/.modal-finetune-auth \
  -F purpose=fine-tune -F file=@train.jsonl \
  "$MODAL_URL/v1/files" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
```

### 2. Create a job

```bash
JOB_ID=$(curl -sS -K ~/.modal-finetune-auth \
  -H 'Content-Type: application/json' \
  -d "{\"training_file\":\"$FILE_ID\",\"model\":\"Qwen/Qwen2.5-1.5B-Instruct\",\"suffix\":\"support\",\"hyperparameters\":{\"n_epochs\":1,\"batch_size\":1,\"learning_rate_multiplier\":1.0}}" \
  "$MODAL_URL/v1/fine_tuning/jobs" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
```

The response is immediate and has `"status":"validating_files"`. For the nested OpenAI
shape, add:

```json
"method": {"type": "supervised", "supervised": {"hyperparameters": {"n_epochs": 2}}}
```

Nested values win over flat ones. The same call with `-d @job.json` is easier to read
than the escaped shell string.

### 3. Poll

```bash
curl -sS -K ~/.modal-finetune-auth \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID"
```

Repeat every 5-10 seconds; this is also the heartbeat that keeps the container alive.
The job moves to `running`, then `succeeded`, `failed`, or `cancelled`. A failure has an
`error` object with `code`, `message`, and `param`. Polling a finished job is free of GPU
work but still keeps the container warm for the 20-minute window.

### 4. Events and checkpoints

```bash
curl -sS -K ~/.modal-finetune-auth \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID/events?limit=100"
```

The list has a start message, one `metrics` event per logged optimizer step, and a
terminal message. Metrics carry `data: {"step": N, "train_loss": X, "learning_rate": Y}`.
`train_loss` is logged every step; it is the loss reported by the trainer for that step,
not a smoothed average.

```bash
curl -sS -K ~/.modal-finetune-auth \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID/checkpoints"
```

A checkpoint is written at the end of each epoch, so the default one-epoch job has one
checkpoint.
Each has `object`, `id` (`ftckpt-...`), `created_at`, `fine_tuned_model_checkpoint`,
`step_number`, `metrics`, and `fine_tuning_job_id`. The metrics object has `step`,
`train_loss`, `valid_loss: null`, `full_valid_loss: null`, and the four token-accuracy
fields all `null`.

### 5. Control

```bash
curl -sS -K ~/.modal-finetune-auth -X POST \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID/pause"
curl -sS -K ~/.modal-finetune-auth -X POST \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID/resume"
curl -sS -K ~/.modal-finetune-auth -X POST \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID/cancel"
```

Pause only works while `running`, and the job **stays `running` while paused**: there is
no `paused` status in this reference. The trainer writes no steps while paused and
resumes at the next step boundary. Resume only works if the pause flag is set. Cancel
works while `validating_files`/`queued` immediately, and during training at the next
step boundary. Pausing/resuming/cancelling a terminal job, pausing a non-running job,
resuming a non-paused job, and controlling a job whose cancel is pending all return 400
with an OpenAI-shaped error naming the current status.

Control is cooperative. It cannot interrupt a model download, tokenization, or a single
long optimizer step. If the container is lost, there is no trainer to observe the
control file; the job remains `running` until the 30-minute stale-marker check fails it
on a later request. A paused job still holds the GPU and the active marker.

### 6. Chat

After success, read `fine_tuned_model` from the job (do not construct it by hand):

```bash
MODEL=$(curl -sS -K ~/.modal-finetune-auth \
  "$MODAL_URL/v1/fine_tuning/jobs/$JOB_ID" | python3 -c 'import json,sys; print(json.load(sys.stdin)["fine_tuned_model"])')

curl -sS -K ~/.modal-finetune-auth \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Hello\"}],\"max_tokens\":128}" \
  "$MODAL_URL/v1/chat/completions"
```

The name looks like `ft:qwen2.5-1.5b-instruct:support:1a2b3c4d`. Chat loads the base
model and the LoRA adapter with `transformers` + `peft`; the first call after a job can
take minutes and its GPU time is billed. The adapter is not merged. `stream=true` is
rejected with 400, unknown models return 404 `model_not_found`, and `finish_reason` is
`"length"` when `completion_tokens >= max_tokens`, otherwise `"stop"`.

`DELETE /v1/models/{model}` removes an adapter from `/v1/models` and from the lookup
path (subsequent chat returns 404), but it does not delete the adapter directory or the
job. There is no job-delete endpoint.

## Training and control design

The job thread validates the file, writes status transitions, then loads the tokenizer
and base model, renders each conversation with the model's chat template, wraps the model
with PEFT LoRA, and runs `trl`'s `SFTTrainer` for the requested epochs. The adapter is
written to `jobs/<id>/adapter.tmp/` and renamed to `jobs/<id>/adapter/` only after
`trainer.train()` returns, so a partial adapter is never published. The job succeeds only
after that rename and a Volume commit.

State on the Volume:

```text
/state/
  files/<file_id>.jsonl
  files/<file_id>.json
  jobs/ACTIVE.json
  jobs/<job_id>/job.json
  jobs/<job_id>/events.jsonl
  jobs/<job_id>/checkpoints.jsonl
  jobs/<job_id>/control.json
  jobs/<job_id>/adapter/
  jobs/<job_id>/trainer/
```

`control.json` is `{"pause": bool, "cancel": bool}`. It is written by the web container,
committed to the Volume, and re-read by the trainer at step boundaries. This is what lets
pause and cancel cross container boundaries; it is also why control is cooperative.

### Versions and exact APIs

The image pins `torch==2.8.0+cu128` (Blackwell sm_120), `transformers==5.22.0`,
`peft==0.21.2`, `trl==1.17.1`, plus `datasets>=4.7.0`, `accelerate>=1.11.0`, FastAPI,
Uvicorn, and `python-multipart`. The code is written against these actual signatures:

- `trl.SFTConfig(max_length=1024, dataset_text_field="text", packing=False, ...)` and
  `trl.SFTTrainer(model, args=..., train_dataset=Dataset, processing_class=tokenizer,
  callbacks=[...])`. `include_num_input_tokens_seen="non_padding"` supplies the real
  `trained_tokens`.
- `transformers.AutoModelForCausalLM.from_pretrained(..., dtype=torch.bfloat16)` — the
  5.x canonical keyword; `torch_dtype` is deprecated.
- `peft.LoraConfig(r=16, lora_alpha=32, lora_dropout=0.05, task_type="CAUSAL_LM",
  target_modules=[...])`, `peft.get_peft_model`, and `peft.PeftModel.from_pretrained`
  for chat.
- `datasets.Dataset.from_dict({"text": [...]})`; no custom data collator.

Training runs in a daemon thread so the create request can return immediately. The
thread catches its own exceptions and turns them into a failed job; the HTTP process
stays up.

## Limits

- **One job at a time.** A second create gets 409 while a job is active. There is no
  queue. The 409 is enforced with an `O_EXCL` marker on the Volume, but overlapping
  requests can still start extra GPU containers before the marker is read.
- **No distributed training and no multi-GPU.** One 96 GB card. The base model plus
  LoRA state must fit; there is no quantization, offload, or DeepSpeed/FSDP.
- **No adapter merge.** Chat loads PEFT at runtime. There is no export to a merged
  Hugging Face model and no upload of the adapter anywhere.
- **No `paused` status.** Pause is a flag while the job remains `running`; see above.
- **No validation metrics.** `validation_file` is ignored; validation losses and token
  accuracies are always `null`.
- **Events are per logged step; checkpoints per epoch.** A long epoch can run for a while
  with no new checkpoint. Default `n_epochs=1` means one checkpoint at the end.
- **No relay fine-tuning routes yet.** The provider capability is metadata only. Upload,
  jobs, events, and checkpoints must be called directly at the Modal URL until the relay
  forwarding lands.
- **No user auth in the app.** Security is Modal Proxy Auth plus the fact that the
  endpoint is single-tenant. Do not expose it without the proxy token, and do not share
  the token with clients you do not trust with GPU spend.
- **Chat is minimal.** Non-streaming, text messages only, no tools/vision/JSON mode,
  one choice, and 409 while a fine-tuning job is active. Chat calls
  `apply_chat_template` directly, so the base model needs a chat template for
  `/v1/chat/completions`; training falls back to plain `role: content` lines if one is
  absent.
- **Idle scale-down can kill training.** Poll at least every 15 minutes. The 20-minute
  window is a Modal setting, not a guarantee that the job survives.
- **Cost figures exclude Volume storage and egress.** Actual Modal billing is the source
  of truth; the table is an estimate at current list prices.

## Tests

`test_app.py` is plain `unittest` (pytest is not required) and runs on a CPU-only box with
no Modal, FastAPI, torch, GPU, or network. It stubs the Modal decorators, FastAPI app,
and Uvicorn, then imports `app.py` and exercises the store, validation, hyperparameters,
pagination, control state machine, checkpoints, name derivation, and path-traversal
guards. It does **not** test the real `trl`/`transformers` API or a Modal deploy.

```bash
python3 -m py_compile app.py
python3 test_app.py
```

Current result: 61 tests, all passing (`Ran 61 tests ... OK`).
