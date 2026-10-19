# Architecture

This document explains how Manarouter is put together and, more importantly, **why**. It is written
to be read by a person about to change the code, and by a language model that has been handed this
repository and needs to know where things live.

---

## 1. The one-sentence version

Two Rust processes: a **relay** that does nothing but move request and response bytes as fast as the
operating system allows, and a **conductor** that owns all the thinking — configuration, provider
and account selection, quota accounting, the panel — and hands the relay a pre-compiled routing
table it can read without parsing anything.

---

## 2. Why two processes

The requirement that shaped everything else was this:

> the router should be optimised obsessively for latency; everything not related to sending and
> receiving requests should be moved off it. Combo logic is done outside the router; the router
> should only have to send out messages and receive them, and should be primed to do so.

There were three ways to honour that, and two of them are wrong.

**Wrong option 1: one process, with the panel and routing logic on the same runtime.** This is what
both reference projects do, and it is why one of them measures 5–9 seconds to serve a purely local
`/v1/models` request. A React dashboard, a SQLite write, an OAuth refresh scheduler and a garbage
collector all share a runtime with the code that is supposed to answer in microseconds. Javascript's
event loop is the extreme case, but a Rust process with a database handle and an admin HTTP server
on the same executor has the same failure mode in miniature: a slow `fsync` blocks a request that
had nothing to do with it.

**Wrong option 2: two processes with an IPC call per request.** This is the obvious fix and it
trades one problem for a worse one. If the relay must ask the conductor "which provider should this
go to?", then every single request pays a socket round trip, a serialisation, a wakeup on another
process, and a scheduler decision — tens to hundreds of microseconds on a good day, and an unbounded
stall when the conductor is busy doing something else. It also means the conductor becomes a single
point of failure for traffic, which is exactly the coupling we were trying to remove.

**What Manarouter does: compile the decision, not the request.** The conductor does all the thinking
*in advance* and writes the result into a flat binary **routing snapshot**. The relay memory-maps
that file and holds it behind an `ArcSwap`. A request needs no IPC at all: it looks up an entry in a
hash table that is already in its address space.

The combination logic — priority ordering, round-robin rotation state, reset-aware cooldowns,
hedge participant selection — is genuinely *outside* the router. It is resolved into a table. The
relay executes a table-driven procedure with no interpretation, no allocation, and no branch that
depends on configuration structure.

One part of that table is compiled but not yet executed: hedging is resolved into the snapshot and
`hedge_decide` is unit-tested, but the relay still walks a combo's steps sequentially, so nothing on
the request path calls it. See §4 and [`23-implementation-status.md`](23-implementation-status.md).

The tradeoff is honest and worth stating: **the relay is fast at the cost of being dumb.** It cannot
make a judgement call, because a judgement call would need context it deliberately does not have. If
a routing decision requires information that only exists at request time, that information must be
something the relay can read from the snapshot or observe directly from the socket.

---

## 3. The request path, step by step

```
client ──► relay :20900
             │
             ├─ 1. accept                    pre-bound listener, SO_REUSEPORT set (exactly one socket today)
             ├─ 2. authenticate              hash a client key, compare against the snapshot
             ├─ 3. scan for "model"          memchr over the raw body, NO full JSON parse
             ├─ 4. resolve                   one hash lookup in the snapshot's index
             ├─ 5. pick a plan               pre-compiled: ordered steps, account order, hedge plan
             ├─ 5b. open the credential      AES-GCM open of the chosen account's secret, from the vault
             ├─ 6. open or reuse a socket    pooled keep-alive connection to the upstream
             ├─ 7. forward the bytes         verbatim when the dialects match
             ├─ 8. pipe the response back    verbatim, streaming, never buffered whole
             └─ 9. record health             in-memory success/failure and cooldown state
```

Step 9 is deliberately the only bookkeeping on the path, and it stays in memory. The relay does
**not** record token usage today, and neither does anything else: the `Store` has a `usage` table
and the conductor has a read-only `/api/quota` that aggregates it, but no code path in either
service constructs a `UsageRecord` or feeds the queue, so the table stays empty and `/api/quota`
reports nothing. The fire-and-forget usage writer the design calls for is still unimplemented; see
[`23-implementation-status.md`](23-implementation-status.md).

Steps 3 to 6 are the whole latency story, and each of them is deliberately a constant-time
operation on data that is already resident — with one deliberate exception. The chosen account's
credential is decrypted from the vault as the request is built (step 5b), once per account attempt,
rather than held in plaintext for the process lifetime: an AES-GCM open of a short blob is cheap
enough, and keeping every provider secret resident in a long-lived process is the worse trade. The
vault file itself is opened once at startup.

### Step 3: scanning for the model, not parsing the JSON

The relay needs exactly one field out of the request body: `model`. Parsing a 200 KB conversation
into a `serde_json::Value` to read one string is tens of microseconds and allocates hundreds of
times. Instead the relay uses `memchr` to find the literal `"model"` and reads the string that
follows it, with a bounded lookahead.

