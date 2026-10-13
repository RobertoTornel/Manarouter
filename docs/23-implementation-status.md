# 23 — Implementation status

This file exists because a specification that reads as finished while the code is not is worse than
no specification. Everything below is checked against the tree, and the command that checks it is
given so you can re-run it.

> **Last verified:** commit `fa78ae7`.

## 1. What runs

| Component | State | Evidence |
|---|---|---|
| `mr-core` — config, snapshot compiler, vault, published constants, ids, dialects | **Complete** | `cargo test -p mr-core` → 150 pass |
| `mr-dialects` — OpenAI↔Anthropic translation, SSE | **Partial** | 73 pass; `responses`, `google`, `commandcode` are stubs returning `Unsupported` |
| `manarouter-relay` — upstream client, model scan, state, selection, HTTP surface | **Request path complete** | `cargo test -p manarouter-relay` → 138 pass |
| `manarouter-conductor` — auth | **Auth complete** | 27 pass |
| `manarouter-conductor` — API, state, serving | **Not built** | no `api.rs`, `state.rs`, `serve.rs` |
| `mr-cli` | **Not built** | 23-line stub |
| `panel/` | **Written, never rendered** | 10 views, 164 KB raw / 43.9 KB gzip; no browser was available to run it |
| OAuth flows | **Not implemented** | `crates/mr-core/src/oauth/` is empty |
| `mr-core::store` | **Not implemented** | 29-line stub |
| `tools/mock-upstream`, `tools/bench` | **Skeletons** | own `[workspace]` tables; `mock-upstream` has no source yet |

Full gate, all green at the commit above:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace          # 388 pass, 0 fail
cargo run -p manarouter-relay -- --port 20900   # boots, serves /healthz on loopback
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

## 4. Other properties that are claimed and true

* **Zero interpretation on the request path.** The model field is found by a `memchr` scan, not a JSON
  parse, and the body is forwarded byte-for-byte. This is what Q8 ratified. It is *not* "zero-copy" —
  the body is buffered — and the documentation never says zero-copy.
* **No lock on the request path.** The snapshot is installed via `ArcSwap`; account health is read as
  one immutable view per request.
* **No credential in a URL.** `AuthKind::QueryParam` is recognised and deliberately not applied.
* **`/v1/models` contacts no provider.** It is served from the compiled snapshot.

## 5. Claims that would be false

* That hedging is live. See §3.
* That OAuth login works for Codex, Antigravity or Copilot. The *constants* and the dossier-derived
  flow descriptions exist in `docs/10`–`docs/14` and `mr-core::published`; no code performs a flow.
* That the provider catalogue was executed against all 55 entries. `docs/providers/catalog.md` marks
  10 UNCERTAIN, and each was written from documentation rather than a live call.
* That the panel works. It has never been rendered — there is no browser on this machine. `panel/README.md` carries a seven-step manual checklist and names six response shapes that were inferred because the conductor API does not exist to check them against.

## 6. Why this file is in the repository

A reader deciding whether to use this project needs to know which of its sentences are backed by
running code. Putting that question in one place, with a command beside each answer, is cheaper for
them than discovering it one 404 at a time.
