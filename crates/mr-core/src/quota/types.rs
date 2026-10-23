//! The normalised quota shapes, and the rules that keep them honest.
//!
//! This is the Rust port of the `UsageQuota` value type and the `createQuotaFromUsage` /
//! `parseResetTime` normalisers in OmniRoute's `open-sse/services/usage/quota.ts` (MIT,
//! © 2026 diegosouzapw). The shape is OmniRoute's; the serialisation is Manarouter's, which is
//! snake_case everywhere — the existing control-plane endpoints (`GET /api/quota`, the declared
//! windows `panel/view-quota.js` reads from provider configuration) already speak snake_case, so
//! OmniRoute's camelCase names are deliberately *not* reproduced. Field names are the ones the plan
//! fixes: `used`, `total`, `remaining`, `remaining_percentage`, `reset_at`, `unlimited`,
//! `fraction_reported`, `source`, `display_name`, `details`.
//!
//! Three decisions worth stating:
//!
//! * **`0` is not always exhausted.** When an upstream omits the remaining fraction, the zero that
//!   lands in `remaining`/`remaining_percentage` is a *sentinel*, and `fraction_reported` is
//!   `false`. [`UsageQuota::remaining_fraction`] and [`UsageQuota::reported_percentage`] return
//!   `None` in that case, so a consumer that renders them cannot print "0%" for an upstream that
//!   never said so.
//! * **Counts are `f64`.** OmniRoute types them as `number` because they carry both integer token
//!   counts and fractional credit balances (USD cents); `f64` reproduces an exact integer for every
//!   count below 2^53 and keeps `12.34` a balance rather than a lie.
//! * **`reset_at` is epoch milliseconds, or `None`.** The panel parses either form
//!   (`format.js` accepts a number), a number survives a JSON round trip without re-parsing, and
//!   [`parse_reset_time`] never invents an instant: an unrecognised shape, an epoch-zero, or a date
//!   before 1970 all come back `None`.

use serde::{Deserialize, Serialize};

pub use crate::quota::window::WindowFamily;

/// A money or credit line item inside a quota, from OmniRoute's `details` array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaDetail {
    /// What the amount is, for example `credits` or `top-up`.
    pub name: String,
    /// The amount consumed. Fractional because credit balances are often USD.
    #[serde(default)]
    pub used: f64,
}

/// One account's allowance at one upstream, normalised.
///
/// Built through the constructors rather than by hand, so `fraction_reported` is always set
/// correctly: [`UsageQuota::from_usage`] when the upstream reported the remaining fraction,
/// [`UsageQuota::from_usage_unreported`] when it did not, and [`UsageQuota::unlimited`] for a plan
/// where no fraction applies at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageQuota {
    /// Amount consumed in the current window.
    #[serde(default)]
    pub used: f64,
    /// The window's limit. Zero means the upstream reported no limit (see `fraction_reported`).
    #[serde(default)]
    pub total: f64,
    /// Amount left. **A zero here is only meaningful when `fraction_reported` is true.**
    #[serde(default)]
    pub remaining: f64,
    /// Percentage left, `0.0..=100.0`. Same sentinel rule as `remaining`.
    #[serde(default)]
    pub remaining_percentage: f64,
    /// When the window resets, in epoch milliseconds (**not** seconds, **not** a string), or
    /// `None` when the upstream gave no usable instant. `None` is not "now" and not "never".
    #[serde(default)]
    pub reset_at: Option<i64>,
    /// True when the plan has no applicable limit; `used`/`total`/`remaining` carry no information.
    #[serde(default)]
    pub unlimited: bool,
    /// Whether the upstream actually reported the remaining fraction.
    ///
    /// `false` means `remaining` and `remaining_percentage` are the `0` sentinel, *not* a
    /// confirmed-exhausted state. A provider-specific signal may still prove the quota blocks
    /// routing; the flag only says what this payload knows. [`UsageQuota::fraction_reported`] is the
    /// method form, for callers that prefer a call to a field read.
    #[serde(default)]
    pub fraction_reported: bool,
    /// Which upstream call produced this row, for example `retrieveUserQuota` or
    /// `localUsageHistory`. Kept open as a string so a new fetcher needs no schema change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Operator-facing name for the plan or window, when the upstream gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Per-currency or per-bucket breakdown, when the upstream gave one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<QuotaDetail>,
}

