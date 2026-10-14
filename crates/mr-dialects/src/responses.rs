//! The OpenAI Responses API, both directions against the Chat Completions pivot.
//!
//! Responses is the dialect Codex speaks at `chatgpt.com/backend-api/codex/responses`, and the one
//! a Responses-native client (`RouteKind::Responses`) expects on the wire. Two properties set it
//! apart from Chat Completions and drive everything in this module:
//!
//! * the transcript lives in a typed `input` item array rather than free-form `messages`, with
//!   `instructions` as the top-level system slot — so a request translation is a structured
//!   rewrite, not a rename;
//! * the endpoint is SSE-first: `stream` is effectively always on, `store` must be `false`, and
//!   the stream is a typed event protocol (`response.created`, `output_text.delta`,
//!   `response.completed`, …) rather than bare `chat.completion.chunk` payloads — so a response
//!   translation has to track cross-frame state: one role chunk, stable tool indices, one
//!   terminator.
//!
//! The terminator is where the two client shapes diverge: a Chat Completions client needs a final
//! `finish_reason` chunk and `data: [DONE]`, while a Responses client terminates on
//! `response.completed` and treats `[DONE]` as protocol noise — so `[DONE]` is suppressed for
//! Responses routes and re-emitted only for Chat Completions clients.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use bytes::Bytes;
use mr_core::{Dialect, RouteKind};
use serde_json::{json, Map, Value};

use crate::openai::{
    flatten_text, pivot_response_to_anthropic, rewrite_model, sse_to_anthropic,
    AnthropicStreamState,
};
use crate::sse;
use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest, SSE_DONE};

/// A request body that could not be read as the client dialect.
fn malformed_request(detail: impl Into<String>) -> TranslateError {
    TranslateError::MalformedRequest {
        dialect: "openai",
        detail: detail.into(),
    }
}

/// Convert a client request into a Responses API request.
///
/// Two client shapes arrive here and they are told apart by the body, not by the context:
///
/// * a **Chat Completions pivot** request (`messages`), which is a structured rewrite into
///   `instructions` + `input`;
/// * a **Responses-native** client (`input`, `RouteKind::Responses`), whose body is already the
///   upstream shape and only needs the model and the two always-on flags normalised.
///
/// Either way the upstream is Codex, so `stream` is forced on and `store` is forced off; the relay
/// reads the SSE answer back even for a client that asked for one JSON document.
pub fn from_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    let v: Value = serde_json::from_slice(body).map_err(|e| malformed_request(e.to_string()))?;
    let obj = v
        .as_object()
        .ok_or_else(|| malformed_request("request body must be a JSON object"))?;

    // A Responses body names its transcript `input`; a pivot body names it `messages`.
    let mut out = match obj.get("messages").and_then(Value::as_array) {
        Some(messages) => pivot_messages_to_responses(ctx, obj, messages),
        None if obj.get("input").is_some() => Value::Object(obj.clone()),
        None => {
            return Err(malformed_request(
                "request body has neither `messages` nor `input`",
            ))
        }
    };

    // The Responses endpoint is SSE-first: `stream` is always true and `store` always false, in
    // both directions, because Codex streams an answer regardless and rejects `store: true`.
    if let Some(map) = out.as_object_mut() {
        map.insert("stream".into(), json!(true));
        map.insert("store".into(), json!(false));
    }
    rewrite_model(&mut out, ctx.upstream_model);
    // A Responses client that sent `input: []` would otherwise be answered with a rejection: the
    // upstream refuses an empty transcript, so the reference bridge substitutes one placeholder
    // user turn and this does the same.
    if out
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::is_empty)
        .unwrap_or(false)
    {
        out["input"] = json!([{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": ""}],
        }]);
    }

    let bytes = serde_json::to_vec(&out).map_err(|e| malformed_request(e.to_string()))?;
    Ok(TranslatedRequest {
        body: Bytes::from(bytes),
        content_type: "application/json",
        // Codex answers with an event stream whatever the client asked for.
        force_stream: true,
        // `/responses` is `Dialect::Codex`'s declared default path, so no override is needed.
        path_override: None,
    })
}

