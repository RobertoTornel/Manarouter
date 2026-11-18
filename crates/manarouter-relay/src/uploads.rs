//! The resumable upload surface: `POST /v1/uploads` and the three steps that follow it.
//!
//! A client that wants to send a large file — a batch of tens of thousands of requests, or a corpus
//! for a future fine-tune — opens an upload, sends the bytes in parts, and completes the upload,
//! which turns the assembled bytes into the same `file` object `POST /v1/files` produces. The point
//! of the shape is that a connection that dies halfway costs one part, not the whole file.
//!
//! ```text
//! POST /v1/uploads                    {"filename","purpose","bytes"} -> upload (pending)
//! POST /v1/uploads/{id}/parts         multipart "data"              -> upload.part
//! POST /v1/uploads/{id}/complete      {"part_ids": [...]}           -> file
//! POST /v1/uploads/{id}/cancel                                      -> upload (cancelled)
//! ```
//!
//! `mime_type` is accepted on create and ignored: OpenAI takes it there, but neither the upload nor
//! the file record has a field for it, so there is nothing here to store it in.
//!
//! # The file is the lane's file
//!
//! Complete does not write a second kind of record. It assembles the parts and hands the bytes to
//! [`crate::batch::BatchLane::put_file`], the same function `POST /v1/files` calls, so the object
//! returned by complete is byte-for-byte the object a one-shot upload returns, the file has the same
//! id shape, and `GET /v1/files/{id}/content` and `POST /v1/batches` find it without knowing that it
//! arrived in parts. Nothing in the file record says "this one was resumable": that is a transport
//! detail, not a property of the file.
//!
//! # Storage
//!
//! One directory per upload under `state_dir()/uploads/<id>/`:
//!
//! ```text
//! upload.json                 the upload record (id, size, status, parts)
//! part-000000000000.bin       the first part, in arrival order
//! part-000000000001.bin       the second part
//! ```
//!
//! `upload.json` is written to a temporary name and renamed into place, so a reader sees the old
//! record or the new one and never half of either. The part *names* come from an internal sequence
//! number, never from anything a client sent: a client filename is metadata that is stored in the
//! record and copied into the file record, and it is never used to open a path. The upload id in the
//! URL *is* used as a path component, so [`valid_id`] checks it first — see the security note there.
//!
//! # Status machine
//!
//! ```text
//! pending --complete--> completed
//!    |
//!    +-----cancel-------> cancelled
//! ```
//!
//! There is no transition out of `completed` or `cancelled`. A part sent to either is refused with
//! `400 invalid_request_error`; completing a cancelled upload, or completing twice, is refused the
//! same way. A cancelled upload may be cancelled again: the state is already what the caller asked
//! for, so the answer is the same object rather than an error.
//!
//! # Expiry
//!
//! An upload expires 24 hours after it was created. The parts of an expired upload are deleted the
//! next time its id is used, and that request gets a 404 that says the upload expired and that a new
//! one should be started.
//!
//! Deleting on access rather than on a timer is deliberate. The only reader of an upload's parts is
//! a request that names the upload, so the access that discovers the expiry is also the access that
//! would have used the parts; a timer per upload would cost a task and a wakeup per open upload to
//! reclaim disk that only that upload's own requests can spend. The cost of this choice is that an
//! upload whose client never comes back keeps its parts until an operator (or a future sweeper)
//! removes them; that cost is bounded per upload by [`MAX_UPLOAD_BYTES`], and a sweep of the
//! `uploads/` directory is the natural place to add one. What lazy deletion does not cost is
//! correctness: an expired upload never serves a byte, whether or not its files are still on disk.
//!
//! # The cap
//!
//! [`MAX_UPLOAD_BYTES`] is *the lane's file cap*, not a second opinion about size: the bytes are
//! going to be stored by the batch lane, so an upload larger than the lane accepts could never
//! become a file. The constant aliases [`crate::batch::MAX_FILE_BYTES`], and when that number moves
//! this one moves with it.

use std::collections::hash_map::DefaultHasher;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path as RoutePath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::batch::{Refusal, MAX_FILE_BYTES};
use crate::server::{authorize_batch_request, json_response, AppState};

/// The largest upload this surface will store, in bytes.
///
/// The bytes end up in the batch lane's file store, so the ceiling is the lane's own file cap, and
/// it is aliased rather than restated: a second, larger number here would only produce uploads that
/// complete into a refusal, and a second, smaller one would refuse files the lane would have taken.
/// When the lane's cap moves, this moves with it.
pub const MAX_UPLOAD_BYTES: usize = MAX_FILE_BYTES;

/// How long an upload stays usable after it is created.
const UPLOAD_TTL_SECONDS: u64 = 24 * 60 * 60;

/// A JSON body larger than this is not an upload or completion request; refuse before buffering it.
const MAX_JSON_BODY_BYTES: usize = 1024 * 1024;

/// A filename is metadata, not a path, but it is still stored; keep it to a size a record can hold.
const MAX_FILENAME_BYTES: usize = 1024;

/// The lane stores the purpose verbatim; a purpose longer than this is not one of anyone's.
const MAX_PURPOSE_BYTES: usize = 64;

/// The number of lock shards used to serialize operations on one upload.
const LOCK_SHARDS: usize = 64;

