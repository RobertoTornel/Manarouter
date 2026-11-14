# Modal platform research

Fact-checking notes for the in-progress `modal-vllm` provider
(`docs/providers/modal-vllm.md`, `crates/mr-core/src/quota/modal.rs`).
All facts below were checked against Modal's official docs and pricing pages;
retrieval date: **2026-11-12**. Nothing was guessed; anything not found on an
official page is marked UNVERIFIED.

## Verified facts

**Q1 — GPU types (canonical SDK/CLI `gpu=` id strings)**
Source: https://modal.com/docs/guide/gpu.md — Confidence: high
Canonical values accepted by the `gpu` argument of `@app.function()` /
`@app.cls()` / `@app.server()`:
`T4`, `L4`, `A10`, `L40S`, `A100`, `A100-40GB`, `A100-80GB`, `RTX-PRO-6000`,
`H100`, `H200`, `B200`, `B300`.
Modifier suffixes documented on top of base ids: `H100!` (opt out of Modal's
automatic H100→H200 upgrade; cost unchanged), `B200+` (run on B200 or B300;
billed as B200). GPU count via `:n`, e.g. `H100:8`; up to 8 GPUs per
container for B300/B200/H200/H100/A100/L4/T4/L40S, up to 4 for A10.
No GPU type is documented as "preview"/limited; B300 requires CUDA 13.1+.
"A10G" appears in Modal's own blog (2025-08-18) and one pricing FAQ example,
but is **not** in the current canonical list — the current id is `A10`; formal
deprecation of "A10G" is not documented (see Unverified).

**Q2 — Per-second GPU/CPU/memory pricing (as published)**
Source: https://modal.com/pricing — Confidence: high (per-second rates as
published; per-hour column computed ×3600 and marked as such)

| GPU type (id) | USD / second | USD / hour (computed) |
|---|---|---|
| B300 | 0.001972 | 7.0992 |
| B200 | 0.001736 | 6.2496 |
| H200 SXM | 0.001261 | 4.5396 |
| H100 SXM5 | 0.001097 | 3.9492 |
| RTX PRO 6000 | 0.000842 | 3.0312 |
| A100, 80 GB | 0.000694 | 2.4984 |
| A100, 40 GB | 0.000583 | 2.0988 |
| L40S | 0.000542 | 1.9512 |
| A10 | 0.000306 | 1.1016 |
| L4 | 0.000222 | 0.7992 |
| T4 | 0.000164 | 0.5904 |

CPU: $0.0000131 per core/sec ($0.04716/core/hr computed); minimum 0.125
cores per container; 1 physical core = 2 vCPU.
Memory: $0.00000222 per GiB/sec ($0.007992/GiB/hr computed); minimum 128 MiB
per container.
Volumes: $0.09/GiB/month, first 1 TiB/month free. Network egress: $0.04/GiB.
Billing is per-second with no minimum usage-time increments
(https://modal.com/docs/guide/billing.md).

**Q3 — Free / Starter plan credits and GPU availability**
Source: https://modal.com/pricing (Starter plan row) — Confidence: high
Starter: $0/month + compute, **$30/month free compute credits**, 3 workspace
seats, 100 containers + 10 GPU concurrency, Scheduled and Web Functions
(limited), region selection, 1 TiB/month free egress. Team: $250/month +
$100/month free credits. Enterprise: custom.
Source: https://modal.com/docs/guide/billing.md — Confidence: high
"You must have a payment method on file in order to use Modal."
Source: https://modal.com/docs/guide/gpu.md — Confidence: high
"Using a GPU requires having a valid payment method on file."
GPU types NOT available on the free plan: **not documented**. The docs do not
draw a per-GPU-type free vs paid distinction; they state a platform-wide
payment-method requirement and list 10 GPU concurrency as included on Starter.
Any claim that specific GPU types are paywall-only is unsupported.

**Q4 — GPU regions/availability**
Source: https://modal.com/docs/guide/region-selection.md — Confidence: high
No per-GPU-region restriction is documented. `region=` is a per-function /
per-sandbox setting (broad regions 1.15x, narrow regions 1.75x multiplier on
base pricing). Routing region defaults to `us-east`. GPU availability "can
fluctuate, especially for tightly-constrained requests" (e.g. 8 co-located GPUs
in a specific region) — https://modal.com/docs/examples/gpu_fallbacks.md.