/// Rewrite a Chat Completions `messages` array into `instructions` + `input`.
///
/// Fields with no Responses equivalent (`n`, `stop`, `stream_options`) are dropped rather than
/// smuggled through: the upstream rejects what it does not understand.
fn pivot_messages_to_responses(
    ctx: &TranslateCtx<'_>,
    obj: &Map<String, Value>,
    messages: &[Value],
) -> Value {
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        message_into_items(message, &mut instructions, &mut input);
    }

    let mut out = Map::new();
    if !instructions.is_empty() {
        out.insert("instructions".into(), json!(instructions));
    }
    out.insert("input".into(), Value::Array(input));

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let tools: Vec<Value> = tools.iter().filter_map(tool_declaration).collect();
        if !tools.is_empty() {
            out.insert("tools".into(), Value::Array(tools));
        }
    }
    if let Some(choice) = obj.get("tool_choice").and_then(tool_choice) {
        out.insert("tool_choice".into(), choice);
    }
    copy_field(obj, "temperature", &mut out);
    copy_field(obj, "top_p", &mut out);
    copy_field(obj, "parallel_tool_calls", &mut out);
    // Both spellings of the output cap mean the same thing upstream.
    if let Some(cap) = obj
        .get("max_completion_tokens")
        .or_else(|| obj.get("max_tokens"))
        .filter(|v| !v.is_null())
    {
        out.insert("max_output_tokens".into(), cap.clone());
    }
    // Chat Completions carries a bare `reasoning_effort`; Responses nests it under `reasoning`.
    if let Some(effort) = obj.get("reasoning_effort").and_then(Value::as_str) {
        out.insert("reasoning".into(), json!({"effort": effort}));
    }
    if let Some(text) = obj.get("response_format").and_then(text_format) {
        out.insert("text".into(), text);
    }
    // Prompt-cache affinity is per client session; Responses spells it `prompt_cache_key`.
    if let Some(session) = ctx.session_id {
        out.insert("prompt_cache_key".into(), json!(session));
    }
    Value::Object(out)
}

/// Append one pivot message to the Responses transcript, folding system turns into `instructions`.
fn message_into_items(message: &Value, instructions: &mut String, out: &mut Vec<Value>) {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let null = Value::Null;
    let content = message.get("content").unwrap_or(&null);
    match role {
        // Responses has no `system` role item in the input array: both spellings become the
        // top-level `instructions` string, joined in the order they appeared.
        "system" | "developer" => {
            let text = flatten_text(content);
            if !text.is_empty() {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(&text);
            }
        }
        "tool" => {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            out.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": flatten_text(content),
            }));
        }
        "assistant" => {
            if let Some(parts) = content_parts("assistant", content) {
                if !parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": parts,
                    }));
                }
            }
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    out.push(function_call_item(call));
                }
            }
        }
        _ => {
            if let Some(parts) = content_parts("user", content) {
                if !parts.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": parts,
                    }));
                }
            }
        }
    }
}

/// One pivot assistant `tool_call` as a Responses `function_call` item.
fn function_call_item(call: &Value) -> Value {
    let function = call.get("function").unwrap_or(&Value::Null);
    json!({
        "type": "function_call",
        "call_id": call.get("id").and_then(Value::as_str).unwrap_or_default(),
        "name": function.get("name").and_then(Value::as_str).unwrap_or_default(),
        "arguments": function
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    })
}

/// Rewrite a message `content` into Responses content parts.
///
/// Returns `None` when there is nothing to say — an absent or empty content string — because an
/// empty `content: []` item is rejected upstream. Text parts are `output_text` for an assistant
/// turn and `input_text` for anything else, which is what the two roles accept.
fn content_parts(role: &str, content: &Value) -> Option<Vec<Value>> {
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    match content {
        Value::String(s) => {
            if s.is_empty() {
                None
            } else {
                Some(vec![json!({"type": text_type, "text": s})])
            }
        }
        Value::Array(parts) => {
            let mut out = Vec::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        out.push(json!({
                            "type": text_type,
                            "text": part.get("text").and_then(Value::as_str).unwrap_or_default(),
                        }));
                    }
                    Some("image_url") => {
                        let image = part.get("image_url");
                        let url = image
                            .and_then(|i| i.get("url"))
                            .and_then(Value::as_str)
                            .or_else(|| image.and_then(Value::as_str));
                        if let Some(url) = url {
                            let mut rewritten = json!({"type": "input_image", "image_url": url});
                            if let Some(detail) = image.and_then(|i| i.get("detail")) {
                                rewritten["detail"] = detail.clone();
                            }
                            out.push(rewritten);
                        }
                    }
                    // Already Responses-shaped parts ride through untouched.
                    Some("input_text") | Some("input_image") | Some("input_file") => {
                        out.push(part.clone());
                    }
                    _ => {}
                }
            }
            Some(out)
        }
        _ => None,
    }
}

