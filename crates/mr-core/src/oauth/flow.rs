//! Loopback redirects, device-code pacing, and the HTTPS client every platform's token calls
//! use.
//!
//! Three separations this file exists to keep:
//!
//! * **State machine vs I/O.** [`DeviceSchedule`] and [`PollPlan`] decide *when* the next poll
//!   may go out and *when* the flow is over; [`drive_device_polls`] performs the sleeping and
//!   the network calls. The schedule is testable with no sockets at all.
//! * **Bind vs publish.** The redirect URI is always derived from the port that was actually
//!   bound ([`Loopback::redirect_uri`]), never from a constant: publishing `…:1455/…` while the
//!   fallback bound 1457 would send the browser to a dead port.
//! * **One callback, bounded in time.** The listener accepts until one *matching-path* request
//!   arrives or the deadline passes, verifies `state` in constant time, renders a
//!   self-contained HTML page and never echoes the code back to the browser.

use std::io;
use std::time::Duration;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{Method, Uri};
use http_body_util::{BodyExt as _, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::TokioExecutor;
use rand::rngs::OsRng;
use rand::RngCore as _;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

use super::{oauth_error, DeviceAuthorization, DevicePoll, OAuthPlatform, RedirectPorts};
use crate::error::{Error, Result};

/// Upper bound on a single token-endpoint exchange or refresh.
///
/// Long enough for a slow TLS handshake, short enough that a hung provider surfaces as an error
/// the refresher can classify instead of pinning a single-flight slot forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Percent-encode one query or form component.
///
/// Everything outside RFC 3986's unreserved set (`A-Za-z0-9-._~`) is escaped, so a code, state
/// or scope string with `&`, `=`, `+` or a non-ASCII byte cannot alter the structure of the
/// message it travels in. The `+` case is the classic trap: form encoding reads a raw `+` as a
/// space, so leaving it unescaped corrupts any value that legitimately contains one.
pub fn url_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Decode one query or form component: `%XX` escapes, plus `+` as a space (form convention).
///
/// Invalid escapes are left verbatim rather than failing: a malformed value will be rejected
/// downstream (state comparison, code exchange) where the error can be reported properly.
pub fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                // Decode from bytes, never by slicing the &str: the next two bytes may fall
                // inside a multi-byte character (e.g. "%aé"), and slicing there panics.
                fn hex_digit(b: u8) -> Option<u8> {
                    match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    }
                }
                match (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi << 4) | lo);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A fresh OAuth `state`: 32 CSPRNG bytes, base64url without padding (43 chars).
///
/// The state is the CSRF token of the redirect flow: it is minted before the browser opens,
/// comes back on the callback, and must match before the code is accepted. Randomness matters —
/// a guessable state lets an attacker who can hit the loopback port pre-author a callback.
pub fn new_state() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Compare the returned `state` against the one we issued, **in constant time**.
///
/// # Why constant time (CSRF)
///
/// The state is a single-use secret gate: if an attacker can drive a victim's browser to
/// `http://127.0.0.1:port/auth/callback?code=<theirs>&state=<guess>`, a *byte-by-byte* comparison
/// that returns early on the first mismatch leaks the longest correct prefix through timing —
/// the attacker refines the guess one response-time measurement per character and eventually
/// forges a callback that our own listener accepts, exchanging a code we never authorized. The
/// XOR fold below touches every byte regardless of where a difference sits, so the response time
/// carries no information about the expected state. The length check that runs first is safe to
/// short-circuit: the length is fixed by our own minting (43 chars) and is not a secret — the
/// content is.
pub fn state_matches(expected: &str, received: &str) -> bool {
    if expected.len() != received.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(received.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Build an `application/x-www-form-urlencoded` body from ordered pairs.
pub fn form_body(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The reply of one token-endpoint call, before any platform interprets it.
#[derive(Debug, Clone)]
pub struct HttpReply {
    /// HTTP status — classification of a refresh failure starts here.
    pub status: u16,
    /// Body, lossily decoded: error bodies from token endpoints are human-readable text or
    /// JSON, never binary.
    pub body: String,
    /// `Content-Type`, so a platform can choose JSON vs form decoding (GitHub's device
    /// endpoints answer either).
    pub content_type: Option<String>,
}

type HttpsClient = LegacyClient<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// A small HTTPS client shared by every platform's token calls.
///
/// Construction mirrors `manarouter-relay/src/upstream.rs`: webpki roots (no system store
/// dependency), ALPN `h2` then `http/1.1`, Nagle off. OAuth calls are rare (login, refresh),
/// so the pool is deliberately tiny.
pub struct Http {
    client: HttpsClient,
}

/// Transport seam used by OAuth protocols and deterministic unit tests.
#[async_trait::async_trait]
pub trait OAuthHttp: Send + Sync {
    async fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        pairs: &[(&str, &str)],
    ) -> Result<HttpReply>;
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> Result<HttpReply>;
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpReply>;
}

#[async_trait::async_trait]
impl OAuthHttp for Http {
    async fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        pairs: &[(&str, &str)],
    ) -> Result<HttpReply> {
        Http::post_form(self, url, headers, pairs).await
    }
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> Result<HttpReply> {
        Http::post_json(self, url, headers, body).await
    }
    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpReply> {
        Http::get(self, url, headers).await
    }
}

impl Http {
    /// Build the client. The webpki roots are loaded once per process — they never change.
    pub fn new() -> Result<Self> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        let mut http = HttpConnector::new();
        http.set_nodelay(true);
        http.enforce_http(false);

        let https = HttpsConnector::from((http, std::sync::Arc::new(tls)));
        let client = LegacyClient::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(2)
            .http2_only(false)
            .build(https);
        Ok(Self { client })
    }

    /// The process-wide instance, built on first use.
    pub fn shared() -> Result<&'static Http> {
        static HTTP: std::sync::OnceLock<Http> = std::sync::OnceLock::new();
        if let Some(existing) = HTTP.get() {
            return Ok(existing);
        }
        let built = Self::new()?;
        Ok(HTTP.get_or_init(|| built))
    }

    /// `POST` an `application/x-www-form-urlencoded` body — the encoding Google, OpenAI's
    /// authorization-code grant and GitHub's device endpoint all use.
    pub async fn post_form(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        pairs: &[(&str, &str)],
    ) -> Result<HttpReply> {
        self.send(
            Method::POST,
            url,
            Some("application/x-www-form-urlencoded"),
            Some(form_body(pairs)),
            headers,
        )
        .await
    }

    /// `POST` a JSON body — the encoding OpenAI's *refresh* grant uses on the very same
    /// endpoint its exchange grant posts as form (see `codex.rs`).
    pub async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> Result<HttpReply> {
        self.send(
            Method::POST,
            url,
            Some("application/json"),
            Some(body),
            headers,
        )
        .await
    }

    /// Plain `GET`, for identity endpoints.
    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpReply> {
        self.send(Method::GET, url, None, None, headers).await
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        content_type: Option<&str>,
        body: Option<String>,
        headers: &[(&str, &str)],
    ) -> Result<HttpReply> {
        // Query strings are never logged or echoed in errors: a URL can always end up carrying
        // something sensitive, and the host is what an operator needs to diagnose a failure.
        let uri: Uri = url
            .parse()
            .map_err(|_| oauth_error(format!("invalid {method} url for OAuth")))?;
        let host = uri.host().unwrap_or("<no host>").to_string();

        let mut builder = hyper::Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        if let Some(content_type) = content_type {
            builder = builder.header(CONTENT_TYPE, content_type);
        }
        let request = builder
            .body(Full::new(Bytes::from(body.unwrap_or_default())))
            .map_err(|e| oauth_error(format!("building OAuth request for {host}: {e}")))?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| oauth_error(format!("timed out contacting {host}")))?
            .map_err(|e| Error::Io(io::Error::other(format!("OAuth request to {host}: {e}"))))?;

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        // Bound the reply: a bare `collect()` under a timeout lets a hostile or broken
        // token endpoint stream gigabytes into RAM before the deadline fires. Token replies
        // are a few kilobytes; one mebibyte is well above any legitimate error body.
        const MAX_REPLY: usize = 1024 * 1024;
        let reply = http_body_util::Limited::new(response.into_body(), MAX_REPLY);
        let collected = tokio::time::timeout(REQUEST_TIMEOUT, reply.collect())
            .await
            .map_err(|_| oauth_error(format!("timed out reading the reply from {host}")))?
            .map_err(|e| Error::Io(io::Error::other(format!("OAuth reply from {host}: {e}"))))?
            .to_bytes();

        Ok(HttpReply {
            status,
            body: String::from_utf8_lossy(&collected).into_owned(),
            content_type,
        })
    }
}

