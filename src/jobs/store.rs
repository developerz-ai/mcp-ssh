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

use super::log::{self, TailBudget, TailPage};
use super::{JobId, JobState, JobStatus, JobSummary, ProcessGroupId};
use crate::db::{Db, now_unix};

/// The output snapshot saved into a finished job's row: its last 500 lines, and
/// never more than 256 KiB of them (each line already clamped). Bounds the row so
/// the tail survives the live log being trimmed/reaped without bloating SQLite or
/// holding the connection over a huge write.
const TAIL_BUDGET: TailBudget = TailBudget {
    lines: 500,
    bytes: 256 * 1024,
};

/// "Record a terminal `failed` state, but only while the row still reads
/// `running`." Every path that ends a job it may not own — kill from the engine,
/// kill from the CLI, the reconcile — writes through this, so a real exit recorded
/// meanwhile by another writer always wins.
///
/// One exception: a bare signal death the engine recorded ([`signal_death`]) is
/// overwritten. It says only that a signal arrived — when that signal was this
/// kill's, the race between the dying job's waiter and this write must not
/// decide whether the row says *why*.
const FAIL_IF_RUNNING: &str = "UPDATE jobs SET status = ?1, error = ?2, \
                               finished_unix = COALESCE(finished_unix, ?3) \
                               WHERE id = ?4 AND (status = ?5 \
                               OR (status = ?1 AND error LIKE 'killed by signal %'))";

/// How the engine words a death by signal it didn't cause (see
/// [`FAIL_IF_RUNNING`], which recognises it).
pub(super) fn signal_death(signal: i32) -> String {
    format!("killed by signal {signal}")
}

/// A job's durable row, mapped back onto the domain types. `tail` is the bounded
/// output snapshot saved when the job finished — empty when it has none yet.
#[derive(Debug)]
pub(super) struct PersistedJob {
    pub(super) state: JobState,
    pub(super) tail: String,
}

/// A `running` row this process doesn't track, with what is needed to probe its
/// group: the raw persisted pgid and the boot it was recorded under.
#[derive(Debug)]
pub(super) struct UntrackedJob {
    pub(super) id: JobId,
    pub(super) pgid: Option<i64>,
    pub(super) boot_id: Option<String>,
}

/// What a kill decides on: the row's status word (rendered back to the operator by
/// the CLI, so kept verbatim), its persisted process group (unvalidated —
/// `super::signal` gates it), and the boot it was recorded under.
#[derive(Debug)]
pub(super) struct KillTarget {
    pub(super) status: String,
    pub(super) pgid: Option<i64>,
    pub(super) boot_id: Option<String>,
}

/// One row the reaper weighs for log compaction or eviction.
#[derive(Debug)]
pub(super) struct ReapRow {
    pub(super) id: JobId,
    /// What the row itself says about liveness. Authoritative only for a job this
    /// process doesn't track — otherwise the in-memory state is.
    pub(super) running: bool,
    /// When the job finished (when it started, for a row predating the column).
    pub(super) ended_unix: i64,
    pub(super) pgid: Option<i64>,
    pub(super) boot_id: Option<String>,
}

