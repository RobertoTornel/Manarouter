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

use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::{self, StreamExt};
use mr_core::error::Result as CoreResult;
use mr_core::quota::http::{HyperQuotaHttp, QuotaHttp, QuotaRequest, QuotaResponse};
use mr_core::quota::registry::{default_registry, QuotaFetchContext};
use mr_core::quota::types::UsageQuota;
use mr_core::store::ProviderQuotaState;
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
}
