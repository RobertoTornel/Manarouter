//! Quota endpoints: cached provider windows, observed usage, and refresh orchestration.
//!
//! Task E1. Its routes are merged into the guarded router by `api::guarded_router`, so this module
//! owns its own file and nobody has to edit the shared route table to add one.
//!
//! The scaffold here deliberately shares one [`HyperQuotaHttp`] across every fetch: building a
//! `rustls` client per request would reload the webpki roots and open a fresh connection pool each
//! time, which is the cost the shared instance exists to avoid. `HyperQuotaHttp` is not `Clone`, so
//! the shared `&'static` is wrapped in an owning newtype to hand out an `Arc<dyn QuotaHttp>` — the
//! shape [`mr_core::quota::registry::QuotaFetchContext`] takes.
//!
//! GET serves cached snapshots plus declared windows. Refreshes use the shared fetcher registry,
//! with bounded concurrency and isolated per-account results; a missing fetcher is reported per row.
//!
//! # The usage endpoint, and where the arithmetic happens
//!
//! `GET /api/usage` serves the token figures the panel shows. They come from two places, and the
//! difference between them is never hidden:
//!
//! * the **provider** reported them — the upstream sent a `usage` block, and the relay wrote the
//!   counts into the ledger;
//! * the **router** estimated them — the provider reported nothing, and the panel process turned
//!   the byte counts the relay measured into an estimate with the project's one rule (four bytes
//!   per token, [`mr_core::usage::estimate_tokens_from_bytes`]).
//!
//! The estimate is computed **here**, in the panel process, and never on the router's request path:
//! the relay only counts bytes, which is not arithmetic. Every estimated row says so, in the
//! `estimated` flag, and the provider's own numbers — `null` when there were none — are served
//! beside them as `provider_prompt_tokens` and `provider_completion_tokens`. A stored row that
//! says "the provider reported nothing" keeps saying that; the estimate is a presentation, never
//! written over the provider's silence.

use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::{self, StreamExt};
use mr_core::error::Result as CoreResult;
use mr_core::quota::http::{HyperQuotaHttp, QuotaHttp, QuotaRequest, QuotaResponse};
use mr_core::quota::registry::{default_registry, QuotaFetchContext};
use mr_core::quota::types::UsageQuota;
use mr_core::store::{KeyUsageRow, ProviderQuotaState};
use mr_core::usage::estimate_tokens_from_bytes;
use mr_core::{Account, Provider};
use serde_json::{json, Value};

use crate::api::{ApiError, ApiResult};
use crate::state::AppState;

/// The process-wide quota transport, as an owning handle.
///
/// Holds the `&'static` the once-cell hands out, so the value can be moved into an `Arc` without
/// cloning the underlying client (there is nothing to clone — it owns a connection pool).
#[derive(Debug)]
struct SharedQuotaHttp(&'static HyperQuotaHttp);

impl QuotaHttp for SharedQuotaHttp {
    /// Written in the desugared form `#[async_trait]` would generate.
    ///
    /// The trait is expanded by `async-trait` into an early-bound `'life0` plus a late-bound
    /// `'async_trait`, so a hand-written impl has to name both and repeat the where-clauses; naming
    /// only a single `'a` is what made this file fail to compile. Matching the generated signature
    /// exactly is the point — the conductor does not otherwise depend on `async-trait`, and adding
    /// that dependency just to re-desugar one method would edit the lockfile for no benefit.
    fn request<'life0, 'async_trait>(
        &'life0 self,
        request: QuotaRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = CoreResult<QuotaResponse>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        // The inner client is `'static`, so its future outlives every `'async_trait` this can be
        // called with; the box is coerced below rather than re-borrowed from `&'life0 self`.
        let inner: &'static HyperQuotaHttp = self.0;
        Box::pin(async move { inner.request(request).await })
    }
}

/// The shared transport, ready for [`mr_core::quota::registry::QuotaFetchContext`].
///
/// Returns `Result` rather than panicking: a broken TLS root set is a startup problem the caller
/// should report through the API, not a panic that takes the panel down.
pub fn shared_http() -> CoreResult<Arc<dyn QuotaHttp>> {
    Ok(Arc::new(SharedQuotaHttp(HyperQuotaHttp::shared()?)))
}

/// The routes this module contributes, merged into the authenticated API surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/quota", axum::routing::get(get_quota))
        .route("/api/quota/refresh", post(refresh_all))
        .route(
            "/api/providers/{id}/accounts/{account_id}/quota/refresh",
            post(refresh_one),
        )
        .route("/api/usage", axum::routing::get(get_usage))
}

