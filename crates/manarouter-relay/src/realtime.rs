//! `GET /v1/realtime`: the OpenAI realtime WebSocket surface.
//!
//! # What this module is
//!
//! A realtime session is a WebSocket that carries JSON events in both directions for as long as the
//! conversation lasts — an audio stream in one direction and events about it in the other. The
//! relay's job is deliberately narrow: authenticate the client key, resolve `?model=`, open **one**
//! upstream socket with the provider's own credential, and then move frames between the two sockets
//! without looking inside them. Nothing here parses an event, buffers a message, or reorders a
//! frame. The realtime protocol is stateful at both ends, so a proxy that reassembled messages would
//! break the stream it is carrying, and one that timed an idle socket out would hang up during the
//! silence between utterances.
//!
//! # Why the upstream socket is opened before the upgrade is answered
//!
//! `WebSocketUpgrade::on_upgrade` does not send the `101 Switching Protocols` itself: it returns a
//! [`Response`] that hyper turns into the handshake once the handler has returned. Everything that
//! can fail therefore happens *before* that response is built — the key, the model, the credential,
//! the URL, and the upstream handshake — so a refusal is an ordinary HTTP status the client can
//! read (`404`, `501`, `502`) rather than a socket that opens and immediately dies. Once the pump is
//! running, failures can only be reported on the wire, and they are: a close frame with the code and
//! reason that caused it, plus a `tracing::warn!` naming the provider.
//!
//! # What is not carried through
//!
//! * The client's own headers. The relay's client key lives in `Authorization`, and forwarding
//!   client headers to the provider would send that key upstream. Only the provider's configured
//!   headers, its credential, and the client's requested WebSocket subprotocol are sent.
//! * The provider's `proxy` setting. `tokio_tungstenite::connect_async` has no proxy support; a
//!   provider that needs one cannot be reached over this route, and the connect failure says so.
//! * `route_overrides` keys other than `realtime`. The route is a WebSocket, not a
//!   [`mr_core::dialect::RouteKind`], so it has no `as_config_key()`; the override is keyed by the
//!   literal `"realtime"` and an operator who writes a raw path (a key starting with `/`) gets a
//!   warning rather than a silent miss.

use std::collections::HashMap;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode, Uri};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use futures_util::{SinkExt, StreamExt};
use mr_core::dialect::{AuthKind, Capability, Dialect};
use mr_core::snapshot::{AccountPlan, ProviderPlan, Resolution, RoutingSnapshot, StepPlan};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as UpstreamCloseFrame;
use tokio_tungstenite::tungstenite::Message as UpstreamMessage;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

use crate::select;
use crate::server::{self, AppState};

/// The upstream socket, at whichever transport the base URL selected.
type UpstreamSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The path the realtime socket lives at when a provider has not overridden it.
///
/// OpenAI serves `GET /v1/realtime?model=…`; every OpenAI-shaped realtime provider keeps that
/// shape, which is why the default is a constant and not a per-provider field.
const DEFAULT_REALTIME_PATH: &str = "/v1/realtime";

/// The query parameter that names the model, and the one key the relay consumes from the client's
/// query string. Everything else is forwarded untouched.
const MODEL_PARAM: &str = "model";

/// The version an OpenAI-shaped API puts in front of its paths.
///
/// `server::dialect_version` is private to that module; this is the one entry of it this route can
/// reach, because a realtime socket exists only on the OpenAI surface.
const OPENAI_API_VERSION: &str = "v1";

/// The router fragment `server::router` merges.
///
/// The path is the full one the client asks for, `/v1/realtime`, because the relay's router mounts
/// complete paths (`/v1/chat/completions`, `/v1/messages`, …) rather than a versioned nest — a bare
/// `/realtime` here would answer a path no OpenAI client ever requests. Only this one path is
/// registered: `/v1/realtime/client_secrets` and `/v1/realtime/calls` are ordinary HTTP routes that
/// `server.rs` owns, and `Router::merge` panics on a path registered twice.
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/realtime", get(realtime))
}

