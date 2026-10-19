//! MiniMax coding-plan usage (`minimax` and `minimax-cn`).
//!
//! Ported from OmniRoute's `open-sse/services/usage/minimax.ts` (MIT, © 2026 diegosouzapw).
//!
//! Two endpoints are tried in order per region, because a key that works on one answers `404` on
//! the other:
//!
//! | provider | first | fallback |
//! |---|---|---|
//! | `minimax` | `https://www.minimax.io/v1/token_plan/remains` | `https://api.minimax.io/v1/api/openplatform/coding_plan/remains` |
//! | `minimax-cn` | `https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains` | `https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains` |
//!
//! `404`, `405` and `5xx` mean "try the next"; `401`/`403`, an envelope `base_resp.status_code` of
//! `1004`, or an auth-shaped `status_msg` mean "stop and report".
//!
//! # `countMeansRemaining`, the column-direction flag
//!
//! Upstream sets `countMeansRemaining = usageUrl.includes("/coding_plan/remains")`. The two
//! endpoints return the same column name (`current_interval_usage_count`) but disagree on its
//! meaning: on `token_plan/remains` it is **used**, on `coding_plan/remains` it is **remaining**.
//! Reproduced here, keyed off the URL that produced the payload, and never guessed from the
//! endpoint that was *asked for* elsewhere.
//!
//! # The `remains_time` trap
//!
//! `remains_time` / `weekly_remains_time` are **milliseconds relative**, so the reset is
//! `now + remains_time`. Reading them as seconds (or adding the value to `end_time`) would place
//! the reset roughly 1000× too far out; the helper below is the only place they are read, and this
//! comment is here so nobody "fixes" it into seconds. When no relative value is present, the
//! absolute `end_time` / `weekly_end_time` is parsed by
//! [`crate::quota::types::parse_reset_time`] instead.
//!
//! # Both windows
//!
//! The upstream map has two entries — `session (5h)` and `weekly (7d)` — and the trait's
//! [`UsageQuota`] return is a single value, so [`MiniMax::fetch`] returns the **session** window
//! (the one an operator acts on first) and stamps its key onto `display_name`. [`parse_windows`]
//! exposes the full keyed map, and [`windows`] the same rows with their [`WindowFamily`]; the panel
//! can use them once it renders more than one window per account.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{parse_reset_time, QuotaWindow, UsageQuota, WindowFamily};

/// Fetcher id, stamped onto [`UsageQuota::source`].
pub const ID: &str = "minimax";

/// The upstream key for the five-hour window.
pub const SESSION_KEY: &str = "session (5h)";
/// The upstream key for the seven-day window.
pub const WEEKLY_KEY: &str = "weekly (7d)";

/// Coding-plan endpoints for the international provider, tried in order.
pub const MINIMAX_USAGE_URLS: [&str; 2] = [
    "https://www.minimax.io/v1/token_plan/remains",
    "https://api.minimax.io/v1/api/openplatform/coding_plan/remains",
];

/// Coding-plan endpoints for the mainland provider, tried in order.
pub const MINIMAX_CN_USAGE_URLS: [&str; 2] = [
    "https://www.minimaxi.com/v1/api/openplatform/coding_plan/remains",
    "https://api.minimaxi.com/v1/api/openplatform/coding_plan/remains",
];

/// The MiniMax coding-plan fetcher.
#[derive(Debug, Clone, Copy, Default)]
pub struct MiniMax;

impl MiniMax {
    /// A fetcher with no configuration of its own.
    pub fn new() -> Self {
        Self
    }
}

/// The ordered endpoints for a provider id, or `None` when the fetcher does not serve it.
pub fn usage_urls(provider_id: &str) -> Option<&'static [&'static str]> {
    match provider_id {
        "minimax" => Some(&MINIMAX_USAGE_URLS),
        "minimax-cn" => Some(&MINIMAX_CN_USAGE_URLS),
        _ => None,
    }
}

