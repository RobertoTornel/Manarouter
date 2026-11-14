//! The vector store surface: OpenAI's `/v1/vector_stores` routes, served locally.
//!
//! A vector store is a small, durable index owned by this relay. Files are attached from the batch
//! lane's file store, chunked as **text**, embedded through the relay's own `/v1/embeddings` route,
//! and ranked by cosine similarity. Nothing here proxies to a vendor: the store, the chunks and the
//! search are ours, and the only outbound call is the embedding request the relay would have made
//! anyway.
//!
//! Four decisions are worth stating plainly because they are visible to a client:
//!
//! * **Chunking is character-based, not token-based.** The chunker targets
//!   [`CHUNK_TARGET`] characters with [`CHUNK_OVERLAP`] characters of overlap. The
//!   `chunking_strategy` accepted on create is validated but does not change the chunker, and the
//!   `chunking_strategy` returned on an attached file reports the static character strategy that
//!   actually ran. Pretending a token strategy ran would make the response a lie.
//! * **Scores are cosine similarity in `[-1, 1]`.** OpenAI's vector-store scores use a different
//!   scale. They are not rescaled here, because a rescale would be a guess dressed as a number.
//! * **`filters` are refused when non-empty.** No per-chunk attributes are stored, so a filtered
//!   search would silently return the wrong set rather than an error.
//! * **An embedding model is required to attach text or to search with text.** It is resolved from
//!   the store's own `embedding_model` extension, then `MANAROUTER_EMBEDDING_MODEL`, then the first
//!   provider in the routing snapshot that declares [`Capability::Embeddings`]. With none of those,
//!   the store still exists and still lists; attaching and text search answer `409`.
//!
//! The store's own `embedding_model` field is a local extension: OpenAI has no such field. It is
//! accepted on create and update so an operator can pin a store to one embedder without setting a
//! process-wide variable. Changing it after files are attached does **not** re-embed the existing
//! chunks; a query embedded with a different model may simply not match them.
//!
//! Storage is one directory per store under `state_dir()/vector_stores/<id>/`: `store.json` for the
//! record (written atomically with a temporary file and a rename) and `chunks.jsonl` for the chunks
//! (appended on attach, rewritten atomically on detach). The JSONL is read into memory when the
//! store is opened.
//!
//! Attaching is bounded by [`MAX_CHUNKS`] chunks and [`MAX_TEXT_BYTES`] bytes of chunk text. That
//! bound is a denial-of-service bound, not a product limit: without it one client could make the
//! relay hold an unbounded index in memory. Exceeding it is a `413` that names the bound.
//!
//! This module also owns `POST /v1/search`, which has two shapes. A body carrying `model` is a
//! provider search: its bytes are handed to the relay's ordinary dispatch, so the provider — and
//! its declared capabilities — answers, and a provider that does not declare search yields the
//! relay's `404 unsupported_endpoint`. A body carrying `vector_store_id` (one store) or
//! `vector_store_ids` (several, merged by score) is local retrieval. If both `model` and a store
//! key are present, `model` wins: the client asked the provider, and a local search would answer a
//! different question. A merged local search requires every named store to resolve to the same
//! embedding model, because cosine scores from different embedding spaces are not comparable.
//! Every search result, on this route and on the scoped one, carries `vector_store_id` so a merged
//! page says which store each hit came from.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body as AxumBody, Bytes};
use axum::extract::{Path as RoutePath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use mr_core::dialect::Capability;

use crate::batch::Refusal;
use crate::server::AppState;

// --- Limits ---------------------------------------------------------------------------------

/// The most chunks one store may hold. A denial-of-service bound, not a product limit.
const MAX_CHUNKS: usize = 200_000;
/// The most bytes of chunk text one store may hold. A denial-of-service bound, not a product limit.
const MAX_TEXT_BYTES: usize = 100 * 1024 * 1024;
/// Target chunk size, in characters.
const CHUNK_TARGET: usize = 800;
/// Overlap between adjacent chunks, in characters.
const CHUNK_OVERLAP: usize = 100;
/// How many chunks are sent to the embedder in one request.
const EMBED_BATCH: usize = 64;
/// The largest JSON body these routes accept.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The largest embedding response these routes will read.
const EMBED_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
/// The largest `max_num_results` a search may ask for.
const MAX_SEARCH_RESULTS: u32 = 50;
/// The default `max_num_results`.
const DEFAULT_SEARCH_RESULTS: usize = 10;
/// The default page size for listings.
const DEFAULT_LIMIT: usize = 20;
/// The largest page size a listing may ask for.
const MAX_LIMIT: usize = 100;
/// The most files one create or one file batch may attach.
const MAX_BATCH_FILES: usize = 500;
/// The most stores one top-level local search may merge.
const MAX_SEARCH_STORES: usize = 100;
/// The longest store name, in characters.
const MAX_NAME_CHARS: usize = 256;
/// The most metadata pairs a store may carry.
const MAX_METADATA_PAIRS: usize = 16;
/// The longest metadata key, in characters.
const MAX_METADATA_KEY_CHARS: usize = 64;
/// The longest metadata string value, in characters.
const MAX_METADATA_VALUE_CHARS: usize = 512;
/// The longest embedding model name, in characters.
const MAX_MODEL_CHARS: usize = 256;

const STORE_FILE: &str = "store.json";
const CHUNKS_FILE: &str = "chunks.jsonl";

// --- Routes ---------------------------------------------------------------------------------

/// The vector store routes, for the server to merge into its router.
///
/// Every handler authenticates first through [`crate::server::authorize_batch_request`], so a
/// vector store request is refused with the same `401` envelope as the rest of the batch surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/vector_stores", get(list_stores).post(create_store))
        .route(
            "/v1/vector_stores/{id}",
            get(retrieve_store).post(update_store).delete(delete_store),
        )
        .route(
            "/v1/vector_stores/{id}/files",
            get(list_files).post(attach_file),
        )
        .route(
            "/v1/vector_stores/{id}/files/{file_id}",
            get(retrieve_file).delete(delete_file),
        )
        .route(
            "/v1/vector_stores/{id}/file_batches",
            post(create_file_batch),
        )
        .route(
            "/v1/vector_stores/{id}/file_batches/{batch_id}",
            get(retrieve_batch),
        )
        .route("/v1/vector_stores/{id}/search", post(search))
        .route("/v1/search", post(search_anywhere))
}

// --- Stored shapes --------------------------------------------------------------------------

/// The record persisted as `store.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoreRecord {
    id: String,
    created_at: u64,
    #[serde(default)]
    last_active_at: u64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    metadata: Map<String, Value>,
    #[serde(default)]
    expires_after: Option<Value>,
    /// A local extension: the embedding model this store is pinned to, if any.
    #[serde(default)]
    embedding_model: Option<String>,
    #[serde(default)]
    files: Vec<FileRecord>,
    #[serde(default)]
    batches: Vec<BatchRecord>,
    #[serde(default)]
    usage_bytes: u64,
}

/// One attached file, as recorded in `store.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileRecord {
    file_id: String,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    usage_bytes: u64,
    #[serde(default)]
    chunks: usize,
}

/// One synchronous file batch, as recorded in `store.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BatchRecord {
    id: String,
    #[serde(default)]
    created_at: u64,
    #[serde(default)]
    file_counts: FileCounts,
}

/// The `file_counts` object OpenAI returns on stores and batches.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
struct FileCounts {
    in_progress: u64,
    completed: u64,
    failed: u64,
    cancelled: u64,
    total: u64,
}

/// One chunk, as persisted in `chunks.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Chunk {
    file_id: String,
    #[serde(default)]
    chunk_index: u32,
    text: String,
    vector: Vec<f32>,
}

/// The in-memory state of one store.
#[derive(Clone)]
struct StoreState {
    record: StoreRecord,
    chunks: Vec<Chunk>,
}

/// One store, with its own lock.
///
/// The lock is per store rather than global so that attaching to one store does not block a search
/// in another. It is never held across an embedding call: embedding is a network round trip, and
/// holding a lock across it would serialize every other request for that store behind it.
struct Store {
    id: String,
    dir: PathBuf,
    deleted: AtomicBool,
    state: Mutex<StoreState>,
}

impl Store {
    fn store_path(&self) -> PathBuf {
        self.dir.join(STORE_FILE)
    }

    fn chunks_path(&self) -> PathBuf {
        self.dir.join(CHUNKS_FILE)
    }

    /// The store as a client sees it, or a refusal when it has expired.
    fn view(&self) -> Result<Value, Refusal> {
        let state = lock(&self.state);
        if self.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
            return Err(Refusal::not_found("vector store", &self.id));
        }
        Ok(store_json(&state.record))
    }
}

/// Every store under one root, loaded lazily.
struct Registry {
    root: PathBuf,
    stores: Mutex<HashMap<String, Arc<Store>>>,
}

impl Registry {
    fn load(root: &Path) -> Self {
        let registry = Self {
            root: root.to_path_buf(),
            stores: Mutex::new(HashMap::new()),
        };
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return registry,
            Err(error) => {
                tracing::warn!(
                    root = %root.display(),
                    %error,
                    "could not read the vector store directory"
                );
                return registry;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(id) = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|id| is_store_id(id))
                .map(str::to_owned)
            else {
                continue;
            };
            match load_store(&id, path) {
                Ok(store) => {
                    lock(&registry.stores).insert(id, Arc::new(store));
                }
                Err(error) => {
                    tracing::warn!(store = %id, %error, "skipping an unreadable vector store")
                }
            }
        }
        registry
    }
}

/// One registry per root, so a process has one in-memory index per state directory.
static REGISTRIES: OnceLock<Mutex<HashMap<PathBuf, Arc<Registry>>>> = OnceLock::new();

fn registry() -> Arc<Registry> {
    let root = stores_root();
    let registries = REGISTRIES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registries = lock(registries);
    if let Some(registry) = registries.get(&root) {
        return Arc::clone(registry);
    }
    let registry = Arc::new(Registry::load(&root));
    registries.insert(root, Arc::clone(&registry));
    registry
}

fn stores_root() -> PathBuf {
    #[cfg(test)]
    if let Some(root) = TEST_ROOT.with(|slot| slot.borrow().clone()) {
        return root;
    }
    mr_core::config::state_dir().join("vector_stores")
}

fn open_store(registry: &Registry, id: &str) -> Result<Arc<Store>, Refusal> {
    if !is_store_id(id) {
        return Err(Refusal::not_found("vector store", id));
    }
    let store = lock(&registry.stores).get(id).cloned();
    let store = store.ok_or_else(|| Refusal::not_found("vector store", id))?;
    if store.deleted.load(Ordering::SeqCst) {
        return Err(Refusal::not_found("vector store", id));
    }
    let expired = {
        let state = lock(&store.state);
        is_expired(&state.record, now())
    };
    if expired {
        return Err(Refusal::not_found("vector store", id));
    }
    Ok(store)
}

fn open(id: &str) -> Result<Arc<Store>, Refusal> {
    let registry = registry();
    open_store(&registry, id)
}

/// A snapshot of what an attach needs to know before it embeds, taken under the lock and then
/// released. The re-check after embedding is what makes a concurrent detach or delete safe.
struct AttachState {
    ids: HashSet<String>,
    chunks: usize,
    bytes: usize,
    embedding_model: Option<String>,
}

fn snapshot_state(store: &Store) -> Result<AttachState, Refusal> {
    let state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", &store.id));
    }
    let ids = state
        .record
        .files
        .iter()
        .map(|file| file.file_id.clone())
        .collect();
    let bytes = state
        .record
        .files
        .iter()
        .fold(0u64, |total, file| total.saturating_add(file.usage_bytes));
    Ok(AttachState {
        ids,
        chunks: state.chunks.len(),
        bytes: usize::try_from(bytes).unwrap_or(usize::MAX),
        embedding_model: state.record.embedding_model.clone(),
    })
}

// --- HTTP handlers --------------------------------------------------------------------------

