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
//! the race, under the combo's `midstream_policy`. Of the four `WinnerFailMode`s only
//! `pick-highest-passed` is implemented; the others are accepted configuration and are reported in
//! the log.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
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
use crate::proxy::{self, ProxyEndpoint};
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
    /// Per-key quota admission and settlement.
    ///
    /// The ledger it reads and writes is the same `key_usage` table the panel reads, so a cap that
    /// is being enforced and a cap that is being displayed are the same number.
    pub quota: Arc<crate::quota::QuotaManager>,
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

    // The inputs every attempt needs. The same context serves the ordinary chain below and the
    // hedged race, so the two cannot drift apart.
    let attempt_ctx = AttemptContext {
        app: &app,
        table: &table,
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
                    HedgeOutcome::Won(response) => return response,
                    HedgeOutcome::Fatal(response) => return response,
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
        match attempt_step(&attempt_ctx, step, is_sticky).await {
            Attempt::Answer(response) => return response,
            Attempt::Failed {
                response,
                sticky_cleared,
            } => {
                if sticky_cleared {
                    sticky_index = None;
                }
                last_failure = response;
            }
            Attempt::Skip {
                sticky_cleared,
                fallback_target: target,
            } => {
                if sticky_cleared {
                    sticky_index = None;
                }
                if target.is_some() {
                    fallback_target = target;
                }
            }
            Attempt::Fatal { response } => return response,
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
/// What one attempt at a step produced.
enum Attempt {
    /// The step was not usable; the caller should try the next one.
    Skip {
        /// Whether the sticky preference was cleared by this attempt.
        sticky_cleared: bool,
        /// The configured default to retry against, when the step could not carry the modality.
        fallback_target: Option<String>,
    },
    /// The step answered with a success; the response is ready for the client.
    Answer(Response),
    /// The step failed.
    Failed {
        /// The failure response, when there is one to show the client.
        response: Option<Response>,
        /// Whether the sticky preference was cleared by this attempt.
        sticky_cleared: bool,
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
    table: &'a RouteTable,
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
async fn attempt_step(actx: &AttemptContext<'_>, step: &StepPlan, is_sticky_step: bool) -> Attempt {
    let app = actx.app;
    let table = actx.table;
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
        };
    };

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
    // The answer is produced before an account is chosen: there is no upstream to authenticate to,
    // no credential to read, and no request to bill. It is an ordinary local answer — the same
    // shape an upstream failure takes — so a step that fails further down still falls through to
    // the next one in the ordinary way.
    if route == RouteKind::CountTokens && step.dialect != Dialect::Anthropic {
        return Attempt::Answer(crate::count_tokens::estimate_response(bytes));
    }

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
            sticky_cleared = true;
        }
        return Attempt::Skip {
            sticky_cleared,
            fallback_target: fallback_target.take(),
        };
    };

    // The credential comes from the vault, never from the configuration, and is never logged.
    // OAuth references resolve to a full token set and platform-specific request headers.
    let mut oauth_headers: Vec<(HeaderName, HeaderValue)> = Vec::new();
    let mut oauth_tokens: Option<mr_core::oauth::TokenSet> = None;
    let credential = if let Some(oauth_ref) = account.oauth_ref.as_deref() {
        let oauth_id = mr_core::OAuthCredentialId::from_raw(oauth_ref);
        let AuthKind::OAuth { platform } = &step.auth else {
            tracing::error!(account = %account.id, "OAuth account is attached to non-OAuth provider auth");
            return Attempt::Skip {
                sticky_cleared,
                fallback_target: fallback_target.take(),
            };
        };
        let record = match app.oauth_store.get_oauth_credential(&oauth_id) {
            Ok(Some(record)) if record.platform == *platform => record,
            _ => {
                tracing::warn!(account = %account.id, "OAuth credential metadata is missing or mismatched");
                return Attempt::Skip {
                    sticky_cleared,
                    fallback_target: fallback_target.take(),
                };
            }
        };
        let Some(oauth_platform) = oauth_platform(platform) else {
            tracing::error!(platform, "OAuth platform is not available in this build");
            return Attempt::Skip {
                sticky_cleared,
                fallback_target: fallback_target.take(),
            };
        };
        let Some(refresher) = app.oauth_refreshers.get(platform) else {
            return Attempt::Skip {
                sticky_cleared,
                fallback_target: fallback_target.take(),
            };
        };
        match refresher.ensure_fresh(&record.vault_ref, false).await {
            Ok(tokens) => {
                oauth_headers = oauth_platform.apply_credential(&tokens);
                oauth_tokens = Some(tokens.clone());
                Some(tokens.access_token.clone())
            }
            Err(failure) => {
                tracing::warn!(platform, class = %failure.class, "OAuth credential refresh failed");
                return Attempt::Skip {
                    sticky_cleared,
                    fallback_target: fallback_target.take(),
                };
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
                        select::cooldown_until(reason, 0, None, quota),
                        reason,
                    );
                    return Attempt::Skip {
                        sticky_cleared,
                        fallback_target: fallback_target.take(),
                    };
                }
                Err(e) => {
                    // A vault that cannot be decrypted is not an account problem, and cooling the
                    // account down would hide the real cause behind a symptom.
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
    if step.dialect == Dialect::Antigravity {
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
    let Some(url) = build_url(&url_step, url_path) else {
        return Attempt::Skip {
            sticky_cleared,
            fallback_target: fallback_target.take(),
        };
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

    let request = UpstreamRequest {
        method: method.clone(),
        url,
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
        proxy: proxy_endpoint,
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
            Attempt::Answer(build_client_response(ctx, translated_body, response, ticket).await)
        }
        Ok(response) => {
            if is_sticky_step {
                if let Some(combo) = combo {
                    app.sticky_success.clear(&combo.id);
                }
                sticky_cleared = true;
            }
            let reason = classify_status(response.status);
            app.health.cool_down(
                account.id.as_str(),
                select::cooldown_until(reason, 0, retry_after_ms(&response.headers), quota),
                reason,
            );
            Attempt::Failed {
                response: Some(build_response(response, None)),
                sticky_cleared,
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
                account.id.as_str(),
                select::cooldown_until(reason, 0, None, quota),
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
            }
        }
    }
}

/// One pending attempt in a race, boxed so that several of them can be polled at once.
type PendingAttempt<'a> = Pin<Box<dyn Future<Output = (usize, Attempt)> + Send + 'a>>;

/// The outcome of a hedged race.
enum HedgeOutcome {
    /// A participant won; the response is ready for the client.
    Won(Response),
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
    Fatal(Response),
}

/// How a race participant is named in the recovery boundary.
///
/// `provider/model`, which is what the client asked for and what the operator wrote in the combo.
fn participant_id(step: &StepPlan) -> String {
    format!("{}/{}", step.provider_id, step.model)
}

/// The participants that passed the race, in priority order, ready to answer in the winner's place.
///
/// "Passed" is the eligibility gate, not "answered eventually": a participant that replied after
/// the window keeps no priority protection, and the operator's rule is about the ones that fit the
/// latency requirement. The winner itself is not in the list — it is the one that died.
fn passed_replacements(
    outcomes: &mut [Option<Attempt>],
    states: &[select::Participant],
    winner: usize,
    window_ms: u64,
    participants: &[&StepPlan],
) -> Vec<crate::recovery::Replacement> {
    let mut passed = Vec::new();
    for (index, outcome) in outcomes.iter_mut().enumerate() {
        if index == winner {
            continue;
        }
        if !states[index].replied_in_window(window_ms) {
            continue;
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
/// highest-priority one, which is the operator's rule. Of the four `WinnerFailMode`s only
/// `pick-highest-passed` is implemented; the others are accepted configuration and are reported in
/// the log.
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
            return HedgeOutcome::Fatal(response);
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
            let replacements =
                passed_replacements(&mut outcomes, &states, winner, window_ms, participants);

            // Only one winner-fail mode is implemented. The other three stay accepted
            // configuration, and the relay says so rather than pretending.
            let mode = &hedge.on_winner_fail;
            if !matches!(mode, mr_core::dialect::WinnerFailMode::PickHighestPassed) {
                tracing::warn!(
                    mode = mode.as_str(),
                    "the configured winner-fail mode is not implemented yet; the race is served as \
                     pick-highest-passed, which takes the highest-priority participant that passed"
                );
            }

            let policy = actx
                .combo
                .map(|combo| combo.midstream_policy)
                .unwrap_or_default();
            let winner_id = participant_id(participants[winner]);
            // The winner's body is wrapped so that the relay owns the stream: a failure after the
            // response arrived can then be recovered instead of ending the client's answer.
            return HedgeOutcome::Won(crate::recovery::wrap(
                response,
                replacements,
                policy,
                actx.client_dialect,
                winner_id,
            ));
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
        Dialect::Google => Some("v1beta"),
        Dialect::Antigravity => Some("v1internal"),
        Dialect::Codex => None,
        Dialect::Copilot => None,
        Dialect::CommandCodeGo => None,
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
        let (mut pa, mut pb) = (String::new(), String::new());

        let run = |t: &mut mr_dialects::commandcode::Translator,
                   pending: &mut String,
                   frame: &'static str|
         -> String {
            let out =
                translate_stream_chunk(&ctx, t, pending, &Bytes::from_static(frame.as_bytes()));
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
        let mut pending = String::new();
        for frame in [
            "{\"type\":\"tool-input-start\",\"id\":\"c1\",\"toolName\":\"f\"}\n",
            "{\"type\":\"finish\",\"finishReason\":\"tool-calls\"}\n",
        ] {
            let _ = translate_stream_chunk(
                &ctx,
                &mut t,
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
            (RouteKind::AudioSpeech, "/v1/audio/speech"),
            (RouteKind::ImagesGenerations, "/v1/images/generations"),
            (RouteKind::Models, "/v1/models"),
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

        let store = Arc::new(mr_core::store::Store::open_in_memory().unwrap());
        let state = AppState {
            relay,
            no_key_required: true,
            health: Arc::new(crate::select::Health::new()),
            vault,
            oauth_refreshers: Arc::new(std::collections::HashMap::new()),
            quota: Arc::new(crate::quota::QuotaManager::new(Arc::clone(&store))),
            oauth_store: store,
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
            oauth_store: store,
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
            oauth_store: store,
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
            oauth_store: store,
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
            oauth_store: store,
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
            oauth_store: store,
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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
                // The declared length is longer than what is sent, so the body ends in an error
                // rather than in an end-of-stream.
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                    body.len() + 64,
                    body
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
            }
        }
    }

    /// A relay whose only route is a two-step combo whose winner dies mid-stream.
    ///
    /// The replacement answers *before* the winner does, and the winner is the higher-priority
    /// participant, so the winner is committed on priority and the replacement is already in the
    /// race's result set when it is. That is the situation `pick-highest-passed` is for.
    async fn recovery_state(spec: RecoverySpec) -> (AppState, Arc<std::sync::Mutex<Vec<String>>>) {
        let (winner_url, winner_seen) =
            dying_upstream(spec.winner_delay_ms, spec.winner_body.clone()).await;
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
            oauth_store: store,
            sticky_success: Arc::new(crate::sticky::StickySuccess::default()),
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

        // The log is captured for the duration of the request: the mode is not implemented, and the
        // operator has to be told that the request was served as `pick-highest-passed`.
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
            logged.contains("rerun-excluding"),
            "the unimplemented mode was not reported: {logged}"
        );
        assert!(
            logged.contains("not implemented yet"),
            "the unimplemented mode was not reported: {logged}"
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
}
