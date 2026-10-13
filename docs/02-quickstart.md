# Quickstart

This is the path from an empty machine to a routed request, with the failure modes named at each
step. It is written so that every command can be typed in order.

> **Interface status.** The `mr` CLI is a skeleton in this tree: `crates/mr-cli/src/main.rs` parses
> only `--config-dir` and logs a startup line. The commands below are the intended interface — the
> names and arguments the CLI is being built to. Where a flag cannot be verified against code yet it
> is called out as *unverified* rather than presented as working. The `config.json` edits shown in
> [configuration](03-configuration.md) are the ground truth in the meantime: hand-editing the file is
> always supported, and is the one path that is verified today.

---

## 1. Build from source

The workspace pins Rust 1.82 as its MSRV and ships a `rust-toolchain.toml` requesting the stable
channel with `rustfmt` and `clippy`. With `rustup` installed, the toolchain file is honoured
automatically:

```bash
git clone https://github.com/RobertoTornel/Manarouter
cd Manarouter
cargo build --release
```

The build produces three binaries you will use:

| Binary | Process |
|---|---|
| `target/release/manarouter-relay` | the data plane, port 20900 |
| `target/release/manarouter-conductor` | the control plane, port 20901 |
| `target/release/mr` | the control CLI |

A debug build works and is easier to read in a debugger, but do not benchmark it. The release profile
is where the latency target lives, and `[profile.release]` settings — LTO, `codegen-units = 1`,
`panic = "abort"` in the binary-only case — are part of the measured number, not decoration. See
[latency engineering](16-latency-engineering.md) §3.8.

**If it fails:**

- `error: package ... requires rustc 1.82` — your default toolchain is older. `rustup show` in the
  repository root should report the pinned stable; `rustup update stable` if not.
- A linker error on the SQLite dependency — the workspace builds `rusqlite` through `libsqlite3-sys`,
  which finds a system SQLite or builds the bundled one depending on features. Report the exact
  message rather than working around it, because the feature choice is deliberate.

---

## 2. Add a provider

A **provider** is a base URL plus a dialect plus a credential attachment. The catalogue in
`catalog/providers.toml` knows the real endpoints; `mr provider add` copies a template into your
`config.json` so you do not have to remember whether Anthropic wants `Authorization: Bearer` or
`x-api-key`.

```bash
./target/release/mr provider add openai
./target/release/mr provider add anthropic
```

After each, `mr provider list` should show the provider with `enabled = true` and no accounts. The
first command writes a provider equivalent to:

```json
{
  "id": "openai",
  "label": "OpenAI",
  "dialect": "open-ai",
  "base_url": "https://api.openai.com/v1",
  "auth": { "type": "bearer" },
  "capabilities": ["chat", "responses", "embeddings", "audio.transcription",
                   "audio.speech", "images.generations", "models", "tool-calling",
                   "reasoning", "vision"],
  "accounts": [],
  "key_strategy": "priority",
  "enabled": true
}
```

The `anthropic` template differs in exactly the two ways that matter: `dialect` is `anthropic`,
`base_url` is `https://api.anthropic.com/v1`, and `auth` is
`{ "type": "api-key-header", "header": "x-api-key" }`. It also carries an
`anthropic-version: 2023-06-01` header, because a missing version header is a hard upstream 400
rather than a warning.

A provider you cannot find in the catalogue is added by hand — any OpenAI-compatible endpoint works:

```bash
./target/release/mr provider add openai-compatible \
  --id gateway --base-url https://gateway.internal/v1
```

*`--id` on `provider add` is unverified: the flag names come from the README, and the CLI that would
parse them is still a stub. If the flag is rejected, add the provider by hand in `config.json`; the
shape above and in [configuration](03-configuration.md) §4 is what the parser expects.*

---

## 3. Add an account

A provider with no accounts routes nothing. An **account** is one credential for that provider —
three API keys for OpenAI are three accounts, each with its own health, quota and rotation position.

