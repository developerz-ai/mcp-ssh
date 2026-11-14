//! Reaper: reconcile, age out, and compact jobs and their logs.
//!
//! Runs once at startup (which makes its first pass the startup reconcile) and
//! hourly after. It never kills anything: a running job is never aged out, and a
//! finished job whose group still holds a process (`server &`) keeps its row and
//! log until that process is gone. Job ages come from the DB (`finished_unix`,
//! wall clock), so retention stays meaningful across restarts.
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::log::{TailBudget, read_tail};
use super::signal::{Probe, probe_persisted};
use super::store::ReapRow;
use super::{Job, JobId, JobRepo, JobState};
use crate::db::now_unix;

/// A finished job (row + log) is dropped this long after it finished. Seconds,
/// to compare against the DB's wall-clock `finished_unix`.
const RETENTION_SECS: i64 = 24 * 3600;

/// This server is meant to run for weeks. Job logs can't grow without bound, so
/// the hourly pass compacts the logs of *finished* jobs to a trailing tail —
/// enough to debug a failure, not enough to fill the disk. Running jobs are never
/// compacted (their log is still being appended). Tiers, by how long ago the job
/// finished: its last `TRIM_RECENT_LINES` while fresh, `TRIM_AGED_LINES` after
/// `TRIM_AGED_AFTER_SECS`, purged at `RETENTION_SECS` — and never more than
/// `TRIM_MAX_BYTES` of either.
const TRIM_AGED_AFTER_SECS: i64 = 3 * 3600;
const TRIM_RECENT_LINES: usize = 5_000;
const TRIM_AGED_LINES: usize = 500;
const TRIM_MAX_BYTES: usize = 4 * 1024 * 1024;
/// First line written into a compacted log. Recognised on the next pass so
/// trimming is idempotent — re-running never erodes the kept tail.
const TRIM_MARKER: &str = "[mcp-ssh: earlier output trimmed";

/// Run a reaping pass once on startup, then hourly.
pub(super) fn spawn_reaper(
    jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>,
    repo: JobRepo,
    dir: PathBuf,
) {
    tokio::spawn(async move {
        reaper_pass(&jobs, &repo, &dir).await;
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.tick().await; // the first tick fires immediately — already covered above
        loop {
            tick.tick().await;
            reaper_pass(&jobs, &repo, &dir).await;
        }
    });
}

/// One full pass: reconcile untracked `running` rows, purge aged-out jobs, compact
/// the logs of finished survivors, sweep orphan log files, and drop expired OAuth
/// state.
async fn reaper_pass(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, repo: &JobRepo, dir: &Path) {
    reconcile_untracked(jobs, repo).await;
    reap_once(jobs, repo, dir, RETENTION_SECS).await;
    compact_once(jobs, repo, dir, TRIM_AGED_AFTER_SECS).await;
    reap_orphans(repo, dir, RETENTION_SECS).await;
    // OAuth state shares this DB and the same wall clock, so it rides the same
    // pass rather than a second timer. Counts only — no token value is read.
    let now = now_unix();
    crate::oauth::sweep_expired_access(repo.db(), now).await;
    crate::oauth::sweep_expired_refresh(repo.db(), now).await;
    crate::oauth::sweep_stale_clients(repo.db(), now).await;
}