/// One pivot tool declaration as a Responses tool.
///
/// Responses flattens the declaration (`{"type":"function","name":…,"parameters":…}`) where Chat
/// Completions nests it under `function`. Built-in tools (`web_search`, `file_search`, …) are
/// already Responses-shaped and pass through.
fn tool_declaration(tool: &Value) -> Option<Value> {
    match tool.get("type").and_then(Value::as_str) {
        Some("function") => {
            let function = tool.get("function").unwrap_or(tool);
            let name = function.get("name").and_then(Value::as_str)?;
            let mut out = Map::new();
            out.insert("type".into(), json!("function"));
            out.insert("name".into(), json!(name));
            if let Some(description) = function.get("description").filter(|v| !v.is_null()) {
                out.insert("description".into(), description.clone());
            }
            if let Some(parameters) = function.get("parameters") {
                out.insert("parameters".into(), parameters.clone());
            }
            if let Some(strict) = function.get("strict").filter(|v| !v.is_null()) {
                out.insert("strict".into(), strict.clone());
            }
            Some(Value::Object(out))
        }
        Some(_) => Some(tool.clone()),
        None => None,
    }
}

/// Rewrite a pivot `tool_choice` into its flattened Responses spelling.
fn tool_choice(choice: &Value) -> Option<Value> {
    match choice {
        Value::String(_) => Some(choice.clone()),
        Value::Object(obj) => {
            match obj
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                Some(name) => Some(json!({"type": "function", "name": name})),
                // A named Responses tool choice is already in this shape.
                None if obj.contains_key("type") => Some(choice.clone()),
                None => None,
            }
        }
        _ => None,
    }
}

/// Rewrite a pivot `response_format` into the Responses `text` object.
fn text_format(format: &Value) -> Option<Value> {
    match format.get("type").and_then(Value::as_str)? {
        "text" => Some(json!({"format": {"type": "text"}})),
        "json_object" => Some(json!({"format": {"type": "json_object"}})),
        "json_schema" => {
            let schema = format.get("json_schema")?;
            let mut out = Map::new();
            out.insert("type".into(), json!("json_schema"));
            for key in ["name", "description", "schema", "strict"] {
                if let Some(value) = schema.get(key).filter(|v| !v.is_null()) {
                    out.insert(key.into(), value.clone());
                }
            }
            Some(json!({"format": Value::Object(out)}))
        }
        _ => None,
    }
}

/// Copy a field through unchanged, skipping JSON nulls so the upstream sees it as absent.
fn copy_field(src: &Map<String, Value>, key: &str, dst: &mut Map<String, Value>) {
    if let Some(value) = src.get(key).filter(|v| !v.is_null()) {
        dst.insert(key.to_string(), value.clone());
    }
}

/// A response body that could not be read as the upstream dialect.
fn malformed_response(detail: impl Into<String>) -> TranslateError {
    TranslateError::MalformedResponse {
        dialect: "responses",
        detail: detail.into(),
    }
}

// ---------------------------------------------------------------------------------------------
// Responses -> Chat Completions pivot
// ---------------------------------------------------------------------------------------------

/// Fold whatever the upstream sent into a Responses response object.
///
/// Two shapes arrive: a JSON document (`error` or `response`), and a whole buffered SSE stream —
/// Codex always streams, so a non-streaming client's answer is folded here.
fn responses_object(text: &str) -> Result<Value> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(malformed_response("empty response body"));
    }
    if trimmed.starts_with('{') {
        let value: Value =
            serde_json::from_str(trimmed).map_err(|e| malformed_response(e.to_string()))?;
        // A single event object — the relay handing over one frame's payload — names its response.
        if let Some(response) = value.get("response") {
            return Ok(response.clone());
        }
        return Ok(value);
    }
    fold_responses_stream(trimmed)
}

