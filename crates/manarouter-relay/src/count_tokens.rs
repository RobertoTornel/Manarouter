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
//! # The rule, exactly
//!
//! The estimate is the **length of the request body in bytes, divided by
//! [`CHARS_PER_TOKEN`](crate::quota::CHARS_PER_TOKEN) (four), rounded up, and never less than one
//! token**. It is the same four-bytes-per-token approximation the relay already uses to size a
//! quota reservation ([`crate::quota::estimate_prompt_tokens`]), so a client sees the same number
//! the relay itself assumes when it reserves quota for the same body.
//!
//! Nothing is parsed. The body is measured, not read: a length and a division, no allocation, no
//! JSON parse, no walking of the request. That is deliberate. This route sits on the request path
//! of the router, and the router's rule is that nothing it does to answer a question may cost the
//! forwarding of a request — the answer has to be cheaper than the work of *not* answering it.
//! A JSON parse would be exactly the kind of cost that rule forbids: it is real CPU on every
//! request, for a number that is an approximation anyway.
//!
//! # The trade-off: the whole body is counted
//!
//! Measuring the body means counting more than the input text. The JSON syntax, the field names,
//! the braces and the quotes are all bytes, and they all count. A real tokenizer counts the text
//! alone; this count includes the container around it. How much that costs depends on the shape
//! of the request: for a long prose prompt the shell is a thin wrapper and the estimate lands
//! within a percent or two of the text-only count, while for a small request or a tool-heavy one —
//! where field names and JSON schemas outweigh the words — the estimate can be several times the
//! text-only count. The trade is deliberate and one-sided:
//! the estimate is cheap, allocation-free and always defined, where a parsed one would be more
//! precise about the text and worse about everything else (it costs more, it can fail on a body
//! that is not JSON, and it makes the number depend on how the body was written rather than on
//! how big it is).
//!
//! The number is therefore an **approximation, not the provider's tokenizer**. The response header
//! marks it as such: a forwarded upstream count does not carry
//! [`ESTIMATE_HEADER`], because that number came from the provider's own tokenizer, and a client
//! that needs to know whether it can trust the number can look. A client that needs the *exact*
//! count has to be pointed at an Anthropic upstream, where the provider's own count comes back
//! unchanged.

use axum::body::Body as AxumBody;
use axum::http::{header::CONTENT_TYPE, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;

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
/// The count is the body's byte length divided by [`CHARS_PER_TOKEN`], rounded up, and at least
/// one — an approximation, not the provider's tokenizer. See the module documentation for what
/// that means.
pub fn estimate_input_tokens(body: &[u8]) -> u64 {
    bytes_to_tokens(body.len() as u64)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A small, ordinary Anthropic-shaped body: 60 bytes.
    const ANTHROPIC_BODY: &str = r#"{"model":"m","messages":[{"role":"user","content":"hello"}]}"#;

    /// A body with a system prompt and a tool: 96 bytes.
    const TOOL_BODY: &str = r#"{"model":"m","system":"be brief","tools":[{"name":"t","description":"a tool that does things"}]}"#;

    #[test]
    fn the_estimate_is_the_body_length_divided_by_four_rounded_up() {
        // 60 bytes / 4 = 15 exactly.
        assert_eq!(estimate_input_tokens(ANTHROPIC_BODY.as_bytes()), 15);
        // 96 bytes / 4 = 24 exactly.
        assert_eq!(estimate_input_tokens(TOOL_BODY.as_bytes()), 24);
    }

    #[test]
    fn the_estimate_rounds_up() {
        // 7 bytes / 4 = 1.75 -> 2.
        assert_eq!(estimate_input_tokens(br#"{"a":1}"#), 2);
        // 15 bytes / 4 = 3.75 -> 4.
        assert_eq!(estimate_input_tokens(b"not json at all"), 4);
    }

    #[test]
    fn the_estimate_counts_the_whole_body_including_its_json() {
        // The old rule walked the body and summed only the string values, skipping `model`. The
        // new rule measures the body, so the JSON is counted too: `{"model":"m"}` is 13 bytes and
        // therefore 4 tokens, where the old rule said 0.
        assert_eq!(estimate_input_tokens(br#"{"model":"m"}"#), 4);
        assert_eq!(estimate_input_tokens(b"{}"), 1);
    }

    #[test]
    fn a_body_that_is_not_json_is_measured_the_same_way() {
        // No special case any more: the byte count is the byte count.
        assert_eq!(estimate_input_tokens(b"not json at all"), 4);
        assert_eq!(estimate_input_tokens(b"\x00\x01\x02"), 1);
    }

    #[test]
    fn an_empty_body_is_still_one_token() {
        assert_eq!(estimate_input_tokens(b""), 1);
    }

    #[test]
    fn the_estimate_never_exceeds_the_body_it_measures() {
        // Four bytes per token means the count is at most a quarter of the body, rounded up.
        for len in 0..64usize {
            let body = vec![b'x'; len];
            let tokens = estimate_input_tokens(&body);
            assert!(tokens >= 1, "len {len} produced {tokens}");
            assert!(
                tokens <= (len as u64).div_ceil(4).max(1),
                "len {len} produced {tokens}"
            );
        }
    }

    #[test]
    fn the_payload_is_exactly_one_field() {
        assert_eq!(
            estimate_payload(ANTHROPIC_BODY.as_bytes()),
            r#"{"input_tokens":15}"#
        );
        assert_eq!(estimate_payload(b""), r#"{"input_tokens":1}"#);
    }

    #[test]
    fn the_response_carries_the_estimate_header() {
        let response = estimate_response(ANTHROPIC_BODY.as_bytes());
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(ESTIMATE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(ESTIMATE_HEADER_VALUE)
        );
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
    }
}
