//! Counting declared quota windows, generically.
//!
//! # What this is for
//!
//! A provider's allowance is often stated in a shape no upstream reports: "500 requests per
//! minute", "2,000,000 tokens a day", "the weekly pool resets on Monday". Those numbers are facts
//! about a *plan*, not about any single response, so the operator declares them as
//! [`crate::model::QuotaWindow`]s — and until now they were a label: the panel printed them beside
//! whatever the upstream said, and routing ignored them.
//!
//! This module is the part of enforcement that knows nothing about HTTP: given the declared windows
//! and a log of what was actually spent, it answers *is a window full, and when does it come back*.
//! The relay feeds it events and cools an account down on the answer; the conductor can serve the
//! same numbers for display. One implementation, both readers.
//!
//! # Nothing here is provider-specific
//!
//! Every rule is derived from the window the operator wrote: its length, its kind, and which
//! dimensions it limits. No provider id, dialect or model appears in this file, so a plan this
//! router has never heard of is measured exactly as one it ships a reader for.
//!
//! # What is counted, and what is deliberately not
//!
//! * **Requests** are counted when the relay dials the account, not when the answer arrives. A
//!   request the upstream refuses still spent a request against a plan's rate, and counting only
//!   the successful ones would let a malfunctioning account be dialled without limit.
//! * **Tokens** are counted from what the upstream reported, and a response that reported nothing
//!   contributes nothing. This *undercounts* a stream whose usage frame never arrived, and that is
//!   the honest direction: the alternative is inventing token counts, and a budget decided from
//!   invented numbers would take a working account out of rotation.
//! * A **`reset_only`** window is a reset clock, not a limit. It never makes an account unusable,
//!   because the operator who wrote it said so by leaving every limit at zero.
//! * A **`provider_declared`** window has no local basis for a verdict: the upstream's own
//!   `retry-after` is what decides, and the relay's cooldown path already handles that. This module
//!   reports it as unmeasured rather than guessing a schedule.
//!
//! # Rolling windows recover gradually, and the reset instant says so
//!
//! A rolling window is not a bucket that empties: it is a moving interval, and a full one becomes
//! usable at the instant the *oldest spend that must age out* does. For a 60-second window of 500
//! requests that has seen 512, the account is usable again 60 seconds after its 13th-oldest request,
//! not 60 seconds after the most recent one. Computing it any other way would park an account for
//! longer than its plan requires, which is the same bug as over-spending it, seen from the other
//! side.
//!
//! A fixed window is the bucket, so it recovers at the boundary and that is what [`WindowSpend`]
//! reports.

use crate::dialect::WindowKind;
use crate::model::QuotaWindow;

/// One thing an account spent, recorded as it happens.
///
/// The token fields are optional because the counts are unknown until the upstream reports them.
/// An event with no token counts is a dialled request whose usage never arrived — a real request
/// against a request budget, and an unknown one against a token budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SpendEvent {
    /// Millisecond timestamp at which the request was dialled.
    pub at_ms: u64,
    /// Prompt tokens the upstream reported, when it reported any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Completion tokens the upstream reported, when it reported any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}

impl SpendEvent {
    /// A request dialled at `at_ms`, with no reported tokens yet.
    pub const fn request(at_ms: u64) -> Self {
        Self {
            at_ms,
            input_tokens: None,
            output_tokens: None,
        }
    }

    /// The same event with the upstream's token counts attached.
    pub const fn with_tokens(mut self, input: Option<u64>, output: Option<u64>) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    /// Total tokens this event is known to have cost. Unknown counts cost nothing, by the rule in
    /// the module docs.
    pub fn tokens(&self) -> u64 {
        self.input_tokens.unwrap_or(0) + self.output_tokens.unwrap_or(0)
    }
}

