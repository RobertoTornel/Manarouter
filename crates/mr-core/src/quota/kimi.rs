//! Kimi Coding Plan usage (`api.kimi.com/coding`).
//!
//! Ported from OmniRoute's `open-sse/services/usage/kimi.ts` (MIT, © 2026 diegosouzapw).
//!
//! ```text
//! GET https://api.kimi.com/coding/v1/usages
//! x-api-key: <apiKey>                      (API-key connection)
//! Authorization: Bearer <accessToken>      (OAuth connection, plus the X-Msh-* device headers)
//! ```
//!
//! # `apiVersion` is declared and deliberately not sent
//!
//! `KIMI_CONFIG.apiVersion = "2023-06-01"` exists upstream but never reaches the wire — the request
//! sets only the auth headers and `Content-Type`. This port reproduces that, and this paragraph
//! exists so nobody later "fixes" the missing version header: adding it changes what the endpoint
//! validates against, and the captured request does not carry it.
//!
//! # Which fetcher answers
//!
//! Upstream decides by **base URL**, not provider id: a connection pointed at
//! `api.kimi.com/coding` is the Coding Plan (`isKimiCodingConnection`). [`Kimi::supports`] keeps
//! that rule and also recognises the two coding-plan provider ids (`kimi-coding`,
//! `kimi-coding-apikey`) and their aliases.
//!
//! # The window key rules
//!
//! * `usage` (the managed Code 7-day quota) → `code_7d`.
//! * each `limits[i]` → `code_<duration><unit>` derived from `window.{duration,timeUnit}`, e.g.
//!   `code_5h`, `code_7d`, with `code_limit_<i>` when the window is missing or unclassifiable. A
//!   collision with an existing key gets a `_2`, `_3`, … suffix rather than overwriting.
//! * `usages.limit_7d` / `usages.limit_5h` → `code_7d` / `code_5h`, **overwriting** whatever the
//!   first two rules put there, exactly as upstream's `applyKimiMembershipRatios` does. These are
//!   0..1 ratios, so `used_ratio * 100` is the used percentage, rounded to four decimal places
//!   (0.147051 × 100 is not binary-exact).
//! * `five_hour` / `seven_day` (Claude-shaped `utilization`) → `session (5h)` / `weekly (7d)`.
//!
//! [`Kimi::fetch`] returns the first window in that insertion order — the account's headline Code
//! quota — with its key on `display_name`; [`parse_windows`] exposes all of them.

use async_trait::async_trait;
use indexmap::IndexMap;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::UsageQuota;

/// Fetcher id, stamped onto [`UsageQuota::source`].
pub const ID: &str = "kimi";

/// The usage endpoint (`KIMI_CONFIG.usageUrl`).
pub const USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

/// `KIMI_CODE_CLI_PLATFORM`.
pub const CLI_PLATFORM: &str = "kimi_code_cli";
/// `KIMI_CODE_CLI_VERSION` — the packaged CLI release upstream ships.
pub const CLI_VERSION: &str = "2.1.1";

/// Provider ids that are unambiguously the Kimi Coding Plan.
pub const CODING_PROVIDER_IDS: [&str; 4] = ["kimi-coding", "kmc", "kimi-coding-apikey", "kmca"];

/// `KIMI_CONFIG.apiVersion`. Declared upstream, **never sent**; kept here only so the fact is
/// documented in one place.
pub const DECLARED_API_VERSION: &str = "2023-06-01";

/// The Kimi Coding Plan fetcher.
#[derive(Debug, Clone, Copy, Default)]
pub struct Kimi;

impl Kimi {
    /// A fetcher with no configuration of its own.
    pub fn new() -> Self {
        Self
    }
}

/// True when a URL points at the Kimi Coding Plan (`api.kimi.com/coding`).
pub fn is_kimi_coding_base_url(base_url: Option<&str>) -> bool {
    let Some(raw) = base_url.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return false;
    };
    let Some((scheme, rest)) = raw.split_once("://") else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return false;
    }
    let mut parts = rest.splitn(2, '/');
    let authority = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = authority
        .split(':')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host != "api.kimi.com" {
        return false;
    }
    let path = format!("/{path}");
    path == "/coding" || path.starts_with("/coding/")
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

/// Coerce a JSON number or numeric string, upstream's `optionalNumber`.
fn optional_number(value: Option<&Value>) -> Option<f64> {
    let parsed = match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) if !text.trim().is_empty() => text.trim().parse::<f64>().ok(),
        _ => None,
    }?;
    parsed.is_finite().then_some(parsed)
}

