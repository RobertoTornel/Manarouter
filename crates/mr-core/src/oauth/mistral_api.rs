//! Mistral Vibe API: the Vibe CLI browser sign-in, in the connector's own terms.
//!
//! api.mistral.ai does not accept Mistral's Ory access or ID tokens: the earlier
//! loopback-PKCE implementation always minted a login that the API rejected with
//! "Invalid API Key". The installed Vibe CLI signs in differently. Its
//! BrowserSignInService generates an S256 PKCE verifier, creates a browser sign-in
//! process at https://console.mistral.ai/api/vibe/sign-in, lets the human sign in
//! on Mistral's own page, polls that process, and exchanges a single-use
//! exchange_token plus the verifier for a real {"api_key": "..."} payload. The
//! resulting API key is then used as the ordinary Mistral API credential. There is
//! no Ory token to send and no refresh-token grant: a stored extracted API key is
//! presented directly as a bearer credential, and a revoked key simply needs a new
//! browser sign-in.
//!
//! The sign-in URLs are validated against the CLI's published browser/API base
//! URLs, following its gateway. The private API key is never logged.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

use super::flow::{self, ImpersonatingHttp, OAuthHttp};
use super::refresh::{endpoint_failure, oauth_failure};
use super::{
    header, oauth_error, AccountIdentity, DevicePoll, OAuthPlatform, Pkce, RedirectPorts,
    ScreenLogin, TokenSet,
};
use crate::error::Result;
use crate::ids::Millis;
use crate::published::mistral;

/// The Mistral Vibe API OAuth platform: browser sign-in plus API-key exchange.
pub struct MistralApi;

impl MistralApi {
    /// Registry id, matching oauth_platform on the mistral-vibe-api catalogue entry.
    pub const ID: &'static str = "mistral-vibe-api";

}

const CREATE_PROCESS_PATH: &str = "/vibe/sign-in";

/// Path of Mistral's whoami endpoint relative to the console API base. The CLI joins
/// console root plus "/api/vibe/whoami" — one /api, not two: base "/api" plus this path.
const WHOAMI_PATH: &str = "/vibe/whoami";

fn parse_json_object(body: &str, context: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Mistral {context} response is not JSON: {e}")))?;
    if !value.is_object() {
        return Err(oauth_error(format!("Mistral {context} response is not an object")));
    }
    Ok(value)
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).map(str::trim)
}

/// Validate a URL returned by the sign-in start reply against the origin the CLI
/// itself trusts. A malicious or misconfigured console answer must not be able to
/// point the panel at an arbitrary page or exfiltrate the verifier.
fn validated_url(raw: &str, base: &str, context: &str) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(oauth_error(format!("Mistral browser sign-in returned an invalid {context} URL")));
    }
    if !value.starts_with(base) {
        return Err(oauth_error(format!("Mistral browser sign-in returned a {context} URL outside the expected origin")));
    }
    Ok(value.to_owned())
}

/// Convert the exchange reply's {"api_key": "..."} payload into a stored credential.
/// A platform API key is not an expiring OAuth token: the expiry stays at the
/// zero sentinel (unknown), so the refresher never tries to renew it, and there is
/// no refresh token. The key rides both in access_token and, mirrored, in
/// extra["api_key"] for tooling that inspects the credential shape.
fn api_key_token_set(api_key: &str) -> Result<TokenSet> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(oauth_error("Mistral browser sign-in exchange returned no API key"));
    }
    let mut tokens = TokenSet::new(api_key);
    // The extracted key does not expire: pin expiry far in the future so the refresher
    // classifies it as fresh (a zero sentinel reads as always-due and would fail every
    // request with no refresh token to renew it).
    tokens.expires_at = Millis(i64::MAX / 4);
    tokens.token_type = "Bearer".to_owned();
    tokens.extra.insert("api_key".into(), Value::String(api_key.into()));
    Ok(tokens)
}

