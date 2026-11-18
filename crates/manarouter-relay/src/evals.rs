//! The evals surface.
//!
//! An eval is a stored definition: a name, a data-source description, the criteria an answer is
//! graded against, and free-form metadata. A run is one execution of that definition over a list of
//! items. The run is synchronous: by the time `POST /v1/evals/{id}/runs` answers, every item has
//! been sent to the model under test through this relay's own request path, every answer has been
//! graded, and the finished run has been written to disk.
//!
//! ## Routes
//!
//! - `POST /v1/evals` creates a definition.
//! - `GET /v1/evals/{id}` returns it.
//! - `POST /v1/evals/{id}/runs` executes a run and returns the finished run.
//! - `GET /v1/evals/{id}/runs/{run_id}` returns a stored run.
//!
//! ## Where the model comes from
//!
//! The spec's run body carries no model, and this router does not invent one. Each item names its
//! model in `item["model"]`; when an item does not, the eval's `metadata.model` is used. If neither
//! names a model the whole run is refused with a `400` that says both places, rather than sending
//! an item to an arbitrary default.
//!
//! ## What is graded
//!
//! Two criterion types are implemented:
//!
//! - `string_check` compares a rendered template against a fixed reference (`eq`, `ne`, `like`,
//!   `ilike`).
//! - `label_model` asks a grader model to label the rendered input, and passes when the label the
//!   grader names is one of `passing_labels`.
//!
//! A criterion type this router does not implement is refused at run time with a `400` naming the
//! type and the two types above. Creation accepts the criteria as written so a definition can be
//! stored before its criteria are runnable; the run is where an unsupported type has to be caught,
//! because that is where grading would otherwise silently pass or silently fail an item.
//!
//! ## Storage
//!
//! Definitions live at `<state_dir>/evals/<eval_id>.json`; runs live at
//! `<state_dir>/evals/<eval_id>/runs/<run_id>.json`. Every write goes to a temporary file in the
//! same directory and is then renamed into place, so a reader never sees half a record and a
//! restart finds exactly the records that were completed. The state directory is the one
//! `mr_core::config::state_dir()` names, so the eval store moves with the rest of the relay's
//! state.
//!
//! Ids arrive in a URL, so every one is checked against `[A-Za-z0-9_-]{1,128}` before it is joined
//! to a path. That check is a security boundary, not a formatting nicety: without it a request
//! could walk out of the eval directory with `..` or a separator.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Path as RoutePath, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use rand::RngCore;
use serde_json::{json, Value};

use crate::batch::Refusal;
use crate::server::{authorize_batch_request, dispatch_internal, json_response, AppState};

/// The largest response body this module will read from an upstream.
///
/// A grading answer is a sentence, and an item's answer is a chat completion, so eight mebibytes is
/// far past anything legitimate. The cap exists because the relay reads the body into memory before
/// it can parse it, and an upstream that streams forever must not be able to exhaust the process.
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// The eval routes, ready to be merged into the relay's router.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/evals", post(create_eval))
        .route("/v1/evals/{id}", get(retrieve_eval))
        .route("/v1/evals/{id}/runs", post(create_run))
        .route("/v1/evals/{id}/runs/{run_id}", get(retrieve_run))
}

// -------------------------------------------------------------------------------------------
// Handlers
// -------------------------------------------------------------------------------------------

async fn create_eval(State(app): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    match create_eval_inner(&body).await {
        Ok(eval) => json_response(StatusCode::OK, eval),
        Err(refusal) => refusal.into_response(),
    }
}

async fn retrieve_eval(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    match load_eval(&id).await {
        Ok(eval) => json_response(StatusCode::OK, eval),
        Err(refusal) => refusal.into_response(),
    }
}

async fn create_run(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath(id): RoutePath<String>,
    body: Bytes,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    match create_run_inner(&app, &id, &body).await {
        Ok(run) => json_response(StatusCode::OK, run),
        Err(refusal) => refusal.into_response(),
    }
}

async fn retrieve_run(
    State(app): State<AppState>,
    headers: HeaderMap,
    RoutePath((id, run_id)): RoutePath<(String, String)>,
) -> Response {
    if let Some(denied) = authorize_batch_request(&app, &headers) {
        return denied;
    }
    match retrieve_run_inner(&id, &run_id).await {
        Ok(run) => json_response(StatusCode::OK, run),
        Err(refusal) => refusal.into_response(),
    }
}

// -------------------------------------------------------------------------------------------
// Definitions
// -------------------------------------------------------------------------------------------

async fn create_eval_inner(body: &[u8]) -> Result<Value, Refusal> {
    let request = parse_object(body, "eval")?;
    let name = required_str(&request, "name", "the eval body")?.to_owned();

    let data_source_config = request
        .get("data_source_config")
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| {
            Refusal::bad_request(
                "invalid_request_error",
                "the eval body needs a \"data_source_config\" object",
            )
        })?;

    let testing_criteria = request
        .get("testing_criteria")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Refusal::bad_request(
                "invalid_request_error",
                "the eval body needs a \"testing_criteria\" array",
            )
        })?;
    for (index, criterion) in testing_criteria.iter().enumerate() {
        if !criterion.is_object() {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!("testing_criteria[{index}] must be a JSON object"),
            ));
        }
    }

    let metadata = match request.get("metadata") {
        Some(value) if value.is_object() => value.clone(),
        None => json!({}),
        Some(_) => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "the eval body's \"metadata\" must be a JSON object",
            ))
        }
    };

    let id = new_id("eval_");
    let path = storage_root().join(format!("{id}.json"));
    let eval = json!({
        "id": id,
        "object": "eval",
        "created_at": now_seconds(),
        "name": name,
        "data_source_config": data_source_config,
        "testing_criteria": testing_criteria,
        "metadata": metadata,
    });
    write_json_async(path, eval.clone())
        .await
        .map_err(Refusal::internal)?;
    Ok(eval)
}

async fn load_eval(id: &str) -> Result<Value, Refusal> {
    if !valid_id(id) {
        return Err(Refusal::not_found("eval", id));
    }
    let path = storage_root().join(format!("{id}.json"));
    read_json_async(path)
        .await
        .map_err(Refusal::internal)?
        .ok_or_else(|| Refusal::not_found("eval", id))
}

// -------------------------------------------------------------------------------------------
// Runs
// -------------------------------------------------------------------------------------------

