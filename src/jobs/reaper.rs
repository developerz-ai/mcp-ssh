//! Reaper + kill signalling: evict aged-out jobs and terminate process groups.
//!
//! A job leads its own process group (so its pgid equals its pid; see
//! `JobStore::run`), which lets a single signal to the negative pid reach the
//! whole tree the command spawned, not just `sh` itself.
use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::sync::{Mutex, watch};

use super::{Job, JobState, ProcessGroupId};

/// Jobs (and their logs) older than this are reaped hourly.
const RETENTION: Duration = Duration::from_secs(24 * 3600);
/// Grace between `SIGTERM` and `SIGKILL` when killing a job's process group.
const KILL_GRACE: Duration = Duration::from_secs(2);

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
pub(super) fn spawn_reaper(jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            reap_once(&jobs, RETENTION).await;
        }
    });
}

/// One reaping pass: evict every job older than `retention`. A still-`Running`
/// job is killed first, so eviction never orphans its process group.
pub(super) async fn reap_once(jobs: &Mutex<HashMap<String, Arc<Job>>>, retention: Duration) {
    let now = tokio::time::Instant::now();
    let map = jobs.lock().await;
    let stale: Vec<(String, Arc<Job>)> = map
        .iter()
        .filter(|(_, j)| now.duration_since(j.started) > retention)
        .map(|(id, j)| (id.clone(), j.clone()))
        .collect();
    drop(map);

    // Kill first so a still-running group is never orphaned by eviction; the job
    // stays in the map (pollable/killable) until its termination completes.
    for (_, job) in &stale {
        kill_job(job).await;
    }

    let mut map = jobs.lock().await;
    for (id, _) in &stale {
        map.remove(id);
    }
    drop(map);

    for (_, job) in &stale {
        let _ = tokio::fs::remove_file(&job.log_path).await;
    }
}
