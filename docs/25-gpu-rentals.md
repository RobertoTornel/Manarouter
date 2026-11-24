# 25 — GPU rentals

Manarouter normally forwards to somebody else's endpoint. This document covers the other arrangement:
the operator **rents a GPU**, the model runs **there**, and Manarouter decides what to send it, how
many sessions it can hold, what it costs, and when it is cheaper to let it go cold.

The interface design is frozen in
[research/ARCHITECTURE-gpu-rentals.md](research/ARCHITECTURE-gpu-rentals.md), which carries the
platform facts and their sources. This document is the operational one: what the operator does, what
the panel shows, and what Manarouter will refuse to pretend.

## API routes

The conductor exposes six GPU-rental endpoints, plus the lane routes in
[26-gpu-lanes.md](26-gpu-lanes.md):

| Method | Route | Purpose |
|---|---|---|
| `GET` | `/api/gpu/catalog` | which GPUs each platform offers |
| `GET` | `/api/lanes` | what may be done on each host (refusals built in) |
| `POST` | `/api/gpu/probe` | measure latency / throughput on a GPU |
| `GET` | `/api/gpu/rentals` | current rentals and their state |
| `POST` | `/api/gpu/apply` | submit a rental application |
| `POST` | `/api/gpu/launch-plan` | order the configured deployments into launch waves (§9) |

> **Status.** Read [23-implementation-status.md](23-implementation-status.md) before relying on any
> behaviour described here. This document describes the intended contract; the status note there says
> which halves are built and verified.

---

## 1. The one thing to understand first

**Modal publishes no public REST API for deploying an app.** This was checked directly, not assumed:
`https://modal.com/docs/api` → 404, `https://modal.com/docs/rest` → 404, `https://modal.com/api` →
404, and `api.modal.com` answers 415. The only documented deployment surface is the CLI
(`modal deploy [OPTIONS] APP_REF`, with no `--detach`).

So Manarouter does the honest thing:

1. It **generates the deployment bundle** — a complete, runnable `modal_app.py` plus the exact
   `modal deploy …` command line, with the GPU, model, quantisation, context length, KV dtype,
   concurrency and cooldown already filled in from the panel's answers.
2. It **shows the operator what will be deployed and what it will cost**, and asks them to run it.
3. When the deployment answers at its `*.modal.run` URL, Manarouter registers it as a provider and
   routes to it.

**Modal flow: bundle → manual deploy → paste `base_url`.** The operator runs the `modal deploy`
command themselves (or on a host where the CLI is installed) and pastes the resulting `*.modal.run`
URL back so Manarouter can route to it.

**Manarouter never claims to have deployed anything it did not deploy.** There is no code path that
fabricates a successful `modal deploy`. If the `modal` CLI happens to be installed and the operator
opts in, the bundle can be executed locally — but that path is a convenience, not the mechanism, and
it degrades to "here is the command, run it yourself" when the CLI or the Python SDK is absent. On the
development host used for this work, **neither is installed**, so that path is *unverified by
construction* and must never become load-bearing.

Everything else in this document is built on that split: Manarouter computes and plans with high
confidence, and hands the irreversible platform action to the human.

## Other deployment platforms

| Platform | Flow |
|---|---|
| **Kaggle** | Serving is **not possible**: a notebook has no inbound endpoint, so Manarouter marks Kaggle `can_serve_requests: false` and will not present it as a viable serving target. Work is submitted as a kernel and collected as files ([26-gpu-lanes.md](26-gpu-lanes.md) §3). |

Cerebrium, Saturn Cloud and molab were also listed here as deployment paths. Their lanes were removed
from the router — with their routes, their generator and their vault entries — so this document no
longer describes them. What remains for these platforms is the record under `docs/research/`.

---

## 2. The two lanes (requirement 9)

A deployment is in one of two modes. They are independent deployments, not two halves of one
deployment, and they can run at the same time on the same model with different card configurations:

