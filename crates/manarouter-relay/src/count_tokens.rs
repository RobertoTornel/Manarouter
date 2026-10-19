//! The Anthropic token-counting route, answered locally when the upstream cannot count.
//!
//! # What this is for
//!
//! `POST /v1/messages/count_tokens` asks a provider how many tokens a request would cost. Only an
//! Anthropic-shaped upstream can answer that question: the endpoint exists in Anthropic's API and
//! nowhere else, and asking any other provider would mean asking for a *generation* instead — a
//! billed answer, in the wrong shape, to a question that was not a question.
//!
//! So the relay splits the route in two:
//!
//! * an **Anthropic** step is asked the real question, on the upstream's own
//!   `/v1/messages/count_tokens` path, with the client's body forwarded untranslated, and the
//!   upstream's answer is returned as it arrived;
//! * any **other** step is not called at all. The relay answers locally with an estimate, and says
//!   so in the `x-manarouter-count-tokens-estimate: true` response header.
//!
//! # What a combo does
//!
//! The rule is per step, and the steps are tried in order. A combo whose first step speaks
//! Anthropic is asked upstream; if that step fails, the next one is tried in the ordinary way, and
//! if that one is not an Anthropic step it answers with an estimate instead. A combo whose first
//! step does not speak Anthropic answers with an estimate without contacting anybody.
//!
//! # The estimate is an estimate
//!
//! The number is a **character-based approximation, not the provider's tokenizer**. It sums the
//! UTF-8 byte length of every string in the request body, skips the top-level `model` (the model
//! name says which model to count for; it is not part of the input), and divides by four — the same
//! four-characters-per-token approximation the relay already uses to size a quota reservation
//! ([`crate::quota::estimate_prompt_tokens`]) — rounding **up**, and never answering less than
//! one. A body that is not JSON falls back to its raw length, divided by four the same way.
//!
//! The estimate is deliberately coarse. It exists so that a client asking "how big is this?" gets
//! an answer of the right shape rather than a billed generation. A client that needs the
//! provider's exact number has to be pointed at an Anthropic upstream.

use axum::body::Body as AxumBody;
use axum::http::{header::CONTENT_TYPE, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::Value;

use crate::quota::CHARS_PER_TOKEN;

/// The header that marks a count the relay estimated rather than asked for.
///
/// A forwarded upstream count does **not** carry it: that number came from the provider's own
/// tokenizer, and a client that needs to know whether it can trust the number can look.
pub const ESTIMATE_HEADER: &str = "x-manarouter-count-tokens-estimate";

/// The value the estimate header carries.
pub const ESTIMATE_HEADER_VALUE: &str = "true";

/// The estimated number of input tokens in a request body.
///
/// The count is an approximation — four bytes per token, rounded up, at least one — and it is not
/// the provider's tokenizer. See the module documentation for what that means.
pub fn estimate_input_tokens(body: &[u8]) -> u64 {
    let total = match serde_json::from_slice::<Value>(body) {
        // The top level is an object in every well-formed request, and its `model` key is the one
        // string that is not part of the input.
        Ok(Value::Object(map)) => {
            let mut total = 0u64;
            for (key, value) in &map {
                if key == "model" {
                    continue;
                }
                sum_string_bytes(value, &mut total);
            }
            total
        }
        Ok(value) => {
            let mut total = 0u64;
            sum_string_bytes(&value, &mut total);
            total
        }
        // A body that is not JSON has no structure to walk, so its raw length is the only measure
        // of it that the relay has.
        Err(_) => return bytes_to_tokens(body.len() as u64),
    };
    bytes_to_tokens(total)
}

/// The body of an estimated count: exactly `{"input_tokens": N}`.
///
/// No other field, so a client that parses it as an Anthropic response needs no adapter. The
/// header, not the body, says that the number was estimated.
pub fn estimate_payload(body: &[u8]) -> String {
    serde_json::json!({ "input_tokens": estimate_input_tokens(body) }).to_string()
}

/// The answer to a count the relay estimated locally.
///
/// It is a normal `200 OK` JSON response — the shape a client expects — plus the
/// [`ESTIMATE_HEADER`] header that says the number came from the relay's own approximation rather
/// than from a provider's tokenizer.
pub fn estimate_response(body: &[u8]) -> Response {
    let mut response = Response::new(AxumBody::from(estimate_payload(body)));
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        HeaderName::from_static(ESTIMATE_HEADER),
        HeaderValue::from_static(ESTIMATE_HEADER_VALUE),
    );
    response
}

/// Four bytes per token, rounded up, and never fewer than one token.
fn bytes_to_tokens(bytes: u64) -> u64 {
    bytes.div_ceil(CHARS_PER_TOKEN).max(1)
}

