//! The supervisor: owns a launched job from spawn to its recorded final state,
//! enforcing the live log cap on the way — and after it, for as long as the job's
//! group still holds a process that can write to the log.
use std::{collections::HashMap, os::unix::process::ExitStatusExt, sync::Arc, time::Duration};

use tokio::sync::{Mutex, oneshot, watch};

use super::signal::{self, Escalation, Kill, Probe, escalate, probe};
use super::{Job, JobError, JobId, JobRepo, JobState, ProcessGroupId, store};

/// Most bytes a job's live log may reach. Past it the job's group is killed and it
/// finishes `failed` — a runaway writer must not fill the volume the database
/// shares (one was measured at 150 GB a minute).
pub(super) const LOG_CAP: u64 = 256 * 1024 * 1024;
pub(super) const LOG_CAP_REASON: &str = "output exceeded the 256 MiB log cap — killed";
/// The same breach by what a finished job left running (`server &`).
pub(super) const LEFTOVERS_LOG_CAP_REASON: &str =
    "output of processes it left running exceeded the 256 MiB log cap — killed";
/// How often a job's log size is checked against the cap.
const LOG_CAP_EVERY: Duration = Duration::from_millis(100);

/// What the supervisor needs to own a launched job.
pub(super) struct Supervised {
    pub(super) id: JobId,
    pub(super) job: Arc<Job>,
    pub(super) label: Option<String>,
    pub(super) log_cap: u64,
    pub(super) done: watch::Sender<bool>,
}

/// Own a launched job to the end: track it, persist its row (reporting the
/// outcome to `run` through `started`), enforce the live log cap, reap the child,
/// and record its final state — then keep the cap on whatever the job left
/// running. Runs as its own task, so it finishes even when the request that
/// launched it is cancelled.
///
/// The map insert precedes the row insert: the reaper's reconcile snapshots the
/// map under its lock before querying `running` rows, so a row this process
/// creates is either in that snapshot or written after the query.
pub(super) async fn supervise(
    s: Supervised,
    mut child: tokio::process::Child,
    jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>,
    repo: JobRepo,
    started: oneshot::Sender<Result<(), JobError>>,
) {
    let Supervised {
        id,
        job,
        label,
        log_cap,
        done,
    } = s;
    jobs.lock().await.insert(id.clone(), job.clone());
    let inserted = repo
        .insert_running(
            &id,
            label,
            crate::db::now_unix(),
            job.pgid,
            signal::boot_id(),
            job.interactive,
        )
        .await;
    if let Err(error) = inserted {
        // A job with no row is invisible to `list`, never evicted, and its finish
        // updates nothing — refuse it rather than run it untracked.
        tracing::warn!(%error, id = %id, "failed to persist job row — stopping the job");
        match job.pgid {
            Some(pgid) => {
                let _ = escalate(pgid, Kill::Immediate).await;
            }
            None => {
                let _ = child.start_kill();
            }
        }
        let _ = child.wait().await;
        jobs.lock().await.remove(&id);
        let _ = tokio::fs::remove_file(&job.log_path).await;
        let _ = started.send(Err(JobError::Store(error)));
        return;
    }
    let _ = started.send(Ok(()));

    let (exit, capped) = tokio::select! {
        exit = child.wait() => (exit, false),
        () = enforce_log_cap(&job, log_cap) => (child.wait().await, true),
    };
    let kill_reason = *job.kill_reason.lock().await;
    let result = final_state(kill_reason, exit);
    *job.state.lock().await = result.clone();
    // Wake the inline waiter BEFORE the DB write: a slow DB must never delay the
    // inline window (which would misreport a fast command as backgrounded).
    let _ = done.send(true);
    repo.finish(&id, &job.log_path, &result).await;
    // A group already killed for the cap gets no second kill.
    if !capped {
        cap_leftovers(&id, &job, log_cap, &repo).await;
    }
}

/// Resolve once the job's live log passes `cap`, after killing its group at once.
/// Never resolves for a job with no group to kill.
async fn enforce_log_cap(job: &Job, cap: u64) {
    let Some(pgid) = job.pgid else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(LOG_CAP_EVERY).await;
        if over_cap(job, cap).await {
            // First reason wins: a kill already under way stays the cause.
            job.kill_reason.lock().await.get_or_insert(LOG_CAP_REASON);
            let _ = kill_over_cap(pgid).await;
            return;
        }
    }
}

/// The leader has exited, but processes it left in its group (`server &`) still
/// hold the log open — so the cap is kept on them until the group is gone. Past
/// it they are killed like a running job, and the job is re-recorded `failed`
/// with the reason: nothing else would say where they went.
///
/// This is the supervisor's kill, not the reaper's (which never kills), and it
/// ends with this process: leftovers that outlive a restart are not watched.
async fn cap_leftovers(id: &JobId, job: &Job, cap: u64, repo: &JobRepo) {
    let Some(pgid) = job.pgid else {
        return;
    };
    loop {
        // The usual case, on the first pass: the job left nothing behind.
        if matches!(probe(pgid), Probe::Gone) {
            return;
        }
        if over_cap(job, cap).await {
            break;
        }
        tokio::time::sleep(LOG_CAP_EVERY).await;
    }
    // One attempt: a group `SIGKILL` could not end is not signalled in a loop.
    if !matches!(kill_over_cap(pgid).await, Escalation::Gone) {
        return;
    }
    let failed = JobState::Failed {
        error: LEFTOVERS_LOG_CAP_REASON.to_string(),
    };
    *job.state.lock().await = failed;
    if let Err(error) = repo
        .mark_leftovers_killed(id, LEFTOVERS_LOG_CAP_REASON)
        .await
    {
        tracing::warn!(%error, id = %id, "failed to record the leftovers' kill");
    }
}

/// Whether the job's log has grown past `cap`. A log that can't be read (reaped,
/// say) is not over it.
async fn over_cap(job: &Job, cap: u64) -> bool {
    tokio::fs::metadata(&job.log_path)
        .await
        .is_ok_and(|meta| meta.len() > cap)
}

async fn kill_over_cap(pgid: ProcessGroupId) -> Escalation {
    tracing::warn!(
        pgid = pgid.get(),
        "job output passed the log cap — killing its group"
    );
    escalate(pgid, Kill::Immediate).await
}

/// A finished child's state. A kill's recorded reason wins; otherwise an exit
/// code, or the signal that ended it — never a made-up code for a signal death.
fn final_state(
    kill_reason: Option<&'static str>,
    exit: std::io::Result<std::process::ExitStatus>,
) -> JobState {
    let status = match exit {
        Ok(status) => status,
        Err(e) => {
            return JobState::Failed {
                error: e.to_string(),
            };
        }
    };
    if let Some(reason) = kill_reason {
        return JobState::Failed {
            error: reason.to_string(),
        };
    }
    match (status.code(), status.signal()) {
        (Some(code), _) => JobState::Exited { code },
        (None, Some(signal)) => JobState::Failed {
            error: store::signal_death(signal),
        },
        (None, None) => JobState::Failed {
            error: "exited without a status".to_string(),
        },
    }
}
