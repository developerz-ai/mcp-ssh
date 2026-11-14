//! Bulk revocation and the reaper's sweeps of expired tokens and stale clients.
//! Counts only — no token value is ever read.
use rusqlite::{Transaction, TransactionBehavior};

use super::{CLIENT_GRACE, secs};
use crate::db::Db;

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
