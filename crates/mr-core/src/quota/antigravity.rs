//! Google Antigravity (Gemini Code Assist) usage.
//!
//! Ported from OmniRoute's per-provider usage fetcher (MIT, © 2026 diegosouzapw):
//! `open-sse/services/usage/antigravity.ts` (`getAntigravityUsage`, the per-model
//! `retrieveUserQuota` / `fetchAvailableModels` reads), `open-sse/services/usage/antigravityWeeklyQuota.ts`
//! (the weekly `retrieveUserQuotaSummary` reader), `open-sse/services/antigravityQuotaFamily.ts`
//! (Gemini vs Claude/GPT allowance scoping) and `open-sse/services/antigravityHeaders.ts` /
//! `open-sse/config/antigravityUpstream.ts` (endpoints, client profile and the IDE/CLI
//! `User-Agent`).
//!
//! What matters most here is the same sentinel rule as everywhere else, and it bites hardest on
//! this provider:
//!
//! * **`remainingFraction` absent is "unknown", never "exhausted".** The catalog endpoint
//!   (`fetchAvailableModels`) is a catalog/eligibility view and routinely omits the fraction for a
//!   model whose bucket has not started. Upstream sets `fractionReported = false` and the zero
//!   that lands in `remaining` is a sentinel; this port uses [`UsageQuota::from_usage_unreported`]
//!   for exactly that case. A bespoke `0/0` would render as an exhausted account.
//! * **A reported `1.0` with no `resetTime` is unlimited**, not a full bucket with a missing
//!   reset.
//! * **`total` is the normalised `1000` base**, so `remaining = round(1000 × fraction)`. That is
//!   upstream's own normalisation, kept so a cached row means the same thing in both programs.
//! * **`source` says how stale a row may be.** `retrieveUserQuota` is the live consumption signal;
//!   `fetchAvailableModels` is the older catalog view, and a consumer needs to know which answered.
//! * **The window type is inferred from text.** The summary RPC has no window-type field, so a
//!   bucket counts as weekly only when its `bucketId`+`displayName` text matches `\bweekly\b`.
//!
//! Every call is a `POST` with `{project: <id>}`; the base URLs are walked in order, treating any
//! non-success other than 401/403 as "try the next", exactly as upstream does. `retrieveUserQuota`
//! and `retrieveUserQuotaSummary` are best-effort (upstream returns `null` on any failure);
//! `fetchAvailableModels` is the hard dependency, and its failure is an `Err`.
//!
//! Two OmniRoute pieces are deliberately **not** ported, because the account model here does not
//! have what they need: the local-usage-history fallback (it reads OmniRoute's own database) and
//! the credits probe `antigravityCredits.ts` (it spends a credit-bearing inference call after a
//! forced refresh). The hard-coded public-model allowlist in `antigravityModelAliases.ts` is also
//! not reproduced: the authenticated catalog is already subscription-gated, and this port keeps
//! every non-internal model the account actually sees rather than a pinned list that would go
//! stale.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, QuotaWindow, UsageQuota, WindowFamily};

/// The runtime base URLs, in the order OmniRoute tries them.
pub const RUNTIME_BASE_URLS: [&str; 2] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
];

/// The discovery base URLs. Model discovery also lists the sandbox host.
pub const DISCOVERY_BASE_URLS: [&str; 3] = [
    "https://daily-cloudcode-pa.googleapis.com",
    "https://cloudcode-pa.googleapis.com",
    "https://daily-cloudcode-pa.sandbox.googleapis.com",
];

/// The catalog + `quotaInfo` read (hard dependency).
pub const FETCH_AVAILABLE_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";
/// The live per-model consumption read (best-effort).
pub const RETRIEVE_USER_QUOTA_PATH: &str = "/v1internal:retrieveUserQuota";
/// The per-family weekly summary read (best-effort).
pub const RETRIEVE_USER_QUOTA_SUMMARY_PATH: &str = "/v1internal:retrieveUserQuotaSummary";

/// The `source` recorded for a row that came from the live per-model read.
pub const SOURCE_LIVE: &str = "retrieveUserQuota";
/// The `source` recorded for a row that came from the catalog read.
pub const SOURCE_CATALOG: &str = "fetchAvailableModels";
/// The `source` recorded for a weekly summary row.
pub const SOURCE_WEEKLY: &str = "retrieveUserQuotaSummary";

/// The IDE client version OmniRoute falls back to when it cannot reach the release feed.
pub const IDE_FALLBACK_VERSION: &str = "2.1.1";
/// The CLI client version OmniRoute falls back to.
pub const CLI_FALLBACK_VERSION: &str = "1.1.5";

/// The normalised quota base upstream divides a `0.0..=1.0` fraction into.
const NORMALIZED_BASE: f64 = 1000.0;
/// Five hours: a per-model bucket is the session window.
const SESSION_SECONDS: u64 = 5 * 60 * 60;
/// Seven days: a summary bucket named "weekly" is the weekly window.
const WEEKLY_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Provider ids this fetcher answers for. `agy` is upstream's alias.
const PROVIDER_IDS: [&str; 2] = ["antigravity", "agy"];

/// Where the Cloud Code project id is read from, in order.
///
/// `cloudcode_project_id` is the key Manarouter's own OAuth migration stores it under
/// (`crates/mr-core/src/oauth/migrate.rs`), and the relay reads the same key
/// (`manarouter-relay/src/server.rs`). The remaining spellings are OmniRoute's
/// (`providerSpecificData.projectId`, and the `project`/`project.id` the load-code-assist response
/// carries), kept so an account migrated from OmniRoute still resolves.
const PROJECT_ID_KEYS: [&str; 4] = ["cloudcode_project_id", "projectId", "project_id", "project"];

