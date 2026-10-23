//! Gemini `generateContent`, both directions against the Chat Completions pivot.
//!
//! The request half turns a Chat Completions body into Gemini's `contents`/`parts` document and
//! names the URL; the response half turns Gemini's candidates back into pivot JSON. Three shapes
//! make this dialect different from every other one here, and each is a place a naive
//! translation breaks:
//!
//! * **The model id is not in the body.** Gemini addresses models through the URL
//!   (`models/{model}:generateContent`), so `path_override` carries it and the body carries no
//!   `model` field at all. The most common integration mistake is putting the id in the body and
//!   letting the URL fall back to a default model.
//! * **A conversation is one array of turns with typed parts,** not alternating messages: the
//!   system prompt is hoisted to a top-level `systemInstruction`, assistant turns become `model`
//!   turns, and a tool result is a *user* turn holding a `functionResponse` keyed by function
//!   name — Gemini has never heard of `tool_call_id`.
//! * **Cloud Code wraps everything.** The Antigravity `v1internal` endpoint answers with
//!   `{"response": {...}, "traceId": ...}` instead of the bare object the public API returns, so
//!   responses are unwrapped before anything is read, and requests are wrapped the same way when
//!   the upstream dialect is [`mr_core::Dialect::Antigravity`].
//!
//! Two rules the tests pin down: an empty `contents` array is illegal (`Content.parts must not be
//! empty`), so a request whose messages contribute nothing still emits one empty user turn; and
//! malformed frames on the stream are skipped, never fatal — one bad frame must not kill a
//! response already half-delivered.
//!
//! Wire spelling: what we *emit* is Gemini's documented camelCase (`functionCall`,
//! `usageMetadata`, …); what we *read* accepts camelCase first and snake_case as a fallback,
//! because several official SDK serialisers produce the snake spelling for the same fields.

use std::cell::RefCell;
use std::collections::HashMap;

use bytes::Bytes;
use mr_core::Dialect;
use serde_json::{json, Map, Value};

use crate::openai::{
    flatten_text, pivot_response_to_anthropic, pivot_wants_stream, rewrite_model, sse_to_anthropic,
    to_pivot_request, AnthropicStreamState, PivotRequest,
};
use crate::sse;
use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest};

// ---------------------------------------------------------------------------------------------
// Request: pivot -> generateContent
// ---------------------------------------------------------------------------------------------

/// Convert a Chat Completions pivot request into a Gemini `generateContent` request.
///
/// The model id never appears in the returned body: it is written into `path_override`
/// (`models/{model}:generateContent`), because that — not a body field — is how Gemini selects a
/// model. `force_stream` marks that the path was built as `streamGenerateContent`, which is the
/// relay's only signal that the answer will be SSE: the Gemini body carries no `stream` flag
/// (the endpoint variant decides), so a `client_streaming` request whose body never asked for a
/// stream would otherwise look identical to a plain JSON round trip.
pub fn from_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    let mut pivot = to_pivot_request(ctx, body)?;

    // Rewrite first so the id embedded in the path and the id left in the body can never
    // disagree: whichever alias the client sent, both end up as the relay's resolved model.
    rewrite_model(&mut pivot.value, ctx.upstream_model);
    let model = pivot.model().unwrap_or(ctx.upstream_model);
    let stream = pivot_wants_stream(&pivot.value) || ctx.client_streaming;

    let gemini = build_body(&pivot);

    let (path, out_body) = match ctx.upstream_dialect {
        // The Cloud Code endpoint names its methods, not its models, and carries the Gemini body
        // in an envelope alongside the details the credential owns.
        Dialect::Antigravity => {
            let mut envelope = Map::new();
            envelope.insert("model".into(), json!(model));
            envelope.insert("request".into(), Value::Object(gemini));
            envelope.insert("userAgent".into(), json!("antigravity"));
            envelope.insert("requestId".into(), json!(mr_core::ids::new_id()));
            // `project` comes from account-specific Cloud Code onboarding and is not client input.
            // The relay adds this value from the encrypted OAuth token metadata before dispatch.
            let p = if stream {
                "v1internal:streamGenerateContent?alt=sse"
            } else {
                "v1internal:generateContent"
            };
            (p.to_string(), Value::Object(envelope))
        }
        _ => {
            let p = if stream {
                format!("models/{}:streamGenerateContent?alt=sse", model)
            } else {
                format!("models/{}:generateContent", model)
            };
            (p, Value::Object(gemini))
        }
    };

    let bytes = serde_json::to_vec(&out_body).map_err(|e| TranslateError::MalformedRequest {
        dialect: ctx.client_dialect.as_str(),
        detail: e.to_string(),
    })?;

    Ok(TranslatedRequest {
        body: Bytes::from(bytes),
        content_type: "application/json",
        force_stream: stream,
        path_override: Some(path),
    })
}

/// Assemble the Gemini request document from a normalised pivot request.
fn build_body(pivot: &PivotRequest) -> Map<String, Value> {
    // Pass 1: `tool_call_id` -> function name. The only place both halves of that pair exist is
    // the assistant turn that opened the call, and Gemini keys `functionResponse` by name.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for m in pivot.messages() {
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for tc in calls {
                let id = tc.get("id").and_then(Value::as_str);
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str);
                if let (Some(id), Some(name)) = (id, name) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    let mut contents: Vec<Value> = Vec::new();
    let mut system_parts: Vec<Value> = Vec::new();

    for m in pivot.messages() {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        match role {
            "system" | "developer" => {
                // System is not a turn in Gemini; it is hoisted to `systemInstruction`. The
                // `developer` role folds in: it carries system-style instructions and Gemini has
                // no third role to hold them.
                let text = flatten_text(m.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    system_parts.push(json!({"text": text}));
                }
            }
            "assistant" => {
                let mut parts: Vec<Value> = Vec::new();
                let text = flatten_text(m.get("content").unwrap_or(&Value::Null));
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
                if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
                    for tc in calls {
                        if let Some(part) = function_call_part(tc) {
                            parts.push(part);
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(json!({"role": "model", "parts": parts}));
                }
                // An assistant turn with neither text nor calls says nothing, and a `model`
                // content with zero parts is a 400 (`Content.parts must not be empty`), so it is
                // dropped rather than sent as an empty shell.
            }
            "tool" => {
                if let Some(turn) = function_response_turn(m, &tool_names) {
                    contents.push(turn);
                }
            }
            _ => {
                // "user", plus anything a client invents that plays the user's part.
                let parts = user_parts(m);
                if !parts.is_empty() {
                    contents.push(json!({"role": "user", "parts": parts}));
                }
            }
        }
    }

    if contents.is_empty() {
        // Gemini rejects an empty `contents` outright, so a system-only or wholly empty request
        // still needs one turn to address. The empty text part is the smallest legal filler —
        // `parts` must not be empty, but nothing is being invented as content.
        contents.push(json!({"role": "user", "parts": [{"text": ""}]}));
    }

    let mut out = Map::new();
    out.insert("contents".into(), Value::Array(contents));
    if !system_parts.is_empty() {
        out.insert("systemInstruction".into(), json!({"parts": system_parts}));
    }
    if let Some(decls) = function_declarations(pivot) {
        out.insert("tools".into(), json!([{"functionDeclarations": decls}]));
        // toolConfig only exists to shape function calling, and Gemini rejects it without tools.
        if let Some(config) = tool_config(pivot) {
            out.insert("toolConfig".into(), config);
        }
    }
    if let Some(gen) = generation_config(pivot) {
        out.insert("generationConfig".into(), gen);
    }
    out
}

/// One assistant `tool_calls` entry as a Gemini `functionCall` part.
fn function_call_part(tc: &Value) -> Option<Value> {
    let func = tc.get("function")?;
    let name = func.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        // A call without a name cannot be answered back; Gemini requires it.
        return None;
    }
    let mut call = Map::new();
    call.insert("name".into(), json!(name));
    let raw = func.get("arguments").and_then(Value::as_str).unwrap_or("");
    if !raw.trim().is_empty() {
        match serde_json::from_str::<Value>(raw) {
            Ok(v @ Value::Object(_)) => {
                call.insert("args".into(), v);
            }
            // Gemini's `args` is a Struct: a bare scalar or a broken fragment cannot ride
            // as-is, and failing the whole turn over one bad fragment loses more than an empty
            // input does — the model sees the call completed and can retry.
            _ => {
                call.insert("args".into(), json!({}));
            }
        }
    }
    // Arguments absent or empty: `args` is optional, and an absent one is the honest report.
    Some(json!({"functionCall": call}))
}

