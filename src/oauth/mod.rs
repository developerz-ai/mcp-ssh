//! Minimal embedded OAuth 2.1 authorization server, enough for Claude and other
//! MCP clients to connect: discovery metadata, dynamic client registration, and
//! the authorization-code + PKCE flow. The MCP server itself is the resource
//! server (see `crate::auth` for bearer validation on `/mcp`).
use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use crate::auth::Credentials;

mod flow;
mod metadata;
mod store;

pub use metadata::base_url;
pub use store::Store;

/// Shared state for both the OAuth routes and the `/mcp` auth middleware.
#[derive(Clone)]
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
        .route("/authorize", get(flow::authorize))
        .route("/token", post(flow::token))
        .route("/register", post(flow::register))
        .with_state(state)
}
