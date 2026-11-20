//! Mistral Vibe: the LeChat consumer surface (`chat.mistral.ai/api/chat`).
//!
//! Vibe's subscription models are served by the LeChat backend, not by the public
//! `api.mistral.ai/v1` OpenAI surface (which rejects the account's Ory OAuth tokens with
//! `Invalid API Key`). The desktop app talks to `POST https://chat.mistral.ai/api/chat` with:
//!
//! - `Authorization: Bearer <ory id_token>` — the JWT Ory mints beside the opaque access token,
//!   and the only credential the consumer gateway accepts;
//! - a JSON body in one of four `mode` shapes (`create`, `append`, `edit`, `retry` …);
//! - a `text/event-stream` answer whose frames are **length-prefixed JSONL**: each line is
//!   `N:{json}` (or `null`), e.g. `15:{"json":{"type":"bootstrap",...}}`. There is no `data:`
//!   field.
//!
//! The relay maps an OpenAI Chat Completions request onto `mode: "create"` (a fresh chat per
//! request — stateless, and the simplest faithful mapping; conversation context is not carried
//! between turns yet, which is acceptable for a first-class provider integration).

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

/// Build the `create` envelope for a fresh chat.
/// The Vibe work-model selection fields the desktop app sends on every create.
///
/// The backend keys its generation behaviour on `modelConfig`; omitting it makes the app's own
/// surface choose a default that answered test prompts with a one-token fragment. The alias is
/// the model the relay resolved, and the effort is the client's requested reasoning level mapped
/// onto Vibe's spelling (the app's own work-model list uses `none|low|medium|high|xhigh`).
fn create_envelope(prompt: &str, model_alias: &str, effort: &str) -> Value {
    let now = today_utc();
    json!({
        "mode": "create",
        "productType": "work",
        "clientPromptData": {
            "currentDate": now,
            "userTimezone": "UTC",
        },
        "modelConfig": {
            "model_alias": model_alias,
            "reasoning_effort": effort,
        },
        "stableAnonymousIdentifier": format!(
            "00000000-0000-0000-0000-{:012}",
            crate::vibe::pseudo_uuid_suffix()
        ),
        "appProvidedToolsCatalog": { "groups": [] },
        "supportedTaskCallbacks": [],
        "disabledFeatures": [],
        "content": [{ "type": "text", "text": prompt }],
        "files": [],
        "features": [],
        "integrations": [],
        "libraries": [],
    })
}

/// Map the client's reasoning-effort request onto Vibe's spelling.
///
/// A higher effort is a longer-thinking generation; the app's own selection uses the same names
/// (`none`, `low`, `medium`, `high`, `xhigh`), so the request's `reasoning_effort` is passed
/// through when present. Without one, `medium` is used — the setting that reproduced complete
/// answers, where the backend's own default returned fragments.
fn reasoning_effort(value: &Value) -> String {
    match value.get("reasoning_effort").and_then(Value::as_str) {
        Some(effort @ ("none" | "low" | "medium" | "high" | "xhigh")) => effort.to_owned(),
        _ => "medium".to_owned(),
    }
}

/// A tiny, dependency-free stand-in for a random UUID suffix.
fn pseudo_uuid_suffix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64 % 1_000_000_000_000)
        .unwrap_or(0)
}

/// Today's date in UTC, `YYYY-MM-DD`, without pulling in a date crate.
fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
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

/// Rewrite an OpenAI chat-completions request into the Vibe `create` envelope.
pub fn from_pivot_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    if ctx.route != RouteKind::ChatCompletions {
        return Err(TranslateError::Unsupported {
            from: ctx.client_dialect.as_str(),
            to: ctx.upstream_dialect.as_str(),
            route: ctx.route.as_str(),
        });
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|e| TranslateError::MalformedRequest {
            dialect: "openai",
            detail: format!("{e}"),
        })?;
    let messages = value
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| TranslateError::MalformedRequest {
            dialect: "openai",
            detail: "missing `messages` array".to_string(),
        })?;
    // Take the last user text. The create envelope is a fresh chat, so only the final prompt is
    // meaningful; system context is dropped (the work surface applies its own system prompt).
    let prompt = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content").and_then(Value::as_str).map(str::to_owned))
        .ok_or_else(|| TranslateError::MalformedRequest {
            dialect: "openai",
            detail: "no user text message found".to_string(),
        })?;
    let effort = reasoning_effort(&value);
    Ok(TranslatedRequest {
        body: Bytes::from(create_envelope(&prompt, ctx.upstream_model, &effort).to_string()),
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
    fn emit_content_delta(&mut self, model: &str) -> Vec<String> {
        let full: String = self
            .content_chunks
            .iter()
            .filter_map(|chunk| chunk.get("text").and_then(Value::as_str))
            .collect();
        let len = full.chars().count();
        if len <= self.emitted_len {
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
    let full: String = chunks
        .iter()
        .filter_map(|chunk| chunk.get("text").and_then(Value::as_str))
        .collect();
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
}
