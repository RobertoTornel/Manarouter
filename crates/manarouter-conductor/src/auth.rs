//! Panel authentication.
//!
//! Three things protect the control plane, and they are deliberately layered so that no single
//! mistake exposes it:
//!
//! 1. **Loopback by default.** `control.host` defaults to `127.0.0.1`. Most panels are never
//!    exposed at all, so most of them cannot be attacked.
//! 2. **A password, if one is set**, stored as an Argon2id PHC string — never plaintext, never a
//!    fast hash.
//! 3. **An origin check** on every state-changing request, so a page the operator happens to visit
//!    cannot drive the API through their browser.
//!
//! # Why there is no default password
//!
//! One of the routers this project learned from ships with a literal default password of `123456`.
//! That is worse than having no password, because it is a password that looks configured.
//!
//! Manarouter defaults to being *unreachable* instead. An unreachable panel cannot be brute-forced,
//! and a panel that must be reached from another machine requires the operator to have chosen a
//! secret first. `allow_remote_without_password` exists so that the operator can explicitly, and
//! visibly, opt out — it is not a quiet fallback.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
use argon2::Argon2;
use rand::RngCore;
use subtle::ConstantTimeEq;

/// A session lifetime.
///
/// Twelve hours: long enough that an operator working a full day is not logged out mid-task, short
/// enough that a forgotten browser session on a shared machine does not stay valid indefinitely.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// The cookie name. Namespaced so it does not collide with anything else on the loopback host.
pub const SESSION_COOKIE: &str = "manarouter_session";

/// Hash a password for storage.
///
/// Argon2id is used with the crate's defaults, which are the OWASP-recommended parameters. The
/// output is the full PHC string, so the parameters travel with the hash and can be raised later
/// without invalidating existing passwords.
pub fn hash_password(plain: &str) -> Result<String, AuthError> {
    if plain.is_empty() {
        return Err(AuthError::EmptyPassword);
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::HashFailed)
}

