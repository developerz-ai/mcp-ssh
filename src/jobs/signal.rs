//! Process-group signalling and the one kill semantics.
//!
//! A job leads its own process group (so its pgid equals its pid; see
//! `JobStore::run`), which lets one `kill(2)` on the negative pid reach the whole
//! tree the command spawned, not just its shell.
//!
//! Two entry points, one decision: [`kill_job`] for a job this process still holds
//! a handle to, [`kill_persisted`] for one it only has a row for (the engine's
//! untracked jobs and every `mcp-ssh job kill`). Both end in [`escalate`], and a
//! kill is reported as done only once a probe finds the group gone — never from
//! the leader's exit alone (descendants can outlive it) and never from an error.
use std::io;
use std::sync::OnceLock;
use std::time::Duration;

use super::{Job, JobId, JobRepo, JobState, JobStatus, ProcessGroupId};

/// Grace between `SIGTERM` and `SIGKILL` when killing a job's process group.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long a group may take to disappear after `SIGKILL` (its members must be
/// reaped by their parent) before it is reported as having survived.
const REAP_WAIT: Duration = Duration::from_secs(1);
const PROBE_EVERY: Duration = Duration::from_millis(50);

/// What `kill(2)` says about a target. An error is never read as "gone": a
/// failed probe keeps a job `running` rather than mark it dead.
#[derive(Debug)]
pub(super) enum Probe {
    /// The target exists (signal delivered, or `EPERM`: it exists but isn't ours).
    Alive,
    /// `ESRCH`: no such process or group.
    Gone,
    /// Any other failure — the answer is unknown.
    Unknown(io::Error),
}

/// Send `sig` to `target` (a pid, or a negated pgid for the whole group) and map
/// the errno. `target` is never `0` or `-1` (the caller's own group / every
/// process we may signal): every caller passes a `ProcessGroupId`, which holds
/// `2..=i32::MAX`.
pub(super) fn send(target: libc::pid_t, sig: libc::c_int) -> Probe {
    // SAFETY: kill(2) takes two plain integers and touches no memory of ours; every
    // argument value is defined behaviour (a bad one only fails with an errno).
    let rc = unsafe { libc::kill(target, sig) };
    if rc == 0 {
        return Probe::Alive;
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Probe::Gone,
        Some(libc::EPERM) => Probe::Alive,
        _ => Probe::Unknown(error),
    }
}

/// Signal every member of the group.
fn signal_group(pgid: ProcessGroupId, sig: libc::c_int) -> Probe {
    send(-pgid.get(), sig)
}

/// Whether process group `pgid` still has a member. Signal 0 delivers nothing,
/// only checks deliverability.
pub(super) fn probe(pgid: ProcessGroupId) -> Probe {
    signal_group(pgid, 0)
}

/// The kernel's id for this boot. A pgid persisted under another boot names
/// nothing that still exists — the number may since have been handed to an
/// unrelated group — so such a row is treated as gone without being probed.
/// `None` where the kernel doesn't expose it (then every row is probed).
fn current_boot_id() -> Option<&'static str> {
    static BOOT_ID: OnceLock<Option<String>> = OnceLock::new();
    BOOT_ID
        .get_or_init(|| {
            // procfs is memory-backed and this runs once per process.
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .as_deref()
}

/// The boot id persisted with a new job row.
pub(super) fn boot_id() -> Option<String> {
    current_boot_id().map(str::to_string)
}

/// The signalable group behind a persisted row, or `None` when there is nothing
/// that may be signalled: no pgid, a corrupt one, or one from a previous boot.
pub(super) fn persisted_group(pgid: Option<i64>, boot: Option<&str>) -> Option<ProcessGroupId> {
    let pgid = pgid.and_then(ProcessGroupId::from_persisted)?;
    match (boot, current_boot_id()) {
        (Some(row), Some(now)) if row != now => None,
        _ => Some(pgid),
    }
}

/// Liveness of a persisted row's group, for the reconcile and the reaper: a row
/// with nothing signalable ([`persisted_group`] is `None`) reads as gone.
pub(super) fn probe_persisted(pgid: Option<i64>, boot: Option<&str>) -> Probe {
    match persisted_group(pgid, boot) {
        Some(pgid) => probe(pgid),
        None => Probe::Gone,
    }
}

/// How a group is brought down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kill {
    /// `SIGTERM`, a grace period, then `SIGKILL`.
    Graceful,
    /// As `Graceful`, but the leader is `SIGSTOP`ped first. An interactive bash
    /// ignores `SIGTERM`: with its foreground child killed it would go on to run
    /// the *next* command of the job during the grace. Stopped, it can't.
    GracefulStopLeader,
    /// `SIGKILL` at once — for a runaway job (output past the live log cap) whose
    /// every further millisecond is more disk.
    Immediate,
}

