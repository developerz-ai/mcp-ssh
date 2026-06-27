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

use rusqlite::OptionalExtension;
use tokio::sync::{Mutex, watch};

mod id;
mod log;
mod reaper;

pub use id::JobId;
use log::{DEFAULT_PAGE, read_page};
pub use log::{JobLogError, Page, paginate};
use reaper::{kill_job, spawn_reaper};

/// Lines of a finished job's output snapshotted into the DB. Bounds the row so the
/// tail survives the live log being trimmed/reaped without bloating SQLite.
const TAIL_LINES: usize = 500;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobState {
    Running,
    Exited { code: i32 },
    Failed { error: String },
}

/// Map a `JobState` onto the DB row's `(status, code, error)` columns.
fn state_columns(state: &JobState) -> (&'static str, Option<i32>, Option<String>) {
    match state {
        JobState::Running => ("running", None, None),
        JobState::Exited { code } => ("exited", Some(*code), None),
        JobState::Failed { error } => ("failed", None, Some(error.clone())),
    }
}

/// Rebuild a `JobState` from a DB row's columns. An unrecognized status (a corrupt
/// row) reads as `Failed` so the anomaly surfaces rather than masquerading as a
/// live or cleanly-exited job.
fn state_from_columns(status: &str, code: Option<i32>, error: Option<String>) -> JobState {
    match status {
        "running" => JobState::Running,
        "exited" => JobState::Exited {
            code: code.unwrap_or(-1),
        },
        _ => JobState::Failed {
            error: error.unwrap_or_else(|| status.to_string()),
        },
    }
}

/// Record a finished job's final state and a bounded output tail into the DB, so
/// `list`/`poll` reflect it across a restart and the tail outlives the live log
/// being trimmed or reaped. Best effort: failures are logged, never propagated —
/// the live log file remains the source of truth while it exists.
async fn persist_final(
    db: &crate::db::Db,
    id: &JobId,
    log_path: &std::path::Path,
    state: &JobState,
) {
    let (status, code, error) = state_columns(state);
    // A tail we can't read just stays empty; the row still records the status.
    let tail = log::tail(log_path, TAIL_LINES).await.unwrap_or_default();
    let row_id = id.as_ref().to_string();
    if let Err(e) = db
        .call(move |conn| {
            conn.execute(
                "UPDATE jobs SET status = ?1, code = ?2, error = ?3, output_tail = ?4 \
                 WHERE id = ?5",
                rusqlite::params![status, code, error, tail, row_id],
            )
        })
        .await
    {
        tracing::warn!(error = %e, id = %id, "failed to persist final job state");
    }
}

/// Render a saved output tail (already bounded at write time) as a single terminal
/// page: there is no live log left to fetch, so `has_more` is false.
fn page_from_tail(tail: &str) -> Page {
    let lines: Vec<String> = tail.lines().map(str::to_string).collect();
    let total = lines.len();
    Page {
        lines,
        next_cursor: total,
        total_lines: total,
        has_more: false,
    }
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
    /// Durable metadata + saved output tails, so `list`/`poll` show history across
    /// restarts. Live output still streams to the per-job log files.
    db: crate::db::Db,
}

impl JobStore {
    pub fn new(
        dir: PathBuf,
        inline_timeout: Duration,
        interactive_shell: Shell,
        db: crate::db::Db,
    ) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let jobs = Arc::new(Mutex::new(HashMap::new()));

        // Startup reconcile: a row left `running` by a previous process can't still
        // be running — that process (and its children) died with it. Flip those to
        // `failed` so history is truthful. Scoped to `started_unix < boot`: only
        // rows from before this process started qualify, so a job *this* process
        // launches (started_unix >= boot) is never clobbered by the racing update.
        // `new` is sync, so the reconcile is spawned; the reaper only touches rows
        // older than its retention, so it won't race this for recent rows.
        let boot = crate::db::now_unix();
        {
            let db = db.clone();
            tokio::spawn(async move {
                if let Err(error) = db
                    .call(move |conn| {
                        conn.execute(
                            "UPDATE jobs SET status = 'failed', error = 'server restarted' \
                             WHERE status = 'running' AND started_unix < ?1",
                            [boot],
                        )
                    })
                    .await
                {
                    tracing::warn!(%error, "startup job reconcile failed");
                }
            });
        }

