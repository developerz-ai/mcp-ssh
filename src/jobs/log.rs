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

/// Hard ceiling on the bytes a single page returns, regardless of line count.
/// Line-count pagination alone doesn't bound context: one 10 MB minified line, or
/// 200 long lines, would still flood the agent. A page stops accumulating once it
/// would exceed this (but always returns at least one line, so paging makes
/// progress). ~64 KB is generous for reading yet safely under any context window.
pub const MAX_PAGE_BYTES: usize = 64 * 1024;

/// Longest single line returned verbatim. A line over this is truncated with a
/// `…[+N bytes]` marker so one pathological line can't blow the byte budget on its
/// own (and the line still counts as one cursor step, so the agent can move past it).
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// Truncate a single line to `MAX_LINE_BYTES` on a UTF-8 boundary, tagging how many
/// bytes were dropped. Short lines pass through untouched.
fn clamp_line(line: &str) -> String {
    if line.len() <= MAX_LINE_BYTES {
        return line.to_string();
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[+{} bytes]", &line[..end], line.len() - end)
}

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
    // A zero limit would yield an empty page whose `next_cursor == cursor` with
    // `has_more` still true, so a client advancing on `next_cursor` would spin
    // forever on the same window. Clamp to at least one line.
    let limit = limit.max(1);
    // A failed read (missing/unreadable log) propagates as a typed error. Only
    // non-UTF-8 *content* is rendered lossily: a command that writes raw bytes
    // (compiled output, escape sequences) must not produce a silently-empty page.
    let bytes = tokio::fs::read(path).await?;
    let content = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = content.lines().collect();
    Ok(paginate(&all, cursor, limit))
}

/// Slice `[cursor, …)` from already-split lines, bounded by BOTH the line `limit`
/// and `MAX_PAGE_BYTES`, with each line clamped to `MAX_LINE_BYTES`. `next_cursor`
/// reflects exactly how many lines were returned, so byte-capping never desyncs the
/// line cursor — the agent always resumes at the first line it hasn't seen. Shared
/// by job-log polling and `file read` so both honor the same ceilings.
pub fn paginate(all: &[&str], cursor: usize, limit: usize) -> Page {
    // A zero limit would yield an empty, non-advancing page; clamp to one line.
    let limit = limit.max(1);
    let total = all.len();
    let start = cursor.min(total);
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for raw in &all[start..] {
        if lines.len() >= limit {
            break;
        }
        let line = clamp_line(raw);
        // Always take at least one line, then stop before exceeding the budget.
        if !lines.is_empty() && bytes + line.len() + 1 > MAX_PAGE_BYTES {
            break;
        }
        bytes += line.len() + 1; // +1 for the newline the agent sees between lines
        lines.push(line);
    }
    let end = start + lines.len();
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
        let page = read_page(&log, 0, 100).await.unwrap();
        // Must return lines, not an empty page.
        assert!(
            !page.lines.is_empty(),
            "binary log must not produce empty page"
        );
        assert!(page.lines[0].contains("line1"));
    }

    #[tokio::test]
    async fn read_page_clamps_zero_limit_to_avoid_infinite_loop() {
        // limit=0 must not produce an empty, non-advancing page: a client paging
        // on next_cursor would otherwise loop forever on the same window.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("multi.log");
        tokio::fs::write(&log, b"a\nb\nc\n").await.unwrap();
        let page = read_page(&log, 0, 0).await.unwrap();
        assert_eq!(page.lines.len(), 1, "zero limit must clamp to one line");
        assert!(page.next_cursor > 0, "cursor must advance past the start");
        assert!(page.has_more, "more lines remain after the clamped page");
    }

    #[test]
    fn paginate_caps_total_bytes_and_advances_cursor_consistently() {
        // Many lines, each ~1 KB. A page must stop at MAX_PAGE_BYTES, and the
        // returned line count must match next_cursor so paging stays in sync.
        let line = "x".repeat(1024);
        let owned: Vec<String> = (0..1000).map(|_| line.clone()).collect();
        let all: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();

        let page = paginate(&all, 0, DEFAULT_PAGE);
        let returned: usize = page.lines.iter().map(|l| l.len() + 1).sum();
        assert!(
            returned <= MAX_PAGE_BYTES,
            "page exceeded byte budget: {returned}"
        );
        assert!(page.has_more, "a 1 MB log must report more");
        assert_eq!(
            page.next_cursor,
            page.lines.len(),
            "next_cursor must equal lines returned so paging doesn't skip content"
        );
    }

    #[test]
    fn paginate_truncates_a_single_oversized_line() {
        let huge = "a".repeat(MAX_LINE_BYTES * 4);
        let all = vec![huge.as_str()];
        let page = paginate(&all, 0, DEFAULT_PAGE);
        assert_eq!(page.lines.len(), 1, "one line in, one line out");
        assert!(
            page.lines[0].len() < huge.len(),
            "oversized line must be truncated"
        );
        assert!(
            page.lines[0].contains("[+"),
            "truncated line must carry a dropped-bytes marker: {}",
            &page.lines[0][page.lines[0].len().saturating_sub(40)..]
        );
        assert!(!page.has_more, "single line, fully consumed");
    }

    #[test]
    fn paginate_never_returns_empty_page_for_nonempty_input() {
        // A single line far larger than the page budget must still come back
        // (clamped to MAX_LINE_BYTES) and advance the cursor — never a zero-line,
        // stuck page. clamp_line keeps it under the budget, so a following small
        // line fits too.
        let big = "z".repeat(MAX_PAGE_BYTES * 2);
        let all = vec![big.as_str(), "next"];
        let page = paginate(&all, 0, DEFAULT_PAGE);
        assert!(!page.lines.is_empty(), "must return at least one line");
        assert!(
            page.lines[0].len() <= MAX_LINE_BYTES + 32,
            "oversized line must be clamped under the budget"
        );
        assert_eq!(page.next_cursor, page.lines.len(), "cursor tracks lines");
        assert!(
            !page.has_more,
            "both lines fit once the huge one is clamped"
        );
    }

    #[tokio::test]
    async fn read_page_errors_on_missing_log() {
        // A missing log is an error, not a silently-empty page.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.log");
        assert!(read_page(&missing, 0, 100).await.is_err());
    }
}
