//! The bounded runner behind the shelled-out file ops (`ls`/`find`/`grep`):
//! spawn a read-only tool, stream its combined output under a byte cap and a
//! wall-clock deadline, kill it when either is hit. File semantics live in the
//! parent module; this one only knows how to run a child safely.
use std::time::Duration;

/// Largest shell-listing (`ls`/`find`/`grep`) output buffered and returned to the
/// agent. Unlike `read`, these aren't line-cursored, so a `find /` or `grep -r` on
/// a huge tree would otherwise dump unbounded text into memory *and* context. The
/// runner streams the child's output and stops at this cap — killing the child —
/// and flags the output `truncated` so the adapter can say so.
pub(in crate::tools) const MAX_SHELL_OUTPUT_BYTES: usize = 64 * 1024;

/// Wall-clock ceiling for a single `ls`/`find`/`grep`. Unlike `bash`, the `file`
/// tool has no inline window or backgrounding, so a `find /` or `grep -rn pat /`
/// would otherwise hang the MCP request forever. On elapse the runner kills the
/// child and returns a timeout error.
pub(super) const MAX_SHELL_RUN_SECS: u64 = 30;

/// A clean (exit 0) run's combined output. `truncated` is a fact — the byte cap
/// cut the stream and the child was killed — rendered into a sentence by the
/// adapter, never stamped into `text` here.
#[derive(Debug)]
pub(in crate::tools) struct ShOutput {
    pub text: String,
    pub truncated: bool,
}

/// A shelled-out command that didn't produce a clean result. Exit status and
/// combined output stay separate so callers can special-case a status (grep's
/// exit-1-means-no-matches, a partial listing) without parsing message text.
/// Display is the bare fact; any advice is the adapter's to add.
///
/// Visible to the whole `tools` module, not just `files`: `FileError::Shell`
/// carries it out intact, so it must be at least as visible as `FileError`.
#[derive(Debug, thiserror::Error)]
pub(in crate::tools) enum ShError {
    #[error("{0}")]
    Spawn(String),
    // `wrote_stdout`: the child produced real results before failing (a recursive
    // listing with one unreadable subdir) — the caller may treat that as partial
    // success rather than a failure.
    #[error("exit status {code}: {out}")]
    Status {
        code: i32,
        out: String,
        wrote_stdout: bool,
    },
    // A signal death carries no exit code: its own variant keeps it distinct from a
    // real status so the grep-exit-1 path can never accidentally match it.
    #[error("killed by signal: {out}")]
    Signal { out: String },
    // A wall-clock timeout: the child outran the deadline and was killed. Its own
    // variant (not a sentinel status) keeps grep's exit-1 no-match path from ever
    // matching it, and carries the partial output gathered before the deadline.
    #[error("timed out after {secs}s")]
    Timeout { secs: u64, out: String },
}

/// Run one of our read-only shell tools (`ls`/`find`/`grep`) with the module's
/// memory + wall-clock bounds applied. Combined stdout+stderr as `Ok` only on
/// exit 0 — a failed command must surface as an error, not as a success whose
/// body happens to contain `ls: cannot access ...`.
pub(super) async fn sh(prog: &str, args: &[&str]) -> Result<ShOutput, ShError> {
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
/// there is no process group to signal). A capped run is `Ok` with `truncated`;
/// a timed-out one is `Err(Timeout)` carrying the partial output. A clean finish
/// keeps the contract: `Ok` only on exit 0.
async fn run_bounded(
    prog: &str,
    args: &[&str],
    deadline: Duration,
    max_bytes: usize,
) -> Result<ShOutput, ShError> {
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
    let mut wrote_stdout = false;
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
                Ok(n) => {
                    wrote_stdout = true;
                    if extend_bounded(&mut buf, &out_chunk[..n], max_bytes) {
                        capped = true;
                        break;
                    }
                }
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
        return Ok(ShOutput {
            text: String::from_utf8_lossy(&buf).into_owned(),
            truncated: true,
        });
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
        Ok(ShOutput {
            text: out,
            truncated: false,
        })
    } else {
        // No exit code means a signal killed the process — model it as its own
        // variant rather than folding it into a sentinel status.
        Err(match status.code() {
            Some(code) => ShError::Status {
                code,
                out,
                wrote_stdout,
            },
            None => ShError::Signal { out },
        })
    }
}

