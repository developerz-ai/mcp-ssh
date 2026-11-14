//! Launching a command: reserve its id + log, spawn it in its own process group,
//! hand it to the supervisor, and wait out the inline window.
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use tokio::sync::{Mutex, oneshot, watch};

use super::log::{DEFAULT_PAGE, TailBudget, read_tail};
use super::supervise::{Supervised, supervise};
use super::{
    Job, JobError, JobId, JobMeta, JobState, JobStore, ProcessGroupId, RunResult, Shell, id,
};

/// Single-quote a path for safe interpolation into a shell command. The job log
/// path is engine-controlled, but quoting keeps an odd `job_dir` (spaces, `$`)
/// from breaking the `exec` redirect in `run`.
fn sh_single_quote(path: &Path) -> String {
    let escaped = path.to_string_lossy().replace('\'', r"'\''");
    format!("'{escaped}'")
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

impl JobStore {
    /// Spawn `cmd`. With `background`, return a job id immediately; otherwise wait
    /// up to the inline window and return output if it finishes in time.
    /// `interactive` runs it through `~/.bashrc` (aliases, version managers);
    /// otherwise the fast bare `sh -c` is used. `meta` names the job (`title`) and
    /// the agent it is recorded as started by.
    pub async fn run(
        &self,
        cmd: String,
        cwd: Option<String>,
        timeout_secs: Option<u64>,
        background: bool,
        interactive: bool,
        meta: JobMeta,
    ) -> Result<RunResult, JobError> {
        if let Some(dir) = &cwd {
            check_cwd(dir).await?;
        }
        // The id's label is the agent-supplied `title` (or the neutral `job`
        // fallback), never `cmd`: a command can carry a secret in its leading
        // tokens, and the id ends up in replies, `job(list)`, logs, and the log
        // filename. The normalized label is also what the row stores, so an
        // operator's terminal never renders a raw agent string.
        let label = meta.title.as_deref().and_then(id::normalize_title);
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
            kill_reason: Mutex::new(None),
        });

        // The supervisor owns the child from here, with no `.await` between the
        // spawn and handing it over: if this request is cancelled now, the job is
        // still tracked, persisted, reaped, and finished — never a phantom.
        let (started_tx, started_rx) = oneshot::channel();
        tokio::spawn(supervise(
            Supervised {
                id: id.clone(),
                job,
                meta: JobMeta {
                    title: label,
                    owner: meta.owner,
                },
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
}