**Q5 — Proxy Tokens**
Source: https://modal.com/docs/guide/webhook-proxy-auth.md — Confidence: high
Header names: `Modal-Key` (token id) + `Modal-Secret` (token secret), or a
single `Authorization: Bearer <token_id>.<token_secret>`. Proxy-token id/secret
prefixes: `wk-` / `ws-` (docs example: `wk-1234abcd` / `ws-1234abcd`).
Tokens created in Dashboard settings or `modal workspace proxy-tokens create`.
Endpoint protection: Endpoints and Servers require authentication by default
(`--unauthenticated` / `unauthenticated=True` to allow public traffic); Web
Functions are public by default and are protected with
`requires_proxy_auth=True` on `fastapi_endpoint`/`asgi_app`/`wsgi_app`/
`web_server`. Workspace managers can block unauthenticated URLs entirely.
Correlation with project doc: `docs/providers/modal-vllm.md` §Auth matches
this (combined `TOKEN_ID.TOKEN_SECRET` form, `Modal-Key`/`Modal-Secret`
headers); the linked URL https://modal.com/docs/guide/webhook-proxy-auth.md is
valid and consistent.
API-token prefixes (separate from proxy tokens): `~/.modal.toml` example uses
`ak-` / `as-` for `token_id`/`token_secret` set via `modal token set`.

**Q6 — Current (2025/2026) deployment API and renames**
Source: https://modal.com/docs/sdk/py/latest/App — Confidence: high
- `modal.App` — current constructor; `App("name")`, `App.lookup(name,
  create_if_missing=True)`.
- `@app.function(image, secrets, gpu, timeout=300, startup_timeout,
  scaledown_window, min_containers, max_containers, buffer_containers, region,
  cloud, nonpreemptible, single_use_containers, ...)` — current decorator.
- `app.deploy(name, environment_name, tag, strategy="rolling"|"recreate")` —
  documented as "a programmatic alternative to the `modal deploy` CLI command".
  No top-level `modal.deploy()` function exists; `modal deploy` is the CLI.
- `@app.server(port=8000, unauthenticated=False, startup_timeout=30, ...)` —
  current primitive for long-lived HTTP servers (replaces ad-hoc web-serving
  patterns); `web_server` decorator (`@app.function()` + `@modal.web_server(8000)`)
  also exists for non-ASGI servers.
- `secrets=[modal.Secret.from_name(...)]` — current `secrets` parameter on
  `@app.function` / `@app.cls` / `@app.server`.
