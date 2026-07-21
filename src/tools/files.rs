//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them — bounded by the runner in [`shell`].
//!
//! Ops return *facts* — a [`FileOutcome`] or a typed [`FileError`] — never a
//! finished sentence. The agent-facing wording, and every hint that names the
//! `file` tool's own actions, is rendered by the adapter in [`super`].
use tokio::{fs, io::AsyncWriteExt};

mod shell;

use shell::{ShError, sh};

/// What a file op produced. The text-producing ops (`read`/`list`/`grep`) carry
/// their already-bounded output; the mutating ops carry only *what happened*, so
/// the sentence describing it stays in the tool adapter.
#[derive(Debug)]
pub enum FileOutcome {
    Output(String),
    /// `grep` matched nothing — a result, not an error, and its own variant so it
    /// can't be confused with matching an empty line.
    NoMatches,
    Wrote {
        path: String,
        bytes: usize,
    },
    Appended {
        path: String,
        bytes: usize,
    },
    Deleted {
        path: String,
    },
    Moved {
        src: String,
        dest: String,
    },
}

/// Why a file op couldn't produce a result. Variants stay structured so the
/// adapter can phrase a next step (redirect a directory `read` to `list`, explain
/// a refused clobbering move) and so a failed `ls`/`find`/`grep` keeps its typed
/// [`ShError`] — grep's exit-1-means-no-matches lives on the code, not on message
/// text, and flattening to a string threw that away.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{path} is a directory")]
    IsDirectory { path: String },
    #[error("destination exists: {dest}")]
    DestinationExists { dest: String },
    #[error(transparent)]
    Shell(#[from] ShError),
}

/// Deepest a recursive `list` (`find`) descends below its starting point. The
/// streamed byte cap already bounds the *output*, but a pathologically deep tree
/// (or a bind-mount / hardlink cycle) could still drive `find` far down before it
/// emits cap-worth of text; `-maxdepth` bounds the walk itself. 20 clears any real
/// source tree while keeping a runaway descent finite.
const MAX_FIND_DEPTH: u32 = 20;

/// Largest single line accumulated while streaming a `read`. `read_until` would
/// buffer a whole line before any trimming, so a no-newline multi-GB file could OOM
/// the service. We keep at most this many bytes per line and drop the rest to the
/// next newline — the agent still sees the line's head, memory stays bounded.
const MAX_READ_LINE_BYTES: usize = 64 * 1024;

/// Stamped on a `read` line the moment it overflows `MAX_READ_LINE_BYTES`, so a
/// dropped tail is visible rather than silently truncated — mirroring the `…[+N
/// bytes]` tag `paginate` adds when it clamps a line for display. `…` is U+2026 (a
/// 3-byte char), so the whole marker is valid UTF-8 that the lossy decode passes
/// through unchanged.
const LINE_TRUNCATED: &str = "…[truncated]";

