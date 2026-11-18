//! GPU rental endpoints: what the hardware is, what a checkpoint needs, and what is already rented.
//!
//! Six routes, merged into the guarded router by `api::guarded_router`:
//!
//! * `GET  /api/gpu/catalog`  — the two catalogs (platforms → GPUs, plus the quant and KV tables);
//! * `GET  /api/lanes`        — what each GPU host can be used for, and what it refuses;
//! * `POST /api/gpu/probe`    — read a Hugging Face repository and say what it would take to serve it;
//! * `GET  /api/gpu/rentals`  — the configured rental accounts and the providers derived from them;
//! * `POST /api/gpu/apply`    — create a rental account, or turn one into a serving provider;
//! * `POST /api/gpu/launch-plan` — what starting this checkpoint on this lane would cost and need.
//!
//! # What this module will not say
//!
//! Almost everything here is imported from `mr_core::gpu`, and the rule the catalog is built on is
//! kept in the wire format: a fact is `documented`, `observed`, or `unverified`, and "unknown" is a
//! value rather than a guess. `free_plan` is the sharp edge of that. Only one card in the catalog is
//! *observed* to be unavailable on the free plan; every other card's free-plan state is `unknown`,
//! and `unknown` is passed through as `unknown`. A panel that renders such a card as `(Paid)` is
//! inventing a price the catalog never learned, so the catalog carries the three-state value and a
//! note saying so, and `rentals` reports an account's plan as the operator declared it, never
//! inferred from the card.
//!
//! # Where the data comes from
//!
//! Rental accounts are a typed field on the configuration, `Config::rental_accounts`, so they are
//! read and written through it. The wire shape is [`RentalAccount`]'s own: a configuration with no
//! such key has no accounts, and an entry that does not parse fails the whole load with the file
//! name rather than being silently dropped. Deployments live in `Provider::extra["rental"]`, which
//! is the layout `mr_core::rental::parse_deployment` already reads, and are written back through
//! `AppState::commit` like every other configuration change.
//!
//! # Credentials
//!
//! Nothing here ever returns, logs, or formats a secret. A rental account's platform credential is
//! written once, when the account is created, at the vault id `rental:<account id>`, as the
//! combined `wk-<id>.ws-<secret>` string the three existing Modal readers already split on
//! (`split_once('.')`). A Hugging Face token is a separate entity with a separate id: it is
//! written to `hf:<token id>` by `create-hf-token`, reported as `secret_ref` and a fingerprint,
//! and named by each deployment that needs it (`deployment.hf_token`). It is deliberately not
//! derived from an account, because one token commonly serves deployments on several accounts and
//! one account may host deployments that need different tokens. Both ids are derived from their
//! own slug rather than accepted from the client. Creating a deployment performs no vault write at
//! all: the providers it derives point at the credentials the account and token already own, so
//! applying another deployment cannot rotate or duplicate them, and a credential that is missing
//! is refused instead of being given a placeholder.
//!
//! # The probe, and the gated-repository fallback
//!
//! A gated checkpoint answers `401` with `X-Error-Code: GatedRepo` for `config.json`, so its
//! architecture cannot be read directly. Most gated repositories on the Hub are quantised copies of
//! a public base model, and their card names it in `cardData.base_model`. The probe therefore has
//! three ordered sources of architecture facts, and it says which one it used:
//!
//! 1. the repository's own `config.json` → `arch.source = "repo"`;
//! 2. the base repository's `config.json`, fetched anonymously → `arch.source = "base_model"`,
//!    carrying the relation and whether that relation is one that preserves the architecture;
//! 3. neither → `arch.source = "unavailable"` and a reason.
//!
//! The metadata call (`GET /api/models/<repo>`) is always anonymous: it is where the gating state
//! and the base-model hint come from, and the Hub answers it without a token even for gated
//! repositories. A caller's Hugging Face token is used only on the `config.json` fetch, only in an
//! `Authorization: Bearer` header, and never in a URL — URLs end up in logs. Every response is
//! passed through [`scrub_secret`] before it leaves the process, so "the token is not in the body"
//! holds by construction even if a future error path were to interpolate it.

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use mr_core::config::Config;
use mr_core::dialect::{AuthKind, Dialect};
use mr_core::gpu::arch::{
    arch_facts_from_config_bytes, base_model_hint_from_hf_api, gated_state_from_hf_api,
    params_from_hf_api, weights_bytes_from_param_count, ArchFacts, GatedState,
};
use mr_core::gpu::catalog::{
    gpu_by_id, kv_cache_dtype_by_id, platforms, quant_method_by_id, quant_methods,
    resolve_kv_dtype_for, resolve_quant_for, ComputeCapability, GpuSpec, QuantSupport,
    CATALOG_CHECKED_AT, CUDA_GPUS_URL, ISSUE_4_URL, MODAL_GPU_DOCS_URL, MODAL_PRICING_URL,
    VLLM_KV_CACHE_URL, VLLM_QUANT_MATRIX_URL,
};
use mr_core::gpu::sizing::{size_deployment, SizingRequest, SizingResult};
use mr_core::lane::{plan_lane, LaneKind, Traffic};
use mr_core::model::ModelEntry;
use mr_core::quota::http::{QuotaHttp, QuotaRequest};
use mr_core::rental::{
    hf_token_vault_id, parse_deployment, plan_launch, BatchFill, BatchShape, HuggingFaceToken,
    RentalAccount, RentalDeployment, RentalMode, RentalPlan, RentalPlatform, RentalSchedule,
    RequantizePlan,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::api::{ApiError, ApiResult, JsonBody};
use crate::state::AppState;

/// Vault id of an account's platform credential.
///
/// `rental:<account id>` is not an invention of this module: `docs/research/ARCHITECTURE-gpu-rentals.md`
/// §4 shows `"secret_ref": "rental:mana-modal"` for the account whose id is `mana-modal`, and the
/// conductor derives the ref from the id rather than accepting one, so the panel, the CLI and the
/// relay cannot disagree about where a credential lives.
///
/// The value stored there is the **combined** Modal credential, `wk-<token_id>.ws-<token_secret>`.
/// That is deliberate: `relay/server.rs`, `api.rs` and `mr-cli/provider.rs` all authenticate by
/// reading the secret and `split_once('.')` into the `modal-key` / `modal-secret` headers, so a
/// structured bundle would have to be re-encoded at every one of those call sites.
fn rental_credential_id(account_id: &str) -> String {
    format!("rental:{account_id}")
}

/// Vault id where an account's Modal API token pair is stored.
///
/// Separate from [`rental_credential_id`]: the proxy pair authenticates the relay's inference
/// requests, the API pair authenticates the Modal CLI/API, and the two are never merged into one
/// entry.
fn rental_api_credential_id(account_id: &str) -> String {
    format!("rental:{account_id}:api")
}

/// **Legacy.** Vault id where a Hugging Face token was stored when it belonged to an account.
///
/// New tokens live at [`hf_token_vault_id`] and are named by a deployment. This id is still read
/// for an account written before that split, so an existing gated-repository setup keeps probing.
fn rental_hf_token_id(account_id: &str) -> String {
    format!("rental:{account_id}:hf")
}

/// Whether a string is shaped like a combined Modal credential.
///
/// Modal's dashboard shows a token id (`wk-…`) and a token secret (`ws-…`), and the relay splits the
/// stored string on the single dot between them. A value with no dot, or with more than one, would
/// authenticate as a mangled key rather than fail loudly, so it is rejected at the door with an
/// explanation instead of being stored and discovered later as a 401.
fn modal_credential_problem(value: &str) -> Option<String> {
    let mut parts = value.split('.');
    let id = parts.next().unwrap_or_default();
    let secret = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Some(
            "the credential contains more than one `.`; the relay splits it on the first dot, so a \
             second one would silently corrupt the secret half"
                .to_owned(),
        );
    }
    if id.trim().is_empty() || secret.trim().is_empty() {
        return Some(
            "the credential must be `<token id>.<token secret>`, the two values Modal's dashboard \
             shows for a proxy auth token"
                .to_owned(),
        );
    }
    if value.chars().any(char::is_whitespace) {
        return Some("the credential contains whitespace".to_owned());
    }
    None
}

/// Whether a string is shaped like a combined Modal **API** credential.
///
/// Modal shows two credential kinds with the same two-half shape: proxy auth tokens (`wk-…` /
/// `ws-…`) for endpoint requests, and API tokens (`ak-…` / `as-…`) for the Modal API and CLI.
/// This validator is for the second kind; the pair is stored as its own vault entry, never merged
/// with the proxy pair.
fn modal_api_credential_problem(value: &str) -> Option<String> {
    let mut parts = value.split('.');
    let id = parts.next().unwrap_or_default();
    let secret = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Some(
            "the API credential contains more than one `.`; it is split on the first dot, so a \
             second one would silently corrupt the secret half"
                .to_owned(),
        );
    }
    if !id.starts_with("ak-") || !secret.starts_with("as-") {
        return Some(
            "the Modal API credential must be `<api token id>.<api token secret>` — `ak-…` and \
             `as-…`, as Modal's dashboard shows them. The proxy credential (`wk-…` / `ws-…`) \
             is a different pair and belongs in the account's proxy-token fields"
                .to_owned(),
        );
    }
    if id.trim().is_empty() || secret.trim().is_empty() || value.chars().any(char::is_whitespace) {
        return Some("the API credential contains whitespace".to_owned());
    }
    None
}

/// The Hub's metadata endpoint. Anonymous, always.
const HF_API_ROOT: &str = "https://huggingface.co/api/models";

/// The Hub's direct file endpoint. `{repo}` and `{revision}` are filled in by the caller.
///
/// This is `https://huggingface.co/{repo}/raw/{revision}/config.json`, **not** the better-known
/// `/resolve/{revision}/config.json`. The `resolve` path does not return the file: it answers
/// `HTTP 307` with a `Location` pointing at `/api/resolve-cache/models/...`, and the shared
/// outbound client in `mr_core::quota::http` is a bare `hyper` client that does not follow
/// redirects. Verified against the live Hub on 2026-11-12: `resolve/main/config.json` for
/// `Qwen/Qwen3.8-27B` returned `307` with the cache `Location`, while
/// `raw/main/config.json` returned `200` with `content-length: 4312`. The `raw` path is
/// same-origin and needs no redirect machinery, which keeps the token-bearing request from
/// being replayed against a CDN host.
const HF_FILE_ROOT: &str = "https://huggingface.co";

/// Contexts at which a sizing answer reports session counts, filtered to what fits.
const SIZING_CONTEXTS: [u64; 4] = [8_192, 32_768, 131_072, 1_048_576];

/// The routes this module owns.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/gpu/catalog", get(get_catalog))
        .route("/api/lanes", get(get_lanes))
        .route("/api/gpu/probe", post(post_probe))
        .route("/api/gpu/rentals", get(get_rentals))
        .route("/api/gpu/apply", post(post_apply))
        .route("/api/gpu/launch-plan", post(post_launch_plan))
}

// ---------------------------------------------------------------------------
// GET /api/gpu/catalog
// ---------------------------------------------------------------------------

async fn get_catalog() -> Json<Value> {
    Json(catalog_body())
}

/// The two catalogs, serialised straight from the `'static` tables.
///
/// Every value is a `Serialize`-only response shape in `mr_core::gpu::catalog`, so it is converted
/// once here and never read back into a typed form.
pub fn catalog_body() -> Value {
    let platform_rows: Vec<Value> = platforms()
        .iter()
        .map(|platform| {
            let plans: Vec<Value> = platform
                .plans
                .iter()
                .map(|plan| json!({"id": plan.id, "label": plan.label}))
                .collect();
            let plan_limits: Vec<Value> = platform
                .plan_limits
                .iter()
                .map(|limit| {
                    json!({
                        "id": limit.id,
                        "label": limit.label,
                        "usd_per_month": limit.usd_per_month,
                        "included_credits_usd": limit.included_credits_usd,
                        "gpu_concurrency": limit.gpu_concurrency,
                        "seats": limit.seats,
                        "source": limit.source.as_str(),
                        "source_url": limit.source_url,
                    })
                })
                .collect();
            let gpus: Vec<Value> = platform.gpus.iter().map(gpu_row).collect();
            json!({
                "id": platform.id,
                "label": platform.label,
                "docs_url": platform.docs_url,
                "source_url": platform.source_url,
                "checked_at": platform.checked_at,
                "plans": plans,
                "deployment": json!({
                    "programmatic": platform.deployment.programmatic,
                    "explain": platform.deployment.explain,
                    "cli": platform.deployment.cli,
                    "sdk": platform.deployment.sdk,
                    "multi_gpu_syntax": platform.deployment.multi_gpu_syntax,
                    "cli_options": platform.deployment.cli_options,
                    "strategies": platform.deployment.strategies,
                    "has_detach_flag": platform.deployment.has_detach_flag,
                }),
                "max_concurrent_containers": json!({
                    "free": platform.max_concurrent_containers.free,
                    "paid": platform.max_concurrent_containers.paid,
                    "source": platform.max_concurrent_containers.source.as_str(),
                    "source_url": platform.max_concurrent_containers.source_url,
                }),
                "plan_limits": plan_limits,
                "payment_method_required": platform.payment_method_required,
                "max_containers_starter": platform.max_containers_starter,
                "scaledown_window": json!({
                    "min_seconds": platform.scaledown_window.min_seconds,
                    "default_seconds": platform.scaledown_window.default_seconds,
                    "max_seconds": platform.scaledown_window.max_seconds,
                }),
                "gpus": gpus,
            })
        })
        .collect();

    let quant_rows: Vec<Value> = quant_methods()
        .iter()
        .map(|method| {
            json!({
                "id": method.id,
                "label": method.label,
                "bytes_per_param": method.bytes_per_param,
                "min_compute_capability": method.min_compute_capability,
                "source": method.source.as_str(),
                "source_url": method.source_url,
                "notes": method.notes,
            })
        })
        .collect();

    let kv_rows: Vec<Value> = mr_core::gpu::catalog::KV_CACHE_DTYPES
        .iter()
        .map(|dtype| {
            json!({
                "id": dtype.id,
                "label": dtype.label,
                "bytes_per_element": dtype.bytes_per_element,
                "follows_model_dtype": dtype.follows_model_dtype,
                "min_cuda": dtype.min_cuda,
                "source": dtype.source.as_str(),
                "source_url": dtype.source_url,
                "notes": dtype.notes,
            })
        })
        .collect();

    json!({
        "checked_at": CATALOG_CHECKED_AT,
        "platforms": platform_rows,
        "quant_methods": quant_rows,
        "kv_cache_dtypes": kv_rows,
        "sources": {
            "modal_pricing": MODAL_PRICING_URL,
            "modal_gpu_docs": MODAL_GPU_DOCS_URL,
            "cuda_gpus": CUDA_GPUS_URL,
            "vllm_quant_matrix": VLLM_QUANT_MATRIX_URL,
            "vllm_kv_cache": VLLM_KV_CACHE_URL,
            "design": ISSUE_4_URL,
        },
        "notes": [
            "`free_plan` is a three-state value: `available`, `unavailable`, or `unknown`. It is \
             `unavailable` only where that was observed (H100, spelled `H100!` on the free plan); \
             every other card is `unknown`, which is not a synonym for `paid` and must not be \
             rendered as one.",
            "`source` on a card is the provenance of its price and VRAM figures; `vram_source` is \
             the provenance of the VRAM figure alone, and is absent when the catalog has none.",
            "`checked_at` is the date the catalog was last reconciled with the URLs in `sources`, \
             not the date of this request.",
        ],
    })
}

/// One GPU card, in the shape the panel reads.
fn gpu_row(gpu: &GpuSpec) -> Value {
    json!({
        "id": gpu.id,
        "label": gpu.label,
        "vram_gib": gpu.vram_gib,
        "vram_bytes": gpu.vram_bytes,
        "vram_source": gpu.vram_source.map(|source| source.as_str()),
        "usable_vram_gib": gpu.usable_vram_gib,
        "usable_vram_bytes": gpu.usable_vram_bytes,
        "price_usd_per_hour": gpu.price_usd_per_hour,
        "price_usd_per_second": gpu.price_usd_per_second,
        "free_plan": gpu.free_plan.as_str(),
        "source": gpu.source.as_str(),
        "checked_at": gpu.checked_at,
        "source_url": gpu.source_url,
        "notes": gpu.notes,
        "compute_capability": gpu.compute_capability.as_ref().map(ComputeCapability::as_string),
        "compute_capability_source": gpu.compute_capability_source.map(|source| source.as_str()),
        "max_gpus_per_container": gpu.max_gpus_per_container,
        "min_cuda": gpu.min_cuda,
        "pins_hardware": gpu.pins_hardware,
        "pinned_spelling": gpu.pinned_spelling,
    })
}

// ---------------------------------------------------------------------------
// GET /api/lanes
// ---------------------------------------------------------------------------

async fn get_lanes() -> Json<Value> {
    Json(lanes_body())
}

/// What each known GPU host may be used for, with the refusals already worked out.
///
/// The catalogue route above answers "what hardware exists"; this one answers "what may I do with
/// it", which is a different question and has a different source: the platform's terms and its
/// documentation about inbound networking. Two of the four hosts researched for issues #11–#13
/// cannot be driven the way their issues asked, and the reason is encoded in `mr_core::lane` rather
/// than in a paragraph: [`plan_lane`] refuses the combination and hands back the clause.
///
/// The plans below are computed for **one account and the operator's own traffic**, which is the
/// only shape a default configuration has. A client that wants to know about a second account or
/// about serving other people can read the raw `accounts`/`use_scope` fields in the same rows; the
/// panel shows the one-account answer because that is the answer for everyone who has not already
/// decided to break a term of service.
///
/// Nothing here is a live quota. `quota` rows are the platform's stated rules, and a lane that has
/// run out of hours fails at submission, which is where the counter actually lives.
fn lanes_body() -> Value {
    let rows: Vec<Value> = LaneKind::ALL
        .iter()
        .map(|kind| {
            let facts = kind.facts();
            let mut row = serde_json::to_value(facts).unwrap_or_else(|_| json!({}));
            let serving = plan_lane(*kind, Traffic::Synchronous, 1, false);
            let batch = plan_lane(*kind, Traffic::Batch, 1, false);
            row["id"] = json!(facts.id);
            row["can_serve_requests"] = json!(serving.usable);
            row["can_run_batch"] = json!(batch.usable);
            row["refusals"] = json!({
                "synchronous": serving.refusals,
                "batch": batch.refusals,
            });
            // Warnings are what the platform's terms say; refusals are what the platform cannot do.
            // They are separate keys because only one of them can stop a lane running.
            row["warnings"] = json!({
                "synchronous": serving.warnings,
                "batch": batch.warnings,
            });
            row
        })
        .collect();
    json!({
        "lanes": rows,
        "note": "Policy facts read from each platform's own documents, not live quotas. A \"refusal\" is something the platform physically cannot do; a \"warning\" quotes a term the operator should read. Warnings never make a lane unusable, because the deployment shape — several people, one account each — is the operator's decision.",
    })
}

// ---------------------------------------------------------------------------
// POST /api/gpu/probe
// ---------------------------------------------------------------------------

/// Everything the probe needs, assembled from the request body.
///
/// `Debug` is written by hand and prints whether a token was supplied, never the token. A derived
/// `Debug` on this struct would be a secret leak waiting for the first `tracing::debug!`.
pub(crate) struct ProbeInput {
    repo: String,
    revision: Option<String>,
    gpu: Option<String>,
    weight_quant: Option<String>,
    kv_cache_dtype: String,
    max_model_len: Option<u64>,
    token: Option<String>,
}

impl std::fmt::Debug for ProbeInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeInput")
            .field("repo", &self.repo)
            .field("revision", &self.revision)
            .field("gpu", &self.gpu)
            .field("weight_quant", &self.weight_quant)
            .field("kv_cache_dtype", &self.kv_cache_dtype)
            .field("max_model_len", &self.max_model_len)
            .field("token", &self.token.as_ref().map(|_| "<supplied>"))
            .finish()
    }
}

async fn post_probe(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let map = object_body(body, "probe")?;
    let repo = match string_field(&map, "repo") {
        Some(repo) => repo,
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "the probe body needs a non-empty `repo`, for example `Qwen/Qwen3.8-27B`",
            ))
        }
    };
    if let Some(problem) = repo_problem(&repo) {
        return Err(ApiError::bad("invalid_body", problem));
    }
    let input = ProbeInput {
        revision: string_field(&map, "revision"),
        gpu: string_field(&map, "gpu"),
        weight_quant: string_field(&map, "weight_quant"),
        kv_cache_dtype: string_field(&map, "kv_cache_dtype").unwrap_or_else(|| "auto".to_owned()),
        max_model_len: map.get("max_model_len").and_then(Value::as_u64),
        token: probe_token(&state, &map).await?,
        repo,
    };
    let http = crate::quota_api::shared_http().map_err(|e| ApiError::internal(e.to_string()))?;
    let mut body = probe_http(http.as_ref(), &input).await;
    scrub_secret(&mut body, input.token.as_deref());
    Ok(Json(body))
}

