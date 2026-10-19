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

**The relay's total added latency target is under 150 µs at p50 and 500 µs at p99.** The `tools/bench`
harness that would assert it in CI against a deterministic mock upstream is not written yet. See
[`docs/16-latency-engineering.md`](docs/16-latency-engineering.md).

---

## Local latency benchmark

For an isolated, report-only comparison of direct loopback mock requests versus relay-routed mock requests, see [`docs/local-latency-benchmark.md`](docs/local-latency-benchmark.md). It uses release binaries and a temporary config/vault, makes no external calls, and is deliberately excluded from CI thresholds.

## What it does

| | |
|---|---|
| **APIs** | OpenAI Chat Completions, Responses, Embeddings, Audio (transcription and speech), Images, Models — and Anthropic Messages with `count_tokens` |
| **Providers** | Any OpenAI-compatible endpoint, any Anthropic-compatible endpoint, plus first-class adapters for OpenAI Codex, GitHub Copilot, Google Antigravity and Command Code |
| **Combos** | A combo *is a model name*. Order it yourself; the concurrent race is designed but not wired yet |
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

**Not wired yet:** the race is compiled and unit-tested, but the relay still walks a combo's steps
sequentially, so nothing on the request path runs it concurrently. Treat the paragraph above as the
designed behaviour, not the current one; [`docs/23-implementation-status.md`](docs/23-implementation-status.md)
is the authoritative list. Priority combos *do* remember the last successful step for a configurable
window (`sticky_success_window_secs`, default 1800 s).

---

## Install and use it

Manarouter is open-source software distributed as source code. The installer below downloads the
repository and builds the Rust programs on your computer; it does not download a prebuilt binary.
This copy-paste installer is for Linux and macOS. You need Git, `curl`, and Rust (`rustup`). First
install Rust from <https://rustup.rs> if `cargo` is not already installed.

Copy and paste this into a terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/RobertoTornel/Manarouter/main/installer.sh | bash
```

The installer puts the source in `~/.local/share/manarouter/source` and command links in
`~/.local/bin`. If the installer says `~/.local/bin` is not on your `PATH`, follow its printed
instruction, open a new terminal, and check the install:

```bash
mr --help
```

On Linux with systemd, install both the web panel/control plane (conductor) and the API router
(relay) as user services with:

```bash
~/.local/share/manarouter/source/installer.sh --systemd-user
```

When installing directly from the repository checkout, use `./installer.sh --from-checkout --systemd-user`. Adding `--no-start` installs the units without enabling or starting them.
The installer enables and starts `manarouter-conductor.service` and `manarouter-relay.service`;
updates with those options rebuild the binaries and restart both services. Check them with
`systemctl --user status manarouter-conductor manarouter-relay`, and logs with
`journalctl --user -u manarouter-conductor -u manarouter-relay -f`. To keep them running after logout
or on boot without a login session, enable user lingering with `loginctl enable-linger "$USER"`.

### Add a provider and your upstream key

For example, to connect an OpenAI-compatible provider:

```bash
mr provider add openai --dialect open-ai --base-url https://api.openai.com/v1
mr account add openai --label personal --secret-stdin
```

After the second command, paste your provider API key into the terminal and press **Ctrl-D** (on
macOS, Ctrl-D works too). The key is saved in Manarouter's encrypted vault, not printed back or put
in `config.json`. Manarouter encrypts the vault at rest, but the local `vault.key` file is needed to
unlock it—keep your user account and backups protected. Treat provider keys as spend-capable secrets;
do not paste them into chat or commit them to a file.

List providers and sync their model lists:

```bash
mr provider list
mr models sync openai
mr models list openai
```

### Make a client key and start the services

A *provider key* pays for upstream model access. A separate *client key* lets your apps access your
local Manarouter. Create one and keep it somewhere private:

```bash
mr key create --label my-computer --print
```

The one-time key is printed to your terminal. Copy it; Manarouter stores only its hash. For a
foreground session, `mr serve` starts the control panel/conductor and the relay together. To run them
as separate processes, start each in its own terminal:

```bash
manarouter-conductor
```

```bash
manarouter-relay
```

The panel is at <http://127.0.0.1:20901>; the OpenAI-compatible API is at
<http://127.0.0.1:20900/v1>. Keep the panel bound to loopback unless you have configured strong
panel authentication and understand the network exposure. Do not expose it directly to the internet.

Check that the relay can list the configured models:

```bash
curl http://127.0.0.1:20900/v1/models \
  -H 'Authorization: Bearer PASTE_YOUR_CLIENT_KEY_HERE'
