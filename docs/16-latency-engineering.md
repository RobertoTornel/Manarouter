# Latency engineering

Manarouter's requirement was unusually blunt: *optimise obsessively for latency; move everything not
related to sending and receiving requests off the router.* This document records what that actually
means in code, why each choice was made, and what was deliberately **not** done.

The short version: the design is the optimisation. Each individual technique below saves microseconds;
the reason the relay is fast is that combo logic, configuration parsing and policy evaluation are not
on the request path at all. A micro-optimisation in a design that parses JSON per request is a
rounding error.

---

## 1. The measured baseline this was built against

These are real numbers taken on the development machine, not estimates.

| Operation | Measured |
|---|---|
| Loopback TCP connect | ~0.14 ms |
| An existing Javascript router's `/v1/models`, **purely local, zero upstream work** | **5.2 – 9.2 s** TTFB |
| The same router's zero-upstream-work 429 short-circuit | 22 – 295 ms |
| The same router's `/v1/chat/completions` cold, before returning 429 | 46 s |

The `/v1/models` figure is the damning one. That endpoint does no network I/O at all — it reads a
list out of a database and serialises it. It takes seconds. That is not a tuning problem; it is what
happens when an admin dashboard, a database handle and a garbage collector share an event loop with
the code that is supposed to answer instantly.

The target for the relay, therefore, is not "faster than that". It is **under 150 µs p50 of added
latency**, which is within a factor of two of a bare loopback round trip and therefore close to the
floor for any design that talks to a remote provider at all.

---

## 2. Architectural decisions

These are the ones that matter. Everything in section 3 is a detail by comparison.

### 2.1 Zero IPC on the request path

The naive way to honour "move everything off the router" is a second process that the router asks
for decisions. That trades a latency problem for a worse one: a socket round trip, a serialisation,
a context switch and a scheduler decision on **every request**, plus a new way for traffic to stop
when the helper is busy.

Instead the control plane **compiles** the routing decision into a table and the relay reads it. There
is no call to make, so there is nothing to wait for. See `docs/04-routing-snapshot.md`.

### 2.2 Combination logic resolved ahead of time

Which combos exist, what order their steps are in, which accounts each step may use, which steps
participate in a hedge, what the window is, what happens when the winner dies — all of that is
resolved by the conductor at configuration-change time.

The relay walks a `ComboPlan`: a flat array of fully-resolved step records. No string parsing, no
alias resolution, no account filtering, no strategy dispatch beyond a `match` on a `u8`.

### 2.3 The relay evaluates no policy

A useful test when reviewing a proposed relay change: *does this require knowing something that is
not already in the snapshot and not directly observable from the socket?* If yes, it belongs in the
conductor. This rule has no exceptions, and it is why the relay has no configuration model and no
database handle.

### 2.4 Passthrough by default

When the client dialect and the upstream dialect match — `openai → openai`, `anthropic → anthropic` —
the request bytes and the response bytes are forwarded unchanged.

On the response side this is more than a CPU saving: it means the relay never buffers a streamed
response. Bytes are read from the upstream and written to the client as they arrive, so a long
generation starts reaching the user immediately and the relay's memory use is independent of
response length.

`mr_dialects::translate_request` signals "no work needed" with `Ok(None)` rather than by returning
the input, so the fast path is also the shape the type system encourages. The non-streaming response
translator is not called at all when the dialects match.

### 2.5 Hot reload without dropping requests

Routing changes must not cost in-flight requests, or operators will batch changes and do them at
3 a.m. The relay holds the compiled table behind an `arc_swap::ArcSwap`:

- a reader performs one atomic load — no lock, no contention, no writer interaction;
- a writer installs a new `Arc`, and readers already holding the old one finish against it;
- nothing is ever blocked, and no request is ever failed because the configuration changed.

An RSS regression is deliberately avoided: the old table is freed as soon as the last request
holding it completes.

---

## 3. Micro-optimisations, in order of how much they actually buy

### 3.1 Locating `model` without parsing JSON

**This is the largest single win on the path.** Deserialising a 200 KB agent conversation to extract
one string costs tens of microseconds and thousands of allocations; scanning for it costs one linear
pass and zero allocations.