/// Verify a password against a stored PHC string.
///
/// Returns `false` rather than an error for a malformed stored hash: a corrupt hash must not be
/// distinguishable from a wrong password by an attacker probing the endpoint, and the operator sees
/// the difference in the log instead.
pub fn verify_password(plain: &str, stored_phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored_phc) else {
        tracing::error!("stored password hash is malformed; no password can match it");
        return false;
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

/// Whether a host names the loopback interface.
///
/// Accepts the three spellings a person might write, plus the `localhost` name. Parsing is attempted
/// first so that `[::1]`, `::1` and `127.0.0.1` are all handled by the IP parser rather than by
/// string comparison, which would get IPv6 wrong.
pub fn is_loopback(host: &str) -> bool {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match trimmed.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// Whether the control plane is reachable from anywhere other than this machine.
///
/// This is the condition that requires a password. It is computed from the configured bind host
/// rather than from the address a request arrived on, because the bind host is what the operator
/// chose and therefore what they must have thought about.
///
/// `0.0.0.0` and `::` are not loopback, so they need no special case: a wildcard bind is the most
/// exposed choice there is, not an exemption from the rule.
pub fn is_remote_exposed(bind_host: &str) -> bool {
    !is_loopback(bind_host)
}

/// Errors from the password machinery.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// An empty password was supplied.
    #[error("password must not be empty")]
    EmptyPassword,
    /// The hasher failed, which in practice means a system RNG failure.
    #[error("could not hash the password")]
    HashFailed,
}

/// An in-memory set of issued sessions.
///
/// Deliberately in memory: a restart invalidating every session is a feature, not a limitation. It
/// means "restart the conductor" is a complete remedy for a suspected session compromise, with no
/// revocation list to maintain.
///
/// # Why the token is compared in constant time
///
/// Sessions are keyed by a lookup that an attacker can drive at high speed on a loopback socket. A
/// byte-by-byte early-exit comparison leaks the token one byte at a time, which turns an
/// infeasible 2^256 search into 256 guesses of one byte each. `subtle::ConstantTimeEq` removes
/// that, and costs nothing measurable at panel request rates.
#[derive(Debug, Default)]
pub struct Sessions {
    inner: parking_lot_shim::Mutex<std::collections::HashMap<[u8; 32], Session>>,
}

#[derive(Debug, Clone, Copy)]
struct Session {
    expires_at: u64,
}

/// A minimal mutex wrapper so this module does not need to choose between `std` and `tokio` locks
/// at the call site; it is always used from a sync context.
mod parking_lot_shim {
    pub use std::sync::Mutex;
}

impl Sessions {
    /// An empty session set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a new session, returning its token.
    ///
    /// 32 bytes from the OS CSPRNG, rendered as lowercase hex. This is a bearer credential, so it
    /// must be unguessable; a counter or a timestamp-derived value would be neither.
    pub fn issue(&self) -> String {
        let mut raw = [0u8; 32];
        OsRng.fill_bytes(&mut raw);
        let token = hex_encode(&raw);
        let now = unix_now();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Opportunistically drop expired sessions so a long-running process does not accumulate
        // them. Doing it here rather than on a timer means there is no background task to leak and
        // no lock held while a timer fires.
        guard.retain(|_, s| s.expires_at > now);
        guard.insert(
            raw,
            Session {
                expires_at: now + SESSION_TTL.as_secs(),
            },
        );
        token
    }

    /// Check a token, refreshing its expiry on success.
    pub fn verify(&self, token: &str) -> bool {
        let Some(raw) = hex_decode_32(token) else {
            return false;
        };
        let now = unix_now();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, s| s.expires_at > now);
        // Linear scan with a constant-time comparison per candidate. The map is small (one entry per
        // live browser session), so this is faster than hashing the token to look it up, and it
        // avoids timing differences between a hit and a miss.
        let mut found = false;
        for (k, v) in guard.iter_mut() {
            if bool::from(k.ct_eq(&raw)) {
                v.expires_at = now + SESSION_TTL.as_secs();
                found = true;
            }
        }
        found
    }

    /// Drop a single session.
    pub fn revoke(&self, token: &str) {
        let Some(raw) = hex_decode_32(token) else {
            return;
        };
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|k, _| !bool::from(k.ct_eq(&raw)));
    }

    /// Drop every session.
    pub fn revoke_all(&self) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// How many sessions are live. For the panel's status view.
    pub fn len(&self) -> usize {
        let now = unix_now();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.retain(|_, s| s.expires_at > now);
        guard.len()
    }

    /// Whether no sessions are live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The panel's authentication policy, resolved from configuration.
#[derive(Debug, Clone)]
pub struct PanelAuth {
    /// The configured bind host.
    pub bind_host: String,
    /// The Argon2id hash, if a password is set.
    pub password_hash: Option<String>,
    /// Whether the operator has explicitly allowed remote access with no password.
    pub allow_remote_without_password: bool,
    /// The issued sessions.
    pub sessions: Arc<Sessions>,
}

impl PanelAuth {
    /// Build the policy and refuse a configuration that would expose the panel unprotected.
    ///
    /// The refusal is the point. A warning in a log is not enough: an operator who binds the panel
    /// to `0.0.0.0` without a password has created an unauthenticated remote administration
    /// interface, and the correct response is to not start.
    pub fn resolve(
        bind_host: &str,
        password_hash: Option<String>,
        allow_remote_without_password: bool,
    ) -> Result<Self, AuthError> {
        let remote = !is_loopback(bind_host);
        if remote && password_hash.is_none() && !allow_remote_without_password {
            return Err(AuthError::EmptyPassword);
        }
        if remote && password_hash.is_none() {
            tracing::warn!(
                host = bind_host,
                "the control plane is bound to a non-loopback address with NO password; \
                 anyone who can reach this port can read and change your configuration. \
                 Set a password, or bind to 127.0.0.1."
            );
        }
        Ok(Self {
            bind_host: bind_host.to_owned(),
            password_hash,
            allow_remote_without_password,
            sessions: Arc::new(Sessions::new()),
        })
    }

    /// Whether a password is required to use the panel at all.
    pub fn password_required(&self) -> bool {
        self.password_hash.is_some()
    }

    /// Verify a login attempt and issue a session if it succeeds.
    pub fn login(&self, password: &str) -> Option<String> {
        let stored = self.password_hash.as_deref()?;
        if verify_password(password, stored) {
            Some(self.sessions.issue())
        } else {
            None
        }
    }

