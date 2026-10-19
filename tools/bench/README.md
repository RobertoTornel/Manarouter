# manarouter-bench

Measures **the relay's own added latency** and checks it against the budget stated in
[`docs/16-latency-engineering.md`](../../docs/16-latency-engineering.md) §6:

> under **150 µs** of added latency at **p50**, under **500 µs** at **p99**, over talking to the
> provider directly.

## What it does

1. Starts a fake upstream on `127.0.0.1` (a real axum HTTP server, because the relay's upstream
   client speaks real HTTP).
2. Builds a one-provider relay configuration pointing at it, with one account, one model and a
   synthetic credential in a temporary vault.
3. Measures the same request twice:
   * **baseline** — straight to the fake upstream, over the same kind of pooled HTTP/1.1
     connection the relay uses;
   * **relay** — through the relay's router, driven in-process with
     `tower::util::ServiceExt::oneshot`, so no client-side TCP hop is charged to the relay.
4. The **added overhead** of a sample is `relay − baseline`, and the tool reports the p50, p90,
   p99 and max of that difference, alongside the absolute numbers of both series.
5. It exits non-zero when the added-overhead p50 is at or above 150 µs, or the p99 is at or above
   500 µs — unless `--report-only` is given.

No network beyond loopback, no real credential, no cost. The vault lives in a temporary
directory that is removed when the run ends, and the configuration directory is pointed at a
temporary one so the operator's own `~/.config/manarouter` is never touched.

## It is not a CI gate

Microsecond-scale measurements on a shared machine are dominated by whatever else the machine is
doing. A background build or a thermal excursion moves the numbers more than the relay does.
This is a **local, report-or-threshold** tool: run it on a quiet machine, read the numbers, and
treat a breach as a reason to re-run rather than as a verdict on the relay. It is deliberately
not wired into any workflow.

## Usage

```sh
cd tools/bench
cargo build --release
./target/release/manarouter-bench --warmups 5 --samples 30 --report-only
```

| Flag | Meaning |
|---|---|
| `--warmups <n>` | Samples measured and discarded first (default 10, allowed 0..=1000). |
| `--samples <n>` | Samples in the reported statistics (default 100, allowed 1..=10000). |
| `--budget-p50-us <n>` | p50 budget for the added overhead (default 150). |
| `--budget-p99-us <n>` | p99 budget for the added overhead (default 500). |
| `--report-only` | Always exit 0, even on a breach. |
| `--json <path>` | Also write a machine-readable report. |
| `-h`, `--help` | The help text. |

Exit status: `0` met the budget (or `--report-only`), `1` the budget was breached, `2` the
measurement could not be made.

## Detached from the root workspace

This crate has its own `[workspace]` table, so it cannot break `cargo check --workspace` in the
repository root, and every dependency is pinned to an exact version rather than inherited from
the root's `[workspace.dependencies]` — the same ones the root workspace resolves to, mirrored by
hand. See `Cargo.toml`.

One caveat: `cargo fmt --all` inside this directory follows the **path dependencies** into
`crates/`, so it will also check (and, without `--check`, rewrite) the relay's and core's
sources. Use `cargo fmt -p manarouter-bench` to format only this crate.

## Tests

`cargo test` covers the pure parts — the percentile function over a known series and the budget
verdict, including the exactly-at-the-boundary case — without needing a network.
