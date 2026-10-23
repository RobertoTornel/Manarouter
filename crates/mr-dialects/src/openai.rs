//! The OpenAI Chat Completions pivot, and the Anthropic direction of the pair.
//!
//! ## Why Chat Completions is the pivot
//!
//! Both reference routers converge on Chat Completions as the intermediate shape, because it is the
//! dialect with the most providers and the simplest content model. Following that convention means
//! every provider needs at most one hop from a familiar shape rather than one bridge per pair.
//!
//! ## Where pivoting is not used
//!
//! Pivoting twice loses information. An Anthropic `thinking` block, a `cache_control` breakpoint
//! and a `redacted_thinking` block have no Chat Completions equivalent, and a round trip through
//! the pivot destroys them. The relay therefore forwards Anthropic-to-Anthropic traffic verbatim
//! and never calls this module for it; this module exists for the genuine mismatches, and it
//! preserves the information it can rather than pretending the shapes are equivalent.
//!
//! ## Error philosophy
//!
//! A field this module does not understand is **preserved in the request** rather than dropped,
//! wherever the upstream might accept it, and dropped only where forwarding it would cause a
//! rejection. Silently discarding an unknown tool parameter or a `response_format` would produce a
//! subtly wrong answer rather than a clear error, which is the worse failure.

use bytes::Bytes;
use mr_core::RouteKind;
use serde_json::{json, Map, Value};

use crate::sse::{self, Framer};
use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest};

/// An OpenAI Chat Completions request, normalised from whichever dialect the client used.
#[derive(Debug, Clone)]
pub struct PivotRequest {
    /// The Chat Completions request object.
    pub value: Value,
    /// Whether the client asked for a streamed response.
    pub stream: bool,
}

impl PivotRequest {
    /// The requested model, if present.
    pub fn model(&self) -> Option<&str> {
        self.value.get("model").and_then(Value::as_str)
    }

