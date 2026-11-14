//! The developer scripts' fresh-checkout contract:
//!
//! - `bin/setup` and `bin/check` work from any CWD: both
//!   `cd "$(dirname "$0")/.."` to the repo root first.
//! - `bin/setup` seeds `.env` from `.env.example` only when `.env` is absent
//!   (mode 600) and never overwrites real credentials.
//! - `bin/setup` wires the pre-commit gate: `git config core.hooksPath .githooks`.
//! - `.env.example` points runtime state at repo-local `.local/` paths, so a
//!   fresh non-root checkout never writes to `/var/lib`.
//! - `bin/stop` kills the pid `bin/dev` recorded even when that pid is the
//!   server itself: without cargo-watch, `exec cargo run -- serve` execs the
//!   built binary in place, leaving ps args `target/debug/mcp-ssh serve`.
//!
//! The scripts are driven for real: each test copies them into a sandbox with
//! a stub `cargo` first on PATH that refuses to run from anywhere but the
//! sandbox repo root — deleting any of the lines above fails a test without
//! invoking the toolchain.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use tempfile::TempDir;

const REPO_ROOT: &str = env!("CARGO_MANIFEST_DIR");

const STUB_CARGO: &str = r#"#!/bin/sh
# Test double for `cargo`: bin/setup and bin/check must only ever reach it
# after cd-ing to the repo root, so refuse anywhere else and log the CWD of
# every accepted call for the tests to assert on.
if [ "$(pwd)" != "$CARGO_STUB_EXPECTED_ROOT" ]; then
  echo "cargo stub: ran in $(pwd), expected $CARGO_STUB_EXPECTED_ROOT" >&2
  exit 90
fi
pwd >> "$CARGO_STUB_LOG"
"#;

/// A throwaway repo: `bin/{setup,check,stop}` + `.env.example` copied from the
/// real checkout, a stub `cargo` on PATH, and a directory that is
/// deliberately not the repo root to run the scripts from.
struct Sandbox {
    _temp: TempDir, // owns sandbox cleanup
    repo: PathBuf,
    foreign: PathBuf,
    stub_dir: PathBuf,
    cargo_log: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create sandbox tempdir");
        let repo = temp.path().join("repo");
        let foreign = temp.path().join("foreign");
        let stub_dir = temp.path().join("stub-bin");
        let cargo_log = temp.path().join("cargo-calls.log");
        fs::create_dir_all(repo.join("bin")).expect("create sandbox bin/");
        fs::create_dir_all(&foreign).expect("create foreign CWD dir");
        fs::create_dir_all(&stub_dir).expect("create stub PATH dir");
        // bin/setup wires the pre-commit gate with `git config core.hooksPath`,
        // which fails outside a repo — a fresh checkout is a repo, so the
        // sandbox is one too.
        let init = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git-init the sandbox repo");
        assert!(
            init.status.success(),
            "git init failed in the sandbox: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        for script in ["setup", "check", "stop"] {
            let src = Path::new(REPO_ROOT).join("bin").join(script);
            let dst = repo.join("bin").join(script);
            fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {src:?} into sandbox: {e}"));
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))
                .expect("make sandbox script executable");
        }
        fs::copy(
            Path::new(REPO_ROOT).join(".env.example"),
            repo.join(".env.example"),
        )
        .expect("copy .env.example into sandbox");
        fs::write(stub_dir.join("cargo"), STUB_CARGO).expect("write cargo stub");
        fs::set_permissions(stub_dir.join("cargo"), fs::Permissions::from_mode(0o755))
            .expect("make cargo stub executable");
        Self {
            _temp: temp,
            repo,
            foreign,
            stub_dir,
            cargo_log,
        }
    }

    /// Run `bin/<script>` from the foreign CWD with the stub first on PATH.
    fn run(&self, script: &str) -> Output {
        // System PATH kept after the stub so bash/dirname/cp/chmod still resolve.
        let path = match std::env::var("PATH") {
            Ok(system) => format!("{}:{system}", self.stub_dir.display()),
            Err(_) => format!("{}:/usr/bin:/bin", self.stub_dir.display()),
        };
        Command::new(self.repo.join("bin").join(script))
            .current_dir(&self.foreign)
            .env("PATH", path)
            .env("CARGO_STUB_EXPECTED_ROOT", &self.repo)
            .env("CARGO_STUB_LOG", &self.cargo_log)
            .output()
            .expect("spawn script")
    }

    /// Fails unless the stub `cargo` ran — and ran only inside the sandbox
    /// repo root, i.e. unless the script cd'd there from the foreign CWD.
    fn assert_cargo_ran_from_repo_root(&self) {
        let log = fs::read_to_string(&self.cargo_log)
            .expect("cargo stub never invoked — the script never reached `cargo`");
        let calls: Vec<&str> = log.lines().collect();
        assert!(!calls.is_empty(), "cargo stub logged no CWDs");
        for call in &calls {
            assert_eq!(
                *call,
                self.repo.to_string_lossy(),
                "cargo ran outside the repo root — the script's cd-to-root line is missing"
            );
        }
    }
}

/// Value of `KEY=` in an .env-style file, skipping comments and stripping
/// optional quotes. None when the key is absent or commented out.
fn env_value(example: &str, key: &str) -> Option<String> {
    example.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with('#') {
            return None;
        }
        let value = line.strip_prefix(key)?.strip_prefix('=')?;
        Some(
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
        )
    })
}

