//! Upstream proxies: URL parsing, CONNECT/handshake connectors, and pool selection.
//!
//! # The seam this module sits in, and why it is a module
//!
//! A proxied request has one extra hop in the middle of the only slow part of a relay request: the
//! connect. `upstream.rs` currently opens a TCP socket and wraps it in TLS. This module owns
//! everything between "we know which proxy" and "there is a byte stream to `host:port`", and nothing
//! else. It never decides *whether* to proxy (that is `Config::effective_proxy` plus
//! [`choose`]), never owns an HTTP client, and never touches a request body.
//!
//! ```text
//!   choose(&config.proxies, strategy, &state)   ->  &ProxyConfig
//!   ProxyEndpoint::from_config(cfg, vault_pw)   ->  ProxyEndpoint
//!   open_tunnel(&endpoint, &target, &limits, tls) -> Tunnel   (a stream dialled *through* the proxy)
//!   <origin TLS over that stream>               ->  hyper client
//! ```
//!
//! The relay's expected wiring is: resolve the credential, build the endpoint, open the tunnel, wrap
//! the tunnel in TLS for the *origin* host, hand it to hyper as `TokioIo::new(tunnel)`. The hop to
//! the proxy itself is TLS only for `https://` proxies, and that TLS config is passed in by the
//! caller because this module does not own the trust store (see [`open_tunnel`]).
//!
//! # Why the handshakes are hand-written
//!
//! There is no `hyper` for `CONNECT`, no dependency-free SOCKS client, and the alternatives
//! (`reqwest`, `hyper-proxy`) either buffer, own their pool, or drag in a second TLS stack. What is
//! left is about 300 lines of framing, which is less code than configuring a substitute correctly
//! for the four timeouts the relay already has.
//!
//! Every read here is bounded and every wait is inside a timeout, for the same reason: a proxy is
//! the one peer that can sit between the relay and every provider at once. A proxy that accepts the
//! connection and then says nothing must cost this request one timeout, not the process.
//!
//! # Credentials
//!
//! `ProxyConfig` never carries the password. It carries a [`ProxyConfig::secret_ref`] into the
//! encrypted vault, so this module's caller resolves the secret and hands it in; [`ProxyAuth`]
//! documents the refusal to accept a value that would be spliced into a header, and its `Debug`
//! redacts the password so a log line cannot leak it.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use mr_core::config::{ProxyConfig, ProxyStrategy};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// The selection counters that go with a pool of [`ProxyConfig`]s.
///
/// Re-exported so a caller can hold `proxy::ProxySelectionState` without reaching into
/// [`crate::state`]; the type itself lives beside [`crate::state::RouteTable`] because it is the
/// same kind of thing — the one piece of mutable routing state per config generation.
pub use crate::state::{ProxyLease, ProxySelectionState};

/// Default for [`TunnelLimits::connect_timeout`].
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default for [`TunnelLimits::reply_timeout`].
pub const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// Default for [`TunnelLimits::max_reply_bytes`].
///
/// 8 KiB is generous for a status line plus the handful of headers a proxy returns, and small
/// enough that a proxy which streams an endless response header cannot make the relay allocate.
pub const DEFAULT_MAX_REPLY_BYTES: usize = 8 * 1024;

/// The bounds every handshake in this module runs under.
///
/// Passed as a value rather than read from globals so a test can shrink the timeout to a
/// millisecond and so the caller can align these numbers with `latency.*` in the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TunnelLimits {
    /// How long the TCP connect to the proxy may take.
    pub connect_timeout: Duration,
    /// How long the proxy may take to answer any single step of a handshake.
    ///
    /// Per step, not for the whole handshake: a SOCKS5 handshake is three round trips, and a global
    /// budget would make the last one fail whenever the first two were slow.
    pub reply_timeout: Duration,
    /// How many bytes of a proxy reply are read before the reply is declared hostile.
    pub max_reply_bytes: usize,
}

impl Default for TunnelLimits {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
            max_reply_bytes: DEFAULT_MAX_REPLY_BYTES,
        }
    }
}

/// The URL schemes a proxy may be configured with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProxyScheme {
    /// Plaintext HTTP `CONNECT` tunnel.
    Http,
    /// HTTP `CONNECT` tunnel inside TLS to the proxy itself.
    Https,
    /// SOCKS4, with the SOCKS4a hostname extension for non-IPv4 targets.
    Socks4,
    /// SOCKS5 with the target hostname resolved by the relay.
    Socks5,
    /// SOCKS5 with the target hostname resolved by the proxy.
    Socks5h,
}

impl ProxyScheme {
    /// Every scheme this module can dial, for error messages.
    ///
    /// Deliberately a superset of [`ProxyConfig::SUPPORTED_SCHEMES`]: `socks5h` is diallable here,
    /// so the day config validation learns the name it needs no second change. Until then a
    /// `socks5h://` proxy URL is rejected by config validation before it ever reaches this module.
    pub const NAMES: [&'static str; 5] = ["http", "https", "socks4", "socks5", "socks5h"];

    /// Parse one scheme token, case-insensitively.
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "http" => Some(Self::Http),
            "https" => Some(Self::Https),
            "socks4" => Some(Self::Socks4),
            "socks5" => Some(Self::Socks5),
            "socks5h" => Some(Self::Socks5h),
            _ => None,
        }
    }

    /// The canonical lowercase name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Socks4 => "socks4",
            Self::Socks5 => "socks5",
            Self::Socks5h => "socks5h",
        }
    }

    /// Whether the hop to the proxy itself is TLS.
    pub const fn is_tls(self) -> bool {
        matches!(self, Self::Https)
    }

    /// Whether the proxy resolves the target hostname rather than the relay.
    pub const fn resolves_at_proxy(self) -> bool {
        matches!(self, Self::Socks5h)
    }
}

/// A username/password pair for a proxy.
///
/// # Why construction is fallible
///
/// The username goes into an HTTP header for `http(s)` proxies and into a length-prefixed field for
/// SOCKS5. A `\r\n` in it would terminate the `CONNECT` request early and let the rest of the
/// username be read as a header — header injection into a request the relay never otherwise lets
/// the operator shape — and a NUL would do the same to a SOCKS4 userid. Both are rejected here, at
/// the one place a credential is built, rather than at each of the three places it is written.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyAuth {
    username: String,
    password: String,
}

impl ProxyAuth {
    /// Build a credential, refusing anything that could break out of its wire field.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, ProxyUrlError> {
        let username = username.into();
        let password = password.into();
        if username.is_empty() {
            return Err(ProxyUrlError::EmptyUsername);
        }
        for (field, value) in [("username", &username), ("password", &password)] {
            if value.chars().any(|c| c.is_control()) {
                return Err(ProxyUrlError::AuthControlCharacter { field });
            }
        }
        Ok(Self { username, password })
    }

    /// The username. Not a secret, which is why `ProxyConfig` stores it in the clear.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The password. Never logged, never serialised.
    pub fn password(&self) -> &str {
        &self.password
    }

    /// `Basic base64(user:pass)`, the value of `Proxy-Authorization`.
    fn basic_authorization(&self) -> String {
        let mut plain = String::with_capacity(self.username.len() + self.password.len() + 1);
        plain.push_str(&self.username);
        plain.push(':');
        plain.push_str(&self.password);
        format!("Basic {}", base64(plain.as_bytes()))
    }
}

impl fmt::Debug for ProxyAuth {
    /// Redacts the password. A `Debug` that prints a credential is a credential in a log file, and
    /// the whole point of `ProxyConfig::secret_ref` is that the password never lands there.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A parsed proxy endpoint: scheme, host, port, and an optional credential.
///
/// Built from a URL that the operator wrote or from a [`ProxyConfig`], never from request data, so
/// the fields here are trusted once they have been validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyEndpoint {
    /// Which protocol the hop speaks.
    pub scheme: ProxyScheme,
    /// Hostname or IP literal, without IPv6 brackets.
    pub host: String,
    /// Port. There is no default: a proxy URL without a port is rejected, matching
    /// `ProxyConfig::validate`.
    pub port: u16,
    /// Credential, when the URL carried `user:pass@` or the config named a username.
    pub auth: Option<ProxyAuth>,
}