async fn create_run_inner(app: &AppState, eval_id: &str, body: &[u8]) -> Result<Value, Refusal> {
    let eval = load_eval(eval_id).await?;
    let request = parse_object(body, "eval run")?;
    validate_run_envelope(&request)?;

    // Criteria are parsed before any item is sent, so an unsupported criterion type refuses the
    // whole run instead of leaving a half-graded run behind.
    let criteria = parse_criteria(&eval)?;
    let items = load_run_items(app, &request).await?;

    // Every item's model is resolved before the first upstream call for the same reason: a run
    // that cannot name a model should not have called one for the items before it.
    let mut models = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        models.push(resolve_model(item, &eval, index)?);
    }

    // The chat body for each item is built before the run starts, so a malformed item is a `400`
    // on the request rather than an "errored" result that blames the model for the client's body.
    let mut messages = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        messages.push(messages_for_item(item).map_err(|message| {
            Refusal::bad_request(
                "invalid_request_error",
                format!("item {index} cannot be sent to a model: {message}"),
            )
        })?);
    }

    let (counts, results, usage) = execute_run(app, &items, &messages, &models, &criteria).await;

    // The run body carries no model, so the run names the first item's resolved model. Items may
    // name different models; `per_model_usage` is where that shows.
    let run_model = models
        .first()
        .cloned()
        .or_else(|| {
            eval.pointer("/metadata/model")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();

    let run_id = new_id("evalrun_");
    let path = storage_root()
        .join(eval_id)
        .join("runs")
        .join(format!("{run_id}.json"));
    let run = json!({
        "id": run_id,
        "object": "eval.run",
        "created_at": now_seconds(),
        "eval_id": eval_id,
        "status": "completed",
        "model": run_model,
        "result_counts": {
            "total": counts.total,
            "passed": counts.passed,
            "failed": counts.failed,
            "errored": counts.errored,
        },
        "per_model_usage": usage_json(&usage),
        "results": results,
        "error": Value::Null,
    });
    write_json_async(path, run.clone())
        .await
        .map_err(Refusal::internal)?;
    Ok(run)
}

async fn retrieve_run_inner(eval_id: &str, run_id: &str) -> Result<Value, Refusal> {
    // The eval has to exist before its run can: a run id under an unknown eval is a 404 for the
    // eval, not a run that happens to be missing.
    load_eval(eval_id).await?;
    if !valid_id(run_id) {
        return Err(Refusal::not_found("eval run", run_id));
    }
    let path = storage_root()
        .join(eval_id)
        .join("runs")
        .join(format!("{run_id}.json"));
    read_json_async(path)
        .await
        .map_err(Refusal::internal)?
        .ok_or_else(|| Refusal::not_found("eval run", run_id))
}

fn validate_run_envelope(request: &Value) -> Result<(), Refusal> {
    if let Some(name) = request.get("name") {
        if !name.is_string() {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "the eval run body's \"name\" must be a string",
            ));
        }
    }
    if let Some(metadata) = request.get("metadata") {
        if !metadata.is_object() {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "the eval run body's \"metadata\" must be a JSON object",
            ));
        }
    }
    Ok(())
}

async fn load_run_items(app: &AppState, request: &Value) -> Result<Vec<Value>, Refusal> {
    let data_source = request
        .get("data_source")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            Refusal::bad_request(
                "invalid_request_error",
                "the eval run body needs a \"data_source\" object",
            )
        })?;

    match data_source.get("type").and_then(Value::as_str) {
        Some("jsonl") => {}
        Some(other) => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                "data source type \"{other}\" is not supported; this router implements \"jsonl\""
            ),
            ))
        }
        None => {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                "the eval run body's \"data_source.type\" must be \"jsonl\"",
            ))
        }
    }

    let source = data_source
        .get("source")
        .filter(|value| value.is_object())
        .ok_or_else(|| {
            Refusal::bad_request(
                "invalid_request_error",
                "the eval run body needs a \"data_source.source\" object",
            )
        })?;

    match source.get("type").and_then(Value::as_str) {
        Some("file_content") => {
            let content = source
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        "a \"file_content\" source needs a \"content\" array",
                    )
                })?;
            let mut items = Vec::with_capacity(content.len());
            for (index, item) in content.iter().enumerate() {
                if !item.is_object() {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!("data_source.source.content[{index}] must be a JSON object"),
                    ));
                }
                items.push(item.clone());
            }
            Ok(items)
        }
        Some("file_id") => {
            let file_id = source.get("id").and_then(Value::as_str).ok_or_else(|| {
                Refusal::bad_request(
                    "invalid_request_error",
                    "a \"file_id\" source needs a string \"id\"",
                )
            })?;
            if !valid_id(file_id) {
                return Err(Refusal::not_found("file", file_id));
            }
            let bytes = read_batch_file(app, file_id).await?;
            parse_jsonl(&bytes)
        }
        Some(other) => Err(Refusal::bad_request(
            "invalid_request_error",
            format!(
                "data source source type \"{other}\" is not supported; this router implements \
                 \"file_id\" and \"file_content\""
            ),
        )),
        None => Err(Refusal::bad_request(
            "invalid_request_error",
            "the eval run body's \"data_source.source.type\" must be \"file_id\" or \
             \"file_content\"",
        )),
    }
}

/// Read the bytes of a batch-lane file.
///
/// The batch lane documents its bytes at `<state_dir>/batches/files/<file_id>.jsonl` (`batch.rs`),
/// and startup opens the lane at `<state_dir>/batches` (`startup.rs`), so that is the layout this
/// reads. `BatchLane::file_bytes` is private, and this module deliberately does not reach into the
/// lane for it; the lane's own root is tried first so a lane opened at another root still works,
/// and the documented state-directory path is the fallback.
async fn read_batch_file(app: &AppState, file_id: &str) -> Result<Vec<u8>, Refusal> {
    let name = format!("{file_id}.jsonl");
    let lane_path = app.batch.root().join("files").join(&name);
    let documented_path = mr_core::config::state_dir()
        .join("batches")
        .join("files")
        .join(&name);

    let mut candidates = vec![lane_path];
    if documented_path != candidates[0] {
        candidates.push(documented_path);
    }

    for path in candidates {
        let read = tokio::task::spawn_blocking(move || std::fs::read(&path))
            .await
            .map_err(|error| Refusal::internal(format!("the file read task failed: {error}")))?;
        match read {
            Ok(bytes) => return Ok(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(Refusal::internal(format!(
                    "could not read file \"{file_id}\": {error}"
                )))
            }
        }
    }
    Err(Refusal::not_found("file", file_id))
}

fn parse_jsonl(bytes: &[u8]) -> Result<Vec<Value>, Refusal> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("the eval data file is not UTF-8: {error}"),
        )
    })?;
    let mut items = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let item: Value = serde_json::from_str(line).map_err(|error| {
            Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "line {} of the eval data file is not JSON: {error}",
                    index + 1
                ),
            )
        })?;
        if !item.is_object() {
            return Err(Refusal::bad_request(
                "invalid_request_error",
                format!(
                    "line {} of the eval data file is not a JSON object",
                    index + 1
                ),
            ));
        }
        items.push(item);
    }
    Ok(items)
}

