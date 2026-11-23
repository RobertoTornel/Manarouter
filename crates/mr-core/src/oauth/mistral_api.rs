//! Mistral Vibe API: the CLI's own login, mimicked — loopback PKCE against Mistral's Ory at
//! auth.mistral.ai.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

use super::flow::{self, ImpersonatingHttp, OAuthHttp};
use super::refresh::{endpoint_failure, oauth_failure};
use super::{
    expires_at_from_secs, header, oauth_error, AccountIdentity, OAuthPlatform, Pkce, RedirectPorts,
    TokenSet,
};
use crate::error::Result;
use crate::published::mistral;

/// The Mistral Vibe API OAuth platform (the CLI login, mimicked).
pub struct MistralApi;

impl MistralApi {
    /// Registry id, matching `oauth_platform` on the `mistral-vibe-api` catalogue entry.
    pub const ID: &'static str = "mistral-vibe-api";
}

/// Fallback access-token lifetime when the response omits `expires_in`, in seconds.
const DEFAULT_TTL_SECS: i64 = 3600;

/// Refresh one hour before expiry: the session token is short-lived and cheap to renew.
const REFRESH_LEAD_SECS: u64 = 60 * 60;

/// Build the authorization URL for a loopback listener on `port`.
fn authorization_url(state: &str, pkce: Option<&Pkce>, port: u16) -> String {
    let challenge = pkce.map(Pkce::challenge).unwrap_or_default();
    let redirect_uri = format!(
        "http://127.0.0.1:{port}{}",
        mistral::LOOPBACK_CALLBACK_PATH
    );
    format!(
        "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&audience={}&code_challenge={}&code_challenge_method=S256&state={}",
        mistral::AUTHORIZE,
        flow::url_encode(mistral::CLIENT_ID),
        flow::url_encode(&redirect_uri),
        flow::url_encode(mistral::SCOPES),
        flow::url_encode(mistral::AUDIENCE),
        flow::url_encode(challenge),
        flow::url_encode(state),
    )
}

