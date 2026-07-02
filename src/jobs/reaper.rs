//! Reaper + kill signalling: evict aged-out jobs and terminate process groups.
//!
//! A job leads its own process group (so its pgid equals its pid; see
//! `JobStore::run`), which lets a single signal to the negative pid reach the
//! whole tree the command spawned, not just `sh` itself.
//!
//! Job ages come from the DB `started_unix` (wall clock), so retention/trim tiers
//! stay meaningful across restarts. The in-memory map is consulted only to kill a
//! still-running group before evicting it.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::{Mutex, watch};

use super::{Job, JobId, JobState, ProcessGroupId};
use crate::db::{Db, now_unix};

/// Jobs (and their logs) older than this are reaped hourly. Seconds, to compare
/// against the DB's wall-clock `started_unix`.
const RETENTION_SECS: i64 = 24 * 3600;
/// Grace between `SIGTERM` and `SIGKILL` when killing a job's process group.
const KILL_GRACE: Duration = Duration::from_secs(2);

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

/// Signal a job's process group dead: `SIGTERM`, then `SIGKILL` if it outlasts a
/// short grace. Returns `true` if it signalled a running job, `false` if there
/// was nothing to kill (already finished, the OS withheld its pid) or the signal
/// could not be delivered.
pub(super) async fn kill_job(job: &Job) -> bool {
    if !matches!(*job.state.lock().await, JobState::Running) {
        return false;
    }
    let Some(pgid) = job.pgid else {
        return false;
    };
    if !signal_group(pgid, "TERM").await {
        return false;
    }
    // Give the group a chance to exit on TERM; force it with KILL otherwise.
    if !exited_within(job.done.clone(), KILL_GRACE).await && !signal_group(pgid, "KILL").await {
        // The group can die between the grace expiring and the KILL — its own
        // TERM worked, the KILL just found nothing. Report by final state, not
        // by KILL delivery, so that success isn't misread as "nothing to kill".
        return !matches!(*job.state.lock().await, JobState::Running);
    }
    true
}

