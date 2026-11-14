//! The flow over the real OAuth router: register → GET /authorize (form) → POST
//! /authorize (code) → /token.
use std::sync::Arc;

use axum::{Router, body::Body, http::Request};
use tower::ServiceExt;

use super::*;
use crate::config::Password;
use crate::oauth::store;
use crate::oauth::{Store, router};

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const CB: &str = "http://localhost/cb";
const PASS: &str = "hunter2-pass";

fn app() -> Router {
    router(AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: Password::new(PASS),
        },
        store: Arc::new(Store::new(crate::db::Db::memory())),
        public_url: None,
    })
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap()
    }

    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }

    /// The consent nonce embedded in the rendered form.
    fn csrf(&self) -> String {
        let marker = "name=\"csrf_token\" value=\"";
        let start = self.body.find(marker).expect("form has a csrf_token") + marker.len();
        self.body[start..].split('"').next().unwrap().to_string()
    }
}

async fn send(app: &Router, req: Request<Body>) -> Reply {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn post_form(app: &Router, path: &str, fields: &[(&str, &str)]) -> Reply {
    let body = fields
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap();
    send(app, req).await
}

async fn post_json(app: &Router, path: &str, body: serde_json::Value) -> Reply {
    let req = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(app, req).await
}

async fn register_client(app: &Router, redirect_uri: &str) -> String {
    let reply = post_json(app, "/register", json!({ "redirect_uris": [redirect_uri] })).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    reply.json()["client_id"].as_str().unwrap().to_string()
}

fn authorize_url(client_id: &str, redirect_uri: &str, extra: &str) -> String {
    format!(
        "/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={CHALLENGE}&code_challenge_method=S256{extra}",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
    )
}

async fn get(app: &Router, uri: &str) -> Reply {
    send(app, Request::get(uri).body(Body::empty()).unwrap()).await
}

/// GET the consent form for a fresh client and return (client_id, csrf).
async fn consent(app: &Router) -> (String, String) {
    let client_id = register_client(app, CB).await;
    let form = get(app, &authorize_url(&client_id, CB, "&state=xyz")).await;
    assert_eq!(form.status, StatusCode::OK, "{}", form.body);
    (client_id, form.csrf())
}

async fn login(app: &Router, csrf: &str, pass: &str) -> Reply {
    post_form(
        app,
        "/authorize",
        &[
            ("csrf_token", csrf),
            ("username", "admin"),
            ("password", pass),
        ],
    )
    .await
}

fn code_of(location: &str) -> String {
    let query = location.split_once('?').unwrap().1;
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("code="))
        .expect("code in location")
        .to_string()
}

async fn exchange(app: &Router, code: &str, client_id: &str) -> Reply {
    post_form(
        app,
        "/token",
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", VERIFIER),
            ("redirect_uri", CB),
            ("client_id", client_id),
        ],
    )
    .await
}

fn assert_no_secret(reply: &Reply, secrets: &[&str]) {
    for s in secrets {
        assert!(
            !reply.body.contains(s),
            "body leaks a secret: {}",
            reply.body
        );
    }
}

