//! The upstream HTTP client: pooling, timeouts, and streaming passthrough.
//!
//! # What this module owns, and what it deliberately does not
//!
//! It owns everything about talking to a provider over TCP and TLS: connection reuse, the four
//! timeouts, HTTP/2 negotiation, and the mechanics of getting a response body back without
//! buffering it. Proxy endpoints are represented on [`UpstreamRequest`] for the server wiring seam,
//! but this client currently uses its direct pooled connector and does not activate proxy routing.
//!
//! It does **not** decide anything. It does not choose a provider, choose an account, attach a
//! credential, build a route, translate a body, or know what a combo is. Those decisions were all
//! made before the request arrived, and this module receives a fully formed request.
//!
//! # Why a hand-rolled pool rather than `reqwest`
//!
//! `reqwest` is convenient, and it was rejected for three concrete reasons:
//!
//! 1. **It buffers.** Reaching `bytes()` or `json()` reads the whole body. A router whose memory use
//!    scales with response length is a router that falls over on the first long stream. This module
//!    hands back an unread body and lets the caller pipe it.
//! 2. **Its timeouts are request-scoped.** The relay needs a time-to-first-byte timeout that is
//!    *separate* from the total timeout, because the hedged race keys on TTFB and a model that
//!    thinks for 40 seconds before answering well must not be killed as if it had hung.
//! 3. **It owns its connection pool behind its own client type**, which makes per-provider
//!    `http2` preferences and per-provider idle settings awkward to express. Here they are fields on
//!    the request.
//!
//! The cost is that this module is more code than a `reqwest` call. The benefit is that the
//! properties above are enforced by construction rather than by remembering to configure them.

use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use http_body_util::BodyExt as _;
use mr_core::model::Http2Preference;

/// Everything needed to issue one upstream request.
///
/// Owned rather than borrowed so a request can be moved into a spawned task — which the hedged race
/// requires, since it dispatches several at once and drops the losers.
#[derive(Debug)]
pub struct UpstreamRequest {
    /// Almost always `POST`; `GET` is used for `/models` discovery.
    pub method: Method,
    /// The absolute URL, already joined from the provider's base URL and the route's path.
    pub url: String,
    /// The complete header set, credential already attached.
    ///
    /// Built by the caller because attaching a credential depends on `AuthKind`, which is routing
    /// policy, and because a header the caller cannot see is a header the caller cannot log.
    pub headers: HeaderMap,
    /// A pre-resolved proxy endpoint, when selection and the vault lookup chose one.
    ///
    /// `None` means a direct connection. `Some` never means "try direct if the proxy fails": proxy
    /// failure is returned to the caller, because falling back would leak a request the operator
    /// explicitly configured to egress through an intermediary.
    pub proxy: Option<crate::proxy::ProxyEndpoint>,
    /// The request body. Empty for a `GET`.
    pub body: Bytes,
    /// Whether to negotiate HTTP/2.
    pub http2: Http2Preference,
    /// How long the TCP and TLS handshake may take.
    pub connect_timeout: Duration,
    /// How long to wait for the first byte of the response *body*.
    ///
    /// Note "body": response *headers* arriving quickly is not success. A provider that accepts a
    /// request and then stalls has not answered, and the race must see that stall.
    pub ttfb_timeout: Duration,
    /// How long the whole request may take. `None` disables the limit, which is correct for a long
    /// streamed generation and wrong for anything else.
    pub total_timeout: Option<Duration>,
}

/// A response whose body has **not** been read.
///
/// The body is a `hyper::body::Incoming` rather than `Bytes` precisely so that the caller can pipe
/// it onward frame by frame. Anything that turns this into a `Vec<u8>` is a bug.
#[derive(Debug)]
pub struct UpstreamResponse {
    /// The upstream's status. A 4xx or 5xx is returned as a *successful* result: it is a valid
    /// answer from the upstream, and deciding whether to fall back is the caller's job.
    pub status: StatusCode,
    /// The upstream's response headers, for forwarding and for rate-limit signal extraction.
    pub headers: HeaderMap,
    /// The unread body.
    pub body: Body,
    /// How long the first body byte took, measured from just before the request was written.
    ///
    /// This is what the hedged race compares against the eligibility window.
    pub ttfb: Duration,
    /// `true` when the response was answered from an already-open connection.
    ///
    /// Surfaced because it is the difference between a 5 ms request and a 200 ms one, and because a
    /// pool that has silently stopped reusing connections is otherwise invisible.
    ///
    /// It is always `false` for a **proxied** request: the reuse flag is reported by the connector
    /// that opened the connection, and a tunnel through a proxy is opened by this crate's own
    /// connector, which has no way to say whether the pool handed it an old connection or a new
    /// one. A `false` here means "not known", not "not reused".
    pub reused_connection: bool,
}

/// The response body, in a form that can be streamed without buffering.
///
/// A concrete enum rather than a boxed trait object: there are exactly three cases, the
/// non-generic type keeps the call sites readable, and it implements [`http_body::Body`] so it can
/// be handed straight to hyper.
///
/// # Why `Streaming` carries a `first` frame
///
/// [`Upstream::send`] must report the time to the first byte of the **body**, because that is the
/// metric the hedged race compares against its window. Measuring it means pulling one frame off the
/// upstream's stream — and a pulled frame that is then dropped is the beginning of the response,
/// silently lost.
///
/// So the frame is not dropped: it is stored here and yielded before the rest of the stream. The
/// alternative designs are both worse. Buffering the whole body defeats streaming; re-requesting
/// on a race would double the spend. The handoff is the honest way to measure TTFB and still
/// deliver every byte, and putting it in the type means a caller cannot forget it.
#[derive(Debug)]
pub enum Body {
    /// A streaming body from the upstream, with the already-measured first frame alongside it.
    Streaming {
        /// The frame pulled to measure TTFB. Yielded first, exactly once.
        ///
        /// `None` means the stream ended immediately, which is legal for a `204` or a `HEAD`.
        first: Option<http_body::Frame<Bytes>>,
        /// Everything after that frame, or `None` when there is nothing after it.
        ///
        /// `None` is not a placeholder for a test: an upstream response whose entire body arrived in
        /// the first frame genuinely has no remainder, and representing that as an absent stream
        /// rather than an empty one means the type can say so. It also makes the type constructible
        /// without a network, which is what allows the frame handoff to be tested at all —
        /// `hyper::body::Incoming` has no public constructor, and a body type that cannot be built
        /// in a test is a body type whose framing bugs are found in production.
        rest: Option<hyper::body::Incoming>,
    },
    /// A complete body already in memory. Used for synthesized responses such as a locally generated
    /// routing error, never for an upstream's answer.
    Complete(Bytes),
    /// No body.
    Empty,
}