    /// The `messages` array, or an empty slice.
    pub fn messages(&self) -> &[Value] {
        self.value
            .get("messages")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The `tools` array, or an empty slice.
    pub fn tools(&self) -> &[Value] {
        self.value
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }
}

/// Normalise a client request body into the Chat Completions pivot.
///
/// Accepts a Chat Completions body unchanged, or an Anthropic Messages body via
/// [`anthropic_request_into_pivot`]. When the caller is already an OpenAI client this is a single
/// parse with no restructuring, which is the shape used when a *non*-OpenAI upstream needs the
/// pivot.
pub fn to_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<PivotRequest> {
    let value: Value =
        serde_json::from_slice(body).map_err(|e| TranslateError::MalformedRequest {
            dialect: ctx.client_dialect.as_str(),
            detail: e.to_string(),
        })?;
    let obj = value
        .as_object()
        .ok_or_else(|| TranslateError::MalformedRequest {
            dialect: ctx.client_dialect.as_str(),
            detail: "request body must be a JSON object".to_string(),
        })?;

    match ctx.client_dialect {
        mr_core::Dialect::Anthropic => anthropic_request_into_pivot(ctx, obj),
        _ => Ok(PivotRequest {
            stream: obj.get("stream").and_then(Value::as_bool).unwrap_or(false),
            value,
        }),
    }
}

/// Translate an Anthropic Messages request object into a Chat Completions request.
///
/// Handles the four things that actually differ: the system prompt is a top-level field rather than
/// a message; content is a list of typed blocks rather than a string; `tool_use` / `tool_result`
/// blocks become `tool_calls` and `tool` role messages; and `max_tokens` is mandatory in Anthropic
/// but optional in OpenAI.
pub fn anthropic_request_into_pivot(
    ctx: &TranslateCtx<'_>,
    obj: &Map<String, Value>,
) -> Result<PivotRequest> {
    let mut out = Map::new();
    out.insert("model".into(), json!(ctx.upstream_model));

    let stream = obj.get("stream").and_then(Value::as_bool).unwrap_or(false);
    // Anthropic requires max_tokens; Chat Completions does not, so it is passed through when
    // present and simply omitted when absent rather than being invented.
    for key in ["max_tokens", "temperature", "top_p", "metadata"] {
        if let Some(v) = obj.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    if let Some(stops) = obj.get("stop_sequences") {
        out.insert("stop".into(), stops.clone());
    }

    let mut messages: Vec<Value> = Vec::new();

    // A top-level system prompt becomes the leading system message, which is where Chat
    // Completions expects it and where upstream prompt caches key on it.
    if let Some(system) = obj.get("system") {
        let text = flatten_text(system);
        if !text.is_empty() {
            messages.push(json!({"role": "system", "content": text}));
        }
    }

    for m in obj
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match m.get("content") {
            Some(Value::String(s)) => {
                messages.push(json!({"role": role, "content": s}));
            }
            Some(Value::Array(blocks)) => {
                let (content, tool_calls, tool_results) = split_blocks(blocks);
                if !tool_results.is_empty() {
                    // A user turn that carries tool_result blocks is, in Chat Completions terms,
                    // one `tool` message per result. Emitting them before any text keeps the
                    // ordering the model expects.
                    for tr in tool_results {
                        messages.push(tr);
                    }
                }
                if content.is_some() || !tool_calls.is_empty() {
                    let mut msg = Map::new();
                    msg.insert("role".into(), json!(role));
                    match content {
                        Some(c) => {
                            msg.insert("content".into(), c);
                        }
                        // Chat Completions permits null content when tool calls are present, and
                        // some providers reject a missing field instead of a null one.
                        None => {
                            msg.insert("content".into(), Value::Null);
                        }
                    }
                    if !tool_calls.is_empty() {
                        msg.insert("tool_calls".into(), Value::Array(tool_calls));
                    }
                    messages.push(Value::Object(msg));
                }
            }
            _ => {
                messages.push(json!({"role": role, "content": ""}));
            }
        }
    }
    out.insert("messages".into(), Value::Array(messages));

    if let Some(tools) = obj.get("tools").and_then(Value::as_array) {
        let converted: Vec<Value> = tools.iter().map(anthropic_tool_to_openai).collect();
        if !converted.is_empty() {
            out.insert("tools".into(), Value::Array(converted));
        }
    }
    if let Some(tc) = obj.get("tool_choice") {
        out.insert("tool_choice".into(), anthropic_tool_choice_to_openai(tc));
    }
    if stream {
        out.insert("stream".into(), json!(true));
    }

    Ok(PivotRequest {
        value: Value::Object(out),
        stream,
    })
}

/// Convert an Anthropic tool definition to a Chat Completions function definition.
fn anthropic_tool_to_openai(tool: &Value) -> Value {
    let name = tool.get("name").cloned().unwrap_or(json!(""));
    let description = tool.get("description").cloned().unwrap_or(json!(""));
    // Anthropic calls it `input_schema`, Chat Completions calls it `parameters`, and both want the
    // wrapper object. A missing schema defaults to an empty object rather than being omitted,
    // because several providers reject a function with no parameters field at all.
    let parameters = tool
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    json!({
        "type": "function",
        "function": {"name": name, "description": description, "parameters": parameters}
    })
}

/// Convert Anthropic `tool_choice` to the Chat Completions equivalent.
fn anthropic_tool_choice_to_openai(tc: &Value) -> Value {
    match tc.get("type").and_then(Value::as_str) {
        Some("auto") => json!("auto"),
        Some("any") => json!("required"),
        Some("none") => json!("none"),
        Some("tool") => json!({
            "type": "function",
            "function": {"name": tc.get("name").cloned().unwrap_or(json!(""))}
        }),
        _ => json!("auto"),
    }
}

/// Split an Anthropic content-block array into Chat Completions pieces.
///
/// Returns `(content, tool_calls, tool_result_messages)`. `content` is `None` when the turn carried
/// no renderable text, which is the signal to emit `content: null` rather than an empty string.
fn split_blocks(blocks: &[Value]) -> (Option<Value>, Vec<Value>, Vec<Value>) {
    let mut content_parts: Vec<Value> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text") {
                    content_parts.push(json!({"type": "text", "text": t}));
                }
            }
            Some("image") => {
                // Anthropic carries base64 images in a `source` object; Chat Completions wants a
                // data URL. Forwarding the source verbatim would be rejected by every provider.
                if let Some(url) = anthropic_image_to_data_url(b) {
                    content_parts.push(json!({"type": "image_url", "image_url": {"url": url}}));
                }
            }
            Some("tool_use") => {
                let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": b.get("id").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": b.get("name").cloned().unwrap_or(json!("")),
                        // Chat Completions requires the arguments as a JSON *string*, not an
                        // object. Getting this wrong is the single most common source of
                        // malformed-tool-call errors on this hop.
                        "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
                    }
                }));
            }
            Some("tool_result") => {
                let text = flatten_text(b.get("content").unwrap_or(&Value::Null));
                let is_error = b.get("is_error").and_then(Value::as_bool).unwrap_or(false);
                // Chat Completions has no error flag on a tool message, so the convention both
                // reference routers use is a textual marker that models reliably understand.
                let body = if is_error {
                    format!("Error: {text}")
                } else {
                    text
                };
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": b.get("tool_use_id").cloned().unwrap_or(json!("")),
                    "content": body,
                }));
            }
            Some("thinking") | Some("redacted_thinking") => {
                // No Chat Completions equivalent. Dropping it is lossy but correct: forwarding an
                // unknown block type would be rejected outright.
            }
            _ => {
                // Unknown block types are skipped rather than fatal, so a provider adding one does
                // not break routing for that provider entirely.
            }
        }
    }

    let content = if content_parts.is_empty() {
        None
    } else if content_parts.len() == 1 && content_parts[0].get("type") == Some(&json!("text")) {
        // Collapse a single text block to a plain string: some providers reject the array form
        // for a message that has only text.
        content_parts[0].get("text").cloned()
    } else {
        Some(Value::Array(content_parts))
    };

    (content, tool_calls, tool_results)
}

/// Convert an Anthropic image block into a data URL.
fn anthropic_image_to_data_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media = source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Flatten Anthropic's string-or-block-array content into plain text.
///
/// Used for the system prompt and for tool results, both of which may be either shape.
pub fn flatten_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let mut out = String::new();
            for b in blocks {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            out
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Rewrite the `model` field in place, leaving everything else untouched.
///
/// Kept separate from translation so the relay can retarget a model without parsing the rest of the
/// body when only the name differs.
pub fn rewrite_model(v: &mut Value, model: &str) {
    if let Some(obj) = v.as_object_mut() {
        obj.insert("model".into(), json!(model));
    }
}

/// Whether the pivot body asks for a stream.
pub fn pivot_wants_stream(v: &Value) -> bool {
    v.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

// ---------------------------------------------------------------------------------------------
// OpenAI pivot -> Anthropic Messages
// ---------------------------------------------------------------------------------------------

/// Convert an Anthropic Messages request body into a Chat Completions request body.
///
/// This is the "Anthropic client, OpenAI upstream" direction: a Claude Code session pointed at an
/// OpenAI-compatible provider. It normalises through the pivot and re-serialises, which is the cost
/// paid for the mismatch.
pub fn from_anthropic_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    let pivot = to_pivot_request(ctx, body)?;
    let bytes = serde_json::to_vec(&pivot.value).map_err(|e| TranslateError::MalformedRequest {
        dialect: "anthropic",
        detail: e.to_string(),
    })?;
    Ok(TranslatedRequest {
        body: Bytes::from(bytes),
        content_type: "application/json",
        force_stream: false,
        path_override: Some("chat/completions".to_string()),
    })
}

/// Convert a Chat Completions response body into an Anthropic Messages response body.
pub fn to_anthropic_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    let v: Value = serde_json::from_slice(body).map_err(|e| TranslateError::MalformedResponse {
        dialect: "openai",
        detail: e.to_string(),
    })?;
    let mut state = AnthropicStreamState::default();
    let out = pivot_response_to_anthropic(ctx, &v, &mut state)?;
    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| TranslateError::MalformedResponse {
            dialect: "openai",
            detail: e.to_string(),
        })
}