/// What one declared window looks like right now.
///
/// Reported for every window that constrains anything, spent or not, because "3 of 500 used" is the
/// number an operator wants before the account stops working — a counter that only appears once it
/// is too late to act on is a counter nobody can act on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WindowSpend {
    /// The operator's name for the window, or a generated one when they wrote none.
    pub name: String,
    /// The window's length in seconds, as declared.
    pub seconds: i64,
    /// The window's kind, as declared.
    pub kind: WindowKind,
    /// Requests counted in the window.
    pub requests: u64,
    /// The declared request limit. Zero means unlimited.
    pub limit_requests: u64,
    /// Total tokens counted in the window.
    pub tokens: u64,
    /// The declared total-token limit. Zero means unlimited.
    pub limit_tokens: u64,
    /// Input tokens counted in the window.
    pub input_tokens: u64,
    /// The declared input-token limit. Zero means unlimited.
    pub limit_input_tokens: u64,
    /// Output tokens counted in the window.
    pub output_tokens: u64,
    /// The declared output-token limit. Zero means unlimited.
    pub limit_output_tokens: u64,
    /// Whether the window is full, so the account must not be used until it recovers.
    pub spent: bool,
    /// When the window becomes usable again, when that is knowable. `None` means the window is not
    /// currently spent, or that it cannot be measured locally.
    pub reset_at_ms: Option<u64>,
    /// Whether the window was measured at all. A `provider_declared` window, or one with a
    /// non-positive length, is reported with this `false`: absence of measurement is not a verdict
    /// of "plenty left".
    pub measured: bool,
}

impl WindowSpend {
    /// The fraction of the window still available, when a limit is known and quantified.
    ///
    /// `None` when nothing is limited, when nothing has been measured, or when the window is
    /// unmeasured — the panel renders `N/A` for those rather than a bar that implies a reading.
    pub fn remaining_fraction(&self) -> Option<f64> {
        let fractions: Vec<f64> = [
            (self.requests, self.limit_requests),
            (self.tokens, self.limit_tokens),
            (self.input_tokens, self.limit_input_tokens),
            (self.output_tokens, self.limit_output_tokens),
        ]
        .into_iter()
        .filter(|(_, limit)| *limit > 0)
        .map(|(used, limit)| {
            let remaining = limit.saturating_sub(used) as f64 / limit as f64;
            remaining.clamp(0.0, 1.0)
        })
        .collect();
        if fractions.is_empty() || !self.measured {
            return None;
        }
        // The most constrained dimension decides: a window with 90% of its requests left and 5% of
        // its tokens left is a window that is 5% left.
        fractions.into_iter().fold(1.0, f64::min).into()
    }
}

/// Whether every declared dimension of a window is full.
///
/// A window with more than one limit is spent when **any** of them is reached: they are separate
/// allowances granted together, not alternatives, so the first one to run out is the one that
/// stops work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exhaustion {
    /// The declared window that is full.
    pub window_index: usize,
    /// When that window becomes usable again.
    pub reset_at_ms: u64,
}

/// Measure every window that constrains anything, at `now_ms`.
///
/// The returned vector is in declaration order so a reader can line it up with the authored
/// configuration without a second lookup.
pub fn measure(windows: &[QuotaWindow], events: &[SpendEvent], now_ms: u64) -> Vec<WindowSpend> {
    windows
        .iter()
        .filter(|window| window.constrains())
        .map(|window| measure_one(window, events, now_ms))
        .collect()
}

/// Whether any declared window is full, and when the last one of them recovers.
///
/// The *latest* reset among the full windows is what matters: parking an account until the first
/// window recovers would let the request through while a second window is still full, which is
/// exactly the over-spend the declaration exists to prevent.
pub fn exhaustion(
    windows: &[QuotaWindow],
    events: &[SpendEvent],
    now_ms: u64,
) -> Option<Exhaustion> {
    let mut worst: Option<Exhaustion> = None;
    for (window_index, window) in windows.iter().enumerate() {
        if !window.constrains() {
            continue;
        }
        let measured = measure_one(window, events, now_ms);
        if !measured.spent {
            continue;
        }
        let Some(reset_at_ms) = measured.reset_at_ms else {
            continue;
        };
        worst = match worst {
            Some(current) if current.reset_at_ms >= reset_at_ms => Some(current),
            _ => Some(Exhaustion {
                window_index,
                reset_at_ms,
            }),
        };
    }
    worst
}

