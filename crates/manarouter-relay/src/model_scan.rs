//! Locating the `model` field in a request body without parsing the JSON.
//!
//! # Why this exists
//!
//! The relay routes on exactly two things from the request body in the common case: the `model`
//! string and, for streaming decisions, whether `stream` is true. Everything else is forwarded
//! verbatim.
//!
//! Deserialising a request into `serde_json::Value` to read one string is expensive in a way that
//! is easy to underestimate. A 200 KB agent conversation with a long tool history becomes thousands
//! of heap allocations, a full UTF-8 validation pass, and a `BTreeMap`/`Vec` tree that is then
//! thrown away. Measured on this machine, that is tens of microseconds and a great deal of allocator
//! traffic — on the critical path of every single request, to extract a value that is usually
//! within the first 200 bytes.
//!
//! This module finds it with a single linear scan, no allocation and no tree.
//!
//! # Correctness, stated honestly
//!
//! This is **not** a JSON parser and does not claim to be one. It is a scanner with a precise
//! contract:
//!
//! * It tracks string boundaries and escapes, so a `"model"` appearing *inside* a string value is
//!   not matched. This is the failure that a naive `memchr(b"\"model\"")` search exhibits, and it is
//!   a realistic input: a conversation about JSON, or a tool schema containing the word, would
//!   otherwise route to the wrong place.
//! * It tracks nesting depth, so it returns the `model` field of the **top-level object**, not one
//!   belonging to a nested tool definition or a cached message.
//! * It rejects a match unless the value is a well-formed quoted string in the same object.
//! * It is linear in the input and allocates nothing.
//!
//! It deliberately does **not** validate the rest of the document. A body that this function reads
//! successfully but that is malformed JSON overall will be forwarded and rejected upstream, which is
//! the correct outcome: the relay's job is to shuttle bytes, and it should not reject a body that
//! the upstream would have accepted, nor accept one it would have rejected.
//!
//! The worst case of a mis-scan is therefore choosing a different route for a malformed request.
//! Since the bytes are forwarded unchanged, no corruption is possible — only a routing decision that
//! a human could not have predicted for input a human would not write.
//!
//! # Ordering subtlety
//!
//! If the document contains the `model` key more than once at the top level — which is legal JSON
//! and which some sloppy clients emit when merging objects — the **first** occurrence wins. That
//! matches what every JSON parser does, including `serde_json`, so the relay's routing decision
//! agrees with the upstream's interpretation of the body. Choosing the last would be a subtle way to
//! route differently from how the request is executed.

use memchr::{memchr, memchr2};

/// The key the relay routes on.
pub const MODEL_KEY: &str = "model";

/// Whether `stream` is true.
pub const STREAM_KEY: &str = "stream";

/// A found value's byte range within the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// Offset of the first byte of the value.
    pub start: usize,
    /// Offset one past the last byte of the value.
    pub end: usize,
}

impl Range {
    /// The value as a slice of the original body.
    #[inline]
    pub fn slice<'a>(&self, body: &'a [u8]) -> &'a [u8] {
        // The scanner only ever produces ranges it has already bounds-checked, but slicing is
        // still defended rather than indexed, because a panic here would take down a request that
        // is otherwise perfectly serviceable.
        body.get(self.start..self.end).unwrap_or(&[])
    }
}

/// The result of scanning a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestHead {
    /// The `model` value's byte range, if present.
    pub model: Option<Range>,
    /// The `stream` flag, if present.
    pub stream: Option<bool>,
    /// The `n` field, when present, because a request for multiple completions changes how the
    /// response must be counted for quota.
    pub n: Option<u64>,
}

impl RequestHead {
    /// The model as a borrowed string, if it is present and valid UTF-8.
    ///
    /// A model id that is not valid UTF-8 cannot be a model id the operator configured, so `None`
    /// is the right answer rather than a lossy conversion that might accidentally collide with a
    /// real name.
    #[inline]
    pub fn model_str<'a>(&self, body: &'a [u8]) -> Option<&'a str> {
        self.model
            .and_then(|r| std::str::from_utf8(r.slice(body)).ok())
    }

    /// Whether the client asked for a streamed response.
    #[inline]
    pub fn wants_stream(&self) -> bool {
        self.stream.unwrap_or(false)
    }
}

