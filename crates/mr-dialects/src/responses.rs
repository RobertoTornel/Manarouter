//! OpenAI Responses API translation. Implementation pending; see [`crate::lib`] for the contract.

use bytes::Bytes;

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
