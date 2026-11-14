//! Job engine: run a command, return inline if it's fast, otherwise hand back
//! a job id the caller polls. Output streams to a per-job log file so polling
//! can page it without holding everything in memory.
use std::{
    collections::{HashMap, HashSet},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock, atomic::AtomicU64},
    time::Duration,
};

use tokio::sync::{Mutex, oneshot, watch};

mod id;
mod log;
mod reaper;
mod shell;
mod signal;
mod status;
mod store;

pub use id::JobId;
pub use log::TailPage;
use log::{DEFAULT_PAGE, TailBudget, read_tail};
use reaper::spawn_reaper;
pub use shell::Shell;
use signal::{Kill, escalate, kill_job};
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

/// Most bytes a job's live log may reach. Past it the job's group is killed and it
/// finishes `failed` — a runaway writer must not fill the volume the database
/// shares (one was measured at 150 GB a minute).
const LOG_CAP: u64 = 256 * 1024 * 1024;
const LOG_CAP_REASON: &str = "output exceeded the 256 MiB log cap — killed";
/// How often a running job's log size is checked against the cap.
const LOG_CAP_EVERY: Duration = Duration::from_millis(100);

/// Most rows one `job(action="list")` page returns.
pub const MAX_LIST_PAGE: usize = 200;

/// Process group id to signal on kill. Holds only `2..=i32::MAX`: `0` is the
/// caller's own group (the server itself), `1` would be `kill(-1)` — every
/// process the service user may signal — and anything past `i32` would wrap onto
/// a real, wrong group. Every path into a signal goes through this type, so no
/// guard can be forgotten at a call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProcessGroupId(libc::pid_t);

impl ProcessGroupId {
    /// The group led by a live child (it leads its own, so pgid == pid).
    fn new(pid: u32) -> Option<Self> {
        libc::pid_t::try_from(pid).ok().and_then(Self::checked)
    }

    /// The group behind a persisted `jobs.pgid`. `None` for anything a real job
    /// could never have written, so a corrupt row can't be signalled. Private to
    /// this module tree: outside it, a kill goes through [`signal::kill_persisted`].
    fn from_persisted(pgid: i64) -> Option<Self> {
        libc::pid_t::try_from(pgid).ok().and_then(Self::checked)
    }

    fn checked(pid: libc::pid_t) -> Option<Self> {
        (pid >= 2).then_some(Self(pid))
    }

    pub(crate) fn get(self) -> libc::pid_t {
        self.0
    }
}

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

