//! Command-line parsing, kept apart from the measurement so the flags can be read on their own.

use std::path::PathBuf;

/// The budget from `docs/16-latency-engineering.md` §6, in microseconds.
pub const DEFAULT_BUDGET_P50_US: f64 = 150.0;
/// The budget from `docs/16-latency-engineering.md` §6, in microseconds.
pub const DEFAULT_BUDGET_P99_US: f64 = 500.0;
/// The default number of warmup samples, which are measured and then thrown away.
pub const DEFAULT_WARMUPS: usize = 10;
/// The default number of measured samples.
pub const DEFAULT_SAMPLES: usize = 100;

/// The bounds the counts are held to, so a typo cannot ask for a million requests.
pub const MAX_WARMUPS: usize = 1000;
/// The bounds the counts are held to, so a typo cannot ask for a million requests.
pub const MAX_SAMPLES: usize = 10_000;

/// What the command line asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Samples measured before the statistics start, and excluded from them.
    pub warmups: usize,
    /// Samples that make up the reported statistics.
    pub samples: usize,
    /// The p50 budget for the relay's added overhead, in microseconds.
    pub budget_p50_us: f64,
    /// The p99 budget for the relay's added overhead, in microseconds.
    pub budget_p99_us: f64,
    /// Report the numbers but never fail on a breach.
    pub report_only: bool,
    /// Where to write the machine-readable report, if anywhere.
    pub json: Option<PathBuf>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            warmups: DEFAULT_WARMUPS,
            samples: DEFAULT_SAMPLES,
            budget_p50_us: DEFAULT_BUDGET_P50_US,
            budget_p99_us: DEFAULT_BUDGET_P99_US,
            report_only: false,
            json: None,
        }
    }
}

/// What the command line asked the program to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Run the measurement.
    Run(Box<Options>),
    /// Print the help text and exit successfully.
    Help,
}

/// The help text.
pub const HELP: &str = "\
manarouter-bench — measure the relay's own added latency overhead

What it does
  Starts a fake upstream on 127.0.0.1, points a one-provider relay at it, and measures the
  same request twice: once directly against the fake upstream, and once through the relay's
  router. The difference is the latency the relay itself adds. No network, no credentials and
  no cost: the upstream is local and the credential is synthetic.

  The relay path is driven in-process, so there is no client-side TCP hop in the measurement
  and what is left is the relay's own work.

How noisy is this?
  A lot. Microsecond-scale measurements on a shared machine are dominated by whatever else the
  machine is doing: a background build, a thermal excursion or a page cache miss moves the
  numbers more than the relay does. This is a local, report-or-threshold tool, not a CI gate.
  It is deliberately not wired into CI, and a breach on a busy machine is a reason to re-run it
  on a quiet one, not a verdict on the relay.

Usage:
  manarouter-bench [options]

Options:
  --warmups <n>          Samples measured and discarded before the statistics (default 10,
                         allowed 0..=1000).
  --samples <n>          Samples in the reported statistics (default 100, allowed 1..=10000).
  --budget-p50-us <n>    p50 budget for the added overhead, in microseconds (default 150).
  --budget-p99-us <n>    p99 budget for the added overhead, in microseconds (default 500).
  --report-only          Always exit 0, even when the budget is breached.
  --json <path>          Also write a machine-readable report to this file.
  -h, --help             Print this help and exit.

Exit status:
  0  the measurement ran and the budget was met (or --report-only was given)
  1  the budget was breached
  2  the measurement could not be made

The default budgets are the ones stated in docs/16-latency-engineering.md §6: under 150 µs of
added latency at p50, under 500 µs at p99.
";

