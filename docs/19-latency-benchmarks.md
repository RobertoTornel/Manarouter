# Latency benchmarks

This document records what has been measured, what has not, and how any number here was obtained.
The relay's **total** added overhead has now been measured (section 3); the per-stage rows in the
budget are still targets. Keeping that distinction is the whole point of the document.

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

The per-stage targets from [Latency engineering §6](16-latency-engineering.md#6-the-budget-restated) are:

| Stage | p50 target | p99 target |
|---|---|---|
| Accept | 15 µs | 60 µs |
| Auth | 2 µs | 10 µs |
| Model scan | 5 µs | 30 µs |
| Snapshot lookup | 1 µs | 5 µs |
| Account select | 1 µs | 5 µs |
| Upstream write | 20 µs | 80 µs |
| **Total added overhead** | **< 150 µs** | **< 500 µs** |

**These are targets**, derived by budgeting the loopback floor across the stages. Only the **Total**
row is measured so far, by `tools/bench` (`manarouter-bench`), which exits non-zero on a breach. It is
not a CI gate: microsecond timings on a shared machine are too noisy to fail a build, so the tool is
run by hand on a quiet machine. The per-stage rows remain unmeasured targets.

To put the scale in context: at the measured 14.6 µs of added overhead against a provider whose own
TTFB is 400 ms, the router is responsible for under 0.004% of what the user waits for. That is the
correct place for it to be. The value the router adds is not speed against a single provider — it is
not spending seconds, and being able to race several providers in parallel.

## 3. What has actually been measured

**The total, not the stages.** `tools/bench` (`manarouter-bench`) sends each request twice — straight
to a loopback fake upstream, then through the shipping relay's router in process — and reports the
difference. One release-build run on an idle machine, 300 samples: **14.6 µs p50, 41.8 µs p90,
110.5 µs p99, 209.9 µs max**, inside the 150/500 µs budget. The per-stage rows are still unmeasured:
there is no per-stage instrumentation, so none is published. The measurement is noisy (a run taken
while the machine was compiling reported p99 1040 µs), so a breach is a reason to re-run, not a
verdict.

This is worth being explicit about because it is exactly the kind of gap that gets papered over. A
plausible-sounding "~40 µs of overhead" invented for a README is worse than a blank, because the
first person to measure it will find the claim false and stop trusting everything else in the
document.

## 4. How it is measured

### Against a fake upstream, not against a provider

`tools/bench` starts its own fake upstream on loopback. `tools/mock-upstream` is a separate
deterministic HTTP server (OpenAI, Anthropic and Google shapes, `--delay` in milliseconds,
configurable body) used to drive the relay as a real process. A claim about **the router's own
overhead** comes from the in-process run, so neither a provider nor a client-side TCP hop is inside
the number.

Determinism is not a nicety here, it is what makes the number meaningful. A benchmark against a real
provider measures the provider, the network, the time of day and the router's contribution summed
together, and the router's share is unidentifiable within the noise. Worse, for a benchmark that runs
in CI: a latency assertion against a real provider is flaky, a flaky assertion gets disabled or given
a wide tolerance, and a disabled or over-toleranced assertion protects nothing. The mock gives a
distribution narrow enough that a regression of tens of microseconds shows up in the percentiles,
which is the only version of this measurement that has any effect.

### The method

```bash
# One process: it starts its own fake upstream, uses a temporary vault and config directory,
# touches no network beyond loopback, uses no real credential and costs nothing.
cargo run --release --manifest-path tools/bench/Cargo.toml -- --warmups 20 --samples 300
```

`--report-only` never fails the run; `--json <path>` also writes a machine-readable report. The
crate is a detached workspace, so `cargo run -p manarouter-bench` from the repository root does not
work: use the `--manifest-path` form above (or `cd tools/bench`) and add flags after `--`.

To exercise the same path end to end, `tests/local_latency_bench.py` drives the real relay process
against `tools/mock-upstream` over sockets and writes a percentile report. Unlike the in-process
bench it includes the relay's accept path and the client-side network hop, so it answers a different
question and its numbers are not the budget above.

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
- **Report the fake upstream's injected delay alongside the result**, so the reader can subtract it
  and see what is left.

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
|---|---|---|---|---|---|---|---|---|
| — | release, idle machine | 300 samples, in-process | 14.6 µs added | 110.5 µs added | loopback fake upstream; p90 41.8 µs, max 209.9 µs; `manarouter-bench --warmups 20 --samples 300` |

Rows are added only from a run whose command line is recorded beside it. The row above is a single
run, not a distribution of runs, and the tool's README warns that a run taken under load can be an
order of magnitude worse. Per-stage numbers stay absent until there is per-stage instrumentation.

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