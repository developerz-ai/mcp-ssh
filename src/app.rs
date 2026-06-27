//! Builds the axum app: `/mcp` (auth-guarded) merged with the public OAuth routes.
//! Kept separate from `main` so it can be exercised without binding a socket.
use std::sync::Arc;

use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use crate::{auth, jobs::JobStore, oauth, tools};

pub fn build(auth_state: oauth::AuthState, store: JobStore, allowed_hosts: Vec<String>) -> Router {
    let mut http = StreamableHttpServerConfig::default();
    http.stateful_mode = true;
    http.allowed_hosts = allowed_hosts;

    let service = StreamableHttpService::new(
        move || Ok(tools::Tools::new(store.clone())),
        Arc::new(LocalSessionManager::default()),
        http,
    );

    let mcp = axum::Router::new().nest_service("/mcp", service).layer(
        axum::middleware::from_fn_with_state(auth_state.clone(), auth::require_auth),
    );
    mcp.merge(oauth::router(auth_state))
}

#[cfg(test)]
#[path = "app_tests.rs"]
mod tests;
