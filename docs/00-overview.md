# Overview

Manarouter is a self-hosted AI router: one OpenAI-compatible and one Anthropic-compatible API surface
in front of many upstream providers, with named **combos** that make several provider/model pairs
reachable under a single model name. It is two Rust processes — a data plane that only sends and
receives bytes, and a control plane that makes every routing decision before a request arrives.

---

## 1. The problem it exists to solve

Routing a request through the wrong router is worse than not routing it at all. The router is on the
critical path of every call, it must hold the operator's credentials, and if it is slower than the
providers behind it, the operator pays latency for the privilege of connecting them.

The author measured the router he was replacing, on the development machine, against a purely local
endpoint that does **no upstream work at all**:

| Operation | Measured |
|---|---|
| An existing Javascript router's `GET /v1/models` — local, zero upstream work | **5.2 – 9.2 s** TTFB |
| The same router's zero-upstream-work 429 short-circuit | **22 – 295 ms** |
| The same router's `POST /v1/chat/completions`, cold, before returning 429 | **46 s** |

The first row is the whole argument. `GET /v1/models` reads a list and serialises it. It made no
network call, opened no socket to a provider, and still took seconds. That is not a tuning problem
with a plausible fix; it is what happens when an admin dashboard, a database handle, an OAuth refresh
scheduler and a garbage collector share one event loop with the code that is supposed to answer
instantly. Adding a second worker thread to that design does not remove the coupling, it widens it.

Manarouter's answer is not to make that event loop faster. It is to refuse to put the request on the
same loop as the policy in the first place. Everything that can be decided before a request arrives
is decided in the conductor and compiled into a flat binary **routing snapshot**; the relay reads one
entry out of that table and forwards bytes. This is the design rule, and it is the reason the code is
organised the way it is:

> If a decision can be made before a request arrives, it belongs in the conductor. The relay only
> sends and receives bytes.

---

## 2. The target

The relay's added latency target is **p50 under 150 µs and p99 under 500 µs**. The p50 figure is
within a factor of two of a bare loopback TCP round trip (~0.14 ms measured on the same machine), so
it is close to the floor for any process that talks to a remote provider at all. Below that number,
the provider's own queueing dominates and further router micro-optimisation buys nothing.

The budget is restated per step in [latency engineering](16-latency-engineering.md) §6, and the
techniques that buy each part of it — `memchr` scanning instead of JSON parsing, connection pooling,
`SO_REUSEPORT`, mimalloc, a separate TTFB timeout — are in the same document. The optimisation that
matters most is not on that list: it is that combo evaluation, quota checks, credential refresh and
configuration parsing are not on the request path at all.

---

## 3. The two-process model, in outline

| Process | Crate | Default port | Responsibility |
|---|---|---|---|
| `manarouter-relay` — the **data plane** | `crates/manarouter-relay` | 20900 | Accept, look up a compiled route, select an account, apply client auth/reasoning and forward. Per-key quota caps are enforced, and traffic egresses through a configured proxy when one applies. |
| `manarouter-conductor` — the **control plane** | `crates/manarouter-conductor` | 20901 | Configuration, snapshot compilation, cached provider-quota refresh APIs, OAuth, the web panel. Request-usage ledger recording is not wired. |

The conductor compiles the authored `config.json` into the routing snapshot and publishes it; the
relay holds a `SnapshotHandle` over an `ArcSwap` and hot-swaps it without dropping a request. There
is **zero IPC on the request path** — no per-request call to the conductor, no lock, no parse. A
request that matches a combo does not "ask" anything: it resolves the name to an already-ordered list
of steps and a key strategy, then sends.

This is deliberately the harder of two designs to build, because it splits every feature into "decide
it here" and "read it there". It is the design that survives an outage of the parts that are not
supposed to be on the request path: the panel can be down, the database can be locked, OAuth can be
refreshing, and requests still flow.

The full request path, step by step, is in [architecture](01-architecture.md).

---

## 4. What it is not

| It is not | Because |
|---|---|
| A caching proxy | Two identical prompts must not silently return one answer. Caching changes the semantics of the request, and a router that does it is a proxy that happens to do routing. Response caching is explicitly rejected in [latency engineering](16-latency-engineering.md) §4. |
| A load balancer for your own GPUs | It balances across *providers* and across *accounts of one provider*. If you run one vLLM box and want least-connections in front of it, this is the wrong tool. |
| A hosted service | Both processes bind loopback by default, they hold your credentials, and there is no multi-tenant model, no account system and no billing. It is a program you run. |
| A way around a provider's terms of service | It routes a request to a provider you already have credentials for. It does not pool strangers' accounts, does not evade rate limits, and does not hide that a subscription-backed session is being used through a proxy — see [security](21-security.md). |
| A secret manager for anything but its own providers | The vault holds provider credentials. It is not a general-purpose vault, it has no versioning, and its key lives in the same directory it protects — see [security](21-security.md) §1. |
| A model gateway that rewrites prompts | Translation happens only when the client's dialect differs from the upstream's. A combo may prepend a configured `system_message` and may run an **off-by-default** compression transformer, and that is the entire extent of content modification. |
| A policy engine | There is no plugin language, no per-request conditionals, no rate-limit DSL. Combos, strategies and accounts are the whole vocabulary; if a behaviour is not expressible as a combo, it is not supported. |

