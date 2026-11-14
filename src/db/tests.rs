use std::os::unix::fs::PermissionsExt;

use super::*;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn version_of(path: &Path) -> i64 {
    user_version(&Connection::open(path).unwrap()).unwrap()
}

#[tokio::test]
async fn schema_applies_and_call_roundtrips() {
    let db = Db::memory();
    db.call(|c| {
        c.execute(
            "INSERT INTO jobs (id, status, started_unix) VALUES ('j', 'running', ?1)",
            [123_i64],
        )
    })
    .await
    .unwrap();
    let started: i64 = db
        .call(|c| {
            c.query_row("SELECT started_unix FROM jobs WHERE id = 'j'", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(started, 123);
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
fn open_creates_the_database_and_its_wal_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.db");
    // Whatever the process umask (typically 022 → 0644), the files come out 0600.
    let db = Db::open(&path).unwrap();
    assert_eq!(mode(&path), 0o600, "db file");
    for suffix in ["-wal", "-shm"] {
        let p = sibling(&path, suffix);
        if p.exists() {
            assert_eq!(mode(&p), 0o600, "{suffix}");
        }
    }
    drop(db);
}

#[test]
fn open_tightens_a_pre_existing_loose_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loose.db");
    drop(Connection::open(&path).unwrap());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let wal = sibling(&path, "-wal");
    std::fs::write(&wal, b"").unwrap();
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).unwrap();

    // Held open: SQLite removes the `-wal` when the last connection closes.
    let _db = Db::open(&path).unwrap();

    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&wal), 0o600);
}

#[test]
fn a_fresh_database_reaches_the_latest_version_and_reopen_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v.db");
    Db::open(&path).unwrap();
    assert_eq!(version_of(&path), SCHEMA_VERSION);
    Db::open(&path).unwrap();
    assert_eq!(version_of(&path), SCHEMA_VERSION);
}

/// The jobs schema of the oldest shipped release, before `title`/`output_tail`/
/// `pgid` existed, with a row in it.
fn legacy_v0(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE access_tokens (token TEXT PRIMARY KEY, expires_unix INTEGER NOT NULL);
         CREATE TABLE refresh_tokens (token TEXT PRIMARY KEY, expires_unix INTEGER NOT NULL);
         CREATE TABLE clients (client_id TEXT NOT NULL, redirect_uri TEXT NOT NULL,
                               PRIMARY KEY (client_id, redirect_uri));
         CREATE TABLE jobs (
             id           TEXT PRIMARY KEY,
             status       TEXT NOT NULL,
             code         INTEGER,
             error        TEXT,
             started_unix INTEGER NOT NULL
         );
         INSERT INTO access_tokens VALUES ('plaintext-bearer', 9999999999);
         INSERT INTO clients VALUES ('c1', 'https://app/cb');
         INSERT INTO jobs (id, status, code, started_unix) VALUES ('old', 'exited', 0, 5);
         INSERT INTO jobs (id, status, started_unix) VALUES ('weird', 'zombie', 6);
         INSERT INTO jobs (id, status, started_unix) VALUES (NULL, 'running', 7);",
    )
    .unwrap();
}