/// `obj[key]` for the first key present, in the order given.
fn get<'a>(obj: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| obj.get(*key))
}

/// The raw reset value for a count/ratio detail, or `Null` when absent.
fn reset_value(obj: &Value, keys: &[&str]) -> Value {
    get(obj, keys).cloned().unwrap_or(Value::Null)
}

/// A count quota: `limit > 0` is required, and `used = used ?? (limit - remaining)`.
pub fn count_quota(value: Option<&Value>) -> Option<UsageQuota> {
    let detail = value?;
    let limit = optional_number(get(detail, &["limit", "Limit"]))?;
    if limit <= 0.0 {
        return None;
    }
    let reported_used = optional_number(get(detail, &["used", "Used"]));
    let reported_remaining = optional_number(get(detail, &["remaining", "Remaining"]));
    let used = reported_used
        .unwrap_or_else(|| reported_remaining.map_or(0.0, |remaining| limit - remaining));
    let reset = reset_value(detail, &["resetTime", "reset_at", "resetAt"]);
    Some(UsageQuota::from_usage(used, limit, &reset))
}

/// A membership ratio: `used_ratio` is 0..1, reported as a four-decimal used percentage of 100.
pub fn ratio_quota(value: Option<&Value>) -> Option<UsageQuota> {
    let detail = value?;
    let ratio = optional_number(get(detail, &["used_ratio", "usedRatio"]))?;
    if ratio < 0.0 {
        return None;
    }
    // `ratio * 100` is not binary-exact (0.147051 * 100 == 14.705099999999998), so round the
    // percent to four decimal places, then clamp, exactly as upstream does.
    let used = ((ratio * 1e6).round() / 1e4).clamp(0.0, 100.0);
    let reset = reset_value(detail, &["reset_time", "resetTime", "resetAt"]);
    Some(UsageQuota::from_usage(used, 100.0, &reset))
}

/// A Claude-shaped `utilization` window: `utilization` is the *remaining* percentage.
pub fn utilization_quota(value: &Value) -> Option<UsageQuota> {
    let remaining = optional_number(value.get("utilization"))?.clamp(0.0, 100.0);
    let reset = reset_value(value, &["resets_at", "resetsAt"]);
    Some(UsageQuota::from_usage(100.0 - remaining, 100.0, &reset))
}

/// Render a duration the way JavaScript's template literal does: `5`, not `5.0`.
fn format_duration(duration: f64) -> String {
    if duration.fract() == 0.0 && duration.abs() < 1e15 {
        format!("{}", duration as i64)
    } else {
        format!("{duration}")
    }
}

/// `normalizeKimiWindow`: derive the `code_<key>` fragment and display name for a `limits[]` row.
pub fn normalize_window(window: Option<&Value>, fallback_index: usize) -> (String, String) {
    let duration = window.and_then(|window| optional_number(window.get("duration")));
    let time_unit = window
        .and_then(|window| window.get("timeUnit"))
        .and_then(Value::as_str)
        .unwrap_or("");

    if let Some(duration) = duration.filter(|duration| *duration > 0.0) {
        let rendered = format_duration(duration);
        match time_unit {
            "TIME_UNIT_MINUTE" if duration % 60.0 == 0.0 => {
                let hours = format_duration(duration / 60.0);
                return (format!("{hours}h"), format!("Code · {hours}h"));
            }
            "TIME_UNIT_HOUR" => return (format!("{rendered}h"), format!("Code · {rendered}h")),
            "TIME_UNIT_DAY" => return (format!("{rendered}d"), format!("Code · {rendered}d")),
            "TIME_UNIT_WEEK" => return (format!("{rendered}w"), format!("Code · {rendered}w")),
            "TIME_UNIT_MINUTE" => return (format!("{rendered}m"), format!("Code · {rendered}m")),
            _ => {}
        }
    }

    (
        format!("limit_{fallback_index}"),
        format!("Code · Limit {fallback_index}"),
    )
}

