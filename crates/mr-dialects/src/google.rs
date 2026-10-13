//! Gemini `generateContent` translation. Implementation pending.

use bytes::Bytes;

use crate::{Result, TranslateCtx, TranslateError, TranslatedRequest};

fn pending() -> TranslateError {
    TranslateError::Unsupported {
        from: "pivot",
        to: "google",
        route: "chat",
    }
}

/// Convert a client request into a `generateContent` request.
pub fn from_pivot_request(_ctx: &TranslateCtx<'_>, _body: &[u8]) -> Result<TranslatedRequest> {
    Err(pending())
}

/// Convert a `generateContent` response into the client dialect.
pub fn to_pivot_response(_ctx: &TranslateCtx<'_>, _body: &[u8]) -> Result<Bytes> {
    Err(pending())
}

/// Translate one Gemini SSE payload into client-dialect frames.
pub fn sse_to_pivot(_ctx: &TranslateCtx<'_>, _data: &str) -> Result<Vec<String>> {
    Err(pending())
}
