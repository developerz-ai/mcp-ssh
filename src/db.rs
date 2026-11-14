//! The single SQLite database behind the server's durable state: OAuth
//! access/refresh tokens (so logins survive a restart) and job metadata + a saved
//! output tail (so `job(list)`/`poll` show history across restarts). High-frequency
//! live job output still streams to per-job log files — SQLite only holds
//! low-frequency structured state, so one serialized connection is ample.
//!
//! `rusqlite` is synchronous; every call runs inside `spawn_blocking` so the async
//! runtime is never blocked, and the connection lock is taken *inside* the blocking
//! closure — never held across an `.await`.
//!
//! The schema is versioned by `PRAGMA user_version`: `migrations::MIGRATIONS` is an ordered,
//! forward-only list, each step applied in its own `BEGIN IMMEDIATE` transaction.
//! A schema change is a new function appended there — nothing else.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};

mod migrations;

use migrations::{SCHEMA_VERSION, migrate, user_version};

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database {0} does not exist (has the server been started?)")]
    Missing(PathBuf),
    #[error(
        "database schema v{found} is newer than this binary supports (v{supported}); \
         upgrade mcp-ssh rather than run an older binary against it"
    )]
    NewerSchema { found: i64, supported: i64 },
    #[error(
        "database schema v{found} is older than this binary's (v{supported}); \
         restart the server once to migrate it"
    )]
    OlderSchema { found: i64, supported: i64 },
    #[error("database file {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Handle to the shared SQLite connection. Cheap to clone (`Arc`).
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Open (creating if absent) the server's database at `path`, migrate it to
    /// [`SCHEMA_VERSION`], and set pragmas for a long-running single-writer service.
    /// The file and its `-wal`/`-shm` siblings are owner-only (0600): they hold
    /// bearer-token hashes and job history.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        create_owner_only(path)?;
        let mut conn = Connection::open(path)?;
        set_pragmas(&conn)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        migrate(&mut conn)?;
        // WAL mode creates `-wal`/`-shm`; SQLite gives them the main file's mode,
        // but a pre-existing loose one (from before this fix) is tightened here.
        for p in [
            path.to_path_buf(),
            sibling(path, "-wal"),
            sibling(path, "-shm"),
        ] {
            tighten_if_present(&p)?;
        }
        Ok(Self::wrap(conn))
    }

    /// Open an *existing* database for the local admin commands: never creates the
    /// file, never migrates, and refuses a schema this binary doesn't match — so a
    /// typo'd path or a `sudo` run before the server's first start changes nothing.
    pub fn open_existing(path: &Path) -> Result<Self, DbError> {
        if !path.is_file() {
            return Err(DbError::Missing(path.to_path_buf()));
        }
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags)?;
        set_pragmas(&conn)?;
        match user_version(&conn)? {
            v if v > SCHEMA_VERSION => Err(DbError::NewerSchema {
                found: v,
                supported: SCHEMA_VERSION,
            }),
            v if v < SCHEMA_VERSION => Err(DbError::OlderSchema {
                found: v,
                supported: SCHEMA_VERSION,
            }),
            _ => Ok(Self::wrap(conn)),
        }
    }

    /// An in-memory database for tests — the real migrated schema, no file.
    #[cfg(test)]
    pub fn memory() -> Self {
        let mut conn = Connection::open_in_memory().expect("open in-memory db");
        migrate(&mut conn).expect("migrate in-memory db");
        Self::wrap(conn)
    }

    fn wrap(conn: Connection) -> Self {
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

/// `busy_timeout` first: every later statement (including `journal_mode`, which
/// needs a lock) then retries a momentary lock held by the other process (server
/// vs. admin CLI) instead of failing with "database is locked".
fn set_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "ON")
}

// ---- file modes ----

/// Create `path` owner-only before SQLite opens it, so the database never exists
/// with the process umask's mode. An existing file is left as is (and tightened
/// after open).
fn create_owner_only(path: &Path) -> Result<(), DbError> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
        .map(drop)
        .map_err(|e| DbError::Io(path.to_path_buf(), e))
}

/// chmod 0600 if the file exists. Failing is an error, not a warning: a loose
/// mode here leaves token hashes and job history readable by any local user.
fn tighten_if_present(path: &Path) -> Result<(), DbError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = match std::fs::metadata(path) {
            Ok(meta) => meta.permissions().mode() & 0o777,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(DbError::Io(path.to_path_buf(), e)),
        };
        if mode != 0o600 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| DbError::Io(path.to_path_buf(), e))?;
        }
    }
    Ok(())
}

/// `path` with `suffix` appended to its file name (`mcp-ssh.db` → `mcp-ssh.db-wal`).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Seconds since the Unix epoch, the on-disk time unit for every expiry/timestamp.
/// Wall-clock (not monotonic) precisely so values stay meaningful across restarts.
///
/// Fails closed: a clock before the epoch reads as the far future, so every token
/// looks expired (re-login) rather than every token looking valid forever.
pub fn now_unix() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => {
            tracing::warn!("system clock is before the Unix epoch; treating now as the far future");
            i64::MAX
        }
    }
}

#[cfg(test)]
mod tests;
