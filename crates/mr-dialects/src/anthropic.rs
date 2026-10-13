//! The "OpenAI client, Anthropic upstream" direction.
//!
//! This is the pairing a Codex CLI or Cursor client needs when it is routed to a Claude model. It
//! has two halves:
//!
//! * the request half, which is genuinely different work from the other direction and lives here;
//! * the response half, which is the exact inverse of what [`crate::openai`] already does, and is
//!   therefore delegated rather than reimplemented. Duplicating an inverse is how the two halves
//!   drift apart.
//!
//! The interesting part of the request half is that Anthropic is stricter than Chat Completions in
//! three ways that a naive translation gets wrong: `max_tokens` is mandatory, `messages` must
//! alternate user/assistant, and a system prompt is a top-level field rather than a message.

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::openai::{flatten_text, pivot_wants_stream};
use crate::sse;
use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest};

/// Convert a Chat Completions request body into an Anthropic Messages request body.
pub fn from_openai_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    let v: Value = serde_json::from_slice(body).map_err(|e| TranslateError::MalformedRequest {
        dialect: "openai",
        detail: e.to_string(),
    })?;
    let obj = v
        .as_object()
        .ok_or_else(|| TranslateError::MalformedRequest {
            dialect: "openai",
            detail: "request body must be a JSON object".to_string(),
        })?;

    let stream = pivot_wants_stream(&v);
    let out = pivot_request_to_anthropic(ctx, obj, stream)?;

    let bytes = serde_json::to_vec(&out).map_err(|e| TranslateError::MalformedRequest {
        dialect: "openai",
        detail: e.to_string(),
    })?;

    Ok(TranslatedRequest {
        body: Bytes::from(bytes),
        content_type: "application/json",
        force_stream: false,
        // Anthropic's Messages endpoint is not the OpenAI path, so the translator names it. The
        // provider's base URL is expected to already end in `/v1`.
        path_override: Some("messages".to_string()),
    })
}

/// Build an Anthropic Messages request from a Chat Completions request object.
pub fn pivot_request_to_anthropic(
    ctx: &TranslateCtx<'_>,
    obj: &Map<String, Value>,
    stream: bool,
) -> Result<Value> {
    let mut out = Map::new();
    out.insert("model".into(), json!(ctx.upstream_model));

    // Anthropic requires max_tokens and rejects the request outright without it. There is no
    // correct way to guess the client's intent, so a deliberately generous default is used and the
    // value is always sent. Getting this wrong is the most common cause of a hard 400 on this hop.
    let max_tokens = obj
        .get("max_tokens")
        .or_else(|| obj.get("max_completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_TOKENS);
    out.insert("max_tokens".into(), json!(max_tokens));

    for key in ["temperature", "top_p"] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(meta) = obj.get("metadata") {
        out.insert("metadata".into(), meta.clone());
    }
    if let Some(stop) = obj.get("stop") {
        // Chat Completions allows a bare string; Anthropic requires an array.
        let seqs = match stop {
            Value::String(s) => json!([s]),
            Value::Array(a) => json!(a),
            _ => json!([]),
        };
        if seqs.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
            out.insert("stop_sequences".into(), seqs);
        }
    }

    let mut messages: Vec<Value> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();

    for m in obj
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            // Anthropic has no system role inside `messages`; it is a top-level field. Collecting
            // them here rather than dropping them is what preserves an agent's instructions.
            "system" | "developer" => {
                let t = flatten_text(m.get("content").unwrap_or(&Value::Null));
                if !t.is_empty() {
                    system_parts.push(t);
                }
            }
            "tool" => {
                // A Chat Completions `tool` message is an Anthropic `tool_result` block inside a
                // user turn.
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": m.get("tool_call_id").cloned().unwrap_or(json!("")),
                    "content": anthropic_tool_result_content(m.get("content")),
                });
                push_block(&mut messages, "user", block);
            }
            "assistant" => {
                let mut blocks: Vec<Value> = Vec::new();
                if let Some(c) = m.get("content") {
                    let t = flatten_text(c);
                    if !t.is_empty() {
                        blocks.push(json!({"type": "text", "text": t}));
                    }
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for c in calls {
                        let f = c.get("function");
                        let input = f
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            // A broken arguments string becomes an empty input rather than failing
                            // the whole request: the model can recover from an empty tool input,
                            // but a transport error loses the whole turn.
                            .unwrap_or_else(|| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": c.get("id").cloned().unwrap_or(json!("")),
                            "name": f.and_then(|f| f.get("name")).cloned().unwrap_or(json!("")),
                            "input": input,
                        }));
                    }
                }
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                push_blocks(&mut messages, "assistant", blocks);
            }
            _ => {
                let blocks = user_content_to_blocks(m.get("content"));
                push_blocks(&mut messages, "user", blocks);
            }
        }
    }

    // Anthropic rejects an empty messages array, and rejects a request whose only content is a
    // system prompt with no turn at all. A single empty user turn is the minimal valid request.
    if messages.is_empty() {
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": ""}]}));
    }

    if !system_parts.is_empty() {
        out.insert("system".into(), json!(system_parts.join("\n\n")));
    }
    out.insert("messages".into(), Value::Array(messages));

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools.iter().filter_map(openai_tool_to_anthropic).collect();
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    if let Some(tc) = obj.get("tool_choice") {
        if let Some(mapped) = openai_tool_choice_to_anthropic(tc) {
            out.insert("tool_choice".into(), mapped);
        }
    }
    if stream {
        out.insert("stream".into(), json!(true));
    }

    // `n > 1` has no Anthropic equivalent. It is dropped rather than forwarded, because sending an
    // unknown field is a hard 400 on this API.
    Ok(Value::Object(out))
}