/// The Hugging Face token for this probe.
///
/// Three spellings, in this order: a literal `hf_token` in the body (a one-off paste, never
/// stored), an `hf_token_id` naming a recorded [`HuggingFaceToken`], or a legacy `account_id`
/// whose account still carries `hf_secret_ref`. The middle one is the normal case and is what makes
/// the token independent of the account: the same token id serves any account.
///
/// The token is never echoed in an error: every message below says *that* a token is missing, not
/// which value was looked for.
async fn probe_token(state: &AppState, map: &Map<String, Value>) -> ApiResult<Option<String>> {
    if let Some(token) = string_field(map, "hf_token") {
        return Ok(Some(token));
    }
    if let Some(token_id) = string_field(map, "hf_token_id") {
        let config = state.current_config().await;
        let Some(token) = config
            .huggingface_tokens
            .iter()
            .find(|token| token.id == token_id)
            .cloned()
        else {
            return Err(ApiError::bad(
                "not_found",
                format!("there is no Hugging Face token with id `{token_id}`"),
            ));
        };
        let vault_id = token
            .secret_ref
            .clone()
            .unwrap_or_else(|| hf_token_vault_id(&token.id));
        let vault = state.vault.lock().await;
        return match vault.get(&vault_id) {
            Ok(value) => Ok(value),
            Err(e) => Err(ApiError::internal(format!(
                "the Hugging Face token `{token_id}` could not be read from the vault: {e}"
            ))),
        };
    }
    let Some(account_id) = string_field(map, "account_id") else {
        return Ok(None);
    };
    let uses_token = map
        .get("use_hf_token")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !uses_token {
        return Ok(None);
    }
    let vault = state.vault.lock().await;
    match vault.get(&rental_hf_token_id(&account_id)) {
        Ok(token) => Ok(token),
        Err(e) => Err(ApiError::internal(format!(
            "the stored Hugging Face token for rental account `{account_id}` could not be read: {e}"
        ))),
    }
}

/// The probe proper, with the transport passed in so tests can answer without a network.
pub(crate) async fn probe_http(http: &dyn QuotaHttp, input: &ProbeInput) -> Value {
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    let from_name = params_from_name(&input.repo);

    // --- 1. repository metadata, anonymously ---
    let (metadata, metadata_status) = fetch_json(
        http,
        &metadata_url(&input.repo, input.revision.as_deref()),
        None,
    )
    .await;
    let gated_state = metadata.as_ref().and_then(gated_state_from_hf_api);
    let params_total = metadata.as_ref().and_then(params_from_hf_api);
    let sha = metadata
        .as_ref()
        .and_then(|value| value.get("sha"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    // --- 2. the repository's own architecture ---
    let config_url = raw_config_url(&input.repo, input.revision.as_deref());
    let (own_facts, own_failure) =
        match fetch_config(http, &config_url, input.token.as_deref()).await {
            Ok(facts) => (Some(facts), None),
            Err(failure) => (None, Some(failure)),
        };

    let mut arch_source = "repo";
    let mut arch_repo = input.repo.clone();
    let mut arch_relation: Option<String> = None;
    let mut arch_shares = true;
    let mut facts = own_facts;

    // --- 3. the base model, when the repository's own config could not be read ---
    if facts.is_none() {
        let hint = metadata.as_ref().and_then(base_model_hint_from_hf_api);
        match hint {
            Some(hint) => {
                let base_url = raw_config_url(&hint.repo_id, None);
                // Anonymous on purpose: the base model is expected to be public, and a token that
                // unlocked one repository has no business travelling to another.
                match fetch_config(http, &base_url, None).await {
                    Ok(base_facts) => {
                        arch_source = "base_model";
                        arch_repo = hint.repo_id.clone();
                        arch_shares = hint.shares_architecture();
                        arch_relation = hint.relation.clone();
                        facts = Some(base_facts);
                        if !arch_shares {
                            warnings.push(format!(
                                "the architecture was read from the base repository `{}`, whose \
                                 relation is `{}` rather than a quantised variant; a fine-tune can \
                                 change the layer count, so check it against the checkpoint's own \
                                 `config.json` before renting",
                                hint.repo_id,
                                hint.relation.as_deref().unwrap_or("unknown"),
                            ));
                        }
                    }
                    Err(second) => {
                        arch_source = "unavailable";
                        errors.push(format!(
                            "the base repository `{}` named on the model card could not be read \
                             either: {}",
                            hint.repo_id, second
                        ));
                        if let Some(first) = own_failure.as_ref().map(|failure| failure.to_string())
                        {
                            errors
                                .insert(0, format!("`{}` could not be read: {first}", input.repo));
                        }
                    }
                }
            }
            None => {
                arch_source = "unavailable";
                if let Some(first) = own_failure.as_ref().map(|failure| failure.to_string()) {
                    errors.push(format!("`{}` could not be read: {first}", input.repo));
                }
                errors.push(
                    "the model card names no `base_model`, so there is no second repository to \
                     read the architecture from"
                        .to_owned(),
                );
            }
        }
    }

    // --- 4. gating ---
    let needs_token = match gated_state {
        Some(GatedState::Auto) | Some(GatedState::Manual) => true,
        // The metadata call failed, but a 401 on the config call is its own evidence.
        _ => matches!(
            own_failure.as_ref(),
            Some(ConfigFailure::Gated { .. }) | Some(ConfigFailure::Unauthorized { .. })
        ),
    };
    let gated = json!({
        "state": gated_state.map(gated_state_str),
        "needs_token": needs_token,
        "explain": gating_explanation(gated_state, needs_token, input.token.is_some()),
        "metadata_status": metadata_status,
    });
    if needs_token && input.token.is_none() {
        warnings.push(
            "this repository is gated and no Hugging Face token was supplied; the architecture \
             could only come from the public base model, if the card names one"
                .to_owned(),
        );
    }

    // --- 5. the architecture object ---
    let kv_bytes_per_element = kv_element_bytes(&input.kv_cache_dtype, facts.as_ref());
    let mut arch = match facts.as_ref() {
        Some(facts) => serde_json::to_value(facts).unwrap_or_else(|_| json!({})),
        None => json!({}),
    };
    if let Value::Object(object) = &mut arch {
        object.insert("source".to_owned(), json!(arch_source));
        object.insert("repo".to_owned(), json!(arch_repo));
        object.insert("relation".to_owned(), json!(arch_relation));
        object.insert("shares_architecture".to_owned(), json!(arch_shares));
        object.insert(
            "kv_bytes_per_token_elem".to_owned(),
            json!(kv_bytes_per_element),
        );
        if let (Some(facts), Some(bytes)) = (facts.as_ref(), kv_bytes_per_element) {
            let geometry = facts.kv_geometry(bytes, &input.kv_cache_dtype);
            object.insert(
                "kv_geometry".to_owned(),
                serde_json::to_value(&geometry).unwrap_or(Value::Null),
            );
        }
    }

    // --- 6. parameters ---
    let params = json!({
        "total": params_total.or(from_name.as_ref().map(|hint| hint.value)),
        "source": if params_total.is_some() {
            "hf"
        } else if from_name.is_some() {
            "name"
        } else {
            "unknown"
        },
        "from_name": from_name.as_ref().map(|hint| json!({
            "value": hint.value,
            "token": hint.token,
        })),
        "agrees_with_name": match (params_total, from_name.as_ref()) {
            // The Hub counts an MoE checkpoint's active parameters differently from a name that
            // only advertises the total, so agreement is a band rather than an equality.
            (Some(total), Some(hint)) => Some(
                (total as f64 - hint.value as f64).abs() / hint.value as f64 <= 0.05,
            ),
            _ => None,
        },
    });

    // --- 7. quantisations ---
    let gpu = input.gpu.as_deref().and_then(|id| {
        let found = gpu_by_id(id);
        if found.is_none() {
            errors.push(format!("`{id}` is not a GPU id in the catalog"));
        }
        found
    });
    let declared_quant = facts
        .as_ref()
        .and_then(|facts| facts.quant_method.as_deref())
        .map(str::to_owned);
    let quants = quant_rows(
        declared_quant.as_deref(),
        gpu,
        input.weight_quant.as_deref(),
    );

    // --- 8. sizing ---
    let (sizing, confidence, sizing_weight_bytes) = sizing_for(
        input,
        facts.as_ref(),
        params_total,
        from_name.as_ref().map(|h| h.value),
        gpu,
        &mut warnings,
        &mut errors,
    );

    if arch_source == "unavailable" && facts.is_none() {
        warnings.push(
            "no architecture and no parameter count means no sizing: the probe will not guess a \
             KV budget from the repository name alone"
                .to_owned(),
        );
    }

    json!({
        "repo": input.repo,
        "revision": input.revision.clone().unwrap_or_else(|| "main".to_owned()),
        "sha": sha,
        "gated": gated,
        "arch": arch,
        "params": params,
        "quants": quants,
        "sizing": sizing,
        "confidence": confidence,
        "weight_bytes": sizing_weight_bytes,
        "warnings": warnings,
        "errors": errors,
    })
}

/// Why a `config.json` fetch failed. Only the variants the probe reacts to are distinguished; the
/// rest carry the message that will be shown to the operator.
enum ConfigFailure {
    /// `401`/`403` with `X-Error-Code: GatedRepo`.
    Gated { status: u16 },
    /// `401`/`403` without that header.
    Unauthorized { status: u16 },
    /// Anything else, already phrased for a human.
    Other(String),
}

impl std::fmt::Display for ConfigFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Gated { status } => write!(
                f,
                "the Hub answered {status} with X-Error-Code: GatedRepo, which means the repository \
                 is gated and needs an accepted licence plus a token"
            ),
            Self::Unauthorized { status } => write!(
                f,
                "the Hub answered {status}, which means the repository is private or gated and the \
                 request carried no usable token"
            ),
            Self::Other(message) => f.write_str(message),
        }
    }
}

fn metadata_url(repo: &str, revision: Option<&str>) -> String {
    match revision {
        Some(revision) => format!("{HF_API_ROOT}/{repo}/revision/{revision}"),
        None => format!("{HF_API_ROOT}/{repo}"),
    }
}

/// The URL of a repository's own `config.json`, served as a file rather than as a redirect.
fn raw_config_url(repo: &str, revision: Option<&str>) -> String {
    let revision = revision.unwrap_or("main");
    format!("{HF_FILE_ROOT}/{repo}/raw/{revision}/config.json")
}

/// The JSON metadata call. Never carries a token: see the module docs.
async fn fetch_json(
    http: &dyn QuotaHttp,
    url: &str,
    token: Option<&str>,
) -> (Option<Value>, Option<u16>) {
    let mut request = QuotaRequest::get(url);
    if let Some(token) = token {
        request = request.bearer(token);
    }
    match http.request(request).await {
        Ok(response) if response.is_success() => (response.json().ok(), Some(response.status)),
        Ok(response) => (None, Some(response.status)),
        Err(_) => (None, None),
    }
}

async fn fetch_config(
    http: &dyn QuotaHttp,
    url: &str,
    token: Option<&str>,
) -> Result<ArchFacts, ConfigFailure> {
    let mut request = QuotaRequest::get(url);
    if let Some(token) = token {
        request = request.bearer(token);
    }
    let response = match http.request(request).await {
        Ok(response) => response,
        // A transport error is phrased without the URL: the URL is safe today, but an error
        // message is the wrong place to start depending on that.
        Err(e) => return Err(ConfigFailure::Other(format!("the request failed: {e}"))),
    };
    if !response.is_success() {
        let gated_header = response
            .header("x-error-code")
            .map(|value| value.eq_ignore_ascii_case("GatedRepo"))
            .unwrap_or(false);
        return Err(match (response.status, gated_header) {
            (status, true) => ConfigFailure::Gated { status },
            (401 | 403, false) => ConfigFailure::Unauthorized {
                status: response.status,
            },
            (status, _) => {
                ConfigFailure::Other(format!("the Hub answered {status} for `config.json`"))
            }
        });
    }
    arch_facts_from_config_bytes(&response.body).map_err(|e| {
        ConfigFailure::Other(format!(
            "`config.json` was served but could not be read: {e}"
        ))
    })
}

fn gated_state_str(state: GatedState) -> &'static str {
    match state {
        GatedState::Open => "open",
        GatedState::Auto => "auto",
        GatedState::Manual => "manual",
    }
}

fn gating_explanation(state: Option<GatedState>, needs_token: bool, had_token: bool) -> String {
    match (state, had_token) {
        (Some(GatedState::Open), _) => {
            "the Hub reports this repository as ungated, so no token was needed".to_owned()
        }
        (Some(GatedState::Auto), true) => {
            "the repository is gated with automatic approval: access is granted on request, and \
             the supplied token was used"
                .to_owned()
        }
        (Some(GatedState::Auto), false) => {
            "the repository is gated with automatic approval: accept the licence on the Hub and \
             supply a read token"
                .to_owned()
        }
        (Some(GatedState::Manual), true) => {
            "the repository is gated with manual approval: a token only works after the repository \
             owner has granted this account access"
                .to_owned()
        }
        (Some(GatedState::Manual), false) => {
            "the repository is gated with manual approval: the owner has to grant access to this \
             account before any token will work"
                .to_owned()
        }
        (None, _) if needs_token => {
            "the architecture could not be fetched without a token and the Hub metadata did not \
             report a gating state"
                .to_owned()
        }
        (None, _) => "the Hub metadata reported no gating state".to_owned(),
    }
}

/// Bytes per KV-cache element: the named dtype when the catalog knows it, else the model's own
/// dtype, else `None`.
fn kv_element_bytes(dtype: &str, facts: Option<&ArchFacts>) -> Option<u32> {
    kv_cache_dtype_by_id(dtype)
        .and_then(|row| row.bytes_per_element)
        .or_else(|| facts.and_then(ArchFacts::torch_dtype_bytes))
}

/// The parameter count advertised in a repository name, e.g. `Qwen3.8-27B` → 27 000 000 000.
///
/// Only a number that is immediately followed by a `b`/`B` and then a non-alphanumeric counts, so
/// the `3.8` in that name is not mistaken for a parameter count, and the *last* such token wins
/// because model names put the size at the end.
fn params_from_name(repo: &str) -> Option<NameParams> {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let bytes = name.as_bytes();
    let mut best: Option<NameParams> = None;
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        let mut seen_dot = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if byte.is_ascii_digit() {
                index += 1;
            } else if byte == b'.' && !seen_dot {
                seen_dot = true;
                index += 1;
            } else {
                break;
            }
        }
        let number_end = index;
        let is_param_token = index < bytes.len()
            && matches!(bytes[index], b'b' | b'B')
            && bytes
                .get(index + 1)
                .map(|next| !next.is_ascii_alphanumeric())
                .unwrap_or(true);
        if is_param_token {
            if let Ok(value) = name[start..number_end].parse::<f64>() {
                if value > 0.0 && value < 10_000.0 {
                    best = Some(NameParams {
                        token: name[start..=index].to_owned(),
                        value: (value * 1e9).round() as u64,
                    });
                }
            }
            index += 1;
        }
    }
    best
}

/// A parameter count read off the repository name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameParams {
    /// The token that was read, e.g. `27B`.
    pub token: String,
    /// What that token means in parameters.
    pub value: u64,
}

/// One row of the probe's `quants` list.
fn quant_rows(declared: Option<&str>, gpu: Option<&GpuSpec>, chosen: Option<&str>) -> Vec<Value> {
    let declared_norm = declared.map(normalise_quant);
    let mut rows = Vec::new();
    for method in quant_methods() {
        let is_declared = declared_norm
            .as_deref()
            .map(|declared| normalise_quant(method.id) == *declared)
            .unwrap_or(false)
            || (method.id == "none" && declared.is_none());
        let verdict = gpu.and_then(|gpu| resolve_quant_for(gpu, method.id));
        let (support, confidence, reason) = match (&verdict, gpu) {
            (Some(verdict), _) => (
                verdict.support.as_str(),
                verdict.confidence.as_str(),
                verdict.reason.clone(),
            ),
            (None, Some(gpu)) => (
                "unsupported",
                "documented",
                format!(
                    "the catalog does not know the quantisation id `{}` for {}",
                    method.id, gpu.id
                ),
            ),
            (None, None) => (
                "unverified",
                "unverified",
                "no GPU was named, so nothing was checked against a card".to_owned(),
            ),
        };
        // Keep what the checkpoint already carries, and what this card can actually run. A row
        // that is neither is noise: the panel's job is to offer a choice that works.
        let runnable = verdict
            .as_ref()
            .map(|verdict| verdict.runnable())
            .unwrap_or(false);
        if !is_declared && !runnable {
            continue;
        }
        let evidence = if is_declared {
            match declared {
                Some(declared) if method.id != "none" => format!(
                    "`config.json` declares `quantization_config.quant_method = \"{declared}\"`"
                ),
                _ => "`config.json` declares no quantisation, so the checkpoint is unquantised"
                    .to_owned(),
            }
        } else {
            format!("catalog only: {}", method.notes)
        };
        rows.push(json!({
            "id": method.id,
            "label": method.label,
            "in_repo": is_declared,
            // The contract's spelling of the same fact: the checkpoint itself carries this
            // quantisation. `in_repo` says the same thing and is what the panel reads.
            "per_repo": is_declared,
            "evidence": evidence,
            "bytes_per_param": method.bytes_per_param,
            "support": support,
            "confidence": confidence,
            "reason": reason,
            "requires_requantize": !is_declared && method.id != "none",
            "chosen": chosen.map(|chosen| normalise_quant(chosen) == normalise_quant(method.id)).unwrap_or(false),
        }));
    }
    rows
}

fn normalise_quant(id: &str) -> String {
    id.trim().to_ascii_lowercase().replace('_', "-")
}

