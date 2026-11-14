//! `/register`: minimal Dynamic Client Registration (RFC 7591).
use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

use super::{is_allowed_redirect, json_error};
use crate::agent::ClientName;
use crate::oauth::AuthState;
use crate::oauth::store::RegisterError;

/// Most redirect URIs one client may register.
const MAX_REDIRECT_URIS: usize = 5;

/// `/register` — minimal Dynamic Client Registration (RFC 7591). Clients are
/// public (PKCE, no secret), so what is persisted is the `client_id` →
/// `redirect_uri` binding that `/authorize` then enforces, plus the optional
/// `client_name` other agents see it by — cleaned by [`ClientName::parse`],
/// never echoed raw. Bounded: a few short URIs per client, and a cap on clients
/// overall.
pub async fn register(
    State(st): State<AuthState>,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    let name = body
        .as_ref()
        .and_then(|Json(b)| b.get("client_name")?.as_str())
        .and_then(ClientName::parse);
    let redirect_uris = match registered_redirect_uris(body) {
        Ok(uris) => uris,
        Err(error) => return json_error(StatusCode::BAD_REQUEST, error),
    };
    let client_id = match st.store.register_client(&redirect_uris, name).await {
        Ok(id) => id,
        Err(RegisterError::Full | RegisterError::Unavailable) => {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "temporarily_unavailable");
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

/// The `redirect_uris` a DCR body is asking to bind, or the RFC 7591 §3.2.2 error.
/// Rejects a missing/non-array/non-string value, an empty list (a client bound to
/// no URI could never authorize), more than [`MAX_REDIRECT_URIS`], and any URI
/// failing the same rule `/authorize` applies.
fn registered_redirect_uris(
    body: Option<Json<serde_json::Value>>,
) -> Result<Vec<String>, &'static str> {
    const BAD_URI: &str = "invalid_redirect_uri";
    let Json(body) = body.ok_or(BAD_URI)?;
    let uris: Vec<String> = body
        .get("redirect_uris")
        .and_then(|v| v.as_array())
        .ok_or(BAD_URI)?
        .iter()
        .map(|u| u.as_str().map(str::to_owned))
        .collect::<Option<_>>()
        .ok_or(BAD_URI)?;
    if uris.len() > MAX_REDIRECT_URIS {
        return Err("invalid_client_metadata");
    }
    if uris.is_empty() || !uris.iter().all(|u| is_allowed_redirect(u)) {
        return Err(BAD_URI);
    }
    Ok(uris)
}
