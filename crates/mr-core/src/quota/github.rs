//! GitHub Copilot subscription usage.
//!
//! Ported from OmniRoute's `open-sse/services/usage/github.ts` and
//! `open-sse/config/providerHeaderProfiles.ts` (`getGitHubCopilotInternalUserHeaders`), with the
//! normalisation rules of `open-sse/services/usage/quota.ts` (MIT, © 2026 diegosouzapw).
//! Implemented by task C1; see `plan.md` section 4, Feature 1.
//!
//! `GET https://api.github.com/copilot_internal/user` is undocumented, and two details of its
//! dialect are silent failures rather than errors:
//!
//! * **The credential goes in a `token ` prefix, not `Bearer `.** The endpoint authenticates the
//!   GitHub OAuth token (not the minted Copilot session token), and GitHub's own convention for a
//!   personal-access/OAuth token on `api.github.com` is `Authorization: token <token>`. Sending
//!   `Bearer` reads as an unsupported scheme and the probe 401s.
//! * **The API-version date and the user-agent/editor triple gate the endpoint.** GitHub rejects or
//!   degrades an unknown client, so these are captured pins ([`GITHUB_API_VERSION`] and friends),
//!   not cosmetics.
//!
//! Two response envelopes are in the wild, and they are *not* translations of each other:
//!
//! * **paid** — `quota_snapshots.{premium_interactions,chat,completions}`, each with counts and an
//!   explicit `unlimited` flag.
//! * **free/limited** — `monthly_quotas[name]` is the **total** and `limited_user_quotas[name]` is
//!   the **remaining** count. Upstream once had this inverted and rendered "0% when not used /
//!   100% when fully used"; [`limited_bucket`] keeps the documented reading (it counts down toward
//!   zero and resets on `limited_user_reset_date`).
//!
//! Every bucket is a **monthly** allowance — they are feature buckets, not durations — so the
//! window family is a month and no 5h/1w family is invented for them.

use std::io;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, UsageQuota, WindowFamily};

/// The undocumented `copilot_internal` user endpoint.
pub const USAGE_URL: &str = "https://api.github.com/copilot_internal/user";

/// The GitHub API version this endpoint is pinned to (`GITHUB_COPILOT_API_VERSION` in OmniRoute's
/// `providerHeaderProfiles.ts`). **Captured, not derived**: the endpoint is undocumented and the
/// date is part of the client fingerprint. It is deliberately *not* the catalogue's inference-time
/// `2025-04-01` — that header belongs to `api.githubcopilot.com`, a different host.
pub const GITHUB_API_VERSION: &str = "2026-08-01";

/// The Copilot client version the probe impersonates. **Captured and version-gated**.
pub const GITHUB_COPILOT_VERSION: &str = "1.0.88";

/// `User-Agent` for the probe. **Captured and version-gated**.
pub const GITHUB_COPILOT_USER_AGENT: &str = "GitHubCopilotChat/1.0.88";

/// `Editor-Version` for the probe. **Captured and version-gated**.
pub const GITHUB_COPILOT_EDITOR_VERSION: &str = "copilot/1.0.88";

/// `Editor-Plugin-Version` for the probe. **Captured and version-gated**.
pub const GITHUB_COPILOT_PLUGIN_VERSION: &str = "copilot-chat/1.0.88";

/// A GitHub bucket is a monthly allowance; this is the length the family is inferred from.
const MONTH_SECONDS: u64 = 30 * 24 * 3600;

/// A hung upstream must not hold a control-plane refresh.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The GitHub Copilot quota fetcher.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHub;

impl GitHub {
    /// The fetcher id, also used as the quota's `source`.
    pub const ID: &str = "github";

    /// Provider ids whose accounts are served by [`USAGE_URL`].
    ///
    /// The catalogue's id is `github-copilot` (the live configuration uses it); `copilot` is
    /// OmniRoute's own name for the family, and the remaining spellings are the obvious aliases.
    /// [`Dialect::Copilot`] is accepted too, so the fetcher still answers if it is registered in
    /// the dialect tier.
    const PROVIDER_IDS: &[&str] = &[
        "github",
        "github-copilot",
        "github_copilot",
        "copilot",
        "gh-copilot",
    ];