async fn create_store(State(app): State<AppState>, headers: HeaderMap, body: AxumBody) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match create_store_inner(&app, body).await {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn list_stores(
    State(app): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<ListParams>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match list_stores_inner(&params) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn retrieve_store(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match retrieve_store_inner(&id) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn update_store(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    body: AxumBody,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match update_store_inner(&id, body).await {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn delete_store(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match delete_store_inner(&id).await {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn list_files(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    Query(params): Query<FileListParams>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match list_files_inner(&id, &params) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn attach_file(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    body: AxumBody,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match attach_file_inner(&app, &id, body).await {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn retrieve_file(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, file_id)): RoutePath<(String, String)>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match retrieve_file_inner(&id, &file_id) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn delete_file(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, file_id)): RoutePath<(String, String)>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match detach_file_inner(&id, &file_id) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn create_file_batch(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    body: AxumBody,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match create_file_batch_inner(&app, &id, body).await {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn retrieve_batch(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, batch_id)): RoutePath<(String, String)>,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match retrieve_batch_inner(&id, &batch_id) {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

async fn search(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    body: AxumBody,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match search_inner(&app, &id, body).await {
        Ok(value) => Json(value).into_response(),
        Err(refusal) => refusal.into_response(),
    }
}

/// `POST /v1/search`: a provider search when the body names a model, local retrieval otherwise.
///
/// The two branches are decided from the body alone, before anything is looked up:
///
/// * a body with `model` is handed to [`crate::server::dispatch_internal`] byte for byte, so the
///   relay's own capability gate (and its `404 unsupported_endpoint` when no step's provider
///   declares the `search` capability) answers exactly as it would for a socket request;
/// * a body without `model` is local retrieval over the stores this relay holds, and must name
///   `vector_store_id` or `vector_store_ids`.
///
/// `model` wins when both are present. A request that names both is a client asking the provider;
/// silently searching locally instead would answer a different question. There is no second
/// handler for this path: the relay's router mounts this one.
async fn search_anywhere(
    State(app): State<AppState>,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    if let Some(denied) = authorize(&app, &headers) {
        return denied;
    }
    match search_anywhere_inner(&app, body).await {
        Ok(response) => response,
        Err(refusal) => refusal.into_response(),
    }
}

// --- Handler bodies -------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateRequest {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    metadata: Option<Value>,
    #[serde(default)]
    expires_after: Option<Value>,
    #[serde(default)]
    chunking_strategy: Option<Value>,
    #[serde(default)]
    file_ids: Option<Vec<String>>,
    /// A local extension: pin this store to one embedding model.
    #[serde(default)]
    embedding_model: Option<String>,
}

async fn create_store_inner(app: &AppState, body: AxumBody) -> Result<Value, Refusal> {
    let value = read_json(body, MAX_BODY_BYTES).await?;
    let request: CreateRequest = parse_body(value)?;

    let name = match request.name {
        Some(name) => {
            validate_name(&name)?;
            Some(name)
        }
        None => None,
    };
    let metadata = match request.metadata {
        Some(metadata) => metadata_object(&metadata)?,
        None => Map::new(),
    };
    let expires_after = match request.expires_after {
        Some(expires) => parse_expires(&expires)?,
        None => None,
    };
    let embedding_model = match request.embedding_model {
        Some(model) => Some(validate_model_name(&model)?),
        None => None,
    };
    if let Some(strategy) = request.chunking_strategy.as_ref() {
        validate_chunking_strategy(strategy)?;
    }
    let file_ids = request.file_ids.unwrap_or_default();
    if file_ids.len() > MAX_BATCH_FILES {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("file_ids may name at most {MAX_BATCH_FILES} files"),
        ));
    }

    let prepared = if file_ids.is_empty() {
        Vec::new()
    } else {
        prepare_attachments(
            app,
            embedding_model.as_deref(),
            &file_ids,
            &HashSet::new(),
            0,
            0,
        )
        .await?
    };

    let id = new_store_id();
    let created_at = now();
    let mut record = StoreRecord {
        id: id.clone(),
        created_at,
        last_active_at: created_at,
        name,
        metadata,
        expires_after,
        embedding_model,
        files: Vec::new(),
        batches: Vec::new(),
        usage_bytes: 0,
    };
    let mut chunks = Vec::new();
    for file in &prepared {
        let usage_bytes: u64 = file.pieces.iter().map(|piece| piece.len() as u64).sum();
        record.files.push(FileRecord {
            file_id: file.file_id.clone(),
            created_at,
            filename: file.filename.clone(),
            usage_bytes,
            chunks: file.pieces.len(),
        });
        record.usage_bytes = record.usage_bytes.saturating_add(usage_bytes);
        for (index, (text, vector)) in file.pieces.iter().zip(&file.vectors).enumerate() {
            chunks.push(Chunk {
                file_id: file.file_id.clone(),
                chunk_index: index as u32,
                text: text.clone(),
                vector: vector.clone(),
            });
        }
    }

    let registry = registry();
    let dir = registry.root.join(&id);
    write_new_store(&dir, &record, &chunks)?;
    let store = Arc::new(Store {
        id: id.clone(),
        dir,
        deleted: AtomicBool::new(false),
        state: Mutex::new(StoreState { record, chunks }),
    });
    lock(&registry.stores).insert(id, Arc::clone(&store));
    let view = {
        let state = lock(&store.state);
        store_json(&state.record)
    };
    Ok(view)
}

#[derive(Deserialize)]
struct ListParams {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    after: Option<String>,
}

fn list_stores_inner(params: &ListParams) -> Result<Value, Refusal> {
    let limit = parse_limit(params.limit.as_deref())?;
    let registry = registry();
    let mut stores: Vec<(u64, String, Arc<Store>)> = lock(&registry.stores)
        .values()
        .map(|store| {
            let created_at = lock(&store.state).record.created_at;
            (created_at, store.id.clone(), Arc::clone(store))
        })
        .collect();
    stores.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));

    let mut views = Vec::new();
    for (_, _, store) in stores {
        if store.deleted.load(Ordering::SeqCst) {
            continue;
        }
        if let Ok(view) = store.view() {
            views.push(view);
        }
    }

    let start = match params.after.as_deref() {
        None => 0,
        Some(cursor) => {
            if !is_store_id(cursor) {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!("after \"{cursor}\" is not a vector store id"),
                ));
            }
            views
                .iter()
                .position(|view| view.get("id").and_then(Value::as_str) == Some(cursor))
                .map(|index| index + 1)
                .ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        format!("after \"{cursor}\" names no vector store"),
                    )
                })?
        }
    };
    let page: Vec<Value> = views.iter().skip(start).take(limit).cloned().collect();
    let has_more = views.len() > start + page.len();
    let first_id = page
        .first()
        .and_then(|view| view.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let last_id = page
        .last()
        .and_then(|view| view.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "object": "list",
        "data": page,
        "first_id": first_id,
        "last_id": last_id,
        "has_more": has_more,
    }))
}

fn retrieve_store_inner(id: &str) -> Result<Value, Refusal> {
    let store = open(id)?;
    store.view()
}

async fn update_store_inner(id: &str, body: AxumBody) -> Result<Value, Refusal> {
    let value = read_json(body, MAX_BODY_BYTES).await?;
    let object = value.as_object().ok_or_else(|| {
        Refusal::bad_request(
            "invalid_request_error",
            "the request body must be a JSON object",
        )
    })?;
    let store = open(id)?;
    let mut state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    let previous = state.record.clone();

    if let Some(name) = object.get("name") {
        if name.is_null() {
            state.record.name = None;
        } else {
            let name = name.as_str().ok_or_else(|| {
                Refusal::bad_request("invalid_request_error", "name must be a string or null")
            })?;
            validate_name(name)?;
            state.record.name = Some(name.to_owned());
        }
    }
    if let Some(metadata) = object.get("metadata") {
        state.record.metadata = if metadata.is_null() {
            Map::new()
        } else {
            metadata_object(metadata)?
        };
    }
    if let Some(expires) = object.get("expires_after") {
        state.record.expires_after = parse_expires(expires)?;
    }
    if let Some(model) = object.get("embedding_model") {
        state.record.embedding_model = if model.is_null() {
            None
        } else {
            let model = model.as_str().ok_or_else(|| {
                Refusal::bad_request(
                    "invalid_request_error",
                    "embedding_model must be a string or null",
                )
            })?;
            Some(validate_model_name(model)?)
        };
    }
    state.record.last_active_at = now();

    let record_bytes = match serde_json::to_vec(&state.record) {
        Ok(bytes) => bytes,
        Err(error) => {
            state.record = previous;
            return Err(Refusal::internal(format!(
                "cannot encode the vector store: {error}"
            )));
        }
    };
    if let Err(error) = write_atomic(&store.store_path(), &record_bytes) {
        state.record = previous;
        return Err(Refusal::internal(format!(
            "cannot write the vector store: {error}"
        )));
    }
    Ok(store_json(&state.record))
}

async fn delete_store_inner(id: &str) -> Result<Value, Refusal> {
    let registry = registry();
    let store = open_store(&registry, id)?;
    store.deleted.store(true, Ordering::SeqCst);
    lock(&registry.stores).remove(id);
    let dir = store.dir.clone();
    match tokio::task::spawn_blocking(move || fs::remove_dir_all(&dir)).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(Err(error)) => {
            tracing::warn!(store = %id, %error, "could not remove the vector store's directory");
        }
        Err(error) => {
            tracing::warn!(store = %id, %error, "could not join the vector store removal task");
        }
    }
    Ok(json!({
        "id": id,
        "object": "vector_store.deleted",
        "deleted": true,
    }))
}

#[derive(Deserialize)]
struct AttachRequest {
    file_id: String,
}

async fn attach_file_inner(app: &AppState, id: &str, body: AxumBody) -> Result<Value, Refusal> {
    let value = read_json(body, MAX_BODY_BYTES).await?;
    let request: AttachRequest = parse_body(value)?;
    let store = open(id)?;
    let snapshot = snapshot_state(&store)?;
    let file_ids = std::slice::from_ref(&request.file_id);
    let prepared = prepare_attachments(
        app,
        snapshot.embedding_model.as_deref(),
        file_ids,
        &snapshot.ids,
        snapshot.chunks,
        snapshot.bytes,
    )
    .await?;

    let mut state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    if state
        .record
        .files
        .iter()
        .any(|file| file.file_id == request.file_id)
    {
        return Err(already_attached(id, &request.file_id));
    }
    append_prepared(&store, &mut state, prepared, None)?;
    let file = state
        .record
        .files
        .iter()
        .find(|file| file.file_id == request.file_id)
        .ok_or_else(|| Refusal::internal("the attached file was not recorded"))?;
    Ok(file_json(&state.record, file))
}

#[derive(Deserialize)]
struct FileListParams {
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    filter: Option<String>,
}

fn list_files_inner(id: &str, params: &FileListParams) -> Result<Value, Refusal> {
    let limit = parse_limit(params.limit.as_deref())?;
    if let Some(filter) = params.filter.as_deref() {
        if !matches!(filter, "completed" | "in_progress" | "failed" | "cancelled") {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "filter \"{filter}\" is not one of completed, in_progress, failed or cancelled"
                ),
            ));
        }
    }
    let store = open(id)?;
    let state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    let mut files: Vec<&FileRecord> = state
        .record
        .files
        .iter()
        .filter(|_| {
            params
                .filter
                .as_deref()
                .is_none_or(|filter| filter == "completed")
        })
        .collect();
    files.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.file_id.cmp(&a.file_id))
    });
    let start = match params.after.as_deref() {
        None => 0,
        Some(cursor) => {
            if !is_file_id(cursor) {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!("after \"{cursor}\" is not a file id"),
                ));
            }
            files
                .iter()
                .position(|file| file.file_id == cursor)
                .map(|index| index + 1)
                .ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        format!("after \"{cursor}\" names no attached file"),
                    )
                })?
        }
    };
    let page: Vec<Value> = files
        .iter()
        .skip(start)
        .take(limit)
        .map(|file| file_json(&state.record, file))
        .collect();
    let has_more = files.len() > start + page.len();
    let first_id = page
        .first()
        .and_then(|view| view.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    let last_id = page
        .last()
        .and_then(|view| view.get("id"))
        .cloned()
        .unwrap_or(Value::Null);
    Ok(json!({
        "object": "list",
        "data": page,
        "first_id": first_id,
        "last_id": last_id,
        "has_more": has_more,
    }))
}

