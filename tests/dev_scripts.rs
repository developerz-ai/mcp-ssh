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
//! - `bin/check` runs ci.yml's two non-cargo gates beside the cargo three:
//!   `shellcheck` over exactly the file list parsed out of ci.yml at runtime
//!   (a test fails if the two lists drift) and `cargo audit` — each skipped
//!   loudly, naming the mirrored ci.yml job, when its tool is not installed.
//!
//! The scripts are driven for real: each test copies them into a sandbox with
//! a stub `cargo` that refuses to run from anywhere but the sandbox repo root
//! and a stub `shellcheck`, both first on PATH — deleting any of the lines
//! above fails a test without invoking the toolchain.

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
printf '%s\n' "$*" >> "$CARGO_STUB_ARGS_LOG"
# CARGO_STUB_FAIL_AUDIT_PROBE (set by the audit-skip test alone): fail the
# `audit --version` probe — bin/check's guard for an absent cargo-audit —
# and nothing else, so the test can watch the guard short-circuit its gate.
if [ -n "${CARGO_STUB_FAIL_AUDIT_PROBE:-}" ] && [ "$*" = "audit --version" ]; then
  exit 91
fi
"#;

const STUB_SHELLCHECK: &str = r#"#!/bin/sh
# Test double for `shellcheck`: shadows any real one so bin/check's parity
# gate behaves identically on boxes with and without the tool (the sandbox
# carries bin/*'s expansion of the list — see the parity test below — so a
# real shellcheck would fail on the files it lacks). Logs its args
# space-joined, one line per call, for the ci.yml parity test to assert on.
printf '%s\n' "$*" >> "$SHELLCHECK_STUB_LOG"
"#;

