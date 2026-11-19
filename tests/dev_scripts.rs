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
//! - `bin/check`'s exit contract (`.claude/skills/verify-change/SKILL.md`):
//!   0 green, 1 a check that ran failed, 75 a required tool is missing,
//!   79 every check that ran passed but an optional CI gate could not run.
//! - The three hooks translate that: they exit 0 on 0 and 79, and propagate
//!   1 and 75.
//!
//! The scripts are driven for real: each test copies them into a sandbox with
//! a stub `cargo` first on PATH that refuses to run from anywhere but the
//! sandbox repo root — deleting any of the lines above fails a test without
//! invoking the toolchain. Gate runs get a *restricted* PATH (stubs plus
//! `dirname` only), so whether the host happens to have shellcheck or
//! cargo-audit never changes a test's outcome.

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use tempfile::TempDir;

const REPO_ROOT: &str = env!("CARGO_MANIFEST_DIR");

const STUB_CARGO: &str = r#"#!/bin/sh
# Test double for `cargo`: bin/setup and bin/check must only ever reach it
# after cd-ing to the repo root, so refuse anywhere else and log the CWD and
# args of every accepted call for the tests to assert on.
if [ "$(pwd)" != "$CARGO_STUB_EXPECTED_ROOT" ]; then
  echo "cargo stub: ran in $(pwd), expected $CARGO_STUB_EXPECTED_ROOT" >&2
  exit 90
fi
echo "$(pwd) $*" >> "$CARGO_STUB_LOG"
sub="$1"
# CARGO_STUB_FAIL lists subcommands (space separated) this run must fail, so a
# test can prove bin/check reports a red change.
case " ${CARGO_STUB_FAIL:-} " in
  *" $sub "*)
    echo "cargo stub: forced failure for '$sub'" >&2
    exit 1
    ;;
esac
# `cargo audit` really execs cargo-audit from PATH; keep that shape so both
# tool presence and tool failure are exercised through the tool itself.
if [ "$sub" = audit ]; then
  shift
  exec cargo-audit "$@"
fi
exit 0
"#;

/// A tool stub that succeeds — rustfmt, cargo-clippy, shellcheck, cargo-audit.
const STUB_PASS: &str = "#!/bin/sh\nexit 0\n";

const STUB_SHELLCHECK_FINDINGS: &str =
    "#!/bin/sh\necho 'In bin/check line 9: SC2086 quote to prevent globbing' >&2\nexit 1\n";

const STUB_AUDIT_VULNERABLE: &str =
    "#!/bin/sh\necho 'error: 1 vulnerability found (RUSTSEC-0000-0000)' >&2\nexit 1\n";

/// Stand-in for `git config core.hooksPath`: prints the value the test asked
/// for. Default (no env) is "no hooks configured", so the agent hooks do not
/// short-circuit; `GIT_STUB_HOOKS_PATH=.githooks` makes them skip the gate.
const STUB_GIT: &str = r#"#!/bin/sh
printf '%s\n' "${GIT_STUB_HOOKS_PATH:-}"
exit "${GIT_STUB_EXIT:-1}"
"#;

/// The three hooks that consume `bin/check`'s status surface.
const HOOKS: [&str; 3] = [
    ".githooks/pre-commit",
    ".claude/hooks/pre-commit.sh",
    ".claude/hooks/pre-push.sh",
];