impl ProxyEndpoint {
    /// Parse `scheme://[user:pass@]host:port`.
    ///
    /// The userinfo is taken **verbatim**: `%41` stays `%41`. Decoding it would mean deciding what a
    /// decoded `@` or `\r` means, and every answer to that is an injection bug waiting for the one
    /// password that contains the wrong byte. An operator whose password contains `@`, `/`, `?` or
    /// `#` should use `username` + `secret_ref` in the config instead, which has no delimiter
    /// problem at all.
    pub fn parse(url: &str) -> Result<Self, ProxyUrlError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(ProxyUrlError::Empty);
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| ProxyUrlError::MissingScheme {
                expected: ProxyScheme::NAMES.join(", "),
            })?;
        if scheme.trim().is_empty() {
            return Err(ProxyUrlError::MissingScheme {
                expected: ProxyScheme::NAMES.join(", "),
            });
        }
        let scheme =
            ProxyScheme::parse(scheme).ok_or_else(|| ProxyUrlError::UnsupportedScheme {
                scheme: scheme.to_ascii_lowercase(),
                expected: ProxyScheme::NAMES.join(", "),
            })?;

        // A proxy URL has no request target. A single trailing `/` is accepted because operators
        // paste it from a browser; anything else would be silently dropped, and a silently dropped
        // `?` is a credential that never reaches the proxy.
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if let Some(bad) = rest.find(['/', '?', '#']) {
            // Do not put the original URL in this error: it may contain `user:password@`, and a
            // validation failure is exactly when an operator is likely to paste the error into a
            // log or bug report.
            return Err(ProxyUrlError::NotAnEndpoint { at: bad });
        }

        let (userinfo, authority) = match rest.rsplit_once('@') {
            Some((userinfo, authority)) => (Some(userinfo), authority),
            None => (None, rest),
        };
        let (host, port) = parse_authority(authority, None)?;
        let auth = match userinfo {
            None => None,
            Some(userinfo) => {
                // No colon means "username with an empty password", which is legal in a URL and
                // which a proxy that only wants a userid (SOCKS4) actually needs.
                let (user, password) = match userinfo.split_once(':') {
                    Some((user, password)) => (user, password),
                    None => (userinfo, ""),
                };
                Some(ProxyAuth::new(user, password)?)
            }
        };
        Ok(Self {
            scheme,
            host,
            port,
            auth,
        })
    }

    /// Build an endpoint from a config entry plus the already-resolved vault secret.
    ///
    /// The config's `username` wins over any userinfo in the URL, because a username in the clear
    /// next to a `secret_ref` must be the username that belongs to that secret. A configured
    /// username with no resolved secret is an **error**, not an unauthenticated dial: the vault can
    /// be locked, and egressing directly through an open proxy in that state is exactly the kind of
    /// silent downgrade a proxy is supposed to prevent.
    pub fn from_config(proxy: &ProxyConfig, password: Option<&str>) -> Result<Self, ProxyUrlError> {
        let endpoint = Self::parse(&proxy.url)?;
        endpoint.with_auth(proxy.username.as_deref(), password, &proxy.id)
    }

    /// Build an endpoint from a compiled proxy plan plus the already-resolved vault secret.
    ///
    /// The plan is what the routing snapshot carries, so this is the shape the request path has
    /// when it resolves a proxy for an account. The rules are the ones
    /// [`ProxyEndpoint::from_config`] documents, because the plan is a compiled copy of the same
    /// configuration.
    pub fn from_plan(
        plan: &mr_core::snapshot::ProxyPlan,
        password: Option<&str>,
    ) -> Result<Self, ProxyUrlError> {
        let endpoint = Self::parse(&plan.url)?;
        endpoint.with_auth(plan.username.as_deref(), password, &plan.id)
    }

    /// Attach the credential, refusing the shapes that would mean an unauthenticated dial.
    fn with_auth(
        mut self,
        username: Option<&str>,
        password: Option<&str>,
        id: &str,
    ) -> Result<Self, ProxyUrlError> {
        match (username, password) {
            // SOCKS4 has a userid field and no password field; allowing it without a vault password
            // is the common, intended shape. It still uses ProxyAuth so control bytes are refused.
            (Some(username), None) if self.scheme == ProxyScheme::Socks4 => {
                self.auth = Some(ProxyAuth::new(username, "")?);
            }
            (Some(username), Some(password)) => {
                self.auth = Some(ProxyAuth::new(username, password)?);
            }
            (Some(_), None) => {
                return Err(ProxyUrlError::MissingProxyPassword {
                    proxy: id.to_owned(),
                });
            }
            (None, Some(_)) => {
                return Err(ProxyUrlError::PasswordWithoutUsername {
                    proxy: id.to_owned(),
                });
            }
            (None, None) => {}
        }
        Ok(self)
    }

    /// `host:port`, with IPv6 brackets, as it appears in a `CONNECT` request line.
    pub fn authority(&self) -> String {
        format_authority(&self.host, self.port)
    }

    /// The proxy's dialable host: port, with IPv6 brackets removed.
    pub fn dial_target(&self) -> (&str, u16) {
        (self.host.as_str(), self.port)
    }

    /// A key that identifies this proxy **and its credential**, for pooling.
    ///
    /// The credential is part of the key because a pooled connection was opened with it: the
    /// `CONNECT` request that opened the tunnel carried the `Proxy-Authorization` header, and a
    /// connection reused for a *different* credential would be a tunnel opened for someone else.
    ///
    /// The password appears only as a truncated SHA-256 fingerprint. The key is not a secret, but a
    /// key that *contained* the password would be a password in a map key, in a `Debug` dump and in
    /// a heap dump.
    pub fn pool_key(&self) -> String {
        let credential = match &self.auth {
            Some(auth) => {
                let mut hasher = Sha256::new();
                hasher.update(auth.username().as_bytes());
                hasher.update(b"\x00");
                hasher.update(auth.password().as_bytes());
                let digest = hasher.finalize();
                let mut hex = String::with_capacity(16);
                for byte in &digest[..8] {
                    hex.push_str(&format!("{byte:02x}"));
                }
                format!("{}:{hex}", auth.username())
            }
            None => String::new(),
        };
        format!(
            "{}://{}:{}/{}",
            self.scheme.as_str(),
            self.host,
            self.port,
            credential
        )
    }
}

impl std::str::FromStr for ProxyEndpoint {
    type Err = ProxyUrlError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// `host:port`, with the port optional when a default is given.
///
/// `authority` is also used in diagnostics for an empty host, where it is expected to be the full
/// URL after scheme/userinfo removal; the function intentionally does not retain a second copy of
/// the whole URL solely for this error.
fn parse_authority(
    authority: &str,
    default_port: Option<u16>,
) -> Result<(String, u16), ProxyUrlError> {
    if authority.is_empty() {
        return Err(ProxyUrlError::MissingHost {
            url: authority.to_owned(),
        });
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6. The closing bracket is required, and a port after it needs its colon, so
        // `[::1]:8080` cannot be confused with a bare address.
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| ProxyUrlError::UnclosedIpv6 {
                authority: authority.to_owned(),
            })?;
        if host.parse::<Ipv6Addr>().is_err() {
            return Err(ProxyUrlError::InvalidHost {
                host: host.to_owned(),
            });
        }
        let port = if tail.is_empty() {
            default_port.ok_or_else(|| ProxyUrlError::MissingPort {
                authority: authority.to_owned(),
            })?
        } else {
            let port = tail
                .strip_prefix(':')
                .ok_or_else(|| ProxyUrlError::InvalidPort {
                    port: tail.to_owned(),
                })?;
            parse_port(port)?
        };
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, parse_port(port)?),
            None => (
                authority,
                default_port.ok_or_else(|| ProxyUrlError::MissingPort {
                    authority: authority.to_owned(),
                })?,
            ),
        }
    };
    if host.is_empty() {
        return Err(ProxyUrlError::MissingHost {
            url: authority.to_owned(),
        });
    }
    if host.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(ProxyUrlError::InvalidHost {
            host: host.to_owned(),
        });
    }
    // An unbracketed colon in the host means the operator wrote a bare IPv6 literal. It is
    // ambiguous with `host:port`, and guessing would send `CONNECT ::1:8080` to a proxy that reads
    // it as a broken port, so say what is wrong instead.
    if !authority.starts_with('[') && host.parse::<Ipv6Addr>().is_ok() {
        return Err(ProxyUrlError::Ipv6NeedsBrackets {
            host: host.to_owned(),
        });
    }
    Ok((host.to_owned(), port))
}

/// A port is a `u16` and a `u16` only; 0 is not dialable and usually means an unset variable.
fn parse_port(port: &str) -> Result<u16, ProxyUrlError> {
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(ProxyUrlError::InvalidPort {
            port: port.to_owned(),
        }),
        Ok(port) => Ok(port),
    }
}

/// `host:port` with IPv6 brackets when the host is a literal v6 address.
fn format_authority(host: &str, port: u16) -> String {
    if host.parse::<Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// The `host:port` a tunnel has to reach.
///
/// Distinct from a URL because the tunnel cares about nothing else: no scheme, no path, no headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyTarget {
    /// Hostname or IP literal, without brackets.
    pub host: String,
    /// Destination port.
    pub port: u16,
}

impl ProxyTarget {
    /// A target from its parts.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }

    /// The target of an ordinary `http://` or `https://` URL, with the default port applied.
    ///
    /// This is what the relay hands over instead of the request URL, so that the proxy layer never
    /// sees a path or a query it might be tempted to use.
    pub fn from_url(url: &str) -> Result<Self, ProxyUrlError> {
        let url = url.trim();
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| ProxyUrlError::MissingScheme {
                expected: "http, https".to_owned(),
            })?;
        let default_port = match scheme.to_ascii_lowercase().as_str() {
            "http" => 80,
            "https" => 443,
            other => {
                return Err(ProxyUrlError::UnsupportedScheme {
                    scheme: other.to_owned(),
                    expected: "http, https".to_owned(),
                });
            }
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        let authority = match authority.rsplit_once('@') {
            Some((_, host)) => host,
            None => authority,
        };
        let (host, port) = parse_authority(authority, Some(default_port))?;
        Ok(Self { host, port })
    }

    /// `host:port`, bracketed for IPv6. Used verbatim in a `CONNECT` request line.
    pub fn authority(&self) -> String {
        format_authority(&self.host, self.port)
    }

    /// The host as an IP literal, when it is one.
    fn ip(&self) -> Option<IpAddr> {
        self.host.parse().ok()
    }
}

/// A byte stream that reaches `host:port` through a proxy.
///
/// Two shapes only, and the difference matters to the caller: for an `https://` proxy the bytes are
/// already inside TLS, so origin TLS wraps a `TlsStream`. For everything else the stream is raw
/// TCP. An enum rather than `Box<dyn AsyncRead + AsyncWrite>` because hyper's `TokioIo` needs the
/// stream to be `Unpin` and sized, and boxing a trait object there costs an allocation per request
/// for the privilege of losing that guarantee.
pub enum Tunnel {
    /// Raw TCP through the proxy hop (`http`, `socks4`, `socks5`, `socks5h`).
    Plain(TcpStream),
    /// TLS to the proxy itself (`https`), with the `CONNECT` tunnel inside it.
    ProxyTls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl Tunnel {
    /// Whether the proxy hop is encrypted. The origin hop is the caller's business either way.
    pub const fn is_encrypted(&self) -> bool {
        matches!(self, Self::ProxyTls(_))
    }

    /// Borrow the underlying TCP connection when there is one.
    ///
    /// For a plain HTTP or SOCKS tunnel this is the entire stream. An HTTPS proxy tunnel is a TLS
    /// stream over this socket, so the caller must not use the socket for origin TLS.
    pub fn tcp_stream(&self) -> Option<&TcpStream> {
        match self {
            Self::Plain(stream) => Some(stream),
            Self::ProxyTls(_) => None,
        }
    }

    /// Consume the tunnel and return its raw TCP stream when it is plaintext to the proxy.
    pub fn into_tcp_stream(self) -> Result<TcpStream, Self> {
        match self {
            Self::Plain(stream) => Ok(stream),
            tunnel @ Self::ProxyTls(_) => Err(tunnel),
        }
    }
}

impl fmt::Debug for Tunnel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not the inner stream: its `Debug` is noise, and a `TcpStream`'s would print addresses a
        // caller would be tempted to log and then correlate.
        f.write_str(match self {
            Self::Plain(_) => "Tunnel::Plain",
            Self::ProxyTls(_) => "Tunnel::ProxyTls",
        })
    }
}