/// Room for the multipart envelope (boundaries, part headers) on top of the part's own bytes.
///
/// A part may be as large as the file cap, so the request body is the cap plus this much. It is not
/// a second size policy: the checks that decide whether a part is acceptable are the declared size
/// and [`MAX_UPLOAD_BYTES`], applied to the part's data as it streams.
const MULTIPART_OVERHEAD_BYTES: usize = 64 * 1024;

// --- Records --------------------------------------------------------------------------------

/// Where an upload is in its life. Persisted as `snake_case`, which is also how it is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum UploadStatus {
    Pending,
    Completed,
    Cancelled,
}

impl UploadStatus {
    fn as_str(self) -> &'static str {
        match self {
            UploadStatus::Pending => "pending",
            UploadStatus::Completed => "completed",
            UploadStatus::Cancelled => "cancelled",
        }
    }
}

/// One stored part. `sequence` is internal: it names the file, and it is the order the parts are
/// assembled in when the client does not name an order itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PartRecord {
    id: String,
    sequence: u64,
    bytes: u64,
    created_at: u64,
}

impl PartRecord {
    fn view(&self, upload_id: &str) -> Value {
        json!({
            "id": self.id,
            "object": "upload.part",
            "created_at": self.created_at,
            "upload_id": upload_id,
        })
    }
}

/// The on-disk record. This is the whole state of an upload; there is no in-memory index, which is
/// what makes an upload survive a restart without replaying anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UploadRecord {
    id: String,
    /// The total the client said it would send. A promise, checked against what actually arrives.
    bytes: u64,
    created_at: u64,
    expires_at: u64,
    filename: String,
    purpose: String,
    status: UploadStatus,
    /// The next sequence number to hand out. A part that fails does not consume one.
    next_sequence: u64,
    parts: Vec<PartRecord>,
}

impl UploadRecord {
    /// The object shape OpenAI documents for an upload. Only these fields: the record's internal
    /// bookkeeping (`parts`, `next_sequence`) stays on disk.
    fn view(&self) -> Value {
        json!({
            "id": self.id,
            "object": "upload",
            "bytes": self.bytes,
            "created_at": self.created_at,
            "expires_at": self.expires_at,
            "filename": self.filename,
            "purpose": self.purpose,
            "status": self.status.as_str(),
        })
    }
}

// --- Paths ----------------------------------------------------------------------------------

/// The root of the surface's own storage.
///
/// In tests it is one temporary directory for the whole test binary, like the eval store's root:
/// pointing the relay at a test directory would mean setting `MANAROUTER_STATE_DIR`, which is
/// process-global, and the test modules run in parallel with each other's use of it.
fn uploads_root() -> PathBuf {
    #[cfg(test)]
    {
        static TEST_ROOT: OnceLock<PathBuf> = OnceLock::new();
        TEST_ROOT
            .get_or_init(|| {
                tempfile::tempdir()
                    .expect("a temporary directory for the upload store")
                    .keep()
                    .join("uploads")
            })
            .clone()
    }
    #[cfg(not(test))]
    {
        mr_core::config::state_dir().join("uploads")
    }
}

/// The directory holding one upload.
///
/// The `id` is a path component here, so every caller must have validated it with [`valid_id`]
/// first. The only ids that reach this function otherwise are ones this module generated.
fn upload_dir(id: &str) -> PathBuf {
    uploads_root().join(id)
}

fn record_path(id: &str) -> PathBuf {
    upload_dir(id).join("upload.json")
}

/// The path of one part. The name is derived from the sequence number, never from client input.
fn part_path(id: &str, sequence: u64) -> PathBuf {
    upload_dir(id).join(format!("part-{sequence:012}.bin"))
}

/// Whether an id taken from a URL may be used as a path component.
///
/// This is a security boundary, not a formatting preference. The id is joined into the state
/// directory, so a value such as `../x` would otherwise let a request address files outside its own
/// upload directory. The rule is deliberately a positive list — ASCII alphanumerics and `-_.`, one
/// to 128 characters — rather than a list of things to strip: a path separator, a NUL or a Windows
/// drive prefix all fail it, and `.` and `..` are rejected by name because they are made only of
/// allowed characters but name a directory rather than a child of one.
///
/// A malformed id is answered with the same 404 as an id that does not exist: it can never name a
/// real upload (this module only ever mints `upload_<uuid>`), and saying "your id contains the wrong
/// character" would describe the path layout to a caller who has no business knowing it.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || byte == b'.'
        })
}

// --- Failure helpers ------------------------------------------------------------------------

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn bad_request(message: impl Into<String>) -> Refusal {
    Refusal::bad_request("invalid_request_error", message)
}

fn internal(message: impl Into<String>) -> Refusal {
    Refusal::internal(message)
}

fn no_such_upload(id: &str) -> Refusal {
    Refusal::not_found("upload", id)
}

/// The 404 an expired upload gets. It says what happened and what to do, unlike a bare "not found",
/// because the client did nothing wrong and a retry of the same id will never work.
fn expired_upload(id: &str, expires_at: u64) -> Refusal {
    Refusal::with_status(
        StatusCode::NOT_FOUND,
        "invalid_request_error",
        format!(
            "upload \"{id}\" expired at {expires_at} and its parts have been deleted; start a new upload"
        ),
    )
}

// --- Persistence ----------------------------------------------------------------------------

