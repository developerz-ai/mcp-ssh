//! Minimal embedded OAuth 2.1 authorization server, enough for Claude and other
//! MCP clients to connect: discovery metadata, dynamic client registration, and
//! the authorization-code + PKCE flow behind a consent form. The MCP server itself
//! is the resource server (see `crate::auth` for bearer validation on `/mcp`).
use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};

use crate::auth::Credentials;

mod consent;
mod flow;
mod metadata;
mod pending;
mod secret;
mod store;

pub use metadata::base_url;
pub use store::{
    Store, revoke_all, sweep_expired_access, sweep_expired_refresh, sweep_stale_clients,
};

/// Largest request body the OAuth routes accept. Every legitimate body (a token
/// request, the consent form, a registration of ≤5 short URIs) is far smaller.
const OAUTH_BODY_LIMIT: usize = 64 * 1024;

/// Shared state for both the OAuth routes and the `/mcp` auth middleware.
#[derive(Debug, Clone)]
pub struct AuthState {
    pub creds: Credentials,
    pub store: Arc<Store>,
    pub public_url: Option<String>,
}

/// Public OAuth + discovery routes (no auth middleware — they bootstrap auth).
pub fn router(state: AuthState) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(metadata::protected_resource),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(metadata::authorization_server),
        )
        .route("/authorize", get(flow::authorize).post(flow::approve))
        .route("/token", post(flow::token))
        .route("/register", post(flow::register))
        .layer(DefaultBodyLimit::max(OAUTH_BODY_LIMIT))
        .with_state(state)
}