/// Single-quote a path for safe interpolation into a shell command. The job log
/// path is engine-controlled, but quoting keeps an odd `job_dir` (spaces, `$`)
/// from breaking the `exec` redirect in `run`.
fn sh_single_quote(path: &Path) -> String {
    let escaped = path.to_string_lossy().replace('\'', r"'\''");
    format!("'{escaped}'")
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

/// A `cwd` must name a directory — checked before anything is reserved, so a bad
/// one fails with its name rather than a bare errno and leaves nothing behind.
async fn check_cwd(cwd: &str) -> Result<(), JobError> {
    match tokio::fs::metadata(cwd).await {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(JobError::CwdNotADirectory {
            cwd: cwd.to_string(),
        }),
        Err(source) => Err(JobError::Cwd {
            cwd: cwd.to_string(),
            source,
        }),
    }
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
            log_cap: LOG_CAP,
            seq: Arc::new(AtomicU64::new(1)),
            jobs,
            repo,
        })
    }

    /// Spawn `cmd`. With `background`, return a job id immediately; otherwise wait
    /// up to the inline window and return output if it finishes in time.
    /// `interactive` runs it through `~/.bashrc` (aliases, version managers);
    /// otherwise the fast bare `sh -c` is used.
    pub async fn run(
        &self,
        cmd: String,
        cwd: Option<String>,
        timeout_secs: Option<u64>,
        background: bool,
        interactive: bool,
        title: Option<String>,
    ) -> Result<RunResult, JobError> {
        if let Some(dir) = &cwd {
            check_cwd(dir).await?;
        }
        // The id's label is the agent-supplied `title` (or the neutral `job`
        // fallback), never `cmd`: a command can carry a secret in its leading
        // tokens, and the id ends up in replies, `job(list)`, logs, and the log
        // filename. The normalized label is also what the row stores, so an
        // operator's terminal never renders a raw agent string.
        let label = title.as_deref().and_then(id::normalize_title);
        let (id, log_path) = self
            .reserve_log(label.as_deref())
            .await
            .map_err(JobError::Log)?;

        // An interactive bash prints job-control warnings to stderr at startup
        // when there's no controlling TTY (always, under systemd). So the child's
        // own stdio goes to /dev/null and the command re-points stdout+stderr at
        // the log itself, *after* startup: only the command's output is captured.
        let wrapped = format!("exec >>{} 2>&1\n{}", sh_single_quote(&log_path), cmd);
        let fast = Shell::sh();
        let shell = if interactive {
            &self.interactive_shell
        } else {
            &fast
        };
        let mut command = tokio::process::Command::new(&shell.program);
        command
            .args(&shell.args)
            .arg(&wrapped)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(dir) = cwd {
            command.current_dir(dir);
        }
        // Own process group (child becomes leader, so pgid == pid). Lets `kill`
        // signal the whole tree the command spawns, not just `sh` itself.
        command.process_group(0);

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                // Nothing will ever write the reserved log; don't leave it behind.
                let _ = tokio::fs::remove_file(&log_path).await;
                return Err(JobError::Spawn(error));
            }
        };
        let (done_tx, done_rx) = watch::channel(false);
        let state = Arc::new(Mutex::new(JobState::Running));
        let job = Arc::new(Job {
            log_path: log_path.clone(),
            pgid: child.id().and_then(ProcessGroupId::new),
            interactive,
            state: state.clone(),
            kill_reason: OnceLock::new(),
        });

        // The supervisor owns the child from here, with no `.await` between the
        // spawn and handing it over: if this request is cancelled now, the job is
        // still tracked, persisted, reaped, and finished — never a phantom.
        let (started_tx, started_rx) = oneshot::channel();
        tokio::spawn(supervise(
            Supervised {
                id: id.clone(),
                job,
                label,
                log_cap: self.log_cap,
                done: done_tx,
            },
            child,
            self.jobs.clone(),
            self.repo.clone(),
            started_tx,
        ));
        match started_rx.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                return Err(JobError::Spawn(std::io::Error::other(
                    "job supervisor ended before the job started",
                )));
            }
        }

        // `bg: true` — don't wait, hand back the id straight away.
        if background {
            return Ok(RunResult::Backgrounded { id });
        }

        // Wait for completion or the inline window, whichever comes first.
        let window = timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.inline_timeout);
        let mut done = done_rx;
        let _ = tokio::time::timeout(window, done.wait_for(|&exited| exited)).await;

        let current = state.lock().await.clone();
        match current {
            JobState::Running => Ok(RunResult::Backgrounded { id }),
            finished => Ok(RunResult::Inline {
                state: finished,
                // The newest page: where a failing command's error is.
                page: read_tail(&log_path, None, TailBudget::page(DEFAULT_PAGE))
                    .await
                    .map_err(JobError::Log)?,
                id,
            }),
        }
    }

    /// Mint a job id and create its log file (0600 — command output may hold
    /// secrets), both off the `jobs` lock — that lock is on every request path,
    /// and a stat/open is a blocking syscall.
    ///
    /// The `<id>.log` file *is* the reservation: `create_new` fails if the path
    /// exists, so one atomic step fends off both claimants to an id — a concurrent
    /// `run` minting the same `<label>-HH-MM-SS` this second, and a job from a
    /// previous process whose log was retained. A candidate lost to either is fed
    /// back to `generate`, which then moves on to a fresh `-<seq>` suffix.
    ///
    /// Creating the log up front also means a poll racing the spawn reads an empty
    /// page, not a NotFound — the command appends to it (see `wrapped` in `run`).
    async fn reserve_log(&self, label: Option<&str>) -> std::io::Result<(JobId, PathBuf)> {
        let mut lost: HashSet<String> = HashSet::new();
        loop {
            let id = JobId::generate(&self.seq, label, |candidate| lost.contains(candidate));
            let log_path = self.dir.join(format!("{id}.log"));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&log_path)
                .await
            {
                Ok(_) => return Ok((id, log_path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    lost.insert(id.as_ref().to_string());
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Status + one newest-first page of a job's log ending at byte `cursor`
    /// (`None` = the latest output). `Ok(None)` means the id is unknown.
    pub async fn poll(
        &self,
        id: &JobId,
        cursor: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Option<(JobState, TailPage)>, JobError> {
        let budget = TailBudget::page(limit.unwrap_or(DEFAULT_PAGE));
        // Bind the lookup to a local so the `jobs` guard is released at this `;`.
        let tracked = self.jobs.lock().await.get(id).cloned();
        if let Some(job) = tracked {
            let state = job.state.lock().await.clone();
            let page = match read_tail(&job.log_path, cursor, budget).await {
                Ok(page) => page,
                // A reaper pass can evict this id and delete its log between the
                // lookup and this read: once the id is gone that's "unknown job",
                // not a read fault.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if self.jobs.lock().await.contains_key(id) {
                        return Err(JobError::Log(error));
                    }
                    return Ok(None);
                }
                Err(error) => return Err(JobError::Log(error)),
            };
            // Once finished, the row is the record: another writer (`mcp-ssh job
            // kill`) may have decided the final state. Fall back to memory only if
            // the row can't be read.
            let state = match state {
                JobState::Running => state,
                finished => match self.repo.find(id).await {
                    Ok(Some(row)) => row.state,
                    _ => finished,
                },
            };
            return Ok(Some((state, page)));
        }
        // Not live here — fall back to the durable row so finished jobs (and jobs
        // from a previous process) stay pollable.
        let Some(row) = self.repo.find(id).await? else {
            return Ok(None);
        };
        let log_path = self.dir.join(format!("{id}.log"));
        let page = match read_tail(&log_path, cursor, budget).await {
            Ok(page) => page,
            // Trimmed away or reaped: serve the tail saved in the row.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                store::page_from_tail(&row.tail, cursor, budget.lines)
            }
            Err(error) => return Err(JobError::Log(error)),
        };
        Ok(Some((row.state, page)))
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

/// What the supervisor needs to own a launched job.
struct Supervised {
    id: JobId,
    job: Arc<Job>,
    label: Option<String>,
    log_cap: u64,
    done: watch::Sender<bool>,
}

/// Own a launched job to the end: track it, persist its row (reporting the
/// outcome to `run` through `started`), enforce the live log cap, reap the child,
/// and record its final state. Runs as its own task, so it finishes even when the
/// request that launched it is cancelled.
///
/// The map insert precedes the row insert: the reaper's reconcile snapshots the
/// map under its lock before querying `running` rows, so a row this process
/// creates is either in that snapshot or written after the query.
async fn supervise(
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

/// Whether `pid` still exists — for tests outside this module tree, which can't
/// reach `signal`.
#[cfg(test)]
pub(crate) fn pid_alive(pid: u32) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(|pid| !matches!(signal::send(pid, 0), signal::Probe::Gone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use rusqlite::OptionalExtension;

    fn store(inline: Duration) -> JobStore {
        let dir = tempfile::tempdir().unwrap().keep();
        JobStore::new(dir, inline, Shell::sh(), Db::memory()).unwrap()
    }

    /// Wait for the waiter task to persist a job's final state. Returns the
    /// finished row's `(status, code, error, output_tail)`.
    async fn await_row(
        db: &Db,
        id: &JobId,
    ) -> (String, Option<i64>, Option<String>, Option<String>) {
        for _ in 0..200 {
            let row_id = id.as_ref().to_string();
            let row = db
                .call(move |conn| {
                    conn.query_row(
                        "SELECT status, code, error, output_tail FROM jobs WHERE id = ?1",
                        [row_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                })
                .await
                .unwrap();
            if let Some(row) = row
                && row.0 != "running"
            {
                return row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job row never reached a finished state");
    }

    /// Wait for a finished row's output tail, written just after its status.
    async fn await_tail(db: &Db, id: &JobId) -> Option<String> {
        for _ in 0..200 {
            let tail = await_row(db, id).await.3;
            if tail.is_some() {
                return tail;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        None
    }

    async fn row_status(db: &Db, id: &str) -> Option<String> {
        let id = id.to_string();
        db.call(move |conn| {
            conn.query_row("SELECT status FROM jobs WHERE id = ?1", [id], |r| {
                r.get::<_, String>(0)
            })
            .optional()
        })
        .await
        .unwrap()
    }

    fn alive(pid: &str) -> bool {
        pid_alive(pid.parse().unwrap())
    }

    async fn bg(store: &JobStore, cmd: &str) -> JobId {
        match store
            .run(cmd.into(), None, None, true, false, None)
            .await
            .unwrap()
        {
            RunResult::Backgrounded { id } => id,
            RunResult::Inline { .. } => panic!("bg should background"),
        }
    }

    /// Poll until the job's log shows a `pid:<n>` line; returns `<n>`.
    async fn logged_pid(store: &JobStore, id: &JobId) -> String {
        for _ in 0..100 {
            let (_s, page) = store.poll(id, None, None).await.unwrap().unwrap();
            if let Some(pid) = page.lines.iter().find_map(|l| l.strip_prefix("pid:")) {
                return pid.trim().to_string();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("never saw the logged pid");
    }

    async fn await_state(store: &JobStore, id: &JobId) -> JobState {
        for _ in 0..200 {
            let (state, _) = store.poll(id, None, None).await.unwrap().unwrap();
            if !matches!(state, JobState::Running) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job never finished");
    }

    async fn gone_soon(pid: &str) -> bool {
        for _ in 0..100 {
            if !alive(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    /// The production shell (interactive bash) must expand aliases defined in
    /// the sourced rc file, and its startup job-control warnings must NOT leak
    /// into the captured log.
    #[tokio::test]
    async fn interactive_shell_expands_rc_aliases_without_leaking_startup_noise() {
        let dir = tempfile::tempdir().unwrap().keep();
        let rc = dir.join("rc");
        std::fs::write(&rc, "alias greet='echo ALIAS_OK'\n").unwrap();
        let store = JobStore::new(
            dir.join("jobs"),
            Duration::from_secs(5),
            Shell::bash_with_rcfile(rc.to_str().unwrap()),
            Db::memory(),
        )
        .unwrap();

        let r = store
            .run("greet".into(), None, None, false, true, None)
            .await
            .unwrap();
        let RunResult::Inline { state, page, .. } = r else {
            panic!("fast command should be inline");
        };
        assert!(matches!(state, JobState::Exited { code: 0 }));
        assert!(
            page.lines.iter().any(|l| l.contains("ALIAS_OK")),
            "{:?}",
            page.lines
        );
        assert!(!page.lines.iter().any(|l| l.contains("no job control")));
    }

    #[tokio::test]
    async fn fast_command_returns_inline() {
        let r = store(Duration::from_secs(5))
            .run("echo hello".into(), None, None, false, false, None)
            .await
            .unwrap();
        let RunResult::Inline { state, page, .. } = r else {
            panic!("fast command should be inline");
        };
        assert!(matches!(state, JobState::Exited { code: 0 }));
        assert_eq!(page.lines, ["hello"]);
        assert_eq!(page.older, None);
    }

    /// D1: job output may hold secrets — the dir is 0700, each log 0600.
    #[tokio::test]
    async fn job_dir_and_logs_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("jobs");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        let store = JobStore::new(
            dir.clone(),
            Duration::from_secs(5),
            Shell::sh(),
            Db::memory(),
        )
        .unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700, "a loose pre-existing dir is tightened");
        let RunResult::Inline { id, .. } = store
            .run("echo s3cret".into(), None, None, false, false, None)
            .await
            .unwrap()
        else {
            panic!("inline");
        };
        assert_eq!(mode(&dir.join(format!("{id}.log"))), 0o600);
    }

    /// D6: the row stores the normalized label, never the raw agent string.
    #[tokio::test]
    async fn title_labels_the_id_and_the_row_stores_it_normalized() {
        let store = store(Duration::from_secs(5));
        let r = store
            .run(
                "echo hi".into(),
                None,
                None,
                true,
                false,
                Some("deploy check\x1b[2J\nforged".into()),
            )
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };
        assert!(id.as_ref().starts_with("deploy-check-2J-forged-"), "{id}");
        let row_id = id.as_ref().to_string();
        let title: Option<String> = store
            .repo
            .db()
            .call(move |c| {
                c.query_row("SELECT title FROM jobs WHERE id = ?1", [row_id], |r| {
                    r.get(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(title.as_deref(), Some("deploy-check-2J-forged"));
    }

    /// K14: a bad `cwd` fails by name and leaves no reserved log behind.
    #[tokio::test]
    async fn a_bad_cwd_is_named_and_leaves_nothing_behind() {
        let store = store(Duration::from_secs(5));
        let missing = store.dir.join("no-such-dir").to_string_lossy().into_owned();
        let Err(error) = store
            .run(
                "true".into(),
                Some(missing.clone()),
                None,
                false,
                false,
                None,
            )
            .await
        else {
            panic!("a missing cwd must fail");
        };
        assert!(
            matches!(&error, JobError::Cwd { cwd, .. } if *cwd == missing),
            "{error:?}"
        );

        let file = store.dir.join("not-a-dir");
        std::fs::write(&file, "").unwrap();
        let file = file.to_string_lossy().into_owned();
        let Err(error) = store
            .run("true".into(), Some(file), None, false, false, None)
            .await
        else {
            panic!("a file cwd must fail");
        };
        assert!(matches!(error, JobError::CwdNotADirectory { .. }));
        let logs = std::fs::read_dir(&store.dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "log")
            })
            .count();
        assert_eq!(logs, 0, "no reserved log left behind");
    }

    #[tokio::test]
    async fn inline_overflow_surfaces_id_and_remains_pollable() {
        let store = store(Duration::from_secs(5));
        let n = DEFAULT_PAGE + 50;
        let r = store
            .run(format!("seq 1 {n}"), None, None, false, false, None)
            .await
            .unwrap();
        let RunResult::Inline { id, page, .. } = r else {
            panic!("seq is fast — should be inline");
        };
        assert_eq!(page.lines.len(), DEFAULT_PAGE, "first page is capped");
        assert_eq!(page.lines.last().unwrap(), &n.to_string(), "newest page");
        let older = page.older.expect("output exceeds one page");
        let (_state, rest) = store.poll(&id, Some(older), None).await.unwrap().unwrap();
        assert_eq!(rest.lines.first().map(String::as_str), Some("1"));
        assert_eq!(rest.lines.len(), 50);
        assert_eq!(rest.older, None);
    }

    /// Concurrent launches sharing a title land on the same `<title>-HH-MM-SS`
    /// base: each must still get its own id and its own log.
    #[tokio::test]
    async fn concurrent_launches_never_share_an_id_or_a_log() {
        const N: usize = 8;
        let store = store(Duration::from_secs(5));
        let launches: Vec<_> = (0..N)
            .map(|i| {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .run(
                            format!("echo marker-{i}"),
                            None,
                            None,
                            true,
                            false,
                            Some("race".into()),
                        )
                        .await
                })
            })
            .collect();
        let mut ids = Vec::new();
        for launch in launches {
            let RunResult::Backgrounded { id } = launch.await.unwrap().unwrap() else {
                panic!("bg should background");
            };
            ids.push(id);
        }
        let unique: HashSet<&str> = ids.iter().map(|id| id.as_ref()).collect();
        assert_eq!(unique.len(), N, "concurrent ids must be distinct: {ids:?}");
        for (i, id) in ids.iter().enumerate() {
            await_state(&store, id).await;
            let (_, page) = store.poll(id, None, None).await.unwrap().unwrap();
            assert_eq!(page.lines, vec![format!("marker-{i}")], "log {id}");
        }
    }

    #[tokio::test]
    async fn slow_command_backgrounds_then_completes() {
        let store = store(Duration::from_millis(100));
        let r = store
            .run(
                "echo start; sleep 1; echo done".into(),
                None,
                None,
                false,
                false,
                None,
            )
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("slow command should background");
        };
        assert!(matches!(
            await_state(&store, &id).await,
            JobState::Exited { code: 0 }
        ));
        let (_, page) = store.poll(&id, None, None).await.unwrap().unwrap();
        assert_eq!(page.lines, ["start", "done"]);
    }

    /// K11: a death by signal is reported as such, not as `exited -1`.
    #[tokio::test]
    async fn a_signal_death_is_reported_by_signal() {
        let store = store(Duration::from_secs(5));
        let RunResult::Inline { state, .. } = store
            .run("kill -9 $$".into(), None, None, false, false, None)
            .await
            .unwrap()
        else {
            panic!("inline");
        };
        assert!(
            matches!(&state, JobState::Failed { error } if error == "killed by signal 9"),
            "{state:?}"
        );
    }

    #[tokio::test]
    async fn kill_reaches_descendants_and_records_the_kill() {
        let store = store(Duration::from_millis(100));
        let id = bg(&store, "sleep 300 & echo \"pid:$!\"; wait").await;
        let child = logged_pid(&store, &id).await;
        assert!(alive(&child), "descendant should be running");

        assert!(matches!(
            store.kill(&id).await.unwrap(),
            KillOutcome::Killed
        ));
        assert!(gone_soon(&child).await, "descendant survived the kill");
        assert!(
            matches!(await_state(&store, &id).await, JobState::Failed { error } if error == "killed")
        );
    }

    /// K2: an interactive bash ignores `SIGTERM`. Killing the job must not let
    /// it go on to run the next command once its foreground child dies.
    #[tokio::test]
    async fn killing_an_interactive_job_never_runs_its_next_command() {
        let dir = tempfile::tempdir().unwrap().keep();
        let rc = dir.join("rc");
        std::fs::write(&rc, "").unwrap();
        let store = JobStore::new(
            dir.join("jobs"),
            Duration::from_millis(100),
            Shell::bash_with_rcfile(rc.to_str().unwrap()),
            Db::memory(),
        )
        .unwrap();
        let RunResult::Backgrounded { id } = store
            .run(
                "echo started; sleep 30; echo AFTER".into(),
                None,
                None,
                true,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("bg");
        };
        for _ in 0..100 {
            let (_, page) = store.poll(&id, None, None).await.unwrap().unwrap();
            if page.lines.iter().any(|l| l == "started") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(matches!(
            store.kill(&id).await.unwrap(),
            KillOutcome::Killed
        ));
        await_state(&store, &id).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (_, page) = store.poll(&id, None, None).await.unwrap().unwrap();
        assert!(!page.lines.iter().any(|l| l == "AFTER"), "{:?}", page.lines);
    }

    /// K12: a CLI kill of a job this server tracks records its reason; the
    /// server's own exit doesn't overwrite it, and polling says the same.
    #[tokio::test]
    async fn a_cli_kill_s_reason_is_the_final_state_everywhere() {
        let store = store(Duration::from_millis(100));
        let id = bg(&store, "sleep 300").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(matches!(
            kill_persisted(&store.repo, &id, "killed via mcp-ssh kill")
                .await
                .unwrap(),
            KillOutcome::Killed
        ));
        let state = await_state(&store, &id).await;
        for _ in 0..50 {
            let (state, _) = store.poll(&id, None, None).await.unwrap().unwrap();
            if matches!(&state, JobState::Failed { error } if error == "killed via mcp-ssh kill") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("poll never adopted the recorded reason: {state:?}");
    }

    #[tokio::test]
    async fn kill_escalates_to_sigkill_when_term_ignored() {
        let store = store(Duration::from_millis(100));
        let id = bg(&store, "trap '' TERM; while true; do sleep 1; done").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(matches!(
            store.kill(&id).await.unwrap(),
            KillOutcome::Killed
        ));
        assert!(matches!(
            await_state(&store, &id).await,
            JobState::Failed { .. }
        ));
    }

    #[tokio::test]
    async fn kill_unknown_and_finished_say_which() {
        let store = store(Duration::from_secs(5));
        assert!(matches!(
            store.kill(&JobId::from("nope")).await.unwrap(),
            KillOutcome::Unknown
        ));
        let RunResult::Inline { id, .. } = store
            .run("echo bye".into(), None, None, false, false, None)
            .await
            .unwrap()
        else {
            panic!("inline");
        };
        assert!(matches!(
            store.kill(&id).await.unwrap(),
            KillOutcome::NotRunning { status } if status == "exited"
        ));
    }

    /// K4: the leader exited, so the job is finished — but the server it started
    /// with `&` is still in its group. Kill reaches it.
    #[tokio::test]
    async fn kill_of_a_finished_job_reaches_what_it_left_running() {
        let store = store(Duration::from_secs(5));
        let RunResult::Inline { id, state, page } = store
            .run(
                "sleep 300 & echo \"pid:$!\"".into(),
                None,
                None,
                false,
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("the leader exits at once");
        };
        assert!(matches!(state, JobState::Exited { code: 0 }));
        let pid = page.lines[0].strip_prefix("pid:").unwrap().to_string();
        assert!(alive(&pid));
        assert!(matches!(
            store.kill(&id).await.unwrap(),
            KillOutcome::KilledLeftovers
        ));
        assert!(gone_soon(&pid).await);
    }

    /// A job whose process group outlived the process that started it is tracked
    /// only by its DB row — `kill` must reach it through the persisted pgid.
    #[tokio::test]
    async fn kill_reaches_a_job_that_outlived_a_restart() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id().expect("child pid");
        let waiter = tokio::spawn(async move { child.wait().await });
        let started = crate::db::now_unix() - 3600;
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix, pgid) \
                 VALUES ('survivor', 'running', ?1, ?2)",
                rusqlite::params![started, i64::from(pid)],
            )
        })
        .await
        .unwrap();

        let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        assert!(matches!(
            store.kill(&JobId::from("survivor")).await.unwrap(),
            KillOutcome::Killed
        ));
        let status = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the signalled group must exit")
            .unwrap()
            .unwrap();
        assert!(!status.success(), "the sleeper died by signal");
        assert_eq!(row_status(&db, "survivor").await.as_deref(), Some("failed"));
    }

    /// A refused kill (no pgid, or a corrupt one) records nothing. `0` is the
    /// dangerous value — it would signal this test runner's own group — so this
    /// test is its own canary.
    #[tokio::test]
    async fn kill_of_an_unsignalable_persisted_job_changes_nothing() {
        let store = store(Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
        for (id, pgid) in [
            ("legacy", None),
            ("zero", Some(0_i64)),
            ("negative", Some(-1)),
        ] {
            store
                .repo
                .db()
                .call(move |conn| {
                    conn.execute(
                        "INSERT INTO jobs (id, status, started_unix, pgid) VALUES (?1, 'running', 1, ?2)",
                        rusqlite::params![id, pgid],
                    )
                })
                .await
                .unwrap();
            let outcome = store.kill(&JobId::from(id)).await.unwrap();
            assert!(
                matches!(
                    outcome,
                    KillOutcome::NoProcessGroup | KillOutcome::CorruptProcessGroup { .. }
                ),
                "{id}: {outcome:?}"
            );
            assert_eq!(
                row_status(store.repo.db(), id).await.as_deref(),
                Some("running")
            );
        }
    }

    /// B2: a job writing past the live log cap is killed and says why; the log
    /// stops near the cap instead of filling the disk. The writer is bounded.
    #[tokio::test]
    async fn a_job_past_the_log_cap_is_killed_and_says_why() {
        let mut store = store(Duration::from_millis(100));
        store.log_cap = 64 * 1024;
        let id = bg(
            &store,
            "i=0; while [ $i -lt 400 ]; do head -c 4096 /dev/zero | tr '\\0' x; echo; i=$((i+1)); sleep 0.01; done",
        )
        .await;
        let state = await_state(&store, &id).await;
        assert!(
            matches!(&state, JobState::Failed { error } if error == LOG_CAP_REASON),
            "{state:?}"
        );
        let size = std::fs::metadata(store.dir.join(format!("{id}.log")))
            .unwrap()
            .len();
        assert!(size < 400 * 4097, "stopped early: {size}");
        assert!(size <= store.log_cap + 256 * 1024, "near the cap: {size}");
    }

    /// K6: a request cancelled after the spawn — here while the row insert waits
    /// on a busy database — still leaves a job that finishes and is recorded.
    #[tokio::test]
    async fn a_cancelled_launch_still_finishes_its_job() {
        let store = store(Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
        let db = store.repo.db().clone();
        let blocker = tokio::spawn(async move {
            db.call(|_| {
                std::thread::sleep(Duration::from_millis(500));
                Ok(())
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let cancelled = tokio::time::timeout(
            Duration::from_millis(100),
            store.run(
                "echo survived".into(),
                None,
                None,
                false,
                false,
                Some("cancel".into()),
            ),
        )
        .await;
        assert!(cancelled.is_err(), "the launch was cancelled mid-flight");
        blocker.await.unwrap().unwrap();

        let mut found = None;
        for _ in 0..100 {
            if let Some(job) = store.list(0, None).await.unwrap().jobs.into_iter().next() {
                found = Some(job.id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let id = found.expect("the cancelled launch is still a tracked job");
        let (status, code, _, _) = await_row(store.repo.db(), &id).await;
        assert_eq!((status.as_str(), code), ("exited", Some(0)));
        assert_eq!(
            await_tail(store.repo.db(), &id).await.as_deref(),
            Some("survived\n")
        );
    }

    /// K7: a job whose row can't be written is refused — stopped, untracked, its
    /// log removed — instead of running invisibly.
    #[tokio::test]
    async fn a_job_whose_row_cannot_be_written_is_refused() {
        let store = store(Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
        store
            .repo
            .db()
            .call(|c| c.execute_batch("DROP TABLE jobs"))
            .await
            .unwrap();
        let result = store
            .run("sleep 300".into(), None, None, true, false, None)
            .await;
        assert!(matches!(result, Err(JobError::Store(_))));
        assert!(store.jobs.lock().await.is_empty(), "not tracked");
        assert_eq!(
            std::fs::read_dir(&store.dir).unwrap().count(),
            0,
            "log removed"
        );
    }

    /// K13: the reaper never kills or evicts a running job, however old.
    #[tokio::test]
    async fn the_reaper_never_reaps_a_running_job() {
        let store = store(Duration::from_millis(100));
        let id = bg(&store, "sleep 300 & echo \"pid:$!\"; wait").await;
        let child = logged_pid(&store, &id).await;

        // Negative retention: every row counts as aged out this pass.
        reaper::reap_once(&store.jobs, &store.repo, &store.dir, -1).await;

        assert!(alive(&child), "a running job is never killed for age");
        assert!(store.poll(&id, None, None).await.unwrap().is_some());
        assert!(matches!(
            store.kill(&id).await.unwrap(),
            KillOutcome::Killed
        ));
    }

    /// B6: newest first by start time, paged, with a cursor to the next page.
    #[tokio::test]
    async fn list_pages_newest_first() {
        let store = store(Duration::from_secs(5));
        for title in ["a", "b", "c"] {
            store
                .run("true".into(), None, None, false, false, Some(title.into()))
                .await
                .unwrap();
            // Distinct start seconds order the rows.
            tokio::time::sleep(Duration::from_millis(1100)).await;
        }
        let first = store.list(0, Some(2)).await.unwrap();
        let labels: Vec<&str> = first.jobs.iter().map(|j| &j.id.as_ref()[..1]).collect();
        assert_eq!(labels, ["c", "b"]);
        assert_eq!(first.next_cursor, Some(2));
        let rest = store.list(2, Some(2)).await.unwrap();
        assert_eq!(rest.jobs.len(), 1);
        assert_eq!(rest.next_cursor, None);
    }

    #[tokio::test]
    async fn list_and_poll_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let store =
            JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        let RunResult::Inline { id, .. } = store
            .run(
                "echo tail-line; exit 7".into(),
                None,
                None,
                false,
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("inline");
        };
        let (status, code, _, _) = await_row(&db, &id).await;
        assert_eq!((status.as_str(), code), ("exited", Some(7)));
        assert_eq!(await_tail(&db, &id).await.as_deref(), Some("tail-line\n"));

        // A fresh store on the same DB + dir, and the log gone: the saved tail.
        let store2 = JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db).unwrap();
        assert!(
            store2
                .list(0, None)
                .await
                .unwrap()
                .jobs
                .iter()
                .any(|j| j.id == id)
        );
        tokio::fs::remove_file(dir.join(format!("{id}.log")))
            .await
            .unwrap();
        let (state, page) = store2.poll(&id, None, None).await.unwrap().unwrap();
        assert!(matches!(state, JobState::Exited { code: 7 }));
        assert_eq!(page.lines, ["tail-line"]);
    }

    #[tokio::test]
    async fn startup_reconcile_marks_dead_running_rows_failed_and_keeps_survivors() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 300")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = i64::from(child.id().expect("child pid"));
        let now = crate::db::now_unix();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('survivor', 'running', ?1, ?2)",
                rusqlite::params![now - 3600, pgid],
            )?;
            // Same second as this boot (a crash loop): just as dead.
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix) VALUES ('same-sec', 'running', ?1)",
                [now],
            )?;
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('ghost', 'running', ?1, 2000000000)",
                [now - 3600],
            )?;
            // A live pgid recorded under another boot names nothing of ours.
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix, pgid, boot_id) \
                 VALUES ('rebooted', 'running', ?1, ?2, 'another-boot')",
                rusqlite::params![now - 3600, pgid],
            )
        })
        .await
        .unwrap();

        let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        for dead in ["ghost", "same-sec"] {
            let (status, _, error, _) = await_row(&db, &JobId::from(dead)).await;
            assert_eq!(
                (status.as_str(), error.as_deref()),
                ("failed", Some("server restarted"))
            );
        }
        if signal::boot_id().is_some() {
            assert_eq!(await_row(&db, &JobId::from("rebooted")).await.0, "failed");
        }
        assert_eq!(
            row_status(&db, "survivor").await.as_deref(),
            Some("running")
        );
        let _ = child.kill().await;
        drop(store);
    }

    #[tokio::test]
    async fn restart_never_reuses_a_retained_job_id() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let store =
            JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        let RunResult::Inline { id: first, .. } = store
            .run(
                "echo first".into(),
                None,
                None,
                false,
                false,
                Some("t".into()),
            )
            .await
            .unwrap()
        else {
            panic!("inline");
        };
        await_row(&db, &first).await;
        let store2 = JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db).unwrap();
        let second = match store2
            .run(
                "echo second".into(),
                None,
                None,
                false,
                false,
                Some("t".into()),
            )
            .await
            .unwrap()
        {
            RunResult::Inline { id, .. } | RunResult::Backgrounded { id } => id,
        };
        assert_ne!(first, second, "retained id must never be reused");
        let old_log = tokio::fs::read_to_string(dir.join(format!("{first}.log")))
            .await
            .unwrap();
        assert!(old_log.contains("first"));
    }

    #[tokio::test]
    async fn poll_surfaces_read_error_while_job_is_tracked() {
        let store = store(Duration::from_secs(5));
        let job = Arc::new(Job {
            log_path: store.dir.join("gone.log"), // never created
            pgid: None,
            interactive: false,
            state: Arc::new(Mutex::new(JobState::Running)),
            kill_reason: OnceLock::new(),
        });
        let id = JobId::from("gone");
        store.jobs.lock().await.insert(id.clone(), job);
        assert!(matches!(
            store.poll(&id, None, None).await,
            Err(JobError::Log(_))
        ));
    }

    /// D4: a store failure is an error, not "no such job" / "no jobs".
    #[tokio::test]
    async fn store_failures_surface_as_errors() {
        let store = store(Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
        store
            .repo
            .db()
            .call(|c| c.execute_batch("DROP TABLE jobs"))
            .await
            .unwrap();
        assert!(matches!(store.list(0, None).await, Err(JobError::Store(_))));
        assert!(matches!(
            store.poll(&JobId::from("x"), None, None).await,
            Err(JobError::Store(_))
        ));
        assert!(matches!(
            store.kill(&JobId::from("x")).await,
            Err(JobError::Store(_))
        ));
    }
}
