//! Anthropic Claude: the subscription login Claude Code performs, finished by hand.
//!
//! ## Why this flow is "paste", not loopback or device
//!
//! Claude's OAuth client is registered to a redirect on **Anthropic's** origin,
//! [`claude::CODE_CALLBACK`]. That page cannot redirect anywhere local, so the browser leaves this
//! machine and never comes back: it renders the authorization code (as `code#state`) for the human
//! to copy into the panel. In the trait's vocabulary the platform is therefore neither
//! [`begins_with_loopback`](OAuthPlatform::begins_with_loopback) nor a device flow — there is no
//! user code to type at the provider and nothing to poll. It declares
//! [`paste_redirect_uri`](OAuthPlatform::paste_redirect_uri), which is exactly the signal the
//! session layer uses to skip binding a listener and wait for a submitted redirect instead.
//!
//! The practical consequence is that **this is the one login that works on a headless box with the
//! browser on a different machine**, without a tunnel: the URL is opened wherever the human is, and
//! the code is typed back. It is also the flow OmniRoute, CLIProxyAPI and every other Claude Code
//! impersonator implements, which is why the shape is well attested even though Anthropic
//! documents none of it.
//!
//! ## The exchange is JSON and repeats the state
//!
//! The token endpoint takes `application/json`, not a form ([`flow::Http::post_json`]), and requires
//! the same `state` the authorization request carried — not merely the verifier. That is why this
//! platform implements [`exchange_code_with_state`](OAuthPlatform::exchange_code_with_state) and
//! refuses the stateless [`exchange_code`](OAuthPlatform::exchange_code): a verifier without its
//! state is answered with `invalid_request`.
//!
//! ## Tokens
//!
//! The response is `{access_token, refresh_token, expires_in, scope, token_type, account?}`.
//! Refresh tokens **rotate**: the response always carries a new one, and the presented one stops
//! working. A refresh that somehow omits it therefore keeps the old value rather than dropping the
//! credential — see [`refresh_with_context`](OAuthPlatform::refresh_with_context).
//!
//! `// UNCERTAIN:` no primary source states the access-token lifetime. `expires_in` is honoured
//! when present (observed: 8 hours); [`DEFAULT_TTL_SECS`] only covers a response that omits it, and
//! is deliberately short so an unlabelled token is refreshed early rather than used dead.
//!
//! The access token is `sk-ant-oat01-…` and is sent as `Authorization: Bearer`, **never**
//! `x-api-key`; the relay's `AuthKind::OAuth` path does that, and the catalogue template for
//! `claude-code` declares OAuth so no key-shaped credential can be configured by accident.
//!
//! ## Identity
//!
//! The token response's `account` object (`uuid`, `email_address`) is persisted in
//! [`TokenSet::extra`], so multi-account identity costs no request. Only a refresh — which returns
//! no `account` — falls back to `GET /api/oauth/profile`, and only when the extras were lost.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{json, Value};
use std::time::Duration;

use super::flow::{self, Http};
use super::refresh::{endpoint_failure, oauth_failure};
use super::{
    expires_at_from_secs, header, oauth_error, AccountIdentity, DeviceAuthorization, OAuthPlatform,
    Pkce, RedirectPorts, TokenSet,
};
use crate::error::Result;
use crate::published::{claude, CLAUDE_CODE_CLIENT_ID};
use crate::quota::claude::CLAUDE_CODE_VERSION;

/// Anthropic's Claude subscription, logged in the way the Claude Code CLI logs in.
pub struct Claude;

impl Claude {
    /// Registry id, matching `oauth_platform` on the `claude-code` catalogue template.
    pub const ID: &'static str = "claude";
}

/// Fallback access-token lifetime when the response omits `expires_in`, in seconds. An hour is
/// shorter than the observed eight, so the worst case is one unnecessary refresh.
const DEFAULT_TTL_SECS: i64 = 3600;

/// Refresh four hours before expiry, the window `docs/10-oauth-overview.md §5` records for Claude.
///
/// `// UNCERTAIN:` that number appears in no primary source. If the real window is shorter, a
/// request can fail with a 401 that the refresher repairs on the spot; if it is longer, a still-valid
/// token is renewed early, which is cheap and rotates the refresh credential sooner. Neither outcome
/// loses the account.
const REFRESH_LEAD_SECS: u64 = 4 * 60 * 60;

/// Build the authorization URL.
///
/// `code=true` comes first and is not a boolean the endpoint reads: the CLI sends the literal
/// string, and the page uses it to render the code for copy-and-paste instead of attempting a
/// redirect. Dropping it produces a page that tries to reach the redirect URI and leaves the human
/// with no code.
fn authorization_url(state: &str, pkce: &Pkce) -> String {
    format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}\
         &code_challenge={}&code_challenge_method=S256&state={}",
        claude::AUTHORIZE,
        flow::url_encode(CLAUDE_CODE_CLIENT_ID),
        flow::url_encode(claude::CODE_CALLBACK),
        flow::url_encode(claude::SCOPES),
        flow::url_encode(pkce.challenge()),
        flow::url_encode(state),
    )
}