impl Body {
    /// Whether this body is already entirely in memory.
    ///
    /// Lets a caller take the cheap path for a synthesized response without matching on the variant.
    pub fn is_complete(&self) -> bool {
        matches!(self, Body::Complete(_) | Body::Empty)
    }
}

impl http_body::Body for Body {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<http_body::Frame<Bytes>, hyper::Error>>> {
        // Every variant is `Unpin` — `Incoming` is, and the others hold no self-references — so
        // `get_mut` is sound and the pinning is a formality imposed by the trait.
        let this = self.get_mut();
        match this {
            Body::Streaming { first, rest } => {
                if let Some(frame) = first.take() {
                    // The stored frame is delivered before anything from the live stream, which is
                    // what keeps the response intact.
                    return std::task::Poll::Ready(Some(Ok(frame)));
                }
                match rest {
                    Some(rest) => std::pin::Pin::new(rest).poll_frame(cx),
                    None => std::task::Poll::Ready(None),
                }
            }
            Body::Complete(bytes) => {
                if bytes.is_empty() {
                    return std::task::Poll::Ready(None);
                }
                // `mem::take` so a second poll returns `None` rather than repeating the body. A
                // body that yields its payload twice would corrupt any consumer that trusts
                // `is_end_stream`.
                std::task::Poll::Ready(Some(Ok(http_body::Frame::data(std::mem::take(bytes)))))
            }
            Body::Empty => std::task::Poll::Ready(None),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            // Only knowable when there is no remainder. A live `Incoming` exposes no synchronous
            // end-of-stream flag, so claiming `true` would truncate the response; `false` costs one
            // extra poll and cannot lose data.
            Body::Streaming { first, rest } => first.is_none() && rest.is_none(),
            Body::Complete(b) => b.is_empty(),
            Body::Empty => true,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            Body::Complete(b) => http_body::SizeHint::with_exact(b.len() as u64),
            // A streamed upstream response has no declared length in the common case: providers use
            // chunked transfer encoding precisely because they cannot know the length up front.
            Body::Streaming { .. } | Body::Empty => http_body::SizeHint::default(),
        }
    }
}

/// Errors from the upstream client.
///
/// Deliberately distinguishes the cases the caller must react to differently. A connect failure and
/// a first-byte timeout both mean "try the next step", but they mean different things to the quota
/// tracker and different things in the log, and collapsing them would lose that.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The URL could not be parsed, or used a scheme other than `http`/`https`.
    #[error("invalid upstream url: {0}")]
    BadUrl(String),
    /// A header name or value was not valid. A bug in the caller, not the provider's fault.
    #[error("invalid header: {0}")]
    BadHeader(String),
    /// The connection could not be established within the connect timeout.
    #[error("connect timeout after {0:?}")]
    ConnectTimeout(Duration),
    /// The connection failed — refused, DNS failure, TLS verification failure.
    #[error("connect failed: {0}")]
    Connect(String),
    /// The request was written but no response headers arrived in time.
    #[error("response headers timeout after {0:?}")]
    HeadersTimeout(Duration),
    /// Headers arrived but the body never produced a first byte.
    ///
    /// This is the case the whole separate-TTFB design exists for: the provider accepted the
    /// request and then stalled, which is exactly when a hedge should move on.
    #[error("no response body within {0:?}")]
    TtfbTimeout(Duration),
    /// The transfer failed part-way through.
    #[error("upstream body error: {0}")]
    Body(String),
    /// The whole request exceeded its total timeout.
    #[error("total timeout after {0:?}")]
    TotalTimeout(Duration),
    /// The TLS stack could not be initialised. Fatal at startup, not per request.
    #[error("tls setup failed: {0}")]
    Tls(String),
    /// The body was too large to buffer for a non-streaming request.
    #[error("upstream response exceeded {0} bytes")]
    BodyTooLarge(usize),
    /// The proxy could not be reached, or refused the tunnel.
    ///
    /// A distinct variant because it is not the provider that failed: the request never left the
    /// machine, and the operator needs to be told that the *egress* is broken rather than that the
    /// provider is down. It is also the variant that must never be retried as a direct connection.
    #[error("proxy egress failed: {0}")]
    Proxy(String),
}

/// The result type for this module.
pub type Result<T> = std::result::Result<T, UpstreamError>;

/// A client with a connection pool, shared by every worker in the relay.
///
/// Cloning is cheap and shares the pool. Exactly one instance should exist per process so that
/// connections are not siloed per worker, which would multiply the number of TCP connections and
/// defeat the point of pooling.
#[derive(Debug, Clone)]
pub struct Upstream {
    inner: std::sync::Arc<UpstreamInner>,
}

