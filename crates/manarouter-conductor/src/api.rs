//! # api.rs — the control plane's REST surface
//!
//! Every endpoint the panel (`panel/README.md`, `panel/api.js`) calls, and the contract they
//! imply. Three rules govern this file:
//!
//! 1. **The panel's client is the contract.** `api.js` returns parsed payloads as-is and only
//!    wraps *errors* in `{ "error": { code, message, details } }`. Success responses are therefore
//!    bare documents (`{"providers": [...]}`, or the created object directly) — never a
//!    success-envelope the client would have to unwrap.
//! 2. **A credential never leaves this process.** Passwords arrive only to be verified against an
//!    Argon2 hash; client-key plaintext exists only in the one response that creates it; provider
//!    secrets live in the vault and reach a response only as a `secret_ref` or a fingerprint
//!    (`••••…`). What *is* a redacted placeholder is healed back to its stored value on the way
//!    in, so a GET→PUT round trip through the panel is loss-free.
//! 3. **Validation before bytes, compile before save.** All mutation goes through
//!    [`AppState::commit`] (or [`AppState::commit_with`] for the single waived condition in
//!    [`create_provider`]), and the vault writes performed while assembling an edit are journaled
//!    in a [`VaultTx`] so a rejected edit rolls the vault back — a 400 response must never leave
//!    orphaned or overwritten secrets behind.
//!
//! Traps this crate has already stepped on, recorded so nobody repeats them: axum 0.8 path
//! patterns are `/{id}` and `/{*rest}`, not `/:id` and `/*rest`; `Path` percent-decodes captures,
//! so `..%2f` arrives here as `../`; a `Secure` cookie attribute sent over plain HTTP makes the
//! browser drop the cookie and bricks the panel, so `Secure` is conditional on the request's
//! effective scheme; and a flat serde map would happily absorb a plaintext secret sent under
//! `secret`, which is why every ingress path strips it before parsing.

use std::path::Path as FsPath;
use std::time::Duration;

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, Request, State};
use axum::http::header::{self, HeaderMap};
use axum::http::{HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as LegacyClient;
use hyper_util::rt::TokioExecutor;
use mr_core::config::Config;
use mr_core::secret::{
    account_secret_id, client_key_secret_id, fingerprint, looks_like_a_credential,
};
use mr_core::{
    Account, AccountId, AuthKind, ClientKey, ClientKeyId, Combo, ComboId, Millis, ModelEntry,
    ModelSource, ProviderId,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::auth::{
    cookie_value, hash_password, is_remote_exposed, origin_is_acceptable, SESSION_COOKIE,
    SESSION_TTL,
};
use crate::state::{AppState, CommitError, VaultTx};

/// Build the control-plane router with its state attached.
///
/// Equivalent to `guarded_router(state).with_state(state)`; kept because it is what the tests
/// drive directly.
pub fn router(state: AppState) -> Router {
    guarded_router(state.clone()).with_state(state)
}

/// Routes + guard layer, state still open (`Router<AppState>`).
///
/// `serve.rs` composes this with the static-panel fallback *before* attaching state, because
/// axum applies `layer` only to routes that already exist: registering the fallback after the
/// layer leaves it unauthenticated (the panel's `index.html` must load before anyone has signed
/// in), while the `/api/{*rest}` catch-all — registered here, inside the guarded set — keeps the
/// JSON error envelope for unknown API paths instead of serving the panel shell.
pub(crate) fn guarded_router(state: AppState) -> Router<AppState> {
    Router::new()
        // --- sessions ---
        .route("/api/health", get(health))
        .route("/api/session", get(session))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        // --- configuration ---
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/validate", get(config_validate))
        .route("/api/config/raw", get(get_config_raw).put(put_config_raw))
        .route("/api/ports", get(get_ports).put(put_ports))
        .route("/api/restart", post(request_restart))
        // --- providers, accounts, models ---
        .route("/api/providers", get(list_providers).post(create_provider))
        .route(
            "/api/providers/{id}",
            put(update_provider).delete(delete_provider),
        )
        .route("/api/providers/{id}/accounts", post(create_account))
        .route(
            "/api/providers/{id}/accounts/{account_id}",
            put(update_account).delete(delete_account),
        )
        .route("/api/providers/{id}/models/sync", post(sync_models))
        .route("/api/providers/{id}/models", post(upsert_model))
        .route("/api/providers/{id}/models/{model}", delete(delete_model))
        // --- combos, aliases, keys ---
        .route("/api/combos", get(list_combos).post(create_combo))
        .route("/api/combos/{id}", put(update_combo).delete(delete_combo))
        .route("/api/aliases", get(list_aliases).post(create_alias))
        .route("/api/aliases/{name}", delete(delete_alias))
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{id}", put(update_key).delete(delete_key))
        // --- documentation ---
        .route("/api/docs", get(list_docs))
        .route("/api/docs/{name}", get(get_doc))
        // --- unknown API paths keep the JSON envelope ---
        .route("/api", any(api_not_found))
        .route("/api/", any(api_not_found))
        .route("/api/{*rest}", any(api_not_found))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
}

// ---------------------------------------------------------------------------
// Request guard
// ---------------------------------------------------------------------------

/// Order matters: origin first, authentication second.
///
/// Refusing a forged cross-origin request *before* consulting the session means an attacker's
/// probe learns neither whether a session exists nor whether a password is set — a 403 with a
/// constant body either way.
async fn guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    if matches!(
        method,
        Method::POST | Method::PUT | Method::DELETE | Method::PATCH
    ) {
        let headers = req.headers();
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        let referer = headers.get(header::REFERER).and_then(|v| v.to_str().ok());
        // The port the browser would have had to match: the one in this request's Host header,
        // falling back to the port this process is actually bound to.
        let port = host_port(headers).unwrap_or_else(|| state.running_control_port());
        if !origin_is_acceptable(origin, referer, port) {
            return ApiError::forbidden(
                "origin_rejected",
                "this request came from another origin; the control plane only accepts \
                 same-origin or loopback requests",
            )
            .into_response();
        }
    }

    if path.starts_with("/api") && !exempt_from_auth(&method, &path) {
        let token = req
            .headers()
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| cookie_value(v, SESSION_COOKIE));
        if !state.auth.read().await.permits(token) {
            return ApiError::unauthorized("a session is required; sign in first").into_response();
        }
    }

    next.run(req).await
}

/// The endpoints reachable without a session: the health probe (the panel shell polls it before
/// anyone has signed in), the session lookup itself, and the two session lifecycle calls. Login
/// still passes the origin check above — a cross-site login attempt is refused all the same.
fn exempt_from_auth(method: &Method, path: &str) -> bool {
    (method == Method::GET && (path == "/api/health" || path == "/api/session"))
        || (method == Method::POST && (path == "/api/login" || path == "/api/logout"))
}

/// The port from a `Host` header (`127.0.0.1:20901` → `20901`), `None` when absent or portless.
fn host_port(headers: &HeaderMap) -> Option<u16> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let authority: axum::http::uri::Authority = host.parse().ok()?;
    authority.port_u16()
}

/// The JSON 404 for unknown API paths: the `/api/{*rest}` catch-all in [`guarded_router`] serves
/// this, and `serve.rs`'s static fallback uses it as its belt — an API path must never fall
/// through to the panel's `index.html`.
pub fn api_not_found_response() -> Response {
    ApiError::not_found("no such control-plane endpoint").into_response()
}

/// The one handler behind the `/api/{*rest}` catch-all.
async fn api_not_found() -> Response {
    api_not_found_response()
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// The response shape for every failure: `{"error": {"code", "message", "details": [...]}}`,
/// exactly what `panel/api.js` parses. `details` carries the full list of validation problems —
/// the panel renders every entry, so partial information would become a partial UI.
pub struct ApiError {
    status: StatusCode,
    code: String,
    message: String,
    details: Vec<Value>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": { "code": self.code, "message": self.message, "details": self.details },
        });
        (self.status, Json(body)).into_response()
    }
}

impl ApiError {
    fn new(status: StatusCode, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            details: Vec::new(),
        }
    }

    fn with_details(mut self, details: Vec<Value>) -> Self {
        self.details = details;
        self
    }

    /// 400 for anything the *request* got wrong: unparseable JSON, a port that cannot work, a
    /// configuration `validate()` refuses.
    fn bad(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message)
    }

    fn forbidden(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "already_exists", message)
    }

    /// 500 for anything *we* failed at: the vault would not write, the config would not save.
    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }

    /// 502/504 for an upstream that refused or never answered during model discovery.
    fn upstream(status: StatusCode, message: impl Into<String>) -> Self {
        Self::new(status, "upstream", message)
    }
}

