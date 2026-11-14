//! Agent-facing wording for `bash` and `job`: `crate::jobs` returns facts (states,
//! pages, kill outcomes, typed errors) and every sentence the agent reads about
//! them is phrased here.
use rmcp::model::CallToolResult;

use super::{err, ok};
use crate::agent::Agent;
use crate::jobs::{JobError, JobId, JobList, JobState, KillOutcome, RunResult, TailPage};
use crate::oauth::{PRESENCE_WINDOW, StoreUnavailable};

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

/// A `job(action="poll")` reply. `foreign` labels the job's owner when that is
/// another agent.
pub(super) fn poll(
    id: &JobId,
    result: Result<Option<(JobState, TailPage)>, JobError>,
    foreign: Option<&str>,
) -> CallToolResult {
    match result {
        Ok(Some((state, page))) => ok(owner_note(foreign, state_and_page(id, &state, &page))),
        Ok(None) => err(format!("no such job: {id}")),
        Err(e) => err(job_error(&e)),
    }
}

/// One `job(action="list")` entry: who started it is `by` — `you`, another
/// agent's label, or `unknown` for a job from before owners were recorded.
#[derive(serde::Serialize)]
struct ListedJob<'a> {
    id: &'a JobId,
    state: &'a JobState,
    by: String,
}

/// A `job(action="list")` reply: the presence line (which agents are on this
/// host), then one page of `{id, state, by}` objects, newest first, and the
/// cursor for the next page when there is one.
pub(super) fn list(
    result: Result<JobList, JobError>,
    caller: Option<&Agent>,
    active: Result<Vec<Agent>, StoreUnavailable>,
) -> CallToolResult {
    let list = match result {
        Ok(list) => list,
        Err(e) => return err(job_error(&e)),
    };
    let rows: Vec<ListedJob> = list
        .jobs
        .iter()
        .map(|job| ListedJob {
            id: &job.id,
            state: &job.state,
            by: match &job.owner {
                Some(owner) if caller == Some(owner) => "you".to_string(),
                Some(owner) => owner.label(),
                None => "unknown".to_string(),
            },
        })
        .collect();
    let mut out = presence(caller, active);
    out.push('\n');
    match serde_json::to_string_pretty(&rows) {
        Ok(json) => out.push_str(&json),
        Err(e) => return err(format!("could not render the job list: {e}")),
    }
    if let Some(next) = list.next_cursor {
        out.push_str(&format!(
            "\n[older jobs: job(action=\"list\", cursor={next})]"
        ));
    }
    ok(out)
}

/// Which agents used this host within [`PRESENCE_WINDOW`], the caller first and
/// marked `you`: `2 agents active (last 15 min): you (claude-code#4f2a), cursor#9b1c`.
fn presence(caller: Option<&Agent>, active: Result<Vec<Agent>, StoreUnavailable>) -> String {
    let Ok(active) = active else {
        return "(which agents are active is unavailable right now)".to_string();
    };
    let you = caller.map(|me| format!("you ({})", me.label()));
    let others = active
        .iter()
        .filter(|agent| caller != Some(*agent))
        .map(Agent::label);
    let names: Vec<String> = you.into_iter().chain(others).collect();
    let minutes = PRESENCE_WINDOW.as_secs() / 60;
    let plural = if names.len() == 1 { "" } else { "s" };
    format!(
        "{} agent{plural} active (last {minutes} min): {}",
        names.len(),
        names.join(", ")
    )
}

/// Prefix a reply about another agent's job with whose it is.
fn owner_note(foreign: Option<&str>, text: String) -> String {
    match foreign {
        Some(owner) => format!("[started by another agent: {owner}]\n{text}"),
        None => text,
    }
}

/// A `job(action="kill")` reply — each outcome says what actually happened, and
/// whose job it was when `foreign` names another agent.
pub(super) fn kill(
    id: &JobId,
    result: Result<KillOutcome, JobError>,
    foreign: Option<&str>,
) -> CallToolResult {
    match kill_sentence(id, result) {
        Ok(text) => ok(owner_note(foreign, text)),
        Err(text) => err(owner_note(foreign, text)),
    }
}

fn kill_sentence(id: &JobId, result: Result<KillOutcome, JobError>) -> Result<String, String> {
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(e) => return Err(job_error(&e)),
    };
    match outcome {
        KillOutcome::Killed => Ok(format!("killed {id}")),
        KillOutcome::KilledLeftovers => Ok(format!(
            "job {id} had already finished; killed the processes it left running"
        )),
        KillOutcome::Unknown => Err(format!("no such job: {id}")),
        KillOutcome::NotRunning { status } => Err(format!(
            "job {id} is not running (status: {status}) — nothing to kill"
        )),
        KillOutcome::NoProcessGroup => Err(format!(
            "job {id} has no recorded process group — it cannot be killed"
        )),
        KillOutcome::CorruptProcessGroup { pgid } => Err(format!(
            "job {id} has a corrupt process group id ({pgid}) — refusing to signal it"
        )),
        KillOutcome::Survived { pgid } => Err(format!(
            "job {id} (process group {}) survived TERM then KILL — still running",
            pgid.get()
        )),
        KillOutcome::NotSignalled { error } => Err(format!(
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
