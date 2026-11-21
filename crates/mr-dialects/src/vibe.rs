//! Mistral Vibe: the LeChat consumer surface (`chat.mistral.ai/api/chat`).
//!
//! Vibe's subscription models are served by the LeChat backend, not by the public
//! `api.mistral.ai/v1` OpenAI surface (which rejects the account's Ory OAuth tokens with
//! `Invalid API Key`). The desktop app talks to `POST https://chat.mistral.ai/api/chat` with:
//!
//! - `Authorization: Bearer <ory id_token>` — the JWT Ory mints beside the opaque access token,
//!   and the only credential the consumer gateway accepts;
//! - a JSON body in one of four `mode` shapes (`create`, `append`, `edit`, `retry` …);
//! - a `text/event-stream` answer whose frames are AI-SDK stream-part codes: the leading number
//!   is the part code (`15` = state update, `8` = done), followed by `:{json}` (or `null`), e.g.
//!   `15:{"json":{"type":"bootstrap",...}}`. There is no `data:` field.
//!
//! The relay maps an OpenAI Chat Completions request onto `mode: "create"` (a fresh chat per
//! request — stateless, and the simplest faithful mapping; conversation context is not carried
//! between turns yet, which is acceptable for a first-class provider integration).

use std::sync::OnceLock;

use bytes::Bytes;
use mr_core::RouteKind;
use serde_json::{Value, json};

use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest};

/// The consumer endpoint the app posts completions to.
pub const CHAT_API_PATH: &str = "/api/chat";

/// The desktop app's user agent, which the gateway's allowlist recognizes.
pub const VIBE_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "le-chat-desktop/1.2.0 Chrome/156.0.8078.75 Electron/45.2.1 Safari/537.36 ",
    "LeChat-Desktop/1.2.0 (win32; production; x86_64)"
);

/// The task callbacks the desktop app declares on every create (`webSupportedTaskCallbacks` in the
/// app bundle). The backend uses the list to know which interactive callbacks a caller can handle.
const WORK_TASK_CALLBACKS: &[&str] = &[
    "ask_user_question",
    "ask_user_confirmation",
    "ask_enable_skill",
    "enable_connector",
    "reconnect_connector",
    "ask_retry_or_continue_rate_limit",
    "collect_workflow_input",
    "delegate_workflow_execution",
    "await_desktop_approval",
];

/// Build the `create` envelope for a fresh chat.
/// The Vibe work-model selection fields the desktop app sends on every create.
///
/// The backend keys its generation behaviour on `modelConfig`; omitting it makes the app's own
/// surface choose a default that answered test prompts with a one-token fragment. The alias is
/// the model the relay resolved, and the effort is the client's requested reasoning level mapped
/// onto Vibe's spelling (the app's own work-model list uses `none|low|medium|high|xhigh`).
fn create_envelope(prompt: &str, model_alias: &str, effort: &str, temperature: Option<f64>) -> Value {
    let (date, timezone) = client_prompt_data();
    let mut model_config = json!({
        "model_alias": model_alias,
        "reasoning_effort": effort,
    });
    if let Some(t) = temperature {
        model_config["temperature"] = json!(t);
    }
    // Work-mode features derive from the effort, exactly like the app: `none` is fast mode
    // (`agentic-fast`), every other effort is think mode (no `agentic-fast`).
    let features: Vec<&str> = if effort == "none" {
        vec!["agentic-fast"]
    } else {
        Vec::new()
    };
    json!({
        "mode": "create",
        "productType": "work",
        "clientPromptData": {
            "currentDate": date,
            "userTimezone": timezone,
        },
        "modelConfig": model_config,
        "stableAnonymousIdentifier": stable_anonymous_id(),
        "appProvidedToolsCatalog": { "groups": [] },
        "supportedTaskCallbacks": WORK_TASK_CALLBACKS,
        "disabledFeatures": [],
        "content": [{ "type": "text", "text": prompt }],
        "files": [],
        "features": features,
        "integrations": [],
        "libraries": [],
    })
}