/// `GET /v1/realtime?model=<provider/model>`.
async fn realtime(
    State(app): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> Response {
    // The client key is checked exactly as the HTTP surfaces check it, by the same function: a
    // WebSocket has no body to read, and `authorize_batch_request` reads only headers and the
    // relay's key table, so it is the whole of the authentication step. `None` means the presented
    // key — or, under `no_key_required`, its absence — is acceptable.
    if let Some(response) = server::authorize_batch_request(&app, &headers) {
        return response;
    }

    let query = uri.query().unwrap_or_default();
    let (requested, passthrough) = split_query(query);
    let Some(requested) = requested else {
        return server::error_response(
            StatusCode::BAD_REQUEST,
            Dialect::OpenAi,
            "invalid_request_error",
            "the realtime route needs a model query parameter, as in \
             \"/v1/realtime?model=provider/model\"",
        );
    };

    // One view of the table and one of the cooldown map, so the selection and the credential loop
    // below cannot see two different generations. The cooldown map is copied rather than held as a
    // guard: this function awaits, and a guard would pin the whole generation for the lifetime of
    // the handshake. The map is empty in the ordinary case, where the copy is free.
    let table = app.relay.table();
    let health = app.health.snapshot();
    let now_ms = mr_core::ids::Millis::now().as_millis() as u64;

    let (step, provider) = match select_step(&table.snapshot, &requested, &health, now_ms) {
        Selection::Found { step, provider } => (step, provider),
        Selection::NotFound { reason } => {
            return server::error_response(
                StatusCode::NOT_FOUND,
                Dialect::OpenAi,
                "model_not_found",
                &reason,
            );
        }
        Selection::NoRealtime => {
            return server::error_response(
                StatusCode::NOT_FOUND,
                Dialect::OpenAi,
                "unsupported_endpoint",
                &format!(
                    "model \"{requested}\" is not served by any provider that declares the \
                     \"realtime\" capability"
                ),
            );
        }
        // The provider can serve the model, just not as a WebSocket, and no amount of retrying
        // changes that: the SPEC asks for a refusal that names the reason, not a 404 that would
        // read as a typo in the model name.
        Selection::Dialect { step, provider } => {
            return server::error_response(
                StatusCode::NOT_IMPLEMENTED,
                Dialect::OpenAi,
                "unsupported_endpoint",
                &format!(
                    "provider `{}` serves model \"{}\" over the \"{}\" dialect, and the realtime \
                     WebSocket route speaks only the OpenAI shape",
                    provider.id,
                    step.model,
                    step.dialect.as_str()
                ),
            );
        }
    };

    // Copied out of the snapshot so the guards can be dropped before the first await; a step and a
    // provider are plain data by design, which is what makes this cheap.
    let step = step.clone();
    let provider = provider.clone();
    let (path, is_operator_path) = realtime_path(&provider);
    drop(table);

    let subprotocol = headers.get(header::SEC_WEBSOCKET_PROTOCOL).cloned();

    // The upstream handshake is the slow, fallible part, and it happens here — while a status code
    // is still possible. Each account of the step gets its own attempt: the first credential may be
    // missing or the first host may be down, and a provider that holds several accounts must not
    // fail a session the second one can serve.
    let mut missing = 0usize;
    let mut unusable = 0usize;
    let mut last_error: Option<String> = None;
    let mut connected: Option<(UpstreamSocket, Option<String>)> = None;
    for account in &provider.accounts {
        if !select::account_allows_model(account, &step.model) {
            continue;
        }
        if !select::account_available_for(
            provider.id.as_str(),
            account.id.as_str(),
            &step.model,
            &health,
            now_ms,
        ) {
            continue;
        }
        let credential = match account_credential(&app, &step, account).await {
            Ok(Credential::Found {
                value,
                headers: oauth_headers,
            }) => Some((value, oauth_headers)),
            Ok(Credential::Missing) => {
                missing += 1;
                continue;
            }
            Ok(Credential::Unusable) => {
                unusable += 1;
                continue;
            }
            Err(error) => {
                return server::error_response(
                    StatusCode::BAD_GATEWAY,
                    Dialect::OpenAi,
                    "proxy_error",
                    &format!("the credential vault could not be read: {error}"),
                );
            }
        };

        let Some(built_url) = build_realtime_url(
            &step.base_url,
            path,
            is_operator_path,
            &step.model,
            &passthrough,
        ) else {
            return server::error_response(
                StatusCode::BAD_GATEWAY,
                Dialect::OpenAi,
                "proxy_error",
                &format!(
                    "provider `{}` has no usable realtime URL: its base URL \"{}\" cannot be \
                     turned into a WebSocket address",
                    provider.id, step.base_url
                ),
            );
        };
        let url = match mr_core::placeholders::substitute(&built_url, &account.placeholders) {
            Ok(url) => url,
            Err(problem) => {
                return server::error_response(
                    StatusCode::BAD_GATEWAY,
                    Dialect::OpenAi,
                    "account_placeholder_unfilled",
                    &format!(
                        "provider {:?} account {:?}: {}",
                        provider.id.as_str(),
                        account.label.as_deref().unwrap_or(account.id.as_str()),
                        problem.message()
                    ),
                );
            }
        };

        let mut handshake_headers = provider_headers(&provider);
        let mut attached = false;
        if let Some((value, oauth_headers)) = credential {
            attached = apply_auth(&mut handshake_headers, &step.auth, Some(&value));
            if !oauth_headers.is_empty() {
                attached = true;
                for (name, value) in oauth_headers {
                    handshake_headers.insert(name, value);
                }
            }
        }
        if !attached {
            // A credential the relay cannot put on the wire — query-parameter auth, which a
            // handshake cannot express, or a header value the HTTP layer rejects — would leave the
            // upstream to answer an unauthenticated handshake with an opaque 401. Counting it as
            // unusable and trying the sibling account is the actionable answer.
            unusable += 1;
            continue;
        }
        if let Some(protocol) = subprotocol.clone() {
            handshake_headers.insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
        }

        let Some(request) = handshake_request(&url, handshake_headers) else {
            return server::error_response(
                StatusCode::BAD_GATEWAY,
                Dialect::OpenAi,
                "proxy_error",
                &format!(
                    "provider `{}` produced a realtime URL that is not a URI",
                    provider.id
                ),
            );
        };

        match connect(request, step.connect_timeout_ms).await {
            Ok((socket, response)) => {
                let selected = response
                    .headers()
                    .get(header::SEC_WEBSOCKET_PROTOCOL)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                connected = Some((socket, selected));
                break;
            }
            Err(error) => {
                tracing::warn!(
                    provider = %provider.id,
                    account = %account.id,
                    error = %error,
                    "the realtime upstream handshake failed"
                );
                last_error = Some(error.to_string());
            }
        }
    }

    let Some((socket, selected_protocol)) = connected else {
        let message = if missing + unusable > 0 {
            no_credential_message(&provider, &step.model, missing, unusable)
        } else if let Some(error) = last_error {
            format!(
                "provider `{}` could not be reached for model \"{}\": {error}",
                provider.id, step.model
            )
        } else {
            format!(
                "no account for model \"{}\" on `{}` is available right now",
                step.model, provider.id
            )
        };
        return server::error_response(
            StatusCode::BAD_GATEWAY,
            Dialect::OpenAi,
            "proxy_error",
            &message,
        );
    };

    // The subprotocol is negotiated twice — once between the client and the relay, once between the
    // relay and the provider — so the provider's choice has to be echoed to the client. A browser
    // that asked for a protocol and is answered with none fails the connection, which is why this
    // is not left out.
    let upgrade = match selected_protocol {
        Some(protocol) => upgrade.protocols([protocol]),
        None => upgrade,
    };

    let provider_id = provider.id.as_str().to_owned();
    upgrade.on_upgrade(move |client| pump(client, socket, provider_id))
}

/// What resolving `?model=` found.
enum Selection<'a> {
    /// A step whose provider declares the capability, in a dialect this route speaks.
    Found {
        step: &'a StepPlan,
        provider: &'a ProviderPlan,
    },
    /// Nothing matched the name at all.
    NotFound { reason: String },
    /// Something matched, but no provider serving it declares `realtime`.
    NoRealtime,
    /// A provider serves it, but not as an OpenAI-shaped WebSocket.
    Dialect {
        step: &'a StepPlan,
        provider: &'a ProviderPlan,
    },
}

