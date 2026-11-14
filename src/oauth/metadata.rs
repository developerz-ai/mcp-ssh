//! OAuth discovery endpoints (RFC 9728 + RFC 8414) and base-URL derivation.
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, uri::Authority},
};
use serde_json::json;

use super::AuthState;

/// Canonical base URL for this server. A configured `public_url` (already
/// normalised by config) wins, and then the request is never consulted. Otherwise
/// it is derived from the `Host` header — which the app's host guard has checked
/// against the allowlist — with the scheme from `X-Forwarded-Proto` only when that
/// says `http` or `https`, defaulting to https off-loopback.
pub fn base_url(public_url: Option<&str>, headers: &HeaderMap) -> String {
    if let Some(url) = public_url {
        return url.trim_end_matches('/').to_string();
    }
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<Authority>().ok())
        .filter(|a| !a.as_str().contains('@'))
        .map_or_else(|| "localhost".to_string(), |a| a.as_str().to_string());
    let forwarded = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase());
    let scheme = match forwarded.as_deref() {
        Some(s @ ("http" | "https")) => s,
        _ if host.starts_with("localhost")
            || host.starts_with("127.")
            || host.starts_with("[::1]") =>
        {
            "http"
        }
        _ => "https",
    };
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
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::HeaderValue;

    use super::*;
    use crate::{auth::Credentials, oauth::Store};

    fn state_with_url(url: &str) -> AuthState {
        AuthState {
            creds: Credentials {
                user: "u".into(),
                pass: crate::config::Password::new("p"),
            },
            store: Arc::new(Store::new(crate::db::Db::memory())),
            public_url: Some(url.into()),
        }
    }

    fn state_no_url() -> AuthState {
        AuthState {
            creds: Credentials {
                user: "u".into(),
                pass: crate::config::Password::new("p"),
            },
            store: Arc::new(Store::new(crate::db::Db::memory())),
            public_url: None,
        }
    }

    // --- base_url ---

    #[test]
    fn base_url_prefers_public_url_and_strips_trailing_slash() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("other.example.com"));
        assert_eq!(
            base_url(Some("https://my.example.com/"), &headers),
            "https://my.example.com"
        );
    }

    #[test]
    fn base_url_uses_x_forwarded_proto_with_host() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("mcp.example.com"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(base_url(None, &headers), "https://mcp.example.com");
    }

    #[test]
    fn base_url_defaults_to_https_for_non_localhost() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("mcp.example.com"));
        assert_eq!(base_url(None, &headers), "https://mcp.example.com");
    }

    #[test]
    fn base_url_defaults_to_http_for_localhost() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("localhost:3000"));
        assert_eq!(base_url(None, &headers), "http://localhost:3000");
    }

    #[test]
    fn base_url_defaults_to_http_for_loopback() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("127.0.0.1:8080"));
        assert_eq!(base_url(None, &headers), "http://127.0.0.1:8080");
    }

    #[test]
    fn base_url_ignores_a_non_http_forwarded_proto() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("mcp.example.com"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("javascript"));
        assert_eq!(base_url(None, &headers), "https://mcp.example.com");
    }

    #[test]
    fn base_url_never_reflects_a_malformed_host() {
        for bad in ["evil\"host", "a@b.example", "x/y"] {
            let mut headers = HeaderMap::new();
            headers.insert("host", HeaderValue::from_str(bad).unwrap());
            assert_eq!(base_url(None, &headers), "http://localhost", "{bad}");
        }
    }

    // --- metadata JSON shape ---

    #[tokio::test]
    async fn authorization_server_metadata_contains_required_fields() {
        let Json(meta) = authorization_server(
            State(state_with_url("https://mcp.example.com")),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(meta["issuer"], "https://mcp.example.com");
        assert_eq!(
            meta["authorization_endpoint"],
            "https://mcp.example.com/authorize"
        );
        assert_eq!(meta["token_endpoint"], "https://mcp.example.com/token");
        assert_eq!(
            meta["registration_endpoint"],
            "https://mcp.example.com/register"
        );
        assert_eq!(meta["code_challenge_methods_supported"][0], "S256");
        assert_eq!(meta["grant_types_supported"][0], "authorization_code");
    }

    #[tokio::test]
    async fn protected_resource_metadata_contains_required_fields() {
        let Json(meta) = protected_resource(
            State(state_with_url("https://mcp.example.com")),
            HeaderMap::new(),
        )
        .await;
        assert_eq!(meta["resource"], "https://mcp.example.com");
        assert_eq!(meta["authorization_servers"][0], "https://mcp.example.com");
    }

    #[tokio::test]
    async fn metadata_falls_back_to_host_header_when_no_public_url() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("box.internal"));
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        let Json(meta) = authorization_server(State(state_no_url()), headers).await;
        assert_eq!(meta["issuer"], "https://box.internal");
        assert_eq!(
            meta["authorization_endpoint"],
            "https://box.internal/authorize"
        );
    }
}
