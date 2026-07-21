//! Reaper: evict aged-out jobs and compact/sweep their logs.
//!
//! Job ages come from the DB `started_unix` (wall clock), so retention/trim tiers
//! stay meaningful across restarts. The in-memory map is consulted only to kill a
//! still-running group before evicting it. Process-group signalling itself lives
//! in `super::signal`.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::signal::{group_alive, kill_job};
use super::{Job, JobId, JobRepo, JobState, ProcessGroupId};
use crate::db::now_unix;

/// Jobs (and their logs) older than this are reaped hourly. Seconds, to compare
/// against the DB's wall-clock `started_unix`.
const RETENTION_SECS: i64 = 24 * 3600;

/// This server is meant to run for weeks. Job logs can't grow without bound, so
/// the hourly pass compacts the logs of *finished* jobs to a trailing tail —
/// enough to debug a failure, not enough to fill the disk. Running jobs are never
/// compacted (their log is still being appended). Tiers, by how long ago the job
/// started: a finished job keeps its last `TRIM_RECENT_LINES` while fresh, drops
/// to `TRIM_AGED_LINES` after `TRIM_AGED_AFTER_SECS`, then is purged at
/// `RETENTION_SECS`.
const TRIM_AGED_AFTER_SECS: i64 = 3 * 3600;
const TRIM_RECENT_LINES: usize = 5_000;
const TRIM_AGED_LINES: usize = 500;
/// First line written into a compacted log. Recognised on the next pass so
/// trimming is idempotent — re-running never erodes the kept tail line by line.
const TRIM_MARKER: &str = "[mcp-ssh: earlier output trimmed";

/// Run a reaping pass once on startup, then hourly. ponytail: time-based only; a
/// busy box could still hold ≤24h of jobs — add a count cap if that ever bites.
pub(super) fn spawn_reaper(
    jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>,
    repo: JobRepo,
    dir: PathBuf,
) {
    tokio::spawn(async move {
        // Run immediately so a long-dead job's log is reclaimed promptly after a
        // restart, then settle into the hourly cadence.
        reaper_pass(&jobs, &repo, &dir).await;
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.tick().await; // the first tick fires immediately — already covered above
        loop {
            tick.tick().await;
            reaper_pass(&jobs, &repo, &dir).await;
        }
    });
}

/// One full pass: purge aged-out jobs, compact the logs of finished survivors,
/// sweep orphan log files left with no row, and drop expired OAuth tokens.
async fn reaper_pass(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, repo: &JobRepo, dir: &Path) {
    reap_once(jobs, repo, dir, RETENTION_SECS).await;
    compact_once(jobs, repo, dir, TRIM_AGED_AFTER_SECS).await;
    reap_orphans(repo, dir, RETENTION_SECS).await;
    // Tokens share this DB and the same wall clock as the job reap, so they ride
    // the same pass rather than a second timer. Counts only — the sweep never
    // reads a token value.
    let now = now_unix();
    crate::oauth::sweep_expired_access(repo.db(), now).await;
    crate::oauth::sweep_expired_refresh(repo.db(), now).await;
}

/// Compact every *finished* job's log to a trailing tail: `TRIM_RECENT_LINES`
/// while younger than `aged_after_secs`, `TRIM_AGED_LINES` once older. Age comes
/// from the DB `started_unix`. Running jobs are skipped — their log is still being
/// written, and truncating under the writer would corrupt it. Idempotent: an
/// already-trimmed log is left alone.
pub(super) async fn compact_once(
    jobs: &Mutex<HashMap<JobId, Arc<Job>>>,
    repo: &JobRepo,
    dir: &Path,
    aged_after_secs: i64,
) {
    let rows = match repo.compaction_rows().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying jobs to compact failed");
            return;
        }
    };

    let now = now_unix();
    for row in rows {
        // Never rewrite a log still being appended. The in-memory map is the
        // authority on liveness in this process; for a row from a previous process
        // (not tracked here) trust the persisted status.
        let running = match jobs.lock().await.get(&row.id) {
            Some(job) => matches!(*job.state.lock().await, JobState::Running),
            None => row.running,
        };
        if running || group_still_writing(row.pgid).await {
            continue;
        }
        let keep = if now - row.started_unix >= aged_after_secs {
            TRIM_AGED_LINES
        } else {
            TRIM_RECENT_LINES
        };
        let path = dir.join(format!("{}.log", row.id));
        match trim_log(&path, keep).await {
            Ok(()) => {}
            // A finished job whose log is already gone (reaped/never produced) is
            // not an error worth logging on every hourly pass.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to trim job log");
            }
        }
    }
}

