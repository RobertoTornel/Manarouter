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
    let mut cached_tokens = 0u64;
    let mut cache_write_tokens = 0u64;
    let mut estimated_requests = 0u64;
    let mut gpu_requests = 0u64;
    let mut gpu_seconds = 0.0;
    let mut gpu_cost_usd = 0.0;
    let now = mr_core::ids::Millis::now();
    let month_ago = mr_core::ids::Millis(now.as_millis().saturating_sub(30 * 24 * 60 * 60 * 1000));
    if let Ok(totals) = state.store.gpu_usage_totals(month_ago, now) {
        gpu_requests = totals.requests;
        gpu_seconds = totals.gpu_seconds;
        gpu_cost_usd = totals.cost_usd;
    }

    for row in rows {
        let view = usage_row(&row);
        requests += 1;
        prompt_tokens += view.prompt_tokens.unwrap_or(0);
        completion_tokens += view.completion_tokens.unwrap_or(0);
        cached_tokens += view.cached_tokens.unwrap_or(0);
        cache_write_tokens += view.cache_write_tokens.unwrap_or(0);
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
            "cached_tokens": cached_tokens,
            "cache_write_tokens": cache_write_tokens,
            "estimated_requests": estimated_requests,
            "gpu_requests": gpu_requests,
            "gpu_seconds": gpu_seconds,
            "gpu_cost_usd": gpu_cost_usd,
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
    /// Prompt tokens the provider served from its cache, if it said so.
    cached_tokens: Option<u64>,
    /// Prompt tokens the provider wrote into its cache, if it said so.
    cache_write_tokens: Option<u64>,
    /// How long the provider spent generating, if the stream revealed it.
    generation_ms: Option<u64>,
    /// Output tokens per second, derived from the two facts above.
    ///
    /// `None` when either is missing: a rate needs a count *and* a window, and stating one from a
    /// guess is worse than stating nothing.
    tokens_per_second: Option<f64>,
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
            "cached_tokens": self.cached_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "generation_ms": self.generation_ms,
            "tokens_per_second": self.tokens_per_second,
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
        cached_tokens: row.cached_tokens,
        cache_write_tokens: row.cache_write_tokens,
        generation_ms: row.generation_ms,
        // The rate is computed from what the provider reported, never from the byte estimate: an
        // estimate has no timing, and dividing an estimate by a real window would look like a
        // measurement while being nothing of the kind.
        tokens_per_second: mr_core::usage::tokens_per_second(
            row.completion_tokens,
            row.generation_ms,
        ),
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
            // An account whose provider reports no allowance is not a quota row. Listing it with a
            // made-up `0/0` window reads as "exhausted" and sends the operator chasing a number
            // nobody measured. It stays hidden until it either gains a real fetcher or produces a
            // cached row.
            if entry.is_none() && !reports_quota(provider, account, &config.quota_probe) {
                continue;
            }
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
                .filter(|a| a.enabled && reports_quota(p, a, &config.quota_probe))
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

/// Whether this provider/account pair has a quota fetcher that can actually report an allowance.
///
/// The generic OpenAI-compatible reader sits in the fallback tier and claims everything, but it can
/// only answer when it has something to read: either the account declares a `quotaEndpoint`, or the
/// `quotaProbe` walk has a candidate list to try on the account's own origin. Everything else it
/// claims has no endpoint to read, which is exactly the "this service does not report quota" case —
/// and the panel must not invent a window for it.
///
/// The provider's base URL has to go into the look-up, not just its id and dialect: those two are
/// operator-chosen, and a vendor is recognised by the host its traffic goes to. A provider called
/// `commandcode` on the generic `open-ai` dialect matched no fetcher until the host claim existed, so
/// this function said "no quota here" about a service that reports one and the account never
/// appeared on the page.
///
/// The walk is asked rather than assumed: [`mr_core::quota::probe::walk_configured`] answers for the
/// account's own declaration, so an account that switches the walk off, or empties the list, stays
/// off this page exactly as it was before the walk existed.
fn reports_quota(
    provider: &Provider,
    account: &Account,
    probe: &mr_core::quota::probe::QuotaProbeConfig,
) -> bool {
    // A declared window is also quota: it has a limit and a reset, both of which the relay
    // enforces from its own ledger. Without this the account is invisible in the quota view
    // even though it is capped — the operator sees no row for a limit that does bind.
    if !provider.effective_windows(account).is_empty() {
        return true;
    }
    let quota_account = mr_core::quota::registry::QuotaAccount::from_account(account);
    let Some(fetcher) = default_registry().lookup(
        provider.dialect,
        provider.id.as_str(),
        Some(provider.base_url.as_str()),
        &quota_account,
    ) else {
        return false;
    };
    if fetcher.id() == mr_core::quota::modal::Modal::ID {
        return account
            .extra
            .get(mr_core::quota::modal::Modal::CONFIG_KEY)
            .is_some_and(|value| !value.is_null());
    }
    if fetcher.id() != mr_core::quota::openai_compat::OpenAiCompat::ID {
        return true;
    }
    account
        .extra
        .get(mr_core::quota::registry::QUOTA_ENDPOINT_KEY)
        .is_some_and(|value| !value.is_null())
        || mr_core::quota::probe::walk_configured(Some(probe), &account.extra)
}

async fn fetch_and_store(
    state: &AppState,
    provider: &Provider,
    account: &Account,
) -> Result<ProviderQuotaState, String> {
    // A key-based account hands over its stored secret; an OAuth account has no secret to hand
    // over, so the conductor mints the same short-lived token the relay would. Until this existed
    // every OAuth provider's quota panel was a dead end.
    let (credential, minted_context) = crate::oauth_token::credential_and_context(
        state,
        provider,
        account,
        crate::oauth_token::TokenUse::Quota,
    )
    .await?;
    let http = shared_http().map_err(|e| e.to_string())?;
    // The account may declare nothing about the upstream project while its credential names one,
    // and that is the usual case: the relay fills Antigravity's envelope from the *token*, so a
    // quota read that only consults the account asks Google about a different allowance and reports
    // a healthy window for an account whose every request is refused. The token's context fills the
    // gaps; an account that does declare a project still wins, because that one is the operator's.
    let mut quota_account = mr_core::quota::registry::QuotaAccount::from_account(account);
    for (key, value) in minted_context {
        quota_account.extra.entry(key).or_insert(value);
    }
    let mut context = QuotaFetchContext::new(provider.id.as_str(), provider.dialect, http)
        .with_account(quota_account)
        .with_base_url(provider.base_url.clone())
        // The candidate list is configuration, and it is read here rather than captured when the
        // refresh was queued: a refresh is the operator's way of testing a list they just saved.
        .with_quota_probe(state.current_config().await.quota_probe.clone());
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
    // The headline quota is stored beside the windows only when it is a reported USD balance.
    // The relay reads this object for the fine-tuning credit floor; a percentage window, an
    // unlimited plan and an unlabelled count are all "not a dollar balance" and are left out
    // rather than guessed at. Windows carry their own `unit`, so an old row still deserialises.
    let mut payload = json!({"windows": windows, "details": quota.details});
    if quota.unit.as_deref() == Some("USD") && quota.fraction_reported && !quota.unlimited {
        payload["credit"] = json!({
            "remaining": quota.remaining,
            "total": quota.total,
            "used": quota.used,
            "unit": "USD",
            "unlimited": false,
            "fraction_reported": true,
        });
    }
    let payload = serde_json::to_string(&payload)
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
    // Nothing is declared, so the only fact left is the one the upstream just reported: *when* the
    // window resets. Classify from that rather than assuming a session window. Command Code reports
    // one dollar pool per billing period, and rendering a pool that resets in three weeks as `5h`
    // would be exactly the label this module exists to avoid — a window is a duration, not a name.
    vec![mr_core::quota::types::QuotaWindow::from_quota(
        observed_family(quota),
        quota,
    )]
}

/// The window family of a quota that declared no length, read off its reset instant.
///
/// The distance to the reset is a *lower bound* on the real window length — a thirty-day window
/// seen with five days left reads as weekly — so this can only ever classify a window shorter than
/// it is, never longer, which is the honest direction to be wrong in. With no instant, or one
/// already in the past, there is nothing to classify by and the session-window assumption stands.
fn observed_family(quota: &UsageQuota) -> mr_core::quota::types::WindowFamily {
    use mr_core::quota::types::WindowFamily;
    let Some(reset_at) = quota.reset_at else {
        return WindowFamily::Session5h;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0);
    let remaining_secs = reset_at.saturating_sub(now_ms) / 1000;
    if remaining_secs <= 0 {
        return WindowFamily::Session5h;
    }
    WindowFamily::from_seconds(remaining_secs as u64)
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
    // No cached reading and no error means this account has not been fetched yet. The row says so
    // by carrying no windows at all: a fabricated `0/0` window renders as an exhausted account and
    // an operator acts on a number nobody measured.
    let windows = parsed
        .get("windows")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
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
    /// A reported row with a real window yields a rate; without one it yields nothing, because a
    /// rate needs both a count and the time the count took.
    #[test]
    fn a_reported_row_yields_its_cache_split_and_rate() {
        let row = KeyUsageRow {
            id: "row-1".to_owned(),
            client_key_id: "key-1".to_owned(),
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            occurred_at: mr_core::ids::Millis(0),
            created_at: mr_core::ids::Millis(0),
            request_bytes: Some(400),
            response_bytes: Some(800),
            cached_tokens: Some(80),
            cache_write_tokens: Some(15),
            generation_ms: Some(2000),
        };
        let view = usage_row(&row);
        assert_eq!(view.cached_tokens, Some(80));
        assert_eq!(view.cache_write_tokens, Some(15));
        assert_eq!(view.generation_ms, Some(2000));
        // 50 tokens in 2 seconds is 25 per second.
        assert_eq!(view.tokens_per_second, Some(25.0));
        // The provider reported both counts, so nothing is estimated.
        assert!(!view.estimated);
        assert!(view.provider_reported);

        let json = view.json();
        assert_eq!(json["cached_tokens"], 80);
        assert_eq!(json["cache_write_tokens"], 15);
        assert_eq!(json["tokens_per_second"], 25.0);
    }

    /// No window means no rate — not a zero, and not a number derived from the byte estimate.
    #[test]
    fn a_row_without_a_window_states_no_rate() {
        let row = KeyUsageRow {
            id: "row-2".to_owned(),
            client_key_id: "key-1".to_owned(),
            prompt_tokens: Some(10),
            completion_tokens: Some(4),
            occurred_at: mr_core::ids::Millis(0),
            created_at: mr_core::ids::Millis(0),
            request_bytes: None,
            response_bytes: Some(16),
            cached_tokens: None,
            cache_write_tokens: None,
            generation_ms: None,
        };
        let view = usage_row(&row);
        assert_eq!(view.tokens_per_second, None);
        assert_eq!(view.cached_tokens, None);
        assert_eq!(view.json()["tokens_per_second"], serde_json::Value::Null);
    }

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

    /// The operator's shape, and the failure it used to cause.
    ///
    /// A Command Code connection is naturally configured with a provider id the operator chose
    /// (`commandcode`) on the generic `open-ai` dialect its OpenAI-compatible surface speaks, which
    /// matches no fetcher id and no dedicated dialect. The look-up therefore used to stop at the
    /// generic reader, which can only answer when the account declares a `quotaEndpoint` — so this
    /// function said "this service reports no quota" about a service that reports a credit pool and
    /// two rolling windows, and the row was dropped from `GET /api/quota` rather than read.
    #[test]
    fn a_provider_recognised_by_its_host_reports_quota_and_the_rest_do_not() {
        let account = Account::new("work");
        let walk = mr_core::quota::probe::QuotaProbeConfig::default();

        // Command Code, on the exact base URL the operator's config carries.
        let command_code = Provider::new(
            "commandcode",
            mr_core::Dialect::OpenAi,
            "https://api.commandcode.ai/provider/v1",
        );
        assert!(reports_quota(&command_code, &account, &walk));

        // A made-up id on somebody else's host is a service with no dedicated reader. What decides
        // whether the page offers it is whether the walk has anything to try there.
        let unknown = Provider::new(
            "some-openai-clone",
            mr_core::Dialect::OpenAi,
            "https://api.openai.com/v1",
        );
        assert!(reports_quota(&unknown, &account, &walk));

        // Switch the walk off, or empty its list, and nothing is offered: byte for byte the
        // behaviour the panel had before the walk existed.
        let off = mr_core::quota::probe::QuotaProbeConfig {
            enabled: false,
            ..mr_core::quota::probe::QuotaProbeConfig::default()
        };
        assert!(!reports_quota(&unknown, &account, &off));
        let empty = mr_core::quota::probe::QuotaProbeConfig {
            paths: Vec::new(),
            ..mr_core::quota::probe::QuotaProbeConfig::default()
        };
        assert!(!reports_quota(&unknown, &account, &empty));

        // The pre-existing escape hatch still works, and it does not need the walk: a declared
        // endpoint is read even with the section switched off.
        let mut declared = Account::new("work");
        declared.extra.insert(
            mr_core::quota::registry::QUOTA_ENDPOINT_KEY.to_owned(),
            json!("https://api.openai.com/v1/credits"),
        );
        assert!(reports_quota(&unknown, &declared, &off));

        // An account that declares its own list is walked with the section off only if it also
        // switches the walk on for itself: a bare list inherits `enabled`, which is how the
        // documented overlay works.
        let mut inherited = Account::new("work");
        inherited.extra.insert(
            mr_core::quota::probe::QUOTA_PROBE_KEY.to_owned(),
            json!(["/v1/balance"]),
        );
        assert!(!reports_quota(&unknown, &inherited, &off));
        let mut own = Account::new("work");
        own.extra.insert(
            mr_core::quota::probe::QUOTA_PROBE_KEY.to_owned(),
            json!({ "enabled": true, "paths": ["/v1/balance"] }),
        );
        assert!(reports_quota(&unknown, &own, &off));

        // A declaration that cannot be read is offered anyway, so the refresh reports the mistake
        // instead of the page silently dropping the account.
        let mut broken = Account::new("work");
        broken.extra.insert(
            mr_core::quota::probe::QUOTA_PROBE_KEY.to_owned(),
            json!("https://api.openai.com/v1/balance"),
        );
        assert!(reports_quota(&unknown, &broken, &off));
    }

    /// The label on the one window an undeclared quota produces.
    ///
    /// Command Code reports a dollar pool per billing period with its reset twenty days out. That
    /// used to be labelled `5h` for every provider that declares no window, which told the operator
    /// a budget they can spend all month was a session allowance.
    #[test]
    fn an_undeclared_window_is_classified_by_when_it_resets() {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_millis() as i64)
            .expect("clock after the epoch");

        // Command Code's real shape: a pool that resets in three weeks is a billing period.
        let billing = UsageQuota::from_counts(39.03, 70.10, Some(now_ms + 20 * 24 * 3_600_000));
        assert_eq!(observed_family(&billing).label(), "30d");

        // A session window still reads as one, which is what the old assumption was right about.
        let session = UsageQuota::from_counts(1.0, 5.0, Some(now_ms + 2 * 3_600_000));
        assert_eq!(observed_family(&session).label(), "5h");

        // Nothing to classify by: no instant, or one already past, keeps the documented assumption.
        let silent = UsageQuota::from_counts(0.0, 0.0, None);
        assert_eq!(observed_family(&silent).label(), "5h");
        let past = UsageQuota::from_counts(1.0, 5.0, Some(now_ms - 3_600_000));
        assert_eq!(observed_family(&past).label(), "5h");
    }
}
