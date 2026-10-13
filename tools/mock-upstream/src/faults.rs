//! The fault model: everything the mock can be told to do wrong, written down as data.
//!
//! ## Why faults are data and not code
//!
//! A fake upstream whose misbehaviour is programmed with closures can only be misconfigured from
//! the process that started it. [`Fault`] is plain serialisable data so that the *same* bytes
//! configure the mock everywhere it can be configured: in-process (`MockUpstream::start_with`),
//! once for the whole server over the wire (`POST /__control`), and once for a single request
//! (`x-mock-fault: <json>`). A bug report therefore contains a string you can paste into `curl`
//! and get the same misbehaviour back, on any machine, forever.
//!
//! ## Why determinism beats realism here
//!
//! Real providers are a mess of jitter: a 429 arrives after a random backoff, a stream dies after
//! a random number of chunks, ids are UUIDv7 and `created` is wall-clock time. None of that is
//! useful for testing a router, and all of it is harmful. Manarouter's headline behaviour is a
//! hedged race whose winner is decided by *time*; if the upstream's own latency were random, a
//! failing race test would be indistinguishable from a flaky upstream. If response bodies carried
//! timestamps or random ids, "did the relay forward the right answer byte-for-byte" would be an
//! unanswerable question. The mock therefore generates fixed ids, a fixed `created` and fixed
//! content: the only variable a test controls is the fault it asked for. Realism is what the
//! staging environment is for; this crate is for causality.
//!
//! ## How overrides merge
//!
//! A [`Fault`] is every knob at once; a [`FaultPatch`] is any subset of them. The server default
//! is a `Fault`, the per-request header and the `POST /__control` body are `FaultPatch`es that
//! merge over it field by field, so a test can change `delay_ms` for one request without
//! restating (or accidentally clearing) the rest of the configuration. The merge is deliberately
//! shallow and lossy-free: a field is either present in the patch (it wins) or absent (the base
//! survives).
//!
//! ## Why three error shapes live side by side
//!
//! The relay exists to translate between dialects, so its failure-path tests must be able to feed
//! it *each* dialect's error document. OpenAI speaks `{"error": {...}}`, Anthropic speaks
//! `{"type":"error","error": {...}}`, and the Command Code bridge speaks
//! `{"success":false,"error": {...}}`. [`error_envelope`] builds all three from one description of
//! the failure, which keeps the retry-after contract (`retry_after` + a machine-readable
//! `reset_at`, as observed from a real provider) identical across dialects even though the
//! envelope differs.
//!
//! ## Where each timing lands, and why the split matters
//!
//! - `delay_ms` sleeps *before the response exists*, so the status line and headers are late.
//!   This is a model that is slow to accept or slow to start.
//! - `ttfb_ms` sleeps before the first byte of the response *body*; the headers are already on
//!   the wire. The hedged race's default winner metric is `first_response_byte` (the first byte
//!   of the body), so this knob alone moves the number the race is decided on.
//! - `slow_first_token_ms` additionally delays the first *content* delta inside an open stream,
//!   after the opening frames. A model that opens a stream and then thinks is qualitatively
//!   different from one that never answers, and `first_complete_token` hedging needs to tell
//!   them apart.
//! - `chunk_delay_ms` delays every subsequent chunk, i.e. slow generation rather than slow
//!   thinking.
//!
//! The four are additive and independent so a test can produce both halves of that distinction
//! (`ttfb_ms = 0, slow_first_token_ms = 800` vs `ttfb_ms = 800`) without touching anything else.

use serde::{Deserialize, Serialize};

/// Which error document a forced failure is dressed in.
///
/// The default [`ErrorBody::Auto`] is what makes one server able to serve several dialects: the
/// envelope follows the endpoint that was called, so a test does not have to remember that
/// `/v1/messages` wants `{"type":"error", ...}` while `/v1/chat/completions` wants
/// `{"error": ...}`. Naming a dialect explicitly overrides that, which is how a test asserts the
/// relay's *decoding* of a shape that does not belong to the endpoint in front of it — including
/// the Command Code shape, which has no endpoint of its own here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorBody {
    /// Follow the endpoint's own dialect; an error status then carries that dialect's envelope.
    #[default]
    Auto,
    /// `{"error": {"message": ..., "type": ..., "code": ..., "retry_after": ..., "reset_at": ...}}`
    Openai,
    /// `{"type": "error", "error": {"type": ..., "message": ..., "retry_after": ..., "reset_at": ...}}`
    Anthropic,
    /// `{"success": false, "error": {"message": ..., "retry_after": ..., "reset_at": ...}}`
    CommandCode,
    /// Force the status but send an empty body, for clients that mishandle error documents.
    Empty,
}

