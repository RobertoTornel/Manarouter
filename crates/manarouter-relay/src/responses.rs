//! The Responses surface: what the relay keeps when a client asks it to remember, and how it is read
//! back.
//!
//! # What is stored, and what is not
//!
//! OpenAI's Responses API lets a client set `"store": true` on a request and later fetch the
//! response object by id. `POST /v1/responses` is proxied by [`crate::server`]; this module is the
//! local half of that contract. [`record`] writes the upstream object to disk **only** when the
//! request asked for it. A response whose request did not ask is not retrievable, and `GET` answers
//! 404 saying so. That is deliberate: a relay that silently retained every prompt would be a
//! transcript archive nobody asked for, and a 404 that explains itself is more honest than a store
//! the client never opted into.
//!
//! One file holds the upstream object, the client request and the time:
//!
//! ```json
//! {"response": <upstream object>, "request": <client object or null>, "stored_at": 1760000000}
//! ```
//!
//! The response is embedded by copying its bytes, not by re-serialising a parsed value, so a client
//! reading it back sees the provider's own numbers and key order rather than this module's opinion
//! of them.
//!
//! # Storage and the id boundary
//!
//! One file per response at `<state_dir>/responses/<id>.json`, written atomically. The id comes from
//! the upstream object and it is the only part of a record that becomes a path, so it is validated
//! before it is ever joined to one: ASCII alphanumerics plus `-`, `_` and `.`, 1 to 128 bytes. That
//! check is a **security boundary**, not a formatting preference — without it an upstream id of
//! `../../config` would let a response body choose which file this process writes and deletes.
//!
//! # What this module refuses to invent
//!
//! `previous_response_id` is not resolved: a stored response is a record, not a conversation
//! replayer, and chaining two stored objects would pretend the relay kept a thread it did not.
//! `cancel` cannot cancel anything either — by the time a response is recorded the relay no longer
//! holds the upstream request — so a finished response is refused with a 400 and an unfinished one
//! is returned untouched rather than reported as cancelled.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body as AxumBody;
use axum::extract::{Path as RoutePath, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use bytes::Bytes;
use futures_util::Stream;
use mr_core::dialect::Dialect;
use serde_json::Value;

use crate::server::AppState;

/// The longest id this module will turn into a file name.
const MAX_ID_BYTES: usize = 128;

/// The largest upstream response this module will record, in bytes.
///
/// An 8 MiB response is already far larger than a stored chat answer; the cap exists so a runaway
/// provider cannot fill the operator's disk one proxied request at a time.
const MAX_STORED_BYTES: usize = 8 * 1024 * 1024;

/// The statuses after which there is nothing left to cancel.
const TERMINAL: [&str; 4] = ["completed", "failed", "cancelled", "incomplete"];

/// Mount this module's routes.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/responses/{id}", get(retrieve).delete(delete))
        .route("/v1/responses/{id}/cancel", post(cancel))
        .route("/v1/responses/{id}/input_items", get(input_items))
}

/// Whether a `/v1/responses` request asked for its answer to be kept.
///
/// The dispatch path has to answer this *before* it builds the client response, because an answer is
/// only recordable while it passes through the relay: once the body has been handed to the client it
/// is the client's. Parsing the same body once here and once in [`record`] costs one small JSON parse
/// on a route that has just spent a generation, which is cheaper than keeping a second shape of the
/// same check.
pub fn asked_to_store(request_body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(request_body)
        .ok()
        .and_then(|request| request.get("store").and_then(Value::as_bool))
        == Some(true)
}