/// Rebuild a response object from a buffered Responses event stream.
///
/// The terminal `response.completed` / `response.incomplete` event carries the complete object and
/// is preferred. A stream that died before one is rebuilt from the output items and text deltas
/// that did arrive, so a truncated answer still reaches the client as something rather than a 502.
fn fold_responses_stream(text: &str) -> Result<Value> {
    let mut terminal: Option<Value> = None;
    let mut error: Option<Value> = None;
    let mut items: BTreeMap<u64, Value> = BTreeMap::new();
    let mut text_by_index: BTreeMap<u64, String> = BTreeMap::new();
    let mut meta = Map::new();
    let mut status = "completed".to_string();
    let mut usage: Option<Value> = None;

    for line in text.lines() {
        let candidate = line
            .trim()
            .strip_prefix("data:")
            .unwrap_or(line.trim())
            .trim();
        if candidate.is_empty() || candidate == SSE_DONE {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(candidate) else {
            // A keep-alive or an unparseable line: skipped, never fatal.
            continue;
        };
        if let Some(response) = event.get("response") {
            for key in ["id", "object", "model", "created_at"] {
                if let Some(value) = response.get(key) {
                    meta.insert(key.to_string(), value.clone());
                }
            }
            if let Some(value) = response.get("status").and_then(Value::as_str) {
                status = value.to_string();
            }
            if let Some(value) = response.get("usage").filter(|u| !u.is_null()) {
                usage = Some(value.clone());
            }
        }
        match event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "response.output_item.added" | "response.output_item.done" => {
                if let (Some(index), Some(item)) = (
                    event.get("output_index").and_then(Value::as_u64),
                    event.get("item"),
                ) {
                    items.insert(index, item.clone());
                }
            }
            "response.output_text.delta" => {
                text_by_index
                    .entry(output_index(&event))
                    .or_default()
                    .push_str(
                        event
                            .get("delta")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
            }
            "response.completed" | "response.incomplete" => {
                terminal = event.get("response").cloned();
            }
            "response.failed" => match event.get("response").and_then(|r| r.get("error")) {
                Some(err) => error = Some(err.clone()),
                None => terminal = event.get("response").cloned(),
            },
            "error" => error = Some(event.clone()),
            _ => {}
        }
    }

    if let Some(err) = error {
        return Ok(json!({"error": err}));
    }
    if let Some(response) = terminal {
        return Ok(response);
    }

    let output: Vec<Value> = items
        .into_iter()
        .map(|(index, mut item)| {
            if item.get("type").and_then(Value::as_str) == Some("message") {
                if let Some(accumulated) = text_by_index.get(&index).filter(|t| !t.is_empty()) {
                    item["content"] = json!([{"type": "output_text", "text": accumulated}]);
                }
            }
            item
        })
        .collect();
    let mut response = Map::new();
    response.insert("object".into(), json!("response"));
    response.insert("status".into(), json!(status));
    for (key, value) in meta {
        response.insert(key, value);
    }
    if let Some(value) = usage {
        response.insert("usage".into(), value);
    }
    response.insert("output".into(), Value::Array(output));
    Ok(Value::Object(response))
}

/// The `output_index` an event applies to, defaulting to the first output item.
fn output_index(event: &Value) -> u64 {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Rewrite a Responses response object into the Chat Completions pivot.
pub fn responses_to_pivot(ctx: &TranslateCtx<'_>, response: &Value) -> Value {
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for item in response
        .get("output")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                        text.push_str(part_text);
                    }
                }
            }
            Some("function_call") => tool_calls.push(json!({
                "id": item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .unwrap_or_default(),
                "type": "function",
                "function": {
                    "name": item.get("name").and_then(Value::as_str).unwrap_or_default(),
                    "arguments": item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                },
            })),
            _ => {}
        }
    }

    let finish_reason = finish_reason(response);
    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    if text.is_empty() && !tool_calls.is_empty() {
        // Chat Completions clients and providers both accept null here, and several reject an
        // empty string paired with tool calls.
        message.insert("content".into(), Value::Null);
    } else {
        message.insert("content".into(), json!(text));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    json!({
        "id": response
            .get("id")
            .cloned()
            .unwrap_or_else(|| json!(format!("chatcmpl-{}", mr_core::ids::new_id()))),
        "object": "chat.completion",
        "created": response
            .get("created_at")
            .cloned()
            .unwrap_or_else(|| json!(mr_core::ids::now_millis() / 1000)),
        "model": response
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(ctx.upstream_model),
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": pivot_usage(response.get("usage")),
    })
}