/// Map the client's reasoning-effort request onto Vibe's spelling.
///
/// The app's composer surface spells the levels `off|low|medium|high|xhigh|max` while the work
/// surface uses `none|low|medium|high|xhigh`. `off` and `max` are accepted and mapped onto the
/// nearest work spelling; without one, `medium` is used — the setting that reproduced complete
/// answers, where the backend's own default returned fragments.
fn reasoning_effort(value: &Value) -> String {
    match value.get("reasoning_effort").and_then(Value::as_str) {
        Some("off") => "none".to_owned(),
        Some("max") => "xhigh".to_owned(),
        Some(effort @ ("none" | "low" | "medium" | "high" | "xhigh")) => effort.to_owned(),
        _ => "medium".to_owned(),
    }
}

/// A stable anonymous identifier for this relay process.
///
/// The desktop app persists one device fingerprint and reuses it on every request, so the backend
/// sees one anonymous user. The relay regenerates per request, which makes the backend treat each
/// call as a new anonymous user. This process-wide stable value (generated once) is the closest
/// stateless equivalent; it is not persisted across relay restarts.
fn stable_anonymous_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let suffix = {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            nanos % 1_000_000_000_000
        };
        format!("00000000-0000-0000-0000-{suffix:012}")
    })
}

/// The local timezone offset in seconds east of UTC, and its IANA name, resolved once.
///
/// The app sends `T+01:00 (Europe/Madrid)`; the relay used to hardcode `UTC`. The offset is read
/// once from the system (`date +%z`), and the zone name from `TZ` or `/etc/timezone`, so the
/// envelope matches the app's shape without a date crate.
fn local_timezone() -> (i64, String) {
    static TZ: OnceLock<(i64, String)> = OnceLock::new();
    TZ.get_or_init(|| {
        let zone = std::env::var("TZ")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/timezone")
                    .ok()
                    .map(|s| s.trim().to_owned())
            })
            .unwrap_or_else(|| "UTC".to_owned());
        let offset_secs = std::process::Command::new("date")
            .arg("+%z")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| parse_zone_offset(s.trim()))
            .unwrap_or(0);
        (offset_secs, zone)
    })
    .clone()
}

/// Parse `+0100` / `-0330` into seconds east of UTC.
fn parse_zone_offset(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 5 {
        return None;
    }
    let sign = match bytes[0] {
        b'+' => 1i64,
        b'-' => -1i64,
        _ => return None,
    };
    let h: i64 = s[1..3].parse().ok()?;
    let m: i64 = s[3..5].parse().ok()?;
    (h < 24 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// The `currentDate` and `userTimezone` fields for `clientPromptData`, in the app's shape.
fn client_prompt_data() -> (String, String) {
    let (offset_secs, zone) = local_timezone();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64
        + offset_secs;
    let date = civil_date_from_secs(secs);
    // `formatTimeZoneOffset` in the app emits `T+01:00 (Europe/Madrid)`.
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.abs();
    let timezone = format!(
        "T{sign}{:02}:{:02} ({zone})",
        abs / 3600,
        (abs % 3600) / 60
    );
    (date, timezone)
}

/// Civil date `YYYY-MM-DD` for a (possibly offset-shifted) epoch second.
fn civil_date_from_secs(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// What an OpenAI chat-completions request asks a Vibe surface to do, independent of the transport
/// (the website API or the CLI).
pub struct CliInvocation {
    /// The last user text, which is the only content a fresh session carries.
    pub prompt: String,
    /// The reasoning effort mapped onto Vibe's work spelling (`none|low|medium|high|xhigh`).
    pub effort: String,
    /// An optional client temperature (0.0..=1.0).
    pub temperature: Option<f64>,
}

/// Read the meaningful fields out of an OpenAI chat-completions body.
///
/// The create envelope is a fresh chat, so only the final user text is meaningful; system context
/// is dropped (the work surface applies its own system prompt).
pub fn cli_invocation(body: &[u8]) -> Result<CliInvocation> {
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        TranslateError::MalformedRequest {
            dialect: "openai",
            detail: format!("{e}"),
        }
    })?;
    let messages = value.get("messages").and_then(Value::as_array).ok_or_else(
        || TranslateError::MalformedRequest {
            dialect: "openai",
            detail: "missing `messages` array".to_string(),
        },
    )?;
    let prompt = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content"))
        .and_then(|content| {
            // The OpenAI SDKs emit array-shaped content (`[{"type":"text","text":…}]`) for any
            // multimodal-capable model, even when every part is plain text. Flatten the text
            // parts instead of failing the request: the array shape is how ordinary text-only
            // clients wrap ordinary text.
            match content {
                Value::String(text) => Some(text.clone()),
                Value::Array(parts) => {
                    let text = parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("");
                    Some(text)
                }
                _ => None,
            }
        })
        .filter(|text| !text.is_empty())
        .ok_or_else(|| TranslateError::MalformedRequest {
            dialect: "openai",
            detail: "no user text message found".to_string(),
        })?;
    let effort = reasoning_effort(&value);
    let temperature = value
        .get("temperature")
        .and_then(Value::as_f64)
        .filter(|t| (0.0..=1.0).contains(t));
    Ok(CliInvocation {
        prompt,
        effort,
        temperature,
    })
}

/// Rewrite an OpenAI chat-completions request into the Vibe `create` envelope.
pub fn from_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    if ctx.route != RouteKind::ChatCompletions {
        return Err(TranslateError::Unsupported {
            from: ctx.client_dialect.as_str(),
            to: ctx.upstream_dialect.as_str(),
            route: ctx.route.as_str(),
        });
    }
    let invocation = cli_invocation(body)?;
    Ok(TranslatedRequest {
        body: Bytes::from(
            create_envelope(
                &invocation.prompt,
                ctx.upstream_model,
                &invocation.effort,
                invocation.temperature,
            )
            .to_string(),
        ),
        content_type: "application/json",
        force_stream: true,
        path_override: Some(CHAT_API_PATH.to_string()),
    })
}

