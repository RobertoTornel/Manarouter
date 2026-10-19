//! A streaming usage tap: read token counts and the generation window out of an upstream response
//! body **as it streams past**, without buffering it and without changing a byte the client sees.
//!
//! ## Why this exists next to `mr_core::usage`
//!
//! [`mr_core::usage`] already knows how to read usage from a **buffered** body
//! ([`TokenUsage::from_body`]) and it owns the one table of provider spellings, so this module does
//! not re-invent any of that: every count it records comes from
//! [`TokenUsage::from_response_value`], which is the same parser the buffered path uses. What the
//! buffered path cannot do is answer a question about *time*: an upstream stream reports usage in
//! pieces (Anthropic splits input across `message_start` and output across `message_delta`; Gemini
//! re-sends a cumulative `usageMetadata` on every frame), and the moment a token was written is not
//! in the body at all — it is in when each frame arrived.
//!
//! So the split is: `mr_core::usage` turns frames into counts, and this module folds those counts
//! together across a stream and measures the interval between the first and the last frame that
//! actually carried output.
//!
//! ## The rule that matters: unknown is not zero
//!
//! Two fields are easy to get wrong and both are decided here, once:
//!
//! * [`GenerationUsage::usage`] keeps every count as an `Option`, exactly as [`TokenUsage`] does. A
//!   stream that ended before its usage frame arrived records *nothing*, not "0 in, 0 out" — the
//!   latter would report a real request as free.
//! * [`GenerationUsage::generation_ms`] is `None` when no output token was ever observed, **not**
//!   zero. `None` means "we cannot honestly compute tokens per second"; `Some(0)` would mean "the
//!   model wrote its whole answer instantaneously", which a router has no way to know. Because the
//!   two are different facts, [`mr_core::usage::tokens_per_second`] can decline a rate for a `None`
//!   window and for a zero-length one without either case pretending to be the other.
//!
//! ## What counts as an output frame
//!
//! The tap is deliberately **dialect-agnostic**: it sees the upstream's frames, and the upstream's
//! dialect is not always the one the client asked for (a Copilot or Command Code upstream may answer
//! in a shape it never advertised). [`carries_output`] therefore recognises the output-bearing part
//! of every stream this router speaks:
//!
//! | Upstream | An output frame |
//! |---|---|
//! | OpenAI Chat | `choices[].delta.content` / `reasoning_content` / `tool_calls[].function.arguments` |
//! | OpenAI Responses | `response.output_text.delta`, `response.reasoning_*.delta`, `response.function_call_arguments.delta` |
//! | Anthropic | `content_block_delta` with a non-empty `text` / `thinking` / `partial_json` |
//! | Google / Antigravity | `candidates[].content.parts[].text`, or a `functionCall` part |
//! | Command Code | `text-delta`, `tool-delta` / `tool-input-delta`, or an atomic `tool-call` |
//!
//! A frame that only carries `usage`, a role, a `message_start`, or a `[DONE]` sentinel is **not**
//! an output frame: counting one would start a generation window before the model wrote anything.
//! The `response.completed` event is likewise not counted even though it echoes the whole answer, so
//! the window ends at the last streamed token rather than at the bookkeeping event after it.
//!
//! ## How the relay is meant to use it
//!
//! The tap is pure — no I/O, no lock, no allocation per frame beyond what `serde_json` needs — so it
//! can be driven from whichever body path is in play:
//!
//! * the **translating** path already walks the upstream stream line by line; it calls
//!   [`UsageTap::observe_data`] with each frame's payload;
//! * the **pass-through** path forwards raw bytes and never parses them, so it calls
//!   [`UsageTap::observe_chunk`], which frames the same bytes for observation only. Forwarding is
//!   untouched: the client still receives the original bytes.
//!
//! The resulting [`GenerationUsage`] is what a usage row is written from, off the request path (the
//! fire-and-forget accounting in `docs/16-latency-engineering.md` §3.9). This module stops at the
//! measurement; it never writes to the store.

use std::time::Instant;

use mr_core::usage::{tokens_per_second, TokenUsage};
use mr_dialects::sse::{Frame, Framer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What one streamed response turned out to have used, and how long it took to write.
///
/// Both fields preserve the "unknown is not zero" distinction described in the module docs, which
/// is why `generation_ms` is an `Option` rather than a `u64` that happens to be `0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationUsage {
    /// The counts the upstream reported, folded across every frame that carried any. Empty when the
    /// stream never reported usage — never a row of zeroes.
    pub usage: TokenUsage,
    /// First-to-last OUTPUT token window, in milliseconds. `None` when the stream produced no output
    /// tokens we could see (so tokens per second is honestly unknown), **not** zero.
    ///
    /// A stream whose whole answer arrived in a single frame yields `Some(0)`: output *was*
    /// observed, so the window is known to be no longer than the gap between two frames — and
    /// [`mr_core::usage::tokens_per_second`] still declines to turn that into a rate, because a
    /// zero-length window cannot support one.
    pub generation_ms: Option<u64>,
}