impl ErrorBody {
    /// Collapse [`ErrorBody::Auto`] onto a concrete dialect.
    ///
    /// `Auto` reaching this point means the caller had no endpoint preference either, so the
    /// OpenAI envelope is the fallback: it is the widest-interoperability shape and the one the
    /// relay's own pivot speaks.
    pub fn resolve(self) -> ErrorBody {
        match self {
            ErrorBody::Auto => ErrorBody::Openai,
            other => other,
        }
    }
}

/// The complete fault configuration: every knob, always present, with a [`Default`].
///
/// The `Default` is the healthy upstream — 200s, no delays, a fixed answer — because the common
/// case in a test suite is "this participant behaves", and because a config file that omits a
/// field must mean *no fault*, never *unspecified behaviour*. See the module docs for where each
/// timing lands.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Fault {
    /// Sleep this long before the response (status line and headers) is produced, in
    /// milliseconds. Simulates a slow model or a slow connection; `0` (the default) responds
    /// immediately.
    pub delay_ms: u64,
    /// Sleep this long before the first byte of the response *body*, in milliseconds, while the
    /// headers go out immediately. This is the time-to-first-byte the hedged race measures when
    /// `winner_metric` is `first_response_byte`, so it is the direct input to the race's window
    /// logic. `0` sends the body as soon as it is ready.
    pub ttfb_ms: u64,
    /// Sleep this long before the first *content* delta of a stream, in milliseconds, after the
    /// opening frames have already been sent. A long value here models a model that is thinking
    /// (stream open, nothing yet) rather than one that is broken (nothing at all), which is
    /// exactly the distinction `first_complete_token` hedging exists to make. On a
    /// non-streaming response there is no token boundary to hide behind, so this degrades to an
    /// extra delay before the body.
    pub slow_first_token_ms: u64,
    /// Sleep this long between streamed chunks, in milliseconds. Simulates slow generation once
    /// the stream is flowing, as opposed to slow thinking before it starts.
    pub chunk_delay_ms: u64,
    /// Force this HTTP status instead of the endpoint's normal one. `0` means "use the
    /// endpoint's normal status". Any status >= 400 turns the body into a provider-shaped error
    /// document (see [`Fault::error_body`]).
    pub status: u16,
    /// When an error is being forced, emit `retry-after: <n>` and put `retry_after` plus a
    /// machine-readable `reset_at` into the error body. `0` omits all three — a 429 without a
    /// retry hint is a genuinely different contract from one with it, so it must be selectable.
    pub retry_after_secs: u64,
    /// Which error envelope to use; [`ErrorBody::Auto`] (the default) follows the endpoint's
    /// dialect. Any other value forces that envelope, and if `status` is still `0` the status
    /// becomes 500, because an error document with a 200 would be a contradiction a client
    /// should never have to model.
    pub error_body: ErrorBody,
    /// In a streaming response, emit exactly this many SSE frames and then close the socket
    /// **without** a terminator (`[DONE]`, `message_stop`, `response.completed` or the final
    /// zero-length chunk). This is the mid-answer death that must never be silently hidden by a
    /// well-mannerled server, and the only way to test it is to actually kill the connection.
    /// `0` disables the fault.
    pub fail_after_chunks: usize,
    /// In a non-streaming response, declare the full `content-length`, send only this many bytes
    /// of the body and then close the socket. The client sees a truncated document cut short of
    /// the length it was promised, which is how a connection dies mid-body in the wild. `0`
    /// disables the fault.
    pub fail_after_bytes: usize,
    /// Send a 200 whose body is syntactically invalid JSON. In a stream, the first delta frame's
    /// payload is the malformed one while framing stays intact — providers really do emit a
    /// commentary line where JSON belongs, and the relay's frame parser is expected to skip it
    /// rather than abort. Pairs with `status`: if a status is forced, the error document wins.
    pub malformed_json: bool,
    /// The exact text to answer with, so a test can assert it received the right answer and
    /// therefore identify which participant won a race. `None` uses the crate's fixed default.
    /// Changing this changes response bytes, which is the point: identity is the assertion.
    pub content: Option<String>,
    /// Answer with a tool call instead of text, shaped for the dialect being served: Chat
    /// Completions `tool_calls[].function.arguments` as a JSON *string*, Anthropic `tool_use`
    /// with an `input` *object*, Responses `function_call` with a JSON-string `arguments`.
    pub include_tool_call: bool,
    /// Include token usage. Real providers disagree about where it goes in a stream — OpenAI
    /// parks it in a final empty-choices chunk before `[DONE]`, Anthropic puts input tokens in
    /// `message_start` and output tokens in `message_delta`, Responses carries it on
    /// `response.completed` — so the mock puts it where each dialect's client expects it and
    /// nowhere else.
    pub include_usage: bool,
}