impl From<CommitError> for ApiError {
    fn from(err: CommitError) -> Self {
        match &err {
            // `Validation` covers both `Config::validate` problems and a snapshot that refused to
            // compile — the operator fixes them the same way, and nothing was written either way.
            CommitError::Validation(_) => {
                ApiError::bad("validation_failed", err.message()).with_details(err.details())
            }
            CommitError::Save(_) => ApiError::internal(err.message()),
            CommitError::SnapshotWrite(_) => ApiError::internal(err.message()),
        }
    }
}

/// Body extractor producing the raw JSON value.
///
/// A custom type (rather than `Json<Value>` in every signature) exists to keep rejections inside
/// the error envelope: axum's default JSON rejection would answer with its own plain-text 415/400
/// shape, which the panel cannot parse.
pub struct JsonBody(pub Value);

impl<S> axum::extract::FromRequest<S> for JsonBody
where
    S: Send + Sync,
    Json<Value>: axum::extract::FromRequest<S, Rejection = JsonRejection>,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<Value>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(ApiError::bad("invalid_json", rejection.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// The body must be a JSON object — every endpoint here takes one.
fn object_body(body: Value, what: &str) -> ApiResult<Map<String, Value>> {
    match body {
        Value::Object(map) => Ok(map),
        _ => Err(ApiError::bad(
            "invalid_body",
            format!("the {what} body must be a JSON object"),
        )),
    }
}

fn sha256_hex(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The first seven characters of a client key, stored so the panel can tell two keys apart
/// without holding either of them.
fn key_prefix(plaintext: &str) -> String {
    plaintext.chars().take(7).collect()
}

/// Mint a client key: `mr-` + 40 lowercase hex characters (25 random bytes).
///
/// The `mr-` prefix and the SHA-256 hash format are cross-crate assumptions: this repository has
/// no relay-side verifier yet, so if the relay later standardises a different format, generation
/// and `key_hash` derivation are the two places to change.
fn generate_client_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 25];
    rand::thread_rng().fill_bytes(&mut bytes);
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("mr-{hex}")
}

/// A header name that makes its value credential-shaped, regardless of the value itself.
fn header_looks_secret(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "authorization",
        "auth",
        "api-key",
        "api_key",
        "apikey",
        "token",
        "secret",
        "password",
        "credential",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// Redact credential-shaped values from a provider's `headers` map, in place.
///
/// Round trips stay loss-free because [`heal_provider_headers`] restores any value that comes
/// back byte-identical to a fingerprint of what is stored.
fn redact_headers(headers: &mut Map<String, Value>) {
    for (name, value) in headers.iter_mut() {
        let Some(text) = value.as_str() else { continue };
        if header_looks_secret(name) || looks_like_a_credential(text) {
            *value = Value::String(fingerprint(text));
        }
    }
}

/// The whole configuration as the panel may see it: password hash removed, client-key hashes
/// removed, credential-shaped provider headers fingerprinted. Everything else is the stored
/// document.
fn redact_config(config: &Config) -> ApiResult<Value> {
    let mut value = serde_json::to_value(config).map_err(|e| {
        ApiError::internal(format!("the configuration could not be serialised: {e}"))
    })?;
    let Some(obj) = value.as_object_mut() else {
        return Err(ApiError::internal(
            "the configuration did not serialise as a JSON object",
        ));
    };
    // An absent `password_hash` reads as "no password"; `heal_control` puts the stored value
    // back on the way in so a panel save cannot silently delete it.
    if let Some(control) = obj.get_mut("control").and_then(Value::as_object_mut) {
        control.remove("password_hash");
    }
    if let Some(keys) = obj.get_mut("client_keys").and_then(Value::as_array_mut) {
        for key in keys.iter_mut() {
            if let Some(key) = key.as_object_mut() {
                key.remove("key_hash");
            }
        }
    }
    if let Some(providers) = obj.get_mut("providers").and_then(Value::as_array_mut) {
        for provider in providers.iter_mut() {
            if let Some(headers) = provider.get_mut("headers").and_then(Value::as_object_mut) {
                redact_headers(headers);
            }
        }
    }
    Ok(value)
}

/// One provider as responses show it (its headers redacted the same way as in the config view).
fn provider_view(provider: &mr_core::Provider) -> ApiResult<Value> {
    let mut value = serde_json::to_value(provider)
        .map_err(|e| ApiError::internal(format!("the provider could not be serialised: {e}")))?;
    if let Some(headers) = value.get_mut("headers").and_then(Value::as_object_mut) {
        redact_headers(headers);
    }
    Ok(value)
}

/// One client key as responses show it: `key_hash` stripped, `prefix` kept.
fn key_view(key: &ClientKey) -> ApiResult<Value> {
    let mut value = serde_json::to_value(key)
        .map_err(|e| ApiError::internal(format!("the client key could not be serialised: {e}")))?;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("key_hash");
    }
    Ok(value)
}

fn ports_view(
    config: &Config,
    relay_running: u16,
    control_running: u16,
    restart_pending: bool,
) -> Value {
    json!({
        "relay": {
            "host": &config.relay.host,
            "port": config.relay.port,
            "running_port": relay_running,
        },
        "control": {
            "host": &config.control.host,
            "port": config.control.port,
            "running_port": control_running,
        },
        // The panel banner reads this to say "applies on next restart" instead of pretending the
        // listeners moved: the control plane could rebind itself, the relay is another process.
        "applies_on": "next_restart",
        "restart_pending": restart_pending,
    })
}

fn text_response(status: StatusCode, content_type: &'static str, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, content_type)], body).into_response()
}

// ---------------------------------------------------------------------------
// Cookies and session material
// ---------------------------------------------------------------------------

/// The session cookie.
///
/// `Secure` is added **only** when the request actually arrived over HTTPS (directly or via a
/// terminating proxy's `X-Forwarded-Proto`). Setting `Secure` on plain HTTP — which is how a
/// loopback panel is served — makes the browser discard the cookie, and the panel then 401-loops
/// forever with a password that is perfectly correct.
fn session_cookie(token: &str, secure: bool) -> String {
    let mut cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_TTL.as_secs()
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// The same cookie with `Max-Age=0`, which makes the browser delete it.
fn clear_cookie(secure: bool) -> String {
    let mut cookie = format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Whether this request should be treated as HTTPS for cookie purposes.
fn request_is_https(headers: &HeaderMap, uri: &Uri) -> bool {
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("https"))
        .unwrap_or(false);
    forwarded || uri.scheme_str() == Some("https")
}

fn append_cookie(response: &mut Response, cookie: &str) -> ApiResult<()> {
    let value = HeaderValue::from_str(cookie)
        .map_err(|e| ApiError::internal(format!("the session cookie could not be built: {e}")))?;
    response.headers_mut().append(header::SET_COOKIE, value);
    Ok(())
}

/// The session token from a request's `Cookie` header, if it carries one.
fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| cookie_value(v, SESSION_COOKIE))
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Ingress healing: keep round trips loss-free and secrets out of the config file
// ----------------------------------------------------------------

/// Restore the control password the GET view removed, and the format version a hand-edit may
/// have dropped.
///
/// An *absent* `password_hash` means "the panel redacted it" — the stored value is reinstated. An
/// explicit `null` means "the operator is clearing the password" and is honoured (subject to the
/// remote-bind policy check after parsing). Without this distinction every panel save would
/// delete the password it just used to authenticate with.
fn heal_control(obj: &mut Map<String, Value>, current: &Config) {
    if let Some(control) = obj.get_mut("control").and_then(Value::as_object_mut) {
        if !control.contains_key("password_hash") {
            match &current.control.password_hash {
                Some(hash) => {
                    control.insert("password_hash".to_owned(), Value::String(hash.clone()));
                }
                None => {
                    control.remove("password_hash");
                }
            }
        }
    }
    if !obj.contains_key("format_version") {
        obj.insert(
            "format_version".to_owned(),
            Value::from(current.format_version),
        );
    }
}

/// Restore a redacted provider-header placeholder (a fingerprint) to the value it stands for.
///
/// The GET view replaced credential-shaped header values with `fingerprint(stored)`; if the panel
/// sends that placeholder back unchanged, we know it means "the stored value". A hand-edit that
/// changes the placeholder is kept as written.
fn heal_provider_headers(provider: &mut Value, current: &mr_core::Provider) {
    let Some(headers) = provider.get_mut("headers").and_then(Value::as_object_mut) else {
        return;
    };
    for (name, stored) in &current.headers {
        if let Some(value) = headers.get_mut(name) {
            if value.as_str() == Some(fingerprint(stored).as_str()) {
                *value = Value::String(stored.clone());
            }
        }
    }
}

/// Store a plaintext account secret arriving in a request body, replacing it with `secret_ref`.
///
/// `null`, absent and empty `secret` all mean "the stored secret stays as it is" — that is how
/// the panel edits an account's label without re-sending its key. The write is journaled in `tx`,
/// so a later validation failure restores the previous secret (or removes this one).
async fn extract_account_secret(
    state: &AppState,
    tx: &mut VaultTx,
    account: &mut Value,
    provider_id: &str,
) -> ApiResult<()> {
    let Some(account) = account.as_object_mut() else {
        return Err(ApiError::bad(
            "invalid_body",
            "an account must be a JSON object",
        ));
    };
    // Account ids are server-generated (the panel's add-account form sends none). A client may
    // supply one, but an id is only filled in when absent — never overwritten.
    if account
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        account.insert("id".to_owned(), json!(AccountId::generate().to_string()));
    }
    let account_id = account
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let Some(secret) = account.remove("secret") else {
        return Ok(());
    };
    if secret.is_null() {
        return Ok(());
    }
    let Some(secret) = secret.as_str() else {
        return Err(ApiError::bad(
            "invalid_secret",
            "an account's `secret` must be a string or null",
        ));
    };
    if secret.is_empty() {
        return Ok(());
    }
    let vault_id = account_secret_id(provider_id, &account_id);
    state
        .tx_put(tx, &vault_id, secret)
        .await
        .map_err(ApiError::internal)?;
    account.insert("secret_ref".to_owned(), Value::String(vault_id));
    Ok(())
}

