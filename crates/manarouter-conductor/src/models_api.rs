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
use http_body_util::{BodyExt, Full, Limited};
use hyper::Request;
use mr_core::model::{PricingPlan, PricingSource};
use serde_json::{json, Value};

use crate::api::{sync_client, ApiError, ApiResult, JsonBody};
use crate::state::AppState;

/// Cap on the upstream body read by `/api/models/test`, so a broken or hostile provider cannot
/// exhaust conductor memory by answering with an enormous (or endless) body.
const MAX_TEST_RESPONSE_BYTES: usize = 1 << 20;

/// How long a test may take before the conductor gives up on the relay.
///
/// It has to cover a reasoning model, not just a round trip: a thinking model can spend ninety
/// seconds before its first token, and that is a *correct* answer, not a broken relay. The old
/// thirty-second budget turned such a model into a red "the relay is not answering" — the route
/// worked the whole time. Two minutes leaves headroom for one slow think without letting a wedged
/// socket hang the page forever.
const MODEL_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The sentence for a test that ran out of time. It must not blame the relay's liveness: time is
/// what ran out, and a slow model is the likelier reason.
fn timeout_message(timeout: std::time::Duration) -> String {
    format!(
        "the model test got no answer within {} s; a reasoning model can be this slow, so retry, \
         or check the relay log for the provider's own error",
        timeout.as_secs()
    )
}

/// The sentence for a transport failure. A refusal at connect time is the only failure that
/// means the relay is not running; anything else (a reset mid-request, a malformed URL) is named
/// as itself, because the old text blamed the process for every error and hid the cause.
fn transport_message(url: &str, refused: bool, error: &str) -> String {
    if refused {
        format!("the relay is not answering on {url}; start it and try again")
    } else {
        format!("the model test could not reach the relay: {error}")
    }
}

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
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    // Deserialize through the envelope rather than axum's `Json` extractor: a malformed body would
    // otherwise answer 422 `text/plain`, which the panel cannot parse and shows as a bare status.
    let request: ModelTestRequest = serde_json::from_value(body).map_err(|e| {
        ApiError::bad(
            "invalid_body",
            format!("the request could not be read: {e}"),
        )
    })?;
    let config = state.current_config().await;
    let provider = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == request.provider)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {}", request.provider)))?;
    provider
        .accounts
        .iter()
        .find(|a| a.enabled)
        .ok_or_else(|| ApiError::bad("no_account", "the provider has no enabled account"))?;
    provider
        .models
        .iter()
        .find(|m| m.id == request.model)
        .ok_or_else(|| {
            ApiError::bad(
                "unknown_model",
                "the model is not configured for this provider",
            )
        })?;
    // The test runs on the relay, not here. This window used to speak the OpenAI shape itself,
    // which meant it could only ever test OpenAI-shaped providers and refused every other dialect
    // — the exact routes an operator most wants to check (Codex, Antigravity, Command Code). The
    // relay's loopback probe runs the ordinary dispatch path instead: same routing table, same
    // dialect translation, same account selection and cooldowns a real request gets.
    let relay = format!("http://127.0.0.1:{}/control/probe", config.relay.port);
    let payload = serde_json::to_vec(&json!({
        "model": format!("{}/{}", request.provider, request.model),
        "prompt": request.prompt,
    }))
    .map_err(|e| ApiError::internal(format!("the test request could not be encoded: {e}")))?;
    // The failure text comes from `send_json`, which knows *why* the call failed. Replacing it
    // here is what used to turn every timeout into a claim that the relay was down.
    let (status, response_body) = send_json(
        &relay,
        &[("content-type".to_owned(), "application/json".to_owned())],
        &payload,
        MODEL_TEST_TIMEOUT,
    )
    .await?;
    let parsed: Value = serde_json::from_slice(&response_body)
        .map_err(|_| ApiError::bad_gateway("the model test returned invalid JSON"))?;
    if !status.is_success() {
        let message = refusal_message(&parsed).unwrap_or("the relay refused the test");
        return Err(probe_failure(status, message));
    }
    let answer = tested_answer(&parsed)?;
    Ok(Json(
        json!({"provider": request.provider, "model": request.model, "ok": true, "response": answer}),
    ))
}

