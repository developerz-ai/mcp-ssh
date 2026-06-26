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
        return token_error("unsupported_grant_type");
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
        Err(e) => token_error(e),
    }
}

/// `/register` — minimal Dynamic Client Registration (RFC 7591). We don't track
/// clients (public + PKCE), so accept anything and echo a generated id.
pub async fn register(body: Option<Json<serde_json::Value>>) -> Response {
    let redirect_uris = body
        .and_then(|Json(v)| v.get("redirect_uris").cloned())
        .unwrap_or_else(|| json!([]));
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

fn token_error(error: &str) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header;
    use store::Store;

    #[tokio::test]
    async fn token_response_sets_cache_control_no_store() {
        let store = Store::default();
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        let code = store.new_code(challenge.into(), "http://cb".into()).await;

        let state = AuthState {
            creds: crate::auth::Credentials {
                user: "u".into(),
                pass: "p".into(),
            },
            store: std::sync::Arc::new(store),
            public_url: None,
        };
        let params = TokenParams {
            grant_type: "authorization_code".into(),
            code,
            code_verifier: verifier.into(),
            redirect_uri: "http://cb".into(),
        };
        let resp = token(State(state), Form(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
        );
    }
}