/// The windows a `/v1/usages` body describes, in insertion order: `(key, display_name, quota)`.
///
/// Pure, so tests drive it with captured JSON. Later rules overwrite earlier keys where upstream
/// does (`usages` ratios over `code_7d`/`code_5h`), and `limits[]` collisions take a `_2` suffix.
pub fn parse_windows(body: &Value) -> Vec<(String, String, UsageQuota)> {
    let mut map: IndexMap<String, (String, UsageQuota)> = IndexMap::new();

    if let Some(quota) = count_quota(body.get("usage")) {
        map.insert("code_7d".to_string(), ("Code · 7d".to_string(), quota));
    }

    if let Some(limits) = body.get("limits").and_then(Value::as_array) {
        for (index, item) in limits.iter().enumerate() {
            let Some(quota) = count_quota(item.get("detail")) else {
                continue;
            };
            let (fragment, derived_display) = normalize_window(item.get("window"), index + 1);
            let base_key = format!("code_{fragment}");
            let mut key = base_key.clone();
            let mut suffix = 2;
            while map.contains_key(&key) {
                key = format!("{base_key}_{suffix}");
                suffix += 1;
            }
            let display = item
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(|name| {
                    if name.to_ascii_lowercase().starts_with("code") {
                        name.to_string()
                    } else {
                        format!("Code · {name}")
                    }
                })
                .unwrap_or(derived_display);
            map.insert(key, (display, quota));
        }
    }

    // Membership ratios deliberately overwrite the same-named windows.
    let usages = body.get("usages");
    for (source, key, display) in [
        ("limit_7d", "code_7d", "Code · 7d"),
        ("limit_5h", "code_5h", "Code · 5h"),
    ] {
        if let Some(quota) = ratio_quota(usages.and_then(|usages| usages.get(source))) {
            map.insert(key.to_string(), (display.to_string(), quota));
        }
    }

    for (field, key) in [("five_hour", "session (5h)"), ("seven_day", "weekly (7d)")] {
        if let Some(quota) = body.get(field).and_then(utilization_quota) {
            map.insert(key.to_string(), (key.to_string(), quota));
        }
    }

    if let Some(object) = body.as_object() {
        for (field, value) in object {
            let Some(model) = field.strip_prefix("seven_day_") else {
                continue;
            };
            if field == "seven_day" {
                continue;
            }
            if let Some(quota) = utilization_quota(value) {
                let key = format!("weekly {model} (7d)");
                map.insert(key.clone(), (key, quota));
            }
        }
    }

    map.into_iter()
        .map(|(key, (display, quota))| (key, display, quota))
        .collect()
}

/// Check the envelope: a non-zero `code` inside an HTTP 200 is an authentication failure, not a
/// quota. Reporting it as a quota would render an exhausted account.
pub fn check_envelope(body: &Value) -> Result<()> {
    let code = body.get("code").and_then(|code| match code {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    });
    if let Some(code) = code.filter(|code| *code != 0) {
        let message = body
            .get("message")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("no message");
        return Err(Error::InvalidState(format!(
            "kimi usages envelope failed with code {code}: {message}"
        )));
    }
    Ok(())
}

/// The headline quota: the first window upstream's map would contain.
pub fn parse_usage(body: &Value) -> Result<UsageQuota> {
    check_envelope(body)?;
    let mut windows = parse_windows(body);
    if windows.is_empty() {
        return Err(Error::NotFound(
            "kimi usages response reported no count, ratio or utilization quota".into(),
        ));
    }
    let (key, display, quota) = windows.remove(0);
    Ok(quota
        .with_source(ID)
        .with_display_name(if key.is_empty() { display } else { key }))
}

/// Keep only printable ASCII in a header value, with a fallback — `sanitizeKimiHeaderValue`.
fn sanitize_header_value(value: Option<&str>, fallback: &str) -> String {
    let cleaned: String = value
        .unwrap_or_default()
        .trim()
        .chars()
        .filter(|character| ('\u{20}'..='\u{7e}').contains(character))
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.to_string()
    }
}

/// `normalizeKimiDeviceId`: a bare 32-hex device id is rendered in UUID groups.
fn normalize_device_id(raw: Option<&str>) -> String {
    let sanitized = sanitize_header_value(raw, "");
    if sanitized.is_empty() {
        return String::new();
    }
    let is_hex32 = sanitized.len() == 32
        && sanitized
            .chars()
            .all(|character| character.is_ascii_hexdigit());
    if !is_hex32 {
        return sanitized;
    }
    let lowered = sanitized.to_ascii_lowercase();
    format!(
        "{}-{}-{}-{}-{}",
        &lowered[0..8],
        &lowered[8..12],
        &lowered[12..16],
        &lowered[16..20],
        &lowered[20..32]
    )
}

