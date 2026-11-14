//! OAuth state. Consent requests and authorization codes are short-lived (10 min)
//! and stay in memory (see [`Pending`]). Access tokens (a day) and refresh tokens
//! (30 days, rotated on use, within a 90-day login "family") live in SQLite so
//! logins survive restarts — stored only as SHA-256 hashes, each bound to the
//! client and family it was issued to. Tokens are opaque (not JWT): this server is
//! the only validator — no signing keys, no JWKS.
//!
//! Registered clients are durable too: `/register` binds a `client_id` to the
//! redirect URIs it may receive auth codes at, and `/authorize` enforces that
//! binding, so a restart must not silently invalidate every client's login.
//!
//! Every SQL statement for these tables lives here. A database failure is logged
//! here (the rusqlite error only — never a token) and surfaces as a distinct error,
//! so an outage is a 503 with a journal line, not an "invalid token".
use std::time::Duration;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use super::pending::Pending;
use super::secret::{hash_token, random_token, verify_pkce};
use crate::db::{Db, now_unix};

mod clients;
mod pair;
mod sweep;

use pair::{Grant, Rotation, hashes, insert_pair, new_tokens, rotate};
pub use sweep::{revoke_all, sweep_expired_access, sweep_expired_refresh, sweep_stale_clients};

const CODE_TTL: Duration = Duration::from_secs(600);
/// A consent page left open longer than this must be reloaded.
const CONSENT_TTL: Duration = Duration::from_secs(600);
/// Live consent requests / codes held at once. Both are minted by unauthenticated
/// requests, so memory is bounded; a legitimate login needs one of each.
const PENDING_CAP: usize = 1024;
/// Access-token lifetime. Kept short (a day) per the MCP authorization guidance —
/// a leaked bearer is root here, so it shouldn't outlive a day. The refresh token
/// renews it silently, so a short access TTL costs the user nothing.
pub const TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);
/// Refresh-token lifetime, reset on each rotation but never past the family cap.
pub const REFRESH_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
/// Absolute lifetime of one login: however often it is refreshed, a client logs in
/// again after this long.
pub const FAMILY_TTL: Duration = Duration::from_secs(90 * 24 * 3600);
/// A registered client with no live token is dropped once it is this old.
pub const CLIENT_GRACE: Duration = Duration::from_secs(7 * 24 * 3600);
/// The RFC 6749 §5.2 wire error codes the grant paths emit. `Display` renders the
/// exact wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GrantError {
    #[error("invalid_grant")]
    InvalidGrant,
    #[error("server_error")]
    ServerError,
}

/// The token store could not be reached. Already logged where it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("token store unavailable")]
pub struct StoreUnavailable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    #[error("too many registered clients")]
    Full,
    #[error("token store unavailable")]
    Unavailable,
}

/// A validated `/authorize` request waiting for the operator's login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentRequest {
    pub client_id: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub state: Option<String>,
}

/// An access token plus the refresh token that renews it.
pub struct Tokens {
    pub access: String,
    pub refresh: String,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens(<redacted>)")
    }
}

#[derive(Debug)]
pub struct Store {
    consents: Pending<ConsentRequest>,
    codes: Pending<ConsentRequest>,
    db: Db,
}

impl Store {
    pub fn new(db: Db) -> Self {
        Self {
            consents: Pending::new(CONSENT_TTL, PENDING_CAP),
            codes: Pending::new(CODE_TTL, PENDING_CAP),
            db,
        }
    }

    /// Park a validated request until the operator logs in; returns the single-use
    /// nonce the consent form posts back. `None` when too many are pending.
    pub async fn begin_consent(&self, request: ConsentRequest) -> Option<String> {
        self.consents.insert(request).await
    }

    /// The request a consent nonce was issued for — once, and only within its TTL.
    pub async fn take_consent(&self, nonce: &str) -> Option<ConsentRequest> {
        self.consents.take(nonce).await
    }

    /// Issue an authorization code for an approved request. `None` when too many
    /// codes are outstanding.
    pub async fn new_code(&self, approved: ConsentRequest) -> Option<String> {
        self.codes.insert(approved).await
    }

