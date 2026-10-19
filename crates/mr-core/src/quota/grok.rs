//! xAI / Grok weekly credit pool, read through the OAuth connection.
//!
//! Ported from OmniRoute's `open-sse/services/usage/xaiOauth.ts`,
//! `open-sse/services/xaiOauthQuotaFetcher.ts` and the shared billing helper in
//! `open-sse/services/grokQuotaFetcher.ts` (MIT, © 2026 diegosouzapw).
//!
//! ```text
//! GET https://cli-chat-proxy.grok.com/v1/billing?format=credits
//! Authorization: Bearer <OAuth accessToken>
//! { "config": { "creditUsagePercent": 42.5, "currentPeriod": { "type": "WEEKLY", "end": "..." } } }
//! ```
//!
//! `creditUsagePercent` is **percent used** (0–100), matching grok.com's own Usage panel, so it
//! maps to `used` against `total = 100` — not to a remaining percentage. The window is the shared
//! weekly pool (`GROK_WINDOW_WEEKLY` upstream), hence [`WindowFamily::Weekly1w`] and the `weekly`
//! key.
//!
//! # Two deliberate limits
//!
//! * **Plain `xai` is never claimed.** With a plain API key there is no quota endpoint at all —
//!   upstream self-tracks token totals in its own store instead of asking xAI. This fetcher
//!   therefore claims only the OAuth ids `xai-oauth` and `xao`; claiming `xai` would make [the
//!   registry] send an API key to a billing endpoint that would answer 401.
//! * **No local credentials file.** The `grok-cli` / `grok-web` path reads OIDC tokens from
//!   `~/.grok/auth.json` (`GROK_AUTH_PATH` override upstream). That file read is intentionally not
//!   implemented here: a quota fetcher must not read a user's home directory, and keeping it out
//!   means no test can ever be tempted to touch it. The OAuth access token handed to
//!   [`Grok::fetch`] as the account credential is the only authentication used.
//!
//! # Reset instant
//!
//! Upstream never pins whether `currentPeriod.end` is seconds or milliseconds, and it is serialised
//! as either a number or an ISO-8601 string depending on the deployment. Rather than guessing, the
//! value is passed straight to [`UsageQuota::from_usage`], whose [`crate::quota::types::parse_reset_time`]
//! applies the documented `< 1e12 ⇒ seconds` rule to numbers and parses RFC 3339 / HTTP-date
//! strings. That is the heuristic this file relies on; a shape it cannot parse yields `reset_at =
//! None` rather than an invented instant.

use async_trait::async_trait;
use serde_json::Value;

use crate::dialect::Dialect;
use crate::error::{Error, Result};
use crate::quota::http::QuotaRequest;
use crate::quota::registry::{QuotaAccount, QuotaFetchContext, QuotaFetcher};
use crate::quota::types::{UsageQuota, WindowFamily};

/// Fetcher id, stamped onto [`UsageQuota::source`].
pub const ID: &str = "xai-oauth";

/// Provider ids this fetcher answers for. Public alias `xao` included, plain `xai` deliberately not.
pub const XAI_OAUTH_PROVIDER_IDS: [&str; 2] = ["xai-oauth", "xao"];

/// The shared weekly credit-pool endpoint.
pub const BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";

/// The upstream's key for the single window (`GROK_WINDOW_WEEKLY`).
pub const WINDOW_KEY: &str = "weekly";

/// The family a Grok pool belongs to: one week.
pub const WINDOW_FAMILY: WindowFamily = WindowFamily::Weekly1w;

/// The xAI OAuth (Grok) weekly-quota fetcher.
#[derive(Debug, Clone, Copy, Default)]
pub struct Grok;

impl Grok {
    /// A fetcher with no configuration of its own.
    pub fn new() -> Self {
        Self
    }
}

