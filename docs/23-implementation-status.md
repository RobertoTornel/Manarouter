# 23 — Implementation status

This file exists because a specification that reads as finished while the code is not is worse than
no specification. Everything below is checked against the tree, and the command that checks it is
given so you can re-run it.

> **Last audited:** working tree at `main` (`8b32c3d` plus uncommitted local edits); source and docs reviewed during the current session. Run the verification commands below again after changes.

## 1. What runs

| Component | State | Evidence / limitation |
|---|---|---|
| `mr-core` — config, snapshot compiler, vault, usage store, OAuth | **Substantial, not complete** | Codex, Copilot, and Antigravity OAuth authorization/token exchange/refresh/Google userinfo identity code is wired; Antigravity IDE client details remain reverse-engineered and unverified. |
| `mr-dialects` — OpenAI, Anthropic, Responses, Google, Command Code translation | **Reachable, test coverage uneven** | Relay dispatches request and response translation per selected step. Responses implementation needs focused regression tests. |
| `manarouter-relay` — HTTP API, auth, snapshot, account selection, translation | **Runs; hedging not wired** | Sequential step loop remains the request executor; `hedge_decide` is not called by the relay. |
| `manarouter-conductor` — auth, API, state, serving, quota query | **Implemented** | API routes and Store-backed `/api/quota` exist. Verify live separately; no conductor was running during this audit. |
| `mr-cli` | **Partial** | Account/provider/combo/key/model families dispatch; client keys can now be rotated in place via `mr key rotate`; `serve`, `doctor`, `migrate`, `config`, and `vault` still return not-implemented errors. |
| `panel/` | **Implemented, browser behavior unverified** | JS syntax checks pass and bootstrap now displays failures; no browser-driven route/login verification completed. |
| OAuth | **Partial; flows wired** | Conductor OAuth routes cover Codex, Copilot, and Antigravity authorization code flows; tokens are vault-backed, metadata lives in SQLite, and relay resolves/refreshes credentials. Antigravity's interactive loopback OAuth exchange/refresh, v2 userinfo identity, free-tier Cloud Code onboarding with bounded LRO polling, encrypted token context persistence, and account-bound relay project injection are implemented. Paid-tier onboarding remains fail-closed without explicit project input. T2 client metadata and PKCE are reverse-engineered/unverified; no live Google login or Cloud Code request was exercised. Cline remains unavailable. |
| `tools/mock-upstream` | **Fault control API wired; behaviors partial** | Detached crate tests pass; `POST /__control` and request-local `x-mock-fault` merge patches are reachable. Not every field in `Fault` is yet applied to response generation. |
| `tools/bench` | **Absent** | No harness currently enforces the latency budget. |

Verification run during this session:

```sh
cargo test --workspace
cargo fmt --manifest-path tools/mock-upstream/Cargo.toml
cargo test --manifest-path tools/mock-upstream/Cargo.toml  # 22 passed
node --check panel/app.js
```
Workspace tests exited successfully. Re-run the full quality gates before release:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
bash scripts/check-secrets.sh
```

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

The remaining work is a runner that spawns the participants, feeds `Participant` states to
`hedge_decide`, and cancels losers under the `MidstreamPolicy` guard. That guard — refusing to splice
once any `tool_calls` delta has been emitted — is the reason this is not a small change.

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
* **Conductor API/store exist.** The panel API includes CRUD routes and `/api/quota`; quota currently
  reports persisted aggregate usage rather than live provider billing windows.

## 5. Claims that remain unverified or false

* That hedging is live. See §3: runner remains sequential.
* That Antigravity works end to end. Authorization, token exchange, refresh, and Google userinfo
  identity are implemented, but Cloud Code onboarding and project injection/context are incomplete.
  The registered T2 OAuth client/PKCE behavior is reverse-engineered and unverified; do not rely on
  production access until a user explicitly opts into a live login and validates the upstream API.
  Codex and Copilot also still need real-provider opt-in smoke verification.
* That the provider catalogue was executed against every entry. `docs/providers/catalog.md` marks
  uncertain entries and the catalogue is not a live reachability guarantee.
* That the panel works in a real browser. Syntax and asset/API checks are not a substitute for browser
  interaction; login and all nine routes remain unverified. The boot failure path is now visible.
* That all mock fault fields are effective: control and request-local patch endpoints are wired, but
  a subset of fault knobs still needs implementation/coverage.
* That the latency budget is enforced. `tools/bench/` has no harness.

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