/// Resolve the model an item is sent to.
///
/// `item["model"]` wins over `eval.metadata.model`, because an item that names a model is asking
/// for that model specifically and the eval-level value is a default for items that do not. The
/// error names both places: the run body itself has no model field, so a caller who expected to
/// put one there needs to be told where this router looks.
fn resolve_model(item: &Value, eval: &Value, index: usize) -> Result<String, Refusal> {
    if let Some(model) = item.get("model").and_then(Value::as_str) {
        if !model.trim().is_empty() {
            return Ok(model.to_owned());
        }
    }
    if let Some(model) = eval.pointer("/metadata/model").and_then(Value::as_str) {
        if !model.trim().is_empty() {
            return Ok(model.to_owned());
        }
    }
    Err(Refusal::bad_request(
        "invalid_request_error",
        format!(
            "item {index} names no model: set \"model\" on the item or \"model\" under \
             eval.metadata; the run body carries no model of its own"
        ),
    ))
}

/// Build the chat body's `messages` for one item.
fn messages_for_item(item: &Value) -> Result<Value, String> {
    if let Some(messages) = item.get("messages") {
        if !messages.is_array() {
            return Err("item.messages must be an array of chat messages".to_owned());
        }
        return Ok(messages.clone());
    }
    match item.get("input") {
        Some(Value::String(text)) => Ok(json!([{"role": "user", "content": text}])),
        Some(Value::Array(messages)) => Ok(Value::Array(messages.clone())),
        Some(_) => Err("item.input must be a string or an array of chat messages".to_owned()),
        None => Err("the item has neither \"messages\" nor \"input\"".to_owned()),
    }
}

struct Counts {
    total: usize,
    passed: usize,
    failed: usize,
    errored: usize,
}

async fn execute_run(
    app: &AppState,
    items: &[Value],
    messages: &[Value],
    models: &[String],
    criteria: &[Criterion],
) -> (Counts, Vec<Value>, BTreeMap<String, Usage>) {
    let mut usage = BTreeMap::new();
    let mut results = Vec::with_capacity(items.len());
    let mut passed = 0usize;
    let mut errored = 0usize;

    for (index, item) in items.iter().enumerate() {
        let model = &models[index];
        let item_id = new_id("evalitem_");
        // A model that was called appears in `per_model_usage` even when the upstream reports no
        // usage for it, so the table names every model the run touched.
        usage.entry(model.clone()).or_default();

        let body = json!({"model": model, "messages": &messages[index]});
        let response = dispatch_internal(
            app,
            Method::POST,
            "/v1/chat/completions",
            serde_json::to_vec(&body).unwrap_or_default(),
        )
        .await;
        let status = response.status();
        let bytes = match axum::body::to_bytes(response.into_body(), MAX_RESPONSE_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                errored += 1;
                results.push(errored_entry(
                    item_id,
                    item,
                    "unreadable_response",
                    None,
                    format!("the model's response body could not be read: {error}"),
                ));
                continue;
            }
        };

        if !status.is_success() {
            let message = error_message(&bytes)
                .unwrap_or_else(|| format!("the model's upstream answered {status}"));
            errored += 1;
            results.push(errored_entry(
                item_id,
                item,
                "upstream_error",
                Some(status.as_u16()),
                message,
            ));
            continue;
        }

        let response_value: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(error) => {
                errored += 1;
                results.push(errored_entry(
                    item_id,
                    item,
                    "invalid_response",
                    Some(status.as_u16()),
                    format!("the model's upstream answered with invalid JSON: {error}"),
                ));
                continue;
            }
        };
        record_usage(&mut usage, model, &response_value);

        let output = match assistant_text(&response_value) {
            Some(text) => text,
            None => {
                errored += 1;
                results.push(errored_entry(
                    item_id,
                    item,
                    "no_assistant_message",
                    Some(status.as_u16()),
                    "the model's upstream answered without an assistant message".to_owned(),
                ));
                continue;
            }
        };

        let mut criterion_results = Vec::with_capacity(criteria.len());
        let mut all_passed = true;
        for criterion in criteria {
            let result = grade(app, criterion, &output, item, &mut usage).await;
            if result.get("passed").and_then(Value::as_bool) != Some(true) {
                all_passed = false;
            }
            criterion_results.push(result);
        }
        if all_passed {
            passed += 1;
        }

        results.push(json!({
            "id": item_id,
            "item": item,
            "sample": {"output": [{"role": "assistant", "content": output}]},
            "results": criterion_results,
        }));
    }

    let total = results.len();
    let failed = total.saturating_sub(passed + errored);
    (
        Counts {
            total,
            passed,
            failed,
            errored,
        },
        results,
        usage,
    )
}

fn errored_entry(
    id: String,
    item: &Value,
    code: &str,
    status: Option<u16>,
    message: String,
) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(status) = status {
        error["status"] = json!(status);
    }
    json!({
        "id": id,
        "item": item,
        "sample": {"output": []},
        "results": [],
        "error": error,
    })
}

// -------------------------------------------------------------------------------------------
// Criteria
// -------------------------------------------------------------------------------------------

#[derive(Debug)]
enum Criterion {
    StringCheck(CheckCriterion),
    LabelModel(LabelCriterion),
}

#[derive(Debug)]
struct CheckCriterion {
    name: String,
    input: Vec<TemplatePart>,
    reference: String,
    operation: Operation,
}

#[derive(Debug)]
struct LabelCriterion {
    name: String,
    model: String,
    input: Vec<TemplatePart>,
    prompt: String,
    labels: Vec<String>,
    /// Lower-cased, so matching a label is case-insensitive.
    passing: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Eq,
    Ne,
    Like,
    ILike,
}

impl Operation {
    fn compare(self, input: &str, reference: &str) -> bool {
        match self {
            Self::Eq => input == reference,
            Self::Ne => input != reference,
            Self::Like => input.contains(reference),
            Self::ILike => input.to_lowercase().contains(&reference.to_lowercase()),
        }
    }
}

fn parse_criteria(eval: &Value) -> Result<Vec<Criterion>, Refusal> {
    let criteria = eval
        .get("testing_criteria")
        .and_then(Value::as_array)
        .ok_or_else(|| Refusal::internal("the stored eval has no \"testing_criteria\" array"))?;
    criteria
        .iter()
        .enumerate()
        .map(|(index, criterion)| parse_criterion(criterion, index))
        .collect()
}