/// Heal + extract + parse for one client-key entry of a configuration body.
///
/// Never persists plaintext: a supplied `key` is hashed (or verified against the stored hash) and
/// dropped; a `key_hash` the GET view redacted is restored from the current configuration. A key
/// with neither hash nor plaintext is refused — creating a key is `POST /api/keys`, the only
/// place plaintext is ever *returned*.
async fn prepare_client_key(key: &mut Value, current: &[ClientKey]) -> ApiResult<()> {
    let Some(entry) = key.as_object_mut() else {
        return Err(ApiError::bad(
            "invalid_body",
            "a client key entry must be a JSON object",
        ));
    };
    if entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        entry.insert("id".to_owned(), json!(ClientKeyId::generate().to_string()));
    }
    if entry.get("key_hash").is_some_and(Value::is_null) {
        entry.remove("key_hash");
    }
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if !entry.contains_key("key_hash") {
        if let Some(stored) = current.iter().find(|k| k.id.as_str() == id) {
            entry.insert(
                "key_hash".to_owned(),
                Value::String(stored.key_hash.clone()),
            );
            if !entry.contains_key("prefix") {
                if let Some(prefix) = &stored.prefix {
                    entry.insert("prefix".to_owned(), Value::String(prefix.clone()));
                }
            }
        }
    }

    // The plaintext, if any, is consumed here and never written to the config file.
    let provided = entry.remove("key").filter(|value| !value.is_null());
    match provided {
        Some(Value::String(plain)) => {
            if plain.is_empty() {
                return Err(ApiError::bad(
                    "invalid_key",
                    format!("client key {id}: `key` must not be empty"),
                ));
            }
            let hash = sha256_hex(&plain);
            match entry.get("key_hash").and_then(Value::as_str) {
                Some(stored) if stored != hash => {
                    return Err(ApiError::bad(
                        "key_mismatch",
                        format!(
                            "client key {id}: the plaintext key provided does not match the \
                             stored hash"
                        ),
                    ));
                }
                Some(_) => {} // matches; the plaintext is discarded
                None => {
                    entry.insert("key_hash".to_owned(), Value::String(hash));
                    if !entry.contains_key("prefix") {
                        entry.insert("prefix".to_owned(), Value::String(key_prefix(&plain)));
                    }
                }
            }
        }
        Some(_) => {
            return Err(ApiError::bad(
                "invalid_key",
                format!("client key {id}: `key` must be a string or null"),
            ));
        }
        None => {
            if !entry.contains_key("key_hash") {
                return Err(ApiError::bad(
                    "missing_key_material",
                    format!(
                        "client key {id}: neither `key_hash` nor `key` was provided. Create \
                         client keys with POST /api/keys — the plaintext is shown exactly once — \
                         or include the plaintext in `key` so its hash can be derived"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Heal one provider body (header placeholders, account secrets) and parse it.
///
/// `current` is the running configuration: header healing compares against the stored provider
/// with the same id, or does nothing when there is none yet (provider creation).
async fn prepare_provider(
    state: &AppState,
    tx: &mut VaultTx,
    provider: &mut Value,
    current: &Config,
) -> ApiResult<mr_core::Provider> {
    let provider_id = provider
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(provider_id) = provider_id {
        if let Some(stored) = current
            .providers
            .iter()
            .find(|p| p.id.as_str() == provider_id)
        {
            heal_provider_headers(provider, stored);
        }
        if let Some(accounts) = provider.get_mut("accounts").and_then(Value::as_array_mut) {
            for account in accounts.iter_mut() {
                extract_account_secret(state, tx, account, &provider_id).await?;
            }
        }
    }
    serde_json::from_value(provider.clone()).map_err(|e| {
        ApiError::bad(
            "config_parse",
            format!("the provider could not be read: {e}"),
        )
    })
}

/// Heal a whole configuration body and parse it: control password, format version, provider
/// header placeholders, account secrets, client keys — in that order.
///
/// The caller owns the vault journal: any `Err` here means `tx` holds writes that must be rolled
/// back before the response is sent.
async fn prepare_config(
    state: &AppState,
    tx: &mut VaultTx,
    value: &mut Value,
    current: &Config,
) -> ApiResult<Config> {
    let Some(obj) = value.as_object_mut() else {
        return Err(ApiError::bad(
            "invalid_body",
            "the configuration must be a JSON object",
        ));
    };

    // An empty object describes no router at all. Rejecting it here (before any healing) means a
    // stray `PUT {}` can never reach `commit` and rewrite the file — the request that sent
    // nothing gets an error, and the bytes on disk stay exactly as they were.
    if obj.is_empty() {
        return Err(ApiError::bad(
            "invalid_body",
            "the configuration document is empty; send the configuration you want to store (the              panel always sends the whole document)",
        ));
    }

    // Sections the body omits are inherited from the stored configuration rather than reset to
    // defaults. mr-core marks `Config` struct-level `#[serde(default)]`, so a partial document
    // would otherwise silently wipe every section it forgot to mention — a curl user adding one
    // provider must not be able to erase the control password, the key list or the relay port.
    // (A section that *is* present is taken exactly as written; clearing `client_keys` means
    // sending `"client_keys": []`, and dropping `control.password_hash` from a present `control`
    // object still means "remove the password".)
    let stored = serde_json::to_value(current).map_err(|e| {
        ApiError::internal(format!("the running configuration could not be read: {e}"))
    })?;
    if let Some(stored_obj) = stored.as_object() {
        for (key, value) in stored_obj {
            if !obj.contains_key(key) {
                obj.insert(key.clone(), value.clone());
            }
        }
    }

    heal_control(obj, current);

    if let Some(providers) = obj.get_mut("providers").and_then(Value::as_array_mut) {
        for provider in providers.iter_mut() {
            let provider_id = provider
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let Some(provider_id) = provider_id else {
                continue;
            };
            if let Some(stored) = current
                .providers
                .iter()
                .find(|p| p.id.as_str() == provider_id)
            {
                heal_provider_headers(provider, stored);
            }
            if let Some(accounts) = provider.get_mut("accounts").and_then(Value::as_array_mut) {
                for account in accounts.iter_mut() {
                    extract_account_secret(state, tx, account, &provider_id).await?;
                }
            }
        }
    }

    if let Some(keys) = obj.get_mut("client_keys").and_then(Value::as_array_mut) {
        for key in keys.iter_mut() {
            prepare_client_key(key, &current.client_keys).await?;
        }
    }

    let config: Config = serde_json::from_value(value.clone()).map_err(|e| {
        ApiError::bad(
            "config_parse",
            format!("the configuration could not be read: {e}"),
        )
    })?;

    // Mirrors `PanelAuth::resolve`, for a configuration edited into the state startup would
    // refuse: a remotely bound control plane with no password is an unauthenticated
    // administration interface, and the correct answer is still "no".
    if config.control.password_hash.is_none()
        && is_remote_exposed(&config.control.host)
        && !config.control.allow_remote_without_password
    {
        return Err(ApiError::bad(
            "insecure_control_bind",
            "refusing to save a configuration that removes the panel password while the control \
             plane is bound to a non-loopback address; set control.allow_remote_without_password \
             only if you accept that anyone who can reach this port can administer the router",
        ));
    }

    Ok(config)
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

async fn health(State(state): State<AppState>) -> Json<Value> {
    let config = state.current_config().await;
    let snapshot = state.snapshot.current();
    let accounts: usize = config.providers.iter().map(|p| p.accounts.len()).sum();
    Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "snapshot": {
            "digest": format!("{:016x}", snapshot.digest()),
            "built_at": snapshot.built_at(),
        },
        "counts": {
            "providers": config.providers.len(),
            "combos": config.combos.len(),
            "aliases": config.aliases.len(),
            "keys": config.client_keys.len(),
            "accounts": accounts,
        },
        "password_set": config.control.password_hash.is_some(),
    }))
}

async fn session(State(state): State<AppState>, headers: HeaderMap) -> Json<Value> {
    let token = session_token(&headers);
    let auth = state.auth.read().await;
    Json(json!({
        "authenticated": auth.permits(token.as_deref()),
        "password_required": auth.password_required(),
    }))
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let secure = request_is_https(&headers, &uri);
    let auth = state.auth.read().await;
    if !auth.password_required() {
        // An open panel (loopback bind, no password): nothing to verify, nothing to issue.
        return Ok(Json(json!({"authenticated": true})).into_response());
    }
    let password = body
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match auth.login(password) {
        Some(token) => {
            let mut response =
                (StatusCode::OK, Json(json!({"authenticated": true}))).into_response();
            append_cookie(&mut response, &session_cookie(&token, secure))?;
            Ok(response)
        }
        None => Err(ApiError::unauthorized("the password is incorrect")),
    }
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Response> {
    let secure = request_is_https(&headers, &uri);
    if let Some(token) = session_token(&headers) {
        state.auth.read().await.sessions.revoke(&token);
    }
    let mut response = Json(json!({"authenticated": false})).into_response();
    append_cookie(&mut response, &clear_cookie(secure))?;
    Ok(response)
}

/// Set (first time) or rotate the panel password.
///
/// The response carries a fresh session cookie: rebuilding `PanelAuth` revokes every existing
/// session, and without a replacement cookie the panel would 401 on its very next request while
/// the operator is still looking at the dialog that "succeeded".
async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let new = match body.get("new") {
        Some(Value::String(new)) => new.clone(),
        Some(_) => {
            return Err(ApiError::bad(
                "invalid_password",
                "the new password must be a string",
            ));
        }
        None => {
            return Err(ApiError::bad(
                "invalid_password",
                "the new password is required",
            ));
        }
    };

    let auth = state.auth.read().await;
    if auth.password_required() {
        let current = body
            .get("current")
            .and_then(Value::as_str)
            .unwrap_or_default();
        // Verify before anything is rebuilt. The token `login` may issue here dies with the
        // rebuild below — that is intentional, not a leak.
        if auth.login(current).is_none() {
            return Err(ApiError::bad(
                "wrong_current_password",
                "the current password is incorrect",
            ));
        }
    }
    drop(auth);

    let hash = hash_password(&new).map_err(|e| ApiError::bad("invalid_password", e.to_string()))?;
    let (host, allow_remote) = {
        let config = state.current_config().await;
        (
            config.control.host.clone(),
            config.control.allow_remote_without_password,
        )
    };
    state
        .replace_password_hash(Some(hash), &host, allow_remote)
        .await
        .map_err(ApiError::internal)?;

    let token =
        state.auth.read().await.login(&new).ok_or_else(|| {
            ApiError::internal("the new password was stored but could not be used")
        })?;
    let secure = request_is_https(&headers, &uri);
    let mut response = Json(json!({"password_set": true})).into_response();
    append_cookie(&mut response, &session_cookie(&token, secure))?;
    Ok(response)
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

async fn get_config(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(redact_config(&state.current_config().await)?))
}

async fn get_config_raw(State(state): State<AppState>) -> ApiResult<Response> {
    let value = redact_config(&state.current_config().await)?;
    let mut text = serde_json::to_string_pretty(&value).map_err(|e| {
        ApiError::internal(format!("the configuration could not be serialised: {e}"))
    })?;
    text.push('\n');
    Ok(text_response(
        StatusCode::OK,
        "application/json; charset=utf-8",
        text,
    ))
}

async fn config_validate(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    Ok(Json(json!({
        "problems": config.validate(),
        "writable": config.is_writable(),
    })))
}

async fn put_config(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    commit_config_body(state, body).await
}

async fn put_config_raw(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    commit_config_body(state, body).await
}

/// Shared body of `PUT /api/config` and `PUT /api/config/raw`.
///
/// On success the redacted document comes back, so the panel's next GET agrees with what was
/// just stored.
async fn commit_config_body(state: AppState, mut value: Value) -> ApiResult<Json<Value>> {
    let current = state.current_config().await;
    let mut tx = VaultTx::default();

    let prepared = match prepare_config(&state, &mut tx, &mut value, &current).await {
        Ok(config) => config,
        Err(err) => {
            state.tx_rollback(tx).await;
            return Err(err);
        }
    };
    if let Err(err) = state.commit(prepared).await {
        state.tx_rollback(tx).await;
        return Err(err.into());
    }

    // The password may have been edited through this endpoint (the raw editor is a real editor).
    // A changed hash rebuilds the auth policy — which revokes every session, including this one;
    // the operator signs in again with the password they just wrote. An unchanged hash must not
    // rebuild, or every panel save would log the operator out.
    let final_config = state.current_config().await;
    if final_config.control.password_hash != current.control.password_hash {
        state
            .replace_password_hash(
                final_config.control.password_hash.clone(),
                &final_config.control.host,
                final_config.control.allow_remote_without_password,
            )
            .await
            .map_err(ApiError::internal)?;
    }

    Ok(Json(redact_config(&final_config)?))
}

// ---------------------------------------------------------------------------
// Ports and restart
// ---------------------------------------------------------------------------

async fn get_ports(State(state): State<AppState>) -> Json<Value> {
    let config = state.current_config().await;
    Json(ports_view(
        &config,
        state.running_relay_port(),
        state.running_control_port(),
        state.restart.is_pending(),
    ))
}

async fn put_ports(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let root = object_body(body, "ports")?;
    let mut next = state.current_config().await;

    for side in ["relay", "control"] {
        let Some(side_body) = root.get(side) else {
            continue;
        };
        let Some(side_body) = side_body.as_object() else {
            return Err(ApiError::bad(
                "invalid_ports",
                format!("{side} must be an object with optional host and port"),
            ));
        };
        // `running_port` in the body is an echo of the live port; only host and port are
        // settable — anything else in the object is ignored rather than rejected, so a panel
        // that echoes the whole section back keeps working.
        if let Some(host) = side_body.get("host") {
            let host = host
                .as_str()
                .map(str::trim)
                .filter(|host| !host.is_empty())
                .ok_or_else(|| {
                    ApiError::bad(
                        "invalid_ports",
                        format!("{side}.host must be a non-empty string"),
                    )
                })?;
            if side == "relay" {
                next.relay.host = host.to_owned();
            } else {
                next.control.host = host.to_owned();
            }
        }
        if let Some(port) = side_body.get("port") {
            let port = port
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p != 0)
                .ok_or_else(|| {
                    ApiError::bad(
                        "invalid_ports",
                        format!(
                            "{side}.port must be an integer between 1 and 65535; 0 would let the \
                             operating system choose a port this panel could not predict"
                        ),
                    )
                })?;
            let running = if side == "relay" {
                state.running_relay_port()
            } else {
                state.running_control_port()
            };
            if port < 1024 && port != running {
                return Err(ApiError::bad(
                    "privileged_port",
                    format!(
                        "{side}.port {port} is below 1024 and needs privileges this service does \
                         not have; the running port is {running}"
                    ),
                ));
            }
            if side == "relay" {
                next.relay.port = port;
            } else {
                next.control.port = port;
            }
        }
    }

    // Same port on overlapping hosts: one of the two binds would fail at the next restart, and
    // finding out then — with the panel down — is the worst possible moment.
    if next.relay.port == next.control.port && hosts_overlap(&next.relay.host, &next.control.host) {
        return Err(ApiError::bad(
            "port_collision",
            format!(
                "relay and control plane both want {}:{}; use a different port for one of them",
                next.relay.host, next.relay.port
            ),
        ));
    }

    // Same policy as startup: a remotely reachable control plane without a password is refused
    // now rather than at the restart that would apply it.
    if is_remote_exposed(&next.control.host)
        && next.control.password_hash.is_none()
        && !next.control.allow_remote_without_password
    {
        return Err(ApiError::bad(
            "insecure_control_bind",
            "refusing to bind the control plane to a non-loopback address without a panel \
             password; set the password first",
        ));
    }

    state.commit(next).await?;

    let config = state.current_config().await;
    Ok(Json(ports_view(
        &config,
        state.running_relay_port(),
        state.running_control_port(),
        state.restart.is_pending(),
    )))
}

/// Whether two bind addresses would collide: identical hosts, or either side a wildcard that
/// accepts traffic for the other.
fn hosts_overlap(a: &str, b: &str) -> bool {
    fn wildcard(host: &str) -> bool {
        matches!(host, "0.0.0.0" | "::" | "[::]" | "*")
    }
    a == b || wildcard(a) || wildcard(b)
}

async fn request_restart(State(state): State<AppState>) -> Json<Value> {
    state.restart.request();
    tracing::info!(
        "restart requested through the API; the process will exit after a graceful shutdown so a \
         supervisor can bring it back"
    );
    Json(json!({"restart_pending": true}))
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

async fn list_providers(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    let providers: Vec<Value> = config
        .providers
        .iter()
        .map(provider_view)
        .collect::<ApiResult<Vec<Value>>>()?;
    Ok(Json(json!({"providers": providers})))
}

/// `POST /api/providers`.
///
/// One deliberate waiver lives here, and only here. `Provider::validate` (via `Config::validate`)
/// unconditionally refuses a provider with no accounts, but `panel/README.md` documents the
/// panel's flow as *create first, add accounts in the drawer afterwards* — strict validation
/// would make the panel's primary creation dialog a permanent 400. The waiver is computed, not
/// hardcoded: the candidate configuration is validated twice, once as-is and once with a
/// syntactically perfect placeholder account appended to the new provider, and exactly the
/// problems that vanish under the placeholder (by construction, only "this provider has no
/// accounts") are dropped. No message text is matched — mr-core may reword its complaints and
/// this still waives the right thing. Every other problem is returned to the operator as usual,
/// and every other endpoint uses plain [`AppState::commit`].
async fn create_provider(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let mut provider_body = object_body(body, "provider")?;
    if provider_body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        provider_body.insert("id".to_owned(), json!(ProviderId::generate().to_string()));
    }
    let id = provider_body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let current = state.current_config().await;
    if current.providers.iter().any(|p| p.id.as_str() == id) {
        return Err(ApiError::conflict(format!(
            "a provider with id {id} already exists"
        )));
    }

    let mut tx = VaultTx::default();
    let mut provider_value = Value::Object(provider_body);
    let prepared = match prepare_provider(&state, &mut tx, &mut provider_value, &current).await {
        Ok(provider) => provider,
        Err(err) => {
            state.tx_rollback(tx).await;
            return Err(err);
        }
    };

    let mut next = current;
    next.providers.push(prepared);
    if let Err(err) = state
        .commit_with(next, |cfg| validate_new_provider(cfg, &id))
        .await
    {
        state.tx_rollback(tx).await;
        return Err(err.into());
    }

    let config = state.current_config().await;
    let created = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::internal("the saved provider could not be read back"))?;
    Ok((StatusCode::CREATED, Json(provider_view(created)?)).into_response())
}

/// See [`create_provider`]: the computed waiver for a provider that is *born* empty.
fn validate_new_provider(config: &Config, new_id: &str) -> Vec<String> {
    let as_is = config.validate();
    let mut probe_config = config.clone();
    if let Some(provider) = probe_config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == new_id)
    {
        let mut probe = Account::new("create-probe");
        probe.enabled = true;
        // The `secret_ref` satisfies the "has a credential" rule for every non-Bearer auth kind
        // as well, so the only problem this probe can remove is the empty-accounts one.
        probe.secret_ref = Some("create-probe".to_owned());
        provider.accounts.push(probe);
    }
    let with_probe = probe_config.validate();
    as_is
        .into_iter()
        .filter(|problem| !with_probe.contains(problem))
        .collect()
}

async fn update_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let mut provider_body = object_body(body, "provider")?;
    // The path says which provider this is; a body id (GET views carry one) must agree with it
    // rather than silently retarget the edit.
    provider_body.insert("id".to_owned(), Value::String(id.clone()));

    let current = state.current_config().await;
    if !current.providers.iter().any(|p| p.id.as_str() == id) {
        return Err(ApiError::not_found(format!("no provider with id {id}")));
    }

    let mut tx = VaultTx::default();
    let mut provider_value = Value::Object(provider_body);
    let prepared = match prepare_provider(&state, &mut tx, &mut provider_value, &current).await {
        Ok(provider) => provider,
        Err(err) => {
            state.tx_rollback(tx).await;
            return Err(err);
        }
    };

    // Any account the edit dropped takes its vault secret with it — but only once the new
    // configuration is safely committed; a rejected edit must not have deleted anything.
    let old_accounts: Vec<Account> = current
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .map(|p| p.accounts.clone())
        .unwrap_or_default();

    let mut next = current;
    let position = next
        .providers
        .iter()
        .position(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;
    let new_account_ids: Vec<String> = prepared.accounts.iter().map(|a| a.id.to_string()).collect();
    next.providers[position] = prepared;

    if let Err(err) = state.commit(next).await {
        state.tx_rollback(tx).await;
        return Err(err.into());
    }
    for account in old_accounts {
        let account_id = account.id.to_string();
        if !new_account_ids
            .iter()
            .any(|existing| existing == &account_id)
        {
            if let Some(vault_id) = account.secret_ref {
                state.delete_secret(&vault_id).await;
            }
        }
    }

    let config = state.current_config().await;
    let updated = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::internal("the saved provider could not be read back"))?;
    Ok(Json(provider_view(updated)?))
}

