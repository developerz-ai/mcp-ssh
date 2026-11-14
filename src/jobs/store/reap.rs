//! The reaper's and startup reconcile's queries against `jobs`: which rows are
//! left `running`, what to compact or evict, and dropping aged-out rows.
use std::collections::HashSet;

use super::{JobRepo, ReapRow, UntrackedJob};
use crate::jobs::{JobId, JobStatus};

impl JobRepo {
    /// Rows still `running` that this process doesn't track — every `running` row
    /// minus the `live` ids.
    pub(in crate::jobs) async fn untracked_running(
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
    pub(in crate::jobs) async fn reap_rows(&self) -> rusqlite::Result<Vec<ReapRow>> {
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
    pub(in crate::jobs) async fn all_ids(&self) -> rusqlite::Result<HashSet<String>> {
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
    pub(in crate::jobs) async fn delete(&self, ids: Vec<JobId>) -> rusqlite::Result<()> {
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
}
