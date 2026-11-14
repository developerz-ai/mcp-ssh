# 01 — Bounded output (memory, disk, context)

> Part of [`overview.md`](overview.md). Depends on: none.

The product promise is "bounded output, no context blowups". Five paths break it.

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| B1 | HIGH | Every log read loads the whole file: `fs::read` → `from_utf8_lossy` → `Vec<&str>` of all lines. Hit by `poll`, by the inline return, and by `finish` on **every** job exit (no poll needed) | [E2E] poll on a ~80 GB live log: request >30 s, RSS 50 332 MiB, earlyoom SIGTERM/SIGKILL |
| B2 | HIGH | Running job's log has no size cap (reaper skips running jobs; trim counts lines only). Shares the volume with the SQLite DB | [E2E] 150 GB in ~60 s |
| B3 | HIGH | Saved output tail bounded by 500 **lines**, not bytes; written under the DB mutex; a near-1 GB line fails the UPDATE and the row stays `running` | [READ] |
| B4 | HIGH | `file read` `limit` has no upper clamp → whole file in memory | [READ] |
| B5 | HIGH | `file read` on a non-regular file (`/dev/zero`, FIFO) never returns; task keeps a core at 100 % after the client disconnects; no deadline | [E2E] |
| B6 | MED | `job(action="list")`: no LIMIT, `cursor`/`limit` ignored, every inline `bash` call leaves a row for 24h; ordered by `id` (title, then `HH-MM-SS` — wrong across titles/midnight) | [E2E] `limit=2` returned all rows |
| B7 | LOW | `clamp_line` keeps the head of an over-long line → `\r` progress output shows its first 8 KB forever | [READ] |
| B8 | LOW | Newest-first cursor is relative to a moving end → paging back on a live job re-serves lines | [READ] |
| B9 | LOW | `file read` long-line marker: 64 KiB seal marker is always cut by the 8 KiB clamp; dropped-byte count wrong | [READ] |

## Files to change
- `src/jobs/log.rs:74,90,100-110,152-181` — replace whole-file reads with bounded reads; byte-cap `tail`; `clamp_line` (`:28-37`).
- `src/jobs/store.rs:135-153` — `finish`: byte-capped tail; status write must land even if the tail write fails.
- `src/jobs/store.rs:182-200` — `listing`: `ORDER BY started_unix DESC`, LIMIT/OFFSET.
- `src/jobs/mod.rs:246,337,448-458` — live log cap enforcement; inline return path; `list` takes cursor/limit.
- `src/jobs/reaper.rs:30-31,98,207` — byte bounds on trim; trim reads bounded.
- `src/tools/files.rs:89-150,185-197` — clamp `limit`, reject non-regular files, scan deadline, single long-line marker.
- `src/tools/mod.rs:128-131` — `job list` passes `cursor`/`limit`, renders a page footer.
- `src/tools/args.rs` — `JobArgs.cursor/limit` doc: applies to `list` too.

## Steps
1. **Tail reader.** In `log.rs` add a reverse reader: seek to EOF, read fixed chunks (64 KiB) backwards until `cursor+limit` newlines or a byte budget (`MAX_PAGE_BYTES`) is reached. `read_page_tail` and `tail` use it. Never allocate proportional to file size. Follow `append_capped` (`src/tools/files.rs`) for per-line capping.
2. **Total line count.** A reverse reader can't give `M` in `[N of M lines shown]` without a full scan. Either count with a streaming byte scan capped by a deadline (report `≥M` past it, as `file read` already does) or drop `M`. Pick one; update `render` in `src/tools/mod.rs:116-121,275-279`.
3. **Byte-cap the saved tail** (B3): 500 lines AND ≤ 256 KiB total, each line through `clamp_line`. Split `finish` so status/exit code are written first, tail second.
4. **Live log cap** (B2): new const (default 256 MiB; no config knob unless asked). The fd is `O_APPEND`, so use a watcher in the job's waiter task: on exceed → kill the group via slice 02's kill loop and finish as `Failed { "output exceeded <cap>" }`. Simplest correct option; rotation is not needed.
5. **`file read`** (B4, B5): `meta.is_file()` else typed `FileError::NotRegularFile`; clamp `limit` to a max page (same order as `DEFAULT_PAGE`); stop filling when accumulated bytes ≥ `MAX_PAGE_BYTES`; wrap the scan in the same 30 s deadline the shelled ops use. Render the new error in `render_file_error`.
6. **`job list` pagination** (B6): newest-first by `started_unix`, default page = existing `DEFAULT_PAGE`, footer names the next cursor. Decide whether inline-finished jobs belong in the default view — recommend: show them (history is useful) but paginate.
7. **B7/B8/B9:** keep the *tail* of a clamped line in the newest-first reader and split on `\r`; return an absolute anchor (byte offset of the first page's end) so later pages don't shift; one long-line marker with a correct dropped count.

## Tests
- `log.rs`: sparse 2 GiB file (`set_len`) with a short real tail — `read_page_tail`/`tail` return promptly; assert bytes read ≤ budget (wrap the reader, count reads). Empty file, no trailing newline, non-UTF-8, single 5 MiB line, cursor past total, `limit=0`.
- `store.rs`: `finish` with a 5 MiB single-line log → `length(output_tail)` ≤ cap, status `exited`.
- `jobs/mod.rs`: job writing past a (test-shrunk) cap is killed and reads `failed`; log size ≤ cap + one chunk.
- `files/tests.rs`: `timeout(5s, read("/dev/zero", 0, 10))` → typed error; 50 MiB file with `limit=usize::MAX` → early stop + `next_cursor`.
- `tools/mod.rs`: 5 000 planted rows → `job list` output ≤ `MAX_PAGE_BYTES`, chronological, cursor hint present.
- Never use `yes`/unbounded writers in tests. Gate: `bin/check`.

## Done when
- No `fs::read`/`read_to_string` of a job log remains in `src/jobs/`.
- Polling a multi-GB live log returns in <1 s with flat RSS.
- A runaway job is stopped at the cap and says why.
- `job list` is paginated and ordered by start time.