/// The `max_tokens` used when a Chat Completions client omitted it.
///
/// Anthropic mandates the field, so *something* must be sent. 8192 is chosen because it is within
/// every current Claude model's output ceiling, so the request never fails for exceeding the limit,
/// while being small enough that a runaway generation is bounded.
pub const DEFAULT_MAX_TOKENS: u64 = 8192;

/// Convert a Chat Completions `content` value into Anthropic content blocks.
fn user_content_to_blocks(content: Option<&Value>) -> Vec<Value> {
    match content {
        Some(Value::String(s)) => vec![json!({"type": "text", "text": s})],
        Some(Value::Array(parts)) => {
            let mut blocks = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = p.get("text") {
                            blocks.push(json!({"type": "text", "text": t}));
                        }
                    }
                    Some("image_url") => {
                        if let Some(block) = openai_image_to_anthropic(p) {
                            blocks.push(block);
                        }
                    }
                    _ => {}
                }
            }
            if blocks.is_empty() {
                blocks.push(json!({"type": "text", "text": ""}));
            }
            blocks
        }
        Some(other) => vec![json!({"type": "text", "text": flatten_text(other)})],
        None => vec![json!({"type": "text", "text": ""})],
    }
}

/// Convert an OpenAI `image_url` part into an Anthropic image block.
///
/// A data URL is split back into the base64 source form Anthropic expects, and a plain URL is
/// passed through as a `url` source, which Anthropic supports directly.
fn openai_image_to_anthropic(part: &Value) -> Option<Value> {
    let url = part
        .get("image_url")
        .and_then(|i| i.get("url"))
        .and_then(Value::as_str)?;
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, data) = rest.split_once(',')?;
        let media_type = meta.split(';').next().unwrap_or("image/png");
        Some(json!({
            "type": "image",
            "source": {"type": "base64", "media_type": media_type, "data": data}
        }))
    } else {
        Some(json!({
            "type": "image",
            "source": {"type": "url", "url": url}
        }))
    }
}

/// Convert an OpenAI tool definition into an Anthropic one.
fn openai_tool_to_anthropic(tool: &Value) -> Option<Value> {
    let f = tool.get("function")?;
    Some(json!({
        "name": f.get("name").cloned().unwrap_or(json!("")),
        "description": f.get("description").cloned().unwrap_or(json!("")),
        // Anthropic calls it `input_schema`; Chat Completions calls it `parameters`.
        "input_schema": f
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
    }))
}

/// Convert an OpenAI `tool_choice` into an Anthropic one.
fn openai_tool_choice_to_anthropic(tc: &Value) -> Option<Value> {
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" | "any" => Some(json!({"type": "any"})),
            "none" => None,
            _ => Some(json!({"type": "auto"})),
        },
        Value::Object(o) => {
            let name = o
                .get("function")
                .and_then(|f| f.get("name"))
                .cloned()
                .unwrap_or(json!(""));
            Some(json!({"type": "tool", "name": name}))
        }
        _ => Some(json!({"type": "auto"})),
    }
}

/// Build the `content` of an Anthropic `tool_result` block.
///
/// Anthropic accepts either a bare string or an array of blocks; a multipart tool result from a
/// vision-aware client is preserved as blocks rather than flattened, because flattening would drop
/// the image.
fn anthropic_tool_result_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => json!(s),
        Some(Value::Array(parts)) => {
            let blocks: Vec<Value> = parts
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("text") => p.get("text").map(|t| json!({"type": "text", "text": t})),
                    Some("image_url") => openai_image_to_anthropic(p),
                    _ => None,
                })
                .collect();
            if blocks.is_empty() {
                json!("")
            } else {
                Value::Array(blocks)
            }
        }
        Some(other) => json!(flatten_text(other)),
        None => json!(""),
    }
}

