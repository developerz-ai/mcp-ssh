use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::time::timeout;

use super::*;

#[test]
fn env_port_rejects_garbage_and_passes_valid_values() {
    assert_eq!(env_port(None).unwrap(), None, "unset → no override");
    assert_eq!(env_port(Some("".into())).unwrap(), None, "empty → unset");
    assert_eq!(env_port(Some("8080".into())).unwrap(), Some(8080));
    // A typo'd or out-of-range port must fail fast, not silently bind the
    // default port.
    assert!(env_port(Some("8080x".into())).is_err());
    assert!(env_port(Some("70000".into())).is_err());
}

#[tokio::test(start_paused = true)]
async fn a_server_that_never_drains_is_dropped_at_the_deadline() {
    let shutdown = CancellationToken::new();
    shutdown.cancel();
    let never = std::future::pending::<std::io::Result<()>>();
    let started = tokio::time::Instant::now();
    serve_until_drained(never, shutdown, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(10));
}

#[tokio::test(start_paused = true)]
async fn a_server_that_drains_returns_at_once() {
    let shutdown = CancellationToken::new();
    let server = async { Ok(()) };
    let started = tokio::time::Instant::now();
    serve_until_drained(server, shutdown, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[test]
fn a_created_state_dir_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("a/b");
    create_private_dir(&state).unwrap();
    for p in [dir.path().join("a"), state] {
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", p.display());
    }
}

// SIGTERM (what systemd sends on stop) must resolve `shutdown_signal` so
// axum begins draining instead of the process being killed outright.
#[tokio::test]
async fn sigterm_resolves_shutdown_signal() {
    // Install the global SIGTERM handler up front: once tokio owns the
    // signal, raising it can never fall through to the default disposition
    // (which would terminate the whole test process).
    let _guard = signal(SignalKind::terminate()).expect("install SIGTERM guard");

    let shutdown = tokio::spawn(shutdown_signal());
    // Give the spawned task a chance to register its own SIGTERM stream.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let status = std::process::Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -TERM failed");

    timeout(Duration::from_secs(5), shutdown)
        .await
        .expect("shutdown_signal did not resolve within 5s")
        .expect("shutdown task panicked");
}