/// Resolve a requested name and pick the first step that can carry a realtime session.
///
/// The order of the checks is the order of the answers the client can act on: a name that does not
/// resolve is a typo (`model_not_found`), a name that resolves to a provider without the capability
/// is a route the operator has not enabled (`unsupported_endpoint`), and a provider that declares
/// the capability but speaks another dialect is a limitation of this route (`501`).
///
/// Among the steps that qualify, one with a usable account comes first and the operator's order is
/// otherwise preserved. A step where *every* account is cooling down is still tried rather than
/// dropped: the cooldown is the relay's observation, the upstream is the authority on its own
/// availability, and refusing here would turn a stale cooldown into an outage.
fn select_step<'a>(
    snapshot: &'a RoutingSnapshot,
    requested: &str,
    health: &HashMap<String, select::Cooldown>,
    now_ms: u64,
) -> Selection<'a> {
    let mut candidates: Vec<(&'a StepPlan, &'a ProviderPlan)> = Vec::new();
    match snapshot.resolve(requested) {
        Resolution::Direct { step, .. } => {
            if let Some(provider) = snapshot.providers().get(step.provider_index as usize) {
                candidates.push((step, provider));
            }
        }
        Resolution::Combo { plan, .. } => {
            for step in &plan.steps {
                if let Some(provider) = snapshot.providers().get(step.provider_index as usize) {
                    candidates.push((step, provider));
                }
            }
        }
        Resolution::NotFound { reason } => {
            return Selection::NotFound {
                reason: reason.into_owned(),
            };
        }
    }

    let mut capable: Vec<(&'a StepPlan, &'a ProviderPlan)> = Vec::new();
    let mut wrong_dialect: Option<(&'a StepPlan, &'a ProviderPlan)> = None;
    for (step, provider) in candidates {
        if !provider.capabilities.contains(&Capability::Realtime) {
            continue;
        }
        if !dialect_speaks_realtime(step.dialect) {
            wrong_dialect.get_or_insert((step, provider));
            continue;
        }
        capable.push((step, provider));
    }

    if capable.is_empty() {
        return match wrong_dialect {
            Some((step, provider)) => Selection::Dialect { step, provider },
            None => Selection::NoRealtime,
        };
    }

    let first_healthy = capable.iter().position(|(step, provider)| {
        select::step_has_usable_account(provider, health, now_ms, step)
    });
    let (step, provider) = capable[first_healthy.unwrap_or(0)];
    Selection::Found { step, provider }
}

/// Whether a dialect's upstream speaks the OpenAI realtime WebSocket protocol.
///
/// Only [`Dialect::OpenAi`] does. The other dialects in this build are HTTP request/response
/// shapes — Anthropic's Messages API, Google's `generateContent`, and the rest — and none of them
/// has a realtime socket to proxy. The SPEC is explicit that such a provider gets a `501` with the
/// reason rather than a socket that cannot work.
fn dialect_speaks_realtime(dialect: Dialect) -> bool {
    matches!(dialect, Dialect::OpenAi)
}

/// The path to append to the provider's base URL, and whether it came from `route_overrides`.
///
/// The override is keyed `"realtime"`, a literal. `RouteKind` has entries for the two HTTP helpers
/// that sit beside this socket — `realtime-client-secrets` and `realtime-calls` — but none for the
/// socket itself, so there is no `as_config_key()` to ask: the literal is the whole of the lookup,
/// and a legacy raw-path key is reported rather than ignored in silence.
fn realtime_path(provider: &ProviderPlan) -> (&str, bool) {
    if let Some(legacy) = provider
        .route_overrides
        .keys()
        .find(|key| key.starts_with('/'))
    {
        tracing::warn!(
            provider = %provider.id,
            key = %legacy,
            "a route_overrides key is a raw request path; the realtime override is keyed \"realtime\""
        );
    }
    match provider.route_overrides.get("realtime") {
        Some(path) => (path.as_str(), true),
        None => (DEFAULT_REALTIME_PATH, false),
    }
}

/// Build the upstream WebSocket URL from a provider's base URL.
///
/// Mirrors [`server::build_url`]'s version handling — a base that already carries a version keeps
/// it, a path that carries one does not repeat it — and adds the two things a socket needs: the
/// scheme becomes `ws`/`wss`, and the query string carries `model` plus whatever the client sent
/// that the relay did not consume.
///
/// `None` means the base URL cannot name a WebSocket at all: an empty base, an unsupported scheme,
/// or a base that already has a query or fragment of its own, where appending another query would
/// produce something no upstream would parse.
fn build_realtime_url(
    base_url: &str,
    path: &str,
    is_operator_path: bool,
    model: &str,
    passthrough: &str,
) -> Option<String> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() || base.contains('?') || base.contains('#') {
        return None;
    }
    let (scheme, rest) = base.split_once("://")?;
    let (authority, base_path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, String::new()),
    };
    if authority.is_empty() {
        return None;
    }
    let scheme = match scheme.to_ascii_lowercase().as_str() {
        "https" | "wss" => "wss",
        "http" | "ws" => "ws",
        _ => return None,
    };

    let mut segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();

    // A provider whose base URL already ends at the socket — the way a realtime-only endpoint is
    // usually configured — must not have the default path appended twice. An operator's explicit
    // override is still appended, because naming a path is a decision about the whole address.
    let base_ends_at_socket = !is_operator_path
        && base_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|segment| segment == "realtime");

    if base_ends_at_socket {
        // The base already names the socket, so nothing is appended and no version is touched.
        segments.clear();
    } else {
        // The version rule from `server::build_url`, unchanged: the version appears exactly once,
        // so a path that begins with one gives it up when the base already carries one, and gains
        // the dialect's when it carries none. The base's own version always stands, and an
        // operator's path is literal — only a version identical to the base's is dropped from it.
        let base_version = base_version_segment(&base_path);
        match base_version {
            Some(version) => {
                let droppable = if is_operator_path {
                    segments.first().is_some_and(|first| *first == version)
                } else {
                    segments
                        .first()
                        .is_some_and(|first| is_pure_version_segment(first))
                };
                if droppable && segments.len() > 1 {
                    segments.remove(0);
                }
            }
            None if !is_operator_path => {
                let already_versioned = segments
                    .first()
                    .is_some_and(|first| is_version_segment(first));
                if !already_versioned {
                    segments.insert(0, OPENAI_API_VERSION);
                }
            }
            None => {}
        }
    }

    let base_path = base_path.trim_end_matches('/');
    let mut url = format!("{scheme}://{authority}{base_path}");
    if !segments.is_empty() {
        if !url.ends_with('/') {
            url.push('/');
        }
        url.push_str(&segments.join("/"));
    }

    let mut query = format!("{MODEL_PARAM}={}", encode_query_value(model));
    if !passthrough.is_empty() {
        query.push('&');
        query.push_str(passthrough);
    }
    url.push('?');
    url.push_str(&query);
    Some(url)
}

/// The base URL's version segment, if it has one — the same reading [`server::build_url`] uses.
fn base_version_segment(base_path: &str) -> Option<&str> {
    base_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .rfind(|segment| is_version_segment(segment))
}

/// Whether a path segment looks like an API version: a `v` followed by a digit.
///
/// A prefix test, as in `server::is_version_segment`: `v1`, `v1beta`, `v4` and
/// `v1internal:generateContent` all qualify.
fn is_version_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('v')) && chars.next().is_some_and(|c| c.is_ascii_digit())
}

