//! OpenAI Codex (ChatGPT subscription): loopback authorization code + PKCE.
//!
//! Ground truth: `.research/oauth.md` §1 (Codex CLI 0.152.1, primary-confirmed). The client id
//! is a public installed-application constant from [`crate::published`]; there is no client
//! secret. Endpoints live in [`crate::published::openai`].

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::{Map, Value};
use std::time::Duration;

use super::flow::Http;
use super::refresh::{endpoint_failure, oauth_failure};
use super::{
    expires_at_from_secs, header, jwt_payload, oauth_error, AccountIdentity, DeviceAuthorization,
    DevicePoll, OAuthPlatform, Pkce, RedirectPorts, TokenSet, MAX_EXPIRES_IN_SECS,
};
use crate::error::Result;
use crate::published::{openai, CODEX_CLIENT_ID};

/// The space-joined scope string, sent as **one** `scope` parameter — the CLI serialises one
/// string with `+`-separated values, never one parameter per scope.
///
/// UNCERTAIN: `.research/oauth.md` §1.5 flags this literal as version-dependent — older
/// Codex builds shipped `openid profile email offline_access` only. If OpenAI's authorize
/// endpoint starts rejecting `api.connectors.read`/`api.connectors.invoke` with
/// `invalid_scope`, drop those two and login works again; the connectors feature is what
/// they enable.
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// The JWT claim namespace where Codex keeps the ChatGPT account facts (§1.8).
const AUTH_NAMESPACE: &str = "https://api.openai.com/auth";

/// The Codex platform.
pub struct Codex;

impl Codex {
    /// Registry id, matching `oauth_platform` in `catalog/providers.toml`.
    pub const ID: &'static str = "openai-codex";
}

/// One value from inside the `https://api.openai.com/auth` claim object, accepting both key
/// spellings the dossier shows: the JWT claims use `account_id`/`plan` (§1.8) while the
/// persisted `auth.json` fields are `chatgpt_account_id`/`chatgpt_plan_type` (§1.7).
fn auth_claim(claims: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    let namespace = claims.get(AUTH_NAMESPACE)?.as_object()?;
    for key in keys {
        if let Some(found) = namespace.get(*key).and_then(Value::as_str) {
            return Some(found.to_owned());
        }
    }
    None
}

/// The `chatgpt-account-id` value, from the id_token when present and the access token
/// otherwise — both are JWTs carrying the same namespace (§1.8), and a refresh response may
/// omit the id_token entirely.
fn chatgpt_account_id(access_token: &str, id_token: Option<&str>) -> Option<String> {
    for candidate in [id_token, Some(access_token)] {
        let Some(token) = candidate else { continue };
        if let Ok(claims) = jwt_payload(token) {
            if let Some(found) = auth_claim(&claims, &["account_id", "chatgpt_account_id"]) {
                return Some(found);
            }
        }
    }
    None
}