/// The sentence a refusal carries, whichever envelope it arrived in.
///
/// The relay hands its client's answer back with the upstream's body untouched, and upstreams do
/// not agree on an envelope: OpenAI and Anthropic nest the sentence under `error.message`, fal.ai
/// answers `{"detail": "User is locked. Reason: Exhausted balance. …"}`, a few send `error` as the
/// string itself. Reading only `error.message` turned fal's exhausted-balance 403 into the panel's
/// useless "the relay answered 403 Forbidden: the relay refused the test" while the real sentence
/// sat in the relay's own log. The shapes read here are the same ones the relay's `refusal_text`
/// reads when it classifies the refusal, so the panel and the log say the same thing.
fn refusal_message(parsed: &Value) -> Option<&str> {
    let error = parsed.get("error").unwrap_or(parsed);
    [
        error.get("message"),
        error.get("detail"),
        parsed.get("detail"),
        parsed.get("message"),
        Some(error),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .next()
}

/// The answer a successful probe actually delivered, or a failure that says why there is none.
///
/// A `200` from the relay only means the route was reached: a model can finish with an empty
/// answer, and reporting `ok: true` with a blank string is the worst of both worlds — the operator
/// sees a green result and no text, and nothing says whether the route is broken or the model simply
/// had nothing to say. The finish reason is carried into the message because it is the diagnosis:
/// `length` means the allowance ran out (a reasoning model can spend all of it thinking), `stop`
/// means the model genuinely answered with nothing.
fn tested_answer(parsed: &Value) -> ApiResult<Value> {
    // A Responses-shaped answer, which is what a responses-only provider (Codex) returns: the probe
    // speaks the provider's own surface, so the answer arrives in that surface's shape. `output_text`
    // is the flattened text the API itself provides; the `output` walk is its fallback.
    if parsed.get("choices").is_none() && parsed.get("output").is_some() {
        if let Some(text) = parsed
            .get("output_text")
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
        {
            return Ok(Value::String(text.to_owned()));
        }
        let texts: Vec<&str> = parsed
            .get("output")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("content"))
                    .filter_map(Value::as_array)
                    .flat_map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text"))
                            .filter_map(Value::as_str)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let text = texts.join("");
        if text.trim().is_empty() {
            let status = parsed
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            return Err(ApiError::bad_gateway(format!(
                "the model answered with no text (status: {status})"
            )));
        }
        return Ok(Value::String(text));
    }
    let choice = parsed.get("choices").and_then(|c| c.get(0));
    let message = choice.and_then(|c| c.get("message"));
    // A tool call is a real answer that carries no prose, so an empty `content` next to one is not
    // a failure.
    let called_a_tool = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .is_some_and(|calls| !calls.is_empty());
    let content = message
        .map(|m| &m["content"])
        .cloned()
        .unwrap_or(Value::Null);
    if !called_a_tool && !has_text(&content) {
        let reason = choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(ApiError::bad_gateway(format!(
            "the model answered with no text (finish reason: {reason})"
        )));
    }
    Ok(content)
}

/// Whether a Chat Completions `content` carries any text.
///
/// The field is a plain string in the ordinary case and a list of typed parts when an image or a
/// refusal rides along, so both shapes are read; anything else counts as no text.
fn has_text(content: &Value) -> bool {
    match content {
        Value::String(text) => !text.trim().is_empty(),
        Value::Array(parts) => parts.iter().any(|part| {
            part.get("text")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        }),
        _ => false,
    }
}

/// The failure to hand back when the probe's route refused.
///
/// The relay's status is passed through, with one exception: a `401`. On *this* API a `401` has
/// exactly one meaning — "the panel session is gone" — and `panel/api.js` acts on it globally by
/// dropping the operator to the sign-in screen. A provider that rejects a stored credential is not
/// a dead session: re-entering the panel password cannot fix it, and the bounce hides the model
/// list along with the verdict the operator was reading. So the refusal is reported as `502`
/// (the gateway did answer; its upstream refused) and the provider's own `401` stays in the
/// message, which is where the truth belongs. Everything else is passed through unchanged: a `429`
/// is a rate limit and a `403` a plan restriction, and the panel should say so with the route's
/// own words.
fn probe_failure(upstream: StatusCode, message: &str) -> ApiError {
    let status = if upstream == StatusCode::UNAUTHORIZED {
        StatusCode::BAD_GATEWAY
    } else {
        upstream
    };
    ApiError::upstream(status, format!("the relay answered {upstream}: {message}"))
}

