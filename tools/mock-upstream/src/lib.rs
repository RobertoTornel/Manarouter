//! # mock-upstream — a deliberately dumb OpenAI-shaped provider
//!
//! Integration tests for the relay need an upstream that behaves like a real provider without a
//! network, credentials or spend: it must answer `/v1/chat/completions`, optionally stream, be
//! able to stall (so the hedged race and the TTFB timeout have something to race against), and be
//! able to fail with a rate-limit answer a client will believe (so retry and account-fallback
//! paths can be exercised end to end). It also serves a minimal CommandCode-style `/alpha/generate`
//! stream, so the relay's OpenAI-to-CommandCode adapter can be tested without the private service.
//!
//! This crate is **detached from the workspace** (its own `[workspace]` table in `Cargo.toml`) so
//! that a fixture can never break `cargo check --workspace`, and a fixture dependency bump can
//! never show up as churn in the repository's lockfile. Build it with
//! `cargo build --manifest-path tools/mock-upstream/Cargo.toml`.
//!
//! # What it reports, and why it reports a prefix of the credential
//!
//! The relay chooses an account by attaching that account's credential, and the credential is the
//! *only* wire-visible difference between two accounts of one provider — a provider's `base_url`
//! is shared by all its accounts. A test asserting "round-robin really rotated the accounts" has
//! nothing else to assert on, so the mock echoes the first 8 characters of the credential it saw
//! in `x-mock-auth-prefix`. That is enough to tell `alpha-key-1` from `beta-key-2`, and only that:
//! the full value never leaves the mock, so it cannot end up in a test log.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod faults;
pub use faults::{ErrorBody, Fault, FaultPatch};

const DEFAULT_CONTENT: &str = "Hello from mock-upstream.";

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
// `StreamExt` must be in scope for `Iter::chain`: without it the name resolves against the
// iterator prelude, and `stream::Iter` is a Stream, not an Iterator. `chain` itself is not
// feature-gated in futures-util, so `default-features = false, features = ["std"]` is enough.
use futures_util::StreamExt;
use serde_json::{json, Value};

/// How the binary should start, derived from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Interface to bind. Loopback by default: a test fixture must never be reachable from
    /// somewhere that could mistake it for a real provider.
    pub bind: IpAddr,
    /// Port to bind. `0` asks the OS for a free one; the binary prints the address it got.
    pub port: u16,
    /// Artificial delay before the first response byte.
    ///
    /// This is the TTFB knob: the relay's hedge window, its `ttfb_timeout_ms` and its "slow
    /// preferred model" tests are all driven with this rather than with real slowness.
    pub delay: Duration,
    /// Answer every chat request with `429` and a `retry-after` header instead of a completion.
    pub fail: bool,
    /// The `retry-after` value used by `--fail`, in seconds.
    pub retry_after_secs: u64,
    /// Send this many SSE data frames and then destroy the connection (0 = never).
    ///
    /// The mock's answer to the relay's mid-stream failure paths: `MidstreamPolicy`, the
    /// loser-cancel grace, and above all the splice guard that refuses to restart once a
    /// `tool_calls` delta has been emitted. Those can only be tested against an upstream that dies
    /// *after* saying something — an upstream that fails before the first byte exercises a
    /// different code path.
    pub die_after: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            // Constructed, not parsed: `Default` is on the startup path of every test and an
            // `expect` there would be a panic reachable from ordinary use.
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 18080,
            delay: Duration::ZERO,
            fail: false,
            retry_after_secs: 1,
            die_after: 0,
        }
    }
}

/// What argument parsing wants the process to do instead of serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Print to stdout and exit 0 (`--help`).
    Help(String),
    /// Print to stderr and exit 2 (a usage error).
    Usage(String),
}

const USAGE: &str = "usage: mock-upstream [--bind IP] [--port N] [--delay MS] [--fail] \
                     [--retry-after SECS] [--die-after N] [--help]";

