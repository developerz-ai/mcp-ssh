//! Reaper + kill signalling: evict aged-out jobs and terminate process groups.
//!
//! A job leads its own process group (so its pgid equals its pid; see
//! `JobStore::run`), which lets a single signal to the negative pid reach the
//! whole tree the command spawned, not just `sh` itself.
use std::path::Path;
use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::{Mutex, watch};

use super::{Job, JobId, JobState, ProcessGroupId};

/// Jobs (and their logs) older than this are reaped hourly.
const RETENTION: Duration = Duration::from_secs(24 * 3600);
/// Grace between `SIGTERM` and `SIGKILL` when killing a job's process group.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// This server is meant to run for weeks. Job logs can't grow without bound, so
/// the hourly pass compacts the logs of *finished* jobs to a trailing tail —
/// enough to debug a failure, not enough to fill the disk. Running jobs are never
/// compacted (their log is still being appended). Tiers, by how long ago the job
/// started: a finished job keeps its last `TRIM_RECENT_LINES` while fresh, drops
/// to `TRIM_AGED_LINES` after `TRIM_AGED_AFTER`, then is purged at `RETENTION`.
const TRIM_AGED_AFTER: Duration = Duration::from_secs(3 * 3600);
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
        return false;
    }
    true
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

/// Hourly: drop jobs (and their log files) older than `RETENTION` so history
/// doesn't grow without bound. ponytail: time-based only; a busy box could still
/// hold ≤24h of jobs in memory — add a count cap if that ever bites.
pub(super) fn spawn_reaper(jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            // Purge aged-out jobs first, then compact the logs of whatever
            // finished jobs remain so disk stays bounded over a long uptime.
            reap_once(&jobs, RETENTION).await;
            compact_once(&jobs, TRIM_AGED_AFTER).await;
        }
    });
}

/// Compact every *finished* job's log to a trailing tail: `TRIM_RECENT_LINES`
/// while younger than `aged_after`, `TRIM_AGED_LINES` once older. Running jobs are
/// skipped — their log is still being written, and truncating under the writer
/// would corrupt it. Idempotent: an already-trimmed log is left alone.
pub(super) async fn compact_once(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, aged_after: Duration) {
    let now = tokio::time::Instant::now();
    let snapshot: Vec<Arc<Job>> = {
        let map = jobs.lock().await;
        map.values().cloned().collect()
    };
    for job in snapshot {
        // Never rewrite a log that's still being appended to.
        if matches!(*job.state.lock().await, JobState::Running) {
            continue;
        }
        let keep = if now.duration_since(job.started) >= aged_after {
            TRIM_AGED_LINES
        } else {
            TRIM_RECENT_LINES
        };
        if let Err(error) = trim_log(&job.log_path, keep).await {
            tracing::warn!(%error, path = %job.log_path.display(), "failed to trim job log");
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

/// One reaping pass: evict every job older than `retention`. A still-`Running`
/// job is killed first, so eviction never orphans its process group.
pub(super) async fn reap_once(jobs: &Mutex<HashMap<JobId, Arc<Job>>>, retention: Duration) {
    let now = tokio::time::Instant::now();
    let map = jobs.lock().await;
    let stale: Vec<(JobId, Arc<Job>)> = map
        .iter()
        .filter(|(_, j)| now.duration_since(j.started) > retention)
        .map(|(id, j)| (id.clone(), j.clone()))
        .collect();
    drop(map);

    // Kill first so a still-running group is never orphaned by eviction. Only
    // evict jobs that finished or whose group we actually signalled; a running
    // job whose kill failed stays tracked (pollable/killable) for a later pass.
    let mut removable: Vec<(JobId, Arc<Job>)> = Vec::new();
    for (id, job) in &stale {
        if kill_job(job).await || !matches!(*job.state.lock().await, JobState::Running) {
            removable.push((id.clone(), job.clone()));
        } else {
            tracing::warn!(id = %id, "stale running job not evicted: kill failed");
        }
    }

    let mut map = jobs.lock().await;
    for (id, _) in &removable {
        map.remove(id);
    }
    drop(map);

    for (_, job) in &removable {
        let _ = tokio::fs::remove_file(&job.log_path).await;
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
