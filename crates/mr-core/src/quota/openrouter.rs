//! OpenRouter credit-pool and per-key USD limit usage.
//!
//! Ported from OmniRoute's `open-sse/services/usage/openrouter.ts`
//! (`buildCreditsQuota`) and `open-sse/services/openrouterQuotaFetcher.ts`
//! (`fetchOpenrouterQuota`, `parseOpenrouterKeyResponse`,
//! `parseOpenrouterCreditsResponse`) — MIT, © 2026 diegosouzapw.
//!
//! OpenRouter exposes two documented monitoring endpoints
//! (<https://openrouter.ai/docs/api/reference/limits>):
//!
//! * `GET /api/v1/key` -> `data.{limit, limit_remaining, limit_reset, usage, usage_daily,
//!   usage_weekly, usage_monthly, is_free_tier, byok_usage, include_byok_in_limit}`.
//!   `limit`/`limit_remaining` are a **per-key USD cap**; `null` means no cap was set.
//! * `GET /api/v1/credits` -> `data.{total_credits, total_usage}`, the account-level pool.
//!
//! **This provider has no 5h/1w/30d window and this file deliberately does not synthesise
//! one.** OpenRouter is credit-based, not subscription-based: the quota is a USD pool
//! (`total_credits - total_usage`) optionally capped flat by a per-key limit. Running it
//! through [`crate::quota::window::WindowFamily`] would invent a rolling family the
//! upstream never reported, which is exactly the lie the module docs warn about — so
//! `WindowFamily` is not referenced here at all. This is the deliberate exception in this
//! file.
//!
//! A *locally tracked* `:free` daily/RPM window is layered on top of this upstream by
//! OmniRoute's `openrouterFreeWindow.ts`; that is client-side bookkeeping, not an upstream
//! API, and is intentionally **not** ported here.
//!
//! Graceful degradation, as upstream: `/credits` is a best-effort secondary signal, so a
//! `/key` failure that is *not* an auth rejection degrades to a credits-only quota rather
//! than discarding a real balance. An auth rejection (401/403) on either endpoint, or two
//! unusable payloads, is an [`Error::Config`]/[`Error::NotFound`] — never a fabricated
//! `Ok(0/0)`, which an operator would read as an exhausted account.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::{QuotaRequest, QuotaResponse};
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, UsageQuota};

/// `GET` the per-key cap and usage counters.
pub const KEY_URL: &str = "https://openrouter.ai/api/v1/key";
/// `GET` the account-level credit pool.
pub const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

/// The fetcher id, also stamped onto the `source` field of what it returns.
pub const SOURCE: &str = OpenRouter::ID;

/// How the USD unit is carried: the ported [`UsageQuota`] has no `currency` field, so the
/// currency is named in the operator-facing `display_name` instead of being dropped.
const USD: &str = "USD";

/// OpenRouter's per-key limit and account credit pool.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenRouter;

impl OpenRouter {
    /// The fetcher id, and the provider id this fetcher claims.
    pub const ID: &str = "openrouter";

    /// A stateless fetcher; `Arc`-shared by the registry.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for OpenRouter {
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
            .ok_or_else(|| {
                Error::Config("openrouter quota requires an account API key".to_owned())
            })?;
        let token = credential.expose();

        let key = match send(ctx, KEY_URL, token).await? {
            response if is_auth_rejection(&response) => {
                return Err(auth_rejected("openrouter", &response));
            }
            response if response.is_success() => response
                .json()
                .ok()
                .and_then(|body| parse_key_response(&body)),
            // A transient/key-endpoint failure degrades to the credits-only read below.
            _ => None,
        };

        let credits = match send(ctx, CREDITS_URL, token).await? {
            response if is_auth_rejection(&response) => {
                return Err(auth_rejected("openrouter", &response));
            }
            response if response.is_success() => response
                .json()
                .ok()
                .and_then(|body| parse_credits_response(&body)),
            _ => None,
        };

