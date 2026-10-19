//! Qwen Cloud / Alibaba Model Studio personal Token Plan usage.
//!
//! Ported from OmniRoute's `open-sse/services/usage/qwen-token-plan.ts` and
//! `open-sse/services/qwenTokenPlanQuotaFetcher.ts` (MIT, © 2026 diegosouzapw).
//!
//! # The credential is a console cookie, not an inference key
//!
//! The personal Token Plan has no usage API authenticated by an inference API key. Its quota is
//! read from the console's gateway with the browser session cookie, optionally accompanied by
//! `sec_token`; this fetcher reads those values from provider-specific account metadata and never
//! sends the account's regular API credential. If no cookie is configured it fails closed before
//! making a request.
//!
//! # Gateway envelope and windows
//!
//! `POST /data/api.json` wraps its result as `{code: 200, data: {DataV2: {data: {code: "SUCCESS",
//! success: true, data: ...}}}}`. The usage data contains optional `per5HourPercentage` and
//! `per1WeekPercentage` fractions plus epoch-millisecond reset times. A console can temporarily
//! omit one of those fields, so only present windows are returned and no missing window is invented.

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::{QuotaRequest, QuotaResponse};
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, UsageQuota};

/// QwenCloud host used by personal Token Plan accounts.
pub const QWENCLOUD_GATEWAY: &str = "https://cs-data.qwencloud.com";
/// Alibaba Model Studio's international console gateway.
pub const ALIYUN_GATEWAY: &str = "https://bailian-singapore-cs.alibabacloud.com";
/// The console gateway action from OmniRoute's token-plan fetcher.
pub const GATEWAY_ACTION: &str = "IntlBroadScopeAspnGateway";
/// Fetcher/source id and provider id for personal Qwen Token Plan accounts.
pub const ID: &str = "qwen-cloud-token-plan";

/// Cookie and optional CSRF token are intentionally provider-specific, not the inference API key.
pub const COOKIE_FIELDS: [&str; 3] = ["qwenCloudCookie", "alibabaConsoleCookie", "cookie"];
/// Explicit sec-token fields accepted from the console account metadata.
pub const SEC_TOKEN_FIELDS: [&str; 2] = ["qwenCloudSecToken", "alibabaConsoleSecToken"];

/// The personal Qwen Token Plan console quota reader.
#[derive(Debug, Clone, Copy, Default)]
pub struct Qwen;

impl Qwen {
    /// A stateless fetcher; credentials and console selection live on each account.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for Qwen {
    fn id(&self) -> &'static str {
        ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        provider_id == ID
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let cookie = first_account_string(&ctx.account, &COOKIE_FIELDS).ok_or_else(|| {
            Error::Config("qwen token-plan quota requires a console session cookie".into())
        })?;
        let sec_token = first_account_string(&ctx.account, &SEC_TOKEN_FIELDS).unwrap_or_default();
        let aliyun = ctx.account.extra.get("consoleSite").and_then(Value::as_str) == Some("ALIYUN")
            || ctx.provider_id == "bailian-coding-plan"
            || ctx.provider_id == "alibaba"
            || ctx.provider_id == "alibaba-cn"
            || cookie.contains("login_aliyunid_ticket=");
        let (gateway, site, domain, origin) = if aliyun {
            (
                ALIYUN_GATEWAY,
                "ALIYUN",
                "modelstudio.console.alibabacloud.com",
                "https://modelstudio.console.alibabacloud.com",
            )
        } else {
            (
                QWENCLOUD_GATEWAY,
                "QWENCLOUD",
                "home.qwencloud.com",
                "https://home.qwencloud.com",
            )
        };

