//! `bash` (inline, auto-background, `bg`, `timeout`, `cwd`) and `job` (poll
//! paging, list, kill escalation, bounded reads of a huge live log).
use std::time::{Duration, Instant};

use serde_json::json;

use crate::client::{job_id, next_cursor, poll_until};
use crate::harness::{Server, rss_kib};

const GIB: u64 = 1 << 30;

/// Wait for a file the job writes (e.g. its pid) and return its trimmed content.
async fn read_when_ready(path: &std::path::Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(path)
            && s.ends_with('\n')
        {
            return s.trim().to_string();
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{} never written",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_dead(pid: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[tokio::test]
async fn bash_inline_background_and_cwd() {
    let server = Server::start().await;
    let mut s = server.session().await;

    let r = s
        .call("bash", json!({ "cmd": "echo inline-$((1+1))" }))
        .await;
    assert!(!r.is_error && r.text.contains("inline-2"), "{r:?}");

    // Past the 1 s inline window (test env) → a job id, output reachable by poll.
    let r = s
        .call(
            "bash",
            json!({ "cmd": "sleep 2; echo auto-done", "title": "auto" }),
        )
        .await;
    let id = job_id(&r.text, "auto");
    poll_until(&mut s, &id, "auto-done").await;

    // `timeout` widens the inline window.
    let r = s
        .call(
            "bash",
            json!({ "cmd": "sleep 2; echo waited", "timeout": 10 }),
        )
        .await;
    assert!(r.text.contains("waited"), "{r:?}");

    let r = s
        .call(
            "bash",
            json!({ "cmd": "echo bg-marker", "bg": true, "title": "bgjob" }),
        )
        .await;
    let id = job_id(&r.text, "bgjob");
    poll_until(&mut s, &id, "bg-marker").await;

    let r = s
        .call("bash", json!({ "cmd": "pwd", "cwd": server.dir.path() }))
        .await;
    assert!(
        r.text.contains(&server.dir.path().display().to_string()),
        "{r:?}"
    );
    let r = s
        .call(
            "bash",
            json!({ "cmd": "pwd", "cwd": "/nonexistent-e2e-dir" }),
        )
        .await;
    assert!(r.is_error, "bad cwd must be an error: {r:?}");
}

#[tokio::test]
async fn job_poll_pages_and_kill_really_kills() {
    let server = Server::start().await;
    let mut s = server.session().await;

    let r = s
        .call(
            "bash",
            json!({ "cmd": "seq 1 50", "bg": true, "title": "seq" }),
        )
        .await;
    let id = job_id(&r.text, "seq");
    poll_until(&mut s, &id, "50").await;
    let newest = s
        .call("job", json!({ "action": "poll", "id": id, "limit": 5 }))
        .await;
    assert!(
        newest.text.lines().any(|l| l == "50") && !newest.text.lines().any(|l| l == "44"),
        "{newest:?}"
    );
    let cursor = next_cursor(&newest.text);
    let older = s
        .call(
            "job",
            json!({ "action": "poll", "id": id, "limit": 5, "cursor": cursor }),
        )
        .await;
    assert!(
        // Compare whole lines: the job id's HH-MM-SS can itself contain "50".
        older.text.lines().any(|l| l == "45") && !older.text.lines().any(|l| l == "50"),
        "{older:?}"
    );
    let list = s.call("job", json!({ "action": "list" })).await;
    assert!(list.text.contains(&id), "{list:?}");

    // A job that ignores SIGTERM still dies: kill escalates to SIGKILL.
    let pid_file = server.dir.path().join("trap.pid");
    let cmd = format!(
        "trap '' TERM; echo $$ > {}; while :; do sleep 1; done",
        pid_file.display()
    );
    let r = s
        .call("bash", json!({ "cmd": cmd, "bg": true, "title": "trap" }))
        .await;
    let id = job_id(&r.text, "trap");
    let pid = read_when_ready(&pid_file).await;
    let r = s.call("job", json!({ "action": "kill", "id": id })).await;
    assert!(!r.is_error, "{r:?}");
    assert!(
        wait_dead(&pid).await,
        "TERM-trapping job {pid} survived kill"
    );

    // Killing it again must not claim a second success.
    let again = s.call("job", json!({ "action": "kill", "id": id })).await;
    assert!(
        again.is_error || again.text.contains("not running") || again.text.contains("already"),
        "{again:?}"
    );
}

#[tokio::test]
async fn polling_a_huge_live_log_keeps_memory_bounded() {
    let server = Server::start().await;
    let mut s = server.session().await;
    let r = s
        .call(
            "bash",
            json!({ "cmd": "sleep 300", "bg": true, "title": "quiet" }),
        )
        .await;
    let id = job_id(&r.text, "quiet");

    // Sparse: 2 GiB of apparent size, no real bytes written to disk.
    let log = server.job_dir().join(format!("{id}.log"));
    std::fs::OpenOptions::new()
        .write(true)
        .open(&log)
        .unwrap()
        .set_len(2 * GIB)
        .unwrap();

    let before = rss_kib(server.pid());
    let start = Instant::now();
    let first = s.call("job", json!({ "action": "poll", "id": id })).await;
    assert!(!first.is_error, "{first:?}");
    let older = s
        .call("job", json!({ "action": "poll", "id": id, "cursor": 200 }))
        .await;
    assert!(!older.is_error, "{older:?}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "poll took {:?}",
        start.elapsed()
    );
    let grown = rss_kib(server.pid()).saturating_sub(before);
    assert!(
        grown < 128 * 1024,
        "server RSS grew {grown} KiB polling a 2 GiB log"
    );
    s.call("job", json!({ "action": "kill", "id": id })).await;
}