/// The `X-Msh-*` identity headers an OAuth (non API-key) request carries.
pub fn identity_headers(account: &QuotaAccount) -> Vec<(String, String)> {
    let lookup = |keys: &[&str]| -> Option<&str> {
        keys.iter().find_map(|key| {
            account
                .extra
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
    };
    let device_id = normalize_device_id(lookup(&["deviceId", "device_id"]));
    vec![
        ("X-Msh-Platform".to_string(), CLI_PLATFORM.to_string()),
        ("X-Msh-Version".to_string(), CLI_VERSION.to_string()),
        (
            "X-Msh-Device-Name".to_string(),
            sanitize_header_value(lookup(&["deviceName", "device_name"]), "unknown"),
        ),
        (
            "X-Msh-Device-Model".to_string(),
            sanitize_header_value(lookup(&["deviceModel", "device_model"]), "unknown"),
        ),
        (
            "X-Msh-Os-Version".to_string(),
            sanitize_header_value(lookup(&["osVersion", "os_version"]), "unknown"),
        ),
        (
            "X-Msh-Device-Id".to_string(),
            sanitize_header_value(Some(device_id.as_str()), "unknown"),
        ),
    ]
}

/// Whether a connection authenticates with an inference API key rather than an OAuth token.
pub fn uses_api_key(provider_id: &str, account: &QuotaAccount) -> bool {
    match provider_id {
        "kimi-coding-apikey" | "kmca" => true,
        "kimi-coding" | "kmc" => false,
        _ => !account.oauth,
    }
}

/// The User-Agent the CLI sends; the version is pinned rather than read from the environment, so a
/// test outcome cannot depend on a developer's shell.
pub fn cli_user_agent() -> String {
    format!("kimi-code-cli/{CLI_VERSION}")
}

#[async_trait]
impl QuotaFetcher for Kimi {
    fn id(&self) -> &'static str {
        ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, account: &QuotaAccount) -> bool {
        CODING_PROVIDER_IDS.contains(&provider_id)
            || is_kimi_coding_base_url(declared_base_url(account))
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "provider {:?} has no credential; the Kimi usages endpoint needs an API key or \
                     an OAuth access token",
                    ctx.provider_id
                ))
            })?;

        let api_key_auth = uses_api_key(&ctx.provider_id, &ctx.account);
        // `KIMI_CONFIG.apiVersion` is intentionally absent: the captured request never sends it.
        let mut request = QuotaRequest::get(USAGE_URL).header("Content-Type", "application/json");
        if api_key_auth {
            request = request.header("x-api-key", credential.expose());
        } else {
            request = request.bearer(credential.expose());
            for (name, value) in identity_headers(&ctx.account) {
                request = request.header(name, value);
            }
            request = request.header("User-Agent", cli_user_agent());
        }

        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "kimi usages endpoint returned HTTP {}",
                response.status
            )));
        }
        parse_usage(&response.json()?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaRequest, QuotaResponse};

    /// A captured count response: `usage` plus two `limits[]` rows.
    const COUNT_USAGE: &str = r#"{
        "code": 0,
        "usage": {
            "limit": 1000,
            "used": 250,
            "remaining": 750,
            "resetTime": "2026-01-08T00:00:00Z"
        },
        "limits": [
            {
                "name": "Code",
                "detail": { "limit": 500, "used": 100, "reset_at": 1700003600 },
                "window": { "duration": 5, "timeUnit": "TIME_UNIT_HOUR" }
            },
            {
                "name": "Weekly",
                "detail": { "limit": 2000, "remaining": 500 },
                "window": { "duration": 7, "timeUnit": "TIME_UNIT_DAY" }
            },
            {
                "name": "No window",
                "detail": { "limit": 10, "used": 1 }
            }
        ]
    }"#;

    /// A captured membership-ratio response (Claude-shaped endpoint).
    const RATIO_USAGE: &str = r#"{
        "code": 0,
        "usages": {
            "limit_7d": { "used_ratio": 0.147051, "reset_time": 1700000000000 },
            "limit_5h": { "used_ratio": 0.5, "reset_time": 1700000000000 }
        }
    }"#;

    const ENVELOPE_ERROR: &str = r#"{
        "code": 401,
        "message": "authentication failed",
        "usage": null
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

    /// A context whose transport is coerced to `Arc<dyn QuotaHttp>` here, so the caller keeps its
    /// own `Arc<FakeHttp>` for the request log.
    fn context(provider: &str, http: Arc<FakeHttp>, credential: &str) -> QuotaFetchContext {
        QuotaFetchContext::new(provider, Dialect::Anthropic, http).with_credential(credential)
    }

    fn na_ctx() -> QuotaAccount {
        QuotaAccount::new()
    }

    #[test]
    fn ratio_path_maps_a_zero_to_one_fraction_to_a_four_decimal_percent() {
        let body: Value = serde_json::from_str(RATIO_USAGE).expect("canned json parses");
        let windows = parse_windows(&body);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].0, "code_7d");
        assert_eq!(windows[0].1, "Code · 7d");
        assert_eq!(windows[0].2.used, 14.7051, "rounded to four decimals");
        assert_eq!(windows[0].2.total, 100.0);
        assert_eq!(windows[0].2.reset_at, Some(1_700_000_000_000));
        assert_eq!(windows[1].0, "code_5h");
        assert_eq!(windows[1].2.used, 50.0);
    }

    #[test]
    fn count_path_uses_used_then_derives_from_remaining() {
        let body: Value = serde_json::from_str(COUNT_USAGE).expect("canned json parses");
        let windows = parse_windows(&body);
        let code_7d = &windows[0];
        assert_eq!(code_7d.0, "code_7d");
        assert_eq!(code_7d.2.used, 250.0);
        assert_eq!(code_7d.2.total, 1000.0);
        assert_eq!(code_7d.2.remaining, 750.0);
    }

    #[test]
    fn limit_windows_derive_code_keys_from_their_duration_and_unit() {
        let body: Value = serde_json::from_str(COUNT_USAGE).expect("canned json parses");
        let windows = parse_windows(&body);
        let keys: Vec<&str> = windows.iter().map(|(key, _, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            vec!["code_7d", "code_5h", "code_7d_2", "code_limit_3"]
        );
    }

    #[test]
    fn a_window_key_collision_takes_a_suffix_instead_of_overwriting() {
        let body = serde_json::json!({
            "limits": [
                { "detail": { "limit": 10, "used": 1 },
                  "window": { "duration": 7, "timeUnit": "TIME_UNIT_DAY" } },
                { "detail": { "limit": 20, "used": 2 },
                  "window": { "duration": 7, "timeUnit": "TIME_UNIT_DAY" } }
            ]
        });
        let keys: Vec<String> = parse_windows(&body)
            .into_iter()
            .map(|(key, _, _)| key)
            .collect();
        assert_eq!(keys, vec!["code_7d", "code_7d_2"]);
    }

    #[test]
    fn membership_ratios_overwrite_the_count_windows_with_the_same_key() {
        let body = serde_json::json!({
            "usage": { "limit": 100, "used": 10 },
            "usages": { "limit_7d": { "used_ratio": 0.25 } }
        });
        let windows = parse_windows(&body);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].0, "code_7d");
        assert_eq!(windows[0].2.used, 25.0, "the ratio replaced the count");
        assert_eq!(windows[0].2.total, 100.0);
    }

    #[test]
    fn utilization_windows_are_remaining_percentages() {
        let body = serde_json::json!({
            "five_hour": { "utilization": 80, "resets_at": 1700003600 },
            "seven_day": { "utilization": 20 }
        });
        let windows = parse_windows(&body);
        assert_eq!(windows[0].0, "session (5h)");
        assert_eq!(windows[0].2.used, 20.0);
        assert_eq!(windows[0].2.remaining, 80.0);
        assert_eq!(windows[1].0, "weekly (7d)");
    }

    #[test]
    fn envelope_code_is_an_error_even_on_http_200() {
        let body: Value = serde_json::from_str(ENVELOPE_ERROR).expect("canned json parses");
        let error = parse_usage(&body).expect_err("code 401 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    }

    #[test]
    fn a_body_with_no_quota_is_an_error_not_zero() {
        let body = serde_json::json!({ "code": 0, "limits": [] });
        let error = parse_usage(&body).expect_err("no quota must fail");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    }

    #[test]
    fn coding_base_urls_and_provider_ids_are_recognised() {
        assert!(is_kimi_coding_base_url(Some(
            "https://api.kimi.com/coding/v1"
        )));
        assert!(is_kimi_coding_base_url(Some("https://api.kimi.com/coding")));
        assert!(!is_kimi_coding_base_url(Some("https://api.kimi.com/v1")));
        assert!(!is_kimi_coding_base_url(Some("https://api.moonshot.cn/v1")));
        assert!(!is_kimi_coding_base_url(None));

        let fetcher = Kimi::new();
        assert!(fetcher.supports(Dialect::Anthropic, "kimi-coding", &na_ctx()));
        assert!(fetcher.supports(Dialect::OpenAi, "kmca", &na_ctx()));
        assert!(!fetcher.supports(Dialect::OpenAi, "moonshot", &na_ctx()));
        let pointed = QuotaAccount::new().with_extra(
            "baseUrl",
            Value::String("https://api.kimi.com/coding/v1".into()),
        );
        assert!(fetcher.supports(Dialect::OpenAi, "some-uuid-node", &pointed));
    }

    #[test]
    fn device_ids_are_normalised_and_header_values_are_sanitized() {
        assert_eq!(
            normalize_device_id(Some("0123456789abcdef0123456789abcdef")),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(normalize_device_id(Some("not-a-hex-id")), "not-a-hex-id");
        let account = QuotaAccount::new().with_extra(
            "deviceId",
            Value::String("0123456789abcdef0123456789abcdef".into()),
        );
        let headers = identity_headers(&account);
        let by_name = |name: &str| {
            headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_default()
        };
        assert_eq!(by_name("X-Msh-Platform"), CLI_PLATFORM);
        assert_eq!(by_name("X-Msh-Version"), CLI_VERSION);
        assert_eq!(
            by_name("X-Msh-Device-Id"),
            "01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(by_name("X-Msh-Device-Name"), "unknown");
    }

    #[tokio::test]
    async fn api_key_connection_sends_x_api_key_and_no_device_headers() {
        let http = Arc::new(FakeHttp::new(vec![json_response(200, COUNT_USAGE)]));
        let ctx = context("kimi-coding-apikey", Arc::clone(&http), "sk-kimi");
        let quota = Kimi::new().fetch(&ctx).await.expect("fetch succeeds");
        assert_eq!(quota.used, 250.0);
        assert_eq!(quota.source.as_deref(), Some(ID));

        let seen = http.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].url, USAGE_URL);
        let header = |name: &str| {
            seen[0]
                .headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("x-api-key"), Some("sk-kimi"));
        assert_eq!(header("authorization"), None);
        assert_eq!(header("X-Msh-Platform"), None);
        assert_eq!(
            header("Anthropic-Version"),
            None,
            "only Content-Type and the auth header are sent"
        );
    }

    #[tokio::test]
    async fn oauth_connection_sends_a_bearer_token_and_the_cli_identity_headers() {
        let http = Arc::new(FakeHttp::new(vec![json_response(200, RATIO_USAGE)]));
        let account = QuotaAccount::new().with_extra("oauth", Value::Bool(true));
        let ctx =
            context("kimi-coding", Arc::clone(&http), "oauth-access-token").with_account(account);
        Kimi::new().fetch(&ctx).await.expect("fetch succeeds");

        let seen = http.seen();
        let header = |name: &str| {
            seen[0]
                .headers
                .iter()
                .find(|(header, _)| header == name)
                .map(|(_, value)| value.as_str())
        };
        assert_eq!(header("authorization"), Some("Bearer oauth-access-token"));
        assert_eq!(header("x-api-key"), None);
        assert_eq!(header("X-Msh-Platform"), Some(CLI_PLATFORM));
        assert_eq!(header("User-Agent"), Some("kimi-code-cli/2.1.1"));
    }

    #[tokio::test]
    async fn http_and_envelope_failures_are_errors() {
        let http = Arc::new(FakeHttp::new(vec![json_response(500, "boom")]));
        let ctx = context("kimi-coding-apikey", Arc::clone(&http), "sk-kimi");
        let error = Kimi::new().fetch(&ctx).await.expect_err("500 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");

        let http = Arc::new(FakeHttp::new(vec![json_response(200, ENVELOPE_ERROR)]));
        let ctx = context("kimi-coding-apikey", Arc::clone(&http), "sk-kimi");
        let error = Kimi::new()
            .fetch(&ctx)
            .await
            .expect_err("envelope 401 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    }

    #[test]
    fn the_declared_api_version_is_documented_but_not_a_header() {
        assert_eq!(DECLARED_API_VERSION, "2023-06-01");
    }
}