/// Size the deployment, if and only if the three inputs it needs are all known.
fn sizing_for(
    input: &ProbeInput,
    facts: Option<&ArchFacts>,
    params_total: Option<u64>,
    params_from_name: Option<u64>,
    gpu: Option<&GpuSpec>,
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) -> (Value, &'static str, Value) {
    let params = match params_total.or(params_from_name) {
        Some(params) => params,
        None => {
            warnings.push(
                "no parameter count was found, in the Hub metadata or in the repository name, so \
                 the weights could not be priced"
                    .to_owned(),
            );
            return (Value::Null, "unavailable", Value::Null);
        }
    };
    let Some(gpu) = gpu else {
        warnings.push(
            "no GPU was named in the request, so nothing was sized; name one of the catalog's \
             `gpu` ids to get a KV budget"
                .to_owned(),
        );
        return (Value::Null, "unavailable", Value::Null);
    };

    // Which quantisation the weights will be read at: the requantisation the caller asked for,
    // else whatever the checkpoint declares, else the checkpoint's own dtype.
    let declared = facts.and_then(|facts| facts.quant_method.as_deref());
    let method_id = input
        .weight_quant
        .as_deref()
        .filter(|id| *id != "none")
        .or(declared)
        .and_then(|id| quant_method_by_id(id).map(|method| (method.id, method.bytes_per_param)));
    let (bytes_per_param, basis) = match method_id {
        Some((id, Some(bytes))) => (
            Some(bytes),
            format!("{params} parameters × {bytes} bytes per parameter (quantisation `{id}`)"),
        ),
        Some((id, None)) => {
            warnings.push(format!(
                "the catalog knows the quantisation `{id}` but not its bytes per parameter, so \
                 the weights were priced from the model's own dtype instead"
            ));
            (None, String::new())
        }
        None => (None, String::new()),
    };
    let (weights_bytes, basis) = match (
        bytes_per_param,
        facts.and_then(ArchFacts::torch_dtype_bytes),
    ) {
        (Some(bytes), _) => match weights_bytes_from_param_count(params, bytes) {
            Some(total) => (Some(total), format!("{basis}; {total} bytes in total")),
            None => (None, basis),
        },
        (None, Some(dtype_bytes)) => {
            let dtype = facts
                .and_then(|facts| facts.torch_dtype.clone())
                .unwrap_or_else(|| "the model's dtype".to_owned());
            match weights_bytes_from_param_count(params, f64::from(dtype_bytes)) {
                Some(total) => (
                    Some(total),
                    format!("{params} parameters × {dtype_bytes} bytes per parameter (`{dtype}`); {total} bytes in total"),
                ),
                None => (None, basis),
            }
        }
        (None, None) => (None, basis),
    };

    let Some(weights_bytes) = weights_bytes else {
        if let Some(reason) = facts.and_then(|facts| facts.quant_method.clone()) {
            errors.push(format!(
                "the checkpoint declares `{reason}`, whose bytes per parameter is not in the \
                 catalog, and it declares no `torch_dtype` either; supply a measurement instead of \
                 a guess"
            ));
        } else {
            errors.push(
                "neither a known quantisation nor a `torch_dtype` was available, so the weights \
                 could not be priced"
                    .to_owned(),
            );
        }
        return (Value::Null, "unavailable", Value::Null);
    };

    let Some(max_model_len) = input
        .max_model_len
        .or_else(|| facts.and_then(|facts| facts.max_position_embeddings))
    else {
        warnings.push(
            "no `max_model_len` was supplied and the architecture declares no \
             `max_position_embeddings`, so the KV budget has no context length to divide by"
                .to_owned(),
        );
        return (
            Value::Null,
            "unavailable",
            json!({"bytes": weights_bytes, "basis": basis}),
        );
    };

    let mut request = SizingRequest::new(
        gpu.id,
        weights_bytes,
        input.kv_cache_dtype.clone(),
        max_model_len,
    );
    if let Some(facts) = facts {
        request = request.with_architecture(input.repo.clone(), facts.clone());
    }
    if let Some(vram) = gpu.vram_bytes {
        request = request.with_vram_bytes(vram);
    }
    let contexts = SIZING_CONTEXTS
        .iter()
        .copied()
        .filter(|context| *context < max_model_len)
        .chain(std::iter::once(max_model_len))
        .collect::<Vec<_>>();
    request = request.with_contexts(contexts);

    let result = size_deployment(&request);
    // `size_deployment` reports the rejection inside `warnings` as well, so a bare `push` here
    // repeats the same sentence twice under two different openings. Keep the prefixed framing and
    // drop the repeat, then de-duplicate the rest: the response is read by a person.
    if let Some(rejection) = result.rejection.as_deref() {
        let framed = format!("the sizing arithmetic refused: {rejection}");
        if !warnings.contains(&framed) {
            warnings.push(framed);
        }
    }
    for warning in &result.warnings {
        if !warnings.contains(warning) {
            warnings.push(warning.clone());
        }
    }
    let confidence = result.confidence.as_str();
    (
        serde_json::to_value(&result).unwrap_or(Value::Null),
        confidence,
        json!({"bytes": weights_bytes, "basis": basis}),
    )
}

// ---------------------------------------------------------------------------
// GET /api/gpu/rentals
// ---------------------------------------------------------------------------

async fn get_rentals(State(state): State<AppState>) -> Json<Value> {
    Json(rentals_body(&state).await)
}

pub async fn rentals_body(state: &AppState) -> Value {
    let config = state.current_config().await;
    let accounts = rental_accounts(&config);
    let mut deployments = Vec::new();
    let mut deployment_errors = Vec::new();

    let vault = state.vault.lock().await;
    for provider in &config.providers {
        match parse_deployment(provider.id.as_ref(), &provider.extra) {
            Ok(Some(deployment)) => {
                let model = provider
                    .models
                    .iter()
                    .find(|entry| entry.id == deployment.repo)
                    .map(|entry| entry.id.clone());
                let problems: Vec<Value> = deployment
                    .problems()
                    .into_iter()
                    .map(|(field, message)| json!({"field": field, "message": message}))
                    .collect();
                let hardware: Vec<Value> = deployment
                    .hardware_problems()
                    .into_iter()
                    .map(|(gpu, message)| json!({"gpu": gpu, "message": message}))
                    .collect();
                deployments.push(json!({
                    "provider_id": provider.id.as_ref(),
                    "label": provider.label,
                    "enabled": provider.enabled,
                    "base_url": provider.base_url,
                    "model": model,
                    "model_in_provider": model.is_some(),
                    "account": deployment.account,
                    "hf_token": deployment.hf_token,
                    "repo": deployment.repo,
                    "revision": deployment.revision,
                    "gpu": deployment.gpu,
                    "gpu_count": deployment.gpu_count,
                    "total_vram_bytes": deployment.total_vram_bytes(),
                    "price_usd_per_hour": deployment.price_usd_per_hour(),
                    "weight_quant": deployment.weight_quant,
                    "kv_cache_dtype": deployment.kv_cache_dtype,
                    "max_model_len": deployment.max_model_len,
                    "max_num_seqs": deployment.max_num_seqs,
                    "shutdown_cooldown_secs": deployment.shutdown_cooldown_secs,
                    "mode": deployment.mode.as_str(),
                    "batch": deployment.batch.map(|batch| json!({
                        "size": batch.size,
                        "fill": batch.fill.as_str(),
                        "timeout_secs": batch.timeout_secs,
                    })),
                    "bundle_id": deployment.bundle_id,
                    "sizing": deployment
                        .sizing
                        .as_ref()
                        .map(|sizing| serde_json::to_value(&**sizing).unwrap_or(Value::Null)),
                    "fit": fit_of(&deployment),
                    "problems": problems,
                    "hardware_problems": hardware,
                }));
            }
            Ok(None) => {}
            Err(e) => deployment_errors.push(e.to_string()),
        }
    }

    let now = mr_core::ids::Millis::now();
    let usage = state.store.gpu_usage_totals(mr_core::ids::Millis(0), now);
    let usage_json = match usage {
        Ok(totals) => json!({
            "from_millis": 0,
            "to_millis": now.as_millis(),
            "gpu_requests": totals.requests,
            "gpu_seconds": totals.gpu_seconds,
            "cost_usd": totals.cost_usd,
            "scope": "instance",
        }),
        Err(e) => json!({
            "error": format!("the GPU usage ledger could not be read: {e}"),
            "scope": "instance",
        }),
    };

    let account_rows: Vec<Value> = accounts
        .iter()
        .map(|account| {
            let credential_fingerprint = account
                .secret_ref
                .as_deref()
                .and_then(|id| vault.fingerprint_of(id))
                .map(str::to_owned);
            let hf_fingerprint = account
                .hf_secret_ref
                .as_deref()
                .and_then(|id| vault.fingerprint_of(id))
                .map(str::to_owned);
            let api_fingerprint = account
                .api_secret_ref
                .as_deref()
                .and_then(|id| vault.fingerprint_of(id))
                .map(str::to_owned);
            let problems: Vec<Value> = account
                .problems()
                .into_iter()
                .map(|message| json!(message))
                .collect();
            json!({
                "id": account.id,
                "label": account.label,
                "platform": account.platform.as_str(),
                "plan": account.plan.as_str(),
                "effective_plan": account.effective_plan().as_str(),
                "plan_is_known": account.plan_is_known(),
                "has_credential": account.has_credential(),
                "credential_fingerprint": credential_fingerprint,
                "has_api_credential": account.api_secret_ref.is_some(),
                "api_credential_fingerprint": api_fingerprint,
                "has_hf_token": account.hf_secret_ref.is_some(),
                "hf_token_fingerprint": hf_fingerprint,
                "max_concurrent_containers": account.max_concurrent_containers,
                "note": account.note,
                "plan_limits": account.extra.get("plan_limits").cloned().unwrap_or(Value::Null),
                // The ledger has one aggregate for the whole instance and no per-account column,
                // so a per-account share would be a division the data does not support.
                "usage": Value::Null,
                "usage_unavailable": "the GPU usage ledger aggregates every provider's records \
                                      for a window and has no per-account filter, so this \
                                      account's share of the totals cannot be attributed",
                "problems": problems,
            })
        })
        .collect();

    // Tokens are listed beside accounts, never inside them: the whole point of the split is that a
    // token is not owned by whichever account happened to be created first.
    let hf_token_rows: Vec<Value> = config
        .huggingface_tokens
        .iter()
        .map(|token| {
            let secret_ref = token
                .secret_ref
                .clone()
                .unwrap_or_else(|| hf_token_vault_id(&token.id));
            let fingerprint = vault.fingerprint_of(&secret_ref).map(str::to_owned);
            let problems: Vec<Value> = token
                .problems()
                .into_iter()
                .map(|message| json!(message))
                .collect();
            json!({
                "id": token.id,
                "label": token.label,
                "secret_ref": secret_ref,
                "has_secret": token.has_secret(),
                "fingerprint": fingerprint,
                "note": token.note,
                "problems": problems,
            })
        })
        .collect();

    let errors = deployment_errors;

    json!({
        "checked_at": CATALOG_CHECKED_AT,
        "accounts": account_rows,
        "hf_tokens": hf_token_rows,
        "deployments": deployments,
        "usage": usage_json,
        "errors": errors,
        "notes": [
            "credentials are reported as vault fingerprints, never as values",
            "`hf_tokens` is independent of `accounts`: a deployment names a token with its \
             `hf_token` field, and several deployments on different accounts may name the same one",
            "`plan` is what the operator declared; it is never inferred from the card's free-plan \
             state, which is often `unknown`",
        ],
    })
}

/// The one fact about a deployment that the operator most wants at a glance: does it fit.
fn fit_of(deployment: &RentalDeployment) -> Value {
    let Some(sizing) = deployment.sizing.as_deref() else {
        return json!({"known": false, "reason": "no sizing was recorded for this deployment"});
    };
    // A client may forward only the figures it computed — `gpu`, `kv_pool_tokens`, a context — and
    // leave `SizingResult`'s derived fields absent, because they are conveniences the conductor
    // fills in. Reporting `null` for a deployment that had sized perfectly would say "unknown" about
    // something known, so recompute from the pool the client did send. The panel now forwards all
    // of them; this keeps the answer honest for any other caller.
    let pool = sizing.kv_pool_tokens_aligned.or(sizing.kv_pool_tokens);
    let sessions_at_max_len = sizing.simultaneous_sessions_at_max_len.or_else(|| {
        (sizing.max_model_len > 0)
            .then(|| pool.map(|pool| pool / sizing.max_model_len))
            .flatten()
    });
    json!({
        "known": sizing.is_usable(),
        "confidence": sizing.confidence.as_str(),
        "kv_pool_tokens": pool,
        "sessions_at_max_len": sessions_at_max_len,
        "rejection": sizing.rejection,
    })
}

/// The configured rental accounts.
///
/// They live in [`Config::rental_accounts`], a typed field, so a document whose entries do not parse
/// fails the whole load with the file name and line rather than being silently dropped here. That is
/// why there is no per-entry error list: by the time a [`Config`] exists, every account in it is one
/// this build understood.
fn rental_accounts(config: &Config) -> &[RentalAccount] {
    &config.rental_accounts
}

// ---------------------------------------------------------------------------
// POST /api/gpu/launch-plan
// ---------------------------------------------------------------------------

/// What the `schedule` field accepts, repeated wherever the value is rejected.
///
/// A bare `"batched"` is deliberately not accepted. It carries no size, and choosing one on the
/// operator's behalf would make `"batched"` indistinguishable from `"sequential"` — the two
/// answers differ by a factor of N in how much money is running at once, so the ambiguity is
/// handed back rather than resolved by guessing.
const SCHEDULE_HINT: &str = "the launch plan needs \"schedule\": \"sequential\" for one card at a                              time, or \"batched:N\" for a deliberate group of N cards; a bare                              \"batched\" is refused because it does not say how many";

/// `POST /api/gpu/launch-plan` — order the configured deployments into launch waves.
///
/// This is a read: it computes a plan and writes nothing. The relay is not told about it, the
/// config is not changed, and no card is started. Starting one is a separate, explicit action.
async fn post_launch_plan(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let map = object_body(body, "launch plan")?;
    let Some(raw) = string_field(&map, "schedule") else {
        return Err(ApiError::bad("invalid_body", SCHEDULE_HINT));
    };
    let Some(schedule) = RentalSchedule::parse(&raw) else {
        return Err(ApiError::bad("invalid_body", SCHEDULE_HINT));
    };
    Ok(Json(launch_plan_body(&state, schedule).await))
}

/// The plan itself, separated from the extractor so it can be called without a router.
///
/// Waves carry indices into the `deployments` array rather than copies of each deployment: a plan
/// is about the *set* the caller already has, and repeating every field inside each wave would let
/// the two copies drift. The `deployments` array is included so those indices can be read, since
/// a list of bare integers is not an answer.
pub async fn launch_plan_body(state: &AppState, schedule: RentalSchedule) -> Value {
    let config = state.current_config().await;
    let accounts = rental_accounts(&config);

    let mut deployments: Vec<RentalDeployment> = Vec::new();
    let mut listed: Vec<Value> = Vec::new();
    let mut malformed: Vec<Value> = Vec::new();
    for provider in &config.providers {
        match parse_deployment(provider.id.as_ref(), &provider.extra) {
            Ok(Some(deployment)) => {
                listed.push(json!({
                    "index": deployments.len(),
                    "provider": provider.id.as_ref(),
                    "repo": deployment.repo,
                    "gpu": deployment.gpu,
                    "gpu_count": deployment.gpu_count,
                    "account": deployment.account,
                }));
                deployments.push(deployment);
            }
            Ok(None) => {}
            Err(error) => malformed.push(json!({
                "provider": provider.id.as_ref(),
                "message": error.to_string(),
            })),
        }
    }

    let plan = plan_launch(schedule, &deployments, accounts);
    let waves: Vec<Value> = plan
        .waves
        .iter()
        .map(|wave| {
            json!({
                "cards": wave.cards,
                "deployments": wave.deployments,
            })
        })
        .collect();
    let problems: Vec<Value> = plan
        .problems
        .into_iter()
        .map(|(field, message)| json!({"field": field, "message": message}))
        .collect();

    json!({
        "schedule": plan.schedule.as_str(),
        "max_live": plan.schedule.max_live(),
        "waves": waves,
        "problems": problems,
        // A deployment that does not parse cannot be planned, and silently omitting it would make
        // the plan look complete. It is reported next to the plan instead.
        "malformed": malformed,
        "deployments": listed,
        // A multi-line literal keeps the source indentation inside the string; the note is read by
        // the panel verbatim, so it is concatenated instead of wrapped.
        "note": concat!(
            "`waves` holds indices into `deployments`. An account's ",
            "`max_concurrent_containers` is reported as a problem, never enforced here, ",
            "because the platform is the authority on its own limit. Nothing is started by ",
            "this route."
        ),
    })
}

// ---------------------------------------------------------------------------
// POST /api/gpu/apply
// ---------------------------------------------------------------------------

async fn post_apply(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let map = object_body(body, "apply")?;
    match string_field(&map, "action").as_deref() {
        Some("create-account") => create_account(&state, &map).await.map(Json),
        Some("create-hf-token") => create_hf_token(&state, &map).await.map(Json),
        Some("create-deployment") => create_deployment(&state, &map).await.map(Json),
        Some(other) => Err(ApiError::bad(
            "invalid_body",
            format!(
                "`{other}` is not a known action; use `create-account`, `create-hf-token` or \
                 `create-deployment`"
            ),
        )),
        None => Err(ApiError::bad(
            "invalid_body",
            "the apply body needs an `action` of `create-account`, `create-hf-token` or \
             `create-deployment`",
        )),
    }
}

async fn create_account(state: &AppState, map: &Map<String, Value>) -> ApiResult<Value> {
    let account_value = map.get("account").cloned().unwrap_or(Value::Null);
    let account_map = object_body(account_value, "account")?;
    let id = match string_field(&account_map, "id") {
        Some(id) => id,
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "the account needs an `id`: a short slug such as `modal-personal`",
            ))
        }
    };
    if let Some(problem) = slug_problem(&id) {
        return Err(ApiError::bad("invalid_body", problem));
    }
    let label = string_field(&account_map, "label").unwrap_or_else(|| id.clone());
    let platform = match string_field(&account_map, "platform") {
        Some(platform) => RentalPlatform::parse(&platform),
        None => RentalPlatform::Modal,
    };
    let plan = match string_field(&account_map, "plan") {
        Some(plan) => match RentalPlan::parse(&plan) {
            Some(plan) => plan,
            None => {
                return Err(ApiError::bad(
                    "invalid_body",
                    "`plan` must be one of `free`, `paid`, or `unknown`; it is never inferred from \
                     the card, because the card's free-plan state is usually unknown",
                ));
            }
        },
        // Absent means unknown. Guessing free would be the exact mistake the catalog's three-state
        // free plan exists to prevent.
        None => RentalPlan::Unknown,
    };
    let workspace = match string_field(&account_map, "workspace") {
        Some(workspace) => {
            if let Some(problem) = workspace_problem(&workspace) {
                return Err(ApiError::bad("invalid_body", problem));
            }
            Some(workspace)
        }
        // Only Modal needs a workspace: it is the first half of that platform's derived endpoint
        // URL. Requiring it from a Kaggle or custom account would record a field that platform
        // does not have, so an absent workspace stays absent there.
        None if platform == RentalPlatform::Modal => {
            return Err(ApiError::bad(
                "invalid_body",
                "the account needs a `workspace`: the Modal workspace its apps deploy into \
                 (for example `mana`). It is not a secret, and it is the half of the endpoint URL \
                 Manarouter can know without deploying.",
            ));
        }
        None => None,
    };
    let token_id = string_field(&account_map, "token_id");
    let token_secret = string_field(&account_map, "token_secret");
    // A Hugging Face token is not account state. Accepting one here is what used to tie a token to
    // whichever account happened to be created first, so it is refused with the action that does
    // hold one: `create-hf-token`, whose id a deployment then names.
    if account_map.contains_key("hf_token") || account_map.contains_key("hf_secret_ref") {
        return Err(ApiError::bad(
            "invalid_body",
            "a Hugging Face token is not part of an account: create it with the `create-hf-token` \
             action and name it from the deployment's `hf_token` field",
        ));
    }
    let max_concurrent_containers = account_map
        .get("max_concurrent_containers")
        .and_then(Value::as_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32);
    // `plan_limits` may arrive beside the account or inside it; both are stored verbatim under the
    // account's flatten map, which is how an unknown-key field survives `mr-core` untouched.
    let plan_limits = map
        .get("plan_limits")
        .or_else(|| account_map.get("plan_limits"))
        .cloned();
    if let Some(limits) = &plan_limits {
        validate_plan_limits(limits)?;
    }

    // Either the two halves Modal's dashboard shows, or the combined string the operator already
    // holds. Both become the same single vault value; sending both spellings at once is ambiguous.
    let combined = match (string_field(&account_map, "token"), token_id, token_secret) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ApiError::bad(
                "invalid_body",
                "send either `token` or `token_id` with `token_secret`, not both",
            ))
        }
        (Some(token), None, None) => Some(token),
        (None, Some(token_id), Some(token_secret)) => Some(format!("{token_id}.{token_secret}")),
        (None, None, None) => None,
        _ => return Err(ApiError::bad(
            "invalid_body",
            "a Modal account needs both `token_id` and `token_secret`, or neither; the pair is \
                 the two halves of one credential",
        )),
    };
    if let Some(combined) = &combined {
        if let Some(problem) = modal_credential_problem(combined) {
            return Err(ApiError::bad("invalid_credential", problem));
        }
    }
    // The Modal API token pair is a second, optional credential: the proxy pair above authenticates
    // inference, the API pair authenticates the Modal CLI/API. Same two-half rule, separate vault
    // entry.
    let api_token_id = string_field(&account_map, "api_token_id");
    let api_token_secret = string_field(&account_map, "api_token_secret");
    let api_combined = match (
        string_field(&account_map, "api_token"),
        api_token_id,
        api_token_secret,
    ) {
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            return Err(ApiError::bad(
                "invalid_body",
                "send either `api_token` or `api_token_id` with `api_token_secret`, not both",
            ))
        }
        (Some(token), None, None) => Some(token),
        (None, Some(token_id), Some(token_secret)) => Some(format!("{token_id}.{token_secret}")),
        (None, None, None) => None,
        _ => {
            return Err(ApiError::bad(
                "invalid_body",
                "a Modal API credential needs both `api_token_id` and `api_token_secret`, or \
                 neither; the pair is the two halves of one credential",
            ))
        }
    };
    if let Some(combined) = &api_combined {
        if let Some(problem) = modal_api_credential_problem(combined) {
            return Err(ApiError::bad("invalid_credential", problem));
        }
    }

    let guard = state.mutation_lock.lock().await;
    let current = state.current_config().await;
    if rental_accounts(&current)
        .iter()
        .any(|account| account.id == id)
    {
        return Err(ApiError::conflict(format!(
            "a rental account with id `{id}` already exists"
        )));
    }

    let mut tx = crate::state::VaultTx::default();
    let mut account = RentalAccount::new(&id);
    account.label = label;
    account.platform = platform;
    account.plan = plan;
    account.workspace = workspace;
    account.max_concurrent_containers = max_concurrent_containers;
    account.note = string_field(&account_map, "note");
    if let Some(limits) = plan_limits {
        account.extra.insert("plan_limits".to_owned(), limits);
    }

    let credential_id = rental_credential_id(&id);
    if let Some(combined) = &combined {
        // A vault id belongs to exactly one account: `Vault::put` replaces in place, so a second
        // account reusing this id would silently take the credential away from the first, and the
        // first account's providers would start authenticating with someone else's workspace.
        {
            let vault = state.vault.lock().await;
            if vault.contains(&credential_id) {
                return Err(ApiError::conflict(format!(
                    "the vault already holds a credential at `{credential_id}` for a different \
                     account; refusing to overwrite it. Use a different account id, or clear that \
                     credential deliberately"
                )));
            }
        }
        if let Err(e) = state.tx_put(&mut tx, &credential_id, combined).await {
            state.tx_rollback(tx).await;
            return Err(ApiError::internal(format!(
                "the credential could not be written to the vault: {e}"
            )));
        }
        account.secret_ref = Some(credential_id.clone());
    }
    let api_credential_id = rental_api_credential_id(&id);
    if let Some(combined) = &api_combined {
        {
            let vault = state.vault.lock().await;
            if vault.contains(&api_credential_id) {
                return Err(ApiError::conflict(format!(
                    "the vault already holds an API credential at `{api_credential_id}` for a \
                     different account; refusing to overwrite it. Use a different account id, or \
                     clear that credential deliberately"
                )));
            }
        }
        if let Err(e) = state.tx_put(&mut tx, &api_credential_id, combined).await {
            state.tx_rollback(tx).await;
            return Err(ApiError::internal(format!(
                "the API credential could not be written to the vault: {e}"
            )));
        }
        account.api_secret_ref = Some(api_credential_id.clone());
    }
    let next = with_account(&current, &account);
    if let Err(error) = state.commit_locked(next).await {
        state.tx_rollback(tx).await;
        return Err(error.into());
    }
    drop(guard);

    let fingerprint = {
        let vault = state.vault.lock().await;
        vault.fingerprint_of(&credential_id).map(str::to_owned)
    };
    let api_fingerprint = {
        let vault = state.vault.lock().await;
        account
            .api_secret_ref
            .as_deref()
            .and_then(|id| vault.fingerprint_of(id))
            .map(str::to_owned)
    };
    Ok(json!({
        "action": "create-account",
        "account": {
            "id": account.id,
            "label": account.label,
            "platform": account.platform.as_str(),
            "plan": account.plan.as_str(),
            "workspace": account.workspace,
            "has_credential": account.has_credential(),
            "has_api_credential": account.api_secret_ref.is_some(),
        },
        "vault": {
            "credential_id": credential_id,
            "fingerprint": fingerprint,
        },
        "api_vault": account.api_secret_ref.as_ref().map(|id| json!({
            "credential_id": id,
            "fingerprint": api_fingerprint,
        })).unwrap_or(Value::Null),
        "restart_pending": state.restart.is_pending(),
        "applies_on": "immediately",
        "note": "the account is a record of what is rented; it takes effect as a provider when a \
                 deployment is applied to it",
    }))
}

