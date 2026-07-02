//! Auth for `/mcp`: bearer-only. A valid access token issued by the embedded
//! OAuth server (Claude and other MCP clients) is required. HTTP Basic is no
//! longer accepted here — it survives solely as the human login inside
//! `/authorize` (see `check_basic`). On failure, return a 401 that points
//! clients at the OAuth metadata so they can start the flow.
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
        .is_some_and(|d| constant_time_eq(&d, expected.as_bytes()))
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
/// through; HTTP Basic is rejected here (it remains the human login for
/// `/authorize`). Anything else gets the OAuth-pointing 401.
pub async fn require_auth(State(st): State<AuthState>, req: Request, next: Next) -> Response {
    let headers = req.headers();

    if let Some(token) = bearer(headers) {
        if st.store.validate(token).await {
            return next.run(req).await;
        }
    }

    unauthorized(st.public_url.as_deref(), headers)
}

/// Build the 401 that points clients at the OAuth metadata so they can start the
/// flow. The `resource_metadata_url` is advertised only when a trusted
/// `public_url` is configured: `/mcp` auth runs *before* the allowed-hosts gate,
/// so an unauthenticated request's `Host` is attacker-controlled and must never
/// be reflected into the challenge. With no `public_url`, return a bare 401. An
/// invalid `WWW-Authenticate` value must never panic the request path, so omit
/// the header when it won't build.
fn unauthorized(public_url: Option<&str>, headers: &HeaderMap) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::UNAUTHORIZED;

    let Some(public_url) = public_url else {
        return response;
    };

    let base = base_url(Some(public_url), headers);
    let challenge =
        format!("Bearer resource_metadata_url=\"{base}/.well-known/oauth-protected-resource\"");
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
        // quote used to delimit the challenge. With no configured public_url the
        // attacker-controlled Host must never reach the challenge: a clean 401
        // with no WWW-Authenticate header, no panic.
        for host in ["h".repeat(64 * 1024), "evil\"host:1\"".into()] {
            let mut headers = HeaderMap::new();
            headers.insert("host", HeaderValue::from_str(&host).unwrap());
            let res = unauthorized(None, &headers);
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
            assert!(!res.headers().contains_key(WWW_AUTHENTICATE));
        }
    }

    #[test]
    fn challenge_advertises_configured_public_url_not_host() {
        // A trusted public_url is the only source for the challenge; the
        // attacker-controlled Host is ignored entirely.
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("evil.example.com"));
        let res = unauthorized(Some("https://mcp.example.com"), &headers);
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let challenge = res
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .expect("challenge present when public_url is configured");
        assert!(
            challenge.contains("https://mcp.example.com/.well-known/oauth-protected-resource"),
            "challenge must use the configured public_url: {challenge}"
        );
        assert!(
            !challenge.contains("evil.example.com"),
            "challenge must never reflect the Host header: {challenge}"
        );
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
