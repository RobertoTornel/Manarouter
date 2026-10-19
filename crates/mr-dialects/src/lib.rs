//! # mr-dialects
//!
//! Translation between client dialects and upstream dialects.
//!
//! ## Why this crate is optional at runtime
//!
//! The dominant case in practice is a client speaking OpenAI Chat Completions routed to a provider
//! that also speaks OpenAI Chat Completions, or an Anthropic client routed to an Anthropic
//! provider. In that case **nothing here runs**: the relay forwards the original request bytes and
//! pipes the upstream response straight back. That is where the latency budget is won.
//!
//! Every entry point is therefore shaped so that "no work needed" is the cheapest and most obvious
//! outcome: [`translate_request`] returns `Ok(None)` to mean *forward the original bytes*, and
//! [`translate_response_body`] is never called at all when the dialects match.
//!
//! ## Layers
//!
//! * [`openai`] — the OpenAI Chat Completions pivot representation.
//! * [`anthropic`] — Anthropic Messages, both directions against the pivot.
//! * [`responses`] — the OpenAI Responses API, both directions against the pivot.
//! * [`google`] — Gemini `generateContent`.
//! * [`commandcode`] — the Command Code CLI `alpha/generate` envelope and its private event stream.
//! * [`sse`] — framing helpers for `text/event-stream`, used by everything that streams.
//!
//! Translation is **hub-and-spoke with Chat Completions as the pivot**, matching both reference
//! routers, with one deliberate difference: a direct pair is preferred wherever it can be done
//! exactly. Pivoting twice loses information — an Anthropic `thinking` block has no OpenAI
//! equivalent and cannot survive the round trip — so direct pairs are used where they exist.

use bytes::Bytes;
use mr_core::{Dialect, RouteKind};

pub mod anthropic;
pub mod commandcode;
pub mod google;
pub mod openai;
pub mod responses;
pub mod sse;

/// Everything a translation needs that is not itself the body.
///
/// Passed by reference and never stored, so translating a request allocates nothing beyond the
/// output buffer.
#[derive(Debug, Clone, Copy)]
pub struct TranslateCtx<'a> {
    /// The client-facing route the request arrived on.
    pub route: RouteKind,
    /// The dialect the client speaks. Implied by `route`, but carried explicitly so a test or a
    /// proxy can exercise an unusual pairing without inventing a fake route.
    pub client_dialect: Dialect,
    /// The dialect of the provider being called.
    pub upstream_dialect: Dialect,
    /// The model id the upstream expects, after alias resolution.
    pub upstream_model: &'a str,
    /// Whether the client asked for a streamed response.
    pub client_streaming: bool,
    /// A stable identifier for this conversation, used for upstream prompt-cache affinity.
    pub session_id: Option<&'a str>,
}

impl<'a> TranslateCtx<'a> {
    /// A context for the common case.
    pub fn new(route: RouteKind, upstream_dialect: Dialect, upstream_model: &'a str) -> Self {
        Self {
            route,
            client_dialect: route.client_dialect(),
            upstream_dialect,
            upstream_model,
            client_streaming: false,
            session_id: None,
        }
    }

    /// Whether the request can be forwarded untouched.
    ///
    /// This is a *necessary* condition, not a sufficient one: a dialect can match while a route
    /// still needs header or model-name rewriting, which the relay handles without touching the
    /// body.
    #[inline]
    pub fn is_passthrough(&self) -> bool {
        self.client_dialect.is_passthrough_to(self.upstream_dialect)
    }

    /// Whether the upstream will answer with `text/event-stream` regardless of what the client
    /// asked for, so a non-streaming client needs a synthesized JSON document.
    #[inline]
    pub fn upstream_forces_stream(&self) -> bool {
        self.upstream_dialect.force_stream()
    }
}