#[test]
fn setup_seeds_env_from_example_when_absent() {
    let sandbox = Sandbox::new();
    let output = sandbox.run("setup");
    assert!(
        output.status.success(),
        "bin/setup failed from a foreign CWD:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let env_path = sandbox.repo.join(".env");
    let example = fs::read_to_string(sandbox.repo.join(".env.example")).expect("read .env.example");
    let seeded =
        fs::read_to_string(&env_path).expect("bin/setup did not seed .env when it was absent");
    assert_eq!(
        seeded, example,
        "seeded .env must be a verbatim copy of .env.example"
    );
    let mode = fs::metadata(&env_path)
        .expect("read seeded .env metadata")
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "seeded .env must be mode 600, got {:o}",
        mode & 0o777
    );
    // bin/setup must leave the checkout's pre-commit gate wired: a fresh
    // checkout gets .githooks as core.hooksPath.
    let hooks = Command::new("git")
        .args(["config", "core.hooksPath"])
        .current_dir(&sandbox.repo)
        .output()
        .expect("read core.hooksPath in the sandbox repo");
    assert!(
        hooks.status.success(),
        "git config core.hooksPath failed in the sandbox: {}",
        String::from_utf8_lossy(&hooks.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&hooks.stdout).trim(),
        ".githooks",
        "bin/setup must set core.hooksPath to .githooks"
    );
    sandbox.assert_cargo_ran_from_repo_root();
}

#[test]
fn setup_never_overwrites_an_existing_env() {
    let sandbox = Sandbox::new();
    let env_path = sandbox.repo.join(".env");
    let real = "MCP_SSH_USER=real-user\nMCP_SSH_PASS=real-secret\n";
    fs::write(&env_path, real).expect("pre-seed a real .env");
    let output = sandbox.run("setup");
    assert!(
        output.status.success(),
        "bin/setup failed with an existing .env:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&env_path).expect("read existing .env after setup"),
        real,
        "bin/setup overwrote an existing .env — real credentials must be left untouched"
    );
    sandbox.assert_cargo_ran_from_repo_root();
}

#[test]
fn check_runs_the_gate_from_the_repo_root() {
    let sandbox = Sandbox::new();
    let output = sandbox.run("check");
    assert!(
        output.status.success(),
        "bin/check failed from a foreign CWD:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    sandbox.assert_cargo_ran_from_repo_root();
}

#[test]
fn env_example_runtime_defaults_are_repo_local() {
    let example =
        fs::read_to_string(Path::new(REPO_ROOT).join(".env.example")).expect("read .env.example");
    for key in ["MCP_SSH_JOB_DIR", "MCP_SSH_DB"] {
        let Some(value) = env_value(&example, key) else {
            panic!(".env.example must set {key} (uncommented)");
        };
        assert!(
            value.starts_with(".local/"),
            "{key}={value} must stay repo-local under .local/"
        );
        assert!(
            !value.contains("/var/lib"),
            "{key}={value} must never point at /var/lib — that breaks a non-root fresh checkout"
        );
    }
}

/// `ps -o args=` for one pid — the input bin/stop's stale-pid check matches
/// against; empty once the process is gone.
fn ps_args(pid: u32) -> String {
    let out = Command::new("ps")
        .args(["-o", "args=", "-p"])
        .arg(pid.to_string())
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn stop_kills_the_serve_pid_bin_dev_records() {
    let sandbox = Sandbox::new();

    // The cargo-watch-less bin/dev path: `exec cargo run -- serve` and `cargo
    // run` execs the built binary in place, so the pid recorded in
    // `.local/dev.pid` is the server itself — ps args `target/debug/mcp-ssh
    // serve`, which the `*cargo*` half of bin/stop's check never matches.
    // `exec -a` fakes those argv words on a self-terminating sleep.
    let mut fake = Command::new("bash")
        .arg("-c")
        .arg("exec -a 'target/debug/mcp-ssh serve' sleep 30")
        .spawn()
        .expect("spawn the fake serve process");
    let pid = fake.id();

    // Wait for the exec to land: only then do the ps args show the server
    // shape the fixture must present to bin/stop (before it, the `-c` text
    // happens to contain the same words).
    let mut args = String::new();
    for _ in 0..200 {
        args = ps_args(pid);
        if args.starts_with("target/debug/mcp-ssh serve") {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        args.starts_with("target/debug/mcp-ssh serve") && !args.contains("cargo"),
        "fixture must present the exec'd server's ps args, got {args:?}"
    );

    fs::create_dir_all(sandbox.repo.join(".local")).expect("create sandbox .local");
    let pid_file = sandbox.repo.join(".local/dev.pid");
    fs::write(&pid_file, format!("{pid}\n")).expect("record the pid as bin/dev would");

    let output = sandbox.run("stop");
    assert!(
        output.status.success(),
        "bin/stop failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("stopped bin/dev (pid {pid})")),
        "bin/stop treated the recorded serve pid as stale instead of stopping it: {stdout:?}"
    );

    // The kill must actually have landed: within the poll window the recorded
    // pid can only die by bin/stop's TERM, never by its own 30s exit — and
    // being this test's own child, it is reaped here rather than checked via
    // ps (a killed-but-unreaped child still shows in ps).
    let mut death = None;
    for _ in 0..200 {
        if let Some(status) = fake.try_wait().expect("poll the fake serve process") {
            death = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = death.expect(
        "the fake serve survived bin/stop — the `*mcp-ssh*serve*` arm of \
         bin/stop's stale-pid check is load-bearing",
    );
    assert!(
        status.code().is_none(),
        "the fake serve exited on its own instead of being killed: {status:?}"
    );
    assert!(
        !pid_file.exists(),
        "bin/stop must remove .local/dev.pid once it stopped the server"
    );
}
