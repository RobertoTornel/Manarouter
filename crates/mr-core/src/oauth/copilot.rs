//! GitHub Copilot: the GitHub device-code flow, then a short-lived session token minted from
//! the long-lived GitHub token.
//!
//! Ground truth: `.research/oauth.md` §3 and `docs/13-oauth-copilot.md`. The client id is
//! [`crate::published::COPILOT_CLIENT_ID`]; there is no client secret.

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use std::time::Duration;

use super::{header, oauth_error, AccountIdentity, OAuthPlatform, RedirectPorts, TokenSet};
use crate::error::Result;

/// The Copilot platform.
pub struct Copilot;

impl Copilot {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "github-copilot";
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

    async fn exchange_code(&self, _code: &str, _pkce: Option<&super::Pkce>, _port: u16) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: there is no authorization code; the device flow is used instead",
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
        // The session token minted from the GitHub token — never the GitHub token itself.
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