/// One `role: "tool"` message as a user turn holding Gemini's `functionResponse`.
fn function_response_turn(m: &Value, tool_names: &HashMap<String, String>) -> Option<Value> {
    let call_id = m.get("tool_call_id").and_then(Value::as_str)?;
    if call_id.is_empty() {
        return None;
    }
    // The resolved name first, because that is Gemini's key; the raw id as a last resort, since
    // a client echoing an id we never minted still holds a real result and dropping it strands
    // the turn.
    let name = tool_names
        .get(call_id)
        .map(String::as_str)
        .unwrap_or(call_id);
    let text = flatten_text(m.get("content").unwrap_or(&Value::Null));
    Some(json!({
        "role": "user",
        "parts": [{"functionResponse": {"name": name, "response": response_value(&text)}}]
    }))
}

/// Shape a tool result for `functionResponse.response`, a protobuf Struct.
fn response_value(text: &str) -> Value {
    match serde_json::from_str::<Value>(text) {
        Ok(v @ Value::Object(_)) => v,
        // Only a JSON object rides as-is. A tool that printed text, a bare array, an empty
        // result — all go under the conventional `result` key rather than being rejected
        // wholesale. This is the one wrapped shape in the translation, and it is forced by the
        // Struct type, not chosen for convenience.
        Ok(v) => json!({"result": v}),
        Err(_) => json!({"result": text}),
    }
}

/// Text and image parts of one user-content value.
fn user_parts(m: &Value) -> Vec<Value> {
    let mut parts = Vec::new();
    match m.get("content") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => {
            if !s.is_empty() {
                parts.push(json!({"text": s}));
            }
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let t = item.get("text").and_then(Value::as_str).unwrap_or("");
                        if !t.is_empty() {
                            parts.push(json!({"text": t}));
                        }
                    }
                    Some("image_url") => {
                        if let Some(p) = image_part(item) {
                            parts.push(p);
                        }
                    }
                    // Other part types (audio, input files) have no generateContent equivalent;
                    // an unsupported part is dropped instead of being reinterpreted.
                    _ => {}
                }
            }
        }
        Some(other) => {
            let t = flatten_text(other);
            if !t.is_empty() {
                parts.push(json!({"text": t}));
            }
        }
    }
    parts
}

/// An OpenAI `image_url` part as Gemini `inlineData` (data URL) or `fileData` (plain URL).
fn image_part(item: &Value) -> Option<Value> {
    let url = match item.get("image_url") {
        Some(Value::String(s)) => s.as_str(),
        Some(obj) => obj.get("url").and_then(Value::as_str)?,
        None => return None,
    };
    if url.starts_with("data:") {
        let (mime, data) = data_url(url)?;
        return Some(json!({"inlineData": {"mimeType": mime, "data": data}}));
    }
    // A plain URL: Gemini takes it as fileData. The mime type is not carried in the URL, so it
    // is inferred from the extension; unknown extensions get the neutral octet-stream rather
    // than a guess at the pixels.
    Some(json!({"fileData": {"mimeType": mime_from_url(url), "fileUri": url}}))
}

/// Split `data:<mime>;base64,<payload>`; `None` when the URL is not that shape.
fn data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (header, payload) = rest.split_once(',')?;
    let mime = header.split(';').next().unwrap_or("");
    // inlineData's `data` field wants base64; a percent-encoded payload is not that, and
    // `data:...` with no comma cannot be split at all — both are dropped rather than sent.
    if mime.is_empty() || payload.is_empty() || !header.contains("base64") {
        return None;
    }
    Some((mime, payload))
}

/// Best-effort mime type for a URL, from its path extension.
fn mime_from_url(url: &str) -> &'static str {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = match path.rsplit_once('.') {
        Some((_, ext)) => ext.to_ascii_lowercase(),
        None => String::new(),
    };
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        _ => "application/octet-stream",
    }
}

/// The pivot `tools` array as Gemini `functionDeclarations`.
fn function_declarations(pivot: &PivotRequest) -> Option<Value> {
    let mut decls: Vec<Value> = Vec::new();
    for tool in pivot.tools() {
        if tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("function")
            != "function"
        {
            // Hosted tools (web search, file search, …) have no generateContent counterpart;
            // they are dropped rather than sent as a malformed function.
            continue;
        }
        let func = match tool.get("function") {
            Some(f) => f,
            None => continue,
        };
        let name = match func.get("name").and_then(Value::as_str) {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };
        let mut decl = Map::new();
        decl.insert("name".into(), json!(name));
        if let Some(d) = func.get("description").and_then(Value::as_str) {
            if !d.is_empty() {
                decl.insert("description".into(), json!(d));
            }
        }
        // The JSON Schema travels as-is: `parameters` is the same schema dialect, and
        // re-encoding it would only risk dropping keywords the model reads.
        if let Some(params) = func.get("parameters") {
            if params.is_object() {
                decl.insert("parameters".into(), params.clone());
            }
        }
        decls.push(Value::Object(decl));
    }
    if decls.is_empty() {
        None
    } else {
        Some(Value::Array(decls))
    }
}

/// Pivot `tool_choice` as Gemini `toolConfig.functionCallingConfig`.
///
/// Absent (or null) means the client expressed no preference, and Gemini's own default already
/// is "model decides", so no toolConfig is emitted; every *present* choice maps explicitly.
fn tool_config(pivot: &PivotRequest) -> Option<Value> {
    let choice = pivot.value.get("tool_choice")?;
    if choice.is_null() {
        return None;
    }
    let mut fcc = Map::new();
    match choice {
        Value::String(s) => match s.as_str() {
            "none" => {
                fcc.insert("mode".into(), json!("NONE"));
            }
            "required" => {
                fcc.insert("mode".into(), json!("ANY"));
            }
            // "auto" and any word this side has no mapping for: AUTO is Gemini's default and
            // the only non-committal reading.
            _ => {
                fcc.insert("mode".into(), json!("AUTO"));
            }
        },
        Value::Object(o) => match o.get("type").and_then(Value::as_str) {
            Some("none") => {
                fcc.insert("mode".into(), json!("NONE"));
            }
            Some("any") | Some("required") => {
                fcc.insert("mode".into(), json!("ANY"));
            }
            Some("function") => {
                // Pin to one function: Gemini expresses this as ANY plus an allow-list.
                fcc.insert("mode".into(), json!("ANY"));
                if let Some(name) = o
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                {
                    if !name.is_empty() {
                        fcc.insert("allowedFunctionNames".into(), json!([name]));
                    }
                }
                // A function-typed choice with no name: ANY over all functions. The name is
                // gone either way, and failing the request over it helps nobody.
            }
            _ => {
                fcc.insert("mode".into(), json!("AUTO"));
            }
        },
        _ => {
            fcc.insert("mode".into(), json!("AUTO"));
        }
    }
    Some(json!({"functionCallingConfig": Value::Object(fcc)}))
}

