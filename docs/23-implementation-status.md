# 23 — Implementation status

This file exists because a specification that reads as finished while the code is not is worse than
no specification. Everything below is checked against the tree, and the command that checks it is
given so you can re-run it.

> **Status note (2026-10-19):** this document retains earlier implementation-history sections below;
> they are not a comprehensive audit of every Round 2 item. Final checks for this close-out passed:
> workspace tests (1 146 passed, 1 ignored), the release-profile test run, `cargo fmt --all -- --check`,
> strict workspace Clippy, strict Clippy and tests in `tools/mock-upstream`, the panel Node tests, the
> secret scan, the isolated installer test, and the isolated relay/conductor/mock-upstream smoke. The
> store schema is at **v3**: `key_usage` and `provider_quota_state` (v2) and `usage_tokens` (v3) are
> present and tested.
> **D2 remains unimplemented** — the per-key ledger is stored and queryable, but the relay neither
> writes to it nor enforces a cap; **proxy egress is not wired** (the transport is direct-only); and
> the user-guide/E5 reviews are partial. **D3 is now implemented**: a request whose modality the
> selected route cannot carry is retried against the configured `routing.defaults` entry, as a
> complete reroute against the same pinned snapshot, with a budget of one fallback (a target that
> also lacks the modality, or names no known model, fails with HTTP 501). Re-run before release and
> inspect `git status`.


## 1. What runs

