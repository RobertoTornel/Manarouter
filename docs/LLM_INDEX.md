# LLM_INDEX — the single file to feed a language model

If you are an LLM working on this repository and can only read one file, read this one. It tells you
what Manarouter is, where everything lives, and which document to read for a given task. It ends with
a machine-readable `MANIFEST` block so you can pick the right file without crawling the filesystem.

**Read this file first, then follow the pointers. Do not guess at an implementation detail from a
filename — the file that documents it is listed below.**

**Two series, two audiences.** The documents in `docs/` are the **build-side** reference: how the
project is put together, what each part does, and where a change belongs. The documents in
`docs/user/` are the **operator and agent** series: how to run the gateway, how to put a provider
behind it, and how to call it. The panel serves the operator series and nothing else, so the two
sets are listed separately in the `MANIFEST` below. If the question is "how do I use this", read
`docs/user/INDEX.md`; if it is "how does this work, or where do I change it", keep reading here.

---

## 1. Project summary

**Manarouter** is a small, fast, self-hosted AI router. It presents one OpenAI-compatible and one
Anthropic-compatible API surface in front of many upstream providers, and it adds named **combos**
(a combo is a *model name* that tries configured provider/model steps in order, and — when the hedge
is enabled — races the chosen steps at once, committing the highest-priority one that answers
inside the eligibility window).

**Language:** Rust, edition 2021, MSRV 1.82. Workspace of five crates.

**Two processes:**

| Process | Crate | Default port | Responsibility |
|---|---|---|---|
| `manarouter-relay` — the **data plane** | `crates/manarouter-relay` | 20900 | Accept a request, look up a compiled route, select an account, translate/forward. Applies relay auth and reasoning policy, enforces per-key quota caps, and egresses through a configured proxy when one applies. A combo with the hedge enabled races its participants: the highest-priority one that answers inside the eligibility window wins, and the losers are dropped. |
| `manarouter-conductor` — the **control plane** | `crates/manarouter-conductor` | 20901 | Configuration, snapshot compilation, quota snapshot refresh APIs, OAuth, and the web panel. Serves the per-request usage ledger at `GET /api/usage`; the token estimate for a provider that reported nothing is computed here, in the panel process. |

`mr serve` runs both binaries as supervised child processes for a foreground session; for an
unattended setup the README documents starting them separately.

The word "compiled" is load-bearing. The conductor compiles combos into a flat binary **routing
snapshot**; the relay memory-maps it and hot-swaps it through an `arc_swap::ArcSwap`. There is **zero
IPC on the request path** — no per-request call to the conductor, no lock, no parse.

**The design rule for every change:** if a decision can be made before a request arrives, it belongs
in the conductor. If it can only be known while bytes are moving, it belongs in the relay. Anything
else is a design bug. Read `docs/01-architecture.md` for the full argument.

**Primary goal:** latency. The relay's own added latency budget is **p50 < 150 µs, p99 < 500 µs** — a
target, not a measurement. The `tools/bench` harness that would assert it in CI does not exist yet,
and nothing has been measured in the relay (`docs/19-latency-benchmarks.md` §3). For contrast, the
measured baseline of an existing Javascript router on the development machine — serving a purely local
`/v1/models` with no upstream work at all — was **5.2 – 9.2 seconds**.

---

## 2. "I want to…" — read these files

