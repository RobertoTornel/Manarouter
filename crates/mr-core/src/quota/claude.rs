//! Anthropic Claude (Claude Code OAuth) usage.
//!
//! Ported from OmniRoute's per-provider usage fetcher (MIT, © 2026 diegosouzapw):
//! `open-sse/services/usage/claude.ts` (`getClaudeUsage`) and
//! `open-sse/services/usage/claudeQuota.ts` (`normalizeClaudeUsageQuotas`). The endpoint is the
//! undocumented Claude Code OAuth usage read at `https://api.anthropic.com/api/oauth/usage`.
//!
//! Four details are deliberate, because they are what keeps the panel honest:
//!
//! * **The `anthropic-beta` header is not optional.** `oauth-2025-04-20` is what makes this
//!   undocumented endpoint answer at all. It is asserted in a test so a future refactor cannot
//!   quietly drop it.
//! * **`utilization` is a percentage used, not a count.** `total` is always `100`, so
//!   `remaining = max(0, 100 - used)`. The payload reports the fraction, so the reported
//!   constructor is used.
//! * **A missing percentage is a sentinel, not exhaustion.** The current `limits[]` shape may omit
//!   `percent`/`utilization`; that row becomes [`UsageQuota::from_usage_unreported`] with
//!   `fraction_reported = false`, never a fabricated `0%`. This is the whole point of the flag.
//! * **`isActive`/`severity` are kept.** Upstream, whether a window blocks routing is decided by
//!   those two fields, not by the percentage, so they ride along as detail rows.
//!
//! Both response shapes are accepted. The legacy shape is `five_hour` / `seven_day` /
//! `seven_day_<codename>`; the current shape is a `limits[]` array whose entries carry
//! `kind: session | weekly_all | weekly_scoped`. A payload may contain both; the `limits[]` entry
//! wins for a key it repeats, exactly as `normalizeClaudeUsageQuotas` does.
//!
//! Only the OAuth endpoint is ported. OmniRoute's legacy fallback (`/v1/settings` plus the
//! per-organization usage endpoint) needs an organization id that the account model here does not
//! carry, and the OAuth read is the surface a Claude Code account actually uses.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, QuotaWindow, UsageQuota, WindowFamily};

/// The Claude Code OAuth usage read.
pub const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";

/// The beta flag the undocumented endpoint requires.
pub const OAUTH_BETA: &str = "oauth-2025-04-20";

/// The captured `claude-cli` release the `User-Agent` is pinned to, from OmniRoute's
/// `shared/constants/claudeCodeClient.ts`.
pub const CLAUDE_CODE_VERSION: &str = "2.1.280";

/// Which upstream call produced a row. Kept stable so a cached snapshot stays readable.
const SOURCE: &str = "oauthUsage";

/// Five hours, the real length of a `session` / `five_hour` window.
const SESSION_SECONDS: u64 = 5 * 60 * 60;
/// Seven days, the real length of a `weekly_*` / `seven_day*` window.
const WEEKLY_SECONDS: u64 = 7 * 24 * 60 * 60;

/// Provider ids this fetcher answers for. `claude` is the canonical id; the other two are the
/// spellings an operator is likely to have configured for the same Claude Code account.
const PROVIDER_IDS: [&str; 3] = ["claude", "anthropic", "claude-code"];

/// Anthropic Claude quota, read from the Claude Code OAuth usage endpoint.
#[derive(Debug, Clone, Copy, Default)]
pub struct Claude;

impl Claude {
    /// The registry id.
    pub const ID: &str = "claude";

