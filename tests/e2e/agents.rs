//! Several agents on one host: two independently registered OAuth clients, each
//! with its own bearer and session, working at the same time — and seeing each
//! other in `job(action="list")`, in replies about each other's jobs, and in the
//! admin CLI.
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::client::{Session, job_id, poll_until};
use crate::harness::Server;

/// Concurrency: neither agent waits on the other. Both background a slow job in
/// the same second under the same title (two distinct ids); while one sits in a
/// 2 s inline call, the other's quick commands return at once; and the two slow
/// jobs run side by side — each agent polls its own to completion in about one
/// job's time, not two.
#[tokio::test]
async fn two_agents_run_commands_and_jobs_concurrently() {
    let server = Server::start().await;
    let (mut alpha, mut beta) = tokio::join!(server.session_as("alpha"), server.session_as("beta"));

    let started = Instant::now();
    let slow = |who: &str| json!({ "cmd": format!("sleep 3; echo {who}-done"), "bg": true, "title": "slow" });
    let (a, b) = tokio::join!(
        alpha.call("bash", slow("alpha")),
        beta.call("bash", slow("beta"))
    );
    assert!(!a.is_error && !b.is_error, "{a:?} {b:?}");
    let (alpha_job, beta_job) = (job_id(&a.text, "slow"), job_id(&b.text, "slow"));
    assert_ne!(
        alpha_job, beta_job,
        "same title, same second: still two jobs"
    );

    let (held, quick) = tokio::join!(
        async {
            let at = Instant::now();
            let r = alpha
                .call(
                    "bash",
                    json!({ "cmd": "sleep 2; echo alpha-inline", "timeout": 10 }),
                )
                .await;
            (r, at.elapsed())
        },
        async {
            let at = Instant::now();
            let mut replies = Vec::new();
            for i in 0..5 {
                let cmd = format!("echo beta-quick-{i}");
                replies.push(beta.call("bash", json!({ "cmd": cmd })).await);
            }
            (replies, at.elapsed())
        }
    );
    let (inline, held_for) = held;
    assert!(inline.text.contains("alpha-inline"), "{inline:?}");
    assert!(held_for >= Duration::from_secs(2), "{held_for:?}");
    let (replies, quick_for) = quick;
    for (i, r) in replies.iter().enumerate() {
        assert!(
            !r.is_error && r.text.contains(&format!("beta-quick-{i}")),
            "beta's command ran inline, not stuck behind alpha's: {r:?}"
        );
    }
    assert!(
        quick_for < Duration::from_millis(1500),
        "beta's 5 quick commands waited on alpha's 2 s call: {quick_for:?}"
    );

    let (a, b) = tokio::join!(
        poll_until(&mut alpha, &alpha_job, "alpha-done"),
        poll_until(&mut beta, &beta_job, "beta-done")
    );
    assert!(
        !a.contains("beta-done") && !b.contains("alpha-done"),
        "{a} {b}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(5500),
        "two 3 s jobs ran one after the other: {:?}",
        started.elapsed()
    );
}