| Task | Read |
|---|---|
| Understand the whole system | `docs/01-architecture.md` |
| Help somebody **use** Manarouter | `docs/user/INDEX.md` — the operator and agent series; the panel serves it |
| Change something on the request path | `docs/01-architecture.md`, `docs/16-latency-engineering.md` |
| Add a provider | `docs/providers/README.md`, `catalog/providers.toml`, `crates/mr-core/src/model.rs` |
| Add a dialect or translator | `crates/mr-dialects/src/lib.rs`, `docs/24-provider-dialects.md`, `docs/18-api-reference.md` |
| Change combo behaviour | `docs/06-combos.md`, `docs/07-hedged-race.md`, `crates/mr-core/src/model.rs` |
| Work on the hedged race | `docs/07-hedged-race.md` — read all of it; the semantics are counterintuitive |
| Change account/key selection | `docs/05-key-selection.md`, `crates/mr-core/src/dialect.rs` (`KeyStrategy`) |
| Work on quota | `docs/08-quota-tracking.md`, `docs/09-reset-aware-priority.md` |
| Add an OAuth platform | `docs/14-oauth-adding-a-platform.md`, `docs/omniroute-reference.md` |
| Touch credentials or secrets | `docs/21-security.md` — read before writing any code |
| Change the configuration schema | `docs/03-configuration.md` §1 first — unknown-key preservation is mandatory |
| Change the snapshot format | `docs/04-routing-snapshot.md` — a format change requires a version bump; the current version is **4** |
| Change model naming, reasoning levels, or `/v1/models` | `docs/18-api-reference.md` §"Model addressing", `docs/04-routing-snapshot.md` §5-§7, `crates/mr-core/src/snapshot.rs` (`compile`, `explain_miss`) |
| Work on the panel | `docs/15-web-panel.md`, `docs/01-architecture.md` §5 |
| Migrate from OmniRoute | `docs/17-migrating-from-omniroute.md` |
| Understand latency claims | `docs/16-latency-engineering.md`, `docs/19-latency-benchmarks.md` |
| Debug something | `docs/20-troubleshooting.md` |
| Run the tests | `docs/23-implementation-status.md`, `.github/workflows/ci.yml` |
| Touch cross-instance sync | `docs/22-sync-design.md` — v1 implements nothing; do not add a transport |
| Check what is actually implemented | `docs/23-implementation-status.md` — read before claiming a feature works |

---

## 3. Repository map

```
Router/
├── Cargo.toml                     workspace: 5 members, shared dependency versions
├── rust-toolchain.toml            stable + rustfmt + clippy
├── README.md
├── catalog/
│   └── providers.toml             ★ provider seed data, compiled in via include_str!
├── docs/                          ★ one file per feature; LLM_INDEX.md is this file
├── panel/                         ★ the browser UI, served from disk by the conductor (no build step)
├── tests/                         Python smoke and local-benchmark checks (run by CI)
├── scripts/
│   └── check-secrets.sh           ★ CI guard: fails on a credential-shaped string
├── tools/
│   ├── mock-upstream/             deterministic fault-injecting fake provider (detached crate)
│   └── bench/                     planned; no benchmark harness exists yet
└── crates/
    ├── mr-core/                    shared types; no network code at all
    │   └── src/
    │       ├── lib.rs              constants: ports, format versions, snapshot magic
    │       ├── dialect.rs          ★ Dialect, Capability, RouteKind, AuthKind, all strategy enums
    │       ├── model.rs            ★ Provider, Account, Combo, ComboStep, HedgeSpec, Alias, ClientKey
    │       ├── ids.rs              UUIDv7 ids, Millis, fnv1a64
    │       ├── config.rs           the authored configuration: load/save/validate/atomic write
    │       ├── secret.rs           ★ the encrypted vault (AES-256-GCM, AAD-bound entries)
    │       ├── snapshot.rs         ★ the compiled routing table: compile/encode/decode/ArcSwap handle
    │       ├── store.rs            SQLite runtime state: health, quota, usage, oplog
    │       ├── catalog.rs          loads catalog/providers.toml into ProviderTemplates
    │       ├── error.rs            Error enum
    │       ├── published.rs        ★ OAuth public client constants, assembled from fragments
    │       └── oauth/              ★ per-platform login/token/refresh (codex, copilot, antigravity)
    ├── mr-dialects/                translation; nothing here runs when dialects match
    │   └── src/
    │       ├── lib.rs              ★ the dispatch + TranslateCtx contract
    │       ├── openai.rs           the Chat Completions pivot + Anthropic→pivot response direction
    │       ├── anthropic.rs        pivot → Anthropic Messages request direction
    │       ├── responses.rs        OpenAI Responses API (Codex)
    │       ├── google.rs           Gemini generateContent
    │       ├── commandcode.rs      Command Code alpha/generate. Its stream state is **owned per
    │       │                       stream** (`commandcode::Translator`), not a thread-local: two
    │       │                       concurrent answers on one worker used to splice tool calls
    │       │                       together. `translate(translator, ctx, data)` is the entry point
    │       │                       the relay uses; `sse_to_pivot` is the legacy per-thread shim.
    │       └── sse.rs              ★ text/event-stream framing (Framer, Frame, data_frame)
    ├── manarouter-relay/           the data plane (lib + bin)
    │   └── src/
    │       ├── lib.rs              global allocator (mimalloc) + module map
    │       ├── main.rs             thin binary
    │       ├── startup.rs          ★ load config, resolve bind, open vault, build state (shared with `mr serve`)
    │       ├── server.rs           ★ the HTTP route table, request path and accept loop
    │       ├── count_tokens.rs     the local token-count estimate (Anthropic's route, other providers' answer)
    │       ├── select.rs           ★ step/account selection and the hedged-race policy (pure functions)
    │       ├── upstream.rs         pooled upstream client, the four timeouts, streaming passthrough
    │       ├── state.rs            snapshot handle, upstream client, client keys
    │       ├── sticky.rs           bounded process-local successful-step preference
    │       └── model_scan.rs       ★ finds "model" without parsing JSON; the biggest hot-path win
    ├── manarouter-conductor/       the control plane + panel (lib + bin)
    └── mr-cli/                     the `mr` command line
```

