//! Moonshot Open Platform prepaid balance.
//!
//! Ported from OmniRoute's `open-sse/services/moonshotQuotaFetcher.ts` and
//! `open-sse/services/usage/moonshotOpenPlatform.ts` (MIT, © 2026 diegosouzapw).
//!
//! # Why this one is not a window
//!
//! Moonshot Open Platform is **prepaid**: it does not report a 5-hour or weekly window at all, only
//! a wallet balance. Passing that through [`crate::quota::window::WindowFamily`] would invent a
//! duration the upstream never stated, so the balance is returned with
//! [`UsageQuota::unlimited`] and the balance in `remaining` — no window, no percentage.
//!
//! # The host rule
//!
//! The origin comes from the account's **configured base URL**, and there is deliberately **no
//! `.ai` fallback for a `.cn` key** (or vice versa): a prepaid key belongs to exactly one platform,
//! and guessing would send a `.cn` credential to the international host. Only `api.moonshot.cn` and
//! `api.moonshot.ai` are recognised; anything else is an `Err` naming the problem rather than a
//! request to the wrong wallet.
//!
//! # Response envelope
//!
//! ```json
//! { "code": 0, "data": { "available_balance": 12.5, "voucher_balance": 2.5, "cash_balance": 10 } }
//! ```
//!
//! A non-zero `code` arrives inside HTTP 200, so it is checked explicitly: an envelope-level
//! failure is an [`Error::InvalidState`], never a `0.00` balance.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::UsageQuota;

/// Fetcher id, stamped onto [`UsageQuota::source`].
pub const ID: &str = "moonshot";

/// The only hosts a Moonshot Open Platform prepaid key may be sent to.
pub const OPEN_PLATFORM_HOSTS: [&str; 2] = ["api.moonshot.cn", "api.moonshot.ai"];

/// The Moonshot Open Platform balance fetcher.
#[derive(Debug, Clone, Copy, Default)]
pub struct Moonshot;

impl Moonshot {
    /// A fetcher with no configuration of its own.
    pub fn new() -> Self {
        Self
    }
}