/// Parse a token response into a `TokenSet`. `previous_refresh` survives a response that omits
/// a replacement. Every non-standard field the endpoint returns (notably the JWT `id_token`) is
/// preserved in `TokenSet::extra` so later steps can fall back to it without another round trip.
fn token_set_from(body: &str, previous_refresh: Option<&str>) -> Result<TokenSet> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Mistral token response is not JSON: {e}")))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Mistral token response has no access_token"))?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_TTL_SECS);
    let mut tokens = TokenSet::new(access_token);
    tokens.refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned));
    tokens.scopes = value
        .get("scope")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    if let Some(token_type) = value.get("token_type").and_then(Value::as_str) {
        tokens.token_type = token_type.to_owned();
    }
    if let Some(extra) = value.as_object() {
        const KNOWN: &[&str] = &[
            "access_token",
            "refresh_token",
            "expires_in",
            "scope",
            "token_type",
        ];
        for (key, val) in extra {
            if !KNOWN.contains(&key.as_str()) {
                tokens.extra.insert(key.clone(), val.clone());
            }
        }
    }
    tokens.expires_at = expires_at_from_secs(expires_in);
    Ok(tokens)
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
        "The Mistral CLI's login (Ory PKCE at auth.mistral.ai), minting a vibe token for api.mistral.ai."
    }

    fn begins_with_loopback(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // The app binds an ephemeral loopback port and registers the `/auth/oidc/callback` path;
        // the redirect URI must match that path exactly. `validates_redirect_port` is false
        // because the port is chosen at runtime, as in the installed app.
        RedirectPorts {
            host: "127.0.0.1",
            primary: 0,
            fallback: None,
            path: mistral::LOOPBACK_CALLBACK_PATH,
        }
    }

    fn requires_pkce(&self) -> bool {
        true
    }

    fn validates_redirect_port(&self) -> bool {
        false
    }

    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String> {
        Ok(authorization_url(state, pkce, port))
    }

    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet> {
        let http = ImpersonatingHttp::new()?;
        let redirect_uri = format!(
            "http://127.0.0.1:{port}{}",
            mistral::LOOPBACK_CALLBACK_PATH
        );
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("client_id", mistral::CLIENT_ID),
        ];
        if let Some(pkce) = pkce {
            pairs.push(("code_verifier", pkce.verifier()));
        }
        let reply = http
            .post_form(mistral::TOKEN, &[], &pairs)
            .await
            .map_err(|e| oauth_error(format!("Mistral token request failed: {e}")))?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, "mistral-vibe-api", failure.1));
        }
        token_set_from(&reply.body, None)
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = ImpersonatingHttp::new()?;
        let pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", mistral::CLIENT_ID),
        ];
        let reply = http
            .post_form(mistral::TOKEN, &[], &pairs)
            .await
            .map_err(|e| oauth_error(format!("Mistral refresh request failed: {e}")))?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, "mistral-vibe-api", failure.1));
        }
        token_set_from(&reply.body, Some(refresh_token))
    }

    fn refresh_lead(&self) -> Duration {
        Duration::from_secs(REFRESH_LEAD_SECS)
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        let http = ImpersonatingHttp::new()?;
        let reply = http
            .get(
                mistral::USERINFO,
                &[("authorization", &format!("Bearer {}", tokens.access_token))],
            )
            .await
            .map_err(|e| oauth_error(format!("Mistral userinfo request failed: {e}")))?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, "mistral-vibe-api", failure.1));
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|e| oauth_error(format!("Mistral userinfo is not JSON: {e}")))?;
        let id = value
            .get("sub")
            .and_then(Value::as_str)
            .ok_or_else(|| oauth_error("Mistral userinfo has no `sub`"))?
            .to_owned();
        let email = value
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let label = value
            .get("name")
            .or_else(|| value.get("preferred_username"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| id.clone());
        Ok(AccountIdentity {
            id,
            label,
            email,
            plan: None,
        })
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        // The access token is issued for the `audience=https://api.mistral.ai` the authorize
        // request carries — the standard OAuth2 bearer for the platform API resource server.
        // The JWT id_token is preserved in `extra` as the documented alternative: if the first
        // live login shows the API prefers the JWT (as the old consumer-surface work measured for
        // the LeChat backend), send `tokens.extra_str("id_token")` here instead.
        vec![
            header(
                "authorization",
                &format!("Bearer {}", tokens.access_token),
            )
            .expect("static header name"),
        ]
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        &[("user-agent", "vibe-desktop/1.2.0")]
    }

    async fn refresh_with_context(
        &self,
        refresh_token: &str,
        previous: &TokenSet,
    ) -> Result<TokenSet> {
        // Refresh returns no new account metadata on this platform; preserve the previous one.
        let mut tokens = self.refresh(refresh_token).await?;
        let prev = json!({
            "id": previous.extra.get("id").cloned().unwrap_or(Value::Null),
            "label": previous.extra.get("label").cloned().unwrap_or(Value::Null),
            "email": previous.extra.get("email").cloned().unwrap_or(Value::Null),
        });
        tokens.extra.insert("account".into(), prev);
        // A refresh grant usually mints a fresh id token, but Ory is not obliged to: when the
        // response omits one, the previous id token is carried forward rather than lost.
        if !tokens.extra.contains_key("id_token") {
            if let Some(previous_id) = previous.extra.get("id_token") {
                tokens.extra.insert("id_token".into(), previous_id.clone());
            }
        }
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The authorize URL must match the registered client: the `/auth/oidc/callback` path, the
    /// `audience` parameter and the scope order are what Ory compares.
    #[test]
    fn the_authorize_url_matches_the_cli_registration() {
        let pkce = Pkce::generate();
        let url = authorization_url("state-value", Some(&pkce), 45_921);
        assert!(url.starts_with("https://auth.mistral.ai/oauth2/auth?"));
        assert!(url.contains("audience=https%3A%2F%2Fapi.mistral.ai"));
        assert!(url.contains(&format!(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A45921{}",
            flow::url_encode(mistral::LOOPBACK_CALLBACK_PATH)
        )));
        assert!(url.contains("scope=openid%20email%20profile%20offline_access"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-value"));
    }

    /// The Ory response carries an opaque `access_token` and a JWT `id_token`. The authorize
    /// request is audience-scoped to `api.mistral.ai`, so the access token is the bearer; the
    /// JWT must nevertheless survive the parse in `extra` for the documented fallback.
    #[test]
    fn the_token_set_keeps_the_id_token_and_the_credential_sends_the_access_token() {
        let body = r#"{"access_token":"ory_at_opaque","id_token":"eyJhbGciOiJSUz","refresh_token":"ory_rt_x","expires_in":3599,"scope":"openid email profile offline_access","token_type":"bearer"}"#;
        let tokens = token_set_from(body, None).unwrap();
        assert_eq!(tokens.access_token, "ory_at_opaque");
        assert_eq!(
            tokens.extra.get("id_token").and_then(|v| v.as_str()),
            Some("eyJhbGciOiJSUz")
        );
        let headers = MistralApi.apply_credential(&tokens);
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers[0].1.to_str().unwrap(),
            "Bearer ory_at_opaque",
            "the API bearer is the audience-scoped access token"
        );
    }

    /// Without an id token (a bare Ory exchange), nothing extra is invented.
    #[test]
    fn a_bare_token_response_has_no_id_token() {
        let body = r#"{"access_token":"ory_at_opaque","refresh_token":"ory_rt_x","expires_in":3599,"token_type":"bearer"}"#;
        let tokens = token_set_from(body, None).unwrap();
        assert!(tokens.extra.get("id_token").is_none());
        let headers = MistralApi.apply_credential(&tokens);
        assert_eq!(headers[0].1.to_str().unwrap(), "Bearer ory_at_opaque");
    }

    /// A refresh that omits a replacement refresh token keeps the previous one.
    #[test]
    fn a_missing_replacement_refresh_token_keeps_the_previous_one() {
        let body = r#"{"access_token":"ory_at_new","expires_in":3599}"#;
        let tokens = token_set_from(body, Some("ory_rt_old")).unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("ory_rt_old"));
    }
}
