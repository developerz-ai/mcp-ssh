//! Process-group signalling: terminate a job's process group, `SIGTERM` then
//! `SIGKILL` on a grace timeout, and probe group liveness.
//!
//! A job leads its own process group (so its pgid equals its pid; see
//! `JobStore::run`), which lets a single signal to the negative pid reach the
//! whole tree the command spawned, not just `sh` itself.
use std::time::Duration;

use tokio::sync::watch;

use super::{Job, JobState, ProcessGroupId};

/// Grace between `SIGTERM` and `SIGKILL` when killing a job's process group.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Signal a job's process group dead: `SIGTERM`, then `SIGKILL` if it outlasts a
/// short grace. Returns `true` if the group is gone afterwards, `false` if there
/// was nothing to kill (the store already recorded the exit, the OS withheld its
/// pid) or the group outlived the signals.
pub(super) async fn kill_job(job: &Job) -> bool {
    if !matches!(*job.state.lock().await, JobState::Running) {
        return false;
    }
    let Some(pgid) = job.pgid else {
        return false;
    };
    if !signal_group(pgid, "TERM").await {
        // `TERM` only fails once the group is gone, so the job exited between the
        // state check and the signal. Probe the group rather than re-reading the
        // state — the waiter task reaps the child before it records the exit, so
        // the state still reads `Running` for that window — and report a group
        // that is already gone as killed, not as a kill failure.
        return !group_alive(pgid).await;
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
pub(crate) async fn kill_group(pgid: ProcessGroupId) -> bool {
    if !group_alive(pgid).await {
        return true; // nothing to signal — already gone
    }
    let _ = signal_group(pgid, "TERM").await;
    let start = tokio::time::Instant::now();
    while start.elapsed() < KILL_GRACE {
        if !group_alive(pgid).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if group_alive(pgid).await {
        let _ = signal_group(pgid, "KILL").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    !group_alive(pgid).await
}

/// True if process group `pgid` still has at least one member. `kill -0` delivers
/// no signal, only checks deliverability; stdio is discarded so a "No such
/// process" line never reaches the terminal. Shared with the startup reconcile in
/// `super` and the reaper's log-compaction gate, which both need the same "did
/// this group outlive the restart?" answer.
pub(super) async fn group_alive(pgid: ProcessGroupId) -> bool {
    tokio::process::Command::new("kill")
        .arg("-0")
        .arg("--")
        .arg(format!("-{}", pgid.get()))
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
        .arg(format!("-{}", pgid.get()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    {
        Ok(status) => status.success(),
        Err(error) => {
            tracing::warn!(%error, pgid = pgid.get(), signal, "failed to signal process group");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

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
        let pid = ProcessGroupId::new(child.id().expect("child pid")).expect("nonzero pid");
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
        let dead = ProcessGroupId::new(2_000_000_000).expect("nonzero pgid");
        assert!(kill_group(dead).await);
    }

    /// The invariant the type carries: no corrupt persisted pgid can become a
    /// signal target. `0` is the dangerous one — `kill -- -0` hits *this* process's
    /// group, i.e. the server — so it must never survive into a `ProcessGroupId`.
    #[test]
    fn corrupt_persisted_pgids_never_become_a_signal_target() {
        for corrupt in [0, -1, i64::from(u32::MAX) + 1] {
            assert!(
                ProcessGroupId::from_persisted(corrupt).is_none(),
                "pgid {corrupt} must not be signalable"
            );
        }
        assert!(ProcessGroupId::from_persisted(1234).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_job_reports_a_group_gone_before_term_as_killed() {
        use std::process::Stdio;
        // The race `kill_job` guards: the group exits between the state check and
        // the `TERM`. Reproduced deterministically by killing and reaping the group
        // first, then killing a job whose state still reads `Running` — exactly the
        // window between the waiter reaping the child and recording its exit.
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
        let waiter = tokio::spawn(async move { child.wait().await });
        assert!(kill_group(pgid).await, "group should be gone after kill");
        let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;

        let (_tx, rx) = watch::channel(false);
        let job = Arc::new(Job {
            pgid: Some(pgid),
            state: Arc::new(Mutex::new(JobState::Running)),
            done: rx,
            log_path: PathBuf::from("unused.log"),
        });

        assert!(
            kill_job(&job).await,
            "a group already gone must report killed, not a kill failure"
        );
    }
}