impl GenerationUsage {
    /// Output tokens per second over the generation window, or `None` when it cannot be known.
    ///
    /// Delegates to [`mr_core::usage::tokens_per_second`] rather than dividing here, so the
    /// "unreported", "zero tokens" and "zero duration" refusals are the same ones the rest of the
    /// codebase applies.
    pub fn tokens_per_second(&self) -> Option<f64> {
        tokens_per_second(self.usage.output_tokens, self.generation_ms)
    }

    /// Whether there is anything worth recording.
    ///
    /// A stream that reported no counts and produced no observable output is an empty measurement;
    /// writing it would inflate the request count of a table whose other columns say nothing.
    pub fn is_empty(&self) -> bool {
        self.usage.is_empty() && self.generation_ms.is_none()
    }
}

/// Accumulates usage and the generation window from a stream's frames.
///
/// One tap observes one stream. Feed it every frame in arrival order — via [`Self::observe_frame`]
/// when the caller has already framed the stream, or [`Self::observe_chunk`] when it has not — and
/// call [`Self::finish`] once the upstream body ends.
#[derive(Debug, Default)]
pub struct UsageTap {
    /// The most complete usage report seen so far, folded field by field.
    usage: TokenUsage,
    /// When the first output-bearing frame arrived. `None` until one does.
    first_output_at: Option<Instant>,
    /// When the most recent output-bearing frame arrived.
    last_output_at: Option<Instant>,
    /// How many frames carried output, for diagnostics and tests.
    output_frames: u64,
    /// A framer for the raw-bytes entry point. Unused on the pre-framed path.
    framer: Framer,
    /// Whether an explicit end-of-stream signal was seen. Advisory.
    saw_done: bool,
}

impl UsageTap {
    /// A tap that has observed nothing yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe one already-parsed SSE frame.
    ///
    /// Takes the frame by reference because the caller almost always still needs it: on the
    /// translating path the frame's bytes are forwarded to the client after they have been
    /// observed, and moving it would force a copy of every frame the relay did not need to touch.
    pub fn observe_frame(&mut self, frame: &Frame) {
        let at = Instant::now();
        self.observe_frame_at(frame, at);
    }

    /// Observe one frame at a caller-supplied instant.
    ///
    /// This is the entry point a test uses to pin the generation window without sleeping; the
    /// production path uses [`Self::observe_frame`].
    pub fn observe_frame_at(&mut self, frame: &Frame, at: Instant) {
        self.ingest_text(&frame.data, at);
    }

    /// Observe one frame payload that the caller has already extracted.
    ///
    /// The relay's translating path has the `data:` payload in hand and no [`Frame`]; this is its
    /// entry point. The payload may be an SSE payload, a bare newline-delimited JSON line (Command
    /// Code's live endpoint), or a whole JSON document.
    pub fn observe_data(&mut self, data: &str) {
        let at = Instant::now();
        self.observe_data_at(data, at);
    }

    /// Observe one extracted payload at a caller-supplied instant.
    pub fn observe_data_at(&mut self, data: &str, at: Instant) {
        self.ingest_text(data, at);
    }

    /// Observe raw bytes off the wire, framing them for observation only.
    ///
    /// This is the pass-through path's entry point: it is handed exactly the bytes that are also
    /// being forwarded to the client, so observing costs one parse and changes nothing on the wire.
    /// A chunk may split a frame anywhere, so the framer retains the partial tail for the next call
    /// and [`Self::finish`] drains whatever is left.
    pub fn observe_chunk(&mut self, bytes: &[u8]) {
        self.framer.push(bytes);
        while let Some((frame, raw)) = self.framer.next_raw() {
            let at = Instant::now();
            if frame.data.is_empty() {
                // A frame with no `data:` field is either a comment / keep-alive, or a bare JSON
                // line: Command Code's live endpoint emits newline-delimited event objects rather
                // than SSE records, and the framer — which implements the SSE spec — sees only
                // unknown fields in them. Re-read the raw text so those events are not lost.
                self.ingest_text(&String::from_utf8_lossy(&raw), at);
            } else {
                self.ingest_text(&frame.data, at);
            }
        }
    }

    /// The usage accumulated so far.
    pub fn usage(&self) -> TokenUsage {
        self.usage
    }

    /// Whether any frame has carried output.
    pub fn saw_output(&self) -> bool {
        self.first_output_at.is_some()
    }