/// The end state of an [`escalate`].
#[derive(Debug)]
pub(super) enum Escalation {
    Gone,
    Survived,
    Unknown(io::Error),
}

/// Bring a process group down, and report what a final probe sees. Success is the
/// group gone, never "the leader exited".
pub(super) async fn escalate(pgid: ProcessGroupId, how: Kill) -> Escalation {
    match probe(pgid) {
        Probe::Gone => return Escalation::Gone,
        Probe::Unknown(error) => return Escalation::Unknown(error),
        Probe::Alive => {}
    }
    if how == Kill::GracefulStopLeader {
        // Best effort: a leader already gone leaves only members to TERM.
        let _ = send(pgid.get(), libc::SIGSTOP);
    }
    if how != Kill::Immediate {
        match signal_group(pgid, libc::SIGTERM) {
            Probe::Gone => return Escalation::Gone,
            Probe::Unknown(error) => return Escalation::Unknown(error),
            Probe::Alive => {}
        }
        if gone_within(pgid, KILL_GRACE).await {
            return Escalation::Gone;
        }
    }
    match signal_group(pgid, libc::SIGKILL) {
        Probe::Gone => return Escalation::Gone,
        Probe::Unknown(error) => return Escalation::Unknown(error),
        Probe::Alive => {}
    }
    if gone_within(pgid, REAP_WAIT).await {
        return Escalation::Gone;
    }
    match probe(pgid) {
        Probe::Gone => Escalation::Gone,
        Probe::Alive => Escalation::Survived,
        Probe::Unknown(error) => Escalation::Unknown(error),
    }
}

/// Poll until the group is gone or `within` elapses. An unknown probe is not gone.
async fn gone_within(pgid: ProcessGroupId, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if matches!(probe(pgid), Probe::Gone) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(PROBE_EVERY).await;
    }
}

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
/// `reason` becomes the job's final error once the waiter sees it die); a finished
/// one is probed for leftovers and those are killed.
pub(super) async fn kill_job(job: &Job, reason: &'static str) -> KillOutcome {
    let state = job.state.lock().await.clone();
    let Some(pgid) = job.pgid else {
        return match state {
            JobState::Running => KillOutcome::NoProcessGroup,
            finished => KillOutcome::NotRunning {
                status: status_word(&finished).to_string(),
            },
        };
    };
    let how = if job.interactive {
        Kill::GracefulStopLeader
    } else {
        Kill::Graceful
    };
    match state {
        JobState::Running => {
            // First reason wins: a cap breach already recorded stays the cause.
            let _ = job.kill_reason.set(reason.to_string());
            running_outcome(escalate(pgid, how).await, pgid)
        }
        finished => leftovers(pgid, how, status_word(&finished)).await,
    }
}

