//! OAuth: the login flows that turn a provider account into live request headers.
//!
//! `catalog/providers.toml` declares *which* providers need OAuth — `auth = { type = "oauth",
//! platform = "…" }` — and this module supplies the implementations those ids resolve to:
//!
//! | platform id        | implementation       | flow                              |
//! |--------------------|----------------------|-----------------------------------|
//! | `openai-codex`     | [`codex::Codex`]     | loopback authorization code + PKCE (S256) |
//! | `github-copilot`   | [`copilot::Copilot`] | GitHub device code                |
//! | `antigravity`      | not registered       | protocol implementation incomplete (Google) |
//! | `cline`            | not registered       | no verified interactive flow      |
//!
//! ## Rules this module exists to enforce
//!
//! * **Client identity comes from [`crate::published`].** The Google client id/secret pair is a
//!   secret-scanning target (push protection and `scripts/check-secrets.sh`), so no Google
//!   credential is ever written as a literal in this directory — the published constants are the
//!   single source of truth, and they are public installed-application credentials by design.
//! * **Nothing secret is ever logged.** [`TokenSet`], [`Pkce`] and [`DeviceAuthorization`] carry
//!   hand-written `Debug` impls that print shapes (lengths, key names, public challenges) instead
//!   of values: a token, authorization code, PKCE verifier, client secret or `Authorization`
//!   header must not appear in any log line, error message or panel payload.
//! * **No cookie or browser-session auth** (decision Q6): a login ends with a credential in the
//!   vault, never in a session cookie the data plane could be tricked into accepting.
//! * **No panics on a flow path.** `panic = "abort"` in this workspace, so an `unwrap` in code
//!   that runs while a human is watching a browser would take the whole process down. Every
//!   failure is an `Err`; hand-rolled `Debug` impls exist precisely so that formatting a value
//!   cannot panic either.
//! * **Uncertainty is marked.** Anything taken from a reverse-engineered client carries a
//!   `// UNCERTAIN:` comment saying what breaks if it turns out to be wrong.
//!
//! ## Storage
//!
//! [`crate::store`] is still a stub ("store not yet implemented"), so durable credential storage
//! for OAuth accounts is the secret [`crate::secret::Vault`]: [`refresh::CredentialStore`] is the
//! narrow trait that wraps it, keyed by the account's `oauth_ref`. The relay and conductor own
//! account mutation; this module only reads and writes credential material.

pub mod antigravity;
pub mod codex;
pub mod copilot;
pub mod flow;
pub mod refresh;
pub mod session;

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use http::{HeaderName, HeaderValue};
use rand::rngs::OsRng;
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};
use crate::ids::Millis;

/// An error that names this subsystem, so a bare `invalid state:` in a log is attributable to
/// a login flow rather than to unrelated state handling.
///
/// Errors that carry a refresh-failure classification go through
/// [`refresh::oauth_failure`] instead, which embeds a machine-readable class marker.
pub(crate) fn oauth_error(detail: impl fmt::Display) -> Error {
    Error::InvalidState(format!("oauth: {detail}"))
}

/// Build one `(name, value)` header pair, returning `None` instead of panicking when a value is
/// not valid HTTP header text.
///
/// `HeaderValue` refuses control bytes, which a corrupt token could contain. Dropping the pair
/// would turn into a clean upstream 401 that the refresher can repair, whereas unwrapping here
/// would abort the process — so the value's bytes are never printed, only its header name.
pub(crate) fn header(name: &'static str, value: &str) -> Option<(HeaderName, HeaderValue)> {
    let parsed_name = HeaderName::from_bytes(name.as_bytes()).ok()?;
    match HeaderValue::from_str(value) {
        Ok(value) => Some((parsed_name, value)),
        Err(_) => {
            tracing::warn!(
                header = name,
                "dropping OAuth header whose value is not valid HTTP header text"
            );
            None
        }
    }
}

/// Clamp for a token endpoint's `expires_in` (seconds): a provider reporting a negative or
/// absurd lifetime must not wrap `Millis` arithmetic. Zero reads as "already expired"
/// (refresh — the honest reading of an unknown expiry), ten years as the longest plausible
/// refresh interval.
pub(crate) const MAX_EXPIRES_IN_SECS: i64 = 10 * 365 * 24 * 3600;