/// Flip `running` rows this process doesn't track to `failed` once their process
/// group is really gone. Such a row was left by a previous process; outside a
/// systemd `KillMode=control-group` stop its group can reparent to init and keep
/// running — then the row stays `running`, truthfully, and is re-checked every
/// pass until the group exits. A row with no signalable group (no or a corrupt
/// pgid, or one recorded under a previous boot) is gone by definition.
///
/// The jobs lock is held across the candidate query, and `supervise` inserts into
/// the map (under that lock) before writing its row — so a row this process
/// creates either has its id in the snapshot or appears only after the query.
pub(super) async fn reconcile_untracked(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, repo: &JobRepo) {
    let candidates = {
        let live = jobs.lock().await;
        let live_ids: Vec<String> = live.keys().map(|id| id.as_ref().to_string()).collect();
        repo.untracked_running(live_ids).await
    };
    let candidates = match candidates {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "job reconcile: querying untracked running rows failed");
            return;
        }
    };
    let mut dead: Vec<JobId> = Vec::new();
    for candidate in candidates {
        match probe_persisted(candidate.pgid, candidate.boot_id.as_deref()) {
            Probe::Gone => dead.push(candidate.id),
            Probe::Alive => {
                tracing::debug!(id = %candidate.id, "job outlived the process that started it — still running");
            }
            Probe::Unknown(error) => {
                tracing::warn!(%error, id = %candidate.id, "job reconcile: cannot probe its group — left running");
            }
        }
    }
    if dead.is_empty() {
        return;
    }
    // Only rows still `running` are flipped: one another writer (the `job kill`
    // CLI) finished since the query keeps its own final state.
    if let Err(error) = repo.mark_restart_failed(dead).await {
        tracing::warn!(%error, "job reconcile: recording dead jobs failed");
    }
}

/// Whether a row's job is running right now. The in-memory map is the authority
/// for a job this process tracks; for one it doesn't, the persisted status.
async fn is_running(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, row: &ReapRow) -> bool {
    let tracked = jobs.lock().await.get(&row.id).cloned();
    match tracked {
        Some(job) => matches!(*job.state.lock().await, JobState::Running),
        None => row.running,
    }
}

/// True unless a row's process group is provably gone. A finished status is not
/// proof the log is closed: the job's shell re-points stdout/stderr at the log
/// (`exec >>log 2>&1`, see `JobStore::run`), so a descendant left running holds
/// that same append fd. Trimming renames a fresh file over the path and eviction
/// unlinks it — either way every later line lands in a file nobody can read. An
/// unknown probe is treated as "may still be writing".
fn group_may_be_writing(row: &ReapRow) -> bool {
    !matches!(
        probe_persisted(row.pgid, row.boot_id.as_deref()),
        Probe::Gone
    )
}

/// Compact every *finished* job's log to a trailing tail: `TRIM_RECENT_LINES`
/// while it finished less than `aged_after_secs` ago, `TRIM_AGED_LINES` after.
/// Running jobs and groups still writing are skipped. Idempotent.
pub(super) async fn compact_once(
    jobs: &Mutex<HashMap<JobId, Arc<Job>>>,
    repo: &JobRepo,
    dir: &Path,
    aged_after_secs: i64,
) {
    let rows = match repo.reap_rows().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying jobs to compact failed");
            return;
        }
    };
    let now = now_unix();
    for row in rows {
        if is_running(jobs, &row).await || group_may_be_writing(&row) {
            continue;
        }
        let keep = if now - row.ended_unix >= aged_after_secs {
            TRIM_AGED_LINES
        } else {
            TRIM_RECENT_LINES
        };
        let path = dir.join(format!("{}.log", row.id));
        match trim_log(&path, keep).await {
            Ok(()) => {}
            // A finished job whose log is already gone is not worth a line per pass.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to trim job log");
            }
        }
    }
}

/// Delete log files with no matching `jobs` row (orphans from a crash or a manually
/// dropped row) once they're older than `retention_secs` by mtime — so a poll
/// racing a just-finished job still finds its log, but truly abandoned files don't
/// accumulate.
async fn reap_orphans(repo: &JobRepo, dir: &Path, retention_secs: i64) {
    let known: HashSet<String> = match repo.all_ids().await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying known job ids failed");
            return;
        }
    };

    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, dir = %dir.display(), "reaper: reading job dir failed");
            return;
        }
    };
    let retention = Duration::from_secs(retention_secs.max(0) as u64);
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "reaper: scanning job dir failed");
                break;
            }
        };
        let path = entry.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("log") => {
                // `<id>.log` -> `<id>`; ids never contain a `.`, so the stem is
                // the id.
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if known.contains(stem) {
                    continue;
                }
            }
            // `<id>.log.trim` — a temp a crashed `trim_log` never renamed. Always
            // an orphan (a completed trim renames it away); the mtime gate below
            // protects one belonging to a trim in progress right now.
            Some("trim") => {}
            _ => continue,
        }
        let aged = match entry.metadata().await.and_then(|m| m.modified()) {
            Ok(modified) => SystemTime::now()
                .duration_since(modified)
                .map(|age| age > retention)
                .unwrap_or(false),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "reaper: stat orphan log failed");
                continue;
            }
        };
        if aged && let Err(error) = tokio::fs::remove_file(&path).await {
            tracing::warn!(%error, path = %path.display(), "reaper: removing orphan log failed");
        }
    }
}

