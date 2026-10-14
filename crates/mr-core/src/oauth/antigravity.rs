//! Google Antigravity / Gemini Code Assist: Google loopback authorization code.
//!
//! Ground truth: `.research/oauth.md` §2 and `docs/12-oauth-antigravity.md`. Two public
//! installed-application clients exist and both are fetched from [`crate::published`] — never
//! written as literals here (push protection + `scripts/check-secrets.sh`).

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::Value;
use std::time::Duration;

use super::flow::{url_encode, Http, OAuthHttp};
use super::refresh::{endpoint_failure, oauth_failure};
use super::{
    expires_at_from_secs, header, oauth_error, AccountIdentity, OAuthPlatform, Pkce, RedirectPorts,
    TokenSet,
};
use crate::error::Result;
use crate::published::{ANTIGRAVITY_EXTRA_SCOPES, ANTIGRAVITY_REDIRECT_URI, GEMINI_CLI_SCOPES};

const GOOGLE_TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

fn all_ide_scopes() -> Vec<&'static str> {
    GEMINI_CLI_SCOPES
        .iter()
        .chain(ANTIGRAVITY_EXTRA_SCOPES)
        .copied()
        .collect()
}

fn token_set_from(
    body: &str,
    previous_refresh: Option<&str>,
    previous_context: Option<&serde_json::Map<String, Value>>,
    scopes_default: Vec<String>,
    tier: ClientTier,
) -> Result<TokenSet> {
    let value: Value =
        serde_json::from_str(body).map_err(|_| oauth_error("Google token response is not JSON"))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Google token response has no access_token"))?;
    let mut tokens = TokenSet::new(access_token);
    tokens.refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned));
    if tokens.refresh_token.is_none() {
        return Err(oauth_error("Google did not return a refresh token; authorization cannot be renewed. Re-authorize with consent enabled."));
    }
    let oauth_tier = previous_context
        .and_then(|ctx| ctx.get("oauth_tier"))
        .and_then(Value::as_str)
        .unwrap_or(match tier {
            ClientTier::Ide => "antigravity-ide",
            ClientTier::GeminiCli => "gemini-cli",
        });
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .unwrap_or(3600);
    tokens.expires_at = expires_at_from_secs(expires_in);
    tokens.scopes = value
        .get("scope")
        .and_then(Value::as_str)
        .map(|s| s.split_whitespace().map(str::to_owned).collect())
        .unwrap_or(scopes_default);
    tokens.token_type = value
        .get("token_type")
        .and_then(Value::as_str)
        .unwrap_or("Bearer")
        .to_owned();
    tokens
        .extra
        .insert("oauth_tier".into(), Value::String(oauth_tier.into()));
    // Preserve only account-scoped Cloud Code context across refresh; never carry arbitrary
    // stale token response fields forward.
    for key in ["cloudcode_project_id", "cloudcode_tier_id"] {
        if let Some(previous) = previous_context.and_then(|ctx| ctx.get(key)) {
            tokens.extra.insert(key.to_owned(), previous.clone());
        }
    }
    Ok(tokens)
}

fn validated_project_id(project: &Value) -> Result<&str> {
    project
        .as_str()
        .or_else(|| project.get("id").and_then(Value::as_str))
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .ok_or_else(|| oauth_error("Cloud Code returned malformed project metadata"))
}

fn store_cloudcode_context(tokens: &mut TokenSet, project_id: &str, tier: Option<&str>) {
    tokens.extra.insert(
        "cloudcode_project_id".into(),
        Value::String(project_id.into()),
    );
    if let Some(tier) = tier {
        tokens
            .extra
            .insert("cloudcode_tier_id".into(), Value::String(tier.into()));
    }
}

fn parse_google_identity(reply: &super::flow::HttpReply) -> Result<AccountIdentity> {
    if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
        return Err(oauth_failure(class, Antigravity::ID, detail));
    }
    let value: Value = serde_json::from_str(&reply.body)
        .map_err(|_| oauth_error("Google userinfo response is not JSON"))?;
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| value.get("sub").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
        .ok_or_else(|| oauth_error("Google userinfo response has no stable account id"))?;
    let email = value
        .get("email")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let label = value
        .get("name")
        .and_then(Value::as_str)
        .or(email.as_deref())
        .unwrap_or(id)
        .to_owned();
    Ok(AccountIdentity {
        id: id.to_owned(),
        label,
        email,
        plan: None,
    })
}