/// A Responses `status` (and `incomplete_details`) as a Chat Completions `finish_reason`.
fn finish_reason(response: &Value) -> &'static str {
    let has_tool_calls = response
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .any(|i| i.get("type").and_then(Value::as_str) == Some("function_call"))
        })
        .unwrap_or(false);
    match response
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("completed")
    {
        // Truncation is the only incomplete reason Codex reports.
        "incomplete" => "length",
        // A failure has no Chat Completions equivalent; the answer is what it is.
        "failed" => "stop",
        _ if has_tool_calls => "tool_calls",
        _ => "stop",
    }
}

/// Responses usage as Chat Completions usage.
fn pivot_usage(usage: Option<&Value>) -> Value {
    let input = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = usage
        .and_then(|u| u.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    let cached = usage
        .and_then(|u| u.get("input_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": total,
        "prompt_tokens_details": {"cached_tokens": cached},
    })
}

/// Serialise a translated value, reporting a serialisation failure as a response error.
fn to_bytes(value: Value) -> Result<Bytes> {
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|e| malformed_response(e.to_string()))
}

/// Convert a Responses API response into the client dialect.
///
/// The upstream always streams, so a client that asked for one JSON document arrives here with a
/// whole buffered SSE body; [`responses_object`] folds it back into a response before the pivot
/// rewrite. A Responses-native client (route [`RouteKind::Responses`]) is answered with the
/// Responses object itself — there is nothing to convert and converting would lose it.
pub fn to_pivot_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    let text = std::str::from_utf8(body).map_err(|e| malformed_response(e.to_string()))?;
    let response = responses_object(text)?;

    if ctx.route == RouteKind::Responses {
        return to_bytes(response);
    }
    // An upstream error is already OpenAI-shaped, which is the pivot's own error shape.
    if response.get("error").is_some() && response.get("output").is_none() {
        return to_bytes(response);
    }

    let pivot = responses_to_pivot(ctx, &response);
    let out = match ctx.client_dialect {
        Dialect::Anthropic => {
            let mut state = AnthropicStreamState::default();
            pivot_response_to_anthropic(ctx, &pivot, &mut state)?
        }
        _ => pivot,
    };
    to_bytes(out)
}

// ---------------------------------------------------------------------------------------------
// The Responses event stream
// ---------------------------------------------------------------------------------------------

/// One accumulated function call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
    /// Whether an opening `tool_calls` delta naming this call has already gone out.
    announced: bool,
}

/// Everything that must survive from one upstream event to the next.
///
/// This is the Rust equivalent of the reference bridge's `content` / `toolCalls` / `toolIndices` /
/// `sentRole` / `finishReason` / `usage` locals. It is deliberately per-stream: sharing it between
/// concurrent conversations is exactly how two answers get spliced together.
#[derive(Debug, Clone)]
struct ResponseStreamState {
    id: String,
    model: String,
    created: i64,
    tool_calls: Vec<ToolCall>,
    /// Item id to index in `tool_calls`. Argument fragments name an item, and the client deltas
    /// must carry the *index* that item already has — not a fresh one per fragment.
    tool_index: HashMap<String, usize>,
    finish_reason: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    usage_seen: bool,
    /// `role: "assistant"` rides the first delta of any kind, and is never repeated.
    sent_role: bool,
    /// The terminal frame has been emitted; the next event starts a new stream.
    done: bool,
}

impl ResponseStreamState {
    fn new(ctx: &TranslateCtx<'_>) -> Self {
        Self {
            id: format!("chatcmpl-{}", mr_core::ids::new_id()),
            model: ctx.upstream_model.to_string(),
            created: mr_core::ids::now_millis() / 1000,
            tool_calls: Vec::new(),
            tool_index: HashMap::new(),
            finish_reason: "stop".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            usage_seen: false,
            sent_role: false,
            done: false,
        }
    }
}