/// Parse the command line.
///
/// Hand-rolled rather than clap: this binary has six flags, it exists to be spawned by tests, and
/// a fixture carrying its own argument parser keeps the detached crate's dependency set small.
/// Every rejection is an [`Exit::Usage`], never a panic — a typo in a test harness must fail as a
/// message, not as an abort.
pub fn parse_args<I, S>(args: I) -> Result<Options, Exit>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut opts = Options::default();
    let mut i = 0usize;

    // Read the value for `flag` at the current cursor, or report the missing value as a usage
    // error. Index-based rather than a closure over the iterator, because a closure that borrows
    // the cursor would also borrow it for the rest of the loop body.
    macro_rules! value {
        ($flag:literal) => {{
            let v = args
                .get(i)
                .cloned()
                .ok_or_else(|| Exit::Usage(format!("{} needs a value\n{USAGE}", $flag)))?;
            i += 1;
            v
        }};
    }

    while i < args.len() {
        let arg = args[i].clone();
        i += 1;
        match arg.as_str() {
            "-h" | "--help" => return Err(Exit::Help(format!("mock-upstream\n{USAGE}"))),
            "--bind" => {
                let v = value!("--bind");
                opts.bind = v.parse::<IpAddr>().map_err(|_| {
                    Exit::Usage(format!("--bind: {v:?} is not an IP address\n{USAGE}"))
                })?;
            }
            "--port" => {
                let v = value!("--port");
                opts.port = v.parse::<u16>().map_err(|_| {
                    Exit::Usage(format!("--port: {v:?} is not a port number\n{USAGE}"))
                })?;
            }
            "--delay" => {
                let v = value!("--delay");
                let ms = v.parse::<u64>().map_err(|_| {
                    Exit::Usage(format!(
                        "--delay: {v:?} is not a millisecond count\n{USAGE}"
                    ))
                })?;
                opts.delay = Duration::from_millis(ms);
            }
            "--fail" => opts.fail = true,
            "--retry-after" => {
                let v = value!("--retry-after");
                opts.retry_after_secs = v.parse::<u64>().map_err(|_| {
                    Exit::Usage(format!(
                        "--retry-after: {v:?} is not a second count\n{USAGE}"
                    ))
                })?;
            }
            "--die-after" => {
                let v = value!("--die-after");
                opts.die_after = v.parse::<usize>().map_err(|_| {
                    Exit::Usage(format!("--die-after: {v:?} is not a frame count\n{USAGE}"))
                })?;
            }
            other => return Err(Exit::Usage(format!("unknown flag {other:?}\n{USAGE}"))),
        }
    }
    Ok(opts)
}

/// The shared, per-process mock state.
pub struct Mock {
    /// How to behave, as parsed from the command line.
    pub opts: Options,
    /// Mutable process-wide fault; request-local patches never modify it.
    fault: RwLock<Fault>,
    /// Monotonic id suffix, so two responses in one test are distinguishable.
    seq: AtomicU64,
}

impl Mock {
    fn new(opts: Options) -> Self {
        let fault = Fault {
            delay_ms: opts.delay.as_millis().min(u64::MAX as u128) as u64,
            status: if opts.fail {
                StatusCode::TOO_MANY_REQUESTS.as_u16()
            } else {
                0
            },
            retry_after_secs: if opts.fail { opts.retry_after_secs } else { 0 },
            fail_after_chunks: opts.die_after,
            ..Fault::default()
        };
        Self {
            opts,
            fault: RwLock::new(fault),
            seq: AtomicU64::new(1),
        }
    }

    fn current_fault(&self, headers: &HeaderMap) -> Fault {
        let base = self.fault.read().unwrap_or_else(|e| e.into_inner()).clone();
        headers
            .get("x-mock-fault")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| serde_json::from_str::<FaultPatch>(v).ok())
            .map(|patch| base.merge(&patch))
            .unwrap_or(base)
    }

    fn update_fault(&self, patch: FaultPatch) -> Fault {
        let mut fault = self.fault.write().unwrap_or_else(|e| e.into_inner());
        *fault = fault.merge(&patch);
        fault.clone()
    }
}