/// Add the UTF-8 byte length of every string inside `value` to `total`.
///
/// Object *keys* are not counted: the count is of the input text, not of the JSON that carries it.
fn sum_string_bytes(value: &Value, total: &mut u64) {
    match value {
        Value::String(text) => *total += text.len() as u64,
        Value::Array(items) => {
            for item in items {
                sum_string_bytes(item, total);
            }
        }
        Value::Object(map) => {
            for (_, item) in map {
                sum_string_bytes(item, total);
            }
        }
        // A number, a boolean and a null carry no text.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    /// An Anthropic-shaped request: one message, one text block.
    const ANTHROPIC_BODY: &[u8] =
        br#"{"model":"m","messages":[{"role":"user","content":"hello"}]}"#;

    /// The strings in `ANTHROPIC_BODY` that count: "user" (4) and "hello" (5).
    const ANTHROPIC_TEXT_BYTES: u64 = 9;

    #[test]
    fn an_anthropic_shaped_body_is_counted_by_its_text() {
        // "user" is 4 bytes and "hello" is 5, so the input is 9 bytes: 9 / 4 rounds up to 3.
        assert_eq!(sum_of_strings(ANTHROPIC_BODY), ANTHROPIC_TEXT_BYTES);
        assert_eq!(estimate_input_tokens(ANTHROPIC_BODY), 3);
    }

    #[test]
    fn a_system_prompt_and_a_tool_description_are_counted() {
        let body = br#"{"model":"m","system":"be brief","tools":[{"name":"t","description":"a tool that does things"}]}"#;
        // "be brief" (8) + "t" (1) + "a tool that does things" (23) = 32 bytes: 32 / 4 is 8.
        assert_eq!(sum_of_strings(body), 32);
        assert_eq!(estimate_input_tokens(body), 8);
    }

    #[test]
    fn the_top_level_model_is_not_part_of_the_input() {
        let long = "anthropic/claude-3-5-sonnet-20241022-with-a-very-long-model-name";
        let with_model =
            format!(r#"{{"model":"{long}","messages":[{{"role":"user","content":"hi"}}]}}"#);
        let without_model = r#"{"messages":[{"role":"user","content":"hi"}]}"#;
        // "user" (4) + "hi" (2) = 6 bytes: 6 / 4 rounds up to 2.
        assert_eq!(estimate_input_tokens(without_model.as_bytes()), 2);
        assert_eq!(
            estimate_input_tokens(with_model.as_bytes()),
            estimate_input_tokens(without_model.as_bytes()),
            "the model name was counted as input"
        );
    }

    #[test]
    fn a_body_that_is_not_json_falls_back_to_its_raw_length() {
        let body = b"not json at all";
        // 15 bytes: 15 / 4 rounds up to 4.
        assert_eq!(estimate_input_tokens(body), 4);
        assert_eq!(
            estimate_input_tokens(body),
            (body.len() as u64).div_ceil(CHARS_PER_TOKEN).max(1)
        );
    }

    #[test]
    fn an_empty_body_is_still_one_token() {
        assert_eq!(estimate_input_tokens(b""), 1);
    }

    #[test]
    fn a_body_with_no_input_text_is_still_one_token() {
        assert_eq!(estimate_input_tokens(br#"{"model":"m"}"#), 1);
    }

    #[test]
    fn the_estimated_response_carries_the_estimate_marker() {
        let response = estimate_response(ANTHROPIC_BODY);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(ESTIMATE_HEADER)
                .expect("the estimate marker was missing")
                .to_str()
                .unwrap(),
            "true"
        );
    }

    #[tokio::test]
    async fn the_estimated_body_is_exactly_the_input_tokens_field() {
        let response = estimate_response(ANTHROPIC_BODY);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert_eq!(text, r#"{"input_tokens":3}"#);
        // And it parses back as the Anthropic-shaped answer, with no field beside the count.
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["input_tokens"], 3);
        assert_eq!(value.as_object().unwrap().len(), 1);
    }

    /// The bytes the estimator counted, computed the slow way so the test does not depend on the
    /// estimator to check itself.
    fn sum_of_strings(body: &[u8]) -> u64 {
        let value: Value = serde_json::from_slice(body).unwrap();
        let map = value.as_object().unwrap();
        let mut total = 0u64;
        for (key, item) in map {
            if key == "model" {
                continue;
            }
            total += strings_in(item);
        }
        total
    }

    fn strings_in(value: &Value) -> u64 {
        match value {
            Value::String(text) => text.len() as u64,
            Value::Array(items) => items.iter().map(strings_in).sum(),
            Value::Object(map) => map.values().map(strings_in).sum(),
            _ => 0,
        }
    }
}
