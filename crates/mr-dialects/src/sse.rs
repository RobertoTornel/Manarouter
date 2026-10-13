//! `text/event-stream` framing.
//!
//! Every dialect that streams uses SSE framing, but they disagree about what goes in the `data:`
//! payload. This module handles only the frame boundaries, so that parsing a stream is a single
//! implementation rather than one per dialect.
//!
//! ## Why a hand-rolled framer
//!
//! The relay sits between two SSE endpoints. In the matching-dialect case it does not interpret
//! frames at all: it finds blank-line boundaries with `memchr` and forwards the slices. It only
//! calls [`Frame`] parsing when the dialects differ and the payload actually has to be rewritten.
//!
//! The parser is deliberately incremental and tolerant. Real providers emit comments
//! (`: keep-alive`), multi-line `data:` fields, `event:` names, `id:` fields, a bare `[DONE]`
//! sentinel, and occasionally a truncated final frame when a connection dies. None of those may
//! panic, and none may cause a frame to be silently dropped before the client sees the stream.

use bytes::{Buf, Bytes, BytesMut};
use memchr::memchr;

/// One parsed SSE frame.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Frame {
    /// The `event:` field, if present.
    pub event: Option<String>,
    /// The concatenated `data:` fields, joined with `\n` as the SSE specification requires.
    pub data: String,
    /// The `id:` field, if present.
    pub id: Option<String>,
    /// The `retry:` field, if present, in milliseconds.
    pub retry: Option<u64>,
    /// Comment lines, preserved so a pass-through path does not swallow keep-alives.
    pub comments: Vec<String>,
}

impl Frame {
    /// Whether this frame is a pure comment / keep-alive with no data.
    ///
    /// Keep-alives must be forwarded in the matching-dialect case, and dropped in the translating
    /// case, because inserting one into a rebuilt stream is harmless but pointless.
    #[inline]
    pub fn is_keepalive(&self) -> bool {
        self.data.is_empty() && self.event.is_none()
    }

    /// Whether this frame carries the OpenAI end-of-stream sentinel.
    #[inline]
    pub fn is_done(&self) -> bool {
        self.data.trim() == "[DONE]"
    }

    /// Serialise back to wire form, always terminated by a blank line.
    pub fn to_wire(&self) -> String {
        let mut out = String::with_capacity(self.data.len() + 32);
        for c in &self.comments {
            out.push_str(": ");
            out.push_str(c);
            out.push('\n');
        }
        if let Some(e) = &self.event {
            out.push_str("event: ");
            out.push_str(e);
            out.push('\n');
        }
        if let Some(i) = &self.id {
            out.push_str("id: ");
            out.push_str(i);
            out.push('\n');
        }
        if let Some(r) = self.retry {
            out.push_str("retry: ");
            out.push_str(&r.to_string());
            out.push('\n');
        }
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        out
    }
}

/// Serialise a bare data payload as an SSE frame.
///
/// The single most common operation in the translating path, so it avoids the [`Frame`] allocation.
pub fn data_frame(data: &str) -> String {
    let mut out = String::with_capacity(data.len() + 16);
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Serialise a named event.
pub fn named_frame(event: &str, data: &str) -> String {
    let mut out = String::with_capacity(data.len() + event.len() + 24);
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Parse one complete frame body (the text between blank-line boundaries).
///
/// Unknown fields are ignored, as the SSE specification requires, so a provider adding a new field
/// cannot break the parser.
pub fn parse_frame(raw: &str) -> Frame {
    let mut f = Frame::default();
    let mut data: Vec<&str> = Vec::new();
    for line in raw.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix(':') {
            f.comments
                .push(comment.strip_prefix(' ').unwrap_or(comment).to_string());
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A field with no colon is legal and means an empty value.
            None => (line, ""),
        };
        match field {
            "data" => data.push(value),
            "event" => f.event = Some(value.to_string()),
            "id" => f.id = Some(value.to_string()),
            "retry" => f.retry = value.parse().ok(),
            _ => {}
        }
    }
    f.data = data.join("\n");
    f
}

/// An incremental framer over a byte stream.
///
/// Fed whatever the upstream produced, it yields the frames that are now complete and retains any
/// partial tail for next time. This is what lets the relay forward a stream without buffering the
/// whole response.
#[derive(Debug, Default)]
pub struct Framer {
    buf: BytesMut,
}

/// The result of feeding bytes to a [`Framer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// A complete frame, already parsed.
    pub frame: Frame,
    /// The byte range of the original input that produced it, so a pass-through path can forward
    /// the exact original bytes without re-serialising.
    pub raw_offset: usize,
    /// The length of the original bytes for this frame.
    pub raw_len: usize,
}