/// Read a record, distinguishing "there is none" from "it could not be read".
///
/// A missing file is a normal answer: the id is unknown, or the upload expired and was cleaned up.
/// A file that exists but cannot be parsed is not a normal answer — something is wrong with the
/// state directory — so it is a 500 rather than a 404 that would invite the client to keep asking.
async fn load_record(id: &str) -> Result<Option<UploadRecord>, Refusal> {
    let bytes = match tokio::fs::read(record_path(id)).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(internal(format!(
                "the record for upload \"{id}\" could not be read: {error}"
            )))
        }
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        internal(format!(
            "the record for upload \"{id}\" is not valid JSON: {error}"
        ))
    })
}

/// Read an unexpired record, deleting the upload's directory if it has expired.
///
/// This is the lazy-deletion point described in the module docs: every route that names an upload
/// arrives here, and the one that finds it expired is the one that removes it.
async fn load_live(id: &str) -> Result<UploadRecord, Refusal> {
    let Some(record) = load_record(id).await? else {
        return Err(no_such_upload(id));
    };
    if now_seconds() >= record.expires_at {
        remove_upload_dir(id).await;
        return Err(expired_upload(id, record.expires_at));
    }
    Ok(record)
}

/// Write a record atomically: a temporary file next to it, then a rename.
///
/// The rename is the commit. A crash before it leaves the previous record — and possibly a `.tmp`
/// file, which [`delete_part_files`] removes — while a crash after it leaves the new record and
/// never a truncated one.
async fn write_record(record: &UploadRecord) -> Result<(), String> {
    let dir = upload_dir(&record.id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    let temp = dir.join(format!(".upload-{}.tmp", mr_core::ids::new_id()));
    tokio::fs::write(&temp, &bytes)
        .await
        .map_err(|error| error.to_string())?;
    match tokio::fs::rename(&temp, record_path(&record.id)).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp).await;
            Err(error.to_string())
        }
    }
}

/// Remove an upload's whole directory, ignoring an already-absent one.
async fn remove_upload_dir(id: &str) {
    match tokio::fs::remove_dir_all(upload_dir(id)).await {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(upload = %id, %error, "an expired upload's directory could not be removed");
        }
    }
}

/// Delete everything in an upload's directory except its record.
///
/// The directory is this module's own: the only things that can be in it are the record, the parts,
/// and the temporary files this module creates. Removing everything but the record therefore
/// reclaims exactly what a cancel or a complete no longer needs without having to trust the part
/// list, which may be out of date after a crash.
async fn delete_part_files(id: &str) {
    let dir = upload_dir(id);
    let mut entries = match tokio::fs::read_dir(&dir).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(upload = %id, %error, "an upload's parts could not be listed");
            return;
        }
    };
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                if entry.file_name() == OsStr::new("upload.json") {
                    continue;
                }
                if let Err(error) = tokio::fs::remove_file(entry.path()).await {
                    tracing::warn!(upload = %id, %error, "an upload's part could not be removed");
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(upload = %id, %error, "an upload's parts could not be listed");
                break;
            }
        }
    }
}

/// Serialize operations on one upload without keeping a map that grows with every id ever seen.
///
/// Parts of one upload arrive concurrently and each one appends to the same record, so the
/// read-modify-write has to be serialized. A lock map would need an entry per upload id and a
/// policy for when to drop it; a fixed set of shards needs neither, at the cost of two unrelated
/// uploads occasionally waiting on each other. The shard is held across the disk work, which is the
/// point — but it is a `tokio` mutex, so waiting does not block the runtime.
fn upload_lock(id: &str) -> &'static tokio::sync::Mutex<()> {
    static SHARDS: OnceLock<[tokio::sync::Mutex<()>; LOCK_SHARDS]> = OnceLock::new();
    let shards = SHARDS.get_or_init(|| std::array::from_fn(|_| tokio::sync::Mutex::new(())));
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    &shards[(hasher.finish() as usize) % LOCK_SHARDS]
}

// --- Bodies ---------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateUploadRequest {
    filename: String,
    #[serde(default)]
    purpose: Option<String>,
    bytes: u64,
    // `mime_type` is accepted and ignored: OpenAI takes it on the request but does not put it on the
    // upload or the file, and this relay has nothing that would use it. An unknown field is ignored
    // by serde, so there is no field here to keep in sync.
}

#[derive(Debug, Deserialize)]
struct CompleteUploadRequest {
    /// Omitted, `null`, or empty all mean "every part, in arrival order".
    #[serde(default)]
    part_ids: Option<Vec<String>>,
}

