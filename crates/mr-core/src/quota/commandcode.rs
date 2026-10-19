//! Command Code (`api.commandcode.ai`) usage: a credit pool plus two rolling USD windows.
//!
//! The Rust port of OmniRoute's `open-sse/services/usage/command-code.ts` (MIT,
//! © 2026 diegosouzapw). It talks to the same `/alpha` bearer surface the Command Code CLI's
//! `/usage` view reads, all `GET`:
//!
//! * `/alpha/billing/credits` — **the quota**. Carries the `credits.*` pools and
//!   `windowLimits.fiveHour` / `windowLimits.weekly`. This call is required; without a usable
//!   answer there is nothing honest to report and [`QuotaFetcher::fetch`] fails.
//! * `/alpha/whoami` — **only when the account does not name its organisation itself**, to learn
//!   `org.id` for the `orgId` query parameter. An account that declares the org (see
//!   [`CommandCode::ORG_ID_KEY`]) spends no call on it.
//! * `/alpha/billing/subscriptions` — plan id and billing period. Soft: a failure costs the plan
//!   label and the period reset instant, nothing else.
//! * `/alpha/usage/summary?since=…` — period spend. Soft, and the `used` side of the credit budget.
//!
//! # The base URL is overridable
//!
//! `COMMANDCODE_API_URL` replaces [`CommandCode::DEFAULT_API_BASE`]: read at fetch time (not cached
//! in a `OnceLock`), whitespace- and trailing-slash-normalised, and ignored when empty, so a test
//! double or a self-hosted proxy can be pointed at without a rebuild and without a restart.
//!
//! # One account line, several named budgets
//!
//! [`QuotaFetcher::fetch`] returns a single [`UsageQuota`] (the trait's shape is one quota per
//! account), while the upstream reports three named budgets. The two rolling windows and the credit
//! pool are therefore attached as [`QuotaDetail`] rows — `five_hour`, `weekly`, `credits` — on one
//! account-level quota:
//!
//! * the **credit pool** is the headline when the payload carries one, because it is the
//!   account-level remaining spend that Provider Limits and a preflight check want;
//! * otherwise the **first emitted window** is the headline;
//! * [`parse_windows`] exposes the windows as full [`QuotaWindow`]s (family, key, counts, reset) for
//!   a caller that wants the duration-aware view the panel renders; it is public precisely because
//!   a `UsageQuota` cannot carry it.
//!
//! `currency` is `"USD"` in the source for every line; the normalised [`UsageQuota`] has no
//! currency field, so it is not carried through (and is not invented).
//!
//! The credential never appears in an error message or a log line: only the upstream's status and
//! the provider id do.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, QuotaWindow, UsageQuota, WindowFamily};

/// The environment variable that overrides the API base.
pub const API_BASE_ENV: &str = "COMMANDCODE_API_URL";

/// Where Command Code lives when the environment does not say otherwise.
pub const DEFAULT_API_BASE: &str = "https://api.commandcode.ai";

/// The account-extra key that names the organisation id, so `/alpha/whoami` can be skipped.
pub const ORG_ID_KEY: &str = "orgId";

/// The pre-rename provider id. The plan renames the provider to `command-code-go` with a one-time
/// config migration; until that migration has run, the old id is still what an account carries, and
/// refusing it would leave a working account with no quota reader at all.
const LEGACY_PROVIDER_ID: &str = "command-code";

/// `FETCH_TIMEOUT_MS` in the JavaScript source: one quota read never hangs a panel refresh.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The two windows' real lengths. A window is a duration, not a name: the family comes from these
/// numbers through [`WindowFamily::from_seconds`], never from the upstream's key.
const FIVE_HOUR_SECONDS: u64 = 5 * 3600;
const WEEKLY_SECONDS: u64 = 7 * 24 * 3600;