```bash
./target/release/mr account add openai --label "personal" --key "<your-api-key>"
```

The key value is passed to the vault and encrypted with AES-256-GCM before it touches disk; what
lands in `config.json` is an id reference, never the value. Confirm that:

```bash
grep -c "<your-api-key>" ~/.config/manarouter/config.json   # expect 0
```

**Never commit a real key, and never paste one into an issue.** `scripts/check-secrets.sh` runs on
every push and fails the build on `sk-`, `ghp_`, `AIza`, `ya29.`, `GOCSPX-` and JWT-shaped strings.
The placeholder `<your-api-key>` above is deliberately not shaped like any of them.

For a loopback runtime such as Ollama (`http://127.0.0.1:11434/v1`) the account still exists but its
key may be a dummy, because the server ignores it. Keep the account anyway: health, quota and
rotation are tracked per account, and an account-less provider is invisible to selection.

**The three things that most commonly go wrong here:**

| Symptom | Cause | Fix |
|---|---|---|
| Provider shows `Unconfigured` | no accounts added, or every account `enabled = false` | add an account, or re-enable one — see [key selection](05-key-selection.md) §5 |
| Requests return a routing error naming the provider | `provider.enabled` is `false` in `config.json` | set it true; disabled providers are removed from the compiled snapshot entirely |
| Requests return a key error naming the model | the account's `models_allow` does not cover this model | widen `models_allow`, or clear it — see [key selection](05-key-selection.md) §4 |

---

## 4. Sync models

Model lists are fetched from the provider, not hard-coded, so a new upstream model appears without a
release:

```bash
./target/release/mr models sync openai
./target/release/mr models sync anthropic
```

Sync writes `ModelEntry` records onto the provider: id, optional context length, capabilities and
prices where the provider reports them. Models are what a bare-model request resolves against, and a
model can be hidden from resolution without being deleted:

```bash
./target/release/mr models sync --all
./target/release/mr models list openai          # includes hidden state
```

A provider that does not expose `/v1/models` can be seeded by hand by adding entries under
`providers[].models` in `config.json`. This matters for the subscription-backed dialects, where the
upstream model list is not always the one the client should see.

---

## 5. Create a combo

A combo is a **named model**. This is the step where the router earns its keep.

The simplest useful combo is one strong model with one fallback, both of which exist as providers
after §2:

```bash
./target/release/mr combo create \
  --name "Good" \
  --step openai/gpt-5.5 \
  --step anthropic/claude-sonnet-4-5 \
  --strategy priority
```

That writes:

```json
{
  "id": "0193f2c1-0000-7000-8000-000000000000",
  "name": "Good",
  "steps": [
    { "provider": "openai",    "model": "gpt-5.5",           "enabled": true },
    { "provider": "anthropic", "model": "claude-sonnet-4-5", "enabled": true }
  ],
  "strategy": "priority",
  "hedge": { "enabled": false },
  "enabled": true
}
```

The `id` is a UUIDv7 generated at creation; the copy above is illustrative, not fixed. A client sends
`"model": "Good"` and gets OpenAI; if OpenAI fails, the relay moves to the next step. Nothing about
the client changes. That property — a combo is just a model name — is what makes migration cheap, and
it is developed further in [combos](06-combos.md).

To make the *default* model good, name the combo after whatever the client already sends:

```bash
./target/release/mr combo create --name "gpt-5.5" --step openai/gpt-5.5 \
  --step anthropic/claude-sonnet-4-5
```

The combo name shadows the bare model id at the `combo-name` precedence rank, so the client's
existing configuration keeps working and now has a fallback behind it. The precedence rules that make
this deterministic are in [routing snapshot](04-routing-snapshot.md) §6.

For anything more interesting than a fallback chain — racing two models, rotating accounts, a
cheap-first chain with a stable fallback — copy from the [combo cookbook](combo-cookbook.md).

---

## 6. Create a client key

The relay authenticates callers with a **client key**: the client sends it, the relay hashes it and
compares, and `config.json` holds only the hash. A leaked config file therefore does not leak a
usable key.