    /// A fetcher value, for `QuotaRegistry::register`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for GitHub {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn supports(&self, dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        dialect == Dialect::Copilot
            || Self::PROVIDER_IDS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(provider_id))
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(
                    "github quota: the account has no GitHub OAuth token to present".to_owned(),
                )
            })?;

        // `token `, not `Bearer `: see the module docs. `QuotaRequest::bearer` would be a silent
        // 401 here, which is why the header is built by hand.
        let request = QuotaRequest::get(USAGE_URL)
            .header("Authorization", format!("token {}", credential.expose()))
            .header("Accept", "application/json")
            .header("X-GitHub-Api-Version", GITHUB_API_VERSION)
            .header("User-Agent", GITHUB_COPILOT_USER_AGENT)
            .header("Editor-Version", GITHUB_COPILOT_EDITOR_VERSION)
            .header("Editor-Plugin-Version", GITHUB_COPILOT_PLUGIN_VERSION)
            .timeout(REQUEST_TIMEOUT);

        let response = ctx.http.request(request).await?;
        let status = response.status;
        if status == 401 || status == 403 {
            return Err(Error::Config(format!(
                "github quota: upstream refused the credential (HTTP {status})"
            )));
        }

        let value = match response.json() {
            Ok(value) => value,
            Err(error) => {
                if response.is_success() {
                    return Err(error);
                }
                return Err(Error::Io(io::Error::other(format!(
                    "github quota: upstream returned HTTP {status} with an unreadable body"
                ))));
            }
        };

        // A non-2xx whose body is still a quota payload may be read; anything else that failed is
        // an error rather than a fabricated empty quota.
        if !response.is_success() && !recognised(&value) {
            return Err(Error::Io(io::Error::other(format!(
                "github quota: upstream returned HTTP {status}"
            ))));
        }

        let parsed = buckets(&value);
        let shown: Vec<&Bucket> = parsed
            .iter()
            .filter(|bucket| should_display(bucket))
            .collect();
        // `inferGitHubPlanName` reads the *unfiltered* premium snapshot, so the plan is still
        // labelled when the premium bucket itself is hidden (an unlimited seat).
        let premium_total = parsed
            .iter()
            .find(|bucket| bucket.name == "premium_interactions")
            .map_or(0.0, |bucket| bucket.total);
        let plan = infer_plan(&value, premium_total);

        if shown.is_empty() {
            // Every bucket was hidden. When the payload says the plan is unlimited, that is a real
            // answer (`UsageQuota::unlimited`); otherwise upstream gave no quota at all.
            if parsed.iter().any(|bucket| bucket.unlimited) {
                return Ok(UsageQuota::unlimited()
                    .with_source(USAGE_URL)
                    .with_display_name(plan));
            }
            return Err(Error::NotFound(
                "github usage payload carried no displayable quota bucket".to_owned(),
            ));
        }

        // The governing bucket: premium interactions when it is shown, else the first shown one.
        let mut quota = shown[0]
            .quota()
            .with_source(USAGE_URL)
            .with_display_name(plan);
        for bucket in &shown {
            quota = quota.with_detail(QuotaDetail {
                name: bucket.detail_name(),
                used: bucket.used,
            });
        }
        Ok(quota)
    }
}

/// One feature bucket, after the paid-envelope or free-envelope normalisation.
#[derive(Debug, Clone, PartialEq)]
struct Bucket {
    /// The upstream's bucket name (`premium_interactions`, `chat`, `completions`).
    name: String,
    /// Amount consumed. Meaningful only together with [`Bucket::reported`].
    used: f64,
    /// The bucket's limit; `0` when the upstream reported none.
    total: f64,
    /// The upstream's raw reset value, for [`UsageQuota::from_usage`].
    reset: Value,
    /// `unlimited` as the payload stated it — never inferred.
    unlimited: bool,
    /// Whether the payload reported the remaining fraction. `false` means `used`/`total` are all
    /// that was said, so the quota is built with the unreported constructor.
    reported: bool,
    /// Whether the payload carried `percent_remaining` itself (upstream's display gate reads this
    /// field, not the value it derives from other counts).
    percent_reported: bool,
}