/// Measure one window, whatever it declares.
fn measure_one(window: &QuotaWindow, events: &[SpendEvent], now_ms: u64) -> WindowSpend {
    let mut spend = WindowSpend {
        name: window_name(window),
        seconds: window.seconds,
        kind: window.kind,
        requests: 0,
        limit_requests: window.limit_requests,
        tokens: 0,
        limit_tokens: window.limit_tokens,
        input_tokens: 0,
        limit_input_tokens: window.limit_input_tokens,
        output_tokens: 0,
        limit_output_tokens: window.limit_output_tokens,
        spent: false,
        reset_at_ms: None,
        measured: false,
    };

    // A window can only be measured from local events when it has a length and a kind whose
    // boundaries are knowable here. `provider_declared` boundaries are learned from the upstream's
    // own reset headers, which is the cooldown path's business, not this module's.
    let span_ms = match u64::try_from(window.seconds) {
        Ok(seconds) if seconds > 0 => seconds.saturating_mul(1000),
        _ => return spend,
    };
    if window.kind == WindowKind::ProviderDeclared {
        return spend;
    }

    let (start_ms, boundary_end_ms) = match window.kind {
        WindowKind::Rolling => (now_ms.saturating_sub(span_ms), None),
        // Fixed: the interval that contains `now_ms`, aligned to the epoch. Alignment to the epoch
        // is what "hourly" means for every provider this router has met; a plan that resets at a
        // different wall clock is expressed as a rolling window, which does not need one.
        WindowKind::Fixed => {
            let start = (now_ms / span_ms).saturating_mul(span_ms);
            (start, Some(start.saturating_add(span_ms)))
        }
        WindowKind::ProviderDeclared => return spend,
    };

    let in_window: Vec<&SpendEvent> = events
        .iter()
        .filter(|event| event.at_ms >= start_ms && event.at_ms <= now_ms)
        .collect();
    spend.measured = true;
    spend.requests = in_window.len() as u64;
    spend.input_tokens = in_window.iter().filter_map(|e| e.input_tokens).sum();
    spend.output_tokens = in_window.iter().filter_map(|e| e.output_tokens).sum();
    spend.tokens = in_window.iter().map(|e| e.tokens()).sum();

    // Each limited dimension is checked against the same events; the window is spent on the first
    // one that is reached, and recovery is the latest instant any of them frees up.
    let mut resets: Vec<u64> = Vec::new();
    if window.limit_requests > 0 && spend.requests >= window.limit_requests {
        resets.extend(recovery(
            &in_window,
            window.limit_requests,
            span_ms,
            boundary_end_ms,
            |_| 1,
        ));
    }
    if window.limit_tokens > 0 && spend.tokens >= window.limit_tokens {
        resets.extend(recovery(
            &in_window,
            window.limit_tokens,
            span_ms,
            boundary_end_ms,
            SpendEvent::tokens,
        ));
    }
    if window.limit_input_tokens > 0 && spend.input_tokens >= window.limit_input_tokens {
        resets.extend(recovery(
            &in_window,
            window.limit_input_tokens,
            span_ms,
            boundary_end_ms,
            |event| event.input_tokens.unwrap_or(0),
        ));
    }
    if window.limit_output_tokens > 0 && spend.output_tokens >= window.limit_output_tokens {
        resets.extend(recovery(
            &in_window,
            window.limit_output_tokens,
            span_ms,
            boundary_end_ms,
            |event| event.output_tokens.unwrap_or(0),
        ));
    }

    if let Some(reset_at_ms) = resets.into_iter().max() {
        spend.spent = true;
        spend.reset_at_ms = Some(reset_at_ms);
    }
    spend
}

