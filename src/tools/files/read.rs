//! `read`: a forward, line-cursored page of a regular file, bounded in lines,
//! bytes, per-line bytes and wall-clock time.
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

use super::{FileError, FileOutcome, PageEnd, ReadLine, ReadPage, io_err};

/// Most lines one page returns, whatever `limit` asks for. The byte budget below
/// already bounds memory; this keeps a `limit=usize::MAX` page of empty lines
/// from turning into a huge `Vec`.
pub(super) const MAX_READ_LINES: usize = 1000;

/// Byte budget for one page's text (same ceiling `job poll` uses). A page always
/// holds at least one line, so paging makes progress.
const MAX_READ_PAGE_BYTES: usize = 64 * 1024;

/// Longest line returned; the tail past it is dropped and *counted*, never
/// buffered — a no-newline multi-GB file can't OOM the service.
pub(super) const MAX_READ_LINE_BYTES: usize = 8 * 1024;

/// Wall-clock ceiling for one scan — the same 30 s the shelled-out ops get. A
/// large `cursor` on a huge file means scanning everything before it.
const READ_DEADLINE: Duration = Duration::from_secs(30);

/// Read lines `[cursor, cursor+limit)` of `path`, streaming: only the page (plus
/// the line in flight, capped) is ever held, never the whole file.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<FileOutcome, FileError> {
    let file = open_regular(path).await?;
    let limit = limit.clamp(1, MAX_READ_LINES);
    match tokio::time::timeout(READ_DEADLINE, scan(file, cursor, limit)).await {
        Ok(page) => page.map(FileOutcome::Read).map_err(io_err("read", path)),
        Err(_elapsed) => Err(FileError::ReadTimedOut {
            path: path.to_string(),
            secs: READ_DEADLINE.as_secs(),
        }),
    }
}

/// Open `path` and accept it only if the opened handle is a regular file.
/// `O_NONBLOCK` keeps opening a FIFO from blocking forever, and classifying the
/// *opened* fd (not a prior stat) leaves no swap-in race. A device or FIFO
/// never ends (`/dev/zero`) or never starts — refuse both.
async fn open_regular(path: &str) -> Result<tokio::fs::File, FileError> {
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .await
        .map_err(io_err("read", path))?;
    let meta = file.metadata().await.map_err(io_err("read", path))?;
    if meta.is_dir() {
        return Err(FileError::IsDirectory {
            path: path.to_string(),
        });
    }
    if !meta.is_file() {
        return Err(FileError::NotRegularFile {
            path: path.to_string(),
        });
    }
    Ok(file)
}

async fn scan(file: tokio::fs::File, cursor: usize, limit: usize) -> std::io::Result<ReadPage> {
    let mut reader = BufReader::new(file);
    let mut lines: Vec<ReadLine> = Vec::new();
    let mut page_bytes = 0usize;
    let mut line = LineBuf::default();
    // Complete lines seen so far, including any skipped before `cursor`.
    let mut seen = 0usize;
    let end = loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            // A final line without a terminating newline still counts.
            if line.has_content() {
                let done = line.take();
                seen += 1;
                if seen > cursor {
                    if !lines.is_empty() && page_bytes + done.text.len() + 1 > MAX_READ_PAGE_BYTES {
                        break PageEnd::More { at_least: seen };
                    }
                    lines.push(done);
                }
            }
            break PageEnd::Eof { total: seen };
        }
        let Some(nl) = chunk.iter().position(|&b| b == b'\n') else {
            let n = chunk.len();
            line.push(chunk);
            reader.consume(n);
            continue;
        };
        line.push(&chunk[..nl]);
        reader.consume(nl + 1);
        let done = line.take();
        seen += 1;
        if seen <= cursor {
            continue;
        }
        // Budget full: leave this line for the next page (its cursor points here).
        if !lines.is_empty() && page_bytes + done.text.len() + 1 > MAX_READ_PAGE_BYTES {
            break PageEnd::More { at_least: seen };
        }
        page_bytes += done.text.len() + 1;
        lines.push(done);
        if lines.len() == limit {
            // Peek once: a file of exactly `limit` lines must not advertise a
            // next page that would come back empty.
            break if reader.fill_buf().await?.is_empty() {
                PageEnd::Eof { total: seen }
            } else {
                PageEnd::More { at_least: seen + 1 }
            };
        }
    };
    let next = cursor.min(seen) + lines.len();
    Ok(ReadPage {
        lines,
        cursor,
        next,
        end,
    })
}