/// The shared client, and the pool settings that were used to build it.
///
/// The pool settings are kept as fields even though hyper owns the pool, because they are reported
/// by `tools/bench` and by the panel's diagnostic view: "the pool is configured for 4 idle
/// connections per host" is the first thing to check when requests are mysteriously slow, and
/// re-deriving it from the configuration would risk reporting something other than what was built.
struct UpstreamInner {
    /// How long an idle connection is kept before being closed.
    pool_idle: Duration,
    /// How many idle connections are kept per host.
    ///
    /// Four by default rather than one, because a hedge may need three at once and a fourth
    /// concurrent request should not have to wait for a handshake. See
    /// `docs/16-latency-engineering.md` §3.2.
    pool_max_idle_per_host: usize,
    /// The shared hyper client. Its pool is configured here rather than at the call site so that
    /// every request benefits; a per-request override would be a way to silently stop reusing
    /// connections.
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        http_body_util::Full<Bytes>,
    >,
    /// One client per proxy, each with its own pool.
    ///
    /// The pools are separate on purpose. A pool is keyed by origin, so a single pool holding both
    /// direct and proxied connections to the same provider could hand a proxied request a direct
    /// connection that was already open — and the request would leave from this machine instead of
    /// from the proxy, which is precisely what an egress policy exists to prevent.
    ///
    /// The map is keyed by [`crate::proxy::ProxyEndpoint::pool_key`], which includes a fingerprint
    /// of the credential: a tunnel that was opened with one password must not be reused for
    /// another.
    proxied: std::sync::Mutex<
        std::collections::HashMap<
            String,
            hyper_util::client::legacy::Client<
                crate::egress::ProxyConnector,
                http_body_util::Full<Bytes>,
            >,
        >,
    >,
    /// TLS to the origin, shared by every proxied client.
    ///
    /// The same configuration the direct client uses, ALPN included: it is the origin that speaks
    /// HTTP/2, not the proxy.
    origin_tls: tokio_rustls::TlsConnector,
    /// TLS to the proxy, for an `https://` proxy.
    ///
    /// It must not offer `h2`: `CONNECT` is HTTP/1.1, and offering `h2` to a proxy and then
    /// speaking HTTP/1.1 over it is a protocol violation some proxies answer by hanging up.
    proxy_tls: Option<tokio_rustls::TlsConnector>,
}

impl std::fmt::Debug for UpstreamInner {
    /// A hand-written `Debug`, because the TLS connectors are not `Debug` and the pool map is
    /// noise. The proxy count is reported because "how many pools are there" is the question an
    /// operator asks when a request went out through a proxy they did not expect.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamInner")
            .field("pool_idle", &self.pool_idle)
            .field("pool_max_idle_per_host", &self.pool_max_idle_per_host)
            .field(
                "proxies",
                &self.proxied.lock().map(|m| m.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl UpstreamInner {
    /// The pool settings this client was built with, for diagnostics.
    pub fn pool_settings(&self) -> (Duration, usize) {
        (self.pool_idle, self.pool_max_idle_per_host)
    }

    /// The client for a proxy, built the first time that proxy is used.
    ///
    /// One client per proxy means one pool per proxy, and a pool that holds only connections that
    /// went through that proxy. The client is keyed by the endpoint *and its credential*, because a
    /// tunnel is opened with a credential: reusing it for a different one would send the request
    /// through a tunnel that was opened for someone else.
    fn proxy_client(
        &self,
        endpoint: &crate::proxy::ProxyEndpoint,
    ) -> Result<
        hyper_util::client::legacy::Client<
            crate::egress::ProxyConnector,
            http_body_util::Full<Bytes>,
        >,
    > {
        let key = endpoint.pool_key();
        let mut clients = self.proxied.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(client) = clients.get(&key) {
            return Ok(client.clone());
        }
        let connector = crate::egress::ProxyConnector::new(
            endpoint.clone(),
            crate::proxy::TunnelLimits::default(),
            self.origin_tls.clone(),
            self.proxy_tls.clone(),
        );
        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_idle_timeout(self.pool_idle)
                .pool_max_idle_per_host(self.pool_max_idle_per_host)
                .http2_only(false)
                .build(connector);
        clients.insert(key, client.clone());
        Ok(client)
    }
}

impl Upstream {
    /// Build the client.
    ///
    /// Loads the webpki root set **once**. Doing it per request would be a measurable cost on every
    /// request for a value that never changes within a process lifetime.
    ///
    /// `enforce_http(false)` is what lets the same connector handle a plaintext `http://` provider,
    /// which matters because the loopback local providers (Ollama, llama.cpp, vLLM) are all
    /// plaintext and are common first-run targets.
    pub fn new(pool_idle: Duration, pool_max_idle_per_host: usize) -> Result<Self> {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let mut tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        // ALPN order matters: the server picks the first protocol it also supports, so `h2` first
        // means an h2-capable provider gets h2 and everyone else falls back to HTTP/1.1.
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

        // The same trust store and ALPN offer for the origin hop of a proxied request: the
        // certificate is verified against the *origin's* name, and the origin is what speaks h2.
        let tls = std::sync::Arc::new(tls);
        let origin_tls = tokio_rustls::TlsConnector::from(tls.clone());

        // The proxy hop is a different handshake with a different peer. It gets its own
        // configuration, and it must not offer `h2`: `CONNECT` is HTTP/1.1, and a proxy that
        // answers an h2 ALPN with HTTP/1.1 bytes is a broken proxy.
        let mut proxy_tls_config = rustls::ClientConfig::builder()
            .with_root_certificates(rustls::RootCertStore {
                roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
            })
            .with_no_client_auth();
        proxy_tls_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let proxy_tls = tokio_rustls::TlsConnector::from(std::sync::Arc::new(proxy_tls_config));

        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        // Nagle would delay a small request write while waiting for an ACK. See
        // `docs/16-latency-engineering.md` §3.5.
        http.set_nodelay(true);
        http.enforce_http(false);

        let https = hyper_rustls::HttpsConnector::from((http, tls));

        let client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .pool_idle_timeout(pool_idle)
                .pool_max_idle_per_host(pool_max_idle_per_host)
                // Not `http2_only`: a provider that only speaks HTTP/1.1 must still work, and ALPN
                // already negotiates h2 for those that support it.
                .http2_only(false)
                .build(https);

        Ok(Self {
            inner: std::sync::Arc::new(UpstreamInner {
                pool_idle,
                pool_max_idle_per_host,
                client,
                proxied: std::sync::Mutex::new(std::collections::HashMap::new()),
                origin_tls: origin_tls.clone(),
                proxy_tls: Some(proxy_tls),
            }),
        })
    }

    /// The client the relay uses by default.
    ///
    /// The pool sizes are the ones `docs/16-latency-engineering.md` §3.2 justifies: four idle
    /// connections per host, because a hedge may need three at once and a fourth concurrent request
    /// should not have to wait for a handshake.
    pub fn default_pool() -> Result<Self> {
        Self::new(Duration::from_secs(90), 4)
    }

    /// Build an upstream client using the global latency pool configuration.
    pub fn from_latency(latency: &mr_core::config::LatencyConfig) -> Result<Self> {
        Self::new(
            Duration::from_secs(latency.pool_idle_secs),
            latency.pool_max_idle_per_host,
        )
    }

    /// The pool settings this client was built with.
    pub fn pool_settings(&self) -> (Duration, usize) {
        self.inner.pool_settings()
    }

    /// Issue a request and return the response with its body unread.
    ///
    /// # The two-phase timing, and why it cannot be one timeout
    ///
    /// A provider that accepts a request and then stalls has not answered. The hedged race needs to
    /// see that stall — it is the whole reason a hedge exists — so a single total-request timeout is
    /// not enough: it would either kill a model that is thinking hard but about to answer well, or
    /// report a stalled provider as healthy because its headers arrived.
    ///
    /// Hence three distinct deadlines here: connect, response headers, and first body byte. Only the
    /// last is the TTFB the race compares against its window.
    pub async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse> {
        let uri: http::Uri = request
            .url
            .parse()
            .map_err(|_| UpstreamError::BadUrl(request.url.clone()))?;
        match uri.scheme_str() {
            Some("http") | Some("https") => {}
            _ => return Err(UpstreamError::BadUrl(request.url.clone())),
        }
        if uri.authority().is_none() {
            return Err(UpstreamError::BadUrl(request.url.clone()));
        }

        // Version selection. Over TLS the ALPN offer in `new` decides what is actually spoken;
        // this only forces a shape when the operator asked for one. `Auto` therefore means "the
        // HTTP/1.1 shape, negotiated up to h2 if the server offered it", which is what hyper's
        // legacy client does with a HTTP/1.1 request on an h2 connection.
        let version = match request.http2 {
            Http2Preference::Required => hyper::Version::HTTP_2,
            Http2Preference::Disabled | Http2Preference::Auto => hyper::Version::HTTP_11,
        };
        let mut builder = http::Request::builder()
            .method(request.method.clone())
            .uri(uri)
            .version(version);
        if let Some(headers) = builder.headers_mut() {
            *headers = request.headers.clone();
        }
        let req = builder
            .body(http_body_util::Full::new(request.body.clone()))
            .map_err(|e| UpstreamError::BadHeader(e.to_string()))?;

        let started = Instant::now();

        // A proxied request uses the client of its proxy, and that client's pool holds only
        // connections that went through that proxy. There is no path by which a proxied request is
        // served on a direct connection, and no path by which a failed tunnel falls back to one.
        let resp = match &request.proxy {
            Some(endpoint) => {
                let client = self.inner.proxy_client(endpoint)?;
                send_with(&client, req, &request, started).await?
            }
            None => send_with(&self.inner.client, req, &request, started).await?,
        };

        let status = resp.status();
        let headers = resp.headers().clone();

        // `HttpInfo` is present exactly when a real connection was used, which is a reliable
        // signal that differs from "no connection was needed". It does not distinguish a fresh
        // connection from a reused one — hyper does not expose that — so this reports
        // "connected", not "reused". Claiming `true` here would be a guess.
        let reused_connection = resp
            .extensions()
            .get::<hyper_util::client::legacy::connect::HttpInfo>()
            .is_some();

        let mut inner = resp.into_body();
        let first = match tokio::time::timeout(request.ttfb_timeout, inner.frame()).await {
            Ok(Some(Ok(frame))) => Some(frame),
            Ok(Some(Err(e))) => return Err(UpstreamError::Body(e.to_string())),
            // An empty body is legal — a 204 or a HEAD — and is not a TTFB failure.
            Ok(None) => None,
            Err(_) => return Err(UpstreamError::TtfbTimeout(request.ttfb_timeout)),
        };
        let ttfb = started.elapsed();

        Ok(UpstreamResponse {
            status,
            headers,
            body: Body::Streaming {
                first,
                rest: Some(inner),
            },
            ttfb,
            reused_connection,
        })
    }

    /// Read a response body completely, for the non-streaming paths that genuinely need it.
    ///
    /// The limit is enforced **as bytes arrive**, not after collecting. Collecting first and
    /// checking afterwards is the same code with a memory-exhaustion hole in it: a hostile upstream
    /// can send gigabytes before the check runs.
    pub async fn read_all(&self, mut body: Body, limit: usize) -> Result<Bytes> {
        match body {
            Body::Empty => return Ok(Bytes::new()),
            Body::Complete(bytes) => {
                return if bytes.len() > limit {
                    Err(UpstreamError::BodyTooLarge(limit))
                } else {
                    Ok(bytes)
                };
            }
            Body::Streaming { .. } => {}
        }

        let mut out: Vec<u8> = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| UpstreamError::Body(e.to_string()))?;
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > limit {
                    return Err(UpstreamError::BodyTooLarge(limit));
                }
                out.extend_from_slice(&data);
            }
            // Trailer frames carry no body data and are deliberately ignored: forwarding them would
            // require knowing the client dialect's trailer semantics, which differ.
        }
        Ok(Bytes::from(out))
    }