/// Google Antigravity quota, read from the Cloud Code `v1internal:*` RPCs.
#[derive(Debug, Clone, Copy, Default)]
pub struct Antigravity;

impl Antigravity {
    /// The registry id.
    pub const ID: &str = "antigravity";

    /// A fetcher instance, for `QuotaRegistry::register`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for Antigravity {
    fn id(&self) -> &'static str {
        Self::ID
    }

    /// Antigravity is an OAuth-only surface: the `v1internal:*` RPCs want a Google access token,
    /// so an API-key account is left to whatever fetcher claims its dialect.
    fn supports(&self, dialect: Dialect, provider_id: &str, account: &QuotaAccount) -> bool {
        account.oauth && (PROVIDER_IDS.contains(&provider_id) || dialect == Dialect::Antigravity)
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(
                    "antigravity quota: the account has no OAuth credential to present".to_owned(),
                )
            })?;

        let project = project_id(&ctx.account);
        let bases = candidates(ctx.base_url.as_deref());

        // The catalog read is the hard dependency: without it there is nothing to report.
        let catalog = match walk(
            ctx,
            credential.expose(),
            &bases,
            FETCH_AVAILABLE_MODELS_PATH,
            project.as_deref(),
            &client_profile(&ctx.account),
        )
        .await?
        {
            Outcome::Answered { value, .. } => value,
            Outcome::Rejected(status) => return Err(status_error("fetchAvailableModels", status)),
            Outcome::Unavailable(last) => {
                return Err(unavailable_error("fetchAvailableModels", last))
            }
        };

        // Both of these are best-effort upstream; a failure means the rows they would have
        // contributed are simply absent, never fabricated.
        let live = optional(
            ctx,
            credential.expose(),
            &bases,
            RETRIEVE_USER_QUOTA_PATH,
            project.as_deref(),
            &client_profile(&ctx.account),
        )
        .await;
        let weekly = optional(
            ctx,
            credential.expose(),
            &bases,
            RETRIEVE_USER_QUOTA_SUMMARY_PATH,
            project.as_deref(),
            &client_profile(&ctx.account),
        )
        .await;

        parse(&catalog, live.as_ref(), weekly.as_ref()).ok_or_else(|| {
            Error::NotFound(
                "antigravity quota: the catalog carried no model with a quota bucket".to_owned(),
            )
        })
    }
}

/// Which client identity the request presents. Upstream reads this from the account's provider
/// data; here it is the `clientProfile` extra, defaulting to `ide` as upstream does.
fn client_profile(account: &QuotaAccount) -> String {
    account
        .extra
        .get("clientProfile")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|profile| profile.eq_ignore_ascii_case("cli"))
        .map(|_| "cli".to_owned())
        .unwrap_or_else(|| "ide".to_owned())
}

/// The `User-Agent` for a profile. The platform token is pinned to the native desktop
/// fingerprint (`darwin/arm64`) because that is what the upstream backend expects, exactly as
/// OmniRoute pins it.
fn user_agent(profile: &str) -> String {
    if profile == "cli" {
        format!(
            "antigravity/cli/{CLI_FALLBACK_VERSION} (aidev_client; os_type=darwin; arch=arm64; auth_method=consumer)"
        )
    } else {
        format!("antigravity/ide/{IDE_FALLBACK_VERSION} darwin/arm64")
    }
}

/// The Cloud Code project id an account declares, if any.
///
/// A malformed id is skipped rather than sent: the same validation the relay applies
/// (`manarouter-relay/src/server.rs`) is applied here, so a corrupt extra cannot produce a
/// nonsense upstream call.
pub fn project_id(account: &QuotaAccount) -> Option<String> {
    for key in PROJECT_ID_KEYS {
        let Some(value) = account.extra.get(key) else {
            continue;
        };
        if let Some(id) = project_id_value(value) {
            return Some(id);
        }
    }
    None
}

/// A project id from a bare string or from an object carrying an `id`.
fn project_id_value(value: &Value) -> Option<String> {
    let text = value
        .as_str()
        .or_else(|| value.get("id").and_then(Value::as_str))?;
    let trimmed = text.trim();
    let valid = !trimmed.is_empty()
        && trimmed.len() <= 256
        && trimmed
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    valid.then(|| trimmed.to_owned())
}

/// The base URLs to try: the operator's declared base first when there is one, then the defaults.
fn candidates(base_url: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(base) = base_url {
        let trimmed = base.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
    }
    for base in DISCOVERY_BASE_URLS {
        if !out.iter().any(|existing| existing == base) {
            out.push(base.to_owned());
        }
    }
    out
}

/// What walking the base-URL candidates produced.
enum Outcome {
    /// A candidate answered with a JSON body.
    Answered {
        /// The parsed body.
        value: Value,
    },
    /// A candidate answered `401`/`403`. No other base URL helps with that.
    Rejected(u16),
    /// No candidate answered; the last non-success status, when one was seen.
    Unavailable(Option<u16>),
}

