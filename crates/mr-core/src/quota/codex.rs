//! OpenAI Codex / ChatGPT subscription usage.
//!
//! Ported from OmniRoute's `open-sse/services/usage/codex.ts`,
//! `open-sse/services/codexUsageQuotas.ts`, `open-sse/config/codexClient.ts` and
//! `open-sse/config/codexQuotaScopes.ts` (MIT, © 2026 diegosouzapw). Implemented by task C1;
//! see `plan.md` section 4, Feature 1.
//!
//! One upstream call, `GET https://chatgpt.com/backend-api/wham/usage`, returns a payload whose
//! `rate_limit` carries a percentage — `used_percent` — rather than a remaining fraction. That
//! matters for the module's first rule: a reported `used_percent: 0` is a real "nothing used yet",
//! not the `0` sentinel an upstream sends when it omits the fraction, so this fetcher builds its
//! quota with [`UsageQuota::from_usage`] (reported) and never with the unreported constructor.
//!
//! Two porting decisions worth stating:
//!
//! * **A window is placed by its length, not its position.** ChatGPT reports
//!   `limit_window_seconds`, and a `primary_window` that is really seven days long must not be
//!   rendered as a five-hour session. Each window's family comes from
//!   [`WindowFamily::from_seconds`] rather than from primary/secondary ordering.
//! * **A latent window is dropped.** A window that has never started reports `used_percent: 0`
//!   with a reset that still spans the whole window (`reset_after_seconds >= limit_window_seconds`)
//!   and recomputes that reset on every fetch, so it is not actionable. [`is_latent`] implements
//!   upstream's `isLatentWindow`.
//!
//! The single returned [`UsageQuota`] is the *governing* window (the first non-latent one) and
//! carries every other window and flag as a [`QuotaDetail`] row, because the normalised shape has
//! one fraction but the payload has several buckets.

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{QuotaDetail, UsageQuota, WindowFamily};

/// The ChatGPT backend usage endpoint (`CODEX_CONFIG.usageUrl` in `usage/codex.ts`).
pub const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// The `originator` the Codex CLI sends. **Reverse-engineered and version-gated**: OpenAI gates
/// this backend on the official client's identity, and the value must stay `codex_cli_rs`.
pub const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// The Codex CLI version this probe impersonates. **Reverse-engineered and version-gated**: it is
/// copied from OmniRoute (`shared/constants/codexClient.ts`), which keeps it in lockstep with the
/// `@openai/codex` pin its image ships. When upstream stops accepting the probe, bump this value —
/// do not drop the identity headers.
pub const CODEX_CLIENT_VERSION: &str = "0.156.1";

/// The `User-Agent` built from [`CODEX_ORIGINATOR`] and [`CODEX_CLIENT_VERSION`].
/// **Reverse-engineered and version-gated** for the same reason as those two; a test pins the
/// three values together so they cannot drift apart.
pub const CODEX_USER_AGENT: &str = "codex_cli_rs/0.156.1";

/// A hung upstream must not hold a control-plane refresh; the HTTP client's default is the same
/// bound, and setting it here keeps it visible next to the request it applies to.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// The Codex / ChatGPT subscription quota fetcher.
#[derive(Debug, Clone, Copy, Default)]
pub struct Codex;

impl Codex {
    /// The fetcher id, also used as the quota's `source`.
    pub const ID: &str = "codex";

    /// Provider ids whose accounts are served by [`USAGE_URL`].
    ///
    /// The catalogue's id is `openai-codex` (the live configuration uses it); `codex` and
    /// `chatgpt` are the spellings OmniRoute's dispatcher and older configurations use, and
    /// `chatgpt-codex` covers the remaining obvious alias. [`Dialect::Codex`] is accepted too, so
    /// the fetcher still answers if it is registered in the dialect tier.
    const PROVIDER_IDS: &[&str] = &["codex", "chatgpt", "openai-codex", "chatgpt-codex"];