        if key.is_none() && credits.is_none() {
            return Err(Error::NotFound(
                "openrouter returned neither a usable /key nor /credits payload".to_owned(),
            ));
        }

        Ok(build_quota(key.as_ref(), credits.as_ref()))
    }
}

/// The `/key` fields this fetcher reads. `limit` stays `Option` because `null` is the
/// meaningful "no per-key cap was set" state, not a zero.
#[derive(Debug, Clone, PartialEq)]
struct KeyFields {
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    /// Raw, so [`UsageQuota::from_usage`] can apply the shared reset parser: `null` means
    /// the cap never resets and stays `None`.
    limit_reset: Value,
    usage: f64,
    usage_daily: f64,
    usage_weekly: f64,
    usage_monthly: f64,
    is_free_tier: bool,
    byok_usage: Option<f64>,
    include_byok_in_limit: bool,
}

impl KeyFields {
    /// The per-bucket counters upstream parses but never turns into windows. Carried as
    /// detail rows so the numbers reach the panel without inventing a window family.
    fn details(&self) -> Vec<QuotaDetail> {
        let mut details = vec![
            QuotaDetail {
                name: "usage".to_owned(),
                used: self.usage,
            },
            QuotaDetail {
                name: "usage_daily".to_owned(),
                used: self.usage_daily,
            },
            QuotaDetail {
                name: "usage_weekly".to_owned(),
                used: self.usage_weekly,
            },
            QuotaDetail {
                name: "usage_monthly".to_owned(),
                used: self.usage_monthly,
            },
        ];
        if let Some(byok) = self.byok_usage {
            details.push(QuotaDetail {
                name: "byok_usage".to_owned(),
                used: byok,
            });
        }
        details
    }
}

/// The `/credits` fields this fetcher reads.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CreditsFields {
    total_credits: f64,
    total_usage: f64,
}

/// `parseOpenrouterKeyResponse`: `None` for a payload with no recognisable `data` object.
fn parse_key_response(body: &Value) -> Option<KeyFields> {
    let inner = body.get("data").unwrap_or(body);
    if !inner.is_object() || inner.as_object().is_none_or(|fields| fields.is_empty()) {
        return None;
    }
    Some(KeyFields {
        limit: opt_number(inner.get("limit")),
        limit_remaining: opt_number(inner.get("limit_remaining")),
        limit_reset: inner.get("limit_reset").cloned().unwrap_or(Value::Null),
        usage: opt_number(inner.get("usage")).unwrap_or(0.0),
        usage_daily: opt_number(inner.get("usage_daily")).unwrap_or(0.0),
        usage_weekly: opt_number(inner.get("usage_weekly")).unwrap_or(0.0),
        usage_monthly: opt_number(inner.get("usage_monthly")).unwrap_or(0.0),
        is_free_tier: inner.get("is_free_tier") == Some(&Value::Bool(true)),
        byok_usage: opt_number(inner.get("byok_usage")),
        include_byok_in_limit: inner.get("include_byok_in_limit") == Some(&Value::Bool(true)),
    })
}

/// `parseOpenrouterCreditsResponse`: absent or non-numeric fields stay `None` so a
/// half-populated payload cannot masquerade as `$0`.
fn parse_credits_response(body: &Value) -> Option<CreditsFields> {
    let inner = body.get("data").unwrap_or(body);
    Some(CreditsFields {
        total_credits: opt_number(inner.get("total_credits"))?,
        total_usage: opt_number(inner.get("total_usage"))?,
    })
}

