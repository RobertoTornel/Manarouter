# Handover — project state and what still needs implementing

State of the tree: **`9b7c47c`** on `main`, **10 commits ahead of `origin/main`** and not yet pushed.
Everything below was checked against this working tree and, where stated, against a running server.
Where a claim is unverified, it says so. Nothing here is recalled from an earlier session.

Supersedes the previous handover (which described `a241c25`). Section 6 records what changed since,
because most of that document's P0/P1 list is now finished and leaving it in place would send the
next person to work that is already done.

---

## 1. Can the tree be trusted right now?

Yes, within these limits:

| Gate | Result |
|---|---|
| `cargo test --workspace --all-targets` | **731 passed, 0 failed**, 1 ignored (opt-in live test) |
| `cargo test -p mr-core --test imported_grants_are_independent -- --ignored` | passes; 10/10 imported grants usable with the reference stopped |
| same test with `MANAROUTER_FORCE_REFRESH=1` | **9/10 refresh**; the tenth is the account an earlier destructive test killed (see §3) |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all -- --check` | clean |
| `bash scripts/check-secrets.sh` | `check-secrets: OK` (134 files) |
| `python3 tests/smoke_e2e.py` | PASS — isolated conductor + relay + mock upstream, routed a chat request |
| `bash scripts/test-installer-systemd.sh` | passed (isolated HOME; install, update, `--no-start`) |
| Panel browser E2E — **Firefox** | PASS: login, one-time client-key lifecycle, low-resource settings save |
| Panel browser E2E — **Chromium** | PASS (same spec) |

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check && bash scripts/check-secrets.sh
python3 tests/smoke_e2e.py && bash scripts/test-installer-systemd.sh
```

Test distribution, so a failure points somewhere:

| Target | Tests |
|---|---|
| `mr-core` lib | 329 |
| `manarouter-relay` lib | 137 |
| `mr-dialects` lib | 133 |
| `mr-cli` bin | 88 |
| `manarouter-conductor` lib | 27 |
| `manarouter-relay` bin | 17 |
| `mr-core` integration (`imported_grants_are_independent`) | 1, **ignored by default** |

**Green tests are not the same as a reachable feature.** Three separate bugs in this project's
history were modules that compiled, passed every unit test, and were never called by anything. The
most recent example, found and fixed in `9ec3284`, was an imported model list whose *synthetic*
entries the upstream rejects — the tests were green and the accounts were unusable. Assume more
exist, and verify by *running the thing*.

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
systemctl --user restart manarouter-relay.service     # needed after a config/snapshot change
```

**The relay reads the compiled snapshot at startup.** It holds a `reload()` that re-reads the config
and recompiles (`crates/manarouter-relay/src/state.rs:260`), but **nothing calls it** — so editing the
config or importing accounts has no effect until the relay is restarted. See §4.

### Shape of the codebase

| Crate | Lines | Role |
|---|---|---|
| `mr-core` | ~20 800 | Config, snapshot compiler, vault, store, OAuth platforms, catalogue |
| `mr-dialects` | ~7 600 | Request/response translation against the pivot |
| `mr-cli` | ~6 800 | `mr` command surface |
| `manarouter-relay` | ~5 900 | Data plane: auth, routing, upstream dispatch |
| `manarouter-conductor` | ~4 400 | Control plane and web panel API |
| `panel/` | 19 files | Panel front end (plain ES modules, no framework) |

`catalog/providers.toml` holds **55 providers**: 49 `open-ai` dialect, and one each of `codex`,
`copilot`, `antigravity`, `command-code`, `anthropic`, `google`. **Four** declare OAuth —
`openai-codex`, `github-copilot`, `antigravity` and `cline` — but only three are registered
(`crates/mr-core/src/oauth/mod.rs:585`). `cline` is deliberately listed in the registry test's
`NOT_IMPLEMENTED` set (`:610`) with the reason "no verified interactive flow", so the gap is asserted
rather than latent: a test fails if a catalogue platform has no implementation and is not on that
list. `mr serve`, `mr config` and
`mr vault` parse their own help but every body still returns `"is not implemented yet"`.

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

---

## 4. What still needs implementing

Ordered by what blocks the most.

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
(`server.rs:884`) and asserted (`:1620`), but translation arms reachable only in combination are
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
  `9ec3284` and now match the code.

### P3 — Release and design debt

- **No git tag.** `v0.1.0` was planned and still does not exist (`git tag -l` is empty).
- **`refresh.rs`'s `FailureClass` string marker is gone.** It is a real enum now
  (`crates/mr-core/src/oauth/refresh.rs:51`), so this earlier item is closed. The `oauth[permanent] …`
  text is now only the `Display` form at the head of an `InvalidState` message (`:123`).
- **Cross-instance sync** (`docs/22-sync-design.md`) is inert groundwork by decision Q15.
- **8 commits are unpushed.** `origin/main` is 8 behind; the earlier handover's URL is the right
  remote. Push before the machine changes hands.

---

## 5. Traps that will cost you a day if you do not know them

- **`export PATH="$HOME/.cargo/bin:$PATH"`** in every shell, or `cargo` is not found.
- **The relay does not hot-reload.** `State::reload()` exists and nothing calls it. After any config
  or snapshot change, `systemctl --user restart manarouter-relay.service`.
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
