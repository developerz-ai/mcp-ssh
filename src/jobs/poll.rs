//! Polling a job: status + one newest-first page of its output, from the live
//! log while it exists, else from the tail saved in its row.
use super::log::{DEFAULT_PAGE, TailBudget, read_tail};
use super::{JobError, JobId, JobState, JobStore, TailPage, store};

impl JobStore {
    /// Status + one newest-first page of a job's log ending at byte `cursor`
    /// (`None` = the latest output). `Ok(None)` means the id is unknown.
    pub async fn poll(
        &self,
        id: &JobId,
        cursor: Option<u64>,
        limit: Option<usize>,
    ) -> Result<Option<(JobState, TailPage)>, JobError> {
        let budget = TailBudget::page(limit.unwrap_or(DEFAULT_PAGE));
        // Bind the lookup to a local so the `jobs` guard is released at this `;`.
        let tracked = self.jobs.lock().await.get(id).cloned();
        if let Some(job) = tracked {
            let state = job.state.lock().await.clone();
            let page = match read_tail(&job.log_path, cursor, budget).await {
                Ok(page) => page,
                // A reaper pass can evict this id and delete its log between the
                // lookup and this read: once the id is gone that's "unknown job",
                // not a read fault.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if self.jobs.lock().await.contains_key(id) {
                        return Err(JobError::Log(error));
                    }
                    return Ok(None);
                }
                Err(error) => return Err(JobError::Log(error)),
            };
            // Once finished, the row is the record: another writer (`mcp-ssh job
            // kill`) may have decided the final state. Fall back to memory only if
            // the row can't be read.
            let state = match state {
                JobState::Running => state,
                finished => match self.repo.find(id).await {
                    Ok(Some(row)) => row.state,
                    _ => finished,
                },
            };
            return Ok(Some((state, page)));
        }
        // Not live here — fall back to the durable row so finished jobs (and jobs
        // from a previous process) stay pollable.
        let Some(row) = self.repo.find(id).await? else {
            return Ok(None);
        };
        let log_path = self.dir.join(format!("{id}.log"));
        let page = match read_tail(&log_path, cursor, budget).await {
            Ok(page) => page,
            // Trimmed away or reaped: serve the tail saved in the row.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                store::page_from_tail(&row.tail, cursor, budget.lines)
            }
            Err(error) => return Err(JobError::Log(error)),
        };
        Ok(Some((row.state, page)))
    }
}