/// `buildCreditsQuota` + `buildQuotaFromParts`, flattened onto one [`UsageQuota`].
///
/// `key`/`credits` are the two payloads, either of which may be absent. The caller has
/// already rejected the case where both are absent.
fn build_quota(key: Option<&KeyFields>, credits: Option<&CreditsFields>) -> UsageQuota {
    let has_key_limit = key.is_some_and(|fields| fields.limit.is_some());
    // `creditBalance` upstream: a per-key remaining beats the account pool.
    let credit_balance = key
        .and_then(|fields| fields.limit_remaining)
        .or_else(|| credits.map(|credits| credits.total_credits - credits.total_usage));
    let has_positive_pool =
        credit_balance.is_some_and(|balance| balance.is_finite() && balance > 0.0);

    if has_key_limit {
        let fields = key.expect("has_key_limit implies a key payload");
        let limit = fields.limit.expect("has_key_limit implies a limit");
        // `limit - (limitRemaining ?? limit)` exactly as `buildCreditsQuota`.
        let remaining = fields.limit_remaining.unwrap_or(limit);
        let used = limit - remaining;
        let mut quota = UsageQuota::from_usage(used, limit, &fields.limit_reset)
            .with_source(SOURCE)
            .with_display_name(format!("OpenRouter Key Limit ({USD})"));
        for detail in fields.details() {
            quota = quota.with_detail(detail);
        }
        return quota;
    }

    // No key cap. `total` is the account pool when it is positive, else the bare balance;
    // a non-positive pool is `unlimited`, never a fabricated 100% bar.
    let mut quota = if !has_positive_pool {
        UsageQuota::unlimited()
            .with_source(SOURCE)
            .with_display_name(format!("OpenRouter Credits ({USD}, no limit)"))
    } else {
        let balance = credit_balance.expect("has_positive_pool implies a balance");
        let total_credits = credits
            .map(|credits| credits.total_credits)
            .filter(|total| total.is_finite() && *total > 0.0);
        let total = total_credits.unwrap_or(balance);
        let used = (total - balance).max(0.0);
        let reset = key
            .map(|fields| fields.limit_reset.clone())
            .unwrap_or(Value::Null);
        // When `total` falls back to the bare balance there is no pool total to divide by;
        // the fraction is still derived from the balance itself (100% of what is known).
        UsageQuota::from_usage(used, total, &reset)
            .with_source(SOURCE)
            .with_display_name(format!("OpenRouter Credits ({USD})"))
    };
    if let Some(fields) = key {
        for detail in fields.details() {
            quota = quota.with_detail(detail);
        }
    }
    quota
}

/// One authenticated `GET` with `Accept: application/json`.
async fn send(ctx: &QuotaFetchContext, url: &str, token: &str) -> Result<QuotaResponse> {
    let request = QuotaRequest::get(url)
        .bearer(token)
        .header("Accept", "application/json");
    ctx.http.request(request).await
}

fn is_auth_rejection(response: &QuotaResponse) -> bool {
    matches!(response.status, 401 | 403)
}

fn auth_rejected(provider: &str, response: &QuotaResponse) -> Error {
    // The status only: a response body is upstream text, but the credential never enters it.
    Error::Config(format!(
        "{provider} rejected the account credential (HTTP {})",
        response.status
    ))
}

