//! Zhipu GLM / Z.ai plan quota.
//!
//! Ported from OmniRoute's `open-sse/services/usage/glm.ts` (`getGlmUsage`,
//! `getGlmTokenQuotaName`, `getGlmQuotaDisplayName`, `glmMonthlyRemainingPercentage`) and
//! `open-sse/config/glmProvider.ts` (`buildGlmQuotaFetch`, `getGlmQuotaUrl`,
//! `getGlmApiRegion`, `getGlmTeamQuotaConfig`) — MIT, © 2026 diegosouzapw.
//!
//! `GET /api/monitor/usage/quota/limit` answers an envelope:
//! `{code, success, msg, data: {planName|level, limits[]}}`, each limit
//! `{type, unit, number, percentage, nextResetTime, usage, currentValue, remaining, models[],
//! usageDetails[]}`.
//!
//! Two details that are easy to get wrong and are load-bearing here:
//!
//! * **`code: 401` arrives inside an HTTP 200 body.** An expired or wrong key answers
//!   `{"code": 401, "success": false, "msg": "..."}` with a 200 status, so the status alone
//!   is not the auth check. The envelope is checked first and an auth code becomes an
//!   [`Error::Config`] — never a quota read from a body that is really an error.
//! * **`nextResetTime` is epoch milliseconds.** Unlike most upstreams (where the same field
//!   name means seconds) it is converted with `new Date(resetMs)` directly, so it is used as
//!   the millisecond instant it is and is **not** passed through the shared seconds
//!   heuristic. `0` or absent means "no scheduled reset" and stays `None`.
//!
//! # Windows, and the magic `unit`/`number` pairs
//!
//! The window a `TOKENS_LIMIT`/`CREDIT_LIMIT` row belongs to is not named by the upstream:
//! it is inferred from an undocumented `unit`/`number` pair. Those pairs live in one named
//! table ([`GLM_WINDOW_RULES`]) rather than inline, because they are magic numbers with no
//! public documentation. The duration each window key maps to still goes through
//! [`WindowFamily::from_seconds`], so a `session` key is shown as a 5h family and a
//! `mcp_monthly` key as 30d; the label follows the real length, not the key's name.
//!
//! The Rust [`QuotaFetcher`] contract returns **one** `UsageQuota` per account, where
//! OmniRoute returned a map of windows. This file therefore returns the most binding window
//! (`session`, then `weekly`, then `mcp_monthly`) as the quota itself and lists every window,
//! duration-labelled, as a [`QuotaDetail`] row so nothing is dropped.
//!
//! Coding Plan *reset cards* (`glmResetCards.ts`) live on a separate endpoint and cost an
//! extra request; they are deliberately not ported here.

use std::collections::BTreeSet;

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, UsageQuota};
use crate::quota::window::WindowFamily;

/// The international (Z.ai) quota endpoint, `GLM_QUOTA_URLS.international`.
pub const QUOTA_URL_INTERNATIONAL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
/// The China (BigModel) quota endpoint, `GLM_QUOTA_URLS.china`.
pub const QUOTA_URL_CHINA: &str = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";

/// The provider id forced onto the China region regardless of the account's `apiRegion`.
pub const CHINA_FORCED_PROVIDER_ID: &str = "glm-cn";

/// The fetcher id, also stamped onto the `source` field of what it returns.
pub const SOURCE: &str = Glm::ID;

/// One `unit`/`number` pair that names a coding-plan window. Undocumented upstream, kept in
/// one table so the magic numbers are greppable and testable in one place.
struct GlmWindowRule {
    unit: f64,
    number: f64,
    key: &'static str,
}

/// `getGlmTokenQuotaName`'s exact pairs. Anything else falls through to the
/// `unit == 3 && number >= 24*7` rule, then to the `seen session ? weekly : session` rule.
const GLM_WINDOW_RULES: &[GlmWindowRule] = &[
    GlmWindowRule {
        unit: 3.0,
        number: 5.0,
        key: "session",
    },
    GlmWindowRule {
        unit: 6.0,
        number: 1.0,
        key: "weekly",
    },
    GlmWindowRule {
        unit: 4.0,
        number: 7.0,
        key: "weekly",
    },
];