/// Build the route table.
///
/// Provider routes plus a deliberately separate fault-control endpoint.
pub fn app(opts: Options) -> Router {
    let mock = Arc::new(Mock::new(opts));
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/alpha/generate", post(commandcode_generate))
        .route("/v1/models", get(models))
        .route("/healthz", get(healthz))
        .route("/__control", post(control))
        .with_state(mock)
}

/// Apply a process-wide partial fault patch and return the resulting complete configuration.
async fn control(State(mock): State<Arc<Mock>>, body: Bytes) -> Response {
    let patch: FaultPatch = match serde_json::from_slice(&body) {
        Ok(patch) => patch,
        Err(err) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                json!({"error": format!("invalid fault patch: {err}")}),
            )
        }
    };
    json_response(
        StatusCode::OK,
        serde_json::to_value(mock.update_fault(patch))
            .unwrap_or_else(|_| json!({"error":"serialization failed"})),
    )
}

async fn healthz() -> &'static str {
    "ok"
}

/// Minimal CommandCode-compatible SSE stream for exercising the relay's dialect adapter.
async fn commandcode_generate(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let fault = mock.current_fault(&headers);
    let _sequence = mock.seq.fetch_add(1, Ordering::Relaxed);
    let _model = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("params")?
                .get("model")?
                .as_str()
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "mock-model".to_owned());
    if fault.forces_error() {
        let status = StatusCode::from_u16(fault.effective_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let envelope = faults::error_envelope(
            fault.error_body.resolve(),
            status.as_u16(),
            fault.content_or_default(),
            fault.retry_after_secs,
        );
        return json_response(
            status,
            serde_json::from_str(&envelope)
                .unwrap_or_else(|_| json!({"success":false,"error":{"message":"invalid configured error envelope"}})),
        );
    }
    if fault.ttfb_ms > 0 {
        tokio::time::sleep(Duration::from_millis(fault.ttfb_ms)).await;
    }
    let content = fault.content_or_default();
    let frames = [
        json!({"type":"text-delta","text":content}),
        json!({"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":16,"outputTokens":8}}),
    ];
    let stream = futures_util::stream::iter(frames.into_iter().map(|event| {
        Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("data: {}\n\n", event)))
    }));
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert("content-type", "text/event-stream".parse().unwrap());
    response
}

/// The model list, in OpenAI's shape.
async fn models(State(_): State<std::sync::Arc<Mock>>) -> Response {
    json_response(
        StatusCode::OK,
        json!({
            "object": "list",
            "data": [
                { "id": "mock-model", "object": "model", "created": 0, "owned_by": "mock-upstream" }
            ],
        }),
    )
}

/// The whole point of the crate: an OpenAI-shaped chat completion, optionally streamed, with the
/// latency and failure knobs applied.
async fn chat_completions(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let fault = mock.current_fault(&headers);
    if fault.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(fault.delay_ms)).await;
    }
    if fault.forces_error() {
        let status = StatusCode::from_u16(fault.effective_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let envelope = faults::error_envelope(
            fault.error_body.resolve(),
            status.as_u16(),
            fault.content_or_default(),
            fault.retry_after_secs,
        );
        let mut resp = json_response(
            status,
            serde_json::from_str(&envelope).unwrap_or_else(
                |_| json!({"error":{"message":"invalid configured error envelope"}}),
            ),
        );
        if fault.retry_after_secs > 0 {
            if let Ok(v) = fault.retry_after_secs.to_string().parse() {
                resp.headers_mut().insert("retry-after", v);
            }
        }
        attach_echo_headers(resp, &headers, body.len())
    } else {
        respond(&mock, &headers, body, fault)
    }
}