/// The line in flight. Keeps at most `MAX_READ_LINE_BYTES` (trimmed to a UTF-8
/// boundary) and only *counts* the rest, so one marker can state the true loss.
#[derive(Default)]
struct LineBuf {
    head: Vec<u8>,
    dropped: usize,
    // Once the cap is hit every later byte is dropped, even if the boundary
    // trim left room — the kept head stays a true prefix of the line.
    sealed: bool,
    ends_cr: bool,
}

impl LineBuf {
    fn has_content(&self) -> bool {
        !self.head.is_empty() || self.dropped > 0
    }

    fn push(&mut self, bytes: &[u8]) {
        let Some(&last) = bytes.last() else {
            return;
        };
        self.ends_cr = last == b'\r';
        if self.sealed {
            self.dropped += bytes.len();
            return;
        }
        let room = MAX_READ_LINE_BYTES - self.head.len();
        if bytes.len() <= room {
            self.head.extend_from_slice(bytes);
            return;
        }
        let keep = floor_char_boundary(bytes, room);
        self.head.extend_from_slice(&bytes[..keep]);
        self.dropped += bytes.len() - keep;
        self.sealed = true;
    }

    /// Finish the line: strip a CRLF's `\r` (from the head, or from the dropped
    /// count when it fell past the cap), decode lossily, reset.
    fn take(&mut self) -> ReadLine {
        if self.ends_cr {
            if self.dropped > 0 {
                self.dropped -= 1;
            } else {
                self.head.pop();
            }
        }
        let line = ReadLine {
            // Binary-safe: non-UTF-8 bytes become U+FFFD rather than an error.
            text: String::from_utf8_lossy(&self.head).into_owned(),
            dropped: self.dropped,
        };
        *self = Self::default();
        line
    }
}

/// Largest index `≤ max` in `bytes` that starts a UTF-8 code point — i.e. is not a
/// continuation byte (`0b10xx_xxxx`). Walks back at most three bytes (a code
/// point's longest tail), so a run of continuation bytes in binary input can't
/// collapse the head. Works on arbitrary bytes and never panics.
fn floor_char_boundary(bytes: &[u8], max: usize) -> usize {
    let mut i = max.min(bytes.len());
    let floor = i.saturating_sub(3);
    while i > floor && i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_long_line_keeps_the_head_and_counts_every_dropped_byte() {
        let mut line = LineBuf::default();
        line.push(&vec![b'a'; MAX_READ_LINE_BYTES + 5_000]);
        line.push(b"tail");
        let done = line.take();
        assert_eq!(done.text.len(), MAX_READ_LINE_BYTES);
        assert_eq!(done.dropped, 5_004, "every byte past the cap is counted");
        assert!(
            !done.text.contains("tail"),
            "bytes past the seal never land in the head"
        );
    }

    #[test]
    fn split_multibyte_char_at_the_cap_is_dropped_whole() {
        // The cap falls inside € (E2 82 AC): the straddling char goes to `dropped`.
        let mut bytes = vec![b'a'; MAX_READ_LINE_BYTES - 1];
        bytes.extend_from_slice("€".as_bytes());
        let mut line = LineBuf::default();
        line.push(&bytes);
        let done = line.take();
        assert_eq!(done.text.len(), MAX_READ_LINE_BYTES - 1);
        assert!(
            !done.text.contains('\u{FFFD}'),
            "no broken code point in the head"
        );
        assert_eq!(done.dropped, 3);
    }

    #[test]
    fn continuation_byte_run_cannot_collapse_the_head() {
        let mut bytes = vec![b'a'; MAX_READ_LINE_BYTES - 64];
        bytes.extend_from_slice(&[0x80u8; 128]);
        let mut line = LineBuf::default();
        line.push(&bytes);
        let done = line.take();
        assert!(done.text.starts_with("aaa"));
        assert!(done.dropped > 0 && done.dropped <= 64 + 3);
    }

    #[test]
    fn crlf_is_stripped_from_the_head_or_from_the_dropped_count() {
        let mut line = LineBuf::default();
        line.push(b"short\r");
        assert_eq!(
            line.take(),
            ReadLine {
                text: "short".into(),
                dropped: 0
            }
        );

        let mut long = vec![b'a'; MAX_READ_LINE_BYTES + 10];
        long.push(b'\r');
        line.push(&long);
        let done = line.take();
        assert_eq!(
            done.dropped, 10,
            "the \\r past the cap isn't counted as content"
        );
    }
}