/// Read a file, paginated by line AND bounded by bytes (via the shared job-log
/// paginator) so neither a huge file nor a single pathological line can flood the
/// agent context.
///
/// Streams line by line, holding at most the requested window (plus the line in
/// flight) — never the whole file. Slurping first and paginating after bounded
/// the reply but not server memory: paging 200 lines of a multi-GB log would
/// have materialized all of it and could OOM the service.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<FileOutcome, FileError> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    // A directory can't be read as text: a typed variant lets the adapter redirect
    // to `list` instead of handing back a raw "Is a directory" errno.
    match fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return Err(FileError::IsDirectory {
                path: path.to_string(),
            });
        }
        Ok(_) => {}
        Err(e) => return Err(e.into()),
    }
    let limit = limit.max(1);
    let file = fs::File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut total = 0usize;
    let mut window: Vec<String> = Vec::new();
    // The line in flight, capped at MAX_READ_LINE_BYTES: fill_buf/consume lets us
    // stop accumulating a pathological no-newline line instead of read_until slurping
    // the whole file into one buffer.
    let mut line: Vec<u8> = Vec::new();
    // Whether the scan reached EOF. If it did, `total` is the file's exact line
    // count; if we stopped early (window full), `total` is only a lower bound.
    let mut reached_eof = false;
    loop {
        // Early stop: the window is full, so the page is complete. Counting the rest
        // of the file just to print an exact total would walk a multi-GB log to answer
        // page 1 — exactly what a line cursor exists to avoid. Leave `total` a lower
        // bound and stop.
        if window.len() >= limit {
            break;
        }
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            reached_eof = true;
            // EOF: emit a final line that had no terminating newline.
            if !line.is_empty() {
                if total >= cursor && window.len() < limit {
                    window.push(String::from_utf8_lossy(&line).into_owned());
                }
                total += 1;
            }
            break;
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(nl) => {
                append_capped(&mut line, &chunk[..nl]);
                reader.consume(nl + 1);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if total >= cursor && window.len() < limit {
                    // Binary-safe: replace non-UTF-8 bytes with U+FFFD, like `lines()`.
                    window.push(String::from_utf8_lossy(&line).into_owned());
                }
                total += 1;
                line.clear();
            }
            None => {
                let n = chunk.len();
                append_capped(&mut line, chunk);
                reader.consume(n);
            }
        }
    }
    // Forward pagination from the top (cursor 0 = first line): a file is read
    // start-to-end. (`job poll` instead reads newest-first — a live log's latest
    // output matters most.) Same byte/line ceilings via `paginate`, applied to
    // the pre-cut window (so the byte cap can shorten the page further).
    let refs: Vec<&str> = window.iter().map(String::as_str).collect();
    let page = crate::jobs::paginate(&refs, 0, limit);
    let body = page.lines.join("\n");
    let next = cursor.min(total) + page.lines.len();
    // More remains either way: with an exact total (EOF reached) part still lies
    // ahead; or we stopped early, so a tail past the window is unknown-but-present.
    let more_remains = !reached_eof || next < total;
    if !more_remains {
        return Ok(FileOutcome::Output(body));
    }
    // Early stop leaves `total` a lower bound — report it as `≥ n`, not a false exact.
    let total_desc = if reached_eof {
        total.to_string()
    } else {
        format!("≥{total}")
    };
    // The cursor footer rides along with the page it describes: it's part of the
    // paginated *output*, not a sentence invented about an op that produced none.
    Ok(FileOutcome::Output(format!(
        "{body}\n[lines {cursor}..{next} of {total_desc}; next_cursor={next}]"
    )))
}

/// Append `bytes` to the in-flight line, holding it at `MAX_READ_LINE_BYTES` so one
/// pathological no-newline line can't grow the buffer without bound. On the first
/// overflow the head that fits is kept — trimmed to a UTF-8 char boundary so the
/// marker isn't preceded by a split code point — a `LINE_TRUNCATED` marker is
/// stamped, and the line is *sealed*: those marker bytes push its length past the
/// cap, so `len() > MAX_READ_LINE_BYTES` reads as "already sealed" and every later
/// call is a no-op. The marker is written exactly once; overflow bytes are dropped.
fn append_capped(line: &mut Vec<u8>, bytes: &[u8]) {
    if line.len() > MAX_READ_LINE_BYTES {
        return; // sealed: the marker is already stamped, past the cap
    }
    let room = MAX_READ_LINE_BYTES - line.len();
    if bytes.len() <= room {
        line.extend_from_slice(bytes);
        return;
    }
    let keep = floor_char_boundary(bytes, room);
    line.extend_from_slice(&bytes[..keep]);
    line.extend_from_slice(LINE_TRUNCATED.as_bytes());
}

