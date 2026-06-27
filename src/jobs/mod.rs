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
pub use log::Page;
use log::{DEFAULT_PAGE, read_page};
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
    cmd: String,
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
    /// Finished within the inline window — output is ready now.
    Inline { state: JobState, page: Page },
    /// Still running — poll this id.
    Backgrounded { id: JobId },
}

#[derive(Debug, serde::Serialize)]
pub struct JobSummary {
    pub id: JobId,
    pub cmd: String,
    pub state: JobState,
}

#[derive(Clone)]
pub struct JobStore {
    dir: PathBuf,
    inline_timeout: Duration,
    seq: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<JobId, Arc<Job>>>>,
}

impl JobStore {
    pub fn new(dir: PathBuf, inline_timeout: Duration) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        let jobs = Arc::new(Mutex::new(HashMap::new()));
        spawn_reaper(jobs.clone());
        Ok(Self {
            dir,
            inline_timeout,
            seq: Arc::new(AtomicU64::new(1)),
            jobs,
        })
    }

    /// Spawn `cmd`. With `background`, return a job id immediately; otherwise wait
    /// up to the inline window and return output if it finishes in time.
    pub async fn run(
        &self,
        cmd: String,
        cwd: Option<String>,
        timeout_secs: Option<u64>,
        background: bool,
    ) -> std::io::Result<RunResult> {
        // Hold the jobs lock across id generation and insertion so the id is
        // reserved atomically: two identical commands launched in the same minute
        // can't both see their `slug-HH:MM` as free and clobber each other's
        // entry. Nothing awaits while the guard is held (file create + spawn are
        // synchronous), so the critical section stays short.
        let mut jobs = self.jobs.lock().await;
        let id = JobId::generate(&cmd, &self.seq, |candidate| jobs.contains_key(candidate));
        let log_path = self.dir.join(format!("{id}.log"));

        // ponytail: stdout+stderr merged into one log (terminal-style). Split into
        // two files if a caller ever needs them apart.
        let out = std::fs::File::create(&log_path)?;
        let err = out.try_clone()?;

        let mut command = tokio::process::Command::new("sh");
        command
            .arg("-c")
            .arg(&cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
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
            cmd,
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
                page: read_page(&log_path, 0, DEFAULT_PAGE).await,
            }),
        }
    }

    /// Status + one page of a job's log.
    pub async fn poll(
        &self,
        id: &JobId,
        cursor: usize,
        limit: Option<usize>,
    ) -> Option<(JobState, Page)> {
        let job = self.jobs.lock().await.get(id).cloned()?;
        let state = job.state.lock().await.clone();
        let page = read_page(&job.log_path, cursor, limit.unwrap_or(DEFAULT_PAGE)).await;
        Some((state, page))
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
                cmd: job.cmd.clone(),
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
mod tests {
    use super::*;

    fn store(inline: Duration) -> JobStore {
        let dir = tempfile::tempdir().unwrap().keep();
        JobStore::new(dir, inline).unwrap()
    }

    #[tokio::test]
    async fn fast_command_returns_inline() {
        let r = store(Duration::from_secs(5))
            .run("echo hello".into(), None, None, false)
            .await
            .unwrap();
        match r {
            RunResult::Inline { state, page } => {
                assert!(matches!(state, JobState::Exited { code: 0 }));
                assert!(page.lines.iter().any(|l| l.contains("hello")));
            }
            RunResult::Backgrounded { .. } => panic!("fast command should be inline"),
        }
    }

    #[tokio::test]
    async fn bg_flag_backgrounds_a_fast_command() {
        // Even though `echo` is instant, bg=true must return an id without waiting.
        let r = store(Duration::from_secs(5))
            .run("echo hi".into(), None, None, true)
            .await
            .unwrap();
        assert!(matches!(r, RunResult::Backgrounded { .. }));
    }

    #[tokio::test]
    async fn slow_command_backgrounds_then_completes() {
        let store = store(Duration::from_millis(100));
        let r = store
            .run("echo start; sleep 1; echo done".into(), None, None, false)
            .await
            .unwrap();
        let id = match r {
            RunResult::Backgrounded { id } => id,
            RunResult::Inline { .. } => panic!("slow command should background"),
        };
        for _ in 0..50 {
            let (state, page) = store.poll(&id, 0, None).await.unwrap();
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
            .run("sleep 300 & echo \"pid:$!\"; wait".into(), None, None, true)
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };

        // Pull the descendant's pid out of the log.
        let mut child_pid = None;
        for _ in 0..50 {
            let (_s, page) = store.poll(&id, 0, None).await.unwrap();
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
            .run("echo bye".into(), None, None, false)
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
            let (state, _) = store.poll(&id, 0, None).await.unwrap();
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
            .run("sleep 300 & echo \"pid:$!\"; wait".into(), None, None, true)
            .await
            .unwrap();
        let RunResult::Backgrounded { id } = r else {
            panic!("bg should background");
        };

        let mut child_pid = None;
        for _ in 0..50 {
            let (_s, page) = store.poll(&id, 0, None).await.unwrap();
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
            store.poll(&id, 0, None).await.is_none(),
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
            cmd: "unkillable".into(),
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
            store.poll(&id, 0, None).await.is_some(),
            "running job whose kill failed must stay tracked"
        );
        assert!(log_path.exists(), "its log must not be deleted");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_terminates_child_process() {
        let store = store(Duration::from_millis(100));
        let r = store
            .run("sleep 1000".into(), None, None, true)
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
            let (state, _) = store.poll(&id, 0, None).await.unwrap();
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
        // Two distinct commands — one inline, one explicitly backgrounded.
        store
            .run("echo alpha".into(), None, None, false)
            .await
            .unwrap();
        store
            .run("echo beta".into(), None, None, true)
            .await
            .unwrap();

        let jobs = store.list().await;
        assert_eq!(jobs.len(), 2, "expected two jobs, got {}", jobs.len());
        let cmds: Vec<&str> = jobs.iter().map(|j| j.cmd.as_str()).collect();
        assert!(cmds.contains(&"echo alpha"), "missing 'echo alpha'");
        assert!(cmds.contains(&"echo beta"), "missing 'echo beta'");
        // IDs must be sorted so the list is deterministic.
        assert!(jobs[0].id < jobs[1].id, "list should be sorted by id");
    }

    #[tokio::test]
    async fn poll_paginates() {
        let store = store(Duration::from_secs(5));
        store
            .run("seq 1 10".into(), None, None, false)
            .await
            .unwrap();
        // seq finishes inline; fetch its id from the listing to re-poll and
        // exercise pagination.
        let id = store.list().await.first().expect("job tracked").id.clone();
        let (_s, page) = store.poll(&id, 0, Some(3)).await.unwrap();
        assert_eq!(page.lines.len(), 3);
        assert_eq!(page.next_cursor, 3);
        assert!(page.has_more);
        assert_eq!(page.total_lines, 10);
    }
}