    /// Whether an explicit end-of-stream signal was observed.
    ///
    /// Advisory: the window never depends on it. It exists so a caller can tell a stream that ended
    /// deliberately (`[DONE]`, `message_stop`, a `finish` event) from one that merely stopped.
    pub fn saw_done(&self) -> bool {
        self.saw_done
    }

    /// How many frames carried output.
    pub fn output_frames(&self) -> u64 {
        self.output_frames
    }

    /// Finish the measurement, draining any partial trailing frame first.
    ///
    /// A connection that dies mid-frame leaves bytes the framer never completed. They are fed
    /// through the same ingestion as a complete frame, because a final fragment is often the usage
    /// report or the last token, and dropping it would silently truncate the measurement.
    pub fn finish(mut self) -> GenerationUsage {
        if !self.framer.pending().is_empty() {
            let pending = String::from_utf8_lossy(self.framer.pending()).into_owned();
            let at = Instant::now();
            self.ingest_text(&pending, at);
            let _ = self.framer.take_tail();
        }
        self.into_generation_usage()
    }

    /// The measurement, without consuming the tap.
    ///
    /// Equivalent to [`Self::finish`] but callable on a borrowed tap — and it does **not** drain a
    /// partial tail, because that would make a read mutate. Use `finish` at the end of a stream.
    pub fn generation_usage(&self) -> GenerationUsage {
        GenerationUsage {
            usage: self.usage,
            generation_ms: match (self.first_output_at, self.last_output_at) {
                (Some(first), Some(last)) => {
                    // `duration_since` saturates at zero rather than panicking on a clock that
                    // moved backwards; the clamp keeps the conversion into `u64` honest.
                    Some(last.duration_since(first).as_millis().min(u64::MAX as u128) as u64)
                }
                // No output was seen, so no window can be claimed. Zero would be a lie.
                _ => None,
            },
        }
    }

    fn into_generation_usage(self) -> GenerationUsage {
        GenerationUsage {
            usage: self.usage,
            generation_ms: match (self.first_output_at, self.last_output_at) {
                (Some(first), Some(last)) => {
                    Some(last.duration_since(first).as_millis().min(u64::MAX as u128) as u64)
                }
                _ => None,
            },
        }
    }

    /// Record that output was seen at `at`, opening or extending the window.
    fn mark_output(&mut self, at: Instant) {
        if self.first_output_at.is_none() {
            self.first_output_at = Some(at);
        }
        self.last_output_at = Some(at);
        self.output_frames += 1;
    }

    /// Ingest one piece of frame text, whatever shape it arrived in.
    ///
    /// Handles, in order: the OpenAI `[DONE]` sentinel, a whole JSON document, and then a
    /// line-oriented fallback that tolerates SSE field prefixes and bare NDJSON lines. A line that
    /// is not JSON is ignored rather than treated as an error — telemetry must never be able to
    /// fail a request, and one unusable frame must not stop the rest of a stream being measured.
    fn ingest_text(&mut self, text: &str, at: Instant) {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "[DONE]" {
            self.saw_done = true;
            return;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            self.ingest_value(&value, at);
            return;
        }

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            // A frame the caller handed us with its SSE fields still attached. Strip the ones we
            // can carry; `event:` and `id:` describe the frame rather than its payload, and the
            // payload's own `type` field is what the dialect check reads.
            let payload = line
                .strip_prefix("data:")
                .or_else(|| line.strip_prefix("event:"))
                .or_else(|| line.strip_prefix("id:"))
                .or_else(|| line.strip_prefix("retry:"))
                .unwrap_or(line)
                .trim();
            if payload.is_empty() {
                continue;
            }
            if payload == "[DONE]" {
                self.saw_done = true;
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(payload) {
                self.ingest_value(&value, at);
            }
        }
    }

