//! Token pairs: minting, inserting a pair within a login family, and the
//! refresh rotation (with reuse detection revoking the whole family).
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::{REFRESH_TTL, TOKEN_TTL, Tokens, secs};
use crate::oauth::secret::{hash_token, random_token};

/// Who a new pair is issued to: the client, and the login it continues.
pub(super) struct Grant<'a> {
    pub(super) client_id: &'a str,
    pub(super) family_id: &'a str,
    pub(super) family_expires: i64,
}

pub(super) enum Rotation {
    Rotated,
    Invalid,
    Reused,
}

/// The body of [`super::Store::refresh`], on the blocking pool.
pub(super) fn rotate(
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
pub(super) struct PairHashes {
    access: String,
    refresh: String,
}

pub(super) fn new_tokens() -> Tokens {
    Tokens {
        access: random_token(),
        refresh: random_token(),
    }
}

pub(super) fn hashes(tokens: &Tokens) -> PairHashes {
    PairHashes {
        access: hash_token(&tokens.access),
        refresh: hash_token(&tokens.refresh),
    }
}

/// Both rows of a pair, inside the caller's transaction: never half-written.
pub(super) fn insert_pair(
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