        spawn_reaper(jobs.clone(), db.clone(), dir.clone());
        Ok(Self {
            dir,
            inline_timeout,
            interactive_shell,
            seq: Arc::new(AtomicU64::new(1)),
            jobs,
            db,
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

        let job = Arc::new(Job {
            log_path: log_path.clone(),
            pgid,
            state: state.clone(),
            done: rx.clone(),
        });
        jobs.insert(id.clone(), job);
        drop(jobs);

        // Persist durable metadata now the in-memory entry exists and the lock is
        // released (the DB write must not block the id-reservation critical
        // section). The row starts `running`; the waiter records the final state +
        // a bounded output tail on exit.
        let started = crate::db::now_unix();
        {
            let db = self.db.clone();
            let row_id = id.as_ref().to_string();
            if let Err(error) = db
                .call(move |conn| {
                    conn.execute(
                        "INSERT INTO jobs (id, title, status, code, error, started_unix, output_tail) \
                         VALUES (?1, ?2, 'running', NULL, NULL, ?3, NULL)",
                        rusqlite::params![row_id, title, started],
                    )
                })
                .await
            {
                tracing::warn!(%error, id = %id, "failed to persist job row");
            }
        }

        // Waiter owns the child so it can reap it; updates shared state on exit,
        // then persists the final state + tail. Spawned *after* the INSERT so the
        // row exists before this UPDATE runs (a fast command can exit immediately).
        {
            let state = state.clone();
            let db = self.db.clone();
            let log_path = log_path.clone();
            let id = id.clone();
            tokio::spawn(async move {
                let result = match child.wait().await {
                    Ok(s) => JobState::Exited {
                        code: s.code().unwrap_or(-1),
                    },
                    Err(e) => JobState::Failed {
                        error: e.to_string(),
                    },
                };
                *state.lock().await = result.clone();
                // Wake any inline waiter / kill grace BEFORE the DB write: a slow
                // DB must never delay the inline window (which would misreport a
                // fast command as backgrounded).
                let _ = tx.send(true);
                persist_final(&db, &id, &log_path, &result).await;
            });
        }

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
        // Live in this process: stream straight from the active log file. Bind the
        // lookup to a local so the `jobs` guard is released at this `;` — otherwise
        // it lives to the end of the `if let` block (temporary-in-scrutinee rule)
        // and the re-lock in the NotFound arm below would deadlock against itself.
        let tracked = self.jobs.lock().await.get(id).cloned();
        if let Some(job) = tracked {
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
            return Ok(Some((state, page)));
        }
        // Not live here — fall back to the durable row so finished jobs (and jobs
        // from a previous process) stay pollable.
        self.poll_persisted(id, cursor, limit).await
    }

    /// Poll a job that isn't tracked in this process from its DB row. Reads the
    /// live log file if it still exists (full pagination); once it's been trimmed
    /// or reaped, serves the saved tail. `Ok(None)` if there's no such row.
    async fn poll_persisted(
        &self,
        id: &JobId,
        cursor: usize,
        limit: Option<usize>,
    ) -> Result<Option<(JobState, Page)>, JobLogError> {
        let row_id = id.as_ref().to_string();
        let row = self
            .db
            .call(move |conn| {
                conn.query_row(
                    "SELECT status, code, error, output_tail FROM jobs WHERE id = ?1",
                    [row_id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, Option<i64>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ))
                    },
                )
                .optional()
            })
            .await;
        let (status, code, error, tail) = match row {
            Ok(Some(row)) => row,
            Ok(None) => return Ok(None),
            Err(e) => {
                tracing::warn!(error = %e, id = %id, "failed to read job row");
                return Ok(None);
            }
        };
        let state = state_from_columns(&status, code.map(|c| c as i32), error);
        let log_path = self.dir.join(format!("{id}.log"));
        let page = match read_page(&log_path, cursor, limit.unwrap_or(DEFAULT_PAGE)).await {
            Ok(page) => page,
            Err(JobLogError::Read(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                page_from_tail(tail.as_deref().unwrap_or(""))
            }
            Err(error) => return Err(error),
        };
        Ok(Some((state, page)))
    }