fn valid_operation_name(name: &str) -> bool {
    let Some(id) = name.strip_prefix("operations/") else {
        return false;
    };
    !id.is_empty()
        && id.len() <= 256
        && !id.contains('/')
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

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
        // T2 PKCE is dossier-derived and still needs live-provider verification. Do not use this
        // flow as production-ready until tested; this flag matches the research protocol.
        self.tier == ClientTier::Ide
    }

    fn validates_redirect_port(&self) -> bool {
        // One registered port, bound exactly or not at all.
        true
    }

    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String> {
        let (redirect, scopes) = match self.tier {
            ClientTier::Ide => {
                if port != 51121 {
                    return Err(oauth_error(
                        "Antigravity IDE redirect must bind registered port 51121",
                    ));
                }
                (ANTIGRAVITY_REDIRECT_URI.to_owned(), all_ide_scopes())
            }
            ClientTier::GeminiCli => (
                crate::published::gemini_cli_loopback_redirect(port),
                GEMINI_CLI_SCOPES.to_vec(),
            ),
        };
        let scope = scopes.join(" ");
        let mut url = format!("https://accounts.google.com/o/oauth2/v2/auth?client_id={}&response_type=code&redirect_uri={}&scope={}",
            url_encode(self.client_id()), url_encode(&redirect), url_encode(&scope));
        if let Some(pkce) = pkce {
            url.push_str(&format!(
                "&code_challenge={}&code_challenge_method=S256",
                url_encode(pkce.challenge())
            ));
        } else if self.tier == ClientTier::Ide {
            return Err(oauth_error("Antigravity IDE authorization requires PKCE"));
        }
        url.push_str(&format!("&state={}", url_encode(state)));
        url.push_str("&access_type=offline");
        if self.tier == ClientTier::Ide {
            url.push_str("&prompt=consent");
        }
        Ok(url)
    }

    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet> {
        let redirect = match self.tier {
            ClientTier::Ide if port == 51121 => ANTIGRAVITY_REDIRECT_URI.to_owned(),
            ClientTier::Ide => {
                return Err(oauth_error(
                    "Antigravity IDE redirect must bind registered port 51121",
                ))
            }
            ClientTier::GeminiCli => crate::published::gemini_cli_loopback_redirect(port),
        };
        let mut pairs = vec![
            ("client_id", self.client_id()),
            ("client_secret", self.client_secret()),
            ("code", code),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect.as_str()),
        ];
        if let Some(pkce) = pkce {
            pairs.push(("code_verifier", pkce.verifier()));
        }
        let reply = Http::shared()?
            .post_form(GOOGLE_TOKEN_ENDPOINT, &[], &pairs)
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let scopes = match self.tier {
            ClientTier::Ide => all_ide_scopes(),
            ClientTier::GeminiCli => GEMINI_CLI_SCOPES.to_vec(),
        };
        token_set_from(
            &reply.body,
            None,
            None,
            scopes.into_iter().map(str::to_owned).collect(),
            self.tier,
        )
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let previous = TokenSet::new("");
        self.refresh_with_context(refresh_token, &previous).await
    }

    async fn refresh_with_context(
        &self,
        refresh_token: &str,
        previous: &TokenSet,
    ) -> Result<TokenSet> {
        let pairs = [
            ("client_id", self.client_id()),
            ("client_secret", self.client_secret()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ];
        let reply = Http::shared()?
            .post_form(GOOGLE_TOKEN_ENDPOINT, &[], &pairs)
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let scopes = match self.tier {
            ClientTier::Ide => all_ide_scopes(),
            ClientTier::GeminiCli => GEMINI_CLI_SCOPES.to_vec(),
        };
        token_set_from(
            &reply.body,
            Some(refresh_token),
            Some(&previous.extra),
            scopes.into_iter().map(str::to_owned).collect(),
            self.tier,
        )
    }

    async fn complete_login(&self, tokens: &mut TokenSet) -> Result<()> {
        let http = Http::shared()?;
        self.complete_login_with(tokens, http).await
    }

    async fn complete_login_with(&self, tokens: &mut TokenSet, http: &dyn OAuthHttp) -> Result<()> {
        let body = serde_json::json!({
            "metadata": {
                "ideType": if self.tier == ClientTier::Ide { "ANTIGRAVITY" } else { "IDE_UNSPECIFIED" },
                "platform": std::env::consts::OS.to_ascii_uppercase(),
                "pluginType": "GEMINI"
            }
        }).to_string();
        let authorization = format!("Bearer {}", tokens.access_token);
        let reply = http
            .post_json(
                "https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist",
                &[("Authorization", authorization.as_str())],
                body,
            )
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|_| oauth_error("Cloud Code loadCodeAssist response is not JSON"))?;
        if let Some(project) = value
            .get("cloudaicompanionProject")
            .filter(|p| !p.is_null())
        {
            let project_id = validated_project_id(project)?;
            store_cloudcode_context(
                tokens,
                project_id,
                value
                    .get("currentTier")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str),
            );
            return Ok(());
        }
        let tier = value
            .get("allowedTiers")
            .and_then(Value::as_array)
            .and_then(|tiers| {
                tiers
                    .iter()
                    .find(|t| t.get("isDefault").and_then(Value::as_bool) == Some(true))
            })
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .unwrap_or("legacy-tier");
        if tier != "free-tier" {
            return Err(oauth_error("Cloud Code requires a paid-tier project; specify an explicit project before onboarding"));
        }
        let onboard_body = serde_json::json!({"tierId": tier, "metadata": {
            "ideType": if self.tier == ClientTier::Ide { "ANTIGRAVITY" } else { "IDE_UNSPECIFIED" },
            "platform": std::env::consts::OS.to_ascii_uppercase(), "pluginType": "GEMINI"
        }})
        .to_string();
        let reply = http
            .post_json(
                "https://cloudcode-pa.googleapis.com/v1internal:onboardUser",
                &[("Authorization", authorization.as_str())],
                onboard_body,
            )
            .await?;
        if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(class, Self::ID, detail));
        }
        let op: Value = serde_json::from_str(&reply.body)
            .map_err(|_| oauth_error("Cloud Code onboarding response is not JSON"))?;
        let name = op
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| valid_operation_name(n))
            .ok_or_else(|| {
                oauth_error("Cloud Code onboarding did not return a valid operation name")
            })?;
        for attempt in 0..12 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            let url = format!("https://cloudcode-pa.googleapis.com/v1internal/{name}");
            let reply = http
                .get(&url, &[("Authorization", authorization.as_str())])
                .await?;
            if let Some((class, detail)) = endpoint_failure(reply.status, &reply.body) {
                return Err(oauth_failure(class, Self::ID, detail));
            }
            let operation: Value = serde_json::from_str(&reply.body)
                .map_err(|_| oauth_error("Cloud Code operation response is not JSON"))?;
            if operation.get("done").and_then(Value::as_bool) == Some(true) {
                if operation.get("error").is_some_and(|e| !e.is_null()) {
                    return Err(oauth_error("Cloud Code onboarding operation failed"));
                }
                let project = operation
                    .pointer("/response/cloudaicompanionProject")
                    .ok_or_else(|| {
                        oauth_error("Cloud Code onboarding completed without a project")
                    })?;
                let project_id = validated_project_id(project)?;
                store_cloudcode_context(tokens, project_id, Some(tier));
                return Ok(());
            }
        }
        Err(oauth_error("Cloud Code onboarding timed out"))
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        // OAuth2 v2 userinfo returns `id` (not necessarily OIDC `sub`); no `openid` scope is requested.
        let authorization = format!("Bearer {}", tokens.access_token);
        let reply = Http::shared()?
            .get(
                "https://www.googleapis.com/oauth2/v2/userinfo",
                &[("Authorization", authorization.as_str())],
            )
            .await?;
        parse_google_identity(&reply)
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
    use crate::oauth::flow::HttpReply;

    struct FakeHttp {
        replies: std::sync::Mutex<Vec<std::result::Result<HttpReply, String>>>,
        requests: std::sync::Mutex<Vec<(String, String)>>,
    }
    impl FakeHttp {
        fn new(replies: Vec<HttpReply>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into_iter().map(Ok).collect()),
                requests: Default::default(),
            }
        }
        fn take(&self, url: &str, body: String) -> Result<HttpReply> {
            self.requests.lock().unwrap().push((url.into(), body));
            self.replies.lock().unwrap().remove(0).map_err(oauth_error)
        }
    }
    #[async_trait::async_trait]
    impl OAuthHttp for FakeHttp {
        async fn post_form(
            &self,
            url: &str,
            _: &[(&str, &str)],
            pairs: &[(&str, &str)],
        ) -> Result<HttpReply> {
            self.take(url, super::super::flow::form_body(pairs))
        }
        async fn post_json(
            &self,
            url: &str,
            _: &[(&str, &str)],
            body: String,
        ) -> Result<HttpReply> {
            self.take(url, body)
        }
        async fn get(&self, url: &str, _: &[(&str, &str)]) -> Result<HttpReply> {
            self.take(url, String::new())
        }
    }
    fn reply(body: &str) -> HttpReply {
        HttpReply {
            status: 200,
            body: body.into(),
            content_type: Some("application/json".into()),
        }
    }

    #[tokio::test]
    async fn free_account_onboards_without_project_and_polls_bounded_operation() {
        let fake = FakeHttp::new(vec![
            reply(r#"{"allowedTiers":[{"id":"free-tier","isDefault":true}]}"#),
            reply(r#"{"name":"operations/op-123"}"#),
            reply(
                r#"{"done":true,"response":{"cloudaicompanionProject":{"id":"managed-project"}}}"#,
            ),
        ]);
        let mut tokens = TokenSet::new("access");
        IDE.complete_login_with(&mut tokens, &fake).await.unwrap();
        assert_eq!(
            tokens.extra_str("cloudcode_project_id"),
            Some("managed-project")
        );
        let rotated = token_set_from(
            r#"{"access_token":"next"}"#,
            Some("refresh"),
            Some(&tokens.extra),
            vec![],
            ClientTier::Ide,
        )
        .unwrap();
        assert_eq!(
            rotated.extra_str("cloudcode_project_id"),
            Some("managed-project")
        );
        let requests = fake.requests.lock().unwrap();
        assert!(requests[1].1.contains("free-tier"));
        assert!(!requests[1].1.contains("cloudaicompanionProject"));
        assert_eq!(
            requests[2].0,
            "https://cloudcode-pa.googleapis.com/v1internal/operations/op-123"
        );
    }

    #[tokio::test]
    async fn paid_tier_without_project_fails_closed() {
        let fake = FakeHttp::new(vec![reply(
            r#"{"allowedTiers":[{"id":"standard-tier","isDefault":true}]}"#,
        )]);
        let mut tokens = TokenSet::new("access");
        assert!(IDE.complete_login_with(&mut tokens, &fake).await.is_err());
        assert_eq!(fake.requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn google_v2_userinfo_id_shape_is_supported() {
        let identity = parse_google_identity(&reply(
            r#"{"id":"123","email":"a@example.test","name":"Alice"}"#,
        ))
        .unwrap();
        assert_eq!(identity.id, "123");
        assert_eq!(identity.email.as_deref(), Some("a@example.test"));
        assert_eq!(identity.label, "Alice");
        assert!(parse_google_identity(&reply(r#"{"name":"No id"}"#)).is_err());
    }

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
    fn ide_authorize_url_has_fixed_redirect_scopes_and_pkce() {
        let pkce = Pkce::from_verifier("A".repeat(43)).unwrap();
        let url = IDE
            .authorization_url("csrf-state", Some(&pkce), 51121)
            .unwrap();
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A51121%2Foauth-callback"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("prompt=consent"));
        assert!(url.contains("access_type=offline"));
        for scope in all_ide_scopes() {
            assert!(url.contains(&url_encode(scope)));
        }
        assert!(!url.contains(&url_encode(pkce.verifier())));
        assert!(IDE.authorization_url("x", Some(&pkce), 51122).is_err());
    }

    #[test]
    fn google_tokens_preserve_refresh_and_project_fields() {
        let first = token_set_from(
            r#"{"access_token":"access","refresh_token":"refresh","expires_in":3600,"scope":"openid email"}"#,
            None,
            None,
            vec!["fallback".into()],
            ClientTier::Ide,
        ).unwrap();
        assert_eq!(first.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(first.scopes, ["openid", "email"]);
        let mut first = first;
        first
            .extra
            .insert("cloudcode_project_id".into(), Value::String("p".into()));
        first.extra.insert(
            "cloudcode_tier_id".into(),
            Value::String("free-tier".into()),
        );
        let rotated = token_set_from(
            r#"{"access_token":"next","expires_in":1800}"#,
            first.refresh_token.as_deref(),
            Some(&first.extra),
            vec![],
            ClientTier::Ide,
        )
        .unwrap();
        assert_eq!(rotated.refresh_token.as_deref(), Some("refresh"));
        assert_eq!(rotated.extra_str("cloudcode_project_id"), Some("p"));
        assert_eq!(rotated.extra_str("cloudcode_tier_id"), Some("free-tier"));
    }

    #[test]
    fn tiers_are_selectable_but_only_ide_is_registered() {
        assert!(
            crate::oauth::platform(Antigravity::ID).is_some(),
            "implemented Antigravity OAuth platform must resolve from the registry"
        );
        assert_eq!(IDE.tier(), "ide");
        assert_eq!(Antigravity::gemini_cli().tier(), "gemini-cli");
        assert_ne!(IDE.client_id(), Antigravity::gemini_cli().client_id());
    }
}