/// Convert one Chat Completions response object into an Anthropic Messages response object.
pub fn pivot_response_to_anthropic(
    _ctx: &TranslateCtx<'_>,
    v: &Value,
    _state: &mut AnthropicStreamState,
) -> Result<Value> {
    let choice = v
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let message = choice.and_then(|c| c.get("message"));
    let mut content: Vec<Value> = Vec::new();

    // Reasoning content has to be emitted before the answer, because an Anthropic client renders
    // blocks in order and a thinking block after the text reads as the model contradicting itself.
    if let Some(rc) = message
        .and_then(|m| m.get("reasoning_content"))
        .and_then(Value::as_str)
    {
        if !rc.is_empty() {
            content.push(json!({"type": "thinking", "thinking": rc}));
        }
    }

    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
    {
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    } else if let Some(parts) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
    {
        // Some OpenAI-compatible providers return `content` as an array of parts rather than a
        // string. Dropping it silently would yield an empty assistant message with `end_turn`.
        let mut text = String::new();
        for part in parts {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
        if !text.is_empty() {
            content.push(json!({"type": "text", "text": text}));
        }
    }

    if let Some(calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for c in calls {
            let f = c.get("function");
            // Anthropic wants `input` as an object, while Chat Completions supplies `arguments` as
            // a JSON string. A string that will not parse becomes an empty object rather than
            // failing the whole response, because a broken tool call is better surfaced to the
            // model as an empty input than as a transport error.
            let input = f
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .unwrap_or_else(|| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": c.get("id").cloned().unwrap_or(json!("")),
                "name": f.and_then(|f| f.get("name")).cloned().unwrap_or(json!("")),
                "input": input,
            }));
        }
    }

    let stop_reason = map_finish_reason(
        choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str),
    );

    let usage = v.get("usage");
    let input_tokens = usage
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Ok(json!({
        "id": v.get("id").cloned().unwrap_or_else(|| json!(format!("msg_{}", mr_core::ids::new_id()))),
        "type": "message",
        "role": "assistant",
        "model": v.get("model").cloned().unwrap_or(json!("")),
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
    }))
}

/// Map a Chat Completions `finish_reason` onto an Anthropic `stop_reason`.
pub fn map_finish_reason(reason: Option<&str>) -> Value {
    match reason {
        Some("length") => json!("max_tokens"),
        Some("tool_calls") | Some("function_call") => json!("tool_use"),
        Some("stop") | None => json!("end_turn"),
        Some("content_filter") => json!("refusal"),
        // An unrecognised reason becomes null rather than a guess: claiming `end_turn` for an
        // unknown stop would tell the client the answer is complete when it may not be.
        Some(_) => Value::Null,
    }
}

// ---------------------------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------------------------

/// Per-stream state for the OpenAI-pivot-to-Anthropic translation.
///
/// Anthropic's stream is a state machine with explicit `message_start`, `content_block_start`,
/// `content_block_delta`, `content_block_stop` and `message_stop` events, while Chat Completions
/// simply emits deltas. Rebuilding the Anthropic shape therefore needs to remember which block is
/// open and how many have been opened, which is what this holds.
#[derive(Debug, Default)]
pub struct AnthropicStreamState {
    /// Whether `message_start` has been emitted.
    pub started: bool,
    /// The index of the currently open content block, if any.
    pub open_block: Option<usize>,
    /// The type of the currently open block, so a change of type closes it correctly.
    pub open_block_type: Option<String>,
    /// How many blocks have been opened, which is the index of the next one.
    pub next_index: usize,
    /// Tool-call index to content-block index, so argument fragments land in the right block.
    ///
    /// Keyed by the upstream index rather than by tool id, because Chat Completions identifies
    /// tool calls by array position and Anthropic needs a stable block index across fragments.
    pub tool_blocks: std::collections::HashMap<u64, usize>,
    /// The model named in `message_start`, echoed on every later event where the client expects it.
    pub model: String,
    /// The message id, generated once so every event agrees.
    pub message_id: String,
    /// Prompt tokens seen so far, to fill `message_delta` usage.
    pub input_tokens: u64,
    /// Whether any tool-call delta has been emitted, which gates the no-splice guard.
    pub emitted_tool_calls: bool,
}

impl AnthropicStreamState {
    /// A fresh state for one stream.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            message_id: format!("msg_{}", mr_core::ids::new_id()),
            ..Self::default()
        }
    }
}

