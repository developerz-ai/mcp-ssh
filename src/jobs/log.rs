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

/// Read lines `[cursor, cursor+limit)` from a log file. Re-reads the whole file
/// each call — fine for typical logs; seek by byte offset if they get huge.
pub async fn read_page(path: &std::path::Path, cursor: usize, limit: usize) -> Page {
    // Binary-safe: a command that writes non-UTF-8 bytes (e.g. compiled output,
    // escape sequences) must not produce a silently-empty log page.
    let bytes = tokio::fs::read(path).await.unwrap_or_default();
    let content = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = content.lines().collect();
    let total = all.len();
    let end = (cursor + limit).min(total);
    let lines = all
        .get(cursor..end)
        .unwrap_or(&[])
        .iter()
        .map(|s| s.to_string())
        .collect();
    Page {
        lines,
        next_cursor: end,
        total_lines: total,
        has_more: end < total,
    }
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
        let page = read_page(&log, 0, 100).await;
        // Must return lines, not an empty page.
        assert!(
            !page.lines.is_empty(),
            "binary log must not produce empty page"
        );
        assert!(page.lines[0].contains("line1"));
    }
}