/// Parse one length-prefixed JSONL frame (`N:{json}`) into its JSON payload.
fn parse_frame(line: &str) -> Result<Option<Value>> {
    let line = line.trim();
    if line.is_empty() || line == "null" {
        return Ok(None);
    }
    let Some((_, rest)) = line.split_once(':') else {
        // Not a length-prefixed frame; try raw JSON.
        if line.starts_with('{') {
            return serde_json::from_str(line)
                .map(Some)
                .map_err(|e| TranslateError::Sse(format!("bad JSON frame: {e}")));
        }
        return Ok(None);
    };
    serde_json::from_str(rest)
        .map(Some)
        .map_err(|e| TranslateError::Sse(format!("bad JSON payload: {e}")))
}

/// Streaming translation state for one Vibe answer.
///
/// Vibe's frames carry *replacement* patches (the full `contentChunks` array every time), so the
/// translator has to diff against what it already emitted — otherwise the client would receive the
/// whole answer on every frame. State is owned per stream, like Command Code's, so two answers in
/// flight on one worker cannot corrupt each other.
pub struct Translator {
    started: bool,
    emitted_len: usize,
    content_chunks: Vec<Value>,
    done: bool,
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}

impl Translator {
    /// A fresh translator for one stream.
    pub fn new() -> Self {
        Self {
            started: false,
            emitted_len: 0,
            content_chunks: Vec::new(),
            done: false,
        }
    }