/// The subset of Chat Completions sampling options Gemini can actually express.
///
/// Fields with no counterpart (`presence_penalty`, `frequency_penalty`, `logit_bias`, `n`,
/// `logprobs`) are dropped: generateContent has no way to say them, and inventing a nearest
/// equivalent would silently change behaviour the client asked for.
fn generation_config(pivot: &PivotRequest) -> Option<Value> {
    let v = &pivot.value;
    let mut cfg = Map::new();
    // Presence, not truthiness: `temperature: 0` is a real answer and must survive.
    if let Some(t) = v.get("temperature").filter(|t| t.is_number()) {
        cfg.insert("temperature".into(), t.clone());
    }
    if let Some(p) = v.get("top_p").or(v.get("topP")).filter(|p| p.is_number()) {
        cfg.insert("topP".into(), p.clone());
    }
    // `max_tokens` plus its newer spelling; Gemini wants an integer, so only whole numbers go.
    if let Some(m) = v
        .get("max_tokens")
        .or(v.get("max_completion_tokens"))
        .filter(|m| m.is_u64())
    {
        cfg.insert("maxOutputTokens".into(), m.clone());
    }
    if let Some(stop) = v.get("stop") {
        let seqs: Vec<Value> = match stop {
            Value::String(s) if !s.is_empty() => vec![json!(s)],
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(|s| json!(s))
                .collect(),
            _ => Vec::new(),
        };
        if !seqs.is_empty() {
            cfg.insert("stopSequences".into(), Value::Array(seqs));
        }
    }
    if cfg.is_empty() {
        None
    } else {
        Some(Value::Object(cfg))
    }
}

// ---------------------------------------------------------------------------------------------
// Response: generateContent -> pivot
// ---------------------------------------------------------------------------------------------

/// Read a field under its camelCase wire spelling, falling back to snake_case.
///
/// Gemini's own REST surface is camelCase and that is what we emit, but the model gRPC surface
/// and several SDK serializers expose the same fields snake_cased; a proxy in front of either
/// should not lose a usage report over spelling.
fn field<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a Value> {
    if camel == snake {
        v.get(camel)
    } else {
        v.get(camel).or_else(|| v.get(snake))
    }
}

/// [`field`] for string values.
fn field_str<'a>(v: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    field(v, camel, snake).and_then(Value::as_str)
}

/// [`field`] for unsigned integer values.
fn field_u64(v: &Value, camel: &str, snake: &str) -> Option<u64> {
    field(v, camel, snake).and_then(Value::as_u64)
}

/// Unwrap the Antigravity / Cloud Code envelope, if present.
///
/// Cloud Code answers `{"response": {...the real generateContent response...}, "traceId": ...}`
/// while the public API returns the bare object. Detection keys off `response` holding
/// `candidates` — widened to `usageMetadata` as well, because a prompt blocked before drafting
/// has no candidates at all but still reports usage; reading `candidates` off the wrapper in
/// that case would silently produce an empty answer instead of an error.
fn unwrap_envelope(v: &Value) -> &Value {
    if let Some(inner) = v.get("response").filter(|i| i.is_object()) {
        if field(inner, "candidates", "candidates").is_some()
            || field(inner, "usageMetadata", "usage_metadata").is_some()
        {
            return inner;
        }
    }
    v
}

/// Map a Gemini `finishReason` onto a Chat Completions `finish_reason`.
///
/// The order matters: a safety verdict outranks everything (the answer was rejected, whatever
/// else it contained), a completed function call outranks "stopped", and `STOP` alone is a plain
/// stop. An unrecognised reason is passed through **lower-cased rather than nulled**: on this
/// side null is not neutral — the pivot-to-Anthropic adapter reads a missing `finish_reason` as
/// `end_turn`, i.e. claims the answer completed, while a lower-cased unknown string maps to a
/// null `stop_reason`, which Anthropic clients correctly read as incomplete.
fn map_finish_reason(reason: Option<&str>, has_function_call: bool) -> Value {
    match reason {
        Some(
            "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "BLOCKLIST" | "SPII" | "IMAGE_SAFETY",
        ) => json!("content_filter"),
        Some("STOP") => {
            if has_function_call {
                json!("tool_calls")
            } else {
                json!("stop")
            }
        }
        Some("MAX_TOKENS") => json!("length"),
        Some(other) => {
            if has_function_call {
                json!("tool_calls")
            } else {
                json!(other.to_ascii_lowercase())
            }
        }
        // No reason field at all, but the stream finished: the usual reason Gemini does not
        // bother stating is a clean stop.
        None => {
            if has_function_call {
                json!("tool_calls")
            } else {
                json!("stop")
            }
        }
    }
}

/// Convert a Gemini response into a pivot response body.
///
/// For an Anthropic client the completed pivot object is then handed to
/// [`crate::openai::pivot_response_to_anthropic`] — the inverse of this mapping already exists
/// there, and two implementations of one conversion is how they drift apart.
pub fn to_pivot_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    let v: Value = serde_json::from_slice(body).map_err(|e| TranslateError::MalformedResponse {
        dialect: "google",
        detail: e.to_string(),
    })?;
    let v = unwrap_envelope(&v);

    let pivot = google_response_to_pivot(ctx, v);
    let out = match ctx.client_dialect {
        Dialect::Anthropic => {
            let mut state = AnthropicStreamState::default();
            pivot_response_to_anthropic(ctx, &pivot, &mut state)?
        }
        _ => pivot,
    };

    serde_json::to_vec(&out)
        .map(Bytes::from)
        .map_err(|e| TranslateError::MalformedResponse {
            dialect: "google",
            detail: e.to_string(),
        })
}