/// One delivered browser callback, after `state` verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    /// The authorization code — to be exchanged, never logged or rendered.
    pub code: String,
    /// The state that was verified (already matched; kept for tests and logs-of-shape).
    pub state: String,
    /// The port the listener actually bound.
    pub port: u16,
}

/// Escape text for embedding in HTML.
///
/// The failure page shows the provider's `error`/`error_description` parameters, which are
/// attacker-influenced input on a loopback URL: unescaped, `&error=<script>…` would run in a
/// page we serve. Keep this conservative — the pages take no scripts either way.
pub fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // Control bytes have no business in a rendered message and can break the page.
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

const PAGE_STYLE: &str = "body{font-family:system-ui,sans-serif;margin:4rem auto;max-width:32rem;\
color:#111;background:#fafafa}h1{font-size:1.4rem}code{background:#eee;padding:.1rem .3rem;\
border-radius:.25rem}";

/// The page shown after a successful login. Self-contained: no scripts, no external requests —
/// this window is only ever shown to confirm, then closed by the human.
pub fn success_page(label: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Sign-in complete</title><style>{PAGE_STYLE}</style></head><body>\
         <h1>Sign-in complete</h1><p>{}</p>\
         <p>You can close this window and return to the terminal.</p></body></html>",
        escape_html(label)
    )
}

/// The page shown when the callback is refused. The reason is escaped; the code and state are
/// never echoed — they would end up in browser history and in any screenshot of the failure.
pub fn failure_page(reason: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Sign-in failed</title><style>{PAGE_STYLE}</style></head><body>\
         <h1>Sign-in failed</h1><p>{}</p>\
         <p>You can close this window and try again from the terminal.</p></body></html>",
        escape_html(reason)
    )
}

/// Split `GET /path?a=1&b=2 HTTP/1.1` into `(method, target)`.
///
/// Pure, so the parser is testable without a socket. A missing or non-HTTP version is refused
/// rather than guessed at: a request line we do not fully recognise gets a 400 from the caller,
/// not best-effort parsing of a shape we have not seen before.
pub fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let mut parts = head.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    let version = parts.next()?;
    if !version.starts_with("HTTP/") {
        return None;
    }
    Some((method, target))
}

/// Query string → pairs, percent-decoding both sides.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| match part.split_once('=') {
            Some((k, v)) => (url_decode(k), url_decode(v)),
            None => (url_decode(part), String::new()),
        })
        .collect()
}

/// An authorization code recovered from text the operator pasted, rather than from the socket.
///
/// The state travels separately because the caller must still compare it against the session's
/// own state. A pasted URL is attacker-influenced input exactly like a loopback request: skipping
/// that comparison would turn "paste your redirect" into a way to redeem a code from somebody
/// else's flow.
pub struct PastedRedirect {
    /// The code to exchange. Never logged; `Debug` reports its length only.
    pub code: String,
    /// The `state` the provider echoed back, if the pasted text carried one.
    pub state: Option<String>,
}

impl std::fmt::Debug for PastedRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PastedRedirect")
            .field("code", &format_args!("<{} bytes>", self.code.len()))
            .field("state_present", &self.state.is_some())
            .finish()
    }
}

