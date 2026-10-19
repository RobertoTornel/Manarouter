//! The Command Code CLI `alpha/generate` envelope and its private event stream.
//!
//! Command Code is not an OpenAI-shaped upstream even though a client reaches it with a Chat
//! Completions request. The whole body is replaced by a fixed envelope
//! (`config`/`memory`/`taste`/`skills`/`params`/`threadId`), and the answer always arrives as a
//! private `text-delta` / `tool-*` / `finish` event stream — there is no non-streaming response and
//! no `[DONE]` sentinel on the wire, so the translator is the only thing that knows where an answer
//! ends.
//!
//! Three details are easy to get wrong and are handled deliberately here:
//!
//! * `params.stream` is always `true`, whatever the client asked for. A non-streaming client still
//!   opens an SSE connection upstream; [`to_pivot_response`] exists to fold such a stream back into
//!   one JSON object.
//! * Tool declarations use `input_schema`, not `parameters`, and are **not** wrapped in
//!   `{type:"function", function:{…}}` — the upstream silently ignores a tool it cannot read.
//! * Errors are not OpenAI-shaped. They are `{"success":false,"error":{…}}`, both as an HTTP body
//!   and as an `error`-typed stream event, and are rewritten into the client's error shape.
//!
//! ## Headers are not here
//!
//! [`TranslatedRequest`] carries a body content type, a forced-stream flag and a path — not headers.
//! The relay must add `authorization: Bearer <key>`, `accept: text/event-stream`,
//! `x-command-code-version` ([`COMMAND_CODE_VERSION`]) and `x-cli-environment`
//! ([`CLI_ENVIRONMENT`]) itself; the constants are exposed for exactly that purpose.

use std::cell::RefCell;
use std::collections::HashMap;

use bytes::Bytes;
use mr_core::Dialect;
use serde_json::{json, Map, Value};

use crate::openai::{pivot_response_to_anthropic, AnthropicStreamState};
use crate::sse;
use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest, SSE_DONE};

/// CLI compatibility version sent in `x-command-code-version`.
///
/// The working CLI-envelope bridge supplied for this adapter uses version 1.82.1.
pub const COMMAND_CODE_VERSION: &str = "1.82.1";

/// The `x-cli-environment` value. Hard-coded upstream, not configurable.
pub const CLI_ENVIRONMENT: &str = "production";

/// The path appended to a provider's `CC_UPSTREAM_BASE` (default `https://api.commandcode.ai`).
pub const GENERATE_PATH: &str = "alpha/generate";

/// `max_tokens` when the client sends neither `max_completion_tokens` nor `max_tokens`.
pub const DEFAULT_MAX_TOKENS: u64 = 64_000;

/// `temperature` when the client omits it. The upstream default, not an OpenAI one.
pub const DEFAULT_TEMPERATURE: f64 = 0.3;

/// Convert a Chat Completions request body into a Command Code `alpha/generate` envelope.
///
/// The upstream is *always* streamed, so `force_stream` is unconditionally `true` even when the
/// client asked for a single JSON object: the relay must read an SSE stream back either way.
pub fn from_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
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

    let envelope = build_envelope(ctx, obj);
    let bytes = serde_json::to_vec(&envelope).map_err(|e| TranslateError::MalformedRequest {
        dialect: "openai",
        detail: e.to_string(),
    })?;

    Ok(TranslatedRequest {
        body: Bytes::from(bytes),
        content_type: "application/json",
        // Always true: `/alpha/generate` answers with an event stream regardless of the client's
        // `stream` flag. The flag is the relay's only signal that it must decode SSE on this hop.
        force_stream: true,
        path_override: Some(GENERATE_PATH.to_string()),
    })
}

/// Build the envelope object from an already-parsed Chat Completions request.
///
/// Kept public and separate from [`from_pivot_request`] so a caller holding a parsed body can build
/// the envelope without a second serialisation round trip.
pub fn build_envelope(ctx: &TranslateCtx<'_>, obj: &Map<String, Value>) -> Value {
    let empty: Vec<Value> = Vec::new();
    let messages = obj
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    // Assistant `tool_calls` are the only place a tool's name is recorded, and a later `tool`
    // message may carry only the id. Collect the map first so a tool result can be labelled.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for message in messages {
        for call in tool_calls_of(message) {
            let id = call.get("id").and_then(Value::as_str);
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str);
            if let (Some(id), Some(name)) = (id, name) {
                tool_names.insert(id.to_string(), name.to_string());
            }
        }
    }

    let mut system: Vec<String> = Vec::new();
    let mut cc_messages: Vec<Value> = Vec::new();

    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");

        // A system prompt is a top-level envelope field, never a message.
        if role == "system" {
            let text = content_to_string(message.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                system.push(text);
            }
            continue;
        }

        if role == "tool" {
            let id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let name = message
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| tool_names.get(id).cloned())
                .unwrap_or_else(|| "unknown".to_string());
            let value = content_to_string(message.get("content").unwrap_or(&Value::Null));
            // The upstream distinguishes a failed tool result from a successful one by textual
            // convention, not by a flag.
            let kind = if value.starts_with("Error:") {
                "error-text"
            } else {
                "text"
            };
            cc_messages.push(json!({
                "role": "tool",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": id,
                    "toolName": name,
                    "output": {"type": kind, "value": value},
                }],
            }));
            continue;
        }

        let mut parts: Vec<Value> = Vec::new();
        let text = content_to_string(message.get("content").unwrap_or(&Value::Null));
        if !text.is_empty() {
            parts.push(json!({"type": "text", "text": text}));
        }
        // Content parts are ordered text first, then tool calls, which is what the upstream
        // expects on an assistant turn that did both.
        for call in tool_calls_of(message) {
            let id = call.get("id").and_then(Value::as_str).unwrap_or("");
            let name = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = call.get("function").and_then(|f| f.get("arguments"));
            parts.push(json!({
                "type": "tool-call",
                "toolCallId": id,
                "toolName": name,
                "input": tool_input(args),
            }));
        }
        // The role is passed through unvalidated, so an unfamiliar `developer` role survives
        // instead of being silently rewritten into `user`.
        cc_messages.push(json!({"role": role, "content": parts}));
    }

    let tools: Vec<Value> = obj
        .get("tools")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter(|t| t.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|t| {
            let function = t.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            let mut tool = Map::new();
            tool.insert("name".into(), json!(name));
            if let Some(description) = function.get("description").and_then(Value::as_str) {
                tool.insert("description".into(), json!(description));
            }
            // `parameters` -> `input_schema` is the whole difference between a tool the upstream
            // reads and one it ignores.
            let schema = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
            tool.insert("input_schema".into(), schema);
            Some(Value::Object(tool))
        })
        .collect();

    let max_tokens = obj
        .get("max_completion_tokens")
        .and_then(Value::as_u64)
        .or_else(|| obj.get("max_tokens").and_then(Value::as_u64))
        .unwrap_or(DEFAULT_MAX_TOKENS);
    // `as_f64` keeps an explicit `0.0` (greedy sampling) rather than treating it as absent.
    let temperature = obj
        .get("temperature")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_TEMPERATURE);

    json!({
        "config": {
            "workingDir": ".",
            "date": utc_date(mr_core::ids::now_millis()),
            "environment": "cli",
            "structure": [],
            "isGitRepo": false,
            "currentBranch": "",
            "mainBranch": "main",
            "gitStatus": "",
            "recentCommits": [],
        },
        "memory": "",
        "taste": "",
        "skills": "",
        "params": {
            "model": ctx.upstream_model,
            "messages": cc_messages,
            "tools": tools,
            "system": system.join("\n"),
            "max_tokens": max_tokens,
            "temperature": temperature,
            "stream": true,
        },
        "threadId": mr_core::ids::new_id(),
    })
}

/// The `tool_calls` array of a message, or an empty slice.
fn tool_calls_of(message: &Value) -> &[Value] {
    message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

/// Flatten OpenAI's string-or-parts content into the plain text the envelope carries.
///
/// Mirrors the reference bridge: strings pass through, object parts contribute `text`, else a
/// string `content`, else an `[Image URL: …]` placeholder (there is no vision support on this hop),
/// and any other JSON value is stringified rather than dropped.
fn content_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                match part {
                    Value::String(s) => out.push_str(s),
                    Value::Object(_) => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            out.push_str(text);
                        } else if let Some(content) = part.get("content").and_then(Value::as_str) {
                            out.push_str(content);
                        } else if let Some(image) = part.get("image_url") {
                            let url = image
                                .as_str()
                                .map(str::to_string)
                                .or_else(|| {
                                    image.get("url").and_then(Value::as_str).map(str::to_string)
                                })
                                .unwrap_or_default();
                            out.push_str("[Image URL: ");
                            out.push_str(&url);
                            out.push(']');
                        }
                    }
                    _ => {}
                }
            }
            out
        }
        other => other.to_string(),
    }
}

