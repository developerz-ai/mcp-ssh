# 02 — Kill semantics + job lifecycle

> Part of [`overview.md`](overview.md). Depends on: 01 (uses the kill loop for the log cap; same files — run after, not alongside).

CLAUDE.md claims "the one kill semantics". There are two, and both can report a kill that didn't happen.

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| K1 | HIGH | Signalling shells out to an external `kill`; a failed spawn or EPERM maps to "group gone" (`unwrap_or(false)`). Consequences: `kill_job` → "killed" with nothing signalled; reconcile fails live survivors; reaper evicts a running job. The published Docker image has **no `kill` binary** (`debian:bookworm-slim`, no procps) → permanent state there | [E2E] image check, [READ] `src/jobs/signal.rs:144-177` |
| K2 | MED-HIGH | `interactive=true` job: `bash -ic` ignores SIGTERM, so `job kill` on `sleep 600; ./deploy.sh` kills the sleep and bash **runs the next command** during the 2 s grace | [E2E by audit agent] `signal.rs:32-41,124-131` |
| K3 | MED | `kill_job` waits on the leader only (`exited_within(job.done)`, `signal.rs:41`); TERM-ignoring descendants survive a kill reported successful. `kill_group` (`:126-136`) probes the group — two semantics | [E2E by audit agent] |
| K4 | MED | Leader exited ⇒ job "finished": `bash("./server &")` returns `exited 0`, then `job kill` says "no such job" while the server runs; at 24h the reaper unlinks the log under the writer | [READ] `signal.rs:26,95`, `reaper.rs:265,279` |
| K5 | MED | Restart survivor is reconciled only at startup; its row reads `running` up to 24h after it exits; at 24h the reaper's `_ =>` arm deletes row+log **without killing** (CLAUDE.md says it kills first) | [READ] `reaper.rs:254-266`, `store.rs:333` |
| K6 | MED | Request cancelled between `command.spawn()` and the waiter `tokio::spawn` (awaits at `mod.rs:283,290`) → phantom `Running` job, never finished | [READ]; cancellation point [PLAUS] |
| K7 | MED | `insert_running` failure only warned → job with no row: invisible to `list`, never evicted, `finish` updates 0 rows; on PK conflict `finish` overwrites another job's row | [READ] `mod.rs:290-292` |
| K8 | MED | Kill outcomes collapse to `bool` → tool renders finished / survived / no-pgid / DB error all as "no such job" | [E2E] 2nd kill of a killed job → "no such job" |
| K9 | LOW | `ProcessGroupId` admits `1` (→ `kill -- -1` = every process of the service user) and values > `i32::MAX` | [READ] `mod.rs:53-64` |
| K10 | LOW | pgid reuse after host reboot (or long server downtime) → `job kill`/`mcp-ssh job kill` may signal an unrelated group, as root via sudo | [PLAUS] |
| K11 | LOW | Death by signal reported as `exited, code -1` (OOM/segv/kill indistinguishable from exit 255) | [E2E] killed job polls as `{"status":"exited","code":-1}` |
| K12 | LOW | CLI kill reason `"killed via mcp-ssh kill"` is overwritten by the server waiter's `exited -1`; reaper removes logs even when the row DELETE failed | [READ] `store.rs:143-146`, `reaper.rs:273-280` |
| K13 | LOW | 24h reaper kills any running job and erases row+log silently; tool description doesn't say so | [READ] policy decision |
| K14 | LOW | Failed spawn (bad `cwd`): error is a bare `No such file or directory (os error 2)` and leaves an empty reserved `<id>.log` | [E2E] |

## Files to change
- `src/jobs/signal.rs` — replace `Command::new("kill")` with `libc::kill` (add `libc`, or `nix` with only the `signal` feature); tri-state probe; one shared escalation loop.
- `src/jobs/mod.rs:53-64` (`ProcessGroupId`), `:238-318` (spawn/waiter ordering, insert failure, cwd pre-check), `:304-306` (signal exit), `:462-499` (`kill` returns an outcome).
- `src/jobs/reaper.rs:243-281` — untracked running rows, eviction gate, delete-failure handling.
- `src/jobs/store.rs:333` (`started_before` returns status+pgid), `:143-146` (`finish` vs prior `failed`).
- `src/jobs/shell.rs:33-38` — interactive launch form if needed for K2.
- `src/tools/mod.rs:83-86,137-141` — render each kill outcome; name `cwd` in the spawn error.
- `src/admin.rs:64-67` — keeps rendering only; exit codes handled in slice 03.
- `src/db.rs` — boot-id column via slice 03's migration framework (K10; do after 03 or as its first consumer).

