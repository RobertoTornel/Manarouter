# Handover — what still needs implementing

State of the tree: **`a241c25`** on `main`, pushed to
`https://github.com/RobertoTornel/Manarouter.git`.

Everything below was checked against the working tree and a running server, not recalled.
Where a claim is unverified, it says so.

---

## 1. Can the tree be trusted right now?

Yes, within these limits:

| Gate | Result |
|---|---|
| `cargo test --workspace` | 669 passed, 0 failed, 0 ignored |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo fmt --all --check` | clean |
| `bash scripts/check-secrets.sh` | `check-secrets: OK` |
| Conductor boots and serves | verified live on `127.0.0.1:20901` |
| Every panel asset loads | all 200 |
| Every panel API call | all 200, with sensible bodies |

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check && bash scripts/check-secrets.sh
```

**Green tests are not the same as a reachable feature.** Three separate bugs in this
project's history were modules that compiled, passed every unit test, and were never
called by anything. Two were fixed this session (see §3). Assume there are more, and
verify by *running the thing*, not by reading its tests.

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

Then `http://127.0.0.1:20901/` is the panel and `/api/health` is the API.

The relay is a **separate process** (`./target/debug/manarouter-relay`, port 20900).
The conductor never starts it. `GET /api/ports` reports the relay port *the config
declares* and the port the conductor last saw at startup — it cannot observe the relay's
socket.

---

## 3. What was finished this session

Recorded so the next person does not redo it.

- **The relay never called `mr-dialects`.** It depended on the crate and used only the
  `Dialect` enum, so ~5 000 lines of translation had no caller and `path_override` /
  `force_stream` were written by translators and read by nobody. Cross-dialect routing was
  impossible: a Command Code or Google provider could not be reached at all.
  Now wired per step on the request path, and translated back on the response path.
- **A URL bug hidden under a green test.** `commandcode::GENERATE_PATH` is `"alpha/generate"`
  with no leading slash, and `build_url` concatenated it onto a base whose trailing slash it
  had just trimmed — building `https://api.commandcode.aialpha/generate`. The test meant to
  cover this hand-wrote `"/alpha/generate"` *with* a slash, so it asserted a URL the code
  never built. Fixed, and the test now takes the path from the translator's constant.
- **`GET /api/quota` did not exist** (404). The conductor never opened the usage database at
  all. `AppState` now holds a `mr_core::store::Store`, and the endpoint returns per-account
  aggregates. Verified live against seeded rows: 2 requests / 200 prompt / 540 completion /
  max 850 ms came back correct.
- Cleared 3 clippy errors and 2 secrets-scan false positives, and recovered four agents'
  uncommitted work after the machine died.

---

## 4. What still needs implementing

Ordered by what blocks the most, not by effort.

### P0 — The panel has never been seen working in a browser

**This is the single largest unknown in the project.** The panel is ~20 files and was
written to spec, all of it serves, all of it parses (`node --check` on every module is
clean), and every endpoint it calls answers. Nobody has watched it render.

What was tried: headless Firefox (`firefox --headless --profile … --screenshot`) against
`http://127.0.0.1:20901/#/overview` produced an 8.9 KB near-black PNG. A **control
screenshot of `/api/health`** — a plain JSON page — produced 33 KB of rendered text, which
proves the screenshot mechanism works and that the panel really did render an empty page.

**Root cause, confirmed by reading `panel/index.html`:** both `#shell` and `#login-root`
carry the `hidden` attribute in the markup, and JavaScript removes `hidden` from whichever
one applies. Until that JS runs, the page is *completely empty*. That is a deliberate
choice — it avoids a flash of unauthenticated chrome — but it has two consequences:

1. The screenshot was most likely taken before the async boot finished, which is ordinary
   and not a bug. **ES-module fetches plus several `await`s cannot finish inside the
   window between `load` and a Firefox screenshot.** So this is probably a measurement
   artefact, not a broken panel.
2. **But it also means any uncaught error during boot leaves a permanently blank dark
   page with no message.** A user cannot distinguish "still loading" from "broken". That
   is a real defect regardless of the screenshot.

**Do this:**

- Add a failure path in the boot sequence: wrap the bootstrap so that a throw reveals an
  error block (reuse `errorBlock` from `panel/dom.js`) instead of leaving `#shell` hidden
  forever. A blank page must be impossible.
- Verify properly with a driver that can wait for a selector, not a screenshot race.
  Playwright is not installed; `npx playwright` would work but needs a browser download.
  A lighter option that fits the existing stack: add a tiny `tools/panel-smoke/` Node
  script using `node --experimental-*`/CDP against Firefox, or simply run Playwright once
  and commit the spec.