/// `POST` one RPC, walking the candidate base URLs in order.
///
/// A transport failure propagates: the caller decides whether that is fatal (the catalog read) or
/// merely means the row is absent (the best-effort reads).
async fn walk(
    ctx: &QuotaFetchContext,
    credential: &str,
    bases: &[String],
    path: &str,
    project: Option<&str>,
    profile: &str,
) -> Result<Outcome> {
    let body = match project {
        Some(project) => json!({ "project": project }).to_string(),
        None => "{}".to_owned(),
    };

    let mut last = None;
    for base in bases {
        let request = QuotaRequest::post_json(format!("{base}{path}"), body.clone())
            .header("User-Agent", user_agent(profile))
            .bearer(credential)
            .timeout(Duration::from_secs(10));
        let response = ctx.http.request(request).await?;

        if response.is_success() {
            return Ok(Outcome::Answered {
                value: response.json()?,
            });
        }
        match response.status {
            401 | 403 => return Ok(Outcome::Rejected(response.status)),
            status => last = Some(status),
        }
    }
    Ok(Outcome::Unavailable(last))
}

/// A best-effort read: any failure, from any candidate, is `None`.
async fn optional(
    ctx: &QuotaFetchContext,
    credential: &str,
    bases: &[String],
    path: &str,
    project: Option<&str>,
    profile: &str,
) -> Option<Value> {
    match walk(ctx, credential, bases, path, project, profile).await {
        Ok(Outcome::Answered { value }) => Some(value),
        _ => None,
    }
}

/// One quota window before it is collapsed into an account-level quota.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    /// The semantic key upstream uses: a model id, or `<group-slug>_weekly`.
    key: String,
    /// The operator-facing name.
    display_key: String,
    /// The duration band the window's real length implies.
    family: WindowFamily,
    /// Amount consumed on the normalised `1000` base.
    used: f64,
    /// The window's limit on the normalised base (`0` when no fraction was reported).
    total: f64,
    /// Whether the upstream reported `remainingFraction`.
    reported: bool,
    /// True when there is no `resetTime` and the reported fraction is `1.0`.
    unlimited: bool,
    /// The raw `resetTime`, handed to the `from_usage*` constructors.
    reset: Value,
    /// Which RPC answered this row.
    source: &'static str,
    /// The allowance family this row belongs to (`gemini`, `claude`, `other`).
    scope: &'static str,
}

impl Window {
    /// The normalised quota for this window, through the constructors so the sentinel rule holds.
    fn to_usage_quota(&self) -> UsageQuota {
        if self.unlimited {
            UsageQuota::unlimited().with_source(self.source)
        } else if self.reported {
            UsageQuota::from_usage(self.used, self.total, &self.reset).with_source(self.source)
        } else {
            UsageQuota::from_usage_unreported(self.used, self.total, &self.reset)
                .with_source(self.source)
        }
    }

    /// The detail-row name, carrying the source and allowance family a consumer needs.
    fn detail_name(&self) -> String {
        format!(
            "{} [source={} family={}]",
            self.display_key, self.source, self.scope
        )
    }
}

/// Every window the three payloads describe.
///
/// Exposed so a caller (or a test) can see each window individually, since [`UsageQuota`] itself
/// carries only the aggregate.
pub fn parse_windows(
    catalog: &Value,
    live: Option<&Value>,
    weekly: Option<&Value>,
) -> Vec<QuotaWindow> {
    let windows = all_windows(catalog, live, weekly);
    windows
        .into_iter()
        .map(|window| {
            let mut out = QuotaWindow::from_quota(window.family, &window.to_usage_quota())
                .with_key(window.key.clone());
            out.display_name = Some(window.display_key.clone());
            out
        })
        .collect()
}

/// The per-model windows from the catalog read, keyed by model id.
///
/// A model is skipped when it is marked internal or carries no `quotaInfo` at all (upstream's
/// "never started" case). When the live read has a bucket for the same model id, that bucket wins
/// and the row's `source` says so.
pub fn parse_catalog(catalog: &Value, live: Option<&Value>) -> Vec<QuotaWindow> {
    windows_from(catalog_windows(catalog, live))
}

/// The weekly windows from a `retrieveUserQuotaSummary` payload, keyed `<group-slug>_weekly`.
///
/// Both observed envelopes are accepted: `groups[]` at the top level, or nested under
/// `quotaSummary.groups[]`.
pub fn parse_weekly_summary(summary: &Value) -> Vec<QuotaWindow> {
    windows_from(weekly_windows(summary))
}

/// Collapse the three payloads into one account-level quota, or `None` when none of them carried a
/// usable bucket.
///
/// The per-model session windows win over the weekly ones, and among the candidates the most
/// constrained reported window is chosen. Every window is attached as a [`QuotaDetail`] so nothing
/// is dropped. The aggregate's `display_name` is the binding window's own name, and its `source`
/// is that window's source — there is no plan label here, because a plan label needs the
/// `loadCodeAssist` call this fetcher deliberately does not make.
pub fn parse(catalog: &Value, live: Option<&Value>, weekly: Option<&Value>) -> Option<UsageQuota> {
    let windows = all_windows(catalog, live, weekly);
    let primary = pick_primary(&windows)?;

    let mut quota = primary
        .to_usage_quota()
        .with_display_name(primary.display_key.clone());
    for window in &windows {
        quota = quota.with_detail(QuotaDetail {
            name: window.detail_name(),
            used: window.used,
        });
    }
    Some(quota)
}

fn all_windows(catalog: &Value, live: Option<&Value>, weekly: Option<&Value>) -> Vec<Window> {
    let mut out = catalog_windows(catalog, live);
    if let Some(weekly) = weekly {
        out.extend(weekly_windows(weekly));
    }
    out
}