| Component | State | Evidence / limitation |
|---|---|---|
| `mr-core` — config, snapshot compiler, vault, usage store, OAuth | **Substantial, not complete** | Codex, Copilot, and Antigravity OAuth authorization/token exchange/refresh/Google userinfo identity code is wired; Antigravity IDE client details remain reverse-engineered and unverified. |
| `mr-dialects` — OpenAI, Anthropic, Responses, Google, Command Code translation | **Reachable, test coverage uneven** | Relay dispatches request and response translation per selected step. Responses implementation needs focused regression tests. |
| `manarouter-relay` — HTTP API, auth, snapshot, account selection, translation | **Runs; concurrent hedging not wired** | Startup, flag/config precedence, listener bind selection and the 2 s `config.json` reload poller live in `crates/manarouter-relay/src/startup.rs` (declared at `crates/manarouter-relay/src/lib.rs:47`; 17 tests). Sequential step loop remains the request executor; `hedge_decide` is not called by the relay. Priority combos with `hedge.enabled` now remember the successful provider/model for a configurable inactivity window (default 1800s), combo-wide across callers; the preference is process-local and rechecked against current eligibility on each request. Client-key scan exits at first match (a deliberate latency tradeoff that reveals configured match position to a local timing observer). |
| `manarouter-conductor` — auth, API, state, serving, provider quota refresh | **Partial** | `/api/quota` returns cached provider/account quota snapshots and declares refresh routes. Provider-specific OAuth refresh is reported unsupported. Historical request-usage rows are a separate Store ledger and relay currently does not write them. No live/provider refresh was exercised in this audit. |
| `mr-cli` | **Partial** | Account/provider/combo/key/model families dispatch; `mr key rotate` is supported; `mr doctor` performs read-only config, vault metadata, snapshot freshness, and listener exposure checks; it fails if no enabled client key is present while relay bind is not confirmed loopback-only. `mr migrate omniroute` imports OAuth grants (by one refresh), model lists and effort-variant aliases — dry run by default, `--apply` to write, reading a copy of the reference database. `mr serve` runs both service binaries as child processes for a foreground session (`crates/mr-cli/src/serve.rs`, 6 tests, committed as `65f543d`; one Ctrl-C stops both, either child exiting stops the other, and the failure message names the child and its status — `mr serve` itself exits 1, it does not forward the child's code); `config` and `vault` remain unimplemented (`crates/mr-cli/src/config.rs:46`, `crates/mr-cli/src/vault.rs:24`). |
| `panel/` | **Implemented; critical browser workflow verified** | Firefox E2E covers panel login, client-key creation, and rotation against an isolated mock API; other routes remain unverified. |
| OAuth | **Partial; flows wired, migration verified live** | Conductor OAuth routes cover Codex (browser **and** device-code flows), Copilot, and Antigravity authorization code flows. Config changes written by the CLI are adopted without a restart by both the conductor (on the next authenticated `GET`) and the relay (2 s poll of `config.json`); tokens are vault-backed, metadata lives in SQLite, and relay resolves/refreshes credentials. Antigravity's interactive loopback OAuth exchange/refresh, v2 userinfo identity, free-tier Cloud Code onboarding with bounded LRO polling, encrypted token context persistence, and account-bound relay project injection are implemented. Paid-tier onboarding remains fail-closed without explicit project input. `mr migrate omniroute` transfers Codex and Antigravity grants by one refresh each, using the client identities in `mr-core/src/published.rs`; this was exercised against a live installation (10 grants imported, all re-refreshed with the reference stopped, and a real completion served through the relay). Copilot tokens issued as `ghu_` GitHub App tokens are refused by GitHub's mint endpoint and are reported as needing a fresh login. T2 client metadata and PKCE remain reverse-engineered/unverified. Cline remains unavailable. |
| `tools/mock-upstream` | **Fault control API wired; behaviors partial** | Detached crate tests pass; `POST /__control` and request-local `x-mock-fault` merge patches are reachable. Not every field in `Fault` is yet applied to response generation. |
| `tools/bench` | **Absent** | No harness currently enforces the latency budget. |

Verification run during this session (all local tests used mock/isolated data):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --all-targets
cargo test --workspace --release
bash scripts/check-secrets.sh
bash scripts/test-installer-systemd.sh
PATH="$HOME/.cargo/bin:$PATH" python3 tests/smoke_e2e.py
node --test panel/test/*.test.mjs
(cd tools/mock-upstream && cargo fmt --all -- --check && cargo clippy --all-targets -- -D warnings && cargo test)
# Browser workflow (local opt-in): Playwright and the browser binaries are not project dependencies.
BROWSER=firefox PLAYWRIGHT_MODULE=/path/to/playwright FIREFOX_EXECUTABLE=/path/to/firefox node panel/test/browser.e2e.mjs
```
Every line above was re-run for the revision described here and passed: **1 146 workspace tests**
(1 ignored), a clean release-profile run, clean strict Clippy in the workspace *and* in
`tools/mock-upstream` (22 tests there), `check-secrets: OK` over 140 files, the isolated installer
simulation, the isolated mock-upstream smoke, and 42/42 panel Node tests. The **smoke test, the
installer test and the panel Node tests are wired into CI**; the browser test remains a local opt-in
because Playwright and the browser binaries are not project dependencies.


The OmniRoute migration was verified against a live installation as well, which is a network operation
and so is not part of the list above:

```sh
# Cheap: proves every imported credential is present, readable and usable. Returns the stored
# token without a network call when it is not yet due, so it is safe to re-run.
cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture

# Strong: forces a real rotation from every provider, proving the grants are independently
# refreshable. Spends one single-use refresh token per account. Do not run it in a loop.
MANAROUTER_FORCE_REFRESH=1 \
  cargo test -p mr-core --test imported_grants_are_independent -- --ignored --nocapture
```

Both read this installation's vault and store only — the reference database is never opened — and both
were run with the reference fully stopped. On the live installation the cheap mode reports **10 of 10**
grants usable and the forced mode reports **9 of 10** refreshing: one Codex account was killed by an
earlier, destructive version of this test, which called `refresh_with_context` directly. That method
returns a rotated token set and persists nothing, so on a single-use platform the replacement was
discarded and the account became unusable, in the reference as well as here. The test now goes through
`Refresher::ensure_fresh`, the production path, which persists rotation; the dead account needs an
interactive re-login and nothing in the repository can repair it. See
[the Codex "move, not a copy" warning](17-migrating-from-omniroute.md#warning-for-codex-this-is-a-move-not-a-copy).

## 2. What running it found

Reading the code did not find these; starting the binary did.

**The relay could not start on a fresh install.** `RoutingSnapshot::load_from` wrapped every read
failure in one variant, so the "compile me a fresh one" arm in `RelayState::load` was unreachable and
the process exited with `cannot read snapshot ... No such file or directory`. Fixed twice over: a
missing file is now a distinct error, and — more to the point — the snapshot is treated as a *derived
cache*, so any problem reading it recompiles rather than refusing to start. A cache that can be
rebuilt must never be able to stop the program.

**A keyless relay served everyone.** With no client keys configured, `authenticate` answers `Open`.
That is right on loopback, where refusing everything would make a fresh install unusable, and wrong
anywhere else: binding a keyless relay to a reachable interface hands this machine's provider
credentials to anyone who can route to it. The bind is now refused with exit code 2 when no keys are
configured and the address is not loopback.

Verified by running, not reading: boots on loopback, `/healthz` in 0.5 ms, `/v1/models` served from the
snapshot without contacting a provider, `--host 0.0.0.0` refused, `--host ::1` bound.

## 3. The honest gap in the headline feature

**The hedged race is decided but not run in parallel.**

`select::hedge_decide` implements and tests the entire policy — eligibility window, priority winner,
the four `WinnerFailMode`s, immediate resolution when the top participant replies in window. The
relay's server sends a request to **one step at a time, in priority order**.

So: hedging is a design with a tested decision function and no concurrent runner. It reduces today to
"try each step in order", which is genuinely useful and is *not* the feature as described. Do not
take a latency measurement from this tree and attribute it to hedging.

A safe executor remains a separate implementation task: request preparation currently sits inside
sequential dispatch, responses may stream immediately after commitment, and upstream cancellation
cannot promise that a provider stops generation or billing. Any runner must define those semantics,
limit fan-out, preserve current sequential behavior when disabled, and test cancellation/fallback with
a mock upstream before enabling it for users.

## 4. Verified implementation properties and limits

* **The relay avoids full JSON interpretation for model lookup.** It scans the buffered body for the
  model and forwards original bytes on passthrough paths. This is *not* zero-copy: the request body is
  buffered. Cross-dialect routes do parse and translate bodies, as required.
* **No conductor IPC on the request path.** The relay reads the compiled snapshot and uses ArcSwap;
  account health is read as one immutable view per request.
* **No credential in a URL.** `AuthKind::QueryParam` is recognized and deliberately not applied.
* **`/v1/models` contacts no provider.** It is served from the compiled snapshot.
* **Dialect translation is reachable.** Request translation is selected per step, with response
  translation, translator path overrides, and forced-stream metadata applied in the relay.
* **Quota APIs are distinct.** `/api/quota` returns cached provider/account quota snapshots and
  supports refresh orchestration for supported integrations; OAuth refresh is reported unsupported.
  This is not the request-usage ledger. Relay/conductor do not currently write client-request usage
  rows, and per-key rolling request/token limits are not enforced in relay dispatch.
* **A modality the route cannot carry is rerouted, not misrouted.** When the selected step's model
  does not accept the request's modality, the relay retries the whole request against the
  `routing.defaults` entry for that modality, resolved against the same pinned snapshot, so the
  target's own provider, account, credential, URL and dialect are used. The budget is one fallback:
  a target that also lacks the modality, or names no known model, fails with HTTP 501
  `modality_fallback_unavailable` rather than looping. Covered by five tests in
  `crates/manarouter-relay/src/server.rs`.

## 5. Claims that remain unverified or false

* That hedging is live. See §3: runner remains sequential.
* That Antigravity works end to end. Authorization, token exchange, refresh, and Google userinfo
  identity are implemented, but Cloud Code onboarding and project injection/context are incomplete.
  The registered T2 OAuth client/PKCE behavior is reverse-engineered and unverified; do not rely on
  production access until a user explicitly opts into a live login and validates the upstream API.
  Codex and Copilot also still need real-provider opt-in smoke verification.
* That the provider catalogue was executed against every entry. `docs/providers/catalog.md` marks
  uncertain entries and the catalogue is not a live reachability guarantee.
* That every panel route works in a real browser. Firefox E2E verifies login and client-key create/rotate against a mock API; the remaining routes are still unverified. The boot failure path is visible.
* That all mock fault fields are effective: control and request-local patch endpoints are wired, but
  a subset of fault knobs still needs implementation/coverage.
* That the latency budget is enforced. `tools/bench/` has no harness.
* That the panel's hedge editor round-trips through the backend. It does not: `panel/format.js`
  (`parseWinnerFail`/`buildWinnerFail`) uses an externally tagged shape for `hedge.on_winner_fail`
  (`{"rerun-excluding":{"max_rounds":2}}`), while `WinnerFailMode` is internally tagged
  (`{"mode":"rerun-excluding","max_rounds":2}`) and rejects the panel's shape. Saving hedge settings
  from the panel therefore cannot store a winner-fail mode, and a config written elsewhere is
  mis-rendered. `panel/README.md` records the defect; `crates/mr-core/src/dialect.rs` has a
  regression test pinning the correct form.

## 6. Preserved partial attempts

Three pieces of real work exist only as gitignored drafts, because each was left in a state that does
not compile and reverting the tracked files was the only way to keep the tree green. They are
resumable and they are worth reading before starting over:

| Path | What it is |
|---|---|
| `.research/incomplete-cli-attempt/` | `mr-cli`: 805-line `main.rs`, 544-line `output.rs`, its `Cargo.toml`. Declares eleven modules and writes two of them, so it fails to build. The *layout* is the useful part. |
| `.research/store-incomplete-attempt.rs` | `mr-core/src/store.rs`: 867 lines, truncated mid-`impl`. References `struct Store` and `struct UsageWriter`, neither of which was ever written. |
| `.research/incomplete-mock-faults-attempt.rs` | `tools/mock-upstream`: 1360 lines attempting the `faults.rs` contract. Superseded — the committed `lib.rs` compiles and its 13 tests pass, so start from that, not this. |

These are gitignored deliberately. Shipping a file that does not compile, to record that someone once
tried, trades a working tree for a note — and the note can live outside the build.

## 7. Why this file is in the repository

A reader deciding whether to use this project needs to know which of its sentences are backed by
running code. Putting that question in one place, with a command beside each answer, is cheaper for
them than discovering it one 404 at a time.
