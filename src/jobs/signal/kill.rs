//! The one kill semantics: what killing a job means, for a job this process
//! tracks ([`kill_job`]) and for one it only has a row for ([`kill_persisted`]).
use std::io;

use super::{Escalation, Kill, Probe, escalate, persisted_group, probe};
use crate::jobs::{Job, JobId, JobRepo, JobState, JobStatus, ProcessGroupId};

/// What a kill decided. Rendered by its callers — `job(action="kill")` and
/// `mcp-ssh job kill` — never formatted here.
#[derive(Debug)]
pub(crate) enum KillOutcome {
    /// No job with that id.
    Unknown,
    /// The job already finished and nothing of its group is left. The status word
    /// is kept verbatim.
    NotRunning { status: String },
    /// No process group is recorded (the OS withheld the pid, or a row predating
    /// pgid tracking). Nothing to signal, and no pgid may be guessed.
    NoProcessGroup,
    /// The persisted pgid is one no real job could have written (outside
    /// `2..=i32::MAX`; `0` and `-1` would hit this process's group or everything we
    /// may signal). Refused, not signalled.
    CorruptProcessGroup { pgid: i64 },
    /// The job was running and its group is gone; recorded `failed`.
    Killed,
    /// The job had finished, but processes it left behind (`server &`) were still
    /// in its group; they are gone now.
    KilledLeftovers,
    /// The group outlived `TERM`→`KILL`; nothing is recorded.
    Survived { pgid: ProcessGroupId },
    /// The group could not be probed or signalled (an unexpected errno): no claim
    /// either way, nothing recorded.
    NotSignalled { error: io::Error },
}

/// Kill a job this process tracks. A running job's group is escalated (its
/// `reason` becomes the job's final error once the waiter sees it die — see
/// [`blamed`]); a finished one is probed for leftovers and those are killed.
pub(in crate::jobs) async fn kill_job(job: &Job, reason: &'static str) -> KillOutcome {
    let state = job.state.lock().await.clone();
    let Some(pgid) = job.pgid else {
        return match state {
            JobState::Running => KillOutcome::NoProcessGroup,
            finished => KillOutcome::NotRunning {
                status: status_word(&finished).to_string(),
            },
        };
    };
    let how = how_to_kill(job.interactive);
    match state {
        JobState::Running => running_outcome(blamed(job, reason, escalate(pgid, how)).await, pgid),
        finished => leftovers(pgid, how, status_word(&finished)).await,
    }
}

/// Run `escalation` with `reason` standing as the job's cause of death, for the
/// waiter to record when the leader dies under it. If the group outlives the
/// kill, the reason is withdrawn: whatever ends the job later was not this kill,
/// and must be recorded as what it was (an exit code, another signal).
pub(super) async fn blamed(
    job: &Job,
    reason: &'static str,
    escalation: impl Future<Output = Escalation>,
) -> Escalation {
    let claimed = {
        let mut cause = job.kill_reason.lock().await;
        // First reason wins: a cap breach already recorded stays the cause.
        let free = cause.is_none();
        if free {
            *cause = Some(reason);
        }
        free
    };
    let escalation = escalation.await;
    if claimed && !matches!(escalation, Escalation::Gone) {
        *job.kill_reason.lock().await = None;
    }
    escalation
}

fn how_to_kill(interactive: bool) -> Kill {
    if interactive {
        Kill::GracefulStopLeader
    } else {
        Kill::Graceful
    }
}

/// Kill a job from its persisted row — for every caller that holds no in-process
/// [`Job`]. Only [`persisted_group`] turns a row into a signal target, and the row
/// says whether the job is interactive, so it is escalated as [`kill_job`] would. The
/// `failed` transition (with `reason` in the `error` column) is written only once
/// the group is gone and only while the row still reads `running`, so a real exit
/// recorded meanwhile wins. That write is best effort — the kill has happened.
pub(crate) async fn kill_persisted(
    repo: &JobRepo,
    id: &JobId,
    reason: &'static str,
) -> rusqlite::Result<KillOutcome> {
    let Some(target) = repo.kill_target(id).await? else {
        return Ok(KillOutcome::Unknown);
    };
    let running = target.status == JobStatus::Running.as_str();
    let Some(raw) = target.pgid else {
        return Ok(if running {
            KillOutcome::NoProcessGroup
        } else {
            KillOutcome::NotRunning {
                status: target.status,
            }
        });
    };
    if ProcessGroupId::from_persisted(raw).is_none() {
        return Ok(if running {
            KillOutcome::CorruptProcessGroup { pgid: raw }
        } else {
            KillOutcome::NotRunning {
                status: target.status,
            }
        });
    }
    let group = persisted_group(Some(raw), target.boot_id.as_deref());
    let how = how_to_kill(target.interactive);
    if !running {
        return Ok(match group {
            Some(pgid) => leftovers(pgid, how, &target.status).await,
            None => KillOutcome::NotRunning {
                status: target.status,
            },
        });
    }
    // A running row from a previous boot: its group cannot exist any more.
    let outcome = match group {
        Some(pgid) => running_outcome(escalate(pgid, how).await, pgid),
        None => KillOutcome::Killed,
    };
    if matches!(outcome, KillOutcome::Killed)
        && let Err(error) = repo.mark_killed(id, reason).await
    {
        tracing::warn!(%error, id = %id, "failed to record killed job");
    }
    Ok(outcome)
}

fn running_outcome(escalation: Escalation, pgid: ProcessGroupId) -> KillOutcome {
    match escalation {
        Escalation::Gone => KillOutcome::Killed,
        Escalation::Survived => KillOutcome::Survived { pgid },
        Escalation::Unknown(error) => KillOutcome::NotSignalled { error },
    }
}

/// A finished job: kill whatever its group still holds.
async fn leftovers(pgid: ProcessGroupId, how: Kill, status: &str) -> KillOutcome {
    match probe(pgid) {
        Probe::Gone => KillOutcome::NotRunning {
            status: status.to_string(),
        },
        Probe::Unknown(error) => KillOutcome::NotSignalled { error },
        Probe::Alive => match escalate(pgid, how).await {
            Escalation::Gone => KillOutcome::KilledLeftovers,
            Escalation::Survived => KillOutcome::Survived { pgid },
            Escalation::Unknown(error) => KillOutcome::NotSignalled { error },
        },
    }
}

fn status_word(state: &JobState) -> &'static str {
    match state {
        JobState::Running => JobStatus::Running.as_str(),
        JobState::Exited { .. } => JobStatus::Exited.as_str(),
        JobState::Failed { .. } => JobStatus::Failed.as_str(),
    }
}