/// Make a client response record itself as it is delivered.
///
/// Returns the response unchanged when there is nothing a record could hold: a failed status, or a
/// body that is not a single JSON object. A `text/event-stream` answer is the second case and it is
/// skipped deliberately — an event stream is a sequence of deltas, not the response object a stored
/// response is defined to be, and assembling one here would invent an object the provider never sent.
///
/// The bytes are copied, not re-serialised, so a client reading the record back sees the provider's
/// own numbers and key order. Recording never delays the client: the copy is written when the stream
/// ends, in a task of its own, and a stream the client abandoned mid-way is not recorded at all —
/// the relay has half an answer, and half an answer stored under a completed id would be a lie.
pub fn record_while_streaming(
    app: AppState,
    request_body: Vec<u8>,
    response: Response,
) -> Response {
    if !response.status().is_success() {
        return response;
    }
    let is_event_stream = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .trim_start()
                .to_ascii_lowercase()
                .starts_with("text/event-stream")
        })
        .unwrap_or(false);
    if is_event_stream {
        return response;
    }
    let status = response.status().as_u16();
    let (parts, body) = response.into_parts();
    let body = AxumBody::from_stream(RecordingBody {
        inner: body.into_data_stream(),
        app,
        request: request_body,
        status,
        collected: Vec::new(),
        overflowed: false,
        finished: false,
    });
    Response::from_parts(parts, body)
}

/// A body that copies what it forwards, and records it once it has all arrived.
///
/// The copy is bounded by [`MAX_STORED_BYTES`]. On overflow the copy is dropped and the body keeps
/// streaming to the client untouched: a response too large to store is still a response, and the
/// client is never the one who pays for the relay's storage limit.
struct RecordingBody<S> {
    inner: S,
    app: AppState,
    request: Vec<u8>,
    status: u16,
    collected: Vec<u8>,
    overflowed: bool,
    finished: bool,
}

impl<S> Drop for RecordingBody<S> {
    fn drop(&mut self) {
        if !self.finished && !self.collected.is_empty() {
            tracing::debug!(
                bytes = self.collected.len(),
                "a stored response was abandoned before it ended; it is not recorded"
            );
        }
    }
}