/// Scan only for the fields the router needs, in one pass.
///
/// This is the function the relay actually calls. It exists as a single pass rather than three
/// calls to [`find_string`] because the parse state — depth and string boundaries — is the expensive
/// part, and re-establishing it three times would triple the cost for no benefit.
pub fn scan_request_head(body: &[u8]) -> RequestHead {
    let mut out = RequestHead::default();
    let mut scanner = Scanner::new(body);

    // A request body is an object at the top level. Anything else — an array, a scalar, nothing —
    // has no fields to find, and is left for the upstream to complain about.
    if !scanner.enter_top_level_object() {
        return out;
    }

    while let Some(key) = scanner.next_key() {
        let key = &body[key.start..key.end];
        // The three keys are compared before the value is decoded, so an unrelated key costs a
        // length check and a short byte compare rather than any parsing at all.
        match key {
            b"model" => {
                if out.model.is_none() {
                    // Only the first occurrence is recorded, matching `serde_json` and therefore
                    // matching how the upstream will interpret the same body.
                    out.model = scanner.skip_value_and_get_string_range();
                } else {
                    scanner.skip_value();
                }
            }
            b"stream" => {
                if out.stream.is_none() {
                    out.stream = scanner.skip_value_and_get_bool();
                } else {
                    scanner.skip_value();
                }
            }
            b"n" => {
                if out.n.is_none() {
                    out.n = scanner.skip_value_and_get_u64();
                } else {
                    scanner.skip_value();
                }
            }
            _ => scanner.skip_value(),
        }
    }

    out
}

/// Find a top-level string field in a JSON object body.
///
/// Returns the byte range of the **decoded** value only when no escape sequences are present, and
/// `None` when the value contains escapes — because decoding would require an allocation or a
/// borrowed-to-owned transition that this API does not have room for. Use [`find_string_unescaped`]
/// when an escaped value must be handled, or fall back to a real parser.
pub fn find_string(body: &[u8], key: &str) -> Option<Range> {
    let mut scanner = Scanner::new(body);
    if !scanner.enter_top_level_object() {
        return None;
    }
    while let Some(found) = scanner.next_key() {
        if &body[found.start..found.end] == key.as_bytes() {
            return scanner.skip_value_and_get_string_range();
        }
        scanner.skip_value();
    }
    None
}

/// Find a top-level boolean field in a JSON object body.
pub fn find_bool(body: &[u8], key: &str) -> Option<bool> {
    let mut scanner = Scanner::new(body);
    if !scanner.enter_top_level_object() {
        return None;
    }
    while let Some(found) = scanner.next_key() {
        if &body[found.start..found.end] == key.as_bytes() {
            return scanner.skip_value_and_get_bool();
        }
        scanner.skip_value();
    }
    None
}

/// Find a top-level unsigned integer field in a JSON object body.
pub fn find_u64(body: &[u8], key: &str) -> Option<u64> {
    let mut scanner = Scanner::new(body);
    if !scanner.enter_top_level_object() {
        return None;
    }
    while let Some(found) = scanner.next_key() {
        if &body[found.start..found.end] == key.as_bytes() {
            return scanner.skip_value_and_get_u64();
        }
        scanner.skip_value();
    }
    None
}

/// Like [`find_string`], but decodes JSON escape sequences into `out`.
///
/// Returns `false` when the value is absent or is not a string. Used for the rare fields that are
/// genuinely needed rather than merely routed on.
pub fn find_string_unescaped(body: &[u8], key: &str, out: &mut String) -> bool {
    out.clear();
    let mut scanner = Scanner::new(body);
    if !scanner.enter_top_level_object() {
        return false;
    }
    while let Some(found) = scanner.next_key() {
        if &body[found.start..found.end] == key.as_bytes() {
            return scanner.read_value_into_string(out);
        }
        scanner.skip_value();
    }
    false
}