/// Rewrite `path` to its last `keep` lines (within `TRIM_MAX_BYTES`), prefixed
/// with a `TRIM_MARKER` note. Reads only that bounded tail, never the whole log.
/// Idempotent and shrink-only: a log whose kept tail already reaches back to its
/// start (or to a prior marker) is left untouched, so repeated passes never erode
/// it. The swap is atomic (write an owner-only temp + rename), so a concurrent
/// `poll` reads either the old log or the new one, never a half-written file.
async fn trim_log(path: &Path, keep: usize) -> std::io::Result<()> {
    let budget = TailBudget {
        lines: keep,
        bytes: TRIM_MAX_BYTES,
    };
    let page = read_tail(path, None, budget).await?;
    let Some(dropped) = page.older else {
        return Ok(()); // the whole log fits — nothing to drop
    };
    if dropped <= marker_len(path).await? {
        return Ok(()); // everything but an earlier pass's marker fits
    }
    let mut out = format!(
        "{TRIM_MARKER}: dropped {dropped} bytes, keeping the last {} lines]\n",
        page.lines.len()
    );
    for line in &page.lines {
        out.push_str(line);
        out.push('\n');
    }

    let tmp = path.with_extension("log.trim");
    let write = tmp.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&write)?;
        file.write_all(out.as_bytes())
    })
    .await
    .map_err(std::io::Error::other)??;
    tokio::fs::rename(&tmp, path).await
}

/// Byte length of the `TRIM_MARKER` line a previous pass left at the head of
/// `path` (including its newline), or 0 when there is none.
async fn marker_len(path: &Path) -> std::io::Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut head = vec![0u8; 256];
    let mut file = tokio::fs::File::open(path).await?;
    let mut filled = 0;
    while filled < head.len() {
        let n = file.read(&mut head[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let head = &head[..filled];
    if !head.starts_with(TRIM_MARKER.as_bytes()) {
        return Ok(0);
    }
    Ok(head
        .iter()
        .position(|&b| b == b'\n')
        .map_or(0, |nl| nl as u64 + 1))
}

/// One eviction pass: drop every *finished* job that finished more than
/// `retention_secs` ago — row, in-memory entry, and log. A running job is never
/// aged out (it is not killed for age either), and a finished one whose group
/// still holds a process stays until that process is gone. The logs go only once
/// their rows are deleted.
pub(super) async fn reap_once(
    jobs: &Mutex<HashMap<JobId, Arc<Job>>>,
    repo: &JobRepo,
    dir: &Path,
    retention_secs: i64,
) {
    let rows = match repo.reap_rows().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying jobs to age out failed");
            return;
        }
    };
    let cutoff = now_unix() - retention_secs;
    let mut evictable: Vec<JobId> = Vec::new();
    for row in rows {
        if row.ended_unix >= cutoff || is_running(jobs, &row).await {
            continue;
        }
        if group_may_be_writing(&row) {
            tracing::info!(id = %row.id, "aged-out job still has live processes — kept");
            continue;
        }
        evictable.push(row.id);
    }
    if evictable.is_empty() {
        return;
    }
    if let Err(error) = repo.delete(evictable.clone()).await {
        // Keep the logs: a row still pointing at a deleted log reads as a job with
        // no output, and the next pass retries the whole batch.
        tracing::warn!(%error, "reaper: deleting aged-out rows failed");
        return;
    }
    for id in &evictable {
        jobs.lock().await.remove(id);
        let path = dir.join(format!("{id}.log"));
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "reaper: removing job log failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProcessGroupId;
    use super::super::signal::{Kill, escalate};
    use super::*;

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
}
