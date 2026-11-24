# Agent GPU jobs — running your own work on a rented card via `mr jobs`

A model route sends one request and gets one reply. Custom GPU work — rendering an animation,
fine-tuning, a data pipeline, a model you downloaded yourself — is a different shape: it runs for
minutes to hours, and the agent wants to write the command itself rather than have the router
guess it. Manarouter's **job lane** rents a card for that. This page is the agent's field guide;
`docs/27-gpu-jobs.md` is the lane's implementation record.

> **Scope note.** Submitting jobs is authorized by any valid client key the same way chat is.
> The `--allow-models`/`--allow-routes` limits on a key scope *model* routes (`/v1/chat/completions`
> and friends) but **not** `/v1/jobs` — so a key narrowed to one model can still submit a rental
> job. A client key is spending authority for its backend; hand it out accordingly. A jobs gate
> is a known limitation recorded in [27-gpu-jobs.md](27-gpu-jobs.md).

## The mental model

```
submit (container image + command) → queued → running → succeeded | failed | cancelled → artifacts
```

Three nouns:

- **job** — one image, one command, one card (or several), one lifetime (bounded by `max_seconds`).
- **backend** — where the card comes from. `demo` is a local simulator that costs nothing and
  exercises the whole lifecycle; `vast` rents from Vast.ai. A real rental bills whether the
  command is rendering or stuck in a loop, so `max_seconds` is a budget as much as a timeout.
- **artifact** — a file the job produced (a rendered MP4, a checkpoint, logs). Fetched by name.

## The five commands

Every command talks to the relay's `/v1/jobs` with **your client key** — the same key that calls
a model (`mr key create --print`; the plaintext is shown once). The key comes from `MR_CLIENT_KEY`,
or piped on stdin when the variable is unset. Never put it in a flag.

```sh
# 1. Submit. The image must be public or the backend's credentials must pull it.
mr jobs submit --backend demo --image python:3.12-slim --command "python render.py" --env STEPS=50 --max-seconds 3600
# → { "id": "01a…", "status": "queued", … }

# 2. Look at it (status, live logs, what's produced so far).
mr jobs show 01a…

# 3. Wait for it. Polls every 2s; the exit code is the job's terminal state: 0 = succeeded.
mr jobs wait 01a… --timeout 10m

# 4. Pull an artifact down. `mr jobs` has no download verb — artifacts are files fetched by name:
curl -H "Authorization: Bearer $MR_CLIENT_KEY" http://127.0.0.1:20900/v1/jobs/01a…/artifacts/output.mp4 -o output.mp4

# 5. Stop it (a card bills until the job ends; cancel is how you stop paying).
mr jobs cancel 01a…
```

`mr jobs list` shows the newest jobs; `--json` (a global flag) prints the lane's own document
verbatim, which is what an agent script should parse. The human view is the same document,
indented.

## Writing a good job

- **One image, one command.** If the work needs two images (say a base and a builder), the job
  runs the image you name and nothing else — make a small image that already contains the tools.
- **`--command`**: repeat the flag per argument (`--command python --command render.py`), or pass
  one quoted string, which splits on whitespace. Prefer repeats for paths with spaces.
- **`--env`**: repeatable `NAME=VALUE`. A secret belongs here, not in the image's layers — the
  value rides the submission and dies with the job. The lane refuses to log it.
- **`--max-seconds`**: set it to what the work should take, plus headroom. An infinite loop on a
  rental is never a free mistake.
- **Don't submit-and-forget.** `wait` with a timeout you can afford; a submit you're not going
  to follow is a card paying for a finished render.
- **Renders specifically**: write the output to a named file in the container's working directory,
  then fetch it by that name. The lane does not guess which file you mean, and `logs` are a
  debugging tail, not the product.

## Checking it costs nothing first

The `demo` backend (`docs/27-gpu-jobs.md`'s verified section) is the way to develop the command:
submit a `demo` job, confirm the lifecycle, then swap `backend: vast` plus your image, GPU model
(`--gpu`), card count (`--gpu-count`) and disk (`--disk-gb`). `mr jobs submit … --backend demo`
with the *same flags* is the pre-flight check for a rental submission.

## What the lane will not do

- It does not run your code locally and does not docker on your host — a *custom job* is, by
  definition, work the laptop will not do. (The relay's own `/v1/jobs` lane runs on the backend;
  this CLI only drives it.)
- It does not save your image credentials — a backend that cannot pull the image fails the job
  fast with a reason rather than silently switching images.
- A `vast` rental that cannot be placed fails `queued`→`failed` with the backend's reason; the
  lane does not substitute a worse GPU.

## Errors, in the agent's terms

| Exit / status | Meaning | Do this |
|---|---|---|
| `401` from the relay | the client key is wrong or the relay hasn't loaded the new key yet | confirm `MR_CLIENT_KEY`; a freshly minted key takes ~2s to hot-load into the relay |
| `failed` (job) | the command exited non-zero, or no card could be placed | `mr jobs show <id>` and read `logs` / `error` before resubmitting |
| `wait` exits 1 with `still running after …` | your `--timeout` elapsed, the job did not | the job still runs; `wait` again with a longer budget, or `cancel` if it is wasting a card |
| `wait` exits 1 with ``job … ended `failed` `` | the job reached `failed` | same as the row above; the exit code mirrors the terminal state so `&&` chains behave |

## See also

- [27-gpu-jobs.md](27-gpu-jobs.md) — the lane: endpoints, lifecycle states, verified scope, and
  the one thing never verified (a real `vast` rental).
- [26-gpu-lanes.md](26-gpu-lanes.md) — the other GPU lanes (Kaggle batch), for comparison.
- [12-cli-reference.md](user/12-cli-reference.md) — every `mr` command with its flags.