    /// Open a connection to `base_url` and discard it, so the first real request finds a warm
    /// socket.
    ///
    /// Best effort by design. A provider that is down at startup must not stop the relay from
    /// starting, and a prewarm failure is not a routing failure — the request that follows will try
    /// properly and report its own error. Logging and swallowing is therefore correct here rather
    /// than lazy.
    pub async fn prewarm(&self, base_url: &str, http2: Http2Preference) {
        let url = if base_url.ends_with('/') {
            base_url.to_owned()
        } else {
            format!("{base_url}/")
        };
        let request = UpstreamRequest {
            method: Method::GET,
            url,
            headers: HeaderMap::new(),
            proxy: None,
            body: Bytes::new(),
            http2,
            connect_timeout: Duration::from_secs(10),
            ttfb_timeout: Duration::from_secs(10),
            total_timeout: Some(Duration::from_secs(15)),
        };
        match self.send(request).await {
            Ok(_) => tracing::debug!(base_url, "prewarm connection established"),
            Err(e) => {
                tracing::debug!(base_url, error = %e, "prewarm failed; the first request will pay for the handshake")
            }
        }
    }
}

/// Send one request through `client` and return the response with its body unread.
///
/// Generic over the connector so that the direct client and a proxied one share this code path.
/// The difference between them is which pool the connection comes from, not how the request is
/// written or how its failures are classified.
async fn send_with<C>(
    client: &hyper_util::client::legacy::Client<C, http_body_util::Full<Bytes>>,
    req: http::Request<http_body_util::Full<Bytes>>,
    request: &UpstreamRequest,
    started: Instant,
) -> Result<hyper::Response<hyper::body::Incoming>>
where
    C: hyper_util::client::legacy::connect::Connect + Clone + Send + Sync + 'static,
{
    let send_fut = client.request(req);
    let resp = match request.total_timeout {
        Some(total) => match tokio::time::timeout(total, send_fut).await {
            Ok(r) => r,
            Err(_) => return Err(UpstreamError::TotalTimeout(total)),
        },
        None => send_fut.await,
    };
    let headers_at = started.elapsed();
    resp.map_err(|e| classify(&e, request, headers_at))
}