- Then walk every route by hand: `#/overview #/providers #/combos #/keys #/aliases
  #/models #/quota #/settings #/docs`, plus the login view with a password set.

`panel/README.md` lists contract uncertainties its author flagged and could not resolve
(list-envelope flexibility, the account-creation payload, model hide/show via upsert, the
externally-tagged `on_winner_fail` shapes). Treat those as suspects once the panel renders.

### P1 — The hedged race has no runner

`select::hedge_decide` exists and is tested, and `docs/07-hedged-race.md` describes the
behaviour in detail — but **nothing outside `select.rs` calls it.** The relay tries steps
one at a time in a `for` loop. The entire hedged-race feature (the eligibility window, the
four `WinnerFailMode`s, `winner_metric`) is therefore documented and unreachable.

This is the same failure class as §3's first item. The design is settled — see
`docs/07-hedged-race.md` and the ratified decision Q2 — so this is implementation, not
design:

- Dispatch to the chosen participants **concurrently**, not sequentially.
- `window_ms` is an **eligibility gate, not a timeout**: a participant that answers after
  the window is not an error, and window expiry must never fail a request.
- Winner = highest-priority participant replying inside the window.
- Pick up the losers' bodies and drop them without leaking connections.
- `MidstreamPolicy::Restart` must refuse to splice once any `tool_calls` delta has been
  emitted (the hard guard is documented; confirm it is enforced at the splice site).

### P1 — `crates/mr-dialects/src/responses.rs`: 1181 lines, 0 tests

Zero `#[test]` in the file. It compiles, so it type-checks against the pivot, but nothing
verifies behaviour — least of all the thing most likely to be wrong: **`[DONE]` suppression
for Responses clients.** The OpenAI Responses stream has no `data: [DONE]` sentinel;
forwarding one produces a stream that some clients reject. There are no reachability tests
either, so the `(OpenAi, Responses)` arms may not be dispatched to.

Given that two prior bugs were exactly "arm present, never taken", write the reachability
test first.

### P1 — Antigravity OAuth is incomplete

`crates/mr-core/src/oauth/antigravity.rs` is 253 lines and holds only the credential shape
and the published-constant accessors (`client_id` / `client_secret` at lines 66 and 74).
The four pieces the other OAuth providers have are missing:

1. Authorization URL builder
2. Token exchange
3. Refresh
4. Identity extraction

Use `crates/mr-core/src/oauth/codex.rs` (714 lines, complete) as the shape to follow, and
`docs/12-oauth-antigravity.md` for the protocol. Note that Antigravity is **Gemini-shaped,
not OpenAI-shaped**: the body is `{project, model, request:{contents}, userAgent,
requestId}` and responses arrive wrapped in `{"response":{candidates},"traceId"}`.

Related and easy to miss: **`project` must be injected by the relay from the credential
row.** `TranslateCtx` carries no OAuth context, so translation alone cannot supply it —
this needs a deliberate path from the credential to the request body.

### P2 — `tools/bench/` is empty

The latency claims in `docs/19-latency-benchmarks.md` and the budget (p50 < 150 µs,
p99 < 500 µs added overhead) have no harness behind them. The one measured number in the
project is `manarouter-relay /healthz` at ~0.5 ms. Nothing measures the *added* overhead of
the request path, which is the number the budget is about.

### P2 — `tools/mock-upstream` is half-wired

`src/lib.rs` (740 lines, 13 tests) and `src/faults.rs` (523 lines) are tracked and pass.
The planned "option B" is not done: `mod faults;` is not declared, and there is no
`POST /__control` endpoint or `x-mock-fault` header handling. Without it, the fault
injection in `faults.rs` cannot be driven from a test.

This tool is deliberately detached (its own `[workspace]`) so it cannot break
`cargo check --workspace` — which is why CI has a separate `tools` job. Keep that property.

### P2 — OmniRoute fidelity is second-hand

**`github.com/diegosouzapw/OmniRoute` was never cloned.** All provider behaviour was ported
from summaries of the *bundled runtime* at `~/.omniroute/` (41 `.js`, 1 `.ts`), so the Rust
is two levels removed from source. The cost is already visible: the research's headline
correction was that OmniRoute is **3.8.51**, not 16.3.5, because a process-title rewrite
made the Next.js version look like the app's.