- `allow_concurrent_inputs` — **removed/replaced**: superseded by
  `@modal.concurrent(max_inputs=, target_inputs=)` (added in modal v0.73.148;
  https://modal.com/docs/sdk/py/latest/concurrent).
- `max_inputs` — **deprecated**, replaced by `single_use_containers`.
- `container_idle_timeout` — **not found** in current docs; the documented
  idle-scale-down parameter is `scaledown_window`.
- `--detach`: exists on `modal run -d/--detach` ("Don't stop the app if the
  local process dies or disconnects") and on `App.run(detach=...)`; it is **not**
  a flag of `modal deploy` (deploy flags: `--name`, `-e/--env`, `--stream-logs`,
  `--tag`, `-m`, `--timestamps`, `--strategy`).
- Autoscaling can be updated without redeploy via
  `Function.autoscaler.update(min_containers/max_containers/buffer_containers)`
  (reverts to decorator config on next deploy)
  https://modal.com/docs/guide/scale.md.

**Q7 — Public REST API for app deployment**
Source: https://modal.com/docs/sdk/py/latest/App (`app.deploy` description) —
Confidence: high
The documented ways to create/update a deployment are the `modal deploy` CLI
and the `App.deploy()` Python method ("a programmatic alternative to the
modal deploy CLI command"). No official public REST API for creating/updating
app deployments is documented. Third-party API mirrors exist
(e.g. modal.rest, an api-evangelist OpenAPI mirror on GitHub) but are not
Modal's official documentation. Token needed for CLI/SDK: Modal API tokens
(`MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET`); endpoint calls use Proxy Tokens.

**Q8 — `modal deploy` authentication**
Source: https://modal.com/docs/reference/modal.config — Confidence: high
Env vars: `MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET`. File: `~/.modal.toml`
(overridable via `MODAL_CONFIG_PATH`), with profile sections, e.g.
`[ default ] token_id = "..." token_secret = "..."`. CLI: `modal token set
--token-id <id> --token-secret <secret>` (value `-` reads from stdin).
Profiles: `MODAL_PROFILE` env var (default `"default"`), `modal profile`
command, `config_profiles()` / `config_set_active_profile()` in Python.
OAuth integrations inject `MODAL_OAUTH_REFRESH_TOKEN`,
`MODAL_OAUTH_CLIENT_ID`, `MODAL_OAUTH_CLIENT_SECRET` or `MODAL_OAUTH_JWT_KEY`.

**Q9 — Cold start / scale-to-zero semantics for web endpoints**
Source: https://modal.com/docs/guide/cold-start.md — Confidence: high
Functions scale to zero by default; when no warm container exists, new inputs
**queue** for a container to boot; a container is not considered warm until all
`modal.enter` hooks complete, and no inputs are routed to a not-yet-ready
container (https://modal.com/docs/guide/scale.md; LLM guide
https://modal.com/docs/guide/high-performance-llm-inference.md).
Modal docs **do not document an explicit 503** on cold start for web
endpoints/functions; they describe queueing. Whether the Modal proxy returns
503 or holds the HTTP connection during scale-up is not stated in the docs
(see Unverified).

**Q10 — `scaledown_window` default and limits**
Source: https://modal.com/docs/guide/cold-start.md — Confidence: high
Default: **60 seconds** (maximum idle time before a container shuts down).
Allowed range: **2 seconds to 20 minutes (1200 seconds)**. Meaning: max time
a container may remain idle while scaling down (per `@app.function` /
`@app.cls` / `@app.server`). The autoscaler may scale down sooner when the
function is substantially over-provisioned.

## Corrections to existing project assumptions

1. `docs/providers/modal-vllm.md` §"Cold starts and fallback" assumes a **503**
   from a scaled-to-zero Modal deployment, with the alternative being a
   deployment that "holds the connection open". Modal's docs document **queueing
   during cold start**, not an explicit 503; neither behavior (503 vs held
   connection) is confirmed by official docs. Treat the project's 503 handling
   as an assumption to verify against the real deployment, not as documented
   Modal behavior.
2. `docs/providers/modal-vllm.md` §Auth is **consistent** with current docs:
   combined `TOKEN_ID.TOKEN_SECRET`, headers `Modal-Key`/`Modal-Secret`, linked
   proxy-auth guide URL valid. Proxy-token prefixes are `wk-`/`ws-`; note that
   API tokens (`.modal.toml`, `modal token set`) use `ak-`/`as-` prefixes in
   Modal's own example — do not conflate the two.
3. GPU id `A10G` (used in the task's example list and possibly in hardcoded
   project values) is **not** a current canonical Modal id; the current id is
   `A10`. "A10G" survives in Modal's blog/FAQ examples; formal deprecation is
   undocumented (Unverified).
4. `crates/mr-core/src/quota/modal.rs`: contains no factual claims about Modal
   (it deliberately uses an operator-declared quota endpoint and fabricates no
   Modal URL). No correction needed.

## Unverified / open

- Whether any specific GPU type is unavailable on the Starter/free plan. Docs
  state a platform-wide payment-method requirement and list "10 GPU concurrency"
  on Starter, but no per-GPU-type free/paid distinction is documented.
- Whether Modal's proxy/web endpoints return HTTP 503 or hold the connection
  during a cold start; docs only describe queueing.
- `container_idle_timeout`: not present in current docs; possibly an older name
  superseded by `scaledown_window` — not confirmed.
- Formal deprecation status of the legacy GPU id `A10G`.
- Any GPU type explicitly marked "preview" or "limited": none found in docs.
- A public REST API for app deployment: none documented; absence is not proven
  (only the CLI and `App.deploy()` are documented).

## Copy-paste constants (retrieved 2026-11-12)

Sources: https://modal.com/docs/guide/gpu.md (ids), https://modal.com/pricing
(per-second rates). Per-hour column is computed (×3600), not published.

```json
{
  "retrieval_date": "2026-11-12",
  "sources": [
    "https://modal.com/docs/guide/gpu.md",
    "https://modal.com/pricing",
    "https://modal.com/docs/guide/webhook-proxy-auth.md",
    "https://modal.com/docs/reference/modal.config",
    "https://modal.com/docs/sdk/py/latest/App",
    "https://modal.com/docs/sdk/py/latest/concurrent",
    "https://modal.com/docs/guide/cold-start.md",
    "https://modal.com/docs/guide/scale.md",
    "https://modal.com/docs/guide/billing.md",
    "https://modal.com/docs/guide/region-selection.md"
  ],
  "gpu_ids": ["T4","L4","A10","L40S","A100","A100-40GB","A100-80GB","RTX-PRO-6000","H100","H200","B200","B300"],
  "gpu_id_modifiers": {"H100!": "opt out of automatic H100->H200 upgrade", "B200+": "B200 or B300, billed as B200"},
  "max_gpus_per_container": {"A10": 4, "default_others": 8},
  "price_usd_per_second": {
    "B300": 0.001972, "B200": 0.001736, "H200": 0.001261, "H100": 0.001097,
    "RTX_PRO_6000": 0.000842, "A100_80GB": 0.000694, "A100_40GB": 0.000583,
    "L40S": 0.000542, "A10": 0.000306, "L4": 0.000222, "T4": 0.000164
  },
  "cpu_usd_per_core_second": 0.0000131,
  "memory_usd_per_gib_second": 0.00000222,
  "starter_free_credits_usd_per_month": 30,
  "scaledown_window": {"default_seconds": 60, "min_seconds": 2, "max_seconds": 1200},
  "proxy_token_headers": ["Modal-Key", "Modal-Secret"],
  "proxy_token_prefixes": {"id": "wk-", "secret": "ws-"},
  "auth_env": ["MODAL_TOKEN_ID", "MODAL_TOKEN_SECRET"],
  "config_file": "~/.modal.toml",
  "config_profile_env": "MODAL_PROFILE"
}
```

| GPU id | USD / sec | USD / hr (computed) |
|---|---|---|
| B300 | 0.001972 | 7.0992 |
| B200 | 0.001736 | 6.2496 |
| H200 | 0.001261 | 4.5396 |
| H100 | 0.001097 | 3.9492 |
| RTX PRO 6000 | 0.000842 | 3.0312 |
| A100-80GB | 0.000694 | 2.4984 |
| A100-40GB | 0.000583 | 2.0988 |
| L40S | 0.000542 | 1.9512 |
| A10 | 0.000306 | 1.1016 |
| L4 | 0.000222 | 0.7992 |
| T4 | 0.000164 | 0.5904 |
| CPU (per core) | 0.0000131 | 0.04716 |
| Memory (per GiB) | 0.00000222 | 0.007992 |

## Verified against Modal's own CLI output (2026-11-12)

A second kind of evidence from the Q1–Q10 above: a live `modal` CLI session in the operator's own
terminal rather than a documentation page. It records how Modal's real login and credential surface
compares with what this tree implements. No credential material is reproduced here or anywhere else
in this document — only the `ak-` / `as-` / `wk-` / `ws-` prefixes Modal uses to *name* each class.

**What Modal's own tooling shows**

- `modal token set --token-id ak-... --token-secret as-... --profile=<name>` creates or updates a
  **named profile**, and the tool warns that the token secret is displayed only once.
  `modal profile activate <name>` then selects that profile.
- Environment/config shapes named by Modal: `MODAL_TOKEN_ID`, `MODAL_TOKEN_SECRET`,
  `MODAL_PROFILE`, and a `.modal.toml` profile file. Q8 above already lists these from the docs; the
  CLI session confirms they are the shapes `token set` / `profile activate` actually write and read.
- For **webhook / proxy authentication** (Auto Endpoints and Modal Servers) Modal documents two
  equivalent forms: the header pair `Modal-Key: wk-...` with `Modal-Secret: ws-...`, **or** a single
  `Authorization: Bearer wk-....ws-...` for OpenAI-compatible clients and gateways, the two parts
  joined by a single `.`. Q5 above records both; nothing there changed.
- The `ak-...` / `as-...` pair is a **CLI login pair** and the `wk-...` / `ws-...` pair is a **proxy
  token pair**. They are different credential classes for different jobs, not two encodings of one
  credential.

**The three shapes side by side**

| Credential shape | What Modal does with it | What Manarouter stores / sends | What matches | What does not match |
|---|---|---|---|---|
| CLI login pair `ak-...` / `as-...` | `modal token set` writes it into a **named profile** (`--profile=<name>`), readable also as `MODAL_TOKEN_ID` / `MODAL_TOKEN_SECRET` / `MODAL_PROFILE` / `.modal.toml`; selected with `modal profile activate` | Nothing. No column, vault slot, environment shape or dialect accepts it, and the panel refuses it in the proxy-token fields (see the trap note below) | Only that both sides treat a credential as a two-part value | Everything else: no profile field, no config file, no env shape, no consumer of this class |
| Proxy token pair `wk-...` + `ws-...`, sent as `Modal-Key` + `Modal-Secret` | One of the two accepted webhook/proxy forms | Stored as **one combined string** at `rental:<account id>`, split on the single `.` per request and sent as exactly these two headers | The pair, the header names, the split point, the `wk-`/`ws-` prefixes | Modal lets an operator name, keep and switch between pairs; we hold one pair per account and expose no rotation or replace surface of our own |
| Single `Authorization: Bearer wk-....ws-...` | The other accepted form, for OpenAI-compatible clients and gateways | **Not offered.** No dialect spelling emits it; every writer emits the header pair instead | The combined `id.secret` string is byte-for-byte what Modal's bearer form carries, so the split produces the right two halves | We cannot send this form at all: a client or fronting gateway that wants one header has no configuration path |

**How the pair is stored — already specified, not repeated**

`docs/research/ARCHITECTURE-gpu-rentals.md` §4 (lines 218–236) already settles the storage shape and
this section does not restate it: the platform credential lives at `rental:<account id>` as **one
combined string** (`wk-<token id>.ws-<token secret>`, joined by a single `.`) rather than a JSON
bundle; a Hugging Face token, when needed, goes to `rental:<account id>:hf`; the apply path refuses a
missing dot, a second dot, an empty half or whitespace; and the entry is written exactly once at
account creation because `Vault::put` is not reference-counted. What this section adds is the pairing
of that decision with the shape Modal's own CLI actually handed the operator.

**Verified in the tree while writing this section (2026-11-12)**

- `crates/mr-core/src/dialect.rs:485-523` — `enum AuthKind`, with the `Modal` variant at :509-510
  documented as "Modal-style authentication, using `Modal-Key` and `Modal-Secret` headers".
- `crates/mr-core/src/catalog.rs:1352-1362` — `auth_cell` renders `AuthKind::Modal` as `` `modal` ``
  (:1359). Note for anyone following an older pointer: there is **no**
  `crates/manarouter-conductor/src/catalog.rs` — `auth_cell` lives in `mr-core`'s catalogue module,
  at the lines given here.
- `crates/mr-core/src/lane/modal.rs:41` — `credentials: &[Credential::ProxyTokenPair]`;
  `crates/mr-core/src/lane/mod.rs:222-240` — `ProxyTokenPair` documented as "Two-part proxy token,
  sent as platform-specific headers" (:228-229); `LaneKind::Modal` at `lane/mod.rs:69-70`.
- `crates/mr-core/src/model.rs:275` — an account stores one `secret_ref`. There is no token-id,
  token-secret or profile column anywhere in `Account`.
- `crates/manarouter-conductor/src/gpu_api.rs:97-115` — `rental_credential_id` returns
  `rental:<account id>` and documents the **combined** `wk-<token_id>.ws-<token_secret>` value. A
  Hugging Face token now has its own entity and its own vault id, `hf:<token id>`
  (`mr_core::rental::hf_token_vault_id`), written by the `create-hf-token` apply action and named by a
  deployment; `rental_hf_token_id` → `rental:<account id>:hf` survives only as a **legacy** read path
  for accounts written before that split, so an existing gated-repository setup keeps probing. The
  shape refusal is `modal_credential_problem` (`:117-132`).
- Split points: `crates/manarouter-relay/src/server.rs:4028-4042` (`apply_auth` splits with
  `split_once('.')` and inserts `modal-key` / `modal-secret`); `crates/manarouter-conductor/src/api.rs:2461-2477`
  (same split, and the `invalid_credential` message "Modal account secrets must be the Token
  ID.Token Secret pair"); `crates/mr-cli/src/provider.rs:898-906` (`auth_headers`, same split).
- `panel/gpu-setup-model.js:1722-1723` — two `kind: 'password'` fields, `tokenId` and `tokenSecret`;
  validated separately at `:2017-2026`, rejecting `^ak-` in the id field (`:2019-2021`) and `^as-` in
  the secret field (`:2024-2026`); the pair is combined into the one vault entry.
- `panel/view-providers.js:281` — the "Modal vLLM" platform entry has `baseUrl: ''`: the operator
  supplies the app URL. On the apply path `base_url` is required from the request body
  (`gpu_api.rs:1822-1830`, "the endpoint URL Modal prints when the bundle is deployed ... it cannot
  be guessed, because Modal assigns it per app"), and `gpu_api.rs:2061-2063` builds the provider from
  that URL with `AuthKind::Modal`. The only hardcoded Modal constants are documentation links
  (`crates/mr-core/src/lane/modal.rs:23-30`, `crates/mr-core/src/gpu/catalog.rs:47-51`). No inference
  endpoint is hardcoded anywhere.

**Gaps**

1. **No named-profile concept.** `MODAL_PROFILE` and `.modal.toml` occur nowhere in the tree outside
   this document (and Q8's description of them); a repository-wide search for the strings finds only
   these notes. One account carries one credential, and there is no way to say "this account is
   Modal's `staging` profile" — nor to keep a `default` and a named profile side by side, which is
   exactly what `modal token set --profile=` exists for.
2. **The `ak-...` / `as-...` login pair is not modelable as an account secret.** `Account` has one
   `secret_ref` (`model.rs:275`) and no token-id / token-secret / profile fields, so the pair has
   nowhere to live; and the only Modal reader in the tree expects the proxy pair, splitting on `.`
   and failing with the Token ID.Token Secret message. Refusing that class is correct behaviour, but
   it also means Manarouter cannot hold the credential that `modal deploy` itself needs.
3. **The single-header form is absent.** Our relay always sends the two headers
   (`server.rs:4028-4047`), and `AuthKind::Modal` (`dialect.rs:485-523`) names only the header pair.
   The `Authorization: Bearer wk-....ws-...` form Modal documents for OpenAI-compatible clients and
   gateways is therefore not an alternative dialect we can be configured into; a caller that wants
   one header in front of us has to terminate that elsewhere.
4. **OPEN, not guessed:** whether `mr account add --secret-stdin` can create a `modal-vllm`
   (`AuthKind::Modal`) provider account at all, or whether such accounts arise only through
   `POST /api/gpu/apply` (`gpu_api.rs:9` and `:173` register that route). Facts observed, no
   conclusion drawn: `add_with` (`crates/mr-cli/src/account.rs:284-330`) resolves the provider id
   first, reads the secret, writes it to a provider-derived vault id and performs **no**
   auth-kind-specific shape check; `parse_auth` (`crates/mr-cli/src/provider.rs:177-224`) lists
   `bearer`, `api-key`/`api-key-header`, `header`, `query`/`query-param` and `oauth` and no `modal`
   spelling, but it tries the serde spelling first (`:180`), so the accepted set cannot be read off
   the match arms alone. Left open deliberately; recorded here rather than under §Unverified so the
   question stays with its evidence.

**The `ak-`/`as-`-in-the-wrong-field trap is already guarded**

The two prefixes are easy to confuse, and a login pair sent as `Modal-Key`/`Modal-Secret` would be
the wrong credential class at the wrong door — presumably answered as an ordinary auth failure that
says nothing about the cause (what Modal returns for that case is **UNVERIFIED**, not stated in the
evidence). The panel's account
step prevents that: it collects the two proxy-token halves as separate password fields
(`gpu-setup-model.js:1722-1723`), validates each independently (`:2017-2026`), and rejects an `ak-`
value in the id field and an `as-` value in the secret field with a message naming the class that
belongs there. An operator pasting the wrong credential class gets an explicit refusal in the form,
not a confusing upstream error later. The guard is **panel-side only**: `mr account add` stores
whatever it is handed (`account.rs:284-330`), so a wrong-class paste on the CLI path is caught only
when the first request fails.

**Operator action.** The token pairs used during that verification session should be treated as
exposed and rotated with Modal; no value is recorded in this document, in the repository or in any
Manarouter configuration.