impl AsyncRead for Tunnel {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Tunnel {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::ProxyTls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Why a proxy URL, credential, or endpoint could not be used.
///
/// Every variant names the specific thing that is wrong, because the operator's next action differs:
/// "unsupported scheme" is a documentation problem, "must include a port" is a typo, and "the vault
/// has not yielded this secret" is a state problem.
#[derive(Debug, thiserror::Error)]
pub enum ProxyUrlError {
    /// The URL was empty or whitespace.
    #[error("proxy url is empty")]
    Empty,
    /// No `scheme://` prefix.
    #[error("proxy url must include a scheme (one of {expected})")]
    MissingScheme {
        /// The schemes that would have been accepted.
        expected: String,
    },
    /// A scheme this module does not speak.
    #[error("unsupported proxy scheme `{scheme}` (expected one of {expected})")]
    UnsupportedScheme {
        /// The scheme as written, lowercased.
        scheme: String,
        /// The schemes that would have been accepted.
        expected: String,
    },
    /// No host between the scheme and the port.
    #[error("proxy url names no host: `{url}`")]
    MissingHost {
        /// Whatever was in the authority position.
        url: String,
    },
    /// A proxy URL with no port. There is no default: `127.0.0.1` is not a proxy.
    #[error("proxy url `{authority}` must include a port")]
    MissingPort {
        /// The authority as written.
        authority: String,
    },
    /// A port that is not a number in `1..=65535`.
    #[error("proxy url has an invalid port `{port}`")]
    InvalidPort {
        /// The port as written.
        port: String,
    },
    /// A host containing a space or a control character.
    #[error("proxy url has an invalid host `{host}`")]
    InvalidHost {
        /// The host as written.
        host: String,
    },
    /// A bare IPv6 literal where `host:port` was expected.
    #[error(
        "proxy url host `{host}` is an IPv6 literal and must be bracketed, as in `[{host}]:1080`"
    )]
    Ipv6NeedsBrackets {
        /// The host as written.
        host: String,
    },
    /// `[` with no `]`.
    #[error("proxy url `{authority}` has an unclosed `[` for an IPv6 host")]
    UnclosedIpv6 {
        /// The authority as written.
        authority: String,
    },
    /// A path, query, or fragment in a proxy URL.
    #[error(
        "proxy url contains a path, query, or fragment (first unexpected byte at offset {at})"
    )]
    NotAnEndpoint {
        /// Byte offset of the first offending character.
        at: usize,
    },
    /// `user:pass@` with an empty user.
    #[error("proxy url has an empty username before `:`")]
    EmptyUsername,
    /// A control character in a credential. See [`ProxyAuth::new`].
    #[error(
        "proxy {field} contains a control character, which would break out of its protocol field"
    )]
    AuthControlCharacter {
        /// `username` or `password`.
        field: &'static str,
    },
    /// `username` configured, but the caller resolved no password.
    #[error("proxy `{proxy}` names a username but no password was resolved from the vault")]
    MissingProxyPassword {
        /// The proxy id.
        proxy: String,
    },
    /// A password with no username to pair it with.
    #[error("proxy `{proxy}` has a password but no username")]
    PasswordWithoutUsername {
        /// The proxy id.
        proxy: String,
    },
}

/// Everything that can go wrong while getting a tunnel through a proxy.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The endpoint or target could not be parsed.
    #[error(transparent)]
    Url(#[from] ProxyUrlError),
    /// The socket failed, the proxy was not listening, DNS failed.
    #[error("proxy i/o: {0}")]
    Io(#[from] io::Error),
    /// The TCP connect to the proxy did not complete in time.
    #[error("proxy connect timeout after {0:?}")]
    ConnectTimeout(Duration),
    /// The proxy accepted the connection and then went quiet.
    #[error("proxy did not answer within {after:?} while {phase}")]
    ReplyTimeout {
        /// Which step of the handshake was waiting.
        phase: &'static str,
        /// The bound that expired.
        after: Duration,
    },
    /// The proxy closed the connection mid-handshake.
    #[error("proxy closed the connection while {0}")]
    Closed(&'static str),
    /// A reply longer than [`TunnelLimits::max_reply_bytes`] without a terminator.
    #[error("proxy reply exceeded {0} bytes")]
    ReplyTooLong(usize),
    /// The proxy answered `CONNECT` with a non-2xx status other than 407.
    #[error("proxy refused `CONNECT {target}`: {status} {reason}")]
    HttpStatus {
        /// The target authority.
        target: String,
        /// The status code.
        status: u16,
        /// The proxy's reason phrase, verbatim.
        reason: String,
    },
    /// The proxy answered `CONNECT` with 407.
    #[error("proxy requires authentication for `CONNECT {target}`: {status} {reason}")]
    HttpAuthRequired {
        /// The target authority.
        target: String,
        /// The status code, always 407 in practice.
        status: u16,
        /// The proxy's reason phrase, verbatim.
        reason: String,
    },
    /// The reply was not HTTP at all.
    #[error("proxy sent a malformed HTTP reply: {0}")]
    HttpMalformed(String),
    /// A SOCKS version byte this module does not implement.
    #[error("socks: unsupported protocol version {0}")]
    SocksVersion(u8),
    /// The proxy accepted none of the offered authentication methods.
    #[error("socks5: proxy accepted none of the offered authentication methods")]
    SocksMethodRejected,
    /// The proxy selected a method the client did not offer.
    #[error("socks5: proxy selected authentication method {0:#04x}, which was not offered")]
    SocksMethodNotOffered(u8),
    /// The proxy demanded username/password and none was configured.
    #[error("socks5: proxy requires username/password authentication but none is configured")]
    SocksAuthMissing,
    /// Username/password authentication failed.
    #[error("socks5: authentication rejected (status {0:#04x})")]
    SocksAuthFailed(u8),
    /// A SOCKS5 `CONNECT` reply code.
    #[error("socks5: {reason} (reply code {code:#04x})")]
    SocksReply {
        /// The raw `REP` value.
        code: u8,
        /// A stable English rendering.
        reason: &'static str,
    },
    /// A SOCKS5 reply address type this module cannot frame.
    #[error("socks5: reply uses address type {0:#04x}, which this client cannot frame")]
    SocksAddressType(u8),
    /// A SOCKS4 reply byte other than 0x5A.
    #[error("socks4: {reason} (code {code:#04x})")]
    Socks4Reply {
        /// The raw `CD` value.
        code: u8,
        /// A stable English rendering.
        reason: &'static str,
    },
    /// The first SOCKS4 reply byte was neither 0x00 nor 0x04.
    #[error("socks4: proxy returned version {0:#04x}")]
    Socks4Version(u8),
    /// A hostname longer than the protocol's 255-byte field.
    #[error("target host is longer than the {0}-byte limit of this protocol")]
    HostTooLong(usize),
    /// A credential longer than SOCKS5's length-prefixed field.
    #[error("proxy credential is longer than the {0}-byte limit of SOCKS5")]
    CredentialTooLong(usize),
    /// Local resolution of a SOCKS5 target failed.
    #[error("could not resolve `{host}` for a socks5 proxy: {message}")]
    Resolve {
        /// The host that failed.
        host: String,
        /// The resolver's message.
        message: String,
    },
    /// Local DNS resolution for the target took too long.
    #[error("resolving target `{1}` through the local resolver timed out after {0:?}")]
    ResolveTimeout(Duration, String),
    /// `https://` proxy with no TLS connector supplied.
    #[error(
        "an https proxy needs a TLS connector for the proxy hop; pass one to `open_tunnel`, or dial
         with `dial_proxy`, wrap the stream yourself, and call `http_connect_over`"
    )]
    TlsConnectorRequired,
    /// The TLS handshake with the proxy failed.
    #[error("tls to the proxy failed: {0}")]
    Tls(String),
}

impl ProxyError {
    /// Whether the proxy rejected the credential.
    ///
    /// The caller needs this to tell "this proxy is broken, try the next one" from "this credential
    /// is wrong, stop and tell the operator" — retrying an auth failure on every request is how a
    /// proxy ends up locking the operator out, and a 407 on a *shared* account can also mean the
    /// upstream is misconfigured in a way worth surfacing.
    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            Self::HttpAuthRequired { .. }
                | Self::SocksAuthFailed(_)
                | Self::SocksMethodRejected
                | Self::SocksAuthMissing
        )
    }

    /// Whether trying another proxy, or the same one again later, could plausibly work.
    ///
    /// A parse error never becomes true; a connect timeout does.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Io(_)
                | Self::ConnectTimeout(_)
                | Self::ReplyTimeout { .. }
                | Self::Closed(_)
                | Self::SocksReply { .. }
                | Self::Socks4Reply { .. }
                | Self::HttpStatus { .. }
                | Self::ResolveTimeout(_, _)
        )
    }
}