/// Absolute expiry for a token whose endpoint reported `expires_in` **seconds** — clamped,
/// and computed in one place so no platform can mistake seconds for milliseconds (a mix-up
/// puts expiry 50 000 years out and the token gets used long after it died).
pub(crate) fn expires_at_from_secs(expires_in: i64) -> Millis {
    Millis::now().plus_secs(expires_in.clamp(0, MAX_EXPIRES_IN_SECS))
}

/// Decode a JWT payload for its *claims only* — signature verification is deliberately not
/// done here.
///
/// Why that is safe in these flows: each token arrives over TLS from the provider's own
/// token endpoint in the very request/response being parsed, and the claims are used only as
/// metadata (the account-id header, the panel's identity line) — never to authorise
/// anything. Verifying would require fetching the provider's JWKS, pinning algorithms and
/// checking clocks; a failure there would lock users out of a login that actually succeeded.
/// If such a token is ever accepted from any other source, this function must gain real
/// verification before it is reused.
pub(crate) fn jwt_payload(token: &str) -> Result<serde_json::Map<String, serde_json::Value>> {
    use base64::Engine as _;

    let payload = token
        .split('.')
        .nth(1)
        .ok_or_else(|| oauth_error("expected a JWT (header.payload.signature)"))?;
    // JWT segments are base64url without padding; some stacks add padding anyway, so accept
    // both spellings before giving up.
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| oauth_error("JWT payload is not base64url"))?;
    serde_json::from_slice(&decoded).map_err(|_| oauth_error("JWT payload is not JSON"))
}

/// A set of OAuth tokens as stored and as returned by a token endpoint.
///
/// Unknown response keys are preserved in [`extra`](TokenSet::extra) (flattened, so they survive
/// a serde round trip verbatim): providers piggyback durable facts on the token response —
/// Codex's `id_token`, Copilot's `sku`/`endpoints`, Antigravity's project packing — and dropping
/// a key the schema does not know about would silently lose state that later steps need.
///
/// `expires_at` is milliseconds since the epoch ([`Millis`]), never seconds: every provider
/// answers in its own unit, and the conversion happens once, at parse time, in the platform that
/// owns the response.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSet {
    /// The bearer presented upstream. Short-lived (minutes to hours).
    pub access_token: String,
    /// The long-lived credential that mints a new access token, when the flow produced one.
    ///
    /// Copilot is the exception: its session token cannot be refreshed through the OAuth grant,
    /// so this holds the long-lived *GitHub* token and refresh re-mints.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Absolute expiry as milliseconds since the Unix epoch.
    ///
    /// Zero means "unknown" and is treated as already expired — refreshing a valid token is
    /// cheap, assuming an expired one is valid would send dead credentials upstream.
    #[serde(default = "default_expires_at")]
    pub expires_at: Millis,
    /// Scopes the grant actually covered, if the provider reported them.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Token type, normally `Bearer`.
    #[serde(default = "default_token_type")]
    pub token_type: String,
    /// Every other key the token endpoint returned, preserved verbatim.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_token_type() -> String {
    "Bearer".to_string()
}

/// `Millis` has no `Default` (an unknown instant is an explicit zero, not a forgotten value).
fn default_expires_at() -> Millis {
    Millis(0)
}

impl TokenSet {
    /// A token set with an unknown expiry, to be filled in by the caller.
    pub fn new(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            refresh_token: None,
            expires_at: Millis(0),
            scopes: Vec::new(),
            token_type: default_token_type(),
            extra: Map::new(),
        }
    }

    /// Whether the access token must not be used at `now`.
    ///
    /// `>=` rather than `>`: one millisecond of clock skew in the direction of "still valid"
    /// produces an upstream 401, while refreshing a second early costs nothing.
    pub fn is_expired(&self, now: Millis) -> bool {
        now.as_millis() >= self.expires_at.as_millis()
    }

    /// A string-valued extra key, for facts like Copilot's `endpoints.api`.
    pub fn extra_str(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }
}

/// Hand-written so that formatting a token set — in a log, an error, a `Result` surfaced by the
/// panel — prints shapes and never values. Values live only in the vault; the lengths stay so
/// that "empty token" and "long token" remain distinguishable when debugging a flow.
impl fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("TokenSet");
        debug.field(
            "access_token",
            &format_args!("***({} bytes)", self.access_token.len()),
        );
        match &self.refresh_token {
            Some(token) => {
                debug.field("refresh_token", &format_args!("***({} bytes)", token.len()));
            }
            None => {
                debug.field("refresh_token", &None::<&str>);
            }
        }
        // Keys are not secrets — values are.
        let extra_keys: Vec<&str> = self.extra.keys().map(String::as_str).collect();
        debug
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .field("token_type", &self.token_type)
            .field("extra_keys", &extra_keys)
            .finish()
    }
}