/// True if a job's persisted process group still has a member, so its log may
/// still be written to whatever the row's status says. A finished status is not
/// proof the log is closed: the job's shell re-points stdout/stderr at the log
/// file (`exec >>log 2>&1`, see `JobStore::run`), so a descendant left running
/// holds that same append fd — and a group that reparented to init across a
/// restart can front a row a reconcile flipped to `failed`. Trimming renames a
/// fresh file over the path, unlinking the inode those writers hold, so every
/// later line lands in a file nobody can read. A row with no usable pgid can't be
/// probed; treat it as done, as before.
async fn group_still_writing(pgid: Option<i64>) -> bool {
    match pgid.and_then(ProcessGroupId::from_persisted) {
        Some(pgid) => group_alive(pgid).await,
        None => false,
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
        // Orphan: drop it only once it's aged past retention by mtime, so a log
        // whose row hasn't been written yet (a brief race) isn't deleted early.
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
        if aged {
            let _ = tokio::fs::remove_file(&path).await;
        }
    }
}

/// Rewrite `path` to its last `keep` lines, prefixed with a `TRIM_MARKER` note.
/// Idempotent and shrink-only: a prior marker is stripped before measuring, so a
/// log already at or under `keep` real lines is left untouched and repeated passes
/// never erode it. The swap is atomic (write temp + rename) so a concurrent
/// `poll` reads either the old log or the new one, never a half-written file.
async fn trim_log(path: &Path, keep: usize) -> std::io::Result<()> {
    let bytes = tokio::fs::read(path).await?;
    let content = String::from_utf8_lossy(&bytes);
    let mut lines = content.lines();
    // Drop a marker left by an earlier pass so it doesn't count toward `keep`.
    let first = lines.next();
    let had_marker = first.is_some_and(|l| l.starts_with(TRIM_MARKER));
    let real: Vec<&str> = match (had_marker, first) {
        (true, _) => lines.collect(),
        (false, Some(f)) => std::iter::once(f).chain(lines).collect(),
        (false, None) => return Ok(()), // empty log
    };
    if real.len() <= keep {
        return Ok(()); // already within budget — nothing to do
    }
    let dropped = real.len() - keep;
    let tail = &real[real.len() - keep..];
    let mut out = format!("{TRIM_MARKER}: dropped {dropped} lines, keeping last {keep}]\n");
    out.push_str(&tail.join("\n"));
    out.push('\n');

    let tmp = path.with_extension("log.trim");
    tokio::fs::write(&tmp, out).await?;
    tokio::fs::rename(&tmp, path).await
}

