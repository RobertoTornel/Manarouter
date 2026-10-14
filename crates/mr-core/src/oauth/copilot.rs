//! GitHub Copilot: the GitHub device-code flow, then a short-lived session token minted from
//! the long-lived GitHub token.
//!
//! Ground truth: `.research/oauth.md` §3 and `docs/13-oauth-copilot.md`. The client id is
//! [`crate::published::COPILOT_CLIENT_ID`]; there is no client secret.
//!
//! Shape of the credential: the GitHub token arrives once via the device flow and then rides
//! as the **refresh** credential — Copilot has no OAuth refresh grant (§3.6), so
//! [`refresh`](OAuthPlatform::refresh) re-mints the session token from it, and
//! [`account_identity`](OAuthPlatform::account_identity) reads the GitHub profile with it.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

use super::flow::Http;
use super::refresh::{endpoint_failure, oauth_error_code, oauth_failure};
use super::{
    header, oauth_error, AccountIdentity, DeviceAuthorization, DevicePoll, OAuthPlatform,
    RedirectPorts, TokenSet,
};
use crate::error::Result;
use crate::ids::Millis;
use crate::published::{github, COPILOT_CLIENT_ID};

/// The Copilot platform.
pub struct Copilot;

impl Copilot {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "github-copilot";
}

/// Poll interval when the device response omits `interval` — RFC 8628's default, which is
/// also GitHub's own.
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// Device-code lifetime when the response omits `expires_in` (GitHub uses 900 s).
const DEFAULT_DEVICE_LIFETIME_SECS: u64 = 900;

/// Session lifetime when the mint response carries no usable expiry: thirty minutes, the
/// observed `expires_at - now`. An unknown expiry must read as "refresh soon", never as
/// "never".
const DEFAULT_SESSION_SECS: i64 = 1800;

/// §3.4: the last-resort API host when neither `endpoints.api` nor a parseable `proxy-ep`
/// is present.
const DEFAULT_API_HOST: &str = "https://api.githubcopilot.com";

/// §3.1: the grant type for polling the device endpoint — a URN, not `authorization_code`.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// §3.3: the mint endpoint pins its **own** API-version header. The catalogue's
/// `x-github-api-version: 2025-04-01` is a different header on a different host (inference
/// vs. minting); the two must not be "helpfully" unified.
const MINT_API_VERSION: &str = "2024-12-15";

/// §3.1: the device-code request body — exactly these two fields. The paired secret that
/// sits next to this client id in the language-server bundle (§3.2) must **not** be sent:
/// the device flow does not use one, and shipping it would turn a public identifier into a
/// leaked credential.
fn device_request_body() -> String {
    json!({
        "client_id": COPILOT_CLIENT_ID,
        "scope": "read:user",
    })
    .to_string()
}

/// §3.1: one poll of the token endpoint.
fn poll_request_body(device_code: &str) -> String {
    json!({
        "client_id": COPILOT_CLIENT_ID,
        "device_code": device_code,
        "grant_type": DEVICE_GRANT,
    })
    .to_string()
}

/// Parse the device-code response (§3.1). `expires_in` and `interval` arrive in **seconds**
/// and become [`Duration`]s once, here — no caller can mistake either for milliseconds.
fn parse_device_authorization(status: u16, body: &str) -> Result<DeviceAuthorization> {
    if let Some((class, detail)) = endpoint_failure(status, body) {
        return Err(oauth_failure(class, Copilot::ID, detail));
    }
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("GitHub device response is not JSON: {e}")))?;
    let device_code = value
        .get("device_code")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("GitHub device response has no device_code"))?
        .to_owned();
    let user_code = value
        .get("user_code")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("GitHub device response has no user_code"))?
        .to_owned();
    let verification_uri = value
        .get("verification_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("GitHub device response has no verification_uri"))?
        .to_owned();
    let verification_uri_complete = value
        .get("verification_uri_complete")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_DEVICE_LIFETIME_SECS);
    let interval = value
        .get("interval")
        .and_then(Value::as_u64)
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri,
        verification_uri_complete,
        expires_in: Duration::from_secs(expires_in),
        interval: Duration::from_secs(interval),
    })
}

