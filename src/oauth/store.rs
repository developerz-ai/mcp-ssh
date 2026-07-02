//! OAuth state. Authorization codes are short-lived (10 min) and stay in-memory —
//! losing them on a restart just costs an in-flight login a retry. Access tokens
//! (a day) and refresh tokens (a year, rotated on use) live in SQLite so logins
//! survive restarts. Tokens are opaque (not JWT): this server is the only
//! validator — no signing keys, no JWKS. Many tokens coexist, so multiple clients
//! authenticate concurrently.
use std::{collections::HashMap, time::Duration};

use base64::Engine;
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, time::Instant};

const CODE_TTL: Duration = Duration::from_secs(600);
/// Access-token lifetime. Kept short (a day) per the MCP authorization guidance —
/// a leaked bearer is root here, so it shouldn't outlive a day. The refresh token
/// (below) renews it silently, so a short access TTL costs the user nothing.
pub const TOKEN_TTL: Duration = Duration::from_secs(24 * 3600);
/// Refresh-token lifetime. Much longer than the access token (a year) and rotated
/// on every use, so a client used at least this often never has to redo the
/// browser flow.
pub const REFRESH_TTL: Duration = Duration::from_secs(365 * 24 * 3600);

struct CodeEntry {
    challenge: String,
    redirect_uri: String,
    expires: Instant,
}

/// An access token plus the refresh token that renews it. Each client gets its own
/// pair; SQLite holds many at once, so several clients (desktop, mobile, CLI) stay
/// authenticated independently and concurrently.
pub struct Tokens {
    pub access: String,
    pub refresh: String,
}

pub struct Store {
    /// Ephemeral authorization codes — in-memory by design (10-min TTL).
    codes: Mutex<HashMap<String, CodeEntry>>,
    /// Durable token store: access + refresh tokens survive restarts.
    db: crate::db::Db,
}

impl Store {
    pub fn new(db: crate::db::Db) -> Self {
        Self {
            codes: Mutex::new(HashMap::new()),
            db,
        }
    }

    /// Issue an authorization code bound to the PKCE challenge + redirect_uri.
    pub async fn new_code(&self, challenge: String, redirect_uri: String) -> String {
        let code = random_token();
        let now = Instant::now();
        let mut codes = self.codes.lock().await;
        // Sweep expired entries here — codes abandoned without a redeem (any
        // failed/dropped login) were otherwise never removed and the map grew
        // without bound for the process lifetime.
        codes.retain(|_, entry| entry.expires > now);
        codes.insert(
            code.clone(),
            CodeEntry {
                challenge,
                redirect_uri,
                expires: now + CODE_TTL,
            },
        );
        code
    }

    /// Exchange a code (with its PKCE verifier) for an access + refresh token pair.
    pub async fn redeem(
        &self,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Result<Tokens, &'static str> {
        let entry = self
            .codes
            .lock()
            .await
            .remove(code)
            .ok_or("invalid_grant")?;
        if entry.expires < Instant::now() {
            return Err("invalid_grant");
        }
        if entry.redirect_uri != redirect_uri {
            return Err("invalid_grant");
        }
        if !verify_pkce(&entry.challenge, verifier) {
            return Err("invalid_grant");
        }
        self.issue().await
    }

