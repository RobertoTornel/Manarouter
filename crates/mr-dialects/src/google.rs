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
            // `project` is deliberately absent: it belongs to the onboarding row the OAuth
            // credential was minted from, and TranslateCtx carries only conversation-scoped
            // state, so the relay's endpoint layer must add it from the credential.
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
    if mime.is_empty() || !header.contains("base64") {
        return None;
    }
    Some((mime, payload))
}

/// Best-effort mime type for a URL, from its path extension.
fn mime_from_url(url: &str) -> &'static str {
    let path = url.split(|c| c == '?' || c == '#').next().unwrap_or(url);
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