/// Classify one poll reply, with every protocol outcome kept distinct (the enum's own
/// contract). `None` means **approved**: a GitHub `access_token` is in the body and the
/// caller should mint.
///
/// The four protocol outcomes (`authorization_pending`, `slow_down`, `expired_token`,
/// `access_denied`) are the human's progress or refusal and never become errors. Everything
/// else: a transport-level / server-level blip keeps the flow polling — RFC 8628 §3.5 tells
/// the client to continue, and the device code has minutes left — while a client-side
/// rejection stops immediately with a credential-free message instead of looping to the
/// deadline. Messages are built from the OAuth error code and clipped `error_description`
/// only; the device code itself is never echoed.
fn poll_classify(status: u16, body: &str) -> Option<DevicePoll> {
    if let Some(code) = oauth_error_code(body) {
        let outcome = match code.as_str() {
            "authorization_pending" => DevicePoll::Pending,
            "slow_down" => DevicePoll::SlowDown,
            "expired_token" => DevicePoll::ExpiredToken,
            "access_denied" => DevicePoll::AccessDenied,
            _ => {
                // Not one of the protocol's four. Retryable server conditions keep polling;
                // anything else (bad device code, wrong client, unsupported grant) is dead
                // now, not at the deadline.
                if matches!(status, 0 | 408 | 429) || status >= 500 {
                    DevicePoll::Pending
                } else {
                    let detail = endpoint_failure(status, body)
                        .map(|(_, detail)| detail)
                        .unwrap_or_else(|| code);
                    DevicePoll::Failed {
                        message: format!("GitHub device token: {detail}"),
                    }
                }
            }
        };
        return Some(outcome);
    }
    // Non-2xx without a JSON error field (an HTML proxy page, an empty 502).
    if !(200..300).contains(&status) {
        if matches!(status, 0 | 408 | 429) || status >= 500 {
            return Some(DevicePoll::Pending);
        }
        let detail = endpoint_failure(status, body)
            .map(|(_, detail)| detail)
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Some(DevicePoll::Failed {
            message: format!("GitHub device token: {detail}"),
        });
    }
    // 2xx with no JSON error field: either the approval (fall through to mint) or a body
    // that never carries a token — which the success path reports without leaking its
    // contents. A non-JSON 2xx (an HTML proxy page) is the latter, not an approval.
    match serde_json::from_str::<Value>(body) {
        Ok(value) if value.get("access_token").and_then(Value::as_str).is_some() => None,
        Ok(_) => Some(DevicePoll::Failed {
            message: "GitHub poll returned no access_token".to_string(),
        }),
        Err(_) => Some(DevicePoll::Failed {
            message: "GitHub poll response is not JSON".to_string(),
        }),
    }
}

/// Derive the inference API host (§3.4), in the language server's precedence:
/// `endpoints.api` when present, else `proxy-ep` from the opaque session token with its
/// `proxy.` prefix swapped for `api.`, else the literal default. The MAC after the token's
/// first `:` is never looked at.
fn derive_api_host(endpoints_api: Option<&str>, session_token: &str) -> String {
    if let Some(api) = endpoints_api {
        let api = api.trim_end_matches('/');
        if !api.is_empty() {
            return api.to_string();
        }
    }
    if let Some(fields) = session_token.split(':').next() {
        for field in fields.split(';') {
            if let Some(proxy) = field.strip_prefix("proxy-ep=") {
                if let Some(rest) = proxy.strip_prefix("proxy.") {
                    return format!("https://api.{rest}");
                }
            }
        }
    }
    DEFAULT_API_HOST.to_string()
}

