//! OAuth discovery endpoints (RFC 9728 + RFC 8414) and base-URL derivation.
use axum::{Json, extract::State, http::HeaderMap};
use serde_json::json;

use super::AuthState;

/// Canonical base URL for this server. Configured `public_url` wins; otherwise
/// derived from the `Host` header (scheme from `X-Forwarded-Proto`, defaulting
/// to https for non-loopback hosts).
pub fn base_url(public_url: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(url) = public_url {
        return url.trim_end_matches('/').to_string();
    }
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if host.starts_with("localhost") || host.starts_with("127.") {
                "http".into()
            } else {
                "https".into()
            }
        });
    format!("{scheme}://{host}")
}

/// RFC 9728 — tells the client where to find authorization-server metadata.
pub async fn protected_resource(
    State(st): State<AuthState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let base = base_url(st.public_url.as_deref(), &headers);
    Json(json!({
        "resource": base,
        "authorization_servers": [base],
    }))
}

/// RFC 8414 — authorization-server metadata (endpoints + PKCE support).
pub async fn authorization_server(
    State(st): State<AuthState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let base = base_url(st.public_url.as_deref(), &headers);
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/authorize"),
        "token_endpoint": format!("{base}/token"),
        "registration_endpoint": format!("{base}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}