/// Parse the request and build the answer.
///
/// A body that is not JSON is answered with a 400 in the upstream's own error shape: that is what
/// a real provider does, and a relay test that accidentally forwards garbage should see a
/// plausible upstream rejection rather than a mock-specific one.
fn respond(mock: &Mock, headers: &HeaderMap, body: Bytes, fault: Fault) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return attach_echo_headers(
                json_response(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": {
                            "message": format!("mock-upstream: body is not valid JSON: {e}"),
                            "type": "invalid_request_error",
                            "code": "invalid_json",
                            "param": null,
                        }
                    }),
                ),
                headers,
                body.len(),
            );
        }
    };

    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("mock-model");
    let wants_stream = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let id = format!("chatcmpl-mock-{}", mock.seq.fetch_add(1, Ordering::Relaxed));
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let resp = if wants_stream {
        sse_response(mock, &id, model, created, &fault)
    } else {
        json_response(
            StatusCode::OK,
            json!({
                "id": id,
                "object": "chat.completion",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "message": if fault.include_tool_call { json!({"role":"assistant","content":null,"tool_calls":[{"id":"call_mock","type":"function","function":{"name":"mock_tool","arguments":"{}"}}]}) } else { json!({"role":"assistant","content":fault.content_or_default()}) },
                    "finish_reason": if fault.include_tool_call { "tool_calls" } else { "stop" },
                    "logprobs": null,
                }],
                "usage": { "prompt_tokens": 16, "completion_tokens": 8, "total_tokens": 24 },
                // Extra top-level key, deliberately: OpenAI clients ignore unknown fields, and a
                // relay passthrough test needs proof that the *original* request bytes arrived
                // unchanged — body_len would differ if the relay had re-serialised the JSON.
                "echo": {
                    "model": model,
                    "body_len": body.len(),
                    "auth_prefix": auth_prefix(headers),
                },
            }),
        )
    };
    attach_echo_headers(resp, headers, body.len())
}

/// An SSE completion whose frames can be cut short by `--die-after`.
fn sse_response(_mock: &Mock, id: &str, model: &str, created: u64, fault: &Fault) -> Response {
    let chunk = |delta: Value, finish: Value| -> String {
        format!(
            "data: {}\n\n",
            json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        )
    };

    if fault.ttfb_ms > 0 {
        std::thread::sleep(Duration::from_millis(fault.ttfb_ms));
    }
    let content = fault.content_or_default();
    let mut frames: Vec<String> = vec![
        chunk(json!({ "role": "assistant", "content": "" }), Value::Null),
        chunk(json!({ "content": content }), Value::Null),
        chunk(json!({}), json!("stop")),
        "data: [DONE]\n\n".to_string(),
    ];

    let die = fault.fail_after_chunks;
    let dying = die > 0 && die <= frames.len();
    if dying {
        frames.truncate(die);
    }

    let ok_items: Vec<Result<String, std::io::Error>> = frames.into_iter().map(Ok).collect();
    let stream = futures_util::stream::iter(ok_items);
    let body = if dying {
        // The error element makes hyper tear the connection down instead of ending the body
        // cleanly: a truncated-but-clean SSE stream reads as a *complete* response to many
        // clients, which would silently downgrade a mid-stream failure test into a test that
        // never reached the failure. The short sleep gives the frames already written a moment to
        // be ACKed, so a reset does not discard them before the client has read them.
        let error = futures_util::stream::once(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "mock-upstream: --die-after reached",
            ))
        });
        Body::from_stream(stream.chain(error))
    } else {
        Body::from_stream(stream)
    };

    match Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .body(body)
    {
        Ok(resp) => resp,
        // Cannot happen with static header names; answered with an empty 500 rather than
        // unwrapping, because this binary also builds with `panic = "abort"` and a fixture that
        // aborts a test run is worse than one that reports an error.
        Err(_) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": { "message": "mock-upstream: failed to build the SSE response" } }),
        ),
    }
}

/// The credential the relay attached, as a short prefix. See the module docs for why 8 characters.
///
/// A `Bearer ` scheme is stripped first (ASCII case-insensitively), so the prefix of a JWT-style
/// `Authorization` and of a raw `x-api-key` are the *token's* first characters — comparable
/// across the two channels a relay might use.
fn auth_prefix(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get("authorization")
        .or_else(|| headers.get("x-api-key"))?;
    let text = value.to_str().ok()?;
    // `get(..7)` rather than slicing: it refuses a non-char-boundary, so no panic on odd bytes.
    let token = match text.get(..7) {
        Some(head) if head.eq_ignore_ascii_case("bearer ") => &text[7..],
        _ => text,
    };
    Some(token.chars().take(8).collect())
}

