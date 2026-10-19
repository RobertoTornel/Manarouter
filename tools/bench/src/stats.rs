//! The pure parts of the tool: percentile arithmetic and the budget verdict.
//!
//! Nothing in this module touches the network, the clock or the filesystem, so the numbers the
//! tool reports can be checked by reading the code, and the verdict can be tested without a
//! relay, an upstream or a machine that is quiet.

use std::cmp::Ordering;

/// The percentile convention this tool uses.
///
/// **Nearest rank**: the `p`-th percentile of `n` sorted samples is the sample at 1-based rank
/// `ceil(p/100 * n)`. No interpolation, so every reported percentile is a value that was actually
/// observed, which is the only property worth having in a latency report: an interpolated
/// percentile is a number that never happened.
pub const PERCENTILE_METHOD: &str = "nearest-rank";

/// The `p`-th percentile of `values`, by [`PERCENTILE_METHOD`].
///
/// `values` need not be sorted. Returns `None` for an empty series, because a percentile of
/// nothing is not a number.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let p = p.clamp(0.0, 100.0);
    // `ceil` of a non-negative quantity, floored at 1 so that p = 0 still names a real sample.
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    let index = rank.min(sorted.len()) - 1;
    Some(sorted[index])
}

/// The shape of a latency series as reported by this tool.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    /// How many samples the series holds. Warmups are excluded before it gets here.
    pub count: usize,
    /// The fastest sample.
    pub min: f64,
    /// The median.
    pub p50: f64,
    /// The 90th percentile.
    pub p90: f64,
    /// The 99th percentile.
    pub p99: f64,
    /// The slowest sample.
    pub max: f64,
    /// The arithmetic mean.
    pub mean: f64,
}

/// Summarise a series of microsecond measurements.
pub fn summarize(values: &[f64]) -> Option<Summary> {
    if values.is_empty() {
        return None;
    }
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    Some(Summary {
        count: values.len(),
        min,
        p50: percentile(values, 50.0).unwrap_or_default(),
        p90: percentile(values, 90.0).unwrap_or_default(),
        p99: percentile(values, 99.0).unwrap_or_default(),
        max,
        mean,
    })
}

/// What the measurement says about the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Both budgets are met.
    Pass,
    /// At least one budget is breached; the strings say which.
    Fail(Vec<String>),
}

impl Verdict {
    /// Whether the measurement met the budget.
    pub fn is_pass(&self) -> bool {
        matches!(self, Verdict::Pass)
    }
}

/// Judge a measurement against the budget.
///
/// A budget is breached when the measured value is **at or above** it: the budget is stated as
/// "under 150 µs", so a measurement of exactly 150 µs is not under it.
pub fn judge(p50_us: f64, p99_us: f64, budget_p50_us: f64, budget_p99_us: f64) -> Verdict {
    let mut reasons = Vec::new();
    if p50_us >= budget_p50_us {
        reasons.push(format!(
            "added-overhead p50 {p50_us:.1} µs is at or above the {budget_p50_us:.1} µs budget"
        ));
    }
    if p99_us >= budget_p99_us {
        reasons.push(format!(
            "added-overhead p99 {p99_us:.1} µs is at or above the {budget_p99_us:.1} µs budget"
        ));
    }
    if reasons.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail(reasons)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A series of `1..=100`, so every percentile is known exactly.
    fn hundreds() -> Vec<f64> {
        (1..=100).map(|n| n as f64).collect()
    }

    #[test]
    fn percentiles_of_a_known_series() {
        let series = hundreds();
        assert_eq!(percentile(&series, 50.0), Some(50.0));
        assert_eq!(percentile(&series, 90.0), Some(90.0));
        assert_eq!(percentile(&series, 99.0), Some(99.0));
        assert_eq!(percentile(&series, 0.0), Some(1.0));
        assert_eq!(percentile(&series, 100.0), Some(100.0));
    }

    #[test]
    fn percentiles_do_not_depend_on_the_input_order() {
        let mut series = hundreds();
        series.reverse();
        assert_eq!(percentile(&series, 50.0), Some(50.0));
        assert_eq!(percentile(&series, 99.0), Some(99.0));
    }

    #[test]
    fn an_empty_series_has_no_percentile() {
        assert_eq!(percentile(&[], 50.0), None);
        assert!(summarize(&[]).is_none());
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let series = [42.0];
        assert_eq!(percentile(&series, 50.0), Some(42.0));
        assert_eq!(percentile(&series, 99.0), Some(42.0));
    }

    #[test]
    fn nearest_rank_never_invents_a_value() {
        // Three samples: p50 is the second, p99 is the third, and neither is interpolated.
        let series = [10.0, 20.0, 30.0];
        assert_eq!(percentile(&series, 50.0), Some(20.0));
        assert_eq!(percentile(&series, 99.0), Some(30.0));
        assert_eq!(percentile(&series, 90.0), Some(30.0));
    }

    #[test]
    fn a_summary_reports_the_extremes_and_the_median() {
        let summary = summarize(&hundreds()).unwrap();
        assert_eq!(summary.count, 100);
        assert_eq!(summary.min, 1.0);
        assert_eq!(summary.max, 100.0);
        assert_eq!(summary.p50, 50.0);
        assert_eq!(summary.p90, 90.0);
        assert_eq!(summary.p99, 99.0);
        assert!((summary.mean - 50.5).abs() < 1e-9);
    }

    #[test]
    fn a_measurement_under_budget_passes() {
        let verdict = judge(149.9, 499.9, 150.0, 500.0);
        assert!(verdict.is_pass());
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn a_measurement_over_budget_fails() {
        let verdict = judge(150.1, 100.0, 150.0, 500.0);
        assert!(!verdict.is_pass());
        let Verdict::Fail(reasons) = verdict else {
            panic!("expected a breach");
        };
        assert_eq!(reasons.len(), 1);
        assert!(reasons[0].contains("p50"));
    }

    #[test]
    fn a_measurement_exactly_at_the_budget_fails() {
        // The budget is "under 150 µs", so exactly 150 µs is a breach.
        assert!(!judge(150.0, 10.0, 150.0, 500.0).is_pass());
        assert!(!judge(10.0, 500.0, 150.0, 500.0).is_pass());
    }

    #[test]
    fn both_budgets_can_breach_at_once() {
        let verdict = judge(200.0, 600.0, 150.0, 500.0);
        let Verdict::Fail(reasons) = verdict else {
            panic!("expected a breach");
        };
        assert_eq!(reasons.len(), 2);
    }
}
