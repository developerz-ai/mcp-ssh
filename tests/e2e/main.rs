//! End-to-end: the real `mcp-ssh serve` binary, booted per test on an ephemeral
//! port with tempdir state, driven over HTTP exactly as an MCP client drives it.
//! One scenario per top audit finding, each asserting the *fixed* behaviour.
//! Run: `bin/e2e` (or `cargo test --test e2e`).
mod harness;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use harness::{PASS, Server, Session, USER, http, job_id, login, post_mcp, rss_kib};
use reqwest::{StatusCode, header};
use serde_json::json;

const GIB: u64 = 1 << 30;

/// Poll `job` until its output contains `needle` (bounded wait).
async fn poll_until(s: &mut Session, id: &str, needle: &str) -> String {
    let start = Instant::now();
    loop {
        let r = s.call("job", json!({ "action": "poll", "id": id })).await;
        if r.text.contains(needle) {
            return r.text;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "{id} never showed {needle:?}: {}",
            r.text
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

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

/// The cursor a poll reply names for the next (older) page: `cursor=<n>`.
fn next_cursor(text: &str) -> u64 {
    let start = text
        .find("cursor=")
        .unwrap_or_else(|| panic!("no next cursor in: {text}"))
        + "cursor=".len();
    let digits: String = text[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("numeric cursor")
}

fn alive(pid: &str) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

async fn wait_dead(pid: &str) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ---- auth ----------------------------------------------------------------

#[tokio::test]
async fn mcp_rejects_every_non_bearer_request() {
    let server = Server::start().await;
    let http = http();

    let health = http
        .get(format!("{}/healthz", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(health.text().await.unwrap(), "ok");

    let url = format!("{}/mcp", server.base);
    let none = http.post(&url).send().await.unwrap();
    let basic = http
        .post(&url)
        .basic_auth(USER, Some(PASS))
        .send()
        .await
        .unwrap();
    let bogus = http
        .post(&url)
        .bearer_auth("not-a-token")
        .send()
        .await
        .unwrap();
    for (what, resp) in [("no auth", none), ("basic", basic), ("bad bearer", bogus)] {
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{what}");
        assert!(
            resp.headers().contains_key(header::WWW_AUTHENTICATE),
            "{what}: 401 must carry the resource-metadata challenge"
        );
    }
}

#[tokio::test]
async fn bad_host_with_a_good_bearer_is_forbidden() {
    let server = Server::start().await;
    let token = login(&server.base).await;
    let resp = http()
        .post(format!("{}/mcp", server.base))
        .bearer_auth(&token)
        .header(header::HOST, "evil.example")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&json!({ "jsonrpc": "2.0", "id": 0, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn get_authorize_with_basic_creds_never_issues_a_code() {
    let server = Server::start().await;
    let http = http();
    let reg: serde_json::Value = http
        .post(format!("{}/register", server.base))
        .json(&json!({ "redirect_uris": ["http://localhost/cb"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = http
        .get(format!("{}/authorize", server.base))
        .basic_auth(USER, Some(PASS))
        .query(&[
            ("response_type", "code"),
            ("client_id", reg["client_id"].as_str().unwrap()),
            ("redirect_uri", "http://localhost/cb"),
            (
                "code_challenge",
                "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            ),
            ("code_challenge_method", "S256"),
        ])
        .send()
        .await
        .unwrap();
    assert!(!resp.status().is_redirection(), "GET issued a redirect");
    assert!(!resp.text().await.unwrap().contains("code="));
}

#[tokio::test]
async fn empty_password_refuses_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut child =
        harness::spawn(dir.path(), harness::free_port(), &[("MCP_SSH_PASS", "")]).unwrap();
    let exited = harness::wait_exit(&mut child, Duration::from_secs(10));
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(exited, "server booted with an empty password");
    assert!(!child.wait().unwrap().success());
}

// ---- bash ----------------------------------------------------------------

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
            json!({ "cmd": "echo now", "bg": true, "title": "bgjob" }),
        )
        .await;
    let id = job_id(&r.text, "bgjob");
    poll_until(&mut s, &id, "now").await;

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

// ---- job -----------------------------------------------------------------

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
        newest.text.contains("50") && !newest.text.contains("\n44\n"),
        "{newest:?}"
    );
    let older = s
        .call(
            "job",
            json!({ "action": "poll", "id": id, "limit": 5, "cursor": next_cursor(&newest.text) }),
        )
        .await;
    assert!(
        older.text.contains("45") && !older.text.contains("50"),
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
    s.call("job", json!({ "action": "poll", "id": id })).await;
    s.call("job", json!({ "action": "poll", "id": id, "cursor": 200 }))
        .await;
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

// ---- file ----------------------------------------------------------------

#[tokio::test]
async fn file_ops_and_their_edge_cases() {
    let server = Server::start().await;
    let mut s = server.session().await;
    let root = server.dir.path();
    let p = |name: &str| root.join(name).display().to_string();

    let r = s
        .call(
            "file",
            json!({ "action": "write", "path": p("a.txt"), "content": "one\n" }),
        )
        .await;
    assert!(!r.is_error, "{r:?}");
    s.call(
        "file",
        json!({ "action": "append", "path": p("a.txt"), "content": "two\n" }),
    )
    .await;
    let r = s
        .call("file", json!({ "action": "read", "path": p("a.txt") }))
        .await;
    assert!(r.text.contains("one") && r.text.contains("two"), "{r:?}");

    // A device never ends: refuse it promptly instead of pinning a core.
    let start = Instant::now();
    let r = s
        .call("file", json!({ "action": "read", "path": "/dev/zero" }))
        .await;
    assert!(r.is_error, "{r:?}");
    assert!(start.elapsed() < Duration::from_secs(5));

    // `delete link/` removes the link, never the directory it points at.
    std::fs::create_dir(root.join("target")).unwrap();
    std::fs::write(root.join("target/keep.txt"), "keep").unwrap();
    std::os::unix::fs::symlink(root.join("target"), root.join("link")).unwrap();
    s.call(
        "file",
        json!({ "action": "delete", "path": format!("{}/", p("link")) }),
    )
    .await;
    assert!(
        root.join("target/keep.txt").exists(),
        "symlink target was emptied"
    );

    // A non-empty directory needs recursive=true.
    let r = s
        .call("file", json!({ "action": "delete", "path": p("target") }))
        .await;
    assert!(r.is_error && root.join("target/keep.txt").exists(), "{r:?}");

    // move never clobbers.
    std::fs::write(root.join("b.txt"), "b").unwrap();
    let r = s
        .call(
            "file",
            json!({ "action": "move", "src": p("a.txt"), "dest": p("b.txt") }),
        )
        .await;
    assert!(r.is_error, "{r:?}");
    assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "b");
    assert!(root.join("a.txt").exists());
}

// ---- lifecycle -----------------------------------------------------------

#[tokio::test]
async fn restart_keeps_tokens_and_history() {
    let server = Server::start().await;
    let mut s = server.session().await;
    let r = s
        .call(
            "bash",
            json!({ "cmd": "echo persisted", "bg": true, "title": "hist" }),
        )
        .await;
    let id = job_id(&r.text, "hist");
    poll_until(&mut s, &id, "persisted").await;
    let token = s.token().to_string();

    let (dir, exited) = server.terminate(Duration::from_secs(15));
    assert!(exited, "server did not stop");
    let server = Server::start_in(dir, &[]).await;

    let mut s = Session::open(&server.base, &token).await;
    let list = s.call("job", json!({ "action": "list" })).await;
    assert!(
        list.text.contains(&id),
        "history lost across restart: {list:?}"
    );
    let r = s.call("job", json!({ "action": "poll", "id": id })).await;
    assert!(r.text.contains("persisted"), "{r:?}");
}

#[tokio::test]
async fn sigterm_with_an_open_stream_exits_within_the_drain_deadline() {
    let server = Server::start().await;
    let token = login(&server.base).await;
    let s = Session::open(&server.base, &token).await;
    drop(s);
    // Hold a standalone SSE stream open (GET /mcp) across the shutdown.
    let init = json!({
        "jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": { "protocolVersion": "2025-03-26", "capabilities": {},
                    "clientInfo": { "name": "e2e", "version": "0" } }
    });
    let http = http();
    let resp = post_mcp(&http, &server.base, &token, None, &init).await;
    let sid = resp.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let _stream = http
        .get(format!("{}/mcp", server.base))
        .bearer_auth(&token)
        .header("mcp-session-id", sid)
        .header(header::ACCEPT, "text/event-stream")
        .send()
        .await
        .unwrap();

    let start = Instant::now();
    let (_dir, exited) = server.terminate(Duration::from_secs(15));
    assert!(exited, "still running {:?} after SIGTERM", start.elapsed());
}

#[tokio::test]
async fn db_and_job_logs_are_private() {
    let server = Server::start().await;
    let mut s = server.session().await;
    let r = s
        .call(
            "bash",
            json!({ "cmd": "echo x", "bg": true, "title": "mode" }),
        )
        .await;
    let id = job_id(&r.text, "mode");
    poll_until(&mut s, &id, "x").await;

    let mode = |p: std::path::PathBuf| std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(server.db_path()), 0o600, "DB holds tokens");
    assert_eq!(
        mode(server.job_dir().join(format!("{id}.log"))),
        0o600,
        "job output"
    );
}