        let url = gateway_url(gateway, "usage");
        let body = gateway_form("usage", &sec_token, site, domain);
        let mut request = QuotaRequest::post_json(url, body)
            .header("Cookie", cookie)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .header("Origin", origin)
            .header("Referer", format!("{origin}/"));
        // The gateway body is form encoded, not JSON; replace the convenience constructor's media
        // type rather than send a misleading duplicate Content-Type field.
        request
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        request.headers.push((
            "Content-Type".into(),
            "application/x-www-form-urlencoded".into(),
        ));
        request.timeout = Some(std::time::Duration::from_secs(8));
        let response = ctx.http.request(request).await?;
        if matches!(response.status, 401 | 403) {
            return Err(Error::Config(
                "qwen token-plan console session was unauthorized".into(),
            ));
        }
        let payload = parse_gateway_response(&response)?;
        let mut windows = parse_usage_windows(&payload);
        if windows.is_empty() {
            return Err(Error::NotFound(
                "qwen token-plan response contained no usage windows".into(),
            ));
        }
        // OmniRoute reports the maximum (most constrained) used percentage across available
        // windows. Preserve both source percentages as detail rows because the normalized shape
        // itself can only carry one primary fraction.
        windows.sort_by(|a, b| b.1.used.total_cmp(&a.1.used));
        let (primary_name, primary) = windows.first().expect("non-empty usage windows");
        let primary_name = primary_name.clone();
        let mut quota = primary
            .clone()
            .with_source(ID)
            .with_display_name("Qwen Token Plan");
        for (name, window) in windows {
            quota.details.push(QuotaDetail {
                name,
                used: window.used,
            });
        }
        quota.display_name = Some(format!("Qwen Token Plan ({primary_name})"));
        Ok(quota)
    }
}