★ marks the files that matter most.

---

## 4. The request path in nine steps

```
client ──► relay :20900
             ├─ 1. accept                  one listener per CPU, SO_REUSEPORT
             ├─ 2. authenticate            SHA-256 the client key, compare against the snapshot
             ├─ 3. scan for "model"        memchr over the raw body; NO full JSON parse
             ├─ 4. resolve                 one hash lookup in the snapshot index
             ├─ 5. pick a plan             pre-compiled steps, account order, hedge plan
             ├─ 6. socket                  pooled keep-alive connection
             ├─ 7. forward bytes           VERBATIM when the dialects match
             ├─ 8. pipe the response       verbatim, streaming, never buffered whole
             └─ 9. emit usage              fire-and-forget; dropped rather than backpressured
```

Dialect translation is reachable per selected step when the client and upstream shapes differ; translated requests may override the path or force streaming. Matching dialects keep the original bytes. The hedged race runs: participants are dispatched concurrently and the highest-priority one that answers inside the eligibility window wins. The winner's recovery runs too: the relay watches the winner's body and falls back to a participant that passed the same window — silently before any byte reached the client, with the `manarouter.retry` boundary after, and never after a tool-call delta. `docs/16-latency-engineering.md` §6 states the per-step budget.

---

## 5. Route table

### The relay (client-facing, OpenAI-compatible unless noted)

| Method | Path | `RouteKind` | Dialect served |
|---|---|---|---|
| POST | `/v1/chat/completions` | `ChatCompletions` | OpenAI |
| POST | `/v1/responses` | `Responses` | OpenAI Responses |
| POST | `/v1/embeddings` | `Embeddings` | OpenAI |
| POST | `/v1/audio/transcriptions` | `AudioTranscription` | OpenAI (multipart) |
| POST | `/v1/audio/speech` | `AudioSpeech` | OpenAI |
| POST | `/v1/images/generations` | `ImagesGenerations` | OpenAI |
| GET | `/v1/models` | `Models` | OpenAI; served entirely from the compiled snapshot, so it lists combos, combo `-<level>` names, `provider/model` and the `provider/model-<level>` names each model really offers |
| POST | `/v1/messages` | `Messages` | **Anthropic** |
| POST | `/v1/messages/count_tokens` | `CountTokens` | **Anthropic** |
| GET | `/healthz` | — | liveness only, never routed upstream |

An Anthropic client points `ANTHROPIC_BASE_URL` at the relay; an OpenAI client points
`OPENAI_BASE_URL` at it. Both work because the path distinguishes them.

`GET /v1/models` contacts **no upstream**. It is served entirely from the compiled snapshot, which is
what makes it a usable first check: each combo name, each combo name with a level suffix,
`provider/model` for every visible model, and `provider/model-<level>` for every level that model
genuinely offers. A model with no reasoning levels is listed exactly once, under its own id, and a
model whose id already ends in a level word gets no suffixes at all. Each provider entry carries a
`manarouter` object with `kind`, `dialect`, `default_reasoning_level`, `reasoning_levels` and
`input_modalities`; a level-qualified entry also carries `reasoning_level`.

