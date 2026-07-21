//! Process-group signalling: terminate a job's process group, `SIGTERM` then
//! `SIGKILL` on a grace timeout, and probe group liveness.
//!
//! A job leads its own process group (so its pgid equals its pid; see
//! `JobStore::run`), which lets a single signal to the negative pid reach the
//! whole tree the command spawned, not just `sh` itself.
//!
//! Two entry points, one semantics: [`kill_job`] for a job this process still
//! holds a handle to, [`kill_persisted`] for one it only has a row for — the
//! engine's untracked jobs and every `mcp-ssh job kill`. Both decide on the same
//! rules; only the answer's rendering differs per caller.
use std::time::Duration;

use tokio::sync::watch;

use super::{Job, JobId, JobRepo, JobState, JobStatus, ProcessGroupId};

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

/// What killing a job from its persisted row decided. The two callers render it
/// differently — `job(action="kill")` answers a bool, `mcp-ssh job kill` an
/// operator sentence — so the decision is returned here, never formatted.
#[derive(Debug)]
pub(crate) enum KillOutcome {
    /// No row with that id.
    Unknown,
    /// The row already holds a terminal state, so there is nothing to signal. The
    /// status word is kept verbatim: a corrupt one is shown, not hidden.
    NotRunning { status: String },
    /// The row records no process group — it predates pgid tracking, or the OS
    /// withheld the pid. Nothing to signal, and no pgid may be guessed.
    NoProcessGroup,
    /// The persisted pgid is one no real job could have written (outside `u32`,
    /// which a raw cast would wrap onto a real but wrong group, or `0`, which is
    /// *this* process's own group). Refused, not signalled.
    CorruptProcessGroup { pgid: i64 },
    /// The group is gone, and the row records the kill.
    Killed,
    /// The group outlived `TERM`→`KILL`, so the row is left `running` rather than
    /// claim a kill that didn't happen.
    Survived { pgid: ProcessGroupId },
}

/// Kill a job from its persisted row — the shared semantics for every caller that
/// holds no in-process [`Job`]: the engine, for a job whose group outlived the
/// process that started it, and the `mcp-ssh job kill` CLI, which never tracks
/// one. Each job leads its own process group, so a persisted pgid stays
/// signalable across a restart.
///
/// Only a row that still reads `running` is signalled, and only through
/// [`ProcessGroupId::from_persisted`], the single gate that keeps a corrupt pgid
/// from becoming a signal target. The `failed` transition is recorded — with
/// `reason` in the `error` column — only once the group is actually gone, and
/// only while the row still reads `running`: a real exit written meanwhile by the
/// server that owns the job wins over ours. That write is best effort, since the
/// kill has already happened by then — a failure is logged, not returned.
pub(crate) async fn kill_persisted(
    repo: &JobRepo,
    id: &JobId,
    reason: &'static str,
) -> rusqlite::Result<KillOutcome> {
    let Some(target) = repo.kill_target(id).await? else {
        return Ok(KillOutcome::Unknown);
    };
    if target.status != JobStatus::Running.as_str() {
        return Ok(KillOutcome::NotRunning {
            status: target.status,
        });
    }
    let Some(pgid) = target.pgid else {
        return Ok(KillOutcome::NoProcessGroup);
    };
    let Some(group) = ProcessGroupId::from_persisted(pgid) else {
        return Ok(KillOutcome::CorruptProcessGroup { pgid });
    };
    if !kill_group(group).await {
        return Ok(KillOutcome::Survived { pgid: group });
    }
    if let Err(error) = repo.mark_killed(id, reason).await {
        tracing::warn!(%error, id = %id, "failed to record killed job");
    }
    Ok(KillOutcome::Killed)
}

