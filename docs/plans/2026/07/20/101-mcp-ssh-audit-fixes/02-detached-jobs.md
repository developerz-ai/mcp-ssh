# 02 — Detached jobs surviving a restart + kill/signal correctness

> Part of [`overview.md`](overview.md). Depends on: none (independent of 01).

Jobs spawn as process-group leaders (`command.process_group(0)`, `src/jobs/mod.rs:326`). Outside a systemd `KillMode=control-group` shutdown, those groups reparent to init and keep running. The code assumes they die — so a survivor's row lies, is unkillable, and can have its live log trimmed out from under it. Plus three smaller signalling bugs.

## Files to change
- `src/jobs/mod.rs:207-246` (startup reconcile) — MEDIUM. Flips *every* `running` row to `failed` (`error='server restarted'`). For a survived group the row now falsely reports `failed`. Probe the persisted `pgid` with a liveness check (`kill -0` via the existing `group_alive`, `src/jobs/reaper.rs:98`) and only mark `failed` when the group is actually gone; otherwise leave it `running`.
- `src/jobs/mod.rs:542-547` (`JobStore::kill`) — MEDIUM. Only consults the in-memory map → a job that outlived a restart returns `false` ("no such job") from `job(action="kill")`, though its `pgid` is persisted and `kill_group` (`reaper.rs:70`) exists. Fall back to the DB `pgid` (via `poll_persisted`/a repo lookup) and `kill_group` when the id isn't tracked in-memory.
- `src/jobs/reaper.rs:198-219` (`compact_once`) — MEDIUM (race). Treats any non-`running` row as compactable; after the reconcile above wrongly flips a live job to `failed`, `compact_once` calls `trim_log` (`:322-324`, write-temp + rename) on a log the live writer still holds an O_APPEND fd to — rename unlinks the inode, stranding all further output. Gate trimming on real liveness (`group_alive` on the persisted pgid), not just DB status. (Fixing the reconcile above removes the trigger, but gate here too as defense-in-depth.)
- `src/jobs/mod.rs:290-297` (`run`, inside the `self.jobs` guard) — LOW/MEDIUM. `Path::exists()` (`:291`) and `std::fs::File::create` (`:297`) are blocking syscalls on the async worker, under the lock. Reserve the id in-map first, `drop(jobs)`, then create the file outside the critical section (or `spawn_blocking`). Keep the id-collision check correct (the disk-existence re-check at `:429-451` still guards reuse).
- `src/jobs/reaper.rs:51-59` (`kill_job`) — LOW. If the job exits between the `Running` check (`:45`) and the TERM signal, `kill -TERM` fails and returns `false`; `JobStore::kill` surfaces that as "no such job" for a job that just completed. On TERM-delivery failure, re-check final state (as the KILL branch at `:59` already does) and report success/finished.
- `src/jobs/reaper.rs:110-124` (`signal_group`) — LOW (cosmetic). Doesn't null stdout/stderr (unlike `group_alive` `:98-99`), so signalling a gone group writes "No such process" to the journal. Add `.stdout(null()).stderr(null())`.

## Steps
1. Factor a `group_alive(pgid)` liveness check usable from both `reaper.rs` and the reconcile in `mod.rs` (it already exists at `reaper.rs:98` — expose/reuse it; slice 05 will move signalling into `jobs/signal.rs`, keep this call-site stable).
2. Reconcile (`mod.rs:207-246`): for each `running` row with a persisted `pgid`, mark `failed` only if `!group_alive(pgid)`. Comment the pgid-recycling caveat (24h retention window makes it negligible) and that systemd `KillMode=control-group` shutdown still yields dead groups → correctly `failed`.
3. `JobStore::kill` (`mod.rs:542-547`): if the id is absent from the in-memory map, look up the persisted row; if `running` with a `pgid`, `kill_group(pgid)` and mark `failed`/`killed` under the `status='running'` guard. Return a truthful result.
4. `compact_once` (`reaper.rs:198-219`): before `trim_log`, skip rows whose persisted pgid is still `group_alive`.
5. `run` (`mod.rs:284-340`): move the log-file `create` and existence stat out of the async-mutex critical section.
6. `kill_job` TERM branch + `signal_group` stderr null — small, local.

## Tests (colocated `#[cfg(test)]` in `src/jobs/`)
- Reconcile: a `running` row with a **live** pgid (spawn a real short sleeper, persist its pgid) stays `running`; a row with a dead/absent pgid flips to `failed`. Existing reconcile tests (`mod.rs:1117,1143`) must still pass.
- `JobStore::kill` on an id present only in the DB (not in-memory) with a live pgid actually signals the group and marks it.
- `compact_once` does not trim a log whose pgid is still alive.
- `kill_job` returns success (not "no such job") when the process exits during the TERM window.
- Gate: `bin/check`.

## Done when
- A background job whose group survives a restart is reported `running`, killable via `job(action="kill")`, and its live log is never trimmed while alive.
- The systemd-normal (group dies on stop) path still reconciles to `failed` correctly.
- No blocking syscall remains under the `self.jobs` guard. `bin/check` green.