/// Recover the authorization code from a redirect the operator pasted by hand.
///
/// This is the fallback for a loopback redirect that never arrived — the browser is on another
/// machine, the port was taken, or the tab was closed. The operator is looking at an address bar
/// that failed to load, so the input may be the whole URL, the query string alone, or just the
/// code value; all three are accepted, because the difference between them is an accident of what
/// they selected, not a different intent.
///
/// Refusals are deliberately specific: a bare code is useless without its state (we cannot verify
/// it), so that case says so instead of failing later with a confusing provider error.
pub fn parse_pasted_redirect(pasted: &str) -> Result<PastedRedirect, String> {
    // A redirect URL is short. Anything longer is not one, and parsing arbitrary input as if it
    // were is how a paste box becomes a denial-of-service.
    const LIMIT: usize = 8 * 1024;

    let trimmed = pasted.trim();
    if trimmed.is_empty() {
        return Err("paste the URL the browser was redirected to, or its code parameter".into());
    }
    if trimmed.len() > LIMIT {
        return Err("that is too long to be a redirect URL".into());
    }
    if trimmed.chars().any(char::is_control) {
        return Err("the pasted value contains control characters".into());
    }

    // Strip scheme and path: keep the query when there is one, otherwise treat the whole value as
    // either a bare query or a bare code.
    let query = match trimmed.split_once('?') {
        Some((_, rest)) => rest.split('#').next().unwrap_or(rest),
        None => trimmed,
    };

    if !query.contains('=') {
        // No parameters at all. A URL without a query is a redirect that carried no code; a bare
        // token is a code the operator copied from somewhere. Distinguish them rather than
        // exchanging a URL as if it were a code.
        if trimmed.contains("://") || trimmed.starts_with('/') || trimmed.contains('/') {
            return Err(
                "that URL carries no authorization code: the sign-in probably did not finish, \
                 so run it again and copy the address the browser lands on"
                    .into(),
            );
        }
        // Some flows report the state in the fragment and are copied as `code#state`, which has no
        // `=` anywhere. Accept it so a correct paste is not rejected for looking odd.
        if let Some((code, state)) = trimmed.split_once('#') {
            if !code.is_empty() && !state.is_empty() {
                return Ok(PastedRedirect {
                    code: code.to_owned(),
                    state: Some(state.to_owned()),
                });
            }
        }
        return Err(
            "that is a bare authorization code with no state parameter, so the anti-forgery \
             check cannot run; paste the full address the browser was redirected to (including \
             everything after the question mark) instead"
                .into(),
        );
    }

    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut description = None;
    for (key, value) in parse_query(query) {
        match key.as_str() {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            "error_description" => description = Some(value),
            _ => {}
        }
    }

    if let Some(error) = error {
        // The provider refused: surface its own words (clipped and escaped where rendered) rather
        // than reporting a missing code, which would send the operator looking in the wrong place.
        return Err(match description {
            Some(description) if !description.is_empty() => {
                format!("the provider refused the sign-in: {error} ({description})")
            }
            _ => format!("the provider refused the sign-in: {error}"),
        });
    }

    let code = code.filter(|value| !value.is_empty()).ok_or_else(|| {
        "the pasted URL has a query but no code parameter; copy the address from the browser's \
         address bar, including everything after the question mark"
            .to_string()
    })?;

    Ok(PastedRedirect { code, state })
}

/// Cap provider-supplied text that will be rendered: 200 characters, on a `char` boundary so
/// multi-byte input cannot panic the truncation.
fn clip(value: &str) -> String {
    const LIMIT: usize = 200;
    if value.chars().count() <= LIMIT {
        return value.to_string();
    }
    let mut out: String = value.chars().take(LIMIT).collect();
    out.push('…');
    out
}

/// A bound loopback listener waiting for the browser to come back.
#[derive(Debug)]
pub struct Loopback {
    listener: TcpListener,
    redirects: RedirectPorts,
    port: u16,
}

async fn bind_dual_stack_localhost(port: u16) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    socket.set_nonblocking(true)?;
    let address = std::net::SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, port, 0, 0);
    socket.bind(&address.into())?;
    socket.listen(128)?;
    let listener: std::net::TcpListener = socket.into();
    TcpListener::from_std(listener)
}

impl Loopback {
    /// Bind the platform's redirect ports **in order**, first one that accepts wins.
    ///
    /// The socket family follows the registered redirect host. For `localhost`, IPv6 loopback
    /// is preferred (with IPv4 fallback), matching browsers that resolve localhost to `::1`.
    ///
    /// `validate_port` is the platform's [`OAuthPlatform::validates_redirect_port`] contract:
    /// refuse to publish a redirect for anything outside the declared set (an ephemeral port
    /// reaching a platform that promised a registered one is a bug, not a convenience).
    pub async fn bind(redirects: RedirectPorts, validate_port: bool) -> Result<Self> {
        let candidates = redirects.candidates();
        let mut failures: Vec<String> = Vec::new();
        for port in candidates.clone() {
            // For registered localhost, make an IPv6 dual-stack socket (IPV6_V6ONLY=false),
            // so browsers resolving to either ::1 or 127.0.0.1 reach this same listener.
            let bind_result = if redirects.host == "localhost" {
                bind_dual_stack_localhost(port).await
            } else {
                TcpListener::bind((redirects.host, port)).await
            };
            match bind_result {
                Ok(listener) => {
                    let actual = listener
                        .local_addr()
                        .map_err(|e| oauth_error(format!("bound socket has no address: {e}")))?
                        .port();
                    if validate_port && !candidates.contains(&actual) {
                        return Err(oauth_error(format!("loopback bound port {actual} is outside the platform's declared redirect set {candidates:?}")));
                    }
                    return Ok(Self {
                        listener,
                        redirects,
                        port: actual,
                    });
                }
                Err(e) => failures.push(format!("port {port}: {e}")),
            }
        }
        Err(oauth_error(format!(
            "could not bind any loopback redirect port ({})",
            failures.join("; ")
        )))
    }

    /// The actual bound port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The redirect URI for **this** bind — built after the port is known, never from a
    /// constant, so the URL given to the browser can only point at a live listener.
    pub fn redirect_uri(&self) -> String {
        self.redirects.redirect_uri(self.port)
    }

    /// Serve until one callback arrives on the redirect path, then verify it.
    ///
    /// Requests on other paths get a 404 page and the listener keeps waiting — browsers fetch
    /// `/favicon.ico` eagerly, and letting that consume the single callback would strand every
    /// login. `state` is checked in constant time ([`state_matches`]); a mismatch discards the
    /// code entirely. Bounded by `timeout` end to end, including the accept loop.
    pub async fn await_callback(
        self,
        expected_state: &str,
        label: &str,
        timeout: Duration,
    ) -> Result<Callback> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let now = tokio::time::Instant::now();
            let Some(remaining) = deadline.checked_duration_since(now) else {
                return Err(oauth_error(
                    "timed out waiting for the browser to return to the loopback redirect",
                ));
            };
            if remaining.is_zero() {
                return Err(oauth_error(
                    "timed out waiting for the browser to return to the loopback redirect",
                ));
            }

            let accepted = tokio::time::timeout(remaining, self.listener.accept()).await;
            let (mut socket, _) = match accepted {
                Err(_) => {
                    return Err(oauth_error(
                        "timed out waiting for the browser to return to the loopback redirect",
                    ))
                }
                Ok(Err(e)) => return Err(Error::Io(e)),
                Ok(Ok(pair)) => pair,
            };

            // Read the request head with the same deadline; a stalled client must not hold the
            // flow open past the timeout.
            let mut head = Vec::with_capacity(1024);
            let mut chunk = [0u8; 1024];
            let head_result: Result<Vec<u8>> = async {
                loop {
                    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if left.is_zero() {
                        return Err(oauth_error("timed out reading the browser callback"));
                    }
                    let read = tokio::time::timeout(left, socket.read(&mut chunk)).await;
                    let n = match read {
                        Err(_) => {
                            return Err(oauth_error("timed out reading the browser callback"))
                        }
                        Ok(Err(e)) => return Err(Error::Io(e)),
                        Ok(Ok(n)) => n,
                    };
                    if n == 0 {
                        break;
                    }
                    head.extend_from_slice(&chunk[..n]);
                    if head.windows(4).any(|w| w == b"\r\n\r\n") || head.len() > 8 * 1024 {
                        break;
                    }
                }
                Ok(head)
            }
            .await;

