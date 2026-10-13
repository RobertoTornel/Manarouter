//! Loopback redirect listener, device-code driving, and this module's HTTPS client.
//!
//! Contents (filled in next): a `127.0.0.1` listener that accepts exactly one request and
//! renders self-contained success/failure pages; constant-time `state` comparison; PKCE and
//! state helpers; the RFC 8628-style device poll schedule; and the shared TLS-enabled HTTP
//! client every platform uses for token calls.