    /// Exchange a refresh token for a fresh pair. The old refresh token is consumed
    /// (rotation, RFC 6749 §10.4 best practice): replaying it fails, and each use
    /// resets the `REFRESH_TTL` window, so an actively-used client never re-auths.
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens, &'static str> {
        let token = refresh_token.to_owned();
        // Delete + expiry-check in one statement so a token can't be redeemed twice
        // by concurrent requests. `RETURNING` hands back the row only if it existed;
        // an absent token yields no row → `OptionalExtension::optional` gives `None`.
        let expiry: Option<i64> = self
            .db
            .call(move |conn| {
                use rusqlite::OptionalExtension;
                conn.query_row(
                    "DELETE FROM refresh_tokens WHERE token = ?1 RETURNING expires_unix",
                    [token],
                    |row| row.get(0),
                )
                .optional()
            })
            .await
            .map_err(|_| "server_error")?;
        // Unknown token, or it was already past its TTL (consuming an expired token
        // on the way out is fine — it's dead either way). Both → invalid_grant.
        match expiry {
            Some(exp) if exp > crate::db::now_unix() => self.issue().await,
            _ => Err("invalid_grant"),
        }
    }

    /// Mint and store a new access + refresh pair. Every call is independent, so
    /// concurrent clients accumulate distinct pairs that don't clobber each other.
    async fn issue(&self) -> Result<Tokens, &'static str> {
        let access = random_token();
        let refresh = random_token();
        let access_exp = crate::db::now_unix() + TOKEN_TTL.as_secs() as i64;
        let refresh_exp = crate::db::now_unix() + REFRESH_TTL.as_secs() as i64;
        let (access_db, refresh_db) = (access.clone(), refresh.clone());
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO access_tokens (token, expires_unix) VALUES (?1, ?2)",
                    (access_db, access_exp),
                )?;
                conn.execute(
                    "INSERT INTO refresh_tokens (token, expires_unix) VALUES (?1, ?2)",
                    (refresh_db, refresh_exp),
                )?;
                Ok(())
            })
            .await
            .map_err(|_| "server_error")?;
        Ok(Tokens { access, refresh })
    }

    /// Insert a token directly — test seam only, bypasses the PKCE flow.
    #[cfg(test)]
    pub(crate) async fn insert_token(&self, token: &str) {
        let token = token.to_owned();
        let exp = crate::db::now_unix() + TOKEN_TTL.as_secs() as i64;
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO access_tokens (token, expires_unix) VALUES (?1, ?2)",
                    (token, exp),
                )
            })
            .await
            .expect("insert_token");
    }

    /// True if the bearer token is known and unexpired. An expired token is evicted
    /// on the way out so the table doesn't accumulate dead rows.
    pub async fn validate(&self, token: &str) -> bool {
        let token = token.to_owned();
        let result: rusqlite::Result<bool> = self
            .db
            .call(move |conn| {
                use rusqlite::OptionalExtension;
                let expiry: Option<i64> = conn
                    .query_row(
                        "SELECT expires_unix FROM access_tokens WHERE token = ?1",
                        [&token],
                        |row| row.get(0),
                    )
                    .optional()?;
                match expiry {
                    Some(exp) if exp > crate::db::now_unix() => Ok(true),
                    Some(_) => {
                        // Known but expired: drop it and report invalid.
                        conn.execute("DELETE FROM access_tokens WHERE token = ?1", [&token])?;
                        Ok(false)
                    }
                    None => Ok(false),
                }
            })
            .await;
        result.unwrap_or(false)
    }
}

pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256: BASE64URL(SHA256(verifier)) == challenge.
pub fn verify_pkce(challenge: &str, verifier: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest) == challenge
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Db, now_unix};

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    fn store() -> Store {
        Store::new(Db::memory())
    }

    async fn mint(store: &Store) -> Tokens {
        let code = store.new_code(CHALLENGE.into(), "http://cb".into()).await;
        store.redeem(&code, VERIFIER, "http://cb").await.unwrap()
    }

    #[tokio::test]
    async fn expired_codes_are_swept_on_the_next_issue() {
        let store = store();
        // Plant an already-expired entry directly; a lazily-checked map would
        // keep it forever if it's never redeemed.
        store.codes.lock().await.insert(
            "stale".into(),
            CodeEntry {
                challenge: CHALLENGE.into(),
                redirect_uri: "http://cb".into(),
                expires: Instant::now() - Duration::from_secs(1),
            },
        );

        let fresh = store.new_code(CHALLENGE.into(), "http://cb".into()).await;

        let codes = store.codes.lock().await;
        assert!(
            !codes.contains_key("stale"),
            "expired code must be swept, not retained until redeem"
        );
        assert!(codes.contains_key(&fresh), "fresh code must be kept");
    }

    #[test]
    fn pkce_roundtrip() {
        // verifier -> S256 challenge, must verify; a wrong verifier must not.
        assert!(verify_pkce(CHALLENGE, VERIFIER));
        assert!(!verify_pkce(CHALLENGE, "wrong-verifier"));
    }

    #[tokio::test]
    async fn redeem_requires_matching_pkce_and_redirect() {
        let store = store();
        let code = store.new_code(CHALLENGE.into(), "http://cb".into()).await;

        assert!(store.redeem(&code, VERIFIER, "http://wrong").await.is_err());
        // code was consumed on the failed attempt above, so issue a fresh one
        let code = store.new_code(CHALLENGE.into(), "http://cb".into()).await;
        let tokens = store.redeem(&code, VERIFIER, "http://cb").await.unwrap();
        assert!(store.validate(&tokens.access).await);
        assert!(!store.validate("nope").await);
    }

    #[tokio::test]
    async fn expired_access_token_is_rejected_and_evicted() {
        let store = store();
        // Seed an already-expired access token directly (wall clock, so we can't use
        // tokio's mock timer — just write a past `expires_unix`).
        let token = random_token();
        let (t, exp) = (token.clone(), now_unix() - 1);
        store
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO access_tokens (token, expires_unix) VALUES (?1, ?2)",
                    (t, exp),
                )
            })
            .await
            .unwrap();

        // First call rejects and evicts the row.
        assert!(!store.validate(&token).await);
        // Second call confirms the row is gone, not merely considered invalid.
        let t = token.clone();
        let still_present: bool = store
            .db
            .call(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM access_tokens WHERE token = ?1",
                    [t],
                    |row| row.get::<_, i64>(0),
                )
                .map(|n| n > 0)
            })
            .await
            .unwrap();
        assert!(!still_present, "expired token must be evicted");
    }

    #[tokio::test]
    async fn refresh_rotates_and_issues_a_working_access_token() {
        let store = store();
        let first = mint(&store).await;

        // Refreshing yields a new, valid access token...
        let second = store.refresh(&first.refresh).await.unwrap();
        assert!(store.validate(&second.access).await);
        assert_ne!(
            first.access, second.access,
            "a fresh access token is issued"
        );
        assert_ne!(first.refresh, second.refresh, "refresh token rotates");

        // ...and the consumed refresh token can't be replayed (rotation).
        assert!(
            store.refresh(&first.refresh).await.is_err(),
            "old refresh token must be single-use"
        );
        // The new one still works.
        assert!(store.refresh(&second.refresh).await.is_ok());
    }

    #[tokio::test]
    async fn expired_refresh_token_is_rejected() {
        let store = store();
        // Seed an expired refresh token directly (past `expires_unix`).
        let token = random_token();
        let (t, exp) = (token.clone(), now_unix() - 1);
        store
            .db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO refresh_tokens (token, expires_unix) VALUES (?1, ?2)",
                    (t, exp),
                )
            })
            .await
            .unwrap();
        assert!(
            store.refresh(&token).await.is_err(),
            "a refresh token past REFRESH_TTL must be rejected"
        );
    }

    #[tokio::test]
    async fn multiple_clients_hold_independent_tokens() {
        // Several clients authenticate; every access token stays valid at once and
        // refreshing one never affects the others.
        let store = store();
        let a = mint(&store).await;
        let b = mint(&store).await;
        let c = mint(&store).await;
        for t in [&a, &b, &c] {
            assert!(store.validate(&t.access).await);
        }
        assert!(a.access != b.access && b.access != c.access);
        // Refreshing b is independent of a and c.
        assert!(store.refresh(&b.refresh).await.is_ok());
        assert!(
            store.validate(&a.access).await,
            "a unaffected by b's refresh"
        );
        assert!(
            store.validate(&c.access).await,
            "c unaffected by b's refresh"
        );
    }
}