thread_local! {
    /// Stream state for whichever Responses stream is in flight on this thread.
    ///
    /// Translation is called frame by frame with no per-stream handle, so the accumulation has to
    /// live somewhere. A thread-local keeps concurrent conversations from sharing it; the state
    /// also resets itself once a terminal frame has gone out. The Anthropic adapter keeps its own
    /// thread-local, which `crate::openai::reset_stream_state` clears.
    static RESPONSE_STREAM_STATE: RefCell<Option<ResponseStreamState>> = const { RefCell::new(None) };
}

/// Forget the in-flight stream state.
///
/// Call between streams on a reused worker thread. The state self-heals after a terminal frame, but
/// a stream that dies without one leaves partial state behind.
pub fn reset_stream_state() {
    RESPONSE_STREAM_STATE.with(|cell| *cell.borrow_mut() = None);
}

/// What one upstream event contributed, in client-visible terms.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// A text fragment of the answer.
    Text(String),
    /// A summary fragment of the model's reasoning, which Chat Completions spells
    /// `reasoning_content`.
    Reasoning(String),
    /// A function call opened, carrying its id and name.
    ToolOpen {
        index: usize,
        id: String,
        name: String,
        arguments: String,
    },
    /// A fragment of a function call's arguments.
    ToolArgs { index: usize, arguments: String },
    /// The answer ended.
    Finish,
    /// The upstream reported a failure.
    Error(Value),
    /// Nothing the client can use. State may still have been captured.
    Ignored,
}

/// Read the response envelope every Responses event may carry.
fn capture_response(state: &mut ResponseStreamState, response: &Value) {
    if let Some(id) = response
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    {
        state.id = id.to_string();
    }
    if let Some(model) = response
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
    {
        state.model = model.to_string();
    }
    if let Some(created) = response.get("created_at").and_then(Value::as_i64) {
        state.created = created;
    }
    if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
        state.prompt_tokens = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(state.prompt_tokens);
        state.completion_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(state.completion_tokens);
        state.cached_tokens = usage
            .get("input_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(state.cached_tokens);
        state.usage_seen = true;
    }
}

/// The slot in `tool_calls` an argument fragment belongs to, opening one if the item is new.
///
/// Keyed by the item id rather than by `output_index`, because the index a Chat Completions client
/// needs counts function calls only — a message item at `output_index: 0` must not push the first
/// tool call to index 1.
fn tool_slot(state: &mut ResponseStreamState, event: &Value) -> usize {
    let key = event
        .get("item_id")
        .and_then(Value::as_str)
        .or_else(|| event.get("call_id").and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_else(|| format!("output_{}", output_index(event)));
    if let Some(&index) = state.tool_index.get(&key) {
        return index;
    }
    state.tool_calls.push(ToolCall {
        id: key.clone(),
        ..ToolCall::default()
    });
    let index = state.tool_calls.len() - 1;
    state.tool_index.insert(key, index);
    index
}

/// Classify one upstream event and fold what it carries into the stream state.
fn classify(state: &mut ResponseStreamState, event: &Value) -> Step {
    if let Some(response) = event.get("response") {
        capture_response(state, response);
    }
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "response.output_text.delta" => Step::Text(
            event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            Step::Reasoning(
                event
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            )
        }
        "response.output_item.added" | "response.output_item.done" => {
            let Some(item) = event.get("item") else {
                return Step::Ignored;
            };
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Step::Ignored;
            }
            let item_id = item
                .get("id")
                .and_then(Value::as_str)
                .or_else(|| item.get("call_id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .or_else(|| item.get("id").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let index = match state.tool_index.get(&item_id) {
                Some(&index) => index,
                None => {
                    state.tool_calls.push(ToolCall {
                        id: call_id.clone(),
                        name: name.clone(),
                        arguments: String::new(),
                        announced: false,
                    });
                    let index = state.tool_calls.len() - 1;
                    state.tool_index.insert(item_id, index);
                    index
                }
            };
            let call = &mut state.tool_calls[index];
            if call.id.is_empty() {
                call.id = call_id;
            }
            if call.name.is_empty() {
                call.name = name;
            }
            // `added` announces the call with its name; `done` only tops up arguments that never
            // arrived as deltas. A call whose name was unknown when its arguments started is
            // announced late rather than left nameless.
            if !call.announced && (kind == "response.output_item.added" || !call.name.is_empty()) {
                call.announced = true;
                return Step::ToolOpen {
                    index,
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: String::new(),
                };
            }
            if call.arguments.is_empty() && !arguments.is_empty() {
                call.arguments = arguments.clone();
                return Step::ToolArgs { index, arguments };
            }
            Step::Ignored
        }
        "response.function_call_arguments.delta" => {
            let arguments = event
                .get("delta")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if arguments.is_empty() {
                return Step::Ignored;
            }
            let index = tool_slot(state, event);
            state.tool_calls[index].arguments.push_str(arguments);
            Step::ToolArgs {
                index,
                arguments: arguments.to_string(),
            }
        }
        "response.function_call_arguments.done" => {
            let arguments = event
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let index = tool_slot(state, event);
            let call = &mut state.tool_calls[index];
            if call.arguments.is_empty() && !arguments.is_empty() {
                call.arguments = arguments.to_string();
                return Step::ToolArgs {
                    index,
                    arguments: arguments.to_string(),
                };
            }
            if arguments.len() > call.arguments.len() {
                // The `done` event is authoritative and longer than what the deltas spelled.
                call.arguments = arguments.to_string();
            }
            Step::Ignored
        }
        "response.completed" | "response.incomplete" | "response.failed" => {
            if kind == "response.failed" {
                if let Some(error) = event.get("response").and_then(|r| r.get("error")) {
                    return Step::Error(error.clone());
                }
            }
            let response = event.get("response").unwrap_or(event);
            state.finish_reason = finish_reason(response).to_string();
            Step::Finish
        }
        "error" => Step::Error(event.clone()),
        _ => Step::Ignored,
    }
}

