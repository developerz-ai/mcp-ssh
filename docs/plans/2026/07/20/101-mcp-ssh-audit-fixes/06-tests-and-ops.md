# 06 — Test gaps + operational fixes

> Part of [`overview.md`](overview.md). Depends on: none (but the admin-kill test aligns with slice 02; the migration test aligns with slice 03 — land after those if run together).

The destructive paths that are uncovered, plus config/doc drift in the ops files.

## Files to change / add
- `src/admin.rs:135-146` (`kill_job` signalling branch) — HIGH test gap. The valid-pgid → `kill_group` → conditional `UPDATE ... WHERE status='running'` path is untested; only ghost / not-running / no-pgid branches are (`admin.rs:267-421`). The corrupt-pgid guard (`admin.rs:130`, `u32::try_from`) is also untested. This is the path that kills a real process group. Add a test: spawn a real short-lived group, persist its pgid to a temp DB, run the admin kill, assert the group is signalled and the row transitions under the `status='running'` guard; add a corrupt/negative pgid case asserting the guard rejects it without signalling.
- `src/config.rs:160-172` (`db_path()` / `db_path_in`) + `src/admin.rs:12` (`open_db`) — MEDIUM test gap. Admin-vs-server DB-path resolution is untested though the comment (`config.rs:157-159`) records a past bug where the CLI "killed in a different database than the server." Drive `db_path_in` through its `EnvSource` seam: assert env-set path, config-file path, and default all resolve to the same location admin and serve use. Regression test for the drift.
- `src/db.rs:47-59` (`Db::open` real path) — MEDIUM test gap. Only `Db::memory()` is exercised (skips WAL pragmas + `ensure_column` migration). Add a test opening a real on-disk temp DB, including a **legacy** table missing `pgid`/`title`/`output_tail`, asserting the forward-migration adds them and a subsequent insert works. (Shared with slice 03 step 4 — do once.)
- `Dockerfile:34` + `.env.example:15` vs `src/config.rs:103` — MEDIUM ops drift. Code default job dir is `/var/lib/mcp-ssh/logs/jobs`; `.env.example` sets `MCP_SSH_JOB_DIR=/var/lib/mcp-ssh/jobs` and the Dockerfile pre-creates+chowns `/var/lib/mcp-ssh/jobs` (dead — Docker doesn't set the env var, so runtime uses `logs/jobs`). Align all three on `/var/lib/mcp-ssh/logs/jobs` (fix `.env.example` value + Dockerfile mkdir/chown path). No code change needed — just make the ops files tell the truth.
- **Optional `/healthz` (LOW).** README/deploy probe liveness via the public `/.well-known/oauth-authorization-server`. Add a trivial unauthenticated `GET /healthz` → `200 "ok"` in `src/app.rs` router (outside the `/mcp` auth nest) if a proper probe is wanted. Confirm before adding — it's a new public route; keep it body-free and leak-free.

## Steps
1. Admin kill signalling test (`admin.rs` `#[cfg(test)]`): real group + temp DB, assert signal + guarded status transition + corrupt-pgid rejection.
2. db-path resolution regression test (`config.rs` `#[cfg(test)]`) via `db_path_in` + `EnvSource`.
3. Legacy-DB `Db::open` migration test (`db.rs` `#[cfg(test)]`) — coordinate with slice 03.
4. Fix `.env.example:15` + `Dockerfile:34` to `/var/lib/mcp-ssh/logs/jobs`.
5. Optional `/healthz` route + a test hitting it unauthenticated (if approved).

## Tests
- The three new `#[cfg(test)]` tests above.
- If `/healthz` added: an integration-style test (booted router) asserting `200` without auth and that `/mcp` still requires auth.
- Gate: `bin/check`.

## Done when
- The admin-kill signalling path and db-path resolution have regression tests; the legacy-DB migration is proven.
- Dockerfile, `.env.example`, and `config.rs` agree on the job dir.
- (If approved) `/healthz` responds `200` unauthenticated without weakening `/mcp` auth. `bin/check` green.
