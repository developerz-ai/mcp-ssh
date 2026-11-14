//! The row⇄domain mapping for `jobs`: the typed rows queries return, and
//! `JobState` onto the `(status, code, error)` columns and back — so a status word
//! is spelled (via `JobStatus`) and read in exactly one place.
use crate::jobs::{JobId, JobState, JobStatus};

/// A job's durable row, mapped back onto the domain types. `tail` is the bounded
/// output snapshot saved when the job finished — empty when it has none yet.
#[derive(Debug)]
pub(in crate::jobs) struct PersistedJob {
    pub(in crate::jobs) state: JobState,
    pub(in crate::jobs) tail: String,
}

/// A `running` row this process doesn't track, with what is needed to probe its
/// group: the raw persisted pgid and the boot it was recorded under.
#[derive(Debug)]
pub(in crate::jobs) struct UntrackedJob {
    pub(in crate::jobs) id: JobId,
    pub(in crate::jobs) pgid: Option<i64>,
    pub(in crate::jobs) boot_id: Option<String>,
}

/// What a kill decides on: the row's status word (rendered back to the operator by
/// the CLI, so kept verbatim), its persisted process group (unvalidated —
/// `super::signal` gates it), the boot it was recorded under, and whether it was
/// launched through the interactive bash (which decides the escalation).
#[derive(Debug)]
pub(in crate::jobs) struct KillTarget {
    pub(in crate::jobs) status: String,
    pub(in crate::jobs) pgid: Option<i64>,
    pub(in crate::jobs) boot_id: Option<String>,
    pub(in crate::jobs) interactive: bool,
}

/// One row the reaper weighs for log compaction or eviction.
#[derive(Debug)]
pub(in crate::jobs) struct ReapRow {
    pub(in crate::jobs) id: JobId,
    /// What the row itself says about liveness. Authoritative only for a job this
    /// process doesn't track — otherwise the in-memory state is.
    pub(in crate::jobs) running: bool,
    /// When the job finished (when it started, for a row predating the column).
    pub(in crate::jobs) ended_unix: i64,
    pub(in crate::jobs) pgid: Option<i64>,
    pub(in crate::jobs) boot_id: Option<String>,
}

/// A row as `mcp-ssh jobs` renders it: the operator's view, so `status` stays the
/// raw word and the title is included.
#[derive(Debug)]
pub(crate) struct JobRow {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) code: Option<i64>,
    pub(crate) error: Option<String>,
    pub(crate) started_unix: i64,
    pub(crate) title: Option<String>,
}

/// Map a `JobState` onto the DB row's `(status, code, error)` columns.
pub(super) fn state_columns(state: &JobState) -> (&'static str, Option<i32>, Option<String>) {
    match state {
        JobState::Running => (JobStatus::Running.as_str(), None, None),
        JobState::Exited { code } => (JobStatus::Exited.as_str(), Some(*code), None),
        JobState::Failed { error } => (JobStatus::Failed.as_str(), None, Some(error.clone())),
    }
}

/// Rebuild a `JobState` from a DB row's columns. Anything the engine could never
/// have written — an unknown status word, an exit code outside `i32` — reads as
/// `Failed`, so the anomaly surfaces instead of masquerading as a live or a
/// cleanly-exited job (or wrapping onto a different code).
pub(super) fn state_from_columns(
    status: &str,
    code: Option<i64>,
    error: Option<String>,
) -> JobState {
    match status.parse::<JobStatus>() {
        Ok(JobStatus::Running) => JobState::Running,
        Ok(JobStatus::Exited) => match code.map(i32::try_from) {
            Some(Ok(code)) => JobState::Exited { code },
            Some(Err(_)) | None => JobState::Failed {
                error: "corrupt row: exited without a valid exit code".to_string(),
            },
        },
        Ok(JobStatus::Failed) | Err(_) => JobState::Failed {
            error: error.unwrap_or_else(|| status.to_string()),
        },
    }
}

/// One `mcp-ssh jobs` row. A free function so both `listing` branches share it.
pub(super) fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        status: r.get(1)?,
        code: r.get(2)?,
        error: r.get(3)?,
        started_unix: r.get(4)?,
        title: r.get(5)?,
    })
}