impl UsageQuota {
    /// Clamp and derive the counts exactly as `createQuotaFromUsage` does.
    ///
    /// With a non-positive total there is no fraction to compute, so used/remaining are zeroed
    /// rather than divided by zero. Used is clamped into `0..=total`, so an upstream reporting
    /// `used > total` produces `remaining == 0` instead of a negative remainder.
    fn normalized(used: f64, total: f64) -> (f64, f64, f64, f64) {
        let total = if total.is_finite() && total > 0.0 {
            total
        } else {
            0.0
        };
        if total == 0.0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let used = if used.is_finite() {
            used.clamp(0.0, total)
        } else {
            0.0
        };
        let remaining = (total - used).max(0.0);
        // `remaining * 100 / total`, not `remaining / total * 100`: for the common `total = 100`
        // the second form divides and re-multiplies by 100, and binary floating point turns an
        // exact `57` into `56.99999999999999`. Multiplying first keeps whole percents whole.
        let percentage = (remaining * 100.0 / total).clamp(0.0, 100.0);
        (used, total, remaining, percentage)
    }

    /// A quota whose remaining fraction the upstream **did** report.
    pub fn from_counts(used: f64, total: f64, reset_at: Option<i64>) -> Self {
        let (used, total, remaining, remaining_percentage) = Self::normalized(used, total);
        Self {
            used,
            total,
            remaining,
            remaining_percentage,
            reset_at,
            unlimited: false,
            fraction_reported: true,
            source: None,
            display_name: None,
            details: Vec::new(),
        }
    }

    /// A quota whose remaining fraction the upstream **omitted**.
    ///
    /// `used` and `total` are what the upstream said; `remaining` and `remaining_percentage` are the
    /// `0` sentinel and `fraction_reported` is `false`, so no consumer can mistake the sentinel for
    /// exhaustion.
    pub fn from_counts_unreported(used: f64, total: f64, reset_at: Option<i64>) -> Self {
        let mut quota = Self::from_counts(used, total, reset_at);
        quota.fraction_reported = false;
        quota.remaining = 0.0;
        quota.remaining_percentage = 0.0;
        quota
    }

    /// [`UsageQuota::from_counts`] with the upstream's raw reset value, parsed by
    /// [`parse_reset_time`] (so an unparsable value becomes `None` rather than a guess).
    pub fn from_usage(used: f64, total: f64, reset: &serde_json::Value) -> Self {
        Self::from_counts(used, total, parse_reset_time(reset))
    }

    /// [`UsageQuota::from_counts_unreported`] with the upstream's raw reset value.
    pub fn from_usage_unreported(used: f64, total: f64, reset: &serde_json::Value) -> Self {
        Self::from_counts_unreported(used, total, parse_reset_time(reset))
    }

    /// A plan with no applicable limit: nothing was measured, and nothing should be rendered as a
    /// percentage. `fraction_reported` stays `false` for that reason.
    pub fn unlimited() -> Self {
        Self {
            used: 0.0,
            total: 0.0,
            remaining: 0.0,
            remaining_percentage: 0.0,
            reset_at: None,
            unlimited: true,
            fraction_reported: false,
            source: None,
            display_name: None,
            details: Vec::new(),
        }
    }

    /// Record which upstream call produced this row.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Record the operator-facing name of the plan or window.
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    /// Attach a breakdown line item.
    pub fn with_detail(mut self, detail: QuotaDetail) -> Self {
        self.details.push(detail);
        self
    }

    /// Whether the upstream reported the remaining fraction — the method form of the field, so a
    /// caller can read `quota.fraction_reported` or `quota.fraction_reported()`.
    pub fn fraction_reported(&self) -> bool {
        self.fraction_reported
    }

    /// The remaining fraction in `0.0..=1.0`, or `None` when no fraction was reported (or the plan
    /// is unlimited). This is the accessor a consumer should render from.
    pub fn remaining_fraction(&self) -> Option<f64> {
        if self.unlimited || !self.fraction_reported || self.total <= 0.0 {
            return None;
        }
        Some((self.remaining / self.total).clamp(0.0, 1.0))
    }