### The conductor (control plane, `/api/…`)

Configuration CRUD, provider and account management, combo editing, the port editor, OAuth login
flows, quota views, usage queries, model discovery, and the panel's static assets. Never a proxy
route. Documented in `docs/18-api-reference.md`.

---

## 6. Enum vocabularies

Exhaustive lists. With the one exception noted under `RouteKind`, these are the exact strings
accepted in configuration and returned by the API. Anything not listed is a validation error.

### `Dialect` — how a provider speaks
`open-ai` · `anthropic` · `google` · `codex` · `copilot` · `antigravity` · `command-code-go`

Passthrough pairs include `open-ai → open-ai`, `anthropic → anthropic`, and OpenAI chat to Copilot's OpenAI-shaped surface. OpenAI chat to Antigravity is **not** a body passthrough: it uses the Google/Antigravity translator and still lacks OAuth/project-context completion.

### `Capability` — what a provider or model can serve
`chat` · `responses` · `embeddings` · `audio-transcription` · `audio-speech` · `images-generations` ·
`models` · `messages` · `tool-calling` · `reasoning` · `vision`

These are the `serde` spellings, so a `config.json` or `catalog/providers.toml` entry must use
`audio-transcription`, **not** `audio.transcription`. The parser behind the `--capability` flag also
accepts the dotted and short forms (`audio.transcription`, `stt`, `tts`, `images`), and validation
messages may print them, but the dotted form is rejected by config deserialization.

### `RouteKind` — a client-facing endpoint
`chat.completions` · `responses` · `embeddings` · `audio.transcription` · `audio.speech` ·
`images.generations` · `models` · `messages` · `count.tokens`

These dotted spellings are canonical for a client key's `allow_routes`, where the CLI normalises
aliases to this form. Where a `RouteKind` itself is serialised — the catalog's `route_overrides`
table — the `serde` kebab spelling is used instead (`chat-completions`, `count-tokens`).

### `AuthKind` — how a credential is attached
`bearer` · `api-key-header` (with `header`) · `header` (with `name`, `prefix`) · `query-param` (with
`param`) · `oauth` (with `platform`)

**There is no cookie or web-session variant, deliberately, and adding one is prohibited.** See
`docs/21-security.md` §4.

### `KeyStrategy` — how an account is chosen within a provider
`priority` · `round-robin` · `random` · `least-used`

All four skip unusable accounts. Account health is never conflated with provider health.

### `ComboStrategy` — how a step is chosen when not hedging
`priority` · `round-robin` · `reset-aware-priority`

### `WinnerMetric` — what counts as "replied" in a race
`first-response-byte` (default) · `first-complete-token`

### `WinnerFailMode` — what happens when the chosen model dies
`rerun-excluding` · `pick-highest-passed` · `pick-highest-passed-different-provider` ·
`stable-fallback`

### `MidstreamPolicy` — what happens when the winner dies mid-stream
`restart` (default) · `terminate`

Hard guard under **both**: once a `tool_calls` delta has been emitted, the relay refuses to splice
and terminates with an error. A corrupted tool call is worse than a clean failure.

### `AccountState` — observed state of one account
`ready` · `disabled` · `rate-limited` · `backoff` · `auth-failed` · `quota-exhausted`

### `ProviderHealth` — a rollup, not a state
`unconfigured` (0 enabled accounts) · `healthy` (all usable) · `degraded` (some usable) ·
`dead` (none usable)

Derived by `ProviderHealth::from_counts(total, usable)`. Never stored.

### `AffinityMode` — how a conversation is identified
`first-system-and-user` · `header-only` · `header-or-content` · `off`

### `WindowKind` — a quota window's reset behaviour
`rolling` · `fixed` · `provider-declared`

### `ServiceTier` / reasoning effort
This section used to say the relay passes a provider's reasoning effort through untouched. That is no
longer true, and the change is deliberate: reasoning is now a **routing** concept.

