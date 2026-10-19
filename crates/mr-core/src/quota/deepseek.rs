//! DeepSeek prepaid balance.
//!
//! Ported from OmniRoute's `open-sse/services/usage/deepseek.ts` (`getDeepseekUsage`) and
//! `open-sse/services/deepseekQuotaFetcher.ts` (`fetchDeepseekQuota`,
//! `parseDeepseekQuotaResponse`, `parseAllBalanceInfos`) — MIT, © 2026 diegosouzapw.
//!
//! `GET https://api.deepseek.com/user/balance` answers:
//!
//! ```json
//! {
//!   "is_available": true,
//!   "balance_infos": [
//!     { "currency": "USD", "total_balance": "10.00",
//!       "granted_balance": "0.00", "topped_up_balance": "10.00" }
//!   ]
//! }
//! ```
//!
//! The money values are **decimal strings**, and a parse failure is deliberately **not** a
//! silent `0` here (upstream's `toNumber(value, 0)` would be): a `0` balance renders as "out
//! of credit", which is a real operational claim, so an unreadable amount is an
//! [`Error::NotFound`] instead.
//!
//! # Why this is `unlimited`, not `0/0`
//!
//! DeepSeek is a **prepaid balance, not a windowed quota**: there is no cap the balance is
//! a fraction of, and no reset. The `used: 0 / total: 0` shape is the deliberate
//! "no cap applies" sentinel, which is exactly what [`UsageQuota::unlimited`] records
//! (`unlimited: true`, `fraction_reported: false`) — so this file uses that constructor and
//! **not** `from_usage`, whose normalisation would otherwise turn a real "no cap" into an
//! exhausted-looking `0/0`. For the same reason no [`crate::quota::window::WindowFamily`]
//! is ever derived here: there is no duration to classify.
//!
//! The ported [`UsageQuota`] has no per-entry map and no `currency` field (OmniRoute's type
//! carries both), so each upstream `balance_infos` entry becomes one `credits_<ccy>`
//! [`QuotaDetail`] row — the only numeric carrier the Rust shape has — and the "which
//! currency" answer lives in that row's name.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::{QuotaRequest, QuotaResponse};
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, UsageQuota};

/// The balance endpoint, from `DEEPSEEK_CONFIG.baseUrl + balancePath`.
pub const BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// The fetcher id, also stamped onto the `source` field of what it returns.
pub const SOURCE: &str = DeepSeek::ID;

/// DeepSeek's prepaid balance.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeepSeek;

impl DeepSeek {
    /// The fetcher id, and the provider id this fetcher claims.
    pub const ID: &str = "deepseek";

    /// A stateless fetcher; `Arc`-shared by the registry.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for DeepSeek {
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
                Error::Config("deepseek quota requires an account API key".to_owned())
            })?;

        let request = QuotaRequest::get(BALANCE_URL)
            .bearer(credential.expose())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");
        let response: QuotaResponse = ctx.http.request(request).await?;

        if matches!(response.status, 401 | 403) {
            return Err(Error::Config(format!(
                "deepseek rejected the account credential (HTTP {})",
                response.status
            )));
        }
        if !response.is_success() {
            return Err(Error::NotFound(format!(
                "deepseek balance API error (HTTP {})",
                response.status
            )));
        }

        let body = response.json()?;
        let balances = parse_balances(&body)?;
        Ok(build_quota(&body, &balances))
    }
}

/// One parsed `balance_infos` entry.
#[derive(Debug, Clone, PartialEq)]
struct BalanceInfo {
    currency: String,
    total: f64,
    granted: f64,
    topped_up: f64,
}

/// Parse every `balance_infos` entry, failing rather than zeroing an unreadable amount.
fn parse_balances(body: &Value) -> Result<Vec<BalanceInfo>> {
    let infos = body
        .get("balance_infos")
        .and_then(Value::as_array)
        .filter(|infos| !infos.is_empty())
        .ok_or_else(|| Error::NotFound("deepseek returned no balance_infos entries".to_owned()))?;

    let mut balances = Vec::with_capacity(infos.len());
    for entry in infos {
        let currency = entry
            .get("currency")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|currency| !currency.is_empty())
            .ok_or_else(|| {
                Error::NotFound("deepseek balance_infos entry has no currency".to_owned())
            })?;
        balances.push(BalanceInfo {
            currency: currency.to_owned(),
            total: money(entry, "total_balance", currency)?,
            // `granted_balance`/`topped_up_balance` are optional in the payload; absent
            // means "not reported", which the detail rows leave at zero.
            granted: money(entry, "granted_balance", currency).unwrap_or(0.0),
            topped_up: money(entry, "topped_up_balance", currency).unwrap_or(0.0),
        });
    }
    Ok(balances)
}