/// A throwaway repo: `bin/{setup,check,stop}` + the hooks + `.env.example`
/// copied from the real checkout, a stub `cargo` on PATH, and a directory that
/// is deliberately not the repo root to run the scripts from.
struct Sandbox {
    _temp: TempDir, // owns sandbox cleanup
    repo: PathBuf,
    foreign: PathBuf,
    stub_dir: PathBuf,
    tool_dir: PathBuf,
    cargo_log: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("create sandbox tempdir");
        let repo = temp.path().join("repo");
        let foreign = temp.path().join("foreign");
        let stub_dir = temp.path().join("stub-bin");
        let tool_dir = temp.path().join("gate-bin");
        let cargo_log = temp.path().join("cargo-calls.log");
        fs::create_dir_all(repo.join("bin")).expect("create sandbox bin/");
        fs::create_dir_all(repo.join(".githooks")).expect("create sandbox .githooks/");
        fs::create_dir_all(repo.join(".claude/hooks")).expect("create sandbox .claude/hooks/");
        fs::create_dir_all(&foreign).expect("create foreign CWD dir");
        fs::create_dir_all(&stub_dir).expect("create stub PATH dir");
        fs::create_dir_all(&tool_dir).expect("create gate tool PATH dir");
        // bin/setup wires the pre-commit gate with `git config core.hooksPath`,
        // which fails outside a repo — a fresh checkout is a repo, so the
        // sandbox is one too. Pin the value away from `.githooks` so a host
        // global setting can never make the agent hooks skip the gate.
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
        let pinned = Command::new("git")
            .args(["config", "core.hooksPath", ".git/hooks"])
            .current_dir(&repo)
            .output()
            .expect("pin core.hooksPath in the sandbox repo");
        assert!(
            pinned.status.success(),
            "git config failed in the sandbox: {}",
            String::from_utf8_lossy(&pinned.stderr)
        );
        let scripts = [
            "bin/setup",
            "bin/check",
            "bin/stop",
            ".githooks/pre-commit",
            ".claude/hooks/pre-commit.sh",
            ".claude/hooks/pre-push.sh",
        ];
        for script in scripts {
            let src = Path::new(REPO_ROOT).join(script);
            let dst = repo.join(script);
            fs::copy(&src, &dst).unwrap_or_else(|e| panic!("copy {src:?} into sandbox: {e}"));
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))
                .expect("make sandbox script executable");
        }
        fs::copy(
            Path::new(REPO_ROOT).join(".env.example"),
            repo.join(".env.example"),
        )
        .expect("copy .env.example into sandbox");
        let sandbox = Self {
            _temp: temp,
            repo,
            foreign,
            stub_dir,
            tool_dir,
            cargo_log,
        };
        // The required toolchain is present and green unless a test says
        // otherwise; rustfmt and cargo-clippy are probes, never executed here.
        sandbox.install_tool("cargo", STUB_CARGO);
        sandbox.install_tool("rustfmt", STUB_PASS);
        sandbox.install_tool("cargo-clippy", STUB_PASS);
        // `bash` (the scripts' interpreter) and `dirname` (how they resolve
        // their own path) are the only host binaries a gate run needs;
        // everything else on the host stays hidden.
        for tool in ["bash", "dirname"] {
            std::os::unix::fs::symlink(host_tool(tool), sandbox.tool_dir.join(tool))
                .unwrap_or_else(|e| panic!("symlink {tool} into the gate PATH: {e}"));
        }
        // The hooks probe `git config core.hooksPath`; this double answers in
        // place of a real repository so the answer is deterministic.
        sandbox.install_tool_at(&sandbox.tool_dir, "git", STUB_GIT);
        sandbox
    }

    /// Write an executable stub into the gate PATH (the stubbed toolchain).
    fn install_tool(&self, name: &str, body: &str) {
        self.install_tool_at(&self.stub_dir, name, body);
    }

    fn install_tool_at(&self, dir: &Path, name: &str, body: &str) {
        let path = dir.join(name);
        fs::write(&path, body).unwrap_or_else(|e| panic!("write {name} stub: {e}"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("make {name} stub executable: {e}"));
    }

    fn remove_tool(&self, name: &str) {
        let path = self.stub_dir.join(name);
        fs::remove_file(&path).unwrap_or_else(|e| panic!("remove {name} stub: {e}"));
    }

    /// Make both optional CI gates (shellcheck, cargo audit) present and green.
    fn install_optional_gates(&self) {
        self.install_tool("shellcheck", STUB_PASS);
        self.install_tool("cargo-audit", STUB_PASS);
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

    /// PATH for a gate run: only the stubs and the tools `bin/check` and the
    /// hooks themselves need. Shellcheck and cargo-audit exist here only when a
    /// test installed them, so "absent" means absent on every host.
    fn gate_path(&self) -> OsString {
        format!("{}:{}", self.stub_dir.display(), self.tool_dir.display()).into()
    }

    /// Run a repo-relative script from the foreign CWD under the gate PATH.
    fn run_gate(&self, script: &str, envs: &[(&str, &str)]) -> Output {
        let mut command = Command::new(self.repo.join(script));
        command
            .current_dir(&self.foreign)
            .env("PATH", self.gate_path())
            .env("CARGO_STUB_EXPECTED_ROOT", &self.repo)
            .env("CARGO_STUB_LOG", &self.cargo_log);
        for (key, value) in envs {
            command.env(key, value);
        }
        command
            .output()
            .unwrap_or_else(|e| panic!("spawn {script}: {e}"))
    }

    /// One `"<cwd> <args>"` line per stub `cargo` call, in order.
    fn cargo_invocations(&self) -> Vec<String> {
        let log = fs::read_to_string(&self.cargo_log)
            .expect("cargo stub never invoked — the script never reached `cargo`");
        log.lines().map(str::to_string).collect()
    }

    /// Fails unless the stub `cargo` ran — and ran only inside the sandbox
    /// repo root, i.e. unless the script cd'd there from the foreign CWD.
    fn assert_cargo_ran_from_repo_root(&self) {
        let calls = self.cargo_invocations();
        assert!(!calls.is_empty(), "cargo stub logged no calls");
        let root = self.repo.to_string_lossy().into_owned();
        for call in &calls {
            let cwd = call.split(' ').next().unwrap_or_default();
            assert_eq!(
                cwd, root,
                "cargo ran outside the repo root — the script's cd-to-root line is missing"
            );
        }
    }

    /// Fails unless the gate ran the three checks CI cannot do without, with
    /// exactly the arguments CI uses: none of them may be dropped or scoped
    /// down to keep the gate green.
    fn assert_gate_ran_every_required_check(&self) {
        let calls = self.cargo_invocations();
        for required in [
            " fmt --check",
            " clippy --locked --all-targets -- -D warnings",
            " test --locked",
        ] {
            assert!(
                calls.iter().any(|call| call.ends_with(required)),
                "bin/check did not run `cargo{}`; calls: {calls:?}",
                required.trim_start()
            );
        }
    }
}

/// Absolute path of a tool on the host PATH.
fn host_tool(name: &str) -> PathBuf {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .expect("resolve a host tool");
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert!(
        out.status.success() && !path.is_empty(),
        "host tool {name} not found on PATH"
    );
    PathBuf::from(path)
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("script was killed by a signal instead of exiting")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
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
        stderr(&output)
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
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(&env_path).expect("read existing .env after setup"),
        real,
        "bin/setup overwrote an existing .env — real credentials must be left untouched"
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
        stderr(&output)
    );
    let out = stdout(&output);
    assert!(
        out.contains(&format!("stopped bin/dev (pid {pid})")),
        "bin/stop treated the recorded serve pid as stale instead of stopping it: {out:?}"
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

#[test]
fn check_runs_every_required_check_from_the_repo_root() {
    let sandbox = Sandbox::new();
    sandbox.install_optional_gates();
    let output = sandbox.run_gate("bin/check", &[]);
    assert_eq!(
        code(&output),
        0,
        "bin/check must exit 0 when every check passes:\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    sandbox.assert_cargo_ran_from_repo_root();
    sandbox.assert_gate_ran_every_required_check();
}

#[test]
fn check_fails_when_a_required_check_fails() {
    for failing in ["fmt", "clippy", "test"] {
        let sandbox = Sandbox::new();
        sandbox.install_optional_gates();
        let output = sandbox.run_gate("bin/check", &[("CARGO_STUB_FAIL", failing)]);
        assert_eq!(
            code(&output),
            1,
            "bin/check must exit 1 when `cargo {failing}` fails:\nstderr: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains(failing),
            "bin/check must name the failed check `{failing}`: {}",
            stderr(&output)
        );
    }
}

#[test]
fn check_fails_when_a_present_shellcheck_fails() {
    let sandbox = Sandbox::new();
    sandbox.install_tool("cargo-audit", STUB_PASS);
    sandbox.install_tool("shellcheck", STUB_SHELLCHECK_FINDINGS);
    let output = sandbox.run_gate("bin/check", &[]);
    assert_eq!(
        code(&output),
        1,
        "a present shellcheck that fails must make bin/check red, not deferred:\nstderr: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("shellcheck"),
        "bin/check must name shellcheck as the failed check: {}",
        stderr(&output)
    );
}

#[test]
fn check_fails_when_a_present_cargo_audit_fails() {
    let sandbox = Sandbox::new();
    sandbox.install_tool("shellcheck", STUB_PASS);
    sandbox.install_tool("cargo-audit", STUB_AUDIT_VULNERABLE);
    let output = sandbox.run_gate("bin/check", &[]);
    assert_eq!(
        code(&output),
        1,
        "a present cargo audit that fails must make bin/check red, not deferred:\nstderr: {}",
        stderr(&output)
    );
    assert!(
        stderr(&output).contains("cargo audit"),
        "bin/check must name cargo audit as the failed check: {}",
        stderr(&output)
    );
}

#[test]
fn check_defers_to_ci_when_an_optional_gate_is_absent() {
    // Each case: which optional tools to omit, and which SKIP lines must name
    // them. A silent skip is exactly what the contract forbids.
    let cases: [(&[&str], &[&str]); 3] = [
        (&["shellcheck"], &["shellcheck"]),
        (&["cargo-audit"], &["cargo audit"]),
        (
            &["shellcheck", "cargo-audit"],
            &["shellcheck", "cargo audit"],
        ),
    ];
    for (omit, expected) in cases {
        let sandbox = Sandbox::new();
        sandbox.install_optional_gates();
        for name in omit {
            sandbox.remove_tool(name);
        }
        let output = sandbox.run_gate("bin/check", &[]);
        assert_eq!(
            code(&output),
            79,
            "omitting {omit:?} from a green gate must exit 79:\nstderr: {}",
            stderr(&output)
        );
        let combined = format!("{}{}", stdout(&output), stderr(&output));
        assert!(
            combined.contains("SKIP:"),
            "bin/check must print a named SKIP line, got: {combined}"
        );
        for tool in expected {
            assert!(
                combined.contains(tool),
                "the SKIP diagnostic must name {tool}: {combined}"
            );
        }
    }
}

#[test]
fn check_never_defers_a_red_change() {
    let sandbox = Sandbox::new();
    // Both optional gates absent *and* a real failure: the failure wins.
    let output = sandbox.run_gate("bin/check", &[("CARGO_STUB_FAIL", "test")]);
    assert_eq!(
        code(&output),
        1,
        "a failing required check must be exit 1 even when part of the gate is skipped:\nstderr: {}",
        stderr(&output)
    );
}

#[test]
fn check_reports_an_unmet_environment_as_75() {
    let fixes = [
        ("cargo", "rustup"),
        ("rustfmt", "rustfmt"),
        ("cargo-clippy", "clippy"),
    ];
    for (tool, fix_word) in fixes {
        let sandbox = Sandbox::new();
        sandbox.install_optional_gates();
        sandbox.remove_tool(tool);
        let output = sandbox.run_gate("bin/check", &[]);
        assert_eq!(
            code(&output),
            75,
            "a missing {tool} must exit 75, never 1:\nstderr: {}",
            stderr(&output)
        );
        let err = stderr(&output);
        assert!(
            err.contains(tool),
            "the 75 diagnostic must name the missing tool {tool}: {err}"
        );
        assert!(
            err.contains(fix_word),
            "the 75 diagnostic must name the fix for {tool} ({fix_word}): {err}"
        );
        assert!(
            err.contains("environment"),
            "the 75 diagnostic must say the environment is at fault, not the change: {err}"
        );
    }
}

#[test]
fn check_refuses_to_grade_when_a_required_tool_is_missing() {
    // Nothing may run — and nothing may be reported as green — before the
    // environment is known good.
    let sandbox = Sandbox::new();
    sandbox.install_optional_gates();
    sandbox.remove_tool("cargo");
    let output = sandbox.run_gate("bin/check", &[]);
    assert_eq!(code(&output), 75, "stderr: {}", stderr(&output));
    assert!(
        !sandbox.cargo_log.exists(),
        "bin/check ran checks despite the precondition guard"
    );
}

#[test]
fn hooks_allow_a_green_gate() {
    let sandbox = Sandbox::new();
    sandbox.install_optional_gates();
    for hook in HOOKS {
        let output = sandbox.run_gate(hook, &[]);
        assert_eq!(
            code(&output),
            0,
            "{hook} must let a green gate through:\nstderr: {}",
            stderr(&output)
        );
    }
}

#[test]
fn hooks_allow_a_partially_graded_gate() {
    // 79 means every check that ran passed and CI grades the rest: blocking it
    // would refuse a correct change.
    let sandbox = Sandbox::new();
    for hook in HOOKS {
        let output = sandbox.run_gate(hook, &[]);
        assert_eq!(
            code(&output),
            0,
            "{hook} must let exit 79 through:\nstderr: {}",
            stderr(&output)
        );
    }
}

#[test]
fn hooks_block_a_red_change() {
    let sandbox = Sandbox::new();
    for hook in HOOKS {
        let output = sandbox.run_gate(hook, &[("CARGO_STUB_FAIL", "test")]);
        assert_eq!(
            code(&output),
            1,
            "{hook} must propagate a red gate:\nstderr: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("red"),
            "{hook} must blame the change on a red gate: {}",
            stderr(&output)
        );
    }
}

#[test]
fn hooks_block_an_unrunnable_gate() {
    let sandbox = Sandbox::new();
    sandbox.remove_tool("rustfmt");
    for hook in HOOKS {
        let output = sandbox.run_gate(hook, &[]);
        assert_eq!(
            code(&output),
            75,
            "{hook} must propagate an unmet environment:\nstderr: {}",
            stderr(&output)
        );
        assert!(
            stderr(&output).contains("environment"),
            "{hook} must blame the environment on exit 75: {}",
            stderr(&output)
        );
    }
}

#[test]
fn agent_hooks_skip_the_gate_when_the_git_hook_is_active() {
    for hook in [".claude/hooks/pre-commit.sh", ".claude/hooks/pre-push.sh"] {
        let sandbox = Sandbox::new();
        let output = sandbox.run_gate(
            hook,
            &[("GIT_STUB_HOOKS_PATH", ".githooks"), ("GIT_STUB_EXIT", "0")],
        );
        assert_eq!(
            code(&output),
            0,
            "{hook} must exit 0 when .githooks already runs the gate:\nstderr: {}",
            stderr(&output)
        );
        assert!(
            !sandbox.cargo_log.exists(),
            "{hook} ran the suite twice — the core.hooksPath short-circuit is load-bearing"
        );
    }
}