fn parse_criterion(value: &Value, index: usize) -> Result<Criterion, Refusal> {
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("criterion {}", index + 1));
    let kind = value.get("type").and_then(Value::as_str).ok_or_else(|| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("testing criterion \"{name}\" has no string \"type\""),
        )
    })?;

    match kind {
        "string_check" => {
            let input = criterion_template(value, "input", &name)?;
            let reference = value
                .get("reference")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" of type \"string_check\" needs a string \
                             \"reference\""
                        ),
                    )
                })?
                .to_owned();
            let operation = match value.get("operation").and_then(Value::as_str) {
                Some("eq") => Operation::Eq,
                Some("ne") => Operation::Ne,
                Some("like") => Operation::Like,
                Some("ilike") => Operation::ILike,
                Some(other) => {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" has operation \"{other}\"; this router \
                             implements \"eq\", \"ne\", \"like\" and \"ilike\""
                        ),
                    ))
                }
                None => {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" of type \"string_check\" needs a string \
                             \"operation\""
                        ),
                    ))
                }
            };
            Ok(Criterion::StringCheck(CheckCriterion {
                name,
                input,
                reference,
                operation,
            }))
        }
        "label_model" => {
            let model = value
                .get("model")
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" of type \"label_model\" needs a \
                             non-empty string \"model\""
                        ),
                    )
                })?
                .to_owned();
            let input = criterion_template(value, "input", &name)?;
            let prompt = match value.get("prompt") {
                Some(Value::String(prompt)) => prompt.clone(),
                Some(_) => {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!("testing criterion \"{name}\" has a non-string \"prompt\""),
                    ))
                }
                None => String::new(),
            };
            let raw_labels = value
                .get("labels")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" of type \"label_model\" needs a \
                             \"labels\" array"
                        ),
                    )
                })?;
            let mut labels = Vec::with_capacity(raw_labels.len());
            for label in raw_labels {
                let label = label
                    .as_str()
                    .filter(|label| !label.is_empty())
                    .ok_or_else(|| {
                        Refusal::bad_request(
                            "invalid_request_error",
                            format!(
                                "testing criterion \"{name}\" has a label that is not a non-empty \
                             string"
                            ),
                        )
                    })?;
                labels.push(label.to_owned());
            }
            if labels.is_empty() {
                return Err(Refusal::bad_request(
                    "invalid_request_error",
                    format!(
                        "testing criterion \"{name}\" of type \"label_model\" needs at least one \
                         label"
                    ),
                ));
            }
            let label_set: BTreeSet<String> =
                labels.iter().map(|label| label.to_lowercase()).collect();
            let raw_passing = match value.get("passing_labels") {
                Some(Value::Array(passing)) => passing.clone(),
                Some(_) => {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!("testing criterion \"{name}\" has a non-array \"passing_labels\""),
                    ))
                }
                None => vec![Value::String("pass".to_owned())],
            };
            let mut passing = Vec::with_capacity(raw_passing.len());
            for label in &raw_passing {
                let label = label.as_str().ok_or_else(|| {
                    Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" has a passing label that is not a string"
                        ),
                    )
                })?;
                let lower = label.to_lowercase();
                if !label_set.contains(&lower) {
                    return Err(Refusal::bad_request(
                        "invalid_request_error",
                        format!(
                            "testing criterion \"{name}\" names passing label \"{label}\", which \
                             is not one of its labels"
                        ),
                    ));
                }
                passing.push(lower);
            }
            Ok(Criterion::LabelModel(LabelCriterion {
                name,
                model,
                input,
                prompt,
                labels,
                passing,
            }))
        }
        other => Err(Refusal::bad_request(
            "invalid_request_error",
            format!(
                "testing criterion type \"{other}\" is not supported by this router; it \
                 implements \"string_check\" and \"label_model\""
            ),
        )),
    }
}

fn criterion_template(
    value: &Value,
    key: &str,
    criterion_name: &str,
) -> Result<Vec<TemplatePart>, Refusal> {
    let template = value.get(key).and_then(Value::as_str).ok_or_else(|| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("testing criterion \"{criterion_name}\" needs a string \"{key}\""),
        )
    })?;
    compile_template(template).map_err(|error| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("testing criterion \"{criterion_name}\": {error}"),
        )
    })
}

async fn grade(
    app: &AppState,
    criterion: &Criterion,
    sample_output: &str,
    item: &Value,
    usage: &mut BTreeMap<String, Usage>,
) -> Value {
    match criterion {
        Criterion::StringCheck(check) => {
            let rendered = render_template(&check.input, sample_output, item);
            criterion_result(
                &check.name,
                check.operation.compare(&rendered, &check.reference),
            )
        }
        Criterion::LabelModel(label) => {
            let rendered = render_template(&label.input, sample_output, item);
            let passed = label_model_passes(app, label, &rendered, usage).await;
            criterion_result(&label.name, passed)
        }
    }
}

fn criterion_result(name: &str, passed: bool) -> Value {
    json!({
        "name": name,
        "passed": passed,
        "score": if passed { 1.0 } else { 0.0 },
    })
}

async fn label_model_passes(
    app: &AppState,
    criterion: &LabelCriterion,
    input: &str,
    usage: &mut BTreeMap<String, Usage>,
) -> bool {
    usage.entry(criterion.model.clone()).or_default();
    let content = if criterion.prompt.is_empty() {
        input.to_owned()
    } else {
        format!("{}\n\n{}", criterion.prompt, input)
    };
    let body = json!({
        "model": criterion.model,
        "messages": [{"role": "user", "content": content}],
    });
    let response = dispatch_internal(
        app,
        Method::POST,
        "/v1/chat/completions",
        serde_json::to_vec(&body).unwrap_or_default(),
    )
    .await;
    let status = response.status();
    let bytes = match axum::body::to_bytes(response.into_body(), MAX_RESPONSE_BYTES).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(
                model = %criterion.model,
                "the label model's response body could not be read: {error}"
            );
            return false;
        }
    };
    if !status.is_success() {
        tracing::warn!(
            model = %criterion.model,
            %status,
            message = %error_message(&bytes).unwrap_or_default(),
            "the label model refused the grading request; the criterion fails"
        );
        return false;
    }
    let response_value: Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                model = %criterion.model,
                "the label model answered with invalid JSON: {error}"
            );
            return false;
        }
    };
    record_usage(usage, &criterion.model, &response_value);
    let text = match assistant_text(&response_value) {
        Some(text) => text,
        None => {
            tracing::warn!(
                model = %criterion.model,
                "the label model answered without an assistant message"
            );
            return false;
        }
    };
    match match_label(&text, &criterion.labels) {
        Some(label) => criterion
            .passing
            .iter()
            .any(|passing| label.to_lowercase() == passing.as_str()),
        None => {
            tracing::warn!(
                model = %criterion.model,
                "the label model named no configured label; the criterion fails"
            );
            false
        }
    }
}

/// The longest label the text contains, case-insensitively.
///
/// Longest wins so a short label cannot shadow a longer one that starts with it: with labels
/// `pass` and `passing`, the answer "passing" is `passing`, not `pass`. `None` means the grader
/// named no configured label, which is a failed criterion rather than an error.
fn match_label<'a>(text: &str, labels: &'a [String]) -> Option<&'a str> {
    let lower = text.to_lowercase();
    let mut best: Option<&'a str> = None;
    for label in labels {
        let label_lower = label.to_lowercase();
        if label_lower.is_empty() || !lower.contains(&label_lower) {
            continue;
        }
        if best.is_none_or(|current| label.len() > current.len()) {
            best = Some(label);
        }
    }
    best
}