/// Append a single block to the trailing message if it is for the same role, else start a new turn.
///
/// Anthropic requires `messages` to alternate strictly between user and assistant. A Chat
/// Completions conversation that contains two consecutive `tool` messages would otherwise produce
/// two consecutive user turns and be rejected, so consecutive same-role content is merged.
fn push_block(messages: &mut Vec<Value>, role: &str, block: Value) {
    push_blocks(messages, role, vec![block]);
}

/// Append blocks to the trailing message if the role matches, else start a new turn.
fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(arr) = last.get_mut("content").and_then(Value::as_array_mut) {
                arr.extend(blocks);
                return;
            }
        }
    }
    messages.push(json!({"role": role, "content": Value::Array(blocks)}));
}

/// Convert an Anthropic Messages response body into a Chat Completions response body.
///
/// Delegated to the shared pivot inverse so the two directions cannot drift.
pub fn to_openai_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    crate::openai::to_openai_response(ctx, body)
}

/// Translate one Anthropic SSE payload into Chat Completions SSE frames.
///
/// Delegated for the same reason: this is the exact inverse of the other direction and keeping one
/// implementation is what stops the halves disagreeing.
pub fn sse_to_openai(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    crate::openai::sse_to_openai(ctx, data)
}

/// Reset the stream state for the Anthropic-to-OpenAI direction.
pub fn reset_stream_state() {
    crate::openai::reset_openai_stream_state();
}

/// Map an Anthropic stop reason, re-exported so callers need only this module.
pub use crate::openai::map_anthropic_stop_reason as map_stop_reason;