    /// A fetcher instance, for `QuotaRegistry::register`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for Claude {
    fn id(&self) -> &'static str {
        Self::ID
    }

    /// Claude Code's usage endpoint is OAuth-only, so an API-key account is left to whatever
    /// fetcher claims its dialect rather than being sent a request that can only 401.
    fn supports(&self, _dialect: Dialect, provider_id: &str, account: &QuotaAccount) -> bool {
        account.oauth && PROVIDER_IDS.contains(&provider_id)
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(
                    "claude quota: the account has no OAuth credential to present".to_owned(),
                )
            })?;

        let request = QuotaRequest::get(USAGE_URL)
            .header("Accept", "application/json, text/plain, */*")
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("claude-code/{CLAUDE_CODE_VERSION}"))
            // The undocumented endpoint only answers when this beta is requested.
            .header("anthropic-beta", OAUTH_BETA)
            .bearer(credential.expose())
            .timeout(Duration::from_secs(10));

        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(status_error(response.status));
        }

        let payload = response.json()?;
        parse(&payload).ok_or_else(|| {
            Error::NotFound(
                "claude quota: the response carried no recognisable usage window".to_owned(),
            )
        })
    }
}

/// One window as parsed from the payload, before it is collapsed into an account-level quota.
#[derive(Debug, Clone, PartialEq)]
struct Window {
    /// The semantic key: `session`, `weekly_all`, or `weekly_scoped:<scope>`.
    key: String,
    /// The operator-facing key, shaped as upstream: `session (5h)`, `weekly (7d)`,
    /// `weekly scoped (7d) [model:…]`.
    display_key: String,
    /// The duration band the real window length implies.
    family: WindowFamily,
    /// Percentage used, `0.0..=100.0`.
    used: f64,
    /// Whether the upstream actually reported the percentage.
    reported: bool,
    /// The raw reset value, handed to the `from_usage*` constructors.
    reset: Value,
    /// `kind` / `isActive` / `severity` / scope, kept because they decide routing upstream.
    metadata: Vec<(&'static str, String)>,
}

impl Window {
    /// The normalised quota for this window, through the constructors so the sentinel rule holds.
    fn to_usage_quota(&self) -> UsageQuota {
        if self.reported {
            UsageQuota::from_usage(self.used, 100.0, &self.reset)
        } else {
            UsageQuota::from_usage_unreported(self.used, 100.0, &self.reset)
        }
    }

