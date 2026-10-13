# Manarouter

A small, fast, self-hosted AI router. One OpenAI-compatible and one Anthropic-compatible API in
front of many providers, with named **combos** that fan a prompt across several models at once.

Written in Rust. Two processes. No JavaScript on the request path, no admin dashboard sharing an
event loop with your traffic.

```
your client ──► manarouter-relay :20900 ──► the provider you configured
                       ▲
                       │ compiled routing table (zero IPC per request)
                       │
              manarouter-conductor :20901 ──► web panel
```

---

## Why this exists

The author was using an existing router and measured it. Serving `/v1/models` — an endpoint that does
**no network work at all**, just reads a list and serialises it — took **5.2 to 9.2 seconds**.
A short-circuit that returns 429 without contacting anything took up to 295 ms. The router was slower
than the providers it was routing to.

Manarouter is built around one rule:

> If a decision can be made before a request arrives, make it in the conductor. The relay only sends
> and receives bytes.

Combos, account ordering, health, quota, fallback rules — all compiled ahead of time into a flat
binary table the relay reads with a single atomic load. Adding a provider or editing a combo does not
touch the request path.

**The relay's total added latency target is under 150 µs at p50 and 500 µs at p99**, verified in CI
against a deterministic mock upstream. See [`docs/16-latency-engineering.md`](docs/16-latency-engineering.md).

---

## What it does

| | |
|---|---|
| **APIs** | OpenAI Chat Completions, Responses, Embeddings, Audio (transcription and speech), Images, Models — and Anthropic Messages with `count_tokens` |
| **Providers** | Any OpenAI-compatible endpoint, any Anthropic-compatible endpoint, plus first-class adapters for OpenAI Codex, GitHub Copilot, Google Antigravity and Command Code |
| **Combos** | A combo *is a model name*. Order it yourself; race it or don't |
| **Key selection** | `priority`, `round-robin`, `random`, `least-used` — always skipping dead accounts |
| **Health** | Per **account**, never per provider. One revoked key does not take a provider down |
| **Quota** | Live upstream signals first, your declared windows as an override |
| **OAuth** | Codex, Copilot, Antigravity — login from the panel; adding a platform is a documented trait |
| **Panel** | A small web UI for providers, accounts, combos and ports, on its own process |
| **Migration** | Imports providers, combos and credentials from an OmniRoute database — from a **copy** |

---

## The hedged race, in one paragraph

A combo can send one prompt to several models at once. The winner is **not** whichever answers first
— it is the **highest-priority participant that replied within your latency window**. The window is an
eligibility gate, not a stopwatch: if your preferred model answers at 600 ms and a lower-ranked one at
200 ms, you get your preferred model. The race exists to bound the *worst* case, not to chase the best
case. If your preferred model is healthy, the race resolves the moment it replies and costs you
nothing. Four modes decide what happens when the winner dies — re-race without it, take the best that
passed, take the best that passed *on a different provider*, or fall back to a model you nominate.

This surprises people, so it is documented in full, with worked timelines:
[`docs/07-hedged-race.md`](docs/07-hedged-race.md).

---

## Quick start

```bash
git clone https://github.com/RobertoTornel/Manarouter
cd Manarouter
cargo build --release
```

Add a provider and a key:

```bash
./target/release/mr provider add openai --dialect open-ai --base-url https://api.openai.com/v1
./target/release/mr account add openai --label personal --secret sk-...
./target/release/mr models sync openai
```

Make a combo that is a model name:

```bash
./target/release/mr combo add Brains \
  --step openai/gpt-4o \
  --step anthropic/claude-sonnet-4-5 \
  --strategy priority
```

Run it:

```bash
./target/release/mr serve          # relay on 20900, conductor + panel on 20901
```

Point anything at it:

```bash
export OPENAI_BASE_URL=http://127.0.0.1:20900/v1
export OPENAI_API_KEY=$(./target/release/mr key create --label my-client --print)

curl $OPENAI_BASE_URL/chat/completions \
  -H "authorization: Bearer $OPENAI_API_KEY" \
  -d '{"model":"Brains","messages":[{"role":"user","content":"hello"}]}'
```