/// Buffer and parse a JSON body with this surface's error envelope.
///
/// The body is read here rather than by a `Json` extractor so that authentication and id validation
/// happen before a byte of the body is buffered, and so that a malformed body comes back as the
/// relay's error envelope instead of the extractor's plain-text rejection.
async fn read_json<T: serde::de::DeserializeOwned>(request: Request) -> Result<T, Refusal> {
    let bytes = axum::body::to_bytes(request.into_body(), MAX_JSON_BODY_BYTES)
        .await
        .map_err(|error| bad_request(format!("the request body could not be read: {error}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|error| bad_request(format!("the request body is not valid JSON: {error}")))
}

// --- Handlers -------------------------------------------------------------------------------

/// Mount this surface. `server::router` merges the result.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/uploads", post(create_upload))
        .route("/v1/uploads/{id}/parts", post(add_part))
        .route("/v1/uploads/{id}/complete", post(complete_upload))
        .route("/v1/uploads/{id}/cancel", post(cancel_upload))
        // `Multipart` otherwise inherits axum's 2 MiB default for extractors, which would cap a
        // part far below the file cap. Raise the ceiling to one full part plus its envelope rather
        // than disabling it: disabled, a field the client put before `data` could make the relay
        // read an unbounded body that no size check ever sees. Within this bound the part is still
        // judged by its declared size and the cap while it streams, which is where the useful
        // messages live.
        .layer(DefaultBodyLimit::max(
            MAX_UPLOAD_BYTES + MULTIPART_OVERHEAD_BYTES,
        ))
}

/// `POST /v1/uploads`.
pub async fn create_upload(
    State(app): State<AppState>,
    headers: HeaderMap,
    request: Request,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    let body = match read_json::<CreateUploadRequest>(request).await {
        Ok(body) => body,
        Err(refusal) => return refusal.into_response(),
    };
    // The name is metadata (it is copied into the file record and never opened as a path), but it is
    // still the name a person will see, so it must not be empty and must not be unbounded.
    let filename = body.filename.trim().to_owned();
    if filename.is_empty() {
        return bad_request("filename must not be empty").into_response();
    }
    if filename.len() > MAX_FILENAME_BYTES {
        return bad_request(format!(
            "filename is longer than the {MAX_FILENAME_BYTES}-byte limit"
        ))
        .into_response();
    }
    // A batch submission is the one kind of file this lane stores today, so an omitted purpose
    // means `batch`; an explicit one is passed through to the lane unchanged.
    let purpose = body
        .purpose
        .map(|purpose| purpose.trim().to_owned())
        .unwrap_or_else(|| "batch".to_owned());
    if purpose.is_empty() || purpose.len() > MAX_PURPOSE_BYTES {
        return bad_request(format!(
            "purpose must be between 1 and {MAX_PURPOSE_BYTES} bytes"
        ))
        .into_response();
    }
    // The declaration is a promise, and the surface refuses a part that would break it. A promise
    // larger than the hard cap can never be kept, so the honest answer is here rather than after
    // the client has spent a connection sending bytes.
    if body.bytes > MAX_UPLOAD_BYTES as u64 {
        return bad_request(format!(
            "this upload declares {} bytes, over the {MAX_UPLOAD_BYTES}-byte limit for one file",
            body.bytes
        ))
        .into_response();
    }
    let created_at = now_seconds();
    let record = UploadRecord {
        id: format!("upload_{}", mr_core::ids::new_id()),
        bytes: body.bytes,
        created_at,
        expires_at: created_at + UPLOAD_TTL_SECONDS,
        filename,
        purpose,
        status: UploadStatus::Pending,
        next_sequence: 0,
        parts: Vec::new(),
    };
    if let Err(error) = write_record(&record).await {
        return internal(format!("the upload could not be created: {error}")).into_response();
    }
    json_response(StatusCode::OK, record.view())
}

/// `POST /v1/uploads/{id}/parts`.
pub async fn add_part(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return no_such_upload(&id).into_response();
    }
    let lock = upload_lock(&id);
    let _guard = lock.lock().await;
    match add_part_locked(&id, request).await {
        Ok(part) => json_response(StatusCode::OK, part),
        Err(refusal) => refusal.into_response(),
    }
}

async fn add_part_locked(id: &str, request: Request) -> Result<Value, Refusal> {
    let mut record = load_live(id).await?;
    match record.status {
        UploadStatus::Pending => {}
        UploadStatus::Completed => {
            return Err(bad_request(format!(
                "upload \"{id}\" is complete; no more parts may be added"
            )))
        }
        UploadStatus::Cancelled => {
            return Err(bad_request(format!(
                "upload \"{id}\" was cancelled; no more parts may be added"
            )))
        }
    }
    let mut form = Multipart::from_request(request, &())
        .await
        .map_err(|error| bad_request(format!("the part form could not be read: {error}")))?;
    let (temp, bytes) = stream_part(&mut form, &record).await?;
    let sequence = record.next_sequence;
    let path = part_path(id, sequence);
    if let Err(error) = tokio::fs::rename(&temp, &path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(internal(format!("the part could not be stored: {error}")));
    }
    let part = PartRecord {
        id: format!("part_{}", mr_core::ids::new_id()),
        sequence,
        bytes,
        created_at: now_seconds(),
    };
    record.next_sequence += 1;
    record.parts.push(part.clone());
    if let Err(error) = write_record(&record).await {
        // The bytes are already on disk; without a record that names them they are unreachable, so
        // remove them rather than leave a file no request can ever read or reclaim.
        let _ = tokio::fs::remove_file(&path).await;
        return Err(internal(format!(
            "the part was stored but its record could not be written: {error}"
        )));
    }
    Ok(part.view(id))
}

/// Read the `data` field and write it to a temporary file, returning the path and the byte count.
///
/// The field is streamed through [`axum::extract::multipart::Field::chunk`] rather than buffered:
/// an upload's whole point is that it can be larger than memory, and the size checks can be applied
/// to each chunk before it is written, so a client that lies about its size is cut off at the first
/// chunk beyond the limit instead of after the relay has held the lie in RAM.
///
/// The temporary file is cleaned up if anything fails. Its name carries a random suffix as well as
/// the sequence number, so a retry after a failure cannot collide with the debris of the failure.
async fn stream_part(
    form: &mut Multipart,
    record: &UploadRecord,
) -> Result<(PathBuf, u64), Refusal> {
    let mut temp: Option<PathBuf> = None;
    let outcome = stream_into_temp(form, record, &mut temp).await;
    match outcome {
        Ok(bytes) => match temp {
            Some(path) => Ok((path, bytes)),
            None => Err(internal("the part was stored without a temporary path")),
        },
        Err(error) => {
            if let Some(path) = temp {
                let _ = tokio::fs::remove_file(path).await;
            }
            Err(error)
        }
    }
}

async fn stream_into_temp(
    form: &mut Multipart,
    record: &UploadRecord,
    temp: &mut Option<PathBuf>,
) -> Result<u64, Refusal> {
    while let Some(mut field) = form
        .next_field()
        .await
        .map_err(|error| bad_request(format!("the part form could not be read: {error}")))?
    {
        // Only `data` carries bytes. Anything else is a field a client library added, and it is
        // dropped rather than refused: axum drains it when the next field is asked for.
        // The first `data` field *is* the part, and the function returns from inside it, so any
        // field after it is left unread: the connection is done with, and reading to the end of a
        // body nobody asked for would only cost bandwidth.
        if field.name() != Some("data") {
            continue;
        }
        let path = upload_dir(&record.id).join(format!(
            ".part-{:012}-{}.tmp",
            record.next_sequence,
            mr_core::ids::new_id()
        ));
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|error| internal(format!("the part could not be stored: {error}")))?;
        *temp = Some(path);
        let mut stored = 0u64;
        while let Some(chunk) = field
            .chunk()
            .await
            .map_err(|error| bad_request(format!("the part data could not be read: {error}")))?
        {
            let arrived = stored + chunk.len() as u64;
            if arrived > record.bytes {
                return Err(bad_request(format!(
                    "this upload declared {} bytes, but the parts received so far would total {arrived}",
                    record.bytes
                )));
            }
            // A backstop, not the first line of defence: create refuses a declaration above the cap,
            // so a record written through the API can never reach this branch. It stays because a
            // record is on disk, and a number on disk is not a promise this process made.
            if arrived > MAX_UPLOAD_BYTES as u64 {
                return Err(bad_request(format!(
                    "this part would put the upload over the {MAX_UPLOAD_BYTES}-byte limit for one file"
                )));
            }
            file.write_all(&chunk)
                .await
                .map_err(|error| internal(format!("the part could not be stored: {error}")))?;
            stored = arrived;
        }
        file.flush()
            .await
            .map_err(|error| internal(format!("the part could not be stored: {error}")))?;
        return Ok(stored);
    }
    Err(bad_request("the part form must contain a \"data\" part"))
}