## Steps
1. **`kill(2)` directly** (K1). `enum Probe { Alive, Gone, Unknown(io::Error) }` from errno: `0`→Alive, `ESRCH`→Gone, `EPERM`→Alive (exists, not ours), else Unknown. **Never** map an error to Gone. `unsafe` stays in one small function with a `// SAFETY:` note. Callers: `Unknown` ⇒ keep the row `running`, don't evict, kill returns a `NotSignalled`-style outcome.
2. **One escalation loop** (K3): `kill_job` and `kill_persisted` both call it; success = `group Gone`, not "leader's `done` fired". TERM → grace → KILL → re-probe.
3. **Interactive shells** (K2): make the leader unable to continue past the killed foreground child. Recommended: `SIGSTOP` the group, then `SIGKILL` the group when the job is interactive (TERM is pointless — bash ignores it); or `HUP`+`TERM` then KILL. Pick the one the regression test passes with.
4. **Finished leader, live group** (K4): `kill` on a non-running job probes the persisted pgid; Alive ⇒ run the loop, outcome `KilledLeftovers`. Reaper eviction and log unlink gate on `Gone` (kill first), same as compaction's `group_still_writing` gate.
5. **Survivors** (K5): each reaper pass re-runs the liveness reconcile for untracked `running` rows (`Gone` ⇒ `failed`), and routes any still-`Alive` untracked row past retention through `kill_persisted` before deleting it.
6. **Spawn ordering** (K6, K7): `tokio::spawn` the waiter immediately after `command.spawn()`; do the map insert + `insert_running` inside it before `wait()`. Insert failure ⇒ kill the child, return a typed error. `finish` checks rows-affected.
7. **Typed outcome** (K8): `JobStore::kill` returns `KillOutcome` (extend the existing enum: `Killed`, `AlreadyFinished`, `Survived`, `NotSignalled`, `NoSuchJob`); wording in `src/tools/mod.rs` and `src/admin.rs` only.
8. **pgid validity** (K9, K10): accept `2..=i32::MAX`. Persist the kernel boot id (`/proc/sys/kernel/random/boot_id`) with the row; mismatch ⇒ treat as `Gone` without probing.
9. **Signal exits** (K11): `ExitStatusExt::signal()` → `Failed { "killed by signal N" }`. Decide K12's final state in one place: a prior `failed` reason survives a signal exit.
10. **K13 policy** (owner decision): recommend age-out from *finish* time, never kill a running job for age; if kept, leave a `failed: reaped after 24h` row and say so in the `bash` tool description.
11. **K14:** validate `cwd` is a directory before `reserve_log`; remove the reserved log on spawn error; error names `cwd`.

## Tests
- `signal.rs`: errno → `Probe` mapping; `kill_persisted` on pgid 1 / `i32::MAX+1` refused (extend `corrupt_persisted_pgids_never_become_a_signal_target`); EPERM target (pid 1's group as non-root) is `Alive`, never `Killed`.
- Interactive: job `sleep 30; echo AFTER` with `interactive=true`, kill → `AFTER` never in the log.
- Descendants: `(trap '' TERM; sleep 300) & wait` → after `kill`, the descendant pid is gone.
- Finished leader: `sleep 300 & echo $!` → `kill` returns `KilledLeftovers`, pid dead.
- Reaper: planted `running` row + live group → group dead or row kept; dead pgid → `failed` after one pass; failed DELETE keeps the log.
- Cancellation: drop the `run` future (`select!` against a blocked DB) → job still reaches a terminal state.
- Tool adapter: one test per outcome wording (none exists today — nothing calls `Tools::job` for poll/kill).
- Every test kills what it spawns. Gate: `bin/check`.

## Done when
- `grep -rn 'Command::new("kill")' src/` is empty; kills work in the Docker image (covered by slice 07's image smoke test).
- No path reports `Killed` unless the group is `Gone`.
- `job kill` on a finished/unknown/unsignallable job says which.
- CLAUDE.md's reaper/kill claims are true again (text updated in slice 08).