#[tokio::test]
async fn a_legacy_v0_database_migrates_to_the_latest_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    legacy_v0(&path);

    let db = Db::open(&path).expect("open must migrate the legacy schema in place");
    assert_eq!(version_of(&path), SCHEMA_VERSION);

    // The job row the server's insert writes (`JobRepo::insert`) lands, with every
    // column the migrations added.
    db.call(|c| {
        c.execute(
            "INSERT INTO jobs (id, title, status, started_unix, output_tail, pgid, boot_id, finished_unix, \
                               client_id, client_name) \
             VALUES ('new', 't', 'running', 1, NULL, 2, 'boot', NULL, 'c1', 'Claude Code')",
            [],
        )
    })
    .await
    .expect("insert against the migrated table");

    let rows: Vec<(String, String)> = db
        .call(|c| {
            let mut stmt = c.prepare("SELECT id, status FROM jobs ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect()
        })
        .await
        .unwrap();
    assert_eq!(
        rows,
        [
            ("new".into(), "running".into()),
            ("old".into(), "exited".into()),
            // An unknown status can't satisfy the CHECK: it becomes `failed`.
            ("weird".into(), "failed".into()),
        ],
        "the null-id row is dropped, the rest survive"
    );

    // A row from before the `interactive` column reads as not interactive.
    let interactive: bool = db
        .call(|c| {
            c.query_row("SELECT interactive FROM jobs WHERE id = 'old'", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert!(!interactive);

    // Plaintext tokens can't be hashed after the fact: they are dropped.
    let tokens: i64 = db
        .call(|c| c.query_row("SELECT COUNT(*) FROM access_tokens", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(tokens, 0);
    // A registered client survives, stamped with a registration time.
    let created: i64 = db
        .call(|c| c.query_row("SELECT created_unix FROM clients", [], |r| r.get(0)))
        .await
        .unwrap();
    assert!(created > 0);
}

#[tokio::test]
async fn the_jobs_table_rejects_an_unknown_status_and_a_null_id() {
    let db = Db::memory();
    let bad_status = db
        .call(|c| {
            c.execute(
                "INSERT INTO jobs (id, status, started_unix) VALUES ('x', 'zombie', 1)",
                [],
            )
        })
        .await;
    assert!(bad_status.is_err(), "status is CHECKed");
    let null_id = db
        .call(|c| {
            c.execute(
                "INSERT INTO jobs (id, status, started_unix) VALUES (NULL, 'running', 1)",
                [],
            )
        })
        .await;
    assert!(null_id.is_err(), "id is NOT NULL");
}

#[test]
fn a_newer_schema_is_refused_by_both_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("newer.db");
    Db::open(&path).unwrap();
    Connection::open(&path)
        .unwrap()
        .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();

    assert!(matches!(Db::open(&path), Err(DbError::NewerSchema { .. })));
    assert!(matches!(
        Db::open_existing(&path),
        Err(DbError::NewerSchema { .. })
    ));
}

#[test]
fn a_failing_migration_step_rolls_back_and_leaves_the_version() {
    let mut conn = Connection::open_in_memory().unwrap();
    // A `jobs` table already carrying `boot_id` makes step 2's ALTER fail.
    conn.execute_batch(
        "PRAGMA user_version = 1;
         CREATE TABLE jobs (id TEXT PRIMARY KEY, status TEXT NOT NULL,
                            started_unix INTEGER NOT NULL, boot_id TEXT);",
    )
    .unwrap();
    assert!(migrate(&mut conn).is_err());
    assert_eq!(user_version(&conn).unwrap(), 1, "version not bumped");
}

#[test]
fn open_existing_on_a_missing_path_errors_and_creates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("typo.db");
    assert!(matches!(Db::open_existing(&path), Err(DbError::Missing(_))));
    assert!(!path.exists(), "nothing created");
}

#[test]
fn open_existing_refuses_an_unmigrated_database_without_altering_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    legacy_v0(&path);
    assert!(matches!(
        Db::open_existing(&path),
        Err(DbError::OlderSchema { found: 0, .. })
    ));
    assert_eq!(version_of(&path), 0, "admin open never migrates");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn open_existing_waits_out_the_servers_write_lock() {
    // Server + CLI model: two connections on one file. The CLI's statements retry
    // within `busy_timeout` instead of failing "database is locked".
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.db");
    let server = Db::open(&path).unwrap();
    let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
    let holder = tokio::spawn(async move {
        server
            .call(move |c| {
                c.execute_batch("BEGIN IMMEDIATE")?;
                locked_tx.send(()).ok();
                std::thread::sleep(std::time::Duration::from_millis(300));
                c.execute_batch("COMMIT")
            })
            .await
    });
    locked_rx.await.unwrap();

    let cli = Db::open_existing(&path).expect("open within busy_timeout");
    cli.call(|c| {
        c.execute(
            "INSERT INTO jobs (id, status, started_unix) VALUES ('cli', 'running', 1)",
            [],
        )
    })
    .await
    .expect("write waits for the server's transaction");
    holder.await.unwrap().unwrap();
}

/// v7: a job from before owners were recorded keeps no owner; clients gain a
/// name and a last-use time, both empty for an existing registration.
#[tokio::test]
async fn v7_adds_owner_and_presence_columns_empty_for_existing_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy.db");
    legacy_v0(&path);
    let db = Db::open(&path).unwrap();
    let (job_owner, client): (Option<String>, (Option<String>, Option<i64>)) = db
        .call(|c| {
            let owner = c.query_row("SELECT client_id FROM jobs WHERE id = 'old'", [], |r| {
                r.get(0)
            })?;
            let client = c.query_row(
                "SELECT client_name, last_used_unix FROM clients WHERE client_id = 'c1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            Ok((owner, client))
        })
        .await
        .unwrap();
    assert_eq!(job_owner, None);
    assert_eq!(client, (None, None));
}