This is *not* a general JSON parser and it is not pretending to be one. Its correctness rests on one
observable property: the first `"model"` key in a well-formed request body is the one the client
meant. A pathological body that puts `"model"` inside a string literal earlier in the document could
mislead it, and the mitigation is not cleverness but **detection**: the scan accepts a match only if
it is followed by `:` and a quoted string within a small window, and rejects a match preceded by a
backslash.

Because the request is forwarded verbatim anyway, a mis-scan cannot corrupt the body. The worst
outcome is choosing the wrong route for a deliberately malformed request, which is the same
behaviour as every other proxy on the wire.

### Step 4: the compiled snapshot

The snapshot is a single file, `~/.local/state/manarouter/routes.snapshot`, whose layout is defined
by `mr-core/src/snapshot.rs`. It carries a magic header, a format version, a digest and a checksum,
then a string table and a set of flat records.

The relay validates the header and checksum **before** building anything, so a truncated or
corrupted file is rejected rather than half-loaded. It then decodes once, into owned structures, and
installs the result behind an `ArcSwap`. Decoding happens on load, not per request — so the honest
claim is *zero parsing per request*, not *zero copies ever*. Stating it precisely matters, because
the first claim is true and the second would not be.

Swapping is atomic. Something writes a new snapshot file — the conductor's API, `mr config`, or any
operator edit that `mr` accepts — and the relay picks it up on its own: it polls the file's
`(mtime, length)` on a timer rather than waiting for a signal, which also means a hand-edited
configuration is adopted without restarting either process. The relay then installs it. In-flight
requests keep the `Arc` they already hold and finish against the old table; the new table serves
everything that arrives afterwards. **No request is ever dropped because the configuration
changed.** That is why `ArcSwap` was chosen over a lock: a reader never blocks, and a writer never
waits for readers to drain.

### Step 5: the plan is already computed

A combo is compiled into a `ComboPlan`: an ordered list of steps, each naming a provider, a model
and the account order to try within that provider, plus the resolved hedge plan and the retry
policy. Nothing about that requires the request. The relay walks the list.

Rotation state is the one thing that must change as requests arrive, and it stays in the relay as a
relaxed `AtomicU64` per combo (`manarouter-relay/src/state.rs`). The counters live *beside* the
decoded table inside the same swapped value, not inside the snapshot file: two separate swaps would
let a request read a new combo list with counters from an older generation. A consequence worth
knowing is that a snapshot swap starts the counters over — harmless for round-robin fairness, and
the reason the counter array is rebuilt with the table rather than persisted.

### Steps 7 and 8: passthrough dominates

`Dialect::is_passthrough_to` answers one question: can these bytes go upstream unread?
`openai → openai` and `anthropic → anthropic` both say yes, and in that case the relay forwards the
original buffer and never constructs a value from it. This is the common case in practice, because
people route OpenAI clients to OpenAI-shaped providers.

Translation only runs on a genuine mismatch, and `mr-dialects` is written so that "no translation
needed" is the cheapest outcome: `translate_request` returns `Ok(None)` to mean *send the original
bytes*, and the non-streaming response translator is never called at all.

The same idea applies to streaming. In the matching case the relay finds frame boundaries with
`memchr` and forwards the slices unchanged, so byte-for-byte fidelity is automatic. It only parses
frames when it has to rewrite their payloads.

---

## 4. What lives where

| Concern | Lives in | Notes |
|---|---|---|
| Listening, TLS, connection pooling | `manarouter-relay` | The whole data plane. |
| Reading the routing table | `manarouter-relay` | `ArcSwap` load; no locks. |
| Dialect detection and passthrough | `mr-core::dialect` | A `const fn` on two enums. |
| Dialect translation | `mr-dialects` | Not on the path when dialects match. |
| Hedged race execution | **not wired** | `hedge_decide` exists and is unit-tested, but the relay runs the sequential step loop, so nothing on the request path calls it. |
| Combo and key-strategy compilation | `manarouter-conductor` | Resolved into ordered steps and account orders once per config change, not per request. |
| Request-time step and account choice | `manarouter-relay` | `select.rs` walks the compiled steps, applies the key strategy and advances the rotation counters. |
| Quota accounting | `manarouter-conductor` | `/api/quota` reads usage rows from `Store`, but nothing writes them yet; the relay applies transient cooldowns from its own in-memory health state. |
| OAuth authorization (login) | `manarouter-conductor` | The relay holds a `Refresher` per platform and refreshes an expiring token in front of a request. |
| The web panel | `manarouter-conductor` | Same process, but a separate task and a separate port. |
| Credential storage | `mr-core::secret` | Encrypted at rest; the vault is opened once at startup and an account's secret is decrypted when a request tries that account (step 5b). |
| Usage history, quota aggregates, the oplog | `mr-core::store` | SQLite, WAL. The `usage` and `account_health` tables have no writer yet, so they stay empty. Reads are off the hot path, except the small `oauth_credential` metadata lookup an OAuth account needs before its token is refreshed. |

The rule of thumb: **if it can be decided before a request arrives, it belongs in the conductor. If
it can only be known while bytes are moving, it belongs in the relay.** Anything that is neither is
a bug in the design. One qualification is deliberate: the conductor decides the *order* of steps and
accounts, but the relay makes the final pick from that order at request time, because that pick
depends on the live health map the relay owns.