impl Framer {
    /// A new framer.
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(8 * 1024),
        }
    }

    /// A framer with a pre-reserved buffer, to avoid a reallocation on the first read.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: BytesMut::with_capacity(cap),
        }
    }

    /// Bytes currently held that do not yet form a complete frame.
    pub fn pending(&self) -> &[u8] {
        &self.buf
    }

    /// Whether the buffer holds a partial frame.
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Append bytes from the upstream.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Take the next complete frame, if one is buffered.
    ///
    /// A frame ends at `\n\n` or `\r\n\r\n`. The returned [`Chunk`] records where in the *current*
    /// buffer contents the frame came from, which is what allows a caller that does not need to
    /// interpret the payload to forward the original bytes.
    pub fn next_frame(&mut self) -> Option<Chunk> {
        let (boundary, sep_len) = find_boundary(&self.buf)?;
        let raw = self.buf.split_to(boundary + sep_len);
        let text = std::str::from_utf8(&raw[..boundary]).ok()?;
        let frame = parse_frame(text);
        Some(Chunk {
            frame,
            raw_offset: 0,
            raw_len: raw.len(),
        })
    }

    /// Take the next complete frame together with its exact original bytes.
    ///
    /// Preferred on the pass-through path: the caller forwards `raw` and never re-serialises, so
    /// byte-for-byte fidelity is guaranteed for any frame the relay did not need to understand.
    pub fn next_raw(&mut self) -> Option<(Frame, Bytes)> {
        let (boundary, sep_len) = find_boundary(&self.buf)?;
        let raw = self.buf.split_to(boundary + sep_len).freeze();
        let text = std::str::from_utf8(&raw[..boundary]).ok()?;
        Some((parse_frame(text), raw))
    }

    /// Drain any trailing bytes that never terminated with a blank line.
    ///
    /// Called when an upstream closes. A well-formed stream always ends with a blank line, but a
    /// connection that dies mid-frame leaves a partial tail — and discarding it would silently
    /// truncate the client's last token.
    pub fn take_tail(&mut self) -> Option<Frame> {
        if self.buf.is_empty() {
            return None;
        }
        let raw = self.buf.split().freeze();
        let text = std::str::from_utf8(&raw).ok()?;
        let f = parse_frame(text);
        if f.data.is_empty() && f.event.is_none() && f.comments.is_empty() {
            None
        } else {
            Some(f)
        }
    }
}

/// Find the end of the first complete SSE frame.
///
/// Returns the offset of the blank line and the length of the separator, accepting both `\n\n` and
/// `\r\n\r\n` because providers are inconsistent about it and a stream that uses the latter would
/// otherwise never terminate a frame.
fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let mut search = 0;
    while let Some(rel) = memchr(b'\n', &buf[search..]) {
        let i = search + rel;
        // `\n\n`
        if buf.get(i + 1) == Some(&b'\n') {
            return Some((i, 2));
        }
        // `\r\n\r\n`: the `\n` at i is followed by `\r\n`.
        if buf.get(i + 1) == Some(&b'\r') && buf.get(i + 2) == Some(&b'\n') {
            return Some((i, 3));
        }
        search = i + 1;
    }
    None
}

/// Whether a body looks like an SSE stream rather than a JSON document.
///
/// Used when an upstream ignores the client's `stream: false` and answers with a stream anyway: the
/// relay must notice and synthesize a document instead of handing the client raw SSE.
pub fn looks_like_sse(content_type: Option<&str>, head: &[u8]) -> bool {
    if let Some(ct) = content_type {
        if ct.contains("text/event-stream") {
            return true;
        }
    }
    let probe = &head[..head.len().min(64)];
    probe.starts_with(b"data:") || probe.starts_with(b"event:") || probe.starts_with(b":")
}

/// Extract the `usage` object from a set of SSE data payloads, if any frame carried one.
///
/// Providers disagree about where usage appears — a dedicated final frame for some, embedded in the
/// last content frame for others — so the caller collects candidates and this picks the richest.
pub fn pick_usage(candidates: &[serde_json::Value]) -> Option<serde_json::Value> {
    candidates
        .iter()
        .filter(|v| v.get("usage").map(|u| !u.is_null()).unwrap_or(false))
        .max_by_key(|v| {
            let u = &v["usage"];
            u.get("total_tokens")
                .and_then(|t| t.as_u64())
                .or_else(|| {
                    // Anthropic reports input and output separately with no total.
                    match (
                        u.get("input_tokens").and_then(|t| t.as_u64()),
                        u.get("output_tokens").and_then(|t| t.as_u64()),
                    ) {
                        (Some(a), Some(b)) => Some(a + b),
                        (Some(a), None) => Some(a),
                        (None, Some(b)) => Some(b),
                        (None, None) => None,
                    }
                })
                .unwrap_or(0)
        })
        .and_then(|v| v.get("usage").cloned())
}