/// Build a Chat Completions `chat.completion` object from a Gemini response.
///
/// `ctx` names the model to echo; the candidates, finish reason and usage are all read off the
/// wire, never assumed.
fn google_response_to_pivot(ctx: &TranslateCtx<'_>, v: &Value) -> Value {
    let candidate = field(v, "candidates", "candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();

    if let Some(parts) = candidate
        .and_then(|c| field(c, "content", "content"))
        .and_then(|content| field(content, "parts", "parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(call) = field(part, "functionCall", "function_call") {
                if let Some(name) = field_str(call, "name", "name").filter(|n| !n.is_empty()) {
                    // Chat Completions keys calls by an id it then echoes on the tool result;
                    // Gemini's functionCall has no id, so one is minted per response. The
                    // request half resolves it back to a name by finding this very entry in
                    // the assistant turn, so the pair round-trips.
                    let arguments = match field(call, "args", "args") {
                        Some(a) if a.is_object() || a.is_array() => a.to_string(),
                        Some(_) => "{}".to_string(),
                        // Absent args is a valid empty call; Chat Completions wants the
                        // string, so it becomes the empty JSON object.
                        None => "{}".to_string(),
                    };
                    tool_calls.push(json!({
                        "id": format!("call_{}", tool_calls.len()),
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }));
                }
                continue;
            }
            let is_thought = field(part, "thought", "thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(t) = field_str(part, "text", "text").filter(|t| !t.is_empty()) {
                if is_thought {
                    // Reasoning never rides in `content`: an OpenAI client would render the
                    // model's private deliberation as the answer.
                    reasoning.push_str(t);
                } else {
                    text.push_str(t);
                }
            }
            // Anything else (thoughtSignature markers, inline media the model returned) carries
            // no text for the client and is skipped rather than guessed at.
        }
    }

    let has_call = !tool_calls.is_empty();
    let finish_reason = match candidate {
        Some(_) => map_finish_reason(
            field_str(
                candidate.unwrap_or(&Value::Null),
                "finishReason",
                "finish_reason",
            ),
            has_call,
        ),
        None => {
            // No candidate: the only explanation Gemini offers is a prompt-level block reason
            // such as `blockReason: SAFETY`.
            let blocked = field(v, "promptFeedback", "prompt_feedback")
                .and_then(|f| field_str(f, "blockReason", "block_reason"));
            match blocked {
                Some(_) => json!("content_filter"),
                None => Value::Null,
            }
        }
    };

    let mut message = Map::new();
    message.insert("role".into(), json!("assistant"));
    if !reasoning.is_empty() {
        message.insert("reasoning_content".into(), json!(reasoning));
    }
    if text.is_empty() && tool_calls.is_empty() {
        // No text and no call (a fully blocked answer): an empty string, which the Anthropic
        // direction renders as no text block at all.
        message.insert("content".into(), json!(""));
    } else if text.is_empty() {
        message.insert("content".into(), Value::Null);
    } else {
        message.insert("content".into(), json!(text));
    }
    if has_call {
        message.insert("tool_calls".into(), Value::Array(tool_calls));
    }

    json!({
        "id": format!("chatcmpl-{}", mr_core::ids::new_id()),
        "object": "chat.completion",
        "created": mr_core::ids::now_millis() / 1000,
        "model": ctx.upstream_model,
        "choices": [{
            "index": 0,
            "message": Value::Object(message),
            "finish_reason": finish_reason,
        }],
        "usage": usage_value(v),
    })
}

/// `usageMetadata` as a pivot `usage` object; all zeros when the field is absent.
///
/// Always present, never partial: Chat Completions marks `usage` required on a completed
/// response, and the pivot-to-Anthropic adapter falls back to the same zero defaults.
fn usage_value(v: &Value) -> Value {
    let u = match field(v, "usageMetadata", "usage_metadata") {
        Some(u) => u,
        None => {
            return json!({"prompt_tokens": 0, "completion_tokens": 0, "total_tokens": 0});
        }
    };
    let prompt = field_u64(u, "promptTokenCount", "prompt_token_count").unwrap_or(0);
    let completion = field_u64(u, "candidatesTokenCount", "candidates_token_count").unwrap_or(0);
    let total = field_u64(u, "totalTokenCount", "total_token_count").unwrap_or(prompt + completion);
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), json!(prompt));
    usage.insert("completion_tokens".into(), json!(completion));
    usage.insert("total_tokens".into(), json!(total));
    if let Some(cached) = field_u64(u, "cachedContentTokenCount", "cached_content_token_count") {
        // Chat Completions puts cache hits under prompt_tokens_details.cached_tokens; there is
        // no legacy field for it in this codebase to copy, so the documented OpenAI spelling is
        // used rather than an invented top-level key.
        usage.insert(
            "prompt_tokens_details".into(),
            json!({"cached_tokens": cached}),
        );
    }
    Value::Object(usage)
}

/// Translate one Gemini SSE data payload into zero or more client frames.
///
/// Each payload is one `generateContentResponse`: parts arrive as deltas, `finishReason` only on
/// the terminal frame (Gemini's SSE has no `[DONE]` sentinel — the finish reason *is* the end).
/// Frames are converted into the same `chat.completion.chunk` JSON a Chat upstream would have
/// produced — one role chunk, then text / reasoning / tool deltas, then a finish-and-usage chunk
/// — and those chunks are framed for whoever is listening: `data:` frames for an OpenAI client,
/// or pushed through [`crate::openai::sse_to_anthropic`] for an Anthropic one, which builds
/// `message_start` / `content_block_*` from them.
///
/// After the terminal chunk the stream terminator goes out: `data: [DONE]` for OpenAI clients,
/// and the adapter's own `[DONE]` handling (which yields `message_stop`) for Anthropic clients —
/// an Anthropic SDK rejects a literal `data: [DONE]`, and an OpenAI SDK hangs without one. A
/// frame that is not JSON, carries no known fields, or arrives after the terminal frame
/// contributes nothing: skipped, never fatal, because one bad frame must not kill a response
/// already half-delivered.
pub fn sse_to_pivot(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    let trimmed = data.trim();
    if trimmed.is_empty() || trimmed == crate::SSE_DONE {
        // A `[DONE]` arriving from this direction would be a mis-routed frame; echoing an
        // OpenAI idiom back at a Gemini-shaped stream helps no one.
        return Ok(Vec::new());
    }
    let v: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        // Providers occasionally put a commentary or keep-alive line on the stream.
        Err(_) => return Ok(Vec::new()),
    };

    GOOGLE_STREAM_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(GoogleStreamState::default);
        let mut out = Vec::new();

        let items: Vec<&Value> = match &v {
            // Batched payloads arrive as an array; each element is one response.
            Value::Array(a) => a.iter().collect(),
            other => vec![other],
        };
        for item in items {
            if state.done {
                // The terminal frame already closed the stream; anything after it is noise.
                break;
            }
            translate_stream_item(ctx, unwrap_envelope(item), state, &mut out)?;
        }
        Ok(out)
    })
}