    /// The reported remaining percentage, or `None` when nothing was reported. Contrast
    /// [`UsageQuota::remaining_fraction`]: a genuine `Some(0.0)` here is an exhausted window.
    /// A zero `total` is not a percentage either — there is no limit to be a fraction of.
    pub fn reported_percentage(&self) -> Option<f64> {
        if self.fraction_reported && !self.unlimited && self.total > 0.0 {
            Some(self.remaining_percentage)
        } else {
            None
        }
    }

    /// Whether the window is provably exhausted.
    ///
    /// Requires a *reported* fraction and a real limit: an upstream that omitted `remaining` yields
    /// `false`, and so does one that reported no limit at all, because a `0` sentinel is not
    /// evidence of exhaustion.
    pub fn is_exhausted(&self) -> bool {
        !self.unlimited && self.fraction_reported && self.total > 0.0 && self.remaining <= 0.0
    }
}

/// One window of an account's allowance, with the family its real length implies.
///
/// `family` comes from a *duration* ([`WindowFamily::from_seconds`]), never from the upstream's key
/// name; `key` keeps that name for routing. Build one from a [`UsageQuota`] with
/// [`QuotaWindow::from_quota`] when an upstream reports per-window rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// The duration band this window belongs to.
    pub family: WindowFamily,
    /// The upstream's own key (`session`, `weekly_all`, …), kept for routing and diagnostics.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Operator-facing name for the window, when the upstream gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Amount consumed in this window.
    #[serde(default)]
    pub used: f64,
    /// The window's limit. Zero means no limit was reported.
    #[serde(default)]
    pub total: f64,
    /// Amount left; the `0` sentinel rule of [`UsageQuota`] applies here too.
    #[serde(default)]
    pub remaining: f64,
    /// Percentage left, `0.0..=100.0`.
    #[serde(default)]
    pub remaining_percentage: f64,
    /// When this window resets, in epoch milliseconds, or `None`.
    #[serde(default)]
    pub reset_at: Option<i64>,
    /// True when the window has no applicable limit.
    #[serde(default)]
    pub unlimited: bool,
    /// Whether the upstream reported the remaining fraction for this window.
    #[serde(default)]
    pub fraction_reported: bool,
}

impl QuotaWindow {
    /// A window from its family and counts, with the same clamping as [`UsageQuota`].
    pub fn new(family: WindowFamily, used: f64, total: f64, reset_at: Option<i64>) -> Self {
        let quota = UsageQuota::from_counts(used, total, reset_at);
        Self::from_quota(family, &quota)
    }

    /// Reuse a normalised [`UsageQuota`] as a window of a known family.
    pub fn from_quota(family: WindowFamily, quota: &UsageQuota) -> Self {
        Self {
            family,
            key: None,
            display_name: quota.display_name.clone(),
            used: quota.used,
            total: quota.total,
            remaining: quota.remaining,
            remaining_percentage: quota.remaining_percentage,
            reset_at: quota.reset_at,
            unlimited: quota.unlimited,
            fraction_reported: quota.fraction_reported,
        }
    }

    /// Record the upstream's own name for this window.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }

    /// The label an operator reads: `5h`, `1w`, `30d`, or `other(<secs>)`.
    pub fn label(&self) -> String {
        self.family.label()
    }

    /// The remaining fraction, or `None` when none was reported (see
    /// [`UsageQuota::remaining_fraction`]).
    pub fn remaining_fraction(&self) -> Option<f64> {
        if self.unlimited || !self.fraction_reported || self.total <= 0.0 {
            return None;
        }
        Some((self.remaining / self.total).clamp(0.0, 1.0))
    }

    /// Whether this window is provably exhausted (a reported fraction of zero, against a real
    /// limit).
    pub fn is_exhausted(&self) -> bool {
        !self.unlimited && self.fraction_reported && self.total > 0.0 && self.remaining <= 0.0
    }
}