/// The query the usage endpoint accepts.
#[derive(Debug, Default, serde::Deserialize)]
struct UsageQuery {
    /// How many rows to serve, newest first. Capped so one request cannot ask for the whole
    /// ledger.
    limit: Option<u32>,
    /// Only the rows of this client key.
    key: Option<String>,
}

/// The most rows one call may ask for.
const USAGE_LIMIT_MAX: u32 = 500;

/// The rows one call serves when it says nothing about how many.
const USAGE_LIMIT_DEFAULT: u32 = 50;

/// The observed usage of the keys, with the estimate marked where the provider said nothing.
async fn get_usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    let limit = query
        .limit
        .unwrap_or(USAGE_LIMIT_DEFAULT)
        .min(USAGE_LIMIT_MAX);
    let rows = state
        .store
        .recent_key_usage(limit, query.key.as_deref())
        .map_err(|e| ApiError::internal(format!("could not read the usage ledger: {e}")))?;

    let mut served = Vec::with_capacity(rows.len());
    let mut requests = 0u64;
    let mut prompt_tokens = 0u64;
    let mut completion_tokens = 0u64;
    let mut estimated_requests = 0u64;

    for row in rows {
        let view = usage_row(&row);
        requests += 1;
        prompt_tokens += view.prompt_tokens.unwrap_or(0);
        completion_tokens += view.completion_tokens.unwrap_or(0);
        if view.estimated {
            estimated_requests += 1;
        }
        served.push(view.json());
    }

    Ok(Json(json!({
        "rows": served,
        "totals": {
            "requests": requests,
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "estimated_requests": estimated_requests,
        }
    })))
}

/// One usage row, with the provider's numbers and the router's estimate kept apart.
struct UsageView {
    /// The row id.
    id: String,
    /// The client key that made the request.
    client_key_id: String,
    /// When the request happened, in milliseconds since the epoch.
    occurred_at: i64,
    /// The prompt tokens to show: the provider's if it reported any, the estimate otherwise.
    prompt_tokens: Option<u64>,
    /// The completion tokens to show, by the same rule.
    completion_tokens: Option<u64>,
    /// The prompt tokens the provider reported, if it reported any.
    provider_prompt_tokens: Option<u64>,
    /// The completion tokens the provider reported, if it reported any.
    provider_completion_tokens: Option<u64>,
    /// Whether either of the served numbers is the router's estimate.
    estimated: bool,
    /// Whether the provider reported any count at all.
    provider_reported: bool,
    /// How many bytes the request body was, when the relay measured it.
    request_bytes: Option<u64>,
    /// How many bytes the response was, when the relay measured it.
    response_bytes: Option<u64>,
}

impl UsageView {
    /// The row as the API serves it.
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "client_key_id": self.client_key_id,
            "occurred_at": self.occurred_at,
            "prompt_tokens": self.prompt_tokens,
            "completion_tokens": self.completion_tokens,
            "provider_prompt_tokens": self.provider_prompt_tokens,
            "provider_completion_tokens": self.provider_completion_tokens,
            "provider_reported": self.provider_reported,
            "estimated": self.estimated,
            "request_bytes": self.request_bytes,
            "response_bytes": self.response_bytes,
        })
    }
}

