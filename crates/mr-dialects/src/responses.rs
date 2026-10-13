//! The OpenAI Responses API, both directions against the Chat Completions pivot.
//!
//! Responses is the dialect Codex speaks at `chatgpt.com/backend-api/codex/responses`, and the one
//! a Responses-native client (`RouteKind::Responses`) expects on the wire. Two properties set it
//! apart from Chat Completions and drive everything in this module:
//!
//! * the transcript lives in a typed `input` item array rather than free-form `messages`, with
//!   `instructions` as the top-level system slot — so a request translation is a structured
//!   rewrite, not a rename;
//! * the endpoint is SSE-first: `stream` is effectively always on, `store` must be `false`, and
//!   the stream is a typed event protocol (`response.created`, `output_text.delta`,
//!   `response.completed`, …) rather than bare `chat.completion.chunk` payloads — so a response
//!   translation has to track cross-frame state: one role chunk, stable tool indices, one
//!   terminator.
//!
//! The terminator is where the two client shapes diverge: a Chat Completions client needs a final
//! `finish_reason` chunk and `data: [DONE]`, while a Responses client terminates on
//! `response.completed` and treats `[DONE]` as protocol noise — so `[DONE]` is suppressed for
//! Responses routes and re-emitted only for Chat Completions clients.

use bytes::Bytes;
use serde_json::{json, Map, Value};

use crate::openai::{flatten_text, rewrite_model};
use crate::sse;
use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest};

fn pending() -> TranslateError {
    TranslateError::Unsupported {
        from: "pivot",
        to: "responses",
        route: "responses",
    }
}

/// Convert a client request into a Responses API request.
pub fn from_pivot_request(_ctx: &TranslateCtx<'_>, _body: &[u8]) -> Result<TranslatedRequest> {
    Err(pending())
}

/// Convert a Responses API response into the client dialect.
pub fn to_pivot_response(_ctx: &TranslateCtx<'_>, _body: &[u8]) -> Result<Bytes> {
    Err(pending())
}

/// Translate one Responses SSE payload into client-dialect frames.
pub fn sse_to_pivot(_ctx: &TranslateCtx<'_>, _data: &str) -> Result<Vec<String>> {
    Err(pending())
}