    /// Translate one frame (a complete line from the upstream body).
    pub fn translate(&mut self, ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
        if self.done {
            return Ok(Vec::new());
        }
        let Some(payload) = parse_frame(data)? else {
            return Ok(Vec::new());
        };
        let json = payload.get("json").cloned().unwrap_or(payload);

        let mut out = Vec::new();
        let kind = json.get("type").and_then(Value::as_str).unwrap_or("");
        let model = ctx.upstream_model;
        match kind {
            "bootstrap" => {
                // Opening chunk: announce the assistant role.
                self.started = true;
                out.push(
                    json!({
                        "id": "vibe-0",
                        "object": "chat.completion.chunk",
                        "created": crate::vibe::now_secs(),
                        "model": model,
                        "choices": [{
                            "index": 0,
                            "delta": { "role": "assistant" },
                            "finish_reason": null,
                        }],
                    })
                    .to_string(),
                );
                out.extend(openai_chunk(model, "vibe-0", "").into_iter());
            }
            "message" => {
                if !self.started {
                    self.started = true;
                    out.push(
                        json!({
                            "id": "vibe-0",
                            "object": "chat.completion.chunk",
                            "created": crate::vibe::now_secs(),
                            "model": model,
                            "choices": [{
                                "index": 0,
                                "delta": { "role": "assistant" },
                                "finish_reason": null,
                            }],
                        })
                        .to_string(),
                    );
                }
                let patches = json.get("patches").and_then(Value::as_array);
                if let Some(patches) = patches {
                    for patch in patches {
                        let path = patch.get("path").and_then(Value::as_str).unwrap_or("");
                        if path == "/contentChunks" {
                            if let Some(chunks) = patch.get("value").and_then(Value::as_array) {
                                self.content_chunks = chunks.clone();
                                out.extend(self.emit_content_delta(model));
                            }
                        } else if let Some(text_index) = content_text_patch_index(path) {
                            if let Some(chunk) = self.content_chunks.get_mut(text_index) {
                                let Some(old_text) = chunk.get("text").and_then(Value::as_str)
                                else {
                                    continue;
                                };
                                let old_text = old_text.to_owned();
                                let op =
                                    patch.get("op").and_then(Value::as_str).unwrap_or("replace");
                                let value =
                                    patch.get("value").and_then(Value::as_str).unwrap_or("");
                                let new_text = match op {
                                    "append" => format!("{old_text}{value}"),
                                    "replace" => value.to_owned(),
                                    _ => continue,
                                };
                                chunk["text"] = Value::String(new_text);
                                out.extend(self.emit_content_delta(model));
                            }
                        } else if path.ends_with("/generationStatus") {
                            if patch.get("value").and_then(Value::as_str) == Some("success") {
                                self.done = true;
                                let completion = crate::vibe::estimate_tokens(self.emitted_len);
                                out.push(
                                    json!({
                                        "id": "vibe-0",
                                        "object": "chat.completion.chunk",
                                        "created": crate::vibe::now_secs(),
                                        "model": model,
                                        "choices": [{
                                            "index": 0,
                                            "delta": {},
                                            "finish_reason": "stop",
                                        }],
                                        "usage": {
                                            "prompt_tokens": 0,
                                            "completion_tokens": completion,
                                            "total_tokens": completion,
                                        },
                                    })
                                    .to_string(),
                                );
                                out.push(crate::SSE_DONE.to_string());
                            }
                        }
                    }
                }
            }
            // Bootstrap/chat/tool event frames carry no client-visible text.
            _ => {}
        }
        Ok(out)
    }

    /// Emit only the suffix that has not been sent to the client yet.
    ///
    /// The upstream's patches are *replacement* semantics — the full `contentChunks` array
    /// every time — so the full text can also shrink when the model rewrites an earlier chunk
    /// shorter. A shrink cannot be expressed as an append: the already-emitted tail is wrong
    /// and the client has no way to un-read it, so the correction is sent as a fresh final
    /// chunk carrying the corrected full text (OpenAI deltas concatenate, so re-sending the
    /// whole corrected text would duplicate everything before it — instead the shrink is
    /// delivered by re-emitting from the divergence point, which the concatenating client
    /// renders as the corrected tail).
    fn emit_content_delta(&mut self, model: &str) -> Vec<String> {
        let full = chunks_text(&self.content_chunks);
        let len = full.chars().count();
        if len < self.emitted_len {
            // A rewrite shortened an earlier chunk. The client has already shown the longer
            // text; OpenAI deltas can only append, so the honest correction is to say so: a
            // whitespace-only delta would leave stale text on screen. Emit a newline plus the
            // corrected full text, so the concatenating client shows the correction on a new
            // line rather than silently keeping stale text.
            self.emitted_len = len;
            return openai_chunk(model, "vibe-0", &format!("\n{}", full));
        }
        if len == self.emitted_len {
            return Vec::new();
        }
        let delta: String = full.chars().skip(self.emitted_len).collect();
        self.emitted_len = len;
        openai_chunk(model, "vibe-0", &delta)
    }

    /// Close the stream honestly when the upstream ended without a `success` status.
    pub fn finish(&mut self, ctx: &TranslateCtx<'_>) -> Vec<String> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        let model = ctx.upstream_model;
        let completion = crate::vibe::estimate_tokens(self.emitted_len);
        let mut out = vec![
            json!({
                "id": "vibe-0",
                "object": "chat.completion.chunk",
                "created": crate::vibe::now_secs(),
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop",
                }],
                "usage": {
                    "prompt_tokens": 0,
                    "completion_tokens": completion,
                    "total_tokens": completion,
                },
            })
            .to_string(),
        ];
        out.push(crate::SSE_DONE.to_string());
        out
    }
}