/// Coerce a JSON number or numeric string to `f64`.
fn to_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Map a billing payload to the single weekly quota.
///
/// Pure, so it is tested against captured JSON with no socket. A payload with no
/// `config.creditUsagePercent` is [`Error::NotFound`]: reporting zero-used would tell an operator a
/// quota is full when the upstream never said so.
pub fn parse_billing(body: &Value) -> Result<UsageQuota> {
    let config = body
        .get("config")
        .ok_or_else(|| Error::NotFound("grok billing response has no `config` object".into()))?;

    let percent = to_number(
        config
            .get("creditUsagePercent")
            .or_else(|| config.get("credit_usage_percent")),
    )
    .ok_or_else(|| {
        Error::NotFound(
            "grok billing response has no numeric `creditUsagePercent`; refusing to report 0 used"
                .into(),
        )
    })?;
    if !percent.is_finite() {
        return Err(Error::InvalidState(
            "grok billing response has a non-finite `creditUsagePercent`".into(),
        ));
    }

    // `creditUsagePercent` is percent used. Clamp into 0..=100 and round to a whole percent,
    // exactly as the shared billing helper does before it presents the weekly pool.
    let used = percent.clamp(0.0, 100.0).round();

    // Either form: a number in seconds or milliseconds, or an ISO-8601 string.
    let reset = config
        .get("currentPeriod")
        .and_then(|period| period.get("end"))
        .or_else(|| config.get("billingPeriodEnd"))
        .cloned()
        .unwrap_or(Value::Null);

    Ok(UsageQuota::from_usage(used, 100.0, &reset)
        .with_source(ID)
        .with_display_name("Grok weekly credits"))
}