async fn delete_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let position = config
        .providers
        .iter()
        .position(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;
    config.providers.remove(position);
    state.commit(config).await?;

    // Cascade after the commit: the prefix covers account secrets, any provider-wide secret and
    // OAuth material, without this crate knowing the OAuth module's id scheme. Doing this before
    // the commit could leave a surviving config pointing at secrets that no longer exist.
    let removed = state
        .delete_secrets_with_prefix(&format!("provider/{id}/"))
        .await;
    tracing::debug!(provider = %id, secrets = removed, "provider deleted; vault cleaned");
    Ok(Json(json!({"deleted": id})))
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

async fn create_account(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let mut account_body = object_body(body, "account")?;
    // Server-generated, always: the panel sends `{label, identity, secret}` and a client-chosen
    // id could collide with an existing account of the same provider.
    account_body.insert("id".to_owned(), json!(AccountId::generate().to_string()));
    let account_id = account_body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let mut tx = VaultTx::default();
    let mut account_value = Value::Object(account_body);
    if let Err(err) = extract_account_secret(&state, &mut tx, &mut account_value, &id).await {
        state.tx_rollback(tx).await;
        return Err(err);
    }
    let parsed: Account = match serde_json::from_value(account_value) {
        Ok(account) => account,
        Err(err) => {
            state.tx_rollback(tx).await;
            return Err(ApiError::bad(
                "config_parse",
                format!("the account could not be read: {err}"),
            ));
        }
    };

    let mut next = state.current_config().await;
    let position = match next.providers.iter().position(|p| p.id.as_str() == id) {
        Some(position) => position,
        None => {
            state.tx_rollback(tx).await;
            return Err(ApiError::not_found(format!("no provider with id {id}")));
        }
    };
    next.providers[position].accounts.push(parsed);
    if let Err(err) = state.commit(next).await {
        state.tx_rollback(tx).await;
        return Err(err.into());
    }

    let config = state.current_config().await;
    let created = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .and_then(|p| p.accounts.iter().find(|a| a.id.as_str() == account_id))
        .ok_or_else(|| ApiError::internal("the saved account could not be read back"))?;
    let view = serde_json::to_value(created)
        .map_err(|e| ApiError::internal(format!("the account could not be serialised: {e}")))?;
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn update_account(
    State(state): State<AppState>,
    Path((id, account_id)): Path<(String, String)>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let mut account_body = object_body(body, "account")?;
    account_body.insert("id".to_owned(), Value::String(account_id.clone()));

    let mut tx = VaultTx::default();
    let mut account_value = Value::Object(account_body);
    if let Err(err) = extract_account_secret(&state, &mut tx, &mut account_value, &id).await {
        state.tx_rollback(tx).await;
        return Err(err);
    }
    let parsed: Account = match serde_json::from_value(account_value) {
        Ok(account) => account,
        Err(err) => {
            state.tx_rollback(tx).await;
            return Err(ApiError::bad(
                "config_parse",
                format!("the account could not be read: {err}"),
            ));
        }
    };

    let mut next = state.current_config().await;
    if let Err(err) = replace_account(&mut next, &id, &account_id, parsed) {
        state.tx_rollback(tx).await;
        return Err(err);
    }
    if let Err(err) = state.commit(next).await {
        state.tx_rollback(tx).await;
        return Err(err.into());
    }

    let config = state.current_config().await;
    let view = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .and_then(|p| p.accounts.iter().find(|a| a.id.as_str() == account_id))
        .ok_or_else(|| ApiError::internal("the saved account could not be read back"))?;
    Ok(Json(serde_json::to_value(view).map_err(|e| {
        ApiError::internal(format!("the account could not be serialised: {e}"))
    })?))
}

fn replace_account(
    config: &mut Config,
    provider_id: &str,
    account_id: &str,
    account: Account,
) -> ApiResult<()> {
    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == provider_id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {provider_id}")))?;
    let slot = provider
        .accounts
        .iter_mut()
        .find(|a| a.id.as_str() == account_id)
        .ok_or_else(|| ApiError::not_found(format!("no account with id {account_id}")))?;
    *slot = account;
    Ok(())
}

async fn delete_account(
    State(state): State<AppState>,
    Path((id, account_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;
    let position = provider
        .accounts
        .iter()
        .position(|a| a.id.as_str() == account_id)
        .ok_or_else(|| ApiError::not_found(format!("no account with id {account_id}")))?;
    let removed = provider.accounts.remove(position);
    state.commit(config).await?;

    // After the commit, so a rejected edit can never have destroyed the credential.
    if let Some(vault_id) = removed.secret_ref {
        state.delete_secret(&vault_id).await;
    }
    Ok(Json(json!({"deleted": account_id})))
}

// ---------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------

async fn upsert_model(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let model_body = object_body(body, "model")?;
    let model_id = model_body
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|mid| !mid.is_empty())
        .ok_or_else(|| {
            ApiError::bad(
                "invalid_model",
                "a model entry needs a non-empty `id` field",
            )
        })?;
    let entry: ModelEntry = serde_json::from_value(Value::Object(model_body)).map_err(|e| {
        ApiError::bad(
            "config_parse",
            format!("the model entry could not be read: {e}"),
        )
    })?;

    let mut config = state.current_config().await;
    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;
    let existed = provider
        .models
        .iter()
        .position(|m| m.id.as_str() == model_id);
    let status = match existed {
        Some(position) => {
            provider.models[position] = entry;
            StatusCode::OK
        }
        None => {
            provider.models.push(entry);
            StatusCode::CREATED
        }
    };
    state.commit(config).await?;

    let config = state.current_config().await;
    let view = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .and_then(|p| p.models.iter().find(|m| m.id.as_str() == model_id))
        .ok_or_else(|| ApiError::internal("the saved model could not be read back"))?;
    Ok((
        status,
        Json(
            serde_json::to_value(view).map_err(|e| {
                ApiError::internal(format!("the model could not be serialised: {e}"))
            })?,
        ),
    )
        .into_response())
}

async fn delete_model(
    State(state): State<AppState>,
    Path((id, model_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;
    let position = provider
        .models
        .iter()
        .position(|m| m.id.as_str() == model_id)
        .ok_or_else(|| ApiError::not_found(format!("no model with id {model_id} on {id}")))?;
    provider.models.remove(position);
    state.commit(config).await?;
    Ok(Json(json!({"deleted": model_id})))
}

/// End-to-end model discovery against `GET {base_url}/models`.
///
/// No request body is expected — the panel's sync button sends none — so this handler takes no
/// body extractor; a required `Json` extractor would answer 415 to the panel's own call.
async fn sync_models(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    let provider = config
        .providers
        .iter()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;

    let base_url = provider.base_url.clone();
    let auth_kind = provider.auth.clone();

    // Pick the credential the way routing would: an enabled account's secret first, then any
    // account's. The vault lock is taken for the read and released before the network call —
    // holding it across a slow upstream would stall every other mutation.
    let secret_ref = provider
        .accounts
        .iter()
        .filter(|a| a.enabled)
        .find(|a| a.secret_ref.is_some())
        .or_else(|| provider.accounts.iter().find(|a| a.secret_ref.is_some()))
        .and_then(|a| a.secret_ref.clone());
    let secret = match secret_ref {
        Some(vault_id) => {
            let vault = state.vault.lock().await;
            vault
                .get(&vault_id)
                .map_err(|e| ApiError::internal(format!("could not read the vault: {e}")))?
        }
        None => None,
    };
    drop(config);

    let mut url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut headers: Vec<(String, String)> = Vec::new();
    match &auth_kind {
        AuthKind::Bearer => {
            if let Some(secret) = &secret {
                headers.push(("authorization".to_owned(), format!("Bearer {secret}")));
            }
            // No secret: some providers serve `/models` without a credential. If the upstream
            // disagrees, the honest upstream error below says so.
        }
        AuthKind::ApiKeyHeader { header } => {
            let Some(secret) = &secret else {
                return Err(ApiError::bad(
                    "no_credential",
                    "this provider authenticates with an API-key header but no account has a \
                     stored secret; add the key to an account first",
                ));
            };
            headers.push((header.clone(), secret.clone()));
        }
        AuthKind::Header { name, prefix } => {
            let Some(secret) = &secret else {
                return Err(ApiError::bad(
                    "no_credential",
                    "this provider authenticates with a custom header but no account has a \
                     stored secret; add the key to an account first",
                ));
            };
            headers.push((name.clone(), format!("{prefix}{secret}")));
        }
        AuthKind::QueryParam { param } => {
            let Some(secret) = &secret else {
                return Err(ApiError::bad(
                    "no_credential",
                    "this provider authenticates with a query parameter but no account has a \
                     stored secret; add the key to an account first",
                ));
            };
            url = format!("{url}?{param}={}", urlencoding::encode(secret));
        }
        // OAuth credentials are minted per request by the relay; the conductor has no refresh
        // path for them yet. Try unauthenticated and report honestly if the upstream objects.
        AuthKind::OAuth { .. } => {}
    }

    let discovered = fetch_model_list(&url, &headers).await?;

    // Merge. `Manual` entries are the operator's own words and are never overwritten. `Synced`
    // and `Inferred` entries are replaced by what upstream says.
    let mut config = state.current_config().await;
    let provider = config
        .providers
        .iter_mut()
        .find(|p| p.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no provider with id {id}")))?;
    let now = Millis::now();
    let mut added = 0usize;
    let mut updated = 0usize;
    let mut kept = 0usize;

    for (model_id, context_length, max_output_tokens) in discovered {
        match provider
            .models
            .iter_mut()
            .find(|m| m.id.as_str() == model_id)
        {
            Some(entry) => {
                if entry.source == ModelSource::Manual {
                    kept += 1;
                    continue;
                }
                let changed = entry.context_length != context_length
                    || entry.max_output_tokens != max_output_tokens;
                entry.context_length = context_length.or(entry.context_length);
                entry.max_output_tokens = max_output_tokens.or(entry.max_output_tokens);
                entry.source = ModelSource::Synced;
                entry.synced_at = Some(now);
                if changed {
                    updated += 1;
                } else {
                    kept += 1;
                }
            }
            None => {
                provider.models.push(ModelEntry {
                    id: model_id,
                    source: ModelSource::Synced,
                    label: None,
                    context_length,
                    max_output_tokens,
                    capabilities: Vec::new(),
                    input_price_per_mtok: None,
                    output_price_per_mtok: None,
                    synced_at: Some(now),
                    hidden: false,
                });
                added += 1;
            }
        }
    }
    let total = provider.models.len();
    provider.models_synced_at = Some(now);
    // `provider` is a borrow of `config`; it simply goes out of scope here (an explicit
    // `drop` on the reference would be a no-op — and clippy says so).

    state.commit(config).await?;

    tracing::info!(provider = %id, added, updated, kept, "model discovery completed");
    Ok(Json(json!({
        "added": added,
        "updated": updated,
        "kept": kept,
        "total": total,
        "synced_at": now,
    })))
}

/// How long model discovery may take per phase (connect+request, then body).
const SYNC_TIMEOUT: Duration = Duration::from_secs(20);

type SyncClient = LegacyClient<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// The HTTP client for discovery: webpki roots (no system-store dependency), ALPN `h2` then
/// `http/1.1`. Construction mirrors `mr-core/src/oauth/flow.rs` and
/// `manarouter-relay/src/upstream.rs` — the same shape everywhere this repo speaks HTTPS.
fn sync_client() -> Result<SyncClient, String> {
    // rustls 0.23 needs its process-wide provider installed once; `ring` matches the features
    // this crate already compiles. `install_default` is idempotent — a later failure means a
    // different provider already won, which is acceptable as long as TLS works process-wide.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut http = HttpConnector::new();
    http.set_nodelay(true);
    http.enforce_http(false);
    let https = HttpsConnector::from((http, std::sync::Arc::new(tls)));
    Ok(LegacyClient::builder(TokioExecutor::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(2)
        .http2_only(false)
        .build(https))
}

/// `GET {base_url}`, then parse the model list out of the answer.
///
/// Recognised shapes: OpenAI/Anthropic `{"data": [{"id": …}]}` and Google's
/// `{"models": [{"name": "models/…", "inputTokenLimit": …}]}`. Anything else is an upstream
/// error surfaced with its status and a truncated body — never an invented model list.
async fn fetch_model_list(
    url: &str,
    request_headers: &[(String, String)],
) -> ApiResult<Vec<(String, Option<u32>, Option<u32>)>> {
    let client = sync_client().map_err(ApiError::internal)?;
    let mut builder = hyper::Request::builder()
        .method(Method::GET)
        .uri(url)
        .header(header::ACCEPT, "application/json");
    for (name, value) in request_headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let request = builder
        .body(Full::new(Bytes::new()))
        .map_err(|e| ApiError::internal(format!("could not build the discovery request: {e}")))?;

    let response = match tokio::time::timeout(SYNC_TIMEOUT, client.request(request)).await {
        Ok(Ok(response)) => response,
        Ok(Err(e)) => {
            return Err(ApiError::upstream(
                StatusCode::BAD_GATEWAY,
                format!("could not reach {url}: {e}"),
            ));
        }
        Err(_) => {
            return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                format!("{url} did not answer within {}s", SYNC_TIMEOUT.as_secs()),
            ));
        }
    };
    let status = response.status();
    let collected = match tokio::time::timeout(SYNC_TIMEOUT, response.into_body().collect()).await {
        Ok(Ok(collected)) => collected,
        Ok(Err(e)) => {
            return Err(ApiError::upstream(
                StatusCode::BAD_GATEWAY,
                format!("the response body from {url} could not be read: {e}"),
            ));
        }
        Err(_) => {
            return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "upstream_timeout",
                format!(
                    "the response body from {url} did not finish within {}s",
                    SYNC_TIMEOUT.as_secs()
                ),
            ));
        }
    };
    let bytes = collected.to_bytes();

    if !status.is_success() {
        let body = String::from_utf8_lossy(&bytes);
        let snippet: String = body.chars().take(512).collect();
        return Err(ApiError::upstream(
            StatusCode::BAD_GATEWAY,
            format!("the model list request failed with {status}: {snippet}"),
        ));
    }

    parse_model_list(&bytes).ok_or_else(|| {
        ApiError::upstream(
            StatusCode::BAD_GATEWAY,
            format!(
                "{url} answered 2xx but the body was neither an OpenAI/Anthropic `data` list nor \
                 a Google `models` list"
            ),
        )
    })
}

/// One discoverable model as its provider described it: `(id, context_length, max_output_tokens)`.
///
/// A named alias rather than the tuple inline, because the tuple appears in the signature, in
/// the push sites below, and in the caller that merges it into the catalogue.
type ModelSummary = (String, Option<u32>, Option<u32>);

/// One `ModelSummary` per discoverable model.
fn parse_model_list(bytes: &[u8]) -> Option<Vec<ModelSummary>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let as_u32 = |value: Option<&Value>| -> Option<u32> {
        value
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
    };
    let mut out = Vec::new();
    if let Some(data) = value.get("data").and_then(Value::as_array) {
        for model in data {
            let Some(id) = model.get("id").and_then(Value::as_str) else {
                continue;
            };
            out.push((
                id.to_owned(),
                as_u32(model.get("context_length")),
                as_u32(model.get("max_output_tokens")),
            ));
        }
        return Some(out);
    }
    if let Some(models) = value.get("models").and_then(Value::as_array) {
        for model in models {
            let Some(raw) = model.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Google returns `models/gemini-…`; the router's model ids carry no prefix.
            let id = raw.strip_prefix("models/").unwrap_or(raw);
            out.push((
                id.to_owned(),
                as_u32(model.get("inputTokenLimit")),
                as_u32(model.get("outputTokenLimit")),
            ));
        }
        return Some(out);
    }
    None
}

// ---------------------------------------------------------------------------
// Combos
// ---------------------------------------------------------------------------

async fn list_combos(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    Ok(Json(json!({"combos": config.combos})))
}

async fn create_combo(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let mut combo_body = object_body(body, "combo")?;
    if combo_body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        combo_body.insert("id".to_owned(), json!(ComboId::generate().to_string()));
    }
    let id = combo_body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let combo: Combo = serde_json::from_value(Value::Object(combo_body))
        .map_err(|e| ApiError::bad("config_parse", format!("the combo could not be read: {e}")))?;

    let mut next = state.current_config().await;
    if next.combos.iter().any(|c| c.id.as_str() == id) {
        return Err(ApiError::conflict(format!(
            "a combo with id {id} already exists"
        )));
    }
    check_unique_combo_name(&next, &combo.name, None)?;
    next.combos.push(combo);
    state.commit(next).await?;

    let config = state.current_config().await;
    let created = config
        .combos
        .iter()
        .find(|c| c.id.as_str() == id)
        .ok_or_else(|| ApiError::internal("the saved combo could not be read back"))?;
    Ok((
        StatusCode::CREATED,
        Json(
            serde_json::to_value(created).map_err(|e| {
                ApiError::internal(format!("the combo could not be serialised: {e}"))
            })?,
        ),
    )
        .into_response())
}

async fn update_combo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let mut combo_body = object_body(body, "combo")?;
    combo_body.insert("id".to_owned(), Value::String(id.clone()));
    let combo: Combo = serde_json::from_value(Value::Object(combo_body))
        .map_err(|e| ApiError::bad("config_parse", format!("the combo could not be read: {e}")))?;

    let mut next = state.current_config().await;
    let position = next
        .combos
        .iter()
        .position(|c| c.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no combo with id {id}")))?;
    check_unique_combo_name(&next, &combo.name, Some(&id))?;
    next.combos[position] = combo;
    state.commit(next).await?;

    let config = state.current_config().await;
    let updated = config
        .combos
        .iter()
        .find(|c| c.id.as_str() == id)
        .ok_or_else(|| ApiError::internal("the saved combo could not be read back"))?;
    Ok(Json(serde_json::to_value(updated).map_err(|e| {
        ApiError::internal(format!("the combo could not be serialised: {e}"))
    })?))
}

/// The callable name *is* the model name clients will pass — two combos answering to one name
/// would resolve silently to whichever the router picked, so uniqueness is enforced here rather
/// than left to a runtime coin flip.
fn check_unique_combo_name(config: &Config, name: &str, skip_id: Option<&str>) -> ApiResult<()> {
    let duplicate = config
        .combos
        .iter()
        .any(|c| c.name == name && skip_id.map(|skip| c.id.as_str() != skip).unwrap_or(true));
    if duplicate {
        Err(ApiError::conflict(format!(
            "another combo is already callable as {name:?}"
        )))
    } else {
        Ok(())
    }
}

async fn delete_combo(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let position = config
        .combos
        .iter()
        .position(|c| c.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no combo with id {id}")))?;
    config.combos.remove(position);
    state.commit(config).await?;
    Ok(Json(json!({"deleted": id})))
}

// ---------------------------------------------------------------------------
// Aliases
// ---------------------------------------------------------------------------

async fn list_aliases(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    Ok(Json(json!({"aliases": config.aliases})))
}

async fn create_alias(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let mut alias_body = object_body(body, "alias")?;
    // `generated` is reserved for aliases the router mints itself; an operator-created alias is
    // always manual, whatever the body claimed.
    alias_body.insert("generated".to_owned(), Value::Bool(false));
    let from = alias_body
        .get("from")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::bad("invalid_alias", "an alias needs a non-empty `from` name"))?;
    let alias: mr_core::Alias = serde_json::from_value(Value::Object(alias_body))
        .map_err(|e| ApiError::bad("config_parse", format!("the alias could not be read: {e}")))?;

    let mut next = state.current_config().await;
    if next.aliases.iter().any(|a| a.from == from) {
        return Err(ApiError::conflict(format!(
            "an alias named {from:?} already exists"
        )));
    }
    next.aliases.push(alias);
    state.commit(next).await?;

    let config = state.current_config().await;
    let created = config
        .aliases
        .iter()
        .find(|a| a.from == from)
        .ok_or_else(|| ApiError::internal("the saved alias could not be read back"))?;
    Ok((
        StatusCode::CREATED,
        Json(
            serde_json::to_value(created).map_err(|e| {
                ApiError::internal(format!("the alias could not be serialised: {e}"))
            })?,
        ),
    )
        .into_response())
}

async fn delete_alias(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let position = config
        .aliases
        .iter()
        .position(|a| a.from == name)
        .ok_or_else(|| ApiError::not_found(format!("no alias named {name:?}")))?;
    config.aliases.remove(position);
    state.commit(config).await?;
    Ok(Json(json!({"deleted": name})))
}

// ---------------------------------------------------------------------------
// Client keys
// ---------------------------------------------------------------------------

async fn list_keys(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let config = state.current_config().await;
    let keys: Vec<Value> = config
        .client_keys
        .iter()
        .map(key_view)
        .collect::<ApiResult<Vec<Value>>>()?;
    Ok(Json(json!({"keys": keys})))
}

async fn create_key(
    State(state): State<AppState>,
    JsonBody(body): JsonBody,
) -> ApiResult<Response> {
    let mut key_body = object_body(body, "key")?;
    // Minting happens here, in one place: the plaintext exists only in this response, the config
    // receives only its hash and prefix. Any `key`/`key_hash` the body carried is discarded.
    key_body.remove("key");
    key_body.remove("key_hash");
    key_body.remove("prefix");
    if key_body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .is_none()
    {
        key_body.insert("id".to_owned(), json!(ClientKeyId::generate().to_string()));
    }
    let id = key_body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    let plaintext = generate_client_key();
    key_body.insert("key_hash".to_owned(), Value::String(sha256_hex(&plaintext)));
    key_body.insert("prefix".to_owned(), Value::String(key_prefix(&plaintext)));

    let key: ClientKey = serde_json::from_value(Value::Object(key_body)).map_err(|e| {
        ApiError::bad(
            "config_parse",
            format!("the client key could not be read: {e}"),
        )
    })?;

    let mut next = state.current_config().await;
    if next.client_keys.iter().any(|k| k.id.as_str() == id) {
        return Err(ApiError::conflict(format!(
            "a client key with id {id} already exists"
        )));
    }
    next.client_keys.push(key);
    state.commit(next).await?;

    let config = state.current_config().await;
    let created = config
        .client_keys
        .iter()
        .find(|k| k.id.as_str() == id)
        .ok_or_else(|| ApiError::internal("the saved client key could not be read back"))?;
    let mut view = key_view(created)?;
    if let Some(obj) = view.as_object_mut() {
        // The one and only time the plaintext is ever emitted.
        obj.insert("key".to_owned(), Value::String(plaintext));
    }
    Ok((StatusCode::CREATED, Json(view)).into_response())
}

async fn update_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    JsonBody(body): JsonBody,
) -> ApiResult<Json<Value>> {
    let mut key_body = object_body(body, "key")?;
    key_body.insert("id".to_owned(), Value::String(id.clone()));

    let current = state.current_config().await;
    if !current.client_keys.iter().any(|k| k.id.as_str() == id) {
        return Err(ApiError::not_found(format!("no client key with id {id}")));
    }

    // Client keys have no vault material — only a hash and a prefix live in the config — so this
    // edit needs no journal; a rejected edit simply never commits.
    let key_value = Value::Object(key_body);
    let key: ClientKey = serde_json::from_value(key_value).map_err(|err| {
        ApiError::bad(
            "config_parse",
            format!("the client key could not be read: {err}"),
        )
    })?;

    let mut next = current;
    let position = next
        .client_keys
        .iter()
        .position(|k| k.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no client key with id {id}")))?;
    next.client_keys[position] = key;
    state.commit(next).await?;

    let config = state.current_config().await;
    let updated = config
        .client_keys
        .iter()
        .find(|k| k.id.as_str() == id)
        .ok_or_else(|| ApiError::internal("the saved client key could not be read back"))?;
    Ok(Json(key_view(updated)?))
}

async fn delete_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut config = state.current_config().await;
    let position = config
        .client_keys
        .iter()
        .position(|k| k.id.as_str() == id)
        .ok_or_else(|| ApiError::not_found(format!("no client key with id {id}")))?;
    config.client_keys.remove(position);
    state.commit(config).await?;

    // After the commit, as everywhere: the config no longer references the id, so the vault
    // entry can only be an orphan now.
    state.delete_secret(&client_key_secret_id(&id)).await;
    Ok(Json(json!({"deleted": id})))
}