/// Who a set of tokens belongs to, for multi-account identity (decision: re-authenticating the
/// same identity replaces the account in place rather than creating a second one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    /// Stable subject id at the provider (Codex's `chatgpt_account_id`, GitHub's numeric user id,
    /// Antigravity's Google account id).
    pub id: String,
    /// Display name for the panel; falls back to the id when the provider gives nothing better.
    pub label: String,
    /// Email, when the provider releases one to this scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Plan or tier, when the token response or identity endpoint reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

/// A PKCE (RFC 7636) verifier/challenge pair for one login.
///
/// The verifier is a secret until the code exchange completes; the challenge is public from the
/// moment the browser opens, which is exactly the point: an attacker who observes the
/// authorization URL still cannot redeem a stolen code without the verifier.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    /// Generate a fresh verifier from the CSPRNG and derive its S256 challenge.
    ///
    /// 32 random bytes → base64url **without padding** → 43 characters, comfortably inside
    /// RFC 7636 §4.1's 43–128 range and avoiding the `+/=` trap (see [`Pkce::from_verifier`]).
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        OsRng.fill_bytes(&mut bytes);
        let verifier = URL_SAFE_NO_PAD.encode(bytes);
        let challenge = Self::s256(&verifier);
        Self {
            verifier,
            challenge,
        }
    }

    /// Wrap an existing verifier, validating it against RFC 7636 §4.1: 43–128 characters from
    /// the unreserved set `[A-Za-z0-9-._~]`.
    pub fn from_verifier(verifier: impl Into<String>) -> Result<Self> {
        let verifier = verifier.into();
        let len = verifier.len();
        let ok = (43..=128).contains(&len)
            && verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
        if !ok {
            return Err(oauth_error(format!(
                "pkce verifier rejected: RFC 7636 requires 43-128 unreserved characters \
                 (A-Za-z0-9-._~), got {len} bytes"
            )));
        }
        let challenge = Self::s256(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }

    /// The base64url (no padding) SHA-256 of `verifier` — the `code_challenge`.
    ///
    /// The alphabet matters twice over: URL-safe so it survives the query string, unpadded so
    /// the `=` a `STANDARD` encoder would append cannot be stripped or re-encoded by a browser,
    /// an intermediary or the provider's own parser.
    pub fn s256(verifier: &str) -> String {
        let digest = Sha256::digest(verifier.as_bytes());
        URL_SAFE_NO_PAD.encode(digest)
    }

    /// The secret half, sent as `code_verifier` on the token exchange.
    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    /// The public half, sent as `code_challenge` on the authorize URL.
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// The challenge is already public (it goes into the browser URL); the verifier never is.
impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field(
                "verifier",
                &format_args!("***({} chars)", self.verifier.len()),
            )
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// The loopback ports and redirect path a browser-based login accepts, plus the host the
/// redirect URI is spelled with.
///
/// The host field exists because providers can register `127.0.0.1` or `localhost`; those are
/// not interchangeable once the URI is compared against a pre-registered value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedirectPorts {
    /// Host component of the redirect URI (`127.0.0.1` or `localhost`).
    pub host: &'static str,
    /// First port to bind: a registered port, or `0` for an ephemeral one.
    pub primary: u16,
    /// Second port to try when the primary is busy, if the platform allows one.
    pub fallback: Option<u16>,
    /// Path component, beginning with `/`.
    pub path: &'static str,
}

impl RedirectPorts {
    /// Ports in bind order: primary first, then the fallback.
    pub fn candidates(&self) -> Vec<u16> {
        let mut ports = vec![self.primary];
        if let Some(fallback) = self.fallback {
            ports.push(fallback);
        }
        ports
    }

    /// The redirect URI for a port that is already bound.
    ///
    /// Built *after* the listener exists: publishing `…:1455/…` while the fallback bound 1457
    /// would send the browser to a port nothing is listening on.
    pub fn redirect_uri(&self, port: u16) -> String {
        format!("http://{}:{}{}", self.host, port, self.path)
    }
}

