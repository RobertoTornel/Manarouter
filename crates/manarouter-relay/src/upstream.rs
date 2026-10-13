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

use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
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
/// A concrete enum rather than a boxed trait object: there are exactly two cases, and the
/// non-generic type keeps the call sites readable.
#[derive(Debug)]
pub enum Body {
    /// A streaming body from the upstream.
    Streaming(hyper::body::Incoming),
    /// A complete body already in memory. Used for synthesized responses such as a locally generated
    /// routing error, never for an upstream's answer.
    Complete(Bytes),
    /// No body.
    Empty,
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
    /// The shared hyper client. Its own pool is disabled; pooling is per-host via the connector so
    /// that `pool_idle` and `pool_max_idle_per_host` can be honoured.
    client: hyper_util::client::legacy::Client<
        hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
        Bytes,
    >,
}

impl Upstream {
    /// Build the client.
    ///
    /// Loads the webpki root set once. Doing it per request would be a measurable cost on every
    /// request for a value that never changes within a process lifetime.
    pub fn new(pool_idle: Duration, pool_max_idle_per_host: usize) -> Result<Self> {
        todo!()
    }

    /// Issue a request and return the response with its body unread.
    pub async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse> {
        let _ = request;
        todo!()
    }

    /// Read a response body completely, for the non-streaming paths that genuinely need it.
    ///
    /// Bounded by `limit` so that a hostile or broken upstream cannot exhaust memory. Returns
    /// [`UpstreamError::BodyTooLarge`] rather than truncating, because a truncated JSON document is
    /// worse than an error.
    pub async fn read_all(&self, body: Body, limit: usize) -> Result<Bytes> {
        let _ = (body, limit);
        todo!()
    }

    /// Open a connection to `base_url` and discard it, so the first real request finds a warm
    /// socket.
    ///
    /// Best effort: a failure here is logged and ignored, because a provider that is down at startup
    /// must not prevent the relay from starting.
    pub async fn prewarm(&self, base_url: &str, http2: Http2Preference) {
        let _ = (base_url, http2);
    }
}
