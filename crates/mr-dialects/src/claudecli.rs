//! Claude Code's own Messages surface: the identity block and the fingerprint around it.
//!
//! The body is Anthropic Messages. A Claude **subscription** token (`sk-ant-oat01-…`) is not an API
//! key, though, and the Messages API validates that a request presenting one was made by the CLI:
//! the `system` field must begin with the CLI's own identity sentence —
//!
//! > [`IDENTITY`] — `You are Claude Code, Anthropic's official CLI for Claude.`
//!
//! — as the *first element* of the `system` array. Issue anthropics/claude-code#40515 records the
//! measurement this crate relies on: with an `sk-ant-oat-*` token,
//!
//! * `system: [{identity}, {caller's block}]` → `200`
//! * `system: "<identity>"` (the whole string equal to it) → `200`
//! * the identity concatenated into one block with the caller's text → `400`
//! * no `system` at all → `400 {"type":"error","error":{"type":"invalid_request_error","message":"Error"}}`
//!
//! `claude-haiku-4-5` is exempt from the check; `claude-opus-4-6` and `claude-sonnet-4-6` are not.
//! The exemption is not something a router can rely on, so [`with_identity`] applies to every
//! request and is idempotent: a body that already begins with the sentence is left as it is, which
//! is what makes a Claude Code client pointed at this relay work unchanged.
//!
//! ## Why this is a body rewrite and not a header
//!
//! The rest of the CLI's fingerprint *is* headers, and they live in `catalog/providers.toml` on the
//! `claude-code` template plus [`mr_core::published::claude`]: `Authorization: Bearer` rather than
//! `x-api-key`, [`ANTHROPIC_BETA`], [`ANTHROPIC_VERSION`], [`user_agent`] and `x-app: cli`. A
//! subscription token sent without them is refused before a model runs. But no header can make a
//! body carry a `system` block, which is why [`mr_core::Dialect::ClaudeCli`] exists as a dialect
//! instead of as an auth method on [`mr_core::Dialect::Anthropic`].
//!
//! ## What is *not* needed
//!
//! `metadata.user_id` is not required (the 200 in #40515 was produced without it), and neither is
//! the CLI's `x-stainless-*` client scaffolding: the surface validates the identity sentence and the
//! beta flag, not a byte-exact Stainless fingerprint. That is worth stating because the opposite is
//! widely repeated.

use bytes::Bytes;
use mr_core::quota::claude::CLAUDE_CODE_VERSION;
use serde_json::{Map, Value};

use crate::{anthropic, Result, TranslateCtx, TranslateError, TranslatedRequest};

/// The `system` sentence Claude Code sends first on every request, and the one a subscription token
/// is validated against. Character for character, including the period.
pub const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

// The wire constants are defined once, in `mr_core::published::claude`, because the login
// platform in `mr-core` sends the same `anthropic-version`/`anthropic-beta` pair on its token and
// profile calls. Re-exported here so the dialect's own call sites read as this module's.
pub use mr_core::published::claude::{ANTHROPIC_BETA, ANTHROPIC_VERSION, APP, APP_HEADER};

/// The `User-Agent` the CLI presents, built from the release pinned in
/// [`mr_core::quota::claude::CLAUDE_CODE_VERSION`] so the wire string and the documented version
/// cannot drift apart.
///
/// `external` is the channel and `cli` the entry point; a build distributed through an IDE says
/// `sdk-cli` there. The surface accepts either, and the plain CLI's own value is the honest one for
/// a relay request.
pub fn user_agent() -> String {
    format!("claude-cli/{CLAUDE_CODE_VERSION} (external, cli)")
}