/// Reduce a configured base URL to `scheme://host[:port]` when it names a recognised Moonshot
/// Open Platform host.
///
/// `None` for a missing URL, a URL without an http(s) scheme, an unparsable URL, or any other
/// host — the caller turns that into an error rather than a fallback, because falling back to the
/// other region would send the credential to the wrong platform.
pub fn parse_origin(base_url: Option<&str>) -> Option<String> {
    let raw = base_url?.trim();
    if raw.is_empty() {
        return None;
    }
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    // The authority ends at the first `/`, `?` or `#`.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() {
        return None;
    }
    // Strip any userinfo, then split off an optional port.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (authority, None),
    };
    let host = host.to_ascii_lowercase();
    if !OPEN_PLATFORM_HOSTS.contains(&host.as_str()) {
        return None;
    }
    match port {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// The declared base URL on an account, when the operator put one there.
fn declared_base_url(account: &QuotaAccount) -> Option<&str> {
    for key in ["baseUrl", "base_url"] {
        let Some(Value::String(url)) = account.extra.get(key) else {
            continue;
        };
        if !url.trim().is_empty() {
            return Some(url.as_str());
        }
    }
    None
}

/// Coerce a JSON number or numeric string to `f64`.
fn to_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Read the balance out of a `/v1/users/me/balance` envelope.
///
/// Pure, so it is tested against captured JSON with no socket. `code != 0` is an error: the
/// envelope failed even though the HTTP status was 200.
pub fn parse_balance(body: &Value) -> Result<f64> {
    let code = body.get("code").and_then(|code| match code {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    });
    if let Some(code) = code {
        if code != 0 {
            let message = body
                .get("message")
                .or_else(|| body.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(Error::InvalidState(format!(
                "moonshot balance envelope failed with code {code}: {message}"
            )));
        }
    }

    let data = body
        .get("data")
        .ok_or_else(|| Error::NotFound("moonshot balance response has no `data` object".into()))?;
    let available = to_number(data.get("available_balance"))
        .or_else(|| to_number(data.get("availableBalance")))
        .ok_or_else(|| {
            Error::NotFound(
                "moonshot balance response has no numeric `available_balance`; refusing to report 0"
                    .into(),
            )
        })?;
    if !available.is_finite() {
        return Err(Error::InvalidState(
            "moonshot balance response has a non-finite `available_balance`".into(),
        ));
    }
    Ok(available)
}

/// Turn a balance into the no-cap quota shape: no window, no fraction, balance as `remaining`.
pub fn balance_quota(available: f64) -> UsageQuota {
    let mut quota = UsageQuota::unlimited();
    quota.remaining = available;
    quota
        .with_source(ID)
        .with_display_name("Moonshot Open Platform")
}

#[async_trait]
impl QuotaFetcher for Moonshot {
    fn id(&self) -> &'static str {
        ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, account: &QuotaAccount) -> bool {
        match provider_id {
            "moonshot" | "moonshot-ai" => true,
            // A `kimi` provider is the Open Platform only when its declared base URL says so;
            // `api.kimi.com/coding` is the Coding Plan and belongs to the Kimi fetcher.
            "kimi" => parse_origin(declared_base_url(account)).is_some(),
            _ => false,
        }
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        // `supports` selects on the account's declared base URL (a `kimi` account is the Open
        // Platform only when that URL is a Moonshot host), so the same source must be preferred
        // here; `ctx.base_url` is only a fallback for providers that carry it at provider level.
        let base_url = declared_base_url(&ctx.account).or(ctx.base_url.as_deref());
        let origin = parse_origin(base_url).ok_or_else(|| {
            Error::Config(format!(
                "provider {:?} needs an account base URL on one of {OPEN_PLATFORM_HOSTS:?} to read the \
                 Moonshot prepaid balance; there is no cross-region fallback",
                ctx.provider_id
            ))
        })?;

        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "provider {:?} has no credential; the Moonshot balance endpoint needs a bearer key",
                    ctx.provider_id
                ))
            })?;

        let url = format!("{origin}/v1/users/me/balance");
        let request = QuotaRequest::get(url)
            .bearer(credential.expose())
            .header("Accept", "application/json");
        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "moonshot balance endpoint returned HTTP {}",
                response.status
            )));
        }
        Ok(balance_quota(parse_balance(&response.json()?)?))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaRequest, QuotaResponse};

    const BALANCE_OK: &str = r#"{
        "code": 0,
        "data": {
            "available_balance": 12.5,
            "voucher_balance": 2.5,
            "cash_balance": 10
        }
    }"#;

    const BALANCE_STRING: &str = r#"{
        "code": 0,
        "data": { "available_balance": "0.00", "cash_balance": "0.00" }
    }"#;

    const BALANCE_CODE_NONZERO: &str = r#"{
        "code": 401,
        "message": "invalid api key",
        "data": null
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

    fn context(
        http: Arc<FakeHttp>,
        base_url: Option<&str>,
        credential: Option<&str>,
    ) -> QuotaFetchContext {
        let mut ctx = QuotaFetchContext::new("moonshot", Dialect::OpenAi, http);
        if let Some(base_url) = base_url {
            ctx = ctx.with_base_url(base_url);
        }
        if let Some(credential) = credential {
            ctx = ctx.with_credential(credential);
        }
        ctx
    }

    #[test]
    fn origin_recognises_both_platform_hosts_and_rejects_everything_else() {
        assert_eq!(
            parse_origin(Some("https://api.moonshot.cn/v1")),
            Some("https://api.moonshot.cn".to_owned())
        );
        assert_eq!(
            parse_origin(Some("https://API.Moonshot.AI/v1/")),
            Some("https://api.moonshot.ai".to_owned())
        );
        assert_eq!(
            parse_origin(Some("http://api.moonshot.cn:8080/v1")),
            Some("http://api.moonshot.cn:8080".to_owned())
        );
        assert_eq!(parse_origin(Some("https://api.kimi.com/coding/v1")), None);
        assert_eq!(parse_origin(Some("https://api.deepseek.com/v1")), None);
        assert_eq!(parse_origin(Some("api.moonshot.cn")), None);
        assert_eq!(parse_origin(Some("")), None);
        assert_eq!(parse_origin(None), None);
    }

    #[test]
    fn balance_is_the_remaining_side_of_a_no_cap_quota() {
        let body: Value = serde_json::from_str(BALANCE_OK).expect("canned json parses");
        let quota = balance_quota(parse_balance(&body).expect("balance parses"));
        assert!(quota.unlimited, "a prepaid balance has no applicable cap");
        assert!(!quota.fraction_reported);
        assert_eq!(quota.remaining, 12.5);
        assert_eq!(quota.used, 0.0);
        assert_eq!(quota.total, 0.0);
        assert_eq!(quota.reset_at, None);
        assert_eq!(quota.source.as_deref(), Some(ID));
    }

    #[test]
    fn numeric_string_balance_is_accepted() {
        let body: Value = serde_json::from_str(BALANCE_STRING).expect("canned json parses");
        let available = parse_balance(&body).expect("balance parses");
        assert_eq!(available, 0.0);
    }

    #[test]
    fn envelope_code_is_an_error_even_on_http_200() {
        let body: Value = serde_json::from_str(BALANCE_CODE_NONZERO).expect("canned json parses");
        let error = parse_balance(&body).expect_err("code 401 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    }

    #[test]
    fn missing_available_balance_is_an_error_not_zero() {
        let body = serde_json::json!({ "code": 0, "data": { "cash_balance": 3.0 } });
        let error = parse_balance(&body).expect_err("no available_balance must fail");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn fetch_reads_the_configured_origin_and_sends_a_bearer_key() {
        let http = Arc::new(FakeHttp::new(vec![json_response(200, BALANCE_OK)]));
        let quota = Moonshot::new()
            .fetch(&context(
                Arc::clone(&http),
                Some("https://api.moonshot.cn/v1"),
                Some("sk-secret"),
            ))
            .await
            .expect("fetch succeeds");

        assert_eq!(quota.remaining, 12.5);
        let seen = http.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, http::Method::GET);
        assert_eq!(seen[0].url, "https://api.moonshot.cn/v1/users/me/balance");
        let auth = seen[0]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("Bearer sk-secret"));
    }

    #[tokio::test]
    async fn fetch_errors_when_the_base_url_is_missing_or_foreign() {
        let http = Arc::new(FakeHttp::new(Vec::new()));
        for base_url in [None, Some("https://api.deepseek.com/v1")] {
            let error = Moonshot::new()
                .fetch(&context(Arc::clone(&http), base_url, Some("sk-secret")))
                .await
                .expect_err("unrecognised origin must fail");
            assert!(matches!(error, Error::Config(_)), "got {error:?}");
        }
        assert!(
            http.seen().is_empty(),
            "no request may be sent to an unknown origin"
        );
    }

    #[tokio::test]
    async fn http_failure_is_an_error() {
        let http = Arc::new(FakeHttp::new(vec![json_response(403, "forbidden")]));
        let error = Moonshot::new()
            .fetch(&context(
                Arc::clone(&http),
                Some("https://api.moonshot.ai/v1"),
                Some("k"),
            ))
            .await
            .expect_err("403 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    }

    #[test]
    fn supports_claims_moonshot_ids_and_kimi_only_on_a_moonshot_host() {
        let fetcher = Moonshot::new();
        assert!(fetcher.supports(Dialect::OpenAi, "moonshot", &QuotaAccount::new()));
        assert!(fetcher.supports(Dialect::OpenAi, "moonshot-ai", &QuotaAccount::new()));
        assert!(!fetcher.supports(Dialect::OpenAi, "kimi", &QuotaAccount::new()));
        let kimi_on_moonshot = QuotaAccount::new().with_extra(
            "baseUrl",
            Value::String("https://api.moonshot.cn/v1".into()),
        );
        assert!(fetcher.supports(Dialect::OpenAi, "kimi", &kimi_on_moonshot));
        let coding = QuotaAccount::new().with_extra(
            "baseUrl",
            Value::String("https://api.kimi.com/coding/v1".into()),
        );
        assert!(!fetcher.supports(Dialect::OpenAi, "kimi", &coding));
        assert!(!fetcher.supports(Dialect::OpenAi, "openrouter", &QuotaAccount::new()));
    }
}