            let head = match head_result {
                Ok(bytes) => bytes,
                Err(_) => continue, // stalled or reset client: keep accepting
            };
            let head_text = String::from_utf8_lossy(&head).into_owned();
            let Some((method, target)) = parse_request_line(&head_text) else {
                write_page(
                    &mut socket,
                    "400 Bad Request",
                    &failure_page("Malformed request."),
                )
                .await;
                continue;
            };
            if !method.eq_ignore_ascii_case("GET") {
                write_page(
                    &mut socket,
                    "405 Method Not Allowed",
                    &failure_page("Expected GET."),
                )
                .await;
                continue;
            }

            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p, q),
                None => (target, ""),
            };
            if path != self.redirects.path {
                write_page(&mut socket, "404 Not Found", &failure_page("Unknown path.")).await;
                continue;
            }

            let params = parse_query(query);

            // Provider-reported denial arrives as `?error=…`, usually with an
            // `error_description` worth showing the human. Both are attacker-influenced, so
            // they are escaped when the page is built and length-capped so a callback URL
            // cannot smuggle an arbitrarily large body into our response.
            if let Some(error) = params.iter().find(|(k, _)| k == "error") {
                let error_value = clip(&error.1);
                let description = params
                    .iter()
                    .find(|(k, _)| k == "error_description")
                    .map(|(_, v)| clip(v))
                    .filter(|v| !v.is_empty());
                let detail = match &description {
                    Some(description) => format!("{error_value}: {description}"),
                    None => error_value.clone(),
                };
                write_page(
                    &mut socket,
                    "200 OK",
                    &failure_page(&format!("The provider refused the login ({detail}).")),
                )
                .await;
                return Err(oauth_error(format!(
                    "authorization denied by provider: {detail}"
                )));
            }

            let code = params
                .iter()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.clone());
            let returned_state = params
                .iter()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.clone());

            let (Some(code), Some(returned_state)) = (code, returned_state) else {
                write_page(
                    &mut socket,
                    "200 OK",
                    &failure_page("The callback carried no authorization code."),
                )
                .await;
                return Err(oauth_error(
                    "loopback callback arrived without a code and state; nothing to exchange",
                ));
            };

            if !state_matches(expected_state, &returned_state) {
                // Discard the code: accepting it would make the CSRF gate decorative. The page
                // and the error both explain the gate rather than saying "mismatch".
                write_page(
                    &mut socket,
                    "200 OK",
                    &failure_page(
                        "State check failed. This login was started somewhere else or has \
                         already been used; nothing was exchanged. Close this window and start \
                         again from the terminal.",
                    ),
                )
                .await;
                return Err(oauth_error(
                    "callback state mismatch — possible CSRF; the authorization code was \
                     discarded without being exchanged",
                ));
            }

            write_page(&mut socket, "200 OK", &success_page(label)).await;
            return Ok(Callback {
                code,
                state: returned_state,
                port: self.port,
            });
        }
    }
}

/// Write one small HTTP response and close. Write failures mean the browser already left —
/// there is nobody left to tell, so they are not errors.
async fn write_page(socket: &mut TcpStream, status: &str, html: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}

/// What the driver does next after one device-code poll.
#[derive(Debug, Clone, PartialEq)]
pub enum PollPlan {
    /// Sleep this long, then poll again (the deadline has not passed).
    Wait(Duration),
    /// Terminal: stop polling and hand this outcome back. `Pending` never produces this.
    Stop(DevicePoll),
}

/// Device-code pacing: the pure half of RFC 8628 polling.
///
/// Time is *injected* (`advance`), never read from a clock, so the schedule — including
/// `slow_down` back-off and the expiry cutoff — is fully testable offline. The driver
/// ([`drive_device_polls`]) is the only part that sleeps.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceSchedule {
    /// The interval the provider asked for at device-code issuance.
    base: Duration,
    /// The current interval: grows on `slow_down`.
    interval: Duration,
    /// Injected elapsed time since issuance.
    elapsed: Duration,
    /// The device code's lifetime (`expires_in`).
    expires_in: Duration,
}

impl DeviceSchedule {
    /// How far `slow_down` pushes the interval back, per GitHub's device endpoint (its
    /// documented response to `slow_down` is "the interval must be increased by 5 seconds";
    /// RFC 8628 §3.5 likewise requires an increased interval without dictating the step).
    pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

    /// Start from the provider's `interval` and `expires_in`.
    pub fn new(interval: Duration, expires_in: Duration) -> Self {
        Self {
            base: interval,
            interval,
            elapsed: Duration::ZERO,
            expires_in,
        }
    }

    /// The current minimum spacing between polls.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Injected elapsed time.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Move the injected clock forward — the driver calls this after sleeping what `Wait`
    /// asked for; tests call it directly.
    pub fn advance(&mut self, dt: Duration) {
        self.elapsed = self.elapsed.saturating_add(dt);
    }

    /// Whether the device code has outlived `expires_in`.
    pub fn expired(&self) -> bool {
        self.elapsed >= self.expires_in
    }

    /// Apply one poll outcome and return the driver's next action.
    ///
    /// Outcome precedence: a terminal answer (`Success`, `AccessDenied`, `ExpiredToken`,
    /// `Failed`) is honored as given — the server's word beats our arithmetic — while the two
    /// progress answers consult the deadline. `SlowDown` grows the interval *and* restarts the
    /// wait from the new interval; forgetting the growth is how a client gets its poll rate
    /// limited into a stall.
    pub fn apply(&mut self, outcome: &DevicePoll) -> PollPlan {
        match outcome {
            DevicePoll::Success(tokens) => PollPlan::Stop(DevicePoll::Success(tokens.clone())),
            DevicePoll::AccessDenied => PollPlan::Stop(DevicePoll::AccessDenied),
            DevicePoll::ExpiredToken => PollPlan::Stop(DevicePoll::ExpiredToken),
            DevicePoll::Failed { message } => PollPlan::Stop(DevicePoll::Failed {
                message: message.clone(),
            }),
            DevicePoll::Pending => {
                if self.expired() {
                    PollPlan::Stop(DevicePoll::ExpiredToken)
                } else {
                    PollPlan::Wait(self.interval)
                }
            }
            DevicePoll::SlowDown => {
                self.interval = self
                    .interval
                    .saturating_add(Self::SLOW_DOWN_STEP)
                    .max(self.base.saturating_add(Self::SLOW_DOWN_STEP));
                if self.expired() {
                    PollPlan::Stop(DevicePoll::ExpiredToken)
                } else {
                    PollPlan::Wait(self.interval)
                }
            }
        }
    }
}