/// A request body rewritten for an upstream that speaks a different dialect.
#[derive(Debug, Clone)]
pub struct TranslatedRequest {
    /// The bytes to send upstream.
    pub body: Bytes,
    /// Content type to declare, always JSON for the dialects here.
    pub content_type: &'static str,
    /// Whether the upstream stream flag must be forced on.
    ///
    /// Codex and Command Code always stream; the relay sets this so the upstream request and the
    /// response handling agree with each other.
    pub force_stream: bool,
    /// A path to use instead of the provider's base URL plus the route's default path.
    ///
    /// Command Code's `/alpha/generate` and Google's `models/{model}:generateContent` do not follow
    /// the OpenAI path convention, so the translator names the path it needs.
    pub path_override: Option<String>,
}

impl TranslatedRequest {
    /// A JSON request body that needs no path change and no forced stream.
    pub fn json(body: impl Into<Bytes>) -> Self {
        Self {
            body: body.into(),
            content_type: "application/json",
            force_stream: false,
            path_override: None,
        }
    }
}

/// Why a translation could not be performed.
#[derive(Debug, thiserror::Error)]
pub enum TranslateError {
    /// The request body was not valid JSON, or not the shape the client dialect requires.
    #[error("malformed {dialect} request: {detail}")]
    MalformedRequest {
        /// The dialect that failed to parse.
        dialect: &'static str,
        /// What was wrong.
        detail: String,
    },

    /// The upstream body was not valid JSON, or not the shape the upstream dialect promises.
    #[error("malformed {dialect} response: {detail}")]
    MalformedResponse {
        /// The dialect that failed to parse.
        dialect: &'static str,
        /// What was wrong.
        detail: String,
    },

    /// The pairing is not implemented.
    ///
    /// The relay turns this into a `400` that names both dialects, rather than forwarding a body
    /// the upstream will reject with a confusing error.
    #[error("no translation from {from} to {to} for route {route}")]
    Unsupported {
        /// Client dialect.
        from: &'static str,
        /// Upstream dialect.
        to: &'static str,
        /// The route that was attempted.
        route: &'static str,
    },

    /// An SSE frame could not be interpreted.
    #[error("malformed SSE frame: {0}")]
    Sse(String),
}

/// The result type used throughout this crate.
pub type Result<T, E = TranslateError> = std::result::Result<T, E>;

/// Rewrite a client request body for an upstream that speaks a different dialect.
///
/// Returns `Ok(None)` when the body can be forwarded **verbatim**, which is the fast path and the
/// common case. Callers must treat `None` as "send the original bytes", never as an error, and must
/// not re-serialise the body in that case.
pub fn translate_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Option<TranslatedRequest>> {
    match (ctx.client_dialect, ctx.upstream_dialect) {
        // Identical dialects: nothing to do.
        (a, b) if a.is_passthrough_to(b) => Ok(None),

        // GitHub Copilot exposes an OpenAI-shaped chat surface once the bearer is attached, so an
        // OpenAI client needs no body translation for it. Antigravity sat on this line too and
        // was wrong there: its credential does not change the wire shape. `cloudcode-pa.googleapis.com`
        // speaks Gemini `generateContent` wrapped in `{model, request, userAgent, requestId}`, its
        // answers come back wrapped in `{"response": ...}`, and mr-core marks it neither
        // passthrough-able nor body-flag streaming (it always streams). It routes through the
        // google translator below instead — see `docs/providers/antigravity.md` and the request
        // body in `docs/12-oauth-antigravity.md` §5.
        (Dialect::OpenAi, Dialect::Copilot) => Ok(None),
        // The Anthropic half of this pair had no arm on ANY of the three functions: Unsupported
        // before a byte moved. Copilot's shape says it belongs here — `docs/providers/copilot.md`
        // gives `/chat/completions` as "OpenAI Chat Completions", and `catalog/providers.toml`
        // lists `dialect = "copilot"` under `id = "github-copilot"` with a `messages` capability,
        // i.e. Claude Code sessions are meant to reach it. It takes the same direct
        // Anthropic-to-OpenAI pair `(Anthropic, OpenAi)` uses below, not a double pivot.
        (Dialect::Anthropic, Dialect::Copilot) => {
            openai::from_anthropic_request(ctx, body).map(Some)
        }

        (Dialect::OpenAi, Dialect::Anthropic) => {
            anthropic::from_openai_request(ctx, body).map(Some)
        }
        (Dialect::Anthropic, Dialect::OpenAi) => {
            openai::from_anthropic_request(ctx, body).map(Some)
        }

        // Plain Gemini and Cloud Code are the same inference shape. `google::from_pivot_request`
        // picks `models/{model}:generateContent` or the `v1internal:` envelope off the upstream
        // dialect, so both pairings land on the same translator.
        (Dialect::OpenAi, Dialect::Google)
        | (Dialect::Anthropic, Dialect::Google)
        | (Dialect::OpenAi, Dialect::Antigravity)
        | (Dialect::Anthropic, Dialect::Antigravity) => {
            google::from_pivot_request(ctx, body).map(Some)
        }

        (Dialect::OpenAi, Dialect::CommandCodeGo)
        | (Dialect::Anthropic, Dialect::CommandCodeGo) => {
            commandcode::from_pivot_request(ctx, body).map(Some)
        }

        (Dialect::OpenAi, Dialect::Codex) | (Dialect::Anthropic, Dialect::Codex) => {
            responses::from_pivot_request(ctx, body).map(Some)
        }

        (from, to) => Err(TranslateError::Unsupported {
            from: from.as_str(),
            to: to.as_str(),
            route: ctx.route.as_str(),
        }),
    }
}