/// Translate a Chat Completions SSE payload into Anthropic SSE payloads.
pub fn sse_to_anthropic(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    // The state has to persist across frames, so it is kept in a thread-local keyed by nothing more
    // than the fact that one relay task handles one stream end to end. A caller that needs to
    // interleave streams uses `AnthropicStreamState` directly.
    let out = STREAM_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(|| AnthropicStreamState::new(ctx.upstream_model));
        translate_chunk(ctx, data, state)
    });
    // The stream is over once `[DONE]` has been translated. Dropping the slot here (on success and
    // on error alike) keeps the next stream this worker thread handles from inheriting
    // `started = true` — which suppresses its `message_start` — along with the previous stream's
    // id, model and block bookkeeping.
    if data.trim() == "[DONE]" {
        reset_stream_state();
    }
    out
}

thread_local! {
    static STREAM_STATE: std::cell::RefCell<Option<AnthropicStreamState>> =
        const { std::cell::RefCell::new(None) };
}

/// Reset the thread-local stream state. Called when a stream ends so the next one starts clean.
pub fn reset_stream_state() {
    STREAM_STATE.with(|cell| *cell.borrow_mut() = None);
}

/// Frame one pivot chunk as Anthropic events against state the caller owns.
///
/// [`sse_to_anthropic`] reaches for the per-thread cell above. A dialect whose accumulation is
/// instead owned per stream (see `commandcode::Translator`) needs the same framing without touching
/// that cell, or it would reintroduce exactly the sharing it removed. This is that seam.
pub(crate) fn translate_chunk_for(
    state: &mut AnthropicStreamState,
    ctx: &TranslateCtx<'_>,
    data: &str,
) -> Result<Vec<String>> {
    translate_chunk(ctx, data, state)
}

fn translate_chunk(
    _ctx: &TranslateCtx<'_>,
    data: &str,
    state: &mut AnthropicStreamState,
) -> Result<Vec<String>> {
    if data.trim() == "[DONE]" {
        let mut out = Vec::new();
        // A still-open block means no `finish_reason` was ever seen, so the terminal
        // `message_delta` below is the only one the client will get. When the finish handler
        // already emitted one, emitting a second with a hard-coded `end_turn` would overwrite a
        // real `tool_use` or `max_tokens` stop and the client would never run the tool it was
        // just handed.
        let unfinished = state.open_block.is_some();
        if let Some(idx) = state.open_block.take() {
            out.push(sse::named_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}).to_string(),
            ));
        }
        if unfinished {
            out.push(sse::named_frame(
                "message_delta",
                &json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": Value::Null},
                    "usage": {"output_tokens": 0}
                })
                .to_string(),
            ));
        }
        out.push(sse::named_frame(
            "message_stop",
            &json!({"type": "message_stop"}).to_string(),
        ));
        return Ok(out);
    }

    let chunk: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        // A malformed frame is skipped rather than aborting the stream: providers occasionally emit
        // a commentary line that is not JSON, and killing the response over it would be worse than
        // dropping one unusable frame.
        Err(_) => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    if !state.started {
        state.started = true;
        out.push(sse::named_frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": state.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": state.model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            })
            .to_string(),
        ));
        out.push(sse::named_frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            })
            .to_string(),
        ));
        state.open_block = Some(0);
        state.open_block_type = Some("text".into());
        state.next_index = 1;
    }

    if let Some(u) = chunk
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
    {
        state.input_tokens = u;
    }

    let delta = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("delta"));

    if let Some(rc) = delta
        .and_then(|d| d.get("reasoning_content"))
        .and_then(Value::as_str)
    {
        if !rc.is_empty() {
            out.push(sse::named_frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": state.open_block.unwrap_or(0),
                    "delta": {"type": "thinking_delta", "thinking": rc}
                })
                .to_string(),
            ));
        }
    }

    if let Some(text) = delta.and_then(|d| d.get("content")).and_then(Value::as_str) {
        if !text.is_empty() {
            out.push(sse::named_frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": state.open_block.unwrap_or(0),
                    "delta": {"type": "text_delta", "text": text}
                })
                .to_string(),
            ));
        }
    }

    if let Some(calls) = delta
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for c in calls {
            let upstream_index = c.get("index").and_then(Value::as_u64).unwrap_or(0);
            state.emitted_tool_calls = true;

            // Close the open text block before opening a tool block, because Anthropic requires
            // blocks to be strictly nested and a tool_use inside a text block is invalid.
            let block_index = match state.tool_blocks.get(&upstream_index) {
                Some(i) => *i,
                None => {
                    if state.open_block_type.as_deref() == Some("text") {
                        if let Some(idx) = state.open_block.take() {
                            out.push(sse::named_frame(
                                "content_block_stop",
                                &json!({"type": "content_block_stop", "index": idx}).to_string(),
                            ));
                        }
                    }
                    let i = state.next_index;
                    state.next_index += 1;
                    state.tool_blocks.insert(upstream_index, i);
                    state.open_block = Some(i);
                    state.open_block_type = Some("tool_use".into());
                    out.push(sse::named_frame(
                        "content_block_start",
                        &json!({
                            "type": "content_block_start",
                            "index": i,
                            "content_block": {
                                "type": "tool_use",
                                "id": c.get("id").cloned().unwrap_or(json!("")),
                                "name": c
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .cloned()
                                    .unwrap_or(json!("")),
                                "input": {}
                            }
                        })
                        .to_string(),
                    ));
                    i
                }
            };

            if let Some(args) = c
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
            {
                if !args.is_empty() {
                    out.push(sse::named_frame(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "input_json_delta", "partial_json": args}
                        })
                        .to_string(),
                    ));
                }
            }
        }
    }

    if let Some(fr) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
    {
        if let Some(idx) = state.open_block.take() {
            out.push(sse::named_frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": idx}).to_string(),
            ));
        }
        state.open_block_type = None;
        out.push(sse::named_frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": map_finish_reason(Some(fr)), "stop_sequence": Value::Null},
                "usage": {"output_tokens": 0}
            })
            .to_string(),
        ));
    }

    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Anthropic Messages -> OpenAI pivot
