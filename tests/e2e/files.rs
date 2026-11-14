//! `file`: write/append/read, device reads, symlink-safe delete, no-clobber move.
use std::time::{Duration, Instant};

use serde_json::json;

use crate::harness::Server;

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