    /// A fetcher value, for `QuotaRegistry::register`.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl QuotaFetcher for Codex {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn supports(&self, dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        dialect == Dialect::Codex
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
                    "codex quota: the account has no ChatGPT access token to present".to_owned(),
                )
            })?;

        let mut request = QuotaRequest::get(USAGE_URL)
            .header("Authorization", format!("Bearer {}", credential.expose()))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .header("User-Agent", CODEX_USER_AGENT)
            .header("originator", CODEX_ORIGINATOR)
            .header("Version", CODEX_CLIENT_VERSION)
            .timeout(REQUEST_TIMEOUT);
        // Binds the probe to the operator's selected workspace. Omitting it when the account
        // does not carry one is upstream's behaviour (no fallback workspace is guessed).
        if let Some(workspace) = workspace_id(&ctx.account) {
            request = request.header("chatgpt-account-id", workspace);
        }

        let response = ctx.http.request(request).await?;
        let status = response.status;
        if status == 401 || status == 403 {
            return Err(Error::Config(format!(
                "codex quota: upstream refused the credential (HTTP {status})"
            )));
        }

        let value = match response.json() {
            Ok(value) => value,
            Err(error) => {
                if response.is_success() {
                    return Err(error);
                }
                return Err(Error::Io(io::Error::other(format!(
                    "codex quota: upstream returned HTTP {status} with an unreadable body"
                ))));
            }
        };

        // A non-2xx whose body is still a quota payload may be read; anything else that failed is
        // an error rather than a fabricated empty quota.
        if !response.is_success() && !recognised(&value) {
            return Err(Error::Io(io::Error::other(format!(
                "codex quota: upstream returned HTTP {status}"
            ))));
        }

        let windows = collect_windows(&value);
        let shown: Vec<&Window> = windows.iter().filter(|window| !window.latent).collect();
        // The top-level quota is the first non-latent window; when *every* window is latent the
        // primary one is used anyway, because its `used_percent: 0` is a real reported value and
        // refusing to answer would be less honest than reporting an untouched allowance.
        let top = shown
            .first()
            .copied()
            .or_else(|| windows.first())
            .ok_or_else(|| {
                Error::NotFound("codex usage payload carried no quota window".to_owned())
            })?;

        let mut quota = UsageQuota::from_usage(top.used_percent, 100.0, &reset_value(top.reset_at))
            .with_source(USAGE_URL);
        if let Some(plan) = plan_type(&value) {
            quota = quota.with_display_name(plan);
        }
        for window in &shown {
            quota = quota.with_detail(window.detail());
        }
        if limit_reached(&value) {
            // A provider-specific blocking signal, not a percentage: it is kept as a flag row
            // rather than folded into the fraction, which upstream never reported.
            quota = quota.with_detail(QuotaDetail {
                name: reached_detail_name(&value),
                used: 1.0,
            });
        }
        if let Some(count) = reset_credits(&value) {
            quota = quota.with_detail(QuotaDetail {
                name: "reset_credits_available".to_owned(),
                used: count,
            });
        }

        Ok(quota)
    }
}

/// One `rate_limit` bucket after alias lookup and normalisation.
struct Window {
    /// The row name upstream uses for this bucket (`session`, `weekly`, `code_review`, …).
    key: String,
    /// The duration family, when the payload reported `limit_window_seconds`.
    family: Option<WindowFamily>,
    /// The reported percentage used, `0..=100` after the normaliser's clamp.
    used_percent: f64,
    /// Reset instant in epoch **milliseconds**, or `None` when upstream gave no usable value.
    reset_at: Option<i64>,
    /// A window that has never started (see [`is_latent`]).
    latent: bool,
}