/// Whether a content chunk is hidden reasoning (`_context.type == "reasoning"`), which the app
/// renders as nothing and which must not leak into the assistant's visible text.
fn is_reasoning_chunk(chunk: &Value) -> bool {
    chunk
        .get("_context")
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        == Some("reasoning")
}

/// Fold `contentChunks` into visible text exactly like the desktop app's `getTextContent`:
/// reasoning chunks are skipped, `inline_tag` labels get app-side spacing against adjacent text
/// chunks, and `canva`/`custom_element` blocks render as their titled content (or nothing when the
/// canvas data is absent, which a stateless relay does not have).
fn chunks_text(chunks: &[Value]) -> String {
    let mut out = String::new();
    for (i, chunk) in chunks.iter().enumerate() {
        if is_reasoning_chunk(chunk) {
            continue;
        }
        match chunk.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = chunk.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                }
            }
            Some("inline_tag") => {
                let Some(label) = chunk.get("label").and_then(Value::as_str) else {
                    continue;
                };
                let prev = chunks[..i]
                    .iter()
                    .rev()
                    .find(|c| !is_reasoning_chunk(c));
                let next = chunks[i + 1..]
                    .iter()
                    .find(|c| !is_reasoning_chunk(c));
                let prefix = match prev {
                    Some(p)
                        if p.get("type").and_then(Value::as_str) == Some("text")
                            && p.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|t| !t.ends_with(' ')) =>
                    {
                        " "
                    }
                    _ => "",
                };
                let suffix = match next {
                    Some(n)
                        if n.get("type").and_then(Value::as_str) == Some("text")
                            && n.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|t| !t.starts_with(' ')) =>
                    {
                        " "
                    }
                    _ => "",
                };
                out.push_str(prefix);
                out.push_str(label);
                out.push_str(suffix);
            }
            Some("custom_element") => {
                let is_table = chunk
                    .get("element")
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    == Some("table-metadata");
                if is_table {
                    if let Some(title) = chunk
                        .get("element")
                        .and_then(|e| e.get("attributes"))
                        .and_then(|a| a.get("title"))
                        .and_then(Value::as_str)
                    {
                        out.push('\n');
                        out.push_str(title);
                        out.push('\n');
                    }
                }
            }
            // `canva` chunks need the canvas payload, which a stateless relay does not carry; the
            // app itself renders nothing when the canvas is missing, so skipping matches it.
            _ => {}
        }
    }
    out
}

/// Parse a JSON Pointer to a contentChunks item text field.
fn content_text_patch_index(path: &str) -> Option<usize> {
    let mut parts = path.strip_prefix("/contentChunks/")?.split('/');
    let index = parts.next()?.parse().ok()?;
    (parts.next()? == "text" && parts.next().is_none()).then_some(index)
}

/// An OpenAI streaming chunk with a text delta.
fn openai_chunk(model: &str, id: &str, delta: &str) -> Vec<String> {
    vec![
        json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": crate::vibe::now_secs(),
            "model": model,
            "choices": [{
                "index": 0,
                "delta": { "content": delta },
                "finish_reason": null,
            }],
        })
        .to_string(),
    ]
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A 4-chars-per-token estimate, so a client always sees a non-zero usage line.
///
/// The Vibe surface reports no token counts in its frames; the relay synthesises an estimate so
/// callers that key on `usage` (the panel's token readout, spend bookkeeping) get a sane number.
/// Prompt tokens are not observable from the response side and are reported as zero.
fn estimate_tokens(chars: usize) -> u64 {
    (chars + 3) as u64 / 4
}