/// Build a minimal Anthropic streaming error frame for the mid-stream retry boundary.
///
/// Emitted when `midstream_policy` is `terminate`, or when a splice is refused because tool-call
/// deltas were already sent.
pub fn stream_error_frame(message: &str) -> String {
    sse::named_frame(
        "error",
        &json!({"type": "error", "error": {"type": "api_error", "message": message}}).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::{Dialect, RouteKind};

    fn ctx() -> TranslateCtx<'static> {
        TranslateCtx::new(
            RouteKind::ChatCompletions,
            Dialect::Anthropic,
            "claude-sonnet",
        )
    }

    fn build(v: Value) -> Value {
        let body = serde_json::to_vec(&v).unwrap();
        let t = from_openai_request(&ctx(), &body).unwrap();
        serde_json::from_slice(&t.body).unwrap()
    }

    #[test]
    fn max_tokens_is_always_sent_because_anthropic_demands_it() {
        let out = build(json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(out["max_tokens"], DEFAULT_MAX_TOKENS);

        let out = build(json!({"model": "x", "max_tokens": 42, "messages": []}));
        assert_eq!(out["max_tokens"], 42);

        // `max_completion_tokens` is the newer Chat Completions spelling and must be honoured too.
        let out = build(json!({"model": "x", "max_completion_tokens": 7, "messages": []}));
        assert_eq!(out["max_tokens"], 7);
    }

    #[test]
    fn system_messages_are_lifted_to_the_top_level_field() {
        let out = build(json!({
            "model": "x",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "developer", "content": "and polite"},
                {"role": "user", "content": "hi"}
            ]
        }));
        assert_eq!(out["system"], "be terse\n\nand polite");
        // The system turns must not also remain in `messages`, or Anthropic rejects the role.
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn the_anthropic_messages_path_is_named_explicitly() {
        // Anthropic's endpoint is not the OpenAI path, so a default base-plus-route join would
        // silently 404.
        let t = from_openai_request(&ctx(), br#"{"model":"x","messages":[]}"#).unwrap();
        assert_eq!(t.path_override.as_deref(), Some("messages"));
    }

    #[test]
    fn consecutive_tool_messages_merge_into_one_user_turn() {
        // Chat Completions emits one `tool` message per result; Anthropic requires strictly
        // alternating roles, so two consecutive tool results must become one user turn with two
        // tool_result blocks rather than two user turns (which is a 400).
        let out = build(json!({
            "model": "x",
            "messages": [
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "a", "type": "function", "function": {"name": "f", "arguments": "{}"}},
                    {"id": "b", "type": "function", "function": {"name": "g", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "a", "content": "ra"},
                {"role": "tool", "tool_call_id": "b", "content": "rb"}
            ]
        }));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(
            msgs.len(),
            2,
            "expected assistant then one merged user turn"
        );
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[1]["role"], "user");
        let blocks = msgs[1]["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "a");
        assert_eq!(blocks[1]["tool_use_id"], "b");
    }

    #[test]
    fn tool_calls_become_tool_use_blocks_with_parsed_input() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "assistant", "content": "ok", "tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
            ]}]
        }));
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "get_weather");
        // Anthropic wants `input` as an object, not a JSON string.
        assert_eq!(blocks[1]["input"]["city"], "Paris");
    }

    #[test]
    fn broken_tool_arguments_become_an_empty_input_rather_than_failing_the_request() {
        // The model can recover from an empty tool input; losing the whole turn to a transport
        // error cannot be recovered from.
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "assistant", "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{oops"}}
            ]}]
        }));
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[0]["input"], json!({}));
    }

    #[test]
    fn tool_definitions_are_renamed_to_input_schema() {
        let out = build(json!({
            "model": "x", "messages": [],
            "tools": [{"type": "function", "function": {
                "name": "f", "description": "d",
                "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}
            }}]
        }));
        let tool = &out["tools"][0];
        assert_eq!(tool["name"], "f");
        assert!(tool["input_schema"]["properties"]["a"].is_object());
        assert!(tool.get("parameters").is_none());
        assert!(
            tool.get("type").is_none(),
            "the function wrapper must be stripped"
        );
    }

    #[test]
    fn tool_without_parameters_still_gets_a_schema() {
        let out = build(json!({
            "model": "x", "messages": [],
            "tools": [{"type": "function", "function": {"name": "f"}}]
        }));
        assert_eq!(out["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn tool_choice_none_is_omitted_because_anthropic_has_no_equivalent() {
        let out = build(json!({"model": "x", "messages": [], "tool_choice": "none"}));
        assert!(out.get("tool_choice").is_none());
        let out = build(json!({"model": "x", "messages": [], "tool_choice": "required"}));
        assert_eq!(out["tool_choice"]["type"], "any");
        let out = build(json!({
            "model": "x", "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "f"}}
        }));
        assert_eq!(out["tool_choice"]["type"], "tool");
        assert_eq!(out["tool_choice"]["name"], "f");
    }

    #[test]
    fn a_bare_string_stop_becomes_a_stop_sequences_array() {
        let out = build(json!({"model": "x", "messages": [], "stop": "\n\n"}));
        assert_eq!(out["stop_sequences"], json!(["\n\n"]));
        let out = build(json!({"model": "x", "messages": [], "stop": []}));
        // An empty array is omitted rather than sent, because Anthropic rejects an empty list.
        assert!(out.get("stop_sequences").is_none());
    }

    #[test]
    fn data_url_images_are_split_back_into_base64_sources() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,ZZZ"}}
            ]}]
        }));
        let blocks = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "ZZZ");
    }

    #[test]
    fn plain_url_images_become_url_sources() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}}
            ]}]
        }));
        assert_eq!(out["messages"][0]["content"][0]["source"]["type"], "url");
        assert_eq!(
            out["messages"][0]["content"][0]["source"]["url"],
            "https://example.test/a.png"
        );
    }

    #[test]
    fn an_empty_messages_array_becomes_one_empty_user_turn() {
        // Anthropic rejects an empty messages array, so a request that was only a system prompt
        // must still carry a turn.
        let out = build(json!({"model": "x", "messages": [{"role": "system", "content": "s"}]}));
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
    }

    #[test]
    fn unsupported_chat_completions_fields_are_not_forwarded() {
        // Anthropic rejects unknown fields outright, so `n` and `stream_options` must be dropped
        // rather than echoed.
        let out = build(json!({
            "model": "x", "messages": [], "n": 3,
            "stream_options": {"include_usage": true}, "logit_bias": {"1": 2}
        }));
        assert!(out.get("n").is_none());
        assert!(out.get("stream_options").is_none());
        assert!(out.get("logit_bias").is_none());
    }

    #[test]
    fn stream_flag_is_propagated_only_when_set() {
        let out = build(json!({"model": "x", "messages": [], "stream": true}));
        assert_eq!(out["stream"], true);
        let out = build(json!({"model": "x", "messages": []}));
        assert!(out.get("stream").is_none());
    }

    #[test]
    fn malformed_json_is_reported_as_an_openai_request_error() {
        let err = from_openai_request(&ctx(), b"nope").unwrap_err();
        match err {
            TranslateError::MalformedRequest { dialect, .. } => assert_eq!(dialect, "openai"),
            other => panic!("expected MalformedRequest, got {other:?}"),
        }
    }

    #[test]
    fn multi_part_tool_result_content_is_preserved_as_blocks() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "tool", "tool_call_id": "a", "content": [
                {"type": "text", "text": "here"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QQ=="}}
            ]}]
        }));
        let content = &out["messages"][0]["content"][0]["content"];
        assert!(content.is_array(), "a multipart result must stay multipart");
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image");
    }

    #[test]
    fn stream_error_frame_is_a_valid_named_event() {
        let f = stream_error_frame("boom");
        assert!(f.starts_with("event: error\n"));
        assert!(f.contains("api_error"));
        assert!(f.ends_with("\n\n"));
    }

    #[test]
    fn stop_reason_mapping_is_reachable_through_this_module() {
        assert_eq!(map_stop_reason(Some("max_tokens")), json!("length"));
    }
}
