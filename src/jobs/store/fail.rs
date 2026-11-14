//! Ending a job this writer may not own — a kill from the engine or the CLI, or
//! the startup reconcile — by recording `failed` only while the row still reads
//! `running`.
use rusqlite::OptionalExtension;

use super::{JobRepo, KillTarget};
use crate::db::now_unix;
use crate::jobs::{JobId, JobStatus};

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
pub(in crate::jobs) fn signal_death(signal: i32) -> String {
    format!("killed by signal {signal}")
}

impl JobRepo {
    /// The status + process group a kill acts on. `Ok(None)` for an unknown id.
    pub(in crate::jobs) async fn kill_target(
        &self,
        id: &JobId,
    ) -> rusqlite::Result<Option<KillTarget>> {
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
    pub(in crate::jobs) async fn mark_killed(
        &self,
        id: &JobId,
        reason: &'static str,
    ) -> rusqlite::Result<()> {
        self.fail_running(vec![id.clone()], reason).await
    }

    /// Flip rows left `running` by a previous process — their groups are gone — to
    /// `failed`. Only the ids the caller probed as dead are named.
    pub(in crate::jobs) async fn mark_restart_failed(
        &self,
        ids: Vec<JobId>,
    ) -> rusqlite::Result<()> {
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
}
