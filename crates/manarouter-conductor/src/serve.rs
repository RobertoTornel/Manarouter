//! # serve.rs — bind the control listener and serve the panel
//!
//! Two jobs, deliberately kept out of `api.rs`: choosing the socket (with an error an operator
//! can act on — never a panic), and serving the panel's static shell.
//!
//! The spec preferred embedding `panel/` at compile time (`include_dir`), but this crate's
//! `Cargo.toml` is owned elsewhere and adding a dependency is off limits — so the panel is
//! **served from disk**: `ServeDirs::discover()` looks for `panel/` once, at startup, relative to
//! the working directory and the executable, and a missing directory degrades to a placeholder
//! page explaining where to put the files rather than to a crash. The search is resolved once so
//! a later `cd` cannot silently change which `panel/` is served.
//!
//! The static fallback is registered **after** the guard layer has been applied to the API
//! routes. axum applies `layer` only to routes that already exist, which is precisely why the
//! panel's `index.html` stays reachable without a session while every `/api` route (including the
//! `/api/{*rest}` JSON 404 catch-all registered inside the router) is guarded.

use std::path::Path as FsPath;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;

use crate::api;
use crate::state::AppState;

/// Bind the configured control address, serve until a restart is requested, then shut down
/// gracefully so `main` can exit and a supervisor restart the process.
///
/// A bind failure is the one error this process must never panic on: it names the host, the port
/// and the operating system's reason, and asks the question the operator actually has to answer
/// (is something else already on this port?).
pub async fn serve(state: AppState) -> Result<(), String> {
    let (host, port) = {
        let config = state.current_config().await;
        (config.control.host.clone(), config.control.port)
    };

    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|e| {
            format!(
                "cannot bind the control plane on {host}:{port}: {e}. Is another instance \
                 already listening? Change `control.port` (panel → Settings) or stop the other \
                 process."
            )
        })?;
    let bound_port = listener
        .local_addr()
        .map_err(|e| format!("the control listener has no local address: {e}"))?
        .port();

    // Record the real bound port (it differs from `port` when the config says 0) before serving,
    // so even the first `GET /api/ports` and the origin check see the truth.
    state
        .running_ports
        .control
        .store(bound_port, std::sync::atomic::Ordering::SeqCst);
    if bound_port != port {
        tracing::warn!(
            configured = port,
            bound = bound_port,
            "control.port 0 let the operating system choose; the panel reads the real port from \
             GET /api/ports"
        );
    }

    // The URL an operator can actually use: a wildcard bind is not a hostname anybody can type,
    // and a bare IPv6 literal needs brackets to look like a URL.
    let display_host = match host.as_str() {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_owned(),
        other if other.contains(':') && !other.starts_with('[') => format!("[{other}]"),
        other => other.to_owned(),
    };
    let origin = format!("http://{display_host}:{bound_port}");
    tracing::info!(
        "control plane listening on {origin} (panel: {origin}/, API: {origin}/api/health)"
    );

    let application = app(state.clone());
    axum::serve(listener, application)
        .with_graceful_shutdown(shutdown_signal(state))
        .await
        .map_err(|e| format!("the control plane stopped with an error: {e}"))?;
    tracing::info!("control plane stopped");
    Ok(())
}

/// The full application: the guarded API, then the unguarded static-panel fallback.
///
/// The composition order is load-bearing — see the module docs.
pub fn app(state: AppState) -> Router {
    api::guarded_router(state.clone())
        .fallback(panel_assets)
        .with_state(state)
}

/// Await the restart signal; returning is what tells `axum::serve` to drain and close the
/// listener, after which `serve` returns and `main` exits for the supervisor to restart.
async fn shutdown_signal(state: AppState) {
    state.restart.wait().await;
    tracing::info!("restart signal received; finishing in-flight requests before exiting");
}

// ---------------------------------------------------------------------------
// Static panel assets
// ---------------------------------------------------------------------------

async fn panel_assets(State(state): State<AppState>, request: Request) -> Response {
    // Belt for the guard: `/api/{*rest}` inside `api::guarded_router` normally answers these,
    // but an API path must never fall through to `index.html` — the panel's 401-bounce logic
    // parses that body as JSON and would loop on it.
    let path = request.uri().path();
    if path == "/api" || path.starts_with("/api/") {
        return api::api_not_found_response();
    }

    let Some(dir) = state.dirs.panel.as_deref() else {
        return text_response(
            StatusCode::OK,
            "text/html; charset=utf-8",
            MISSING_PANEL_PAGE.to_owned(),
        );
    };

    match panel_file(dir, path) {
        Some((file, content_type)) => match std::fs::read(&file) {
            Ok(bytes) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, content_type)],
                bytes,
            )
                .into_response(),
            Err(_) => index_or_404(dir, path),
        },
        None => index_or_404(dir, path),
    }
}

/// Serve `index.html` for extensionless paths (so a hand-typed deep link lands on the shell and
/// the hash router takes over) and a bare 404 for missing files that were clearly files.
fn index_or_404(dir: &FsPath, path: &str) -> Response {
    if !path.contains('.') {
        if let Ok(bytes) = std::fs::read(dir.join("index.html")) {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                bytes,
            )
                .into_response();
        }
    }
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "not found",
    )
        .into_response()
}

/// Map a URL path to a file under `dir`, or `None`.
///
/// The path is **not** percent-decoded — filenames in `panel/` are plain ASCII, so an encoded
/// path failing to match is correct behaviour — and any `.`/`..` component is refused outright,
/// so no traversal component ever reaches `join`.
fn panel_file(dir: &FsPath, path: &str) -> Option<(std::path::PathBuf, &'static str)> {
    let relative = path.strip_prefix('/').unwrap_or(path);
    let relative = if relative.is_empty() || relative.ends_with('/') {
        "index.html"
    } else {
        relative
    };
    if relative.split('/').any(|segment| {
        segment.is_empty() || segment == ".." || segment == "." || segment.contains('\\')
    }) {
        return None;
    }
    let candidate = dir.join(relative);
    if candidate.is_file() {
        return Some((candidate, content_type(relative)));
    }
    None
}

fn content_type(file: &str) -> &'static str {
    match file.rsplit('.').next().unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn text_response(status: StatusCode, content_type: &'static str, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// Shown when `panel/` could not be found — an explanation beats a blank 404 for the operator
/// who just installed the binary somewhere new.
const MISSING_PANEL_PAGE: &str = "<!doctype html>\
<html lang=\"en\"><meta charset=\"utf-8\">\
<title>Manarouter — panel not found</title>\
<body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto;line-height:1.6\">\
<h1>Panel assets not found</h1>\
<p>The control plane is running, but the <code>panel/</code> directory could not be located next \
to the working directory or the executable. Copy <code>panel/</code> from the repository (or the \
release archive) next to this binary, or start the process from the repository root. The API is \
unaffected: see <code>/api/health</code>.</p>\
</body></html>";