/// A device-code ("poll until the human approves") authorization, as issued by the device
/// endpoint.
#[derive(Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    /// The opaque code the poller presents; secret-ish, so [`Debug`] masks it.
    pub device_code: String,
    /// What the human types at `verification_uri`.
    pub user_code: String,
    /// Where the human types it.
    pub verification_uri: String,
    /// A deep link with the code already filled in, when the provider supplies one.
    pub verification_uri_complete: Option<String>,
    /// When the device code itself expires.
    pub expires_in: Duration,
    /// Minimum spacing between poll attempts, as the provider dictates.
    pub interval: Duration,
}

impl fmt::Debug for DeviceAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_struct("DeviceAuthorization");
        debug.field(
            "device_code",
            &format_args!("***({} bytes)", self.device_code.len()),
        );
        // The user code is meant to be read aloud; the URI is public.
        debug
            .field("user_code", &self.user_code)
            .field("verification_uri", &self.verification_uri)
            .field(
                "verification_uri_complete",
                &self.verification_uri_complete.as_deref(),
            )
            .field("expires_in", &self.expires_in)
            .field("interval", &self.interval)
            .finish()
    }
}

/// The outcome of one device-code poll, with each protocol outcome kept distinct.
///
/// The distinction is load-bearing: only [`AccessDenied`](DevicePoll::AccessDenied) is the
/// human refusing. `authorization_pending` and `slow_down` are *progress*; `expired_token` is a
/// deadline; anything else is a failure of the exchange itself. Collapsing them into "failed"
/// would either hide a refusal behind a retry loop or report a refusal as a transient error.
#[derive(Debug, Clone, PartialEq)]
pub enum DevicePoll {
    /// The human has not finished yet — poll again after the current interval.
    Pending,
    /// The provider asked us to slow down: back off by *increasing* the interval (RFC 8628).
    SlowDown,
    /// The human approved and the token came back.
    Success(TokenSet),
    /// The device code outlived its `expires_in`; restarting the whole flow is the only remedy.
    ExpiredToken,
    /// The human explicitly refused at the verification page.
    AccessDenied,
    /// The exchange failed for a reason that is none of the above (bad device code, server
    /// error). The message must be credential-free.
    Failed {
        /// Credential-free explanation for the panel.
        message: String,
    },
}

impl DevicePoll {
    /// Whether the poll should be repeated (the human is still working on it).
    pub const fn is_continue(&self) -> bool {
        matches!(self, Self::Pending | Self::SlowDown)
    }

    /// Whether the human refused — the only outcome the panel words as a denial.
    pub const fn is_refusal(&self) -> bool {
        matches!(self, Self::AccessDenied)
    }
}

/// One OAuth-capable provider: how to log in, how to refresh, and how the credential becomes
/// request headers.
///
/// The trait is dyn-compatible so the registry can hand out `&'static dyn OAuthPlatform`.
///
/// A `const ID: &'static str` *on the trait* would break exactly that: an associated const makes
/// the trait not dyn-compatible (`error[E0038]: … contains associated const 'ID'`, verified),
/// so identity is the [`id`](OAuthPlatform::id) method while each implementation type still
/// carries its own `pub const ID` for static use at call sites that know the type.
#[async_trait]
pub trait OAuthPlatform: Send + Sync {
    /// Stable machine id, matching `oauth_platform` in `catalog/providers.toml`.
    fn id(&self) -> &'static str;

    /// Short human label for the panel.
    fn label(&self) -> &'static str;

    /// One line explaining what this platform is, shown next to the label.
    fn description(&self) -> &'static str;

    /// Whether login starts by binding a loopback HTTP listener and opening the browser at a
    /// redirect it will receive; `false` means the device-code flow instead.
    fn begins_with_loopback(&self) -> bool;

    /// Where a browser-based login may send the user back. Device platforms return an inert
    /// value (`primary` 0, no path); check [`begins_with_loopback`](Self::begins_with_loopback)
    /// before reading it.
    fn redirect_ports(&self) -> RedirectPorts;

    /// Whether the flow uses PKCE (S256). The Copilot device flow has no redirect at all, hence no code to bind.
    fn requires_pkce(&self) -> bool;

    /// Whether the bound port must be checked against [`redirect_ports`](Self::redirect_ports)
    /// before the redirect URI is published: true for fixed/registered port sets, false for
    /// device platforms with no port whatsoever.
    fn validates_redirect_port(&self) -> bool;

    /// Build the browser authorize URL for a login whose listener is already bound to `port`.
    ///
    /// `pkce` is an `Option` because the platforms differ: Codex requires a challenge, Google's
    /// T2 client sends one, while a device-code platform has no redirect URI to protect.
    /// The redirect URI is derived from `port` here — after the bind — never from a constant,
    /// so the URL can only point at a listener that actually exists.
    fn authorization_url(&self, state: &str, pkce: Option<&Pkce>, port: u16) -> Result<String>;