- The canonical ladder, ascending, is `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `ultra`,
  `max`. `xhigh` also accepts the spellings `x-high`, `extra-high`, `extra_high` and `extrahigh`.
- A model name may end in a `-<level>` suffix. Which levels a model offers is compiled into the
  snapshot: the operator's explicit `reasoning_levels` wins, else the bundled table
  `catalog/model_meta.json` (keyed `provider/model`, falling back to a provider-agnostic `model`
  key), else the generic set `none, low, medium, high, xhigh, max` when the model or provider
  declares the `reasoning` capability (or the dialect serves reasoning models only, currently
  `codex`). A model that reaches none of those has **no** levels and its name gets no suffix.
- The router rewrites the upstream body per dialect: `reasoning_effort` for OpenAI-shaped dialects,
  `reasoning.effort` for the Responses dialect, `thinking.budget_tokens` for Anthropic, and
  `generationConfig.thinkingConfig` for Google/Antigravity. Level `none` emits no reasoning field at
  all, on every dialect. A per-model `reasoning_wire` map replaces the dialect default **wholesale**
  for the levels it names, and `null` at a level means "send nothing".
- **A client's own explicit reasoning field is never overwritten.** `docs/18-api-reference.md`
  §"Model addressing" has the full table.

---

## 7. Where the invariants live

These are the properties most likely to be broken by a well-meaning change. Each is enforced by a
test; if you change the behaviour, change the test deliberately and say so.

| Invariant | Enforced in |
|---|---|
| A request is never dropped because the configuration changed | `snapshot.rs` `SnapshotHandle` tests |
| Migration is idempotent; opening the database twice is safe | `store.rs` tests |
| Unknown config keys survive a load/save round trip | `config.rs` `unknown_keys_are_preserved_across_a_round_trip` |
| `extra` fields use `#[serde(flatten)]` — without it, keys are silently lost | `model.rs` round-trip test |
| A credential never appears in `config.json` or `state.sqlite` | `config.rs`, `store.rs`, and `scripts/check-secrets.sh` |
| No nonce is ever reused under AES-GCM | `secret.rs` `each_write_uses_a_fresh_nonce` |
| A ciphertext cannot be moved between vault entries | `secret.rs` `ciphertexts_cannot_be_swapped_between_entries` |
| Identical dialects cause no translation work at all | `crates/mr-dialects/src/lib.rs` `identical_dialects_need_no_translation_at_all` |
| A body that is not JSON is still forwarded | `crates/mr-dialects/src/lib.rs` `passthrough_works_for_a_body_that_is_not_even_json` |
| A `model` inside a string or a nested object is not matched | `model_scan.rs` tests |
| Step order is never reordered | `config.rs` `providers_are_ordered_as_the_operator_wrote_them` |
| A whole provider is never marked dead over one bad account | `model.rs` health tests |
| No provider uses cookie auth | `catalog.rs` test |

**Feature 3 properties that currently rest on code, not on a test.** These are real behaviours of the
compiler and the relay, but as of this writing no test names them, so a change could break one without
CI noticing. If you touch naming or levels, add the test alongside the change:

- An exact model id beats a `base-level` reading of it (`snapshot.rs` `compile`, rank 3 versus rank 5).
- An out-of-range level reroutes to the model's highest instead of failing (`compile`, the
  `RANK_QUALIFIED_LEVEL` pass).
- A client's own explicit reasoning field is never overwritten (`server.rs` `merge_reasoning` leaves an
  existing scalar alone).

---

## 8. Things that will get a change rejected

1. **Parsing the request body on the hot path** when `model_scan` would do.
2. **Calling the conductor from the relay** for any reason. There is no IPC on the request path, by
   design, and adding one is the single change most likely to destroy the project's reason to exist.
3. **Adding a cookie or web-session auth kind.** Ratified as never.
4. **Reordering combo steps.** The operator's order is the tie-breaker; nothing sorts it.
5. **Buffering a streamed response.** Passthrough is what keeps latency flat and memory bounded.
6. **`unwrap()` on the request path.** `panic = "abort"` means a panic kills the process.
7. **Removing `#[serde(flatten)] extra`** from a configuration struct. That turns every future
   downgrade into silent data loss.
8. **Adding an unknown-key-destroying parse.** Same reason.
9. **Spending a provider's rate-limit reset credits.** One reference implementation does; it consumes
   an allowance the user may have wanted, and it is deliberately not implemented.
10. **Documenting a mock benchmark number as if it were a real-provider number.** They are always
    reported separately. See `docs/19-latency-benchmarks.md`.