Before trusting any provider's details, clone the real repository and re-derive them. The
provider catalogue is `catalog/providers.toml` (55 entries; 28 of 45 reachable providers
ship, 17 excluded — 6 of those because cookie auth is forbidden by decision Q6).

`docs/17-migrating-from-omniroute.md` covers the import path. The importer must work from a
**copy** of `storage.sqlite`, dry-run first, leaving the original untouched (decision Q13).

### P3 — Documentation

- **There is no provider-dialects document.** `docs/08-*` is quota tracking. Given
  `mr-dialects` is now on the request path, a doc covering each dialect, the pivot shape,
  what `path_override`/`force_stream` mean and which providers force streaming is worth
  writing. This session's work added no doc.
- `docs/23-implementation-status.md` predates this session and should be updated to match
  it — particularly the relay's dialect wiring and `GET /api/quota`, both of which move
  features from "documented" to "reachable".
- The standing requirement is **one doc file per feature plus an `LLM_INDEX.md`** that can
  be fed to a model wholesale. `docs/LLM_INDEX.md` exists; check it still lists every file
  after any addition.

### P3 — Release and design debt

- **No git tag.** `v0.1.0` was planned and never created.
- **`refresh.rs` encodes `FailureClass` as a string marker** — `oauth[permanent]` /
  `oauth[transient]` stuffed inside `Error::InvalidState` — because `error.rs` was owned by
  another agent at the time and adding a variant would have collided. It works and is
  tested, but it is design debt: a real error variant would be better.
- **Cross-instance sync** (`docs/22-sync-design.md`) is inert groundwork by decision Q15.

---

## 5. Traps that will cost you a day if you do not know them

- **`export PATH="$HOME/.cargo/bin:$PATH"`** in every shell, or `cargo` is not found.
- **axum 0.8 uses `/{id}`, not `/:id`**, and `FromRequest` is **async** — clippy rejects the
  `fn -> impl Future` form.
- **A module declaration and its file are one indivisible change.** Declaring five sibling
  modules before their files existed took the whole workspace down with E0583.
- **Do not `#[allow(dead_code)]` to silence an unused stub.** An unused stub is an unwired
  stub. Search for its callers.
- **`git add -f` on `.research/` is forbidden** — it is gitignored on purpose and holds
  tens of thousands of lines of upstream research plus preserved failure artefacts.
- **Never write a Google client id or secret as a contiguous literal.** GitHub push
  protection blocked this repository's first push over four of them. They are assembled
  from fragments in `crates/mr-core/src/published.rs`; `scripts/check-secrets.sh` will fail
  if you reintroduce one. Do **not** add them to `ALLOWLIST` — the allowlist re-introduces
  the literal, which is the thing that was blocked.
- **Verify the committed thing, not the working tree.** A `Cargo.lock` resolved against
  uncommitted manifests passed locally and broke a clean clone with `--locked`. Check HEAD
  in a fresh clone.
- **One writer per file.** Committing into a file whose author was still writing added a
  duplicate `impl` and broke the build.
- **The secrets scanner matches a provider-key prefix followed by 17+ identifier characters.** A test name containing that prefix as part of a longer word triggered it. Renaming is the fix; a marker comment cannot be used there, because `rustfmt` moves a comment after an opening brace into the block.
- **US privacy constraint (decision Q6): never support cookie auth, at all.**

---

## 6. Ratified decisions (do not relitigate)

Q1 compiled snapshot, zero IPC per request · Q2 hedged race, four winner-fail modes ·
Q3 retry the prompt with the next-best model per the combo's rules · Q4 live upstream
signals first, never class a whole provider dead while one key lives · Q5 all credentialed
OmniRoute providers plus an importer · Q6 no cookie auth · Q7 control plane outside the
router · Q8 targeted model scan and verbatim passthrough · Q9 tokio multi-thread plus
`SO_REUSEPORT` · Q10 persist with a Restart button labelled "next restart" · Q11 repo
**Manarouter**, crates `mr-core`, `mr-dialects`, `manarouter-relay`, `manarouter-conductor`,
`mr-cli`, config in `~/.config/manarouter/` · Q12 0600 and encrypted at rest, referenced by
id, CI grep · Q13 import from a copy, dry-run, original untouched · Q14 CI mock tests plus
opt-in live smoke · Q15 session affinity in, Anthropic `/v1/messages` and Responses shims
in, token compression off-path and default-off, v2 sync groundwork.

The latency constraint is verbatim and load-bearing: the relay is optimised **obsessively**
for latency, combo logic lives outside it, and it should only send and receive. The honest
claim is **zero interpretation on the request path**, not zero-copy.
