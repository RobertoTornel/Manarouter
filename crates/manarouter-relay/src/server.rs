//! The relay's HTTP surface: the route table, the request path, and the accept loop.
//!
//! # The shape of a request
//!
//! ```text
//! accept -> auth -> targeted model scan -> snapshot lookup -> step order -> account select
//!        -> upstream -> stream back
//! ```
//!
//! Every stage after `accept` is bounded work with no lock, no filesystem access, and no allocation
//! that is not needed to hold the bytes. The budget in `docs/16-latency-engineering.md` §6 assigns
//! each stage a cost, and this module is where those costs are paid.
//!
//! # The hedged race
//!
//! A combo whose hedge is enabled races its participants: every participant is dispatched at once,
//! and the highest-priority participant that produced a usable answer inside the eligibility
//! window wins. The policy — who wins, and when — lives in [`crate::select::hedge_decide`]; the
//! runner that dispatches the participants and commits the winner lives here.
//!
//! What the race does with a winner that dies mid-stream lives in [`crate::recovery`]. The commit
//! happens when the upstream response arrives, before the body streams, so the relay wraps the
//! winner's body: a failure after that point is recovered from a participant that already passed
//! the race, under the combo's `midstream_policy`. All four `WinnerFailMode`s are implemented:
//! `pick-highest-passed` (the default) spends no extra upstream work because the participants that
//! passed the race are already in hand, `pick-highest-passed-different-provider` filters those same
//! participants to another provider, and `rerun-excluding` and `stable-fallback` re-dispatch, bounded
//! by `max_rounds` (one round for `stable-fallback`).

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::Body as AxumBody;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Json;
use axum::Router;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, BodyStream, StreamBody};
use mr_core::dialect::{AuthKind, Capability, Dialect, InputModality, ReasoningLevel, RouteKind};
use mr_core::ids::ProviderId;
use mr_core::oauth::platform as oauth_platform;
use mr_core::oauth::refresh::Refresher;
use mr_core::secret::Vault;
use mr_core::snapshot::{AccountPlan, ProviderPlan, Resolution, StepPlan};

use mr_dialects::{translate_request, TranslateCtx};

use crate::model_scan::{multipart_boundary, scan_multipart_model, scan_request_head, RequestHead};
use crate::proxy::{self, ProxyEndpoint};
use crate::recovery::RerunFactory;
use crate::select::{self, Health};
use crate::state::{AuthOutcome, ProxyLease, RelayState, RouteTable};
use crate::upstream::{UpstreamError, UpstreamRequest};

/// How much of a request body the relay will buffer.
///
/// Audio and image uploads are legitimately large and a chat request is not. 32 MiB is comfortably
/// above the largest realistic transcription and well below anything that would exhaust memory on a
/// machine that is also running inference.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

// Both bounds are compile-time facts, so they are asserted at compile time rather than in a test:
// a test that fails tells you after a build, and this tells you during one.
const _: () = assert!(
    MAX_BODY_BYTES >= 16 * 1024 * 1024,
    "the limit must admit a realistic audio transcription"
);
const _: () = assert!(
    MAX_BODY_BYTES <= 64 * 1024 * 1024,
    "the limit must not be unbounded, or one client can exhaust memory"
);

/// Why a request body could not be read.
///
/// A distinct type rather than a `Response`, because the response is more than a hundred bytes and
/// returning one through a `Result` makes every caller's frame that large for a case that does not
/// happen on the happy path.
pub(crate) enum BodyError {
    /// The body exceeded [`MAX_BODY_BYTES`].
    TooLarge,
    /// The body could not be read at all.
    Read(String),
}

impl BodyError {
    /// The client-facing response.
    pub(crate) fn into_response(self) -> Response {
        match self {
            Self::TooLarge => error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                Dialect::OpenAi,
                "payload_too_large",
                &format!("the request body exceeds {MAX_BODY_BYTES} bytes"),
            ),
            Self::Read(e) => error_response(
                StatusCode::BAD_REQUEST,
                Dialect::OpenAi,
                "invalid_request_error",
                &format!("the request body could not be read: {e}"),
            ),
        }
    }
}

/// Everything a handler needs beyond the shared state.
#[derive(Clone)]
pub struct AppState {
    /// The routing table, client keys and upstream client.
    pub relay: Arc<RelayState>,
    /// Permit absent client credentials even when keys are configured.
    pub no_key_required: bool,
    /// Account health, as the relay has observed it.
    pub health: Arc<Health>,
    /// The credential vault.
    ///
    /// Opened once at startup rather than per request: it decrypts a file, and putting disk I/O and
    /// an AEAD operation between a client and its answer is exactly the cost this architecture
    /// exists to remove.
    pub vault: Arc<Vault>,
    /// Per-platform refreshers over the shared vault.
    pub oauth_refreshers: Arc<std::collections::HashMap<String, Arc<Refresher>>>,
    /// Metadata lookup for OAuth references attached to snapshot accounts.
    pub oauth_store: Arc<mr_core::store::Store>,
    /// Persistent request/usage database for GPU billing records.
    pub quota_store: Arc<mr_core::store::Store>,
    /// Per-key quota admission and settlement.
    ///
    /// The ledger it reads and writes is the same `key_usage` table the panel reads, so a cap that
    /// is being enforced and a cap that is being displayed are the same number.
    pub quota: Arc<crate::quota::QuotaManager>,
    /// Bounded combo-wide successful-step memory; reset when the relay process restarts.
    pub sticky_success: Arc<crate::sticky::StickySuccess>,
    /// The last few combo decisions, for the panel's live view.
    ///
    /// An `Arc` swapped nowhere and locked briefly: it is read by `GET /control/activity` and
    /// written once per combo request, so it costs one uncontended lock on a path that has
    /// already done several orders of magnitude more work.
    pub activity: Arc<crate::activity::ActivityLog>,
    /// Local provider admission plus GPU-second ledger handles.
    pub kv_usage: Arc<crate::kv_budget::AdmissionPool>,
    /// The batch lane: files, batches, the worker pool and the cost report.
    ///
    /// Held here, next to the admission pool it shares, because a batch item is dispatched **through
    /// the request path** — the lane decides *when* an item runs and this process decides *where* it
    /// goes — and the lane's executor needs the state to reach the routing snapshot and the upstream
    /// client.
    pub batch: Arc<crate::batch::BatchLane>,
}

/// Build the route table.
///
/// The paths are OpenAI's and Anthropic's, not ours. A client is configured with a base URL and
/// expects the vendor's routes; inventing our own would mean every client needs an adapter, which
/// defeats the purpose of speaking the vendor's dialect.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/v1/chat/completions", any(dispatch_openai))
        .route("/v1/responses", any(dispatch_openai))
        .route("/v1/embeddings", any(dispatch_openai))
        .route("/v1/audio/transcriptions", any(dispatch_openai))
        .route("/v1/audio/translations", any(dispatch_openai))
        .route("/v1/audio/speech", any(dispatch_openai))
        .route("/v1/images/generations", any(dispatch_openai))
        .route("/v1/images/edits", any(dispatch_openai))
        .route("/v1/images/variations", any(dispatch_openai))
        .route("/v1/moderations", any(dispatch_openai))
        // Videos: the create names its model in the body and goes through the ordinary path; the
        // two reads name the render by id in the path, so they are addressed by capability.
        .route("/v1/videos", any(dispatch_openai))
        .route("/v1/videos/{*rest}", any(video_read))
        .route("/v1/realtime/client_secrets", post(realtime_session))
        .route("/v1/realtime/calls", post(realtime_session))
        // Fine-tuning is a job API: the client names no model, so every path in the family is
        // addressed by capability. `/v1/search` is deliberately **not** mounted here — the
        // vector-store module owns it and merges its own router; registering it twice makes
        // `Router::merge` panic at startup.
        .route("/v1/fine_tuning/jobs", any(fine_tuning))
        .route("/v1/fine_tuning/jobs/{*rest}", any(fine_tuning))
        .route("/v1/containers", any(containers))
        .route("/v1/containers/{*rest}", any(containers))
        .route("/v1/messages", any(dispatch_anthropic))
        .route("/v1/messages/count_tokens", any(dispatch_anthropic))
        .route("/v1/models", any(list_models))
        // A model id in the listing is `provider/model`, which contains a `/`, so the wildcard is
        // the only parameter shape that can name every listed model. It does not shadow
        // `/v1/models`, which has no trailing segment.
        .route(
            "/v1/models/{*model}",
            get(retrieve_model).delete(delete_model),
        )
        // The batch lane. OpenAI's file and batch surfaces, mounted on the same listener because a
        // batch is submitted with the same client key that calls a model. Axum 0.8 spells a path
        // parameter `{id}`, not `:id`.
        .route(
            "/v1/files",
            post(crate::batch::create_file).get(crate::batch::list_files),
        )
        .route(
            "/v1/files/{id}",
            get(crate::batch::retrieve_file).delete(crate::batch::delete_file),
        )
        .route("/v1/files/{id}/content", get(crate::batch::file_content))
        .route(
            "/v1/batches",
            post(crate::batch::create_batch).get(crate::batch::list_batches),
        )
        .route("/v1/batches/{id}", get(crate::batch::retrieve_batch))
        .route("/v1/batches/{id}/cancel", post(crate::batch::cancel_batch))
        .route("/control/probe", post(control_probe))
        .route("/control/activity", get(control_activity))
        // The families that keep their own state rather than forwarding it. Each module owns its
        // paths and returns a router without state; merging them before `.with_state` is what lets
        // one listener answer both halves. `Router::merge` panics on a path registered twice, so a
        // duplicate is a startup failure with a line number rather than a silently shadowed route —
        // which is why `/v1/search` lives in `vector_stores` alone and is not repeated above.
        .merge(crate::responses::routes())
        .merge(crate::uploads::routes())
        .merge(crate::evals::routes())
        .merge(crate::vector_stores::routes())
        .merge(crate::realtime::routes())
        .with_state(state)
}

/// Liveness.
///
/// Answers without consulting any provider. A health check that a model provider can fail is a health
/// check that reports the *provider's* health, and when that provider is down the router looks down
/// while it is working perfectly.
async fn healthz() -> impl IntoResponse {
    (
        StatusCode::OK,
        [("content-type", "application/json")],
        r#"{"status":"ok"}"#,
    )
}

/// What the control plane asks a probe to run.
#[derive(Debug, serde::Deserialize)]
struct ProbeRequest {
    /// The model or combo name, exactly as a client would send it.
    model: String,
    /// The prompt. Kept tiny: a probe answers "did this route work", not "is this model clever".
    #[serde(default)]
    prompt: Option<String>,
}

/// The one completion a probe asks for.
///
/// **No output cap on purpose.** The probe used to send `max_tokens: 32`, which is only safe for a
/// model that answers with its first token: a model that thinks by default spends the whole
/// allowance in its head, finishes `MAX_TOKENS`, and returns nothing a client could read.
/// Antigravity's `gemini-3.8-flash-tiered` did exactly that — 27 of the 32 tokens were thoughts, and
/// one run in four came back with no `parts` at all — so the panel reported a successful test with a
/// blank answer and called a working route broken. The prompt is one short phrase, so an uncapped
/// probe still costs a handful of tokens, and the route now gets the same budget a real client would
/// rather than a ceiling the probe invented.
fn probe_payload(model: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    })
}

/// `POST /control/probe` — run one real request for the control plane.
///
/// The panel's "test model" and the combo health check used to speak the OpenAI shape themselves,
/// which meant they could only ever test OpenAI-shaped dialects and refused everything else. That
/// made the checks useless for exactly the routes worth checking (Codex, Antigravity, Command
/// Code). This endpoint runs the request through the same dispatch a client uses — routing table,
/// dialect translation, account selection, cooldowns, fallbacks — and returns the ordinary OpenAI
/// response, so a check means "this route answers right now" rather than "this route is shaped
/// like the tester".
///
/// **Loopback only.** The instance binds a LAN or tailnet interface in normal use, and this route
/// deliberately skips the client-key check, so anything that is not a local caller is refused
/// before the body is read. A local process already runs as the operator and can read the vault.
async fn control_probe(
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    State(app): State<AppState>,
    Json(body): Json<ProbeRequest>,
) -> Response {
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            r#"{"error":{"code":"probe_not_local","message":"the probe endpoint only accepts loopback callers"}}"#,
        )
            .into_response();
    }
    let prompt = body
        .prompt
        .filter(|prompt| !prompt.trim().is_empty())
        .unwrap_or_else(|| "Reply with the single word OK.".to_owned());
    let payload = probe_payload(&body.model, &prompt);
    let Ok(bytes) = serde_json::to_vec(&payload) else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let Ok(uri) = "/v1/chat/completions".parse::<Uri>() else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    dispatch_routed(
        DispatchRequest {
            app,
            client_dialect: Dialect::OpenAi,
            method: Method::POST,
            uri,
            headers,
            body: AxumBody::from(bytes),
            pre_authorized: true,
            pre_admitted: None,
        },
        0,
        None,
    )
    .await
}

/// `GET /control/activity` — the relay's own view of what its combos are doing.
///
/// The panel asks this while a request is streaming, so the answer has to come from the process
/// that made the decision: only the relay knows which step it committed to, and only the body it
/// handed to the client knows when it stopped being in flight.
///
/// **Loopback only**, for the same reason as the probe: it bypasses the client-key check, and the
/// relay may bind a LAN or tailnet interface. What it returns is bounded and boring on purpose —
/// combo and model names, the step chosen, timestamps, and the account ids currently cooling down.
/// No credential, no label, no URL, no request body.
async fn control_activity(
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    State(app): State<AppState>,
) -> Response {
    if !peer.ip().is_loopback() {
        return (
            StatusCode::FORBIDDEN,
            [("content-type", "application/json")],
            r#"{"error":{"code":"activity_not_local","message":"the activity endpoint only accepts loopback callers"}}"#,
        )
            .into_response();
    }
    let entries: Vec<serde_json::Value> = app
        .activity
        .snapshot()
        .iter()
        .map(crate::activity::Entry::to_json)
        .collect();
    // The cooldown map is reported as the relay holds it, expired-but-not-yet-cleared entries
    // included: the relay's own selection uses `until_ms` against its clock, and hiding a stale
    // entry here would make this view disagree with the decisions it is meant to explain.
    let cooldowns = crate::select::Health::snapshot(&app.health);
    json_response(
        StatusCode::OK,
        serde_json::json!({ "entries": entries, "cooldowns": cooldowns }),
    )
}

/// The OpenAI-shaped surface.
async fn dispatch_openai(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    dispatch(app, Dialect::OpenAi, method, uri, headers, body).await
}

/// The Anthropic-shaped surface.
async fn dispatch_anthropic(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    dispatch(app, Dialect::Anthropic, method, uri, headers, body).await
}

/// `GET /v1/models`.
///
/// Served **entirely from the compiled snapshot**. No provider is contacted, so this route cannot be
/// slow and cannot fail because a provider is down. The reference router took 5.2–9.2 seconds here
/// with zero upstream work, which is the measurement that motivated this project.
///
/// ## What is listed
///
/// Every name a client can actually put in a `model` field, which is what makes this route useful
/// as a first check:
///
/// * each combo name,
/// * each combo name with the reasoning levels a model can be pinned to,
/// * `provider/model` for every visible model,
/// * `provider/model-<level>` for every **level that model really offers**.
///
/// A model that advertises no reasoning levels is listed exactly once, under its own id — which is
/// what stops a client picking `...-high` and discovering at request time that the suffix was part
/// of the model's name. The reverse is also deliberate: a level the model does *not* offer is not
/// listed, even though requesting it succeeds by rerouting to the model's highest. Advertising it
/// would be advertising a tier that is not this model's.
fn model_listing(snapshot: &mr_core::snapshot::RoutingSnapshot) -> Vec<serde_json::Value> {
    let mut data: Vec<serde_json::Value> = Vec::new();

    for combo in snapshot.combos() {
        data.push(serde_json::json!({
            "id": combo.name,
            "object": "model",
            "created": 0,
            "owned_by": "manarouter",
            "manarouter": {
                "kind": "combo",
                "strategy": combo.strategy.as_str(),
                "steps": combo.steps.len(),
            },
        }));
        // The levels a combo can be pinned to. Deduplicated across steps: a two-step combo offers
        // `coding-high` **once**, not once per step, and the suffix attaches to the combo name, so
        // it is the *name* that must not already end in a level word — not the step's model.
        for level in combo_listed_levels(combo) {
            data.push(serde_json::json!({
                "id": format!("{}-{}", combo.name, level.as_str()),
                "object": "model",
                "created": 0,
                "owned_by": "manarouter",
                "manarouter": {
                    "kind": "combo",
                    "strategy": combo.strategy.as_str(),
                    "steps": combo.steps.len(),
                    "reasoning_level": level.as_str(),
                },
            }));
        }
    }

    for provider in snapshot.providers() {
        for step in &provider.steps {
            let base = format!("{}/{}", provider.id, step.model);
            data.push(serde_json::json!({
                "id": base,
                "object": "model",
                "created": 0,
                "owned_by": provider.id.as_str(),
                "manarouter": {
                    "kind": "provider",
                    "dialect": provider.dialect.as_str(),
                    "default_reasoning_level": ReasoningLevel::from_u8(step.default_level)
                        .map(|l| l.as_str())
                        .unwrap_or("none"),
                    "reasoning_levels": step
                        .levels()
                        .iter()
                        .map(|l| l.as_str())
                        .collect::<Vec<_>>(),
                    "input_modalities": listed_modalities(step),
                },
            }));
            // Only levels this model genuinely offers, and never for a model whose own id already
            // ends in a level word: there, `-low` is part of the name, so listing
            // `...-low-high` would offer a name nothing resolves.
            for level in listed_levels(step) {
                data.push(serde_json::json!({
                    "id": format!("{base}-{}", level.as_str()),
                    "object": "model",
                    "created": 0,
                    "owned_by": provider.id.as_str(),
                    "manarouter": {
                        "kind": "provider",
                        "dialect": provider.dialect.as_str(),
                        "reasoning_level": level.as_str(),
                    },
                }));
            }
        }
    }

    data
}

/// The reasoning levels to advertise as `-<level>` suffixes for a step.
///
/// One shared definition, used for both providers and combos, so the two listings cannot disagree
/// about what a level suffix means.
fn listed_levels(step: &StepPlan) -> Vec<ReasoningLevel> {
    if ends_in_level_word(&step.model) {
        // A model whose own id ends in a level word already *is* a tier. Adding suffixes would
        // list names that resolve to nothing, because the exact-model-id rule deliberately wins
        // over the suffix reading.
        return Vec::new();
    }
    step.levels()
}

/// The levels a combo can be pinned to, deduplicated across its steps.
///
/// A combo named `coding` is pinnable to a level when **any** of its steps offers one, because a
/// combo with a tier is asking "run this route at `<level>`" and each step then uses that level or
/// its own highest. The union is what makes `coding-high` a name that resolves, so the union is
/// what gets listed. Duplicates are impossible by construction of the bitmask.
///
/// The suffix attaches to the *combo's* name, so the guard is about that name: a combo whose own
/// name is `fast-low` must not be listed as `fast-low-high`, for exactly the reason a model called
/// `...-low` must not be. The step model's name is irrelevant here — that is a different name.
fn combo_listed_levels(combo: &mr_core::snapshot::ComboPlan) -> Vec<ReasoningLevel> {
    if ends_in_level_word(&combo.name) {
        return Vec::new();
    }
    // The union of every step's levels. Taken straight from `step.levels()` rather than through
    // `listed_levels`, because the suppression that function applies is about a *model's* own id
    // being a tier — irrelevant when the suffix is being attached to the combo's name instead.
    let mut mask = 0u32;
    for step in &combo.steps {
        for level in step.levels() {
            mask |= 1u32 << level.rank();
        }
    }
    ReasoningLevel::ALL
        .into_iter()
        .filter(|level| mask & (1u32 << level.rank()) != 0)
        .collect()
}

/// Whether `name` ends in `-<level>` for one of the level words.
fn ends_in_level_word(name: &str) -> bool {
    ReasoningLevel::ALL.into_iter().any(|level| {
        let suffix = level.as_str();
        name.len() > suffix.len()
            && name.ends_with(suffix)
            && name.as_bytes()[name.len() - suffix.len() - 1] == b'-'
    })
}

/// The input modalities a step accepts, for the listing's metadata.
fn listed_modalities(step: &StepPlan) -> Vec<&'static str> {
    InputModality::ALL
        .into_iter()
        .filter(|modality| step.accepts_modality(*modality))
        .map(|modality| modality.as_str())
        .collect()
}

async fn list_models(State(app): State<AppState>, headers: HeaderMap) -> Response {
    let presented = presented_key(&headers);
    let auth = authenticate_request(&app.relay, app.no_key_required, presented.as_deref());
    if !auth.is_allowed() {
        return auth_error(&auth, Dialect::OpenAi);
    }

    let table = app.relay.table();
    let data = model_listing(&table.snapshot);

    json_response(
        StatusCode::OK,
        serde_json::json!({ "object": "list", "data": data }),
    )
}

/// `GET /v1/models/{model}`.
///
/// One row of exactly the listing `GET /v1/models` serves, found by id. Building the row from the
/// same function is what keeps the two routes consistent: a name the list shows is a name this
/// route returns, with the same shape, the same `owned_by` and the same `manarouter` metadata, and
/// a name the list does not show is a 404. The listing is compiled from the snapshot, so no provider
/// is contacted and this cannot be slow because a provider is down.
async fn retrieve_model(
    State(app): State<AppState>,
    Path(model): Path<String>,
    headers: HeaderMap,
) -> Response {
    let presented = presented_key(&headers);
    let auth = authenticate_request(&app.relay, app.no_key_required, presented.as_deref());
    if !auth.is_allowed() {
        return auth_error(&auth, Dialect::OpenAi);
    }

    let table = app.relay.table();
    // A wildcard capture may keep the separator that the route pattern consumed; the listing never
    // contains it, so the name is compared without it either way.
    let model = model.strip_prefix('/').unwrap_or(&model);
    match model_listing(&table.snapshot)
        .into_iter()
        .find(|row| row.get("id").and_then(|id| id.as_str()) == Some(model))
    {
        Some(row) => json_response(StatusCode::OK, row),
        None => error_response(
            StatusCode::NOT_FOUND,
            Dialect::OpenAi,
            "model_not_found",
            &format!("the model \"{model}\" does not exist"),
        ),
    }
}

/// `DELETE /v1/models/{model}`.
///
/// OpenAI's surface allows this for a **fine-tuned** model, where deleting it removes the fine-tune
/// at the provider that owns it. Manarouter can create a fine-tune job, but it keeps no record of
/// which listed model came from one, and a base model cannot be deleted anywhere, so there is
/// nothing this route could honestly delete. Answering locally says so; forwarding the DELETE would
/// ask a provider to remove a model the operator configured by hand, which is not a request a proxy
/// should invent.
///
/// TODO(fine-tuning): to forward this the relay would need (1) a `RouteKind` for the delete — none
/// exists, and inventing one changes the capability table — (2) the model name carried in the path
/// rather than a JSON body, which `requested_model` and the step scan do not read, and (3) a
/// persisted record of which listed model is a fine-tune of which provider model. Until all three
/// exist, the local refusal below is the truthful answer.
async fn delete_model(
    State(app): State<AppState>,
    Path(model): Path<String>,
    headers: HeaderMap,
) -> Response {
    let presented = presented_key(&headers);
    let auth = authenticate_request(&app.relay, app.no_key_required, presented.as_deref());
    if !auth.is_allowed() {
        return auth_error(&auth, Dialect::OpenAi);
    }

    let table = app.relay.table();
    let model = model.strip_prefix('/').unwrap_or(&model);
    // An unknown name is a 404, the same as the read route: saying "this model is not a fine-tune"
    // about a model that does not exist would be a second, less useful error.
    if !model_listing(&table.snapshot)
        .iter()
        .any(|row| row.get("id").and_then(|id| id.as_str()) == Some(model))
    {
        return error_response(
            StatusCode::NOT_FOUND,
            Dialect::OpenAi,
            "model_not_found",
            &format!("the model \"{model}\" does not exist"),
        );
    }

    error_response(
        StatusCode::BAD_REQUEST,
        Dialect::OpenAi,
        "invalid_request_error",
        "deleting a model is only possible for a fine-tuned model on a provider that serves fine-tuning, and this model is not one",
    )
}

/// What a [`BodyShaper`] may see about the attempt it is shaping a body for.
///
/// Every field is unread today, because both shapers are the identity. They are not speculative:
/// they are exactly the inputs a staging implementation needs, and they are handed over on every
/// attempt so filling the seam does not change this file's plumbing.
#[allow(dead_code)]
///
/// It is the whole attempt, not just the bytes: a shaper that stages something upstream needs the
/// provider's address, the account's credential and proxy, and the same upstream client every other
/// request uses — otherwise it would have to guess at auth, placeholders or egress, and guess wrong.
struct ShapeTarget<'a> {
    /// The shared state, for the upstream client and the vault.
    app: &'a AppState,
    /// The provider that will run the request.
    provider: &'a ProviderPlan,
    /// The step whose base URL, timeouts and model id describe how to reach it.
    step: &'a StepPlan,
    /// The account the request will be sent as, whose placeholders the URL was built with.
    account: &'a AccountPlan,
    /// The account's API key, when it has one. An OAuth account authenticates through headers that
    /// [`upstream_headers`] has already applied; a shaper that needs to dial the provider itself
    /// must do the same, and `credential` is `None` for it.
    credential: Option<&'a str>,
    /// The proxy this attempt egresses through, when one is configured.
    proxy: Option<&'a ProxyEndpoint>,
}

/// A body shaper's answer: the bytes to send, possibly after an upstream round trip.
type ShapedBody<'a> = Pin<Box<dyn Future<Output = Bytes> + Send + 'a>>;

/// The one place a capability-addressed body is turned into the body the provider receives.
///
/// It is `async` because the honest implementations are: staging a training file means uploading it
/// to the provider and substituting the id that comes back. It is a plain function pointer so the
/// set of behaviours is closed and visible at the call sites, rather than a boxed closure chosen at
/// runtime.
type BodyShaper = for<'a> fn(Bytes, ShapeTarget<'a>) -> ShapedBody<'a>;

/// Send the body exactly as the client sent it.
fn passthrough_body<'a>(body: Bytes, _target: ShapeTarget<'a>) -> ShapedBody<'a> {
    Box::pin(async move { body })
}

/// The fields a fine-tuning request names its training data with.
const FINE_TUNING_FILE_FIELDS: [&str; 2] = ["training_file", "validation_file"];

/// The largest answer `/v1/files` may send back while a file is being staged.
///
/// The answer is an object with an id in it. The cap exists so a provider that answers an upload with
/// a stream of something cannot hold the relay's task open while it is buffered.
const MAX_STAGE_ANSWER_BYTES: usize = 64 * 1024;

/// The fine-tuning seam: the one place a fine-tuning request body becomes the provider's body.
///
/// A fine-tuning job names its data by a **provider** file id. The relay serves `/v1/files` itself, so
/// the id a client gets back is a local one (`file_…`) that no upstream can resolve. Staging closes
/// that gap: each id that names a file this relay holds is uploaded to the resolved step's
/// `/v1/files` as multipart with `purpose=fine-tune`, the id that comes back replaces it in the body,
/// and the job is created against bytes the provider can actually read. The body bytes are otherwise
/// untouched — every other field of the request reaches the provider exactly as the client wrote it.
///
/// What it deliberately does **not** do:
///
/// * An id the relay does not hold is left alone. It may be a file the client uploaded to the
///   provider directly, which is a perfectly good way to use this route, and rewriting it would break
///   a request that works.
/// * A failed upload leaves the local id in place and logs why. The shaper's answer is the body to
///   send and nothing else — it cannot turn the request into a relay error — so the alternative to
///   forwarding is dropping the request, and a provider's own "unknown file" 400 names the id and is
///   at least true about where the problem is.
/// * An account whose credential is not a header (`OAuth`, `QueryParam`) is not staged: the upload
///   needs the same authentication the job would, and a second authentication scheme invented here
///   would be a different credential path from the one the operator configured. The job request is
///   forwarded as it arrived.
///
/// It is called once per attempt, immediately before the request is built.
fn fine_tuning_body<'a>(body: Bytes, target: ShapeTarget<'a>) -> ShapedBody<'a> {
    Box::pin(stage_fine_tuning_files(body, target))
}

/// Rewrite the training-data ids in a fine-tuning body to the ids the provider assigned them.
async fn stage_fine_tuning_files(body: Bytes, target: ShapeTarget<'_>) -> Bytes {
    let Some(mut request) = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .filter(serde_json::Value::is_object)
    else {
        return body;
    };
    let mut staged = false;
    for field in FINE_TUNING_FILE_FIELDS {
        let Some(local_id) = request
            .get(field)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        // Not one of ours: a provider file id the client already has, and the provider is the only
        // party that can resolve it.
        let Some(bytes) = target.app.batch.file_bytes(&local_id).await else {
            continue;
        };
        let filename = target
            .app
            .batch
            .file_name(&local_id)
            .unwrap_or_else(|| format!("{local_id}.jsonl"));
        match upload_training_file(&target, &filename, bytes).await {
            Ok(provider_id) => {
                request[field] = serde_json::Value::String(provider_id);
                staged = true;
            }
            Err(reason) => tracing::warn!(
                field,
                file = %local_id,
                %reason,
                "a training file could not be staged; the provider is being asked for a local id"
            ),
        }
    }
    if !staged {
        return body;
    }
    serde_json::to_vec(&request)
        .map(Bytes::from)
        .unwrap_or(body)
}

/// `POST /v1/files` on the resolved step, as multipart, returning the id it assigned.
async fn upload_training_file(
    target: &ShapeTarget<'_>,
    filename: &str,
    bytes: Vec<u8>,
) -> Result<String, String> {
    let Some(credential) = target.credential else {
        return Err("the account has no credential to authenticate the upload with".to_owned());
    };
    if matches!(
        target.step.auth,
        AuthKind::OAuth { .. } | AuthKind::QueryParam { .. }
    ) {
        return Err(
            "this account's authentication cannot be presented on a file upload".to_owned(),
        );
    }
    let Some(url) = build_url(target.step, UpstreamPath::Dialect("/v1/files")) else {
        return Err("the provider's base URL is empty".to_owned());
    };
    let boundary = format!("manarouter-{}", mr_core::ids::new_id());
    let mut payload = Vec::with_capacity(bytes.len() + 512);
    payload.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"purpose\"\r\n\r\nfine-tune\r\n"
        )
        .as_bytes(),
    );
    payload.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/jsonl\r\n\r\n"
        )
        .as_bytes(),
    );
    payload.extend_from_slice(&bytes);
    payload.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let content_type = HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}"))
        .map_err(|_| "the multipart boundary is not a valid header value".to_owned())?;
    let mut client_headers = HeaderMap::new();
    client_headers.insert(axum::http::header::CONTENT_TYPE, content_type);
    client_headers.insert(
        axum::http::header::ACCEPT,
        HeaderValue::from_static("application/json"),
    );
    let head = RequestHead {
        model: None,
        stream: None,
        n: None,
    };
    let request = UpstreamRequest {
        method: Method::POST,
        url,
        headers: upstream_headers(
            target.provider,
            target.step,
            Some(credential),
            &client_headers,
            &head,
            false,
            None,
            &[],
        ),
        proxy: target.proxy.cloned(),
        body: Bytes::from(payload),
        http2: target.provider.http2,
        connect_timeout: Duration::from_millis(target.step.connect_timeout_ms),
        // An upload spends its time in the request body, not in the provider's thinking: the answer
        // starts only after the last byte arrives, so the first-byte deadline must not be the
        // generation timeout applied to a request that is still being written.
        ttfb_timeout: Duration::from_millis(target.step.ttfb_timeout_ms.max(300_000)),
        total_timeout: Some(Duration::from_millis(
            target.step.total_timeout_ms.max(600_000),
        )),
    };
    let response = target
        .app
        .relay
        .upstream()
        .send(request)
        .await
        .map_err(|error| format!("the upload could not be sent: {error}"))?;
    let status = response.status;
    let answer = http_body_util::Limited::new(response.body, MAX_STAGE_ANSWER_BYTES)
        .collect()
        .await
        .map_err(|error| format!("the upload's answer could not be read: {error}"))?
        .to_bytes();
    if !status.is_success() {
        return Err(format!(
            "the provider answered {status} to the upload: {}",
            String::from_utf8_lossy(&answer[..answer.len().min(512)])
        ));
    }
    let answer: serde_json::Value = serde_json::from_slice(&answer)
        .map_err(|_| "the upload's answer was not JSON".to_owned())?;
    answer
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "the upload's answer carried no file id".to_owned())
}

/// One parameter from a query string.
///
/// Only `provider` is ever read, and only as a provider slug, so a full percent-decoder would be
/// more machinery than the single caller needs.
fn query_provider(query: Option<&str>) -> Option<String> {
    let query = query?;
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == "provider" && !value.is_empty()).then(|| value.to_owned())
    })
}

/// The OpenAI routes whose `model` field is optional, and the capability a provider must declare to
/// serve them when a client leaves it out.
///
/// Each of these has a documented default that the *provider* applies: `omni-moderation-latest` for
/// moderations, `dall-e-2` for image edits and variations. A client that omits the field is asking
/// for that default, so the honest answer is to forward the request to a provider that declares the
/// capability and let it apply its own default — refusing with "no model" would be a 400 on a route
/// this router advertises.
///
/// The list is deliberately exactly three routes, matched on the route kind rather than on a path
/// prefix, and every other route keeps the 400: chat, responses, embeddings, audio and video all
/// require a model, and substituting one there would be a lie about what ran.
fn model_optional_capability(route: RouteKind) -> Option<Capability> {
    match route {
        RouteKind::Moderations => Some(Capability::Moderations),
        RouteKind::ImagesEdits => Some(Capability::ImagesEdits),
        RouteKind::ImagesVariations => Some(Capability::ImagesVariations),
        _ => None,
    }
}

/// The least credit, in US cents, that a fine-tuning account may hold and still be asked to
/// start a job.
///
/// Fifteen dollars is a floor on the **balance**, not a spend limit: a training run can outlive
/// the request that started it, so an account that is about to be refused is worse than no
/// account at all. "Over fifteen" is strict, because the operator said over.
const FINE_TUNING_CREDIT_FLOOR_CENTS: u64 = 1_500;

/// How old a cached USD balance may be before it stops counting as a balance.
///
/// The conductor refreshes quota on its own schedule and writes to the same SQLite file the relay
/// reads, so no second fetcher lives here. A day is the longest a balance can plausibly still be
/// true on an account that has been serving traffic; older than that is **unknown**, never
/// "zero" and never "spent". An operator who declares `credit_usd` does not depend on this at all.
const FINE_TUNING_CREDIT_MAX_AGE_MS: i64 = 24 * 60 * 60 * 1000;

/// Convert a dollar figure to whole cents, refusing anything that is not money.
fn credit_cents_from_usd(usd: f64) -> Option<u64> {
    (usd.is_finite() && usd >= 0.0).then(|| (usd * 100.0).round() as u64)
}

/// What the router knows about one account's remaining credit, in US cents.
///
/// A declared [`AccountPlan::credit_cents`] wins, because it is the operator's assertion about an
/// account the router cannot price. Otherwise the shared quota cache is consulted for a **USD**
/// balance the provider itself reported. A percentage, a token count, an unlimited plan and a row
/// older than [`FINE_TUNING_CREDIT_MAX_AGE_MS`] are all *unknown*: an unknown balance never
/// qualifies an account to start a job the router cannot fund.
fn known_credit_cents(
    app: &AppState,
    provider: &ProviderPlan,
    account: &AccountPlan,
    now_ms: i64,
) -> Option<u64> {
    if let Some(cents) = account.credit_cents {
        return Some(cents);
    }
    let rows = app
        .quota_store
        .provider_quota_state(provider.id.as_str())
        .ok()?;
    let row = rows.iter().find(|row| row.account_id == account.id)?;
    let age = now_ms.saturating_sub(row.fetched_at.as_millis());
    // A row stamped in the future is a clock disagreement, not fresh knowledge.
    if !(0..=FINE_TUNING_CREDIT_MAX_AGE_MS).contains(&age) {
        return None;
    }
    cached_usd_credit_cents(&row.payload)
}

/// Read a USD balance out of a cached quota payload.
///
/// The conductor stores the headline quota as `credit` only when it is a reported, limited USD
/// balance; the window fallback exists for rows written before that object did, and takes the
/// largest single USD window rather than summing, because two windows may be the same money
/// described twice.
fn cached_usd_credit_cents(payload: &str) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let usd = |figure: &serde_json::Value| -> bool {
        figure.get("unit").and_then(|unit| unit.as_str()) == Some("USD")
    };
    let reported = |figure: &serde_json::Value| -> bool {
        figure.get("unlimited").and_then(|flag| flag.as_bool()) != Some(true)
            && figure
                .get("fraction_reported")
                .and_then(|flag| flag.as_bool())
                == Some(true)
    };
    if let Some(credit) = value.get("credit") {
        if usd(credit) && reported(credit) {
            if let Some(cents) = credit
                .get("remaining")
                .and_then(|remaining| remaining.as_f64())
                .and_then(credit_cents_from_usd)
            {
                return Some(cents);
            }
        }
    }
    value
        .get("windows")
        .and_then(|windows| windows.as_array())
        .into_iter()
        .flatten()
        .filter(|window| usd(window) && reported(window))
        .filter_map(|window| {
            window
                .get("remaining")
                .and_then(|remaining| remaining.as_f64())
                .and_then(credit_cents_from_usd)
        })
        .max()
}

/// Whether this request **starts** a fine-tuning job rather than reading or controlling one.
///
/// Only creation spends money up front, and only creation can be moved to another account before
/// any work exists. A `GET` on the job list must keep its account's view, and a `POST …/cancel`
/// must reach the account that owns the job.
fn starts_a_fine_tuning_job(capability: Capability, method: &Method, path: &str) -> bool {
    capability == Capability::FineTuning
        && *method == Method::POST
        && path == "/v1/fine_tuning/jobs"
}

/// One account the fine-tuning credit floor admitted, paired with the cents it was ranked by.
type FundedAccount<'a> = (
    u64,
    (
        &'a AccountPlan,
        Option<String>,
        Vec<(HeaderName, HeaderValue)>,
    ),
);

/// Forward a request whose provider is chosen by **capability** rather than by a model name.
///
/// Some surfaces name no model in a field the router can read: a fine-tuning job is addressed by
/// the path, a video render by an id in the path, a realtime session by a nested `session.model`
/// the body scanner deliberately does not descend into. `dispatch_routed` cannot serve those — it
/// requires a readable `model` and resolves a step from it — so this is the second, narrower path:
///
/// 1. the provider is the first configured one that declares `capability`, or the one named by
///    `?provider=`;
/// 2. its account is chosen by the same `select::account_candidates` the model-bound path uses, so
///    a key strategy, a cooldown and a per-model restriction all behave identically;
/// 3. the URL is built from the step's base URL and the client's own path, so a base that already
///    carries `/v1` does not get a second one;
/// 4. the body is the client's bytes, optionally shaped by `shape` (see [`fine_tuning_body`]), and
///    is **never** translated: these are OpenAI-shaped management routes, and a provider that
///    declares the capability is expected to speak them;
/// 5. the upstream's status and body are returned as they are, streamed, with hop-by-hop headers
///    dropped by [`build_response`]. A provider's own error about a job is the client's answer.
///
/// No provider declaring the capability is a `404 unsupported_endpoint` naming it, never a 500 and
/// never a chat fallback: the same rule the capability gate applies to model-bound routes.
///
/// A failure to connect is reported, not used to cool an account down. A management route runs once
/// per job rather than once per token, so a cooldown bought here would take an account out of
/// rotation for chat traffic on the strength of one administrative request.
async fn forward_to_capability(
    app: AppState,
    capability: Capability,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
    shape: BodyShaper,
) -> Response {
    let presented = presented_key(&headers);
    let auth = authenticate_request(&app.relay, app.no_key_required, presented.as_deref());
    if !auth.is_allowed() {
        return auth_error(&auth, Dialect::OpenAi);
    }

    let bytes = match read_body(body, MAX_BODY_BYTES).await {
        Ok(bytes) => bytes,
        Err(e) => return e.into_response(),
    };
    // Scanned, not parsed: the only thing wanted from it is whether the client asked for a stream,
    // which decides the `accept` header the upstream sees.
    let head = scan_request_head(&bytes);

    forward_with_bytes(
        &app, capability, method, uri, headers, bytes, head, shape, None,
    )
    .await
}

/// The part of [`forward_to_capability`] that runs once the body is in hand.
///
/// Split out because one caller already has the bytes: a route whose `model` is optional reaches
/// this path from `dispatch_routed`, after authentication, body reading, the head scan and quota
/// admission have all happened. Reading the body a second time is not possible, and doing the first
/// three again would be wrong — the request is already admitted and must be billed as one request.
///
/// `ticket` is that admission. It is passed on to [`build_response`] so the response's bytes and
/// usage are accounted to it exactly as they would be on the model-bound path; when this function
/// refuses or fails, dropping it settles the admission with no usage.
#[allow(clippy::too_many_arguments)]
async fn forward_with_bytes(
    app: &AppState,
    capability: Capability,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    bytes: Bytes,
    head: RequestHead,
    shape: BodyShaper,
    ticket: Option<Arc<crate::quota::QuotaTicket>>,
) -> Response {
    let app = app.clone();
    let mut ticket = ticket;
    let table = app.relay.table();
    let wanted = query_provider(uri.query());
    let health = app.health.view();
    let now_ms = mr_core::ids::now_millis().max(0) as u64;

    let mut targets: Vec<(&ProviderPlan, &StepPlan)> = Vec::new();
    for provider in table.snapshot.providers() {
        if !provider.capabilities.contains(&capability) {
            continue;
        }
        if wanted
            .as_deref()
            .is_some_and(|wanted| provider.id.as_str() != wanted)
        {
            continue;
        }
        // A provider's first step is its canonical address: base URL, dialect, auth and timeouts.
        // Which *model* the step carries is irrelevant here, because the route names none — it is
        // only the vehicle the snapshot uses to describe "how to reach this provider".
        if let Some(step) = provider.steps.first() {
            targets.push((provider, step));
        }
    }
    if targets.is_empty() {
        let detail = match wanted.as_deref() {
            Some(wanted) => format!(
                "no provider named {wanted:?} declares the {} capability",
                capability.as_str()
            ),
            None => format!(
                "the route {} requires the {} capability, and no provider declares it",
                uri.path(),
                capability.as_str()
            ),
        };
        return error_response(
            StatusCode::NOT_FOUND,
            Dialect::OpenAi,
            "unsupported_endpoint",
            &detail,
        );
    }

    let mut last_failure: Option<Response> = None;
    for (provider, step) in targets {
        let counter = table.advance_account_rr(step);
        let candidates = select::account_candidates(
            provider,
            provider.key_strategy,
            counter,
            &health,
            now_ms,
            &step.model,
        );
        let mut usable = Vec::new();
        for account in candidates {
            match account_credential(&app, step, account).await {
                AccountCredential::Usable {
                    credential,
                    oauth_headers,
                    ..
                } => usable.push((account, credential, oauth_headers)),
                AccountCredential::Unusable(_) => continue,
                AccountCredential::VaultUnreadable(e) => {
                    // A vault that cannot be decrypted fails every account the same way, so it is
                    // reported as the relay's problem rather than as this account's.
                    tracing::error!(error = %e, "the credential vault could not be read");
                    return error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        Dialect::OpenAi,
                        "server_error",
                        "the credential vault could not be read, so no account's credential could be used",
                    );
                }
            }
        }
        if usable.is_empty() {
            last_failure = Some(error_response(
                StatusCode::TOO_MANY_REQUESTS,
                Dialect::OpenAi,
                "no_usable_accounts",
                &format!(
                    "no account for `{}` has a usable credential; attach one with \
                     `mr account add {} --secret-stdin`",
                    provider.id, provider.id
                ),
            ));
            continue;
        }

        // Starting a fine-tuning job spends money before the client sees anything, and the run
        // then outlives the request on the account that created it. So creation goes to the
        // richest funded account, and an account that cannot clear the floor is not asked at all.
        // Everything else keeps the strategy's order: a chat request is retried somewhere else,
        // but a created training job cannot be moved.
        if starts_a_fine_tuning_job(capability, &method, uri.path()) {
            let mut funded: Vec<FundedAccount<'_>> = usable
                .drain(..)
                .filter_map(|entry| {
                    let cents = known_credit_cents(&app, provider, entry.0, now_ms as i64)?;
                    (cents > FINE_TUNING_CREDIT_FLOOR_CENTS).then_some((cents, entry))
                })
                .collect();
            funded.sort_by_key(|(cents, _)| std::cmp::Reverse(*cents));
            usable = funded.into_iter().map(|(_, entry)| entry).collect();
            if usable.is_empty() {
                last_failure = Some(error_response(
                    StatusCode::PAYMENT_REQUIRED,
                    Dialect::OpenAi,
                    "insufficient_credit",
                    &format!(
                        "no account for `{}` has more than $15.00 of known credit, and starting a \
                         fine-tuning job requires it; declare a balance with \
                         `mr account set-credit {} --usd <amount>` or refresh the provider's quota",
                        provider.id, provider.id
                    ),
                ));
                continue;
            }
        }

        for (account, credential, oauth_headers) in usable {
            // The client's own path, treated as a dialect path so `build_url` performs the same
            // version reconciliation it performs for every other route: a base URL ending in
            // `/v1` does not produce `/v1/v1/...`, and a base without one gets it added.
            let Some(built_url) = build_url(step, UpstreamPath::Dialect(uri.path())) else {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    Dialect::OpenAi,
                    "upstream_error",
                    &format!("provider {:?} has no usable base URL", provider.id.as_str()),
                );
            };
            let url = match mr_core::placeholders::substitute(&built_url, &account.placeholders) {
                Ok(url) => url,
                Err(problem) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        Dialect::OpenAi,
                        "account_placeholder_unfilled",
                        &format!(
                            "provider {:?} account {:?}: {}",
                            provider.id.as_str(),
                            account.label.as_deref().unwrap_or(account.id.as_str()),
                            problem.message()
                        ),
                    );
                }
            };
            let proxy = match resolve_proxy(&app, &table.snapshot, provider, account) {
                Ok(choice) => choice,
                Err(message) => {
                    return error_response(
                        StatusCode::BAD_GATEWAY,
                        Dialect::OpenAi,
                        "proxy_error",
                        &message,
                    );
                }
            };
            // Shaped before the request is built, because a shaper may itself dial the provider
            // (staging a file) and the body it returns is what this attempt sends.
            let shaped = shape(
                bytes.clone(),
                ShapeTarget {
                    app: &app,
                    provider,
                    step,
                    account,
                    credential: credential.as_deref(),
                    proxy: proxy.as_ref().map(|choice| &choice.endpoint),
                },
            )
            .await;
            let request = UpstreamRequest {
                method: method.clone(),
                url,
                headers: upstream_headers(
                    provider,
                    step,
                    credential.as_deref(),
                    &headers,
                    &head,
                    false,
                    None,
                    &oauth_headers,
                ),
                proxy: proxy.map(|choice| choice.endpoint),
                body: shaped,
                http2: provider.http2,
                connect_timeout: Duration::from_millis(step.connect_timeout_ms),
                ttfb_timeout: Duration::from_millis(step.ttfb_timeout_ms),
                total_timeout: (step.total_timeout_ms > 0)
                    .then(|| Duration::from_millis(step.total_timeout_ms)),
            };
            match app.relay.upstream().send(request).await {
                Ok(response) => {
                    if response.status.is_success() {
                        app.health.record_success(
                            step.provider_id.as_str(),
                            account.id.as_str(),
                            &step.model,
                        );
                        return build_response(response, ticket.take());
                    }
                    // The account was refused rather than the job: a sibling account of the same
                    // provider may serve it, exactly as it would for a chat request. A refusal
                    // that names money is the same story with a different status: a spent
                    // account answers 402, or a 400 whose body says credits ran out, and the
                    // client must never see that while another account can still pay. The dead
                    // account is cooled so the next request does not pay for the same refusal.
                    let refusal = response
                        .body
                        .first_frame_bytes()
                        .and_then(|bytes| exhausted_by_body(bytes));
                    let refused_account = matches!(
                        response.status,
                        StatusCode::UNAUTHORIZED
                            | StatusCode::FORBIDDEN
                            | StatusCode::TOO_MANY_REQUESTS
                            | StatusCode::PAYMENT_REQUIRED
                    ) || refusal.is_some();
                    if refused_account {
                        let reason = refusal.map(|refusal| refusal.reason).unwrap_or_else(|| {
                            classify_status(response.status, step.dialect == Dialect::ModalVllm)
                        });
                        let wait = refusal
                            .and_then(|refusal| refusal.wait_ms)
                            .or_else(|| retry_after_ms(&response.headers));
                        let scope = refusal
                            .and_then(|refusal| refusal.scope)
                            .unwrap_or(provider.cooldown_scope);
                        app.health.cool_down(
                            &select::cooldown_key(
                                scope,
                                provider.id.as_str(),
                                account.id.as_str(),
                                &step.model,
                                reason,
                            ),
                            select::cooldown_until(
                                reason,
                                app.health.failures(
                                    scope,
                                    provider.id.as_str(),
                                    account.id.as_str(),
                                    &step.model,
                                ),
                                wait,
                                &app.relay.quota_config(),
                            ),
                            reason,
                        );
                        tracing::warn!(
                            provider = %provider.id,
                            status = %response.status,
                            reason = ?reason,
                            "the capability-addressed request was refused for this account"
                        );
                        last_failure = Some(build_response(response, None));
                        continue;
                    }
                    return build_response(response, ticket.take());
                }
                Err(error) => {
                    tracing::warn!(
                        provider = %provider.id,
                        error = %error,
                        "the capability-addressed upstream request failed"
                    );
                    last_failure = Some(error_response(
                        StatusCode::BAD_GATEWAY,
                        Dialect::OpenAi,
                        "upstream_error",
                        &format!(
                            "provider {:?} could not be reached: {error}",
                            provider.id.as_str()
                        ),
                    ));
                }
            }
        }
    }

    last_failure.unwrap_or_else(|| {
        error_response(
            StatusCode::BAD_GATEWAY,
            Dialect::OpenAi,
            "upstream_error",
            "no provider that declares this capability could be reached",
        )
    })
}

/// `POST|GET /v1/fine_tuning/jobs` and every job sub-resource.
async fn fine_tuning(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    forward_to_capability(
        app,
        Capability::FineTuning,
        method,
        uri,
        headers,
        body,
        fine_tuning_body,
    )
    .await
}

/// `GET /v1/videos/{id}` and `GET /v1/videos/{id}/content`.
///
/// The create is model-bound and goes through `dispatch_openai`; these two name the render by id in
/// the path, so they are capability-addressed. The content read is binary and is forwarded as bytes
/// with the provider's own content type — [`build_response`] drops `content-length` and streams.
async fn video_read(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    forward_to_capability(
        app,
        Capability::Video,
        method,
        uri,
        headers,
        body,
        passthrough_body,
    )
    .await
}

/// `POST /v1/realtime/client_secrets` and `POST /v1/realtime/calls`.
///
/// The model these sessions name lives inside `session`, which the scanner does not descend into,
/// so the provider is chosen by capability. The response is an SDP answer or a secret bundle: both
/// are forwarded byte for byte.
async fn realtime_session(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    forward_to_capability(
        app,
        Capability::Realtime,
        method,
        uri,
        headers,
        body,
        passthrough_body,
    )
    .await
}

/// `/v1/containers` and its sub-resources.
///
/// Refused with `501 unsupported_endpoint` unless a provider declares `Containers`: this router does
/// not run containers itself, so the only honest answer without such a provider is that the endpoint
/// is not supported here. With one, the request is forwarded like any other capability-addressed
/// route.
async fn containers(
    State(app): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    let declares = app
        .relay
        .table()
        .snapshot
        .providers()
        .iter()
        .any(|provider| provider.capabilities.contains(&Capability::Containers));
    if !declares {
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            Dialect::OpenAi,
            "unsupported_endpoint",
            "the route containers requires the containers capability, and no provider declares it: \
             this router does not run containers itself, so it serves this surface only by proxying \
             to a provider that does",
        );
    }
    forward_to_capability(
        app,
        Capability::Containers,
        method,
        uri,
        headers,
        body,
        passthrough_body,
    )
    .await
}

/// Apply a step's reasoning level to an outgoing body, and retarget the model id.
///
/// Returns `Ok(None)` when nothing had to change, which is the common case for a request that
/// already named the model it wants at the level it wants, and for any request to a model that
/// advertises no reasoning levels at all.
///
/// ## Why this is a merge and not a re-encode
///
/// The relay's whole latency argument is that a same-dialect request is forwarded byte-for-byte, so
/// the router never parses a body it does not have to and never silently deletes a provider field
/// it does not understand. That property only survives here because the rewrite is surgical: parse,
/// merge one fragment, re-serialise — and only when there is a fragment to merge, or a model field
/// that has to be retargeted. A request to `provider/model-high` on an OpenAI-shaped provider is
/// the case that needs both, because `model-high` is not a model the upstream has.
///
/// ## Who wins when the client already said something
///
/// The client's own explicit reasoning field is never overwritten. A client that sends
/// `reasoning_effort` is being more specific than a level suffix can be, and quietly replacing it
/// would make the same request mean different things depending on whether it arrived through a
/// model alias.
/// Whether the bytes about to go upstream may be read as JSON.
///
/// Every rewrite the relay performs on a body — the reasoning merge, the modality model retarget,
/// asking an OpenAI upstream for stream usage — has to parse it, and a parse is only legitimate if
/// the body is JSON. Multipart audio and image uploads and raw binary bodies are forwarded byte for
/// byte; parsing one would either fail the request or corrupt it.
///
/// The client's `Content-Type` decides, because it is the only statement about the body the relay
/// has before reading it:
///
/// * A translated body is JSON by construction, whatever the client sent. Dialect translation
///   already refused anything it could not parse.
/// * A JSON media type (`application/json`, `text/json`, anything ending in `+json`) commits the
///   sender to JSON. A malformed body under this label is a client error the relay reports, not a
///   binary body it silently forwards.
/// * A media type that is not JSON is a statement that the body is not JSON, and it is never
///   parsed.
/// * No content type is no statement at all, so the body is probed: a body that parses as JSON is
///   treated as JSON, preserving the behaviour clients that omit the header have always had, and
///   anything else is left alone.
fn outgoing_is_json(headers: &HeaderMap, translated: bool, body: &[u8]) -> bool {
    if translated {
        return true;
    }
    let Some(value) = headers.get("content-type").and_then(|v| v.to_str().ok()) else {
        return serde_json::from_slice::<serde_json::Value>(body).is_ok();
    };
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type.eq_ignore_ascii_case("application/json")
        || media_type.eq_ignore_ascii_case("text/json")
        || media_type
            .get(media_type.len().saturating_sub(5)..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
}

fn apply_reasoning_level(
    outgoing: &[u8],
    step: &StepPlan,
    translated: bool,
    level_override: Option<mr_core::dialect::ReasoningLevel>,
) -> Result<Option<Vec<u8>>, String> {
    let rename_model = !translated;
    // A combo step runs at the tier the combo's name asked for when there is one, and otherwise at
    // the level the step was compiled with. A level the model does not offer falls back to that
    // model's highest available one, never to a fragment the upstream would reject.
    let level = match level_override {
        Some(level) if step.supports_level(level) => level,
        Some(_) => mr_core::dialect::ReasoningLevel::from_u8(
            step.default_level.max(
                step.levels()
                    .last()
                    .map(|l| l.rank())
                    .unwrap_or(mr_core::dialect::ReasoningLevel::None.rank()),
            ),
        )
        .unwrap_or(mr_core::dialect::ReasoningLevel::None),
        None => mr_core::dialect::ReasoningLevel::from_u8(step.level)
            .unwrap_or(mr_core::dialect::ReasoningLevel::None),
    };
    let fragment = step.reasoning_fragment_for(level);
    if fragment.is_none() && !rename_model {
        return Ok(None);
    }

    let mut body: serde_json::Value = serde_json::from_slice(outgoing).map_err(|_| {
        "the request body is not JSON, so a reasoning level cannot be applied".to_string()
    })?;
    // Read before the body is borrowed mutably: the cap decides how much of it thinking may take.
    let output_cap = stated_output_cap(&body);
    let Some(object) = body.as_object_mut() else {
        return Ok(None);
    };

    let mut changed = false;
    if rename_model {
        // Only when the field is present and actually different: a body whose `model` is already
        // the step's model must not be rewritten, because rewriting it would mean re-serialising
        // every field of a body we had no other reason to touch.
        let differs = object
            .get("model")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|current| current != step.model);
        if differs {
            object.insert(
                "model".into(),
                serde_json::Value::String(step.model.clone()),
            );
            changed = true;
        }
    }

    if let Some(fragment) = fragment {
        let mut fragment: serde_json::Value = serde_json::from_str(fragment)
            .map_err(|_| "the compiled reasoning fragment is not valid JSON".to_string())?;
        // The compiled budget is already fitted to the model's own ceiling. This fits it to the
        // ceiling *this* request states, because Anthropic rejects `budget_tokens` at or above
        // `max_tokens` outright, and a budget that eats the whole allowance is how a Gemini answer
        // arrives with nothing in `content` but thoughts. The dialect's own range is applied here
        // too: a client cannot raise a budget past what the surface accepts by stating a larger
        // `max_tokens`.
        if !mr_core::snapshot::fit_thinking_budget(step.dialect, &mut fragment, output_cap) {
            // No room left to both think and answer: send no level rather than one the upstream
            // will refuse.
            fragment = serde_json::Value::Object(serde_json::Map::new());
        }
        if merge_reasoning(object, &fragment) {
            changed = true;
        }
    }

    if !changed {
        return Ok(None);
    }
    serde_json::to_vec(&body)
        .map(Some)
        .map_err(|_| "the rewritten request body could not be encoded".to_string())
}

/// The output cap a request states, in whichever spelling and nesting its dialect uses.
///
/// Only the two placements a real body uses are read: the top level (`max_tokens` for the Anthropic
/// and OpenAI shapes, `max_completion_tokens` and `max_output_tokens` for the newer OpenAI ones) and
/// `generationConfig`, which Gemini spells `maxOutputTokens` and Antigravity nests under `request`.
/// A number anywhere else is not a cap on the answer, so it is deliberately not searched for.
fn stated_output_cap(body: &serde_json::Value) -> Option<u32> {
    fn read(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<u32> {
        keys.iter()
            .find_map(|key| object.get(*key).and_then(serde_json::Value::as_u64))
            .and_then(|value| u32::try_from(value).ok())
    }
    let top = body.as_object()?;
    read(
        top,
        &["max_tokens", "max_completion_tokens", "max_output_tokens"],
    )
    .or_else(|| {
        let generation = top
            .get("generationConfig")
            .or_else(|| top.get("request")?.get("generationConfig"))?
            .as_object()?;
        read(generation, &["maxOutputTokens", "max_output_tokens"])
    })
}

/// Recursively merge `fragment` into `object`, returning whether anything was written.
///
/// Two rules make this safe to run on a body the router did not author:
///
/// * An existing **scalar** under a key the fragment also names is left alone. That is what keeps a
///   client's explicit `reasoning_effort` authoritative, and it also means a provider-specific
///   object under `generationConfig` keeps every field the fragment does not mention.
/// * Nothing is ever created that the fragment does not name. A model that takes no reasoning gets
///   no invented field.
fn merge_reasoning(
    object: &mut serde_json::Map<String, serde_json::Value>,
    fragment: &serde_json::Value,
) -> bool {
    let Some(fragment) = fragment.as_object() else {
        return false;
    };
    let mut changed = false;
    for (key, value) in fragment {
        match object.get_mut(key) {
            Some(serde_json::Value::Object(existing)) if value.is_object() => {
                changed |= merge_reasoning(existing, value);
            }
            Some(_) => {}
            None => {
                object.insert(key.clone(), value.clone());
                changed = true;
            }
        }
    }
    changed
}

/// The shared path for every model route.
/// Ask an OpenAI-shaped upstream to report token usage on a stream.
///
/// Without `stream_options.include_usage` the final chunk of a stream carries no usage at all, so a
/// streamed request is billed as zero tokens against a token cap. A body that is not streaming, or
/// that already states a preference, is returned untouched — including one that does not parse: it
/// is the client's request verbatim, and rewriting it into an error because the router could not
/// see through it would be a worse answer than a miscount.
fn with_stream_usage(body: Bytes) -> Bytes {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return body;
    };
    if value.get("stream") != Some(&serde_json::Value::Bool(true)) {
        return body;
    }
    if value.pointer("/stream_options/include_usage").is_some() {
        return body;
    }
    let Some(object) = value.as_object_mut() else {
        return body;
    };
    match object.get_mut("stream_options") {
        Some(serde_json::Value::Object(options)) => {
            options.insert("include_usage".into(), serde_json::Value::Bool(true));
        }
        _ => {
            object.insert(
                "stream_options".into(),
                serde_json::json!({"include_usage": true}),
            );
        }
    }
    match serde_json::to_vec(&value) {
        Ok(encoded) => Bytes::from(encoded),
        Err(_) => body,
    }
}

fn inject_antigravity_project(
    outgoing: &[u8],
    tokens: Option<&mr_core::oauth::TokenSet>,
) -> Result<Vec<u8>, &'static str> {
    let project = tokens
        .and_then(|t| t.extra.get("cloudcode_project_id"))
        .and_then(serde_json::Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        .ok_or("Antigravity Cloud Code project metadata is missing or malformed")?;
    let mut envelope: serde_json::Value = serde_json::from_slice(outgoing)
        .map_err(|_| "Antigravity request must be a JSON envelope")?;
    let object = envelope
        .as_object_mut()
        .ok_or("Antigravity request envelope must be a JSON object")?;
    object.insert("project".into(), serde_json::Value::String(project.into()));
    serde_json::to_vec(&envelope).map_err(|_| "failed to encode Antigravity request envelope")
}

/// A request as the dispatch path needs it: who it is from, what it asks for, and its bytes.
///
/// Bundled into one value because the dispatch path can be re-entered — for a modality fallback —
/// and eight positional arguments make a re-entry easy to get wrong.
struct DispatchRequest {
    /// The shared state.
    app: AppState,
    /// The dialect the client speaks.
    client_dialect: Dialect,
    /// The request method.
    method: Method,
    /// The request URI.
    uri: Uri,
    /// The request headers.
    headers: HeaderMap,
    /// The request body, still unread.
    body: AxumBody,
    /// Whether this request already proved it may run, so the key check is skipped.
    ///
    /// Only the loopback control-plane probe sets this, and the probe handler refuses anything that
    /// did not arrive over the loopback interface. It is not a bypass of the *routing* rules: the
    /// request goes through the same resolution, dialects, accounts and fallbacks a client would.
    pre_authorized: bool,
    /// The provider whose KV-token budget **already** covers this request, when it is not the request
    /// path that reserved it.
    ///
    /// Set by exactly one caller: the batch lane, which reserves an item's KV tokens before it
    /// dispatches the item, because only the lane knows that the item is one of thousands and that its
    /// tokens must not be counted twice. When this names the step the attempt is about to use, the
    /// attempt skips its own reservation — the lane's lease is the one that is dropped and billed — and
    /// a fallback attempt on a *different* provider reserves normally.
    pre_admitted: Option<String>,
}

/// Run a request through the ordinary dispatch path from inside this process.
///
/// A few of the router's own features need a model answer they are not themselves: a vector store
/// embeds a query, and an eval asks a model a question. Those are the relay's own requests, not a
/// client's, so they arrive already authorised — there is no client key to present and nothing to
/// meter — and they take the same provider selection, retries and translation as any other call.
///
/// The caller supplies the JSON body; the path decides the dialect pivot exactly as it does for a
/// socket request. Errors come back as the response they would have been, so a caller can pass an
/// upstream failure straight through instead of inventing a second error shape.
pub(crate) async fn dispatch_internal(
    app: &AppState,
    method: Method,
    path: &str,
    body: Vec<u8>,
) -> Response {
    let uri: Uri = match path.parse() {
        Ok(uri) => uri,
        Err(_) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                Dialect::OpenAi,
                "internal_error",
                "the router built a request path it cannot parse",
            );
        }
    };
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    dispatch_routed(
        DispatchRequest {
            app: app.clone(),
            client_dialect: Dialect::OpenAi,
            method,
            uri,
            headers,
            body: AxumBody::from(body),
            pre_authorized: true,
            pre_admitted: None,
        },
        0,
        None,
    )
    .await
}

async fn dispatch(
    app: AppState,
    client_dialect: Dialect,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: AxumBody,
) -> Response {
    dispatch_routed(
        DispatchRequest {
            app,
            client_dialect,
            method,
            uri,
            headers,
            body,
            pre_authorized: false,
            pre_admitted: None,
        },
        0,
        None,
    )
    .await
}

/// The proxy one upstream attempt must egress through.
struct ProxyChoice {
    /// The endpoint to dial.
    endpoint: ProxyEndpoint,
    /// The lease that keeps this proxy marked in flight.
    ///
    /// It is held for its `Drop`, not for anything it returns: when the attempt ends — served,
    /// failed, or given up on — the lease is dropped and the proxy stops looking busy. The
    /// `least-used` strategy is a tie-break, not a health check.
    _lease: ProxyLease,
}

/// Resolve the proxy for one upstream attempt.
///
/// The precedence is: the account's override, then the provider's, then the configured strategy
/// over the enabled pool. `Ok(None)` means a direct connection.
///
/// # Why a named proxy that is not in the pool is not an error
///
/// A proxy the operator *disabled* is one they said not to use, and the request goes out directly
/// — the same answer [`mr_core::config::Config::effective_proxy`] documents. A proxy that does not
/// exist at all cannot reach this point: the configuration names an unknown proxy id as a
/// validation problem, and the relay refuses to start on an invalid configuration.
///
/// # Why the credential is read here
///
/// The password lives in the vault, and the vault is the only place it can come from. It is read
/// per attempt rather than cached, for the same reason the provider credential is: the vault is
/// the authority on what the current password is, and a cached copy would keep dialling with a
/// password the operator has already rotated.
fn resolve_proxy(
    app: &AppState,
    snapshot: &mr_core::snapshot::RoutingSnapshot,
    provider: &mr_core::snapshot::ProviderPlan,
    account: &mr_core::snapshot::AccountPlan,
) -> Result<Option<ProxyChoice>, String> {
    let wanted = account.proxy.as_deref().or(provider.proxy.as_deref());
    let plan = match wanted {
        Some(id) => match snapshot.proxy(id) {
            Some(plan) => plan,
            // Disabled: a direct connection, as `Config::effective_proxy` documents.
            None => return Ok(None),
        },
        None => {
            let pool = snapshot.proxies();
            if pool.is_empty() {
                return Ok(None);
            }
            match proxy::choose(pool, snapshot.proxy_strategy(), app.relay.proxy_selection()) {
                Some(plan) => plan,
                None => return Ok(None),
            }
        }
    };

    let password = match plan.secret_ref.as_deref() {
        Some(secret_ref) => match app.vault.get(secret_ref) {
            Ok(Some(password)) => Some(password),
            Ok(None) => {
                return Err(format!(
                    "the proxy `{}` names the vault secret `{}`, which does not exist",
                    plan.id, secret_ref
                ));
            }
            Err(e) => {
                return Err(format!(
                    "the credential for the proxy `{}` could not be read: {e}",
                    plan.id
                ));
            }
        },
        None => None,
    };

    let endpoint = ProxyEndpoint::from_plan(plan, password.as_deref())
        .map_err(|e| format!("the proxy `{}` is not usable: {e}", plan.id))?;
    let lease = app.relay.proxy_selection().lease(&plan.id);
    Ok(Some(ProxyChoice {
        endpoint,
        _lease: lease,
    }))
}

/// The request path, with the modality-fallback budget and the snapshot generation it may use.
///
/// `fallback_depth` counts the fallbacks already taken. One is the budget: a fallback target that
/// itself cannot carry the request's modality must fail rather than fall back again, because a
/// cycle of defaults would otherwise loop.
///
/// `table` pins the routing table to the generation the original request resolved against. A
/// fallback must not mix a step from one snapshot with a provider from another, so the pinned
/// table is passed down rather than re-read.
async fn dispatch_routed(
    request: DispatchRequest,
    fallback_depth: u8,
    table: Option<Arc<RouteTable>>,
) -> Response {
    let DispatchRequest {
        app,
        client_dialect,
        method,
        uri,
        headers,
        body,
        pre_authorized,
        pre_admitted,
    } = request;
    let started = std::time::Instant::now();

    // --- Authentication, before anything reads the body. ---
    //
    // The control-plane probe has already been authenticated by its own handler, which refuses
    // every caller that is not on the loopback interface. Representing that as the `Open` outcome
    // is exact: no client key was presented, nothing is metered, and the request is admitted.
    let presented = presented_key(&headers);
    let auth = if pre_authorized {
        AuthOutcome::Open
    } else {
        authenticate_request(&app.relay, app.no_key_required, presented.as_deref())
    };
    if !auth.is_allowed() {
        return auth_error(&auth, client_dialect);
    }
    let auth_done = started.elapsed();

    let bytes = match read_body(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    // --- The model field, without parsing the JSON. ---
    //
    // A multipart body has no JSON object to scan: audio transcriptions and image edits carry their
    // parameters as form fields. The `model` part is located in the same bytes that will be
    // forwarded, and nothing about the body is rewritten, so the boundary, the file and every other
    // field reach the upstream exactly as the client sent them.
    let mut head = scan_request_head(&bytes);
    if head.model.is_none() {
        if let Some(boundary) = headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .and_then(multipart_boundary)
        {
            head.model = scan_multipart_model(&bytes, boundary);
        }
    }
    let scan_done = started.elapsed();

    // --- Quota admission, before anything is sent upstream. ---
    //
    // The check is here rather than after the routing lookup because a cap is a property of the
    // *key*, not of the route: a key that is out of budget must be refused whatever model it
    // asked for, and refusing it before the upstream is contacted is the whole point of
    // admitting rather than accounting after the fact.
    //
    // The fallback re-entry inherits the ticket of the request it is retrying, so a reroute is
    // still one admitted request and not two.
    let ticket = if fallback_depth == 0 {
        match &auth {
            AuthOutcome::Ok(entry) if entry.is_metered() => {
                let request_id = mr_core::ids::new_id();
                let estimate = crate::quota::estimate_prompt_tokens(bytes.len());
                match app.quota.admit(
                    &entry.id,
                    &entry.limits,
                    entry.limit_requests,
                    &request_id,
                    estimate,
                    mr_core::ids::Millis::now(),
                ) {
                    crate::quota::Admission::Allowed(ticket) => Some(ticket),
                    // Caps are configured but none of them binds, so nothing is reserved. The
                    // request is still recorded: token counts, the cache split and the rate are
                    // facts about what was spent, and reporting them cannot depend on whether the
                    // operator happened to set a budget.
                    crate::quota::Admission::Unmetered => Some(app.quota.observe(
                        &entry.id,
                        &request_id,
                        mr_core::ids::Millis::now(),
                        Some(bytes.len() as u64),
                    )),
                    crate::quota::Admission::Denied(denial) => {
                        return quota_error(&denial, client_dialect);
                    }
                }
            }
            // An authenticated key with no caps at all: nothing to enforce, everything to report.
            AuthOutcome::Ok(entry) => Some(app.quota.observe(
                &entry.id,
                &mr_core::ids::new_id(),
                mr_core::ids::Millis::now(),
                Some(bytes.len() as u64),
            )),
            _ => None,
        }
    } else {
        None
    };

    // The client's route decides the pivot the body is read in, and therefore which translation
    // applies. Derived from the path rather than passed in by each handler so that a new route
    // cannot be registered and silently fall through to Chat Completions.
    let route = route_kind_for(uri.path());

    let Some(model_name) = requested_model(&head, &bytes) else {
        // Three routes make `model` optional, and a client that omits it is asking for the
        // provider's documented default rather than making an unanswerable request. Those are
        // forwarded by capability, with the bytes already read and the admission already taken, so
        // the request is billed once and the body is not read twice.
        if let Some(capability) = model_optional_capability(route) {
            return forward_with_bytes(
                &app,
                capability,
                method,
                uri,
                headers,
                bytes,
                head,
                passthrough_body,
                ticket,
            )
            .await;
        }
        return error_response(
            StatusCode::BAD_REQUEST,
            client_dialect,
            "invalid_request_error",
            "the request body has no readable \"model\" field",
        );
    };

    // --- Routing lookup. ---
    //
    // The table is pinned for the whole request, and a modality fallback inherits the pinned
    // generation rather than reading a newer one: a fallback must resolve its target against the
    // same snapshot that produced the original step.
    let table = match table {
        Some(pinned) => pinned,
        None => Arc::clone(&*app.relay.table()),
    };
    let resolution = table.snapshot.resolve(model_name);
    let lookup_done = started.elapsed();

    // The level is part of the resolution, not a side channel: `-high` on a model that offers it
    // selects that tier, and `-max` on a model that tops out at `xhigh` is *rerouted* to `xhigh`
    // rather than failing, with the substitution reported so the logs show what actually ran.
    //
    // `level_override` is set only for a combo named *with* a tier (`coding-high`). Each step then
    // runs at that tier rather than at its own model's default, and a step whose model does not
    // offer it falls back to that model's highest — the same substitution rule as a direct name.
    let mut combo = None;
    let mut direct = None;
    let mut level_override: Option<mr_core::dialect::ReasoningLevel> = None;
    match resolution {
        Resolution::Combo { plan, level } => {
            combo = Some(plan);
            level_override = level.and_then(mr_core::dialect::ReasoningLevel::from_u8);
        }
        Resolution::Direct {
            step,
            level,
            substituted,
        } => {
            if substituted {
                tracing::debug!(
                    model = model_name,
                    provider = %step.provider_id,
                    level = mr_core::dialect::ReasoningLevel::from_u8(level)
                        .map(|l| l.as_str())
                        .unwrap_or("unknown"),
                    "the requested reasoning level is not one this model offers; using its highest"
                );
            }
            direct = Some(step);
        }
        Resolution::NotFound { reason } => {
            return error_response(
                StatusCode::NOT_FOUND,
                client_dialect,
                "model_not_found",
                &reason,
            );
        }
    }

    let health = app.health.view();
    let now_ms = mr_core::ids::now_millis().max(0) as u64;

    // --- Step order. ---
    let mut sticky_index = None;
    let steps: Vec<&StepPlan> = match (combo, direct) {
        (_, Some(step)) => vec![step],
        (Some(combo), None) => {
            let counter = table.advance_combo_rr(&combo.id);
            let order =
                select::order_steps(combo, table.snapshot.providers(), &health, counter, now_ms);
            let sticky_enabled = sticky_applies(combo);
            if sticky_enabled {
                sticky_index = app.sticky_success.preferred_index(
                    &combo.id,
                    &combo.steps,
                    |step| {
                        table
                            .snapshot
                            .providers()
                            .get(step.provider_index as usize)
                            .is_some_and(|provider| {
                                select::step_has_usable_account(
                                    provider,
                                    &health,
                                    now_ms,
                                    &step.model,
                                )
                            })
                    },
                    std::time::Instant::now(),
                );
            }
            let mut indices = order;
            if let Some(index) = sticky_index {
                indices.retain(|i| *i != index);
                indices.insert(0, index);
            }
            indices
                .into_iter()
                .filter_map(|i| combo.steps.get(i))
                .collect()
        }
        (None, None) => Vec::new(),
    };

    // --- The capability gate. ---
    //
    // A provider declares which client surfaces it serves, and a model it hosts is only reachable
    // through those surfaces. A route whose capability no step's provider declares is not dialled:
    // the upstream would answer with a 404 that blames the model for a feature the operator never
    // configured, and falling back to chat would answer in a shape the client cannot read. The
    // honest answer is that this endpoint is unsupported here.
    //
    // A step whose provider is missing from the snapshot is kept, so the mid-flight skip can report
    // the stale snapshot instead of disguising it as a missing capability.
    let declared_steps = steps.len();
    let steps: Vec<&StepPlan> = steps
        .into_iter()
        .filter(|step| {
            table
                .snapshot
                .providers()
                .get(step.provider_index as usize)
                .is_none_or(|provider| provider.capabilities.contains(&route.capability()))
        })
        .collect();
    if steps.is_empty() && declared_steps > 0 {
        return error_response(
            StatusCode::NOT_FOUND,
            client_dialect,
            "unsupported_endpoint",
            &format!(
                "the route {} requires the {} capability, and no step's provider declares it",
                route.as_str(),
                Capability::for_route(route).as_str()
            ),
        );
    }

    if steps.is_empty() {
        return error_response(
            StatusCode::NOT_FOUND,
            client_dialect,
            "model_not_found",
            &format!("\"{model_name}\" resolved but has no usable steps"),
        );
    }

    // --- Try each step in order, until one answers successfully. ---
    let mut last_failure: Option<Response> = None;
    let quota = app.relay.quota_config();
    // The model the request should be retried against when no step can carry its modality.
    let mut fallback_target: Option<String> = None;
    // Why the steps were skipped, so a refusal that tried nothing can name the reason instead of
    // assuming a cooldown. Bounded by the number of distinct causes.
    let mut skip_causes: Vec<SkipCause> = Vec::new();

    // What this request is filed under, when it is combo activity at all: the combo it resolved to
    // and the name the client actually sent. A direct model is `None`, so the recording helpers
    // below are no-ops for it — a plain model request is not a combo decision.
    let activity = combo.map(|combo| (combo.name.as_str(), model_name));
    // The last step the relay actually dispatched. Reported when no step answers, so the entry
    // still names a step rather than an empty string.
    let mut attempted: Option<String> = None;

    // The inputs every attempt needs. The same context serves the ordinary chain below and the
    // hedged race, so the two cannot drift apart.
    let attempt_ctx = AttemptContext {
        app: &app,
        table: Arc::clone(&table),
        now_ms,
        combo,
        client_dialect,
        method: &method,
        headers: &headers,
        head,
        bytes: &bytes,
        model_name,
        route,
        uri: &uri,
        level_override,
        quota: &quota,
        ticket: ticket.clone(),
        started,
        auth_done,
        scan_done,
        lookup_done,
        sticky_index,
        // Cloned rather than moved: the modality-fallback re-entry below is the same request and has
        // to carry the same reservation, so the original stays in hand after this context is built.
        pre_admitted: pre_admitted.clone(),
    };

    // --- The hedged race. ---
    //
    // A combo whose hedge is enabled races its participants: every participant is dispatched at
    // once, and the winner is the highest-priority participant that produced a usable answer
    // inside the eligibility window.
    let mut raced: Vec<&StepPlan> = Vec::new();
    if let Some(combo) = combo {
        if combo.hedge.enabled {
            let participants: Vec<&StepPlan> = combo
                .hedge
                .participants
                .iter()
                .filter_map(|index| combo.steps.get(*index as usize))
                .collect();
            if participants.len() >= 2 {
                tracing::debug!(
                    combo = %combo.id,
                    participants = participants.len(),
                    window_ms = combo.hedge.window_ms,
                    "hedged race"
                );
                match run_hedge(&attempt_ctx, &participants, &combo.hedge).await {
                    HedgeOutcome::Won { response, winner } => {
                        return served_combo(&app, activity, winner, now_ms, response);
                    }
                    HedgeOutcome::Fatal {
                        response,
                        attempted: step,
                    } => {
                        failed_combo(&app, activity, Some(&step), now_ms);
                        return response;
                    }
                    HedgeOutcome::NoWinner {
                        last_failure: failure,
                        fallback_target: target,
                        sticky_cleared: cleared,
                    } => {
                        last_failure = failure;
                        if target.is_some() {
                            fallback_target = target;
                        }
                        if cleared {
                            sticky_index = None;
                        }
                        // The participants have been tried. The ordinary chain continues with the
                        // steps that were not in the race, so a failed race does not send the
                        // same request to the same provider twice.
                        raced = participants.clone();
                        if activity.is_some() {
                            attempted = participants.last().map(|step| participant_id(step));
                        }
                    }
                }
            }
        }
    }

    for step in &steps {
        // A step that was in the race has already been tried.
        if raced.iter().any(|tried| std::ptr::eq(*tried, *step)) {
            continue;
        }
        let is_sticky = sticky_step_is(combo, sticky_index, step);
        if activity.is_some() {
            attempted = Some(participant_id(step));
        }
        // One step can hold several accounts, and an account that is throttled or spent is not a
        // failed *step*: the sibling accounts are still there to serve the request. The attempt
        // cools the account it used down before returning, so retrying the same step resolves its
        // candidates again and lands on the next account. Bounded by the number of accounts, so a
        // provider whose every account refuses costs one attempt each and then falls through to
        // the next step as before.
        let account_retries = table
            .snapshot
            .providers()
            .get(step.provider_index as usize)
            .map(|provider| provider.accounts.len().saturating_sub(1).min(8))
            .unwrap_or(0);
        let mut retries_left = account_retries;
        let mut attempt = attempt_step(&attempt_ctx, step, is_sticky, false).await;
        loop {
            let retry = match &attempt {
                // A body to hand the client means the upstream answered and refused. That answer is
                // worth retrying on a sibling account only when another of *this step's* accounts
                // is still usable and the refusal was about the credential rather than the bytes;
                // a request the upstream rejected as malformed fails identically everywhere.
                Attempt::Failed { retry_accounts, .. } => {
                    *retry_accounts
                        && retries_left > 0
                        && step_has_a_live_account(&app, &table, step, now_ms)
                }
                _ => false,
            };
            if !retry {
                break;
            }
            retries_left -= 1;
            tracing::debug!(
                provider = %step.provider_id,
                model = %step.model,
                "retrying the same step on another account"
            );
            attempt = attempt_step(&attempt_ctx, step, is_sticky, false).await;
        }
        match attempt {
            Attempt::Answer(response) => {
                // A non-hedge step has no replacement, but the body still needs to be buffered to
                // event boundaries so that a mid-stream death never hands the client half a frame.
                let wrapped = crate::recovery::wrap(
                    response,
                    Vec::new(),
                    crate::recovery::RecoveryConfig {
                        policy: combo.map(|c| c.midstream_policy).unwrap_or_default(),
                        mode: mr_core::dialect::WinnerFailMode::PickHighestPassed,
                        client_dialect,
                        winner_id: participant_id(step),
                        announceable: false,
                        rerun_factory: None,
                        rerun_max_rounds: 0,
                    },
                );
                return served_combo(&app, activity, participant_id(step), now_ms, wrapped);
            }
            Attempt::Failed {
                response,
                sticky_cleared,
                retry_accounts: _,
            } => {
                if sticky_cleared {
                    sticky_index = None;
                }
                last_failure = response;
            }
            Attempt::Skip {
                sticky_cleared,
                fallback_target: target,
                cause,
            } => {
                if sticky_cleared {
                    sticky_index = None;
                }
                skip_causes.push(cause);
                if target.is_some() {
                    fallback_target = target;
                }
            }
            Attempt::Fatal { response } => {
                failed_combo(&app, activity, attempted.as_deref(), now_ms);
                return response;
            }
        }
    }

    // No step could carry the request's modality. The configured default names a model that can,
    // so the request is retried against it — as a **complete** reroute: the target is resolved
    // against the same snapshot, with its own provider, account, credential, URL, dialect and
    // headers. Rewriting only the model string and sending it to the original provider would be
    // a silent misroute, which is why the retry goes back through the whole request path.
    if let Some(target) = fallback_target {
        if fallback_depth == 0 {
            // A default that names no known model cannot be routed. Saying so is more useful than
            // letting the retry fail with a 404 for a model the client never asked for.
            if let Resolution::NotFound { reason } = table.snapshot.resolve(&target) {
                return error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    client_dialect,
                    "modality_fallback_unavailable",
                    &format!(
                        "the selected route does not support this input modality, and the configured default \"{target}\" is not a model this build can route ({reason})"
                    ),
                );
            }
            if let Some(retry_body) = request_with_model(&bytes, &target) {
                tracing::debug!(
                    model = model_name,
                    fallback = %target,
                    "the selected route cannot carry this modality; retrying against the configured default"
                );
                // The retry goes through a boxed call: an async function that calls itself has an
                // infinitely recursive future type, so the one re-entry point is made through a
                // `BoxFuture`. The allocation happens only on the rare fallback path.
                return dispatch_routed_boxed(
                    DispatchRequest {
                        app,
                        client_dialect,
                        method,
                        uri,
                        headers,
                        body: AxumBody::from(retry_body),
                        // The re-entry is the same request, so it keeps whatever authorization
                        // the original carried — including the control-plane probe's.
                        pre_authorized,
                        // And the same KV-token reservation, which the batch lane made for this item
                        // before dispatching it in the first place. Cloned because the request that
                        // is still in hand owns the original value.
                        pre_admitted: pre_admitted.clone(),
                    },
                    fallback_depth + 1,
                    Some(table),
                )
                .await;
            }
        }
        return error_response(
            StatusCode::NOT_IMPLEMENTED,
            client_dialect,
            "modality_fallback_unavailable",
            &format!(
                "the selected route does not support this input modality, and the configured default \"{target}\" cannot be used for it"
            ),
        );
    }

    // Every step was skipped because its accounts are cooling down. A cooldown is a prediction
    // about the near future, not a fact about now, and refusing the request without trying
    // anything is the worst of the available answers: the operator sees a combo that died
    // instantly while the provider was fine. The step that recovers soonest is tried anyway, and
    // its answer — or its real error — is what the client gets.
    if last_failure.is_none() {
        if let Some(step) = last_resort_step(&app, &table, &steps, &raced, now_ms) {
            if activity.is_some() {
                attempted = Some(participant_id(step));
            }
            tracing::debug!(
                provider = %step.provider_id,
                model = %step.model,
                "every step is cooling down; trying the one that recovers soonest anyway"
            );
            match attempt_step(&attempt_ctx, step, false, true).await {
                Attempt::Answer(response) => {
                    let wrapped = crate::recovery::wrap(
                        response,
                        Vec::new(),
                        crate::recovery::RecoveryConfig {
                            policy: combo.map(|c| c.midstream_policy).unwrap_or_default(),
                            mode: mr_core::dialect::WinnerFailMode::PickHighestPassed,
                            client_dialect,
                            winner_id: participant_id(step),
                            announceable: false,
                            rerun_factory: None,
                            rerun_max_rounds: 0,
                        },
                    );
                    return served_combo(&app, activity, participant_id(step), now_ms, wrapped);
                }
                Attempt::Failed {
                    response: Some(response),
                    ..
                } => last_failure = Some(response),
                _ => {}
            }
        }
    }

    // Every step was tried. Where a provider gave a specific error, that is more informative than
    // anything synthesized here, so it is returned. Where the accounts were the problem, the answer
    // is our own 429 with the earliest known recovery.
    failed_combo(&app, activity, attempted.as_deref(), now_ms);
    match last_failure {
        Some(r) => r,
        None => {
            // The wait is named in the message, not only in `Retry-After`. A header is invisible to
            // whoever reads the error body, and "cooling down" without a duration reads as broken:
            // the operator cannot tell a four-second backoff from a four-day one.
            let wait_secs = app
                .health
                .earliest_reset()
                .filter(|until| *until > now_ms)
                .map(|until| until.saturating_sub(now_ms) / 1000);
            let message = skipped_steps_message(&skip_causes, wait_secs);
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                client_dialect,
                "no_usable_accounts",
                &message,
            );
            if let Some(until) = app.health.earliest_reset() {
                let secs = until.saturating_sub(now_ms) / 1000;
                if secs > 0 {
                    if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                        response.headers_mut().insert("retry-after", v);
                    }
                }
            }
            response
        }
    }
}

/// A duration in the units a person reads, for a message that has to stay short.
///
/// One unit of detail below the largest: an hour and four minutes is `1h 4m`, not `3844s`, because
/// the reader's question is "should I wait, or should I go and fix something".
fn humanize_wait(secs: u64) -> String {
    if secs == 0 {
        return "moments".to_owned();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let (minutes, rest) = (secs / 60, secs % 60);
    if minutes < 60 {
        return if rest == 0 {
            format!("{minutes}m")
        } else {
            format!("{minutes}m {rest}s")
        };
    }
    let (hours, rest) = (minutes / 60, minutes % 60);
    if rest == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {rest}m")
    }
}

fn budget_lease_response(
    mut response: Response,
    lease: crate::kv_budget::KvBudgetLease,
) -> Response {
    let inner = std::mem::replace(response.body_mut(), AxumBody::empty());
    *response.body_mut() = AxumBody::new(crate::kv_budget::BudgetedBody::new(inner, lease));
    response
}

/// A boxed call to [`dispatch_routed`], for the one place that re-enters it.
///
/// An async function that calls itself has an infinitely recursive future type, which the compiler
/// cannot lay out. Routing the recursive call through a `BoxFuture` gives it a concrete type at
/// that point, so the recursion terminates in the type system.
fn dispatch_routed_boxed(
    request: DispatchRequest,
    fallback_depth: u8,
    table: Option<Arc<RouteTable>>,
) -> futures_util::future::BoxFuture<'static, Response> {
    Box::pin(dispatch_routed(request, fallback_depth, table))
}

/// Why one step was skipped, so the terminal refusal can name the real cause.
///
/// Before this existed every skipped step was attributed to a cooldown, which is only true for one
/// of the four reasons a step can be skipped. A request whose provider had disappeared from the
/// loaded snapshot was told to wait for accounts to recover; a request whose route could not carry
/// its modality was told the same. Both send an operator to the wrong state, and the second one
/// hides a route that will never serve that modality no matter how long it waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SkipCause {
    /// The step's provider index does not resolve in the loaded snapshot.
    ProviderMissing,
    /// No account of the step can serve the model right now.
    NoUsableAccount,
    /// The step's provider declares it cannot carry the request's input modality.
    UnsupportedModality,
    /// The step's provider does not declare the capability this route requires.
    UnsupportedCapability,
    /// The step's base URL and route path do not form an absolute URL.
    UnbuildableUrl,
}

impl SkipCause {
    /// The clause this cause contributes to the terminal refusal, in the operator's terms.
    fn clause(self) -> &'static str {
        match self {
            SkipCause::ProviderMissing => {
                "the loaded model table no longer holds a provider for this route (reload the snapshot)"
            }
            SkipCause::NoUsableAccount => "no account for this model is usable right now",
            SkipCause::UnsupportedModality => {
                "the route cannot carry this input modality and has no usable default"
            }
            SkipCause::UnsupportedCapability => {
                "the provider does not declare the capability this route requires"
            }
            SkipCause::UnbuildableUrl => {
                "the provider's base URL did not form an absolute URL for this route"
            }
        }
    }
}

/// The sentence a request gets when every step was skipped, naming each distinct cause once.
///
/// `wait_secs` is the earliest known recovery, and is only apposite when an account is actually
/// cooling down: a missing provider or an unbuildable URL does not recover with time, so appending a
/// wait to it would promise something the relay cannot know.
fn skipped_steps_message(causes: &[SkipCause], wait_secs: Option<u64>) -> String {
    let mut seen: Vec<SkipCause> = Vec::new();
    for cause in causes {
        if !seen.contains(cause) {
            seen.push(*cause);
        }
    }
    if seen.is_empty() {
        return "every step for this model was skipped".to_owned();
    }
    let clauses: Vec<&str> = seen.iter().map(|c| c.clause()).collect();
    if let Some(secs) = wait_secs.filter(|s| *s > 0) {
        if seen.contains(&SkipCause::NoUsableAccount) {
            return format!(
                "nothing served this model: {}; the earliest account recovery is in {}",
                clauses.join("; "),
                humanize_wait(secs)
            );
        }
    }
    format!("nothing served this model: {}", clauses.join("; "))
}

/// A stable per-step key for the account round-robin counter.
///
/// The counter is per step rather than per provider so that two steps on one provider do not advance
/// each other's position — otherwise adding a second step would silently change which key the first
/// one uses.
/// What one attempt at a step produced.
enum Attempt {
    /// The step was not usable; the caller should try the next one.
    Skip {
        /// Whether the sticky preference was cleared by this attempt.
        sticky_cleared: bool,
        /// The configured default to retry against, when the step could not carry the modality.
        fallback_target: Option<String>,
        /// Why the step was skipped, so the terminal refusal can say it.
        cause: SkipCause,
    },
    /// The step answered with a success; the response is ready for the client.
    Answer(Response),
    /// The step failed, but the request is still viable: the caller should try the next step.
    Failed {
        /// The failure response, when there is one to show the client.
        response: Option<Response>,
        /// Whether the sticky preference was cleared by this attempt.
        sticky_cleared: bool,
        /// Whether trying this same step on **another of its accounts** could plausibly succeed.
        ///
        /// `true` for the failures that are a property of one credential or one moment — a spent
        /// window, a throttled key, an unreachable host — because the sibling accounts are still
        /// there and a provider holding several must not fail a request the second one can serve.
        ///
        /// `false` for the failures that are a property of the *request*: a malformed body, a path
        /// the upstream does not have, a refusal of the endpoint itself. Re-sending the same bytes
        /// to the same step under a different credential cannot change that answer, so retrying
        /// only burns an attempt and delays the step the combo configured for exactly this case.
        retry_accounts: bool,
    },
    /// The step failed in a way that ends the request rather than the attempt.
    Fatal {
        /// The response to send the client.
        response: Response,
    },
}

/// The inputs one attempt at a step needs.
///
/// Grouped into a struct because an attempt runs in more than one place — the ordinary sequential
/// fallback and the hedged race — and a function with twenty parameters is not reviewable.
struct AttemptContext<'a> {
    /// The relay's shared state.
    app: &'a AppState,
    /// The routing table the request is pinned to.
    table: Arc<RouteTable>,
    /// The wall clock the account selection uses.
    now_ms: u64,
    /// The combo the request resolved to, when it resolved to one.
    combo: Option<&'a mr_core::snapshot::ComboPlan>,
    /// The dialect the client speaks.
    client_dialect: Dialect,
    /// The client's method.
    method: &'a Method,
    /// The client's headers.
    headers: &'a HeaderMap,
    /// What the body scan found.
    head: RequestHead,
    /// The request body, as received.
    bytes: &'a Bytes,
    /// The model the client asked for.
    model_name: &'a str,
    /// The route the client asked for.
    route: RouteKind,
    /// The request URI.
    uri: &'a Uri,
    /// The reasoning level the client asked for, if any.
    level_override: Option<ReasoningLevel>,
    /// The quota settings the cooldown calculation uses.
    quota: &'a mr_core::config::QuotaConfig,
    /// The admission ticket for this request, when the key is metered.
    ticket: Option<Arc<crate::quota::QuotaTicket>>,
    /// When the request started.
    started: std::time::Instant,
    /// How long authentication took.
    auth_done: std::time::Duration,
    /// How long the body scan took.
    scan_done: std::time::Duration,
    /// How long the route lookup took.
    lookup_done: std::time::Duration,
    /// The combo step the sticky preference points at, when there is one.
    sticky_index: Option<usize>,
    /// The provider whose KV budget the caller already reserved, when it did. See
    /// [`DispatchRequest::pre_admitted`].
    pre_admitted: Option<String>,
}

/// What one candidate account can contribute to an attempt.
///
/// The distinction is the whole point of this type: a credential that cannot be resolved is a
/// statement about *one account*, so it moves the step to its next candidate, while a vault that
/// cannot be read is a statement about the relay and ends the request.
enum AccountCredential {
    /// The account can serve. `credential` is what goes on the wire, if anything.
    Usable {
        credential: Option<String>,
        /// Platform-specific headers an OAuth credential contributes, beyond its bearer token.
        oauth_headers: Vec<(HeaderName, HeaderValue)>,
        /// The full token set, for the steps that need more than the access token itself.
        oauth_tokens: Option<mr_core::oauth::TokenSet>,
    },
    /// This account's credential cannot be used; the step should try its next candidate.
    Unusable(CredentialMiss),
    /// The vault could not be read at all, so no account's credential can be judged.
    VaultUnreadable(mr_core::error::Error),
}

/// Why one account's credential could not be used.
enum CredentialMiss {
    /// Nothing is stored under the account's vault reference — the normal state between adding an
    /// account and attaching its key, and not something waiting can fix.
    Missing,
    /// Something is referenced but cannot be turned into a usable credential: the OAuth metadata is
    /// missing or mismatched, the platform is not in this build, or the refresh was rejected.
    Unusable,
}

/// The result of looking an account's static credential up in the vault.
enum StoredCredential {
    /// A value is stored under the reference.
    Found(String),
    /// Nothing is stored under it, or the account names no reference at all.
    Miss,
}

/// Look up one account's static credential, giving a miss a second look at the file.
///
/// The relay's vault is a snapshot taken when the process started, while the panel and `mr` write
/// credentials into that same vault at any moment — so "nothing under this reference" can simply
/// mean "nothing there when we started". Re-reading once turns a permanent-looking failure into a
/// credential that works as soon as it is added, without waiting for a restart nobody knows to
/// perform. A vault that cannot be read at all is reported as such rather than as a missing secret:
/// that is a statement about the relay, not about this account.
///
/// A compiled static account always names a vault slot — `account_plan` resolves the conventional
/// id for it — so an account with no reference at all has nothing to attach, which is the same
/// problem as a reference with nothing stored under it.
fn stored_credential(
    app: &AppState,
    account: &mr_core::snapshot::AccountPlan,
) -> Result<StoredCredential, mr_core::error::Error> {
    let Some(id) = account.secret_ref.as_deref() else {
        return Ok(StoredCredential::Miss);
    };
    match app.vault.get(id) {
        Ok(Some(value)) => Ok(StoredCredential::Found(value)),
        Ok(None) => match app.vault.refreshed() {
            Ok(fresh) => match fresh.get(id) {
                Ok(Some(value)) => {
                    tracing::info!(
                        account = %account.id,
                        "credential was stored after the relay started; using it"
                    );
                    Ok(StoredCredential::Found(value))
                }
                Ok(None) => Ok(StoredCredential::Miss),
                // The file could not be re-read at all: that is a vault problem, not a missing
                // secret, and it is reported as such.
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    }
}

/// Whether a step's auth kind can be attached to a request that carries **no** credential.
///
/// Only [`AuthKind::Bearer`] can, and it is the same exemption `Provider::validate` makes when it
/// allows an account with nothing attached: a local llama.cpp or Ollama server accepts requests with
/// no credential at all, and a provider pointed at one is configured with an account that has no
/// secret. Every other auth kind names a header, a query parameter or a platform that exists to
/// *carry* a value, so a request without one would be rejected upstream as a malformed request; for
/// those, an account with nothing attached stays the refusal it is today. This is deliberately a
/// property of the auth kind alone — no provider id, host or base URL is consulted, so it holds for
/// any provider configured this way.
fn auth_tolerates_no_credential(auth: &AuthKind) -> bool {
    matches!(auth, AuthKind::Bearer)
}

/// Whether this account was configured with no credential of its own at all.
///
/// `Provider::validate` treats "an account with no credential" as a choice an operator is allowed to
/// make for a bearer-token provider, and on that side of the line the test is easy: the account has
/// neither a `secret_ref` nor an `oauth_ref` in the configuration. The relay only ever sees the
/// compiled plan, where the compiler has already resolved a missing `secret_ref` to the
/// conventional [`mr_core::secret::account_secret_id`] slot — the same path `mr account add` writes
/// to. So the configuration's `None` has to be recognised indirectly: an account whose reference is
/// *nothing but* that conventional fallback was never given a credential of its own, while one that
/// names a slot of its own was.
///
/// This is why the distinction matters rather than being pedantic. An account that names its own
/// slot and has nothing stored under it is an account someone is still going to fill in — the step
/// must step over it and go on to a sibling that does have a credential, which is what the
/// `provider/{id}/account/{label}-credential` tests pin. An account that was never given a
/// reference at all is the local-server case, and it is the only one of the two that may run with no
/// credential.
///
/// No provider id, host, base URL or model is consulted — only whether the reference is the
/// compiler's own fallback for this account, so nothing here is specific to any upstream.
fn account_names_no_own_slot(step: &StepPlan, account: &mr_core::snapshot::AccountPlan) -> bool {
    let conventional =
        mr_core::secret::account_secret_id(step.provider_id.as_str(), account.id.as_str());
    account.secret_ref.as_deref() == Some(conventional.as_str())
}

/// Resolve one account's credential, without treating a missing one as an error.
///
/// Separate from [`attempt_step`] so each per-account case — a missing secret, an OAuth reference
/// with no metadata, a refresh the provider rejected — can be exercised without a live upstream.
async fn account_credential(
    app: &AppState,
    step: &StepPlan,
    account: &mr_core::snapshot::AccountPlan,
) -> AccountCredential {
    let Some(oauth_ref) = account.oauth_ref.as_deref() else {
        // Both halves are required: the step's auth kind has to be one that a credential-less
        // request is valid for, *and* the account has to be one that was configured with no
        // credential. Either alone would take the `Authorization` header away from requests that
        // are supposed to keep it.
        let usable_without_credential =
            auth_tolerates_no_credential(&step.auth) && account_names_no_own_slot(step, account);
        let stored = match stored_credential(app, account) {
            Ok(stored) => stored,
            Err(e) => return AccountCredential::VaultUnreadable(e),
        };
        return match stored {
            StoredCredential::Found(value) => AccountCredential::Usable {
                credential: Some(value),
                oauth_headers: Vec::new(),
                oauth_tokens: None,
            },
            // A provider whose account needs no credential serves this account with no credential
            // at all, and the request goes upstream with no `Authorization` header — `apply_auth`
            // attaches nothing for a `None` credential rather than sending an empty header, which
            // the upstream would reject as malformed. Nothing is logged as a warning either, because
            // for such a provider this is the configured, intended state and not a fault to fix.
            StoredCredential::Miss if usable_without_credential => AccountCredential::Usable {
                credential: None,
                oauth_headers: Vec::new(),
                oauth_tokens: None,
            },
            // Nothing stored, and an account that has a vault slot of its own to be filled: the
            // operator has an account that cannot serve yet. Naming it is what sends them to the
            // account that needs a key.
            StoredCredential::Miss => {
                tracing::warn!(account = %account.id, "no credential in the vault for this account");
                AccountCredential::Unusable(CredentialMiss::Missing)
            }
        };
    };

    let oauth_id = mr_core::OAuthCredentialId::from_raw(oauth_ref);
    let AuthKind::OAuth { platform } = &step.auth else {
        tracing::error!(account = %account.id, "OAuth account is attached to non-OAuth provider auth");
        return AccountCredential::Unusable(CredentialMiss::Unusable);
    };
    let record = match app.oauth_store.get_oauth_credential(&oauth_id) {
        Ok(Some(record)) if record.platform == *platform => record,
        _ => {
            tracing::warn!(account = %account.id, "OAuth credential metadata is missing or mismatched");
            return AccountCredential::Unusable(CredentialMiss::Unusable);
        }
    };
    let Some(oauth_platform) = oauth_platform(platform) else {
        tracing::error!(platform, "OAuth platform is not available in this build");
        return AccountCredential::Unusable(CredentialMiss::Unusable);
    };
    let Some(refresher) = app.oauth_refreshers.get(platform) else {
        tracing::warn!(
            platform,
            "no OAuth refresher is registered for this platform"
        );
        return AccountCredential::Unusable(CredentialMiss::Unusable);
    };
    match refresher.ensure_fresh(&record.vault_ref, false).await {
        Ok(tokens) => AccountCredential::Usable {
            credential: Some(tokens.access_token.clone()),
            oauth_headers: oauth_platform.apply_credential(&tokens),
            oauth_tokens: Some(tokens),
        },
        Err(failure) => {
            // A refresh the provider rejected is this account's problem. A sibling account may
            // hold a credential that works, so the step moves on rather than failing outright.
            tracing::warn!(platform, class = %failure.class, "OAuth credential refresh failed");
            AccountCredential::Unusable(CredentialMiss::Unusable)
        }
    }
}

/// The client-facing reason a step had candidate accounts but no usable credential among them.
///
/// Both causes are reported rather than only the first, because one step can mix them and naming a
/// single one would describe a different problem from the one that exists.
fn no_credential_message(
    provider: &mr_core::snapshot::ProviderPlan,
    model: &str,
    missing: usize,
    unusable: usize,
) -> String {
    let mut causes = Vec::with_capacity(2);
    if missing == 1 {
        causes.push("1 account has no credential in the vault".to_owned());
    } else if missing > 1 {
        causes.push(format!(
            "{missing} accounts have no credential in the vault"
        ));
    }
    if unusable == 1 {
        causes.push("1 account's credential cannot be used".to_owned());
    } else if unusable > 1 {
        causes.push(format!("{unusable} accounts' credentials cannot be used"));
    }
    format!(
        "no account for model \"{model}\" on `{}` has a usable credential: {}; attach one with \
         `mr account add {} --secret-stdin`",
        provider.id,
        causes.join(", "),
        provider.id
    )
}

/// Run one attempt at one step.
///
/// This is the body of the step loop, extracted so that the same code can run in the ordinary
/// sequential fallback and in the hedged race, where several attempts run at once. The two paths
/// must not differ in any way that matters: a race that dispatched requests slightly differently
/// from the sequential path would be a race nobody could reason about.
///
/// The `is_sticky_step` flag says whether this step is the one the combo's sticky preference
/// points at. A step that turns out to be unusable clears that preference, which is why the
/// outcome carries a `sticky_cleared` flag rather than the caller deciding it.
/// The client's own conversation marker, when it sent one as a header.
///
/// Codex keys its prompt cache on `prompt_cache_key`, and the official CLI sends its conversation
/// id as `session-id` on every request; a generic client tends to use `x-session-id`. Passing the
/// client's own marker through is the whole of the affinity the upstream needs — inventing one here
/// would either fold two unrelated conversations into one cache bucket or hand the provider a
/// session the client cannot keep stable across its own turns.
fn client_session_hint(headers: &HeaderMap) -> Option<&str> {
    /// The headers a client is known to use for this. Order is preference, not priority.
    const NAMES: [&str; 2] = ["session-id", "x-session-id"];
    /// Past this a header is not an id a provider can key anything on, and forwarding it would
    /// only put an arbitrary-length string in front of the upstream's validation.
    const MAX_LEN: usize = 128;
    NAMES.iter().find_map(|name| {
        let value = headers.get(*name)?.to_str().ok()?.trim();
        (!value.is_empty() && value.len() <= MAX_LEN).then_some(value)
    })
}

async fn attempt_step(
    actx: &AttemptContext<'_>,
    step: &StepPlan,
    is_sticky_step: bool,
    ignore_cooldowns: bool,
) -> Attempt {
    let app = actx.app;
    let table = actx.table.clone();
    // The cooldown view is taken per attempt rather than once for the whole request, so that a
    // race sees the accounts that the other participants just cooled down.
    let health = app.health.view();
    let now_ms = actx.now_ms;
    let combo = actx.combo;
    let client_dialect = actx.client_dialect;
    let method = actx.method;
    let headers = actx.headers;
    let head = actx.head;
    let bytes = actx.bytes;
    let model_name = actx.model_name;
    let route = actx.route;
    let uri = actx.uri;
    let level_override = actx.level_override;
    let quota = actx.quota;
    let ticket = actx.ticket.clone();
    let started = actx.started;
    let auth_done = actx.auth_done;
    let scan_done = actx.scan_done;
    let lookup_done = actx.lookup_done;
    let mut sticky_cleared = false;
    let mut fallback_target: Option<String> = None;
    let Some(provider) = table.snapshot.providers().get(step.provider_index as usize) else {
        if is_sticky_step {
            if let Some(combo) = combo {
                app.sticky_success.clear(&combo.id);
            }
            sticky_cleared = true;
        }
        return Attempt::Skip {
            sticky_cleared,
            fallback_target: fallback_target.take(),
            cause: SkipCause::ProviderMissing,
        };
    };
    // The gate before the loop dropped every incapable step it could see, but a hedged participant
    // or a rerun can still arrive here. Skipping rather than dialling keeps the request honest and
    // degrades to the next step instead of failing it: the provider would answer in a shape this
    // route is not.
    if !provider.capabilities.contains(&route.capability()) {
        return Attempt::Skip {
            sticky_cleared,
            fallback_target: fallback_target.take(),
            cause: SkipCause::UnsupportedCapability,
        };
    }

    // --- The token-counting route, when this step's upstream cannot count. ---
    //
    // `POST /v1/messages/count_tokens` asks a provider how many tokens a request would cost. Only
    // an Anthropic upstream can answer that: the endpoint exists in Anthropic's API and nowhere
    // else, so asking any other provider would mean asking for a *generation* instead — a billed
    // answer, in the wrong shape, to a question that was not a question.
    //
    // So a step whose dialect is not Anthropic is not called at all. The relay answers locally with
    // an estimate and marks it with a header, so a client can tell the difference between a number
    // the provider counted and a number the relay guessed.
    //
    // ZCode is the dialect that makes this rule visible: its upstream *is* an Anthropic Messages
    // endpoint, so a count could plausibly be proxied to `…/v1/messages/count_tokens` — but the app
    // never counts that way, and a count sent down the generation path would be billed as a
    // generation. It stays local.
    //
    // The answer is produced before an account is chosen: there is no upstream to authenticate to,
    // no credential to read, and no request to bill. It is an ordinary local answer — the same
    // shape an upstream failure takes — so a step that fails further down still falls through to
    // the next one in the ordinary way.
    if route == RouteKind::CountTokens && step.dialect != Dialect::Anthropic {
        return Attempt::Answer(crate::count_tokens::estimate_response(bytes));
    }

    let account_counter = table.advance_account_rr(step);
    let candidates = if ignore_cooldowns {
        select::account_candidates_ignoring_cooldowns(
            provider,
            provider.key_strategy,
            account_counter,
            &health,
            now_ms,
            &step.model,
        )
    } else {
        select::account_candidates(
            provider,
            provider.key_strategy,
            account_counter,
            &health,
            now_ms,
            &step.model,
        )
    };
    if candidates.is_empty() {
        if is_sticky_step {
            if let Some(combo) = combo {
                app.sticky_success.clear(&combo.id);
            }
            sticky_cleared = true;
        }
        return Attempt::Skip {
            sticky_cleared,
            fallback_target: fallback_target.take(),
            cause: SkipCause::NoUsableAccount,
        };
    }

    // --- The credential, for each candidate until one can serve. ---
    //
    // The credential comes from the vault, never from the configuration, and is never logged.
    // OAuth references resolve to a full token set and platform-specific request headers.
    //
    // A credential is a property of the *account*, so an account without one is skipped and the
    // next candidate is tried. Failing the step here would refuse a request a sibling account can
    // serve perfectly well, which is the whole point of a provider holding several keys. Cooling
    // the account down would be wrong as well: a missing credential will not reappear with time,
    // and the cooldown would keep excluding an account that is fine from every later request.
    let mut without_credential = 0usize;
    let mut unusable_credential = 0usize;
    // Every candidate with a usable credential, in selection order. The loop below takes them one
    // at a time: an account that is throttled, spent, or unreachable is cooled down and the *next
    // one on the same step* is tried, because a provider holding five accounts is exactly the
    // situation where one exhausted account must not fail the request. Stopping at the first
    // failure is what made a Codex account with a spent window hide four siblings with quota.
    let mut usable_candidates = Vec::new();
    for candidate in candidates {
        match account_credential(app, step, candidate).await {
            AccountCredential::Usable {
                credential,
                oauth_headers,
                oauth_tokens,
            } => usable_candidates.push((candidate, credential, oauth_headers, oauth_tokens)),
            AccountCredential::Unusable(CredentialMiss::Missing) => without_credential += 1,
            AccountCredential::Unusable(CredentialMiss::Unusable) => unusable_credential += 1,
            AccountCredential::VaultUnreadable(e) => {
                // A vault that cannot be decrypted is not an account problem: every account would
                // fail the same way, and reporting it as one account's failure would hide the real
                // cause behind a symptom.
                tracing::error!(error = %e, "the credential vault could not be read");
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        client_dialect,
                        "vault_error",
                        "the credential vault could not be read",
                    ),
                };
            }
        }
    }
    // The first candidate in selection order, not the last: round-robin and priority both encode
    // the operator's intent in this order.
    let Some((account, credential, oauth_headers, oauth_tokens)) =
        usable_candidates.into_iter().next()
    else {
        // Every candidate was unusable for the same kind of reason, and it is not a cooldown: no
        // amount of waiting produces a credential. Saying so sends the operator to the account that
        // needs one; blaming a cooldown would send them to look at the wrong state.
        return Attempt::Failed {
            response: Some(error_response(
                StatusCode::TOO_MANY_REQUESTS,
                client_dialect,
                "no_usable_accounts",
                &no_credential_message(
                    provider,
                    &step.model,
                    without_credential,
                    unusable_credential,
                ),
            )),
            sticky_cleared,
            retry_accounts: true,
        };
    };

    // --- Dialect translation, per step, because the upstream dialect belongs to the provider
    // and a combo may mix dialects across its steps. ---
    //
    // `translate_request` returns `None` when the client and the upstream speak the same shape,
    // and that is the fast path: the original bytes go upstream with no parse and no re-encode,
    // which preserves provider-specific fields this router does not understand — a re-encode
    // would silently delete them. A body is built only when the dialects actually differ.
    let ctx = TranslateCtx {
        route,
        client_dialect,
        upstream_dialect: provider.dialect,
        upstream_model: step.model.as_str(),
        client_streaming: head.wants_stream(),
        session_id: client_session_hint(headers),
    };
    let translated = match translate_request(&ctx, bytes) {
        Ok(t) => t,
        Err(e) => {
            return Attempt::Fatal {
                response: error_response(
                    StatusCode::BAD_REQUEST,
                    client_dialect,
                    "invalid_request_error",
                    &e.to_string(),
                ),
            };
        }
    };
    // Destructured in one move rather than read field-by-field: taking `path_override` by
    // reference and then moving `body` out of the same value is a borrow conflict, and cloning
    // the body to sidestep it would copy the whole request for every translated call.
    let (mut outgoing, force_stream, dialect_content_type, dialect_path, translated_body) =
        match translated {
            Some(t) => (
                t.body,
                t.force_stream,
                Some(t.content_type),
                t.path_override,
                true,
            ),
            None => (bytes.clone(), false, None, None, false),
        };
    // Does any rewrite below have the right to parse this body at all? A multipart upload or a
    // binary response body is forwarded verbatim, and every pass that assumes JSON is skipped.
    let inspect_json = outgoing_is_json(headers, translated_body, &outgoing);
    // Ask for usage on streams. An OpenAI-compatible upstream reports none unless it is asked, and
    // the relay then records the request as costing zero tokens — which the quota layer sums as
    // zero, so every token cap silently stops binding for streamed traffic. Only a body that is
    // already streaming is touched.
    if inspect_json && matches!(step.dialect, Dialect::OpenAi | Dialect::Copilot) {
        outgoing = with_stream_usage(outgoing);
    }
    if step.dialect == Dialect::Antigravity && inspect_json {
        match inject_antigravity_project(&outgoing, oauth_tokens.as_ref()) {
            Ok(body) => outgoing = body.into(),
            Err(message) => {
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        client_dialect,
                        "server_error",
                        message,
                    ),
                };
            }
        }
    }

    // --- The model field, when the JSON merge below will not reach it. ---
    //
    // A multipart body is forwarded as it was uploaded, so the resolved model cannot be written
    // into it by the JSON merge. The `model` part was located during the route scan, and only that
    // value is replaced: the boundary, the audio, the file name and every other field are copied
    // through unchanged, so a request for a model the provider serves under another name reaches
    // the provider with that name rather than with the relay's composite one.
    if !inspect_json {
        if let Some(range) = head.model {
            let current = range.slice(bytes);
            if current != step.model.as_bytes() {
                let mut retargeted =
                    Vec::with_capacity(bytes.len() + step.model.len() - current.len());
                retargeted.extend_from_slice(&bytes[..range.start]);
                retargeted.extend_from_slice(step.model.as_bytes());
                retargeted.extend_from_slice(&bytes[range.end..]);
                outgoing = retargeted.into();
            }
        }
    }

    // --- The reasoning level, applied to the bytes that actually go upstream. ---
    //
    // This runs *after* translation so it lands on the upstream's own shape: an OpenAI client
    // routed to Gemini gets `generationConfig.thinkingConfig`, not a field Gemini never reads.
    // It also runs on the passthrough path, where `outgoing` is still the client's own bytes —
    // which is exactly why the rewrite is a merge rather than a re-encode, and why it retargets
    // the `model` field in the same pass.
    if inspect_json {
        match apply_reasoning_level(&outgoing, step, translated_body, level_override) {
            Ok(Some(body)) => outgoing = body.into(),
            Ok(None) => {}
            Err(message) => {
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::BAD_REQUEST,
                        client_dialect,
                        "invalid_request_error",
                        &message,
                    ),
                };
            }
        }
    }

    // A body that is not JSON has no top-level `model` to rewrite and no content array to inspect.
    // It is on its way upstream exactly as the client wrote it, so the pass is skipped rather than
    // turned into the 400 a parse failure would produce for a multipart or binary upload.
    if inspect_json {
        // Modality detection runs on the translated upstream shape, so the same helper can inspect
        // both passthrough and cross-dialect requests. Only a differing configured default changes
        // the model field; the request content is never dropped or rewritten.
        let mut modality_body: serde_json::Value = match serde_json::from_slice(&outgoing) {
            Ok(body) => body,
            Err(_) => {
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::BAD_REQUEST,
                        client_dialect,
                        "invalid_request_error",
                        "the request body must be valid JSON to inspect its modalities",
                    ),
                };
            }
        };
        match crate::upstream::rewrite_model_for_modalities(
            &mut modality_body,
            |modality| step.accepts_modality(modality),
            table.snapshot.routing_defaults(),
        ) {
            Ok(Some(target)) => {
                // This route cannot carry the request's modality. The next step of the combo may
                // be able to, so the step is skipped rather than failed. The configured default
                // is remembered and used after the loop, where a *complete* reroute is possible:
                // the target is resolved against the same snapshot, with its own provider,
                // account, credential, URL, dialect and headers.
                fallback_target = Some(target);
                return Attempt::Skip {
                    sticky_cleared,
                    fallback_target: fallback_target.take(),
                    cause: SkipCause::UnsupportedModality,
                };
            }
            Ok(None) => {}
            Err(error) => {
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::BAD_REQUEST,
                        client_dialect,
                        "invalid_request_error",
                        &error.with_model(&step.model).to_string(),
                    ),
                };
            }
        }
    }

    // The Copilot base host comes from the OAuth token, and replaces the configured base URL
    // entirely: the token names the host the request must go to.
    let copilot_host = oauth_tokens
        .as_ref()
        .and_then(|t| t.extra_str("copilot_api_host"));
    let mut url_step = (*step).clone();
    if step.dialect == Dialect::Copilot {
        if let Some(host) =
            copilot_host.filter(|host| host.starts_with("https://") && !host.contains('@'))
        {
            url_step.base_url = host.trim_end_matches('/').to_owned();
        }
    }
    // The override table is read for its own sake even when the translator produced a path, so
    // that a provider still keyed by the raw request path is reported rather than silently
    // ignored. Copilot is the exception: it has no upstream path of its own for an override to
    // replace.
    let operator_path = if step.dialect == Dialect::Copilot {
        None
    } else {
        route_override_for(provider, route, uri.path())
    };
    let url_path = upstream_path(step.dialect, route, dialect_path.as_deref(), operator_path);
    let Some(built_url) = build_url(&url_step, url_path) else {
        return Attempt::Skip {
            sticky_cleared,
            fallback_target: fallback_target.take(),
            cause: SkipCause::UnbuildableUrl,
        };
    };

    // --- Named placeholders in the base URL. ---
    //
    // A catalogue entry cannot spell some endpoints without a value that belongs to the *account*:
    // Cloudflare Workers AI addresses `…/accounts/{account_id}/…` and Snowflake addresses
    // `{account}.snowflakecomputing.com`. The name is in the catalogue; the value is on the
    // account, which is why this happens here, once the account is chosen, and not when the
    // snapshot is compiled.
    //
    // A name the account does not supply is reported rather than sent. Literal braces reach the
    // upstream as `%7Baccount_id%7D` and come back as a 404 that blames the provider for a local
    // configuration mistake — the exact failure this exists to prevent.
    //
    // The attempt may still be served by a *sibling account* of the same provider, because the
    // value is per account: `retry_accounts` stays true, and the response below is what the client
    // sees if no account of no step can fill it.
    let url = match mr_core::placeholders::substitute(&built_url, &account.placeholders) {
        Ok(url) => url,
        Err(problem) => {
            let message = format!(
                "provider {:?} account {:?}: {}",
                provider.id.as_str(),
                account.label.as_deref().unwrap_or(account.id.as_str()),
                problem.message()
            );
            return Attempt::Failed {
                response: Some(error_response(
                    StatusCode::BAD_GATEWAY,
                    client_dialect,
                    "account_placeholder_unfilled",
                    &message,
                )),
                sticky_cleared,
                retry_accounts: true,
            };
        }
    };

    // --- The proxy this attempt must egress through. ---
    //
    // Resolved here, after the account is known and before anything is sent, because the proxy
    // is a property of the *account*: two accounts of one provider can egress differently, and
    // that is the whole reason the override exists.
    //
    // A proxy that cannot be used at all — a missing vault secret, an unusable URL — is a
    // configuration problem, and the request fails rather than quietly going out directly. The
    // failure is deliberately *not* a routing fallback: trying the next step would send the
    // request somewhere the operator did not intend.
    let proxy = match resolve_proxy(app, &table.snapshot, provider, account) {
        Ok(choice) => choice,
        Err(message) => {
            return Attempt::Fatal {
                response: error_response(
                    StatusCode::BAD_GATEWAY,
                    client_dialect,
                    "proxy_error",
                    &message,
                ),
            };
        }
    };
    let proxy_endpoint = proxy.as_ref().map(|choice| choice.endpoint.clone());
    // Does this attempt's provider already hold a reservation made on its behalf?
    //
    // The batch lane reserves an item's KV tokens *before* dispatching the item, for two reasons that
    // the request path cannot satisfy: it must not start an item it cannot run (the lane would rather
    // wait than fail 4 000 items as the pool fills), and one item must produce exactly one ledger row,
    // which is only possible if the lease outlives the dispatch. So when the lane's reservation names
    // this provider, the reservation is the lane's and taking a second one here would double-count the
    // item — the pool would report half the capacity the operator configured.
    //
    // The comparison is by provider id because that is the pool's key: the budget belongs to the
    // endpoint, not to the route. A *fallback* to a different provider does not match, so it reserves
    // normally, which is correct and deliberately over-reserves by one item — the alternative is a
    // fallback that runs unbilled, and a cost report that quietly omits what the fallback cost.
    let pre_admitted_here = actx.pre_admitted.as_deref() == Some(step.provider_id.as_str());
    let budget_lease = if pre_admitted_here {
        None
    } else if provider.kv_pool_tokens.is_some()
        || provider.max_concurrent.is_some()
        || (step.dialect == Dialect::ModalVllm && provider.price_per_second.is_some())
    {
        let request_tokens = crate::quota::estimate_prompt_tokens(outgoing.len()).tokens;
        match app.kv_usage.reserve(
            step.provider_id.as_str().to_owned(),
            mr_core::ids::new_id(),
            request_tokens,
            crate::kv_budget::KvBudgetLimits {
                pool_tokens: provider.kv_pool_tokens,
                max_context: step.context_length.map(u64::from).or(provider.max_context),
                max_concurrent: provider.max_concurrent,
            },
            Some(crate::kv_budget::GpuBilling {
                store: Arc::clone(&app.quota_store),
                provider_id: step.provider_id.as_str().to_owned(),
                model: step.model.clone(),
                price_per_second: provider.price_per_second,
                started: std::time::Instant::now(),
            }),
        ) {
            Ok(lease) => Some(lease),
            Err(reason) => {
                let message = match reason {
                    crate::kv_budget::KvBudgetDenial::ContextExceeded => {
                        "the request exceeds the endpoint's configured model context"
                    }
                    crate::kv_budget::KvBudgetDenial::PoolTooSmall => {
                        "the request exceeds the endpoint's configured KV-token pool"
                    }
                    crate::kv_budget::KvBudgetDenial::PoolFull => {
                        "the endpoint's KV-token pool is fully reserved by in-flight requests"
                    }
                    crate::kv_budget::KvBudgetDenial::ConcurrencyFull => {
                        "the endpoint is at its configured maximum concurrent requests"
                    }
                };
                return Attempt::Failed {
                    response: Some(error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        client_dialect,
                        "kv_budget_exceeded",
                        message,
                    )),
                    sticky_cleared,
                    retry_accounts: false,
                };
            }
        }
    } else {
        None
    };

    // --- Cursor: a bidirectional stream, not a request/response pair. ---
    //
    // Every other dialect can be expressed as "send these bytes, read that response", which is what
    // the pooled client below does. Cursor's agent endpoint keeps the request side *open* so the
    // server can ask this client questions while the answer is written — what tools it has, what a
    // blob it wrote in an earlier turn held — and a client that half-closes the stream never gets an
    // answer at all. So `cursor_transport` owns the exchange, and this branch only decides whether
    // the turn can be attempted at all.
    //
    // The request is shaped here, before a socket is opened, so a body that cannot be expressed
    // fails as a client error instead of as a provider one: the difference decides whether the
    // operator's account gets cooled down for a request this router was never able to send.
    let mut cursor_prepared = if step.dialect == Dialect::Cursor {
        if proxy_endpoint.is_some() {
            // Not a limitation of the request but of the transport underneath it, and silence would
            // be worse than refusal: going direct here would quietly defeat an egress policy the
            // operator configured on purpose.
            return Attempt::Fatal {
                response: error_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    client_dialect,
                    "server_error",
                    "this Cursor provider has a proxy configured, and the Cursor transport speaks \
                     HTTP/2 directly: a proxied bidirectional stream is not implemented, so the \
                     turn was refused rather than sent around your egress policy. Remove the proxy \
                     from this provider, or route to Cursor without one.",
                ),
            };
        }
        // Reuse the one place that decides how a credential becomes a header, so a Cursor provider
        // configured with any other auth kind is refused here instead of being sent a header the
        // agent endpoint will not read.
        let mut auth_headers = HeaderMap::new();
        apply_auth(&mut auth_headers, &step.auth, credential.as_deref());
        let authorization = auth_headers
            .get(http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        match authorization {
            None => {
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        client_dialect,
                        "server_error",
                        "the Cursor agent endpoint is authenticated with a bearer credential, and \
                         this provider's account has none: attach one with `mr account add` and set \
                         the provider's auth to `bearer`.",
                    ),
                };
            }
            Some(authorization) => {
                match crate::cursor_transport::prepare(
                    &ctx,
                    &outgoing,
                    authorization,
                    crate::cursor_transport::GHOST_MODE_DEFAULT,
                    Some(step.base_url.as_str()),
                ) {
                    Ok(prepared) => Some(prepared),
                    Err(error) => {
                        // A body this router cannot turn into a run is the client's problem (400);
                        // a transport that cannot be built is ours (503).
                        let (status, kind) = match error {
                            crate::cursor_transport::Error::Request(_) => {
                                (StatusCode::BAD_REQUEST, "invalid_request_error")
                            }
                            _ => (StatusCode::SERVICE_UNAVAILABLE, "server_error"),
                        };
                        return Attempt::Fatal {
                            response: error_response(
                                status,
                                client_dialect,
                                kind,
                                &error.to_string(),
                            ),
                        };
                    }
                }
            }
        }
    } else {
        None
    };

    let initial_body = outgoing.clone();
    let mut current_body = outgoing;
    let mut reconciled_once = false;

    let response = loop {
        let sent = match cursor_prepared.take() {
            // The ordinary path: one request, one response, through the pooled client.
            None => {
                let request = UpstreamRequest {
                    method: method.clone(),
                    url: url.clone(),
                    headers: upstream_headers(
                        provider,
                        step,
                        credential.as_deref(),
                        headers,
                        &head,
                        force_stream,
                        dialect_content_type,
                        &oauth_headers,
                    ),
                    proxy: proxy_endpoint.clone(),
                    body: current_body.clone(),
                    http2: provider.http2,
                    connect_timeout: Duration::from_millis(
                        provider
                            .cold_start_ttfb_timeout_ms
                            .map(|_| step.connect_timeout_ms.max(300_000))
                            .unwrap_or(step.connect_timeout_ms),
                    ),
                    ttfb_timeout: Duration::from_millis(
                        provider
                            .cold_start_ttfb_timeout_ms
                            .unwrap_or(step.ttfb_timeout_ms),
                    ),
                    total_timeout: (step.total_timeout_ms > 0).then(|| {
                        Duration::from_millis(
                            step.total_timeout_ms
                                .max(provider.cold_start_ttfb_timeout_ms.unwrap_or(0)),
                        )
                    }),
                };
                app.relay.upstream().send(request).await
            }
            // The Cursor path: the transport owns the exchange and returns a response whose body
            // streams the rendered answer. Everything downstream — rendering, the usage tap, the
            // quota lease, health and sticky bookkeeping — is the ordinary code.
            //
            // A failure is reported as a connect failure so the existing arms treat it as a
            // provider failure: cool the account down, try the next step, keep the sticky
            // preference cleared. Inventing a special case here would mean a Cursor outage no
            // longer fails over the way every other provider's does.
            Some(prepared) => match crate::cursor_transport::default_exchange() {
                Ok(exchange) => crate::cursor_transport::run(
                    exchange,
                    prepared,
                    crate::cursor_transport::TOTAL_TIMEOUT,
                )
                .await
                .map_err(|error| UpstreamError::Connect(error.to_string())),
                Err(error) => Err(UpstreamError::Tls(error.to_string())),
            },
        };

        match sent {
            Ok(resp) if resp.status.is_success() => break Ok(resp),
            Ok(resp)
                if !reconciled_once
                    // The reconcile retry rewrites the upstream body as a *JSON* request, which a
                    // Cursor turn is not: continuing here would send that JSON to the agent
                    // endpoint as a second run. A refused Cursor turn is simply refused.
                    && step.dialect != Dialect::Cursor
                    && resp.status.is_client_error()
                    && resp.status != StatusCode::UNAUTHORIZED
                    && resp.status != StatusCode::FORBIDDEN
                    && resp.status != StatusCode::TOO_MANY_REQUESTS =>
            {
                let first_frame = resp.body.first_frame_bytes();
                let error_text = first_frame
                    .map(|b| String::from_utf8_lossy(b))
                    .unwrap_or_default();
                let requested_effort = crate::reasoning::extract_requested_effort(&initial_body);

                if let Some(action) =
                    crate::reasoning::inspect_refusal(&error_text, requested_effort.as_deref())
                {
                    if let Some(new_body) =
                        crate::reasoning::apply_reconcile_to_body(&initial_body, &action)
                    {
                        reconciled_once = true;
                        let target_desc = match &action {
                            crate::reasoning::ReconcileAction::Remap(target) => target.as_str(),
                            crate::reasoning::ReconcileAction::DropField => "none (dropped)",
                        };
                        tracing::warn!(
                            provider = %step.provider_id,
                            model = model_name,
                            original = requested_effort.as_deref().unwrap_or("unspecified"),
                            reconciled = target_desc,
                            "reconciling unsupported reasoning effort and retrying upstream attempt once"
                        );
                        current_body = Bytes::from(new_body);
                        continue;
                    }
                }
                break Ok(resp);
            }
            other => break other,
        }
    };

    match response {
        Ok(response) if response.status.is_success() => {
            app.health
                .record_success(step.provider_id.as_str(), account.id.as_str(), &step.model);
            if let Some(combo) = combo.filter(|c| sticky_applies(c)) {
                app.sticky_success.record_success(
                    &combo.id,
                    step,
                    Duration::from_secs(combo.sticky_success_window_secs as u64),
                    std::time::Instant::now(),
                );
            }
            tracing::debug!(
                model = model_name,
                provider = %step.provider_id,
                auth_us = auth_done.as_micros() as u64,
                scan_us = (scan_done - auth_done).as_micros() as u64,
                lookup_us = (lookup_done - scan_done).as_micros() as u64,
                total_us = started.elapsed().as_micros() as u64,
                "served"
            );
            // A Cursor turn is always an event stream on the wire, whatever the client asked for,
            // so the response half is always a translation — including for an OpenAI client, whose
            // request needed none. Reporting `false` here would hand a non-streaming client raw
            // `data:` frames instead of the one document it asked for.
            let response_translated = translated_body || step.dialect == Dialect::Cursor;
            let client_response =
                build_client_response(ctx, response_translated, response, ticket).await;
            // A stored response is recorded on its way out, not after: the body is a stream, and the
            // relay only sees its bytes once. Skipped for a translated answer, whose bytes are this
            // relay's dialect rather than the provider's object.
            let client_response = if route == RouteKind::Responses
                && !response_translated
                && crate::responses::asked_to_store(&initial_body)
            {
                crate::responses::record_while_streaming(
                    app.clone(),
                    initial_body.to_vec(),
                    client_response,
                )
            } else {
                client_response
            };
            Attempt::Answer(match budget_lease {
                Some(mut lease) => {
                    lease.mark_success();
                    budget_lease_response(client_response, lease)
                }
                None => client_response,
            })
        }
        Ok(response) => {
            if is_sticky_step {
                if let Some(combo) = combo {
                    app.sticky_success.clear(&combo.id);
                }
                sticky_cleared = true;
            }
            // A refusal body can carry the account's own recovery instant, and that is worth more
            // than a status-code guess: it is the difference between parking a spent account for
            // the minute it takes to be refused again and parking it until it can actually serve.
            // Read this before the body is handed to the client — after that it is the client's.
            let mut reason = classify_status(response.status, step.dialect == Dialect::ModalVllm);
            let modal_cold_start = reason == select::CooldownReason::ColdStart;
            let mut named_wait = retry_after_ms(&response.headers);
            // Only a refusal is read, and only a refusal small enough to be a refusal: a provider
            // that answers a 429 or a 402 with a stream is not describing a quota window, and
            // buffering it would trade this bug for a memory one. A 400 is read because some
            // providers deliver a spent credit balance under a generic bad-request code.
            let refusal_wait = if matches!(
                response.status,
                StatusCode::TOO_MANY_REQUESTS
                    | StatusCode::PAYMENT_REQUIRED
                    | StatusCode::FORBIDDEN
                    | StatusCode::BAD_REQUEST
            ) {
                response
                    .body
                    .first_frame_bytes()
                    .and_then(|bytes| exhausted_by_body(bytes))
            } else {
                None
            };
            if let Some(refusal) = refusal_wait {
                reason = refusal.reason;
                named_wait = refusal.wait_ms.or(named_wait);
            }
            // A model the *plan* does not include is not a spent account, and the difference is
            // worth two lines: the account keeps serving every other model, so parking it whole
            // would take request capacity away from the models that do work in order to punish
            // one that never will. Cursor reports this under `resource_exhausted`, the same code it
            // uses for a rate limit, and the only field that separates them is the model-switch
            // button on the error — see `cursor::Refusal::model_unavailable`. The transport reads
            // that structure and, once, writes the answer here.
            //
            // The consequence is a cooldown keyed on this model alone (`CooldownScope::Model`), and
            // a request that fails over to the account or step that does have it, which is what an
            // operator asking for a model means by "my account is on the Free plan".
            let model_unavailable = response
                .body
                .first_frame_bytes()
                .is_some_and(|bytes| names_an_unavailable_model(bytes));
            if model_unavailable {
                reason = select::CooldownReason::QuotaExhausted;
            }
            // A refusal used to leave no trace at all, which made the most expensive failure in
            // a combo — the one that silently sends every request to the next step — invisible.
            // The excerpt is what names the cause: `prefill_cap_refused` reads as a rate limit in
            // the status line and is nothing of the sort.
            let excerpt = response
                .body
                .first_frame_bytes()
                .map(|bytes| String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned())
                .unwrap_or_default();
            tracing::warn!(
                model = model_name,
                provider = %step.provider_id,
                status = response.status.as_u16(),
                reason = ?reason,
                model_unavailable,
                named_wait_ms = named_wait,
                body = %excerpt,
                "upstream refused the request"
            );
            let rebuilt = response;
            // A 4xx client error (400, 404, 422…) is normally a statement about the *request*, not
            // about the account: the credential is fine, so the account is not cooled down, and
            // another account of the same step would receive the same bytes and give the same
            // answer. The exception is the one `mentions_a_spent_account` recognises — a 400 that
            // says the balance is empty is about the account, and it never reaches this branch
            // because its reason has already been rewritten to `QuotaExhausted`.
            //
            // It is not a statement about the *combo*, though. An upstream can reject a dialect
            // it does not actually implement, or refuse one model of the several a step's
            // provider hosts, while every other step of the combo is perfectly willing to serve.
            // Treating the 4xx as fatal therefore ended the whole request at the first step that
            // said no, which is precisely the case a combo exists to survive: the request never
            // reached the step configured as its fallback, and the operator saw the refusal of an
            // upstream they had not even chosen.
            //
            // So the failure advances to the next step like any other, carrying the provider's own
            // answer so that a request every step refuses still reports a real cause rather than a
            // synthesized one.
            if matches!(reason, select::CooldownReason::InvalidRequest) {
                return Attempt::Failed {
                    response: Some(build_response(rebuilt, None)),
                    sticky_cleared,
                    retry_accounts: false,
                };
            }
            let effective_scope = if modal_cold_start || model_unavailable {
                mr_core::model::CooldownScope::Model
            } else {
                refusal_wait
                    .and_then(|refusal| refusal.scope)
                    .unwrap_or(provider.cooldown_scope)
            };
            app.health.cool_down(
                &select::cooldown_key(
                    effective_scope,
                    step.provider_id.as_str(),
                    account.id.as_str(),
                    &step.model,
                    reason,
                ),
                select::cooldown_until(
                    reason,
                    app.health.failures(
                        effective_scope,
                        step.provider_id.as_str(),
                        account.id.as_str(),
                        &step.model,
                    ),
                    named_wait,
                    quota,
                ),
                reason,
            );
            Attempt::Failed {
                response: Some(build_response(rebuilt, None)),
                sticky_cleared,
                retry_accounts: true,
            }
        }
        Err(e) => {
            if is_sticky_step {
                if let Some(combo) = combo {
                    app.sticky_success.clear(&combo.id);
                }
                sticky_cleared = true;
            }
            // A proxy failure ends the attempt, and the request, here.
            //
            // The fallback loop exists to survive a *provider* failure by trying a different
            // route. A proxy failure is not that: the egress the operator configured is broken,
            // and the next step might egress directly, which is exactly the silent downgrade a
            // proxy is supposed to prevent. So the request fails, loudly, with the proxy's own
            // error.
            if let UpstreamError::Proxy(message) = &e {
                tracing::warn!(
                    model = model_name,
                    provider = %step.provider_id,
                    error = %e,
                    "the configured proxy could not be used; the request is not being rerouted"
                );
                return Attempt::Fatal {
                    response: error_response(
                        StatusCode::BAD_GATEWAY,
                        client_dialect,
                        "proxy_error",
                        &format!(
                            "the request could not be sent through the configured proxy: {message}"
                        ),
                    ),
                };
            }
            let reason = select::CooldownReason::Backoff;
            app.health.cool_down(
                &select::cooldown_key(
                    provider.cooldown_scope,
                    step.provider_id.as_str(),
                    account.id.as_str(),
                    &step.model,
                    reason,
                ),
                select::cooldown_until(
                    reason,
                    app.health.failures(
                        provider.cooldown_scope,
                        step.provider_id.as_str(),
                        account.id.as_str(),
                        &step.model,
                    ),
                    None,
                    quota,
                ),
                reason,
            );
            tracing::warn!(
                model = model_name,
                provider = %step.provider_id,
                error = %e,
                "upstream failed"
            );
            Attempt::Failed {
                response: None,
                sticky_cleared,
                retry_accounts: true,
            }
        }
    }
}

/// One pending attempt in a race, boxed so that several of them can be polled at once.
type PendingAttempt<'a> = Pin<Box<dyn Future<Output = (usize, Attempt)> + Send + 'a>>;

/// The outcome of a hedged race.
enum HedgeOutcome {
    /// A participant won; the response is ready for the client.
    Won {
        /// The winner's client-facing response.
        response: Response,
        /// The winner, as `provider/model`, so the decision can be recorded.
        winner: String,
    },
    /// No participant produced an answer; the caller continues with the ordinary chain.
    NoWinner {
        /// The last failure seen, to report if the ordinary chain also fails.
        last_failure: Option<Response>,
        /// The configured default to retry against, when a participant could not carry the
        /// modality.
        fallback_target: Option<String>,
        /// Whether the sticky preference was cleared by one of the participants.
        sticky_cleared: bool,
    },
    /// A participant failed in a way that ends the request rather than the race.
    Fatal {
        /// The response the client receives.
        response: Response,
        /// The participant whose failure ended the race, as `provider/model`.
        attempted: String,
    },
}

/// How a race participant is named in the recovery boundary.
///
/// `provider/model`, which is what the client asked for and what the operator wrote in the combo.
fn participant_id(step: &StepPlan) -> String {
    format!("{}/{}", step.provider_id, step.model)
}

/// File a served combo answer and keep its record open for as long as the client's body.
///
/// `activity` is `None` for a direct model, so this is a no-op there: a name that did not resolve
/// to a combo is not a combo decision, and pretending otherwise would fill the panel with requests
/// the operator never created a route for.
///
/// The guard is attached to the body rather than dropped here because "in flight" means "the client
/// is still receiving it". Dropping it at the return would mark the decision finished before the
/// first byte went out.
fn served_combo(
    app: &AppState,
    activity: Option<(&str, &str)>,
    step: String,
    started_at: u64,
    response: Response,
) -> Response {
    let Some((combo, requested)) = activity else {
        return response;
    };
    let guard = app.activity.begin(combo, requested, &step, started_at);
    crate::activity::watch(response, guard)
}

/// File a combo decision that ended without an answer.
///
/// `attempted` is `None` when no step was ever dispatched — every step was skipped for a modality
/// the route cannot carry. There is no decision to report then: the request was rerouted rather
/// than committed to a step, and an entry naming an untried step would be a fabrication.
fn failed_combo(
    app: &AppState,
    activity: Option<(&str, &str)>,
    attempted: Option<&str>,
    started_at: u64,
) {
    let (Some((combo, requested)), Some(step)) = (activity, attempted) else {
        return;
    };
    app.activity.failed(combo, requested, step, started_at);
}

/// The participants that passed the race, in priority order, ready to answer in the winner's place.
///
/// "Passed" is the eligibility gate, not "answered eventually": a participant that replied after
/// the window keeps no priority protection, and the operator's rule is about the ones that fit the
/// latency requirement. The winner itself is not in the list — it is the one that died.
///
/// When `exclude_provider` is `Some` (for `pick-highest-passed-different-provider`), replacements
/// backed by that provider are dropped: a whole-provider outage must not be answered by a sibling
/// model on the same provider.
fn passed_replacements(
    outcomes: &mut [Option<Attempt>],
    states: &[select::Participant],
    winner: usize,
    window_ms: u64,
    participants: &[&StepPlan],
    exclude_provider: Option<ProviderId>,
) -> Vec<crate::recovery::Replacement> {
    let mut passed = Vec::new();
    for (index, outcome) in outcomes.iter_mut().enumerate() {
        if index == winner {
            continue;
        }
        if !states[index].replied_in_window(window_ms) {
            continue;
        }
        // `pick-highest-passed-different-provider`: skip a sibling on the same provider as the
        // one that died, since it would fail in exactly the same way.
        if let Some(ref excluded) = exclude_provider {
            if participants[index].provider_id == *excluded {
                continue;
            }
        }
        // The answer is already in the race's result set; only its body has not been read.
        if let Some(Attempt::Answer(response)) = outcome.take() {
            passed.push(crate::recovery::Replacement {
                id: participant_id(participants[index]),
                body: response.into_body(),
            });
        }
    }
    passed
}

/// An owned snapshot of everything `attempt_step` needs, so that a fallback task spawned for
/// `rerun-excluding` or `stable-fallback` can run without borrowing the live request's context.
///
/// All fields are `'static` (the relay state is `Arc`-backed and the per-request scalars are
/// copied), so the whole struct can be moved into a `tokio::spawn`'d task.
#[derive(Clone)]
struct OwnedAttempt {
    app: AppState,
    table: Arc<RouteTable>,
    now_ms: u64,
    combo: Option<mr_core::snapshot::ComboPlan>,
    client_dialect: Dialect,
    method: Method,
    headers: HeaderMap,
    head: RequestHead,
    bytes: Bytes,
    model_name: String,
    route: RouteKind,
    uri: Uri,
    level_override: Option<ReasoningLevel>,
    quota: mr_core::config::QuotaConfig,
    ticket: Option<std::sync::Arc<crate::quota::QuotaTicket>>,
    started: std::time::Instant,
    auth_done: std::time::Duration,
    scan_done: std::time::Duration,
    lookup_done: std::time::Duration,
    sticky_index: Option<usize>,
    pre_admitted: Option<String>,
}

/// Build an `OwnedAttempt` from the borrowed `AttemptContext`, copying out the per-request values
/// that a spawned fallback task needs to own.
fn owned_from_actx(actx: &AttemptContext<'_>) -> OwnedAttempt {
    OwnedAttempt {
        app: actx.app.clone(),
        table: actx.table.clone(),
        now_ms: actx.now_ms,
        combo: actx.combo.cloned(),
        client_dialect: actx.client_dialect,
        method: actx.method.clone(),
        headers: actx.headers.clone(),
        head: actx.head,
        bytes: actx.bytes.clone(),
        model_name: actx.model_name.to_owned(),
        route: actx.route,
        uri: actx.uri.clone(),
        level_override: actx.level_override,
        quota: actx.quota.clone(),
        ticket: actx.ticket.clone(),
        started: actx.started,
        auth_done: actx.auth_done,
        scan_done: actx.scan_done,
        lookup_done: actx.lookup_done,
        sticky_index: actx.sticky_index,
        pre_admitted: actx.pre_admitted.clone(),
    }
}

/// Run one attempt from owned data, rebuilding the borrowed `AttemptContext` for the duration of
/// the call. Used by the fallback tasks for `rerun-excluding` and `stable-fallback`.
async fn attempt_from_owned(owned: &OwnedAttempt, step: &StepPlan, is_sticky: bool) -> Attempt {
    let ctx = AttemptContext {
        app: &owned.app,
        table: owned.table.clone(),
        now_ms: owned.now_ms,
        combo: owned.combo.as_ref(),
        client_dialect: owned.client_dialect,
        method: &owned.method,
        headers: &owned.headers,
        head: owned.head,
        bytes: &owned.bytes,
        model_name: &owned.model_name,
        route: owned.route,
        uri: &owned.uri,
        level_override: owned.level_override,
        quota: &owned.quota,
        ticket: owned.ticket.clone(),
        started: owned.started,
        auth_done: owned.auth_done,
        scan_done: owned.scan_done,
        lookup_done: owned.lookup_done,
        sticky_index: owned.sticky_index,
        pre_admitted: owned.pre_admitted.clone(),
    };
    attempt_step(&ctx, step, is_sticky, false).await
}

/// Run a hedged race over a combo's participants.
///
/// Every participant is dispatched at once. The winner is the **highest-priority** participant that
/// produced a usable answer inside the eligibility window — not the fastest one, because a race
/// that rewarded raw speed would make the operator's ordering decorative. `select::hedge_decide`
/// owns that rule; this function owns the machinery around it.
///
/// The window is an **eligibility gate, not a timeout**: it never fails a request, and a
/// participant that answers after it is still an answer.
///
/// The commit below happens when the upstream response arrives, which is before the body streams.
/// The winner's body is therefore wrapped by [`crate::recovery`], so that a winner which dies
/// while its body streams can be replaced by a participant that passed the race — the
/// highest-priority one, which is the operator's rule.
async fn run_hedge(
    actx: &AttemptContext<'_>,
    participants: &[&StepPlan],
    hedge: &mr_core::snapshot::HedgePlan,
) -> HedgeOutcome {
    let started = std::time::Instant::now();
    let window_ms = u64::from(hedge.window_ms);

    // Every participant is dispatched at once. The window is not a delay before the second
    // request goes out: it is the deadline by which a reply keeps its priority protection, and a
    // hedged request that waited for the window before sending the second request would be
    // strictly worse than an unhedged one.
    let mut pending: Vec<PendingAttempt<'_>> = participants
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let future = attempt_step(
                actx,
                step,
                sticky_step_is(actx.combo, actx.sticky_index, step),
                false,
            );
            Box::pin(async move { (index, future.await) }) as PendingAttempt<'_>
        })
        .collect();

    let mut states: Vec<select::Participant> =
        vec![select::Participant::PENDING; participants.len()];
    let mut outcomes: Vec<Option<Attempt>> = (0..participants.len()).map(|_| None).collect();
    let mut fallback_target: Option<String> = None;
    let mut sticky_cleared = false;

    while !pending.is_empty() {
        // The first participant to settle is the one the decision is taken on. `select_all` polls
        // every pending attempt, so a participant that is already ready wins the poll immediately
        // and the others keep their place in the queue.
        let (settled, _index, rest) = futures_util::future::select_all(pending.drain(..)).await;
        pending = rest;
        let (participant, attempt) = settled;
        let now_ms = started.elapsed().as_millis() as u64;

        // A participant that answered is "arrived"; one that failed is settled and can never win.
        states[participant] = match &attempt {
            Attempt::Answer(_) => select::Participant::arrived(now_ms),
            _ => select::Participant::FAILED,
        };
        match &attempt {
            Attempt::Failed {
                sticky_cleared: cleared,
                ..
            } => {
                sticky_cleared = sticky_cleared || *cleared;
            }
            Attempt::Skip {
                fallback_target: target,
                sticky_cleared: cleared,
                cause: _,
            } => {
                if target.is_some() {
                    fallback_target = target.clone();
                }
                sticky_cleared = sticky_cleared || *cleared;
            }
            Attempt::Fatal { .. } => {}
            Attempt::Answer(_) => {}
        }

        // A fatal failure — a broken proxy — ends the request rather than the race. The other
        // participants are dropped with it.
        if let Attempt::Fatal { response } = attempt {
            return HedgeOutcome::Fatal {
                response,
                attempted: participant_id(participants[participant]),
            };
        }

        outcomes[participant] = Some(attempt);

        // The decision belongs to one place: `hedge_decide` knows the priority order, the
        // eligibility window and the rule that a lower-priority participant cannot win while a
        // higher-priority one might still reply in time.
        if let select::HedgeDecision::Resolved(winner) =
            select::hedge_decide(&states, window_ms, now_ms)
        {
            let response = match outcomes[winner].take() {
                Some(Attempt::Answer(response)) => response,
                _ => {
                    // The decision named a participant that did not answer, which the rule does
                    // not allow. Rather than serve the wrong answer, keep waiting.
                    outcomes[winner] = None;
                    continue;
                }
            };

            // The participants that passed the race are kept: their answers are the fallback for a
            // winner that dies while its body streams. The participants that are still pending are
            // dropped with the race, exactly as before — their upstream connections are closed,
            // which is the documented cost of a hedge and does not change.
            let exclude_provider = match &hedge.on_winner_fail {
                mr_core::dialect::WinnerFailMode::PickHighestPassedDifferentProvider => {
                    Some(participants[winner].provider_id.clone())
                }
                _ => None,
            };
            let replacements = passed_replacements(
                &mut outcomes,
                &states,
                winner,
                window_ms,
                participants,
                exclude_provider,
            );

            // What happens when the committed winner dies after this point. Only
            // `pick-highest-passed` costs zero extra upstream work — its replacements are already
            // in hand. The other two implemented modes re-dispatch new upstream requests, which is
            // deliberately more expensive and only runs when the winner actually fails.
            let mode = hedge.on_winner_fail.clone();
            let rerun_max_rounds = match mode {
                mr_core::dialect::WinnerFailMode::RerunExcluding { max_rounds } => max_rounds,
                // `stable-fallback` nominates exactly one step, so a single round is all it can
                // ever need.
                mr_core::dialect::WinnerFailMode::StableFallback { .. } => 1,
                _ => 0,
            };
            // The step `stable-fallback` names, if the operator chose that mode. Ignoring it would
            // silently turn the mode into a one-round `rerun-excluding`, i.e. a different promise
            // than the documented "the same deterministic fallback every time".
            let stable_step = match mode {
                mr_core::dialect::WinnerFailMode::StableFallback { step } => Some(step),
                _ => None,
            };
            let rerun_factory = if rerun_max_rounds > 0
                && !matches!(
                    mode,
                    mr_core::dialect::WinnerFailMode::PickHighestPassedDifferentProvider
                ) {
                // `rerun-excluding` and `stable-fallback` need new upstream dispatches. Build the
                // factory now so the recovery body can spawn it when the winner fails. A different
                // provider is also excluded there: a whole-provider outage is exactly what this
                // mode exists to survive.
                let rerun_provider = match mode {
                    mr_core::dialect::WinnerFailMode::PickHighestPassedDifferentProvider => {
                        Some(participants[winner].provider_id.clone())
                    }
                    _ => None,
                };
                let owned = owned_from_actx(actx);
                let remaining: Vec<StepPlan> = participants
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != winner)
                    .filter(|(i, _step)| {
                        // Exclude models that definitively failed in this round; pending ones were
                        // dropped when the winner committed and are re-dispatched here.
                        !matches!(outcomes[*i], Some(Attempt::Failed { .. }))
                    })
                    .filter(|(_, step)| rerun_provider.as_ref() != Some(&step.provider_id))
                    // `stable-fallback` names one step, and only that step is the fallback. Without
                    // this it silently degrades into a one-round `rerun-excluding`, which promises
                    // "something that passed", not "the same deterministic fallback every time".
                    .filter(|(i, _)| match stable_step {
                        Some(nominated) => actx
                            .combo
                            .map(|combo| {
                                combo.hedge.participants.get(*i).copied() == Some(nominated)
                            })
                            .unwrap_or(false),
                        None => true,
                    })
                    .map(|(_, s)| (*s).clone())
                    .collect();
                let factory: RerunFactory = Box::new(move || {
                    let owned = owned.clone();
                    let remaining = remaining.clone();
                    Box::pin(async move {
                        let mut found: Vec<crate::recovery::Replacement> = Vec::new();
                        for step in remaining {
                            let is_sticky =
                                sticky_step_is(owned.combo.as_ref(), owned.sticky_index, &step);
                            let attempt = attempt_from_owned(&owned, &step, is_sticky).await;
                            if let Attempt::Answer(response) = attempt {
                                found.push(crate::recovery::Replacement {
                                    id: participant_id(&step),
                                    body: response.into_body(),
                                });
                                break; // highest-priority one wins
                            }
                        }
                        Ok::<Vec<crate::recovery::Replacement>, axum::Error>(found)
                    })
                        as Pin<
                            Box<
                                dyn Future<
                                        Output = Result<
                                            Vec<crate::recovery::Replacement>,
                                            axum::Error,
                                        >,
                                    > + Send,
                            >,
                        >
                });
                Some(factory)
            } else {
                None
            };

            let policy = actx
                .combo
                .map(|combo| combo.midstream_policy)
                .unwrap_or_default();
            let winner_id = participant_id(participants[winner]);
            // The winner's body is wrapped so that the relay owns the stream: a failure after the
            // response arrived can then be recovered instead of ending the client's answer.
            let wrapped = crate::recovery::wrap(
                response,
                replacements,
                crate::recovery::RecoveryConfig {
                    policy,
                    mode,
                    client_dialect: actx.client_dialect,
                    winner_id: winner_id.clone(),
                    announceable: false,
                    rerun_factory,
                    rerun_max_rounds,
                },
            );
            return HedgeOutcome::Won {
                response: wrapped,
                winner: winner_id,
            };
        }
    }

    // Every participant failed. The failure to report is the one the ordinary chain would have
    // reported: the last participant that failed *with a response*, unless a later participant
    // failed in a way that produced no response at all, which clears it — the same rule the
    // sequential chain uses.
    let mut last_failure: Option<Response> = None;
    for outcome in outcomes.into_iter().flatten() {
        if let Attempt::Failed { response, .. } = outcome {
            last_failure = response;
        }
    }

    HedgeOutcome::NoWinner {
        last_failure,
        fallback_target,
        sticky_cleared,
    }
}

/// Whether this combo keeps a sticky-success preference at all.
///
/// Stickiness is about cache locality, not about racing. A combo that starts at the same step on
/// every request keeps the provider's prompt cache warm, and that is worth having whether or not
/// the combo hedges, so hedging is not a precondition. Round-robin is excluded because spreading
/// load across steps is its whole purpose, and a window of zero is the operator switching it off.
fn sticky_applies(combo: &mr_core::snapshot::ComboPlan) -> bool {
    matches!(
        combo.strategy,
        mr_core::ComboStrategy::Priority | mr_core::ComboStrategy::ResetAwarePriority
    ) && combo.sticky_success_window_secs > 0
}

/// The step to try when every step of a combo is cooling down.
///
/// The one whose accounts recover soonest, because that is the closest thing to a live step the
/// combo has. Steps that were already raced are excluded: they have been tried in this request.
fn last_resort_step<'a>(
    app: &AppState,
    table: &RouteTable,
    steps: &[&'a StepPlan],
    raced: &[&StepPlan],
    now_ms: u64,
) -> Option<&'a StepPlan> {
    let health = app.health.view();
    steps
        .iter()
        .copied()
        .filter(|step| !raced.iter().any(|tried| std::ptr::eq(*tried, *step)))
        .filter_map(|step| {
            let provider = table
                .snapshot
                .providers()
                .get(step.provider_index as usize)?;
            let soonest = provider
                .accounts
                .iter()
                .filter(|account| select::account_allows_model(account, &step.model))
                .map(|account| {
                    select::route_reset_instant(
                        provider.id.as_str(),
                        account.id.as_str(),
                        &step.model,
                        &health,
                    )
                    .unwrap_or(now_ms)
                })
                .min()?;
            Some((soonest, step))
        })
        .min_by_key(|(soonest, _)| *soonest)
        .map(|(_, step)| step)
}

/// Whether a step is the one the combo's sticky preference points at.
///
/// The same comparison the sequential chain uses, in one place, so the race and the chain agree
/// about which step the preference is about.
fn sticky_step_is(
    combo: Option<&mr_core::snapshot::ComboPlan>,
    sticky_index: Option<usize>,
    step: &StepPlan,
) -> bool {
    sticky_index.is_some_and(|index| {
        combo.is_some_and(|combo| {
            combo
                .steps
                .get(index)
                .is_some_and(|candidate| std::ptr::eq(candidate, step))
        })
    })
}

/// The client's request body with its `model` field replaced.
///
/// Used by the modality fallback, which retries the request against a different model. The
/// replacement is the *client-facing* model id — the same one the client would have sent — because
/// the retry goes back through the whole request path and must be resolved by the snapshot, not
/// carried over from the previous route.
///
/// Returns `None` when the body is not a JSON object, or has no `model` field to replace. Guessing
/// would be worse than refusing: a request retried against the wrong model is a misroute the
/// operator would never see.
fn request_with_model(body: &Bytes, model: &str) -> Option<Bytes> {
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object_mut()?;
    if !object.contains_key("model") {
        return None;
    }
    object.insert("model".into(), serde_json::Value::String(model.to_owned()));
    serde_json::to_vec(&value).ok().map(Bytes::from)
}

/// Read the whole body, refusing anything over the limit.
///
/// `max` is checked before the body is buffered in full, so an oversized upload is refused while it
/// arrives rather than after it has already been held in memory.
pub(crate) async fn read_body(mut body: AxumBody, max: usize) -> Result<Bytes, BodyError> {
    use http_body_util::BodyExt as _;
    // The limit is enforced while the body arrives, not after it has been buffered in full: a
    // client must not be able to make the relay hold an arbitrarily large upload before it is told
    // the request is too big.
    let mut buf: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = match frame {
            Ok(frame) => frame,
            Err(e) => return Err(BodyError::Read(e.to_string())),
        };
        if let Some(data) = frame.data_ref() {
            if buf.len() + data.len() > max {
                return Err(BodyError::TooLarge);
            }
            buf.extend_from_slice(data);
        }
    }
    Ok(Bytes::from(buf))
}

/// The model name from a scanned head.
fn requested_model<'a>(head: &RequestHead, body: &'a [u8]) -> Option<&'a str> {
    head.model_str(body)
}

/// A client key from either header form.
///
/// Both are accepted on every route. Anthropic-shaped clients send `x-api-key`, OpenAI-shaped ones
/// send `Authorization`, and refusing the "wrong" one for a given path would break working clients
/// for no benefit.
pub(crate) fn presented_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_owned());
        }
    }
    let raw = headers.get("authorization")?.to_str().ok()?.trim();

    // Strip the scheme case-insensitively, and accept it whether or not a space follows. Written as
    // a manual boundary test rather than two `strip_prefix` calls because `HeaderValue` normalises
    // trailing whitespace: `Authorization: Bearer ` arrives as `Bearer`, so a version that only
    // looked for `"Bearer "` would return the literal word "Bearer" as somebody's API key and report
    // it as invalid rather than as missing.
    let raw = match raw.len() >= 6 && raw[..6].eq_ignore_ascii_case("bearer") {
        true => raw[6..].trim(),
        false => raw,
    };

    if raw.is_empty() {
        None
    } else {
        Some(raw.to_owned())
    }
}

/// Authenticate through the relay, allowing only genuinely absent credentials through open mode.
/// Any presented credential — including an invalid one — is always checked and rejected normally.
pub(crate) fn authenticate_request(
    relay: &RelayState,
    no_key_required: bool,
    presented: Option<&str>,
) -> AuthOutcome {
    if presented.is_none() && no_key_required {
        return AuthOutcome::Open;
    }
    match relay.authenticate(presented) {
        // In the bootstrap/no-key configuration the relay's legacy state treats every credential as
        // open. A presented key cannot be validated when no keys exist, so reject it rather than
        // silently treating it as an accepted credential.
        AuthOutcome::Open if presented.is_some() => AuthOutcome::Rejected,
        outcome => outcome,
    }
}

/// Map an authentication outcome onto a response.
pub(crate) fn auth_error(outcome: &AuthOutcome, dialect: Dialect) -> Response {
    let message = match outcome {
        AuthOutcome::Missing => {
            "no client key was presented. Send one as `Authorization: Bearer <key>` or `x-api-key: <key>`."
        }
        _ => "the client key is not valid",
    };
    let mut response = error_response(
        StatusCode::UNAUTHORIZED,
        dialect,
        "invalid_api_key",
        message,
    );
    if let Ok(v) = HeaderValue::from_str("Bearer") {
        response.headers_mut().insert("www-authenticate", v);
    }
    response
}

/// What a 429 body says about *when* the account recovers.
///
/// A bare 429 is a throttle: the provider wants a moment. A 429 that carries a spent window is a
/// different thing — the budget is gone until the instant the provider names. Codex says so with
/// `{"error":{"type":"usage_limit_reached","resets_in_seconds":1233929}}`, and OpenAI's own
/// `rate_limit_reached` details carry the same shape. Treating those as an ordinary throttle parks
/// the account for the configured minute, it comes back, is refused again, and the request fails
/// while a sibling account with quota sits unused.
///
/// Returns the reason plus the millisecond wait the body states. The wait is clamped to a day, the
/// same cap `retry_after_ms` applies, so a bogus value cannot park an account indefinitely.
/// A refusal that is about *this request* rather than the account's standing.
///
/// OpenCode's gateway answers an oversized prefill with a 429 whose body carries a
/// self-describing marker and its own recovery hint:
///
/// `[prefill_cap_refused] the cell is at capacity; retry shortly (after 30 s)`
///
/// That is not a rate limit on the credential. The credential is fine; one request was too big
/// for the cell to admit. Giving it the flat rate-limit cooldown parks a perfectly healthy
/// account for longer than the upstream asked, and letting it grow with the failure count would
/// park it for longer still, on every request that happens to be large. The hint is parsed
/// instead, so the account comes back exactly when the upstream said it would.
fn capacity_refusal(body: &[u8]) -> Option<u64> {
    /// The marker OpenCode uses, and the wait to assume if its hint cannot be read.
    const MARKER: &str = "prefill_cap_refused";
    const FALLBACK_MS: u64 = 30_000;
    let text = std::str::from_utf8(body).ok()?;
    let at = text.find(MARKER)?;
    // The hint reads `after 30 s`. A body that names the marker but not the delay still gets
    // the fallback: retrying a refused prefill at full rate is worse than waiting.
    // The hint reads `(after 30 s)` — the word is parenthesised and the count can be too, so
    // both are trimmed of punctuation before they are compared or parsed. Splitting on
    // whitespace alone would miss `(after` and silently fall back on every real body.
    let mut words = text[at + MARKER.len()..]
        .split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()));
    let hinted = words
        .by_ref()
        .find(|word| word.eq_ignore_ascii_case("after"))
        .and_then(|_| words.next())
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .map(|seconds| seconds.min(86_400).saturating_mul(1000));
    Some(hinted.unwrap_or(FALLBACK_MS))
}

/// Whether a refusal body says the *plan* does not include the requested model.
///
/// Only transports that can tell the two apart write this member, because the upstream status alone
/// cannot: Cursor answers both a rate limit and a model its Free tier lacks with
/// `resource_exhausted`, and a relay reading the code would park a healthy account for a model it was
/// never going to be allowed to use. The marker is owned by the relay — its transport puts it there,
/// having read the upstream's structured switch-model button — so this is not prose matching.
fn names_an_unavailable_model(body: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return false;
    };
    value
        .get("error")
        .and_then(|error| error.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("model_unavailable")
}

/// What a refusal body says about the account, beyond the status code.
///
/// `scope` is the narrow reading the body itself asks for: a refusal that names the plan's limit for
/// one model leaves the account's other models usable, so the cooldown belongs on that model rather
/// than on the credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BodyRefusal {
    reason: select::CooldownReason,
    wait_ms: Option<u64>,
    scope: Option<mr_core::model::CooldownScope>,
}

/// How long a refusal that says the account cannot pay is parked when the body states no reset.
///
/// The configured default is a minute, which is the right guess for a throttle: the window turns
/// over on its own. A balance does not. "Exhausted balance, top up" is a person's job, and probing it
/// sixty times an hour spends a request on every call to learn the same fact. An hour is long enough
/// that a locked account stops poisoning requests and short enough that a top-up is noticed the same
/// afternoon.
const SPENT_BALANCE_PARK_MS: u64 = 3_600_000;

/// The sentence a refusal carries, whichever envelope it arrived in.
///
/// OpenAI and Anthropic nest it under `error.message`; FastAPI-shaped upstreams put it at `detail`;
/// a few send `error` as the string itself. Reading only the first shape is not a cosmetic gap: a
/// locked, unpaid account answered `{"detail":"User is locked. Reason: Exhausted balance. …"}` and
/// because that sentence was invisible the 403 was classified as a failed credential — the wrong
/// reason, a fifteen-minute park, and a retry that could never succeed.
fn refusal_text(value: &serde_json::Value) -> &str {
    let error = value.get("error").unwrap_or(value);
    [
        error.get("message"),
        error.get("detail"),
        value.get("detail"),
        value.get("message"),
        Some(error),
    ]
    .into_iter()
    .flatten()
    .filter_map(serde_json::Value::as_str)
    .next()
    .unwrap_or_default()
}

fn exhausted_by_body(body: &[u8]) -> Option<BodyRefusal> {
    if let Some(wait) = capacity_refusal(body) {
        return Some(BodyRefusal {
            reason: select::CooldownReason::RateLimited,
            wait_ms: Some(wait),
            scope: None,
        });
    }
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let error = value.get("error").unwrap_or(&value);
    let kind = error
        .get("type")
        .or_else(|| error.get("code"))
        .and_then(|kind| kind.as_str())
        .unwrap_or_default();
    let exhausted = matches!(
        kind,
        "usage_limit_reached" | "rate_limit_reached" | "insufficient_quota"
    );
    let seconds = error
        .get("resets_in_seconds")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            // Some upstreams name an absolute instant in seconds instead of a delay.
            error
                .get("resets_at")
                .and_then(serde_json::Value::as_u64)
                .map(|at| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    at.saturating_sub(now)
                })
        });
    // A structured reset is the strongest statement, but it is not the only one. A body that says
    // when the account recovers, in words, is exactly as actionable, and stopping at the field
    // shape parks a spent account for the configured minute so it can be refused again on the next
    // request. The prose read is only a *signal* when it carries an anchor word ("resets in", "try
    // again in"), so an error message that happens to contain a number is not read as a wait.
    let message = refusal_text(&value);
    let spoken = seconds.is_none().then(|| stated_wait_ms(message)).flatten();
    let announced = seconds
        .map(|s| s.min(86_400).saturating_mul(1000))
        .or(spoken);
    // A plan that does not include the model is a refusal about *that model*, not about the
    // credential: the same account can still serve everything else its plan covers. It is read
    // before the money phrases because "Upgrade for unlimited Pro model use" is a plan statement
    // that also names money, and the wider reading would park the whole account for a limit the
    // plan imposes on one model.
    if names_a_plan_limit(message) {
        return Some(BodyRefusal {
            reason: select::CooldownReason::QuotaExhausted,
            wait_ms: announced.or(Some(SPENT_BALANCE_PARK_MS)),
            scope: Some(mr_core::model::CooldownScope::Model),
        });
    }
    // A refusal that names money rather than shape is about the account even when it arrives as a
    // 400. Providers do this: a credit balance can run out and come back as a plain `BAD_REQUEST`
    // with a generic code, which reads as "the request is malformed" and is not cooled down, so a
    // spent account is re-probed on every single request. The wording is the only signal there is.
    let spent_account = mentions_a_spent_account(message);
    if !exhausted && spoken.is_none() && !spent_account {
        return None;
    }
    let wait_ms =
        announced.or_else(|| names_a_spent_balance(message).then_some(SPENT_BALANCE_PARK_MS));
    Some(BodyRefusal {
        reason: select::CooldownReason::QuotaExhausted,
        wait_ms,
        scope: None,
    })
}

/// Whether a message says the account itself cannot pay, rather than the request being wrong.
///
/// The phrases are deliberately about *money and balance* and not about throttling: "rate limit",
/// "too many requests" and friends are already answered by the status code, and matching them here
/// would turn an ordinary 429 into a day-long quota park. Nothing in this list names a provider, a
/// model or an error code — it is the small set of English phrases that mean the credit is gone or
/// the plan's quota is spent, which is a statement no retry can change until someone tops up or the
/// window turns over.
fn mentions_a_spent_account(message: &str) -> bool {
    // A quota that comes back on its own is still about the account, but it is not about a balance,
    // so it is parked by the configured default rather than by the hour.
    const WINDOWS: [&str; 6] = [
        "quota exceeded",
        "quota exhausted",
        "exceeded your quota",
        "exceeded your current quota",
        "spend limit",
        "credit limit",
    ];
    let lower = message.to_ascii_lowercase();
    names_a_spent_balance(message) || WINDOWS.iter().any(|phrase| lower.contains(phrase))
}

/// Whether a message says the account's *balance* is gone, rather than a window being spent.
///
/// The difference decides the park: a spent window turns over on its own, while a balance only comes
/// back when a person pays. The list is English and provider-neutral on purpose — it is the small
/// set of sentences that mean "no money", and it names no vendor, no model and no error code.
fn names_a_spent_balance(message: &str) -> bool {
    const PHRASES: [&str; 22] = [
        "insufficient credit",
        "insufficient balance",
        "insufficient funds",
        "enough credit",
        "out of credit",
        "no credit",
        "credit balance",
        "billing issue",
        "billing problem",
        "payment required",
        "exhausted balance",
        "balance exhausted",
        "balance is exhausted",
        "no balance",
        "zero balance",
        "out of balance",
        "top up",
        "add funds",
        "add credit",
        "recharge",
        "user is locked",
        "account is locked",
    ];
    let lower = message.to_ascii_lowercase();
    PHRASES.iter().any(|phrase| lower.contains(phrase))
}

/// Whether a message says the plan does not cover the model that was asked for.
///
/// This is narrower than a spent account and must be read that way: the same credential keeps
/// serving the models its plan does include, so the refusal is parked against the one model. Both
/// halves are required — a plan word and a limit word — because "your plan has been upgraded" and
/// "this model is not included" are different statements, and only the pair means what it says here.
fn names_a_plan_limit(message: &str) -> bool {
    const PLAN: [&str; 5] = [
        "free plan",
        "free tier",
        "your plan",
        "your current plan",
        "your subscription",
    ];
    const LIMIT: [&str; 7] = [
        "upgrade to",
        "upgrade for",
        "upgrade your",
        "does not include",
        "not included in",
        "is not included",
        "isn't included",
    ];
    let lower = message.to_ascii_lowercase();
    PLAN.iter().any(|phrase| lower.contains(phrase))
        && LIMIT.iter().any(|phrase| lower.contains(phrase))
}

/// A recovery instant the provider states in words rather than in a field.
///
/// Bodies in the wild say `Resets every 24 hours`, `It resets in 13h 24m` and `try again in 30
/// seconds`. Those are as actionable as `resets_in_seconds`: the account is spent until the named
/// moment, and a flat one-minute cooldown spends that minute being refused again on every request.
///
/// The anchor word is what makes this safe. A duration is read only when it follows a word that
/// promises recovery, because a bare number in an error message is usually a limit or an offset
/// (`messages must not exceed 5 per request`) and attaching a day's cooldown to a validation error
/// would take a healthy account out of rotation.
fn stated_wait_ms(text: &str) -> Option<u64> {
    const ANCHORS: [&str; 7] = [
        "resets in",
        "reset in",
        "resets every",
        "reset every",
        "retry in",
        "try again in",
        "available in",
    ];
    let lower = text.to_ascii_lowercase();
    ANCHORS
        .iter()
        .find_map(|anchor| {
            lower
                .find(anchor)
                .and_then(|at| duration_after(&lower[at + anchor.len()..]))
        })
        .or_else(|| reset_instant_ms(&lower, time::OffsetDateTime::now_utc()))
}

/// A reset the provider states as a calendar date rather than as a distance in time.
///
/// `Your free allowance is used up. It comes back on 12 Nov 2026 at 15:05 UTC` is the same
/// knowledge as `resets in 4 hours`, just harder to read, and providers that meter a week or a
/// month tend to write it this way. Reading it is worth the code: the alternative is a one-minute
/// guess that walks the account back into the same refusal, sixty times an hour, for a week.
///
/// The shape accepted is narrow on purpose — a promising anchor, then a day, a month name, a
/// four-digit year, and optionally a clock. Anything else is not a reset: a bare date in a
/// validation message must not park an account, so the anchor is required, the year is required
/// (guessing one turns a typo into a year-long cooldown), and a date in the past, or further away
/// than ninety days, is left to the default guess.
fn reset_instant_ms(text: &str, now: time::OffsetDateTime) -> Option<u64> {
    const DATE_ANCHORS: [&str; 10] = [
        "comes back on",
        "come back on",
        "starts on",
        "starts at",
        "resets on",
        "reset on",
        "resumes on",
        "available on",
        "available from",
        "back on",
    ];
    let (at, _) = DATE_ANCHORS
        .iter()
        .filter_map(|anchor| text.find(anchor).map(|at| (at, anchor.len())))
        .max_by_key(|(at, _)| *at)?;
    let rest = &text[at..];

    let mut tokens: Vec<(usize, usize, &str)> = Vec::new();
    let mut word_start: Option<usize> = None;
    for (i, c) in rest.char_indices() {
        if c.is_ascii_alphanumeric() {
            word_start.get_or_insert(i);
        } else if let Some(start) = word_start.take() {
            tokens.push((start, i, &rest[start..i]));
        }
    }
    if let Some(start) = word_start {
        tokens.push((start, rest.len(), &rest[start..]));
    }

    for (i, &(_, _, word)) in tokens.iter().enumerate() {
        let Some(month) = month_of(word) else {
            continue;
        };
        let Some(day) = i
            .checked_sub(1)
            .and_then(|d| tokens[d].2.parse::<u8>().ok())
        else {
            continue;
        };
        let Some(year) = tokens.get(i + 1).and_then(|t| t.2.parse::<i32>().ok()) else {
            continue;
        };
        if !(2020..=2100).contains(&year) {
            continue;
        }
        let (hour, minute) = tokens
            .get(i + 1)
            .and_then(|(_, end, _)| clock_after(&rest[*end..]))
            .unwrap_or((0, 0));
        let Ok(date) = time::Date::from_calendar_date(year, month, day) else {
            continue;
        };
        let Ok(target) = date.with_hms(hour, minute, 0) else {
            continue;
        };
        let millis = (target.assume_utc() - now).whole_milliseconds();
        if millis <= 0 || millis > 90 * 86_400_000 {
            return None;
        }
        return u64::try_from(millis).ok();
    }
    None
}

/// The month a lowercase word names, full or abbreviated.
fn month_of(word: &str) -> Option<time::Month> {
    use time::Month;
    Some(match word {
        "jan" | "january" => Month::January,
        "feb" | "february" => Month::February,
        "mar" | "march" => Month::March,
        "apr" | "april" => Month::April,
        "may" => Month::May,
        "jun" | "june" => Month::June,
        "jul" | "july" => Month::July,
        "aug" | "august" => Month::August,
        "sep" | "sept" | "september" => Month::September,
        "oct" | "october" => Month::October,
        "nov" | "november" => Month::November,
        "dec" | "december" => Month::December,
        _ => return None,
    })
}

/// A `HH:MM` clock within the next few words, as in `at 15:05 UTC`.
fn clock_after(rest: &str) -> Option<(u8, u8)> {
    let rest = rest.trim_start();
    let rest = rest.strip_prefix("at ").unwrap_or(rest).trim_start();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() && i < 8 {
        if bytes[i].is_ascii_digit() {
            let hour_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
            if hour_digits <= 2 && bytes.get(i + hour_digits) == Some(&b':') {
                let minute_at = i + hour_digits + 1;
                let minute_digits = bytes[minute_at..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if minute_digits == 2 {
                    let hour: u8 = rest[i..i + hour_digits].parse().ok()?;
                    let minute: u8 = rest[minute_at..minute_at + 2].parse().ok()?;
                    return (hour < 24 && minute < 60).then_some((hour, minute));
                }
            }
            i += hour_digits;
            continue;
        }
        if !bytes[i].is_ascii_whitespace() {
            return None;
        }
        i += 1;
    }
    None
}

/// Read a human duration at the start of `rest`: `13h 24m`, `24 hours`, `45 minutes`, `2 days`.
///
/// Returns `None` unless at least one number was followed by a unit, so `resets in 3 requests` is
/// not a wait. Units are matched by their first letter (`s`, `m`, `h`, `d`), with `mo` excluded so
/// that "months" cannot be read as minutes; the result is clamped to a day by the caller.
fn duration_after(rest: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut found = false;
    let mut rest = rest.trim_start_matches([' ', '(', ':']);
    loop {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            break;
        }
        let Ok(amount) = rest[..digits].parse::<u64>() else {
            break;
        };
        rest = rest[digits..].trim_start_matches(' ');
        let letters = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let unit = &rest[..letters];
        if unit.starts_with("mo") {
            break;
        }
        let per_unit = match unit.as_bytes().first() {
            Some(b's') => 1_000,
            Some(b'm') => 60_000,
            Some(b'h') => 3_600_000,
            Some(b'd') => 86_400_000,
            _ => break,
        };
        rest = &rest[letters..];
        total = total.saturating_add(amount.saturating_mul(per_unit));
        found = true;
        if total >= 86_400_000 {
            break;
        }
        let next = rest.trim_start_matches([' ', ',', 'a', 'n', 'd']);
        if !next.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        rest = next;
    }
    found.then(|| total.min(86_400_000)).filter(|ms| *ms > 0)
}

/// Which cooldown a status code deserves.
fn classify_status(status: StatusCode, modal_cold_start: bool) -> select::CooldownReason {
    match status.as_u16() {
        // The credential is wrong. Retrying cannot fix that, and the long cooldown is what stops the
        // logs filling with the same unchanged failure while nobody acts on it.
        401 | 403 => select::CooldownReason::AuthFailed,
        402 => select::CooldownReason::QuotaExhausted,
        429 => select::CooldownReason::RateLimited,
        // A client error (400, 404, 422…) is the client’s fault. Do not cool down the account
        // and do not retry: another account would reject the same malformed request.
        400 | 404 | 405 | 413 | 415 | 422 => select::CooldownReason::InvalidRequest,
        503 if modal_cold_start => select::CooldownReason::ColdStart,
        _ => select::CooldownReason::Backoff,
    }
}

/// A `retry-after` in milliseconds, accepting only the seconds form.
///
/// The seconds form is easy; the HTTP-date form is where implementations break, because taking the
/// digits out of `Wed, 21 Oct 2026` and reading them as seconds produces a cooldown measured in
/// decades. Returning `None` for the date form is deliberate: the configured default is a better
/// answer than a confidently wrong measurement, and the date form is rare enough that it is an
/// acceptable loss.
pub(crate) fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    let secs: u64 = raw.parse().ok()?;
    // Capped, so a provider sending a bogus large value cannot park an account indefinitely.
    Some(secs.min(86_400).saturating_mul(1000))
}

/// The provider's own path for the client's route, if it has one.
///
/// `route_overrides` is keyed by the route's *configuration* spelling — "chat-completions",
/// "responses", "messages" — not by the raw request path, so the lookup is by
/// [`RouteKind::as_config_key`]. A key that still looks like a raw request path can never match,
/// and it is worth saying so by name rather than leaving the operator to wonder why the override
/// never fires. That warning is a report, not a fallback: it is made even when the canonical key
/// is present, because the stray key is a mistake either way.
fn route_override_for<'a>(
    provider: &'a mr_core::snapshot::ProviderPlan,
    route: RouteKind,
    request_path: &str,
) -> Option<&'a String> {
    if provider.route_overrides.is_empty() {
        return None;
    }
    let canonical = route.as_config_key();
    let found = provider.route_overrides.get(canonical);
    if let Some(legacy) = provider
        .route_overrides
        .keys()
        .find(|key| key.starts_with('/'))
    {
        tracing::warn!(
            provider = %provider.id,
            route = canonical,
            key = %legacy,
            request_path,
            "a route_overrides key is a raw request path; the override is keyed by the route's \
             configuration spelling instead"
        );
    }
    found
}

/// The path to put on the upstream URL, and where it came from.
///
/// The distinction matters for one thing only: whether a version segment may be added to a path
/// that has none. A path the dialect produced may have one filled in; an operator's own path is
/// used exactly as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpstreamPath<'a> {
    /// A path the dialect produced: the dialect's own default, or the translator's path.
    Dialect(&'a str),
    /// An operator's explicit `route_overrides` path.
    Operator(&'a str),
}

/// The path for one attempt, and where it came from.
///
/// The precedence is: the provider's own `route_overrides` entry for the client's route, then a
/// path the translator produced, then the dialect's default path for that route.
///
/// The override comes first because it is the operator's explicit statement of where *this*
/// provider serves *this* route, and that is strictly more specific than a path derived from a
/// dialect's naming convention. A translator path is a convention; an override is a fact about
/// the provider, and the fact wins.
///
/// Copilot is the exception. Its base host comes from its OAuth token, and it has no upstream
/// path of its own for an override to replace, so it keeps the translator's path — or, failing
/// that, the Copilot default.
///
/// The count-tokens route is the other exception. A translator names the endpoint for a
/// *generation* — `/v1/messages`, `/v1/chat/completions` — and a count asked on one of those is a
/// billed generation rather than a count. So for that route the dialect's own path wins over a
/// translator's, and the count can only ever reach a counting endpoint.
fn upstream_path<'a>(
    dialect: Dialect,
    route: RouteKind,
    dialect_path: Option<&'a str>,
    operator_path: Option<&'a String>,
) -> UpstreamPath<'a> {
    if dialect == Dialect::Copilot {
        return UpstreamPath::Dialect(dialect_path.unwrap_or_else(|| default_path(dialect, route)));
    }
    if let Some(path) = operator_path {
        return UpstreamPath::Operator(path);
    }
    if route != RouteKind::CountTokens {
        if let Some(path) = dialect_path {
            return UpstreamPath::Dialect(path);
        }
    }
    UpstreamPath::Dialect(default_path(dialect, route))
}

/// The version segment a dialect's canonical path starts with.
///
/// `None` for the dialects whose upstream paths carry no version segment: Codex's `/responses`,
/// Copilot's `/chat/completions` and Command Code's `/alpha/generate`.
fn dialect_version(dialect: Dialect) -> Option<&'static str> {
    match dialect {
        Dialect::OpenAi => Some("v1"),
        Dialect::Anthropic => Some("v1"),
        // The plan base URL is `…/api/v1/zcode-plan/anthropic`, i.e. it already carries the `v1` of
        // the API prefix but not the one the Messages endpoint needs, so the version is added.
        Dialect::Zcode => Some("v1"),
        Dialect::Google => Some("v1beta"),
        Dialect::GoogleAiStudio => Some("v1beta"),
        Dialect::Antigravity => Some("v1internal"),
        Dialect::Codex => None,
        Dialect::Copilot => None,
        Dialect::CommandCodeGo => None,
        Dialect::ModalVllm => None,
        // Cursor's base URL is an *origin*, not a path prefix, and the RPC path is absolute
        // (`/agent.v1.AgentService/Run`). Nothing is ever appended to it, so there is no version
        // segment to add — and adding one would produce a path the server does not serve.
        Dialect::Cursor => None,
    }
}

/// Whether a path segment looks like a version segment: a "v" followed by a digit.
///
/// "v1", "v2", "v1beta" and "v4" qualify. The test is a prefix test, so
/// "v1internal:generateContent" — where the version and the action share one segment — qualifies
/// too, and that is what stops Antigravity's path from being given a second version.
fn is_version_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('v')) && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// Whether a path segment is a version and nothing else: `^v\d[a-z0-9]*$`.
///
/// "v1", "v4" and "v1beta" qualify. "v1internal:generateContent" does not, because the colon
/// means the segment carries the action as well as the version — and that segment is the one
/// Antigravity's path needs. A pure version segment is a version that can be replaced by
/// another: the base URL's version takes its place, so that the URL carries exactly one.
fn is_pure_version_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('v'))
        && chars.next().is_some_and(|c| c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// The version segment a base URL carries, if it has one.
///
/// The segment does not have to be the last one: DeepInfra's base is
/// "https://api.deepinfra.com/v1/openai", and the version there is "v1", not "openai". The job of
/// this lookup is to answer "does the base URL already carry a version?", because the rule it
/// feeds is that the version appears exactly once in the final URL — so the last version-like
/// segment anywhere in the base's path is the one that counts.
///
/// The host is not a path segment: for "https://api.openai.com/v1" the path is "v1", and for
/// "https://api.deepseek.com" there is no path at all.
fn base_version_segment(base: &str) -> Option<&str> {
    // A query string is not part of the path.
    let base = base.split('?').next().unwrap_or(base);
    // The path begins after the authority, which begins after the scheme.
    let after_scheme = base.split_once("://").map_or(base, |(_, rest)| rest);
    let path = after_scheme
        .find('/')
        .map(|index| &after_scheme[index + 1..])?;
    path.split('/').rfind(|segment| is_version_segment(segment))
}

/// Build the upstream URL.
///
/// The version segment appears exactly once. The base URL may carry one
/// ("https://api.openai.com/v1") and the path may carry one ("/v1/chat/completions"), and joining
/// them gives "…/v1/v1/chat/completions"; or the base may carry none
/// ("https://api.deepseek.com") and the path may carry none ("chat/completions"), which drops the
/// version altogether. So a duplicated version is dropped, and a missing one is added — the second
/// only for a path the dialect produced.
///
/// The base's version is the one that stands. A path the dialect produced carries the dialect's
/// version ("v1" for OpenAI, "v1beta" for Google), and the base may carry a different one — the
/// GLM coding surface's base ends in "v4" — so the path's version is replaced by the base's
/// rather than sitting beside it. An operator's `route_overrides` path is theirs, spelled out:
/// only a version that is exactly the base's is dropped from it, and nothing is ever added.
fn build_url(step: &StepPlan, path: UpstreamPath<'_>) -> Option<String> {
    let base = step.base_url.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let (raw, is_operator) = match path {
        UpstreamPath::Dialect(p) => (p, false),
        UpstreamPath::Operator(p) => (p, true),
    };
    // A declared path may be relative. Command Code's own constant is `alpha/generate`, with no
    // leading slash, because it reads as a path fragment rather than a route; the base has just had
    // its trailing slash trimmed, so concatenating the two produced `…commandcode.aialpha/generate`.
    // The separator is supplied here so that no constant has to remember to include it.
    let mut segments: Vec<&str> = raw
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let base_version = base_version_segment(base);

    // A version the base already carries is not repeated.
    if let Some(version) = base_version {
        // A path the dialect produced gives up its own version, whatever it is called: the
        // base's version is the one the URL carries. An operator's path gives up only a version
        // that is exactly the base's — a version the operator wrote that differs is theirs, and
        // is never replaced.
        let droppable = if is_operator {
            segments.first().is_some_and(|segment| *segment == version)
        } else {
            segments
                .first()
                .is_some_and(|segment| is_pure_version_segment(segment))
        };
        if droppable && segments.len() > 1 {
            segments.remove(0);
        }
    }
    // A version the base does not carry is added, but only to a path the dialect produced.
    if !is_operator && base_version.is_none() {
        if let Some(version) = dialect_version(step.dialect) {
            let already_versioned = segments.first().is_some_and(|s| is_version_segment(s));
            if !already_versioned {
                segments.insert(0, version);
            }
        }
    }
    if segments.is_empty() {
        return Some(base.to_string());
    }
    Some(format!("{base}/{}", segments.join("/")))
}

/// The default upstream path for a dialect and a route.
///
/// The route matters: an OpenAI-shaped provider serves six routes, and an embeddings request sent
/// to its chat-completions path reaches the wrong endpoint. The dialect's convention says where
/// each route lives; a provider that deviates from it says so in `route_overrides`.
fn default_path(dialect: Dialect, route: RouteKind) -> &'static str {
    match dialect {
        Dialect::OpenAi => match route {
            RouteKind::ChatCompletions => "/v1/chat/completions",
            RouteKind::Responses => "/v1/responses",
            RouteKind::Embeddings => "/v1/embeddings",
            RouteKind::AudioTranscription => "/v1/audio/transcriptions",
            RouteKind::AudioSpeech => "/v1/audio/speech",
            RouteKind::ImagesGenerations => "/v1/images/generations",
            RouteKind::Models => "/v1/models",
            RouteKind::AudioTranslation => "/v1/audio/translations",
            RouteKind::ImagesEdits => "/v1/images/edits",
            RouteKind::ImagesVariations => "/v1/images/variations",
            RouteKind::Moderations => "/v1/moderations",
            RouteKind::VideoCreate => "/v1/videos",
            // The family prefix only: a read names its render by id in the path, and that path is
            // the client's own, so the capability-addressed proxy forwards it verbatim rather than
            // building it from this default. Naming a concrete job here would send every poll to
            // the same non-existent video.
            RouteKind::VideoRetrieve | RouteKind::VideoContent => "/v1/videos",
            RouteKind::RealtimeClientSecrets => "/v1/realtime/client_secrets",
            RouteKind::RealtimeCalls => "/v1/realtime/calls",
            RouteKind::Search => "/v1/search",
            // An Anthropic client is translated, so the translator names the path. These are the
            // fallback for a request that was not translated.
            RouteKind::Messages | RouteKind::CountTokens => "/v1/chat/completions",
        },
        Dialect::Anthropic => match route {
            RouteKind::CountTokens => "/v1/messages/count_tokens",
            // Every other route falls back to the messages path: an OpenAI client is translated,
            // so the translator names the path.
            _ => "/v1/messages",
        },
        // ZCode is the same Anthropic Messages surface on a different origin. Its count-tokens
        // path is named for completeness only: the route is answered locally for this dialect (the
        // app never asks the endpoint to count, and a count sent to `/v1/messages` would be billed
        // as a generation), so nothing reaches it in practice.
        Dialect::Zcode => match route {
            RouteKind::CountTokens => "/v1/messages/count_tokens",
            _ => "/v1/messages",
        },
        Dialect::Google => "/v1beta/models:generateContent",
        Dialect::GoogleAiStudio => "/v1beta/models:generateContent",
        Dialect::Antigravity => "/v1internal:generateContent",
        Dialect::Codex => "/responses",
        Dialect::Copilot => "/chat/completions",
        Dialect::CommandCodeGo => "/alpha/generate",
        Dialect::ModalVllm => "/v1/chat/completions",
        // Not reachable in practice: a Cursor attempt is opened by `cursor_transport`, which builds
        // its own request and never asks this function for a path. It is named here rather than
        // defaulted to a chat path because a chat path does not exist on that host, and a wrong
        // path in an error message sends the operator looking in the wrong place.
        Dialect::Cursor => "/agent.v1.AgentService/Run",
    }
}

/// The per-request headers ZCode's app attaches to a model request.
///
/// Four ids, and nothing else: `x-request-id` and `x-zcode-trace-id` are fresh for every attempt,
/// `x-zcode-session-type` is `main` because a relay request is a turn somebody asked for, and
/// `x-session-id` is the client's own conversation marker when it sent one — with the `sess_` and
/// `subagent_agent_` prefixes the app strips before putting it on the wire.
///
/// `anthropic-version` is set only when the provider row did not already supply it: the AI SDK's
/// Anthropic client always sends it, so a request without one is not a request the app could make,
/// but an operator who pinned a different value keeps it.
///
/// A session id is minted when the client sent no marker. The app always has a conversation to name,
/// and a request the endpoint cannot correlate is a request it can reject; the cost of the minted id
/// is that two turns of one conversation do not share it, which is why the client's own marker is
/// preferred whenever there is one.
fn zcode_attribution_headers(headers: &mut HeaderMap, session_id: Option<&str>) {
    use mr_dialects::zcode as zc;

    headers
        .entry(zc::ANTHROPIC_VERSION_HEADER)
        .or_insert(HeaderValue::from_static(zc::ANTHROPIC_VERSION));

    let fresh = uuid::Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&fresh) {
        headers.insert(zc::REQUEST_ID_HEADER, value.clone());
        headers.insert(zc::TRACE_ID_HEADER, value);
    }
    headers.insert(
        zc::SESSION_TYPE_HEADER,
        HeaderValue::from_static(zc::SESSION_TYPE_MAIN),
    );
    let session = session_id.and_then(zcode_session_id).unwrap_or(&fresh);
    if let Ok(value) = HeaderValue::from_str(session) {
        headers.insert(zc::SESSION_ID_HEADER, value);
    }
}

/// Normalise a client's conversation marker into the id ZCode's endpoint expects.
///
/// Returns `None` for a marker that cannot be an id on this wire: the app strips a `sess_` or
/// `subagent_agent_` prefix, and its source-header sanitiser drops anything outside printable ASCII.
/// Normalising here rather than forwarding verbatim keeps a marker that was meaningful to the client
/// from reaching an endpoint that would reject it.
fn zcode_session_id(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let trimmed = trimmed
        .strip_prefix("subagent_agent_")
        .or_else(|| trimmed.strip_prefix("sess_"))
        .unwrap_or(trimmed);
    (!trimmed.is_empty()
        && trimmed.len() <= 128
        && trimmed.bytes().all(|b| (0x20..=0x7e).contains(&b)))
    .then_some(trimmed)
}

/// Build the outgoing headers for one attempt.
#[allow(clippy::too_many_arguments)]
fn upstream_headers(
    provider: &mr_core::snapshot::ProviderPlan,
    step: &StepPlan,
    credential: Option<&str>,
    client_headers: &HeaderMap,
    head: &RequestHead,
    force_stream: bool,
    translated_content_type: Option<&'static str>,
    oauth_headers: &[(HeaderName, HeaderValue)],
) -> HeaderMap {
    let mut map = HeaderMap::with_capacity(provider.headers.len() + 3);
    for (name, value) in &provider.headers {
        let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) else {
            // A malformed header in a provider's configuration is an operator typo. Skipping it logs
            // the problem; refusing every request would turn a typo into an outage.
            tracing::warn!(header = %name, "skipping an invalid provider header");
            continue;
        };
        map.insert(n, v);
    }

    apply_auth(&mut map, &step.auth, credential);
    for (name, value) in oauth_headers {
        map.insert(name.clone(), value.clone());
    }

    // A translated body is JSON by construction, so the client's content type is dropped for it —
    // forwarding `multipart/form-data` alongside a JSON body would be a lie the upstream acts on.
    // Untranslated bodies keep the client's content type, so multipart uploads stay multipart.
    match translated_content_type {
        Some(ct) => {
            map.insert("content-type", HeaderValue::from_static(ct));
        }
        None => {
            if let Some(ct) = client_headers.get("content-type") {
                map.insert("content-type", ct.clone());
            }
        }
    }

    // Command Code and Codex answer only in `text/event-stream`, whatever the client asked for. A
    // non-streaming client still gets an event stream from upstream; the relay folds it back into
    // one JSON document on the way out. Asking for JSON here would get a JSON *error* instead.
    if head.wants_stream() || force_stream {
        map.insert("accept", HeaderValue::from_static("text/event-stream"));
    }

    // Command Code will not answer without these two: they are how its CLI identifies itself, and
    // the constants live next to the translator that writes the body so the two cannot drift apart.
    if step.dialect == Dialect::CommandCodeGo {
        map.insert(
            "x-command-code-version",
            HeaderValue::from_static(mr_dialects::commandcode::COMMAND_CODE_VERSION),
        );
        map.insert(
            "x-cli-environment",
            HeaderValue::from_static(mr_dialects::commandcode::CLI_ENVIRONMENT),
        );
    }

    // ZCode's plan endpoint is Anthropic Messages behind the desktop app's own correlation
    // headers. `apply_auth` has already put the credential in `x-api-key`; what is left is what the
    // app's attribution layer attaches to every model request, and `x-session-id` is the one its
    // signing layer refuses to sign without. The app's *identity* headers — `User-Agent`,
    // `X-ZCode-App-Version`, `X-Title`, `X-Platform` and the `X-Client-*` family — are deliberately
    // absent: they ride its configuration requests, not its model requests, and a header the app
    // would never send is exactly what this dialect exists to avoid.
    if step.dialect == Dialect::Zcode {
        zcode_attribution_headers(&mut map, client_session_hint(client_headers));
    }

    // Cloud Code refuses clients below a rolling minimum build ("This version of Antigravity is
    // no longer supported"). The recorded fingerprint is a browser User-Agent whose version is a
    // literal, so rewrite just that token from the release feed; every other recorded header stays
    // exactly as the operator configured it.
    if step.dialect == Dialect::Antigravity {
        if let Some(rewritten) = map
            .get("user-agent")
            .and_then(|value| value.to_str().ok())
            .and_then(mr_core::quota::antigravity::user_agent_with_current_ide_version)
        {
            match HeaderValue::from_str(&rewritten) {
                Ok(value) => {
                    map.insert("user-agent", value);
                }
                Err(_) => {
                    tracing::warn!("the resolved Antigravity user agent is not a valid header");
                }
            }
        }
    }

    map
}

/// The client route a request arrived on, from its path.
///
/// The fallback is Chat Completions because that is the route an unknown path cannot have reached:
/// `router()` registers an explicit list, and a path outside it is a 404 before this is called.
fn route_kind_for(path: &str) -> RouteKind {
    // Exact paths first: a path that is exactly a route can never be confused with a prefix of a
    // sibling, and reading them in one table is what makes "every registered route maps to its own
    // kind" checkable at a glance.
    match path {
        "/v1/chat/completions" => return RouteKind::ChatCompletions,
        "/v1/responses" => return RouteKind::Responses,
        "/v1/embeddings" => return RouteKind::Embeddings,
        "/v1/audio/transcriptions" => return RouteKind::AudioTranscription,
        "/v1/audio/translations" => return RouteKind::AudioTranslation,
        "/v1/audio/speech" => return RouteKind::AudioSpeech,
        "/v1/images/generations" => return RouteKind::ImagesGenerations,
        "/v1/images/edits" => return RouteKind::ImagesEdits,
        "/v1/images/variations" => return RouteKind::ImagesVariations,
        "/v1/moderations" => return RouteKind::Moderations,
        "/v1/models" => return RouteKind::Models,
        "/v1/messages" => return RouteKind::Messages,
        "/v1/messages/count_tokens" => return RouteKind::CountTokens,
        "/v1/videos" => return RouteKind::VideoCreate,
        "/v1/realtime/client_secrets" => return RouteKind::RealtimeClientSecrets,
        "/v1/realtime/calls" => return RouteKind::RealtimeCalls,
        "/v1/search" => return RouteKind::Search,
        _ => {}
    }

    // Paths that carry an id after a fixed prefix. Videos are the only family whose *read* is
    // addressed by id in the path rather than by a model in the body, so the suffix decides between
    // the metadata read and the binary content read.
    if let Some(rest) = path.strip_prefix("/v1/videos/") {
        let rest = rest.trim_end_matches('/');
        if !rest.is_empty() {
            return match rest.strip_suffix("/content") {
                Some(id) if !id.is_empty() && !id.contains('/') => RouteKind::VideoContent,
                _ => RouteKind::VideoRetrieve,
            };
        }
    }

    // Anything else is a chat-completions request. The fallback is deliberate rather than an error:
    // a client may spell the chat path with a trailing slash, and a 404 for a path the router does
    // not serve is produced by axum's own fallback, not by this function guessing wrong.
    RouteKind::ChatCompletions
}

/// Apply a provider's auth to the outgoing headers.
///
/// A credential is **never** placed in a URL. Query-parameter auth is recognised and deliberately not
/// applied: a key in a URL ends up in provider logs, in intermediary logs and in browser history, and
/// no provider in the catalogue requires it.
fn apply_auth(headers: &mut HeaderMap, auth: &AuthKind, credential: Option<&str>) {
    let Some(secret) = credential else {
        // Sending an empty `Authorization` would have the provider parse it and reject it with a
        // confusing message. Sending nothing lets it say "missing credentials", which is true.
        return;
    };

    if matches!(auth, AuthKind::Modal) {
        let Some((key, token_secret)) = secret.split_once('.') else {
            tracing::warn!(
                "could not build a Modal auth header; the credential must be TokenId.TokenSecret"
            );
            return;
        };
        match (
            HeaderValue::from_str(key),
            HeaderValue::from_str(token_secret),
        ) {
            (Ok(key), Ok(token_secret)) => {
                headers.insert("modal-key", key);
                headers.insert("modal-secret", token_secret);
            }
            _ => tracing::warn!(
                "could not build a Modal auth header; the credential contains invalid header bytes"
            ),
        }
        return;
    }

    let (name, value) = match auth {
        AuthKind::Bearer => ("authorization".to_owned(), format!("Bearer {secret}")),
        AuthKind::ApiKeyHeader { header } => (header.to_ascii_lowercase(), secret.to_owned()),
        AuthKind::Header { name, prefix } => {
            let value = if prefix.is_empty() {
                secret.to_owned()
            } else {
                format!("{prefix}{secret}")
            };
            (name.to_ascii_lowercase(), value)
        }
        AuthKind::Modal => unreachable!("modal auth is handled above"),
        AuthKind::QueryParam { .. } => {
            tracing::warn!(
                "query-parameter authentication is not applied; the credential will be missing"
            );
            return;
        }
        AuthKind::OAuth { .. } => {
            // The token exchange happens in the control plane, but the relay refreshes an expiring
            // token itself just before use (`Refresher::ensure_fresh`, called in the step loop above)
            // so a long-lived relay does not need a restart to pick up a new access token. By the
            // time this runs, `secret` is already the current access token.
            ("authorization".to_owned(), format!("Bearer {secret}"))
        }
    };

    match (
        HeaderName::from_bytes(name.as_bytes()),
        HeaderValue::from_str(&value),
    ) {
        (Ok(n), Ok(v)) => {
            headers.insert(n, v);
        }
        _ => tracing::warn!("could not build an auth header; the credential will be missing"),
    }
}

/// Turn an upstream response into a client response.
///
/// The body is streamed through as it arrives. Buffering would defeat streaming entirely, and
/// re-encoding would risk altering bytes the provider chose deliberately.
/// Build the client-facing response, translating the body back when the request was translated.
///
/// `translated` is whether `translate_request` produced a body. It is the response half's only
/// signal, and it is deliberately the same flag: a pairing that needed no request translation needs
/// no response translation either, and deriving the two from one decision is what stops them
/// disagreeing. `translate_request` already knows the answer — `is_passthrough` is documented as a
/// necessary but not sufficient condition, so re-deriving it here could only introduce a second,
/// subtler opinion.
async fn build_client_response(
    ctx: TranslateCtx<'_>,
    translated: bool,
    upstream: crate::upstream::UpstreamResponse,
    ticket: Option<std::sync::Arc<crate::quota::QuotaTicket>>,
) -> Response {
    if !translated {
        return build_response(upstream, ticket);
    }

    let status = upstream.status;
    let headers = upstream.headers.clone();

    // A streaming client gets a streaming answer, frame by frame. A non-streaming client gets a
    // single document, which means an upstream that always streams has to be folded — that is the
    // ordinary case for Command Code, whose every answer is an event stream.
    let body = if ctx.client_streaming {
        // `axum::body::Body` requires `'static`, so nothing that borrows this function's `ctx` can
        // be captured. The scalars are lifted out and the model id is owned here, and a fresh
        // context is built per frame from the owned id — which is why only the id needs copying.
        let route = ctx.route;
        let client_dialect = ctx.client_dialect;
        let upstream_dialect = ctx.upstream_dialect;
        let client_streaming = ctx.client_streaming;
        let model = ctx.upstream_model.to_owned();

        let frames = BodyStream::new(match &ticket {
            // The tap is only present for a metered key. Wrapping every body would mean parsing
            // every frame of every request, and the accounting is only worth that where a cap
            // depends on it.
            Some(ticket) => http_body_util::combinators::UnsyncBoxBody::new(
                crate::quota::TapBody::new(upstream.body, std::sync::Arc::clone(ticket)),
            ),
            None => http_body_util::combinators::UnsyncBoxBody::new(upstream.body),
        });
        // The buffer is carried across frames because a `data:` line has no obligation to arrive in
        // one piece; translating a half-line would corrupt the stream in a way that looks like an
        // upstream fault. `scan` is what makes the carry-over expressible without a mutex.
        //
        // `started` is what makes the *stream* boundary real for the dialects that still accumulate
        // in per-thread state. A stream that ended without translating a terminal frame — upstream
        // died, client hung up, this attempt was abandoned — leaves partial state behind, and the
        // next stream translated on this thread would inherit it. Clearing on the first frame of
        // every stream is what stops that; it cannot be done once around the whole stream, because
        // the body is polled on whichever worker the task happens to be scheduled on.
        //
        // Command Code does **not** rely on this, and must not: a reset keyed on a worker thread
        // cannot separate two answers in flight on that thread — it wipes the *other* live stream's
        // state instead, which is the same corruption seen from the other side. Its state is
        // `commandcode` below, owned per stream and dropped with the body.
        let mut commandcode = mr_dialects::commandcode::Translator::new();
        // Codex is the other dialect whose stream state is per answer, and for the same reason: a
        // thread-local cannot tell two answers in flight on one worker apart.
        let mut responses = mr_dialects::responses::Translator::new();
        // A `None` marker closes the stream: it is how the last partial line in `pending` and any
        // closing frame the translator still owes the client get written before the body ends.
        let frames = frames
            .map(Some)
            .chain(futures_util::stream::once(async { None }));
        let stream = frames.scan(
            (Vec::<u8>::new(), false, false),
            move |(pending, started, failed), frame| {
                if !*started {
                    *started = true;
                    mr_dialects::reset_stream_state();
                }
                let ctx = TranslateCtx {
                    route,
                    client_dialect,
                    upstream_dialect,
                    upstream_model: &model,
                    client_streaming,
                    session_id: None,
                };
                let out = match frame {
                    Some(Ok(frame)) => frame.into_data().ok().map(|chunk| {
                        translate_stream_chunk(
                            &ctx,
                            &mut commandcode,
                            &mut responses,
                            pending,
                            &chunk,
                        )
                    }),
                    Some(Err(e)) => {
                        *failed = true;
                        Some(vec![Err(e)])
                    }
                    None => Some(flush_stream_chunk(
                        &ctx,
                        &mut commandcode,
                        &mut responses,
                        pending,
                        *failed,
                    )),
                };
                async move { Some(out) }
            },
        );
        let stream = stream.filter_map(|item| async move { item });
        let stream = stream.flat_map(|chunks| {
            futures_util::stream::iter(chunks.into_iter().map(|c| c.map(http_body::Frame::data)))
        });
        AxumBody::new(StreamBody::new(stream))
    } else {
        let collected = match &ticket {
            Some(ticket) => {
                crate::quota::TapBody::new(upstream.body, std::sync::Arc::clone(ticket))
                    .collect()
                    .await
            }
            None => upstream.body.collect().await,
        };
        match collected {
            Ok(collected) => {
                let raw = collected.to_bytes();
                match mr_dialects::translate_response_body(&ctx, &raw) {
                    Ok(out) => AxumBody::from(out),
                    Err(e) => {
                        tracing::warn!(error = %e, "could not translate the upstream response");
                        AxumBody::from(raw)
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the upstream body did not finish");
                AxumBody::empty()
            }
        }
    };

    let mut response = Response::new(body);
    *response.status_mut() = status;
    // The content type describes the body *this* function produced, not the one the upstream sent,
    // and the two disagree whenever the request was translated. A non-streaming client gets a
    // single folded JSON document no matter that the upstream only ever streams; forwarding
    // `text/event-stream` alongside it was actively harmful, because the recovery layer keys its
    // event framing on that type and would hold the document waiting for a blank-line-terminated
    // event that never comes, then drop the whole answer at end of stream. Codex and Command Code
    // both stream unconditionally, so both produced an empty 200 for every non-streaming client.
    let translated_content_type = if ctx.client_streaming {
        "text/event-stream; charset=utf-8"
    } else {
        "application/json; charset=utf-8"
    };
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }
    if let Ok(value) = HeaderValue::from_str(translated_content_type) {
        response.headers_mut().insert("content-type", value);
    }
    response
}

/// Translate one upstream chunk, carrying any partial trailing line into the next call.
///
/// Returns one result per output frame. A chunk that contains no complete line yields nothing, and
/// its bytes stay in `pending` — an event stream is line-oriented, so a chunk boundary in the middle
/// of a `data:` line is normal rather than exceptional.
fn translate_stream_chunk(
    ctx: &TranslateCtx<'_>,
    commandcode: &mut mr_dialects::commandcode::Translator,
    responses: &mut mr_dialects::responses::Translator,
    pending: &mut Vec<u8>,
    chunk: &Bytes,
) -> Vec<Result<Bytes, hyper::Error>> {
    // The carry is kept as bytes, not as `str`. A multi-byte character split across two frames is
    // normal on a byte stream, and decoding each frame on its own would replace both halves with
    // U+FFFD; only complete lines are decoded, so the character arrives intact with the next frame.
    pending.extend_from_slice(chunk);

    // When both ends speak the same dialect the bytes are passed through untouched, so the
    // upstream's own framing — `event:` names, separators — is what the client expects. Once
    // anything is translated it is not: the translator emits complete frames of its own, in the
    // *client's* dialect, and an upstream frame marker forwarded alongside them describes an event
    // that does not exist. That is the difference between a chat client receiving chat chunks and
    // it receiving `event: response.created` with no data at all.
    let pass_through = ctx.upstream_dialect == ctx.client_dialect;

    let mut out = Vec::new();
    while let Some(idx) = pending.iter().position(|b| *b == b'\n') {
        let line = String::from_utf8_lossy(&pending[..=idx]).into_owned();
        pending.drain(..=idx);
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // The blank line that ends an upstream event. Translated frames already end with their
            // own separator, so forwarding this one dispatches an extra, empty event between them,
            // and a client that parses each event's data reads an empty string. Only a pass-through
            // needs it, where it separates the upstream's own frames.
            if pass_through {
                out.push(Ok(Bytes::from_static(b"\n")));
            }
            continue;
        }
        let data = trimmed.strip_prefix("data: ").or_else(|| {
            // The live Command Code endpoint emits newline-delimited JSON event objects rather than
            // standard SSE `data:` records. Treat each JSON line as one event; metadata events are
            // ignored by the dialect translator, while text/tool/finish events become OpenAI SSE.
            (ctx.upstream_dialect == Dialect::CommandCodeGo && trimmed.starts_with('{'))
                .then_some(trimmed)
        });
        match data {
            Some(data) => match if ctx.upstream_dialect == Dialect::CommandCodeGo {
                mr_dialects::commandcode::translate(commandcode, ctx, data)
            } else if ctx.upstream_dialect == Dialect::Codex && !ctx.is_passthrough() {
                responses.translate(ctx, data)
            } else {
                mr_dialects::translate_sse_data(ctx, data)
            } {
                Ok(lines) => {
                    for line in lines {
                        if line.is_empty() {
                            continue;
                        }
                        out.push(Ok(translated_frame(&line)));
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not translate an event-stream frame");
                }
            },
            // Command Code metadata JSON is intentionally suppressed.
            None if ctx.upstream_dialect == Dialect::CommandCodeGo => {}
            // A comment is a keep-alive: no payload, ignored by every SSE parser.
            None if trimmed.starts_with(':') => out.push(Ok(Bytes::from(format!("{trimmed}\n")))),
            None if pass_through => out.push(Ok(Bytes::from(format!("{trimmed}\n")))),
            // `event:`, `id:` and `retry:` name a frame in the *upstream's* dialect. The translated
            // output's frames are named after the client's, so keeping these would either mislabel a
            // chunk or — with no `data:` to join — hand a strict client an event whose data is empty
            // and unparseable, which surfaces as malformed server-sent event JSON.
            None => {}
        }
    }
    out
}

/// Write out whatever the end of the upstream body still owes the client.
///
/// Two things are still held when the upstream stops sending: the bytes of a line that never got its
/// newline, and the closing frame of a translated dialect. Both are part of the answer. A final
/// `data: [DONE]` very often arrives without a trailing newline, and a Codex stream whose terminal
/// event was cut off still has to be closed honestly — see
/// [`mr_dialects::responses::Translator::finish`].
fn flush_stream_chunk(
    ctx: &TranslateCtx<'_>,
    commandcode: &mut mr_dialects::commandcode::Translator,
    responses: &mut mr_dialects::responses::Translator,
    pending: &mut Vec<u8>,
    upstream_failed: bool,
) -> Vec<Result<Bytes, hyper::Error>> {
    let mut out = Vec::new();
    if !pending.is_empty() {
        let mut tail = std::mem::take(pending);
        tail.push(b'\n');
        out.extend(translate_stream_chunk(
            ctx,
            commandcode,
            responses,
            pending,
            &Bytes::from(tail),
        ));
    }
    if ctx.upstream_dialect == Dialect::Codex && !ctx.is_passthrough() {
        for line in responses.finish(ctx) {
            if !line.is_empty() {
                out.push(Ok(translated_frame(&line)));
            }
        }
    }
    // Gemini (and the Antigravity envelope around it) does not always state a reason for
    // finishing. A clean end of body is still the end of the answer, and a strict Chat client
    // rejects an unterminated stream even when every token arrived. A broken body is different:
    // it ends without a synthesized completion.
    if matches!(
        ctx.upstream_dialect,
        Dialect::Google | Dialect::Antigravity | Dialect::GoogleAiStudio
    ) && !ctx.is_passthrough()
        && !upstream_failed
    {
        match mr_dialects::google::finish_stream(ctx) {
            Ok(lines) => {
                for line in lines {
                    out.push(Ok(translated_frame(&line)));
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not close the translated Gemini stream"),
        }
    }
    out
}

/// One translated frame, on the wire exactly once.
///
/// Two things go wrong if this is spliced naively. A frame the dialect layer rendered already ends
/// with its blank line, so appending another separator dispatches an extra empty event between
/// frames — an event whose `data` is the empty string, which a client that parses every event
/// cannot read. And a translator that hands back a bare payload still has to become an SSE frame:
/// without the `data:` field the line is not a field at all, so the client silently receives
/// nothing. Both are invisible in a test that only checks the payloads are present.
fn translated_frame(line: &str) -> Bytes {
    if line.ends_with("\n\n") {
        return Bytes::from(line.to_owned());
    }
    let mut frame = String::with_capacity(line.len() + 8);
    if !line.starts_with("data:")
        && !line.starts_with("event:")
        && !line.starts_with(':')
        && !line.starts_with("id:")
    {
        frame.push_str("data: ");
    }
    frame.push_str(line);
    if !frame.ends_with('\n') {
        frame.push('\n');
    }
    frame.push('\n');
    Bytes::from(frame)
}

/// Headers that describe the connection between the relay and the provider, hoisted so the direct
/// and translated response paths drop exactly the same set.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// Whether a step still has an account that has not been cooled down.
///
/// Used after a failed attempt to decide whether retrying the *same* step can reach a different
/// account. The failed account is cooled down before this runs, so its own entry excludes it.
fn step_has_a_live_account(
    app: &AppState,
    table: &RouteTable,
    step: &StepPlan,
    now_ms: u64,
) -> bool {
    let health = app.health.view();
    table
        .snapshot
        .providers()
        .get(step.provider_index as usize)
        .is_some_and(|provider| {
            select::step_has_usable_account(provider, &health, now_ms, &step.model)
        })
}

fn build_response(
    upstream: crate::upstream::UpstreamResponse,
    ticket: Option<std::sync::Arc<crate::quota::QuotaTicket>>,
) -> Response {
    let status = upstream.status;
    let mut response = Response::new(AxumBody::new(match &ticket {
        // The tap reads the bytes the client is about to receive and changes none of them; it only
        // notices what the upstream said it spent.
        Some(ticket) => http_body_util::combinators::UnsyncBoxBody::new(
            crate::quota::TapBody::new(upstream.body, std::sync::Arc::clone(ticket)),
        ),
        None => http_body_util::combinators::UnsyncBoxBody::new(upstream.body),
    }));
    *response.status_mut() = status;

    // Hop-by-hop headers describe the connection between the relay and the provider. Forwarding them
    // would tell the client something false about its own connection. `content-length` is dropped
    // because the body is streamed from a source whose length is not known here.
    for (name, value) in upstream.headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str()) {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }

    response
}

/// A JSON response.
pub(crate) fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let mut response = Response::new(AxumBody::from(bytes));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    response
}

/// An error in the dialect the client asked in.
///
/// The two envelopes genuinely differ. A client debugging a `/v1/messages` failure must not be handed
/// OpenAI's fields — answering in the dialect you were asked in is the entire point of a shim.
pub(crate) fn error_response(
    status: StatusCode,
    dialect: Dialect,
    code: &str,
    message: &str,
) -> Response {
    let body = match dialect {
        Dialect::Anthropic => serde_json::json!({
            "type": "error",
            "error": { "type": code, "message": message },
        }),
        _ => serde_json::json!({
            "error": { "message": message, "type": code, "code": code },
        }),
    };
    json_response(status, body)
}

/// A refused request, in the dialect the client asked in.
///
/// The status is 429 and the code is `rate_limit_error`, because that is what a client already
/// knows how to read: a budget that has been spent is a rate limit from the caller's point of
/// view, and a client that retries on a 429 with backoff will behave sensibly against it.
fn quota_error(denial: &crate::quota::Denial, dialect: Dialect) -> Response {
    let mut response = error_response(
        StatusCode::TOO_MANY_REQUESTS,
        dialect,
        "rate_limit_error",
        &denial.message,
    );
    if let Some(ms) = denial.retry_after_ms {
        // `retry-after` is in seconds, and a fractional value is not allowed by the header's
        // grammar, so a sub-second wait is rounded up to one second rather than to zero — telling
        // a client to retry immediately is worse than telling it to wait a moment too long.
        let seconds = (ms as f64 / 1000.0).ceil().max(1.0) as u64;
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            response.headers_mut().insert("retry-after", value);
        }
    }
    response
}

/// Bind a listener that several workers can share.
///
/// `SO_REUSEPORT` lets the kernel spread accepts across workers, which removes the single accept queue
/// as a serialisation point: each worker gets its own queue and connections are hashed across them, so
/// a burst does not queue behind one thread's `accept`.
#[cfg(unix)]
fn bind_reuseport(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Bind without `SO_REUSEPORT`, for platforms that do not have it.
#[cfg(not(unix))]
fn bind_reuseport(addr: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Serve until the process is asked to stop.
pub async fn serve(state: AppState, addr: SocketAddr) -> std::io::Result<()> {
    let listener = bind_reuseport(addr)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    tracing::info!(%addr, "relay listening");

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
}

/// Resolve when the process is asked to stop.
///
/// Graceful shutdown earns its place here for a specific reason: a completion in flight is a request
/// the client is waiting on and, on a metered provider, one that has already been billed. Cutting it
/// off to save a second of shutdown time spends the operator's money.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("interrupt received; draining"),
        _ = terminate => tracing::info!("termination requested; draining"),
    }
}

// --- The batch lane's executor --------------------------------------------------------------

/// The relay's implementation of [`crate::batch::ItemExecutor`]: where a batch item goes, and how
/// it is dispatched.
///
/// The lane owns *when* an item runs and what it costs; this owns *where* it goes. The split is what
/// makes the lane's scheduler testable without an HTTP server, and it is also why the two halves are
/// allowed to be in different modules: the lane must never import the request path, or the seam that
/// lets it admit an item before dispatch (and therefore refuse to start work it cannot run) would
/// disappear.
///
/// `app` is installed after the state is built, because the state owns the lane and the lane owns
/// this executor: constructing one of them first and the other second is unavoidable, and a
/// [`OnceLock`] says so without an `Option` that every caller has to unwrap.
#[derive(Clone, Default)]
pub struct ServerBatchExecutor {
    app: Arc<OnceLock<AppState>>,
}

impl ServerBatchExecutor {
    /// Attach the shared state. Called once, after `build_state` has assembled it.
    pub fn install(&self, app: AppState) {
        // A second install is a startup bug, not a runtime condition: the only writer is
        // `build_state`, and the lane cannot be running before the state exists. Ignoring it keeps
        // the failure visible in the logs without taking the process down.
        if self.app.set(app).is_err() {
            tracing::error!("the batch executor was installed twice; the first state is kept");
        }
    }

    /// The shared state, or `None` before startup finished.
    fn app(&self) -> Option<&AppState> {
        self.app.get()
    }
}

impl crate::batch::ItemExecutor for ServerBatchExecutor {
    fn plan(&self, body: &[u8]) -> Option<crate::batch::ItemPlan> {
        let app = self.app()?;
        let head = scan_request_head(body);
        let model_name = head.model_str(body)?;
        let table = Arc::clone(&*app.relay.table());
        let step = match table.snapshot.resolve(model_name) {
            // A combo, an alias chain, or a name that resolves to nothing: there is no single
            // endpoint to reserve against, so the item is dispatched like any other request and the
            // lane claims no cost for it. Guessing a provider here would bill the wrong card.
            Resolution::Direct { step, .. } => step,
            _ => return None,
        };
        let provider = table
            .snapshot
            .providers()
            .get(step.provider_index as usize)?;

        // The admission condition is copied from the request path on purpose. If this returned a
        // plan for a provider the request path would not admit — and the request path's own
        // reservation is skipped for the provider named here — then the item would run with no
        // reservation at all, which is exactly the over-subscription the pool exists to prevent.
        // The reverse mistake is cheaper but still a mistake: reserving where the request path does
        // not would write GPU-second ledger rows for an endpoint the operator did not enable
        // billing for.
        let admits = provider.kv_pool_tokens.is_some()
            || provider.max_concurrent.is_some()
            || (step.dialect == Dialect::ModalVllm && provider.price_per_second.is_some());
        if !admits {
            return None;
        }

        Some(crate::batch::ItemPlan {
            provider_id: step.provider_id.as_str().to_owned(),
            model: step.model.clone(),
            tokens: crate::quota::estimate_prompt_tokens(body.len()).tokens,
            limits: crate::kv_budget::KvBudgetLimits {
                pool_tokens: provider.kv_pool_tokens,
                max_context: step.context_length.map(u64::from).or(provider.max_context),
                max_concurrent: provider.max_concurrent,
            },
            price_per_second: provider.price_per_second,
        })
    }

    fn store(&self) -> Option<Arc<mr_core::store::Store>> {
        // The same store the request path bills against, so a batch's GPU-second rows and an
        // interactive request's rows land in one table and one `gpu_usage_totals` answers for both.
        self.app().map(|app| Arc::clone(&app.quota_store))
    }

    fn execute<'a>(&'a self, request: crate::batch::ItemRequest) -> crate::batch::ItemFuture<'a> {
        Box::pin(async move {
            let Some(app) = self.app().cloned() else {
                return crate::batch::ItemReply {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    headers: HeaderMap::new(),
                    body: Bytes::from_static(
                        br#"{"error":{"message":"the batch lane is not wired to a relay","type":"internal_error","code":"internal_error"}}"#,
                    ),
                    cold_starts: 0,
                };
            };

            // Cold starts are counted by observation, not by asking: the relay records a cold start
            // as a cooldown with `CooldownReason::ColdStart` on the model's cooldown scope, and the
            // only way to know one happened during *this* item is to see an entry appear (or change
            // to cold-start) between the two snapshots. Counting them as errors would be wrong;
            // counting them as nothing would hide the fixed fee the batch paid.
            // The health map is shared, so a clone of the handle is all the observation needs: the
            // dispatch may consume its own clone, and this one still sees what the dispatch recorded.
            let health = Arc::clone(&app.health);
            let before = health.snapshot();
            let response =
                dispatch_batch_item(app, request.uri, request.body, request.pre_admitted).await;
            let after = health.snapshot();
            let cold_starts = after
                .iter()
                .filter(|(key, cooldown)| {
                    cooldown.reason == select::CooldownReason::ColdStart
                        && before
                            .get(*key)
                            .is_none_or(|prior| prior.reason != select::CooldownReason::ColdStart)
                })
                .count() as u32;

            let status = response.status();
            let headers = response.headers().clone();
            // A batch item is never a stream, so the whole answer is buffered here. The bound is the
            // relay's own body limit: an upstream that answers a single item with more than that is
            // a failure of the upstream, not something to hold in memory.
            let body = match axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::warn!(error = %error, "a batch item's answer could not be read in full");
                    return crate::batch::ItemReply {
                        status: StatusCode::BAD_GATEWAY,
                        headers,
                        body: Bytes::from(
                            serde_json::json!({
                                "error": {
                                    "message": format!("the answer to a batch item could not be read: {error}"),
                                    "type": "upstream_error",
                                }
                            })
                            .to_string(),
                        ),
                        cold_starts,
                    };
                }
            };
            crate::batch::ItemReply {
                status,
                headers,
                body,
                cold_starts,
            }
        })
    }
}

/// Dispatch one batch item through the request path.
///
/// Deliberately **not** `router(app).oneshot(...)`: that would re-enter authentication, which has
/// already happened once for the batch's client at upload time, and it would re-read a body this
/// function already holds. What is reused is everything that matters — resolution, dialects, account
/// selection, cooldowns, fallbacks, the KV-token reservation and the GPU-second ledger.
///
/// The key check is skipped (`pre_authorized`) because the items are pre-authorized: the client that
/// uploaded the file proved its key then, and a key revoked between upload and execution should not
/// leave a half-run batch. The consequence — per-item client-key metering is not re-applied to the
/// individual items — is reported as a limitation of this lane rather than hidden.
pub(crate) async fn dispatch_batch_item(
    app: AppState,
    uri: Uri,
    body: Bytes,
    pre_admitted: Option<String>,
) -> Response {
    dispatch_routed(
        DispatchRequest {
            app,
            client_dialect: Dialect::OpenAi,
            method: Method::POST,
            uri,
            headers: HeaderMap::new(),
            body: AxumBody::from(body),
            pre_authorized: true,
            pre_admitted,
        },
        0,
        None,
    )
    .await
}

/// Authenticate a batch-surface request, returning the refusal to send when it may not run.
///
/// The batch surface is OpenAI-shaped, so its refusals are too — including the shape of a 401, which
/// is what a client that has the wrong key needs to see rather than a routing error.
pub(crate) fn authorize_batch_request(app: &AppState, headers: &HeaderMap) -> Option<Response> {
    let presented = presented_key(headers);
    let outcome = authenticate_request(&app.relay, app.no_key_required, presented.as_deref());
    if outcome.is_allowed() {
        None
    } else {
        Some(auth_error(&outcome, Dialect::OpenAi))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_requests_request_usage_and_leave_everything_else_alone() {
        let out = with_stream_usage(Bytes::from_static(br#"{"model":"m","stream":true}"#));
        let value: serde_json::Value = serde_json::from_slice(&out).expect("still json");
        assert_eq!(value["stream_options"]["include_usage"], true);
        assert_eq!(value["model"], "m");

        // A non-streaming request reports usage in its own body; nothing to ask for.
        let plain = Bytes::from_static(br#"{"model":"m"}"#);
        assert_eq!(with_stream_usage(plain.clone()), plain);

        // An explicit choice is respected rather than overridden.
        let chosen =
            Bytes::from_static(br#"{"stream":true,"stream_options":{"include_usage":false}}"#);
        assert_eq!(with_stream_usage(chosen.clone()), chosen);

        // A body the router cannot read is forwarded verbatim, never turned into an error.
        let opaque = Bytes::from_static(b"not json at all");
        assert_eq!(with_stream_usage(opaque.clone()), opaque);
    }

    #[test]
    fn antigravity_project_is_injected_for_translated_and_raw_envelopes() {
        let mut tokens = mr_core::oauth::TokenSet::new("secret");
        tokens.extra.insert(
            "cloudcode_project_id".into(),
            serde_json::Value::String("account-project".into()),
        );
        for input in [
            br#"{"model":"m"}"#.as_slice(),
            br#"{"model":"m","project":"attacker"}"#.as_slice(),
        ] {
            let output = inject_antigravity_project(input, Some(&tokens)).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(value["project"], "account-project");
        }
        assert!(inject_antigravity_project(br#"{}"#, None).is_err());
        assert!(inject_antigravity_project(b"not-json", Some(&tokens)).is_err());
    }

    #[test]
    fn no_key_required_permits_missing_credentials_but_still_checks_presented_keys() {
        let config = mr_core::config::Config::default();
        let relay = RelayState::new(
            &config,
            mr_core::snapshot::RoutingSnapshot::default(),
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        );
        assert!(authenticate_request(&relay, true, None).is_allowed());
        assert!(matches!(
            authenticate_request(&relay, false, None),
            AuthOutcome::Open
        ));
        assert!(matches!(
            authenticate_request(&relay, true, Some("invalid")),
            AuthOutcome::Rejected
        ));
    }

    fn combo_plan(
        strategy: mr_core::dialect::ComboStrategy,
        window: u32,
    ) -> mr_core::snapshot::ComboPlan {
        mr_core::snapshot::ComboPlan {
            id: mr_core::ids::ComboId::generate(),
            name: "test".into(),
            strategy,
            steps: Vec::new(),
            hedge: mr_core::snapshot::HedgePlan {
                enabled: false,
                participants: Vec::new(),
                window_ms: 0,
                winner_metric: Default::default(),
                on_winner_fail: Default::default(),
                cancel_losers: true,
                loser_grace_ms: 0,
            },
            midstream_policy: Default::default(),
            sticky_success_window_secs: window,
            max_retries: 0,
            retry_delay_ms: 0,
            affinity: false,
            compression: Default::default(),
            signature: 0,
        }
    }

    #[test]
    fn sticky_success_is_not_gated_on_hedging_or_on_the_plain_priority_strategy() {
        use mr_core::dialect::ComboStrategy;
        // The bug this pins: stickiness used to require `hedge.enabled`, so a reset-aware combo
        // with hedging off could never keep a step, and every failover bounced back to the head of
        // the list on the next request, losing the provider's prompt cache each time.
        assert!(sticky_applies(&combo_plan(
            ComboStrategy::ResetAwarePriority,
            1800
        )));
        assert!(sticky_applies(&combo_plan(ComboStrategy::Priority, 1800)));
        // Round-robin exists to spread load; pinning it would defeat it.
        assert!(!sticky_applies(&combo_plan(
            ComboStrategy::RoundRobin,
            1800
        )));
        // Zero is the operator switching it off.
        assert!(!sticky_applies(&combo_plan(
            ComboStrategy::ResetAwarePriority,
            0
        )));
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn a_stated_output_cap_is_read_from_the_shapes_that_actually_carry_one() {
        let cap = |body: &str| stated_output_cap(&serde_json::from_str(body).unwrap());
        assert_eq!(cap(r#"{"max_tokens":8000}"#), Some(8000));
        assert_eq!(cap(r#"{"max_completion_tokens":4096}"#), Some(4096));
        assert_eq!(cap(r#"{"max_output_tokens":2048}"#), Some(2048));
        assert_eq!(
            cap(r#"{"generationConfig":{"maxOutputTokens":65536}}"#),
            Some(65536)
        );
        // Antigravity's envelope buries the Gemini payload under `request`.
        assert_eq!(
            cap(r#"{"model":"m","request":{"generationConfig":{"maxOutputTokens":64000}}}"#),
            Some(64000)
        );
        // A number in prose is not a cap on the answer.
        assert_eq!(cap(r#"{"messages":[{"content":"max_tokens is 5"}]}"#), None);
    }

    #[test]
    fn the_relay_fits_a_thinking_budget_under_the_requests_own_max_tokens() {
        // The Antigravity failure this pins: the compiled budget was under the model's ceiling but
        // far over the request's `max_tokens`, and Anthropic answered 400 `max_tokens` must be
        // greater than `thinking.budget_tokens` — a clash the relay could see coming, because both
        // numbers are in the body it is already rewriting.
        let mut step = step(Dialect::Anthropic, "http://upstream");
        step.reasoning_json[mr_core::dialect::ReasoningLevel::High.rank() as usize] =
            Some(r#"{"thinking":{"type":"enabled","budget_tokens":59904}}"#.to_string());

        let body =
            br#"{"model":"m","max_tokens":8000,"messages":[{"role":"user","content":"hi"}]}"#;
        let rewritten = apply_reasoning_level(body, &step, false, None)
            .unwrap()
            .expect("the fragment has to be rewritten, so the body changes");
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["thinking"]["budget_tokens"], 8000 - 4096);
        // Everything else the client sent survives the rewrite untouched.
        assert_eq!(value["max_tokens"], 8000);
        assert_eq!(value["messages"][0]["content"], "hi");

        // With no room to think and answer, no level is sent at all rather than a budget the
        // upstream would refuse.
        let tiny = br#"{"model":"m","max_tokens":2000}"#;
        assert!(
            apply_reasoning_level(tiny, &step, false, None)
                .unwrap()
                .is_none(),
            "a clamp to nothing must leave the body byte-for-byte alone"
        );
    }

    #[test]
    fn an_ai_studio_step_is_sent_a_thinking_level_and_never_a_budget_with_it() {
        // The failure this pins, reproduced against the public API on 2026-11-11: a compiled
        // Google fragment states a level *and* a budget, and the AI Studio surface answers 400 to
        // that pair — "You can only set only one of thinking budget and thinking level." — and to
        // any budget above 65535 ("thinking_budget must be in the range [-1, 65535]"). So the
        // dialect's own fragment must arrive here carrying the level alone, whatever the client
        // sent. The step is compiled from a configuration rather than hand-written so the dialect
        // default is what is under test, not a literal repeated in the test.
        let body = br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#;
        let rewritten = |dialect: Dialect| {
            let step = snapshot_with(
                dialect,
                "gemini-3.6-flash",
                &[ReasoningLevel::High],
                &[],
                &[],
            )
            .providers()[0]
                .steps[0]
                .clone();
            let rewritten = apply_reasoning_level(body, &step, false, None)
                .unwrap()
                .expect("a level was compiled, so the body has to change");
            let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
            (step, value)
        };

        let (step, value) = rewritten(Dialect::GoogleAiStudio);
        assert_eq!(
            value["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert_eq!(
            value["generationConfig"]["thinkingConfig"].get("thinkingBudget"),
            None,
            "the two thinking controls are mutually exclusive on this surface"
        );
        // The request is still retargeted and otherwise untouched: dropping the budget must not
        // quietly turn the rewrite into a no-op for the other fields.
        assert_eq!(value["model"], "gemini-3.6-flash");
        assert_eq!(value["messages"][0]["content"], "hi");
        assert_eq!(step.dialect, Dialect::GoogleAiStudio);

        // The control: the Cloud Code shape is a different surface and keeps both fields, which is
        // why this is a dialect rather than a change to Google's fragment.
        let (_, cloud_code) = rewritten(Dialect::Google);
        assert_eq!(
            cloud_code["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        assert!(
            cloud_code["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_some(),
            "Cloud Code tolerates the pair, so its fragment must be left alone"
        );
    }

    fn step(dialect: Dialect, base_url: &str) -> StepPlan {
        StepPlan {
            provider_index: 0,
            provider_id: mr_core::ids::ProviderId::from_raw("p"),
            model: "m".into(),
            dialect,
            auth: AuthKind::Bearer,
            base_url: base_url.into(),
            connect_timeout_ms: 1,
            ttfb_timeout_ms: 1,
            idle_timeout_ms: 1,
            total_timeout_ms: 1,
            level: mr_core::dialect::ReasoningLevel::High.rank(),
            // A model that advertises reasoning, so the helper's steps look like the ones the
            // compiler produces for a real provider.
            levels: level_mask(&mr_core::dialect::ReasoningLevel::ALL),
            default_level: mr_core::dialect::ReasoningLevel::High.rank(),
            reasoning_json: Default::default(),
            input_modalities: 1,
            pricing: None,
            context_length: None,
        }
    }

    fn level_mask(levels: &[mr_core::dialect::ReasoningLevel]) -> u32 {
        levels
            .iter()
            .fold(0, |mask, level| mask | (1u32 << level.rank()))
    }

    /// A minimal provider plan: no headers, no accounts, no route overrides.
    fn provider_plan() -> mr_core::snapshot::ProviderPlan {
        mr_core::snapshot::ProviderPlan {
            id: mr_core::ids::ProviderId::from_raw("p"),
            label: None,
            dialect: Dialect::OpenAi,
            base_url: "https://example.com".into(),
            auth: AuthKind::Bearer,
            key_strategy: Default::default(),
            cooldown_scope: Default::default(),
            proxy: None,
            accounts: Vec::new(),
            capabilities: Vec::new(),
            connect_timeout_ms: 1,
            ttfb_timeout_ms: 1,
            idle_timeout_ms: 1,
            total_timeout_ms: 1,
            headers: Default::default(),
            route_overrides: Default::default(),
            http2: Default::default(),
            prewarm: false,
            kv_pool_tokens: None,
            max_context: None,
            max_concurrent: None,
            price_per_second: None,
            cold_start_ttfb_timeout_ms: None,
            steps: Vec::new(),
            hidden_models: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // `GET /v1/models` and the reasoning level a name selects
    // -----------------------------------------------------------------------------------------

    /// A snapshot with one provider whose single model carries explicit metadata.
    fn snapshot_with(
        dialect: Dialect,
        model_id: &str,
        levels: &[ReasoningLevel],
        modalities: &[InputModality],
        combos: &[(&str, &str)],
    ) -> mr_core::snapshot::RoutingSnapshot {
        use mr_core::model::{ModelEntry, ModelSource};

        let mut config = mr_core::config::Config::default();
        let mut provider = mr_core::model::Provider::new("p1", dialect, "https://p1.example/v1");
        provider.models = vec![ModelEntry {
            id: model_id.into(),
            source: ModelSource::Manual,
            label: None,
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_modalities: modalities.to_vec(),
            reasoning_levels: levels.to_vec(),
            default_reasoning_level: None,
            reasoning_wire: None,
            input_price_per_mtok: None,
            output_price_per_mtok: None,
            pricing: None,
            synced_at: None,
            hidden: false,
        }];
        provider.accounts = vec![mr_core::model::Account::new("a")];
        config.providers = vec![provider];
        config.combos = combos
            .iter()
            .map(|(id, name)| {
                let mut combo = mr_core::model::Combo::new(
                    *name,
                    mr_core::model::ComboStep::new("p1", model_id),
                );
                combo.id = mr_core::ids::ComboId::from_raw(*id);
                combo
            })
            .collect();
        mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap()
    }

    /// An otherwise-empty [`ModelEntry`], so a test can name exactly the fields it cares about.
    ///
    /// Every other field is a `Default`, which is the honest starting point: a test that sets only
    /// `reasoning_levels` is asserting what happens when nothing else is declared.
    fn dm() -> mr_core::model::ModelEntry {
        mr_core::model::ModelEntry {
            id: String::new(),
            source: mr_core::model::ModelSource::Manual,
            label: None,
            context_length: None,
            max_output_tokens: None,
            capabilities: Vec::new(),
            input_modalities: Vec::new(),
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

    fn listed_ids(snapshot: &mr_core::snapshot::RoutingSnapshot) -> Vec<String> {
        model_listing(snapshot)
            .into_iter()
            .map(|entry| entry["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_model_listing_advertises_only_the_levels_a_model_really_offers() {
        // The listing is a client's contract: a name it finds here must work verbatim, and a
        // `-<level>` it does NOT find must be recognisable as unsupported rather than a typo.
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "m1",
            &[ReasoningLevel::Low, ReasoningLevel::High],
            &[InputModality::Text],
            &[],
        );
        let ids = listed_ids(&snapshot);

        assert!(ids.contains(&"p1/m1".to_string()));
        assert!(ids.contains(&"p1/m1-low".to_string()));
        assert!(ids.contains(&"p1/m1-high".to_string()));
        // Offered by the ladder but not by this model.
        for absent in ["p1/m1-minimal", "p1/m1-medium", "p1/m1-max", "p1/m1-ultra"] {
            assert!(!ids.contains(&absent.to_string()), "{absent} was listed");
        }
    }

    #[test]
    fn the_model_listing_gives_a_modelless_name_exactly_one_entry() {
        // No levels means the suffix is not part of this model's vocabulary, so advertising
        // `p1/m1-high` would offer a name that resolves to nothing.
        let snapshot = snapshot_with(Dialect::OpenAi, "m1", &[], &[InputModality::Text], &[]);
        let ids = listed_ids(&snapshot);
        assert_eq!(ids, vec!["p1/m1".to_string()]);
    }

    #[test]
    fn the_model_listing_does_not_qualify_a_model_whose_id_ends_in_a_level_word() {
        // `gemini-3.8-flash-low` already is a tier; `...-low-high` would name nothing because the
        // exact-id rule wins. Listing it would be listing a name that cannot resolve.
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "gemini-3.8-flash-low",
            &[ReasoningLevel::Low, ReasoningLevel::High],
            &[InputModality::Text],
            &[],
        );
        let ids = listed_ids(&snapshot);
        assert_eq!(ids, vec!["p1/gemini-3.8-flash-low".to_string()]);
    }

    #[test]
    fn a_listed_name_is_a_name_the_snapshot_actually_resolves() {
        // The property that makes the listing worth trusting: every id it publishes resolves, and
        // resolves at the level the entry claims.
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "m1",
            &[
                ReasoningLevel::Low,
                ReasoningLevel::High,
                ReasoningLevel::Max,
            ],
            &[InputModality::Text, InputModality::Image],
            &[("c1", "coding")],
        );
        for entry in model_listing(&snapshot) {
            let id = entry["id"].as_str().unwrap();
            match snapshot.resolve(id) {
                Resolution::Direct { step, level, .. } => {
                    if let Some(named) = entry["manarouter"]["reasoning_level"].as_str() {
                        let expected =
                            ReasoningLevel::parse(named).expect("a canonical level spelling");
                        assert_eq!(level, expected.rank(), "{id} resolved at the wrong level");
                    }
                    let suffix = suffix_of(&entry);
                    if !suffix.is_empty() {
                        assert!(!step.model.ends_with(&suffix), "{id}");
                    }
                }
                Resolution::Combo { .. } => {
                    assert_eq!(entry["manarouter"]["kind"], "combo", "{id}");
                }
                Resolution::NotFound { reason } => {
                    panic!("the listing published {id}, which does not resolve: {reason}")
                }
            }
        }
    }

    /// The `-<level>` suffix an entry was listed under, or the empty string for a base name.
    fn suffix_of(entry: &serde_json::Value) -> String {
        entry["manarouter"]["reasoning_level"]
            .as_str()
            .map(|level| format!("-{level}"))
            .unwrap_or_default()
    }

    #[test]
    fn the_model_listing_reports_each_models_levels_and_modalities() {
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "m1",
            &[ReasoningLevel::Low, ReasoningLevel::High],
            &[
                InputModality::Text,
                InputModality::Image,
                InputModality::Pdf,
            ],
            &[],
        );
        let entry = model_listing(&snapshot)
            .into_iter()
            .find(|entry| entry["id"] == "p1/m1")
            .expect("the base name is listed");

        assert_eq!(entry["object"], "model");
        assert_eq!(entry["owned_by"], "p1");
        assert_eq!(entry["manarouter"]["kind"], "provider");
        assert_eq!(entry["manarouter"]["dialect"], Dialect::OpenAi.as_str());
        assert_eq!(
            entry["manarouter"]["reasoning_levels"],
            serde_json::json!(["low", "high"])
        );
        assert_eq!(
            entry["manarouter"]["input_modalities"],
            serde_json::json!(["text", "image", "pdf"])
        );
    }

    #[test]
    fn the_model_listing_lists_a_combo_plain_and_per_level() {
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "m1",
            &[ReasoningLevel::Low, ReasoningLevel::High],
            &[InputModality::Text],
            &[("c1", "coding")],
        );
        let ids = listed_ids(&snapshot);
        assert!(ids.contains(&"coding".to_string()));
        assert!(ids.contains(&"coding-low".to_string()));
        assert!(ids.contains(&"coding-high".to_string()));

        let entry = model_listing(&snapshot)
            .into_iter()
            .find(|entry| entry["id"] == "coding-high")
            .unwrap();
        assert_eq!(entry["manarouter"]["kind"], "combo");
        assert_eq!(entry["manarouter"]["reasoning_level"], "high");
    }

    #[test]
    fn a_multi_step_combo_lists_each_level_once() {
        // Two steps offering overlapping levels must not produce `coding-high` twice. A duplicate
        // is not merely untidy: a client that builds a picker from this list shows the same name
        // twice, and one that keys on id sees the list contradict itself.
        let mut config = mr_core::config::Config::default();
        let mut provider = mr_core::model::Provider::new("p1", Dialect::OpenAi, "https://p1/v1");
        provider.models = vec![
            {
                let mut m = mr_core::model::ModelEntry {
                    id: "m1".into(),
                    ..dm()
                };
                m.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::High];
                m
            },
            {
                let mut m = mr_core::model::ModelEntry {
                    id: "m2".into(),
                    ..dm()
                };
                // `xhigh`, not `max`: the openai dialect rewrites `max` to `xhigh`, so the compiler
                // never offers `max` and the listing must not name a tier it cannot send.
                m.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::XHigh];
                m
            },
        ];
        provider.accounts = vec![mr_core::model::Account::new("a")];
        config.providers = vec![provider];
        let mut combo =
            mr_core::model::Combo::new("coding", mr_core::model::ComboStep::new("p1", "m1"));
        combo.steps.push(mr_core::model::ComboStep::new("p1", "m2"));
        combo.id = mr_core::ids::ComboId::from_raw("c1");
        config.combos = vec![combo];

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let ids = listed_ids(&snapshot);
        let mut sorted = ids.clone();
        sorted.sort();
        let mut deduped = sorted.clone();
        deduped.dedup();
        assert_eq!(
            sorted, deduped,
            "the listing contains a duplicate id: {ids:?}"
        );
        // The union of both steps, which is what a combo pinned to that tier can actually run.
        assert!(ids.contains(&"coding-low".to_string()));
        assert!(ids.contains(&"coding-high".to_string()));
        assert!(ids.contains(&"coding-xhigh".to_string()));
        assert!(!ids.contains(&"coding-medium".to_string()));
    }

    #[test]
    fn a_combos_levels_are_its_own_not_its_step_models() {
        // A combo whose step model id ends in a level word still supports pinning the *combo* to a
        // tier: the suffix attaches to `coding`, not to `gemini-3.8-flash-low`. Reading the step's
        // name here suppressed the combo's whole level list.
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "gemini-3.8-flash-low",
            &[
                ReasoningLevel::None,
                ReasoningLevel::Low,
                ReasoningLevel::High,
            ],
            &[InputModality::Text],
            &[("c1", "coding")],
        );
        let ids = listed_ids(&snapshot);
        assert!(
            ids.contains(&"coding-low".to_string()),
            "the combo is pinnable even though its step model is a tier: {ids:?}"
        );
        assert!(ids.contains(&"coding-high".to_string()));
        // The *model* is still listed once, because its own id is already a tier.
        assert!(ids.contains(&"p1/gemini-3.8-flash-low".to_string()));
        assert!(!ids.contains(&"p1/gemini-3.8-flash-low-high".to_string()));
    }

    #[test]
    fn a_combo_whose_own_name_ends_in_a_level_word_lists_no_variants() {
        let snapshot = snapshot_with(
            Dialect::OpenAi,
            "m1",
            &[ReasoningLevel::Low, ReasoningLevel::High],
            &[InputModality::Text],
            &[("c1", "fast-low")],
        );
        let ids = listed_ids(&snapshot);
        assert!(ids.contains(&"fast-low".to_string()));
        assert!(
            !ids.contains(&"fast-low-high".to_string()),
            "the combo's own name would swallow the suffix: {ids:?}"
        );
    }

    #[test]
    fn a_model_with_no_levels_compiles_with_an_empty_level_mask() {
        let snapshot = snapshot_with(Dialect::OpenAi, "m1", &[], &[InputModality::Text], &[]);
        let step = &snapshot.providers()[0].steps[0];
        assert_eq!(step.levels, 0);
        assert!(!step.supports_level(ReasoningLevel::High));
    }

    #[test]
    fn a_bearer_header_yields_the_key() {
        let h = headers(&[("authorization", "Bearer mr-abc")]);
        assert_eq!(presented_key(&h).as_deref(), Some("mr-abc"));
    }

    #[test]
    fn an_x_api_key_header_yields_the_key() {
        let h = headers(&[("x-api-key", "mr-abc")]);
        assert_eq!(presented_key(&h).as_deref(), Some("mr-abc"));
    }

    #[test]
    fn a_lowercase_bearer_prefix_is_accepted() {
        // Some clients lowercase the scheme, and HTTP treats it as case-insensitive. It is the same
        // credential.
        let h = headers(&[("authorization", "bearer mr-abc")]);
        assert_eq!(presented_key(&h).as_deref(), Some("mr-abc"));
    }

    #[test]
    fn no_credential_header_yields_nothing() {
        assert_eq!(presented_key(&HeaderMap::new()), None);
    }

    #[test]
    fn an_empty_credential_is_treated_as_absent() {
        // Reporting "no key presented" is a better message than "invalid key" for a client bug the
        // operator can see immediately.
        assert_eq!(presented_key(&headers(&[("x-api-key", "")])), None);
        // `HeaderValue` strips the trailing space, so this arrives as the bare word `Bearer`. It must
        // not be returned as somebody's API key.
        assert_eq!(
            presented_key(&headers(&[("authorization", "Bearer ")])),
            None
        );
        assert_eq!(
            presented_key(&headers(&[("authorization", "Bearer")])),
            None
        );
        assert_eq!(presented_key(&headers(&[("authorization", "   ")])), None);
    }

    #[test]
    fn x_api_key_wins_when_both_are_present() {
        let h = headers(&[
            ("x-api-key", "mr-from-x"),
            ("authorization", "Bearer mr-from-auth"),
        ]);
        assert_eq!(presented_key(&h).as_deref(), Some("mr-from-x"));
    }

    #[test]
    fn a_401_is_returned_for_an_unauthenticated_request() {
        let r = auth_error(&AuthOutcome::Missing, Dialect::OpenAi);
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        assert!(r.headers().contains_key("www-authenticate"));
    }

    /// A wait is reported in the units a person plans around.
    #[test]
    fn a_wait_is_reported_in_readable_units() {
        assert_eq!(humanize_wait(0), "moments");
        assert_eq!(humanize_wait(45), "45s");
        assert_eq!(humanize_wait(60), "1m");
        assert_eq!(humanize_wait(90), "1m 30s");
        assert_eq!(humanize_wait(3600), "1h");
        assert_eq!(humanize_wait(3844), "1h 4m");
        assert_eq!(humanize_wait(86_400), "24h");
    }

    #[test]
    fn every_dialect_has_a_default_upstream_path() {
        // A dialect with no path would build a URL ending at the base and send the request to the
        // host root, which fails as a provider 404 and reads as a wrong base URL. `Dialect::ALL` is
        // the source of truth rather than a list written out here, so a new dialect is covered the
        // moment it exists.
        for d in Dialect::ALL {
            let p = default_path(d, RouteKind::ChatCompletions);
            assert!(p.starts_with('/'), "{d:?} must have an absolute path");
            assert!(p.len() > 1, "{d:?} must not be the root");
        }
    }

    #[test]
    fn commandcode_ndjson_metadata_is_suppressed_and_content_is_translated() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::CommandCodeGo,
            upstream_model: "deepseek/deepseek-v4-flash",
            client_streaming: true,
            session_id: None,
        };
        let mut commandcode = mr_dialects::commandcode::Translator::new();
        let mut responses = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        let metadata = Bytes::from_static(b"{\"type\":\"start\"}\n");
        assert!(translate_stream_chunk(
            &ctx,
            &mut commandcode,
            &mut responses,
            &mut pending,
            &metadata
        )
        .is_empty());
        let token = Bytes::from_static(b"{\"type\":\"text-delta\",\"text\":\"OK\"}\n");
        let output =
            translate_stream_chunk(&ctx, &mut commandcode, &mut responses, &mut pending, &token);
        let output = output.into_iter().map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(output.len(), 1);
        let text = std::str::from_utf8(&output[0]).unwrap();
        assert!(text.starts_with("data: {"), "{text:?}");
        assert!(text.contains("chat.completion.chunk"), "{text:?}");
        assert!(text.contains("\"content\":\"OK\""), "{text:?}");
        let finish = Bytes::from_static(
            b"{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":2,\"outputTokens\":1}}\n",
        );
        let output = translate_stream_chunk(
            &ctx,
            &mut commandcode,
            &mut responses,
            &mut pending,
            &finish,
        );
        let text = output
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();
        assert!(text.contains("finish_reason"), "{text:?}");
        assert!(text.contains("[DONE]"), "{text:?}");
    }

    /// A Responses upstream's frame markers must not reach a chat client's stream.
    ///
    /// The live codex endpoint frames its stream with `event:` lines — `response.created`,
    /// `response.in_progress`, `response.output_item.added` — and only the payload under
    /// `response.output_text.delta` is a delta. Forwarding those names put a chat client in front of
    /// events in a dialect it does not speak: an `event:` line with no `data:` of its own, dispatched
    /// by the blank line that follows. A client that parses every event's data reads an empty string,
    /// and the whole turn dies as "malformed server-sent event JSON". The translated frames carry the
    /// client's own framing, so the upstream's is dropped.
    #[test]
    fn a_responses_upstream_does_not_leak_frame_names_into_a_chat_stream() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Codex,
            upstream_model: "gpt-6-luna",
            client_streaming: true,
            session_id: None,
        };
        mr_dialects::reset_stream_state();
        let mut translator = mr_dialects::commandcode::Translator::new();
        let mut responses = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        let chunk = Bytes::from_static(
            b"event: response.created\n\nevent: response.in_progress\n\nevent: \
response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hi\"}\n\n",
        );
        let out =
            translate_stream_chunk(&ctx, &mut translator, &mut responses, &mut pending, &chunk);
        let text = out
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();

        assert!(
            !text.contains("event:"),
            "a chat client must not see the upstream's frame names: {text:?}"
        );
        assert!(
            !text.contains("\n\n\n"),
            "no empty event may be dispatched: {text:?}"
        );
        assert_eq!(
            text.matches("data: ").count(),
            1,
            "one delta, one frame: {text:?}"
        );
        assert!(text.contains("\"content\":\"Hi\""), "{text:?}");
    }

    /// The same bytes pass through untouched when the client speaks the upstream's dialect.
    #[test]
    fn a_responses_client_keeps_the_upstreams_frame_markers() {
        let ctx = TranslateCtx {
            route: RouteKind::Responses,
            client_dialect: Dialect::Codex,
            upstream_dialect: Dialect::Codex,
            upstream_model: "gpt-6-luna",
            client_streaming: true,
            session_id: None,
        };
        let mut translator = mr_dialects::commandcode::Translator::new();
        let mut responses = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        let chunk = Bytes::from_static(
            b"event: response.created\ndata: {\"type\":\"response.created\"}\n\n",
        );
        let out =
            translate_stream_chunk(&ctx, &mut translator, &mut responses, &mut pending, &chunk);
        let text = out
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();
        assert!(text.contains("event: response.created"), "{text:?}");
        assert!(
            text.contains("\n\n"),
            "the separator is part of the framing: {text:?}"
        );
    }

    /// A translator that hands back a bare payload still becomes a frame on the wire.
    ///
    /// Copilot's stream is OpenAI SSE end to end, so its translator passes the payload through
    /// untouched — no `data:` field, no separator. Written straight out, the client would read a
    /// line that is not an SSE field at all and receive an empty answer with a 200 status.
    #[test]
    fn a_bare_payload_is_wrapped_into_a_data_frame() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Copilot,
            upstream_model: "gpt-6-luna",
            client_streaming: true,
            session_id: None,
        };
        let mut translator = mr_dialects::commandcode::Translator::new();
        let mut responses = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        let out = translate_stream_chunk(
            &ctx,
            &mut translator,
            &mut responses,
            &mut pending,
            &Bytes::from_static(b"data: {\"id\":\"x\",\"choices\":[]}\n\n"),
        );
        let text = out
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();
        assert_eq!(text, "data: {\"id\":\"x\",\"choices\":[]}\n\n");
    }

    /// A keep-alive comment is neither a frame name nor a payload, and every parser ignores it.
    #[test]
    fn a_keep_alive_comment_reaches_the_client_without_a_translation() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Codex,
            upstream_model: "gpt-6-luna",
            client_streaming: true,
            session_id: None,
        };
        let mut translator = mr_dialects::commandcode::Translator::new();
        let mut responses = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        let out = translate_stream_chunk(
            &ctx,
            &mut translator,
            &mut responses,
            &mut pending,
            &Bytes::from_static(b": keep-alive\n"),
        );
        let text = out
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();
        assert_eq!(text, ": keep-alive\n");
    }

    /// Two Codex answers in flight on the gateway at once must not share translation state.
    ///
    /// This is the relay's own entry point with two translators, interleaving chunks exactly as a busy
    /// runtime does. While the state lived in a `thread_local` the two streams wrote into one cell: the
    /// second answer's text arrived attributed to the first answer's id, and one of the two clients was
    /// left with a stream that never carried a `finish_reason`.
    #[test]
    fn two_codex_answers_in_flight_do_not_share_state() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Codex,
            upstream_model: "gpt-6-luna",
            client_streaming: true,
            session_id: None,
        };
        let (mut open_a, mut open_b) = (
            mr_dialects::responses::Translator::new(),
            mr_dialects::responses::Translator::new(),
        );
        let (mut cc_a, mut cc_b) = (
            mr_dialects::commandcode::Translator::new(),
            mr_dialects::commandcode::Translator::new(),
        );
        let (mut pending_a, mut pending_b) = (Vec::new(), Vec::new());
        let feed = |open: &mut mr_dialects::responses::Translator,
                    cc: &mut mr_dialects::commandcode::Translator,
                    pending: &mut Vec<u8>,
                    frame: &'static [u8]| {
            translate_stream_chunk(&ctx, cc, open, pending, &Bytes::from_static(frame))
                .into_iter()
                .map(Result::unwrap)
                .map(|b| String::from_utf8(b.to_vec()).unwrap())
                .collect::<String>()
        };

        let mut text_a = String::new();
        let mut text_b = String::new();
        text_a.push_str(&feed(
            &mut open_a,
            &mut cc_a,
            &mut pending_a,
            br#"data: {"type":"response.created","response":{"id":"resp_a","status":"in_progress","model":"gpt-6-luna"}}

"#,
        ));
        text_b.push_str(&feed(
            &mut open_b,
            &mut cc_b,
            &mut pending_b,
            br#"data: {"type":"response.created","response":{"id":"resp_b","status":"in_progress","model":"gpt-6-luna"}}

"#,
        ));
        // The two answers now interleave, one delta each.
        text_a.push_str(&feed(
            &mut open_a,
            &mut cc_a,
            &mut pending_a,
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"A1\"}\n\n",
        ));
        text_b.push_str(&feed(
            &mut open_b,
            &mut cc_b,
            &mut pending_b,
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"B1\"}\n\n",
        ));
        text_a.push_str(&feed(
            &mut open_a,
            &mut cc_a,
            &mut pending_a,
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"A2\"}\n\n",
        ));
        text_b.push_str(&feed(
            &mut open_b,
            &mut cc_b,
            &mut pending_b,
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_b\",\"status\":\"completed\",\"model\":\"gpt-6-luna\"}}\n\n",
        ));
        text_a.push_str(&feed(
            &mut open_a,
            &mut cc_a,
            &mut pending_a,
            b"data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_a\",\"status\":\"completed\",\"model\":\"gpt-6-luna\"}}\n\n",
        ));

        assert!(
            text_a.contains("\"A1\"") && text_a.contains("\"A2\""),
            "{text_a:?}"
        );
        assert!(text_b.contains("\"B1\""), "{text_b:?}");
        assert!(
            !text_a.contains("B1"),
            "A must not carry B's text: {text_a:?}"
        );
        assert!(
            !text_b.contains("A1"),
            "B must not carry A's text: {text_b:?}"
        );
        assert!(
            text_a.contains("resp_a") && !text_a.contains("resp_b"),
            "{text_a:?}"
        );
        assert!(
            text_b.contains("resp_b") && !text_b.contains("resp_a"),
            "{text_b:?}"
        );
        assert_eq!(
            text_a.matches("finish_reason\":\"stop").count(),
            1,
            "each stream closes exactly once: {text_a:?}"
        );
        assert!(
            text_b.contains("finish_reason\":\"stop"),
            "B is not left open: {text_b:?}"
        );
        assert!(
            text_a.trim_end().ends_with("[DONE]"),
            "the client is told the stream is over: {text_a:?}"
        );
    }

    /// The probe must not cap the answer, because a cap is a ceiling on thinking *and* text together.
    ///
    /// The panel's "test this model" reports what the probe returns, so a cap small enough to starve a
    /// reasoning model showed up as a successful test with an empty answer. What the probe asks for is
    /// therefore pinned here: a plain non-streaming completion with no `max_tokens`, so the route gets
    /// the same budget a real client would.
    #[test]
    fn the_probe_asks_for_an_answer_without_dictating_its_length() {
        let payload = probe_payload("antigravity/gemini-3.8-flash-tiered", "reply with \"yes\"");
        assert_eq!(payload["model"], "antigravity/gemini-3.8-flash-tiered");
        assert_eq!(payload["stream"], false);
        assert_eq!(payload["messages"][0]["content"], "reply with \"yes\"");
        for cap in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
            assert!(
                payload.get(cap).is_none(),
                "the probe must not send {cap}: {payload}"
            );
        }
    }
    /// The upstream keys its prompt cache on `prompt_cache_key`, and the only honest source of one
    /// is the client's own session marker. Reading it off a header keeps the fast path free: no body
    /// is parsed just to find a cache key.
    #[test]
    fn a_client_session_header_becomes_the_prompt_cache_key() {
        let mut headers = HeaderMap::new();
        assert!(
            client_session_hint(&headers).is_none(),
            "a client that declares no session asks for no affinity"
        );

        headers.insert("x-session-id", HeaderValue::from_static("  conv-1  "));
        assert_eq!(
            client_session_hint(&headers),
            Some("conv-1"),
            "the generic spelling is read, and whitespace trimmed"
        );

        // The official CLI's own spelling outranks the generic one when a client sends both.
        headers.insert("session-id", HeaderValue::from_static("official"));
        assert_eq!(client_session_hint(&headers), Some("official"));

        let mut blank = HeaderMap::new();
        blank.insert("session-id", HeaderValue::from_static("   "));
        assert!(
            client_session_hint(&blank).is_none(),
            "a blank id would put every conversation in one cache bucket"
        );

        let mut oversized = HeaderMap::new();
        oversized.insert(
            "session-id",
            HeaderValue::from_str(&"x".repeat(129)).expect("a long header is still a header"),
        );
        assert!(
            client_session_hint(&oversized).is_none(),
            "an arbitrary-length string is not a cache key"
        );
    }

    /// A Codex stream that stops before its terminal event is closed honestly at end of body.
    ///
    /// The upstream dying mid-answer is the ordinary case this has to survive: the client must learn
    /// that the answer was cut short, and the last partial line in the buffer — a `data:` payload
    /// whose newline never arrived — must not be thrown away.
    #[test]
    fn a_codex_stream_that_stops_early_is_closed_at_the_end_of_the_body() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Codex,
            upstream_model: "gpt-6-luna",
            client_streaming: true,
            session_id: None,
        };
        let mut cc = mr_dialects::commandcode::Translator::new();
        let mut open = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        // The tail of the body has no trailing newline, which is how a cut connection ends.
        translate_stream_chunk(
            &ctx,
            &mut cc,
            &mut open,
            &mut pending,
            &Bytes::from_static(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}",
            ),
        );
        let out = flush_stream_chunk(&ctx, &mut cc, &mut open, &mut pending, false);
        let text = out
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();

        assert!(
            text.contains("Hel"),
            "the partial tail is not lost: {text:?}"
        );
        assert!(text.contains("stream_truncated"), "{text:?}");
        assert!(
            !text.contains("finish_reason"),
            "a truncated answer must not claim it finished: {text:?}"
        );
        assert!(
            text.trim_end().ends_with("[DONE]"),
            "the client is told the stream is over: {text:?}"
        );
        assert!(pending.is_empty(), "the buffer is drained");
        // A second flush, as the body ending twice would do, says nothing again.
        assert!(flush_stream_chunk(&ctx, &mut cc, &mut open, &mut pending, false).is_empty());
    }

    /// A Gemini body that ends cleanly without a finish reason is still closed as an answer.
    ///
    /// Antigravity answers a retired model with a notice carried as ordinary candidate text and no
    /// `finishReason`. The relay used to forward that text and end the body, which a strict Chat
    /// client reports as `Stream ended without finish_reason` — the text never reaches the reader.
    /// End of body is the end of the answer, so it gets a terminal chunk and `[DONE]`.
    #[test]
    fn a_gemini_stream_without_a_finish_reason_is_closed_at_end_of_body() {
        for upstream_dialect in [
            Dialect::Google,
            Dialect::Antigravity,
            Dialect::GoogleAiStudio,
        ] {
            let ctx = TranslateCtx {
                route: RouteKind::ChatCompletions,
                client_dialect: Dialect::OpenAi,
                upstream_dialect,
                upstream_model: "antigravity/gemini-3.8-flash-tiered",
                client_streaming: true,
                session_id: None,
            };
            mr_dialects::reset_stream_state();
            let mut cc = mr_dialects::commandcode::Translator::new();
            let mut open = mr_dialects::responses::Translator::new();
            let mut pending = Vec::new();
            translate_stream_chunk(
                &ctx,
                &mut cc,
                &mut open,
                &mut pending,
                &Bytes::from_static(
                    b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"notice\"}]}}]}",
                ),
            );
            let text = flush_stream_chunk(&ctx, &mut cc, &mut open, &mut pending, false)
                .into_iter()
                .map(Result::unwrap)
                .map(|b| String::from_utf8(b.to_vec()).unwrap())
                .collect::<String>();

            assert!(
                text.contains("notice"),
                "{upstream_dialect:?}: the delivered text survives: {text:?}"
            );
            assert!(
                text.contains(r#""finish_reason":"stop""#),
                "{upstream_dialect:?}: {text:?}"
            );
            assert!(
                text.trim_end().ends_with("[DONE]"),
                "{upstream_dialect:?}: {text:?}"
            );
            assert!(
                flush_stream_chunk(&ctx, &mut cc, &mut open, &mut pending, false).is_empty(),
                "{upstream_dialect:?}: closing twice says nothing the second time"
            );
        }
    }

    /// A Gemini body that breaks is not dressed up as a completed answer.
    #[test]
    fn a_broken_gemini_stream_is_not_closed_as_complete() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::Google,
            upstream_model: "gemini-2.0-flash",
            client_streaming: true,
            session_id: None,
        };
        mr_dialects::reset_stream_state();
        let mut cc = mr_dialects::commandcode::Translator::new();
        let mut open = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        translate_stream_chunk(
            &ctx,
            &mut cc,
            &mut open,
            &mut pending,
            &Bytes::from_static(
                b"data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"half\"}]}}]}",
            ),
        );
        let text = flush_stream_chunk(&ctx, &mut cc, &mut open, &mut pending, true)
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();

        assert!(text.contains("half"), "what arrived is kept: {text:?}");
        assert!(
            !text.contains(r#""finish_reason":"stop""#),
            "no invented completion: {text:?}"
        );
        assert!(!text.contains("[DONE]"), "no completion sentinel: {text:?}");
    }

    /// Two Command Code answers in flight on the gateway at once must not share translation state.
    ///
    /// This drives the relay's own entry point — [`translate_stream_chunk`], the function the
    /// response-body `scan` calls — with two independent [`mr_dialects::commandcode::Translator`]s,
    /// interleaving the byte chunks exactly as a busy runtime does. Before the translator was owned
    /// per stream it was a `thread_local`, so these two would have written into one cell: the second
    /// stream's argument fragments landed in the first stream's tool call and the client received a
    /// nameless, id-less `tool-call` block that it could neither dispatch nor match, which is what
    /// killed the turn with `requires toolCallId matching its tool source`.
    #[test]
    fn two_concurrent_commandcode_streams_do_not_share_translation_state() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::CommandCodeGo,
            upstream_model: "deepseek/deepseek-v4-flash",
            client_streaming: true,
            session_id: None,
        };
        let mut a = mr_dialects::commandcode::Translator::new();
        let mut b = mr_dialects::commandcode::Translator::new();
        let (mut pa, mut pb) = (Vec::new(), Vec::new());

        let run = |t: &mut mr_dialects::commandcode::Translator,
                   pending: &mut Vec<u8>,
                   frame: &'static str|
         -> String {
            let mut responses = mr_dialects::responses::Translator::new();
            let out = translate_stream_chunk(
                &ctx,
                t,
                &mut responses,
                pending,
                &Bytes::from_static(frame.as_bytes()),
            );
            out.into_iter()
                .map(Result::unwrap)
                .map(|b| String::from_utf8(b.to_vec()).unwrap())
                .collect()
        };

        // Open a call on each stream, then interleave the opening and the fragments.
        let a_open = run(
            &mut a,
            &mut pa,
            "{\"type\":\"tool-input-start\",\"id\":\"call_A\",\"toolName\":\"alpha\"}\n",
        );
        let b_open = run(
            &mut b,
            &mut pb,
            "{\"type\":\"tool-input-start\",\"id\":\"call_B\",\"toolName\":\"beta\"}\n",
        );
        let a_frag = run(
            &mut a,
            &mut pa,
            "{\"type\":\"tool-input-delta\",\"id\":\"call_A\",\"delta\":\"A-ARGS\"}\n",
        );
        let b_frag = run(
            &mut b,
            &mut pb,
            "{\"type\":\"tool-input-delta\",\"id\":\"call_B\",\"delta\":\"B-ARGS\"}\n",
        );

        // Each open names its own call, and neither fragment carries the other's name.
        assert!(
            a_open.contains("call_A") && a_open.contains("alpha"),
            "{a_open:?}"
        );
        assert!(
            b_open.contains("call_B") && b_open.contains("beta"),
            "{b_open:?}"
        );
        assert!(
            !a_open.contains("call_B"),
            "A must not see B's call: {a_open:?}"
        );
        assert!(
            !b_open.contains("call_A"),
            "B must not see A's call: {b_open:?}"
        );
        // A fragment delta never repeats an id or a name, and never invents an empty one.
        for frag in [&a_frag, &b_frag] {
            assert!(
                !frag.contains("\"id\":\"\""),
                "no empty id may be emitted: {frag:?}"
            );
            assert!(
                !frag.contains("\"name\":\"\""),
                "no empty name may be emitted: {frag:?}"
            );
        }

        // Each stream folds its own arguments into its own single call.
        let a_end = run(
            &mut a,
            &mut pa,
            "{\"type\":\"finish\",\"finishReason\":\"tool-calls\"}\n",
        );
        let b_end = run(
            &mut b,
            &mut pb,
            "{\"type\":\"finish\",\"finishReason\":\"tool-calls\"}\n",
        );
        assert!(a_end.contains("[DONE]"), "{a_end:?}");
        assert!(b_end.contains("[DONE]"), "{b_end:?}");

        // Nothing either stream emitted may contain an empty tool-call id or name anywhere.
        for (label, all) in [
            ("A", [a_open, a_frag, a_end].concat()),
            ("B", [b_open, b_frag, b_end].concat()),
        ] {
            assert!(
                !all.contains("\"id\":\"\""),
                "{label} emitted an empty id: {all}"
            );
            assert!(
                !all.contains("\"name\":\"\""),
                "{label} emitted an empty name: {all}"
            );
        }
    }

    /// The translator the relay holds must forget a finished stream, so the next request that
    /// happens to reuse the same owned value cannot inherit a completed call.
    #[test]
    fn a_finished_commandcode_stream_leaves_nothing_for_the_next() {
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::CommandCodeGo,
            upstream_model: "deepseek/deepseek-v4-flash",
            client_streaming: true,
            session_id: None,
        };
        let mut t = mr_dialects::commandcode::Translator::new();
        let mut responses = mr_dialects::responses::Translator::new();
        let mut pending = Vec::new();
        for frame in [
            "{\"type\":\"tool-input-start\",\"id\":\"c1\",\"toolName\":\"f\"}\n",
            "{\"type\":\"finish\",\"finishReason\":\"tool-calls\"}\n",
        ] {
            let _ = translate_stream_chunk(
                &ctx,
                &mut t,
                &mut responses,
                &mut pending,
                &Bytes::from_static(frame.as_bytes()),
            );
        }
        assert!(t.is_finished(), "the finish was seen");

        // A fresh translator starts empty: no inherited call, no inherited id.
        let fresh = mr_dialects::commandcode::Translator::new();
        assert!(!fresh.is_finished());
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_does_not_double() {
        let s = step(Dialect::OpenAi, "https://api.example.com/");
        assert_eq!(
            build_url(
                &s,
                UpstreamPath::Dialect(default_path(Dialect::OpenAi, RouteKind::ChatCompletions))
            )
            .as_deref(),
            Some("https://api.example.com/v1/chat/completions")
        );
    }

    #[test]
    fn an_empty_base_url_builds_no_url() {
        // The step is skipped rather than sent to a nonsense URL.
        let s = step(Dialect::OpenAi, "");
        assert_eq!(
            build_url(
                &s,
                UpstreamPath::Dialect(default_path(Dialect::OpenAi, RouteKind::ChatCompletions))
            ),
            None
        );
    }

    #[test]
    fn a_provider_route_override_wins_over_the_dialect_default() {
        // Uses the dialect's own constant rather than a hand-written copy of it. The earlier version
        // of this test wrote `"/alpha/generate"` with a leading slash while the constant is
        // `"alpha/generate"` without one, so the test asserted a URL the code never built and the
        // real one — `https://api.commandcode.aialpha/generate` — went unnoticed.
        let s = step(Dialect::CommandCodeGo, "https://api.commandcode.ai");
        let over = mr_dialects::commandcode::GENERATE_PATH.to_string();
        assert_eq!(
            build_url(&s, UpstreamPath::Operator(&over)).as_deref(),
            Some("https://api.commandcode.ai/alpha/generate")
        );
    }

    #[test]
    fn a_relative_route_override_gains_its_separator() {
        let s = step(Dialect::OpenAi, "https://example.com");
        let over = "v1/other".to_string();
        assert_eq!(
            build_url(&s, UpstreamPath::Operator(&over)).as_deref(),
            Some("https://example.com/v1/other")
        );
    }

    #[test]
    fn a_base_url_with_a_trailing_slash_does_not_double_it() {
        let s = step(Dialect::OpenAi, "https://example.com/");
        let over = "/v1/other".to_string();
        assert_eq!(
            build_url(&s, UpstreamPath::Operator(&over)).as_deref(),
            Some("https://example.com/v1/other")
        );
    }

    /// Where a path in the URL table comes from.
    #[derive(Clone, Copy, Debug)]
    enum PathSource {
        /// The dialect's own default path.
        DialectDefault,
        /// A path the translator produced for the upstream dialect.
        Translator(&'static str),
        /// An operator's explicit `route_overrides` path.
        Operator(&'static str),
    }

    impl PathSource {
        fn path(self, dialect: Dialect, route: RouteKind) -> UpstreamPath<'static> {
            match self {
                PathSource::DialectDefault => UpstreamPath::Dialect(default_path(dialect, route)),
                PathSource::Translator(path) => UpstreamPath::Dialect(path),
                PathSource::Operator(path) => UpstreamPath::Operator(path),
            }
        }
    }

    /// The version segment appears exactly once, whatever the base URL and the path carry.
    ///
    /// The base URLs are the ones the catalogue ships, and the paths are the ones the dialects and
    /// the translators produce, so this is the table that says which upstream is called where.
    #[test]
    fn the_version_segment_appears_exactly_once() {
        let cases: &[(Dialect, RouteKind, &str, PathSource, &str)] = &[
            // A base that already ends in the version, and a path that starts with it.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.openai.com/v1",
                PathSource::DialectDefault,
                "https://api.openai.com/v1/chat/completions",
            ),
            // A base with no version, and a path that carries it.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.deepseek.com",
                PathSource::DialectDefault,
                "https://api.deepseek.com/v1/chat/completions",
            ),
            // A base that already ends in the version, and a path that does not.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.openai.com/v1",
                PathSource::Translator("chat/completions"),
                "https://api.openai.com/v1/chat/completions",
            ),
            // A base with no version, and a path that does not: the version is added.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.deepseek.com",
                PathSource::Translator("chat/completions"),
                "https://api.deepseek.com/v1/chat/completions",
            ),
            // A base whose version is not its last segment: the version is still found, so the
            // path's own version is dropped rather than repeated.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.deepinfra.com/v1/openai",
                PathSource::DialectDefault,
                "https://api.deepinfra.com/v1/openai/chat/completions",
            ),
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.deepinfra.com/v1/openai",
                PathSource::Translator("chat/completions"),
                "https://api.deepinfra.com/v1/openai/chat/completions",
            ),
            // A base whose version is a different version from the path's: the base's version is
            // the one the URL carries, and the path's gives way to it.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.z.ai/api/coding/paas/v4",
                PathSource::DialectDefault,
                "https://api.z.ai/api/coding/paas/v4/chat/completions",
            ),
            (
                Dialect::Anthropic,
                RouteKind::Messages,
                "https://api.anthropic.com/v1",
                PathSource::DialectDefault,
                "https://api.anthropic.com/v1/messages",
            ),
            (
                Dialect::Anthropic,
                RouteKind::Messages,
                "https://api.anthropic.com",
                PathSource::Translator("messages"),
                "https://api.anthropic.com/v1/messages",
            ),
            (
                Dialect::Google,
                RouteKind::ChatCompletions,
                "https://generativelanguage.googleapis.com/v1beta",
                PathSource::Translator("models/gemini-2.5-pro:generateContent"),
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
            ),
            // The AI Studio dialect is the plain Gemini shape, so a base that already carries
            // `v1beta` keeps it and the translator's model-bearing path is appended once.
            (
                Dialect::GoogleAiStudio,
                RouteKind::ChatCompletions,
                "https://generativelanguage.googleapis.com/v1beta",
                PathSource::Translator("models/gemini-2.5-pro:generateContent"),
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
            ),
            // Antigravity's version and its action share one segment, so the segment is left
            // alone rather than being taken for a version to strip or to add.
            (
                Dialect::Antigravity,
                RouteKind::ChatCompletions,
                "https://cloudcode-pa.googleapis.com",
                PathSource::Translator("v1internal:generateContent"),
                "https://cloudcode-pa.googleapis.com/v1internal:generateContent",
            ),
            (
                Dialect::Codex,
                RouteKind::ChatCompletions,
                "https://chatgpt.com/backend-api/codex",
                PathSource::DialectDefault,
                "https://chatgpt.com/backend-api/codex/responses",
            ),
            (
                Dialect::Copilot,
                RouteKind::ChatCompletions,
                "https://api.githubcopilot.com",
                PathSource::DialectDefault,
                "https://api.githubcopilot.com/chat/completions",
            ),
            (
                Dialect::CommandCodeGo,
                RouteKind::ChatCompletions,
                "https://api.commandcode.ai",
                PathSource::Translator("alpha/generate"),
                "https://api.commandcode.ai/alpha/generate",
            ),
            // An operator's path is theirs: it is used exactly as written, and no version is
            // added to it or taken from it.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.openai.com/v1",
                PathSource::Operator("ai/v1/chat/completions"),
                "https://api.openai.com/v1/ai/v1/chat/completions",
            ),
            // The Cloudflare entry: the base's "v4" is a version-like segment, and it must not
            // take anything off the operator's path.
            (
                Dialect::OpenAi,
                RouteKind::ChatCompletions,
                "https://api.cloudflare.com/client/v4/accounts/{account_id}",
                PathSource::Operator("ai/v1/chat/completions"),
                "https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1/chat/completions",
            ),
            // An operator's path that names the base's own version is de-duplicated, and one
            // that names a different version is left as the operator wrote it.
            (
                Dialect::OpenAi,
                RouteKind::Responses,
                "https://api.openai.com/v1",
                PathSource::Operator("v1/responses"),
                "https://api.openai.com/v1/responses",
            ),
            (
                Dialect::OpenAi,
                RouteKind::Responses,
                "https://api.openai.com/v1",
                PathSource::Operator("v2/responses"),
                "https://api.openai.com/v1/v2/responses",
            ),
        ];
        for (dialect, route, base, source, expected) in cases {
            let s = step(*dialect, base);
            assert_eq!(
                build_url(&s, source.path(*dialect, *route)).as_deref(),
                Some(*expected),
                "{base} with {dialect:?} and {source:?}"
            );
        }
    }

    #[test]
    fn a_google_path_without_a_version_gets_the_dialects_version() {
        // Google's translator writes the model into the path, so the path has no version of its
        // own; the base URL decides whether one is needed.
        let s = step(Dialect::Google, "https://generativelanguage.googleapis.com");
        assert_eq!(
            build_url(
                &s,
                UpstreamPath::Dialect("models/gemini-2.5-pro:generateContent")
            )
            .as_deref(),
            Some("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent")
        );
    }

    #[test]
    fn a_provider_override_wins_over_a_translator_path() {
        // The override is the operator's explicit statement of where this provider serves this
        // route; a path the translator derived from a dialect convention is less specific, so it
        // does not shadow the override.
        let over = "ai/v1/chat/completions".to_string();
        let s = step(Dialect::OpenAi, "https://api.openai.com/v1");
        let path = upstream_path(
            Dialect::OpenAi,
            RouteKind::ChatCompletions,
            Some("chat/completions"),
            Some(&over),
        );
        assert_eq!(path, UpstreamPath::Operator("ai/v1/chat/completions"));
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://api.openai.com/v1/ai/v1/chat/completions")
        );
    }

    #[test]
    fn a_translator_path_wins_when_the_provider_has_no_override() {
        let s = step(Dialect::OpenAi, "https://api.openai.com/v1");
        let path = upstream_path(
            Dialect::OpenAi,
            RouteKind::ChatCompletions,
            Some("chat/completions"),
            None,
        );
        assert_eq!(path, UpstreamPath::Dialect("chat/completions"));
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://api.openai.com/v1/chat/completions")
        );
    }

    #[test]
    fn the_dialect_default_applies_when_there_is_neither() {
        let s = step(Dialect::OpenAi, "https://api.deepseek.com");
        let path = upstream_path(Dialect::OpenAi, RouteKind::ChatCompletions, None, None);
        assert_eq!(
            path,
            UpstreamPath::Dialect(default_path(Dialect::OpenAi, RouteKind::ChatCompletions))
        );
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://api.deepseek.com/v1/chat/completions")
        );
    }

    #[test]
    fn copilot_ignores_a_provider_override() {
        // Copilot's base host comes from its OAuth token, and it has no upstream path of its own
        // for an override to replace, so the Copilot default is used.
        let over = "ai/v1/chat/completions".to_string();
        let s = step(Dialect::Copilot, "https://api.githubcopilot.com");
        let path = upstream_path(
            Dialect::Copilot,
            RouteKind::ChatCompletions,
            None,
            Some(&over),
        );
        assert_eq!(
            path,
            UpstreamPath::Dialect(default_path(Dialect::Copilot, RouteKind::ChatCompletions))
        );
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://api.githubcopilot.com/chat/completions")
        );
    }

    /// The OpenAI convention puts each of its routes on its own path.
    ///
    /// A dialect default that ignored the route sent every request to the chat-completions path,
    /// so an embeddings request reached the provider's chat endpoint.
    #[test]
    fn the_openai_dialects_default_path_is_the_one_for_the_route() {
        let cases = [
            (RouteKind::ChatCompletions, "/v1/chat/completions"),
            (RouteKind::Responses, "/v1/responses"),
            (RouteKind::Embeddings, "/v1/embeddings"),
            (RouteKind::AudioTranscription, "/v1/audio/transcriptions"),
            (RouteKind::AudioTranslation, "/v1/audio/translations"),
            (RouteKind::AudioSpeech, "/v1/audio/speech"),
            (RouteKind::ImagesGenerations, "/v1/images/generations"),
            (RouteKind::ImagesEdits, "/v1/images/edits"),
            (RouteKind::ImagesVariations, "/v1/images/variations"),
            (RouteKind::Moderations, "/v1/moderations"),
            (RouteKind::Models, "/v1/models"),
            (RouteKind::VideoCreate, "/v1/videos"),
            (RouteKind::VideoRetrieve, "/v1/videos"),
            (RouteKind::VideoContent, "/v1/videos"),
            (
                RouteKind::RealtimeClientSecrets,
                "/v1/realtime/client_secrets",
            ),
            (RouteKind::RealtimeCalls, "/v1/realtime/calls"),
            (RouteKind::Search, "/v1/search"),
            // An Anthropic client is translated, so the translator names the path; the default
            // is only the fallback.
            (RouteKind::Messages, "/v1/chat/completions"),
            (RouteKind::CountTokens, "/v1/chat/completions"),
        ];
        for (route, expected) in cases {
            assert_eq!(default_path(Dialect::OpenAi, route), expected, "{route:?}");
        }
    }

    #[test]
    fn the_anthropic_dialects_default_path_is_the_one_for_the_route() {
        assert_eq!(
            default_path(Dialect::Anthropic, RouteKind::Messages),
            "/v1/messages"
        );
        assert_eq!(
            default_path(Dialect::Anthropic, RouteKind::CountTokens),
            "/v1/messages/count_tokens"
        );
        // Every other route falls back to the messages path.
        assert_eq!(
            default_path(Dialect::Anthropic, RouteKind::ChatCompletions),
            "/v1/messages"
        );
        assert_eq!(
            default_path(Dialect::Anthropic, RouteKind::Embeddings),
            "/v1/messages"
        );
    }

    #[test]
    fn the_other_dialects_default_path_does_not_depend_on_the_route() {
        // These dialects serve one route each; the translator always names their path.
        let cases = [
            (Dialect::Google, "/v1beta/models:generateContent"),
            (Dialect::GoogleAiStudio, "/v1beta/models:generateContent"),
            (Dialect::Antigravity, "/v1internal:generateContent"),
            (Dialect::Codex, "/responses"),
            (Dialect::Copilot, "/chat/completions"),
            (Dialect::CommandCodeGo, "/alpha/generate"),
        ];
        for (dialect, expected) in cases {
            assert_eq!(
                default_path(dialect, RouteKind::ChatCompletions),
                expected,
                "{dialect:?}"
            );
            assert_eq!(
                default_path(dialect, RouteKind::Embeddings),
                expected,
                "{dialect:?}"
            );
            assert_eq!(
                default_path(dialect, RouteKind::Responses),
                expected,
                "{dialect:?}"
            );
        }
    }

    #[test]
    fn a_provider_override_still_wins_over_the_route_aware_default() {
        // The route-aware default is the fallback; the operator's statement about one route is
        // still the last word.
        let over = "embeddings".to_string();
        let s = step(Dialect::OpenAi, "https://api.openai.com/v1");
        let path = upstream_path(Dialect::OpenAi, RouteKind::Embeddings, None, Some(&over));
        assert_eq!(path, UpstreamPath::Operator("embeddings"));
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://api.openai.com/v1/embeddings")
        );
    }

    #[test]
    fn the_embeddings_route_reaches_the_embeddings_path() {
        // The defect the route-aware default fixes: an embeddings request must not be sent to
        // the provider's chat-completions endpoint.
        let s = step(Dialect::OpenAi, "https://api.openai.com/v1");
        let path = upstream_path(Dialect::OpenAi, RouteKind::Embeddings, None, None);
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://api.openai.com/v1/embeddings")
        );
    }

    #[test]
    fn the_commandcode_translation_reaches_the_right_url() {
        // The whole chain in one assertion: an OpenAI client, a Command Code provider, the
        // translator's own path, and the URL the relay would actually fetch. Every other test here
        // checks one link; this is the one that would have caught the missing separator, because it
        // is the only one that takes the path from the translator rather than inventing it.
        let ctx = TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::CommandCodeGo,
            upstream_model: "claude-sonnet-4-5",
            client_streaming: false,
            session_id: None,
        };
        let translated = translate_request(&ctx, br#"{"model":"m","messages":[]}"#)
            .expect("an OpenAI body translates to Command Code")
            .expect("the dialects differ, so a body must be produced");
        assert!(translated.force_stream, "Command Code always streams");
        let path = translated.path_override.expect("a path is declared");
        let s = step(Dialect::CommandCodeGo, "https://api.commandcode.ai");
        assert_eq!(
            build_url(&s, UpstreamPath::Dialect(&path)).as_deref(),
            Some("https://api.commandcode.ai/alpha/generate")
        );
    }

    #[test]
    fn a_forced_stream_asks_the_upstream_for_an_event_stream() {
        // A non-streaming client calling an upstream that only speaks SSE still has to be sent
        // `accept: text/event-stream`; asking for JSON gets a JSON *error* back instead.
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"m"}"#);
        assert!(!head.wants_stream());
        let out = upstream_headers(
            &p,
            &step(Dialect::CommandCodeGo, "https://example.com"),
            None,
            &HeaderMap::new(),
            &head,
            true,
            None,
            &[],
        );
        assert_eq!(out.get("accept").unwrap(), "text/event-stream");
    }

    #[test]
    fn commandcode_is_told_which_client_is_calling() {
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"m"}"#);
        let out = upstream_headers(
            &p,
            &step(Dialect::CommandCodeGo, "https://example.com"),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        assert_eq!(
            out.get("x-command-code-version").unwrap(),
            mr_dialects::commandcode::COMMAND_CODE_VERSION
        );
        assert_eq!(
            out.get("x-cli-environment").unwrap(),
            mr_dialects::commandcode::CLI_ENVIRONMENT
        );
    }

    #[test]
    fn another_dialect_is_not_told_it_is_commandcode() {
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"m"}"#);
        let out = upstream_headers(
            &p,
            &step(Dialect::OpenAi, "https://example.com"),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        assert!(!out.contains_key("x-command-code-version"));
    }

    #[test]
    fn antigravity_inference_carries_the_resolved_client_version() {
        // Cloud Code refuses clients below a rolling minimum build, and the catalogue records a
        // literal version that ages out. The relay rewrites that one token on the way out, so a
        // saved provider config written months ago cannot pin the client to an old build.
        const RECORDED: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Antigravity/2.1.1 Chrome/142.0.7444.175 Electron/39.2.3 Safari/537.36";
        let mut p = provider_plan();
        p.headers
            .insert("user-agent".to_owned(), RECORDED.to_owned());
        let head = scan_request_head(br#"{"model":"m"}"#);
        let out = upstream_headers(
            &p,
            &step(Dialect::Antigravity, "https://example.com"),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        let user_agent = out.get("user-agent").unwrap().to_str().unwrap();
        assert!(user_agent.contains(&format!(
            "Antigravity/{}",
            mr_core::quota::antigravity::ide_version()
        )));
        assert!(!user_agent.contains("Antigravity/2.1.1"));

        // Only Antigravity gets the rewrite: another provider keeps the recorded header verbatim.
        let mut other = provider_plan();
        other.headers = p.headers.clone();
        let out = upstream_headers(
            &other,
            &step(Dialect::OpenAi, "https://example.com"),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        assert_eq!(out.get("user-agent").unwrap().to_str().unwrap(), RECORDED);
    }

    #[test]
    fn zcode_inference_carries_the_apps_attribution_headers() {
        // What the app attaches to a model request, and nothing else: the correlation ids, the
        // session marker its signing layer keys on, the Anthropic version, and the credential.
        let mut p = provider_plan();
        p.headers
            .insert("anthropic-version".to_owned(), "2023-06-01".to_owned());
        let head = scan_request_head(br#"{"model":"glm-4.6"}"#);
        let out = upstream_headers(
            &p,
            &step(
                Dialect::Zcode,
                "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1",
            ),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );

        let request_id = out
            .get("x-request-id")
            .expect("a model request carries x-request-id")
            .to_str()
            .unwrap()
            .to_owned();
        assert!(request_id.len() >= 32, "{request_id}");
        // One id, two names: the app reuses the same value for the request and its trace.
        assert_eq!(out.get("x-zcode-trace-id").unwrap(), request_id.as_str());
        assert_eq!(out.get("x-zcode-session-type").unwrap(), "main");
        // With no client marker the fresh id becomes the session so the header is always present.
        assert_eq!(out.get("x-session-id").unwrap(), request_id.as_str());
        assert_eq!(out.get("anthropic-version").unwrap(), "2023-06-01");
    }

    #[test]
    fn zcode_invents_no_identity_header_the_app_does_not_send() {
        // The recorded model request has no `User-Agent` override, no app version, no title and no
        // `X-Client-*` family; those ride its configuration requests. Sending them would be a
        // fingerprint of a request the app never makes.
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"glm-4.6"}"#);
        let out = upstream_headers(
            &p,
            &step(
                Dialect::Zcode,
                "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1",
            ),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        for name in [
            "user-agent",
            "x-zcode-app-version",
            "x-title",
            "x-platform",
            "x-os-name",
            "x-client-version",
            "x-client-sig",
            "x-client-ts",
            "x-client-nonce",
            "x-app-id",
            "x-client-pow",
        ] {
            assert!(!out.contains_key(name), "{name} must not be invented");
        }
        // And the attribution block is this dialect's alone.
        let out = upstream_headers(
            &p,
            &step(Dialect::Anthropic, "https://api.anthropic.com"),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        assert!(!out.contains_key("x-zcode-session-type"));
        assert!(!out.contains_key("x-request-id"));
    }

    #[test]
    fn zcode_takes_the_clients_session_marker_in_the_shape_its_endpoint_expects() {
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"glm-4.6"}"#);
        let with_session = |raw: &str| {
            let mut client = HeaderMap::new();
            client.insert("session-id", raw.parse().unwrap());
            upstream_headers(
                &p,
                &step(
                    Dialect::Zcode,
                    "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1",
                ),
                None,
                &client,
                &head,
                false,
                None,
                &[],
            )
            .get("x-session-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned()
        };

        // The app strips its own prefixes before the id reaches the wire.
        assert_eq!(with_session("sess_abc123"), "abc123");
        assert_eq!(with_session("subagent_agent_xyz789"), "xyz789");
        assert_eq!(with_session("  plain-id  "), "plain-id");
        // A marker that cannot be an id falls back to the fresh one rather than being forwarded.
        for unusable in ["", "   ", "sess_", "has space\tand tab"] {
            let fallback = with_session(unusable);
            assert!(fallback.len() >= 32, "{unusable:?} -> {fallback}");
        }
    }

    #[test]
    fn zcode_keeps_the_operators_anthropic_version() {
        // `or_insert`, not `insert`: an operator who pins a different revision keeps it.
        let mut p = provider_plan();
        p.headers
            .insert("anthropic-version".to_owned(), "2024-10-22".to_owned());
        let head = scan_request_head(br#"{"model":"glm-4.6"}"#);
        let out = upstream_headers(
            &p,
            &step(
                Dialect::Zcode,
                "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1",
            ),
            None,
            &HeaderMap::new(),
            &head,
            false,
            None,
            &[],
        );
        assert_eq!(out.get("anthropic-version").unwrap(), "2024-10-22");
    }

    #[test]
    fn zcode_builds_the_messages_url_the_app_sends() {
        // The base URL stops at `anthropic`; the dialect supplies `v1/messages`, and the join must
        // not double the version the base already ends short of.
        let s = step(
            Dialect::Zcode,
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1",
        );
        let path = upstream_path(Dialect::Zcode, RouteKind::Messages, None, None);
        assert_eq!(path, UpstreamPath::Dialect("/v1/messages"));
        assert_eq!(
            build_url(&s, path).as_deref(),
            Some("https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages")
        );
    }

    #[test]
    fn the_zcode_default_path_is_the_messages_path_for_every_route() {
        for route in [
            RouteKind::Messages,
            RouteKind::ChatCompletions,
            RouteKind::CountTokens,
        ] {
            let expected = match route {
                RouteKind::CountTokens => "/v1/messages/count_tokens",
                _ => "/v1/messages",
            };
            assert_eq!(default_path(Dialect::Zcode, route), expected, "{route:?}");
        }
    }

    #[test]
    fn a_translated_body_replaces_the_clients_content_type() {
        // Forwarding `multipart/form-data` next to a JSON body is a lie the upstream acts on.
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"m"}"#);
        let mut client = HeaderMap::new();
        client.insert(
            "content-type",
            "multipart/form-data; boundary=x".parse().unwrap(),
        );
        let out = upstream_headers(
            &p,
            &step(Dialect::CommandCodeGo, "https://example.com"),
            None,
            &client,
            &head,
            false,
            Some("application/json"),
            &[],
        );
        assert_eq!(out.get("content-type").unwrap(), "application/json");
    }

    #[test]
    fn an_untranslated_body_keeps_the_clients_content_type() {
        let p = provider_plan();
        let head = scan_request_head(br#"{"model":"m"}"#);
        let mut client = HeaderMap::new();
        client.insert(
            "content-type",
            "multipart/form-data; boundary=x".parse().unwrap(),
        );
        let out = upstream_headers(
            &p,
            &step(Dialect::OpenAi, "https://example.com"),
            None,
            &client,
            &head,
            false,
            None,
            &[],
        );
        assert_eq!(
            out.get("content-type").unwrap(),
            "multipart/form-data; boundary=x"
        );
    }

    #[test]
    fn every_registered_route_maps_to_its_own_kind() {
        // A new route that fell through to the fallback would be translated as Chat Completions.
        assert_eq!(
            route_kind_for("/v1/chat/completions"),
            RouteKind::ChatCompletions
        );
        assert_eq!(route_kind_for("/v1/responses"), RouteKind::Responses);
        assert_eq!(route_kind_for("/v1/embeddings"), RouteKind::Embeddings);
        assert_eq!(
            route_kind_for("/v1/audio/transcriptions"),
            RouteKind::AudioTranscription
        );
        assert_eq!(
            route_kind_for("/v1/audio/translations"),
            RouteKind::AudioTranslation
        );
        assert_eq!(route_kind_for("/v1/audio/speech"), RouteKind::AudioSpeech);
        assert_eq!(
            route_kind_for("/v1/images/generations"),
            RouteKind::ImagesGenerations
        );
        assert_eq!(route_kind_for("/v1/images/edits"), RouteKind::ImagesEdits);
        assert_eq!(
            route_kind_for("/v1/images/variations"),
            RouteKind::ImagesVariations
        );
        assert_eq!(route_kind_for("/v1/moderations"), RouteKind::Moderations);
        assert_eq!(route_kind_for("/v1/videos"), RouteKind::VideoCreate);
        // A video read carries the id in the path; the suffix decides between the metadata read and
        // the binary content read.
        assert_eq!(
            route_kind_for("/v1/videos/video_123"),
            RouteKind::VideoRetrieve
        );
        assert_eq!(
            route_kind_for("/v1/videos/video_123/content"),
            RouteKind::VideoContent
        );
        assert_eq!(
            route_kind_for("/v1/realtime/client_secrets"),
            RouteKind::RealtimeClientSecrets
        );
        assert_eq!(
            route_kind_for("/v1/realtime/calls"),
            RouteKind::RealtimeCalls
        );
        assert_eq!(route_kind_for("/v1/search"), RouteKind::Search);
        assert_eq!(route_kind_for("/v1/messages"), RouteKind::Messages);
        assert_eq!(
            route_kind_for("/v1/messages/count_tokens"),
            RouteKind::CountTokens
        );
    }

    #[test]
    fn bearer_auth_sets_the_authorization_header() {
        let mut h = HeaderMap::new();
        apply_auth(&mut h, &AuthKind::Bearer, Some("sk-x"));
        assert_eq!(h.get("authorization").unwrap(), "Bearer sk-x");
    }

    #[test]
    fn an_api_key_header_uses_the_configured_name() {
        let mut h = HeaderMap::new();
        apply_auth(
            &mut h,
            &AuthKind::ApiKeyHeader {
                header: "api-key".into(),
            },
            Some("sk-x"),
        );
        assert_eq!(h.get("api-key").unwrap(), "sk-x");
    }

    #[test]
    fn a_prefixed_header_auth_includes_its_prefix() {
        let mut h = HeaderMap::new();
        apply_auth(
            &mut h,
            &AuthKind::Header {
                name: "X-Auth".into(),
                prefix: "Token ".into(),
            },
            Some("sk-x"),
        );
        assert_eq!(h.get("x-auth").unwrap(), "Token sk-x");
    }

    #[test]
    fn no_credential_means_no_auth_header_rather_than_an_empty_one() {
        let mut h = HeaderMap::new();
        apply_auth(&mut h, &AuthKind::Bearer, None);
        assert!(h.is_empty());
    }

    #[test]
    fn modal_auth_splits_token_id_and_secret_headers() {
        let mut h = HeaderMap::new();
        apply_auth(&mut h, &AuthKind::Modal, Some("wk-1234.ws-5678"));
        assert_eq!(h.get("modal-key").unwrap(), "wk-1234");
        assert_eq!(h.get("modal-secret").unwrap(), "ws-5678");
        assert!(!h.contains_key("authorization"));
    }

    #[test]
    fn malformed_modal_auth_is_never_sent_as_headers() {
        let mut h = HeaderMap::new();
        apply_auth(&mut h, &AuthKind::Modal, Some("wk-1234"));
        assert!(h.is_empty());
    }

    #[test]
    fn query_param_auth_is_never_applied() {
        // A credential in a URL ends up in provider logs, intermediary logs and browser history.
        let mut h = HeaderMap::new();
        apply_auth(
            &mut h,
            &AuthKind::QueryParam {
                param: "key".into(),
            },
            Some("sk-x"),
        );
        assert!(h.is_empty(), "a credential must never be placed in a URL");
    }

    #[test]
    fn an_oauth_credential_is_sent_as_a_bearer_token() {
        // Whoever refreshed it, the token is presented as a bearer header.
        let mut h = HeaderMap::new();
        apply_auth(
            &mut h,
            &AuthKind::OAuth {
                platform: "openai-codex".into(),
            },
            Some("access-token"),
        );
        assert_eq!(h.get("authorization").unwrap(), "Bearer access-token");
    }

    #[test]
    fn an_invalid_provider_header_is_skipped_rather_than_failing_the_request() {
        // A typo in a provider's configuration should log, not take the provider down.
        let mut p = provider_plan();
        p.headers.insert("good".into(), "fine".into());
        p.headers.insert("bad header".into(), "value".into());
        let s = step(Dialect::OpenAi, "https://example.com");
        let head = scan_request_head(br#"{"model":"m"}"#);
        let out = upstream_headers(&p, &s, None, &HeaderMap::new(), &head, false, None, &[]);
        assert!(out.contains_key("good"));
        assert_eq!(out.len(), 1, "the malformed header must not be forwarded");
    }

    #[test]
    fn retry_after_seconds_are_read() {
        let h = headers(&[("retry-after", "47")]);
        assert_eq!(retry_after_ms(&h), Some(47_000));
    }

    #[test]
    fn an_absurd_retry_after_is_capped() {
        // A bogus huge value must not park an account for years.
        let h = headers(&[("retry-after", "999999999")]);
        assert_eq!(retry_after_ms(&h), Some(86_400_000));
    }

    #[test]
    fn the_http_date_form_of_retry_after_is_not_guessed_at() {
        // Reading the "21" out of "Wed, 21 Oct 2026" as seconds is how a cooldown ends up measured in
        // decades. The configured default is a better answer than a wrong measurement.
        let h = headers(&[("retry-after", "Wed, 21 Oct 2026 07:28:00 GMT")]);
        assert_eq!(retry_after_ms(&h), None);
    }

    #[test]
    fn an_absent_retry_after_is_none() {
        assert_eq!(retry_after_ms(&HeaderMap::new()), None);
    }
    #[test]
    fn a_codex_usage_limit_is_a_spent_window_and_its_reset_is_honoured() {
        // The exact body Codex returns, captured from a free account whose window was spent. Before
        // this, the 429 was classified as a throttle: the account came back after a minute only to
        // be refused again, while a sibling account with quota sat unused.
        let body = br#"{"error":{"type":"usage_limit_reached","message":"The usage limit has been reached","plan_type":"free","resets_at":1795016770,"limit_window_minutes":43200,"resets_in_seconds":1233929}}"#;
        let refusal = exhausted_by_body(body).expect("a spent window");
        assert_eq!(refusal.reason, select::CooldownReason::QuotaExhausted);
        // The body says ~14 days; the account is parked for the cap of one day instead. Long enough
        // that it stops poisoning requests, short enough that a wrong or stale instant cannot hide a
        // recovered account for a fortnight. Retrying once a day costs one refused request.
        assert_eq!(refusal.wait_ms, Some(86_400_000));
    }

    #[test]
    fn an_oversized_prefill_is_a_capacity_refusal_and_its_hint_is_used() {
        // The exact body OpenCode's gateway returns for a prompt past its prefill cap, captured
        // from a 20 000-word request. It is a 429 with `type: server_error`, so the status line
        // reads as a throttle and the old code parked a healthy account for the full default
        // minute. The hint is the upstream saying when it can try again.
        let body = br#"{"error":{"type":"server_error","message":"Upstream request failed: [prefill_cap_refused] the cell is at capacity; retry shortly (after 30 s)"}}"#;
        let refusal = exhausted_by_body(body).expect("a capacity refusal");
        assert_eq!(refusal.reason, select::CooldownReason::RateLimited);
        assert_eq!(refusal.wait_ms, Some(30_000));
    }

    #[test]
    fn a_capacity_refusal_without_a_readable_hint_still_waits() {
        // Naming the marker without naming a delay must not mean "retry immediately": the
        // request was refused, so a cooldown of some length is always the honest answer.
        let body = br#"{"error":{"type":"server_error","message":"[prefill_cap_refused]"}}"#;
        let refusal = exhausted_by_body(body).expect("a capacity refusal");
        assert_eq!(refusal.wait_ms, Some(30_000));
    }

    #[test]
    fn a_capacity_hint_is_capped_like_every_other_wait() {
        let body = br#"[prefill_cap_refused] retry shortly (after 999999 s)"#;
        let refusal = exhausted_by_body(body).expect("a capacity refusal");
        assert_eq!(refusal.wait_ms, Some(86_400_000));
    }

    #[test]
    fn a_body_without_the_marker_is_not_a_capacity_refusal() {
        assert_eq!(
            exhausted_by_body(b"[cap_refused] the cell is at capacity; retry shortly (after 30 s)"),
            None
        );
        assert_eq!(exhausted_by_body(b"429 too many requests"), None);
    }

    #[test]
    fn an_ordinary_throttle_is_still_a_throttle() {
        // A 429 that only says "slow down" must not be read as a spent window: the difference is
        // whether waiting can help, and it decides how long the account is parked.
        assert_eq!(
            exhausted_by_body(br#"{"error":{"type":"rate_limit_error","message":"slow down"}}"#),
            None
        );
        assert_eq!(exhausted_by_body(b"not json at all"), None);
        assert_eq!(exhausted_by_body(b""), None);
        assert_eq!(exhausted_by_body(br#"{"error":{}}"#), None);
    }

    #[test]
    fn a_reset_stated_as_an_instant_is_converted_to_a_wait() {
        // OpenAI states an instant rather than a delay on some endpoints. Reading it as a delay
        // would park the account until 1970.
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        let body = format!(r#"{{"error":{{"type":"usage_limit_reached","resets_at":{future}}}}}"#);
        let refusal = exhausted_by_body(body.as_bytes()).expect("a spent window");
        let wait = refusal.wait_ms.expect("a wait");
        assert!(
            (500_000..=600_000).contains(&wait),
            "expected roughly ten minutes, got {wait} ms"
        );
    }

    #[test]
    fn a_window_longer_than_a_day_is_capped() {
        // The same cap `retry_after_ms` applies: a bogus value must not park an account for a year.
        let body = br#"{"error":{"type":"usage_limit_reached","resets_in_seconds":999999999}}"#;
        let refusal = exhausted_by_body(body).expect("a spent window");
        assert_eq!(refusal.wait_ms, Some(86_400_000));
    }

    #[test]
    fn a_reset_stated_in_words_is_as_good_as_a_field() {
        // The exact body a provider returned for a spent daily window, captured live. There is no
        // `resets_in_seconds` and no `type`/`code` on the error object, so it used to be read as an
        // ordinary 429: the account came back after the configured minute, was refused again, and
        // every request in the combo paid for that probe. The sentence says exactly when the account
        // recovers, and a day's quota deserves a day's cooldown.
        let body = br#"{"error":{"message":"You've used today's Free usage. It resets in 13h 24m. Add a usage pack or choose a plan to keep going: https://example.invalid/billing","type":"budget_exceeded","code":"budget_exceeded"}}"#;
        let refusal = exhausted_by_body(body).expect("a spent window");
        assert_eq!(refusal.reason, select::CooldownReason::QuotaExhausted);
        assert_eq!(refusal.wait_ms, Some(13 * 3_600_000 + 24 * 60_000));
    }

    #[test]
    fn a_reset_stated_as_a_period_is_read_once() {
        // "Resets every 24 hours" names the *period*, not a countdown, and the honest reading is
        // the same: the account is spent until the period turns over. Parsing it as a repeating
        // interval would be a lie the cooldown cannot express.
        let body = br#"{"error":{"message":"Daily quota reached (100%). Resets every 24 hours, or upgrade for more capacity.","limits":{"plan":"free","day":{"percent":100}}}}"#;
        let refusal = exhausted_by_body(body).expect("a spent window");
        assert_eq!(refusal.wait_ms, Some(86_400_000));
    }

    #[test]
    fn a_number_in_a_validation_message_is_not_a_wait() {
        // The guard that makes the prose read safe: without an anchor word, a number is a limit,
        // not a recovery instant. Reading it would park a healthy account for an hour.
        let body = br#"{"error":{"type":"invalid_request_error","message":"messages must not exceed 5 per request"}}"#;
        assert_eq!(exhausted_by_body(body), None);
        let body = br#"{"error":{"message":"slow down"}}"#;
        assert_eq!(exhausted_by_body(body), None);
    }

    #[test]
    fn a_bad_request_that_says_the_credit_is_gone_cools_the_account() {
        // Captured live from a provider whose credit balance was empty. The status is 400 and the
        // code is the generic `BAD_REQUEST`, so it used to be classified as the client's fault: no
        // cooldown, no account switch, one useless upstream round trip on every request.
        let body = br#"{"success":false,"error":{"code":"BAD_REQUEST","status":400,"message":"You have insufficient credits. Please top up your account to continue."}}"#;
        let refusal = exhausted_by_body(body).expect("a spent balance");
        assert_eq!(refusal.reason, select::CooldownReason::QuotaExhausted);
        // No reset was stated, and none is coming on its own: a balance that is gone needs a person,
        // so the park is the hour rather than the configured minute that would re-probe it.
        assert_eq!(refusal.wait_ms, Some(SPENT_BALANCE_PARK_MS));
    }

    #[test]
    fn a_locked_balance_under_detail_is_read_as_a_spent_account() {
        // The exact body fal returns once an account's balance is gone. The sentence sits at
        // `detail` (FastAPI's envelope), not under `error.message`, so the reader used to see no
        // message at all and the 403 was classified as a failed credential: the wrong reason, a
        // fifteen-minute park, and a retry that could never succeed while the money was the whole
        // story.
        let body = br#"{"detail":"User is locked. Reason: Exhausted balance. Top up your balance at fal.ai/dashboard/billing."}"#;
        let refusal = exhausted_by_body(body).expect("a spent account");
        assert_eq!(refusal.reason, select::CooldownReason::QuotaExhausted);
        assert_eq!(refusal.scope, None, "the whole account is locked");
        assert_eq!(refusal.wait_ms, Some(SPENT_BALANCE_PARK_MS));
    }

    #[test]
    fn a_plan_that_excludes_the_model_cools_only_that_model() {
        // Captured from a free account that had spent its monthly allowance for the Pro models. The
        // same credential still serves the Lite models, so parking it whole would remove capacity
        // the plan does allow, and a minute of park would walk it back into the same 429 on the
        // next request.
        let body = br#"{"error":{"message":"You've used this month's 1M tokens on Pro models, which is what the free plan includes. Lite and other models still work on your plan. Upgrade for unlimited Pro model use.","type":"rate_limit_error"}}"#;
        let refusal = exhausted_by_body(body).expect("a plan limit");
        assert_eq!(refusal.reason, select::CooldownReason::QuotaExhausted);
        assert_eq!(
            refusal.scope,
            Some(mr_core::model::CooldownScope::Model),
            "the plan limits this model, not the credential"
        );
        assert_eq!(refusal.wait_ms, Some(SPENT_BALANCE_PARK_MS));
    }

    #[test]
    fn a_plan_word_without_a_limit_word_is_not_a_plan_limit() {
        // The pair carries the meaning: a message that only mentions a plan says nothing about the
        // model, and one that only says "not included" says nothing about the plan.
        for body in [
            br#"{"error":{"message":"Your plan has been upgraded to Pro"}}"# as &[u8],
            br#"{"error":{"message":"the model is not included in this deployment"}}"#,
        ] {
            assert_eq!(
                exhausted_by_body(body),
                None,
                "{:?}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn a_refusal_sent_as_a_bare_error_string_is_still_read() {
        // A few gateways answer `{"error":"…"}` with no object around the message.
        let refusal = exhausted_by_body(br#"{"error":"insufficient credit: top up the account"}"#)
            .expect("a spent balance");
        assert_eq!(refusal.reason, select::CooldownReason::QuotaExhausted);
        assert_eq!(refusal.wait_ms, Some(SPENT_BALANCE_PARK_MS));
    }

    #[test]
    fn money_wording_from_other_providers_still_cools_the_account() {
        // Captured shapes from OpenAI-compatible providers: none of them uses the word "credit" the
        // way the first captured message did, and every one of them is about money rather than the
        // request. A 400 here is still an account problem, so it must move to another account.
        for message in [
            "Your account does not have enough credits to run this request",
            "You exceeded your current quota, please check your plan and billing details",
            "Insufficient funds on this account",
            "The account has reached its spend limit",
            "Your credit limit has been reached",
        ] {
            let body = format!(r#"{{"error":{{"message":"{message}"}}}}"#);
            assert!(exhausted_by_body(body.as_bytes()).is_some(), "{message}");
        }
    }

    #[test]
    fn a_validation_message_that_mentions_a_quota_is_not_a_spent_account() {
        // The vocabulary must not swallow ordinary request errors, or one malformed request would
        // cool every account the step can use.
        for message in [
            r#"{"error":{"type":"invalid_request_error","message":"max_tokens must be a positive integer"}}"#,
            r#"{"error":{"type":"invalid_request_error","message":"messages must not exceed 5 per request"}}"#,
            r#"{"error":{"type":"rate_limit_error","message":"too many requests, slow down"}}"#,
        ] {
            assert_eq!(exhausted_by_body(message.as_bytes()), None, "{message}");
        }
    }

    #[test]
    fn a_human_duration_is_read_in_the_forms_providers_write_it() {
        assert_eq!(
            stated_wait_ms("resets in 13h 24m"),
            Some(13 * 3_600_000 + 24 * 60_000)
        );
        assert_eq!(stated_wait_ms("Resets every 24 hours"), Some(86_400_000));
        assert_eq!(stated_wait_ms("try again in 45 minutes"), Some(2_700_000));
        assert_eq!(stated_wait_ms("retry in 30s"), Some(30_000));
        assert_eq!(stated_wait_ms("available in 2 days"), Some(86_400_000));
        assert_eq!(
            stated_wait_ms("resets in 0 seconds"),
            None,
            "a zero wait is no wait"
        );
        assert_eq!(
            stated_wait_ms("resets in 2 months"),
            None,
            "months are not minutes"
        );
        assert_eq!(stated_wait_ms("resets in 3 requests"), None);
        assert_eq!(stated_wait_ms("slow down"), None);
    }

    #[test]
    fn a_reset_stated_as_a_calendar_date_is_read() {
        // Captured live: the provider meters a four-hour window and names its end. `Retry-After`
        // said one hour, so the account walked back into the same refusal three times.
        let now = time::macros::datetime!(2026-11-12 14:00:00 UTC);
        let message = "your free allowance for ':free' models is used up. it comes back on 12 nov 2026 at 15:05 utc.";
        assert_eq!(reset_instant_ms(message, now), Some(65 * 60_000));
        // `stated_wait_ms` reads the clock, so its own answer moves with the wall; the fixed-clock
        // assertion above is the one that proves the arithmetic.
    }

    #[test]
    fn a_weekly_reset_stated_as_a_date_is_read() {
        let now = time::macros::datetime!(2026-11-12 14:00:00 UTC);
        let message = "your next rolling 7-day period starts on 19 nov 2026 at 01:29 utc.";
        assert_eq!(
            reset_instant_ms(message, now),
            Some(6 * 86_400_000 + 11 * 3_600_000 + 29 * 60_000)
        );
    }

    #[test]
    fn a_date_that_promises_nothing_is_not_a_wait() {
        let now = time::macros::datetime!(2026-11-12 14:00:00 UTC);
        // No anchor, so this is a timestamp in a diagnostic, not a recovery instant.
        assert_eq!(
            reset_instant_ms("request failed at 12 nov 2026 15:05 utc", now),
            None
        );
        // Already elapsed: the width of the reset is unknown, so leave it to the default guess.
        assert_eq!(
            reset_instant_ms("resets on 12 nov 2026 at 13:00 utc", now),
            None
        );
        // Further away than any real quota window: a typo, not a fact.
        assert_eq!(
            reset_instant_ms("resets on 12 nov 2027 at 15:05 utc", now),
            None
        );
        // A year is required; a missing one would turn a typo into a very long cooldown.
        assert_eq!(reset_instant_ms("resets on 12 nov at 15:05 utc", now), None);
    }

    #[test]
    fn status_codes_map_to_the_cooldowns_they_deserve() {
        use select::CooldownReason;
        // The distinction that decides whether an account can recover on its own.
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED, false),
            CooldownReason::AuthFailed
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN, false),
            CooldownReason::AuthFailed
        );
        assert_eq!(
            classify_status(StatusCode::PAYMENT_REQUIRED, false),
            CooldownReason::QuotaExhausted
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, false),
            CooldownReason::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::BAD_GATEWAY, false),
            CooldownReason::Backoff
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR, false),
            CooldownReason::Backoff
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, false),
            CooldownReason::Backoff,
            "only the self-hosted Modal provider should classify its 503s as cold starts"
        );
        assert_eq!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE, true),
            CooldownReason::ColdStart
        );
    }

    #[test]
    fn healthz_does_not_consult_a_provider() {
        // It returns a constant, so it cannot fail because a provider is down.
        let parsed: serde_json::Value = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert_eq!(parsed["status"], "ok");
    }

    #[tokio::test]
    async fn the_documented_route_list_is_the_one_registered() {
        // A route that is documented but not registered is a 404 that reads as a client bug. Every
        // path in `docs/18-api-reference.md` is therefore asked of the **mounted router**, and each
        // must answer something other than axum's own empty 404 — which is what an unregistered path
        // produces. A 401, a 405, a 400 or a JSON 404 all prove the path reached a handler, and that
        // is the whole claim: this list is about mounting, not about what each route then does.
        let documented = [
            // Liveness and the control plane.
            "/healthz",
            "/control/probe",
            "/control/activity",
            // Text.
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/responses/{id}",
            "/v1/responses/{id}/cancel",
            "/v1/responses/{id}/input_items",
            "/v1/messages",
            "/v1/messages/count_tokens",
            // Models.
            "/v1/models",
            "/v1/models/{*model}",
            // The modalities.
            "/v1/embeddings",
            "/v1/moderations",
            "/v1/audio/transcriptions",
            "/v1/audio/translations",
            "/v1/audio/speech",
            "/v1/images/generations",
            "/v1/images/edits",
            "/v1/images/variations",
            "/v1/videos",
            "/v1/videos/{*rest}",
            "/v1/realtime",
            "/v1/realtime/client_secrets",
            "/v1/realtime/calls",
            // Files, resumable uploads and batches.
            "/v1/files",
            "/v1/files/{id}",
            "/v1/files/{id}/content",
            "/v1/uploads",
            "/v1/uploads/{id}/parts",
            "/v1/uploads/{id}/complete",
            "/v1/uploads/{id}/cancel",
            "/v1/batches",
            "/v1/batches/{id}",
            "/v1/batches/{id}/cancel",
            // Vector stores and retrieval.
            "/v1/vector_stores",
            "/v1/vector_stores/{id}",
            "/v1/vector_stores/{id}/files",
            "/v1/vector_stores/{id}/files/{file_id}",
            "/v1/vector_stores/{id}/file_batches",
            "/v1/vector_stores/{id}/file_batches/{batch_id}",
            "/v1/vector_stores/{id}/search",
            "/v1/search",
            // Jobs.
            "/v1/fine_tuning/jobs",
            "/v1/fine_tuning/jobs/{*rest}",
            "/v1/containers",
            "/v1/containers/{*rest}",
            "/v1/evals",
            "/v1/evals/{id}",
            "/v1/evals/{id}/runs",
            "/v1/evals/{id}/runs/{run_id}",
        ];
        // Every documented family is in the list exactly once, so a path added to the router and
        // forgotten here is caught by the count rather than by a reader noticing.
        assert_eq!(documented.len(), 51);
        let mut unique = documented.to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), documented.len(), "a path is listed twice");

        let state = state_over(Vec::new(), Vec::new());
        for template in documented {
            let path = template
                .replace("{*model}", "open-ai/gpt-4o-mini")
                .replace("{*rest}", "x")
                .replace("{run_id}", "run-1")
                .replace("{batch_id}", "vsb-1")
                .replace("{file_id}", "file-1")
                .replace("{id}", "x");
            let (status, body) = call_router(&state, Method::GET, &path, Vec::new()).await;
            assert!(
                status != StatusCode::NOT_FOUND || !body.is_empty(),
                "{template} is documented but nothing is mounted there (axum answered an empty 404)"
            );
        }
    }

    #[test]
    fn no_path_is_registered_twice() {
        // `Router::merge` and `Router::route` both panic on a duplicate path, so the mount itself is
        // the test: this call would abort the process rather than fail the assertion. It exists
        // because the families are merged from five modules plus this file, and `/v1/search` in
        // particular belongs to exactly one of them.
        let state = state_over(Vec::new(), Vec::new());
        let _ = router(state);
    }

    #[test]
    fn hop_by_hop_headers_are_never_forwarded() {
        const HOP_BY_HOP: &[&str] = &[
            "connection",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "content-length",
        ];
        assert!(HOP_BY_HOP.contains(&"content-length"));
        assert!(HOP_BY_HOP.contains(&"transfer-encoding"));
        // Content type describes the payload, not the connection, so it must survive.
        assert!(!HOP_BY_HOP.contains(&"content-type"));
    }

    #[test]
    fn an_oversized_body_is_refused_with_a_413() {
        // The bound itself is asserted at compile time next to the constant; this checks the response
        // the bound produces.
        assert_eq!(
            BodyError::TooLarge.into_response().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            BodyError::Read("closed".into()).into_response().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn a_route_override_is_found_by_its_canonical_key() {
        // The catalogue and the configuration documentation key `route_overrides` by the route's
        // configuration spelling, so a client on "/v1/chat/completions" must find the entry
        // "chat-completions".
        let mut provider = provider_plan();
        provider
            .route_overrides
            .insert("chat-completions".into(), "ai/v1/chat/completions".into());
        let found = route_override_for(
            &provider,
            RouteKind::ChatCompletions,
            "/v1/chat/completions",
        );
        assert_eq!(found.map(String::as_str), Some("ai/v1/chat/completions"));
    }

    #[test]
    fn a_route_override_keyed_by_a_raw_request_path_no_longer_matches() {
        // The old spelling. It can never match a route, so the override would silently never
        // fire; the lookup must not find it.
        let mut provider = provider_plan();
        provider.route_overrides.insert(
            "/v1/chat/completions".into(),
            "ai/v1/chat/completions".into(),
        );
        let found = route_override_for(
            &provider,
            RouteKind::ChatCompletions,
            "/v1/chat/completions",
        );
        assert_eq!(found, None);
    }

    #[test]
    fn a_canonical_override_is_used_even_when_a_legacy_key_is_present() {
        // The stray raw-path key is reported, but it is a report and not a fallback: the
        // canonical key is the one that decides.
        let mut provider = provider_plan();
        provider
            .route_overrides
            .insert("chat-completions".into(), "ai/v1/chat/completions".into());
        provider
            .route_overrides
            .insert("/v1/chat/completions".into(), "legacy".into());
        let found = route_override_for(
            &provider,
            RouteKind::ChatCompletions,
            "/v1/chat/completions",
        );
        assert_eq!(found.map(String::as_str), Some("ai/v1/chat/completions"));
    }

    #[test]
    fn a_provider_without_route_overrides_has_no_override() {
        let provider = provider_plan();
        assert_eq!(
            route_override_for(
                &provider,
                RouteKind::ChatCompletions,
                "/v1/chat/completions"
            ),
            None
        );
    }

    #[test]
    fn a_route_override_is_looked_up_by_the_route_it_was_called_on() {
        // The override belongs to the client's route, not to the provider's dialect: a provider
        // serving "/v1/responses" has its own key.
        let mut provider = provider_plan();
        provider
            .route_overrides
            .insert("responses".into(), "v1/responses".into());
        assert_eq!(
            route_override_for(&provider, RouteKind::Responses, "/v1/responses")
                .map(String::as_str),
            Some("v1/responses")
        );
        assert_eq!(
            route_override_for(
                &provider,
                RouteKind::ChatCompletions,
                "/v1/chat/completions"
            ),
            None
        );
    }

    #[test]
    fn model_extraction_reads_a_plain_name() {
        let head = scan_request_head(br#"{"model":"coding","messages":[]}"#);
        assert_eq!(
            requested_model(&head, br#"{"model":"coding","messages":[]}"#),
            Some("coding")
        );
    }

    #[test]
    fn model_extraction_reports_none_without_a_model_field() {
        let head = scan_request_head(br#"{"messages":[]}"#);
        assert_eq!(requested_model(&head, br#"{"messages":[]}"#), None);
    }

    #[test]
    fn a_body_that_is_not_json_is_a_400_not_a_panic() {
        // The scan is the first thing to touch the body and the binary is built with `panic = "abort"`,
        // so a panic here takes the process down.
        let head = scan_request_head(b"not json at all");
        assert_eq!(requested_model(&head, br#"{"messages":[]}"#), None);
    }

    #[test]
    fn a_streaming_request_asks_the_upstream_for_events() {
        let head = scan_request_head(br#"{"model":"m","stream":true}"#);
        assert!(head.wants_stream());
    }

    // -----------------------------------------------------------------------------------------
    // The modality fallback
    // -----------------------------------------------------------------------------------------

    /// A provider serving one model, with the modalities that model accepts.
    fn provider_serving(
        id: &str,
        base_url: &str,
        model: &str,
        modalities: &[InputModality],
    ) -> mr_core::model::Provider {
        let mut provider = mr_core::model::Provider::new(id, Dialect::OpenAi, base_url);
        let mut account = mr_core::model::Account::new(id);
        // An account without an explicit reference falls back to a conventional vault path, so the
        // test names one explicitly and puts a value there.
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        let mut entry = dm();
        entry.id = model.to_owned();
        entry.input_modalities = modalities.to_vec();
        provider.models = vec![entry];
        provider
    }

    /// A fake upstream: records every body it receives and answers with a canned completion.
    ///
    /// Returns the base URL to point a provider at, and the recorded bodies.
    async fn fake_upstream() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(String::from_utf8_lossy(&body).to_string());
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"id":"cmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
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

    /// A relay state wired to two fake upstreams: one that takes text only, one that takes images.
    ///
    /// `default_image` is the `routing.defaults.image` entry, so a test can point it at a model
    /// that does not exist, or at one that cannot carry the image either.
    async fn fallback_state_with(
        default_image: &str,
    ) -> (
        AppState,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (text_url, text_seen) = fake_upstream().await;
        let (vision_url, vision_seen) = fake_upstream().await;

        let config = mr_core::config::Config {
            providers: vec![
                provider_serving("text", &text_url, "text-model", &[InputModality::Text]),
                provider_serving(
                    "vision",
                    &vision_url,
                    "vision-model",
                    &[InputModality::Text, InputModality::Image],
                ),
            ],
            routing: mr_core::config::RoutingConfig {
                defaults: mr_core::config::RoutingDefaults {
                    image: Some(default_image.to_owned()),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        // A vault in a private temporary directory, holding the one credential the accounts name.
        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, text_seen, vision_seen)
    }

    /// A chat body that carries an image, which the text-only route cannot serve.
    fn image_body(model: &str) -> String {
        serde_json::json!({
            "model": model,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is in this picture?"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}},
                ],
            }],
        })
        .to_string()
    }

    #[tokio::test]
    async fn an_image_request_falls_back_to_the_configured_default_and_reroutes_completely() {
        let (state, text_seen, vision_seen) = fallback_state_with("vision/vision-model").await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(image_body("text/text-model")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);

        // The text-only upstream must not have been called at all: the fallback is a *reroute*,
        // not a second attempt at the same provider with a different model name.
        assert!(
            text_seen.lock().unwrap().is_empty(),
            "the route that cannot carry the image was called anyway"
        );

        // The vision upstream got the request, with the fallback model in the body. The upstream
        // sees its own model id, not the client-facing one the fallback was resolved from.
        let seen = vision_seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one upstream call");
        let body: serde_json::Value = serde_json::from_str(&seen[0]).unwrap();
        assert_eq!(body["model"], "vision-model");
        // The request's content travelled with it: the fallback retried the request, it did not
        // replace it with a stub.
        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "what is in this picture?"
        );
    }

    #[tokio::test]
    async fn a_request_the_route_can_serve_is_not_fallback() {
        let (state, text_seen, vision_seen) = fallback_state_with("vision/vision-model").await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(
                serde_json::json!({
                    "model": "text/text-model",
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(text_seen.lock().unwrap().len(), 1);
        assert!(vision_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_default_that_names_no_known_model_is_reported_rather_than_routed() {
        let (state, text_seen, vision_seen) = fallback_state_with("nosuch/model").await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(image_body("text/text-model")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(text_seen.lock().unwrap().is_empty());
        assert!(vision_seen.lock().unwrap().is_empty());
    }

    // -----------------------------------------------------------------------------------------
    // Per-key quota enforcement
    // -----------------------------------------------------------------------------------------

    /// A state with one client key that carries a request budget.
    ///
    /// The key is presented in the `Authorization` header, and the budget is deliberately small
    /// enough to be reached inside the test.
    async fn quota_state(budget: u64) -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
        let (url, seen) = fake_upstream().await;
        let mut provider = provider_serving("p", &url, "m", &[InputModality::Text]);

        // The relay compares hashes, so the configuration carries the hash of the key the test
        // presents — never the key itself.
        let presented = "mr-test-key";
        let hash = RelayState::hash_presented(presented);
        let key_hash: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        provider.accounts[0].secret_ref = Some("test-credential".to_owned());

        let config = mr_core::config::Config {
            providers: vec![provider],
            client_keys: vec![mr_core::model::ClientKey {
                id: mr_core::ids::ClientKeyId::from_raw("key-1"),
                label: "quota-test".to_owned(),
                key_hash,
                prefix: Some("mr-test".to_owned()),
                enabled: true,
                allow_models: Vec::new(),
                allow_routes: Vec::new(),
                limit_requests: budget,
                limits: Default::default(),
                note: None,
            }],
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: false,
            health: Arc::new(crate::select::Health::new()),
            vault,
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, seen)
    }

    /// A chat request for the model the quota fixture serves.
    fn quota_body() -> String {
        serde_json::json!({
            "model": "p/m",
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string()
    }

    #[tokio::test]
    async fn a_key_within_its_budget_is_served_and_the_next_one_is_refused() {
        let (state, seen) = quota_state(1).await;
        let auth = headers(&[("authorization", "Bearer mr-test-key")]);

        let first = dispatch(
            state.clone(),
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            auth.clone(),
            AxumBody::from(quota_body()),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);

        let second = dispatch(
            state.clone(),
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            auth,
            AxumBody::from(quota_body()),
        )
        .await;
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        // The refusal happened before the upstream was contacted: the budget is spent by the
        // request, not by the round trip.
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_key_with_no_budget_is_never_refused() {
        // Zero means unlimited, and a key that asked for no budget is not metered at all.
        let (state, seen) = quota_state(0).await;
        let auth = headers(&[("authorization", "Bearer mr-test-key")]);
        for _ in 0..3 {
            let response = dispatch(
                state.clone(),
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                auth.clone(),
                AxumBody::from(quota_body()),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(seen.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_default_that_cannot_carry_the_image_either_fails_instead_of_looping() {
        // The budget is one fallback. A default that points back at a text-only route must fail
        // the request, not send it around the loop again.
        let (state, text_seen, vision_seen) = fallback_state_with("text/text-model").await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(image_body("text/text-model")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(text_seen.lock().unwrap().is_empty());
        assert!(vision_seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_fallback_body_keeps_the_request_and_replaces_the_model() {
        let original =
            Bytes::from_static(br#"{"model":"old","messages":[{"role":"user","content":"hi"}]}"#);
        let rewritten = request_with_model(&original, "vision/vision-model").unwrap();
        let value: serde_json::Value = serde_json::from_slice(&rewritten).unwrap();
        assert_eq!(value["model"], "vision/vision-model");
        assert_eq!(value["messages"][0]["content"], "hi");
    }

    #[test]
    fn a_body_without_a_model_is_not_rewritten() {
        // There is no field to replace, and guessing one would retarget a request the client
        // never named.
        let original = Bytes::from_static(br#"{"messages":[]}"#);
        assert!(request_with_model(&original, "vision/vision-model").is_none());
    }

    #[test]
    fn a_body_that_is_not_json_is_not_rewritten() {
        let original = Bytes::from_static(b"not json");
        assert!(request_with_model(&original, "vision/vision-model").is_none());
    }

    // =========================================================================================
    // Proxy egress
    // =========================================================================================

    /// A fake HTTP `CONNECT` proxy.
    ///
    /// It records the target of every tunnel it is asked for and then pipes bytes to it, so the
    /// relay's tunnel really is exercised end to end: a stub that only recorded the request would
    /// not prove that the relay speaks the protocol, nor that the bytes arrive.
    ///
    /// `refuse` makes it answer `403` instead of establishing the tunnel, which is how a proxy that
    /// will not carry a request behaves.
    async fn fake_connect_proxy(refuse: bool) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    // Read the CONNECT head: the request line and the headers.
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    while socket.read(&mut byte).await.unwrap_or(0) == 1 {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&head).to_string();
                    let target = text.split_whitespace().nth(1).unwrap_or("").to_string();
                    sink.lock().unwrap().push(target.clone());

                    if refuse {
                        let _ = socket
                            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        return;
                    }
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                        .await;

                    let Ok(upstream) = tokio::net::TcpStream::connect(&target).await else {
                        return;
                    };
                    let (mut client_read, mut client_write) = socket.into_split();
                    let (mut upstream_read, mut upstream_write) = upstream.into_split();
                    let to_upstream = tokio::spawn(async move {
                        tokio::io::copy(&mut client_read, &mut upstream_write).await
                    });
                    let to_client = tokio::spawn(async move {
                        tokio::io::copy(&mut upstream_read, &mut client_write).await
                    });
                    let _ = tokio::join!(to_upstream, to_client);
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// A state whose provider egresses through a proxy.
    ///
    /// `provider_proxy` is what the provider names: `Some("edge")` for the explicit assignment,
    /// `None` to let the configured strategy choose from the pool.
    async fn proxy_state(
        proxy_url: &str,
        provider_proxy: Option<&str>,
        proxy_enabled: bool,
    ) -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
        let (url, upstream_seen) = fake_upstream().await;
        let mut provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        provider.proxy = provider_proxy.map(str::to_owned);

        let config = mr_core::config::Config {
            providers: vec![provider],
            proxies: vec![mr_core::config::ProxyConfig {
                id: "edge".to_owned(),
                label: None,
                url: proxy_url.to_owned(),
                username: None,
                secret_ref: None,
                enabled: proxy_enabled,
                priority: 0,
                extra: Default::default(),
            }],
            proxy_strategy: mr_core::config::ProxyStrategy::Priority,
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(opened),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, upstream_seen)
    }

    #[tokio::test]
    async fn a_provider_that_names_a_proxy_egresses_through_it() {
        let (proxy_url, proxy_seen) = fake_connect_proxy(false).await;
        let (state, upstream_seen) = proxy_state(&proxy_url, Some("edge"), true).await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(
                serde_json::json!({
                    "model": "p/m",
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            proxy_seen.lock().unwrap().len(),
            1,
            "the request did not go through the proxy"
        );
        assert_eq!(
            upstream_seen.lock().unwrap().len(),
            1,
            "the upstream did not receive the request"
        );
        let tunnelled = proxy_seen.lock().unwrap()[0].clone();
        assert!(
            tunnelled.starts_with("127.0.0.1:"),
            "the proxy was asked for the wrong target: {tunnelled}"
        );
    }

    #[tokio::test]
    async fn a_proxy_that_refuses_the_tunnel_fails_the_request_without_going_direct() {
        let (proxy_url, proxy_seen) = fake_connect_proxy(true).await;
        let (state, upstream_seen) = proxy_state(&proxy_url, Some("edge"), true).await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(
                serde_json::json!({
                    "model": "p/m",
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            upstream_seen.lock().unwrap().len(),
            0,
            "the request reached the upstream directly, which is the leak the proxy exists to stop"
        );
        assert_eq!(proxy_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_provider_with_no_override_uses_the_configured_strategy() {
        let (proxy_url, proxy_seen) = fake_connect_proxy(false).await;
        let (state, upstream_seen) = proxy_state(&proxy_url, None, true).await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(
                serde_json::json!({
                    "model": "p/m",
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(proxy_seen.lock().unwrap().len(), 1);
        assert_eq!(upstream_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_disabled_proxy_is_not_dialed_and_the_request_goes_out_directly() {
        let (proxy_url, proxy_seen) = fake_connect_proxy(false).await;
        let (state, upstream_seen) = proxy_state(&proxy_url, Some("edge"), false).await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(
                serde_json::json!({
                    "model": "p/m",
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            proxy_seen.lock().unwrap().is_empty(),
            "a disabled proxy was dialled"
        );
        assert_eq!(upstream_seen.lock().unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------------------------
    // The hedged race
    // -----------------------------------------------------------------------------------------

    /// A fake upstream that answers after a delay, with a body that says which one it was.
    ///
    /// The delay is what makes the race observable: the slower participant is the higher-priority
    /// one in the tests below, and a race that rewarded speed instead of priority would answer
    /// with the other body.
    async fn delayed_upstream(
        delay_ms: u64,
        content: &'static str,
        fail: bool,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&body).to_string());
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let status = if fail {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                };
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    format!(
                        r#"{{"id":"cmpl-1","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"{content}"}},"finish_reason":"stop"}}]}}"#
                    ),
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

    /// A fake upstream that answers every request with a fixed status and body.
    ///
    /// Unlike [`delayed_upstream`] the refusal is the point of the fixture, so the status and the
    /// body are exactly what the test asserts reach the client.
    async fn refusing_upstream(
        status: StatusCode,
        body: &'static str,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |req_body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req_body).to_string());
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
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

    /// A relay whose only route is a two-step combo, where the first step is `first_status` and
    /// the second step always answers.
    ///
    /// This is the shape every combo has: a preferred step that is expected to work and a fallback
    /// that exists for the day it does not. What the first step's *status* must not be able to do is
    /// cancel the second.
    async fn failover_state(
        first_status: StatusCode,
        first_body: &'static str,
    ) -> (
        AppState,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (first_url, first_seen) = refusing_upstream(first_status, first_body).await;
        let (second_url, second_seen) = fake_upstream().await;

        let mut combo = mr_core::model::Combo::new(
            "failover",
            mr_core::model::ComboStep::new("first", "shared-model"),
        );
        combo
            .steps
            .push(mr_core::model::ComboStep::new("second", "shared-model"));

        let config = mr_core::config::Config {
            providers: vec![
                provider_serving("first", &first_url, "shared-model", &[InputModality::Text]),
                provider_serving(
                    "second",
                    &second_url,
                    "shared-model",
                    &[InputModality::Text],
                ),
            ],
            combos: vec![combo],
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
        (state, first_seen, second_seen)
    }

    /// A request naming the failover combo.
    fn failover_body() -> String {
        serde_json::json!({
            "model": "failover",
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string()
    }

    /// A 4xx from one step must advance the combo, not end the request.
    ///
    /// Providers reject a request for reasons that say nothing about the *other* steps: a dialect a
    /// given endpoint does not implement, a model it has retired, a path it has moved. The combo is
    /// the operator's answer to exactly that, so a refusal at the first step has to fall through to
    /// the second. When this branch was `Attempt::Fatal` the request died at the first `400`/`404`
    /// and the client saw an upstream it had never asked for — reported, correctly but uselessly, as
    /// "the relay refused the test".
    #[tokio::test]
    async fn a_client_error_from_one_step_falls_through_to_the_next() {
        let (state, first_seen, second_seen) = failover_state(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"upstream rejected this request body"}}"#,
        )
        .await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(failover_body()),
        )
        .await;

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a 4xx from the first step ended the combo instead of falling through"
        );
        assert_eq!(first_seen.lock().unwrap().len(), 1, "the first step runs");
        assert_eq!(
            second_seen.lock().unwrap().len(),
            1,
            "the fallback step was never reached"
        );
    }

    /// A `404` is the same class as a `400`: a statement about the request, not about the account.
    ///
    /// Asserted separately because `404` is what a placeholder left unsubstituted in a base URL
    /// produces — a literal `{account_id}` in the path — and the operator's report of a provider
    /// "refusing the test" was this status all the way down.
    #[tokio::test]
    async fn a_not_found_from_one_step_also_falls_through_to_the_next() {
        let (state, first_seen, second_seen) = failover_state(
            StatusCode::NOT_FOUND,
            r#"{"errors":[{"code":7003,"message":"Could not route to an invalid object identifier"}]}"#,
        )
        .await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(failover_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(first_seen.lock().unwrap().len(), 1);
        assert_eq!(second_seen.lock().unwrap().len(), 1);
    }

    /// When every step refuses with a 4xx, the client sees the provider's own words.
    ///
    /// The failover must not turn a specific upstream explanation into a generic one: the body the
    /// upstream sent is the only thing that tells the operator *which* of the request's fields it
    /// disliked, and synthesizing a replacement here would throw that away at the last moment.
    #[tokio::test]
    async fn a_refusal_from_every_step_reports_the_upstreams_own_message() {
        let (first_url, _first_seen) = refusing_upstream(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"the first upstream refused for its own reason"}}"#,
        )
        .await;
        let (second_url, _second_seen) = refusing_upstream(
            StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"error":{"message":"the second upstream refused for a different reason"}}"#,
        )
        .await;

        let mut combo = mr_core::model::Combo::new(
            "allfail",
            mr_core::model::ComboStep::new("first", "shared-model"),
        );
        combo
            .steps
            .push(mr_core::model::ComboStep::new("second", "shared-model"));

        let config = mr_core::config::Config {
            providers: vec![
                provider_serving("first", &first_url, "shared-model", &[InputModality::Text]),
                provider_serving(
                    "second",
                    &second_url,
                    "shared-model",
                    &[InputModality::Text],
                ),
            ],
            combos: vec![combo],
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(opened),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            batch: crate::batch::BatchLane::inert(),
        };

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(
                serde_json::json!({
                    "model": "allfail",
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;

        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !status.is_success(),
            "every step refused, so the request cannot have succeeded: {text}"
        );
        // The *last* step's message is the one the sequential chain reports, on the same rule it
        // uses for every other failure kind.
        assert!(
            text.contains("the second upstream refused for a different reason"),
            "the upstream's own explanation was replaced by a synthesized one: {text}"
        );
    }

    /// What the race fixture should look like.
    struct RaceSpec {
        /// How long the first (higher-priority) participant takes.
        first_delay_ms: u64,
        /// Whether the first participant fails.
        first_fails: bool,
        /// How long the second participant takes.
        second_delay_ms: u64,
        /// Whether the second participant fails.
        second_fails: bool,
        /// Whether the combo hedges at all.
        hedge: bool,
    }

    impl Default for RaceSpec {
        fn default() -> Self {
            Self {
                first_delay_ms: 0,
                first_fails: false,
                second_delay_ms: 0,
                second_fails: false,
                hedge: true,
            }
        }
    }

    /// A relay whose only route is a two-step combo.
    ///
    /// The first step is the higher-priority one. In the tests below the second answers faster
    /// than the first, so a race that picked the fastest answer would be caught immediately.
    async fn race_state(
        spec: RaceSpec,
    ) -> (
        AppState,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (first_url, first_seen) =
            delayed_upstream(spec.first_delay_ms, "first", spec.first_fails).await;
        let (second_url, second_seen) =
            delayed_upstream(spec.second_delay_ms, "second", spec.second_fails).await;

        let mut combo = mr_core::model::Combo::new(
            "race",
            mr_core::model::ComboStep::new("first", "race-model"),
        );
        combo
            .steps
            .push(mr_core::model::ComboStep::new("second", "race-model"));
        combo.hedge = mr_core::model::HedgeSpec {
            enabled: spec.hedge,
            participants: vec![0, 1],
            window_ms: 2_000,
            ..Default::default()
        };

        let config = mr_core::config::Config {
            providers: vec![
                provider_serving("first", &first_url, "race-model", &[InputModality::Text]),
                provider_serving("second", &second_url, "race-model", &[InputModality::Text]),
            ],
            combos: vec![combo],
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, first_seen, second_seen)
    }

    /// A request for the combo the race fixture serves.
    fn race_body() -> String {
        serde_json::json!({
            "model": "race",
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string()
    }

    /// A Command Code upstream that gives every request its *own* tool call and interleaves slowly
    /// enough that two answers are genuinely in flight at once.
    ///
    /// The nonce is taken from the request's own text, so the call a response carries can only be
    /// right if that response's translation state was private to it.
    async fn commandcode_tool_upstream() -> String {
        let app = Router::new().fallback(any(|body: Bytes| async move {
            let value: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let text = value
                .get("params")
                .and_then(|p| p.get("messages"))
                .and_then(|m| m.as_array())
                .and_then(|a| a.first())
                .and_then(|m| m.get("content"))
                .map(|c| c.to_string())
                .unwrap_or_default();
            let nonce: String = text
                .chars()
                .filter(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let call = format!("call_{nonce}");
            let name = format!("tool_{nonce}");
            // Stream `alpha` is deliberately *long*: its fragments must still be arriving when
            // the other stream begins, so a fragment lands after that stream boundary. That is the
            // exact instant the shared per-thread state used to hand the fragment to the wrong
            // call, because an id it no longer held fell back to "the most recently opened call".
            let mut lines =
                vec![format!(
                    r#"{{"type":"tool-input-start","id":"{call}","toolName":"{name}"}}"#
                )];
            let fragments = if nonce.contains("alpha") { 10 } else { 2 };
            for n in 0..fragments {
                lines.push(format!(
                    r#"{{"type":"tool-input-delta","id":"{call}","delta":"{nonce}-{n}"}}"#
                ));
            }
            lines.push(format!(r#"{{"type":"tool-input-end","id":"{call}"}}"#));
            lines.push(format!(
                r#"{{"type":"tool-call","toolCallId":"{call}","toolName":"{name}","input":{{"n":"{nonce}"}}}}"#
            ));
            lines.push(r#"{"type":"finish","finishReason":"tool-calls","rawFinishReason":"tool_calls","totalUsage":{"inputTokens":100,"inputTokenDetails":{"cacheReadTokens":80,"cacheWriteTokens":15},"outputTokens":25}}"#.to_owned());
            // The two streams are deliberately *staggered*, because simultaneity is not the
            // condition that broke: the corruption needed the **second** stream to begin while the
            // first was already mid-flight, so that a stream boundary fell between the first
            // stream's open and its own argument fragments. A per-thread reset at that boundary is
            // what erased the live stream's state and sent its fragments to whatever call the cell
            // happened to hold last.
            //
            // `beta` therefore starts late and then runs at the normal tick.
            let tick = if nonce.contains("beta") {
                std::time::Duration::from_millis(130)
            } else {
                std::time::Duration::from_millis(25)
            };
            let stream = futures_util::stream::unfold(0usize, move |i| {
                let lines = lines.clone();
                async move {
                    if i >= lines.len() {
                        return None;
                    }
                    tokio::time::sleep(tick).await;
                    Some((
                        Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!(
                            "{}\n",
                            lines[i]
                        ))),
                        i + 1,
                    ))
                }
            });
            let mut response = Response::new(AxumBody::from_stream(stream));
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            response
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// The whole point, through the real router: two Command Code answers served concurrently must
    /// each carry exactly their own tool call, with no empty id or name anywhere.
    ///
    /// This is the failure the sessions died of — `format v4 tool/result at seq N requires
    /// toolCallId matching its tool source` — reproduced as the condition that caused it: two
    /// streams in flight at once on one gateway.
    #[tokio::test]
    async fn two_concurrent_commandcode_requests_through_the_router_keep_their_own_tool_calls() {
        let base_url = commandcode_tool_upstream().await;
        let mut provider = mr_core::model::Provider::new("cmd", Dialect::CommandCodeGo, &base_url);
        let mut account = mr_core::model::Account::new("cmd");
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        let mut entry = dm();
        entry.id = "m".to_owned();
        provider.models = vec![entry];
        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut vault = mr_core::secret::Vault::open(dir.path()).unwrap();
        vault.put("test-credential", "sk-not-a-real-key").unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(vault),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };

        // Two requests, each baking its own nonce into the prompt, dispatched together.
        let body_for = |nonce: &str| {
            serde_json::json!({
                "model": "cmd/m",
                "stream": true,
                "messages": [{"role": "user", "content": nonce}],
            })
            .to_string()
        };
        let (first, second) = tokio::join!(
            dispatch(
                state.clone(),
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                headers(&[]),
                AxumBody::from(body_for("alpha1")),
            ),
            dispatch(
                state.clone(),
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                headers(&[]),
                AxumBody::from(body_for("beta2x")),
            )
        );
        let (first_status, second_status) = (first.status(), second.status());
        let (first, second) = (body_text(first).await, body_text(second).await);
        assert_eq!(
            first_status,
            StatusCode::OK,
            "first request failed: {first}"
        );
        assert_eq!(
            second_status,
            StatusCode::OK,
            "second request failed: {second}"
        );

        // The invariant a client assembler actually depends on: a tool-call delta may only refer
        // to an index that this same response already opened with an id and a name, and no opened
        // block may ever carry an empty id or name. A fragment emitted under some other index is
        // how the phantom block appeared — `{"type":"tool-call","id":"","name":"","arguments":"{}"}`
        // — and it is why the client could neither dispatch the call nor match its result.
        for (label, body) in [("first", &first), ("second", &second)] {
            let mut opened: std::collections::HashMap<u64, (String, String)> =
                std::collections::HashMap::new();
            for line in body.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data.trim() == "[DONE]" {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    continue;
                };
                let Some(calls) = value["choices"][0]["delta"].get("tool_calls") else {
                    continue;
                };
                for call in calls.as_array().unwrap() {
                    let index = call["index"].as_u64().unwrap_or(0);
                    match call.get("id").and_then(|v| v.as_str()) {
                        // An open: it must name the call, and it registers the index.
                        Some(id) => {
                            assert!(!id.is_empty(), "{label}: opened a block with an empty id");
                            let name = call["function"]["name"].as_str().unwrap_or("");
                            assert!(
                                !name.is_empty(),
                                "{label}: opened a block with an empty name"
                            );
                            opened.insert(index, (id.to_owned(), name.to_owned()));
                        }
                        // A fragment: its index must be one this response already opened, or the
                        // client is being handed arguments for a call it never saw named.
                        None => assert!(
                            opened.contains_key(&index),
                            "{label}: a fragment referred to index {index}, which this response \
                             never opened (opened: {opened:?}) — body: {body}"
                        ),
                    }
                }
            }
            assert_eq!(
                opened.len(),
                1,
                "{label} must present exactly one tool call, saw {opened:?}"
            );
        }

        for (label, body, mine, theirs) in [
            ("first", &first, "alpha1", "beta2x"),
            ("second", &second, "beta2x", "alpha1"),
        ] {
            assert!(
                body.contains(&format!("call_{mine}")),
                "{label} did not carry its own call id: {body}"
            );
            assert!(
                body.contains(&format!("tool_{mine}")),
                "{label} did not carry its own tool name: {body}"
            );
            assert!(
                !body.contains(&format!("call_{theirs}")),
                "{label} carried the OTHER request's call id: {body}"
            );
            // The exact signature of the corruption: a tool call the client can neither dispatch
            // nor match, because it arrived with no id and no name.
            assert!(
                !body.contains(r#""id":"""#),
                "{label} emitted an empty tool-call id: {body}"
            );
            assert!(
                !body.contains(r#""name":"""#),
                "{label} emitted an empty tool-call name: {body}"
            );
        }

        // And each stream reports the usage and cache split the upstream sent, which is what makes
        // a streamed answer's cost and rate observable at all.
        for (label, body) in [("first", &first), ("second", &second)] {
            assert!(body.contains(r#""prompt_tokens":100"#), "{label}: {body}");
            assert!(
                body.contains(r#""completion_tokens":25"#),
                "{label}: {body}"
            );
            assert!(
                body.contains(r#""cached_tokens":80"#),
                "{label} lost the cache read split: {body}"
            );
            assert!(
                body.contains(r#""cache_write_input_tokens":15"#),
                "{label} lost the cache write split: {body}"
            );
        }
    }

    /// A non-streaming client talking to a stream-only upstream must receive the folded answer.
    ///
    /// Codex and Command Code both stream unconditionally: a client asking for a single JSON
    /// document gets one synthesised by the relay, which consumes the upstream's SSE and folds it.
    /// That folded body is JSON, but the response kept the upstream's `text/event-stream` content
    /// type. The recovery layer keys its event framing on that type, so it held the document
    /// waiting for a blank-line-terminated event that never arrives, then dropped the whole thing
    /// at end of stream — an empty 200 with no body and no error. Every non-streaming call through
    /// a stream-only provider failed this way.
    /// A `Dialect::Codex` upstream that answers with a plain Responses event stream.
    async fn codex_stream_upstream() -> String {
        let app = axum::Router::new().fallback(move || async move {
            let lines = vec![
                r#"{"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}"#
                    .to_owned(),
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","content":[]}}"#.to_owned(),
                r#"{"type":"response.output_text.delta","item_id":"msg_1","output_index":0,"content_index":0,"delta":"hello from codex"}"#.to_owned(),
                r#"{"type":"response.output_text.done","item_id":"msg_1","output_index":0,"content_index":0,"text":"hello from codex"}"#.to_owned(),
                r#"{"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":11,"output_tokens":4,"total_tokens":15}}}"#.to_owned(),
            ];
            let stream = futures_util::stream::iter(
                lines
                    .into_iter()
                    .map(|line| Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("data: {line}\n\n")))),
            );
            let mut response = Response::new(AxumBody::from_stream(stream));
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            response
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn a_folded_answer_keeps_its_body_and_is_labelled_json() {
        let base_url = codex_stream_upstream().await;
        let mut provider = mr_core::model::Provider::new("cmd", Dialect::Codex, &base_url);
        let mut account = mr_core::model::Account::new("cmd");
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        let mut entry = dm();
        entry.id = "m".to_owned();
        provider.models = vec![entry];
        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut vault = mr_core::secret::Vault::open(dir.path()).unwrap();
        vault.put("test-credential", "sk-not-a-real-key").unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(vault),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };

        // Deliberately *not* streaming: this is the path that folded the answer into nothing.
        // A Responses-shaped request, because Codex speaks only that shape.
        let body = serde_json::json!({
            "model": "cmd/m",
            "input": "fold1x",
        })
        .to_string();
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(body),
        )
        .await;

        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = body_text(response).await;
        assert_eq!(status, StatusCode::OK, "the folded request failed: {body}");
        assert!(
            content_type.contains("application/json"),
            "a folded answer is JSON, but it is labelled {content_type:?}: {body}"
        );
        assert!(
            !body.trim().is_empty(),
            "the folded answer was empty; the document was dropped instead of delivered"
        );
        // The upstream's own content must survive the fold, not merely the envelope around it.
        assert!(
            body.contains("chat.completion"),
            "the folded body is not a completion document: {body}"
        );
    }

    /// A streaming answer's usage must land in the ledger with its cache split and its window.
    ///
    /// This is the token-reporting path end to end: the provider reports counts on its `finish`
    /// event, the tap folds them (with the generation window) out of the stream, the ticket records
    /// them when the body ends, and the ledger row carries them back out. Before this, the counts
    /// were dropped, the window was discarded, and tokens-per-second was therefore never computable
    /// at all.
    #[tokio::test]
    async fn a_streamed_answer_records_its_tokens_cache_split_and_rate() {
        let base_url = commandcode_tool_upstream().await;
        let mut provider = mr_core::model::Provider::new("cmd", Dialect::CommandCodeGo, &base_url);
        let mut account = mr_core::model::Account::new("cmd");
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        let mut entry = dm();
        entry.id = "m".to_owned();
        provider.models = vec![entry];

        let presented = "mr-test-key";
        let key_hash: String = RelayState::hash_presented(presented)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let config = mr_core::config::Config {
            providers: vec![provider],
            client_keys: vec![mr_core::model::ClientKey {
                id: mr_core::ids::ClientKeyId::from_raw("key-1"),
                label: "usage-test".to_owned(),
                key_hash,
                prefix: Some("mr-test".to_owned()),
                enabled: true,
                allow_models: Vec::new(),
                allow_routes: Vec::new(),
                // No caps: the point is that usage is recorded even when nothing is enforced.
                limit_requests: 0,
                limits: Default::default(),
                note: None,
            }],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut vault = mr_core::secret::Vault::open(dir.path()).unwrap();
        vault.put("test-credential", "sk-not-a-real-key").unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: false,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(vault),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[("authorization", "Bearer mr-test-key")]),
            AxumBody::from(
                serde_json::json!({
                    "model": "cmd/m",
                    "stream": true,
                    "messages": [{"role": "user", "content": "alpha1"}],
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // Draining the body is what lets the tap finish and the ticket settle.
        let _ = body_text(response).await;

        let rows = store.recent_key_usage(10, None).unwrap();
        assert_eq!(rows.len(), 1, "the request was not recorded: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.client_key_id, "key-1");
        assert_eq!(row.prompt_tokens, Some(100), "input tokens lost");
        assert_eq!(row.completion_tokens, Some(25), "output tokens lost");
        assert_eq!(row.cached_tokens, Some(80), "cache read lost");
        assert_eq!(row.cache_write_tokens, Some(15), "cache write lost");
        // The window is recorded when the tap could measure one. A body that arrived in a single
        // read has no measurable window, and then the column stays `NULL` — which is correct, and
        // is why the assertion is "never a fabricated zero" rather than "always present".
        assert!(
            row.generation_ms != Some(0),
            "a zero window was recorded, which would make a rate infinite"
        );
    }

    /// A stream whose frames genuinely arrive apart must produce a real generation window.
    ///
    /// This is the counterpart to the test above: that one showed a coalesced body has no window to
    /// report, and this one shows the window *is* reported when the bytes really do arrive
    /// separately. Without both, "tokens per second is blank" would be indistinguishable from a
    /// tap that never measures at all — which was the original complaint.
    #[tokio::test]
    async fn a_slow_stream_records_a_real_generation_window_and_rate() {
        // Four frames, 200 ms apart, so a whole-stream read is impossible.
        let app = Router::new().fallback(any(|_body: Bytes| async move {
            let frames = [
                r#"data: {"choices":[{"delta":{"content":"one"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"two"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"three"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":"four"}}],"usage":{"prompt_tokens":40,"completion_tokens":8}}"#,
                "data: [DONE]",
            ];
            let stream = futures_util::stream::unfold(0usize, move |i| async move {
                if i >= frames.len() {
                    return None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                Some((
                    Ok::<Bytes, std::convert::Infallible>(Bytes::from(format!("{}\n\n", frames[i]))),
                    i + 1,
                ))
            });
            let mut response = Response::new(AxumBody::from_stream(stream));
            response
                .headers_mut()
                .insert("content-type", "text/event-stream".parse().unwrap());
            response
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut provider =
            mr_core::model::Provider::new("p", Dialect::OpenAi, format!("http://{addr}"));
        let mut account = mr_core::model::Account::new("p");
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        let mut entry = dm();
        entry.id = "m".to_owned();
        provider.models = vec![entry];

        let key_hash: String = RelayState::hash_presented("mr-test-key")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let config = mr_core::config::Config {
            providers: vec![provider],
            client_keys: vec![mr_core::model::ClientKey {
                id: mr_core::ids::ClientKeyId::from_raw("key-1"),
                label: "window-test".to_owned(),
                key_hash,
                prefix: Some("mr-test".to_owned()),
                enabled: true,
                allow_models: Vec::new(),
                allow_routes: Vec::new(),
                limit_requests: 0,
                limits: Default::default(),
                note: None,
            }],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut vault = mr_core::secret::Vault::open(dir.path()).unwrap();
        vault.put("test-credential", "sk-not-a-real-key").unwrap();
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: false,
            health: Arc::new(crate::select::Health::new()),
            vault: Arc::new(vault),
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: Arc::clone(&store),
            quota_store: Arc::clone(&store),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
            activity: Arc::new(crate::activity::ActivityLog::new()),
            kv_usage: Arc::new(crate::kv_budget::AdmissionPool::new()),
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[("authorization", "Bearer mr-test-key")]),
            AxumBody::from(
                serde_json::json!({
                    "model": "p/m",
                    "stream": true,
                    "messages": [{"role": "user", "content": "hello"}],
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let _ = body_text(response).await;

        let rows = store.recent_key_usage(10, None).unwrap();
        let row = rows.first().expect("no row was recorded");
        assert_eq!(row.prompt_tokens, Some(40));
        assert_eq!(row.completion_tokens, Some(8));
        let window = row
            .generation_ms
            .expect("a slow stream has a measurable window");
        assert!(
            window >= 500,
            "four frames 200 ms apart must span at least 500 ms, got {window}"
        );
        let rate = mr_core::usage::tokens_per_second(row.completion_tokens, row.generation_ms)
            .expect("a measured window must yield a rate");
        assert!((rate - 8.0 / (window as f64 / 1000.0)).abs() < 1e-6);
    }

    /// The text of a response body, as a string.
    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn the_race_serves_the_highest_priority_answer_even_when_it_is_the_slower_one() {
        let (state, first_seen, second_seen) = race_state(RaceSpec {
            first_delay_ms: 120,
            second_delay_ms: 0,
            ..Default::default()
        })
        .await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(race_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let text = body_text(response).await;
        assert!(
            text.contains("first"),
            "the race did not serve the higher-priority answer: {text}"
        );
        // Both participants were dispatched: that is what a race is.
        assert_eq!(first_seen.lock().unwrap().len(), 1);
        assert_eq!(second_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_participant_hands_the_race_to_the_one_that_answered() {
        let (state, first_seen, second_seen) = race_state(RaceSpec {
            first_fails: true,
            ..Default::default()
        })
        .await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(race_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let text = body_text(response).await;
        assert!(
            text.contains("second"),
            "the race did not fall to the participant that answered: {text}"
        );
        assert_eq!(first_seen.lock().unwrap().len(), 1);
        assert_eq!(second_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_race_whose_participants_all_fail_reports_the_failure_rather_than_succeeding() {
        let (state, _first_seen, _second_seen) = race_state(RaceSpec {
            first_fails: true,
            second_fails: true,
            ..Default::default()
        })
        .await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(race_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn a_combo_without_the_hedge_calls_only_its_first_step() {
        let (state, first_seen, second_seen) = race_state(RaceSpec {
            hedge: false,
            ..Default::default()
        })
        .await;
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(race_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(first_seen.lock().unwrap().len(), 1);
        assert!(
            second_seen.lock().unwrap().is_empty(),
            "the second step was called without a hedge"
        );
    }

    // -----------------------------------------------------------------------------------------
    // The token-counting route
    // -----------------------------------------------------------------------------------------

    /// A count-tokens request body, in the shape an Anthropic client sends it.
    fn count_tokens_body(model: &str) -> String {
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string()
    }

    /// A fake upstream that records the path and the body of every request it receives, and
    /// answers with a canned status and body.
    async fn counting_upstream(
        status: StatusCode,
        answer: &'static str,
    ) -> (String, Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |uri: Uri, body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push((
                    uri.path().to_string(),
                    String::from_utf8_lossy(&body).to_string(),
                ));
                (
                    status,
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

    /// A relay with one provider, in the dialect under test, pointing at `base_url`.
    ///
    /// The provider serves one model, `m`, so a client asks for it as `p/m`.
    fn count_tokens_state(dialect: Dialect, base_url: &str) -> AppState {
        let mut provider = provider_serving("p", base_url, "m", &[InputModality::Text]);
        provider.dialect = dialect;
        // The route this fixture serves is count-tokens, which the Messages capability covers.
        // Without the declaration the capability gate answers `unsupported_endpoint` before any
        // upstream is dialled, which is the behaviour the gate's own tests pin.
        provider.capabilities = vec![Capability::Messages];
        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        // A vault in a private temporary directory, holding the one credential the account names.
        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        AppState {
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
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        }
    }

    /// An Anthropic step's count goes to the upstream's own count-tokens endpoint.
    ///
    /// The path comes from the dialect, because the route is the one that names it: a count asked
    /// on `/v1/messages` would be a generation.
    #[test]
    fn a_count_tokens_request_for_an_anthropic_step_goes_to_the_count_tokens_path() {
        let path = upstream_path(Dialect::Anthropic, RouteKind::CountTokens, None, None);
        assert_eq!(path, UpstreamPath::Dialect("/v1/messages/count_tokens"));
        assert_eq!(
            build_url(&step(Dialect::Anthropic, "https://api.anthropic.com"), path).as_deref(),
            Some("https://api.anthropic.com/v1/messages/count_tokens")
        );
    }

    /// A translator's path names a generation endpoint, so it cannot be allowed to redirect a count.
    #[test]
    fn a_translator_path_cannot_redirect_a_count_to_a_generation_endpoint() {
        let path = upstream_path(
            Dialect::Anthropic,
            RouteKind::CountTokens,
            Some("messages"),
            None,
        );
        assert_eq!(path, UpstreamPath::Dialect("/v1/messages/count_tokens"));
    }

    /// An Anthropic client talking to an Anthropic upstream is a passthrough, so nothing turns the
    /// count into a translated body, a translated path, or a stream.
    #[test]
    fn a_count_tokens_request_is_neither_translated_nor_streamed() {
        let ctx = TranslateCtx {
            route: RouteKind::CountTokens,
            client_dialect: Dialect::Anthropic,
            upstream_dialect: Dialect::Anthropic,
            upstream_model: "m",
            client_streaming: false,
            session_id: None,
        };
        let body = br#"{"model":"m","messages":[{"role":"user","content":"hello"}]}"#;
        // No translation at all: the body goes upstream as the client wrote it, so there is no
        // translator path to override the count-tokens path and no `force_stream` to apply.
        assert!(translate_request(&ctx, body).unwrap().is_none());
        // And the client did not ask for a stream, so the relay does not either.
        assert!(!scan_request_head(body).wants_stream());
    }

    /// The upstream counts the tokens, and its answer is the client's answer.
    #[tokio::test]
    async fn an_anthropic_upstream_is_asked_to_count_the_tokens() {
        let (base_url, seen) = counting_upstream(StatusCode::OK, r#"{"input_tokens":42}"#).await;
        let state = count_tokens_state(Dialect::Anthropic, &base_url);
        let response = dispatch(
            state,
            Dialect::Anthropic,
            Method::POST,
            Uri::from_static("/v1/messages/count_tokens"),
            headers(&[]),
            AxumBody::from(count_tokens_body("p/m")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        // A count that came from the provider is not an estimate, and does not say it is.
        assert!(response
            .headers()
            .get(crate::count_tokens::ESTIMATE_HEADER)
            .is_none());
        // The upstream's answer is returned as it arrived.
        assert_eq!(body_text(response).await, r#"{"input_tokens":42}"#);

        // It was asked on the count-tokens path, with the client's own content.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "expected exactly one upstream call");
        assert_eq!(seen[0].0, "/v1/messages/count_tokens");
        let body: serde_json::Value = serde_json::from_str(&seen[0].1).unwrap();
        assert_eq!(body["messages"][0]["content"], "hello");
        // The upstream sees its own model id, not the client-facing one.
        assert_eq!(body["model"], "m");
        // A count is not a generation: nothing asked for a stream.
        assert!(
            body.get("stream").is_none(),
            "the count was turned into a stream"
        );
    }

    /// A non-Anthropic upstream cannot count, so it is not asked at all.
    ///
    /// A generation would have been billed, and its answer would have been the wrong shape.
    #[tokio::test]
    async fn a_non_anthropic_upstream_is_not_called_and_the_count_is_estimated() {
        let (base_url, seen) = counting_upstream(StatusCode::OK, r#"{"input_tokens":42}"#).await;
        let state = count_tokens_state(Dialect::OpenAi, &base_url);
        let response = dispatch(
            state,
            Dialect::Anthropic,
            Method::POST,
            Uri::from_static("/v1/messages/count_tokens"),
            headers(&[]),
            AxumBody::from(count_tokens_body("p/m")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(crate::count_tokens::ESTIMATE_HEADER)
                .expect("the estimate marker was missing")
                .to_str()
                .unwrap(),
            "true"
        );
        // The body is exactly the Anthropic count shape: one field, no more.
        let text = body_text(response).await;
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 1);
        // The whole body is counted — the container around the text is bytes too. The body is 62
        // bytes, and 62 / 4 rounds up to 16.
        assert_eq!(value["input_tokens"], 16);

        // Nothing was sent upstream.
        assert!(
            seen.lock().unwrap().is_empty(),
            "the count was asked of an upstream that cannot count it"
        );
    }

    /// An upstream that cannot count is one thing; an upstream that *fails* is another.
    ///
    /// The failure is reported as it happened — the upstream's own status and body — because
    /// replacing it with an estimate would hide a broken route behind a plausible answer.
    #[tokio::test]
    async fn an_anthropic_upstream_that_fails_is_reported_as_a_failure() {
        let (base_url, seen) =
            counting_upstream(StatusCode::NOT_FOUND, r#"{"error":"no such endpoint"}"#).await;
        let state = count_tokens_state(Dialect::Anthropic, &base_url);
        let response = dispatch(
            state,
            Dialect::Anthropic,
            Method::POST,
            Uri::from_static("/v1/messages/count_tokens"),
            headers(&[]),
            AxumBody::from(count_tokens_body("p/m")),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        // The failure was not dressed up as an estimate.
        assert!(response
            .headers()
            .get(crate::count_tokens::ESTIMATE_HEADER)
            .is_none());
        assert_eq!(body_text(response).await, r#"{"error":"no such endpoint"}"#);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------------------------
    // Winner recovery
    // -----------------------------------------------------------------------------------------

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use mr_core::dialect::{MidstreamPolicy, WinnerFailMode};

    /// A fake upstream that writes part of a body and then dies.
    ///
    /// The socket is closed without the rest of the declared body, which is what an upstream that
    /// dies mid-stream looks like from the relay's side: the bytes it did send arrive, and then
    /// the body errors. A raw socket is used rather than an axum handler because the point of the
    /// fixture is *where* the stream stops, and an axum handler cannot stop in the middle of a
    /// chunked body.
    async fn dying_upstream(
        delay_ms: u64,
        body: String,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        upstream_writing(delay_ms, body, false).await
    }

    /// An upstream that writes `body` and then ends the response properly.
    ///
    /// The declared length is exact, so the client sees an end-of-stream rather than an error: this
    /// is what a provider that stops mid-event looks like when nothing on the wire reports a fault.
    async fn ending_upstream(
        delay_ms: u64,
        body: String,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        upstream_writing(delay_ms, body, true).await
    }

    async fn upstream_writing(
        delay_ms: u64,
        body: String,
        ends_cleanly: bool,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                read_request(&mut socket, &mut request).await;
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).to_string());
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                // Unless the upstream ends cleanly, the declared length is longer than what is sent,
                // so the body ends in an error rather than in an end-of-stream.
                let declared = if ends_cleanly {
                    body.len()
                } else {
                    body.len() + 64
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\n\r\n{body}"
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.flush().await;
                // Close without the rest of the body: the client's body errors.
                drop(socket);
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// Read one HTTP request: the headers, and then as many body bytes as they declare.
    async fn read_request(socket: &mut tokio::net::TcpStream, request: &mut Vec<u8>) {
        let mut chunk = [0u8; 4096];
        loop {
            let Ok(read) = socket.read(&mut chunk).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(request);
            let Some(split) = text.find("\r\n\r\n") else {
                continue;
            };
            let length = text[..split]
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            if request.len() >= split + 4 + length {
                return;
            }
        }
    }

    /// A fake upstream that answers with an OpenAI event stream.
    async fn sse_upstream(
        delay_ms: u64,
        content: &'static str,
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push(String::from_utf8_lossy(&body).to_string());
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                let chunk = format!(
                    "data: {{\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":\
                     [{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{content}\"}}}}]}}\n\n"
                );
                let stream = futures_util::stream::iter(vec![
                    Ok::<Bytes, std::io::Error>(Bytes::from(chunk)),
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(b"data: [DONE]\n\n")),
                ]);
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    AxumBody::from_stream(stream),
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

    /// A provider that speaks a particular dialect.
    fn provider_speaking(
        id: &str,
        dialect: Dialect,
        base_url: &str,
        model: &str,
        modalities: &[InputModality],
    ) -> mr_core::model::Provider {
        let mut provider = mr_core::model::Provider::new(id, dialect, base_url);
        let mut account = mr_core::model::Account::new(id);
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        let mut entry = dm();
        entry.id = model.to_owned();
        entry.input_modalities = modalities.to_vec();
        provider.models = vec![entry];
        provider
    }

    /// What the recovery fixture should look like.
    #[derive(Clone)]
    struct RecoverySpec {
        /// How long the winner (the higher-priority participant) takes to answer.
        winner_delay_ms: u64,
        /// What the winner writes before it dies.
        winner_body: String,
        /// The dialect the winner's provider speaks.
        winner_dialect: Dialect,
        /// How long the replacement (the lower-priority participant) takes to answer.
        replacement_delay_ms: u64,
        /// The dialect the replacement's provider speaks.
        replacement_dialect: Dialect,
        /// The combo's mid-stream policy.
        policy: MidstreamPolicy,
        /// The configured winner-fail mode.
        on_winner_fail: WinnerFailMode,
        /// Whether the winner's body ends in an error (`true`) or in a clean end-of-stream.
        ///
        /// A clean end is how a provider that simply stops writing looks, and it is the case no
        /// failover can recover: there is no error to react to, only a truncated tail on the wire.
        winner_ends_cleanly: bool,
    }

    impl Default for RecoverySpec {
        fn default() -> Self {
            Self {
                winner_delay_ms: 120,
                winner_body:
                    "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":\
                     [{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"winner\"}}]}\n\n"
                        .to_owned(),
                winner_dialect: Dialect::OpenAi,
                replacement_delay_ms: 40,
                replacement_dialect: Dialect::OpenAi,
                policy: MidstreamPolicy::Restart,
                on_winner_fail: WinnerFailMode::PickHighestPassed,
                winner_ends_cleanly: false,
            }
        }
    }

    /// A relay whose only route is a two-step combo whose winner dies mid-stream.
    ///
    /// The replacement answers *before* the winner does, and the winner is the higher-priority
    /// participant, so the winner is committed on priority and the replacement is already in the
    /// race's result set when it is. That is the situation `pick-highest-passed` is for.
    async fn recovery_state(spec: RecoverySpec) -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
        let winner_body = spec.winner_body.clone();
        let winner = if spec.winner_ends_cleanly {
            ending_upstream(spec.winner_delay_ms, winner_body).await
        } else {
            dying_upstream(spec.winner_delay_ms, winner_body).await
        };
        let (winner_url, winner_seen) = winner;
        let (replacement_url, _replacement_seen) =
            sse_upstream(spec.replacement_delay_ms, "second").await;

        let mut combo = mr_core::model::Combo::new(
            "race",
            mr_core::model::ComboStep::new("first", "race-model"),
        );
        combo
            .steps
            .push(mr_core::model::ComboStep::new("second", "race-model"));
        combo.hedge = mr_core::model::HedgeSpec {
            enabled: true,
            participants: vec![0, 1],
            window_ms: 2_000,
            on_winner_fail: spec.on_winner_fail,
            ..Default::default()
        };
        combo.midstream_policy = spec.policy;

        let config = mr_core::config::Config {
            providers: vec![
                provider_speaking(
                    "first",
                    spec.winner_dialect,
                    &winner_url,
                    "race-model",
                    &[InputModality::Text],
                ),
                provider_speaking(
                    "second",
                    spec.replacement_dialect,
                    &replacement_url,
                    "race-model",
                    &[InputModality::Text],
                ),
            ],
            combos: vec![combo],
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, winner_seen)
    }

    /// A streaming request for the combo the recovery fixture serves.
    fn streaming_body() -> String {
        serde_json::json!({
            "model": "race",
            "messages": [{"role": "user", "content": "hello"}],
            "stream": true,
        })
        .to_string()
    }

    /// Every frame the client received, and the error that ended the stream.
    async fn drain(response: Response) -> (String, Option<String>) {
        let mut body = response.into_body();
        let mut text = String::new();
        loop {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Some(data) = frame.data_ref() {
                        text.push_str(&String::from_utf8_lossy(data));
                    }
                }
                Some(Err(err)) => return (text, Some(err.to_string())),
                None => return (text, None),
            }
        }
    }

    #[tokio::test]
    async fn a_winner_that_dies_before_anything_reaches_the_client_is_replaced_silently() {
        // The winner's provider speaks another dialect, so the relay translates its frames — and
        // the frame it sent is a partial line, which the translator is still holding. Nothing
        // reaches the client, so the switch is invisible.
        let spec = RecoverySpec {
            winner_dialect: Dialect::Anthropic,
            winner_body: "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":\
                          {\"type\":\"text_delta\",\"text\":\"partial\"}}"
                .to_owned(),
            ..RecoverySpec::default()
        };
        let (state, _seen) = recovery_state(spec).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(streaming_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let (text, error) = drain(response).await;
        assert_eq!(error, None, "the stream ended in an error: {text}");
        assert!(
            text.contains("second"),
            "the client did not receive the replacement's answer: {text}"
        );
        assert!(
            !text.contains("manarouter.retry"),
            "the switch was announced, but nothing had reached the client yet: {text}"
        );
    }

    /// Half an event must never reach the client, whether it is left dangling or spliced.
    ///
    /// A body arrives in chunks and a chunk boundary falls anywhere, including mid-`data:` line.
    /// Forwarding that fragment is harmless while one upstream keeps writing — the client buffers
    /// until the line ends. It stops being harmless the moment the relay splices, because the retry
    /// boundary starts with `event:`, so a dangling line is concatenated with it. The client then
    /// receives one line that is not JSON at all, which is the reported failure: "malformed
    /// server-sent event JSON" whenever a combo step died mid-frame.
    #[tokio::test]
    async fn a_winner_that_dies_mid_event_never_hands_the_client_half_a_frame() {
        let spec = RecoverySpec {
            // The dead model's last bytes stop inside the delta: no closing brace, no newline.
            winner_body: "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\
                          \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\
                          \"content\":\"half an answer\""
                .to_owned(),
            ..RecoverySpec::default()
        };
        let (state, _seen) = recovery_state(spec).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(streaming_body()),
        )
        .await;

        let (text, error) = drain(response).await;
        assert_eq!(error, None, "the stream ended in an error: {text}");

        // Every payload the client is asked to parse is a JSON document, and the splice happened.
        assert!(text.contains("second"), "no replacement answer: {text}");
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload == "[DONE]" {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(payload).unwrap_or_else(|e| {
                panic!(
                    "the client was handed a malformed data line ({e}): {payload:?}\nin:\n{text}"
                )
            });
        }

        // The fragment itself is dropped: it is not an event, so nothing reconstructable is lost.
        assert!(
            !text.contains("half an answer"),
            "a partial event reached the client: {text}"
        );
    }

    #[tokio::test]
    async fn a_winner_that_dies_mid_stream_is_announced_and_replaced() {
        let (state, _seen) = recovery_state(RecoverySpec::default()).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(streaming_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let (text, error) = drain(response).await;
        assert_eq!(error, None, "the stream ended in an error: {text}");

        // The dead model's own bytes are still there: the client was already reading them.
        assert!(text.contains("winner"), "{text}");
        // The boundary says which model died and which one takes over.
        assert!(text.contains("event: manarouter.retry"), "{text}");
        assert!(text.contains(r#""reason":"upstream_failed""#), "{text}");
        assert!(text.contains(r#""from":"first/race-model""#), "{text}");
        assert!(text.contains(r#""to":"second/race-model""#), "{text}");
        // The dead model's turn is closed, in the client's dialect.
        assert!(
            text.contains(r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}]}"#),
            "{text}"
        );
        // And the replacement's answer follows it.
        assert!(text.contains("second"), "{text}");
    }

    /// A stream that simply stops mid-event, with nothing on the wire reporting a fault.
    ///
    /// There is no error to fail over on, so the only thing that matters is what the client is
    /// handed: a truncated `data:` line at end-of-stream is exactly what the OpenAI client parses
    /// and rejects ("malformed server-sent event JSON"), with no fault anywhere else to blame.
    #[tokio::test]
    async fn a_stream_that_ends_inside_an_event_drops_the_fragment() {
        let spec = RecoverySpec {
            winner_ends_cleanly: true,
            // Two whole events, then a third that stops inside the content string.
            winner_body: "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\
                          \"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\
                          \"content\":\"whole\"}}]}\n\n\
                          data: {\"id\":\"cmpl-2\",\"object\":\"chat.completion.chunk\",\
                          \"choices\":[{\"index\":0,\"delta\":{\"content\":\"half an answer\""
                .to_owned(),
            ..RecoverySpec::default()
        };
        let (state, _seen) = recovery_state(spec).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(streaming_body()),
        )
        .await;

        let (text, error) = drain(response).await;
        assert_eq!(error, None, "a clean end should not be an error: {text}");
        assert!(text.contains("whole"), "the whole event was lost: {text}");
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data: ") else {
                continue;
            };
            if payload == "[DONE]" {
                continue;
            }
            serde_json::from_str::<serde_json::Value>(payload).unwrap_or_else(|e| {
                panic!(
                    "the client was handed a malformed data line ({e}): {payload:?}\nin:\n{text}"
                )
            });
        }
        assert!(
            !text.contains("half an answer"),
            "the truncated event reached the client: {text}"
        );
    }

    #[tokio::test]
    async fn the_terminate_policy_ends_the_stream_with_an_error() {
        let spec = RecoverySpec {
            policy: MidstreamPolicy::Terminate,
            ..RecoverySpec::default()
        };
        let (state, _seen) = recovery_state(spec).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            AxumBody::from(streaming_body()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let (text, error) = drain(response).await;
        let error = error.expect("the stream should have ended in an error");
        assert!(error.contains("could not be recovered"), "{error}");
        // What the client had already received is still there; the replacement is not.
        assert!(text.contains("winner"), "{text}");
        assert!(!text.contains("second"), "{text}");
        assert!(!text.contains("manarouter.retry"), "{text}");
    }

    #[tokio::test]
    async fn a_tool_call_delta_stops_the_splice_under_both_policies() {
        for policy in [MidstreamPolicy::Restart, MidstreamPolicy::Terminate] {
            let spec = RecoverySpec {
                policy,
                winner_body: "data: {\"id\":\"cmpl-1\",\"object\":\"chat.completion.chunk\",\
                              \"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\
                              \"call_1\",\"function\":{\"name\":\"do_it\",\"arguments\":\"{\"}]}}]}}\n\n"
                    .to_owned(),
                ..RecoverySpec::default()
            };
            let (state, _seen) = recovery_state(spec).await;

            let response = dispatch(
                state,
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                headers(&[]),
                AxumBody::from(streaming_body()),
            )
            .await;

            let (text, error) = drain(response).await;
            let error = error.expect("the stream should have ended in an error");
            assert!(error.contains("could not be recovered"), "{error}");
            // The tool call that was already emitted is still on the wire, and nothing was
            // spliced onto it.
            assert!(text.contains("tool_calls"), "{text}");
            assert!(!text.contains("second"), "{text}");
            assert!(!text.contains("manarouter.retry"), "{text}");
        }
    }

    #[tokio::test]
    async fn an_unimplemented_winner_fail_mode_still_answers_and_is_reported() {
        let spec = RecoverySpec {
            on_winner_fail: WinnerFailMode::RerunExcluding { max_rounds: 2 },
            ..RecoverySpec::default()
        };
        let (state, _seen) = recovery_state(spec).await;

        // `rerun-excluding` is now implemented: it should not produce an "unimplemented" warning.
        let log = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(LogWriter(Arc::clone(&log)))
            .finish();

        let response = {
            let _guard = tracing::subscriber::set_default(subscriber);
            dispatch(
                state,
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                headers(&[]),
                AxumBody::from(streaming_body()),
            )
            .await
        };

        assert_eq!(response.status(), StatusCode::OK);
        let (text, error) = drain(response).await;
        assert_eq!(error, None, "the stream ended in an error: {text}");
        assert!(text.contains("second"), "{text}");

        let logged = String::from_utf8_lossy(&log.lock().unwrap()).to_string();
        assert!(
            !logged.contains("not implemented yet"),
            "rerun-excluding is implemented and should not warn: {logged}"
        );
    }

    // -----------------------------------------------------------------------------------------
    // A provider with several accounts, only some of which have a credential
    // -----------------------------------------------------------------------------------------

    /// A fake upstream that records the `Authorization` header of every call.
    ///
    /// The header is the only place an OpenAI-compatible upstream sees which credential it was
    /// given, so recording it is how a test proves *which* account served a request rather than
    /// merely that the request was served.
    async fn upstream_recording_auth() -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |request_headers: HeaderMap| {
            let sink = Arc::clone(&sink);
            async move {
                let presented = request_headers
                    .get(http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                sink.lock().unwrap().push(presented);
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"id":"cmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
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

    /// A state whose provider holds two accounts at the same priority.
    ///
    /// `stored` names the accounts whose credential is actually in the vault, so a test can build
    /// the reported shape — the first account has no credential, the second does — and the
    /// degenerate one where neither has anything attached.
    async fn two_account_state(
        stored: &[&str],
    ) -> (
        AppState,
        Arc<std::sync::Mutex<Vec<String>>>,
        tempfile::TempDir,
    ) {
        two_account_state_with(stored, mr_core::dialect::KeyStrategy::Priority).await
    }

    /// The same, with the account strategy the test is about.
    ///
    /// The strategy is all these tests vary, so it is a parameter rather than a second fixture that
    /// could drift from this one in the vault setup.
    async fn two_account_state_with(
        stored: &[&str],
        strategy: mr_core::dialect::KeyStrategy,
    ) -> (
        AppState,
        Arc<std::sync::Mutex<Vec<String>>>,
        tempfile::TempDir,
    ) {
        let (url, seen) = upstream_recording_auth().await;
        let mut provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        provider.key_strategy = strategy;

        // The configuration order is the priority order, so the account the operator listed first
        // is the one the bug stopped at.
        let names = ["first", "second"];
        provider.accounts = names
            .iter()
            .map(|name| {
                let mut account = mr_core::model::Account::new(*name);
                account.secret_ref = Some(format!("{name}-credential"));
                account
            })
            .collect();

        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        for name in stored {
            opened
                .put(&format!("{name}-credential"), &format!("sk-{name}"))
                .unwrap();
        }
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, seen, dir)
    }

    fn two_account_body() -> AxumBody {
        AxumBody::from(
            serde_json::json!({
                "model": "p/m",
                "messages": [{"role": "user", "content": "hello"}],
            })
            .to_string(),
        )
    }

    /// The reported bug: the first account has no credential and the second does.
    ///
    /// A missing secret is a property of one account, not a failure of the step. The relay must
    /// walk to the sibling that has one, exactly as the operator's ordering says.
    #[tokio::test]
    async fn an_account_without_a_credential_does_not_stop_the_step() {
        let (state, seen, _dir) = two_account_state(&["second"]).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            two_account_body(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let called = seen.lock().unwrap().clone();
        assert_eq!(
            called.len(),
            1,
            "one attempt, on the usable sibling: {called:?}"
        );
        assert_eq!(
            called[0], "Bearer sk-second",
            "the request must carry the sibling's credential"
        );
    }

    /// The same, with both accounts usable: the operator's first choice still wins.
    ///
    /// Falling through must not become "skip the first account": the ordering is the operator's
    /// decision, and it is only the *unusable* entries that are stepped over.
    #[tokio::test]
    async fn an_account_with_a_credential_is_still_preferred_when_the_first_one_has_one() {
        let (state, seen, _dir) = two_account_state(&["first", "second"]).await;

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            two_account_body(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let called = seen.lock().unwrap().clone();
        assert_eq!(called, vec!["Bearer sk-first".to_owned()]);
    }

    /// Round-robin must take turns: a second request may not land on the first account again.
    ///
    /// Both accounts hold a credential and the operator declared `round-robin`, so nothing but the
    /// rotation counter decides. The regression this guards is that the counter was keyed by a
    /// synthetic `provider#model` id the routing table never allocated: the lookup missed, the
    /// counter read `0` on every request, and rotating by zero is the identity. The account choice
    /// was therefore byte-for-byte `priority` — every account after the first was dead weight no
    /// matter how healthy or how recently used — and the only visible symptom was a request served
    /// successfully by the wrong account.
    #[tokio::test]
    async fn round_robin_accounts_take_turns_across_requests() {
        let (state, seen, _dir) = two_account_state_with(
            &["first", "second"],
            mr_core::dialect::KeyStrategy::RoundRobin,
        )
        .await;

        for _ in 0..3 {
            let response = dispatch(
                state.clone(),
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                headers(&[]),
                two_account_body(),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }

        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                "Bearer sk-first".to_owned(),
                "Bearer sk-second".to_owned(),
                "Bearer sk-first".to_owned(),
            ],
            "three requests must cycle the two accounts, not pin the first"
        );
    }

    /// A credential stored while the relay is running must be usable without a restart.
    ///
    /// This is the shape the panel produces: the relay starts, the operator adds a provider, then
    /// pastes its key. The relay's vault is a snapshot from startup, so the account reported "no
    /// credential in the vault" — accurately, about a file it had not looked at again — until the
    /// process was restarted. The miss path re-reads it now, which is what this proves: the state's
    /// own vault does not hold the secret, the file does, and the request goes through.
    #[tokio::test]
    async fn a_credential_stored_after_startup_is_used_without_a_restart() {
        let (state, seen, dir) = two_account_state(&[]).await;

        // Standing in for `mr account add` in another process: a second handle writes the secret
        // and the relay's own snapshot never saw it.
        {
            let mut writer = mr_core::secret::Vault::open(dir.path()).unwrap();
            writer.put("second-credential", "sk-late").unwrap();
            writer.flush().unwrap();
        }
        assert!(
            state.vault.get("second-credential").unwrap().is_none(),
            "the relay's snapshot must not contain the late credential, or this proves nothing"
        );

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            two_account_body(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec!["Bearer sk-late".to_owned()]
        );
    }

    /// When no account has a credential the refusal must name that, not a cooldown.
    ///
    /// The distinction is the whole report: "cooling down" sends the operator to look at health
    /// and wait, while "no credential" sends them to the account that needs one. Nothing is
    /// cooled down either — a secret does not reappear with time, so excluding the accounts from
    /// later requests would hide the problem rather than surface it.
    #[tokio::test]
    async fn a_step_where_no_account_has_a_credential_says_so() {
        let (state, seen, _dir) = two_account_state(&[]).await;
        let health = Arc::clone(&state.health);

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            headers(&[]),
            two_account_body(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            seen.lock().unwrap().is_empty(),
            "the upstream must not be called"
        );
        let (text, _) = drain(response).await;
        assert!(
            text.contains("no credential") || text.contains("no usable credential"),
            "the refusal must name the missing credential: {text}"
        );
        assert!(
            !text.contains("cooling down"),
            "a missing credential is not a cooldown: {text}"
        );
        assert!(
            health.view().is_empty(),
            "a missing credential must not put an account in cooldown: {:?}",
            health.view()
        );
    }

    /// Every skip cause reaches the operator in its own words, and the wait is not invented.
    ///
    /// The regression this pins: all four causes used to produce "every step for this model has an
    /// account cooling down or unavailable", which is wrong for three of them and sends the
    /// operator to look at health state that is not the problem.
    #[test]
    fn every_skip_cause_is_named_in_the_terminal_refusal() {
        let cooldown = skipped_steps_message(&[SkipCause::NoUsableAccount], Some(9));
        assert!(
            cooldown.contains("no account for this model is usable"),
            "{cooldown}"
        );
        assert!(
            cooldown.contains("earliest account recovery is in"),
            "{cooldown}"
        );

        let stale = skipped_steps_message(&[SkipCause::ProviderMissing], Some(9));
        assert!(stale.contains("no longer holds a provider"), "{stale}");
        assert!(stale.contains("reload the snapshot"), "{stale}");
        assert!(
            !stale.contains("recovery is in"),
            "a provider that is not in the snapshot does not recover with time: {stale}"
        );

        let url = skipped_steps_message(&[SkipCause::UnbuildableUrl], None);
        assert!(url.contains("absolute URL"), "{url}");

        let modality = skipped_steps_message(&[SkipCause::UnsupportedModality], Some(30));
        assert!(modality.contains("input modality"), "{modality}");
        assert!(!modality.contains("recovery is in"), "{modality}");
    }

    /// Two steps skipped for the same reason read as one reason, not two.
    #[test]
    fn repeated_skip_causes_are_stated_once() {
        let message = skipped_steps_message(
            &[
                SkipCause::NoUsableAccount,
                SkipCause::ProviderMissing,
                SkipCause::NoUsableAccount,
            ],
            None,
        );
        assert_eq!(
            message
                .matches("no account for this model is usable")
                .count(),
            1,
            "{message}"
        );
        assert_eq!(
            message.matches("no longer holds a provider").count(),
            1,
            "{message}"
        );
    }

    /// A request with no recorded cause still gets a sentence rather than an empty message.
    #[test]
    fn an_empty_cause_list_still_explains_itself() {
        let message = skipped_steps_message(&[], None);
        assert!(message.contains("skipped"), "{message}");
        assert!(!message.is_empty());
    }

    /// The message counts both causes, because one step can mix them.
    #[test]
    fn the_refusal_counts_missing_and_unusable_accounts_separately() {
        let config = mr_core::config::Config {
            providers: vec![provider_serving(
                "p",
                "http://127.0.0.1:1",
                "m",
                &[InputModality::Text],
            )],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let provider = &snapshot.providers()[0];

        let one = no_credential_message(provider, "m", 1, 0);
        assert!(one.contains("1 account has no credential"), "{one}");

        let many = no_credential_message(provider, "m", 3, 2);
        assert!(many.contains("3 accounts have no credential"), "{many}");
        assert!(
            many.contains("2 accounts' credentials cannot be used"),
            "{many}"
        );

        // The suggestion names the provider, so it is actionable without reading the config.
        assert!(many.contains("mr account add p --secret-stdin"), "{many}");
    }

    // -----------------------------------------------------------------------------------------
    // A provider that needs no credential
    // -----------------------------------------------------------------------------------------

    /// A state with an open vault and no routes: enough to resolve one account's credential.
    fn credential_state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        // The vault is empty on purpose: this is the provider that needs no credential, and a
        // credential that happens to be stored would hide exactly the case under test.
        let vault = Arc::new(mr_core::secret::Vault::open(dir.path()).unwrap());

        let config = mr_core::config::Config::default();
        let relay = Arc::new(RelayState::new(
            &config,
            mr_core::snapshot::RoutingSnapshot::default(),
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        AppState {
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
        }
    }

    /// A compiled account with no OAuth reference and (when asked) no vault reference at all.
    ///
    /// Compiled through the snapshot so the plan under test has the shape the relay actually sees,
    /// rather than a hand-built literal that could drift from it.
    fn compiled_account(secret_ref: Option<&str>) -> mr_core::snapshot::AccountPlan {
        let mut provider =
            mr_core::model::Provider::new("local", Dialect::OpenAi, "http://127.0.0.1:11434/v1");
        let mut account = mr_core::model::Account::new("local");
        // `Account::new` leaves `secret_ref` as `None`, which is how "this provider needs no
        // credential" is expressed; the compiler then resolves the conventional vault slot for it.
        if let Some(id) = secret_ref {
            account.secret_ref = Some(id.to_owned());
        }
        provider.accounts = vec![account];

        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        snapshot.providers()[0].accounts[0].clone()
    }

    /// One step over the keyless local provider, differing only in how it attaches a credential.
    fn compiled_step(auth: AuthKind) -> StepPlan {
        let mut provider =
            mr_core::model::Provider::new("local", Dialect::OpenAi, "http://127.0.0.1:11434/v1");
        provider.auth = auth;
        let mut entry = dm();
        entry.id = "m".into();
        entry.input_modalities = vec![InputModality::Text];
        provider.models = vec![entry];

        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        snapshot.providers()[0].steps[0].clone()
    }

    /// A keyless account on a Bearer provider is usable, and the request carries no credential.
    ///
    /// This is the local llama.cpp / Ollama case: the provider was deliberately configured with an
    /// account that has no secret, because the upstream wants no credential. It used to fail with
    /// `429 no_usable_accounts ... 1 account has no credential in the vault`, which told the
    /// operator to attach a key to a provider that must not have one.
    #[tokio::test]
    async fn a_bearer_step_serves_an_account_with_no_credential_at_all() {
        let app = credential_state();
        let step = compiled_step(AuthKind::Bearer);
        // No reference in the configuration at all, which the compiler resolves to the conventional
        // slot; nothing is stored under it and nothing is expected to be.
        let account = compiled_account(None);
        let conventional = format!("provider/local/account/{}", account.id);
        assert_eq!(
            account.secret_ref.as_deref(),
            Some(conventional.as_str()),
            "the compiler must resolve the conventional slot for this to be the case under test"
        );

        match account_credential(&app, &step, &account).await {
            AccountCredential::Usable {
                credential,
                oauth_headers,
                oauth_tokens,
            } => {
                assert!(
                    credential.is_none(),
                    "a keyless account must send no credential, not an empty one"
                );
                assert!(oauth_headers.is_empty());
                assert!(oauth_tokens.is_none());
            }
            _ => panic!("a keyless account on a Bearer provider must be usable"),
        }
    }

    /// An account with a vault slot of its own is not keyless, even when the step is a Bearer one.
    ///
    /// This is the other half of the rule, and the half the bug report's provider does not have: an
    /// account that names its own slot is one somebody is still going to fill in, so the step must
    /// step over it rather than send a credential-less request in its name.
    #[tokio::test]
    async fn a_bearer_step_does_not_serve_an_account_that_names_its_own_empty_slot() {
        let app = credential_state();
        let step = compiled_step(AuthKind::Bearer);
        let account = compiled_account(Some("second-credential"));

        assert!(
            matches!(
                account_credential(&app, &step, &account).await,
                AccountCredential::Unusable(CredentialMiss::Missing)
            ),
            "an account with its own empty slot must still be reported as missing"
        );
    }

    /// Every other auth kind still refuses an account with nothing attached.
    ///
    /// Those kinds exist to carry a value — a header, a query parameter, a Modal pair, an OAuth
    /// platform — so an account with no credential is a configuration error rather than a choice,
    /// and the refusal has to stay so the operator is still sent to the account that needs a key.
    /// The accounts here are the keyless ones, so **only** the auth kind can be doing the refusing.
    #[tokio::test]
    async fn only_bearer_serves_an_account_with_no_credential() {
        let app = credential_state();
        let kinds = [
            AuthKind::ApiKeyHeader {
                header: "x-api-key".into(),
            },
            AuthKind::Header {
                name: "x-goog-api-key".into(),
                prefix: String::new(),
            },
            AuthKind::QueryParam {
                param: "key".into(),
            },
            AuthKind::Modal,
        ];
        for auth in kinds {
            let step = compiled_step(auth.clone());
            assert!(
                matches!(
                    account_credential(&app, &step, &compiled_account(None)).await,
                    AccountCredential::Unusable(CredentialMiss::Missing)
                ),
                "{auth:?} accepted an account with no credential attached"
            );
        }
        assert!(auth_tolerates_no_credential(&AuthKind::Bearer));
        assert!(!auth_tolerates_no_credential(&AuthKind::ApiKeyHeader {
            header: "x-api-key".into(),
        }));
    }

    /// A keyless local provider serving model `m`, with an empty vault.
    ///
    /// The account is left without a `secret_ref`, which is how "this provider needs no credential"
    /// is written in a configuration, and the model is named so a request can route to it.
    fn keyless_provider() -> mr_core::model::Provider {
        let mut provider =
            mr_core::model::Provider::new("local", Dialect::OpenAi, "http://127.0.0.1:1");
        provider.accounts = vec![mr_core::model::Account::new("local")];
        let mut entry = dm();
        entry.id = "m".into();
        entry.input_modalities = vec![InputModality::Text];
        provider.models = vec![entry];
        provider
    }

    /// A relay whose only route is one keyless account, over `upstream_recording_auth`.
    ///
    /// That upstream records the `Authorization` header each request carried (empty when it carried
    /// none), so a test can check the wire and not only the status code. A non-empty `base_url`
    /// points the same provider somewhere nothing listens, for the test that expects a refusal.
    async fn keyless_state(
        auth: AuthKind,
        base_url: Option<&str>,
    ) -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
        let (url, seen) = upstream_recording_auth().await;
        let mut provider = keyless_provider();
        provider.auth = auth;
        provider.base_url = base_url.map(str::to_owned).unwrap_or(url);
        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let compiled = snapshot.providers()[0].accounts[0]
            .secret_ref
            .clone()
            .unwrap_or_default();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        // Deliberately empty, and checked empty: the conventional slot exists in the compiled plan
        // but nothing is stored under it, which is the state the operator reported as unusable.
        let vault = Arc::new(mr_core::secret::Vault::open(dir.path()).unwrap());
        assert!(
            vault.get(&compiled).unwrap().is_none(),
            "this fixture is only meaningful with nothing stored for the keyless account"
        );

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
        (state, seen)
    }

    /// The whole request path serves a keyless Bearer account instead of returning 429.
    ///
    /// The unit tests above pin the resolution; this one pins the consequence the operator saw, so a
    /// later change that reintroduced the refusal anywhere between the vault and the upstream fails
    /// here rather than in the field. It also checks the wire: the request must arrive with no
    /// `Authorization` header at all, rather than with an empty one.
    #[tokio::test]
    async fn a_request_to_a_keyless_bearer_provider_reaches_the_upstream() {
        let (state, seen) = keyless_state(AuthKind::Bearer, None).await;
        let response = chat(state, "local/m").await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a keyless Bearer account was refused: {}",
            body_text(response).await
        );
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "the request never reached the upstream");
        assert!(
            sent[0].is_empty(),
            "the upstream was sent an Authorization header: {:?}",
            sent[0]
        );
    }

    /// The same request against an account-keyed auth kind still reports the honest 429.
    ///
    /// Without this the test above would pass on the strength of the 429 having been deleted
    /// everywhere rather than only where it was wrong. The provider is otherwise identical, so the
    /// auth kind is the only difference between the two outcomes.
    #[tokio::test]
    async fn a_request_to_a_keyless_header_provider_still_reports_no_usable_accounts() {
        let (state, seen) = keyless_state(
            AuthKind::ApiKeyHeader {
                header: "x-api-key".into(),
            },
            Some("http://127.0.0.1:1"),
        )
        .await;

        let response = chat(state, "local/m").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = body_text(response).await;
        assert!(body.contains("no_usable_accounts"), "{body}");
        assert!(body.contains("no credential"), "{body}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "nothing should go upstream"
        );
    }

    /// A tracing writer that keeps what it is given, so a test can read the log.
    #[derive(Clone)]
    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogWriter {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    // -----------------------------------------------------------------------------------------
    // Combo activity
    // -----------------------------------------------------------------------------------------

    /// A relay whose only route is one combo over one fake provider.
    ///
    /// The provider id is the shape a real one takes (`openai-codex`), because the recorded step is
    /// `provider/model` and a fixture with a bare `p/m` would not catch a formatting mistake there.
    async fn combo_state() -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
        let (url, seen) = fake_upstream().await;
        let provider = provider_serving("openai-codex", &url, "gpt-6-luna", &[InputModality::Text]);
        let combo = mr_core::model::Combo::new(
            "expensivesmart",
            mr_core::model::ComboStep::new("openai-codex", "gpt-6-luna"),
        );
        let config = mr_core::config::Config {
            providers: vec![provider],
            combos: vec![combo],
            ..Default::default()
        };

        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
            // A lane with no disk state and no workers: these tests exercise the request path, and a
            // lane that recovered the operator's real batches would be both slow and surprising.
            batch: crate::batch::BatchLane::inert(),
        };
        (state, seen)
    }

    /// A chat request for one model name, in the shape a client sends.
    fn chat_body(model: &str) -> String {
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
        })
        .to_string()
    }

    async fn chat(state: AppState, model: &str) -> Response {
        dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            HeaderMap::new(),
            AxumBody::from(chat_body(model)),
        )
        .await
    }

    #[tokio::test]
    async fn a_combo_request_is_recorded_with_the_step_it_chose_and_stays_open_until_the_body_ends()
    {
        let (state, seen) = combo_state().await;
        let activity = Arc::clone(&state.activity);

        let response = chat(state, "expensivesmart").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);

        let open = activity.snapshot();
        assert_eq!(open.len(), 1, "one combo decision, one entry");
        assert_eq!(open[0].combo, "expensivesmart");
        assert_eq!(open[0].requested, "expensivesmart");
        assert_eq!(open[0].step, "openai-codex/gpt-6-luna");
        assert!(open[0].started_at > 0);
        assert!(
            open[0].finished_at.is_none(),
            "the body has not been read or dropped yet, so the request is still in flight"
        );
        assert!(open[0].outcome.is_none());

        drop(response);

        let closed = activity.snapshot();
        assert!(closed[0].finished_at.is_some(), "the body was dropped");
        assert_eq!(closed[0].outcome, Some(crate::activity::Outcome::Ok));
    }

    #[tokio::test]
    async fn a_plain_model_request_is_not_combo_activity() {
        let (state, seen) = combo_state().await;
        let activity = Arc::clone(&state.activity);

        let response = chat(state, "openai-codex/gpt-6-luna").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);
        drop(response);

        assert!(
            activity.snapshot().is_empty(),
            "a direct model is not a combo decision"
        );
    }

    #[tokio::test]
    async fn the_activity_endpoint_refuses_callers_that_are_not_on_loopback() {
        let (state, _seen) = combo_state().await;
        let response = control_activity(
            ConnectInfo(std::net::SocketAddr::from(([10, 1, 2, 3], 4000))),
            State(state),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn the_activity_endpoint_reports_the_entries_and_the_current_cooldowns() {
        let (state, _seen) = combo_state().await;
        state.health.cool_down(
            "01a18287-8246-7280-b00a-a7be06b51e4d",
            1_793_641_349_000,
            select::CooldownReason::RateLimited,
        );
        // Held for the length of the test: the decision is in flight until the guard drops.
        let _in_flight = state.activity.begin(
            "expensivesmart",
            "expensivesmart",
            "openai-codex/gpt-6-luna",
            1_793_641_349_000,
        );

        let response = control_activity(
            ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 4000))),
            State(state),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(json["entries"][0]["combo"], "expensivesmart");
        assert_eq!(json["entries"][0]["requested"], "expensivesmart");
        assert_eq!(json["entries"][0]["step"], "openai-codex/gpt-6-luna");
        assert_eq!(json["entries"][0]["started_at"], 1_793_641_349_000u64);
        assert!(json["entries"][0]["finished_at"].is_null());
        assert!(json["entries"][0]["outcome"].is_null());

        let cooldown = &json["cooldowns"]["01a18287-8246-7280-b00a-a7be06b51e4d"];
        assert_eq!(cooldown["until_ms"], 1_793_641_349_000u64);
        assert_eq!(cooldown["reason"], "rate_limited");
        assert_eq!(cooldown["failures"], 1);

        // The cooldown block is ids and numbers only. A label, a URL or a secret here would be a
        // disclosure the panel never needed.
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(!text.contains("sk-"), "{text}");
        assert!(!text.contains("http"), "{text}");
    }

    /// A mock upstream that records every request body it receives, and responds according to a closure.
    async fn mock_upstream_custom<F>(handler: F) -> (String, Arc<std::sync::Mutex<Vec<String>>>)
    where
        F: Fn(usize, &str) -> (StatusCode, &'static str) + Send + Sync + 'static,
    {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let handler = Arc::new(handler);

        let app = Router::new().fallback(any(move |body: Bytes| {
            let sink = Arc::clone(&sink);
            let handler = Arc::clone(&handler);
            async move {
                let text = String::from_utf8_lossy(&body).into_owned();
                let mut guard = sink.lock().unwrap();
                let attempt = guard.len();
                guard.push(text.clone());
                drop(guard);

                let (status, resp_body) = handler(attempt, &text);
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    resp_body,
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

    async fn reasoning_test_state<F>(
        handler: F,
    ) -> (
        AppState,
        Arc<std::sync::Mutex<Vec<String>>>,
        tempfile::TempDir,
    )
    where
        F: Fn(usize, &str) -> (StatusCode, &'static str) + Send + Sync + 'static,
    {
        let (url, seen) = mock_upstream_custom(handler).await;
        let mut provider = keyless_provider();
        provider.auth = AuthKind::Bearer;
        provider.base_url = url;
        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(mr_core::secret::Vault::open(dir.path()).unwrap());
        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
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
        (state, seen, dir)
    }

    #[tokio::test]
    async fn reasoning_effort_refusal_is_reconciled_to_nearest_and_retried_once() {
        let (state, seen, _dir) = reasoning_test_state(|attempt, body| {
            if attempt == 0 {
                assert!(body.contains("\"reasoning_effort\":\"high\""));
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":{"message":"reasoning effort must be low, medium, xhigh","type":"invalid_request_error","code":"unsupported_reasoning_effort"}}"#,
                )
            } else {
                assert!(body.contains("\"reasoning_effort\":\"xhigh\""));
                (
                    StatusCode::OK,
                    r#"{"id":"cmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"success"},"finish_reason":"stop"}]}"#,
                )
            }
        })
        .await;

        let req_body = r#"{"model":"local/m","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}"#;
        let resp = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            HeaderMap::new(),
            AxumBody::from(req_body.to_owned()),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let sent = seen.lock().unwrap();
        assert_eq!(
            sent.len(),
            2,
            "exactly two upstream attempts: first rejected, second reconciled"
        );
        assert!(sent[0].contains("\"reasoning_effort\":\"high\""));
        assert!(sent[1].contains("\"reasoning_effort\":\"xhigh\""));
    }

    #[tokio::test]
    async fn reasoning_effort_accepted_value_issues_one_attempt_no_rewrite() {
        let (state, seen, _dir) = reasoning_test_state(|_attempt, body| {
            assert!(body.contains("\"reasoning_effort\":\"medium\""));
            (
                StatusCode::OK,
                r#"{"id":"cmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
            )
        })
        .await;

        let req_body = r#"{"model":"local/m","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"medium"}"#;
        let resp = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            HeaderMap::new(),
            AxumBody::from(req_body.to_owned()),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1, "exactly one attempt");
    }

    #[tokio::test]
    async fn non_reconcilable_statuses_401_429_500_never_reconcile() {
        for (status, body_text) in [
            (
                StatusCode::UNAUTHORIZED,
                r#"{"error":{"message":"unauthorized: reasoning effort must be low"}}"#,
            ),
            (
                StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":{"message":"rate limited: reasoning effort must be low"}}"#,
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                r#"{"error":{"message":"server error: reasoning effort must be low"}}"#,
            ),
        ] {
            let (state, seen, _dir) =
                reasoning_test_state(move |_attempt, _body| (status, body_text)).await;

            let req_body = r#"{"model":"local/m","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}"#;
            let resp = dispatch(
                state,
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static("/v1/chat/completions"),
                HeaderMap::new(),
                AxumBody::from(req_body.to_owned()),
            )
            .await;

            assert_eq!(resp.status(), status);
            assert_eq!(
                seen.lock().unwrap().len(),
                1,
                "never retried on status {status}"
            );
        }
    }

    #[tokio::test]
    async fn reconciled_request_refused_too_caps_at_two_attempts() {
        let (state, seen, _dir) = reasoning_test_state(|attempt, _body| {
            if attempt == 0 {
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":{"message":"reasoning effort must be low, medium, xhigh"}}"#,
                )
            } else {
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":{"message":"still bad request"}}"#,
                )
            }
        })
        .await;

        let health = Arc::clone(&state.health);
        let req_body = r#"{"model":"local/m","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}"#;
        let resp = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            HeaderMap::new(),
            AxumBody::from(req_body.to_owned()),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(seen.lock().unwrap().len(), 2, "at most two attempts");
        assert_eq!(
            health.failures(
                mr_core::model::CooldownScope::Account,
                "local",
                "local",
                "m"
            ),
            0
        );
    }

    #[tokio::test]
    async fn reasoning_effort_unsupported_without_list_drops_field_and_retries_once() {
        let (state, seen, _dir) = reasoning_test_state(|attempt, body| {
            if attempt == 0 {
                assert!(body.contains("\"reasoning_effort\""));
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":{"message":"The reasoning_effort parameter is not supported by this model."}}"#,
                )
            } else {
                assert!(!body.contains("\"reasoning_effort\""));
                (
                    StatusCode::OK,
                    r#"{"id":"cmpl-1","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#,
                )
            }
        })
        .await;

        let req_body = r#"{"model":"local/m","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high"}"#;
        let resp = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            HeaderMap::new(),
            AxumBody::from(req_body.to_owned()),
        )
        .await;

        assert_eq!(resp.status(), StatusCode::OK);
        let sent = seen.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].contains("\"reasoning_effort\""));
        assert!(!sent[1].contains("\"reasoning_effort\""));
    }

    #[test]
    fn reasoning_ranking_table_and_tie_breaking_all_six_values() {
        let candidates = vec!["low".to_string(), "medium".to_string(), "xhigh".to_string()];
        // none (0) -> low (2) (closest)
        assert_eq!(
            crate::reasoning::find_closest_effort("none", &candidates),
            Some("low".to_string())
        );
        // minimal (1) -> low (2) (closest)
        assert_eq!(
            crate::reasoning::find_closest_effort("minimal", &candidates),
            Some("low".to_string())
        );
        // low (2) -> low (2) (exact)
        assert_eq!(
            crate::reasoning::find_closest_effort("low", &candidates),
            Some("low".to_string())
        );
        // medium (3) -> medium (3) (exact)
        assert_eq!(
            crate::reasoning::find_closest_effort("medium", &candidates),
            Some("medium".to_string())
        );
        // high (4) -> equidistant to medium(3, diff 1) and xhigh(5, diff 1) -> tie breaker picks HIGHER rank: xhigh
        assert_eq!(
            crate::reasoning::find_closest_effort("high", &candidates),
            Some("xhigh".to_string())
        );
        // xhigh (5) -> xhigh (5) (exact)
        assert_eq!(
            crate::reasoning::find_closest_effort("xhigh", &candidates),
            Some("xhigh".to_string())
        );
        // case insensitivity
        assert_eq!(
            crate::reasoning::find_closest_effort("HIGH", &candidates),
            Some("xhigh".to_string())
        );
        assert_eq!(
            crate::reasoning::find_closest_effort("Low", &candidates),
            Some("low".to_string())
        );
    }

    // -----------------------------------------------------------------------------------------
    // The capability gate, the model read/delete routes, and byte-for-byte multipart forwarding
    // -----------------------------------------------------------------------------------------

    /// A relay over exactly the given providers and combos, with the credential the fixture
    /// accounts name.
    fn state_over(
        providers: Vec<mr_core::model::Provider>,
        combos: Vec<mr_core::model::Combo>,
    ) -> AppState {
        let config = mr_core::config::Config {
            providers,
            combos,
            ..Default::default()
        };
        let snapshot = mr_core::snapshot::RoutingSnapshot::compile(&config).unwrap();
        let relay = Arc::new(RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).unwrap(),
        ));

        let dir = tempfile::tempdir().unwrap();
        let mut opened = mr_core::secret::Vault::open(dir.path()).unwrap();
        opened.put("test-credential", "sk-not-a-real-key").unwrap();
        let vault = Arc::new(opened);

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        AppState {
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
        }
    }

    /// A provider declaring exactly the capabilities a test means it to declare.
    fn provider_with_capabilities(
        id: &str,
        base_url: &str,
        model: &str,
        capabilities: &[Capability],
    ) -> mr_core::model::Provider {
        let mut provider = provider_serving(id, base_url, model, &[InputModality::Text]);
        provider.capabilities = capabilities.to_vec();
        provider
    }

    /// Declare a balance on a provider's only account, which is what lets it start a job.
    fn declare_credit(provider: &mut mr_core::model::Provider, usd: f64) {
        provider.accounts[0].credit_usd = Some(usd);
    }

    /// An account whose vault reference and declared balance a test can tell apart upstream.
    fn funded_account(id: &str, secret_ref: &str, usd: f64) -> mr_core::model::Account {
        let mut account = mr_core::model::Account::new(id);
        account.secret_ref = Some(secret_ref.to_owned());
        account.credit_usd = Some(usd);
        account
    }

    /// Add a second credential to a fresh state, which `state_over` seeds with only one.
    fn with_credential(mut state: AppState, secret_ref: &str, secret: &str) -> AppState {
        Arc::get_mut(&mut state.vault)
            .expect("a fresh state owns its vault")
            .put(secret_ref, secret)
            .expect("the vault accepts a second credential");
        state
    }

    #[tokio::test]
    async fn a_route_no_step_declares_is_an_unsupported_endpoint_and_is_never_dialled() {
        let (url, seen) = fake_upstream().await;
        // The default capability set is [Chat, Models]: the model is reachable, embeddings are not.
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![provider], Vec::new());

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/embeddings"),
            HeaderMap::new(),
            AxumBody::from(r#"{"model":"p/m","input":"hello"}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_text(response).await;
        assert!(body.contains("unsupported_endpoint"), "{body}");
        assert!(
            body.contains(
                "the route embeddings requires the embeddings capability, and no step's provider \
                 declares it"
            ),
            "{body}"
        );
        // The refusal is the whole answer: the provider never sees a request it would have failed
        // with a shape the client could not read.
        assert!(
            seen.lock().unwrap().is_empty(),
            "a provider that does not declare the capability was dialled anyway"
        );
    }

    #[tokio::test]
    async fn a_provider_that_declares_the_capability_serves_the_route() {
        let (url, seen) = fake_upstream().await;
        let provider = provider_with_capabilities(
            "p",
            &url,
            "m",
            &[Capability::Chat, Capability::Models, Capability::Embeddings],
        );
        let state = state_over(vec![provider], Vec::new());

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/embeddings"),
            HeaderMap::new(),
            AxumBody::from(r#"{"model":"p/m","input":"hello"}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_speech_only_provider_serves_speech_and_refuses_every_other_route() {
        let (url, seen) = fake_upstream().await;
        // The fish.audio shape: one family is declared, so exactly one route is reachable and the
        // model is listed for discovery. A provider that declares only this must not be asked for
        // a chat completion it never promised, and the speech request must arrive as its own JSON
        // body rather than translated into a chat shape on the way out.
        let provider = provider_with_capabilities(
            "fish",
            &url,
            "speech-1",
            &[Capability::AudioSpeech, Capability::Models],
        );
        let state = state_over(vec![provider], Vec::new());

        let response = dispatch(
            state.clone(),
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/audio/speech"),
            HeaderMap::new(),
            AxumBody::from(r#"{"model":"fish/speech-1","input":"hello","voice":"default"}"#),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        {
            let seen = seen.lock().unwrap();
            assert_eq!(
                seen.len(),
                1,
                "the speech request did not reach the provider"
            );
            assert!(seen[0].contains(r#""input":"hello""#), "{}", seen[0]);
            assert!(
                !seen[0].contains("messages"),
                "a speech body was rewritten into a chat body: {}",
                seen[0]
            );
        }

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/chat/completions"),
            HeaderMap::new(),
            AxumBody::from(
                r#"{"model":"fish/speech-1","messages":[{"role":"user","content":"hi"}]}"#,
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_text(response).await;
        assert!(body.contains("unsupported_endpoint"), "{body}");
        assert!(
            body.contains("chat.completions requires the chat capability"),
            "{body}"
        );
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "a provider that does not declare chat was dialled anyway"
        );
    }

    #[tokio::test]
    async fn a_combo_reaches_the_capable_step_when_the_first_one_cannot_serve_the_route() {
        let (incapable_url, incapable_seen) = fake_upstream().await;
        let (capable_url, capable_seen) = fake_upstream().await;
        let incapable = provider_with_capabilities(
            "incapable",
            &incapable_url,
            "chat-only",
            &[Capability::Chat, Capability::Models],
        );
        let capable = provider_with_capabilities(
            "capable",
            &capable_url,
            "embedding",
            &[Capability::Chat, Capability::Models, Capability::Embeddings],
        );
        let mut combo = mr_core::model::Combo::new(
            "mixed",
            mr_core::model::ComboStep::new("incapable", "chat-only"),
        );
        combo
            .steps
            .push(mr_core::model::ComboStep::new("capable", "embedding"));
        let state = state_over(vec![incapable, capable], vec![combo]);

        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/embeddings"),
            HeaderMap::new(),
            AxumBody::from(r#"{"model":"mixed","input":"hello"}"#),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            incapable_seen.lock().unwrap().is_empty(),
            "the step whose provider does not declare embeddings was dialled"
        );
        assert_eq!(
            capable_seen.lock().unwrap().len(),
            1,
            "the capable step did not receive the request"
        );
    }

    #[tokio::test]
    async fn the_model_read_route_returns_the_row_the_listing_shows() {
        let (url, _seen) = fake_upstream().await;
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let combo = mr_core::model::Combo::new("both", mr_core::model::ComboStep::new("p", "m"));
        let state = state_over(vec![provider], vec![combo]);
        let listing = model_listing(&state.relay.table().snapshot);

        // A provider's model is listed as `provider/model`, and the read route answers with that
        // exact row — the same id, the same `owned_by`, the same `manarouter` metadata.
        let response = retrieve_model(
            State(state.clone()),
            Path("p/m".to_owned()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let row: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        let listed = listing
            .iter()
            .find(|row| row["id"] == "p/m")
            .expect("the listing shows the provider's model");
        assert_eq!(&row, listed);

        // A combo resolves to the combo's own name, exactly as the list shows it.
        let response = retrieve_model(
            State(state.clone()),
            Path("both".to_owned()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let row: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(row["id"], "both");

        // A name the list does not show is a 404 naming the model, not an empty 200.
        let response =
            retrieve_model(State(state), Path("p/nope".to_owned()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let body = body_text(response).await;
        assert!(body.contains("model_not_found"), "{body}");
        assert!(body.contains("p/nope"), "{body}");
    }

    #[tokio::test]
    async fn the_model_delete_route_refuses_honestly_and_leaves_the_model_alone() {
        let (url, _seen) = fake_upstream().await;
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![provider], Vec::new());

        let response = delete_model(
            State(state.clone()),
            Path("p/m".to_owned()),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = body_text(response).await;
        assert!(body.contains("invalid_request_error"), "{body}");
        assert!(
            body.contains("deleting a model is only possible for a fine-tuned model"),
            "{body}"
        );
        // The refusal is local: the model is still in the listing.
        assert!(model_listing(&state.relay.table().snapshot)
            .iter()
            .any(|row| row["id"] == "p/m"));

        // An unknown name is the same 404 the read route gives, rather than a claim about what
        // kind of model it is.
        let response =
            delete_model(State(state), Path("p/nope".to_owned()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(body_text(response).await.contains("model_not_found"));
    }

    /// A fake upstream that records the exact bytes and content type of every request.
    #[allow(clippy::type_complexity)]
    async fn raw_upstream() -> (
        String,
        Arc<std::sync::Mutex<Vec<(Option<String>, Vec<u8>)>>>,
    ) {
        let seen: Arc<std::sync::Mutex<Vec<(Option<String>, Vec<u8>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |headers: HeaderMap, body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                let content_type = headers
                    .get("content-type")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                sink.lock().unwrap().push((content_type, body.to_vec()));
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"text":"ok"}"#,
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

    #[tokio::test]
    async fn a_multipart_body_reaches_the_upstream_byte_for_byte_with_its_content_type() {
        const BOUNDARY: &str = "manarouter-test-boundary";
        // Bytes that are not valid UTF-8 anywhere: a body read as a string, or re-encoded, would
        // not survive this part.
        let binary: Vec<u8> = vec![0xff, 0xd8, 0x00, 0x01, 0x80, b'n', 0x0a, 0x0d, 0xfe];
        let multipart = |model: &[u8]| {
            let mut body = Vec::new();
            body.extend_from_slice(
                format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\n")
                    .as_bytes(),
            );
            body.extend_from_slice(model);
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                     filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(&binary);
            body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
            body
        };

        let (url, seen) = raw_upstream().await;
        let provider = provider_with_capabilities(
            "p",
            &url,
            "whisper",
            &[
                Capability::Chat,
                Capability::Models,
                Capability::AudioTranscription,
            ],
        );
        let state = state_over(vec![provider], Vec::new());

        let sent = multipart(b"p/whisper");
        let mut sent_headers = HeaderMap::new();
        sent_headers.insert(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}")
                .parse()
                .unwrap(),
        );
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/audio/transcriptions"),
            sent_headers,
            AxumBody::from(sent),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let (content_type, body) = &seen[0];
        assert_eq!(
            content_type.as_deref(),
            Some("multipart/form-data; boundary=manarouter-test-boundary"),
            "the multipart content type did not reach the upstream"
        );
        // The only difference from what the client sent is the model value: the client names the
        // model as the listing does, and the upstream only understands its own id. Every other
        // byte — the boundary, the file, the binary payload — is the client's.
        assert_eq!(
            body,
            &multipart(b"whisper"),
            "the multipart body was not forwarded byte for byte"
        );
        assert!(
            body.windows(binary.len()).any(|window| window == binary),
            "the binary part was corrupted on the way upstream"
        );
    }

    // -----------------------------------------------------------------------------------------
    // Capability-addressed forwarding: the routes that name no model in a field we can read
    // -----------------------------------------------------------------------------------------

    /// A fake upstream that records the method, the path and the exact body of every request, and
    /// answers with one fixed status and body.
    #[allow(clippy::type_complexity)]
    async fn recording_upstream(
        status: StatusCode,
        answer: &'static str,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
    ) {
        let seen: Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                sink.lock().unwrap().push((
                    method.as_str().to_owned(),
                    uri.path().to_string(),
                    body.to_vec(),
                ));
                (
                    status,
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

    /// A fake upstream that records the bearer token it was dialled with and answers through
    /// `answer`, so a test can see which account was tried, and in what order.
    #[allow(clippy::type_complexity)]
    async fn credential_upstream(
        answer: fn(&str) -> (StatusCode, &'static str),
    ) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |headers: HeaderMap| {
            let sink = Arc::clone(&sink);
            async move {
                let token = headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("<no authorization>")
                    .to_owned();
                sink.lock().unwrap().push(token.clone());
                let (status, body) = answer(&token);
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body.to_owned(),
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

    /// One request through the **mounted router**, so a test also proves the path is registered and
    /// that its wildcard shape captures what it should.
    async fn call_router(
        app: &AppState,
        method: Method,
        uri: &str,
        body: Vec<u8>,
    ) -> (StatusCode, String) {
        use tower::ServiceExt;

        let request = axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(AxumBody::from(body))
            .expect("a request");
        let response = router(app.clone())
            .oneshot(request)
            .await
            .expect("the router answered");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// A fake upstream that answers a file upload with an id and records everything it is sent, so a
    /// test can see both the staging request and the job request that follows it.
    #[allow(clippy::type_complexity)]
    async fn staging_upstream() -> (
        String,
        Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>>,
    ) {
        let seen: Arc<std::sync::Mutex<Vec<(String, String, Vec<u8>)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let app = Router::new().fallback(any(move |method: Method, uri: Uri, body: Bytes| {
            let sink = Arc::clone(&sink);
            async move {
                let path = uri.path().to_string();
                sink.lock().unwrap().push((
                    method.as_str().to_owned(),
                    path.clone(),
                    body.to_vec(),
                ));
                if path.ends_with("/files") {
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"id":"file-upstream-1","object":"file","purpose":"fine-tune"}"#,
                    )
                } else {
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        r#"{"id":"ftjob-1","status":"queued"}"#,
                    )
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn a_fine_tuning_job_stages_its_training_file_with_the_provider() {
        let (url, seen) = staging_upstream().await;
        let mut provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        declare_credit(&mut provider, 40.0);
        let state = state_over(vec![provider], Vec::new());
        let training = b"{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}\n".to_vec();
        let stored = match state
            .batch
            .put_file(
                training.clone(),
                Some("train.jsonl".to_owned()),
                Some("fine-tune".to_owned()),
            )
            .await
        {
            Ok(file) => file,
            Err(_) => panic!("the lane refused a fine-tune file"),
        };
        let local_id = stored["id"].as_str().expect("a file id").to_owned();
        let sent =
            format!(r#"{{"model":"gpt-4o-mini","training_file":"{local_id}","suffix":"mine"}}"#);
        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            sent.into_bytes(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "the upload and the job: {seen:?}");
        // The upload: multipart, the lane's bytes, and the purpose OpenAI requires for training data.
        let (method, path, upload) = &seen[0];
        assert_eq!(method, "POST");
        assert!(path.ends_with("/files"), "{path}");
        let upload = String::from_utf8_lossy(upload);
        assert!(
            upload.contains("name=\"purpose\"\r\n\r\nfine-tune"),
            "{upload}"
        );
        assert!(
            upload.contains("name=\"file\"; filename=\"train.jsonl\""),
            "{upload}"
        );
        assert!(upload.contains("content\":\"hi\""), "{upload}");
        // The job: the local id is gone, and the provider's id is what it is asked to train on.
        let (method, path, job) = &seen[1];
        assert_eq!(method, "POST");
        assert!(path.ends_with("/fine_tuning/jobs"), "{path}");
        let job: serde_json::Value = serde_json::from_slice(job).expect("a JSON body");
        assert_eq!(job["training_file"], "file-upstream-1");
        assert_eq!(job["model"], "gpt-4o-mini");
        assert_eq!(job["suffix"], "mine");
        assert!(!job.to_string().contains(&local_id), "{job}");
    }

    #[tokio::test]
    async fn a_fine_tuning_job_naming_a_file_the_relay_does_not_hold_is_forwarded_untouched() {
        // A client may have uploaded its data to the provider directly; that id is the provider's to
        // resolve, and rewriting it here would break a request that works.
        let (url, seen) =
            recording_upstream(StatusCode::OK, r#"{"id":"ftjob-2","status":"queued"}"#).await;
        let mut provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        declare_credit(&mut provider, 40.0);
        let state = state_over(vec![provider], Vec::new());

        let sent = r#"{"model":"gpt-4o-mini","training_file":"file-theirs"}"#;
        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            sent.as_bytes().to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "no upload is attempted: {seen:?}");
        assert_eq!(seen[0].2, sent.as_bytes(), "the body is the client's own");
    }

    #[tokio::test]
    async fn a_fine_tuning_job_reaches_the_provider_that_declares_the_capability() {
        let (url, seen) =
            recording_upstream(StatusCode::OK, r#"{"id":"ftjob-1","status":"queued"}"#).await;
        let mut provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        declare_credit(&mut provider, 40.0);
        let state = state_over(vec![provider], Vec::new());

        let sent = r#"{"model":"gpt-4o-mini","training_file":"file-abc","suffix":"mine"}"#;
        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            sent.as_bytes().to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(answer.contains("ftjob-1"), "{answer}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the job was not sent exactly once");
        assert_eq!(seen[0].0, "POST");
        // The base URL carries no version and the client's path does, and it appears exactly once.
        assert_eq!(seen[0].1, "/v1/fine_tuning/jobs");
        // The seam is the identity: this router stages no training files, so rewriting
        // `training_file` would send the provider an id it cannot resolve.
        assert_eq!(String::from_utf8_lossy(&seen[0].2), sent);
    }

    #[tokio::test]
    async fn a_fine_tuning_route_no_provider_declares_is_an_unsupported_endpoint() {
        let (url, seen) = recording_upstream(StatusCode::OK, "{}").await;
        // The default capability set is [Chat, Models], so this provider serves chat but no jobs.
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![provider], Vec::new());

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(answer.contains("unsupported_endpoint"), "{answer}");
        assert!(
            answer.contains(
                "the route /v1/fine_tuning/jobs requires the fine-tuning capability, and no \
                 provider declares it"
            ),
            "{answer}"
        );
        // No chat fallback and no dial: the refusal is local.
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_fine_tuning_sub_resource_keeps_its_id_and_its_method() {
        let (url, seen) =
            recording_upstream(StatusCode::OK, r#"{"object":"list","data":[]}"#).await;
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        let state = state_over(vec![provider], Vec::new());

        let (status, _) = call_router(
            &state,
            Method::GET,
            "/v1/fine_tuning/jobs/ftjob-1/events?limit=5",
            Vec::new(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "GET");
        // The id and the sub-resource are the client's path, not a default: there is no `RouteKind`
        // for a fine-tuning job, so nothing else could have supplied them.
        assert_eq!(seen[0].1, "/v1/fine_tuning/jobs/ftjob-1/events");
        assert!(
            seen[0].2.is_empty(),
            "a GET gained a body on the way upstream"
        );
    }

    #[tokio::test]
    async fn the_named_provider_serves_a_capability_addressed_route() {
        let (first_url, first_seen) = recording_upstream(StatusCode::OK, r#"{"id":"first"}"#).await;
        let (second_url, second_seen) =
            recording_upstream(StatusCode::OK, r#"{"id":"second"}"#).await;
        let mut first = provider_with_capabilities("a", &first_url, "m", &[Capability::FineTuning]);
        declare_credit(&mut first, 40.0);
        let mut second =
            provider_with_capabilities("b", &second_url, "m", &[Capability::FineTuning]);
        declare_credit(&mut second, 40.0);
        let state = state_over(vec![first, second], Vec::new());

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs?provider=b",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(answer.contains("second"), "{answer}");
        assert!(
            first_seen.lock().unwrap().is_empty(),
            "the first provider was dialled despite ?provider=b"
        );
        assert_eq!(second_seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn only_a_reported_usd_balance_is_read_as_credit() {
        let usd = r#"{"credit":{"remaining":42.5,"total":50.0,"unit":"USD","unlimited":false,"fraction_reported":true},"windows":[]}"#;
        assert_eq!(cached_usd_credit_cents(usd), Some(4_250));

        // The credit headline beats the windows it summarises.
        let both = r#"{"credit":{"remaining":42.5,"unit":"USD","unlimited":false,"fraction_reported":true},"windows":[{"remaining":9.0,"unit":"USD","unlimited":false,"fraction_reported":true}]}"#;
        assert_eq!(cached_usd_credit_cents(both), Some(4_250));

        // No unit: a bare number is not dollars, and comparing it to a dollar floor would be a
        // guess about what the provider counts.
        let unitless = r#"{"credit":{"remaining":100000,"unlimited":false,"fraction_reported":true},"windows":[]}"#;
        assert_eq!(cached_usd_credit_cents(unitless), None);

        // An unlimited plan reports no spendable number, so there is nothing to compare.
        let unlimited = r#"{"credit":{"remaining":0,"unit":"USD","unlimited":true,"fraction_reported":true},"windows":[]}"#;
        assert_eq!(cached_usd_credit_cents(unlimited), None);

        // A percentage is a fraction of a total the router does not know.
        let percent = r#"{"windows":[{"remaining":80.0,"unit":"%","unlimited":false,"fraction_reported":true}]}"#;
        assert_eq!(cached_usd_credit_cents(percent), None);

        // `remaining: 0` with `fraction_reported: false` is the sentinel for "not reported", not
        // a declared zero.
        let unreported = r#"{"windows":[{"remaining":0,"unit":"USD","unlimited":false,"fraction_reported":false}]}"#;
        assert_eq!(cached_usd_credit_cents(unreported), None);

        // With no headline, the largest known USD window is the best available figure: two windows
        // may be the same money described twice, so they are not summed.
        let windows = r#"{"windows":[{"remaining":3.0,"unit":"USD","unlimited":false,"fraction_reported":true},{"remaining":9.5,"unit":"USD","unlimited":false,"fraction_reported":true}]}"#;
        assert_eq!(cached_usd_credit_cents(windows), Some(950));

        assert_eq!(cached_usd_credit_cents("not json"), None);
    }

    #[tokio::test]
    async fn a_fine_tuning_job_is_refused_before_any_dial_when_no_account_is_funded() {
        let (url, seen) =
            recording_upstream(StatusCode::OK, r#"{"id":"ftjob-1","status":"queued"}"#).await;
        // A capability is not a balance: this provider can fine-tune, and the relay cannot tell
        // that its account has any money, so it must not start a job it cannot pay for.
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        let state = state_over(vec![provider], Vec::new());

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"gpt-4o-mini","training_file":"file-abc"}"#.to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{answer}");
        assert!(answer.contains("insufficient_credit"), "{answer}");
        assert!(answer.contains("more than $15.00"), "{answer}");
        assert!(answer.contains("mr account set-credit p"), "{answer}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "the provider was asked to train with an account that cannot be shown to pay"
        );

        // Reading is not spending: the same unfunded account may list the jobs it already owns.
        let (status, _) =
            call_router(&state, Method::GET, "/v1/fine_tuning/jobs", Vec::new()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fifteen_dollars_is_not_more_than_fifteen_dollars() {
        let (url, seen) =
            recording_upstream(StatusCode::OK, r#"{"id":"ftjob-1","status":"queued"}"#).await;
        let mut poor = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        declare_credit(&mut poor, 15.0);
        let refused = state_over(vec![poor], Vec::new());

        let (status, answer) = call_router(
            &refused,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{answer}");
        assert!(seen.lock().unwrap().is_empty());

        // One cent more is the smallest balance that clears the floor.
        let mut funded = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        declare_credit(&mut funded, 15.01);
        let accepted = state_over(vec![funded], Vec::new());
        let (status, answer) = call_router(
            &accepted,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert!(answer.contains("ftjob-1"), "{answer}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_created_job_goes_to_the_account_with_the_most_credit() {
        let (url, seen) =
            credential_upstream(|_| (StatusCode::OK, r#"{"id":"ftjob-ok","status":"queued"}"#))
                .await;
        let mut provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        // Configured order is deliberately cheapest-first: the choice must come from the balance,
        // not from the position in the file.
        provider.accounts = vec![
            funded_account("cheap", "cheap-credential", 20.0),
            funded_account("rich", "rich-credential", 200.0),
        ];
        let state = with_credential(
            state_over(vec![provider], Vec::new()),
            "cheap-credential",
            "sk-cheap",
        );
        let state = with_credential(state, "rich-credential", "sk-rich");

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{answer}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "one account starts the job: {seen:?}");
        assert_eq!(
            seen[0], "Bearer sk-rich",
            "the richest account was not used"
        );
    }

    #[tokio::test]
    async fn a_spent_account_is_silently_replaced_by_a_funded_sibling() {
        fn refuses_the_spent_account(token: &str) -> (StatusCode, &'static str) {
            if token.ends_with("sk-spent") {
                (
                    StatusCode::PAYMENT_REQUIRED,
                    r#"{"error":{"message":"Your credit balance is too low to start a fine-tuning job","code":"insufficient_quota"}}"#,
                )
            } else {
                (StatusCode::OK, r#"{"id":"ftjob-ok","status":"queued"}"#)
            }
        }

        let (url, seen) = credential_upstream(refuses_the_spent_account).await;
        let mut provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        // The spent account is the richer one, so it is tried first and the client never learns
        // that the first attempt happened.
        provider.accounts = vec![
            funded_account("spent", "spent-credential", 200.0),
            funded_account("funded", "funded-credential", 20.0),
        ];
        let state = with_credential(
            state_over(vec![provider], Vec::new()),
            "spent-credential",
            "sk-spent",
        );
        let state = with_credential(state, "funded-credential", "sk-funded");

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{answer}");
        assert!(answer.contains("ftjob-ok"), "{answer}");
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 2, "both accounts were tried: {seen:?}");
        assert_eq!(seen[0], "Bearer sk-spent");
        assert_eq!(seen[1], "Bearer sk-funded");
        // The dead account is cooled, so the next request does not pay for the same refusal.
        assert_eq!(state.health.cooling_count(), 1);
    }

    #[tokio::test]
    async fn a_four_hundred_that_names_credit_moves_the_job_to_another_account() {
        fn refuses_with_a_body(token: &str) -> (StatusCode, &'static str) {
            if token.ends_with("sk-spent") {
                (
                    StatusCode::BAD_REQUEST,
                    r#"{"error":{"message":"Insufficient credit to start this job","type":"invalid_request_error"}}"#,
                )
            } else {
                (StatusCode::OK, r#"{"id":"ftjob-ok","status":"queued"}"#)
            }
        }

        let (url, seen) = credential_upstream(refuses_with_a_body).await;
        let mut provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        provider.accounts = vec![
            funded_account("spent", "spent-credential", 200.0),
            funded_account("funded", "funded-credential", 20.0),
        ];
        let state = with_credential(
            state_over(vec![provider], Vec::new()),
            "spent-credential",
            "sk-spent",
        );
        let state = with_credential(state, "funded-credential", "sk-funded");

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;

        // A 400 usually means the request was malformed, and must not rotate accounts. This one
        // names money, which is a fact about the account, so it is the same story as a 402.
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(seen.lock().unwrap().len(), 2);
        assert_eq!(state.health.cooling_count(), 1);
    }

    #[tokio::test]
    async fn a_cached_usd_balance_can_fund_a_job_until_it_goes_stale() {
        let (url, seen) =
            recording_upstream(StatusCode::OK, r#"{"id":"ftjob-1","status":"queued"}"#).await;
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::FineTuning]);
        let account_id = provider.accounts[0].id.clone();
        let state = state_over(vec![provider], Vec::new());
        let payload = r#"{"credit":{"remaining":25.0,"total":25.0,"used":0.0,"unit":"USD","unlimited":false,"fraction_reported":true},"windows":[]}"#;
        let fresh =
            mr_core::store::ProviderQuotaState::new("p", account_id.clone(), "provider", payload);
        state.quota_store.put_provider_quota_state(&fresh).unwrap();

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{answer}");
        assert_eq!(seen.lock().unwrap().len(), 1);

        // A cached balance is only as fresh as the row says. A day-old figure is not evidence that
        // the account can pay today, so it stops qualifying.
        let now = mr_core::ids::Millis::now().as_millis();
        let mut stale = fresh.clone();
        stale.fetched_at = mr_core::ids::Millis::from_millis(now - 25 * 60 * 60 * 1000);
        state.quota_store.put_provider_quota_state(&stale).unwrap();

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/fine_tuning/jobs",
            br#"{"model":"m","training_file":"file-abc"}"#.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{answer}");
        assert!(answer.contains("insufficient_credit"), "{answer}");
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "a stale balance was treated as current"
        );
    }

    #[tokio::test]
    async fn a_video_read_is_addressed_by_capability_because_the_path_names_the_render() {
        let (url, seen) = recording_upstream(StatusCode::OK, "binary-render-bytes").await;
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::Video]);
        let state = state_over(vec![provider], Vec::new());

        // A read carries no body and no model field: it can only be routed by capability.
        let (status, answer) = call_router(
            &state,
            Method::GET,
            "/v1/videos/video_1/content",
            Vec::new(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(answer, "binary-render-bytes");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "GET");
        assert_eq!(seen[0].1, "/v1/videos/video_1/content");
    }

    #[tokio::test]
    async fn a_realtime_session_is_addressed_by_capability() {
        let (url, seen) = recording_upstream(StatusCode::OK, r#"{"value":"ek_1"}"#).await;
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::Realtime]);
        let state = state_over(vec![provider], Vec::new());

        // The model lives inside `session`, which the scanner does not descend into, so this
        // request has no readable `model` and still reaches the provider that declares realtime.
        let sent = r#"{"session":{"type":"realtime","model":"gpt-realtime"}}"#;
        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/realtime/client_secrets",
            sent.as_bytes().to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert!(answer.contains("ek_1"), "{answer}");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].1, "/v1/realtime/client_secrets");
        assert_eq!(String::from_utf8_lossy(&seen[0].2), sent);
    }

    #[tokio::test]
    async fn a_fine_tuned_model_name_a_provider_advertises_is_readable() {
        // A fine-tune id is an ordinary model id as far as this router is concerned: once the
        // provider advertises it (`mr models sync` records it), it is in the listing, and the read
        // route answers for it because it answers from the listing. No special case, and no claim
        // about a fine-tune the provider has not told us about.
        let (url, _) = recording_upstream(StatusCode::OK, "{}").await;
        let id = "p/ft:gpt-4o-mini:acme::abc123";
        let provider = provider_serving(
            "p",
            &url,
            "ft:gpt-4o-mini:acme::abc123",
            &[InputModality::Text],
        );
        let state = state_over(vec![provider], Vec::new());

        assert!(
            model_listing(&state.relay.table().snapshot)
                .iter()
                .any(|row| row["id"] == id),
            "the advertised fine-tune is not in the listing"
        );
        let response = retrieve_model(State(state), Path(id.to_owned()), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let row: serde_json::Value =
            serde_json::from_str(&body_text(response).await).expect("a JSON row");
        assert_eq!(row["id"], id);
    }

    #[tokio::test]
    async fn the_containers_surface_is_refused_until_a_provider_serves_it() {
        let (url, seen) = recording_upstream(StatusCode::OK, "{}").await;
        let plain = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![plain], Vec::new());

        let (status, answer) =
            call_router(&state, Method::POST, "/v1/containers", b"{}".to_vec()).await;

        // 501, not 404: the endpoint exists in OpenAI's surface and this router does not implement
        // it, which is a different statement from "no route here".
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert!(answer.contains("unsupported_endpoint"), "{answer}");
        assert!(answer.contains("containers"), "{answer}");
        assert!(seen.lock().unwrap().is_empty());

        // With a provider that does serve containers, the same request is proxied.
        let serving = provider_with_capabilities("p", &url, "m", &[Capability::Containers]);
        let state = state_over(vec![serving], Vec::new());
        let (status, _) = call_router(&state, Method::POST, "/v1/containers", b"{}".to_vec()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    // -----------------------------------------------------------------------------------------
    // Routes whose `model` is optional: a documented default the provider applies
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn a_model_less_image_edit_reaches_a_provider_that_serves_image_edits() {
        const BOUNDARY: &str = "manarouter-model-less";
        let binary: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x00, 0xff, 0x0a, 0x0d];
        // No `model` part at all: the client is relying on the documented `dall-e-2` default.
        let mut sent = Vec::new();
        sent.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\n\
                 a cat\r\n"
            )
            .as_bytes(),
        );
        sent.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"image\"; \
                 filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n"
            )
            .as_bytes(),
        );
        sent.extend_from_slice(&binary);
        sent.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

        let (url, seen) = raw_upstream().await;
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::ImagesEdits]);
        let state = state_over(vec![provider], Vec::new());

        let mut sent_headers = HeaderMap::new();
        sent_headers.insert(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}")
                .parse()
                .unwrap(),
        );
        let response = dispatch(
            state,
            Dialect::OpenAi,
            Method::POST,
            Uri::from_static("/v1/images/edits"),
            sent_headers,
            AxumBody::from(sent.clone()),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the edit was not forwarded exactly once");
        let (content_type, body) = &seen[0];
        assert_eq!(
            content_type.as_deref(),
            Some("multipart/form-data; boundary=manarouter-model-less"),
            "the multipart content type did not reach the upstream"
        );
        // Nothing to rewrite: there is no model field, so every byte is the client's.
        assert_eq!(
            body, &sent,
            "the multipart body was not forwarded byte for byte"
        );
        assert!(
            body.windows(binary.len()).any(|window| window == binary),
            "the binary part was corrupted on the way upstream"
        );
    }

    #[tokio::test]
    async fn a_model_less_moderation_is_forwarded_to_the_provider_that_serves_moderations() {
        let (url, seen) = recording_upstream(StatusCode::OK, r#"{"results":[]}"#).await;
        let provider = provider_with_capabilities("p", &url, "m", &[Capability::Moderations]);
        let state = state_over(vec![provider], Vec::new());

        let sent = r#"{"input":"hello"}"#;
        let (status, _) = call_router(
            &state,
            Method::POST,
            "/v1/moderations",
            sent.as_bytes().to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "the moderation was not forwarded exactly once"
        );
        assert_eq!(seen[0].1, "/v1/moderations");
        // The provider applies its own default model; the router invents nothing.
        assert_eq!(String::from_utf8_lossy(&seen[0].2), sent);
    }

    #[tokio::test]
    async fn a_model_less_image_edit_with_no_capable_provider_is_an_unsupported_endpoint() {
        let (url, seen) = recording_upstream(StatusCode::OK, "{}").await;
        // [Chat, Models]: the provider serves chat, not image edits.
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![provider], Vec::new());

        let (status, answer) =
            call_router(&state, Method::POST, "/v1/images/edits", b"{}".to_vec()).await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(answer.contains("unsupported_endpoint"), "{answer}");
        assert!(
            answer.contains(
                "the route /v1/images/edits requires the images.edits capability, and no provider \
                 declares it"
            ),
            "{answer}"
        );
        assert!(seen.lock().unwrap().is_empty(), "the refusal is local");
    }

    #[tokio::test]
    async fn a_model_less_chat_request_is_still_refused() {
        let (url, seen) = recording_upstream(StatusCode::OK, "{}").await;
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![provider], Vec::new());

        let (status, answer) = call_router(
            &state,
            Method::POST,
            "/v1/chat/completions",
            br#"{"messages":[{"role":"user","content":"hi"}]}"#.to_vec(),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            // The body is JSON, so the quoted word arrives escaped; match around it.
            answer.contains("the request body has no readable") && answer.contains("model"),
            "{answer}"
        );
        // A model is required here, and substituting one would be a lie about what ran.
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_three_routes_with_a_documented_default_accept_a_missing_model() {
        // Every route kind, with the path the router serves it on. This is the list a later route
        // cannot quietly join: the three optional ones are named here, and everything else must
        // still answer "no readable model".
        let routes: [(RouteKind, &str); 19] = [
            (RouteKind::ChatCompletions, "/v1/chat/completions"),
            (RouteKind::Responses, "/v1/responses"),
            (RouteKind::Embeddings, "/v1/embeddings"),
            (RouteKind::AudioTranscription, "/v1/audio/transcriptions"),
            (RouteKind::AudioTranslation, "/v1/audio/translations"),
            (RouteKind::AudioSpeech, "/v1/audio/speech"),
            (RouteKind::ImagesGenerations, "/v1/images/generations"),
            (RouteKind::ImagesEdits, "/v1/images/edits"),
            (RouteKind::ImagesVariations, "/v1/images/variations"),
            (RouteKind::Moderations, "/v1/moderations"),
            (RouteKind::Models, "/v1/models"),
            (RouteKind::Messages, "/v1/messages"),
            (RouteKind::CountTokens, "/v1/messages/count_tokens"),
            (RouteKind::VideoCreate, "/v1/videos"),
            (RouteKind::VideoRetrieve, "/v1/videos/video_1"),
            (RouteKind::VideoContent, "/v1/videos/video_1/content"),
            (
                RouteKind::RealtimeClientSecrets,
                "/v1/realtime/client_secrets",
            ),
            (RouteKind::RealtimeCalls, "/v1/realtime/calls"),
            (RouteKind::Search, "/v1/search"),
        ];

        let (url, seen) = recording_upstream(StatusCode::OK, "{}").await;
        let provider = provider_serving("p", &url, "m", &[InputModality::Text]);
        let state = state_over(vec![provider], Vec::new());

        for (kind, path) in routes {
            assert_eq!(route_kind_for(path), kind, "{path} is not {kind:?}");
            let response = dispatch(
                state.clone(),
                Dialect::OpenAi,
                Method::POST,
                Uri::from_static(path),
                HeaderMap::new(),
                AxumBody::from("{}"),
            )
            .await;
            match model_optional_capability(kind) {
                Some(capability) => {
                    // Optional: forwarded, so the answer is the missing capability, not a 400.
                    assert_eq!(
                        response.status(),
                        StatusCode::NOT_FOUND,
                        "{path} should be forwarded by capability"
                    );
                    let body = body_text(response).await;
                    assert!(body.contains("unsupported_endpoint"), "{body}");
                    assert!(
                        body.contains(capability.as_str()),
                        "{path} did not name {capability:?}: {body}"
                    );
                }
                None => {
                    assert_eq!(
                        response.status(),
                        StatusCode::BAD_REQUEST,
                        "{path} accepted a request with no model"
                    );
                    let body = body_text(response).await;
                    assert!(
                        body.contains("the request body has no readable") && body.contains("model"),
                        "{path}: {body}"
                    );
                }
            }
        }

        // Exactly three, and no dial happened for any of them.
        assert_eq!(
            [
                RouteKind::Moderations,
                RouteKind::ImagesEdits,
                RouteKind::ImagesVariations,
            ]
            .iter()
            .filter(|kind| model_optional_capability(**kind).is_some())
            .count(),
            3
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "a model-less route dialled"
        );
    }
}
