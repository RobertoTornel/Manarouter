//! Mistral Vibe: the desktop client's OAuth (Ory Hydra at auth.mistral.ai).
//!
//! ## Where the flow comes from
//!
//! The Vibe desktop app (v1.2.0, NSIS installer) talks to `https://chat.mistral.ai` and
//! `wss://chat-ws.mistral.ai`, and logs in through an Ory deployment: the authorization URL,
//! PKCE (S256), and a token endpoint on `https://auth.mistral.ai`. The API the router wants is
//! the ordinary OpenAI-compatible one at `https://api.mistral.ai`. This module implements the
//! loopback PKCE login with the endpoints and client id extracted from the desktop app's
//! bytecode bundle.
//!
//! `// UNCERTAIN:` the exact Ory paths (`/oauth2/auth`, `/oauth2/token`, `/userinfo`) are the
//! defaults Ory serves — they are not separately verified against a live Mistral login, and the
//! client id is the one embedded in the desktop app, which may be registered per-app. If a login
//! fails at the authorization stage, the panel shows the URL; the operator can compare it with
//! what the desktop app opens and we adjust the constants.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

use super::flow::{self, ImpersonatingHttp, OAuthHttp};
use super::refresh::{oauth_failure, FailureClass};
use super::{
    expires_at_from_secs, header, oauth_error, AccountIdentity, OAuthPlatform, Pkce, RedirectPorts,
    TokenSet,
};
use crate::error::Result;
use crate::published::mistral_vibe;

/// Mistral Vibe's OAuth platform.
pub struct MistralVibe;

impl MistralVibe {
    /// Registry id, matching `oauth_platform` on the `mistral-vibe` catalogue template.
    pub const ID: &'static str = "mistral-vibe";
}

/// Fallback access-token lifetime when the response omits `expires_in`, in seconds.
const DEFAULT_TTL_SECS: i64 = 3600;

/// Refresh one hour before expiry: the desktop session token is short-lived and cheap to renew.
const REFRESH_LEAD_SECS: u64 = 60 * 60;

/// Build the authorization URL for a loopback listener on `port`.
fn authorization_url(state: &str, pkce: Option<&Pkce>, port: u16) -> String {
    let challenge = pkce.map(Pkce::challenge).unwrap_or_default();
    // Captured from the installed Vibe 1.2.0 app's authorize URL. Its registered loopback URI
    // includes this path (not `/`) and the API audience. Ory rejects even a correct client id when
    // the redirect path differs from its registered URI.
    let redirect_uri = format!(
        "http://127.0.0.1:{port}{}",
        mistral_vibe::LOOPBACK_CALLBACK_PATH
    );
    format!(
        "{}?audience={}&client_id={}&code_challenge={}&code_challenge_method=S256\
         &nonce={}&redirect_uri={}&response_type=code&scope={}&state={}",
        mistral_vibe::AUTHORIZE,
        flow::url_encode(mistral_vibe::AUDIENCE),
        flow::url_encode(mistral_vibe::CLIENT_ID),
        flow::url_encode(challenge),
        flow::url_encode(state),
        flow::url_encode(&redirect_uri),
        flow::url_encode(mistral_vibe::SCOPES),
        flow::url_encode(state),
    )
}

/// Parse a token response into a [`TokenSet`]. `previous_refresh` survives a response that omits
/// a replacement, exactly like Claude's rotating refresh.
fn token_set_from(body: &str, previous_refresh: Option<&str>) -> Result<TokenSet> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Mistral Vibe token response is not JSON: {e}")))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Mistral Vibe token response has no access_token"))?;
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
    // Ory issues an opaque `access_token` *and* a JWT `id_token`. The LeChat backend
    // (`chat.mistral.ai`) authenticates with the JWT; the opaque token is not accepted there.
    // Keep the id token in `extra` so the relay can attach it as the bearer.
    if let Some(id_token) = value.get("id_token").and_then(Value::as_str) {
        tokens.extra.insert("id_token".into(), json!(id_token));
    }
    tokens.expires_at = expires_at_from_secs(expires_in);
    Ok(tokens)
}

