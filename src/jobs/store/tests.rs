use super::*;
use crate::db::now_unix;
use std::collections::HashSet;

fn repo() -> JobRepo {
    JobRepo::new(Db::memory())
}

/// A row exactly as it sits on disk — the columns the old inline SQL wrote,
/// read back raw so the assertions are about storage, not about the mapping.
#[derive(Debug)]
struct Columns {
    status: String,
    title: Option<String>,
    code: Option<i64>,
    error: Option<String>,
    started_unix: i64,
    tail: Option<String>,
    pgid: Option<i64>,
}

async fn columns(repo: &JobRepo, id: &str) -> Columns {
    let id = id.to_string();
    repo.db()
        .call(move |conn| {
            conn.query_row(
                "SELECT status, title, code, error, started_unix, output_tail, pgid \
                 FROM jobs WHERE id = ?1",
                [id],
                |r| {
                    Ok(Columns {
                        status: r.get(0)?,
                        title: r.get(1)?,
                        code: r.get(2)?,
                        error: r.get(3)?,
                        started_unix: r.get(4)?,
                        tail: r.get(5)?,
                        pgid: r.get(6)?,
                    })
                },
            )
        })
        .await
        .unwrap()
}

/// Seed a row the typed API can't produce (a legacy or corrupt one).
async fn seed_raw(repo: &JobRepo, sql: &'static str) {
    repo.db()
        .call(move |conn| conn.execute(sql, []))
        .await
        .unwrap();
}

#[tokio::test]
async fn insert_running_writes_the_columns_the_inline_sql_did() {
    let repo = repo();
    let id = JobId::from("job-10-00-00");
    repo.insert_running(
        &id,
        Some("deploy".into()),
        1_700_000_000,
        ProcessGroupId::new(4242),
        Some("boot-a".into()),
    )
    .await
    .unwrap();

    let row = columns(&repo, id.as_ref()).await;
    assert_eq!(
        row.status, "running",
        "the on-disk status word is unchanged"
    );
    assert_eq!(row.title.as_deref(), Some("deploy"));
    assert_eq!((row.code, row.error, row.tail), (None, None, None));
    assert_eq!(row.started_unix, 1_700_000_000);
    assert_eq!(row.pgid, Some(4242), "the pgid a later CLI kill signals");
}

#[tokio::test]
async fn finish_records_the_final_state_and_a_bounded_tail() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("j.log");
    let body: String = (1..=TAIL_BUDGET.lines + 10)
        .map(|i| format!("line{i}\n"))
        .collect();
    tokio::fs::write(&log, body).await.unwrap();

    let repo = repo();
    let id = JobId::from("job-10-00-01");
    repo.insert_running(&id, None, now_unix(), None, None)
        .await
        .unwrap();
    repo.finish(&id, &log, &JobState::Exited { code: 7 }).await;

    let row = columns(&repo, id.as_ref()).await;
    assert_eq!(
        (row.status.as_str(), row.code, row.error),
        ("exited", Some(7), None)
    );
    let tail = row.tail.expect("a finished row carries its tail");
    assert_eq!(tail.lines().count(), TAIL_BUDGET.lines, "line-bounded");
    assert!(tail.ends_with(&format!("line{}\n", TAIL_BUDGET.lines + 10)));
}

/// B3: a single 5 MiB line must not become a 5 MiB row — the tail is
/// byte-capped — and the status lands either way.
#[tokio::test]
async fn finish_byte_caps_the_tail_of_one_huge_line() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("j.log");
    tokio::fs::write(&log, "x".repeat(5 * 1024 * 1024))
        .await
        .unwrap();
    let repo = repo();
    let id = JobId::from("job-huge");
    repo.insert_running(&id, None, now_unix(), None, None)
        .await
        .unwrap();
    repo.finish(&id, &log, &JobState::Exited { code: 0 }).await;

    let row = columns(&repo, id.as_ref()).await;
    assert_eq!(row.status, "exited");
    let tail = row.tail.unwrap();
    assert!(
        tail.len() <= TAIL_BUDGET.bytes,
        "tail is {} bytes",
        tail.len()
    );
}

/// A missing log costs the tail, never the status.
#[tokio::test]
async fn finish_records_the_status_even_when_the_log_is_gone() {
    let repo = repo();
    let id = JobId::from("job-nolog");
    repo.insert_running(&id, None, now_unix(), None, None)
        .await
        .unwrap();
    repo.finish(
        &id,
        Path::new("/nonexistent.log"),
        &JobState::Exited { code: 3 },
    )
    .await;
    let row = columns(&repo, id.as_ref()).await;
    assert_eq!((row.status.as_str(), row.code), ("exited", Some(3)));
}