impl Fault {
    /// Merge `patch` over `self`: fields present in the patch win, fields absent from it survive.
    ///
    /// This is the one merge rule in the crate, shared by the control endpoint and the
    /// per-request header, so that a test which learns to reconfigure the server already knows
    /// how to reconfigure a single request.
    pub fn merge(&self, patch: &FaultPatch) -> Fault {
        Fault {
            delay_ms: patch.delay_ms.unwrap_or(self.delay_ms),
            ttfb_ms: patch.ttfb_ms.unwrap_or(self.ttfb_ms),
            slow_first_token_ms: patch
                .slow_first_token_ms
                .unwrap_or(self.slow_first_token_ms),
            chunk_delay_ms: patch.chunk_delay_ms.unwrap_or(self.chunk_delay_ms),
            status: patch.status.unwrap_or(self.status),
            retry_after_secs: patch.retry_after_secs.unwrap_or(self.retry_after_secs),
            error_body: patch.error_body.unwrap_or(self.error_body),
            fail_after_chunks: patch
                .fail_after_chunks
                .unwrap_or(self.fail_after_chunks),
            fail_after_bytes: patch.fail_after_bytes.unwrap_or(self.fail_after_bytes),
            malformed_json: patch.malformed_json.unwrap_or(self.malformed_json),
            content: patch.content.clone().or_else(|| self.content.clone()),
            include_tool_call: patch.include_tool_call.unwrap_or(self.include_tool_call),
            include_usage: patch.include_usage.unwrap_or(self.include_usage),
        }
    }

    /// Whether this fault forces a failure response instead of the endpoint's normal body.
    ///
    /// True when a status >= 400 was demanded, or when an explicit envelope was demanded — an
    /// envelope without a status still implies a failure (the status becomes 500, as documented
    /// on [`Fault::error_body`]), because choosing an error shape is itself the statement "this
    /// request failed". The one exception is an explicit status below 400: a caller who asks for
    /// `status: 200` keeps the 200 and simply gets whatever envelope was requested.
    pub fn forces_error(&self) -> bool {
        self.status >= 400 || self.error_body != ErrorBody::Auto
    }

    /// The status a forced failure uses, once [`Fault::forces_error`] has decided there is one.
    ///
    /// A bare `error_body` with no `status` becomes 500: "here is an error document" with no
    /// other information means the generic server-error case.
    pub fn effective_status(&self) -> u16 {
        if self.status != 0 {
            self.status
        } else if self.forces_error() {
            500
        } else {
            200
        }
    }

    /// The text this fault answers with, falling back to the crate default.
    pub fn content_or_default(&self) -> &str {
        self.content.as_deref().unwrap_or(crate::DEFAULT_CONTENT)
    }
}

/// Any subset of [`Fault`]; the patch language for `POST /__control` and `x-mock-fault`.
///
/// Every field is `Option` and missing fields leave the base untouched, so `{"status": 500}`
/// changes exactly one thing. Note the one asymmetry: `content` can be *set* by a patch but not
/// *cleared* (there is no "unset" in JSON once the key is present), because `null` and a missing
/// key are indistinguishable to `serde` here. Clearing is a whole-`Fault` operation
/// (`MockUpstream::set_fault`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FaultPatch {
    /// Override for [`Fault::delay_ms`].
    pub delay_ms: Option<u64>,
    /// Override for [`Fault::ttfb_ms`].
    pub ttfb_ms: Option<u64>,
    /// Override for [`Fault::slow_first_token_ms`].
    pub slow_first_token_ms: Option<u64>,
    /// Override for [`Fault::chunk_delay_ms`].
    pub chunk_delay_ms: Option<u64>,
    /// Override for [`Fault::status`].
    pub status: Option<u16>,
    /// Override for [`Fault::retry_after_secs`].
    pub retry_after_secs: Option<u64>,
    /// Override for [`Fault::error_body`].
    pub error_body: Option<ErrorBody>,
    /// Override for [`Fault::fail_after_chunks`].
    pub fail_after_chunks: Option<usize>,
    /// Override for [`Fault::fail_after_bytes`].
    pub fail_after_bytes: Option<usize>,
    /// Override for [`Fault::malformed_json`].
    pub malformed_json: Option<bool>,
    /// Override for [`Fault::content`].
    pub content: Option<String>,
    /// Override for [`Fault::include_tool_call`].
    pub include_tool_call: Option<bool>,
    /// Override for [`Fault::include_usage`].
    pub include_usage: Option<bool>,
}