    /// Redeem the authorization code the loopback listener received for a token set.
    async fn exchange_code(&self, code: &str, pkce: Option<&Pkce>, port: u16) -> Result<TokenSet>;

    /// Exchange a refresh token for a fresh token set.
    ///
    /// Refresh tokens are frequently **single-use**: a successful refresh invalidates the token
    /// that produced it, so the caller ([`refresh::Refresher`]) guarantees that only one refresh
    /// for a given credential is ever in flight, and persists the result before reporting
    /// success. Implementations must not persist on their own.
    async fn refresh(&self, refresh_token: &str) -> Result<TokenSet>;

    /// Resolve a token set to the human/account it belongs to (multi-account identity).
    async fn account_identity(&self, tokens: &TokenSet) -> Result<AccountIdentity>;

    /// Credential-derived request headers (the bearer plus provider-specific identity headers).
    ///
    /// Static fingerprint headers come from the catalogue and from
    /// [`fingerprint_headers`](Self::fingerprint_headers); this method returns only what the
    /// tokens themselves contribute.
    fn apply_credential(&self, tokens: &TokenSet) -> Vec<(HeaderName, HeaderValue)>;

    /// The exact static name/value pairs this client's upstream expects, for the golden-header
    /// test that compares them against the catalogue. Values that only make sense as templates
    /// (Copilot's `Editor-Version`) stay in the catalogue instead.
    fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)];

    /// How long before `expires_at` a proactive refresh should run.
    ///
    /// Codex refreshes at 5 minutes to its clock (`L = 8`-style window in codex's manager);
    /// Copilot's session lasts ~30 minutes and its own client refreshes 300 s early;
    /// Claude Code is
    /// documented at **4 hours** in `docs/10-oauth-overview.md §5` —
    /// `// UNCERTAIN:` that number appears in no primary source; if it is wrong, either a
    /// long-lived token expires in flight (one failed request, repaired by a forced refresh) or
    /// we refresh more often than necessary (harmless).
    fn refresh_lead(&self) -> Duration;

    /// Begin the device-code flow. Platforms that start with a loopback listener inherit this
    /// error, and vice versa for [`authorization_url`](Self::authorization_url).
    async fn device_start(&self) -> Result<DeviceAuthorization> {
        Err(oauth_error(format!(
            "{} does not use the device-code flow",
            self.id()
        )))
    }

    /// One poll of the device-code endpoint. Platforms that start with a loopback listener
    /// inherit this error.
    async fn device_poll(&self, device_code: &str) -> Result<DevicePoll> {
        let _ = device_code;
        Err(oauth_error(format!(
            "{} does not use the device-code flow",
            self.id()
        )))
    }
}

// The registry. Each platform type carries its own `pub const ID` (see the trait docs); the
// statics below exist so `platforms()` can hand out `&'static dyn OAuthPlatform`.
use codex::Codex;
use copilot::Copilot;

static CODEX: Codex = Codex;
static COPILOT: Copilot = Copilot;

/// Registration order for complete and supported implementations, matching `catalog/providers.toml`.
static PLATFORMS: [&'static dyn OAuthPlatform; 2] = [&CODEX, &COPILOT];

/// Every registered platform. Required to resolve every `AuthKind::OAuth` catalogue id — see
/// the tests at the bottom of this file, which check both directions.
pub fn platforms() -> &'static [&'static dyn OAuthPlatform] {
    &PLATFORMS
}