/// Parse an upstream reset value into epoch **milliseconds**, or `None`.
///
/// The shapes upstreams actually use:
///
/// | input | interpretation |
/// |---|---|
/// | `1700000000` / `"1700000000"` | Unix seconds (any magnitude below `1e12`) |
/// | `1700000000000` / `"1700000000000"` | Unix milliseconds |
/// | `"2026-01-02T03:04:05Z"` | RFC 3339, with `Z`, `±HH:MM` or `±HHMM`, fractional seconds allowed |
/// | `"Fri, 02 Jan 2026 03:04:05 GMT"` | HTTP date (also RFC 850 and asctime) |
///
/// Anything else — `null`, a boolean, an object, an unparsable string, an epoch-zero or a pre-1970
/// instant — returns `None`. Epoch zero means "no scheduled reset" in OmniRoute, and the same rule
/// is kept here rather than rendering 1970-01-01 as a real reset.
pub fn parse_reset_time(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(number) => number.as_f64().and_then(epoch_number_millis),
        serde_json::Value::String(text) => parse_reset_time_str(text),
        _ => None,
    }
}

/// [`parse_reset_time`] for a string that is already in hand (a header value, a config field).
pub fn parse_reset_time_str(raw: &str) -> Option<i64> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(millis) = numeric_millis(text) {
        return Some(millis);
    }
    if let Some(millis) = rfc3339_millis(text) {
        return Some(millis);
    }
    httpdate::parse_http_date(text)
        .ok()
        .and_then(system_time_millis)
}

/// Seconds-or-milliseconds, exactly as `parseResetTime` decides it: below `1e12` is seconds.
///
/// A non-finite, zero, or negative value yields `None`.
fn epoch_number_millis(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let millis = if value.abs() < 1e12 {
        value * 1000.0
    } else {
        value
    };
    // A saturating cast: a wild value must not wrap into a plausible date.
    let millis = millis as i64;
    if millis <= 0 {
        None
    } else {
        Some(millis)
    }
}

/// A numeric string is a Unix timestamp too — `new Date("1700000000")` is an invalid date, which is
/// why OmniRoute special-cases this, and so do we.
fn numeric_millis(text: &str) -> Option<i64> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || digits.matches('.').count() > 1 {
        return None;
    }
    if !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    epoch_number_millis(value)
}

fn system_time_millis(time: std::time::SystemTime) -> Option<i64> {
    let since_epoch = time.duration_since(std::time::UNIX_EPOCH).ok()?;
    let millis = i64::try_from(since_epoch.as_millis()).ok()?;
    if millis <= 0 {
        None
    } else {
        Some(millis)
    }
}

/// RFC 3339 (and the space-separated spelling some upstreams emit) to epoch milliseconds.
fn rfc3339_millis(text: &str) -> Option<i64> {
    let (date, rest) = split_date_time(text)?;
    let (year, month, day) = parse_date(date)?;
    let (hour, minute, second, fraction_millis, offset_seconds) = parse_clock(rest)?;

    let seconds = days_from_civil(year, month, day)
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))?
        .checked_sub(offset_seconds)?;
    let millis = seconds.checked_mul(1000)?.checked_add(fraction_millis)?;
    if millis <= 0 {
        None
    } else {
        Some(millis)
    }
}

fn split_date_time(text: &str) -> Option<(&str, &str)> {
    let (date, rest) = text
        .split_once('T')
        .or_else(|| text.split_once('t'))
        .or_else(|| text.split_once(' '))?;
    if date.is_empty() || rest.is_empty() {
        return None;
    }
    Some((date, rest))
}

fn parse_date(date: &str) -> Option<(i64, u32, u32)> {
    let mut parts = date.split('-');
    let year = parse_uint(parts.next()?)?;
    let month = parse_uint(parts.next()?)?;
    let day = parse_uint(parts.next()?)?;
    if parts.next().is_some() || year > 9999 || !(1..=12).contains(&month) {
        return None;
    }
    if day < 1 || day > days_in_month(year, month) {
        return None;
    }
    Some((i64::from(year), month, day))
}

