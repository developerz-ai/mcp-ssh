use super::*;
use crate::db::Db;
use crate::jobs::{JobId, JobMeta, JobRepo};

fn repo() -> (JobRepo, Db) {
    let db = Db::memory();
    (JobRepo::new(db.clone()), db)
}

/// Plant one row verbatim — `status`/`pgid` combinations `insert_running` can't
/// write, which is exactly what the refusals guard against.
async fn plant(db: &Db, id: &'static str, status: &'static str, pgid: Option<i64>) {
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix, pgid) VALUES (?1, ?2, 1, ?3)",
            rusqlite::params![id, status, pgid],
        )
    })
    .await
    .unwrap();
}

async fn status_of(db: &Db, id: &'static str) -> Option<String> {
    use rusqlite::OptionalExtension;
    db.call(move |conn| {
        conn.query_row("SELECT status FROM jobs WHERE id = ?1", [id], |r| r.get(0))
            .optional()
    })
    .await
    .unwrap()
}

async fn kill(repo: &JobRepo, id: &str) -> KillOutcome {
    kill_persisted(repo, &JobId::from(id), "killed")
        .await
        .unwrap()
}

/// A detached group like a real job's: its leader leads it (pgid == pid) and
/// is reaped in the background, as the server or init would.
fn spawn_group(script: &str) -> (ProcessGroupId, tokio::task::JoinHandle<()>) {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = ProcessGroupId::new(child.id().unwrap()).unwrap();
    let waiter = tokio::spawn(async move {
        let _ = child.wait().await;
    });
    (pgid, waiter)
}

/// errno → `Probe`: an error is never "gone". Signal 0 delivers nothing.
#[test]
fn errno_maps_onto_probe_without_reading_failure_as_gone() {
    // pid 1 exists; as non-root that's EPERM — alive, not ours. As root, 0.
    assert!(matches!(send(1, 0), Probe::Alive));
    // Above any pid_max: ESRCH.
    assert!(matches!(send(-2_000_000_000, 0), Probe::Gone));
    // An invalid signal is EINVAL: unknown, not gone and not alive.
    let me = libc::pid_t::try_from(std::process::id()).unwrap();
    assert!(matches!(send(me, 9999), Probe::Unknown(_)));
}

/// The invariant the type carries: no corrupt persisted pgid becomes a signal
/// target. `0` would hit this process's own group, `1` (`kill -1`) every
/// process we may signal, and anything past `i32` wraps onto a wrong group.
#[test]
fn corrupt_persisted_pgids_never_become_a_signal_target() {
    for corrupt in [0, 1, -1, i64::from(i32::MAX) + 1, i64::from(u32::MAX) + 1] {
        assert!(
            ProcessGroupId::from_persisted(corrupt).is_none(),
            "pgid {corrupt} must not be signalable"
        );
    }
    assert!(ProcessGroupId::from_persisted(1234).is_some());
    assert!(ProcessGroupId::new(1).is_none());
}

#[test]
fn a_pgid_from_another_boot_is_never_signalled() {
    let Some(now) = current_boot_id() else {
        return; // no boot id on this kernel: every row is probed
    };
    assert!(persisted_group(Some(4242), Some(now)).is_some());
    assert!(persisted_group(Some(4242), None).is_some(), "legacy row");
    assert!(persisted_group(Some(4242), Some("another-boot")).is_none());
    assert!(matches!(
        probe_persisted(Some(4242), Some("another-boot")),
        Probe::Gone
    ));
}

#[tokio::test]
async fn unsignalable_rows_are_refused_and_left_untouched() {
    let (repo, db) = repo();
    plant(&db, "done", "exited", Some(2_000_000_000)).await;
    plant(&db, "legacy", "running", None).await;
    plant(&db, "zero", "running", Some(0)).await;
    plant(&db, "one", "running", Some(1)).await;
    plant(&db, "wrapped", "running", Some(i64::from(u32::MAX) + 1)).await;

    assert!(matches!(kill(&repo, "ghost").await, KillOutcome::Unknown));
    assert!(
        matches!(kill(&repo, "done").await, KillOutcome::NotRunning { status } if status == "exited"),
        "a finished row whose group is gone is not signalled"
    );
    assert!(matches!(
        kill(&repo, "legacy").await,
        KillOutcome::NoProcessGroup
    ));
    // `0` is the dangerous one: it would signal *this* process's own group, so
    // this test is its own canary — a regression kills the runner.
    for (id, pgid) in [("zero", 0), ("one", 1)] {
        assert!(
            matches!(kill(&repo, id).await, KillOutcome::CorruptProcessGroup { pgid: p } if p == pgid)
        );
    }
    assert!(matches!(
        kill(&repo, "wrapped").await,
        KillOutcome::CorruptProcessGroup { .. }
    ));
    for id in ["legacy", "zero", "one", "wrapped"] {
        assert_eq!(status_of(&db, id).await.as_deref(), Some("running"), "{id}");
    }
    assert_eq!(status_of(&db, "done").await.as_deref(), Some("exited"));
}

