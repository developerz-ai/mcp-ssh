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

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::pending::Pending;
use super::secret::{hash_token, random_token, verify_pkce};
use crate::db::{Db, now_unix};

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
/// Registered clients held at once. `/register` is unauthenticated (it bootstraps
/// auth), so the table is bounded; the stale-client sweep frees room.
const MAX_CLIENTS: i64 = 1000;

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

/// Who a new pair is issued to: the client, and the login it continues.
struct Grant<'a> {
    client_id: &'a str,
    family_id: &'a str,
    family_expires: i64,
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

    /// Register a client: mint an id and bind it to the redirect URIs that may
    /// receive its auth codes. Duplicate URIs collapse to one row. The returned
    /// `client_id` is public data, but it is still never logged.
    pub async fn register_client(&self, redirect_uris: &[String]) -> Result<String, RegisterError> {
        let client_id = random_token();
        let id = client_id.clone(); // the closure must own its inputs ('static)
        let uris = redirect_uris.to_vec();
        let now = now_unix();
        let registered = self
            .db
            .call(move |conn| {
                // Immediate: the count and the insert see the same table.
                let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
                let clients: i64 =
                    tx.query_row("SELECT COUNT(DISTINCT client_id) FROM clients", [], |r| {
                        r.get(0)
                    })?;
                if clients >= MAX_CLIENTS {
                    return Ok(false);
                }
                for uri in &uris {
                    tx.execute(
                        "INSERT OR IGNORE INTO clients (client_id, redirect_uri, created_unix) \
                         VALUES (?1, ?2, ?3)",
                        (&id, uri, now),
                    )?;
                }
                tx.commit()?;
                Ok(true)
            })
            .await
            .map_err(|e| {
                unavailable("register_client", e);
                RegisterError::Unavailable
            })?;
        if registered {
            Ok(client_id)
        } else {
            tracing::warn!(event = "register_refused", "client table full");
            Err(RegisterError::Full)
        }
    }

    /// True only if `client_id` is registered and `redirect_uri` is *exactly* one
    /// of the URIs it registered.
    pub async fn client_allows_redirect(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<bool, StoreUnavailable> {
        let (client_id, redirect_uri) = (client_id.to_owned(), redirect_uri.to_owned());
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT 1 FROM clients WHERE client_id = ?1 AND redirect_uri = ?2",
                    (client_id, redirect_uri),
                    |_| Ok(()),
                )
                .optional()
                .map(|row| row.is_some())
            })
            .await
            .map_err(|e| unavailable("client_allows_redirect", e))
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

    /// True if the bearer token is known and unexpired. An expired token is evicted
    /// on the way out so the table doesn't accumulate dead rows.
    pub async fn validate(&self, token: &str) -> Result<bool, StoreUnavailable> {
        let hash = hash_token(token);
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

enum Rotation {
    Rotated,
    Invalid,
    Reused,
}

/// The body of [`Store::refresh`], on the blocking pool.
fn rotate(
    conn: &Connection,
    presented: &str,
    client_id: Option<&str>,
    pair: &PairHashes,
    now: i64,
) -> rusqlite::Result<Rotation> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let row: Option<(String, String, i64, i64)> = tx
        .query_row(
            "DELETE FROM refresh_tokens WHERE token_hash = ?1 \
             RETURNING client_id, family_id, expires_unix, family_expires_unix",
            [presented],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((owner, family_id, expires, family_expires)) = row else {
        let reused: Option<String> = tx
            .query_row(
                "SELECT family_id FROM refresh_tombstones WHERE token_hash = ?1",
                [presented],
                |r| r.get(0),
            )
            .optional()?;
        let Some(family_id) = reused else {
            return Ok(Rotation::Invalid);
        };
        revoke_family(&tx, &family_id)?;
        tx.commit()?;
        return Ok(Rotation::Reused);
    };
    if client_id.is_some_and(|id| id != owner) {
        // Rolled back on drop: a token presented by the wrong client isn't burned.
        return Ok(Rotation::Invalid);
    }
    if expires <= now || family_expires <= now {
        tx.commit()?; // dead either way; consuming it on the way out is fine
        return Ok(Rotation::Invalid);
    }
    tx.execute(
        "INSERT INTO refresh_tombstones (token_hash, family_id, expires_unix) VALUES (?1, ?2, ?3)",
        (presented, &family_id, family_expires),
    )?;
    let grant = Grant {
        client_id: &owner,
        family_id: &family_id,
        family_expires,
    };
    insert_pair(&tx, pair, &grant, now)?;
    tx.commit()?;
    Ok(Rotation::Rotated)
}

/// Hashes of a freshly minted pair, the only form that reaches the database.
struct PairHashes {
    access: String,
    refresh: String,
}

fn new_tokens() -> Tokens {
    Tokens {
        access: random_token(),
        refresh: random_token(),
    }
}

fn hashes(tokens: &Tokens) -> PairHashes {
    PairHashes {
        access: hash_token(&tokens.access),
        refresh: hash_token(&tokens.refresh),
    }
}

/// Both rows of a pair, inside the caller's transaction: never half-written.
fn insert_pair(
    tx: &Transaction,
    pair: &PairHashes,
    grant: &Grant,
    now: i64,
) -> rusqlite::Result<()> {
    let access_exp = now
        .saturating_add(secs(TOKEN_TTL))
        .min(grant.family_expires);
    let refresh_exp = now
        .saturating_add(secs(REFRESH_TTL))
        .min(grant.family_expires);
    tx.execute(
        "INSERT INTO access_tokens (token_hash, client_id, family_id, expires_unix) \
         VALUES (?1, ?2, ?3, ?4)",
        (&pair.access, grant.client_id, grant.family_id, access_exp),
    )?;
    tx.execute(
        "INSERT INTO refresh_tokens \
         (token_hash, client_id, family_id, expires_unix, family_expires_unix) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        (
            &pair.refresh,
            grant.client_id,
            grant.family_id,
            refresh_exp,
            grant.family_expires,
        ),
    )?;
    Ok(())
}