impl Window {
    /// The operator-facing row: the upstream key plus the *real* duration family, so a seven-day
    /// `primary_window` reads `session (1w)` rather than being mistyped as a five-hour window.
    fn detail(&self) -> QuotaDetail {
        QuotaDetail {
            name: match &self.family {
                Some(family) => format!("{} ({})", self.key, family.label()),
                None => self.key.clone(),
            },
            used: self.used_percent,
        }
    }
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

/// The `additional_rate_limits` entries, as an iterator over the objects in the array.
fn additional_entries<'a>(data: &'a Value) -> impl Iterator<Item = &'a Value> + 'a {
    field(data, &["additional_rate_limits", "additionalRateLimits"])
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| entry.is_object())
}

/// The descriptor strings of an `additional_rate_limits` entry, in upstream's lookup order.
fn descriptors(entry: &Value) -> impl Iterator<Item = &str> {
    [
        "limit_name",
        "limitName",
        "metered_feature",
        "meteredFeature",
        "limit_id",
        "limitId",
        "id",
        "name",
    ]
    .into_iter()
    .filter_map(|key| entry.get(key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|value| !value.is_empty())
}

/// The best human name for an `additional_rate_limits` entry.
fn descriptor(entry: &Value) -> Option<String> {
    descriptors(entry).next().map(str::to_owned)
}

/// Whether an `additional_rate_limits` entry describes the code-review bucket.
///
/// Upstream matches `code_review` / `codex_review` / `review` (by equality) and the two underscored
/// contains-checks. This port normalises `-` and spaces to `_` first, which is a superset: a
/// hyphenated `code-review` descriptor is caught here rather than falling through.
fn is_review_descriptor(entry: &Value) -> bool {
    descriptors(entry).any(|value| {
        let normalized = value.to_lowercase().replace(['-', ' '], "_");
        normalized == "review"
            || normalized.contains("code_review")
            || normalized.contains("codex_review")
    })
}

/// The dedicated `code_review_rate_limit` block, or the first matching `additional_rate_limits`
/// entry as upstream's fallback for plans that bucket every secondary limit together.
fn review_block(data: &Value) -> Option<&Value> {
    object(data, &["code_review_rate_limit", "codeReviewRateLimit"])
        .or_else(|| additional_entries(data).find(|entry| is_review_descriptor(entry)))
}

/// Every quota bucket in the payload, in upstream's order: the two main windows, then each
/// `additional_rate_limits` entry (each contributing a primary and a secondary window), then the
/// code-review block.
fn collect_windows(data: &Value) -> Vec<Window> {
    let mut windows = Vec::new();

    if let Some(rate_limit) = object(data, &["rate_limit", "rateLimit"]) {
        push_window(
            &mut windows,
            "session",
            &["primary_window", "primaryWindow"],
            rate_limit,
        );
        push_window(
            &mut windows,
            "weekly",
            &["secondary_window", "secondaryWindow"],
            rate_limit,
        );
    }

    for entry in additional_entries(data) {
        // The review entry is emitted once, below, as `code_review`.
        if is_review_descriptor(entry) {
            continue;
        }
        let Some(rate_limit) = object(entry, &["rate_limit", "rateLimit"]) else {
            continue;
        };
        let name = descriptor(entry).unwrap_or_else(|| "additional".to_owned());
        push_window(
            &mut windows,
            name.clone(),
            &["primary_window", "primaryWindow"],
            rate_limit,
        );
        push_window(
            &mut windows,
            format!("{name}_weekly"),
            &["secondary_window", "secondaryWindow"],
            rate_limit,
        );
    }

    if let Some(review) = review_block(data) {
        push_window(
            &mut windows,
            "code_review",
            &["primary_window", "primaryWindow"],
            review,
        );
        push_window(
            &mut windows,
            "code_review_weekly",
            &["secondary_window", "secondaryWindow"],
            review,
        );
    }

    windows
}

/// Read one window out of `record` and push it, if it really is a percentage window.
fn push_window(out: &mut Vec<Window>, key: impl Into<String>, names: &[&str], record: &Value) {
    let Some(window) = object(record, names) else {
        return;
    };
    // A bucket *without* `used_percent` is not a percentage row. Upstream also treats
    // `remaining_count` as evidence that a window exists and then reports 0% — exactly the
    // sentinel-as-exhaustion lie this module's first rule forbids — so presence of the percentage
    // is the test here, and `used_percent: 0` (present, finite) passes through as a real zero.
    let Some(used_percent) = number(field(window, &["used_percent", "usedPercent"])) else {
        return;
    };
    let limit_seconds = number(field(
        window,
        &[
            "limit_window_seconds",
            "limitWindowSeconds",
            "window_seconds",
            "windowSeconds",
        ],
    ))
    .filter(|seconds| *seconds > 0.0);

    out.push(Window {
        key: key.into(),
        family: limit_seconds.map(|seconds| WindowFamily::from_seconds(seconds as u64)),
        used_percent,
        reset_at: window_reset_at(window),
        latent: is_latent(window),
    });
}

/// `parseWindowReset`: `reset_at` is a **seconds** epoch (converted to the milliseconds the
/// normalisers want), else `now + reset_after_seconds`, else `None`. A zero or absent value yields
/// `None` rather than "now", because an exhausted-looking reset would be a fabrication.
fn window_reset_at(window: &Value) -> Option<i64> {
    if let Some(seconds) = number(field(window, &["reset_at", "resetAt"])).filter(|v| *v > 0.0) {
        // The same seconds-or-milliseconds rule the rest of the module uses: a value large enough
        // to already be milliseconds (`>= 1e12`) is left alone.
        let millis = if seconds < 1e12 {
            seconds * 1000.0
        } else {
            seconds
        };
        let millis = millis as i64;
        return (millis > 0).then_some(millis);
    }
    let after = number(field(window, &["reset_after_seconds", "resetAfterSeconds"]))
        .filter(|v| *v > 0.0)?;
    Some(now_millis().saturating_add((after * 1000.0) as i64))
}

/// `isLatentWindow`: `used_percent == 0` and the reset still spans the whole window
/// (`reset_after_seconds >= limit_window_seconds`, with a reported window length). Such a window
/// updates its reset on every fetch, so it is a permanent never-started ceiling rather than
/// something an operator can act on.
fn is_latent(window: &Value) -> bool {
    let used = number(field(window, &["used_percent", "usedPercent"]));
    let limit = number(field(
        window,
        &[
            "limit_window_seconds",
            "limitWindowSeconds",
            "window_seconds",
            "windowSeconds",
        ],
    ));
    let after = number(field(window, &["reset_after_seconds", "resetAfterSeconds"]));
    matches!(
        (used, limit, after),
        (Some(used), Some(limit), Some(after)) if used == 0.0 && limit > 0.0 && after >= limit
    )
}

/// Whether the body is a quota payload at all — the test for reading a non-2xx response.
fn recognised(data: &Value) -> bool {
    object(data, &["rate_limit", "rateLimit"]).is_some()
        || object(data, &["code_review_rate_limit", "codeReviewRateLimit"]).is_some()
        || additional_entries(data).next().is_some()
}

/// `plan_type`, into the quota's display name.
fn plan_type(data: &Value) -> Option<String> {
    field(data, &["plan_type", "planType"])
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|plan| !plan.is_empty())
        .map(str::to_owned)
}