/// Rewrite an Anthropic Messages body so its `system` field begins with [`IDENTITY`].
///
/// Handles every shape the field takes:
///
/// * absent or `null` → `[{type: "text", text: IDENTITY}]`
/// * a string equal to [`IDENTITY`] → the same sentence as one text block (the shape the CLI itself
///   sends, so a request that already is a CLI request does not gain a duplicate)
/// * any other string → `[{identity block}, {type: "text", text: <the string>}]`, never concatenated
/// * an array → the identity block prepended unless element zero already *is* it
///
/// Anything else in `system` is a malformed request rather than something to guess at, so it is
/// refused with the same error a bad body gets. The rest of the document is untouched: `thinking`,
/// cache markers, `tool_use` blocks and sampling fields all survive, which is the point of editing
/// one field instead of translating the body.
pub fn with_identity(body: &[u8]) -> Result<Bytes> {
    let mut value: Value =
        serde_json::from_slice(body).map_err(|e| TranslateError::MalformedRequest {
            dialect: "anthropic",
            detail: e.to_string(),
        })?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| TranslateError::MalformedRequest {
            dialect: "anthropic",
            detail: "the request body is not a JSON object".to_owned(),
        })?;

    let identity_block = text_block(IDENTITY);
    match object.remove("system") {
        None | Some(Value::Null) => {
            object.insert("system".to_owned(), Value::Array(vec![identity_block]));
        }
        Some(Value::String(text)) if text == IDENTITY => {
            object.insert("system".to_owned(), Value::Array(vec![identity_block]));
        }
        Some(Value::String(text)) => {
            object.insert(
                "system".to_owned(),
                Value::Array(vec![identity_block, text_block(&text)]),
            );
        }
        Some(Value::Array(mut blocks)) => {
            let already_first = blocks
                .first()
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str)
                == Some(IDENTITY);
            if !already_first {
                blocks.insert(0, identity_block);
            }
            object.insert("system".to_owned(), Value::Array(blocks));
        }
        Some(_) => {
            return Err(TranslateError::MalformedRequest {
                dialect: "anthropic",
                detail: "`system` must be a string or an array of content blocks".to_owned(),
            });
        }
    }

    serde_json::to_vec(&value)
        .map(Bytes::from)
        .map_err(|e| TranslateError::MalformedRequest {
            dialect: "anthropic",
            detail: format!("the request body could not be re-encoded: {e}"),
        })
}

/// One plain text content block.
fn text_block(text: &str) -> Value {
    let mut block = Map::new();
    block.insert("type".to_owned(), Value::String("text".to_owned()));
    block.insert("text".to_owned(), Value::String(text.to_owned()));
    Value::Object(block)
}

/// An Anthropic request from an Anthropic client, with the identity block added.
///
/// This is the one pairing that needs a body rewrite even though the two dialects agree on the
/// shape, which is why it cannot be `Ok(None)` the way [`mr_core::Dialect::Zcode`] is.
pub fn from_anthropic_request(_ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    Ok(TranslatedRequest::json(with_identity(body)?))
}

