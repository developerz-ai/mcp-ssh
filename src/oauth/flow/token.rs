//! `/token`: the `authorization_code` and `refresh_token` grants.
use axum::{
    Json,
    extract::{Form, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;

use crate::oauth::AuthState;
use crate::oauth::store::{self, GrantError};

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
    /// Public clients send it (RFC 6749 §4.1.3); when present it must match the
    /// client the code or refresh token was issued to.
    #[serde(default)]
    client_id: Option<String>,
}

/// `/token` — issue an access + refresh pair. Two grants: `authorization_code`
/// (code + PKCE verifier, first login) and `refresh_token` (silent renewal once
/// the access token expires, so the client doesn't re-run the browser flow).
/// Every response is `no-store`; a store failure is a 503, so a client keeps its
/// refresh token and retries instead of discarding a valid login.
pub async fn token(State(st): State<AuthState>, Form(p): Form<TokenParams>) -> Response {
    let client_id = p.client_id.as_deref();
    let issued = match p.grant_type.as_str() {
        "authorization_code" => {
            let (Some(code), Some(verifier), Some(redirect_uri)) =
                (&p.code, &p.code_verifier, &p.redirect_uri)
            else {
                return token_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            st.store
                .redeem(code, verifier, redirect_uri, client_id)
                .await
        }
        "refresh_token" => {
            let Some(refresh_token) = &p.refresh_token else {
                return token_error(StatusCode::BAD_REQUEST, "invalid_request");
            };
            st.store.refresh(refresh_token, client_id).await
        }
        _ => return token_error(StatusCode::BAD_REQUEST, "unsupported_grant_type"),
    };
    match issued {
        Ok(tokens) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({
                "access_token": tokens.access,
                "token_type": "Bearer",
                "expires_in": store::TOKEN_TTL.as_secs(),
                "refresh_token": tokens.refresh,
            })),
        )
            .into_response(),
        Err(e @ GrantError::InvalidGrant) => token_error(StatusCode::BAD_REQUEST, &e.to_string()),
        Err(e @ GrantError::ServerError) => {
            token_error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string())
        }
    }
}

fn token_error(status: StatusCode, error: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "error": error })),
    )
        .into_response()
}