// -------------------------------------------------------------------------------------------
// Templates
// -------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Literal(String),
    SampleOutputText,
    ItemField(String),
}

/// Compile a criterion's `input` template.
///
/// Exactly two placeholders are understood: `{{sample.output_text}}` and `{{item.<field>}}`. A
/// placeholder this router does not understand is an error rather than an empty string: silently
/// rendering it empty would grade against the wrong text and report a pass or fail that has
/// nothing to do with the criterion the operator wrote.
fn compile_template(template: &str) -> Result<Vec<TemplatePart>, String> {
    let mut parts = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        if open > 0 {
            parts.push(TemplatePart::Literal(rest[..open].to_owned()));
        }
        let after_open = &rest[open + 2..];
        let close = after_open.find("}}").ok_or_else(|| {
            format!("template placeholder starting at \"{after_open}\" has no closing \"}}\"")
        })?;
        let name = after_open[..close].trim();
        let part = match name {
            "sample.output_text" => TemplatePart::SampleOutputText,
            _ => match name.strip_prefix("item.") {
                Some(field)
                    if !field.is_empty()
                        && !field.contains(char::is_whitespace)
                        && !field.contains(['{', '}']) =>
                {
                    TemplatePart::ItemField(field.to_owned())
                }
                _ => {
                    return Err(format!(
                        "unknown template placeholder \"{name}\"; this router supports \
                         \"sample.output_text\" and \"item.<field>\""
                    ))
                }
            },
        };
        parts.push(part);
        rest = &after_open[close + 2..];
    }
    if !rest.is_empty() {
        parts.push(TemplatePart::Literal(rest.to_owned()));
    }
    Ok(parts)
}

fn render_template(parts: &[TemplatePart], sample_output: &str, item: &Value) -> String {
    let mut rendered = String::new();
    for part in parts {
        match part {
            TemplatePart::Literal(text) => rendered.push_str(text),
            TemplatePart::SampleOutputText => rendered.push_str(sample_output),
            TemplatePart::ItemField(field) => {
                rendered.push_str(&item_value_text(item.get(field)));
            }
        }
    }
    rendered
}

fn item_value_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// -------------------------------------------------------------------------------------------
// Chat responses and usage
// -------------------------------------------------------------------------------------------

/// The assistant's text from a chat completion.
fn assistant_text(response: &Value) -> Option<String> {
    let content = response.pointer("/choices/0/message/content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            let mut saw_part = false;
            for part in parts {
                if let Some(piece) = part.get("text").and_then(Value::as_str) {
                    saw_part = true;
                    text.push_str(piece);
                }
            }
            saw_part.then_some(text)
        }
        _ => None,
    }
}

fn error_message(body: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .map(str::to_owned)
}

#[derive(Default, Clone, Copy)]
struct Usage {
    input: u64,
    output: u64,
    total: u64,
}

fn record_usage(usage: &mut BTreeMap<String, Usage>, model: &str, response: &Value) {
    let entry = usage.entry(model.to_owned()).or_default();
    let Some(reported) = response.get("usage") else {
        return;
    };
    let input = reported
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = reported
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = reported
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input.saturating_add(output));
    entry.input = entry.input.saturating_add(input);
    entry.output = entry.output.saturating_add(output);
    entry.total = entry.total.saturating_add(total);
}

fn usage_json(usage: &BTreeMap<String, Usage>) -> Vec<Value> {
    usage
        .iter()
        .map(|(model, usage)| {
            json!({
                "model": model,
                "input_tokens": usage.input,
                "output_tokens": usage.output,
                "total_tokens": usage.total,
            })
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// Small shared helpers
// -------------------------------------------------------------------------------------------

fn parse_object(body: &[u8], what: &str) -> Result<Value, Refusal> {
    let value: Value = serde_json::from_slice(body).map_err(|error| {
        Refusal::bad_request(
            "invalid_request_error",
            format!("the {what} body is not valid JSON: {error}"),
        )
    })?;
    if !value.is_object() {
        return Err(Refusal::bad_request(
            "invalid_request_error",
            format!("the {what} body must be a JSON object"),
        ));
    }
    Ok(value)
}

fn required_str<'a>(object: &'a Value, key: &str, what: &str) -> Result<&'a str, Refusal> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            Refusal::bad_request(
                "invalid_request_error",
                format!("{what} needs a non-empty string \"{key}\""),
            )
        })
}

/// Whether an id is safe to join to a path.
///
/// This is the security boundary described in the module docs. Ids this router generates are hex,
/// but a URL id is whatever the caller typed, so it is checked before it reaches a filesystem path.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn new_id(prefix: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut id = String::with_capacity(prefix.len() + bytes.len() * 2);
    id.push_str(prefix);
    for byte in bytes {
        id.push(HEX[(byte >> 4) as usize] as char);
        id.push(HEX[(byte & 0x0f) as usize] as char);
    }
    id
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

// -------------------------------------------------------------------------------------------
// Storage
// -------------------------------------------------------------------------------------------

#[cfg(not(test))]
fn storage_root() -> PathBuf {
    mr_core::config::state_dir().join("evals")
}

/// A process-wide temporary root for tests.
///
/// The tests run in parallel, so pointing `MANAROUTER_STATE_DIR` at a per-test directory would race
/// with every other module's tests that set the same process-global variable. A `OnceLock` gives
/// this module one isolated root for the whole test binary, and the random ids keep the tests from
/// colliding inside it.
#[cfg(test)]
fn storage_root() -> PathBuf {
    use std::sync::OnceLock;

    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        tempfile::tempdir()
            .expect("a temporary directory for the eval store")
            .keep()
    })
    .clone()
}

async fn write_json_async(path: PathBuf, value: Value) -> Result<(), String> {
    tokio::task::spawn_blocking(move || write_json_atomic(&path, &value))
        .await
        .map_err(|error| format!("the eval write task failed: {error}"))?
}

/// Write a JSON record atomically: a temporary file in the destination directory, then a rename.
///
/// The rename is what makes a stored eval or run whole. A reader either sees the previous record or
/// the new one, never a partially written file, and a crash leaves at most a `.tmp` file that the
/// next successful write does not depend on.
fn write_json_atomic(path: &Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| format!("could not encode the eval record: {error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| "the eval record has no parent directory".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;

    let temp = parent.join(format!(".{}.tmp", new_id("write_")));
    let write = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("could not write the eval record: {error}"));
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("could not publish the eval record: {error}"));
    }
    Ok(())
}

