//! OpenAI Codex (ChatGPT subscription): loopback authorization code + PKCE.
//!
//! Ground truth: `.research/oauth.md` §1 (Codex CLI 0.152.1, primary-confirmed). The client id
//! is a public installed-application constant from [`crate::published`]; there is no client
//! secret.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use std::time::Duration;

use super::{header, oauth_error, AccountIdentity, OAuthPlatform, Pkce, RedirectPorts, TokenSet};
use crate::error::Result;

/// The Codex platform.
pub struct Codex;

impl Codex {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "openai-codex";
}

#[async_trait]
impl OAuthPlatform for Codex {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "OpenAI Codex"
    }

    fn description(&self) -> &'static str {
        "ChatGPT subscription login: loopback redirect with PKCE, refreshed 5 minutes before expiry."
    }

    fn begins_with_loopback(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        RedirectPorts {
            host: "127.0.0.1",
            primary: 1455,
            fallback: Some(1457),
            path: "/auth/callback",
        }
    }

    fn requires_pkce(&self) -> bool {
        true
    }

    fn validates_redirect_port(&self) -> bool {
        // Fixed registered ports (1455, with 1457 as the documented fallback): the flow must
        // refuse to publish a redirect for a port outside that set.
        true
    }

    fn authorization_url(&self, _state: &str, _pkce: Option<&Pkce>, _port: u16) -> Result<String> {
        Err(oauth_error(format!(
            "{}: authorization url builder not yet wired",
            Self::ID
        )))
    }

    async fn exchange_code(&self, _code: &str, _pkce: Option<&Pkce>, _port: u16) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: token exchange not yet wired",
            Self::ID
        )))
    }

    async fn refresh(&self, _refresh_token: &str) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: refresh not yet wired",
            Self::ID
        )))
    }

    async fn account_identity(&self, _tokens: &TokenSet) -> Result<AccountIdentity> {
        Err(oauth_error(format!(
            "{}: identity extraction not yet wired",
            Self::ID
        )))
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        let mut out = Vec::new();
        if let Some(h) = header("authorization", &format!("Bearer {}", tokens.access_token)) {
            out.push(h);
        }
        // The account id is extracted from the JWT claim at https://api.openai.com/auth during
        // the exchange and stored in `extra`; a literal value there is a hard upstream failure.
        if let Some(account) = tokens.extra_str("chatgpt_account_id") {
            if let Some(h) = header("chatgpt-account-id", account) {
                out.push(h);
            }
        }
        out
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // Byte-identical to the catalogue's `[providers.headers]` for this provider — the
        // registry test enforces that coupling. `chatgpt-account-id` is credential-derived, so
        // it is deliberately absent: it comes from `apply_credential`.
        &[
            ("originator", "codex_cli_rs"),
            ("OpenAI-Beta", "responses=experimental"),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        // Codex refreshes at 5 minutes to expiry (its manager's `L = 8`-style window): a
        // refresh that lands after expiry costs a failed request, one before costs nothing.
        Duration::from_secs(300)
    }
}