/// An OpenAI client's request, translated to Messages and then given the identity block.
///
/// The translation's own choices — a path it names, a forced stream — are preserved: only the body
/// bytes are replaced.
pub fn from_openai_request(ctx: &TranslateCtx<'_>, body: &[u8]) -> Result<TranslatedRequest> {
    let translated = anthropic::from_openai_request(ctx, body)?;
    Ok(TranslatedRequest {
        body: with_identity(&translated.body)?,
        ..translated
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::{Dialect, RouteKind};
    use serde_json::json;

    fn rewrite(body: Value) -> Value {
        let bytes = serde_json::to_vec(&body).expect("serialize");
        let out = with_identity(&bytes).expect("rewrite");
        serde_json::from_slice(&out).expect("parse")
    }

    #[test]
    fn a_missing_system_gains_the_identity_block() {
        let out = rewrite(json!({"model": "claude-sonnet-4-6", "messages": []}));
        assert_eq!(
            out["system"],
            json!([{"type": "text", "text": IDENTITY}]),
            "the identity must be the only entry, not a string"
        );
        assert_eq!(out["model"], "claude-sonnet-4-6");
    }

    #[test]
    fn a_null_system_is_treated_as_absent() {
        let out = rewrite(json!({"system": null, "messages": []}));
        assert_eq!(out["system"], json!([{"type": "text", "text": IDENTITY}]));
    }

    #[test]
    fn a_caller_string_becomes_a_second_block_and_is_never_concatenated() {
        let out = rewrite(json!({"system": "You are a helpful assistant.", "messages": []}));
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": IDENTITY},
                {"type": "text", "text": "You are a helpful assistant."}
            ])
        );
    }

    #[test]
    fn a_string_that_is_the_identity_does_not_gain_a_duplicate() {
        let out = rewrite(json!({"system": IDENTITY, "messages": []}));
        assert_eq!(out["system"], json!([{"type": "text", "text": IDENTITY}]));
    }

    #[test]
    fn an_array_gains_the_identity_first_and_keeps_its_blocks_in_order() {
        let out = rewrite(json!({"system": [
            {"type": "text", "text": "house style", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "more"}
        ], "messages": []}));
        assert_eq!(
            out["system"],
            json!([
                {"type": "text", "text": IDENTITY},
                {"type": "text", "text": "house style", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "more"}
            ])
        );
    }

    #[test]
    fn a_body_that_already_starts_with_the_identity_is_unchanged() {
        let body = json!({
            "system": [{"type": "text", "text": IDENTITY}, {"type": "text", "text": "extra"}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let bytes = serde_json::to_vec(&body).expect("serialize");
        let once = with_identity(&bytes).expect("rewrite");
        let twice = with_identity(&once).expect("rewrite again");
        assert_eq!(once, twice, "the rewrite must be idempotent");
        let out: Value = serde_json::from_slice(&once).expect("parse");
        assert_eq!(out["system"], body["system"]);
    }

    #[test]
    fn the_rest_of_the_document_is_untouched() {
        let body = json!({
            "model": "claude-opus-4-6",
            "max_tokens": 32_000,
            "thinking": {"type": "enabled", "budget_tokens": 16_000},
            "tools": [{"name": "read", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let out = rewrite(body.clone());
        assert_eq!(out["thinking"], body["thinking"]);
        assert_eq!(out["tools"], body["tools"]);
        assert_eq!(out["max_tokens"], body["max_tokens"]);
        assert_eq!(out["messages"], body["messages"]);
    }

    #[test]
    fn a_malformed_system_is_refused_rather_than_guessed() {
        let bytes = serde_json::to_vec(&json!({"system": 7, "messages": []})).expect("serialize");
        assert!(with_identity(&bytes).is_err());
        assert!(with_identity(b"not json").is_err());
    }

    #[test]
    fn an_openai_request_is_translated_and_then_identified() {
        let ctx = TranslateCtx::new(RouteKind::ChatCompletions, Dialect::ClaudeCli, "claude-x");
        let body = serde_json::to_vec(&json!({
            "model": "claude-x",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ]
        }))
        .expect("serialize");
        let translated = from_openai_request(&ctx, &body).expect("translate");
        let out: Value = serde_json::from_slice(&translated.body).expect("parse");
        let system = out["system"].as_array().expect("system is an array");
        assert_eq!(system[0]["text"], IDENTITY);
        assert!(
            system.iter().filter(|b| b["text"] == IDENTITY).count() == 1,
            "the identity appears once, first"
        );
        assert!(system.iter().any(|b| b["text"] == "be brief"));
    }

    #[test]
    fn the_documented_fingerprint_is_the_recorded_one() {
        assert_eq!(ANTHROPIC_VERSION, "2023-06-01");
        assert_eq!(ANTHROPIC_BETA, "claude-code-20250219,oauth-2025-04-20");
        assert_eq!(APP, "cli");
        assert_eq!(user_agent(), "claude-cli/2.1.280 (external, cli)");
    }
}