/// `POST /v1/uploads/{id}/complete`.
pub async fn complete_upload(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    request: Request,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return no_such_upload(&id).into_response();
    }
    // The body is tiny and independent of the record, so it is read before the lock: a slow client
    // must not hold an upload's lock while it finishes sending its JSON.
    let body = match read_json::<CompleteUploadRequest>(request).await {
        Ok(body) => body,
        Err(refusal) => return refusal.into_response(),
    };
    let lock = upload_lock(&id);
    let _guard = lock.lock().await;
    match complete_locked(&app, &id, body).await {
        Ok(file) => json_response(StatusCode::OK, file),
        Err(refusal) => refusal.into_response(),
    }
}

async fn complete_locked(
    app: &AppState,
    id: &str,
    body: CompleteUploadRequest,
) -> Result<Value, Refusal> {
    let record = load_live(id).await?;
    match record.status {
        UploadStatus::Pending => {}
        UploadStatus::Completed => {
            return Err(bad_request(format!("upload \"{id}\" is already complete")))
        }
        UploadStatus::Cancelled => {
            return Err(bad_request(format!(
                "upload \"{id}\" was cancelled and cannot be completed"
            )))
        }
    }
    let selected = match body.part_ids {
        None => record.parts.clone(),
        Some(part_ids) if part_ids.is_empty() => record.parts.clone(),
        Some(part_ids) => {
            let mut selected = Vec::with_capacity(part_ids.len());
            for part_id in part_ids {
                match record.parts.iter().find(|part| part.id == part_id) {
                    Some(part) => selected.push(part.clone()),
                    None => {
                        return Err(bad_request(format!(
                            "part \"{part_id}\" does not belong to upload \"{id}\""
                        )))
                    }
                }
            }
            selected
        }
    };
    let assembled = assemble(id, &selected).await?;
    let file = app
        .batch
        .put_file(
            assembled,
            Some(record.filename.clone()),
            Some(record.purpose.clone()),
        )
        .await?;
    // The file exists in the lane now. Marking the upload complete is what makes a second complete a
    // 400 instead of a second file; the window between the two is one atomic rename, and the order
    // is deliberate: a completed record with no file would be a broken reference, while a file with
    // a still-pending upload is merely an unreferenced file that a retry would duplicate.
    let mut record = record;
    record.status = UploadStatus::Completed;
    if let Err(error) = write_record(&record).await {
        return Err(internal(format!(
            "the file was stored but the upload record could not be updated: {error}"
        )));
    }
    // The bytes are the file's now; the parts are a second copy nobody can read through this
    // surface any more. Keep the record (it documents what was assembled) and reclaim the bytes.
    delete_part_files(id).await;
    Ok(file)
}