/// Drive a device flow to completion: poll, sleep per the schedule, stop on a terminal outcome
/// or the overall deadline.
///
/// This is the I/O half — every decision about *when* and *whether* belongs to
/// [`DeviceSchedule`], which the tests exercise without a socket.
pub async fn drive_device_polls(
    platform: &dyn OAuthPlatform,
    authorization: &DeviceAuthorization,
    interval: Duration,
    expires_in: Duration,
) -> Result<DevicePoll> {
    let mut schedule = DeviceSchedule::new(interval, expires_in);
    loop {
        // The first poll happens one interval after issuance, as the device endpoint asks;
        // every later wait uses the schedule's current interval, which `SlowDown` has grown.
        // The injected clock advances only when we actually sleep, so `expired()` tracks the
        // deadline instead of drifting ahead of it.
        // Cap the wait at the code's remaining lifetime: a provider-supplied interval larger
        // than `expires_in` would otherwise park the login for the whole interval. The 1s floor
        // stops a zero interval from becoming a hot poll loop against the provider.
        let remaining = expires_in.saturating_sub(schedule.elapsed());
        let wait = schedule
            .interval()
            .min(remaining)
            .max(Duration::from_secs(1));
        tokio::time::sleep(wait).await;
        schedule.advance(wait);
        if schedule.expired() {
            return Ok(DevicePoll::ExpiredToken);
        }

        let outcome = platform.device_poll(authorization).await?;
        match schedule.apply(&outcome) {
            // Next iteration sleeps `schedule.interval()`, which is exactly the Wait value
            // (Pending keeps it, SlowDown grew it) — advancing here would double-count.
            PollPlan::Wait(_) => {}
            PollPlan::Stop(final_outcome) => return Ok(final_outcome),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::net::Ipv4Addr;
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;

    // ---------------------------------------------------------------- pasted redirects

    /// The whole point of the fallback: the exact text in a browser's address bar, after the
    /// loopback page failed to load, still yields the code that completes the sign-in.
    #[test]
    fn a_full_redirect_url_yields_its_code_and_state() {
        let pasted = "http://localhost:1455/auth/callback?code=abc123&state=st-9";
        let parsed = parse_pasted_redirect(pasted).expect("a full URL must parse");
        assert_eq!(parsed.code, "abc123");
        assert_eq!(parsed.state.as_deref(), Some("st-9"));
    }

    /// People select whichever part they can see. The query string alone means the same thing.
    #[test]
    fn a_bare_query_string_parses_the_same_way() {
        let parsed = parse_pasted_redirect("?code=xyz&state=q1").expect("a query must parse");
        assert_eq!(parsed.code, "xyz");
        assert_eq!(parsed.state.as_deref(), Some("q1"));
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        let parsed =
            parse_pasted_redirect("  http://127.0.0.1:1455/auth/callback?code=c&state=s\n")
                .expect("trimmed input must parse");
        assert_eq!(parsed.code, "c");
    }

    /// Google and OpenAI both report a refusal in the redirect rather than by failing. Reporting
    /// the provider's own words sends the operator to the real problem; "no code" would not.
    #[test]
    fn a_refusal_reports_the_provider_reason() {
        let error = parse_pasted_redirect(
            "http://localhost:1455/auth/callback?error=access_denied&error_description=user+cancelled",
        )
        .expect_err("a refusal is an error");
        assert!(error.contains("access_denied"), "{error}");
        assert!(error.contains("user cancelled"), "{error}");
    }

    /// A redirect that carried no code at all is a sign-in that did not finish, and saying so is
    /// more useful than complaining about a parser.
    #[test]
    fn a_url_without_a_query_is_refused_as_incomplete() {
        let error = parse_pasted_redirect("http://localhost:1455/auth/callback")
            .expect_err("no code means no redirect to use");
        assert!(error.contains("did not finish"), "{error}");
    }

    #[test]
    fn a_query_without_a_code_is_refused() {
        let error = parse_pasted_redirect("?state=only").expect_err("no code parameter");
        assert!(error.contains("no code parameter"), "{error}");
    }

    /// A bare code cannot be checked against the session's state, so the paste is refused with an
    /// explanation instead of being handed on with `state: None` for a caller to overlook. A code
    /// without a state is a login-CSRF hole the moment anyone forgets the `None` branch.
    #[test]
    fn a_bare_code_is_refused_because_it_has_no_state_to_check() {
        let error = parse_pasted_redirect("abc123").expect_err("a bare code has no state to check");
        assert!(error.contains("bare authorization code"), "{error}");
    }

    /// `code#state` is what some flows put in the fragment, and it contains no `=`, so it must be
    /// recognised before the bare-code branch swallows it whole.
    #[test]
    fn a_code_and_state_joined_by_a_hash_parse() {
        let parsed = parse_pasted_redirect("abc#st-1").expect("code#state must parse");
        assert_eq!(parsed.code, "abc");
        assert_eq!(parsed.state.as_deref(), Some("st-1"));
    }

    #[test]
    fn empty_input_is_refused() {
        assert!(parse_pasted_redirect("   ").is_err());
    }

    /// Rendered text is one thing; a paste box must not accept newlines or escapes that could
    /// reach a header or a log line.
    #[test]
    fn control_characters_are_refused() {
        assert!(parse_pasted_redirect("code=a\u{0}b").is_err());
        assert!(parse_pasted_redirect("code=a\r\nX-Injected: 1").is_err());
    }

    #[test]
    fn an_oversized_paste_is_refused() {
        let huge = format!("?code={}", "a".repeat(9 * 1024));
        let error = parse_pasted_redirect(&huge).expect_err("too long");
        assert!(error.contains("too long"), "{error}");
    }

    /// A code is a credential for the length of a sign-in, so it must not reach a log line.
    #[test]
    fn the_debug_form_does_not_print_the_code() {
        let parsed = PastedRedirect {
            code: "super-secret-code".into(),
            state: Some("s".into()),
        };
        let shown = format!("{parsed:?}");
        assert!(!shown.contains("super-secret-code"), "{shown}");
        assert!(shown.contains("17 bytes"), "{shown}");
    }

    /// A device authorization for driver tests; only the fields the driver reads matter.
    fn test_authorization() -> DeviceAuthorization {
        DeviceAuthorization {
            device_code: "device-code".into(),
            user_code: "TEST-CODE".into(),
            verification_uri: "https://example.test/device".into(),
            verification_uri_complete: None,
            expires_in: Duration::from_secs(60),
            interval: Duration::from_secs(1),
        }
    }
    use crate::oauth::{AccountIdentity, Pkce, TokenSet};

    // ----- encoding -------------------------------------------------------

    #[test]
    fn url_encode_keeps_unreserved_and_escapes_everything_else() {
        assert_eq!(url_encode("abcXYZ09-._~"), "abcXYZ09-._~");
        // The form-encoding trap: a raw `+` would be read back as a space.
        assert_eq!(url_encode("a+b"), "a%2Bb");
        assert_eq!(url_encode("x=y&z"), "x%3Dy%26z");
        assert_eq!(url_encode("héllo"), "h%C3%A9llo");
        assert_eq!(url_encode(" "), "%20");
    }

    #[test]
    fn url_decode_inverts_encode_and_tolerates_garbage() {
        for value in ["plain", "a+b", "x=y&z", "héllo/ünïcode", "% weird"] {
            assert_eq!(
                url_decode(&url_encode(value)),
                value,
                "round trip for {value:?}"
            );
        }
        assert_eq!(url_decode("a%20b"), "a b");
        assert_eq!(url_decode("a+b"), "a b");
        // Invalid escapes stay verbatim instead of panicking or swallowing bytes.
        assert_eq!(url_decode("100%zz"), "100%zz");
        assert_eq!(url_decode("%2"), "%2");
    }

    #[test]
    fn form_body_joins_encoded_pairs_in_order() {
        assert_eq!(
            form_body(&[
                ("grant_type", "authorization_code"),
                ("code", "a+b/c=d"),
                ("scope", "openid profile"),
            ]),
            "grant_type=authorization_code&code=a%2Bb%2Fc%3Dd&scope=openid%20profile"
        );
        assert_eq!(form_body(&[]), "");
    }

    // ----- state ----------------------------------------------------------

    #[test]
    fn state_is_43_urlsafe_chars_and_fresh_every_time() {
        let state = new_state();
        assert_eq!(state.len(), 43, "32 bytes encode to 43 base64url chars");
        assert!(state
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert!(!state.contains('='));
        assert_ne!(new_state(), state, "states must not repeat");
    }

    #[test]
    fn state_matches_only_the_exact_value() {
        let expected = new_state();
        assert!(state_matches(&expected, &expected));
        // Mismatch, empty, and length games are all refusals.
        assert!(!state_matches(&expected, &new_state()));
        assert!(!state_matches(&expected, ""));
        assert!(!state_matches("", &expected));
        assert!(!state_matches(&expected, &format!("{expected}X")));
        assert!(!state_matches(&expected, &expected[..42]));
        // A single flipped byte anywhere — including the last, where a lazy compare would have
        // already returned true for the first 42 bytes — must fail.
        let mut tampered = expected.clone();
        let last = tampered.pop().unwrap();
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert!(!state_matches(&expected, &tampered));
    }

    // ----- request parsing and pages --------------------------------------

    #[test]
    fn request_line_parses_and_refuses_garbage() {
        assert_eq!(
            parse_request_line("GET /auth/callback?code=x HTTP/1.1\r\nHost: h\r\n\r\n"),
            Some(("GET", "/auth/callback?code=x"))
        );
        assert_eq!(parse_request_line("get /p HTTP/1.1"), Some(("get", "/p")));
        assert_eq!(parse_request_line("GET /p"), None);
        assert_eq!(parse_request_line("GET /p FTP/1.1"), None);
        assert_eq!(parse_request_line(""), None);
        assert_eq!(parse_request_line("\r\n"), None);
    }

    #[test]
    fn query_pairs_are_percent_decoded() {
        assert_eq!(
            parse_query("code=a%2Bb&state=abc&error=access_denied"),
            vec![
                ("code".to_string(), "a+b".to_string()),
                ("state".to_string(), "abc".to_string()),
                ("error".to_string(), "access_denied".to_string()),
            ]
        );
        // A bare key and an empty value both parse; neither panics.
        assert_eq!(
            parse_query("flag"),
            vec![("flag".to_string(), String::new())]
        );
        assert_eq!(parse_query(""), Vec::<(String, String)>::new());
        assert_eq!(parse_query("="), vec![(String::new(), String::new())]);
    }

    #[test]
    fn escape_html_neutralises_injected_markup() {
        assert_eq!(
            escape_html("<script>alert(1)</script>"),
            "&lt;script&gt;alert(1)&lt;/script&gt;"
        );
        assert_eq!(
            escape_html("a & b \"c\" 'd'"),
            "a &amp; b &quot;c&quot; &#39;d&#39;"
        );
        // Control bytes (including CR/LF, which could split the HTML) become spaces — each one,
        // so CRLF collapses to two spaces rather than being silently swallowed.
        assert_eq!(escape_html("a\r\nb\tc"), "a  b c");
    }

    #[test]
    fn pages_are_self_contained_and_never_echo_secrets() {
        let success = success_page("OpenAI <Codex>");
        assert!(
            success.contains("OpenAI &lt;Codex&gt;"),
            "label must be escaped"
        );
        assert!(success.contains("<style>"), "self-contained styling");
        assert!(
            !success.contains("<script"),
            "no scripts on the callback page"
        );
        assert!(
            !success.contains("http://") && !success.contains("https://"),
            "no external resources"
        );

        let injected = "</style><script>steal()</script>";
        let failure = failure_page(injected);
        assert!(!failure.contains("<script>steal"), "reason must be escaped");
        assert!(failure.contains("&lt;/style&gt;"));
    }

    // ----- device schedule (pure, no I/O) ---------------------------------

    #[test]
    fn schedule_starts_from_the_provider_values() {
        let mut schedule = DeviceSchedule::new(Duration::from_secs(5), Duration::from_secs(900));
        assert_eq!(schedule.interval(), Duration::from_secs(5));
        assert_eq!(schedule.elapsed(), Duration::ZERO);
        assert!(!schedule.expired());
        assert_eq!(
            schedule.apply(&DevicePoll::Pending),
            PollPlan::Wait(Duration::from_secs(5)),
            "pending polls wait the current interval"
        );
    }

    #[test]
    fn slow_down_grows_the_interval_and_that_growth_is_what_waits() {
        let mut schedule = DeviceSchedule::new(Duration::from_secs(5), Duration::from_secs(900));
        let plan = schedule.apply(&DevicePoll::SlowDown);
        let grown = Duration::from_secs(10); // 5 s base + GitHub's documented +5 s
        assert_eq!(
            schedule.interval(),
            grown,
            "slow_down must increase the interval"
        );
        assert_eq!(
            plan,
            PollPlan::Wait(grown),
            "the wait must use the grown interval"
        );
        assert_ne!(
            schedule.apply(&DevicePoll::SlowDown),
            plan,
            "second slow_down grows again"
        );
        assert_eq!(schedule.interval(), Duration::from_secs(15));
        // Pending after the back-off keeps the grown value rather than snapping back.
        assert_eq!(
            schedule.apply(&DevicePoll::Pending),
            PollPlan::Wait(Duration::from_secs(15))
        );
    }

    #[test]
    fn expired_device_code_stops_the_progress_outcomes_only() {
        let mut schedule = DeviceSchedule::new(Duration::from_secs(5), Duration::from_secs(10));
        schedule.advance(Duration::from_secs(11));
        assert!(schedule.expired());
        assert_eq!(
            schedule.apply(&DevicePoll::Pending),
            PollPlan::Stop(DevicePoll::ExpiredToken),
            "pending past the deadline is expiry"
        );
        assert_eq!(
            schedule.apply(&DevicePoll::SlowDown),
            PollPlan::Stop(DevicePoll::ExpiredToken),
            "slow_down past the deadline is expiry too"
        );
        // The server's word beats our clock: an approval that arrives at the wire wins.
        assert_eq!(
            schedule.apply(&DevicePoll::Success(TokenSet::new("at"))),
            PollPlan::Stop(DevicePoll::Success(TokenSet::new("at")))
        );
        assert_eq!(
            schedule.apply(&DevicePoll::AccessDenied),
            PollPlan::Stop(DevicePoll::AccessDenied)
        );
    }

    #[test]
    fn only_pending_and_slow_down_continue() {
        assert!(DevicePoll::Pending.is_continue());
        assert!(DevicePoll::SlowDown.is_continue());
        for terminal in [
            DevicePoll::ExpiredToken,
            DevicePoll::AccessDenied,
            DevicePoll::Failed {
                message: "x".to_string(),
            },
            DevicePoll::Success(TokenSet::new("at")),
        ] {
            assert!(!terminal.is_continue(), "{terminal:?} must be terminal");
        }
    }

    // ----- the driver (I/O half) with a scripted platform ------------------

    struct FakeDevice {
        script: Mutex<VecDeque<DevicePoll>>,
        polls: Mutex<usize>,
    }

    impl FakeDevice {
        fn new(script: Vec<DevicePoll>) -> Self {
            Self {
                script: Mutex::new(script.into()),
                polls: Mutex::new(0),
            }
        }

        fn polls(&self) -> usize {
            *self.polls.lock().unwrap()
        }
    }

    #[async_trait]
    impl OAuthPlatform for FakeDevice {
        fn id(&self) -> &'static str {
            "fake-device"
        }
        fn label(&self) -> &'static str {
            "Fake device platform"
        }
        fn description(&self) -> &'static str {
            "Test double that replays a scripted device poll."
        }
        fn begins_with_loopback(&self) -> bool {
            false
        }
        fn redirect_ports(&self) -> RedirectPorts {
            RedirectPorts {
                host: "",
                primary: 0,
                fallback: None,
                path: "",
            }
        }
        fn requires_pkce(&self) -> bool {
            false
        }
        fn validates_redirect_port(&self) -> bool {
            false
        }
        fn authorization_url(
            &self,
            _state: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> Result<String> {
            Err(oauth_error("device platform"))
        }
        async fn exchange_code(
            &self,
            _code: &str,
            _pkce: Option<&Pkce>,
            _port: u16,
        ) -> Result<TokenSet> {
            Err(oauth_error("device platform"))
        }
        async fn refresh(&self, _refresh_token: &str) -> Result<TokenSet> {
            Err(oauth_error("device platform"))
        }
        async fn account_identity(&self, _tokens: &TokenSet) -> Result<AccountIdentity> {
            Err(oauth_error("device platform"))
        }
        fn apply_credential(
            &self,
            _tokens: &TokenSet,
        ) -> Vec<(http::HeaderName, http::HeaderValue)> {
            Vec::new()
        }
        fn fingerprint_headers(&self) -> &'static [(&'static str, &'static str)] {
            &[("x-fake", "1")]
        }
        fn refresh_lead(&self) -> Duration {
            Duration::from_secs(60)
        }
        async fn device_poll(&self, _authorization: &DeviceAuthorization) -> Result<DevicePoll> {
            *self.polls.lock().unwrap() += 1;
            let mut script = self.script.lock().unwrap();
            Ok(script.pop_front().unwrap_or_else(|| DevicePoll::Failed {
                message: "script exhausted".to_string(),
            }))
        }
    }

    #[tokio::test]
    async fn driver_polls_through_pending_to_success() {
        // No `SlowDown` here on purpose: the step is an absolute 5 s (GitHub's documented
        // "+5 seconds"), so a back-off in a millisecond-scale test would itself take 5 s of
        // wall time. Back-off pacing is covered by the pure schedule tests above; what this
        // test pins is the driver honouring `Wait` and returning the terminal outcome.
        let fake = FakeDevice::new(vec![
            DevicePoll::Pending,
            DevicePoll::Success(TokenSet::new("minted")),
        ]);
        let outcome = drive_device_polls(
            &fake,
            &test_authorization(),
            Duration::from_millis(5),
            Duration::from_secs(5),
        )
        .await
        .expect("drive");
        assert_eq!(outcome, DevicePoll::Success(TokenSet::new("minted")));
        assert_eq!(fake.polls(), 2, "two scripted outcomes, two polls");
    }

    #[tokio::test]
    async fn driver_stops_on_refusal_without_waiting_out_the_deadline() {
        let fake = FakeDevice::new(vec![DevicePoll::AccessDenied]);
        let started = std::time::Instant::now();
        let outcome = drive_device_polls(
            &fake,
            &test_authorization(),
            Duration::from_millis(5),
            Duration::from_secs(30),
        )
        .await
        .expect("drive");
        assert_eq!(outcome, DevicePoll::AccessDenied);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a refusal must end the flow immediately, not near the 30 s deadline"
        );
        assert_eq!(fake.polls(), 1);
    }

    #[tokio::test]
    async fn driver_reports_expiry_when_the_script_never_approves() {
        // All-Pending script: the deadline (25 ms) must cut it off rather than poll forever.
        let fake = FakeDevice::new(vec![DevicePoll::Pending; 1000]);
        let outcome = drive_device_polls(
            &fake,
            &test_authorization(),
            Duration::from_millis(5),
            Duration::from_millis(25),
        )
        .await
        .expect("drive");
        assert_eq!(outcome, DevicePoll::ExpiredToken);
        assert!(
            fake.polls() <= 6,
            "polled {} times before expiry",
            fake.polls()
        );
    }

    // ----- loopback listener (hermetic: 127.0.0.1 only) -------------------

    fn redirects_for_test(path: &'static str) -> RedirectPorts {
        RedirectPorts {
            host: "127.0.0.1",
            primary: 0,
            fallback: None,
            path,
        }
    }

    async fn http_get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .expect("connect");
        stream
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: test\r\n\r\n").as_bytes())
            .await
            .expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).await.expect("read");
        response
    }

    #[tokio::test]
    async fn loopback_serves_404_then_accepts_the_real_callback() {
        let listener = Loopback::bind(redirects_for_test("/cb"), false)
            .await
            .expect("bind");
        let port = listener.port();
        assert_ne!(port, 0, "an ephemeral bind still reports its real port");
        assert_eq!(
            listener.redirect_uri(),
            format!("http://127.0.0.1:{port}/cb")
        );

        let state = new_state();
        let expected = state.clone();
        let server = tokio::spawn(async move {
            listener
                .await_callback(&expected, "Test", Duration::from_secs(5))
                .await
        });

        // A browser's eager favicon request must not consume the single callback.
        let favicon = http_get(port, "/favicon.ico").await;
        assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
        assert!(!favicon.contains("THE-CODE-VALUE"));

        let callback = http_get(port, &format!("/cb?code=THE-CODE-VALUE&state={state}")).await;
        assert!(callback.starts_with("HTTP/1.1 200"), "{callback}");
        assert!(callback.contains("Sign-in complete"));
        assert!(
            !callback.contains("THE-CODE-VALUE"),
            "the code must never be echoed into the page (browser history, screenshots)"
        );
        assert!(callback.contains("no-store"));

        let delivered = server.await.expect("join").expect("callback");
        assert_eq!(delivered.code, "THE-CODE-VALUE");
        assert_eq!(delivered.state, state);
        assert_eq!(delivered.port, port);
    }

    #[tokio::test]
    async fn loopback_refuses_a_state_mismatch_and_discards_the_code() {
        let listener = Loopback::bind(redirects_for_test("/cb"), false)
            .await
            .expect("bind");
        let port = listener.port();
        let expected = new_state();
        let server = tokio::spawn(async move {
            listener
                .await_callback(&expected, "Test", Duration::from_secs(5))
                .await
        });

        // Attacker-supplied code with a forged state: CSRF gate must discard it.
        let page = http_get(port, "/cb?code=ATTACKER-CODE&state=forged").await;
        assert!(page.starts_with("HTTP/1.1 200"));
        assert!(
            page.contains("State check failed"),
            "the page explains the gate"
        );
        assert!(
            !page.contains("ATTACKER-CODE"),
            "discarded code is not echoed"
        );

        let err = server.await.expect("join").expect_err("mismatch must fail");
        let message = err.to_string();
        assert!(message.contains("state mismatch"), "{message}");
        assert!(
            message.contains("CSRF"),
            "the error names the threat: {message}"
        );
    }

    #[tokio::test]
    async fn loopback_surfaces_a_provider_denial_as_an_error() {
        let listener = Loopback::bind(redirects_for_test("/cb"), false)
            .await
            .expect("bind");
        let port = listener.port();
        let expected = new_state();
        let server = tokio::spawn(async move {
            listener
                .await_callback(&expected, "Test", Duration::from_secs(5))
                .await
        });

        let page = http_get(port, "/cb?error=access_denied&error_description=no+thanks").await;
        assert!(page.contains("refused the login"));
        assert!(page.contains("no thanks"), "provider text arrives decoded");
        let err = server.await.expect("join").expect_err("denial must fail");
        assert!(err.to_string().contains("access_denied"));
    }

    #[tokio::test]
    async fn loopback_gives_up_at_the_deadline() {
        let listener = Loopback::bind(redirects_for_test("/cb"), false)
            .await
            .expect("bind");
        let expected = new_state();
        let started = std::time::Instant::now();
        let err = listener
            .await_callback(&expected, "Test", Duration::from_millis(50))
            .await
            .expect_err("nobody connects");
        assert!(err.to_string().contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bound by the given timeout"
        );
    }

    #[tokio::test]
    async fn bind_falls_back_to_the_next_port_when_the_first_is_busy() {
        // Occupy a port, then declare it primary with an ephemeral fallback.
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("occupy");
        let busy_port = busy.local_addr().unwrap().port();
        let redirects = RedirectPorts {
            host: "127.0.0.1",
            primary: busy_port,
            fallback: Some(0),
            path: "/cb",
        };
        // No port promise (`validate_port = false`): an OS-assigned fallback is acceptable and
        // — crucially — the redirect is published for the port that was actually taken.
        let listener = Loopback::bind(redirects, false).await.expect("fallback");
        assert_ne!(
            listener.port(),
            busy_port,
            "primary was busy, fallback used"
        );
        assert!(listener
            .redirect_uri()
            .contains(&format!(":{}/cb", listener.port())));
    }

    #[tokio::test]
    async fn bind_never_silently_uses_an_arbitrary_port_when_the_port_is_pinned() {
        // docs/10 §8: primary → registered fallback → *fail naming the port*. A platform that
        // promised fixed ports must not degrade into an arbitrary ephemeral one, because the
        // redirect the browser is sent to would no longer match the registered set.
        let busy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("occupy");
        let busy_port = busy.local_addr().unwrap().port();
        let redirects = RedirectPorts {
            host: "127.0.0.1",
            primary: busy_port,
            fallback: Some(0),
            path: "/cb",
        };
        let err = Loopback::bind(redirects, true)
            .await
            .expect_err("must refuse");
        assert!(err
            .to_string()
            .contains("outside the platform's declared redirect set"));
    }

    #[tokio::test]
    async fn bind_names_every_port_it_could_not_take() {
        let busy_a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a");
        let busy_b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("b");
        let a = busy_a.local_addr().unwrap().port();
        let b = busy_b.local_addr().unwrap().port();
        let redirects = RedirectPorts {
            host: "127.0.0.1",
            primary: a,
            fallback: Some(b),
            path: "/cb",
        };
        let err = Loopback::bind(redirects, true)
            .await
            .expect_err("both busy");
        let message = err.to_string();
        assert!(message.contains(&a.to_string()), "names port A: {message}");
        assert!(message.contains(&b.to_string()), "names port B: {message}");
    }

    #[tokio::test]
    async fn bind_refuses_a_port_outside_the_declared_set_when_validation_is_on() {
        // `validate_port` with an ephemeral primary: the OS-assigned port is not in the
        // declared set, which is exactly what the check exists to catch.
        let redirects = RedirectPorts {
            host: "127.0.0.1",
            primary: 0,
            fallback: None,
            path: "/cb",
        };
        let err = Loopback::bind(redirects, true).await.expect_err("validate");
        assert!(err
            .to_string()
            .contains("outside the platform's declared redirect set"));
    }
}