/// A throwaway repo: `bin/{setup,check,stop}` + `.env.example` copied from the
/// real checkout, stub `cargo` and `shellcheck` on PATH, and a directory that
/// is deliberately not the repo root to run the scripts from.
struct Sandbox {
    _temp: TempDir, // owns sandbox cleanup
    repo: PathBuf,
    foreign: PathBuf,
    stub_dir: PathBuf,
    cargo_log: PathBuf,
    cargo_args_log: PathBuf,
    shellcheck_log: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create sandbox tempdir");
        let repo = temp.path().join("repo");
        let foreign = temp.path().join("foreign");
        let stub_dir = temp.path().join("stub-bin");
        let cargo_log = temp.path().join("cargo-calls.log");
        let cargo_args_log = temp.path().join("cargo-args.log");
        let shellcheck_log = temp.path().join("shellcheck-calls.log");
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
        fs::write(stub_dir.join("shellcheck"), STUB_SHELLCHECK).expect("write shellcheck stub");
        fs::set_permissions(
            stub_dir.join("shellcheck"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("make shellcheck stub executable");
        Self {
            _temp: temp,
            repo,
            foreign,
            stub_dir,
            cargo_log,
            cargo_args_log,
            shellcheck_log,
        }
    }

    /// Stub dir first, system PATH after: the stubs shadow any real
    /// cargo/shellcheck while bash/dirname/cp/chmod still resolve.
    fn stubbed_path(&self) -> String {
        match std::env::var("PATH") {
            Ok(system) => format!("{}:{system}", self.stub_dir.display()),
            Err(_) => format!("{}:/usr/bin:/bin", self.stub_dir.display()),
        }
    }

    /// Run `bin/<script>` from the foreign CWD with the stub first on PATH.
    fn run(&self, script: &str) -> Output {
        self.run_with(script, Path::new(&self.stubbed_path()), &[])
    }

    /// `run` with the PATH and the child env under the caller's control: the
    /// shellcheck-skip test passes the stub dir alone — so a real shellcheck
    /// anywhere on the system PATH cannot mask the absence it exercises — and
    /// the audit-skip test carries its failure flag into the stub cargo.
    fn run_with(&self, script: &str, path: &Path, extra_env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(self.repo.join("bin").join(script));
        cmd.current_dir(&self.foreign)
            .env("PATH", path)
            .env("CARGO_STUB_EXPECTED_ROOT", &self.repo)
            .env("CARGO_STUB_LOG", &self.cargo_log)
            .env("CARGO_STUB_ARGS_LOG", &self.cargo_args_log)
            .env("SHELLCHECK_STUB_LOG", &self.shellcheck_log);
        for (key, value) in extra_env {
            cmd.env(key, value);
        }
        cmd.output().expect("spawn script")
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

/// ci.yml's shellcheck job's file list, parsed at runtime from the real
/// workflow — never a copy in this file — so bin/check's list is pinned to
/// CI's mechanically: either side drifting fails the parity test below.
fn ci_shellcheck_files() -> String {
    let ci = fs::read_to_string(Path::new(REPO_ROOT).join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    let lines: Vec<&str> = ci.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with("shellcheck "))
        .expect("ci.yml: shellcheck job's file list not found");
    // The job runs `shellcheck` as a folded (`>-`) step: the first line names
    // the tool, its continuations are the following lines indented at least
    // as deep, until a blank line or the next job ends the block.
    let first = lines[start].trim_start();
    let indent = lines[start].len() - first.len();
    let mut list = first["shellcheck ".len()..].trim().to_string();
    for line in &lines[start + 1..] {
        let trimmed = line.trim_start();
        let continuation = !trimmed.is_empty()
            && line.len() - trimmed.len() >= indent
            && !trimmed.starts_with('#')
            && !trimmed.starts_with('-');
        if !continuation {
            break;
        }
        list.push(' ');
        list.push_str(trimmed);
    }
    list
}

#[test]
fn check_matches_ci_shellcheck_list_and_runs_audit() {
    let sandbox = Sandbox::new();
    let output = sandbox.run("check");
    assert!(
        output.status.success(),
        "bin/check failed from a foreign CWD:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The sandbox stubs exist so neither parity gate is skipped here.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("SKIP:"),
        "bin/check skipped a gate in the sandbox, where both tools are stubbed: {stderr}"
    );

    // The audit gate ran through the stub cargo, not just its guard probe.
    let args = fs::read_to_string(&sandbox.cargo_args_log)
        .expect("cargo stub logged no args — bin/check bypassed the stub");
    assert!(
        args.lines().any(|call| call == "audit"),
        "bin/check must run a plain `cargo audit` (ci.yml's audit job), got {args:?}"
    );
    sandbox.assert_cargo_ran_from_repo_root();

    // bin/* expands in the sandbox (to bin/{check,setup,stop}) where ci.yml
    // keeps it literal, so compare in two halves: the stub's recorded args
    // must open with that expansion — bin/check itself included — and then
    // continue with exactly ci.yml's remaining file list.
    let ci = ci_shellcheck_files();
    assert!(
        ci.starts_with("bin/* "),
        "ci.yml's shellcheck list must start with bin/* for this comparison, got {ci:?}"
    );
    let shellchecked =
        fs::read_to_string(&sandbox.shellcheck_log).expect("shellcheck stub never invoked");
    let checked: Vec<&str> = shellchecked.split_whitespace().collect();
    let expanded = checked
        .iter()
        .take_while(|arg| arg.starts_with("bin/"))
        .count();
    assert!(
        expanded > 0 && checked[..expanded].contains(&"bin/check"),
        "bin/check must shellcheck its own bin/* — the stub saw {checked:?}"
    );
    assert_eq!(
        checked[expanded..].to_vec(),
        ci["bin/* ".len()..]
            .split_whitespace()
            .collect::<Vec<&str>>(),
        "bin/check's shellcheck file list drifted from ci.yml's shellcheck job"
    );
}

/// Symlink one system tool into `dir`: the shellcheck-skip test runs
/// bin/check against a PATH of just the stub dir, which then has to carry
/// the external commands bin/check needs besides cargo — bash, for the
/// `#!/usr/bin/env bash` shebang, and dirname.
fn link_system_tool(tool: &str, dir: &Path) {
    let found = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|search_dir| search_dir.join(tool))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("no {tool} on the test process's PATH"))
        .canonicalize()
        .expect("resolve the tool's real path");
    std::os::unix::fs::symlink(found, dir.join(tool))
        .unwrap_or_else(|e| panic!("symlink {tool} into the stub PATH dir: {e}"));
}

/// The skip branch bin/check guards shellcheck with: without the tool the
/// gate must neither fail nor pass silently — it prints a SKIP notice naming
/// the mirrored ci.yml job and carries on with the audit gate. Deleting the
/// else-arm makes the skip silent; making the skip abort the gate makes
/// bin/check fail: either breaks this test.
#[test]
fn check_skips_shellcheck_loudly_when_it_is_absent() {
    let sandbox = Sandbox::new();
    fs::remove_file(sandbox.stub_dir.join("shellcheck")).expect("remove the shellcheck stub");
    // Pin the absence against the box: a real shellcheck later on the system
    // PATH would answer the guard's `command -v` and run — and fail, on the
    // sandbox's file list — so this run sees the stub dir alone.
    link_system_tool("bash", &sandbox.stub_dir);
    link_system_tool("dirname", &sandbox.stub_dir);
    let output = sandbox.run_with("check", &sandbox.stub_dir, &[]);
    assert!(
        output.status.success(),
        "bin/check must exit 0 when shellcheck is absent:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SKIP:") && stderr.contains("ci.yml shellcheck job"),
        "bin/check must skip loudly, naming the mirrored ci.yml shellcheck job: {stderr:?}"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("SKIP:"),
        "the SKIP notice belongs on stderr, not stdout"
    );
    assert!(
        !sandbox.shellcheck_log.exists(),
        "nothing may shellcheck when the tool is absent"
    );
    // Skipping one gate must not end the run: the audit gate follows it.
    let args = fs::read_to_string(&sandbox.cargo_args_log).expect("cargo stub never invoked");
    assert!(
        args.lines().any(|call| call == "audit"),
        "bin/check must still run `cargo audit` after the skipped shellcheck gate, got {args:?}"
    );
    sandbox.assert_cargo_ran_from_repo_root();
}

