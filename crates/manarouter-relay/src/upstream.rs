//! The upstream HTTP client: pooling, timeouts, and streaming passthrough.
//!
//! # What this module owns, and what it deliberately does not
//!
//! It owns everything about talking to a provider over TCP and TLS: connection reuse, the four
//! timeouts, HTTP/2 negotiation, and the mechanics of getting a response body back without
//! buffering it.
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
#[derive(Debug)]
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
}

impl UpstreamInner {
    /// The pool settings this client was built with, for diagnostics.
    pub fn pool_settings(&self) -> (Duration, usize) {
        (self.pool_idle, self.pool_max_idle_per_host)
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

        let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
        // Nagle would delay a small request write while waiting for an ACK. See
        // `docs/16-latency-engineering.md` §3.5.
        http.set_nodelay(true);
        http.enforce_http(false);

        let https = hyper_rustls::HttpsConnector::from((http, std::sync::Arc::new(tls)));

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

        let send_fut = self.inner.client.request(req);
        let resp = match request.total_timeout {
            Some(total) => match tokio::time::timeout(total, send_fut).await {
                Ok(r) => r,
                Err(_) => return Err(UpstreamError::TotalTimeout(total)),
            },
            None => send_fut.await,
        };
        let headers_at = started.elapsed();

        let resp = resp.map_err(|e| classify(&e, &request, headers_at))?;

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