/// Dial the proxy's own TCP endpoint, with no handshake.
///
/// The step before [`open_tunnel`] exists on its own so that the `https://` case can be finished by
/// a caller that owns a TLS configuration: dial, wrap in TLS, then [`http_connect_over`].
pub async fn dial_proxy(
    endpoint: &ProxyEndpoint,
    limits: &TunnelLimits,
) -> Result<TcpStream, ProxyError> {
    let (host, port) = endpoint.dial_target();
    let connect = TcpStream::connect((host, port));
    let stream = timeout(limits.connect_timeout, connect)
        .await
        .map_err(|_| ProxyError::ConnectTimeout(limits.connect_timeout))??;
    // Nagle would hold the 30-byte CONNECT request back waiting for more data, adding a round trip
    // to the slowest part of a cold request. The same reason `latency.tcp_nodelay` exists.
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

/// Produce a tunnel to `target` through `endpoint`.
///
/// # TLS ownership
///
/// * **The hop to the proxy** is encrypted only for `https://`, and the trust store that decides
///   whether to believe the proxy's certificate belongs to the caller: pass `proxy_tls` and this
///   function runs the handshake. Without it, an `https://` endpoint fails with
///   [`ProxyError::TlsConnectorRequired`] *before* dialling — refusing early is the point, because a
///   caller that fell back to plaintext here would be leaking the `Proxy-Authorization` header.
/// * **The hop to the origin** is always the caller's. Wrap the returned [`Tunnel`] in TLS for the
///   origin host, then hand it to hyper. This function has deliberately not opened that TLS, because
///   the certificate for a provider must be verified against the origin's name, not the proxy's.
///
/// The `proxy_tls` connector must not advertise `h2` in ALPN: `CONNECT` is HTTP/1.1, and offering
/// `h2` to the proxy and then speaking HTTP/1.1 over it is a protocol violation some proxies answer
/// by hanging up.
pub async fn open_tunnel(
    endpoint: &ProxyEndpoint,
    target: &ProxyTarget,
    limits: &TunnelLimits,
    proxy_tls: Option<&tokio_rustls::TlsConnector>,
) -> Result<Tunnel, ProxyError> {
    if endpoint.scheme.is_tls() && proxy_tls.is_none() {
        return Err(ProxyError::TlsConnectorRequired);
    }
    let mut stream = dial_proxy(endpoint, limits).await?;
    match endpoint.scheme {
        ProxyScheme::Http => {
            http_connect_over(&mut stream, target, endpoint.auth.as_ref(), limits).await?;
            Ok(Tunnel::Plain(stream))
        }
        ProxyScheme::Https => {
            let connector = proxy_tls.ok_or(ProxyError::TlsConnectorRequired)?;
            let server_name = rustls::pki_types::ServerName::try_from(endpoint.host.clone())
                .map_err(|e| {
                    ProxyError::Tls(format!(
                        "`{}` is not a valid TLS server name: {e}",
                        endpoint.host
                    ))
                })?;
            let mut tls = connector
                .connect(server_name, stream)
                .await
                .map_err(|e| ProxyError::Tls(e.to_string()))?;
            http_connect_over(&mut tls, target, endpoint.auth.as_ref(), limits).await?;
            Ok(Tunnel::ProxyTls(Box::new(tls)))
        }
        ProxyScheme::Socks4 => {
            socks4_connect_over(&mut stream, target, endpoint.auth.as_ref(), limits).await?;
            Ok(Tunnel::Plain(stream))
        }
        ProxyScheme::Socks5 | ProxyScheme::Socks5h => {
            socks5_connect_over(
                &mut stream,
                target,
                endpoint.auth.as_ref(),
                endpoint.scheme.resolves_at_proxy(),
                limits,
            )
            .await?;
            Ok(Tunnel::Plain(stream))
        }
    }
}

/// Send the `CONNECT` request and validate the reply.
///
/// Generic over the stream so the same code serves a plaintext proxy and an `https://` one, where
/// the stream is already a `TlsStream`. On return the stream is positioned at the first tunneled
/// byte: nothing of the reply is left in the socket, which is the property that makes the caller's
/// TLS handshake work.
///
/// # Request shape
///
/// ```text
/// CONNECT host:port HTTP/1.1
/// Host: host:port
/// Proxy-Authorization: Basic ...      (only when a credential is configured)
///
/// ```
///
/// `Host` is sent because proxies that route by virtual host require it, and RFC 9110 requires it on
/// HTTP/1.1 anyway. No `Proxy-Connection`: it is a Netscape-era header that RFC 7230 deprecated and
/// some proxies log as an error.
pub async fn http_connect_over<S>(
    stream: &mut S,
    target: &ProxyTarget,
    auth: Option<&ProxyAuth>,
    limits: &TunnelLimits,
) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let authority = target.authority();
    let mut request = String::with_capacity(96 + authority.len() * 2);
    request.push_str("CONNECT ");
    request.push_str(&authority);
    request.push_str(" HTTP/1.1\r\nHost: ");
    request.push_str(&authority);
    request.push_str("\r\n");
    if let Some(auth) = auth {
        request.push_str("Proxy-Authorization: ");
        request.push_str(&auth.basic_authorization());
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    write_all_timed(
        stream,
        request.as_bytes(),
        limits,
        "sending the CONNECT request",
    )
    .await?;

    let head = read_http_head(stream, limits).await?;
    let (status, reason) = parse_status_line(&head)?;
    match status {
        200..=299 => Ok(()),
        407 => Err(ProxyError::HttpAuthRequired {
            target: authority,
            status,
            reason,
        }),
        _ => Err(ProxyError::HttpStatus {
            target: authority,
            status,
            reason,
        }),
    }
}

/// Read the status line and headers of a `CONNECT` reply.
///
/// Stops **only** at the blank line. Stopping one byte earlier — at the end of the status line —
/// would be tempting and wrong: for a proxy that sends headers, the header bytes would stay in the
/// socket and the caller's TLS handshake would read `Proxy-Agent: ...` as a `ClientHello` reply.
///
/// The reply is bounded by `max_reply_bytes`, and the tail of the buffer is *not* retained: the
/// terminator is dropped so no reply byte can be mistaken for tunnel data.
async fn read_http_head<S>(stream: &mut S, limits: &TunnelLimits) -> Result<Vec<u8>, ProxyError>
where
    S: AsyncRead + Unpin,
{
    const TERMINATOR: &[u8] = b"\r\n\r\n";
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        // One byte at a time is deliberate. A larger `read` can consume TLS application bytes
        // immediately following the header terminator; dropping those bytes would corrupt the
        // caller's first record. Header parsing is a one-time handshake, so preserving the tunnel
        // boundary is worth the handful of syscalls.
        let n = timeout(limits.reply_timeout, stream.read(&mut byte))
            .await
            .map_err(|_| ProxyError::ReplyTimeout {
                phase: "reading the CONNECT reply",
                after: limits.reply_timeout,
            })?
            .map_err(ProxyError::Io)?;
        if n == 0 {
            return Err(ProxyError::Closed("reading the CONNECT reply"));
        }
        head.push(byte[0]);
        if let Some(end) = find_subslice(&head, TERMINATOR) {
            head.truncate(end);
            return Ok(head);
        }
        if head.len() > limits.max_reply_bytes {
            return Err(ProxyError::ReplyTooLong(limits.max_reply_bytes));
        }
    }
}

/// Parse `HTTP/1.x <status> <reason>` out of a reply head.
fn parse_status_line(head: &[u8]) -> Result<(u16, String), ProxyError> {
    let end = find_subslice(head, b"\r\n").unwrap_or(head.len());
    let line = std::str::from_utf8(&head[..end])
        .map_err(|_| ProxyError::HttpMalformed("status line is not valid UTF-8".to_owned()))?;
    let mut parts = line.splitn(3, ' ');
    let version = parts.next().unwrap_or_default();
    if !version.starts_with("HTTP/") {
        return Err(ProxyError::HttpMalformed(format!(
            "expected an HTTP status line, got `{line}`"
        )));
    }
    let status = parts
        .next()
        .ok_or_else(|| {
            ProxyError::HttpMalformed(format!("status line `{line}` has no status code"))
        })?
        .parse::<u16>()
        .map_err(|_| {
            ProxyError::HttpMalformed(format!("status code in `{line}` is not a number"))
        })?;
    if !(100..=599).contains(&status) {
        return Err(ProxyError::HttpMalformed(format!(
            "status code {status} is outside 100..=599"
        )));
    }
    // The reason phrase is free text and may be absent; the proxy's own wording is more useful in a
    // log than a generic message, so it is passed through verbatim (it is only ever formatted, never
    // re-emitted as a header).
    let reason = parts.next().unwrap_or_default().trim().to_owned();
    Ok((status, reason))
}

/// Run a SOCKS5 handshake and `CONNECT` to `target`.
///
/// `resolves_at_proxy` selects the address type: `true` (the `socks5h` scheme) sends the hostname as
/// an `ATYP_DOMAIN` so the proxy does the DNS lookup, which is the whole reason `socks5h` exists —
/// it keeps the relay's resolver out of the path and stops DNS leaks beside the tunnel. `false`
/// resolves locally first and sends an IP, because silently sending a domain for plain `socks5`
/// would make the two schemes identical and would leak every hostname to a proxy the operator did
/// not ask to be the resolver.
pub async fn socks5_connect_over<S>(
    stream: &mut S,
    target: &ProxyTarget,
    auth: Option<&ProxyAuth>,
    resolves_at_proxy: bool,
    limits: &TunnelLimits,
) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const VERSION: u8 = 0x05;
    const METHOD_NO_AUTH: u8 = 0x00;
    const METHOD_USER_PASS: u8 = 0x02;
    const METHOD_NONE_ACCEPTABLE: u8 = 0xFF;
    const CMD_CONNECT: u8 = 0x01;
    const ATYP_IPV4: u8 = 0x01;
    const ATYP_DOMAIN: u8 = 0x03;
    const ATYP_IPV6: u8 = 0x04;

    // Method negotiation. When a credential is configured the client offers *only* username/
    // password: offering no-auth as well lets a proxy that ignores credentials take the
    // unauthenticated path without anyone noticing, which is the downgrade this module exists to
    // make impossible.
    let greeting: [u8; 3] = match auth {
        Some(_) => [VERSION, 1, METHOD_USER_PASS],
        None => [VERSION, 1, METHOD_NO_AUTH],
    };
    write_all_timed(stream, &greeting, limits, "sending the SOCKS5 greeting").await?;
    let mut method = [0u8; 2];
    read_exact_timed(stream, &mut method, limits, "the SOCKS5 method reply").await?;
    if method[0] != VERSION {
        return Err(ProxyError::SocksVersion(method[0]));
    }
    match (method[1], auth) {
        (METHOD_NO_AUTH, None) => {}
        (METHOD_USER_PASS, Some(auth)) => socks5_authenticate(stream, auth, limits).await?,
        (METHOD_USER_PASS, None) => return Err(ProxyError::SocksAuthMissing),
        (METHOD_NONE_ACCEPTABLE, _) => return Err(ProxyError::SocksMethodRejected),
        (other, _) => return Err(ProxyError::SocksMethodNotOffered(other)),
    }

    let mut request = Vec::with_capacity(32);
    request.extend_from_slice(&[VERSION, CMD_CONNECT, 0x00]);
    match (resolves_at_proxy, target.ip()) {
        (false, Some(IpAddr::V4(ip))) => {
            request.push(ATYP_IPV4);
            request.extend_from_slice(&ip.octets());
        }
        (false, Some(IpAddr::V6(ip))) => {
            request.push(ATYP_IPV6);
            request.extend_from_slice(&ip.octets());
        }
        (true, _) | (false, None) => {
            let host = if resolves_at_proxy {
                target.host.as_str()
            } else {
                // Local resolution: `socks5` means the client resolves, and an IP literal is what
                // the proxy expects. Only the first address is used; a proxy takeover race is not
                // this module's to solve, and trying every address would multiply the timeout.
                return socks5_connect_resolved(stream, target, limits).await;
            };
            if host.len() > 255 {
                return Err(ProxyError::HostTooLong(255));
            }
            request.push(ATYP_DOMAIN);
            request.push(host.len() as u8);
            request.extend_from_slice(host.as_bytes());
        }
    }
    request.extend_from_slice(&target.port.to_be_bytes());
    write_all_timed(
        stream,
        &request,
        limits,
        "sending the SOCKS5 CONNECT request",
    )
    .await?;
    socks5_read_reply(stream, limits).await
}

/// Resolve `target` locally and continue the SOCKS5 conversation with an IP literal.
///
/// Split out because the resolution is `await` and the framing above is not; keeping it in one
/// function made the borrow checker and the reader both unhappy.
async fn socks5_connect_resolved<S>(
    stream: &mut S,
    target: &ProxyTarget,
    limits: &TunnelLimits,
) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const VERSION: u8 = 0x05;
    const CMD_CONNECT: u8 = 0x01;
    const ATYP_IPV4: u8 = 0x01;
    const ATYP_IPV6: u8 = 0x04;

    let ip = timeout(limits.connect_timeout, resolve_locally(target))
        .await
        .map_err(|_| ProxyError::ResolveTimeout(limits.connect_timeout, target.host.clone()))??;
    let mut request = Vec::with_capacity(22);
    request.extend_from_slice(&[VERSION, CMD_CONNECT, 0x00]);
    match ip {
        IpAddr::V4(ip) => {
            request.push(ATYP_IPV4);
            request.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            request.push(ATYP_IPV6);
            request.extend_from_slice(&ip.octets());
        }
    }
    request.extend_from_slice(&target.port.to_be_bytes());
    write_all_timed(
        stream,
        &request,
        limits,
        "sending the SOCKS5 CONNECT request",
    )
    .await?;
    socks5_read_reply(stream, limits).await
}

