# 27 — GPU jobs: custom work on rental GPUs

[25-gpu-rentals.md](25-gpu-rentals.md) rents a GPU to serve a *model*; [26-gpu-lanes.md](26-gpu-lanes.md) drives Kaggle for batched prompts. This page is the third shape: an **agent wants the card itself** for a custom job — render an animation, fine-tune a LoRA, run a Blender pass, execute any container with a command — and then hand back files. The lane lives in the relay at `crates/manarouter-relay/src/jobs.rs`.

## The rule

**The lane never claims a platform did something it did not do.** Each job names a backend; the backend is the only thing that talks to a rental platform, and the job's `status` is exactly what the backend reported. No backend is "implicit": submitting with a missing or unknown `backend` is a `400` naming the supported values, not a guess.

## API

| Method | Route | Purpose |
|---|---|---|
| `POST` | `/v1/jobs` | submit a job |
| `GET` | `/v1/jobs` | list jobs (`limit`, `after`) |
| `GET` | `/v1/jobs/{id}` | poll status, logs, artifacts |
| `POST` | `/v1/jobs/{id}/cancel` | cancel a queued/running job |
| `GET` | `/v1/jobs/{id}/artifacts/{name}` | download one artifact |

Auth is exactly the batch lane's: the same client key that calls a model. Bodies are the usual error envelope.

### Submit body

```json
{
  "backend": "demo",
  "image": "python:3.12-slim",
  "command": ["python", "render.py"],
  "env": {"STEPS": "50"},
  "gpu": "RTX4090",
  "gpu_count": 1,
  "disk_gb": 16,
  "max_seconds": 3600
}
```

`backend` and `image` are required. `command` empty means the image's default command. `env` is a string map passed through to the container. `gpu_count` 1–8; `max_seconds` 1–21600 (six hours) — the lane fails the job when the budget is spent, because a card bills whether the job is making progress or not.

### Status

`queued` → `running` → `succeeded` | `failed` | `cancelled`. A job's `error` is set only on `failed`; `artifacts` is populated only on `succeeded` (except a cancelled job keeps whatever it had). Logs append in `logs` and persist with the record.

## Backends

A backend implements `JobBackend`: `launch`, `poll`, `cancel`. Two ship.

### `demo` — the local simulator

No network, no credential, no money. It exists so an agent (or a test) can prove the whole API before renting anything, and it is what the live verification below runs against.

- `DEMO_MS` in `env` sets how long the simulated GPU work takes; default 500 ms.
- A `command` entry containing `fail` makes it fail with a plain reason.
- On success it writes one artifact, `output.txt`, containing the job id.

It is a simulator, never a fallback for `vast`. A typo in the backend name is a refusal, not a silent free run.

### `vast` — Vast.ai

Reads the operator's `VAST_API_KEY` from the relay's environment, **never** from `config.json`: a rental key on disk is a rental key in every backup.

Uses the published REST contract under `https://console.vast.ai/api/v0`:

| Call | Request |
|---|---|
| launch | `POST /instances/create/` with `image`, `num_gpus`, `gpu_name`, `disk_size` and the generated `onstart` script |
| poll | `GET /instances/?api_key=…`, then this instance's `actual_status` |
| cancel | `DELETE /instances/{id}/?api_key=…` |

The id `create` returns is remembered in `backend_detail.instance_id`; every later call is addressed by it.

**What the startup script does — and nothing more.** `onstart` echoes a start marker carrying the job id, runs the job's command, then echoes a finish marker. It does not install anything, clone anything, or fetch anything: an image that needs its dependencies should carry them. The markers exist so the operator can see in the Vast console which of their jobs a running instance is.

**Status mapping, stated rather than guessed:** `running` / `waiting` / `pending` → `running`; `stopped` → `succeeded`; `error` → `failed` with Vast's own wording; any other value stays `running` and the raw value is logged. Because "succeeded" is Vast's word for "the container stopped", a non-zero exit is **not** distinguishable from a zero one through this route — the honest reading of `stopped` is "the instance stopped", and the artifacts are what a caller should trust. That is a real limitation of the platform's list endpoint, not of the mapping, and it is why the lane is not presented as a job-result oracle for `vast`.

### What `vast` does not do yet

- **Artifacts are not collected.** The `demo` backend writes `output.txt`; the `vast` backend reports none. Pulling files off a Vast instance needs either an upload the job performs itself or a `vastai` CLI fetch, and neither is built here. A Vast job that produces a rendering must put it somewhere the operator can reach — its own command uploading to storage is the intended shape — and until then `artifacts` is empty and the view says so rather than pointing at a file that is not there.
- **Not verified against a live rental.** There is no `VAST_API_KEY` in this environment, so the tests cover the parsers and the whole lifecycle with the simulator; the network half is written to the published contract and has never been run. Treat the first real Vast job as the verification.
- **Logs are the lane's own**, not the container's stdout. A Vast instance's output lives in Vast's console, not in this process's memory.

## Security

A job body names a container image and a command, and the `vast` backend hands that command to a rented machine. That is remote code execution **by design** — it is the entire point of the lane — so the guard is the one the rest of the admin API uses: the client key. Both routes call `authorize_batch_request`, so an unauthenticated caller is refused before the body is read, and a keyless relay on a non-loopback bind refuses to start (`docs/23-implementation-status.md`).

Stated plainly, because it is easy to assume more:

- The relay does **not** sandbox the command. It does not inspect it for `rm -rf`, network access, or anything else. Isolation is the container's on the rental platform, and the operator's choice of platform.
- A `vast` job runs with whatever the operator's `VAST_API_KEY` permits. Scope the key at Vast if the account does more than rentals.
- An artifact name must be a plain file name: empty names, `/`, `\`, `.` and `..` are refused before any path is joined, so a job cannot name its way out of its directory. The artifact's *content* is whatever the job wrote, and it is served without inspection.

## What is verified, and what is not

**Verified by running** — unit tests in `jobs.rs`, plus the live probe on the release build:

- `demo`: submit → `queued` → `running` → `succeeded`, `output.txt` produced and downloadable, traversal names refused.
- `demo` failure: a command containing `fail` ends `failed` with a reason, not a hang.
- cancel: a running job goes `cancelled`, and the driver stops instead of overwriting the cancellation.
- validation: unknown backend, blank image, `gpu_count` outside 1–8, `max_seconds` outside its range.
- the Vast instance-list parser, against a recorded body.

**Not verified:** any real `vast` rental — create, poll to `stopped`, terminate, or a live artifact fetch. The parsers and the lifecycle are tested; the HTTP contract has not been exercised.

## See also

- [25-gpu-rentals.md](25-gpu-rentals.md) — renting a GPU to serve a model.
- [26-gpu-lanes.md](26-gpu-lanes.md) — the Kaggle batch lane, and the lane vocabulary.
- [31-endpoints-in-use.md](31-endpoints-in-use.md) — the same routes with runnable calls.

