//! Persistence boundary for the `jobs` table: every query against it lives here,
//! and the engine (`super`), the reaper, and the admin CLI call typed methods
//! instead of writing SQL. Mirrors `oauth::Store`, which does the same for tokens.
//!
//! The row⇄domain mapping lives in `row`, the kill/reconcile `failed` writes in
//! `fail`, and the reaper's queries in `reap`.
//!
//! A row never carries command text: only the id, the agent-supplied title, the
//! status, and a bounded output tail. A command can hide a secret in its leading
//! tokens (`PGPASSWORD=…`, `mysql -psecret`), and these rows are surfaced by
//! `job(action="list")` and `mcp-ssh jobs` — so `cmd` reaches no query and no log
//! line here (see `super::id` for the same rule on ids).
use std::path::Path;

use rusqlite::OptionalExtension;

use super::log::{self, TailBudget, TailPage};
use super::{JobId, JobState, JobStatus, JobSummary, ProcessGroupId};
use crate::db::{Db, now_unix};

mod fail;
mod reap;
mod row;

pub(super) use fail::signal_death;

pub(crate) use row::JobRow;
pub(super) use row::{KillTarget, PersistedJob, ReapRow, UntrackedJob};
use row::{job_row, state_columns, state_from_columns};

/// The output snapshot saved into a finished job's row: its last 500 lines, and
/// never more than 256 KiB of them (each line already clamped). Bounds the row so
/// the tail survives the live log being trimmed/reaped without bloating SQLite or
/// holding the connection over a huge write.
const TAIL_BUDGET: TailBudget = TailBudget {
    lines: 500,
    bytes: 256 * 1024,
};

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
    /// `interactive` is persisted so that kill escalates as a live one would.
    pub(super) async fn insert_running(
        &self,
        id: &JobId,
        title: Option<String>,
        started_unix: i64,
        pgid: Option<ProcessGroupId>,
        boot_id: Option<String>,
        interactive: bool,
    ) -> rusqlite::Result<()> {
        let row_id = id.as_ref().to_string();
        let pgid = pgid.map(|p| i64::from(p.get()));
        self.db
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs \
                     (id, title, status, started_unix, pgid, boot_id, interactive) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        row_id,
                        title,
                        JobStatus::Running.as_str(),
                        started_unix,
                        pgid,
                        boot_id,
                        interactive
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

/// Page a saved output tail (bounded at write time) the same newest-first way the
/// live log is polled, so a finished job whose log was reaped pages consistently.
pub(super) fn page_from_tail(tail: &str, cursor: Option<u64>, limit: usize) -> TailPage {
    log::tail_of_text(tail, cursor, TailBudget::page(limit))
}

#[cfg(test)]
mod tests;
