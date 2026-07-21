//! Persistence boundary for the `jobs` table: every query against it lives here,
//! and the engine (`super`), the reaper, and the admin CLI call typed methods
//! instead of writing SQL. Mirrors `oauth::Store`, which does the same for tokens.
//!
//! This module also owns the row⇄domain mapping — `JobState` onto the
//! `(status, code, error)` columns and back — so a status word is spelled (via
//! `JobStatus`) and read in exactly one place.
//!
//! A row never carries command text: only the id, the agent-supplied title, the
//! status, and a bounded output tail. A command can hide a secret in its leading
//! tokens (`PGPASSWORD=…`, `mysql -psecret`), and these rows are surfaced by
//! `job(action="list")` and `mcp-ssh jobs` — so `cmd` reaches no query and no log
//! line here (see `super::id` for the same rule on ids).
use std::collections::HashSet;
use std::path::Path;

use rusqlite::OptionalExtension;

use super::log::{self, Page};
use super::{JobId, JobState, JobStatus, JobSummary, ProcessGroupId};
use crate::db::Db;

/// Lines of a finished job's output snapshotted into its row. Bounds the row so
/// the tail survives the live log being trimmed/reaped without bloating SQLite.
const TAIL_LINES: usize = 500;

/// "Record a terminal `failed` state, but only while the row still reads
/// `running`." Every path that ends a job it may not own — kill from the engine,
/// kill from the CLI, the startup reconcile — writes through this, so a real exit
/// recorded meanwhile by another writer always wins.
const FAIL_IF_RUNNING: &str = "UPDATE jobs SET status = ?1, error = ?2 \
                               WHERE id = ?3 AND status = ?4";

/// A job's durable row, mapped back onto the domain types. `tail` is the bounded
/// output snapshot saved when the job finished — empty when it has none yet.
#[derive(Debug)]
pub(super) struct PersistedJob {
    pub(super) state: JobState,
    pub(super) tail: String,
}

/// A row left `running` by a previous process. `pgid` is `None` when the row holds
/// nothing signalable (no pgid persisted, or a corrupt one), which the caller
/// reads as "this group is gone".
#[derive(Debug)]
pub(super) struct StaleJob {
    pub(super) id: JobId,
    pub(super) pgid: Option<ProcessGroupId>,
}

/// What a kill decides on: the row's status word (rendered back to the operator by
/// the CLI, so kept verbatim) and its persisted process group, unvalidated —
/// `super::signal` gates it through `ProcessGroupId::from_persisted`.
#[derive(Debug)]
pub(super) struct KillTarget {
    pub(super) status: String,
    pub(super) pgid: Option<i64>,
}

/// One row the reaper weighs for log compaction.
#[derive(Debug)]
pub(super) struct CompactionRow {
    pub(super) id: JobId,
    /// What the row itself says about liveness. Authoritative only for a job this
    /// process doesn't track — otherwise the in-memory state is.
    pub(super) running: bool,
    pub(super) started_unix: i64,
    pub(super) pgid: Option<i64>,
}

/// A row as `mcp-ssh jobs` renders it: the operator's view, so `status` stays the
/// raw word (a corrupt one is shown, not hidden) and the title is included.
#[derive(Debug)]
pub(crate) struct JobRow {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) code: Option<i64>,
    pub(crate) error: Option<String>,
    pub(crate) started_unix: i64,
    pub(crate) title: Option<String>,
}

/// Typed access to the `jobs` table. Cheap to clone — `Db` is an `Arc` handle.
#[derive(Clone)]
pub(crate) struct JobRepo {
    db: Db,
}

impl JobRepo {
    pub(crate) fn new(db: Db) -> Self {
        Self { db }
    }

    /// The shared handle, for the reaper's one non-`jobs` concern: it sweeps
    /// expired OAuth tokens on the same hourly pass. No `jobs` SQL is written
    /// through it — that all lives in this module.
    pub(super) fn db(&self) -> &Db {
        &self.db
    }

