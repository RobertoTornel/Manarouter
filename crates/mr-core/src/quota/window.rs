//! Duration-aware quota window families: the Rust port of OmniRoute's
//! `open-sse/services/quotaWindowLabel.ts` (MIT, © 2026 diegosouzapw).
//!
//! Two things this file is deliberately careful about:
//!
//! * **A window is a duration, not a name.** An upstream that reports a bucket called `session`
//!   may be describing five hours or thirty days. [`WindowFamily::from_seconds`] classifies the
//!   *length*, so a 30-day window is never labelled `5h`; the upstream's own key is kept for
//!   routing, not for display.
//! * **`Other` keeps the real number.** When a length fits none of the three familiar families, the
//!   exact seconds survive inside the variant ([`WindowFamily::Other`]) instead of being rounded
//!   into the nearest familiar name.
//!
//! The thresholds, and their inclusivity, are copied from the JavaScript source:
//!
//! | observed length | family | label |
//! |---|---|---|
//! | `0 ..= 6h` | [`WindowFamily::Session5h`] | `5h` |
//! | `6h+1s ..= 8d` | [`WindowFamily::Weekly1w`] | `1w` |
//! | `8d+1s ..= 40d` | [`WindowFamily::Monthly30d`] | `30d` |
//! | `> 40d` | [`WindowFamily::Other`] | `other(<secs>)` |

use serde::{Deserialize, Serialize};

/// `SESSION_MAX_SECONDS` in `quotaWindowLabel.ts`.
pub const SESSION_MAX_SECONDS: u64 = 6 * 3600;
/// `WEEKLY_MAX_SECONDS` in `quotaWindowLabel.ts`.
pub const WEEKLY_MAX_SECONDS: u64 = 8 * 24 * 3600;
/// `MONTHLY_MAX_SECONDS` in `quotaWindowLabel.ts`.
pub const MONTHLY_MAX_SECONDS: u64 = 40 * 24 * 3600;

/// The rolling family a quota window belongs to, inferred from its real length.
///
/// Serialised as the label an operator reads: `"5h"`, `"1w"`, `"30d"`, or
/// `{"other":{"secs":3456001}}` for a length outside the three familiar families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowFamily {
    /// Up to six hours. The name matches the five-hour windows most subscription plans use; the
    /// test is the inequality, not the name.
    #[serde(rename = "5h")]
    Session5h,
    /// More than six hours and up to eight days.
    #[serde(rename = "1w")]
    Weekly1w,
    /// More than eight days and up to forty days — wide enough for an upstream that reports
    /// `monthly` as 31 days.
    #[serde(rename = "30d")]
    Monthly30d,
    /// Anything longer, carrying the exact length so nothing is rounded away.
    #[serde(rename = "other")]
    Other {
        /// The observed window length in seconds.
        secs: u64,
    },
}

impl WindowFamily {
    /// Classify a window by its real length in seconds.
    ///
    /// Boundaries are inclusive on the upper end, exactly as `inferWindowFamilyLabel` has them:
    /// 6 h is still a session window, 8 d is still weekly, 40 d is still monthly. `seconds` must be
    /// a positive window length — `0` means "no window known", and a caller that has no length
    /// should not guess one (it would classify as [`WindowFamily::Session5h`], the shortest
    /// family).
    pub fn from_seconds(seconds: u64) -> Self {
        if seconds <= SESSION_MAX_SECONDS {
            Self::Session5h
        } else if seconds <= WEEKLY_MAX_SECONDS {
            Self::Weekly1w
        } else if seconds <= MONTHLY_MAX_SECONDS {
            Self::Monthly30d
        } else {
            Self::Other { secs: seconds }
        }
    }

    /// The label an operator reads: `5h`, `1w`, `30d`, or `other(<secs>)`.
    ///
    /// `Other` prints its exact length because there is no familiar name to print instead;
    /// rounding it to `30d` would misstate the window by up to a factor.
    pub fn label(&self) -> String {
        match self {
            Self::Session5h => "5h".to_owned(),
            Self::Weekly1w => "1w".to_owned(),
            Self::Monthly30d => "30d".to_owned(),
            Self::Other { secs } => format!("other({secs})"),
        }
    }

    /// The canonical length of the family: 5 h, 7 d, 30 d, or the observed seconds for `Other`.
    ///
    /// This is the *nominal* length used for display and for ordering windows; a session bucket
    /// reported as four hours still returns five here, because the family names a band rather than
    /// a measurement. Use [`WindowFamily::observed_seconds`] when the real length is what matters.
    pub fn nominal_seconds(&self) -> u64 {
        match self {
            Self::Session5h => 5 * 3600,
            Self::Weekly1w => 7 * 24 * 3600,
            Self::Monthly30d => 30 * 24 * 3600,
            Self::Other { secs } => *secs,
        }
    }

    /// The measured length, when this family was derived from one that has no familiar name.
    ///
    /// `None` for the three familiar bands: the caller discarded the exact length when it chose a
    /// band, and printing the band's nominal length here would present a guess as a measurement.
    /// Callers that need the real length for a familiar band have to keep it themselves.
    pub fn observed_seconds(&self) -> Option<u64> {
        match self {
            Self::Other { secs } => Some(*secs),
            Self::Session5h | Self::Weekly1w | Self::Monthly30d => None,
        }
    }

    /// Whether this family is outside the three familiar bands.
    pub fn is_other(&self) -> bool {
        matches!(self, Self::Other { .. })
    }