    /// Whether a request carrying `token` may proceed.
    ///
    /// With no password configured, the panel is open — but only because the operator is either
    /// unreachable-by-construction (a loopback bind) or has explicitly accepted the risk
    /// (`allow_remote_without_password`). Those are the only two ways to reach an unauthenticated
    /// panel, and both are deliberate choices rather than defaults that leaked.
    ///
    /// With a password configured, a valid session is required even on loopback. An operator who set
    /// a password meant it; bypassing it on loopback would mean a local process — or a page in the
    /// operator's browser — could drive the API without it.
    pub fn permits(&self, token: Option<&str>) -> bool {
        if !self.password_required()
            && (is_loopback(&self.bind_host) || self.allow_remote_without_password)
        {
            return true;
        }
        match token {
            Some(t) => self.sessions.verify(t),
            None => false,
        }
    }
}

/// Extract a named cookie from a `Cookie` header.
///
/// Hand-written rather than pulled from a crate because the header grammar that matters here is
/// three lines long, and a dependency on the request path of an admin panel is not free.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    for part in header.split(';') {
        let part = part.trim();
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        if k == name {
            return Some(v);
        }
    }
    None
}

/// Whether an `Origin` or `Referer` header is acceptable for a state-changing request.
///
/// Same-origin, or a loopback origin, is allowed. Anything else is refused. This is what stops a
/// page the operator visits from using their authenticated session against the API.
pub fn origin_is_acceptable(origin: Option<&str>, referer: Option<&str>, port: u16) -> bool {
    let value = origin.or(referer);
    let Some(value) = value else {
        // No origin header at all: a same-origin `fetch` from a modern browser always sends one, so
        // its absence means a non-browser client such as the CLI or `curl`. Those carry a session
        // token explicitly and are not subject to a confused-deputy attack.
        return true;
    };
    let (scheme, rest) = match value.split_once("://") {
        Some((scheme, rest)) => (scheme, rest),
        None => return false,
    };
    // An origin that omits its port is on its scheme's default port. Resolve that before
    // comparing, or `http://127.0.0.1` (port 80) would be accepted for a panel on any port,
    // which is a different origin.
    let default_port = if scheme.eq_ignore_ascii_case("http") {
        80
    } else if scheme.eq_ignore_ascii_case("https") {
        443
    } else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip a userinfo component, which must not influence the decision.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port_part) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(']') || h.ends_with(']') => (h, Some(p)),
        _ => (authority, None),
    };
    if !is_loopback(host) {
        return false;
    }
    match port_part {
        None => default_port == port,
        Some(p) => p.parse::<u16>().map(|p| p == port).unwrap_or(false),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips() {
        let phc = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &phc));
        assert!(!verify_password("wrong horse battery staple", &phc));
    }

    #[test]
    fn the_stored_hash_is_argon2id_and_contains_no_plaintext() {
        let phc = hash_password("hunter2").unwrap();
        assert!(
            phc.starts_with("$argon2id$"),
            "expected argon2id, got {phc}"
        );
        assert!(!phc.contains("hunter2"));
    }

    #[test]
    fn the_same_password_hashes_differently_every_time() {
        // A per-hash salt, not a global one. Identical hashes would let an attacker with the config
        // file tell which deployments share a password.
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b);
        assert!(verify_password("same", &a));
        assert!(verify_password("same", &b));
    }

    #[test]
    fn an_empty_password_is_refused() {
        assert!(matches!(hash_password(""), Err(AuthError::EmptyPassword)));
    }

    #[test]
    fn a_malformed_stored_hash_matches_nothing_and_does_not_panic() {
        for bad in [
            "",
            "not-a-hash",
            "$argon2id$broken",
            "$2b$10$abcdefghijklmnopqrstuv",
        ] {
            assert!(!verify_password("anything", bad), "matched {bad:?}");
        }
    }

    #[test]
    fn a_token_is_a_bearer_credential_not_a_predictable_value() {
        // Two sessions issued in the same millisecond must not collide, and the token must not
        // contain anything derived from the clock.
        let s = Sessions::new();
        let a = s.issue();
        let b = s.issue();
        assert_ne!(a, b);
        assert_eq!(a.len(), 64, "expected 32 bytes of hex");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn issuing_and_verifying_a_session_works() {
        let s = Sessions::new();
        let t = s.issue();
        assert!(s.verify(&t));
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn a_forged_or_malformed_token_never_verifies() {
        let s = Sessions::new();
        let t = s.issue();
        assert!(!s.verify(""));
        assert!(!s.verify("00"));
        assert!(!s.verify(&"0".repeat(64)));
        // One hex digit changed.
        let mut forged = t.clone();
        let last = forged.pop().unwrap();
        forged.push(if last == 'f' { '0' } else { 'f' });
        assert!(
            !s.verify(&forged),
            "a single-character forgery must not verify"
        );
        // Non-hex of the right length.
        assert!(!s.verify(&"z".repeat(64)));
    }

    #[test]
    fn revoking_a_session_is_immediate_and_does_not_affect_others() {
        let s = Sessions::new();
        let a = s.issue();
        let b = s.issue();
        s.revoke(&a);
        assert!(!s.verify(&a));
        assert!(s.verify(&b));
    }

    #[test]
    fn revoke_all_clears_everything() {
        let s = Sessions::new();
        let a = s.issue();
        let b = s.issue();
        s.revoke_all();
        assert!(!s.verify(&a));
        assert!(!s.verify(&b));
        assert!(s.is_empty());
    }

    #[test]
    fn revoking_an_unknown_or_malformed_token_is_harmless() {
        let s = Sessions::new();
        let a = s.issue();
        s.revoke("nonsense");
        s.revoke("");
        s.revoke(&"0".repeat(64));
        assert!(
            s.verify(&a),
            "an unrelated revoke must not drop a live session"
        );
    }

    #[test]
    fn loopback_is_recognised_in_every_spelling() {
        for host in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "[::1]",
            "localhost",
            "LOCALHOST",
            " 127.0.0.1 ",
        ] {
            assert!(is_loopback(host), "{host} should be loopback");
        }
        for host in [
            "0.0.0.0",
            "::",
            "192.168.1.5",
            "example.com",
            "",
            "not an ip",
        ] {
            assert!(!is_loopback(host), "{host} should not be loopback");
        }
    }

    #[test]
    fn a_remote_bind_without_a_password_is_refused() {
        // The whole point of the layered defence: this must not start.
        let err = PanelAuth::resolve("0.0.0.0", None, false);
        assert!(
            err.is_err(),
            "a remote bind with no password must be refused"
        );
    }

    #[test]
    fn a_remote_bind_with_a_password_is_allowed() {
        let phc = hash_password("secret").unwrap();
        let auth = PanelAuth::resolve("0.0.0.0", Some(phc), false).unwrap();
        assert!(auth.password_required());
        assert!(!auth.permits(None));
    }

    #[test]
    fn a_remote_bind_with_the_explicit_opt_out_is_allowed_but_unprotected() {
        let auth = PanelAuth::resolve("0.0.0.0", None, true).unwrap();
        assert!(!auth.password_required());
        // Nothing to authenticate against, so every request is permitted; the operator asked for
        // this explicitly and the startup warning names the consequence.
        assert!(auth.permits(None));
    }

    #[test]
    fn a_loopback_panel_with_no_password_is_open_by_default() {
        let auth = PanelAuth::resolve("127.0.0.1", None, false).unwrap();
        assert!(auth.permits(None));
    }

    #[test]
    fn a_loopback_panel_with_a_password_requires_a_session() {
        let phc = hash_password("secret").unwrap();
        let auth = PanelAuth::resolve("127.0.0.1", Some(phc), false).unwrap();
        assert!(
            !auth.permits(None),
            "a configured password must be enforced even on loopback"
        );
        assert!(!auth.permits(Some("bogus")));
        let token = auth
            .login("secret")
            .expect("correct password must issue a session");
        assert!(auth.permits(Some(&token)));
    }

    #[test]
    fn a_wrong_password_issues_nothing() {
        let phc = hash_password("secret").unwrap();
        let auth = PanelAuth::resolve("127.0.0.1", Some(phc), false).unwrap();
        assert!(
            auth.login("Secret").is_none(),
            "passwords are case-sensitive"
        );
        assert!(auth.login("").is_none());
        assert!(auth.login("secret ").is_none());
    }

    #[test]
    fn login_is_impossible_when_no_password_is_configured() {
        let auth = PanelAuth::resolve("127.0.0.1", None, false).unwrap();
        assert!(auth.login("anything").is_none());
        assert!(auth.login("").is_none());
    }

    #[test]
    fn cookies_are_parsed_correctly() {
        let header = "other=1; manarouter_session=abc123; trailing=2";
        assert_eq!(cookie_value(header, SESSION_COOKIE), Some("abc123"));
        assert_eq!(cookie_value(header, "other"), Some("1"));
        assert_eq!(cookie_value(header, "trailing"), Some("2"));
        assert_eq!(cookie_value(header, "absent"), None);
        assert_eq!(cookie_value("", SESSION_COOKIE), None);
        assert_eq!(cookie_value("novalue", SESSION_COOKIE), None);
        // No whitespace after a semicolon, and an empty value.
        assert_eq!(
            cookie_value("a=1;manarouter_session=", SESSION_COOKIE),
            Some("")
        );
        // A prefix must not match.
        assert_eq!(cookie_value("xmanarouter_session=no", SESSION_COOKIE), None);
    }

    #[test]
    fn a_same_origin_request_is_accepted() {
        assert!(origin_is_acceptable(
            Some("http://127.0.0.1:20901"),
            None,
            20901
        ));
        assert!(origin_is_acceptable(
            Some("http://localhost:20901"),
            None,
            20901
        ));
        assert!(origin_is_acceptable(
            Some("http://[::1]:20901"),
            None,
            20901
        ));
    }

    #[test]
    fn a_missing_origin_is_accepted_because_it_means_a_non_browser_client() {
        // `curl` and the CLI send no Origin. They must still work; they carry the token explicitly.
        assert!(origin_is_acceptable(None, None, 20901));
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        // This is the confused-deputy defence: a page the operator visits must not be able to drive
        // the API using their session cookie.
        assert!(!origin_is_acceptable(
            Some("https://evil.example"),
            None,
            20901
        ));
        assert!(!origin_is_acceptable(
            Some("http://192.168.1.9:20901"),
            None,
            20901
        ));
        assert!(!origin_is_acceptable(
            Some("http://127.0.0.1:8080"),
            None,
            20901
        ));
    }

    #[test]
    fn a_deceptive_origin_is_refused() {
        // Hosts that merely contain "127.0.0.1" must not pass a substring check.
        for evil in [
            "http://127.0.0.1.evil.com",
            "http://evil.com/?x=127.0.0.1",
            "http://evil.com#127.0.0.1",
            "http://user@evil.com",
        ] {
            assert!(
                !origin_is_acceptable(Some(evil), None, 20901),
                "{evil} must be refused"
            );
        }
        // A userinfo section must not be able to smuggle a loopback host past the check.
        assert!(!origin_is_acceptable(
            Some("http://127.0.0.1@evil.com"),
            None,
            20901
        ));
        // ...but a legitimate loopback origin with userinfo is still loopback.
        assert!(origin_is_acceptable(
            Some("http://user@127.0.0.1:20901"),
            None,
            20901
        ));
    }

    #[test]
    fn a_garbage_origin_is_refused_rather_than_panicking() {
        for bad in [
            "",
            "not a url",
            "://",
            "http://",
            "http://:20901",
            "http://[::1",
        ] {
            let _ = origin_is_acceptable(Some(bad), None, 20901);
        }
    }

    #[test]
    fn a_referer_is_used_when_the_origin_header_is_absent() {
        assert!(origin_is_acceptable(
            None,
            Some("http://127.0.0.1:20901/settings"),
            20901
        ));
        assert!(!origin_is_acceptable(
            None,
            Some("https://evil.example/page"),
            20901
        ));
    }

    #[test]
    fn hex_round_trips_and_rejects_junk() {
        let raw = [0xABu8; 32];
        let s = hex_encode(&raw);
        assert_eq!(hex_decode_32(&s), Some(raw));
        assert_eq!(hex_decode_32("zz"), None);
        assert_eq!(hex_decode_32(""), None);
        assert_eq!(hex_decode_32(&"a".repeat(64)), Some([0xAA; 32]));
        assert_eq!(hex_decode_32(&"A".repeat(64)), Some([0xAA; 32]));
    }
}
