//! The HTTPS client every quota fetcher goes through, and the seam that lets tests replace it.
//!
//! Two separations this file exists to keep:
//!
//! * **Transport vs interpretation.** [`QuotaHttp`] knows how to send one request and hand back the
//!   status, the headers and the body — nothing about quota shapes, provider dialects or JSON
//!   paths. A fetcher (Phase C) builds a [`QuotaRequest`], awaits the [`QuotaResponse`], and does
//!   all the interpreting itself.
//! * **Real vs fake.** The registry holds an `Arc<dyn QuotaHttp>`, so a unit test injects a fake
//!   that replays captured upstream JSON and touches no socket. The real implementation
//!   ([`HyperQuotaHttp`]) is the only thing in this module that can reach the network.
//!
//! Construction mirrors `oauth/flow.rs` (and, behind it, `manarouter-relay/src/upstream.rs`):
//! webpki roots rather than the system store, ALPN `h2` then `http/1.1`, Nagle off. Quota reads are
//! rare control-plane calls, so one shared client with a bounded per-request timeout is enough.
//!
//! Two error classes, deliberately distinct:
//!
//! * A malformed URL or header is a **configuration** error ([`Error::Config`]) and is caught
//!   before anything is sent — a header a fetcher builds out of a credential must never panic the
//!   process (`panic = "abort"` in this workspace), so headers are validated rather than passed to
//!   `Request::builder` unchecked.
//! * A connect/read failure or a timeout is [`Error::Io`]. Error messages name the *host* only: a
//!   quota URL can carry a token in its query string, and a header value usually *is* one, so
//!   neither is ever echoed.

use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::CONTENT_TYPE;
use http::{HeaderMap, Method, Uri};
use http_body_util::{BodyExt as _, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::TokioExecutor;

use crate::error::{Error, Result};

/// Bound on one quota request unless a fetcher asks for something else. Matches OmniRoute's
/// `QUOTA_ENDPOINT_TIMEOUT_MS`, so one unreachable upstream cannot hang a whole panel refresh.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// One HTTP request a quota fetcher wants made.
///
/// Owned rather than borrowed so a test double can keep the requests it was handed, and so the
/// trait method needs no lifetime plumbing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaRequest {
    /// The HTTP method.
    pub method: Method,
    /// Absolute URL, `http` or `https`.
    pub url: String,
    /// Request headers, in the order they were added.
    pub headers: Vec<(String, String)>,
    /// Request body; empty for a `GET`.
    pub body: Bytes,
    /// Per-request timeout, or `None` for the client's default.
    pub timeout: Option<Duration>,
}

impl QuotaRequest {
    /// A `GET` with no headers and no body.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::GET,
            url: url.into(),
            headers: Vec::new(),
            body: Bytes::new(),
            timeout: None,
        }
    }

    /// A `POST` with a JSON body and the matching `Content-Type`.
    pub fn post_json(url: impl Into<String>, body: impl Into<Bytes>) -> Self {
        Self {
            method: Method::POST,
            url: url.into(),
            headers: vec![(
                CONTENT_TYPE.as_str().to_owned(),
                "application/json".to_owned(),
            )],
            body: body.into(),
            timeout: None,
        }
    }

    /// Set the method explicitly, for the handful of upstreams that want `HEAD` or `DELETE`.
    pub fn method(mut self, method: Method) -> Self {
        self.method = method;
        self
    }

    /// Add one header. Added last wins for a repeated name only if the upstream honours that;
    /// otherwise both are sent, which is what an upstream that looks at the first one expects to be
    /// avoided by the caller.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Add `Authorization: Bearer <token>` — the credential shape most OpenAI-compatible upstreams
    /// use. The token is never logged.
    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }

    /// Replace the body.
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Bound this one request instead of using [`DEFAULT_TIMEOUT`].
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// One HTTP reply, after the body has been read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers. Names are stored lowercased, as they arrive on the wire.
    pub headers: Vec<(String, String)>,
    /// Response body, exactly as received.
    pub body: Bytes,
}

impl QuotaResponse {
    /// Look up a header by name, case-insensitively (the stored names are lowercased).
    pub fn header(&self, name: &str) -> Option<&str> {
        let wanted = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(existing, _)| *existing == wanted)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the status is `2xx`.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The body as text, lossily decoded — upstream error bodies are text or JSON, never binary.
    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Parse the body as JSON, mapping a parse failure onto [`Error::Json`].
    pub fn json(&self) -> Result<serde_json::Value> {
        Ok(serde_json::from_slice(&self.body)?)
    }

    /// The `Retry-After` header as an absolute instant in epoch milliseconds.
    ///
    /// Unlike every other timestamp an upstream sends, `Retry-After` may be *delta-seconds* — a
    /// duration, not an instant — so it needs `now` to become a time; an HTTP-date form is parsed
    /// as the absolute instant it is. `None` when the header is absent or unparsable.
    pub fn retry_after_millis(&self) -> Option<i64> {
        let raw = self.header("retry-after")?.trim();
        if let Ok(seconds) = raw.parse::<u64>() {
            let delta = i64::try_from(seconds).ok()?.saturating_mul(1000);
            return Some(crate::ids::now_millis().saturating_add(delta));
        }
        crate::quota::types::parse_reset_time_str(raw)
    }
}