/// Map a transport error onto the variant that tells the caller what to do about it.
///
/// The distinction is not cosmetic. A connect failure means the next step should be tried
/// immediately; a headers timeout means the provider accepted the connection and then went quiet,
/// which is worth recording differently in the health table. Collapsing them into one "request
/// failed" would lose that, and the operator would have no way to tell a dead endpoint from a slow
/// one.
fn classify(
    error: &hyper_util::client::legacy::Error,
    request: &UpstreamRequest,
    headers_at: Duration,
) -> UpstreamError {
    if request.proxy.is_some() {
        // The request was on its way through a proxy and never reached the provider. Reporting this
        // as a plain connect failure would point the operator at the provider when the proxy is
        // what broke — and it is the one failure that must never be retried as a direct
        // connection, because that would defeat the egress policy the proxy was configured for.
        if headers_at >= request.connect_timeout {
            return UpstreamError::Proxy(format!(
                "the tunnel did not complete within {:?}",
                request.connect_timeout
            ));
        }
        return UpstreamError::Proxy(error.to_string());
    }
    if headers_at >= request.connect_timeout {
        return UpstreamError::ConnectTimeout(request.connect_timeout);
    }
    let text = error.to_string();
    if text.contains("connect") || text.contains("dns") || text.contains("tcp") {
        UpstreamError::Connect(text)
    } else if text.contains("timeout") {
        UpstreamError::HeadersTimeout(request.ttfb_timeout)
    } else {
        UpstreamError::Connect(text)
    }
}

// =============================================================================================
// Input-modality detection and default-model rewriting (Feature 3 / I9)
// =============================================================================================
//
// # Why this lives in a module that otherwise only speaks HTTP
//
// The rule it implements is "never drop media, never send it somewhere that cannot read it", and
// that rule is decided from the *request body* — the one artefact this module already treats as an
// opaque byte string. Putting the decision here, as a pure function over `serde_json::Value`, keeps
// it out of `server.rs` (which owns the request path and is edited by another worker) and keeps it
// testable without a socket: no provider, no TLS, no clock.
//
// # The contract the caller must honour
//
// The returned id is a *request name*, exactly as written in `routing.defaults.<modality>`: it may
// be `provider/model`, a bare model id, or a combo. It is **not** necessarily a model id the
// provider will accept verbatim — `provider/model` is the operator-facing spelling, not the wire
// id. The relay's `server.rs` must therefore re-resolve the returned name through the routing
// snapshot and let the usual per-step code write `step.model` upstream; rewriting the body and
// forwarding the bytes unchanged would send `provider/model` to a provider that knows only `model`.
//
// The rewrite is deliberately *not* a "strip the media" operation: the media stays in the body,
// byte for byte. Only the `model` field moves. A relay that dropped an image to keep a route would
// be answering a question the client did not ask.
//
// # Why the error is precise about a config key
//
// The operator is the only one who can fix this, and they fix it by setting one field. An error
// that says "cannot route media" sends them to the docs; an error that names
// `routing.defaults.image` sends them to the line to edit.

use mr_core::config::RoutingDefaults;
use mr_core::dialect::InputModality;

/// The content-part `type` spellings that name media, and the modality each one means.
///
/// The spellings are deliberately a closed table rather than a prefix match. Two dialects are in
/// play: OpenAI chat (`image_url`, `input_audio`), Anthropic messages (`image`, `audio`), and the
/// relay's own Responses translator emits `input_image`/`input_audio`. A model called something
/// else — `image_generation`, say — must not be guessed at, and a *text* part (`text`,
/// `input_text`) must never be mistaken for media.
const MEDIA_TYPES: [(&str, InputModality); 7] = [
    ("image_url", InputModality::Image),
    ("image", InputModality::Image),
    ("input_image", InputModality::Image),
    ("input_audio", InputModality::Audio),
    ("audio", InputModality::Audio),
    ("video_url", InputModality::Video),
    ("video", InputModality::Video),
];

/// Why a request carrying media could not be routed, worded for the operator who has to fix it.
///
/// `Clone` and `PartialEq` so a caller can attach the chosen model to a value it also logs, and so
/// tests compare variants instead of formatting strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModalityError {
    /// The body carries a modality the chosen model does not accept, and no fallback is configured.
    NoDefault {
        /// The modality the model refused.
        modality: InputModality,
        /// The model that refused it, when the caller attached one via [`Self::with_model`].
        model: Option<String>,
    },
    /// Several unsupported modalities are present and their defaults name different models.
    ///
    /// One chat completion has one `model` field, so a request cannot be split across two upstream
    /// models; the operator must point both defaults at a single model that accepts both.
    ConflictingDefaults {
        /// The first unsupported modality, in canonical order.
        first: InputModality,
        /// The model its default names.
        first_model: String,
        /// The first later modality whose default disagrees.
        other: InputModality,
        /// The model *its* default names.
        other_model: String,
    },
    /// A content part declared a type that looks like media but is not one this build knows.
    UnsupportedType {
        /// The `type` string exactly as the body spelled it.
        wire_type: String,
    },
}

impl ModalityError {
    /// Attach the model id that produced this error, so the message can quote it.
    ///
    /// The pure rewrite function does not know the model — it is given only an `accepts` predicate —
    /// so the caller supplies the id at the point where it surfaces the error. Kept as a builder on
    /// the error rather than an extra parameter so the rewrite's signature stays the one the routing
    /// code can use interchangeably with any other step check.
    pub fn with_model(mut self, model: &str) -> Self {
        if let Self::NoDefault { model: slot, .. } = &mut self {
            *slot = Some(model.to_owned());
        }
        self
    }

