# Local report-only latency benchmark

`tests/local_latency_bench.py` compares direct calls to the repository's mock upstream with
calls routed through Manarouter to that same mock. It is an observational tool for comparing
machines/configurations, **not** a performance gate or a production/provider benchmark. No threshold
is applied; do not interpret mock results as end-to-end model-provider latency.

## Safety and isolation

The script builds release binaries, then starts the mock upstream, conductor, and relay bound only to
`127.0.0.1`. It creates an isolated temporary config/state/HOME and inserts only a synthetic,
nonfunctional credential through `mr account add --secret-stdin`; the vault is confined to that
temporary directory. It contacts no external endpoint and reads no normal user configuration or
credentials. Child processes are stopped and temporary files removed on exit, including errors.

Direct and relay-routed requests use separate persistent HTTP connections, are run sequentially in
paired samples, and alternate which path goes first. Warmups are excluded. Every measured request
posts the same small non-streaming chat payload; the relay path includes relay routing, vault/account
lookup and local mock I/O. The direct path contacts the mock API itself. Other background load,
thermal state, compiler/build cost, OS scheduling, and loopback/network-stack overhead can affect the
results. Run multiple times and compare distributions on the same machine; use a real provider only
in a separately authorized benchmark with explicit cost/quota safeguards.

## Run

From the repository root, with Python 3 and Cargo/Rust available:

```sh
python3 tests/local_latency_bench.py --warmups 10 --samples 100
```

The script builds the `mr-cli`, conductor, relay, and detached mock-upstream binaries with
`--release`. Counts are bounded (`0..1000` warmups, `1..10000` measured samples per path); timeout
is bounded to `0.1..120` seconds. Use `--output report.json` to save a JSON report, otherwise JSON is
printed to stdout. A nonzero exit status indicates setup or request errors. Partial successful
pairs are still reported with the errors; no threshold causes a pass/fail result.

The JSON contains machine/OS/architecture information, release profile, topology and isolation
notes, settings, nearest-rank latency distributions (min, p50, p90, p95, p99, max, mean, standard
deviation) for each path and the paired `relay - direct` delta, plus request errors. The benchmark is
intentionally not run in CI and should not be used to compare unrelated hosts without accounting for
their hardware, OS, load, and thermal differences.