/// A depth-aware, allocation-free cursor over a JSON document.
///
/// It is deliberately not a general-purpose parser: it must be able to *skip* any value correctly
/// (including nested objects and arrays, all string and number forms, and the three literals) while
/// only being able to *read* the handful of scalar forms the router needs.
#[derive(Debug, Clone)]
struct Scanner<'a> {
    body: &'a [u8],
    pos: usize,
}

/// What went wrong, for the internal fallible paths. Not exposed publicly because every public
/// entry point converts a failure into `None`: a body the scanner cannot understand is not an error
/// the router should raise, it is a body the router should forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanError {
    /// Ran off the end of the input.
    Eof,
    /// Found a byte that cannot appear where it appeared.
    Unexpected,
}

impl<'a> Scanner<'a> {
    #[inline]
    fn new(body: &'a [u8]) -> Self {
        Self { body, pos: 0 }
    }

    /// The maximum nesting depth the scanner will walk.
    ///
    /// A limit exists so that a hostile body cannot make the scanner's own stack usage unbounded.
    /// The scanner is iterative rather than recursive, so this is a bound on a counter rather than
    /// on stack frames — but a body nested deeper than this is not a request any provider would
    /// accept, and refusing early is cheaper than walking it.
    const MAX_DEPTH: usize = 64;

    #[inline]
    fn peek(&self) -> Option<u8> {
        self.body.get(self.pos).copied()
    }

    /// Skip insignificant whitespace.
    ///
    /// The four bytes JSON allows are checked with a `matches!` rather than a table lookup; at this
    /// size the branch predictor handles it better than a memory access would.
    #[inline]
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Position the scanner just after the opening brace of the top-level object.
    ///
    /// Returns `false` if the body is not an object, which is not an error: the router simply has
    /// nothing to route on, and the upstream will produce the real error message.
    fn enter_top_level_object(&mut self) -> bool {
        self.skip_ws();
        if self.peek() != Some(b'{') {
            return false;
        }
        self.pos += 1;
        true
    }

    /// Advance to the next key in the current object, returning its byte range.
    ///
    /// The returned range excludes the surrounding quotes. Returns `None` at the closing brace or
    /// on any malformed input, which the caller treats identically: no more keys.
    fn next_key(&mut self) -> Option<Range> {
        self.skip_ws();
        match self.peek() {
            // End of the object.
            Some(b'}') => {
                self.pos += 1;
                None
            }
            Some(b',') => {
                self.pos += 1;
                self.skip_ws();
                self.read_key_range()
            }
            // The first key, immediately after the opening brace.
            _ => self.read_key_range(),
        }
    }

    /// Read a quoted string as a key and leave the cursor before its colon.
    fn read_key_range(&mut self) -> Option<Range> {
        let (range, _had_escapes) = self.read_raw_string().ok()?;
        self.skip_ws();
        // A key with no colon is not a key. Stopping here rather than scanning on means a
        // malformed body costs a constant amount of work instead of a full pass.
        if self.peek() == Some(b':') {
            self.pos += 1;
            Some(range)
        } else {
            None
        }
    }

    /// Read a quoted string, returning the raw (still escaped) inner range.
    ///
    /// The scan itself is the hot loop of this module. It is written around `memchr2` — which finds
    /// the earlier of a quote or a backslash — so that a long string is skipped at memory bandwidth
    /// rather than one byte per iteration. Numbers and literals are handled by their own
    /// single-character terminators.
    fn read_raw_string(&mut self) -> Result<(Range, bool), ScanError> {
        self.skip_ws();
        if self.peek() != Some(b'"') {
            return Err(if self.pos >= self.body.len() {
                ScanError::Eof
            } else {
                ScanError::Unexpected
            });
        }
        self.pos += 1;
        let start = self.pos;
        let mut had_escapes = false;
        loop {
            let rest = &self.body[self.pos..];
            if rest.is_empty() {
                return Err(ScanError::Eof);
            }
            // Find the next quote or backslash; everything before it is ordinary string content.
            match memchr2(b'"', b'\\', rest) {
                Some(offset) => {
                    let at = self.pos + offset;
                    if self.body[at] == b'"' {
                        let range = Range { start, end: at };
                        self.pos = at + 1;
                        return Ok((range, had_escapes));
                    }
                    // A backslash escapes the next byte. `\uXXXX` is six bytes but only the
                    // immediately-following byte matters for finding the closing quote, because the
                    // remaining four are hex digits and can never be a quote in any encoding the
                    // scanner cares about.
                    had_escapes = true;
                    self.pos = at + 2;
                    if self.pos > self.body.len() {
                        return Err(ScanError::Eof);
                    }
                }
                None => return Err(ScanError::Eof),
            }
        }
    }