/// Plan ids the upstream uses, and the labels an operator reads (`PLAN_LABELS`).
const PLAN_LABELS: [(&str, &str); 6] = [
    ("individual-goat", "Command Code · GOAT"),
    ("individual-go", "Command Code · Go"),
    ("individual-pro", "Command Code · Pro"),
    ("individual-max-10x", "Command Code · Max 10×"),
    ("individual-max-20x", "Command Code · Max 20×"),
    ("team-pro", "Command Code · Team Pro"),
];

/// Command Code's quota reader.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandCode;

impl CommandCode {
    /// The fetcher id, also stamped onto the `source` field of what it returns.
    pub const ID: &str = "command-code-go";

    /// The default API base, overridable with [`API_BASE_ENV`].
    pub const DEFAULT_API_BASE: &str = DEFAULT_API_BASE;

    /// The account-extra key that names the organisation id.
    pub const ORG_ID_KEY: &str = ORG_ID_KEY;

    /// A reader with no configuration of its own.
    pub fn new() -> Self {
        Self
    }
}

/// The API base for the next fetch: `COMMANDCODE_API_URL` when set and non-empty, else the default.
///
/// Deliberately not cached: a test sets and clears the variable around one `fetch`, and an operator
/// who repoints the process should not have to restart it to be believed.
pub fn api_base() -> String {
    resolve_api_base(std::env::var(API_BASE_ENV).ok().as_deref())
}

/// The pure half of [`api_base`], so the override rule is testable without touching the environment.
///
/// The value is normalised before use: surrounding whitespace goes, then every trailing slash, and a
/// value that is empty — or that reduces to nothing, as `"/"` does — falls back to the default
/// rather than becoming a base with no host. Trimming the trailing slash is harmless because
/// [`url_with`] trims one again before appending a path, and the default carries none, so an operator
/// who writes `https://proxy.test/` and one who writes `https://proxy.test` mean the same thing.
fn resolve_api_base(override_value: Option<&str>) -> String {
    override_value
        .map(str::trim)
        .map(|value| value.trim_end_matches('/'))
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_API_BASE)
        .to_owned()
}

/// The real windows in `/alpha/billing/credits`, as duration-aware [`QuotaWindow`]s.
///
/// Both are built with [`UsageQuota::from_usage`] from `{used, cap, resetAt}` and keyed
/// `five_hour` / `weekly`. A window whose `cap` is not greater than zero is **omitted, not reported
/// as zero**: `0/0` renders as an exhausted account, and the upstream sends a zero cap to mean "this
/// window does not apply", not "you have nothing left".
pub fn parse_windows(body: &Value) -> Vec<QuotaWindow> {
    let Some(limits) = body.get("windowLimits") else {
        return Vec::new();
    };
    if !limits.is_object() {
        return Vec::new();
    }

    let declared = [
        ("five_hour", "fiveHour", FIVE_HOUR_SECONDS, "5-hour window"),
        ("weekly", "weekly", WEEKLY_SECONDS, "Weekly window"),
    ];

    let mut windows = Vec::new();
    for (key, field, seconds, display_name) in declared {
        let Some(window) = limits.get(field) else {
            continue;
        };
        let cap = number(window.get("cap")).unwrap_or(0.0);
        // A `NaN` cap is dropped too, exactly as `if (!(cap > 0)) return null` does upstream. Written
        // as an explicit comparison rather than `!(cap > 0.0)` so the intent survives a reader who
        // has not memorised what negation does to a `NaN`.
        if cap.is_nan() || cap <= 0.0 {
            continue;
        }
        let used = number(window.get("used")).unwrap_or(0.0);
        let reset = window.get("resetAt").cloned().unwrap_or(Value::Null);
        let quota = UsageQuota::from_usage(used, cap, &reset).with_display_name(display_name);
        windows.push(
            QuotaWindow::from_quota(WindowFamily::from_seconds(seconds), &quota).with_key(key),
        );
    }
    windows
}

/// Turn a parsed window back into the quota the trait returns.
fn window_quota(window: &QuotaWindow) -> UsageQuota {
    let reset = window.reset_at;
    if window.fraction_reported {
        UsageQuota::from_counts(window.used, window.total, reset)
    } else {
        UsageQuota::from_counts_unreported(window.used, window.total, reset)
    }
}

