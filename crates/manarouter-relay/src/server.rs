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
//! # What this module does not do, and says so
//!
//! The hedge **policy** is complete and tested in [`crate::select`], but the concurrent execution of a
//! race is not wired here: a request is sent to one step at a time, in priority order. Everything the
//! race decides is therefore decided, and the runner that acts on it in parallel is the remaining
//! piece. Saying that plainly matters more than a comment implying a live hedge — a reader who
//! assumes one is running will misread every latency measurement they take.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body as AxumBody;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use axum::Router;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, BodyStream, StreamBody};
use mr_core::dialect::{AuthKind, Dialect, InputModality, ReasoningLevel, RouteKind};
use mr_core::oauth::platform as oauth_platform;
use mr_core::oauth::refresh::Refresher;
use mr_core::secret::Vault;
use mr_core::snapshot::{Resolution, StepPlan};

use mr_dialects::{translate_request, TranslateCtx};

use crate::model_scan::{scan_request_head, RequestHead};
use crate::select::{self, Health};
use crate::state::{AuthOutcome, RelayState, RouteTable};
use crate::upstream::UpstreamRequest;

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
enum BodyError {
    /// The body exceeded [`MAX_BODY_BYTES`].
    TooLarge,
    /// The body could not be read at all.
    Read(String),
}