/// `rate_limit.limit_reached`.
fn limit_reached(data: &Value) -> bool {
    object(data, &["rate_limit", "rateLimit"])
        .and_then(|rate_limit| field(rate_limit, &["limit_reached", "limitReached"]))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// `rate_limit_reached_type`, which is a bare string or an object carrying a `type`.
fn reached_type(data: &Value) -> Option<String> {
    let value = field(data, &["rate_limit_reached_type", "rateLimitReachedType"])?;
    let text = match value {
        Value::String(text) => text.as_str(),
        other => field(other, &["type"]).and_then(Value::as_str)?,
    };
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The name of the blocking-signal row: `limit_reached`, or `limit_reached (weekly)` when the
/// payload says which window is blocking.
fn reached_detail_name(data: &Value) -> String {
    match reached_type(data) {
        Some(kind) => format!("limit_reached ({kind})"),
        None => "limit_reached".to_owned(),
    }
}

/// `rate_limit_reset_credits.available_count` — banked reset credits, display-only.
fn reset_credits(data: &Value) -> Option<f64> {
    let credits = object(data, &["rate_limit_reset_credits", "rateLimitResetCredits"])?;
    number(field(credits, &["available_count", "availableCount"]))
}

/// [`UsageQuota::from_usage`] wants the upstream's raw reset value; the fetcher has already
/// converted it to epoch milliseconds, and `null` means "no reset reported".
fn reset_value(reset_at: Option<i64>) -> Value {
    reset_at.map_or(Value::Null, |millis| json!(millis))
}

/// The workspace the probe should be scoped to, from the account's non-secret extras.
fn workspace_id(account: &QuotaAccount) -> Option<&str> {
    [
        "chatgpt_account_id",
        "chatgptAccountId",
        "workspace_id",
        "workspaceId",
        "account_id",
    ]
    .into_iter()
    .find_map(|key| account.extra.get(key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|value| !value.is_empty())
}

/// Wall clock in epoch milliseconds; `0` only if the host clock predates 1970, in which case a
/// relative reset is better than a panic.
fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaResponse};

    /// A real-shaped capture of `GET /backend-api/wham/usage` for a Plus account with two windows,
    /// a metered additional limit, a code-review limit and banked reset credits.
    const PAID_PAYLOAD: &str = r#"{
      "plan_type": "plus",
      "rate_limit": {
        "allowed": true,
        "limit_reached": false,
        "primary_window": {
          "used_percent": 12.5,
          "reset_at": 1767225600,
          "reset_after_seconds": 3600,
          "limit_window_seconds": 18000
        },
        "secondary_window": {
          "used_percent": 40,
          "reset_at": 1767830400,
          "reset_after_seconds": 200000,
          "limit_window_seconds": 604800
        }
      },
      "additional_rate_limits": [
        {
          "limit_name": "GPT-5.3-Codex-Spark",
          "metered_feature": "gpt_5_3_codex_spark",
          "limit_id": "codex-spark",
          "rate_limit": {
            "primary_window": {
              "used_percent": 7,
              "reset_after_seconds": 100,
              "limit_window_seconds": 18000
            },
            "secondary_window": {
              "used_percent": 2,
              "reset_after_seconds": 1000,
              "limit_window_seconds": 604800
            }
          }
        },
        {
          "limit_name": "code_review",
          "metered_feature": "code_review",
          "rate_limit": {
            "primary_window": { "used_percent": 90, "limit_window_seconds": 604800 }
          }
        }
      ],
      "code_review_rate_limit": {
        "primary_window": {
          "used_percent": 33,
          "reset_after_seconds": 120,
          "limit_window_seconds": 18000
        }
      },
      "rate_limit_reset_credits": { "available_count": 2 }
    }"#;

    /// The camelCase spelling of the same payload, as some clients and older plans report it.
    const CAMEL_PAYLOAD: &str = r#"{
      "planType": "pro",
      "rateLimit": {
        "limitReached": true,
        "primaryWindow": {
          "usedPercent": 0,
          "resetAt": 1767225600,
          "resetAfterSeconds": 60,
          "limitWindowSeconds": 2592000
        }
      },
      "additionalRateLimits": [
        {
          "limitName": "GPT-5.3-Codex-Spark",
          "meteredFeature": "gpt_5_3_codex_spark",
          "rateLimit": {
            "primaryWindow": {
              "usedPercent": 5,
              "resetAfterSeconds": 30,
              "limitWindowSeconds": 18000
            }
          }
        }
      ]
    }"#;

    /// A never-started per-feature ceiling: zero used and the reset still spans the whole window.
    const LATENT_PAYLOAD: &str = r#"{
      "plan_type": "plus",
      "rate_limit": {
        "primary_window": {
          "used_percent": 25,
          "reset_after_seconds": 1000,
          "limit_window_seconds": 18000
        }
      },
      "additional_rate_limits": [
        {
          "limit_name": "GPT-5.3-Codex-Spark",
          "metered_feature": "gpt_5_3_codex_spark",
          "rate_limit": {
            "primary_window": {
              "used_percent": 0,
              "reset_after_seconds": 18000,
              "limit_window_seconds": 18000
            }
          }
        }
      ]
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

    fn fetcher() -> Codex {
        Codex::new()
    }

    fn context(http: Arc<FakeHttp>, account: QuotaAccount) -> QuotaFetchContext {
        QuotaFetchContext::new(Codex::ID, Dialect::Codex, http)
            .with_account(account)
            .with_credential("test-access-token")
    }

    fn payload(text: &str) -> Value {
        serde_json::from_str(text).expect("the canned payload is valid JSON")
    }

    #[test]
    fn identity_constants_cannot_drift_apart() {
        assert_eq!(CODEX_ORIGINATOR, "codex_cli_rs");
        assert_eq!(
            CODEX_USER_AGENT,
            format!("{CODEX_ORIGINATOR}/{CODEX_CLIENT_VERSION}")
        );
    }

    #[test]
    fn supports_claims_the_configured_provider_ids() {
        let codex = fetcher();
        let account = QuotaAccount::new();
        for id in ["codex", "chatgpt", "openai-codex", "chatgpt-codex", "CODEX"] {
            assert!(
                codex.supports(Dialect::Codex, id, &account),
                "expected {id} to be claimed"
            );
        }
        assert!(codex.supports(Dialect::Codex, "anything", &account));
        assert!(!codex.supports(Dialect::OpenAi, "claude", &account));
        assert!(!codex.supports(Dialect::Copilot, "github-copilot", &account));
    }

    #[tokio::test]
    async fn request_carries_the_codex_identity_and_the_workspace() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let account = QuotaAccount::new().with_extra("chatgpt_account_id", json!("acct-7"));
        let ctx = context(Arc::clone(&http), account);

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.source.as_deref(), Some(USAGE_URL));

        let requests = http.requests();
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.url, USAGE_URL);
        assert_eq!(request.method, http::Method::GET);
        assert!(request.body.is_empty());
        assert_eq!(
            header(request, "Authorization"),
            Some("Bearer test-access-token")
        );
        assert_eq!(header(request, "Content-Type"), Some("application/json"));
        assert_eq!(header(request, "Accept"), Some("application/json"));
        assert_eq!(header(request, "User-Agent"), Some(CODEX_USER_AGENT));
        assert_eq!(header(request, "originator"), Some(CODEX_ORIGINATOR));
        assert_eq!(header(request, "Version"), Some(CODEX_CLIENT_VERSION));
        assert_eq!(header(request, "chatgpt-account-id"), Some("acct-7"));
    }

    #[test]
    fn windows_are_placed_by_their_real_length() {
        let windows = collect_windows(&payload(PAID_PAYLOAD));
        let family = |key: &str| {
            windows
                .iter()
                .find(|window| window.key == key)
                .and_then(|window| window.family)
        };
        assert_eq!(family("session"), Some(WindowFamily::Session5h));
        assert_eq!(family("weekly"), Some(WindowFamily::Weekly1w));
        assert_eq!(family("GPT-5.3-Codex-Spark"), Some(WindowFamily::Session5h));
        assert_eq!(
            family("code_review"),
            Some(WindowFamily::Session5h),
            "the dedicated code-review block wins over the additional entry"
        );

        // A 30-day primary window is a monthly family, not a session.
        let camel = payload(CAMEL_PAYLOAD);
        let windows = collect_windows(&camel);
        assert_eq!(windows[0].key, "session");
        assert_eq!(windows[0].family, Some(WindowFamily::Monthly30d));
    }

    #[tokio::test]
    async fn both_windows_become_detail_rows_with_their_families() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.display_name.as_deref(), Some("plus"));
        assert_eq!(quota.used, 12.5);
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.remaining, 87.5);
        assert!(quota.fraction_reported);

        let names: Vec<&str> = quota.details.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&"session (5h)"), "{names:?}");
        assert!(names.contains(&"weekly (1w)"), "{names:?}");
        assert!(
            names.contains(&"GPT-5.3-Codex-Spark (5h)"),
            "additional_rate_limits becomes a detail row: {names:?}"
        );
        assert!(names.contains(&"code_review (5h)"), "{names:?}");
        assert!(names.contains(&"reset_credits_available"), "{names:?}");
    }

    #[tokio::test]
    async fn reset_at_seconds_becomes_epoch_milliseconds() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(
            quota.reset_at,
            Some(1_767_225_600_000),
            "1767225600 seconds is 1767225600000 milliseconds"
        );
    }

    #[test]
    fn a_reported_zero_percent_is_a_real_zero() {
        // The camelCase payload's primary window reports `usedPercent: 0` with a live reset:
        // a real "nothing used", not the unreported sentinel.
        let windows = collect_windows(&payload(CAMEL_PAYLOAD));
        assert_eq!(windows[0].used_percent, 0.0);
        assert!(!windows[0].latent, "the reset is 60s into a 30-day window");
    }

    #[tokio::test]
    async fn zero_used_percent_is_reported_not_sentinel() {
        let http = FakeHttp::serving(CAMEL_PAYLOAD);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.used, 0.0);
        assert_eq!(quota.total, 100.0);
        assert!(quota.fraction_reported, "0% here is a reported value");
        assert_eq!(quota.remaining_fraction(), Some(1.0));
    }

    #[tokio::test]
    async fn a_latent_window_is_skipped() {
        let data = payload(LATENT_PAYLOAD);
        let windows = collect_windows(&data);
        let spark = windows
            .iter()
            .find(|window| window.key == "GPT-5.3-Codex-Spark")
            .expect("the latent window is still parsed");
        assert!(spark.latent);

        let http = FakeHttp::serving(LATENT_PAYLOAD);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());
        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        let names: Vec<&str> = quota.details.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, vec!["session (5h)"], "the latent row is not shown");
        assert!(quota.fraction_reported);
        assert_eq!(quota.used, 25.0);
    }

    #[tokio::test]
    async fn camel_case_spellings_are_accepted() {
        let http = FakeHttp::serving(CAMEL_PAYLOAD);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());

        let quota = fetcher().fetch(&ctx).await.expect("the payload parses");
        assert_eq!(quota.display_name.as_deref(), Some("pro"));
        let names: Vec<&str> = quota.details.iter().map(|row| row.name.as_str()).collect();
        assert!(names.contains(&"limit_reached"), "{names:?}");
        assert!(
            names.contains(&"GPT-5.3-Codex-Spark (5h)"),
            "additionalRateLimits / limitName / usedPercent all resolve: {names:?}"
        );
    }

    #[tokio::test]
    async fn unauthorized_is_an_error() {
        let http = FakeHttp::failing(401);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
    }

    #[tokio::test]
    async fn forbidden_is_an_error() {
        let http = FakeHttp::failing(403);
        let ctx = context(Arc::clone(&http), QuotaAccount::new());
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
    }

    #[tokio::test]
    async fn an_unrecognised_payload_is_an_error_not_an_empty_quota() {
        let http = FakeHttp::serving("{}");
        let ctx = context(Arc::clone(&http), QuotaAccount::new());
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::NotFound(_))), "{result:?}");
    }

    #[tokio::test]
    async fn a_missing_credential_is_a_configuration_error() {
        let http = FakeHttp::serving(PAID_PAYLOAD);
        let ctx = QuotaFetchContext::new(Codex::ID, Dialect::Codex, http.clone());
        let result = fetcher().fetch(&ctx).await;
        assert!(matches!(result, Err(Error::Config(_))), "{result:?}");
        assert!(
            http.requests().is_empty(),
            "nothing is sent without a token"
        );
    }
}