async fn send_json(
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: std::time::Duration,
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
    let response = tokio::time::timeout(timeout, client.request(request))
        .await
        .map_err(|_| ApiError::bad_gateway(timeout_message(timeout)))?
        .map_err(|e| {
            ApiError::bad_gateway(transport_message(url, e.is_connect(), &e.to_string()))
        })?;
    let status = response.status();
    let body = tokio::time::timeout(
        timeout,
        Limited::new(response.into_body(), MAX_TEST_RESPONSE_BYTES).collect(),
    )
    .await
    .map_err(|_| ApiError::bad_gateway(timeout_message(timeout)))?
    .map_err(|e| ApiError::bad_gateway(format!("could not read model-test response: {e}")))?
    .to_bytes();
    Ok((status, body))
}

async fn sync_pricing(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    // `commit` replaces the whole config, so hold the mutation lock across the read-modify-write;
    // otherwise a concurrent panel edit landing between the read and the commit is reverted.
    let _mutation = state.mutation_lock.lock().await;
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
    state.commit_locked(config).await?;
    Ok(Json(
        json!({"updated": updated, "manual_overrides_preserved": true, "preserved": preserved, "message": format!("Updated {updated} models; preserved {preserved} manual overrides.")}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    /// A provider's refusal must reach the panel as a refusal, not as a dead session.
    ///
    /// `panel/api.js` treats any `401` as "your session died" and swaps the whole UI for the sign-in
    /// screen, so a provider that answers `401` to a generation request used to log the operator out
    /// and bury the model test's real verdict — the reason the panel said "Your session has expired"
    /// on a perfectly valid session. The status is translated to `502`; the provider's own status and
    /// words stay in the message, where they are actionable.
    #[tokio::test]
    async fn a_provider_refusal_keeps_its_verdict_but_never_borrows_the_sessions_401() {
        let refused = probe_failure(StatusCode::UNAUTHORIZED, "the key was revoked");
        let response = refused.into_response();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            body.contains("the relay answered 401 Unauthorized"),
            "{body}"
        );
        assert!(body.contains("the key was revoked"), "{body}");
        assert!(body.contains("\"code\":\"upstream\""), "{body}");

        // Everything else keeps the route's own status: a rate limit reading as a rate limit is the
        // point of passing the relay's answer through at all.
        for (upstream, expected) in [
            (StatusCode::TOO_MANY_REQUESTS, StatusCode::TOO_MANY_REQUESTS),
            (StatusCode::FORBIDDEN, StatusCode::FORBIDDEN),
            (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
            (StatusCode::PAYMENT_REQUIRED, StatusCode::PAYMENT_REQUIRED),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        ] {
            let response = probe_failure(upstream, "x").into_response();
            assert_eq!(response.status(), expected, "{upstream}");
        }
    }

    /// A green test with no text is worse than a red one: it says "the route works" and shows nothing.
    ///
    /// Antigravity's `gemini-3.8-flash-tiered` thinks by default, and the probe used to cap the answer
    /// at 32 tokens, so the thoughts consumed the allowance and the route finished `length` with an
    /// empty `content`. The panel showed a successful test with a blank answer, which reads as a broken
    /// model rather than a starved one. The empty answer is now a failure whose message carries the
    /// finish reason — `length` for that case — while a tool call, which is a real answer with no prose,
    /// still counts as an answer.
    #[tokio::test]
    async fn an_empty_answer_is_a_failed_test_that_names_the_finish_reason() {
        let starved = json!({"choices": [{"message": {"content": ""}, "finish_reason": "length"}]});
        let refusal = tested_answer(&starved)
            .expect_err("an empty answer is refused")
            .into_response();
        assert_eq!(refusal.status(), StatusCode::BAD_GATEWAY);
        let body = refusal.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("finish reason: length"), "{body}");

        // A null content is the same failure under another spelling.
        let absent =
            json!({"choices": [{"message": {"content": Value::Null}, "finish_reason": "stop"}]});
        assert!(tested_answer(&absent).is_err());

        // Whitespace is not text, in either shape the field takes.
        assert!(tested_answer(&json!({"choices": [{"message": {"content": "  "}}]})).is_err());
        assert!(tested_answer(
            &json!({"choices": [{"message": {"content": [{"type": "text", "text": ""}]}}]})
        )
        .is_err());

        // A real answer passes through untouched, as does a tool call with no prose.
        let answered =
            json!({"choices": [{"message": {"content": "yes"}, "finish_reason": "stop"}]});
        assert_eq!(
            tested_answer(&answered).ok().expect("an answer passes"),
            json!("yes")
        );
        let parts =
            json!({"choices": [{"message": {"content": [{"type": "text", "text": "yes"}]}}]});
        assert!(tested_answer(&parts).is_ok());
        let tool = json!({"choices": [{"message": {"content": "", "tool_calls": [{"id": "call_0"}]}, "finish_reason": "tool_calls"}]});
        assert!(tested_answer(&tool).is_ok());
    }

    #[test]
    fn a_timeout_names_time_running_out_not_a_dead_relay() {
        // The old text claimed the relay was down after thirty seconds, which sent an operator
        // to restart a process that was working fine. A timeout must say what actually expired.
        let message = timeout_message(std::time::Duration::from_secs(120));
        assert!(message.contains("120 s"), "{message}");
        assert!(message.contains("reasoning model"), "{message}");
        assert!(!message.contains("not answering"), "{message}");
    }

    /// Every refusal envelope an upstream uses must reach the panel in its own words.
    ///
    /// fal.ai answers an exhausted balance as `{"detail": "User is locked. Reason: Exhausted
    /// balance. …"}` — no `error.message` anywhere. Reading only that one field turned the refusal
    /// into "the relay refused the test" in the panel while the relay's log held the real sentence,
    /// and the operator (rightly) read it as the relay failing rather than the account being empty.
    #[test]
    fn a_refusal_in_any_envelope_reaches_the_panel_in_its_own_words() {
        let fal = json!({"detail": "User is locked. Reason: Exhausted balance. Top up your balance at fal.ai/dashboard/billing."});
        assert_eq!(
            refusal_message(&fal),
            Some("User is locked. Reason: Exhausted balance. Top up your balance at fal.ai/dashboard/billing.")
        );

        // The shapes the relay's own classifier reads, in the same order.
        assert_eq!(
            refusal_message(&json!({"error": {"message": "openai shape"}})),
            Some("openai shape")
        );
        assert_eq!(
            refusal_message(&json!({"error": {"detail": "nested detail"}})),
            Some("nested detail")
        );
        assert_eq!(
            refusal_message(&json!({"message": "top-level message"})),
            Some("top-level message")
        );
        assert_eq!(
            refusal_message(&json!({"error": "error as a string"})),
            Some("error as a string")
        );
        assert_eq!(refusal_message(&json!({"error": {"code": 403}})), None);
        assert_eq!(refusal_message(&json!({})), None);
    }

    /// A responses-only provider answers in the Responses shape, and its text must be read there.
    #[tokio::test]
    async fn a_responses_shaped_answer_is_read_from_output() {
        let answered = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "OK"}]
            }]
        });
        assert_eq!(
            tested_answer(&answered).ok().expect("an answer passes"),
            json!("OK")
        );

        let flat = json!({"status": "completed", "output_text": "OK", "output": []});
        assert_eq!(
            tested_answer(&flat).ok().expect("an answer passes"),
            json!("OK")
        );

        let silent = json!({"status": "completed", "output": []});
        let refusal = tested_answer(&silent)
            .expect_err("no text is a failed test")
            .into_response();
        assert_eq!(refusal.status(), StatusCode::BAD_GATEWAY);
        let body = String::from_utf8(
            refusal
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains("status: completed"), "{body}");
    }

    #[test]
    fn only_a_refused_connection_claims_the_relay_is_down() {
        let url = "http://127.0.0.1:20900/control/probe";
        let refused = transport_message(url, true, "connect error: Connection refused");
        assert!(refused.contains("is not answering"), "{refused}");
        assert!(refused.contains(url), "{refused}");

        // Anything else is reported as itself: a reset mid-request means the relay accepted the
        // connection and then failed, which is a different problem with a different fix.
        let reset = transport_message(url, false, "connection reset by peer");
        assert!(reset.contains("connection reset by peer"), "{reset}");
        assert!(!reset.contains("not answering"), "{reset}");
    }
}
