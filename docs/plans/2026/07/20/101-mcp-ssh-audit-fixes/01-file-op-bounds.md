# 01 — File-op bounds (DoS + data-loss fixes)

> Part of [`overview.md`](overview.md). Depends on: none.

The `file` tool's shelled-out `ls`/`find`/`grep` path defeats the very bounding the module claims. `read` is memory-bounded but does full-file I/O. `move` clobbers silently. These are the highest-impact, exploitable issues found.

## Files to change
- `src/tools/files.rs:221-231` (`sh`) — HIGH. `tokio::process::Command::…output()` fully buffers stdout+stderr into RAM, then `cap_bytes(_, MAX_SHELL_OUTPUT_BYTES)` truncates *after*. `file(action="list", recursive=true, path="/")` / `grep -r` on a huge tree → server OOM. Stream child stdout with `Stdio::piped()`, read up to the cap, kill the child once exceeded. (Module comment `:6-10,22-25` claims this path is bounded — make it true.)
- `src/tools/files.rs:221-242` — HIGH. No deadline on `sh`. `find /` / `grep -rn pat /` hangs the MCP request forever (unlike `bash`, `file` has no inline window / backgrounding). Wrap in `tokio::time::timeout`; on elapse, kill the child and return a friendly "timed out" error.
- `src/tools/files.rs:170-201` (recursive `list` → `find`) — MEDIUM. No `-maxdepth`, no result cap. Add a sane `-maxdepth` and/or bound the child by the streamed cap from above.
- `src/tools/files.rs:26-96` (`read`) — MEDIUM. Loop runs to EOF to compute `total` even for `read(path,0,200)` → full-file I/O with no timeout on a multi-GB file. Stop once `cursor+limit` lines are collected; report `total` as `≥ n` / unknown past the first page.
- `src/tools/files.rs:149-153` (`rename`/`move`) — LOW/MEDIUM. `fs::rename` silently overwrites `dest`. Error if `dest` exists (stat first); no overwrite flag unless the goal demands one (don't grow the param surface speculatively).
- `src/tools/files.rs:102-108` (`append_capped`) — LOW. Over-long single line dropped past `MAX_READ_LINE_BYTES` with no marker (unlike `cap_bytes` `:256-258`, which appends one). Append a truncation marker so the agent knows the line was cut.

## Steps
1. Add a streamed, capped, timed shell runner: spawn with `.stdout(Stdio::piped()).stderr(Stdio::piped())`, read incrementally into a buffer capped at `MAX_SHELL_OUTPUT_BYTES`, wrap the whole read+wait in `tokio::time::timeout`. On cap-exceed or timeout, kill the child (process-group kill not needed here — these are short-lived non-backgrounded children; a plain `child.kill()` suffices) and return the partial output with a truncation/timeout marker. Introduce a `MAX_SHELL_RUN_SECS` const (reuse the config inline-timeout notion if it fits, else a local const).
2. Route `list`/`grep` through the new runner; add `-maxdepth` to the recursive `find` invocation (`:174-185`).
3. Rework `read` (`:26-96`) to break out of the line loop once `cursor+limit` lines are read; keep `append_capped` memory bounding. Adjust the `next_cursor`/`total` line (`:88-91`) wording to reflect the now-unknown total.
4. `move` (`:149-153`): `tokio::fs::symlink_metadata(dest)` (don't follow) → if it exists, return a typed "destination exists" error instead of renaming.
5. `append_capped` (`:102-108`): when a line exceeds `MAX_READ_LINE_BYTES`, append a `…[truncated]` marker on a UTF-8 boundary before pushing.
6. Keep command-injection safety intact — the `Command::new(prog).args(&[…])` + `--` + `./`-anchoring (`:174-192`) must stay; do not reintroduce a shell.

## Tests (colocated `#[cfg(test)]` in `src/tools/files/tests.rs`)
- Streamed cap: a command emitting > `MAX_SHELL_OUTPUT_BYTES` returns capped output with the marker; assert peak buffer ~cap, not the full stream.
- Timeout: a `sleep`-style command returns a timeout error within the deadline, child is reaped (no orphan).
- Recursive list depth bound honored.
- `read` early-stop: first page of a large temp file does not scan to EOF (assert via a sentinel/count), later pages behave.
- `move` onto an existing file errors and leaves both files intact; onto a free path still works.
- Over-long line carries the truncation marker.
- Gate: `bin/check`.

## Done when
- `file` list/grep/read on an arbitrarily large tree/file bounds both server memory and wall-clock; verified by tests.
- `move` never destroys an existing destination.
- Truncated lines are marked. `bin/check` green.