---

## 5. Why the panel is on a different port

The panel listens on `:20901`, the relay on `:20900`. They are different processes with different
privileges:

- the relay holds decrypted upstream credentials in memory and can spend the operator's money;
- the panel can be reached from a browser, which means it can be reached by a malicious page the
  operator visits.

Keeping them apart means a cross-site request forgery against the panel cannot cause a request to be
proxied, and a compromised relay cannot rewrite its own configuration. The panel's API is
credential-minting and therefore guarded by an Argon2id password hash and an origin check; the relay
has no administrative endpoints at all.

This is the specific mistake both reference projects made — one serves its dashboard and its
inference API from one Next.js process on one port, with no privilege separation between them.

---

## 6. Failure behaviour

The design principle is: **degrade to stale, never to broken.**

| Failure | Behaviour |
|---|---|
| Conductor crashes | The relay keeps serving the last snapshot indefinitely. Routing is frozen, not broken. The panel is unavailable. |
| Snapshot file is corrupt | The relay refuses it and keeps the previous one. It logs loudly. It never loads a table it cannot verify. |
| Snapshot version is newer than the relay understands | Refused, previous table retained. Version skew degrades to stale routing rather than a crash. |
| Relay crashes | A supervisor restarts it: the installed systemd units set `Restart=`. `mr serve` deliberately does not — it stops the other service and returns the dead child's exit code, so a crash loop is visible rather than hidden. Clients see a connection failure for the restart window. |
| An upstream account returns 401 | That *account* is marked failed — never the whole provider. Another account on the same provider is tried. |
| An upstream returns 429 | The account is parked until its reset hint, or a default cooldown. Other accounts are unaffected. |
| Every account on a provider is unusable | The provider is reported dead, other providers still serve. |
| A stream dies mid-answer | Governed by `midstream_policy`. Never silently spliced once tool-call deltas have been sent. |

The distinction in rows 5 and 6 is the single most important operational lesson taken from the
reference implementations, and it was ratified explicitly: *do not class a whole provider as dead if
there are multiple keys or accounts and only some of them are failing.* Health is tracked per
account, and the provider's health is a rollup computed from account counts
(`ProviderHealth::from_counts`).

---

## 7. Latency budget

The relay's own overhead, excluding upstream time, is the number that matters. The budget is:

| Stage | Target (p50) | Target (p99) |
|---|---|---|
| Accept + parse the request line | < 15 µs | < 60 µs |
| Authenticate a client key | < 2 µs | < 10 µs |
| Locate `model` in the body | < 5 µs | < 30 µs |
| Snapshot lookup | < 1 µs | < 5 µs |
| Account selection | < 1 µs | < 5 µs |
| Write to the upstream socket | < 20 µs | < 80 µs |
| **Total added** | **< 150 µs** | **< 500 µs** |

For contrast, the measured baseline of an existing Javascript router on this machine, serving a
purely local `/v1/models` with no upstream involved at all, was **5.2–9.2 seconds**. The gap is not
a matter of tuning the same design; it is a different design.

`tools/bench/` is where the harness that enforces these numbers is meant to live, and it is
currently an empty directory — no automated check measures them yet, so treat the table as a design
budget, not a measured guarantee. `tools/mock-upstream` is the deterministic upstream such a harness
would drive. See `docs/16-latency-engineering.md` and
[`23-implementation-status.md`](23-implementation-status.md).

---

## 8. Cross-instance sync groundwork

Ratified decision Q15 asked for the groundwork for syncing credentials, combos and settings between
two machines. Nothing is synced today, and no transport exists. What exists is the *shape* that
makes it possible without a migration later:

- **UUIDv7 identifiers** everywhere. Monotonic, so they sort by creation time, and globally unique,
  so two machines cannot collide. A sequence number or an autoincrement id would have made this
  impossible to retrofit.
- **`revision` on every mutable row.** Last-writer-wins needs a version to compare.
- **`deleted_at` tombstones.** A hard delete cannot be replicated — the other machine never learns
  that the row should go. Every delete is a soft one.
- **A versioned configuration that preserves unknown keys.** A newer instance's fields survive a
  round trip through an older binary, so a downgrade does not silently destroy configuration.
- **An append-only `oplog`.** Written, currently unread. It is the change log a future sync would
  ship.

`docs/22-sync-design.md` describes the intended design in full.

---

## 9. Reading order for a newcomer

1. This file.
2. `mr-core/src/dialect.rs` — the shared vocabulary. Everything else is phrased in these types.
3. `mr-core/src/model.rs` — what a provider, an account, a combo and a hedge *are*.
4. `mr-core/src/snapshot.rs` — how the authored form becomes the compiled form.
5. `mr-dialects/src/lib.rs` — how translation decides whether it can be skipped.
6. `manarouter-relay/src/` — the data plane.
7. `manarouter-conductor/src/` — everything else.
8. `docs/07-hedged-race.md` — the most subtle behaviour in the project.
