//! Bounded reading of per-job output files.
//!
//! A job log can be any size (a runaway command writes gigabytes before the live
//! cap stops it), so nothing here reads a log whole: [`read_tail`] walks backwards
//! from a byte offset in fixed chunks and stops at a line budget, a byte budget, or
//! a scan ceiling — whichever comes first. Memory and I/O per call are bounded by
//! the budget, never by the file size.
use std::io;
use std::path::{Path, PathBuf};

/// One newest-first page of a job's output: `lines` in chronological order, and
/// `older` — the byte offset where this page starts — to pass back as the cursor
/// for the next (older) page. An absolute offset rather than a count of lines from
/// the end: a live job keeps appending, and a count from a moving end would
/// re-serve lines on every page. `None` once the start of the output is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailPage {
    pub lines: Vec<String>,
    pub older: Option<u64>,
}

/// How much one backward read may return: at most `lines` lines and `bytes` bytes
/// of (already clamped) text, always at least one line so paging makes progress.
#[derive(Debug, Clone, Copy)]
pub struct TailBudget {
    pub lines: usize,
    pub bytes: usize,
}

impl TailBudget {
    /// One `job(action="poll")` page: `limit` lines (at least one) within
    /// [`MAX_PAGE_BYTES`].
    pub fn page(limit: usize) -> Self {
        Self {
            lines: limit.max(1),
            bytes: MAX_PAGE_BYTES,
        }
    }
}

pub const DEFAULT_PAGE: usize = 200;

/// Hard ceiling on the bytes a single page returns, regardless of line count.
/// Line-count pagination alone doesn't bound context: one 10 MB minified line, or
/// 200 long lines, would still flood the agent. A page stops accumulating once it
/// would exceed this (but always returns at least one line, so paging makes
/// progress). ~64 KB is generous for reading yet safely under any context window.
pub const MAX_PAGE_BYTES: usize = 64 * 1024;

/// Longest single line returned verbatim. A longer one keeps its newest
/// `MAX_LINE_BYTES` behind a `[+N bytes]…` marker, so one pathological line can't
/// blow the byte budget on its own.
pub const MAX_LINE_BYTES: usize = 8 * 1024;

/// Bytes read per backward step.
const CHUNK: usize = 64 * 1024;

/// How far past its byte budget one backward read may scan looking for a line
/// start. A log with no newline for gigabytes (`head -c` of a binary, a progress
/// bar that only ever writes `\r`) would otherwise be walked to the start of the
/// file to find where its last "line" begins. Past this the line is cut and the
/// next page resumes inside it.
const SCAN_SLACK: u64 = 1024 * 1024;

/// Prefix on a line whose earlier bytes were cut by the scan ceiling; the next
/// (older) page continues the same line.
const CUT_MARKER: &str = "[…]";

/// The newest-first page of `path` that ends at byte `end` (`None` = the end of
/// the file). Never reads more than the budget plus a bounded scan ceiling,
/// whatever the file's size. A missing or unreadable log is an error, never an
/// empty page — "log gone" must not look like "no output yet".
pub async fn read_tail(path: &Path, end: Option<u64>, budget: TailBudget) -> io::Result<TailPage> {
    let path: PathBuf = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        tail_of(len, end, budget, |buf, offset| {
            file.read_exact_at(buf, offset)
        })
    })
    .await
    .map_err(io::Error::other)?
}

/// The newest-first page of an in-memory text (a finished job's saved tail), with
/// the same cursor semantics as [`read_tail`].
pub fn tail_of_text(text: &str, end: Option<u64>, budget: TailBudget) -> TailPage {
    let bytes = text.as_bytes();
    let read = |buf: &mut [u8], offset: u64| {
        let start = usize::try_from(offset).map_err(io::Error::other)?;
        let src = bytes
            .get(start..start + buf.len())
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buf.copy_from_slice(src);
        Ok(())
    };
    // An in-memory read past the length is the only failure, and `tail_of` never
    // asks for one — so this is an empty page only on a bug, never a panic.
    tail_of(bytes.len() as u64, end, budget, read).unwrap_or(TailPage {
        lines: Vec::new(),
        older: None,
    })
}