#[async_trait]
impl QuotaFetcher for Grok {
    fn id(&self) -> &'static str {
        ID
    }

    fn supports(&self, _dialect: Dialect, provider_id: &str, _account: &QuotaAccount) -> bool {
        // Plain `xai` is self-tracked upstream: there is no billing endpoint for an API key.
        XAI_OAUTH_PROVIDER_IDS.contains(&provider_id)
    }

    async fn fetch(&self, ctx: &QuotaFetchContext) -> Result<UsageQuota> {
        let credential = ctx
            .credential
            .as_ref()
            .filter(|credential| !credential.is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "provider {:?} has no OAuth access token; the Grok billing endpoint needs one \
                     (there is no API-key quota endpoint)",
                    ctx.provider_id
                ))
            })?;

        let request = QuotaRequest::get(BILLING_URL)
            .bearer(credential.expose())
            .header("Accept", "application/json");
        let response = ctx.http.request(request).await?;
        if !response.is_success() {
            return Err(Error::InvalidState(format!(
                "grok billing endpoint returned HTTP {}",
                response.status
            )));
        }
        parse_billing(&response.json()?)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;

    use super::*;
    use crate::quota::http::{QuotaHttp, QuotaRequest, QuotaResponse};

    const BILLING: &str = r#"{
        "config": {
            "creditUsagePercent": 42.5,
            "currentPeriod": { "type": "WEEKLY", "start": "2026-01-01T00:00:00Z", "end": "2026-01-08T00:00:00Z" },
            "isUnifiedBillingUser": true
        }
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

    fn context(http: Arc<FakeHttp>, credential: Option<&str>) -> QuotaFetchContext {
        let mut ctx = QuotaFetchContext::new("xai-oauth", Dialect::OpenAi, http);
        if let Some(credential) = credential {
            ctx = ctx.with_credential(credential);
        }
        ctx
    }

    #[test]
    fn credit_usage_percent_is_percent_used() {
        let body: Value = serde_json::from_str(BILLING).expect("canned json parses");
        let quota = parse_billing(&body).expect("billing parses");
        assert_eq!(quota.used, 43.0, "42.5 rounds to 43 percent used");
        assert_eq!(quota.total, 100.0);
        assert_eq!(quota.remaining, 57.0);
        assert_eq!(quota.remaining_percentage, 57.0);
        assert!(quota.fraction_reported);
        assert_eq!(quota.source.as_deref(), Some(ID));
    }

    #[test]
    fn percent_is_clamped_into_zero_to_one_hundred() {
        let over = serde_json::json!({ "config": { "creditUsagePercent": 150 } });
        let quota = parse_billing(&over).expect("billing parses");
        assert_eq!(quota.used, 100.0);
        assert_eq!(quota.remaining, 0.0);

        let under = serde_json::json!({ "config": { "creditUsagePercent": -3 } });
        let quota = parse_billing(&under).expect("billing parses");
        assert_eq!(quota.used, 0.0);
        assert_eq!(quota.remaining, 100.0);
    }

    #[test]
    fn current_period_end_is_accepted_in_seconds_and_milliseconds() {
        let seconds = serde_json::json!({
            "config": { "creditUsagePercent": 10, "currentPeriod": { "end": 1_700_000_000_i64 } }
        });
        let quota = parse_billing(&seconds).expect("seconds parse");
        assert_eq!(quota.reset_at, Some(1_700_000_000_000));

        let millis = serde_json::json!({
            "config": { "creditUsagePercent": 10, "currentPeriod": { "end": 1_700_000_000_000_i64 } }
        });
        let quota = parse_billing(&millis).expect("millis parse");
        assert_eq!(quota.reset_at, Some(1_700_000_000_000));

        let iso = serde_json::json!({
            "config": { "creditUsagePercent": 10, "currentPeriod": { "end": "2026-01-08T00:00:00Z" } }
        });
        let quota = parse_billing(&iso).expect("iso parse");
        assert_eq!(quota.reset_at, Some(1_767_830_400_000));
    }

    #[test]
    fn missing_percent_is_an_error_not_a_full_quota() {
        let body = serde_json::json!({ "config": { "currentPeriod": { "end": 1 } } });
        let error = parse_billing(&body).expect_err("missing percent must fail");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");

        let no_config = serde_json::json!({ "unexpected": true });
        let error = parse_billing(&no_config).expect_err("missing config must fail");
        assert!(matches!(error, Error::NotFound(_)), "got {error:?}");
    }

    #[tokio::test]
    async fn fetch_sends_the_oauth_token_to_the_credits_endpoint() {
        let http = Arc::new(FakeHttp::new(vec![json_response(200, BILLING)]));
        let quota = Grok::new()
            .fetch(&context(Arc::clone(&http), Some("xai-oauth-token")))
            .await
            .expect("fetch succeeds");
        assert_eq!(quota.used, 43.0);

        let seen = http.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, http::Method::GET);
        assert_eq!(seen[0].url, BILLING_URL);
        let auth = seen[0]
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str());
        assert_eq!(auth, Some("Bearer xai-oauth-token"));
    }

    #[tokio::test]
    async fn fetch_without_a_token_and_a_non_success_status_are_errors() {
        let http = Arc::new(FakeHttp::new(vec![json_response(401, "{}")]));
        let error = Grok::new()
            .fetch(&context(Arc::clone(&http), None))
            .await
            .expect_err("no token must fail");
        assert!(matches!(error, Error::Config(_)), "got {error:?}");
        assert!(http.seen().is_empty());

        let error = Grok::new()
            .fetch(&context(Arc::clone(&http), Some("token")))
            .await
            .expect_err("401 must fail");
        assert!(matches!(error, Error::InvalidState(_)), "got {error:?}");
    }

    #[test]
    fn supports_oauth_ids_and_never_plain_xai() {
        let fetcher = Grok::new();
        assert!(fetcher.supports(Dialect::OpenAi, "xai-oauth", &QuotaAccount::new()));
        assert!(fetcher.supports(Dialect::OpenAi, "xao", &QuotaAccount::new()));
        assert!(!fetcher.supports(Dialect::OpenAi, "xai", &QuotaAccount::new()));
        assert!(!fetcher.supports(Dialect::OpenAi, "grok-web", &QuotaAccount::new()));
        assert_eq!(WINDOW_FAMILY, WindowFamily::Weekly1w);
        assert_eq!(WINDOW_FAMILY.label(), "1w");
    }
}
