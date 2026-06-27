//! Authorization Code + PKCE flow: `/authorize`, `/token`, `/register`.
use axum::{
    Json,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use super::{AuthState, store};
use crate::auth;

#[derive(Deserialize)]
pub struct AuthorizeParams {
    response_type: String,
    redirect_uri: String,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    #[serde(default)]
    state: Option<String>,
    // RFC 8707: accepted (Claude sends it) but not enforced — we're the only
    // resource server, so every token's audience is implicitly this host.
    #[serde(default)]
    #[allow(dead_code)]
    resource: Option<String>,
}

/// `/authorize` — authenticate the resource owner with HTTP Basic (browser shows
/// the native prompt), then issue an auth code and redirect back to the client.
pub async fn authorize(
    State(st): State<AuthState>,
    headers: HeaderMap,
    Query(p): Query<AuthorizeParams>,
) -> Response {
    if !auth::check_basic(&headers, &st.creds) {
        // 401 with a Basic challenge → the browser pops a login dialog.
        return (
            StatusCode::UNAUTHORIZED,
            [("WWW-Authenticate", "Basic realm=\"mcp-ssh\"")],
            "authentication required",
        )
            .into_response();
    }

    // Open-redirect guard (OAuth 2.1 §4.1.1 / MCP spec MUST): this URI is about to
    // receive an auth code, so validate it *before* any redirect. Reject inline with
    // 400 — never bounce the user-agent to an unvalidated `redirect_uri`.
    if !is_allowed_redirect(&p.redirect_uri) {
        return bad_request("invalid_request");
    }

    if p.response_type != "code" {
        return redirect_error(
            &p.redirect_uri,
            "unsupported_response_type",
            p.state.as_deref(),
        );
    }
    // PKCE S256 is mandatory for MCP clients.
    let Some(challenge) = p
        .code_challenge
        .filter(|_| p.code_challenge_method.as_deref() == Some("S256"))
    else {
        return redirect_error(&p.redirect_uri, "invalid_request", p.state.as_deref());
    };

    let code = st.store.new_code(challenge, p.redirect_uri.clone()).await;
    let sep = if p.redirect_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    let mut location = format!(
        "{}{}code={}",
        p.redirect_uri,
        sep,
        urlencoding::encode(&code)
    );
    if let Some(s) = p.state {
        location.push_str(&format!("&state={}", urlencoding::encode(&s)));
    }
    (StatusCode::FOUND, [("Location", location)]).into_response()
}

#[derive(Deserialize)]
pub struct TokenParams {
    grant_type: String,
    code: String,
    code_verifier: String,
    redirect_uri: String,
}

/// `/token` — exchange an auth code + PKCE verifier for an opaque access token.
pub async fn token(State(st): State<AuthState>, Form(p): Form<TokenParams>) -> Response {
    if p.grant_type != "authorization_code" {
        return bad_request("unsupported_grant_type");
    }
    match st
        .store
        .redeem(&p.code, &p.code_verifier, &p.redirect_uri)
        .await
    {
        Ok(access_token) => (
            // RFC 6749 §5.1 — token responses MUST NOT be cached.
            [("Cache-Control", "no-store")],
            Json(json!({
                "access_token": access_token,
                "token_type": "Bearer",
                "expires_in": store::TOKEN_TTL.as_secs(),
            })),
        )
            .into_response(),
        Err(e) => bad_request(e),
    }
}

/// `/register` — minimal Dynamic Client Registration (RFC 7591). We don't track
/// clients (public + PKCE), so accept anything and echo a generated id.
pub async fn register(body: Option<Json<serde_json::Value>>) -> Response {
    let redirect_uris = body
        .and_then(|Json(v)| v.get("redirect_uris").cloned())
        .unwrap_or_else(|| json!([]));
    // Same rule as /authorize: refuse to register an http non-loopback (or otherwise
    // malformed) redirect URI. RFC 7591 §3.2.2 → invalid_redirect_uri.
    let has_disallowed_uri = redirect_uris.as_array().is_some_and(|uris| {
        uris.iter()
            .any(|u| u.as_str().is_none_or(|s| !is_allowed_redirect(s)))
    });
    if has_disallowed_uri {
        return bad_request("invalid_redirect_uri");
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": store::random_token(),
            "token_endpoint_auth_method": "none",
            "redirect_uris": redirect_uris,
        })),
    )
        .into_response()
}