/// Whether a path segment is a version and nothing else: `^v\d[a-z0-9]*$`.
///
/// As in `server::is_pure_version_segment`: only a segment that is *only* a version can be dropped
/// in favour of the base URL's version, because a segment that carries an action too is the
/// endpoint's own spelling.
fn is_pure_version_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    matches!(chars.next(), Some('v'))
        && chars.next().is_some_and(|c| c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Split the client's query string into the model it names and everything else, verbatim.
///
/// The passthrough is kept byte-for-byte, not re-encoded: an unknown parameter is the upstream's to
/// interpret, and re-encoding could change what it means. `model` is consumed (the relay sends the
/// *step's* model, which is the upstream's own name for it) and percent-decoded, because a model id
/// may legitimately contain a slash that the client had to encode.
fn split_query(query: &str) -> (Option<String>, String) {
    let mut model = None;
    let mut passthrough = String::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        };
        if decode_query_value(key) == MODEL_PARAM {
            if model.is_none() {
                model = Some(decode_query_value(value));
            }
            continue;
        }
        if !passthrough.is_empty() {
            passthrough.push('&');
        }
        passthrough.push_str(pair);
    }
    (model, passthrough)
}

/// Percent-decode one query-string component, treating `+` as a space.
fn decode_query_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len()
                && bytes[index + 1].is_ascii_hexdigit()
                && bytes[index + 2].is_ascii_hexdigit() =>
            {
                match u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode one query-string value, leaving only RFC 3986's unreserved set bare.
fn encode_query_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The provider's configured headers, as the handshake's starting set.
///
/// A header the HTTP layer will not accept is reported and left off, which is what the HTTP path
/// does with the same map: a typo in one header must not make the whole route unusable.
fn provider_headers(provider: &ProviderPlan) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(provider.headers.len() + 1);
    for (name, value) in &provider.headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                headers.insert(name, value);
            }
            _ => tracing::warn!(
                provider = %provider.id,
                header = %name,
                "a provider header is not a valid HTTP header; it is left off the realtime handshake"
            ),
        }
    }
    headers
}

/// The outbound handshake request: the upgrade the protocol requires, plus the headers decided
/// above.
///
/// The request is *generated* rather than assembled, because a WebSocket handshake is more than a
/// `GET`: it needs a random `sec-websocket-key`, the version, and the `upgrade`/`connection`
/// headers, and `IntoClientRequest` is what supplies them. A hand-built request gets answered
/// `400 Missing, duplicated or incorrect header sec-websocket-key` — which is exactly what the
/// end-to-end test below saw before this used the generator.
fn handshake_request(
    url: &str,
    headers: HeaderMap,
) -> Option<tokio_tungstenite::tungstenite::handshake::client::Request> {
    let mut request = url.into_client_request().ok()?;
    for (name, value) in &headers {
        request.headers_mut().insert(name.clone(), value.clone());
    }
    Some(request)
}

/// Connect to the upstream socket, bounded by the provider's connect timeout.
async fn connect(
    request: tokio_tungstenite::tungstenite::handshake::client::Request,
    connect_timeout_ms: u64,
) -> Result<(UpstreamSocket, http::Response<Option<Vec<u8>>>), tokio_tungstenite::tungstenite::Error>
{
    if connect_timeout_ms == 0 {
        return connect_async(request).await;
    }
    match tokio::time::timeout(
        Duration::from_millis(connect_timeout_ms),
        connect_async(request),
    )
    .await
    {
        Ok(result) => result,
        // The timeout is a connect failure like any other: the caller reports it as a `502` and
        // moves to the next account, which may be a host that answers.
        Err(_) => Err(tokio_tungstenite::tungstenite::Error::Io(
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("the upstream handshake did not complete within {connect_timeout_ms} ms"),
            ),
        )),
    }
}

/// What one account can contribute to a handshake.
enum Credential {
    /// A value to attach, plus any extra headers an OAuth platform adds.
    Found {
        value: String,
        headers: Vec<(HeaderName, HeaderValue)>,
    },
    /// Nothing is stored under this account's reference.
    Missing,
    /// Something is referenced but cannot be turned into a usable credential.
    Unusable,
}

/// Resolve one account's credential, exactly as the HTTP path does.
///
/// This mirrors `server::account_credential`, which is private to that module. The rules are the
/// same and must stay the same: an OAuth account refreshes through the shared refresher and adds the
/// platform's own headers, a static account reads the vault with the one re-read that makes a key
/// added after startup visible, and a vault that cannot be read at all is a relay fault rather than
/// an account fault.
async fn account_credential(
    app: &AppState,
    step: &StepPlan,
    account: &AccountPlan,
) -> Result<Credential, mr_core::error::Error> {
    let Some(oauth_ref) = account.oauth_ref.as_deref() else {
        let stored = match stored_credential(app, account) {
            Ok(stored) => stored,
            Err(error) => return Err(error),
        };
        return Ok(match stored {
            StoredCredential::Found(value) => Credential::Found {
                value,
                headers: Vec::new(),
            },
            StoredCredential::Miss => {
                tracing::warn!(account = %account.id, "no credential in the vault for this account");
                Credential::Missing
            }
        });
    };

    let oauth_id = mr_core::OAuthCredentialId::from_raw(oauth_ref);
    let AuthKind::OAuth { platform } = &step.auth else {
        tracing::warn!(
            account = %account.id,
            "an OAuth account is attached to a step that does not use OAuth auth"
        );
        return Ok(Credential::Unusable);
    };
    let record = match app.oauth_store.get_oauth_credential(&oauth_id) {
        Ok(Some(record)) if record.platform == *platform => record,
        _ => {
            tracing::warn!(account = %account.id, "OAuth credential metadata is missing or mismatched");
            return Ok(Credential::Unusable);
        }
    };
    let Some(oauth_platform) = mr_core::oauth::platform(platform) else {
        tracing::warn!(platform, "OAuth platform is not available in this build");
        return Ok(Credential::Unusable);
    };
    let Some(refresher) = app.oauth_refreshers.get(platform) else {
        tracing::warn!(
            platform,
            "no OAuth refresher is registered for this platform"
        );
        return Ok(Credential::Unusable);
    };
    match refresher.ensure_fresh(&record.vault_ref, false).await {
        Ok(tokens) => Ok(Credential::Found {
            value: tokens.access_token.clone(),
            headers: oauth_platform.apply_credential(&tokens),
        }),
        Err(failure) => {
            tracing::warn!(platform, class = %failure.class, "OAuth credential refresh failed");
            Ok(Credential::Unusable)
        }
    }
}

/// The result of looking an account's static credential up in the vault.
enum StoredCredential {
    Found(String),
    Miss,
}

