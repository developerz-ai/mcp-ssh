//! Schema versioning: the ordered, forward-only migration steps and the loop that
//! applies them, each in its own `BEGIN IMMEDIATE` transaction.
use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::{DbError, now_unix};

/// A migration step. Runs inside an immediate transaction that also bumps
/// `user_version`, so a failed step leaves the database exactly as it was.
type Migration = fn(&Transaction) -> rusqlite::Result<()>;

/// Every schema step, in order. `user_version` = how many have been applied.
/// Append only — never edit or reorder a shipped step.
const MIGRATIONS: &[Migration] = &[
    adopt_legacy_schema,
    add_job_boot_id,
    add_job_finished_unix,
    constrain_jobs,
    hash_and_bind_tokens,
    add_job_interactive,
    add_agent_identity,
];

/// The newest schema version this binary understands.
pub(super) const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

pub(super) fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
}

/// Apply every pending step. Each runs in its own `BEGIN IMMEDIATE`, re-reading
/// `user_version` under the write lock, so two processes opening at once never
/// apply a step twice.
pub(super) fn migrate(conn: &mut Connection) -> Result<(), DbError> {
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version = user_version(&tx)?;
        if version > SCHEMA_VERSION {
            return Err(DbError::NewerSchema {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        // In range: `version` is 0..=len, so the index cast can't truncate.
        let Some(step) = MIGRATIONS.get(version as usize) else {
            return Ok(());
        };
        step(&tx)?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
}

// ---- migration steps (append only) ----

/// v1: adopt the schema as it stood before versioning. Idempotent, so a database
/// created by any earlier release (tables present, maybe missing the later `jobs`
/// columns) and a brand-new one both land on the same shape.
fn adopt_legacy_schema(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS access_tokens (
             token        TEXT PRIMARY KEY,
             expires_unix INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS refresh_tokens (
             token        TEXT PRIMARY KEY,
             expires_unix INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS clients (
             client_id    TEXT NOT NULL,
             redirect_uri TEXT NOT NULL,
             PRIMARY KEY (client_id, redirect_uri)
         );
         CREATE TABLE IF NOT EXISTS jobs (
             id           TEXT PRIMARY KEY,
             title        TEXT,
             status       TEXT NOT NULL,
             code         INTEGER,
             error        TEXT,
             started_unix INTEGER NOT NULL,
             output_tail  TEXT,
             pgid         INTEGER
         );",
    )?;
    for (column, decl) in [
        ("title", "TEXT"),
        ("output_tail", "TEXT"),
        ("pgid", "INTEGER"),
    ] {
        if !has_column(tx, "jobs", column)? {
            tx.execute(&format!("ALTER TABLE jobs ADD COLUMN {column} {decl}"), [])?;
        }
    }
    Ok(())
}

/// v2: which server process started a job, so the startup reconcile can tell its
/// own rows from a previous process's.
fn add_job_boot_id(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE jobs ADD COLUMN boot_id TEXT;")
}

/// v3: when a job finished, so retention ages a job from its end, not its start.
fn add_job_finished_unix(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE jobs ADD COLUMN finished_unix INTEGER;")
}

/// v4: constraints behind the row mapping — a non-null id and a known status word.
/// SQLite can't add a CHECK to an existing table, so rebuild it; a legacy row with
/// a null id is unaddressable and dropped, an unknown status becomes `failed`.
fn constrain_jobs(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE jobs_v4 (
             id            TEXT PRIMARY KEY NOT NULL,
             title         TEXT,
             status        TEXT NOT NULL CHECK (status IN ('running', 'exited', 'failed')),
             code          INTEGER,           -- exit code when status = 'exited'
             error         TEXT,              -- message when status = 'failed'
             started_unix  INTEGER NOT NULL,
             output_tail   TEXT,              -- bounded tail, saved when the job finishes
             pgid          INTEGER,           -- process group, so `mcp-ssh job kill` can signal it
             boot_id       TEXT,
             finished_unix INTEGER
         );
         INSERT INTO jobs_v4
             SELECT id, title,
                    CASE WHEN status IN ('running', 'exited', 'failed') THEN status ELSE 'failed' END,
                    code, error, started_unix, output_tail, pgid, boot_id, finished_unix
             FROM jobs WHERE id IS NOT NULL;
         DROP TABLE jobs;
         ALTER TABLE jobs_v4 RENAME TO jobs;",
    )
}

/// v5: tokens are stored only as SHA-256 hashes and bound to the client + login
/// ("family") they were issued to; clients get a registration time so stale ones
/// can be swept. Plaintext rows can't be converted, so every existing login is
/// dropped — a documented one-time re-login.
fn hash_and_bind_tokens(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "DROP TABLE access_tokens;
         DROP TABLE refresh_tokens;
         CREATE TABLE access_tokens (
             token_hash   TEXT PRIMARY KEY NOT NULL,
             client_id    TEXT NOT NULL,
             family_id    TEXT NOT NULL,
             expires_unix INTEGER NOT NULL
         );
         CREATE TABLE refresh_tokens (
             token_hash          TEXT PRIMARY KEY NOT NULL,
             client_id           TEXT NOT NULL,
             family_id           TEXT NOT NULL,
             expires_unix        INTEGER NOT NULL,
             family_expires_unix INTEGER NOT NULL
         );
         -- A rotated-away refresh token, kept until its family expires: presenting
         -- one again means the token leaked, so the whole family is revoked.
         CREATE TABLE refresh_tombstones (
             token_hash   TEXT PRIMARY KEY NOT NULL,
             family_id    TEXT NOT NULL,
             expires_unix INTEGER NOT NULL
         );
         ALTER TABLE clients ADD COLUMN created_unix INTEGER NOT NULL DEFAULT 0;",
    )?;
    // Existing clients count as registered now, so the stale-client sweep gives
    // them the full grace period to log in again rather than dropping them at once.
    tx.execute("UPDATE clients SET created_unix = ?1", [now_unix()])?;
    Ok(())
}

/// v6: whether a job was launched through the interactive bash, so a kill that has
/// only the row to go on stops its leader first, as a kill of a tracked job does.
/// Rows from before this step read as not interactive.
fn add_job_interactive(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch("ALTER TABLE jobs ADD COLUMN interactive INTEGER NOT NULL DEFAULT 0;")
}

/// v7: which OAuth client started a job (its id and the name it registered, kept
/// on the row so the owner still reads right after the client is swept), the
/// registered name on `clients`, and when each client last used a token — what
/// lets agents sharing the host see each other. Older rows have no owner.
fn add_agent_identity(tx: &Transaction) -> rusqlite::Result<()> {
    tx.execute_batch(
        "ALTER TABLE jobs ADD COLUMN client_id TEXT;
         ALTER TABLE jobs ADD COLUMN client_name TEXT;
         ALTER TABLE clients ADD COLUMN client_name TEXT;
         ALTER TABLE clients ADD COLUMN last_used_unix INTEGER;",
    )
}

/// `table`/`column` are compile-time constants (never user input), so
/// interpolating them into the pragma is injection-safe.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}
