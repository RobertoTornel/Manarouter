//! Cursor Pro plan usage.
//!
//! Ported from OmniRoute's `open-sse/services/usage/cursor.ts` (MIT, © 2026 diegosouzapw).
//!
//! # Endpoint order
//!
//! PKCE tokens get the Bearer chain first: `POST /aiserver.v1.DashboardService/GetCurrentPeriodUsage`,
//! then `GET /api/usage/summary`, then `GET /auth/usage`, all on `https://api2.cursor.sh`. The
//! first payload shape we can interpret wins, just as it does in OmniRoute. This order matters:
//! the first endpoint gives plan spend in cents, while the older two report usage as their own
//! units and should only be consulted if the current endpoint did not return a usable plan.
//!
//! If those APIs do not yield a recognized plan, OmniRoute derives the WorkOS subject from the
//! access token's JWT and retries the dashboard's cookie-authenticated endpoint. This fallback is
//! ported last, so the provider's documented preference order stays intact.

use async_trait::async_trait;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::{QuotaRequest, QuotaResponse};
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::UsageQuota;

/// Cursor's first-choice gRPC-Web JSON endpoint.
pub const PERIOD_USAGE_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
/// Cursor's newer usage-summary endpoint.
pub const USAGE_SUMMARY_URL: &str = "https://api2.cursor.sh/api/usage/summary";
/// Cursor's legacy auth usage endpoint.
pub const AUTH_USAGE_URL: &str = "https://api2.cursor.sh/auth/usage";

/// Fetcher id, also used as the normalized quota source.
pub const SOURCE: &str = Cursor::ID;

/// Cursor Pro plan usage reader.
#[derive(Debug, Clone, Copy, Default)]
pub struct Cursor;

impl Cursor {
    /// The fetcher id and the provider id this reader claims.
    pub const ID: &'static str = "cursor";

    /// A stateless fetcher; the registry shares it behind an `Arc`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for Cursor {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        provider_id == Self::ID
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| Error::Config("cursor quota requires an account access token".into()))?;
        let token = credential.expose();
        let auth_headers = || {
            vec![
                ("Accept".to_owned(), "application/json".to_owned()),
                ("Authorization".to_owned(), format!("Bearer {token}")),
                ("User-Agent".to_owned(), "omniroute-cursor-quota".to_owned()),
            ]
        };

        // Keep the upstream's preference order: current period first, with the older summary and
        // auth endpoints as fallbacks for accounts whose response shape does not carry plan usage.
        let mut period = QuotaRequest::post_json(PERIOD_USAGE_URL, "{}")
            .header("Authorization", format!("Bearer {token}"))
            .header("Accept", "application/json")
            .header("User-Agent", "omniroute-cursor-quota")
            .header("Connect-Protocol-Version", "1");
        period.timeout = Some(crate::quota::http::DEFAULT_TIMEOUT);
        let period_response = ctx.http.request(period).await?;
        let period_data = usable_json(&period_response);
        if let Some(data) = period_data.as_ref() {
            if let Some(quota) = parse_period_usage(data) {
                return Ok(quota.with_source(SOURCE).with_display_name("Cursor Pro"));
            }
        }

        let mut summary = QuotaRequest::get(USAGE_SUMMARY_URL);
        for (name, value) in auth_headers() {
            summary = summary.header(name, value);
        }
        summary.timeout = Some(crate::quota::http::DEFAULT_TIMEOUT);
        let summary_response = ctx.http.request(summary).await?;
        if let Some(data) = usable_json(&summary_response) {
            if let Some(quota) = parse_usage_summary(&data) {
                return Ok(quota.with_source(SOURCE).with_display_name("Cursor Pro"));
            }
        }

        let mut auth_usage = QuotaRequest::get(AUTH_USAGE_URL);
        for (name, value) in auth_headers() {
            auth_usage = auth_usage.header(name, value);
        }
        auth_usage.timeout = Some(crate::quota::http::DEFAULT_TIMEOUT);
        let auth_response = ctx.http.request(auth_usage).await?;
        if let Some(data) = usable_json(&auth_response) {
            if let Some(quota) = parse_auth_usage(&data) {
                return Ok(quota.with_source(SOURCE).with_display_name("Cursor Pro"));
            }
        }

        // Cursor IDE imports often need the dashboard's WorkOS cookie route rather than a Bearer
        // quota API. Match OmniRoute: use an explicitly supplied userId first, otherwise decode the
        // JWT sub. The access token is sent only as the cookie's session credential.
        let user_id = ctx
            .account
            .extra
            .get("userId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .or_else(|| decode_jwt_sub(token));
        if let Some(user_id) = user_id {
            let mut dashboard = QuotaRequest::post_json(
                "https://cursor.com/api/dashboard/get-current-period-usage", "{}")
                .header("Cookie", format!("WorkosCursorSessionToken={user_id}::{token}"))
                .header("Origin", "https://cursor.com")
                .header("Referer", "https://cursor.com/dashboard/spending")
                .header("Accept", "application/json")
                .header("User-Agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36");
            dashboard.timeout = Some(crate::quota::http::DEFAULT_TIMEOUT);
            let dashboard_response = ctx.http.request(dashboard).await?;
            if let Some(data) = usable_json(&dashboard_response) {
                if let Some(quota) = parse_period_usage(&data) {
                    return Ok(quota.with_source(SOURCE).with_display_name("Cursor Pro"));
                }
            }
            if matches!(dashboard_response.status, 401 | 403 | 302 | 303) {
                return Err(Error::Config(
                    "cursor dashboard session was unauthorized".into(),
                ));
            }
        }

        if matches!(period_response.status, 401 | 403)
            || matches!(summary_response.status, 401 | 403)
            || matches!(auth_response.status, 401 | 403)
        {
            return Err(Error::Config(
                "cursor quota request was unauthorized".into(),
            ));
        }
        Err(Error::NotFound(
            "cursor usage endpoints returned no recognized plan usage".into(),
        ))
    }
}