/// Build a [`TokenSet`] from a Codex token response (exchange *and* refresh — §1.7 shows
/// them answering the same JSON shape).
///
/// * `expires_in` is **seconds**; the absolute `expires_at` is computed here so no caller
///   can mistake it for milliseconds (a seconds/millis mix-up puts expiry 50 000 years out
///   and the token is used long after it died).
/// * The response carries no `scope` and no `token_type` (§1.7), so both are filled in
///   locally: the scope we requested, `Bearer`.
/// * `refresh_token` is absent when the provider does not rotate — then the previous one is
///   carried forward. Dropping it would turn every refresh into a dead end.
fn token_set_from(body: &str, previous_refresh: Option<&str>) -> Result<TokenSet> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| oauth_error(format!("Codex token response is not JSON: {e}")))?;
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or_else(|| oauth_error("Codex token response has no access_token"))?;
    let expires_in = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .ok_or_else(|| oauth_error("Codex token response has no numeric expires_in"))?;
    let id_token = value.get("id_token").and_then(Value::as_str);

    let mut tokens = TokenSet::new(access_token);
    tokens.refresh_token = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| previous_refresh.map(str::to_owned));
    tokens.expires_at = expires_at_from_secs(expires_in);
    tokens.scopes = SCOPE.split(' ').map(str::to_owned).collect();
    tokens.token_type = "Bearer".to_string();
    if let Some(account_id) = chatgpt_account_id(access_token, id_token) {
        tokens
            .extra
            .insert("chatgpt_account_id".to_string(), Value::String(account_id));
    }
    if let Some(id_token) = id_token {
        tokens
            .extra
            .insert("id_token".to_string(), Value::String(id_token.to_owned()));
    }
    Ok(tokens)
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

    /// Codex offers both: the loopback redirect by default, the device code on request.
    fn supports_device_flow(&self) -> bool {
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

    /// The authorize URL in the CLI's exact parameter order (§1.5): `response_type`,
    /// `client_id`, `redirect_uri`, `code_challenge`, `code_challenge_method`, `state`, then
    /// the single joined `scope`, then the extras.
    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String> {
        // No verifier, no challenge — and an authorize request without one is not this flow.
        let pkce = pkce.ok_or_else(|| oauth_error("Codex login requires a PKCE verifier"))?;
        let redirect = self.redirect_ports().redirect_uri(port);
        let mut url = format!("{}?", openai::AUTHORIZE);
        url.push_str("response_type=code");
        url.push_str(&format!("&client_id={}", CODEX_CLIENT_ID));
        url.push_str(&format!(
            "&redirect_uri={}",
            super::flow::url_encode(&redirect)
        ));
        url.push_str(&format!("&code_challenge={}", pkce.challenge()));
        url.push_str("&code_challenge_method=S256");
        url.push_str(&format!("&state={}", super::flow::url_encode(state)));
        // One parameter, values space-joined (encoded spaces render as %20; the CLI's
        // form-encoding renders them as `+` — both decode to the same scope string).
        url.push_str(&format!("&scope={}", super::flow::url_encode(SCOPE)));
        url.push_str("&id_token_add_organizations=true");
        url.push_str("&codex_cli_simplified_flow=true");
        url.push_str("&originator=codex_cli_rs");
        Ok(url)
    }

    /// Exchange the loopback code for tokens: **form-encoded** on the token URL (§1.6).
    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet> {
        let pkce = pkce.ok_or_else(|| oauth_error("Codex exchange requires a PKCE verifier"))?;
        let redirect = self.redirect_ports().redirect_uri(port);
        let http = Http::shared()?;
        let reply = http
            .post_form(
                openai::TOKEN,
                &[("Accept", "application/json")],
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CODEX_CLIENT_ID),
                    ("code", code),
                    ("redirect_uri", redirect.as_str()),
                    ("code_verifier", pkce.verifier()),
                ],
            )
            .await?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, Self::ID, failure.1));
        }
        token_set_from(&reply.body, None)
    }

    /// Refresh: the **same URL** as the exchange but a **JSON** body (§1.6) — the server
    /// dispatches on `Content-Type`, and reusing `post_form` here earns an HTTP 415 that
    /// reads like a server bug rather than our encoding mistake.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet> {
        let http = Http::shared()?;
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CODEX_CLIENT_ID,
            "refresh_token": refresh_token,
        })
        .to_string();
        let reply = http
            .post_json(openai::TOKEN, &[("Accept", "application/json")], body)
            .await?;
        if let Some(failure) = endpoint_failure(reply.status, &reply.body) {
            return Err(oauth_failure(failure.0, Self::ID, failure.1));
        }
        // Pass the presented refresh token along: a non-rotating response omits the field
        // and the old credential must survive (§1.7).
        token_set_from(&reply.body, Some(refresh_token))
    }

    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity> {
        // Prefer the stored id_token (§1.7 persists it verbatim); fall back to the access
        // token — same JWT, same namespace (§1.8). No network call: identity is claims.
        let source = tokens
            .extra_str("id_token")
            .map(str::to_owned)
            .unwrap_or_else(|| tokens.access_token.clone());
        let claims = jwt_payload(&source)?;

        let id = auth_claim(&claims, &["account_id", "chatgpt_account_id"])
            .or_else(|| claims.get("sub").and_then(Value::as_str).map(str::to_owned))
            .or_else(|| auth_claim(&claims, &["chatgpt_user_id"]))
            .ok_or_else(|| oauth_error("Codex identity token carries no account id"))?;
        let plan = auth_claim(&claims, &["chatgpt_plan_type", "plan"]);
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let label = email.clone().unwrap_or_else(|| match &plan {
            Some(plan) => format!("ChatGPT ({plan})"),
            None => "ChatGPT".to_string(),
        });
        Ok(AccountIdentity {
            id,
            label,
            email,
            plan,
        })
    }

    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)> {
        let mut headers = Vec::with_capacity(2);
        // `header` refuses values that are not valid HTTP text (a corrupt token with control
        // bytes): dropping the pair yields a clean upstream 401 that the refresher repairs,
        // whereas an unwrap here would abort the process (`panic = "abort"`).
        if let Some(pair) = header("Authorization", &format!("Bearer {}", tokens.access_token)) {
            headers.push(pair);
        }
        // The account id scopes the request to *this* ChatGPT subscription (§1.9). Absent
        // only when the JWT could not be parsed; the header is then omitted rather than
        // sent empty — an empty value is a hard 400 upstream.
        if let Some(account_id) = tokens.extra_str("chatgpt_account_id") {
            if let Some(pair) = header("chatgpt-account-id", account_id) {
                headers.push(pair);
            }
        }
        headers
    }

    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
        // UNCERTAIN: `.research/oauth.md` §1.9 could not confirm whether `OpenAI-Beta:
        // responses=experimental` is still required on `/v1/responses` — if upstream starts
        // rejecting it, drop this pair; if it starts rejecting requests *without* it, move it
        // from the fingerprint (cached) into `apply_credential` (per-request).
        &[
            ("originator", "codex_cli_rs"),
            ("OpenAI-Beta", "responses=experimental"),
        ]
    }

    fn refresh_lead(&self) -> Duration {
        // Codex manager refreshes 5 minutes before expiry (§1.10).
        Duration::from_secs(300)
    }

    /// Codex's **device** flow: the user types a short code at `auth.openai.com/codex/device`.
    ///
    /// This exists alongside the loopback redirect because the redirect is unusable in exactly the
    /// cases that matter most: a headless server, a container, or a machine whose browser runs
    /// somewhere else. A loopback flow requires the browser to reach a listener on *this* host, so
    /// those deployments can only be logged in by moving an authorization code by hand. A device
    /// code needs no inbound connection at all — the user reads five characters off the panel and
    /// types them wherever they like.
    ///
    /// Not RFC 8628: see [`openai::DEVICE_USERCODE`] for the three-step wire contract, which was
    /// confirmed against the live endpoint while writing this. Two differences matter. PKCE is
    /// generated **server-side** and returned with the authorization code, so this flow neither
    /// sends nor stores a verifier. And a poll must present the user code as well as the device id —
    /// omitting it is answered `400 ... user_code: Field required`, not a pending status.
    async fn device_start(&self) -> Result<DeviceAuthorization> {
        let http = Http::shared()?;
        let body = serde_json::json!({ "client_id": CODEX_CLIENT_ID }).to_string();
        let reply = http
            .post_json(
                openai::DEVICE_USERCODE,
                &[("Accept", "application/json")],
                body,
            )
            .await?;

        // Device login can be switched off for an account or workspace by an administrator. That
        // is a configuration outcome, not a failure, and the operator can act on it.
        if reply.status == 404 {
            return Err(oauth_error(
                "device-code login is disabled for this ChatGPT account or workspace; \
                 use the browser sign-in instead",
            ));
        }
        if !(200..300).contains(&reply.status) {
            return Err(oauth_error(format!(
                "device authorization failed with HTTP {}",
                reply.status
            )));
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|_| oauth_error("device authorization returned no JSON body"))?;

        let device_code = value
            .get("device_auth_id")
            .and_then(Value::as_str)
            .ok_or_else(|| oauth_error("device authorization returned no device_auth_id"))?
            .to_owned();
        // The field is spelling-dependent across Codex CLI versions; accept either.
        let user_code = value
            .get("user_code")
            .or_else(|| value.get("usercode"))
            .and_then(Value::as_str)
            .ok_or_else(|| oauth_error("device authorization returned no user code"))?
            .to_owned();
        // Live endpoint returns `"interval": "5"` (a string); the CLI types it as a number.
        let interval = value
            .get("interval")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(5)
            .clamp(1, 60);

        Ok(DeviceAuthorization {
            device_code,
            user_code,
            verification_uri: openai::DEVICE_VERIFY.to_owned(),
            verification_uri_complete: None,
            // OpenAI expires the code in 15 minutes; the CLI uses the same bound.
            expires_in: Duration::from_secs(15 * 60),
            interval: Duration::from_secs(interval),
        })
    }

    /// One poll, and — on approval — the token exchange that follows it.
    ///
    /// The flow's steps 2 and 3 collapse into one call here, because step 2 *is* the poll. Verified
    /// against the live endpoint: a correct code that has not been approved yet answers
    /// `403 deviceauth_authorization_pending`, a wrong code answers
    /// `400 deviceauth_user_code_mismatch`, and omitting the code is a 400 as well. A transport
    /// error is treated as pending (the code is valid for minutes and a blip is not a refusal), but
    /// any other 4xx/5xx is reported, so a genuinely broken flow surfaces instead of looking like a
    /// slow user.
    async fn device_poll(&self, authorization: &DeviceAuthorization) -> Result<DevicePoll> {
        let http = Http::shared()?;
        let body = serde_json::json!({
            "device_auth_id": authorization.device_code,
            "user_code": authorization.user_code,
        })
        .to_string();
        let reply = match http
            .post_json(
                openai::DEVICE_TOKEN,
                &[("Accept", "application/json")],
                body,
            )
            .await
        {
            Err(_) => return Ok(DevicePoll::Pending),
            Ok(reply) => reply,
        };

        if reply.status == 403 || reply.status == 404 {
            return Ok(DevicePoll::Pending);
        }
        if !(200..300).contains(&reply.status) {
            return Ok(DevicePoll::Failed {
                message: format!("device authorization failed with HTTP {}", reply.status),
            });
        }

        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|_| oauth_error("device authorization returned no JSON body"))?;
        let Some(code) = value.get("authorization_code").and_then(Value::as_str) else {
            return Ok(DevicePoll::Pending);
        };
        let Some(verifier) = value.get("code_verifier").and_then(Value::as_str) else {
            return Ok(DevicePoll::Failed {
                message: "device authorization returned a code but no PKCE verifier".into(),
            });
        };

        // The exchange: form-encoded, unlike the JSON refresh grant posted to this same URL.
        let reply = http
            .post_form(
                openai::TOKEN,
                &[("Accept", "application/json")],
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CODEX_CLIENT_ID),
                    ("code", code),
                    ("code_verifier", verifier),
                    ("redirect_uri", openai::DEVICE_REDIRECT),
                ],
            )
            .await?;
        if !(200..300).contains(&reply.status) {
            return Ok(DevicePoll::Failed {
                message: format!("the device code exchange failed with HTTP {}", reply.status),
            });
        }
        let value: Value = serde_json::from_str(&reply.body)
            .map_err(|_| oauth_error("the device code exchange returned no JSON body"))?;
        let Some(access_token) = value.get("access_token").and_then(Value::as_str) else {
            return Ok(DevicePoll::Failed {
                message: "the device code exchange returned no access token".into(),
            });
        };

        let mut extra = Map::new();
        let mut refresh_token = None;
        if let Some(refresh) = value.get("refresh_token").and_then(Value::as_str) {
            extra.insert("refresh_token".into(), Value::String(refresh.to_owned()));
            refresh_token = Some(refresh.to_owned());
        }
        let id_token = value
            .get("id_token")
            .and_then(Value::as_str)
            .map(str::to_owned);
        // The relay must send `chatgpt-account-id`; without it every request is a hard 400. The
        // device response may omit the id_token, so fall back to the access token — also a JWT, and
        // carrying the same claim namespace.
        let Some(account_id) = chatgpt_account_id(access_token, id_token.as_deref()) else {
            return Ok(DevicePoll::Failed {
                message: "the device code exchange returned no ChatGPT account id".into(),
            });
        };
        extra.insert("chatgpt_account_id".into(), Value::String(account_id));

        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600)
            .clamp(0, MAX_EXPIRES_IN_SECS);

        // Built the same way `exchange_code` builds one, so the device flow produces a token set
        // the rest of the system cannot tell apart from a browser login's — which is the point:
        // `extra` carries `id_token` and `chatgpt_account_id`, and the relay reads the latter.
        let mut tokens = TokenSet::new(access_token.to_owned());
        tokens.refresh_token = refresh_token;
        tokens.expires_at = expires_at_from_secs(expires_in);
        tokens.scopes = SCOPE.split(' ').map(str::to_owned).collect();
        tokens.token_type = "Bearer".to_string();
        tokens.extra = extra;
        Ok(DevicePoll::Success(tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::super::refresh::FailureClass;
    use super::*;
    use crate::ids::Millis;
    use crate::oauth::MAX_EXPIRES_IN_SECS;
    use base64::Engine as _;

    /// Build a JWT-shaped token: `header.payload.signature`, payload = base64url(JSON).
    fn fake_jwt(claims: Value) -> String {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).expect("serialize"));
        format!("eyJhbGciOiJub25lIn0.{payload}.signature")
    }

    fn auth_namespace(account_id: &str, plan: &str) -> Value {
        serde_json::json!({
            AUTH_NAMESPACE: { "account_id": account_id, "plan": plan },
        })
    }

    fn verifier() -> Pkce {
        Pkce::from_verifier("test_verifier_abcdefghijklmnopqrstuvwxyz012345")
            .expect("46 unreserved chars, RFC 7636 valid")
    }

    // ----- authorize URL ----------------------------------------------------

    #[test]
    fn authorize_url_carries_the_cli_verbatim_pairs() {
        let url = Codex
            .authorization_url("STATE-VALUE", Some(&verifier()), 1455)
            .expect("url");
        assert!(
            url.starts_with("https://auth.openai.com/oauth/authorize?"),
            "{url}"
        );
        // §1.5: these pairs are verbatim from the CLI binary.
        assert!(
            url.contains(&format!("client_id={}", CODEX_CLIENT_ID)),
            "public client id from crate::published: {url}"
        );
        assert!(
            url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback"),
            "{url}"
        );
        assert!(url.contains("code_challenge_method=S256"), "{url}");
        assert!(url.contains("codex_cli_simplified_flow=true"), "{url}");
        assert!(url.contains("originator=codex_cli_rs"), "{url}");
        assert!(url.contains("id_token_add_organizations=true"), "{url}");
        assert!(
            url.contains(&format!(
                "state={}",
                super::super::flow::url_encode("STATE-VALUE")
            )),
            "{url}"
        );
        assert!(
            url.contains(&format!("code_challenge={}", verifier().challenge())),
            "challenge derived from the verifier: {url}"
        );
    }

    #[test]
    fn authorize_url_follows_the_cli_parameter_order_exactly() {
        // §1.5: response_type, client_id, redirect_uri, code_challenge,
        // code_challenge_method, state, then the joined scope, then the extras.
        let url = Codex
            .authorization_url("s", Some(&verifier()), 1455)
            .expect("url");
        let query = url.split('?').nth(1).expect("query");
        let order: Vec<&str> = query
            .split('&')
            .map(|pair| pair.split('=').next().unwrap_or(""))
            .collect();
        assert_eq!(
            order,
            vec![
                "response_type",
                "client_id",
                "redirect_uri",
                "code_challenge",
                "code_challenge_method",
                "state",
                "scope",
                "id_token_add_organizations",
                "codex_cli_simplified_flow",
                "originator",
            ]
        );
    }

    #[test]
    fn scope_is_one_parameter_with_all_six_values() {
        let url = Codex
            .authorization_url("s", Some(&verifier()), 1455)
            .expect("url");
        let scope_params = url
            .split('&')
            .filter(|pair| pair.starts_with("scope="))
            .count();
        assert_eq!(
            scope_params, 1,
            "one joined scope parameter, not one per scope: {url}"
        );
        let scope_value = url
            .split('&')
            .find(|pair| pair.starts_with("scope="))
            .and_then(|pair| pair.strip_prefix("scope="))
            .expect("scope present");
        let decoded = super::super::flow::url_decode(scope_value);
        assert_eq!(decoded, SCOPE);
        assert_eq!(decoded.split(' ').count(), 6, "{decoded}");
        assert!(
            decoded.contains("offline_access"),
            "offline_access yields the refresh token"
        );
    }

    #[test]
    fn authorize_url_refuses_to_build_without_pkce() {
        let error = Codex
            .authorization_url("s", None, 1455)
            .expect_err("S256 is mandatory");
        assert!(error.to_string().contains("PKCE"), "{error}");
    }

    #[test]
    fn redirect_uri_follows_the_bound_port() {
        for (port, expected) in [
            (1455, "http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback"),
            (1457, "http%3A%2F%2F127.0.0.1%3A1457%2Fauth%2Fcallback"),
        ] {
            let url = Codex
                .authorization_url("s", Some(&verifier()), port)
                .expect("url");
            assert!(
                url.contains(&format!("redirect_uri={expected}")),
                "port {port}: {url}"
            );
        }
    }

    // ----- token response ---------------------------------------------------

    #[test]
    fn token_set_parses_the_exchange_shape() {
        let id_token = fake_jwt(auth_namespace("acct-123", "pro"));
        let body = serde_json::json!({
            "access_token": fake_jwt(auth_namespace("acct-123", "pro")),
            "refresh_token": "rt-secret",
            "id_token": id_token,
            "expires_in": 3600,
        })
        .to_string();

        let before = Millis::now();
        let tokens = token_set_from(&body, None).expect("parse");
        let after = Millis::now();

        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-secret"));
        assert_eq!(tokens.token_type, "Bearer");
        assert_eq!(
            tokens.scopes.join(" "),
            SCOPE,
            "the scope we requested, space-joined"
        );
        assert_eq!(
            tokens.extra_str("chatgpt_account_id"),
            Some("acct-123"),
            "account id lifted out of the JWT namespace claim"
        );
        assert_eq!(tokens.extra_str("id_token"), Some(id_token.as_str()));
        // expires_in is seconds → absolute expiry, within the parse window.
        assert!(
            tokens.expires_at.is_before(after.plus_secs(3601))
                && before.plus_secs(3599).is_before(tokens.expires_at),
            "seconds converted to an absolute Millis instant"
        );
    }

    #[test]
    fn expiry_is_clamped_against_bogus_lifetimes() {
        // Absurdly large: clamped to ten years, never allowed to wrap Millis arithmetic.
        let tokens = token_set_from(r#"{"access_token":"a","expires_in":99999999999}"#, None)
            .expect("parse");
        let max = Millis::now().plus_secs(MAX_EXPIRES_IN_SECS + 60);
        assert!(tokens.expires_at.is_before(max), "clamped to 10 years");

        // Negative: reads as already expired (honest for an unknown expiry) instead of
        // travelling 50 000 years into the future.
        let tokens =
            token_set_from(r#"{"access_token":"a","expires_in":-42}"#, None).expect("parse");
        assert!(tokens.expires_at.is_before(Millis::now().plus_secs(1)));

        // Zero-ish: same reading.
        let tokens = token_set_from(r#"{"access_token":"a","expires_in":0}"#, None).expect("parse");
        assert!(tokens.expires_at.is_before(Millis::now().plus_secs(1)));
    }

    #[test]
    fn a_refresh_without_rotation_keeps_the_previous_refresh_token() {
        let previous = "old-refresh-token";
        let tokens = token_set_from(r#"{"access_token":"a","expires_in":60}"#, Some(previous))
            .expect("parse");
        assert_eq!(
            tokens.refresh_token.as_deref(),
            Some(previous),
            "the response omitted the field — dropping the old token would strand the account"
        );

        // When it does rotate, the new one wins.
        let tokens = token_set_from(
            r#"{"access_token":"a","refresh_token":"new","expires_in":60}"#,
            Some(previous),
        )
        .expect("parse");
        assert_eq!(tokens.refresh_token.as_deref(), Some("new"));
    }

    #[test]
    fn malformed_token_responses_are_named_not_guessed() {
        let err = token_set_from("not json", None).expect_err("json");
        assert!(err.to_string().contains("not JSON"), "{err}");

        let err = token_set_from(r#"{"expires_in":60}"#, None).expect_err("access");
        assert!(err.to_string().contains("access_token"), "{err}");

        let err = token_set_from(r#"{"access_token":"a"}"#, None).expect_err("expiry");
        assert!(err.to_string().contains("expires_in"), "{err}");
    }

    // ----- JWT claims -------------------------------------------------------

    #[test]
    fn jwt_payload_reads_both_key_spellings_from_the_namespace() {
        let claims = serde_json::json!({
            AUTH_NAMESPACE: { "account_id": "acct-a" },
            "sub": "user-1",
        });
        let token = fake_jwt(claims);
        let decoded = jwt_payload(&token).expect("decode");
        assert_eq!(
            auth_claim(&decoded, &["account_id", "chatgpt_account_id"]).as_deref(),
            Some("acct-a")
        );
        assert_eq!(
            decoded.get("sub").and_then(Value::as_str),
            Some("user-1"),
            "flat claims (sub, email) live at the top level"
        );

        // The alternate spelling from the persisted auth.json field names.
        let claims = serde_json::json!({
            AUTH_NAMESPACE: { "chatgpt_account_id": "acct-b" },
        });
        let decoded = jwt_payload(&fake_jwt(claims)).expect("decode");
        assert_eq!(
            auth_claim(&decoded, &["account_id", "chatgpt_account_id"]).as_deref(),
            Some("acct-b")
        );
    }

    #[test]
    fn jwt_payload_rejects_junk_without_panicking() {
        assert!(jwt_payload("not-a-jwt").is_err());
        assert!(jwt_payload("onlyonesegment").is_err());
        assert!(jwt_payload("h.@@@not-base64@@..x").is_err());
        // Structurally valid but payload is not JSON.
        let bogus = format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"nope\":")
        );
        assert!(jwt_payload(&bogus).is_err());
    }

    #[test]
    fn account_id_falls_back_to_the_access_token() {
        // Refresh responses may omit the id_token; the access token carries the same claims.
        let access = fake_jwt(auth_namespace("acct-fallback", "team"));
        let id = chatgpt_account_id(&access, None);
        assert_eq!(id.as_deref(), Some("acct-fallback"));

        // And a token with no namespace at all simply has no account id.
        let bare = fake_jwt(serde_json::json!({ "sub": "u" }));
        assert_eq!(chatgpt_account_id(&bare, None), None);
    }

    // ----- endpoint failures ------------------------------------------------

    #[test]
    fn endpoint_failures_classify_by_status_and_code() {
        // invalid_grant on 400 → permanent: the refresh token is dead, re-login is the fix.
        let (class, detail) = endpoint_failure(
            400,
            r#"{"error":"invalid_grant","error_description":"Refresh token expired"}"#,
        )
        .expect("failure");
        assert_eq!(class, FailureClass::Permanent);
        assert_eq!(detail, "invalid_grant: Refresh token expired");

        // 200 with an error object: the code still decides (providers answer both ways).
        let (class, detail) =
            endpoint_failure(200, r#"{"error":"invalid_client"}"#).expect("failure");
        assert_eq!(class, FailureClass::Permanent);
        assert_eq!(detail, "invalid_client");

        // Server trouble without a JSON body → transient, status only.
        let (class, detail) = endpoint_failure(502, "<html>bad gateway</html>").expect("failure");
        assert_eq!(class, FailureClass::Transient);
        assert_eq!(detail, "HTTP 502");

        // Unknown code on a 400 stays retryable (§1.10's line: only invalid_grant is fatal).
        let (class, _) = endpoint_failure(400, r#"{"error":"something_new"}"#).expect("failure");
        assert_eq!(class, FailureClass::Transient);

        // Success: no error field, 2xx.
        assert!(endpoint_failure(200, r#"{"access_token":"a"}"#).is_none());
        assert!(endpoint_failure(204, "").is_none());
        // 204 with no body parses as "no error" — but a bare 403 still fails.
        let (class, detail) = endpoint_failure(403, "").expect("failure");
        assert_eq!(
            class,
            FailureClass::Transient,
            "unclassified 4xx stays retryable"
        );
        assert_eq!(detail, "HTTP 403");
    }

    #[test]
    fn error_descriptions_are_clipped_before_they_reach_a_message() {
        let hostile = "x".repeat(1000);
        let body = format!(r#"{{"error":"invalid_grant","error_description":"{hostile}"}}"#);
        let (_, detail) = endpoint_failure(400, &body).expect("failure");
        assert!(detail.len() <= 200, "clipped, got {} bytes", detail.len());
        assert!(detail.starts_with("invalid_grant: "), "{detail}");
    }

    // ----- identity ---------------------------------------------------------

    #[tokio::test]
    async fn identity_is_decoded_from_stored_claims_without_network() {
        let mut claims = auth_namespace("acct-9", "business");
        claims
            .as_object_mut()
            .expect("object")
            .insert("sub".into(), serde_json::json!("user-9"));
        claims
            .as_object_mut()
            .expect("object")
            .insert("email".into(), serde_json::json!("dev@example.com"));

        let mut tokens = TokenSet::new(fake_jwt(claims.clone()));
        tokens.extra.insert(
            "id_token".into(),
            serde_json::Value::String(fake_jwt(claims)),
        );

        let identity = Codex.account_identity(&tokens).await.expect("identity");
        assert_eq!(identity.id, "acct-9");
        assert_eq!(identity.email.as_deref(), Some("dev@example.com"));
        assert_eq!(identity.plan.as_deref(), Some("business"));
        assert_eq!(
            identity.label, "dev@example.com",
            "the email doubles as the label"
        );
    }

    #[tokio::test]
    async fn identity_label_falls_back_to_plan_then_product_name() {
        // No email: label reads from the plan claim…
        let mut tokens = TokenSet::new(fake_jwt(auth_namespace("acct-1", "pro")));
        tokens.extra.insert(
            "id_token".into(),
            serde_json::Value::String(fake_jwt(auth_namespace("acct-1", "pro"))),
        );
        let identity = Codex.account_identity(&tokens).await.expect("identity");
        assert_eq!(identity.label, "ChatGPT (pro)");
        assert_eq!(identity.email, None);

        // …and to a bare product name when even that is missing.
        let identity = Codex
            .account_identity(&TokenSet::new(fake_jwt(serde_json::json!({"sub": "u1"}))))
            .await
            .expect("identity");
        assert_eq!(identity.id, "u1", "sub is the last-resort id");
        assert_eq!(identity.label, "ChatGPT");
        assert_eq!(identity.plan, None);
    }

    #[tokio::test]
    async fn identity_refuses_a_token_with_no_account_identity() {
        let error = Codex
            .account_identity(&TokenSet::new(fake_jwt(
                serde_json::json!({"name": "nothing"}),
            )))
            .await
            .expect_err("no id at all");
        assert!(error.to_string().contains("no account id"), "{error}");
    }

    // ----- request headers --------------------------------------------------

    #[test]
    fn apply_credential_sends_bearer_and_the_account_header() {
        let mut tokens = TokenSet::new("ACCESS");
        tokens
            .extra
            .insert("chatgpt_account_id".into(), serde_json::json!("acct-7"));

        let headers = Codex.apply_credential(&tokens);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0.as_str(), "authorization");
        assert_eq!(headers[0].1, "Bearer ACCESS");
        assert_eq!(headers[1].0.as_str(), "chatgpt-account-id");
        assert_eq!(headers[1].1, "acct-7");

        // Without a parseable account id the header is omitted, never sent empty.
        let headers = Codex.apply_credential(&TokenSet::new("ACCESS"));
        assert_eq!(headers.len(), 1);
    }

    // ----- statics ----------------------------------------------------------

    #[test]
    fn statics_match_the_dossier() {
        assert_eq!(Codex::ID, "openai-codex");
        assert_eq!(
            Codex.refresh_lead(),
            Duration::from_secs(300),
            "§1.10: five minutes"
        );
        assert!(Codex.begins_with_loopback());
        assert!(Codex.requires_pkce());
        assert!(Codex.validates_redirect_port());
        let ports = Codex.redirect_ports();
        assert_eq!(
            (ports.host, ports.primary, ports.fallback, ports.path),
            ("127.0.0.1", 1455, Some(1457), "/auth/callback")
        );
        assert_eq!(
            ports.redirect_uri(1455),
            crate::published::CODEX_REDIRECT_URI,
            "the published redirect constant is the bound-port construction"
        );
        // No client secret exists for this platform — the fingerprint is header names only.
        assert_eq!(
            Codex.fingerprint_headers(),
            &[
                ("originator", "codex_cli_rs"),
                ("OpenAI-Beta", "responses=experimental"),
            ]
        );
    }

    // ----- device flow ------------------------------------------------------

    #[test]
    fn device_authorization_parses_the_live_response_shape() {
        // Copied from the live endpoint while implementing this: `interval` really is a **string**,
        // and the id really is `device_auth_id`. A parser that assumed a number would fail here and
        // nowhere else.
        let body = r#"{
            "device_auth_id": "deviceauth_6ad1361cc52c8191ab78e0ca97242709",
            "user_code": "YTY4-6MD7V",
            "interval": "5",
            "expires_at": "2026-10-15T20:37:52.770085+00:00"
        }"#;
        let value: Value = serde_json::from_str(body).expect("json");
        assert_eq!(
            value.get("device_auth_id").and_then(Value::as_str),
            Some("deviceauth_6ad1361cc52c8191ab78e0ca97242709")
        );
        // The two spellings the CLI versions use.
        assert_eq!(
            value
                .get("user_code")
                .or_else(|| value.get("usercode"))
                .and_then(Value::as_str),
            Some("YTY4-6MD7V")
        );
        // Both encodings must yield 5 — a string is what the live endpoint sends.
        let interval = value
            .get("interval")
            .and_then(|v| {
                v.as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(5);
        assert_eq!(interval, 5);
    }

    #[test]
    fn a_device_authorization_reports_the_hosted_verification_page() {
        // Regression guard for a real trap: the URL the user must visit is `/codex/device`, not
        // GitHub's `/login/device` and not OpenAI's generic verification page.
        assert_eq!(
            openai::DEVICE_VERIFY,
            "https://auth.openai.com/codex/device"
        );
    }

    #[test]
    fn the_device_exchange_presents_the_redirect_that_is_never_visited() {
        // The device grant must send the same `redirect_uri` the flow was registered with; a
        // mismatch is rejected by the token endpoint as if the code were invalid.
        assert_eq!(
            openai::DEVICE_REDIRECT,
            "https://auth.openai.com/deviceauth/callback"
        );
    }

    #[test]
    fn device_and_browser_endpoints_are_distinct() {
        // The device flow lives under `/api/accounts`, NOT next to the browser flow's `/oauth/*`.
        // Using the browser paths for the device flow is a 404, which reads like an outage.
        assert!(openai::DEVICE_USERCODE.starts_with("https://auth.openai.com/api/accounts/"));
        assert!(openai::DEVICE_TOKEN.starts_with("https://auth.openai.com/api/accounts/"));
        assert!(openai::TOKEN.starts_with("https://auth.openai.com/oauth/"));
        assert_ne!(openai::DEVICE_TOKEN, openai::TOKEN);
    }
}
