//! Paginated log reading for per-job output files.

/// One page of log lines plus a cursor to fetch the next page.
#[derive(Debug, serde::Serialize)]
pub struct Page {
    pub lines: Vec<String>,
    pub next_cursor: usize,
    pub total_lines: usize,
    pub has_more: bool,
}

pub const DEFAULT_PAGE: usize = 200;

/// A job log could not be read. Kept distinct from a legitimately empty log so
/// `job(action="poll")` can surface "log unavailable" instead of silently
/// returning an empty page (a missing or unreadable log must not look identical
/// to one that simply has no output yet).
#[derive(Debug, thiserror::Error)]
pub enum JobLogError {
    #[error("reading job log: {0}")]
    Read(#[from] std::io::Error),
}

/// Convert back to `io::Error` at the `JobStore::run` boundary, which already
/// reports in `std::io::Result`. The typed error matters for `poll`, which
/// surfaces it to the caller rather than collapsing it into an empty page.
impl From<JobLogError> for std::io::Error {
    fn from(e: JobLogError) -> Self {
        match e {
            JobLogError::Read(io) => io,
        }
    }
}

/// Read lines `[cursor, cursor+limit)` from a log file. Re-reads the whole file
/// each call — fine for typical logs; seek by byte offset if they get huge.
pub async fn read_page(
    path: &std::path::Path,
    cursor: usize,
    limit: usize,
) -> Result<Page, JobLogError> {
    // A failed read (missing/unreadable log) propagates as a typed error. Only
    // non-UTF-8 *content* is rendered lossily: a command that writes raw bytes
    // (compiled output, escape sequences) must not produce a silently-empty page.
    let bytes = tokio::fs::read(path).await?;
    let content = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = content.lines().collect();
    let total = all.len();
    // cursor + limit come from tool input; saturate so an oversized request can't
    // overflow into a bogus window.
    let end = cursor.saturating_add(limit).min(total);
    let lines = all
        .get(cursor..end)
        .unwrap_or(&[])
        .iter()
        .map(|s| s.to_string())
        .collect();
    Ok(Page {
        lines,
        next_cursor: end,
        total_lines: total,
        has_more: end < total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_page_handles_binary_log_without_empty_output() {
        // Simulate a command that writes non-UTF-8 bytes to its log.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("bin.log");
        // Valid UTF-8 prefix + invalid bytes + valid suffix.
        tokio::fs::write(&log, b"line1\nline2\xff\xfeline3\n")
            .await
            .unwrap();
        let page = read_page(&log, 0, 100).await.unwrap();
        // Must return lines, not an empty page.
        assert!(
            !page.lines.is_empty(),
            "binary log must not produce empty page"
        );
        assert!(page.lines[0].contains("line1"));
    }

    #[tokio::test]
    async fn read_page_errors_on_missing_log() {
        // A missing log is an error, not a silently-empty page.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.log");
        assert!(read_page(&missing, 0, 100).await.is_err());
    }
}
