//! Auth for `/mcp`: accept either HTTP Basic (simple clients, curl) or a bearer
//! token issued by the embedded OAuth server (Claude and other GUI clients).
//! On failure, return a 401 that points clients at the OAuth metadata so they
//! can start the flow.
use axum::{
    body::Body,
    extract::{Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{AUTHORIZATION, WWW_AUTHENTICATE},
    },
    middleware::Next,
    response::Response,
};
use base64::Engine;

use crate::oauth::{AuthState, base_url};

#[derive(Clone)]
pub struct Credentials {
    pub user: String,
    pub pass: String,
}

/// True if the request carries valid HTTP Basic credentials.
pub fn check_basic(headers: &HeaderMap, creds: &Credentials) -> bool {
    let expected = format!("{}:{}", creds.user, creds.pass);
    headers
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Basic "))
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map(|d| String::from_utf8_lossy(&d).into_owned())
        .as_deref()
        == Some(expected.as_str())
}

/// Extract a `Bearer <token>` value, if present.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Middleware guarding `/mcp`. Basic creds or a valid bearer token let it through.
pub async fn require_auth(State(st): State<AuthState>, req: Request, next: Next) -> Response {
    let headers = req.headers();

    if check_basic(headers, &st.creds) {
        return next.run(req).await;
    }
    if let Some(token) = bearer(headers) {
        if st.store.validate(token).await {
            return next.run(req).await;
        }
    }

    unauthorized(st.public_url.as_deref(), headers)
}

/// Build the 401 that points clients at the OAuth metadata so they can start the
/// flow. The challenge embeds a base URL that may be reflected from an attacker's
/// `Host`; an invalid `WWW-Authenticate` value must never panic the request path,
/// so fall back to a bare 401 when it won't build.
fn unauthorized(public_url: Option<&str>, headers: &HeaderMap) -> Response {
    let base = base_url(public_url, headers);
    let challenge =
        format!("Bearer resource_metadata_url=\"{base}/.well-known/oauth-protected-resource\"");
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;
    if let Ok(value) = HeaderValue::try_from(challenge) {
        response.headers_mut().insert(WWW_AUTHENTICATE, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds() -> Credentials {
        Credentials {
            user: "admin".into(),
            pass: "secret".into(),
        }
    }

    #[test]
    fn basic_auth_accepts_correct_and_rejects_wrong() {
        let mut headers = HeaderMap::new();
        let token = base64::engine::general_purpose::STANDARD.encode("admin:secret");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {token}")).unwrap(),
        );
        assert!(check_basic(&headers, &creds()));

        let bad = base64::engine::general_purpose::STANDARD.encode("admin:nope");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {bad}")).unwrap(),
        );
        assert!(!check_basic(&headers, &creds()));
    }

    #[test]
    fn malformed_or_huge_host_still_yields_401_never_panics() {
        // Host values a hostile proxy might forward: oversized, and embedding the
        // quote used to delimit the challenge. Either way: a clean 401, no panic.
        for host in ["h".repeat(64 * 1024), "evil\"host:1\"".into()] {
            let mut headers = HeaderMap::new();
            headers.insert("host", HeaderValue::from_str(&host).unwrap());
            assert_eq!(
                unauthorized(None, &headers).status(),
                StatusCode::UNAUTHORIZED
            );
        }
    }

    #[test]
    fn invalid_challenge_value_falls_back_to_bare_401() {
        // A configured public_url is used verbatim; a newline makes the
        // WWW-Authenticate value invalid — omit the header, still 401, no panic.
        let res = unauthorized(Some("http://example.com\n"), &HeaderMap::new());
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert!(!res.headers().contains_key(WWW_AUTHENTICATE));
    }
}