/// Translate one Gemini stream frame: accumulate state, append frames to `out`.
fn translate_stream_item(
    ctx: &TranslateCtx<'_>,
    v: &Value,
    state: &mut GoogleStreamState,
    out: &mut Vec<String>,
) -> Result<()> {
    if state.id.is_empty() {
        state.id = format!("chatcmpl-{}", mr_core::ids::new_id());
        state.model = ctx.upstream_model.to_string();
    }

    // `usageMetadata` is cumulative and rides on every frame; the latest wins, and it is
    // attached to the terminal chunk — mid-stream chunks carry no usage, matching how a Chat
    // upstream reports it.
    if let Some(u) = field(v, "usageMetadata", "usage_metadata") {
        if let Some(p) = field_u64(u, "promptTokenCount", "prompt_token_count") {
            state.prompt_tokens = p;
            state.usage_seen = true;
        }
        if let Some(c) = field_u64(u, "candidatesTokenCount", "candidates_token_count") {
            state.completion_tokens = c;
            state.usage_seen = true;
        }
        if let Some(t) = field_u64(u, "totalTokenCount", "total_token_count") {
            state.total_tokens = t;
            state.usage_seen = true;
        }
        if let Some(c) = field_u64(u, "cachedContentTokenCount", "cached_content_token_count") {
            state.cached_tokens = Some(c);
        }
    }

    let candidate = field(v, "candidates", "candidates")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let mut chunks: Vec<Value> = Vec::new();

    if let Some(parts) = candidate
        .and_then(|c| field(c, "content", "content"))
        .and_then(|content| field(content, "parts", "parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(call) = field(part, "functionCall", "function_call") {
                if let Some(name) = field_str(call, "name", "name").filter(|n| !n.is_empty()) {
                    push_role_chunk(state, &mut chunks);
                    state.saw_tool_call = true;
                    let index = state.next_tool_index;
                    state.next_tool_index += 1;
                    let arguments = match field(call, "args", "args") {
                        Some(a) if a.is_object() || a.is_array() => a.to_string(),
                        // Gemini sends a whole call at once, so one chunk carries name and
                        // full arguments together — no argument splicing across frames.
                        _ => "{}".to_string(),
                    };
                    chunks.push(stream_chunk(
                        state,
                        json!({
                            "tool_calls": [{
                                "index": index,
                                "id": format!("call_{}", index),
                                "type": "function",
                                "function": {"name": name, "arguments": arguments},
                            }]
                        }),
                    ));
                }
                continue;
            }
            let is_thought = field(part, "thought", "thought")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if let Some(t) = field_str(part, "text", "text").filter(|t| !t.is_empty()) {
                push_role_chunk(state, &mut chunks);
                // Reasoning rides its own delta key: mixed into `content` it would render the
                // model's deliberation as the answer.
                let delta = if is_thought {
                    json!({"reasoning_content": t})
                } else {
                    json!({"content": t})
                };
                chunks.push(stream_chunk(state, delta));
            }
            // thoughtSignature markers and any media parts carry no client-visible text.
        }
    }

    let reason = candidate.and_then(|c| field_str(c, "finishReason", "finish_reason"));
    let blocked = field(v, "promptFeedback", "prompt_feedback")
        .and_then(|f| field_str(f, "blockReason", "block_reason"));
    // Terminal when a candidate states its finish reason, or when a prompt-level block explains
    // why there is no candidate at all.
    let terminal = reason.is_some() || (candidate.is_none() && blocked.is_some());

    if terminal {
        push_role_chunk(state, &mut chunks);
        let finish = if candidate.is_some() {
            map_finish_reason(reason, state.saw_tool_call)
        } else {
            json!("content_filter")
        };
        let mut done_chunk = json!({
            "id": state.id,
            "object": "chat.completion.chunk",
            "created": mr_core::ids::now_millis() / 1000,
            "model": state.model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
        });
        if state.usage_seen {
            done_chunk["usage"] = stream_usage(state);
        }
        chunks.push(done_chunk);
        state.done = true;
    }

    for c in &chunks {
        let body = c.to_string();
        match ctx.client_dialect {
            Dialect::Anthropic => out.extend(sse_to_anthropic(ctx, &body)?),
            // Every other client on this route is a Chat Completions client.
            _ => out.push(sse::data_frame(&body)),
        }
    }

    if terminal {
        match ctx.client_dialect {
            // The adapter's `[DONE]` handling closes the open content block and emits
            // `message_stop`; feeding the literal frame through it is what keeps Anthropic
            // stream bookkeeping in one place.
            Dialect::Anthropic => out.extend(sse_to_anthropic(ctx, crate::SSE_DONE)?),
            _ => out.push(format!("data: {}\n\n", crate::SSE_DONE)),
        }
    }
    Ok(())
}

/// The stream's id/model envelope around one delta.
fn stream_chunk(state: &GoogleStreamState, delta: Value) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "created": mr_core::ids::now_millis() / 1000,
        "model": state.model,
        "choices": [{"index": 0, "delta": delta, "finish_reason": Value::Null}]
    })
}

/// Emit the role-only opener once, before the first content or terminal chunk: Chat clients
/// initialise the message from it.
fn push_role_chunk(state: &mut GoogleStreamState, chunks: &mut Vec<Value>) {
    if state.role_sent {
        return;
    }
    state.role_sent = true;
    chunks.push(stream_chunk(
        state,
        json!({"role": "assistant", "content": ""}),
    ));
}

/// Accumulated `usageMetadata` as a pivot `usage` object for the terminal chunk.
fn stream_usage(state: &GoogleStreamState) -> Value {
    let mut usage = Map::new();
    usage.insert("prompt_tokens".into(), json!(state.prompt_tokens));
    usage.insert("completion_tokens".into(), json!(state.completion_tokens));
    let total = if state.total_tokens == 0 {
        state.prompt_tokens + state.completion_tokens
    } else {
        state.total_tokens
    };
    usage.insert("total_tokens".into(), json!(total));
    if let Some(cached) = state.cached_tokens {
        usage.insert(
            "prompt_tokens_details".into(),
            json!({"cached_tokens": cached}),
        );
    }
    Value::Object(usage)
}

/// Stream bookkeeping for `streamGenerateContent`.
///
/// Gemini's stream has no ids, no role fields and no finish chunk of its own — those are all
/// pivot vocabulary — so the first client frame's identity and the counters that span frames
/// live here. The thread-local wrapper above keeps one per relay task, on the same rationale as
/// [`crate::openai`]'s: one task handles one stream end to end.
#[derive(Debug, Default)]
pub struct GoogleStreamState {
    /// The id echoed on every chunk so a client correlating them sees one response.
    pub id: String,
    /// The model name to echo, filled from the context on first use.
    pub model: String,
    /// Whether the opening role chunk has been emitted yet.
    pub role_sent: bool,
    /// Tool-call index counter: Chat Completions numbers tool calls across the stream.
    pub next_tool_index: u64,
    /// Whether any frame carried a functionCall, for the terminal finish reason.
    pub saw_tool_call: bool,
    /// Whether any frame carried `usageMetadata`, gating the usage attachment.
    pub usage_seen: bool,
    /// Prompt tokens from the latest frame carrying `usageMetadata`.
    pub prompt_tokens: u64,
    /// Completion tokens from the latest frame carrying them.
    pub completion_tokens: u64,
    /// Total tokens from the latest frame carrying them.
    pub total_tokens: u64,
    /// Cached-prompt tokens, when the report includes them.
    pub cached_tokens: Option<u64>,
    /// Whether the terminal chunk has been emitted, so a stray later frame cannot emit twice.
    pub done: bool,
}

thread_local! {
    static GOOGLE_STREAM_STATE: RefCell<Option<GoogleStreamState>> =
        const { RefCell::new(None) };
}