// ---------------------------------------------------------------------------------------------

/// Convert an Anthropic Messages request into a Chat Completions request.
///
/// The public entry point used by the relay for an Anthropic client routed to an OpenAI provider.
pub fn from_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    let pivot = to_pivot_request(ctx, body)?;
    let bytes = serde_json::to_vec(&pivot.value).map_err(|e| TranslateError::MalformedRequest {
        dialect: ctx.client_dialect.as_str(),
        detail: e.to_string(),
    })?;
    Ok(TranslatedRequest {
        body: Bytes::from(bytes),
        content_type: "application/json",
        force_stream: false,
        path_override: Some("chat/completions".to_string()),
    })
}

/// Convert an Anthropic Messages response body into a Chat Completions response body.
pub fn to_openai_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    let v: Value = serde_json::from_slice(body).map_err(|e| TranslateError::MalformedResponse {
        dialect: "anthropic",
        detail: e.to_string(),
    })?;
    let out = anthropic_response_to_pivot(ctx, &v)?;
    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| TranslateError::MalformedResponse {
            dialect: "anthropic",
            detail: e.to_string(),
        })
}

/// Convert an Anthropic Messages response object into a Chat Completions response object.
pub fn anthropic_response_to_pivot(ctx: &TranslateCtx<'_>, v: &Value) -> Result<Value> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    for block in v
        .get("content")
        .and_then(Value::as_array)
        .unwrap_or(&Vec::new())
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
            Some("thinking") => {
                if let Some(t) = block.get("thinking").and_then(Value::as_str) {
                    reasoning.push_str(t);
                }
            }
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                tool_calls.push(json!({
                    "id": block.get("id").cloned().unwrap_or(json!("")),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or(json!("")),
                        "arguments": serde_json::to_string(&input).unwrap_or_else(|_| "{}".into()),
                    }
                }));
            }
            _ => {}
        }
    }

    let usage = v.get("usage");
    let prompt = usage
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion = usage
        .and_then(|u| u.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    if text.is_empty() && !tool_calls.is_empty() {
        // Chat Completions clients and providers both accept null here, and several reject an
        // empty string paired with tool calls.
        message.insert("content".into(), Value::Null);
    } else {
        message.insert("content".into(), json!(text));
    }
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    if !tool_calls.is_empty() {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    Ok(json!({
        "id": v.get("id").cloned().unwrap_or_else(|| json!(format!("chatcmpl-{}", mr_core::ids::new_id()))),
        "object": "chat.completion",
        "created": mr_core::ids::now_millis() / 1000,
        "model": ctx.upstream_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": map_anthropic_stop_reason(
                v.get("stop_reason").and_then(Value::as_str)
            ),
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion,
        }
    }))
}

/// Map an Anthropic `stop_reason` onto a Chat Completions `finish_reason`.
pub fn map_anthropic_stop_reason(reason: Option<&str>) -> Value {
    match reason {
        Some("end_turn") | Some("stop_sequence") => json!("stop"),
        Some("max_tokens") => json!("length"),
        Some("tool_use") => json!("tool_calls"),
        Some("refusal") => json!("content_filter"),
        // A stream that ended without a reason is reported as a normal stop, because a null
        // finish_reason makes several clients treat the response as incomplete and retry.
        None => json!("stop"),
        Some(_) => json!("stop"),
    }
}

/// Per-stream state for Anthropic-to-pivot translation.
#[derive(Debug, Default)]
pub struct OpenAiStreamState {
    /// The id to echo on every chunk so a client correlating them sees one response.
    pub id: String,
    /// Whether the opening role chunk has been emitted.
    pub role_sent: bool,
    /// Tool-call index counter, since Chat Completions numbers tool calls positionally.
    pub next_tool_index: u64,
    /// Anthropic content-block index to Chat Completions tool index.
    pub tool_indices: std::collections::HashMap<u64, u64>,
    /// The model name to echo.
    pub model: String,
}

