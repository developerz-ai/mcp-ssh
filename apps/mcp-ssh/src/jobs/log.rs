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
mod tests;