/// Forget stream state left behind by a previous stream on this thread.
///
/// Also clears the pivot adapter's Anthropic stream state: the Anthropic half of a Gemini
/// stream lives there (Gemini frames are converted to pivot chunks and pushed through
/// [`crate::openai::sse_to_anthropic`]), and this reset is the one the relay calls at stream
/// end for this dialect.
pub fn reset_stream_state() {
    GOOGLE_STREAM_STATE.with(|cell| *cell.borrow_mut() = None);
    crate::openai::reset_stream_state();
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::RouteKind;
    use serde_json::{json, Value};

    fn ctx() -> TranslateCtx<'static> {
        TranslateCtx::new(
            RouteKind::ChatCompletions,
            Dialect::Google,
            "gemini-2.0-flash",
        )
    }

    fn anthropic_ctx() -> TranslateCtx<'static> {
        TranslateCtx::new(RouteKind::Messages, Dialect::Google, "gemini-2.0-flash")
    }

    /// Translate a request body and return the Gemini document.
    fn build(v: Value) -> Value {
        let body = serde_json::to_vec(&v).unwrap();
        let req = from_pivot_request(&ctx(), &body).unwrap();
        serde_json::from_slice(&req.body).unwrap()
    }

    /// Translate a response body back into the pivot shape.
    fn response(v: Value) -> Value {
        let bytes = to_pivot_response(&ctx(), &serde_json::to_vec(&v).unwrap()).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// The JSON payload of one emitted SSE frame.
    fn chunk(frame: &str) -> Value {
        serde_json::from_str(&sse::parse_frame(frame).data).unwrap()
    }

    // ------------------------------------------------------------------ request

    #[test]
    fn system_prompt_moves_out_of_the_turn_list() {
        let out = build(json!({
            "model": "client-alias",
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "Hi"}
            ]
        }));
        assert!(
            out.get("model").is_none(),
            "the model id belongs in the URL path, never in the body"
        );
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "Be terse.");
        // The system message must not appear as a turn of its own.
        assert_eq!(out["contents"].as_array().unwrap().len(), 1);
        assert_eq!(out["contents"][0]["role"], "user");
        assert_eq!(out["contents"][0]["parts"][0]["text"], "Hi");
    }

    #[test]
    fn assistant_calls_and_tool_results_key_by_function_name() {
        let out = build(json!({
            "model": "x",
            "messages": [
                {"role": "user", "content": "Weather?"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_abc", "type": "function",
                     "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_abc", "content": "{\"temperature\":18}"}
            ]
        }));
        let contents = out["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3);
        assert_eq!(contents[1]["role"], "model");
        let call = &contents[1]["parts"][0]["functionCall"];
        assert_eq!(call["name"], "get_weather");
        assert_eq!(call["args"]["city"], "Paris");
        // The tool answer comes back as a user turn keyed by NAME, resolved from the assistant
        // call above: Gemini has no tool_call_id to key on.
        assert_eq!(contents[2]["role"], "user");
        let reply = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(reply["name"], "get_weather");
        assert_eq!(reply["response"]["temperature"], 18);
    }

    #[test]
    fn plain_text_tool_results_are_wrapped_for_the_struct_field() {
        let out = build(json!({
            "model": "x",
            "messages": [
                {"role": "assistant", "tool_calls": [
                    {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}
                ]},
                {"role": "tool", "tool_call_id": "c1", "content": "hello world"}
            ]
        }));
        let reply = &out["contents"][1]["parts"][0]["functionResponse"];
        assert_eq!(reply["name"], "f");
        assert_eq!(reply["response"], json!({"result": "hello world"}));
    }

    #[test]
    fn images_become_inline_data_or_file_data() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                {"type": "image_url", "image_url": "data:image/jpeg;base64,WA=="},
                {"type": "image_url", "image_url": {"url": "https://example.com/cat.webp"}}
            ]}]
        }));
        let parts = out["contents"][0]["parts"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "look");
        assert_eq!(parts[1]["inlineData"]["mimeType"], "image/png");
        assert_eq!(parts[1]["inlineData"]["data"], "QUJD");
        assert_eq!(parts[2]["inlineData"]["mimeType"], "image/jpeg");
        assert_eq!(parts[3]["fileData"]["mimeType"], "image/webp");
        assert_eq!(
            parts[3]["fileData"]["fileUri"],
            "https://example.com/cat.webp"
        );
    }

    #[test]
    fn empty_messages_still_produce_one_user_turn() {
        let out = build(json!({"model": "x", "messages": []}));
        assert_eq!(
            out["contents"],
            json!([{"role": "user", "parts": [{"text": ""}]}]),
            "an empty contents array is a 400 from Gemini"
        );

        // System-only: the instruction still hoists, and the filler turn still appears.
        let out = build(json!({"model": "x", "messages": [{"role": "system", "content": "s"}]}));
        assert_eq!(out["systemInstruction"]["parts"][0]["text"], "s");
        assert_eq!(out["contents"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tools_become_function_declarations_and_required_maps_to_any() {
        let out = build(json!({
            "model": "x",
            "messages": [],
            "tools": [{"type": "function", "function": {
                "name": "get_weather", "description": "Look up weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }}],
            "tool_choice": "required"
        }));
        let decl = &out["tools"][0]["functionDeclarations"][0];
        assert_eq!(decl["name"], "get_weather");
        assert_eq!(decl["description"], "Look up weather");
        assert_eq!(decl["parameters"]["properties"]["city"]["type"], "string");
        assert_eq!(out["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    }

    #[test]
    fn tool_choice_maps_to_modes_and_allow_lists() {
        let tool = json!({"type": "function", "function": {"name": "f", "description": "d"}});

        let none =
            build(json!({"model": "x", "messages": [], "tools": [tool], "tool_choice": "none"}));
        assert_eq!(none["toolConfig"]["functionCallingConfig"]["mode"], "NONE");

        let auto =
            build(json!({"model": "x", "messages": [], "tools": [tool], "tool_choice": "auto"}));
        assert_eq!(auto["toolConfig"]["functionCallingConfig"]["mode"], "AUTO");

        let pinned = build(json!({
            "model": "x", "messages": [], "tools": [tool],
            "tool_choice": {"type": "function", "function": {"name": "f"}}
        }));
        let fcc = &pinned["toolConfig"]["functionCallingConfig"];
        assert_eq!(fcc["mode"], "ANY");
        assert_eq!(fcc["allowedFunctionNames"], json!(["f"]));

        // No preference expressed: Gemini's own default already is "model decides", and a
        // toolConfig without tools is rejected outright.
        let absent = build(json!({"model": "x", "messages": [], "tools": [tool]}));
        assert!(absent.get("toolConfig").is_none());
        let no_tools = build(json!({"model": "x", "messages": [], "tool_choice": "required"}));
        assert!(no_tools.get("toolConfig").is_none());
    }

    #[test]
    fn generation_config_carries_only_expressible_fields() {
        let out = build(json!({
            "model": "x", "messages": [],
            "temperature": 0.0, "top_p": 0.9, "max_tokens": 256,
            "stop": ["END", "DONE"], "presence_penalty": 0.5, "n": 2
        }));
        let cfg = &out["generationConfig"];
        assert_eq!(
            cfg["temperature"].as_f64(),
            Some(0.0),
            "temperature 0 is a real answer, not an absent field"
        );
        assert_eq!(cfg["topP"].as_f64(), Some(0.9));
        assert_eq!(cfg["maxOutputTokens"], 256);
        assert_eq!(cfg["stopSequences"], json!(["END", "DONE"]));
        assert!(
            cfg.get("presence_penalty").is_none() && cfg.get("n").is_none(),
            "fields Gemini cannot express are dropped, not invented"
        );

        let bare = build(json!({"model": "x", "messages": []}));
        assert!(bare.get("generationConfig").is_none());
    }

    #[test]
    fn path_carries_the_model_and_selects_the_stream_endpoint() {
        let body = serde_json::to_vec(&json!({"model": "client-alias", "messages": []})).unwrap();
        let r = from_pivot_request(&ctx(), &body).unwrap();
        assert_eq!(
            r.path_override.as_deref(),
            Some("models/gemini-2.0-flash:generateContent")
        );
        assert!(!r.force_stream);
        assert_eq!(r.content_type, "application/json");
        let doc: Value = serde_json::from_slice(&r.body).unwrap();
        assert!(
            doc.get("model").is_none(),
            "the URL names the model, not the body"
        );

        let body =
            serde_json::to_vec(&json!({"model": "x", "messages": [], "stream": true})).unwrap();
        let r = from_pivot_request(&ctx(), &body).unwrap();
        assert_eq!(
            r.path_override.as_deref(),
            Some("models/gemini-2.0-flash:streamGenerateContent?alt=sse")
        );
        assert!(
            r.force_stream,
            "the relay must know the answer will be SSE: the body carries no stream flag"
        );

        // A streaming client whose body never mentioned a stream still needs the SSE endpoint.
        let mut c = ctx();
        c.client_streaming = true;
        let r = from_pivot_request(&c, br#"{"model":"x","messages":[]}"#).unwrap();
        assert!(r.force_stream);
        assert_eq!(
            r.path_override.as_deref(),
            Some("models/gemini-2.0-flash:streamGenerateContent?alt=sse")
        );
    }

    #[test]
    fn antigravity_upstream_wraps_the_body_and_names_v1internal() {
        let c = TranslateCtx::new(
            RouteKind::ChatCompletions,
            Dialect::Antigravity,
            "gemini-2.5-pro",
        );
        let body = serde_json::to_vec(
            &json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]}),
        )
        .unwrap();
        let r = from_pivot_request(&c, &body).unwrap();
        assert_eq!(
            r.path_override.as_deref(),
            Some("v1internal:generateContent")
        );
        let doc: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(doc["model"], "gemini-2.5-pro");
        assert_eq!(doc["request"]["contents"][0]["parts"][0]["text"], "hi");
        assert!(doc["requestId"].is_string());
    }

    #[test]
    fn malformed_request_bodies_are_rejected() {
        match from_pivot_request(&ctx(), b"{oops") {
            Err(TranslateError::MalformedRequest { .. }) => {}
            other => panic!(
                "expected MalformedRequest, got {:?}",
                other.map(|r| r.path_override)
            ),
        }
        match from_pivot_request(&ctx(), b"[1,2,3]") {
            Err(TranslateError::MalformedRequest { .. }) => {}
            other => panic!(
                "expected MalformedRequest, got {:?}",
                other.map(|r| r.path_override)
            ),
        }
    }

    // ----------------------------------------------------------------- response

    #[test]
    fn bare_response_becomes_a_chat_completion() {
        let out = response(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "Hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 5, "totalTokenCount": 16}
        }));
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "gemini-2.0-flash");
        assert!(out["id"].as_str().unwrap().starts_with("chatcmpl-"));
        assert_eq!(out["choices"][0]["index"], 0);
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["message"]["content"], "Hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(out["usage"]["prompt_tokens"], 11);
        assert_eq!(out["usage"]["completion_tokens"], 5);
        assert_eq!(out["usage"]["total_tokens"], 16);
    }

    #[test]
    fn antigravity_envelope_unwraps_to_the_same_answer() {
        let bare = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hello"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2, "totalTokenCount": 5}
        });
        let wrapped = json!({"response": bare.clone(), "traceId": "t-1"});

        let from_bare = response(bare);
        let from_wrapped = response(wrapped);
        // Reading `candidates` off the wrapper without unwrapping yields an empty answer; both
        // paths must instead carry the inner text.
        assert_eq!(from_bare["choices"][0]["message"]["content"], "Hello");
        assert_eq!(
            from_wrapped["choices"][0]["message"]["content"],
            from_bare["choices"][0]["message"]["content"]
        );
        assert_eq!(from_wrapped["usage"]["total_tokens"], 5);
    }

    #[test]
    fn finish_reasons_map_to_pivot_vocabulary() {
        let finish = |reason: &str| {
            response(json!({
                "candidates": [{
                    "content": {"parts": [{"text": "t"}]},
                    "finishReason": reason
                }]
            }))["choices"][0]["finish_reason"]
                .clone()
        };
        assert_eq!(finish("STOP"), json!("stop"));
        assert_eq!(finish("MAX_TOKENS"), json!("length"));
        assert_eq!(finish("SAFETY"), json!("content_filter"));
        // An unrecognised reason is passed through lower-cased rather than claimed as a stop:
        // the pivot-to-Anthropic adapter reads a null finish reason as incomplete.
        assert_eq!(finish("OTHER"), json!("other"));
    }

    #[test]
    fn a_function_call_reported_as_tool_calls_even_on_stop() {
        let out = response(json!({
            "candidates": [{
                "content": {"parts": [{"functionCall": {"name": "f", "args": {"a": 1}}}]},
                "finishReason": "STOP"
            }]
        }));
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        let call = &out["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], "call_0");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "f");
        // Chat Completions carries arguments as a JSON string, not an object.
        assert_eq!(
            serde_json::from_str::<Value>(call["function"]["arguments"].as_str().unwrap()).unwrap(),
            json!({"a": 1})
        );
    }

    #[test]
    fn thought_parts_become_reasoning_content_not_content() {
        let out = response(json!({
            "candidates": [{
                "content": {"parts": [
                    {"thought": true, "text": "let me think"},
                    {"text": "the answer"}
                ]},
                "finishReason": "STOP"
            }]
        }));
        let message = &out["choices"][0]["message"];
        assert_eq!(message["reasoning_content"], "let me think");
        assert_eq!(message["content"], "the answer");
    }

    #[test]
    fn blocked_prompt_without_candidates_reports_a_filter() {
        let out = response(json!({
            "promptFeedback": {"blockReason": "SAFETY"},
            "usageMetadata": {"promptTokenCount": 4, "totalTokenCount": 4}
        }));
        assert_eq!(out["choices"][0]["finish_reason"], "content_filter");
        assert_eq!(out["choices"][0]["message"]["content"], "");
        assert_eq!(out["usage"]["prompt_tokens"], 4);
    }

    #[test]
    fn cached_prompt_tokens_ride_in_prompt_tokens_details() {
        let out = response(json!({
            "candidates": [{"content": {"parts": [{"text": "t"}]}, "finishReason": "STOP"}],
            "usageMetadata": {
                "promptTokenCount": 100, "candidatesTokenCount": 1, "totalTokenCount": 101,
                "cachedContentTokenCount": 64
            }
        }));
        assert_eq!(out["usage"]["prompt_tokens_details"]["cached_tokens"], 64);
    }

    #[test]
    fn malformed_response_bodies_are_rejected() {
        match to_pivot_response(&ctx(), b"not json at all") {
            Err(TranslateError::MalformedResponse { dialect, .. }) => {
                assert_eq!(dialect, "google");
            }
            other => panic!("expected MalformedResponse, got {:?}", other.is_ok()),
        }
    }

    #[test]
    fn anthropic_client_gets_a_message_from_the_pivot_adapter() {
        let v = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hi"}]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 5, "totalTokenCount": 16}
        });
        let bytes = to_pivot_response(&anthropic_ctx(), &serde_json::to_vec(&v).unwrap()).unwrap();
        let out: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "gemini-2.0-flash");
        assert_eq!(out["content"], json!([{"type": "text", "text": "Hi"}]));
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 11);
        assert_eq!(out["usage"]["output_tokens"], 5);
    }

    // ------------------------------------------------------------------- stream

    #[test]
    fn stream_sends_role_once_then_deltas_then_done() {
        reset_stream_state();
        let c = ctx();

        let first = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}"#,
        )
        .unwrap();
        assert_eq!(first.len(), 2, "role chunk then content chunk");
        let role = chunk(&first[0]);
        assert_eq!(role["object"], "chat.completion.chunk");
        assert_eq!(role["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(role["choices"][0]["finish_reason"], Value::Null);
        let text = chunk(&first[1]);
        assert_eq!(text["choices"][0]["delta"]["content"], "Hel");

        let second = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"lo"}]},"finishReason":"STOP"}]}"#,
        )
        .unwrap();
        assert_eq!(second.len(), 3, "content chunk, terminal chunk, [DONE]");
        let more = chunk(&second[0]);
        assert_eq!(more["choices"][0]["delta"]["content"], "lo");
        assert!(
            !second[0].contains("assistant"),
            "the role chunk is emitted exactly once per stream"
        );
        assert_eq!(chunk(&second[1])["choices"][0]["finish_reason"], "stop");
        assert!(
            second[2].contains("[DONE]"),
            "Chat clients hang without the terminator"
        );
    }

    #[test]
    fn stream_usage_and_finish_arrive_together() {
        reset_stream_state();
        let c = ctx();
        let frames = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"a"}]},"finishReason":"MAX_TOKENS"}],
                 "usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":4,"totalTokenCount":14}}"#,
        )
        .unwrap();
        assert_eq!(frames.len(), 4, "role, text, terminal, [DONE]");
        let done = chunk(&frames[2]);
        assert_eq!(done["choices"][0]["finish_reason"], "length");
        assert_eq!(done["usage"]["prompt_tokens"], 10);
        assert_eq!(done["usage"]["completion_tokens"], 4);
        assert_eq!(done["usage"]["total_tokens"], 14);

        // A repeated terminal frame after the stream closed emits nothing, least of all a
        // second [DONE].
        let again = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"a"}]},"finishReason":"MAX_TOKENS"}]}"#,
        )
        .unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn stream_tool_calls_get_indices_and_full_arguments() {
        reset_stream_state();
        let c = ctx();
        let one = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{"a":1}}}]}}]}"#,
        )
        .unwrap();
        assert_eq!(one.len(), 2, "role chunk then tool chunk");
        let call = &chunk(&one[1])["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "call_0");
        assert_eq!(call["function"]["name"], "f");
        assert_eq!(call["function"]["arguments"], "{\"a\":1}");

        let two = sse_to_pivot(
            &c,
            &json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "g"}}]}}]})
                .to_string(),
        )
        .unwrap();
        let call = &chunk(&two[0])["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 1, "indices continue across frames");
        assert_eq!(call["function"]["arguments"], "{}");

        let end = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}]}"#,
        )
        .unwrap();
        assert_eq!(
            chunk(&end[0])["choices"][0]["finish_reason"],
            json!("tool_calls"),
            "a completed call outranks the bare stop"
        );
        assert!(end[end.len() - 1].contains("[DONE]"));
    }

    #[test]
    fn stream_thoughts_become_reasoning_deltas() {
        reset_stream_state();
        let c = ctx();
        let frames = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[
                 {"thought":true,"text":"pondering"},{"text":"answer"}]}}]}"#,
        )
        .unwrap();
        assert_eq!(frames.len(), 3, "role, reasoning, content");
        assert_eq!(
            chunk(&frames[1])["choices"][0]["delta"]["reasoning_content"],
            "pondering"
        );
        assert_eq!(
            chunk(&frames[2])["choices"][0]["delta"]["content"],
            "answer"
        );
    }

    #[test]
    fn stream_noise_is_skipped_and_no_finish_means_no_done() {
        reset_stream_state();
        let c = ctx();
        assert!(sse_to_pivot(&c, "keep-alive commentary")
            .unwrap()
            .is_empty());
        assert!(sse_to_pivot(&c, "{}").unwrap().is_empty());

        let partial = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#,
        )
        .unwrap();
        assert!(
            !partial.iter().any(|f| f.contains("[DONE]")),
            "a truncated stream must not claim completion"
        );
    }

    #[test]
    fn stream_batch_array_translates_every_element() {
        reset_stream_state();
        let c = ctx();
        let frames = sse_to_pivot(
            &c,
            r#"[{"candidates":[{"content":{"parts":[{"text":"a"}]}}]},
                {"candidates":[{"content":{"parts":[{"text":"b"}]},"finishReason":"STOP"}]}]"#,
        )
        .unwrap();
        assert!(frames[1].contains(r#""content":"a""#));
        assert!(frames[2].contains(r#""content":"b""#));
        assert!(
            frames[4].contains("[DONE]"),
            "role, two text chunks, terminal, [DONE]"
        );
    }

    #[test]
    fn stream_envelope_wrapper_is_unwrapped_too() {
        reset_stream_state();
        let c = ctx();
        let frames = sse_to_pivot(
            &c,
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"hi"}]},
                 "finishReason":"STOP"}]},"traceId":"t"}"#,
        )
        .unwrap();
        assert!(
            frames[1].contains(r#""content":"hi""#),
            "reading candidates off the wrapper yields nothing"
        );
        assert!(frames[frames.len() - 1].contains("[DONE]"));
    }

    #[test]
    fn anthropic_stream_gets_message_stop_without_a_done_frame() {
        reset_stream_state();
        let c = anthropic_ctx();
        let start = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#,
        )
        .unwrap();
        let joined = start.join("");
        assert!(joined.contains("message_start"));
        assert!(joined.contains("content_block_start"));
        assert!(joined.contains("text_delta"));
        assert!(
            !joined.contains("[DONE]"),
            "an Anthropic SDK rejects a literal data: [DONE]"
        );

        let end = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[]},"finishReason":"MAX_TOKENS"}],
                 "usageMetadata":{"promptTokenCount":7,"candidatesTokenCount":3,"totalTokenCount":10}}"#,
        )
        .unwrap();
        let joined = end.join("");
        assert!(
            joined.contains("content_block_stop"),
            "the open block must close before the stream ends"
        );
        assert!(
            joined.contains("max_tokens"),
            "the real stop reason must survive"
        );
        assert!(joined.contains("message_stop"));
        assert!(!joined.contains("[DONE]"));
    }

    #[test]
    fn reset_stream_state_starts_the_next_stream_clean() {
        reset_stream_state();
        let c = ctx();
        let a = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#,
        )
        .unwrap();
        assert!(
            a[0].contains("assistant"),
            "first stream opens with a role chunk"
        );

        reset_stream_state();
        let b = sse_to_pivot(
            &c,
            r#"{"candidates":[{"content":{"parts":[{"text":"y"}]}}]}"#,
        )
        .unwrap();
        assert!(
            b[0].contains("assistant"),
            "a reset stream must open with a role chunk again"
        );
    }
}