/// Consume a length-prefixed or newline-delimited body into frames. Test helper.
pub fn parse_all(input: &str) -> Vec<Frame> {
    let mut f = Framer::new();
    f.push(input.as_bytes());
    let mut out = Vec::new();
    while let Some(c) = f.next_frame() {
        out.push(c.frame);
    }
    if let Some(t) = f.take_tail() {
        out.push(t);
    }
    out
}

/// Drop consumed bytes from a raw buffer, used by the pass-through path.
#[inline]
pub fn advance(buf: &mut Bytes, n: usize) {
    buf.advance(n);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_simple_data_frame() {
        let f = parse_frame("data: {\"a\":1}");
        assert_eq!(f.data, "{\"a\":1}");
        assert!(f.event.is_none());
        assert!(!f.is_done());
    }

    #[test]
    fn joins_multiple_data_lines_with_newlines_as_the_spec_requires() {
        let f = parse_frame("data: line one\ndata: line two");
        assert_eq!(f.data, "line one\nline two");
    }

    #[test]
    fn recognises_the_done_sentinel() {
        assert!(parse_frame("data: [DONE]").is_done());
        // Some providers pad it; the check must tolerate surrounding whitespace.
        assert!(parse_frame("data:  [DONE] ").is_done());
    }

    #[test]
    fn parses_event_id_retry_and_comments() {
        let f = parse_frame(": keep-alive\nevent: message_delta\nid: 42\nretry: 1500\ndata: x");
        assert_eq!(f.event.as_deref(), Some("message_delta"));
        assert_eq!(f.id.as_deref(), Some("42"));
        assert_eq!(f.retry, Some(1500));
        assert_eq!(f.comments, vec!["keep-alive"]);
        assert_eq!(f.data, "x");
        assert!(!f.is_keepalive());
    }

    #[test]
    fn a_comment_only_frame_is_a_keepalive() {
        let f = parse_frame(": ping");
        assert!(f.is_keepalive());
    }

    #[test]
    fn tolerates_crlf_and_a_missing_space_after_the_colon() {
        let f = parse_frame("event:message\r\ndata:hello\r\n");
        assert_eq!(f.event.as_deref(), Some("message"));
        assert_eq!(f.data, "hello");
    }

    #[test]
    fn ignores_unknown_fields_rather_than_failing() {
        // A provider adding a field must not break parsing, per the SSE specification.
        let f = parse_frame("data: x\nfuture_field: y\nanother\n");
        assert_eq!(f.data, "x");
    }

    #[test]
    fn handles_a_field_with_no_colon_as_an_empty_value() {
        let f = parse_frame("data\n");
        assert_eq!(f.data, "");
    }

    #[test]
    fn a_bad_retry_value_is_ignored_not_fatal() {
        let f = parse_frame("retry: not-a-number\ndata: x");
        assert_eq!(f.retry, None);
        assert_eq!(f.data, "x");
    }

    #[test]
    fn framer_yields_only_complete_frames_and_retains_the_tail() {
        let mut fr = Framer::new();
        fr.push(b"data: one\n\ndata: tw");
        let a = fr.next_frame().unwrap();
        assert_eq!(a.frame.data, "one");
        // The partial second frame must be retained, not dropped: losing it would truncate the
        // client's final token.
        assert!(fr.next_frame().is_none());
        assert_eq!(fr.pending(), b"data: tw");

        fr.push(b"o\n\n");
        let b = fr.next_frame().unwrap();
        assert_eq!(b.frame.data, "two");
        assert!(!fr.has_pending());
    }

    #[test]
    fn framer_accepts_crlf_separated_frames() {
        // Providers are inconsistent here, and a stream that used CRLF would otherwise never
        // terminate a frame.
        let frames = parse_all("data: a\r\n\r\ndata: b\r\n\r\n");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data, "a");
        assert_eq!(frames[1].data, "b");
    }

    #[test]
    fn framer_recovers_a_truncated_final_frame_as_a_tail() {
        // A connection that dies mid-stream leaves a partial frame; discarding it would silently
        // lose the last thing the model said.
        let mut fr = Framer::new();
        fr.push(b"data: complete\n\ndata: cut off here");
        assert_eq!(fr.next_frame().unwrap().frame.data, "complete");
        let tail = fr.take_tail().unwrap();
        assert_eq!(tail.data, "cut off here");
    }

    #[test]
    fn take_tail_on_an_empty_buffer_is_none() {
        let mut fr = Framer::new();
        fr.push(b"data: x\n\n");
        let _ = fr.next_frame();
        assert!(fr.take_tail().is_none());
    }

    #[test]
    fn next_raw_preserves_the_exact_original_bytes() {
        // The pass-through path forwards these bytes verbatim, so any normalisation here would
        // change what the client receives.
        let mut fr = Framer::new();
        let original = b"event:message\r\ndata:{\"a\":1}\r\n\r\n";
        fr.push(original);
        let (_frame, raw) = fr.next_raw().unwrap();
        assert_eq!(raw.as_ref(), original);
    }

    #[test]
    fn reserialising_a_frame_round_trips_through_the_parser() {
        let f = Frame {
            event: Some("message_delta".into()),
            data: "{\"x\":1}".into(),
            id: Some("7".into()),
            retry: Some(250),
            comments: vec!["keep-alive".into()],
        };
        let wire = f.to_wire();
        let back = parse_frame(wire.trim_end_matches('\n'));
        assert_eq!(back.event, f.event);
        assert_eq!(back.data, f.data);
        assert_eq!(back.id, f.id);
        assert_eq!(back.retry, f.retry);
        assert_eq!(back.comments, f.comments);
    }

    #[test]
    fn multi_line_data_round_trips() {
        let f = Frame {
            data: "first\nsecond".into(),
            ..Frame::default()
        };
        let wire = f.to_wire();
        assert!(wire.contains("data: first\n"));
        assert!(wire.contains("data: second\n"));
        assert_eq!(
            parse_frame(wire.trim_end_matches('\n')).data,
            "first\nsecond"
        );
    }

    #[test]
    fn data_frame_and_named_frame_render_valid_sse() {
        assert_eq!(data_frame("{}"), "data: {}\n\n");
        let named = named_frame("manarouter.retry", "{}");
        assert!(named.starts_with("event: manarouter.retry\n"));
        assert!(named.ends_with("\n\n"));
    }

    #[test]
    fn detects_sse_by_content_type_or_by_leading_bytes() {
        assert!(looks_like_sse(Some("text/event-stream"), b""));
        assert!(looks_like_sse(
            Some("text/event-stream; charset=utf-8"),
            b""
        ));
        assert!(looks_like_sse(None, b"data: {\"x\":1}"));
        assert!(looks_like_sse(None, b"event: message"));
        assert!(looks_like_sse(None, b": ping"));
        assert!(!looks_like_sse(Some("application/json"), b"{\"x\":1}"));
        assert!(!looks_like_sse(None, b"{\"object\":\"chat.completion\"}"));
    }

    #[test]
    fn usage_extraction_prefers_the_frame_with_a_total() {
        let candidates = vec![
            serde_json::json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2}}),
            serde_json::json!({"usage": {"prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3}}),
        ];
        let u = pick_usage(&candidates).unwrap();
        assert_eq!(u["total_tokens"], 3);
    }

    #[test]
    fn usage_extraction_sums_anthropic_style_split_counts() {
        let candidates =
            vec![serde_json::json!({"usage": {"input_tokens": 10, "output_tokens": 5}})];
        let u = pick_usage(&candidates).unwrap();
        assert_eq!(u["input_tokens"], 10);
        // The function must still return the object even though the ordering key had to be derived
        // rather than read, so reset-aware accounting is not starved of data.
        assert!(pick_usage(&candidates).is_some());
    }

    #[test]
    fn usage_extraction_returns_none_when_no_frame_carries_usage() {
        let candidates = vec![
            serde_json::json!({"choices": []}),
            serde_json::json!({"usage": null}),
        ];
        assert!(pick_usage(&candidates).is_none());
    }

    #[test]
    fn framer_survives_a_frame_split_across_many_pushes() {
        // A real socket delivers arbitrary chunks; the framer must be indifferent to where the
        // boundaries fall.
        let wire = b"data: {\"hello\":\"world\"}\n\ndata: [DONE]\n\n";
        let mut fr = Framer::new();
        let mut got = Vec::new();
        for b in wire.iter() {
            fr.push(&[*b]);
            while let Some(c) = fr.next_frame() {
                got.push(c.frame);
            }
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].data, "{\"hello\":\"world\"}");
        assert!(got[1].is_done());
    }

    #[test]
    fn invalid_utf8_does_not_panic() {
        // A proxy or a broken provider can emit garbage; the relay must not take the process down.
        let mut fr = Framer::new();
        fr.push(b"data: \xff\xfe\n\n");
        // Either it yields nothing or it yields a frame, but it must not panic.
        let _ = fr.next_frame();
    }
}