fn to_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.trim().is_empty() => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// `obj[snake]` falling back to `obj[camel]` — upstream's `getFieldValue`.
fn field<'a>(obj: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    obj.get(snake).or_else(|| obj.get(camel))
}

/// Only the text/coding buckets describe the coding plan; media buckets (`video`, `image`, `music`)
/// must not win the representative-row pick.
pub fn is_text_quota_model(model_name: &str) -> bool {
    let normalized = model_name.trim().to_ascii_lowercase();
    normalized.starts_with("minimax-m")
        || normalized.starts_with("coding-plan")
        || normalized == "general"
}

/// Whether a `base_resp.status_msg` reads like an authentication/plan failure.
fn looks_like_auth_failure(message: &str) -> bool {
    const NEEDLES: [&str; 6] = [
        "token plan",
        "coding plan",
        "invalid api key",
        "invalid key",
        "unauthorized",
        "inactive",
    ];
    let normalized = message.to_ascii_lowercase();
    NEEDLES.iter().any(|needle| normalized.contains(needle))
}

/// The session total used both for the session window and for the fallback plan label.
pub fn session_total(model: &Value) -> f64 {
    to_number(field(
        model,
        "current_interval_total_count",
        "currentIntervalTotalCount",
    ))
    .unwrap_or(0.0)
    .max(0.0)
}

/// The weekly total.
pub fn weekly_total(model: &Value) -> f64 {
    to_number(field(
        model,
        "current_weekly_total_count",
        "currentWeeklyTotalCount",
    ))
    .unwrap_or(0.0)
    .max(0.0)
}

/// The row with the largest total; ties keep the first, exactly as upstream's `reduce` does.
fn representative<'a>(models: &[&'a Value], total: impl Fn(&Value) -> f64) -> Option<&'a Value> {
    let with_quota: Vec<&'a Value> = models
        .iter()
        .copied()
        .filter(|model| total(model) > 0.0)
        .collect();
    let pool = if with_quota.is_empty() {
        models.to_vec()
    } else {
        with_quota
    };
    let mut best: Option<&'a Value> = None;
    for model in pool {
        if best.is_none_or(|current| total(model) > total(current)) {
            best = Some(model);
        }
    }
    best
}

/// `now + remains_time` when the relative field is positive, else the absolute `end_time`.
fn reset_at(
    model: &Value,
    captured_at_ms: i64,
    remains_keys: (&str, &str),
    end_keys: (&str, &str),
) -> Option<i64> {
    let remains_ms = to_number(field(model, remains_keys.0, remains_keys.1)).unwrap_or(0.0);
    if remains_ms > 0.0 {
        // Milliseconds relative. Reading this as seconds is the trap this comment exists for.
        return Some(captured_at_ms.saturating_add(remains_ms as i64));
    }
    field(model, end_keys.0, end_keys.1).and_then(parse_reset_time)
}

/// One window: a count quota when the endpoint reported a real total, else the remaining-percent
/// signal a Coding Plan key returns with zero counts.
fn build_window(
    models: &[&Value],
    total: impl Fn(&Value) -> f64,
    usage_count_keys: (&str, &str),
    percent_keys: (&str, &str),
    reset_keys: ((&str, &str), (&str, &str)),
    captured_at_ms: i64,
    count_means_remaining: bool,
) -> Option<UsageQuota> {
    let model = representative(models, &total)?;
    let reset = reset_at(model, captured_at_ms, reset_keys.0, reset_keys.1);
    let total_value = total(model);

    if total_value > 0.0 {
        let count = to_number(field(model, usage_count_keys.0, usage_count_keys.1))
            .unwrap_or(0.0)
            .max(0.0);
        let used = if count_means_remaining {
            (total_value - count).max(0.0)
        } else {
            count
        };
        return Some(UsageQuota::from_counts(used, total_value, reset));
    }

    // The Coding Plan reports zero counts and a 0..100 remaining percent instead.
    let remaining = field(model, percent_keys.0, percent_keys.1)
        .filter(|value| !value.is_null())
        .and_then(|value| to_number(Some(value)))?;
    let clamped = remaining.clamp(0.0, 100.0);
    Some(UsageQuota::from_counts(100.0 - clamped, 100.0, reset))
}

