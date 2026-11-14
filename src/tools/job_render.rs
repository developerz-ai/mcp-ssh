//! Agent-facing wording for `bash` and `job`: `crate::jobs` returns facts (states,
//! pages, kill outcomes, typed errors) and every sentence the agent reads about
//! them is phrased here.
use rmcp::model::CallToolResult;

use super::{err, ok};
use crate::jobs::{JobError, JobId, JobList, JobState, KillOutcome, RunResult, TailPage};

/// A `bash` call's reply. `background` is whether the caller asked for `bg`, so a
/// backgrounded job reads as started rather than as "still running".
pub(super) fn run(result: Result<RunResult, JobError>, background: bool) -> CallToolResult {
    match result {
        Ok(RunResult::Inline { id, state, page }) => ok(state_and_page(&id, &state, &page)),
        Ok(RunResult::Backgrounded { id }) if background => ok(format!(
            "job {id} started in the background. Monitor it with job(action=\"poll\", id=\"{id}\")."
        )),
        Ok(RunResult::Backgrounded { id }) => ok(format!(
            "job {id} still running after the inline window. Monitor it with job(action=\"poll\", id=\"{id}\")."
        )),
        Err(e) => err(job_error(&e)),
    }
}

/// A `job(action="poll")` reply.
pub(super) fn poll(
    id: &JobId,
    result: Result<Option<(JobState, TailPage)>, JobError>,
) -> CallToolResult {
    match result {
        Ok(Some((state, page))) => ok(state_and_page(id, &state, &page)),
        Ok(None) => err(format!("no such job: {id}")),
        Err(e) => err(job_error(&e)),
    }
}

/// A `job(action="list")` reply: one page of `{id, status}` objects, newest
/// first, and the cursor for the next page when there is one.
pub(super) fn list(result: Result<JobList, JobError>) -> CallToolResult {
    let list = match result {
        Ok(list) => list,
        Err(e) => return err(job_error(&e)),
    };
    let mut out = match serde_json::to_string_pretty(&list.jobs) {
        Ok(json) => json,
        Err(e) => return err(format!("could not render the job list: {e}")),
    };
    if let Some(next) = list.next_cursor {
        out.push_str(&format!(
            "\n[older jobs: job(action=\"list\", cursor={next})]"
        ));
    }
    ok(out)
}

/// A `job(action="kill")` reply — each outcome says what actually happened.
pub(super) fn kill(id: &JobId, result: Result<KillOutcome, JobError>) -> CallToolResult {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => return err(job_error(&e)),
    };
    match outcome {
        KillOutcome::Killed => ok(format!("killed {id}")),
        KillOutcome::KilledLeftovers => ok(format!(
            "job {id} had already finished; killed the processes it left running"
        )),
        KillOutcome::Unknown => err(format!("no such job: {id}")),
        KillOutcome::NotRunning { status } => err(format!(
            "job {id} is not running (status: {status}) — nothing to kill"
        )),
        KillOutcome::NoProcessGroup => err(format!(
            "job {id} has no recorded process group — it cannot be killed"
        )),
        KillOutcome::CorruptProcessGroup { pgid } => err(format!(
            "job {id} has a corrupt process group id ({pgid}) — refusing to signal it"
        )),
        KillOutcome::Survived { pgid } => err(format!(
            "job {id} (process group {}) survived TERM then KILL — still running",
            pgid.get()
        )),
        KillOutcome::NotSignalled { error } => err(format!(
            "could not signal job {id}: {error} — it may still be running"
        )),
    }
}

/// An unrecognised `action` string — named, with the valid ones.
pub(super) fn unknown_action(action: &str) -> CallToolResult {
    err(format!(
        "unknown job action {action:?} — use one of: poll, list, kill"
    ))
}

/// The status line, then the page. When older output exists, a footer says how
/// much was shown and the exact call that pages back.
fn state_and_page(id: &JobId, state: &JobState, page: &TailPage) -> String {
    let mut s = serde_json::to_string(state).unwrap_or_default();
    s.push('\n');
    s.push_str(&page.lines.join("\n"));
    if let Some(older) = page.older {
        s.push_str(&format!(
            "\n[latest {} lines shown; earlier output: job(action=\"poll\", id=\"{id}\", cursor={older})]",
            page.lines.len()
        ));
    }
    s
}

fn job_error(e: &JobError) -> String {
    match e {
        JobError::Cwd { cwd, source } => format!("cwd {cwd} is not usable: {source}"),
        JobError::CwdNotADirectory { cwd } => format!("cwd {cwd} is not a directory"),
        JobError::Spawn(source) => format!("could not start the command: {source}"),
        JobError::Log(source) => format!("job log unavailable: {source}"),
        // The rusqlite detail goes to the server log, not to the agent: all it can
        // do is retry.
        JobError::Store(error) => {
            tracing::warn!(%error, "job store error");
            "job store unavailable — try again shortly".to_string()
        }
    }
}