/// Kill a process group by raw pgid. `SIGTERM`, then `SIGKILL` if the group
/// outlives a short grace. Returns whether the group is gone afterwards. Liveness
/// is probed with `kill -0` rather than a completion flag — the group's real
/// parent (the server, or init after a restart) reaps the exited process, so no
/// zombie lingers to read as alive.
pub(super) async fn kill_group(pgid: ProcessGroupId) -> bool {
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
    use crate::db::Db;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// A repository over a fresh in-memory DB, plus the raw handle the tests use to
    /// plant rows the typed API can't produce (legacy or corrupt ones) and to read
    /// columns back.
    fn repo() -> (JobRepo, Db) {
        let db = Db::memory();
        (JobRepo::new(db.clone()), db)
    }

    /// Plant one row verbatim — `status`/`pgid` combinations `insert_running` can't
    /// write, which is exactly what the refusals guard against.
    async fn plant(db: &Db, id: &'static str, status: &'static str, pgid: Option<i64>) {
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix, pgid) VALUES (?1, ?2, 1, ?3)",
                rusqlite::params![id, status, pgid],
            )
        })
        .await
        .unwrap();
    }

    async fn status_of(db: &Db, id: &'static str) -> Option<String> {
        use rusqlite::OptionalExtension;
        db.call(move |conn| {
            conn.query_row("SELECT status FROM jobs WHERE id = ?1", [id], |r| r.get(0))
                .optional()
        })
        .await
        .unwrap()
    }

    /// The kill under test, with the engine's reason unless the test cares.
    async fn kill(repo: &JobRepo, id: &str) -> KillOutcome {
        kill_persisted(repo, &JobId::from(id), "killed")
            .await
            .unwrap()
    }

    /// The decision table both kill callers now share: every row that must not be
    /// signalled is refused with its own reason and left exactly as it was.
    #[tokio::test]
    async fn unsignalable_rows_are_refused_and_left_untouched() {
        let (repo, db) = repo();
        plant(&db, "done", "exited", Some(1234)).await;
        plant(&db, "legacy", "running", None).await;
        plant(&db, "zero", "running", Some(0)).await;
        plant(&db, "wrapped", "running", Some(i64::from(u32::MAX) + 1)).await;

        assert!(matches!(kill(&repo, "ghost").await, KillOutcome::Unknown));
        assert!(
            matches!(kill(&repo, "done").await, KillOutcome::NotRunning { status } if status == "exited"),
            "a finished row is never signalled"
        );
        assert!(matches!(
            kill(&repo, "legacy").await,
            KillOutcome::NoProcessGroup
        ));
        // `0` is the dangerous one: `kill -- -0` would signal *this* process's own
        // group, so this test is its own canary — a regression kills the runner.
        assert!(matches!(
            kill(&repo, "zero").await,
            KillOutcome::CorruptProcessGroup { pgid: 0 }
        ));
        assert!(matches!(
            kill(&repo, "wrapped").await,
            KillOutcome::CorruptProcessGroup { .. }
        ));

        for id in ["legacy", "zero", "wrapped"] {
            assert_eq!(
                status_of(&db, id).await.as_deref(),
                Some("running"),
                "a refused kill must not record a kill that never happened: {id}"
            );
        }
        assert_eq!(status_of(&db, "done").await.as_deref(), Some("exited"));
    }

    /// The recorded transition carries the caller's reason verbatim — that column is
    /// how `mcp-ssh jobs` tells a CLI kill from the server's own.
    #[tokio::test]
    async fn a_gone_group_is_recorded_failed_with_the_caller_s_reason() {
        let (repo, db) = repo();
        // A pgid with no live members: `kill_group` reports it already gone, which is
        // the same "the group is dead" answer a real signal ends at.
        plant(&db, "gone", "running", Some(2_000_000_000)).await;

        let outcome = kill_persisted(&repo, &JobId::from("gone"), "killed via mcp-ssh kill")
            .await
            .unwrap();
        assert!(matches!(outcome, KillOutcome::Killed));

        let row: (String, Option<String>) = db
            .call(|conn| {
                conn.query_row(
                    "SELECT status, error FROM jobs WHERE id = 'gone'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(row.0, "failed");
        assert_eq!(row.1.as_deref(), Some("killed via mcp-ssh kill"));
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