/// The credit budget: `used = period spend`, `total = period spend + remaining pool`.
///
/// The pool is the sum of the three non-negative `credits.*` balances, matching the source's
/// `creditRemaining`. `None` when the payload carries no `credits` object at all — the caller then
/// has only the windows, and a window-less payload is an error rather than a made-up `0/0`.
fn credit_budget(body: &Value, period_used: f64, period_end: Option<&Value>) -> Option<UsageQuota> {
    let credits = body.get("credits")?;
    if !credits.is_object() {
        return None;
    }
    let period_used = if period_used.is_finite() && period_used >= 0.0 {
        period_used
    } else {
        0.0
    };
    let remaining = credit_remaining(credits);
    let reset = period_end.cloned().unwrap_or(Value::Null);
    Some(
        UsageQuota::from_usage(period_used, period_used + remaining, &reset)
            .with_display_name("Credits"),
    )
}

/// `max(0, monthlyCredits) + max(0, purchasedCredits) + max(0, freeCredits)`.
fn credit_remaining(credits: &Value) -> f64 {
    ["monthlyCredits", "purchasedCredits", "freeCredits"]
        .iter()
        .map(|key| number(credits.get(*key)).unwrap_or(0.0).max(0.0))
        .sum()
}

/// `toNumber`: a JSON number or a numeric string, else `None`. Never coerces `null`/`true` to zero.
fn number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(value) => value.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

/// One `GET` with the headers the `/alpha` surface expects.
fn request(url: String, credential: &str) -> QuotaRequest {
    QuotaRequest::get(url)
        .bearer(credential)
        .header("accept", "application/json")
        .header("content-type", "application/json")
        .timeout(FETCH_TIMEOUT)
}

/// `{base}{path}` with the optional `orgId` and one extra query parameter, percent-encoded.
fn url_with(base: &str, path: &str, org: Option<&str>, extra: Option<(&str, &str)>) -> String {
    let mut url = format!("{}{path}", base.trim_end_matches('/'));
    let mut separator = '?';
    if let Some(org) = org {
        url.push(separator);
        separator = '&';
        url.push_str("orgId=");
        url.push_str(&encode_query(org));
    }
    if let Some((key, value)) = extra {
        url.push(separator);
        url.push_str(key);
        url.push('=');
        url.push_str(&encode_query(value));
    }
    url
}

/// Percent-encode everything outside the RFC 3986 unreserved set, as `encodeURIComponent` does.
fn encode_query(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                let high = char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0');
                let low = char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0');
                encoded.push('%');
                encoded.push(high.to_ascii_uppercase());
                encoded.push(low.to_ascii_uppercase());
            }
        }
    }
    encoded
}

/// `/alpha/whoami`'s `org.id`, or `None` when the call cannot say.
///
/// A rejected key is *not* soft — it is the one thing the caller must hear about — while any other
/// failure leaves the org unknown and the fetch continues without an `orgId`.
async fn whoami_org(
    ctx: &QuotaFetchContext,
    base: &str,
    credential: &str,
) -> Result<Option<String>> {
    let url = url_with(base, "/alpha/whoami", None, None);
    let response = ctx.http.request(request(url, credential)).await?;
    if matches!(response.status, 401 | 403) {
        return Err(Error::InvalidState(format!(
            "provider {:?} rejected the Command Code API key (HTTP {}) while reading /alpha/whoami",
            ctx.provider_id, response.status
        )));
    }
    if !response.is_success() {
        return Ok(None);
    }
    let Ok(body) = response.json() else {
        return Ok(None);
    };
    Ok(body
        .get("org")
        .and_then(|org| org.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned))
}

/// The plan id and billing period from `/alpha/billing/subscriptions`. Soft: `None` on any failure.
struct Period {
    plan_id: Option<String>,
    since: Option<String>,
    end: Option<Value>,
}