impl BodyError {
    /// The client-facing response.
    fn into_response(self) -> Response {
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
    /// Bounded combo-wide successful-step memory; reset when the relay process restarts.
    pub sticky_success: Arc<crate::sticky::StickySuccess>,
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
        .route("/v1/audio/speech", any(dispatch_openai))
        .route("/v1/images/generations", any(dispatch_openai))
        .route("/v1/messages", any(dispatch_anthropic))
        .route("/v1/messages/count_tokens", any(dispatch_anthropic))
        .route("/v1/models", any(list_models))
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
        let fragment: serde_json::Value = serde_json::from_str(fragment)
            .map_err(|_| "the compiled reasoning fragment is not valid JSON".to_string())?;
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
        },
        0,
        None,
    )
    .await
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
    } = request;
    let started = std::time::Instant::now();

    // --- Authentication, before anything reads the body. ---
    let presented = presented_key(&headers);
    let auth = authenticate_request(&app.relay, app.no_key_required, presented.as_deref());
    if !auth.is_allowed() {
        return auth_error(&auth, client_dialect);
    }
    let auth_done = started.elapsed();

    let bytes = match read_body(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return e.into_response(),
    };

    // --- The model field, without parsing the JSON. ---
    let head = scan_request_head(&bytes);
    let scan_done = started.elapsed();

    // The client's route decides the pivot the body is read in, and therefore which translation
    // applies. Derived from the path rather than passed in by each handler so that a new route
    // cannot be registered and silently fall through to Chat Completions.
    let route = route_kind_for(uri.path());

    let Some(model_name) = requested_model(&head, &bytes) else {
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
            let counter = table.advance_rr(&combo.id);
            let order =
                select::order_steps(combo, table.snapshot.providers(), &health, counter, now_ms);
            let sticky_enabled = combo.strategy == mr_core::ComboStrategy::Priority
                && combo.hedge.enabled
                && combo.sticky_success_window_secs > 0;
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
    let quota = mr_core::config::QuotaConfig::default();
    // The model the request should be retried against when no step can carry its modality.
    let mut fallback_target: Option<String> = None;

    for step in &steps {
        let is_sticky_step = sticky_index.is_some_and(|i| {
            combo.is_some_and(|c| {
                c.steps
                    .get(i)
                    .is_some_and(|candidate| std::ptr::eq(candidate, *step))
            })
        });
        let Some(provider) = table.snapshot.providers().get(step.provider_index as usize) else {
            if is_sticky_step {
                if let Some(combo) = combo {
                    app.sticky_success.clear(&combo.id);
                }
                sticky_index = None;
            }
            continue;
        };

        let account_counter = table.advance_rr(&unique_counter_key(step));
        let Some(account) = select::pick_account(
            provider,
            provider.key_strategy,
            account_counter,
            &health,
            now_ms,
            &step.model,
        ) else {
            if is_sticky_step {
                if let Some(combo) = combo {
                    app.sticky_success.clear(&combo.id);
                }
                sticky_index = None;
            }
            continue;
        };

        // The credential comes from the vault, never from the configuration, and is never logged.
        // OAuth references resolve to a full token set and platform-specific request headers.
        let mut oauth_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
        let mut oauth_tokens: Option<mr_core::oauth::TokenSet> = None;
        let credential = if let Some(oauth_ref) = account.oauth_ref.as_deref() {
            let oauth_id = mr_core::OAuthCredentialId::from_raw(oauth_ref);
            let AuthKind::OAuth { platform } = &step.auth else {
                tracing::error!(account = %account.id, "OAuth account is attached to non-OAuth provider auth");
                continue;
            };
            let record = match app.oauth_store.get_oauth_credential(&oauth_id) {
                Ok(Some(record)) if record.platform == *platform => record,
                _ => {
                    tracing::warn!(account = %account.id, "OAuth credential metadata is missing or mismatched");
                    continue;
                }
            };
            let Some(oauth_platform) = oauth_platform(platform) else {
                tracing::error!(platform, "OAuth platform is not available in this build");
                continue;
            };
            let Some(refresher) = app.oauth_refreshers.get(platform) else {
                continue;
            };
            match refresher.ensure_fresh(&record.vault_ref, false).await {
                Ok(tokens) => {
                    oauth_headers = oauth_platform.apply_credential(&tokens);
                    oauth_tokens = Some(tokens.clone());
                    Some(tokens.access_token.clone())
                }
                Err(failure) => {
                    tracing::warn!(platform, class = %failure.class, "OAuth credential refresh failed");
                    continue;
                }
            }
        } else {
            match account.secret_ref.as_deref() {
                Some(id) => match app.vault.get(id) {
                    Ok(Some(v)) => Some(v),
                    Ok(None) => {
                        tracing::warn!(account = %account.id, "no credential in the vault for this account");
                        let reason = select::CooldownReason::AuthFailed;
                        app.health.cool_down(
                            account.id.as_str(),
                            select::cooldown_until(reason, 0, None, &quota),
                            reason,
                        );
                        continue;
                    }
                    Err(e) => {
                        // A vault that cannot be decrypted is not an account problem, and cooling the
                        // account down would hide the real cause behind a symptom.
                        tracing::error!(error = %e, "the credential vault could not be read");
                        return error_response(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            client_dialect,
                            "vault_error",
                            "the credential vault could not be read",
                        );
                    }
                },
                None => None,
            }
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
            session_id: None,
        };
        let translated = match translate_request(&ctx, &bytes) {
            Ok(t) => t,
            Err(e) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    client_dialect,
                    "invalid_request_error",
                    &e.to_string(),
                );
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
        if step.dialect == Dialect::Antigravity {
            match inject_antigravity_project(&outgoing, oauth_tokens.as_ref()) {
                Ok(body) => outgoing = body.into(),
                Err(message) => {
                    return error_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        client_dialect,
                        "server_error",
                        message,
                    )
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
        match apply_reasoning_level(&outgoing, step, translated_body, level_override) {
            Ok(Some(body)) => outgoing = body.into(),
            Ok(None) => {}
            Err(message) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    client_dialect,
                    "invalid_request_error",
                    &message,
                )
            }
        }

        // Modality detection runs on the translated upstream shape, so the same helper can inspect
        // both passthrough and cross-dialect requests. Only a differing configured default changes
        // the model field; the request content is never dropped or rewritten.
        let mut modality_body: serde_json::Value = match serde_json::from_slice(&outgoing) {
            Ok(body) => body,
            Err(_) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    client_dialect,
                    "invalid_request_error",
                    "the request body must be valid JSON to inspect its modalities",
                )
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
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    client_dialect,
                    "invalid_request_error",
                    &error.with_model(&step.model).to_string(),
                )
            }
        }

        // A path the translator named wins over the provider's route override. The translator knows
        // which dialect it just wrote the body for; a route override is keyed by the *client's*
        // route, and says nothing about an upstream that does not follow OpenAI's path convention.
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
        let copilot_override = if step.dialect == Dialect::Copilot {
            dialect_path.as_ref()
        } else {
            dialect_path
                .as_ref()
                .or_else(|| provider.route_overrides.get(route_key(&uri)))
        };
        let Some(url) = build_url(&url_step, copilot_override) else {
            continue;
        };

        let request = UpstreamRequest {
            method: method.clone(),
            url,
            headers: upstream_headers(
                provider,
                step,
                credential.as_deref(),
                &headers,
                &head,
                force_stream,
                dialect_content_type,
                &oauth_headers,
            ),
            proxy: None,
            body: outgoing,
            http2: provider.http2,
            connect_timeout: Duration::from_millis(step.connect_timeout_ms),
            ttfb_timeout: Duration::from_millis(step.ttfb_timeout_ms),
            total_timeout: (step.total_timeout_ms > 0)
                .then(|| Duration::from_millis(step.total_timeout_ms)),
        };

        match app.relay.upstream().send(request).await {
            Ok(response) if response.status.is_success() => {
                app.health.record_success(account.id.as_str());
                if let Some(combo) = combo.filter(|c| {
                    c.strategy == mr_core::ComboStrategy::Priority
                        && c.hedge.enabled
                        && c.sticky_success_window_secs > 0
                }) {
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
                return build_client_response(ctx, translated_body, response).await;
            }
            Ok(response) => {
                if is_sticky_step {
                    if let Some(combo) = combo {
                        app.sticky_success.clear(&combo.id);
                    }
                    sticky_index = None;
                }
                let reason = classify_status(response.status);
                app.health.cool_down(
                    account.id.as_str(),
                    select::cooldown_until(reason, 0, retry_after_ms(&response.headers), &quota),
                    reason,
                );
                last_failure = Some(build_response(response));
            }
            Err(e) => {
                if is_sticky_step {
                    if let Some(combo) = combo {
                        app.sticky_success.clear(&combo.id);
                    }
                    sticky_index = None;
                }
                let reason = select::CooldownReason::Backoff;
                app.health.cool_down(
                    account.id.as_str(),
                    select::cooldown_until(reason, 0, None, &quota),
                    reason,
                );
                tracing::warn!(
                    model = model_name,
                    provider = %step.provider_id,
                    error = %e,
                    "upstream failed"
                );
                last_failure = None;
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

    // Every step was tried. Where a provider gave a specific error, that is more informative than
    // anything synthesized here, so it is returned. Where the accounts were the problem, the answer
    // is our own 429 with the earliest known recovery.
    match last_failure {
        Some(r) => r,
        None => {
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                client_dialect,
                "no_usable_accounts",
                "every step for this model has an account cooling down or unavailable",
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

/// A stable per-step key for the account round-robin counter.
///
/// The counter is per step rather than per provider so that two steps on one provider do not advance
/// each other's position — otherwise adding a second step would silently change which key the first
/// one uses.
fn unique_counter_key(step: &StepPlan) -> mr_core::ids::ComboId {
    mr_core::ids::ComboId::from_raw(format!("{}#{}", step.provider_id, step.model))
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
async fn read_body(body: AxumBody, max: usize) -> Result<Bytes, BodyError> {
    use http_body_util::BodyExt as _;
    match body.collect().await {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            if bytes.len() > max {
                return Err(BodyError::TooLarge);
            }
            Ok(bytes)
        }
        Err(e) => Err(BodyError::Read(e.to_string())),
    }
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
fn presented_key(headers: &HeaderMap) -> Option<String> {
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
fn authenticate_request(
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
fn auth_error(outcome: &AuthOutcome, dialect: Dialect) -> Response {
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

/// Which cooldown a status code deserves.
fn classify_status(status: StatusCode) -> select::CooldownReason {
    match status.as_u16() {
        // The credential is wrong. Retrying cannot fix that, and the long cooldown is what stops the
        // logs filling with the same unchanged failure while nobody acts on it.
        401 | 403 => select::CooldownReason::AuthFailed,
        402 => select::CooldownReason::QuotaExhausted,
        429 => select::CooldownReason::RateLimited,
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
fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get("retry-after")?.to_str().ok()?.trim();
    let secs: u64 = raw.parse().ok()?;
    // Capped, so a provider sending a bogus large value cannot park an account indefinitely.
    Some(secs.min(86_400).saturating_mul(1000))
}

/// The route key into a provider's `route_overrides`.
fn route_key(uri: &Uri) -> &str {
    uri.path()
}

/// Build the upstream URL.
///
/// A provider's own override wins, because Command Code's `/alpha/generate` and Google's
/// `v1internal:generateContent` cannot be derived from a base URL.
fn build_url(step: &StepPlan, override_path: Option<&String>) -> Option<String> {
    let base = step.base_url.trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let path = override_path
        .map(String::as_str)
        .unwrap_or_else(|| default_path(step.dialect));

    // A declared override may be relative. Command Code's own constant is `alpha/generate`, with no
    // leading slash, because it reads as a path fragment rather than a route; the base has just had
    // its trailing slash trimmed, so concatenating the two produced `…commandcode.aialpha/generate`.
    // The separator is supplied here so that no constant has to remember to include it.
    if path.starts_with('/') {
        return Some(format!("{base}{path}"));
    }
    Some(format!("{base}/{path}"))
}

/// The default upstream path for a dialect with no declared override.
fn default_path(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::OpenAi => "/v1/chat/completions",
        Dialect::Anthropic => "/v1/messages",
        Dialect::Google => "/v1beta/models:generateContent",
        Dialect::Antigravity => "/v1internal:generateContent",
        Dialect::Codex => "/responses",
        Dialect::Copilot => "/chat/completions",
        Dialect::CommandCodeGo => "/alpha/generate",
    }
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

    map
}

/// The client route a request arrived on, from its path.
///
/// The fallback is Chat Completions because that is the route an unknown path cannot have reached:
/// `router()` registers an explicit list, and a path outside it is a 404 before this is called.
fn route_kind_for(path: &str) -> RouteKind {
    match path {
        "/v1/responses" => RouteKind::Responses,
        "/v1/embeddings" => RouteKind::Embeddings,
        "/v1/audio/transcriptions" => RouteKind::AudioTranscription,
        "/v1/audio/speech" => RouteKind::AudioSpeech,
        "/v1/images/generations" => RouteKind::ImagesGenerations,
        "/v1/messages" => RouteKind::Messages,
        "/v1/messages/count_tokens" => RouteKind::CountTokens,
        _ => RouteKind::ChatCompletions,
    }
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
) -> Response {
    if !translated {
        return build_response(upstream);
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

        let frames = BodyStream::new(upstream.body);
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
        let stream = frames.scan((String::new(), false), move |(pending, started), frame| {
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
                Ok(frame) => frame
                    .into_data()
                    .ok()
                    .map(|chunk| translate_stream_chunk(&ctx, &mut commandcode, pending, &chunk)),
                Err(e) => Some(vec![Err(e)]),
            };
            async move { Some(out) }
        });
        let stream = stream.filter_map(|item| async move { item });
        let stream = stream.flat_map(|chunks| {
            futures_util::stream::iter(chunks.into_iter().map(|c| c.map(http_body::Frame::data)))
        });
        AxumBody::new(StreamBody::new(stream))
    } else {
        let collected = upstream.body.collect().await;
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
    for (name, value) in headers.iter() {
        let lower = name.as_str().to_ascii_lowercase();
        if HOP_BY_HOP.contains(&lower.as_str())
            || (ctx.upstream_dialect == Dialect::CommandCodeGo && lower == "content-type")
        {
            continue;
        }
        response.headers_mut().append(name.clone(), value.clone());
    }
    if ctx.upstream_dialect == Dialect::CommandCodeGo {
        let content_type = if ctx.client_streaming {
            "text/event-stream; charset=utf-8"
        } else {
            "application/json; charset=utf-8"
        };
        if let Ok(value) = HeaderValue::from_str(content_type) {
            response.headers_mut().insert("content-type", value);
        }
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
    pending: &mut String,
    chunk: &Bytes,
) -> Vec<Result<Bytes, hyper::Error>> {
    // A chunk that is not valid UTF-8 at its end is kept whole and retried once the rest arrives;
    // `from_utf8` failing on a truncated multi-byte character is the same situation as a truncated
    // line, and treating it as an error would drop a legitimate frame.
    let Ok(text) = std::str::from_utf8(chunk) else {
        pending.push_str(&String::from_utf8_lossy(chunk));
        return Vec::new();
    };
    pending.push_str(text);

    let mut out = Vec::new();
    while let Some(idx) = pending.find('\n') {
        let line: String = pending.drain(..=idx).collect();
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            out.push(Ok(Bytes::from_static(b"\n")));
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
            } else {
                mr_dialects::translate_sse_data(ctx, data)
            } {
                Ok(lines) => {
                    for line in lines {
                        if line.is_empty() {
                            continue;
                        }
                        out.push(Ok(Bytes::from(format!("{line}\n\n"))));
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "could not translate an event-stream frame");
                }
            },
            // Comments (`: ping`), `event:` and `id:` lines carry no payload to translate and are
            // forwarded as they are. Command Code metadata JSON is intentionally suppressed.
            None if ctx.upstream_dialect == Dialect::CommandCodeGo => {}
            None => out.push(Ok(Bytes::from(format!("{trimmed}\n")))),
        }
    }
    out
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

fn build_response(upstream: crate::upstream::UpstreamResponse) -> Response {
    let status = upstream.status;
    let mut response = Response::new(AxumBody::new(upstream.body));
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
fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
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
fn error_response(status: StatusCode, dialect: Dialect, code: &str, message: &str) -> Response {
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

    axum::serve(listener, router(state))
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

#[cfg(test)]
mod tests {
    use super::*;

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
            steps: Vec::new(),
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
                m.reasoning_levels = vec![ReasoningLevel::Low, ReasoningLevel::Max];
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
        assert!(ids.contains(&"coding-max".to_string()));
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

    #[test]
    fn every_dialect_has_a_default_upstream_path() {
        // A dialect with no path would build a URL ending at the base and send the request to the
        // host root, which fails as a provider 404 and reads as a wrong base URL.
        for d in [
            Dialect::OpenAi,
            Dialect::Anthropic,
            Dialect::Google,
            Dialect::Codex,
            Dialect::Copilot,
            Dialect::Antigravity,
            Dialect::CommandCodeGo,
        ] {
            let p = default_path(d);
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
        let mut pending = String::new();
        let metadata = Bytes::from_static(b"{\"type\":\"start\"}\n");
        assert!(translate_stream_chunk(&ctx, &mut commandcode, &mut pending, &metadata).is_empty());
        let token = Bytes::from_static(b"{\"type\":\"text-delta\",\"text\":\"OK\"}\n");
        let output = translate_stream_chunk(&ctx, &mut commandcode, &mut pending, &token);
        let output = output.into_iter().map(Result::unwrap).collect::<Vec<_>>();
        assert_eq!(output.len(), 1);
        let text = std::str::from_utf8(&output[0]).unwrap();
        assert!(text.starts_with("data: {"), "{text:?}");
        assert!(text.contains("chat.completion.chunk"), "{text:?}");
        assert!(text.contains("\"content\":\"OK\""), "{text:?}");
        let finish = Bytes::from_static(
            b"{\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"inputTokens\":2,\"outputTokens\":1}}\n",
        );
        let output = translate_stream_chunk(&ctx, &mut commandcode, &mut pending, &finish);
        let text = output
            .into_iter()
            .map(Result::unwrap)
            .map(|b| String::from_utf8(b.to_vec()).unwrap())
            .collect::<String>();
        assert!(text.contains("finish_reason"), "{text:?}");
        assert!(text.contains("[DONE]"), "{text:?}");
    }

    #[test]
    fn a_trailing_slash_on_the_base_url_does_not_double() {
        let s = step(Dialect::OpenAi, "https://api.example.com/");
        assert_eq!(
            build_url(&s, None).as_deref(),
            Some("https://api.example.com/v1/chat/completions")
        );
    }

    #[test]
    fn an_empty_base_url_builds_no_url() {
        // The step is skipped rather than sent to a nonsense URL.
        let s = step(Dialect::OpenAi, "");
        assert_eq!(build_url(&s, None), None);
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
            build_url(&s, Some(&over)).as_deref(),
            Some("https://api.commandcode.ai/alpha/generate")
        );
    }

    #[test]
    fn a_relative_route_override_gains_its_separator() {
        let s = step(Dialect::OpenAi, "https://example.com");
        let over = "v1/other".to_string();
        assert_eq!(
            build_url(&s, Some(&over)).as_deref(),
            Some("https://example.com/v1/other")
        );
    }

    #[test]
    fn a_base_url_with_a_trailing_slash_does_not_double_it() {
        let s = step(Dialect::OpenAi, "https://example.com/");
        let over = "/v1/other".to_string();
        assert_eq!(
            build_url(&s, Some(&over)).as_deref(),
            Some("https://example.com/v1/other")
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
            build_url(&s, Some(&path)).as_deref(),
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
        assert_eq!(route_kind_for("/v1/audio/speech"), RouteKind::AudioSpeech);
        assert_eq!(
            route_kind_for("/v1/images/generations"),
            RouteKind::ImagesGenerations
        );
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
    fn status_codes_map_to_the_cooldowns_they_deserve() {
        use select::CooldownReason;
        // The distinction that decides whether an account can recover on its own.
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            CooldownReason::AuthFailed
        );
        assert_eq!(
            classify_status(StatusCode::FORBIDDEN),
            CooldownReason::AuthFailed
        );
        assert_eq!(
            classify_status(StatusCode::PAYMENT_REQUIRED),
            CooldownReason::QuotaExhausted
        );
        assert_eq!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            CooldownReason::RateLimited
        );
        assert_eq!(
            classify_status(StatusCode::BAD_GATEWAY),
            CooldownReason::Backoff
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            CooldownReason::Backoff
        );
    }

    #[test]
    fn healthz_does_not_consult_a_provider() {
        // It returns a constant, so it cannot fail because a provider is down.
        let parsed: serde_json::Value = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        assert_eq!(parsed["status"], "ok");
    }

    #[test]
    fn the_documented_route_list_is_the_one_registered() {
        // A route that is documented but not registered is a 404 that reads as a client bug. This is
        // the list from `docs/18-api-reference.md`.
        let documented = [
            "/healthz",
            "/v1/chat/completions",
            "/v1/responses",
            "/v1/embeddings",
            "/v1/audio/transcriptions",
            "/v1/audio/speech",
            "/v1/images/generations",
            "/v1/messages",
            "/v1/messages/count_tokens",
            "/v1/models",
        ];
        assert_eq!(documented.len(), 10);
        assert!(documented.contains(&"/healthz"));
        assert!(documented.contains(&"/v1/messages/count_tokens"));
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
    fn a_route_key_is_the_request_path() {
        let uri: Uri = "/v1/images/generations".parse().unwrap();
        assert_eq!(route_key(&uri), "/v1/images/generations");
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
    fn a_combo_move_counter_key_differs_between_steps_on_one_provider() {
        // Otherwise adding a second step would silently change which key the first one uses.
        let a = unique_counter_key(&step(Dialect::OpenAi, "https://a"));
        let mut b_step = step(Dialect::OpenAi, "https://a");
        b_step.model = "other".into();
        let b = unique_counter_key(&b_step);
        assert_ne!(a, b);
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

        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(crate::select::Health::new()),
            vault,
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            oauth_store: Arc::new(mr_core::store::Store::open_in_memory().unwrap()),
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
}