/// Create a Hugging Face token, or replace the value of one that already exists.
///
/// The token is its own entity because it authenticates a repository fetch, not a rental: the same
/// token may be named by deployments on several accounts, and an account may host deployments that
/// need none. The vault id is derived from the token id, exactly as an account's credential id is
/// derived from the account id, and only the id and a fingerprint are ever returned.
async fn create_hf_token(state: &AppState, map: &Map<String, Value>) -> ApiResult<Value> {
    let token_value = map.get("token").cloned().unwrap_or(Value::Null);
    let token_map = object_body(token_value, "token")?;
    let id = match string_field(&token_map, "id") {
        Some(id) => id,
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "the token needs an `id`: a short slug such as `hub-main`",
            ))
        }
    };
    if let Some(problem) = slug_problem(&id) {
        return Err(ApiError::bad("invalid_body", problem));
    }
    let label = string_field(&token_map, "label").unwrap_or_else(|| id.clone());
    let Some(secret) = string_field(&token_map, "secret") else {
        return Err(ApiError::bad(
            "invalid_body",
            "the token needs a `secret`: the value from huggingface.co/settings/tokens. It is \
             written to the vault and never returned",
        ));
    };
    if secret.trim().is_empty() {
        return Err(ApiError::bad("invalid_body", "`secret` must not be empty"));
    }

    let guard = state.mutation_lock.lock().await;
    let current = state.current_config().await;
    let existing = current
        .huggingface_tokens
        .iter()
        .find(|token| token.id == id)
        .cloned();
    // Replacing the value of a recorded token is a rotation, and is allowed deliberately: the id
    // names the vault entry, so `Vault::put` replaces the value in place and every deployment that
    // names this token picks the new value up without being re-applied.
    let mut token = existing
        .clone()
        .unwrap_or_else(|| HuggingFaceToken::new(&id));
    token.label = label;
    token.note = string_field(&token_map, "note").or(token.note);
    let vault_id = token
        .secret_ref
        .clone()
        .unwrap_or_else(|| hf_token_vault_id(&token.id));
    token.secret_ref = Some(vault_id.clone());

    let mut tx = crate::state::VaultTx::default();
    if let Err(e) = state.tx_put(&mut tx, &vault_id, &secret).await {
        state.tx_rollback(tx).await;
        return Err(ApiError::internal(format!(
            "the Hugging Face token could not be written to the vault: {e}"
        )));
    }
    let next = with_hf_token(&current, &token);
    if let Err(error) = state.commit_locked(next).await {
        state.tx_rollback(tx).await;
        return Err(error.into());
    }
    drop(guard);

    let fingerprint = {
        let vault = state.vault.lock().await;
        vault.fingerprint_of(&vault_id).map(str::to_owned)
    };
    Ok(json!({
        "action": "create-hf-token",
        "token": {
            "id": token.id,
            "label": token.label,
            "replaced": existing.is_some(),
        },
        "vault": {
            "secret_ref": vault_id,
            "fingerprint": fingerprint,
        },
        "applies_on": "immediately",
        "note": "a deployment names this token with its `hf_token` field; the token is never \
                 reached through an account",
    }))
}

/// Check the shape of a `plan_limits` array without interpreting it.
///
/// The catalog's per-plan limits are documented facts about a platform; an operator copying their
/// own plan's numbers is adding a *third* source, so the entries are stored as given and only their
/// shape is enforced.
fn validate_plan_limits(value: &Value) -> ApiResult<()> {
    let Some(entries) = value.as_array() else {
        return Err(ApiError::bad(
            "invalid_body",
            "`plan_limits` must be an array of plan limit objects",
        ));
    };
    for (index, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            return Err(ApiError::bad(
                "invalid_body",
                format!("`plan_limits[{index}]` must be an object"),
            ));
        };
        match object.get("id").and_then(Value::as_str) {
            Some(id) if !id.trim().is_empty() => {}
            _ => {
                return Err(ApiError::bad(
                    "invalid_body",
                    format!("`plan_limits[{index}]` needs a non-empty `id`"),
                ))
            }
        }
    }
    Ok(())
}

/// Read the `batch` object of a `create-deployment` body.
///
/// Kept separate from the long field list so the two decisions it encodes — a batch has a size, and
/// only a batch that ships early has a timeout — are testable without an HTTP round trip.
fn parse_batch_shape(value: &Value, mode: RentalMode) -> ApiResult<BatchShape> {
    if mode != RentalMode::Batch {
        return Err(ApiError::bad(
            "invalid_body",
            "`batch` describes how a `batch` deployment fills; a `streaming` deployment has no \
             batch to fill",
        ));
    }
    let Some(object) = value.as_object() else {
        return Err(ApiError::bad(
            "invalid_body",
            "`batch` must be an object, for example {\"size\": 8, \"fill\": \"timeout\", \
             \"timeout_secs\": 30}",
        ));
    };
    let size = match object.get("size").and_then(Value::as_u64) {
        Some(size) if size > 0 => size.min(u64::from(u32::MAX)) as u32,
        Some(_) => {
            return Err(ApiError::bad(
                "invalid_body",
                "`batch.size` must be at least 1",
            ))
        }
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "`batch.size` is the number of requests one batch holds",
            ))
        }
    };
    let fill =
        match object.get("fill") {
            None => BatchFill::default(),
            Some(Value::String(text)) => match BatchFill::parse(text) {
                Some(fill) => fill,
                None => return Err(ApiError::bad(
                    "invalid_body",
                    "`batch.fill` must be `full` (wait until the batch is complete) or `timeout` \
                     (ship a short batch)",
                )),
            },
            Some(_) => {
                return Err(ApiError::bad(
                    "invalid_body",
                    "`batch.fill` must be `full` or `timeout`",
                ))
            }
        };
    let timeout_secs = match object.get("timeout_secs") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(secs) if secs > 0 && secs <= 3600 => Some(secs as u32),
            _ => {
                return Err(ApiError::bad(
                    "invalid_body",
                    "`batch.timeout_secs` must be between 1 and 3600 seconds",
                ))
            }
        },
    };
    let shape = BatchShape {
        size,
        fill,
        timeout_secs,
    };
    if let Some((field, message)) = shape.problems().into_iter().next() {
        return Err(ApiError::bad(
            "invalid_body",
            format!("`{field}`: {message}"),
        ));
    }
    Ok(shape)
}

async fn create_deployment(state: &AppState, map: &Map<String, Value>) -> ApiResult<Value> {
    let account_id = match string_field(map, "account_id") {
        Some(id) => id,
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "the deployment needs an `account_id` naming a configured rental account",
            ))
        }
    };
    let deployment_map = object_body(
        map.get("deployment").cloned().unwrap_or(Value::Null),
        "deployment",
    )?;

    let repo = match string_field(&deployment_map, "repo") {
        Some(repo) => repo,
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "the deployment needs a `repo`, for example `Qwen/Qwen3.8-27B`",
            ))
        }
    };
    if let Some(problem) = repo_problem(&repo) {
        return Err(ApiError::bad("invalid_body", problem));
    }
    let gpu_id = match string_field(&deployment_map, "gpu") {
        Some(id) => id,
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                "the deployment needs a `gpu` id from GET /api/gpu/catalog",
            ))
        }
    };
    let Some(gpu) = gpu_by_id(&gpu_id) else {
        return Err(ApiError::bad(
            "invalid_body",
            format!("`{gpu_id}` is not a GPU id in the catalog; see GET /api/gpu/catalog"),
        ));
    };
    // Optional. Absent means "derive it from the account's workspace and the app name the bundle
    // will use"; present is an explicit override, and the value `modal deploy` printed is the one
    // that should win.
    let base_url_override = string_field(&deployment_map, "base_url");
    if let Some(url) = &base_url_override {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ApiError::bad(
                "invalid_body",
                "`base_url` must start with `http://` or `https://`",
            ));
        }
    }

    let gpu_count = deployment_map
        .get("gpu_count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .max(1);
    if gpu_count > u64::from(gpu.max_gpus_per_container) {
        return Err(ApiError::bad(
            "invalid_body",
            format!(
                "`gpu_count` is {gpu_count}, but this platform allows at most {} per container \
                 for {}",
                gpu.max_gpus_per_container, gpu.id
            ),
        ));
    }
    let weight_quant =
        string_field(&deployment_map, "weight_quant").unwrap_or_else(|| "none".to_owned());
    let kv_cache_dtype =
        string_field(&deployment_map, "kv_cache_dtype").unwrap_or_else(|| "auto".to_owned());
    let mode = match string_field(&deployment_map, "mode") {
        Some(mode) => match RentalMode::parse(&mode) {
            Some(mode) => mode,
            None => {
                return Err(ApiError::bad(
                    "invalid_body",
                    "`mode` must be `streaming` or `batch`",
                ));
            }
        },
        None => RentalMode::default(),
    };
    // A batch deployment states how it fills a batch, because "batching N" is undefined without
    // it. A streaming deployment may not carry one: the value would be a setting nothing reads.
    let batch = match deployment_map.get("batch") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_batch_shape(value, mode)?),
    };
    if mode == RentalMode::Batch && batch.is_none() {
        return Err(ApiError::bad(
            "invalid_body",
            "a `batch` deployment needs a `batch` object with `size` and `fill` (`full` waits for \
             the batch to fill; `timeout` also needs `timeout_secs` and ships a short batch)",
        ));
    }
    let shutdown_cooldown_secs = deployment_map
        .get("shutdown_cooldown_secs")
        .and_then(Value::as_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32);
    // Optional. A requantisation is a recipe the operator runs separately, never something this
    // endpoint executes; accepting the plan here only keeps it with the deployment that will serve
    // the resulting checkpoint, so a supplied plan is never silently dropped.
    let requantize = match deployment_map.get("requantize") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<RequantizePlan>(value.clone()).map_err(|err| {
                ApiError::bad(
                    "invalid_body",
                    format!("`requantize` is not a plan this server understands: {err}"),
                )
            })?,
        ),
    };

    // Hardware legality. An `unsupported` verdict is a refusal with the catalog's own reason; an
    // `unverified` one is allowed and carried out as a warning, because "the catalog has not
    // checked" is not the same as "the catalog says no".
    let mut warnings = Vec::new();
    if weight_quant != "none" {
        match resolve_quant_for(gpu, &weight_quant) {
            Some(verdict) => match verdict.support {
                QuantSupport::Unsupported => {
                    return Err(ApiError::bad(
                        "unsupported_quantization",
                        format!("{} cannot run `{weight_quant}`: {}", gpu.id, verdict.reason),
                    ))
                }
                QuantSupport::Unverified => warnings.push(format!(
                    "`{weight_quant}` on {} is unverified: {}",
                    gpu.id, verdict.reason
                )),
                QuantSupport::Supported => {}
            },
            None => {
                return Err(ApiError::bad(
                    "invalid_body",
                    format!(
                        "`{weight_quant}` is not a quantisation id in the catalog; see GET \
                         /api/gpu/catalog"
                    ),
                ))
            }
        }
    }
    match resolve_kv_dtype_for(gpu, &kv_cache_dtype) {
        Some(verdict) => match verdict.support {
            QuantSupport::Unsupported => {
                return Err(ApiError::bad(
                    "unsupported_kv_cache_dtype",
                    format!(
                        "{} cannot use `{kv_cache_dtype}`: {}",
                        gpu.id, verdict.reason
                    ),
                ))
            }
            QuantSupport::Unverified => warnings.push(format!(
                "`{kv_cache_dtype}` on {} is unverified: {}",
                gpu.id, verdict.reason
            )),
            QuantSupport::Supported => {}
        },
        None => {
            return Err(ApiError::bad(
                "invalid_body",
                format!(
                    "`{kv_cache_dtype}` is not a KV-cache dtype in the catalog; see GET \
                     /api/gpu/catalog"
                ),
            ))
        }
    }
    if let Some(cooldown) = shutdown_cooldown_secs {
        let window = mr_core::gpu::catalog::modal_scaledown_window();
        if cooldown < window.min_seconds || cooldown > window.max_seconds {
            return Err(ApiError::bad(
                "invalid_body",
                format!(
                    "`shutdown_cooldown_secs` is {cooldown}, outside Modal's allowed range of {} \
                     to {} seconds",
                    window.min_seconds, window.max_seconds
                ),
            ));
        }
    }

    let sizing = match deployment_map.get("sizing") {
        Some(Value::Null) | None => None,
        Some(value) => match serde_json::from_value::<SizingResult>(value.clone()) {
            Ok(sizing) => Some(Box::new(sizing)),
            Err(e) => {
                return Err(ApiError::bad(
                    "invalid_body",
                    format!(
                        "`sizing` was supplied but is not a sizing result; pass the object the \
                         probe returned, or omit it: {e}"
                    ),
                ))
            }
        },
    };
    if sizing.is_none() {
        warnings.push(
            "no `sizing` was supplied, so the provider is created without a KV budget and the \
             relay cannot admit by tokens for it"
                .to_owned(),
        );
    }

    let guard = state.mutation_lock.lock().await;
    let current = state.current_config().await;
    let Some(account) = rental_accounts(&current)
        .iter()
        .find(|account| account.id == account_id)
    else {
        return Err(ApiError::not_found(format!(
            "there is no rental account with id `{account_id}`"
        )));
    };
    // `RentalPlatform` has one implementation today, so an account on another platform is refused
    // by name rather than silently given a Modal image.
    if account.platform != RentalPlatform::Modal {
        return Err(ApiError::bad(
            "unsupported_platform",
            format!(
                "account `{account_id}` is on platform `{}`, and this build can only generate a \
                 deployment for `modal`",
                account.platform.as_str()
            ),
        ));
    }

    // Several deployments may share one account, and one model-on-a-GPU may be recorded under
    // several accounts: each pairing gets its own provider id (`provider_id_for` above), so two
    // models under one account stay distinguishable, and so do the same model under two accounts.
    // The account owns the credential and the vault entry; each deployment names the account it
    // bills through, and usage is attributed per provider. Account creation and model setup are
    // therefore separate acts, which is the point of the two entities.

    // One account may host the same checkpoint more than once — a streaming endpoint on one card
    // and a batch endpoint on another, or two batch sizes on two cards — so the identity carries the
    // mode and the card, and each of those is its own provider, app and endpoint. Both the derived
    // URL (below) and the provider id read this shape, so they cannot disagree.
    let shape = mr_core::rental::DeploymentShape {
        account_id: &account.id,
        repo: &repo,
        mode,
        gpu: gpu.id,
        gpu_count: gpu_count as u32,
    };

    let base_url = match base_url_override {
        Some(url) => url,
        None => {
            let Some(workspace) = account
                .workspace
                .as_deref()
                .map(str::trim)
                .filter(|workspace| !workspace.is_empty())
            else {
                return Err(ApiError::bad(
                    "invalid_body",
                    format!(
                        "rental account `{account_id}` has no recorded workspace, so the endpoint \
                         URL cannot be derived; record the workspace on the account, or send the \
                         `base_url` that `modal deploy` prints"
                    ),
                ));
            };
            let Some(url) = mr_core::rental::modal_base_url(workspace, shape) else {
                return Err(ApiError::bad(
                    "invalid_body",
                    "the endpoint URL could not be derived from that workspace and repository \
                     name; send the `base_url` that `modal deploy` prints",
                ));
            };
            url
        }
    };

    let provider_id = provider_id_for(shape);
    if current
        .providers
        .iter()
        .any(|p| p.id.as_ref() == provider_id)
    {
        return Err(ApiError::conflict(format!(
            "a provider with id `{provider_id}` already exists; delete it or apply this deployment \
             to the account under a different repository name"
        )));
    }

    // The account's own credential is written once, when the account is created; this deployment
    // only points at it. The vault id is derived from the account id, so every deployment on this
    // account reads the same entry: a rotation on the account reaches all of them, and no copy can
    // go stale. Another account cannot point here, because its id derives a different vault id.
    let credential_id = account
        .secret_ref
        .clone()
        .unwrap_or_else(|| rental_credential_id(&account.id));
    {
        let vault = state.vault.lock().await;
        if !vault.contains(&credential_id) {
            return Err(ApiError::bad(
                "missing_credential",
                format!(
                    "rental account `{account_id}` has no credential at `{credential_id}` in the \
                     vault; add the Modal token to the account before deploying"
                ),
            ));
        }
    }

    // The Hugging Face token is named by the deployment, not inherited from the account: the same
    // token may serve deployments on several accounts, and this deployment may need none. A named
    // token must exist and hold a vault entry, so a gated repository fails here rather than at the
    // first fetch inside a container.
    let hf_token =
        match string_field(map, "hf_token").or_else(|| string_field(&deployment_map, "hf_token")) {
            Some(token_id) => {
                let Some(token) = current
                    .huggingface_tokens
                    .iter()
                    .find(|token| token.id == token_id)
                else {
                    return Err(ApiError::not_found(format!(
                        "there is no Hugging Face token with id `{token_id}`; create it with the \
                     `create-hf-token` action, or omit `hf_token` for a public repository"
                    )));
                };
                let vault_id = token
                    .secret_ref
                    .clone()
                    .unwrap_or_else(|| hf_token_vault_id(&token.id));
                let vault = state.vault.lock().await;
                if !vault.contains(&vault_id) {
                    return Err(ApiError::bad(
                        "missing_credential",
                        format!(
                            "Hugging Face token `{token_id}` has no value at `{vault_id}` in the \
                         vault; set it with the `create-hf-token` action before deploying"
                        ),
                    ));
                }
                Some(token.id.clone())
            }
            None => None,
        };

    let mut deployment = RentalDeployment::new(account.id.clone(), &repo, gpu.id);
    deployment.hf_token = hf_token;
    deployment.gpu_count = gpu_count as u32;
    deployment.weight_quant = weight_quant;
    deployment.kv_cache_dtype = kv_cache_dtype;
    deployment.max_model_len = deployment_map.get("max_model_len").and_then(Value::as_u64);
    deployment.max_num_seqs = deployment_map
        .get("max_num_seqs")
        .and_then(Value::as_u64)
        .map(|value| value.min(u64::from(u32::MAX)) as u32);
    deployment.shutdown_cooldown_secs = shutdown_cooldown_secs;
    deployment.mode = mode;
    deployment.batch = batch;
    deployment.requantize = requantize;
    deployment.sizing = sizing;
    let review = deployment.problems();
    if !review.is_empty() {
        let details: Vec<Value> = review
            .iter()
            .map(|(field, message)| json!({"field": field, "message": message}))
            .collect();
        return Err(ApiError::bad(
            "invalid_body",
            "the deployment is not consistent with the catalog",
        )
        .with_details(details));
    }

    let bundle = deployment_bundle(&provider_id, &deployment, gpu);
    deployment.bundle_id = Some(bundle.id.clone());

    // A recorded token id is not a token in the container. The bundle mounts the Modal secret by
    // name and never carries the value, so the secret has to exist before the first deploy; say
    // that now rather than letting the first fetch inside the container discover it.
    if deployment.hf_token.is_some() {
        warnings.push(
            "the generated app mounts the Hugging Face secret `manarouter-hf`: create it before \
             the first deploy with `modal secret create manarouter-hf HF_TOKEN=...`, or the \
             container will fail to resolve the secret and a gated repository will not load"
                .to_owned(),
        );
    }

    let mut provider = mr_core::Provider::new(provider_id.clone(), Dialect::ModalVllm, &base_url);
    provider.label = Some(format!("{repo} · {}×{}", deployment.gpu_count, gpu.id));
    provider.auth = AuthKind::Modal;
    provider.note = Some(format!(
        "rental account `{}` on {}",
        account.id,
        account.platform.as_str()
    ));
    if let Some(sizing) = deployment.sizing.as_deref() {
        provider.kv_pool_tokens = sizing.kv_pool_tokens_aligned;
        provider.max_context = sizing.max_model_len.into();
        provider.max_concurrent = sizing
            .simultaneous_sessions_at_max_len
            .and_then(|sessions| u32::try_from(sessions).ok())
            .filter(|sessions| *sessions > 0);
    }
    provider.price_per_second = gpu.price_usd_per_second.into();
    let model = ModelEntry {
        id: repo.clone(),
        source: mr_core::model::ModelSource::Manual,
        label: Some(format!("{repo} ({}×{})", deployment.gpu_count, gpu.id)),
        context_length: deployment
            .max_model_len
            .and_then(|len| u32::try_from(len).ok()),
        ..serde_json::from_value::<ModelEntry>(json!({"id": repo}))
            .map_err(|e| ApiError::internal(format!("the model entry could not be built: {e}")))?
    };
    provider.models.push(model);

    // The provider's sole account references the rental account's vault entry. Several
    // deployments may reference the same entry: the credential belongs to the account, not to one
    // rental.
    let mut account_row = mr_core::Account::new(account.label.clone());
    account_row.secret_ref = Some(credential_id.clone());
    account_row.max_concurrent = 1;
    provider.accounts.push(account_row);

    let extra = deployment
        .to_extra_value()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    provider
        .extra
        .insert(mr_core::rental::RENTAL_EXTRA_KEY.to_owned(), extra);

    let mut next = current.clone();
    next.providers.push(provider);
    let restart_reason = change_needs_restart(&current, &next);

    // No vault write here: the credential already exists and the provider merely references it, so
    // the commit's own rollback covers the only mutable step.
    if let Err(error) = state.commit_locked(next).await {
        return Err(error.into());
    }
    drop(guard);

    if let Some(_reason) = restart_reason {
        state.restart.request();
    }
    let fingerprint = {
        let vault = state.vault.lock().await;
        vault.fingerprint_of(&credential_id).map(str::to_owned)
    };

    Ok(json!({
        "action": "create-deployment",
        "provider_id": provider_id,
        "account_id": account.id,
        // The token id this deployment named, or null for a public repository. The id, never the
        // value: the value lives at `hf:<id>` and is read only when a probe needs it.
        "deployment": {
            "account": deployment.account,
            "repo": deployment.repo,
            "hf_token": deployment.hf_token,
        },
        "vault": {
            // The account's own entry, referenced — not copied. A second deployment on the same
            // account references this same entry.
            "secret_id": credential_id,
            "fingerprint": fingerprint,
        },
        "restart_pending": state.restart.is_pending(),
        "applies_on": if restart_reason.is_some() { "restart" } else { "immediately" },
        "applies_on_note": restart_reason.unwrap_or(
            "the relay re-reads the routing snapshot when its digest changes, so a new provider is \
             routable without restarting either process"
        ),
        "bundle": {
            "id": bundle.id,
            "filename": bundle.filename,
            "text": bundle.text,
        },
        "command": bundle.command,
        "warnings": warnings,
    }))
}