---

## 9. Ratified decisions

These came from the project owner and are binding. Do not relitigate them in a change; if one seems
wrong, say so explicitly rather than quietly working around it.

| # | Decision |
|---|---|
| Q1 | Compiled snapshot, **zero IPC per request** |
| Q2 | The winner of a hedge is the **highest-priority participant that replied inside the latency window**; four `on_winner_fail` modes; the operator may choose the participants |
| Q3 | On failure the prompt is retried with the next best model according to the combo's mode and rules |
| Q4 | Live upstream signals first, user-declared windows override; **never** mark a whole provider dead while any of its accounts is usable; account selection skips dead accounts |
| Q5 | Port all 35 credentialed OmniRoute providers, plus an importer |
| Q6 | **Never support cookie authentication, at all** |
| Q7 | The panel lives outside the router, communicates changes as fast as possible, and uses as little of the host as possible |
| Q8 | Targeted `model` scan + verbatim passthrough |
| Q9 | tokio multi-thread + `SO_REUSEPORT` |
| Q10 | Persist a port change and apply it on **next restart**, with a Restart button |
| Q11 | Repo `Manarouter`; crates `mr-core`, `mr-dialects`, `manarouter-relay`, `manarouter-conductor`, `mr-cli`; config in `~/.config/manarouter/` |
| Q12 | Secrets `0600`, encrypted at rest, referenced by id, with a CI grep for key-shaped strings |
| Q13 | Import credentials from a **copy** of OmniRoute's database, dry-run first, original untouched |
| Q14 | CI mock tests plus an opt-in live smoke test with real credentials |
| Q15 | Session affinity in; Anthropic `/v1/messages` and Responses shims in; token compression as an off-path, default-**OFF** transformer; plus v2 groundwork for cross-instance sync |

---

## 10. Glossary

- **Account** — one credential for a provider. A provider with three API keys has three accounts, and
  health, quotas and rotation are tracked **per account**.
- **Combo** — a named model that resolves to an ordered list of provider/model steps. Clients call it
  exactly like a model name. A combo name may carry a `-<level>` suffix (`coding-high`), which pins
  every step to that tier; a combo named with no suffix runs each step at that step's own default.
- **Reasoning level** — one rung of the ladder `none`, `minimal`, `low`, `medium`, `high`, `xhigh`,
  `ultra`, `max`, optionally written as a `-<level>` suffix on a model or combo name. Compiled into
  the snapshot and merged into the upstream body, never parsed per request.
- **Conductor** — the control-plane process. Owns configuration and compile-time policy; relay applies request-time auth and reasoning behavior.
- **Dialect** — the wire shape a client or provider speaks. Same word as the `Dialect` enum.
- **Eligibility gate** — the hedged race's latency window. It decides *priority protection*, not who
  wins outright. See `docs/07-hedged-race.md` §3.
- **Hedge** — sending one prompt to several models at once. The winner is the highest-priority
  participant that replied inside the window.
- **Passthrough** — forwarding request or response bytes unchanged because the dialects match.
- **Relay** — the data-plane process. Applies client authentication and reasoning-level behavior, but does not yet enforce per-key quotas or route upstream connections through configured proxies.
- **Snapshot** — the compiled binary routing table the relay reads.
- **Step** — one provider/model pair inside a combo, at a fixed position in the operator's order.
- **TTFB** — time to first byte. The metric the hedged race keys on, and a separate timeout from the
  total request timeout.

---

## 11. MANIFEST

Machine-readable. Each entry is one document; `purpose` is what it answers, `read_when` is the
trigger.