/// Build the view of one usage row.
///
/// The provider's numbers always win. The estimate only fills in what the provider left empty,
/// and only when the relay measured enough bytes to estimate from — otherwise the number stays
/// `null`, because "unknown" and "zero" are not the same fact.
fn usage_row(row: &KeyUsageRow) -> UsageView {
    let provider_prompt = row.prompt_tokens;
    let provider_completion = row.completion_tokens;

    let (prompt_tokens, prompt_estimated) = match provider_prompt {
        Some(tokens) => (Some(tokens), false),
        None => (
            row.request_bytes.map(estimate_tokens_from_bytes),
            row.request_bytes.is_some(),
        ),
    };
    let (completion_tokens, completion_estimated) = match provider_completion {
        Some(tokens) => (Some(tokens), false),
        None => (
            row.response_bytes.map(estimate_tokens_from_bytes),
            row.response_bytes.is_some(),
        ),
    };
    let estimated = prompt_estimated || completion_estimated;

    UsageView {
        id: row.id.clone(),
        client_key_id: row.client_key_id.clone(),
        occurred_at: row.occurred_at.as_millis(),
        prompt_tokens,
        completion_tokens,
        provider_prompt_tokens: provider_prompt,
        provider_completion_tokens: provider_completion,
        estimated,
        provider_reported: provider_prompt.is_some() || provider_completion.is_some(),
        request_bytes: row.request_bytes,
        response_bytes: row.response_bytes,
    }
}

async fn get_quota(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    let cached = state
        .store
        .all_provider_quota_state()
        .map_err(|e| ApiError::internal(format!("could not read cached quota: {e}")))?;
    let mut rows = Vec::new();
    for provider in &config.providers {
        for account in &provider.accounts {
            let entry = cached
                .iter()
                .find(|e| e.provider_id == provider.id.as_str() && e.account_id == account.id);
            rows.push(account_row(provider, account, entry, None));
        }
    }
    Ok(Json(json!({"accounts": rows})))
}

async fn refresh_all(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    if !config.quota.enabled {
        return Err(ApiError::bad(
            "quota_disabled",
            "quota tracking is disabled in config.quota.enabled",
        ));
    }
    let targets: Vec<_> = config
        .providers
        .iter()
        .flat_map(|p| {
            p.accounts
                .iter()
                .filter(|a| a.enabled)
                .map(move |a| (p.clone(), a.clone()))
        })
        .collect();
    let results = stream::iter(targets.into_iter().map(|(provider, account)| {
        let state = state.clone();
        async move { refresh_account(&state, &provider, &account).await }
    }))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await;
    Ok(Json(json!({"accounts": results})))
}

async fn refresh_one(
    State(state): State<AppState>,
    Path((provider_id, account_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    if !config.quota.enabled {
        return Err(ApiError::bad(
            "quota_disabled",
            "quota tracking is disabled in config.quota.enabled",
        ));
    }
    let provider = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == provider_id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {provider_id}")))?;
    let account = provider
        .accounts
        .iter()
        .find(|a| a.id.as_str() == account_id)
        .ok_or_else(|| ApiError::not_found(format!("no account with id {account_id}")))?;
    let result = refresh_account(&state, provider, account).await;
    Ok(Json(json!({"accounts": [result]})))
}

async fn refresh_account(state: &AppState, provider: &Provider, account: &Account) -> Value {
    let result = fetch_and_store(state, provider, account).await;
    let cached = state
        .store
        .provider_quota_state(provider.id.as_str())
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.account_id == account.id));
    match result {
        Ok(entry) => account_row(provider, account, Some(&entry), None),
        Err(message) => account_row(provider, account, cached.as_ref(), Some(message)),
    }
}

