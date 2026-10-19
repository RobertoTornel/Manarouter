# Handover — project state and what still needs implementing

State of the tree: `main` as of **2026-10-19**. This handover is committed *together with* the work it
describes, so the authoritative hash is `git log -1` rather than a number written here that the next
commit would invalidate. Before the 2026-10-19 work, `main` was at `9439b93` (level with
`origin/main`) and the tree carried a large body of uncommitted Feature Round 2 work; that work is now
committed, so **the tree is clean and `git status` is the first thing to check**.
Everything below was checked against this working tree and, where stated, against a running server.
Where a claim is unverified, it says so. Nothing here is recalled from an earlier session.

Supersedes the previous handover (which described `a241c25` and then `65f543d`). Section 6 records
what changed since, because most of that document's P0/P1 list is now finished and leaving it in place
would send the next person to work that is already done.

---

## 0. What the 2026-10-19 revision changed

This revision is the Feature Round 2 close-out. What it contains, and what it deliberately does not:

| Area | State | Evidence / limitation |
|---|---|---|
| Store schema | **v3**, with the v2 and v3 migrations restored and tested | `crates/mr-core/src/store.rs`: `MIGRATION_V2` (`key_usage`, `provider_quota_state`), `MIGRATION_V3` (`usage_tokens`). A half-applied edit had bumped `SCHEMA_VERSION` to 4 without adding a migration, which broke the build; the constant is back at 3 and the compile-time assert holds. |
| Per-key usage ledger | **Stored, not enforced** | `record_key_usage`, `key_window_usage`, `key_window_usage_many`, `prune_key_usage` exist and are tested (rolling-window maths, dense many-key map, prune, idempotent re-record). **The relay does not write to it and does not enforce any cap** — see §4. |
| Automatic modality fallback | **Implemented, bounded to one hop** | A request whose modality the selected route cannot carry is retried against the configured `routing.defaults` entry, as a complete reroute against the same pinned snapshot — its own provider, account, credential, URL and dialect. A target that also lacks the modality, or names no known model, fails with HTTP 501. Five tests; see §4. |
| Provider quota cache | **Wired to the panel** | `put_provider_quota_state` / `provider_quota_state` / `all_provider_quota_state` / `delete_provider_quota_state` back the conductor's `/api/quota` routes. |
| CI | **Green, and wider than before** | `.github/workflows/ci.yml` runs fmt, strict Clippy, the secret scan, build, workspace tests, release tests, the isolated installer test, the isolated mock-upstream smoke, and the panel Node tests. The standalone `tools/mock-upstream` job passes strict Clippy again (three lints fixed). |
| Test files | **Tracked again** | The earlier session's staged deletions of `tests/smoke_e2e.py`, `tests/local_latency_bench.py`, `panel/test/browser.e2e.mjs` and `scripts/test-installer-systemd.sh` were reverted: CI now *runs* them, so removing them from the repository would have removed the checks rather than protected them. `.gitignore` no longer ignores `/tests/`, `/panel/test/` or `/scripts/test-*.sh`. |
| Documentation | **Reconciled with the code** | `plan.md`, `docs/23-implementation-status.md`, `docs/user/08-proxies.md` and this file now say the same thing about what is and is not implemented. A broken relative link in `docs/user/08-proxies.md` was fixed. |

**Still not implemented, and still described as such:** proxy egress (D4, the transport is direct-only)
and per-key quota *enforcement* (D2, the ledger exists but nothing writes to it yet). Automatic
modality fallback (D3) is now implemented — a bounded, complete reroute, described in §4. The hedged
race runner, `responses.rs` test coverage, and the `tools/bench/` fixture directory are alsostill open — see §4.

---

## 1. Can the tree be trusted right now?

Yes, within these limits:

| Gate | Result |
|---|---|
| `cargo test --workspace --all-targets` | **1 146 passed, 0 failed**, 1 ignored (the opt-in live test) — re-run in this revision |
| `cargo test --workspace --release` | **passed** (release profile: LTO, `panic = "abort"`, `strip`) — re-run in this revision |
| `cargo test -p mr-core --lib store` | **50 passed / 0 failed** (7 new: v1→v3 upgrade, rolling window, dense many-key map, prune, idempotent re-record, quota cache CRUD, token breakdown) |
| `cargo test -p mr-core --test imported_grants_are_independent -- --ignored` | passes; 10/10 imported grants usable with the reference stopped — **not re-run in this revision** (it needs the live reference account) |
| same test with `MANAROUTER_FORCE_REFRESH=1` | **9/10 refresh**; the tenth is the account an earlier destructive test killed (see §3) — **not re-run in this revision** |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` in `tools/mock-upstream` | clean (three lints fixed: an unnecessary cast, a field assigned after `default()`, and a `loop` that should be `while let`) |
| `cargo test` in `tools/mock-upstream` | **22 passed / 0 failed** |
| `bash scripts/check-secrets.sh` | `check-secrets: OK` (189 files, re-run in this revision) |
| `python3 tests/smoke_e2e.py` | **PASS** — isolated conductor + relay + mock-upstream, synthetic credential, disposable config and state |
| `bash scripts/test-installer-systemd.sh` | **passed** — isolated HOME, mocked `git`/`cargo`/`systemctl`; install, update and `--no-start` |
| `node --test panel/test/*.test.mjs` | **42 passed / 0 failed** (`markdown.test.mjs`, `dom.test.mjs`) |
| Panel browser E2E (Firefox and Chromium) | PASS in the `a13405e` session — **not re-run in this revision**: Playwright and the browser binaries are not project dependencies, so the browser test stays a local opt-in |
| `curl http://127.0.0.1:20900/healthz` | `{"status":"ok"}` — no upstream call |

> The browser-E2E row is the one thing here that a fresh checkout cannot reproduce on its own. The
> Node panel tests, the smoke harness, the installer harness and the mock-upstream suite are all
> wired into CI; the browser workflow is not, and says so.

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check && bash scripts/check-secrets.sh
python3 tests/smoke_e2e.py && bash scripts/test-installer-systemd.sh
```

Test distribution, so a failure points somewhere. These are the counts from the fresh
`cargo test --workspace --all-targets` run in this revision, not from counting attributes:

| Target | Tests |
|---|---|
| `mr-core` lib | 641 |
| `manarouter-relay` lib | 230 |
| `mr-dialects` lib | 142 |
| `mr-cli` bin | 96 |
| `manarouter-conductor` lib | 34 |
| `manarouter-relay` bin | 3 |
| `mr-core` integration (`imported_grants_are_independent`) | 1, **ignored by default** |
| **total (excluding the ignored one)** | **1 146** |

Three more suites live outside the workspace crate list and are run separately:
`tools/mock-upstream` (22), `panel/test/*.test.mjs` (42, Node) and the two Python/bash harnesses
(`tests/smoke_e2e.py`, `scripts/test-installer-systemd.sh`).

**Green tests are not the same as a reachable feature.** Three separate bugs in this project's
history were modules that compiled, passed every unit test, and were never called by anything. The
most recent of those, found and fixed in `9ec3284`, was an imported model list whose *synthetic*
entries the upstream rejects — the tests were green and the accounts were unusable. `b63fb47` is the
same lesson in a different shape: not a call nobody made but a check nobody ran — the installer test
passed on every machine while a systemd ordering cycle left the relay `inactive` on every real boot
(§5). Assume more exist, and verify by *running the thing*.

---

## 2. How to run it

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --workspace

# Isolated config + state, so nothing touches your real ~/.config/manarouter
rm -rf /tmp/mr-config /tmp/mr-state && mkdir -p /tmp/mr-config /tmp/mr-state
XDG_CONFIG_HOME=/tmp/mr-config \
MANAROUTER_STATE_DIR=/tmp/mr-state \
  ./target/debug/manarouter-conductor
```

Then `http://127.0.0.1:20901/` is the panel and `/api/health` is the API. The relay is a **separate
process** (`./target/debug/manarouter-relay`, port 20900, configured under `relay.listen`). The
conductor never starts it.

On this machine both run as user services:

```bash
systemctl --user status  manarouter-relay.service manarouter-conductor.service
systemctl --user restart manarouter-relay.service     # only after upgrading the binary
```

**Both processes adopt configuration changes without a restart.** The relay reads the compiled
snapshot at startup and then polls `config.json` every 2 s, recompiling through its existing
`reload()` (`crates/manarouter-relay/src/state.rs:260`) whenever the file's mtime or length moved —
so an `mr migrate` or a model import is live within about two seconds, with no restart and no
`systemctl` step. The conductor does the same on the next authenticated `GET`. Restart only to pick
up a new binary. See §4 and §5 for why the two use different triggers.

### Shape of the codebase

| Crate | Lines | Role |
|---|---|---|
| `mr-core` | ~21 200 | Config, snapshot compiler, vault, store, OAuth platforms, catalogue |
| `mr-dialects` | ~7 600 | Request/response translation against the pivot |
| `mr-cli` | ~7 200 | `mr` command surface |
| `manarouter-relay` | ~6 100 | Data plane: auth, routing, upstream dispatch |
| `manarouter-conductor` | ~4 800 | Control plane and web panel API |
| `panel/` | 19 files | Panel front end (plain ES modules, no framework) |

`catalog/providers.toml` holds **55 providers**: 49 `open-ai` dialect, and one each of `codex`,
`copilot`, `antigravity`, `command-code`, `anthropic`, `google`. **Four** declare OAuth —
`openai-codex`, `github-copilot`, `antigravity` and `cline` — but only three are registered
(`crates/mr-core/src/oauth/mod.rs:629`). `cline` is deliberately listed in the registry test's
`NOT_IMPLEMENTED` set (`:654`) with the reason "no verified interactive flow", so the gap is asserted
rather than latent: a test fails if a catalogue platform has no implementation and is not on that
list. `mr serve` is now a real supervisor that runs both binaries as children (foreground, one
Ctrl-C stops both, either child exiting stops the other — see §3); `mr config` and
`mr vault` still parse their own help but every body returns `"is not implemented yet"`
(`crates/mr-cli/src/config.rs:46`, `crates/mr-cli/src/vault.rs:24`).

---

## 3. What was finished recently

### `9ec3284` — OmniRoute OAuth migration (this session's main work)

`mr migrate omniroute` now imports Codex and Antigravity OAuth accounts, their model lists and the
aliases that name them. The previous position, recorded in `docs/17`, was that grants cannot be
migrated. That was wrong for these platforms: OmniRoute and this project ship the **same public
installed-app client identities** (`crates/mr-core/src/published.rs` already asserted each digest), so
a refresh token issued to one is accepted by the other. The migration does one refresh per account and
stores the result, so the reference becomes irrelevant afterwards.

Verified against the live installation: **10 grants imported**, all usable with the reference stopped,
and a real completion served through the relay. **Nine of the ten still refresh**; the tenth is dead
because an earlier version of the verification test spent its single-use token without persisting the
replacement (last entry below). Findings worth keeping:

- **Codex needs its `id_token`** — the relay reads `chatgpt-account-id` from it; without it every
  request is a hard 400.
- **Antigravity needs a Cloud Code project** in the request envelope, and the reference writes the
  literal `__REQUIRES_GCP_PROJECT__` when it has none. `--apply` tries to onboard one and refuses the
  account if it cannot, rather than creating one that fails every request.
- **Copilot tokens can be genuinely unimportable.** GitHub refuses to mint a Copilot session from a
  `ghu_` GitHub **App** token (`403 programmatic_token_generation, approved clients only`) while
  accepting `gho_`. The prefix is checked up front, so a doomed account is reported instead of
  imported. Both Copilot rows on this machine are `ghu_` and were correctly refused.
- **The reference lists synthetic reasoning-effort variants that are not real model ids** —
  `gpt-6-luna`, then `gpt-6-luna-high`, `-low`, … Only the base exists upstream; a variant is answered
  with `400 ... not supported when using Codex with a ChatGPT account`. Base models are imported and
  the suffixed names registered as aliases onto them. A genuinely suffixed model (Antigravity's
  `gemini-3.8-flash-high`) is kept, and the distinction is drawn from the listing itself rather than a
  hardcoded vocabulary.
- **Importing Codex is a *move*, not a copy** (`9b7c47c`). Codex refresh tokens are single-use, so the
  import consumes the reference's copy: after a Codex import OmniRoute cannot refresh that account
  without its own re-login. Antigravity's tokens are *reusable*, so an Antigravity import leaves the
  reference working. Verified both directions against the live installation. This asymmetry is
  dangerous precisely because it is invisible: a test written against Antigravity passes while Codex
  accounts are being consumed.
- **A verification test killed a live account, and that is a lesson about the test, not the feature.**
  The first version of `imported_grants_are_independent` called `refresh_with_context` directly to
  "check" the credentials. That method returns the rotated set and persists nothing, so every run
  rotated up to six single-use tokens and discarded the replacements. One account
  (`jujupatata038@gmail.com`) is now permanently dead — the reference's copy died with it — and only
  an interactive re-login can restore it. **Nine of ten accounts still work.** The test now uses
  `Refresher::ensure_fresh(.., false)`, the production path, which persists any rotation and makes no
  network call when the token is not due. A diagnostic that wants to *check* a credential must never
  be able to *spend* it.

Also in this commit: model lists carry labels, context windows and output limits across, because a
provider with no model entries resolves nothing and the account would be live but unreachable.

### `b936fb2` — sticky-success preference, pool tuning, local latency benchmark

Priority combos with hedging enabled remember the successful provider/model for an inactivity window
(default 1800 s, combo-wide, rechecked for eligibility each request). `tests/local_latency_bench.py`
is a **report-only** comparison of direct vs relay-routed calls to the local mock; no threshold is
applied and it must not be read as a provider-latency benchmark.

### `73bceab` — external config edits adopted live, and Codex device-code login

Both processes now adopt an external `config.json` write without a restart: the conductor re-reads on
the next authenticated `GET` behind a `stat` fast path (`crates/manarouter-conductor/src/api.rs:194`,
`state.rs:342`), and the relay polls every 2 s and recompiles through the `reload()` that previously
had no caller (`crates/manarouter-relay/src/startup.rs:232`). The panel now offers the OAuth flows a
platform actually supports and adds a Codex device-code fallback. Its message records **746 tests
passing, up from 731**, and says the conductor-reload wiring test was mutation-checked.

### `b63fb47` — systemd ordering cycle fixed

The generated conductor unit was ordered `After=default.target` while also installed
`WantedBy=default.target`, which deadlocked at boot and made systemd delete the relay's start job; the
conductor is now ordered `After=network.target`. `scripts/check-systemd-cycle.py` and the extended
`scripts/test-installer-systemd.sh` guard against a regression. See §5.

### `65f543d` — `mr serve` and the `startup.rs` split

`crates/mr-cli/src/serve.rs` grew from a stub into a real supervisor: it locates and runs both
binaries in their own process groups, forwards signals to the group, stops the sibling when either
child exits, and propagates the exit code. The relay's startup, flag/config precedence, bind selection
and config poller moved out of `main.rs` into `crates/manarouter-relay/src/startup.rs` (476 lines, 17
tests), leaving `main.rs` at 147 lines. Committed and pushed as `65f543d`.

---

## 4. What still needs implementing

Ordered by what blocks the most.

### P1 — Per-key quota enforcement (D2): the ledger exists, nothing writes to it

The store side is done and tested: `key_usage` is an immutable ledger, `record_key_usage` appends to
it, `key_window_usage` / `key_window_usage_many` compute genuinely **rolling** windows from the
history, and `prune_key_usage` bounds its growth. The `KeyPlan` in the compiled snapshot carries
`limit_requests` and the per-window `KeyLimits`.

What is missing is the relay half, and it is the half that matters:

- `crates/manarouter-relay/src/state.rs`'s `KeyEntry` holds only `id`, `label`, `hash` and the two
  allow-lists — **the snapshot's limits never reach the request path**, so the relay cannot know what
  a key is allowed to spend.
- Nothing in the relay calls `record_key_usage`; the `UsageTap` measures a response but does not
  persist it.
- Enforcement therefore needs, in this order: carry the limits into `KeyEntry`; **admit** atomically
  before any upstream side effect (a `BEGIN IMMEDIATE` transaction that counts the window and
  reserves the request); **settle** the actual token spend when the response ends, releasing the
  reservation on failure or on an abandoned stream; and answer `429` on denial.

A request counter alone is not enough: the configured caps are token caps, and a token cap needs a
reservation, because the number of tokens is only known after the upstream has answered. The
`key_usage` ledger is the right place to settle into, but the *admission* decision needs its own
ledger (in-flight reservations) — a second table, or a `reserved` column — so that two concurrent
requests cannot both pass a check that only one of them may pass.

### Closed — Automatic modality fallback (D3): a bounded re-dispatch

**Done in this revision.** `upstream::rewrite_model_for_modalities` detects the input modalities and
consults `routing.defaults`; when the selected route cannot carry the modality, the relay now retries
the **whole request** against the configured default rather than refusing it
(`crates/manarouter-relay/src/server.rs`, `dispatch_routed`).

The retry is a complete reroute, not a model-string swap: the client body's `model` is replaced with
the target, and the request goes back through the whole path — resolution against the *same* pinned
`RouteTable`, so the provider, account, credential, URL, dialect and headers all come from the
snapshot generation that produced the original step. The route that could not carry the modality is
never asked to serve it.

The budget is **one** fallback (`fallback_depth`): a target that also lacks the modality fails with
HTTP 501 `modality_fallback_unavailable` instead of looping, and a default that names no known model
fails with the same code and says so, rather than routing to something the operator did not name.

The re-entry is a boxed call (`dispatch_routed_boxed`): an async function that calls itself has an
infinitely recursive future type, so the single re-entry point is made through a `BoxFuture`.

Five tests cover it (`crates/manarouter-relay/src/server.rs`): the fallback reroutes to the provider
that can take the image and the text-only upstream is never called; a request the route *can* serve
is not a fallback; a default naming no known model is reported; a default that cannot carry the
image either fails rather than looping; and the rewritten body keeps the request and replaces only
the model.

### P1 — Proxy egress (D4): configured, selected, and never used

`crates/manarouter-relay/src/proxy.rs` implements HTTP CONNECT, SOCKS4, SOCKS5 and SOCKS5h
handshakes, `Config::proxies` carries the pool, and `snapshot::ProxyPlan` plus `ProxyPlan`/`AccountPlan`
carry the per-provider and per-account assignments. The selector (`proxy::choose`) picks one.

**None of it is on the request path.** `UpstreamRequest.proxy` is set to `None`
(`crates/manarouter-relay/src/server.rs`), and the relay's hyper client is built on a direct
`HttpsConnector<HttpConnector>`. So today a configured proxy changes nothing about where traffic
goes, and `docs/user/08-proxies.md` must keep saying that until it does.

Wiring it is a transport change, not a call-site change: TLS to the proxy and TLS to the origin are
different handshakes, so the connector has to become per-endpoint, with a pool that cannot mix a
direct connection with a proxied one. The safe rule is **fail closed**: if a proxy is selected and
the tunnel fails, the request fails — it must never silently fall back to a direct connection, which
would defeat the operator's egress policy.

### P1 — The hedged race has no runner

**Still open, and this is now the largest gap.** `select::hedge_decide` exists and is tested
(`crates/manarouter-relay/src/select.rs:456`, tests from `:1035`), and `docs/07-hedged-race.md`
describes the behaviour in detail — but **it is called 0 times from `server.rs`**, which dispatches
sequentially (`for step in &steps`, `crates/manarouter-relay/src/server.rs:369`). The entire feature —
the eligibility window, the four `WinnerFailMode`s, `winner_metric` — is documented and unreachable.

This is the same failure class as the dialect-wiring bug fixed earlier. The design is settled
(decision Q2), so this is implementation, not design:

- Dispatch to the chosen participants **concurrently**, not sequentially.
- `window_ms` is an **eligibility gate, not a timeout**: a participant answering after the window is
  not an error, and window expiry must never fail a request.
- Winner = highest-priority participant replying inside the window.
- Pick up the losers' bodies and drop them without leaking connections.
- `MidstreamPolicy::Restart` must refuse to splice once any `tool_calls` delta has been emitted
  (the guard is documented; confirm it is enforced at the splice site).

Verify by *running* it: two mock participants with the second artificially faster.

**Interaction with the latency constraint.** The standing requirement is that the relay is optimised
obsessively for latency and combo logic lives outside it. Concurrent dispatch adds a join and
cancellation machinery to the request path, so measure the added overhead before and after
(`tests/local_latency_bench.py`) rather than assuming it is free.

### P1 — `crates/mr-dialects/src/responses.rs`: 1221 lines, 1 test

The one test is the important one — `[DONE]` suppression for Responses clients vs Chat clients
(`:1205`) — and it was written deliberately. But one test over 1221 lines is thin for the dialect that
sits under the Codex path, which is the provider this session just made live. `google.rs` has 29
tests, `commandcode.rs` 28, `openai.rs` 25 by comparison. The `(OpenAi, Responses)` arms in
particular deserve reachability tests: `route_kind_for("/v1/responses")` is wired
(`server.rs:884`) and asserted (`:1621`), but translation arms reachable only in combination are
exactly where the two prior "arm present, never taken" bugs lived.

### P2 — `tools/bench/` is empty

The directory exists and contains no files. The latency budget (p50 < 150 µs, p99 < 500 µs added
overhead) has **no harness behind it**. `tests/local_latency_bench.py` is a different, deliberately
threshold-free tool. If the budget is meant to be enforced, something has to measure *added* overhead
on the request path — the one measured number in the project is the relay's `/healthz` at ~0.5 ms,
which is not that number.

### P2 — OmniRoute fidelity is second-hand

`github.com/diegosouzapw/OmniRoute` was never cloned. Provider behaviour was ported from summaries of
the bundled runtime at `~/.omniroute/`. The migration work in `9ec3284` narrowed this gap for the
three OAuth platforms, because it read the reference's **live database** — real encrypted tokens,
real model lists, real health fields — and the decode is pinned by a cross-implementation test vector.
The detail confirmed by that reading is recorded in `docs/17`. For the remaining **52 API-key
providers** the original caveat stands: before trusting any one provider's details, clone the real
repository and re-derive them.

### P3 — Documentation

- **There is still no provider-dialects document.** `docs/08-*` is quota tracking. `mr-dialects` is on
  the request path, and a doc covering each dialect, the pivot shape, and what
  `path_override`/`force_stream` mean is worth writing. This session added none.
- **`docs/LLM_INDEX.md` does not list every doc.** `23-implementation-status.md` and
  `local-latency-benchmark.md` are both missing from it (28 docs exist; check after any addition):
  ```bash
  for f in docs/*.md; do b=$(basename "$f"); grep -q "$b" docs/LLM_INDEX.md || echo "NOT LISTED: $b"; done
  ```
- `docs/17-migrating-from-omniroute.md` and `docs/23-implementation-status.md` were both corrected in
  `9ec3284` and now match the code (`docs/23` was corrected again in `65f543d`).

### P3 — Release and design debt

- **No git tag.** `v0.1.0` was planned and still does not exist (`git tag -l` is empty).
- **`refresh.rs`'s `FailureClass` string marker is gone.** It is a real enum now
  (`crates/mr-core/src/oauth/refresh.rs:51`), so this earlier item is closed. The `oauth[permanent] …`
  text is now only the `Display` form at the head of an `InvalidState` message (`:123`).
- **Cross-instance sync** (`docs/22-sync-design.md`) is inert groundwork by decision Q15.
- **Nothing is unpushed.** `65f543d` is level with `origin/main`
  (`https://github.com/RobertoTornel/Manarouter.git`); the only uncommitted work is the working-tree
  doc/lint edits described at the top of this file.

---

## 5. Traps that will cost you a day if you do not know them

- **A systemd ordering cycle used to leave the relay silently never started — fixed in `b63fb47`, and
  now guarded.** The generated conductor unit was ordered `After=default.target` while also installed
  `WantedBy=default.target`; at boot that closed a loop (`default.target` wants the relay, the relay
  wants the conductor, the conductor waits for `default.target`), which systemd broke by **deleting a
  start job**. The victim was not `failed`, it was `inactive`, with one journal line and nothing to
  notice; and it reproduced **only** on a real boot, so `systemctl --user start` looked perfectly
  healthy. The conductor is now ordered `After=network.target`, the dependency it actually has. Two
  things catch a regression: `systemd --user --test` (the real loader — needs a live user manager) and
  `scripts/check-systemd-cycle.py`, which `scripts/test-installer-systemd.sh` now runs and which is the
  portable equivalent. Note `systemd-analyze verify` does **not** check the ordering graph, so it
  reports these units as fine.
- **`export PATH="$HOME/.cargo/bin:$PATH"`** in every shell, or `cargo` is not found.
- **Configuration changes are picked up without a restart, by both processes — but through two
  different mechanisms, and the difference explains most "it works in the panel but not the API"
  reports.** The conductor re-reads `config.json` on the next authenticated `GET` (a `stat`, then a
  parse only when the file actually moved), so the panel reflects an external `mr migrate` /
  `mr provider add` immediately. The relay polls the same file every 2 s and recompiles its snapshot,
  so a call to a just-imported model can 404 for up to ~2 s after the panel starts showing it. That
  window is deliberate: the relay is on the request path and polling costs one `stat`, where an
  inotify watcher would add a dependency and can still lose events across an editor's rename dance.
  If you are changing routing code, note that neither process reloads on a *write* — only the
  conductor's `GET` hook reloads, so a change made through the API is already in memory and needs no
  re-read, and an external change is adopted by whichever process notices the file next.
- **The panel browser test needs a resolvable Playwright, and the error is misleading.** The module is
  not a project dependency, so `node panel/test/browser.e2e.mjs` exits 2 telling you to install it. On
  a machine where browsers are already cached, point it at any resolvable copy and the **matching**
  browser binary — a version mismatch otherwise fails with "Executable doesn't exist" naming a build
  number you do not have:
  ```bash
  BROWSER=firefox \
  PLAYWRIGHT_MODULE=/path/to/node_modules/playwright \
  FIREFOX_EXECUTABLE=$HOME/.cache/ms-playwright/firefox-1555/firefox/firefox \
    node panel/test/browser.e2e.mjs
  ```
  Both Firefox and Chromium pass. The chromium path is `chrome-linux64/chrome`, **not**
  `chrome-linux/chrome`.
- **axum 0.8 uses `/{id}`, not `/:id`**, and `FromRequest` is **async** — clippy rejects the
  `fn -> impl Future` form.
- **A module declaration and its file are one indivisible change.** Declaring sibling modules before
  their files existed took the whole workspace down with E0583.
- **Do not `#[allow(dead_code)]` to silence an unused stub.** An unused stub is an unwired stub.
  Search for its callers. (`hedge_decide` in §4 is the live example.)
- **`git add -f` on `.research/` is forbidden** — gitignored on purpose; it holds tens of thousands of
  lines of upstream research plus preserved failure artefacts.
- **Never write a Google client id or secret as a contiguous literal.** GitHub push protection blocked
  this repository's first push over four of them. They are assembled from fragments in
  `crates/mr-core/src/published.rs`; `scripts/check-secrets.sh` fails if you reintroduce one. Do **not**
  add them to `ALLOWLIST` — the allowlist re-introduces the literal, which is the thing that was
  blocked. The migration's own tests use obviously-fake fixtures (`gho_000…`) for this reason.
- **`Cargo.lock` is resolved against the manifests.** A lock resolved against uncommitted manifests
  once passed locally and broke a clean clone with `--locked`. Check HEAD in a fresh clone.
- **One writer per file.** Committing into a file whose author was still writing added a duplicate
  `impl` and broke the build.
- **The secrets scanner matches a provider-key prefix followed by 17+ identifier characters.** A test
  name containing that prefix as part of a longer word triggers it. Renaming is the fix; a marker
  comment cannot be used there, because `rustfmt` moves a comment after an opening brace into the block.
- **`scripts/check-secrets.sh` scans `git ls-files` — tracked files only.** A green run therefore says
  nothing about a brand-new untracked file, and this bit during the migration work: the gate passed,
  and only after committing did the two `ghu_`/`gho_` test fixtures trip it. If you add a file that
  contains a credential-shaped fixture, run the scanner against it explicitly *before* committing:
  `bash scripts/check-secrets.sh path/to/file.rs`. When it does flag a deliberate fixture, the
  exemption is an inline `// secret-scan: allow` marker **on the same line as the value** (a marker on
  the preceding line does not match, because the check is line-based) — and it must survive `rustfmt`,
  so format first and re-run the scanner.
- **US privacy constraint (decision Q6): never support cookie auth, at all.** The migration enforces
  this — web-session providers are refused by *policy*, before any "not implemented" check, so the
  refusal is not mistaken for a gap.

---

## 6. What changed since the previous handover

The previous document described `a241c25`. Every item in its P0/P1 list was re-checked against this
tree; this is the disposition, so nobody redoes finished work:

| Previous item | Now | Evidence |
|---|---|---|
| P0 Panel never seen in a browser | **Closed** | `panel/test/browser.e2e.mjs` exists and **passes on Firefox and Chromium** (run in this session). The blank-page defect is also fixed: `boot()` wraps everything in try/catch and calls `showBootError` (`panel/app.js:354`). |
| P1 Hedged race has no runner | **Open** | §4. `hedge_decide` called 0 times from `server.rs`; relay still loops sequentially. |
| P1 `responses.rs` 0 tests | **Mostly open** | 1 test now, the `[DONE]`-suppression one specifically asked for (`responses.rs:1205`). 1221 lines, still thin. |
| P1 Antigravity OAuth incomplete | **Closed** | `antigravity.rs` is 742 lines with `authorization_url` (`:266`), `exchange_code` (`:300`), `refresh` (`:339`), `refresh_with_context` (`:344`), `complete_login` (`:374`) and Google identity parsing (`:114`). The relay injects the project (`server.rs:222`, called at `:511`). |
| P2 `tools/bench/` empty | **Still open** | Directory exists, 0 files. |
| P2 `tools/mock-upstream` half-wired | **Closed** | `mod faults;` declared (`lib.rs:29`), `POST /__control` (`:229`), `x-mock-fault` handled (`:205`). |
| P2 OmniRoute fidelity second-hand | **Partly closed** | OAuth platforms now derived from the live database (§4). 52 API-key providers unchanged. |
| P3 No provider-dialects doc | **Still open** | No such doc in `docs/`. |
| P3 `refresh.rs` string marker | **Closed** | Real `FailureClass` enum (`refresh.rs:51`). |
| P3 No git tag | **Still open** | `git tag -l` empty. |
| P3 docs index incomplete | **Still open** | 2 docs missing from `LLM_INDEX.md`. |

The old document's advice still holds verbatim and is worth repeating: **green tests are not the same
as a reachable feature**, and two prior bugs were exactly "arm present, never taken".

---

## 7. Ratified decisions (do not relitigate)

Q1 compiled snapshot, zero IPC per request · Q2 hedged race, four winner-fail modes ·
Q3 retry the prompt with the next-best model per the combo's rules · Q4 live upstream signals first,
never class a whole provider dead while one key lives · Q5 all credentialed OmniRoute providers plus an
importer · Q6 no cookie auth · Q7 control plane outside the router · Q8 targeted model scan and
verbatim passthrough · Q9 tokio multi-thread plus `SO_REUSEPORT` · Q10 persist with a Restart button
labelled "next restart" · Q11 repo **Manarouter**, crates `mr-core`, `mr-dialects`,
`manarouter-relay`, `manarouter-conductor`, `mr-cli`, config in `~/.config/manarouter/` · Q12 0600 and
encrypted at rest, referenced by id, CI grep · Q13 import from a copy, dry-run, original untouched ·
Q14 CI mock tests plus opt-in live smoke · Q15 session affinity in, Anthropic `/v1/messages` and
Responses shims in, token compression off-path and default-off, v2 sync groundwork.

Two decisions were extended, not changed, by the migration work:

- **Q13** now also covers credentials: the reference is read from a **copy** with its `-wal`/`-shm`
  siblings folded in, the original is never written to, a change to it during the read is *reported
  rather than fatal* (the reference is usually running), and the dry run remains the default.
- **Q6** is enforced in the importer by policy rather than by capability, so a refused web-session
  provider never reads as "not implemented".

The latency constraint is verbatim and load-bearing: the relay is optimised **obsessively** for
latency, combo logic lives outside it, and it should only send and receive. The honest claim is
**zero interpretation on the request path**, not zero-copy.

---

## 8. Deployment record (2026-10-19)

The user authorised a rebuild and a restart of the two live services. What was done, and what it
fixed:

| Step | Result |
|---|---|
| `cargo build --release --workspace` | Finished in 1 m 32 s; `manarouter-relay` and `manarouter-conductor` replaced. |
| `systemctl --user restart manarouter-conductor.service manarouter-relay.service` | Both units **active (running)**. |
| `curl http://127.0.0.1:20900/healthz` | `{"status":"ok"}` (HTTP 200). |
| `curl http://127.0.0.1:20901/api/health` | `{"version":"0.1.0","uptime_secs":7,"snapshot":{"digest":"8e088c7662c05ee9",...},"counts":{"providers":3,"combos":0,"aliases":0,"keys":4,"accounts":11},"password_set":false}` — the control plane is serving the real snapshot again. |
| `curl http://127.0.0.1:20900/v1/models` | HTTP 401 without a client key, which is the correct answer. |

A second build-and-restart followed the D3 commit (`046ce02`), so the deployed relay carries the
modality fallback too: `cargo build --release --workspace` (1 m 30 s), both units restarted, both
**active (running)**, `/healthz` → `{"status":"ok"}`, the panel → HTTP 200, and
`/api/health` → `{"version":"0.1.0","uptime_secs":3,"snapshot":{"digest":"8e088c7662c05ee9",...},"counts":{"providers":3,"combos":0,"aliases":0,"keys":4,"accounts":11},"password_set":false}`.
The relay's startup log shows the expected lines (`routing table loaded keys=4 combos=0
providers=3`, `relay listening addr=127.0.0.1:20900`) and no errors.

**The control plane had been down.** Before the restart, `manarouter-conductor.service` was in
`activating (auto-restart)` with a restart counter above 12 000, failing every three seconds with:

> `database schema version 3 is newer than this build supports (2); refusing to open it rather than
> run a migration set this build does not have`

The deployed binary was the older release that only knew schema v2, while the live database had
already been migrated to v3 by a build from the working tree. The rebuilt binary knows v3 and opens
it, so the panel and the `/api/*` routes are back. The relay had stayed up only because it had been
running since before the database moved.

The database was not touched: it is still at `user_version = 3`, and the v2/v3 migrations are no-ops
against it. Nothing under `~/.config/manarouter` or `~/.local/state/manarouter` was edited.