/// Turn a token response into a [`TokenSet`].
///
/// `previous_refresh` carries the presented refresh token: a rotating response replaces it, and a
/// response without one leaves it intact.
fn token_set_from(body: &str, previous_refresh: Option<&str>) -> Result<TokenSet> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Claude token response is not JSON: {e}")))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Claude token response has no access_token"))?;
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
    tokens.expires_at = expires_at_from_secs(expires_in);
    tokens.scopes = value
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or_else(|| claude::SCOPES.split(' ').map(str::to_owned).collect());
    tokens.token_type = value
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or("Bearer")
        .to_string();
    carry_account(&mut tokens, &value);
    Ok(tokens)
}

/// Copy the `account` object out of a token response into the token set's extras.
///
/// Only the two fields that make a useful identity line are kept. The organization id is stored
/// because the profile endpoint is scoped by it, but it is never sent anywhere by this module.
fn carry_account(tokens: &mut TokenSet, value: &Value) {
    let Some(account) = value.get("account").and_then(Value::as_object) else {
        return;
    };
    for (from, to) in [
        ("uuid", "account_uuid"),
        ("email_address", "account_email"),
        ("organization_uuid", "organization_uuid"),
    ] {
        if let Some(text) = account.get(from).and_then(Value::as_str) {
            tokens
                .extra
                .insert(to.to_string(), Value::String(text.to_owned()));
        }
    }
}