/// `(hour, minute, second, fraction_millis, offset_seconds)`.
///
/// Seconds may be `60`: a leap second is added as a real second rather than rejected, which is what
/// the arithmetic below actually means.
fn parse_clock(rest: &str) -> Option<(u32, u32, u32, i64, i64)> {
    let (time, offset_seconds) = split_offset(rest)?;
    let (clock, fraction) = match time.split_once('.') {
        Some((clock, fraction)) => (clock, Some(fraction)),
        None => (time, None),
    };

    let mut parts = clock.split(':');
    let hour = parse_uint(parts.next()?)?;
    let minute = parse_uint(parts.next()?)?;
    let second = parse_uint(parts.next()?)?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }

    let fraction_millis = match fraction {
        Some(digits) => fraction_millis(digits)?,
        None => 0,
    };
    Some((hour, minute, second, fraction_millis, offset_seconds))
}

/// Split a trailing `Z` / `±HH:MM` / `±HHMM` off the clock, returning the clock and the offset.
fn split_offset(rest: &str) -> Option<(&str, i64)> {
    if let Some(time) = rest.strip_suffix('Z').or_else(|| rest.strip_suffix('z')) {
        return if time.is_empty() {
            None
        } else {
            Some((time, 0))
        };
    }

    // The date was consumed by the caller, so neither sign can occur inside the clock itself.
    let index = rest.rfind('+').or_else(|| rest.rfind('-'))?;
    let (time, offset) = rest.split_at(index);
    if time.is_empty() {
        return None;
    }
    let (sign, digits) = offset.split_at(1);
    let (hours, minutes) = match digits.split_once(':') {
        Some(pair) => pair,
        None if digits.len() == 4 => digits.split_at(2),
        None => return None,
    };
    let hours = parse_uint(hours)?;
    let minutes = parse_uint(minutes)?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    let magnitude = i64::from(hours) * 3600 + i64::from(minutes) * 60;
    let offset_seconds = if sign == "-" { -magnitude } else { magnitude };
    Some((time, offset_seconds))
}

/// The first three fractional digits, right-padded, as milliseconds.
fn fraction_millis(digits: &str) -> Option<i64> {
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut millis = 0i64;
    let mut count = 0;
    for byte in digits.bytes().take(3) {
        millis = millis * 10 + i64::from(byte - b'0');
        count += 1;
    }
    for _ in count..3 {
        millis *= 10;
    }
    Some(millis)
}