fn api_key_from_exchange_body(body: &str) -> Result<String> {
    let value = parse_json_object(body, "exchange")?;
    let api_key = value
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .ok_or_else(|| oauth_error("Mistral browser sign-in exchange returned no API key"))?;
    Ok(api_key.to_owned())
}

/// The CLI maps a 403 with error_code 9004 to a workspace-eligibility message.
/// Mirror that so the panel says something the operator can act on.
fn exchange_failure_message(status: u16, body: &str) -> String {
    if status == 403 {
        if let Ok(value) = serde_json::from_str::<Value>(body) {
            if value.get("error_code").and_then(Value::as_i64) == Some(9_004) {
                return "Your current workspace is not eligible for Mistral Vibe. Sign in again and select another workspace in the browser, or contact your administrator.".to_owned();
            }
        }
    }
    "Mistral browser sign-in exchange failed.".to_owned()
}

/// Map Mistral's whoami reply to our account identity. The endpoint is plan and
/// tenant data, not OIDC userinfo: the customer id is the stable account key and
/// the plan name is the label. The email is not exposed by this endpoint.
fn identity_from_whoami(body: &str) -> Result<AccountIdentity> {
    let value = parse_json_object(body, "whoami")?;
    let id = string_field(&value, "customer_id")
        .or_else(|| string_field(&value, "organization_id"))
        .or_else(|| string_field(&value, "user_id"))
        .or_else(|| string_field(&value, "id"))
        .ok_or_else(|| oauth_error("Mistral whoami response contains no account identifier"))?
        .to_owned();
    let plan = string_field(&value, "plan_name")
        .or_else(|| string_field(&value, "plan_type"))
        .unwrap_or("Mistral Vibe API")
        .to_owned();
    Ok(AccountIdentity {
        id,
        label: plan.clone(),
        email: None,
        plan: Some(plan),
    })
}

async fn create_browser_sign_in(pkce: &Pkce) -> Result<(String, String, String)> {
    let http = ImpersonatingHttp::new()?;
    let body = json!({
        "code_challenge": pkce.challenge(),
        "code_challenge_method": "S256",
    });
    let reply = http
        .post_json(
            &format!("{}{CREATE_PROCESS_PATH}", mistral::BROWSER_AUTH_API_BASE_URL),
            &[],
            body.to_string(),
        )
        .await
        .map_err(|e| oauth_error(format!("Mistral browser sign-in start request failed: {e}")))?;
    if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
        return Err(oauth_failure(failure.0, "mistral-vibe-api", failure.1));
    }
    let value = parse_json_object(&reply.body, "browser sign-in start")?;
    let process_id = string_field(&value, "process_id")
        .ok_or_else(|| oauth_error("Mistral browser sign-in start returned no process id"))?
        .to_owned();
    let sign_in_url = validated_url(
        string_field(&value, "sign_in_url")
            .ok_or_else(|| oauth_error("Mistral browser sign-in start returned no sign-in URL"))?,
        mistral::BROWSER_AUTH_BASE_URL,
        "sign-in",
    )?;
    let poll_url = validated_url(
        string_field(&value, "poll_url")
            .ok_or_else(|| oauth_error("Mistral browser sign-in start returned no poll URL"))?,
        mistral::BROWSER_AUTH_API_BASE_URL,
        "poll",
    )?;
    Ok((process_id, sign_in_url, poll_url))
}

async fn exchange_sign_in(
    process_id: &str,
    exchange_token: &str,
    code_verifier: &str,
) -> Result<TokenSet> {
    let http = ImpersonatingHttp::new()?;
    let body = json!({
        "exchange_token": exchange_token,
        "code_verifier": code_verifier,
    });
    let reply = http
        .post_json(
            &format!("{}/vibe/sign-in/{}/exchange", mistral::BROWSER_AUTH_API_BASE_URL, flow::url_encode(process_id)),
            &[],
            body.to_string(),
        )
        .await
        .map_err(|e| oauth_error(format!("Mistral browser sign-in exchange request failed: {e}")))?;
    if reply.status >= 400 {
        return Err(oauth_failure(
            crate::oauth::refresh::FailureClass::Permanent,
            "mistral-vibe-api",
            exchange_failure_message(reply.status, &reply.body),
        ));
    }
    let api_key = api_key_from_exchange_body(&reply.body)?;
    api_key_token_set(&api_key)
}