```json
{
  "project": "Manarouter",
  "version_file": "crates/mr-core/src/lib.rs",
  "entry_point_for_llms": "docs/LLM_INDEX.md",
  "documents": [
    { "path": "docs/00-overview.md", "purpose": "What Manarouter is, who it is for, and what it deliberately does not do", "read_when": "starting from zero" },
    { "path": "docs/01-architecture.md", "purpose": "Why there are two processes, what the compiled snapshot is, and the full request path", "read_when": "changing anything structural" },
    { "path": "docs/02-quickstart.md", "purpose": "A working configuration in five minutes", "read_when": "installing or writing a test harness" },
    { "path": "docs/03-configuration.md", "purpose": "The complete config.json schema, plus why unknown keys are preserved", "read_when": "changing any config field" },
    { "path": "docs/04-routing-snapshot.md", "purpose": "The compiled binary table: layout (format version 4), level-aware compilation, index precedence, versioning, hot swap", "read_when": "changing the snapshot format or the relay's lookup" },
    { "path": "docs/05-key-selection.md", "purpose": "priority, round-robin, random and least-used, and how dead accounts are skipped", "read_when": "touching account selection" },
    { "path": "docs/06-combos.md", "purpose": "The combo reference and worked examples", "read_when": "changing combo semantics" },
    { "path": "docs/07-hedged-race.md", "purpose": "The eligibility gate, the four on-failure modes, midstream policy, session affinity", "read_when": "touching the race — read it all" },
    { "path": "docs/08-quota-tracking.md", "purpose": "How consumption is observed, recorded and surfaced", "read_when": "touching accounting" },
    { "path": "docs/09-reset-aware-priority.md", "purpose": "How cooldowns demote rather than remove a step, and how resets restore it", "read_when": "touching reset-aware behaviour" },
    { "path": "docs/10-oauth-overview.md", "purpose": "The shared OAuth machinery: PKCE, loopback, device flow, refresh, single-flight", "read_when": "touching any login flow" },
    { "path": "docs/11-oauth-codex.md", "purpose": "The OpenAI Codex login and inference fingerprint", "read_when": "working on Codex" },
    { "path": "docs/12-oauth-antigravity.md", "purpose": "The Antigravity login, project onboarding and Cloud Code inference", "read_when": "working on Antigravity" },
    { "path": "docs/13-oauth-copilot.md", "purpose": "The GitHub Copilot device flow and mandatory headers", "read_when": "working on Copilot" },
    { "path": "docs/14-oauth-adding-a-platform.md", "purpose": "A worked example of adding a new OAuth platform", "read_when": "adding a platform" },
    { "path": "docs/omniroute-reference.md", "purpose": "Reference notes on the author's own OmniRoute install: the provider-flow inventory the catalogue was built from, and the client-key rotation scope", "read_when": "reading a catalogue note's provenance or the client-key rotation scope" },
    { "path": "docs/15-web-panel.md", "purpose": "The panel: views, API, port editor, design language", "read_when": "touching the UI" },
    { "path": "docs/16-latency-engineering.md", "purpose": "Every latency decision, the rejected ones, and the per-step budget", "read_when": "changing the hot path" },
    { "path": "docs/17-migrating-from-omniroute.md", "purpose": "Importing providers, combos and credentials safely", "read_when": "migrating or touching the importer" },
    { "path": "docs/18-api-reference.md", "purpose": "Every route on both surfaces, with request and response shapes, model addressing with reasoning levels, and the /v1/models listing", "read_when": "adding or calling an endpoint" },
    { "path": "docs/19-latency-benchmarks.md", "purpose": "Measured numbers, mock and real, kept strictly separate", "read_when": "making a performance claim" },
    { "path": "docs/20-troubleshooting.md", "purpose": "Symptom, cause, fix", "read_when": "something is broken" },
    { "path": "docs/21-security.md", "purpose": "Threat model, the vault, cookie prohibition, ToS posture", "read_when": "before writing credential code" },
    { "path": "docs/22-sync-design.md", "purpose": "The intended cross-instance sync design; v1 implements nothing", "read_when": "touching ids, revisions or tombstones" },
    { "path": "docs/23-implementation-status.md", "purpose": "What actually runs today, with the command that checks each claim", "read_when": "before claiming a feature is implemented" },
    { "path": "docs/24-provider-dialects.md", "purpose": "The seven wire dialects: what each upstream speaks, which client routes reach it, and what the relay translates", "read_when": "adding a dialect or tracing a translation" },
    { "path": "docs/combo-cookbook.md", "purpose": "Copy-pasteable combo configurations for common goals", "read_when": "building a combo" },
    { "path": "docs/credits.md", "purpose": "Attribution for the quota fetchers derived from OmniRoute", "read_when": "checking licensing or provenance" },
    { "path": "docs/providers/README.md", "purpose": "How the provider catalogue works and how to add an entry", "read_when": "adding a provider" },
    { "path": "docs/providers/catalog.md", "purpose": "Generated: every provider, its dialect, auth and base URL", "read_when": "looking up a provider" },
    { "path": "docs/providers/anthropic-compatible.md", "purpose": "Providers that serve the Anthropic Messages API natively", "read_when": "routing to an Anthropic-shaped upstream" },
    { "path": "docs/providers/openai-compatible.md", "purpose": "Providers that serve the OpenAI chat surface, and the base-URL convention", "read_when": "routing to an OpenAI-shaped upstream" },
    { "path": "docs/providers/commandcode.md", "purpose": "The Command Code CLI envelope, its event stream and its fingerprint headers", "read_when": "working on Command Code" },
    { "path": "docs/providers/codex.md", "purpose": "The Codex provider: OAuth, fingerprint headers and the Responses surface", "read_when": "working on Codex" },
    { "path": "docs/providers/antigravity.md", "purpose": "The Antigravity provider: tiers, Cloud Code onboarding and scopes", "read_when": "working on Antigravity" },
    { "path": "docs/providers/copilot.md", "purpose": "The GitHub Copilot provider: the device flow and the mandatory fingerprint headers", "read_when": "working on Copilot" },
    { "path": "docs/local-latency-benchmark.md", "purpose": "The report-only local benchmark method and its caveats", "read_when": "running tests/local_latency_bench.py" },
    { "path": "docs/user/INDEX.md", "purpose": "The operator and agent series: what it covers, in which order to read it, and how the panel serves it", "read_when": "pointing somebody at the user-facing documentation" },
    { "path": "docs/user/00-getting-started.md", "purpose": "From a fresh install to the first request through the gateway", "read_when": "setting Manarouter up" },
    { "path": "docs/user/01-add-a-provider.md", "purpose": "Choosing an upstream, storing its credential safely, confirming reachability", "read_when": "adding a provider" },
    { "path": "docs/user/02-client-keys.md", "purpose": "Creating and protecting the client key an app sends, and keeping it separate from the provider credential", "read_when": "wiring an app to the gateway" },
    { "path": "docs/user/03-calling-the-api.md", "purpose": "The two client surfaces, request examples, and how to read the first response", "read_when": "calling the relay" },
    { "path": "docs/user/04-models-and-reasoning-levels.md", "purpose": "Which model names the relay resolves, and how to request a reasoning level", "read_when": "choosing a model name" },
    { "path": "docs/user/05-combos.md", "purpose": "Creating and maintaining named model routes over several providers", "read_when": "building a combo" },
    { "path": "docs/user/06-quota.md", "purpose": "Configured quota windows, transient cooldowns, and the usage data the panel does not yet show", "read_when": "setting a quota" },
    { "path": "docs/user/07-images-audio-video.md", "purpose": "The media routes and which providers accept them", "read_when": "calling an image, audio or video route" },
    { "path": "docs/user/08-proxies.md", "purpose": "Sending upstream traffic through a proxy", "read_when": "configuring egress" },
    { "path": "docs/user/09-pricing.md", "purpose": "What a request costs upstream and how the relay counts it", "read_when": "estimating cost" },
    { "path": "docs/user/10-troubleshooting.md", "purpose": "The failures an operator hits most, and what causes each", "read_when": "something is failing for a user" },
    { "path": "docs/user/11-security.md", "purpose": "The vault, the panel's access controls, and what must not be exposed", "read_when": "reviewing a deployment" },
    { "path": "docs/user/12-cli-reference.md", "purpose": "Every mr command with its flags", "read_when": "looking up a command" }
  ],
  "machine_readable_state": {
    "compiled_snapshot": "crates/mr-core/src/snapshot.rs",
    "provider_catalog_data": "catalog/providers.toml",
    "config_schema": "crates/mr-core/src/config.rs",
    "domain_model": "crates/mr-core/src/model.rs",
    "enum_vocabulary": "crates/mr-core/src/dialect.rs",
    "dialect_dispatch": "crates/mr-dialects/src/lib.rs",
    "hot_path_scanner": "crates/manarouter-relay/src/model_scan.rs",
    "secret_guard": "scripts/check-secrets.sh",
    "ci": ".github/workflows/ci.yml"
  }
}
```