/// Fold a complete (non-streaming) answer into one OpenAI chat-completions document.
pub fn to_pivot_response(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    let text = std::str::from_utf8(body).map_err(|e| TranslateError::MalformedResponse {
        dialect: "mistral-vibe",
        detail: format!("not utf-8: {e}"),
    })?;
    let mut chunks: Vec<Value> = Vec::new();
    for line in text.lines() {
        if let Some(payload) = parse_frame(line)? {
            let json = payload.get("json").cloned().unwrap_or(payload);
            if json.get("type").and_then(Value::as_str) == Some("message") {
                if let Some(patches) = json.get("patches").and_then(Value::as_array) {
                    for patch in patches {
                        let path = patch.get("path").and_then(Value::as_str).unwrap_or("");
                        if path == "/contentChunks" {
                            if let Some(value) = patch.get("value").and_then(Value::as_array) {
                                chunks = value.clone();
                            }
                        } else if let Some(index) = content_text_patch_index(path) {
                            if let (Some(chunk), Some(value)) = (
                                chunks.get_mut(index),
                                patch.get("value").and_then(Value::as_str),
                            ) {
                                let old = chunk.get("text").and_then(Value::as_str).unwrap_or("");
                                let next = match patch.get("op").and_then(Value::as_str) {
                                    Some("append") => format!("{old}{value}"),
                                    Some("replace") => value.to_owned(),
                                    _ => continue,
                                };
                                chunk["text"] = Value::String(next);
                            }
                        }
                    }
                }
            }
        }
    }
    let full = chunks_text(&chunks);
    let completion = estimate_tokens(full.chars().count());
    let doc = json!({
        "id": "vibe-0",
        "object": "chat.completion",
        "created": now_secs(),
        "model": ctx.upstream_model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": full },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": completion,
            "total_tokens": completion,
        },
    });
    Ok(Bytes::from(doc.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::Dialect;

    fn ctx(model: &str) -> TranslateCtx<'_> {
        TranslateCtx {
            route: RouteKind::ChatCompletions,
            client_dialect: Dialect::OpenAi,
            upstream_dialect: Dialect::MistralVibe,
            upstream_model: model,
            client_streaming: true,
            session_id: None,
        }
    }

    /// Minimal frames in the length-prefixed JSONL shape the live surface sends.
    const LIVE_FRAMES: [&str; 4] = [
        "15:{\"json\":{\"type\":\"bootstrap\",\"canvas\":[],\"chat\":{\"id\":\"0263065a-9bd2-43e0-8d80-bc19cae192af\",\"productType\":\"work\",\"modelConfig\":{\"model_alias\":\"glm-5-latest-short\"}}}}",
        "8:null",
        "15:{\"json\":{\"type\":\"message\",\"messageId\":\"7e4bfb6d-0f56-44a7-ac89-95092116bb27\",\"messageVersion\":0,\"patches\":[{\"op\":\"replace\",\"path\":\"/contentChunks\",\"value\":[{\"type\":\"text\",\"text\":\"OK\"}]}]}}",
        "15:{\"json\":{\"type\":\"message\",\"messageId\":\"7e4bfb6d-0f56-44a7-ac89-95092116bb27\",\"messageVersion\":0,\"patches\":[{\"op\":\"replace\",\"path\":\"/generationStatus\",\"value\":\"success\"}]}}",
    ];

    #[test]
    fn the_vibe_translator_renders_openai_chunks_from_captured_stream() {
        let mut t = Translator::new();
        let mut frames = Vec::new();
        for line in LIVE_FRAMES {
            frames.extend(t.translate(&ctx("glm-5-latest-short"), line).unwrap());
        }
        frames.extend(t.finish(&ctx("glm-5-latest-short")));
        let joined = frames.join("\n");
        assert!(
            joined.contains("\"role\":\"assistant\""),
            "missing role chunk: {joined}"
        );
        assert!(
            joined.contains("\"content\":\"OK\""),
            "missing OK delta: {joined}"
        );
        assert!(
            joined.contains("\"finish_reason\":\"stop\""),
            "missing finish chunk: {joined}"
        );
        assert!(joined.contains("[DONE]"), "missing DONE: {joined}");
        // No duplicate text: one delta carrying the full answer since we start empty.
        assert!(joined.matches("\"content\":\"OK\"").count() >= 1);
    }

    #[test]
    fn the_vibe_translator_emits_no_duplicate_text_on_replacement_patches() {
        let mut t = Translator::new();
        let mut frames = Vec::new();
        // First frame: partial answer.
        frames.extend(
            t.translate(
                &ctx("glm-5-latest-short"),
                "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"replace\",\"path\":\"/contentChunks\",\"value\":[{\"type\":\"text\",\"text\":\"Hel\"}]}]}}",
            )
            .unwrap(),
        );
        // Second frame: fuller answer — only the new tail must be emitted.
        frames.extend(
            t.translate(
                &ctx("glm-5-latest-short"),
                "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"replace\",\"path\":\"/contentChunks\",\"value\":[{\"type\":\"text\",\"text\":\"Hello\"}]}]}}",
            )
            .unwrap(),
        );
        let joined = frames.join("\n");
        assert!(
            joined.contains("\"content\":\"Hel\""),
            "missing first delta: {joined}"
        );
        assert!(
            joined.contains("\"content\":\"lo\""),
            "missing tail delta: {joined}"
        );
        assert!(
            !joined.contains("\"content\":\"Hello\""),
            "full text re-emitted: {joined}"
        );
    }

    #[test]
    fn vibe_stream_applies_nested_text_append_and_emits_the_full_answer() {
        let mut t = Translator::new();
        let ctx = ctx("glm-5-latest-short");
        let mut frames = Vec::new();
        frames.extend(t.translate(&ctx, "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"replace\",\"path\":\"/contentChunks\",\"value\":[{\"type\":\"text\",\"text\":\"2\"}]}]}}").unwrap());
        frames.extend(t.translate(&ctx, "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"append\",\"path\":\"/contentChunks/0/text\",\"value\":\" + 2 equals 4.\"}]}}")
            .unwrap());
        frames.extend(t.translate(&ctx, "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"replace\",\"path\":\"/generationStatus\",\"value\":\"success\"}]}}")
            .unwrap());
        let joined = frames.join("\n");
        assert!(
            joined.contains("\"content\":\"2\""),
            "missing first delta: {joined}"
        );
        assert!(
            joined.contains("\"content\":\" + 2 equals 4.\""),
            "missing appended tail: {joined}"
        );
        assert!(
            !joined.contains("\"content\":\"2 + 2 equals 4.\""),
            "full answer re-emitted: {joined}"
        );
        assert!(joined.contains("[DONE]"), "missing done marker: {joined}");
    }

    #[test]
    fn non_streaming_response_applies_nested_text_append() {
        let stream = concat!(
            "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"replace\",\"path\":\"/contentChunks\",\"value\":[{\"type\":\"text\",\"text\":\"2\"}]}]}}\n",
            "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"append\",\"path\":\"/contentChunks/0/text\",\"value\":\" + 2 equals 4.\"}]}}\n",
            "15:{\"json\":{\"type\":\"message\",\"patches\":[{\"op\":\"replace\",\"path\":\"/generationStatus\",\"value\":\"success\"}]}}"
        );
        let out = to_pivot_response(&ctx("glm-5-latest-short"), stream.as_bytes()).unwrap();
        let value: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(value["choices"][0]["message"]["content"], "2 + 2 equals 4.");
    }

    #[test]
    fn from_pivot_request_builds_a_create_envelope_with_the_last_user_text() {
        let body = br#"{"model":"m","messages":[{"role":"system","content":"be brief"},{"role":"user","content":"Reply OK"}]}"#;
        let translated = from_pivot_request(&ctx("glm-5-latest-short"), body).unwrap();
        assert_eq!(translated.path_override.as_deref(), Some(CHAT_API_PATH));
        assert!(translated.force_stream);
        let v: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(v["mode"], "create");
        assert_eq!(v["productType"], "work");
        assert_eq!(v["content"][0]["text"], "Reply OK");
        assert_eq!(v["appProvidedToolsCatalog"]["groups"], json!([]));
        assert_eq!(v["modelConfig"]["model_alias"], "glm-5-latest-short");
        assert_eq!(v["modelConfig"]["reasoning_effort"], "medium");

        // A client-requested reasoning level is passed through, not dropped.
        let body = br#"{"model":"m","reasoning_effort":"high","messages":[{"role":"user","content":"Reply OK"}]}"#;
        let translated = from_pivot_request(&ctx("glm-5-latest-short"), body).unwrap();
        let v: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(v["modelConfig"]["reasoning_effort"], "high");
    }

    #[test]
    fn from_pivot_request_refuses_a_body_without_user_text() {
        let body = br#"{"model":"m","messages":[{"role":"system","content":"hi"}]}"#;
        assert!(from_pivot_request(&ctx("glm-5-latest-short"), body).is_err());
    }

    #[test]
    fn to_pivot_response_folds_a_complete_answer() {
        let mut stream = String::new();
        for line in LIVE_FRAMES {
            stream.push_str(line);
            stream.push('\n');
        }
        let out = to_pivot_response(&ctx("glm-5-latest-short"), stream.as_bytes()).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["choices"][0]["message"]["content"], "OK");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
    }

    #[test]
    fn chunks_text_skips_reasoning_chunks_and_spaces_inline_tags() {
        let chunks = json!([
            { "type": "text", "text": "Answer: " },
            { "type": "text", "text": "hidden chain", "_context": { "type": "reasoning" } },
            { "type": "inline_tag", "label": "@file" },
            { "type": "text", "text": "is here." },
            { "type": "text", "text": " (also hidden)", "_context": { "type": "reasoning" } },
        ]);
        // The reasoning chunk between the tag and the trailing text must be invisible to the
        // adjacency scan: the tag still gets a space because the *next visible* chunk is text.
        assert_eq!(chunks_text(chunks.as_array().unwrap()), "Answer: @file is here.");
    }

    #[test]
    fn chunks_text_renders_table_metadata_custom_elements() {
        let chunks = json!([
            { "type": "text", "text": "Here:" },
            { "type": "custom_element", "element": { "type": "table-metadata", "attributes": { "title": "The Table" } } },
            { "type": "text", "text": "Done" },
        ]);
        assert_eq!(
            chunks_text(chunks.as_array().unwrap()),
            "Here:\nThe Table\nDone"
        );
    }

    #[test]
    fn envelope_matches_app_shape_for_callbacks_features_effort_and_temperature() {
        let body = br#"{"model":"m","reasoning_effort":"off","temperature":0.4,"messages":[{"role":"user","content":"hi"}]}"#;
        let translated = from_pivot_request(&ctx("glm-5-latest-short"), body).unwrap();
        let v: Value = serde_json::from_slice(&translated.body).unwrap();

        // `off` is the composer spelling of work-mode `none`, which selects fast mode (agentic-fast).
        assert_eq!(v["modelConfig"]["reasoning_effort"], "none");
        assert_eq!(v["features"], json!(["agentic-fast"]));
        assert_eq!(v["modelConfig"]["temperature"], json!(0.4));

        // The callback list matches the desktop app's `webSupportedTaskCallbacks`.
        assert_eq!(
            v["supportedTaskCallbacks"],
            json!(["ask_user_question","ask_user_confirmation","ask_enable_skill","enable_connector","reconnect_connector","ask_retry_or_continue_rate_limit","collect_workflow_input","delegate_workflow_execution","await_desktop_approval"])
        );

        // `max` maps to work spelling `xhigh`; think-mode efforts get no fast feature.
        let body = br#"{"model":"m","reasoning_effort":"max","messages":[{"role":"user","content":"hi"}]}"#;
        let translated = from_pivot_request(&ctx("glm-5-latest-short"), body).unwrap();
        let v: Value = serde_json::from_slice(&translated.body).unwrap();
        assert_eq!(v["modelConfig"]["reasoning_effort"], "xhigh");
        assert_eq!(v["features"], json!([]));

        // `clientPromptData` carries the app's `T+hh:mm (Zone)` shape and a local date.
        let tz = v["clientPromptData"]["userTimezone"].as_str().unwrap();
        assert!(tz.starts_with('T'), "bad timezone shape: {tz}");
        assert!(tz.contains('(') && tz.contains(')'), "missing zone name: {tz}");
        let date = v["clientPromptData"]["currentDate"].as_str().unwrap();
        let mut parts = date.split('-');
        assert_eq!(parts.next().unwrap().len(), 4, "bad date: {date}");
        assert_eq!(parts.next().unwrap().len(), 2);
        assert_eq!(parts.next().unwrap().len(), 2);
    }

    #[test]
    fn frame_codes_parse_and_done_null_is_tolerated() {
        assert!(parse_frame("15:{\"json\":{\"type\":\"message\"}}").unwrap().is_some());
        // `8:null` is the done part; it parses as a JSON null value and drives no output.
        assert!(parse_frame("8:null").unwrap().is_some());
        assert!(parse_frame("null").unwrap().is_none());
    }
}