/// Read an identity out of `/api/oauth/profile`.
///
/// The payload is `{account: {uuid, email_address, full_name}, organization: {...}}`. It is parsed
/// tolerantly — a missing `account` wrapper, `email` instead of `email_address`, and a plan under
/// the organization — because the endpoint is undocumented and its shape has changed before.
fn identity_from_profile(body: &str) -> Result<AccountIdentity> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Claude profile response is not JSON: {e}")))?;
    let account = value.get("account").unwrap_or(&value);
    let id = account
        .get("uuid")
        .or_else(|| account.get("account_uuid"))
        .or_else(|| account.get("sub"))
        .or_else(|| account.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Claude profile carries no account id"))?
        .to_owned();
    let email = account
        .get("email_address")
        .or_else(|| account.get("email"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let plan = value
        .get("organization")
        .and_then(|org| {
            org.get("rate_limit_tier")
                .or_else(|| org.get("organization_type"))
                .or_else(|| org.get("plan"))
        })
        .or_else(|| value.get("plan"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let label = account
        .get("full_name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| email.clone())
        .unwrap_or_else(|| "Claude".to_string());
    Ok(AccountIdentity {
        id,
        label,
        email,
        plan,
    })
}

#[async_trait]
impl OAuthPlatform for Claude {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Claude (subscription)"
    }

    fn description(&self) -> &'static str {
        "Sign in at claude.ai with your Claude subscription, then paste the code the page shows. \
         No local port is used, so this also works on a headless server."
    }

    fn begins_with_loopback(&self) -> bool {
        false
    }

    /// The signal that makes this a paste flow: a redirect URI that belongs to someone else.
    fn paste_redirect_uri(&self) -> Option<&'static str> {
        Some(claude::CODE_CALLBACK)
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Inert: no listener is bound for this flow. Callers must consult
        // `paste_redirect_uri()`/`begins_with_loopback()` before using these.
        RedirectPorts {
            host: "",
            primary: 0,
            fallback: None,
            path: "",
        }
    }

    fn requires_pkce(&self) -> bool {
        true
    }

    fn validates_redirect_port(&self) -> bool {
        false
    }

    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, _port: u16) -> Result<String> {
        let pkce = pkce.ok_or_else(|| oauth_error("Claude login requires a PKCE verifier"))?;
        Ok(authorization_url(state, pkce))
    }

    /// Refused: the exchange needs the `state` the authorization request carried, which a
    /// stateless call cannot supply. The session layer routes this platform through
    /// [`exchange_code_with_state`](Self::exchange_code_with_state).
    async fn exchange_code(
        &self,
        _code: &str,
        _pkce: Option<&Pkce>,
        _port: u16,
    ) -> Result<TokenSet> {
        Err(oauth_error(
            "Claude login exchanges the authorization code together with its state; \
             the browser session supplies it",
        ))
    }

    async fn exchange_code_with_state(
        &self,
        code: &str,
        state: &str,
        pkce: Option<&Pkce>,
        _port: u16,
    ) -> Result<TokenSet> {
        let pkce = pkce.ok_or_else(|| oauth_error("Claude exchange requires a PKCE verifier"))?;
        let http = Http::shared()?;
        let body = json!({
            "grant_type": "authorization_code",
            "code": code,
            "state": state,
            "client_id": CLAUDE_CODE_CLIENT_ID,
            "redirect_uri": claude::CODE_CALLBACK,
            "code_verifier": pkce.verifier(),
        })
        .to_string();
        let reply = http
            .post_json(
                claude::TOKEN,
                &[
                    ("Accept", "application/json"),
                    ("anthropic-beta", claude::OAUTH_BETA),
                ],
                body,
            )
            .await?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, Self::ID, failure.1));
        }
        token_set_from(&reply.body, None)
    }

    /// Refresh: same endpoint, **JSON** body. The presented refresh token is single-use, so a
    /// response that omits the replacement keeps the old one instead of leaving the account
    /// without a way to renew itself.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = Http::shared()?;
        let body = json!({
            "grant_type": "refresh_token",
            "client_id": CLAUDE_CODE_CLIENT_ID,
            "refresh_token": refresh_token,
        })
        .to_string();
        let reply = http
            .post_json(
                claude::TOKEN,
                &[
                    ("Accept", "application/json"),
                    ("anthropic-beta", claude::OAUTH_BETA),
                ],
                body,
            )
            .await?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, Self::ID, failure.1));
        }
        token_set_from(&reply.body, Some(refresh_token))
    }

    /// Keep the account facts across a refresh: the token response for a refresh grant carries no
    /// `account`, and re-fetching the profile on every renewal would turn a local refresh into a
    /// network round trip.
    async fn refresh_with_context(
        &self,
        refresh_token: &str,
        previous: &TokenSet,
    ) -> Result<TokenSet> {
        let mut refreshed = self.refresh(refresh_token).await?;
        for key in ["account_uuid", "account_email", "organization_uuid"] {
            if !refreshed.extra.contains_key(key) {
                if let Some(value) = previous.extra.get(key) {
                    refreshed.extra.insert(key.to_string(), value.clone());
                }
            }
        }
        Ok(refreshed)
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        if let Some(id) = tokens.extra_str("account_uuid") {
            let email = tokens.extra_str("account_email").map(str::to_owned);
            let label = email.clone().unwrap_or_else(|| "Claude".to_string());
            return Ok(AccountIdentity {
                id: id.to_owned(),
                label,
                email,
                plan: None,
            });
        }

        let http = Http::shared()?;
        let reply = http
            .get(
                claude::PROFILE,
                &[
                    ("Authorization", &format!("Bearer {}", tokens.access_token)),
                    ("anthropic-beta", claude::OAUTH_BETA),
                    ("User-Agent", &format!("claude-code/{CLAUDE_CODE_VERSION}")),
                ],
            )
            .await?;
        if !(200..300).contains(&reply.status) {
            return Err(oauth_error(format!(
                "Claude profile lookup failed with HTTP {}",
                reply.status
            )));
        }
        identity_from_profile(&reply.body)
    }

    /// The bearer, exactly as the CLI sends it. No `x-api-key`: a subscription token is refused on
    /// that header, and the catalogue template cannot carry one.
    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        header("Authorization", &format!("Bearer {}", tokens.access_token))
            .into_iter()
            .collect()
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        &[
            ("anthropic-version", claude::ANTHROPIC_VERSION),
            ("anthropic-beta", claude::ANTHROPIC_BETA),
            (claude::APP_HEADER, claude::APP),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        Duration::from_secs(REFRESH_LEAD_SECS)
    }

    /// Not a device flow: there is no user code and nothing to poll, which is why
    /// [`paste_redirect_uri`](Self::paste_redirect_uri) is what selects this platform's login shape.
    async fn device_start(&self) -> Result<DeviceAuthorization> {
        Err(oauth_error(
            "Claude login has no device-code flow; use the paste-the-code login",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authorization_url_carries_the_paste_shape() {
        let pkce = Pkce::from_verifier("k".repeat(43)).expect("a valid verifier");
        let url = authorization_url("state-123", &pkce);
        assert!(url.contains(&format!(
            "code_challenge={}",
            flow::url_encode(pkce.challenge())
        )));
        assert!(url.starts_with("https://claude.ai/oauth/authorize?code=true&client_id="));
        assert!(url.contains(&format!("client_id={}", CLAUDE_CODE_CLIENT_ID)));
        assert!(url.contains("response_type=code"));
        assert!(url.contains(
            "redirect_uri=https%3A%2F%2Fconsole.anthropic.com%2Foauth%2Fcode%2Fcallback"
        ));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=state-123"));
        assert!(url.contains(&format!("scope={}", flow::url_encode(claude::SCOPES))));
    }

    #[test]
    fn a_token_response_keeps_the_account_and_rotates_the_refresh_token() {
        let body = r#"{
            "access_token": "sk-ant-oat01-access",
            "refresh_token": "sk-ant-ort01-new",
            "expires_in": 28800,
            "scope": "user:inference user:profile",
            "token_type": "Bearer",
            "account": {
                "uuid": "acct-1",
                "email_address": "person@example.test",
                "organization_uuid": "org-1"
            }
        }"#;
        let tokens = token_set_from(body, Some("sk-ant-ort01-old")).unwrap();
        assert_eq!(tokens.access_token, "sk-ant-oat01-access");
        assert_eq!(tokens.refresh_token.as_deref(), Some("sk-ant-ort01-new"));
        assert_eq!(
            tokens.scopes,
            vec!["user:inference".to_string(), "user:profile".to_string()]
        );
        assert_eq!(tokens.extra_str("account_uuid"), Some("acct-1"));
        assert_eq!(
            tokens.extra_str("account_email"),
            Some("person@example.test")
        );
        assert!(tokens.expires_at > crate::ids::Millis::now());
    }

    #[test]
    fn a_refresh_without_a_new_refresh_token_keeps_the_old_one() {
        let tokens = token_set_from(
            r#"{"access_token":"a","expires_in":60}"#,
            Some("old-refresh"),
        )
        .unwrap();
        assert_eq!(tokens.refresh_token.as_deref(), Some("old-refresh"));
        assert_eq!(tokens.scopes, claude::SCOPES.split(' ').collect::<Vec<_>>());
    }

    #[test]
    fn the_profile_parser_accepts_the_wrapped_and_flat_shapes() {
        let wrapped = r#"{
            "account": {"uuid": "u1", "email_address": "a@b.test", "full_name": "A Person"},
            "organization": {"rate_limit_tier": "max_20x", "organization_uuid": "o1"}
        }"#;
        let identity = identity_from_profile(wrapped).unwrap();
        assert_eq!(identity.id, "u1");
        assert_eq!(identity.label, "A Person");
        assert_eq!(identity.email.as_deref(), Some("a@b.test"));
        assert_eq!(identity.plan.as_deref(), Some("max_20x"));

        let flat = r#"{"account_uuid": "u2", "email": "c@d.test"}"#;
        let identity = identity_from_profile(flat).unwrap();
        assert_eq!(identity.id, "u2");
        assert_eq!(identity.label, "c@d.test");
        assert_eq!(identity.plan, None);

        assert!(identity_from_profile("{}").is_err());
    }

    #[test]
    fn the_catalogue_entry_agrees_with_this_code() {
        let catalog = crate::catalog::catalog();
        let template = catalog.get("claude-code").expect("claude-code is shipped");
        assert_eq!(
            template.dialect,
            crate::dialect::Dialect::ClaudeCli,
            "the template must select the surface that prepends the identity block"
        );
        assert_eq!(template.oauth_platform.as_deref(), Some(Claude::ID));
        assert!(matches!(
            template.auth,
            crate::dialect::AuthKind::OAuth { ref platform } if platform == Claude::ID
        ));
        for (key, expected) in [
            ("oauth_client_id", CLAUDE_CODE_CLIENT_ID),
            ("oauth_authorize_url", claude::AUTHORIZE),
            ("oauth_token_url", claude::TOKEN),
            ("oauth_redirect_uri", claude::CODE_CALLBACK),
            ("oauth_scopes", claude::SCOPES),
        ] {
            assert_eq!(
                template.extra.get(key).and_then(Value::as_str),
                Some(expected),
                "the catalogue's {key} must be the constant the flow sends"
            );
        }
        // The relay injects a missing header from the fingerprint, so an operator-edited template
        // cannot silently lose one; what the test above cannot see is a template that *contradicts*
        // the code, which is why every pair is compared byte for byte.
        for (name, value) in Claude.fingerprint_headers() {
            assert_eq!(
                template.headers.get(*name).map(String::as_str),
                Some(*value),
                "the catalogue must send {name}: {value}"
            );
        }
    }

    #[test]
    fn the_fingerprint_is_the_cli_one() {
        let platform = Claude;
        assert_eq!(
            platform.fingerprint_headers(),
            &[
                ("anthropic-version", "2023-06-01"),
                ("anthropic-beta", "claude-code-20250219,oauth-2025-04-20"),
                ("x-app", "cli"),
            ]
        );
        assert_eq!(platform.paste_redirect_uri(), Some(claude::CODE_CALLBACK));
        assert!(!platform.begins_with_loopback());
        assert!(!platform.supports_device_flow());
        assert!(platform.requires_pkce());
        assert_eq!(platform.refresh_lead(), Duration::from_secs(4 * 60 * 60));
    }
}