fn retrieve_file_inner(id: &str, file_id: &str) -> Result<Value, Refusal> {
    let store = open(id)?;
    let state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    let file = state
        .record
        .files
        .iter()
        .find(|file| file.file_id == file_id)
        .ok_or_else(|| Refusal::not_found("vector store file", file_id))?;
    Ok(file_json(&state.record, file))
}

fn detach_file_inner(id: &str, file_id: &str) -> Result<Value, Refusal> {
    if !is_file_id(file_id) {
        return Err(Refusal::not_found("vector store file", file_id));
    }
    let store = open(id)?;
    let mut state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    let index = state
        .record
        .files
        .iter()
        .position(|file| file.file_id == file_id)
        .ok_or_else(|| Refusal::not_found("vector store file", file_id))?;
    let removed_file = state.record.files.remove(index);
    let removed_bytes = removed_file.usage_bytes;
    let removed_chunks: Vec<Chunk> = state
        .chunks
        .iter()
        .filter(|chunk| chunk.file_id == file_id)
        .cloned()
        .collect();
    state.chunks.retain(|chunk| chunk.file_id != file_id);
    state.record.usage_bytes = state.record.usage_bytes.saturating_sub(removed_bytes);
    state.record.last_active_at = now();

    let record_bytes = match serde_json::to_vec(&state.record) {
        Ok(bytes) => bytes,
        Err(error) => {
            restore_detached(
                &mut state,
                index,
                removed_file,
                removed_bytes,
                removed_chunks,
            );
            return Err(Refusal::internal(format!(
                "cannot encode the vector store: {error}"
            )));
        }
    };
    if let Err(error) = write_chunks_atomic(&store.chunks_path(), &state.chunks) {
        restore_detached(
            &mut state,
            index,
            removed_file,
            removed_bytes,
            removed_chunks,
        );
        return Err(Refusal::internal(format!(
            "cannot write the vector store's chunks: {error}"
        )));
    }
    if let Err(error) = write_atomic(&store.store_path(), &record_bytes) {
        restore_detached(
            &mut state,
            index,
            removed_file,
            removed_bytes,
            removed_chunks,
        );
        let _ = write_chunks_atomic(&store.chunks_path(), &state.chunks);
        return Err(Refusal::internal(format!(
            "cannot write the vector store: {error}"
        )));
    }
    Ok(json!({
        "id": file_id,
        "object": "vector_store.file.deleted",
        "deleted": true,
    }))
}

fn restore_detached(
    state: &mut StoreState,
    index: usize,
    file: FileRecord,
    usage_bytes: u64,
    chunks: Vec<Chunk>,
) {
    state.record.files.insert(index, file);
    state.record.usage_bytes = state.record.usage_bytes.saturating_add(usage_bytes);
    state.chunks.extend(chunks);
}

#[derive(Deserialize)]
struct BatchRequest {
    file_ids: Vec<String>,
}

async fn create_file_batch_inner(
    app: &AppState,
    id: &str,
    body: AxumBody,
) -> Result<Value, Refusal> {
    let value = read_json(body, MAX_BODY_BYTES).await?;
    let request: BatchRequest = parse_body(value)?;
    if request.file_ids.is_empty() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "file_ids must name at least one file",
        ));
    }
    if request.file_ids.len() > MAX_BATCH_FILES {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("file_ids may name at most {MAX_BATCH_FILES} files"),
        ));
    }
    let store = open(id)?;
    let snapshot = snapshot_state(&store)?;
    let prepared = prepare_attachments(
        app,
        snapshot.embedding_model.as_deref(),
        &request.file_ids,
        &snapshot.ids,
        snapshot.chunks,
        snapshot.bytes,
    )
    .await?;

    let mut state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    for file_id in &request.file_ids {
        if state
            .record
            .files
            .iter()
            .any(|file| &file.file_id == file_id)
        {
            return Err(already_attached(id, file_id));
        }
    }
    let batch_id = new_batch_id();
    let counts = FileCounts {
        completed: prepared.len() as u64,
        total: prepared.len() as u64,
        ..FileCounts::default()
    };
    let batch = BatchRecord {
        id: batch_id.clone(),
        created_at: now(),
        file_counts: counts,
    };
    append_prepared(&store, &mut state, prepared, Some(batch))?;
    let batch = state
        .record
        .batches
        .iter()
        .find(|batch| batch.id == batch_id)
        .ok_or_else(|| Refusal::internal("the file batch was not recorded"))?;
    Ok(batch_json(&state.record, batch))
}

fn retrieve_batch_inner(id: &str, batch_id: &str) -> Result<Value, Refusal> {
    let store = open(id)?;
    let state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    let batch = state
        .record
        .batches
        .iter()
        .find(|batch| batch.id == batch_id)
        .ok_or_else(|| Refusal::not_found("vector store file batch", batch_id))?;
    Ok(batch_json(&state.record, batch))
}

#[derive(Deserialize)]
struct SearchRequest {
    query: Value,
    #[serde(default)]
    max_num_results: Option<u32>,
    #[serde(default)]
    filters: Option<Value>,
    #[serde(default)]
    ranking_options: Option<RankingOptions>,
}

#[derive(Deserialize)]
struct RankingOptions {
    #[serde(default)]
    ranker: Option<String>,
    #[serde(default)]
    score_threshold: Option<f64>,
}

async fn search_inner(app: &AppState, id: &str, body: AxumBody) -> Result<Value, Refusal> {
    let value = read_json(body, MAX_BODY_BYTES).await?;
    let request: SearchRequest = parse_body(value)?;
    validate_filters(request.filters.as_ref())?;
    let threshold = validate_ranking(request.ranking_options.as_ref())?;
    let limit = search_limit(request.max_num_results)?;
    let store = open(id)?;
    let query_vector = match &request.query {
        Value::String(text) => {
            let text = text.trim();
            if text.is_empty() {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    "query must not be empty",
                ));
            }
            let explicit = {
                let state = lock(&store.state);
                if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
                    return Err(Refusal::not_found("vector store", id));
                }
                state.record.embedding_model.clone()
            };
            let model =
                configured_embedding_model(app, explicit.as_deref()).ok_or_else(no_embedder)?;
            let mut vectors = embed_texts(app, &model, &[text.to_owned()])
                .await
                .map_err(|detail| {
                    Refusal::with_status(
                        StatusCode::BAD_GATEWAY,
                        "upstream_error",
                        format!(
                            "embedding model \"{model}\" failed while embedding the search query: {detail}"
                        ),
                    )
                })?;
            vectors
                .pop()
                .ok_or_else(|| Refusal::internal("the embedding model returned no query vector"))?
        }
        Value::Array(values) => {
            if values.is_empty() {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    "query must not be an empty array",
                ));
            }
            let mut vector = Vec::with_capacity(values.len());
            for value in values {
                let number = value.as_f64().ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        "query array must contain only numbers",
                    )
                })?;
                if !number.is_finite() {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        "query array must contain only finite numbers",
                    ));
                }
                let narrowed = number as f32;
                if !narrowed.is_finite() {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        "query array contains a number outside the f32 range",
                    ));
                }
                vector.push(narrowed);
            }
            vector
        }
        _ => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "query must be a string or an array of numbers",
            ))
        }
    };

    let state = lock(&store.state);
    if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
        return Err(Refusal::not_found("vector store", id));
    }
    let (results, total) = score_store(&state, id, &query_vector, threshold, limit);
    let has_more = total > limit;
    let data: Vec<Value> = results.iter().map(scored_json).collect();
    drop(state);
    record_activity(&store, id);

    Ok(json!({
        "object": "vector_store.search_results.page",
        "search_query": request.query,
        "data": data,
        "has_more": has_more,
    }))
}

/// One ranked chunk, with the store it came from.
///
/// The store id is on every result because a merged search that cannot say which store answered is
/// not usable, and keeping the field on the scoped route too means a client can switch between the
/// two paths without changing its parser.
struct Scored {
    vector_store_id: String,
    file_id: String,
    filename: Option<String>,
    score: f32,
    text: String,
}

fn scored_json(scored: &Scored) -> Value {
    json!({
        "file_id": scored.file_id,
        "filename": scored.filename,
        "score": scored.score,
        "attributes": {},
        "content": [{"type": "text", "text": scored.text}],
        "vector_store_id": scored.vector_store_id,
    })
}

/// Rank one store's chunks against `query`, returning at most `cap` of them and the exact number
/// that passed the threshold.
///
/// `cap` is safe for a merged search as long as it is at least the final page size: a result outside
/// a store's own top `cap` cannot be in the merged top `cap`.
fn score_store(
    state: &StoreState,
    store_id: &str,
    query: &[f32],
    threshold: Option<f32>,
    cap: usize,
) -> (Vec<Scored>, usize) {
    let filenames: HashMap<&str, Option<&str>> = state
        .record
        .files
        .iter()
        .map(|file| (file.file_id.as_str(), file.filename.as_deref()))
        .collect();
    let mut ranked: Vec<(&Chunk, f32)> = state
        .chunks
        .iter()
        .map(|chunk| (chunk, cosine(query, &chunk.vector)))
        .filter(|(_, score)| threshold.is_none_or(|threshold| *score >= threshold))
        .collect();
    let total = ranked.len();
    ranked.sort_by(|(a, a_score), (b, b_score)| {
        b_score
            .partial_cmp(a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file_id.cmp(&b.file_id))
            .then_with(|| a.text.cmp(&b.text))
    });
    let results = ranked
        .into_iter()
        .take(cap)
        .map(|(chunk, score)| Scored {
            vector_store_id: store_id.to_owned(),
            file_id: chunk.file_id.clone(),
            filename: filenames
                .get(chunk.file_id.as_str())
                .copied()
                .flatten()
                .map(str::to_owned),
            score,
            text: chunk.text.clone(),
        })
        .collect();
    (results, total)
}

#[derive(Deserialize)]
struct TopSearchRequest {
    #[serde(default)]
    vector_store_id: Option<String>,
    #[serde(default)]
    vector_store_ids: Option<Vec<String>>,
    query: Value,
    #[serde(default)]
    max_num_results: Option<u32>,
    #[serde(default)]
    filters: Option<Value>,
    #[serde(default)]
    ranking_options: Option<RankingOptions>,
}