/// Awareness: each agent's `job(action="list")` opens with who is active (the
/// caller as `you`), each job says `by` whom; another agent's job can still be
/// polled and killed, and the reply says whose it is. A hostile `client_name`
/// reaches no one with its control characters. The admin CLI shows owners and
/// last-active times.
#[tokio::test]
async fn agents_see_each_other_and_whose_job_is_whose() {
    let server = Server::start().await;
    let mut alpha = server.session_as("alpha").await;
    let mut beta = server.session_as("beta").await;
    let mut hostile = server.session_as("evil\u{1b}[31m\nforged-row").await;

    let r = alpha
        .call(
            "bash",
            json!({ "cmd": "sleep 30", "bg": true, "title": "alpha-job" }),
        )
        .await;
    let alpha_job = job_id(&r.text, "alpha-job");
    let r = beta
        .call(
            "bash",
            json!({ "cmd": "echo beta-out", "bg": true, "title": "beta-job" }),
        )
        .await;
    let beta_job = job_id(&r.text, "beta-job");
    hostile.call("job", json!({ "action": "list" })).await;

    let (presence, jobs) = list(&mut alpha).await;
    assert!(
        presence.starts_with("3 agents active (last 15 min): you (alpha#"),
        "{presence}"
    );
    assert!(
        presence.contains("beta#") && presence.contains("evil[31m forged-row#"),
        "{presence}"
    );
    let alpha_label = between(&presence, "you (", ")");
    assert_eq!(by(&jobs, &alpha_job), "you");
    let beta_label = by(&jobs, &beta_job);
    assert!(beta_label.starts_with("beta#"), "{beta_label}");

    // Beta sees the same jobs from its side.
    let (presence, jobs) = list(&mut beta).await;
    assert!(
        presence.starts_with(&format!(
            "3 agents active (last 15 min): you ({beta_label})"
        )),
        "{presence}"
    );
    assert_eq!(by(&jobs, &alpha_job), alpha_label);
    assert_eq!(by(&jobs, &beta_job), "you");

    // Shared, not isolated: beta can poll and kill alpha's job — told whose it is.
    let note = format!("[started by another agent: {alpha_label}]");
    let r = beta
        .call("job", json!({ "action": "poll", "id": alpha_job }))
        .await;
    assert!(!r.is_error && r.text.starts_with(&note), "{r:?}");
    let r = beta
        .call("job", json!({ "action": "poll", "id": beta_job }))
        .await;
    assert!(!r.text.contains("another agent"), "own job, no note: {r:?}");
    let r = beta
        .call("job", json!({ "action": "kill", "id": alpha_job }))
        .await;
    assert!(
        !r.is_error && r.text == format!("{note}\nkilled {alpha_job}"),
        "{r:?}"
    );
    let r = alpha
        .call("job", json!({ "action": "poll", "id": alpha_job }))
        .await;
    assert!(
        r.text.contains("killed") && !r.text.contains("another agent"),
        "{r:?}"
    );

    let jobs_cli = admin(&server, &["jobs", "--all"]);
    assert!(jobs_cli.contains("BY"), "{jobs_cli}");
    for (job, owner) in [(&alpha_job, &alpha_label), (&beta_job, &beta_label)] {
        let row = jobs_cli
            .lines()
            .find(|l| l.starts_with(job.as_str()))
            .expect("listed");
        assert!(row.contains(owner.as_str()), "{row}");
    }
    let sessions = admin(&server, &["sessions"]);
    assert!(
        sessions.contains(&alpha_label) && sessions.contains("last active"),
        "{sessions}"
    );
    for out in [&jobs_cli, &sessions] {
        assert!(
            !out.contains('\u{1b}'),
            "a control character reached the terminal: {out:?}"
        );
    }
}

/// `job(action="list")` split into its presence line and its job rows.
async fn list(s: &mut Session) -> (String, Vec<Value>) {
    let r = s.call("job", json!({ "action": "list" })).await;
    assert!(!r.is_error, "{r:?}");
    assert!(
        !r.text.contains('\u{1b}'),
        "a control character reached the agent: {r:?}"
    );
    let (presence, json) = r.text.split_once('\n').expect("presence line, then JSON");
    let jobs: Vec<Value> = serde_json::from_str(json).expect("job rows");
    (presence.to_string(), jobs)
}

fn by(jobs: &[Value], id: &str) -> String {
    let job = jobs
        .iter()
        .find(|j| j["id"] == id)
        .unwrap_or_else(|| panic!("{id} not listed: {jobs:?}"));
    job["by"].as_str().expect("by").to_string()
}

fn between(s: &str, open: &str, close: &str) -> String {
    let start = s.find(open).expect("open") + open.len();
    let len = s[start..].find(close).expect("close");
    s[start..start + len].to_string()
}

/// Run an admin subcommand against the server's database, as the operator would.
fn admin(server: &Server, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_mcp-ssh"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env(
            "MCP_SSH_CONFIG",
            server.dir.path().join("absent-config.toml"),
        )
        .env("MCP_SSH_DB", server.db_path())
        .env("RUST_LOG", "warn")
        .output()
        .expect("run mcp-ssh admin command");
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf-8")
}