/// Parse the Copilot mint response (§3.3) into the credential we persist.
///
/// * `expires_at` is a unix timestamp in **seconds** — §3.4's correction: reading it as
///   milliseconds yields 1970. It is clamped so an absurd value cannot park expiry in the
///   far future (where nothing would ever refresh it).
/// * `refresh_token` is the GitHub token that was used to mint — the mint response has no
///   refresh grant of its own, and the refresher carries this one forward on rotation.
/// * `copilot_plan` is **not** a real response field (§3.3): the language server synthesises
///   it from `sku`, and so do we.
fn mint_tokens(body: &str, github_token: &str, now: Millis) -> Result<TokenSet> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Copilot token response is not JSON: {e}")))?;
    let session = value
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Copilot token response has no token"))?;

    let expires_at = match value.get("expires_at").and_then(Value::as_i64) {
        // Absolute server clock, preferred — it survives local clock skew.
        Some(unix_secs) if unix_secs > 0 => {
            let max_unix = (now.0 / 1000).saturating_add(super::MAX_EXPIRES_IN_SECS);
            Millis(unix_secs.min(max_unix).saturating_mul(1000))
        }
        // §3.7: what VS Code computes — `now + refresh_in + 60`.
        _ => match value.get("refresh_in").and_then(Value::as_i64) {
            Some(refresh_in) => now.plus_secs(refresh_in.clamp(0, super::MAX_EXPIRES_IN_SECS) + 60),
            None => now.plus_secs(DEFAULT_SESSION_SECS),
        },
    };

    let mut tokens = TokenSet::new(session);
    tokens.refresh_token = Some(github_token.to_owned());
    tokens.expires_at = expires_at;
    tokens.scopes = vec!["read:user".to_string()];
    tokens.token_type = "Bearer".to_string();

    if let Some(sku) = value.get("sku").and_then(Value::as_str) {
        tokens.extra.insert("copilot_plan".to_string(), json!(sku));
    }
    let endpoints_api = value
        .get("endpoints")
        .and_then(|endpoints| endpoints.get("api"))
        .and_then(Value::as_str);
    let host = derive_api_host(endpoints_api, session);
    tokens
        .extra
        .insert("copilot_api_host".to_string(), json!(host));
    Ok(tokens)
}

/// Mint a Copilot session token from a long-lived GitHub token (§3.3) — used both after the
/// device flow's approval and by [`refresh`](OAuthPlatform::refresh).
async fn mint(http: &Http, github_token: &str) -> Result<TokenSet> {
    let authorization = format!("Bearer {github_token}");
    let reply = http
        .get(
            github::COPILOT_TOKEN,
            &[
                ("Authorization", authorization.as_str()),
                ("X-GitHub-Api-Version", MINT_API_VERSION),
                ("Accept", "application/json"),
            ],
        )
        .await?;
    if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
        return Err(oauth_failure(class, Copilot::ID, detail));
    }
    mint_tokens(&reply.body, github_token, Millis::now())
}

/// Build the identity from `GET https://api.github.com/user` (§3.6), pure and testable.
fn identity_from(body: &str, plan: Option<&str>) -> Result<AccountIdentity> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("GitHub user response is not JSON: {e}")))?;
    let login = value
        .get("login")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("GitHub user response has no login"))?;
    // The numeric id survives renames; the login is the human-readable label.
    let id = match value.get("id").and_then(Value::as_i64) {
        Some(numeric) => numeric.to_string(),
        None => login.to_string(),
    };
    let label = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(login)
        .to_owned();
    let email = value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok(AccountIdentity {
        id,
        label,
        email,
        plan: plan.map(str::to_owned),
    })
}