/// The binding window: a session window when there is one, then the tightest reported window, then
/// an unlimited one, and only last an unreported one (unknown is not exhaustion).
fn pick_primary(windows: &[Window]) -> Option<&Window> {
    let session: Vec<&Window> = windows
        .iter()
        .filter(|window| window.family == WindowFamily::Session5h)
        .collect();
    let pool: Vec<&Window> = if session.is_empty() {
        windows.iter().collect()
    } else {
        session
    };
    pool.into_iter().min_by(|a, b| rank(a).total_cmp(&rank(b)))
}

/// Sort key for [`pick_primary`]: a reported remainder first, then unknown.
fn rank(window: &Window) -> f64 {
    if window.unlimited {
        2.0
    } else if window.reported && window.total > 0.0 {
        (window.remaining_fraction()).unwrap_or(2.0)
    } else {
        3.0
    }
}

impl Window {
    /// The reported **remainder**, as a fraction of the normalised base.
    ///
    /// `(total - used) / total`, never `used / total`: the latter is the *consumed* fraction, so
    /// ranking by it would make the least-constrained window look like the binding one.
    fn remaining_fraction(&self) -> Option<f64> {
        (self.reported && self.total > 0.0)
            .then(|| ((self.total - self.used) / self.total).clamp(0.0, 1.0))
    }
}

/// Build the model windows from the catalog, letting a live bucket override the catalog's.
fn catalog_windows(catalog: &Value, live: Option<&Value>) -> Vec<Window> {
    let live_buckets = live_buckets(live);
    let mut out = Vec::new();

    if let Some(models) = catalog.get("models").and_then(Value::as_object) {
        for (model_id, info) in models {
            let model_key = model_id.trim();
            if model_key.is_empty() || info.get("isInternal").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            let quota_info = info.get("quotaInfo").filter(|value| value.is_object());
            let Some(quota_info) = quota_info else {
                continue;
            };
            if quota_info
                .as_object()
                .is_some_and(|object| object.is_empty())
            {
                continue;
            }

            // A live bucket with no fraction means "not started", not "empty": it must not
            // overwrite a reported catalog row, or a healthy account would read as unknown. Same
            // rule the live-only loop below applies to buckets the catalog never named.
            let live_bucket = live_buckets
                .get(model_key)
                .filter(|bucket| fraction(bucket).is_some());
            let (source, bucket) = match live_bucket {
                Some(bucket) => (SOURCE_LIVE, bucket),
                None => (SOURCE_CATALOG, quota_info),
            };
            out.push(bucket_window(
                model_key,
                bucket,
                source,
                quota_family(model_key),
            ));
        }
    }

    // Live buckets the catalog has not caught up with yet. A bucket whose fraction is absent has
    // not started; upstream skips it rather than inventing a row.
    //
    // The keys are collected as owned strings rather than borrowed ones: the loop below pushes onto
    // `out`, and a borrow of `out` cannot coexist with that mutable borrow.
    let known: Vec<String> = out.iter().map(|window| window.key.clone()).collect();
    for (model_key, bucket) in &live_buckets {
        if known.iter().any(|key| key == model_key) {
            continue;
        }
        if fraction(bucket).is_none() {
            continue;
        }
        out.push(bucket_window(
            model_key,
            bucket,
            SOURCE_LIVE,
            quota_family(model_key),
        ));
    }
    out
}

/// One model bucket as a window.
fn bucket_window(
    model_key: &str,
    bucket: &Value,
    source: &'static str,
    scope: &'static str,
) -> Window {
    let reported = fraction(bucket);
    let reset = bucket.get("resetTime").cloned().unwrap_or(Value::Null);
    let reset_at = crate::quota::types::parse_reset_time(&reset);

    // A reported `1.0` with no reset instant is an unlimited bucket, not a missing reset. An
    // unreported fraction is never unlimited.
    let unlimited = reported.is_some() && reset_at.is_none() && reported.is_some_and(|f| f >= 1.0);
    let total = if reported.is_some() {
        NORMALIZED_BASE
    } else {
        0.0
    };
    let remaining = reported.map_or(0.0, |f| (total * f).round());
    let used = if unlimited {
        0.0
    } else {
        (total - remaining).max(0.0)
    };

    Window {
        key: model_key.to_owned(),
        display_key: model_key.to_owned(),
        family: WindowFamily::from_seconds(SESSION_SECONDS),
        used,
        total: if unlimited { 0.0 } else { total },
        reported: reported.is_some(),
        unlimited,
        reset,
        source,
        scope,
    }
}

/// The live per-model buckets, keyed by model id.
fn live_buckets(live: Option<&Value>) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let Some(buckets) = live
        .and_then(|value| value.get("buckets"))
        .and_then(Value::as_array)
    else {
        return out;
    };
    for bucket in buckets {
        let Some(model_id) = bucket.get("modelId").and_then(Value::as_str) else {
            continue;
        };
        let model_id = model_id.trim();
        if !model_id.is_empty() {
            out.insert(model_id.to_owned(), bucket.clone());
        }
    }
    out
}

