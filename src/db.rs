//! The single SQLite database behind the server's durable state: OAuth
//! access/refresh tokens (so logins survive a restart) and job metadata + a saved
//! output tail (so `job(list)`/`poll` show history across restarts). High-frequency
//! live job output still streams to per-job log files — SQLite only holds
//! low-frequency structured state, so one serialized connection is ample.
//!
//! `rusqlite` is synchronous; every call runs inside `spawn_blocking` so the async
//! runtime is never blocked, and the connection lock is taken *inside* the blocking
//! closure — never held across an `.await`.
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS access_tokens (
    token        TEXT PRIMARY KEY,
    expires_unix INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS refresh_tokens (
    token        TEXT PRIMARY KEY,
    expires_unix INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS jobs (
    id           TEXT PRIMARY KEY,
    title        TEXT,
    status       TEXT NOT NULL,   -- 'running' | 'exited' | 'failed'
    code         INTEGER,         -- exit code when status = 'exited'
    error        TEXT,            -- message when status = 'failed'
    started_unix INTEGER NOT NULL,
    output_tail  TEXT,            -- bounded tail, saved when the job finishes
    pgid         INTEGER          -- process group id, so `mcp-ssh job kill` can signal it
);
";

/// Handle to the shared SQLite connection. Cheap to clone (`Arc`).
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    /// Open (creating if absent) the database at `path`, apply the schema, and set
    /// pragmas for a long-running single-writer service: WAL for durable, concurrent
    /// reads; a busy timeout so a momentary lock retries instead of erroring.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        // Forward-only migration for databases created before `jobs.pgid` existed
        // (CREATE TABLE IF NOT EXISTS won't add a column to an existing table).
        ensure_column(&conn, "jobs", "pgid", "INTEGER")?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// An in-memory database for tests — same schema, no file.
    #[cfg(test)]
    pub fn memory() -> Self {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(SCHEMA).expect("apply schema");
        Self {
            conn: Arc::new(Mutex::new(conn)),
        }
    }

    /// Run `f` against the connection on the blocking pool. The guard lives only
    /// inside the closure (a separate thread), so it never crosses an `.await`.
    ///
    /// Never panics: this sits on the request path (every bearer validation and
    /// job write), where a panic would poison the mutex and turn ALL later calls
    /// into panics until restart. A poisoned lock is recovered (the connection
    /// itself is fine — only the closure panicked mid-hold), and a panicking
    /// closure comes back as an error to its caller alone.
    pub async fn call<F, T>(&self, f: F) -> rusqlite::Result<T>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&guard)
        })
        .await
        .unwrap_or_else(|join_error| {
            Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(format!("db task panicked: {join_error}")),
            ))
        })
    }
}

/// Add `column` to `table` if it isn't already present — a tiny idempotent
/// migration. `table`/`column`/`decl` are compile-time constants (never user
/// input), so interpolating them into the DDL is injection-safe.
fn ensure_column(conn: &Connection, table: &str, column: &str, decl: &str) -> rusqlite::Result<()> {
    let present = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let names = stmt.query_map([], |r| r.get::<_, String>(1))?;
        names.filter_map(Result::ok).any(|name| name == column)
    };
    if !present {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

/// Seconds since the Unix epoch, the on-disk time unit for every expiry/timestamp.
/// Wall-clock (not monotonic) precisely so values stay meaningful across restarts.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn schema_applies_and_call_roundtrips() {
        let db = Db::memory();
        // The token tables exist and a write/read roundtrips through `call`.
        db.call(|c| {
            c.execute(
                "INSERT INTO access_tokens (token, expires_unix) VALUES (?1, ?2)",
                ("abc", 123_i64),
            )
        })
        .await
        .unwrap();
        let exp: i64 = db
            .call(|c| {
                c.query_row(
                    "SELECT expires_unix FROM access_tokens WHERE token = ?1",
                    ["abc"],
                    |r| r.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(exp, 123);
    }

    #[tokio::test]
    async fn panicking_closure_is_an_error_and_the_connection_survives() {
        // A panic inside a db closure must come back as Err to its caller only —
        // not panic the request path, and not poison the connection for every
        // later call (which previously turned one bug into a full outage).
        let db = Db::memory();
        let r = db
            .call(|_conn| -> rusqlite::Result<()> { panic!("boom") })
            .await;
        assert!(r.is_err(), "panicking closure must surface as an error");

        let one: i64 = db
            .call(|conn| conn.query_row("SELECT 1", [], |row| row.get(0)))
            .await
            .expect("connection must remain usable after a closure panic");
        assert_eq!(one, 1);
    }

    #[test]
    fn ensure_column_adds_missing_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER);").unwrap();

        let has_b = |c: &Connection| -> bool {
            let mut stmt = c.prepare("PRAGMA table_info(t)").unwrap();
            let names = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
            names.filter_map(Result::ok).any(|n| n == "b")
        };
        assert!(!has_b(&conn), "column starts absent");
        ensure_column(&conn, "t", "b", "INTEGER").unwrap();
        assert!(has_b(&conn), "column added");
        // Second call is a no-op, not a duplicate-column error.
        ensure_column(&conn, "t", "b", "INTEGER").unwrap();
        assert!(has_b(&conn));
    }
}
