use super::*;

#[test]
fn humanize_duration_scales_units() {
    assert_eq!(humanize_duration(0), "0s");
    assert_eq!(humanize_duration(-5), "0s");
    assert_eq!(humanize_duration(45), "45s");
    assert_eq!(humanize_duration(12 * 60), "12m");
    assert_eq!(humanize_duration(3_600 + 12 * 60), "1h 12m");
    assert_eq!(humanize_duration(3 * 86_400 + 4 * 3_600), "3d 4h");
}

#[test]
fn render_jobs_aligns_and_prefers_error_for_failures() {
    let rows = vec![
        JobRow {
            id: "minime-v1-07-16-59".into(),
            status: "running".into(),
            code: None,
            error: None,
            started_unix: 1_700_000_000,
            title: Some("minime-v1".into()),
        },
        JobRow {
            id: "job-01".into(),
            status: "failed".into(),
            code: None,
            error: Some("server restarted".into()),
            started_unix: 1_700_000_100,
            title: None,
        },
    ];
    let out = render_jobs(&rows);
    assert!(out.contains("ID"), "has a header");
    // Column width is driven by the longest id, so the short id is padded.
    assert!(
        out.contains("job-01             "),
        "short id padded to the widest id:\n{out}"
    );
    assert!(
        out.contains("server restarted"),
        "failure shows its error, not its (absent) title:\n{out}"
    );
    assert!(
        out.contains("minime-v1"),
        "running row shows its title:\n{out}"
    );
}

/// A repository over a fresh in-memory DB, plus the raw handle the tests use to
/// plant rows the typed API can't produce (legacy or corrupt ones) and to read
/// columns back.
fn repo() -> (JobRepo, Db) {
    let db = Db::memory();
    (JobRepo::new(db.clone()), db)
}

#[tokio::test]
async fn kill_job_reports_unknown_and_finished_without_signalling() {
    let (repo, db) = repo();
    assert_eq!(
        kill_job(&repo, "ghost").await.unwrap(),
        (false, "no such job: ghost".to_string())
    );

    db.call(|conn| {
        conn.execute(
            "INSERT INTO jobs (id, status, code, started_unix) VALUES ('done', 'exited', 0, 1)",
            [],
        )
    })
    .await
    .unwrap();
    assert_eq!(
        kill_job(&repo, "done").await.unwrap(),
        (
            false,
            "job done is not running (status: exited)".to_string()
        )
    );
}