/// Look a platform up by its catalogue id.
pub fn platform(id: &str) -> Option<&'static dyn OAuthPlatform> {
    platforms().iter().copied().find(|p| p.id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::catalog;
    use crate::dialect::AuthKind;
    use serde_json::json;

    /// Providers whose catalogue note admits the platform is not implemented in this build.
    /// Each such entry must say so in its own note — an undocumented gap would silently route
    /// an operator into a login that cannot exist.
    const NOT_IMPLEMENTED: &[&str] = &["antigravity", "cline"];
    const NON_INTERACTIVE: &[&str] = &["xai"];

    fn oauth_platform_ids() -> Vec<String> {
        catalog()
            .all()
            .iter()
            .filter_map(|provider| match &provider.auth {
                AuthKind::OAuth { platform } => Some(platform.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn every_catalogue_oauth_platform_resolves() {
        let mut seen_unimplemented = Vec::new();
        for id in oauth_platform_ids() {
            if NON_INTERACTIVE.contains(&id.as_str()) {
                continue;
            }
            if NOT_IMPLEMENTED.contains(&id.as_str()) {
                // The exception is only legal while the catalogue admits it.
                let note = catalog()
                    .get(&id)
                    .and_then(|p| p.note.as_deref())
                    .unwrap_or("");
                assert!(
                    note.contains("does not implement") || note.contains("not implemented"),
                    "provider {id} is exempt from the OAuth registry but its note does not \
                     say the platform is unimplemented"
                );
                seen_unimplemented.push(id);
                continue;
            }
            assert!(
                platform(&id).is_some(),
                "catalogue provider declares auth type oauth with platform {id:?}, but no \
                 OAuthPlatform is registered under that id"
            );
        }
        // OAuth extras on otherwise API-key templates are not treated as OAuth registry entries.
        // The exception list must track the catalogue, not rot: a documented gap that got
        // implemented (or removed) has to be noticed.
        let catalogue_gaps: Vec<String> = oauth_platform_ids()
            .into_iter()
            .filter(|id| !NON_INTERACTIVE.contains(&id.as_str()))
            .filter(|id| platform(id).is_none())
            .collect();
        assert_eq!(
            catalogue_gaps, seen_unimplemented,
            "unimplemented OAuth platforms changed; update NOT_IMPLEMENTED"
        );
    }

    #[test]
    fn every_registered_platform_is_in_the_catalogue() {
        let ids = oauth_platform_ids();
        for registered in platforms() {
            assert!(
                ids.iter().any(|id| id == registered.id()),
                "platform {} is registered but no catalogue provider declares it",
                registered.id()
            );
        }
    }

    #[test]
    fn registry_ids_are_unique_and_nonempty() {
        for (i, a) in platforms().iter().enumerate() {
            assert!(!a.id().is_empty());
            for b in platforms().iter().skip(i + 1) {
                assert_ne!(a.id(), b.id(), "duplicate platform id");
            }
        }
    }

    #[test]
    fn platform_metadata_is_one_line_and_present() {
        for p in platforms() {
            assert!(!p.label().is_empty(), "{}: empty label", p.id());
            assert!(!p.description().is_empty(), "{}: empty description", p.id());
            assert!(
                !p.label().contains('\n') && !p.description().contains('\n'),
                "{}: label and description must be single lines",
                p.id()
            );
            assert!(p.refresh_lead() > Duration::ZERO, "{}: zero lead", p.id());
        }
    }

    #[test]
    fn refresh_leads_match_the_documented_windows() {
        assert_eq!(
            platform("openai-codex").unwrap().refresh_lead(),
            Duration::from_secs(300)
        );
        assert_eq!(
            platform("github-copilot").unwrap().refresh_lead(),
            Duration::from_secs(300)
        );
        // The incomplete Antigravity implementation is intentionally not registered.
        assert!(platform("antigravity").is_none());
    }

    #[test]
    fn fingerprint_headers_are_valid_http_and_present_in_the_catalogue() {
        for p in platforms() {
            let pairs = p.fingerprint_headers();
            assert!(!pairs.is_empty(), "{}: no fingerprint headers", p.id());
            let template = catalog()
                .all()
                .iter()
                .find(|provider| provider.oauth_platform.as_deref() == Some(p.id()));
            let template =
                template.unwrap_or_else(|| panic!("platform {} has no catalogue entry", p.id()));
            for (name, value) in pairs {
                assert!(
                    HeaderName::from_bytes(name.as_bytes()).is_ok(),
                    "{}/invalid header name {name:?}",
                    p.id()
                );
                assert!(
                    HeaderValue::from_str(value).is_ok(),
                    "{}/invalid header value for {name:?}",
                    p.id()
                );
                // The golden-header invariant: what the code will send is what the data says.
                let in_catalogue = template
                    .headers
                    .iter()
                    .any(|(k, v)| k.eq_ignore_ascii_case(name) && v == value);
                assert!(
                    in_catalogue,
                    "{}: fingerprint header {name:?} = {value:?} does not match the catalogue",
                    p.id()
                );
            }
        }
    }

    #[test]
    fn loopback_and_device_platforms_declare_their_flows() {
        let codex = platform("openai-codex").unwrap();
        assert!(codex.begins_with_loopback());
        assert!(codex.requires_pkce());
        assert!(codex.validates_redirect_port());
        let ports = codex.redirect_ports();
        assert_eq!(ports.host, "127.0.0.1");
        assert_eq!(ports.candidates(), vec![1455, 1457]);
        assert_eq!(ports.path, "/auth/callback");

        assert!(platform("antigravity").is_none());

        let copilot = platform("github-copilot").unwrap();
        assert!(!copilot.begins_with_loopback());
        assert!(!copilot.requires_pkce());
        // No port exists to validate.
        assert!(!copilot.validates_redirect_port());
        assert_eq!(copilot.redirect_ports().primary, 0);
    }

    #[test]
    fn redirect_uri_is_built_from_the_bound_port() {
        let ports = platform("openai-codex").unwrap().redirect_ports();
        assert_eq!(
            ports.redirect_uri(1455),
            "http://127.0.0.1:1455/auth/callback"
        );
        assert_eq!(
            ports.redirect_uri(1457),
            "http://127.0.0.1:1457/auth/callback"
        );
        assert!(platform("antigravity").is_none());
    }

    #[tokio::test]
    async fn flow_directions_reject_the_wrong_entry_point() {
        // Loopback platforms have no device flow …
        for id in ["openai-codex"] {
            let p = platform(id).unwrap();
            assert!(p.device_start().await.is_err(), "{id}");
            assert!(p.device_poll("anything").await.is_err(), "{id}");
        }
        // … and the device platform has no authorize URL to pre-build.
        let copilot = platform("github-copilot").unwrap();
        assert!(copilot.authorization_url("state", None, 0).is_err());
    }

    #[test]
    fn pkce_matches_the_rfc_7636_appendix_b_vector() {
        // RFC 7636 Appendix B: verifier -> challenge, verified independently with `openssl`.
        let challenge = Pkce::s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_is_urlsafe_unpadded_and_in_range() {
        let pkce = Pkce::generate();
        let verifier = pkce.verifier();
        assert_eq!(verifier.len(), 43, "32 bytes encode to 43 base64url chars");
        assert!(verifier.len() >= 43 && verifier.len() <= 128);
        assert!(
            verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')),
            "verifier must use the RFC 7636 unreserved alphabet"
        );
        assert!(
            !verifier.contains('=') && !verifier.contains('+') && !verifier.contains('/'),
            "verifier must be base64url without padding"
        );
        assert_eq!(pkce.challenge(), Pkce::s256(verifier));
        assert_eq!(pkce.challenge().len(), 43);
        assert!(!pkce.challenge().contains('='));
        // Fresh every time: a predictable verifier defeats PKCE entirely.
        assert_ne!(Pkce::generate().verifier(), verifier);
    }

    #[test]
    fn pkce_rejects_verifiers_outside_the_rfc() {
        assert!(Pkce::from_verifier("too-short").is_err());
        assert!(Pkce::from_verifier("a".repeat(129)).is_err());
        assert!(
            Pkce::from_verifier("a".repeat(43).replace('a', "=").as_str()).is_err(),
            "padding is not in the allowed alphabet"
        );
        assert!(Pkce::from_verifier("b".repeat(43)).is_ok());
    }

    #[test]
    fn pkce_debug_hides_the_verifier() {
        let pkce = Pkce::generate();
        let printed = format!("{pkce:?}");
        assert!(
            !printed.contains(pkce.verifier()),
            "Debug leaked the PKCE verifier: {printed}"
        );
        // The challenge is already public in the authorize URL, so it stays visible.
        assert!(printed.contains(pkce.challenge()));
    }

    #[test]
    fn token_set_round_trips_and_preserves_unknown_keys() {
        let mut extra = Map::new();
        extra.insert(
            "id_token".to_string(),
            Value::String("header.payload.sig".into()),
        );
        extra.insert("weird_new_key".to_string(), json!({"nested": [1, 2]}));
        extra.insert("expires_in".to_string(), json!(3600));
        let tokens = TokenSet {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            expires_at: Millis(1_700_000_000_000),
            scopes: vec!["openid".into(), "email".into()],
            token_type: "Bearer".into(),
            extra,
        };
        let encoded = serde_json::to_string(&tokens).expect("serialize");
        let decoded: TokenSet = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, tokens);
        assert_eq!(
            decoded.extra_str("id_token"),
            Some("header.payload.sig"),
            "unknown keys must survive the round trip"
        );
        assert_eq!(
            decoded.extra.get("weird_new_key"),
            Some(&json!({"nested": [1, 2]}))
        );
        assert_eq!(decoded.expires_at, Millis(1_700_000_000_000));
        // …and the JSON itself still carries them as plain keys, not as a nested blob.
        let value: serde_json::Value = serde_json::from_str(&encoded).expect("reparse");
        assert_eq!(value["weird_new_key"]["nested"], json!([1, 2]));
        assert_eq!(value["access_token"], json!("at"));
    }

    #[test]
    fn token_set_expiry_math_is_in_milliseconds() {
        let mut tokens = TokenSet::new("at");
        tokens.expires_at = Millis(1_700_000_060_000);
        let now = Millis(1_700_000_000_000);
        assert!(!tokens.is_expired(now));
        assert!(tokens.is_expired(now.plus_millis(60_000)));
        assert!(tokens.is_expired(now.plus_millis(60_001)));
        // An unknown expiry (zero) must never be read as "valid forever".
        assert!(TokenSet::new("at").is_expired(now));
        // Seconds passed off as milliseconds would expire in 1970; this pins the unit.
        assert!(TokenSet::new("at").is_expired(Millis::from_millis(1)));
    }

    #[test]
    fn token_set_debug_hides_every_token_value() {
        let mut extra = Map::new();
        extra.insert(
            "id_token".to_string(),
            Value::String("super-secret-jwt".into()),
        );
        let tokens = TokenSet {
            access_token: "ACCESS-SECRET-VALUE".into(),
            refresh_token: Some("REFRESH-SECRET-VALUE".into()),
            expires_at: Millis(1_700_000_000_000),
            scopes: vec!["openid".into()],
            token_type: "Bearer".into(),
            extra,
        };
        let printed = format!("{tokens:?}");
        assert!(
            !printed.contains("ACCESS-SECRET-VALUE"),
            "Debug leaked access token: {printed}"
        );
        assert!(
            !printed.contains("REFRESH-SECRET-VALUE"),
            "Debug leaked refresh token: {printed}"
        );
        assert!(
            !printed.contains("super-secret-jwt"),
            "Debug leaked an extra value: {printed}"
        );
        // Shape survives: lengths and key names are the point of the hand-written impl.
        assert!(printed.contains("20 bytes"));
        assert!(printed.contains("id_token"));
        assert!(printed.contains("expires_at"));
    }

    #[test]
    fn device_authorization_debug_masks_the_code_only() {
        let auth = DeviceAuthorization {
            device_code: "DEVICE-CODE-SECRET".into(),
            user_code: "ABCD-1234".into(),
            verification_uri: "https://github.com/login/device".into(),
            verification_uri_complete: None,
            expires_in: Duration::from_secs(900),
            interval: Duration::from_secs(5),
        };
        let printed = format!("{auth:?}");
        assert!(
            !printed.contains("DEVICE-CODE-SECRET"),
            "Debug leaked device code: {printed}"
        );
        // The human needs the user code and the URI, so those stay visible.
        assert!(printed.contains("ABCD-1234"));
        assert!(printed.contains("https://github.com/login/device"));
    }

    #[test]
    fn device_poll_outcomes_stay_distinct() {
        let pending = DevicePoll::Pending;
        let slow = DevicePoll::SlowDown;
        assert!(pending.is_continue());
        assert!(slow.is_continue());
        assert_ne!(
            pending, slow,
            "slow_down is not pending: the interval must change"
        );

        let refused = DevicePoll::AccessDenied;
        assert!(refused.is_refusal());
        assert!(!refused.is_continue());

        // Only AccessDenied is a refusal: the others are progress, a deadline, or a fault.
        assert!(!DevicePoll::ExpiredToken.is_refusal());
        assert!(!DevicePoll::Failed {
            message: "boom".into()
        }
        .is_refusal());
        assert!(!DevicePoll::Success(TokenSet::new("at")).is_refusal());
        assert!(!DevicePoll::Success(TokenSet::new("at")).is_continue());
    }

    #[test]
    fn header_builder_rejects_invalid_values_without_panicking() {
        assert!(header("x-ok", "value").is_some());
        assert!(header("not a header name", "value").is_none());
        // A newline in a value would be header injection; it must be dropped, not unwrapped.
        assert!(header("x-bad", "value\r\nInjected: 1").is_none());
    }

    #[test]
    fn oauth_error_names_the_subsystem() {
        let err = oauth_error("something broke");
        assert!(err.to_string().contains("oauth: something broke"));
    }
}
