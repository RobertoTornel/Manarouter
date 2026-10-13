# Latency benchmarks

This document records what has been measured, what has not, and how any number here was obtained.
It is deliberately short on numbers, because at the time of writing the relay's own overhead has
**not** been measured. Everything in the "budget" section is a target. The distinction is the whole
point of the document.

## 1. The baseline that motivated the project

These are observations of a **Javascript reference router** on one machine, taken before Manarouter
existed, while investigating why it felt slow. They are single-machine spot measurements, not a
study: no repetitions, no percentile reporting, and no control for machine load. They are recorded
because they set the shape of the problem, not because they are precise.

| Operation | Observed | What it tells you |
|---|---|---|
| `GET /v1/models`, purely local, zero upstream work | **5.2 – 9.2 s** TTFB | The router was doing seconds of work with no network involved. |
| A 429 short-circuit with zero upstream work | **22 – 295 ms** | Even the "fast path" was not fast. |
| A cold `POST /v1/chat/completions` hitting one provider | **46 s** before returning a 429 | Under a retry chain, a single request could occupy the better part of a minute. |
| Loopback TCP connect | **~0.14 ms** | The floor. Whatever a router adds is measured against this, and it is three orders of magnitude below a second. |

The conclusion drawn was not "the Javascript router is badly written" — it is, in places, but that is
not the interesting part. The interesting part is that **a request path which interprets anything at
all will eventually be doing seconds of work**, because interpretation accretes. Each feature added a
lookup, a middleware, a re-parse. None of them were individually expensive.

That is why the architecture is what it is: combos are compiled to a flat binary snapshot ahead of
time, and the relay does no interpretation per request. See
[Routing snapshot](04-routing-snapshot.md) and [Latency engineering](16-latency-engineering.md).

## 2. The budget, stated as a budget

The per-stage targets from [Latency engineering §6](16-latency-engineering.md#6-the-budget) are:

| Stage | p50 target | p99 target |
|---|---|---|
| Accept | 15 µs | 60 µs |
| Auth | 2 µs | 10 µs |
| Model scan | 5 µs | 30 µs |
| Snapshot lookup | 1 µs | 5 µs |
| Account select | 1 µs | 5 µs |
| Upstream write | 20 µs | 80 µs |
| **Total added overhead** | **< 150 µs** | **< 500 µs** |

**These are targets.** They were derived by budgeting the loopback floor across the stages, and they
are enforced (once the data plane is complete) by an assertion in CI, not by hope. A target that
nothing checks is a wish.

To put the scale in context: at 150 µs of added overhead against a provider whose own TTFB is 400 ms,
the router is responsible for 0.04% of what the user waits for. That is the correct place for it to
be. The value the router adds is not speed against a single provider — it is not spending seconds,
and being able to race several providers in parallel.

## 3. What has actually been measured

**Nothing, in the relay.** The data plane is still being built. There is no honest number to publish,
and this section will stay empty rather than be filled with an estimate.

This is worth being explicit about because it is exactly the kind of gap that gets papered over. A
plausible-sounding "~40 µs of overhead" invented for a README is worse than a blank, because the
first person to measure it will find the claim false and stop trusting everything else in the
document.

## 4. How it will be measured

### Against the mock, not against a provider

`tools/mock-upstream` is a deterministic HTTP server that answers the OpenAI, Anthropic and Google
shapes with a configurable delay and a configurable body. Every benchmark that produces a claim about
**the router's own overhead** runs against it.

Determinism is not a nicety here, it is what makes the number meaningful. A benchmark against a real
provider measures the provider, the network, the time of day and the router's contribution summed
together, and the router's share is unidentifiable within the noise. Worse, for a benchmark that runs
in CI: a latency assertion against a real provider is flaky, a flaky assertion gets disabled or given
a wide tolerance, and a disabled or over-toleranced assertion protects nothing. The mock gives a
distribution narrow enough that a regression of tens of microseconds fails the build, which is the
only version of this measurement that has any effect.

### The method

```bash
# Terminal 1 — the deterministic upstream, 5 ms of deliberate delay.
cargo run --release -p mock-upstream -- --port 21900 --delay-ms 5

# Terminal 2 — the relay, pointing at a config whose only provider is the mock.
cargo run --release -p manarouter-relay -- --config-dir /tmp/mr-bench

# Terminal 3 — the benchmark.
cargo run --release -p mr-bench -- --requests 20000 --concurrency 1 --url http://127.0.0.1:20900
```

Rules for the run, each of which exists because ignoring it produces a number that is wrong in a
direction that flatters the project:

- **`--release`.** A debug build's overhead is dominated by unoptimised bounds checks, and the number
  would be a statement about the compiler flags.
- **An idle machine.** A benchmark run while a `cargo build` is in the other window measures the
  scheduler.
- **Loopback.** A remote target adds a network whose jitter exceeds everything being measured.
- **Warm the pool first.** The first request pays for a TCP and TLS handshake, which is
  milliseconds. That is a real cost — which is why `prewarm` exists — but it is not the per-request
  overhead the budget describes, and mixing the two produces a p99 that describes nothing.
- **Report percentiles, not a mean.** A mean latency is the one statistic that hides the problem: it
  is the tail that users notice.
- **Report the mock's delay alongside the result**, so the reader can subtract it and see what is
  left.

### The mock-versus-live rule

**A mock number and a live number are never presented together, and never compared.**

- A **mock** number answers *"how much does the router add?"* It is a controlled measurement of this
  code.
- A **live** number answers *"is this faster than what I had?"* It is a measurement of the whole
  path, and it is dominated by providers.

Presenting the first as though it were the second is the most common way a router project misleads
its own users. A single-provider live comparison also has to state which provider, from where, at
what time of day, and how many samples, or it is an anecdote with a decimal point.

## 5. Results

| Date | Build | Workload | p50 | p99 | Notes |
|---|---|---|---|---|---|
| — | — | — | not measured | not measured | The relay's data plane is not complete. |

This table is empty on purpose and will be filled only from `mr-bench` output, with the command line
recorded alongside it.

## 6. What will not go in this document

- Numbers from a machine that is not described.
- Numbers from a debug build.
- A mean without a percentile.
- A provider's latency presented as the router's.
- An extrapolation from a smaller run, or from a microbenchmark of one function, presented as
  end-to-end overhead.
- Any figure carried over from the reference router as though it were this project's.

## See also

- [Latency engineering](16-latency-engineering.md) — the techniques and the full budget.
- [Routing snapshot](04-routing-snapshot.md) — why there is no interpretation per request.
- [Architecture](01-architecture.md) — the two-process split.
- [Troubleshooting](20-troubleshooting.md) — what to check when requests are slow.