impl<S, E> Stream for RecordingBody<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
{
    type Item = Result<Bytes, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if !this.overflowed {
                    if this.collected.len() + chunk.len() > MAX_STORED_BYTES {
                        this.overflowed = true;
                        this.collected = Vec::new();
                    } else {
                        this.collected.extend_from_slice(&chunk);
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finished = true;
                this.collected = Vec::new();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finished = true;
                if this.overflowed {
                    tracing::warn!(
                        limit = MAX_STORED_BYTES,
                        "a response was too large to record; it is not stored"
                    );
                } else if !this.collected.is_empty() {
                    let body = std::mem::take(&mut this.collected);
                    let request = std::mem::take(&mut this.request);
                    let app = this.app.clone();
                    let status = this.status;
                    // The client's last byte is on its way out; the write happens after it.
                    tokio::spawn(async move {
                        record(&app, &request, status, &body).await;
                    });
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Record a response the relay just proxied, when the request asked for it to be stored.
///
/// A no-op unless the request body is a JSON object with `"store": true`, the upstream answered with
/// a 2xx whose body is a JSON object carrying a safe `id`, and the body fits [`MAX_STORED_BYTES`].
/// Each of those is a reason *not* to write, never a reason to fail the request: the client already
/// has its answer, so a record the relay declined to keep is reported as a warning and nothing else.
pub async fn record(
    _app: &AppState,
    request_body: &[u8],
    response_status: u16,
    response_body: &[u8],
) {
    // The status is not part of the record — the object carries its own `status` field, and copying
    // the HTTP status into it would invent a second source of truth. An error answer is not a
    // response object at all, so it is never recorded.
    if !(200..300).contains(&response_status) {
        return;
    }

    let Ok(request) = serde_json::from_slice::<Value>(request_body) else {
        return;
    };
    if request.get("store").and_then(Value::as_bool) != Some(true) {
        return;
    }

    let Ok(response) = serde_json::from_slice::<Value>(response_body) else {
        return;
    };
    if !response.is_object() {
        return;
    }
    let Some(id) = response.get("id").and_then(Value::as_str) else {
        return;
    };
    if !valid_id(id) {
        // The id is client-visible data from the upstream, and it would become a file name. Refuse
        // it loudly: a record that cannot be written safely is not written at all.
        tracing::warn!(
            id = %id,
            "refusing to record a response whose id is not a safe file name"
        );
        return;
    }
    if response_body.len() > MAX_STORED_BYTES {
        tracing::warn!(
            id = %id,
            bytes = response_body.len(),
            limit = MAX_STORED_BYTES,
            "the response is too large to record; it is not stored"
        );
        return;
    }

    let envelope = envelope_bytes(response_body, request_body);
    if let Err(error) = write_atomic_async(record_path(id), envelope).await {
        // The proxied answer has already been sent. A failed write is a lost record, not a failed
        // request, and the warning is the whole remedy.
        tracing::warn!(id = %id, error = %error, "a response could not be recorded");
    }
}

/// `GET /v1/responses/{id}`.
///
/// Returns the stored upstream object byte-for-byte. Anything that was not stored — including an id
/// this module would never have written — is a 404 that says why, never an empty object.
async fn retrieve(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return not_stored(&id);
    }
    let bytes = match read_record(&id).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return not_stored(&id),
        Err(error) => return internal_error(&error),
    };
    let record = match parse_record(&bytes) {
        Ok(record) => record,
        Err(error) => return internal_error(&error),
    };
    raw_json_response(StatusCode::OK, record.response.get().as_bytes())
}

/// `DELETE /v1/responses/{id}`.
///
/// Removing the file is the deletion; a second delete has nothing to remove and answers 404, which
/// is what a client that retries a delete should see.
async fn delete(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return not_stored(&id);
    }
    match remove_record(&id).await {
        Ok(true) => crate::server::json_response(
            StatusCode::OK,
            serde_json::json!({
                "id": id,
                "object": "response.deleted",
                "deleted": true,
            }),
        ),
        Ok(false) => not_stored(&id),
        Err(error) => internal_error(&error),
    }
}

/// `POST /v1/responses/{id}/cancel`.
///
/// A recorded response is already finished: the relay records it after the upstream answered, so
/// there is no in-flight request left to cancel. A terminal status is therefore a 400 that explains
/// itself. A record that is somehow not terminal is returned untouched — the relay has no handle to
/// cancel anything with, and claiming a cancellation it did not perform would be worse than saying
/// nothing.
async fn cancel(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return not_stored(&id);
    }
    let bytes = match read_record(&id).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return not_stored(&id),
        Err(error) => return internal_error(&error),
    };
    let record = match parse_record(&bytes) {
        Ok(record) => record,
        Err(error) => return internal_error(&error),
    };
    let status = serde_json::from_str::<Value>(record.response.get())
        .ok()
        .and_then(|value| {
            value
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    if let Some(status) = status.as_deref().filter(|status| TERMINAL.contains(status)) {
        return refusal(
            StatusCode::BAD_REQUEST,
            &format!(
                "response \"{id}\" is already {status} and cannot be cancelled: the relay records a \
                 response only after it has finished, so there is no in-flight upstream request to \
                 cancel"
            ),
        );
    }
    raw_json_response(StatusCode::OK, record.response.get().as_bytes())
}

/// `GET /v1/responses/{id}/input_items`.
///
/// The items are the request's `input`, given ids this module mints. A string input is the single
/// user message the API's own rule says it is, not an empty list; a request that could not be read
/// back (an old or hand-repaired record) has no items, and says so with an empty list.
async fn input_items(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = crate::server::authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return not_stored(&id);
    }
    let bytes = match read_record(&id).await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return not_stored(&id),
        Err(error) => return internal_error(&error),
    };
    let record = match parse_record(&bytes) {
        Ok(record) => record,
        Err(error) => return internal_error(&error),
    };
    let items = items_from_request(record.request.as_ref());
    crate::server::json_response(StatusCode::OK, list_envelope(items))
}

// --- The record on disk ---------------------------------------------------------------------