/// A generated deployment bundle: the file, its name, an id derived from its contents, and the
/// command that deploys it.
struct Bundle {
    id: String,
    filename: String,
    text: String,
    command: String,
}

/// The batch assembler this file generates for `mode: batch`.
///
/// vLLM schedules whatever arrives and never holds a request back waiting for a fuller batch, so
/// "a batch of N" is not a vLLM flag: it is this queue. It is stdlib-only on purpose — it has to run
/// in whatever image vLLM runs in, and every extra import is one more way for a paid container to
/// fail at boot. The panel's generator emits the same layer for the same reason, in the same words.
/// How many simultaneous requests a streaming container admits when the deployment records no
/// `max_num_seqs`. Modal admits one input per container by default, so a second concurrent request
/// starts a second GPU; a live run did exactly that and reached ten cards at $3.03/hour each.
const DEFAULT_STREAMING_INPUTS: &str = "32";

const BATCH_ASSEMBLER: &str = r#"
VLLM_PORT = 8001

import json
import queue
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAM = "http://127.0.0.1:%d" % VLLM_PORT
PENDING = queue.Queue()


def _post(body):
    request = urllib.request.Request(
        UPSTREAM + "/v1/chat/completions",
        data=body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib.request.urlopen(request) as answer:
            return answer.status, answer.headers.get("Content-Type"), answer.read()
    except urllib.error.HTTPError as error:
        return error.code, error.headers.get("Content-Type"), error.read()
    except Exception as error:  # a dead upstream must still answer its waiters
        return 502, "application/json", json.dumps({"error": str(error)}).encode()


def _wait_until_serving():
    # A cold container loads weights for minutes; dispatching before vLLM answers only turns
    # that wait into a batch of failed requests.
    while True:
        try:
            with urllib.request.urlopen(UPSTREAM + "/v1/models", timeout=5):
                return
        except Exception:
            time.sleep(5.0)


def _assemble():
    _wait_until_serving()
    while True:
        batch = [PENDING.get()]
        if BATCH_SIZE > 1:
            if BATCH_TIMEOUT is None:
                while len(batch) < BATCH_SIZE:
                    batch.append(PENDING.get())
            else:
                deadline = time.monotonic() + BATCH_TIMEOUT
                while len(batch) < BATCH_SIZE:
                    left = deadline - time.monotonic()
                    if left <= 0:
                        break
                    try:
                        batch.append(PENDING.get(timeout=left))
                    except queue.Empty:
                        break
        answers = [None] * len(batch)

        def run(index, request_body):
            answers[index] = _post(request_body)

        threads = [
            threading.Thread(target=run, args=(index, body))
            for index, (body, _) in enumerate(batch)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        for (_, slot), answer in zip(batch, answers):
            slot["answer"] = answer
            slot["done"].set()


class BatchHandler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def _reply(self, status, content_type, payload):
        self.send_response(status)
        self.send_header("Content-Type", content_type or "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _proxy_get(self, path):
        with urllib.request.urlopen(UPSTREAM + path) as answer:
            self._reply(answer.status, answer.headers.get("Content-Type"), answer.read())

    def do_GET(self):
        if self.path == "/v1/models":
            self._proxy_get(self.path)
        else:
            self._reply(404, "application/json", b'{"error":"not found"}')

    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self._reply(404, "application/json", b'{"error":"not found"}')
            return
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        slot = {"done": threading.Event(), "answer": None}
        PENDING.put((body, slot))
        slot["done"].wait()
        status, content_type, payload = slot["answer"]
        self._reply(status, content_type, payload)
"#;

/// Render `modal_app.py` for a deployment.
///
/// Nothing in `mr_core` generates this file, so it is generated here. Every interpolation is
/// escaped for a Python string literal, and the repository id is validated before it gets this far,
/// so the file cannot be broken out of by a crafted `repo`.
fn deployment_bundle(provider_id: &str, deployment: &RentalDeployment, gpu: &GpuSpec) -> Bundle {
    // The same derivation the recorded `base_url` uses, so the URL the panel shows is the URL this
    // app is deployed under.
    let app_name =
        mr_core::rental::modal_app_name(mr_core::rental::DeploymentShape::of(deployment));
    let gpu_spec = if deployment.gpu_count > 1 {
        format!("{}:{}", gpu.id, deployment.gpu_count)
    } else {
        gpu.id.to_owned()
    };
    let cooldown = deployment.shutdown_cooldown_secs.unwrap_or(60);
    let max_len = deployment
        .max_model_len
        .map(|len| len.to_string())
        .unwrap_or_else(|| "None".to_owned());
    // `MAX_NUM_SEQS` does double duty: it caps the sequences a step schedules when the operator
    // set one, and it is the number of inputs a streaming container admits. vLLM's own default of
    // 1024 sequences is more than a hybrid model can often admit, and leaving this at 1 made every
    // simultaneous request start another GPU container; 32 is a modest ceiling that neither pins
    // vLLM's scheduler (an unset value still emits no flag) nor pays for a card per request.
    let max_seqs = deployment
        .max_num_seqs
        .map(|seqs| seqs.to_string())
        .unwrap_or_else(|| DEFAULT_STREAMING_INPUTS.to_owned());
    let quant_args = if deployment.weight_quant == "none" {
        String::new()
    } else {
        format!(
            "        \"--quantization\",\n        {},\n",
            py_str(&deployment.weight_quant)
        )
    };
    let kv_args = if deployment.kv_cache_dtype == "auto" {
        String::new()
    } else {
        format!(
            "        \"--kv-cache-dtype\",\n        {},\n",
            py_str(&deployment.kv_cache_dtype)
        )
    };
    let max_len_args = match deployment.max_model_len {
        Some(len) => format!("        \"--max-model-len\",\n        \"{len}\",\n"),
        None => String::new(),
    };
    let max_seqs_args = match deployment.max_num_seqs {
        Some(seqs) => format!("        \"--max-num-seqs\",\n        \"{seqs}\",\n"),
        None => String::new(),
    };

    // What the recorded mode means for this file, in the file. A batch deployment gets the queue
    // that decides when a batch ships, and the file says so; a streaming deployment keeps serving
    // vLLM directly.
    let mode_note = match (deployment.mode, deployment.batch) {
        (RentalMode::Batch, Some(batch)) => {
            let when = match batch.fill {
                BatchFill::Full => format!(
                    "waits until {} requests have arrived, however long that takes",
                    batch.size
                ),
                BatchFill::Timeout => format!(
                    "ships whatever has arrived after {}s, so a half-full batch still goes out",
                    batch.timeout_secs.unwrap_or(0)
                ),
            };
            format!(
                "# Mode: batch — one batch holds {size} requests and {when}.\n\
                 # The queue below is that batch: /v1/chat/completions holds each request until the\n\
                 # batch is ready, sends the whole batch to vLLM on 127.0.0.1:8001, and answers every\n\
                 # request with its own response. This is this endpoint's own queue, not the relay's\n\
                 # batch lane: the lane submits items to a provider, this collects callers.\n\
                 # max_inputs=BATCH_SIZE is required, not decorative: Modal admits one input per\n\
                 # container by default, and a batch that can never be full would wait for a request\n\
                 # that is not allowed to arrive.\n",
                size = batch.size,
                when = when
            )
        }
        (RentalMode::Batch, None) => {
            "# Mode: batch, with no recorded batch shape, so no assembler is generated and this file\n\
             # serves vLLM directly. Record a batch size and fill policy to get the queue.\n"
                .to_owned()
        }
        (RentalMode::Streaming, _) => format!(
            "# Mode: streaming — several sessions may share this card; the container admits up to\n\
             # MAX_NUM_SEQS ({max_seqs}) simultaneous requests and vLLM schedules the rest as KV\n\
             # cache frees.\n"
        ),
    };

    // The assembler, when this deployment has a batch to assemble, and the two lines that differ
    // between a batch app and a streaming one.
    let (batch_block, max_inputs, vllm_port_arg, serve_tail) =
        match (deployment.mode, deployment.batch) {
            (RentalMode::Batch, Some(batch)) => {
                let timeout = match batch.fill {
                    BatchFill::Full => "None".to_owned(),
                    BatchFill::Timeout => format!("{}.0", batch.timeout_secs.unwrap_or(0)),
                };
                let head = format!("BATCH_SIZE = {}\nBATCH_TIMEOUT = {}\n", batch.size, timeout);
                (
                    format!("\n{head}{BATCH_ASSEMBLER}"),
                    "BATCH_SIZE".to_owned(),
                    "str(VLLM_PORT)".to_owned(),
                    "    threading.Thread(target=_assemble, daemon=True).start()\n\
                 \x20   server = ThreadingHTTPServer((\"0.0.0.0\", 8000), BatchHandler)\n\
                 \x20   server.daemon_threads = True\n\
                 \x20   server.serve_forever()"
                        .to_owned(),
                )
            }
            _ => (
                String::new(),
                "MAX_NUM_SEQS".to_owned(),
                "\"8000\"".to_owned(),
                "    subprocess.Popen(args)".to_owned(),
            ),
        };

    // A gated repository needs a token inside the container, and the only place a token may live is
    // a Modal secret the operator created. The file names it and mounts it; it never contains it.
    let secret_decl = if deployment.hf_token.is_some() {
        "\n# The token itself lives in this Modal secret, never in this file.\n\
         HF_SECRET = modal.Secret.from_name(\"manarouter-hf\")\n"
            .to_owned()
    } else {
        String::new()
    };
    let secret_arg = if deployment.hf_token.is_some() {
        "\n    secrets=[HF_SECRET],".to_owned()
    } else {
        String::new()
    };

    let text = format!(
        r#"# Generated by Manarouter (POST /api/gpu/apply) for provider `{provider_id}`.
#
# Deploy it with:
#     modal deploy {filename}
#
# The panel records the endpoint URL this app should answer on, derived from the workspace and the
# app name:
#     https://<workspace>--<app name>-serve.modal.run
# `modal deploy` prints the authoritative URL. If they differ, use the printed one: the panel's
# base URL is an editable override for exactly that case.
{mode_note}import subprocess

import modal
{secret_decl}
APP_NAME = {app_name}
MODEL = {model}
GPU = {gpu}
SCALEDOWN_WINDOW = {cooldown}
MAX_MODEL_LEN = {max_len}
MAX_NUM_SEQS = {max_seqs}

app = modal.App(APP_NAME)

# The vLLM server image. `add_python=None` keeps the image's own entrypoint off the path so the
# process this function starts is the only server on the port.
image = modal.Image.from_registry("vllm/vllm-openai:latest", add_python=None)
{batch_block}

@app.function(
    image=image,
    gpu=GPU,
    scaledown_window=SCALEDOWN_WINDOW,
    timeout=60 * 60,{secret_arg}
)
@modal.concurrent(max_inputs={max_inputs})
@modal.web_server(port=8000, startup_timeout=60 * 20)
def serve() -> None:
    args = [
        "vllm",
        "serve",
        MODEL,
        "--host",
        "0.0.0.0",
        "--port",
        {vllm_port_arg},
        "--served-model-name",
        MODEL,
{quant_args}{kv_args}{max_len_args}{max_seqs_args}    ]
{serve_tail}
"#,
        filename = "modal_app.py",
        mode_note = mode_note,
        provider_id = provider_id,
        app_name = py_str(&app_name),
        model = py_str(&deployment.repo),
        gpu = py_str(&gpu_spec),
        cooldown = cooldown,
        max_len = max_len,
        max_seqs = max_seqs,
        quant_args = quant_args,
        kv_args = kv_args,
        max_len_args = max_len_args,
        max_seqs_args = max_seqs_args,
        batch_block = batch_block,
        max_inputs = max_inputs,
        vllm_port_arg = vllm_port_arg,
        serve_tail = serve_tail,
        secret_decl = secret_decl,
        secret_arg = secret_arg,
    );

    // `PlatformSpec::deployment.has_detach_flag` is false for Modal, so the command is plain
    // `modal deploy`: there is no detach flag to add.
    let command = format!("modal deploy {}", "modal_app.py");
    Bundle {
        id: format!("bundle-{}", &sha256_hex(&text)[..16]),
        filename: "modal_app.py".to_owned(),
        text,
        command,
    }
}

/// A Python string literal for `value`, safe against quotes, backslashes, and newlines.
fn py_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Whether a change only takes effect when a process rebinds.
///
/// A listener's address is read once, at bind time, so changing it is the one edit in this module
/// that a running process cannot adopt. Everything else — providers, accounts, models — lives in
/// the routing snapshot, which the relay re-reads whenever its digest changes.
fn change_needs_restart(before: &Config, after: &Config) -> Option<&'static str> {
    if before.relay.host != after.relay.host || before.relay.port != after.relay.port {
        return Some(
            "the relay listener address changed; it takes effect when the relay process restarts",
        );
    }
    if before.control.host != after.control.host || before.control.port != after.control.port {
        return Some(
            "the control listener address changed; it takes effect when the conductor process \
             restarts",
        );
    }
    None
}

/// A provider id for a deployment: stable, lowercase, and derived from the account and repository
/// so that applying the same deployment twice collides instead of duplicating.
fn provider_id_for(shape: mr_core::rental::DeploymentShape<'_>) -> String {
    // The same helper the panel and the derived URL use, so the provider id and the Modal app name
    // cannot drift apart.
    mr_core::rental::app_slug(shape)
}

/// Why a value is not usable as a Modal workspace.
///
/// A workspace is a DNS label in `https://<workspace>--<app>-serve.modal.run`, so it is validated
/// as one rather than guessed at; normalising it silently would record a workspace the operator
/// did not type, and the derived URL with it.
fn workspace_problem(workspace: &str) -> Option<String> {
    if workspace.is_empty() {
        return Some("`workspace` must not be empty".to_owned());
    }
    if workspace.len() > 63 {
        return Some("`workspace` must be at most 63 characters".to_owned());
    }
    if workspace.starts_with('-') || workspace.ends_with('-') {
        return Some("`workspace` must not start or end with `-`".to_owned());
    }
    if !workspace
        .chars()
        .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Some(format!(
            "`workspace` `{workspace}` must be a lowercase workspace slug: letters `a-z`, digits \
             and `-`, as in `mana`"
        ));
    }
    None
}

/// Why a value is not usable as an account id.
fn slug_problem(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("the account `id` must not be empty".to_owned());
    }
    if id.len() > 64 {
        return Some("the account `id` must be at most 64 characters".to_owned());
    }
    let mut chars = id.chars();
    let first = chars.next().unwrap_or('-');
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Some(format!(
            "the account `id` `{id}` must start with a lowercase letter or a digit"
        ));
    }
    if let Some(bad) = id.chars().find(|ch| {
        !ch.is_ascii_lowercase() && !ch.is_ascii_digit() && !matches!(ch, '.' | '_' | '-')
    }) {
        return Some(format!(
            "the account `id` `{id}` contains `{bad}`, which is not allowed; use lowercase \
             letters, digits, and `.`, `_`, `-`"
        ));
    }
    None
}

/// Why a value is not usable as a Hugging Face repository id.
fn repo_problem(repo: &str) -> Option<String> {
    if repo.is_empty() {
        return Some("the repository id must not be empty".to_owned());
    }
    if repo.len() > 200 {
        return Some("the repository id must be at most 200 characters".to_owned());
    }
    if repo.matches('/').count() > 1 {
        return Some(format!(
            "`{repo}` is not a repository id; the Hub's spelling is `owner/name`"
        ));
    }
    if let Some(bad) = repo
        .chars()
        .find(|ch| !ch.is_ascii_alphanumeric() && !matches!(ch, '.' | '_' | '-' | '/'))
    {
        return Some(format!(
            "`{repo}` contains `{bad}`, which is not allowed in a repository id"
        ));
    }
    if repo.starts_with('/') || repo.ends_with('/') || repo.contains("..") {
        return Some(format!(
            "`{repo}` is not a repository id; the Hub's spelling is `owner/name`"
        ));
    }
    None
}