// ---------------------------------------------------------------------------
// Documentation
// ---------------------------------------------------------------------------

async fn list_docs(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    Ok(Json(
        json!({"docs": list_doc_files(state.dirs.docs.as_deref())?}),
    ))
}

/// Enumerate `*.md` one level under the docs directory as `{name, title}`.
///
/// `name` is the file stem (what `GET /api/docs/{name}` takes); `title` is the first markdown
/// heading, falling back to the name. A missing directory is an empty list, not an error: the
/// panel renders an honest "no documentation installed" state.
fn list_doc_files(dir: Option<&FsPath>) -> ApiResult<Vec<Value>> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let mut docs: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let title = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| {
                text.lines().find_map(|line| {
                    let line = line.trim();
                    line.strip_prefix("# ")
                        .map(|heading| heading.trim().to_owned())
                })
            })
            .unwrap_or_else(|| name.to_owned());
        docs.push((name.to_owned(), title));
    }
    docs.sort();
    Ok(docs
        .into_iter()
        .map(|(name, title)| json!({"name": name, "title": title}))
        .collect())
}

async fn get_doc(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> ApiResult<Response> {
    let name = name.map_err(|_| ApiError::not_found("no such document"))?.0;
    let path = resolve_doc(state.dirs.docs.as_deref(), &name)?;
    let text = std::fs::read_to_string(&path).map_err(|e| {
        ApiError::not_found(format!("the document {name:?} could not be read: {e}"))
    })?;
    Ok(text_response(
        StatusCode::OK,
        "text/markdown; charset=utf-8",
        text,
    ))
}

/// Turn a request's `{name}` capture into a file **inside** the docs directory, or refuse.
///
/// Refusals are all a bare 404 with no detail: a 403-vs-404 difference would tell an attacker
/// which probe got them closest. The capture arrives already percent-decoded from axum's `Path`
/// extractor, so `..%2f..%2fetc` is seen here as `../../etc` — the `..` check is what catches it.
/// Symlinks are covered too: both the candidate and its parent are canonicalised and must stay
/// under the canonicalised docs root, so a `docs/evil.md -> /etc/shadow` link cannot read
/// anything.
fn resolve_doc(dir: Option<&FsPath>, name: &str) -> ApiResult<std::path::PathBuf> {
    fn refuse() -> ApiError {
        ApiError::not_found("no such document")
    }

    let dir = dir.ok_or_else(refuse)?;
    if name.is_empty()
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains("..")
        || name.contains('\\')
        || name.contains('\0')
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return Err(refuse());
    }

    let file_name = if name.ends_with(".md") {
        name.to_owned()
    } else {
        format!("{name}.md")
    };
    let candidate = dir.join(&file_name);
    let root = std::fs::canonicalize(dir).map_err(|_| refuse())?;
    let resolved = std::fs::canonicalize(&candidate).map_err(|_| refuse())?;
    let parent = resolved.parent().ok_or_else(refuse)?;
    if !resolved.starts_with(&root) || !parent.starts_with(&root) {
        return Err(refuse());
    }
    if !resolved.is_file() {
        return Err(refuse());
    }
    Ok(resolved)
}

// ---8<--- tests appended below ---