/// Look up one account's static credential, giving a miss a second look at the file.
///
/// The relay's vault is a snapshot taken at startup while `mr` and the panel write to the same
/// vault at any moment, so "nothing under this reference" can simply mean "nothing there when we
/// started". Re-reading once turns a permanent-looking failure into a credential that works as soon
/// as it is added. Mirrors `server::stored_credential`.
fn stored_credential(
    app: &AppState,
    account: &AccountPlan,
) -> Result<StoredCredential, mr_core::error::Error> {
    let Some(reference) = account.secret_ref.as_deref() else {
        return Ok(StoredCredential::Miss);
    };
    match app.vault.get(reference)? {
        Some(secret) => Ok(StoredCredential::Found(secret)),
        None => {
            let fresh = app.vault.refreshed()?;
            match fresh.get(reference)? {
                Some(secret) => {
                    tracing::info!("credential was stored after the relay started; using it");
                    Ok(StoredCredential::Found(secret))
                }
                None => Ok(StoredCredential::Miss),
            }
        }
    }
}

/// The client-facing reason a step had candidate accounts but no usable credential among them.
///
/// Mirrors `server::no_credential_message`, which is private to that module: the two surfaces must
/// give the operator the same instruction, because it is the same problem.
fn no_credential_message(
    provider: &ProviderPlan,
    model: &str,
    missing: usize,
    unusable: usize,
) -> String {
    let mut causes = Vec::with_capacity(2);
    if missing == 1 {
        causes.push("1 account has no credential in the vault".to_owned());
    } else if missing > 1 {
        causes.push(format!(
            "{missing} accounts have no credential in the vault"
        ));
    }
    if unusable == 1 {
        causes.push("1 account's credential cannot be used".to_owned());
    } else if unusable > 1 {
        causes.push(format!("{unusable} accounts' credentials cannot be used"));
    }
    format!(
        "no account for model \"{model}\" on `{}` has a usable credential: {}; attach one with \
         `mr account add {} --secret-stdin`",
        provider.id,
        causes.join(", "),
        provider.id
    )
}

/// Attach a credential to the handshake headers, returning whether anything was attached.
///
/// Mirrors `server::apply_auth`, which is private to that module and returns nothing. The one
/// deliberate divergence is the return value: the HTTP path warns and sends the request without the
/// credential when it cannot be applied, but a WebSocket handshake sent without one is answered with
/// an opaque `401` that the client cannot act on, so this route counts the account as unusable and
/// either tries a sibling account or answers `502` naming the provider.
fn apply_auth(headers: &mut HeaderMap, auth: &AuthKind, credential: Option<&str>) -> bool {
    let Some(credential) = credential else {
        return false;
    };
    match auth {
        AuthKind::Bearer | AuthKind::OAuth { .. } => insert_header(
            headers,
            header::AUTHORIZATION,
            format!("Bearer {credential}"),
        ),
        AuthKind::ApiKeyHeader { header: name } => {
            let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
                tracing::warn!(
                    header = %name,
                    "the API-key header name is not a valid HTTP header; the credential cannot be attached"
                );
                return false;
            };
            insert_header(headers, name, credential.to_owned())
        }
        AuthKind::Header { name, prefix } => {
            let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
                tracing::warn!(
                    header = %name,
                    "the auth header name is not a valid HTTP header; the credential cannot be attached"
                );
                return false;
            };
            insert_header(headers, name, format!("{prefix}{credential}"))
        }
        AuthKind::Modal => {
            let Some((key, secret)) = credential.split_once('.') else {
                tracing::warn!(
                    "a Modal credential is not \"TokenId.TokenSecret\"; the credential cannot be attached"
                );
                return false;
            };
            insert_header(
                headers,
                HeaderName::from_static("modal-key"),
                key.to_owned(),
            ) && insert_header(
                headers,
                HeaderName::from_static("modal-secret"),
                secret.to_owned(),
            )
        }
        AuthKind::QueryParam { param } => {
            // A handshake is a `GET` with no body; the credential would have to be in the URL. The
            // URL is built by `build_realtime_url`, which appends only `model` and the client's
            // passthrough, so this cannot be expressed without putting the secret somewhere it
            // would be logged. Refusing is the honest answer.
            tracing::warn!(
                param,
                "query-parameter authentication cannot be applied to a WebSocket handshake; \
                 the credential will be missing"
            );
            false
        }
    }
}

/// Insert one header, reporting whether the value was a valid header value.
fn insert_header(headers: &mut HeaderMap, name: HeaderName, value: String) -> bool {
    match HeaderValue::from_str(&value) {
        Ok(value) => {
            headers.insert(name, value);
            true
        }
        Err(_) => {
            tracing::warn!(
                header = %name,
                "could not build an auth header; the credential will be missing"
            );
            false
        }
    }
}