async fn search_anywhere_inner(app: &AppState, body: AxumBody) -> Result<Response, Refusal> {
    let bytes = read_bytes(body, MAX_BODY_BYTES).await?;
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).map_err(|error| {
            Refusal::bad_request(
                "invalid_request_error",
                format!("the request body is not valid JSON: {error}"),
            )
        })?
    };
    let object = value.as_object().ok_or_else(|| {
        Refusal::bad_request(
            "invalid_request_error",
            "the request body must be a JSON object",
        )
    })?;
    if object.contains_key("model") {
        // `model` wins over the store keys; see the handler's comment. The bytes are passed
        // through untouched so provider search keeps the relay's ordinary request semantics.
        return Ok(crate::server::dispatch_internal(
            app,
            Method::POST,
            "/v1/search",
            bytes.to_vec(),
        )
        .await);
    }

    let request: TopSearchRequest = serde_json::from_value(value).map_err(|error| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("the request body is not valid: {error}"),
        )
    })?;
    validate_filters(request.filters.as_ref())?;
    let threshold = validate_ranking(request.ranking_options.as_ref())?;
    let limit = search_limit(request.max_num_results)?;
    let query = match &request.query {
        Value::String(text) if !text.trim().is_empty() => text.trim().to_owned(),
        Value::String(_) => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "query must not be empty",
            ))
        }
        _ => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "query must be a string on /v1/search; use /v1/vector_stores/{id}/search for a numeric query",
            ))
        }
    };

    let ids = match (request.vector_store_id, request.vector_store_ids) {
        (Some(_), Some(_)) => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "name either vector_store_id (one store) or vector_store_ids (several stores), not both",
            ))
        }
        (Some(id), None) => vec![id],
        (None, Some(ids)) if ids.is_empty() => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "vector_store_ids must name at least one store",
            ))
        }
        (None, Some(ids)) => ids,
        (None, None) => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "this body names no target: include `model` for a provider search, or `vector_store_id` (one store) or `vector_store_ids` (several stores) for local retrieval",
            ))
        }
    };
    if ids.len() > MAX_SEARCH_STORES {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("a merged search may name at most {MAX_SEARCH_STORES} stores"),
        ));
    }
    let mut unique = HashSet::new();
    for id in &ids {
        if !unique.insert(id.as_str()) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("vector store \"{id}\" appears more than once"),
            ));
        }
    }

    // Open every store before embedding anything, so a missing store is a 404 rather than a
    // partial search, and resolve each store's model while doing so.
    let mut stores = Vec::with_capacity(ids.len());
    let mut model: Option<String> = None;
    for id in &ids {
        let store = open(id)?;
        let explicit = {
            let state = lock(&store.state);
            if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
                return Err(Refusal::not_found("vector store", id));
            }
            state.record.embedding_model.clone()
        };
        let resolved =
            configured_embedding_model(app, explicit.as_deref()).ok_or_else(no_embedder)?;
        match &model {
            None => model = Some(resolved),
            Some(existing) if existing != &resolved => {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!(
                        "a merged search needs one embedding model, but the named stores resolve to \"{existing}\" and \"{resolved}\"; their cosine scores are not comparable, so search them separately"
                    ),
                ))
            }
            Some(_) => {}
        }
        stores.push(store);
    }
    let model = model.ok_or_else(no_embedder)?;
    let mut query_vectors = embed_texts(app, &model, std::slice::from_ref(&query))
        .await
        .map_err(|detail| {
            Refusal::with_status(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                format!(
                    "embedding model \"{model}\" failed while embedding the search query: {detail}"
                ),
            )
        })?;
    let query_vector = query_vectors
        .pop()
        .ok_or_else(|| Refusal::internal("the embedding model returned no query vector"))?;

    let mut merged: Vec<Scored> = Vec::new();
    let mut total = 0usize;
    for store in &stores {
        let state = lock(&store.state);
        if store.deleted.load(Ordering::SeqCst) || is_expired(&state.record, now()) {
            return Err(Refusal::not_found("vector store", &store.id));
        }
        let (results, store_total) =
            score_store(&state, &store.id, &query_vector, threshold, limit);
        total = total.saturating_add(store_total);
        merged.extend(results);
        drop(state);
        record_activity(store, &store.id);
    }
    merged.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.vector_store_id.cmp(&b.vector_store_id))
            .then_with(|| a.file_id.cmp(&b.file_id))
            .then_with(|| a.text.cmp(&b.text))
    });
    merged.truncate(limit);
    let has_more = total > limit;
    let data: Vec<Value> = merged.iter().map(scored_json).collect();

    Ok(Json(json!({
        "object": "vector_store.search_results.page",
        "search_query": query,
        "data": data,
        "has_more": has_more,
    }))
    .into_response())
}

// --- Attach preparation ---------------------------------------------------------------------

/// One file's chunks and vectors, ready to be recorded.
struct PreparedFile {
    file_id: String,
    filename: Option<String>,
    pieces: Vec<String>,
    vectors: Vec<Vec<f32>>,
}

/// The metadata the batch lane writes next to a file, read here without touching its private API.
#[derive(Deserialize)]
struct BatchFileMeta {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    deleted: bool,
}

async fn prepare_attachments(
    app: &AppState,
    explicit_model: Option<&str>,
    file_ids: &[String],
    existing_ids: &HashSet<String>,
    existing_chunks: usize,
    existing_bytes: usize,
) -> Result<Vec<PreparedFile>, Refusal> {
    let mut seen = HashSet::new();
    for file_id in file_ids {
        if !is_file_id(file_id) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("file id \"{file_id}\" is not a file id this relay issued"),
            ));
        }
        if !seen.insert(file_id.clone()) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("file \"{file_id}\" appears more than once"),
            ));
        }
        if existing_ids.contains(file_id) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("file \"{file_id}\" is already attached to this vector store"),
            ));
        }
    }

    let mut sources: Vec<(String, Option<String>, Vec<String>)> =
        Vec::with_capacity(file_ids.len());
    for file_id in file_ids {
        let (text, filename) = read_batch_file(app, file_id).await?;
        let owned_id = file_id.clone();
        let pieces = tokio::task::spawn_blocking(move || chunk(&text))
            .await
            .map_err(|error| {
                Refusal::internal(format!("chunking file \"{owned_id}\" failed: {error}"))
            })?;
        sources.push((file_id.clone(), filename, pieces));
    }

    let (max_chunks, max_text_bytes) = chunk_caps();
    let mut total_chunks = existing_chunks;
    let mut total_bytes = existing_bytes;
    for (_, _, pieces) in &sources {
        let bytes = pieces.iter().map(|piece| piece.len()).sum::<usize>();
        total_chunks = total_chunks
            .checked_add(pieces.len())
            .ok_or_else(|| too_large(max_chunks, max_text_bytes))?;
        total_bytes = total_bytes
            .checked_add(bytes)
            .ok_or_else(|| too_large(max_chunks, max_text_bytes))?;
        if total_chunks > max_chunks || total_bytes > max_text_bytes {
            return Err(too_large(max_chunks, max_text_bytes));
        }
    }

    let mut texts = Vec::new();
    let mut spans = Vec::with_capacity(sources.len());
    for (_, _, pieces) in &sources {
        let start = texts.len();
        texts.extend(pieces.iter().cloned());
        spans.push(start..texts.len());
    }
    let vectors = if texts.is_empty() {
        Vec::new()
    } else {
        let model = configured_embedding_model(app, explicit_model).ok_or_else(no_embedder)?;
        embed_texts(app, &model, &texts).await.map_err(|detail| {
            Refusal::with_status(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                format!(
                    "embedding model \"{model}\" failed while embedding the attached file(s): {detail}"
                ),
            )
        })?
    };

    let mut prepared = Vec::with_capacity(sources.len());
    for ((file_id, filename, pieces), span) in sources.into_iter().zip(spans) {
        prepared.push(PreparedFile {
            file_id,
            filename,
            pieces,
            vectors: vectors[span].to_vec(),
        });
    }
    Ok(prepared)
}

/// Read a file the batch lane published, without using its private accessors.
///
/// The batch lane's `root()` is public and its layout is stable: `files/<id>.json` for the metadata
/// and `files/<id>.jsonl` for the body. Reading it here rather than widening the batch lane's API
/// keeps the two surfaces independent.
async fn read_batch_file(
    app: &AppState,
    file_id: &str,
) -> Result<(String, Option<String>), Refusal> {
    let files = app.batch.root().join("files");
    let meta_path = files.join(format!("{file_id}.json"));
    let body_path = files.join(format!("{file_id}.jsonl"));
    let owned_id = file_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let bytes = fs::read(&meta_path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Refusal::not_found("file", &owned_id),
            _ => Refusal::internal(format!(
                "cannot read the metadata of file \"{owned_id}\": {error}"
            )),
        })?;
        let meta: BatchFileMeta = serde_json::from_slice(&bytes).map_err(|error| {
            Refusal::internal(format!(
                "the metadata of file \"{owned_id}\" is not valid: {error}"
            ))
        })?;
        if meta.deleted {
            return Err(Refusal::not_found("file", &owned_id));
        }
        let body = fs::read(&body_path).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => Refusal::not_found("file", &owned_id),
            _ => Refusal::internal(format!("cannot read file \"{owned_id}\": {error}")),
        })?;
        let text = String::from_utf8(body).map_err(|_| {
            Refusal::bad_request(
                "invalid_request_error",
                format!("file \"{owned_id}\" is not UTF-8 text, so it cannot be chunked"),
            )
        })?;
        Ok((text, meta.filename))
    })
    .await
    .map_err(|error| Refusal::internal(format!("reading file \"{file_id}\" failed: {error}")))?
}

/// Append prepared files and, when given, a batch record, then write `store.json` once.
///
/// The chunks file is appended before the record is written, so a crash between the two leaves
/// chunks that no file record names — harmless, because search only sees chunks through the record's
/// files and a later detach rewrites the file. The reverse order would leave a file record whose
/// chunks are missing, which is a half-attach and is what this function must never produce.
fn append_prepared(
    store: &Store,
    state: &mut StoreState,
    prepared: Vec<PreparedFile>,
    batch: Option<BatchRecord>,
) -> Result<(), Refusal> {
    let previous_record = state.record.clone();
    let previous_len = state.chunks.len();
    let created_at = now();
    let mut chunks = Vec::new();
    for file in &prepared {
        for (index, (text, vector)) in file.pieces.iter().zip(&file.vectors).enumerate() {
            chunks.push(Chunk {
                file_id: file.file_id.clone(),
                chunk_index: index as u32,
                text: text.clone(),
                vector: vector.clone(),
            });
        }
    }
    if let Err(error) = append_chunks(&store.chunks_path(), &chunks) {
        return Err(Refusal::internal(format!(
            "cannot write the vector store's chunks: {error}"
        )));
    }

    for file in &prepared {
        let usage_bytes: u64 = file.pieces.iter().map(|piece| piece.len() as u64).sum();
        state.record.files.push(FileRecord {
            file_id: file.file_id.clone(),
            created_at,
            filename: file.filename.clone(),
            usage_bytes,
            chunks: file.pieces.len(),
        });
        state.record.usage_bytes = state.record.usage_bytes.saturating_add(usage_bytes);
    }
    state.record.last_active_at = created_at;
    if let Some(batch) = batch {
        state.record.batches.push(batch);
    }
    state.chunks.extend(chunks);

    let record_bytes = match serde_json::to_vec(&state.record) {
        Ok(bytes) => bytes,
        Err(error) => {
            state.chunks.truncate(previous_len);
            state.record = previous_record;
            let _ = write_chunks_atomic(&store.chunks_path(), &state.chunks);
            return Err(Refusal::internal(format!(
                "cannot encode the vector store: {error}"
            )));
        }
    };
    if let Err(error) = write_atomic(&store.store_path(), &record_bytes) {
        state.chunks.truncate(previous_len);
        state.record = previous_record;
        let _ = write_chunks_atomic(&store.chunks_path(), &state.chunks);
        return Err(Refusal::internal(format!(
            "cannot write the vector store: {error}"
        )));
    }
    Ok(())
}

// --- Embeddings -----------------------------------------------------------------------------

/// Resolve the embedding model for a store, in the documented order.
///
/// 1. the store's own `embedding_model` extension,
/// 2. `MANAROUTER_EMBEDDING_MODEL`,
/// 3. the first provider in the routing snapshot that declares [`Capability::Embeddings`].
///
/// The chosen model is logged once at `debug`, because an operator debugging a dimension mismatch
/// needs to know which model actually ran.
fn configured_embedding_model(app: &AppState, explicit: Option<&str>) -> Option<String> {
    if let Some(model) = explicit.map(str::trim).filter(|model| !model.is_empty()) {
        let model = model.to_owned();
        tracing::debug!(
            model = %model,
            source = "vector store",
            "vector store embedding model resolved"
        );
        return Some(model);
    }
    if let Some(model) = env_embedding_model() {
        tracing::debug!(
            model = %model,
            source = "MANAROUTER_EMBEDDING_MODEL",
            "vector store embedding model resolved"
        );
        return Some(model);
    }
    let table = app.relay.table();
    for provider in table.snapshot.providers() {
        if !provider.capabilities.contains(&Capability::Embeddings) {
            continue;
        }
        let Some(step) = provider.steps.first() else {
            continue;
        };
        let model = format!("{}/{}", provider.id.as_ref(), step.model);
        tracing::debug!(
            model = %model,
            source = "routing snapshot",
            "vector store embedding model resolved"
        );
        return Some(model);
    }
    tracing::debug!("no vector store embedding model is configured");
    None
}