#[tokio::test]
async fn kill_job_without_pgid_cannot_signal() {
    let (repo, db) = repo();
    // A running row from before pgid tracking (pgid is NULL).
    db.call(|conn| {
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix) VALUES ('legacy', 'running', 1)",
            [],
        )
    })
    .await
    .unwrap();
    let (done, msg) = kill_job(&repo, "legacy").await.unwrap();
    assert!(!done);
    assert!(
        msg.contains("no recorded process group"),
        "must refuse to guess a pgid: {msg}"
    );
    // The row is untouched — we didn't fabricate a kill.
    let status: String = db
        .call(|conn| {
            conn.query_row("SELECT status FROM jobs WHERE id = 'legacy'", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(status, "running");
}

/// True once `pid` is no longer signalable, polled up to `deadline`. A killed
/// process stays signalable until its parent reaps it, so a single probe right
/// after the kill would be a race; a bounded poll is the deterministic form.
#[cfg(unix)]
async fn gone_within(pid: u32, deadline: std::time::Duration) -> bool {
    let start = tokio::time::Instant::now();
    loop {
        if !crate::jobs::pid_alive(pid) {
            return true;
        }
        if start.elapsed() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn kill_job_signals_a_real_process_group_and_updates_row() {
    use std::process::Stdio;
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("descendant.pid");
    // Leader of its own group (pgid == pid) with a background descendant in that
    // same group — the shape of a real job's shell. The descendant is what makes
    // this a *group* test: a pid-only signal would leave it running.
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "sleep 300 & echo $! > '{}'; wait",
            pidfile.display()
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = i64::from(child.id().expect("child pid"));
    // Reap in the background so the signalled child leaves no zombie — mirrors
    // the real parent (server/init) reaping it.
    let waiter = tokio::spawn(async move { child.wait().await });
    let descendant = read_pid(&pidfile).await;

    let (repo, db) = repo();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO jobs (id, status, pgid, started_unix) VALUES ('real', 'running', ?1, 1)",
            [pid],
        )
    })
    .await
    .unwrap();

    let msg = kill_job(&repo, "real").await.unwrap();
    assert_eq!(
        msg,
        (true, "killed real".to_string()),
        "the whole group must be gone"
    );
    let exit = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("group leader should exit after kill_job")
        .expect("wait task should not panic")
        .expect("waiting on the leader should succeed");
    assert!(
        !exit.success(),
        "leader exits from the signal, not normally"
    );
    assert!(
        gone_within(descendant, std::time::Duration::from_secs(5)).await,
        "descendant {descendant} outlived the group kill"
    );

    let status: String = db
        .call(|conn| {
            conn.query_row("SELECT status FROM jobs WHERE id = 'real'", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(
        status, "failed",
        "row transitions out of running once the group is signalled"
    );
}

/// The pid the shell wrote for its background child, once the write lands.
#[cfg(unix)]
async fn read_pid(path: &std::path::Path) -> u32 {
    for _ in 0..100 {
        if let Ok(text) = tokio::fs::read_to_string(path).await
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("shell never recorded its background child's pid");
}

#[tokio::test]
async fn kill_job_rejects_a_corrupt_pgid_without_signalling() {
    let (repo, db) = repo();
    // Neither value can come from a real job's process group: negative is outside
    // u32, and `-0` is *this* process's own group. Both simulate a corrupt row
    // (hand-edited DB, or a future bug writing garbage into the column).
    for (id, pgid) in [("negative", -1), ("zero", 0)] {
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, pgid, started_unix) VALUES (?1, 'running', ?2, 1)",
                rusqlite::params![id, pgid],
            )
        })
        .await
        .unwrap();

        let (done, msg) = kill_job(&repo, id).await.unwrap();
        assert!(
            !done && msg.contains("corrupt pgid"),
            "must refuse to signal pgid {pgid}: {msg}"
        );
        let lookup = id.to_string();
        let status: String = db
            .call(move |conn| {
                conn.query_row("SELECT status FROM jobs WHERE id = ?1", [lookup], |r| {
                    r.get(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(status, "running", "row untouched — no signal was sent");
    }
}

#[tokio::test]
async fn render_sessions_counts_without_leaking_tokens() {
    let db = Db::memory();
    let now = now_unix();
    let (active_exp, expired_exp) = (now + 3_600, now - 1);
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO access_tokens (token_hash, client_id, family_id, expires_unix) \
             VALUES ('AAA', 'c', 'f', ?1)",
            [active_exp],
        )?;
        conn.execute(
            "INSERT INTO access_tokens (token_hash, client_id, family_id, expires_unix) \
             VALUES ('BBB', 'c', 'f', ?1)",
            [expired_exp],
        )?;
        conn.execute(
            "INSERT INTO refresh_tokens \
             (token_hash, client_id, family_id, expires_unix, family_expires_unix) \
             VALUES ('CCC', 'c', 'f', ?1, ?1)",
            [active_exp],
        )
    })
    .await
    .unwrap();

    let access = token_stats(&db, "access_tokens", now).await.unwrap();
    let refresh = token_stats(&db, "refresh_tokens", now).await.unwrap();
    assert_eq!((access.active, access.expired), (1, 1));
    assert_eq!((refresh.active, refresh.expired), (1, 0));

    let out = render_sessions("/tmp/x.db", &access, &refresh, now);
    assert!(out.contains("access tokens : 1 active, 1 expired"), "{out}");
    assert!(out.contains("refresh tokens: 1 active, 0 expired"), "{out}");
    // The whole point: token values never appear.
    for secret in ["AAA", "BBB", "CCC"] {
        assert!(
            !out.contains(secret),
            "token value leaked into output: {out}"
        );
    }
}