async fn read_json_async(path: PathBuf) -> Result<Option<Value>, String> {
    let read = tokio::task::spawn_blocking(move || std::fs::read(&path))
        .await
        .map_err(|error| format!("the eval read task failed: {error}"))?;
    match read {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("a stored eval record is not valid JSON: {error}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("could not read the eval store: {error}")),
    }
}

// -------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::{header, Request};
    use tower::ServiceExt;

    // ---------------------------------------------------------------------------------------
    // Pure helpers
    // ---------------------------------------------------------------------------------------

    #[test]
    fn templates_render_the_sample_output_and_item_fields() {
        let parts = compile_template(
            "answer={{sample.output_text}} question={{item.question}} missing={{item.nope}}",
        )
        .expect("a valid template");
        let rendered = render_template(&parts, "yes", &json!({"question": "why", "count": 42}));
        assert_eq!(rendered, "answer=yes question=why missing=");

        let parts = compile_template("{{item.count}}").expect("a valid template");
        assert_eq!(render_template(&parts, "", &json!({"count": 42})), "42");
    }

    #[test]
    fn a_template_with_an_unknown_placeholder_is_refused() {
        let error = compile_template("{{sample.nope}}").expect_err("an unknown placeholder");
        assert!(error.contains("sample.nope"), "{error}");
        assert!(error.contains("sample.output_text"), "{error}");
        assert!(compile_template("{{item.}}").is_err());
        assert!(compile_template("{{unclosed").is_err());
    }

    #[test]
    fn string_check_operations_compare_as_documented() {
        assert!(Operation::Eq.compare("yes", "yes"));
        assert!(!Operation::Eq.compare("Yes", "yes"));
        assert!(Operation::Ne.compare("no", "yes"));
        assert!(Operation::Like.compare("a yes b", "yes"));
        assert!(!Operation::Like.compare("a no b", "yes"));
        assert!(Operation::ILike.compare("A YES B", "yes"));
        assert!(!Operation::ILike.compare("A NO B", "yes"));
    }

    #[test]
    fn label_matching_prefers_the_longest_label_and_no_match_is_none() {
        let labels = vec!["pass".to_owned(), "passing".to_owned()];
        assert_eq!(match_label("this is PASSING", &labels), Some("passing"));
        assert_eq!(match_label("this is pass", &labels), Some("pass"));
        assert_eq!(match_label("maybe", &labels), None);
    }

    #[test]
    fn ids_are_validated_before_they_touch_a_path() {
        assert!(valid_id("eval_abc-123"));
        assert!(valid_id(&"a".repeat(128)));
        assert!(!valid_id(""));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a/b"));
        assert!(!valid_id("a b"));
        assert!(!valid_id(&"a".repeat(129)));
    }

    #[test]
    fn the_item_model_wins_and_the_eval_metadata_model_is_the_fallback() {
        let eval = json!({"metadata": {"model": "test/eval-model"}});
        assert_eq!(
            resolve_model(&json!({"model": "test/item-model"}), &eval, 0)
                .ok()
                .as_deref(),
            Some("test/item-model")
        );
        assert_eq!(
            resolve_model(&json!({}), &eval, 0).ok().as_deref(),
            Some("test/eval-model")
        );
        assert!(resolve_model(&json!({}), &json!({"metadata": {}}), 3).is_err());
    }

    #[test]
    fn usage_sums_each_model_and_derives_total_when_it_is_absent() {
        let mut usage = BTreeMap::new();
        record_usage(
            &mut usage,
            "test/model",
            &json!({"usage": {"prompt_tokens": 2, "completion_tokens": 3}}),
        );
        record_usage(
            &mut usage,
            "test/model",
            &json!({"usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 5}}),
        );
        record_usage(&mut usage, "test/grader", &json!({}));

        assert_eq!(usage["test/model"].input, 3);
        assert_eq!(usage["test/model"].output, 4);
        assert_eq!(usage["test/model"].total, 10);
        assert_eq!(usage["test/grader"].total, 0);
        assert_eq!(
            usage_json(&usage),
            vec![
                json!({"model": "test/grader", "input_tokens": 0, "output_tokens": 0, "total_tokens": 0}),
                json!({"model": "test/model", "input_tokens": 3, "output_tokens": 4, "total_tokens": 10}),
            ]
        );
    }

    #[test]
    fn an_unsupported_criterion_type_is_refused_by_the_parser() {
        assert!(parse_criterion(&json!({"type": "python"}), 0).is_err());
        assert!(parse_criterion(&json!({"type": "string_check"}), 0).is_err());
        assert!(parse_criterion(
            &json!({
                "type": "label_model",
                "model": "test/grader",
                "input": "{{sample.output_text}}",
                "labels": ["pass", "fail"],
                "passing_labels": ["maybe"],
            }),
            0,
        )
        .is_err());
    }

    // ---------------------------------------------------------------------------------------
    // Router-level tests
    // ---------------------------------------------------------------------------------------

    async fn fake_upstream<F>(respond: F) -> (String, Arc<Mutex<Vec<Value>>>)
    where
        F: Fn(&Value) -> (StatusCode, Value) + Send + Sync + 'static,
    {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        // The handler closure has to be `Clone`, and a closure that captures `respond` by value is
        // only `Clone` when `respond` is. An `Arc` makes the closure cloneable whatever `F` is.
        let respond = Arc::new(respond);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let address = listener.local_addr().expect("an address");
        let app = Router::new().route(
            "/v1/chat/completions",
            axum::routing::any(move |body: Bytes| {
                let recorder = Arc::clone(&recorder);
                let respond = Arc::clone(&respond);
                async move {
                    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    recorder.lock().expect("the recorder").push(value.clone());
                    let (status, answer) = respond(&value);
                    (
                        status,
                        [(header::CONTENT_TYPE, "application/json")],
                        answer.to_string(),
                    )
                }
            }),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{address}/v1"), seen)
    }

    fn model_entry() -> mr_core::model::ModelEntry {
        mr_core::model::ModelEntry {
            id: String::new(),
            source: mr_core::model::ModelSource::Manual,
            label: None,
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_modalities: vec![mr_core::dialect::InputModality::Text],
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

    fn provider(base_url: &str) -> mr_core::model::Provider {
        let mut provider =
            mr_core::model::Provider::new("test", mr_core::Dialect::OpenAi, base_url);
        let mut account = mr_core::model::Account::new("test");
        // An account without an explicit reference falls back to a conventional vault path, so the
        // test names one explicitly and puts a value there.
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        provider.models = ["test-model", "grader-model"]
            .into_iter()
            .map(|id| {
                let mut entry = model_entry();
                entry.id = id.to_owned();
                entry
            })
            .collect();
        provider
    }

    fn app_with(base_url: &str, require_key: bool) -> AppState {
        let mut config = mr_core::config::Config {
            providers: vec![provider(base_url)],
            ..Default::default()
        };
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

        // A vault in a private temporary directory, holding the one credential the account names.
        let vault_dir = tempfile::tempdir().expect("a vault directory");
        let mut opened = mr_core::secret::Vault::open(vault_dir.path()).expect("a vault");
        opened
            .put("test-credential", "sk-not-a-real-key")
            .expect("a test credential");
        let vault = Arc::new(opened);

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
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            batch: crate::batch::BatchLane::inert(),
        }
    }

    async fn call(
        app: &AppState,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let payload = match body {
            Some(body) => serde_json::to_vec(&body).expect("a JSON body"),
            None => Vec::new(),
        };
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload))
            .expect("a request");
        let response = routes()
            .with_state(app.clone())
            .oneshot(request)
            .await
            .expect("the router answered");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn create_eval(app: &AppState, criteria: Value, metadata: Value) -> (StatusCode, Value) {
        call(
            app,
            Method::POST,
            "/v1/evals",
            Some(json!({
                "name": "test eval",
                "data_source_config": {"type": "custom", "item_schema": {"type": "object"}},
                "testing_criteria": criteria,
                "metadata": metadata,
            })),
        )
        .await
    }

    async fn start_run(app: &AppState, eval_id: &str, source: Value) -> (StatusCode, Value) {
        call(
            app,
            Method::POST,
            &format!("/v1/evals/{eval_id}/runs"),
            Some(json!({
                "name": "test run",
                "data_source": {"type": "jsonl", "source": source},
                "metadata": {},
            })),
        )
        .await
    }

    fn file_content(items: Value) -> Value {
        json!({"type": "file_content", "content": items})
    }

    fn completion(content: &str, prompt_tokens: u64, completion_tokens: u64) -> Value {
        json!({
            "id": "cmpl-test",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop",
            }],
            "usage": {
                "prompt_tokens": prompt_tokens,
                "completion_tokens": completion_tokens,
                "total_tokens": prompt_tokens + completion_tokens,
            },
        })
    }

    fn message_text(body: &Value) -> &str {
        body.pointer("/messages/0/content")
            .and_then(Value::as_str)
            .unwrap_or("")
    }

    fn exact_yes() -> Value {
        json!([{
            "type": "string_check",
            "name": "exact",
            "input": "{{sample.output_text}}",
            "reference": "yes",
            "operation": "eq",
        }])
    }

    #[tokio::test]
    async fn a_run_over_file_content_passes_and_fails_the_right_items() {
        let (url, seen) = fake_upstream(|body| {
            let answer = if message_text(body).contains("first") {
                "yes"
            } else {
                "no"
            };
            (StatusCode::OK, completion(answer, 3, 2))
        })
        .await;
        let app = app_with(&url, false);

        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        assert_eq!(eval["object"], "eval");
        assert!(eval["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("eval_")));
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, run) = start_run(
            &app,
            eval_id,
            file_content(json!([{"input": "first"}, {"input": "second"}])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(run["object"], "eval.run");
        assert_eq!(run["eval_id"], eval_id);
        assert_eq!(run["status"], "completed");
        assert_eq!(run["model"], "test/test-model");
        assert_eq!(
            run["result_counts"],
            json!({"total": 2, "passed": 1, "failed": 1, "errored": 0})
        );
        assert_eq!(run["results"][0]["sample"]["output"][0]["content"], "yes");
        assert_eq!(run["results"][0]["results"][0]["passed"], true);
        assert_eq!(run["results"][0]["results"][0]["score"], 1.0);
        assert_eq!(run["results"][1]["results"][0]["passed"], false);
        assert_eq!(run["results"][1]["results"][0]["score"], 0.0);
        assert_eq!(
            run["per_model_usage"],
            json!([{
                "model": "test/test-model",
                "input_tokens": 6,
                "output_tokens": 4,
                "total_tokens": 10,
            }])
        );

        let seen = seen.lock().expect("the recorder");
        assert_eq!(seen.len(), 2);
        // The relay receives `provider/model`; the provider receives the bare model id.
        assert_eq!(seen[0]["model"], "test-model");
        assert_eq!(seen[0]["messages"][0]["content"], "first");
    }

    #[tokio::test]
    async fn a_run_over_a_file_id_reads_the_lane_bytes() {
        let (url, _seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let file = app
            .batch
            .put_file(
                br#"{"input":"from a file"}"#.to_vec(),
                Some("items.jsonl".to_owned()),
                Some("batch".to_owned()),
            )
            .await
            .ok()
            .expect("a stored file");
        let file_id = file["id"].as_str().expect("a file id").to_owned();

        let (status, run) =
            start_run(&app, eval_id, json!({"type": "file_id", "id": file_id})).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(
            run["result_counts"],
            json!({"total": 1, "passed": 1, "failed": 0, "errored": 0})
        );
        assert_eq!(run["results"][0]["sample"]["output"][0]["content"], "yes");
    }

    #[tokio::test]
    async fn an_upstream_error_is_counted_errored_and_the_run_still_completes() {
        let (url, _seen) = fake_upstream(|body| {
            if message_text(body).contains("boom") {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": {"message": "upstream exploded", "type": "server_error"}}),
                )
            } else {
                (StatusCode::OK, completion("yes", 1, 1))
            }
        })
        .await;
        let app = app_with(&url, false);
        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, run) = start_run(
            &app,
            eval_id,
            file_content(json!([{"input": "fine"}, {"input": "boom"}])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(run["status"], "completed");
        assert_eq!(
            run["result_counts"],
            json!({"total": 2, "passed": 1, "failed": 0, "errored": 1})
        );
        assert_eq!(run["results"][1]["error"]["code"], "upstream_error");
        assert_eq!(run["results"][1]["error"]["status"], 500);
        assert!(run["results"][1]["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("upstream exploded")));
        assert_eq!(run["results"][1]["sample"]["output"], json!([]));
        assert_eq!(
            run["per_model_usage"],
            json!([{
                "model": "test/test-model",
                "input_tokens": 1,
                "output_tokens": 1,
                "total_tokens": 2,
            }])
        );
    }

    #[tokio::test]
    async fn a_label_model_criterion_asks_the_grader_and_matches_its_label() {
        let (url, seen) = fake_upstream(|body| {
            if message_text(body).contains("Grade this answer.") {
                (StatusCode::OK, completion("pass", 4, 1))
            } else {
                (StatusCode::OK, completion("the answer", 2, 3))
            }
        })
        .await;
        let app = app_with(&url, false);
        let criteria = json!([{
            "type": "label_model",
            "name": "quality",
            "model": "test/grader-model",
            "input": "{{sample.output_text}}",
            "labels": ["pass", "fail"],
            "passing_labels": ["pass"],
            "prompt": "Grade this answer.",
        }]);
        let (status, eval) = create_eval(&app, criteria, json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, run) = start_run(
            &app,
            eval_id,
            file_content(json!([{"input": "say something"}])),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(
            run["result_counts"],
            json!({"total": 1, "passed": 1, "failed": 0, "errored": 0})
        );
        assert_eq!(run["results"][0]["results"][0]["name"], "quality");
        assert_eq!(run["results"][0]["results"][0]["passed"], true);
        assert_eq!(
            run["per_model_usage"],
            json!([
                {"model": "test/grader-model", "input_tokens": 4, "output_tokens": 1, "total_tokens": 5},
                {"model": "test/test-model", "input_tokens": 2, "output_tokens": 3, "total_tokens": 5},
            ])
        );

        let seen = seen.lock().expect("the recorder");
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0]["model"], "test-model");
        assert_eq!(seen[1]["model"], "grader-model");
        let grader_prompt = message_text(&seen[1]);
        assert!(
            grader_prompt.contains("Grade this answer."),
            "{grader_prompt}"
        );
        assert!(grader_prompt.contains("the answer"), "{grader_prompt}");
    }

    #[tokio::test]
    async fn a_label_model_with_no_matching_label_fails_the_criterion() {
        let (url, _seen) = fake_upstream(|body| {
            if message_text(body).contains("Grade") {
                (StatusCode::OK, completion("maybe", 1, 1))
            } else {
                (StatusCode::OK, completion("an answer", 1, 1))
            }
        })
        .await;
        let app = app_with(&url, false);
        let criteria = json!([{
            "type": "label_model",
            "name": "quality",
            "model": "test/grader-model",
            "input": "{{sample.output_text}}",
            "labels": ["pass", "fail"],
            "prompt": "Grade",
        }]);
        let (status, eval) = create_eval(&app, criteria, json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, run) = start_run(&app, eval_id, file_content(json!([{"input": "x"}]))).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(
            run["result_counts"],
            json!({"total": 1, "passed": 0, "failed": 1, "errored": 0})
        );
        assert_eq!(run["results"][0]["results"][0]["passed"], false);
        assert_eq!(run["results"][0]["results"][0]["score"], 0.0);
    }

    #[tokio::test]
    async fn unknown_evals_and_runs_are_not_found() {
        let (url, _seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);

        let (status, body) = call(&app, Method::GET, "/v1/evals/eval_missing", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("eval_missing")));

        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");
        let (status, body) = call(
            &app,
            Method::GET,
            &format!("/v1/evals/{eval_id}/runs/evalrun_missing"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("evalrun_missing")));
    }

    #[tokio::test]
    async fn an_unsupported_criterion_type_is_refused_at_run_time() {
        let (url, seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let criteria = json!([{"type": "python", "name": "code"}]);
        let (status, eval) = create_eval(&app, criteria, json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, body) = start_run(&app, eval_id, file_content(json!([{"input": "x"}]))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(message.contains("python"), "{message}");
        assert!(message.contains("string_check"), "{message}");
        assert!(message.contains("label_model"), "{message}");
        assert!(seen.lock().expect("the recorder").is_empty());
    }

    #[tokio::test]
    async fn a_missing_model_is_refused_naming_both_places() {
        let (url, seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let (status, eval) = create_eval(&app, exact_yes(), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, body) = start_run(&app, eval_id, file_content(json!([{"input": "x"}]))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(message.contains("model"), "{message}");
        assert!(message.contains("eval.metadata"), "{message}");
        assert!(seen.lock().expect("the recorder").is_empty());
    }

    #[tokio::test]
    async fn a_template_with_an_unknown_placeholder_is_refused_at_run_time() {
        let (url, seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let criteria = json!([{
            "type": "string_check",
            "name": "bad template",
            "input": "{{sample.nope}}",
            "reference": "yes",
            "operation": "eq",
        }]);
        let (status, eval) = create_eval(&app, criteria, json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, body) = start_run(&app, eval_id, file_content(json!([{"input": "x"}]))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(message.contains("sample.nope"), "{message}");
        assert!(message.contains("sample.output_text"), "{message}");
        assert!(seen.lock().expect("the recorder").is_empty());
    }

    #[tokio::test]
    async fn an_eval_and_its_run_survive_a_new_router() {
        let (url, _seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id").to_owned();
        let (status, run) = start_run(&app, &eval_id, file_content(json!([{"input": "x"}]))).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        let run_id = run["id"].as_str().expect("a run id").to_owned();

        // A fresh state and a fresh router, reading the same store: this is the restart case.
        let reopened = app_with(&url, false);
        let (status, stored_eval) = call(
            &reopened,
            Method::GET,
            &format!("/v1/evals/{eval_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{stored_eval}");
        assert_eq!(stored_eval, eval);

        let (status, stored_run) = call(
            &reopened,
            Method::GET,
            &format!("/v1/evals/{eval_id}/runs/{run_id}"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{stored_run}");
        assert_eq!(stored_run, run);
    }

    #[tokio::test]
    async fn a_missing_file_is_not_found() {
        let (url, _seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, body) = start_run(
            &app,
            eval_id,
            json!({"type": "file_id", "id": "file_missing"}),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("file_missing")));
    }

    #[tokio::test]
    async fn an_eval_route_requires_a_client_key_when_keys_are_configured() {
        let (url, _seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, true);

        let (status, body) = call(
            &app,
            Method::POST,
            "/v1/evals",
            Some(json!({
                "name": "test eval",
                "data_source_config": {"type": "custom"},
                "testing_criteria": [],
            })),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body["error"].is_object(), "{body}");
    }

    #[tokio::test]
    async fn an_empty_file_is_a_completed_run_with_no_items() {
        let (url, seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let (status, run) = start_run(&app, eval_id, file_content(json!([]))).await;
        assert_eq!(status, StatusCode::OK, "{run}");
        assert_eq!(
            run["result_counts"],
            json!({"total": 0, "passed": 0, "failed": 0, "errored": 0})
        );
        assert_eq!(run["results"], json!([]));
        assert_eq!(run["per_model_usage"], json!([]));
        assert_eq!(run["model"], "test/test-model");
        assert!(seen.lock().expect("the recorder").is_empty());
    }

    #[tokio::test]
    async fn a_run_body_that_is_not_json_is_a_400_not_a_panic() {
        let (url, _seen) = fake_upstream(|_body| (StatusCode::OK, completion("yes", 1, 1))).await;
        let app = app_with(&url, false);
        let (status, eval) =
            create_eval(&app, exact_yes(), json!({"model": "test/test-model"})).await;
        assert_eq!(status, StatusCode::OK, "{eval}");
        let eval_id = eval["id"].as_str().expect("an eval id");

        let response = routes()
            .with_state(app.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/v1/evals/{eval_id}/runs"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("not json"))
                    .expect("a request"),
            )
            .await
            .expect("the router answered");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