fn env_embedding_model() -> Option<String> {
    #[cfg(test)]
    if let Some(value) = TEST_EMBEDDING_ENV.with(|slot| slot.borrow().clone()) {
        return value;
    }
    std::env::var("MANAROUTER_EMBEDDING_MODEL")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Embed `texts` through the relay's own `/v1/embeddings` route.
///
/// The route is dispatched internally, so it goes through the same routing, credential and
/// admission path as a client request. A non-2xx answer, a malformed body, a wrong vector count or a
/// non-finite number is a failure with a message that names what went wrong; the caller turns that
/// into a `502` and does not record a half-attached file.
async fn embed_texts(
    app: &AppState,
    model: &str,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let mut vectors = Vec::with_capacity(texts.len());
    for batch in texts.chunks(EMBED_BATCH) {
        let body = serde_json::to_vec(&json!({"model": model, "input": batch}))
            .map_err(|error| format!("the embedding request could not be encoded: {error}"))?;
        let response =
            crate::server::dispatch_internal(app, Method::POST, "/v1/embeddings", body).await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), EMBED_RESPONSE_BYTES)
            .await
            .map_err(|error| format!("the embedding response could not be read: {error}"))?;
        if !status.is_success() {
            let detail = serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| String::from_utf8_lossy(&bytes).chars().take(500).collect());
            return Err(format!("the relay answered {status}: {detail}"));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("the embedding response is not JSON: {error}"))?;
        let data = value
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| "the embedding response has no `data` array".to_owned())?;
        if data.len() != batch.len() {
            return Err(format!(
                "the embedding response has {} vectors for {} inputs",
                data.len(),
                batch.len()
            ));
        }
        let mut ordered: Vec<Option<Vec<f32>>> = vec![None; batch.len()];
        for (position, item) in data.iter().enumerate() {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .map(|index| index as usize)
                .unwrap_or(position);
            if index >= batch.len() {
                return Err(format!(
                    "the embedding response names index {index} for {} inputs",
                    batch.len()
                ));
            }
            let embedding = item
                .get("embedding")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("the embedding at index {index} has no vector"))?;
            if embedding.is_empty() {
                return Err(format!("the embedding at index {index} is empty"));
            }
            let mut vector = Vec::with_capacity(embedding.len());
            for value in embedding {
                let number = value.as_f64().ok_or_else(|| {
                    format!("the embedding at index {index} contains a non-number")
                })?;
                if !number.is_finite() {
                    return Err(format!(
                        "the embedding at index {index} contains a non-finite number"
                    ));
                }
                let narrowed = number as f32;
                if !narrowed.is_finite() {
                    return Err(format!(
                        "the embedding at index {index} contains a number outside the f32 range"
                    ));
                }
                vector.push(narrowed);
            }
            if ordered[index].is_some() {
                return Err(format!("the embedding response repeats index {index}"));
            }
            ordered[index] = Some(vector);
        }
        let dimensions = ordered.iter().flatten().next().map(Vec::len).unwrap_or(0);
        for (index, vector) in ordered.into_iter().enumerate() {
            let vector =
                vector.ok_or_else(|| format!("the embedding response omits index {index}"))?;
            if vector.len() != dimensions {
                return Err(format!(
                    "the embedding at index {index} has {} dimensions, expected {dimensions}",
                    vector.len()
                ));
            }
            vectors.push(vector);
        }
    }
    Ok(vectors)
}

// --- Chunking -------------------------------------------------------------------------------

/// Split `text` into chunks of at most [`CHUNK_TARGET`] characters, with up to [`CHUNK_OVERLAP`]
/// characters of overlap.
///
/// Paragraphs are preferred, then sentences, then characters. The function never splits a UTF-8
/// code point: every cut is made at a `char` boundary. Empty or whitespace-only text produces no
/// chunks.
fn chunk(text: &str) -> Vec<String> {
    let units = split_units(text);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for unit in units {
        let unit_len = unit.chars().count();
        if current_len == 0 {
            current.push_str(&unit);
            current_len = unit_len;
            continue;
        }
        if current_len + 1 + unit_len <= CHUNK_TARGET {
            current.push(' ');
            current.push_str(&unit);
            current_len += 1 + unit_len;
            continue;
        }
        // Overlap only at a word boundary: a tail that fills the overlap window without meeting
        // whitespace is a cut through a long run (a URL, a base64 blob, CJK text), and repeating
        // part of it would duplicate characters without helping retrieval.
        let overlap = {
            let tail = overlap_tail(&current, CHUNK_OVERLAP);
            let tail_len = tail.chars().count();
            if current.chars().count() > CHUNK_OVERLAP && tail_len < CHUNK_OVERLAP {
                tail.to_owned()
            } else {
                String::new()
            }
        };
        let overlap_len = overlap.chars().count();
        chunks.push(std::mem::take(&mut current));
        if !overlap.is_empty() && overlap_len + 1 + unit_len <= CHUNK_TARGET {
            current.push_str(&overlap);
            current.push(' ');
            current.push_str(&unit);
            current_len = overlap_len + 1 + unit_len;
        } else {
            current.push_str(&unit);
            current_len = unit_len;
        }
    }
    if current_len > 0 {
        chunks.push(current);
    }
    chunks
}

fn split_units(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    for paragraph in text.split('\n') {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        if paragraph.chars().count() <= CHUNK_TARGET {
            units.push(paragraph.to_owned());
            continue;
        }
        for sentence in split_sentences(paragraph) {
            let sentence = sentence.trim();
            if sentence.is_empty() {
                continue;
            }
            if sentence.chars().count() <= CHUNK_TARGET {
                units.push(sentence.to_owned());
            } else {
                for piece in split_long(sentence) {
                    let piece = piece.trim();
                    if !piece.is_empty() {
                        units.push(piece.to_owned());
                    }
                }
            }
        }
    }
    units
}

fn split_sentences(paragraph: &str) -> Vec<&str> {
    let mut sentences = Vec::new();
    let mut start = 0usize;
    for (index, character) in paragraph.char_indices() {
        if !is_sentence_end(character) {
            continue;
        }
        let end = index + character.len_utf8();
        let next = paragraph[end..].chars().next();
        if next.is_some_and(|next| !next.is_whitespace()) {
            continue;
        }
        sentences.push(&paragraph[start..end]);
        start = end;
    }
    if start < paragraph.len() {
        sentences.push(&paragraph[start..]);
    }
    sentences
}

fn split_long(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut remaining = text.chars().count();
    let mut start = 0usize;
    while start < text.len() {
        if remaining <= CHUNK_TARGET {
            pieces.push(&text[start..]);
            break;
        }
        let mut end = start;
        for (count, (offset, character)) in text[start..].char_indices().enumerate() {
            if count == CHUNK_TARGET {
                break;
            }
            end = start + offset + character.len_utf8();
        }
        let mut cut = end;
        if let Some((offset, character)) = text[start..end]
            .char_indices()
            .rev()
            .find(|(_, character)| character.is_whitespace())
        {
            let candidate = start + offset;
            if candidate > start + (end - start) / 2 {
                cut = candidate + character.len_utf8();
            }
        }
        if cut <= start {
            cut = end;
        }
        pieces.push(&text[start..cut]);
        let consumed = text[start..cut].chars().count();
        remaining = remaining.saturating_sub(consumed);
        start = cut;
    }
    pieces
}

fn overlap_tail(text: &str, limit: usize) -> &str {
    let total = text.chars().count();
    if total <= limit {
        return text;
    }
    let skip = total - limit;
    let mut start = 0usize;
    for (index, (offset, _)) in text.char_indices().enumerate() {
        if index == skip {
            start = offset;
            break;
        }
    }
    let tail = &text[start..];
    let tail = match tail
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
    {
        Some((offset, character)) => &tail[offset + character.len_utf8()..],
        None => tail,
    };
    whole_word_tail(tail)
}

fn whole_word_tail(text: &str) -> &str {
    match text
        .char_indices()
        .rev()
        .find(|(_, character)| character.is_whitespace())
    {
        Some((offset, character)) => text[offset + character.len_utf8()..].trim(),
        None => text.trim(),
    }
}

fn is_sentence_end(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | ';' | '。' | '！' | '？' | '；')
}

// --- Similarity -----------------------------------------------------------------------------

/// Cosine similarity, in `[-1, 1]`.
///
/// Returns `0.0` — never `NaN` — for a length mismatch, an empty vector, a zero vector or a
/// non-finite component. A `NaN` score would sort unpredictably and serialize as `null`, so the
/// degenerate cases are answered rather than propagated.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (left, right) in a.iter().zip(b) {
        let left = f64::from(*left);
        let right = f64::from(*right);
        if !left.is_finite() || !right.is_finite() {
            return 0.0;
        }
        dot += left * right;
        norm_a += left * left;
        norm_b += right * right;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    let score = dot / (norm_a.sqrt() * norm_b.sqrt());
    if !score.is_finite() {
        return 0.0;
    }
    score.clamp(-1.0, 1.0) as f32
}

// --- Validation -----------------------------------------------------------------------------

fn validate_name(name: &str) -> Result<(), Refusal> {
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("name may be at most {MAX_NAME_CHARS} characters"),
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "name must not contain control characters",
        ));
    }
    Ok(())
}

fn metadata_object(value: &Value) -> Result<Map<String, Value>, Refusal> {
    let object = value.as_object().ok_or_else(|| {
        Refusal::bad_request("invalid_request_error", "metadata must be an object")
    })?;
    if object.len() > MAX_METADATA_PAIRS {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("metadata may hold at most {MAX_METADATA_PAIRS} pairs"),
        ));
    }
    let mut metadata = Map::new();
    for (key, value) in object {
        if key.is_empty() || key.chars().count() > MAX_METADATA_KEY_CHARS {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("metadata keys must be 1 to {MAX_METADATA_KEY_CHARS} characters"),
            ));
        }
        if key.chars().any(char::is_control) {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("metadata key \"{key}\" contains a control character"),
            ));
        }
        match value {
            Value::String(text) if text.chars().count() <= MAX_METADATA_VALUE_CHARS => {}
            Value::String(_) => {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!(
                        "metadata value for \"{key}\" is longer than {MAX_METADATA_VALUE_CHARS} characters"
                    ),
                ))
            }
            Value::Number(_) | Value::Bool(_) => {}
            _ => {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!("metadata value for \"{key}\" must be a string, number or boolean"),
                ))
            }
        }
        metadata.insert(key.clone(), value.clone());
    }
    Ok(metadata)
}

fn parse_expires(value: &Value) -> Result<Option<Value>, Refusal> {
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object().ok_or_else(|| {
        Refusal::bad_request(
            "invalid_request_error",
            "expires_after must be an object or null",
        )
    })?;
    let anchor = object
        .get("anchor")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            Refusal::bad_request(
                "invalid_request_error",
                "expires_after.anchor must be \"last_active_at\"",
            )
        })?;
    if anchor != "last_active_at" {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("expires_after.anchor \"{anchor}\" is not supported; use \"last_active_at\""),
        ));
    }
    let days = object.get("days").and_then(Value::as_u64).ok_or_else(|| {
        Refusal::bad_request(
            "invalid_request_error",
            "expires_after.days must be a positive integer",
        )
    })?;
    if !(1..=365).contains(&days) {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "expires_after.days must be between 1 and 365",
        ));
    }
    Ok(Some(json!({"anchor": "last_active_at", "days": days})))
}

fn validate_model_name(model: &str) -> Result<String, Refusal> {
    let model = model.trim();
    if model.is_empty() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "embedding_model must not be empty",
        ));
    }
    if model.chars().count() > MAX_MODEL_CHARS {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("embedding_model may be at most {MAX_MODEL_CHARS} characters"),
        ));
    }
    if model.chars().any(char::is_control) {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "embedding_model must not contain control characters",
        ));
    }
    Ok(model.to_owned())
}

fn validate_chunking_strategy(value: &Value) -> Result<(), Refusal> {
    match value {
        Value::Null => Ok(()),
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str).unwrap_or("static");
            if kind != "static" {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!(
                        "chunking_strategy.type \"{kind}\" is not supported; this store chunks by characters with a static strategy"
                    ),
                ));
            }
            if let Some(static_) = map.get("static") {
                let object = static_.as_object().ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        "chunking_strategy.static must be an object",
                    )
                })?;
                for key in object.keys() {
                    if !matches!(
                        key.as_str(),
                        "max_chunk_size_tokens"
                            | "chunk_overlap_tokens"
                            | "max_chunk_size_characters"
                            | "chunk_overlap_characters"
                    ) {
                        return Err(Refusal::bad_request(
                            "invalid_request_error",
                            format!("chunking_strategy.static.{key} is not a recognized field"),
                        ));
                    }
                }
                for key in ["max_chunk_size_tokens", "chunk_overlap_tokens"] {
                    if let Some(number) = object.get(key) {
                        if number.as_u64().is_none_or(|number| number == 0) {
                            return Err(Refusal::bad_request(
                                "invalid_request_error",
                                format!(
                                    "chunking_strategy.static.{key} must be a positive integer"
                                ),
                            ));
                        }
                    }
                }
            }
            Ok(())
        }
        _ => Err(Refusal::bad_request(
            "invalid_request_error",
            "chunking_strategy must be an object or null",
        )),
    }
}