/// Replace every occurrence of `secret` in a JSON value with `[redacted]`, recursively.
///
/// This is the last line of defence, not the first: no code path above puts a token into a
/// response. It exists so that a future error message which interpolates one cannot leak it.
pub fn scrub_secret(value: &mut Value, secret: Option<&str>) {
    let Some(secret) = secret.filter(|secret| !secret.is_empty()) else {
        return;
    };
    match value {
        Value::String(text) => {
            if text.contains(secret) {
                *text = text.replace(secret, "[redacted]");
            }
        }
        Value::Array(items) => {
            for item in items {
                scrub_secret(item, Some(secret));
            }
        }
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                let mut entry = map.remove(&key).unwrap_or(Value::Null);
                scrub_secret(&mut entry, Some(secret));
                let scrubbed_key = if key.contains(secret) {
                    key.replace(secret, "[redacted]")
                } else {
                    key
                };
                map.insert(scrubbed_key, entry);
            }
        }
        _ => {}
    }
}

/// A JSON object body, or `400` in the standard envelope.
fn object_body(body: Value, what: &str) -> ApiResult<Map<String, Value>> {
    match body {
        Value::Object(map) => Ok(map),
        Value::Null if what == "probe" => Err(ApiError::bad(
            "invalid_body",
            "the probe body must be a JSON object",
        )),
        _ => Err(ApiError::bad(
            "invalid_body",
            format!("the {what} body must be a JSON object"),
        )),
    }
}

/// A non-empty trimmed string field.
fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// `config` with `account` in [`Config::rental_accounts`], replacing any entry with the same id.
fn with_hf_token(config: &Config, token: &HuggingFaceToken) -> Config {
    let mut next = config.clone();
    next.huggingface_tokens
        .retain(|existing| existing.id != token.id);
    next.huggingface_tokens.push(token.clone());
    next
}

