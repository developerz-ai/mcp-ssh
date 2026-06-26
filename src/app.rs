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
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use base64::Engine;
    use tower::ServiceExt; // oneshot

    fn test_app() -> Router {
        let dir = tempfile::tempdir().unwrap().keep();
        let store = JobStore::new(dir, std::time::Duration::from_secs(2)).unwrap();
        let state = oauth::AuthState {
            creds: auth::Credentials {
                user: "admin".into(),
                pass: "secret".into(),
            },
            store: Arc::new(oauth::Store::default()),
            public_url: None,
        };
        build(state, store, vec!["localhost".into(), "127.0.0.1".into()])
    }

    #[tokio::test]
    async fn discovery_metadata_is_public() {
        let res = test_app()
            .oneshot(
                Request::builder()
                    .uri("/.well-known/oauth-authorization-server")
                    .header(header::HOST, "mcp.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_without_auth_returns_401_with_oauth_challenge() {
        let res = test_app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "mcp.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let challenge = res
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(challenge.contains("Bearer"));
        assert!(challenge.contains("oauth-protected-resource"));
    }

    #[tokio::test]
    async fn mcp_with_basic_auth_passes_the_guard() {
        // A bad body still gets past auth (real MCP needs more headers); the point
        // is the auth layer accepts valid Basic creds rather than 401-ing.
        let creds = base64::engine::general_purpose::STANDARD.encode("admin:secret");
        let res = test_app()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "mcp.example.com")
                    .header(header::AUTHORIZATION, format!("Basic {creds}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(res.status(), StatusCode::UNAUTHORIZED);
    }
}
