//! Auth for `/mcp`: bearer-only. A valid access token issued by the embedded
//! OAuth server (Claude and other MCP clients) is required. The operator's
//! username/password is accepted only by the `/authorize` consent form (see
//! `check_login`), never here. On failure, return a 401 whose challenge points
//! clients at the OAuth metadata so they can start the flow.
use axum::{
    body::Body,
    extract::{Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{AUTHORIZATION, RETRY_AFTER, WWW_AUTHENTICATE},
    },
    middleware::Next,
    response::Response,
};

use crate::config::Password;
use crate::oauth::{AuthState, base_url, hash_token};

mod revalidate;

pub use revalidate::STREAM_REVALIDATE;

#[derive(Debug, Clone)]
pub struct Credentials {
    pub user: String,
    pub pass: Password,
}

/// True if `user`/`pass` are the operator's credentials. Both halves are always
/// compared, so the timing doesn't reveal which one was wrong.
pub fn check_login(user: &str, pass: &str, creds: &Credentials) -> bool {
    let user_ok = constant_time_eq(user.as_bytes(), creds.user.as_bytes());
    let pass_ok = constant_time_eq(pass.as_bytes(), creds.pass.expose().as_bytes());
    user_ok & pass_ok
}

/// Compare a candidate secret against the expected value without leaking how much
/// of it matched. This guards the password an attacker can iterate against
/// (`/authorize` mints root-equivalent bearer tokens), so a plain `==` — which
/// short-circuits on length and on the first differing byte — is a timing oracle.
/// Comparing SHA-256 digests makes the timing independent of where the inputs
/// differ, and the attacker controls neither digest's bytes.
fn constant_time_eq(candidate: &[u8], expected: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    Sha256::digest(candidate) == Sha256::digest(expected)
}

/// Extract a `Bearer <token>` value, if present.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Middleware guarding `/mcp`. Only a valid OAuth bearer token lets a request
/// through, carrying the [`Agent`](crate::agent::Agent) it was issued to as a request extension (the
/// tools attribute jobs to it). A token store that can't answer is a 503 (retry),
/// not a 401 — a 401 would send every client back through the login flow during
/// a DB blip. A streaming response keeps checking its token for as long as it
/// stays open.
pub async fn require_auth(State(st): State<AuthState>, mut req: Request, next: Next) -> Response {
    if let Some(token) = bearer(req.headers()) {
        let token_hash = hash_token(token);
        match st.store.authenticate(&token_hash).await {
            Ok(Some(agent)) => {
                req.extensions_mut().insert(agent);
                let response = next.run(req).await;
                return revalidate::end_when_revoked(
                    response,
                    st.store,
                    token_hash,
                    st.revalidate_every,
                );
            }
            Ok(None) => {}
            Err(_) => return unavailable(),
        }
    }
    unauthorized(st.public_url.as_deref(), req.headers())
}

fn unavailable() -> Response {
    let mut response = Response::new(Body::from("token store unavailable"));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
        .headers_mut()
        .insert(RETRY_AFTER, HeaderValue::from_static("5"));
    response
}

/// The 401 that points clients at the protected-resource metadata (RFC 9728 §5.1,
/// the MCP authorization spec): `Bearer resource_metadata="…"`. The base URL is
/// the configured `public_url`, else the request's `Host` — safe to reflect here
/// because the app's host guard has already rejected any Host not on the
/// allowlist. A value that won't build as a header never panics the request path:
/// the header is omitted and the 401 stands.
fn unauthorized(public_url: Option<&str>, headers: &HeaderMap) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;

    let base = base_url(public_url, headers);
    let challenge =
        format!("Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\"");
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
            pass: Password::new("secret"),
        }
    }

    fn challenge(res: &Response) -> &str {
        res.headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .expect("challenge present")
    }

    #[test]
    fn login_accepts_correct_and_rejects_wrong() {
        assert!(check_login("admin", "secret", &creds()));
        assert!(!check_login("admin", "nope", &creds()));
        assert!(!check_login("root", "secret", &creds()));
        assert!(!check_login("", "", &creds()));
    }

    #[test]
    fn credentials_debug_hides_the_password() {
        assert!(!format!("{:?}", creds()).contains("secret"));
    }

    #[test]
    fn challenge_uses_the_spec_parameter_and_the_configured_public_url() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("other.example.com"));
        let res = unauthorized(Some("https://mcp.example.com"), &headers);
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            challenge(&res),
            "Bearer resource_metadata=\"https://mcp.example.com/.well-known/oauth-protected-resource\""
        );
    }

    #[test]
    fn without_public_url_the_challenge_derives_from_the_allowed_host() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("127.0.0.1:1337"));
        let res = unauthorized(None, &headers);
        assert_eq!(
            challenge(&res),
            "Bearer resource_metadata=\"http://127.0.0.1:1337/.well-known/oauth-protected-resource\""
        );
    }

    #[test]
    fn a_host_that_cannot_form_a_header_yields_a_bare_401_never_a_panic() {
        // A quote would break out of the quoted parameter; it must not be reflected.
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("evil\"host"));
        let res = unauthorized(None, &headers);
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let shown = res
            .headers()
            .get(WWW_AUTHENTICATE)
            .map(|v| v.to_str().unwrap_or_default().to_string())
            .unwrap_or_default();
        assert!(!shown.contains("evil\""), "{shown}");
    }
}
