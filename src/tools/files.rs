//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them.
use tokio::{fs, io::AsyncWriteExt};

/// Largest shell-listing (`ls`/`find`/`grep`) output returned to the agent. Unlike
/// `read`, these aren't line-cursored, so a `find /` or `grep -r` on a huge tree
/// would otherwise dump unbounded text into context. Truncate with a marker that
/// tells the agent to narrow the path/pattern.
const MAX_SHELL_OUTPUT_BYTES: usize = 64 * 1024;

/// Largest single line accumulated while streaming a `read`. `read_until` would
/// buffer a whole line before any trimming, so a no-newline multi-GB file could OOM
/// the service. We keep at most this many bytes per line and drop the rest to the
/// next newline — the agent still sees the line's head, memory stays bounded.
const MAX_READ_LINE_BYTES: usize = 64 * 1024;

/// Read a file, paginated by line AND bounded by bytes (via the shared job-log
/// paginator) so neither a huge file nor a single pathological line can flood the
/// agent context.
///
/// Streams line by line, holding at most the requested window (plus the line in
/// flight) — never the whole file. Slurping first and paginating after bounded
/// the reply but not server memory: paging 200 lines of a multi-GB log would
/// have materialized all of it and could OOM the service.
pub async fn read(path: &str, cursor: usize, limit: usize) -> Result<String, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    // A directory can't be read as text: surface a useful redirect to `list`
    // instead of a raw "Is a directory" errno.
    match fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return Err(format!(
                "{path} is a directory — use file(action=\"list\", path=\"{path}\") instead"
            ));
        }
        Ok(_) => {}
        Err(e) => return Err(e.to_string()),
    }
    let limit = limit.max(1);
    let file = fs::File::open(path).await.map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(file);
    let mut total = 0usize;
    let mut window: Vec<String> = Vec::new();
    // The line in flight, capped at MAX_READ_LINE_BYTES: fill_buf/consume lets us
    // stop accumulating a pathological no-newline line instead of read_until slurping
    // the whole file into one buffer.
    let mut line: Vec<u8> = Vec::new();
    loop {
        let chunk = reader.fill_buf().await.map_err(|e| e.to_string())?;
        if chunk.is_empty() {
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
    if next < total {
        Ok(format!(
            "{body}\n[lines {cursor}..{next} of {total}; next_cursor={next}]"
        ))
    } else {
        Ok(body)
    }
}

/// Append `bytes` to the in-flight line without letting it grow past
/// `MAX_READ_LINE_BYTES`. Overflow bytes are dropped (the line is already longer
/// than any page will show), so one pathological no-newline line can't grow the
/// buffer without bound.
fn append_capped(line: &mut Vec<u8>, bytes: &[u8]) {
    let room = MAX_READ_LINE_BYTES.saturating_sub(line.len());
    if room == 0 {
        return;
    }
    line.extend_from_slice(&bytes[..room.min(bytes.len())]);
}

pub async fn write(path: &str, content: &str) -> Result<String, String> {
    ensure_parent(path).await?;
    fs::write(path, content).await.map_err(|e| e.to_string())?;
    Ok(format!("wrote {} bytes to {path}", content.len()))
}

pub async fn append(path: &str, content: &str) -> Result<String, String> {
    ensure_parent(path).await?;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| e.to_string())?;
    f.write_all(content.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    // Flush before returning so a follow-up read is guaranteed to see the bytes.
    f.flush().await.map_err(|e| e.to_string())?;
    Ok(format!("appended {} bytes to {path}", content.len()))
}

pub async fn delete(path: &str) -> Result<String, String> {
    // symlink_metadata, not metadata: a symlink must be unlinked, never followed.
    // Following classified a dir-symlink as a directory (remove_dir_all refuses
    // the top-level link) and made a dangling symlink undeletable (ENOENT on the
    // stat before any removal was attempted).
    let meta = fs::symlink_metadata(path)
        .await
        .map_err(|e| e.to_string())?;
    let r = if meta.is_dir() {
        fs::remove_dir_all(path).await
    } else {
        fs::remove_file(path).await
    };
    r.map_err(|e| e.to_string())?;
    Ok(format!("deleted {path}"))
}

pub async fn rename(src: &str, dest: &str) -> Result<String, String> {
    ensure_parent(dest).await?;
    fs::rename(src, dest).await.map_err(|e| e.to_string())?;
    Ok(format!("moved {src} -> {dest}"))
}

/// Create the target's parent directories so writing a new file under a fresh
/// path "just works" (like `mkdir -p` before a redirect), instead of failing with
/// a bare `ENOENT` the agent then has to diagnose. A no-op when the parent already
/// exists or the path has none (a bare filename in the cwd).
async fn ensure_parent(path: &str) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

pub async fn list(path: &str, recursive: bool) -> Result<String, String> {
    if recursive {
        // `find` has no `--`; anchor a leading-dash relative path with `./` so
        // it can't be parsed as an expression.
        let path = if path.starts_with('-') {
            format!("./{path}")
        } else {
            path.to_string()
        };
        sh("find", &[&path]).await.map_err(|e| e.to_string())
    } else {
        sh("ls", &["-la", "--", path])
            .await
            .map_err(|e| e.to_string())
    }
}

pub async fn grep(pattern: &str, path: &str, recursive: bool) -> Result<String, String> {
    let flag = if recursive { "-rn" } else { "-n" };
    // `--` so a pattern like `->` or `-r` is a pattern, not an option: without
    // it, grepping Rust code for `->` errored, and `-r` silently recursed with
    // the *path* as the pattern — wrong results, not even an error.
    match sh("grep", &[flag, "--", pattern, path]).await {
        Ok(s) => Ok(s),
        // Exit 1 is grep's "no line matched" — a legitimate empty result. The
        // marker keeps it distinguishable from matching an empty line.
        Err(ShError::Status { code: 1, out }) if out.is_empty() => {
            Ok("[grep: no matches]".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}

/// A shelled-out command that didn't produce a clean result. Exit status and
/// combined output stay separate so callers can special-case a status (grep's
/// exit-1-means-no-matches) without parsing message text.
#[derive(Debug, thiserror::Error)]
enum ShError {
    #[error("{0}")]
    Spawn(String),
    #[error("exit status {code}: {out}")]
    Status { code: i32, out: String },
    // A signal death carries no exit code: its own variant keeps it distinct from a
    // real status so the grep-exit-1 path can never accidentally match it.
    #[error("killed by signal: {out}")]
    Signal { out: String },
}

/// Run `prog` and return its combined stdout+stderr — as `Ok` only on exit 0.
/// A failed command must surface as an error, not as a success whose body
/// happens to contain `ls: cannot access ...`.
async fn sh(prog: &str, args: &[&str]) -> Result<String, ShError> {
    let out = tokio::process::Command::new(prog)
        .args(args)
        .output()
        .await
        .map_err(|e| ShError::Spawn(e.to_string()))?;
    let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.stderr.is_empty() {
        s.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    let s = cap_bytes(s, MAX_SHELL_OUTPUT_BYTES);
    if out.status.success() {
        Ok(s)
    } else {
        // No exit code means a signal killed the process — model it as its own
        // variant rather than folding it into a sentinel status.
        Err(match out.status.code() {
            Some(code) => ShError::Status { code, out: s },
            None => ShError::Signal { out: s },
        })
    }
}

/// Bound a non-paginated listing to `max` bytes, cut on a UTF-8 boundary, with a
/// marker telling the agent to narrow the path/pattern. A no-op under the cap.
fn cap_bytes(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    let dropped = s.len() - end;
    s.truncate(end);
    s.push_str(&format!(
        "\n[output truncated: +{dropped} bytes — narrow the path or pattern]"
    ));
    s
}

#[cfg(test)]
mod tests;