/// Echo the observed request metadata as response headers.
///
/// Headers as well as body because SSE has no body structure to hang an echo on.
fn attach_echo_headers(mut resp: Response, headers: &HeaderMap, body_len: usize) -> Response {
    if let Ok(v) = body_len.to_string().parse() {
        resp.headers_mut().insert("x-mock-echo-body-len", v);
    }
    if let Some(prefix) = auth_prefix(headers) {
        if let Ok(v) = prefix.parse() {
            resp.headers_mut().insert("x-mock-auth-prefix", v);
        }
    }
    resp
}

fn json_response(status: StatusCode, body: Value) -> Response {
    // A serialisation failure is impossible for these values; `b"{}"` keeps the response valid
    // JSON rather than unwrapping on a path that ships with `panic = "abort"`.
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    if let Ok(v) = "application/json".parse() {
        resp.headers_mut().insert("content-type", v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // ---------------------------------------------------------------- parse

    #[test]
    fn defaults_are_loopback_18080_and_healthy() {
        let opts = parse_args(Vec::<String>::new()).expect("no arguments is valid");
        assert_eq!(opts.port, 18080);
        assert_eq!(opts.bind, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(!opts.fail);
        assert_eq!(opts.delay, Duration::ZERO);
        assert_eq!(opts.die_after, 0);
    }

    #[test]
    fn every_flag_is_recognised() {
        let opts = parse_args([
            "--bind",
            "10.0.0.5",
            "--port",
            "20999",
            "--delay",
            "250",
            "--fail",
            "--retry-after",
            "7",
            "--die-after",
            "3",
        ])
        .expect("valid flags");
        assert_eq!(opts.bind, "10.0.0.5".parse::<IpAddr>().expect("literal"));
        assert_eq!(opts.port, 20999);
        assert_eq!(opts.delay, Duration::from_millis(250));
        assert!(opts.fail);
        assert_eq!(opts.retry_after_secs, 7);
        assert_eq!(opts.die_after, 3);
    }

    #[test]
    fn help_is_a_clean_exit_not_a_serve() {
        match parse_args(["--help"]) {
            Err(Exit::Help(text)) => assert!(text.contains("usage:")),
            other => panic!("--help must request exit 0, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_malformed_flags_are_usage_errors() {
        for bad in [
            vec!["--nope"],
            vec!["--port", "six"],
            vec!["--port", "70000"],
            vec!["--delay", "-1"],
            vec!["--bind", "example.com"],
            vec!["--port"],
        ] {
            match parse_args(bad.clone()) {
                Err(Exit::Usage(msg)) => assert!(msg.contains("usage:"), "msg was {msg}"),
                other => panic!("{bad:?} must be a usage error, got {other:?}"),
            }
        }
    }

    // -------------------------------------------------------------- network

    async fn spawn(opts: Options) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("loopback bind");
        let addr = listener.local_addr().expect("bound address");
        let app = app(opts);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        addr
    }

    /// A raw HTTP/1.1 client. Hand-rolled so the mock's tests need no HTTP crate of their own.
    ///
    /// Deliberately does *not* half-close the socket after writing: a `FIN` before the response
    /// arrives makes hyper discard the exchange instead of answering it (observed empirically —
    /// every request answered emptily with the shutdown in place). Termination comes instead from
    /// `connection: close` on every request: the server closes when it is done, so `read` to EOF
    /// ends the test for every mode, including SSE, and a mid-body reset ends it via `Err`.
    async fn send(addr: SocketAddr, request: &str) -> Vec<u8> {
        let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
        stream.write_all(request.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                // A reset (`fail_after_chunks`) lands here with whatever was delivered already
                // in buf.
                Err(_) => break,
            }
        }
        buf
    }

    fn chat_request(body: &str, extra_headers: &str) -> String {
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nhost: mock\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n{}\r\n{}",
            body.len(),
            extra_headers,
            body
        )
    }

    fn split_response(raw: &[u8]) -> (String, String) {
        let text = String::from_utf8_lossy(raw).into_owned();
        match text.find("\r\n\r\n") {
            // Skip the whole terminator: `find` returns the index of its first `\r`, and keeping
            // those four bytes would put them at the front of the body — invisible to `contains`
            // assertions, fatal to a chunk decoder reading the size line.
            Some(at) => (text[..at].to_string(), text[at + 4..].to_string()),
            None => (text.clone(), String::new()),
        }
    }

    /// Undo HTTP/1.1 chunked transfer encoding, as a real client must.
    ///
    /// A streaming body has no length when it starts, so hyper frames it in chunks on the wire;
    /// asserting on the raw bytes would be asserting on the transport, not the payload. A
    /// truncated stream (the mid-answer death fault) simply stops mid-chunk, so the decoder stops
    /// with it instead of requiring a terminating zero chunk.
    fn dechunk(head: &str, body: &str) -> String {
        if !head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            return body.to_string();
        }
        let data = body.as_bytes();
        let mut out: Vec<u8> = Vec::new();
        let mut pos = 0usize;
        while let Some(crlf) = data[pos..]
            .windows(2)
            .position(|w| w == b"\r\n")
            .map(|i| i + pos)
        {
            let size = std::str::from_utf8(&data[pos..crlf])
                .ok()
                .and_then(|s| usize::from_str_radix(s.trim(), 16).ok())
                .unwrap_or(0);
            if size == 0 {
                break;
            }
            let start = crlf + 2;
            let end = match start.checked_add(size) {
                Some(e) if e <= data.len() => e,
                _ => break,
            };
            out.extend_from_slice(&data[start..end]);
            pos = if end + 2 <= data.len() {
                end + 2
            } else {
                data.len()
            };
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn control_endpoint_changes_process_default_and_header_overrides_one_request() {
        let addr = spawn(Options::default()).await;
        let patch = r#"{"content":"controlled answer"}"#;
        let response = send(addr, &format!(
            "POST /__control HTTP/1.1\r\nhost: mock\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            patch.len(), patch
        )).await;
        let (head, body) = split_response(&response);
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        assert!(body.contains("controlled answer"), "{body}");

        let response = send(addr, &chat_request(r#"{"model":"m"}"#, "")).await;
        let (_, body) = split_response(&response);
        assert!(body.contains("controlled answer"), "{body}");

        let override_patch = r#"{"content":"one request only"}"#;
        let response = send(
            addr,
            &chat_request(
                r#"{"model":"m"}"#,
                &format!("x-mock-fault: {}\r\n", override_patch),
            ),
        )
        .await;
        let (_, body) = split_response(&response);
        assert!(body.contains("one request only"), "{body}");

        let response = send(addr, &chat_request(r#"{"model":"m"}"#, "")).await;
        let (_, body) = split_response(&response);
        assert!(body.contains("controlled answer"), "{body}");
    }

    #[tokio::test]
    async fn healthz_answers() {
        let addr = spawn(Options::default()).await;
        let raw = send(
            addr,
            "GET /healthz HTTP/1.1\r\nhost: mock\r\nconnection: close\r\n\r\n",
        )
        .await;
        let (head, body) = split_response(&raw);
        assert!(head.starts_with("HTTP/1.1 200"), "head was {head}");
        assert_eq!(body.trim(), "ok");
    }

    #[tokio::test]
    async fn chat_json_echoes_model_length_and_credential_prefix() {
        let addr = spawn(Options::default()).await;
        let body = r#"{"model":"cx/gpt-6-astra","messages":[{"role":"user","content":"hi"}]}"#;
        let raw = send(
            addr,
            &chat_request(body, "authorization: Bearer alpha-key-123456\r\n"),
        )
        .await;
        let (head, payload) = split_response(&raw);
        assert!(head.starts_with("HTTP/1.1 200"), "head was {head}");
        assert!(
            head.to_ascii_lowercase()
                .contains("x-mock-auth-prefix: alpha-ke"),
            "head was {head}"
        );

        let v: Value = serde_json::from_str(payload.trim()).expect("JSON body");
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["model"], "cx/gpt-6-astra");
        assert_eq!(v["echo"]["body_len"], body.len());
        assert_eq!(v["echo"]["auth_prefix"], "alpha-ke");
        assert!(v["id"].as_str().expect("id").starts_with("chatcmpl-mock-"));
    }

    #[tokio::test]
    async fn x_api_key_is_the_secondary_credential_channel() {
        let addr = spawn(Options::default()).await;
        let raw = send(
            addr,
            &chat_request(r#"{"model":"m"}"#, "x-api-key: beta-key-999\r\n"),
        )
        .await;
        let (_, payload) = split_response(&raw);
        let v: Value = serde_json::from_str(payload.trim()).expect("JSON body");
        assert_eq!(v["echo"]["auth_prefix"], "beta-key");
    }

    #[tokio::test]
    async fn stream_true_gets_sse_frames_ending_in_done() {
        let addr = spawn(Options::default()).await;
        let raw = send(addr, &chat_request(r#"{"model":"m","stream":true}"#, "")).await;
        let (head, body) = split_response(&raw);
        let body = dechunk(&head, &body);
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: text/event-stream"),
            "head was {head}"
        );
        assert!(body.contains("chat.completion.chunk"));
        assert!(body.contains("Hello from "));
        assert!(body.contains("\"finish_reason\":\"stop\""));
        assert!(body.trim_end().ends_with("data: [DONE]"));
    }

    #[tokio::test]
    async fn fail_returns_429_with_retry_after() {
        let addr = spawn(Options {
            fail: true,
            retry_after_secs: 3,
            ..Options::default()
        })
        .await;
        let raw = send(addr, &chat_request(r#"{"model":"m"}"#, "")).await;
        let (head, body) = split_response(&raw);
        assert!(head.starts_with("HTTP/1.1 429"), "head was {head}");
        assert!(
            head.to_ascii_lowercase().contains("retry-after: 3"),
            "head was {head}"
        );
        assert!(body.contains("rate_limit_exceeded"));
    }

    #[tokio::test]
    async fn delay_is_paid_before_the_first_byte() {
        let addr = spawn(Options {
            delay: Duration::from_millis(150),
            ..Options::default()
        })
        .await;
        let start = std::time::Instant::now();
        let raw = send(addr, &chat_request(r#"{"model":"m"}"#, "")).await;
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(150), "elapsed {elapsed:?}");
        assert!(String::from_utf8_lossy(&raw).contains("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn die_after_truncates_the_stream_without_done() {
        let addr = spawn(Options {
            die_after: 2,
            ..Options::default()
        })
        .await;
        let raw = send(addr, &chat_request(r#"{"model":"m","stream":true}"#, "")).await;
        let (head, raw_body) = split_response(&raw);
        let body = dechunk(&head, &raw_body);
        // Two data frames made it; the terminator and [DONE] did not — the connection died, which
        // is what the relay's mid-stream failure paths need to observe.
        assert!(!body.contains("[DONE]"), "body was {body}");
        assert!(
            !body.contains("\"finish_reason\":\"stop\""),
            "body was {body}"
        );
        let frames = body.matches("data: ").count();
        assert_eq!(frames, 2, "body was {body}");
    }

    #[tokio::test]
    async fn malformed_json_is_an_upstream_400() {
        let addr = spawn(Options::default()).await;
        let raw = send(addr, &chat_request("not json at all", "")).await;
        let (head, body) = split_response(&raw);
        assert!(head.starts_with("HTTP/1.1 400"), "head was {head}");
        assert!(body.contains("invalid_json"));
    }

    #[tokio::test]
    async fn models_endpoint_answers_openai_shape() {
        let addr = spawn(Options::default()).await;
        let raw = send(
            addr,
            "GET /v1/models HTTP/1.1\r\nhost: mock\r\nconnection: close\r\n\r\n",
        )
        .await;
        let (head, body) = split_response(&raw);
        assert!(head.starts_with("HTTP/1.1 200"), "head was {head}");
        let v: Value = serde_json::from_str(body.trim()).expect("JSON body");
        assert_eq!(v["object"], "list");
        assert_eq!(v["data"][0]["id"], "mock-model");
    }
}