fn first_account_string(account: &QuotaAccount, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        account
            .extra
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

/// Build the encoded URL, with each endpoint represented by the upstream's `api` query parameter.
pub fn gateway_url(gateway: &str, endpoint: &str) -> String {
    let api = format!("zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/{endpoint}");
    format!(
        "{gateway}/data/api.json?product=sfm_bailian&action={GATEWAY_ACTION}&api={}",
        percent_encode(&api)
    )
}

/// Build the POST form payload used by the console gateway. Values are percent-encoded as
/// `URLSearchParams` would encode them, keeping the captured request derivable without live access.
pub fn gateway_form(endpoint: &str, sec_token: &str, site: &str, domain: &str) -> String {
    let api = format!("zeldaHttp.apikeyMgr./tokenplan/personal/api/v2/{endpoint}");
    let params = json!({
        "Api": api,
        "V": "1.0",
        "Data": {
            "commodityCode": "sfm_tokenplansolo_public_intl",
            "cornerstoneParam": {
                "console": "ONE_CONSOLE",
                "consoleSite": site,
                "domain": domain,
                "productCode": "p_efm",
                "protocol": "V2",
                "xsp_lang": "en-US"
            }
        }
    })
    .to_string();
    [
        ("product", "sfm_bailian".to_owned()),
        ("action", GATEWAY_ACTION.to_owned()),
        ("sec_token", sec_token.to_owned()),
        ("region", "ap-southeast-1".to_owned()),
        ("params", params),
    ]
    .into_iter()
    .map(|(key, value)| format!("{key}={}", percent_encode(&value)))
    .collect::<Vec<_>>()
    .join("&")
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else if byte == b' ' {
            encoded.push('+');
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

/// Unwrap the nested console `DataV2` success response without treating failures as empty quotas.
pub fn parse_gateway_response(response: &QuotaResponse) -> Result<Value> {
    if !(200..300).contains(&response.status) {
        return Err(Error::InvalidState(format!(
            "qwen token-plan gateway returned HTTP {}",
            response.status
        )));
    }
    let value: Value = serde_json::from_slice(&response.body)
        .map_err(|_| Error::NotFound("qwen token-plan gateway returned invalid JSON".into()))?;
    parse_gateway_envelope(&value).cloned().ok_or_else(|| {
        Error::InvalidState("qwen token-plan gateway envelope was not successful".into())
    })
}

/// Extract `{code: 200, data: {DataV2: {data: {code: SUCCESS, success: true, data}}}}`.
pub fn parse_gateway_envelope(body: &Value) -> Option<&Value> {
    let code_ok = body
        .get("code")
        .is_some_and(|code| code.as_i64() == Some(200) || code.as_str() == Some("200"));
    if !code_ok {
        return None;
    }
    let inner = body.get("data")?.get("DataV2")?.get("data")?;
    if inner.get("code")?.as_str()? != "SUCCESS" || !inner.get("success")?.as_bool()? {
        return None;
    }
    inner.get("data")
}

/// Parse the optional 5-hour and weekly usage fractions into independent quota windows.
pub fn parse_usage_windows(payload: &Value) -> Vec<(String, UsageQuota)> {
    [
        ("per5HourPercentage", "per5HourResetTime", "5h"),
        ("per1WeekPercentage", "per1WeekResetTime", "1w"),
    ]
    .into_iter()
    .filter_map(|(percentage, reset, label)| {
        let used_percent = numeric(payload.get(percentage))?;
        let used_percent = used_percent.clamp(0.0, 1.0);
        let reset_at = payload
            .get(reset)
            .and_then(crate::quota::types::parse_reset_time);
        let quota =
            UsageQuota::from_counts(used_percent * 100.0, 100.0, reset_at).with_display_name(label);
        Some((label.to_owned(), quota))
    })
    .collect()
}

fn numeric(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64().filter(|value| value.is_finite()),
        Value::String(value) => value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::http::QuotaHttp;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};

    const ENVELOPE: &str = r#"{"code":"200","data":{"DataV2":{"data":{"code":"SUCCESS","success":true,"data":{"per5HourPercentage":0.25,"per5HourResetTime":1782864000000,"per1WeekPercentage":0.4,"per1WeekResetTime":1783296000000}}}}}"#;

    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: Mutex<Vec<QuotaResponse>>,
        requests: Mutex<Vec<QuotaRequest>>,
    }
    impl FakeHttp {
        fn new(reply: QuotaResponse) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(vec![reply]),
                requests: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.requests.lock().unwrap().push(request);
            Ok(self.replies.lock().unwrap().remove(0))
        }
    }
    fn response(body: &str) -> QuotaResponse {
        QuotaResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from(body.to_owned()),
        }
    }

    #[test]
    fn parses_derived_gateway_fixture_and_preserves_both_windows() {
        let response = response(ENVELOPE);
        let payload = parse_gateway_response(&response).unwrap();
        let windows = parse_usage_windows(&payload);
        assert_eq!(
            windows
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["5h", "1w"]
        );
        assert_eq!(windows[0].1.used, 25.0);
        assert_eq!(windows[0].1.remaining_percentage, 75.0);
        assert_eq!(windows[1].1.used, 40.0);
    }
    #[test]
    fn omitted_windows_and_failed_envelopes_are_not_zero_quotas() {
        let only_week = json!({"per1WeekPercentage":"0.5"});
        assert_eq!(parse_usage_windows(&only_week).len(), 1);
        assert!(parse_usage_windows(&json!({"per5HourResetTime":123})).is_empty());
        assert!(parse_gateway_envelope(&json!({"code":500,"data":{}})).is_none());
    }
    #[test]
    fn gateway_urls_and_forms_match_the_console_protocol() {
        assert_eq!(gateway_url(QWENCLOUD_GATEWAY, "usage"), "https://cs-data.qwencloud.com/data/api.json?product=sfm_bailian&action=IntlBroadScopeAspnGateway&api=zeldaHttp.apikeyMgr.%2Ftokenplan%2Fpersonal%2Fapi%2Fv2%2Fusage");
        let form = gateway_form("usage", "csrf token", "QWENCLOUD", "home.qwencloud.com");
        assert!(form.contains("sec_token=csrf+token"));
        assert!(form.contains("sfm_tokenplansolo_public_intl"));
        assert!(form.contains("QWENCLOUD"));
    }
    #[tokio::test]
    async fn fetch_uses_cookie_gateway_without_exposing_inference_key() {
        let fake = FakeHttp::new(response(ENVELOPE));
        let account = QuotaAccount::new().with_extra("qwenCloudCookie", json!("session-cookie"));
        let ctx = QuotaFetchContext::new(ID, Dialect::OpenAi, fake.clone())
            .with_account(account)
            .with_credential("inference-key-must-not-be-used");
        let quota = Qwen::new().fetch(&ctx).await.unwrap();
        assert_eq!(quota.source.as_deref(), Some(ID));
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
                .unwrap()
                .1,
            "session-cookie"
        );
        assert!(!requests[0]
            .headers
            .iter()
            .any(|(_, value)| value.contains("inference-key")));
    }
    #[tokio::test]
    async fn missing_cookie_is_rejected_before_http() {
        let fake = FakeHttp::new(response(ENVELOPE));
        let ctx = QuotaFetchContext::new(ID, Dialect::OpenAi, fake.clone());
        assert!(matches!(
            Qwen::new().fetch(&ctx).await,
            Err(Error::Config(_))
        ));
        assert!(fake.requests.lock().unwrap().is_empty());
    }
    #[test]
    fn supports_only_its_token_plan_provider() {
        assert!(Qwen::new().supports(Dialect::OpenAi, ID, &QuotaAccount::new()));
        assert!(!Qwen::new().supports(Dialect::OpenAi, "qwen", &QuotaAccount::new()));
    }
}