/// Fixed instant every `reset_at` is measured from: `2026-01-01T00:00:00Z`.
///
/// A wall-clock `reset_at` would make two runs of the same test produce different bytes. This
/// one is a constant epoch instead, and `reset_at = epoch + retry_after_secs`, so the value is
/// still internally consistent (a client can check that `reset_at` is `retry_after` seconds
/// after the reference instant) while remaining byte-for-byte reproducible. The date is recent
/// enough to read as plausible and fixed forever by contract, not by chance.
const RESET_EPOCH_SECS: u64 = 1_767_225_600;

/// `retry_after` seconds after [`RESET_EPOCH_SECS`], as an RFC 3339 UTC timestamp with no
/// fractional seconds (e.g. `retry_after_secs = 300` → `2026-01-01T00:05:00Z`).
///
/// Pure arithmetic on a constant: no clock is consulted, so the string is identical on every
/// run and on every machine. See [`RESET_EPOCH_SECS`] for why the mock refuses to use real time
/// here.
pub fn reset_at(retry_after_secs: u64) -> String {
    let secs = RESET_EPOCH_SECS + retry_after_secs;
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Days-since-1970 to a civil (year, month, day), using the usual shift-and-correct arithmetic
/// so no date table is needed. Kept tiny and total: it cannot panic for any `i64` input the
/// caller can reach through [`reset_at`].
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01 so leap days land at the end of the cycle.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // year of era
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month index, 0 = March
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Build the error document for `style` out of one description of the failure.
///
/// `message` is the human part (a test may assert on it), the `type`/`code` pair is derived from
/// the status because clients switch on it (429 must read as rate limiting in every dialect),
/// and `retry_after`/`reset_at` are included exactly when a retry hint exists, matching the
/// provider contract described in the module docs. [`ErrorBody::Auto`] resolves to the OpenAI
/// envelope; callers resolve it to the endpoint's dialect first when that matters.
pub fn error_envelope(style: ErrorBody, status: u16, message: &str, retry_after_secs: u64) -> String {
    let retry_hint = retry_after_secs > 0;
    let reset = reset_at(retry_after_secs);
    match style.resolve() {
        ErrorBody::Anthropic => {
            let kind = match status {
                429 => "rate_limit_error",
                s if (400..500).contains(&s) => "invalid_request_error",
                _ => "api_error",
            };
            let mut err = serde_json::Map::new();
            err.insert("type".into(), kind.into());
            err.insert("message".into(), message.into());
            if retry_hint {
                err.insert("retry_after".into(), retry_after_secs.into());
                err.insert("reset_at".into(), reset.as_str().into());
            }
            serde_json::json!({"type": "error", "error": serde_json::Value::Object(err)}).to_string()
        }
        ErrorBody::CommandCode => {
            let mut err = serde_json::Map::new();
            err.insert("message".into(), message.into());
            if retry_hint {
                err.insert("retry_after".into(), retry_after_secs.into());
                err.insert("reset_at".into(), reset.as_str().into());
            }
            serde_json::json!({"success": false, "error": serde_json::Value::Object(err)}).to_string()
        }
        ErrorBody::Empty => String::new(),
        // Auto has already resolved; this arm is only reachable if a future variant forgets to.
        ErrorBody::Openai | ErrorBody::Auto => {
            let kind = match status {
                429 => "rate_limit_error",
                s if (400..500).contains(&s) => "invalid_request_error",
                _ => "server_error",
            };
            let code = if status == 429 { "rate_limit_exceeded" } else { "mock_error" };
            let mut err = serde_json::Map::new();
            err.insert("message".into(), message.into());
            err.insert("type".into(), kind.into());
            err.insert("param".into(), serde_json::Value::Null);
            err.insert("code".into(), code.into());
            if retry_hint {
                err.insert("retry_after".into(), retry_after_secs.into());
                err.insert("reset_at".into(), reset.as_str().into());
            }
            serde_json::json!({"error": serde_json::Value::Object(err)}).to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_takes_patch_fields_and_keeps_the_rest() {
        let base = Fault {
            delay_ms: 10,
            status: 429,
            content: Some("base".into()),
            ..Fault::default()
        };
        let merged = base.merge(&FaultPatch {
            ttfb_ms: Some(50),
            ..FaultPatch::default()
        });
        assert_eq!(merged.delay_ms, 10, "absent patch fields must survive");
        assert_eq!(merged.status, 429);
        assert_eq!(merged.ttfb_ms, 50);
        assert_eq!(merged.content.as_deref(), Some("base"));
    }

    #[test]
    fn merge_patch_can_override_every_field() {
        let base = Fault::default();
        let merged = base.merge(&FaultPatch {
            delay_ms: Some(1),
            ttfb_ms: Some(2),
            slow_first_token_ms: Some(3),
            chunk_delay_ms: Some(4),
            status: Some(503),
            retry_after_secs: Some(5),
            error_body: Some(ErrorBody::CommandCode),
            fail_after_chunks: Some(6),
            fail_after_bytes: Some(7),
            malformed_json: Some(true),
            content: Some("x".into()),
            include_tool_call: Some(true),
            include_usage: Some(true),
        });
        assert_eq!(
            merged,
            Fault {
                delay_ms: 1,
                ttfb_ms: 2,
                slow_first_token_ms: 3,
                chunk_delay_ms: 4,
                status: 503,
                retry_after_secs: 5,
                error_body: ErrorBody::CommandCode,
                fail_after_chunks: 6,
                fail_after_bytes: 7,
                malformed_json: true,
                content: Some("x".into()),
                include_tool_call: true,
                include_usage: true,
            }
        );
    }

    #[test]
    fn empty_patch_is_the_identity() {
        let base = Fault {
            delay_ms: 42,
            ..Fault::default()
        };
        assert_eq!(base.merge(&FaultPatch::default()), base);
    }

    #[test]
    fn default_fault_forces_no_error_and_is_a_plain_200() {
        let f = Fault::default();
        assert!(!f.forces_error());
        assert_eq!(f.effective_status(), 200);
    }

    #[test]
    fn bare_error_body_becomes_500_and_forced_status_wins() {
        let f = Fault {
            error_body: ErrorBody::Anthropic,
            ..Fault::default()
        };
        assert!(f.forces_error());
        assert_eq!(f.effective_status(), 500);

        let f = Fault {
            status: 529,
            error_body: ErrorBody::Openai,
            ..Fault::default()
        };
        assert_eq!(f.effective_status(), 529);
    }

    #[test]
    fn reset_at_is_deterministic_arithmetic_from_a_fixed_epoch() {
        assert_eq!(reset_at(0), "2026-01-01T00:00:00Z");
        assert_eq!(reset_at(300), "2026-01-01T00:05:00Z");
        assert_eq!(reset_at(86_400), "2026-01-02T00:00:00Z");
        assert_eq!(reset_at(31_536_000), "2027-01-01T00:00:00Z");
        // Same input, same bytes, twice over.
        assert_eq!(reset_at(3_000), reset_at(3_000));
    }

    #[test]
    fn envelope_shapes_are_dialect_specific() {
        let o = error_envelope(ErrorBody::Openai, 429, "slow down", 300);
        let v: serde_json::Value = serde_json::from_str(&o).unwrap();
        assert_eq!(v["error"]["message"], "slow down");
        assert_eq!(v["error"]["retry_after"], 300);
        assert_eq!(v["error"]["reset_at"], "2026-01-01T00:05:00Z");

        let a = error_envelope(ErrorBody::Anthropic, 429, "slow down", 300);
        let v: serde_json::Value = serde_json::from_str(&a).unwrap();
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "rate_limit_error");
        assert_eq!(v["error"]["retry_after"], 300);

        let c = error_envelope(ErrorBody::CommandCode, 500, "boom", 0);
        let v: serde_json::Value = serde_json::from_str(&c).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["error"]["message"], "boom");
        assert!(v["error"].get("retry_after").is_none(), "no hint requested, none sent");

        // Auto resolves to the OpenAI envelope rather than failing.
        let auto = error_envelope(ErrorBody::Auto, 500, "boom", 0);
        let v: serde_json::Value = serde_json::from_str(&auto).unwrap();
        assert!(v.get("error").is_some());
    }

    #[test]
    fn faults_round_trip_through_json() {
        let f = Fault {
            delay_ms: 5,
            status: 429,
            error_body: ErrorBody::Anthropic,
            content: Some("hi".into()),
            ..Fault::default()
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: Fault = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);

        // A partial document parses as a patch with the missing fields unset.
        let p: FaultPatch = serde_json::from_str(r#"{"ttfb_ms": 9}"#).unwrap();
        assert_eq!(p.ttfb_ms, Some(9));
        assert!(p.delay_ms.is_none());
    }
}
