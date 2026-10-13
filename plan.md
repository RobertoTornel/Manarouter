# Plan — a latency-obsessed AI router (Rust data plane + control plane)

> Status: **awaiting user decisions** on the questions in §15 before implementation begins.
> Working dir: `/home/mana/Documents/seek/Router`

---

## 1. Objective

Build a small, public, open-source AI router that:

- Speaks **OpenAI-compatible** (Chat Completions, Responses, Embeddings, Audio Transcriptions, Audio Speech, Image Generations, Models) **and Anthropic-compatible** (`/v1/messages`) on the *client* side.
- Fans out to many upstream providers (OpenAI-compatible + Anthropic-compatible), with **multiple API keys per provider** selected by **priority / round-robin / random / least-used**.
- Supports **OAuth login** for OpenAI Codex, Google Antigravity, GitHub Copilot — and documents how to add more.
- Has **custom combos**: user-named models that route to an ordered list of steps with per-combo strategy (priority, round-robin, reset-aware priority) and a **hedged race mode**.
- Tracks **provider quota** and schedules **reset-aware**.
- Ships a **small web panel** to add/remove/modify providers, keys, combos, and change the **listen port for next restart**.
- Ports the provider set from a local **OmniRoute** install (v16.3.5) and integrates the **commandcode-bridge** protocol as a first-class provider.
- Is **obsessively optimized for latency**: the data plane does nothing but send and receive bytes.

### Hard constraints from the brief

| # | Constraint |
|---|---|
| C1 | Priority/routing decisions live on a **separate process** from the router. |
| C2 | The router is **Rust** and moves everything not related to sending/receiving off the hot path. Combo logic is computed **outside** the router. |
| C3 | The router must route to **multiple providers concurrently**. |
| C4 | The panel may be any language but must be **lightweight**. |
| C5 | The project is **LLM-usage-first**, while remaining fully operable from the panel. |
| C6 | Every feature gets **its own documentation file**; one **index file** is feedable to an LLM to locate any doc. |
| C7 | The port is changeable **from the website**, effective on next restart. |
| C8 | Public GitHub repo, committed. |

---

## 2. Evidence gathered (what the references actually do)

This is not guesswork — the local OmniRoute install was inspected directly.

### 2.1 OmniRoute (local install, v16.3.5)

- Install root: `~/.local/lib/node_modules/omniroute` (~2.5 GB, Node/TypeScript).
- Live instance: `http://127.0.0.1:20129`, 9500 models across **45 `owned_by` provider ids**.
- State: `~/.omniroute/storage.sqlite` — **639 MB**, **142 tables**.
- Relevant tables: `provider_connections` (74 rows), `combos` (7), `provider_quota_state`, `provider_quota_reset_events`, `api_key_quota_counters`, `usage_history`, `model_capabilities`, `key_value` (namespaced settings), `session_account_affinity`, `key_groups`, `plugin`-ish tables.
- **Providers actually credentialed on this machine: 35** (74 connections), across `apikey` and `oauth` auth types.

`provider_connections` columns that prove the routing model we must match:

```
provider, auth_type, priority, global_priority, is_active, access_token, refresh_token,
expires_at, project_id, backoff_level, rate_limited_until, api_key, provider_specific_data,
consecutive_use_count, rate_limit_protection, max_concurrent, group, quota_visible,
quota_window_thresholds_json, rate_limit_overrides_json, last_pinged_reset_key, synced_models_at
```

`consecutive_use_count` ⇒ round-robin / least-used are already tracked per connection.
`backoff_level` + `rate_limited_until` ⇒ reset-aware scheduling is already a first-class concept.

A real combo row (`combos.data`, redacted of nothing sensitive):

```json
{
  "models": [
    { "id": "nsfw-model-1-deepinfra-sao10k-l3-8b-lunaris-v1-turbo", "kind": "model",
      "model": "deepinfra/Sao10K/L3-8B-Lunaris-v1-Turbo", "providerId": "deepinfra", "weight": 0 }
  ],
  "strategy": "priority",
  "config": {
    "maxRetries": 1, "retryDelayMs": 2000, "handoffThreshold": 0.85, "handoffModel": "",
    "maxMessagesForSummary": 30, "trackMetrics": true,
    "reasoningTokenBufferEnabled": true, "zeroLatencyOptimizationsEnabled": false
  },
  "version": 2, "name": "nsfw", "sortOrder": 2, "isHidden": false
}
```