`crates/manarouter-relay/src/model_scan.rs` implements a depth-tracking, string-aware scanner that:

- uses `memchr2` to jump between quotes and backslashes, so string skipping runs at memory bandwidth
  rather than one byte per iteration;
- tracks nesting so a `model` inside a tool definition or a cached message does not win;
- tracks escapes so the literal text `"model"` inside a conversation *about* JSON does not win;
- allocates nothing and returns byte ranges borrowing from the original body;
- takes the **first** occurrence of a duplicated key, matching what a streaming JSON reader sees.

It is not a validator and does not pretend to be. Its own contract is documented in the module and
enforced by tests that cover the pathological cases, including a body with 200 levels of array
nesting and a body with a trailing backslash.

The relay also reads `stream` and `n` in the same single pass, because re-establishing the scan state
three times would cost three times as much for no benefit.

### 3.2 Connection reuse

A TCP connect plus a TLS handshake is one to three round trips and, for a distant provider, tens to
hundreds of milliseconds. That dwarfs everything the router itself does.

The relay keeps a pool of warm keep-alive connections per upstream authority, with
`pool_max_idle_per_host` and `pool_idle_secs` configurable per provider. `prewarm` opens a connection
at startup so the first request does not pay for it.

Care is taken not to confuse *keep-alive* with *connection count*: four idle connections per host is
the default because a hedge may need three at once, and a fourth concurrent request should not queue
behind a pool that is momentarily exhausted.

### 3.3 A separate TTFB timeout

The hedged race keys on time to first byte. Using a single total-request timeout for that decision
would kill a model that is thinking hard but about to answer well, and would call a model healthy
merely because it opened a stream quickly.

`connect_timeout_ms`, `ttfb_timeout_ms`, `idle_timeout_ms` and `total_timeout_ms` are therefore four
independent settings. `ttfb_timeout_ms` is the one the race consults.

### 3.4 HTTP/2 where it helps

Most providers speak HTTP/2. It multiplexes concurrent hedged requests to the same host over one
connection, which removes head-of-line setup cost precisely in the case where the relay is issuing
several requests at once. `http2` is a per-provider preference (`auto`, `required`, `disabled`)
because a few providers behave worse on h2 than h1.

### 3.5 `TCP_NODELAY`

Enabled by default on both sides. Nagle's algorithm delays a small write while waiting for an ACK;
for a request that is a few hundred bytes followed by a wait for the first token, that delay is pure
added latency. For streamed token-by-token responses the same applies in reverse.

### 3.6 `SO_REUSEPORT` with one listener per worker

With `reuse_port` enabled, the relay binds one listening socket per CPU with `SO_REUSEPORT`. The
kernel distributes incoming connections across them, so there is no single accept queue and no
thundering herd when several workers wake for one connection.

The choice to use `SO_REUSEPORT` rather than a shared listener is deliberate: a shared listener means
every accept wakes every worker, and the wakeup cost grows with core count. The tradeoff is that
per-connection distribution is the kernel's hash rather than the runtime's judgement, which is
acceptable here because the relay's work per connection is uniform.

### 3.7 mimalloc

Declared in `lib.rs` rather than `main.rs`, specifically so that `tools/bench` links the same
allocator the shipping binary uses. A benchmark that measures a different allocator is measuring the
wrong program.

The relay's pattern — many short-lived buffers, allocated and freed on different threads as hyper
tasks migrate between workers — is where a general-purpose allocator is weakest. mimalloc's
thread-local free lists and deferred reclamation handle cross-thread frees far better than glibc's
per-arena locking.