---

## 5. Feature matrix

| Feature | Status | Document |
|---|---|---|
| OpenAI surface: chat, responses, embeddings, audio, images, models | implemented (types, dialects, routes) | [LLM_INDEX](LLM_INDEX.md) §5 |
| Anthropic Messages and `count_tokens` | implemented (dialect layer) | [LLM_INDEX](LLM_INDEX.md) §5 |
| Provider catalogue and `mr provider add` templates | implemented | `catalog/providers.toml` |
| Encrypted credential vault (AES-256-GCM, per-entry AAD) | implemented | [security](21-security.md) |
| Authored config load/save/validate, unknown-key preservation | implemented | [configuration](03-configuration.md) |
| Compiled routing snapshot: compile, encode, decode, verify, atomic write | implemented | [routing snapshot](04-routing-snapshot.md) |
| Hot swap through `ArcSwap` | implemented | [routing snapshot](04-routing-snapshot.md) §8 |
| Per-account health states and provider rollup | implemented | [key selection](05-key-selection.md) §6 |
| Key strategies: `priority`, `round-robin`, `random`, `least-used` | implemented (config, compile and relay selection) | [key selection](05-key-selection.md) |
| Combos and the three combo strategies | implemented (schema, compile and relay execution) | [combos](06-combos.md) |
| Hedged race with four winner-fail modes | plan implemented and unit-tested; the relay still runs the sequential step loop, so `hedge_decide` is never called | [the hedged race](07-hedged-race.md) |
| Reset-aware priority | implemented (the selector demotes an exhausted step and restores it once its reset passes) | [combos](06-combos.md) §3 |
| Session affinity for a combo winner | implemented (combo-wide, process-local, default 1800 s window) | [the hedged race](07-hedged-race.md) |
| Quota windows, live signals, cooldowns | cooldowns and live signals implemented in the relay; `/api/quota` reads a `usage` table that nothing writes yet, so it reports nothing | [configuration](03-configuration.md) §3 |
| OAuth login flows | implemented for Codex, Copilot and Antigravity; the `cline` catalogue entry is a migration refusal, not a working login | [security](21-security.md) §5 |
| Web panel | implemented (served by the conductor on 20901) | [architecture](01-architecture.md) §6 |
| `mr` CLI | implemented except `mr vault` and `mr config`, whose bodies still return "not implemented yet" | [quickstart](02-quickstart.md) |
| Relay request loop and listener | implemented | [latency engineering](16-latency-engineering.md) |
| Cross-instance sync | not implemented; design only | [LLM_INDEX](LLM_INDEX.md) §9 |

Status here means "present in this tree", not "shipped". The schema, the snapshot format, both dialect
layers, the provider catalogue, the data plane, the control plane and the panel are in place and
tested. The exceptions are named in the table above — hedged execution, and the two stubbed CLI
bodies. [`23-implementation-status.md`](23-implementation-status.md) holds the audited list.

---

## 6. Who it is for

Someone running **several providers with several keys each**, who wants one endpoint for their tools
and a bounded worst case. Concretely: an operator with two or three paid API keys, one or two
subscription-backed logins, and a client — an editor, an agent, a CLI — that can be pointed at a base
URL. They want "the good model, but do not hang if it is down" and "spread this across the keys so I
do not hit a rate limit at 3pm".

They do **not** need a control plane cluster, a plugin ecosystem or a metrics pipeline. Manarouter
has no opinion about your prompts, does not log them, and will not become a data platform.

---

## 7. Reading order for a newcomer

1. This file — what it is, what it is not, and what "done" means.
2. [Architecture](01-architecture.md) — the two processes and the full request path, with the two
   rejected alternatives and why they are worse.
3. [Quickstart](02-quickstart.md) — a working configuration, end to end.
4. [Configuration](03-configuration.md) — the `config.json` schema, including unknown-key
   preservation and the location of every file.
5. [Routing snapshot](04-routing-snapshot.md) — how that configuration becomes the table the relay
   reads, and why it is a compile step rather than a per-request decision.
6. [Key selection](05-key-selection.md) — the four strategies, and the rule that health is per
   account.
7. [Combos](06-combos.md), then [combo cookbook](combo-cookbook.md) — the feature people actually
   come for, and twelve configurations to copy.
8. [The hedged race](07-hedged-race.md) — the one behaviour most likely to be misread.
9. [Latency engineering](16-latency-engineering.md) and [security](21-security.md) — why the code
   looks like it does, and what it does not protect.

---

## See also

- [Architecture](01-architecture.md) — why there are two processes, and the rejected alternatives
- [Quickstart](02-quickstart.md) — a working configuration in five minutes
- [Configuration](03-configuration.md) — the complete `config.json` schema
- [Routing snapshot](04-routing-snapshot.md) — the compiled table the relay reads
- [Key selection](05-key-selection.md) — the four strategies, and per-account health
- [Combos](06-combos.md) — the combo reference and worked examples
- [Combo cookbook](combo-cookbook.md) — copy-pasteable configurations
- [The hedged race](07-hedged-race.md) — the eligibility gate in detail
- [Latency engineering](16-latency-engineering.md) — every latency decision, and the rejected ones
- [Security](21-security.md) — the threat model, the vault, and the limits
- [LLM_INDEX](LLM_INDEX.md) — the manifest that maps every task to the file that describes it