Observed combo strategies in the DB: `priority`, `round-robin`, `fill-first`.
(Note: the brief's requested **hedged race** mode and **reset-aware priority** are richer than anything OmniRoute exposes in `strategy` today — that is net-new work, see §9.)

Observed quota behaviour (important, and worth beating):

```
HTTP/1.1 429 Too Many Requests
retry-after: 300
{"error":{"message":"[openadapter/DeepSeek-V4.1-Flash] All openadapter accounts have
 exhausted their quota (cached quota state, no upstream attempt; earliest reset reset
 after 5m) (reset after 5m)","retry_after":300,"reset_at":"2026-10-13T19:16:52.762Z"}}
```

So OmniRoute short-circuits on *cached* quota state without touching the network, and emits a
machine-readable `reset_at`. We will match that contract — and additionally expose reset state to
the scheduler so a combo can deprioritise an exhausted step *before* the request is even sent.

Other namespaced settings found in `key_value` that inform our data model:
`modelAliases`, `customModels`, `syncedAvailableModels`, `fallbackStrategy` (= `priority`),
`lkgp` (last-known-good-provider, used for session affinity), `compression` / `caveman`
(token compression), `databaseSettings.semanticCache*`, `promptCache*`.

### 2.2 `commandcode-bridge` (local)

- Path: `/home/mana/Documents/seek/fornai/commandcode-bridge` (Node, zero deps).
- Purpose: translate OpenAI Chat Completions → Command Code CLI `POST /alpha/generate` envelope,
  then convert the CLI event stream back to OpenAI JSON or SSE. Targets **Command Code GO-plan**
  users, who must use `/alpha/generate` rather than `/provider/v1/...`.
- Upstream: `https://api.commandcode.ai`, header `x-command-code-version` (default `1.79.2`).
- Env: `CC_API_KEY`, `PROXY_API_KEY`, `PORT`/`HOST` (3050/127.0.0.1), `CC_UPSTREAM_BASE`,
  `CC_CLI_VERSION`, `MAX_BODY_BYTES` (4 MiB).
- This will be reimplemented as a native Rust provider adapter (`dialect = commandcode`), so no
  Node sidecar is needed. Full protocol spec is being extracted (see `docs/providers/commandcode.md`).

### 2.3 9router

Referenced for its multi-provider catalog, client shims (Claude Code / Codex / Cursor / Cline /
Copilot) and RTK token compression. Research artifact: `.research/9router.md` (in flight).

### 2.4 Latency baseline measured so far

Against the local OmniRoute at `127.0.0.1:20129`:

| Measurement | Value |
|---|---|
| Local HTTP accept→first-byte for `/v1/models` (200) | ~0.03–0.17 s |
| Local loopback connect time | ~0.00014 s |
| Quota short-circuit 429 (no upstream call) | ~0.024 s TTFB |
| Real upstream call (`oad/DeepSeek-V4.1-Flash`) | 46 s first warmup, then quota-exhausted |
| `/v1/models`, a purely local cached-list call | **5.2 – 9.2 s TTFB** |
| Zero-upstream-work quota 429 short-circuit | 22 – 295 ms |

**Conclusion:** the existing gateway spends **hundreds of milliseconds on pure gateway overhead** —
a 5–9 s purely local list call, and a 22–295 ms quota rejection that never touches the network — while
loopback RTT itself is only ~0.14 ms. That gap is the entire opportunity, and it is why no
per-request control-plane hop is acceptable: hence the ratified **compiled-snapshot** design (§3.1).
The openadapter model is currently quota-limited, so a full live comparison needs a different probe
model or a reset window; it is a task, not a blocker.

---

## 3. Architecture

Two processes, one cargo workspace, one repo.

```
                       ┌──────────────────────────────────────────────┐
   Claude Code ─┐      │  DATA PLANE  "manarouter-relay"   (Rust, axum/hyper)   │
   Codex CLI  ──┼─────▶│                                              │
   Cursor     ──┤      │  • accept, pull `model` field only           │
   curl/SDK   ──┘      │  • hashmap lookup → precompiled Plan         │
                       │  • key pick (branch-free, atomic counters)   │
                       │  • open/reuse upstream connection            │
                       │  • stream bytes both ways, no JSON re-encode │
                       │  • race/hedge when the Plan says so          │
                       │  • emit async usage/quota deltas (off-path)  │
                       └───────┬──────────────────────┬───────────────┘
                               │ mmap  (read, atomic  │ async, off hot path
                               │ snapshot swap)       │ fire-and-forget
                       ┌───────▼──────────────────────▼───────────────┐
                       │  CONTROL PLANE  "conductor"  (Rust, axum)    │
                       │                                              │
                       │  • web panel (static HTML+JS, no npm)        │
                       │  • provider/key/alias/combo CRUD             │
                       │  • OAuth device+loopback flows, token refresh│
                       │  • /models discovery + manual model add      │
                       │  • combo compiler → snapshot                 │
                       │  • quota tracker, reset windows, backoff     │
                       │  • usage rollups, health probes              │
                       │  • SQLite (rusqlite) + JSON config           │
                       └──────────────────────────────────────────────┘
```

### 3.1 The key idea: *compile* combos, don't *interpret* them

The brief says combo logic must live outside the router. Taken literally as "the router asks the
control plane what to do", that costs an IPC round trip per request — with measured loopback RTT
at ~0.14 ms and a real RPC + JSON encode/decode on both sides, that is realistically 0.3–2 ms
added to every single request, forever.

Instead:

1. The conductor holds the full, rich, editable model (combos, weights, races, quota policy).
2. On every change it **compiles** that into a flat, self-contained **Routing Snapshot**: a
   `Vec<RouteEntry>` in a layout the data plane can binary-search/hash directly, plus an interned
   string table for model names, plus `Arc`-shared atomic counters.
3. It writes the snapshot to a file and pings the data plane.
4. The data plane `mmap`s it and atomically swaps an `arc_swap::ArcSwap<Snapshot>` pointer.

Result: **zero IPC per request.** The data plane's routing step is pointer load + hash + a couple
of atomic ops. Combo *authoring* stays entirely in the control plane, satisfying C1/C2 while not
paying for it at runtime. Same trick for key counters: the snapshot carries `Arc<AtomicU64>`
so "least-used" is a local min-scan over an array that is almost always 1–4 entries.

Anything genuinely dynamic and unavoidable (OAuth token refresh, quota reset transitions) is
handled as **optimistic + repair**: the data plane assumes the last snapshot is valid, and on a
401/403/429 it emits a signal to the conductor and (where safe) retries once with the refreshed
credit.

### 3.2 Why two Rust binaries rather than Rust + Node

- One toolchain, one build, no `node_modules`, one CI.
- Panel is a single static `index.html` + `app.js` + `style.css` embedded with `include_str!`
  → the conductor binary is self-contained and starts in milliseconds.
- The conductor is *not* on the request path, so it can afford SQLite, background tasks, and a
  web UI without any latency consequence. (C4 satisfied: total panel payload target < 100 KB.)

### 3.3 Request lifecycle (hot path), with the cost of each step

| Step | Work | Budget |
|---|---|---|
| 0 | Accept on a `SO_REUSEPORT` listener, one per worker thread | — |
| 1 | Read headers + body into a pooled `BytesMut` | alloc-once |
| 2 | Extract `model` via a targeted byte scan of the first ~4 KiB (no full JSON parse) | ~0.5 µs |
| 3 | `ArcSwap::load()` → `&Snapshot`; hash lookup `model → Plan` | ~50 ns |
| 4 | Key pick: strategy branch over a tiny array + one relaxed atomic | ~20 ns |
| 5 | Build the upstream request: clone the *original body bytes verbatim*, rewrite/insert only the auth + routing headers | ~1 µs |
| 6 | Take a pooled connection (keep-alive, TLS session resumption, pre-warmed) | ~0 ns warm |
| 7 | Stream response bytes to the client, byte-shuttling, no deserialization | — |
| 8 | On completion, push one `UsageDelta` into an unbounded MPSC, never await it | ~100 ns |

**Total added overhead target: < 150 µs p50, < 500 µs p99.**

The only cases that pay a translation cost are dialect mismatches, which are explicitly opt-in per
route (§6.3).

---

## 4. Repository layout

```
Router/
├── Cargo.toml                  # workspace: manarouter-relay, conductor, core, dialects
├── rust-toolchain.toml
├── README.md                   # project README (C6)
├── LICENSE                     # MIT
├── plan.md                     # this file
├── docs/
│   ├── LLM_INDEX.md            # ★ the single file to feed an LLM: map of every doc (C6)
│   ├── 00-overview.md
│   ├── 01-architecture.md
│   ├── 02-quickstart.md
│   ├── 03-configuration.md
│   ├── 04-routing-snapshot.md
│   ├── 05-key-selection.md
│   ├── 06-combos.md
│   ├── 07-hedged-race.md
│   ├── 08-quota-tracking.md
│   ├── 09-reset-aware-priority.md
│   ├── 10-oauth-overview.md
│   ├── 11-oauth-codex.md
│   ├── 12-oauth-antigravity.md
│   ├── 13-oauth-copilot.md
│   ├── 14-oauth-adding-a-platform.md     # C6: how to add future platforms
│   ├── 15-web-panel.md
│   ├── 16-latency-engineering.md
│   ├── 17-migrating-from-omniroute.md
│   ├── 18-api-reference.md
│   ├── 19-latency-benchmarks.md
│   ├── 20-troubleshooting.md
│   ├── 21-security.md
│   ├── 22-sync-design.md        # v1 ships this doc; v1 implements no sync
│   ├── providers/
│   │   ├── README.md
│   │   ├── catalog.md          # generated: every provider, dialect, auth, base URL
│   │   ├── openai-compatible.md
│   │   ├── anthropic-compatible.md
│   │   ├── commandcode.md
│   │   ├── codex.md
│   │   ├── copilot.md
│   │   ├── antigravity.md
│   │   └── <one file per provider family ported>
│   └── combo-cookbook.md
├── crates/
│   ├── rr-core/                # shared types: Snapshot, Plan, Provider, Key, Combo, Dialect
│   ├── rr-dialects/            # dialect translation: openai <-> anthropic <-> responses <-> commandcode
│   ├── rr-relay/               # ← the data plane binary (hot path)
│   ├── rr-conductor/           # ← the control plane binary + panel + SQLite
│   └── rr-cli/                 # `mr` CLI: login, add provider, test, bench, migrate
├── panel/                      # static, embedded into conductor via include_str!
│   ├── index.html
│   ├── app.js
│   └── style.css
├── migrations/                 # SQLite schema migrations, numbered
├── tests/
│   ├── snapshot_compile.rs
│   ├── key_selection.rs
│   ├── combo_race.rs
│   ├── dialect_roundtrip.rs
│   ├── quota_reset.rs
│   └── e2e_mock_upstream.rs
└── tools/
    └── bench/                  # latency harness, reports p50/p90/p99 + overhead vs direct
```

---

## 5. Core data model

### 5.1 Config (human-editable, JSON, `~/.config/manarouter/`)

```jsonc
{
  "listen": { "host": "127.0.0.1", "port": 20900 },        // ← panel-editable (C7)
  "control": { "host": "127.0.0.1", "port": 20901, "password_hash": "$2b$…" },
  "client_keys": [ { "key": "rr-…", "label": "claude-code", "allow": ["*"] } ],
  "providers": [ /* see below */ ],
  "aliases": { "oad/DeepSeek-V4.1-Flash": "openadapter/DeepSeek-V4.1-Flash" },
  "combos": [ /* see §9 */ ],
  "quota": { "track": true, "reset_aware": true, "cooldown_on_429_s": 60 },
  "latency": {
    "prewarm_connections": true, "pool_idle_s": 300,
    "upstream_http2": "auto", "tcp_nodelay": true,
    "hedge": { "default_max_parallel": 3, "default_window_ms": 800 }
  }
}
```

### 5.2 Provider

```jsonc
{
  "id": "openadapter",
  "label": "OpenAdapter",
  "dialect": "openai",                 // openai | anthropic | google | commandcode | codex | copilot | antigravity
  "base_url": "https://…/v1",
  "auth": { "type": "bearer" },        // bearer | x-api-key | header | query | cookie | oauth
  "capabilities": ["chat","responses","embeddings","audio.transcription",
                   "audio.speech","images.generations","models"],
  "keys": [
    { "id": "k1", "api_key": "…", "priority": 0, "weight": 1, "enabled": true,
      "max_concurrent": 4, "models_allow": ["*"] }
  ],
  "key_strategy": "priority",          // priority | round-robin | random | least-used
  "models": { "source": "synced", "synced_at": "…", "manual": ["gpt-x-custom"] },
  "quota": { "windows": [ { "kind": "rolling", "seconds": 18000, "limit_tokens": 1000000 } ] },
  "headers": { "extra": {} },
  "fronted_by": null                    // optional: route via OmniRoute/existing gateway
}
```

### 5.3 Compiled snapshot (the only routing structure the data plane sees)

```rust
#[repr(C)]
pub struct RouteEntry {
    model_hash: u64,        // FNV-1a of the requested model id
    model_off: u32, model_len: u32,   // interned into the snapshot's string table
    kind: RouteKind,        // Single | Combo
    plan_off: u32,          // offset into PlanTable
}
pub enum RouteKind { Single, Combo }

pub struct SinglePlan {
    provider_idx: u16,
    dialect: Dialect,
    base_url: InternedStr,
    auth_kind: AuthKind,
    keys: SmallVec<[KeyRef; 4]>,     // KeyRef holds Arc<AtomicU64> counters + Arc<str> secret
    key_strategy: KeyStrategy,
    header_ops: SmallVec<[HeaderOp; 8]>,
}

pub struct ComboPlan {
    strategy: ComboStrategy,          // Priority | RoundRobin | ResetAwarePriority
    hedge: Option<HedgeSpec>,
    steps: SmallVec<[SinglePlan; 8]>, // pre-resolved, ordered
    on_fail: Option<u16>,             // index into steps, or a dedicated plan
    hedge_spec: Option<HedgeSpec>,
}
pub struct HedgeSpec {
    parallel: u8,                     // "first X models"
    window_ms: u16,                   // "X latency"
    on_winner_fail: WinnerFail,       // ReRace | UseOnFail | Error
}
```

`Dialect` + `SinglePlan` being fully resolved at compile time is what lets the data plane do no
interpretation. JSON is never touched on the hot path for the matching-dialect case.

---

## 6. Provider support

### 6.1 Dialects

| Dialect | Upstream shape | Notes |
|---|---|---|
| `openai` | Chat Completions, Responses, Embeddings, Audio, Images, Models | The default. Covers the majority of the 35 ported providers. |
| `anthropic` | `/v1/messages`, `/v1/messages/count_tokens` | Native, no translation when the client also speaks Anthropic. |
| `google` | `generateContent` / `streamGenerateContent` | Gemini API-key path. |
| `antigravity` | `cloudcode-pa.googleapis.com` `v1internal:*` | OAuth; needs the project/tier onboarding envelope. |
| `codex` | `chatgpt.com/backend-api/codex/responses` | OAuth; Responses-only, `store:false`, `instructions` required. |
| `copilot` | `api.githubcopilot.com` | OAuth→Copilot bearer; `/chat/completions`, `/v1/messages`. |
| `commandcode` | `api.commandcode.ai/alpha/generate` | CLI envelope + custom event stream (§2.2). |

### 6.2 Client surfaces (shims)

The brief is LLM-usage-first, so the relay must be droppable into existing agent CLIs:

- `POST /v1/chat/completions` (+ `stream`)
- `POST /v1/responses`
- `POST /v1/embeddings`
- `POST /v1/audio/transcriptions`
- `POST /v1/audio/speech`
- `POST /v1/images/generations`
- `GET  /v1/models`, `GET /v1/models/{id}`
- `POST /v1/messages` (+ `stream`) — Anthropic, for Claude Code
- `POST /v1/messages/count_tokens`
- `POST /v1/complete` — legacy FIM/Codex CLI compatibility
- `GET  /health`, `GET /metrics`

Point Claude Code at `ANTHROPIC_BASE_URL`, Codex CLI at the Responses URL, Cursor/Cline at the
OpenAI URL.

### 6.3 Translation policy (latency-relevant)

| Client → Upstream | Behaviour | Cost |
|---|---|---|
| openai → openai | **byte passthrough**, headers only | ~0 |
| anthropic → anthropic | **byte passthrough** | ~0 |
| openai → anthropic | full translate | serde + SSE rebuild |
| anthropic → openai | full translate | serde + SSE rebuild |
| any → codex/responses | translate + enforce `store:false`, `instructions` | serde |
| any → commandcode | translate to CLI envelope, rebuild event stream | serde |
| any → google/antigravity | translate | serde |

Translation lives in `mr-dialects`, is pure, and is only invoked when
`client_dialect != upstream_dialect`. This is the single biggest latency decision in the project
(Q9).

For Response/SSE streams where dialects match, the relay never materialises a JSON value: it
finds SSE frame boundaries and forwards `Bytes` slices, only peeking at `usage` frames via a
cheap memchr for `"usage"` before the terminator.

### 6.4 Model discovery

- **From `/models`**: conductor task per provider, paginated, scheduled (`synced_models_at`),
  writes into the `models` cache table; diffed and surfaced in the panel with a "sync now" button.
- **Manually**: panel form (model id, dialect override, context length, capabilities, aliases) and
  config-file editing. Manual entries win over synced ones (mirrors OmniRoute's
  `modelMetadataPrecedence`).
- Aliases: provider-prefixed (`openadapter/X`), short (`oad/X`), bare (`X`), plus user aliases and
  combo names — mirroring OmniRoute's observed triple-exposure (Q17).

### 6.5 Porting OmniRoute's providers

`mr-cli migrate omniroute --db ~/.omniroute/storage.sqlite` will:

1. Read `provider_connections` and emit one `Provider + keys` block per connection, **preserving
   `priority`, `global_priority`, `max_concurrent`, `group`** and de-duplicating by provider.
2. Map `auth_type`: `apikey` → `api_key`; `oauth` → an OAuth credential record (tokens copied from
   `access_token`/`refresh_token`/`expires_at`/`project_id`/`provider_specific_data`).
3. Read `combos` and emit combo definitions, mapping `strategy` `priority|round-robin|fill-first`
   onto ours, and carrying `config.maxRetries`/`retryDelayMs`/`handoffThreshold`/`handoffModel`.
4. Read `key_value` namespaces `modelAliases`, `customModels`, `syncedAvailableModels`,
   `fallbackStrategy` into our config.
5. Report a dry-run diff before writing.

Secrets are **never** written into the repo or the snapshot; they live in `~/.config/manarouter/secrets/`
mode `0600` (Q14) and are referenced by id.

---

## 7. OAuth subsystem

Uniform trait so every platform is the same shape (this is also the answer to C6's
"how to add more OAuth platforms later"):

```rust
#[async_trait]
pub trait OAuthPlatform: Send + Sync {
    fn id(&self) -> &'static str;                    // "codex" | "copilot" | "antigravity"
    fn label(&self) -> &'static str;
    /// Which of the three flows this platform uses.
    fn flow(&self) -> OAuthFlow;                     // LoopbackPkce | DeviceCode
    async fn build_authorize(&self, st: &PkceState) -> AuthorizeRequest;
    async fn exchange(&self, code: &str, st: &PkceState) -> Result<TokenSet>;
    async fn refresh(&self, rt: &str) -> Result<TokenSet>;
    /// Turn a live TokenSet into the headers/params the upstream dialect needs.
    async fn apply(&self, tok: &TokenSet, req: &mut UpstreamRequest) -> Result<()>;
    /// Optional: exchange a provider token for a narrower one (Copilot).
    async fn mint_session(&self, tok: &TokenSet) -> Result<Option<TokenSet>>;
}
```

Registry `PLATFORMS: &[&dyn OAuthPlatform]`; adding one is a new module + one line, documented
step-by-step in `docs/14-oauth-adding-a-platform.md`.

Flows implemented:

- **LoopbackPkce** (Codex, Antigravity): bind `127.0.0.1:<ephemeral>`, open the browser, capture
  `?code=`, exchange, store.
- **DeviceCode** (GitHub Copilot candidate): print `user_code` + `verification_uri`, poll.

Token lifecycle: background refresh in the conductor at `expiry - 5 min`; the data plane reads the
current token from the snapshot; on a `401` the relay signals the conductor and retries once with
a freshly minted token. Refresh is single-flight per credential to avoid stampedes.

Exact endpoints/client-ids/PKCE details are being extracted into
`docs/11-oauth-codex.md`, `docs/12-oauth-antigravity.md`, `docs/13-oauth-copilot.md`.

---

## 8. Web panel

Single static page, embedded in the conductor binary, no build step, no npm.

Views:
1. **Dashboard** — relay/control status, listen port, request rate, p50/p95/p99 overlay, error rate,
   active connections, snapshot version + "recompile" indicator.
2. **Providers** — list with dialect, base URL, key count, health, quota bars, enable/disable,
   edit, delete, "test", "sync models".
3. **Keys** — per-provider list: add/remove/reorder (drag), priority, weight, strategy selector
   (priority / round-robin / random / least-used), per-key limits, masked display (`sk-06fe…`).
4. **Models** — synced + manual, search across the whole catalog, add-manually form, alias editor.
5. **Combos** — the centrepiece:
   - create/rename/delete; custom name is the model id clients call
   - drag-ordered step list with per-step provider+model picker
   - strategy dropdown: `priority` / `round-robin` / `reset-aware priority`
   - **hedge panel**: `parallel` (first X), `window_ms` (X latency), `on_winner_fail`
     (`re-race` / `use on-fail model` / `error`), plus the on-fail model picker
   - per-step overrides, weights, enable/disable
   - "dry run" that shows the compiled plan and simulates which step a request would pick
6. **OAuth** — one card per platform, "Log in" button → device code or browser redirect, live
   status, token expiry countdown, multi-account list.
7. **Quota** — per provider/key windows, used/limit, reset countdown, 429 history, backoff state.
8. **Settings** — **listen port (C7)**, control port, client API keys, auth password, latency
   knobs, snapshot path, `restart` button (graceful drain + re-exec) with the explicit note
   *"applies on next restart"*.
9. **Logs / Usage** — recent requests (sampled, off hot path), token spend rollups.

Panel auth: localhost-bind by default + password login for non-loopback (Q20).

---

## 9. Combo engine — ratified spec

A combo is a **named model**. Clients call it exactly like a model: `{"model": "Brains"}`.

### 9.1 Definition

```jsonc
{
  "name": "Brains",                     // the model id clients call
  "strategy": "priority",               // priority | round-robin | reset-aware-priority
  "steps": [                            // EXACTLY the user's order; never reordered
    { "provider": "codex",        "model": "cx/gpt-6-astra", "enabled": true, "weight": 1 },
    { "provider": "command-code", "model": "cmd/gpt-5.5",    "enabled": true },
    { "provider": "openadapter",  "model": "oad/DeepSeek-V4.1-Flash" }
  ],
  "hedge": {
    "enabled": true,
    "participants": [0, 1, 2],               // WHICH steps compete. [] = first `count` in order
    "count": 2,                              // "first X models", used when participants is empty
    "window_ms": 800,                        // "X latency" — the eligibility window
    "winner_metric": "first_response_byte",   // first_response_byte | first_complete_token
    "on_winner_fail": "rerun-excluding",     // see 9.4
    "stable_model": null,                    // required by on_winner_fail = "stable-fallback"
    "max_rounds": 2                          // bound on "rerun-excluding"
  },
  "midstream_policy": "restart",         // restart | terminate   (see 9.5)
  "limits": { "max_retries": 1, "retry_delay_ms": 2000 },
  "compression": { "enabled": false, "mode": "rtk" }   // off-path transformer, default OFF
}
```

### 9.2 Strategies (non-hedged)

- **priority** — always step 1; on failure step 2; and so on. Deterministic and sticky.
- **round-robin** — start at `n mod len`, then priority-fall forward from there. `n` is a per-combo
  relaxed atomic, so it costs one `fetch_add`.
- **reset-aware priority** — priority order, but any step with **no available account** is demoted
  below all healthy steps rather than removed, and is restored automatically by a timer at its
  earliest known reset. Within each bucket the user's order is preserved exactly.

### 9.3 The latency window is an eligibility gate, not a race to be fastest

This is the crux of the requested behaviour, and it is deliberately **not** "fastest wins":

> **Winner = the highest-priority participant that produced a valid reply within `window_ms`.**

The consequences are the reason this is the right rule:

- If step 1 replies at 150 ms it wins **immediately**: nothing lower-priority can beat it, so the
  race resolves at once and the losers are cancelled. **Hedging costs nothing when the preferred
  model is healthy** — no window wait, no added latency.
- If step 1 replies at 600 ms and step 2 at 200 ms with `window_ms = 800`, **step 1 still wins**,
  because both were inside the window and step 1 is higher in the user's order. Hedging here buys
  insurance against the preferred model failing or stalling — not raw speed.
- If step 1 fails outright, the winner is the next highest participant that replied inside the
  window.
- If nobody replies inside the window the request **does not fail**: the window only expires a
  participant's *priority protection*. The race keeps waiting and the highest-priority participant
  that eventually replies wins.

Implementation, therefore: participant `i` is committed as winner as soon as every participant
**above** it (in the participant list) is resolved — either replied outside the window, or failed.
Worst-case added latency is bounded by `window_ms`, and it is only ever paid when a
higher-priority model is slow, which is exactly the case where switching is the point.

`winner_metric` controls what "replied" means, at the user's choice:

| Value | Definition | Cost of a lost hedge |
|---|---|---|
| `first_response_byte` | first non-error upstream response **body** byte (TTFB) | ~0 — loser has generated nothing billable |
| `first_complete_token` | first fully-parsed content delta, or the complete response when non-streaming | one token on every loser |

Ties are always broken by the user's step order.

### 9.4 What happens when the chosen model dies — four user-selectable modes

| Mode | Behaviour |
|---|---|
| `rerun-excluding` | Re-run the whole competition over the remaining participants, **excluding every model that has already failed**, up to `max_rounds`. This is the requested "run this again" branch. |
| `pick-highest-passed` | Do **not** re-dispatch. Take the highest-priority model that already **passed** in the first competition — its result is already in the race's result set or still in flight. Zero added latency. |
| `pick-highest-passed-different-provider` | As above, restricted to a participant on a **different provider** than the one that died. Protects against a whole-provider outage, including a provider-wide quota reset. |
| `stable-fallback` | Jump straight to a user-chosen `stable_model` that need not be part of the competition. This is the requested "on fail" model. |

`hedge.participants` additionally lets the user pick **exactly which models enter the competition**,
independently of the full ordered step list — a model can be in the fallback chain without ever
being hedged.

When the failure happens **before any byte reached the client**, the selected mode runs fully
transparently.

### 9.5 When the winner dies mid-stream (ratified: retry per combo rules)

Per the ratified decision, the prompt is **retried with the next best model according to the mode
and rules inside the combo** — `on_winner_fail` is re-evaluated and the retry follows it. That is
only safe in two shapes, so `midstream_policy` makes the choice explicit:

- **`restart` (default)** — the retry proceeds. If **no bytes** have reached the client the switch
  is completely invisible. If bytes have already been flushed, the relay emits an explicit,
  documented SSE boundary (`event: manarouter.retry` plus the terminated `finish_reason`) and then
  streams the new model's content, so clients that ignore unknown SSE events still receive a
  complete, terminated, non-overlapping response.
  **Hard guard:** if any `tool_calls` delta was already emitted, the relay **refuses to splice** and
  terminates with an error. A duplicated or half-emitted tool call corrupts the client's state
  machine, which is strictly worse than a clean failure.
- **`terminate`** — never retry once bytes are on the wire; close the stream with a proper error and
  let the client decide.

Both are per-combo configurable, so the behaviour is auditable rather than implicit.

### 9.6 Why this is still "outside the router"

The relay evaluates **no policy**. It executes a `ComboPlan` that the conductor already compiled:
a fixed-length array of fully-resolved `SinglePlan`s, the participant index list, a `u8` count, a
`u16` window, two enums, and `Arc<AtomicU64>` availability stamps. Running a race is *sending and
receiving*, which is precisely the relay's remit; deciding order, eligibility, participants and
fallback is not, and lives in the conductor.

### 9.7 Session affinity (latency + cost win)

Keyed by a hash of the conversation's first system + first user message, or by a client-supplied
`x-manarouter-session` header. The relay keeps a small sharded LRU `session → chosen step`. Because
the winner is sticky for the session, hedged races happen **once per conversation**, not once per
turn, and upstream prompt caches stay warm. This mirrors OmniRoute's observed `lkgp` /
`session_account_affinity` behaviour but is a lock-free sharded map, so it is free on the hot path.

## 10. Key selection

Per provider, `key_strategy ∈ {priority, round-robin, random, least-used}`:

| Strategy | Data-plane operation | Cost |
|---|---|---|
| priority | first `enabled && !cooling` key in priority order | array scan |
| round-robin | `idx = counter.fetch_add(1, Relaxed) % n` | 1 atomic |
| random | one xorshift from a per-thread RNG seeded from the CLOCK_MONOTONIC | ~5 ns |
| least-used | min over `AtomicU64` counters, cached in a per-provider `AtomicU32` "best" hint refreshed every N picks | ~20 ns |

Plus: skip accounts whose `available_after` (rate-limit reset) is in the future, honour
`max_concurrent` via a per-account semaphore, and honour per-account `models_allow`. All of it is
array scanning over ≤8 entries — no locks on the hot path (only the optional `max_concurrent`
semaphore, which is a fast `try_acquire`).

**Health is per account, never per provider (ratified Q4).** A provider with N accounts is *never*
declared dead while at least one account is alive. The provider-level status shown in the panel is a
derived rollup (`healthy` / `degraded 2/6 accounts available` / `dead 0/6`), and it never gates
routing on its own. Account selection is dynamic on every request and always skips accounts that are
rate-limited, cooling down, over their window, disabled, or out of quota — so a provider with one
throttled key and five healthy keys behaves exactly like a five-key provider.

---

## 11. Quota tracking

Per `(provider, key, model, window)`:

- Counters in the data plane as `AtomicU64` (requests, input tokens, output tokens) — incremented
  *after* the response, never awaited.
- A batched flusher task drains deltas to SQLite in the conductor every ~1 s or every 512 events.
- Window kinds: `rolling(seconds)`, `fixed(seconds, aligned)`, `daily`, `monthly`, `provider_declared`.
- Signals folded in: HTTP `429` + `retry-after`, `x-ratelimit-remaining-*`/`-reset-*`,
  provider-specific quota endpoints (e.g. Copilot entitlement, Codex usage), and the panel's
  manual "reset at" overrides.
- On exhaustion the data plane short-circuits with the same machine-readable contract OmniRoute
  emits (`retry_after`, `reset_at`) — see §2.1 — so existing clients and our own combo scheduler
  both understand it. (Q4 decides signal priority.)

A `provider_quota_reset_events`-equivalent history is kept so the panel can show reset countdowns
and the scheduler can be *predictive*: a step whose accounts all reset in 30 s is demoted now and
restored by a timer, not by trial and error.

**Reset-aware priority operates per account and rolls up per step (ratified Q4).** A combo step is
only demoted when *every* account backing it is unavailable; when *some* accounts are available the
step stays in place and the selection layer simply routes around the dead accounts. This is what
makes a provider with a mixed set of keys — some throttled, some fresh — keep serving traffic at full
priority instead of being skipped wholesale.

---

## 12. Latency engineering checklist

Build:
- `opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`.
- `RUSTFLAGS="-C target-cpu=native"` for local release builds; a portable `x86-64-v2` default build
  for published artifacts.
- `mimalloc` (or `jemalloc`) global allocator.
- `tokio` with `net` + `rt-multi-thread`, one worker thread per core, `SO_REUSEPORT` listeners.

Network:
- Connection pool per `(provider, key, resolved-ip)`, keep-alive, `TCP_NODELAY`, `SO_KEEPALIVE`.
- `rustls` with a shared `ClientConfig` and **session resumption** (tickets) so repeat TLS
  handshakes are 0-RTT-ish rather than full.
- Pre-warm: conductor marks a provider "hot" (recently used) → data plane keeps ≥2 idle
  connections ready. First request after idle pays no handshake in the common case.
- HTTP/2 to upstreams that support it, with prior-knowledge/ALPN chosen from a cached capability
  flag; HTTP/1.1 otherwise. No upgrade dance on the hot path.
- Happy Eyeballs connect with a cached DNS decision; DNS resolved by a background task and cached
  with TTL, never resolved inline on the hot path.

Data handling:
- `bytes::Bytes`/`BytesMut` with pooling; no `String` allocation for bodies.
- `memchr` for the `model` field and SSE framing.
- `serde_json` only on translation paths; `simd-json` behind a feature flag for large bodies.
- No `format!`, no tracing spans, no histogram updates on the hot path — sampling is off-path.

Process hygiene:
- Logging via a bounded ring buffer + a dedicated writer task; if full, drop (never block).
- `SO_BUSY_POLL`/`io_uring` behind a non-default feature (Q11).
- Optional: `mlockall` and huge pages for the snapshot mapping.

Measurement (must be in CI, not vibes):
- `tools/bench` reports, for each surface: direct-to-provider vs through-relay, with
  p50/p90/p99 and a computed **added overhead** column; asserts overhead p50 < 150 µs and
  p99 < 500 µs against a local mock upstream.
- A loopback mock upstream removes provider variance so the number is attributable to us.
- Real-provider comparisons (openadapter `oad/DeepSeek-V4.1-Flash`, command-code) are documented in
  `docs/19-latency-benchmarks.md`, clearly separated from the controlled mock numbers, because
  provider latency dominates and varies (already observed: 46 s cold, then quota-exhausted).

---

## 13. Documentation deliverables (C6)

- Every feature file listed in §4 is a real, standalone doc: what it is, why, config reference,
  API/JSON examples, failure modes, how to debug it.
- `docs/LLM_INDEX.md` is the **single file to feed an LLM**. It must contain: project summary,
  one-line purpose + scope + key entry points for every doc, a table of "task → read these files",
  the exact config schema, the full route table, the enum vocabularies (dialects, strategies,
  flows), and a machine-readable `MANIFEST` block listing every doc path with its purpose so an LLM
  can pick the right file without a filesystem crawl.
- `docs/providers/catalog.md` is generated from the ported provider registry so it cannot drift.
- README.md: what it is, why it exists, architecture diagram, quickstart, feature matrix vs
  OmniRoute/9router, screenshots of the panel, and pointers into `docs/`.

---

## 14. Phases & task breakdown

Each phase ends with something runnable and verified. No phase is "done" without its doc file.

### Phase 0 — Foundation
- [ ] 0.1 Verify rust toolchain (done: rustc 1.99.0), pin `rust-toolchain.toml`.
- [ ] 0.2 `cargo` workspace skeleton: 5 crates, shared `mr-core` types.
- [ ] 0.3 MIT LICENSE, `.gitignore`, `rustfmt.toml`, CI workflow (build + test + clippy + bench gate).
- [ ] 0.4 `create_goal` already set in DSH (done).
- **Exit:** `cargo build --workspace` + `cargo test` green.

### Phase 1 — Core types + dialect engine
- [ ] 1.1 `mr-core`: `Provider`, `Key`, `Dialect`, `RouteEntry`, `SinglePlan`, `ComboPlan`,
      `Snapshot`, `RouteKind`, `KeyStrategy`, `ComboStrategy`, `HedgeSpec`, `WinnerFail`.
- [ ] 1.2 Snapshot binary format + `mmap` reader + `arc_swap` publish/reload (inotify + a
      control-socket ping).
- [ ] 1.3 `mr-dialects`: OpenAI Chat Completions ↔ Anthropic Messages (req + resp + SSE, tools,
      reasoning blocks).
- [ ] 1.4 `mr-dialects`: OpenAI Chat Completions ↔ Responses API.
- [ ] 1.5 `mr-dialects`: Command Code envelope encode/decode (from the local bridge spec).
- [ ] 1.6 Golden round-trip tests for every pair, including awkward payloads
      (tool-call streams, reasoning blocks, image parts, multi-modal content).
- **Exit:** dialect round-trip test suite green; docs 01, 04, 18 + `providers/openai-compatible.md`,
  `providers/anthropic-compatible.md`, `providers/commandcode.md`.

### Phase 2 — The relay (data plane)
- [ ] 2.1 axum/hyper server, `SO_REUSEPORT` workers, per-worker runtimes.
- [ ] 2.2 Fast `model` extraction (memchr scan, no full parse) + unit tests on hostile bodies.
- [ ] 2.3 Snapshot lookup, key selection (all 4 strategies), `available_after` gating.
- [ ] 2.4 Upstream client: pooled connections, rustls + resumption, HTTP/2 auto, pre-warm.
- [ ] 2.5 Pure passthrough streaming for matched dialects; verbatim body forwarding.
- [ ] 2.6 Translation path wiring for mismatched dialects.
- [ ] 2.7 Client surfaces: all routes in §6.2, both streaming and not.
- [ ] 2.8 Usage delta emitter (non-blocking) + SSPL-free metrics endpoint.
- [ ] 2.9 Quota short-circuit 429 with `retry_after`/`reset_at` contract.
- **Exit:** mock-upstream e2e tests green for every surface; docs 05, 16, 18.

### Phase 3 — Combos + hedging
- [ ] 3.1 Combo compiler: user order → `ComboPlan`; dry-run validator.
- [ ] 3.2 priority / round-robin / reset-aware-priority execution.
- [ ] 3.3 Hedged race: parallel dispatch, TTFB winner, tie-break by order, loser abort.
- [ ] 3.4 `on_winner_fail` × {pre-byte, post-byte} state machine, both `re-race` and `use-on-fail`.
- [ ] 3.5 Session affinity sticky map (sharded, lock-free) + `x-manarouter-session`.
- [ ] 3.6 Exhaustive tests: slow-first/fast-second, all-fail, winner-dies-pre-byte,
      winner-dies-mid-stream, deadline passing, 8-step combos, concurrent sessions.
- [ ] 3.7 Four `on_winner_fail` modes incl. participants selection and `stable-fallback`.
- [ ] 3.8 `midstream_policy` (`restart` / `terminate`) with the `tool_calls` no-splice guard.
- **Exit:** docs 06, 07, `combo-cookbook.md`.

### Phase 4 — Conductor + storage + panel
- [ ] 4.1 SQLite schema + migrations (providers, accounts, models, combos, quota, usage, oauth,
      settings, **oplog**) — UUIDv7 ids, `revision`, `deleted_at` tombstones from the first migration (§18).
- [ ] 4.2 Config load/validate/atomic-save; snapshot compile + publish on every mutation.
- [ ] 4.3 REST API for all CRUD (documented in 18) + password auth.
- [ ] 4.4 Panel: single static page, all 9 views, embedded via `include_str!`.
- [ ] 4.5 Port editor + graceful restart (drain + re-exec) with "applies next restart" semantics (C7).
- [ ] 4.6 Model discovery scheduler + manual model add + alias editor.
- [ ] 4.7 Quota tracker: windows, reset timers, prediction, per-account health rollup, panel visualisation.
- [ ] 4.8 Off-path compression transformer (RTK/caveman), **default OFF**, enableable per combo (Q15).
- [ ] 4.9 `sync` module: inert oplog writer + panel "pending changes" counter + `docs/22-sync-design.md` (Q15).
- **Exit:** a human can drive the whole system from the browser; docs 03, 08, 09, 15, 21.

### Phase 5 — OAuth
- [ ] 5.1 `OAuthPlatform` trait + registry + credential store (encrypted, 0600).
- [ ] 5.2 Codex (loopback PKCE) end-to-end, including the inference envelope.
- [ ] 5.3 Copilot (device/token mint) end-to-end.
- [ ] 5.4 Antigravity (loopback PKCE + project onboarding) end-to-end.
- [ ] 5.5 Single-flight refresh, 401 repair path, multi-account, panel cards.
- [ ] 5.6 `docs/14-oauth-adding-a-platform.md` with a worked example of adding a *new* platform.
- **Exit:** all three log in from the panel and serve real traffic; docs 10–14 +
  `providers/codex.md`, `providers/copilot.md`, `providers/antigravity.md`.

### Phase 6 — Provider port from OmniRoute + Command Code
- [ ] 6.1 Extract the OmniRoute provider registry → our TOML/JSON catalog (in progress:
      `.research/omniroute.md`).
- [ ] 6.2 `mr-cli migrate omniroute`: connections, keys, combos, aliases, settings (dry-run first).
- [ ] 6.3 Port the 35 credentialed providers as config entries + native adapters for the special
      dialects; generate `docs/providers/catalog.md`.
- [ ] 6.4 Command Code GO-plan adapter (native Rust reimplementation of the local bridge).
- [ ] 6.5 Cookie/web-session provider type (scope per Q6).
- **Exit:** all ported providers pass a live "hello world" where credentials allow; docs 17 +
  per-provider files.

### Phase 7 — Latency measurement & tuning
- [ ] 7.1 `tools/bench` harness + mock upstream; overhead assertions wired into CI.
- [ ] 7.2 Measure vs direct; profile with `perf`; hunt and remove any allocation, syscall, or
      lock on the hot path that the profile flags.
- [ ] 7.3 Real-provider comparison incl. openadapter `oad/DeepSeek-V4.1-Flash` and command-code.
- [ ] 7.4 Publish numbers in `docs/19-latency-benchmarks.md`.
- **Exit:** overhead budget met and documented honestly, including where we lose.

### Phase 8 — Documentation, polish, publish
- [ ] 8.1 Write/complete every doc file in §4 (including `LLM_INDEX.md`).
- [ ] 8.2 README.md with feature matrix, architecture, quickstart, screenshots.
- [ ] 8.3 `create` public GitHub repo, push, verify the rendered README + docs links resolve.
- [ ] 8.4 Tag `v0.1.0`, add a release with prebuilt binaries if the build is portable.
- **Exit:** repo is public, builds from a clean clone, docs are complete and cross-linked.

---

## 15. Ratified decisions

All questions raised before implementation were answered. These are binding.

| Q | Topic | Decision |
|---|---|---|
| Q1 | Data-plane / control-plane coupling | **Compiled routing snapshot, zero IPC per request.** The relay `mmap`s the snapshot and atomically swaps a pointer; the conductor owns all combo authoring and compilation. |
| Q2 | Hedge winner & window semantics | **The window is an eligibility gate; the winner is the highest-priority participant that replied inside it.** `winner_metric` is user-selectable: `first_response_byte` or `first_complete_token`. Participants are user-selectable. Four failure modes: `rerun-excluding`, `pick-highest-passed`, `pick-highest-passed-different-provider`, `stable-fallback`. See §9. |
| Q3 | Winner dies mid-stream | **Retry with the next best model per the combo's mode and rules.** `midstream_policy` = `restart` (default) or `terminate`; the relay refuses to splice once `tool_calls` deltas have been emitted. See §9.5. |
| Q4 | Reset-aware source of truth | **Live upstream signals first, then user windows override.** Health is tracked **per account**, never per provider: a provider with N accounts is only dead when *all* accounts are dead, and account selection is dynamic and always skips dead accounts. See §11. |
| Q5 | Provider port scope | **All 35 credentialed providers + a working OmniRoute importer** (dry-run first, reading from a copy of the database). |
| Q6 | Cookie / web-scraping providers | **Never supported.** No cookie auth and no reverse-engineered web endpoints. `claude-web`, `gemini-web`, `zai-web`, `deepseek-web`, `duckduckgo-web`, `pollinations`, `aihorde`, `veoaifree-web` are explicitly out of scope and documented as such. |
| Q7 | Control plane language | **Rust (axum)**, living entirely outside the relay, with a minimal host footprint, propagating changes to the relay as fast as possible. The panel follows the greys + single-pink-accent philosophy: greys carry all structure, one pink accent carries emphasis and interaction, and pink is **never** used for destructive or error states — those use green `#4cc38a`, amber `#d9a441`, red `#ef6a6a`. |
| Q8 | Hot-path body handling | **Targeted `model` scan + verbatim byte passthrough**; translation is invoked only when client and upstream dialects differ. |
| Q9 | Concurrency model | **Tokio multi-thread + `SO_REUSEPORT`**; an io_uring backend may be added later as a non-default feature. |
| Q10 | Port change UX | **Persist + a Restart button**, explicitly labelled "applies on next restart", with graceful drain then re-exec. |
| Q11 | Repository name | **`Manarouter`** — a public GitHub repo under `RobertoTornel`. Crates: `mr-core`, `mr-dialects`, `manarouter-relay`, `manarouter-conductor`, `mr-cli`. Config dir `~/.config/manarouter/`. |
| Q12 | Secret storage | **`0600` + encrypted at rest**, referenced by id, never committed, with a pre-commit check and a CI grep that fails the build on key-shaped strings. |
| Q13 | OmniRoute migration | **Import credentials too**, read from a *copy* of `storage.sqlite`, dry-run first, original never modified. |
| Q14 | Testing depth | **CI mock-based tests + an opt-in live smoke harness** using the real credentials on this machine. Every latency number is published honestly. |
| Q15 | Optional capabilities | Session affinity: **in**. Anthropic `/v1/messages` + Responses shims: **in**. Token compression (RTK/caveman): **built as an off-path, default-OFF transformer** — it exists and is enableable per combo, but never runs on the default hot path, which satisfies both the "leave the groundwork" and "make it available" intent. **Plus: v2 groundwork for cross-instance sync** (§18). |

### 15.1 Explicit non-goals for v1

- Cookie and web-scraping providers (Q6).
- Any in-band content transformation on the default request path, including token compression.
- Multi-tenant hosting, billing, or per-user isolation: this is a single-operator, local-first tool.
- A hosted service, SaaS control plane, or remote database.

### 15.2 Recorded answers, verbatim intent

- **Q2:** "First complete token or first non error response body user chooses. Highest model up that replies in the latency window means the model highest in the priority list that fits the latency window chosen by the user." Four failure modes and user-selected participants, as implemented in §9.4.
- **Q3:** "The prompt should be retried with the next best model depending on the mode and rules inside of the combo."
- **Q4:** "Live upstream signals first, then user windows override. Don't class a whole provider as dead if there's multiple api keys/oauth providers if at least one of them is dead. Choosing accounts should be dynamic depending on user choice and should skip dead accounts."
- **Q7:** "Up to your judgement. Must live outside of the router itself and should both communicate changes as fast as possible and take as little resources as possible on the host machine."
- **Q15:** "Make the groundwork for v2 so credentials, combos and settings can be synced between 2 instances in different machines."

---

## 16. Verification plan

- `cargo test --workspace` — unit + integration, no network.
- `tests/e2e_mock_upstream.rs` — spin a mock upstream, drive every surface (chat, responses,
  embeddings, audio×2, images, models, messages, count_tokens), streaming and not, asserting
  byte-level output shape and that passthrough really is passthrough.
- `tests/combo_race.rs` — deterministic races using a mock upstream with scripted per-step delays
  (test 2 releases deadlock/split issues, which is the whole point of the observer patch).
- Snapshot tests: compile N combos, reload, assert the data plane sees the new plan within X ms and
  that zero requests fail during the swap.
- Latency gate in CI: mock upstream, assert added overhead p50 < 150 µs, p99 < 500 µs.
- Live smoke (opt-in, env-gated): openadapter `oad/DeepSeek-V4.1-Flash`, command-code, codex,
  copilot, antigravity — one round trip each, recorded in `docs/19-latency-benchmarks.md`.
- Migration test: point `mr-cli migrate omniroute` at a *copy* of `~/.omniroute/storage.sqlite` and
  assert it extracts 35 providers / 74 connections / 7 combos without touching the original.

## 17. Risks

| Risk | Mitigation |
|---|---|
| Single-stream release build; 46 s cold upstream observed | Pre-warm + fast paths + releasable blame-free latency harness; never bench against a cold provider |
| Streaming the body 1:1 causes unnecessary deadlock/split issues when 2+ combo steps race | Explicitly tested in `tests/combo_race.rs` before the combo engine is considered done |
| Hedging burns quota | Abort losers at first byte; never hedge a non-streaming embeddings/images call |
| OAuth client-ids may be unpublished/undocumented | Research marked uncertain values explicitly; each OAuth platform is independently testable and documentedly replaceable via `OAuthPlatform` |
| 35 providers is a lot to port faithfully | Config-driven generic adapters + generated catalog; only 8 special dialects need native code |
| Panel scope creep | 9 fixed views, no framework, no npm, < 100 KB total |
| Secrets leaking into git | Secrets only under `~/.config/manarouter/secrets/`, `.gitignore`d, and a pre-commit check + CI grep for key-shaped strings |
| Public repo exposing this machine's credentials during migration | Migration writes only to the local config dir; the repo ships templates with placeholders; CI asserts no `sk-`/`ghp_`/`gho_`/JWT-shaped string is ever committed |

---

## 18. v2 groundwork: cross-instance sync (ratified Q15)

The user asked for the groundwork so that **credentials, combos and settings can be synced between
two instances on different machines** in v2. v1 does not ship networking, but it must not make v2
impossible. Therefore v1 commits to these invariants from the very first migration:

1. **Every record has a stable, device-independent id** (UUIDv7), never an autoincrement integer, so
   two instances can merge without collisions. `created_at` and `updated_at` are monotonic UTC.
2. **Every mutable record carries a `revision`** (a monotonic counter) and a `deleted_at` tombstone
   instead of a hard delete, so a merge can tell "changed" from "removed" — a plain delete is
   indistinguishable from "the other side never had it".
3. **Secrets are addressed by id, never inline in the config**, so the sync layer can choose to send
   ciphertext, a wrapped key, or nothing at all without touching any other table.
4. **All writes go through one repository layer** (`mr-core::store`), so a future replication log has
   exactly one choke point to hook.
5. **A `sync` module exists but is inert**: an `oplog` table records `(entity, id, revision, op,
   updated_at, payload_hash)` for every write from day one. v1 never reads it beyond a panel
   "pending changes" counter; v2 replays it. Writing the oplog from day one is what avoids a
   painful backfill later.
6. **The config format is explicitly versioned** (`"format_version": 1`) and forward-compatible:
   unknown keys are preserved on round-trip rather than dropped, so an older instance never destroys
   a newer instance's fields.
7. **Conflicts are last-writer-wins per field for settings, and per record for credentials/combos**,
   with the losing value retained in the oplog so nothing is silently lost. Documented in
   `docs/22-sync-design.md`, which v1 writes even though it implements nothing.

This is cheap in v1 — a UUID, a revision column, tombstones, and one append-only table — and it is
the difference between a v2 sync feature that is a week of work and one that is a rewrite.

---

## 19. Reconciliation: where the plan met reality (added during implementation)

This plan was written from a research dossier before any code existed. Some of it turned out to be
wrong, and the corrections are recorded here rather than quietly applied, because a plan that has
been silently edited to match the outcome is no longer evidence of anything.

### 19.1 The provider count: 28 ship, not 35

**§6 Q5 said "port all 35 credentialed providers". That number cannot be reconstructed, and 28 of
the 45 connected providers in the reference database ship instead.**

Why the original number does not survive contact with the source:

1. The dossier's §6.3 provider list is **truncated** — it ends mid-sentence ("singletons including"),
   so the tail of the list was never captured. Any count derived from it is a lower bound presented
   as a total.
2. The reference count **includes the web-session providers**, which Q6 forbids outright. Six are in
   that group (`claude-web`, `deepseek-web`, `gemini-web`, `zai-web`, `duckduckgo-web`,
   `veoaifree-web`). They are not "not yet done"; they are permanently out of scope by decision.
3. Several remaining entries are not expressible as an HTTP provider at all:
   - `kiro` speaks AWS CodeWhisperer's event-stream protocol, which is not any `Dialect`.
   - `fishaudio` is a bespoke TTS surface with no shared shape.
   - `auggie`, `zcode`, `codex-app-server` and `devin-cli-agentic` are not HTTP services.
   - `cloudflare-playground` is a WebSocket.
   - `combo` is not a provider, `uno` has no registry row, `wawawa` is a user-supplied node.
4. Two more (`aihorde`, `pollinations`) are no-session community proxies that Q6's logic excludes.

The shipped catalogue is **`catalog/providers.toml`, 55 entries**: 4 OAuth platforms, Command Code,
19 major key-based providers, 3 loopback locals (Ollama, llama.cpp, vLLM), 27 providers matched to
the reference database, and 11 additional sources.

**The honest status is therefore 28 of 45 reachable providers implemented, 17 excluded for stated
reasons, and the original "35" unsupportable.** Anything a reader was told based on "35" should be
corrected.

### 19.2 Two providers that migration cannot reach

`kiro` and `fishaudio` are credentialed in the reference database and **cannot be imported**, because
no `Dialect` can express their wire format. The importer must report them as skipped with the reason,
not silently omit them — an operator migrating wants to know what did not come across and why. This
is a documented gap, not a bug.

### 19.3 A real bug the plan did not anticipate: `o-auth`

`AuthKind` derives `#[serde(rename_all = "kebab-case")]`, which renders `AuthKind::OAuth` as
`o-auth`. The catalogue was written against the documented name, `oauth`, and nine tests failed.

The kebab-case rule is right for `Dialect` (`open-ai`), `Capability` (`images.generations`) and the
rest, but it is wrong for an acronym: `o-auth` is not a word anyone would guess, and it reads as a
typo in a hand-edited configuration. The fix is a variant-level `#[serde(rename = "oauth")]`, which
pins the wire name and states it explicitly rather than leaving it an emergent property of a rename
rule. §5's data model should be read with that pin in place.

**The lesson worth keeping:** a derived wire name is an API. It was only caught because the catalogue
and the config schema were written independently and disagreed.

### 19.4 Corrections to specific claims

| Plan said | Reality |
|---|---|
| §12 "compiled snapshot, zero-copy" | Decode builds owned `String`s and `Vec`s once at load. The honest claim is **zero interpretation on the request path**, not literal zero-copy. The snapshot module states this plainly. |
| §4 crates, 5 members | Correct, and `tools/mock-upstream` is a **detached** crate with its own `[workspace]`, so it cannot break `cargo check --workspace`. |
| §13 document list | `docs/01`, `03`, `07`, `16`, `21` and `LLM_INDEX.md` are written. The rest follow. |
| §12 "budget p50 < 150 µs" | Unchanged as the target; not yet measured, because the data plane is still being built. No number will be published before it is measured. |

### 19.5 Process

Work was parallelised across subagents, one file each, with frozen interfaces, which is why the
compile errors were concentrated in integration rather than in the middle of anyone's task. Two
observations worth recording for anyone extending this:

- **Concurrent agents in one crate see each other's in-flight breakage.** Three agents editing
  `mr-core` at once each briefly saw a crate that did not compile. Telling each of them "only your
  file is yours, report other breakage rather than fixing it" was what kept that from becoming
  mutual interference.
- **Agents that died without writing produced no partial file**, so the failure was detectable by
  checking for the file's existence rather than by reading it. Two of nine died this way and were
  restarted.