/// Add `role: "assistant"` to the first delta of a stream, and to no other.
fn with_role(state: &mut ResponseStreamState, mut delta: Value) -> Value {
    if !state.sent_role {
        if let Some(obj) = delta.as_object_mut() {
            obj.insert("role".into(), json!("assistant"));
        }
        state.sent_role = true;
    }
    delta
}

/// A `chat.completion.chunk` carrying one delta.
fn pivot_chunk(state: &ResponseStreamState, delta: Value) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "created": state.created,
        "model": state.model,
        "choices": [{"index": 0, "delta": delta}],
    })
}

/// The terminal chunk: an empty delta carrying the finish reason.
///
/// Responses reports usage on the object every terminal event carries, so it is carried here on
/// the terminal chunk — the shape OpenAI-compatible providers use, and the one
/// [`sse_to_anthropic`] reads `prompt_tokens` from.
fn final_chunk(state: &ResponseStreamState) -> Value {
    let mut value = json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "created": state.created,
        "model": state.model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": state.finish_reason}],
    });
    if state.usage_seen {
        value["usage"] = json!({
            "prompt_tokens": state.prompt_tokens,
            "completion_tokens": state.completion_tokens,
            "total_tokens": state.prompt_tokens + state.completion_tokens,
            "prompt_tokens_details": {"cached_tokens": state.cached_tokens},
        });
    }
    value
}

/// An upstream error in the client's own error shape.
fn error_to_client(error: &Value) -> Value {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the upstream stream failed");
    let kind = match error.get("type").and_then(Value::as_str) {
        Some(kind) if kind != "error" => kind,
        _ => "upstream_error",
    };
    json!({
        "error": {
            "message": message,
            "type": kind,
            "code": error.get("code").cloned().unwrap_or(Value::Null),
        }
    })
}

/// Frame already-built pivot chunks, and close the client's stream the way its dialect requires.
fn frames_for(ctx: &TranslateCtx<'_>, chunks: Vec<Value>, terminate: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for value in chunks {
        let payload = value.to_string();
        match ctx.client_dialect {
            Dialect::Anthropic => out.extend(sse_to_anthropic(ctx, &payload)?),
            _ => out.push(sse::data_frame(&payload)),
        }
    }
    if terminate {
        match ctx.client_dialect {
            // Anthropic terminates on `message_stop`, which the adapter produces from the pivot's
            // own `[DONE]` sentinel; the sentinel itself is never forwarded.
            Dialect::Anthropic => out.extend(sse_to_anthropic(ctx, SSE_DONE)?),
            // Only a Chat Completions client sees `[DONE]`. A Responses client terminates on
            // `response.completed` and takes the pass-through arm above, so it never reaches here.
            _ => out.push(sse::data_frame(SSE_DONE)),
        }
    }
    Ok(out)
}