/// A row as `mcp-ssh jobs` renders it: the operator's view, so `status` stays the
/// raw word and the title is included.
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
    /// expired OAuth state on the same hourly pass. No `jobs` SQL is written
    /// through it — that all lives in this module.
    pub(super) fn db(&self) -> &Db {
        &self.db
    }

    /// Record a newly launched job as `running`. `title` is the normalized label
    /// (never command text); `pgid` + `boot_id` are persisted so `mcp-ssh job kill`
    /// can signal the group after the launching process is gone — and refuse to
    /// once the host has rebooted and the number may name another group.
    pub(super) async fn insert_running(
        &self,
        id: &JobId,
        title: Option<String>,
        started_unix: i64,
        pgid: Option<ProcessGroupId>,
        boot_id: Option<String>,
    ) -> rusqlite::Result<()> {
        let row_id = id.as_ref().to_string();
        let pgid = pgid.map(|p| i64::from(p.get()));
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs (id, title, status, started_unix, pgid, boot_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        row_id,
                        title,
                        JobStatus::Running.as_str(),
                        started_unix,
                        pgid,
                        boot_id
                    ],
                )
            })
            .await?;
        Ok(())
    }

    /// Record a finished job's final state, then a bounded output tail. Two writes,
    /// status first: the tail is the part that can fail (an unreadable log), and it
    /// must never leave the row reading `running`. Only a row still `running` is
    /// finished — a `failed` already written by a kill or the reconcile keeps its
    /// reason. Best effort: failures are logged, never propagated.
    pub(super) async fn finish(&self, id: &JobId, log_path: &Path, state: &JobState) {
        let (status, code, error) = state_columns(state);
        let row_id = id.as_ref().to_string();
        let updated = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE jobs SET status = ?1, code = ?2, error = ?3, finished_unix = ?4 \
                     WHERE id = ?5 AND status = ?6",
                    rusqlite::params![
                        status,
                        code,
                        error,
                        now_unix(),
                        row_id,
                        JobStatus::Running.as_str()
                    ],
                )
            })
            .await;
        match updated {
            Ok(1) => {}
            Ok(_) => {
                tracing::info!(id = %id, "job row already final (or missing) — exit not recorded over it");
            }
            Err(e) => tracing::warn!(error = %e, id = %id, "failed to persist final job state"),
        }

        let tail = match log::tail(log_path, TAIL_BUDGET).await {
            Ok(tail) => tail,
            Err(e) => {
                tracing::warn!(error = %e, id = %id, "failed to read job log tail");
                return;
            }
        };
        let row_id = id.as_ref().to_string();
        if let Err(e) = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE jobs SET output_tail = ?1 WHERE id = ?2",
                    rusqlite::params![tail, row_id],
                )
            })
            .await
        {
            tracing::warn!(error = %e, id = %id, "failed to persist job output tail");
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

    /// One page of job history, newest start first — the rows behind
    /// `job(action="list")`, so finished jobs and jobs from a previous process stay
    /// listable. Returns the page and whether more rows follow it. A row that
    /// can't be mapped is skipped with a warning rather than failing the listing.
    pub(super) async fn summaries(
        &self,
        offset: usize,
        limit: usize,
    ) -> rusqlite::Result<(Vec<JobSummary>, bool)> {
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);
        // One extra row answers "is there another page?" without a COUNT.
        let fetch = i64::try_from(limit).unwrap_or(i64::MAX - 1) + 1;
        let (mut rows, skipped) = self
            .db
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, status, code, error FROM jobs \
                     ORDER BY started_unix DESC, id DESC LIMIT ?1 OFFSET ?2",
                )?;
                let mapped = stmt.query_map([fetch, offset], |r| {
                    Ok(JobSummary {
                        id: JobId::from(r.get::<_, String>(0)?),
                        state: state_from_columns(
                            &r.get::<_, String>(1)?,
                            r.get::<_, Option<i64>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                        ),
                    })
                })?;
                let mut rows = Vec::new();
                let mut skipped = Vec::new();
                for row in mapped {
                    match row {
                        Ok(row) => rows.push(row),
                        Err(error) => skipped.push(error),
                    }
                }
                Ok((rows, skipped))
            })
            .await?;
        for error in skipped {
            tracing::warn!(%error, "skipping a job row that could not be read");
        }
        let more = rows.len() > limit;
        rows.truncate(limit);
        Ok((rows, more))
    }

    /// The status + process group a kill acts on. `Ok(None)` for an unknown id.
    pub(super) async fn kill_target(&self, id: &JobId) -> rusqlite::Result<Option<KillTarget>> {
        let row_id = id.as_ref().to_string();
        self.db
            .call(move |conn| {
                conn.query_row(
                    "SELECT status, pgid, boot_id FROM jobs WHERE id = ?1",
                    [row_id],
                    |r| {
                        Ok(KillTarget {
                            status: r.get(0)?,
                            pgid: r.get(1)?,
                            boot_id: r.get(2)?,
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

    /// Flip rows left `running` by a previous process — their groups are gone — to
    /// `failed`. Only the ids the caller probed as dead are named.
    pub(super) async fn mark_restart_failed(&self, ids: Vec<JobId>) -> rusqlite::Result<()> {
        self.fail_running(ids, "server restarted").await
    }

    /// Flip rows that still read `running` to `failed` with a fixed `reason`, in
    /// one transaction: all or none, and one short hold of the connection instead
    /// of a commit per row. One statement per id — a long backlog would otherwise
    /// run into SQLite's bound-parameter cap — and each names its id explicitly,
    /// so a job started meanwhile is never caught.
    async fn fail_running(&self, ids: Vec<JobId>, reason: &'static str) -> rusqlite::Result<()> {
        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                let now = now_unix();
                for id in &ids {
                    tx.execute(
                        FAIL_IF_RUNNING,
                        rusqlite::params![
                            JobStatus::Failed.as_str(),
                            reason,
                            now,
                            id.as_ref(),
                            JobStatus::Running.as_str()
                        ],
                    )?;
                }
                tx.commit()
            })
            .await
    }

    /// Rows still `running` that this process doesn't track — every `running` row
    /// minus the `live` ids.
    pub(super) async fn untracked_running(
        &self,
        live: Vec<String>,
    ) -> rusqlite::Result<Vec<UntrackedJob>> {
        self.db
            .call(move |conn| {
                // `NOT IN ()` is a syntax error — only add the clause when there are ids.
                let exclude = if live.is_empty() {
                    String::new()
                } else {
                    let placeholders = vec!["?"; live.len()].join(",");
                    format!(" AND id NOT IN ({placeholders})")
                };
                let sql = format!("SELECT id, pgid, boot_id FROM jobs WHERE status = ?{exclude}");
                let params = std::iter::once(rusqlite::types::Value::from(
                    JobStatus::Running.as_str().to_string(),
                ))
                .chain(live.into_iter().map(rusqlite::types::Value::from));
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
                    Ok(UntrackedJob {
                        id: JobId::from(r.get::<_, String>(0)?),
                        pgid: r.get(1)?,
                        boot_id: r.get(2)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await
    }

    /// Every row, with what the reaper decides compaction and eviction on.
    pub(super) async fn reap_rows(&self) -> rusqlite::Result<Vec<ReapRow>> {
        self.db
            .call(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, status, COALESCE(finished_unix, started_unix), pgid, boot_id \
                     FROM jobs",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(ReapRow {
                        id: JobId::from(r.get::<_, String>(0)?),
                        running: r.get::<_, String>(1)? == JobStatus::Running.as_str(),
                        ended_unix: r.get(2)?,
                        pgid: r.get(3)?,
                        boot_id: r.get(4)?,
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

    /// Drop rows outright (the reaper, once a job has aged out), in one
    /// transaction — all or none. One statement per id, for the same
    /// bound-parameter reason as `fail_running`.
    pub(super) async fn delete(&self, ids: Vec<JobId>) -> rusqlite::Result<()> {
        self.db
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;
                for id in &ids {
                    tx.execute("DELETE FROM jobs WHERE id = ?1", [id.as_ref()])?;
                }
                tx.commit()
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

/// Rebuild a `JobState` from a DB row's columns. Anything the engine could never
/// have written — an unknown status word, an exit code outside `i32` — reads as
/// `Failed`, so the anomaly surfaces instead of masquerading as a live or a
/// cleanly-exited job (or wrapping onto a different code).
fn state_from_columns(status: &str, code: Option<i64>, error: Option<String>) -> JobState {
    match status.parse::<JobStatus>() {
        Ok(JobStatus::Running) => JobState::Running,
        Ok(JobStatus::Exited) => match code.map(i32::try_from) {
            Some(Ok(code)) => JobState::Exited { code },
            Some(Err(_)) | None => JobState::Failed {
                error: "corrupt row: exited without a valid exit code".to_string(),
            },
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

/// Page a saved output tail (bounded at write time) the same newest-first way the
/// live log is polled, so a finished job whose log was reaped pages consistently.
pub(super) fn page_from_tail(tail: &str, cursor: Option<u64>, limit: usize) -> TailPage {
    log::tail_of_text(tail, cursor, TailBudget::page(limit))
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
}