/// Parse the command line.
pub fn parse<I>(args: I) -> Result<Request, String>
where
    I: IntoIterator<Item = String>,
{
    let mut options = Options::default();
    let mut args = args.into_iter().peekable();

    while let Some(arg) = args.next() {
        // `--flag=value` and `--flag value` are both accepted.
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };

        match flag.as_str() {
            "-h" | "--help" => return Ok(Request::Help),
            "--report-only" => options.report_only = true,
            "--warmups" => {
                let value = take_value(&flag, inline, &mut args)?;
                options.warmups = parse_count(&value, "--warmups", MAX_WARMUPS, 0)?;
            }
            "--samples" => {
                let value = take_value(&flag, inline, &mut args)?;
                options.samples = parse_count(&value, "--samples", MAX_SAMPLES, 1)?;
            }
            "--budget-p50-us" => {
                let value = take_value(&flag, inline, &mut args)?;
                options.budget_p50_us = parse_budget(&value, "--budget-p50-us")?;
            }
            "--budget-p99-us" => {
                let value = take_value(&flag, inline, &mut args)?;
                options.budget_p99_us = parse_budget(&value, "--budget-p99-us")?;
            }
            "--json" => {
                let value = take_value(&flag, inline, &mut args)?;
                options.json = Some(PathBuf::from(value));
            }
            other => {
                return Err(format!(
                    "unknown option {other:?}\n\nRun `manarouter-bench --help` for the options."
                ))
            }
        }
    }

    Ok(Request::Run(Box::new(options)))
}

/// The value of a flag, whether it was written `--flag value` or `--flag=value`.
fn take_value<I>(flag: &str, inline: Option<String>, args: &mut I) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    match inline {
        Some(value) => Ok(value),
        None => args.next().ok_or_else(|| {
            format!("{flag} needs a value\n\nRun `manarouter-bench --help` for the options.")
        }),
    }
}

/// A count, bounded so a typo cannot ask for a million requests.
fn parse_count(value: &str, flag: &str, max: usize, min: usize) -> Result<usize, String> {
    let parsed: usize = value
        .parse()
        .map_err(|_| format!("{flag} needs a whole number, got {value:?}"))?;
    if parsed < min || parsed > max {
        return Err(format!(
            "{flag} must be between {min} and {max}, got {parsed}"
        ));
    }
    Ok(parsed)
}

/// A budget in microseconds.
fn parse_budget(value: &str, flag: &str) -> Result<f64, String> {
    let parsed: f64 = value
        .parse()
        .map_err(|_| format!("{flag} needs a number of microseconds, got {value:?}"))?;
    if !parsed.is_finite() || parsed < 0.0 {
        return Err(format!(
            "{flag} must be a non-negative number, got {value:?}"
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(args: &[&str]) -> Options {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        match parse(owned).expect("should parse") {
            Request::Run(options) => *options,
            Request::Help => panic!("expected a run"),
        }
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let options = opts(&[]);
        assert_eq!(options.warmups, DEFAULT_WARMUPS);
        assert_eq!(options.samples, DEFAULT_SAMPLES);
        assert_eq!(options.budget_p50_us, 150.0);
        assert_eq!(options.budget_p99_us, 500.0);
        assert!(!options.report_only);
        assert!(options.json.is_none());
    }

    #[test]
    fn flags_are_read_in_both_spellings() {
        let spaced = opts(&["--samples", "30", "--budget-p50-us", "10"]);
        let inline = opts(&["--samples=30", "--budget-p50-us=10"]);
        assert_eq!(spaced, inline);
        assert_eq!(spaced.samples, 30);
        assert_eq!(spaced.budget_p50_us, 10.0);
    }

    #[test]
    fn help_is_recognised() {
        let owned = vec!["--help".to_string()];
        assert_eq!(parse(owned).unwrap(), Request::Help);
        let owned = vec!["-h".to_string()];
        assert_eq!(parse(owned).unwrap(), Request::Help);
    }

    #[test]
    fn counts_are_bounded() {
        assert!(parse(vec!["--samples".into(), "0".into()]).is_err());
        assert!(parse(vec!["--samples".into(), "10001".into()]).is_err());
        assert!(parse(vec!["--warmups".into(), "1001".into()]).is_err());
        assert!(parse(vec!["--warmups".into(), "0".into()]).is_ok());
        assert!(parse(vec!["--samples".into(), "10000".into()]).is_ok());
    }

    #[test]
    fn a_missing_value_is_an_error_not_a_panic() {
        assert!(parse(vec!["--samples".into()]).is_err());
        assert!(parse(vec!["--json".into()]).is_err());
    }

    #[test]
    fn an_unknown_flag_is_an_error() {
        assert!(parse(vec!["--nope".into()]).is_err());
    }

    #[test]
    fn the_help_text_says_the_measurement_is_noisy() {
        assert!(HELP.contains("noisy"));
        assert!(HELP.contains("not a CI gate"));
    }
}
