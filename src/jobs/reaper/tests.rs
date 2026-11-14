use super::super::ProcessGroupId;
use super::super::signal::{Kill, escalate};
use super::*;
use compact::{TRIM_AGED_LINES, TRIM_MARKER, TRIM_MAX_BYTES, trim_log};

async fn write_lines(dir: &Path, name: &str, n: usize) -> PathBuf {
    let path = dir.join(name);
    let body: String = (1..=n).map(|i| format!("line{i}\n")).collect();
    tokio::fs::write(&path, body).await.unwrap();
    path
}

async fn lines_of(path: &Path) -> Vec<String> {
    let s = tokio::fs::read_to_string(path).await.unwrap();
    s.lines().map(str::to_string).collect()
}

async fn plant(repo: &JobRepo, sql: &'static str, params: Vec<rusqlite::types::Value>) {
    repo.db()
        .call(move |conn| conn.execute(sql, rusqlite::params_from_iter(params)))
        .await
        .unwrap();
}

fn sleeper() -> (ProcessGroupId, tokio::task::JoinHandle<()>) {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 300")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = ProcessGroupId::new(child.id().unwrap()).unwrap();
    let waiter = tokio::spawn(async move {
        let _ = child.wait().await;
    });
    (pgid, waiter)
}

#[tokio::test]
async fn reap_orphans_removes_aged_trim_temps_and_keeps_known_logs() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JobRepo::new(crate::db::Db::memory());
    plant(
        &repo,
        "INSERT INTO jobs (id, status, started_unix) VALUES ('known', 'exited', 0)",
        vec![],
    )
    .await;
    let trim_tmp = dir.path().join("dead.log.trim");
    let orphan = dir.path().join("orphan.log");
    let known = dir.path().join("known.log");
    for p in [&trim_tmp, &orphan, &known] {
        let file = std::fs::File::create(p).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
    }
    reap_orphans(&repo, dir.path(), RETENTION_SECS).await;
    assert!(!trim_tmp.exists(), "aged .log.trim temp must be swept");
    assert!(!orphan.exists(), "aged orphan log must be swept");
    assert!(known.exists(), "a log with a matching row must be kept");
}

/// K5: every pass re-checks untracked `running` rows — a dead group flips to
/// `failed` (and then ages from now), a live one stays `running`.
#[tokio::test]
async fn each_pass_reconciles_untracked_running_rows() {
    let repo = JobRepo::new(crate::db::Db::memory());
    let jobs = Mutex::new(HashMap::new());
    let (pgid, waiter) = sleeper();
    plant(
        &repo,
        "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('live', 'running', 1, ?1), \
         ('dead', 'running', 1, 2000000000)",
        vec![i64::from(pgid.get()).into()],
    )
    .await;
    reconcile_untracked(&jobs, &repo).await;
    let rows = repo.reap_rows().await.unwrap();
    let running = |id: &str| rows.iter().find(|r| r.id.as_ref() == id).unwrap().running;
    assert!(running("live"), "a live group's row stays running");
    assert!(!running("dead"), "a dead group's row is failed");
    assert!(
        rows.iter()
            .find(|r| r.id.as_ref() == "dead")
            .unwrap()
            .ended_unix
            > 1,
        "and ages from now, not from its start"
    );
    let _ = escalate(pgid, Kill::Immediate).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
}

/// K4/K13: an aged-out finished job whose group still holds a process keeps
/// its row and log (nothing is killed for age); once the group is gone it goes.
#[tokio::test]
async fn an_aged_out_job_with_live_leftovers_is_kept_until_they_exit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JobRepo::new(crate::db::Db::memory());
    let jobs = Mutex::new(HashMap::new());
    let (pgid, waiter) = sleeper();
    plant(
        &repo,
        "INSERT INTO jobs (id, status, started_unix, finished_unix, pgid) \
         VALUES ('left', 'exited', 1, 1, ?1)",
        vec![i64::from(pgid.get()).into()],
    )
    .await;
    let log = write_lines(dir.path(), "left.log", 3).await;

    reap_once(&jobs, &repo, dir.path(), RETENTION_SECS).await;
    assert!(log.exists(), "a log still being written is kept");
    assert!(
        matches!(super::super::signal::probe(pgid), Probe::Alive),
        "not killed"
    );

    let _ = escalate(pgid, Kill::Immediate).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    reap_once(&jobs, &repo, dir.path(), RETENTION_SECS).await;
    assert!(!log.exists());
    assert!(repo.all_ids().await.unwrap().is_empty());
}

/// Retention runs from the finish: a job that ran for days and just finished
/// is not purged on the next pass.
#[tokio::test]
async fn retention_counts_from_the_finish_not_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JobRepo::new(crate::db::Db::memory());
    let jobs = Mutex::new(HashMap::new());
    plant(
        &repo,
        "INSERT INTO jobs (id, status, started_unix, finished_unix) VALUES \
         ('long', 'exited', 1, ?1), ('old', 'exited', 1, 1)",
        vec![now_unix().into()],
    )
    .await;
    reap_once(&jobs, &repo, dir.path(), RETENTION_SECS).await;
    assert_eq!(
        repo.all_ids().await.unwrap(),
        HashSet::from(["long".to_string()])
    );
}