/// The RFC 1929 username/password sub-negotiation.
async fn socks5_authenticate<S>(
    stream: &mut S,
    auth: &ProxyAuth,
    limits: &TunnelLimits,
) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const AUTH_VERSION: u8 = 0x01;
    let user = auth.username().as_bytes();
    let password = auth.password().as_bytes();
    if user.len() > 255 || password.len() > 255 {
        return Err(ProxyError::CredentialTooLong(255));
    }
    let mut request = Vec::with_capacity(3 + user.len() + password.len());
    request.push(AUTH_VERSION);
    request.push(user.len() as u8);
    request.extend_from_slice(user);
    request.push(password.len() as u8);
    request.extend_from_slice(password);
    write_all_timed(
        stream,
        &request,
        limits,
        "sending the SOCKS5 username/password request",
    )
    .await?;
    let mut reply = [0u8; 2];
    read_exact_timed(
        stream,
        &mut reply,
        limits,
        "the SOCKS5 authentication reply",
    )
    .await?;
    if reply[0] != AUTH_VERSION {
        return Err(ProxyError::SocksVersion(reply[0]));
    }
    if reply[1] != 0x00 {
        return Err(ProxyError::SocksAuthFailed(reply[1]));
    }
    Ok(())
}

/// Read and validate the SOCKS5 `CONNECT` reply, consuming the bound address.
///
/// The bound address is not used, but it must be *read*: it is part of the reply frame, and leaving
/// it in the socket would make the caller's first TLS record start with four bytes of a `BND.ADDR`.
async fn socks5_read_reply<S>(stream: &mut S, limits: &TunnelLimits) -> Result<(), ProxyError>
where
    S: AsyncRead + Unpin,
{
    const VERSION: u8 = 0x05;
    const ATYP_IPV4: u8 = 0x01;
    const ATYP_DOMAIN: u8 = 0x03;
    const ATYP_IPV6: u8 = 0x04;

    let mut head = [0u8; 4];
    read_exact_timed(stream, &mut head, limits, "the SOCKS5 reply").await?;
    if head[0] != VERSION {
        return Err(ProxyError::SocksVersion(head[0]));
    }
    if head[1] != 0x00 {
        // No need to drain the address: the stream is dropped on this path.
        return Err(ProxyError::SocksReply {
            code: head[1],
            reason: socks5_reply_reason(head[1]),
        });
    }
    // ATYP is the fourth byte; the reserved byte in between is ignored, as RFC 1928 requires.
    let skip = match head[3] {
        ATYP_IPV4 => 4 + 2,
        ATYP_IPV6 => 16 + 2,
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            read_exact_timed(stream, &mut len, limits, "the SOCKS5 reply domain length").await?;
            1 + usize::from(len[0]) + 2
        }
        other => return Err(ProxyError::SocksAddressType(other)),
    };
    // The SOCKS5 bounded address fields are tiny: IPv6 + port is 18 bytes, and the domain field is
    // bounded by its 8-bit length. A fixed-size stack buffer avoids allocating from an untrusted
    // reply length, even though that length is already structurally capped at 255.
    let mut scratch = [0u8; 1 + 255 + 2];
    read_exact_timed(
        stream,
        &mut scratch[..skip],
        limits,
        "the SOCKS5 reply address",
    )
    .await?;
    Ok(())
}

/// A stable English rendering of a SOCKS5 `REP` code.
///
/// RFC 1928 leaves the wording to the implementation; these strings are the ones the panel shows, so
/// they are named here rather than formatted from the number.
pub fn socks5_reply_reason(code: u8) -> &'static str {
    match code {
        0x01 => "general SOCKS server failure",
        0x02 => "connection not allowed by ruleset",
        0x03 => "network unreachable",
        0x04 => "host unreachable",
        0x05 => "connection refused",
        0x06 => "TTL expired",
        0x07 => "command not supported",
        0x08 => "address type not supported",
        _ => "unknown SOCKS5 reply code",
    }
}

/// Run a SOCKS4 (or, for non-IPv4 targets, SOCKS4a) `CONNECT`.
///
/// SOCKS4 has no password field, so [`ProxyAuth::username`] is sent as the userid and the password is
/// ignored — construction still validates it, because a credential that is unused today should not
/// become an injection tomorrow.
///
/// # SOCKS4a
///
/// SOCKS4 can only carry an IPv4 literal. When the target is a hostname, this sends the SOCKS4a
/// form: the address `0.0.0.1` (the last octet must be non-zero; 1 is the convention) and the
/// hostname NUL-terminated after the userid. A proxy that predates SOCKS4a will reject or
/// mis-route it, which is exactly what an 0x5B reply reports.
pub async fn socks4_connect_over<S>(
    stream: &mut S,
    target: &ProxyTarget,
    auth: Option<&ProxyAuth>,
    limits: &TunnelLimits,
) -> Result<(), ProxyError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    const VERSION: u8 = 0x04;
    const CMD_CONNECT: u8 = 0x01;
    const REPLY_GRANTED: u8 = 0x5A;

    let userid = auth.map(ProxyAuth::username).unwrap_or_default();
    // A NUL here would end the userid early and make the rest of the username look like the
    // hostname of a SOCKS4a request. `ProxyAuth::new` refuses control characters, so this is a
    // defence in depth rather than the only check.
    if userid.bytes().any(|b| b == 0) {
        return Err(ProxyUrlError::AuthControlCharacter { field: "username" }.into());
    }
    let ipv4 = target.host.parse::<Ipv4Addr>().ok();
    let mut request = Vec::with_capacity(16 + userid.len() + target.host.len());
    request.push(VERSION);
    request.push(CMD_CONNECT);
    request.extend_from_slice(&target.port.to_be_bytes());
    match ipv4 {
        Some(ip) => request.extend_from_slice(&ip.octets()),
        None => request.extend_from_slice(&[0, 0, 0, 1]),
    }
    request.extend_from_slice(userid.as_bytes());
    request.push(0x00);
    if ipv4.is_none() {
        if target.host.len() > 255 {
            return Err(ProxyError::HostTooLong(255));
        }
        request.extend_from_slice(target.host.as_bytes());
        request.push(0x00);
    }
    write_all_timed(
        stream,
        &request,
        limits,
        "sending the SOCKS4 CONNECT request",
    )
    .await?;
    let mut reply = [0u8; 8];
    read_exact_timed(stream, &mut reply, limits, "the SOCKS4 reply").await?;
    // 0x00 is what RFC 1928-era servers send; a few send their own version back.
    if reply[0] != 0x00 && reply[0] != VERSION {
        return Err(ProxyError::Socks4Version(reply[0]));
    }
    if reply[1] != REPLY_GRANTED {
        return Err(ProxyError::Socks4Reply {
            code: reply[1],
            reason: socks4_reply_reason(reply[1]),
        });
    }
    Ok(())
}

/// A stable English rendering of a SOCKS4 `CD` byte.
pub fn socks4_reply_reason(code: u8) -> &'static str {
    match code {
        0x5A => "request granted",
        0x5B => "request rejected or failed",
        0x5C => "request rejected: identd is unreachable",
        0x5D => "request rejected: identd could not confirm the user id",
        _ => "unknown SOCKS4 reply code",
    }
}

/// Resolve `target` with the system resolver, for the `socks5` (client-resolves) case.
async fn resolve_locally(target: &ProxyTarget) -> Result<IpAddr, ProxyError> {
    if let Some(ip) = target.ip() {
        return Ok(ip);
    }
    let resolved = tokio::net::lookup_host((target.host.as_str(), target.port))
        .await
        .map_err(|e| ProxyError::Resolve {
            host: target.host.clone(),
            message: e.to_string(),
        })?;
    resolved
        .map(|addr| addr.ip())
        .next()
        .ok_or_else(|| ProxyError::Resolve {
            host: target.host.clone(),
            message: "the resolver returned no addresses".to_owned(),
        })
}

/// Write all of `bytes` and flush, inside the reply timeout.
///
/// The flush matters for the TLS-to-proxy case: `tokio_rustls` buffers and will happily return from
/// `write_all` with a record still in memory, so a handshake that "sent" its greeting would then
/// wait forever for a reply to a record that was never transmitted.
async fn write_all_timed<S>(
    stream: &mut S,
    bytes: &[u8],
    limits: &TunnelLimits,
    phase: &'static str,
) -> Result<(), ProxyError>
where
    S: AsyncWrite + Unpin,
{
    timeout(limits.reply_timeout, stream.write_all(bytes))
        .await
        .map_err(|_| ProxyError::ReplyTimeout {
            phase,
            after: limits.reply_timeout,
        })??;
    timeout(limits.reply_timeout, stream.flush())
        .await
        .map_err(|_| ProxyError::ReplyTimeout {
            phase,
            after: limits.reply_timeout,
        })??;
    Ok(())
}

/// Read exactly `buf.len()` bytes, inside the reply timeout, with EOF reported as [`ProxyError::Closed`].
async fn read_exact_timed<S>(
    stream: &mut S,
    buf: &mut [u8],
    limits: &TunnelLimits,
    phase: &'static str,
) -> Result<(), ProxyError>
where
    S: AsyncRead + Unpin,
{
    timeout(limits.reply_timeout, stream.read_exact(buf))
        .await
        .map_err(|_| ProxyError::ReplyTimeout {
            phase,
            after: limits.reply_timeout,
        })?
        .map(|_| ())
        .map_err(|e| match e.kind() {
            io::ErrorKind::UnexpectedEof => ProxyError::Closed(phase),
            _ => ProxyError::Io(e),
        })
}

/// Find `needle` in `haystack`, returning its first offset.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Base64 (RFC 4648, standard alphabet, padded), for `Proxy-Authorization: Basic`.
///
/// Fifteen lines rather than a dependency: this is an *encoder* only, with no padding edge cases to
/// get wrong on the decode side, and the vector test below pins it to the RFC. A base64 crate is a
/// reasonable thing to add to the relay the day something else needs to decode it.
pub fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from);
        let b2 = chunk.get(2).copied().map(u32::from);
        let triple = (b0 << 16) | (b1.unwrap_or(0) << 8) | b2.unwrap_or(0);
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        out.push(match b1 {
            Some(_) => ALPHABET[((triple >> 6) & 0x3F) as usize] as char,
            None => '=',
        });
        out.push(match b2 {
            Some(_) => ALPHABET[(triple & 0x3F) as usize] as char,
            None => '=',
        });
    }
    out
}

/// What the selector needs to know about a proxy.
///
/// The configuration entry an operator wrote and the compiled plan the routing snapshot holds carry
/// the same selection information. Selecting over a trait rather than over one concrete type keeps
/// **one** implementation of the strategies, so the pool the operator configured and the pool the
/// snapshot compiled cannot disagree about what "the second proxy" means.
pub trait SelectableProxy {
    /// The stable id, which is also the key the in-flight counters use.
    fn proxy_id(&self) -> &str;
    /// Whether the proxy may be chosen.
    fn proxy_enabled(&self) -> bool;
}

impl SelectableProxy for ProxyConfig {
    fn proxy_id(&self) -> &str {
        &self.id
    }
    fn proxy_enabled(&self) -> bool {
        self.enabled
    }
}

impl SelectableProxy for mr_core::snapshot::ProxyPlan {
    fn proxy_id(&self) -> &str {
        &self.id
    }
    /// The snapshot only carries the proxies that are enabled, so every one of them is selectable.
    fn proxy_enabled(&self) -> bool {
        true
    }
}