/// K12: the final state is decided once — a `failed` reason already recorded
/// (a CLI kill, the reconcile) is never overwritten by the waiter's exit.
#[tokio::test]
async fn a_recorded_kill_reason_survives_the_waiter_s_exit() {
    let repo = repo();
    let id = JobId::from("job-cli-killed");
    repo.insert_running(&id, None, now_unix(), None, None)
        .await
        .unwrap();
    repo.mark_killed(&id, "killed via mcp-ssh kill")
        .await
        .unwrap();
    repo.finish(
        &id,
        Path::new("/nonexistent.log"),
        &JobState::Failed {
            error: "killed by signal 15".into(),
        },
    )
    .await;
    let row = columns(&repo, id.as_ref()).await;
    assert_eq!(row.error.as_deref(), Some("killed via mcp-ssh kill"));

    // The other order: the waiter recorded the bare signal death first. The
    // kill's reason still lands — it is what that signal was.
    let id = JobId::from("job-waiter-first");
    repo.insert_running(&id, None, now_unix(), None, None)
        .await
        .unwrap();
    let died = JobState::Failed {
        error: signal_death(15),
    };
    repo.finish(&id, Path::new("/nonexistent.log"), &died).await;
    repo.mark_killed(&id, "killed via mcp-ssh kill")
        .await
        .unwrap();
    let row = columns(&repo, id.as_ref()).await;
    assert_eq!(row.error.as_deref(), Some("killed via mcp-ssh kill"));
}

#[tokio::test]
async fn find_maps_rows_back_onto_job_state() {
    let repo = repo();
    let failed = JobId::from("job-b");
    repo.insert_running(&failed, None, now_unix(), None, None)
        .await
        .unwrap();
    repo.finish(
        &failed,
        Path::new("/nonexistent.log"),
        &JobState::Failed {
            error: "boom".into(),
        },
    )
    .await;

    let found = repo.find(&failed).await.unwrap().expect("row exists");
    assert!(matches!(&found.state, JobState::Failed { error } if error == "boom"));
    assert_eq!(found.tail, "", "an unreadable log leaves an empty tail");
    assert!(repo.find(&JobId::from("ghost")).await.unwrap().is_none());
}

/// B6: newest start first, paged, with "more" answered without a full count.
#[tokio::test]
async fn summaries_page_newest_first() {
    let repo = repo();
    for (id, started) in [("b-late", 30), ("a-early", 10), ("z-mid", 20)] {
        repo.insert_running(&JobId::from(id), None, started, None, None)
            .await
            .unwrap();
    }
    let (page, more) = repo.summaries(0, 2).await.unwrap();
    let ids: Vec<&str> = page.iter().map(|s| s.id.as_ref()).collect();
    assert_eq!(ids, ["b-late", "z-mid"], "by start time, not by id");
    assert!(more);
    let (page, more) = repo.summaries(2, 2).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id.as_ref(), "a-early");
    assert!(!more);
}

/// D4: one unreadable row is skipped, not the whole listing.
#[tokio::test]
async fn summaries_skip_a_corrupt_row() {
    let repo = repo();
    repo.insert_running(&JobId::from("good"), None, 10, None, None)
        .await
        .unwrap();
    // A blob where the id text should be: the row can't be mapped.
    seed_raw(
        &repo,
        "INSERT INTO jobs (id, status, started_unix) VALUES (x'00ff', 'running', 20)",
    )
    .await;
    let (page, _) = repo.summaries(0, 10).await.unwrap();
    let ids: Vec<&str> = page.iter().map(|s| s.id.as_ref()).collect();
    assert_eq!(ids, ["good"]);
}

#[tokio::test]
async fn untracked_running_skips_live_ids() {
    let repo = repo();
    let boot = now_unix();
    let (ghost, mine) = (JobId::from("job-ghost"), JobId::from("job-mine"));
    repo.insert_running(
        &ghost,
        None,
        boot - 3600,
        ProcessGroupId::new(2_000_000_000),
        Some("boot-a".into()),
    )
    .await
    .unwrap();
    repo.insert_running(&mine, None, boot, None, None)
        .await
        .unwrap();

    let stale = repo
        .untracked_running(vec![mine.as_ref().to_string()])
        .await
        .unwrap();
    let ids: Vec<&str> = stale.iter().map(|c| c.id.as_ref()).collect();
    assert_eq!(ids, vec!["job-ghost"], "only the previous process's row");
    assert_eq!(stale[0].pgid, Some(2_000_000_000));
    assert_eq!(stale[0].boot_id.as_deref(), Some("boot-a"));

    repo.mark_restart_failed(vec![ghost.clone()]).await.unwrap();
    let reconciled = columns(&repo, ghost.as_ref()).await;
    assert_eq!(reconciled.status, "failed");
    assert_eq!(reconciled.error.as_deref(), Some("server restarted"));
    assert_eq!(columns(&repo, mine.as_ref()).await.status, "running");
}

#[tokio::test]
async fn a_finished_row_keeps_its_own_state_when_a_kill_is_recorded() {
    let repo = repo();
    let id = JobId::from("job-done");
    repo.insert_running(&id, None, now_unix(), None, None)
        .await
        .unwrap();
    repo.finish(
        &id,
        Path::new("/nonexistent.log"),
        &JobState::Exited { code: 0 },
    )
    .await;
    repo.mark_killed(&id, "killed").await.unwrap();
    let row = columns(&repo, id.as_ref()).await;
    assert_eq!(
        (row.status.as_str(), row.code, row.error),
        ("exited", Some(0), None)
    );
}