/// Translate one Responses SSE payload into client-dialect frames.
///
/// Three client shapes pass through here and the terminator is where they diverge:
///
/// * a **Responses client** already speaks this protocol, so frames ride through untouched and
///   `[DONE]` is suppressed — Responses ends on `response.completed`;
/// * a **Chat Completions client** gets `chat.completion.chunk` frames followed by `[DONE]`;
/// * an **Anthropic client** gets the same chunks through the Anthropic adapter, which turns the
///   `[DONE]` sentinel into `message_delta` + `message_stop` instead of forwarding it.
pub fn sse_to_pivot(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    let trimmed = data.trim();
    // The relay strips the `data:` prefix before calling this, but a raw NDJSON line would not
    // have one, and tolerating both costs nothing.
    let candidate = trimmed.strip_prefix("data:").unwrap_or(trimmed).trim();
    if candidate.is_empty() {
        return Ok(Vec::new());
    }
    if ctx.route == RouteKind::Responses {
        if candidate == SSE_DONE {
            return Ok(Vec::new());
        }
        return Ok(vec![candidate.to_owned()]);
    }

    RESPONSE_STREAM_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if candidate == SSE_DONE {
            // The upstream is not expected to send a sentinel of its own; if it does, close the
            // answer rather than dropping it, so the client still gets its finish reason.
            return match slot.as_mut().filter(|state| !state.done) {
                Some(state) => {
                    state.finish_reason = "stop".to_string();
                    state.done = true;
                    let mut chunks = Vec::new();
                    if !state.sent_role {
                        let delta = with_role(state, json!({}));
                        chunks.push(pivot_chunk(state, delta));
                    }
                    chunks.push(final_chunk(state));
                    frames_for(ctx, chunks, true)
                }
                None => Ok(Vec::new()),
            };
        }
        if slot.as_ref().map(|state| state.done).unwrap_or(false) {
            *slot = None;
        }
        let state = slot.get_or_insert_with(|| ResponseStreamState::new(ctx));
        let Ok(event) = serde_json::from_str::<Value>(candidate) else {
            // A keep-alive or a line the upstream never intended as an event: skipped, never
            // fatal, because one bad frame must not kill an answer already half-delivered.
            return Ok(Vec::new());
        };

        let mut chunks: Vec<Value> = Vec::new();
        let mut terminate = false;
        match classify(state, &event) {
            Step::Text(text) => {
                if !text.is_empty() {
                    let delta = with_role(state, json!({"content": text}));
                    chunks.push(pivot_chunk(state, delta));
                }
            }
            Step::Reasoning(text) => {
                if !text.is_empty() {
                    let delta = with_role(state, json!({"reasoning_content": text}));
                    chunks.push(pivot_chunk(state, delta));
                }
            }
            Step::ToolOpen {
                index,
                id,
                name,
                arguments,
            } => {
                let delta = with_role(
                    state,
                    json!({"tool_calls": [{
                        "index": index,
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }]}),
                );
                chunks.push(pivot_chunk(state, delta));
            }
            Step::ToolArgs { index, arguments } => {
                let delta = with_role(
                    state,
                    json!({"tool_calls": [{
                        "index": index,
                        "function": {"arguments": arguments},
                    }]}),
                );
                chunks.push(pivot_chunk(state, delta));
            }
            Step::Finish => {
                // An answer that produced no delta at all still gets a role chunk, so an Anthropic
                // client has a `message_start` to hang its `message_stop` on.
                if !state.sent_role {
                    let delta = with_role(state, json!({}));
                    chunks.push(pivot_chunk(state, delta));
                }
                state.done = true;
                chunks.push(final_chunk(state));
                terminate = true;
            }
            Step::Error(error) => {
                state.done = true;
                let body = error_to_client(&error);
                return Ok(match ctx.client_dialect {
                    Dialect::Anthropic => vec![sse::named_frame("error", &body.to_string())],
                    _ => vec![
                        sse::data_frame(&body.to_string()),
                        sse::data_frame(SSE_DONE),
                    ],
                });
            }
            Step::Ignored => {}
        }
        frames_for(ctx, chunks, terminate)
    })
}
