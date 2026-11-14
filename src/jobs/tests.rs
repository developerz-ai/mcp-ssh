use super::log::DEFAULT_PAGE;
use super::supervise::{LEFTOVERS_LOG_CAP_REASON, LOG_CAP_REASON};
use super::*;
use crate::db::Db;
use rusqlite::OptionalExtension;
use std::{collections::HashSet, process::Stdio};

fn store(inline: Duration) -> JobStore {
    let dir = tempfile::tempdir().unwrap().keep();
    JobStore::new(dir, inline, Shell::sh(), Db::memory()).unwrap()
}

/// Wait for the waiter task to persist a job's final state. Returns the
/// finished row's `(status, code, error, output_tail)`.
async fn await_row(db: &Db, id: &JobId) -> (String, Option<i64>, Option<String>, Option<String>) {
    for _ in 0..200 {
        let row_id = id.as_ref().to_string();
        let row = db
            .call(move |conn| {
                conn.query_row(
                    "SELECT status, code, error, output_tail FROM jobs WHERE id = ?1",
                    [row_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()
            })
            .await
            .unwrap();
        if let Some(row) = row
            && row.0 != "running"
        {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("job row never reached a finished state");
}

/// Wait for a finished row's output tail, written just after its status.
async fn await_tail(db: &Db, id: &JobId) -> Option<String> {
    for _ in 0..200 {
        let tail = await_row(db, id).await.3;
        if tail.is_some() {
            return tail;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

async fn row_status(db: &Db, id: &str) -> Option<String> {
    let id = id.to_string();
    db.call(move |conn| {
        conn.query_row("SELECT status FROM jobs WHERE id = ?1", [id], |r| {
            r.get::<_, String>(0)
        })
        .optional()
    })
    .await
    .unwrap()
}

fn alive(pid: &str) -> bool {
    pid_alive(pid.parse().unwrap())
}

async fn bg(store: &JobStore, cmd: &str) -> JobId {
    match store
        .run(cmd.into(), None, None, true, false, JobMeta::default())
        .await
        .unwrap()
    {
        RunResult::Backgrounded { id } => id,
        RunResult::Inline { .. } => panic!("bg should background"),
    }
}

/// Poll until the job's log shows a `pid:<n>` line; returns `<n>`.
async fn logged_pid(store: &JobStore, id: &JobId) -> String {
    for _ in 0..100 {
        let (_s, page) = store.poll(id, None, None).await.unwrap().unwrap();
        if let Some(pid) = page.lines.iter().find_map(|l| l.strip_prefix("pid:")) {
            return pid.trim().to_string();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw the logged pid");
}

async fn await_state(store: &JobStore, id: &JobId) -> JobState {
    for _ in 0..200 {
        let (state, _) = store.poll(id, None, None).await.unwrap().unwrap();
        if !matches!(state, JobState::Running) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job never finished");
}

async fn gone_soon(pid: &str) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The production shell (interactive bash) must expand aliases defined in
/// the sourced rc file, and its startup job-control warnings must NOT leak
/// into the captured log.
#[tokio::test]
async fn interactive_shell_expands_rc_aliases_without_leaking_startup_noise() {
    let dir = tempfile::tempdir().unwrap().keep();
    let rc = dir.join("rc");
    std::fs::write(&rc, "alias greet='echo ALIAS_OK'\n").unwrap();
    let store = JobStore::new(
        dir.join("jobs"),
        Duration::from_secs(5),
        Shell::bash_with_rcfile(rc.to_str().unwrap()),
        Db::memory(),
    )
    .unwrap();

    let r = store
        .run("greet".into(), None, None, false, true, JobMeta::default())
        .await
        .unwrap();
    let RunResult::Inline { state, page, .. } = r else {
        panic!("fast command should be inline");
    };
    assert!(matches!(state, JobState::Exited { code: 0 }));
    assert!(
        page.lines.iter().any(|l| l.contains("ALIAS_OK")),
        "{:?}",
        page.lines
    );
    assert!(!page.lines.iter().any(|l| l.contains("no job control")));
}

#[tokio::test]
async fn fast_command_returns_inline() {
    let r = store(Duration::from_secs(5))
        .run(
            "echo hello".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap();
    let RunResult::Inline { state, page, .. } = r else {
        panic!("fast command should be inline");
    };
    assert!(matches!(state, JobState::Exited { code: 0 }));
    assert_eq!(page.lines, ["hello"]);
    assert_eq!(page.older, None);
}

/// #69: every directory the engine creates on the way to the job dir is
/// owner-only from creation — `logs/` in the default `…/logs/jobs` included.
#[test]
fn the_job_dir_s_missing_parents_are_created_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let base = tempfile::tempdir().unwrap();
    let logs = base.path().join("logs");
    create_private_dir(&logs.join("jobs")).unwrap();
    for dir in [logs.join("jobs"), logs] {
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "{}", dir.display());
    }
}

/// D1: job output may hold secrets — the dir is 0700, each log 0600.
#[tokio::test]
async fn job_dir_and_logs_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("jobs");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
    let store = JobStore::new(
        dir.clone(),
        Duration::from_secs(5),
        Shell::sh(),
        Db::memory(),
    )
    .unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700, "a loose pre-existing dir is tightened");
    let RunResult::Inline { id, .. } = store
        .run(
            "echo s3cret".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("inline");
    };
    assert_eq!(mode(&dir.join(format!("{id}.log"))), 0o600);
}

/// D6: the row stores the normalized label, never the raw agent string.
#[tokio::test]
async fn title_labels_the_id_and_the_row_stores_it_normalized() {
    let store = store(Duration::from_secs(5));
    let r = store
        .run(
            "echo hi".into(),
            None,
            None,
            true,
            false,
            JobMeta::titled("deploy check\x1b[2J\nforged".into()),
        )
        .await
        .unwrap();
    let RunResult::Backgrounded { id } = r else {
        panic!("bg should background");
    };
    assert!(id.as_ref().starts_with("deploy-check-2J-forged-"), "{id}");
    let row_id = id.as_ref().to_string();
    let title: Option<String> = store
        .repo
        .db()
        .call(move |c| {
            c.query_row("SELECT title FROM jobs WHERE id = ?1", [row_id], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(title.as_deref(), Some("deploy-check-2J-forged"));
}

/// K14: a bad `cwd` fails by name and leaves no reserved log behind.
#[tokio::test]
async fn a_bad_cwd_is_named_and_leaves_nothing_behind() {
    let store = store(Duration::from_secs(5));
    let missing = store.dir.join("no-such-dir").to_string_lossy().into_owned();
    let Err(error) = store
        .run(
            "true".into(),
            Some(missing.clone()),
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
    else {
        panic!("a missing cwd must fail");
    };
    assert!(
        matches!(&error, JobError::Cwd { cwd, .. } if *cwd == missing),
        "{error:?}"
    );

    let file = store.dir.join("not-a-dir");
    std::fs::write(&file, "").unwrap();
    let file = file.to_string_lossy().into_owned();
    let Err(error) = store
        .run(
            "true".into(),
            Some(file),
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
    else {
        panic!("a file cwd must fail");
    };
    assert!(matches!(error, JobError::CwdNotADirectory { .. }));
    let logs = std::fs::read_dir(&store.dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "log")
        })
        .count();
    assert_eq!(logs, 0, "no reserved log left behind");
}

#[tokio::test]
async fn inline_overflow_surfaces_id_and_remains_pollable() {
    let store = store(Duration::from_secs(5));
    let n = DEFAULT_PAGE + 50;
    let r = store
        .run(
            format!("seq 1 {n}"),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap();
    let RunResult::Inline { id, page, .. } = r else {
        panic!("seq is fast — should be inline");
    };
    assert_eq!(page.lines.len(), DEFAULT_PAGE, "first page is capped");
    assert_eq!(page.lines.last().unwrap(), &n.to_string(), "newest page");
    let older = page.older.expect("output exceeds one page");
    let (_state, rest) = store.poll(&id, Some(older), None).await.unwrap().unwrap();
    assert_eq!(rest.lines.first().map(String::as_str), Some("1"));
    assert_eq!(rest.lines.len(), 50);
    assert_eq!(rest.older, None);
}

/// Concurrent launches sharing a title land on the same `<title>-HH-MM-SS`
/// base: each must still get its own id and its own log.
#[tokio::test]
async fn concurrent_launches_never_share_an_id_or_a_log() {
    const N: usize = 8;
    let store = store(Duration::from_secs(5));
    let launches: Vec<_> = (0..N)
        .map(|i| {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .run(
                        format!("echo marker-{i}"),
                        None,
                        None,
                        true,
                        false,
                        JobMeta::titled("race".into()),
                    )
                    .await
            })
        })
        .collect();
    let mut ids = Vec::new();
    for launch in launches {
        let RunResult::Backgrounded { id } = launch.await.unwrap().unwrap() else {
            panic!("bg should background");
        };
        ids.push(id);
    }
    let unique: HashSet<&str> = ids.iter().map(|id| id.as_ref()).collect();
    assert_eq!(unique.len(), N, "concurrent ids must be distinct: {ids:?}");
    for (i, id) in ids.iter().enumerate() {
        await_state(&store, id).await;
        let (_, page) = store.poll(id, None, None).await.unwrap().unwrap();
        assert_eq!(page.lines, vec![format!("marker-{i}")], "log {id}");
    }
}

#[tokio::test]
async fn slow_command_backgrounds_then_completes() {
    let store = store(Duration::from_millis(100));
    let r = store
        .run(
            "echo start; sleep 1; echo done".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap();
    let RunResult::Backgrounded { id } = r else {
        panic!("slow command should background");
    };
    assert!(matches!(
        await_state(&store, &id).await,
        JobState::Exited { code: 0 }
    ));
    let (_, page) = store.poll(&id, None, None).await.unwrap().unwrap();
    assert_eq!(page.lines, ["start", "done"]);
}

/// K11: a death by signal is reported as such, not as `exited -1`.
#[tokio::test]
async fn a_signal_death_is_reported_by_signal() {
    let store = store(Duration::from_secs(5));
    let RunResult::Inline { state, .. } = store
        .run(
            "kill -9 $$".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("inline");
    };
    assert!(
        matches!(&state, JobState::Failed { error } if error == "killed by signal 9"),
        "{state:?}"
    );
}

#[tokio::test]
async fn kill_reaches_descendants_and_records_the_kill() {
    let store = store(Duration::from_millis(100));
    let id = bg(&store, "sleep 300 & echo \"pid:$!\"; wait").await;
    let child = logged_pid(&store, &id).await;
    assert!(alive(&child), "descendant should be running");

    assert!(matches!(
        store.kill(&id).await.unwrap(),
        KillOutcome::Killed
    ));
    assert!(gone_soon(&child).await, "descendant survived the kill");
    assert!(
        matches!(await_state(&store, &id).await, JobState::Failed { error } if error == "killed")
    );
}

/// A backgrounded interactive job that has printed `started` and is now in its
/// `sleep`, with `echo AFTER` still to run.
async fn started_interactive_job() -> (JobStore, JobId) {
    let dir = tempfile::tempdir().unwrap().keep();
    let rc = dir.join("rc");
    std::fs::write(&rc, "").unwrap();
    let store = JobStore::new(
        dir.join("jobs"),
        Duration::from_millis(100),
        Shell::bash_with_rcfile(rc.to_str().unwrap()),
        Db::memory(),
    )
    .unwrap();
    let RunResult::Backgrounded { id } = store
        .run(
            "echo started; sleep 30; echo AFTER".into(),
            None,
            None,
            true,
            true,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("bg");
    };
    for _ in 0..100 {
        let (_, page) = store.poll(&id, None, None).await.unwrap().unwrap();
        if page.lines.iter().any(|l| l == "started") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    (store, id)
}

async fn assert_never_ran_after(store: &JobStore, id: &JobId) {
    await_state(store, id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, page) = store.poll(id, None, None).await.unwrap().unwrap();
    assert!(!page.lines.iter().any(|l| l == "AFTER"), "{:?}", page.lines);
}

/// K2: an interactive bash ignores `SIGTERM`. Killing the job must not let
/// it go on to run the next command once its foreground child dies.
#[tokio::test]
async fn killing_an_interactive_job_never_runs_its_next_command() {
    let (store, id) = started_interactive_job().await;
    assert!(matches!(
        store.kill(&id).await.unwrap(),
        KillOutcome::Killed
    ));
    assert_never_ran_after(&store, &id).await;
}

/// #69: the same holds for a kill that has only the row to go on (`mcp-ssh job
/// kill`, or `job(action="kill")` after a restart) — the row says the job is
/// interactive, so its leader is stopped first there too.
#[tokio::test]
async fn a_persisted_kill_of_an_interactive_job_never_runs_its_next_command() {
    let (store, id) = started_interactive_job().await;
    assert!(matches!(
        kill_persisted(&store.repo, &id, "killed via mcp-ssh kill")
            .await
            .unwrap(),
        KillOutcome::Killed
    ));
    assert_never_ran_after(&store, &id).await;
}

/// K12: a CLI kill of a job this server tracks records its reason; the
/// server's own exit doesn't overwrite it, and polling says the same.
#[tokio::test]
async fn a_cli_kill_s_reason_is_the_final_state_everywhere() {
    let store = store(Duration::from_millis(100));
    let id = bg(&store, "sleep 300").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(matches!(
        kill_persisted(&store.repo, &id, "killed via mcp-ssh kill")
            .await
            .unwrap(),
        KillOutcome::Killed
    ));
    let state = await_state(&store, &id).await;
    for _ in 0..50 {
        let (state, _) = store.poll(&id, None, None).await.unwrap().unwrap();
        if matches!(&state, JobState::Failed { error } if error == "killed via mcp-ssh kill") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("poll never adopted the recorded reason: {state:?}");
}

#[tokio::test]
async fn kill_escalates_to_sigkill_when_term_ignored() {
    let store = store(Duration::from_millis(100));
    let id = bg(&store, "trap '' TERM; while true; do sleep 1; done").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(matches!(
        store.kill(&id).await.unwrap(),
        KillOutcome::Killed
    ));
    assert!(matches!(
        await_state(&store, &id).await,
        JobState::Failed { .. }
    ));
}

#[tokio::test]
async fn kill_unknown_and_finished_say_which() {
    let store = store(Duration::from_secs(5));
    assert!(matches!(
        store.kill(&JobId::from("nope")).await.unwrap(),
        KillOutcome::Unknown
    ));
    let RunResult::Inline { id, .. } = store
        .run(
            "echo bye".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("inline");
    };
    assert!(matches!(
        store.kill(&id).await.unwrap(),
        KillOutcome::NotRunning { status } if status == "exited"
    ));
}

/// K4: the leader exited, so the job is finished — but the server it started
/// with `&` is still in its group. Kill reaches it.
#[tokio::test]
async fn kill_of_a_finished_job_reaches_what_it_left_running() {
    let store = store(Duration::from_secs(5));
    let RunResult::Inline { id, state, page } = store
        .run(
            "sleep 300 & echo \"pid:$!\"".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("the leader exits at once");
    };
    assert!(matches!(state, JobState::Exited { code: 0 }));
    let pid = page.lines[0].strip_prefix("pid:").unwrap().to_string();
    assert!(alive(&pid));
    assert!(matches!(
        store.kill(&id).await.unwrap(),
        KillOutcome::KilledLeftovers
    ));
    assert!(gone_soon(&pid).await);
}

/// A job whose process group outlived the process that started it is tracked
/// only by its DB row — `kill` must reach it through the persisted pgid.
#[tokio::test]
async fn kill_reaches_a_job_that_outlived_a_restart() {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = Db::memory();
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id().expect("child pid");
    let waiter = tokio::spawn(async move { child.wait().await });
    let started = crate::db::now_unix() - 3600;
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix, pgid) \
             VALUES ('survivor', 'running', ?1, ?2)",
            rusqlite::params![started, i64::from(pid)],
        )
    })
    .await
    .unwrap();

    let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
    assert!(matches!(
        store.kill(&JobId::from("survivor")).await.unwrap(),
        KillOutcome::Killed
    ));
    let status = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the signalled group must exit")
        .unwrap()
        .unwrap();
    assert!(!status.success(), "the sleeper died by signal");
    assert_eq!(row_status(&db, "survivor").await.as_deref(), Some("failed"));
}

/// A refused kill (no pgid, or a corrupt one) records nothing. `0` is the
/// dangerous value — it would signal this test runner's own group — so this
/// test is its own canary.
#[tokio::test]
async fn kill_of_an_unsignalable_persisted_job_changes_nothing() {
    let store = store(Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
    for (id, pgid) in [
        ("legacy", None),
        ("zero", Some(0_i64)),
        ("negative", Some(-1)),
    ] {
        store
            .repo
            .db()
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs (id, status, started_unix, pgid) VALUES (?1, 'running', 1, ?2)",
                    rusqlite::params![id, pgid],
                )
            })
            .await
            .unwrap();
        let outcome = store.kill(&JobId::from(id)).await.unwrap();
        assert!(
            matches!(
                outcome,
                KillOutcome::NoProcessGroup | KillOutcome::CorruptProcessGroup { .. }
            ),
            "{id}: {outcome:?}"
        );
        assert_eq!(
            row_status(store.repo.db(), id).await.as_deref(),
            Some("running")
        );
    }
}

/// B2: a job writing past the live log cap is killed and says why; the log
/// stops near the cap instead of filling the disk. The writer is bounded.
#[tokio::test]
async fn a_job_past_the_log_cap_is_killed_and_says_why() {
    let mut store = store(Duration::from_millis(100));
    store.log_cap = 64 * 1024;
    let id = bg(
        &store,
        "i=0; while [ $i -lt 400 ]; do head -c 4096 /dev/zero | tr '\\0' x; echo; i=$((i+1)); sleep 0.01; done",
    )
    .await;
    let state = await_state(&store, &id).await;
    assert!(
        matches!(&state, JobState::Failed { error } if error == LOG_CAP_REASON),
        "{state:?}"
    );
    let size = std::fs::metadata(store.dir.join(format!("{id}.log")))
        .unwrap()
        .len();
    assert!(size < 400 * 4097, "stopped early: {size}");
    assert!(size <= store.log_cap + 256 * 1024, "near the cap: {size}");
}

/// #69: the cap outlives the leader. What a finished job left running (`server
/// &`) still appends to its log, so past the cap that group is killed too and the
/// job says why. The growth is sparse — no real writer.
#[tokio::test]
async fn leftovers_past_the_log_cap_are_killed_and_the_job_says_why() {
    let mut store = store(Duration::from_secs(5));
    store.log_cap = 64 * 1024;
    let RunResult::Inline { id, state, page } = store
        .run(
            "sleep 300 & echo \"pid:$!\"".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("the leader exits at once");
    };
    assert!(matches!(state, JobState::Exited { code: 0 }));
    let pid = page.lines[0].strip_prefix("pid:").unwrap().to_string();
    assert!(alive(&pid));

    std::fs::OpenOptions::new()
        .write(true)
        .open(store.dir.join(format!("{id}.log")))
        .unwrap()
        .set_len(store.log_cap + 1)
        .unwrap();
    assert!(gone_soon(&pid).await, "the leftover outlived the cap");
    let expected = |state: &JobState| matches!(state, JobState::Failed { error } if error == LEFTOVERS_LOG_CAP_REASON);
    for _ in 0..100 {
        let (state, _) = store.poll(&id, None, None).await.unwrap().unwrap();
        if expected(&state) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the job never said why its leftovers died");
}

/// K6: a request cancelled after the spawn — here while the row insert waits
/// on a busy database — still leaves a job that finishes and is recorded.
#[tokio::test]
async fn a_cancelled_launch_still_finishes_its_job() {
    let store = store(Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
    let db = store.repo.db().clone();
    let blocker = tokio::spawn(async move {
        db.call(|_| {
            std::thread::sleep(Duration::from_millis(500));
            Ok(())
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let cancelled = tokio::time::timeout(
        Duration::from_millis(100),
        store.run(
            "echo survived".into(),
            None,
            None,
            false,
            false,
            JobMeta::titled("cancel".into()),
        ),
    )
    .await;
    assert!(cancelled.is_err(), "the launch was cancelled mid-flight");
    blocker.await.unwrap().unwrap();

    let mut found = None;
    for _ in 0..100 {
        if let Some(job) = store.list(0, None).await.unwrap().jobs.into_iter().next() {
            found = Some(job.id);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let id = found.expect("the cancelled launch is still a tracked job");
    let (status, code, _, _) = await_row(store.repo.db(), &id).await;
    assert_eq!((status.as_str(), code), ("exited", Some(0)));
    assert_eq!(
        await_tail(store.repo.db(), &id).await.as_deref(),
        Some("survived\n")
    );
}

/// K7: a job whose row can't be written is refused — stopped, untracked, its
/// log removed — instead of running invisibly.
#[tokio::test]
async fn a_job_whose_row_cannot_be_written_is_refused() {
    let store = store(Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
    store
        .repo
        .db()
        .call(|c| c.execute_batch("DROP TABLE jobs"))
        .await
        .unwrap();
    let result = store
        .run(
            "sleep 300".into(),
            None,
            None,
            true,
            false,
            JobMeta::default(),
        )
        .await;
    assert!(matches!(result, Err(JobError::Store(_))));
    assert!(store.jobs.lock().await.is_empty(), "not tracked");
    assert_eq!(
        std::fs::read_dir(&store.dir).unwrap().count(),
        0,
        "log removed"
    );
}

/// K13: the reaper never kills or evicts a running job, however old.
#[tokio::test]
async fn the_reaper_never_reaps_a_running_job() {
    let store = store(Duration::from_millis(100));
    let id = bg(&store, "sleep 300 & echo \"pid:$!\"; wait").await;
    let child = logged_pid(&store, &id).await;

    // Negative retention: every row counts as aged out this pass.
    reaper::reap_once(&store.jobs, &store.repo, &store.dir, -1).await;

    assert!(alive(&child), "a running job is never killed for age");
    assert!(store.poll(&id, None, None).await.unwrap().is_some());
    assert!(matches!(
        store.kill(&id).await.unwrap(),
        KillOutcome::Killed
    ));
}

/// B6: newest first by start time, paged, with a cursor to the next page.
#[tokio::test]
async fn list_pages_newest_first() {
    let store = store(Duration::from_secs(5));
    for title in ["a", "b", "c"] {
        store
            .run(
                "true".into(),
                None,
                None,
                false,
                false,
                JobMeta::titled(title.into()),
            )
            .await
            .unwrap();
        // Distinct start seconds order the rows.
        tokio::time::sleep(Duration::from_millis(1100)).await;
    }
    let first = store.list(0, Some(2)).await.unwrap();
    let labels: Vec<&str> = first.jobs.iter().map(|j| &j.id.as_ref()[..1]).collect();
    assert_eq!(labels, ["c", "b"]);
    assert_eq!(first.next_cursor, Some(2));
    let rest = store.list(2, Some(2)).await.unwrap();
    assert_eq!(rest.jobs.len(), 1);
    assert_eq!(rest.next_cursor, None);
}

#[tokio::test]
async fn list_and_poll_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = Db::memory();
    let store =
        JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
    let RunResult::Inline { id, .. } = store
        .run(
            "echo tail-line; exit 7".into(),
            None,
            None,
            false,
            false,
            JobMeta::default(),
        )
        .await
        .unwrap()
    else {
        panic!("inline");
    };
    let (status, code, _, _) = await_row(&db, &id).await;
    assert_eq!((status.as_str(), code), ("exited", Some(7)));
    assert_eq!(await_tail(&db, &id).await.as_deref(), Some("tail-line\n"));

    // A fresh store on the same DB + dir, and the log gone: the saved tail.
    let store2 = JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db).unwrap();
    assert!(
        store2
            .list(0, None)
            .await
            .unwrap()
            .jobs
            .iter()
            .any(|j| j.id == id)
    );
    tokio::fs::remove_file(dir.join(format!("{id}.log")))
        .await
        .unwrap();
    let (state, page) = store2.poll(&id, None, None).await.unwrap().unwrap();
    assert!(matches!(state, JobState::Exited { code: 7 }));
    assert_eq!(page.lines, ["tail-line"]);
}

#[tokio::test]
async fn startup_reconcile_marks_dead_running_rows_failed_and_keeps_survivors() {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = Db::memory();
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg("sleep 300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pgid = i64::from(child.id().expect("child pid"));
    let now = crate::db::now_unix();
    db.call(move |conn| {
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('survivor', 'running', ?1, ?2)",
            rusqlite::params![now - 3600, pgid],
        )?;
        // Same second as this boot (a crash loop): just as dead.
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix) VALUES ('same-sec', 'running', ?1)",
            [now],
        )?;
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('ghost', 'running', ?1, 2000000000)",
            [now - 3600],
        )?;
        // A live pgid recorded under another boot names nothing of ours.
        conn.execute(
            "INSERT INTO jobs (id, status, started_unix, pgid, boot_id) \
             VALUES ('rebooted', 'running', ?1, ?2, 'another-boot')",
            rusqlite::params![now - 3600, pgid],
        )
    })
    .await
    .unwrap();

    let store = JobStore::new(dir, Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
    for dead in ["ghost", "same-sec"] {
        let (status, _, error, _) = await_row(&db, &JobId::from(dead)).await;
        assert_eq!(
            (status.as_str(), error.as_deref()),
            ("failed", Some("server restarted"))
        );
    }
    if signal::boot_id().is_some() {
        assert_eq!(await_row(&db, &JobId::from("rebooted")).await.0, "failed");
    }
    assert_eq!(
        row_status(&db, "survivor").await.as_deref(),
        Some("running")
    );
    let _ = child.kill().await;
    drop(store);
}

#[tokio::test]
async fn restart_never_reuses_a_retained_job_id() {
    let dir = tempfile::tempdir().unwrap().keep();
    let db = Db::memory();
    let store =
        JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db.clone()).unwrap();
    let RunResult::Inline { id: first, .. } = store
        .run(
            "echo first".into(),
            None,
            None,
            false,
            false,
            JobMeta::titled("t".into()),
        )
        .await
        .unwrap()
    else {
        panic!("inline");
    };
    await_row(&db, &first).await;
    let store2 = JobStore::new(dir.clone(), Duration::from_secs(5), Shell::sh(), db).unwrap();
    let second = match store2
        .run(
            "echo second".into(),
            None,
            None,
            false,
            false,
            JobMeta::titled("t".into()),
        )
        .await
        .unwrap()
    {
        RunResult::Inline { id, .. } | RunResult::Backgrounded { id } => id,
    };
    assert_ne!(first, second, "retained id must never be reused");
    let old_log = tokio::fs::read_to_string(dir.join(format!("{first}.log")))
        .await
        .unwrap();
    assert!(old_log.contains("first"));
}

#[tokio::test]
async fn poll_surfaces_read_error_while_job_is_tracked() {
    let store = store(Duration::from_secs(5));
    let job = Arc::new(Job {
        log_path: store.dir.join("gone.log"), // never created
        pgid: None,
        interactive: false,
        state: Arc::new(Mutex::new(JobState::Running)),
        kill_reason: Mutex::new(None),
    });
    let id = JobId::from("gone");
    store.jobs.lock().await.insert(id.clone(), job);
    assert!(matches!(
        store.poll(&id, None, None).await,
        Err(JobError::Log(_))
    ));
}

/// D4: a store failure is an error, not "no such job" / "no jobs".
#[tokio::test]
async fn store_failures_surface_as_errors() {
    let store = store(Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(100)).await; // let the first reaper pass run
    store
        .repo
        .db()
        .call(|c| c.execute_batch("DROP TABLE jobs"))
        .await
        .unwrap();
    assert!(matches!(store.list(0, None).await, Err(JobError::Store(_))));
    assert!(matches!(
        store.poll(&JobId::from("x"), None, None).await,
        Err(JobError::Store(_))
    ));
    assert!(matches!(
        store.kill(&JobId::from("x")).await,
        Err(JobError::Store(_))
    ));
}

/// Every job records the agent that started it: `owner` reads it back (live or
/// from the row) and `list` carries it per job; a job with no known caller has
/// no owner.
#[tokio::test]
async fn a_job_records_the_agent_that_started_it() {
    let store = store(Duration::from_secs(5));
    let alpha = crate::agent::Agent::new("client-alpha".into(), Some("alpha"));
    let r = store
        .run(
            "true".into(),
            None,
            None,
            false,
            false,
            JobMeta {
                title: Some("owned".into()),
                owner: Some(alpha.clone()),
            },
        )
        .await
        .unwrap();
    let RunResult::Inline { id: owned, .. } = r else {
        panic!("`true` finishes inline");
    };
    let anonymous = bg(&store, "true").await;

    assert_eq!(store.owner(&owned).await.unwrap(), Some(alpha.clone()));
    assert_eq!(store.owner(&anonymous).await.unwrap(), None);
    assert_eq!(
        store.owner(&JobId::from("no-such-job")).await.unwrap(),
        None
    );

    let page = store.list(0, None).await.unwrap();
    let owner_of = |id: &JobId| {
        page.jobs
            .iter()
            .find(|job| &job.id == id)
            .map(|job| job.owner.clone())
            .expect("listed")
    };
    assert_eq!(owner_of(&owned), Some(alpha));
    assert_eq!(owner_of(&anonymous), None);
}
