//! Model endpoints: testing a model, and syncing its price table.
//!
//! Task E2. Its routes are merged into the guarded router by `api::guarded_router`, so this
//! module owns its own file and nobody has to edit the shared route table to add one.
//!
//! `routes()` returns an empty router until the handlers land, which keeps the module list and the
//! merge site in `api.rs` stable while the handlers are written.

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use mr_core::model::{PricingPlan, PricingSource};
use serde_json::{json, Value};

use crate::api::{sync_client, ApiError, ApiResult};
use crate::state::AppState;

/// The routes this module contributes, merged into the authenticated API surface.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/pricing/sync", post(sync_pricing))
        .route("/api/models/test", post(test_model))
}

/// Safe config roundtrip proving E5 fields remain readable/writable; the canonical config API is
/// deliberately generic and preserves fields defined by mr-core rather than duplicating DTOs.

#[derive(Debug, serde::Deserialize)]
struct ModelTestRequest {
    provider: String,
    model: String,
    #[serde(default = "default_test_prompt")]
    prompt: String,
}
fn default_test_prompt() -> String {
    "Reply with exactly: OK".into()
}

async fn test_model(
    State(state): State<AppState>,
    Json(request): Json<ModelTestRequest>,
) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    let provider = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == request.provider)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {}", request.provider)))?;
    let account = provider
        .accounts
        .iter()
        .find(|a| a.enabled)
        .ok_or_else(|| ApiError::bad("no_account", "the provider has no enabled account"))?;
    let model = provider
        .models
        .iter()
        .find(|m| m.id == request.model)
        .ok_or_else(|| {
            ApiError::bad(
                "unknown_model",
                "the model is not configured for this provider",
            )
        })?;
    let secret = if let Some(reference) = &account.secret_ref {
        state.vault.lock().await.get(reference).map_err(|e| {
            ApiError::internal(format!("could not read the provider credential: {e}"))
        })?
    } else {
        None
    };
    let mut url = format!(
        "{}/chat/completions",
        provider.base_url.trim_end_matches('/')
    );
    let mut headers = vec![("content-type".to_owned(), "application/json".to_owned())];
    match &provider.auth {
        mr_core::AuthKind::Bearer => {
            if let Some(secret) = secret.as_ref() {
                headers.push(("authorization".into(), format!("Bearer {secret}")));
            }
        }
        mr_core::AuthKind::ApiKeyHeader { header } => {
            if let Some(secret) = secret.as_ref() {
                headers.push((header.clone(), secret.clone()));
            }
        }
        mr_core::AuthKind::Header { name, prefix } => {
            if let Some(secret) = secret.as_ref() {
                headers.push((name.clone(), format!("{prefix}{secret}")));
            }
        }
        mr_core::AuthKind::QueryParam { param } => {
            if let Some(secret) = secret.as_ref() {
                url = format!("{url}?{param}={}", urlencoding::encode(secret));
            }
        }
        mr_core::AuthKind::OAuth { .. } => {
            return Err(ApiError::bad(
                "oauth_unsupported",
                "model test does not yet mint OAuth credentials",
            ))
        }
    }
    for (name, value) in &provider.headers {
        headers.push((name.clone(), value.clone()));
    }
    let body = serde_json::to_vec(&json!({
        "model": model.id,
        "messages": [{"role":"user", "content": request.prompt}],
        "max_tokens": 8,
    }))
    .map_err(|e| ApiError::internal(format!("could not encode test request: {e}")))?;
    let (status, response_body) = send_json(&url, &headers, &body).await?;
    if !status.is_success() {
        return Err(ApiError::bad_gateway(format!(
            "upstream model test returned HTTP {status}"
        )));
    }
    let parsed: Value = serde_json::from_slice(&response_body)
        .map_err(|_| ApiError::bad_gateway("upstream model test returned invalid JSON"))?;
    Ok(Json(
        json!({"provider": request.provider, "model": request.model, "ok": true, "response": parsed.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message")).and_then(|m| m.get("content")).cloned().unwrap_or(Value::Null)}),
    ))
}

async fn send_json(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> ApiResult<(StatusCode, Bytes)> {
    let client = sync_client().map_err(ApiError::internal)?;
    let mut builder = Request::builder()
        .method(if body.is_empty() {
            http::Method::GET
        } else {
            http::Method::POST
        })
        .uri(url);
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(Full::new(Bytes::copy_from_slice(body)))
        .map_err(|e| ApiError::internal(format!("could not build model request: {e}")))?;
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(15), client.request(request))
            .await
            .map_err(|_| ApiError::bad_gateway("model-test upstream timed out"))?
            .map_err(|e| {
                ApiError::bad_gateway(format!("model test could not reach upstream: {e}"))
            })?;
    let status = response.status();
    let body = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        response.into_body().collect(),
    )
    .await
    .map_err(|_| ApiError::bad_gateway("model-test response timed out"))?
    .map_err(|e| ApiError::bad_gateway(format!("could not read model-test response: {e}")))?
    .to_bytes();
    Ok((status, body))
}

async fn sync_pricing(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let bundled: Value = serde_json::from_str(include_str!("../../../catalog/pricing.json"))
        .map_err(|e| ApiError::internal(format!("bundled pricing table is invalid: {e}")))?;
    let table = bundled
        .get("models")
        .and_then(Value::as_object)
        .ok_or_else(|| ApiError::internal("bundled pricing table has no models map"))?;
    let mut updated = 0usize;
    let mut preserved = 0usize;
    for provider in &mut config.providers {
        for model in &mut provider.models {
            if model
                .pricing
                .as_ref()
                .is_some_and(|p| p.source == PricingSource::Manual)
            {
                preserved += 1;
                continue;
            }
            let qualified = format!("{}/{}", provider.id.as_str(), model.id);
            let candidate = table.get(&qualified).or_else(|| table.get(&model.id));
            let Some(candidate) = candidate else {
                continue;
            };
            let input = candidate.get("input").and_then(Value::as_f64);
            let output = candidate.get("output").and_then(Value::as_f64);
            if input.is_none() && output.is_none() {
                continue;
            }
            let plan = PricingPlan {
                input_per_mtok: input,
                output_per_mtok: output,
                cache_read_per_mtok: candidate.get("cache_read").and_then(Value::as_f64),
                cache_write_per_mtok: candidate.get("cache_write").and_then(Value::as_f64),
                source: PricingSource::Bundled,
            };
            model.input_price_per_mtok = input;
            model.output_price_per_mtok = output;
            model.pricing = Some(plan);
            updated += 1;
        }
    }
    state.commit(config).await?;
    Ok(Json(
        json!({"updated": updated, "manual_overrides_preserved": true, "preserved": preserved, "message": format!("Updated {updated} models; preserved {preserved} manual overrides.")}),
    ))
}