    /// Fold one parsed JSON value — an object, or Google's batched array of them — into the tap.
    fn ingest_value(&mut self, value: &Value, at: Instant) {
        let reported = usage_from_frame(value);
        if !reported.is_empty() {
            self.usage = self.usage.merged_with(reported);
        }
        if frame_reports_done(value) {
            self.saw_done = true;
        }
        if carries_output(value) {
            self.mark_output(at);
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Reading one frame
// -------------------------------------------------------------------------------------------------

/// Read usage out of any shape a stream frame puts it in.
///
/// [`TokenUsage::from_response_value`] covers the top-level spellings and the Google `{response: …}`
/// envelope. Anthropic is the reason for the extra look: its `message_start` nests usage inside
/// `message`, so the input count never appears at the top level of a stream frame. Without this the
/// input half of every Anthropic stream would be unrecorded while the output half was not.
fn usage_from_frame(value: &Value) -> TokenUsage {
    match value {
        // Google may batch several responses into one JSON array frame; each element is one report.
        Value::Array(items) => items.iter().fold(TokenUsage::default(), |acc, item| {
            acc.merged_with(usage_from_frame(item))
        }),
        _ => {
            let direct = TokenUsage::from_response_value(value);
            if !direct.is_empty() {
                return direct;
            }
            for key in ["response", "message", "data"] {
                if let Some(inner) = value.get(key) {
                    let nested = TokenUsage::from_response_value(inner);
                    if !nested.is_empty() {
                        return nested;
                    }
                }
            }
            TokenUsage::default()
        }
    }
}

/// Whether a frame carries an explicit end-of-stream signal.
fn frame_reports_done(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(frame_reports_done),
        Value::Object(map) => {
            if map.get("done").and_then(Value::as_bool) == Some(true) {
                return true;
            }
            if let Some(kind) = map.get("type").and_then(Value::as_str) {
                if matches!(
                    kind,
                    "message_stop"
                        | "response.completed"
                        | "response.incomplete"
                        | "response.failed"
                        | "finish"
                        | "error"
                ) {
                    return true;
                }
            }
            // Anthropic ends a turn with a `stop_reason` on `message_delta`.
            if map
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .is_some_and(|r| !r.is_null())
            {
                return true;
            }
            // An OpenAI-style final chunk carries a `finish_reason` instead.
            if map
                .get("choices")
                .and_then(Value::as_array)
                .is_some_and(|choices| {
                    choices
                        .iter()
                        .any(|c| c.get("finish_reason").is_some_and(|r| !r.is_null()))
                })
            {
                return true;
            }
            for key in ["response", "message"] {
                if let Some(inner) = map.get(key) {
                    if inner.is_object() && frame_reports_done(inner) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// Whether a frame carries output the model actually wrote.
///
/// Deliberately structural rather than dialect-switched: the caller knows the upstream's *declared*
/// dialect, but a provider is free to answer in a shape it did not advertise, and this is a check
/// over fields that are present rather than over a name. See the table in the module documentation
/// for the shapes it recognises.
fn carries_output(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(carries_output),
        Value::Object(map) => {
            if object_carries_output(value) {
                return true;
            }
            // Look through the envelopes a frame may wrap its payload in: Google's Cloud Code
            // `{response: …}` and a few proxies' `{data: …}`. `message` is included so an Anthropic
            // frame written in the non-stream envelope would still be read.
            for key in ["response", "message", "data"] {
                if let Some(inner) = map.get(key) {
                    if (inner.is_object() || inner.is_array()) && carries_output(inner) {
                        return true;
                    }
                }
            }
            false
        }
        _ => false,
    }
}

/// The checks against one object, before envelope unwrapping.
fn object_carries_output(value: &Value) -> bool {
    // OpenAI Chat Completions, and every pivot-shaped chunk a translator emits internally.
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(delta) = choice.get("delta") {
                if delta_carries_output(delta) {
                    return true;
                }
            }
            // A non-streaming `message` has the same output-bearing fields as a `delta`.
            if let Some(message) = choice.get("message") {
                if delta_carries_output(message) {
                    return true;
                }
            }
        }
    }

    // Anthropic `content_block_delta`.
    if anthropic_delta_carries_output(value) {
        return true;
    }

    // OpenAI Responses deltas, and Command Code's `type`-tagged events.
    if typed_delta_carries_output(value) {
        return true;
    }

    // A Responses function call announced whole.
    if value
        .get("item")
        .and_then(|i| i.get("type"))
        .and_then(Value::as_str)
        == Some("function_call")
    {
        return true;
    }

    // Gemini / Antigravity.
    if google_carries_output(value) {
        return true;
    }

    false
}

/// Whether an OpenAI-style `delta` (or `message`) object carries text or tool output.
fn delta_carries_output(delta: &Value) -> bool {
    if non_empty_str(delta, "content")
        || non_empty_str(delta, "reasoning_content")
        || non_empty_str(delta, "reasoning")
    {
        return true;
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            if let Some(function) = call.get("function") {
                if non_empty_str(function, "arguments") || non_empty_str(function, "name") {
                    return true;
                }
            }
            if non_empty_str(call, "arguments") {
                return true;
            }
        }
    }
    if let Some(function) = delta.get("function_call") {
        if non_empty_str(function, "arguments") || non_empty_str(function, "name") {
            return true;
        }
    }
    false
}

/// Whether an Anthropic `content_block_delta` carries output bytes.
fn anthropic_delta_carries_output(value: &Value) -> bool {
    let Some(delta) = value.get("delta") else {
        return false;
    };
    match delta.get("type").and_then(Value::as_str).unwrap_or("") {
        "text_delta" => non_empty_str(delta, "text"),
        "thinking_delta" => non_empty_str(delta, "thinking"),
        "input_json_delta" => non_empty_str(delta, "partial_json"),
        // A `signature_delta` is bookkeeping for a thinking block, not a token the model wrote.
        _ => false,
    }
}

/// Whether a `type`-tagged delta event carries output.
///
/// This is the broad arm, and it is gated on the type name containing `delta` so a frame whose only
/// content is a nested object (Anthropic's `message_delta`, which carries a `stop_reason`) cannot be
/// mistaken for output. Command Code spells the payload `text` where the Responses API spells it
/// `delta`, so both are accepted.
fn typed_delta_carries_output(value: &Value) -> bool {
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return false;
    };
    if kind == "tool-call" {
        // Command Code's atomic call: one event carries the whole function call.
        return value.get("input").is_some() || non_empty_str(value, "toolName");
    }
    if !kind.contains("delta") {
        return false;
    }
    non_empty_str(value, "delta") || non_empty_str(value, "text")
}

/// Whether a Gemini-shaped frame carries output text or a function call.
fn google_carries_output(value: &Value) -> bool {
    let Some(candidates) = value.get("candidates").and_then(Value::as_array) else {
        return false;
    };
    for candidate in candidates {
        let parts = candidate
            .get("content")
            .and_then(|content| content.get("parts"))
            .and_then(Value::as_array);
        let Some(parts) = parts else {
            continue;
        };
        for part in parts {
            if non_empty_str(part, "text") {
                return true;
            }
            if part.get("functionCall").is_some() {
                return true;
            }
        }
    }
    false
}

/// Whether a key holds a non-empty string.
fn non_empty_str(value: &Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Build a frame carrying an SSE `data:` payload, the way the relay's framer would.
    fn frame(data: &str) -> Frame {
        Frame {
            data: data.to_owned(),
            ..Frame::default()
        }
    }

    #[test]
    fn an_openai_style_stream_reports_input_output_and_cached_tokens() {
        // Instants are pinned so the window is exact: the production path stamps `Instant::now()`,
        // which two adjacent frames would collapse into a zero-length window.
        let t0 = Instant::now();
        let mut tap = UsageTap::new();
        tap.observe_frame_at(&frame(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#), t0);
        tap.observe_frame_at(
            &frame(r#"{"choices":[{"delta":{"content":" world"}}]}"#),
            t0 + Duration::from_millis(400),
        );
        tap.observe_frame_at(
            &frame(
                r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2,"prompt_tokens_details":{"cached_tokens":5}}}"#,
            ),
            t0 + Duration::from_millis(401),
        );
        tap.observe_frame_at(&frame("[DONE]"), t0 + Duration::from_millis(402));

        // Read before finishing, because `finish` consumes the tap.
        let out = tap.generation_usage();
        assert_eq!(out.usage.input_tokens, Some(9));
        assert_eq!(out.usage.output_tokens, Some(2));
        assert_eq!(out.usage.cache_read_tokens, Some(5));
        assert_eq!(out.generation_ms, Some(400), "first output token to last");
        assert_eq!(out.tokens_per_second(), Some(5.0), "2 tokens over 400 ms");
        assert_eq!(
            tap.output_frames(),
            2,
            "the usage and DONE frames are not output"
        );
        assert!(tap.saw_done());
        assert_eq!(tap.finish(), out, "finishing changes nothing once drained");
    }

    #[test]
    fn every_dialects_output_frame_is_recognised() {
        // One case per row of the table in the module docs, so a dialect added later has an obvious
        // place to add its own.
        let cases: &[(&str, &str)] = &[
            (
                "openai content",
                r#"{"choices":[{"delta":{"content":"x"}}]}"#,
            ),
            (
                "openai reasoning",
                r#"{"choices":[{"delta":{"reasoning_content":"x"}}]}"#,
            ),
            (
                "openai tool arguments",
                r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{a}"}}]}}]}"#,
            ),
            (
                "responses text",
                r#"{"type":"response.output_text.delta","delta":"x"}"#,
            ),
            (
                "responses reasoning",
                r#"{"type":"response.reasoning_summary_text.delta","delta":"x"}"#,
            ),
            (
                "responses tool arguments",
                r#"{"type":"response.function_call_arguments.delta","delta":"{a}"}"#,
            ),
            (
                "anthropic text",
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}"#,
            ),
            (
                "anthropic thinking",
                r#"{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"x"}}"#,
            ),
            (
                "anthropic tool input",
                r#"{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{a}"}}"#,
            ),
            (
                "google",
                r#"{"candidates":[{"content":{"parts":[{"text":"x"}]}}]}"#,
            ),
            (
                "google function call",
                r#"{"candidates":[{"content":{"parts":[{"functionCall":{"name":"f","args":{}}}]}}]}"#,
            ),
            ("command code text", r#"{"type":"text-delta","text":"x"}"#),
            (
                "command code tool fragment",
                r#"{"type":"tool-delta","text":"{a}"}"#,
            ),
            (
                "command code atomic call",
                r#"{"type":"tool-call","toolName":"f","input":{}}"#,
            ),
        ];
        for (name, payload) in cases {
            let mut tap = UsageTap::new();
            tap.observe_data(payload);
            assert!(
                tap.saw_output(),
                "the {name} frame carries output but was not counted"
            );
        }
    }

    #[test]
    fn bookkeeping_frames_are_not_mistaken_for_output() {
        // Each of these arrives before or after the model writes. Counting one would either start
        // the window early or extend it past the last token.
        let cases: &[(&str, &str)] = &[
            (
                "role only",
                r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            ),
            ("empty content", r#"{"choices":[{"delta":{"content":""}}]}"#),
            (
                "anthropic start",
                r#"{"type":"message_start","message":{"content":[]}}"#,
            ),
            (
                "anthropic block start",
                r#"{"type":"content_block_start","content_block":{"type":"text","text":""}}"#,
            ),
            (
                "anthropic message delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
            ),
            (
                "usage only",
                r#"{"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
            ),
            (
                "google usage only",
                r#"{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}}"#,
            ),
            (
                "empty google parts",
                r#"{"candidates":[{"content":{"parts":[{"text":""}]}}]}"#,
            ),
            (
                "command code empty delta",
                r#"{"type":"text-delta","text":""}"#,
            ),
            ("command code message start", r#"{"type":"message-start"}"#),
        ];
        for (name, payload) in cases {
            let mut tap = UsageTap::new();
            tap.observe_data(payload);
            assert!(
                !tap.saw_output(),
                "the {name} frame is bookkeeping but was counted as output"
            );
        }
    }

    #[test]
    fn the_done_sentinel_marks_the_stream_finished_without_counting_as_output() {
        let mut tap = UsageTap::new();
        tap.observe_frame(&frame("  [DONE] "));
        assert!(tap.saw_done());
        assert!(!tap.saw_output());
        assert_eq!(tap.finish().generation_ms, None);
    }

    #[test]
    fn a_stream_with_no_output_frame_reports_no_generation_window() {
        // `observe_frame` has to accept a frame whose `data` is empty, is a comment, or is not JSON
        // at all: a keep-alive is a frame, a truncated final frame is a frame, and a provider that
        // emits a line this reader does not understand must not be able to make it fail. None of
        // them wrote a token, so none of them may open a window — the answer must be `None`, which
        // says "we cannot know the rate", never `Some(0)`, which would claim an instantaneous answer.
        let mut tap = UsageTap::new();
        assert!(frame("").is_keepalive());
        tap.observe_frame(&frame(""));
        tap.observe_frame(&frame("   \n  "));
        tap.observe_frame(&Frame {
            data: String::new(),
            comments: vec!["keep-alive".into()],
            ..Frame::default()
        });
        tap.observe_frame(&frame("not json at all"));
        tap.observe_frame(&frame("data: {\"unterminated\": "));

        assert!(!tap.saw_output(), "nothing here carried output");
        let out = tap.finish();
        assert_eq!(out.generation_ms, None, "no output token, so no window");
        assert!(out.usage.is_empty(), "and nothing invented a count");
        assert_eq!(out.tokens_per_second(), None);
        assert!(out.is_empty(), "so there is no row worth writing");
    }

    #[test]
    fn a_usage_frame_alone_still_reports_no_generation_window() {
        // The two measurements come from different frames and are decided independently: an upstream
        // that reports counts without ever streaming a token we recognise leaves the rate unknown.
        let mut tap = UsageTap::new();
        tap.observe_data(r#"{"usage":{"prompt_tokens":80,"completion_tokens":12}}"#);
        let out = tap.finish();
        assert_eq!(out.usage.output_tokens, Some(12));
        assert_eq!(out.generation_ms, None);
        assert_eq!(out.tokens_per_second(), None);
        assert!(
            !out.is_empty(),
            "the counts alone are still worth recording"
        );
    }

    #[test]
    fn observing_raw_bytes_frames_an_sse_stream_split_at_arbitrary_boundaries() {
        // The pass-through path forwards opaque chunks; a `data:` line has no obligation to arrive
        // whole, and a tap that only worked on tidy frames would silently measure nothing.
        let wire = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"one\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"two\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut tap = UsageTap::new();
        for byte in wire.as_bytes() {
            tap.observe_chunk(std::slice::from_ref(byte));
        }
        assert_eq!(
            tap.output_frames(),
            2,
            "read before finishing, which consumes"
        );
        assert!(tap.saw_done());
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(4));
        assert_eq!(out.usage.output_tokens, Some(2));
    }

    #[test]
    fn observing_raw_bytes_reads_command_codes_newline_delimited_json() {
        // A non-SSE frame reaches the framer as unknown fields, so the raw text has to be re-read;
        // otherwise a Command Code stream would measure nothing at all.
        let wire = concat!(
            "{\"type\":\"message-start\",\"id\":\"x\"}\n\n",
            "{\"type\":\"text-delta\",\"text\":\"Hi\"}\n\n",
            "{\"type\":\"finish\",\"totalUsage\":{\"inputTokens\":6,\"outputTokens\":1}}\n\n",
        );
        let mut tap = UsageTap::new();
        tap.observe_chunk(wire.as_bytes());
        assert!(tap.saw_output());
        assert!(tap.saw_done());
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(6));
        assert_eq!(out.usage.output_tokens, Some(1));
    }

    #[test]
    fn a_partial_trailing_frame_is_still_measured_when_the_stream_dies() {
        // A connection that dies mid-frame must not drop the last thing the model said, nor a usage
        // report that was half-written when the socket closed.
        let mut tap = UsageTap::new();
        tap.observe_chunk(
            b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":7}}",
        );
        assert!(tap.saw_output(), "the complete content frame was observed");
        assert_eq!(
            tap.usage().input_tokens,
            None,
            "the usage frame is incomplete, so its count is not known yet"
        );
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(7));
        assert_eq!(
            out.generation_ms,
            Some(0),
            "one output frame, so a zero-length window"
        );
        assert_eq!(
            out.tokens_per_second(),
            None,
            "and a zero-length window still yields no rate"
        );
    }

    #[test]
    fn a_whole_json_document_can_be_observed_as_one_frame() {
        // Not every upstream is streaming, and a caller that has a buffered body should get the same
        // answer from this tap as `TokenUsage::from_body` would give it.
        let mut tap = UsageTap::new();
        tap.observe_data(
            r#"{
                 "object": "chat.completion",
                 "choices": [{"message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                 "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15}
               }"#,
        );
        assert!(tap.saw_output(), "a buffered answer is still output");
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(12));
        assert_eq!(out.usage.output_tokens, Some(3));
    }

    #[test]
    fn an_anthropic_stream_has_its_input_count_read_from_beneath_the_message_envelope() {
        // Anthropic nests usage inside `message` on `message_start`, so a reader that only looked at
        // the top level would record the output half of every Anthropic stream and none of the input
        // half — silently reporting every prompt as free.
        let mut tap = UsageTap::new();
        tap.observe_data(
            r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":120,"cache_read_input_tokens":90,"output_tokens":1}}}"#,
        );
        tap.observe_data(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
        );
        tap.observe_data(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":17}}"#,
        );
        tap.observe_data(r#"{"type":"message_stop"}"#);

        assert!(tap.saw_done(), "message_stop ends the stream");
        assert_eq!(
            tap.output_frames(),
            1,
            "only the content delta wrote a token"
        );
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(120));
        assert_eq!(out.usage.output_tokens, Some(17), "the final count wins");
        assert_eq!(out.usage.cache_read_tokens, Some(90));
    }

    #[test]
    fn a_gemini_stream_folds_the_cumulative_metadata_that_rides_on_every_frame() {
        // Gemini re-sends an accumulating `usageMetadata`, so the last frame is the complete one; a
        // fold that summed the frames instead of replacing fields would multiply the prompt count.
        let mut tap = UsageTap::new();
        tap.observe_data(
            r#"{"candidates":[{"content":{"parts":[{"text":"a"}]}}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":1}}"#,
        );
        tap.observe_data(
            r#"{"candidates":[{"content":{"parts":[{"text":"b"}]}}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":2}}"#,
        );
        tap.observe_data(
            r#"{"candidates":[{"content":{"parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":30,"candidatesTokenCount":2,"cachedContentTokenCount":20}}"#,
        );

        assert_eq!(
            tap.output_frames(),
            2,
            "the terminal frame carries no new text"
        );
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(30), "not 60 or 90");
        assert_eq!(out.usage.output_tokens, Some(2));
        assert_eq!(out.usage.cache_read_tokens, Some(20));
    }

    #[test]
    fn a_command_code_stream_reports_its_final_total_usage() {
        // The `finish` event is the only frame that carries counts, and its shape is its own
        // (`totalUsage.inputTokens`), which is why the reader is not dialect-switched.
        let mut tap = UsageTap::new();
        tap.observe_data(r#"{"type":"message-start","id":"c1"}"#);
        tap.observe_data(r#"{"type":"text-delta","text":"Hi"}"#);
        tap.observe_data(
            r#"{"type":"finish","finishReason":"stop","totalUsage":{"inputTokens":21,"outputTokens":8}}"#,
        );

        assert!(tap.saw_done());
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(21));
        assert_eq!(out.usage.output_tokens, Some(8));
        assert!(
            out.generation_ms.is_some(),
            "one output frame starts a window"
        );
        assert!(
            !out.is_empty(),
            "a stream with counts is worth recording even if the window is degenerate"
        );
    }

    #[test]
    fn a_field_a_provider_sends_as_a_float_or_a_string_is_still_a_count() {
        // Real gateways emit `1234.0` and occasionally `"1234"`; the reading must survive both or a
        // stream would report nothing where the buffered path reports a number.
        let mut tap = UsageTap::new();
        tap.observe_data(
            r#"{"usage":{"prompt_tokens":10.0,"completion_tokens":"4","prompt_tokens_details":{"cached_tokens":6.0}}}"#,
        );
        let out = tap.finish();
        assert_eq!(out.usage.input_tokens, Some(10));
        assert_eq!(out.usage.output_tokens, Some(4));
        assert_eq!(out.usage.cache_read_tokens, Some(6));
    }

    #[test]
    fn a_later_frame_wins_field_by_field_over_an_earlier_one() {
        // Anthropic sends output twice in some flows (a cumulative estimate then the final count);
        // last-writer-wins per field is what makes the most complete report fall out of the fold.
        let mut tap = UsageTap::new();
        tap.observe_data(r#"{"usage":{"output_tokens":1,"input_tokens":50}}"#);
        tap.observe_data(r#"{"usage":{"output_tokens":99}}"#);
        let out = tap.finish();
        assert_eq!(
            out.usage.input_tokens,
            Some(50),
            "the later frame omitted input"
        );
        assert_eq!(
            out.usage.output_tokens,
            Some(99),
            "the later frame corrected output"
        );
    }

    #[test]
    fn usage_does_not_invent_a_window_and_output_does_not_invent_usage() {
        // The two halves are measured independently: a stream can have one without the other, and
        // neither may be fabricated from the other.
        let t0 = Instant::now();
        let mut output_only = UsageTap::new();
        output_only.observe_frame_at(&frame(r#"{"type":"text-delta","text":"x"}"#), t0);
        output_only.observe_frame_at(
            &frame(r#"{"type":"text-delta","text":"y"}"#),
            t0 + Duration::from_millis(100),
        );
        let out = output_only.finish();
        assert_eq!(out.generation_ms, Some(100));
        assert!(out.usage.is_empty());
        assert_eq!(out.tokens_per_second(), None, "no reported tokens, no rate");

        let mut usage_only = UsageTap::new();
        usage_only.observe_frame(&frame(
            r#"{"usage":{"prompt_tokens":3,"completion_tokens":9}}"#,
        ));
        let out = usage_only.finish();
        assert_eq!(out.usage.output_tokens, Some(9));
        assert_eq!(out.generation_ms, None);
    }

    #[test]
    fn a_generation_usage_with_nothing_in_it_reports_itself_empty() {
        assert!(GenerationUsage::default().is_empty());
        assert!(!GenerationUsage {
            usage: TokenUsage {
                output_tokens: Some(1),
                ..Default::default()
            },
            generation_ms: None,
        }
        .is_empty());
        assert!(!GenerationUsage {
            usage: TokenUsage::default(),
            generation_ms: Some(0),
        }
        .is_empty());
    }

    #[test]
    fn generation_usage_round_trips_through_serde() {
        // It is what the relay hands to whatever writes the usage row, so it has to survive a trip
        // through JSON — including `null`, which is how an absent window must be spelled.
        let original = GenerationUsage {
            usage: TokenUsage {
                input_tokens: Some(10),
                output_tokens: Some(4),
                cache_read_tokens: Some(6),
                cache_write_tokens: None,
            },
            generation_ms: Some(250),
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(json.contains(r#""generation_ms":250"#));
        let back: GenerationUsage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);

        let unknown = GenerationUsage::default();
        let json = serde_json::to_string(&unknown).unwrap();
        assert!(json.contains(r#""generation_ms":null"#));
        assert_eq!(
            serde_json::from_str::<GenerationUsage>(&json).unwrap(),
            unknown
        );
    }
}