/// `unit == 3 && number >= 24 * 7` — a month's worth of the 5-hour unit is the weekly window.
const GLM_WEEKLY_MIN_NUMBER_FOR_UNIT_3: f64 = 24.0 * 7.0;

/// Zhipu GLM / Z.ai coding-plan quota.
#[derive(Debug, Clone, Copy, Default)]
pub struct Glm;

impl Glm {
    /// The fetcher id, and the first provider id this fetcher claims.
    pub const ID: &str = "glm";

    /// A stateless fetcher; `Arc`-shared by the registry.
    pub fn new() -> Self {
        Self
    }
}

/// Which regional API answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GlmRegion {
    International,
    China,
}

#[async_trait]
impl QuotaFetcher for Glm {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        provider_id == Self::ID || provider_id == CHINA_FORCED_PROVIDER_ID
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| Error::Config("glm quota requires an account API key".to_owned()))?;

        let team = team_config(&ctx.account)?;
        let region = region_for(&ctx.provider_id, &ctx.account);
        let mut url = quota_url(region).to_owned();
        if team.is_configured() {
            // `buildGlmQuotaFetch`: team keys add `?type=2`.
            url.push_str(if url.contains('?') {
                "&type=2"
            } else {
                "?type=2"
            });
        }

        let mut request = QuotaRequest::get(url)
            .bearer(credential.expose())
            .header("Accept", "application/json");
        if let Some(organization) = team.organization() {
            request = request.header("bigmodel-organization", organization);
        }
        if let Some(project) = team.project() {
            request = request.header("bigmodel-project", project);
        }

        let response = ctx.http.request(request).await?;
        if matches!(response.status, 401 | 403) {
            return Err(Error::Config(format!(
                "glm rejected the account credential (HTTP {})",
                response.status
            )));
        }
        if !response.is_success() {
            return Err(Error::NotFound(format!(
                "GLM quota API error (HTTP {})",
                response.status
            )));
        }

        let body = response.json()?;
        check_envelope(&body)?;
        Ok(assemble(parse_limits(&body)?))
    }
}

/// The regional endpoint for a provider id and account. `glm-cn` is forced to China even
/// when the account says otherwise; every other id follows `apiRegion == "china"`.
fn region_for(provider_id: &str, account: &QuotaAccount) -> GlmRegion {
    if provider_id == CHINA_FORCED_PROVIDER_ID {
        return GlmRegion::China;
    }
    if extra_str(account, "apiRegion").as_deref() == Some("china") {
        return GlmRegion::China;
    }
    GlmRegion::International
}

fn quota_url(region: GlmRegion) -> &'static str {
    match region {
        GlmRegion::International => QUOTA_URL_INTERNATIONAL,
        GlmRegion::China => QUOTA_URL_CHINA,
    }
}

/// The upstream organization/project keys, in `getGlmTeamQuotaConfig`'s precedence order.
const ORGANIZATION_KEYS: &[&str] = &[
    "glmOrganizationId",
    "bigmodelOrganization",
    "glmOrganization",
];
const PROJECT_KEYS: &[&str] = &["glmProjectId", "bigmodelProject", "glmProject"];

/// `getGlmTeamQuotaConfig`'s three states.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TeamConfig {
    /// Neither id is present: a personal key.
    None,
    /// Both ids are present: the team quota, with the extra headers and `?type=2`.
    Configured {
        organization: String,
        project: String,
    },
}

impl TeamConfig {
    fn is_configured(&self) -> bool {
        matches!(self, Self::Configured { .. })
    }

    fn organization(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Configured { organization, .. } => Some(organization),
        }
    }

    fn project(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Configured { project, .. } => Some(project),
        }
    }
}