/// The audit gate's skip branch, driven through the guard bin/check itself
/// uses: `cargo audit --version`. The stub fails exactly that probe when
/// CARGO_STUB_FAIL_AUDIT_PROBE is set — a box without cargo-audit — so the
/// gate must skip loudly (naming the mirrored ci.yml audit job), exit 0,
/// and leave the real `cargo audit` unrun.
#[test]
fn check_skips_audit_loudly_when_cargo_audit_is_absent() {
    let sandbox = Sandbox::new();
    let output = sandbox.run_with(
        "check",
        Path::new(&sandbox.stubbed_path()),
        &[("CARGO_STUB_FAIL_AUDIT_PROBE", "1")],
    );
    assert!(
        output.status.success(),
        "bin/check must exit 0 when cargo-audit is absent (the failing probe runs inside an if):\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("SKIP:") && stderr.contains("ci.yml audit job"),
        "bin/check must skip loudly, naming the mirrored ci.yml audit job: {stderr:?}"
    );
    // The probe ran and failed, so the gate behind it never did — and the
    // shellcheck gate ahead of it is unaffected.
    let args = fs::read_to_string(&sandbox.cargo_args_log).expect("cargo stub never invoked");
    assert!(
        args.lines().any(|call| call == "audit --version"),
        "bin/check must probe for cargo-audit before skipping, got {args:?}"
    );
    assert!(
        !args.lines().any(|call| call == "audit"),
        "a failed probe must skip the `cargo audit` gate itself, got {args:?}"
    );
    assert!(
        sandbox.shellcheck_log.exists(),
        "the shellcheck gate ahead of the skipped audit gate must still run"
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