/// When a full window frees up, given the events that filled it.
///
/// A fixed window frees at its boundary, always. A rolling window frees when enough of the oldest
/// spend has aged out: for a count, when the oldest `used - limit + 1` requests have expired; for a
/// sum, when the oldest events whose total exceeds the overrun have. The result is the instant the
/// *last of those* expires, which is the earliest moment the window is under its limit again.
fn recovery(
    in_window: &[&SpendEvent],
    limit: u64,
    span_ms: u64,
    boundary_end_ms: Option<u64>,
    value_of: impl Fn(&SpendEvent) -> u64,
) -> Option<u64> {
    if let Some(end) = boundary_end_ms {
        return Some(end);
    }
    let total: u64 = in_window.iter().map(|event| value_of(event)).sum();
    let mut remaining = total;
    let mut dropped = 0usize;
    while remaining >= limit && dropped < in_window.len() {
        remaining -= value_of(in_window[dropped]);
        dropped += 1;
    }
    if remaining >= limit || dropped == 0 {
        return None;
    }
    // The event at `dropped - 1` is the last that had to age out; the window is under its limit the
    // moment it expires.
    in_window
        .get(dropped - 1)
        .map(|event| event.at_ms.saturating_add(span_ms))
}

/// The name to report a window under, generated when the operator wrote none.
///
/// A generated name is a duration, never a plan label: calling an unlabelled 86,400-second window
/// "daily" would be this module asserting something the operator did not.
fn window_name(window: &QuotaWindow) -> String {
    match window.name.as_deref() {
        Some(name) if !name.trim().is_empty() => name.trim().to_owned(),
        _ => format_window_seconds(window.seconds),
    }
}

