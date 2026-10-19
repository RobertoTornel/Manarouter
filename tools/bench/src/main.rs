//! `manarouter-bench` — measure the latency the relay itself adds.
//!
//! # The question
//!
//! Manarouter states a latency budget for itself: under **150 µs of added latency at p50** and
//! under **500 µs at p99**, over talking to the provider directly
//! (`docs/16-latency-engineering.md` §6). Nothing measured it. This tool does, and it answers one
//! question — *does the relay add more than the budget?*
//!
//! # How
//!
//! A fake upstream is started on `127.0.0.1` and a one-provider relay is pointed at it. The same
//! request is then measured twice: once directly against the fake upstream, and once through the
//! relay's router, driven in-process so that no client-side TCP hop is counted as the relay's
//! cost. The difference between the two is the relay's own added overhead.
//!
//! No network beyond loopback, no real credential, no cost. The credential is a synthetic string
//! in a temporary vault that is deleted when the run ends.
//!
//! # What this tool is not
//!
//! It is not a CI gate. Microsecond-scale measurements on a shared machine are dominated by
//! whatever else the machine is doing, so this is a **local, report-or-threshold** tool: run it on
//! a quiet machine, read the numbers, and treat a breach as a reason to re-run rather than as a
//! verdict on the relay. It is deliberately not wired into any workflow.
//!
//! # Detached from the root workspace
//!
//! This crate has its own `[workspace]` table, so it cannot break `cargo check --workspace` in
//! the repository root, and every dependency is pinned to an exact version rather than inherited
//! from the root's `[workspace.dependencies]`. The versions are the ones the root workspace
//! resolves to, mirrored by hand — see `Cargo.toml`.

mod cli;
mod error;
mod harness;
mod stats;

use std::path::Path;
use std::time::Instant;

use crate::cli::{Request, HELP};
use crate::error::{Error, Result};
use crate::harness::{Harness, Measurement};
use crate::stats::{judge, summarize, Summary, Verdict, PERCENTILE_METHOD};

/// Exit status for a budget breach.
const EXIT_BUDGET: i32 = 1;
/// Exit status for a hard error.
const EXIT_ERROR: i32 = 2;

fn main() {
    // The relay's state records the configuration directory even when it never reads it, so point
    // it at a temporary one: this tool must not touch the operator's real configuration.
    let config_dir = tempfile::tempdir().expect("a temporary directory for the configuration path");

    let request = match cli::parse(std::env::args().skip(1)) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("manarouter-bench: {message}");
            std::process::exit(EXIT_ERROR);
        }
    };

    let options = match request {
        Request::Help => {
            print!("{HELP}");
            std::process::exit(0);
        }
        Request::Run(options) => *options,
    };

    // Set before the runtime starts any thread: the environment is process-wide.
    std::env::set_var("MANAROUTER_CONFIG_DIR", config_dir.path());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("manarouter-bench: the async runtime did not start: {e}");
            std::process::exit(EXIT_ERROR);
        }
    };

    let outcome = runtime.block_on(run(&options));

    match outcome {
        Ok(report) => {
            print!("{}", report.summary);
            if let Err(e) = write_json(&report, options.json.as_deref()) {
                eprintln!("manarouter-bench: the JSON report was not written: {e}");
                std::process::exit(EXIT_ERROR);
            }
            if report.verdict.is_pass() || options.report_only {
                std::process::exit(0);
            }
            eprintln!(
                "manarouter-bench: the added overhead is over budget: {}",
                report.reasons.join("; ")
            );
            std::process::exit(EXIT_BUDGET);
        }
        Err(e) => {
            eprintln!("manarouter-bench: {e}");
            std::process::exit(EXIT_ERROR);
        }
    }
}

/// Everything the run produced.
struct Report {
    /// The human-readable summary.
    summary: String,
    /// The verdict.
    verdict: Verdict,
    /// Why the verdict went the way it did, when it went that way.
    reasons: Vec<String>,
    /// The added-overhead statistics, in microseconds.
    added: Summary,
    /// The baseline statistics, in microseconds.
    baseline: Summary,
    /// The relay statistics, in microseconds.
    relay: Summary,
    /// The URL both series were measured against.
    upstream_url: String,
    /// The p50 budget the measurement was judged against, in microseconds.
    budget_p50_us: f64,
    /// The p99 budget the measurement was judged against, in microseconds.
    budget_p99_us: f64,
    /// The raw series, so the report can be re-analysed without re-measuring.
    measurement: Measurement,
    /// The relay's upstream pool settings: idle seconds and connections per host.
    pool: (u64, usize),
    /// The warmup samples that were discarded.
    warmups: usize,
    /// The measured samples.
    samples: usize,
}

/// Run the measurement and build the report.
async fn run(options: &cli::Options) -> Result<Report> {
    let harness = Harness::start().await?;
    let pool = harness.pool_settings();

    let started = Instant::now();
    let measurement = harness::measure(&harness, options.warmups, options.samples).await?;
    let wall = started.elapsed();

    let added =
        summarize(&measurement.added_us).ok_or_else(|| Error::new("no samples were measured"))?;
    let baseline = summarize(&measurement.baseline_us)
        .ok_or_else(|| Error::new("no baseline samples were measured"))?;
    let relay = summarize(&measurement.relay_us)
        .ok_or_else(|| Error::new("no relay samples were measured"))?;

    let verdict = judge(
        added.p50,
        added.p99,
        options.budget_p50_us,
        options.budget_p99_us,
    );
    let reasons = match &verdict {
        Verdict::Pass => Vec::new(),
        Verdict::Fail(reasons) => reasons.clone(),
    };

    let summary = render_summary(
        options,
        &measurement,
        &baseline,
        &relay,
        &added,
        &verdict,
        &reasons,
        harness.upstream_url(),
        pool,
        wall,
    );

    Ok(Report {
        summary,
        verdict,
        reasons,
        added,
        baseline,
        relay,
        upstream_url: harness.upstream_url().to_owned(),
        budget_p50_us: options.budget_p50_us,
        budget_p99_us: options.budget_p99_us,
        measurement,
        pool,
        warmups: options.warmups,
        samples: options.samples,
    })
}