```bash
./target/release/mr key create --label "my-editor"
# prints: mr-3f9a...  (shown once, never again)
```

Copy the printed key. `mr key list` shows the label, prefix and permissions, never the key. A key can
be restricted by model and by route:

```bash
./target/release/mr key create --label "cheap-only" --allow-models "Good,gpt-5.5"
```

A key with an empty `allow_models` may call anything. A key that is `enabled = false` is rejected at
authentication, before any routing happens.

---

## 7. Point a client at the relay

Both surfaces live on the same port. The **path** distinguishes them, so the base URL a client is
configured with is the whole integration.

### OpenAI-compatible clients

`OPENAI_BASE_URL` must include the `/v1` segment, because these clients append `/chat/completions`
to it verbatim. The base URL is therefore `http://127.0.0.1:20900/v1`:

```bash
export OPENAI_BASE_URL=http://127.0.0.1:20900/v1
export OPENAI_API_KEY=mr-3f9a...        # your client key, not a provider key
```

```bash
curl -sS http://127.0.0.1:20900/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
        "model": "Good",
        "messages": [{"role": "user", "content": "Say hello in five words."}],
        "stream": false
      }'
```

The response is an ordinary Chat Completions object. `GET /v1/models` lists combos (unless hidden),
synced model ids, and qualified `provider/model` ids — it is a local read, not an upstream call, so it
returns immediately.

### Anthropic-compatible clients

Anthropic's SDK appends `/v1/messages` to `ANTHROPIC_BASE_URL`, so that variable is the bare origin:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:20900
export ANTHROPIC_API_KEY=mr-3f9a...     # the same client key works on both surfaces
```

```bash
curl -sS http://127.0.0.1:20900/v1/messages \
  -H "x-api-key: $ANTHROPIC_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{
        "model": "Good",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "Say hello in five words."}]
      }'
```

The two conventions differ and that is the single most common integration mistake: the OpenAI base
URL carries `/v1`, the Anthropic one does not. Getting it wrong produces a 404 from the relay, which
is a routing miss rather than an upstream error — if you see a 404, check the base URL before
anything else.

The relay accepts the client key in either header, `Authorization: Bearer` or `x-api-key`, so a
client that insists on one form can still talk to the surface that normally uses the other.

---

## 8. Start it and open the panel

Two processes, two terminals (or two unit files):

```bash
./target/release/manarouter-conductor     # control plane, panel on :20901
./target/release/manarouter-relay         # data plane, traffic on :20900
```

The conductor must run first in the intended design: it owns `config.json` and compiles the snapshot
the relay loads. The relay's load path treats a missing snapshot as a cold start rather than a fatal
error, so the order is a robustness preference, not a hard dependency — but a relay started with no
snapshot serves no routes until the conductor publishes one.

Open the panel at <http://127.0.0.1:20901>. It is loopback-only: reaching it from another machine
requires setting a password first, because it can add providers and mint client keys. That rule, and
the origin check behind it, are in [security](21-security.md) §2.

> Both binaries are skeletons in this tree — `manarouter-relay` logs *"data plane not yet wired up:
> no listener is bound"* and exits, and the conductor logs its version. The ports, the panel address
> and the `--config-dir` global flag on `mr` are fixed by the code; the listener itself is the work
> in progress.

---

## 9. Where the files live

| Purpose | Path | Override |
|---|---|---|
| Configuration | `~/.config/manarouter/config.json` | `MANAROUTER_CONFIG_DIR` |
| Encrypted credentials | `~/.config/manarouter/vault.json` | same directory |
| Vault key | `~/.config/manarouter/vault.key` | same directory, mode `0600` |
| Compiled routing table | `~/.local/state/manarouter/routes.snapshot` | `MANAROUTER_STATE_DIR` |
| Runtime state database | `~/.local/state/manarouter/state.sqlite` | `MANAROUTER_STATE_DIR` |

The two directories are separate on purpose. Configuration is authored by a human and is worth
backing up (without the vault); state is derived, churns constantly, and can be deleted.

**Safe to delete:**

- `state.sqlite` — you lose observed account health, quota counters and usage history. Those rebuild
  from live traffic. The config is untouched.
- `routes.snapshot` — the conductor recompiles it. Deleting it while the relay is running changes
  nothing until the next reload, because the relay holds the decoded table in memory.
- Anything in the state directory, in fact. Nothing there is the source of truth.

**Not safe to delete:**

- `vault.key` — every credential in `vault.json` becomes undecryptable. There is no recovery path,
  because there is no key escrow. Back it up separately from `vault.json`, or accept that losing it
  means re-entering every key.
- `config.json` — it is the only record of providers, combos, aliases and client keys. Client keys
  cannot be recovered even in principle: only their hashes are stored, so a lost key is a re-issued
  key.
- `vault.json` without `vault.key`, or vice versa. The pair is the unit.

`XDG_CONFIG_HOME` and `XDG_STATE_HOME` are honoured when the `MANAROUTER_*_DIR` overrides are unset,
so a session-scoped or second instance can be run without touching the real configuration.

---

## 10. Verify it is actually working

Do not conclude from "the process is running" that traffic is flowing. Three checks, cheapest first.

### The relay is alive

```bash
curl -sS -i http://127.0.0.1:20900/healthz
# HTTP/1.1 200 OK
# body: ok
```

`/healthz` is **liveness only**. It never routes upstream, never checks a provider, and is
unauthenticated. A 200 proves the process is listening and the worker is not wedged. It does not
prove a single route works, and it is deliberately not a health check for upstreams.

### The routes exist

```bash
curl -sS http://127.0.0.1:20900/v1/models \
  -H "Authorization: Bearer $OPENAI_API_KEY"