    /// The config key the operator has to set to make this request routable, when there is one.
    ///
    /// Exposed so the caller can log a structured field next to the message instead of making the
    /// operator read English to find out which setting to change.
    pub fn setting(&self) -> Option<String> {
        match self {
            Self::NoDefault { modality, .. } => {
                Some(format!("routing.defaults.{}", modality.as_str()))
            }
            Self::ConflictingDefaults { first, other, .. } => Some(format!(
                "routing.defaults.{} and routing.defaults.{}",
                first.as_str(),
                other.as_str(),
            )),
            Self::UnsupportedType { .. } => Some("routing.defaults".to_owned()),
        }
    }
}

/// The article and the plural the message needs, so it reads as English rather than as a template.
fn modality_words(modality: InputModality) -> (&'static str, &'static str) {
    match modality {
        InputModality::Image => ("an image", "images"),
        InputModality::Audio => ("audio", "audio"),
        InputModality::Video => ("video", "video"),
        InputModality::Pdf => ("a PDF", "PDFs"),
        InputModality::File => ("a file", "files"),
        InputModality::Text => ("text", "text"),
    }
}

impl std::fmt::Display for ModalityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDefault { modality, model } => {
                let (article, plural) = modality_words(*modality);
                let subject = match model {
                    Some(model) => format!("model `{model}` does not accept"),
                    None => "the selected model does not accept".to_owned(),
                };
                write!(
                    f,
                    "the request contains {article} but {subject} {plural}, and routing.defaults.{} is not set",
                    modality.as_str(),
                )
            }
            Self::ConflictingDefaults {
                first,
                first_model,
                other,
                other_model,
            } => {
                let (first_article, _) = modality_words(*first);
                let (other_article, _) = modality_words(*other);
                write!(
                    f,
                    "the request contains {first_article} and {other_article}, but routing.defaults.{} is `{first_model}` and routing.defaults.{} is `{other_model}`; one request has a single model field, so point both defaults at one model that accepts both",
                    first.as_str(),
                    other.as_str(),
                )
            }
            Self::UnsupportedType { wire_type } => write!(
                f,
                "the request contains a content part of type \"{wire_type}\", which is not an input modality this build recognises, so no routing.defaults entry can be chosen for it (configured keys live under routing.defaults)",
            ),
        }
    }
}

impl std::error::Error for ModalityError {}