| Mode | What it is | Which card | Why |
|---|---|---|---|
| **Streaming** (`streaming`) | A live vLLM endpoint holding N simultaneous sessions. Each request answers on its own; nothing waits for company. | the card that keeps latency down | latency matters more than throughput |
| **Batch** (`batch`) | *n* requests are collected into a batch on the deployment's card and executed together, then the answers are written out. | the card where throughput is cheapest | throughput matters more than latency |

Streaming is not "single": a streaming endpoint is one container with a KV pool and a
`--max-num-seqs`, so it serves many sessions concurrently — that is the whole point of deploying
vLLM rather than calling a model one request at a time. The wizard asks how many simultaneous
sessions it must hold, and the fit check judges that number, not one.

**A batch has to decide when to fire.** `batch.fill` is one of:

| Fill | What happens | When to choose it |
|---|---|---|
| `full` (default) | Nothing ships until the batch holds `batch.size` items — however long that takes. | the job is big enough that a full batch always arrives, and a partial one wastes a cold start |
| `timeout` | Whatever has arrived ships after `timeout_secs` (1–3600) as a short batch. | the traffic is bursty, or the operator would rather have a partial answer promptly than wait for stragglers |

`full` really is unbounded: a request can wait indefinitely for company. That is an informed choice
by the operator, recorded with the deployment, not a timeout someone forgot to set; the panel says so
in the sizing step. `timeout` always carries its duration, and the shape is validated on both sides:
a size of zero, a duration without the timeout policy, or a timeout policy without a duration is
refused.

The fill policy never changes the fit arithmetic. A batch is judged against `batch.size` items (it
must be able to hold that many at the configured context length); a streaming deployment is judged
against `sessions`. The rest of the sizing is identical.

The operator's own measurements on a 27 B-class model make the case: the more expensive card came out
**40 % cheaper per token** on decode-bound batches (derived $1.26/M vs $2.08/M at 2K context with 64
streams), while the cheaper card is the one that makes a small job worth starting at all. Capacity for
admission is measured in **KV tokens, not requests per minute** — the pool is a token pool, and a
512-token request and a 200 000-token request do not cost the same thing.

A single request that arrives when the pool is full is refused with a distinguishable reason
(`kv-budget-exceeded`), not silently queued behind a 200 K-token batch job.

---

## 3. Creating an account (requirement 8)

**An account is a platform credential. It is created on its own, and many deployments may bill
through it.** This is the one place the GPU-rental flow deliberately differs from the ordinary
provider flow, where adding a provider drops you straight into that provider's first-account form.

Creating a rental account asks for the platform and its credentials — for Modal, a **proxy token**
pair, `Modal-Key` (`wk-…`) and `Modal-Secret` (`ws-…`). The form also asks for the optional Modal
**API token** pair (`ak-…` / `as-…`) — the credential the Modal CLI/API uses, a different pair from
the proxy tokens — stored as its own vault entry when given. Each credential is written to the vault
as a bundle and referenced from `config.json` by id; **the secret itself never appears in
`config.json`, in a snapshot, in a log, or in this repository.** Entering it goes through the same
`--secret-stdin`-style flow the rest of the project uses, so it does not land in shell history.

An account carries no model. Models are attached to it afterwards, and the relation is not 1:1 in
either direction: one account can run many deployments, and the same repository on the same card can
be deployed under a second account when the operator needs more containers than one workspace will
give. What identifies a deployment is the tuple (account, repository, mode, card, card count); two
deployments that agree on all five are the same provider and the second is refused as a duplicate,
while disagreeing on any of them produces a distinct provider id, app name and endpoint.

**Hugging Face tokens are their own list, not account state.** A token is created on its own page —
`Add HuggingFace token`, the same shape as adding an account — and stored at `hf:<id>`. A deployment
that needs one names it; a token can be used by deployments on several accounts, several tokens can
exist for one operator, and a public repository needs none. Nothing requires the number of tokens to
match the number of accounts. Re-recording an existing token id replaces the value in place, which is
how a token is rotated without touching any deployment.

**The page itself has three actions and one list.** `#/gpu-rentals` opens on the operator's existing
deployments — provider id, model, account, card, mode, fill policy, endpoint, and the state the
conductor reports — with three actions above it:

| Action | What it opens |
|---|---|
| **Deploy model** | the deployment window: account, model, mode, batch size and fill policy (batch only), sessions (streaming only), card, card count, model quantisation, KV cache dtype, then the fit report and the bundle. |
| **Add Modal account** | the same form `#/providers` uses to add a provider: id, label, plan, workspace, proxy token and secret, each with a hint, validated in place. |
| **Add HuggingFace token** | the same shape again, for a token: id, label, value. The value is written once and only a fingerprint is ever shown back. |

The window is where the fit check runs. Nothing is applied until the numbers agree, and a sizing that
does not fit produces an error naming what to change — a bigger card would hold it, this quantisation
would, a smaller cache dtype would, fewer sessions or a smaller batch would — with the exact value
that fits, not advice to "reduce the batch size". Each suggestion is produced by re-running the same
sizing with that one field changed, so it is printed only when that change makes this repository fit
this card; the order is the order of the cost the change imposes on the model (card, weights, cache,
concurrency, context), and the first suggestion is the one to take unless a later cost matters more.
"Record deployment" stays disabled until the report is positive, and clicking a suggestion edits the
draft rather than applying anything: the write still happens once, on the button.

A sizing the panel cannot compute is refused as **unknown**, never reported as "does not fit": the
report names the missing figure (a catalog width for the quant, a `head_dim` in the config, an
unrecognised card), prints no suggestions because none of them could be proven, and keeps the button
disabled. The distinction matters because "does not fit" is a claim about a measurement, and an
unestimated pool is not one. The step wizard behind `#/gpu-rentals/new/<step>` still exists for a
first-time walkthrough, and the window reuses it rather than duplicating the questions.

---

## 4. Attaching a model to an account

This is the flow that satisfies requirements 1–7. The panel walks it as a sequence of steps —
platform, account, model, card, sizing, review — and each step is validated before the next is
offered.

### 4.1 Paste the model, then the quantisation opens up (requirements 4, 5)

The quantisation field is **blank and disabled until a model has been pasted**, because the available
quantisations depend on the model. Once the model is known, the panel determines its size:

1. **From the name first.** `…-27B-…` means 27 billion parameters. This is reported as
   `from_name`, and it is cheap and usually right, but it is a *name*, so it is not trusted as the
   final word.
2. **Then from the repository**, using the Hugging Face metadata API, which is authoritative about
   what actually exists.

The candidate list is then **intersected with what the chosen GPU can actually run**. Quantisation
methods are not GPU-neutral: an FP8 kernel needs a compute capability the card may not have, and
asking a T4 for an FP8 GEMM will fail at load time, not at request time. The panel shows what remains
after that intersection rather than offering a menu of things that will break.

### 4.2 A quantisation that does not exist is an error *and* a plan (requirement 6)

Asking for a quantisation the repository does not ship is not a dead end and it is not a silent
downgrade. The probe answers with an error **and** a `requantize` plan: the source quantisation
actually available, the target asked for, the tool that would do it, approximate time and VRAM, whether
it fits on the chosen card, and the ordered steps.