/// A duration as a short human string: `60` → `1m`, `86400` → `1d`.
fn format_window_seconds(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds == 0 {
        return "window".to_owned();
    }
    if seconds % 86_400 == 0 {
        format!("{}d", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{}h", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rolling(seconds: i64, limit_requests: u64) -> QuotaWindow {
        QuotaWindow::requests_per(seconds, limit_requests)
    }

    #[test]
    fn a_rolling_request_window_fills_at_its_limit_and_not_before() {
        let window = rolling(60, 3);
        let events: Vec<SpendEvent> = (0..2).map(|i| SpendEvent::request(10_000 + i)).collect();
        let spend = measure(std::slice::from_ref(&window), &events, 10_500);
        assert_eq!(spend.len(), 1);
        assert_eq!(spend[0].requests, 2);
        assert!(
            !spend[0].spent,
            "two of three requests is not a full window"
        );
        assert_eq!(spend[0].remaining_fraction(), Some(1.0 / 3.0));
        assert!(exhaustion(&[window], &events, 10_500).is_none());
    }

    #[test]
    fn a_rolling_window_recovers_when_the_oldest_required_request_ages_out() {
        // Five requests in a 10-second window of limit 3: the window stays full until three of them
        // have expired (five minus a limit of three, plus one), which happens when the third-oldest
        // (t=3000) ages out at t=13000.
        let window = rolling(10, 3);
        let events: Vec<SpendEvent> = [1_000, 2_000, 3_000, 4_000, 5_000]
            .into_iter()
            .map(SpendEvent::request)
            .collect();
        let exhausted = exhaustion(std::slice::from_ref(&window), &events, 5_500).expect("full");
        assert_eq!(exhausted.window_index, 0);
        assert_eq!(
            exhausted.reset_at_ms, 13_000,
            "the window frees when the third-oldest request expires, not the newest"
        );
        // The oldest two have aged out by t=12_500, leaving three in the window: still full, and it
        // frees as soon as the request at t=3000 expires.
        let still = exhaustion(&[window], &events, 12_500).expect("still full");
        assert_eq!(still.reset_at_ms, 13_000);
    }

    #[test]
    fn a_fixed_window_recovers_at_its_boundary() {
        let window = QuotaWindow {
            kind: WindowKind::Fixed,
            ..rolling(60, 2)
        };
        let events = vec![SpendEvent::request(10_000), SpendEvent::request(20_000)];
        let exhausted = exhaustion(&[window], &events, 30_000).expect("full");
        assert_eq!(
            exhausted.reset_at_ms, 60_000,
            "a fixed window is a bucket: it empties at the boundary, not gradually"
        );
    }

    #[test]
    fn token_limits_are_counted_from_what_the_upstream_reported() {
        let window = QuotaWindow {
            limit_tokens: 100,
            ..rolling(3_600, 0)
        };
        let events = vec![
            SpendEvent::request(1_000).with_tokens(Some(60), Some(20)),
            // A response that reported nothing costs nothing against the budget: unknown is not
            // zero, and it is certainly not a guess.
            SpendEvent::request(2_000),
        ];
        let spend = measure(std::slice::from_ref(&window), &events, 3_000);
        assert_eq!(spend[0].tokens, 80);
        assert!(!spend[0].spent);
        let exhausted = exhaustion(
            &[window],
            &[
                SpendEvent::request(1_000).with_tokens(Some(60), Some(20)),
                SpendEvent::request(2_000).with_tokens(Some(30), None),
            ],
            3_000,
        )
        .expect("full at 110 of 100");
        assert_eq!(
            exhausted.reset_at_ms, 3_601_000,
            "the 60-token event is the last that has to age out"
        );
    }

    #[test]
    fn input_and_output_limits_are_separate_allowances() {
        let window = QuotaWindow {
            limit_input_tokens: 100,
            limit_output_tokens: 10,
            ..rolling(3_600, 0)
        };
        // Input is well under, output is over: the window is full on the dimension that ran out.
        let events = vec![SpendEvent::request(1_000).with_tokens(Some(50), Some(12))];
        assert!(measure(std::slice::from_ref(&window), &events, 2_000)[0].spent);
        // The reverse: output under, input over.
        let events = vec![SpendEvent::request(1_000).with_tokens(Some(250), Some(2))];
        assert!(measure(&[window], &events, 2_000)[0].spent);
    }

    #[test]
    fn a_reset_only_window_never_stops_an_account() {
        let window = QuotaWindow {
            reset_only: true,
            ..rolling(3_600, 0)
        };
        let events: Vec<SpendEvent> = (0..1_000).map(SpendEvent::request).collect();
        assert!(
            measure(std::slice::from_ref(&window), &events, 10_000).is_empty(),
            "a window that constrains nothing is not measured at all"
        );
        assert!(exhaustion(&[window], &events, 10_000).is_none());
    }

    #[test]
    fn a_provider_declared_window_is_unmeasured_rather_than_assumed_available() {
        let window = QuotaWindow {
            kind: WindowKind::ProviderDeclared,
            ..rolling(3_600, 5)
        };
        let events: Vec<SpendEvent> = (0..50).map(SpendEvent::request).collect();
        let spend = measure(std::slice::from_ref(&window), &events, 10_000);
        assert!(!spend[0].measured);
        assert_eq!(spend[0].remaining_fraction(), None);
        assert!(
            exhaustion(&[window], &events, 10_000).is_none(),
            "the upstream's own retry-after decides this window, not a local guess"
        );
    }

    #[test]
    fn the_latest_reset_among_full_windows_is_the_one_that_holds_the_account() {
        let minute = rolling(60, 1);
        let hour = rolling(3_600, 1);
        let events = vec![SpendEvent::request(1_000)];
        let exhausted = exhaustion(&[minute, hour], &events, 2_000).expect("both full");
        assert_eq!(
            exhausted.window_index, 1,
            "the hourly window is the later one"
        );
        assert_eq!(exhausted.reset_at_ms, 3_601_000);
    }

    #[test]
    fn a_window_with_no_declared_name_is_reported_as_its_length() {
        assert_eq!(window_name(&rolling(60, 1)), "1m");
        assert_eq!(window_name(&rolling(3_600, 1)), "1h");
        assert_eq!(window_name(&rolling(86_400, 1)), "1d");
        assert_eq!(window_name(&rolling(90, 1)), "90s");
        assert_eq!(
            window_name(&QuotaWindow {
                name: Some("weekly".into()),
                ..rolling(604_800, 1)
            }),
            "weekly"
        );
    }

    #[test]
    fn events_outside_the_window_do_not_count() {
        let window = rolling(60, 2);
        let events = vec![
            SpendEvent::request(1_000),
            SpendEvent::request(100_000),
            SpendEvent::request(100_500),
        ];
        let spend = measure(&[window], &events, 101_000);
        assert_eq!(
            spend[0].requests, 2,
            "the request from long before the window is not part of it"
        );
    }
}