    /// Format a duration the way `formatWindowDuration` does: whole days, else whole hours, else
    /// seconds (`86400` → `1d`, `3600` → `1h`, `5400` → `5400s`).
    ///
    /// Zero mirrors the JavaScript exactly and yields `0d`; zero is not a window, so callers should
    /// keep it away from here rather than rely on that rendering.
    pub fn format_duration(seconds: u64) -> String {
        if seconds.is_multiple_of(86_400) {
            format!("{}d", seconds / 86_400)
        } else if seconds.is_multiple_of(3_600) {
            format!("{}h", seconds / 3_600)
        } else {
            format!("{seconds}s")
        }
    }
}

impl std::fmt::Display for WindowFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Session5h => f.write_str("5h"),
            Self::Weekly1w => f.write_str("1w"),
            Self::Monthly30d => f.write_str("30d"),
            Self::Other { secs } => write!(f, "other({secs})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_boundaries_stay_in_the_lower_family() {
        assert_eq!(
            WindowFamily::from_seconds(SESSION_MAX_SECONDS),
            WindowFamily::Session5h
        );
        assert_eq!(
            WindowFamily::from_seconds(WEEKLY_MAX_SECONDS),
            WindowFamily::Weekly1w
        );
        assert_eq!(
            WindowFamily::from_seconds(MONTHLY_MAX_SECONDS),
            WindowFamily::Monthly30d
        );
    }

    #[test]
    fn one_second_over_each_boundary_moves_up() {
        assert_eq!(
            WindowFamily::from_seconds(SESSION_MAX_SECONDS + 1),
            WindowFamily::Weekly1w
        );
        assert_eq!(
            WindowFamily::from_seconds(WEEKLY_MAX_SECONDS + 1),
            WindowFamily::Monthly30d
        );
        assert_eq!(
            WindowFamily::from_seconds(MONTHLY_MAX_SECONDS + 1),
            WindowFamily::Other {
                secs: MONTHLY_MAX_SECONDS + 1
            }
        );
    }

    #[test]
    fn familiar_lengths_land_where_the_plan_says() {
        assert_eq!(WindowFamily::from_seconds(0), WindowFamily::Session5h);
        assert_eq!(WindowFamily::from_seconds(3600), WindowFamily::Session5h);
        assert_eq!(
            WindowFamily::from_seconds(5 * 3600),
            WindowFamily::Session5h
        );
        assert_eq!(
            WindowFamily::from_seconds(12 * 3600),
            WindowFamily::Weekly1w
        );
        assert_eq!(
            WindowFamily::from_seconds(7 * 24 * 3600),
            WindowFamily::Weekly1w
        );
        assert_eq!(
            WindowFamily::from_seconds(2 * 24 * 3600),
            WindowFamily::Weekly1w
        );
        assert_eq!(
            WindowFamily::from_seconds(30 * 24 * 3600),
            WindowFamily::Monthly30d
        );
        assert_eq!(
            WindowFamily::from_seconds(365 * 24 * 3600),
            WindowFamily::Other { secs: 31_536_000 }
        );
    }

    #[test]
    fn labels_are_the_operator_facing_names() {
        assert_eq!(WindowFamily::Session5h.label(), "5h");
        assert_eq!(WindowFamily::Weekly1w.label(), "1w");
        assert_eq!(WindowFamily::Monthly30d.label(), "30d");
        assert_eq!(
            WindowFamily::Other {
                secs: MONTHLY_MAX_SECONDS + 1
            }
            .label(),
            "other(3456001)"
        );
        assert_eq!(WindowFamily::Session5h.to_string(), "5h");
    }

    #[test]
    fn nominal_and_observed_lengths_stay_distinct() {
        assert_eq!(WindowFamily::Session5h.nominal_seconds(), 18_000);
        assert_eq!(WindowFamily::Weekly1w.nominal_seconds(), 604_800);
        assert_eq!(WindowFamily::Monthly30d.nominal_seconds(), 2_592_000);
        assert_eq!(WindowFamily::Other { secs: 999 }.nominal_seconds(), 999);
        assert_eq!(
            WindowFamily::Other { secs: 999 }.observed_seconds(),
            Some(999)
        );
        // The three bands do not claim to know the measurement they were inferred from.
        assert_eq!(WindowFamily::Session5h.observed_seconds(), None);
        assert!(!WindowFamily::Session5h.is_other());
        assert!(WindowFamily::Other { secs: 999 }.is_other());
    }

    #[test]
    fn duration_formatting_matches_the_port() {
        assert_eq!(WindowFamily::format_duration(86_400), "1d");
        assert_eq!(WindowFamily::format_duration(604_800), "7d");
        assert_eq!(WindowFamily::format_duration(3_600), "1h");
        assert_eq!(WindowFamily::format_duration(21_600), "6h");
        assert_eq!(WindowFamily::format_duration(5_400), "5400s");
        // Mirrors `formatWindowDuration(0)`; zero is not a real window (see `from_seconds`).
        assert_eq!(WindowFamily::format_duration(0), "0d");
    }

    #[test]
    fn serde_round_trips_every_family() {
        for family in [
            WindowFamily::Session5h,
            WindowFamily::Weekly1w,
            WindowFamily::Monthly30d,
            WindowFamily::Other { secs: 3_456_001 },
        ] {
            let json = serde_json::to_string(&family).expect("family serialises");
            let back: WindowFamily = serde_json::from_str(&json).expect("family deserialises");
            assert_eq!(back, family, "round trip through {json}");
        }
        assert_eq!(
            serde_json::to_string(&WindowFamily::Session5h).unwrap(),
            "\"5h\""
        );
        assert_eq!(
            serde_json::to_string(&WindowFamily::Other { secs: 42 }).unwrap(),
            "{\"other\":{\"secs\":42}}"
        );
    }
}