/// The human-readable summary.
#[allow(clippy::too_many_arguments)]
fn render_summary(
    options: &cli::Options,
    measurement: &Measurement,
    baseline: &Summary,
    relay: &Summary,
    added: &Summary,
    verdict: &Verdict,
    reasons: &[String],
    upstream_url: &str,
    pool: (u64, usize),
    wall: std::time::Duration,
) -> String {
    let mut out = String::new();
    out.push_str("manarouter-bench — the relay's own added latency\n");
    out.push_str(&format!(
        "  upstream        : {upstream_url} (local fake upstream)\n"
    ));
    out.push_str(&format!(
        "  upstream pool   : {} s idle, {} connections per host\n",
        pool.0, pool.1
    ));
    out.push_str(&format!(
        "  warmups         : {} (measured, then discarded)\n",
        options.warmups
    ));
    out.push_str(&format!(
        "  samples         : {} (measured, and reported)\n",
        options.samples
    ));
    out.push_str(&format!(
        "  wall clock      : {:.3} s\n",
        wall.as_secs_f64()
    ));
    out.push('\n');
    out.push_str(&format!(
        "  baseline (direct to the upstream) : p50 {:>9.1} µs   p99 {:>9.1} µs   max {:>9.1} µs\n",
        baseline.p50, baseline.p99, baseline.max
    ));
    out.push_str(&format!(
        "  relay    (through the relay)       : p50 {:>9.1} µs   p99 {:>9.1} µs   max {:>9.1} µs\n",
        relay.p50, relay.p99, relay.max
    ));
    out.push_str(&format!(
        "  added overhead (relay - baseline)   : p50 {:>9.1} µs   p90 {:>9.1} µs   p99 {:>9.1} µs   max {:>9.1} µs\n",
        added.p50, added.p90, added.p99, added.max
    ));
    out.push('\n');
    out.push_str(&format!(
        "  budget           : p50 < {:.0} µs, p99 < {:.0} µs  (docs/16-latency-engineering.md §6)\n",
        options.budget_p50_us, options.budget_p99_us
    ));
    match verdict {
        Verdict::Pass => out.push_str("  verdict          : PASS\n"),
        Verdict::Fail(_) => {
            out.push_str("  verdict          : FAIL\n");
            for reason in reasons {
                out.push_str(&format!("                    - {reason}\n"));
            }
        }
    }
    out.push_str(&format!(
        "  percentiles      : {PERCENTILE_METHOD} (every reported percentile is an observed sample)\n"
    ));
    out.push_str(&format!(
        "  samples used     : {} baseline, {} relay, {} differences\n",
        measurement.baseline_us.len(),
        measurement.relay_us.len(),
        measurement.added_us.len()
    ));
    out.push('\n');
    out.push_str(
        "  This is a local, report-or-threshold tool, not a CI gate: microsecond measurements on a\n  \
         shared machine are dominated by machine load. Re-run it on a quiet machine before\n  \
         believing a breach.\n",
    );
    out
}

/// Write the machine-readable report.
fn write_json(report: &Report, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let json = serde_json::json!({
        "tool": "manarouter-bench",
        "generated_at_unix_s": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        "upstream_url": report.upstream_url,
        "upstream_pool": {
            "idle_secs": report.pool.0,
            "max_idle_per_host": report.pool.1,
        },
        "warmups": report.warmups,
        "samples": report.samples,
        "percentile_method": PERCENTILE_METHOD,
        "budget_us": {
            "p50": report.budget_p50_us,
            "p99": report.budget_p99_us,
        },
        "added_overhead_us": summary_json(&report.added),
        "baseline_us": summary_json(&report.baseline),
        "relay_us": summary_json(&report.relay),
        "verdict": if report.verdict.is_pass() { "pass" } else { "fail" },
        "reasons": report.reasons,
        "samples_us": {
            "baseline": report.measurement.baseline_us,
            "relay": report.measurement.relay_us,
            "added_overhead": report.measurement.added_us,
        },
        "note": "Local, report-or-threshold measurement of the relay's own added latency. \
                 No network, no credentials, no cost. Not a CI gate.",
    });
    let text = serde_json::to_string_pretty(&json)
        .map_err(|e| Error::new(format!("the report did not serialise: {e}")))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::new(format!("{} could not be created: {e}", parent.display()))
            })?;
        }
    }
    std::fs::write(path, format!("{text}\n"))
        .map_err(|e| Error::new(format!("{} could not be written: {e}", path.display())))
}

/// A summary as JSON.
fn summary_json(summary: &Summary) -> serde_json::Value {
    serde_json::json!({
        "count": summary.count,
        "min": summary.min,
        "p50": summary.p50,
        "p90": summary.p90,
        "p99": summary.p99,
        "max": summary.max,
        "mean": summary.mean,
    })
}