    /// The detail-row name: the operator-facing key, the **semantic key**, and the
    /// `kind` / `isActive` / `severity` / scope metadata upstream routes on.
    ///
    /// The semantic key is carried in the row, not just the display name, so a detail row can be
    /// traced back to the window it came from — without it a `weekly scoped` row is unattributable
    /// when two scoped windows share a display label.
    fn detail_name(&self) -> String {
        let mut parts = vec![format!("key={}", self.key)];
        parts.extend(
            self.metadata
                .iter()
                .map(|(name, value)| format!("{name}={value}")),
        );
        format!("{} [{}]", self.display_key, parts.join(" "))
    }
}

/// Every window a payload describes, keyed as upstream keys it.
///
/// This is the pure parser: no network, no state. Exposed so a caller (or a test) can see each
/// window individually, since [`UsageQuota`] itself carries only the aggregate.
pub fn parse_windows(payload: &Value) -> Vec<QuotaWindow> {
    windows(payload)
        .into_iter()
        .map(|window| {
            let mut out = QuotaWindow::from_quota(window.family, &window.to_usage_quota())
                .with_key(window.key.clone());
            out.display_name = Some(window.display_key.clone());
            out
        })
        .collect()
}

/// The plan label the payload declares: the first non-empty `tier`, `plan` or `subscription_type`.
///
/// Mirrors `getClaudePlanLabel`, including its refusal to report the placeholder labels
/// `claude code` and `unknown`.
pub fn plan_label(payload: &Value) -> Option<String> {
    for key in ["tier", "plan", "subscription_type"] {
        let Some(text) = payload.get(key).and_then(Value::as_str) else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.is_empty()
            || trimmed.eq_ignore_ascii_case("claude code")
            || trimmed.eq_ignore_ascii_case("unknown")
        {
            continue;
        }
        return Some(trimmed.to_owned());
    }
    None
}

/// Collapse a payload into one account-level quota, or `None` when it carries no usage window.
///
/// The account-wide windows (`session`, `weekly_all`) win over a model-scoped one, and among the
/// candidates the most constrained reported window is chosen — the one that would block routing
/// first. Every window, chosen or not, is attached as a [`QuotaDetail`] so nothing is dropped.
pub fn parse(payload: &Value) -> Option<UsageQuota> {
    let parsed = windows(payload);
    let primary = pick_primary(&parsed)?;

    let mut quota = primary.to_usage_quota().with_source(SOURCE);
    if let Some(label) = plan_label(payload) {
        quota = quota.with_display_name(label);
    }
    for window in &parsed {
        quota = quota.with_detail(QuotaDetail {
            name: window.detail_name(),
            used: window.used,
        });
    }
    Some(quota)
}

/// The binding window: the `session` window when the payload has one, else `weekly_all`, else any
/// window — and within that pool the tightest reported one, then only last an unreported one
/// (unknown is not exhaustion).
///
/// The session window outranks `weekly_all` because it is the one that blocks a request *now*: a
/// 5-hour window at 12.5% used is binding even while the 7-day window sits at 40%. Pooling the two
/// account-wide windows together and taking the tightest would report the weekly figure instead.
fn pick_primary(windows: &[Window]) -> Option<&Window> {
    let pool: Vec<&Window> = match windows.iter().find(|window| window.key == "session") {
        Some(session) => vec![session],
        None => match windows.iter().find(|window| window.key == "weekly_all") {
            Some(weekly) => vec![weekly],
            None => windows.iter().collect(),
        },
    };
    pool.into_iter().min_by(|a, b| rank(a).total_cmp(&rank(b)))
}

/// Sort key for [`pick_primary`]: a reported remainder first, then unknown.
fn rank(window: &Window) -> f64 {
    if window.reported {
        (100.0 - window.used).clamp(0.0, 100.0) / 100.0
    } else {
        3.0
    }
}

/// Parse both response shapes into windows, the current `limits[]` replacing a legacy key.
fn windows(payload: &Value) -> Vec<Window> {
    let mut out = legacy_windows(payload);

    if let Some(limits) = payload.get("limits").and_then(Value::as_array) {
        for (index, limit) in limits.iter().enumerate() {
            if let Some(window) = limit_window(limit, index) {
                // The current shape is authoritative for a key the legacy shape also reported.
                out.retain(|existing| existing.key != window.key);
                out.push(window);
            }
        }
    }
    out
}

/// The legacy shape: `five_hour`, `seven_day`, and `seven_day_<codename>`.
fn legacy_windows(payload: &Value) -> Vec<Window> {
    let mut out = Vec::new();

    if let Some(five_hour) = payload.get("five_hour") {
        if let Some(window) = legacy_window("session", "session (5h)", None, None, five_hour) {
            out.push(window);
        }
    }
    if let Some(seven_day) = payload.get("seven_day") {
        if let Some(window) = legacy_window("weekly_all", "weekly (7d)", None, None, seven_day) {
            out.push(window);
        }
    }

    let Some(object) = payload.as_object() else {
        return out;
    };
    for (key, value) in object {
        let Some(codename) = key.strip_prefix("seven_day_") else {
            continue;
        };
        // `seven_day_*` suffixes are Anthropic codenames; `omelette` is the one upstream already
        // maps to a public name. The raw codename is carried through in the window key and in a
        // detail row, so nothing is lost for a codename upstream has not mapped.
        let model = match codename {
            "omelette" => "designer".to_owned(),
            other => other.to_owned(),
        };
        let semantic = format!("weekly_scoped:{codename}");
        let display = format!("weekly scoped (7d) [model:{model}]");
        if let Some(window) =
            legacy_window(&semantic, &display, Some(&model), Some(codename), value)
        {
            out.push(window);
        }
    }
    out
}

/// One legacy window. Upstream returns `null` when `utilization` is absent, so this does too.
fn legacy_window(
    key: &str,
    display_key: &str,
    scope: Option<&str>,
    codename: Option<&str>,
    window: &Value,
) -> Option<Window> {
    let used = safe_percentage(window.get("utilization"))?;
    let mut metadata = Vec::new();
    if let Some(scope) = scope {
        metadata.push(("scope", format!("model:{scope}")));
    }
    if let Some(codename) = codename {
        metadata.push(("codename", codename.to_owned()));
    }
    Some(Window {
        key: key.to_owned(),
        display_key: display_key.to_owned(),
        family: family_for(key),
        used,
        reported: true,
        reset: window.get("resets_at").cloned().unwrap_or(Value::Null),
        metadata,
    })
}

/// One entry of the current `limits[]` shape.
fn limit_window(limit: &Value, index: usize) -> Option<Window> {
    let kind = limit.get("kind").and_then(Value::as_str)?;
    if !matches!(kind, "session" | "weekly_all" | "weekly_scoped") {
        return None;
    }

    let percent = limit_percent(limit);
    let model = limit_model(limit);
    let surface = limit
        .get("scope")
        .and_then(|scope| scope.get("surface"))
        .and_then(|surface| surface.get("display_name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty());

    // The scope token names the key. An unresolved scope is numbered so two of them stay distinct,
    // exactly as upstream's `#<index>` suffix does.
    let (key, display_key) = match kind {
        "session" => ("session".to_owned(), "session (5h)".to_owned()),
        "weekly_all" => ("weekly_all".to_owned(), "weekly (7d)".to_owned()),
        _ => {
            let scope = model
                .as_ref()
                .map(|model| model.label.as_str())
                .or(surface)
                .map(str::to_owned);
            match scope {
                Some(scope) => (
                    format!("weekly_scoped:{}", normalize_token(&scope)),
                    format!("weekly scoped (7d) [model:{scope}]"),
                ),
                None => (
                    format!("weekly_scoped:unresolved#{index}"),
                    format!("weekly scoped (7d) [model:unresolved #{index}]"),
                ),
            }
        }
    };

    let mut metadata = vec![
        ("kind", kind.to_owned()),
        (
            "active",
            limit
                .get("isActive")
                .or_else(|| limit.get("is_active"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .to_string(),
        ),
    ];
    if let Some(severity) = limit.get("severity").and_then(Value::as_str).map(str::trim) {
        if !severity.is_empty() {
            metadata.push(("severity", severity.to_owned()));
        }
    }
    if let Some(model) = &model {
        metadata.push((
            "model",
            model.id.clone().unwrap_or_else(|| model.label.clone()),
        ));
    }
    if let Some(surface) = surface {
        metadata.push(("surface", surface.to_owned()));
    }

    Some(Window {
        family: family_for(&key),
        key,
        display_key,
        used: percent.unwrap_or(0.0),
        reported: percent.is_some(),
        reset: ["resetsAt", "resets_at", "resetAt", "reset_at"]
            .iter()
            .find_map(|name| limit.get(*name))
            .cloned()
            .unwrap_or(Value::Null),
        metadata,
    })
}

/// The scope model of a `limits[]` entry, from either a bare string or an object.
struct LimitModel {
    /// The model id, when the payload gave one.
    id: Option<String>,
    /// The best human label: display name, then id.
    label: String,
}

fn limit_model(limit: &Value) -> Option<LimitModel> {
    let model = limit.get("scope")?.get("model")?;
    if let Some(id) = model.as_str().map(str::trim).filter(|id| !id.is_empty()) {
        return Some(LimitModel {
            id: Some(id.to_owned()),
            label: id.to_owned(),
        });
    }
    let id = ["id", "model_id", "modelId"]
        .iter()
        .find_map(|key| non_empty_str(model.get(*key)));
    let display = ["displayName", "display_name", "name"]
        .iter()
        .find_map(|key| non_empty_str(model.get(*key)));
    let label = display.or(id)?.to_owned();
    Some(LimitModel {
        id: id.map(str::to_owned),
        label,
    })
}

/// The percentage a `limits[]` entry reports, requiring the `0..=100` range upstream enforces.
fn limit_percent(limit: &Value) -> Option<f64> {
    ["percent", "utilization", "percentage"]
        .iter()
        .find_map(|key| safe_percentage(limit.get(*key)))
        .filter(|percent| (0.0..=100.0).contains(percent))
}

/// The family a semantic key belongs to, from the window's *real* length.
fn family_for(key: &str) -> WindowFamily {
    if key == "session" {
        WindowFamily::from_seconds(SESSION_SECONDS)
    } else {
        WindowFamily::from_seconds(WEEKLY_SECONDS)
    }
}

/// A finite percentage from a JSON number, or from a numeric string some captures use.
fn safe_percentage(value: Option<&Value>) -> Option<f64> {
    let value = value?;
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// The lowercased, hyphen-joined token upstream uses to keep unresolved scopes distinct.
fn normalize_token(value: &str) -> String {
    value
        .to_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// An upstream status as an error. Never invents a quota for a failed read.
fn status_error(status: u16) -> Error {
    match status {
        401 | 403 => Error::Config(format!(
            "claude quota: the upstream rejected the credential (status {status})"
        )),
        404 => Error::NotFound(
            "claude quota: the OAuth usage endpoint was not found for this account".to_owned(),
        ),
        _ => Error::Io(std::io::Error::other(format!(
            "claude quota: the upstream returned status {status}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};
    use crate::quota::types::parse_reset_time;
    use bytes::Bytes;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// A transport that replays canned replies and records what it was asked to send.
    #[derive(Debug)]
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
            self.seen.lock().expect("fake http poisoned").clone()
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen.lock().expect("fake http poisoned").push(request);
            self.replies
                .lock()
                .expect("fake http poisoned")
                .pop_front()
                .ok_or_else(|| Error::Io(std::io::Error::other("fake http: no reply left")))
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

    fn context(http: Arc<FakeHttp>) -> QuotaFetchContext {
        QuotaFetchContext::new("claude", Dialect::Anthropic, http)
            .with_account(QuotaAccount::new())
            .with_credential("test-token-not-a-real-secret")
    }

    fn payload(text: &str) -> Value {
        serde_json::from_str(text).expect("test payload is valid JSON")
    }

    /// Captured shape of the legacy payload.
    const LEGACY: &str = r#"{
        "five_hour": {"utilization": 12.5, "resets_at": "2026-01-02T03:04:05Z"},
        "seven_day": {"utilization": 40, "resets_at": 1767234245},
        "seven_day_omelette": {"utilization": 3, "resets_at": "2026-01-09T00:00:00Z"},
        "tier": "max"
    }"#;

    /// Captured shape of the current payload.
    const CURRENT: &str = r#"{
        "limits": [
            {"kind": "session", "utilization": 25.0, "resets_at": "2026-01-02T08:00:00Z",
             "isActive": true, "severity": "warning"},
            {"kind": "weekly_all", "percent": 60, "resetsAt": "2026-01-05T00:00:00Z",
             "isActive": false, "severity": "normal"},
            {"kind": "weekly_scoped", "percent": 5, "reset_at": "2026-01-09T00:00:00Z",
             "isActive": true, "severity": "critical",
             "scope": {"model": {"id": "claude-opus-4-6", "display_name": "Opus"}}}
        ],
        "subscription_type": "pro"
    }"#;

    /// A `weekly_scoped` row that never reported a percentage.
    const MISSING_PERCENT: &str = r#"{
        "limits": [
            {"kind": "weekly_scoped", "resets_at": "2026-01-09T00:00:00Z",
             "isActive": false, "severity": "normal",
             "scope": {"model": {"id": "claude-opus-4-6", "display_name": "Opus"}}}
        ]
    }"#;

    #[test]
    fn legacy_shape_maps_percentages_and_keeps_the_codename() {
        let parsed = payload(LEGACY);
        let quota = parse(&parsed).expect("legacy payload yields a quota");

        // `session` is account-wide and the tightest, so it is the aggregate.
        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.remaining, 87.5);
        assert_eq!(quota.remaining_percentage, 87.5);
        assert!(quota.fraction_reported);
        assert_eq!(quota.display_name.as_deref(), Some("max"));
        assert_eq!(
            quota.reset_at,
            parse_reset_time(&payload(LEGACY)["five_hour"]["resets_at"])
        );

        let windows = parse_windows(&parsed);
        let keys: Vec<&str> = windows
            .iter()
            .map(|window| window.key.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(keys, ["session", "weekly_all", "weekly_scoped:omelette"]);
        // The codename is carried through, and the duration decides the family.
        assert_eq!(
            windows[2].display_name.as_deref(),
            Some("weekly scoped (7d) [model:designer]")
        );
        assert_eq!(windows[0].family, WindowFamily::Session5h);
        assert_eq!(windows[1].family, WindowFamily::Weekly1w);
        assert_eq!(windows[2].family, WindowFamily::Weekly1w);
        assert_eq!(windows[1].used, 40.0);
        assert_eq!(windows[1].reset_at, Some(1_767_234_245_000));
        let detail = quota
            .details
            .iter()
            .find(|detail| detail.name.contains("weekly_scoped:omelette"))
            .expect("the legacy scoped window is kept as a detail row");
        assert!(detail.name.contains("codename=omelette"), "{}", detail.name);
    }

    #[test]
    fn current_limits_shape_wins_and_records_metadata() {
        let parsed = payload(CURRENT);
        let quota = parse(&parsed).expect("limits payload yields a quota");

        assert_eq!(quota.display_name.as_deref(), Some("pro"));
        assert!(quota.fraction_reported);
        assert_eq!(quota.source.as_deref(), Some("oauthUsage"));
        // The most constrained account-wide window (25%) is the aggregate.
        assert_eq!(quota.used, 25.0);
        assert_eq!(quota.remaining, 75.0);

        let windows = parse_windows(&parsed);
        let scoped = windows
            .iter()
            .find(|window| window.key.as_deref() == Some("weekly_scoped:opus"))
            .expect("scoped window is keyed by its model");
        assert_eq!(scoped.used, 5.0);
        assert_eq!(scoped.remaining, 95.0);
        assert_eq!(
            scoped.display_name.as_deref(),
            Some("weekly scoped (7d) [model:Opus]")
        );

        // `isActive`/`severity` must survive: upstream they decide whether the window blocks.
        let detail = quota
            .details
            .iter()
            .find(|detail| detail.name.contains("weekly_scoped:opus"))
            .expect("the scoped window is kept as a detail row");
        assert!(detail.name.contains("active=true"), "{}", detail.name);
        assert!(detail.name.contains("severity=critical"), "{}", detail.name);
        assert!(
            detail.name.contains("model=claude-opus-4-6"),
            "{}",
            detail.name
        );
        assert_eq!(detail.used, 5.0);
    }

    #[test]
    fn weekly_scoped_keying_prefers_the_model_id() {
        let parsed = payload(CURRENT);
        let windows = parse_windows(&parsed);
        assert!(windows
            .iter()
            .any(|window| window.key.as_deref() == Some("weekly_scoped:opus")));
        // A bare-string scope is keyed the same way.
        let bare = payload(
            r#"{"limits":[{"kind":"weekly_scoped","percent":1,"scope":{"model":"claude-sonnet-4-5"}}]}"#,
        );
        let window = parse_windows(&bare)
            .into_iter()
            .next()
            .expect("bare scope yields a window");
        assert_eq!(
            window.key.as_deref(),
            Some("weekly_scoped:claude-sonnet-4-5")
        );
    }

    #[test]
    fn a_missing_percentage_is_a_sentinel_not_exhaustion() {
        let parsed = payload(MISSING_PERCENT);
        let quota = parse(&parsed).expect("a window with no percentage is still a window");

        assert!(!quota.fraction_reported, "a 0 here is a sentinel");
        assert_eq!(quota.used, 0.0);
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.remaining, 0.0);
        assert_eq!(quota.remaining_percentage, 0.0);
        assert!(
            !quota.is_exhausted(),
            "an omitted percentage is not exhaustion"
        );
        assert_eq!(quota.reported_percentage(), None);
        // The reset instant upstream did send is still carried.
        assert_eq!(quota.reset_at, Some(1_767_916_800_000));
    }

    #[test]
    fn the_beta_header_and_auth_are_sent() {
        let http = Arc::new(FakeHttp::new(vec![reply(200, LEGACY)]));
        let ctx = context(Arc::clone(&http));
        let quota = block_on(Claude::new().fetch(&ctx)).expect("fetch succeeds");
        assert_eq!(quota.total, 100.0);

        let seen = http.seen();
        assert_eq!(seen.len(), 1);
        let request = &seen[0];
        assert_eq!(request.method, http::Method::GET);
        assert_eq!(request.url, USAGE_URL);
        assert_eq!(header(request, "anthropic-beta"), Some(OAUTH_BETA));
        assert_eq!(
            header(request, "authorization"),
            Some("Bearer test-token-not-a-real-secret")
        );
        assert_eq!(header(request, "user-agent"), Some("claude-code/2.1.280"));
        assert_eq!(
            header(request, "accept"),
            Some("application/json, text/plain, */*")
        );
        assert_eq!(header(request, "content-type"), Some("application/json"));
    }

    #[test]
    fn a_401_is_an_error_and_never_a_fabricated_quota() {
        let http = Arc::new(FakeHttp::new(vec![reply(401, "{}")]));
        let ctx = context(Arc::clone(&http));
        let error = block_on(Claude::new().fetch(&ctx)).expect_err("401 is an error");
        assert!(matches!(error, Error::Config(_)), "{error:?}");
        // The credential never appears in the message.
        assert!(!format!("{error:?}").contains("test-token"));
    }

    #[test]
    fn a_403_is_an_error_and_an_empty_payload_is_not_found() {
        let http = Arc::new(FakeHttp::new(vec![reply(403, "{}")]));
        let ctx = context(Arc::clone(&http));
        assert!(matches!(
            block_on(Claude::new().fetch(&ctx)),
            Err(Error::Config(_))
        ));

        let http = Arc::new(FakeHttp::new(vec![reply(200, "{}")]));
        let ctx = context(Arc::clone(&http));
        assert!(matches!(
            block_on(Claude::new().fetch(&ctx)),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn supports_only_oauth_accounts_with_a_claude_provider_id() {
        let claude = Claude::new();
        let oauth = QuotaAccount {
            oauth: true,
            ..QuotaAccount::new()
        };
        let api_key = QuotaAccount::new();
        assert_eq!(claude.id(), "claude");
        assert!(claude.supports(Dialect::Anthropic, "claude", &oauth));
        assert!(claude.supports(Dialect::Anthropic, "anthropic", &oauth));
        assert!(claude.supports(Dialect::OpenAi, "claude-code", &oauth));
        assert!(!claude.supports(Dialect::Anthropic, "claude", &api_key));
        assert!(!claude.supports(Dialect::Anthropic, "openrouter", &oauth));
    }

    #[test]
    fn an_unparsable_reset_becomes_none_rather_than_a_guess() {
        let parsed = payload(r#"{"five_hour":{"utilization":50,"resets_at":"soon"}}"#);
        let quota = parse(&parsed).expect("payload yields a quota");
        assert_eq!(quota.reset_at, None);
        // And the sentinel flag does not move.
        assert!(quota.fraction_reported);
        let window = parse_windows(&parsed).remove(0);
        assert_eq!(window.reset_at, None);
    }

    #[test]
    fn a_limits_row_replaces_the_legacy_key_it_repeats() {
        let parsed = payload(
            r#"{
                "five_hour": {"utilization": 90, "resets_at": 1767234245},
                "limits": [{"kind": "session", "utilization": 10, "resets_at": 1767234245}]
            }"#,
        );
        let windows = parse_windows(&parsed);
        assert_eq!(windows.len(), 1, "the current shape wins for the same key");
        assert_eq!(windows[0].used, 10.0);
    }
}