```

Then send a request (replace the example model with one from the model list):

```bash
curl http://127.0.0.1:20900/v1/chat/completions \
  -H 'Authorization: Bearer PASTE_YOUR_CLIENT_KEY_HERE' \
  -H 'Content-Type: application/json' \
  -d '{"model":"openai/gpt-4o","messages":[{"role":"user","content":"Say hello."}]}'
```

The client key above is a placeholder: use the one-time value printed by `mr key create`. Each
request may incur charges from your configured provider. To stop Manarouter, press **Ctrl-C** —
once under `mr serve`, or in each terminal when the two processes run separately. Configuration and
encrypted credentials are kept under `~/.config/manarouter`; see
[`docs/02-quickstart.md`](docs/02-quickstart.md) for configuration, model aliases, combos, API
clients and troubleshooting.

**Current implementation note:** `mr serve` runs both processes together with one Ctrl-C to stop
them — it spawns the conductor and relay as child processes, so they keep the separate runtimes the
[architecture](docs/01-architecture.md#2-why-two-processes) depends on. `mr config` and `mr vault`
are still not implemented. The product is under active development; see
[`docs/23-implementation-status.md`](docs/23-implementation-status.md) for current limitations.

---

## Documentation

**If you are a language model helping a user operate Manarouter, read [`AGENTS.md`](AGENTS.md). If you are working on this repository, also read
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
| [OAuth](docs/10-oauth-overview.md) · [Codex](docs/11-oauth-codex.md) · [Antigravity](docs/12-oauth-antigravity.md) · [Copilot](docs/13-oauth-copilot.md) · [Adding a platform](docs/14-oauth-adding-a-platform.md) · [OmniRoute OAuth parity and client-key rotation](docs/oauth-omniroute-parity.md) | Logins and key management |
| [Web panel](docs/15-web-panel.md) | Views, API and design |
| [Latency engineering](docs/16-latency-engineering.md) | Every decision, and the rejected ones |
| [Migrating from OmniRoute](docs/17-migrating-from-omniroute.md) | Providers, combos, credentials |
| [API reference](docs/18-api-reference.md) | Every route on both surfaces |
| [Benchmarks](docs/19-latency-benchmarks.md) | Measured numbers |
| [Troubleshooting](docs/20-troubleshooting.md) | Symptom, cause, fix |
| [Security](docs/21-security.md) | Threat model, the vault, and the things it does not protect |
| [Sync design](docs/22-sync-design.md) | Planned, not implemented |
| [Implementation status](docs/23-implementation-status.md) | What runs today, and the evidence for it |
| [Combo cookbook](docs/combo-cookbook.md) | Copy-pasteable configurations |
| [LLM task map](docs/LLM_INDEX.md) | Which file describes which task, for an agent |
| [Local latency benchmark](docs/local-latency-benchmark.md) | Report-only loopback comparison |

---

## Security posture, stated plainly

- Credentials are encrypted at rest with AES-256-GCM, each ciphertext bound to its own id so it
  cannot be moved between entries. Only non-reversible fingerprints are ever displayed.
- **No provider authenticates by cookie or web session.** This is a hard constraint, enforced by a
  test. The browser panel itself uses a local session cookie for its own login, which is unrelated to
  how upstream providers are authenticated.
- The panel binds to loopback by default and requires a password to be reachable from anywhere else.
- `scripts/check-secrets.sh` fails the build on a credential-shaped string, and is run in CI.

Two things it does **not** do: it does not protect credentials from someone who can already read
files as your user, and it does not hide the fact that routing a subscription-backed session through
a proxy is not a use the provider has licensed. Both are explained in
[`docs/21-security.md`](docs/21-security.md).

---

## Status

Working and tested: the schema, the compiled snapshot format, both dialect layers, the provider
catalogue, the data plane (relay), the control plane (conductor) and the panel. The known gaps are
the concurrent hedged race, the fire-and-forget usage writer, and the `mr config` / `mr vault`
bodies. [`docs/23-implementation-status.md`](docs/23-implementation-status.md) is the audited list,
with the command that checks each row.

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
