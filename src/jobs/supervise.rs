//! The supervisor: owns a launched job from spawn to its recorded final state,
//! enforcing the live log cap on the way.
use std::{collections::HashMap, os::unix::process::ExitStatusExt, sync::Arc, time::Duration};

use tokio::sync::{Mutex, oneshot, watch};

use super::signal::{self, Kill, escalate};
use super::{Job, JobError, JobId, JobRepo, JobState, store};

/// Most bytes a job's live log may reach. Past it the job's group is killed and it
/// finishes `failed` — a runaway writer must not fill the volume the database
/// shares (one was measured at 150 GB a minute).
pub(super) const LOG_CAP: u64 = 256 * 1024 * 1024;
pub(super) const LOG_CAP_REASON: &str = "output exceeded the 256 MiB log cap — killed";
/// How often a running job's log size is checked against the cap.
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
/// and record its final state. Runs as its own task, so it finishes even when the
/// request that launched it is cancelled.
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

    let exit = tokio::select! {
        exit = child.wait() => exit,
        () = enforce_log_cap(&job, log_cap) => child.wait().await,
    };
    let result = final_state(&job, exit);
    *job.state.lock().await = result.clone();
    // Wake the inline waiter BEFORE the DB write: a slow DB must never delay the
    // inline window (which would misreport a fast command as backgrounded).
    let _ = done.send(true);
    repo.finish(&id, &job.log_path, &result).await;
}

/// Resolve once the job's live log passes `cap`, after killing its group at once.
/// Never resolves for a job with no group to kill.
async fn enforce_log_cap(job: &Job, cap: u64) {
    let Some(pgid) = job.pgid else {
        return std::future::pending().await;
    };
    loop {
        tokio::time::sleep(LOG_CAP_EVERY).await;
        let size = match tokio::fs::metadata(&job.log_path).await {
            Ok(meta) => meta.len(),
            Err(_) => continue,
        };
        if size > cap {
            let _ = job.kill_reason.set(LOG_CAP_REASON.to_string());
            tracing::warn!(
                pgid = pgid.get(),
                size,
                "job output passed the log cap — killing it"
            );
            let _ = escalate(pgid, Kill::Immediate).await;
            return;
        }
    }
}

/// A finished child's state. A kill's recorded reason wins; otherwise an exit
/// code, or the signal that ended it — never a made-up code for a signal death.
fn final_state(job: &Job, exit: std::io::Result<std::process::ExitStatus>) -> JobState {
    let status = match exit {
        Ok(status) => status,
        Err(e) => {
            return JobState::Failed {
                error: e.to_string(),
            };
        }
    };
    if let Some(reason) = job.kill_reason.get() {
        return JobState::Failed {
            error: reason.clone(),
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