    /// Read the value after a key, returning it only if it is a string with no escapes.
    fn skip_value_and_get_string_range(&mut self) -> Option<Range> {
        self.skip_ws();
        if self.peek() != Some(b'"') {
            self.skip_value();
            return None;
        }
        match self.read_raw_string() {
            // An escaped value is reported as absent rather than decoded, because decoding needs an
            // allocation the caller did not opt into. The `_unescaped` variant exists for callers
            // that need it.
            Ok((range, false)) => Some(range),
            Ok((_, true)) => None,
            Err(_) => None,
        }
    }

    /// Read the value after a key, returning it only if it is a boolean.
    fn skip_value_and_get_bool(&mut self) -> Option<bool> {
        self.skip_ws();
        if self.body[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Some(true)
        } else if self.body[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Some(false)
        } else {
            self.skip_value();
            None
        }
    }

    /// Read the value after a key, returning it only if it is a non-negative integer.
    fn skip_value_and_get_u64(&mut self) -> Option<u64> {
        self.skip_ws();
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            self.skip_value();
            return None;
        }
        // A fraction or an exponent is not an unsigned integer. Consume the rest of the literal so
        // the scan stays aligned with the document, then report the value as absent rather than
        // returning a truncated prefix that strands the scanner on the `.` and hides later fields.
        if matches!(self.peek(), Some(b'.' | b'e' | b'E')) {
            self.skip_value();
            return None;
        }
        // `from_utf8` cannot fail here: the slice is all ASCII digits by construction.
        std::str::from_utf8(&self.body[start..self.pos])
            .ok()
            .and_then(|s| s.parse().ok())
    }

    /// Read a string value, decoding escapes into `out`.
    fn read_value_into_string(&mut self, out: &mut String) -> bool {
        self.skip_ws();
        if self.peek() != Some(b'"') {
            self.skip_value();
            return false;
        }
        let (range, had_escapes) = match self.read_raw_string() {
            Ok(v) => v,
            Err(_) => return false,
        };
        let raw = &self.body[range.start..range.end];
        if !had_escapes {
            return match std::str::from_utf8(raw) {
                Ok(s) => {
                    out.push_str(s);
                    true
                }
                Err(_) => false,
            };
        }
        let mut buf = Vec::with_capacity(raw.len());
        if decode_escapes(raw, &mut buf).is_err() {
            return false;
        }
        match String::from_utf8(buf) {
            Ok(s) => {
                out.push_str(&s);
                true
            }
            Err(_) => false,
        }
    }

    /// Skip any JSON value, leaving the cursor immediately after it.
    ///
    /// Iterative rather than recursive, with an explicit depth counter, so a deeply nested body
    /// costs a counter increment rather than a stack frame and cannot overflow the stack.
    fn skip_value(&mut self) {
        let mut depth = 0usize;
        self.skip_ws();
        loop {
            self.skip_ws();
            let Some(b) = self.peek() else { return };
            match b {
                b'{' | b'[' => {
                    depth += 1;
                    if depth > Self::MAX_DEPTH {
                        // Abandon the scan, leaving the cursor where it is. The public entry points
                        // treat an unhelpful cursor as "field not found", which is the safe answer.
                        return;
                    }
                    self.pos += 1;
                    continue;
                }
                b'}' | b']' => {
                    self.pos += 1;
                    if depth == 0 {
                        // A closing bracket at depth zero ends the enclosing object: step back so
                        // the caller's `next_key` sees it and terminates cleanly.
                        self.pos -= 1;
                        return;
                    }
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                    continue;
                }
                b'"' => {
                    if self.read_raw_string().is_err() {
                        return;
                    }
                    if depth == 0 {
                        return;
                    }
                    // Inside a container, a string is just an element; keep walking.
                    self.skip_separator_and_continue(&mut depth);
                    if depth == 0 {
                        return;
                    }
                    continue;
                }
                b',' | b':' | b' ' | b'\t' | b'\n' | b'\r' => {
                    self.pos += 1;
                    continue;
                }
                _ => {
                    // A number, `true`, `false` or `null`.
                    if self.skip_literal().is_err() {
                        return;
                    }
                    if depth == 0 {
                        return;
                    }
                    continue;
                }
            }
        }
    }

    /// After a scalar inside a container, keep walking until the container closes.
    fn skip_separator_and_continue(&mut self, depth: &mut usize) {
        loop {
            self.skip_ws();
            match self.peek() {
                None => return,
                Some(b'}') | Some(b']') => {
                    self.pos += 1;
                    if *depth == 0 {
                        self.pos -= 1;
                        return;
                    }
                    *depth -= 1;
                    if *depth == 0 {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// Consume a number or one of the three literal keywords.
    fn skip_literal(&mut self) -> Result<(), ScanError> {
        let rest = &self.body[self.pos..];
        for lit in [&b"true"[..], b"false", b"null"] {
            if rest.starts_with(lit) {
                self.pos += lit.len();
                return Ok(());
            }
        }
        // Otherwise a number. Scan the characters JSON permits in a number and require at least
        // one, so a stray byte cannot be silently swallowed.
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            Err(ScanError::Unexpected)
        } else {
            Ok(())
        }
    }
}

/// Decode JSON string escapes into `out`.
///
/// Handles the two-character escapes, `\uXXXX` including surrogate pairs, and rejects an unknown
/// escape rather than passing it through — because passing a raw backslash through would produce a
/// model id that silently does not match anything.
fn decode_escapes(raw: &[u8], out: &mut Vec<u8>) -> Result<(), ScanError> {
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&esc) = raw.get(i) else {
            return Err(ScanError::Eof);
        };
        i += 1;
        match esc {
            b'"' => out.push(b'"'),
            b'\\' => out.push(b'\\'),
            b'/' => out.push(b'/'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'u' => {
                let hi = read_hex4(raw, i)?;
                i += 4;
                // Explicitly `u32`: the surrogate-pair branch produces a code point above the BMP
                // and the single-unit branch does not, and letting inference pick `u16` would
                // silently make the pair branch unrepresentable.
                let cp: u32 = if (0xD800..0xDC00).contains(&hi) {
                    // A high surrogate must be followed by a low surrogate; anything else is
                    // malformed and is rejected rather than replaced, because a silent replacement
                    // character in a model id would be an extremely confusing bug.
                    if raw.get(i) != Some(&b'\\') || raw.get(i + 1) != Some(&b'u') {
                        return Err(ScanError::Unexpected);
                    }
                    let lo = read_hex4(raw, i + 2)?;
                    i += 6;
                    if !(0xDC00..0xE000).contains(&lo) {
                        return Err(ScanError::Unexpected);
                    }
                    0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00)
                } else {
                    hi as u32
                };
                let ch = char::from_u32(cp).ok_or(ScanError::Unexpected)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            _ => return Err(ScanError::Unexpected),
        }
    }
    Ok(())
}

/// Read four hex digits as a `u16`.
fn read_hex4(raw: &[u8], at: usize) -> Result<u16, ScanError> {
    let s = raw.get(at..at + 4).ok_or(ScanError::Eof)?;
    let mut v: u16 = 0;
    for &c in s {
        let d = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return Err(ScanError::Unexpected),
        };
        v = (v << 4) | d as u16;
    }
    Ok(v)
}

/// Whether a body plausibly needs the `model` field at all.
///
/// A cheap pre-check for the relay: the overwhelming majority of bodies contain `model` within the
/// first bytes, and a body with no `m` byte anywhere cannot contain the key. Avoiding the scanner
/// setup for such a body is a micro-optimisation, but it is a free one.
#[inline]
pub fn may_contain_model(body: &[u8]) -> bool {
    memchr(b'm', body).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_of(body: &[u8]) -> Option<String> {
        scan_request_head(body).model_str(body).map(str::to_owned)
    }

    #[test]
    fn finds_a_model_in_a_minimal_body() {
        let body = br#"{"model":"gpt-4o"}"#;
        assert_eq!(model_of(body).as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn finds_a_model_among_many_fields() {
        let body = br#"{"messages":[{"role":"user","content":"hi"}],"temperature":0.7,"model":"claude-sonnet-4-5","stream":true}"#;
        let head = scan_request_head(body);
        assert_eq!(head.model_str(body), Some("claude-sonnet-4-5"));
        assert_eq!(head.stream, Some(true));
        assert!(head.wants_stream());
    }

    #[test]
    fn tolerates_arbitrary_whitespace_and_key_order() {
        let body = b"{\n\t\"stream\" : false ,\r\n \"model\"\t:\n\"m-1\"\n}";
        let head = scan_request_head(body);
        assert_eq!(head.model_str(body), Some("m-1"));
        assert_eq!(head.stream, Some(false));
        assert!(!head.wants_stream());
    }

    #[test]
    fn a_model_inside_a_string_value_is_not_matched() {
        // This is the failure a naive `memchr(b"\"model\"")` search exhibits, and it is a
        // realistic input: a conversation that talks about JSON, or a tool schema with a `model`
        // property.
        let body = br#"{"messages":[{"role":"user","content":"what does \"model\":\"x\" mean in this payload?"}]}"#;
        assert_eq!(model_of(body), None);
    }

    #[test]
    fn a_model_inside_a_nested_object_is_not_matched() {
        // The top-level object is the one the upstream reads, so a nested `model` — a tool
        // definition, a cached message, a provider-specific block — must not win.
        let body = br#"{"tools":[{"type":"function","function":{"name":"f","parameters":{"model":"nested"}}}]}"#;
        assert_eq!(model_of(body), None);
    }

    #[test]
    fn a_nested_model_does_not_shadow_a_top_level_one() {
        // Either order must still find the real one — this catches a depth counter that is not
        // decremented correctly.
        let body = br#"{"tools":[{"a":{"model":"nested"}}],"model":"real"}"#;
        assert_eq!(model_of(body).as_deref(), Some("real"));

        let body = br#"{"model":"real","tools":[{"a":{"model":"nested"}}]}"#;
        assert_eq!(model_of(body).as_deref(), Some("real"));
    }

    #[test]
    fn the_first_occurrence_wins_matching_a_real_json_parser() {
        // Duplicate keys are legal JSON; `serde_json` keeps the last when building a map but the
        // streaming deserialiser most providers use sees the first. The relay must agree with the
        // body as the upstream executes it, and forwarding is verbatim, so nothing is lost either
        // way — but the choice must be deliberate and tested.
        let body = br#"{"model":"first","model":"second"}"#;
        assert_eq!(model_of(body).as_deref(), Some("first"));
    }

    #[test]
    fn an_escaped_model_value_is_reported_as_absent_rather_than_wrong() {
        // The router must never act on a partially-decoded model name. Absent is the safe answer;
        // the unescaped variant exists for callers that need the real value.
        let body = br#"{"model":"a\u0041b"}"#;
        assert_eq!(model_of(body), None);

        let mut out = String::new();
        assert!(find_string_unescaped(body, MODEL_KEY, &mut out));
        assert_eq!(out, "aAb");
    }

    #[test]
    fn escapes_in_other_fields_do_not_derail_the_scan() {
        // A long escaped tool result before the model field must not break string tracking.
        let body =
            br#"{"messages":[{"content":"line1\nline2\ttabbed \"quoted\""}],"model":"after"}"#;
        assert_eq!(model_of(body).as_deref(), Some("after"));
    }

    #[test]
    fn a_trailing_backslash_at_the_end_of_input_does_not_panic() {
        let body = br#"{"model":"abc\"#;
        assert_eq!(model_of(body), None);

        let body = br#"{"model":"\"#;
        let _ = model_of(body);

        let body = br#"{"a":"\"}"#;
        let _ = model_of(body);
    }

    #[test]
    fn unterminated_input_returns_no_model_rather_than_panicking() {
        for body in [
            &br#"{"model"#[..],
            &br#"{"model:"#[..],
            &br#"{"model":"#[..],
            &br#"{"model":"x"#[..],
            &br#"{"#[..],
            &br#""#[..],
            &br#"{"model":"x","#[..],
            &br#"{"model":"x", "stream""#[..],
        ] {
            let _ = scan_request_head(body);
        }
    }

    #[test]
    fn a_non_object_body_has_no_fields() {
        for body in [
            &br#"[]"#[..],
            &br#""str""#[..],
            &br#"42"#[..],
            &br#"null"#[..],
        ] {
            let head = scan_request_head(body);
            assert!(head.model.is_none(), "unexpected model in {body:?}");
            assert!(head.stream.is_none());
        }
    }

    #[test]
    fn non_string_and_non_boolean_values_are_reported_as_absent() {
        let body = br#"{"model":123,"stream":"yes"}"#;
        let head = scan_request_head(body);
        assert!(head.model.is_none());
        assert!(head.stream.is_none());
    }

    #[test]
    fn stream_defaults_to_false_when_absent_or_not_a_boolean() {
        assert!(!scan_request_head(br#"{"model":"m"}"#).wants_stream());
        assert!(scan_request_head(br#"{"stream":true}"#).wants_stream());
        assert!(!scan_request_head(br#"{"stream":false}"#).wants_stream());
        assert!(!scan_request_head(br#"{"stream":null}"#).wants_stream());
    }

    #[test]
    fn n_is_read_for_quota_accounting_and_ignores_non_integers() {
        assert_eq!(scan_request_head(br#"{"n":3}"#).n, Some(3));
        assert_eq!(scan_request_head(br#"{"n":0}"#).n, Some(0));
        assert_eq!(scan_request_head(br#"{"n":-1}"#).n, None);
        assert_eq!(scan_request_head(br#"{"n":1.5}"#).n, None);
        assert_eq!(scan_request_head(br#"{"n":"2"}"#).n, None);
    }

    #[test]
    fn deeply_nested_arrays_and_objects_are_skipped_without_recursion() {
        // A hostile body must not be able to exhaust the stack. The scanner is iterative and
        // bounded, so this must simply succeed or give up — never crash.
        let mut body = Vec::from(&b"{\"model\":\"deep\",\"x\":"[..]);
        body.resize(body.len() + 200, b'[');
        body.resize(body.len() + 200, b']');
        body.push(b'}');
        assert_eq!(model_of(&body).as_deref(), Some("deep"));
    }

    #[test]
    fn a_realistic_agent_request_is_scanned_correctly() {
        // The shape that actually matters: a long tool-heavy conversation with the model field not
        // first, and strings containing JSON-like text.
        let body = br#"{
            "messages": [
                {"role": "system", "content": "You are a helpful agent. Respond with {\"model\": \"never\"} if asked."},
                {"role": "user", "content": "Read /tmp/x"},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"/tmp/x\"}"}}
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "file contents"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}],
            "model": "cx/gpt-6-astra",
            "stream": true,
            "max_tokens": 4096
        }"#;
        let head = scan_request_head(body);
        assert_eq!(head.model_str(body), Some("cx/gpt-6-astra"));
        assert!(head.wants_stream());
    }

    #[test]
    fn a_unicode_model_id_survives_when_not_escaped() {
        let body = "{\"model\":\"模型-一\"}".as_bytes();
        assert_eq!(model_of(body).as_deref(), Some("模型-一"));
    }

    #[test]
    fn surrogate_pairs_decode_correctly() {
        // U+1F600, which requires a surrogate pair in `\u` form.
        let body = br#"{"model":"a\ud83d\ude00b"}"#;
        let mut out = String::new();
        assert!(find_string_unescaped(body, MODEL_KEY, &mut out));
        assert_eq!(out, "a\u{1F600}b");
    }

    #[test]
    fn a_lone_high_surrogate_is_rejected_rather_than_replaced() {
        // A replacement character in a model id would be an extremely confusing bug, so a broken
        // escape is reported as absent instead.
        let body = br#"{"model":"a\ud83db"}"#;
        let mut out = String::new();
        assert!(!find_string_unescaped(body, MODEL_KEY, &mut out));
    }

    #[test]
    fn an_unknown_escape_is_rejected() {
        let body = br#"{"model":"a\qb"}"#;
        let mut out = String::new();
        assert!(!find_string_unescaped(body, MODEL_KEY, &mut out));
    }

    #[test]
    fn find_string_handles_literals_numbers_and_null_in_other_fields() {
        let body = br#"{"a":null,"b":true,"c":-1.5e10,"d":[1,2,{"e":"f"}],"model":"m","z":{}}"#;
        assert_eq!(
            find_string(body, MODEL_KEY).map(|r| r.slice(body)),
            Some(&b"m"[..])
        );
        assert_eq!(find_bool(body, "b"), Some(true));
        assert_eq!(find_u64(body, "c"), None);
    }

    #[test]
    fn find_string_misses_cleanly_when_the_key_is_absent() {
        let body = br#"{"a":"b"}"#;
        assert!(find_string(body, MODEL_KEY).is_none());
        assert!(find_bool(body, "stream").is_none());
        assert!(find_u64(body, "n").is_none());
    }

    #[test]
    fn invalid_utf8_in_the_body_does_not_panic() {
        let body = b"{\"model\":\"\xff\xfe\"}";
        // Reported as absent because it cannot be a configured model id, but never a panic.
        assert_eq!(model_of(body), None);
    }

    #[test]
    fn the_range_returned_borrows_from_the_original_body() {
        // Zero-copy is the point: no allocation must occur on this path.
        let body = br#"{"model":"gpt-4o"}"#;
        let r = find_string(body, MODEL_KEY).unwrap();
        assert_eq!(r.slice(body), b"gpt-4o");
        assert!(std::ptr::eq(
            r.slice(body).as_ptr(),
            body[r.start..].as_ptr()
        ));
    }

    #[test]
    fn a_body_with_no_m_byte_is_rejected_by_the_cheap_precheck() {
        assert!(!may_contain_model(br#"{"a":"b"}"#));
        assert!(may_contain_model(br#"{"model":"m"}"#));
    }

    #[test]
    fn empty_and_degenerate_bodies_are_handled() {
        assert_eq!(model_of(b""), None);
        assert_eq!(model_of(b"{}"), None);
        assert_eq!(model_of(b"   "), None);
        assert_eq!(model_of(b"{}}"), None);
    }

    #[test]
    fn a_body_of_only_commas_and_colons_terminates() {
        // Fuzz-ish: no crash on structurally nonsensical input.
        for body in [
            &b",,,"[..],
            b"::::",
            b"{{{{",
            b"}}}}",
            b"\"\"\"\"",
            b"\\\\\\\\",
        ] {
            let _ = scan_request_head(body);
            let _ = find_string(body, MODEL_KEY);
            let _ = find_bool(body, STREAM_KEY);
        }
    }

    #[test]
    fn scanning_is_deterministic_across_repeated_calls() {
        let body = br#"{"model":"m","stream":true,"n":2}"#;
        let a = scan_request_head(body);
        for _ in 0..50 {
            assert_eq!(scan_request_head(body), a);
        }
    }

    #[test]
    fn a_model_field_appearing_after_a_large_payload_is_still_found() {
        // The scanner must not have a hidden length limit; a big conversation is the normal case
        // for an agentic client.
        let mut body = Vec::with_capacity(300_000);
        body.extend_from_slice(br#"{"messages":[{"role":"user","content":""#);
        body.extend(std::iter::repeat_n(b'x', 200_000));
        body.extend_from_slice(br#""}],"model":"big-model"}"#);
        assert_eq!(model_of(&body).as_deref(), Some("big-model"));
    }
}