/// The weekly windows from a summary payload, `groups[]` or `quotaSummary.groups[]`.
fn weekly_windows(summary: &Value) -> Vec<Window> {
    let groups = summary.get("groups").and_then(Value::as_array).or_else(|| {
        summary
            .get("quotaSummary")
            .and_then(|nested| nested.get("groups"))
            .and_then(Value::as_array)
    });

    let mut out = Vec::new();
    let Some(groups) = groups else {
        return out;
    };
    for group in groups {
        if let Some(window) = weekly_window(group) {
            out.push(window);
        }
    }
    out
}

/// One model-family group's weekly bucket.
fn weekly_window(group: &Value) -> Option<Window> {
    let buckets = group.get("buckets").and_then(Value::as_array)?;
    // The window type is inferred from the bucket's own text: there is no field for it.
    let bucket = buckets.iter().find(|bucket| {
        let text = format!(
            "{} {}",
            bucket.get("bucketId").and_then(Value::as_str).unwrap_or(""),
            bucket
                .get("displayName")
                .and_then(Value::as_str)
                .unwrap_or("")
        )
        .to_lowercase();
        text.split(|character: char| !character.is_ascii_alphanumeric())
            .any(|word| word == "weekly")
    })?;
    if bucket.get("disabled").and_then(Value::as_bool) == Some(true) {
        return None;
    }

    let display_name = group
        .get("displayName")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())?;
    let slug = slugify_group(display_name)?;

    // An absent fraction means the weekly window has not started; upstream skips the row.
    let reported = fraction(bucket)?;
    let reset = bucket.get("resetTime").cloned().unwrap_or(Value::Null);
    let reset_at = crate::quota::types::parse_reset_time(&reset);
    let unlimited = reset_at.is_none() && reported >= 1.0;
    let total = NORMALIZED_BASE;
    let remaining = (total * reported).round();
    let used = if unlimited {
        0.0
    } else {
        (total - remaining).max(0.0)
    };

    Some(Window {
        key: format!("{slug}_weekly"),
        display_key: display_name.to_owned(),
        family: WindowFamily::from_seconds(WEEKLY_SECONDS),
        used,
        total: if unlimited { 0.0 } else { total },
        reported: true,
        unlimited,
        reset,
        source: SOURCE_WEEKLY,
        scope: group_family(display_name),
    })
}

/// A finite `remainingFraction` clamped to `0.0..=1.0`.
fn fraction(bucket: &Value) -> Option<f64> {
    let value = bucket.get("remainingFraction")?;
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then(|| number.clamp(0.0, 1.0))
}

/// The group display name as a quota-key slug, upstream's `slugifyGroupWeeklyKey`.
fn slugify_group(display_name: &str) -> Option<String> {
    let slug = display_name
        .to_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty() && *word != "model" && *word != "models" && *word != "and")
        .collect::<Vec<_>>()
        .join("_");
    (!slug.is_empty()).then_some(slug)
}

/// The allowance family a model belongs to, from `antigravityQuotaFamily.ts`.
///
/// Gemini and Claude/GPT models draw on separate Antigravity allowances, so the family is carried
/// on every model row to keep them from being read as one pot. Unknown models stay `other`.
pub fn quota_family(model: &str) -> &'static str {
    let normalized = model.trim().to_lowercase();
    let normalized = normalized
        .strip_prefix("antigravity/")
        .or_else(|| normalized.strip_prefix("agy/"))
        .unwrap_or(&normalized);
    let bare = match normalized.split_once('/') {
        Some((_, bare)) => bare,
        None => normalized,
    };

    if bare.starts_with("gemini-") || bare.contains("/gemini-") || bare.contains("gemini") {
        "gemini"
    } else if bare.starts_with("claude-")
        || bare.starts_with("cloud-")
        || bare.contains("/claude-")
        || bare.contains("/cloud-")
        || bare.contains("anthropic")
    {
        "claude"
    } else {
        "other"
    }
}

/// The allowance family a weekly group describes, from its display name.
fn group_family(display_name: &str) -> &'static str {
    let lowered = display_name.to_lowercase();
    if lowered.contains("gemini") {
        "gemini"
    } else if lowered.contains("claude") || lowered.contains("gpt") {
        "claude"
    } else {
        "other"
    }
}

/// Build [`QuotaWindow`] views of parsed windows.
fn windows_from(windows: Vec<Window>) -> Vec<QuotaWindow> {
    windows
        .into_iter()
        .map(|window| {
            let mut out = QuotaWindow::from_quota(window.family, &window.to_usage_quota())
                .with_key(window.key.clone());
            out.display_name = Some(window.display_key.clone());
            out
        })
        .collect()
}

/// An upstream status as an error. Never invents a quota for a failed read.
fn status_error(path: &str, status: u16) -> Error {
    match status {
        401 | 403 => Error::Config(format!(
            "antigravity quota: the upstream rejected the credential for {path} (status {status})"
        )),
        _ => Error::Io(std::io::Error::other(format!(
            "antigravity quota: {path} returned status {status}"
        ))),
    }
}

