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
    /// REQUIRED (RFC 6749 §4.1.1). Must name a client registered via `/register`;
    /// it's what `redirect_uri` is checked against.
    client_id: String,
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

    // Client binding (OAuth 2.1 §4.1.2.1): the code may only be delivered to a URI
    // this client registered. Without it, a victim lured to an `/authorize` link
    // carrying an attacker's `redirect_uri` + PKCE challenge hands their code to the
    // attacker the moment they complete the Basic login. Unknown client or
    // unregistered URI → 400 inline, never a redirect to the unbound URI.
    if !st
        .store
        .client_allows_redirect(&p.client_id, &p.redirect_uri)
        .await
    {
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
    // Present for `authorization_code`; absent for `refresh_token`.
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    // Present for `refresh_token`.
    #[serde(default)]
    refresh_token: Option<String>,
}

/// `/token` — issue an access + refresh pair. Two grants: `authorization_code`
/// (code + PKCE verifier, first login) and `refresh_token` (silent renewal once
/// the access token expires, so the client doesn't re-run the browser flow).
pub async fn token(State(st): State<AuthState>, Form(p): Form<TokenParams>) -> Response {
    let issued = match p.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier), Some(redirect_uri)) =
                (p.code, p.code_verifier, p.redirect_uri)
            else {
                return bad_request("invalid_request");
            };
            st.store.redeem(&code, &verifier, &redirect_uri).await
        }
        "refresh_token" => {
            let Some(refresh_token) = p.refresh_token else {
                return bad_request("invalid_request");
            };
            st.store.refresh(&refresh_token).await
        }
        _ => return bad_request("unsupported_grant_type"),
    };
    match issued {
        Ok(tokens) => (
            // RFC 6749 §5.1 — token responses MUST NOT be cached.
            [("Cache-Control", "no-store")],
            Json(json!({
                "access_token": tokens.access,
                "token_type": "Bearer",
                "expires_in": store::TOKEN_TTL.as_secs(),
                "refresh_token": tokens.refresh,
            })),
        )
            .into_response(),
        Err(e) => bad_request(e),
    }
}

/// `/register` — minimal Dynamic Client Registration (RFC 7591). Clients are
/// public (PKCE, no secret), so the only thing persisted is the `client_id` →
/// `redirect_uri` binding that `/authorize` then enforces.
pub async fn register(
    State(st): State<AuthState>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let Some(redirect_uris) = registered_redirect_uris(body) else {
        return bad_request("invalid_redirect_uri");
    };
    let client_id = match st.store.register_client(&redirect_uris).await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e })),
            )
                .into_response();
        }
    };
    (
        StatusCode::CREATED,
        Json(json!({
            "client_id": client_id,
            "token_endpoint_auth_method": "none",
            "redirect_uris": redirect_uris,
        })),
    )
        .into_response()
}