/// The transport seam. Object-safe on purpose: the registry holds `Arc<dyn QuotaHttp>`.
///
/// # What Phase C's fetchers call
///
/// ```ignore
/// let response = ctx
///     .http
///     .request(QuotaRequest::get(url).bearer(ctx.credential.as_ref()?.expose()))
///     .await?;
/// if !response.is_success() {
///     return Err(Error::InvalidState(format!("upstream answered {}", response.status)));
/// }
/// let body = response.json()?;
/// ```
///
/// One method, and it returns everything a fetcher needs to interpret: status, headers, body.
#[async_trait]
pub trait QuotaHttp: Send + Sync + std::fmt::Debug {
    /// Send one request and read its whole body.
    ///
    /// Errors are [`Error::Config`] for a malformed URL or header (nothing was sent) and
    /// [`Error::Io`] for a transport failure or timeout.
    async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse>;
}

type HttpsClient = LegacyClient<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// The real client: `hyper` + `hyper-rustls` over the webpki root set.
pub struct HyperQuotaHttp {
    client: HttpsClient,
    default_timeout: Duration,
}

impl HyperQuotaHttp {
    /// Build the client. The webpki roots are loaded once per process — they never change.
    pub fn new() -> Result<Self> {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    /// Build the client with a different default timeout.
    pub fn with_timeout(default_timeout: Duration) -> Result<Self> {
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
        let client = LegacyClient::builder(TokioExecutor::new()).build(https);
        Ok(Self {
            client,
            default_timeout,
        })
    }

    /// The process-wide instance, built on first use.
    pub fn shared() -> Result<&'static HyperQuotaHttp> {
        static HTTP: OnceLock<HyperQuotaHttp> = OnceLock::new();
        if let Some(existing) = HTTP.get() {
            return Ok(existing);
        }
        let built = Self::new()?;
        Ok(HTTP.get_or_init(|| built))
    }

    /// The timeout used when a [`QuotaRequest`] does not set its own.
    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }
}

impl std::fmt::Debug for HyperQuotaHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HyperQuotaHttp")
            .field("default_timeout", &self.default_timeout)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl QuotaHttp for HyperQuotaHttp {
    async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
        let QuotaRequest {
            method,
            url,
            headers,
            body,
            timeout,
        } = request;

        // A query string can carry a token, so errors name the host and never the whole URL.
        let uri: Uri = url
            .parse()
            .map_err(|_| Error::Config("a quota request url was not a valid URI".to_owned()))?;
        if uri.scheme().is_none() || uri.host().is_none() {
            return Err(Error::Config(
                "a quota request url must be absolute (scheme and host)".to_owned(),
            ));
        }
        let host = uri.host().unwrap_or("<no host>").to_owned();

        let mut builder = http::Request::builder().method(method).uri(uri);
        for (name, value) in &headers {
            // `Request::builder().header()` panics on a bad name or value, and this workspace
            // builds with `panic = "abort"`: a credential with a stray newline in it would take the
            // whole process down. Validate instead, and echo neither (a header value is a secret).
            let name = http::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                Error::Config("a quota request header name was not valid HTTP".to_owned())
            })?;
            let value = http::header::HeaderValue::from_str(value).map_err(|_| {
                Error::Config("a quota request header value was not valid HTTP".to_owned())
            })?;
            builder = builder.header(name, value);
        }
        let request = builder.body(Full::new(body)).map_err(|e| {
            Error::Io(io::Error::other(format!(
                "building a quota request for {host}: {e}"
            )))
        })?;

        let timeout = timeout.unwrap_or(self.default_timeout);
        let response = tokio::time::timeout(timeout, self.client.request(request))
            .await
            .map_err(|_| {
                Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out contacting {host}"),
                ))
            })?
            .map_err(|e| Error::Io(io::Error::other(format!("quota request to {host}: {e}"))))?;

        let status = response.status().as_u16();
        let response_headers = collect_headers(response.headers());

        let collected = tokio::time::timeout(timeout, response.into_body().collect())
            .await
            .map_err(|_| {
                Error::Io(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("timed out reading the quota reply from {host}"),
                ))
            })?
            .map_err(|e| Error::Io(io::Error::other(format!("quota reply from {host}: {e}"))))?
            .to_bytes();

        Ok(QuotaResponse {
            status,
            headers: response_headers,
            body: collected,
        })
    }
}

