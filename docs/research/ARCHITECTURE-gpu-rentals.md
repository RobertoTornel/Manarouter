# GPU rentals — frozen architecture and interface contract

Status: **authoritative interface spec** for the work in progress (2026-11-12).
Every agent working on this feature reads this file first. Do not change a wire name,
a file path, or a JSON key in this document without saying so explicitly in your report.

> **Amended 2026-11-13.** Saturn Cloud, molab and Cerebrium were removed from the router: their lane
> modules, routes and vault entries are gone, and `RentalPlatform` now lists only `modal` and
> `kaggle`. References to them below are kept as the record of what was designed and why it changed;
> for the surface that is live today read [../26-gpu-lanes.md](../26-gpu-lanes.md).

Provenance marker used throughout: **[facts]** = verified against primary documentation and
recorded in this directory's research files; **[observed]** = measured by the operator and recorded
in `issue-4-spec.md`; **[inferred]** = derived by us and clearly labelled as an estimate in the UI.

### Implementation status against this contract (2026-11-12)

The sections below are the contract. This list is what the tree actually does today; where the two
differ, the contract is the intent and this list is the truth.

**Implemented**

* `crates/mr-core/src/rental.rs` — the account and deployment shapes, `RentalSchedule`
  (`sequential` by default, `batched:N`), and launch-wave planning, all unit-tested.
* `crates/manarouter-conductor/src/gpu_api.rs` — seven routes: the five in §5 plus
  `POST /api/gpu/launch-plan` and `GET /api/gpu/lanes`. `apply` creates, and
  answers `409 already_exists` for a taken id or an existing vault entry; there is no update or
  delete route.
