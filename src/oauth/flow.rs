//! Authorization Code + PKCE flow: `/authorize` (GET renders the consent form,
//! POST logs in and issues the code), `/token` (`token`), `/register` (`register`).
//!
//! A code is only ever issued by the POST, after the operator typed the
//! credentials into the form: nothing ambient (cached Basic auth, a cookie) can
//! make a page load mint one, so a link alone can't steal a login.
use std::time::Duration;

use axum::{
    Json,
    extract::{Form, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use super::store::ConsentRequest;
use super::{AuthState, consent, secret};
use crate::auth;

mod register;
mod token;

pub use register::register;
pub use token::token;

/// The longest redirect URI accepted.
const MAX_REDIRECT_URI_LEN: usize = 2048;
/// Flat delay on a failed login: slows a password guesser without per-client
/// state. Real rate limiting belongs to the reverse proxy.
const LOGIN_FAILURE_DELAY: Duration = Duration::from_millis(500);

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

/// `GET /authorize` — validate the request and render the consent form. Never
/// issues a code.
pub async fn authorize(State(st): State<AuthState>, Query(p): Query<AuthorizeParams>) -> Response {
    // Open-redirect guard (OAuth 2.1 §4.1.1 / MCP spec MUST): validate the URI
    // *before* any redirect to it. Reject inline — never bounce the user-agent to
    // an unvalidated `redirect_uri`.
    if !is_allowed_redirect(&p.redirect_uri) {
        return json_error(StatusCode::BAD_REQUEST, "invalid_request");
    }
    // Client binding (OAuth 2.1 §4.1.2.1): the code may only go to a URI this
    // client registered. `/register` is open, so an attacker can still register
    // their own callback — the consent form, showing where the code will go and
    // demanding the password, is the defence there.
    match st
        .store
        .client_allows_redirect(&p.client_id, &p.redirect_uri)
        .await
    {
        Ok(true) => {}
        Ok(false) => return json_error(StatusCode::BAD_REQUEST, "invalid_request"),
        Err(_) => return json_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    }
    if p.response_type != "code" {
        return redirect_error(
            &p.redirect_uri,
            "unsupported_response_type",
            p.state.as_deref(),
        );
    }
    // PKCE S256 is mandatory for MCP clients.
    let Some(challenge) = p.code_challenge.filter(|c| {
        p.code_challenge_method.as_deref() == Some("S256") && secret::is_valid_challenge(c)
    }) else {
        return redirect_error(&p.redirect_uri, "invalid_request", p.state.as_deref());
    };

    let request = ConsentRequest {
        client_id: p.client_id,
        redirect_uri: p.redirect_uri,
        challenge,
        state: p.state,
    };
    render_consent(&st, request, StatusCode::OK, None).await
}

/// The consent form's fields. Hand-written `Debug`: it carries the password.
#[derive(Deserialize)]
pub struct ConsentForm {
    csrf_token: String,
    username: String,
    password: String,
}

impl std::fmt::Debug for ConsentForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConsentForm(<redacted>)")
    }
}

/// `POST /authorize` — the consent form. The `csrf_token` names the request the
/// GET validated (held server-side, single use, 10 minutes), so nothing in the
/// posted form can change where the code goes. Wrong credentials re-render the
/// form with a fresh token; right ones redirect to the client with the code.
pub async fn approve(
    State(st): State<AuthState>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    let Some(request) = st.store.take_consent(&form.csrf_token).await else {
        return consent::refused(
            StatusCode::FORBIDDEN,
            "This login request expired or was already used. Start again from your MCP client.",
        );
    };
    if !auth::check_login(&form.username, &form.password, &st.creds) {
        tracing::warn!(
            event = "login_failed",
            forwarded_for = ?forwarded_for(&headers),
            "failed login on /authorize"
        );
        tokio::time::sleep(LOGIN_FAILURE_DELAY).await;
        return render_consent(
            &st,
            request,
            StatusCode::UNAUTHORIZED,
            Some("Wrong username or password."),
        )
        .await;
    }
    let (redirect_uri, state) = (request.redirect_uri.clone(), request.state.clone());
    let Some(code) = st.store.new_code(request).await else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
    };
    tracing::info!(event = "login_succeeded", "authorization code issued");
    let mut location = format!(
        "{redirect_uri}{}code={}",
        query_separator(&redirect_uri),
        urlencoding::encode(&code)
    );
    if let Some(s) = state {
        location.push_str(&format!("&state={}", urlencoding::encode(&s)));
    }
    (
        StatusCode::FOUND,
        [
            (header::LOCATION, location),
            (header::CACHE_CONTROL, "no-store".into()),
        ],
    )
        .into_response()
}

/// Park `request` and render its form.
async fn render_consent(
    st: &AuthState,
    request: ConsentRequest,
    status: StatusCode,
    error: Option<&str>,
) -> Response {
    let destination = request.redirect_uri.clone();
    match st.store.begin_consent(request).await {
        Some(nonce) => consent::page(status, &nonce, &destination, error),
        None => json_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable"),
    }
}

/// The client address a proxy reported, for the failed-login journal line only:
/// the first `X-Forwarded-For` hop, capped, and logged with `Debug` so control
/// characters are escaped.
fn forwarded_for(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = value.split(',').next()?.trim();
    Some(first.chars().take(64).collect())
}

fn json_error(status: StatusCode, error: &str) -> Response {
    (status, Json(json!({ "error": error }))).into_response()
}

fn query_separator(uri: &str) -> char {
    if uri.contains('?') { '&' } else { '?' }
}

fn redirect_error(redirect_uri: &str, error: &str, state: Option<&str>) -> Response {
    let mut location = format!(
        "{redirect_uri}{}error={error}",
        query_separator(redirect_uri)
    );
    if let Some(s) = state {
        location.push_str(&format!("&state={}", urlencoding::encode(s)));
    }
    (StatusCode::FOUND, [(header::LOCATION, location)]).into_response()
}

/// OAuth 2.1 redirect-URI rule: `https` to any host, or plain `http` only to a
/// loopback host (`localhost`, `127.0.0.1`, `[::1]`). Parsed as a URI, never
/// prefix-matched; anything with userinfo, a backslash or a fragment is refused —
/// those are how a URI says one host to a naive check and another to a browser.
fn is_allowed_redirect(raw: &str) -> bool {
    if raw.len() > MAX_REDIRECT_URI_LEN || raw.contains(['#', '\\']) {
        return false;
    }
    let Ok(uri) = raw.parse::<axum::http::Uri>() else {
        return false;
    };
    let Some(authority) = uri.authority() else {
        return false;
    };
    if authority.as_str().contains('@') {
        return false;
    }
    let host = authority.host();
    match uri.scheme_str() {
        Some("https") => !host.trim().is_empty(),
        Some("http") => matches!(host, "localhost" | "127.0.0.1" | "[::1]"),
        _ => false,
    }
}

#[cfg(test)]
mod tests;