/// Turn an OpenAI tool-call `arguments` value into the envelope's `input` object.
///
/// Arguments arrive as a JSON *string*; the upstream wants the parsed value. A string that is not
/// valid JSON is preserved under `arguments` rather than discarded, matching the reference bridge.
fn tool_input(arguments: Option<&Value>) -> Value {
    match arguments {
        Some(Value::Object(_)) => arguments.cloned().unwrap_or_else(|| json!({})),
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| json!({"arguments": s}))
        }
        _ => json!({}),
    }
}

/// A UTC `YYYY-MM-DD` date from Unix milliseconds, without a date-library dependency.
fn utc_date(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since the Unix epoch to a proleptic Gregorian `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, which is exact for every date this code can see and needs no
/// leap-second table.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if month <= 2 { year + 1 } else { year }, month, day)
}

// ---------------------------------------------------------------------------------------------
// The upstream event stream
// ---------------------------------------------------------------------------------------------

/// One accumulated tool call.
///
/// Both `id` and `name` are always non-empty. That is an invariant of this struct, not a hope:
/// a call the client cannot name cannot be dispatched, and a call with no id cannot be matched to
/// its result, so neither is ever constructed with an empty string — the upstream event that would
/// have produced one is dropped instead (see [`classify`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Everything that must survive from one upstream event to the next.
///
/// This is the Rust equivalent of the reference bridge's `content` / `toolCalls` / `toolIndices` /
/// `sentRole` / `finishReason` / `usage` locals, and it is deliberately per-stream: sharing it
/// between concurrent conversations is exactly how two answers get spliced together.
#[derive(Debug, Clone)]
pub(crate) struct StreamState {
    id: String,
    model: String,
    content: String,
    tool_calls: Vec<ToolCall>,
    /// Tool-call id to index in `tool_calls`. The incremental `tool-*` events name an id, and the
    /// client deltas must carry the *index* that id already has — not a fresh one per fragment.
    tool_index: HashMap<String, usize>,
    finish_reason: String,
    prompt_tokens: u64,
    completion_tokens: u64,
    /// Prompt tokens served from the provider's cache, and prompt tokens written *into* it.
    ///
    /// The live endpoint reports these under `totalUsage.inputTokenDetails.cacheReadTokens` and
    /// `.cacheWriteTokens`. They were previously dropped, which is why a cache hit showed as zero
    /// and the cost of every cached turn was overstated.
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    usage_seen: bool,
    /// `role: "assistant"` rides the first delta of any kind, and is never repeated.
    sent_role: bool,
    /// The terminal frame has been emitted; the next event starts a new stream.
    done: bool,
}

impl StreamState {
    fn new(ctx: &TranslateCtx<'_>) -> Self {
        Self {
            id: new_completion_id(),
            model: ctx.upstream_model.to_string(),
            content: String::new(),
            tool_calls: Vec::new(),
            tool_index: HashMap::new(),
            finish_reason: "stop".to_string(),
            prompt_tokens: 0,
            completion_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            usage_seen: false,
            sent_role: false,
            done: false,
        }
    }
}

/// What one upstream event contributed, in client-visible terms.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// A text fragment.
    Text(String),
    /// A tool call opened, atomically or incrementally. For `tool-call` the arguments are already
    /// complete; for `tool-use` / `tool-input-start` they are empty and arrive as [`Step::ToolArgs`].
    ToolOpen {
        index: usize,
        id: String,
        name: String,
        arguments: String,
    },
    /// A fragment appended to an open tool call.
    ///
    /// `arguments` alone is all a fragment legitimately carries: the call it belongs to was opened
    /// and named by a preceding event, and the client keys the block by the index that event
    /// established. Nothing here can create a block — [`open_tool_call`] is the only constructor —
    /// which is what keeps an id-less, nameless tool call from ever being emitted.
    ToolArgs { index: usize, arguments: String },
    /// The upstream said it was done.
    Finish,
    /// The upstream reported an error.
    Error(Value),
    /// Nothing the client can use: a keep-alive, an unknown event, or an empty delta.
    Ignored,
}

/// A non-empty string field of an upstream event, or `None`.
///
/// `Value::as_str` answers `Some("")` for a present-but-empty string, so a bare
/// `and_then(Value::as_str).unwrap_or_else(…)` fallback never runs for `"toolCallId": ""`. That is
/// how an empty upstream tool-call id used to reach the client verbatim as `id: ""`, which the
/// client then recorded as a tool call it could neither execute nor match — and which made the
/// session log fail its own format validation on the next read.
fn nonempty_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Fold one upstream event into `state` and report what the client should see.
fn classify(state: &mut StreamState, v: &Value) -> Step {
    // An error is recognised before the `type` switch because it may arrive either as an
    // `error`-typed event or as the `{"success":false,…}` body shape on a stream line.
    if let Some(err) = error_envelope(v) {
        state.done = true;
        return Step::Error(err);
    }

    let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
    match kind {
        "text-delta" => {
            let text = v.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return Step::Ignored;
            }
            state.content.push_str(text);
            Step::Text(text.to_string())
        }

        // Atomic: one event carries id, name and the complete argument string.
        "tool-call" => {
            let id = nonempty_str(v, "toolCallId")
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{}", mr_core::ids::new_id()));
            // `input` is an object upstream; stringify it because a Chat Completions client reads
            // `function.arguments` as a JSON *string*.
            let arguments = v
                .get("input")
                .map(|input| input.to_string())
                .unwrap_or_else(|| "{}".to_string());
            let name = v
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // A tool call the client cannot name is not a tool call: naming it `""` hands the
            // client a function it cannot dispatch. Drop the event and let the turn end as text,
            // which is the honest outcome for a malformed upstream frame.
            if name.is_empty() {
                return Step::Ignored;
            }
            // A repeat of a call already opened under this id is the *same* tool call, not a second
            // one. The incremental arm has always ignored the repeat; this arm did not, and
            // `open_tool_call` repoints `tool_index` at the new entry. That is how a single upstream
            // call reached the client as two blocks: the first holding the real argument string, the
            // second carrying the *same* id with `{}`, because the repeat event has no `input`. A
            // client can dispatch neither a duplicate id nor an empty argument object, and the
            // first entry is the one that holds the answer.
            if state.tool_index.contains_key(&id) {
                return Step::Ignored;
            }
            let index = open_tool_call(state, id.clone(), name.clone(), arguments.clone());
            state.finish_reason = "tool_calls".to_string();
            Step::ToolOpen {
                index,
                id,
                name,
                arguments,
            }
        }

        // Incremental: open with empty arguments, then append fragments.
        "tool-use" | "tool-input-start" => {
            // Both spellings must be non-empty: a present-but-empty id would otherwise be recorded
            // under `""` and could never be matched by a later fragment.
            let id = nonempty_str(v, "toolCallId")
                .or_else(|| nonempty_str(v, "id"))
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{}", mr_core::ids::new_id()));
            if state.tool_index.contains_key(&id) {
                return Step::Ignored;
            }
            let name = v
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            // A nameless incremental open cannot be dispatched either; see the atomic arm above.
            if name.is_empty() {
                return Step::Ignored;
            }
            let index = open_tool_call(state, id.clone(), name.clone(), String::new());
            state.finish_reason = "tool_calls".to_string();
            Step::ToolOpen {
                index,
                id,
                name,
                arguments: String::new(),
            }
        }

        "tool-delta" | "tool-input-delta" => {
            // An empty id is "no id", which falls through to the most-recently-opened call.
            let id = nonempty_str(v, "toolCallId")
                .or_else(|| nonempty_str(v, "id"))
                .unwrap_or("");
            let index = state
                .tool_index
                .get(id)
                .copied()
                // A fragment with no id, or with an id never opened, belongs to the most recently
                // opened call — that is what the reference bridge assumes.
                .or_else(|| state.tool_calls.len().checked_sub(1));
            let arguments = v
                .get("text")
                .or_else(|| v.get("delta"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let Some(index) = index else {
                return Step::Ignored;
            };
            if arguments.is_empty() {
                return Step::Ignored;
            }
            let Some(call) = state.tool_calls.get_mut(index) else {
                return Step::Ignored;
            };
            call.arguments.push_str(arguments);
            state.finish_reason = "tool_calls".to_string();
            Step::ToolArgs {
                index,
                arguments: arguments.to_string(),
            }
        }

        "finish" => {
            let reason = v.get("finishReason").and_then(Value::as_str).unwrap_or("");
            // A `stop` after a tool call must not erase `tool_calls`: last writer wins, but only
            // for the two length spellings the upstream uses.
            if reason == "length" || reason == "max_tokens" {
                state.finish_reason = "length".to_string();
            }
            if let Some(usage) = v.get("totalUsage") {
                state.prompt_tokens = usage
                    .get("inputTokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                state.completion_tokens = usage
                    .get("outputTokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                // Cache accounting lives one level down, under `inputTokenDetails`. The vendor CLI
                // reads exactly these two paths; anything else silently reports a cache miss.
                let details = usage.get("inputTokenDetails");
                state.cache_read_tokens = details
                    .and_then(|d| d.get("cacheReadTokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                state.cache_write_tokens = details
                    .and_then(|d| d.get("cacheWriteTokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                state.usage_seen = true;
            }
            Step::Finish
        }

        "error" => {
            // `{"type":"error"}` with the details beside it, or nested under `error`.
            let err = v
                .get("error")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let err = if err.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                v.clone()
            } else {
                err
            };
            state.done = true;
            Step::Error(err)
        }

        // `message-start`, `text-start`/`text-end`, `reasoning`, `thinking`, `step-*`, `usage`,
        // `tool-result` and any Anthropic-shaped event carry nothing this dialect can forward.
        _ => Step::Ignored,
    }
}

/// Append a tool call and record its id, returning its index.
fn open_tool_call(state: &mut StreamState, id: String, name: String, arguments: String) -> usize {
    let index = state.tool_calls.len();
    state.tool_index.insert(id.clone(), index);
    state.tool_calls.push(ToolCall {
        id,
        name,
        arguments,
    });
    index
}

/// A fresh `chatcmpl-…` id, matching the reference bridge's `requestId`.
fn new_completion_id() -> String {
    format!("chatcmpl-{}", mr_core::ids::new_id())
}

// ---------------------------------------------------------------------------------------------
// Errors — deliberately not OpenAI-shaped upstream
// ---------------------------------------------------------------------------------------------

/// Extract the upstream error object from the shapes Command Code actually sends.
///
/// The canonical failure body is `{"success":false,"error":{…}}`. An `error`-typed stream event
/// nests the same object under `error`, and a bare `{"error":{…}}` is accepted too so a shape
/// change does not turn a reported failure into a silent empty answer.
fn error_envelope(v: &Value) -> Option<Value> {
    let failed = v.get("success").and_then(Value::as_bool) == Some(false);
    if let Some(error) = v.get("error") {
        if failed || v.get("choices").is_none() {
            // An inner `{"error":{…}}` is already the error object; an inner non-object is kept as
            // the message.
            return Some(error.clone());
        }
    }
    if failed {
        return Some(v.clone());
    }
    None
}

/// The human-readable message from an upstream error object.
fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| "Command Code upstream error".to_string())
}

/// Rewrite an upstream error into the client's dialect error shape.
///
/// `code`, `status` and `docs` are preserved where present: dropping them is how a caller loses the
/// one field that explains a failure.
fn error_to_client(ctx: &TranslateCtx<'_>, error: &Value) -> Value {
    let message = error_message(error);
    if ctx.client_dialect == Dialect::Anthropic {
        return json!({
            "type": "error",
            "error": {"type": "api_error", "message": message},
        });
    }

    let mut err = Map::new();
    err.insert("message".into(), json!(message));
    err.insert("type".into(), json!("api_error"));
    for key in ["code", "status", "docs"] {
        if let Some(value) = error.get(key) {
            if !value.is_null() {
                err.insert(key.into(), value.clone());
            }
        }
    }
    json!({"error": err})
}

// ---------------------------------------------------------------------------------------------
// Upstream stream -> one client response
// ---------------------------------------------------------------------------------------------

/// Fold a complete Command Code stream (or a single non-streaming body) into the client dialect.
///
/// The upstream never answers with a JSON object — `accept: text/event-stream` is always set — so
/// this function exists for the case where the relay has already buffered a whole stream, and for
/// the failure path, where the buffered body is an error envelope rather than a stream at all.
pub fn to_pivot_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    let text = std::str::from_utf8(body).map_err(|e| TranslateError::MalformedResponse {
        dialect: "command-code-go",
        detail: e.to_string(),
    })?;

    let mut state = StreamState::new(ctx);
    let mut error: Option<Value> = None;
    let mut existing: Option<Value> = None;

    for line in text.lines() {
        let candidate = line
            .trim()
            .strip_prefix("data:")
            .unwrap_or(line.trim())
            .trim();
        if candidate.is_empty() || candidate == SSE_DONE {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(candidate) else {
            // An unparseable line is skipped, exactly as the reference framing loop does.
            continue;
        };
        if let Some(err) = error_envelope(&v) {
            error = Some(err);
            break;
        }
        // The live endpoint also emits bare JSON metadata events (`start`, `start-step`, etc.).
        // They are not completion payloads and must not disturb the folded result.
        if v.get("type").and_then(Value::as_str).is_some() {
            if let Step::Error(err) = classify(&mut state, &v) {
                error = Some(err);
                break;
            }
            continue;
        }
        // A body that is already a Chat Completions object is passed on rather than re-derived.
        if existing.is_none() && v.get("choices").is_some() {
            existing = Some(v);
            continue;
        }
        if let Step::Error(err) = classify(&mut state, &v) {
            error = Some(err);
            break;
        }
    }

    if let Some(err) = error {
        return to_bytes(error_to_client(ctx, &err));
    }

    let pivot = match existing {
        Some(mut v) => {
            if v.get("id").and_then(Value::as_str).is_none() {
                v["id"] = json!(state.id);
            }
            if v.get("model").and_then(Value::as_str).is_none() {
                v["model"] = json!(state.model);
            }
            v
        }
        None => completion_object(&state),
    };

    let out = match ctx.client_dialect {
        Dialect::Anthropic => {
            let mut anthropic = AnthropicStreamState::default();
            pivot_response_to_anthropic(ctx, &pivot, &mut anthropic)?
        }
        _ => pivot,
    };
    to_bytes(out)
}

/// Build one `chat.completion` object from the accumulated stream.
fn completion_object(state: &StreamState) -> Value {
    let message = if state.tool_calls.is_empty() {
        json!({"role": "assistant", "content": state.content})
    } else {
        let tool_calls: Vec<Value> = state
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                })
            })
            .collect();
        // `content` is null whenever `tool_calls` is present — the client's own rule, restated.
        json!({"role": "assistant", "content": Value::Null, "tool_calls": tool_calls})
    };

    json!({
        "id": state.id,
        "object": "chat.completion",
        "created": mr_core::ids::now_millis() / 1000,
        "model": state.model,
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": state.finish_reason,
        }],
        "usage": {
            "prompt_tokens": state.prompt_tokens,
            "completion_tokens": state.completion_tokens,
            "total_tokens": state.prompt_tokens + state.completion_tokens,
            // OpenAI's own spellings, so an OpenAI-shaped client (and this gateway's own usage tap)
            // sees the cache split without having to know Command Code's nesting.
            "prompt_tokens_details": {
                "cached_tokens": state.cache_read_tokens,
            },
            "cache_read_input_tokens": state.cache_read_tokens,
            "cache_write_input_tokens": state.cache_write_tokens,
        },
    })
}

/// Serialise a translated value, reporting a serialisation failure as a response error.
fn to_bytes(value: Value) -> Result<Bytes> {
    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|e| TranslateError::MalformedResponse {
            dialect: "command-code-go",
            detail: e.to_string(),
        })
}

/// Per-stream translation state for the Command Code dialect.
///
/// ## Why this is an owned value and not a thread-local
///
/// A Command Code answer arrives as a private event stream, so the translator has to remember what
/// it has already seen: the accumulated text, every tool call opened so far, whether the opening
/// role has been sent, the finish reason, and the usage the `finish` event reports. That state
/// belongs to **one stream**.
///
/// It used to live in a `thread_local!`, cleared between streams by [`reset_stream_state`]. A
/// thread-local is keyed by the *worker thread*, and a thread is not a stream: the relay polls a
/// response body on whichever runtime worker is free, and two answers are routinely in flight on
/// the same worker at once. Both wrote into the same cell, and the damage was specific and
/// reproducible — one stream's `tool-delta` argument fragments were appended to the *other*
/// stream's tool call, because the fragment arm falls back to "the most recently opened call" when
/// its id is not found in the state it is holding.
///
/// The client then saw a second tool call with no id and no name. `deepseek-harness` can neither
/// execute nor match such a call, so the turn died and the session log failed validation with
/// `format v4 tool/result at seq N requires toolCallId matching its tool source`. Clearing the cell
/// on a stream's first frame made that worse rather than better: it wiped the *other* live stream's
/// state, which is the same defect seen from the other side.
///
/// Owning the state removes the whole class of bug: there is no shared cell, so there is nothing to
/// reset and nothing to splice.
///
/// ## Why it also holds the Anthropic half
///
/// For an Anthropic client, every pivot chunk this dialect produces is framed by the inverse
/// translator, which keeps its own accumulation (the open content block, tool-block indices,
/// `message_start` bookkeeping). That state is *also* per stream, so it rides here; otherwise the
/// Anthropic half of a stream would splice while the Chat Completions half did not.
#[derive(Debug)]
pub struct Translator {
    /// The Command Code event accumulation, absent until the first frame arrives.
    state: Option<StreamState>,
    /// The pivot→Anthropic framing state, used only when the client speaks Anthropic.
    anthropic: AnthropicStreamState,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    /// A translator with nothing accumulated.
    ///
    /// Create exactly one per upstream stream and drop it when the stream ends. Sharing one between
    /// two streams reintroduces the splice documented on this type.
    pub fn new() -> Self {
        Self {
            state: None,
            anthropic: AnthropicStreamState::default(),
        }
    }

    /// Whether a terminal frame has been translated, so the next event starts a fresh answer.
    pub fn is_finished(&self) -> bool {
        self.state.as_ref().map(|s| s.done).unwrap_or(false)
    }
}

thread_local! {
    /// **Retained only for the legacy entry point.** [`translate`] does not read this; its state is
    /// the [`Translator`] the caller owns. Kept so [`reset_stream_state`] still means something to a
    /// caller that has not moved to the per-stream API.
    #[allow(dead_code)]
    static COMMAND_CODE_STREAM_STATE: RefCell<Option<StreamState>> = const { RefCell::new(None) };
}

/// Forget the legacy per-thread Command Code state.
///
/// **This is not how correctness is achieved any more.** A thread is not a stream, so this cannot
/// separate two concurrent answers; [`Translator`] is what does that. Retained for callers still on
/// the old entry point.
pub fn reset_stream_state() {
    COMMAND_CODE_STREAM_STATE.with(|cell| *cell.borrow_mut() = None);
}

/// Translate one Command Code event into client-dialect frames, against caller-owned state.
///
/// This is the entry point the relay uses. `translator` must be the same value for every frame of
/// one stream and a fresh value for every other stream; see [`Translator`] for why.
pub fn translate(
    translator: &mut Translator,
    ctx: &TranslateCtx<'_>,
    data: &str,
) -> Result<Vec<String>> {
    let trimmed = data.trim();
    // The relay strips the `data:` prefix before calling this, but a raw NDJSON line would not
    // have one, and tolerating both costs nothing.
    let candidate = trimmed.strip_prefix("data:").unwrap_or(trimmed).trim();
    if candidate.is_empty() || candidate == SSE_DONE {
        return Ok(Vec::new());
    }
    let Ok(v) = serde_json::from_str::<Value>(candidate) else {
        // A keep-alive or a line the upstream never intended as an event: skipped, never fatal,
        // because one bad frame must not kill an answer already half-delivered.
        return Ok(Vec::new());
    };

    // Disjoint borrows of the two fields, so the Anthropic framing below can run while `state` is
    // still borrowed for the chunk building above.
    let Translator { state, anthropic } = translator;
    let state = state.get_or_insert_with(|| StreamState::new(ctx));
    let step = classify(state, &v);
    let mut out: Vec<String> = Vec::new();
    let mut chunks: Vec<Value> = Vec::new();
    let mut terminate = false;

    match &step {
        Step::Text(text) => {
            let delta = with_role(state, json!({"content": text}));
            chunks.push(chunk(state, delta));
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
            chunks.push(chunk(state, delta));
        }

        Step::ToolArgs { index, arguments } => {
            let delta = with_role(
                state,
                json!({"tool_calls": [{
                    "index": index,
                    "function": {"arguments": arguments},
                }]}),
            );
            chunks.push(chunk(state, delta));
        }

        Step::Finish => {
            // An answer that produced no delta at all still gets a role chunk, so an Anthropic
            // client has a `message_start` to hang its `message_stop` on.
            if !state.sent_role {
                let delta = with_role(state, json!({}));
                chunks.push(chunk(state, delta));
            }
            state.done = true;
            chunks.push(final_chunk(state));
            terminate = true;
        }

        Step::Error(err) => {
            // The reference bridge swallows this event and reports a normal `stop`. That turns
            // a real failure into an empty answer, so the error is forwarded instead — in the
            // client's own error shape, and followed by that dialect's terminator.
            state.done = true;
            let body = error_to_client(ctx, err);
            match ctx.client_dialect {
                Dialect::Anthropic => out.push(sse::named_frame("error", &body.to_string())),
                _ => {
                    out.push(sse::data_frame(&body.to_string()));
                    out.push(sse::data_frame(SSE_DONE));
                }
            }
        }

        Step::Ignored => {}
    }

    // Every chunk is a Chat Completions chunk at this point; who is listening decides the
    // framing. An Anthropic client's stream is rebuilt by the inverse translator rather than
    // handed OpenAI frames it would reject.
    for c in &chunks {
        let body = c.to_string();
        match ctx.client_dialect {
            Dialect::Anthropic => {
                out.extend(crate::openai::translate_chunk_for(anthropic, ctx, &body)?)
            }
            _ => out.push(sse::data_frame(&body)),
        }
    }

    if terminate {
        match ctx.client_dialect {
            Dialect::Anthropic => out.extend(crate::openai::translate_chunk_for(
                anthropic, ctx, SSE_DONE,
            )?),
            _ => out.push(sse::data_frame(SSE_DONE)),
        }
    }

    Ok(out)
}

/// Translate one Command Code event into client-dialect frames.
///
/// A `finish` event is the only end-of-answer signal this upstream sends — there is no `[DONE]` on
/// the wire — so the terminal `delta:{}` + `finish_reason` frame (and the client's own terminator)
/// is emitted here rather than left to the relay.
///
/// **Prefer [`translate`].** This entry point accumulates into a per-thread cell, and a thread is
/// not a stream, so two concurrent Command Code streams on one worker contaminate each other — see
/// [`Translator`]. It is retained for callers that drive a single stream on a thread of their own,
/// and as the Command Code arm of [`crate::translate_sse_data`].
pub fn sse_to_pivot(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    COMMAND_CODE_STREAM_STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|s| s.done).unwrap_or(false) {
            *slot = None;
        }
        let mut translator = Translator {
            state: slot.take(),
            anthropic: AnthropicStreamState::default(),
        };
        let out = translate(&mut translator, ctx, data);
        // A finished stream leaves nothing behind; an unfinished one is parked for its next frame.
        *slot = translator.state.filter(|s| !s.done);
        out
    })
}

/// Add `role: "assistant"` to the first delta of a stream, and to no other.
fn with_role(state: &mut StreamState, mut delta: Value) -> Value {
    if !state.sent_role {
        if let Some(obj) = delta.as_object_mut() {
            obj.insert("role".into(), json!("assistant"));
        }
        state.sent_role = true;
    }
    delta
}

/// A `chat.completion.chunk` carrying one delta.
fn chunk(state: &StreamState, delta: Value) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "created": mr_core::ids::now_millis() / 1000,
        "model": state.model,
        "choices": [{"index": 0, "delta": delta}],
    })
}

/// The terminal chunk: an empty delta carrying the finish reason.
///
/// Usage is deliberately absent, matching both the upstream and the reference bridge — this hop
/// never receives a `stream_options.include_usage` usage frame, and inventing one would lie.
fn final_chunk(state: &StreamState) -> Value {
    json!({
        "id": state.id,
        "object": "chat.completion.chunk",
        "created": mr_core::ids::now_millis() / 1000,
        "model": state.model,
        "choices": [{"index": 0, "delta": {}, "finish_reason": state.finish_reason}],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::RouteKind;

    fn ctx() -> TranslateCtx<'static> {
        TranslateCtx::new(
            RouteKind::ChatCompletions,
            Dialect::CommandCodeGo,
            "cmd-model",
        )
    }

    fn anthropic_ctx() -> TranslateCtx<'static> {
        let mut c = ctx();
        c.client_dialect = Dialect::Anthropic;
        c
    }

    /// Reset every thread-local a translation touches, so one test cannot see another's stream.
    fn reset_all() {
        crate::reset_stream_state();
    }

    /// Every tool-call delta in a set of frames, as `(index, id, name)`.
    ///
    /// Ids and names are `Option` so a test can tell "absent" from "present but empty" — the
    /// distinction the whole regression turns on.
    fn tool_deltas(frames: &[String]) -> Vec<(u64, Option<String>, Option<String>)> {
        let mut out = Vec::new();
        for frame in frames {
            let parsed = sse::parse_frame(frame);
            if parsed.data == "[DONE]" {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&parsed.data) else {
                continue;
            };
            let Some(calls) = v["choices"][0]["delta"].get("tool_calls") else {
                continue;
            };
            for call in calls.as_array().unwrap() {
                out.push((
                    call["index"].as_u64().unwrap_or(0),
                    call.get("id").and_then(Value::as_str).map(str::to_string),
                    call["function"]
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                ));
            }
        }
        out
    }

    fn build(v: Value) -> Value {
        let body = serde_json::to_vec(&v).unwrap();
        let t = from_pivot_request(&ctx(), &body).unwrap();
        serde_json::from_slice(&t.body).unwrap()
    }

    fn data_json(frame: &str) -> Value {
        let frame = sse::parse_frame(frame);
        serde_json::from_str(&frame.data).unwrap()
    }

    #[test]
    fn the_envelope_always_carries_every_top_level_and_config_field() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "user", "content": "hi"}],
        }));

        assert_eq!(out["memory"], json!(""));
        assert_eq!(out["taste"], json!(""));
        assert_eq!(out["skills"], json!(""));
        assert_eq!(out["config"]["workingDir"], ".");
        assert_eq!(out["config"]["environment"], "cli");
        assert_eq!(out["config"]["structure"], json!([]));
        assert_eq!(out["config"]["isGitRepo"], false);
        assert_eq!(out["config"]["currentBranch"], "");
        assert_eq!(out["config"]["mainBranch"], "main");
        assert_eq!(out["config"]["gitStatus"], "");
        assert_eq!(out["config"]["recentCommits"], json!([]));
        // A YYYY-MM-DD date, checked as a shape because the value is today's.
        let date = out["config"]["date"].as_str().unwrap();
        assert_eq!(date.len(), 10);
        assert_eq!(date.as_bytes()[4], b'-');
        assert_eq!(date.as_bytes()[7], b'-');

        assert_eq!(out["params"]["model"], "cmd-model");
        // The upstream is streamed whatever the client asked for.
        assert_eq!(out["params"]["stream"], true);
        assert!(out["threadId"].as_str().unwrap().len() >= 36);
    }

    #[test]
    fn every_request_gets_a_fresh_thread_id() {
        let a = build(json!({"model": "x", "messages": []}));
        let b = build(json!({"model": "x", "messages": []}));
        assert_ne!(a["threadId"], b["threadId"]);
    }

    #[test]
    fn the_request_names_alpha_generate_and_forces_streaming() {
        let body = serde_json::to_vec(&json!({"model": "x", "messages": []})).unwrap();
        let t = from_pivot_request(&ctx(), &body).unwrap();
        assert_eq!(t.path_override.as_deref(), Some("alpha/generate"));
        assert!(t.force_stream);
        assert_eq!(t.content_type, "application/json");
    }

    #[test]
    fn supplied_bridge_defaults_match_the_live_cli_envelope() {
        let out = build_envelope(
            &TranslateCtx::new(
                RouteKind::ChatCompletions,
                Dialect::CommandCodeGo,
                "deepseek/deepseek-v4-flash",
            ),
            json!({"model": "deepseek/deepseek-v4-flash", "messages": []})
                .as_object()
                .unwrap(),
        );
        assert_eq!(COMMAND_CODE_VERSION, "1.82.1");
        assert_eq!(out["config"]["workingDir"], ".");
        assert_eq!(out["config"]["environment"], "cli");
        assert_eq!(out["config"]["mainBranch"], "main");
        assert_eq!(out["config"]["structure"], json!([]));
        assert_eq!(out["config"]["recentCommits"], json!([]));
        assert_eq!(out["params"]["system"], "");
        assert_eq!(out["params"]["tools"], json!([]));
        assert_eq!(out["params"]["model"], "deepseek/deepseek-v4-flash");
    }

    #[test]
    fn max_tokens_and_temperature_defaults_match_the_upstream_builder() {
        let out = build(json!({"model": "x", "messages": []}));
        assert_eq!(out["params"]["max_tokens"], 64_000);
        assert_eq!(out["params"]["temperature"], 0.3);
        assert_eq!(out["config"]["workingDir"], ".");
        assert_eq!(out["config"]["environment"], "cli");
        assert_eq!(out["config"]["mainBranch"], "main");
        assert_eq!(out["params"]["system"], "");

        // `??` semantics: an explicit zero is respected, not treated as absent.
        let out = build(json!({"model": "x", "messages": [], "max_tokens": 0, "temperature": 0}));
        assert_eq!(out["params"]["max_tokens"], 0);
        assert_eq!(out["params"]["temperature"], 0.0);

        // `max_completion_tokens` wins when both are present.
        let out = build(json!({
            "model": "x", "messages": [],
            "max_completion_tokens": 7, "max_tokens": 42,
        }));
        assert_eq!(out["params"]["max_tokens"], 7);
    }

    #[test]
    fn system_messages_are_lifted_out_of_the_message_list() {
        let out = build(json!({
            "model": "x",
            "messages": [
                {"role": "system", "content": "a"},
                {"role": "system", "content": [{"type": "text", "text": "b"}]},
                {"role": "user", "content": "hi"},
            ],
        }));
        assert_eq!(out["params"]["system"], "a\nb");
        let messages = out["params"]["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(
            messages[0]["content"][0],
            json!({"type": "text", "text": "hi"})
        );
    }

    #[test]
    fn tools_use_input_schema_and_drop_the_function_wrapper() {
        let out = build(json!({
            "model": "x", "messages": [],
            "tools": [
                {"type": "function", "function": {"name": "f", "description": "d", "parameters": {"type": "object"}}},
                {"type": "not-a-function", "function": {"name": "g"}},
                {"type": "function", "function": {}},
            ],
        }));
        let tools = out["params"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "f");
        assert_eq!(tools[0]["description"], "d");
        assert_eq!(tools[0]["input_schema"], json!({"type": "object"}));
        assert!(tools[0].get("parameters").is_none());
        assert!(tools[0].get("type").is_none());

        let out = build(json!({
            "model": "x", "messages": [],
            "tools": [{"type": "function", "function": {"name": "f"}}],
        }));
        assert_eq!(
            out["params"]["tools"][0]["input_schema"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn assistant_tool_calls_keep_text_first_and_parse_their_arguments() {
        let out = build(json!({
            "model": "x",
            "messages": [
                {"role": "assistant", "content": "thinking", "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "Error: boom"},
            ],
        }));
        let messages = out["params"]["messages"].as_array().unwrap();
        let parts = messages[0]["content"].as_array().unwrap();
        assert_eq!(parts[0], json!({"type": "text", "text": "thinking"}));
        assert_eq!(parts[1]["type"], "tool-call");
        assert_eq!(parts[1]["toolCallId"], "call_1");
        assert_eq!(parts[1]["toolName"], "f");
        assert_eq!(parts[1]["input"], json!({"a": 1}));

        // The tool result recovers the name from the assistant call, and an `Error:` prefix is the
        // only signal the upstream has that the tool failed.
        let result = &messages[1]["content"][0];
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(result["type"], "tool-result");
        assert_eq!(result["toolName"], "f");
        assert_eq!(
            result["output"],
            json!({"type": "error-text", "value": "Error: boom"})
        );
    }

    #[test]
    fn unparseable_arguments_are_preserved_rather_than_dropped() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "assistant", "tool_calls": [
                {"id": "c", "function": {"name": "f", "arguments": "not json"}}
            ]}],
        }));
        assert_eq!(
            out["params"]["messages"][0]["content"][0]["input"],
            json!({"arguments": "not json"})
        );
    }

    #[test]
    fn image_parts_become_a_text_placeholder_because_there_is_no_vision_here() {
        let out = build(json!({
            "model": "x",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "see "},
                {"type": "image_url", "image_url": {"url": "http://x/y.png"}},
            ]}],
        }));
        assert_eq!(
            out["params"]["messages"][0]["content"][0]["text"],
            "see [Image URL: http://x/y.png]"
        );
    }

    #[test]
    fn utc_dates_are_computed_without_a_date_library() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(86_400_000), "1970-01-02");
        assert_eq!(utc_date(1_700_000_000_000), "2023-11-14");
        // A leap day, which is where a naive implementation drifts.
        assert_eq!(utc_date(1_709_164_800_000), "2024-02-29");
    }

    #[test]
    fn text_deltas_open_with_the_role_and_then_only_carry_content() {
        reset_all();
        let frames = sse_to_pivot(&ctx(), r#"{"type":"text-delta","text":"Hel"}"#).unwrap();
        assert_eq!(frames.len(), 1);
        let v = data_json(&frames[0]);
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["model"], "cmd-model");
        assert_eq!(v["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(v["choices"][0]["delta"]["content"], "Hel");

        let frames = sse_to_pivot(&ctx(), r#"{"type":"text-delta","text":"lo"}"#).unwrap();
        let v = data_json(&frames[0]);
        assert!(v["choices"][0]["delta"].get("role").is_none());
        assert_eq!(v["choices"][0]["delta"]["content"], "lo");
    }

    #[test]
    fn incremental_tool_events_keep_one_index_across_fragments() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"t1","toolName":"search"}"#,
        )
        .unwrap();
        let v = data_json(&frames[0]);
        assert_eq!(v["choices"][0]["delta"]["role"], "assistant");
        let call = &v["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["id"], "t1");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["name"], "search");
        assert_eq!(call["function"]["arguments"], "");

        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-input-delta","toolCallId":"t1","delta":"{\"q\":"}"#,
        )
        .unwrap();
        let v = data_json(&frames[0]);
        let call = &v["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["index"], 0);
        assert_eq!(call["function"]["arguments"], "{\"q\":");
        assert!(v["choices"][0]["delta"].get("role").is_none());
        assert!(call.get("id").is_none());
    }

    #[test]
    fn an_atomic_tool_call_carries_complete_arguments_in_one_frame() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-call","toolCallId":"c1","toolName":"f","input":{"a":1}}"#,
        )
        .unwrap();
        let call = &data_json(&frames[0])["choices"][0]["delta"]["tool_calls"][0];
        assert_eq!(call["id"], "c1");
        assert_eq!(call["function"]["name"], "f");
        assert_eq!(call["function"]["arguments"], "{\"a\":1}");
    }

    /// A present-but-empty `toolCallId` must mint a real id, never `id: ""`.
    ///
    /// `Value::as_str` answers `Some("")`, so the `unwrap_or_else` id fallback never used to run.
    /// `""` reached the client, which advertised an unexecutable tool call under it and wrote a
    /// session log that failed v4 validation as `tool call id requires a nonempty string`.
    #[test]
    fn an_empty_tool_call_id_is_replaced_rather_than_forwarded() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-call","toolCallId":"","toolName":"f","input":{"a":1}}"#,
        )
        .unwrap();
        let call = &data_json(&frames[0])["choices"][0]["delta"]["tool_calls"][0];
        let id = call["id"].as_str().unwrap();
        assert!(!id.is_empty(), "an empty upstream id must be replaced");
        assert!(
            id.starts_with("call_"),
            "the minted id keeps the call_ shape"
        );
        assert_eq!(call["function"]["name"], "f");
    }

    #[test]
    fn an_empty_incremental_tool_call_id_is_replaced_rather_than_forwarded() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-input-start","toolCallId":"","toolName":"search"}"#,
        )
        .unwrap();
        let call = &data_json(&frames[0])["choices"][0]["delta"]["tool_calls"][0];
        let id = call["id"].as_str().unwrap();
        assert!(!id.is_empty(), "an empty upstream id must be replaced");
        assert!(id.starts_with("call_"));
    }

    /// A tool call the client cannot name is dropped, never emitted as `name: ""`.
    #[test]
    fn a_nameless_tool_call_is_ignored_in_both_paths() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-call","toolCallId":"c1","toolName":"","input":{}}"#,
        )
        .unwrap();
        assert!(
            frames.is_empty(),
            "a nameless atomic tool call must not reach the client"
        );

        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-input-start","toolCallId":"c2","toolName":""}"#,
        )
        .unwrap();
        assert!(
            frames.is_empty(),
            "a nameless incremental tool call must not reach the client"
        );
    }

    /// A repeated atomic `tool-call` under the same id is the same call, not a second one.
    ///
    /// This is the defect that reached a real client as *two* content blocks for one upstream call:
    /// the first holding the whole argument string and the second the **same id** with `{}`, because
    /// the repeat event carries no `input`. `open_tool_call` also repointed `tool_index` at the new,
    /// empty entry, so every later fragment for that id appended to the wrong call. The incremental
    /// arm has always ignored a duplicate id; the atomic arm has to as well.
    #[test]
    fn a_repeated_atomic_tool_call_does_not_open_a_second_call() {
        reset_all();
        let event = |raw: &str| serde_json::from_str::<Value>(raw).unwrap();

        let first = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-call","toolCallId":"call_same","toolName":"write","input":{"content":"hello"}}"#,
        )
        .unwrap();
        let second = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-call","toolCallId":"call_same","toolName":"write"}"#,
        )
        .unwrap();

        assert_eq!(
            tool_deltas(&first).len(),
            1,
            "the first event opens the call"
        );
        assert!(
            second.is_empty(),
            "a repeat of an already-open call must emit nothing, got {second:?}"
        );

        // And the accumulated call is still the first one, still holding the real arguments.
        let mut state = StreamState::new(&ctx());
        classify(
            &mut state,
            &event(
                r#"{"type":"tool-call","toolCallId":"call_same","toolName":"write","input":{"content":"hello"}}"#,
            ),
        );
        classify(
            &mut state,
            &event(r#"{"type":"tool-call","toolCallId":"call_same","toolName":"write"}"#),
        );
        let folded = completion_object(&state);
        let calls = folded["choices"][0]["message"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(
            calls.len(),
            1,
            "one upstream call must fold to one tool call"
        );
        assert_eq!(calls[0]["id"], "call_same");
        assert_eq!(
            calls[0]["function"]["arguments"], "{\"content\":\"hello\"}",
            "the surviving call keeps the arguments the first event carried"
        );
    }

    /// A later fragment must not reach into a call an abandoned stream left behind.
    ///
    /// Adapters accumulate into a thread-local, so a stream that ends without a terminal frame —
    /// the upstream died, the client disconnected — leaves its calls behind for the next stream on
    /// that worker. `reset_stream_state` is what the relay calls at each stream boundary; this is
    /// the leak it closes, and the reason a stale index can no longer be chosen.
    #[test]
    fn an_abandoned_stream_cannot_donate_a_tool_call_to_the_next_one() {
        reset_all();
        // A stream that opens a call and then simply stops, with no `finish`.
        let opened = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-call","toolCallId":"abandoned","toolName":"write","input":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(tool_deltas(&opened).len(), 1);

        // The relay's stream boundary.
        crate::reset_stream_state();

        // A fresh stream whose first event is an id-less fragment. Without the reset this would
        // append to `abandoned` and be reported under a call this client never saw opened.
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-input-delta","delta":"{\"stolen\":true}"}"#,
        )
        .unwrap();
        assert!(
            frames.is_empty(),
            "an id-less fragment with no open call must be dropped, got {frames:?}"
        );

        // And the new stream's own completion must not mention the dead call.
        let mut state = StreamState::new(&ctx());
        classify(
            &mut state,
            &serde_json::from_str(r#"{"type":"finish","finishReason":"stop"}"#).unwrap(),
        );
        let folded = completion_object(&state);
        assert!(
            folded["choices"][0]["message"].get("tool_calls").is_none(),
            "the abandoned call must not appear in the next stream: {folded}"
        );
    }

    /// The regression in full: a stream whose malformed frames used to emit `id: ""` must now
    /// emit no empty id at all, while the well-formed call in the same stream is untouched.
    #[test]
    fn no_emitted_delta_ever_carries_an_empty_tool_call_id_or_name() {
        reset_all();
        let events = [
            r#"{"type":"tool-call","toolCallId":"good_1","toolName":"bash","input":{"command":"ls"}}"#,
            r#"{"type":"tool-call","toolCallId":"","toolName":"","input":{}}"#,
            r#"{"type":"tool-input-start","toolCallId":"","toolName":""}"#,
            r#"{"type":"tool-input-delta","toolCallId":"","delta":"{\"a\":1}"}"#,
            r#"{"type":"finish","finishReason":"tool_calls"}"#,
        ];
        let mut seen = 0;
        for event in events {
            for frame in sse_to_pivot(&ctx(), event).unwrap() {
                let frame = sse::parse_frame(&frame);
                if frame.data == "[DONE]" {
                    continue;
                }
                let v: Value = serde_json::from_str(&frame.data).unwrap();
                let Some(calls) = v["choices"][0]["delta"].get("tool_calls") else {
                    continue;
                };
                for call in calls.as_array().unwrap() {
                    seen += 1;
                    if let Some(id) = call.get("id") {
                        assert!(!id.as_str().unwrap().is_empty(), "id must not be empty");
                    }
                    if let Some(name) = call["function"].get("name") {
                        assert!(!name.as_str().unwrap().is_empty(), "name must not be empty");
                    }
                }
            }
        }
        assert_eq!(seen, 2, "the one good open plus its argument fragment");
    }

    #[test]
    fn finish_emits_the_terminal_frame_and_then_the_done_sentinel() {
        reset_all();
        sse_to_pivot(&ctx(), r#"{"type":"text-delta","text":"hi"}"#).unwrap();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":3,"outputTokens":1}}"#,
        )
        .unwrap();
        assert_eq!(frames.len(), 2);

        let v = data_json(&frames[0]);
        assert_eq!(v["choices"][0]["delta"], json!({}));
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        // No usage frame is fabricated: this upstream never sends one.
        assert!(v.get("usage").is_none());
        assert_eq!(frames[1], "data: [DONE]\n\n");
    }

    #[test]
    fn a_late_stop_does_not_erase_the_tool_calls_finish_reason() {
        reset_all();
        sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"t1","toolName":"f"}"#,
        )
        .unwrap();
        let frames = sse_to_pivot(&ctx(), r#"{"type":"finish","finishReason":"stop"}"#).unwrap();
        assert_eq!(
            data_json(&frames[0])["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[test]
    fn max_tokens_is_reported_as_a_length_finish() {
        reset_all();
        let frames =
            sse_to_pivot(&ctx(), r#"{"type":"finish","finishReason":"max_tokens"}"#).unwrap();
        // A role chunk is inserted first, so the terminal frame is the one before `[DONE]`.
        assert_eq!(frames.len(), 3);
        assert_eq!(
            data_json(&frames[1])["choices"][0]["finish_reason"],
            "length"
        );
    }

    #[test]
    fn an_empty_answer_still_gets_a_role_frame_before_the_terminator() {
        reset_all();
        let frames = sse_to_pivot(&ctx(), r#"{"type":"finish","finishReason":"stop"}"#).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(
            data_json(&frames[0])["choices"][0]["delta"]["role"],
            "assistant"
        );
        assert_eq!(frames[2], "data: [DONE]\n\n");
    }

    #[test]
    fn keepalives_unknown_events_and_junk_contribute_nothing() {
        reset_all();
        assert!(sse_to_pivot(&ctx(), "").unwrap().is_empty());
        assert!(sse_to_pivot(&ctx(), "[DONE]").unwrap().is_empty());
        assert!(sse_to_pivot(&ctx(), "not json").unwrap().is_empty());
        assert!(sse_to_pivot(&ctx(), r#"{"type":"reasoning","text":"x"}"#)
            .unwrap()
            .is_empty());
        assert!(sse_to_pivot(&ctx(), r#"{"type":"text-start"}"#)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn an_error_event_is_forwarded_even_though_the_reference_bridge_swallows_it() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"type":"error","error":{"code":"invalid_api_key","status":401,"message":"bad key","docs":"http://d"}}"#,
        )
        .unwrap();
        assert_eq!(frames.len(), 2);
        let v = data_json(&frames[0]);
        assert_eq!(v["error"]["message"], "bad key");
        assert_eq!(v["error"]["type"], "api_error");
        assert_eq!(v["error"]["code"], "invalid_api_key");
        assert_eq!(v["error"]["status"], 401);
        assert_eq!(v["error"]["docs"], "http://d");
        assert_eq!(frames[1], "data: [DONE]\n\n");
    }

    #[test]
    fn the_success_false_envelope_is_recognised_on_a_stream_line_too() {
        reset_all();
        let frames = sse_to_pivot(
            &ctx(),
            r#"{"success":false,"error":{"code":"rate_limited","status":429,"message":"slow down"}}"#,
        )
        .unwrap();
        assert_eq!(data_json(&frames[0])["error"]["code"], "rate_limited");
    }

    #[test]
    fn a_non_openai_error_body_is_rewritten_into_an_openai_error_body() {
        let body = br#"{"success":false,"error":{"code":"invalid_api_key","status":401,"message":"bad key","docs":"http://d"}}"#;
        let out: Value = serde_json::from_slice(&to_pivot_response(&ctx(), body).unwrap()).unwrap();
        assert!(out.get("choices").is_none());
        assert_eq!(out["error"]["message"], "bad key");
        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["code"], "invalid_api_key");
        assert_eq!(out["error"]["status"], 401);
        assert_eq!(out["error"]["docs"], "http://d");
    }

    #[test]
    fn the_error_shape_is_rewritten_for_an_anthropic_client() {
        let body = br#"{"success":false,"error":{"message":"nope"}}"#;
        let out: Value =
            serde_json::from_slice(&to_pivot_response(&anthropic_ctx(), body).unwrap()).unwrap();
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], "nope");
    }

    #[test]
    fn a_buffered_stream_folds_into_one_completion() {
        let body = concat!(
            r#"{"type":"start"}"#,
            "\n",
            r#"{"type":"start-step","request":{"body":{"model":"internal"}}}"#,
            "\n",
            r#"{"type":"text-delta","text":"Hel"}"#,
            "\n",
            r#"{"type":"text-delta","text":"lo"}"#,
            "\n",
            r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":2,"outputTokens":1}}"#,
            "\n",
        );
        let out: Value =
            serde_json::from_slice(&to_pivot_response(&ctx(), body.as_bytes()).unwrap()).unwrap();
        assert_eq!(out["object"], "chat.completion");
        assert_eq!(out["model"], "cmd-model");
        assert_eq!(out["choices"][0]["message"]["role"], "assistant");
        assert_eq!(out["choices"][0]["message"]["content"], "Hello");
        assert_eq!(out["choices"][0]["finish_reason"], "stop");
        assert_eq!(
            out["usage"],
            json!({
                "prompt_tokens": 2,
                "completion_tokens": 1,
                "total_tokens": 3,
                "prompt_tokens_details": {"cached_tokens": 0},
                "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0,
            })
        );
    }

    #[test]
    fn a_folded_completion_with_tool_calls_has_null_content() {
        let body = concat!(
            r#"{"type":"tool-use","toolCallId":"t1","toolName":"f"}"#,
            "\n",
            r#"{"type":"tool-input-delta","toolCallId":"t1","delta":"{\"a\":"}"#,
            "\n",
            r#"{"type":"tool-input-delta","toolCallId":"t1","delta":"1}"}"#,
            "\n",
            r#"{"type":"finish","finishReason":"tool_calls"}"#,
            "\n",
        );
        let out: Value =
            serde_json::from_slice(&to_pivot_response(&ctx(), body.as_bytes()).unwrap()).unwrap();
        let message = &out["choices"][0]["message"];
        assert_eq!(message["content"], Value::Null);
        assert_eq!(message["tool_calls"][0]["id"], "t1");
        assert_eq!(message["tool_calls"][0]["function"]["name"], "f");
        assert_eq!(
            message["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn an_anthropic_client_gets_anthropic_frames_and_its_own_terminator() {
        reset_all();
        let frames =
            sse_to_pivot(&anthropic_ctx(), r#"{"type":"text-delta","text":"hi"}"#).unwrap();
        let joined = frames.join("");
        assert!(joined.contains("event: message_start"), "{joined}");
        assert!(joined.contains("text_delta"), "{joined}");

        let frames = sse_to_pivot(
            &anthropic_ctx(),
            r#"{"type":"finish","finishReason":"stop"}"#,
        )
        .unwrap();
        let joined = frames.join("");
        assert!(joined.contains("message_stop"), "{joined}");
        // A literal `data: [DONE]` is an OpenAI idiom an Anthropic SDK rejects.
        assert!(!joined.contains("[DONE]"), "{joined}");
    }

    #[test]
    fn a_new_stream_on_a_reused_thread_does_not_inherit_the_last_one() {
        reset_all();
        let first = sse_to_pivot(&ctx(), r#"{"type":"finish","finishReason":"stop"}"#).unwrap();
        assert_eq!(
            data_json(&first[0])["choices"][0]["delta"]["role"],
            "assistant"
        );
        // No reset between the two: the state must notice the previous stream ended.
        let second = sse_to_pivot(&ctx(), r#"{"type":"text-delta","text":"next"}"#).unwrap();
        let v = data_json(&second[0]);
        assert_eq!(v["choices"][0]["delta"]["role"], "assistant");
        assert_eq!(v["choices"][0]["delta"]["content"], "next");
    }

    /// The regression, at the entry point the relay uses.
    ///
    /// Two streams are in flight **concurrently on one worker thread** — which is the normal case,
    /// because a response body is polled on whichever runtime worker is free. Each opens its own
    /// tool call and then sends its own argument fragment. Before the per-stream `Translator`, both
    /// accumulated into one `thread_local`, so a fragment whose id was not in the shared map was
    /// appended to "the most recently opened call" — the *other* stream's. The client saw a second
    /// tool call with an empty id and an empty name, which cannot be executed or matched, and the
    /// session log then failed validation with
    /// `format v4 tool/result at seq N requires toolCallId matching its tool source`.
    ///
    /// The assertion is on the frames each stream *sees*, because that is what the client gets: no
    /// stream may ever emit a tool call that is missing its id or its name, and no fragment may be
    /// attributed to a call the stream did not open.
    #[test]
    fn two_concurrent_streams_do_not_contaminate_each_others_tool_calls() {
        // Deliberately NOT reset_all(): the point is that two live streams coexist.
        let mut a = Translator::new();
        let mut b = Translator::new();

        // Interleave the two streams frame by frame, as a shared worker does.
        let a_open = translate(
            &mut a,
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"call_A","toolName":"alpha"}"#,
        )
        .unwrap();
        let b_open = translate(
            &mut b,
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"call_B","toolName":"beta"}"#,
        )
        .unwrap();
        let a_args = translate(
            &mut a,
            &ctx(),
            r#"{"type":"tool-delta","toolCallId":"call_A","delta":"A-ARGS"}"#,
        )
        .unwrap();
        let b_args = translate(
            &mut b,
            &ctx(),
            r#"{"type":"tool-delta","toolCallId":"call_B","delta":"B-ARGS"}"#,
        )
        .unwrap();

        // Every open carries the id and name of the call its own stream opened.
        for (label, frames, id, name) in [
            ("A open", &a_open, "call_A", "alpha"),
            ("B open", &b_open, "call_B", "beta"),
        ] {
            let calls = tool_deltas(frames);
            assert_eq!(calls.len(), 1, "{label} opens exactly one call");
            assert_eq!(
                calls[0].1.as_deref(),
                Some(id),
                "{label} carries its own id"
            );
            assert_eq!(
                calls[0].2.as_deref(),
                Some(name),
                "{label} carries its own name"
            );
        }

        // And each stream's fragments fold into its own accumulated call, unmixed.
        assert_eq!(
            tool_deltas(&a_args)[0].0,
            0,
            "A's fragment lands on A's call index"
        );
        assert_eq!(
            tool_deltas(&b_args)[0].0,
            0,
            "B's fragment lands on B's call"
        );

        // The decisive check: each stream's own accumulation holds only its own fragment.
        let a_done = translate(
            &mut a,
            &ctx(),
            r#"{"type":"finish","finishReason":"tool_calls"}"#,
        )
        .unwrap();
        let b_done = translate(
            &mut b,
            &ctx(),
            r#"{"type":"finish","finishReason":"tool_calls"}"#,
        )
        .unwrap();
        assert!(!a_done.is_empty() && !b_done.is_empty());

        // No frame of either stream may carry an empty id or an empty name anywhere.
        for (label, frames) in [
            ("A", a_open.iter().chain(&a_args).chain(&a_done)),
            ("B", b_open.iter().chain(&b_args).chain(&b_done)),
        ] {
            for frame in frames {
                let parsed = sse::parse_frame(frame);
                if parsed.data == SSE_DONE {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(&parsed.data) else {
                    continue;
                };
                let Some(calls) = v["choices"][0]["delta"].get("tool_calls") else {
                    continue;
                };
                for call in calls.as_array().unwrap() {
                    if let Some(id) = call.get("id") {
                        assert!(
                            !id.as_str().unwrap().is_empty(),
                            "{label}: empty id emitted"
                        );
                    }
                    if let Some(name) = call["function"].get("name") {
                        assert!(
                            !name.as_str().unwrap().is_empty(),
                            "{label}: empty name emitted"
                        );
                    }
                }
            }
        }
    }

    /// PROOF that the legacy per-thread path is what produced the phantom call.
    ///
    /// This drives two logical streams through [`sse_to_pivot`] — the pre-fix entry point — with no
    /// reset between them, exactly as the relay did when two answers shared a worker. Stream A's
    /// fragment is attributed to stream B's call, which is the corruption reduced to one thread.
    #[test]
    fn proof_the_legacy_thread_local_path_misattributes_a_fragment() {
        reset_all();
        sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"A","toolName":"alpha"}"#,
        )
        .unwrap();
        // The relay's per-stream reset fires for B on this worker...
        reset_all();
        sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"B","toolName":"beta"}"#,
        )
        .unwrap();
        sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-delta","toolCallId":"B","delta":"B-ARGS"}"#,
        )
        .unwrap();
        // ...then A's own fragment arrives, whose id this worker no longer holds.
        sse_to_pivot(
            &ctx(),
            r#"{"type":"tool-delta","toolCallId":"A","delta":"A-ARGS"}"#,
        )
        .unwrap();

        COMMAND_CODE_STREAM_STATE.with(|cell| {
            let slot = cell.borrow();
            let state = slot.as_ref().expect("B's parked state");
            let b = state
                .tool_calls
                .iter()
                .find(|c| c.id == "B")
                .expect("B's call");
            // B opened with "B-ARGS"; A's "A-ARGS" was appended to it as well.
            assert_eq!(
                b.arguments, "B-ARGSA-ARGS",
                "documents the legacy leak: A's fragment was folded into B's call"
            );
        });
    }

    /// A stream driven through the owned translator accumulates exactly one call per open, with the
    /// arguments its own fragments supplied — the fold the client actually dispatches.
    #[test]
    fn an_owned_translator_folds_its_own_fragments_into_its_own_call() {
        let mut t = Translator::new();
        translate(
            &mut t,
            &ctx(),
            r#"{"type":"tool-use","toolCallId":"t1","toolName":"search"}"#,
        )
        .unwrap();
        translate(
            &mut t,
            &ctx(),
            r#"{"type":"tool-delta","toolCallId":"t1","delta":"{\"q\":"}"#,
        )
        .unwrap();
        translate(
            &mut t,
            &ctx(),
            r#"{"type":"tool-delta","toolCallId":"t1","delta":"\"x\"}"}"#,
        )
        .unwrap();
        translate(
            &mut t,
            &ctx(),
            r#"{"type":"finish","finishReason":"tool_calls"}"#,
        )
        .unwrap();

        assert!(t.is_finished(), "the finish was seen by this translator");
        let state = t.state.as_ref().expect("state");
        assert_eq!(state.tool_calls.len(), 1);
        assert_eq!(state.tool_calls[0].id, "t1");
        assert_eq!(state.tool_calls[0].name, "search");
        assert_eq!(state.tool_calls[0].arguments, "{\"q\":\"x\"}");
    }

    #[test]
    fn an_anthropic_client_gets_a_messages_body_when_a_stream_is_folded() {
        let body = concat!(
            r#"{"type":"text-delta","text":"hi"}"#,
            "\n",
            r#"{"type":"finish","finishReason":"stop"}"#,
            "\n",
        );
        let out: Value =
            serde_json::from_slice(&to_pivot_response(&anthropic_ctx(), body.as_bytes()).unwrap())
                .unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["content"][0], json!({"type": "text", "text": "hi"}));
        assert_eq!(out["stop_reason"], "end_turn");
    }
}