### 3.8 Build profile

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
strip = true
```

`lto = "fat"` plus `codegen-units = 1` costs build time and buys inline-ability across crate
boundaries, which matters because the dialect check, the snapshot lookup and the header build are all
small cross-crate functions that should vanish into the caller.

`panic = "abort"` removes landing pads and therefore unwinding tables, shrinking the binary and
letting the optimiser reason more freely. The cost is real and worth naming: **a panic terminates the
process rather than failing one request.** For the relay this is the right trade — a panic on the
request path means the invariants the hot path relies on have been violated, and continuing to serve
traffic from an unknown state is worse than restarting. The relay is designed so that its only
panics are genuine invariant violations, and the supervisor restarts it.

`[profile.dev.package."*"] opt-level = 2` keeps dependencies optimised in debug builds, because an
unoptimised `rustls` or `hyper` makes local profiling meaningless.

### 3.9 Fire-and-forget usage accounting

Usage data — tokens, latency, which participant won — is recorded, but never synchronously. The relay
pushes a record onto a bounded channel and the conductor batches writes to SQLite. If the channel is
full the record is **dropped and counted**, not retried.

That is a deliberate choice: an observability feature must never be able to apply backpressure to the
thing it observes. The panel surfaces the dropped-record count so the loss is visible rather than
silent.

---

## 4. Explicitly rejected optimisations

Recording these matters, because each one looks attractive and each one trades correctness for a
number nobody would notice.

| Rejected | Why |
|---|---|
| Serving requests directly from the conductor with no relay | Removes the process boundary that keeps a slow `fsync` off the request path. This is the reference implementation's mistake. |
| A shared-memory ring buffer between the processes | Sub-microsecond, and adds a synchronisation protocol whose failure modes are far worse than the latency it saves. The compiled snapshot already achieves zero IPC. |
| Caching upstream responses | Changes semantics: two identical prompts should not silently return one answer. Caching is a proxy's job, not a router's. |
| Skipping the request-body scan and requiring `model` in a header | Breaks compatibility with every existing client. The scan is already sub-microsecond. |
| A custom HTTP parser on the hot path | `hyper` is already among the fastest available and is the component most likely to be exercised by adversarial input. Writing a bespoke parser to save microseconds would be trading a well-audited implementation for an unaudited one. |
| Reading the request body fully before routing | The scan needs only the head. Buffering a 200 KB body to read its first 200 bytes is pure cost, and it also defeats streaming. |
| `unwrap()` on the hot path | A panic with `panic = "abort"` is a process death. Every fallible hot-path operation returns a value the caller handles. |

---

## 5. How the numbers are verified

Two mechanisms, and the distinction between them matters.

### `tools/bench` against `tools/mock-upstream`

`tools/mock-upstream` answers with a fixed, tiny payload after a configurable delay, so the time
measured is the relay's and not the upstream's. The benchmark asserts that p50 added latency is under
150 µs and p99 under 500 µs, and it runs in CI. A regression therefore fails a test rather than being
discovered in production.

The mock is deterministic — identical request bytes produce identical response bytes — so a
comparison between two runs is meaningful and the benchmark does not flake.

### Opt-in live comparison

`docs/19-latency-benchmarks.md` records measurements against real providers, taken deliberately with
real credentials and clearly separated from the controlled numbers above.

Real providers are unsuitable for a pass/fail assertion — their latency varies by orders of magnitude
with time of day, load and geography — but they are the only honest way to answer "how does this
compare to what I was using before". Presenting a mock number as though it were a real-world one
would be misleading, so the two are never mixed.

---

## 6. The budget, restated

| Stage | Target p50 | Target p99 |
|---|---|---|
| Accept + request line | < 15 µs | < 60 µs |
| Client-key authentication | < 2 µs | < 10 µs |
| `model` scan | < 5 µs | < 30 µs |
| Snapshot lookup | < 1 µs | < 5 µs |
| Account selection | < 1 µs | < 5 µs |
| Write to upstream socket | < 20 µs | < 80 µs |
| **Total added** | **< 150 µs** | **< 500 µs** |

If a proposed change cannot be accounted for within one of these rows, it does not belong on the
request path.

---

## See also

- `docs/01-architecture.md` — why there are two processes at all
- `docs/04-routing-snapshot.md` — the compiled table the relay reads
- `docs/07-hedged-race.md` — the one feature that deliberately spends money to save latency
- `docs/19-latency-benchmarks.md` — the measured numbers
- `crates/manarouter-relay/src/model_scan.rs` — the largest single hot-path win, with its contract