fn bad_request(error: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response()
}

fn redirect_error(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let mut location = format!("{redirect_uri}{sep}error={error}");
    if let Some(s) = state {
        location.push_str(&format!("&state={}", urlencoding::encode(s)));
    }
    (StatusCode::FOUND, [("Location", location)]).into_response()
}

/// OAuth 2.1 redirect-URI safety rule: only `https` (anywhere) or a loopback
/// (`localhost`/`127.0.0.1`) HTTP address may receive an auth code — everything
/// else is an open-redirect vector. Hand-rolled (no `url` dep) and fails closed:
/// anything it can't confidently classify as loopback is rejected.
fn is_allowed_redirect(uri: &str) -> bool {
    if let Some(rest) = uri.strip_prefix("https://") {
        return !rest.is_empty();
    }
    if let Some(rest) = uri.strip_prefix("http://") {
        // Host is everything before the first `/`, `?`, `#`, then strip an optional
        // `:port`. Any userinfo (`user@host`) keeps the `@` in the slice, so spoofs
        // like `http://localhost@evil.com` never equal a bare loopback host.
        let host = rest
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("");
        return host == "localhost" || host == "127.0.0.1";
    }
    false
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::http::{HeaderValue, header};
    use base64::Engine;

    use super::*;
    use store::Store;

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn test_state(store: Store) -> AuthState {
        AuthState {
            creds: crate::auth::Credentials {
                user: "u".into(),
                pass: "p".into(),
            },
            store: Arc::new(store),
            public_url: None,
        }
    }

    fn basic_headers(user: &str, pass: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let enc = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {enc}")).unwrap(),
        );
        headers
    }

    // --- /token ---

    #[tokio::test]
    async fn token_response_sets_cache_control_no_store() {
        let store = Store::default();
        let code = store.new_code(CHALLENGE.into(), "http://cb".into()).await;
        let params = TokenParams {
            grant_type: "authorization_code".into(),
            code,
            code_verifier: VERIFIER.into(),
            redirect_uri: "http://cb".into(),
        };
        let resp = token(State(test_state(store)), Form(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
        );
    }

    #[tokio::test]
    async fn token_rejects_unsupported_grant_type() {
        let params = TokenParams {
            grant_type: "client_credentials".into(),
            code: "irrelevant".into(),
            code_verifier: "irrelevant".into(),
            redirect_uri: "http://cb".into(),
        };
        let resp = token(State(test_state(Store::default())), Form(params)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "unsupported_grant_type");
    }

    // --- /authorize ---

    #[tokio::test]
    async fn authorize_rejects_missing_pkce_s256() {
        // Valid Basic creds but no PKCE challenge → invalid_request redirect.
        let params = AuthorizeParams {
            response_type: "code".into(),
            redirect_uri: "http://localhost/cb".into(),
            code_challenge: None,
            code_challenge_method: None,
            state: None,
            resource: None,
        };
        let resp = authorize(
            State(test_state(Store::default())),
            basic_headers("u", "p"),
            Query(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(
            location.contains("error=invalid_request"),
            "location={location}"
        );
    }

    #[tokio::test]
    async fn authorize_rejects_plain_challenge_method() {
        // challenge present but method != S256 → invalid_request.
        let params = AuthorizeParams {
            response_type: "code".into(),
            redirect_uri: "http://localhost/cb".into(),
            code_challenge: Some("abc".into()),
            code_challenge_method: Some("plain".into()),
            state: None,
            resource: None,
        };
        let resp = authorize(
            State(test_state(Store::default())),
            basic_headers("u", "p"),
            Query(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(
            location.contains("error=invalid_request"),
            "location={location}"
        );
    }

    #[tokio::test]
    async fn authorize_redirects_with_code_on_valid_basic_and_pkce() {
        let params = AuthorizeParams {
            response_type: "code".into(),
            redirect_uri: "http://localhost/cb".into(),
            code_challenge: Some(CHALLENGE.into()),
            code_challenge_method: Some("S256".into()),
            state: None,
            resource: None,
        };
        let resp = authorize(
            State(test_state(Store::default())),
            basic_headers("u", "p"),
            Query(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(
            location.starts_with("http://localhost/cb?code="),
            "location={location}"
        );
    }

    #[tokio::test]
    async fn authorize_returns_401_on_bad_credentials() {
        let params = AuthorizeParams {
            response_type: "code".into(),
            redirect_uri: "http://localhost/cb".into(),
            code_challenge: Some(CHALLENGE.into()),
            code_challenge_method: Some("S256".into()),
            state: None,
            resource: None,
        };
        let resp = authorize(
            State(test_state(Store::default())),
            basic_headers("u", "wrong"),
            Query(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authorize_rejects_open_redirect_without_redirecting() {
        // Valid creds + PKCE, but a non-loopback http redirect_uri → 400, NOT a
        // redirect. The auth code must never leak to an attacker-controlled URI.
        let params = AuthorizeParams {
            response_type: "code".into(),
            redirect_uri: "http://evil.com/cb".into(),
            code_challenge: Some(CHALLENGE.into()),
            code_challenge_method: Some("S256".into()),
            state: None,
            resource: None,
        };
        let resp = authorize(
            State(test_state(Store::default())),
            basic_headers("u", "p"),
            Query(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers().get("Location").is_none(),
            "must not redirect to an unvalidated URI"
        );
    }

    // --- /register ---

    #[tokio::test]
    async fn register_echoes_redirect_uris() {
        let body = Json(serde_json::json!({ "redirect_uris": ["http://localhost/cb"] }));
        let resp = register(Some(body)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["redirect_uris"][0], "http://localhost/cb");
        assert!(json["client_id"].is_string());
    }

    #[tokio::test]
    async fn register_rejects_open_redirect_uri() {
        let body = Json(serde_json::json!({ "redirect_uris": ["http://evil.com/cb"] }));
        let resp = register(Some(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "invalid_redirect_uri");
    }

    #[tokio::test]
    async fn register_with_no_body_returns_empty_redirect_uris() {
        let resp = register(None).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["redirect_uris"], serde_json::json!([]));
    }

    // --- redirect-uri allow rules ---

    #[test]
    fn is_allowed_redirect_accepts_https_and_loopback_only() {
        // https anywhere is fine (OAuth 2.1).
        assert!(is_allowed_redirect("https://example.com/cb"));
        assert!(is_allowed_redirect("https://evil.com"));
        // loopback over http is fine, with or without a port.
        assert!(is_allowed_redirect("http://localhost/cb"));
        assert!(is_allowed_redirect("http://localhost:8080/cb"));
        assert!(is_allowed_redirect("http://127.0.0.1/cb"));

        // http non-loopback is the open-redirect vector → rejected.
        assert!(!is_allowed_redirect("http://evil.com/cb"));
        // subdomain + userinfo spoofs must not pass as loopback.
        assert!(!is_allowed_redirect("http://localhost.evil.com/cb"));
        assert!(!is_allowed_redirect("http://localhost@evil.com/cb"));
        // non-http(s) schemes and junk fail closed.
        assert!(!is_allowed_redirect("ftp://localhost/cb"));
        assert!(!is_allowed_redirect("not a url"));
        assert!(!is_allowed_redirect("https://"));
    }
}
