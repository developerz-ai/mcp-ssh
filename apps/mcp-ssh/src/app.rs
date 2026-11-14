//! Builds the axum app: `/mcp` (auth-guarded) merged with the public OAuth routes,
//! both behind the `Host` allowlist; `/healthz` outside it for local probes.
//! Kept separate from `main` so it can be exercised without binding a socket.
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header, uri::Authority};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;

use crate::{auth, jobs::JobStore, oauth, tools};

/// `shutdown`, once cancelled, ends every open MCP session and its SSE stream, so
/// a graceful shutdown isn't held open by long-lived client connections.
pub fn build(
    auth_state: oauth::AuthState,
    store: JobStore,
    allowed_hosts: Vec<String>,
    shutdown: CancellationToken,
) -> Router {
    let mut http = StreamableHttpServerConfig::default();
    http.legacy_session_mode = true;
    http.allowed_hosts = allowed_hosts.clone();
    http.cancellation_token = shutdown;

    let agents = auth_state.store.clone();
    let service = StreamableHttpService::new(
        move || Ok(tools::Tools::new(store.clone(), agents.clone())),
        Arc::new(LocalSessionManager::default()),
        http,
    );

    let mcp = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(from_fn_with_state(auth_state.clone(), auth::require_auth));
    // The OAuth routes build their issuer and endpoint URLs (and `/mcp`'s 401 its
    // challenge) from `Host` when no `public_url` is set, so the allowlist guards
    // them all — rmcp's own check only covers `/mcp`, and only after auth.
    let guarded = mcp
        .merge(oauth::router(auth_state))
        .layer(from_fn_with_state(
            Arc::<[String]>::from(allowed_hosts),
            host_guard,
        ));
    let health = axum::Router::new().route("/healthz", get(|| async { "ok" }));
    guarded.merge(health)
}

/// Refuse a request whose `Host` (or HTTP/2 `:authority`) isn't on the allowlist:
/// 403, or 400 when there is no parseable host at all. Same matching as rmcp's
/// guard: case-insensitive host, and the port only when the entry names one.
async fn host_guard(State(allowed): State<Arc<[String]>>, req: Request, next: Next) -> Response {
    let authority = match req.headers().get(header::HOST) {
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|h| h.parse::<Authority>().ok()),
        None => req.uri().authority().cloned(),
    };
    match authority {
        Some(a) if host_allowed(&a, &allowed) => next.run(req).await,
        Some(_) => (
            StatusCode::FORBIDDEN,
            "Forbidden: Host header is not allowed",
        )
            .into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            "Bad Request: missing or invalid Host header",
        )
            .into_response(),
    }
}

fn host_allowed(request: &Authority, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        // rmcp's semantics; config never produces an empty list (see `config.rs`).
        return true;
    }
    let host = normalize_host(request.host());
    allowed
        .iter()
        .any(|entry| match entry.trim().parse::<Authority>() {
            Ok(a) => {
                normalize_host(a.host()) == host
                    && a.port_u16().is_none_or(|p| request.port_u16() == Some(p))
            }
            Err(_) => normalize_host(entry.trim()) == host,
        })
}

fn normalize_host(host: &str) -> String {
    host.trim_matches(['[', ']']).to_ascii_lowercase()
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
