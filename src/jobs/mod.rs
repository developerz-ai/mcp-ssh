//! Job engine: run a command, return inline if it's fast, otherwise hand back
//! a job id the caller polls. Output streams to a per-job log file so polling
//! can page it without holding everything in memory.
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock, atomic::AtomicU64},
    time::Duration,
};

use tokio::sync::Mutex;

mod id;
mod log;
mod pgid;
mod poll;
mod reaper;
mod run;
mod shell;
mod signal;
mod status;
mod store;
mod supervise;

pub use id::JobId;
pub use log::TailPage;
pub(crate) use pgid::ProcessGroupId;
use reaper::spawn_reaper;
pub use shell::Shell;
use signal::kill_job;
pub(crate) use signal::{KillOutcome, kill_persisted};
pub use status::JobStatus;
pub(crate) use store::{JobRepo, JobRow};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobState {
    Running,
    Exited { code: i32 },
    Failed { error: String },
}

/// Most rows one `job(action="list")` page returns.
pub const MAX_LIST_PAGE: usize = 200;

struct Job {
    log_path: PathBuf,
    /// Process group to signal on kill. The child leads its own group, so this
    /// equals its pid (see `run`). `None` only if the OS withheld a pid.
    pgid: Option<ProcessGroupId>,
    /// Launched through an interactive bash, which ignores `SIGTERM` — killing it
    /// takes a different escalation (see `signal::Kill`).
    interactive: bool,
    state: Arc<Mutex<JobState>>,
    /// Why this job is being killed, set before the first signal. The waiter
    /// records it as the job's final error instead of a bare "killed by signal".
    kill_reason: OnceLock<String>,
}

/// Result of starting a command.
pub enum RunResult {
    /// Finished within the inline window — output is ready now. Carries the `id`
    /// too: the job is still in the store, so if the inline page is truncated the
    /// agent can poll `id` for older output.
    Inline {
        id: JobId,
        state: JobState,
        page: TailPage,
    },
    /// Still running — poll this id.
    Backgrounded { id: JobId },
}

/// Public, per-job metadata for `job(action="list")`. Deliberately omits the
/// command text: a command can carry a secret in its leading tokens
/// (`PGPASSWORD=…`, bearer headers, pasted tokens), and this is returned in
/// ordinary MCP responses — so the listing exposes only the id and status.
#[derive(Debug, serde::Serialize)]
pub struct JobSummary {
    pub id: JobId,
    pub state: JobState,
}

/// One page of `job(action="list")`, newest start first.
#[derive(Debug)]
pub struct JobList {
    pub jobs: Vec<JobSummary>,
    /// Pass back as `cursor` for the next (older) page; `None` on the last one.
    pub next_cursor: Option<usize>,
}

/// A job operation failed. Facts only — `src/tools` phrases them for the agent.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("cwd {cwd}: {source}")]
    Cwd {
        cwd: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cwd {cwd} is not a directory")]
    CwdNotADirectory { cwd: String },
    #[error("starting the command: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("reading job log: {0}")]
    Log(#[source] std::io::Error),
    #[error("job store: {0}")]
    Store(#[from] rusqlite::Error),
}

/// Create the job dir owner-only (0700) — job logs are command output and may
/// hold secrets — and tighten a pre-existing looser one, failing loudly if that
/// isn't possible.
fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
    if mode != 0o700 {
        tracing::warn!(dir = %dir.display(), mode = format!("{mode:o}"), "tightening job dir to 0700");
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct JobStore {
    dir: PathBuf,
    inline_timeout: Duration,
    /// Shell used when a `bash` call opts into `interactive` (sources `~/.bashrc`).
    /// The default path uses a bare `sh -c` (`Shell::sh`).
    interactive_shell: Shell,
    /// The live log cap ([`LOG_CAP`]; a field so tests can shrink it).
    log_cap: u64,
    seq: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>,
    /// Durable metadata + saved output tails, so `list`/`poll` show history across
    /// restarts. Live output still streams to the per-job log files.
    repo: JobRepo,
}

impl JobStore {
    /// The reaper's first pass (spawned here) is also the startup reconcile: rows
    /// a previous process left `running` are flipped to `failed` once their group
    /// is really gone.
    pub fn new(
        dir: PathBuf,
        inline_timeout: Duration,
        interactive_shell: Shell,
        db: crate::db::Db,
    ) -> std::io::Result<Self> {
        create_private_dir(&dir)?;
        let jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>> = Arc::new(Mutex::new(HashMap::new()));
        let repo = JobRepo::new(db);
        spawn_reaper(jobs.clone(), repo.clone(), dir.clone());
        Ok(Self {
            dir,
            inline_timeout,
            interactive_shell,
            log_cap: supervise::LOG_CAP,
            seq: Arc::new(AtomicU64::new(1)),
            jobs,
            repo,
        })
    }

    /// One page of job history, newest first. History lives in the DB, so this
    /// reflects finished jobs and jobs from a previous process too.
    pub async fn list(&self, cursor: usize, limit: Option<usize>) -> Result<JobList, JobError> {
        let limit = limit.unwrap_or(MAX_LIST_PAGE).clamp(1, MAX_LIST_PAGE);
        let (jobs, more) = self.repo.summaries(cursor, limit).await?;
        let next_cursor = more.then(|| cursor + jobs.len());
        Ok(JobList { jobs, next_cursor })
    }

    /// Kill a job's process group — through the in-process handle when this
    /// process tracks it, else through its persisted row (a job whose group
    /// outlived the process that started it is still killable).
    pub async fn kill(&self, id: &JobId) -> Result<KillOutcome, JobError> {
        // Bind to a local so the `jobs` guard is released at this `;`.
        let tracked = self.jobs.lock().await.get(id).cloned();
        let outcome = match tracked {
            Some(job) => kill_job(&job, "killed").await,
            None => kill_persisted(&self.repo, id, "killed").await?,
        };
        if matches!(
            outcome,
            KillOutcome::NoProcessGroup
                | KillOutcome::CorruptProcessGroup { .. }
                | KillOutcome::NotSignalled { .. }
                | KillOutcome::Survived { .. }
        ) {
            // Worth a line: the job may still be running, and nothing stopped it.
            tracing::warn!(id = %id, ?outcome, "job kill did not take");
        }
        Ok(outcome)
    }
}

/// Whether `pid` still exists — for tests outside this module tree, which can't
/// reach `signal`.
#[cfg(test)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(|pid| !matches!(signal::send(pid, 0), signal::Probe::Gone))
}

#[cfg(test)]
mod tests;