/// Assemble the selected parts, in the given order, into one buffer.
///
/// The parts are copied through the filesystem with a fixed-size buffer rather than read into
/// memory one at a time and concatenated: the assembled file is going to be read once at the end
/// for the lane, and this way it is never held twice. The final read is the one copy the lane needs.
async fn assemble(id: &str, parts: &[PartRecord]) -> Result<Vec<u8>, Refusal> {
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let dir = upload_dir(id);
    let path = dir.join(format!(".assembled-{}.tmp", mr_core::ids::new_id()));
    let bytes = match assemble_into(id, parts, &path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(error);
        }
    };
    if let Err(error) = tokio::fs::remove_file(&path).await {
        tracing::warn!(upload = %id, %error, "an assembled upload could not be removed");
    }
    Ok(bytes)
}

async fn assemble_into(id: &str, parts: &[PartRecord], path: &Path) -> Result<Vec<u8>, Refusal> {
    let mut output = tokio::fs::File::create(path)
        .await
        .map_err(|error| internal(format!("the upload could not be assembled: {error}")))?;
    let mut total = 0u64;
    for part in parts {
        let part_path = part_path(id, part.sequence);
        let mut input = tokio::fs::File::open(&part_path).await.map_err(|error| {
            internal(format!("part \"{}\" could not be read: {error}", part.id))
        })?;
        total += tokio::io::copy(&mut input, &mut output)
            .await
            .map_err(|error| internal(format!("the upload could not be assembled: {error}")))?;
    }
    output
        .flush()
        .await
        .map_err(|error| internal(format!("the upload could not be assembled: {error}")))?;
    drop(output);
    let expected: u64 = parts.iter().map(|part| part.bytes).sum();
    if total != expected {
        return Err(internal(format!(
            "the parts of upload \"{id}\" total {total} bytes on disk but {} in its record",
            expected
        )));
    }
    tokio::fs::read(path)
        .await
        .map_err(|error| internal(format!("the upload could not be read back: {error}")))
}

/// `POST /v1/uploads/{id}/cancel`.
pub async fn cancel_upload(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    if !valid_id(&id) {
        return no_such_upload(&id).into_response();
    }
    let lock = upload_lock(&id);
    let _guard = lock.lock().await;
    match cancel_locked(&id).await {
        Ok(upload) => json_response(StatusCode::OK, upload),
        Err(refusal) => refusal.into_response(),
    }
}