#[tokio::test]
async fn the_full_flow_issues_a_code_only_after_the_form_post() {
    let app = app();
    let client_id = register_client(&app, CB).await;

    let form = get(&app, &authorize_url(&client_id, CB, "&state=xyz")).await;
    assert_eq!(form.status, StatusCode::OK);
    assert!(
        form.headers.get(header::LOCATION).is_none(),
        "GET never redirects"
    );
    assert!(!form.body.contains("code="), "GET never carries a code");
    assert_eq!(form.headers[header::CACHE_CONTROL], "no-store");
    assert!(form.body.contains(CB), "the destination is shown");

    let approved = login(&app, &form.csrf(), PASS).await;
    assert_eq!(approved.status, StatusCode::FOUND, "{}", approved.body);
    assert!(approved.location().starts_with("http://localhost/cb?code="));
    assert!(approved.location().ends_with("&state=xyz"));
    let code = code_of(approved.location());

    let tokens = exchange(&app, &code, &client_id).await;
    assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.body);
    assert_eq!(tokens.headers[header::CACHE_CONTROL], "no-store");
    let body = tokens.json();
    assert!(body["access_token"].is_string() && body["refresh_token"].is_string());
    assert_eq!(body["token_type"], "Bearer");
    assert_eq!(body["expires_in"], store::TOKEN_TTL.as_secs());

    // The code is single-use.
    let again = exchange(&app, &code, &client_id).await;
    assert_eq!(again.status, StatusCode::BAD_REQUEST);
    assert_eq!(again.json()["error"], "invalid_grant");
    assert_eq!(again.headers[header::CACHE_CONTROL], "no-store");

    // And the refresh grant renews.
    let refreshed = post_form(
        &app,
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", body["refresh_token"].as_str().unwrap()),
            ("client_id", &client_id),
        ],
    )
    .await;
    assert_eq!(refreshed.status, StatusCode::OK, "{}", refreshed.body);
    assert_eq!(refreshed.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn a_forged_or_replayed_csrf_token_is_forbidden() {
    let app = app();
    let (_, csrf) = consent(&app).await;
    let forged = login(&app, "not-a-real-token", PASS).await;
    assert_eq!(forged.status, StatusCode::FORBIDDEN);
    assert!(forged.headers.get(header::LOCATION).is_none());

    assert_eq!(login(&app, &csrf, PASS).await.status, StatusCode::FOUND);
    let replay = login(&app, &csrf, PASS).await;
    assert_eq!(replay.status, StatusCode::FORBIDDEN, "single use");
}

#[tokio::test(start_paused = true)]
async fn wrong_credentials_re_render_the_form_with_a_fresh_token() {
    let app = app();
    let (_, csrf) = consent(&app).await;
    let denied = login(&app, &csrf, "wrong").await;
    assert_eq!(denied.status, StatusCode::UNAUTHORIZED);
    assert!(denied.headers.get(header::LOCATION).is_none());
    assert_no_secret(&denied, &[PASS, "wrong"]);
    let retry = denied.csrf();
    assert_ne!(retry, csrf);

    assert_eq!(
        login(&app, &csrf, PASS).await.status,
        StatusCode::FORBIDDEN,
        "old token spent"
    );
    assert_eq!(login(&app, &retry, PASS).await.status, StatusCode::FOUND);
}

#[tokio::test]
async fn authorize_get_with_basic_credentials_still_issues_nothing() {
    use base64::Engine;
    let app = app();
    let client_id = register_client(&app, CB).await;
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("admin:{PASS}"));
    let req = Request::get(authorize_url(&client_id, CB, ""))
        .header(header::AUTHORIZATION, format!("Basic {basic}"))
        .body(Body::empty())
        .unwrap();
    let reply = send(&app, req).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.headers.get(header::LOCATION).is_none());
}

#[tokio::test]
async fn authorize_rejects_bad_requests_before_rendering() {
    let app = app();
    let client_id = register_client(&app, CB).await;
    let other = register_client(&app, "https://attacker.example/cb").await;

    // Inline 400, never a redirect: open redirect, unregistered URI, another
    // client's URI, unknown client.
    for (id, uri) in [
        (client_id.as_str(), "http://evil.com/cb"),
        (client_id.as_str(), "https://attacker.example/cb"),
        (other.as_str(), CB),
        ("never-registered", CB),
    ] {
        let reply = get(&app, &authorize_url(id, uri, "")).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{id} {uri}");
        assert!(reply.headers.get(header::LOCATION).is_none());
        assert_eq!(reply.json()["error"], "invalid_request");
    }

    // A bound URI with a bad PKCE challenge or response type: an error redirect.
    for (url, error) in [
        (
            format!(
                "/authorize?response_type=code&client_id={client_id}&redirect_uri={}",
                urlencoding::encode(CB)
            ),
            "invalid_request",
        ),
        (
            authorize_url(&client_id, CB, "").replace("S256", "plain"),
            "invalid_request",
        ),
        (
            authorize_url(&client_id, CB, "").replace(CHALLENGE, "short"),
            "invalid_request",
        ),
        (
            authorize_url(&client_id, CB, "").replace("response_type=code", "response_type=token"),
            "unsupported_response_type",
        ),
    ] {
        let reply = get(&app, &url).await;
        assert_eq!(reply.status, StatusCode::FOUND, "{url}");
        assert!(
            reply.location().contains(&format!("error={error}")),
            "{url}"
        );
        assert!(!reply.location().contains("code="));
    }
}

#[tokio::test]
async fn token_rejects_mismatches_and_unknown_grants() {
    let app = app();
    let (client_id, csrf) = consent(&app).await;
    let code = code_of(login(&app, &csrf, PASS).await.location());

    let wrong_client = exchange(&app, &code, "someone-else").await;
    assert_eq!(wrong_client.status, StatusCode::BAD_REQUEST);
    assert_eq!(wrong_client.json()["error"], "invalid_grant");
    assert_no_secret(&wrong_client, &[&code, VERIFIER]);

    for fields in [
        vec![("grant_type", "client_credentials")],
        vec![("grant_type", "refresh_token")],
        vec![("grant_type", "authorization_code"), ("code", "x")],
    ] {
        let reply = post_form(&app, "/token", &fields).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    }
    let unknown = post_form(
        &app,
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", "not-real"),
            ("client_id", &client_id),
        ],
    )
    .await;
    assert_eq!(unknown.json()["error"], "invalid_grant");
}