#[async_trait]
impl OAuthPlatform for MistralApi {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Mistral Vibe API"
    }

    fn description(&self) -> &'static str {
        "Mistral Vibe CLI browser sign-in, exchanging the completed sign-in for the platform API key."
    }

    fn begins_with_loopback(&self) -> bool {
        false
    }

    fn begins_with_screen_login(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Screen login never binds a local listener. An inert zero port keeps the
        // trait requirement without advertising a callback.
        RedirectPorts {
            host: "127.0.0.1",
            primary: 0,
            fallback: None,
            path: "/",
        }
    }

    fn requires_pkce(&self) -> bool {
        false
    }

    fn validates_redirect_port(&self) -> bool {
        false
    }

    fn authorization_url(&self, _state: &str, _pkce: Option<&Pkce>, _port: u16) -> Result<String> {
        Err(oauth_error(            "Mistral Vibe API signs in on Mistral's own page; there is no loopback redirect",
        ))
    }

    async fn exchange_code(&self, _code: &str, _pkce: Option<&Pkce>, _port: u16) -> Result<TokenSet> {
        Err(oauth_error(            "Mistral Vibe API signs in on Mistral's own page; there is no authorization code",
        ))
    }
    async fn screen_login_start(&self) -> Result<ScreenLogin> {
        let pkce = Pkce::generate();
        let (process_id, sign_in_url, poll_url) = create_browser_sign_in(&pkce).await?;
        // The registry stores only the halves a screen login really needs; the poll
        // endpoint is server-minted, so it rides inside the private ticket half,
        // split from the verifier by a control separator.
        let ticket = format!("{}\u{1E}{poll_url}", pkce.verifier());
        Ok(ScreenLogin::new(sign_in_url, process_id, ticket))
    }

    async fn screen_login_poll(&self, login: &ScreenLogin) -> Result<DevicePoll> {
        let (verifier, poll_url) = login
            .ticket()
            .split_once('\u{1E}')
            .ok_or_else(|| oauth_error("Mistral browser sign-in ticket is missing its poll endpoint"))?;
        let http = ImpersonatingHttp::new()?;
        let reply = http
            .get(poll_url, &[("Accept", "application/json")])
            .await
            .map_err(|e| oauth_error(format!("Mistral browser sign-in poll request failed: {e}")))?;

        if reply.status == 410 {
            return Ok(DevicePoll::ExpiredToken);
        }
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, "mistral-vibe-api", failure.1));
        }

        let value = parse_json_object(&reply.body, "browser sign-in poll")?;
        match string_field(&value, "status") {
            Some("pending") => Ok(DevicePoll::Pending),
            Some("completed") => {
                let exchange_token = string_field(&value, "exchange_token")
                    .filter(|token| !token.is_empty())
                    .ok_or_else(|| {
                        oauth_error("Mistral browser sign-in completed without an exchange token")
                    })?
                    .to_owned();
                let tokens = exchange_sign_in(login.session(), &exchange_token, verifier).await?;
                Ok(DevicePoll::Success(tokens))
            }
            Some("expired") => Ok(DevicePoll::ExpiredToken),
            Some("denied") => Ok(DevicePoll::AccessDenied),
            Some("error") => Err(oauth_error(
                string_field(&value, "message")
                    .filter(|message| !message.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| "Mistral browser sign-in failed.".to_owned()),
            )),
            _ => Err(oauth_error("Mistral browser sign-in returned an unknown state.")),
        }
    }    async fn refresh(&self, _refresh_token: &str) -> Result<TokenSet> {
        Err(oauth_failure(
            crate::oauth::refresh::FailureClass::Permanent,
            "mistral-vibe-api",
            "Mistral Vibe API credentials are an extracted platform API key and cannot be refreshed; sign in again if the key is revoked",
        ))
    }

    fn refresh_lead(&self) -> Duration {
        Duration::from_secs(3600)
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        let http = ImpersonatingHttp::new()?;
        let reply = http
            .get(
                &format!("{}{WHOAMI_PATH}", mistral::BROWSER_AUTH_API_BASE_URL),
                &[("authorization", &format!("Bearer {}", tokens.access_token))],
            )
            .await
            .map_err(|e| oauth_error(format!("Mistral whoami request failed: {e}")))?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, "mistral-vibe-api", failure.1));
        }
        identity_from_whoami(&reply.body)
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        vec![
            header(
                "authorization",
                &format!("Bearer {}", tokens.access_token),
            )
            .expect("static header name"),
        ]
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // The exact identity `vibe.utils.http.get_user_agent("mistral")` sends for Mistral
        // completions in the official package.
        &[("user-agent", mistral::USER_AGENT)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_fingerprint_is_the_official_vibe_user_agent() {
        // The platform upstream gates on the identity `vibe.utils.http.get_user_agent("mistral")`
        // builds: `mistral-client-python/` + `Mistral-Vibe/<version>` (3.1.4 captured). Drifting
        // this value turns every account into a client Mistral has never seen.
        assert_eq!(
            MistralApi.fingerprint_headers(),
            &[("user-agent", mistral::USER_AGENT)]
        );
        assert_eq!(mistral::USER_AGENT, "mistral-client-python/Mistral-Vibe/3.1.4");
    }

    #[test]
    fn an_api_key_maps_to_a_bearer_token_set_without_an_expiry_or_refresh_token() {
        let tokens = api_key_token_set("mistral-api-key").unwrap();
        assert_eq!(tokens.access_token, "mistral-api-key");
        assert!(tokens.refresh_token.is_none());
        // No expiry: far-future pin so the refresher never treats the key as due.
        assert!(tokens.expires_at.as_millis() > 1_000_000_000_000);
        assert_eq!(
            tokens.extra.get("api_key").and_then(Value::as_str),
            Some("mistral-api-key"),
        );
        let headers = MistralApi.apply_credential(&tokens);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].1.to_str().unwrap(), "Bearer mistral-api-key");
    }

    #[test]
    fn an_exchange_response_extracts_the_api_key() {
        let body = r#"{"api_key":"mistral-api-key"}"#;
        assert_eq!(api_key_from_exchange_body(body).unwrap(), "mistral-api-key");
    }

    #[test]
    fn an_empty_exchange_response_is_an_error() {
        assert!(api_key_from_exchange_body(r#"{"api_key":""}"#).is_err());
        assert!(api_key_from_exchange_body(r#"{}"#).is_err());
    }

    #[test]
    fn a_workspace_not_eligible_failure_is_surfaced_in_plain_language() {
        let body = r#"{"error_code":9004}"#;
        assert!(exchange_failure_message(403, body).contains("workspace"));
    }

    #[test]
    fn whoami_fields_map_to_account_identity() {
        let body = json!({
            "customer_id": "cust_123",
            "plan_name": "Pro",
            "organization_id": "org_123",
            "api_base": "https://api.mistral.ai/v1",
        })
        .to_string();
        let identity = identity_from_whoami(&body).unwrap();
        assert_eq!(identity.id, "cust_123");
        assert_eq!(identity.label, "Pro");
        assert_eq!(identity.plan.as_deref(), Some("Pro"));
        assert!(identity.email.is_none());
    }

    #[test]
    fn a_sign_in_url_outside_the_expected_origin_is_rejected() {
        assert!(validated_url("https://evil.example/x", "https://console.mistral.ai", "sign-in").is_err());
        assert!(validated_url("https://console.mistral.ai/sign-in", "https://console.mistral.ai", "sign-in").is_ok());
    }
}