/// One decimal-string money field. A missing `granted_balance`/`topped_up_balance` is fine
/// (the caller defaults it); a *present but unparsable* one is not.
fn money(entry: &Value, field: &str, currency: &str) -> Result<f64> {
    match entry.get(field) {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::Number(number)) => number
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| unreadable(field, currency)),
        Some(Value::String(text)) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .ok_or_else(|| unreadable(field, currency)),
        Some(_) => Err(unreadable(field, currency)),
    }
}

fn unreadable(field: &str, currency: &str) -> Error {
    // The value itself is never echoed: a balance is not a secret, but the rule keeps the
    // error text stable and free of operator data.
    Error::NotFound(format!(
        "deepseek balance field {field} is not a number for {currency}"
    ))
}

/// `parseDeepseekQuotaResponse` + `getDeepseekUsage`, flattened onto one [`UsageQuota`].
fn build_quota(body: &Value, balances: &[BalanceInfo]) -> UsageQuota {
    // `is_available ?? isAvailable`, and only a literal `false` is an insufficiency claim:
    // an absent field is not evidence of a problem.
    let available = body
        .get("is_available")
        .or_else(|| body.get("isAvailable"))
        .and_then(Value::as_bool);
    let has_positive_balance = balances.iter().any(|balance| balance.total > 0.0);
    let limit_reached = available == Some(false) || !has_positive_balance;

    let mut quota = UsageQuota::unlimited()
        .with_source(SOURCE)
        .with_display_name(if limit_reached {
            // Upstream's plan label for this exact state.
            "DeepSeek (Insufficient Balance)"
        } else {
            "DeepSeek"
        });

    for balance in balances {
        let currency = balance.currency.to_lowercase();
        quota = quota
            .with_detail(QuotaDetail {
                // The per-currency key upstream uses, e.g. `credits_usd`.
                name: format!("credits_{currency}"),
                used: balance.total,
            })
            .with_detail(QuotaDetail {
                name: format!("credits_{currency}_granted"),
                used: balance.granted,
            })
            .with_detail(QuotaDetail {
                name: format!("credits_{currency}_topped_up"),
                used: balance.topped_up,
            });
    }
    quota
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use http::Method;

    use super::*;
    use crate::quota::http::QuotaHttp;
    use crate::quota::registry::QuotaCredential;

    /// Real-shaped captured body: decimal strings, USD and CNY.
    const AVAILABLE_TWO_CURRENCIES: &str = r#"{
        "is_available": true,
        "balance_infos": [
            {
                "currency": "USD",
                "total_balance": "10.00",
                "granted_balance": "0.00",
                "topped_up_balance": "10.00"
            },
            {
                "currency": "CNY",
                "total_balance": "72.50",
                "granted_balance": "1.50",
                "topped_up_balance": "71.00"
            }
        ]
    }"#;

    /// `isAvailable` camelCase spelling, balance present but zero.
    const ZERO_BALANCE: &str = r#"{
        "isAvailable": true,
        "balance_infos": [
            { "currency": "USD", "total_balance": "0.00",
              "granted_balance": "0.00", "topped_up_balance": "0.00" }
        ]
    }"#;

    /// `is_available: false` with money still on the account.
    const UNAVAILABLE: &str = r#"{
        "is_available": false,
        "balance_infos": [
            { "currency": "USD", "total_balance": "3.25",
              "granted_balance": "0.00", "topped_up_balance": "3.25" }
        ]
    }"#;

    /// A malformed money string (`1O` with a capital letter O).
    const MALFORMED_BALANCE: &str = r#"{
        "is_available": true,
        "balance_infos": [
            { "currency": "USD", "total_balance": "1O.00",
              "granted_balance": "0.00", "topped_up_balance": "0.00" }
        ]
    }"#;

    const UNAUTHORIZED: &str = r#"{"error": {"message": "Authentication Fails"}}"#;

    #[derive(Debug)]
    struct FakeHttp {
        status: u16,
        body: &'static str,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(status: u16, body: &'static str) -> Arc<Self> {
            Arc::new(Self {
                status,
                body,
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
            Ok(QuotaResponse {
                status: self.status,
                headers: Vec::new(),
                body: self.body.as_bytes().to_vec().into(),
            })
        }
    }

    fn context(http: Arc<FakeHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new(DeepSeek::ID, Dialect::OpenAi, http)
            .with_credential(QuotaCredential::new("sk-deepseek-test"))
    }

    fn detail<'a>(quota: &'a UsageQuota, name: &str) -> Option<&'a QuotaDetail> {
        quota.details.iter().find(|detail| detail.name == name)
    }

    #[tokio::test]
    async fn decimal_string_money_is_parsed_per_currency() {
        let http = FakeHttp::new(200, AVAILABLE_TWO_CURRENCIES);
        let quota = DeepSeek::new()
            .fetch(&context(http.clone()))
            .await
            .expect("balance quota");

        assert!(quota.unlimited);
        assert_close(detail(&quota, "credits_usd").expect("usd row").used, 10.0);
        assert_close(detail(&quota, "credits_cny").expect("cny row").used, 72.5);
        assert_close(
            detail(&quota, "credits_usd_topped_up")
                .expect("top-up row")
                .used,
            10.0,
        );
        assert_close(
            detail(&quota, "credits_cny_granted")
                .expect("granted row")
                .used,
            1.5,
        );
        assert_eq!(quota.display_name.as_deref(), Some("DeepSeek"));

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, Method::GET);
        assert_eq!(requests[0].url, BALANCE_URL);
        assert_eq!(
            requests[0]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.as_str()),
            Some("Bearer sk-deepseek-test")
        );
        assert_eq!(
            requests[0]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.as_str()),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn balance_renders_as_unlimited_not_exhausted_zero() {
        let http = FakeHttp::new(200, AVAILABLE_TWO_CURRENCIES);
        let quota = DeepSeek::new()
            .fetch(&context(http.clone()))
            .await
            .expect("balance quota");

        // The prepaid balance is *not* a windowed quota: `0/0` is the no-cap sentinel.
        assert!(quota.unlimited);
        assert_eq!(quota.total, 0.0);
        assert_eq!(quota.used, 0.0);
        assert!(!quota.fraction_reported());
        assert_eq!(quota.reset_at, None);
        assert!(!quota.is_exhausted());
    }

    #[tokio::test]
    async fn zero_balance_and_unavailable_are_surfaced() {
        let zero = FakeHttp::new(200, ZERO_BALANCE);
        let quota = DeepSeek::new()
            .fetch(&context(zero.clone()))
            .await
            .expect("zero-balance quota");
        assert_eq!(
            quota.display_name.as_deref(),
            Some("DeepSeek (Insufficient Balance)")
        );

        let unavailable = FakeHttp::new(200, UNAVAILABLE);
        let quota = DeepSeek::new()
            .fetch(&context(unavailable.clone()))
            .await
            .expect("unavailable quota");
        assert_eq!(
            quota.display_name.as_deref(),
            Some("DeepSeek (Insufficient Balance)")
        );
        assert_close(detail(&quota, "credits_usd").expect("usd row").used, 3.25);
    }

    #[tokio::test]
    async fn malformed_balance_string_is_an_error_not_zero() {
        let http = FakeHttp::new(200, MALFORMED_BALANCE);
        let error = DeepSeek::new()
            .fetch(&context(http.clone()))
            .await
            .expect_err("an unreadable amount must not become 0");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert!(format!("{error:?}").contains("total_balance"));
    }

    #[tokio::test]
    async fn unauthorized_is_an_error() {
        let http = FakeHttp::new(401, UNAUTHORIZED);
        let error = DeepSeek::new()
            .fetch(&context(http.clone()))
            .await
            .expect_err("401 must not fabricate a quota");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn empty_balance_list_is_an_error() {
        let http = FakeHttp::new(200, r#"{"is_available": true, "balance_infos": []}"#);
        let error = DeepSeek::new()
            .fetch(&context(http.clone()))
            .await
            .expect_err("no balances must not fabricate a 0/0 quota");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    }

    #[test]
    fn supports_only_the_deepseek_provider_id() {
        let account = QuotaAccount::new();
        assert!(DeepSeek::new().supports(Dialect::OpenAi, "deepseek", &account));
        assert!(!DeepSeek::new().supports(Dialect::OpenAi, "openrouter", &account));
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }
}