async fn fetch_and_store(
    state: &AppState,
    provider: &Provider,
    account: &Account,
) -> Result<ProviderQuotaState, String> {
    let credential = if let Some(reference) = &account.secret_ref {
        state
            .vault
            .lock()
            .await
            .get(reference)
            .map_err(|e| format!("vault read failed: {e}"))?
    } else {
        None
    };
    if matches!(provider.auth, mr_core::AuthKind::OAuth { .. }) && account.oauth_ref.is_some() {
        return Err("OAuth quota refresh is not supported by the conductor yet".into());
    }
    let http = shared_http().map_err(|e| e.to_string())?;
    let mut context = QuotaFetchContext::new(provider.id.as_str(), provider.dialect, http)
        .with_account(mr_core::quota::registry::QuotaAccount::from_account(
            account,
        ))
        .with_base_url(provider.base_url.clone());
    if let Some(secret) = credential {
        context = context.with_credential(secret);
    }
    let quota = default_registry()
        .fetch(&context)
        .await
        .map_err(|e| e.to_string())?;
    let windows = quota_windows(provider, account, &quota);
    let source = quota
        .source
        .clone()
        .unwrap_or_else(|| "provider".to_owned());
    let payload = serde_json::to_string(&json!({"windows": windows, "details": quota.details}))
        .map_err(|e| format!("could not encode quota snapshot: {e}"))?;
    let row = ProviderQuotaState::new(provider.id.as_str(), account.id.clone(), source, payload);
    state
        .store
        .put_provider_quota_state(&row)
        .map_err(|e| format!("could not cache quota: {e}"))?;
    Ok(row)
}

fn quota_windows(
    provider: &Provider,
    account: &Account,
    quota: &UsageQuota,
) -> Vec<mr_core::quota::types::QuotaWindow> {
    let declared: &[mr_core::QuotaWindow] = if !account.quota.is_empty() {
        &account.quota
    } else {
        &provider.quota
    };
    if !declared.is_empty() {
        return declared
            .iter()
            .map(|window| {
                let mut normalized = mr_core::quota::types::QuotaWindow::from_quota(
                    mr_core::quota::types::WindowFamily::from_seconds(window.seconds.max(0) as u64),
                    quota,
                );
                normalized.display_name = window.name.clone();
                normalized
            })
            .collect();
    }
    vec![mr_core::quota::types::QuotaWindow::from_quota(
        mr_core::quota::types::WindowFamily::Session5h,
        quota,
    )]
}

