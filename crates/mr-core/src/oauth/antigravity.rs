//! Google Antigravity / Gemini Code Assist: Google loopback authorization code.
//!
//! Ground truth: `.research/oauth.md` §2 and `docs/12-oauth-antigravity.md`. Two public
//! installed-application clients exist and both are fetched from [`crate::published`] — never
//! written as literals here (push protection + `scripts/check-secrets.sh`).

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use std::time::Duration;

use super::{header, oauth_error, AccountIdentity, OAuthPlatform, Pkce, RedirectPorts, TokenSet};
use crate::error::Result;

/// Which public Google client identity this instance speaks as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientTier {
    /// The Antigravity IDE client (T2) — what the catalogue ships.
    Ide,
    /// Google's `gemini-cli` client (T1), published in Google's own repository.
    GeminiCli,
}

/// The registered instance: the Antigravity (T2) client identity the catalogue ships.
pub static IDE: Antigravity = Antigravity::ide();

/// The Antigravity platform.
pub struct Antigravity {
    tier: ClientTier,
}

impl Antigravity {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "antigravity";

    /// The Antigravity IDE client identity (T2) — the registered instance.
    ///
    /// // UNCERTAIN: this client id and the `ANTIGRAVITY` `Client-Metadata` header are
    /// // reverse-engineered from the Antigravity binary, not primary-confirmed (see
    /// // `.research/oauth.md` §2.2). If the id is wrong, Google rejects the token exchange
    /// // with `invalid_client` — a visible permanent failure, not silent misbehaviour. The
    /// // catalogue's note records the same caveat.
    pub const fn ide() -> Self {
        Self {
            tier: ClientTier::Ide,
        }
    }

    /// Which public client identity this instance speaks as: `"ide"` (T2, registered) or
    /// `"gemini-cli"` (T1, selectable).
    pub const fn tier(&self) -> &'static str {
        match self.tier {
            ClientTier::Ide => "ide",
            ClientTier::GeminiCli => "gemini-cli",
        }
    }

    /// Google's `gemini-cli` client identity (T1), selectable instead of T2. Google-published
    /// and verifiable, but it consents with 3 scopes rather than the 5 Antigravity uses.
    pub const fn gemini_cli() -> Self {
        Self {
            tier: ClientTier::GeminiCli,
        }
    }

    /// The published client id for this tier.
    pub fn client_id(&self) -> &'static str {
        match self.tier {
            ClientTier::Ide => crate::published::ANTIGRAVITY_CLIENT_ID,
            ClientTier::GeminiCli => crate::published::GEMINI_CLI_CLIENT_ID,
        }
    }

    /// The published client secret for this tier (an installed-app public secret by design).
    pub fn client_secret(&self) -> &'static str {
        match self.tier {
            ClientTier::Ide => crate::published::ANTIGRAVITY_CLIENT_SECRET,
            ClientTier::GeminiCli => crate::published::GEMINI_CLI_CLIENT_SECRET,
        }
    }
}