* Bundle generation — `modal_app.py` and its `modal deploy` command. (The Cerebrium `cerebrium.toml`
  generator went with issue #10's lane, removed 2026-11-13.)
* The panel setup flow (catalog → account → model → sizing → bundle), including reuse of an account
  that is already stored: `USE` points a second deployment at the same id and sends no credential.
* One lane held in conductor memory: Kaggle. (Molab, Saturn and Cerebrium were removed 2026-11-13.)
* `apply` takes three actions — `create-account`, `create-hf-token`, `create-deployment`. A Hugging
  Face token is its own record at `hf:<id>`, named by each deployment that needs it (§3.4), and
  re-recording an id rotates the value in place rather than creating a second token.
* Batch deployments record `BatchShape` (`size`, `fill`, `timeout_secs`) and the fill policy is
  reflected in the generated bundle's `mode_note`; the bundle also carries the assembler that
  enforces it (§3.2).
* The panel's `#/gpu-rentals` page is three actions over one list — `Deploy model`,
  `Add Modal account`, `Add HuggingFace token` — with the fit check inside the deploy window. The
  step wizard behind `#/gpu-rentals/new/<step>` remains for a first-time walkthrough.

**Not implemented, and not claimed by the panel**

* A router-controlled Modal lifecycle. The conductor never runs the `modal` CLI or an SDK; a
  container starts when the operator runs the bundle's command.
* A per-batch dollar cap on a *deployment*. The relay's batch lane has `max_cost_usd`
  ([../18-api-reference.md](../18-api-reference.md)), but `RentalDeployment` carries no budget field; the panel reports measured cost after the fact and
  derives nothing it did not measure.
* A mounted Hugging Face secret. The bundle names no `modal.Secret`, so a gated repository needs the
  operator to mount `manarouter-hf` by hand; `create-deployment` warns when a token is named.
* Lane credential persistence. The lanes that remembered an attach (Saturn, molab, Cerebrium) wrote
  it to the vault; those lanes were removed together with `lane_vault.rs`, so no lane credential is
  stored today (`docs/26-gpu-lanes.md` §9).

---

## 1. The problem in one paragraph

Manarouter can already route to an OpenAI-compatible endpoint. It cannot help an operator *create*
one. Renting a GPU is four decisions the operator has to get right by hand — which card, how big the
KV cache can be, which quantisation the card can actually execute, and when the container is allowed
to stop — and today none of those are computed, validated or recorded. The feature under
construction turns those decisions into a form that computes the answer, refuses the impossible
combinations, and writes a provider the relay can drive.

## 2. The single most important verified constraint

**Modal publishes no REST API for deployment.** [facts] `docs/research/modal-platform.md`: the
supported paths are the `modal` CLI and the Python SDK (`modal.App`, `app.deploy()`). There is no
`https://api.modal.com/v1/apps`.

Therefore, and this is not negotiable:

* Manarouter **generates a deployment bundle** — a complete, correct `modal_app.py` plus the exact
  command — from the operator's decisions.
* Manarouter **does not pretend to deploy**. It never POSTs to an invented Modal endpoint.
* The wizard's last step registers the app once it is reachable at the URL the operator confirms,
  and the wizard can poll that URL to confirm it answers.
* If the `modal` CLI is present on the host, offering to run it is a *separate, opt-in, cost-shown*
  action. It is not the default path and it is not required.

**On this host, it is not present.** Checked 2026-11-12: `command -v modal` finds nothing, and
`python3 -c "import modal"` raises `ModuleNotFoundError`. So the opt-in execution path is
**untestable here** and must be written to detect the CLI's absence and say so plainly rather than
fail obscurely. It also must not become load-bearing: the bundle-and-command path is the one that
works with nothing installed, which is exactly why it is the primary path. Any claim about the
CLI path in this workstream is unverified by construction, and is labelled as such.

An implementation that fakes a deploy call is a failure, not a shortcut.

### 2.1 Verified platform facts that the generated bundle must honour

These were re-checked directly against Modal's documentation on 2026-11-12 by the orchestrator, as an
independent second pass over the research agent's work. They are not decoration: each one changes
what the generated `modal_app.py` must contain.

| Fact | Value | Source |
|---|---|---|
| Canonical `gpu=` values | `T4`, `L4`, `A10`, `L40S`, `A100`, `A100-40GB`, `A100-80GB`, `RTX-PRO-6000`, `H100`/`H100!`, `H200`, `B200`/`B200+`, `B300` | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| `A10G` | **not** a valid value; the only `A10G` occurrence on the pricing page is an FAQ marketing example | [gpu.md](https://modal.com/docs/guide/gpu.md), [pricing](https://modal.com/pricing) |
| Multi-GPU syntax | `gpu="H100:8"` — count appended with `:n` | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| Multi-GPU limits | up to 8 per container for B300/B200/H200/H100/A100/L4/T4/L40S; **A10 only up to 4** | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| H100 variant | always SXM on Modal; Modal **may auto-upgrade `gpu="H100"` to an H200 at no extra cost**; `H100!` suppresses that | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| H200 / H100 memory | H200 = 141 GB HBM3e, 4.8 TB/s; H100 = 80 GiB | [gpu.md](https://modal.com/docs/guide/gpu.md), [gpu-ram](https://modal.com/gpu-glossary/device-hardware/gpu-ram) |
| L40S memory | 48 GB | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| A10 memory | 24 GB (96 GB across the maximum 4 GPUs) | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| B300 requirement | **CUDA 13.1+**; the container image must be built for CUDA 13 | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| Any GPU | "Using a GPU requires having a valid payment method on file." | [gpu.md](https://modal.com/docs/guide/gpu.md) |
| `modal deploy` options | `--name`, `-e/--env`, `--stream-logs`, `--tag`, `-m`, `--timestamps`, `--strategy [rolling\|recreate]`. **No `--detach`.** | [cli/deploy](https://modal.com/docs/reference/cli/deploy) |
| Deployment strategy | `rolling` or `recreate` are accepted by the CLI, so the bundle should let the operator choose | [cli/deploy](https://modal.com/docs/reference/cli/deploy) |

Two consequences the generator cannot skip:

1. **`gpu="H100"` is not a fixed choice.** Modal may silently give an H200 instead, which changes
   the available VRAM and therefore the KV budget. The bundle must not hardcode `H100` while claiming
   a 80 GiB budget; it either emits `H100!` (pinned) or records that the pool may upgrade. The catalog
   carries this as a note, and the deployment record should keep the requested id as *requested*, not
   as observed hardware.
2. **B300 with a CUDA-12 image will fail.** The generated `modal_app.py` must not offer B300 unless it
   also emits a CUDA-13-compatible image, or it must warn in the bundle itself.

**VRAM provenance is weaker than the price provenance, and the code must not pretend otherwise.**
Modal publishes no per-GPU VRAM table; the figures above are the ones its own documentation happens to
state (H100, H200, L40S, A10) or NVIDIA datasheet values. Datasheet capacity is also **not** what a
container can use: the operator measured 95 GiB usable on the RTX PRO 6000 and 46 GiB on the L40S,
against 96 GB and 48 GB datasheet figures. Sizing therefore treats VRAM as an input with a stated
source and never as a self-evident constant, and the `overhead_bytes` term in §7 is what absorbs the
difference — visibly, with the delta reported, rather than silently.

## 3. Object model

The operator asked for requirements 8 and 9 in the original note:

> 8. Multiple models may be hosted under the same account; account creation should be separate from
>    model setup.
> 9. Support both one request on one chosen GPU, **or** batching when n requests pile up on another
>    chosen GPU.

That fixes the model to two entities.

### 3.1 Rental account — a platform credential, no model attached

A rental account is a **workspace credential for a platform**. It is created on its own, before any
model is chosen, and it is a *credential*, not a deployment: several rentals may bill through one
account, and the same model on the same card may be deployed once per account when an operator wants
more capacity than one workspace will give. Nothing about the account knows which models use it —
that direction is recorded on the deployment, as `RentalDeployment.account`.

The count is the operator's business. Three models on three GPU configurations, five or six accounts
and one or two Hugging Face tokens is a plausible arrangement, and so is one account used by one
rental; the only thing the conductor enforces is that every deployment names an account that exists.
A deployment's provider id carries the account and the shape, so two rentals of the same repo under
different accounts stay distinct providers rather than colliding.

```rust
// crates/mr-core/src/rental.rs
pub struct RentalAccount {
    pub id: String,                    // slug, operator-visible, unique
    pub label: String,
    pub platform: RentalPlatform,      // "modal" | "kaggle" | "custom:<x>"
    pub plan: RentalPlan,              // "free" | "paid" | "unknown"
    pub workspace: Option<String>,     // Modal workspace slug; not a secret, half of the derived URL
    pub secret_ref: Option<String>,    // vault entry id holding this account's own credential
    pub hf_secret_ref: Option<String>, // LEGACY: see "Hugging Face tokens are not account-scoped" below
    pub max_concurrent_containers: Option<u32>, // platform-side ceiling (Modal Starter: 10) [facts]
    pub note: Option<String>,
    pub extra: Map<String, Value>,     // exit hatch, round-trips unknown keys
}
```

`RentalPlan::Unknown` is a real state and must not be rendered as `Free`. Requirement 10's `(Paid)`
marker is only truthful when the plan is known *and* the GPU is known to be gated.

`hf_secret_ref` is kept only so files written before Hugging Face tokens became their own entity keep
working: a deployment whose repo is gated, and which names no token, still falls back to the
account's recorded token. Nothing new writes it. Tokens live in `Config.huggingface_tokens` and are
named by the deployment (see §3.4).

### 3.2 Rental deployment — one model on one GPU

A deployment is **a Manarouter provider**. It already has everything needed: `base_url`, `dialect`,
`auth`, `models`, `kv_pool_tokens`, `max_context`, `max_concurrent`, `price_per_second`,
`cold_start_ttfb_timeout_ms`, and a flattened `extra` map.

```rust
// crates/mr-core/src/rental.rs — parsed from Provider.extra["rental"]
pub struct RentalDeployment {
    pub account: String,             // RentalAccount.id
    pub repo: String,                // "orcarouter/Qwen3.8-27B-Uncensored-FP8"
    pub revision: Option<String>,
    pub gpu: String,                 // catalog id, e.g. "RTX-PRO-6000"
    pub gpu_count: u32,              // default 1
    pub weight_quant: String,        // catalog quant id, "fp8" | "awq" | ... | "none"
    pub kv_cache_dtype: String,      // "auto" | "fp8" | "fp8_e4m3" | "fp8_e5m2" | "bfloat16"
    pub max_model_len: Option<u64>,
    pub max_num_seqs: Option<u32>,
    pub shutdown_cooldown_secs: Option<u32>,
    pub mode: RentalMode,            // "streaming" | "batch"; legacy "interactive" loads as streaming
    pub batch: Option<BatchShape>,   // batch size and fill policy; required when mode == batch
    pub bundle_id: Option<String>,
    pub requantize: Option<RequantizePlan>,
}
```

Why `extra` rather than a new `Provider` field: `RoutingSnapshot` builds its own `ProviderPlan`
struct (`crates/mr-core/src/snapshot.rs:416`), so `Provider.extra` is **not** part of the snapshot
layout and adding keys to it cannot shift a byte in the compiled snapshot. A new `Provider` field
would be a `SNAPSHOT_VERSION` bump for no runtime benefit — the relay never reads the GPU name.
**[facts]**

`RentalDeployment` is still a typed struct in Rust: storage is flexible, the code that reads it is
not. `Provider.extra["rental"]` is deserialised on demand and a malformed value is a validation
error, not a silent default.

`mode` is one axis with two values, not two GPUs: a rental is one model on a card configuration.
`streaming` is a live endpoint several sessions reach at once; `batch` is a queue drained in bulk.
`batch_gpu` is gone — batching changes how the card is scheduled, never which card it is. The wire
value is `streaming`; older files that say `interactive` are still parsed as that same mode, but
nothing new is written with the old spelling.

**Identity is (account, repo, mode, card, count).** Two deployments that agree on all five are the
same provider and the second is refused; differing on any of them is a different endpoint with a
different app name and a different `base_url`. `DeploymentShape` (`rental.rs`) is that tuple, and
`app_slug`, `modal_app_name` and `modal_base_url` all take it, so the panel preview and the apply
path cannot disagree about a name. The suffix is `-<mode>-<card-lowercased>[-x<count>]`, e.g.
`manarouter-rental-mana-modal-qwen3-8-27b-streaming-rtx-pro-6000`; because the head is truncated at
64 characters first, the suffix is what survives and keeps the names distinct.

**A batch records how it fills.** `BatchShape { size, fill, timeout_secs }`:

```rust
pub enum BatchFill { Full, Timeout }   // "full" (default) | "timeout"
```

`full` means a request waits until the batch is complete, however long that takes — that is the
operator's stated choice, not a missing timeout. `timeout` means whatever has arrived ships after
`timeout_secs` (1–3600) as a short batch. `BatchShape::problems()` refuses a zero size, a timeout
without a duration, a duration without the timeout policy, and a `fill` value it does not know. The
fill policy never changes the fit calculation: a batch is judged against `batch.size`, a streaming
deployment against `sessions`.

**A batch deployment carries its own queue.** vLLM schedules whatever arrives and never holds a
request back waiting for a fuller batch, so "a batch of N" is not a vLLM flag: both generators emit
a stdlib-only assembler into `modal_app.py` when the deployment has a batch shape. vLLM listens on
`127.0.0.1:8001`; the assembler owns port 8000, holds each `/v1/chat/completions` request until
`BATCH_SIZE` requests are in hand (`fill: full`, which can wait indefinitely) or until
`BATCH_TIMEOUT` expires (`fill: timeout`), sends the whole batch to vLLM at once, and answers each
caller with its own response. The function is declared `@modal.concurrent(max_inputs=BATCH_SIZE)`:
Modal admits one input per container by default, and a batch that can never be full would wait for
a request that is not allowed to arrive. A streaming deployment keeps vLLM on port 8000 and
generates no assembler. `stream: true` is answered with the finished body in one piece — the
assembler exists for whole-response batching, not for token streaming.

`base_url` is **derived**, never pasted in the normal path. The account records the Modal workspace
and the deployment's generated app name is deterministic from the shape, so
`https://<workspace>--<modal-app-name>-serve.modal.run` is computed by one shared derivation used by
both the panel preview and `POST /api/gpu/apply`. Modal publishes no deployment REST API, so this is
a derived prediction, not a confirmed URL: `modal deploy` prints the authoritative one, and an
explicit `base_url` in the request overrides the derived value when they differ.

### 3.3 Why not `RentalAccount` as a provider

A provider requires a `base_url`, and a Modal account has none: on Modal each app gets its own
`https://<workspace>--<app>-<fn>.modal.run` URL. There is no workspace-level base URL to put in
`base_url`, so an account derived from a provider would have to carry a lie. A separate
`Config.rental_accounts` list avoids the lie and is the seam issues #10–#13 plug into.

### 3.4 Hugging Face tokens are not account-scoped

A Hugging Face token is a credential a *repository* needs, not a workspace. It belongs to the
operator, is named by id, and is written once to `hf:<id>`. The relation is many-to-many in both
directions: one token may be named by deployments on several accounts, several tokens may exist for
one account's deployments, and a public repository needs none.

```rust
// crates/mr-core/src/rental.rs
pub struct HuggingFaceToken {
    pub id: String,                  // operator-chosen, unique, half of the vault id
    pub label: String,
    pub secret_ref: Option<String>,  // "hf:<id>", written by create-hf-token
    pub note: Option<String>,
    pub extra: Map<String, Value>,
}
```

The config holds `huggingface_tokens: Vec<HuggingFaceToken>`, independent of `rental_accounts`.
Nothing forces the two lists to have equal length in either direction; the operator's arrangement
(five or six accounts sharing one or two tokens, or one token per account, or no token at all) is
recorded as given. `RentalDeployment.hf_token: Option<String>` is the deployment's reference, and
`Config` keeps no implicit "current token".

**What the record does and does not do.** `create-deployment` validates that a named token exists and
has a vault value, and records the id on the deployment, so a gated repository fails at apply time
rather than on the first fetch inside a container. It does **not** interpolate the token into the
generated `modal_app.py`: Modal has no per-run secret environment for this, and a bundle carrying a
token would be a bundle carrying a secret. The container reads a Modal secret named
`manarouter-hf`, which the operator creates on their own machine
(`modal secret create manarouter-hf HF_TOKEN=...`) and whose name the generated file should mount. The
recorded `hf_token` id says *which* token the deployment expects; mounting it is a deployment-time
step. **The mount is generated, the value is not:** the generated `modal_app.py` mounts that secret by
name — `HF_SECRET = modal.Secret.from_name("manarouter-hf")` and `secrets=[HF_SECRET]` on the
function — whenever the deployment names a token, so the operator's only remaining step is to create
the secret once with `modal secret create manarouter-hf HF_TOKEN=...` before the first deploy. The
token's value is never interpolated into the bundle: a bundle carrying a token would be a bundle
carrying a secret, and the panel does not assume the operator's Modal token has secret-write scope.
The apply response still carries a warning whenever a deployment names a token, now saying that the
secret it mounts must exist (`"... mounts the Hugging Face secret `manarouter-hf` ..."`), so the
requirement is visible at the moment of deployment rather than only in this document.

**Legacy fallback, kept deliberately.** Files written before this entity existed may hold
`RentalAccount.hf_secret_ref = Some("rental:<account>:hf")`. A gated probe with no named token falls
back to that entry, so old deployments keep fetching. Nothing writes the field any more, and the
panel labels it as legacy rather than offering to edit it.

## 4. Configuration schema change

Two new top-level keys, both `#[serde(default)]`, placed **before** `extra` in `Config`
(`huggingface_tokens` immediately after `rental_accounts`):

```json
"rental_accounts": [
  { "id": "mana-modal", "label": "Mana on Modal", "platform": "modal", "plan": "paid",
    "secret_ref": "rental:mana-modal", "max_concurrent_containers": 10, "note": null }
],
"huggingface_tokens": [
  { "id": "hf-main", "label": "Personal — read", "secret_ref": "hf:hf-main", "note": null }
]
```

Round-trip safety: every `Config` struct carries `#[serde(flatten)] extra`, so a build that does not
know `rental_accounts` preserves the key verbatim instead of dropping it. **No `SNAPSHOT_VERSION`
bump.** The snapshot does not serialise `Config`.

*Verified against source, 2026-11-12:* `Config` ends with `#[serde(flatten)] pub extra: Map<String,
Value>` (`crates/mr-core/src/config.rs:128-129`, documented as "Fields this binary does not know
about, preserved verbatim"), so the round-trip claim above is a property of code that already exists
rather than a design intent. It also means the key **works before the typed field is added** — a
conductor can read `extra["rental_accounts"]` and deserialise into `RentalAccount` while the field is
still absent from the struct. That is a useful unblocking property, not a preferred end state: the
typed field is what makes the key discoverable and type-checked, so it is specified as the destination.
`extra` must stay the last field for the flatten to keep absorbing unknown keys, so the new field goes
immediately before it and no existing field is reordered.

Vault ids use the existing `secret::` helpers' convention, and are derived from the ids the operator
chose rather than accepted from the client: the platform credential lives at `rental:<account id>`,
and a Hugging Face token at `hf:<token id>` (`hf_token_vault_id`). The old
`rental:<account id>:hf` entry is still read when a gated deployment names no token, and is never
written again. The credential is **one combined string**, not
a JSON wrapper — a `wk-...` token id and a `ws-...` token secret joined by a single `.`, the two
halves Modal's dashboard shows and the shape the existing Modal readers already split with
`split_once('.')`. The apply path is designed and checked against that format: it refuses a missing
dot, a second dot, an empty half or whitespace, because `split_once` would otherwise truncate the
secret or find none.

**A storage qualification, verified from source:** `Vault::put` takes
`&mut self` and is **not reference-counted** (`crates/mr-core/src/secret.rs:237`), and
`Account.secret_ref` is a free-form `Option<String>` with no validation against a naming scheme
(`model.rs:275`), so uniqueness of ids is the conductor's to keep rather than the compiler's. Each
account writes its own vault entry, `rental:<account-id>`, exactly once, at account creation, and no
two accounts may share an entry: writing an id twice clobbers it, and deleting it while a rental
still references it silently breaks that rental. The conductor therefore refuses a second account
with an existing id (`account_already_exists`) and refuses `create-account` when the vault entry is
already occupied. A *deployment* may reuse an account freely — that is the normal case, not a
collision.

Hugging Face tokens follow the same discipline with a different id space: `hf:<token-id>`, written
once by `create-hf-token`. Recording the same token id again is deliberately allowed and replaces
the stored value in place: that is how a token is rotated without touching a single deployment,
because deployments name the token by id rather than holding a copy. `Vault::ids()`, `contains()` and
`fingerprint_of()` (`secret.rs:356-366`) are what let the panel list accounts and tokens by
fingerprint without ever reading a secret back out.

## 5. Conductor API — `/api/gpu/*`

New module `crates/manarouter-conductor/src/gpu_api.rs`, registered exactly like `quota_api.rs`
(own file, own route group, `.merge()`d into the guarded router). Success is a bare JSON body;
failure is `{"error":{"code","message","details"}}` with **every** detail rendered by the panel.

### `GET /api/gpu/catalog`

The verified GPU table. The panel renders it; the panel never hardcodes a GPU, a price or a flag.

```json
{
  "platforms": [{
    "id": "modal",
    "label": "Modal",
    "docs_url": "https://modal.com/docs",
    "plans": [{"id":"free","label":"Free credits"},{"id":"paid","label":"Paid"}],
    "deployment": {
      "programmatic": false,
      "explain": "Modal publishes no deployment REST API; Manarouter emits a bundle and a command.",
      "cli": "modal deploy modal_app.py",
      "sdk": "modal.App / app.deploy()"
    },
    "max_concurrent_containers": {"free": 10, "source": "documented"},
    "plan_limits": [
      {"id": "free", "label": "Starter", "usd_per_month": 0,
       "included_credits_usd": 30, "gpu_concurrency": 10,
       "seats": 3, "source": "documented", "source_url": "https://modal.com/pricing"},
      {"id": "paid", "label": "Team", "usd_per_month": 250,
       "included_credits_usd": 100, "gpu_concurrency": 50, "seats": null,
       "source": "documented", "source_url": "https://modal.com/pricing"}
    ],
    "gpus": [{
      "id": "RTX-PRO-6000",
      "label": "RTX PRO 6000",
      "vram_gib": 96,
      "vram_bytes": 103079215104,
      "price_usd_per_hour": 3.0312,
      "price_usd_per_second": 0.000842,
      "free_plan": "unknown",
      "source": "documented",
      "checked_at": "2026-11-12",
      "source_url": "https://modal.com/pricing",
      "notes": "..."
    }]
  }],
  "quant_methods": [ ... see section 6 ... ],
  "kv_cache_dtypes": [ ... see section 6 ... ]
}
```

`free_plan` is one of `"available" | "unavailable" | "unknown"`. **Modal documents no per-GPU
free-tier gating** [facts], and the operator observed that H100 is unavailable on Starter
[observed]. So H100 ships as `"unavailable"` with `"source": "observed"`, and every other card ships
as `"unknown"` unless a better source exists. Rendering `(Paid)` for an `"unknown"` card would be a
fabrication, so the panel does not: it renders `(Paid)` only for `"unavailable"`, and shows the
provenance text for the rest.

**Requirement 10, checked against the primary source on 2026-11-12.** The pricing page was fetched
and parsed directly, as a second independent check on the research agent. It found no per-GPU-type
gating at all: all eleven cards carry an unconditional per-second price, and all eleven reproduce the
researched figures to the digit (RTX PRO 6000 `$0.000842/s` = $3.03/h and L40S `$0.000542/s` =
$1.95/h are the operator's own anchors from issue #4; B300 `$0.001972/s` = $7.10/h, B200
`$0.001736/s` = $6.25/h, H200 `$0.001261/s` = $4.54/h, H100 `$0.001097/s` = $3.95/h,
A100-80GB `$0.000694/s` = $2.50/h, A100-40GB `$0.000583/s` = $2.10/h, A10 `$0.000306/s` = $1.10/h,
L4 `$0.000222/s` = $0.80/h, T4 `$0.000164/s` = $0.59/h). What the free plan actually constrains is
**concurrency and credits** — Starter is `$0` + `$30` credits with **10 GPU concurrency**, Team is
`$250` + `$100` credits with **50 GPU concurrency** — which is why `plan_limits` above carries those
documented numbers and the panel renders them. The one genuinely plan-scoped exclusion is for *model
APIs*, not GPUs: "Free usage of Shared Endpoints is not part of the Starter plan."

So **requirement 10 as literally stated cannot be implemented, because the premise does not hold** —
a `(Paid)` badge would be wrong in eleven cases out of eleven. It is implemented honestly and more
usefully instead: the catalog carries the documented `plan_limits`, which is the real boundary the
operator was reaching for, plus the `free_plan` mechanism above for any card whose refusal is
actually observed. The panel therefore never shows an invented badge, and never shows `"unknown"` as
either free or paid. A test asserts exactly one card is `"unavailable"` and every other card is
`"unknown"`, so a future edit that starts guessing fails loudly instead of shipping eleven invented
flags. The shortfall against the literal requirement is recorded in §11 rather than papered over.

### `POST /api/gpu/probe`

The one call the wizard needs after the operator pastes a model. Server-side because the panel
cannot reach `huggingface.co` cross-origin and must never hold an HF token in JavaScript.

Request:

```json
{"repo": "orcarouter/Qwen3.8-27B-Uncensored-FP8", "revision": null,
 "account_id": "mana-modal", "use_hf_token": true}
```

Response:

```json
{
  "repo": "...", "revision": "main", "sha": "b32584cc1599...",
  "gated": {"state": "auto", "needs_token": true, "explain": "..."},
  "arch": {"source": "base_model",
           "repo": "Qwen/Qwen3.8-27B", "relation": "quantized", "shares_architecture": true,
           "architectures": ["Qwen3_5ForConditionalGeneration"],
           "model_type": "qwen3_5",
           "num_hidden_layers": 64, "num_attention_heads": 24,
           "num_key_value_heads": 4, "head_dim": 256, "hidden_size": 5120,
           "num_full_attention_layers": 16, "num_linear_attention_layers": 48,
           "full_attention_interval": 4, "kv_topology": "hybrid-linear",
           "max_position_embeddings": 262144,
           "torch_dtype": "bfloat16",
           "kv_bytes_per_token_elem": 1,
           "kv_geometry": {"kv_bytes_per_token": 32768, "topology": "hybrid-linear"}},
  "params": {"total": 27781427952, "source": "hf",
             "from_name": {"value": 27000000000, "token": "27B"}, "agrees_with_name": true},
  "quants": [{"id": "fp8", "label": "FP8 (block)", "in_repo": true,
              "evidence": "config.quantization_config.quant_method=fp8",
              "bytes_per_param": 1.0, "per_repo": null}],
  "sizing": {"gpu": "RTX-PRO-6000",
             "vram_bytes": 103079215104, "vram_source": "datasheet",
             "usable_vram_bytes": 102005473280,
             "kv_bytes_per_token": 32768, "kv_pool_tokens": 1784103,
             "simultaneous_sessions_at_max_len": 6, "max_num_seqs_effective": 6,
             "confidence": "analytic",
             "arch_source": "orcarouter/Qwen3.8-27B-Uncensored-FP8"},
  "warnings": ["..."], "errors": []
}
```

Ordering, as the operator specified it:

1. Nothing is fetched until `repo` is non-empty. The quant selector stays **blank** before that.
2. The **name** is inspected first (`27B` → 27e9 params) and reported as `from_name`.
3. Only then is the **repo** queried, and `params.source` says which of the two won.
4. The result is intersected with what the **selected GPU** can execute (section 6).

`quants` lists **only** what the repo can actually serve, each with its `evidence` string. A quant
that does not exist in the repo is not silently offered — it is what section 8's `requantize`
answer is for.

### `GET /api/gpu/rentals`

The inventory the panel's view renders, derived from config plus the usage store. Returns
`{"accounts": [...], "deployments": [...]}` where each account carries its live `gpu_requests`,
`gpu_seconds` and `cost_usd` from `Store::gpu_usage_totals` [implemented], and each deployment
carries its provider id, enabled state, resolved model name and derived sizing.

### `POST /api/gpu/apply`

One endpoint, three actions, because a platform credential, a Hugging Face token and a deployment
are three separate things:

```json
{"action": "create-account", "account": {"id": "mana-modal", "label": "...", "platform": "modal",
   "plan": "paid", "workspace": "mana", "token_id": "...", "token_secret": "..."}}
```

```json
{"action": "create-hf-token", "token": {"id": "hf-main", "label": "Personal — read",
   "secret": "hf_..."}}
```

```json
{"action": "create-deployment", "account_id": "mana-modal",
 "deployment": {"repo": "...", "gpu": "RTX-PRO-6000", "gpu_count": 1, "weight_quant": "fp8",
   "kv_cache_dtype": "fp8", "max_model_len": 262144, "max_num_seqs": 64,
   "shutdown_cooldown_secs": 60, "mode": "batch",
   "batch": {"size": 8, "fill": "timeout", "timeout_secs": 45}, "sizing": {...}}}
```

`create-account` carries **no** `hf_token`: a token is not account-scoped, and the action refuses
the field rather than ignoring it. `create-hf-token` writes `hf:<id>` and returns the vault entry and
a fingerprint, never the value; recording an existing id reports `"replaced": true` and rotates it.
A deployment names a token with `deployment.hf_token` (or a top-level `hf_token`), and only needs one
when the repository is gated. A named token must exist and hold a value, so that failure happens at
apply time rather than on the first fetch inside a container.

`create-deployment` requires `batch` when `mode` is `batch` and refuses it otherwise; the accepted
shape is `{"size": <u32 >= 1>, "fill": "full" | "timeout", "timeout_secs": <1..=3600>}`, and
`timeout_secs` is required exactly when `fill` is `timeout`. It returns:

```json
{"action": "create-deployment", "provider_id": "manarouter-rental-mana-modal-...",
 "account_id": "mana-modal",
 "deployment": {"account": "mana-modal", "repo": "...", "hf_token": "hf-main"},
 "vault": {"secret_id": "rental:mana-modal", "fingerprint": "..."},
 "restart_pending": false, "applies_on": "immediately", "applies_on_note": "...",
 "bundle": {"id": "...", "filename": "modal_app.py", "text": "..."},
 "command": "modal deploy modal_app.py", "warnings": []}
```

`vault.secret_id` is the **account's** entry, referenced not copied — a second deployment on the same
account reports the same id. `deployment.hf_token` is the token *id*, never its value. There is no
`provider` object in the response; `provider_id` is the thing to look up.

`base_url` may be omitted: the conductor derives
`https://<workspace>--<generated-app-name>-serve.modal.run` from the account's recorded workspace.
When present it is an explicit override for the URL `modal deploy` printed.

```json
{"action": "create-deployment", "account_id": "mana-modal",
 "deployment": {"repo": "...", "mode": "streaming",
   "base_url": "https://mana--printed-by-modal.modal.run", "sizing": {...}}}
```

`create-deployment` writes a `modal-vllm` provider with `auth: {"type":"modal"}`, the account's
credential bound as its account, the model entry, and the derived budget fields
(`kv_pool_tokens`, `max_context`, `max_concurrent`, `price_per_second`,
`cold_start_ttfb_timeout_ms`) so the relay enforces them with the KV budget already on the request
path. It also returns the **bundle** (`modal_app.py` text + command) so the wizard can show it.

The bundle is a *streaming* vLLM app unless the deployment is `batch`, in which case the file also
carries the assembler: a stdlib queue that holds each request until `BATCH_SIZE` requests are in hand
or `BATCH_TIMEOUT` expires, sends the batch to vLLM on `127.0.0.1:8001` at once, and answers each
caller with its own response (§3.2). A generated `mode_note` says which mode the deployment was
applied in and what its fill policy does, so the file is exactly as capable as it claims.

Everything goes through `AppState::commit` (validate → compile → atomic save → install) and the
vault write is rolled back if commit fails. No secret is ever echoed back; the response carries the
vault id and a fingerprint, never the value.

## 6. Hardware legality — the two catalogs

Two selectors, and neither may offer a value the hardware cannot execute.

**Weight quantisation** (`--quantization`) and **KV cache dtype** (`--kv-cache-dtype`) are separate
axes with separate legality. The catalog carries, per quant method, the minimum compute capability
and VRAM-per-parameter, and the resolver filters by the selected GPU. `docs/research/vllm-sizing-and-quant.md`
is the source of truth for the matrix; where a cell there is marked unverified, the catalog ships
`"confidence": "unverified"` and the panel shows it as such rather than implying it works.

## 7. Sizing — what is computed, and honestly, how well

**This is the part most likely to be over-claimed, so the boundary is drawn here.**

The KV bytes-per-token formula is now **verified against vLLM's own source**
(`docs/research/vllm-sizing-and-quant.md`: `vllm/config/cache.py`, `vllm/v1/kv_cache_interface.py`,
`vllm/v1/core/kv_cache_planning.py`), not guessed:

```
kv_bytes_per_token = num_hidden_layers * num_key_value_heads * head_dim * kv_dtype_bytes * 2
                     ^ the trailing 2 is K and V
```

with HuggingFace fallbacks, also verified: `num_key_value_heads` defaults to `num_attention_heads`
when absent, and `head_dim` defaults to `hidden_size / num_attention_heads` when absent. Both
defaults are real traps — Qwen2/Mistral/Llama omit `head_dim`, and Qwen3-32B declares a `head_dim`
that is *not* `hidden_size / num_attention_heads` [facts, huggingface-discovery.md].

```text
kv_budget_bytes = vram_bytes * gpu_memory_utilization - weights_bytes - overhead_bytes
kv_pool_tokens  = kv_budget_bytes / kv_bytes_per_token
```

`gpu_memory_utilization` defaults to **0.92** in vLLM, and it is a budget that covers weights,
activations *and* CUDA-graph capture — not a KV-only allowance [facts]. vLLM documents no separate
overhead percentage, so `overhead_bytes` is an **operator-visible assumption with a documented
default**, listed in `assumptions[]`, and never presented as a measured quantity.

### 7.1 The model's real architecture is now known — and it breaks the naive formula

Earlier drafts of this document said the operator's model had an "unobtainable" topology because the
quantised repo is gated. **That was wrong, and the workaround is worth knowing.** The gated repo's
model card names its base model, and the *base* model is public:

```text
GET /api/models/orcarouter/Qwen3.8-27B-Uncensored-FP8
   -> cardData.base_model = "Qwen/Qwen3.8-27B",  base_model_relation = "quantized"
GET https://huggingface.co/Qwen/Qwen3.8-27B/raw/main/config.json   -> HTTP 200
```

So a gated quantisation of a public model does **not** block KV sizing. Read `base_model` from the
card data, fetch that repo's `config.json`, and only fall back to `confidence: "unavailable"` when the
base model is itself gated or private. This is now the documented first fallback, ahead of "give up".

The base config (`Qwen/Qwen3.8-27B`, `architectures: ["Qwen3_5ForConditionalGeneration"]`,
`model_type: "qwen3_5"`) contains:

| key | value |
|---|---|
| `num_hidden_layers` | 64 |
| `layer_types` | **48 × `linear_attention`, 16 × `full_attention`** (every 4th layer is full attention; `full_attention_interval: 4`) |
| `num_attention_heads` | 24 |
| `num_key_value_heads` | 4 |
| `head_dim` | **256** (explicit) |
| `hidden_size` | 5120 |
| `max_position_embeddings` | 262144 |
| `linear_conv_kernel_dim` | 4 |
| `linear_num_key_heads` / `linear_key_head_dim` | 16 / 128 |
| `linear_num_value_heads` / `linear_value_head_dim` | 48 / 128 |
| `mamba_ssm_dtype` | `float32` |
| `tie_word_embeddings` | false |
| top level | also carries `vision_config` — this is a multimodal (image-text-to-text) model |

**This is a hybrid linear-attention model** (Gated DeltaNet-style, `model_type: "qwen3_5"`), not a
uniform transformer. Two consequences, both load-bearing:

**(a) The naive formula overestimates KV by exactly 4×.** `num_hidden_layers × num_kv_heads × head_dim
× dtype_bytes × 2` = 64 × 4 × 256 × 1 × 2 = **131 072** bytes/token. But only the **16 `full_attention`
layers** hold a per-token KV cache; the 48 `linear_attention` layers hold a **fixed-size recurrent
state** that does not grow with sequence length. The correct figure is
16 × 4 × 256 × 1 × 2 = **32 768 bytes/token** — exactly a quarter of that, and within 1.2 % of the
value the two-point fit independently produced (32 355).

**And `32 768` is no longer only an inference from the config — it is confirmed against vLLM's own
source.** The full trace is in `docs/research/hybrid-attention-kv.md`. `Attention.get_kv_cache_spec()`
returns a `FullAttentionSpec` (whose `max_memory_usage_bytes()` *grows* with `max_model_len`) for
full-attention layers, and a `MambaSpec` (whose `max_memory_usage_bytes()` is **independent of
sequence length**) for each GDN `linear_attention` layer; `kv_cache_planning.py` then sorts them into
separate cache groups, and `kv_cache_utils.py` reports capacity **group-aware**, with the source noting
that `num_gpu_blocks * block_size` "can be wrong for hybrid models". So the engine that produces the
operator's own startup line allocates a growing token cache for the 16 full-attention layers only.
The 4× error would not merely mis-shape a curve; it would contradict the engine being measured.

**(c) The fixed state is real, is charged against the budget — and is far too small to change the
answer.** This is the one place the earlier draft was too glib in the other direction. The GDN layers
are not free: their state consumes KV-cache bytes, so it does reduce the full-attention block count.
Sized from vLLM's own `gated_delta_net_state_shape()` (conv state `(128·16·2 + 128·48, 3)` = 30 720
elements, plus recurrent state `(48, 128, 128)` = 786 432 → **817 152 elements per layer**, ~1.63 MB at
bf16, ~**78.4 MB** for all 48 layers at `tp=1`; ~157 MB if the state is fp32 per `mamba_ssm_dtype`).
Against the ~8 GiB overhead that is under 1 % — it is **absorbed by `overhead_bytes`**, which is
already documented as calibrated rather than physical, and it is one more reason that constant is not
exactly physical. Two related facts the plan must carry: `--kv-cache-dtype fp8` applies to the
full-attention KV only (the GDN state dtype is a **separate** setting, `mamba_cache_dtype` /
`mamba_ssm_cache_dtype`, default `auto` → model dtype); and the operator's `mamba_ssm_dtype: "float32"`
in the HF config is **not** confirmed to map onto `mamba_ssm_cache_dtype`, so the sizing code must not
claim it knows the state dtype.

So the sizing code **must read `layer_types` and count only the `full_attention` entries** when the key
is present, falling back to `num_hidden_layers` only when it is absent. Shipping the naive form would
under-report capacity by 4× for this model, which is the difference between "6 sessions at 256 K" and
"26 sessions at 256 K". A test must pin this, because the failure is silent and large.

**(b) The `hidden_size / num_attention_heads` head_dim fallback is also wrong here** — it yields
5120 / 24 = 213.3 against the declared 256. The config declares `head_dim` explicitly, so the fallback
is not used, but this is a second trap in the same config and the code must prefer the explicit key.

### 7.2 The architecture-derived value is corroborated, and the naive value is refuted

The strongest check here is not a fit — it is a **falsification**, and it needs no fitted parameter at
all. Take the naive reading (all 64 layers hold per-token KV, `b = 131 072`) and solve for the overhead
the RTX PRO 6000 measurement would require:

```
A = 0.92 × 95 GiB − weights − b × measured_pool
  = 93 845 035 417 − 27 781 427 952 − 131 072 × 1 777 758
  = −167.0 GB
```

A **negative** overhead is physically impossible. So the measurement does not merely fail to confirm the
naive formula — it **refutes** it, and it does so by a margin (167 GB) far too large to be measurement
noise. The same arithmetic on the L40S gives **−19.3 GB**, also impossible. This is a genuine one-bit
discriminator between the two hypotheses, and it is what first proved the model is hybrid rather than
uniform.

The second check uses one fitted parameter. With `b` taken from the **published architecture**
(32 768 — independent of any measurement) and a single fitted `A`:

| card | VRAM | predicted | measured | error |
|---|---|---|---|---|
| RTX PRO 6000 | 95 GiB | 1 753 957 | 1 777 758 | **−1.34 %** |
| L40S | 46 GiB | 276 775 | 281 729 | **−1.76 %** |

and the overhead implied by each card *separately* agrees to within a quarter of the term itself:
**7.27 GiB** from the RTX PRO 6000 against **7.85 GiB** from the L40S. One parameter, two independent
observations, ~2 % residual on both. That is evidence, not a tautology — which is exactly what the
earlier two-point fit could not provide.

Its weight should not be overstated either. Two observations estimating one quantity leaves **one
degree of freedom**: the agreement bounds the overhead to roughly **7.3–7.9 GiB**, it does not
establish the model form. The part that carries real weight is the falsification above, because it
needs no fitted constant and rejects the alternative outright.

The honest reading: `overhead_bytes = 8 GiB` is a **calibrated default that sits above both fitted
values**, not a fitted fudge and still not a physical constant. The fits are 7.274 GiB and 7.849 GiB,
so 8 GiB is **0.151 GiB above the higher one** and its error is one-sided: it understates the cache
rather than promising a pool the card cannot hold. The declared width for the separation between the
two fits is `OVERHEAD_SPREAD_BYTES = 644 245 094` bytes (0.6 GiB) — a one-sided bound, not a ±
tolerance around 8 GiB. `assumptions[]` says so, and `delta_vs_measurement_pct` keeps reporting the
residual rather than being tuned away.

Cross-check against the operator's measured pair [observed] (`docs/research/issue-4-spec.md`), same
model, `--kv-cache-dtype fp8`, `--max-model-len 262144`:

| card | VRAM | measured KV pool | measured cold start |
|---|---|---|---|
| RTX PRO 6000 | 95 GiB | 1 777 758 tok | 160–181 s |
| L40S | 46 GiB | 281 729 tok | ~110 s |

Before the base-model config was found, these two points were solved as a linear system in two
unknowns (vLLM's own `gpu_memory_utilization` 0.92 applied to both cards), giving `b = 32 355`
bytes/token and `A = 7.96 GiB`. **That fit is exact by construction and therefore proves nothing** —
two equations, two unknowns, so it reproduces the L40S figure to the token because it was solved to
do so. It is retained here only because the value it produced (32 355) landing within 1.2 % of the
architecture-derived 32 768 is a useful corroboration *once the architecture is known independently*.

Two warnings survive and both are implemented:

1. **Never present the 2-point fit as a check.** Any test or UI string implying the estimation was
   "validated against measurements" by a fit that consumed those measurements is a lie about
   provenance. The non-circular check is §7.2, where `b` comes from the published config.
2. **Reject architectures that imply a negative overhead** instead of clamping to zero. This is what
   first falsified the Qwen3-32B guess (`64 L x 8 kvh x 128 x 2 = 131 072` forces `A = -155 GiB`), and
   it is exactly the rule that would have caught the eventual error had it gone the other way: the
   same falsification test applied to a *plausible-looking* 64-layer uniform transformer is what
   proves this model is hybrid rather than uniform.

So `overhead_bytes` is an **operator-visible assumption with a calibrated default that sits 0.151 GiB
above the higher of the two fits** (see §7.2):

1. The absolute number cannot be reproduced from configuration alone, even now that the architecture
   is known. vLLM's reported figure absorbs block padding, `--max-num-batched-tokens`, CUDA-graph
   memory and the fixed linear-attention state, none of which the per-token formula models — which is
   why §7.2 needs a fitted overhead term at all, and why the residual is ~2 % rather than ~0 %.
2. A ~8 GiB overhead term fitted to two data points is not a physical constant, and shipping it
   as one would be a fabrication. It is evidenced enough to be a *default* and not a *truth*. So the
   code does **not** tune it silently to match a measurement.

Instead, `sizing` carries a **calibration table of measured points** — the operator's own
instrument, seeded with the pair above — and reports the analytic number and the measured number
side by side rather than silently preferring either.

**When the architecture facts are unavailable for a gated repo with no token, `sizing` reports
`confidence: "unavailable"` with the reason. It does not fabricate a KV number.** The operator can
enter a measured one, and requirement 1 is still satisfied: the number is computed whenever computing
it is possible, and the reason is stated when it is not.

The sizing result therefore carries its own honesty fields and the UI renders them:

```json
"sizing": {
  "confidence": "analytic",
  "arch_source": "orcarouter/Qwen3.8-27B-Uncensored-FP8",
  "kv_bytes_per_token": 32768,
  "kv_bytes_per_token_basis": "full_attention_layers=16 of 64 (hybrid linear attention; the other 48 hold a fixed-size recurrent state and no per-token KV) x num_kv_heads=4 x head_dim=256 x dtype_bytes=1; x2 for K and V",
  "weights_bytes": 27781427952,
  "gpu_memory_utilization": 0.92,
  "overhead_bytes": 8589934592,
  "kv_budget_bytes": 58461515351,
  "kv_pool_tokens": 1784103,
  "kv_pool_tokens_aligned": 1784064,
  "measured_reference": {"kv_pool_tokens": 1777758, "card": "RTX-PRO-6000",
                         "source": "operator measurement, issue #4",
                         "measured_at": "2026-11-12"},
  "delta_vs_measurement_pct": 0.36,
  "simultaneous_sessions_at": [{"context_len": 2048, "blocks_per_session": 32,
                                "bytes_per_session": 67108864, "sessions": 871},
                               {"context_len": 8192, "blocks_per_session": 128,
                                "bytes_per_session": 268435456, "sessions": 217},
                               {"context_len": 32768, "blocks_per_session": 512,
                                "bytes_per_session": 1073741824, "sessions": 54},
                               {"context_len": 262144, "blocks_per_session": 4096,
                                "bytes_per_session": 8589934592, "sessions": 6}],
  "simultaneous_sessions_at_max_len": 6,
  "max_num_seqs_effective": 6,
  "assumptions": ["gpu_memory_utilization 0.92 (vLLM default)",
                  "overhead_bytes 8 GiB for activations, CUDA-graph capture, block padding and the fixed linear-attention state, excluding weights; vLLM documents no separate figure. The operator's two measurements imply 7.27 GiB (RTX) and 7.85 GiB (L40S) independently, so this default is calibrated, not physical.",
                  "only full_attention layers are counted; layer_types came from the public base model Qwen/Qwen3.8-27B because the gated quantised repo hides head topology",
                  "every counted layer is assumed to cache per token; a sliding-window layer would cache less and this is therefore an overestimate",
                  "max_model_len is the per-session context used for the session count",
                  "the candidate architecture is rejected if it implies a negative overhead"],
  "warnings": ["vLLM's own startup line (`GPU KV cache size: N tokens`) is authoritative; this is an estimate.",
               "Only the RTX PRO 6000 and L40S have been measured for this model; other cards are extrapolated and their VRAM is a datasheet value, not usable capacity.",
               "A 0.36 % agreement would NOT be independent confirmation: overhead_bytes is itself calibrated against those measurements. The honest residual with the architecture-derived kv_bytes_per_token is -1.34 % (RTX PRO 6000) and -1.76 % (L40S)."]
}
```

The example is internally consistent: with `vram_bytes` = 96 GiB, `weights_bytes` = 27 781 427 952,
`b` = 32 768 and 8 GiB overhead, the budget is 58 461 515 351 bytes and the pool is 1 784 103 tokens —
**+0.36 %** from the operator's 1 777 758. The earlier version of this block quoted a budget and pool
that did not follow from its own stated inputs; the numbers above were recomputed and do.

`max_num_seqs_effective` is the concurrency at the full `max_model_len` — here **6 at 262 144
tokens** — capped by the operator's `max_num_seqs`. The 54 in the ladder is the **32 768-token
rung**: the same pool read at a shorter context, not a session count at the full 256 K.

`confidence` is one of `"analytic"` (computed from real architecture facts), `"calibrated"` (derived
by inverting an operator measurement of the same model), or `"unavailable"` (the architecture could
not be read — a gated repo with no token — and no measurement exists; `kv_pool_tokens` is then
`null` and the reason is in `warnings`).

The derived `kv_pool_tokens` is written into the provider as a **default the operator can override**,
because vLLM's reported number is authoritative and it is not available until the container has
booted. The panel must say that, and must show `measured_reference` beside the estimate whenever the
same card and model have been measured before.

`delta_vs_measurement_pct` is a required field. Shipping an estimate with no stated error is the
failure mode this section exists to prevent.

## 8. Requirement 6 — a quant that does not exist in the repo

Requesting a quant the repo cannot serve is not a dead end, and not a silent downgrade. `probe`
answers with an error **and** a plan. Every number below was corrected against primary sources in
[requantize-recipes.md](requantize-recipes.md); the first draft of this section was wrong in three
fields, in two opposite directions, and the false version is called out explicitly.

```json
"requantize": {
  "requested": "awq",
  "unavailable_reason": "the repository ships FP8 only; no AWQ sibling and no AWQ tensors",
  "source": {"id": "bf16", "label": "BF16 (base model)", "bytes_per_param": 2.0},
  "target": {"id": "awq", "label": "AWQ 4-bit", "bytes_per_param": 0.5,
             "produced_quant_method": "compressed-tensors"},
  "path": "dense",                     // "dense" = verified recipe; "model-free" = one-pass RTN
  "feasible_on_gpu": true,
  "failed_on": null,                   // "vram" | "disk" | "host_ram" | null
  "requires_vram_bytes": 12884901888,  // ~12 GiB: mem(1 layer) x2 for GPTQ
  "requires_extra_vram_bytes": 0,
  "requires_host_ram_bytes": 69000000000,
  "requires_disk_bytes": 100000000000,
  "estimated_minutes": 480,
  "gpu_hours_at_k": [{"k_gpus": 1, "speedup": 1.0, "wall_clock_hours": 8.0, "gpu_hours": 8.0,
                      "cost_usd": 15.61, "speedup_source": "observed"},
                     {"k_gpus": 4, "speedup": 3.8, "wall_clock_hours": 2.11, "gpu_hours": 8.42,
                      "cost_usd": 16.43, "speedup_source": "observed"}],
  "tool": "llm-compressor 0.15.0",
  "steps": ["pip install llm-compressor", "write quantize.py (see recipe)", "python quantize.py", "..."],
  "serve_command": "vllm serve <output-dir> --max-model-len 262144 --kv-cache-dtype fp8",
  "note": "This is a real offline job on a real GPU. Manarouter records the recipe and the command; it does not run it for you."
}
```

### 8.1 Three corrections to the first draft, and why they went the wrong way

| Field | First draft | Corrected | Why |
|---|---|---|---|
| `requires_vram_bytes` | 42 GB | **~12 GiB** | llm-compressor keeps the model in **host RAM** and onloads **one layer at a time**: `mem(1 layer)` for RTN, `×2` for GPTQ. 27 781 427 952 params / 64 layers = 0.87 GB/layer. |
| `requires_extra_vram_bytes` | 14 GB | **0** | *"LLM Compressor does not currently support tensor parallelism for compression."* More GPUs buy **wall-clock**, never VRAM headroom. A non-zero value here would name a remedy that does not exist. |
| `estimated_minutes` | 90 | **~480** | Measured anchor: GPTQ on Qwen3-30B-A3B = 3.9 h on 1 GPU. 27B AWQ+GPTQ reasoned to 450–515 min, i.e. **$15–26**, paid before the first token. |

And the prose conclusion drawn from those wrong numbers was **also** wrong. The first draft said
requantising a 27B model "does not fit on the card you were trying to squeeze it into". On both
catalogue cards that is **false** — `feasible_on_gpu` is `true` on the L40S (46 GiB) and the RTX PRO
6000 (95 GiB). The real gates are **host RAM (~69 GB), scratch disk (~100 GB), wall clock (~8 h) and
quality**, and those are what the plan reports and the panel must show. `requires_extra_vram_bytes: 0`
plus a non-null `failed_on` is how a `false` stays actionable: it says *which* resource ran out,
rather than implying another GPU would fix it.

One narrowly-true version survives: a *badly written* recipe — the `pipeline="basic"` regime, which
holds the FP16 upcast resident (`params × 2 × 2` = **111 GB**) — OOMs both cards. So the emitted recipe
must carry its offload settings explicitly. `failed_on: "vram"` is the honest answer for that recipe,
not for this model.

### 8.2 Two paths, and they are not equivalent — label them by quality, not by speed

`FP8 → AWQ/GPTQ` **cannot be done in one arithmetic step**; llm-compressor's own converter doc calls it
"requiring a round-trip through dense weights first". There *is* an officially demonstrated one-pass
route, `model_free_ptq(model_stub=<FP8 repo>, scheme="W4A16", converter=FP8BlockDequantizer(...))`,
which chains dequant→requant inside one call "without writing an intermediate checkpoint to disk" —
**20–45 min and ~6 GiB VRAM instead of ~8 h and ~100 GB of disk.** But it produces **RTN, not AWQ**:
`model_free_ptq` provably refuses activation calibration ("Model Free PTQ cannot calibrate activations.
Please use `oneshot` instead").

So the plan carries a `path` field and the panel offers **two choices labelled by quality**
("higher quality, ~8 h" vs "faster, lower quality"), never one button that silently picks RTN. Treat
`path: "model-free"` as *attempt and report failure* — it is proven on a different model
(MiniMax-M2.7) and is **not** confirmed to survive this architecture's vision tower and `linear_attn`
layers.

### 8.3 The default recommendation is to start from the base model, not the FP8 repo

This is the best outcome of the research and it inverts the obvious choice. The FP8 repo
`orcarouter/Qwen3.8-27B-Uncensored-FP8` is `gated: "auto"` (token plus gate acceptance) and is itself a
lossy FP8 quantisation. Its declared base `Qwen/Qwen3.8-27B` is **ungated, Apache-2.0** and public.
Quantising **from the BF16 base** is therefore better on quality (no lossy FP8 round-trip) *and* easier
operationally (no credential, no gate). The only advantage of the FP8 route is half the download.

The plan should recommend the base by default, and offer the FP8 route as the fallback for an operator
who cannot pull ~56 GB.

### 8.4 The trap that would ship a deployment serving zero tokens

`target.id` is `"awq"`, but the artefact llm-compressor produces declares
`quant_method: "compressed-tensors"`, `format: "pack-quantized"` — **not** AutoAWQ's
`{"quant_method":"awq","bits":4,"group_size":128}`. And `_verify_quantization` in `vllm/config/model.py`
**raises `ValueError`** when `--quantization` disagrees with the checkpoint; it does not coerce.

Therefore: **the generated serve command must carry no `--quantization` flag derived from `target.id`.**
Plain `vllm serve <dir>` and let auto-detection read the artefact — which is exactly what the official
deploy guide does. Emitting `--quantization awq` against a compressed-tensors artefact is a paid boot
that serves nothing. A test must assert the absence of that flag, and `target.produced_quant_method`
exists so the panel can state what was actually made rather than what was asked for.

Two related traps:
- **AutoAWQ is deprecated** and must not be named as the tool ("AutoAWQ is officially deprecated and
  will no longer be maintained"; vLLM: "This functionality has been adopted by the vLLM project in
  llm-compressor"). There is **no `llmcompressor` CLI** — it registers only `llmcompressor.trace` — so
  `steps[]` cannot be a one-liner: it is `pip install`, write a Python file, run it.
- **The vision tower.** The official 27B example states the model "is a vision-language model, so it
  must be loaded with its `Qwen3_5ForConditionalGeneration` class to keep the vision tower". Loading
  through a generic `AutoModel*` silently drops it. The tower loads whole onto the GPU unquantised.
- **`FP8BlockDequantizer`, not `CompressedTensorsDequantizer`** — native vLLM FP8 uses
  `weight_scale_inv` + `weight_block_size`, not compressed-tensors' `weight_scale`. Verified against
  `deepseek-ai/DeepSeek-V3.2/config.json`, the repo the official example dequantises.

### 8.5 A chosen constant, stated as one

Feasibility uses `k_gpu = 4` on the published `mem(1 layer)` figure (1.0 published for RTN, 2.0 for
GPTQ) to leave margin for calibration activations and batch. **This is a chosen constant with no
published equivalent** — the same category as §7's `overhead_bytes` — and the plan output names it as
an assumption rather than presenting it as documentation.

### 8.6 `vLLM` online quantisation is not a requantisation mechanism

`--quantization fp8_per_block` accepts BF16/FP16 input only. It is the shortcut a reviewer reaches for
when looking at an FP8 source and a 4-bit target, so it deserves an explicit negative note in the plan:
it cannot convert FP8 to anything.

### 8.7 What Manarouter does with all this

It records the recipe in `Provider.extra.rental.requantize` and generates the commands. **It does not
run the quantisation, and it does not claim to have run it.** Only `path: "dense"` is validated by an
official example for this exact architecture; `path: "model-free"` is proven on a different model; the
layerwise no-disk path needs the real decoder-layer class name and could not be confirmed with the
vision tower present, so it is **not shipped**.

## 9. Requirement 9 — one card, one job: streaming XOR batch

A rental is one model on one card. `mode` says what that card is doing, and it is a single-choice
axis: `streaming` or `batch`, never both. `streaming` runs a live endpoint a request can reach and
is latency-shaped (`max_num_seqs` small, `shutdown_cooldown_secs` short,
`cold_start_ttfb_timeout_ms` long enough to survive a cold boot — the observed boots are 110–181 s,
so the existing default is checked against that and raised if it is shorter).

`batch` accumulates requests on the *same* card and flushes when the batch is worth a cold start.
The issue's own arithmetic still explains why the card choice matters to the planner [observed]: a
boot costs 180 s at $3.03/h = **$0.1516** on the 6000 and 110 s at $1.95/h = **$0.0596** on the
L40S, and the more expensive card is **40 % cheaper per token** on decode-bound batches ($1.26/M vs
$2.08/M at 2K/64). It does **not** justify a second card: a batch lane on `batch_gpu` was the defect.
The scheduler's job is to hold a batch that is too small to pay for its own boot, and the planner's
job is to compare cards by workload cost rather than by hourly rate alone.

Both lanes are the same provider shape with different budget fields; the mode is recorded in
`extra.rental.mode` and is what the panel renders. There is no `batch_gpu`, and no path produces a
deployment that is both streaming and batch.

## 10. Panel work

Route `#/gpu-rentals`, nav label **GPU rentals**; setup lives on the sub-path
`#/gpu-rentals/new/<step>`, so the operator crosses between planning and renting without leaving
the route.

Two modes in one file, with a pure model beside it:

* `panel/view-gpu-rentals.js` — **both modes behind one route**: the inventory at `#/gpu-rentals`
  (accounts and deployments as dense tables, live GPU-seconds and dollars, plan and provenance,
  per-deployment derived sizing, the deployment bundle with a copy button, `GET /api/gpu/rentals`
  and `GET /api/gpu/catalog`) and the setup flow at `#/gpu-rentals/new/<step>`: platform → account
  (separate step, requirement 8) → model → GPU → quant → sizing → bundle. They read the same two
  endpoints and render the same catalog, so splitting them would duplicate both. Calls
  `POST /api/gpu/probe` and `POST /api/gpu/apply`.
* `panel/gpu-setup-model.js` — the setup **logic**, as pure functions: step order, validation,
  probe application, sizing arithmetic and the generated `modal_app.py` text. The view renders what
  these return and posts what the operator asks for; it does not re-implement them.

Design constraints are not stylistic preferences here, they are acceptance criteria:
`docs/15-web-panel.md` and `panel/README.md` as summarised in `docs/research/panel-conventions.md`.
`dom.js` `h()` only; `innerHTML` nowhere; server strings as text nodes; every `error.details` entry
rendered; validation problems mapped to fields by leaf name; unsaved-edit guards on the wizard;
`ctx.confirm({...})` for anything destructive; polling stops when the tab is hidden.

The tests are behavioural rather than source-string greps: `panel/test/gpu-rental-view.test.mjs`
drives the view through an element stub, while `panel/test/gpu-setup-model.test.mjs` and
`panel/test/deploy-bundle.test.mjs` pin the pure arithmetic and the generated `modal_app.py` flags.
A test that greps for `scaledownWindowSec` proves nothing about the feature.

## 11. What is explicitly out of scope

* No invented Modal REST call. Ever.
* No `SNAPSHOT_VERSION` bump: nothing here needs the relay to read a new field.
* No live `modal deploy` unless the operator opts in and the CLI exists.
* No claim that the analytic KV estimate equals vLLM's measured number.
* No `(Paid)` marker on a card whose free-tier availability is unknown.
* Requantisation is *planned and recorded*, not executed.

### 11.1 Known shortfalls against the operator's stated requirements

Recorded here deliberately, so the gap is visible rather than silently absorbed.

* **Requirement 10 is not implementable as stated.** The operator asked for a `(Paid)` marker on the
  GPUs that do not work in the free plan. Modal documents no per-GPU-type gating, and the pricing
  page re-parsed on 2026-11-12 confirms it: all eleven cards carry an unconditional per-second price.
  The real free-plan boundary is concurrency (10 GPUs) and credits ($30/month), both of which ship as
  `plan_limits` and are rendered. See §5. The literal request is therefore declined with evidence,
  not implemented as invented flags.
* **Requirement 1's "automatically calculated" KV size cannot be exact.** vLLM's own startup line
  (`GPU KV cache size: N tokens`) is authoritative and is unavailable until the container has booted.
  The automatic figure is an estimate with its formula, assumptions and delta-vs-measurement shown;
  for a gated repo with no token it is reported as `unavailable` with the reason rather than guessed.
  See §7.
* **Requirement 6's requantisation is a recipe, not an action.** Manarouter records the recipe and
  shows the command; it does not run a multi-hour GPU job. See §8.
* **Issues #10, #12 and #13 are not addressed here, and their lanes were removed on 2026-11-13.**
  The seam (`RentalPlatform`) is built so others can be added, but only Modal and Kaggle remain.

### 11.2 Open questions the requantisation recipes could not close

`requantize-recipes.md` §10 records eight things that could not be verified from source or docs. They
are repeated here because §8 of this document depends on them, and a reader of the architecture should
not have to discover the recipe document to learn where its certainty ends. None of these are bugs;
each is a place where the honest answer is "read it at run time" or "attempt and report failure".

1. **`model_free_ptq` + `FP8BlockDequantizer` on `Qwen3_5ForConditionalGeneration` is unproven.** The
   only precedent example is a different architecture (MiniMax-M2.7). The vision tower and the
   `linear_attn` layers have to be excluded from the scheme targets, and nothing in source or docs
   confirms the fused and linear-attention modules survive the round trip. This is why the
   model-free path is specified as *attempt, then report the failure* rather than as a guaranteed
   route, and why it produces RTN rather than AWQ.
2. **No measured wall clock for a 27B dense W4A16 AWQ+GPTQ run exists.** The 450–515 minute figure is
   extrapolated from a 30B measurement whose two official sources disagree by roughly 2× on how cost
   scales from 8B to 30B. The range could be wrong in either direction, so the plan's
   `estimated_minutes` is an estimate carrying its own spread, not a schedule.
3. **The vision tower's size is reasoned, not measured.** The memory model loads it whole onto the GPU
   and does not quantise it, but the gated checkpoint's tensor index cannot be read to weigh it. The
   1–4 GB range comes from the Qwen2.5-VL-7B row (1.3 GB) and is marked as reasoned.
4. **`Qwen3_5DecoderLayer` is not confirmed as a usable `sequential_target`** for the layerwise path
   with the vision tower present, and the real class name under `transformers>=5.15` is unknown. That
   is why path C is not shipped.
5. **The tokenizer and preprocessor sidecar files are not enumerated.** Whether `save_compressed=True`
   with `processor=processor` writes everything a re-served `Qwen3_5ForConditionalGeneration` needs is
   unknown, so the plan tells the operator to verify the output directory before serving it.
6. **The source repository's actual `weight_block_size` is unread.** The repository is gated and its
   full `quantization_config` could not be fetched. `(128, 128)` is the common default and matches
   DeepSeek-V3.2, but the recipe must read the value rather than assume it.
7. **Whether `requires_extra_vram_bytes` is ever non-zero remains a judgement.**
   `requantize.rs` ships `REQUIRES_EXTRA_VRAM_BYTES = 0`, on the reading that a compression job cannot
   use tensor parallelism (LLM Compressor does not support it for compression) and therefore runs
   inside the serving card's capacity. That is a reading, not a measurement.
8. **No legal conclusion is drawn about redistributing quantised derivatives of Llama-licensed
   models.** §7 reports what the licences say — Apache-2.0 requires a prominent notice that the files
   were changed, and `meta-llama/Llama-3.3-70B-Instruct` is `gated: "manual"` under `license: llama3.3`
   whose terms propagate. Whether serving a derivative over an API constitutes distribution is not
   something this document can settle, and it does not try.

### 11.3 Corrections made after implementation

The implementation pass found nine places where the contract above and the shipped halves disagreed.
Each is recorded with the corrected behaviour, because the version that ships is not the version this
document first described.

* **The generated bundle double-quoted Python literals.** A pre-quoted, pre-escaped `py_str(..)` was
  interpolated inside another pair of quotes, producing `""fp8""`. The file was never executed, so a
  green unit suite could not catch it; the generator now quotes each argument exactly once and an
  `ast.parse` check is part of verification.
* **The probe read the wrong config URL.** `{repo}/resolve/main/config.json` answers **307** to a
  cache host, and the quota client does not follow redirects, so the gated-repo `base_model` fallback
  could never run. `/raw/{rev}/config.json` answers 200 directly, which made
  `arch.source: "base_model"` reachable for the first time. `curl -L` would have hidden the defect.
* **The panel sent a display-shaped session ladder.** Rows were `{context, sessions}`; the conductor
  re-parses `sizing` into `SizingResult`, whose `SessionsAtContext` requires non-`Option`
  `context_len`, `blocks_per_session` and `bytes_per_session`, and answered HTTP 400. Rows now use
  the server's field names, with `context` derived for display only.
* **A correctly sized deployment reported a null fit.** `fit_of` reads `kv_pool_tokens_aligned` and
  `simultaneous_sessions_at_max_len`; the panel computed the pool but did not forward those derived
  fields, so `GET /api/gpu/rentals` returned
  `fit: {known: true, kv_pool_tokens: null, sessions_at_max_len: null}`. The panel now forwards the
  derived figures, and the conductor can divide the raw pool when only that arrives.
* **`POST /api/gpu/apply` is create-only.** Re-applying a deployment with the same
  (account, repository, mode, card, count) returns HTTP **409** (`a provider with id ... already
  exists`) rather than mutating or duplicating it. The same repository on a different card, in the
  other mode, on another account or with a different card count is a *different* deployment and is
  accepted — that is how an operator runs a streaming endpoint and a filling batch of one model at
  once. The deployment references the account's existing vault entry rather than copying the
  credential, so many deployments share one account without sharing a billing identity.
* **`command` is not inside `bundle`.** The apply response returns `command` as a top-level sibling
  of `bundle`; a consumer reading `bundle.command` gets nothing.
* **Rental facts are a top-level `rental` object.** The generated provider inserts them into
  `Provider::extra`, which is `#[serde(flatten)]`, so the wire object has `rental` at the top level,
  not `extra.rental`. A client that looks under `extra` finds nothing.
* **`plan` is a billing class, not a tier name.** It is `free`, `paid` or `unknown`; absent means
  `unknown`, never `free`. `plan_limits` is optional on create, and when it is not posted the stored
  account has no such key and `GET /api/gpu/rentals` reports `plan_limits: null` — which is not
  evidence that the account is free. The catalog's plan-limit rows carry `id` `free`/`paid` with
  labels `Starter`/`Team`.
* **The batch lane does not predict tokens.** It reports `boot_share_pct` and `cold_start_verdict`
  (`boot_dominates` above half the bill, `work_dominates` below, `unknown` when there is no price to
  divide), both derived from measured dollars rather than an invented throughput figure.