/// The text-model rows of a payload.
fn text_models(body: &Value) -> Vec<&Value> {
    body.get("model_remains")
        .or_else(|| body.get("modelRemains"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|row| {
                    let name = field(row, "model_name", "modelName")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    is_text_quota_model(name)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Strip MiniMax's own boilerplate out of a plan title, as upstream does with three regexes.
fn clean_plan_title(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    if text
        .get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("minimax "))
    {
        text = text.get(8..).unwrap_or_default().to_string();
    }
    // Drop a case-insensitive "coding plan" and collapse the whitespace it leaves behind.
    let lowered = text.to_ascii_lowercase();
    if let Some(index) = lowered.find("coding plan") {
        text.replace_range(index..index + "coding plan".len(), " ");
    }
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The plan label, from the payload title or the session totals.
pub fn plan_label(body: &Value, models: &[&Value]) -> String {
    let raw = ["current_subscribe_title", "plan_name", "plan"]
        .iter()
        .chain(["current_plan_title", "combo_title"].iter())
        .find_map(|key| {
            body.get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .map(clean_plan_title)
        .filter(|value| !value.is_empty());

    raw.or_else(|| infer_plan_label(models))
        .unwrap_or_else(|| "Coding Plan".to_string())
}

/// The tier inferred from the largest session total.
fn infer_plan_label(models: &[&Value]) -> Option<String> {
    let max_session_total = models
        .iter()
        .map(|model| session_total(model))
        .fold(0.0_f64, f64::max);
    if max_session_total >= 15_000.0 {
        Some("Max".to_string())
    } else if max_session_total >= 4_500.0 {
        Some("Plus".to_string())
    } else if max_session_total >= 1_500.0 {
        Some("Starter".to_string())
    } else {
        None
    }
}

/// The keyed windows upstream returns: `session (5h)` and `weekly (7d)`, each optional.
///
/// Pure, so tests drive it with captured JSON and a frozen `captured_at_ms` instead of a clock.
pub fn parse_windows(
    body: &Value,
    captured_at_ms: i64,
    count_means_remaining: bool,
) -> Vec<(String, UsageQuota)> {
    let models = text_models(body);
    let mut windows = Vec::new();

    if let Some(session) = build_window(
        &models,
        session_total,
        ("current_interval_usage_count", "currentIntervalUsageCount"),
        (
            "current_interval_remaining_percent",
            "currentIntervalRemainingPercent",
        ),
        (("remains_time", "remainsTime"), ("end_time", "endTime")),
        captured_at_ms,
        count_means_remaining,
    ) {
        windows.push((SESSION_KEY.to_string(), session));
    }

    if let Some(weekly) = build_window(
        &models,
        weekly_total,
        ("current_weekly_usage_count", "currentWeeklyUsageCount"),
        (
            "current_weekly_remaining_percent",
            "currentWeeklyRemainingPercent",
        ),
        (
            ("weekly_remains_time", "weeklyRemainsTime"),
            ("weekly_end_time", "weeklyEndTime"),
        ),
        captured_at_ms,
        count_means_remaining,
    ) {
        windows.push((WEEKLY_KEY.to_string(), weekly));
    }

    windows
}

/// The same windows as [`QuotaWindow`]s, carrying the family their length implies.
pub fn windows(body: &Value, captured_at_ms: i64, count_means_remaining: bool) -> Vec<QuotaWindow> {
    parse_windows(body, captured_at_ms, count_means_remaining)
        .into_iter()
        .map(|(key, quota)| {
            let family = if key == SESSION_KEY {
                WindowFamily::Session5h
            } else {
                WindowFamily::Weekly1w
            };
            QuotaWindow::from_quota(family, &quota).with_key(key)
        })
        .collect()
}

/// Wall-clock milliseconds, the reference point for the relative `remains_time` fields.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl QuotaFetcher for MiniMax {
    fn id(&self) -> &'static str {
        ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        usage_urls(provider_id).is_some()
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let urls = usage_urls(&ctx.provider_id).ok_or_else(|| {
            Error::Config(format!(
                "provider {:?} is not a MiniMax coding-plan provider",
                ctx.provider_id
            ))
        })?;
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "provider {:?} has no credential; the MiniMax remains endpoint needs a bearer key",
                    ctx.provider_id
                ))
            })?;

        let mut last_error: Option<Error> = None;
        for (index, url) in urls.iter().enumerate() {
            let can_fallback = index + 1 < urls.len();
            let request = QuotaRequest::get(*url)
                .bearer(credential.expose())
                .header("Accept", "application/json")
                .header("Content-Type", "application/json");

            let response = match ctx.http.request(request).await {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error);
                    if can_fallback {
                        continue;
                    }
                    break;
                }
            };

            let raw = response.body_str();
            let payload: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
            let status_code = to_number(
                field(&payload, "base_resp", "baseResp")
                    .and_then(|resp| field(resp, "status_code", "statusCode")),
            )
            .unwrap_or(0.0);
            let status_msg = field(&payload, "base_resp", "baseResp")
                .and_then(|resp| field(resp, "status_msg", "statusMsg"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();

            // Authentication failures never fall through: the next host would reject the same key.
            if response.status == 401
                || response.status == 403
                || status_code as i64 == 1004
                || looks_like_auth_failure(&status_msg)
            {
                return Err(Error::InvalidState(format!(
                    "minimax quota rejected the credential (HTTP {}): {}",
                    response.status,
                    if status_msg.is_empty() {
                        "no upstream message"
                    } else {
                        status_msg.as_str()
                    }
                )));
            }

            if !response.is_success() {
                let retryable =
                    response.status == 404 || response.status == 405 || response.status >= 500;
                last_error = Some(Error::InvalidState(format!(
                    "minimax remains endpoint {} returned HTTP {}",
                    *url, response.status
                )));
                if retryable && can_fallback {
                    continue;
                }
                break;
            }

            if status_code != 0.0 {
                return Err(Error::InvalidState(format!(
                    "minimax remains envelope failed with status_code {}: {}",
                    status_code, status_msg
                )));
            }

            let keyed = parse_windows(&payload, now_ms(), url.contains("/coding_plan/remains"));
            let Some((key, quota)) = keyed.into_iter().next() else {
                last_error = Some(Error::NotFound(format!(
                    "minimax remains endpoint {} returned no text-model quota",
                    *url
                )));
                if can_fallback {
                    continue;
                }
                break;
            };

            let display = plan_label(&payload, &text_models(&payload));
            return Ok(quota
                .with_source(ID)
                .with_display_name(format!("{display} · {key}")));
        }

        Err(last_error.unwrap_or_else(|| {
            Error::InvalidState("minimax remains endpoints produced no usable quota".into())
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaRequest, QuotaResponse};

    const CAPTURED_AT_MS: i64 = 1_700_000_000_000;

    /// A `token_plan/remains` payload: the count column means **used**.
    const TOKEN_PLAN: &str = r#"{
        "base_resp": { "status_code": 0, "status_msg": "success" },
        "current_subscribe_title": "MiniMax Coding Plan",
        "model_remains": [
            {
                "model_name": "general",
                "current_interval_total_count": 100,
                "current_interval_usage_count": 30,
                "remains_time": 3600000,
                "current_weekly_total_count": 1000,
                "current_weekly_usage_count": 250,
                "weekly_remains_time": 86400000
            },
            {
                "model_name": "video-01",
                "current_interval_total_count": 999999,
                "current_interval_usage_count": 1
            }
        ]
    }"#;

    /// The same numbers on a `coding_plan/remains` endpoint: the count column means **remaining**.
    const CODING_PLAN: &str = r#"{
        "base_resp": { "status_code": 0, "status_msg": "success" },
        "plan_name": "MiniMax Plus",
        "model_remains": [
            {
                "model_name": "coding-plan-v1",
                "current_interval_total_count": 100,
                "current_interval_usage_count": 70,
                "remains_time": 1000,
                "current_weekly_total_count": 1000,
                "current_weekly_usage_count": 750,
                "weekly_remains_time": 2000
            }
        ]
    }"#;

    /// Counts are zero and the remaining percent carries the signal instead.
    const PERCENT_ONLY: &str = r#"{
        "base_resp": { "status_code": 0 },
        "model_remains": [
            {
                "model_name": "general",
                "current_interval_total_count": 0,
                "current_interval_remaining_percent": 25,
                "end_time": 1700003600
            }
        ]
    }"#;

    const AUTH_1004: &str = r#"{
        "base_resp": { "status_code": 1004, "status_msg": "invalid api key" }
    }"#;

    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: Mutex<VecDeque<QuotaResponse>>,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(replies: Vec<QuotaResponse>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("seen lock").clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen.lock().expect("seen lock").push(request);
            self.replies
                .lock()
                .expect("replies lock")
                .pop_front()
                .ok_or_else(|| Error::InvalidState("FakeHttp: no canned reply left".into()))
        }
    }

    fn json_response(status: u16, body: &str) -> QuotaResponse {
        QuotaResponse {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Bytes::from(body.to_owned()),
        }
    }

    fn context(http: Arc<FakeHttp>, provider: &str) -> QuotaFetchContext {
        QuotaFetchContext::new(provider, Dialect::OpenAi, http).with_credential("mm-secret")
    }

    #[test]
    fn token_plan_counts_are_used_and_coding_plan_counts_are_remaining() {
        let body: Value = serde_json::from_str(TOKEN_PLAN).expect("canned json parses");
        let used_side = parse_windows(&body, CAPTURED_AT_MS, false);
        let session = &used_side[0].1;
        assert_eq!(session.used, 30.0, "count column means used on token_plan");
        assert_eq!(session.total, 100.0);

        let body: Value = serde_json::from_str(CODING_PLAN).expect("canned json parses");
        let remaining_side = parse_windows(&body, CAPTURED_AT_MS, true);
        let session = &remaining_side[0].1;
        assert_eq!(
            session.used, 30.0,
            "count column means remaining on coding_plan"
        );
        assert_eq!(session.total, 100.0);
    }

    #[test]
    fn remains_time_is_milliseconds_relative_to_now() {
        let body: Value = serde_json::from_str(TOKEN_PLAN).expect("canned json parses");
        let keyed = parse_windows(&body, CAPTURED_AT_MS, false);
        assert_eq!(keyed[0].0, SESSION_KEY);
        assert_eq!(
            keyed[0].1.reset_at,
            Some(CAPTURED_AT_MS + 3_600_000),
            "remains_time is milliseconds, not seconds"
        );
        assert_eq!(keyed[1].0, WEEKLY_KEY);
        assert_eq!(keyed[1].1.reset_at, Some(CAPTURED_AT_MS + 86_400_000));
    }

    #[test]
    fn absent_relative_time_falls_back_to_an_absolute_end_time() {
        let body: Value = serde_json::from_str(PERCENT_ONLY).expect("canned json parses");
        let keyed = parse_windows(&body, CAPTURED_AT_MS, false);
        assert_eq!(keyed.len(), 1);
        assert_eq!(keyed[0].1.used, 75.0, "used = 100 - remaining_percent");
        assert_eq!(keyed[0].1.total, 100.0);
        assert_eq!(keyed[0].1.reset_at, Some(1_700_003_600_000));
    }

    #[test]
    fn media_models_never_win_the_representative_pick() {
        let body: Value = serde_json::from_str(TOKEN_PLAN).expect("canned json parses");
        let models = text_models(&body);
        assert_eq!(models.len(), 1, "video-01 is excluded");
        assert!(is_text_quota_model("general"));
        assert!(is_text_quota_model("minimax-m2"));
        assert!(is_text_quota_model("Coding-Plan-V1"));
        assert!(!is_text_quota_model("video-01"));
        assert!(!is_text_quota_model("image-01"));
    }

    #[test]
    fn plan_label_comes_from_the_title_or_the_session_total() {
        let titled: Value = serde_json::from_str(CODING_PLAN).expect("canned json parses");
        // The upstream title cleaner drops the vendor prefix.
        assert_eq!(plan_label(&titled, &text_models(&titled)), "Plus");

        let untitled = serde_json::json!({
            "model_remains": [{ "model_name": "general", "current_interval_total_count": 5000 }]
        });
        let models = text_models(&untitled);
        assert_eq!(plan_label(&untitled, &models), "Plus");

        let no_models = serde_json::json!({});
        assert_eq!(plan_label(&no_models, &[]), "Coding Plan");
    }

    #[test]
    fn window_families_follow_the_real_lengths() {
        let body: Value = serde_json::from_str(TOKEN_PLAN).expect("canned json parses");
        let rows = windows(&body, CAPTURED_AT_MS, false);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label(), "5h");
        assert_eq!(rows[0].key.as_deref(), Some(SESSION_KEY));
        assert_eq!(rows[1].label(), "1w");
        assert_eq!(rows[1].key.as_deref(), Some(WEEKLY_KEY));
    }

    #[tokio::test]
    async fn a_404_falls_through_to_the_next_endpoint() {
        let http = Arc::new(FakeHttp::new(vec![
            json_response(404, "not found"),
            json_response(200, CODING_PLAN),
        ]));
        let quota = MiniMax::new()
            .fetch(&context(Arc::clone(&http), "minimax"))
            .await
            .expect("fallback succeeds");
        assert_eq!(quota.used, 30.0, "second endpoint is a coding_plan URL");
        assert_eq!(quota.source.as_deref(), Some(ID));

        let seen = http.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].url, MINIMAX_USAGE_URLS[0]);
        assert_eq!(seen[1].url, MINIMAX_USAGE_URLS[1]);
        let auth = seen[0]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("Bearer mm-secret"));
    }

    #[tokio::test]
    async fn envelope_status_code_1004_is_an_error() {
        let http = Arc::new(FakeHttp::new(vec![json_response(200, AUTH_1004)]));
        let error = MiniMax::new()
            .fetch(&context(Arc::clone(&http), "minimax"))
            .await
            .expect_err("1004 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
        assert_eq!(
            http.seen().len(),
            1,
            "an auth failure must not fall through"
        );
    }

    #[tokio::test]
    async fn a_non_retryable_status_is_an_error() {
        let http = Arc::new(FakeHttp::new(vec![json_response(400, "{}")]));
        let error = MiniMax::new()
            .fetch(&context(Arc::clone(&http), "minimax"))
            .await
            .expect_err("400 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
        assert_eq!(http.seen().len(), 1);
    }

    #[tokio::test]
    async fn mainland_provider_uses_the_minimaxi_twins() {
        let http = Arc::new(FakeHttp::new(vec![json_response(200, CODING_PLAN)]));
        MiniMax::new()
            .fetch(&context(Arc::clone(&http), "minimax-cn"))
            .await
            .expect("cn fetch succeeds");
        assert_eq!(http.seen()[0].url, MINIMAX_CN_USAGE_URLS[0]);
    }

    #[test]
    fn only_coding_plan_provider_ids_are_supported() {
        let fetcher = MiniMax::new();
        assert!(fetcher.supports(Dialect::OpenAi, "minimax", &QuotaAccount::new()));
        assert!(fetcher.supports(Dialect::OpenAi, "minimax-cn", &QuotaAccount::new()));
        assert!(!fetcher.supports(Dialect::OpenAi, "minimax-audio", &QuotaAccount::new()));
    }
}
