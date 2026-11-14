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

use super::ProcessGroupId;

mod kill;

pub(super) use kill::kill_job;
pub(crate) use kill::{KillOutcome, kill_persisted};

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

#[cfg(test)]
mod tests;