/// K12: a failed DELETE keeps the logs, so no surviving row loses its output.
#[tokio::test]
async fn a_failed_row_delete_keeps_the_logs() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JobRepo::new(crate::db::Db::memory());
    let jobs = Mutex::new(HashMap::new());
    plant(
        &repo,
        "INSERT INTO jobs (id, status, started_unix, finished_unix) VALUES ('old', 'exited', 1, 1)",
        vec![],
    )
    .await;
    let log = write_lines(dir.path(), "old.log", 3).await;
    repo.db()
        .call(|c| {
            c.execute_batch(
                "CREATE TRIGGER no_delete BEFORE DELETE ON jobs \
                 BEGIN SELECT RAISE(ABORT, 'refused'); END;",
            )
        })
        .await
        .unwrap();
    reap_once(&jobs, &repo, dir.path(), RETENTION_SECS).await;
    assert!(log.exists());
}

#[tokio::test]
async fn compact_once_skips_a_log_whose_group_is_still_alive() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JobRepo::new(crate::db::Db::memory());
    let jobs = Mutex::new(HashMap::new());
    let (pgid, waiter) = sleeper();
    plant(
        &repo,
        "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('live', 'failed', ?1, ?2)",
        vec![now_unix().into(), i64::from(pgid.get()).into()],
    )
    .await;
    let log = write_lines(dir.path(), "live.log", 1000).await;

    compact_once(&jobs, &repo, dir.path(), 0).await;
    assert_eq!(
        lines_of(&log).await.len(),
        1000,
        "a live group's log is untouched"
    );

    let _ = escalate(pgid, Kill::Immediate).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    compact_once(&jobs, &repo, dir.path(), 0).await;
    let lines = lines_of(&log).await;
    assert!(lines[0].starts_with(TRIM_MARKER), "marker first: {lines:?}");
    assert_eq!(lines.len(), TRIM_AGED_LINES + 1, "marker + aged tail");
}

#[tokio::test]
async fn reaper_pass_sweeps_expired_oauth_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let repo = JobRepo::new(crate::db::Db::memory());
    let jobs = Mutex::new(HashMap::new());
    repo.db()
        .call(|conn| {
            let (dead, live) = (now_unix() - 1, now_unix() + 3600);
            for table in ["access_tokens", "refresh_tokens"] {
                let extra = if table == "refresh_tokens" { ", family_expires_unix" } else { "" };
                let extra_v = if table == "refresh_tokens" { ", ?1" } else { "" };
                for (hash, exp) in [("dead", dead), ("live", live)] {
                    conn.execute(
                        &format!(
                            "INSERT INTO {table} (token_hash, client_id, family_id, expires_unix{extra}) \
                             VALUES ('{hash}', 'c', 'f', ?1{extra_v})"
                        ),
                        [exp],
                    )?;
                }
            }
            Ok(())
        })
        .await
        .unwrap();

    reaper_pass(&jobs, &repo, dir.path()).await;

    let (access, refresh): (i64, i64) = repo
        .db()
        .call(|conn| {
            conn.query_row(
                "SELECT (SELECT COUNT(*) FROM access_tokens), \
                        (SELECT COUNT(*) FROM refresh_tokens)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!((access, refresh), (1, 1), "expired tokens swept, live kept");
}

#[tokio::test]
async fn trim_log_keeps_the_tail_with_a_marker() {
    let dir = tempfile::tempdir().unwrap();
    let log = write_lines(dir.path(), "j.log", 1000).await;
    trim_log(&log, 100).await.unwrap();
    let lines = lines_of(&log).await;
    assert!(
        lines[0].starts_with(TRIM_MARKER),
        "marker first: {:?}",
        lines[0]
    );
    assert_eq!(lines.len(), 101, "marker + last 100");
    assert_eq!(lines[1], "line901");
    assert_eq!(lines[100], "line1000");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a compacted log stays owner-only");
}

#[tokio::test]
async fn trim_log_is_idempotent_and_tightens_when_the_budget_shrinks() {
    let dir = tempfile::tempdir().unwrap();
    let log = write_lines(dir.path(), "j.log", 1000).await;
    trim_log(&log, 500).await.unwrap();
    let first = lines_of(&log).await;
    trim_log(&log, 500).await.unwrap();
    assert_eq!(first, lines_of(&log).await, "a second pass is a no-op");
    trim_log(&log, 50).await.unwrap();
    let lines = lines_of(&log).await;
    assert_eq!(lines.len(), 51, "marker + last 50, the old marker dropped");
    assert_eq!(lines[50], "line1000");
}

#[tokio::test]
async fn trim_log_leaves_a_short_log_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let log = write_lines(dir.path(), "j.log", 10).await;
    trim_log(&log, 500).await.unwrap();
    let lines = lines_of(&log).await;
    assert_eq!(lines.len(), 10);
    assert!(!lines[0].starts_with(TRIM_MARKER));
}

/// The trim reads only a bounded tail: a 2 GiB sparse log is compacted
/// without being read whole.
#[tokio::test]
async fn trim_log_of_a_huge_log_reads_a_bounded_tail() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("huge.log");
    let file = std::fs::File::create(&log).unwrap();
    file.set_len(2 * 1024 * 1024 * 1024).unwrap();
    drop(file);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"\nlast\n").unwrap();
    }
    tokio::time::timeout(Duration::from_secs(10), trim_log(&log, 500))
        .await
        .expect("bounded")
        .unwrap();
    let len = std::fs::metadata(&log).unwrap().len();
    assert!(len <= TRIM_MAX_BYTES as u64 + 4096, "{len}");
    assert_eq!(
        lines_of(&log).await.last().map(String::as_str),
        Some("last")
    );
}