/// Kill a job from its persisted row — for every caller that holds no in-process
/// [`Job`]. Only [`persisted_group`] turns a row into a signal target. The
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
    if !running {
        return Ok(match group {
            Some(pgid) => leftovers(pgid, Kill::Graceful, &target.status).await,
            None => KillOutcome::NotRunning {
                status: target.status,
            },
        });
    }
    // A running row from a previous boot: its group cannot exist any more.
    let outcome = match group {
        Some(pgid) => running_outcome(escalate(pgid, Kill::Graceful).await, pgid),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

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

    async fn kill(repo: &JobRepo, id: &str) -> KillOutcome {
        kill_persisted(repo, &JobId::from(id), "killed")
            .await
            .unwrap()
    }

    /// A detached group like a real job's: its leader leads it (pgid == pid) and
    /// is reaped in the background, as the server or init would.
    fn spawn_group(script: &str) -> (ProcessGroupId, tokio::task::JoinHandle<()>) {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = ProcessGroupId::new(child.id().unwrap()).unwrap();
        let waiter = tokio::spawn(async move {
            let _ = child.wait().await;
        });
        (pgid, waiter)
    }

    /// errno → `Probe`: an error is never "gone". Signal 0 delivers nothing.
    #[test]
    fn errno_maps_onto_probe_without_reading_failure_as_gone() {
        // pid 1 exists; as non-root that's EPERM — alive, not ours. As root, 0.
        assert!(matches!(send(1, 0), Probe::Alive));
        // Above any pid_max: ESRCH.
        assert!(matches!(send(-2_000_000_000, 0), Probe::Gone));
        // An invalid signal is EINVAL: unknown, not gone and not alive.
        let me = libc::pid_t::try_from(std::process::id()).unwrap();
        assert!(matches!(send(me, 9999), Probe::Unknown(_)));
    }

    /// The invariant the type carries: no corrupt persisted pgid becomes a signal
    /// target. `0` would hit this process's own group, `1` (`kill -1`) every
    /// process we may signal, and anything past `i32` wraps onto a wrong group.
    #[test]
    fn corrupt_persisted_pgids_never_become_a_signal_target() {
        for corrupt in [0, 1, -1, i64::from(i32::MAX) + 1, i64::from(u32::MAX) + 1] {
            assert!(
                ProcessGroupId::from_persisted(corrupt).is_none(),
                "pgid {corrupt} must not be signalable"
            );
        }
        assert!(ProcessGroupId::from_persisted(1234).is_some());
        assert!(ProcessGroupId::new(1).is_none());
    }

    #[test]
    fn a_pgid_from_another_boot_is_never_signalled() {
        let Some(now) = current_boot_id() else {
            return; // no boot id on this kernel: every row is probed
        };
        assert!(persisted_group(Some(4242), Some(now)).is_some());
        assert!(persisted_group(Some(4242), None).is_some(), "legacy row");
        assert!(persisted_group(Some(4242), Some("another-boot")).is_none());
        assert!(matches!(
            probe_persisted(Some(4242), Some("another-boot")),
            Probe::Gone
        ));
    }

    #[tokio::test]
    async fn unsignalable_rows_are_refused_and_left_untouched() {
        let (repo, db) = repo();
        plant(&db, "done", "exited", Some(2_000_000_000)).await;
        plant(&db, "legacy", "running", None).await;
        plant(&db, "zero", "running", Some(0)).await;
        plant(&db, "one", "running", Some(1)).await;
        plant(&db, "wrapped", "running", Some(i64::from(u32::MAX) + 1)).await;

        assert!(matches!(kill(&repo, "ghost").await, KillOutcome::Unknown));
        assert!(
            matches!(kill(&repo, "done").await, KillOutcome::NotRunning { status } if status == "exited"),
            "a finished row whose group is gone is not signalled"
        );
        assert!(matches!(
            kill(&repo, "legacy").await,
            KillOutcome::NoProcessGroup
        ));
        // `0` is the dangerous one: it would signal *this* process's own group, so
        // this test is its own canary — a regression kills the runner.
        for (id, pgid) in [("zero", 0), ("one", 1)] {
            assert!(
                matches!(kill(&repo, id).await, KillOutcome::CorruptProcessGroup { pgid: p } if p == pgid)
            );
        }
        assert!(matches!(
            kill(&repo, "wrapped").await,
            KillOutcome::CorruptProcessGroup { .. }
        ));
        for id in ["legacy", "zero", "one", "wrapped"] {
            assert_eq!(status_of(&db, id).await.as_deref(), Some("running"), "{id}");
        }
        assert_eq!(status_of(&db, "done").await.as_deref(), Some("exited"));
    }

    #[tokio::test]
    async fn a_gone_group_is_recorded_failed_with_the_caller_s_reason() {
        let (repo, db) = repo();
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

    #[tokio::test]
    async fn escalate_terminates_a_detached_group() {
        let (pgid, waiter) = spawn_group("sleep 300");
        assert!(matches!(
            escalate(pgid, Kill::Graceful).await,
            Escalation::Gone
        ));
        let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    }

    #[tokio::test]
    async fn escalate_on_a_dead_pgid_reports_gone() {
        let dead = ProcessGroupId::from_persisted(2_000_000_000).unwrap();
        assert!(matches!(
            escalate(dead, Kill::Graceful).await,
            Escalation::Gone
        ));
    }

    /// K3: a TERM-ignoring descendant keeps the group alive after the leader dies;
    /// success is the group gone, so the KILL must still reach it.
    #[tokio::test]
    async fn escalate_reaches_a_term_ignoring_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let (pgid, waiter) = spawn_group(&format!(
            "(trap '' TERM; sleep 300) & echo $! > '{}'; wait",
            pidfile.display()
        ));
        let descendant = loop {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse::<libc::pid_t>()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        assert!(matches!(
            escalate(pgid, Kill::Graceful).await,
            Escalation::Gone
        ));
        assert!(
            matches!(send(descendant, 0), Probe::Gone),
            "descendant reaped"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    }

    /// K4: a finished row whose group still holds a process (`server &`) kills the
    /// leftovers rather than answering "not running".
    #[tokio::test]
    async fn a_finished_row_with_live_leftovers_kills_them() {
        let (pgid, waiter) = spawn_group("sleep 300");
        let (repo, db) = repo();
        plant(&db, "left", "exited", Some(i64::from(pgid.get()))).await;
        assert!(matches!(
            kill(&repo, "left").await,
            KillOutcome::KilledLeftovers
        ));
        assert!(matches!(probe(pgid), Probe::Gone));
        assert_eq!(
            status_of(&db, "left").await.as_deref(),
            Some("exited"),
            "the job's own exit stays its final state"
        );
        let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
    }
}