fn with_account(config: &Config, account: &RentalAccount) -> Config {
    let mut next = config.clone();
    next.rental_accounts
        .retain(|existing| existing.id != account.id);
    next.rental_accounts.push(account.clone());
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PanelAuth;
    use crate::state::{ServeDirs, StateFiles};
    use mr_core::error::Result as CoreResult;
    use mr_core::quota::http::QuotaResponse;
    use std::path::Path;
    use std::sync::Mutex;

    // --- a fake transport, so the probe can be tested without a network ---

    #[derive(Debug)]
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Reply {
        fn json(body: Value) -> Self {
            Self {
                status: 200,
                headers: vec![("content-type".to_owned(), "application/json".to_owned())],
                body: body.to_string(),
            }
        }

        fn status(status: u16, headers: &[(&str, &str)], body: &str) -> Self {
            Self {
                status,
                headers: headers
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
                body: body.to_owned(),
            }
        }
    }

    /// A transport that answers by URL substring and records what it was asked.
    #[derive(Debug, Default)]
    struct FakeHttp {
        replies: Mutex<Vec<(String, Reply)>>,
        seen: Mutex<Vec<QuotaRequest>>,
    }

    impl FakeHttp {
        fn on(&self, url_fragment: &str, reply: Reply) {
            self.replies
                .lock()
                .expect("replies lock")
                .push((url_fragment.to_owned(), reply));
        }

        fn urls(&self) -> Vec<String> {
            self.seen
                .lock()
                .expect("seen lock")
                .iter()
                .map(|request| request.url.clone())
                .collect()
        }

        fn headers_to(&self, url_fragment: &str) -> Vec<(String, String)> {
            self.seen
                .lock()
                .expect("seen lock")
                .iter()
                .filter(|request| request.url.contains(url_fragment))
                .flat_map(|request| request.headers.clone())
                .collect()
        }
    }

    impl QuotaHttp for FakeHttp {
        fn request<'life0, 'async_trait>(
            &'life0 self,
            request: QuotaRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = CoreResult<QuotaResponse>> + Send + 'async_trait>,
        >
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            let replies = self
                .replies
                .lock()
                .expect("replies lock")
                .iter()
                .map(|(fragment, reply)| {
                    (
                        fragment.clone(),
                        Reply {
                            status: reply.status,
                            headers: reply.headers.clone(),
                            body: reply.body.clone(),
                        },
                    )
                })
                .collect::<Vec<_>>();
            let url = request.url.clone();
            self.seen.lock().expect("seen lock").push(request);
            Box::pin(async move {
                for (fragment, reply) in replies {
                    if url.contains(&fragment) {
                        return Ok(QuotaResponse {
                            status: reply.status,
                            headers: reply
                                .headers
                                .into_iter()
                                .map(|(name, value)| (name.to_ascii_lowercase(), value))
                                .collect(),
                            body: reply.body.into_bytes().into(),
                        });
                    }
                }
                Ok(QuotaResponse {
                    status: 404,
                    headers: Vec::new(),
                    body: b"{\"error\":\"Not Found\"}".to_vec().into(),
                })
            })
        }
    }

    fn input(repo: &str) -> ProbeInput {
        ProbeInput {
            repo: repo.to_owned(),
            revision: None,
            gpu: Some("H100".to_owned()),
            weight_quant: None,
            kv_cache_dtype: "auto".to_owned(),
            max_model_len: None,
            token: None,
        }
    }

    fn config_json() -> Value {
        json!({
            "architectures": ["Qwen3ForCausalLM"],
            "model_type": "qwen3",
            "num_hidden_layers": 64,
            "num_attention_heads": 40,
            "num_key_value_heads": 8,
            "head_dim": 128,
            "hidden_size": 5120,
            "max_position_embeddings": 262144,
            "torch_dtype": "bfloat16",
        })
    }

    // --- requirement 1: the catalog never invents a free-plan answer ---

    #[test]
    fn the_catalog_marks_one_card_unavailable_on_the_free_plan_and_the_rest_unknown() {
        let body = catalog_body();
        let gpus = body["platforms"][0]["gpus"]
            .as_array()
            .expect("the platform lists GPUs");
        assert_eq!(gpus.len(), 11, "the Modal catalog has eleven cards");

        let mut unavailable = Vec::new();
        for gpu in gpus {
            let state = gpu["free_plan"].as_str().expect("free_plan is a string");
            match state {
                "unavailable" => unavailable.push(gpu["id"].as_str().expect("id").to_owned()),
                "unknown" => {}
                other => panic!(
                    "free_plan on {} is {other:?}; only `available`, `unavailable` and `unknown` \
                     are honest answers",
                    gpu["id"]
                ),
            }
            // The VRAM source is a four-state fact, never a bare number with no provenance.
            if let Some(source) = gpu["vram_source"].as_str() {
                assert!(
                    ["docs", "datasheet", "observed", "unverified"].contains(&source),
                    "{} has VRAM source {source:?}",
                    gpu["id"]
                );
            }
        }
        assert_eq!(
            unavailable,
            vec!["H100".to_owned()],
            "exactly one card is observed unavailable on the free plan"
        );

        // The specific fabrication the design document warns about: `(Paid)` is not derivable from
        // `unknown`, so it must not appear anywhere in the payload.
        let text = body.to_string();
        assert!(
            !text.contains("(Paid)"),
            "the catalog must not render a free-plan `unknown` as `(Paid)`"
        );
        assert_eq!(body["checked_at"], CATALOG_CHECKED_AT);
        assert!(
            body["platforms"][0]["plan_limits"]
                .as_array()
                .expect("plan limits")
                .iter()
                .any(|limit| limit["id"] == "free"),
            "the free plan's limits are part of the catalog"
        );
    }

    // --- requirement 2: the gated-repository base-model fallback ---

    #[tokio::test]
    async fn a_gated_repository_borrows_the_public_base_models_architecture() {
        let http = FakeHttp::default();
        http.on(
            "huggingface.co/api/models/acme/Qwen3.8-27B-AWQ",
            Reply::json(json!({
                "id": "acme/Qwen3.8-27B-AWQ",
                "sha": "0123456789abcdef",
                "gated": "auto",
                "safetensors": {"total": 27_000_000_000u64},
                "cardData": {
                    "base_model": "Qwen/Qwen3.8-27B",
                    "base_model_relation": "quantized",
                },
            })),
        );
        // The gated repository refuses its config, exactly as the Hub does.
        http.on(
            "/acme/Qwen3.8-27B-AWQ/raw/main/config.json",
            Reply::status(
                401,
                &[("x-error-code", "GatedRepo")],
                "{\"error\":\"GatedRepo\"}",
            ),
        );
        http.on(
            "/Qwen/Qwen3.8-27B/raw/main/config.json",
            Reply::json(config_json()),
        );

        let body = probe_http(&http, &input("acme/Qwen3.8-27B-AWQ")).await;

        assert_eq!(body["arch"]["source"], "base_model");
        assert_eq!(body["arch"]["repo"], "Qwen/Qwen3.8-27B");
        assert_eq!(body["arch"]["relation"], "quantized");
        assert_eq!(body["arch"]["shares_architecture"], true);
        assert_eq!(
            body["arch"]["num_hidden_layers"], 64,
            "the layer count came from the base model's config.json"
        );
        assert_eq!(body["arch"]["num_key_value_heads"], 8);
        assert_eq!(body["gated"]["state"], "auto");
        assert_eq!(body["gated"]["needs_token"], true);
        assert_eq!(body["sha"], "0123456789abcdef");
        assert_eq!(body["revision"], "main");
        // The base model's config was fetched anonymously: no credential travelled to it.
        for (name, _) in http.headers_to("/Qwen/Qwen3.8-27B/raw") {
            assert!(
                !name.eq_ignore_ascii_case("authorization"),
                "the base repository must be read anonymously"
            );
        }
        assert_eq!(body["params"]["source"], "hf");
        assert_eq!(body["params"]["from_name"]["token"], "27B");
        assert_eq!(body["params"]["agrees_with_name"], true);
        assert_eq!(body["confidence"], "analytic");
        assert_eq!(body["sizing"]["confidence"], "analytic");
        assert!(body["sizing"]["kv_pool_tokens"].as_u64().is_some());
    }

    #[tokio::test]
    async fn a_base_model_on_another_relation_is_followed_but_flagged() {
        let http = FakeHttp::default();
        http.on(
            "huggingface.co/api/models/acme/Finetune",
            Reply::json(json!({
                "id": "acme/Finetune",
                "gated": false,
                "cardData": {"base_model": "Qwen/Qwen3.8-27B", "base_model_relation": "finetune"},
            })),
        );
        http.on(
            "/acme/Finetune/raw/main/config.json",
            Reply::status(401, &[("x-error-code", "GatedRepo")], "{}"),
        );
        http.on(
            "/Qwen/Qwen3.8-27B/raw/main/config.json",
            Reply::json(config_json()),
        );

        let body = probe_http(&http, &input("acme/Finetune")).await;
        assert_eq!(body["arch"]["source"], "base_model");
        assert_eq!(body["arch"]["shares_architecture"], false);
        assert!(
            body["warnings"]
                .as_array()
                .expect("warnings")
                .iter()
                .any(|warning| warning
                    .as_str()
                    .unwrap_or_default()
                    .contains("a fine-tune can change the layer count")),
            "the doubt is stated, not hidden: {body}"
        );
        // `gated: false` on the metadata call, yet the config call was refused: the probe believes
        // the 401 rather than the flag.
        assert_eq!(body["gated"]["state"], "open");
        assert_eq!(body["gated"]["needs_token"], true);
    }

    // --- requirement 3: both fetches failing is reported as unavailable ---

    #[tokio::test]
    async fn when_both_configs_fail_the_architecture_is_unavailable_with_a_reason() {
        let http = FakeHttp::default();
        http.on(
            "huggingface.co/api/models/acme/Mystery-7B",
            Reply::json(json!({
                "id": "acme/Mystery-7B",
                "gated": "manual",
                "cardData": {"base_model": "acme/Private-Base"},
            })),
        );
        http.on(
            "/acme/Mystery-7B/raw/main/config.json",
            Reply::status(401, &[("x-error-code", "GatedRepo")], "{}"),
        );
        http.on(
            "/acme/Private-Base/raw/main/config.json",
            Reply::status(401, &[("x-error-code", "GatedRepo")], "{}"),
        );

        let body = probe_http(&http, &input("acme/Mystery-7B")).await;
        assert_eq!(body["arch"]["source"], "unavailable");
        assert_eq!(body["confidence"], "unavailable");
        assert_eq!(body["sizing"], Value::Null);
        assert_eq!(body["arch"]["num_hidden_layers"], Value::Null);
        let errors = body["errors"].as_array().expect("errors");
        assert!(
            errors.iter().any(|error| error
                .as_str()
                .unwrap_or_default()
                .contains("acme/Mystery-7B")),
            "the repository that failed is named: {body}"
        );
        assert!(
            errors.iter().any(|error| error
                .as_str()
                .unwrap_or_default()
                .contains("acme/Private-Base")),
            "the base repository that also failed is named: {body}"
        );
        assert_eq!(body["gated"]["state"], "manual");
    }

    #[tokio::test]
    async fn a_repository_whose_metadata_and_config_both_404_is_unavailable() {
        let http = FakeHttp::default();
        let body = probe_http(&http, &input("nobody/Nothing-3B")).await;
        assert_eq!(body["arch"]["source"], "unavailable");
        assert_eq!(body["confidence"], "unavailable");
        assert_eq!(body["gated"]["state"], Value::Null);
        assert!(!body["errors"].as_array().expect("errors").is_empty());
    }

    // --- requirement 4: the token never comes back ---

    #[tokio::test]
    async fn the_token_appears_in_no_response_body_and_no_error_message() {
        const TOKEN: &str = "hf_fake_token_for_tests_9d3f2a";
        let http = FakeHttp::default();
        http.on(
            "huggingface.co/api/models/acme/Gated-7B",
            Reply::json(json!({"id": "acme/Gated-7B", "gated": "auto"})),
        );
        // The (fake) server echoes the credential back in its error body, which is the worst case:
        // it must not survive into the response.
        http.on(
            "/acme/Gated-7B/raw/main/config.json",
            Reply::status(
                401,
                &[("x-error-code", "GatedRepo")],
                "{\"error\":\"token hf_fake_token_for_tests_9d3f2a rejected\"}",
            ),
        );

        let mut probe = input("acme/Gated-7B");
        probe.token = Some(TOKEN.to_owned());
        let mut body = probe_http(&http, &probe).await;
        scrub_secret(&mut body, Some(TOKEN));

        // The token really was sent, in a header and not in a URL.
        let headers = http.headers_to("/acme/Gated-7B/raw");
        assert!(
            headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                    && value == &format!("Bearer {TOKEN}")),
            "the token was sent as a bearer header: {headers:?}"
        );
        for url in http.urls() {
            assert!(
                !url.contains(TOKEN),
                "the token must not appear in a URL: {url}"
            );
        }

        assert!(
            !body.to_string().contains(TOKEN),
            "the response body contains the token: {body}"
        );

        // And the error messages a failure would produce cannot contain it either: the messages
        // that are built from a response never include its headers or URL.
        let failure = fetch_config(&http, &raw_config_url("acme/Gated-7B", None), Some(TOKEN))
            .await
            .expect_err("the gated repository refuses the config");
        let message = failure.to_string();
        assert!(
            !message.contains(TOKEN),
            "a config failure message contains the token: {message}"
        );
    }

    // --- the probe's smaller promises ---

    #[test]
    fn a_parameter_count_is_read_from_the_name_but_an_api_version_is_not() {
        let hint = params_from_name("Qwen/Qwen3.8-27B").expect("27B is in the name");
        assert_eq!(hint.token, "27B");
        assert_eq!(hint.value, 27_000_000_000);
        assert_eq!(
            params_from_name("meta-llama/Llama-3.1-8B").map(|h| h.value),
            Some(8_000_000_000)
        );
        assert_eq!(
            params_from_name("acme/mistral-7b-instruct").map(|h| h.value),
            Some(7_000_000_000)
        );
        assert_eq!(
            params_from_name("acme/model-1500M"),
            None,
            "only `B` is a parameter count"
        );
        // `3.8` is a version, and a trailing `B` with no number is not a size.
        assert_eq!(params_from_name("acme/Qwen3.8"), None);
    }

    #[tokio::test]
    async fn no_gpu_named_means_no_quant_is_claimed_to_run() {
        let http = FakeHttp::default();
        http.on(
            "huggingface.co/api/models/acme/Plain-7B",
            Reply::json(json!({"id": "acme/Plain-7B", "gated": false})),
        );
        http.on(
            "/acme/Plain-7B/raw/main/config.json",
            Reply::json(config_json()),
        );
        let mut probe = input("acme/Plain-7B");
        probe.gpu = None;
        let body = probe_http(&http, &probe).await;
        assert_eq!(body["arch"]["source"], "repo");
        assert_eq!(body["sizing"], Value::Null);
        assert_eq!(body["confidence"], "unavailable");
        let rows = body["quants"].as_array().expect("quants");
        assert!(!rows.is_empty());
        for row in rows {
            assert_eq!(
                row["support"], "unverified",
                "with no card named, nothing is claimed to run: {row}"
            );
        }
    }

    #[tokio::test]
    async fn a_quantised_checkpoint_reports_its_quantisation_as_in_the_repository() {
        let http = FakeHttp::default();
        http.on(
            "huggingface.co/api/models/acme/Qwen-27B-FP8",
            Reply::json(json!({
                "id": "acme/Qwen-27B-FP8",
                "gated": false,
                "safetensors": {"total": 27_000_000_000u64},
            })),
        );
        let mut config = config_json();
        config["quantization_config"] = json!({"quant_method": "fp8"});
        http.on(
            "/acme/Qwen-27B-FP8/raw/main/config.json",
            Reply::json(config),
        );

        let body = probe_http(&http, &input("acme/Qwen-27B-FP8")).await;
        let rows = body["quants"].as_array().expect("quants");
        let fp8 = rows
            .iter()
            .find(|row| row["id"] == "fp8")
            .expect("the declared quantisation is listed");
        assert_eq!(fp8["in_repo"], true);
        assert_eq!(fp8["per_repo"], true);
        assert_eq!(fp8["support"], "supported");
        assert_eq!(fp8["bytes_per_param"], 1.0);
        // One byte per parameter and 27e9 parameters: the weights are priced from the declared
        // quantisation, not from the dtype.
        assert_eq!(
            body["weight_bytes"]["bytes"], 27_000_000_000u64,
            "sizing priced the weights at the declared quantisation"
        );
    }

    #[test]
    fn a_repository_id_is_validated_before_it_reaches_a_generated_file() {
        assert!(repo_problem("Qwen/Qwen3.8-27B").is_none());
        assert!(repo_problem("acme/evil\").append(open(\"/etc/passwd").is_some());
        assert!(repo_problem("a/b/c").is_some());
        assert!(repo_problem("../etc").is_some());
        assert!(repo_problem("").is_some());
    }

    #[test]
    fn a_python_string_literal_cannot_be_broken_out_of() {
        assert_eq!(py_str("a\"b"), "\"a\\\"b\"");
        assert_eq!(py_str("a\\b"), "\"a\\\\b\"");
        assert_eq!(py_str("a\nb"), "\"a\\nb\"");
    }

    #[test]
    fn only_a_listener_change_needs_a_restart() {
        let before = Config::default();
        let mut after = before.clone();
        assert_eq!(change_needs_restart(&before, &after), None);

        // A provider edit is adopted from the snapshot, so it needs no restart.
        after.providers.push(mr_core::Provider::new(
            "rental-x",
            Dialect::ModalVllm,
            "https://example.modal.run",
        ));
        assert_eq!(change_needs_restart(&before, &after), None);

        let mut port = before.clone();
        port.relay.port = port.relay.port.wrapping_add(1);
        assert!(change_needs_restart(&before, &port)
            .expect("a port change needs a restart")
            .contains("relay listener"));
    }

    #[test]
    fn a_credential_is_rejected_unless_the_relay_could_split_it() {
        // Exactly the shape `relay/server.rs` expects: one dot, both halves non-empty.
        assert_eq!(modal_credential_problem("wk-abc.ws-def"), None);
        // No dot: `split_once('.')` would yield no secret half at all.
        assert!(modal_credential_problem("wk-abc").is_some());
        // A second dot: `split_once` splits on the first, so the secret half would be truncated.
        assert!(modal_credential_problem("wk-a.ws-b.ws-c").is_some());
        // An empty half is not a credential, even though it has the right punctuation.
        assert!(modal_credential_problem("wk-a.").is_some());
        assert!(modal_credential_problem(".ws-b").is_some());
        // Whitespace would end up in an HTTP header value.
        assert!(modal_credential_problem("wk-a. ws-b").is_some());
    }

    #[test]
    fn the_bundle_is_a_modal_app_that_matches_the_platform_facts() {
        let deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "H100");
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);
        assert_eq!(bundle.filename, "modal_app.py");
        assert!(bundle.id.starts_with("bundle-"));
        assert_eq!(bundle.command, "modal deploy modal_app.py");
        assert!(
            !bundle.command.contains("--detach"),
            "Modal has no detach flag"
        );
        assert!(bundle.text.contains("modal.web_server(port=8000"));
        assert!(bundle.text.contains("\"Qwen/Qwen3.8-27B\""));
        assert!(bundle.text.contains("\"H100\""));

        let mut multi = deployment.clone();
        multi.gpu_count = 8;
        let bundle = deployment_bundle("rental-acc-qwen38", &multi, gpu);
        assert!(
            bundle.text.contains("\"H100:8\""),
            "multi-GPU uses the `card:N` spelling: {}",
            bundle.text
        );
    }

    #[test]
    fn a_streaming_bundle_serves_vllm_on_the_public_port_and_generates_no_assembler() {
        let deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "H100");
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);
        assert!(!bundle.text.contains("BATCH_SIZE"));
        assert!(!bundle.text.contains("BatchHandler"));
        assert!(bundle.text.contains("\"--port\",\n        \"8000\","));
        assert!(bundle.text.contains("subprocess.Popen(args)"));
        assert!(bundle.text.contains("max_inputs=MAX_NUM_SEQS"));
    }

    #[test]
    fn a_streaming_bundle_admits_more_than_one_request_per_container() {
        // Modal's default is one input per container, so a second simultaneous request starts a
        // second card: a live run reached ten RTX PRO 6000 containers, $3.03/hour each. An unset
        // `max_num_seqs` must therefore not become `MAX_NUM_SEQS = 1`, and it must still emit no
        // `--max-num-seqs` flag, because that value is only an admission ceiling here.
        let deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "H100");
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        assert_eq!(deployment.max_num_seqs, None);
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);
        assert!(bundle.text.contains("MAX_NUM_SEQS = 32"), "{}", bundle.text);
        assert!(!bundle.text.contains("\"--max-num-seqs\""));
        assert!(!bundle.text.contains("max_inputs=1"));

        let mut pinned = deployment.clone();
        pinned.max_num_seqs = Some(54);
        let bundle = deployment_bundle("rental-acc-qwen38", &pinned, gpu);
        assert!(bundle.text.contains("MAX_NUM_SEQS = 54"), "{}", bundle.text);
        assert!(bundle.text.contains("\"--max-num-seqs\",\n        \"54\","));
    }

    #[test]
    fn a_batch_bundle_generates_the_queue_that_decides_when_a_batch_ships() {
        let mut deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "H100");
        deployment.mode = RentalMode::Batch;
        deployment.batch = Some(BatchShape {
            size: 8,
            fill: BatchFill::Full,
            timeout_secs: None,
        });
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);

        assert!(bundle.text.contains("BATCH_SIZE = 8\n"), "{}", bundle.text);
        assert!(bundle.text.contains("BATCH_TIMEOUT = None\n"));
        // vLLM keeps an internal port so the assembler can own the one Modal proxies.
        assert!(bundle.text.contains("VLLM_PORT = 8001"));
        assert!(bundle.text.contains("\"--port\",\n        str(VLLM_PORT),"));
        assert!(bundle.text.contains("def _assemble():"));
        assert!(bundle
            .text
            .contains("class BatchHandler(BaseHTTPRequestHandler):"));
        assert!(bundle
            .text
            .contains("ThreadingHTTPServer((\"0.0.0.0\", 8000), BatchHandler)"));
        // A batch that can never be admitted would wait for a request Modal will not let in.
        assert!(bundle.text.contains("max_inputs=BATCH_SIZE"));
        assert!(!bundle.text.contains("max_inputs=MAX_NUM_SEQS"));
        // The old text admitted the queue was missing; the generated file must not say that now.
        assert!(!bundle.text.contains("UNCERTAIN"));
        assert!(!bundle.text.contains("does not generate it"));
        assert!(bundle.text.contains("however long that takes"));
    }

    #[test]
    fn a_timed_batch_bundle_ships_what_arrived_when_the_timeout_expires() {
        let mut deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "H100");
        deployment.mode = RentalMode::Batch;
        deployment.batch = Some(BatchShape {
            size: 4,
            fill: BatchFill::Timeout,
            timeout_secs: Some(45),
        });
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);

        assert!(bundle.text.contains("BATCH_SIZE = 4\n"), "{}", bundle.text);
        assert!(bundle.text.contains("BATCH_TIMEOUT = 45.0\n"));
        assert!(bundle.text.contains("half-full batch still goes out"));
        assert!(bundle
            .text
            .contains("deadline = time.monotonic() + BATCH_TIMEOUT"));
    }

    #[test]
    fn a_gated_deployment_mounts_the_secret_by_name_and_embeds_no_token() {
        let mut deployment =
            RentalDeployment::new("acc", "orcarouter/Qwen3.8-27B-Uncensored", "H100");
        deployment.hf_token = Some("hf-token-1".to_owned());
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);

        assert!(bundle
            .text
            .contains("HF_SECRET = modal.Secret.from_name(\"manarouter-hf\")"));
        assert!(bundle.text.contains("    secrets=[HF_SECRET],"));
        // The secret's name is the only thing the file may know about it.
        assert!(!bundle.text.contains("modal.Secret("));
        assert!(!bundle.text.contains("HF_TOKEN="));
        assert!(!bundle.text.contains("hf-token-1"));
    }

    #[test]
    fn a_public_deployment_mounts_no_secret() {
        let deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "H100");
        let gpu = gpu_by_id("H100").expect("H100 is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);
        assert!(!bundle.text.contains("modal.Secret"));
        assert!(!bundle.text.contains("secrets="));
        assert!(!bundle.text.contains("HF_SECRET"));
    }

    #[test]
    fn the_generated_python_quotes_each_argument_exactly_once() {
        // This bundle is text an operator pastes into `modal deploy`. `py_str` already returns a
        // quoted literal, so wrapping it in another pair produced `""fp8""`: plausible-looking
        // output that is a Python syntax error, and one no assertion here would have caught
        // because nothing parsed the text. It surfaced only by applying a real deployment and
        // reading the returned bundle.
        let mut deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "L40S");
        deployment.weight_quant = "fp8".to_owned();
        deployment.kv_cache_dtype = "fp8".to_owned();
        deployment.max_model_len = Some(262_144);
        deployment.max_num_seqs = Some(6);
        let gpu = gpu_by_id("L40S").expect("L40S is in the catalog");
        let bundle = deployment_bundle("rental-acc-qwen38", &deployment, gpu);

        assert!(
            bundle
                .text
                .contains("        \"--quantization\",\n        \"fp8\",\n"),
            "each argument is one Python string literal:\n{}",
            bundle.text
        );
        assert!(
            bundle
                .text
                .contains("        \"--kv-cache-dtype\",\n        \"fp8\",\n"),
            "the KV dtype is one Python string literal:\n{}",
            bundle.text
        );
        assert!(
            !bundle.text.contains("\"\""),
            "no value is quoted twice anywhere in the file:\n{}",
            bundle.text
        );
        // The numeric arguments take their values from the deployment, not from a default.
        for expected in [
            "\"--max-model-len\",\n        \"262144\",",
            "\"--max-num-seqs\",\n        \"6\",",
        ] {
            assert!(
                bundle.text.contains(expected),
                "missing {expected}:\n{}",
                bundle.text
            );
        }
    }

    #[test]
    fn a_client_that_forwards_only_the_pool_still_gets_a_fit() {
        // `SizingResult` carries figures that are conveniences: the conductor derives the aligned
        // pool, the block count and the session count from the pool and the context. A client that
        // forwards only what it computed is therefore not wrong, and the rentals view must not
        // report "unknown" about a deployment that sized. This is the shape the panel used to send,
        // and it made `fit` read `known: true, kv_pool_tokens: null`.
        let request: mr_core::gpu::sizing::SizingRequest = serde_json::from_value(json!({
            "gpu_id": "RTX-PRO-6000",
            "weights_bytes": 27_781_427_952u64,
            "kv_cache_dtype": "fp8",
            "max_model_len": 262_144u64,
            "kv_bytes_per_token": 32_768u64,
        }))
        .expect("only five fields are required");
        let mut sizing = mr_core::gpu::sizing::size_deployment(&request);
        let pool = sizing.kv_pool_tokens.expect("the arithmetic resolved");
        let aligned = sizing
            .kv_pool_tokens_aligned
            .expect("the arithmetic aligned");
        assert!(aligned <= pool);

        // Derive nothing, the way a minimal client would.
        sizing.kv_blocks = None;
        sizing.kv_pool_tokens_aligned = None;
        sizing.kv_bytes_per_session_at_max_len = None;
        sizing.simultaneous_sessions_at_max_len = None;

        let mut deployment = RentalDeployment::new("acc", "Qwen/Qwen3.8-27B", "RTX-PRO-6000");
        deployment.sizing = Some(Box::new(sizing));
        let fit = fit_of(&deployment);
        assert_eq!(fit["known"], true);
        assert_eq!(fit["kv_pool_tokens"].as_u64(), Some(pool));
        assert_eq!(fit["sessions_at_max_len"].as_u64(), Some(pool / 262_144));

        // When the client does forward the aligned figure, that one wins.
        if let Some(sizing) = deployment.sizing.as_deref_mut() {
            sizing.kv_pool_tokens_aligned = Some(aligned);
        }
        let fit = fit_of(&deployment);
        assert_eq!(fit["kv_pool_tokens"].as_u64(), Some(aligned));
        assert_eq!(fit["sessions_at_max_len"].as_u64(), Some(aligned / 262_144));
    }

    // --- the apply path, against a real temporary config and a real vault ---

    /// A state whose config, snapshot, database and vault all live in one temporary directory, so
    /// `commit` performs its real atomic save and the vault is a real encrypted file.
    fn state_in(dir: &Path) -> AppState {
        let config_path = dir.join("config.json");
        Config::default()
            .save(&config_path)
            .expect("write the initial config");
        AppState::new(
            Config::default(),
            StateFiles {
                config_path,
                snapshot_path: dir.join("routes.snapshot"),
                database_path: dir.join("state.sqlite"),
                dirs: ServeDirs {
                    panel: None,
                    docs: None,
                },
            },
            PanelAuth::resolve("127.0.0.1", None, false).expect("loopback panel needs no password"),
            20900,
            20901,
        )
        .expect("build state")
    }

    /// Run one apply request and keep the error readable even though `ApiError` has no `Debug`:
    /// the message is taken from the response body the handler would actually have sent.
    async fn apply_body(state: &AppState, body: Value) -> Result<Value, String> {
        match post_apply(State(state.clone()), JsonBody(body)).await {
            Ok(Json(value)) => Ok(value),
            Err(error) => {
                use axum::response::IntoResponse;
                let response = error.into_response();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("read the error body");
                Err(format!(
                    "HTTP {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ))
            }
        }
    }

    /// The body a handler would have sent for an error, so a test can assert the message without
    /// `ApiError` implementing `Debug`.
    async fn error_text(error: ApiError) -> String {
        use axum::response::IntoResponse;
        let response = error.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read the error body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Create a Modal rental account the way the panel does.
    async fn create_modal_account(state: &AppState, id: &str, workspace: &str, credential: &str) {
        let plan_limits = serde_json::to_value(
            mr_core::gpu::catalog::platform_by_id("modal")
                .expect("modal is in the catalog")
                .plan_limits,
        )
        .expect("serialise the plan limits");
        let created = apply_body(
            state,
            json!({
                "action": "create-account",
                "account": {
                    "id": id,
                    "label": id,
                    "platform": "modal",
                    "plan": "paid",
                    "workspace": workspace,
                    "token": credential,
                    "max_concurrent_containers": 10,
                },
                "plan_limits": plan_limits,
            }),
        )
        .await
        .expect("create the account");
        assert_eq!(created["account"]["workspace"], workspace);
    }

    #[tokio::test]
    async fn each_rental_account_gets_its_own_credential_and_vault_entry() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        // Shaped like Modal proxy tokens so they pass the same check production does.
        let first_credential = "wk-test-id-9f3a2c.ws-test-secret-4b8d1e";
        let second_credential = "wk-second-id-1a2b3c.ws-second-secret-5e6f7a";

        create_modal_account(&state, "mana-modal", "mana", first_credential).await;
        create_modal_account(&state, "second-rental", "mana", second_credential).await;

        let first = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "H100",
                    "gpu_count": 1,
                    "max_model_len": 32768,
                },
            }),
        )
        .await
        .expect("create the first deployment");
        assert_eq!(first["account_id"], "mana-modal");
        assert!(!first.to_string().contains(first_credential));
        assert!(!first.to_string().contains(second_credential));

        let second = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "second-rental",
                "deployment": {
                    "repo": "Qwen/Qwen3-14B",
                    "gpu": "H100",
                    "gpu_count": 1,
                    "max_model_len": 32768,
                },
            }),
        )
        .await
        .expect("create the second deployment");
        assert_eq!(second["account_id"], "second-rental");
        assert!(!second.to_string().contains(first_credential));
        assert!(!second.to_string().contains(second_credential));

        // Two accounts, two vault entries: neither can be reached through the other.
        {
            let vault = state.vault.lock().await;
            let mut rental_ids: Vec<String> = vault
                .ids()
                .into_iter()
                .filter(|id| id.starts_with("rental:"))
                .collect();
            rental_ids.sort();
            assert_eq!(
                rental_ids,
                vec![
                    "rental:mana-modal".to_owned(),
                    "rental:second-rental".to_owned()
                ],
                "one entry per account, not one per workspace"
            );
            assert_eq!(
                vault
                    .get("rental:mana-modal")
                    .expect("read the vault")
                    .as_deref(),
                Some(first_credential)
            );
            assert_eq!(
                vault
                    .get("rental:second-rental")
                    .expect("read the vault")
                    .as_deref(),
                Some(second_credential)
            );
        }

        let config = state.current_config().await;
        assert_eq!(config.providers.len(), 2);
        let mut by_account: Vec<(String, String)> = config
            .providers
            .iter()
            .map(|provider| {
                let deployment = parse_deployment(provider.id.as_ref(), &provider.extra)
                    .expect("read the deployment")
                    .expect("a rental deployment");
                let reference = provider.accounts[0]
                    .secret_ref
                    .clone()
                    .expect("the provider references its account's entry");
                (deployment.account, reference)
            })
            .collect();
        by_account.sort();
        assert_eq!(
            by_account,
            vec![
                ("mana-modal".to_owned(), "rental:mana-modal".to_owned()),
                (
                    "second-rental".to_owned(),
                    "rental:second-rental".to_owned()
                ),
            ],
            "each provider references its account's entry"
        );
        // A second model may be hosted under the same account. The account owns the credential;
        // the new deployment names that account and references the same vault entry.
        let third = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-32B",
                    "gpu": "H100",
                    "gpu_count": 1,
                },
            }),
        )
        .await
        .expect("a second model may share an account");
        assert_eq!(third["account_id"], "mana-modal");
        assert_eq!(third["vault"]["secret_id"], "rental:mana-modal");
        assert!(!third.to_string().contains(first_credential));
        assert!(!third.to_string().contains(second_credential));
        assert_eq!(state.current_config().await.providers.len(), 3);

        let body = rentals_body(&state).await;
        assert_eq!(body["accounts"].as_array().expect("accounts").len(), 2);
        assert_eq!(
            body["deployments"].as_array().expect("deployments").len(),
            3
        );
        assert!(!body.to_string().contains(first_credential));
        assert!(!body.to_string().contains(second_credential));
    }

    #[tokio::test]
    async fn an_account_accepts_an_api_token_pair_beside_the_proxy_pair() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        let created = apply_body(
            &state,
            json!({
                "action": "create-account",
                "account": {
                    "id": "mana-modal",
                    "platform": "modal",
                    "plan": "paid",
                    "workspace": "mana",
                    "token": "wk-proxy-id.ws-proxy-secret",
                    "api_token_id": "ak-api-id",
                    "api_token_secret": "as-api-secret",
                },
            }),
        )
        .await
        .expect("create the account with both credential pairs");
        assert_eq!(created["account"]["has_api_credential"], json!(true));
        let api_vault = created["api_vault"].clone();
        assert_eq!(api_vault["credential_id"], json!("rental:mana-modal:api"));
        assert!(api_vault["fingerprint"].is_string());

        // The API pair lives in its own vault entry; the proxy entry is untouched.
        {
            let vault = state.vault.lock().await;
            assert_eq!(
                vault
                    .get("rental:mana-modal")
                    .expect("proxy entry")
                    .as_deref(),
                Some("wk-proxy-id.ws-proxy-secret")
            );
            assert_eq!(
                vault
                    .get("rental:mana-modal:api")
                    .expect("api entry")
                    .as_deref(),
                Some("ak-api-id.as-api-secret")
            );
        }

        // A proxy-shaped pair in the API fields is refused, with a message pointing at the split.
        let refused = apply_body(
            &state,
            json!({
                "action": "create-account",
                "account": {
                    "id": "bad-api-account",
                    "platform": "modal",
                    "plan": "unknown",
                    "workspace": "mana",
                    "token": "wk-proxy-id.ws-proxy-secret",
                    "api_token_id": "wk-wrong-id",
                    "api_token_secret": "ws-wrong-secret",
                },
            }),
        )
        .await
        .expect_err("a proxy pair is not an API pair");
        let error = refused.to_string();
        assert!(
            error.contains("ak-"),
            "the refusal names the api prefixes: {error}"
        );

        // One half alone is also refused: the pair is atomic.
        let refused = apply_body(
            &state,
            json!({
                "action": "create-account",
                "account": {
                    "id": "bad-api-account",
                    "platform": "modal",
                    "plan": "unknown",
                    "workspace": "mana",
                    "token": "wk-proxy-id.ws-proxy-secret",
                    "api_token_id": "ak-only-id",
                },
            }),
        )
        .await
        .expect_err("one half of the API pair is not enough");
        let error = refused.to_string();
        assert!(
            error.contains("api_token_secret"),
            "the refusal names the missing half"
        );
    }

    #[tokio::test]
    async fn a_deployment_derives_its_url_unless_the_operator_overrides_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(
            &state,
            "mana-modal",
            "mana",
            "wk-derive-id-0a1b2c.ws-derive-3d4e5f",
        )
        .await;

        // Absent base_url: derived from the workspace and the generated app name.
        apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "H100", "gpu_count": 1},
            }),
        )
        .await
        .expect("create the derived deployment");
        let config = state.current_config().await;
        assert_eq!(
            config.providers[0].base_url,
            "https://mana--manarouter-rental-mana-modal-qwen3-8b-streaming-h100-serve.modal.run"
        );
        let extra = &config.providers[0].extra[mr_core::rental::RENTAL_EXTRA_KEY];
        assert_eq!(extra["mode"], "streaming");
        assert!(
            extra.get("batch_gpu").is_none(),
            "the shared batch card is gone: {extra}"
        );

        // Present base_url: an explicit override, because `modal deploy`'s printed URL wins.
        create_modal_account(
            &state,
            "override",
            "mana",
            "wk-override-id-6g7h8i.ws-override-9j0k1l",
        )
        .await;
        apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "override",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "H100",
                    "gpu_count": 1,
                    "base_url": "https://mana--printed-by-modal.modal.run",
                },
            }),
        )
        .await
        .expect("create the overridden deployment");
        assert!(state
            .current_config()
            .await
            .providers
            .iter()
            .any(|provider| provider.base_url == "https://mana--printed-by-modal.modal.run"));
    }

    #[tokio::test]
    async fn a_supplied_requantisation_plan_is_kept_with_the_deployment() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(
            &state,
            "mana-modal",
            "mana",
            "wk-requant-id-8s9t0u.ws-requant-1v2w3x",
        )
        .await;

        // A complete plan, shaped by the same type the config reads back. The endpoint must keep it
        // verbatim: this is a recorded recipe the operator runs themselves, never an action.
        let plan = mr_core::gpu::requantize::plan_requantize(&Default::default());
        assert!(plan.runs_remotely);
        apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "H100",
                    "gpu_count": 1,
                    "requantize": serde_json::to_value(&plan).expect("serialise the plan"),
                },
            }),
        )
        .await
        .expect("record the deployment with its requantisation plan");

        let config = state.current_config().await;
        let extra = &config.providers[0].extra[mr_core::rental::RENTAL_EXTRA_KEY];
        let stored: RequantizePlan = serde_json::from_value(extra["requantize"].clone())
            .expect("the plan must round-trip through the provider's extra map");
        assert_eq!(stored, plan);
    }

    #[tokio::test]
    async fn an_account_without_a_workspace_needs_an_explicit_url() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(
            &state,
            "mana-modal",
            "mana",
            "wk-legacy-id-2m3n4o.ws-legacy-5p6q7r",
        )
        .await;

        // Simulate a config written before workspaces were recorded.
        let mut account = rental_accounts(&state.current_config().await)[0].clone();
        account.workspace = None;
        let next = with_account(&state.current_config().await, &account);
        state.commit(next).await.expect("commit the legacy account");

        let refused = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "H100", "gpu_count": 1},
            }),
        )
        .await;
        let message = refused.expect_err("no workspace means no derived URL");
        assert!(
            message.contains("no recorded workspace"),
            "the refusal must say why: {message}"
        );

        // The account is otherwise usable: an explicit URL is accepted.
        apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "H100",
                    "gpu_count": 1,
                    "base_url": "https://mana--printed-by-modal.modal.run",
                },
            }),
        )
        .await
        .expect("an explicit URL stands in for the missing workspace");
    }

    #[tokio::test]
    async fn an_account_without_a_credential_is_reported_rather_than_invented() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());

        let created = apply_body(
            &state,
            json!({
                "action": "create-account",
                "account": {"id": "mana-modal", "plan": "unknown", "workspace": "mana"},
            }),
        )
        .await
        .expect("an account may be created before its credential exists");
        assert_eq!(created["account"]["has_credential"], false);

        let body = rentals_body(&state).await;
        assert_eq!(body["accounts"][0]["has_credential"], false);
        assert!(
            body["accounts"][0]["credential_fingerprint"].is_null(),
            "there is no credential, so there is no fingerprint to show"
        );
        assert_eq!(
            body["deployments"].as_array().expect("deployments").len(),
            0
        );

        // Deploying from it is refused by name. The refusal must leave no half-written provider
        // behind, which is what going through `commit` is for.
        let refused = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "H100",
                    "base_url": "https://mana--qwen3-8b.modal.run",
                },
            }),
        )
        .await;
        assert!(
            refused.is_err(),
            "a deployment with no credential must be refused"
        );
        assert_eq!(state.current_config().await.providers.len(), 0);
        let vault = state.vault.lock().await;
        assert!(
            !vault.contains("rental:mana-modal"),
            "a refused deployment must not create an empty credential entry"
        );
    }

    /// The config fetch must not use the Hub's `/resolve/` path.
    ///
    /// `/resolve/` answers `307` with a `Location` at `/api/resolve-cache/...` rather than the
    /// file, and `mr_core::quota::http` does not follow redirects, so a `/resolve/` URL turns a
    /// readable public base model into `arch.source: "unavailable"`. Observed live on
    /// 2026-11-12 against `Qwen/Qwen3.8-27B`, which is the base model of the repository issue #4
    /// measures, so this is the exact case that regressed.
    #[test]
    fn the_config_url_avoids_the_redirecting_resolve_path() {
        let url = raw_config_url("Qwen/Qwen3.8-27B", None);
        assert_eq!(
            url,
            "https://huggingface.co/Qwen/Qwen3.8-27B/raw/main/config.json"
        );
        assert!(
            !url.contains("/resolve/"),
            "the resolve path redirects and is not followed: {url}"
        );
        assert!(url.ends_with("/config.json"));
        assert_eq!(
            raw_config_url("acme/Model", Some("v1.2")),
            "https://huggingface.co/acme/Model/raw/v1.2/config.json"
        );
    }

    #[test]
    fn the_lane_route_says_which_hosts_can_answer_a_request_and_which_cannot() {
        let body = lanes_body();
        let rows = body["lanes"].as_array().expect("lanes is an array");
        assert_eq!(rows.len(), LaneKind::ALL.len(), "every lane is reported");

        let row = |id: &str| {
            rows.iter()
                .find(|row| row["id"] == json!(id))
                .unwrap_or_else(|| panic!("{id} is missing from the lane catalogue"))
                .clone()
        };

        // Modal answers a request while it is open. Kaggle collects files and has no endpoint.
        assert_eq!(
            row("modal")["can_serve_requests"],
            json!(true),
            "modal should serve requests"
        );
        assert_eq!(
            row("modal")["can_run_batch"],
            json!(true),
            "modal can also run a job"
        );
        assert_eq!(row("kaggle")["can_serve_requests"], json!(false));
        assert_eq!(
            row("kaggle")["can_run_batch"],
            json!(true),
            "a kernel push is unattended"
        );

        // A refusal must quote the platform, not the router's opinion.
        let kaggle_batch = &row("kaggle")["refusals"]["batch"];
        assert_eq!(
            kaggle_batch,
            &json!([]),
            "batch work is allowed on one account"
        );
        let kaggle_sync = row("kaggle")["refusals"]["synchronous"].clone();
        assert_eq!(kaggle_sync[0]["code"], json!("lane_is_batch_only"));

        // Every row carries the fields the panel renders, and every one cites a source.
        for row in rows {
            assert!(row["display"].as_str().unwrap().len() > 1);
            assert!(row["shape"].is_string(), "shape is a word, not an index");
            assert!(row["sources"]
                .as_array()
                .unwrap()
                .iter()
                .all(|s| s.as_str().unwrap().starts_with("https://")));
            assert!(row["headline"].as_str().unwrap().len() > 20);
        }
        assert!(body["note"].as_str().unwrap().contains("not live quotas"));
    }

    // --- the launch plan ---

    /// Build a provider that carries a rental deployment, which is how the config stores one.
    fn rental_provider(id: &str, account: &str, repo: &str, gpu: &str) -> mr_core::model::Provider {
        let deployment = RentalDeployment::new(account, repo, gpu);
        let mut provider: mr_core::model::Provider = serde_json::from_value(json!({
            "id": id,
            "dialect": "open-ai",
            "base_url": "http://127.0.0.1:1/v1",
        }))
        .expect("a minimal provider deserialises");
        provider.extra.insert(
            mr_core::rental::RENTAL_EXTRA_KEY.to_owned(),
            deployment
                .to_extra_value()
                .expect("the deployment serialises"),
        );
        provider
    }

    /// The plan's `note` is prose the panel renders verbatim, so it must not carry source
    /// indentation. A wrapped string literal put a run of eighteen spaces between two words in the
    /// production payload; nothing failed, it simply read wrong on screen.
    #[tokio::test]
    async fn the_launch_plan_note_has_no_baked_in_indentation() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        let body = launch_plan_body(&state, RentalSchedule::Sequential).await;
        let note = body["note"].as_str().expect("note is a string");
        assert!(
            !note.contains("  "),
            "the note must read as one sentence, not as source indentation: {note}"
        );
        assert!(note.starts_with("`waves` holds indices into `deployments`."));
    }

    #[tokio::test]
    async fn the_launch_plan_sequences_cards_and_reports_what_it_cannot_place() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        {
            let mut config = state.config.write().await;
            config.providers.push(rental_provider(
                "lane-a",
                "acct-1",
                "Qwen/Qwen3.8-27B",
                "RTX-PRO-6000",
            ));
            config.providers.push(rental_provider(
                "lane-b",
                "acct-2",
                "Qwen/Qwen3.8-27B",
                "RTX-PRO-6000",
            ));
        }

        // Sequential: two deployments of one card each cannot share a wave, so there are two.
        let body = launch_plan_body(&state, RentalSchedule::Sequential).await;
        assert_eq!(body["schedule"], json!("sequential"));
        assert_eq!(body["max_live"], json!(1));
        assert_eq!(
            body["deployments"].as_array().expect("deployments").len(),
            2
        );
        let waves = body["waves"].as_array().expect("waves");
        assert_eq!(waves.len(), 2, "one card per wave: {body}");
        assert_eq!(waves[0]["cards"], json!(1));
        assert_eq!(waves[1]["cards"], json!(1));
        // The waves hold indices, and the deployments array is what makes them readable.
        assert_eq!(waves[0]["deployments"], json!([0]));
        assert_eq!(waves[1]["deployments"], json!([1]));

        // Batched with room for both: a single wave, which is the deliberate batching case.
        let body = launch_plan_body(&state, RentalSchedule::Batched { max_live: 2 }).await;
        assert_eq!(body["schedule"], json!("batched"));
        assert_eq!(body["max_live"], json!(2));
        let waves = body["waves"].as_array().expect("waves");
        assert_eq!(waves.len(), 1, "both fit in one wave: {body}");
        assert_eq!(waves[0]["cards"], json!(2));
        assert_eq!(waves[0]["deployments"], json!([0, 1]));

        // A batch size of zero is a policy error, not something to clamp to one: clamping would
        // silently turn an impossible request into a billable launch.
        let body = launch_plan_body(&state, RentalSchedule::Batched { max_live: 0 }).await;
        assert_eq!(body["waves"].as_array().expect("waves").len(), 0);
        let problems = body["problems"].as_array().expect("problems");
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0]["field"], json!("schedule"));

        // A deployment that does not parse is reported beside the plan rather than dropped, so a
        // partial plan cannot look complete.
        {
            let mut config = state.config.write().await;
            let mut broken =
                rental_provider("lane-c", "acct-3", "Qwen/Qwen3.8-27B", "RTX-PRO-6000");
            broken.extra.insert(
                mr_core::rental::RENTAL_EXTRA_KEY.to_owned(),
                json!("not an object"),
            );
            config.providers.push(broken);
        }
        let body = launch_plan_body(&state, RentalSchedule::Sequential).await;
        assert_eq!(
            body["malformed"].as_array().expect("malformed").len(),
            1,
            "the unparseable deployment is named: {body}"
        );
        assert_eq!(body["malformed"][0]["provider"], json!("lane-c"));
        assert_eq!(
            body["deployments"].as_array().expect("deployments").len(),
            2
        );
    }

    #[test]
    fn the_launch_plan_schedule_hint_refuses_a_bare_batched() {
        // The route's error text is the only thing telling an operator what to send, so the two
        // forms it accepts are asserted to parse and the ambiguous one to be refused.
        assert_eq!(
            RentalSchedule::parse("sequential"),
            Some(RentalSchedule::Sequential)
        );
        assert_eq!(
            RentalSchedule::parse("batched:4"),
            Some(RentalSchedule::Batched { max_live: 4 })
        );
        assert_eq!(
            RentalSchedule::parse("batched"),
            None,
            "a batch without a size is ambiguous, not a batch of one"
        );
        assert!(SCHEDULE_HINT.contains("batched:N"));
        assert!(SCHEDULE_HINT.contains("sequential"));
    }

    // --- Hugging Face tokens are their own entity, not account state ---

    /// Create a token the way the panel does.
    async fn create_hf_token(state: &AppState, id: &str, secret: &str) {
        let created = apply_body(
            state,
            json!({
                "action": "create-hf-token",
                "token": { "id": id, "label": id, "secret": secret },
            }),
        )
        .await
        .expect("create the token");
        assert_eq!(created["token"]["id"], id);
        assert!(
            !created.to_string().contains(secret),
            "the token value is never echoed: {created}"
        );
    }

    #[tokio::test]
    async fn one_hugging_face_token_serves_deployments_on_several_accounts() {
        // The shape the operator described: several accounts, one or two tokens. Nothing here may
        // require the counts to match, and nothing may tie the token to the first account.
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(&state, "acct-a", "mana", "wk-a-id.ws-a-secret").await;
        create_modal_account(&state, "acct-b", "mana", "wk-b-id.ws-b-secret").await;
        create_hf_token(&state, "hub-main", "hf_test_value_1").await;

        let first = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "acct-a",
                "hf_token": "hub-main",
                "deployment": { "repo": "org/gated-a", "gpu": "H100", "gpu_count": 1 },
            }),
        )
        .await
        .expect("deploy on the first account");
        let second = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "acct-b",
                "hf_token": "hub-main",
                "deployment": { "repo": "org/gated-b", "gpu": "H100", "gpu_count": 1 },
            }),
        )
        .await
        .expect("deploy on the second account");
        // One token, two accounts: both name the same id, and neither account owns it.
        assert_eq!(first["deployment"]["hf_token"], json!("hub-main"));
        assert_eq!(second["deployment"]["hf_token"], json!("hub-main"));
        // The id is recorded; the value is not. The bundle mounts the secret by name, so the
        // response says the secret has to exist rather than implying the container has the token.
        let warnings = first["warnings"].as_array().expect("warnings array");
        assert!(
            warnings.iter().any(|warning| warning.as_str().is_some_and(
                |text| text.contains("mounts the Hugging Face secret `manarouter-hf`")
            )),
            "a deploy naming a token says the secret it mounts must exist: {warnings:?}"
        );

        let config = state.current_config().await;
        assert_eq!(config.huggingface_tokens.len(), 1);
        assert!(
            config
                .rental_accounts
                .iter()
                .all(|account| account.hf_secret_ref.is_none()),
            "no account may inherit a token"
        );

        // A third deployment on one of those accounts may need no token at all.
        let public = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "acct-a",
                "deployment": { "repo": "org/public", "gpu": "H100", "gpu_count": 1 },
            }),
        )
        .await
        .expect("deploy a public repo on the first account");
        assert_eq!(public["deployment"]["hf_token"], Value::Null);
    }

    #[tokio::test]
    async fn one_repository_runs_in_several_modes_and_cards_under_one_account() {
        // The shape the operator asked for: stream on one card, batch shipping a short batch on
        // another, batch waiting for a full batch somewhere else — one account, one repository,
        // three deployments that must not overwrite each other.
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(&state, "mana-modal", "mana", "wk-id-0a1b.ws-secret-2c3d").await;

        let streaming = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "H100", "gpu_count": 1},
            }),
        )
        .await
        .expect("deploy the streaming endpoint");
        let timed = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "L40S",
                    "gpu_count": 1,
                    "mode": "batch",
                    "batch": {"size": 8, "fill": "timeout", "timeout_secs": 30},
                },
            }),
        )
        .await
        .expect("deploy the batch endpoint that ships early");
        let full = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {
                    "repo": "Qwen/Qwen3-8B",
                    "gpu": "RTX-PRO-6000",
                    "gpu_count": 1,
                    "mode": "batch",
                    "batch": {"size": 4, "fill": "full"},
                },
            }),
        )
        .await
        .expect("deploy the batch endpoint that waits");

        // Each deployment is its own provider, with its own derived URL.
        assert_eq!(
            streaming["provider_id"],
            json!("rental-mana-modal-qwen3-8b-streaming-h100")
        );
        assert_eq!(
            timed["provider_id"],
            json!("rental-mana-modal-qwen3-8b-batch-l40s")
        );
        assert_eq!(
            full["provider_id"],
            json!("rental-mana-modal-qwen3-8b-batch-rtx-pro-6000")
        );

        let config = state.current_config().await;
        assert_eq!(
            config.providers[0].base_url,
            "https://mana--manarouter-rental-mana-modal-qwen3-8b-streaming-h100-serve.modal.run"
        );
        assert_eq!(
            config.providers[1].base_url,
            "https://mana--manarouter-rental-mana-modal-qwen3-8b-batch-l40s-serve.modal.run"
        );
        assert_eq!(config.providers.len(), 3);
        let batch = &config.providers[1].extra[mr_core::rental::RENTAL_EXTRA_KEY];
        assert_eq!(batch["mode"], "batch");
        assert_eq!(batch["batch"]["size"], json!(8));
        assert_eq!(batch["batch"]["fill"], json!("timeout"));
        assert_eq!(batch["batch"]["timeout_secs"], json!(30));
        let waiting = &config.providers[2].extra[mr_core::rental::RENTAL_EXTRA_KEY];
        assert_eq!(waiting["batch"]["fill"], json!("full"));
        assert_eq!(waiting["batch"]["timeout_secs"], Value::Null);

        // The same repo, mode and card a second time is still a collision: identity widened, not
        // abandoned.
        let repeat = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "L40S", "mode": "batch", "gpu_count": 1,
                    "batch": {"size": 16, "fill": "timeout", "timeout_secs": 5}},
            }),
        )
        .await;
        let error = repeat.expect_err("the same deployment twice must be refused");
        assert!(error.contains("already"), "got {error}");
    }

    #[tokio::test]
    async fn a_batch_deployment_says_how_its_batch_fills() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(&state, "mana-modal", "mana", "wk-id-4e5f.ws-secret-6g7h").await;

        // A batch with no shape at all cannot be sized, so it is refused by name.
        let bare = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "L40S", "mode": "batch"},
            }),
        )
        .await
        .expect_err("a batch deployment with no batch");
        assert!(bare.contains("batch"), "got {bare}");

        // A timeout with no number is the same problem, one level down.
        let no_seconds = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "L40S", "mode": "batch",
                    "batch": {"size": 4, "fill": "timeout"}},
            }),
        )
        .await
        .expect_err("a timeout with no seconds");
        assert!(no_seconds.contains("timeout_secs"), "got {no_seconds}");

        // A streaming deployment has no batch to fill.
        let misplaced = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "L40S",
                    "batch": {"size": 4, "fill": "full"}},
            }),
        )
        .await
        .expect_err("a batch shape on a streaming deployment");
        assert!(misplaced.contains("batch"), "got {misplaced}");

        // A batch larger than the server can schedule is refused rather than recorded to fail later.
        let oversized = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "mana-modal",
                "deployment": {"repo": "Qwen/Qwen3-8B", "gpu": "L40S", "mode": "batch",
                    "max_num_seqs": 2,
                    "batch": {"size": 8, "fill": "full"}},
            }),
        )
        .await
        .expect_err("a batch the server cannot schedule");
        assert!(oversized.contains("max_num_seqs"), "got {oversized}");
    }

    #[tokio::test]
    async fn a_token_named_by_a_deployment_must_exist_and_hold_a_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_modal_account(&state, "acct-a", "mana", "wk-a-id.ws-a-secret").await;

        let missing = apply_body(
            &state,
            json!({
                "action": "create-deployment",
                "account_id": "acct-a",
                "hf_token": "never-recorded",
                "deployment": { "repo": "org/gated", "gpu": "H100", "gpu_count": 1 },
            }),
        )
        .await
        .expect_err("an unrecorded token is refused");
        assert!(missing.contains("HTTP 404"), "{missing}");
        assert!(missing.contains("never-recorded"), "{missing}");
        assert!(
            !missing.contains("hf_test"),
            "the refusal names the id, never a value: {missing}"
        );
    }

    #[tokio::test]
    async fn create_account_refuses_a_hugging_face_token() {
        // The old spelling is refused with the action that does hold a token, rather than being
        // quietly stored on the account: silently accepting it is how a token became account state.
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        let refused = apply_body(
            &state,
            json!({
                "action": "create-account",
                "account": {
                    "id": "acct-a",
                    "platform": "modal",
                    "plan": "paid",
                    "workspace": "mana",
                    "token": "wk-a-id.ws-a-secret",
                    "hf_token": "hf_test_value_1",
                },
            }),
        )
        .await
        .expect_err("an account may not carry a token");
        assert!(refused.contains("HTTP 400"), "{refused}");
        assert!(refused.contains("create-hf-token"), "{refused}");
        let config = state.current_config().await;
        assert!(config.rental_accounts.is_empty(), "nothing was recorded");
        assert!(config.huggingface_tokens.is_empty());
    }

    #[tokio::test]
    async fn the_probe_reads_a_token_by_id_without_an_account() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        create_hf_token(&state, "hub-main", "hf_test_value_1").await;

        let mut map = Map::new();
        map.insert("hf_token_id".to_owned(), json!("hub-main"));
        let token = match probe_token(&state, &map).await {
            Ok(token) => token,
            Err(error) => panic!("read the token: {}", error_text(error).await),
        };
        assert_eq!(token.as_deref(), Some("hf_test_value_1"));

        // No account is involved at all: the same call works for a config with zero accounts.
        assert!(state.current_config().await.rental_accounts.is_empty());

        let mut unknown = Map::new();
        unknown.insert("hf_token_id".to_owned(), json!("never-recorded"));
        let error = match probe_token(&state, &unknown).await {
            Ok(token) => panic!("an unrecorded id must be refused, got {token:?}"),
            Err(error) => error,
        };
        let text = error_text(error).await;
        assert!(text.contains("never-recorded"), "{text}");
    }
}