fn validate_filters(filters: Option<&Value>) -> Result<(), Refusal> {
    match filters {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Object(map)) if map.is_empty() => Ok(()),
        Some(Value::Object(_)) => Err(Refusal::bad_request(
            "invalid_request_error",
            "filters are not supported: this relay does not store per-chunk attributes, so a filtered search would silently return the wrong set",
        )),
        Some(_) => Err(Refusal::bad_request(
            "invalid_request_error",
            "filters must be an object",
        )),
    }
}

fn validate_ranking(options: Option<&RankingOptions>) -> Result<Option<f32>, Refusal> {
    let Some(options) = options else {
        return Ok(None);
    };
    if let Some(ranker) = options.ranker.as_deref() {
        if !matches!(ranker, "auto" | "default" | "default-2024-11-15" | "cosine") {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "ranking_options.ranker \"{ranker}\" is not supported; this store ranks by cosine similarity"
                ),
            ));
        }
    }
    let Some(threshold) = options.score_threshold else {
        return Ok(None);
    };
    let threshold = threshold as f32;
    if !threshold.is_finite() || !(-1.0..=1.0).contains(&threshold) {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "ranking_options.score_threshold must be a number between -1 and 1",
        ));
    }
    Ok(Some(threshold))
}

fn parse_limit(raw: Option<&str>) -> Result<usize, Refusal> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_LIMIT);
    };
    let limit: usize = raw.parse().map_err(|_| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("limit \"{raw}\" is not an integer"),
        )
    })?;
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("limit must be between 1 and {MAX_LIMIT}"),
        ));
    }
    Ok(limit)
}

fn search_limit(raw: Option<u32>) -> Result<usize, Refusal> {
    match raw {
        None => Ok(DEFAULT_SEARCH_RESULTS),
        Some(limit) if (1..=MAX_SEARCH_RESULTS).contains(&limit) => Ok(limit as usize),
        Some(_) => Err(Refusal::bad_request(
            "invalid_request_error",
            format!("max_num_results must be between 1 and {MAX_SEARCH_RESULTS}"),
        )),
    }
}

fn no_embedder() -> Refusal {
    Refusal::with_status(
        StatusCode::CONFLICT,
        "unsupported_endpoint",
        "this vector store has no embedding model. Set `embedding_model` on the vector store, set MANAROUTER_EMBEDDING_MODEL, or configure a provider that declares the embeddings capability.",
    )
}

fn too_large(max_chunks: usize, max_text_bytes: usize) -> Refusal {
    Refusal::with_status(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        format!(
            "attaching this file would exceed the vector store's denial-of-service bound of {max_chunks} chunks or {max_text_bytes} bytes of text"
        ),
    )
}

fn already_attached(store_id: &str, file_id: &str) -> Refusal {
    Refusal::bad_request(
        "invalid_request_error",
        format!("file \"{file_id}\" is already attached to vector store \"{store_id}\""),
    )
}

// --- JSON views -----------------------------------------------------------------------------

fn store_json(record: &StoreRecord) -> Value {
    let attached = record.files.len() as u64;
    json!({
        "id": record.id,
        "object": "vector_store",
        "created_at": record.created_at,
        "name": record.name,
        "status": "completed",
        "usage_bytes": record.usage_bytes,
        "file_counts": {
            "in_progress": 0,
            "completed": attached,
            "failed": 0,
            "cancelled": 0,
            "total": attached,
        },
        "metadata": record.metadata,
        "expires_after": record.expires_after,
        "last_active_at": record.last_active_at,
    })
}

fn file_json(record: &StoreRecord, file: &FileRecord) -> Value {
    json!({
        "id": file.file_id,
        "object": "vector_store.file",
        "created_at": file.created_at,
        "vector_store_id": record.id,
        "status": "completed",
        "usage_bytes": file.usage_bytes,
        "last_error": null,
        "chunking_strategy": {
            "type": "static",
            "static": {
                "max_chunk_size_characters": CHUNK_TARGET,
                "chunk_overlap_characters": CHUNK_OVERLAP,
            },
        },
    })
}

fn batch_json(record: &StoreRecord, batch: &BatchRecord) -> Value {
    json!({
        "id": batch.id,
        "object": "vector_store.file_batch",
        "created_at": batch.created_at,
        "vector_store_id": record.id,
        "status": "completed",
        "file_counts": batch.file_counts,
    })
}

// --- Small helpers --------------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic in another request must not turn this store into a permanent 500. The state is a
    // record plus a chunk list, and every mutation either completed or was rolled back, so the
    // poisoned value is safe to keep using.
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn authorize(app: &AppState, headers: &HeaderMap) -> Option<Response> {
    crate::server::authorize_batch_request(app, headers)
}

async fn read_bytes(body: AxumBody, max: usize) -> Result<Bytes, Refusal> {
    crate::server::read_body(body, max)
        .await
        .map_err(|error| match error {
            crate::server::BodyError::TooLarge => Refusal::with_status(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                format!("the request body exceeds {max} bytes"),
            ),
            crate::server::BodyError::Read(detail) => Refusal::bad_request(
                "invalid_request_error",
                format!("the request body could not be read: {detail}"),
            ),
        })
}

async fn read_json(body: AxumBody, max: usize) -> Result<Value, Refusal> {
    let bytes = read_bytes(body, max).await?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("the request body is not valid JSON: {error}"),
        )
    })
}

fn parse_body<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, Refusal> {
    if value.is_null() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            "a JSON object body is required",
        ));
    }
    serde_json::from_value(value).map_err(|error| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("the request body is not valid: {error}"),
        )
    })
}

fn now() -> u64 {
    #[cfg(test)]
    if let Some(now) = TEST_NOW.with(Cell::get) {
        return now;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn chunk_caps() -> (usize, usize) {
    #[cfg(test)]
    if let Some(caps) = TEST_CAPS.with(Cell::get) {
        return caps;
    }
    (MAX_CHUNKS, MAX_TEXT_BYTES)
}

fn is_expired(record: &StoreRecord, now: u64) -> bool {
    let Some(expires) = record.expires_after.as_ref() else {
        return false;
    };
    let days = expires.get("days").and_then(Value::as_u64).unwrap_or(0);
    if days == 0 {
        return false;
    }
    now.saturating_sub(record.last_active_at) >= days.saturating_mul(86_400)
}

fn new_store_id() -> String {
    let hex = mr_core::ids::new_id().replace('-', "");
    format!("vs_{}", &hex[..24])
}

fn new_batch_id() -> String {
    let hex = mr_core::ids::new_id().replace('-', "");
    format!("vsfb_{}", &hex[..24])
}

fn is_store_id(id: &str) -> bool {
    match id.strip_prefix("vs_") {
        Some(rest) => {
            rest.len() == 24
                && rest
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        }
        None => false,
    }
}

fn is_file_id(id: &str) -> bool {
    match id.strip_prefix("file_") {
        Some(rest) => {
            rest.len() == 36
                && rest.bytes().enumerate().all(|(index, byte)| match index {
                    8 | 13 | 18 | 23 => byte == b'-',
                    _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
                })
        }
        None => false,
    }
}

fn record_activity(store: &Store, id: &str) {
    // Re-read the record under the lock: another request may have attached a file since this
    // search scored its chunks, and writing a stale clone would erase that attachment from
    // `store.json` while its chunks stayed on disk.
    let mut state = lock(&store.state);
    state.record.last_active_at = now();
    let Ok(bytes) = serde_json::to_vec(&state.record) else {
        tracing::warn!(store = %id, "could not encode the vector store after a search");
        return;
    };
    if let Err(error) = write_atomic(&store.store_path(), &bytes) {
        tracing::warn!(store = %id, %error, "could not record vector store activity");
    }
}

// --- Disk -----------------------------------------------------------------------------------

fn load_store(id: &str, dir: PathBuf) -> Result<Store, String> {
    let bytes = fs::read(dir.join(STORE_FILE))
        .map_err(|error| format!("cannot read {STORE_FILE}: {error}"))?;
    let record: StoreRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{STORE_FILE} is not valid: {error}"))?;
    if record.id != id {
        return Err(format!("{STORE_FILE} names \"{}\"", record.id));
    }
    let chunks = read_chunks(&dir.join(CHUNKS_FILE))
        .map_err(|error| format!("cannot read {CHUNKS_FILE}: {error}"))?;
    Ok(Store {
        id: id.to_owned(),
        dir,
        deleted: AtomicBool::new(false),
        state: Mutex::new(StoreState { record, chunks }),
    })
}

fn read_chunks(path: &Path) -> std::io::Result<Vec<Chunk>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let reader = BufReader::new(file);
    let mut chunks = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let chunk: Chunk = serde_json::from_str(&line).map_err(json_io)?;
        chunks.push(chunk);
    }
    Ok(chunks)
}