    /// Exchange a code (with its PKCE verifier) for a new login's token pair. The
    /// code is bound to its redirect URI and client (RFC 6749 §4.1.3); a token
    /// request that names a different `client_id` is refused.
    pub async fn redeem(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
        client_id: Option<&str>,
    ) -> Result<Tokens, GrantError> {
        let grant = self
            .codes
            .take(code)
            .await
            .ok_or(GrantError::InvalidGrant)?;
        if grant.redirect_uri != redirect_uri
            || client_id.is_some_and(|id| id != grant.client_id)
            || !verify_pkce(&grant.challenge, verifier)
        {
            return Err(GrantError::InvalidGrant);
        }
        let tokens = new_tokens();
        let (pair, client) = (hashes(&tokens), grant.client_id);
        let now = now_unix();
        self.db
            .call(move |conn| {
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let family_id = random_token();
                let grant = Grant {
                    client_id: &client,
                    family_id: &family_id,
                    family_expires: now.saturating_add(secs(FAMILY_TTL)),
                };
                insert_pair(&tx, &pair, &grant, now)?;
                tx.commit()
            })
            .await
            .map_err(|e| {
                unavailable("redeem", e);
                GrantError::ServerError
            })?;
        Ok(tokens)
    }

    /// Exchange a refresh token for a fresh pair in the same login family, all in
    /// one transaction: the presented token is consumed and tombstoned, and the new
    /// pair inserted — or nothing changes. Presenting an already-rotated token is
    /// reuse (the token leaked): the whole family is revoked.
    pub async fn refresh(
        &self,
        refresh_token: &str,
        client_id: Option<&str>,
    ) -> Result<Tokens, GrantError> {
        let presented = hash_token(refresh_token);
        let client_id = client_id.map(str::to_owned);
        let tokens = new_tokens();
        let pair = hashes(&tokens);
        let now = now_unix();
        let outcome = self
            .db
            .call(move |conn| rotate(conn, &presented, client_id.as_deref(), &pair, now))
            .await
            .map_err(|e| {
                unavailable("refresh", e);
                GrantError::ServerError
            })?;
        match outcome {
            Rotation::Rotated => Ok(tokens),
            Rotation::Invalid => Err(GrantError::InvalidGrant),
            Rotation::Reused => {
                tracing::warn!(
                    event = "refresh_reuse",
                    "a rotated refresh token was presented again; its login was revoked"
                );
                Err(GrantError::InvalidGrant)
            }
        }
    }

    /// [`Store::validate_hash`] for a bearer in hand — test convenience.
    #[cfg(test)]
    pub(crate) async fn validate(&self, token: &str) -> Result<bool, StoreUnavailable> {
        self.validate_hash(&hash_token(token)).await
    }

    /// True if the bearer token with this hash is known and unexpired. An expired
    /// token is evicted on the way out so the table doesn't accumulate dead rows.
    /// Takes the hash so a caller that keeps checking (an open stream) never has
    /// to hold the bearer itself.
    pub async fn validate_hash(&self, token_hash: &str) -> Result<bool, StoreUnavailable> {
        let hash = token_hash.to_owned(); // moves into the blocking DB call
        let now = now_unix();
        self.db
            .call(move |conn| {
                let expiry: Option<i64> = conn
                    .query_row(
                        "SELECT expires_unix FROM access_tokens WHERE token_hash = ?1",
                        [&hash],
                        |row| row.get(0),
                    )
                    .optional()?;
                match expiry {
                    Some(exp) if exp > now => Ok(true),
                    Some(_) => {
                        conn.execute("DELETE FROM access_tokens WHERE token_hash = ?1", [&hash])?;
                        Ok(false)
                    }
                    None => Ok(false),
                }
            })
            .await
            .map_err(|e| unavailable("validate", e))
    }

    /// Insert a live access token directly — test seam only, bypasses the flow.
    #[cfg(test)]
    pub(crate) async fn insert_token(&self, token: &str) {
        let hash = hash_token(token);
        let exp = now_unix() + secs(TOKEN_TTL);
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO access_tokens (token_hash, client_id, family_id, expires_unix) \
                     VALUES (?1, 'test', 'test', ?2)",
                    (hash, exp),
                )
            })
            .await
            .expect("insert_token");
    }
}

/// Log a store failure at the boundary: the operation and the rusqlite error,
/// which never carries a bound parameter (so never a token hash or value).
fn unavailable(op: &'static str, error: rusqlite::Error) -> StoreUnavailable {
    tracing::warn!(op, %error, "token store unavailable");
    StoreUnavailable
}

fn secs(d: Duration) -> i64 {
    i64::try_from(d.as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