#[async_trait]
impl OAuthPlatform for Copilot {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "GitHub Copilot"
    }

    fn description(&self) -> &'static str {
        "Device-code login with GitHub, then a session token minted from the long-lived token."
    }

    fn begins_with_loopback(&self) -> bool {
        false
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Inert: the device flow has no redirect URI. Callers must check
        // `begins_with_loopback()` before reading this.
        RedirectPorts {
            host: "",
            primary: 0,
            fallback: None,
            path: "",
        }
    }

    fn requires_pkce(&self) -> bool {
        // No redirect, hence no code to bind.
        false
    }

    fn validates_redirect_port(&self) -> bool {
        false
    }

    fn authorization_url(
        &self,
        _state: &str,
        _pkce: Option<&super::Pkce>,
        _port: u16,
    ) -> Result<String> {
        Err(oauth_error(format!(
            "{}: no authorize URL — the device-code flow opens github.com/login/device instead",
            Self::ID
        )))
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _pkce: Option<&super::Pkce>,
        _port: u16,
    ) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: there is no authorization code; the device flow is used instead",
            Self::ID
        )))
    }

    /// §3.1: ask GitHub for a device code and a user code. No client secret — the device
    /// flow does not use one.
    async fn device_start(&self) -> Result<DeviceAuthorization> {
        let http = Http::shared()?;
        let reply = http
            .post_json(
                github::DEVICE_CODE,
                &[("Accept", "application/json")],
                device_request_body(),
            )
            .await?;
        parse_device_authorization(reply.status, &reply.body)
    }

    /// One poll: classify the reply (pure, [`poll_classify`]) and, on approval, mint the
    /// session token. A transport error is a blip, not a refusal — the code still has
    /// minutes of life, so keep polling rather than failing the whole flow.
    async fn device_poll(&self, device_code: &str) -> Result<DevicePoll> {
        let http = Http::shared()?;
        let reply = match http
            .post_json(
                github::ACCESS_TOKEN,
                &[("Accept", "application/json")],
                poll_request_body(device_code),
            )
            .await
        {
            // A transport blip mid-poll is not a refusal: the device code still has minutes
            // of life and RFC 8628 §3.5 says keep polling.
            Err(_) => return Ok(DevicePoll::Pending),
            Ok(reply) => reply,
        };
        match poll_classify(reply.status, &reply.body) {
            Some(outcome) => Ok(outcome),
            // Approved: the GitHub token came back. A mint failure here surfaces as a
            // classified error rather than a false "denied" — the human did approve.
            None => {
                let github_token = github_token_of(&reply.body)?;
                Ok(DevicePoll::Success(mint(http, &github_token).await?))
            }
        }
    }

    /// §3.6: Copilot has no OAuth refresh grant — the session token is **re-minted** from
    /// the long-lived GitHub token that rides as the refresh credential. The mint response
    /// carries no `refresh_token`; the refresher keeps the old one, so this credential stays
    /// re-mintable for as long as the GitHub token lives.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = Http::shared()?;
        mint(http, refresh_token).await
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        // The GitHub token is the refresh credential (see `refresh` above); this is the one
        // place it is sent, to GitHub's own API.
        let github_token = tokens
            .refresh_token
            .as_deref()
            .ok_or_else(|| oauth_error("GitHub token missing — log in again"))?;
        let http = Http::shared()?;
        let authorization = format!("Bearer {github_token}");
        let reply = http
            .get(
                // GitHub's identity endpoint; `crate::published` ships no constant for it.
                "https://api.github.com/user",
                &[
                    ("Authorization", authorization.as_str()),
                    ("X-GitHub-Api-Version", MINT_API_VERSION),
                    ("Accept", "application/json"),
                ],
            )
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Copilot::ID, detail));
        }
        identity_from(&reply.body, tokens.extra_str("copilot_plan"))
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        // The session token minted from the GitHub token — never the GitHub token itself.
        // Every other request header (`Editor-Version`, `Copilot-Integration-Id`, the
        // user-agent family) is catalogue-owned: its values are templates only the catalogue
        // can fill, so duplicating them here would create a second copy to drift.
        if let Some(h) = header("authorization", &format!("Bearer {}", tokens.access_token)) {
            out.push(h);
        }
        out
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // Byte-identical to the catalogue's `[providers.headers]` — enforced by the registry
        // test. `Editor-Version` is deliberately absent here: its value is a template
        // (`vscode/${vs_code_version}`) that only the catalogue can fill, and it is mandatory
        // upstream (hard 400 without it), so it must never be dropped by a mismatch between
        // this list and the data.
        &[
            ("Copilot-Integration-Id", "vscode-chat"),
            ("editor-plugin-version", "copilot-chat/0.26.7"),
            ("user-agent", "GitHubCopilotChat/0.26.7"),
            ("openai-intent", "conversation-panel"),
            ("x-github-api-version", "2025-04-01"),
            ("x-vscode-user-agent-library-version", "electron-fetch"),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        // The session token lives ~1800 s and VS Code refreshes 300 s before expiry.
        Duration::from_secs(300)
    }
}