/// Kill a process group by raw pgid, for callers that hold no in-process `Job`
/// (the `mcp-ssh job kill` CLI, which acts on the persisted pgid). `SIGTERM`,
/// then `SIGKILL` if the group outlives a short grace. Returns whether the group
/// is gone afterwards. Liveness is probed with `kill -0` rather than a completion
/// flag — the group's real parent (the server, or init after a restart) reaps the
/// exited process, so no zombie lingers to read as alive.
pub(crate) async fn kill_group(pgid: u32) -> bool {
    let pg = ProcessGroupId(pgid);
    if !group_alive(pgid).await {
        return true; // nothing to signal — already gone
    }
    let _ = signal_group(pg, "TERM").await;
    let start = tokio::time::Instant::now();
    while start.elapsed() < KILL_GRACE {
        if !group_alive(pgid).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if group_alive(pgid).await {
        let _ = signal_group(pg, "KILL").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !group_alive(pgid).await
}

/// True if process group `pgid` still has at least one member. `kill -0` delivers
/// no signal, only checks deliverability; stdio is discarded so a "No such
/// process" line never reaches the terminal.
async fn group_alive(pgid: u32) -> bool {
    tokio::process::Command::new("kill")
        .arg("-0")
        .arg("--")
        .arg(format!("-{pgid}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Send `signal` (`"TERM"`, `"KILL"`, …) to process group `pgid`. The negative
/// pid targets the whole group so descendants die too, not just `sh`; `--` stops
/// `kill` reading it as an option. Returns whether the signal was delivered.
/// ponytail: pid reuse is a non-issue here.
async fn signal_group(pgid: ProcessGroupId, signal: &str) -> bool {
    match tokio::process::Command::new("kill")
        .arg(format!("-{signal}"))
        .arg("--")
        .arg(format!("-{}", pgid.0))
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(error) => {
            tracing::warn!(%error, pgid = pgid.0, signal, "failed to signal process group");
            false
        }
    }
}

/// Wait up to `grace` for the job to exit, watching its completion flag rather
/// than polling. Returns true if it exited in time, false if the grace elapsed.
async fn exited_within(mut done: watch::Receiver<bool>, grace: Duration) -> bool {
    // The waiter flips the flag to true exactly once, on exit. A receiver error
    // means the sender dropped, which only happens after that same exit.
    tokio::time::timeout(grace, done.wait_for(|&exited| exited))
        .await
        .is_ok()
}

/// Run a reaping pass once on startup, then hourly. ponytail: time-based only; a
/// busy box could still hold ≤24h of jobs — add a count cap if that ever bites.
pub(super) fn spawn_reaper(jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>, db: Db, dir: PathBuf) {
    tokio::spawn(async move {
        // Run immediately so a long-dead job's log is reclaimed promptly after a
        // restart, then settle into the hourly cadence.
        reaper_pass(&jobs, &db, &dir).await;
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.tick().await; // the first tick fires immediately — already covered above
        loop {
            tick.tick().await;
            reaper_pass(&jobs, &db, &dir).await;
        }
    });
}

/// One full pass: purge aged-out jobs, compact the logs of finished survivors, and
/// sweep orphan log files left with no row.
async fn reaper_pass(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, db: &Db, dir: &Path) {
    reap_once(jobs, db, dir, RETENTION_SECS).await;
    compact_once(jobs, db, dir, TRIM_AGED_AFTER_SECS).await;
    reap_orphans(db, dir, RETENTION_SECS).await;
}

/// Compact every *finished* job's log to a trailing tail: `TRIM_RECENT_LINES`
/// while younger than `aged_after_secs`, `TRIM_AGED_LINES` once older. Age comes
/// from the DB `started_unix`. Running jobs are skipped — their log is still being
/// written, and truncating under the writer would corrupt it. Idempotent: an
/// already-trimmed log is left alone.
pub(super) async fn compact_once(
    jobs: &Mutex<HashMap<JobId, Arc<Job>>>,
    db: &Db,
    dir: &Path,
    aged_after_secs: i64,
) {
    let rows = match db
        .call(|conn| {
            let mut stmt = conn.prepare("SELECT id, status, started_unix FROM jobs")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying jobs to compact failed");
            return;
        }
    };

    let now = now_unix();
    for (id, status, started) in rows {
        // Never rewrite a log still being appended. The in-memory map is the
        // authority on liveness in this process; for a row from a previous process
        // (not tracked here) trust the persisted status.
        let jid = JobId::from(id.clone());
        let running = match jobs.lock().await.get(&jid) {
            Some(job) => matches!(*job.state.lock().await, JobState::Running),
            None => status == "running",
        };
        if running {
            continue;
        }
        let keep = if now - started >= aged_after_secs {
            TRIM_AGED_LINES
        } else {
            TRIM_RECENT_LINES
        };
        let path = dir.join(format!("{id}.log"));
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

/// Delete log files with no matching `jobs` row (orphans from a crash or a manually
/// dropped row) once they're older than `retention_secs` by mtime — so a poll
/// racing a just-finished job still finds its log, but truly abandoned files don't
/// accumulate.
async fn reap_orphans(db: &Db, dir: &Path, retention_secs: i64) {
    let known: HashSet<String> = match db
        .call(|conn| {
            let mut stmt = conn.prepare("SELECT id FROM jobs")?;
            let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
            ids.collect::<rusqlite::Result<HashSet<_>>>()
        })
        .await
    {
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
    db: &Db,
    dir: &Path,
    retention_secs: i64,
) {
    let cutoff = now_unix() - retention_secs;
    let stale = match db
        .call(move |conn| {
            let mut stmt = conn.prepare("SELECT id FROM jobs WHERE started_unix < ?1")?;
            let ids = stmt.query_map([cutoff], |r| r.get::<_, String>(0))?;
            ids.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
    {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying stale jobs failed");
            return;
        }
    };

    let mut evictable: Vec<String> = Vec::new();
    for id in &stale {
        let jid = JobId::from(id.clone());
        let live = jobs.lock().await.get(&jid).cloned();
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

    let ids = evictable.clone();
    if let Err(error) = db
        .call(move |conn| {
            for id in &ids {
                conn.execute("DELETE FROM jobs WHERE id = ?1", [id.as_str()])?;
            }
            Ok(())
        })
        .await
    {
        tracing::warn!(%error, "reaper: deleting stale rows failed");
    }

    for id in &evictable {
        jobs.lock().await.remove(&JobId::from(id.clone()));
        let _ = tokio::fs::remove_file(dir.join(format!("{id}.log"))).await;
    }
}

#[cfg(test)]
mod tests {
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

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_group_terminates_a_detached_group() {
        use std::process::Stdio;
        // Leader of its own group (pgid == pid), like a real job's shell.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().expect("child pid");
        // Reap in the background so the signalled child leaves no zombie — mirrors
        // the real parent (server/init) reaping it, which is what `group_alive`
        // assumes.
        let waiter = tokio::spawn(async move { child.wait().await });

        assert!(
            kill_group(pid).await,
            "group should be gone after kill_group"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_group_on_dead_pgid_reports_gone() {
        // A pgid with no live members must read as already-gone, not hang.
        assert!(kill_group(2_000_000_000).await);
    }

    #[tokio::test]
    async fn reap_orphans_removes_aged_trim_temps_and_keeps_known_logs() {
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::memory();
        db.call(|conn| {
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

        reap_orphans(&db, dir.path(), RETENTION_SECS).await;

        assert!(
            !trim_tmp.exists(),
            "aged .log.trim temp must be swept — no reaper path deleted it before"
        );
        assert!(!orphan.exists(), "aged orphan log must be swept");
        assert!(known.exists(), "a log with a matching row must be kept");
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