async fn cancel_locked(id: &str) -> Result<Value, Refusal> {
    let mut record = load_live(id).await?;
    match record.status {
        // Already what the caller asked for. Cancelling twice is a retry, not a mistake.
        UploadStatus::Cancelled => return Ok(record.view()),
        UploadStatus::Completed => {
            return Err(bad_request(format!(
                "upload \"{id}\" is complete; it cannot be cancelled"
            )))
        }
        UploadStatus::Pending => {}
    }
    record.status = UploadStatus::Cancelled;
    if let Err(error) = write_record(&record).await {
        return Err(internal(format!(
            "the upload could not be cancelled: {error}"
        )));
    }
    delete_part_files(id).await;
    Ok(record.view())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method};
    use std::sync::Arc;
    use tower::ServiceExt;

    /// Where one test's upload lives. The ids are unique, so tests share the root safely.
    fn dir_for(id: &str) -> PathBuf {
        uploads_root().join(id)
    }

    fn test_app(config: mr_core::config::Config, no_key_required: bool) -> AppState {
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(crate::state::RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let vault = mr_core::secret::Vault::open(dir.path()).unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        AppState {
            relay,
            no_key_required,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(vault),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            batch: crate::batch::BatchLane::inert(),
        }
    }

    fn open_app() -> AppState {
        test_app(mr_core::config::Config::default(), true)
    }

    fn keyed_app() -> AppState {
        let presented = "mr-test-key";
        let key_hash: String = crate::state::RelayState::hash_presented(presented)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let config = mr_core::config::Config {
            client_keys: vec![mr_core::model::ClientKey {
                id: mr_core::ids::ClientKeyId::from_raw("key-1"),
                label: "uploads-test".to_owned(),
                key_hash,
                prefix: Some("mr-test".to_owned()),
                enabled: true,
                allow_models: Vec::new(),
                allow_routes: Vec::new(),
                allow_combos: Vec::new(),
                limit_requests: 0,
                limits: Default::default(),
                note: None,
            }],
            ..Default::default()
        };
        test_app(config, false)
    }

    fn api(state: AppState) -> Router {
        routes().with_state(state)
    }

    async fn json_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn send(
        router: &Router,
        method: Method,
        uri: &str,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut builder = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match body {
            Some(body) => builder
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        json_of(router.clone().oneshot(request).await.unwrap()).await
    }

    async fn create(router: &Router, filename: &str, bytes: u64) -> (StatusCode, Value) {
        send(
            router,
            Method::POST,
            "/v1/uploads",
            Some(json!({
                "filename": filename,
                "purpose": "batch",
                "bytes": bytes,
                "mime_type": "application/jsonl",
            })),
            &[],
        )
        .await
    }

    async fn create_id(router: &Router, bytes: u64) -> String {
        let (status, upload) = create(router, "rows.jsonl", bytes).await;
        assert_eq!(status, StatusCode::OK, "create failed: {upload}");
        upload["id"].as_str().unwrap().to_owned()
    }

    /// A multipart part with an extra text field in front of `data`, the way a client library that
    /// adds a field this surface does not use would send one.
    async fn send_part(router: &Router, id: &str, data: &[u8]) -> (StatusCode, Value) {
        let mut body = Vec::new();
        body.extend_from_slice(
            b"--test-boundary\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nbatch\r\n",
        );
        body.extend_from_slice(
            b"--test-boundary\r\nContent-Disposition: form-data; name=\"data\"; filename=\"part.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n",
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n--test-boundary--\r\n");
        let request = Request::builder()
            .method(Method::POST)
            .uri(format!("/v1/uploads/{id}/parts"))
            .header(
                header::CONTENT_TYPE,
                "multipart/form-data; boundary=test-boundary",
            )
            .body(Body::from(body))
            .unwrap();
        json_of(router.clone().oneshot(request).await.unwrap()).await
    }

    async fn complete(router: &Router, id: &str, body: Value) -> (StatusCode, Value) {
        send(
            router,
            Method::POST,
            &format!("/v1/uploads/{id}/complete"),
            Some(body),
            &[],
        )
        .await
    }

    async fn cancel(router: &Router, id: &str) -> (StatusCode, Value) {
        send(
            router,
            Method::POST,
            &format!("/v1/uploads/{id}/cancel"),
            None,
            &[],
        )
        .await
    }

    fn file_bytes(app: &AppState, file: &Value) -> Vec<u8> {
        let id = file["id"].as_str().unwrap();
        std::fs::read(app.batch.root().join("files").join(format!("{id}.jsonl"))).unwrap()
    }

    /// The on-disk record of a test upload, with the error path reduced to a panic: a test that
    /// cannot read its own record has already failed.
    async fn stored(id: &str) -> UploadRecord {
        load_record(id)
            .await
            .ok()
            .flatten()
            .expect("the upload record is on disk")
    }

    #[test]
    fn ids_are_checked_before_they_become_paths() {
        assert!(valid_id("upload_0199c0de-0000-7000-8000-000000000000"));
        assert!(valid_id("a.b-c_1"));
        assert!(valid_id(&"a".repeat(128)));
        assert!(!valid_id(""));
        assert!(!valid_id("."));
        assert!(!valid_id(".."));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a\\b"));
        assert!(!valid_id("a b"));
        assert!(!valid_id("a\0b"));
        assert!(!valid_id(&"a".repeat(129)));
    }

    #[tokio::test]
    async fn uploads_create_two_parts_and_complete_into_the_summed_file() {
        let app = open_app();
        let router = api(app.clone());
        let (status, upload) = create(&router, "rows.jsonl", 8).await;
        assert_eq!(status, StatusCode::OK, "{upload}");
        assert_eq!(upload["object"], "upload");
        assert_eq!(upload["status"], "pending");
        assert_eq!(upload["bytes"], 8);
        assert_eq!(upload["filename"], "rows.jsonl");
        assert_eq!(upload["purpose"], "batch");
        assert_eq!(
            upload["expires_at"].as_u64().unwrap() - upload["created_at"].as_u64().unwrap(),
            UPLOAD_TTL_SECONDS
        );
        assert!(upload.get("mime_type").is_none());
        let id = upload["id"].as_str().unwrap().to_owned();

        let (status, first) = send_part(&router, &id, b"abc").await;
        assert_eq!(status, StatusCode::OK, "{first}");
        assert_eq!(first["object"], "upload.part");
        assert_eq!(first["upload_id"], id);
        assert!(first["id"].as_str().unwrap().starts_with("part_"));
        assert!(dir_for(&id).join("part-000000000000.bin").exists());

        let (status, second) = send_part(&router, &id, b"defgh").await;
        assert_eq!(status, StatusCode::OK, "{second}");
        assert!(dir_for(&id).join("part-000000000001.bin").exists());

        let (status, file) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::OK, "complete failed: {file}");
        assert_eq!(file["object"], "file");
        assert_eq!(file["bytes"], 8);
        assert_eq!(file["filename"], "rows.jsonl");
        assert_eq!(file["purpose"], "batch");
        let mut keys: Vec<&str> = file
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["bytes", "created_at", "filename", "id", "object", "purpose"]
        );
        assert_eq!(file_bytes(&app, &file), b"abcdefgh");

        let record = stored(&id).await;
        assert_eq!(record.status, UploadStatus::Completed);
        assert!(!part_path(&id, 0).exists());
        assert!(!part_path(&id, 1).exists());
        assert!(record_path(&id).exists());
    }

    #[tokio::test]
    async fn uploads_accept_a_part_above_the_extractor_body_limit() {
        let app = open_app();
        let router = api(app.clone());
        let size = 3 * 1024 * 1024;
        let id = create_id(&router, size as u64).await;
        let data = vec![b'a'; size];
        let (status, body) = send_part(&router, &id, &data).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, file) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{file}");
        assert_eq!(file["bytes"], size as u64);
        assert_eq!(file_bytes(&app, &file).len(), size);
    }

    #[tokio::test]
    async fn uploads_explicit_part_ids_assemble_in_the_given_order() {
        let app = open_app();
        let router = api(app.clone());
        let id = create_id(&router, 6).await;
        let (status, first) = send_part(&router, &id, b"AAA").await;
        assert_eq!(status, StatusCode::OK, "{first}");
        let (status, second) = send_part(&router, &id, b"BBB").await;
        assert_eq!(status, StatusCode::OK, "{second}");

        let (status, refused) =
            complete(&router, &id, json!({"part_ids": ["part_does-not-exist"]})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            refused["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not belong"),
            "{refused}"
        );

        let (status, file) = complete(
            &router,
            &id,
            json!({"part_ids": [second["id"], first["id"]]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "complete failed: {file}");
        assert_eq!(file["bytes"], 6);
        assert_eq!(file_bytes(&app, &file), b"BBBAAA");
    }

    #[tokio::test]
    async fn uploads_cancel_deletes_the_parts_and_freezes_the_upload() {
        let app = open_app();
        let router = api(app);
        let id = create_id(&router, 4).await;
        let (status, _) = send_part(&router, &id, b"abcd").await;
        assert_eq!(status, StatusCode::OK);
        assert!(part_path(&id, 0).exists());

        let (status, cancelled) = cancel(&router, &id).await;
        assert_eq!(status, StatusCode::OK, "{cancelled}");
        assert_eq!(cancelled["status"], "cancelled");
        assert!(!part_path(&id, 0).exists());
        assert!(record_path(&id).exists());
        assert_eq!(stored(&id).await.status, UploadStatus::Cancelled);

        let (status, refused) = send_part(&router, &id, b"more").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cancelled"));
        let (status, _) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Cancelling what is already cancelled asks for a state it is already in.
        let (status, again) = cancel(&router, &id).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(again["status"], "cancelled");
    }

    #[tokio::test]
    async fn uploads_unknown_and_traversing_ids_are_404s() {
        let app = open_app();
        let router = api(app);
        for id in [
            "upload_00000000-0000-0000-0000-000000000000",
            "..",
            "%2e%2e%2fescape",
        ] {
            let (status, body) = send_part(&router, id, b"x").await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{id}: {body}");
            assert!(body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("no upload with id"));
            let (status, _) = complete(&router, id, json!({})).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
            let (status, _) = cancel(&router, id).await;
            assert_eq!(status, StatusCode::NOT_FOUND);
        }
        assert!(!uploads_root().join("escape").exists());
    }

    #[tokio::test]
    async fn uploads_expire_and_their_parts_go_with_them() {
        let app = open_app();
        let router = api(app);
        let id = create_id(&router, 4).await;
        let (status, _) = send_part(&router, &id, b"ab").await;
        assert_eq!(status, StatusCode::OK);
        assert!(part_path(&id, 0).exists());

        // Backdate the record on disk: the same state a day of wall-clock time would produce.
        let mut record = stored(&id).await;
        record.created_at = now_seconds() - UPLOAD_TTL_SECONDS - 1;
        record.expires_at = now_seconds() - 1;
        write_record(&record).await.unwrap();

        let (status, body) = send_part(&router, &id, b"c").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("expired"), "{message}");
        assert!(message.contains("start a new upload"), "{message}");
        assert!(!dir_for(&id).exists(), "the expired directory is gone");

        let (status, _) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn uploads_refuse_a_part_beyond_the_declared_bytes() {
        let app = open_app();
        let router = api(app);
        let id = create_id(&router, 3).await;
        let (status, body) = send_part(&router, &id, b"abcd").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains('3'), "{message}");
        assert!(message.contains('4'), "{message}");
        assert!(!part_path(&id, 0).exists());
        assert!(stored(&id).await.parts.is_empty());
    }

    #[tokio::test]
    async fn uploads_refuse_a_declaration_over_the_cap() {
        let app = open_app();
        let router = api(app);
        let (status, body) = create(&router, "rows.jsonl", MAX_UPLOAD_BYTES as u64 + 1).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains(&MAX_UPLOAD_BYTES.to_string()), "{message}");
    }

    #[tokio::test]
    async fn uploads_refuse_a_part_to_a_completed_upload_and_a_second_complete() {
        let app = open_app();
        let router = api(app);
        let id = create_id(&router, 1).await;
        let (status, _) = send_part(&router, &id, b"a").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = send_part(&router, &id, b"b").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("complete"));
        let (status, body) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already complete"));
        let (status, body) = cancel(&router, &id).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("cannot be cancelled"));
    }

    #[tokio::test]
    async fn uploads_are_refused_without_a_client_key() {
        let app = keyed_app();
        let router = api(app);
        let body = Some(json!({"filename": "rows.jsonl", "purpose": "batch", "bytes": 1}));
        let (status, _) = send(&router, Method::POST, "/v1/uploads", body.clone(), &[]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = send(
            &router,
            Method::POST,
            "/v1/uploads",
            body.clone(),
            &[("authorization", "Bearer not-the-key")],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, upload) = send(
            &router,
            Method::POST,
            "/v1/uploads",
            body,
            &[("authorization", "Bearer mr-test-key")],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{upload}");
    }

    #[tokio::test]
    async fn uploads_survive_a_restart_of_the_relay() {
        let first = open_app();
        let id = {
            let router = api(first);
            let id = create_id(&router, 3).await;
            let (status, _) = send_part(&router, &id, b"ab").await;
            assert_eq!(status, StatusCode::OK);
            id
        };

        // A new process: new state, same state directory. The upload is entirely on disk, so the
        // second instance picks it up without any in-memory handoff.
        let second = open_app();
        let router = api(second.clone());
        let (status, part) = send_part(&router, &id, b"c").await;
        assert_eq!(status, StatusCode::OK, "{part}");
        let (status, file) = complete(&router, &id, json!({})).await;
        assert_eq!(status, StatusCode::OK, "{file}");
        assert_eq!(file["bytes"], 3);
        assert_eq!(file_bytes(&second, &file), b"abc");
    }
}