fn collect_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A fake that replays a scripted reply and remembers what it was asked — the shape Phase C's
    /// and the registry's tests inject. It holds no socket and never can.
    #[derive(Debug)]
    struct FakeHttp {
        reply: QuotaResponse,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn new(reply: QuotaResponse) -> Self {
            Self {
                reply,
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl QuotaHttp for FakeHttp {
        async fn request(&self, request: QuotaRequest) -> Result<QuotaResponse> {
            self.seen.lock().expect("not poisoned").push(request);
            Ok(self.reply.clone())
        }
    }

    fn reply(status: u16, body: &str) -> QuotaResponse {
        QuotaResponse {
            status,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Bytes::from(body.to_owned()),
        }
    }

    #[test]
    fn request_builders_shape_the_wire_request() {
        let get = QuotaRequest::get("https://example.test/v1/quota")
            .bearer("sk-secret")
            .header("x-org", "acme")
            .timeout(Duration::from_secs(3));
        assert_eq!(get.method, Method::GET);
        assert_eq!(get.url, "https://example.test/v1/quota");
        assert!(get.body.is_empty());
        assert_eq!(get.timeout, Some(Duration::from_secs(3)));
        assert_eq!(
            get.headers,
            vec![
                ("authorization".to_owned(), "Bearer sk-secret".to_owned()),
                ("x-org".to_owned(), "acme".to_owned()),
            ]
        );

        let post = QuotaRequest::post_json("https://example.test/v1/quota", "{\"a\":1}");
        assert_eq!(post.method, Method::POST);
        assert_eq!(
            post.headers,
            vec![("content-type".to_owned(), "application/json".to_owned())]
        );
        assert_eq!(post.body, Bytes::from_static(b"{\"a\":1}"));
        assert_eq!(post.timeout, None);
    }

    #[test]
    fn response_accessors_read_headers_and_json() {
        let response = reply(200, "{\"used\":30,\"total\":100}");
        assert!(response.is_success());
        assert_eq!(response.header("Content-Type"), Some("application/json"));
        assert_eq!(response.header("content-type"), Some("application/json"));
        assert_eq!(response.header("missing"), None);
        assert_eq!(
            response.json().expect("json")["used"],
            serde_json::json!(30)
        );

        let failure = reply(429, "slow down");
        assert!(!failure.is_success());
        assert_eq!(failure.body_str(), "slow down");
        assert!(failure.json().is_err());
    }

    #[test]
    fn retry_after_reads_both_forms() {
        let delta = QuotaResponse {
            status: 429,
            headers: vec![("retry-after".to_owned(), "120".to_owned())],
            body: Bytes::new(),
        };
        let now = crate::ids::now_millis();
        let instant = delta.retry_after_millis().expect("delta seconds parse");
        assert!(
            instant >= now + 120_000,
            "delta seconds are relative to now"
        );
        assert!(instant <= crate::ids::now_millis() + 120_000);

        let date = QuotaResponse {
            status: 429,
            headers: vec![(
                "retry-after".to_owned(),
                "Fri, 02 Jan 2026 03:04:05 GMT".to_owned(),
            )],
            body: Bytes::new(),
        };
        assert_eq!(date.retry_after_millis(), Some(1_767_323_045_000));

        let junk = QuotaResponse {
            status: 429,
            headers: vec![("retry-after".to_owned(), "soon".to_owned())],
            body: Bytes::new(),
        };
        assert_eq!(junk.retry_after_millis(), None);
        assert_eq!(reply(200, "").retry_after_millis(), None);
    }

    #[tokio::test]
    async fn the_trait_is_object_safe_and_injectable() {
        let fake = Arc::new(FakeHttp::new(reply(200, "{\"used\":1,\"total\":2}")));
        let http: Arc<dyn QuotaHttp> = fake.clone();
        let got = http
            .request(QuotaRequest::get("https://example.test/quota"))
            .await
            .expect("fake replies");
        assert_eq!(got.status, 200);
        assert_eq!(got.json().expect("json")["total"], serde_json::json!(2));
        assert_eq!(fake.seen.lock().expect("not poisoned").len(), 1);
    }

    #[tokio::test]
    async fn malformed_requests_fail_before_any_network_call() {
        let client = HyperQuotaHttp::new().expect("client builds without network");
        assert_eq!(client.default_timeout(), DEFAULT_TIMEOUT);

        // A url that is not a uri at all.
        let bad_url = client
            .request(QuotaRequest::get("not a url"))
            .await
            .expect_err("rejected");
        assert!(matches!(bad_url, Error::Config(_)), "got {bad_url:?}");

        // A url with no scheme or host: rejected rather than resolved against nothing.
        let relative = client
            .request(QuotaRequest::get("/v1/quota"))
            .await
            .expect_err("rejected");
        assert!(matches!(relative, Error::Config(_)), "got {relative:?}");

        // A header name that is not a token.
        let bad_name = client
            .request(QuotaRequest::get("https://example.test/quota").header("bad name", "x"))
            .await
            .expect_err("rejected");
        assert!(matches!(bad_name, Error::Config(_)), "got {bad_name:?}");

        // A header value carrying a newline — a credential copied with a trailing line break. This
        // is the case that would panic if it reached `Request::builder`.
        let bad_value = client
            .request(
                QuotaRequest::get("https://example.test/quota")
                    .header("authorization", "Bearer abc\ndef"),
            )
            .await
            .expect_err("rejected");
        assert!(matches!(bad_value, Error::Config(_)), "got {bad_value:?}");
    }
}