impl Bucket {
    /// The normalised quota for this bucket.
    fn quota(&self) -> UsageQuota {
        let mut quota = if self.reported {
            UsageQuota::from_usage(self.used, self.total, &self.reset)
        } else {
            UsageQuota::from_usage_unreported(self.used, self.total, &self.reset)
        };
        quota.unlimited = self.unlimited;
        quota
    }

    /// The operator-facing row: the bucket name and the family every GitHub bucket belongs to.
    fn detail_name(&self) -> String {
        format!(
            "{} ({})",
            self.name,
            WindowFamily::from_seconds(MONTH_SECONDS).label()
        )
    }
}

/// `shouldDisplayGitHubQuota`: a bucket is hidden when the plan is unlimited *and* no total or
/// percentage is known — the free `{"unlimited": true}`-style entries that would otherwise render
/// as a permanent empty row. Upstream's second clause, `total > 0 || remainingPercentage !==
/// undefined`, is why a percentage-only bucket is shown (it is promoted to `total = 100` first).
fn should_display(bucket: &Bucket) -> bool {
    if bucket.unlimited && bucket.total <= 0.0 {
        return false;
    }
    bucket.total > 0.0 || bucket.percent_reported
}

/// The first non-null alias of `keys` on `record` — upstream's `getFieldValue`.
fn field<'a>(record: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    let object = record.as_object()?;
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find(|value| !value.is_null())
}

/// [`field`] restricted to object values — upstream's `toRecord`, which never throws on a scalar.
fn object<'a>(record: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    field(record, keys).filter(|value| value.is_object())
}

/// `toNumber`: a finite number, or a numeric string, and nothing else.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

/// Whether the body is one of the two quota envelopes — the test for reading a non-2xx response.
fn recognised(data: &Value) -> bool {
    field(data, &["quota_snapshots", "quotaSnapshots"]).is_some()
        || field(data, &["monthly_quotas", "monthlyQuotas"]).is_some()
        || field(data, &["limited_user_quotas", "limitedUserQuotas"]).is_some()
}

/// The bucket names upstream reads, in its order.
const BUCKET_NAMES: &[&str] = &["premium_interactions", "chat", "completions"];

/// Every bucket in the payload, whichever envelope it used.
fn buckets(data: &Value) -> Vec<Bucket> {
    if let Some(snapshots) = object(data, &["quota_snapshots", "quotaSnapshots"]) {
        let reset = reset_value(field(data, &["quota_reset_date", "quotaResetDate"]));
        return BUCKET_NAMES
            .iter()
            .filter_map(|name| paid_bucket(name, snapshots.get(*name), &reset))
            .collect();
    }
    if let Some(monthly) = object(data, &["monthly_quotas", "monthlyQuotas"]) {
        let remaining = object(data, &["limited_user_quotas", "limitedUserQuotas"]);
        let reset = reset_value(field(
            data,
            &["limited_user_reset_date", "limitedUserResetDate"],
        ));
        return BUCKET_NAMES
            .iter()
            .filter_map(|name| limited_bucket(name, monthly, remaining, &reset))
            .collect();
    }
    Vec::new()
}