fn parse_uint(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date (Howard Hinnant's algorithm).
///
/// Hand-rolled because the crate's date library is not otherwise needed on the quota path, and a
/// dependency there would add a parsing surface for four shapes we already know exactly.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 2026-01-02T03:04:05Z, from `date -u -d 2026-01-02T03:04:05Z +%s`.
    const KNOWN_SECONDS: i64 = 1_767_323_045;
    const KNOWN_MILLIS: i64 = KNOWN_SECONDS * 1000;

    #[test]
    fn epoch_seconds_and_milliseconds_agree() {
        assert_eq!(parse_reset_time(&json!(KNOWN_SECONDS)), Some(KNOWN_MILLIS));
        assert_eq!(parse_reset_time(&json!(KNOWN_MILLIS)), Some(KNOWN_MILLIS));
        // Numeric strings are timestamps too: `new Date("1767323045")` would be an invalid date.
        assert_eq!(
            parse_reset_time(&json!(KNOWN_SECONDS.to_string())),
            Some(KNOWN_MILLIS)
        );
        assert_eq!(
            parse_reset_time(&json!(KNOWN_MILLIS.to_string())),
            Some(KNOWN_MILLIS)
        );
    }

    #[test]
    fn rfc3339_offsets_and_fractions_are_honoured() {
        assert_eq!(
            parse_reset_time(&json!("2026-01-02T03:04:05Z")),
            Some(KNOWN_MILLIS)
        );
        // Offset clocks name the same instant.
        assert_eq!(
            parse_reset_time(&json!("2026-01-02T05:04:05+02:00")),
            Some(KNOWN_MILLIS)
        );
        assert_eq!(
            parse_reset_time(&json!("2026-01-02T05:04:05+0200")),
            Some(KNOWN_MILLIS)
        );
        assert_eq!(
            parse_reset_time(&json!("2026-01-01T22:04:05-05:00")),
            Some(KNOWN_MILLIS)
        );
        // Fractional seconds, and the space-separated spelling some upstreams emit.
        assert_eq!(
            parse_reset_time(&json!("2026-01-02T03:04:05.250Z")),
            Some(KNOWN_MILLIS + 250)
        );
        assert_eq!(
            parse_reset_time(&json!("2026-01-02 03:04:05Z")),
            Some(KNOWN_MILLIS)
        );
        assert_eq!(
            parse_reset_time(&json!("2026-01-02t03:04:05z")),
            Some(KNOWN_MILLIS)
        );
        // A leap second is added as a real second, not rejected: 03:04:60 is 03:05:00, which is
        // 55 seconds after 03:04:05.
        assert_eq!(
            parse_reset_time(&json!("2026-01-02T03:04:60Z")),
            Some(KNOWN_MILLIS + 55_000)
        );
    }

    #[test]
    fn httpdate_is_parsed() {
        // 2026-01-02 is a Friday.
        assert_eq!(
            parse_reset_time(&json!("Fri, 02 Jan 2026 03:04:05 GMT")),
            Some(KNOWN_MILLIS)
        );
        assert_eq!(
            parse_reset_time_str("Fri, 02 Jan 2026 03:04:05 GMT"),
            Some(KNOWN_MILLIS)
        );
    }

    #[test]
    fn unparsable_and_sentinel_values_are_rejected_rather_than_guessed() {
        for value in [
            json!(null),
            json!(true),
            json!(false),
            json!({}),
            json!([]),
            json!(""),
            json!("   "),
            json!("not a date"),
            json!("2026-13-01T00:00:00Z"),      // month 13
            json!("2026-02-30T00:00:00Z"),      // day past February
            json!("2025-02-29T00:00:00Z"),      // 2025 is not a leap year
            json!("2026-01-02T24:00:00Z"),      // hour 24
            json!("2026-01-02T03:60:00Z"),      // minute 60
            json!("2026-01-02T03:04:05"),       // no offset
            json!("2026-01-02T03:04:05+25:00"), // offset out of range
            json!("2026-01-02"),                // a date is not an instant
            json!(0),                           // epoch zero means "no reset"
            json!("0"),
            json!(-1),
            json!(-1_700_000_000i64),
        ] {
            assert_eq!(parse_reset_time(&value), None, "expected None for {value}");
        }
        // A pre-1970 instant is not a scheduled reset either.
        assert_eq!(parse_reset_time(&json!("1969-12-31T23:59:59Z")), None);
        assert_eq!(parse_reset_time(&json!("1970-01-01T00:00:00Z")), None);
        // A plain year string is a (tiny) Unix timestamp, so "2026" reads as 2026 seconds after the
        // epoch, exactly as OmniRoute reads it. Spelled out here because it is surprising.
        assert_eq!(parse_reset_time(&json!("2026")), Some(2_026_000));
    }

    #[test]
    fn reported_fractions_are_renderable() {
        let quota = UsageQuota::from_usage(30.0, 100.0, &json!(KNOWN_SECONDS));
        assert!(quota.fraction_reported());
        assert!(quota.fraction_reported);
        assert_eq!(quota.used, 30.0);
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.remaining, 70.0);
        assert_eq!(quota.remaining_percentage, 70.0);
        assert_eq!(quota.reset_at, Some(KNOWN_MILLIS));
        assert!(!quota.unlimited);
        assert_eq!(quota.remaining_fraction(), Some(0.7));
        assert_eq!(quota.reported_percentage(), Some(70.0));
        assert!(!quota.is_exhausted());

        // Reported exhaustion is a real zero, and stays distinguishable from the sentinel below.
        let exhausted = UsageQuota::from_usage(100.0, 100.0, &json!(null));
        assert_eq!(exhausted.remaining, 0.0);
        assert_eq!(exhausted.reported_percentage(), Some(0.0));
        assert!(exhausted.is_exhausted());
        assert_eq!(exhausted.reset_at, None);
    }

    #[test]
    fn unreported_fractions_are_not_zero_percent() {
        let quota = UsageQuota::from_usage_unreported(30.0, 100.0, &json!(null));
        assert!(!quota.fraction_reported());
        assert_eq!(quota.used, 30.0);
        assert_eq!(quota.total, 100.0);
        // The sentinel, recorded as such.
        assert_eq!(quota.remaining, 0.0);
        assert_eq!(quota.remaining_percentage, 0.0);
        // A consumer rendering from these cannot print 0%.
        assert_eq!(quota.remaining_fraction(), None);
        assert_eq!(quota.reported_percentage(), None);
        assert!(!quota.is_exhausted());
    }

    #[test]
    fn unlimited_plans_have_no_fraction() {
        let quota = UsageQuota::unlimited().with_display_name("unlimited plan");
        assert!(quota.unlimited);
        assert!(!quota.fraction_reported());
        assert_eq!(quota.display_name.as_deref(), Some("unlimited plan"));
        assert_eq!(quota.remaining_fraction(), None);
        assert_eq!(quota.reported_percentage(), None);
        assert!(!quota.is_exhausted());
    }

    #[test]
    fn counts_are_clamped_like_create_quota_from_usage() {
        // used > total: remaining floors at zero rather than going negative.
        let over = UsageQuota::from_counts(150.0, 100.0, None);
        assert_eq!(over.used, 100.0);
        assert_eq!(over.remaining, 0.0);
        assert!(over.is_exhausted());
        // A non-positive total has no fraction to compute, and is not an exhausted window either:
        // there is no limit that could be exhausted.
        let none = UsageQuota::from_counts(10.0, 0.0, None);
        assert_eq!(none.used, 0.0);
        assert_eq!(none.total, 0.0);
        assert_eq!(none.remaining_percentage, 0.0);
        assert_eq!(none.remaining_fraction(), None);
        assert_eq!(none.reported_percentage(), None);
        assert!(!none.is_exhausted());
        // Fractional credit balances survive.
        let credits = UsageQuota::from_counts(12.34, 50.0, None);
        assert!((credits.remaining - 37.66).abs() < 1e-9);
        assert!(
            (credits.reported_percentage().expect("reported") - 75.32).abs() < 1e-9,
            "percentage was {:?}",
            credits.reported_percentage()
        );
    }

    #[test]
    fn quota_shape_serialises_with_the_names_the_panel_uses() {
        let quota = UsageQuota::from_usage(30.0, 100.0, &json!(KNOWN_SECONDS))
            .with_source("retrieveUserQuota")
            .with_display_name("Pro plan")
            .with_detail(QuotaDetail {
                name: "credits".to_owned(),
                used: 1.5,
            });
        let value = serde_json::to_value(&quota).expect("serialises");
        assert_eq!(value["used"], json!(30.0));
        assert_eq!(value["total"], json!(100.0));
        assert_eq!(value["remaining"], json!(70.0));
        assert_eq!(value["remaining_percentage"], json!(70.0));
        assert_eq!(value["reset_at"], json!(KNOWN_MILLIS));
        assert_eq!(value["unlimited"], json!(false));
        assert_eq!(value["fraction_reported"], json!(true));
        assert_eq!(value["source"], json!("retrieveUserQuota"));
        assert_eq!(value["display_name"], json!("Pro plan"));
        assert_eq!(value["details"][0]["name"], json!("credits"));
        // Round trip, including the sentinel flag.
        let back: UsageQuota = serde_json::from_value(value).expect("deserialises");
        assert_eq!(back, quota);
    }

    #[test]
    fn window_families_and_counts_serialise() {
        let quota = UsageQuota::from_usage(25.0, 100.0, &json!(null));
        let window = QuotaWindow::from_quota(WindowFamily::Weekly1w, &quota).with_key("weekly_all");
        assert_eq!(window.label(), "1w");
        assert_eq!(window.remaining_fraction(), Some(0.75));
        assert!(!window.is_exhausted());
        let value = serde_json::to_value(&window).expect("serialises");
        assert_eq!(value["family"], json!("1w"));
        assert_eq!(value["key"], json!("weekly_all"));
        assert_eq!(value["remaining_percentage"], json!(75.0));
        let back: QuotaWindow = serde_json::from_value(value).expect("deserialises");
        assert_eq!(back, window);

        let long = QuotaWindow::new(WindowFamily::Other { secs: 3_456_001 }, 1.0, 2.0, None);
        assert_eq!(long.label(), "other(3456001)");
        assert_eq!(long.remaining, 1.0);
    }
}