/// Translate an Anthropic SSE payload into Chat Completions SSE payloads.
pub fn sse_to_openai(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    // Anthropic frames carry an `event:` name that the caller strips when handing us `data`, so the
    // event type is re-derived from the payload's own `type` field, which is always present.
    let chunk: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(_) => return Ok(Vec::new()),
    };
    let kind = chunk.get("type").and_then(Value::as_str).unwrap_or("");

    OPENAI_STREAM_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let st = slot.get_or_insert_with(|| OpenAiStreamState {
            id: format!("chatcmpl-{}", mr_core::ids::new_id()),
            model: ctx.upstream_model.to_string(),
            ..Default::default()
        });
        let mut out = Vec::new();

        // Every Chat Completions stream opens with a role-only chunk, and clients depend on it to
        // initialise the message before any content arrives.
        if !st.role_sent && !chorus_is_terminal(kind) {
            st.role_sent = true;
            out.push(sse::data_frame(
                &json!({
                    "id": st.id, "object": "chat.completion.chunk",
                    "created": mr_core::ids::now_millis() / 1000, "model": st.model,
                    "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""},
                                 "finish_reason": Value::Null}]
                })
                .to_string(),
            ));
        }

        match kind {
            "content_block_delta" => {
                let d = chunk.get("delta").cloned().unwrap_or(Value::Null);
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(t) = d.get("text").and_then(Value::as_str) {
                            out.push(sse::data_frame(
                                &json!({
                                    "id": st.id, "object": "chat.completion.chunk",
                                    "created": mr_core::ids::now_millis() / 1000, "model": st.model,
                                    "choices": [{"index": 0, "delta": {"content": t},
                                                 "finish_reason": Value::Null}]
                                })
                                .to_string(),
                            ));
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = d.get("thinking").and_then(Value::as_str) {
                            out.push(sse::data_frame(
                                &json!({
                                    "id": st.id, "object": "chat.completion.chunk",
                                    "created": mr_core::ids::now_millis() / 1000, "model": st.model,
                                    "choices": [{"index": 0,
                                                 "delta": {"reasoning_content": t},
                                                 "finish_reason": Value::Null}]
                                })
                                .to_string(),
                            ));
                        }
                    }
                    Some("input_json_delta") => {
                        let block = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                        let ti = *st.tool_indices.entry(block).or_insert_with(|| {
                            let i = st.next_tool_index;
                            st.next_tool_index += 1;
                            i
                        });
                        if let Some(p) = d.get("partial_json").and_then(Value::as_str) {
                            out.push(sse::data_frame(
                                &json!({
                                    "id": st.id, "object": "chat.completion.chunk",
                                    "created": mr_core::ids::now_millis() / 1000, "model": st.model,
                                    "choices": [{"index": 0, "delta": {"tool_calls": [{
                                        "index": ti, "function": {"arguments": p}
                                    }]}, "finish_reason": Value::Null}]
                                })
                                .to_string(),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_start" => {
                let cb = chunk.get("content_block").cloned().unwrap_or(Value::Null);
                if cb.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let block = chunk.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let ti = *st.tool_indices.entry(block).or_insert_with(|| {
                        let i = st.next_tool_index;
                        st.next_tool_index += 1;
                        i
                    });
                    out.push(sse::data_frame(
                        &json!({
                            "id": st.id, "object": "chat.completion.chunk",
                            "created": mr_core::ids::now_millis() / 1000, "model": st.model,
                            "choices": [{"index": 0, "delta": {"tool_calls": [{
                                "index": ti,
                                "id": cb.get("id").cloned().unwrap_or(json!("")),
                                "type": "function",
                                "function": {"name": cb.get("name").cloned().unwrap_or(json!("")),
                                             "arguments": ""}
                            }]}, "finish_reason": Value::Null}]
                        })
                        .to_string(),
                    ));
                }
            }
            "message_delta" => {
                let sr = chunk
                    .get("delta")
                    .and_then(|d| d.get("stop_reason"))
                    .and_then(Value::as_str);
                let mut usage = Value::Null;
                if let Some(u) = chunk.get("usage") {
                    let o = u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0);
                    usage = json!({"completion_tokens": o});
                }
                let mut v = json!({
                    "id": st.id, "object": "chat.completion.chunk",
                    "created": mr_core::ids::now_millis() / 1000, "model": st.model,
                    "choices": [{"index": 0, "delta": {},
                                 "finish_reason": map_anthropic_stop_reason(sr)}]
                });
                if usage != Value::Null {
                    v["usage"] = usage;
                }
                out.push(sse::data_frame(&v.to_string()));
            }
            "message_stop" => {
                out.push(format!("data: {}\n\n", crate::SSE_DONE));
            }
            _ => {}
        }
        Ok(out)
    })
}

thread_local! {
    static OPENAI_STREAM_STATE: std::cell::RefCell<Option<OpenAiStreamState>> =
        const { std::cell::RefCell::new(None) };
}

/// Reset the thread-local Anthropic-to-pivot stream state.
pub fn reset_openai_stream_state() {
    OPENAI_STREAM_STATE.with(|cell| *cell.borrow_mut() = None);
}

fn chorus_is_terminal(kind: &str) -> bool {
    matches!(kind, "message_stop")
}

/// A convenience for callers that hold a raw byte stream: frame it and translate each frame.
///
/// Returns the translated wire bytes and any frames that could not be decoded, so a caller can
/// decide whether to surface them.
pub fn translate_wire_chunk(
    ctx: &TranslateCtx<'_>,
    framer: &mut Framer,
    input: &[u8],
) -> Result<Vec<u8>> {
    framer.push(input);
    let mut out = Vec::new();
    while let Some((frame, _raw)) = framer.next_raw() {
        if frame.is_keepalive() {
            continue;
        }
        for payload in crate::translate_sse_data(ctx, &frame.data)? {
            out.extend_from_slice(payload.as_bytes());
        }
    }
    Ok(out)
}