/// Choose a proxy for one request.
///
/// `state` carries the only mutable inputs selection needs (the round-robin ticket and the in-flight
/// counters), so this function is `&self`-free and can be called from any task without a lock. It is
/// not async and does no I/O: selection is exactly the part that must be testable without a network.
///
/// Disabled proxies are skipped. There is no health field on [`ProxyConfig`], so failing proxies are
/// **not** skipped here: the caller holds that knowledge (it is the caller that saw the failure) and
/// should either filter the slice or retry with the one it was given. Keeping health out of this
/// signature is deliberate — a selector that silently hides a proxy makes "why did nothing egress
/// through the proxy I configured?" unanswerable from a log.
pub fn choose<'a, P: SelectableProxy>(
    proxies: &'a [P],
    strategy: ProxyStrategy,
    state: &ProxySelectionState,
) -> Option<&'a P> {
    choose_index(proxies, strategy, state).map(|index| &proxies[index])
}

/// [`choose`], returning the position in `proxies` instead of a reference.
///
/// The caller needs the proxy's id to open a [`ProxyLease`] on it, which is how `least-used` learns
/// about in-flight requests at all.
pub fn choose_index<P: SelectableProxy>(
    proxies: &[P],
    strategy: ProxyStrategy,
    state: &ProxySelectionState,
) -> Option<usize> {
    let mut rng = rand::thread_rng();
    choose_index_with_rng(proxies, strategy, state, &mut rng)
}

/// The selection core, with the RNG injected so a test can seed it.
///
/// Every strategy iterates the enabled subset lazily: no `Vec` is built, because this runs once per
/// upstream attempt and allocating a list of proxies per request would be a small, permanent tax on
/// the hot path.
pub fn choose_index_with_rng<P: SelectableProxy, R: rand::Rng + ?Sized>(
    proxies: &[P],
    strategy: ProxyStrategy,
    state: &ProxySelectionState,
    rng: &mut R,
) -> Option<usize> {
    // The task's priority contract is explicit: first enabled in the config's ordered slice. The
    // `priority` field is retained for config compatibility, but changing its meaning here would
    // make file order stop being observable and violate that contract.
    match strategy {
        ProxyStrategy::Priority => enabled_indices(proxies).next(),
        ProxyStrategy::Random => {
            let count = enabled_count(proxies);
            if count == 0 {
                return None;
            }
            let pick = rng.gen_range(0..count);
            enabled_indices(proxies).nth(pick)
        }
        ProxyStrategy::RoundRobin => {
            let count = enabled_count(proxies);
            if count == 0 {
                return None;
            }
            // A relaxed ticket, not a lock: two requests may take the same ticket after a wraparound
            // and nobody cares, because the only promise is a fair cycle over time.
            let pick = (state.next_round_robin() % count as u64) as usize;
            enabled_indices(proxies).nth(pick)
        }
        ProxyStrategy::LeastUsed => enabled_indices(proxies).min_by_key(|&index| {
            // Ties fall to declaration order through the index, which makes the choice stable for a
            // burst of simultaneous requests arriving at an idle pool.
            (state.inflight(proxies[index].proxy_id()), index)
        }),
    }
}

/// The positions of the enabled proxies, in declaration order.
fn enabled_indices<P: SelectableProxy>(proxies: &[P]) -> impl Iterator<Item = usize> + '_ {
    proxies
        .iter()
        .enumerate()
        .filter(|(_, proxy)| proxy.proxy_enabled())
        .map(|(index, _)| index)
}