/// Largest index `≤ max` in `bytes` that starts a UTF-8 code point — i.e. is not a
/// continuation byte (`0b10xx_xxxx`). A code point is at most 4 bytes, so this walks
/// back at most three, keeping a multi-byte char from being split right before the
/// truncation marker. Works on arbitrary bytes (a binary `read`) and never panics.
fn floor_char_boundary(bytes: &[u8], max: usize) -> usize {
    let mut i = max.min(bytes.len());
    // Floor the walk at three bytes. Unbounded, a run of continuation bytes (a
    // binary `read`) could collapse `i` to 0 — the head would vanish and, worse,
    // the marker alone would leave the line *under* the cap, breaking the
    // length-based seal in `append_capped` so a second marker could be stamped.
    let floor = i.saturating_sub(3);
    while i > floor && i < bytes.len() && (bytes[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    i
}

pub async fn write(path: &str, content: &str) -> Result<FileOutcome, FileError> {
    ensure_parent(path).await?;
    fs::write(path, content).await?;
    Ok(FileOutcome::Wrote {
        path: path.to_string(),
        bytes: content.len(),
    })
}

pub async fn append(path: &str, content: &str) -> Result<FileOutcome, FileError> {
    ensure_parent(path).await?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(content.as_bytes()).await?;
    // Flush before returning so a follow-up read is guaranteed to see the bytes.
    f.flush().await?;
    Ok(FileOutcome::Appended {
        path: path.to_string(),
        bytes: content.len(),
    })
}

pub async fn delete(path: &str) -> Result<FileOutcome, FileError> {
    // symlink_metadata, not metadata: a symlink must be unlinked, never followed.
    // Following classified a dir-symlink as a directory (remove_dir_all refuses
    // the top-level link) and made a dangling symlink undeletable (ENOENT on the
    // stat before any removal was attempted).
    let meta = fs::symlink_metadata(path).await?;
    if meta.is_dir() {
        fs::remove_dir_all(path).await?;
    } else {
        fs::remove_file(path).await?;
    }
    Ok(FileOutcome::Deleted {
        path: path.to_string(),
    })
}

pub async fn rename(src: &str, dest: &str) -> Result<FileOutcome, FileError> {
    // No silent clobber: `fs::rename` overwrites an existing `dest`, destroying data.
    // symlink_metadata (don't follow) so a symlink already at `dest` also counts as
    // occupying the path — we must not follow it and overwrite its target.
    if fs::symlink_metadata(dest).await.is_ok() {
        return Err(FileError::DestinationExists {
            dest: dest.to_string(),
        });
    }
    ensure_parent(dest).await?;
    fs::rename(src, dest).await?;
    Ok(FileOutcome::Moved {
        src: src.to_string(),
        dest: dest.to_string(),
    })
}

/// Create the target's parent directories so writing a new file under a fresh
/// path "just works" (like `mkdir -p` before a redirect), instead of failing with
/// a bare `ENOENT` the agent then has to diagnose. A no-op when the parent already
/// exists or the path has none (a bare filename in the cwd).
async fn ensure_parent(path: &str) -> Result<(), FileError> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).await?;
        }
    }
    Ok(())
}

pub async fn list(path: &str, recursive: bool) -> Result<FileOutcome, FileError> {
    if recursive {
        // `find` has no `--`; anchor a leading-dash relative path with `./` so
        // it can't be parsed as an expression.
        let path = if path.starts_with('-') {
            format!("./{path}")
        } else {
            path.to_string()
        };
        // `-maxdepth` (after the starting point, before any test → no find warning)
        // caps how far the walk descends, so a deep tree can't recurse without bound.
        let depth = MAX_FIND_DEPTH.to_string();
        Ok(FileOutcome::Output(
            sh("find", &[&path, "-maxdepth", &depth]).await?,
        ))
    } else {
        Ok(FileOutcome::Output(sh("ls", &["-la", "--", path]).await?))
    }
}

pub async fn grep(pattern: &str, path: &str, recursive: bool) -> Result<FileOutcome, FileError> {
    let flag = if recursive { "-rn" } else { "-n" };
    // `--` so a pattern like `->` or `-r` is a pattern, not an option: without
    // it, grepping Rust code for `->` errored, and `-r` silently recursed with
    // the *path* as the pattern — wrong results, not even an error.
    match sh("grep", &[flag, "--", pattern, path]).await {
        Ok(s) => Ok(FileOutcome::Output(s)),
        // Exit 1 is grep's "no line matched" — a legitimate empty result, so it
        // leaves as an outcome rather than an error.
        Err(ShError::Status { code: 1, out }) if out.is_empty() => Ok(FileOutcome::NoMatches),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests;