/// `formatGitHubQuotaSnapshot` for one paid-plan bucket.
///
/// The axes are taken verbatim where they are finite and derived where they are not, in upstream's
/// order: `used` from `total - remaining`, `remaining` from `total - used`, the percentage from
/// `remaining / total`. A bucket whose total is unknown but whose percentage is known is promoted
/// to `total = 100` (upstream does the same, which is what makes the percentage-only shape
/// renderable). `unlimited` is only ever what the payload said.
fn paid_bucket(name: &str, snapshot: Option<&Value>, reset: &Value) -> Option<Bucket> {
    let snapshot = snapshot?;
    let source = snapshot.as_object()?;
    if source.is_empty() {
        return None;
    }

    let unlimited = source.get("unlimited").and_then(Value::as_bool) == Some(true);
    let entitlement = number(source.get("entitlement"));
    let total_value = number(source.get("total"));
    let remaining_value = number(source.get("remaining"));
    let used_value = number(source.get("used"));
    let percent_value = number(field(snapshot, &["percent_remaining", "percentRemaining"]));

    let mut total = total_value.or(entitlement).unwrap_or(0.0).max(0.0);
    let mut remaining = remaining_value.map(|value| value.max(0.0));
    let mut used = used_value.map(|value| value.max(0.0));
    let mut percentage = percent_value.map(|value| value.clamp(0.0, 100.0));
    let percent_reported = percentage.is_some();
    let reported = remaining.is_some() || used.is_some() || percentage.is_some();

    if used.is_none() && total > 0.0 {
        if let Some(remaining) = remaining {
            used = Some((total - remaining).max(0.0));
        }
    }
    if remaining.is_none() && total > 0.0 {
        if let Some(used) = used {
            remaining = Some((total - used).max(0.0));
        }
    }
    if percentage.is_none() && total > 0.0 {
        if let Some(remaining) = remaining {
            percentage = Some(((remaining / total) * 100.0).clamp(0.0, 100.0));
        }
    }
    if total <= 0.0 && percentage.is_some() {
        total = 100.0;
        used = Some(100.0 - percentage.unwrap_or(0.0));
    }

    Some(Bucket {
        name: name.to_owned(),
        used: used.unwrap_or(0.0).max(0.0),
        total,
        reset: reset.clone(),
        unlimited,
        reported,
        percent_reported,
    })
}

/// The free/limited envelope for one bucket.
///
/// `monthly_quotas[name]` is the **total** and `limited_user_quotas[name]` is the **remaining**
/// count — the reading upstream's #2876 fix settled on, after an earlier version treated the
/// remaining count as the used count and showed a fresh account at 0% and a spent one at 100%.
///
/// One deliberate difference from upstream: when `limited_user_quotas` does not carry the name at
/// all, upstream substitutes `0`, which renders as a fully exhausted bucket. An absent field is not
/// a reported zero, so this port marks the fraction unreported instead and lets
/// `fraction_reported == false` say so.
fn limited_bucket(
    name: &str,
    monthly: &Value,
    remaining_quotas: Option<&Value>,
    reset: &Value,
) -> Option<Bucket> {
    let total = number(monthly.get(name)).unwrap_or(0.0);
    if total <= 0.0 {
        return None;
    }
    let remaining = remaining_quotas.and_then(|quotas| number(quotas.get(name)));
    let (used, reported) = match remaining {
        Some(remaining) => (total - remaining.clamp(0.0, total), true),
        None => (0.0, false),
    };
    Some(Bucket {
        name: name.to_owned(),
        used,
        total,
        reset: reset.clone(),
        unlimited: false,
        reported,
        percent_reported: reported,
    })
}

/// The reset instant to hand [`UsageQuota::from_usage`].
///
/// `quota_reset_date` and `limited_user_reset_date` are **date-only** (`2026-09-01`) in the
/// captured responses, and upstream's JavaScript reads them through `new Date(...)`, which resolves
/// a date-only ISO string to UTC midnight. The shared normaliser accepts only shapes that carry
/// their own clock, so the zero clock is completed here — that is the same reading of the same
/// string, not an invented instant — and every other shape is passed through untouched.
fn reset_value(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(text)) if is_date_only(text.trim()) => {
            json!(format!("{}T00:00:00Z", text.trim()))
        }
        Some(value) => value.clone(),
        None => Value::Null,
    }
}

/// Whether `text` is exactly `YYYY-MM-DD`.
fn is_date_only(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
}