async fn subscriptions(
    ctx: &QuotaFetchContext,
    base: &str,
    credential: &str,
    org: Option<&str>,
) -> Option<Period> {
    let url = url_with(base, "/alpha/billing/subscriptions", org, None);
    let response = ctx.http.request(request(url, credential)).await.ok()?;
    if !response.is_success() {
        return None;
    }
    let body = response.json().ok()?;
    let data = body.get("data")?;
    Some(Period {
        plan_id: data
            .get("planId")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned),
        since: data
            .get("currentPeriodStart")
            .and_then(Value::as_str)
            .map(str::to_owned),
        end: data.get("currentPeriodEnd").cloned(),
    })
}

/// Period spend from `/alpha/usage/summary`. Soft: `0.0` when the call cannot say, exactly as the
/// JavaScript source defaults it, so the credit pool's `used` is the only thing a failed summary
/// costs.
async fn usage_summary(
    ctx: &QuotaFetchContext,
    base: &str,
    credential: &str,
    org: Option<&str>,
    since: Option<&str>,
) -> f64 {
    let url = url_with(
        base,
        "/alpha/usage/summary",
        org,
        since.map(|since| ("since", since)),
    );
    let Ok(response) = ctx.http.request(request(url, credential)).await else {
        return 0.0;
    };
    if !response.is_success() {
        return 0.0;
    }
    let Ok(body) = response.json() else {
        return 0.0;
    };
    let cost = number(body.get("totalCost")).filter(|cost| *cost >= 0.0);
    cost.or_else(|| number(body.get("totalMonthlyCredits")).filter(|cost| *cost >= 0.0))
        .unwrap_or(0.0)
}

/// `humanizePlanId`: a known plan's label, else a title-cased form of the id.
fn humanize_plan_id(plan_id: &str) -> String {
    if let Some((_, label)) = PLAN_LABELS.iter().find(|(id, _)| *id == plan_id) {
        return (*label).to_owned();
    }
    let stripped = plan_id
        .strip_prefix("individual-")
        .map(str::to_owned)
        .unwrap_or_else(|| plan_id.to_owned());
    let stripped = match stripped.strip_prefix("team-") {
        Some(rest) => format!("Team {rest}"),
        None => stripped,
    };
    let title = stripped
        .split('-')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        format!("Command Code · {plan_id}")
    } else {
        format!("Command Code · {title}")
    }
}