fn revoke_family(tx: &Transaction, family_id: &str) -> rusqlite::Result<()> {
    for table in ["access_tokens", "refresh_tokens", "refresh_tombstones"] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE family_id = ?1"),
            [family_id],
        )?;
    }
    Ok(())
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

/// End every login: all access and refresh tokens (and tombstones). Returns how
/// many tokens went away — a count only. Registered clients stay: they hold no
/// credential and can log in again. Used by `set-auth` and `sessions --revoke`.
pub async fn revoke_all(db: &Db) -> rusqlite::Result<usize> {
    db.call(|conn| {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let access = tx.execute("DELETE FROM access_tokens", [])?;
        let refresh = tx.execute("DELETE FROM refresh_tokens", [])?;
        tx.execute("DELETE FROM refresh_tombstones", [])?;
        tx.commit()?;
        Ok(access + refresh)
    })
    .await
}

/// Drop every access token whose expiry has passed. Returns how many rows went
/// away — a count only, never a token value. Run periodically by the reaper:
/// `validate` evicts a dead row only when that exact token is re-presented, and
/// a rotated pair's superseded access token never is.
pub async fn sweep_expired_access(db: &Db, now: i64) -> usize {
    sweep(
        db,
        "access_tokens",
        "DELETE FROM access_tokens WHERE expires_unix <= ?1",
        now,
    )
    .await
}

/// Drop every refresh token and rotation tombstone whose expiry has passed.
/// Returns the deleted count.
pub async fn sweep_expired_refresh(db: &Db, now: i64) -> usize {
    sweep(
        db,
        "refresh_tokens",
        "DELETE FROM refresh_tokens WHERE expires_unix <= ?1",
        now,
    )
    .await
        + sweep(
            db,
            "refresh_tombstones",
            "DELETE FROM refresh_tombstones WHERE expires_unix <= ?1",
            now,
        )
        .await
}

/// Drop registered clients older than [`CLIENT_GRACE`] that hold no live token —
/// abandoned or drive-by registrations. Returns the number of registration rows
/// (client, redirect URI) removed.
pub async fn sweep_stale_clients(db: &Db, now: i64) -> usize {
    let cutoff = now.saturating_sub(secs(CLIENT_GRACE));
    let sql = "DELETE FROM clients WHERE created_unix <= ?1 AND client_id NOT IN ( \
                   SELECT client_id FROM access_tokens WHERE expires_unix > ?2 \
                   UNION SELECT client_id FROM refresh_tokens WHERE expires_unix > ?2)";
    match db.call(move |conn| conn.execute(sql, (cutoff, now))).await {
        Ok(deleted) => {
            tracing::debug!(deleted, "swept stale clients");
            deleted
        }
        Err(error) => {
            tracing::warn!(%error, "sweeping stale clients failed");
            0
        }
    }
}

/// One bound bulk `DELETE`. Nothing is selected, so no token hash is ever read
/// into the process. A failed sweep reports zero and is logged: the reaper simply
/// retries on the next pass.
async fn sweep(db: &Db, table: &'static str, sql: &'static str, now: i64) -> usize {
    match db.call(move |conn| conn.execute(sql, [now])).await {
        Ok(deleted) => {
            tracing::debug!(table, deleted, "swept expired rows");
            deleted
        }
        Err(error) => {
            tracing::warn!(%error, table, "sweeping expired rows failed");
            0
        }
    }
}

#[cfg(test)]
mod tests;