#[tokio::test]
async fn a_store_outage_is_a_503_not_an_invalid_grant() {
    let db = crate::db::Db::memory();
    let state = AuthState {
        creds: auth::Credentials {
            user: "admin".into(),
            pass: Password::new(PASS),
        },
        store: Arc::new(Store::new(db.clone())),
        public_url: None,
    };
    let app = router(state);
    db.call(|c| c.execute_batch("DROP TABLE refresh_tokens"))
        .await
        .unwrap();
    let reply = post_form(
        &app,
        "/token",
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", "anything"),
        ],
    )
    .await;
    assert_eq!(reply.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(reply.headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(reply.json()["error"], "server_error");
}

#[tokio::test]
async fn register_echoes_and_binds_and_enforces_its_caps() {
    let app = app();
    let reply = post_json(&app, "/register", json!({ "redirect_uris": [CB] })).await;
    assert_eq!(reply.status, StatusCode::CREATED);
    assert_eq!(reply.json()["redirect_uris"][0], CB);

    let six: Vec<String> = (0..6).map(|i| format!("https://app/cb{i}")).collect();
    let too_many = post_json(&app, "/register", json!({ "redirect_uris": six })).await;
    assert_eq!(too_many.status, StatusCode::BAD_REQUEST);
    assert_eq!(too_many.json()["error"], "invalid_client_metadata");

    let long = format!("https://app/{}", "a".repeat(MAX_REDIRECT_URI_LEN));
    let too_long = post_json(&app, "/register", json!({ "redirect_uris": [long] })).await;
    assert_eq!(too_long.json()["error"], "invalid_redirect_uri");

    for body in [
        json!({}),
        json!({ "redirect_uris": [] }),
        json!({ "redirect_uris": CB }),
        json!({ "redirect_uris": [42] }),
        json!({ "redirect_uris": ["http://evil.com/cb"] }),
    ] {
        let reply = post_json(&app, "/register", body).await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.json()["error"], "invalid_redirect_uri");
    }
}

#[tokio::test]
async fn an_oversized_body_is_refused() {
    let app = app();
    let huge = format!("https://app/{}", "a".repeat(OAUTH_LIMIT_PROBE));
    let reply = post_json(&app, "/register", json!({ "redirect_uris": [huge] })).await;
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
}

/// Comfortably past the 64 KiB OAuth body limit.
const OAUTH_LIMIT_PROBE: usize = 70 * 1024;

#[test]
fn redirect_rule_accepts_https_and_loopback_only() {
    for ok in [
        "https://example.com/cb",
        "https://example.com?next=/x",
        "https://example.com/cb?u=a@b",
        "http://localhost/cb",
        "http://localhost:8080/cb",
        "http://127.0.0.1/cb",
        "http://[::1]:9000/cb",
    ] {
        assert!(is_allowed_redirect(ok), "{ok}");
    }
    for bad in [
        "http://evil.com/cb",
        "http://localhost.evil.com/cb",
        "http://localhost@evil.com/cb",
        "http://localhost:1@evil.example/cb",
        "http://127.0.0.1:80@evil.com/",
        "https://x/cb#f",
        "http://localhost\\@evil.com/cb",
        "ftp://localhost/cb",
        "not a url",
        "https://",
        "https://?evil",
        "https:// /cb",
    ] {
        assert!(!is_allowed_redirect(bad), "{bad}");
    }
}

#[test]
fn consent_form_debug_hides_the_password() {
    let form = ConsentForm {
        csrf_token: "c".into(),
        username: "u".into(),
        password: PASS.into(),
    };
    assert!(!format!("{form:?}").contains(PASS));
}

#[tokio::test(start_paused = true)]
async fn auth_log_events_never_carry_a_secret() {
    let captured = crate::log_capture_tests::capture();

    let app = app();
    let (client_id, csrf) = consent(&app).await;
    let denied = login(&app, &csrf, "wrong-guess").await;
    let approved = login(&app, &denied.csrf(), PASS).await;
    let code = code_of(approved.location());
    let tokens = exchange(&app, &code, &client_id).await.json();
    let refresh = tokens["refresh_token"].as_str().unwrap().to_string();
    let rotate = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh.as_str()),
    ];
    assert_eq!(
        post_form(&app, "/token", &rotate).await.status,
        StatusCode::OK
    );
    let reused = post_form(&app, "/token", &rotate).await;
    assert_eq!(reused.status, StatusCode::BAD_REQUEST, "reuse");

    let logs = captured.text();
    assert!(logs.contains("login_failed"), "{logs}");
    assert!(logs.contains("login_succeeded"), "{logs}");
    assert!(logs.contains("refresh_reuse"), "{logs}");
    let access = tokens["access_token"].as_str().unwrap();
    for secret in [
        PASS,
        "wrong-guess",
        &code,
        access,
        &refresh,
        VERIFIER,
        &csrf,
    ] {
        assert!(
            !logs.contains(secret),
            "a log line carries a secret: {logs}"
        );
    }
}
