//! Reaper: reconcile, age out, and compact jobs and their logs.
//!
//! Runs once at startup (which makes its first pass the startup reconcile) and
//! hourly after. It never kills anything: a running job is never aged out, and a
//! finished job whose group still holds a process (`server &`) keeps its row and
//! log until that process is gone. Job ages come from the DB (`finished_unix`,
//! wall clock), so retention stays meaningful across restarts.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;

use super::signal::{Probe, probe_persisted};
use super::store::ReapRow;
use super::{Job, JobId, JobRepo, JobState};
use crate::db::now_unix;

mod compact;

use compact::{TRIM_AGED_AFTER_SECS, compact_once};

/// A finished job (row + log) is dropped this long after it finished. Seconds,
/// to compare against the DB's wall-clock `finished_unix`.
const RETENTION_SECS: i64 = 24 * 3600;

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
mod tests;