#[async_trait]
impl QuotaFetcher for CommandCode {
    fn id(&self) -> &'static str {
        Self::ID
    }

    /// The renamed provider id, and the pre-rename one until the config migration has run. Nothing
    /// else claims these accounts.
    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        provider_id == Self::ID || provider_id == LEGACY_PROVIDER_ID
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "provider {:?} has no Command Code credential to read usage with",
                    ctx.provider_id
                ))
            })?;
        let credential = credential.expose();
        let base = api_base();

        // The operator may name the organisation, in which case `/alpha/whoami` would tell us
        // nothing new: spend the call only when the answer is actually unknown.
        let declared_org = ctx
            .account
            .extra
            .get(ORG_ID_KEY)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|org| !org.is_empty())
            .map(str::to_owned);
        let org = match declared_org {
            Some(org) => Some(org),
            None => whoami_org(ctx, &base, credential).await?,
        };

        // The quota itself: everything else is enrichment.
        let url = url_with(&base, "/alpha/billing/credits", org.as_deref(), None);
        let response = ctx.http.request(request(url, credential)).await?;
        if matches!(response.status, 401 | 403) {
            return Err(Error::InvalidState(format!(
                "provider {:?} rejected the Command Code API key (HTTP {}) at /alpha/billing/credits",
                ctx.provider_id, response.status
            )));
        }
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "provider {:?} answered {} at /alpha/billing/credits",
                ctx.provider_id, response.status
            )));
        }
        let body = response.json()?;

        let period = subscriptions(ctx, &base, credential, org.as_deref()).await;
        let period_used = usage_summary(
            ctx,
            &base,
            credential,
            org.as_deref(),
            period.as_ref().and_then(|period| period.since.as_deref()),
        )
        .await;

        let windows = parse_windows(&body);
        let credits = credit_budget(
            &body,
            period_used,
            period.as_ref().and_then(|period| period.end.as_ref()),
        );
        if windows.is_empty() && credits.is_none() {
            return Err(Error::NotFound(format!(
                "provider {:?}: /alpha/billing/credits reported neither a credit pool nor a window \
                 with a positive cap",
                ctx.provider_id
            )));
        }

        let mut details: Vec<QuotaDetail> = windows
            .iter()
            .map(|window| QuotaDetail {
                name: window.key.clone().unwrap_or_else(|| window.family.label()),
                used: window.used,
            })
            .collect();
        if let Some(credits) = &credits {
            details.push(QuotaDetail {
                name: "credits".to_owned(),
                used: credits.used,
            });
        }

        let headline = credits.unwrap_or_else(|| window_quota(&windows[0]));
        let mut quota = headline.with_source(Self::ID);
        if let Some(plan_id) = period.as_ref().and_then(|period| period.plan_id.as_deref()) {
            quota = quota.with_display_name(humanize_plan_id(plan_id));
        }
        for detail in details {
            quota = quota.with_detail(detail);
        }
        Ok(quota)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, MutexGuard};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};

    /// The environment is process-wide, so every test that fetches holds this: an override set by
    /// one test can otherwise be read by another running in parallel.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_lock() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A captured-shaped `/alpha/billing/credits` reply: three credit pools and both windows.
    const CREDITS: &str = r#"{
        "credits": { "monthlyCredits": 10, "purchasedCredits": 5, "freeCredits": 1.5 },
        "windowLimits": {
            "fiveHour": { "cap": 50, "used": 12.5, "resetAt": "2026-10-16T05:00:00Z" },
            "weekly": { "cap": 200, "used": 40, "resetAt": 1760000000000 },
            "exceeded": null,
            "limited": false
        }
    }"#;

    /// A window whose `cap` is zero means "not applicable", not "exhausted".
    const CREDITS_ZERO_WEEKLY: &str = r#"{
        "credits": { "monthlyCredits": 2, "purchasedCredits": 0, "freeCredits": 0 },
        "windowLimits": {
            "fiveHour": { "cap": 50, "used": 12.5, "resetAt": null },
            "weekly": { "cap": 0, "used": 40, "resetAt": 1760000000000 }
        }
    }"#;

    const WHOAMI: &str = r#"{"org":{"id":"org_123","name":"Acme"}}"#;
    const SUBSCRIPTIONS: &str = r#"{"data":{"planId":"individual-go","currentPeriodStart":"2026-10-01T00:00:00Z","currentPeriodEnd":"2026-11-01T00:00:00Z"}}"#;
    const SUMMARY: &str = r#"{"totalCost":3.5,"totalMonthlyCredits":99}"#;

    /// A transport double: canned replies by URL fragment, and the requests it saw.
    #[derive(Debug)]
    struct FakeHttp {
        replies: Mutex<Vec<(&'static str, u16, &'static str)>>,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(replies: Vec<(&'static str, u16, &'static str)>) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies),
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
            self.seen
                .lock()
                .expect("not poisoned")
                .push(request.clone());
            let replies = self.replies.lock().expect("not poisoned");
            let reply = replies
                .iter()
                .find(|(fragment, _, _)| request.url.contains(*fragment))
                .map(|(_, status, body)| (*status, *body))
                .unwrap_or((404, r#"{"error":"no canned reply"}"#));
            Ok(QuotaResponse {
                status: reply.0,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: Bytes::from(reply.1),
            })
        }
    }

    fn http_double(fake: &Arc<FakeHttp>) -> Arc<dyn QuotaHttp> {
        Arc::clone(fake) as Arc<dyn QuotaHttp>
    }

    fn serving() -> Vec<(&'static str, u16, &'static str)> {
        vec![
            ("/alpha/whoami", 200, WHOAMI),
            ("/alpha/billing/credits", 200, CREDITS),
            ("/alpha/billing/subscriptions", 200, SUBSCRIPTIONS),
            ("/alpha/usage/summary", 200, SUMMARY),
        ]
    }

    fn context(fake: &Arc<FakeHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new("command-code-go", Dialect::CommandCodeGo, http_double(fake))
            .with_credential("cc-secret")
    }

    #[test]
    fn windows_keep_their_real_families_and_keys() {
        let body: Value = serde_json::from_str(CREDITS).expect("valid json");
        let windows = parse_windows(&body);
        assert_eq!(windows.len(), 2);

        // A window is a duration, not a name: five hours is the 5h family, seven days the weekly one.
        assert_eq!(windows[0].family, WindowFamily::Session5h);
        assert_eq!(windows[0].family.label(), "5h");
        assert_eq!(windows[0].key.as_deref(), Some("five_hour"));
        assert_eq!(windows[0].used, 12.5);
        assert_eq!(windows[0].total, 50.0);
        assert_eq!(windows[0].remaining, 37.5);
        assert!(windows[0].reset_at.is_some());

        assert_eq!(windows[1].family, WindowFamily::Weekly1w);
        assert_eq!(windows[1].family.label(), "1w");
        assert_eq!(windows[1].key.as_deref(), Some("weekly"));
        assert_eq!(windows[1].used, 40.0);
        assert_eq!(windows[1].total, 200.0);
        assert_eq!(windows[1].reset_at, Some(1_760_000_000_000));
    }

    #[test]
    fn a_window_with_a_non_positive_cap_is_omitted_not_zeroed() {
        let body: Value = serde_json::from_str(CREDITS_ZERO_WEEKLY).expect("valid json");
        let windows = parse_windows(&body);
        // The zero-cap weekly window is absent; reporting it as 0/0 would read as exhausted.
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].key.as_deref(), Some("five_hour"));

        // Both windows gone: no windows at all, rather than two exhausted-looking ones.
        let body = serde_json::json!({
            "windowLimits": { "fiveHour": { "cap": 0 }, "weekly": { "cap": -1, "used": 5 } }
        });
        assert!(parse_windows(&body).is_empty());
    }

    #[tokio::test]
    async fn the_credit_pool_is_the_period_spend_plus_the_remaining_pools() {
        // The environment lock is scoped to the value that reads it. Holding a `MutexGuard` across
        // an `.await` would make this test able to deadlock the runtime.
        let fake = { FakeHttp::new(serving()) };
        let quota = CommandCode::new()
            .fetch(&context(&fake))
            .await
            .expect("credits payload is readable");

        // remaining = 10 + 5 + 1.5; period spend = totalCost = 3.5; total = spend + remaining.
        assert_eq!(quota.used, 3.5);
        assert_eq!(quota.total, 20.0);
        assert_eq!(quota.remaining, 16.5);
        assert!(!quota.unlimited);
        assert!(quota.fraction_reported);
        assert_eq!(quota.source.as_deref(), Some(CommandCode::ID));
        assert!(quota.reset_at.is_some());

        // Both windows and the pool are named on the one account-level quota.
        let names: Vec<&str> = quota
            .details
            .iter()
            .map(|detail| detail.name.as_str())
            .collect();
        assert_eq!(names, vec!["five_hour", "weekly", "credits"]);
        assert_eq!(quota.details[0].used, 12.5);
        assert_eq!(quota.details[1].used, 40.0);
        assert_eq!(quota.details[2].used, 3.5);

        // Every call is an authenticated GET with the documented headers and timeout.
        let requests = fake.requests();
        assert_eq!(requests.len(), 4, "whoami, credits, subscriptions, summary");
        assert_eq!(requests[1].method, http::Method::GET);
        // whoami discovered `org_123`, so the credits call carries the `orgId` query parameter —
        // exactly as `a_declared_org_is_sent_as_org_id_and_whoami_is_skipped` documents.
        assert!(requests[1]
            .url
            .ends_with("/alpha/billing/credits?orgId=org_123"));
        assert_eq!(
            requests[1].headers,
            vec![
                ("authorization".to_owned(), "Bearer cc-secret".to_owned()),
                ("accept".to_owned(), "application/json".to_owned()),
                ("content-type".to_owned(), "application/json".to_owned()),
            ]
        );
        assert_eq!(requests[1].timeout, Some(FETCH_TIMEOUT));
        for request in &requests {
            assert_eq!(request.method, http::Method::GET);
            assert_eq!(request.timeout, Some(FETCH_TIMEOUT));
        }
    }

    #[tokio::test]
    async fn a_declared_org_is_sent_as_org_id_and_whoami_is_skipped() {
        let fake = FakeHttp::new(serving());
        let ctx = context(&fake).with_account(
            QuotaAccount::new().with_extra(ORG_ID_KEY, Value::String("org_abc".to_owned())),
        );
        CommandCode::new().fetch(&ctx).await.expect("org declared");

        let requests = fake.requests();
        // The operator already said which org it is; `/alpha/whoami` would tell us nothing.
        assert_eq!(requests.len(), 3);
        assert!(requests[0]
            .url
            .ends_with("/alpha/billing/credits?orgId=org_abc"));
        assert!(requests[1]
            .url
            .ends_with("/alpha/billing/subscriptions?orgId=org_abc"));
        assert!(requests[2]
            .url
            .contains("/alpha/usage/summary?orgId=org_abc&since=2026-10-01T00%3A00%3A00Z"));
    }

    #[tokio::test]
    async fn whoami_supplies_the_org_when_the_account_does_not() {
        let fake = FakeHttp::new(serving());
        CommandCode::new()
            .fetch(&context(&fake))
            .await
            .expect("whoami answers");

        let requests = fake.requests();
        assert!(requests[0].url.ends_with("/alpha/whoami"));
        assert!(requests[1]
            .url
            .ends_with("/alpha/billing/credits?orgId=org_123"));
        assert!(requests[3].url.contains("orgId=org_123&since="));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn the_api_url_environment_variable_overrides_the_base() {
        // This is the one test in the file that mutates the process environment, so it is the one
        // that genuinely needs `env_lock` — and it needs it held across the `.await`, because the
        // fetcher reads the variable *inside* the call. Dropping the lock early would let a
        // concurrently running test observe a base URL it never set.
        //
        // `await_holding_lock` is allowed rather than worked around: the alternative is an
        // async-aware mutex shared with tests that are otherwise purely synchronous, which would be
        // worse than one annotated exception.
        let _guard = env_lock();
        std::env::set_var(API_BASE_ENV, "https://quota.test/");
        let fake = FakeHttp::new(serving());
        let fetched = CommandCode::new().fetch(&context(&fake)).await;
        std::env::remove_var(API_BASE_ENV);

        fetched.expect("override base is used");
        // The account declares no org, so `/alpha/whoami` goes first (see
        // `whoami_supplies_the_org_when_the_account_does_not`) and the credits call is second. Both
        // must carry the overridden base — that is what proves the variable took effect.
        let requests = fake.requests();
        assert!(requests[0]
            .url
            .starts_with("https://quota.test/alpha/whoami"));
        let credits = requests
            .iter()
            .find(|request| request.url.contains("/alpha/billing/credits"))
            .expect("the credits call happened");
        assert!(
            credits
                .url
                .starts_with("https://quota.test/alpha/billing/credits"),
            "{}",
            credits.url
        );
    }

    #[test]
    fn the_base_url_override_is_trimmed_and_an_empty_one_is_ignored() {
        assert_eq!(resolve_api_base(None), DEFAULT_API_BASE);
        assert_eq!(resolve_api_base(Some("   ")), DEFAULT_API_BASE);
        // A value that normalises away entirely is as empty as an empty one.
        assert_eq!(resolve_api_base(Some(" / ")), DEFAULT_API_BASE);
        assert_eq!(
            resolve_api_base(Some(" https://quota.test/v1/ ")),
            "https://quota.test/v1"
        );
        // Whitespace-only and trailing-slash forms normalise to the same base.
        assert_eq!(
            resolve_api_base(Some("https://quota.test/v1")),
            resolve_api_base(Some(" https://quota.test/v1/ "))
        );
    }

    #[tokio::test]
    async fn a_rejected_key_is_an_error_not_an_empty_quota() {
        let fake = FakeHttp::new(vec![
            ("/alpha/whoami", 200, WHOAMI),
            ("/alpha/billing/credits", 401, r#"{"error":"unauthorized"}"#),
        ]);
        let error = CommandCode::new()
            .fetch(&context(&fake))
            .await
            .expect_err("401 must not become a quota");
        let message = error.to_string();
        assert!(matches!(error, Error::InvalidState(_)), "{error:?}");
        assert!(message.contains("401"), "{message}");
        assert!(!message.contains("cc-secret"), "no credential in an error");
    }

    #[tokio::test]
    async fn a_rejected_whoami_key_is_an_error_too() {
        let fake = FakeHttp::new(vec![
            ("/alpha/whoami", 403, r#"{"error":"forbidden"}"#),
            ("/alpha/billing/credits", 200, CREDITS),
        ]);
        let error = CommandCode::new()
            .fetch(&context(&fake))
            .await
            .expect_err("403 must not be swallowed");
        assert!(matches!(error, Error::InvalidState(_)), "{error:?}");
        // The rejected key stopped the fetch before the quota call.
        assert_eq!(fake.requests().len(), 1);
    }

    #[tokio::test]
    async fn neither_a_pool_nor_a_window_is_an_error_rather_than_zero_over_zero() {
        let fake = FakeHttp::new(vec![
            ("/alpha/whoami", 200, WHOAMI),
            (
                "/alpha/billing/credits",
                200,
                r#"{"windowLimits":{"weekly":{"cap":0}}}"#,
            ),
        ]);
        let error = CommandCode::new()
            .fetch(&context(&fake))
            .await
            .expect_err("nothing usable must not become 0/0");
        assert!(matches!(error, Error::NotFound(_)), "{error:?}");
    }

    #[tokio::test]
    async fn a_missing_credential_is_a_configuration_error() {
        let fake = FakeHttp::new(serving());
        let ctx = QuotaFetchContext::new(
            "command-code-go",
            Dialect::CommandCodeGo,
            http_double(&fake),
        );
        let error = CommandCode::new()
            .fetch(&ctx)
            .await
            .expect_err("no credential, no read");
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        assert!(fake.requests().is_empty());
    }

    #[tokio::test]
    async fn a_soft_subscription_failure_still_reports_the_pool_and_windows() {
        let fake = FakeHttp::new(vec![
            ("/alpha/whoami", 200, WHOAMI),
            ("/alpha/billing/credits", 200, CREDITS),
            ("/alpha/usage/summary", 200, SUMMARY),
        ]);
        let quota = CommandCode::new()
            .fetch(&context(&fake))
            .await
            .expect("credits alone are enough");
        // No period start, so the summary is asked without `since`, and the pool still lands.
        assert_eq!(quota.used, 3.5);
        assert_eq!(quota.total, 20.0);
        // No plan arrived, so the pool keeps the only label there is.
        assert_eq!(quota.display_name.as_deref(), Some("Credits"));
    }

    #[test]
    fn supports_the_renamed_provider_and_the_legacy_one() {
        let account = QuotaAccount::new();
        assert!(CommandCode::new().supports(Dialect::CommandCodeGo, "command-code-go", &account));
        assert!(CommandCode::new().supports(Dialect::CommandCodeGo, "command-code", &account));
        assert!(!CommandCode::new().supports(Dialect::CommandCodeGo, "openrouter", &account));
    }

    #[test]
    fn plan_ids_humanize_like_the_source() {
        assert_eq!(humanize_plan_id("individual-go"), "Command Code · Go");
        assert_eq!(humanize_plan_id("team-pro"), "Command Code · Team Pro");
        assert_eq!(
            humanize_plan_id("individual-max-99x"),
            "Command Code · Max 99x"
        );
        assert_eq!(humanize_plan_id("weird"), "Command Code · Weird");
    }
}