#[async_trait]
impl OAuthPlatform for MistralVibe {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Mistral Vibe"
    }

    fn description(&self) -> &'static str {
        "Mistral's Vibe subscription login (Ory PKCE at auth.mistral.ai)."
    }

    fn begins_with_loopback(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Vibe binds an ephemeral loopback port and registers the `/auth/oidc/callback` path;
        // the OAuth redirect URI must match that path exactly. `validates_redirect_port` is false
        // below because the port is chosen at runtime, as in the installed desktop app.
        RedirectPorts {
            host: "127.0.0.1",
            primary: 0,
            fallback: None,
            path: "/auth/oidc/callback",
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
            mistral_vibe::LOOPBACK_CALLBACK_PATH
        );
        let mut pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &redirect_uri),
            ("client_id", mistral_vibe::CLIENT_ID),
        ];
        if let Some(pkce) = pkce {
            pairs.push(("code_verifier", pkce.verifier()));
        }
        let reply = http
            .post_form(mistral_vibe::TOKEN, &[], &pairs)
            .await
            .map_err(|e| oauth_error(format!("Mistral Vibe token request failed: {e}")))?;
        if reply.status != 200 {
            return Err(oauth_failure(
                super::refresh::FailureClass::Transient,
                "Mistral Vibe token",
                format!("HTTP {}: {}", reply.status, reply.body),
            ));
        }
        token_set_from(&reply.body, None)
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = ImpersonatingHttp::new()?;
        let pairs: Vec<(&str, &str)> = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", mistral_vibe::CLIENT_ID),
        ];
        let reply = http
            .post_form(mistral_vibe::TOKEN, &[], &pairs)
            .await
            .map_err(|e| oauth_error(format!("Mistral Vibe refresh request failed: {e}")))?;
        if reply.status != 200 {
            return Err(oauth_failure(
                super::refresh::FailureClass::Transient,
                "Mistral Vibe refresh",
                format!("HTTP {}: {}", reply.status, reply.body),
            ));
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
                mistral_vibe::USERINFO,
                &[("authorization", &format!("Bearer {}", tokens.access_token))],
            )
            .await
            .map_err(|e| oauth_error(format!("Mistral Vibe userinfo request failed: {e}")))?;
        if reply.status != 200 {
            return Err(oauth_failure(
                FailureClass::Transient,
                "Mistral Vibe userinfo",
                format!("HTTP {}: {}", reply.status, reply.body),
            ));
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|e| oauth_error(format!("Mistral Vibe userinfo is not JSON: {e}")))?;
        let id = value
            .get("sub")
            .and_then(Value::as_str)
            .ok_or_else(|| oauth_error("Mistral Vibe userinfo has no `sub`"))?
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
        // The LeChat consumer surface accepts the Ory `id_token` (a JWT), not the opaque
        // `access_token`. Send the JWT when we have one; fall back to the opaque token so a
        // plain Ory session still has some credential to present.
        let bearer = tokens
            .extra
            .get("id_token")
            .and_then(Value::as_str)
            .unwrap_or(&tokens.access_token);
        vec![
            header("authorization", &format!("Bearer {bearer}")).expect("static header name"),
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
        // Refresh returns no account metadata on this platform; preserve the previous identity.
        let mut tokens = self.refresh(refresh_token).await?;
        let prev = json!({
            "id": previous.extra.get("id").cloned().unwrap_or(Value::Null),
            "label": previous.extra.get("label").cloned().unwrap_or(Value::Null),
            "email": previous.extra.get("email").cloned().unwrap_or(Value::Null),
        });
        tokens.extra.insert("account".into(), prev);
        // A fresh id token was minted by the refresh grant; the previous one is dead the moment
        // it expires, so carry no stale copy forward. TokenSet carries its own extra already.
        Ok(tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The authorize URL must match the registered client: the `/auth/oidc/callback` path and the
    /// `audience` parameter are what Ory compares byte-for-byte. Captured from the installed Vibe
    /// 1.2.0 app after its login was refused as `invalid_request` with a bare `/` redirect.
    #[test]
    fn the_authorize_url_matches_the_desktop_registration() {
        let pkce = Pkce::generate();
        let url = authorization_url("state-value", Some(&pkce), 45_921);
        assert!(url.starts_with("https://auth.mistral.ai/oauth2/auth?"));
        assert!(url.contains("audience=https%3A%2F%2Fapi.mistral.ai"));
        assert!(url.contains(&format!(
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A45921{}",
            urlencoding_path()
        )));
        assert!(url.contains("scope=openid%20email%20profile%20offline_access"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-value"));
    }

    /// `flow::url_encode` percent-encodes the path's slash; keep the expectation honest instead of
    /// duplicating the encoder here.
    fn urlencoding_path() -> String {
        flow::url_encode(mistral_vibe::LOOPBACK_CALLBACK_PATH)
    }

    /// The Ory response carries both an opaque `access_token` and a JWT `id_token`. The LeChat
    /// backend authenticates with the JWT, so the exchange must keep it — dropping it was the
    /// original bug: every request went out with the opaque token and the consumer gateway
    /// rejected it.
    #[test]
    fn the_token_set_keeps_the_id_token_and_the_credential_sends_it() {
        let body = r#"{"access_token":"ory_at_opaque","id_token":"eyJhbGciOiJSUz","refresh_token":"ory_rt_x","expires_in":3599,"scope":"openid email profile offline_access","token_type":"bearer"}"#;
        let tokens = token_set_from(body, None).unwrap();
        assert_eq!(tokens.access_token, "ory_at_opaque");
        assert_eq!(
            tokens.extra.get("id_token").and_then(|v| v.as_str()),
            Some("eyJhbGciOiJSUz")
        );
        let headers = MistralVibe.apply_credential(&tokens);
        assert_eq!(headers.len(), 1);
        assert_eq!(
            headers[0].1.to_str().unwrap(),
            "Bearer eyJhbGciOiJSUz",
            "the relay must attach the id_token, not the opaque access token"
        );
    }

    /// Without an id token (a bare Ory exchange), the opaque token is what we have to present.
    #[test]
    fn the_credential_falls_back_to_the_opaque_token_without_an_id_token() {
        let body = r#"{"access_token":"ory_at_opaque","refresh_token":"ory_rt_x","expires_in":3599,"token_type":"bearer"}"#;
        let tokens = token_set_from(body, None).unwrap();
        assert!(tokens.extra.get("id_token").is_none());
        let headers = MistralVibe.apply_credential(&tokens);
        assert_eq!(headers[0].1.to_str().unwrap(), "Bearer ory_at_opaque");
    }
}