/// Decode the JWT subject Cursor uses as the user id for its WorkOS session cookie.
fn decode_jwt_sub(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims
        .get("sub")?
        .as_str()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Return a JSON object only for a successful HTTP response with a non-empty object body.
fn usable_json(response: &QuotaResponse) -> Option<Value> {
    if !(200..300).contains(&response.status) {
        return None;
    }
    let value: Value = serde_json::from_slice(&response.body).ok()?;
    value.as_object().filter(|object| !object.is_empty())?;
    Some(value)
}

/// Translate the current-period response's `planUsage` cents and billing cycle end.
pub fn parse_period_usage(body: &Value) -> Option<UsageQuota> {
    let plan_usage = body.get("planUsage")?.as_object()?;
    if plan_usage.is_empty() {
        return None;
    }
    let limit = number_field(plan_usage, &["limit", "limitCents", "totalLimitCents"])
        .unwrap_or(0.0)
        .max(0.0);
    let spend = number_field(plan_usage, &["includedSpend", "usedCents", "used"])
        .map(|value| value.max(0.0))
        .unwrap_or_else(|| {
            number_field(plan_usage, &["totalSpend"])
                .unwrap_or(0.0)
                .max(0.0)
        });
    let total_percent = number_field(plan_usage, &["totalPercentUsed", "percentUsed"])
        .or_else(|| (limit > 0.0).then_some(spend * 100.0 / limit))?;
    let effective_limit = if limit > 0.0 { limit } else { 100.0 };
    let reset_at = body
        .get("billingCycleEnd")
        .or_else(|| plan_usage.get("billingCycleEnd"))
        .and_then(crate::quota::types::parse_reset_time);
    let mut quota = percent_quota(total_percent, effective_limit, reset_at, true);
    quota.used = (if limit > 0.0 {
        spend
    } else {
        (effective_limit * clamp_percent(total_percent) / 100.0).round()
    })
    .min(effective_limit)
        / 100.0;
    quota.total = effective_limit / 100.0;
    quota.remaining = (effective_limit - quota.used * 100.0).max(0.0) / 100.0;
    quota.remaining_percentage = 100.0 - clamp_percent(total_percent);
    quota.details.push(crate::quota::types::QuotaDetail {
        name: "Auto + Composer".into(),
        used: number_field(plan_usage, &["autoPercentUsed"]).unwrap_or(0.0),
    });
    quota.details.push(crate::quota::types::QuotaDetail {
        name: "API".into(),
        used: number_field(plan_usage, &["apiPercentUsed"]).unwrap_or(0.0),
    });
    Some(quota)
}

/// Translate Cursor's alternate `individualUsage.plan` response.
pub fn parse_usage_summary(body: &Value) -> Option<UsageQuota> {
    let plan = body.get("individualUsage")?.get("plan")?.as_object()?;
    if plan.is_empty() {
        return None;
    }
    let used = number_field(plan, &["used"]);
    let limit = number_field(plan, &["limit"]);
    let raw_percent = number_field(plan, &["totalPercentUsed"]);
    let percent = clamp_percent(raw_percent.or_else(|| Some(used? * 100.0 / limit?))?);
    let limit = limit.filter(|limit| *limit > 0.0).unwrap_or(100.0);
    let reset_at = body
        .get("billingCycleEnd")
        .and_then(crate::quota::types::parse_reset_time);
    // `buildPlanUsageQuotas` treats this legacy summary's `used`/`limit` as cents, just like
    // the current-period endpoint, even though the summary percentages are already in 0..100.
    let mut quota = percent_quota(percent, limit, reset_at, true);
    quota.used = used.unwrap_or(percent.round()) / 100.0;
    quota.total = limit / 100.0;
    quota.remaining = (limit - quota.used * 100.0).max(0.0) / 100.0;
    quota.remaining_percentage = 100.0 - percent;
    Some(quota)
}

/// Translate the legacy per-model request buckets, preferring `gpt-4` when present.
pub fn parse_auth_usage(body: &Value) -> Option<UsageQuota> {
    let bucket = body.get("gpt-4").and_then(Value::as_object);
    let mut used = bucket.and_then(|map| number_field(map, &["numRequests", "used"]));
    let mut limit =
        bucket.and_then(|map| number_field(map, &["maxRequestUsage", "limit", "maxRequests"]));
    if !matches!((used, limit), (Some(u), Some(l)) if l > 0.0 && u.is_finite()) {
        for (key, value) in body.as_object()? {
            if matches!(key.as_str(), "startOfMonth" | "billingCycleStart") {
                continue;
            }
            let Some(map) = value.as_object() else {
                continue;
            };
            let candidate_used = number_field(map, &["numRequests", "used"]);
            let candidate_limit = number_field(map, &["maxRequestUsage", "limit", "maxRequests"]);
            if matches!((candidate_used, candidate_limit), (Some(u), Some(l)) if l > 0.0 && u.is_finite())
            {
                used = candidate_used;
                limit = candidate_limit;
                break;
            }
        }
    }
    let (used, limit) = match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => (used.max(0.0), limit),
        _ => return None,
    };
    let percent = clamp_percent(used * 100.0 / limit);
    let reset_at = body
        .get("startOfMonth")
        .or_else(|| body.get("billingCycleStart"))
        .and_then(crate::quota::types::parse_reset_time)
        .and_then(next_month_same_day);
    let mut quota = percent_quota(percent, limit, reset_at, true);
    quota.used = used;
    quota.total = limit;
    quota.remaining = (limit - used).max(0.0);
    quota.remaining_percentage = 100.0 - percent;
    Some(quota)
}