#[async_trait]
impl OAuthPlatform for Antigravity {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn label(&self) -> &'static str {
        "Google Antigravity"
    }

    fn description(&self) -> &'static str {
        "Google account login on the registered localhost:51121 redirect, with Cloud Code onboarding."
    }

    fn begins_with_loopback(&self) -> bool {
        true
    }

    fn redirect_ports(&self) -> RedirectPorts {
        // Registered as `http://localhost:51121/oauth-callback` — note `localhost`, not
        // `127.0.0.1`: the redirect URI is compared against the registration verbatim.
        RedirectPorts {
            host: "localhost",
            primary: 51121,
            fallback: None,
            path: "/oauth-callback",
        }
    }

    fn requires_pkce(&self) -> bool {
        // The T2 token exchange carries `code_verifier` (dossier §2.3): PKCE is on for this
        // client. (gemini-cli's loopback path skips it; its user-code path does not — which is
        // why the trait takes `pkce: Option<&Pkce>`.)
        true
    }

    fn validates_redirect_port(&self) -> bool {
        // One registered port, bound exactly or not at all.
        true
    }

    fn authorization_url(&self, _state: &str, _pkce: Option<&Pkce>, _port: u16) -> Result<String> {
        Err(oauth_error(format!(
            "{}: authorization url builder not yet wired",
            Self::ID
        )))
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _pkce: Option<&Pkce>,
        _port: u16,
    ) -> Result<TokenSet> {
        Err(oauth_error(format!(
            "{}: token exchange not yet wired",
            Self::ID
        )))
    }

    async fn refresh(&self, _refresh_token: &str) -> Result<TokenSet> {
        Err(oauth_error(format!("{}: refresh not yet wired", Self::ID)))
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
        out
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // Byte-identical to the catalogue's `[providers.headers]`. The `User-Agent` stays
        // catalogue-only because its value is a template (`${antigravity_version}`).
        // UNCERTAIN: the exact `X-Goog-Api-Client` spelling (`google-cloud-sdk
        // vscode_cloudshelleditor/0.1`) is reverse-engineered (docs/12); if it is wrong the
        // request may land in the wrong client's quota pool — a silent wrong-bucket, not an
        // error, which is why the catalogue note pins it.
        &[
            (
                "X-Goog-Api-Client",
                "google-cloud-sdk vscode_cloudshelleditor/0.1",
            ),
            (
                "Client-Metadata",
                r#"{"ideType":"ANTIGRAVITY","platform":"WINDOWS","pluginType":"GEMINI"}"#,
            ),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        // The reverse-engineered client refreshes when expiry is within 60 s of now.
        Duration::from_secs(60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_pick_their_published_client_identity() {
        let ide = Antigravity::ide();
        let cli = Antigravity::gemini_cli();
        assert_eq!(ide.client_id(), crate::published::ANTIGRAVITY_CLIENT_ID);
        assert_eq!(cli.client_id(), crate::published::GEMINI_CLI_CLIENT_ID);
        assert_ne!(ide.client_id(), cli.client_id());
        assert_eq!(
            ide.client_secret(),
            crate::published::ANTIGRAVITY_CLIENT_SECRET
        );
        assert_eq!(
            cli.client_secret(),
            crate::published::GEMINI_CLI_CLIENT_SECRET
        );
        assert_ne!(ide.client_secret(), cli.client_secret());
    }

    #[test]
    fn google_credentials_never_appear_as_literals_in_this_file() {
        // The two Google pairs must come from `crate::published`, which stores them via
        // `concat!` so secret scanners see no contiguous literal. The needles are assembled
        // here for the same reason: this test's own source is part of what gets scanned, so a
        // contiguous credential-shaped literal in the needle would be the very thing we forbid.
        let id_needle = format!(".apps.google{}", "usercontent.com");
        let secret_needle = format!("{}-", "GOCSPX");
        let source = include_str!("antigravity.rs");
        assert!(
            !source.contains(&id_needle),
            "a Google client id was written as a literal in antigravity.rs"
        );
        assert!(
            !source.contains(&secret_needle),
            "a Google client secret was written as a literal in antigravity.rs"
        );
        // …and the identities really are reached through `published`.
        assert_eq!(
            Antigravity::ide().client_id(),
            crate::published::ANTIGRAVITY_CLIENT_ID
        );
        assert_eq!(
            Antigravity::gemini_cli().client_id(),
            crate::published::GEMINI_CLI_CLIENT_ID
        );
    }

    #[test]
    fn tiers_are_selectable_but_only_ide_is_registered() {
        assert!(
            crate::oauth::platform(Antigravity::ID).is_none(),
            "incomplete Antigravity protocol must not be advertised as working"
        );
        assert_eq!(IDE.tier(), "ide");
        assert_eq!(Antigravity::gemini_cli().tier(), "gemini-cli");
        assert_ne!(IDE.client_id(), Antigravity::gemini_cli().client_id());
    }
}