/// The last `budget.lines` lines of a log (within `budget.bytes`), newline-joined
/// with a trailing newline — the snapshot a finished job saves into its row so it
/// outlives the live log being trimmed or reaped.
pub async fn tail(path: &Path, budget: TailBudget) -> io::Result<String> {
    let page = read_tail(path, None, budget).await?;
    let mut out = page.lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The backward walk behind every tail read. `len` is the source length and
/// `read_at` fills a buffer from an absolute offset. Lines are `\n`-terminated; a
/// final unterminated line (output still being written) counts as a line.
fn tail_of(
    len: u64,
    end: Option<u64>,
    budget: TailBudget,
    mut read_at: impl FnMut(&mut [u8], u64) -> io::Result<()>,
) -> io::Result<TailPage> {
    let end = end.map_or(len, |e| e.min(len));
    let floor = end.saturating_sub(budget.bytes as u64 + SCAN_SLACK);
    let mut lines: Vec<String> = Vec::new(); // newest first
    let mut used = 0usize;
    // Start offset of the oldest line taken so far: the next page's `end`.
    let mut oldest = end;
    let mut line = LineTail::default();

    let mut chunk = vec![0u8; CHUNK];
    let mut chunk_start = end; // the loaded chunk starts here and ends at its last `i`
    let mut i = end; // the next byte examined is `i - 1`
    // A `\n` as the very last byte terminates the newest line rather than starting
    // an empty one after it.
    let mut skip_newline = true;

    loop {
        if i == 0 || i == floor {
            // Start of the source, or the scan ceiling inside one endless line.
            let cut = i > 0;
            let fits = lines.is_empty() || used + line.text_len(cut) <= budget.bytes;
            if i < end && fits {
                lines.push(line.finish(cut));
                oldest = i;
            }
            break;
        }
        if i <= chunk_start {
            let start = i.saturating_sub(CHUNK as u64).max(floor);
            let n = (i - start) as usize;
            read_at(&mut chunk[..n], start)?;
            chunk_start = start;
        }
        let byte = chunk[(i - 1 - chunk_start) as usize];
        if byte == b'\n' {
            if skip_newline {
                skip_newline = false;
                i -= 1;
                continue;
            }
            // `[i, …)` is a complete line.
            let fits = lines.is_empty() || used + line.text_len(false) <= budget.bytes;
            if !fits {
                break;
            }
            let text = line.finish(false);
            used += text.len() + 1;
            lines.push(text);
            oldest = i;
            if lines.len() >= budget.lines {
                break;
            }
            line = LineTail::default();
        } else {
            skip_newline = false;
            line.push_front(byte);
        }
        i -= 1;
    }

    lines.reverse();
    Ok(TailPage {
        lines,
        older: (oldest > 0).then_some(oldest),
    })
}

/// The visible tail of one line, accumulated right to left. Keeps what a terminal
/// would show: the text after the line's last `\r` (a progress bar redraws itself
/// with `\r`), and of that only the last `MAX_LINE_BYTES` — the newest part of an
/// over-long line, not its stale head.
#[derive(Default)]
struct LineTail {
    /// The kept bytes, in reverse order.
    rev: Vec<u8>,
    /// Bytes past `MAX_LINE_BYTES` that were dropped from the line's head.
    dropped: u64,
    /// A `\r` was seen: everything further left was overwritten on screen.
    overwritten: bool,
    /// At least one byte of this line was seen (to strip a CRLF's `\r`).
    seen: bool,
}

impl LineTail {
    fn push_front(&mut self, byte: u8) {
        let first = !self.seen;
        self.seen = true;
        if self.overwritten {
            return;
        }
        if byte == b'\r' {
            // A trailing `\r` is a CRLF line ending, not a redraw.
            if !first {
                self.overwritten = true;
            }
            return;
        }
        if self.rev.len() < MAX_LINE_BYTES {
            self.rev.push(byte);
        } else {
            self.dropped += 1;
        }
    }

    /// Length of the rendered line plus its newline, for the byte budget.
    fn text_len(&self, cut: bool) -> usize {
        self.rev.len() + marker(cut, self.dropped).len() + 1
    }

    fn finish(&mut self, cut: bool) -> String {
        let mut bytes: Vec<u8> = self.rev.iter().rev().copied().collect();
        if self.dropped > 0 {
            // The head was cut mid-character: drop orphaned continuation bytes.
            let skip = bytes
                .iter()
                .take(3)
                .take_while(|&&b| b & 0xC0 == 0x80)
                .count();
            bytes.drain(..skip);
            self.dropped += skip as u64;
        }
        format!(
            "{}{}",
            marker(cut, self.dropped),
            String::from_utf8_lossy(&bytes)
        )
    }
}

/// What precedes a line's kept text: [`CUT_MARKER`] when the scan ceiling cut it
/// (the rest is on the next page), then `[+N bytes]…` for a clamped head.
fn marker(cut: bool, dropped: u64) -> String {
    let cut = if cut { CUT_MARKER } else { "" };
    match dropped {
        0 => cut.to_string(),
        n => format!("{cut}[+{n} bytes]…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(text: &str, end: Option<u64>, limit: usize) -> TailPage {
        tail_of_text(text, end, TailBudget::page(limit))
    }

    #[test]
    fn newest_window_first_then_older_by_absolute_offset() {
        let text: String = (1..=10).map(|i| format!("{i}\n")).collect();
        let p0 = page(&text, None, 3);
        assert_eq!(p0.lines, ["8", "9", "10"], "newest window, in order");
        let older = p0.older.expect("older lines remain");
        assert_eq!(&text[older as usize..], "8\n9\n10\n", "cursor = page start");

        let p1 = page(&text, Some(older), 3);
        assert_eq!(p1.lines, ["5", "6", "7"]);

        let p3 = page(&text, Some(2), 3);
        assert_eq!(p3.lines, ["1"], "only the oldest line is left");
        assert_eq!(p3.older, None, "nothing older remains");
    }

    /// The cursor is anchored to bytes already written, so output appended after
    /// the first page never shifts the pages behind it (B8).
    #[test]
    fn appending_output_never_shifts_an_older_page() {
        let mut text: String = (1..=10).map(|i| format!("{i}\n")).collect();
        let older = page(&text, None, 3).older.unwrap();
        text.push_str("11\n12\n13\n");
        assert_eq!(page(&text, Some(older), 3).lines, ["5", "6", "7"]);
    }

    #[test]
    fn edge_shapes() {
        assert_eq!(
            page("", None, 5),
            TailPage {
                lines: vec![],
                older: None
            }
        );
        assert_eq!(page("abc", None, 5).lines, ["abc"], "no trailing newline");
        assert_eq!(page("\n\n", None, 5).lines, ["", ""], "two empty lines");
        assert_eq!(page("a\r\nb\r\n", None, 5).lines, ["a", "b"], "CRLF");
        assert_eq!(
            page("a\nb\n", Some(999), 5).lines,
            ["a", "b"],
            "cursor past end"
        );
        assert_eq!(
            page("a\nb\nc\n", None, 0).lines,
            ["c"],
            "limit 0 is one line"
        );
    }

    /// A progress bar redraws one line with `\r`: the page shows what the
    /// terminal would — the last frame, not the first (B7).
    #[test]
    fn carriage_return_keeps_the_last_frame() {
        assert_eq!(
            page("10%\r50%\r100%\ndone\n", None, 5).lines,
            ["100%", "done"]
        );
    }

    #[test]
    fn an_over_long_line_keeps_its_newest_bytes() {
        let line = format!("{}END", "x".repeat(100 * 1024));
        let p = page(&format!("{line}\n"), None, 5);
        assert_eq!(p.lines.len(), 1);
        let shown = &p.lines[0];
        assert!(shown.ends_with("END"), "the tail of the line survives");
        assert!(
            shown.len() <= MAX_LINE_BYTES + 32,
            "clamped: {}",
            shown.len()
        );
        let want = format!("[+{} bytes]…", line.len() - MAX_LINE_BYTES);
        assert!(
            shown.starts_with(&want),
            "exact dropped count: {}",
            &shown[..30]
        );
        assert_eq!(p.older, None, "the whole line was scanned");
    }

    /// A 5 MiB line exceeds the scan ceiling: the page shows its newest bytes,
    /// marked cut, and the next page resumes inside it.
    #[test]
    fn a_line_longer_than_the_scan_ceiling_is_cut_and_continued() {
        let text = format!("{}END\n", "x".repeat(5 * 1024 * 1024));
        let p = page(&text, None, 5);
        assert_eq!(p.lines.len(), 1);
        assert!(p.lines[0].starts_with(CUT_MARKER) && p.lines[0].ends_with("END"));
        assert!(p.lines[0].len() <= MAX_LINE_BYTES + 64);
        let next = page(&text, p.older, 5);
        assert!(next.lines[0].starts_with(CUT_MARKER) && next.older < p.older);
    }

    #[test]
    fn non_utf8_is_rendered_lossily_not_dropped() {
        let p = tail_of_bytes(b"line1\nline2\xff\xfeline3\n", 5);
        assert_eq!(p.lines.len(), 2);
        assert!(p.lines[1].contains("line3"));
    }

    fn tail_of_bytes(bytes: &[u8], limit: usize) -> TailPage {
        tail_of(
            bytes.len() as u64,
            None,
            TailBudget::page(limit),
            |buf, off| {
                let off = off as usize;
                buf.copy_from_slice(&bytes[off..off + buf.len()]);
                Ok(())
            },
        )
        .unwrap()
    }

    #[test]
    fn a_page_stays_under_the_byte_budget_keeping_the_newest() {
        let text: String = (0..1000).map(|i| format!("{i:01000}\n")).collect();
        let p = page(&text, None, DEFAULT_PAGE);
        let returned: usize = p.lines.iter().map(|l| l.len() + 1).sum();
        assert!(
            returned <= MAX_PAGE_BYTES,
            "byte budget honored: {returned}"
        );
        assert!(p.older.is_some());
        assert_eq!(p.lines.last().unwrap(), &format!("{:01000}", 999));
    }

    /// The server-OOM regression (B1): a multi-GB log with a short real tail must
    /// come back promptly having read only a bounded number of bytes.
    #[tokio::test]
    async fn a_huge_sparse_log_is_read_in_bounded_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge.log");
        let file = std::fs::File::create(&path).unwrap();
        let hole = 2u64 * 1024 * 1024 * 1024;
        file.set_len(hole).unwrap();
        drop(file);
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(b"\nlast-1\nlast-2\n").unwrap();
        }

        let started = std::time::Instant::now();
        let p = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_tail(&path, None, TailBudget::page(2)),
        )
        .await
        .expect("bounded read returns promptly")
        .unwrap();
        assert_eq!(p.lines, ["last-1", "last-2"]);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));

        // Count the bytes a full-budget read touches: the scan ceiling, not 2 GiB.
        let mut read = 0u64;
        let file = std::fs::File::open(&path).unwrap();
        let len = file.metadata().unwrap().len();
        let p = tail_of(len, None, TailBudget::page(DEFAULT_PAGE), |buf, off| {
            use std::os::unix::fs::FileExt;
            read += buf.len() as u64;
            file.read_exact_at(buf, off)
        })
        .unwrap();
        assert!(
            read <= MAX_PAGE_BYTES as u64 + SCAN_SLACK + CHUNK as u64,
            "read {read} bytes"
        );
        assert!(
            p.lines[0].starts_with(CUT_MARKER),
            "the endless line is cut"
        );
        assert!(p.older.is_some(), "and the next page resumes inside it");
    }

    #[tokio::test]
    async fn a_missing_log_is_an_error_not_an_empty_page() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.log");
        assert!(
            read_tail(&missing, None, TailBudget::page(5))
                .await
                .is_err()
        );
        assert!(tail(&missing, TailBudget::page(5)).await.is_err());
    }
}