fn account_row(
    provider: &Provider,
    account: &Account,
    cached: Option<&ProviderQuotaState>,
    error: Option<String>,
) -> Value {
    let parsed: Value = cached
        .and_then(|entry| serde_json::from_str(&entry.payload).ok())
        .unwrap_or_else(|| json!({}));
    let windows = parsed
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_else(|| {
            quota_windows(provider, account, &UsageQuota::unlimited())
                .iter()
                .map(|window| serde_json::to_value(window).unwrap_or(Value::Null))
                .collect()
        });
    let details = parsed.get("details").cloned().unwrap_or_else(|| json!([]));
    let windows = windows
        .into_iter()
        .map(|mut window| {
            if let Some(object) = window.as_object_mut() {
                object.insert("details".into(), details.clone());
            }
            window
        })
        .collect::<Vec<_>>();
    json!({
        "provider": provider.id.as_str(), "account": account.id.as_str(),
        "label": account.label.as_deref().or(account.identity.as_deref()),
        "fetched_at": cached.map(|entry| entry.fetched_at),
        "source": cached.map(|entry| entry.source.as_str()),
        "windows": windows,
        "error": error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::PanelAuth;
    use crate::state::{ServeDirs, StateFiles};
    use mr_core::config::Config;
    use mr_core::ids::Millis;
    use mr_core::store::KeyUsageRecord;
    use std::path::Path;

    /// The shared handle must be constructible and object-safe; that is the whole contract the
    /// registry needs from it, and it is what used to fail to compile.
    #[test]
    fn the_shared_transport_is_an_object_safe_quota_transport() {
        let http: Arc<dyn QuotaHttp> = match shared_http() {
            Ok(http) => http,
            // A machine with no usable TLS roots cannot build it; that is an environment fact, not
            // a defect in this wiring, and the assertion below is about the type, not the socket.
            Err(e) => panic!("the shared quota transport should build: {e}"),
        };
        let debugged = format!("{http:?}");
        assert!(
            debugged.contains("SharedQuotaHttp"),
            "unexpected Debug form: {debugged}"
        );
    }

    /// A state on a private temp directory, so no test touches the real installation.
    fn state_in(dir: &Path) -> AppState {
        let config_path = dir.join("config.json");
        let config = Config::default();
        config.save(&config_path).expect("write the initial config");
        AppState::new(
            config,
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

    /// Ask the usage endpoint for everything it has.
    async fn usage_body(state: AppState) -> Value {
        match get_usage(State(state), Query(UsageQuery::default())).await {
            Ok(Json(body)) => body,
            Err(e) => {
                use axum::response::IntoResponse;
                panic!(
                    "the usage endpoint must answer: {}",
                    e.into_response().status()
                );
            }
        }
    }

    /// Write one usage row into the ledger.
    fn record(
        state: &AppState,
        key: &str,
        tokens: (Option<u64>, Option<u64>),
        bytes: (Option<u64>, Option<u64>),
    ) {
        let record = KeyUsageRecord::new(key, Millis::now())
            .with_tokens(tokens.0, tokens.1)
            .with_bytes(bytes.0, bytes.1);
        state.store.record_key_usage(&record).expect("record");
    }

    /// A row the provider said nothing about is served with the router's estimate, and the
    /// estimate says it is one.
    #[tokio::test]
    async fn a_row_without_provider_tokens_is_served_with_an_estimate_and_the_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        // 480 bytes of request and 180 bytes of response: 120 and 45 tokens at four bytes each.
        record(&state, "key-a", (None, None), (Some(480), Some(180)));

        let body = usage_body(state).await;
        let rows = body["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row["estimated"], json!(true));
        assert_eq!(row["provider_reported"], json!(false));
        // The estimate is served…
        assert_eq!(row["prompt_tokens"], json!(120));
        assert_eq!(row["completion_tokens"], json!(45));
        // …and the provider's silence is still visible beside it.
        assert_eq!(row["provider_prompt_tokens"], Value::Null);
        assert_eq!(row["provider_completion_tokens"], Value::Null);
        // The measurements the estimate came from are served too.
        assert_eq!(row["request_bytes"], json!(480));
        assert_eq!(row["response_bytes"], json!(180));

        assert_eq!(body["totals"]["requests"], json!(1));
        assert_eq!(body["totals"]["prompt_tokens"], json!(120));
        assert_eq!(body["totals"]["completion_tokens"], json!(45));
        assert_eq!(body["totals"]["estimated_requests"], json!(1));
    }

    /// A row the provider counted is served with the provider's numbers and no estimate.
    #[tokio::test]
    async fn a_row_with_provider_tokens_is_served_with_those_and_no_estimate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        record(&state, "key-a", (Some(12), Some(7)), (Some(480), Some(180)));

        let body = usage_body(state).await;
        let rows = body["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row["estimated"], json!(false));
        assert_eq!(row["provider_reported"], json!(true));
        assert_eq!(row["prompt_tokens"], json!(12));
        assert_eq!(row["completion_tokens"], json!(7));
        assert_eq!(row["provider_prompt_tokens"], json!(12));
        assert_eq!(row["provider_completion_tokens"], json!(7));
        assert_eq!(body["totals"]["estimated_requests"], json!(0));
    }

    /// A row with neither provider counts nor measured bytes stays unknown: no number is invented
    /// for a request the relay never saw the end of.
    #[tokio::test]
    async fn a_row_with_neither_counts_nor_bytes_stays_unknown() {
        let dir = tempfile::tempdir().expect("temp dir");
        let state = state_in(dir.path());
        record(&state, "key-a", (None, None), (None, None));

        let body = usage_body(state).await;
        let rows = body["rows"].as_array().expect("rows");
        let row = &rows[0];
        assert_eq!(row["prompt_tokens"], Value::Null);
        assert_eq!(row["completion_tokens"], Value::Null);
        assert_eq!(row["estimated"], json!(false));
        assert_eq!(row["provider_reported"], json!(false));
    }
}