/// How many proxies are enabled.
fn enabled_count<P: SelectableProxy>(proxies: &[P]) -> usize {
    proxies.iter().filter(|proxy| proxy.proxy_enabled()).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// A proxy config with the fields the tests do not care about left at their defaults.
    fn proxy(id: &str, url: &str, enabled: bool, priority: i32) -> ProxyConfig {
        ProxyConfig {
            id: id.to_owned(),
            label: None,
            url: url.to_owned(),
            username: None,
            secret_ref: None,
            enabled,
            priority,
            extra: Default::default(),
        }
    }

    /// A one-shot fake proxy: accepts a single connection, runs `script`, and hands the test back
    /// every byte the client sent so the wire format can be asserted exactly.
    ///
    /// A real local `TcpListener` on port 0 rather than a mock, because half the value of these
    /// tests is proving the framing survives an actual socket with real buffering.
    fn fake_proxy<F, Fut>(script: F) -> (SocketAddr, JoinHandle<Vec<u8>>)
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Vec<u8>> + Send,
    {
        // Bind through std so callers need no extra `.await` to acquire the ephemeral port; the
        // listener is handed to Tokio before the spawned async script starts.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        listener.set_nonblocking(true).expect("nonblocking");
        let listener = TcpListener::from_std(listener).expect("tokio listener");
        let handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.expect("accept");
            script(socket).await
        });
        (addr, handle)
    }

    /// Read one byte at a time until `terminator` has been seen, so the bytes captured are exactly
    /// the request and not the start of what the client sends after the handshake.
    async fn read_until<S>(socket: &mut S, terminator: &[u8]) -> Vec<u8>
    where
        S: AsyncRead + Unpin,
    {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while !buf.ends_with(terminator) {
            socket.read_exact(&mut byte).await.expect("read");
            buf.push(byte[0]);
        }
        buf
    }

    /// Read exactly `n` bytes.
    async fn read_n(socket: &mut TcpStream, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        socket.read_exact(&mut buf).await.expect("read exact");
        buf
    }

    fn endpoint(url: &str) -> ProxyEndpoint {
        ProxyEndpoint::parse(url).expect("a parsable proxy url")
    }

    // ---------------------------------------------------------------- URL parsing

    #[test]
    fn proxy_urls_parse_into_scheme_host_port_and_auth() {
        type ParseCase<'a> = (
            &'a str,
            ProxyScheme,
            &'a str,
            u16,
            Option<(&'a str, &'a str)>,
        );
        let cases: &[ParseCase<'_>] = &[
            (
                "http://127.0.0.1:8080",
                ProxyScheme::Http,
                "127.0.0.1",
                8080,
                None,
            ),
            (
                "HTTP://Proxy.Example:3128/",
                ProxyScheme::Http,
                "Proxy.Example",
                3128,
                None,
            ),
            (
                "https://proxy.example:443",
                ProxyScheme::Https,
                "proxy.example",
                443,
                None,
            ),
            (
                "socks4://10.0.0.1:1080",
                ProxyScheme::Socks4,
                "10.0.0.1",
                1080,
                None,
            ),
            (
                "socks5://10.0.0.1:1080",
                ProxyScheme::Socks5,
                "10.0.0.1",
                1080,
                None,
            ),
            (
                "socks5h://proxy.example:1080",
                ProxyScheme::Socks5h,
                "proxy.example",
                1080,
                None,
            ),
            (
                "http://user:pass@127.0.0.1:8080",
                ProxyScheme::Http,
                "127.0.0.1",
                8080,
                Some(("user", "pass")),
            ),
            (
                "socks5://user@127.0.0.1:1080",
                ProxyScheme::Socks5,
                "127.0.0.1",
                1080,
                Some(("user", "")),
            ),
            (
                "socks5://[::1]:1080",
                ProxyScheme::Socks5,
                "::1",
                1080,
                None,
            ),
            (
                "http://[2001:db8::1]:8080",
                ProxyScheme::Http,
                "2001:db8::1",
                8080,
                None,
            ),
        ];
        for (url, scheme, host, port, auth) in cases {
            let parsed = ProxyEndpoint::parse(url).unwrap_or_else(|e| panic!("{url}: {e}"));
            assert_eq!(parsed.scheme, *scheme, "{url}");
            assert_eq!(parsed.host, *host, "{url}");
            assert_eq!(parsed.port, *port, "{url}");
            match auth {
                None => assert!(parsed.auth.is_none(), "{url}"),
                Some((user, password)) => {
                    let got = parsed.auth.expect("auth");
                    assert_eq!(
                        (got.username(), got.password()),
                        (*user, *password),
                        "{url}"
                    );
                }
            }
        }
    }

    #[test]
    fn proxy_urls_compose_their_authority_for_connect() {
        assert_eq!(endpoint("socks5://[::1]:1080").authority(), "[::1]:1080");
        assert_eq!(endpoint("http://proxy:8080").authority(), "proxy:8080");
    }

    #[test]
    fn proxy_url_rejections_name_the_problem() {
        type RejectionCheck = (&'static str, fn(&ProxyUrlError) -> bool);
        let cases: &[RejectionCheck] = &[
            ("", |e| matches!(e, ProxyUrlError::Empty)),
            ("   ", |e| matches!(e, ProxyUrlError::Empty)),
            ("127.0.0.1:8080", |e| {
                matches!(e, ProxyUrlError::MissingScheme { .. })
            }),
            ("http://", |e| {
                matches!(e, ProxyUrlError::MissingHost { .. })
            }),
            (
                "ftp://proxy:8080",
                |e| matches!(e, ProxyUrlError::UnsupportedScheme { scheme, .. } if scheme == "ftp"),
            ),
            ("socks6://proxy:1080", |e| {
                matches!(e, ProxyUrlError::UnsupportedScheme { .. })
            }),
            ("http://proxy", |e| {
                matches!(e, ProxyUrlError::MissingPort { .. })
            }),
            ("http://proxy:0", |e| {
                matches!(e, ProxyUrlError::InvalidPort { .. })
            }),
            ("http://proxy:99999", |e| {
                matches!(e, ProxyUrlError::InvalidPort { .. })
            }),
            ("http://proxy:https", |e| {
                matches!(e, ProxyUrlError::InvalidPort { .. })
            }),
            ("http://proxy:8080/health", |e| {
                matches!(e, ProxyUrlError::NotAnEndpoint { .. })
            }),
            ("http://proxy:8080?x=1", |e| {
                matches!(e, ProxyUrlError::NotAnEndpoint { .. })
            }),
            ("http://proxy:8080#frag", |e| {
                matches!(e, ProxyUrlError::NotAnEndpoint { .. })
            }),
            ("http://::1:8080", |e| {
                matches!(e, ProxyUrlError::Ipv6NeedsBrackets { .. })
            }),
            ("http://[::1:8080", |e| {
                matches!(e, ProxyUrlError::UnclosedIpv6 { .. })
            }),
            ("http://:pass@proxy:8080", |e| {
                matches!(e, ProxyUrlError::EmptyUsername)
            }),
            (
                "http://user:pa\r\nss@proxy:8080",
                |e| matches!(e, ProxyUrlError::AuthControlCharacter { field } if *field == "password"),
            ),
        ];
        for (url, check) in cases {
            match ProxyEndpoint::parse(url) {
                Ok(parsed) => panic!("{url} parsed unexpectedly: {parsed:?}"),
                Err(error) => assert!(check(&error), "{url} produced {error:?}"),
            }
        }
    }

    #[test]
    fn a_control_character_in_a_username_cannot_reach_a_header() {
        let error = ProxyAuth::new("user\r\nX-Injected: 1", "pass").unwrap_err();
        assert!(matches!(
            error,
            ProxyUrlError::AuthControlCharacter { field } if field == "username"
        ));
    }

    #[test]
    fn a_credential_never_prints_its_password() {
        let auth = ProxyAuth::new("alice", "hunter2").expect("valid");
        let rendered = format!("{auth:?}");
        assert!(rendered.contains("alice"));
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn config_credentials_come_from_the_username_and_the_vault() {
        let mut config = proxy("office", "http://proxy.example:8080", true, 0);
        config.username = Some("alice".to_owned());
        config.secret_ref = Some("proxy-office".to_owned());

        let resolved = ProxyEndpoint::from_config(&config, Some("hunter2")).expect("resolved");
        let auth = resolved.auth.expect("auth");
        assert_eq!(auth.username(), "alice");
        assert_eq!(auth.password(), "hunter2");

        // A configured username with no secret resolved is an error, not an anonymous dial.
        let error = ProxyEndpoint::from_config(&config, None).unwrap_err();
        assert!(
            matches!(error, ProxyUrlError::MissingProxyPassword { proxy } if proxy == "office")
        );

        // A secret with no username is a config bug worth naming.
        let mut nameless = proxy("office", "http://proxy.example:8080", true, 0);
        nameless.secret_ref = Some("proxy-office".to_owned());
        let error = ProxyEndpoint::from_config(&nameless, Some("hunter2")).unwrap_err();
        assert!(matches!(
            error,
            ProxyUrlError::PasswordWithoutUsername { .. }
        ));
    }

    #[test]
    fn userinfo_in_the_url_still_works_without_a_configured_username() {
        let config = proxy("office", "http://alice:hunter2@proxy.example:8080", true, 0);
        let auth = ProxyEndpoint::from_config(&config, None)
            .expect("url userinfo")
            .auth
            .expect("auth");
        assert_eq!((auth.username(), auth.password()), ("alice", "hunter2"));
    }

    #[test]
    fn basic_auth_matches_the_rfc_4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"alice:hunter2"), "YWxpY2U6aHVudGVyMg==");
    }

    #[test]
    fn proxy_targets_take_default_ports_from_the_url() {
        assert_eq!(
            ProxyTarget::from_url("https://api.example/v1/x").unwrap(),
            ProxyTarget::new("api.example", 443)
        );
        assert_eq!(
            ProxyTarget::from_url("http://127.0.0.1:20900/v1").unwrap(),
            ProxyTarget::new("127.0.0.1", 20900)
        );
        assert_eq!(
            ProxyTarget::from_url("http://[::1]/v1").unwrap(),
            ProxyTarget::new("::1", 80)
        );
        assert_eq!(ProxyTarget::new("::1", 443).authority(), "[::1]:443");
        assert!(matches!(
            ProxyTarget::from_url("ftp://example/x"),
            Err(ProxyUrlError::UnsupportedScheme { .. })
        ));
    }

    // ---------------------------------------------------------------- HTTP CONNECT

    #[tokio::test]
    async fn http_connect_writes_the_exact_request_and_opens_the_tunnel() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let request = read_until(&mut socket, b"\r\n\r\n").await;
            assert_eq!(
                String::from_utf8(request.clone()).expect("utf-8 request"),
                "CONNECT api.example:443 HTTP/1.1\r\nHost: api.example:443\r\n\r\n"
            );
            let mut socket = socket;
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\nProxy-Agent: fake\r\n\r\n")
                .await
                .expect("reply");
            // Anything the client writes now must arrive unmodified: the handshake must not have
            // swallowed a byte of tunnel data.
            let mut echo = [0u8; 4];
            socket.read_exact(&mut echo).await.expect("tunnel read");
            socket.write_all(&echo).await.expect("tunnel write");
            request
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://{addr}")).expect("url");
        let mut tunnel = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        tunnel.write_all(b"ping").await.expect("write");
        let mut echoed = [0u8; 4];
        tunnel.read_exact(&mut echoed).await.expect("read");
        assert_eq!(&echoed, b"ping");
        assert!(!tunnel.is_encrypted());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn http_connect_sends_proxy_authorization_for_a_credential() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let request = read_until(&mut socket, b"\r\n\r\n").await;
            assert_eq!(
                String::from_utf8(request.clone()).expect("utf-8"),
                "CONNECT api.example:443 HTTP/1.1\r\n\
                 Host: api.example:443\r\n\
                 Proxy-Authorization: Basic YWxpY2U6aHVudGVyMg==\r\n\
                 \r\n"
            );
            let mut socket = socket;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\n\r\n")
                .await
                .expect("reply");
            request
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://alice:hunter2@{addr}")).expect("url");
        open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn http_connect_reports_a_407_as_an_auth_failure() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let request = read_until(&mut socket, b"\r\n\r\n").await;
            let mut socket = socket;
            socket
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .expect("reply");
            request
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://{addr}")).expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("407 must fail");
        match &error {
            ProxyError::HttpAuthRequired {
                status,
                reason,
                target,
            } => {
                assert_eq!(*status, 407);
                assert_eq!(reason, "Proxy Authentication Required");
                assert_eq!(target, "api.example:443");
            }
            other => panic!("expected HttpAuthRequired, got {other:?}"),
        }
        assert!(error.is_auth_failure());
        assert!(!error.is_transient());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn http_connect_reports_a_refusal_with_the_proxys_own_words() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let request = read_until(&mut socket, b"\r\n\r\n").await;
            let mut socket = socket;
            socket
                .write_all(b"HTTP/1.1 403 Forbidden Host Not Allowed\r\n\r\n")
                .await
                .expect("reply");
            request
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://{addr}")).expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("403 must fail");
        match &error {
            ProxyError::HttpStatus { status, reason, .. } => {
                assert_eq!(*status, 403);
                assert_eq!(reason, "Forbidden Host Not Allowed");
            }
            other => panic!("expected HttpStatus, got {other:?}"),
        }
        assert!(!error.is_auth_failure());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn an_endless_http_reply_is_bounded_by_the_read_limit() {
        // A proxy that answers with headers and never terminates them: without a bound this test
        // would allocate until the machine gave up.
        let (addr, server) = fake_proxy(|mut socket| async move {
            let mut sent = Vec::new();
            let line = b"X-Filler: 0123456789012345678901234567890123456789\r\n";
            for _ in 0..200 {
                if socket.write_all(line).await.is_err() {
                    break;
                }
                sent.extend_from_slice(line);
            }
            sent
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://{addr}")).expect("url");
        let limits = TunnelLimits {
            max_reply_bytes: 512,
            ..TunnelLimits::default()
        };
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &limits,
            None,
        )
        .await
        .expect_err("must be bounded");
        assert!(
            matches!(
                error,
                ProxyError::ReplyTooLong(512) | ProxyError::HttpMalformed(_)
            ),
            "{error:?}"
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn a_silent_proxy_costs_one_timeout() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            // Read the request and then say nothing at all, which is the failure mode a timeout
            // exists for.
            let mut scratch = [0u8; 512];
            let _ = socket.read(&mut scratch).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            Vec::new()
        });

        let endpoint = ProxyEndpoint::parse(&format!("http://{addr}")).expect("url");
        let limits = TunnelLimits {
            reply_timeout: Duration::from_millis(80),
            ..TunnelLimits::default()
        };
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &limits,
            None,
        )
        .await
        .expect_err("must time out");
        assert!(
            matches!(error, ProxyError::ReplyTimeout { .. }),
            "{error:?}"
        );
        assert!(error.is_transient());
        let _ = server.await;
    }

    #[tokio::test]
    async fn a_malformed_status_line_is_reported_as_such() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let request = read_until(&mut socket, b"\r\n\r\n").await;
            let mut socket = socket;
            socket
                .write_all(b"PROXY 200 OK\r\n\r\n")
                .await
                .expect("reply");
            request
        });
        let endpoint = ProxyEndpoint::parse(&format!("http://{addr}")).expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("must fail");
        assert!(matches!(error, ProxyError::HttpMalformed(_)), "{error:?}");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn an_https_proxy_without_a_tls_connector_explains_the_gap_before_dialling() {
        // Port 1 on loopback: if this dialled first the error would be a connect failure, so the
        // error kind also asserts the ordering.
        let endpoint = ProxyEndpoint::parse("https://127.0.0.1:1").expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("must refuse");
        assert!(
            matches!(error, ProxyError::TlsConnectorRequired),
            "{error:?}"
        );
    }

    // ---------------------------------------------------------------- SOCKS5

    #[tokio::test]
    async fn socks5_chooses_auth_and_sends_the_exact_handshake_bytes() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let greeting = read_n(&mut socket, 2).await;
            assert_eq!(greeting, vec![0x05, 0x01], "one offered method");
            let method = read_n(&mut socket, 1).await;
            assert_eq!(
                method,
                vec![0x00],
                "no-auth is offered without a credential"
            );
            socket.write_all(&[0x05, 0x00]).await.expect("method reply");

            let head = read_n(&mut socket, 4).await;
            assert_eq!(head, vec![0x05, 0x01, 0x00, 0x01], "CONNECT, IPv4");
            let addr = read_n(&mut socket, 4).await;
            assert_eq!(addr, vec![127, 0, 0, 1]);
            let port = read_n(&mut socket, 2).await;
            assert_eq!(port, vec![0x01, 0xBB], "443 big-endian");
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .expect("reply");

            let mut echo = [0u8; 4];
            socket.read_exact(&mut echo).await.expect("tunnel read");
            socket.write_all(&echo).await.expect("tunnel write");
            vec![0x05, 0x00]
        });

        let endpoint = ProxyEndpoint::parse(&format!("socks5://{addr}")).expect("url");
        let mut tunnel = open_tunnel(
            &endpoint,
            &ProxyTarget::new("127.0.0.1", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        tunnel.write_all(b"ping").await.expect("write");
        let mut echoed = [0u8; 4];
        tunnel.read_exact(&mut echoed).await.expect("read");
        assert_eq!(&echoed, b"ping");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn socks5_offers_only_username_password_when_a_credential_exists() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let greeting = read_n(&mut socket, 2).await;
            assert_eq!(
                greeting,
                vec![0x05, 0x01],
                "offering no-auth alongside would allow a silent downgrade"
            );
            let method = read_n(&mut socket, 1).await;
            assert_eq!(method, vec![0x02], "username/password only");
            socket.write_all(&[0x05, 0x02]).await.expect("method reply");

            let auth_head = read_n(&mut socket, 2).await;
            assert_eq!(auth_head, vec![0x01, 0x05], "auth version, username length");
            assert_eq!(read_n(&mut socket, 5).await, b"alice".to_vec());
            let password_len = read_n(&mut socket, 1).await;
            assert_eq!(password_len, vec![0x07]);
            assert_eq!(read_n(&mut socket, 7).await, b"hunter2".to_vec());
            socket.write_all(&[0x01, 0x00]).await.expect("auth reply");

            let head = read_n(&mut socket, 4).await;
            assert_eq!(head, vec![0x05, 0x01, 0x00, 0x03], "domain address type");
            let len = read_n(&mut socket, 1).await;
            assert_eq!(len, vec![11]);
            assert_eq!(read_n(&mut socket, 11).await, b"api.example".to_vec());
            assert_eq!(read_n(&mut socket, 2).await, vec![0x01, 0xBB]);
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x03, 3, b'h', b'o', b'm', 0, 80, 0])
                .await
                .expect("reply");
            vec![0x02]
        });

        let endpoint =
            ProxyEndpoint::parse(&format!("socks5h://alice:hunter2@{addr}")).expect("url");
        open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn socks5_authentication_rejection_is_an_auth_failure() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let _ = read_n(&mut socket, 3).await;
            socket.write_all(&[0x05, 0x02]).await.expect("method");
            let _ = read_n(&mut socket, 2 + 5 + 1 + 7).await;
            socket.write_all(&[0x01, 0x01]).await.expect("auth failure");
            Vec::new()
        });

        let endpoint =
            ProxyEndpoint::parse(&format!("socks5://alice:hunter2@{addr}")).expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("must fail");
        assert!(
            matches!(error, ProxyError::SocksAuthFailed(0x01)),
            "{error:?}"
        );
        assert!(error.is_auth_failure());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn socks5_method_rejection_is_named() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let _ = read_n(&mut socket, 3).await;
            socket.write_all(&[0x05, 0xFF]).await.expect("no method");
            Vec::new()
        });
        let endpoint = ProxyEndpoint::parse(&format!("socks5://{addr}")).expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("must fail");
        assert!(
            matches!(error, ProxyError::SocksMethodRejected),
            "{error:?}"
        );
        assert!(error.is_auth_failure());
        server.await.expect("server");
    }

    #[tokio::test]
    async fn socks5_reply_codes_become_named_errors() {
        for code in [0x01u8, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x42] {
            let (addr, server) = fake_proxy(move |mut socket| async move {
                let _ = read_n(&mut socket, 3).await;
                socket.write_all(&[0x05, 0x00]).await.expect("method");
                let _ = read_n(&mut socket, 4 + 4 + 2).await;
                socket
                    .write_all(&[0x05, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                    .await
                    .expect("reply");
                Vec::new()
            });
            let endpoint = ProxyEndpoint::parse(&format!("socks5://{addr}")).expect("url");
            let error = open_tunnel(
                &endpoint,
                &ProxyTarget::new("127.0.0.1", 443),
                &TunnelLimits::default(),
                None,
            )
            .await
            .expect_err("must fail");
            match &error {
                ProxyError::SocksReply { code: got, reason } => {
                    assert_eq!(*got, code);
                    assert_eq!(*reason, socks5_reply_reason(code));
                }
                other => panic!("code {code:#04x} produced {other:?}"),
            }
            server.await.expect("server");
        }
    }

    #[tokio::test]
    async fn socks5h_sends_the_hostname_where_socks5_sends_an_ip() {
        // socks5 with an IP literal: ATYP 0x01.
        let (addr, server) = fake_proxy(|mut socket| async move {
            let _ = read_n(&mut socket, 3).await;
            socket.write_all(&[0x05, 0x00]).await.expect("method");
            let head = read_n(&mut socket, 4).await;
            assert_eq!(head, vec![0x05, 0x01, 0x00, 0x01]);
            let _ = read_n(&mut socket, 6).await;
            socket
                .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .expect("reply");
            Vec::new()
        });
        let endpoint = ProxyEndpoint::parse(&format!("socks5://{addr}")).expect("url");
        open_tunnel(
            &endpoint,
            &ProxyTarget::new("192.0.2.7", 80),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        server.await.expect("server");

        // socks5h with the same target shape: ATYP 0x03, the hostname, no local resolution.
        let (addr, server) = fake_proxy(|mut socket| async move {
            let _ = read_n(&mut socket, 3).await;
            socket.write_all(&[0x05, 0x00]).await.expect("method");
            let head = read_n(&mut socket, 4).await;
            assert_eq!(head, vec![0x05, 0x01, 0x00, 0x03]);
            assert_eq!(read_n(&mut socket, 1).await, vec![13]);
            assert_eq!(read_n(&mut socket, 13).await, b"api.example.c".to_vec());
            socket
                .write_all(&[
                    0x05, 0x00, 0x00, 0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                ])
                .await
                .expect("reply");
            Vec::new()
        });
        let endpoint = ProxyEndpoint::parse(&format!("socks5h://{addr}")).expect("url");
        open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example.c", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        server.await.expect("server");
    }

    // ---------------------------------------------------------------- SOCKS4

    #[tokio::test]
    async fn socks4_connects_with_ipv4_and_a_userid() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let prefix = read_n(&mut socket, 8).await;
            assert_eq!(prefix, vec![0x04, 0x01, 0x01, 0xBB, 192, 0, 2, 7]);
            let userid = read_until(&mut socket, b"\0").await;
            assert_eq!(userid, b"alice\0");
            // 8-byte SOCKS4 reply: VN, CD, port, IPv4 address.
            let mut socket = socket;
            socket
                .write_all(&[0x00, 0x5A, 0x01, 0xBB, 192, 0, 2, 7])
                .await
                .expect("reply");
            [prefix, userid].concat()
        });
        let endpoint = ProxyEndpoint::parse(&format!("socks4://alice@{addr}")).expect("url");
        let tunnel = open_tunnel(
            &endpoint,
            &ProxyTarget::new("192.0.2.7", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect("tunnel");
        assert!(!tunnel.is_encrypted());
        let sent = server.await.expect("server");
        assert_eq!(
            sent,
            vec![0x04, 0x01, 0x01, 0xBB, 192, 0, 2, 7, b'a', b'l', b'i', b'c', b'e', 0]
        );
    }

    #[tokio::test]
    async fn socks4a_encodes_a_hostname_and_reports_refusal() {
        let (addr, server) = fake_proxy(|mut socket| async move {
            let prefix = read_n(&mut socket, 8).await;
            assert_eq!(prefix, vec![0x04, 0x01, 0x01, 0xBB, 0, 0, 0, 1]);
            let userid = read_until(&mut socket, b"\0").await;
            assert_eq!(userid, b"\0");
            let hostname = read_until(&mut socket, b"\0").await;
            assert_eq!(hostname, b"api.example\0");
            let mut socket = socket;
            socket
                .write_all(&[0x00, 0x5B, 0, 0, 0, 0, 0, 0])
                .await
                .expect("reply");
            [prefix, userid, hostname].concat()
        });
        let endpoint = ProxyEndpoint::parse(&format!("socks4://{addr}")).expect("url");
        let error = open_tunnel(
            &endpoint,
            &ProxyTarget::new("api.example", 443),
            &TunnelLimits::default(),
            None,
        )
        .await
        .expect_err("refusal");
        assert!(matches!(
            error,
            ProxyError::Socks4Reply {
                code: 0x5B,
                reason: "request rejected or failed"
            }
        ));
        let sent = server.await.expect("server");
        assert_eq!(
            sent,
            vec![
                0x04, 0x01, 0x01, 0xBB, 0, 0, 0, 1, 0, b'a', b'p', b'i', b'.', b'e', b'x', b'a',
                b'm', b'p', b'l', b'e', 0
            ]
        );
    }

    // ---------------------------------------------------------------- selection

    #[test]
    fn proxy_selection_skips_disabled_and_empty_lists() {
        let proxies = vec![
            proxy("first", "http://proxy:8001", false, 0),
            proxy("second", "http://proxy:8002", false, -10),
        ];
        let state = ProxySelectionState::new(&proxies);
        for strategy in [
            ProxyStrategy::Priority,
            ProxyStrategy::Random,
            ProxyStrategy::RoundRobin,
            ProxyStrategy::LeastUsed,
        ] {
            assert!(choose(&proxies, strategy, &state).is_none());
            assert!(choose::<ProxyConfig>(&[], strategy, &ProxySelectionState::empty()).is_none());
        }
    }

    #[test]
    fn priority_selection_chooses_the_first_enabled_proxy_in_config_order() {
        let proxies = vec![
            proxy("first", "http://proxy:8001", true, 4),
            proxy("second", "http://proxy:8002", true, -2),
            proxy("disabled", "http://proxy:8003", false, -100),
        ];
        let state = ProxySelectionState::new(&proxies);
        assert_eq!(
            choose(&proxies, ProxyStrategy::Priority, &state)
                .unwrap()
                .id,
            "first"
        );
    }

    #[test]
    fn round_robin_cycles_over_enabled_proxies_in_config_order() {
        let proxies = vec![
            proxy("first", "http://proxy:8001", true, 0),
            proxy("disabled", "http://proxy:8002", false, 0),
            proxy("second", "http://proxy:8003", true, 0),
            proxy("third", "http://proxy:8004", true, 0),
        ];
        let state = ProxySelectionState::new(&proxies);
        let selected: Vec<_> = (0..7)
            .map(|_| {
                choose(&proxies, ProxyStrategy::RoundRobin, &state)
                    .unwrap()
                    .id
                    .as_str()
            })
            .collect();
        assert_eq!(
            selected,
            ["first", "second", "third", "first", "second", "third", "first"]
        );
    }

    #[test]
    fn least_used_selects_the_lowest_inflight_count_and_ties_by_order() {
        let proxies = vec![
            proxy("first", "http://proxy:8001", true, 0),
            proxy("second", "http://proxy:8002", true, 0),
            proxy("third", "http://proxy:8003", true, 0),
            proxy("disabled", "http://proxy:8004", false, 0),
        ];
        let state = ProxySelectionState::new(&proxies);
        let first_lease = state.lease("first");
        let second_lease = state.lease("second");
        let second_lease2 = state.lease("second");
        assert_eq!(
            choose(&proxies, ProxyStrategy::LeastUsed, &state)
                .unwrap()
                .id,
            "third"
        );
        drop(second_lease2);
        assert_eq!(
            choose(&proxies, ProxyStrategy::LeastUsed, &state)
                .unwrap()
                .id,
            "third"
        );
        drop(first_lease);
        assert_eq!(
            choose(&proxies, ProxyStrategy::LeastUsed, &state)
                .unwrap()
                .id,
            "first"
        );
        drop(second_lease);
        assert_eq!(
            choose(&proxies, ProxyStrategy::LeastUsed, &state)
                .unwrap()
                .id,
            "first"
        );
    }

    #[test]
    fn random_selection_stays_inside_the_enabled_set() {
        let proxies = vec![
            proxy("disabled", "http://proxy:8001", false, 0),
            proxy("first", "http://proxy:8002", true, 0),
            proxy("second", "http://proxy:8003", true, 0),
        ];
        let state = ProxySelectionState::new(&proxies);
        for _ in 0..512 {
            let selected = choose(&proxies, ProxyStrategy::Random, &state).unwrap();
            assert!(
                selected.id == "first" || selected.id == "second",
                "{}",
                selected.id
            );
        }
    }
}