/// Move frames between the client and the upstream until either side closes.
///
/// Frames are forwarded as they arrive: `Text`, `Binary`, `Ping`, `Pong` and `Close` keep their
/// payloads and their order, and nothing is parsed, reassembled or dropped. The loop ends when a
/// side closes or fails, and the other side is closed with the same code and reason so neither end
/// is left waiting on a socket whose peer has gone.
///
/// There is deliberately **no idle timeout**. A realtime session is silent between utterances — the
/// client is recording, or the model is thinking, or the user has simply stopped talking — and any
/// timeout here would hang up on a session that is working exactly as intended.
async fn pump(client: WebSocket, upstream: UpstreamSocket, provider_id: String) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    loop {
        tokio::select! {
            message = client_rx.next() => match message {
                Some(Ok(message)) => {
                    let closing = matches!(message, Message::Close(_));
                    if let Some(frame) = client_frame(message) {
                        if upstream_tx.send(frame).await.is_err() {
                            tracing::warn!(
                                provider = %provider_id,
                                "the realtime upstream stopped accepting frames"
                            );
                            break;
                        }
                    }
                    if closing {
                        // The close frame went upstream with its own code and reason; answering it
                        // here is what completes the handshake on the client side.
                        let _ = client_tx.close().await;
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::warn!(
                        provider = %provider_id,
                        error = %error,
                        "the realtime client websocket failed"
                    );
                    let frame = UpstreamCloseFrame {
                        code: CloseCode::Error,
                        reason: "client websocket error".into(),
                    };
                    let _ = upstream_tx
                        .send(UpstreamMessage::Close(Some(frame)))
                        .await;
                    break;
                }
                None => {
                    let _ = upstream_tx.close().await;
                    break;
                }
            },
            message = upstream_rx.next() => match message {
                Some(Ok(message)) => {
                    let closing = matches!(message, UpstreamMessage::Close(_));
                    if let Some(frame) = upstream_frame(message) {
                        if client_tx.send(frame).await.is_err() {
                            tracing::warn!(
                                provider = %provider_id,
                                "the realtime client stopped accepting frames"
                            );
                            break;
                        }
                    }
                    if closing {
                        let _ = upstream_tx.close().await;
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::warn!(
                        provider = %provider_id,
                        error = %error,
                        "the realtime upstream websocket failed"
                    );
                    let frame = CloseFrame {
                        code: 1011,
                        reason: "upstream websocket error".into(),
                    };
                    let _ = client_tx.send(Message::Close(Some(frame))).await;
                    break;
                }
                None => {
                    let _ = client_tx.close().await;
                    break;
                }
            },
        }
    }
}

/// Translate a client frame for the upstream, or `None` for a frame that has no wire form.
fn client_frame(message: Message) -> Option<UpstreamMessage> {
    match message {
        Message::Text(text) => Some(UpstreamMessage::Text(text.as_str().into())),
        Message::Binary(bytes) => Some(UpstreamMessage::Binary(bytes)),
        Message::Ping(bytes) => Some(UpstreamMessage::Ping(bytes)),
        Message::Pong(bytes) => Some(UpstreamMessage::Pong(bytes)),
        Message::Close(frame) => Some(UpstreamMessage::Close(frame.map(|frame| {
            UpstreamCloseFrame {
                code: CloseCode::from(frame.code),
                reason: frame.reason.as_str().into(),
            }
        }))),
    }
}

/// Translate an upstream frame for the client, or `None` for a frame that has no wire form.
///
/// `tungstenite`'s message is `#[non_exhaustive]` and can carry a raw `Frame`, which is a
/// representation detail rather than something to forward; the arms below are the whole wire
/// vocabulary.
fn upstream_frame(message: UpstreamMessage) -> Option<Message> {
    match message {
        UpstreamMessage::Text(text) => Some(Message::Text(text.as_str().into())),
        UpstreamMessage::Binary(bytes) => Some(Message::Binary(bytes)),
        UpstreamMessage::Ping(bytes) => Some(Message::Ping(bytes)),
        UpstreamMessage::Pong(bytes) => Some(Message::Pong(bytes)),
        UpstreamMessage::Close(frame) => Some(Message::Close(frame.map(|frame| CloseFrame {
            code: u16::from(frame.code),
            reason: frame.reason.as_str().into(),
        }))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::model::{Account, Combo, ComboStep, ModelEntry, Provider};

    /// A minimal model entry, enough for the compiler to accept a provider.
    ///
    /// Built through `serde` because `ModelEntry` has no `Default`: every field but `id` carries a
    /// `#[serde(default)]`, which is exactly the "an entry named by its id alone" case.
    fn model_entry(id: &str) -> ModelEntry {
        serde_json::from_value(serde_json::json!({ "id": id }))
            .expect("a model entry with only an id is valid")
    }

    /// A provider with the given capabilities and one model.
    fn provider(id: &str, base_url: &str, capabilities: Vec<Capability>) -> Provider {
        let mut provider = Provider::new(id, Dialect::OpenAi, base_url.to_owned());
        provider.capabilities = capabilities;
        provider.models = vec![model_entry("m")];
        let mut account = Account::new(id);
        account.secret_ref = Some("test-credential".to_owned());
        provider.accounts = vec![account];
        provider
    }

    fn snapshot(providers: Vec<Provider>, combos: Vec<Combo>) -> RoutingSnapshot {
        let config = mr_core::config::Config {
            providers,
            combos,
            ..Default::default()
        };
        RoutingSnapshot::compile(&config).expect("the fixture must compile")
    }

    fn no_health() -> HashMap<String, select::Cooldown> {
        HashMap::new()
    }

    #[test]
    fn a_base_url_with_a_version_becomes_a_realtime_socket() {
        let url = build_realtime_url(
            "https://api.openai.com/v1",
            DEFAULT_REALTIME_PATH,
            false,
            "gpt-realtime",
            "",
        );
        assert_eq!(
            url.as_deref(),
            Some("wss://api.openai.com/v1/realtime?model=gpt-realtime")
        );
    }

    #[test]
    fn a_trailing_slash_and_a_missing_version_are_both_handled() {
        assert_eq!(
            build_realtime_url(
                "https://api.openai.com/v1/",
                DEFAULT_REALTIME_PATH,
                false,
                "m",
                "",
            )
            .as_deref(),
            Some("wss://api.openai.com/v1/realtime?model=m")
        );
        assert_eq!(
            build_realtime_url(
                "https://api.openai.com",
                DEFAULT_REALTIME_PATH,
                false,
                "m",
                "",
            )
            .as_deref(),
            Some("wss://api.openai.com/v1/realtime?model=m")
        );
    }

    #[test]
    fn a_base_url_that_already_ends_at_the_socket_is_not_doubled() {
        assert_eq!(
            build_realtime_url(
                "https://api.openai.com/v1/realtime",
                DEFAULT_REALTIME_PATH,
                false,
                "m",
                "",
            )
            .as_deref(),
            Some("wss://api.openai.com/v1/realtime?model=m")
        );
    }

    #[test]
    fn an_operator_override_wins_over_the_default_path() {
        let mut config = provider(
            "p",
            "https://example.test/v1",
            vec![Capability::Chat, Capability::Realtime],
        );
        config
            .route_overrides
            .insert("realtime".to_owned(), "/v2/socket".to_owned());
        let table = snapshot(vec![config], Vec::new());
        let plan = &table.providers()[0];
        let (path, is_operator) = realtime_path(plan);
        assert_eq!(path, "/v2/socket");
        assert!(is_operator);
        // The override replaces the *default* path; it is joined to the base exactly as the HTTP
        // route joins it, so the base's own path stays in front.
        assert_eq!(
            build_realtime_url(&plan.base_url, path, is_operator, "m", "").as_deref(),
            Some("wss://example.test/v1/v2/socket?model=m")
        );
    }

    #[test]
    fn http_becomes_ws_and_the_client_query_is_carried_except_for_model() {
        let (model, passthrough) = split_query("model=p%2Fm&temperature=0.8&model=other");
        assert_eq!(model.as_deref(), Some("p/m"));
        assert_eq!(passthrough, "temperature=0.8");
        assert_eq!(
            build_realtime_url(
                "http://127.0.0.1:8080",
                DEFAULT_REALTIME_PATH,
                false,
                "p/m",
                &passthrough,
            )
            .as_deref(),
            Some("ws://127.0.0.1:8080/v1/realtime?model=p%2Fm&temperature=0.8")
        );
    }

    #[test]
    fn a_base_url_with_its_own_query_cannot_name_a_socket() {
        assert_eq!(
            build_realtime_url(
                "https://example.test/v1?key=1",
                DEFAULT_REALTIME_PATH,
                false,
                "m",
                "",
            ),
            None
        );
        assert_eq!(
            build_realtime_url("", DEFAULT_REALTIME_PATH, false, "m", ""),
            None
        );
        assert_eq!(
            build_realtime_url("ftp://example.test", DEFAULT_REALTIME_PATH, false, "m", "",),
            None
        );
    }

    #[test]
    fn a_bearer_credential_becomes_an_authorization_header() {
        let mut headers = HeaderMap::new();
        assert!(apply_auth(&mut headers, &AuthKind::Bearer, Some("sk-x")));
        assert_eq!(headers[header::AUTHORIZATION], "Bearer sk-x");
    }

    #[test]
    fn each_auth_kind_lands_where_the_http_path_puts_it() {
        let mut headers = HeaderMap::new();
        assert!(apply_auth(
            &mut headers,
            &AuthKind::ApiKeyHeader {
                header: "x-api-key".to_owned(),
            },
            Some("sk-x"),
        ));
        assert_eq!(headers["x-api-key"], "sk-x");

        let mut headers = HeaderMap::new();
        assert!(apply_auth(
            &mut headers,
            &AuthKind::Header {
                name: "api-key".to_owned(),
                prefix: String::new(),
            },
            Some("sk-x"),
        ));
        assert_eq!(headers["api-key"], "sk-x");

        let mut headers = HeaderMap::new();
        assert!(apply_auth(
            &mut headers,
            &AuthKind::Header {
                name: "x-token".to_owned(),
                prefix: "Token ".to_owned(),
            },
            Some("sk-x"),
        ));
        assert_eq!(headers["x-token"], "Token sk-x");
    }

    #[test]
    fn a_modal_credential_is_split_into_its_two_headers() {
        let mut headers = HeaderMap::new();
        assert!(apply_auth(
            &mut headers,
            &AuthKind::Modal,
            Some("id.secret")
        ));
        assert_eq!(headers["modal-key"], "id");
        assert_eq!(headers["modal-secret"], "secret");
    }

    #[test]
    fn an_oauth_bearer_is_an_authorization_header_too() {
        let mut headers = HeaderMap::new();
        assert!(apply_auth(
            &mut headers,
            &AuthKind::OAuth {
                platform: "test".to_owned(),
            },
            Some("access"),
        ));
        assert_eq!(headers[header::AUTHORIZATION], "Bearer access");
    }

    #[test]
    fn a_credential_that_cannot_be_attached_is_reported_rather_than_dropped() {
        let mut headers = HeaderMap::new();
        assert!(!apply_auth(
            &mut headers,
            &AuthKind::QueryParam {
                param: "key".to_owned(),
            },
            Some("sk-x"),
        ));
        assert!(headers.is_empty());
        assert!(!apply_auth(&mut headers, &AuthKind::Modal, Some("no-dot")));
        assert!(headers.is_empty());
        assert!(!apply_auth(&mut headers, &AuthKind::Bearer, None));
        assert!(headers.is_empty());
    }

    #[test]
    fn a_direct_name_resolves_to_a_provider_that_declares_realtime() {
        let table = snapshot(
            vec![provider(
                "p",
                "https://example.test/v1",
                vec![Capability::Chat, Capability::Realtime],
            )],
            Vec::new(),
        );
        match select_step(&table, "p/m", &no_health(), 0) {
            Selection::Found { step, provider } => {
                assert_eq!(step.model, "m");
                assert_eq!(provider.id.as_str(), "p");
            }
            _ => panic!("the provider declares realtime and must be selected"),
        }
    }

    #[test]
    fn a_provider_without_the_capability_is_unsupported_not_missing() {
        let table = snapshot(
            vec![provider(
                "p",
                "https://example.test/v1",
                vec![Capability::Chat],
            )],
            Vec::new(),
        );
        assert!(matches!(
            select_step(&table, "p/m", &no_health(), 0),
            Selection::NoRealtime
        ));
    }

    #[test]
    fn a_combo_skips_the_step_that_cannot_speak_realtime() {
        let mut combo = Combo::new("fast", ComboStep::new("plain", "m"));
        combo.steps.push(ComboStep::new("socket", "m"));
        let table = snapshot(
            vec![
                provider("plain", "https://plain.test/v1", vec![Capability::Chat]),
                provider(
                    "socket",
                    "https://socket.test/v1",
                    vec![Capability::Chat, Capability::Realtime],
                ),
            ],
            vec![combo],
        );
        match select_step(&table, "fast", &no_health(), 0) {
            Selection::Found { provider, .. } => assert_eq!(provider.id.as_str(), "socket"),
            _ => panic!("the second step can serve realtime and must be selected"),
        }
    }

    #[test]
    fn a_provider_in_another_dialect_gets_the_reason_not_a_not_found() {
        let mut anthropic = Provider::new("a", Dialect::Anthropic, "https://a.test/v1".to_owned());
        anthropic.capabilities = vec![Capability::Chat, Capability::Realtime];
        anthropic.models = vec![model_entry("m")];
        let mut account = Account::new("a");
        account.secret_ref = Some("test-credential".to_owned());
        anthropic.accounts = vec![account];
        let table = snapshot(vec![anthropic], Vec::new());
        match select_step(&table, "a/m", &no_health(), 0) {
            Selection::Dialect { step, .. } => assert_eq!(step.dialect, Dialect::Anthropic),
            _ => panic!("a realtime-capable provider in another dialect must be named as such"),
        }
    }

    #[test]
    fn an_unknown_name_is_not_found_with_the_resolvers_reason() {
        let table = snapshot(
            vec![provider(
                "p",
                "https://example.test/v1",
                vec![Capability::Realtime],
            )],
            Vec::new(),
        );
        match select_step(&table, "nothing", &no_health(), 0) {
            Selection::NotFound { reason } => assert!(!reason.is_empty()),
            _ => panic!("an unknown name must not resolve"),
        }
    }

    /// The relay state the route runs against: one provider at `base_url`, declaring `realtime`,
    /// with one account whose credential is in a vault in a temporary directory.
    ///
    /// The directory is returned with the state because the vault reads its key file lazily: a
    /// dropped `TempDir` would delete the key under a live state.
    fn state_at(base_url: &str) -> (AppState, tempfile::TempDir) {
        state_at_with(base_url, vec![Capability::Chat, Capability::Realtime])
    }

    /// The same state, with the provider declaring exactly `capabilities`.
    fn state_at_with(
        base_url: &str,
        capabilities: Vec<Capability>,
    ) -> (AppState, tempfile::TempDir) {
        let provider = provider("p", base_url, capabilities);
        let config = mr_core::config::Config {
            providers: vec![provider],
            ..Default::default()
        };
        let snapshot = RoutingSnapshot::compile(&config).expect("the fixture must compile");
        let relay = std::sync::Arc::new(crate::state::RelayState::new(
            &config,
            snapshot,
            crate::upstream::Upstream::from_latency(&config.latency).expect("a transport"),
        ));
        let dir = tempfile::tempdir().expect("a temporary directory");
        let mut vault = mr_core::secret::Vault::open(dir.path()).expect("a vault");
        vault
            .put("test-credential", "sk-not-a-real-key")
            .expect("a stored credential");
        let store = std::sync::Arc::new(mr_core::store::Store::open_in_memory().expect("a store"));
        let state = AppState {
            relay,
            no_key_required: true,
            health: std::sync::Arc::new(select::Health::new()),
            vault: std::sync::Arc::new(vault),
            oauth_refreshers: std::sync::Arc::new(HashMap::new()),
            quota: std::sync::Arc::new(crate::quota::QuotaManager::new(std::sync::Arc::clone(
                &store,
            ))),
            oauth_store: std::sync::Arc::clone(&store),
            quota_store: std::sync::Arc::clone(&store),
            sticky_success: std::sync::Arc::new(crate::sticky::StickySuccess::default()),
            activity: std::sync::Arc::new(crate::activity::ActivityLog::new()),
            account_inflight: std::sync::Arc::new(crate::state::AccountInflight::default()),
            kv_usage: std::sync::Arc::new(crate::kv_budget::AdmissionPool::new()),
            batch: crate::batch::BatchLane::inert(),
        };
        (state, dir)
    }

    /// The whole route, end to end, against a real WebSocket upstream on localhost.
    ///
    /// Nothing here is stubbed on either side: a client connects to the axum router with
    /// `connect_async`, the route opens its own socket to a `tokio_tungstenite` listener, and a text
    /// frame makes the round trip. What that pins, beyond the frame plumbing, is the credential: the
    /// provider's key has to arrive on the upstream *handshake*, not on some later frame, because a
    /// realtime upstream authenticates the upgrade and closes anything else.
    #[tokio::test]
    #[allow(clippy::result_large_err)] // the handshake callback's error type is tungstenite's
    async fn a_text_frame_makes_the_round_trip_and_the_credential_is_on_the_handshake() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let upstream_addr = listener.local_addr().expect("an address");
        // (what the upstream was asked for, the credential it was offered). Both are facts about the
        // handshake, which is the only place either can be observed at the far end.
        let seen: std::sync::Arc<std::sync::Mutex<Option<(String, String)>>> = Default::default();
        let recorded = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("an upstream connection");
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                move |request: &http::Request<()>, response: http::Response<()>| {
                    let target = request
                        .uri()
                        .path_and_query()
                        .map(|target| target.as_str().to_owned())
                        .unwrap_or_default();
                    let credential = request
                        .headers()
                        .get(header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    *recorded.lock().expect("the recording mutex") = Some((target, credential));
                    Ok(response)
                },
            )
            .await
            .expect("an accepted handshake");
            while let Some(Ok(message)) = socket.next().await {
                if let UpstreamMessage::Text(text) = message {
                    socket
                        .send(UpstreamMessage::Text(format!("echo:{text}").into()))
                        .await
                        .expect("an echoed frame");
                }
            }
        });

        let (state, _dir) = state_at(&format!("http://{upstream_addr}"));
        let app = Router::new().merge(routes()).with_state(state);
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a relay listener");
        let relay_addr = relay.local_addr().expect("a relay address");
        tokio::spawn(async move { axum::serve(relay, app).await.expect("the relay serves") });

        let (mut client, response) =
            connect_async(format!("ws://{relay_addr}/v1/realtime?model=p/m"))
                .await
                .expect("the relay must answer the upgrade");
        assert_eq!(response.status(), 101, "the upgrade must be accepted");
        client
            .send(UpstreamMessage::Text("hello".into()))
            .await
            .expect("a sent frame");
        let echoed = match client.next().await.expect("a frame").expect("a good frame") {
            UpstreamMessage::Text(text) => text.as_str().to_owned(),
            other => panic!("the relay forwarded an unexpected frame: {other:?}"),
        };
        assert_eq!(echoed, "echo:hello");
        let (target, credential) = seen
            .lock()
            .expect("the recording mutex")
            .clone()
            .expect("the upstream saw a handshake");
        assert_eq!(
            target, "/v1/realtime?model=m",
            "the upstream must be asked for its own realtime socket, with the provider's model name"
        );
        assert_eq!(
            credential, "Bearer sk-not-a-real-key",
            "the provider's credential must be on the upstream handshake"
        );
        client.close(None).await.expect("a close");
    }

    /// A provider that cannot serve the socket is refused *before* the upgrade, and nothing dials it.
    ///
    /// This is the ordering that matters: a `404` the client can read, delivered as an ordinary HTTP
    /// answer to its handshake, rather than a `101` followed by a close frame it has to interpret.
    /// The second half of the assertion is the part a status code alone cannot make — the upstream
    /// listener is polled and must stay silent, so the refusal cannot have opened a socket and
    /// thrown it away.
    #[tokio::test]
    async fn a_provider_without_the_capability_is_refused_without_the_socket_being_opened() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a listener");
        let upstream_addr = listener.local_addr().expect("an address");
        let (state, _dir) =
            state_at_with(&format!("http://{upstream_addr}"), vec![Capability::Chat]);
        let app = Router::new().merge(routes()).with_state(state);
        let relay = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a relay listener");
        let relay_addr = relay.local_addr().expect("a relay address");
        tokio::spawn(async move { axum::serve(relay, app).await.expect("the relay serves") });

        let error = connect_async(format!("ws://{relay_addr}/v1/realtime?model=p/m"))
            .await
            .expect_err("a provider without the capability cannot be upgraded");
        let (status, body) = http_error(error);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["type"], "unsupported_endpoint");
        let message = body["error"]["message"].as_str().expect("a message");
        assert!(
            message.contains("realtime"),
            "the refusal must name the missing capability: {message}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "the refusal must not have dialled the provider"
        );
    }

    /// The status and JSON envelope of an HTTP refusal, as `tungstenite` reports one.
    fn http_error(error: tokio_tungstenite::tungstenite::Error) -> (StatusCode, serde_json::Value) {
        match error {
            tokio_tungstenite::tungstenite::Error::Http(response) => {
                let status = response.status();
                let body = response.into_body().unwrap_or_default();
                let body = serde_json::from_slice(&body).expect("the refusal is a JSON envelope");
                (status, body)
            }
            other => panic!("expected an HTTP answer, got {other:?}"),
        }
    }
}