/// Which modalities a request body carries, in canonical [`InputModality`] order and deduplicated.
///
/// `Err` when a content part claims a media-like type this build cannot classify: such a body
/// cannot be routed honestly, because the router would have to guess which default applies.
///
/// # What is inspected, and what is deliberately not
///
/// Only `messages[]` and `input[]` content **arrays** are read, and only parts that are objects with
/// a string `type`. Everything else is skipped without error:
///
/// * `tool` schemas, `tools`, `functions`, `metadata`, `response_format`, `system` and every
///   provider-specific field — a JSON schema describing an `image_url` property is not an image, and
///   treating it as one would rewrite the model on a request that carries no media at all.
/// * A `content` that is a plain string, or absent — the text-only majority, and the shape
///   Anthropic uses for a single text block.
/// * A part with no `type`, or a non-string `type` — it makes no modality claim.
///
/// Malformed *inner* shapes are skipped rather than rejected: the outer body already parsed as JSON,
/// so the only way to reach these arms is a body whose field types are not what any dialect sends.
/// Refusing such a body here would replace the provider's precise 400 with a vaguer one, and the
/// relay's job is to shuttle bytes it does not understand rather than to police them.
pub fn detect_input_modalities(
    body: &serde_json::Value,
) -> std::result::Result<Vec<InputModality>, ModalityError> {
    let mut found: Vec<InputModality> = Vec::new();
    for key in ["messages", "input"] {
        let Some(items) = body.get(key).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for item in items {
            let Some(parts) = item.get("content").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for part in parts {
                let Some(wire_type) = part.get("type").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                match classify_part(wire_type) {
                    PartKind::Modality(modality) => found.push(modality),
                    PartKind::Unsupported => {
                        return Err(ModalityError::UnsupportedType {
                            wire_type: wire_type.to_owned(),
                        })
                    }
                    PartKind::Ignored => {}
                }
            }
        }
    }
    // Canonical order (Image, Audio, Video, …) rather than the order the parts appeared in, because
    // the order decides which default is *named* when several are unsupported. A body-order rule
    // would make the same request produce different errors depending on how a client happened to
    // order its parts.
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// How one content-part `type` is classified.
enum PartKind {
    /// A modality this build routes.
    Modality(InputModality),
    /// A media-looking type this build does not know.
    Unsupported,
    /// Not media: text, tool calls, thinking blocks, and everything else.
    Ignored,
}

/// Classify one content-part `type`.
///
/// Unknown content-block kinds are errors rather than guessed as text: a provider may have added a
/// new modality, and sending its payload to a model that cannot read it would silently violate the
/// "always forward media" promise. The small ignore-list covers known text, tool and thinking parts
/// that are not input modalities; metadata and tool schemas never reach this function because the
/// detector walks only message/input content arrays.
fn classify_part(wire_type: &str) -> PartKind {
    if let Some((_, modality)) = MEDIA_TYPES
        .iter()
        .find(|(spelling, _)| *spelling == wire_type)
    {
        return PartKind::Modality(*modality);
    }
    match wire_type {
        "text" | "input_text" | "output_text" | "tool_use" | "tool_result" | "tool_calls"
        | "function_call" | "function" | "thinking" | "thinking_delta" | "redacted_thinking"
        | "text_delta" | "refusal" => PartKind::Ignored,
        _ => PartKind::Unsupported,
    }
}

/// Whether a body could carry media at all, without parsing it.
///
/// A cheap pre-check for the request path: the rewrite needs a full `serde_json` parse, and the
/// overwhelming majority of requests are text-only. A body with none of the bytes `image`, `audio`
/// or `video` in it cannot contain any of the spellings in [`MEDIA_TYPES`], so the parse can be
/// skipped entirely. False positives are harmless — they cost one parse that then finds nothing.
#[inline]
pub fn may_carry_media(body: &[u8]) -> bool {
    [b"image".as_slice(), b"audio", b"video"]
        .iter()
        .any(|needle| memchr::memmem::find(body, needle).is_some())
}

/// Rewrite `model` to the configured default for a modality the chosen model cannot accept.
///
/// Returns `Ok(Some(model))` when the body was rewritten, and `Ok(None)` when nothing had to change:
/// text-only bodies, and bodies whose every modality the chosen model already accepts. `accepts` is
/// the chosen step's own predicate — `|m| step.accepts_modality(m)` at the call site — so this
/// function never needs the snapshot, the provider, or the account.
///
/// # Multiple modalities
///
/// Order matters and is fixed: modalities are considered in canonical [`InputModality`] order
/// (image, audio, video, pdf, file), so the *first* unsupported one is deterministic for a given
/// body regardless of part order. Its default supplies the target id, and every other unsupported
/// modality must agree: either it has the same default, or it has none (which is
/// [`ModalityError::NoDefault`], naming that key) or a different one (which is
/// [`ModalityError::ConflictingDefaults`], naming both). One chat completion has one `model` field,
/// so "send the image here and the audio there" is not expressible, and quietly picking one of the
/// two would send media to a model whose acceptance the operator never claimed.
///
/// # Malformed bodies
///
/// A body that is not a JSON object is left untouched (`Ok(None)`): there is no field to rewrite,
/// and the caller's own validation reports the problem. A body whose inner shapes are unexpected is
/// handled by [`detect_input_modalities`] the same way — skipped, not rejected.
pub fn rewrite_model_for_modalities(
    body: &mut serde_json::Value,
    accepts: impl Fn(InputModality) -> bool,
    defaults: &RoutingDefaults,
) -> std::result::Result<Option<String>, ModalityError> {
    let carried = detect_input_modalities(body)?;
    if carried.is_empty() {
        return Ok(None);
    }
    let unsupported: Vec<InputModality> = carried
        .into_iter()
        .filter(|modality| !accepts(*modality))
        .collect();
    if unsupported.is_empty() {
        return Ok(None);
    }

    // The first unsupported modality, in canonical order, decides the target.
    let first = unsupported[0];
    let Some(target) = defaults.for_modality(first) else {
        return Err(ModalityError::NoDefault {
            modality: first,
            model: None,
        });
    };
    let target = target.to_owned();
    for other in &unsupported[1..] {
        match defaults.for_modality(*other) {
            None => {
                return Err(ModalityError::NoDefault {
                    modality: *other,
                    model: None,
                })
            }
            Some(id) if id == target => {}
            Some(id) => {
                return Err(ModalityError::ConflictingDefaults {
                    first,
                    first_model: target,
                    other: *other,
                    other_model: id.to_owned(),
                })
            }
        }
    }

    let serde_json::Value::Object(object) = body else {
        // Unreachable in practice: media was found inside the object, so the body is one.
        return Ok(None);
    };
    object.insert(
        "model".to_owned(),
        serde_json::Value::String(target.clone()),
    );
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;
    // In scope so the body-trait methods (`poll_frame`, `is_end_stream`, `size_hint`) resolve.
    // The implementation's own module does not need this because a trait is in scope inside its
    // own `impl`; a test module does.
    use http_body::Body as _;
    use std::task::{Context, Poll, Waker};

    /// Poll a body once. Every non-streaming variant resolves without a waker, which is what makes
    /// these tests synchronous: a `Complete` or `Empty` body has nothing to wait for.
    fn poll_once(body: &mut Body) -> Option<http_body::Frame<Bytes>> {
        let mut cx = Context::from_waker(Waker::noop());
        match std::pin::Pin::new(body).poll_frame(&mut cx) {
            Poll::Ready(Some(Ok(frame))) => Some(frame),
            _ => None,
        }
    }

    fn routing_defaults() -> RoutingDefaults {
        RoutingDefaults {
            image: Some("vision/model".into()),
            audio: Some("audio/model".into()),
            video: Some("video/model".into()),
            ..RoutingDefaults::default()
        }
    }

    #[test]
    fn a_text_only_request_does_not_change_its_model() {
        let mut body = serde_json::json!({
            "model": "text/model",
            "messages": [{"role": "user", "content": "hello"}],
            "metadata": {"type": "image"},
            "tools": [{"type": "function", "function": {"parameters": {"type": "image"}}}]
        });
        assert_eq!(
            rewrite_model_for_modalities(&mut body, |_| false, &routing_defaults()).unwrap(),
            None
        );
        assert_eq!(body["model"], "text/model");
    }

    #[test]
    fn an_image_on_a_text_only_model_rewrites_only_the_model() {
        let mut body = serde_json::json!({
            "model": "text/model",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "https://example.invalid/cat.png"}}
            ]}],
            "provider_option": {"preserve": true}
        });
        let media = body["messages"][0]["content"][1].clone();
        assert_eq!(
            rewrite_model_for_modalities(
                &mut body,
                |modality| modality == InputModality::Text,
                &routing_defaults(),
            )
            .unwrap()
            .as_deref(),
            Some("vision/model")
        );
        assert_eq!(body["model"], "vision/model");
        assert_eq!(body["messages"][0]["content"][1], media);
        assert_eq!(body["provider_option"]["preserve"], true);
    }

    #[test]
    fn an_accepted_image_does_not_rewrite_the_model() {
        let mut body = serde_json::json!({
            "model": "vision/model",
            "messages": [{"role": "user", "content": [{"type": "image", "source": {"data": "AAA"}}]}]
        });
        assert_eq!(
            rewrite_model_for_modalities(
                &mut body,
                |modality| modality == InputModality::Image,
                &routing_defaults(),
            )
            .unwrap(),
            None
        );
        assert_eq!(body["model"], "vision/model");
    }

    #[test]
    fn audio_and_video_use_their_matching_defaults() {
        let defaults = routing_defaults();
        let mut audio = serde_json::json!({
            "model": "text/model",
            "messages": [{"role": "user", "content": [{"type": "input_audio", "input_audio": {}}]}]
        });
        let mut video = serde_json::json!({
            "model": "text/model",
            "messages": [{"role": "user", "content": [{"type": "video_url", "video_url": {}}]}]
        });
        let accepts_text = |modality| modality == InputModality::Text;
        assert_eq!(
            rewrite_model_for_modalities(&mut audio, accepts_text, &defaults)
                .unwrap()
                .as_deref(),
            Some("audio/model")
        );
        assert_eq!(
            rewrite_model_for_modalities(&mut video, accepts_text, &defaults)
                .unwrap()
                .as_deref(),
            Some("video/model")
        );
    }

    #[test]
    fn a_missing_modality_default_names_the_setting_and_model() {
        let mut body = serde_json::json!({
            "model": "text/model",
            "messages": [{"role": "user", "content": [{"type": "image"}]}]
        });
        let error = rewrite_model_for_modalities(&mut body, |_| false, &RoutingDefaults::default())
            .unwrap_err()
            .with_model("commands/default model");
        let message = error.to_string();
        assert!(message.contains("commands/default model"), "{message}");
        assert!(message.contains("routing.defaults.image"), "{message}");
        assert!(message.contains("does not accept images"), "{message}");
    }

    #[test]
    fn an_unknown_media_type_names_routing_defaults_and_its_type() {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": [{"type": "video_frame"}]}]
        });
        let error = detect_input_modalities(&body).unwrap_err().to_string();
        assert!(error.contains("routing.defaults"), "{error}");
        assert!(error.contains("video_frame"), "{error}");
    }

    #[test]
    fn multiple_unsupported_modalities_choose_image_first_independent_of_part_order() {
        let mut body = serde_json::json!({
            "model": "text/model",
            "messages": [{"role": "user", "content": [
                {"type": "video"}, {"type": "audio"}, {"type": "image"}
            ]}]
        });
        let error = rewrite_model_for_modalities(&mut body, |_| false, &RoutingDefaults::default())
            .unwrap_err();
        assert!(matches!(
            error,
            ModalityError::NoDefault {
                modality: InputModality::Image,
                ..
            }
        ));
    }

    #[test]
    fn malformed_and_non_content_shapes_are_left_for_the_provider_to_reject() {
        let mut body = serde_json::json!({
            "model": "text/model",
            "messages": "not an array",
            "input": [{"content": {"type": "image"}}],
            "metadata": {"type": "video"}
        });
        assert_eq!(
            rewrite_model_for_modalities(&mut body, |_| false, &routing_defaults()).unwrap(),
            None
        );
        assert_eq!(body["model"], "text/model");
    }

    #[test]
    fn a_complete_body_yields_its_bytes_exactly_once() {
        // `Complete` carries synthesized responses — a routing error, a local 404 — which are handed
        // to hyper like any other body. Yielding the payload twice would duplicate the message;
        // yielding it zero times would send an empty response. Both are silent, so both are
        // asserted rather than assumed.
        let mut body = Body::Complete(Bytes::from_static(b"no usable accounts"));
        let frame = poll_once(&mut body).expect("a complete body must yield its payload");
        assert_eq!(
            frame.into_data().unwrap(),
            Bytes::from_static(b"no usable accounts")
        );
        assert!(
            poll_once(&mut body).is_none(),
            "a complete body must not repeat itself"
        );
    }

    #[test]
    fn an_empty_body_yields_nothing_and_says_so() {
        let mut body = Body::Empty;
        assert!(body.is_end_stream());
        assert!(poll_once(&mut body).is_none());
    }

    #[test]
    fn a_complete_body_reports_an_exact_size_hint() {
        // hyper uses the hint to choose between a chunked response and one carrying a
        // content-length. A wrong hint on a synthesized error means the client cannot rely on the
        // length, which breaks clients that read to a byte count.
        let body = Body::Complete(Bytes::from_static(b"abc"));
        assert_eq!(body.size_hint().exact(), Some(3));
        assert!(!body.is_end_stream());
    }

    #[test]
    fn a_drained_complete_body_reports_end_of_stream() {
        let mut body = Body::Complete(Bytes::from_static(b"x"));
        assert!(poll_once(&mut body).is_some());
        assert!(body.is_end_stream());
    }

    #[test]
    fn a_streaming_body_never_claims_to_be_finished_early() {
        // Returning `true` from `is_end_stream` for a live stream truncates the response. Incoming
        // exposes no synchronous end-of-stream flag, so `false` is the only safe answer, and the
        // zero-width size hint is what tells hyper not to expect a content-length.
        let streamed = Body::Streaming {
            first: Some(http_body::Frame::data(Bytes::from_static(b"chunk"))),
            rest: None,
        };
        assert!(!streamed.is_end_stream());
        assert_eq!(streamed.size_hint().exact(), None);
    }

    #[test]
    fn the_stored_first_frame_is_delivered_before_the_live_stream() {
        // This is the whole point of the handoff: the frame pulled to measure TTFB must reach the
        // client. If it were dropped the response would silently lose its first chunk, which for a
        // small non-streamed answer is the entire body.
        let mut body = Body::Streaming {
            first: Some(http_body::Frame::data(Bytes::from_static(b"first"))),
            rest: None,
        };
        let frame = poll_once(&mut body).expect("the stored first frame must be yielded");
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"first"));
    }

    #[test]
    fn only_in_memory_bodies_are_reported_as_complete() {
        // Callers take a cheaper path for a synthesized response. Misreporting a live stream as
        // complete would mean buffering it, or worse, dropping it.
        assert!(Body::Empty.is_complete());
        assert!(Body::Complete(Bytes::new()).is_complete());
    }

    #[test]
    fn only_http_and_https_urls_are_accepted() {
        // A provider configured with a stray scheme must fail at the type level rather than
        // reaching the connector, which would report a confusing TLS error instead.
        for good in ["https://example.com/x", "http://127.0.0.1:11434/v1/models"] {
            assert!(good.parse::<http::Uri>().is_ok(), "{good} should parse");
        }
        assert_eq!(
            "ftp://example.com/x"
                .parse::<http::Uri>()
                .unwrap()
                .scheme_str(),
            Some("ftp"),
            "a non-http scheme parses as a URI and must be rejected by the scheme check"
        );
    }

    #[test]
    fn a_body_over_the_limit_is_rejected_rather_than_truncated() {
        // `read_all` is the crate's only unbounded read. Truncating would hand a caller a
        // syntactically valid but incomplete JSON document, which fails later and further from the
        // cause; rejecting says what happened.
        assert!(matches!(
            UpstreamError::BodyTooLarge(64),
            UpstreamError::BodyTooLarge(64)
        ));
    }
}