/// A record as it sits on disk, borrowed from the file's bytes.
///
/// `response` is a [`serde_json::value::RawValue`] so the upstream object is returned exactly as it
/// was stored; `stored_at` is deliberately not read, since it is provenance for an operator reading
/// the file, not part of the API's answer.
#[derive(serde::Deserialize)]
struct StoredRecord<'a> {
    #[serde(borrow)]
    response: &'a serde_json::value::RawValue,
    #[serde(default)]
    request: Option<Value>,
}

fn parse_record(bytes: &[u8]) -> Result<StoredRecord<'_>, String> {
    serde_json::from_slice(bytes)
        .map_err(|error| format!("the stored record is not readable: {error}"))
}

/// The directory every record lives in.
fn storage_root() -> PathBuf {
    mr_core::config::state_dir().join("responses")
}

/// The one path a response id may name.
///
/// The caller must have passed `id` through [`valid_id`]; this function does not re-check, so it is
/// private and only called after the check.
fn record_path(id: &str) -> PathBuf {
    storage_root().join(format!("{id}.json"))
}

/// Whether an id may become a file name.
///
/// **This is a security boundary.** An id arrives inside an upstream response body, and this module
/// joins it to the state directory. Only ASCII alphanumerics and `-`, `_`, `.` are accepted, and the
/// length is bounded, so no id can contain a separator, a parent reference, a drive letter or an
/// escape. `.` and `..` are harmless here because the file name always ends in `.json`.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Build the envelope without re-serialising the upstream object.
///
/// Copying the response's bytes is the point: the stored object is the provider's JSON, not a value
/// this module parsed and printed again. A request body that is not JSON is stored as `null` rather
/// than as a broken fragment, so the envelope itself always parses.
fn envelope_bytes(response_body: &[u8], request_body: &[u8]) -> Vec<u8> {
    let request = match serde_json::from_slice::<Value>(request_body) {
        Ok(_) => request_body,
        Err(_) => b"null",
    };
    let mut out = Vec::with_capacity(response_body.len() + request.len() + 64);
    out.extend_from_slice(br#"{"response":"#);
    out.extend_from_slice(response_body);
    out.extend_from_slice(br#","request":"#);
    out.extend_from_slice(request);
    out.extend_from_slice(br#","stored_at":"#);
    out.extend_from_slice(now_seconds().to_string().as_bytes());
    out.push(b'}');
    out
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

// --- The list answer ------------------------------------------------------------------------

/// The items `input_items` reports for a stored request.
fn items_from_request(request: Option<&Value>) -> Vec<Value> {
    let Some(input) = request.and_then(|request| request.get("input")) else {
        return Vec::new();
    };
    match input {
        Value::Array(values) => values.iter().map(item_from_input).collect(),
        // The API's own rule: a bare string is one user message. Dropping it would make the item
        // list disagree with the request that produced the response.
        Value::String(text) => vec![user_message_item(text)],
        // A shape this module cannot describe honestly gets an empty list, not an invented item.
        _ => Vec::new(),
    }
}

/// One array element as an item object with an id this module mints.
fn item_from_input(value: &Value) -> Value {
    match value {
        Value::Object(fields) => {
            let kind = fields
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("message");
            let mut item = serde_json::Map::new();
            item.insert("type".to_owned(), Value::String(kind.to_owned()));
            item.insert("id".to_owned(), Value::String(item_id(kind)));
            for (key, value) in fields {
                // A client-supplied item id is a claim this module cannot honour: the id it reports
                // is the one it handed out.
                if key == "type" || key == "id" {
                    continue;
                }
                item.insert(key.clone(), value.clone());
            }
            Value::Object(item)
        }
        // An array element that is not an object is not a documented input shape. Wrapping it keeps
        // what the client sent visible instead of dropping it; no role is invented for it.
        other => serde_json::json!({
            "type": "message",
            "id": item_id("message"),
            "content": other,
        }),
    }
}

fn user_message_item(text: &str) -> Value {
    serde_json::json!({
        "type": "message",
        "id": item_id("message"),
        "role": "user",
        "content": text,
    })
}

/// An id in the shape the API uses for items: `msg_` for a message, `item_` otherwise.
fn item_id(kind: &str) -> String {
    let prefix = if kind == "message" { "msg" } else { "item" };
    format!("{prefix}_{}", mr_core::ids::new_id())
}

fn list_envelope(items: Vec<Value>) -> Value {
    let first_id = items
        .first()
        .and_then(|item| item.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let last_id = items
        .last()
        .and_then(|item| item.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    serde_json::json!({
        "object": "list",
        "data": items,
        "first_id": first_id,
        "last_id": last_id,
        "has_more": false,
    })
}

// --- Answers ---------------------------------------------------------------------------------

fn refusal(status: StatusCode, message: &str) -> Response {
    crate::server::error_response(status, Dialect::OpenAi, "invalid_request_error", message)
}

/// The one answer for every id this module cannot serve.
///
/// An id that was never stored, one that could never have been stored, and one that is not a legal
/// file name all get the same message: distinguishing them would let a caller probe this relay's
/// disk, and none of the three has a response to return.
fn not_stored(id: &str) -> Response {
    refusal(
        StatusCode::NOT_FOUND,
        &format!(
            "no response with id \"{id}\" is stored here. The relay records a response only when the \
             request that produced it set \"store\": true, and it never keeps one that was not asked \
             for."
        ),
    )
}

fn internal_error(error: &str) -> Response {
    crate::server::error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        Dialect::OpenAi,
        "internal_error",
        &format!("the stored response could not be read: {error}"),
    )
}

/// A JSON answer whose bytes are already JSON.
///
/// Used by `retrieve` and `cancel` so the stored object is returned as the provider wrote it.
fn raw_json_response(status: StatusCode, bytes: &[u8]) -> Response {
    let mut response = Response::new(AxumBody::from(bytes.to_vec()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

// --- File I/O --------------------------------------------------------------------------------
//
// These three helpers have the same properties as the private ones in `batch.rs`, and are written
// here rather than shared because `batch.rs` belongs to another surface: reaching into its
// internals would couple the responses store to a module this one does not own.

/// Write a file atomically: a temporary file in the same directory, then a rename.
///
/// A reader either sees the whole record or no record, never half of one. The temporary name is
/// per-write, so two writes of the same id cannot collide on it.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".{}.tmp", mr_core::ids::new_id()));
    std::fs::write(&temporary, bytes).map_err(|error| error.to_string())?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Leaving the temporary file behind would accumulate junk; failing to remove it is not
            // worth reporting twice, so it is ignored.
            let _ = std::fs::remove_file(&temporary);
            Err(error.to_string())
        }
    }
}

/// [`write_atomic`], off the async runtime's worker threads.
///
/// A stored response can be megabytes; writing one inside an async task would block a thread the
/// interactive request path shares. `spawn_blocking` moves it to the blocking pool, which is what
/// that pool exists for.
async fn write_atomic_async(path: PathBuf, bytes: Vec<u8>) -> Result<(), String> {
    match tokio::task::spawn_blocking(move || write_atomic(&path, &bytes)).await {
        Ok(result) => result,
        Err(error) => Err(format!("the write task failed: {error}")),
    }
}

/// Read a record, off the async runtime's worker threads.
///
/// `Ok(None)` is "no such record", which is a 404; every other failure is an `Err`, which is a 500.
/// The distinction matters: a permissions problem must not look like a response that was never
/// stored.
async fn read_record(id: &str) -> Result<Option<Vec<u8>>, String> {
    let path = record_path(id);
    match tokio::task::spawn_blocking(move || std::fs::read(&path)).await {
        Ok(Ok(bytes)) => Ok(Some(bytes)),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(format!("the read task failed: {error}")),
    }
}

/// Delete a record. `Ok(false)` means there was nothing to delete.
async fn remove_record(id: &str) -> Result<bool, String> {
    let path = record_path(id);
    match tokio::task::spawn_blocking(move || std::fs::remove_file(&path)).await {
        Ok(Ok(())) => Ok(true),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(format!("the delete task failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    /// Serialises the tests that point `MANAROUTER_STATE_DIR` at their own temporary directory.
    ///
    /// The variable is process-global, so two tests that set it would otherwise race: one would
    /// record into the other's directory and then read back a record it never wrote. A tokio mutex
    /// rather than a std one because a test holds the guard across awaits, and a std guard held
    /// across a suspension point would block the runtime's worker thread.
    static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A temporary state directory, held for the length of one test.
    struct Sandbox {
        _dir: tempfile::TempDir,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    async fn sandbox() -> Sandbox {
        let guard = ENV_LOCK.lock().await;
        let dir = tempfile::tempdir().expect("a temporary directory");
        std::env::set_var("MANAROUTER_STATE_DIR", dir.path());
        Sandbox {
            _dir: dir,
            _guard: guard,
        }
    }

    /// The upstream object every record test stores.
    const RESPONSE: &str = concat!(
        r#"{"id":"resp_123","object":"response","created_at":1760000000,"status":"completed","#,
        r#""model":"m","output":[{"type":"message","id":"msg_upstream","role":"assistant","#,
        r#""content":[{"type":"output_text","text":"hi"}]}]}"#
    );

    /// A request that asked for the response to be stored.
    const STORE_REQUEST: &[u8] = br#"{"model":"m","store":true,"input":"hello"}"#;

    /// A relay state, built through the same public constructors `server.rs`'s tests use.
    fn state() -> AppState {
        state_requiring_key(false)
    }

    fn state_requiring_key(require_key: bool) -> AppState {
        let mut config = mr_core::config::Config::default();
        if require_key {
            config.client_keys.push(mr_core::model::ClientKey {
                id: mr_core::ids::ClientKeyId::from_raw("test-key"),
                label: "test".to_owned(),
                key_hash: "0".repeat(64),
                prefix: None,
                enabled: true,
                allow_models: Vec::new(),
                allow_routes: Vec::new(),
                allow_combos: Vec::new(),
                limit_requests: 0,
                limits: mr_core::model::KeyLimits::default(),
                note: None,
            });
        }
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).expect("a snapshot");
        let relay = Arc::new(crate::state::RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).expect("an upstream client"),
        ));

        // A vault in a private temporary directory. It decrypts nothing in these tests; it only has
        // to exist for the state to be constructible.
        let vault_dir = tempfile::tempdir().expect("a vault directory");
        let vault = Arc::new(mr_core::secret::Vault::open(vault_dir.path()).expect("a vault"));

        let store = Arc::new(mr_core::store::Store::open_in_memory().expect("a ledger"));
        AppState {
            relay,
            no_key_required: !require_key,
            health: Arc::new(crate::select::Health::new()),
            vault,
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            account_inflight: Arc::new(crate::state::AccountInflight::default()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            batch: crate::batch::BatchLane::inert(),
            jobs: Arc::new(crate::jobs::JobLane::inert()),
        }
    }

    async fn call(app: &AppState, method: Method, uri: &str) -> (StatusCode, Vec<u8>) {
        call_with(app, method, uri, Body::empty()).await
    }

    async fn call_with(
        app: &AppState,
        method: Method,
        uri: &str,
        body: Body,
    ) -> (StatusCode, Vec<u8>) {
        let response = routes()
            .with_state(app.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(body)
                    .expect("a request"),
            )
            .await
            .expect("the router answered");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body")
            .to_vec();
        (status, bytes)
    }

    fn json(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("a json body")
    }

    #[tokio::test]
    async fn a_stored_response_is_returned_byte_for_byte() {
        let _sandbox = sandbox().await;
        let app = state();
        record(&app, STORE_REQUEST, 200, RESPONSE.as_bytes()).await;

        let (status, body) = call(&app, Method::GET, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::OK);
        // Not merely equivalent JSON: the provider's bytes are what the client reads back.
        assert_eq!(body, RESPONSE.as_bytes());
    }

    #[tokio::test]
    async fn a_request_that_did_not_ask_to_store_is_not_recorded() {
        let _sandbox = sandbox().await;
        let app = state();
        let request = br#"{"model":"m","input":"hello"}"#;
        record(&app, request, 200, RESPONSE.as_bytes()).await;
        assert!(!record_path("resp_123").exists());

        let (status, body) = call(&app, Method::GET, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body = json(&body);
        assert_eq!(body["error"]["code"], "invalid_request_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("store")),
            "the 404 has to say why: {body}"
        );
    }

    #[tokio::test]
    async fn delete_removes_the_record_and_a_second_delete_is_not_found() {
        let _sandbox = sandbox().await;
        let app = state();
        record(&app, STORE_REQUEST, 200, RESPONSE.as_bytes()).await;

        let (status, body) = call(&app, Method::DELETE, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            json(&body),
            serde_json::json!({
                "id": "resp_123",
                "object": "response.deleted",
                "deleted": true,
            })
        );

        let (status, _) = call(&app, Method::GET, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = call(&app, Method::DELETE, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(json(&body)["error"]["code"], "invalid_request_error");
    }

    #[tokio::test]
    async fn a_completed_response_cannot_be_cancelled() {
        let _sandbox = sandbox().await;
        let app = state();
        record(&app, STORE_REQUEST, 200, RESPONSE.as_bytes()).await;

        let (status, body) = call(&app, Method::POST, "/v1/responses/resp_123/cancel").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let body = json(&body);
        assert_eq!(body["error"]["code"], "invalid_request_error");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("completed")),
            "the 400 has to name the final status: {body}"
        );
    }

    #[tokio::test]
    async fn an_unfinished_response_is_returned_untouched_rather_than_pretending_to_cancel() {
        let _sandbox = sandbox().await;
        let app = state();
        let response = r#"{"id":"resp_live","object":"response","status":"in_progress"}"#;
        record(&app, STORE_REQUEST, 200, response.as_bytes()).await;

        let (status, body) = call(&app, Method::POST, "/v1/responses/resp_live/cancel").await;
        assert_eq!(status, StatusCode::OK);
        // The relay has no upstream handle left, so it returns the record with its status
        // untouched instead of reporting a cancellation that did not happen.
        assert_eq!(body, response.as_bytes());
    }

    #[tokio::test]
    async fn input_items_returns_one_item_per_array_element() {
        let _sandbox = sandbox().await;
        let app = state();
        let request = concat!(
            r#"{"model":"m","store":true,"input":["#,
            r#"{"type":"message","role":"user","content":"hello"},"#,
            r#"{"type":"function_call_output","call_id":"c1","output":"42"}]}"#
        );
        record(&app, request.as_bytes(), 200, RESPONSE.as_bytes()).await;

        let (status, body) = call(&app, Method::GET, "/v1/responses/resp_123/input_items").await;
        assert_eq!(status, StatusCode::OK);
        let body = json(&body);
        assert_eq!(body["object"], "list");
        assert_eq!(body["has_more"], false);
        let data = body["data"].as_array().expect("a data array");
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["type"], "message");
        assert_eq!(data[0]["role"], "user");
        assert_eq!(data[0]["content"], "hello");
        assert!(
            data[0]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("msg_")),
            "a message item gets a message id: {}",
            data[0]
        );
        assert_eq!(data[1]["type"], "function_call_output");
        assert_eq!(data[1]["output"], "42");
        assert!(
            data[1]["id"]
                .as_str()
                .is_some_and(|id| id.starts_with("item_")),
            "a non-message item gets an item id: {}",
            data[1]
        );
        assert_eq!(body["first_id"], data[0]["id"]);
        assert_eq!(body["last_id"], data[1]["id"]);
    }

    #[tokio::test]
    async fn a_string_input_is_one_user_message() {
        let _sandbox = sandbox().await;
        let app = state();
        record(&app, STORE_REQUEST, 200, RESPONSE.as_bytes()).await;

        let (status, body) = call(&app, Method::GET, "/v1/responses/resp_123/input_items").await;
        assert_eq!(status, StatusCode::OK);
        let body = json(&body);
        let data = body["data"].as_array().expect("a data array");
        assert_eq!(data.len(), 1, "a string input is one message, not none");
        assert_eq!(data[0]["type"], "message");
        assert_eq!(data[0]["role"], "user");
        assert_eq!(data[0]["content"], "hello");
        assert_eq!(body["first_id"], data[0]["id"]);
        assert_eq!(body["last_id"], data[0]["id"]);
    }

    #[tokio::test]
    async fn a_record_without_a_readable_request_has_no_input_items() {
        let _sandbox = sandbox().await;
        let app = state();
        // `record` will not store a request it cannot read, so the file is written directly: this is
        // the shape an older relay, or a hand-repaired record, would leave behind.
        let envelope = envelope_bytes(RESPONSE.as_bytes(), b"not json at all");
        write_atomic(&record_path("resp_123"), &envelope).expect("a record");

        let (status, body) = call(&app, Method::GET, "/v1/responses/resp_123/input_items").await;
        assert_eq!(status, StatusCode::OK);
        let body = json(&body);
        assert_eq!(body["object"], "list");
        assert!(body["data"].as_array().expect("a data array").is_empty());
        assert_eq!(body["first_id"], Value::Null);
        assert_eq!(body["last_id"], Value::Null);
        assert_eq!(body["has_more"], false);
    }

    #[test]
    fn ids_that_could_escape_the_store_are_rejected() {
        assert!(valid_id("resp_123"));
        assert!(valid_id("resp-123.v2"));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"a".repeat(200)));
        assert!(!valid_id(""));
        assert!(!valid_id("a b"));
        assert!(!valid_id("caf\u{e9}"));
    }

    #[tokio::test]
    async fn a_response_with_a_path_like_id_is_not_written() {
        let _sandbox = sandbox().await;
        let app = state();
        let long = "a".repeat(200);
        for id in ["../x", "a/b", long.as_str()] {
            let response = format!(r#"{{"id":"{id}","object":"response"}}"#);
            record(&app, STORE_REQUEST, 200, response.as_bytes()).await;
        }
        let entries = std::fs::read_dir(storage_root())
            .map(|dir| dir.count())
            .unwrap_or(0);
        assert_eq!(
            entries, 0,
            "an id that is not a safe file name must never become a path"
        );
    }

    #[tokio::test]
    async fn a_record_survives_a_restart() {
        let _sandbox = sandbox().await;
        let first = state();
        record(&first, STORE_REQUEST, 200, RESPONSE.as_bytes()).await;
        assert!(record_path("resp_123").is_file());
        drop(first);

        // A second state stands in for a restarted process. This module keeps no in-memory index, so
        // the file is the only thing that can answer — and it does.
        let second = state();
        let (status, body) = call(&second, Method::GET, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, RESPONSE.as_bytes());
    }

    #[tokio::test]
    async fn an_oversized_response_is_not_recorded_and_the_caller_is_unaffected() {
        let _sandbox = sandbox().await;
        let app = state();
        let huge = format!(
            r#"{{"id":"resp_big","object":"response","status":"completed","padding":"{}"}}"#,
            "x".repeat(MAX_STORED_BYTES)
        );
        // The call returns normally: the client already has its answer, and a record the relay
        // declined to keep must not turn a successful proxy into an error.
        record(&app, STORE_REQUEST, 200, huge.as_bytes()).await;
        assert!(!record_path("resp_big").exists());

        let (status, _) = call(&app, Method::GET, "/v1/responses/resp_big").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unauthenticated_read_is_refused() {
        let _sandbox = sandbox().await;
        let app = state_requiring_key(true);
        let (status, body) = call(&app, Method::GET, "/v1/responses/resp_123").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(json(&body)["error"]["code"], "invalid_api_key");
    }
}