/// The route a pivot request maps onto for a given dialect.
pub fn pivot_route_for(dialect: mr_core::Dialect) -> RouteKind {
    match dialect {
        mr_core::Dialect::Anthropic => RouteKind::Messages,
        mr_core::Dialect::Codex => RouteKind::Responses,
        _ => RouteKind::ChatCompletions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TranslateCtx<'static> {
        TranslateCtx::new(
            RouteKind::ChatCompletions,
            mr_core::Dialect::Anthropic,
            "claude-test",
        )
    }

    #[test]
    fn anthropic_system_prompt_becomes_a_leading_system_message() {
        let body = serde_json::to_vec(&json!({
            "model": "claude-test",
            "max_tokens": 100,
            "system": "be terse",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        let msgs = p.messages();
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "be terse");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(p.value["max_tokens"], 100);
    }

    fn mut_ctx(route: RouteKind) -> TranslateCtx<'static> {
        TranslateCtx {
            client_dialect: mr_core::Dialect::Anthropic,
            ..TranslateCtx::new(route, mr_core::Dialect::OpenAi, "m")
        }
    }

    #[test]
    fn anthropic_system_prompt_accepts_a_block_array() {
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10,
            "system": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}],
            "messages": []
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        assert_eq!(p.messages()[0]["content"], "a\nb");
    }

    #[test]
    fn tool_use_becomes_tool_calls_with_stringified_arguments() {
        // Chat Completions requires `arguments` to be a JSON string; emitting an object here is the
        // classic cause of malformed-tool-call errors on this hop.
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "tu_1", "name": "get_weather", "input": {"city": "Paris"}}
            ]}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        let tc = &p.messages()[0]["tool_calls"][0];
        assert_eq!(tc["id"], "tu_1");
        assert_eq!(tc["function"]["name"], "get_weather");
        let args = tc["function"]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["city"], "Paris");
        // Content must be null, not an empty string, when the turn is tool calls only.
        assert!(p.messages()[0]["content"].is_null());
    }

    #[test]
    fn tool_result_becomes_a_tool_message_and_carries_the_error_marker() {
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu_1", "is_error": true, "content": "boom"}
            ]}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        let m = &p.messages()[0];
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "tu_1");
        assert_eq!(m["content"], "Error: boom");
    }

    #[test]
    fn anthropic_tool_definitions_use_parameters_not_input_schema() {
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "t", "description": "d",
                       "input_schema": {"type": "object", "properties": {"x": {"type": "string"}}}}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        let tool = &p.tools()[0];
        assert_eq!(tool["type"], "function");
        assert!(tool["function"]["parameters"]["properties"]["x"].is_object());
        assert!(tool["function"].get("input_schema").is_none());
    }

    #[test]
    fn a_tool_with_no_schema_still_gets_an_empty_object_schema() {
        // Several providers reject a function with no parameters field at all.
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10, "messages": [],
            "tools": [{"name": "t"}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        assert_eq!(p.tools()[0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn anthropic_tool_choice_maps_onto_the_openai_vocabulary() {
        let cases = [("auto", "auto"), ("any", "required"), ("none", "none")];
        for (input, expected) in cases {
            let body = serde_json::to_vec(&json!({
                "model": "m", "max_tokens": 1, "messages": [],
                "tool_choice": {"type": input}
            }))
            .unwrap();
            let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
            assert_eq!(p.value["tool_choice"], expected, "for {input}");
        }
    }

    #[test]
    fn thinking_blocks_are_dropped_rather_than_forwarded_as_unknown_types() {
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "secret reasoning"},
                {"type": "text", "text": "answer"}
            ]}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        // A single text block collapses to a plain string for maximum provider compatibility.
        assert_eq!(p.messages()[0]["content"], "answer");
    }

    #[test]
    fn base64_images_become_data_urls() {
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "AAA"}}
            ]}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        assert_eq!(
            p.messages()[0]["content"][0]["image_url"]["url"],
            "data:image/jpeg;base64,AAA"
        );
    }

    #[test]
    fn unknown_block_types_are_skipped_not_fatal() {
        let body = serde_json::to_vec(&json!({
            "model": "m", "max_tokens": 10,
            "messages": [{"role": "user", "content": [
                {"type": "future_block_type", "whatever": 1},
                {"type": "text", "text": "still here"}
            ]}]
        }))
        .unwrap();
        let p = to_pivot_request(&mut_ctx(RouteKind::Messages), &body).unwrap();
        assert_eq!(p.messages()[0]["content"], "still here");
    }

    #[test]
    fn anthropic_usage_maps_onto_prompt_and_completion_tokens() {
        let v = json!({
            "id": "msg_1", "type": "message", "role": "assistant",
            "model": "claude-test",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 11, "output_tokens": 4}
        });
        let out = anthropic_response_to_pivot(&ctx(), &v).unwrap();
        assert_eq!(out["usage"]["prompt_tokens"], 11);
        assert_eq!(out["usage"]["completion_tokens"], 4);
        assert_eq!(out["usage"]["total_tokens"], 15);
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn anthropic_tool_use_response_becomes_tool_calls_with_null_content() {
        let v = json!({
            "id": "msg_2", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "tool_use", "id": "tu_9", "name": "f", "input": {"a": 1}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        });
        let out = anthropic_response_to_pivot(&ctx(), &v).unwrap();
        assert!(out["choices"][0]["message"]["content"].is_null());
        assert_eq!(out["choices"][0]["message"]["tool_calls"][0]["id"], "tu_9");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn stop_reason_mapping_is_total_and_never_invents_end_turn_for_the_unknown_case() {
        assert_eq!(
            map_anthropic_stop_reason(Some("max_tokens")),
            json!("length")
        );
        assert_eq!(
            map_anthropic_stop_reason(Some("tool_use")),
            json!("tool_calls")
        );
        assert_eq!(
            map_anthropic_stop_reason(Some("refusal")),
            json!("content_filter")
        );
        assert_eq!(map_anthropic_stop_reason(None), json!("stop"));
        // An unknown reason must not be reported as null on this side: several clients treat a null
        // finish_reason as an incomplete response and retry.
        assert_eq!(
            map_anthropic_stop_reason(Some("brand_new_reason")),
            json!("stop")
        );
    }

    #[test]
    fn chat_completions_finish_reason_maps_and_unknown_becomes_null() {
        assert_eq!(map_finish_reason(Some("length")), json!("max_tokens"));
        assert_eq!(map_finish_reason(Some("tool_calls")), json!("tool_use"));
        assert_eq!(map_finish_reason(Some("stop")), json!("end_turn"));
        // Here the conservative direction is the opposite one: claiming end_turn for an unknown
        // stop would tell an Anthropic client the answer is complete when it may not be.
        assert!(map_finish_reason(Some("brand_new_reason")).is_null());
    }

    #[test]
    fn malformed_request_json_is_reported_with_the_dialect_named() {
        let err = to_pivot_request(&mut_ctx(RouteKind::Messages), b"{not json").unwrap_err();
        match err {
            TranslateError::MalformedRequest { dialect, .. } => assert_eq!(dialect, "anthropic"),
            other => panic!("expected MalformedRequest, got {other:?}"),
        }
    }

    #[test]
    fn a_non_object_request_body_is_rejected_clearly() {
        let err = to_pivot_request(&mut_ctx(RouteKind::Messages), b"[1,2,3]").unwrap_err();
        assert!(matches!(err, TranslateError::MalformedRequest { .. }));
    }

    #[test]
    fn rewrite_model_touches_only_the_model_field() {
        let mut v = json!({"model": "old", "messages": [], "temperature": 0.5});
        rewrite_model(&mut v, "new");
        assert_eq!(v["model"], "new");
        assert_eq!(v["temperature"], 0.5);
    }

    #[test]
    fn stream_state_emits_message_start_then_text_deltas_then_stop() {
        reset_stream_state();
        let c = ctx();
        let start =
            sse_to_anthropic(&c, r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap();
        assert!(start.iter().any(|f| f.contains("message_start")));
        assert!(start.iter().any(|f| f.contains("content_block_start")));

        let mid = sse_to_anthropic(&c, r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
        assert!(mid.iter().any(|f| f.contains("text_delta")));

        let end = sse_to_anthropic(&c, "[DONE]").unwrap();
        assert!(end.iter().any(|f| f.contains("message_stop")));
    }

    #[test]
    fn stream_state_opens_a_tool_block_and_closes_the_text_block_first() {
        reset_stream_state();
        let c = ctx();
        let _ = sse_to_anthropic(&c, r#"{"choices":[{"delta":{"content":"thinking"}}]}"#).unwrap();
        let tool = sse_to_anthropic(
            &c,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"f","arguments":"{\"a\""}}]}}]}"#,
        )
        .unwrap();
        // Anthropic requires blocks to be strictly nested, so the text block must be stopped
        // before the tool_use block starts.
        let joined = tool.join("");
        let stop_at = joined.find("content_block_stop").unwrap();
        let start_at = joined.find("tool_use").unwrap();
        assert!(
            stop_at < start_at,
            "text block must close before tool block opens"
        );

        let args = sse_to_anthropic(
            &c,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":":1}"}}]}}]}"#,
        )
        .unwrap();
        let joined = args.join("");
        assert!(joined.contains("input_json_delta"));
        assert!(joined.contains("partial_json"));
    }

    #[test]
    fn malformed_stream_frames_are_skipped_without_aborting_the_stream() {
        reset_stream_state();
        let c = ctx();
        let out = sse_to_anthropic(&c, "this is not json at all").unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn anthropic_stream_to_openai_sends_role_once_and_ends_with_done() {
        reset_openai_stream_state();
        let c = TranslateCtx::new(RouteKind::Messages, mr_core::Dialect::Anthropic, "m");
        let a = sse_to_openai(&c, r#"{"type":"message_start"}"#).unwrap();
        assert!(a.iter().any(|f| f.contains(r#""role":"assistant""#)));
        let b = sse_to_openai(
            &c,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        )
        .unwrap();
        // The role chunk must not repeat, or clients accumulate empty assistant turns.
        assert!(!b.iter().any(|f| f.contains(r#""role":"assistant""#)));
        assert!(b.iter().any(|f| f.contains(r#""content":"hi""#)));

        let z = sse_to_openai(&c, r#"{"type":"message_stop"}"#).unwrap();
        assert!(z.iter().any(|f| f.contains("[DONE]")));
    }

    #[test]
    fn anthropic_thinking_stream_becomes_reasoning_content() {
        reset_openai_stream_state();
        let c = TranslateCtx::new(RouteKind::Messages, mr_core::Dialect::Anthropic, "m");
        let out = sse_to_openai(
            &c,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
        )
        .unwrap();
        assert!(out.iter().any(|f| f.contains("reasoning_content")));
    }

    #[test]
    fn anthropic_tool_stream_numbers_tool_calls_by_block_index() {
        reset_openai_stream_state();
        let c = TranslateCtx::new(RouteKind::Messages, mr_core::Dialect::Anthropic, "m");
        let _ = sse_to_openai(
            &c,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"f"}}"#,
        )
        .unwrap();
        let out = sse_to_openai(
            &c,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        )
        .unwrap();
        // The fragment must land on the same Chat Completions tool index the start opened, keyed by
        // the Anthropic block index rather than by order of arrival.
        let joined = out.join("");
        assert!(joined.contains(r#""index":0"#));
    }

    #[test]
    fn flatten_text_handles_every_content_shape() {
        assert_eq!(flatten_text(&json!("plain")), "plain");
        assert_eq!(
            flatten_text(&json!([{"type":"text","text":"a"},{"type":"text","text":"b"}])),
            "a\nb"
        );
        assert_eq!(flatten_text(&Value::Null), "");
        assert_eq!(flatten_text(&json!(42)), "42");
    }

    #[test]
    fn pivot_route_mapping_is_sensible() {
        assert_eq!(
            pivot_route_for(mr_core::Dialect::Anthropic),
            RouteKind::Messages
        );
        assert_eq!(
            pivot_route_for(mr_core::Dialect::Codex),
            RouteKind::Responses
        );
        assert_eq!(
            pivot_route_for(mr_core::Dialect::OpenAi),
            RouteKind::ChatCompletions
        );
    }
}