/// One reaping pass: evict every job whose row is older than `retention_secs`.
/// A still-`Running` job tracked in this process is killed first, so eviction
/// never orphans its process group; only then is the DB row, the in-memory entry,
/// and the log file dropped. A running job whose kill fails stays fully tracked
/// (pollable/killable, row + log intact) for a later pass.
pub(super) async fn reap_once(
    jobs: &Mutex<HashMap<JobId, Arc<Job>>>,
    repo: &JobRepo,
    dir: &Path,
    retention_secs: i64,
) {
    let cutoff = now_unix() - retention_secs;
    let stale = match repo.started_before(cutoff).await {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying stale jobs failed");
            return;
        }
    };

    let mut evictable: Vec<JobId> = Vec::new();
    for id in &stale {
        let live = jobs.lock().await.get(id).cloned();
        match live {
            Some(job) if matches!(*job.state.lock().await, JobState::Running) => {
                // Kill before evict so a live group is never orphaned. If the kill
                // fails while it still reads Running, keep it for a later pass.
                if kill_job(&job).await || !matches!(*job.state.lock().await, JobState::Running) {
                    evictable.push(id.clone());
                } else {
                    tracing::warn!(id = %id, "stale running job not evicted: kill failed");
                }
            }
            _ => evictable.push(id.clone()),
        }
    }

    if evictable.is_empty() {
        return;
    }

    if let Err(error) = repo.delete(evictable.clone()).await {
        tracing::warn!(%error, "reaper: deleting stale rows failed");
    }

    for id in &evictable {
        jobs.lock().await.remove(id);
        let _ = tokio::fs::remove_file(dir.join(format!("{id}.log"))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::signal::kill_group;
    use super::*;

    async fn write_lines(dir: &std::path::Path, name: &str, n: usize) -> std::path::PathBuf {
        let path = dir.join(name);
        let body: String = (1..=n).map(|i| format!("line{i}\n")).collect();
        tokio::fs::write(&path, body).await.unwrap();
        path
    }

    async fn lines_of(path: &Path) -> Vec<String> {
        let s = tokio::fs::read_to_string(path).await.unwrap();
        s.lines().map(str::to_string).collect()
    }

    #[tokio::test]
    async fn reap_orphans_removes_aged_trim_temps_and_keeps_known_logs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = JobRepo::new(crate::db::Db::memory());
        repo.db()
            .call(|conn| {
                conn.execute(
                    "INSERT INTO jobs (id, status, started_unix) VALUES ('known', 'exited', 0)",
                    [],
                )
            })
            .await
            .unwrap();

        // A crashed trim's temp, an orphan log, and a known job's log.
        let trim_tmp = dir.path().join("dead.log.trim");
        let orphan = dir.path().join("orphan.log");
        let known = dir.path().join("known.log");
        for p in [&trim_tmp, &orphan, &known] {
            tokio::fs::write(p, "x\n").await.unwrap();
            // Age past any retention: mtime at the epoch.
            let status = tokio::process::Command::new("touch")
                .args(["-d", "@0"])
                .arg(p)
                .status()
                .await
                .unwrap();
            assert!(status.success());
        }

        reap_orphans(&repo, dir.path(), RETENTION_SECS).await;

        assert!(
            !trim_tmp.exists(),
            "aged .log.trim temp must be swept — no reaper path deleted it before"
        );
        assert!(!orphan.exists(), "aged orphan log must be swept");
        assert!(known.exists(), "a log with a matching row must be kept");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn compact_once_skips_a_log_whose_group_is_still_alive() {
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let repo = JobRepo::new(crate::db::Db::memory());
        let jobs = Mutex::new(HashMap::new());

        // Leader of its own group (pgid == pid), like a real job's shell — and its
        // descendants — still holding the log's append fd.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = ProcessGroupId::new(child.id().expect("child pid")).expect("nonzero pid");
        // Reap in the background so the signalled child leaves no zombie reading
        // as alive — mirrors the real parent (server/init).
        let waiter = tokio::spawn(async move { child.wait().await });

        // The row a mis-reconcile leaves behind: finished per the DB, alive per the
        // OS. Not tracked in the (empty) map, exactly like a previous process's job.
        let started = now_unix();
        repo.db()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs (id, status, started_unix, pgid) \
                     VALUES ('live', 'failed', ?1, ?2)",
                    rusqlite::params![started, i64::from(pgid.get())],
                )
            })
            .await
            .unwrap();
        let log = write_lines(dir.path(), "live.log", 1000).await;

        // `aged_after_secs = 0` puts the row in the aged tier immediately, so the
        // only thing that can save this log is the liveness gate.
        compact_once(&jobs, &repo, dir.path(), 0).await;
        assert_eq!(
            lines_of(&log).await.len(),
            1000,
            "a log whose process group is still alive must not be trimmed"
        );

        // Same row, same budget, group actually gone: now it trims — proving the
        // assertion above is the gate, not an inert pass.
        assert!(kill_group(pgid).await, "group should be gone after kill");
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
        // One dead and one live row per table: lazy eviction never touches either
        // unless the very same token is re-presented.
        repo.db()
            .call(|conn| {
                let (dead, live) = (now_unix() - 1, now_unix() + 3600);
                for table in ["access_tokens", "refresh_tokens"] {
                    conn.execute(
                        &format!("INSERT INTO {table} (token, expires_unix) VALUES ('dead', ?1)"),
                        [dead],
                    )?;
                    conn.execute(
                        &format!("INSERT INTO {table} (token, expires_unix) VALUES ('live', ?1)"),
                        [live],
                    )?;
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
        assert_eq!(access, 1, "the pass must sweep expired access tokens");
        assert_eq!(refresh, 1, "the pass must sweep expired refresh tokens");
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
        assert_eq!(lines[1], "line901", "tail starts at the 100th-from-last");
        assert_eq!(lines[100], "line1000", "the very last line is kept");
    }

    #[tokio::test]
    async fn trim_log_is_idempotent_and_never_erodes_the_tail() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_lines(dir.path(), "j.log", 1000).await;

        trim_log(&log, 100).await.unwrap();
        let after_first = lines_of(&log).await;
        // Re-running at the same budget must not drop another line off the tail.
        trim_log(&log, 100).await.unwrap();
        let after_second = lines_of(&log).await;
        assert_eq!(after_first, after_second, "second pass must be a no-op");
    }

    #[tokio::test]
    async fn trim_log_tightens_when_the_budget_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_lines(dir.path(), "j.log", 1000).await;

        trim_log(&log, 500).await.unwrap(); // recent tier
        trim_log(&log, 50).await.unwrap(); // aged tier
        let lines = lines_of(&log).await;

        assert!(lines[0].starts_with(TRIM_MARKER));
        assert_eq!(
            lines.len(),
            51,
            "marker + last 50 after the prior marker is stripped"
        );
        assert_eq!(
            lines[50], "line1000",
            "still the real last line, not a stale marker"
        );
    }

    #[tokio::test]
    async fn trim_log_leaves_a_short_log_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let log = write_lines(dir.path(), "j.log", 10).await;
        trim_log(&log, 500).await.unwrap();
        let lines = lines_of(&log).await;
        assert_eq!(lines.len(), 10, "under budget — unchanged");
        assert!(!lines[0].starts_with(TRIM_MARKER), "no marker added");
    }
}