fn write_new_store(dir: &Path, record: &StoreRecord, chunks: &[Chunk]) -> Result<(), Refusal> {
    let result = (|| {
        fs::create_dir_all(dir).map_err(|error| {
            Refusal::internal(format!("cannot create the vector store directory: {error}"))
        })?;
        let record_bytes = serde_json::to_vec(record).map_err(|error| {
            Refusal::internal(format!("cannot encode the vector store: {error}"))
        })?;
        write_atomic(&dir.join(STORE_FILE), &record_bytes).map_err(|error| {
            Refusal::internal(format!("cannot write the vector store: {error}"))
        })?;
        write_chunks_atomic(&dir.join(CHUNKS_FILE), chunks).map_err(|error| {
            Refusal::internal(format!("cannot write the vector store's chunks: {error}"))
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(dir);
    }
    result
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "the path has no parent")
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("store");
    let temp = parent.join(format!(".{name}.tmp-{}", mr_core::ids::new_id()));
    let result = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn append_chunks(path: &Path, chunks: &[Chunk]) -> std::io::Result<()> {
    if chunks.is_empty() {
        return Ok(());
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = BufWriter::new(file);
    for chunk in chunks {
        serde_json::to_writer(&mut writer, chunk).map_err(json_io)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()
}

fn write_chunks_atomic(path: &Path, chunks: &[Chunk]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "the path has no parent")
    })?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("chunks");
    let temp = parent.join(format!(".{name}.tmp-{}", mr_core::ids::new_id()));
    let result = (|| {
        let file = fs::File::create(&temp)?;
        let mut writer = BufWriter::new(file);
        for chunk in chunks {
            serde_json::to_writer(&mut writer, chunk).map_err(json_io)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn json_io(error: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error)
}

// --- Test overrides -------------------------------------------------------------------------

#[cfg(test)]
use std::cell::{Cell, RefCell};

#[cfg(test)]
thread_local! {
    static TEST_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    static TEST_EMBEDDING_ENV: RefCell<Option<Option<String>>> = const { RefCell::new(None) };
    static TEST_CAPS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
    static TEST_NOW: Cell<Option<u64>> = const { Cell::new(None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Bytes;
    use axum::routing::any;
    use tower::ServiceExt;

    use mr_core::dialect::{Capability, Dialect, InputModality};

    const FILE_ALPHA: &str = "file_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";
    const FILE_BETA: &str = "file_0190a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5c";

    struct TestEnv {
        app: AppState,
        _dir: tempfile::TempDir,
        _vault_dir: tempfile::TempDir,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            TEST_ROOT.with(|slot| *slot.borrow_mut() = None);
            TEST_EMBEDDING_ENV.with(|slot| *slot.borrow_mut() = None);
            TEST_CAPS.with(|slot| slot.set(None));
            TEST_NOW.with(|slot| slot.set(None));
        }
    }

    impl TestEnv {
        async fn new() -> Self {
            Self::new_with(vec![
                Capability::Chat,
                Capability::Models,
                Capability::Embeddings,
            ])
            .await
        }

        async fn new_with(capabilities: Vec<Capability>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("vector_stores");
            TEST_ROOT.with(|slot| *slot.borrow_mut() = Some(root));
            TEST_EMBEDDING_ENV.with(|slot| *slot.borrow_mut() = Some(None));
            TEST_CAPS.with(|slot| slot.set(None));
            TEST_NOW.with(|slot| slot.set(None));

            let (base_url, seen) = fake_embeddings().await;
            let mut provider = mr_core::model::Provider::new("p", Dialect::OpenAi, &base_url);
            provider.capabilities = capabilities;
            let mut account = mr_core::model::Account::new("a");
            account.secret_ref = Some("test-credential".to_owned());
            provider.accounts = vec![account];
            let mut entry = model_entry();
            entry.id = "embed-1".to_owned();
            provider.models = vec![entry];

            let config = mr_core::config::Config {
                providers: vec![provider],
                ..Default::default()
            };
            let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
            let relay = Arc::new(crate::state::RelayState::new(
                &config,
                snapshot,
                crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
            ));

            let vault_dir = tempfile::tempdir().unwrap();
            let mut opened = mr_core::secret::Vault::open(vault_dir.path()).unwrap();
            opened.put("test-credential", "sk-not-a-real-key").unwrap();
            let vault = Arc::new(opened);

            let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
            let app = AppState {
                relay,
                no_key_required: true,
                health: Arc::new(crate::select::Health::new()),
                vault,
                oauth_refreshers: Arc::new(std::collections::HashMap::new()),
                quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
                oauth_store: Arc::clone(&store),
                quota_store: Arc::clone(&store),
                sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
                activity: Arc::new(crate::activity::ActivityLog::new()),
                kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
                batch: crate::batch::BatchLane::inert(),
            };
            Self {
                app,
                _dir: dir,
                _vault_dir: vault_dir,
                seen,
            }
        }

        async fn call(&self, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
            Self::call_with(&self.app, method, uri, body).await
        }

        async fn call_with(
            app: &AppState,
            method: Method,
            uri: &str,
            body: Value,
        ) -> (StatusCode, Value) {
            Self::call_with_key(app, method, uri, body, None).await
        }

        async fn call_with_key(
            app: &AppState,
            method: Method,
            uri: &str,
            body: Value,
            key: Option<&str>,
        ) -> (StatusCode, Value) {
            let bytes = if body.is_null() {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            };
            let mut builder = axum::http::Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json");
            if let Some(key) = key {
                builder = builder.header("authorization", format!("Bearer {key}"));
            }
            let request = builder.body(AxumBody::from(bytes)).unwrap();
            let response = routes()
                .with_state(app.clone())
                .oneshot(request)
                .await
                .unwrap();
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let value = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap_or(Value::Null)
            };
            (status, value)
        }

        async fn create(&self, body: Value) -> Value {
            let (status, value) = self.call(Method::POST, "/v1/vector_stores", body).await;
            assert_eq!(status, StatusCode::OK, "create failed: {value}");
            value
        }

        async fn attach(&self, id: &str, file_id: &str) -> (StatusCode, Value) {
            self.call(
                Method::POST,
                &format!("/v1/vector_stores/{id}/files"),
                json!({"file_id": file_id}),
            )
            .await
        }

        async fn search(&self, id: &str, body: Value) -> (StatusCode, Value) {
            self.call(
                Method::POST,
                &format!("/v1/vector_stores/{id}/search"),
                body,
            )
            .await
        }

        fn write_file(&self, id: &str, text: &str) {
            let dir = self.app.batch.root().join("files");
            std::fs::create_dir_all(&dir).unwrap();
            let meta = json!({
                "id": id,
                "bytes": text.len(),
                "created_at": 1,
                "filename": format!("{id}.txt"),
                "purpose": "batch",
                "deleted": false,
            });
            std::fs::write(
                dir.join(format!("{id}.json")),
                serde_json::to_vec(&meta).unwrap(),
            )
            .unwrap();
            std::fs::write(dir.join(format!("{id}.jsonl")), text.as_bytes()).unwrap();
        }
    }

    fn model_entry() -> mr_core::model::ModelEntry {
        mr_core::model::ModelEntry {
            id: String::new(),
            source: mr_core::model::ModelSource::Manual,
            label: None,
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_modalities: vec![InputModality::Text],
            reasoning_levels: Vec::new(),
            default_reasoning_level: None,
            reasoning_wire: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            pricing: None,
            synced_at: None,
            hidden: false,
        }
    }

    async fn fake_embeddings() -> (String, Arc<Mutex<Vec<String>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                let body = String::from_utf8_lossy(&body).to_string();
                sink.lock().unwrap().push(body.clone());
                let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let inputs: Vec<String> = match value.get("input") {
                    Some(Value::String(text)) => vec![text.clone()],
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|item| item.as_str().unwrap_or_default().to_owned())
                        .collect(),
                    _ => Vec::new(),
                };
                let data: Vec<Value> = inputs
                    .iter()
                    .enumerate()
                    .map(|(index, text)| {
                        json!({
                            "object": "embedding",
                            "index": index,
                            "embedding": embedding_for(text),
                        })
                    })
                    .collect();
                let answer = serde_json::to_vec(&json!({
                    "object": "list",
                    "data": data,
                    "model": "embed-1",
                    "usage": {
                        "prompt_tokens": inputs.len(),
                        "total_tokens": inputs.len(),
                    },
                }))
                .unwrap();
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    answer,
                )
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    fn embedding_for(text: &str) -> Vec<f32> {
        let mut vector = vec![0.0f32; 8];
        if text.contains("alpha") {
            vector[0] = 1.0;
        } else if text.contains("beta") {
            vector[1] = 1.0;
        } else {
            vector[2] = 1.0;
        }
        vector
    }

    // --- Pure tests -------------------------------------------------------------------------

    #[test]
    fn chunk_keeps_every_piece_within_the_target() {
        let text = "a".repeat(5000);
        let chunks = chunk(&text);
        assert!(chunks.len() > 1);
        for piece in &chunks {
            assert!(!piece.is_empty());
            assert!(piece.chars().count() <= CHUNK_TARGET);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_of_empty_text_is_empty() {
        assert!(chunk("").is_empty());
        assert!(chunk("  \n\n\t ").is_empty());
    }

    #[test]
    fn chunk_of_emoji_only_text_does_not_split_a_code_point() {
        let text = "😀".repeat(5000);
        let chunks = chunk(&text);
        assert!(chunks.len() > 1);
        for piece in &chunks {
            assert!(piece.chars().count() <= CHUNK_TARGET);
        }
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn chunk_prefers_paragraphs_and_overlaps() {
        let paragraph = "alpha ".repeat(100);
        let text = format!("{paragraph}\n{paragraph}\n{paragraph}");
        let chunks = chunk(&text);
        assert!(chunks.len() >= 2);
        for piece in &chunks {
            assert!(piece.chars().count() <= CHUNK_TARGET);
        }
        let last_word = chunks[0].split_whitespace().last().unwrap();
        assert!(
            chunks[1].starts_with(last_word),
            "expected an overlap of the last word, got {:?}",
            &chunks[1][..40.min(chunks[1].len())]
        );
    }

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        let vector = [0.2, 0.4, 0.6];
        assert!((cosine(&vector, &vector) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_of_orthogonal_vectors_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn cosine_of_a_zero_vector_is_zero() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_of_mismatched_lengths_is_zero() {
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_never_returns_nan() {
        assert_eq!(cosine(&[f32::NAN], &[1.0]), 0.0);
        assert_eq!(cosine(&[f32::INFINITY], &[1.0]), 0.0);
    }

    #[test]
    fn store_and_file_ids_are_validated() {
        let id = new_store_id();
        assert!(is_store_id(&id));
        assert!(!is_store_id("vs_../"));
        assert!(!is_store_id("vs_ABCDEF"));
        assert!(!is_store_id(&format!("vs_{}", "a".repeat(23))));
        let batch = new_batch_id();
        assert!(batch.starts_with("vsfb_"));
        assert!(is_file_id(FILE_ALPHA));
        assert!(!is_file_id("file_../x"));
        assert!(!is_file_id("file_"));
    }

    // --- Resolution -------------------------------------------------------------------------

    #[tokio::test]
    async fn embedding_model_resolution_prefers_the_store_then_env_then_snapshot() {
        let env = TestEnv::new().await;
        assert_eq!(
            configured_embedding_model(&env.app, Some("explicit-model")).as_deref(),
            Some("explicit-model")
        );
        TEST_EMBEDDING_ENV.with(|slot| *slot.borrow_mut() = Some(Some("env-model".to_owned())));
        assert_eq!(
            configured_embedding_model(&env.app, None).as_deref(),
            Some("env-model")
        );
        TEST_EMBEDDING_ENV.with(|slot| *slot.borrow_mut() = Some(None));
        assert_eq!(
            configured_embedding_model(&env.app, None).as_deref(),
            Some("p/embed-1")
        );
    }

    #[tokio::test]
    async fn embedding_model_resolution_is_none_without_an_embeddings_provider() {
        let env = TestEnv::new_with(vec![Capability::Chat, Capability::Models]).await;
        assert!(configured_embedding_model(&env.app, None).is_none());
    }

    // --- Router tests -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_list_get_update_delete_a_store() {
        let env = TestEnv::new().await;
        let created = env
            .create(json!({
                "name": "docs",
                "metadata": {"team": "a"},
                "expires_after": {"anchor": "last_active_at", "days": 7},
            }))
            .await;
        let id = created["id"].as_str().unwrap().to_owned();
        assert_eq!(created["object"].as_str(), Some("vector_store"));
        assert_eq!(created["status"].as_str(), Some("completed"));
        assert_eq!(created["usage_bytes"].as_u64(), Some(0));
        assert_eq!(created["file_counts"]["total"].as_u64(), Some(0));
        assert_eq!(created["expires_after"]["days"].as_u64(), Some(7));
        assert_eq!(created["metadata"]["team"].as_str(), Some("a"));

        let (status, list) = env
            .call(Method::GET, "/v1/vector_stores", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["object"].as_str(), Some("list"));
        assert_eq!(list["data"].as_array().unwrap().len(), 1);
        assert_eq!(list["first_id"].as_str(), Some(id.as_str()));
        assert_eq!(list["last_id"].as_str(), Some(id.as_str()));
        assert_eq!(list["has_more"].as_bool(), Some(false));

        let (status, got) = env
            .call(Method::GET, &format!("/v1/vector_stores/{id}"), Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["name"].as_str(), Some("docs"));

        let (status, updated) = env
            .call(
                Method::POST,
                &format!("/v1/vector_stores/{id}"),
                json!({"name": "renamed", "metadata": {"team": "b"}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(updated["name"].as_str(), Some("renamed"));
        assert_eq!(updated["metadata"]["team"].as_str(), Some("b"));

        let (status, cleared) = env
            .call(
                Method::POST,
                &format!("/v1/vector_stores/{id}"),
                json!({"name": null, "metadata": {}, "expires_after": null}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(cleared["name"].is_null());
        assert_eq!(cleared["metadata"].as_object().unwrap().len(), 0);
        assert!(cleared["expires_after"].is_null());

        let (status, deleted) = env
            .call(
                Method::DELETE,
                &format!("/v1/vector_stores/{id}"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(deleted["object"].as_str(), Some("vector_store.deleted"));
        assert_eq!(deleted["deleted"].as_bool(), Some(true));

        let (status, _) = env
            .call(Method::GET, &format!("/v1/vector_stores/{id}"), Value::Null)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn attach_then_search_ranks_the_file() {
        let env = TestEnv::new().await;
        let store = env.create(json!({"name": "docs"})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha is a word. alpha appears here.");

        let (status, attached) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::OK, "{attached}");
        assert_eq!(attached["object"].as_str(), Some("vector_store.file"));
        assert_eq!(attached["status"].as_str(), Some("completed"));
        assert_eq!(attached["vector_store_id"].as_str(), Some(id.as_str()));
        assert!(attached["usage_bytes"].as_u64().unwrap() > 0);
        assert_eq!(
            attached["chunking_strategy"]["type"].as_str(),
            Some("static")
        );

        let (status, search) = env.search(&id, json!({"query": "alpha"})).await;
        assert_eq!(status, StatusCode::OK, "{search}");
        assert_eq!(
            search["object"].as_str(),
            Some("vector_store.search_results.page")
        );
        assert_eq!(search["search_query"].as_str(), Some("alpha"));
        let data = search["data"].as_array().unwrap();
        assert_eq!(data.len(), 1);
        assert_eq!(data[0]["file_id"].as_str(), Some(FILE_ALPHA));
        assert_eq!(data[0]["vector_store_id"].as_str(), Some(id.as_str()));
        assert_eq!(
            data[0]["filename"].as_str(),
            Some(format!("{FILE_ALPHA}.txt").as_str())
        );
        assert!((data[0]["score"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        assert!(data[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("alpha"));
        assert_eq!(search["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn detach_removes_the_file_from_search() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        let (status, _) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::OK);

        let (status, detached) = env
            .call(
                Method::DELETE,
                &format!("/v1/vector_stores/{id}/files/{FILE_ALPHA}"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            detached["object"].as_str(),
            Some("vector_store.file.deleted")
        );
        assert_eq!(detached["deleted"].as_bool(), Some(true));

        let (status, search) = env.search(&id, json!({"query": "alpha"})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(search["data"].as_array().unwrap().is_empty());

        let (status, _) = env
            .call(
                Method::GET,
                &format!("/v1/vector_stores/{id}/files/{FILE_ALPHA}"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn numeric_query_searches_without_an_embedder() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        let (status, _) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::OK);
        env.seen.lock().unwrap().clear();

        let (status, search) = env
            .search(
                &id,
                json!({"query": [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{search}");
        assert_eq!(search["data"][0]["file_id"].as_str(), Some(FILE_ALPHA));
        assert!(
            env.seen.lock().unwrap().is_empty(),
            "a numeric query must not call the embedder"
        );
    }

    #[tokio::test]
    async fn empty_store_search_returns_an_empty_page() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        let (status, search) = env.search(&id, json!({"query": "anything"})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(search["data"].as_array().unwrap().is_empty());
        assert_eq!(search["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn top_level_search_with_a_model_goes_to_the_provider() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        // The store exists, so a local search would answer 200; the provider branch must win and
        // report the capability gate instead.
        let (status, error) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({"model": "p/embed-1", "vector_store_id": id, "query": "alpha"}),
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{error}");
        assert_eq!(
            error["error"]["code"].as_str(),
            Some("unsupported_endpoint")
        );
    }

    #[tokio::test]
    async fn top_level_local_search_accepts_one_store_and_names_it() {
        let env = TestEnv::new().await;
        let store = env.create(json!({"name": "docs"})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        let (status, _) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::OK);

        let (status, page) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({"vector_store_id": id, "query": "alpha"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(
            page["object"].as_str(),
            Some("vector_store.search_results.page")
        );
        assert_eq!(page["search_query"].as_str(), Some("alpha"));
        assert_eq!(page["data"][0]["file_id"].as_str(), Some(FILE_ALPHA));
        assert_eq!(
            page["data"][0]["vector_store_id"].as_str(),
            Some(id.as_str())
        );
        assert_eq!(page["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn top_level_local_search_merges_several_stores() {
        let env = TestEnv::new().await;
        let first = env.create(json!({"name": "first"})).await;
        let first_id = first["id"].as_str().unwrap().to_owned();
        let second = env.create(json!({"name": "second"})).await;
        let second_id = second["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        env.write_file(FILE_BETA, "beta");
        assert_eq!(env.attach(&first_id, FILE_ALPHA).await.0, StatusCode::OK);
        assert_eq!(env.attach(&second_id, FILE_BETA).await.0, StatusCode::OK);

        let (status, page) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({"vector_store_ids": [first_id, second_id], "query": "alpha"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let data = page["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["file_id"].as_str(), Some(FILE_ALPHA));
        assert_eq!(data[0]["vector_store_id"].as_str(), Some(first_id.as_str()));
        assert_eq!(data[1]["file_id"].as_str(), Some(FILE_BETA));
        assert_eq!(
            data[1]["vector_store_id"].as_str(),
            Some(second_id.as_str())
        );
        assert!(data[0]["score"].as_f64().unwrap() > data[1]["score"].as_f64().unwrap());
        assert_eq!(page["has_more"].as_bool(), Some(false));
    }

    #[tokio::test]
    async fn top_level_search_refuses_an_ambiguous_or_numeric_query() {
        let env = TestEnv::new().await;
        let (status, error) = env
            .call(Method::POST, "/v1/search", json!({"query": "alpha"}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("model"), "{message}");
        assert!(message.contains("vector_store_id"), "{message}");
        assert!(message.contains("vector_store_ids"), "{message}");

        let (status, _) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({
                    "vector_store_id": "vs_0190a1b2c3d47e5f8a9b0c1d2e3f4a5b",
                    "vector_store_ids": ["vs_0190a1b2c3d47e5f8a9b0c1d2e3f4a5b"],
                    "query": "alpha",
                }),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, _) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({"vector_store_ids": [], "query": "alpha"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, error) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({"vector_store_id": "vs_0190a1b2c3d47e5f8a9b0c1d2e3f4a5b", "query": [1.0, 0.0]}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("numeric"));
    }

    #[tokio::test]
    async fn merged_search_refuses_stores_with_different_embedding_models() {
        let env = TestEnv::new().await;
        let first = env.create(json!({"embedding_model": "embed-one"})).await;
        let first_id = first["id"].as_str().unwrap().to_owned();
        let second = env.create(json!({"embedding_model": "embed-two"})).await;
        let second_id = second["id"].as_str().unwrap().to_owned();

        let (status, error) = env
            .call(
                Method::POST,
                "/v1/search",
                json!({"vector_store_ids": [first_id, second_id], "query": "alpha"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("not comparable"));
    }

    #[tokio::test]
    async fn attach_without_an_embedder_is_409() {
        let env = TestEnv::new_with(vec![Capability::Chat, Capability::Models]).await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");

        let (status, error) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            error["error"]["code"].as_str(),
            Some("unsupported_endpoint")
        );
        let message = error["error"]["message"].as_str().unwrap();
        assert!(message.contains("embedding_model"));
        assert!(message.contains("MANAROUTER_EMBEDDING_MODEL"));
        assert!(message.contains("embeddings"));

        let (status, _) = env.search(&id, json!({"query": "alpha"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, search) = env.search(&id, json!({"query": [1.0, 0.0]})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(search["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn attach_over_the_chunk_cap_is_413() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, &"a".repeat(2000));
        TEST_CAPS.with(|slot| slot.set(Some((1, MAX_TEXT_BYTES))));

        let (status, error) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error["error"]["code"].as_str(), Some("payload_too_large"));
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("denial-of-service"));
    }

    #[tokio::test]
    async fn unauthorised_requests_are_refused() {
        let env = TestEnv::new().await;
        let mut app = env.app.clone();
        app.no_key_required = false;
        // A presented key that the relay does not know is refused even in the bootstrap
        // configuration, where a *missing* key is deliberately open.
        let bogus = Some("sk-not-a-real-key");
        let (status, _) =
            TestEnv::call_with_key(&app, Method::GET, "/v1/vector_stores", Value::Null, bogus)
                .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) =
            TestEnv::call_with_key(&app, Method::POST, "/v1/vector_stores", json!({}), bogus).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = TestEnv::call_with_key(
            &app,
            Method::POST,
            "/v1/search",
            json!({"query": "alpha"}),
            bogus,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn file_batch_attaches_every_file_and_is_recorded() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        env.write_file(FILE_BETA, "beta");

        let (status, batch) = env
            .call(
                Method::POST,
                &format!("/v1/vector_stores/{id}/file_batches"),
                json!({"file_ids": [FILE_ALPHA, FILE_BETA]}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{batch}");
        assert_eq!(batch["object"].as_str(), Some("vector_store.file_batch"));
        assert_eq!(batch["status"].as_str(), Some("completed"));
        assert_eq!(batch["file_counts"]["completed"].as_u64(), Some(2));
        assert_eq!(batch["file_counts"]["total"].as_u64(), Some(2));
        let batch_id = batch["id"].as_str().unwrap().to_owned();

        let (status, got) = env
            .call(
                Method::GET,
                &format!("/v1/vector_stores/{id}/file_batches/{batch_id}"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got["id"].as_str(), Some(batch_id.as_str()));

        let (status, search) = env.search(&id, json!({"query": "beta"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(search["data"][0]["file_id"].as_str(), Some(FILE_BETA));
    }

    #[tokio::test]
    async fn duplicate_attach_is_rejected() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        let (status, _) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::OK);

        let (status, error) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("already attached"));

        let (status, _) = env
            .call(
                Method::POST,
                &format!("/v1/vector_stores/{id}/file_batches"),
                json!({"file_ids": [FILE_BETA, FILE_BETA]}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn expires_after_expiry_hides_the_store() {
        let env = TestEnv::new().await;
        TEST_NOW.with(|slot| slot.set(Some(1_000_000)));
        let store = env
            .create(json!({
                "expires_after": {"anchor": "last_active_at", "days": 1},
            }))
            .await;
        let id = store["id"].as_str().unwrap().to_owned();
        TEST_NOW.with(|slot| slot.set(Some(1_000_000 + 2 * 86_400)));

        let (status, _) = env
            .call(Method::GET, &format!("/v1/vector_stores/{id}"), Value::Null)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, list) = env
            .call(Method::GET, "/v1/vector_stores", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(list["data"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_pagination_and_filter() {
        let env = TestEnv::new().await;
        let first = env.create(json!({"name": "first"})).await;
        let second = env.create(json!({"name": "second"})).await;
        let third = env.create(json!({"name": "third"})).await;
        let ids: Vec<String> = [&first, &second, &third]
            .iter()
            .map(|store| store["id"].as_str().unwrap().to_owned())
            .collect();

        let (status, page) = env
            .call(Method::GET, "/v1/vector_stores?limit=2", Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page["data"].as_array().unwrap().len(), 2);
        assert_eq!(page["has_more"].as_bool(), Some(true));
        assert_eq!(page["data"][0]["id"].as_str(), Some(ids[2].as_str()));
        assert_eq!(page["data"][1]["id"].as_str(), Some(ids[1].as_str()));
        let last = page["last_id"].as_str().unwrap().to_owned();

        let (status, page2) = env
            .call(
                Method::GET,
                &format!("/v1/vector_stores?limit=2&after={last}"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(page2["data"].as_array().unwrap().len(), 1);
        assert_eq!(page2["has_more"].as_bool(), Some(false));
        assert_eq!(page2["data"][0]["id"].as_str(), Some(ids[0].as_str()));

        let (status, _) = env
            .call(
                Method::GET,
                "/v1/vector_stores?after=vs_000000000000000000000000",
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = env
            .call(Method::GET, "/v1/vector_stores?limit=0", Value::Null)
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn files_list_supports_filter_and_pagination() {
        let env = TestEnv::new().await;
        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        env.write_file(FILE_ALPHA, "alpha");
        let (status, _) = env.attach(&id, FILE_ALPHA).await;
        assert_eq!(status, StatusCode::OK);

        let (status, files) = env
            .call(
                Method::GET,
                &format!("/v1/vector_stores/{id}/files?filter=completed"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(files["data"].as_array().unwrap().len(), 1);
        assert_eq!(files["data"][0]["id"].as_str(), Some(FILE_ALPHA));

        let (status, files) = env
            .call(
                Method::GET,
                &format!("/v1/vector_stores/{id}/files?filter=in_progress"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(files["data"].as_array().unwrap().is_empty());

        let (status, _) = env
            .call(
                Method::GET,
                &format!("/v1/vector_stores/{id}/files?filter=bogus"),
                Value::Null,
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invalid_requests_are_refused_with_400() {
        let env = TestEnv::new().await;
        let (status, _) = env
            .call(
                Method::POST,
                "/v1/vector_stores",
                json!({"metadata": {"a": {"nested": true}}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let store = env.create(json!({})).await;
        let id = store["id"].as_str().unwrap().to_owned();
        let (status, _) = env
            .search(&id, json!({"query": "alpha", "filters": {"a": "b"}}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = env
            .search(
                &id,
                json!({"query": "alpha", "ranking_options": {"ranker": "bm25"}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = env
            .search(&id, json!({"query": "alpha", "max_num_results": 0}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = env.search(&id, json!({"query": []})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
