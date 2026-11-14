//! Restart persistence, bounded shutdown drain, private state-file modes.
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::json;

use crate::client::{Session, job_id, poll_until};
use crate::harness::Server;

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
    let server = Server::start_in(dir).await;

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
    // An initialized session holding its standalone SSE stream (GET /mcp) open
    // across the shutdown.
    let s = server.session().await;
    let stream = s.stream().await;
    assert_eq!(stream.status(), StatusCode::OK, "no SSE stream held open");

    let start = Instant::now();
    let (_dir, exited) = server.terminate(Duration::from_secs(15));
    assert!(exited, "still running {:?} after SIGTERM", start.elapsed());
    drop(stream);
}

#[tokio::test]
async fn db_and_job_logs_are_private() {
    let server = Server::start().await;
    let mut s = server.session().await;
    let r = s
        .call(
            "bash",
            json!({ "cmd": "echo mode-marker", "bg": true, "title": "mode" }),
        )
        .await;
    let id = job_id(&r.text, "mode");
    poll_until(&mut s, &id, "mode-marker").await;

    let mode = |p: std::path::PathBuf| std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(server.db_path()), 0o600, "DB holds tokens");
    assert_eq!(
        mode(server.job_dir().join(format!("{id}.log"))),
        0o600,
        "job output"
    );
}