/// A half-configured team key cannot be queried, so it is an [`Error::Config`] naming the
/// missing field, exactly as upstream's `incomplete` state does.
fn team_config(account: &QuotaAccount) -> Result<TeamConfig> {
    let organization = first_extra(account, ORGANIZATION_KEYS);
    let project = first_extra(account, PROJECT_KEYS);
    match (organization, project) {
        (None, None) => Ok(TeamConfig::None),
        (Some(organization), Some(project)) => Ok(TeamConfig::Configured {
            organization,
            project,
        }),
        (None, Some(_)) => Err(Error::Config(
            "GLM team plan quota requires both Organization ID and Project ID; \
             the Organization ID is missing"
                .to_owned(),
        )),
        (Some(_), None) => Err(Error::Config(
            "GLM team plan quota requires both Organization ID and Project ID; \
             the Project ID is missing"
                .to_owned(),
        )),
    }
}

fn first_extra(account: &QuotaAccount, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| extra_str(account, key))
        .filter(|value| !value.is_empty())
}

fn extra_str(account: &QuotaAccount, key: &str) -> Option<String> {
    account
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// The `code: 401`-inside-HTTP-200 check, plus the `success: false` envelope error.
fn check_envelope(body: &Value) -> Result<()> {
    // `toNumber(json.code, 200) === 401`. This is the single most important line in the
    // file: the HTTP status was 200 and only the body knows the key is bad.
    if opt_number(body.get("code")).map(|code| code as i64) == Some(401) {
        return Err(Error::Config(
            "GLM quota API reported an invalid API key (code 401 in an HTTP 200 body)".to_owned(),
        ));
    }
    if body.get("success") == Some(&Value::Bool(false)) {
        let message = {
            let msg = text(body.get("msg"));
            if msg.is_empty() {
                text(body.get("message"))
            } else {
                msg
            }
        };
        return Err(Error::NotFound(if message.is_empty() {
            "GLM quota request failed".to_owned()
        } else {
            format!("GLM quota request failed: {message}")
        }));
    }
    Ok(())
}

/// Walk `data.limits`, producing one `(window key, quota)` per recognised row.
///
/// An empty or unrecognised `limits` array is an [`Error::NotFound`]: with no numbers there
/// is nothing to report, and an invented `0/0` would read as an exhausted account.
fn parse_limits(body: &Value) -> Result<Vec<(String, UsageQuota)>> {
    let limits = body
        .get("data")
        .and_then(|data| data.get("limits"))
        .and_then(Value::as_array)
        .filter(|limits| !limits.is_empty())
        .ok_or_else(|| Error::NotFound("GLM returned no quota limits".to_owned()))?;

    let mut windows: Vec<(String, UsageQuota)> = Vec::new();
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();
    for limit in limits {
        let kind = text(limit.get("type")).to_uppercase();
        // Milliseconds, straight through — see the module docs.
        let reset_at = reset_at_millis(limit);
        match kind.as_str() {
            "TOKENS_LIMIT" | "CREDIT_LIMIT" => {
                let unit = opt_number(limit.get("unit")).unwrap_or(0.0);
                let number = opt_number(limit.get("number")).unwrap_or(0.0);
                let key = window_key(unit, number, &seen);
                seen.insert(key);
                windows.push((
                    key.to_owned(),
                    attach_details(count_quota(limit, reset_at), limit),
                ));
            }
            "TIME_LIMIT" => {
                windows.push((
                    "mcp_monthly".to_owned(),
                    attach_details(monthly_quota(limit, reset_at), limit),
                ));
            }
            // Unknown types are skipped rather than guessed at.
            _ => {}
        }
    }
    if windows.is_empty() {
        return Err(Error::NotFound(
            "GLM returned no recognised quota limit types".to_owned(),
        ));
    }
    Ok(windows)
}

/// `getGlmTokenQuotaName`: the magic pairs, then the month-of-5h rule, then
/// `seen session ? weekly : session`.
fn window_key(unit: f64, number: f64, seen: &BTreeSet<&'static str>) -> &'static str {
    for rule in GLM_WINDOW_RULES {
        if rule.unit == unit && rule.number == number {
            return rule.key;
        }
    }
    if unit == 3.0 && number >= GLM_WEEKLY_MIN_NUMBER_FOR_UNIT_3 {
        return "weekly";
    }
    if seen.contains("session") {
        "weekly"
    } else {
        "session"
    }
}

/// `TOKENS_LIMIT`/`CREDIT_LIMIT`: the percentage path (`used = percentage`, `total = 100`),
/// or the absolute-credit path when the row carries a positive `usage` window total.
///
/// `from_counts` derives `remaining` as `total - used`, which is the same absolute credit
/// figure upstream reads from `remaining` when the two agree.
fn count_quota(limit: &Value, reset_at: Option<i64>) -> UsageQuota {
    let percent = percent(limit);
    let window_total = opt_number(limit.get("usage")).unwrap_or(0.0);
    let (used, total) = if window_total > 0.0 {
        // CREDIT_LIMIT (and absolute TOKENS_LIMIT) rows: prefer the real credits so the
        // card renders "24659 / 28000" instead of a percent-only scale.
        // A missing currentValue must not put a 0..=100 percentage on a credit scale;
        // scale it the way monthly_quota does so "used" stays in the total's unit.
        let used = opt_number(limit.get("currentValue"))
            .unwrap_or_else(|| (window_total * percent / 100.0).clamp(0.0, window_total));
        (used, window_total)
    } else {
        (percent, 100.0)
    };
    UsageQuota::from_counts(used, total, reset_at).with_source(SOURCE)
}

/// `TIME_LIMIT` -> the `mcp_monthly` window.
///
/// With an absolute cap (`usage > 0`) the credits are authoritative. Without one the
/// percentage is used on a 100 scale so "no monthly cap" renders full rather than the
/// misleading `0/0` upstream fixed in #3580.
fn monthly_quota(limit: &Value, reset_at: Option<i64>) -> UsageQuota {
    let percent = percent(limit);
    let total = opt_number(limit.get("usage"))
        .or_else(|| opt_number(limit.get("total")))
        .unwrap_or(0.0);
    let quota = if total > 0.0 {
        let used = opt_number(limit.get("currentValue"))
            .or_else(|| {
                opt_number(limit.get("remaining")).map(|remaining| (total - remaining).max(0.0))
            })
            .unwrap_or_else(|| (total * percent / 100.0).clamp(0.0, total));
        UsageQuota::from_counts(used, total, reset_at)
    } else {
        let remaining_percent = opt_number(limit.get("remaining"))
            .map(|remaining| remaining.clamp(0.0, 100.0))
            .unwrap_or_else(|| (100.0 - percent).max(0.0));
        UsageQuota::from_counts(100.0 - remaining_percent, 100.0, reset_at)
    };
    quota.with_source(SOURCE)
}

/// `models[].{model, percentage}` and `usageDetails[].{modelCode|name, usage}` become detail
/// rows, as upstream does.
fn attach_details(mut quota: UsageQuota, limit: &Value) -> UsageQuota {
    if let Some(models) = limit.get("models").and_then(Value::as_array) {
        for model in models {
            let name = text(model.get("model"));
            if name.is_empty() {
                continue;
            }
            quota = quota.with_detail(QuotaDetail {
                name,
                used: opt_number(model.get("percentage")).unwrap_or(0.0),
            });
        }
    }
    if let Some(items) = limit.get("usageDetails").and_then(Value::as_array) {
        for item in items {
            let name = {
                let code = text(item.get("modelCode"));
                if code.is_empty() {
                    let name = text(item.get("name"));
                    if name.is_empty() {
                        "usage".to_owned()
                    } else {
                        name
                    }
                } else {
                    code
                }
            };
            quota = quota.with_detail(QuotaDetail {
                name,
                used: opt_number(item.get("usage")).unwrap_or(0.0),
            });
        }
    }
    quota
}

/// One `UsageQuota` for the account: the most binding window, every window listed as a
/// duration-labelled detail.
fn assemble(windows: Vec<(String, UsageQuota)>) -> UsageQuota {
    const PRIORITY: &[&str] = &["session", "weekly", "mcp_monthly"];
    let primary = PRIORITY
        .iter()
        .find(|key| windows.iter().any(|(window, _)| window == *key))
        .copied()
        .unwrap_or_else(|| windows[0].0.as_str());

    let mut quota = windows
        .iter()
        .find(|(window, _)| window == primary)
        .map(|(_, quota)| quota.clone())
        .expect("the primary window was selected from this list");
    quota.display_name = Some(display_name(primary).to_owned());
    for (window, detail) in &windows {
        // The key names the routing bucket; `WindowFamily` fixes the duration label, so a
        // 30-day `mcp_monthly` is never presented as a 5-hour window.
        quota = quota.with_detail(QuotaDetail {
            name: format!("{window} ({})", window_family(window).label()),
            used: detail.used,
        });
    }
    quota
}

/// The display name upstream gives each window.
fn display_name(window: &str) -> &'static str {
    match window {
        "session" => "5 Hours Quota",
        "weekly" => "Weekly Quota",
        "mcp_monthly" => "Monthly",
        _ => "Quota",
    }
}

/// The real duration behind a window key, classified by [`WindowFamily::from_seconds`].
fn window_family(window: &str) -> WindowFamily {
    let seconds = match window {
        "session" => 5 * 3600,
        "weekly" => 7 * 24 * 3600,
        _ => 30 * 24 * 3600,
    };
    WindowFamily::from_seconds(seconds)
}

/// `nextResetTime` as epoch **milliseconds**, used as-is. `0`, absent or unparsable -> `None`.
fn reset_at_millis(limit: &Value) -> Option<i64> {
    let millis = match limit.get("nextResetTime")? {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !millis.is_finite() || millis <= 0.0 {
        return None;
    }
    Some(millis as i64)
}

/// `toPercentage`: clamped `0..=100`, absent -> `0`.
fn percent(limit: &Value) -> f64 {
    opt_number(limit.get("percentage"))
        .unwrap_or(0.0)
        .clamp(0.0, 100.0)
}

fn text(value: Option<&Value>) -> String {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_owned()
}

/// `toNumber`: a finite number or a numeric string; anything else is `None`.
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
    use crate::quota::http::{QuotaHttp, QuotaResponse};
    use crate::quota::registry::QuotaCredential;

    /// Real-shaped captured body: a 5-hour token window with per-model percentages and a
    /// weekly token window with usage detail.
    const SESSION_AND_WEEKLY: &str = r#"{
        "code": 200,
        "success": true,
        "msg": "ok",
        "data": {
            "planName": "GLM Coding Plan",
            "limits": [
                {
                    "type": "TOKENS_LIMIT",
                    "unit": 3,
                    "number": 5,
                    "percentage": 40,
                    "nextResetTime": 1735689600000,
                    "models": [{ "model": "glm-4.6", "percentage": 12 }]
                },
                {
                    "type": "TOKENS_LIMIT",
                    "unit": 6,
                    "number": 1,
                    "percentage": 10,
                    "nextResetTime": 1735776000000,
                    "usageDetails": [{ "modelCode": "glm-4.5", "usage": 3 }]
                }
            ]
        }
    }"#;

    /// A CREDIT_LIMIT row carrying absolute credits alongside the percentage.
    const CREDIT_LIMIT: &str = r#"{
        "code": 200,
        "success": true,
        "data": {
            "planName": "GLM Coding Max",
            "limits": [
                {
                    "type": "CREDIT_LIMIT",
                    "unit": 3,
                    "number": 5,
                    "percentage": 88,
                    "usage": 28000,
                    "currentValue": 24659,
                    "remaining": 3341,
                    "nextResetTime": 999999999999
                }
            ]
        }
    }"#;

    /// A TIME_LIMIT row: the monthly tools window.
    const TIME_LIMIT: &str = r#"{
        "code": 200,
        "success": true,
        "data": {
            "planName": "GLM Coding Plan",
            "limits": [
                {
                    "type": "TIME_LIMIT",
                    "unit": 5,
                    "number": 1,
                    "percentage": 25,
                    "usage": 200,
                    "currentValue": 50,
                    "nextResetTime": 1735689600000
                }
            ]
        }
    }"#;

    /// Two unnamed windows: neither matches a magic pair, so the first is `session` and the
    /// second falls back to `weekly`.
    const FALLBACK_WINDOWS: &str = r#"{
        "code": 200,
        "success": true,
        "data": {
            "limits": [
                { "type": "TOKENS_LIMIT", "unit": 1, "number": 2, "percentage": 5 },
                { "type": "TOKENS_LIMIT", "unit": 1, "number": 2, "percentage": 50 }
            ]
        }
    }"#;

    /// `unit=3, number=168` (24*7) is the weekly window by the range rule.
    const WEEKLY_BY_RANGE: &str = r#"{
        "code": 200,
        "success": true,
        "data": {
            "limits": [
                { "type": "TOKENS_LIMIT", "unit": 3, "number": 168, "percentage": 5 }
            ]
        }
    }"#;

    /// `unit=4, number=7` is the third magic weekly pair.
    const WEEKLY_BY_UNIT_4: &str = r#"{
        "code": 200,
        "success": true,
        "data": {
            "limits": [
                { "type": "TOKENS_LIMIT", "unit": 4, "number": 7, "percentage": 5 }
            ]
        }
    }"#;

    /// The bad-key answer: HTTP 200 with `code: 401` inside the body.
    const CODE_401_IN_200: &str = r#"{
        "code": 401,
        "success": false,
        "msg": "invalid api key"
    }"#;

    #[derive(Debug)]
    struct FakeHttp {
        body: &'static str,
        status: u16,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn ok(body: &'static str) -> Arc<Self> {
            Arc::new(Self {
                body,
                status: 200,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn with_status(status: u16, body: &'static str) -> Arc<Self> {
            Arc::new(Self {
                body,
                status,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("fake http mutex").clone()
        }

        fn only_request(&self) -> QuotaRequest {
            let requests = self.requests();
            assert_eq!(requests.len(), 1, "expected exactly one request");
            requests.into_iter().next().expect("one request")
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

    fn context(
        http: Arc<FakeHttp>,
        provider_id: &str,
        extras: &[(&str, &str)],
    ) -> QuotaFetchContext {
        let mut account = QuotaAccount::new();
        for (key, value) in extras {
            account = account.with_extra(*key, json!(value));
        }
        QuotaFetchContext::new(provider_id, Dialect::OpenAi, http)
            .with_account(account)
            .with_credential(QuotaCredential::new("glm-test-key"))
    }

    fn header(request: &QuotaRequest, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[tokio::test]
    async fn region_follows_account_extra_and_glm_cn_forces_china() {
        // `glm` + no extra -> international.
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("international quota");
        assert_eq!(http.only_request().url, QUOTA_URL_INTERNATIONAL);

        // `glm` + apiRegion=china -> China.
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        Glm::new()
            .fetch(&context(http.clone(), "glm", &[("apiRegion", "china")]))
            .await
            .expect("china quota");
        assert_eq!(http.only_request().url, QUOTA_URL_CHINA);

        // `glm-cn` -> China even with no extra.
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        Glm::new()
            .fetch(&context(http.clone(), "glm-cn", &[]))
            .await
            .expect("forced china quota");
        assert_eq!(http.only_request().url, QUOTA_URL_CHINA);

        // `glm-cn` wins over an explicit international extra.
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        Glm::new()
            .fetch(&context(
                http.clone(),
                "glm-cn",
                &[("apiRegion", "international")],
            ))
            .await
            .expect("forced china quota");
        assert_eq!(http.only_request().url, QUOTA_URL_CHINA);
    }

    #[tokio::test]
    async fn unit_number_pairs_map_to_session_weekly_and_monthly() {
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("session + weekly");
        // The most binding window (session) is the quota itself.
        assert_eq!(quota.display_name.as_deref(), Some("5 Hours Quota"));
        assert_close(quota.used, 40.0);
        assert_close(quota.total, 100.0);
        // Both windows are listed, duration-labelled: 5h and 1w.
        assert!(quota.details.iter().any(|d| d.name == "session (5h)"));
        assert!(quota.details.iter().any(|d| d.name == "weekly (1w)"));
        assert!(quota
            .details
            .iter()
            .any(|d| d.name == "glm-4.6" && d.used == 12.0));

        // unit=4/number=7 -> weekly.
        let http = FakeHttp::ok(WEEKLY_BY_UNIT_4);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("weekly by unit 4");
        assert_eq!(quota.display_name.as_deref(), Some("Weekly Quota"));
        assert!(quota.details.iter().any(|d| d.name == "weekly (1w)"));

        // unit=3/number=168 -> weekly by the range rule.
        let http = FakeHttp::ok(WEEKLY_BY_RANGE);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("weekly by range");
        assert_eq!(quota.display_name.as_deref(), Some("Weekly Quota"));

        // No pair matches: first is session, second falls back to weekly.
        let http = FakeHttp::ok(FALLBACK_WINDOWS);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("fallback windows");
        assert!(quota.details.iter().any(|d| d.name == "session (5h)"));
        assert!(quota.details.iter().any(|d| d.name == "weekly (1w)"));

        // TIME_LIMIT -> mcp_monthly, displayed "Monthly" on a 30d family.
        let http = FakeHttp::ok(TIME_LIMIT);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("monthly tools");
        assert_eq!(quota.display_name.as_deref(), Some("Monthly"));
        assert!(quota.details.iter().any(|d| d.name == "mcp_monthly (30d)"));
        assert_close(quota.used, 50.0);
        assert_close(quota.total, 200.0);
    }

    #[tokio::test]
    async fn next_reset_time_is_milliseconds() {
        // 999_999_999_999 is below the shared parser's `1e12` seconds threshold; if it were
        // run through that heuristic it would become 999_999_999_999_000.
        let http = FakeHttp::ok(CREDIT_LIMIT);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("credit limit");
        assert_eq!(quota.reset_at, Some(999_999_999_999));

        // A realistic millisecond instant survives unchanged, and 0 is "no reset".
        let http = FakeHttp::ok(TIME_LIMIT);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("monthly");
        assert_eq!(quota.reset_at, Some(1_735_689_600_000));

        let body: &'static str = r#"{"code":200,"success":true,
            "data":{"limits":[{"type":"TOKENS_LIMIT","unit":3,"number":5,
            "percentage":1,"nextResetTime":0}]}}"#;
        let http = FakeHttp::ok(body);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("no reset");
        assert_eq!(quota.reset_at, None);
    }

    #[tokio::test]
    async fn credit_limit_prefers_absolute_credits() {
        let http = FakeHttp::ok(CREDIT_LIMIT);
        let quota = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect("credit limit");
        assert_close(quota.total, 28000.0);
        assert_close(quota.used, 24659.0);
        assert_close(quota.remaining, 3341.0);
        assert_close(quota.remaining_percentage, 3341.0 / 28000.0 * 100.0);
    }

    #[tokio::test]
    async fn code_401_inside_http_200_is_an_error() {
        let http = FakeHttp::ok(CODE_401_IN_200);
        let error = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect_err("an envelope 401 must not be read as a quota");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(
            !format!("{error:?}").contains("glm-test-key"),
            "the credential leaked into the error: {error:?}"
        );
    }

    #[tokio::test]
    async fn http_401_and_success_false_are_errors() {
        let http = FakeHttp::with_status(401, r#"{"code":401,"success":false,"msg":"bad key"}"#);
        let error = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect_err("HTTP 401 must fail");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");

        let http = FakeHttp::ok(r#"{"code":500,"success":false,"msg":"coding plan not found"}"#);
        let error = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect_err("success:false must fail");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
        assert!(format!("{error:?}").contains("coding plan not found"));
    }

    #[tokio::test]
    async fn empty_limits_is_an_error() {
        let http = FakeHttp::ok(r#"{"code":200,"success":true,"data":{"limits":[]}}"#);
        let error = Glm::new()
            .fetch(&context(http.clone(), "glm", &[]))
            .await
            .expect_err("no limits must not fabricate a 0/0 quota");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn team_keys_add_type_two_and_bigmodel_headers() {
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        Glm::new()
            .fetch(&context(
                http.clone(),
                "glm",
                &[
                    ("glmOrganizationId", "org-123"),
                    ("glmProjectId", "proj-456"),
                ],
            ))
            .await
            .expect("team quota");

        let request = http.only_request();
        assert_eq!(request.method, Method::GET);
        assert_eq!(request.url, format!("{QUOTA_URL_INTERNATIONAL}?type=2"));
        assert_eq!(
            header(&request, "bigmodel-organization").as_deref(),
            Some("org-123")
        );
        assert_eq!(
            header(&request, "bigmodel-project").as_deref(),
            Some("proj-456")
        );
        assert_eq!(
            header(&request, "authorization").as_deref(),
            Some("Bearer glm-test-key")
        );
        assert_eq!(
            header(&request, "accept").as_deref(),
            Some("application/json")
        );

        // The alias keys work too, and the region extra still applies.
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        Glm::new()
            .fetch(&context(
                http.clone(),
                "glm",
                &[
                    ("apiRegion", "china"),
                    ("bigmodelOrganization", "org-alias"),
                    ("glmProject", "proj-alias"),
                ],
            ))
            .await
            .expect("team quota via aliases");
        let request = http.only_request();
        assert_eq!(request.url, format!("{QUOTA_URL_CHINA}?type=2"));
        assert_eq!(
            header(&request, "bigmodel-organization").as_deref(),
            Some("org-alias")
        );
    }

    #[tokio::test]
    async fn incomplete_team_config_is_a_config_error() {
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        let error = Glm::new()
            .fetch(&context(
                http.clone(),
                "glm",
                &[("glmOrganizationId", "org-123")],
            ))
            .await
            .expect_err("a half-configured team key cannot be queried");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(http.requests().is_empty(), "no request may be sent");
    }

    #[tokio::test]
    async fn missing_credential_is_a_config_error() {
        let http = FakeHttp::ok(SESSION_AND_WEEKLY);
        let ctx = QuotaFetchContext::new("glm", Dialect::OpenAi, http);
        let error = Glm::new()
            .fetch(&ctx)
            .await
            .expect_err("no credential must fail before any request");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
    }

    #[test]
    fn supports_glm_and_glm_cn_only() {
        let account = QuotaAccount::new();
        assert!(Glm::new().supports(Dialect::OpenAi, "glm", &account));
        assert!(Glm::new().supports(Dialect::OpenAi, "glm-cn", &account));
        assert!(!Glm::new().supports(Dialect::OpenAi, "zai", &account));
        assert!(!Glm::new().supports(Dialect::OpenAi, "deepseek", &account));
    }

    #[test]
    fn window_families_are_duration_based() {
        assert_eq!(window_family("session"), WindowFamily::Session5h);
        assert_eq!(window_family("weekly"), WindowFamily::Weekly1w);
        assert_eq!(window_family("mcp_monthly"), WindowFamily::Monthly30d);
        assert_eq!(window_family("session").label(), "5h");
        assert_eq!(window_family("mcp_monthly").label(), "30d");
    }

    #[test]
    fn reset_time_zero_or_absent_is_none() {
        assert_eq!(reset_at_millis(&json!({"nextResetTime": 0})), None);
        assert_eq!(reset_at_millis(&json!({})), None);
        assert_eq!(reset_at_millis(&json!({"nextResetTime": null})), None);
        assert_eq!(
            reset_at_millis(&json!({"nextResetTime": "1735689600000"})),
            Some(1_735_689_600_000)
        );
    }
}