    /// Record a newly launched job as `running`. `title` is the agent-supplied
    /// label (never command text); `pgid` is persisted so `mcp-ssh job kill` can
    /// signal the group even after the launching process is gone.
    pub(super) async fn insert_running(
        &self,
        id: &JobId,
        title: Option<String>,
        started_unix: i64,
        pgid: Option<ProcessGroupId>,
    ) -> rusqlite::Result<()> {
        let row_id = id.as_ref().to_string();
        let pgid = pgid.map(|p| i64::from(p.get()));
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs (id, title, status, code, error, started_unix, output_tail, pgid) \
                     VALUES (?1, ?2, ?3, NULL, NULL, ?4, NULL, ?5)",
                    rusqlite::params![
                        row_id,
                        title,
                        JobStatus::Running.as_str(),
                        started_unix,
                        pgid
                    ],
                )
            })
            .await?;
        Ok(())
    }

    /// Record a finished job's final state and a bounded output tail, so
    /// `list`/`poll` reflect it across a restart and the tail outlives the live log
    /// being trimmed or reaped. Best effort: failures are logged, never propagated
    /// — the live log file remains the source of truth while it exists.
    pub(super) async fn finish(&self, id: &JobId, log_path: &Path, state: &JobState) {
        let (status, code, error) = state_columns(state);
        // A tail we can't read just stays empty; the row still records the status.
        let tail = log::tail(log_path, TAIL_LINES).await.unwrap_or_default();
        let row_id = id.as_ref().to_string();
        if let Err(e) = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE jobs SET status = ?1, code = ?2, error = ?3, output_tail = ?4 \
                     WHERE id = ?5",
                    rusqlite::params![status, code, error, tail, row_id],
                )
            })
            .await
        {
            tracing::warn!(error = %e, id = %id, "failed to persist final job state");
        }
    }

    /// A job's durable state + saved tail. `Ok(None)` when there is no such row.
    pub(super) async fn find(&self, id: &JobId) -> rusqlite::Result<Option<PersistedJob>> {
        let row_id = id.as_ref().to_string();
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT status, code, error, output_tail FROM jobs WHERE id = ?1",
                    [row_id],
                    |r| {
                        Ok(PersistedJob {
                            state: state_from_columns(
                                &r.get::<_, String>(0)?,
                                r.get::<_, Option<i64>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                            ),
                            tail: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        })
                    },
                )
                .optional()
            })
            .await
    }

    /// Every job's id + state, sorted by id — the durable history behind
    /// `job(action="list")`, so finished jobs and jobs from a previous process
    /// stay listable.
    pub(super) async fn summaries(&self) -> rusqlite::Result<Vec<JobSummary>> {
        self.db
            .call(|conn| {
                let mut stmt =
                    conn.prepare("SELECT id, status, code, error FROM jobs ORDER BY id")?;
                let rows = stmt.query_map([], |r| {
                    Ok(JobSummary {
                        id: JobId::from(r.get::<_, String>(0)?),
                        state: state_from_columns(
                            &r.get::<_, String>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ),
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
    }

    /// The status + process group a kill acts on. `Ok(None)` for an unknown id.
    pub(super) async fn kill_target(&self, id: &JobId) -> rusqlite::Result<Option<KillTarget>> {
        let row_id = id.as_ref().to_string();
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT status, pgid FROM jobs WHERE id = ?1",
                    [row_id],
                    |r| {
                        Ok(KillTarget {
                            status: r.get(0)?,
                            pgid: r.get(1)?,
                        })
                    },
                )
                .optional()
            })
            .await
    }

    /// Record a kill against a row that still reads `running`. `reason` lands in
    /// the `error` column and is a fixed message from the caller — never command
    /// text.
    pub(super) async fn mark_killed(
        &self,
        id: &JobId,
        reason: &'static str,
    ) -> rusqlite::Result<()> {
        self.fail_running(vec![id.clone()], reason).await
    }

    /// Flip rows a previous process left `running` — their processes died with it —
    /// to `failed`. Only the ids the caller probed as dead are named.
    pub(super) async fn mark_restart_failed(&self, ids: Vec<JobId>) -> rusqlite::Result<()> {
        self.fail_running(ids, "server restarted").await
    }

    /// Flip rows that still read `running` to `failed` with a fixed `reason`. One
    /// statement per id — a long backlog would otherwise run into SQLite's
    /// bound-parameter cap — and each names its id explicitly, so a job started
    /// meanwhile is never caught.
    async fn fail_running(&self, ids: Vec<JobId>, reason: &'static str) -> rusqlite::Result<()> {
        self.db
            .call(move |conn| {
                for id in &ids {
                    conn.execute(
                        FAIL_IF_RUNNING,
                        rusqlite::params![
                            JobStatus::Failed.as_str(),
                            reason,
                            id.as_ref(),
                            JobStatus::Running.as_str()
                        ],
                    )?;
                }
                Ok(())
            })
            .await
    }

    /// Candidates for the startup reconcile: rows still `running` that started at
    /// or before `boot`, minus the ids `live` in this process. `<=` rather than
    /// `<` because a crash loop can restart within the same wall-clock second.
    pub(super) async fn stale_running(
        &self,
        boot: i64,
        live: Vec<String>,
    ) -> rusqlite::Result<Vec<StaleJob>> {
        self.db
            .call(move |conn| {
                // `NOT IN ()` is a syntax error, and at boot the map is always
                // empty — only add the clause when there are ids.
                let exclude = if live.is_empty() {
                    String::new()
                } else {
                    let placeholders = vec!["?"; live.len()].join(",");
                    format!(" AND id NOT IN ({placeholders})")
                };
                let sql = format!(
                    "SELECT id, pgid FROM jobs WHERE status = ? AND started_unix <= ?{exclude}"
                );
                let params = [
                    rusqlite::types::Value::from(JobStatus::Running.as_str().to_string()),
                    rusqlite::types::Value::from(boot),
                ]
                .into_iter()
                .chain(live.into_iter().map(rusqlite::types::Value::from));
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
                    Ok(StaleJob {
                        id: JobId::from(r.get::<_, String>(0)?),
                        pgid: r
                            .get::<_, Option<i64>>(1)?
                            .and_then(ProcessGroupId::from_persisted),
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
    }

    /// Every row the reaper weighs for log compaction.
    pub(super) async fn compaction_rows(&self) -> rusqlite::Result<Vec<CompactionRow>> {
        self.db
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT id, status, started_unix, pgid FROM jobs")?;
                let rows = stmt.query_map([], |r| {
                    Ok(CompactionRow {
                        id: JobId::from(r.get::<_, String>(0)?),
                        running: r.get::<_, String>(1)? == JobStatus::Running.as_str(),
                        started_unix: r.get(2)?,
                        pgid: r.get(3)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
    }

    /// Ids of every known job, for spotting log files no row owns.
    pub(super) async fn all_ids(&self) -> rusqlite::Result<HashSet<String>> {
        self.db
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT id FROM jobs")?;
                let ids = stmt.query_map([], |r| r.get::<_, String>(0))?;
                ids.collect::<rusqlite::Result<HashSet<_>>>()
            })
            .await
    }

    /// Ids of jobs that started before `cutoff` — the reaper's eviction candidates.
    pub(super) async fn started_before(&self, cutoff: i64) -> rusqlite::Result<Vec<JobId>> {
        self.db
            .call(move |conn| {
                let mut stmt = conn.prepare("SELECT id FROM jobs WHERE started_unix < ?1")?;
                let ids = stmt.query_map([cutoff], |r| Ok(JobId::from(r.get::<_, String>(0)?)))?;
                ids.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
    }

    /// Drop rows outright (the reaper, once a job has aged out). One statement per
    /// id, for the same bound-parameter reason as `fail_running`.
    pub(super) async fn delete(&self, ids: Vec<JobId>) -> rusqlite::Result<()> {
        self.db
            .call(move |conn| {
                for id in &ids {
                    conn.execute("DELETE FROM jobs WHERE id = ?1", [id.as_ref()])?;
                }
                Ok(())
            })
            .await
    }

    /// Rows behind `mcp-ssh jobs`: active jobs by default, or every job newest
    /// first with `all` (capped, so a long-lived box's history can't flood the
    /// terminal).
    pub(crate) async fn listing(&self, all: bool) -> rusqlite::Result<Vec<JobRow>> {
        self.db
            .call(move |conn| {
                let sql = if all {
                    "SELECT id, status, code, error, started_unix, title \
                     FROM jobs ORDER BY started_unix DESC LIMIT 200"
                } else {
                    "SELECT id, status, code, error, started_unix, title \
                     FROM jobs WHERE status = ?1 ORDER BY started_unix DESC"
                };
                let mut stmt = conn.prepare(sql)?;
                if all {
                    stmt.query_map([], job_row)?.collect()
                } else {
                    stmt.query_map([JobStatus::Running.as_str()], job_row)?
                        .collect()
                }
            })
            .await
    }
}

/// Map a `JobState` onto the DB row's `(status, code, error)` columns.
fn state_columns(state: &JobState) -> (&'static str, Option<i32>, Option<String>) {
    match state {
        JobState::Running => (JobStatus::Running.as_str(), None, None),
        JobState::Exited { code } => (JobStatus::Exited.as_str(), Some(*code), None),
        JobState::Failed { error } => (JobStatus::Failed.as_str(), None, Some(error.clone())),
    }
}

/// Rebuild a `JobState` from a DB row's columns. An unrecognized status (a corrupt
/// row) reads as `Failed` so the anomaly surfaces rather than masquerading as a
/// live or cleanly-exited job — `JobStatus::from_str` itself never silently
/// coerces; the coercion to `Failed` happens explicitly, here.
fn state_from_columns(status: &str, code: Option<i64>, error: Option<String>) -> JobState {
    match status.parse::<JobStatus>() {
        Ok(JobStatus::Running) => JobState::Running,
        Ok(JobStatus::Exited) => JobState::Exited {
            code: code.map(|c| c as i32).unwrap_or(-1),
        },
        Ok(JobStatus::Failed) | Err(_) => JobState::Failed {
            error: error.unwrap_or_else(|| status.to_string()),
        },
    }
}

/// One `mcp-ssh jobs` row. A free function so both `listing` branches share it.
fn job_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        id: r.get(0)?,
        status: r.get(1)?,
        code: r.get(2)?,
        error: r.get(3)?,
        started_unix: r.get(4)?,
        title: r.get(5)?,
    })
}

/// Render a saved output tail (already bounded at write time) the same newest-first
/// way the live log is polled, so a finished job whose log was reaped paginates
/// consistently — cursor 0 is the newest saved lines, paging back through the tail.
pub(super) fn page_from_tail(tail: &str, cursor: usize, limit: usize) -> Page {
    let lines: Vec<&str> = tail.lines().collect();
    log::paginate_tail(&lines, cursor, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::now_unix;

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
        let body: String = (1..=TAIL_LINES + 10)
            .map(|i| format!("line{i}\n"))
            .collect();
        tokio::fs::write(&log, body).await.unwrap();

        let repo = repo();
        let id = JobId::from("job-10-00-01");
        repo.insert_running(&id, None, now_unix(), None)
            .await
            .unwrap();
        repo.finish(&id, &log, &JobState::Exited { code: 7 }).await;

        let row = columns(&repo, id.as_ref()).await;
        assert_eq!(
            (row.status.as_str(), row.code, row.error),
            ("exited", Some(7), None)
        );
        let tail = row.tail.expect("a finished row carries its tail");
        assert_eq!(tail.lines().count(), TAIL_LINES, "the tail is bounded");
        assert!(
            tail.ends_with(&format!("line{}\n", TAIL_LINES + 10)),
            "newest kept"
        );
    }

    #[tokio::test]
    async fn find_and_summaries_map_rows_back_onto_job_state() {
        let repo = repo();
        let (running, failed) = (JobId::from("job-a"), JobId::from("job-b"));
        repo.insert_running(&running, None, now_unix(), None)
            .await
            .unwrap();
        repo.insert_running(&failed, None, now_unix(), None)
            .await
            .unwrap();
        repo.finish(
            &failed,
            std::path::Path::new("/nonexistent.log"),
            &JobState::Failed {
                error: "boom".into(),
            },
        )
        .await;

        let found = repo.find(&failed).await.unwrap().expect("row exists");
        assert!(matches!(&found.state, JobState::Failed { error } if error == "boom"));
        assert_eq!(found.tail, "", "an unreadable log leaves an empty tail");
        assert!(repo.find(&JobId::from("ghost")).await.unwrap().is_none());

        let summaries = repo.summaries().await.unwrap();
        let states: Vec<_> = summaries
            .iter()
            .map(|s| (s.id.as_ref(), &s.state))
            .collect();
        assert_eq!(states.len(), 2, "sorted by id: {states:?}");
        assert_eq!(states[0].0, "job-a");
        assert!(matches!(states[0].1, JobState::Running));
        assert!(matches!(states[1].1, JobState::Failed { .. }));
    }

    #[tokio::test]
    async fn stale_running_skips_live_ids_and_reconciles_the_rest() {
        let repo = repo();
        let boot = now_unix();
        let (ghost, mine) = (JobId::from("job-ghost"), JobId::from("job-mine"));
        // Both rows are `running` at or before boot; only `ghost` is unaccounted
        // for — `mine` is live in this process and must never be touched.
        repo.insert_running(
            &ghost,
            None,
            boot - 3600,
            ProcessGroupId::new(2_000_000_000),
        )
        .await
        .unwrap();
        repo.insert_running(&mine, None, boot, None).await.unwrap();
        // A job that started after boot belongs to this process too.
        repo.insert_running(&JobId::from("job-new"), None, boot + 1, None)
            .await
            .unwrap();

        let stale = repo
            .stale_running(boot, vec![mine.as_ref().to_string()])
            .await
            .unwrap();
        let ids: Vec<&str> = stale.iter().map(|c| c.id.as_ref()).collect();
        assert_eq!(ids, vec!["job-ghost"], "only the previous process's row");
        assert!(stale[0].pgid.is_some(), "its persisted group is probeable");

        repo.mark_restart_failed(vec![ghost.clone()]).await.unwrap();
        let reconciled = columns(&repo, ghost.as_ref()).await;
        assert_eq!(reconciled.status, "failed");
        assert_eq!(reconciled.error.as_deref(), Some("server restarted"));
        assert_eq!(
            columns(&repo, mine.as_ref()).await.status,
            "running",
            "a live job's row is untouched"
        );
    }

    #[tokio::test]
    async fn a_finished_row_keeps_its_own_state_when_a_kill_is_recorded() {
        let repo = repo();
        let id = JobId::from("job-done");
        repo.insert_running(&id, None, now_unix(), None)
            .await
            .unwrap();
        repo.finish(
            &id,
            std::path::Path::new("/nonexistent.log"),
            &JobState::Exited { code: 0 },
        )
        .await;

        // The guard: the job's real exit was recorded first, so the kill must not
        // overwrite it with `failed`.
        repo.mark_killed(&id, "killed").await.unwrap();

        let row = columns(&repo, id.as_ref()).await;
        assert_eq!(
            (row.status.as_str(), row.code, row.error),
            ("exited", Some(0), None)
        );
    }

    #[tokio::test]
    async fn kill_target_reads_status_and_pgid_verbatim() {
        let repo = repo();
        // A corrupt row the typed API can't write: `kill_target` hands the raw
        // values back so the caller (not the repo) refuses to signal them.
        seed_raw(
            &repo,
            "INSERT INTO jobs (id, status, started_unix, pgid) VALUES ('odd', 'weird', 1, 0)",
        )
        .await;

        let target = repo
            .kill_target(&JobId::from("odd"))
            .await
            .unwrap()
            .expect("row exists");
        assert_eq!(target.status, "weird");
        assert_eq!(target.pgid, Some(0));
        assert!(
            repo.kill_target(&JobId::from("ghost"))
                .await
                .unwrap()
                .is_none(),
            "an unknown id is None, not an error"
        );
    }

    #[tokio::test]
    async fn reaper_queries_track_the_table() {
        let repo = repo();
        let (old, fresh) = (JobId::from("job-old"), JobId::from("job-fresh"));
        repo.insert_running(&old, None, 100, ProcessGroupId::new(7))
            .await
            .unwrap();
        repo.insert_running(&fresh, None, 300, None).await.unwrap();
        repo.finish(
            &fresh,
            std::path::Path::new("/nonexistent.log"),
            &JobState::Exited { code: 0 },
        )
        .await;

        assert_eq!(
            repo.all_ids().await.unwrap(),
            HashSet::from(["job-old".to_string(), "job-fresh".to_string()])
        );
        let stale = repo.started_before(200).await.unwrap();
        assert_eq!(
            stale.iter().map(|i| i.as_ref()).collect::<Vec<_>>(),
            ["job-old"]
        );

        let mut rows = repo.compaction_rows().await.unwrap();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(
            rows.iter()
                .map(|r| (r.id.as_ref(), r.running, r.started_unix, r.pgid))
                .collect::<Vec<_>>(),
            [
                ("job-fresh", false, 300, None),
                ("job-old", true, 100, Some(7))
            ]
        );

        repo.delete(stale).await.unwrap();
        assert_eq!(
            repo.all_ids().await.unwrap(),
            HashSet::from(["job-fresh".to_string()])
        );
    }

    #[tokio::test]
    async fn listing_filters_active_unless_all() {
        let repo = repo();
        let (a, b) = (JobId::from("a"), JobId::from("b"));
        repo.insert_running(&a, Some("first".into()), 10, None)
            .await
            .unwrap();
        repo.insert_running(&b, None, 20, None).await.unwrap();
        repo.finish(
            &b,
            std::path::Path::new("/nonexistent.log"),
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
    fn state_columns_round_trips_and_a_corrupt_status_reads_as_failed() {
        // Every state the engine writes must come back unchanged...
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
        // ...and an unknown word surfaces as a failure carrying it, never as a
        // running or cleanly-exited job.
        let corrupt = state_from_columns("weird", None, None);
        assert!(matches!(corrupt, JobState::Failed { error } if error == "weird"));
    }

    #[test]
    fn page_from_tail_serves_the_newest_lines_first() {
        let tail: String = (1..=10).map(|i| format!("line{i}\n")).collect();
        let page = page_from_tail(&tail, 0, 3);
        assert_eq!(page.lines, ["line8", "line9", "line10"], "newest window");
        assert!(page.has_more, "older lines remain");
        assert_eq!(page.total_lines, 10);
        // An absent tail paginates as an empty terminal page, not an error.
        let empty = page_from_tail("", 0, 3);
        assert!(empty.lines.is_empty() && !empty.has_more);
    }
}