fn percent_quota(percent: f64, total: f64, reset_at: Option<i64>, reported: bool) -> UsageQuota {
    let percent = clamp_percent(percent);
    let used = total * percent / 100.0;
    if reported {
        UsageQuota::from_counts(used, total, reset_at)
    } else {
        UsageQuota::from_counts_unreported(used, total, reset_at)
    }
}

fn number_field(map: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| match map.get(*key)? {
        Value::Number(number) => number.as_f64().filter(|number| number.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite()),
        _ => None,
    })
}

fn clamp_percent(percent: f64) -> f64 {
    if !percent.is_finite() {
        0.0
    } else {
        percent.clamp(0.0, 100.0)
    }
}

/// Match OmniRoute's next-month reset derived from the first day of the active month.
fn next_month_same_day(start_ms: i64) -> Option<i64> {
    let date =
        time::OffsetDateTime::from_unix_timestamp_nanos((start_ms as i128) * 1_000_000).ok()?;
    let next_month = if date.month() == time::Month::December {
        time::Date::from_calendar_date(date.year() + 1, time::Month::January, date.day()).ok()?
    } else {
        time::Date::from_calendar_date(date.year(), date.month().next(), date.day()).ok()?
    };
    let midnight = next_month.with_time(time::Time::MIDNIGHT).assume_utc();
    i64::try_from(midnight.unix_timestamp_nanos() / 1_000_000).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};
    use async_trait::async_trait;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};

    const PERIOD: &str = r#"{"planUsage":{"limit":5000,"includedSpend":1250,"totalPercentUsed":25,"autoPercentUsed":20,"apiPercentUsed":5},"billingCycleEnd":1782864000000}"#;
    const SUMMARY: &str =
        r#"{"individualUsage":{"plan":{"used":2,"limit":10}},"billingCycleEnd":1782864000000}"#;
    const AUTH: &str =
        r#"{"gpt-4":{"numRequests":40,"maxRequestUsage":100},"startOfMonth":1767225600000}"#;

    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: Mutex<Vec<QuotaResponse>>,
        seen: Mutex<Vec<QuotaRequest>>,
    }
    impl FakeHttp {
        fn new(replies: Vec<QuotaResponse>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
                seen: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, req: QuotaRequest) -> Result<QuotaResponse> {
            self.seen.lock().unwrap().push(req);
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Ok(QuotaResponse {
                    status: 404,
                    headers: vec![],
                    body: Bytes::from_static(b"{}"),
                });
            }
            Ok(replies.remove(0))
        }
    }
    fn reply(body: &str) -> QuotaResponse {
        QuotaResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from(body.to_owned()),
        }
    }

    #[test]
    fn period_plan_parses_cents_and_keeps_exact_windows() {
        let quota = parse_period_usage(&serde_json::from_str(PERIOD).unwrap()).unwrap();
        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 50.0);
        assert_eq!(quota.remaining, 37.5);
        assert_eq!(quota.remaining_percentage, 75.0);
        assert!(quota.fraction_reported);
        assert_eq!(quota.details.len(), 2);
    }
    #[test]
    fn summary_and_auth_shapes_parse_as_upstream_reports_them() {
        let summary = parse_usage_summary(&serde_json::from_str(SUMMARY).unwrap()).unwrap();
        assert_eq!(
            (summary.used, summary.total, summary.remaining),
            (0.02, 0.1, 0.08)
        );
        let auth = parse_auth_usage(&serde_json::from_str(AUTH).unwrap()).unwrap();
        assert_eq!((auth.used, auth.total, auth.remaining), (40.0, 100.0, 60.0));
        assert!(auth.reset_at.is_some());
    }
    #[test]
    fn absent_plan_or_unusable_data_is_not_a_fabricated_zero_quota() {
        let no_plan: Value = serde_json::from_str(r#"{"individualUsage":{}}"#).unwrap();
        assert!(parse_period_usage(&no_plan).is_none());
        assert!(parse_usage_summary(&no_plan).is_none());
        assert!(parse_auth_usage(&no_plan).is_none());
    }
    #[tokio::test]
    async fn fetch_uses_bearer_chain_and_returns_first_interpretable_shape() {
        let fake = FakeHttp::new(vec![reply(PERIOD)]);
        let ctx = QuotaFetchContext::new(Cursor::ID, Dialect::OpenAi, fake.clone())
            .with_credential("token-not-a-secret");
        let quota = Cursor::new().fetch(&ctx).await.unwrap();
        assert_eq!(quota.source.as_deref(), Some(Cursor::ID));
        assert_eq!(fake.seen.lock().unwrap().len(), 1);
        let request = &fake.seen.lock().unwrap()[0];
        assert_eq!(request.url, PERIOD_USAGE_URL);
        assert_eq!(
            request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .unwrap()
                .1,
            "Bearer token-not-a-secret"
        );
    }
    #[test]
    fn supports_only_the_cursor_provider() {
        let fetcher = Cursor::new();
        assert!(fetcher.supports(Dialect::OpenAi, Cursor::ID, &QuotaAccount::new()));
        assert!(!fetcher.supports(Dialect::OpenAi, "not-cursor", &QuotaAccount::new()));
    }

    #[test]
    fn jwt_subject_is_decoded_for_the_legacy_cookie_fallback() {
        let claims = URL_SAFE_NO_PAD.encode(br#"{"sub":"user-123"}"#);
        assert_eq!(
            decode_jwt_sub(&format!("header.{claims}.signature")).as_deref(),
            Some("user-123")
        );
        assert_eq!(decode_jwt_sub("not-a-jwt"), None);
    }
    #[tokio::test]
    async fn missing_credential_is_rejected_before_http() {
        let fake = FakeHttp::new(vec![]);
        let ctx = QuotaFetchContext::new(Cursor::ID, Dialect::OpenAi, fake.clone());
        assert!(matches!(
            Cursor::new().fetch(&ctx).await,
            Err(Error::Config(_))
        ));
        assert!(fake.seen.lock().unwrap().is_empty());
    }
}