/// `toNullableNumber`: a finite number, or a numeric string; anything else is `None`.
fn opt_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|number| number.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|number| number.is_finite()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use http::Method;
    use serde_json::json;

    use super::*;
    use crate::quota::http::QuotaHttp;
    use crate::quota::registry::QuotaCredential;

    /// Canned `/key` payload: a $10 per-key cap with $4.20 left and no concrete reset.
    const KEY_WITH_LIMIT: &str = r#"{
        "data": {
            "label": "sk-or-v1-...redacted",
            "limit": 10,
            "limit_remaining": 4.2,
            "limit_reset": "weekly",
            "usage": 5.8,
            "usage_daily": 0.4,
            "usage_weekly": 5.8,
            "usage_monthly": 5.8,
            "is_free_tier": false,
            "byok_usage": 1.25,
            "include_byok_in_limit": true
        }
    }"#;

    /// PAYG account with no per-key cap: `limit` is `null`, the pool is authoritative.
    const KEY_NO_LIMIT: &str = r#"{
        "data": {
            "limit": null,
            "limit_remaining": null,
            "limit_reset": null,
            "usage": 12.5,
            "usage_daily": 0,
            "usage_weekly": 3.5,
            "usage_monthly": 12.5,
            "is_free_tier": false,
            "byok_usage": null,
            "include_byok_in_limit": false
        }
    }"#;

    /// Only `limit_remaining` is present — the missing-axis case.
    const KEY_REMAINING_ONLY: &str = r#"{
        "data": {
            "limit": null,
            "limit_remaining": 12.5,
            "limit_reset": null,
            "usage": 0
        }
    }"#;

    const CREDITS: &str = r#"{"data": {"total_credits": 100, "total_usage": 12.5}}"#;
    const CREDITS_EMPTY: &str = r#"{"data": {"total_credits": 0, "total_usage": 0}}"#;
    const UNAUTHORIZED: &str =
        r#"{"error": {"message": "No auth credentials found", "code": 401}}"#;

    /// A canned transport that records every request it was handed. Routes match on exact
    /// URL and are replayed on every hit, so a test can assert ordering and headers.
    #[derive(Debug)]
    struct FakeHttp {
        routes: Vec<(&'static str, QuotaResponse)>,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(routes: Vec<(&'static str, u16, &str)>) -> Arc<Self> {
            Arc::new(Self {
                routes: routes
                    .into_iter()
                    .map(|(url, status, body)| {
                        (
                            url,
                            QuotaResponse {
                                status,
                                headers: Vec::new(),
                                body: body.as_bytes().to_vec().into(),
                            },
                        )
                    })
                    .collect(),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("fake http mutex").clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen
                .lock()
                .expect("fake http mutex")
                .push(request.clone());
            self.routes
                .iter()
                .find(|(url, _)| *url == request.url)
                .map(|(_, response)| response.clone())
                .ok_or_else(|| Error::NotFound(format!("no canned response for {}", request.url)))
        }
    }

    fn context(http: Arc<FakeHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new(OpenRouter::ID, Dialect::OpenAi, http)
            .with_credential(QuotaCredential::new("sk-or-v1-test"))
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    /// The recorded `Authorization` header must be exactly a bearer of the credential.
    fn assert_bearer(request: &QuotaRequest) {
        let header = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.clone());
        assert_eq!(header.as_deref(), Some("Bearer sk-or-v1-test"));
    }

    #[tokio::test]
    async fn key_limit_path_uses_the_per_key_cap() {
        let http = FakeHttp::new(vec![
            (KEY_URL, 200, KEY_WITH_LIMIT),
            (CREDITS_URL, 200, CREDITS),
        ]);
        let quota = OpenRouter::new()
            .fetch(&context(http.clone()))
            .await
            .expect("key-limit quota");

        assert_close(quota.total, 10.0);
        assert_close(quota.used, 5.8);
        assert_close(quota.remaining, 4.2);
        assert_close(quota.remaining_percentage, 42.0);
        assert!(!quota.unlimited);
        assert!(quota.fraction_reported());
        // `limit_reset: "weekly"` is not an instant, so no reset is invented.
        assert_eq!(quota.reset_at, None);
        assert_eq!(quota.source.as_deref(), Some(OpenRouter::ID));
        assert!(quota
            .display_name
            .as_deref()
            .is_some_and(|name| name.contains("USD")));

        let requests = http.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, Method::GET);
        assert_eq!(requests[0].url, KEY_URL);
        assert_bearer(&requests[0]);
        assert_eq!(requests[1].url, CREDITS_URL);
        assert_bearer(&requests[1]);
        assert_eq!(
            requests[0]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("accept"))
                .map(|(_, value)| value.as_str()),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn credit_pool_path_used_when_limit_is_null() {
        let http = FakeHttp::new(vec![
            (KEY_URL, 200, KEY_NO_LIMIT),
            (CREDITS_URL, 200, CREDITS),
        ]);
        let quota = OpenRouter::new()
            .fetch(&context(http.clone()))
            .await
            .expect("credit-pool quota");

        assert_close(quota.total, 100.0);
        assert_close(quota.used, 12.5);
        assert_close(quota.remaining, 87.5);
        assert_close(quota.remaining_percentage, 87.5);
        assert!(!quota.unlimited);
        assert_eq!(quota.reset_at, None);
        // The upstream counters survive as details without becoming windows.
        assert!(quota.details.iter().any(|d| d.name == "usage_daily"));
        assert!(quota
            .details
            .iter()
            .any(|d| d.name == "usage_weekly" && d.used == 3.5));
    }

    #[tokio::test]
    async fn empty_pool_with_no_key_limit_is_unlimited_not_exhausted() {
        let http = FakeHttp::new(vec![
            (KEY_URL, 200, KEY_NO_LIMIT),
            (CREDITS_URL, 200, CREDITS_EMPTY),
        ]);
        let quota = OpenRouter::new()
            .fetch(&context(http.clone()))
            .await
            .expect("unlimited quota");

        assert!(quota.unlimited);
        assert_close(quota.total, 0.0);
        assert_close(quota.used, 0.0);
        assert!(!quota.fraction_reported());
        assert_close(quota.remaining_percentage, 0.0);
    }

    #[tokio::test]
    async fn missing_pool_axis_falls_back_to_limit_remaining() {
        // `limit` is null but `limit_remaining` is present: upstream treats the remaining
        // value as the balance and has no pool to divide by, so the bar is that balance.
        let http = FakeHttp::new(vec![
            (KEY_URL, 200, KEY_REMAINING_ONLY),
            (CREDITS_URL, 404, "{}"),
        ]);
        let quota = OpenRouter::new()
            .fetch(&context(http.clone()))
            .await
            .expect("balance-only quota");

        assert_close(quota.total, 12.5);
        assert_close(quota.remaining, 12.5);
        assert_close(quota.used, 0.0);
        assert!(!quota.unlimited);
        assert!(quota.fraction_reported());
    }

    #[tokio::test]
    async fn unauthorized_key_is_an_error() {
        let http = FakeHttp::new(vec![
            (KEY_URL, 401, UNAUTHORIZED),
            (CREDITS_URL, 401, UNAUTHORIZED),
        ]);
        let error = OpenRouter::new()
            .fetch(&context(http.clone()))
            .await
            .expect_err("401 must not fabricate a quota");

        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(
            !format!("{error:?}").contains("sk-or-v1-test"),
            "the credential leaked into the error: {error:?}"
        );
    }

    #[tokio::test]
    async fn both_payloads_unusable_is_an_error() {
        let http = FakeHttp::new(vec![
            (KEY_URL, 500, "upstream exploded"),
            (CREDITS_URL, 500, "upstream exploded"),
        ]);
        let error = OpenRouter::new()
            .fetch(&context(http.clone()))
            .await
            .expect_err("two unusable payloads must not fabricate a quota");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn missing_credential_is_a_config_error() {
        let http = FakeHttp::new(vec![(KEY_URL, 200, KEY_WITH_LIMIT)]);
        let ctx = QuotaFetchContext::new(OpenRouter::ID, Dialect::OpenAi, http);
        let error = OpenRouter::new()
            .fetch(&ctx)
            .await
            .expect_err("no credential must fail before any request");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
    }

    #[test]
    fn supports_only_the_openrouter_provider_id() {
        let account = QuotaAccount::new();
        assert!(OpenRouter::new().supports(Dialect::OpenAi, "openrouter", &account));
        assert!(!OpenRouter::new().supports(Dialect::OpenAi, "deepseek", &account));
        assert!(!OpenRouter::new().supports(Dialect::OpenAi, "openrouter-free", &account));
    }

    #[test]
    fn parses_non_numeric_shapes_as_absent() {
        let body = json!({"data": {"limit": "not a number", "usage": "3.5"}});
        let parsed = parse_key_response(&body).expect("recognisable data object");
        assert_eq!(parsed.limit, None);
        assert_close(parsed.usage, 3.5);
        assert!(parse_key_response(&json!({"data": {}})).is_none());
    }
}