/// Rewrite a complete (non-streaming) upstream response body into the client's dialect.
///
/// Only called when the dialects differ. When they match, the relay pipes bytes and never reaches
/// this function.
pub fn translate_response_body(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<Bytes> {
    match (ctx.client_dialect, ctx.upstream_dialect) {
        (Dialect::OpenAi, Dialect::Anthropic) => anthropic::to_openai_response(ctx, body),
        (Dialect::Anthropic, Dialect::OpenAi) => openai::to_anthropic_response(ctx, body),
        // Copilot's chat surface answers in OpenAI Chat JSON, so an OpenAI client's answer is
        // byte-identical — the generic passthrough guard below cannot carry this, because mr-core
        // allows it only for identical dialects — and an Anthropic client gets the same direct
        // OpenAI-to-Anthropic pair `(Anthropic, OpenAi)` uses. Without these arms a Copilot
        // round trip failed with Unsupported at the response even though the request succeeded.
        (Dialect::OpenAi, Dialect::Copilot) => Ok(Bytes::copy_from_slice(body)),
        (Dialect::Anthropic, Dialect::Copilot) => openai::to_anthropic_response(ctx, body),
        // `google::to_pivot_response` unwraps the Cloud Code `{response: {...}, traceId}`
        // envelope when the upstream is Antigravity, so both Gemini pairings answer here.
        (Dialect::OpenAi, Dialect::Google)
        | (Dialect::Anthropic, Dialect::Google)
        | (Dialect::OpenAi, Dialect::Antigravity)
        | (Dialect::Anthropic, Dialect::Antigravity) => google::to_pivot_response(ctx, body),
        (Dialect::OpenAi, Dialect::CommandCodeGo)
        | (Dialect::Anthropic, Dialect::CommandCodeGo) => commandcode::to_pivot_response(ctx, body),
        (Dialect::OpenAi, Dialect::Codex) | (Dialect::Anthropic, Dialect::Codex) => {
            responses::to_pivot_response(ctx, body)
        }
        _ if ctx.is_passthrough() => Ok(Bytes::copy_from_slice(body)),
        (from, to) => Err(TranslateError::Unsupported {
            from: from.as_str(),
            to: to.as_str(),
            route: ctx.route.as_str(),
        }),
    }
}

/// Translate one upstream SSE frame into zero or more client-dialect frames.
///
/// The input is a single complete frame's **data payload** with the `data:` prefix already
/// stripped. The output is a `Vec` because one upstream event can legitimately become several
/// client events.
///
/// Returning an empty `Vec` means "this frame carries nothing the client needs", which is the
/// correct handling for keep-alives and for event types the client's dialect has no equivalent for.
///
/// **Command Code is not driven through here.** Its stream state must be owned per stream, so the
/// relay uses [`commandcode::Translator`] for that dialect instead; see its documentation for the
/// corruption this avoids. Everything else still accumulates in the per-thread state that
/// [`reset_stream_state`] clears at a stream boundary.
pub fn translate_sse_data(ctx: &TranslateCtx<'_>, data: &str) -> Result<Vec<String>> {
    match (ctx.client_dialect, ctx.upstream_dialect) {
        (Dialect::OpenAi, Dialect::Anthropic) => anthropic::sse_to_openai(ctx, data),
        (Dialect::Anthropic, Dialect::OpenAi) => openai::sse_to_anthropic(ctx, data),
        // Same shapes as the response half: Copilot's stream is OpenAI SSE end to end, so the
        // frames ride through untouched for an OpenAI client and into the Anthropic adapter for
        // the rest.
        (Dialect::OpenAi, Dialect::Copilot) => Ok(vec![data.to_owned()]),
        (Dialect::Anthropic, Dialect::Copilot) => openai::sse_to_anthropic(ctx, data),
        (Dialect::OpenAi, Dialect::CommandCodeGo)
        | (Dialect::Anthropic, Dialect::CommandCodeGo) => commandcode::sse_to_pivot(ctx, data),
        // Cloud Code's stream frames carry the same `{"response": ...}` wrapper, unwrapped per
        // frame inside `google::sse_to_pivot`.
        (Dialect::OpenAi, Dialect::Google)
        | (Dialect::Anthropic, Dialect::Google)
        | (Dialect::OpenAi, Dialect::Antigravity)
        | (Dialect::Anthropic, Dialect::Antigravity) => google::sse_to_pivot(ctx, data),
        (Dialect::OpenAi, Dialect::Codex) | (Dialect::Anthropic, Dialect::Codex) => {
            responses::sse_to_pivot(ctx, data)
        }
        _ if ctx.is_passthrough() => Ok(vec![data.to_owned()]),
        (from, to) => Err(TranslateError::Unsupported {
            from: from.as_str(),
            to: to.as_str(),
            route: ctx.route.as_str(),
        }),
    }
}

/// The `[DONE]` sentinel used by the OpenAI streaming protocol.
pub const SSE_DONE: &str = "[DONE]";

/// Forget every per-thread translator state this crate keeps.
///
/// These are keyed by nothing but the worker thread, and are only self-healing once a *terminal*
/// frame has been translated — so a stream that ends any other way (the upstream dies, the client
/// disconnects, the relay abandons the attempt) leaves its partial calls behind, and the next
/// stream on that thread inherits them. That is how a completed tool call can reappear, or how an
/// argument fragment can land in a call the client never saw opened.
///
/// **This is not sufficient for Command Code**, whose two concurrent streams on one worker thread
/// provably contaminated each other through exactly this cell; that dialect owns its state per
/// stream through [`commandcode::Translator`]. Resetting the rest is still meaningful for the
/// remaining dialects. Resetting all of them is deliberate: one hop can consult more than one
/// adapter, and a state left behind in *any* of them corrupts the next answer.
pub fn reset_stream_state() {
    commandcode::reset_stream_state();
    google::reset_stream_state();
    responses::reset_stream_state();
    // The two halves of the OpenAI module: pivot→Anthropic and Anthropic→pivot.
    openai::reset_stream_state();
    openai::reset_openai_stream_state();
    anthropic::reset_stream_state();
}

/// The event name the relay emits when it transparently retries a stream with a new model.
///
/// Namespaced so it can never collide with a provider's own event, and so a client that ignores
/// unknown events still terminates the message correctly. See `midstream_policy` in
/// `docs/07-hedged-race.md`.
pub const RETRY_EVENT: &str = "manarouter.retry";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_dialects_need_no_translation_at_all() {
        // This is the hot path: if it ever starts doing work, the latency budget is gone.
        for route in [RouteKind::ChatCompletions, RouteKind::Messages] {
            let ctx = TranslateCtx::new(route, route.client_dialect(), "m");
            assert!(ctx.is_passthrough());
            assert!(translate_request(&ctx, br#"{"model":"m"}"#)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn passthrough_works_for_a_body_that_is_not_even_json() {
        // Forwarding must not depend on the body being parseable: the relay's job is to shuttle
        // bytes, and a body it cannot read is still a body the upstream might accept.
        let ctx = TranslateCtx::new(RouteKind::ChatCompletions, Dialect::OpenAi, "m");
        let junk: &[u8] = b"not json at all \x00\xff";
        assert!(translate_request(&ctx, junk).unwrap().is_none());
        assert_eq!(translate_response_body(&ctx, junk).unwrap().as_ref(), junk);
    }

    #[test]
    fn upstream_forced_stream_is_visible_to_the_caller() {
        let ctx = TranslateCtx::new(RouteKind::Responses, Dialect::Codex, "gpt-5");
        assert!(ctx.upstream_forces_stream());
        let ctx = TranslateCtx::new(RouteKind::ChatCompletions, Dialect::OpenAi, "m");
        assert!(!ctx.upstream_forces_stream());
    }

    #[test]
    fn openai_to_copilot_needs_no_body_translation() {
        // Copilot speaks OpenAI chat once the bearer is attached, so only headers change.
        let ctx = TranslateCtx::new(RouteKind::ChatCompletions, Dialect::Copilot, "gpt-4o");
        assert!(translate_request(&ctx, br#"{"model":"gpt-4o"}"#)
            .unwrap()
            .is_none());
    }

    #[test]
    fn unsupported_pairings_name_both_dialects_rather_than_failing_obscurely() {
        // Anthropic in, Google out is not wired directly; it must report the pairing rather than
        // silently forwarding an Anthropic body to a Gemini endpoint.
        let mut ctx = TranslateCtx::new(RouteKind::Messages, Dialect::CommandCodeGo, "m");
        ctx.client_dialect = Dialect::Copilot;
        match translate_request(&ctx, b"{}") {
            Err(TranslateError::Unsupported { from, to, .. }) => {
                assert_eq!(from, "copilot");
                assert_eq!(to, "command-code-go");
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn retry_event_is_namespaced_so_it_cannot_collide_with_a_provider_event() {
        assert!(RETRY_EVENT.starts_with("manarouter."));
    }

    #[test]
    fn antigravity_upstreams_reach_the_gemini_translator_on_every_half() {
        // Reachability, not correctness. google.rs's own tests all passed while this pairing
        // still forwarded an OpenAI body verbatim to `cloudcode-pa/v1internal:generateContent`
        // and failed the response half with Unsupported — a dead module with perfect unit
        // coverage. This asserts the dispatcher arms instead.
        let ctx = TranslateCtx::new(
            RouteKind::ChatCompletions,
            Dialect::Antigravity,
            "gemini-2.5-pro",
        );

        let req = translate_request(
            &ctx,
            br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("an OpenAI client must reach the translator for an Antigravity upstream")
        .expect("byte-forwarding would hand v1internal an OpenAI body");
        let doc: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(doc["request"]["contents"][0]["parts"][0]["text"], "hi");
        assert!(
            req.path_override
                .as_deref()
                .is_some_and(|p| p.starts_with("v1internal:")),
            "the Cloud Code endpoint names its methods, not its models"
        );

        let resp = translate_response_body(
            &ctx,
            br#"{"response":{"candidates":[{"content":{"parts":[{"text":"hey"}]},"finishReason":"STOP"}]},"traceId":"t"}"#,
        )
        .expect("the Cloud Code response wrapper must unwrap, not report Unsupported");
        let resp: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(resp["choices"][0]["message"]["content"], "hey");

        let frames = translate_sse_data(
            &ctx,
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"x"}]},"finishReason":"STOP"}]}}"#,
        )
        .expect("the SSE half must route to the Gemini translator as well");
        assert!(frames.iter().any(|f| f.contains("[DONE]")));

        // The Anthropic-client pairing had no arm at all before: Unsupported outright.
        let anthropic_ctx =
            TranslateCtx::new(RouteKind::Messages, Dialect::Antigravity, "gemini-2.5-pro");
        assert!(translate_request(
            &anthropic_ctx,
            br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#
        )
        .is_ok());
    }

    #[test]
    fn copilot_upstreams_reach_the_right_translator_on_every_half() {
        // Reachability, not correctness — the same defect class as the Antigravity test above.
        // Before this change (Anthropic, Copilot) was Unsupported on all three functions, and
        // the OpenAI client's response and SSE halves were too: mr-core's passthrough guard
        // covers identical dialects only, so no generic arm ever fired for Copilot either.
        // The shapes are settled by docs/providers/copilot.md (`/chat/completions` = "OpenAI
        // Chat Completions") and catalog/providers.toml (`dialect = "copilot"`, capabilities
        // chat + messages): byte-identical for an OpenAI client, the direct Anthropic-to-OpenAI
        // pair for an Anthropic one, answers inverted on the way back.
        let openai_ctx = TranslateCtx::new(RouteKind::ChatCompletions, Dialect::Copilot, "gpt-4o");
        let anthropic_ctx = TranslateCtx::new(RouteKind::Messages, Dialect::Copilot, "gpt-4o");

        let req = translate_request(
            &openai_ctx,
            br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("an OpenAI client must reach Copilot");
        assert!(
            req.is_none(),
            "and its body travels untouched: for Copilot only headers change"
        );

        let req = translate_request(
            &anthropic_ctx,
            br#"{"model":"claude-sonnet-4","max_tokens":64,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#,
        )
        .expect("an Anthropic client must reach Copilot, not Unsupported")
        .expect("an Anthropic body is not byte-compatible with /chat/completions");
        let doc: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(doc["messages"][0]["content"], "hi");
        assert_eq!(
            req.path_override.as_deref(),
            Some("chat/completions"),
            "Copilot's chat endpoint is named the same way the direct pair names it"
        );

        let raw = br#"{"id":"x","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"hey"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
        let resp = translate_response_body(&openai_ctx, raw)
            .expect("an OpenAI client's Copilot answer must not report Unsupported");
        assert_eq!(
            resp.as_ref(),
            &raw[..],
            "byte-identical: an OpenAI answer for an OpenAI client needs no re-serialisation"
        );

        let resp = translate_response_body(&anthropic_ctx, raw)
            .expect("an Anthropic client's Copilot answer must invert, not report Unsupported");
        let msg: serde_json::Value = serde_json::from_slice(&resp).unwrap();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], "hey");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["usage"]["input_tokens"], 1);

        let payload = r#"{"choices":[{"delta":{"content":"a"}}]}"#;
        let frames = translate_sse_data(&openai_ctx, payload)
            .expect("an OpenAI client's Copilot stream must not report Unsupported");
        assert_eq!(frames.len(), 1, "one upstream frame, one client frame");
        assert_eq!(frames[0], payload, "and untouched");

        openai::reset_stream_state();
        let start = translate_sse_data(
            &anthropic_ctx,
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
        )
        .expect("an Anthropic client's Copilot stream must reach the adapter");
        assert!(start.join("").contains("message_start"));
        let end = translate_sse_data(&anthropic_ctx, "[DONE]")
            .expect("the stream must be allowed to close");
        let end = end.join("");
        assert!(end.contains("message_stop"), "the message must terminate");
        assert!(
            !end.contains("data: [DONE]"),
            "an Anthropic SDK rejects the OpenAI terminator"
        );
    }
}