/// Extend `buf` with `chunk`, stopping at `max` bytes total. Returns `true` once
/// bytes had to be *dropped* — the caller then stops reading and kills the child,
/// so the peak buffer tracks `max` rather than the whole (possibly unbounded)
/// stream. Output of exactly `max` bytes fits and is not truncated.
fn extend_bounded(buf: &mut Vec<u8>, chunk: &[u8], max: usize) -> bool {
    let room = max.saturating_sub(buf.len());
    let take = room.min(chunk.len());
    buf.extend_from_slice(&chunk[..take]);
    take < chunk.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_output_is_capped_and_producer_killed() {
        // Emit 3x the cap, then hang (the `exec` makes the hanging process the very
        // child we spawned, so the runner's kill reaps it — no orphan). A correct
        // runner reads to the cap, kills, and returns in milliseconds; one that waited
        // for EOF would sit out the sleep — so the outer timeout proves the kill, and
        // the bounded length proves the peak buffer tracks the cap. Bounded producer
        // on purpose: a regression can't spin on an infinite stream.
        let script = format!(
            "head -c {} /dev/zero; exec sleep 30",
            MAX_SHELL_OUTPUT_BYTES * 3
        );
        let result = tokio::time::timeout(Duration::from_secs(10), sh("sh", &["-c", &script]))
            .await
            .expect("runner must terminate an infinite producer (streamed cap)")
            .expect("a capped listing is still Ok");
        assert!(result.truncated, "a capped run must be flagged truncated");
        assert!(
            result.text.len() <= MAX_SHELL_OUTPUT_BYTES,
            "peak buffer must track the cap, got {} bytes",
            result.text.len()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shell_run_times_out_and_reaps_child() {
        // A command that outlives the deadline must error promptly, not wait out the
        // full sleep. `run_bounded` SIGKILLs and awaits the child, so it's reaped — no
        // orphan outlives this call.
        let start = std::time::Instant::now();
        let err = run_bounded(
            "sleep",
            &["30"],
            Duration::from_millis(200),
            MAX_SHELL_OUTPUT_BYTES,
        )
        .await
        .expect_err("a command exceeding the deadline must error");
        let elapsed = start.elapsed();
        assert!(
            matches!(err, ShError::Timeout { .. }),
            "must surface as a timeout: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "the deadline must fire promptly, not wait out the child: {elapsed:?}"
        );
        assert_eq!(
            err.to_string(),
            "timed out after 0s",
            "Display is the bare fact — advice belongs to the adapter"
        );
    }

    /// F12: output of exactly the cap fits — it must not be labelled truncated.
    #[cfg(unix)]
    #[tokio::test]
    async fn output_of_exactly_the_cap_is_not_truncated() {
        let out = run_bounded("printf", &["abcd"], Duration::from_secs(5), 4)
            .await
            .unwrap();
        assert_eq!(out.text, "abcd");
        assert!(!out.truncated, "exactly-cap output lost nothing");

        let out = run_bounded("printf", &["abcde"], Duration::from_secs(5), 4)
            .await
            .unwrap();
        assert_eq!(out.text, "abcd");
        assert!(out.truncated, "one byte over the cap is truncated");
    }

    #[test]
    fn extend_bounded_reports_only_dropped_bytes() {
        let mut buf = Vec::new();
        assert!(!extend_bounded(&mut buf, b"ab", 4));
        assert!(
            !extend_bounded(&mut buf, b"cd", 4),
            "exactly full, nothing dropped"
        );
        assert!(
            extend_bounded(&mut buf, b"e", 4),
            "a byte past the cap is dropped"
        );
        assert_eq!(buf, b"abcd");
    }
}