/// `inferGitHubPlanName`: the plan label, from the payload's own plan/SKU strings where they say
/// something and from the entitlement sizes where they do not.
fn infer_plan(data: &Value, premium_total: f64) -> String {
    let raw_plan = field(data, &["copilot_plan", "copilotPlan"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let raw_sku = field(data, &["access_type_sku", "accessTypeSku"])
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let combined = format!("{raw_sku} {raw_plan}").trim().to_uppercase();

    let monthly = object(data, &["monthly_quotas", "monthlyQuotas"]);
    let premium_total = if premium_total > 0.0 {
        premium_total
    } else {
        monthly
            .and_then(|quotas| {
                number(quotas.get("premium_interactions"))
                    .or_else(|| number(quotas.get("premiumInteractions")))
            })
            .unwrap_or(0.0)
    };
    let chat_total = monthly
        .and_then(|quotas| number(quotas.get("chat")))
        .unwrap_or(0.0);

    if combined.contains("PRO+") || combined.contains("PRO_PLUS") || combined.contains("PROPLUS") {
        return "Copilot Pro+".to_owned();
    }
    if combined.contains("ENTERPRISE") {
        return "Copilot Enterprise".to_owned();
    }
    if combined.contains("BUSINESS") {
        return "Copilot Business".to_owned();
    }
    if combined.contains("STUDENT") {
        return "Copilot Student".to_owned();
    }
    if combined.contains("FREE") {
        return "Copilot Free".to_owned();
    }
    if combined.contains("PRO") {
        return "Copilot Pro".to_owned();
    }

    if premium_total >= 1400.0 {
        return "Copilot Pro+".to_owned();
    }
    if premium_total >= 900.0 {
        return "Copilot Enterprise".to_owned();
    }
    if premium_total >= 250.0 {
        if combined.contains("INDIVIDUAL") {
            return "Copilot Pro".to_owned();
        }
        return "Copilot Business".to_owned();
    }
    if premium_total > 0.0 || chat_total == 50.0 {
        return "Copilot Free".to_owned();
    }

    if !raw_sku.is_empty() {
        let label = to_display_label(raw_sku);
        if !label.is_empty() {
            return format!("Copilot {label}");
        }
    }
    if !raw_plan.is_empty() {
        let label = to_display_label(raw_plan);
        if !label.is_empty() {
            return format!("Copilot {label}");
        }
    }
    "GitHub Copilot".to_owned()
}

/// `toDisplayLabel` from `usage/scalars.ts`: drop a leading `copilot` token, split on whitespace /
/// `_` / `-`, capitalise the parts that are plain lowercase words, title-case `pro+`.
fn to_display_label(value: &str) -> String {
    strip_copilot_prefix(value)
        .split(|character: char| character.is_whitespace() || character == '_' || character == '-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part.eq_ignore_ascii_case("pro+") {
                return "Pro+".to_owned();
            }
            // Upstream tests `/^[a-z]{2,}$/` against the *original* part, so an all-uppercase
            // token is left alone rather than being normalised.
            if part.len() >= 2 && part.bytes().all(|byte| byte.is_ascii_lowercase()) {
                let mut characters = part.chars();
                return match characters.next() {
                    Some(first) => {
                        format!("{}{}", first.to_ascii_uppercase(), characters.as_str())
                    }
                    None => String::new(),
                };
            }
            part.to_owned()
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}

/// `/^copilot[_\s-]*/i`: a leading `copilot` token and any run of separators after it.
fn strip_copilot_prefix(value: &str) -> &str {
    match value.get(..7) {
        Some(head) if head.eq_ignore_ascii_case("copilot") => {
            let bytes = value.as_bytes();
            let mut cut = 7;
            while cut < bytes.len() && matches!(bytes[cut], b'_' | b'-' | b' ' | b'\t') {
                cut += 1;
            }
            &value[cut..]
        }
        _ => value,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};

    /// A real-shaped paid-plan capture: one metered premium bucket plus the two unlimited
    /// feature buckets a seat reports with no percentage at all.
    const PAID_PAYLOAD: &str = r#"{
      "login": "octocat",
      "copilot_plan": "business",
      "access_type_sku": "business_seat",
      "quota_reset_date": "2026-09-01",
      "quota_snapshots": {
        "premium_interactions": {
          "entitlement": 300,
          "remaining": 150,
          "percent_remaining": 50,
          "unlimited": false
        },
        "chat": { "entitlement": 0, "unlimited": true },
        "completions": { "unlimited": true }
      }
    }"#;

    /// A paid capture whose buckets each report a different subset of the axes.
    const PARTIAL_PAYLOAD: &str = r#"{
      "quota_snapshots": {
        "premium_interactions": { "total": 400, "used": 100 },
        "chat": { "entitlement": 50, "remaining": 20 },
        "completions": { "percent_remaining": 25 }
      }
    }"#;

    /// The free/limited envelope: `monthly_quotas` is the total, `limited_user_quotas` the
    /// remaining count.
    const FREE_PAYLOAD: &str = r#"{
      "login": "octocat",
      "copilot_plan": "individual",
      "access_type_sku": "free_limited_copilot",
      "monthly_quotas": { "chat": 50, "completions": 50 },
      "limited_user_quotas": { "chat": 12, "completions": 45 },
      "limited_user_reset_date": "2026-07-01"
    }"#;

    /// A transport double: records every request and replays one canned reply. It holds no socket
    /// and never can.
    #[derive(Debug)]
    struct FakeHttp {
        status: u16,
        body: &'static str,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn serving(body: &'static str) -> Arc<Self> {
            Arc::new(Self {
                status: 200,
                body,
                seen: Mutex::new(Vec::new()),
            })
        }

        fn failing(status: u16) -> Arc<Self> {
            Arc::new(Self {
                status,
                body: "{}",
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("not poisoned").clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen.lock().expect("not poisoned").push(request);
            Ok(QuotaResponse {
                status: self.status,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: Bytes::from(self.body),
            })
        }
    }

    fn header<'a>(request: &'a QuotaRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn fetcher() -> GitHub {
        GitHub::new()
    }

    fn context(http: Arc<FakeHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new(GitHub::ID, Dialect::Copilot, http)
            .with_account(QuotaAccount::new())
            .with_credential("ghu_test_github_token")
    }

    fn payload(text: &str) -> Value {
        serde_json::from_str(text).expect("the canned payload is valid JSON")
    }

    fn names(quota: &UsageQuota) -> Vec<&str> {
        quota.details.iter().map(|row| row.name.as_str()).collect()
    }

    #[test]
    fn header_constants_cannot_drift_apart() {
        assert_eq!(
            GITHUB_COPILOT_USER_AGENT,
            format!("GitHubCopilotChat/{GITHUB_COPILOT_VERSION}")
        );
        assert_eq!(
            GITHUB_COPILOT_EDITOR_VERSION,
            format!("copilot/{GITHUB_COPILOT_VERSION}")
        );
        assert_eq!(
            GITHUB_COPILOT_PLUGIN_VERSION,
            format!("copilot-chat/{GITHUB_COPILOT_VERSION}")
        );
    }

    #[test]
    fn supports_claims_the_configured_provider_ids() {
        let github = fetcher();
        let account = QuotaAccount::new();
        for id in ["github", "github-copilot", "copilot", "GITHUB-COPILOT"] {
            assert!(
                github.supports(Dialect::Copilot, id, &account),
                "expected {id} to be claimed"
            );
        }
        assert!(github.supports(Dialect::Copilot, "anything", &account));
        assert!(!github.supports(Dialect::OpenAi, "openai-codex", &account));
        assert!(!github.supports(Dialect::Codex, "codex", &account));
    }

    #[tokio::test]
    async fn the_credential_is_sent_with_a_token_prefix_not_bearer() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = context(Arc::clone(&http));

        fetcher().fetch(&ctx).await.expect("the payload parses");

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.url, USAGE_URL);
        assert_eq!(request.method, http::Method::GET);
        assert!(request.body.is_empty());

        let authorization = header(request, "Authorization").expect("authorization is sent");
        assert_eq!(authorization, "token ghu_test_github_token");
        assert!(authorization.starts_with("token "), "{authorization}");
        assert!(
            !authorization.starts_with("Bearer "),
            "the endpoint rejects the Bearer scheme"
        );

        assert_eq!(header(request, "Accept"), Some("application/json"));
        assert_eq!(
            header(request, "X-GitHub-Api-Version"),
            Some(GITHUB_API_VERSION)
        );
        assert_eq!(
            header(request, "User-Agent"),
            Some(GITHUB_COPILOT_USER_AGENT)
        );
        assert_eq!(
            header(request, "Editor-Version"),
            Some(GITHUB_COPILOT_EDITOR_VERSION)
        );
        assert_eq!(
            header(request, "Editor-Plugin-Version"),
            Some(GITHUB_COPILOT_PLUGIN_VERSION)
        );
    }

    #[tokio::test]
    async fn the_paid_envelope_maps_its_buckets() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = context(Arc::clone(&http));

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.source.as_deref(), Some(USAGE_URL));
        assert_eq!(quota.display_name.as_deref(), Some("Copilot Business"));
        assert_eq!(quota.used, 150.0);
        assert_eq!(quota.total, 300.0);
        assert_eq!(quota.remaining, 150.0);
        assert_eq!(quota.remaining_percentage, 50.0);
        assert!(quota.fraction_reported);
        assert!(!quota.unlimited);
        assert_eq!(
            quota.reset_at,
            Some(1_788_220_800_000),
            "2026-09-01T00:00:00Z as epoch milliseconds"
        );
        assert_eq!(names(&quota), vec!["premium_interactions (30d)"]);
    }

    #[tokio::test]
    async fn the_free_envelope_treats_limited_user_quotas_as_remaining() {
        let http = FakeHttp::serving(FREE_PAYLOAD);
        let ctx = context(Arc::clone(&http));

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.display_name.as_deref(), Some("Copilot Free"));
        // chat: 50 total, 12 remaining -> 38 used, 24% remaining.
        assert_eq!(quota.used, 38.0);
        assert_eq!(quota.total, 50.0);
        assert_eq!(quota.remaining, 12.0);
        assert_eq!(quota.remaining_percentage, 24.0);
        assert!(quota.fraction_reported);
        assert_eq!(
            quota.reset_at,
            Some(1_782_864_000_000),
            "2026-07-01T00:00:00Z as epoch milliseconds"
        );
        assert_eq!(
            names(&quota),
            vec!["chat (30d)", "completions (30d)"],
            "an account that has used 12 of 50 chat requests must not read as 76% used"
        );
    }

    #[test]
    fn missing_axes_are_derived_from_the_ones_that_are_present() {
        let parsed = buckets(&payload(PARTIAL_PAYLOAD));
        let bucket = |name: &str| {
            parsed
                .iter()
                .find(|bucket| bucket.name == name)
                .expect("the bucket is parsed")
        };

        // total + used -> remaining and percentage derived.
        let premium = bucket("premium_interactions");
        assert_eq!(premium.total, 400.0);
        assert_eq!(premium.used, 100.0);
        assert_eq!(
            premium.quota().remaining_percentage,
            75.0,
            "remaining/percentage come from the counts"
        );

        // entitlement + remaining -> the total alias and the used count derived.
        let chat = bucket("chat");
        assert_eq!(chat.total, 50.0);
        assert_eq!(chat.used, 30.0);

        // percentage only -> promoted to a 100-point scale.
        let completions = bucket("completions");
        assert_eq!(completions.total, 100.0);
        assert_eq!(completions.used, 75.0);
        assert_eq!(completions.quota().remaining_percentage, 25.0);
        assert!(completions.percent_reported);
    }

    #[tokio::test]
    async fn a_bucket_with_only_a_total_reports_an_unreported_fraction() {
        let http = FakeHttp::serving(
            r#"{"quota_snapshots": {"premium_interactions": {"entitlement": 300}}}"#,
        );
        let ctx = context(Arc::clone(&http));

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.total, 300.0);
        assert!(
            !quota.fraction_reported,
            "an entitlement with no remaining/used is not a reported 0%"
        );
        assert_eq!(quota.remaining_fraction(), None);
    }

    #[tokio::test]
    async fn an_absent_remaining_count_is_not_a_reported_zero() {
        let http = FakeHttp::serving(r#"{"monthly_quotas": {"chat": 50}}"#);
        let ctx = context(Arc::clone(&http));

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.total, 50.0);
        assert!(
            !quota.fraction_reported,
            "`limited_user_quotas` omitting the name is not the same as it saying 0"
        );
    }

    #[tokio::test]
    async fn a_hidden_bucket_is_not_a_detail_row() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = context(Arc::clone(&http));

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        let shown = names(&quota);
        for hidden in ["chat", "completions"] {
            assert!(
                !shown.iter().any(|name| name.starts_with(hidden)),
                "`unlimited` with no total or percentage is hidden: {shown:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unlimited_plan_with_no_buckets_is_the_unlimited_quota() {
        let http = FakeHttp::serving(
            r#"{
              "copilot_plan": "business",
              "quota_snapshots": {
                "premium_interactions": { "unlimited": true },
                "chat": { "unlimited": true },
                "completions": { "unlimited": true }
              }
            }"#,
        );
        let ctx = context(Arc::clone(&http));

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert!(quota.unlimited);
        assert_eq!(quota.display_name.as_deref(), Some("Copilot Business"));
        assert!(names(&quota).is_empty());
        assert_eq!(quota.remaining_fraction(), None);
    }

    #[test]
    fn the_display_gate_hides_only_unlimited_without_a_fraction() {
        let bucket = |unlimited: bool, total: f64, percent: bool| Bucket {
            name: "chat".to_owned(),
            used: 0.0,
            total,
            reset: Value::Null,
            unlimited,
            reported: percent,
            percent_reported: percent,
        };
        assert!(!should_display(&bucket(true, 0.0, false)));
        assert!(should_display(&bucket(true, 100.0, true)));
        assert!(
            !should_display(&bucket(true, 0.0, true)),
            "unlimited wins over the percentage clause; the parser promotes a percentage-only \
             bucket to total = 100 first, so this combination cannot reach the gate"
        );
        assert!(should_display(&bucket(false, 0.0, true)));
        assert!(!should_display(&bucket(false, 0.0, false)));
    }

    #[test]
    fn the_plan_label_is_inferred_from_plan_sku_or_entitlement() {
        let plan = |json_text: &str, premium: f64| infer_plan(&payload(json_text), premium);
        assert_eq!(plan(r#"{"copilot_plan": "pro_plus"}"#, 0.0), "Copilot Pro+");
        assert_eq!(
            plan(r#"{"access_type_sku": "enterprise_seat"}"#, 0.0),
            "Copilot Enterprise"
        );
        assert_eq!(
            plan(r#"{"copilot_plan": "business"}"#, 0.0),
            "Copilot Business"
        );
        assert_eq!(
            plan(r#"{"copilot_plan": "student"}"#, 0.0),
            "Copilot Student"
        );
        assert_eq!(
            plan(r#"{"access_type_sku": "free_limited_copilot"}"#, 0.0),
            "Copilot Free"
        );
        assert_eq!(
            plan(r#"{"copilot_plan": "individual_pro"}"#, 0.0),
            "Copilot Pro"
        );
        // No usable string: the entitlement size decides.
        assert_eq!(plan("{}", 1500.0), "Copilot Pro+");
        assert_eq!(plan("{}", 1000.0), "Copilot Enterprise");
        assert_eq!(plan("{}", 300.0), "Copilot Business");
        assert_eq!(
            plan(r#"{"copilot_plan": "individual"}"#, 300.0),
            "Copilot Pro"
        );
        assert_eq!(
            plan(r#"{"monthly_quotas": {"chat": 50}}"#, 0.0),
            "Copilot Free"
        );
        // Last resort: the SKU or plan string, title-cased.
        assert_eq!(
            plan(r#"{"access_type_sku": "my_custom_sku"}"#, 0.0),
            "Copilot My Custom Sku"
        );
        assert_eq!(plan("{}", 0.0), "GitHub Copilot");
    }

    #[test]
    fn to_display_label_matches_the_javascript_rule() {
        assert_eq!(
            to_display_label("free_limited_copilot"),
            "Free Limited Copilot",
            "only a leading `copilot` token is stripped"
        );
        assert_eq!(to_display_label("COPILOT-business"), "Business");
        assert_eq!(to_display_label("pro+"), "Pro+");
        assert_eq!(to_display_label("PRO"), "PRO");
        assert_eq!(to_display_label("copilot"), "");
    }

    #[tokio::test]
    async fn unauthorized_is_an_error() {
        let http = FakeHttp::failing(401);
        let ctx = context(Arc::clone(&http));
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
    }

    #[tokio::test]
    async fn forbidden_is_an_error() {
        let http = FakeHttp::failing(403);
        let ctx = context(Arc::clone(&http));
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
    }

    #[tokio::test]
    async fn an_unrecognised_payload_is_an_error_not_an_empty_quota() {
        let http = FakeHttp::serving(r#"{"message": "Not Found"}"#);
        let ctx = context(Arc::clone(&http));
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }

    #[tokio::test]
    async fn a_missing_credential_is_a_configuration_error() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = QuotaFetchContext::new(GitHub::ID, Dialect::Copilot, http.clone());
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
        assert!(
            http.requests().is_empty(),
            "nothing is sent without a token"
        );
    }
}
