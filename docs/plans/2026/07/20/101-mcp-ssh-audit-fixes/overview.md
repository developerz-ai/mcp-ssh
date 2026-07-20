# mcp-ssh — Audit Fixes

## Goal
Close the correctness/DoS bugs, unbounded-growth gaps, test holes, and architecture debt surfaced by a 5-agent deep dive of the codebase. Priority: user-impacting bugs (server OOM/hang, jobs lying about their state) first, then hardening and refactors.

## Context
- Stack: Rust 2024, tokio, axum 0.8, rmcp 1.7 (Streamable HTTP MCP at `/mcp`), SQLite via `rusqlite` (bundled), OAuth 2.1 + HTTP Basic auth. Single binary, one host, one service user.
- Constant 3-tool MCP surface (`bash`/`job`/`file`) dispatched on `action` — **do not add tools**; new capability = new `action`/param.
- Reference patterns to follow:
  - Bounded, streamed reads: `src/tools/files.rs:26-96` (`read` — memory-bounded via `append_capped`). The shelled-out `ls`/`find`/`grep` path (`files.rs:221`) does **not** follow this — slice 01 fixes that.
  - A real persistence repository: `src/oauth/store.rs` (`oauth::Store`). The jobs side has no equivalent — slice 05 introduces one.
  - Process-group kill by persisted pgid: `src/jobs/reaper.rs:70` (`kill_group`) — already exists, reused in slices 02.
  - Forward migration: `src/db.rs:104-117` (`ensure_column`) — slice 03 extends its use.
- Security invariants (CLAUDE.md NEVER): never log/return password or token; never run as root; always behind TLS proxy; `MCP_SSH_ALLOWED_HOSTS` always set; never weaken auth. Job ids/log filenames derive only from the normalized `title`, never from `cmd` (`src/jobs/id.rs:69-83`) — keep it that way in every slice.

## Plan files (execute in order)
1. [`01-file-op-bounds.md`](01-file-op-bounds.md) — HIGH: stream+cap+timeout the shelled `ls`/`find`/`grep`; bound recursive list; early-stop `read`; stop `move` clobber; mark truncated lines.
2. [`02-detached-jobs.md`](02-detached-jobs.md) — MEDIUM: jobs whose process group survives a restart — probe pgid liveness in reconcile/compact, make `job(kill)` reach them; fix TERM-race false negative, blocking syscalls under the lock, stderr signal noise.
3. [`03-token-gc-and-db.md`](03-token-gc-and-db.md) — MEDIUM: sweep expired OAuth tokens (unbounded DB growth); wrap `issue()` in a transaction; drive post-v1 columns through `ensure_column`.
4. [`04-auth-hardening.md`](04-auth-hardening.md) — LOW/MEDIUM defense-in-depth: bind `redirect_uri` to registered client; reject empty https host; fail-fast on non-loopback bind without explicit allowed-hosts.
5. [`05-architecture-refactor.md`](05-architecture-refactor.md) — extract `JobRepo`, add `JobStatus` enum, split oversized modules (`jobs/mod.rs` 549 LOC, `reaper.rs` 394 LOC), type `FileError`, fix stale docs. Rebase onto the post-bugfix code.
6. [`06-tests-and-ops.md`](06-tests-and-ops.md) — cover the untested destructive paths (admin kill signalling, db-path resolution, legacy-DB migration); fix Dockerfile/`.env.example` job-dir drift; optional `/healthz`.

## Done when
- `file` list/grep/read on a huge tree can neither OOM nor hang the server (bounded memory + timeout + depth cap), proven by tests.
- A background job whose group survives a restart is reported truthfully and is killable via `job(action="kill")`; its live log is never trimmed out from under it.
- Expired access/refresh tokens are swept periodically — DB does not grow unboundedly per refresh.
- The destructive admin-kill signalling path and admin/server db-path resolution have regression tests.
- `bin/check` (fmt --check + clippy -D warnings + test) is green after every slice.
- Ops files agree on the job dir; CLAUDE.md module map + `auth.rs` header reflect reality.

## Risks / open questions
- **Slice ordering vs 05.** Bug-fix slices (01–04) touch `files.rs`, `jobs/mod.rs`, `reaper.rs`, `store.rs`. The refactor (05) moves that code into repositories/enums/new modules. Execute 05 last and rebase its moves onto the fixed code, or the fixes get lost. Each slice must leave `bin/check` green so 05 starts from a clean base.
- **Detached-job liveness (02).** `kill -0`/pgid probing assumes the persisted `pgid` hasn't been recycled by the OS to an unrelated process. Low risk over a 24h retention window; call it out in the reconcile comment. Under systemd with `KillMode=control-group` the groups *are* killed on stop, so the "survives restart" case is real only outside that config — fix must not regress the systemd-normal path.
- **Auth redirect binding (04).** Persisting client_id→redirect_uri at DCR changes the currently-stateless `register`. Confirm this is wanted vs. accepting the documented "public client, Basic-auth-gated" residual risk — it is defense-in-depth, not a live exploit. Lowest priority; can be dropped if scope tightens.
- **`read` early-stop (01).** Reporting `total` without a full-file scan means the `next_cursor` line becomes "≥ n / unknown" past the first page. Acceptable UX change; note it in the tool output wording.
