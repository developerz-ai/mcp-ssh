//! Boots the real `mcp-ssh serve` binary on an ephemeral port with all state in a
//! tempdir. Dropping a `Server` SIGKILLs it *and* every job process group it
//! spawned, so a failing test leaks no `sleep 300` onto the runner.
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use reqwest::Client;

use crate::client::Session;
use crate::oauth::{login, login_as};

pub const USER: &str = "e2e";
pub const PASS: &str = "e2e-password";
const BOOT_DEADLINE: Duration = Duration::from_secs(20);

/// A running server plus the tempdir holding its DB, job logs, config and HOME.
pub struct Server {
    child: Option<Child>,
    pub base: String,
    pub dir: tempfile::TempDir,
}

impl Server {
    /// Boot with the default test env; panics if it isn't healthy in time.
    pub async fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self::start_in(dir).await
    }

    /// Boot on an existing state dir (restart scenarios).
    pub async fn start_in(dir: tempfile::TempDir) -> Self {
        let port = free_port();
        let child = command(dir.path(), port, &[])
            .spawn()
            .expect("spawn mcp-ssh");
        let server = Self {
            child: Some(child),
            base: format!("http://127.0.0.1:{port}"),
            dir,
        };
        server.wait_healthy().await;
        server
    }

    pub fn pid(&self) -> u32 {
        self.child.as_ref().expect("running").id()
    }

    pub fn job_dir(&self) -> PathBuf {
        self.dir.path().join("jobs")
    }

    pub fn db_path(&self) -> PathBuf {
        self.dir.path().join("state").join("mcp-ssh.db")
    }

    /// SIGTERM the server and wait up to `deadline` for it to exit. Returns the
    /// state dir (for a restart) and whether it exited in time. Job groups it
    /// leaves behind are killed when `self` drops at the end of this call.
    pub fn terminate(mut self, deadline: Duration) -> (tempfile::TempDir, bool) {
        let groups = self.job_groups();
        let mut child = self.child.take().expect("running");
        signal(child.id(), libc::SIGTERM);
        let exited = wait_exit(&mut child, deadline);
        if !exited {
            let _ = child.kill();
            let _ = child.wait();
        }
        kill_groups(&groups);
        let dir = std::mem::replace(&mut self.dir, tempfile::tempdir().expect("tempdir"));
        (dir, exited)
    }

    /// Process groups of every descendant of the server — its jobs. The server
    /// runs in its own group (see `command`), so ours is never among them.
    fn job_groups(&self) -> BTreeSet<libc::pid_t> {
        let Some(child) = self.child.as_ref() else {
            return BTreeSet::new();
        };
        let procs = proc_table();
        let mut frontier = vec![child.id() as libc::pid_t];
        let mut groups = BTreeSet::new();
        while let Some(parent) = frontier.pop() {
            for &(pid, ppid, pgrp) in &procs {
                if ppid == parent {
                    groups.insert(pgrp);
                    frontier.push(pid);
                }
            }
        }
        // SAFETY: getpgrp(2) has no preconditions.
        groups.remove(&unsafe { libc::getpgrp() });
        groups
    }

    async fn wait_healthy(&self) {
        let start = Instant::now();
        let http = Client::new();
        while start.elapsed() < BOOT_DEADLINE {
            let url = format!("{}/healthz", self.base);
            if let Ok(r) = http.get(url).send().await
                && r.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("mcp-ssh did not answer /healthz within {BOOT_DEADLINE:?}");
    }

    /// Run the full OAuth login and open an initialized MCP session.
    pub async fn session(&self) -> Session {
        let token = login(&self.base).await;
        Session::open(&self.base, &token).await
    }

    /// [`Server::session`] as its own OAuth client, registered as `client_name`
    /// — one per agent sharing the host.
    pub async fn session_as(&self, client_name: &str) -> Session {
        let token = login_as(&self.base, Some(client_name)).await;
        Session::open(&self.base, &token).await
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let groups = self.job_groups();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        kill_groups(&groups);
    }
}

/// `mcp-ssh serve` with a scrubbed env: nothing from the developer's shell (a
/// stray `MCP_SSH_*`) leaks in, and nothing touches /etc or /var/lib. Runs in
/// its own process group so cleanup can never signal the test runner.
pub fn command(dir: &Path, port: u16, extra_env: &[(&str, &str)]) -> Command {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).expect("home dir");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mcp-ssh"));
    cmd.arg("serve")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("MCP_SSH_CONFIG", dir.join("absent-config.toml"))
        .env("MCP_SSH_BIND", format!("127.0.0.1:{port}"))
        .env("MCP_SSH_USER", USER)
        .env("MCP_SSH_PASS", PASS)
        .env("MCP_SSH_DB", dir.join("state").join("mcp-ssh.db"))
        .env("MCP_SSH_JOB_DIR", dir.join("jobs"))
        .env("MCP_SSH_INLINE_TIMEOUT_SECS", "1")
        .env("RUST_LOG", "warn")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd
}

/// An OS-assigned free port (never 1337). The listener is dropped before the
/// server binds; the window for a collision is negligible in a test run.
pub fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind :0");
    listener.local_addr().expect("local addr").port()
}

fn signal(pid: u32, sig: libc::c_int) {
    let pid = libc::pid_t::try_from(pid).expect("pid fits");
    // SAFETY: plain kill(2) on our own child's pid.
    let rc = unsafe { libc::kill(pid, sig) };
    assert_eq!(rc, 0, "kill({pid}, {sig}) failed");
}

fn kill_groups(groups: &BTreeSet<libc::pid_t>) {
    for &pgid in groups {
        // SAFETY: kill(2) on a job's process group; ESRCH (already gone) is fine.
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
    }
}

/// `(pid, ppid, pgrp)` of every process, from /proc/<pid>/stat.
fn proc_table() -> Vec<(libc::pid_t, libc::pid_t, libc::pid_t)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|e| {
            let pid: libc::pid_t = e.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            // Fields after the parenthesised comm: state ppid pgrp ...
            let mut rest = stat.rsplit_once(')')?.1.split_whitespace().skip(1);
            let ppid = rest.next()?.parse().ok()?;
            let pgrp = rest.next()?.parse().ok()?;
            Some((pid, ppid, pgrp))
        })
        .collect()
}

pub fn wait_exit(child: &mut Child, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// Resident set size of `pid` in KiB, from /proc.
pub fn rss_kib(pid: u32) -> u64 {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("proc status");
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .expect("VmRSS")
}
