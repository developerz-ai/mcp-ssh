//! File operations, executed locally as the service user. Read/write/move go
//! through `tokio::fs`; list/grep shell out to `ls`/`find`/`grep` rather than
//! reimplementing them.
use std::time::Duration;

use tokio::{fs, io::AsyncWriteExt};

/// Largest shell-listing (`ls`/`find`/`grep`) output buffered and returned to the
/// agent. Unlike `read`, these aren't line-cursored, so a `find /` or `grep -r` on
/// a huge tree would otherwise dump unbounded text into memory *and* context. The
/// runner streams the child's output and stops at this cap — killing the child —
/// then appends a marker telling the agent to narrow the path/pattern.
const MAX_SHELL_OUTPUT_BYTES: usize = 64 * 1024;

/// Wall-clock ceiling for a single `ls`/`find`/`grep`. Unlike `bash`, the `file`
/// tool has no inline window or backgrounding, so a `find /` or `grep -rn pat /`
/// would otherwise hang the MCP request forever. On elapse the runner kills the
/// child and returns a timeout error.
const MAX_SHELL_RUN_SECS: u64 = 30;

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
        let chunk = reader.fill_buf().await.map_err(|e| e.to_string())?;
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
        return Ok(body);
    }
    // Early stop leaves `total` a lower bound — report it as `≥ n`, not a false exact.
    let total_desc = if reached_eof {
        total.to_string()
    } else {
        format!("≥{total}")
    };
    Ok(format!(
        "{body}\n[lines {cursor}..{next} of {total_desc}; next_cursor={next}]"
    ))
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
        // `-maxdepth` (after the starting point, before any test → no find warning)
        // caps how far the walk descends, so a deep tree can't recurse without bound.
        let depth = MAX_FIND_DEPTH.to_string();
        sh("find", &[&path, "-maxdepth", &depth])
            .await
            .map_err(|e| e.to_string())
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
    // A wall-clock timeout: the child outran the deadline and was killed. Its own
    // variant (not a sentinel status) keeps grep's exit-1 no-match path from ever
    // matching it, and carries the partial output gathered before the deadline.
    #[error("{out}\n[timed out after {secs}s — narrow the path or pattern]")]
    Timeout { secs: u64, out: String },
}

/// Run one of our read-only shell tools (`ls`/`find`/`grep`) with the module's
/// memory + wall-clock bounds applied. Combined stdout+stderr as `Ok` only on
/// exit 0 — a failed command must surface as an error, not as a success whose
/// body happens to contain `ls: cannot access ...`.
async fn sh(prog: &str, args: &[&str]) -> Result<String, ShError> {
    run_bounded(
        prog,
        args,
        Duration::from_secs(MAX_SHELL_RUN_SECS),
        MAX_SHELL_OUTPUT_BYTES,
    )
    .await
}

/// The streamed, capped, timed core behind `sh`. Streams the child's combined
/// stdout+stderr into a buffer bounded at `max_bytes`, under an overall
/// `deadline`.
///
/// Both pipes are drained concurrently — so neither can fill its OS buffer and
/// deadlock the child (`find /` as a non-root user floods *stderr* with
/// permission errors) — and reading stops at `max_bytes`: a huge tree can't
/// buffer its whole listing into memory. Hitting the cap or the deadline kills
/// the child (a plain `child.kill()`; these are direct, non-forking children, so
/// there is no process group to signal) and returns the partial output with a
/// truncation / timeout marker. A clean finish keeps the contract: `Ok` only on
/// exit 0.
async fn run_bounded(
    prog: &str,
    args: &[&str],
    deadline: Duration,
    max_bytes: usize,
) -> Result<String, ShError> {
    use tokio::io::AsyncReadExt;

    let mut child = tokio::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Kill the child if this future is dropped mid-run, so a cancelled request
        // never orphans a `find`/`grep` still walking the tree.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ShError::Spawn(e.to_string()))?;

    // `piped()` guarantees these are `Some`; match rather than `expect` so a
    // surprise `None` can't panic the request path.
    let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill().await;
        return Err(ShError::Spawn("child stdio pipe missing".into()));
    };

    let mut buf: Vec<u8> = Vec::new();
    let mut out_chunk = [0u8; 8 * 1024];
    let mut err_chunk = [0u8; 8 * 1024];
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut capped = false;
    let mut timed_out = false;
    let mut read_err: Option<std::io::Error> = None;

    let deadline_at = tokio::time::Instant::now() + deadline;
    let sleep = tokio::time::sleep_until(deadline_at);
    tokio::pin!(sleep);

    while !(stdout_done && stderr_done) {
        tokio::select! {
            _ = &mut sleep => {
                timed_out = true;
                break;
            }
            r = stdout.read(&mut out_chunk), if !stdout_done => match r {
                Ok(0) => stdout_done = true,
                Ok(n) => if extend_bounded(&mut buf, &out_chunk[..n], max_bytes) {
                    capped = true;
                    break;
                },
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            },
            r = stderr.read(&mut err_chunk), if !stderr_done => match r {
                Ok(0) => stderr_done = true,
                Ok(n) => if extend_bounded(&mut buf, &err_chunk[..n], max_bytes) {
                    capped = true;
                    break;
                },
                Err(e) => {
                    read_err = Some(e);
                    break;
                }
            },
        }
    }

    if let Some(e) = read_err {
        let _ = child.kill().await;
        return Err(ShError::Spawn(e.to_string()));
    }
    if timed_out {
        // Direct child, no subprocess tree — a plain kill (SIGKILL + reap) is
        // enough, and awaiting it means no orphan outlives this call.
        let _ = child.kill().await;
        return Err(ShError::Timeout {
            secs: deadline.as_secs(),
            out: String::from_utf8_lossy(&buf).into_owned(),
        });
    }
    if capped {
        let _ = child.kill().await;
        let mut out = String::from_utf8_lossy(&buf).into_owned();
        out.push_str(&format!(
            "\n[output truncated at {max_bytes} bytes — narrow the path or pattern]"
        ));
        return Ok(out);
    }

    // Both streams hit EOF within budget. Reap the process for its status, still
    // bounded by the same deadline so a child that closes its pipes but never
    // exits can't hang the request.
    let status = match tokio::time::timeout_at(deadline_at, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => return Err(ShError::Spawn(e.to_string())),
        Err(_elapsed) => {
            let _ = child.kill().await;
            return Err(ShError::Timeout {
                secs: deadline.as_secs(),
                out: String::from_utf8_lossy(&buf).into_owned(),
            });
        }
    };
    let out = String::from_utf8_lossy(&buf).into_owned();
    if status.success() {
        Ok(out)
    } else {
        // No exit code means a signal killed the process — model it as its own
        // variant rather than folding it into a sentinel status.
        Err(match status.code() {
            Some(code) => ShError::Status { code, out },
            None => ShError::Signal { out },
        })
    }
}

/// Extend `buf` with `chunk`, stopping at `max` bytes total. Returns `true` once
/// the cap is reached — the caller then stops reading and kills the child, so the
/// peak buffer tracks `max` rather than the whole (possibly unbounded) stream.
fn extend_bounded(buf: &mut Vec<u8>, chunk: &[u8], max: usize) -> bool {
    let room = max.saturating_sub(buf.len());
    let take = room.min(chunk.len());
    buf.extend_from_slice(&chunk[..take]);
    buf.len() >= max
}

#[cfg(test)]
mod tests;