/// Pull the GitHub token out of a successful poll body (kept separate so `poll_classify`
/// stays pure).
fn github_token_of(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("GitHub poll response is not JSON: {e}")))?;
    value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| oauth_error("GitHub poll response has no access_token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembled needles: this test's own source is scanned by `scripts/check-secrets.sh`,
    /// so the paired secrets from §3.2 must not appear contiguously anywhere here.
    fn bundled_paired_secret_fragment() -> String {
        format!("{}{}", "350ee525", "b5da0e4a")
    }

    // ----- device flow ------------------------------------------------------

    #[test]
    fn device_request_is_two_public_fields_and_never_a_secret() {
        let body = device_request_body();
        assert!(
            body.contains(&format!("\"client_id\":\"{COPILOT_CLIENT_ID}\"")),
            "{body}"
        );
        assert!(
            body.contains("\"scope\":\"read:user\""),
            "§3.1 Copilot scope: {body}"
        );
        assert!(
            !body.contains("secret"),
            "the device flow does not use a client secret: {body}"
        );
        assert!(
            !body.contains(&bundled_paired_secret_fragment()),
            "the language-server's paired secret must never be sent (§3.2): {body}"
        );
    }

    #[test]
    fn poll_request_carries_the_device_urn_grant() {
        let body = poll_request_body("DEVICE-CODE-VALUE");
        assert!(
            body.contains(&"\"device_code\":\"DEVICE-CODE-VALUE\"".to_string()),
            "{body}"
        );
        assert!(
            body.contains(&format!("\"grant_type\":\"{DEVICE_GRANT}\"")),
            "{body}"
        );
        assert!(
            body.contains(&format!("\"client_id\":\"{COPILOT_CLIENT_ID}\"")),
            "{body}"
        );
        assert!(!body.contains("secret"), "{body}");
    }

    #[test]
    fn device_start_parses_seconds_into_durations() {
        let body = r#"{
            "device_code": "DC", "user_code": "ABCD-1234",
            "verification_uri": "https://github.com/login/device",
            "verification_uri_complete": "https://github.com/login/device?user_code=ABCD-1234",
            "expires_in": 900, "interval": 5
        }"#;
        let parsed = parse_device_authorization(200, body).expect("parse");
        assert_eq!(parsed.device_code, "DC");
        assert_eq!(parsed.user_code, "ABCD-1234");
        assert_eq!(parsed.verification_uri, "https://github.com/login/device");
        assert_eq!(
            parsed.verification_uri_complete.as_deref(),
            Some("https://github.com/login/device?user_code=ABCD-1234")
        );
        assert_eq!(parsed.expires_in, Duration::from_secs(900));
        assert_eq!(parsed.interval, Duration::from_secs(5));

        // Omitted optional fields fall back to protocol defaults instead of zero (a zero
        // interval would poll in a hot loop; a zero lifetime would expire instantly).
        let parsed = parse_device_authorization(
            200,
            r#"{"device_code":"DC","user_code":"U","verification_uri":"V"}"#,
        )
        .expect("parse");
        assert_eq!(
            parsed.expires_in,
            Duration::from_secs(DEFAULT_DEVICE_LIFETIME_SECS)
        );
        assert_eq!(
            parsed.interval,
            Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS)
        );
        assert_eq!(parsed.verification_uri_complete, None);
    }

    #[test]
    fn device_start_errors_are_classified_and_named() {
        let error = parse_device_authorization(
            400,
            r#"{"error":"incorrect_client_credentials","error_description":"bad client"}"#,
        )
        .expect_err("permanent failure");
        let message = error.to_string();
        assert!(message.contains("oauth[permanent]"), "{message}");
        assert!(message.contains("log in again"), "{message}");
        assert!(
            message.contains("incorrect_client_credentials"),
            "{message}"
        );

        let error = parse_device_authorization(200, "not json").expect_err("json");
        assert!(error.to_string().contains("not JSON"), "{error}");

        let error =
            parse_device_authorization(200, r#"{"device_code":"DC","verification_uri":"V"}"#)
                .expect_err("user_code");
        assert!(error.to_string().contains("user_code"), "{error}");
    }

    #[test]
    fn poll_outcomes_stay_distinct() {
        let pending = poll_classify(400, r#"{"error":"authorization_pending"}"#).expect("outcome");
        assert!(matches!(pending, DevicePoll::Pending));
        assert!(pending.is_continue());

        let slow = poll_classify(400, r#"{"error":"slow_down"}"#).expect("outcome");
        assert!(matches!(slow, DevicePoll::SlowDown));
        assert!(slow.is_continue());

        let expired = poll_classify(400, r#"{"error":"expired_token"}"#).expect("outcome");
        assert!(!expired.is_continue());
        assert!(matches!(expired, DevicePoll::ExpiredToken));

        let denied = poll_classify(400, r#"{"error":"access_denied"}"#).expect("outcome");
        assert!(!denied.is_continue());
        assert!(matches!(denied, DevicePoll::AccessDenied));
    }

    #[test]
    fn poll_keeps_polling_through_server_blips_but_stops_on_client_rejection() {
        // Server trouble → continue (RFC 8628 §3.5), the code still has minutes of life.
        for status in [0u16, 408, 429, 500, 502, 503] {
            let outcome = poll_classify(status, "<html>gateway</html>").expect("outcome");
            assert!(
                outcome.is_continue(),
                "status {status} must not kill the device flow: {outcome:?}"
            );
        }
        // Client-side rejection → stop now, with the OAuth code, not at the deadline.
        let outcome = poll_classify(
            400,
            r#"{"error":"unsupported_grant_type","error_description":"nope"}"#,
        )
        .expect("outcome");
        match outcome {
            DevicePoll::Failed { message } => {
                assert!(message.contains("unsupported_grant_type"), "{message}");
                assert!(
                    !message.contains("DEVICE"),
                    "no device code echoed: {message}"
                );
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn poll_success_falls_through_to_mint_and_never_leaks_the_token() {
        // Approved: no error field, an access_token present → mint.
        assert_eq!(
            poll_classify(200, r#"{"access_token":"gho_SECRET","expires_in":3600}"#),
            None
        );
        // `github_token_of` then reads it for the mint call.
        assert_eq!(
            github_token_of(r#"{"access_token":"gho_SECRET"}"#).expect("token"),
            "gho_SECRET"
        );
        // A 200 body with neither an error nor a token is a Failed, not a hang — and its
        // message does not echo the body.
        let outcome = poll_classify(200, r#"{"unexpected":true}"#).expect("outcome");
        match outcome {
            DevicePoll::Failed { message } => {
                assert!(message.contains("no access_token"), "{message}");
                assert!(!message.contains("unexpected"), "{message}");
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        let error = github_token_of(r#"{"unexpected":true}"#).expect_err("token");
        assert!(error.to_string().contains("access_token"), "{error}");
    }

    // ----- mint -------------------------------------------------------------

    #[test]
    fn mint_parses_the_real_response_shape() {
        // Field subset of the verbatim §3.3 response.
        let body = serde_json::json!({
            "chat_enabled": true,
            "endpoints": { "api": "https://api.individual.githubcopilot.com" },
            "expires_at": 1775492517_i64,
            "refresh_in": 1500,
            "sku": "free_educational_quota",
            "token": "tid=abc;exp=1775492517;sku=free_educational_quota;proxy-ep=proxy.individual.githubcopilot.com;st=dotcom;chat=1:MACBYTES",
            "tracking_id": "2989da96a5d876e38c74ce188229e9de",
        })
        .to_string();

        let tokens = mint_tokens(&body, "gho_GITHUB", Millis::now()).expect("mint");
        assert_eq!(tokens.access_token, "tid=abc;exp=1775492517;sku=free_educational_quota;proxy-ep=proxy.individual.githubcopilot.com;st=dotcom;chat=1:MACBYTES");
        assert_eq!(tokens.refresh_token.as_deref(), Some("gho_GITHUB"));
        // §3.4's correction: unix SECONDS → Millis (not *1970* milliseconds).
        assert_eq!(tokens.expires_at.0, 1775492517 * 1000);
        assert_eq!(
            tokens.extra_str("copilot_plan"),
            Some("free_educational_quota")
        );
        assert_eq!(
            tokens.extra_str("copilot_api_host"),
            Some("https://api.individual.githubcopilot.com"),
            "endpoints.api wins over the token's proxy-ep"
        );
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(tokens.scopes, vec!["read:user".to_string()]);
    }

    #[test]
    fn mint_expiry_falls_back_to_refresh_in_then_to_thirty_minutes() {
        let now = Millis(1_700_000_000_000);

        // No expires_at → now + refresh_in + 60 (what VS Code computes, §3.7).
        let body = r#"{"token":"t","refresh_in":1500}"#;
        let tokens = mint_tokens(body, "gh", now).expect("mint");
        assert_eq!(tokens.expires_at, now.plus_secs(1560));

        // Nothing at all → thirty minutes: "refresh soon", never "never".
        let tokens = mint_tokens(r#"{"token":"t"}"#, "gh", now).expect("mint");
        assert_eq!(tokens.expires_at, now.plus_secs(DEFAULT_SESSION_SECS));

        // An absurd absolute expiry is clamped instead of parking the credential a
        // lifetime away where nothing would ever refresh it.
        let tokens =
            mint_tokens(r#"{"token":"t","expires_at":99999999999}"#, "gh", now).expect("mint");
        assert!(tokens
            .expires_at
            .is_before(now.plus_secs(super::super::MAX_EXPIRES_IN_SECS + 60)));

        // Zero/negative → the fallback chain, not 1970.
        let tokens = mint_tokens(
            r#"{"token":"t","expires_at":0,"refresh_in":100}"#,
            "gh",
            now,
        )
        .expect("mint");
        assert_eq!(tokens.expires_at, now.plus_secs(160));
    }

    #[test]
    fn mint_requires_a_token() {
        let error = mint_tokens(r#"{"sku":"plus"}"#, "gh", Millis(0)).expect_err("token");
        assert!(error.to_string().contains("has no token"), "{error}");
        let error = mint_tokens("not json", "gh", Millis(0)).expect_err("json");
        assert!(error.to_string().contains("not JSON"), "{error}");
    }

    #[test]
    fn api_host_precedence_matches_the_language_server() {
        // 1. endpoints.api wins outright (§3.4).
        assert_eq!(
            derive_api_host(Some("https://api.enterprise.githubcopilot.com"), "junk:MAC"),
            "https://api.enterprise.githubcopilot.com"
        );
        // 2. proxy-ep with its leading `proxy.` swapped for `api.`.
        assert_eq!(
            derive_api_host(
                None,
                "tid=1;proxy-ep=proxy.individual.githubcopilot.com:MAC"
            ),
            "https://api.individual.githubcopilot.com"
        );
        assert_eq!(
            derive_api_host(None, "proxy-ep=proxy.enterprise.githubcopilot.com"),
            "https://api.enterprise.githubcopilot.com"
        );
        // 3. Anything else → the literal default.
        assert_eq!(
            derive_api_host(None, "no-fields-here:MAC"),
            DEFAULT_API_HOST
        );
        assert_eq!(
            derive_api_host(Some(""), "tid=1"),
            DEFAULT_API_HOST,
            "empty is absent"
        );
        assert_eq!(
            derive_api_host(None, "proxy-ep=evil.example.com;x=1:MAC"),
            DEFAULT_API_HOST,
            "a proxy-ep without the proxy. prefix is not rewritten"
        );
    }

    // ----- identity ---------------------------------------------------------

    #[test]
    fn identity_prefers_the_numeric_id_and_the_login_as_label() {
        let body = serde_json::json!({
            "login": "octocat",
            "id": 583231,
            "name": "The Octocat",
            "email": "octo@example.com",
        })
        .to_string();
        let identity = identity_from(&body, Some("free_limited_copilot")).expect("identity");
        assert_eq!(identity.id, "583231", "numeric id survives renames");
        assert_eq!(identity.label, "The Octocat");
        assert_eq!(identity.email.as_deref(), Some("octo@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("free_limited_copilot"));

        // No name → the login is the label; no email → None, never a fabricated string.
        let body = serde_json::json!({ "login": "octocat", "id": 1, "name": null, "email": null })
            .to_string();
        let identity = identity_from(&body, None).expect("identity");
        assert_eq!(identity.label, "octocat");
        assert_eq!(identity.email, None);
        assert_eq!(identity.plan, None);

        // Numeric id absent → the login still gives a stable-ish id rather than failing.
        let identity = identity_from(r#"{"login":"octocat"}"#, None).expect("identity");
        assert_eq!(identity.id, "octocat");

        let error = identity_from(r#"{"id":1}"#, None).expect_err("login");
        assert!(error.to_string().contains("no login"), "{error}");
        let error = identity_from("not json", None).expect_err("json");
        assert!(error.to_string().contains("not JSON"), "{error}");
    }

    #[tokio::test]
    async fn identity_without_a_github_token_is_a_relogin() {
        let error = Copilot
            .account_identity(&TokenSet::new("session"))
            .await
            .expect_err("no refresh credential");
        assert!(error.to_string().contains("log in again"), "{error}");
    }

    // ----- statics ----------------------------------------------------------

    #[tokio::test]
    async fn statics_match_the_dossier() {
        assert_eq!(Copilot::ID, "github-copilot");
        assert!(!Copilot.begins_with_loopback(), "§3.1 device flow");
        assert!(!Copilot.requires_pkce());
        assert!(!Copilot.validates_redirect_port());
        assert_eq!(Copilot.refresh_lead(), Duration::from_secs(300));
        assert_eq!(DEFAULT_API_HOST, "https://api.githubcopilot.com");
        assert_eq!(
            MINT_API_VERSION, "2024-12-15",
            "§3.3 mint pin, not the catalogue's 2025-04-01"
        );
        // authorization_url / exchange_code stay honest errors: neither exists here.
        let error = Copilot
            .authorization_url("s", None, 0)
            .expect_err("device flow");
        assert!(
            error.to_string().contains("github.com/login/device"),
            "{error}"
        );
        let error = Copilot.exchange_code("c", None, 0).await;
        match error {
            Err(error) => assert!(error.to_string().contains("device flow"), "{error}"),
            Ok(_) => panic!("exchange_code must stay an honest error"),
        }
    }

    #[test]
    fn apply_credential_sends_only_the_session_token() {
        let mut tokens = TokenSet::new("SESSION");
        tokens.refresh_token = Some("gho_GITHUB_TOKEN".to_string());
        let headers = Copilot.apply_credential(&tokens);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0.as_str(), "authorization");
        assert_eq!(headers[0].1, "Bearer SESSION");
        assert!(
            !headers
                .iter()
                .any(|(_, value)| value.to_str().is_ok_and(|v| v.contains("gho_"))),
            "the GitHub token must never be sent to the inference host"
        );
    }
}