```

You should see `Good` in the returned list. If the combo is missing, the snapshot does not contain
it: check that the combo is `enabled`, that at least one step names a provider that exists in
`providers`, and that the provider is itself `enabled`. A combo whose every step points at a disabled
provider is silently omitted from the snapshot rather than failing the compile — see
[routing snapshot](04-routing-snapshot.md) §5.

### The panel agrees

Open <http://127.0.0.1:20901> and look at two things:

1. **Provider health.** Each provider shows a rollup over its accounts. `Unconfigured` means zero
   enabled accounts; `Dead` means accounts exist but none is usable — and *all accounts deliberately
   disabled* reports `Unconfigured`, because that is an instruction, not an outage. The distinction
   and its consequences are in [key selection](05-key-selection.md) §5.
2. **The snapshot digest.** The panel shows the digest of the compiled table and when it was built.
   If you edit `config.json` and the digest does not change, the edit was not routing-relevant — or
   the conductor did not see it. `routing_digest` hashes provider order and enabled state, account
   priority/weight/enabled and model allow/deny lists, model ids and hidden flags, combo signatures,
   and aliases; a change to a field outside that set correctly does not trigger a recompile.

End-to-end proof is one streamed generation: run the `curl` from §7 with `"stream": true` and confirm
the first token arrives in a time that looks like the provider, not the router. The p50 and p99
budgets under which that is true are in [latency engineering](16-latency-engineering.md) §1.

---

## See also

- [Overview](00-overview.md) — what the project is and what it is not
- [Configuration](03-configuration.md) — the full `config.json` schema and every location
- [Routing snapshot](04-routing-snapshot.md) — what actually happens to the config after you save it
- [Key selection](05-key-selection.md) — how an account is chosen, and what "unusable" means
- [Combos](06-combos.md) — the combo reference
- [Combo cookbook](combo-cookbook.md) — twelve configurations to copy
- [The hedged race](07-hedged-race.md) — the behaviour most people misread
- [Latency engineering](16-latency-engineering.md) — the budget, and how it is verified
- [Security](21-security.md) — the vault, the panel and the ToS posture
