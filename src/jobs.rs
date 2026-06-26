//! Job engine: run a command, return inline if it's fast, otherwise hand back
//! a job id the caller polls. Output streams to a per-job log file so polling
//! can paginate it without holding everything in memory.
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::sync::{Mutex, watch};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JobState {
    Running,
    Exited { code: i32 },
    Failed { error: String },
}

struct Job {
    cmd: String,
    log_path: PathBuf,
    /// Process group to signal on kill. The child leads its own group, so this
    /// equals its pid (see `run`). `None` only if the OS withheld a pid.
    pgid: Option<u32>,
    state: Arc<Mutex<JobState>>,
    started: tokio::time::Instant,
}

/// Result of starting a command.
pub enum RunResult {
    /// Finished within the inline window — output is ready now.
    Inline { state: JobState, page: Page },
    /// Still running — poll this id.
    Backgrounded { id: String },
}

/// One page of log lines plus a cursor to fetch the next page.
#[derive(Debug, serde::Serialize)]
pub struct Page {
    pub lines: Vec<String>,
    pub next_cursor: usize,
    pub total_lines: usize,
    pub has_more: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct JobSummary {
    pub id: String,
    pub cmd: String,
    pub state: JobState,
}

#[derive(Clone)]
pub struct JobStore {
    dir: PathBuf,
    inline_timeout: Duration,
    seq: Arc<AtomicU64>,
    jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>,
}

const DEFAULT_PAGE: usize = 200;
/// Jobs (and their logs) older than this are reaped hourly.
const RETENTION: Duration = Duration::from_secs(24 * 3600);

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
        let id = format!("j{}", self.seq.fetch_add(1, Ordering::Relaxed));
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
        let pgid = child.id();
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
            started: tokio::time::Instant::now(),
        });
        self.jobs.lock().await.insert(id.clone(), job);

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
        id: &str,
        cursor: usize,
        limit: Option<usize>,
    ) -> Option<(JobState, Page)> {
        let job = self.jobs.lock().await.get(id).cloned()?;
        let state = job.state.lock().await.clone();
        let page = read_page(&job.log_path, cursor, limit.unwrap_or(DEFAULT_PAGE)).await;
        Some((state, page))
    }

    pub async fn list(&self) -> Vec<JobSummary> {
        let jobs = self.jobs.lock().await;
        let mut out = Vec::with_capacity(jobs.len());
        for (id, job) in jobs.iter() {
            out.push(JobSummary {
                id: id.clone(),
                cmd: job.cmd.clone(),
                state: job.state.lock().await.clone(),
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// Kill a running job's whole process group. Returns false if unknown id.
    pub async fn kill(&self, id: &str) -> bool {
        let Some(job) = self.jobs.lock().await.get(id).cloned() else {
            return false;
        };
        if let Some(pgid) = job.pgid {
            // Negative pid targets the process group, so descendants the command
            // spawned die too — not just `sh`. `--` keeps `kill` from reading the
            // leading `-` as an option. ponytail: pid reuse is a non-issue here.
            let _ = tokio::process::Command::new("kill")
                .arg("--")
                .arg(format!("-{pgid}"))
                .status()
                .await;
        }
        true
    }
}

/// Read lines `[cursor, cursor+limit)` from a log file. Re-reads the whole file
/// each call — fine for typical logs; seek by byte offset if they get huge.
async fn read_page(path: &std::path::Path, cursor: usize, limit: usize) -> Page {
    let content = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let all: Vec<&str> = content.lines().collect();
    let total = all.len();
    let end = (cursor + limit).min(total);
    let lines = all
        .get(cursor..end)
        .unwrap_or(&[])
        .iter()
        .map(|s| s.to_string())
        .collect();
    Page {
        lines,
        next_cursor: end,
        total_lines: total,
        has_more: end < total,
    }
}

/// Hourly: drop jobs (and their log files) older than `RETENTION` so history
/// doesn't grow without bound. ponytail: time-based only; a busy box could still
/// hold ≤24h of jobs in memory — add a count cap if that ever bites.
fn spawn_reaper(jobs: Arc<Mutex<HashMap<String, Arc<Job>>>>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let now = tokio::time::Instant::now();
            let mut map = jobs.lock().await;
            let stale: Vec<(String, PathBuf)> = map
                .iter()
                .filter(|(_, j)| now.duration_since(j.started) > RETENTION)
                .map(|(id, j)| (id.clone(), j.log_path.clone()))
                .collect();
            for (id, _) in &stale {
                map.remove(id);
            }
            drop(map);
            for (_, path) in stale {
                let _ = tokio::fs::remove_file(path).await;
            }
        }
    });
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
    async fn poll_paginates() {
        let store = store(Duration::from_secs(5));
        let r = store
            .run("seq 1 10".into(), None, None, false)
            .await
            .unwrap();
        // seq finishes inline; re-poll the job id to exercise pagination.
        let id = match r {
            RunResult::Inline { .. } => "j1".to_string(),
            RunResult::Backgrounded { id } => id,
        };
        let (_s, page) = store.poll(&id, 0, Some(3)).await.unwrap();
        assert_eq!(page.lines.len(), 3);
        assert_eq!(page.next_cursor, 3);
        assert!(page.has_more);
        assert_eq!(page.total_lines, 10);
    }
}