#[tokio::test]
async fn kill_target_reads_status_pgid_and_boot_verbatim() {
    let repo = repo();
    // A corrupt pgid the typed API can't write: handed back raw so the caller
    // (not the repo) refuses to signal it.
    seed_raw(
        &repo,
        "INSERT INTO jobs (id, status, started_unix, pgid, boot_id) \
         VALUES ('odd', 'exited', 1, 0, 'b1')",
    )
    .await;
    let target = repo
        .kill_target(&JobId::from("odd"))
        .await
        .unwrap()
        .expect("row exists");
    assert_eq!(target.status, "exited");
    assert_eq!(target.pgid, Some(0));
    assert_eq!(target.boot_id.as_deref(), Some("b1"));
    assert!(
        repo.kill_target(&JobId::from("ghost"))
            .await
            .unwrap()
            .is_none()
    );
}

/// Retention ages a finished job from when it *finished* (K13), falling back
/// to its start for a row predating the column.
#[tokio::test]
async fn reap_rows_age_from_the_finish() {
    let repo = repo();
    let (old, fresh) = (JobId::from("job-old"), JobId::from("job-fresh"));
    repo.insert_running(&old, None, 100, ProcessGroupId::new(7), None)
        .await
        .unwrap();
    repo.insert_running(&fresh, None, 300, None, None)
        .await
        .unwrap();
    repo.finish(
        &fresh,
        Path::new("/nonexistent.log"),
        &JobState::Exited { code: 0 },
    )
    .await;

    assert_eq!(
        repo.all_ids().await.unwrap(),
        HashSet::from(["job-old".to_string(), "job-fresh".to_string()])
    );
    let mut rows = repo.reap_rows().await.unwrap();
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(rows[0].id.as_ref(), "job-fresh");
    assert!(!rows[0].running);
    assert!(rows[0].ended_unix >= now_unix() - 5, "aged from its finish");
    assert_eq!(
        (rows[1].running, rows[1].ended_unix, rows[1].pgid),
        (true, 100, Some(7))
    );

    repo.delete(vec![old]).await.unwrap();
    assert_eq!(
        repo.all_ids().await.unwrap(),
        HashSet::from(["job-fresh".to_string()])
    );
}

#[tokio::test]
async fn listing_filters_active_unless_all() {
    let repo = repo();
    let (a, b) = (JobId::from("a"), JobId::from("b"));
    repo.insert_running(&a, Some("first".into()), 10, None, None)
        .await
        .unwrap();
    repo.insert_running(&b, None, 20, None, None).await.unwrap();
    repo.finish(
        &b,
        Path::new("/nonexistent.log"),
        &JobState::Exited { code: 0 },
    )
    .await;

    let active = repo.listing(false).await.unwrap();
    assert_eq!(active.len(), 1, "only the running job");
    assert_eq!(active[0].id, "a");
    assert_eq!(active[0].title.as_deref(), Some("first"));

    let all = repo.listing(true).await.unwrap();
    assert_eq!(all.len(), 2, "every job");
    assert_eq!(all[0].id, "b", "newest first");
    assert_eq!(all[0].code, Some(0));
}

#[test]
fn state_columns_round_trip_and_anomalies_read_as_failed() {
    for state in [
        JobState::Running,
        JobState::Exited { code: 3 },
        JobState::Failed {
            error: "boom".into(),
        },
    ] {
        let (status, code, error) = state_columns(&state);
        let back = state_from_columns(status, code.map(i64::from), error);
        match (&state, &back) {
            (JobState::Running, JobState::Running) => {}
            (JobState::Exited { code: a }, JobState::Exited { code: b }) => assert_eq!(a, b),
            (JobState::Failed { error: a }, JobState::Failed { error: b }) => assert_eq!(a, b),
            _ => panic!("{state:?} round-tripped to {back:?}"),
        }
    }
    let corrupt = state_from_columns("weird", None, None);
    assert!(matches!(corrupt, JobState::Failed { error } if error == "weird"));
    // D10: a code outside i32 must not wrap onto a different exit code.
    let wrapped = state_from_columns("exited", Some(i64::from(i32::MAX) + 1), None);
    assert!(matches!(wrapped, JobState::Failed { .. }));
}

#[test]
fn page_from_tail_serves_the_newest_lines_first() {
    let tail: String = (1..=10).map(|i| format!("line{i}\n")).collect();
    let page = page_from_tail(&tail, None, 3);
    assert_eq!(page.lines, ["line8", "line9", "line10"], "newest window");
    assert!(page.older.is_some(), "older lines remain");
    let empty = page_from_tail("", None, 3);
    assert!(empty.lines.is_empty() && empty.older.is_none());
}
