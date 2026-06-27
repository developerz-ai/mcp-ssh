//! Job engine: run a command, return inline if it's fast, otherwise hand back
//! a job id the caller polls. Output streams to a per-job log file so polling
//! can paginate it without holding everything in memory.
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use tokio::sync::{Mutex, watch};

mod id;
mod log;
mod reaper;

pub use id::JobId;
use log::{DEFAULT_PAGE, read_page};
pub use log::{JobLogError, Page, paginate};
use reaper::{kill_job, spawn_reaper};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobState {
    Running,
    Exited { code: i32 },
    Failed { error: String },
}

/// Process group id to signal on kill. Wrapping the raw pid keeps this
/// lifecycle/security boundary from being confused with any other `u32`.
#[derive(Debug, Clone, Copy)]
struct ProcessGroupId(u32);

struct Job {
    log_path: PathBuf,
    /// Process group to signal on kill. The child leads its own group, so this
    /// equals its pid (see `run`). `None` only if the OS withheld a pid.
    pgid: Option<ProcessGroupId>,
    state: Arc<Mutex<JobState>>,
    /// Flips to `true` when the process exits; lets `kill` wait out its grace
    /// period instead of polling.
    done: watch::Receiver<bool>,
    started: tokio::time::Instant,
}

/// Result of starting a command.
pub enum RunResult {
    /// Finished within the inline window — output is ready now. Carries the `id`
    /// too: the job is still in the store, so if the inline page is truncated the
    /// agent can poll `id` for the rest instead of losing it.
    Inline {
        id: JobId,
        state: JobState,
        page: Page,
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

/// How a user command is launched. The default is a bare `sh -c`; a `bash` call
/// that opts into `interactive` instead gets an interactive bash that sources
/// the service user's `~/.bashrc` — so aliases and version managers
/// (mise/nvm/rbenv) resolve, matching a real shell.
///
/// `program` plus `args` form the launcher prefix; `run` appends the (wrapped)
/// command string as the final argument.
#[derive(Debug, Clone)]
pub struct Shell {
    program: String,
    args: Vec<String>,
}

impl Shell {
    /// Bare `sh -c` — the default, fast path: no rc files, no per-call startup
    /// cost. Output never depends on the host's shell config.
    pub fn sh() -> Self {
        Self {
            program: "sh".into(),
            args: vec!["-c".into()],
        }
    }

    /// Interactive bash. `-i` sources `~/.bashrc`, where aliases and version
    /// managers live behind its `case $- in *i*) ;; *) return;; esac`
    /// non-interactive guard — so commands see the same environment an
    /// interactive shell does. Opt-in per call (`bash` tool's `interactive`
    /// flag) because sourcing `~/.bashrc` adds startup cost. Startup job-control
    /// warnings (no controlling TTY under systemd) are discarded by the
    /// exec-redirect in `run`.
    pub fn interactive_bash() -> Self {
        Self {
            program: "bash".into(),
            args: vec!["-ic".into()],
        }
    }
}

/// Single-quote a path for safe interpolation into a shell command. The job log
/// path is engine-controlled, but quoting keeps an odd `job_dir` (spaces, `$`)
/// from breaking the `exec` redirect in `run`.
fn sh_single_quote(path: &std::path::Path) -> String {
    let escaped = path.to_string_lossy().replace('\'', r"'\''");
    format!("'{escaped}'")
}

#[derive(Clone)]
pub struct JobStore {
    dir: PathBuf,
    inline_timeout: Duration,
    /// Shell used when a `bash` call opts into `interactive` (sources `~/.bashrc`).
    /// The default path uses a bare `sh -c` (`Shell::sh`).
    interactive_shell: Shell,
    seq: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>,
}

impl JobStore {
    pub fn new(
        dir: PathBuf,
        inline_timeout: Duration,
        interactive_shell: Shell,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let jobs = Arc::new(Mutex::new(HashMap::new()));
        spawn_reaper(jobs.clone());
        Ok(Self {
            dir,
            inline_timeout,
            interactive_shell,
            seq: Arc::new(AtomicU64::new(1)),
            jobs,
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
    ) -> std::io::Result<RunResult> {
        // Hold the jobs lock across id generation and insertion so the id is
        // reserved atomically: two jobs launched in the same second can't both
        // see their `<label>-HH:MM:SS` as free and clobber each other's entry.
        // Nothing awaits while the guard is held (file create + spawn are
        // synchronous), so the critical section stays short.
        //
        // The id's label is the agent-supplied `title` (or the neutral `job`
        // fallback), never `cmd`: a command can carry a secret in its leading
        // tokens (`mysql -psecret`, `PGPASSWORD=…`), and the id ends up in `bash`'s
        // reply, `job(list)`, reaper logs, and the log filename — so deriving it
        // from `cmd` would leak that secret. The title is normalized in `JobId`.
        let mut jobs = self.jobs.lock().await;
        let id = JobId::generate(&self.seq, title.as_deref(), |candidate| {
            jobs.contains_key(candidate)
        });
        let log_path = self.dir.join(format!("{id}.log"));

        // Create the log up front so a poll racing the spawn reads an empty page,
        // not a NotFound — the command appends to it (see `wrapped`).
        std::fs::File::create(&log_path)?;

        // An interactive bash (production shell) prints two job-control warnings
        // to stderr at startup when there's no controlling TTY (always, under
        // systemd). So the child's own stdio goes to /dev/null and the command
        // re-points stdout+stderr at the log itself, *after* startup: only the
        // command's output is captured, merged terminal-style. `sh -c` (tests)
        // runs the identical wrapper with no warnings to discard.
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
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command.spawn()?;
        let pgid = child.id().map(ProcessGroupId);
        let (tx, rx) = watch::channel(false);
        let state = Arc::new(Mutex::new(JobState::Running));

        // Waiter owns the child so it can reap it; updates shared state on exit.
        {
            let state = state.clone();
            tokio::spawn(async move {
                let result = match child.wait().await {
                    Ok(s) => JobState::Exited {
                        code: s.code().unwrap_or(-1),
                    },
                    Err(e) => JobState::Failed {
                        error: e.to_string(),
                    },
                };
                *state.lock().await = result;
                let _ = tx.send(true);
            });
        }

        let job = Arc::new(Job {
            log_path: log_path.clone(),
            pgid,
            state: state.clone(),
            done: rx.clone(),
            started: tokio::time::Instant::now(),
        });
        jobs.insert(id.clone(), job);
        drop(jobs);

        // `bg: true` — don't wait, hand back the id straight away.
        if background {
            return Ok(RunResult::Backgrounded { id });
        }

        // Wait for completion or the inline window, whichever comes first.
        let window = timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.inline_timeout);
        let mut done = rx;
        let _ = tokio::time::timeout(window, done.changed()).await;

        let current = state.lock().await.clone();
        match current {
            JobState::Running => Ok(RunResult::Backgrounded { id }),
            finished => Ok(RunResult::Inline {
                state: finished,
                page: read_page(&log_path, 0, DEFAULT_PAGE).await?,
                id,
            }),
        }
    }

    /// Status + one page of a job's log. `Ok(None)` means the id is unknown;
    /// `Err` means the log existed but could not be read — surfaced to the caller
    /// rather than collapsed into an empty page.
    pub async fn poll(
        &self,
        id: &JobId,
        cursor: usize,
        limit: Option<usize>,
    ) -> Result<Option<(JobState, Page)>, JobLogError> {
        let Some(job) = self.jobs.lock().await.get(id).cloned() else {
            return Ok(None);
        };
        let state = job.state.lock().await.clone();
        // A reaper pass can evict this id and delete its log between the clone
        // above and this read. If the log is gone *and* the id is no longer
        // tracked, that's the stable "unknown job" result, not a read fault —
        // re-check membership so an eviction race doesn't surface as an error.
        let page = match read_page(&job.log_path, cursor, limit.unwrap_or(DEFAULT_PAGE)).await {
            Ok(page) => page,
            Err(JobLogError::Read(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.jobs.lock().await.contains_key(id) {
                    return Err(JobLogError::Read(error));
                }
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        Ok(Some((state, page)))
    }

    pub async fn list(&self) -> Vec<JobSummary> {
        // Snapshot while holding the map lock, then drop it before any .await.
        let snapshot: Vec<(JobId, Arc<Job>)> = {
            let jobs = self.jobs.lock().await;
            jobs.iter()
                .map(|(id, job)| (id.clone(), Arc::clone(job)))
                .collect()
        };
        let mut out = Vec::with_capacity(snapshot.len());
        for (id, job) in snapshot {
            out.push(JobSummary {
                id,
                state: job.state.lock().await.clone(),
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Kill a running job by signalling its whole process group. Returns `false`
    /// when the id is unknown or the job already finished — nothing to signal.
    pub async fn kill(&self, id: &JobId) -> bool {
        let Some(job) = self.jobs.lock().await.get(id).cloned() else {
            return false;
        };
        kill_job(&job).await
    }
}

#[cfg(test)]
impl Shell {
    /// Interactive bash pinned to a specific rc file — lets a test prove alias
    /// resolution against a controlled rc instead of the host's `~/.bashrc`.
    fn bash_with_rcfile(rcfile: &str) -> Self {
        Self {
            program: "bash".into(),
            args: vec!["--rcfile".into(), rcfile.into(), "-ic".into()],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(inline: Duration) -> JobStore {
        let dir = tempfile::tempdir().unwrap().keep();
        JobStore::new(dir, inline, Shell::sh()).unwrap()
    }

    /// The production shell (interactive bash) must expand aliases defined in
    /// the sourced rc file — that's the whole point of `-i`. A controlled
    /// rcfile keeps the test independent of the host's `~/.bashrc`, and the
    /// startup job-control warnings must NOT leak into the captured log.
    #[tokio::test]
    async fn interactive_shell_expands_rc_aliases_without_leaking_startup_noise() {
        let dir = tempfile::tempdir().unwrap().keep();
        let rc = dir.join("rc");
        std::fs::write(&rc, "alias greet='echo ALIAS_OK'\n").unwrap();
        let store = JobStore::new(
            dir,
            Duration::from_secs(5),
            Shell::bash_with_rcfile(rc.to_str().unwrap()),
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
            "alias should expand: {:?}",
            page.lines
        );
        assert!(
            !page.lines.iter().any(|l| l.contains("no job control")),
            "startup job-control noise leaked into the log: {:?}",
            page.lines
        );
    }

    #[tokio::test]
    async fn fast_command_returns_inline() {
        let r = store(Duration::from_secs(5))
            .run("echo hello".into(), None, None, false, false, None)
            .await
            .unwrap();
        match r {
            RunResult::Inline { state, page, .. } => {
                assert!(matches!(state, JobState::Exited { code: 0 }));
                assert!(page.lines.iter().any(|l| l.contains("hello")));
            }
            RunResult::Backgrounded { .. } => panic!("fast command should be inline"),
        }
    }

    #[tokio::test]
    async fn title_labels_the_job_id() {
        let r = store(Duration::from_secs(5))
            .run(
                "echo hi".into(),
                None,
                None,
                true,
                false,
                Some("deploy check".into()),
            )
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };
        assert!(
            id.as_ref().starts_with("deploy-check-"),
            "title must prefix the id: {id}"
        );
    }

    #[tokio::test]
    async fn inline_overflow_surfaces_id_and_remains_pollable() {
        // A fast command whose output exceeds one page must still hand back an id
        // (in the Inline result) so the agent can poll the rest — not strand it.
        let store = store(Duration::from_secs(5));
        let n = DEFAULT_PAGE + 50;
        let r = store
            .run(format!("seq 1 {n}"), None, None, false, false, None)
            .await
            .unwrap();
        let RunResult::Inline { id, page, .. } = r else {
            panic!("seq is fast — should be inline");
        };
        assert!(page.has_more, "output exceeds one page");
        assert_eq!(page.lines.len(), DEFAULT_PAGE, "first page is capped");
        // The id is live in the store: poll the continuation.
        let (_state, rest) = store
            .poll(&id, page.next_cursor, None)
            .await
            .unwrap()
            .expect("inline job stays in the store");
        assert!(
            rest.lines.iter().any(|l| l == &n.to_string()),
            "the tail must be reachable via poll"
        );
    }

    #[tokio::test]
    async fn bg_flag_backgrounds_a_fast_command() {
        // Even though `echo` is instant, bg=true must return an id without waiting.
        let r = store(Duration::from_secs(5))
            .run("echo hi".into(), None, None, true, false, None)
            .await
            .unwrap();
        assert!(matches!(r, RunResult::Backgrounded { .. }));
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
        let id = match r {
            RunResult::Backgrounded { id } => id,
            RunResult::Inline { .. } => panic!("slow command should background"),
        };
        for _ in 0..50 {
            let (state, page) = store.poll(&id, 0, None).await.unwrap().unwrap();
            if matches!(state, JobState::Exited { .. }) {
                assert!(page.lines.iter().any(|l| l.contains("done")));
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("job never finished");
    }

    /// `kill -0` probes liveness without delivering a signal.
    #[cfg(unix)]
    async fn alive(pid: &str) -> bool {
        tokio::process::Command::new("kill")
            .arg("-0")
            .arg(pid)
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_reaches_descendants() {
        let store = store(Duration::from_millis(100));
        // `sh` backgrounds a long sleep, prints its pid, then waits on it. Job
        // control is off in `sh -c`, so the child shares the shell's group.
        let r = store
            .run(
                "sleep 300 & echo \"pid:$!\"; wait".into(),
                None,
                None,
                true,
                false,
                None,
            )
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };

        // Pull the descendant's pid out of the log.
        let mut child_pid = None;
        for _ in 0..50 {
            let (_s, page) = store.poll(&id, 0, None).await.unwrap().unwrap();
            if let Some(line) = page.lines.iter().find_map(|l| l.strip_prefix("pid:")) {
                child_pid = Some(line.trim().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let child_pid = child_pid.expect("never saw the child pid");
        assert!(alive(&child_pid).await, "descendant should be running");

        assert!(store.kill(&id).await);

        // Group kill must reap the descendant, not just `sh`.
        for _ in 0..50 {
            if !alive(&child_pid).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("descendant survived the kill");
    }

    #[tokio::test]
    async fn kill_unknown_id_returns_false() {
        assert!(
            !store(Duration::from_secs(5))
                .kill(&JobId::from("nope"))
                .await
        );
    }

    #[tokio::test]
    async fn kill_finished_job_returns_false() {
        let store = store(Duration::from_secs(5));
        // Runs inline, so it has already exited by the time `run` returns.
        let r = store
            .run("echo bye".into(), None, None, false, false, None)
            .await
            .unwrap();
        assert!(matches!(r, RunResult::Inline { .. }));
        // Inline doesn't surface the id; fetch the finished job's id from the
        // listing. Killing an already-exited job must return false.
        let id = store.list().await.first().expect("job tracked").id.clone();
        assert!(!store.kill(&id).await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_escalates_to_sigkill_when_term_ignored() {
        let store = store(Duration::from_millis(100));
        // The shell traps (ignores) TERM, and the ignore disposition is inherited
        // by its children, so only KILL can reap the group.
        let r = store
            .run(
                "trap '' TERM; while true; do sleep 1; done".into(),
                None,
                None,
                true,
                false,
                None,
            )
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(store.kill(&id).await);

        // TERM is ignored; the post-grace KILL must still bring it down.
        for _ in 0..100 {
            let (state, _) = store.poll(&id, 0, None).await.unwrap().unwrap();
            if matches!(state, JobState::Exited { .. }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("job survived TERM->KILL escalation");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reaper_kills_running_job_before_eviction() {
        let store = store(Duration::from_millis(100));
        // Backgrounded descendant; print its pid so we can probe it post-eviction.
        let r = store
            .run(
                "sleep 300 & echo \"pid:$!\"; wait".into(),
                None,
                None,
                true,
                false,
                None,
            )
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };

        let mut child_pid = None;
        for _ in 0..50 {
            let (_s, page) = store.poll(&id, 0, None).await.unwrap().unwrap();
            if let Some(line) = page.lines.iter().find_map(|l| l.strip_prefix("pid:")) {
                child_pid = Some(line.trim().to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let child_pid = child_pid.expect("never saw the child pid");
        assert!(alive(&child_pid).await, "descendant should be running");

        // Retention zero => the just-started job is already stale.
        reaper::reap_once(&store.jobs, Duration::ZERO).await;

        // Evicted from the map (poll can't find it)...
        assert!(
            store.poll(&id, 0, None).await.unwrap().is_none(),
            "job should be evicted"
        );
        // ...and its process group reaped, not orphaned.
        for _ in 0..50 {
            if !alive(&child_pid).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("descendant survived eviction");
    }

    #[tokio::test]
    async fn reaper_keeps_running_job_when_kill_fails() {
        let store = store(Duration::from_millis(100));
        // A job with no pgid can't be signalled, so kill always fails while the
        // job still reads as Running. Evicting it would orphan a live group and
        // delete its log, so the reaper must keep it tracked for a later pass.
        let (_tx, rx) = watch::channel(false);
        let log_path = store.dir.join("jfake.log");
        tokio::fs::write(&log_path, "running\n").await.unwrap();
        let job = Arc::new(Job {
            log_path: log_path.clone(),
            pgid: None,
            state: Arc::new(Mutex::new(JobState::Running)),
            done: rx,
            started: tokio::time::Instant::now() - Duration::from_secs(1),
        });
        let id = JobId::from("jfake");
        store.jobs.lock().await.insert(id.clone(), job);

        // The backdated job is stale; kill fails => must not be evicted.
        reaper::reap_once(&store.jobs, Duration::ZERO).await;

        assert!(
            store.poll(&id, 0, None).await.unwrap().is_some(),
            "running job whose kill failed must stay tracked"
        );
        assert!(log_path.exists(), "its log must not be deleted");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_terminates_child_process() {
        let store = store(Duration::from_millis(100));
        let r = store
            .run("sleep 1000".into(), None, None, true, false, None)
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };
        // Give the process a moment to start before we try to kill it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            store.kill(&id).await,
            "kill should return true for a running job"
        );

        // After kill the state must transition away from Running.
        for _ in 0..50 {
            let (state, _) = store.poll(&id, 0, None).await.unwrap().unwrap();
            if !matches!(state, JobState::Running) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("job remained Running after kill");
    }

    #[tokio::test]
    async fn list_reports_all_jobs() {
        let store = store(Duration::from_secs(5));
        // Two commands — one inline, one explicitly backgrounded — both tracked.
        store
            .run("echo alpha".into(), None, None, false, false, None)
            .await
            .unwrap();
        store
            .run("echo beta".into(), None, None, true, false, None)
            .await
            .unwrap();

        let jobs = store.list().await;
        assert_eq!(jobs.len(), 2, "expected two jobs, got {}", jobs.len());
        // IDs must be sorted so the list is deterministic.
        assert!(jobs[0].id < jobs[1].id, "list should be sorted by id");
    }

    #[tokio::test]
    async fn poll_paginates() {
        let store = store(Duration::from_secs(5));
        store
            .run("seq 1 10".into(), None, None, false, false, None)
            .await
            .unwrap();
        // seq finishes inline; fetch its id from the listing to re-poll and
        // exercise pagination.
        let id = store.list().await.first().expect("job tracked").id.clone();
        let (_s, page) = store.poll(&id, 0, Some(3)).await.unwrap().unwrap();
        assert_eq!(page.lines.len(), 3);
        assert_eq!(page.next_cursor, 3);
        assert!(page.has_more);
        assert_eq!(page.total_lines, 10);
    }

    #[tokio::test]
    async fn poll_surfaces_read_error_while_job_is_tracked() {
        let store = store(Duration::from_secs(5));
        // A tracked job whose log has vanished is a genuine read fault, not an
        // eviction race: poll must surface the error, never collapse it to
        // Ok(None) (the NotFound -> Ok(None) mapping applies only once the id is
        // gone from the map).
        let (_tx, rx) = watch::channel(false);
        let log_path = store.dir.join("gone.log"); // never created
        let job = Arc::new(Job {
            log_path,
            pgid: None,
            state: Arc::new(Mutex::new(JobState::Running)),
            done: rx,
            started: tokio::time::Instant::now(),
        });
        let id = JobId::from("gone");
        store.jobs.lock().await.insert(id.clone(), job);

        assert!(
            store.poll(&id, 0, None).await.is_err(),
            "missing log for a still-tracked job must surface as an error"
        );
    }
}