/// The `redirect_uris` a DCR body is asking to bind, or `None` if the request
/// can't be honoured (→ `invalid_redirect_uri`, RFC 7591 §3.2.2). Rejects a
/// missing/non-array/non-string value, an empty list — REQUIRED for the
/// authorization-code grant, the only one we support, and a client bound to no
/// URI could never authorize — and any URI failing the same open-redirect rule
/// `/authorize` applies.
fn registered_redirect_uris(body: Option<Json<serde_json::Value>>) -> Option<Vec<String>> {
    let Json(body) = body?;
    let uris: Vec<String> = body
        .get("redirect_uris")?
        .as_array()?
        .iter()
        .map(|u| u.as_str().map(str::to_owned))
        .collect::<Option<_>>()?;
    (!uris.is_empty() && uris.iter().all(|u| is_allowed_redirect(u))).then_some(uris)
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
        let host = rest.split(['/', '?', '#']).next().unwrap_or("");
        return !host.trim().is_empty();
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

    /// Deserialize a response body as JSON.
    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 8192).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Register a client for `redirect_uri` through the real `/register` handler
    /// and return its `client_id` — `/authorize` now needs one.
    async fn register_client(st: &AuthState, redirect_uri: &str) -> String {
        let body = Json(json!({ "redirect_uris": [redirect_uri] }));
        let resp = register(State(st.clone()), Some(body)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        body_json(resp).await["client_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A valid `/authorize` request (correct PKCE, no state); tests tweak one field
    /// to exercise the branch they're about.
    fn authorize_params(client_id: &str, redirect_uri: &str) -> AuthorizeParams {
        AuthorizeParams {
            response_type: "code".into(),
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
            code_challenge: Some(CHALLENGE.into()),
            code_challenge_method: Some("S256".into()),
            state: None,
            resource: None,
        }
    }

    // --- /token ---

    fn auth_code_params(code: String) -> TokenParams {
        TokenParams {
            grant_type: "authorization_code".into(),
            code: Some(code),
            code_verifier: Some(VERIFIER.into()),
            redirect_uri: Some("http://cb".into()),
            refresh_token: None,
        }
    }

    #[tokio::test]
    async fn token_response_sets_cache_control_no_store() {
        let store = Store::new(crate::db::Db::memory());
        let code = store.new_code(CHALLENGE.into(), "http://cb".into()).await;
        let resp = token(State(test_state(store)), Form(auth_code_params(code))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
        );
    }

    #[tokio::test]
    async fn authorization_code_grant_returns_access_and_refresh() {
        let store = Store::new(crate::db::Db::memory());
        let code = store.new_code(CHALLENGE.into(), "http://cb".into()).await;
        let resp = token(State(test_state(store)), Form(auth_code_params(code))).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert!(body["access_token"].is_string());
        assert!(
            body["refresh_token"].is_string(),
            "must issue a refresh token"
        );
        assert_eq!(body["token_type"], "Bearer");
        assert_eq!(body["expires_in"], store::TOKEN_TTL.as_secs());
    }

    #[tokio::test]
    async fn refresh_token_grant_renews_silently() {
        let st = test_state(Store::new(crate::db::Db::memory()));
        // First login.
        let code = st
            .store
            .new_code(CHALLENGE.into(), "http://cb".into())
            .await;
        let first = body_json(token(State(st.clone()), Form(auth_code_params(code))).await).await;
        let refresh = first["refresh_token"].as_str().unwrap().to_string();

        // Exchange the refresh token — no code, no browser.
        let params = TokenParams {
            grant_type: "refresh_token".into(),
            code: None,
            code_verifier: None,
            redirect_uri: None,
            refresh_token: Some(refresh),
        };
        let resp = token(State(st.clone()), Form(params)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        let new_access = body["access_token"].as_str().unwrap();
        assert!(
            st.store.validate(new_access).await,
            "renewed token is valid"
        );
        assert_ne!(new_access, first["access_token"].as_str().unwrap());
    }

    #[tokio::test]
    async fn refresh_token_grant_rejects_unknown_token() {
        let params = TokenParams {
            grant_type: "refresh_token".into(),
            code: None,
            code_verifier: None,
            redirect_uri: None,
            refresh_token: Some("not-a-real-refresh-token".into()),
        };
        let resp = token(
            State(test_state(Store::new(crate::db::Db::memory()))),
            Form(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn token_rejects_unsupported_grant_type() {
        let params = TokenParams {
            grant_type: "client_credentials".into(),
            code: Some("irrelevant".into()),
            code_verifier: Some("irrelevant".into()),
            redirect_uri: Some("http://cb".into()),
            refresh_token: None,
        };
        let resp = token(
            State(test_state(Store::new(crate::db::Db::memory()))),
            Form(params),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "unsupported_grant_type");
    }

    // --- /authorize ---

    const CB: &str = "http://localhost/cb";

    #[tokio::test]
    async fn authorize_rejects_missing_pkce_s256() {
        // Valid Basic creds and a registered client, but no PKCE challenge →
        // invalid_request redirect.
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;
        let mut params = authorize_params(&client_id, CB);
        params.code_challenge = None;
        params.code_challenge_method = None;

        let resp = authorize(State(st), basic_headers("u", "p"), Query(params)).await;
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
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;
        let mut params = authorize_params(&client_id, CB);
        params.code_challenge = Some("abc".into());
        params.code_challenge_method = Some("plain".into());

        let resp = authorize(State(st), basic_headers("u", "p"), Query(params)).await;
        assert_eq!(resp.status(), StatusCode::FOUND);
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        assert!(
            location.contains("error=invalid_request"),
            "location={location}"
        );
    }

    #[tokio::test]
    async fn authorize_redirects_with_code_on_valid_basic_and_pkce() {
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;

        let resp = authorize(
            State(st),
            basic_headers("u", "p"),
            Query(authorize_params(&client_id, CB)),
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
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;

        let resp = authorize(
            State(st),
            basic_headers("u", "wrong"),
            Query(authorize_params(&client_id, CB)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn authorize_rejects_open_redirect_without_redirecting() {
        // Valid creds + PKCE, but a non-loopback http redirect_uri → 400, NOT a
        // redirect. The auth code must never leak to an attacker-controlled URI.
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;

        let resp = authorize(
            State(st),
            basic_headers("u", "p"),
            Query(authorize_params(&client_id, "http://evil.com/cb")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(
            resp.headers().get("Location").is_none(),
            "must not redirect to an unvalidated URI"
        );
    }

    #[tokio::test]
    async fn authorize_rejects_a_redirect_uri_the_client_did_not_register() {
        // The phishing case: everything is valid — Basic login, PKCE, and an https
        // redirect_uri that passes `is_allowed_redirect` — but the client never
        // registered that URI, so the code must not be issued or redirected.
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;

        let resp = authorize(
            State(st),
            basic_headers("u", "p"),
            Query(authorize_params(&client_id, "https://attacker.example/cb")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "invalid_request");
    }

    #[tokio::test]
    async fn authorize_rejects_an_unregistered_client_id() {
        // No registration at all → nothing to bind the redirect_uri to. Fails
        // closed rather than falling back to "any allowed URI".
        let st = test_state(Store::new(crate::db::Db::memory()));

        let resp = authorize(
            State(st),
            basic_headers("u", "p"),
            Query(authorize_params("never-registered", CB)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(resp.headers().get("Location").is_none());
    }

    #[tokio::test]
    async fn authorize_rejects_another_clients_redirect_uri() {
        // Two registered clients: one may not borrow the other's callback.
        let st = test_state(Store::new(crate::db::Db::memory()));
        let victim = register_client(&st, CB).await;
        let attacker = register_client(&st, "https://attacker.example/cb").await;
        assert_ne!(victim, attacker);

        let resp = authorize(
            State(st),
            basic_headers("u", "p"),
            Query(authorize_params(&victim, "https://attacker.example/cb")),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "invalid_request");
    }

    #[tokio::test]
    async fn a_registered_client_completes_register_authorize_token() {
        // End to end over the bound URI: the single-use code + PKCE guards still
        // hold, and the code redeems for a token pair.
        let st = test_state(Store::new(crate::db::Db::memory()));
        let client_id = register_client(&st, CB).await;

        let resp = authorize(
            State(st.clone()),
            basic_headers("u", "p"),
            Query(authorize_params(&client_id, CB)),
        )
        .await;
        let location = resp.headers().get("Location").unwrap().to_str().unwrap();
        let code = location
            .strip_prefix("http://localhost/cb?code=")
            .expect("code on the registered redirect_uri")
            .to_string();

        let params = || TokenParams {
            grant_type: "authorization_code".into(),
            code: Some(code.clone()),
            code_verifier: Some(VERIFIER.into()),
            redirect_uri: Some(CB.into()),
            refresh_token: None,
        };
        let resp = token(State(st.clone()), Form(params())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(body_json(resp).await["access_token"].is_string());

        // Still single-use.
        let resp = token(State(st), Form(params())).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "invalid_grant");
    }

    // --- /register ---

    #[tokio::test]
    async fn register_echoes_redirect_uris_and_binds_them() {
        let st = test_state(Store::new(crate::db::Db::memory()));
        let body = Json(json!({ "redirect_uris": [CB] }));

        let resp = register(State(st.clone()), Some(body)).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let json = body_json(resp).await;
        assert_eq!(json["redirect_uris"][0], CB);
        let client_id = json["client_id"].as_str().unwrap();

        assert!(
            st.store.client_allows_redirect(client_id, CB).await,
            "the echoed id must be bound to the URI it registered"
        );
    }

    #[tokio::test]
    async fn register_rejects_open_redirect_uri() {
        let st = test_state(Store::new(crate::db::Db::memory()));
        let body = Json(json!({ "redirect_uris": ["http://evil.com/cb"] }));
        let resp = register(State(st), Some(body)).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(resp).await["error"], "invalid_redirect_uri");
    }

    #[tokio::test]
    async fn register_rejects_a_body_without_usable_redirect_uris() {
        // RFC 7591 §2 makes redirect_uris REQUIRED for the authorization-code
        // grant; a client bound to nothing could never authorize, so 201 would be
        // a lie. Missing body, missing key, empty list, and non-strings all fail.
        let st = test_state(Store::new(crate::db::Db::memory()));
        for body in [
            None,
            Some(Json(json!({}))),
            Some(Json(json!({ "redirect_uris": [] }))),
            Some(Json(json!({ "redirect_uris": CB }))),
            Some(Json(json!({ "redirect_uris": [42] }))),
        ] {
            let resp = register(State(st.clone()), body).await;
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            assert_eq!(body_json(resp).await["error"], "invalid_redirect_uri");
        }
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
        // empty/whitespace host after the scheme is still no host → rejected.
        assert!(!is_allowed_redirect("https://?evil"));
        assert!(!is_allowed_redirect("https:// /cb"));
        // valid https URL with query/fragment still accepted.
        assert!(is_allowed_redirect("https://example.com?next=/x"));
    }
}