Full walkthrough: [`docs/02-quickstart.md`](docs/02-quickstart.md).

---

## Documentation

**If you are a language model working on this repository, read
[`docs/LLM_INDEX.md`](docs/LLM_INDEX.md).** It is the single file that maps every task to the files
that describe it, and it ends with a machine-readable manifest.

For humans:

| Document | |
|---|---|
| [Overview](docs/00-overview.md) | What this is and is not |
| [Architecture](docs/01-architecture.md) | Why two processes, and the full request path |
| [Quickstart](docs/02-quickstart.md) | Working in five minutes |
| [Configuration](docs/03-configuration.md) | The complete `config.json` schema |
| [Routing snapshot](docs/04-routing-snapshot.md) | The compiled table |
| [Key selection](docs/05-key-selection.md) | Four strategies, and skipping dead accounts |
| [Combos](docs/06-combos.md) | Reference and worked examples |
| [The hedged race](docs/07-hedged-race.md) | The eligibility gate, in detail |
| [Quota tracking](docs/08-quota-tracking.md) | Signals, windows, cooldowns |
| [Reset-aware priority](docs/09-reset-aware-priority.md) | Demote, never remove |
| [OAuth](docs/10-oauth-overview.md) · [Codex](docs/11-oauth-codex.md) · [Antigravity](docs/12-oauth-antigravity.md) · [Copilot](docs/13-oauth-copilot.md) · [Adding a platform](docs/14-oauth-adding-a-platform.md) | Logins |
| [Web panel](docs/15-web-panel.md) | Views, API and design |
| [Latency engineering](docs/16-latency-engineering.md) | Every decision, and the rejected ones |
| [Migrating from OmniRoute](docs/17-migrating-from-omniroute.md) | Providers, combos, credentials |
| [API reference](docs/18-api-reference.md) | Every route on both surfaces |
| [Benchmarks](docs/19-latency-benchmarks.md) | Measured numbers |
| [Troubleshooting](docs/20-troubleshooting.md) | Symptom, cause, fix |
| [Security](docs/21-security.md) | Threat model, the vault, and the things it does not protect |
| [Sync design](docs/22-sync-design.md) | Planned, not implemented |
| [Combo cookbook](docs/combo-cookbook.md) | Copy-pasteable configurations |

---

## Security posture, stated plainly

- Credentials are encrypted at rest with AES-256-GCM, each ciphertext bound to its own id so it
  cannot be moved between entries. Only non-reversible fingerprints are ever displayed.
- **Cookie and web-session authentication is not supported at all.** No provider uses it. This is a
  hard constraint, enforced by a test.
- The panel binds to loopback by default and requires a password to be reachable from anywhere else.
- `scripts/check-secrets.sh` fails the build on a credential-shaped string, and is run in CI.

Two things it does **not** do: it does not protect credentials from someone who can already read
files as your user, and it does not hide the fact that routing a subscription-backed session through
a proxy is not a use the provider has licensed. Both are explained in
[`docs/21-security.md`](docs/21-security.md).

---

## Status

Under active construction. The schema, the compiled snapshot format, both dialect layers and the
provider catalogue are in place and tested; the data plane, control plane and panel are being wired
up.

The design decisions in this repository are recorded, not folklore. [`docs/LLM_INDEX.md`](docs/LLM_INDEX.md)
§9 lists them, and `docs/LLM_INDEX.md` §8 lists the changes that will be rejected and why.

---

## Contributing

`cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace` and
`scripts/check-secrets.sh` must all pass. CI runs them on Linux and macOS.

If you are changing something on the request path, read
[`docs/16-latency-engineering.md`](docs/16-latency-engineering.md) first — including §4, which lists
the plausible optimisations that were deliberately rejected and why.

---

## Licence

MIT. See [`LICENSE`](LICENSE).