    pub async fn list(&self) -> Vec<JobSummary> {
        // History lives in the DB, so `list` reflects finished jobs and jobs from a
        // previous process — not just what this process currently tracks.
        let rows = self
            .db
            .call(|conn| {
                let mut stmt =
                    conn.prepare("SELECT id, status, code, error FROM jobs ORDER BY id")?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await;
        match rows {
            Ok(rows) => rows
                .into_iter()
                .map(|(id, status, code, error)| JobSummary {
                    id: JobId::from(id),
                    state: state_from_columns(&status, code.map(|c| c as i32), error),
                })
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "failed to list jobs");
                Vec::new()
            }
        }
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
    use crate::db::Db;
    use rusqlite::OptionalExtension;

    fn store(inline: Duration) -> JobStore {
        let dir = tempfile::tempdir().unwrap().keep();
        JobStore::new(dir, inline, Shell::sh(), Db::memory()).unwrap()
    }

    /// Wait for the waiter task to persist a job's final state (it does so after
    /// returning from the inline window, asynchronously). Returns the finished
    /// row's `(status, code, output_tail)`.
    async fn await_row(db: &Db, id: &JobId) -> (String, Option<i64>, Option<String>) {
        for _ in 0..100 {
            let row_id = id.as_ref().to_string();
            let row = db
                .call(move |conn| {
                    conn.query_row(
                        "SELECT status, code, output_tail FROM jobs WHERE id = ?1",
                        [row_id],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, Option<i64>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )
                    .optional()
                })
                .await
                .unwrap();
            if let Some((status, code, tail)) = row {
                if status != "running" {
                    return (status, code, tail);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("job row never reached a finished state");
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

        // Negative retention => cutoff is in the future, so the just-started job
        // (its DB row written by `run`) counts as stale this pass.
        reaper::reap_once(&store.jobs, &store.db, &store.dir, -1).await;

        // Evicted from the map and its row deleted (poll can't find it)...
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
        });
        let id = JobId::from("jfake");
        store.jobs.lock().await.insert(id.clone(), job);
        // A matching running row so the DB-driven reaper actually considers it.
        store
            .db
            .call(|conn| {
                conn.execute(
                    "INSERT INTO jobs (id, status, started_unix) VALUES ('jfake', 'running', ?1)",
                    [crate::db::now_unix()],
                )
            })
            .await
            .unwrap();

        // The job is stale (negative retention); kill fails => must not be evicted.
        reaper::reap_once(&store.jobs, &store.db, &store.dir, -1).await;

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
    async fn fast_command_persists_exited_row_with_tail() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        let r = store
            .run(
                "echo persisted; exit 7".into(),
                None,
                None,
                false,
                false,
                None,
            )
            .await
            .unwrap();
        let RunResult::Inline { id, .. } = r else {
            panic!("fast command should be inline");
        };
        // The waiter persists asynchronously after the inline window returns.
        let (status, code, tail) = await_row(&db, &id).await;
        assert_eq!(status, "exited");
        assert_eq!(code, Some(7), "exit code persisted");
        assert!(
            tail.unwrap_or_default().contains("persisted"),
            "output tail must be saved into the row"
        );
    }

    #[tokio::test]
    async fn list_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let store =
            JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        let r = store
            .run(
                "echo hi".into(),
                None,
                None,
                false,
                false,
                Some("restart-job".into()),
            )
            .await
            .unwrap();
        let RunResult::Inline { id, .. } = r else {
            panic!("fast command should be inline");
        };
        await_row(&db, &id).await;

        // Simulate a restart: a fresh store on the same DB + dir, empty in-mem map.
        let store2 = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db).unwrap();
        let listed = store2.list().await;
        assert!(
            listed.iter().any(|j| j.id == id),
            "history must survive a restart: {:?}",
            listed.iter().map(|j| j.id.as_ref()).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn poll_finished_job_not_in_memory_returns_tail() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        let store =
            JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        let r = store
            .run("echo tail-line".into(), None, None, false, false, None)
            .await
            .unwrap();
        let RunResult::Inline { id, .. } = r else {
            panic!("fast command should be inline");
        };
        await_row(&db, &id).await;

        // Restart with no in-mem entry, and remove the log so only the saved tail
        // remains — poll must serve that.
        let store2 = JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db).unwrap();
        tokio::fs::remove_file(dir.join(format!("{id}.log")))
            .await
            .unwrap();
        let (state, page) = store2
            .poll(&id, 0, None)
            .await
            .unwrap()
            .expect("finished job must stay pollable from history");
        assert!(matches!(state, JobState::Exited { code: 0 }));
        assert!(
            page.lines.iter().any(|l| l.contains("tail-line")),
            "saved tail must come back: {:?}",
            page.lines
        );
        assert!(!page.has_more, "the tail is a single terminal page");
    }

    #[tokio::test]
    async fn startup_reconcile_marks_stale_running_failed() {
        let dir = tempfile::tempdir().unwrap().keep();
        let db = Db::memory();
        // A row left `running` by a previous process — started well before boot.
        let started = crate::db::now_unix() - 3600;
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix) VALUES ('ghost-01:00:00', 'running', ?1)",
                [started],
            )
        })
        .await
        .unwrap();

        let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
        // `new` spawns the reconcile; wait for it to flip the stale row.
        let id = JobId::from("ghost-01:00:00");
        let (status, _code, _tail) = await_row(&db, &id).await;
        assert_eq!(
            status, "failed",
            "a stale running row must reconcile to failed"
        );
        drop(store); // keep the store (its reaper task) alive across the wait
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
        });
        let id = JobId::from("gone");
        store.jobs.lock().await.insert(id.clone(), job);

        assert!(
            store.poll(&id, 0, None).await.is_err(),
            "missing log for a still-tracked job must surface as an error"
        );
    }
}