**Manarouter does not run the quantisation.** It prints the exact command for the operator to run, in
the same spirit as §1: the tool is named, the parameters are computed, and the human executes it.
Re-quantisation is an offline job (llm-compressor, AutoAWQ, GPTQModel, or vLLM's own online FP8 path),
not something a router should start behind your back.

### 4.3 The Hugging Face token appears only when it is needed (requirement 7)

A token field that is always visible trains people to paste secrets into forms. The panel asks for an
HF token **only once the repository is known to be gated** — and it can know that **before any login
prompt**, because the metadata endpoint answers anonymously (HTTP 200) and reports `gated` as
`false`, `"auto"` or `"manual"` even for repositories whose *files* are 401.

So: public repository → no token field at all. Gated repository → the field appears, with the reason,
offering the recorded tokens by id and a "new token" path that writes `hf:<id>` through
`create-hf-token` before the deployment is submitted. The token is never inherited from the account,
and the deployment records the token *id*, never the value.

One consequence has to be said plainly: recording the id does not put the token in the container. The
generated `modal_app.py` mounts the Modal secret `manarouter-hf` by name whenever the deployment names
a token, and never carries the value itself, so a gated repository needs the operator to create that
secret once (`modal secret create manarouter-hf HF_TOKEN=...`) before the first deploy; the apply
response warns about this whenever a token is named. No hand edit of the generated file is needed —
only the one-time secret.

**And a gated repository is not the end of the sizing story.** A quantised repository usually names
its unquantised base (`cardData.base_model`), and the base is usually public. Fetching the base's
`config.json` anonymously gives the real layer and head topology, which is what the KV calculation
needs. Manarouter follows that path automatically. `confidence: "unavailable"` is reported only when
the repository *and* its stated base are both inaccessible.

### 4.4 Which GPUs are listed, and the honest `(Paid)` marker (requirement 10)

The catalogue lists every GPU the platform offers. Requirement 10 asks for a `(Paid)` marker on the
GPUs that do not work on the free plan, **if the operator is on the free plan**.

**That marker cannot be implemented as literally stated, and the reason matters.** Modal publishes no
per-GPU free/paid gating at all: all eleven GPUs carry unconditional per-second prices, and the
platform's own words are that *using a GPU requires having a valid payment method on file* — which is
a property of the whole account, not a discriminator between cards. A `(Paid)` badge on eleven GPUs
would therefore be **fabricated**.

What the panel does instead, and why each part is defensible:

| What is shown | Basis | Provenance |
|---|---|---|
| Plan limits per plan — Starter 10 concurrent GPUs / $30 monthly credits; Team 50 / $100 | published plan documentation | `source: "documented"` |
| `(Paid)` on a GPU **refused on the Starter plan** | observed directly | `source: "observed"` |
| nothing on a GPU whose plan availability is unknown, with the provenance stated | absence of evidence | `source: "unknown"` |

So exactly **one** card carries `(Paid)` — the H100, which was observed refused on Starter — and the
rest honestly say `"unknown"`. A test asserts that split, so the honesty cannot silently erode.

`(Paid)` renders **only** for an observed refusal. Unknown is not paid, and the panel does not let a
guess look like a fact.

### 4.5 Two sizing numbers the operator should not have to guess

| Operator's words | What it is | What it does |
|---|---|---|
| **"Simultaneous Sessions"** (streaming) | how many requests must be in flight at once; sized against the KV pool | the fit check refuses the deployment if the pool cannot hold that many at `max_model_len` |
| **"Batch size"** (batch) | how many items one batch holds | replaces sessions in the fit check; the batch must be able to hold all of them at once |
| **"Shutdown Cooldown (seconds)"** | Modal's `scaledown_window` (default 60 s, range 2–1200 s) | how long an idle container stays warm before it is released |

`--max-num-seqs` is still what vLLM schedules per step, and the generator sets it from the
deployment's own numbers — but it is a scheduling knob, not the operator's question. The operator
answers "how many sessions at once" for streaming, or "how big is a batch" for batch; the panel
derives the flag. Confusing the two is what makes a sizing form ask a question nobody can answer.

They are shown together because they trade against each other: a longer cooldown costs money while
idle and saves a cold start later, and a cold start on this class of model is **~110 s on the cheaper
card and 160–181 s on the more expensive one**. Cold-start cost is a fixed per-batch cost, not a
per-request one, which is why the batch lane should say "too small to be worth a cold start" rather
than quietly starting one.

---

## 5. The KV cache number, and exactly how much to trust it (requirement 1)

The requirement is that the KV cache size is calculated **automatically**. It is, and the calculation
is worth explaining because a wrong answer here is not visible — it just means the container refuses
work it could have done, or accepts work it cannot hold.

vLLM's pool is, per token:

```
bytes_per_token = caching_layers × num_kv_heads × head_dim × KV_dtype_bytes × 2      (K and V)
kv_pool_tokens  = (gpu_memory_utilization × VRAM − weights − overhead) / bytes_per_token
```

with `gpu_memory_utilization = 0.92` and `block_size = 64` (vLLM's own defaults), and
`overhead_bytes` covering activations, CUDA-graph capture, block padding and any fixed recurrent
state — everything except the weights.

Three things are easy to get wrong, and this project got all three wrong at first:

1. **`caching_layers` is not `num_hidden_layers`.** A hybrid model can be mostly *linear* attention,
   where those layers hold a fixed-size state that does **not** grow with sequence length and
   therefore has no per-token KV cost. On the operator's own model — 64 layers, of which **48 are
   `linear_attention` and only 16 are `full_attention`** — the naive formula is **exactly 4× too
   high** (131 072 vs 32 768 bytes/token), which would under-report capacity four-fold. The layer
   split comes from the model's `config.json` `layer_types`.
2. **`head_dim` must be read, not derived.** The common fallback `hidden_size / num_attention_heads`
   gives `5120 / 24 = 213.3` on that model, against a declared `head_dim` of **256**. Prefer the
   explicit key.
3. **`overhead_bytes` is an assumption, and it is labelled as one.** vLLM documents no separate
   figure, so Manarouter uses **8 GiB** and says so. Two independent measurements of the same model
   imply 7.27 GiB and 7.85 GiB respectively, so the honest accuracy claim is **about ±2 %**, and the
   result is reported with `delta_vs_measurement_pct` next to the operator's measured value rather
   than being presented as exact.

**Independent confirmation is not claimed.** When a measurement exists, the panel shows the analytic
estimate and the measured number side by side and lets the operator pick. Two cautions are stated in
the output itself:

- A fit that was solved *from* two measurements cannot then be described as validated *by* them.
- **vLLM's own startup line is authoritative** — it prints `GPU KV cache size : N tokens` and
  `Maximum concurrency for L tokens per request : Cx` — and it is not available until the container
  has booted. The calculated number exists to fill the gap before that, and to be replaced by it.

Where the architecture genuinely cannot be read, `confidence` is `"unavailable"`, `kv_pool_tokens` is
`null`, and the reason is in `warnings` — the panel does not invent a number to fill the box.

---

## 6. What the operator ends up with

On success a rental deployment is a normal Manarouter provider of dialect `modal-vllm`: it appears in
the provider list, its models appear in `/v1/models`, and it can be used in combos like anything else.
Its provider id names the shape — account, repository, mode, card and card count — so
`#/providers` lists a streaming endpoint and a batch endpoint of the same model as two providers
rather than one that silently changed meaning. Deployments applied before this identity widened keep
their old ids: a provider is not renamed under an operator's feet.
The difference is that its capacity is tracked in **KV tokens** and its spend is tracked in
**dollars**.

Cost accounting is per-GPU-second, from the published per-second price for the card, because a
scale-to-zero deployment is billed for the time it exists rather than for a monthly average. A
`$X/month` figure that assumes 730 hours of uptime is wrong for a deployment that spends most of its
life scaled to zero, and the panel does not show one.

---

## 7. Operationally

- The **relay** (port 20900) is the request path and is the only process that talks to the rented
  GPU. It learns about rental changes through the compiled snapshot digest, exactly like every other
  provider — there is no IPC between the two processes.
- The **conductor** (port 20901) owns the admin surface: the catalogue, the probe, the sizing, the
  bundle generation, and the panel.
- Restarting is a live-traffic event. The units bind `0.0.0.0`, and the restart procedure is in
  [23-implementation-status.md](23-implementation-status.md) and the README; do not restart to test a
  change that a test can prove.

---

## 8. What Manarouter will not do

- **Deploy for you** and tell you it succeeded. See §1.
- **Run your quantisation.** See §4.2.
- **Show a `(Paid)` badge it cannot evidence.** See §4.4.
- **Claim an exact KV number.** See §5.
- **Present a fitted constant as a physical one.** `overhead_bytes` is named an assumption in the
  output, every time.
- **Serve request/response traffic from Kaggle.** A notebook has no inbound endpoint, so there is
  nowhere for the relay to send a request. The lanes API marks Kaggle `can_serve_requests: false`
  for that physical reason, not as a policy judgement, and the panel will not offer it as a serving
  target. See [26-gpu-lanes.md](26-gpu-lanes.md).

The last five are the same rule stated five ways: where the platform does not publish a fact,
Manarouter reports *what it does know* and *how it knows it*, and refuses to manufacture the rest.

---

## 9. Launching several deployments without launching them all at once

The requirement behind this section is one sentence: *"gpu rentals should be able to be done in
sequence so 2 gpus are never running or as batches of multiple GPUs."* Two cards billing at the same
time by accident is the failure mode; two cards billing at the same time **on purpose** is a feature.

So the operator states a policy and Manarouter orders the work to fit it.

| Schedule | Meaning |
|---|---|
| `sequential` | at most one card is live at any moment |
| `batched:N` | up to `N` cards are live together |

`POST /api/gpu/launch-plan` with `{"schedule": "sequential"}` or `{"schedule": "batched:4"}` returns
the waves that policy produces for the deployments already in the config. It is a **read**: it writes
nothing, starts nothing, and tells the relay nothing. Starting a card stays a separate, explicit
action.

```json
{
  "schedule": "batched", "max_live": 4,
  "waves": [{"cards": 4, "deployments": [0, 1, 2, 3]}, {"cards": 2, "deployments": [4, 5]}],
  "problems": [], "malformed": [], "deployments": [{"index": 0, "...": "..."}],
  "note": "…"
}
```

Wave entries are **indices into `deployments`**. A list of bare integers is not an answer, so the
`deployments` array is returned alongside it; a plan whose indices cannot be resolved would be a plan
nobody can act on.

Four decisions worth stating, because each is a place where a plausible default would be a lie:

- A **bare `"batched"` is refused.** It does not say how many, and choosing one on the operator's
  behalf would make it indistinguishable from `"sequential"` — the two answers differ by a factor of
  `N` in how much money is running. The 400 repeats both accepted forms.
- A **batch size of `0` is refused, never clamped.** Clamping to 1 would turn an impossible request
  into a billable launch.
- A deployment whose own `gpu_count` exceeds the ceiling is **excluded and reported**, not silently
  split. Splitting a multi-GPU deployment would change what the model is being sized for.
- An account's `max_concurrent_containers` is reported as a **problem, never enforced**, because the
  platform is the authority on its own limit and that field is populated for one platform and `None`
  for the rest. Re-planning the operator's work against an unverified number would move real work for
  no reason.

A deployment that does not parse is listed under `malformed` rather than dropped, so a partial plan
cannot look complete.

`RentalMode` (`interactive` / `batch`) is **not** this. It labels what a deployment is *for*; the
schedule says how many may be live at once. Conflating them was the reason `RentalSchedule` got its
own type.

---

## 10. A ceiling on what a batch may spend

Every dollar figure the batch lane reports is a report. A report does not stop anything, so a submission may
also carry a ceiling:

```json
{"input_file_id": "file_…", "endpoint": "/v1/chat/completions",
 "completion_window": "24h", "max_cost_usd": 2.50}
```

`max_cost_usd` is optional; absent or `null` means no ceiling, which is what every batch submitted
before this field existed still means. When it is present it must be a finite number of dollars
greater than zero and at most 1000. Zero is refused rather than obeyed: it would stop before the first
item while looking like a request that runs a batch. Anything that is not a number — a string, a
boolean, an array — is refused with the usual `invalid_request_error` shape instead of being coerced.

**The check is against money already spent, never against a forecast.** Before each item is
dispatched the lane reads the ledger it has been keeping (`gpu_seconds × price_per_second`, plus the
boot fee for a cold start) and dispatches nothing further once that total has reached the cap. The
consequence the operator should expect is that the cap is a *stopping* rule, not a hard bound: an item
that is already in flight is never interrupted, because killing a request mid-answer wastes the GPU
seconds it has already been billed for and yields neither a result nor an error.

Two honest limits follow, and both are reported rather than papered over:

- **An endpoint with no configured `price_per_second` contributes nothing to the ledger.** A capped
  batch can therefore overspend by the unpriced share of its work, but it can never under-report what
  it spent. If the operator wants the ceiling to bind exactly, they must declare the price.
- **Items that never ran are retired, not failed.** They appear in no result file and in no error
  file, exactly as a cancellation retires them: nothing about them failed, the ceiling arrived first.
  `request_counts` therefore sums to fewer than `total`, which is the visible sign that a batch was
  cut off.

The terminal status is `cancelled`, because the OpenAI batch vocabulary has no "stopped early" status
and inventing one would break clients that switch on the documented set. The reason is never left to
inference: the batch object carries `budget_exceeded` (a boolean) and `stopped_reason`
(`"max_cost_usd_exceeded"`, or `"cancelled"` when an operator cancelled it), alongside
`max_cost_usd` echoing the requested cap so a reader can tell "no cap" from "a cap that was not hit".
A batch that ends with its spend exactly at the cap reports `budget_exceeded: true` as well: the
ceiling did bind, and pretending otherwise would invite a retry with the same cap.

These three fields are **extensions to OpenAI's batch object**, not part of its schema. A client that
does not know them ignores them; one that reads them learns something OpenAI's own batches cannot say.

## 11. Uploading the input file

A batch is submitted in two steps: upload the JSONL, then reference the file id. The upload route
accepts both shapes people actually use, and they are equally supported:

```bash
# What OpenAI's examples and every SDK send.
curl -H "Authorization: Bearer $KEY" \
     -F purpose=batch -F file=@input.jsonl \
     http://127.0.0.1:20900/v1/files

# What a shell client sends when it would rather not build a multipart envelope.
curl -H "Authorization: Bearer $KEY" -H 'Content-Type: application/octet-stream' \
     --data-binary @input.jsonl \
     'http://127.0.0.1:20900/v1/files?filename=input.jsonl&purpose=batch'
```

`multipart/form-data` is the documented shape, so when a request somehow carries both, the form
decides. In the form, `file` is the only part with bytes and `purpose` is a text field; another part is
read and dropped rather than refused, because a client library that adds a field this lane ignores is
still sending a valid upload. A form with no `file` part is refused with `invalid_request_error`, and
so is a second `file` part — one upload is one file.

The filename is metadata. It is recorded next to the file id and is never used to open a path, so a
name containing `../` cannot reach outside the state directory. `purpose` may be `batch`,
`fine-tune`, or absent, which the lane treats as `batch`; any other value is refused with
`invalid_request_error` naming the purpose it got. A `fine-tune` file is stored exactly like a batch
file — same bytes, same 32 MiB cap — and is staged to a trainer by the fine-tuning job rather than by
this lane.

Both shapes were exercised against the running relay with the same input file, and the result of the
form upload was read back from `/v1/files/{id}/content` byte for byte, which is the only way to know
that the lane stored the JSONL rather than the envelope around it. That check is worth repeating after
any change to the route: the first version of it stored the raw multipart body, and every other part
of the lane — the line parser, the counts, the cap — was happy with a file that was not a batch.

## 12. Sharing one card with interactive traffic

The batch lane and the request path dispatch through the same executor and the same KV budget, so a
batch is not a separate card — it is a second consumer of the one the operator rented. Two facts make
that safe to run overnight:

* An item holds nothing while it waits. Its KV reservation is released before the retry sleep, so a
  batch cannot starve a request by queueing, only by running.
* The pool of workers is the knob. `MANAROUTER_BATCH_WORKERS` (default 4, clamped to 1–64) sets how
  many items a batch may have in flight at once; `1` gives a batch one slot and leaves the rest of
  the endpoint to interactive calls.

**This lane is not the deployment's batching.** §2's `batch` mode is a property of the deployed
endpoint: the container collects items and decides when to fire, and the fill policy is recorded with
the deployment. The relay's batch lane is a separate thing: it takes an uploaded JSONL file, dispatches
items through the ordinary request path, caps the spend, and retries. It does not read the deployment's
`mode` and does not wait for a batch to fill; if an operator wants items to accumulate before dispatch,
that happens on the rented endpoint, which is where the `fill` policy applies. The two are documented
together because they are easy to confuse and impossible to substitute for one another.

There is deliberately no priority class and no pre-emption. Cancelling a generation to make room for
another request does not refund the GPU seconds — the card was occupied either way — so the choice is
between a batch delaying an interactive request by one item and an interactive request costing the
operator a killed generation. The lane picks the first, and an operator who wants interactive traffic
always to win lowers the worker count rather than trusting a scheduler this build does not have.

## 13. Fine-tuning on the same card

Serving and fine-tuning are two apps, not two modes of one. The reference trainer in
[deploy/modal-finetune/](../deploy/modal-finetune/README.md) is a single Modal app on the same
`RTX-PRO-6000` string — 96 GB at $3.0312/hr per container, the same card §1 describes — but its
container trains instead of serving: `transformers`, `peft` and `trl` on a `modal.Volume` at
`/state`, with `@modal.concurrent(max_inputs=1)` and a 30-minute startup allowance. It declares
itself as a `modal-vllm` provider with `capabilities: ["fine-tuning", "chat", "models"]`, so the relay
routes `POST /v1/fine_tuning/jobs`, the job by id, its `events` and `checkpoints`, and `pause`,
`resume`, `cancel` to it, and stages a training file uploaded to the relay's own store as multipart
with `purpose=fine-tune`. What it does not do is serve chat and train at once: chat is refused with
`409` while a job is active and the model cache is evicted when a new job starts, which is the honest
shape for a card that cannot hold both.

The credit floor in [key selection](05-key-selection.md#the-declared-balance-and-the-one-route-ranked-by-it)
applies here too, and it does not know that Modal bills the GPU separately: the trainer's account has
no provider-side balance to probe, so a fresh installation refuses `POST /v1/fine_tuning/jobs` with
`402 insufficient_credit` until the operator declares one. That is deliberate — the declaration is
the acknowledgement that this route spends money — and
`mr account set-credit modal-vllm <account> --usd 15.01` is enough. The number is a floor for choosing
an account, not a budget; Modal's own spend check remains `modal app list`.

Two constraints belong here because they are easy to forget when the numbers look cheap. The hourly
rate is per container that is *running*; an idle deployment costs nothing after `scaledown_window`,
which is why `modal app list` is the spend check and why the cost line in §10 is about a batch, not
about a fleet. And the app has never been deployed: its 61 tests run CPU-only against stubs, so the
code is a design that compiles, not a training run with a measured duration. A first real job is
where the per-step time, the memory headroom on a 1.5B base model, and the volume's throughput become
facts rather than estimates.

## See also

- [research/ARCHITECTURE-gpu-rentals.md](research/ARCHITECTURE-gpu-rentals.md) — the frozen interface:
  object model, config schema, conductor API, catalogues, and the platform facts with sources.
- [26-gpu-lanes.md](26-gpu-lanes.md) — the remaining lane surface on the conductor (Kaggle): routes,
  request and error shapes, and why no batch platform can hold an endpoint open at all.
- [providers/modal-vllm.md](providers/modal-vllm.md) — the `modal-vllm` dialect on the wire.
- [research/modal-platform.md](research/modal-platform.md) — the verified Modal facts: GPU ids,
  prices, plans, proxy tokens, `scaledown_window`.
- [research/vllm-sizing-and-quant.md](research/vllm-sizing-and-quant.md) — the KV formula, the hybrid
  correction, and the quantisation support matrix.
- [research/huggingface-discovery.md](research/huggingface-discovery.md) — anonymous metadata,
  gated detection, and the `base_model` workaround.
- [research/requantize-recipes.md](research/requantize-recipes.md) — the commands behind §4.2.
- [deploy/modal-finetune/README.md](../deploy/modal-finetune/README.md) — the reference fine-tuning
  server: routes, state layout, environment, deployment, and its own limitations.