/// No candidate for a required read answered.
fn unavailable_error(path: &str, last: Option<u16>) -> Error {
    match last {
        Some(status) => Error::NotFound(format!(
            "antigravity quota: no base URL answered {path} (last status {status})"
        )),
        None => Error::NotFound(format!("antigravity quota: no base URL answered {path}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};
    use crate::quota::types::parse_reset_time;
    use bytes::Bytes;
    use std::sync::{Arc, Mutex};

    /// A transport that answers by exact URL and records what it was asked to send.
    #[derive(Debug)]
    struct FakeHttp {
        routes: HashMap<String, QuotaResponse>,
        fallback: QuotaResponse,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(routes: Vec<(&str, QuotaResponse)>, fallback: QuotaResponse) -> Self {
            Self {
                routes: routes
                    .into_iter()
                    .map(|(url, reply)| (url.to_owned(), reply))
                    .collect(),
                fallback,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<QuotaRequest> {
            self.seen.lock().expect("fake http poisoned").clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen
                .lock()
                .expect("fake http poisoned")
                .push(request.clone());
            Ok(self
                .routes
                .get(&request.url)
                .cloned()
                .unwrap_or_else(|| self.fallback.clone()))
        }
    }

    /// A minimal executor, so the tests need no async-runtime dependency.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        fn noop(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);

        let mut future = Box::pin(future);
        // SAFETY: the vtable's functions only ever hand back a null-data waker; no ownership,
        // allocation or thread-local state is involved.
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        let mut context = Context::from_waker(&waker);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
            std::thread::yield_now();
        }
    }

    fn reply(status: u16, body: &str) -> QuotaResponse {
        QuotaResponse {
            status,
            headers: Vec::new(),
            body: Bytes::copy_from_slice(body.as_bytes()),
        }
    }

    fn header<'a>(request: &'a QuotaRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn url(base: &str, path: &str) -> String {
        format!("{base}{path}")
    }

    fn daily(path: &str) -> String {
        url(RUNTIME_BASE_URLS[0], path)
    }

    fn public(path: &str) -> String {
        url(RUNTIME_BASE_URLS[1], path)
    }

    fn context(http: Arc<FakeHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new("antigravity", Dialect::Antigravity, http)
            .with_account(
                QuotaAccount::new().with_extra("cloudcode_project_id", json!("test-project-id")),
            )
            .with_credential("test-token-not-a-real-secret")
    }

    fn payload(text: &str) -> Value {
        serde_json::from_str(text).expect("test payload is valid JSON")
    }

    /// A catalog with one model reporting a quarter left, and no reset.
    const CATALOG_REPORTED: &str = r#"{
        "models": {
            "gemini-3-pro": {
                "displayName": "Gemini 3 Pro",
                "quotaInfo": {"remainingFraction": 0.25, "resetTime": "2026-01-02T03:04:05Z"}
            }
        }
    }"#;

    /// A catalog whose only model never reported a fraction.
    const CATALOG_UNREPORTED: &str = r#"{
        "models": {
            "gemini-3-pro": {
                "displayName": "Gemini 3 Pro",
                "quotaInfo": {"resetTime": "2026-01-02T03:04:05Z"}
            }
        }
    }"#;

    /// A catalog whose only model has a full fraction and no reset instant.
    const CATALOG_UNLIMITED: &str = r#"{
        "models": {
            "tab-completion": {"quotaInfo": {"remainingFraction": 1.0}}
        }
    }"#;

    /// The live per-model read answers with a lower fraction than the catalog.
    const LIVE_LOWER: &str = r#"{
        "buckets": [
            {"modelId": "gemini-3-pro", "remainingFraction": 0.1,
             "resetTime": "2026-01-02T03:04:05Z"}
        ]
    }"#;

    const EMPTY: &str = r#"{"buckets": []}"#;

    #[test]
    fn a_reported_fraction_uses_the_normalised_thousand_base() {
        let catalog = payload(CATALOG_REPORTED);
        let quota = parse(&catalog, None, None).expect("catalog yields a quota");

        assert!(quota.fraction_reported);
        assert_eq!(quota.total, 1000.0);
        assert_eq!(quota.remaining, 250.0);
        assert_eq!(quota.remaining_percentage, 25.0);
        assert_eq!(quota.used, 750.0);
        assert!(!quota.unlimited);
        assert_eq!(quota.source.as_deref(), Some("fetchAvailableModels"));
        assert_eq!(
            quota.reset_at,
            parse_reset_time(
                &payload(CATALOG_REPORTED)["models"]["gemini-3-pro"]["quotaInfo"]["resetTime"]
            )
        );
        // The Gemini allowance family rides along.
        let detail = &quota.details[0];
        assert!(detail.name.contains("family=gemini"), "{}", detail.name);
    }

    #[test]
    fn an_absent_fraction_is_a_sentinel_not_exhaustion() {
        let catalog = payload(CATALOG_UNREPORTED);
        let quota = parse(&catalog, None, None).expect("a bucket with no fraction is still a row");

        assert!(!quota.fraction_reported, "a 0 here is a sentinel");
        assert_eq!(quota.total, 0.0);
        assert_eq!(quota.used, 0.0);
        assert_eq!(quota.remaining, 0.0);
        assert_eq!(quota.remaining_percentage, 0.0);
        assert!(
            !quota.is_exhausted(),
            "an omitted fraction is not exhaustion"
        );
        assert_eq!(quota.reported_percentage(), None);
        assert_eq!(quota.source.as_deref(), Some("fetchAvailableModels"));
        // The reset instant upstream did send is still carried.
        assert!(quota.reset_at.is_some());
    }

    #[test]
    fn a_full_fraction_with_no_reset_is_unlimited() {
        let catalog = payload(CATALOG_UNLIMITED);
        let quota = parse(&catalog, None, None).expect("the model yields a quota");
        assert!(quota.unlimited);
        assert!(!quota.is_exhausted());
        assert_eq!(quota.total, 0.0);
        assert_eq!(quota.used, 0.0);
    }

    #[test]
    fn the_live_read_wins_and_sets_the_source() {
        let catalog = payload(CATALOG_REPORTED);
        let live = payload(LIVE_LOWER);
        let quota = parse(&catalog, Some(&live), None).expect("catalog plus live yields a quota");

        assert_eq!(quota.source.as_deref(), Some("retrieveUserQuota"));
        assert_eq!(quota.used, 900.0);
        assert_eq!(quota.remaining, 100.0);
        assert_eq!(quota.remaining_percentage, 10.0);
    }

    #[test]
    fn the_weekly_groups_envelope_is_parsed() {
        let summary = payload(
            r#"{
                "groups": [
                    {"displayName": "Gemini Models", "buckets": [
                        {"bucketId": "5h", "displayName": "5 hour limit", "remainingFraction": 0.5},
                        {"bucketId": "weekly", "displayName": "Weekly limit",
                         "remainingFraction": 0.8, "resetTime": "2026-01-09T00:00:00Z"}
                    ]},
                    {"displayName": "Claude and GPT models", "buckets": [
                        {"bucketId": "weekly", "displayName": "Weekly limit",
                         "remainingFraction": 0.4, "resetTime": "2026-01-09T00:00:00Z"}
                    ]}
                ]
            }"#,
        );
        let windows = parse_weekly_summary(&summary);
        let keys: Vec<&str> = windows
            .iter()
            .map(|window| window.key.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(keys, ["gemini_weekly", "claude_gpt_weekly"]);
        assert_eq!(windows[0].used, 200.0);
        assert_eq!(windows[0].remaining, 800.0);
        assert_eq!(windows[0].remaining_percentage, 80.0);
        assert_eq!(windows[0].family, WindowFamily::Weekly1w);

        // The weekly rows can be the whole account-level answer. The aggregate is the most
        // constrained window, so the Claude/GPT group (40% left) wins over the Gemini one.
        let quota =
            parse(&payload("{}"), None, Some(&summary)).expect("weekly only yields a quota");
        assert_eq!(quota.source.as_deref(), Some("retrieveUserQuotaSummary"));
        assert_eq!(quota.used, 600.0);
        assert_eq!(quota.remaining, 400.0);
        assert_eq!(quota.display_name.as_deref(), Some("Claude and GPT models"));
    }

    #[test]
    fn the_alternative_quota_summary_envelope_is_parsed() {
        let summary = payload(
            r#"{"quotaSummary": {"groups": [
                {"displayName": "Gemini Models", "buckets": [
                    {"bucketId": "weekly-limit", "displayName": "Weekly",
                     "remainingFraction": 0.75, "resetTime": "2026-01-09T00:00:00Z"}
                ]}
            ]}}"#,
        );
        let windows = parse_weekly_summary(&summary);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].key.as_deref(), Some("gemini_weekly"));
        assert_eq!(windows[0].remaining, 750.0);
    }

    #[test]
    fn a_disabled_or_never_started_weekly_bucket_is_skipped() {
        let summary = payload(
            r#"{
                "groups": [
                    {"displayName": "Gemini Models", "buckets": [
                        {"bucketId": "weekly", "displayName": "Weekly", "disabled": true,
                         "remainingFraction": 0.8}
                    ]},
                    {"displayName": "Claude and GPT models", "buckets": [
                        {"bucketId": "weekly", "displayName": "Weekly"}
                    ]}
                ]
            }"#,
        );
        assert!(parse_weekly_summary(&summary).is_empty());
    }

    #[test]
    fn base_url_fallback_tries_the_next_candidate() {
        // The first candidate 404s for the catalog; the second answers.
        let http = Arc::new(FakeHttp::new(
            vec![
                (
                    daily(FETCH_AVAILABLE_MODELS_PATH).as_str(),
                    reply(404, "{}"),
                ),
                (
                    public(FETCH_AVAILABLE_MODELS_PATH).as_str(),
                    reply(200, CATALOG_REPORTED),
                ),
            ],
            reply(404, "{}"),
        ));
        let ctx = context(Arc::clone(&http));
        let quota = block_on(Antigravity::new().fetch(&ctx)).expect("fallback succeeds");
        assert_eq!(quota.remaining, 250.0);

        // The catalog was tried on both hosts, in order, and the bodies carry the project.
        let seen = http.seen();
        let catalog_calls: Vec<String> = seen
            .iter()
            .filter(|request| request.url.contains(FETCH_AVAILABLE_MODELS_PATH))
            .map(|request| request.url.clone())
            .collect();
        assert_eq!(
            catalog_calls,
            vec![
                daily(FETCH_AVAILABLE_MODELS_PATH),
                public(FETCH_AVAILABLE_MODELS_PATH)
            ]
        );
        let first = &seen[0];
        assert_eq!(first.method, http::Method::POST);
        assert_eq!(
            header(first, "authorization"),
            Some("Bearer test-token-not-a-real-secret")
        );
        assert_eq!(
            header(first, "user-agent"),
            Some("antigravity/ide/2.1.1 darwin/arm64")
        );
        assert_eq!(header(first, "content-type"), Some("application/json"));
        assert_eq!(
            serde_json::from_slice::<Value>(&first.body).expect("body is JSON"),
            json!({"project": "test-project-id"})
        );
    }

    #[test]
    fn a_401_is_an_error_and_never_a_fabricated_quota() {
        let http = Arc::new(FakeHttp::new(Vec::new(), reply(401, "{}")));
        let ctx = context(Arc::clone(&http));
        let error = block_on(Antigravity::new().fetch(&ctx)).expect_err("401 is an error");
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        assert!(!format!("{error:?}").contains("test-token"));
        // A rejected credential stops at the first candidate: walking the rest cannot help.
        assert_eq!(http.seen().len(), 1);
    }

    #[test]
    fn a_catalog_with_no_usable_bucket_is_not_found() {
        let http = Arc::new(FakeHttp::new(
            vec![(
                daily(FETCH_AVAILABLE_MODELS_PATH).as_str(),
                reply(200, r#"{"models": {"gemini-3-pro": {"quotaInfo": {}}}}"#),
            )],
            reply(404, "{}"),
        ));
        let ctx = context(Arc::clone(&http));
        assert!(matches!(
            block_on(Antigravity::new().fetch(&ctx)),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn the_weekly_read_is_best_effort_and_the_cli_profile_has_its_own_user_agent() {
        let http = Arc::new(FakeHttp::new(
            vec![
                (
                    daily(FETCH_AVAILABLE_MODELS_PATH).as_str(),
                    reply(200, r#"{"models": {}}"#),
                ),
                (
                    daily(RETRIEVE_USER_QUOTA_SUMMARY_PATH).as_str(),
                    reply(
                        200,
                        r#"{"groups": [{"displayName": "Gemini Models", "buckets": [
                            {"bucketId": "weekly", "displayName": "Weekly",
                             "remainingFraction": 0.5, "resetTime": "2026-01-09T00:00:00Z"}
                        ]}]}"#,
                    ),
                ),
            ],
            reply(404, "{}"),
        ));
        let http_dyn: Arc<dyn QuotaHttp> = http.clone();
        let ctx = QuotaFetchContext::new("antigravity", Dialect::Antigravity, http_dyn)
            .with_account(
                QuotaAccount::new()
                    .with_extra("cloudcode_project_id", json!("test-project-id"))
                    .with_extra("clientProfile", json!("cli")),
            )
            .with_credential("test-token-not-a-real-secret");
        let quota = block_on(Antigravity::new().fetch(&ctx)).expect("weekly answer is enough");
        assert_eq!(quota.source.as_deref(), Some("retrieveUserQuotaSummary"));
        assert_eq!(quota.remaining, 500.0);

        let seen = http.seen();
        let ua = header(&seen[0], "user-agent").expect("a user agent");
        assert!(ua.starts_with("antigravity/cli/1.1.5"), "{ua}");
    }

    #[test]
    fn supports_only_oauth_antigravity_accounts() {
        let fetcher = Antigravity::new();
        let oauth = QuotaAccount {
            oauth: true,
            ..QuotaAccount::new()
        };
        let api_key = QuotaAccount::new();
        assert_eq!(fetcher.id(), "antigravity");
        assert!(fetcher.supports(Dialect::Antigravity, "antigravity", &oauth));
        assert!(fetcher.supports(Dialect::Google, "agy", &oauth));
        assert!(fetcher.supports(Dialect::Antigravity, "google", &oauth));
        assert!(!fetcher.supports(Dialect::Google, "antigravity", &api_key));
        assert!(!fetcher.supports(Dialect::OpenAi, "openrouter", &oauth));
    }

    #[test]
    fn the_project_id_is_read_from_the_migration_key_and_malformed_ids_are_ignored() {
        let account = QuotaAccount::new()
            .with_extra("projectId", json!("from-omniroute"))
            .with_extra("cloudcode_project_id", json!("from-migration"));
        assert_eq!(project_id(&account).as_deref(), Some("from-migration"));

        let object =
            QuotaAccount::new().with_extra("cloudcode_project_id", json!({"id": "nested-project"}));
        assert_eq!(project_id(&object).as_deref(), Some("nested-project"));

        let malformed = QuotaAccount::new().with_extra("cloudcode_project_id", json!("bad id!"));
        assert_eq!(project_id(&malformed), None);
        assert_eq!(project_id(&QuotaAccount::new()), None);
    }

    #[test]
    fn the_allowance_family_follows_the_upstream_classifier() {
        assert_eq!(quota_family("gemini-3-pro"), "gemini");
        assert_eq!(quota_family("antigravity/gemini-3.6-flash-high"), "gemini");
        assert_eq!(quota_family("claude-sonnet-4-5"), "claude");
        assert_eq!(quota_family("agy/cloud-code-1"), "claude");
        assert_eq!(quota_family("gpt-oss-120b"), "other");
    }

    /// The weekly summary must not leak a five-hour bucket into a weekly key.
    #[test]
    fn a_five_hour_summary_bucket_is_not_weekly() {
        let summary = payload(
            r#"{"groups": [{"displayName": "Gemini Models", "buckets": [
                {"bucketId": "5h", "displayName": "5 hour limit", "remainingFraction": 0.1}
            ]}]}"#,
        );
        assert!(parse_weekly_summary(&summary).is_empty());
        assert!(parse(&payload("{}"), None, Some(&summary)).is_none());
    }

    /// `EMPTY` keeps the live read's empty shape in one obvious place for the source tests.
    #[test]
    fn an_empty_live_read_falls_back_to_the_catalog_source() {
        let catalog = payload(CATALOG_REPORTED);
        let live = payload(EMPTY);
        let quota = parse(&catalog, Some(&live), None).expect("catalog still answers");
        assert_eq!(quota.source.as_deref(), Some("fetchAvailableModels"));
    }
}