#[tokio::test]
async fn a_gone_group_is_recorded_failed_with_the_caller_s_reason() {
    let (repo, db) = repo();
    plant(&db, "gone", "running", Some(2_000_000_000)).await;

    let outcome = kill_persisted(&repo, &JobId::from("gone"), "killed via mcp-ssh kill")
        .await
        .unwrap();
    assert!(matches!(outcome, KillOutcome::Killed));

    let row: (String, Option<String>) = db
        .call(|conn| {
            conn.query_row(
                "SELECT status, error FROM jobs WHERE id = 'gone'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert_eq!(row.0, "failed");
    assert_eq!(row.1.as_deref(), Some("killed via mcp-ssh kill"));
}

#[tokio::test]
async fn escalate_terminates_a_detached_group() {
    let (pgid, waiter) = spawn_group("sleep 300");
    assert!(matches!(
        escalate(pgid, Kill::Graceful).await,
        Escalation::Gone
    ));
    let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
}

#[tokio::test]
async fn escalate_on_a_dead_pgid_reports_gone() {
    let dead = ProcessGroupId::from_persisted(2_000_000_000).unwrap();
    assert!(matches!(
        escalate(dead, Kill::Graceful).await,
        Escalation::Gone
    ));
}

/// K3: a TERM-ignoring descendant keeps the group alive after the leader dies;
/// success is the group gone, so the KILL must still reach it.
#[tokio::test]
async fn escalate_reaches_a_term_ignoring_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pid");
    let (pgid, waiter) = spawn_group(&format!(
        "(trap '' TERM; sleep 300) & echo $! > '{}'; wait",
        pidfile.display()
    ));
    let descendant = loop {
        if let Ok(text) = std::fs::read_to_string(&pidfile)
            && let Ok(pid) = text.trim().parse::<libc::pid_t>()
        {
            break pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(matches!(
        escalate(pgid, Kill::Graceful).await,
        Escalation::Gone
    ));
    assert!(
        matches!(send(descendant, 0), Probe::Gone),
        "descendant reaped"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
}

/// K4: a finished row whose group still holds a process (`server &`) kills the
/// leftovers rather than answering "not running".
#[tokio::test]
async fn a_finished_row_with_live_leftovers_kills_them() {
    let (pgid, waiter) = spawn_group("sleep 300");
    let (repo, db) = repo();
    plant(&db, "left", "exited", Some(i64::from(pgid.get()))).await;
    assert!(matches!(
        kill(&repo, "left").await,
        KillOutcome::KilledLeftovers
    ));
    assert!(matches!(probe(pgid), Probe::Gone));
    assert_eq!(
        status_of(&db, "left").await.as_deref(),
        Some("exited"),
        "the job's own exit stays its final state"
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), waiter).await;
}

/// #69: a kill the group outlived (`Survived`, `NotSignalled`) must not name
/// itself the cause of an exit the job reaches later on its own.
#[tokio::test]
async fn a_kill_that_did_not_take_is_not_blamed_for_the_job_s_later_exit() {
    use crate::jobs::{JobState, JobStore, RunResult, Shell};
    let dir = tempfile::tempdir().unwrap().keep();
    let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), Db::memory()).unwrap();
    let RunResult::Backgrounded { id } = store
        .run(
            "sleep 0.5; exit 3".into(),
            None,
            None,
            true,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("bg");
    };
    let job = store.jobs.lock().await.get(&id).cloned().expect("tracked");

    let escalation = kill::blamed(&job, "killed", async { Escalation::Survived }).await;
    assert!(matches!(escalation, Escalation::Survived));

    for _ in 0..100 {
        let (state, _) = store.poll(&id, None, None).await.unwrap().unwrap();
        match state {
            JobState::Running => tokio::time::sleep(Duration::from_millis(50)).await,
            JobState::Exited { code: 3 } => return,
            other => panic!("the job's own exit was not recorded: {other:?}"),
        }
    }
    panic!("job never finished");
}
