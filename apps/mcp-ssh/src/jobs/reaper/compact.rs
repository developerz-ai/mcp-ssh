//! Log compaction: trim each finished job's log to a bounded trailing tail.
use std::collections::HashMap;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use super::{group_may_be_writing, is_running};
use crate::db::now_unix;
use crate::jobs::log::{TailBudget, read_tail};
use crate::jobs::{Job, JobId, JobRepo};

/// This server is meant to run for weeks. Job logs can't grow without bound, so
/// the hourly pass compacts the logs of *finished* jobs to a trailing tail —
/// enough to debug a failure, not enough to fill the disk. Running jobs are never
/// compacted (their log is still being appended). Tiers, by how long ago the job
/// finished: its last `TRIM_RECENT_LINES` while fresh, `TRIM_AGED_LINES` after
/// `TRIM_AGED_AFTER_SECS`, purged at `RETENTION_SECS` — and never more than
/// `TRIM_MAX_BYTES` of either.
pub(super) const TRIM_AGED_AFTER_SECS: i64 = 3 * 3600;
const TRIM_RECENT_LINES: usize = 5_000;
pub(super) const TRIM_AGED_LINES: usize = 500;
pub(super) const TRIM_MAX_BYTES: usize = 4 * 1024 * 1024;
/// First line written into a compacted log. Recognised on the next pass so
/// trimming is idempotent — re-running never erodes the kept tail.
pub(super) const TRIM_MARKER: &str = "[mcp-ssh: earlier output trimmed";

/// Compact every *finished* job's log to a trailing tail: `TRIM_RECENT_LINES`
/// while it finished less than `aged_after_secs` ago, `TRIM_AGED_LINES` after.
/// Running jobs and groups still writing are skipped. Idempotent.
pub(super) async fn compact_once(
    jobs: &Mutex<HashMap<JobId, Arc<Job>>>,
    repo: &JobRepo,
    dir: &Path,
    aged_after_secs: i64,
) {
    let rows = match repo.reap_rows().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "reaper: querying jobs to compact failed");
            return;
        }
    };
    let now = now_unix();
    for row in rows {
        if is_running(jobs, &row).await || group_may_be_writing(&row) {
            continue;
        }
        let keep = if now - row.ended_unix >= aged_after_secs {
            TRIM_AGED_LINES
        } else {
            TRIM_RECENT_LINES
        };
        let path = dir.join(format!("{}.log", row.id));
        match trim_log(&path, keep).await {
            Ok(()) => {}
            // A finished job whose log is already gone is not worth a line per pass.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "failed to trim job log");
            }
        }
    }
}

/// Rewrite `path` to its last `keep` lines (within `TRIM_MAX_BYTES`), prefixed
/// with a `TRIM_MARKER` note. Reads only that bounded tail, never the whole log.
/// Idempotent and shrink-only: a log whose kept tail already reaches back to its
/// start (or to a prior marker) is left untouched, so repeated passes never erode
/// it. The swap is atomic (write an owner-only temp + rename), so a concurrent
/// `poll` reads either the old log or the new one, never a half-written file.
pub(super) async fn trim_log(path: &Path, keep: usize) -> std::io::Result<()> {
    let budget = TailBudget {
        lines: keep,
        bytes: TRIM_MAX_BYTES,
    };
    let page = read_tail(path, None, budget).await?;
    let Some(dropped) = page.older else {
        return Ok(()); // the whole log fits — nothing to drop
    };
    if dropped <= marker_len(path).await? {
        return Ok(()); // everything but an earlier pass's marker fits
    }
    let mut out = format!(
        "{TRIM_MARKER}: dropped {dropped} bytes, keeping the last {} lines]\n",
        page.lines.len()
    );
    for line in &page.lines {
        out.push_str(line);
        out.push('\n');
    }

    let tmp = path.with_extension("log.trim");
    let write = tmp.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&write)?;
        file.write_all(out.as_bytes())
    })
    .await
    .map_err(std::io::Error::other)??;
    tokio::fs::rename(&tmp, path).await
}

/// Byte length of the `TRIM_MARKER` line a previous pass left at the head of
/// `path` (including its newline), or 0 when there is none.
async fn marker_len(path: &Path) -> std::io::Result<u64> {
    use tokio::io::AsyncReadExt;
    let mut head = vec![0u8; 256];
    let mut file = tokio::fs::File::open(path).await?;
    let mut filled = 0;
    while filled < head.len() {
        let n = file.read(&mut head[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    let head = &head[..filled];
    if !head.starts_with(TRIM_MARKER.as_bytes()) {
        return Ok(0);
    }
    Ok(head
        .iter()
        .position(|&b| b == b'\n')
        .map_or(0, |nl| nl as u64 + 1))
}
