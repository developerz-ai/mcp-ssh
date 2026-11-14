# CLAUDE.md

`mcp-ssh` — a single Rust binary. An MCP server giving an AI agent remote shell + file access to **one** host (the box it runs on), over authenticated MCP Streamable HTTP at `/mcp`. Executes commands **locally** as the service user. Runs as a systemd service. "ssh, but over `/mcp` from any MCP client."

## NEVER

- Log or return the password / token — not in responses, errors, or logs.
- Run as root. Dedicated service user, full stop.
- Serve without TLS. Always behind a TLS-terminating reverse proxy.
- Ship without `MCP_SSH_ALLOWED_HOSTS` set.
- Weaken or bypass the auth middleware.
- Add more MCP tools to dodge a param. Parametrize the existing surface.
- Force-push `main`.
- `--no-verify` on commits — fix the hook.

## Response Rules

- Execute, no preamble, no restating the task. Lead with action or answer; reasoning after, only if non-obvious.
- Parallel tool calls when independent. Read before speculating.
- Disagree when the user is wrong — state the correction.
- Terse; fragments OK. Code/commands/paths verbatim; only prose compresses.
- End-of-turn summary: 1–2 sentences.

## Stack

| Concern | Choice |
|---|---|
| Lang | Rust 2024, toolchain pinned to 1.100.0 (`apps/mcp-ssh/rust-toolchain.toml` = `apps/mcp-ssh/Cargo.toml` `rust-version` = apps/mcp-ssh/Dockerfile base; bump together) |
| Runtime | tokio |
| HTTP | axum 0.8 |
| MCP | rmcp 3.9 (Streamable HTTP server transport) |
| Durable state | SQLite via `rusqlite` (`bundled` — compiled in, no system libsqlite). OAuth tokens (hashed) + clients + job metadata durable across restarts; schema versioned by `PRAGMA user_version` |
| Errors | thiserror (domain) + anyhow (main boundary) |
| Logging | tracing |
| Auth | OAuth 2.1 (MCP spec) only: `/mcp` is bearer-only; the operator logs in on the `/authorize` consent page. No HTTP Basic anywhere |
| TLS | reverse proxy — **not** in the binary |
| CLI | `mcp-ssh serve`, `mcp-ssh set-auth <user>` (revokes every session), `mcp-ssh jobs [--all]`, `mcp-ssh job kill <id>`, `mcp-ssh sessions [--revoke]` |

Deps pinned to latest stable at implementation time.

## Commands

| Task | Command |
|---|---|
| First-time setup (`cargo build --locked`; seeds `apps/mcp-ssh/.env` from `apps/mcp-ssh/.env.example` if absent; activates the pre-commit hook) | `bin/setup` |
| Full gate (fmt --check + clippy --locked --all-targets -D warnings + cargo test --locked; unit + in-process tests; `tests/` (e2e, dev_scripts) stays at the root pending its own move and is not compiled yet) | `bin/check` |
| Run locally (watch + reload, loads `.env`, state in `apps/mcp-ssh/.local/`) | `bin/dev` |
| Stop `bin/dev` (targets the pid in `apps/mcp-ssh/.local/dev.pid` and its children only, args-checked — never a name-based hunt for `mcp-ssh serve`) | `bin/stop` |
| End-to-end suite only (real binary over HTTP, `--test-threads=1`) | `bin/e2e` |
| Mint a bearer for a headless client (OAuth PKCE, creds from `.env`) | `bin/mcp-token` |
| Build / test / single test / format + lint | `cargo build` / `cargo test` / `cargo test <pattern>` / `cargo fmt && cargo clippy` |
| Run the binary | `cargo run -- serve` |

`.githooks/pre-commit` (activated by `bin/setup`) runs `bin/check` on every commit.
Secrets and local config: copy `apps/mcp-ssh/.env.example` → `apps/mcp-ssh/.env`; `.env` is gitignored.

## Module map

Keep this accurate — it's the navigation aid. Paths below are relative to the app root (`apps/mcp-ssh/`), not the repo root. One row per non-test file (`tests/module_map.rs` enforces it); extracted tests (`tests.rs`, `*_tests.rs`) need no row.

| Module | Owns |
|---|---|
| `src/main.rs` | entry: CLI parse, Sentry + tracing init, config load, serve with a 10s graceful drain (`DRAIN_DEADLINE`); dispatches `set-auth` (writes creds, then revokes every session) + the admin subcommands. Tests in `src/main_tests.rs` |
| `src/cli.rs` | clap command definitions (`serve`/`set-auth`/`jobs`/`job kill`/`sessions [--revoke]`) |
| `src/app.rs` | builds the axum router: `/mcp` (rmcp `StreamableHttpService` behind `auth::require_auth`) + the OAuth routes, both behind the `Host` allowlist guard; `/healthz` outside it; the shutdown token ending every SSE stream. In-process router tests in `src/app_tests.rs` |
| `src/agent.rs` | `Agent`: the OAuth client behind a request or job — `client_id` + `ClientName` (the DCR `client_name`, untrusted: control/invisible chars stripped, whitespace collapsed, ≤32 chars), shown as `label()` = `<name>#<4-hex of SHA-256(client_id)>`; `Debug` shows only the label. Tests in `src/agent/tests.rs` |
| `src/admin.rs` | local admin subcommands (`jobs` with a `BY` owner column / `job kill` / `sessions [--revoke]` + last activity per logged-in agent): opens the existing DB (`Db::open_existing` — no create, no migration) through `jobs::JobRepo` / `oauth` — never builds a `JobStore` (that would start a reaper + reconcile and fail the live server's running rows); `job kill` only renders `jobs::kill_persisted`'s `KillOutcome`. Never prints token values |
| `src/config.rs` | env + TOML config; fails fast on missing/empty creds or a non-loopback bind without `MCP_SSH_ALLOWED_HOSTS`; clamps `inline_timeout` ≤60s; `Password` with redacted `Debug`; TOML parse errors scrubbed of the file's text; `db_path()` resolves the DB path alone for the cred-free admin commands |
| `src/config/set_auth.rs` | `set-auth`'s config write: atomic replace, 0600 from the first byte, owner adopted from the config dir |
| `src/db.rs` | SQLite connection (`rusqlite`, `bundled`): open/create (file 0600, WAL), `open_existing` for the admin CLI, one serialized connection driven via `spawn_blocking`, typed `DbError`; DB at `/var/lib/mcp-ssh/mcp-ssh.db`; queries live in the repositories (`src/jobs/store.rs`, `src/oauth/store.rs`) |
| `src/db/migrations.rs` | the schema (`jobs` incl. `interactive` + the owner's `client_id`/`client_name`, `clients` incl. `client_name`/`last_used_unix`, hashed `access_tokens`/`refresh_tokens`/`refresh_tombstones`) as ordered forward-only steps versioned by `PRAGMA user_version`, each in its own `BEGIN IMMEDIATE` transaction. Append only |
| `src/auth.rs` | bearer-only guard for `/mcp` (`require_auth`: puts the bearer's `Agent` in the request extensions; store outage → 503, not 401; a streaming response keeps re-checking its token — see `src/auth/revalidate.rs`) and `check_login`, the consent form's constant-time credential check. No HTTP Basic |
| `src/auth/revalidate.rs` | re-checks an open `/mcp` stream's token on an interval (`STREAM_REVALIDATE`, 30s), ending the body once it is revoked or expired; holds the token hash, never the bearer; a store outage keeps the stream |
| `src/oauth/mod.rs` | minimal OAuth 2.1 wiring: `AuthState` (creds, store, public URL, the stream re-check interval), the public routes (discovery, `/authorize` GET+POST, `/token`, `/register`) with a 64 KiB body limit |
| `src/oauth/metadata.rs` | RFC 8414 / RFC 9728 discovery documents; `base_url` from `MCP_SSH_PUBLIC_URL` else the allow-listed `Host` |
| `src/oauth/flow.rs` | `/authorize`: GET validates (registered `client_id` + exact `redirect_uri`, else `invalid_request`; PKCE S256) and renders the consent page — never a code; POST takes the single-use CSRF nonce + credentials, only then redirecting with a code. Flat delay on a failed login; redirect-URI rule |
| `src/oauth/flow/token.rs` | `/token`: `authorization_code` (code + PKCE verifier) and `refresh_token` grants; `no-store`; store outage → 503 |
| `src/oauth/flow/register.rs` | `/register`: Dynamic Client Registration (RFC 7591), bounded URIs per client, optional `client_name` cleaned by `ClientName::parse` before it is stored |
| `src/oauth/consent.rs` | the consent page HTML: static template, escaped values, `no-store`, CSP `frame-ancestors 'none'`, `X-Frame-Options: DENY` |
| `src/oauth/pending.rs` | `Pending<T>`: bounded, TTL'd, single-use in-memory map for consent requests and auth codes (10 min) |
| `src/oauth/secret.rs` | random 256-bit tokens, SHA-256 `hash_token` (how tokens are stored and looked up), PKCE S256 + verifier rules |
| `src/oauth/store.rs` | `Store`: the OAuth repository facade — lifetimes (access 24h, refresh 30d, family 90d, client grace 7d, ≤1000 clients), codes/consents, issue + validate; typed `GrantError`/`StoreUnavailable`/`RegisterError`. Only token hashes reach the DB |
| `src/oauth/store/pair.rs` | token pairs: mint + insert within a login family; refresh rotation with reuse detection (a replayed refresh token revokes its whole family via tombstones) |
| `src/oauth/store/clients.rs` | registered clients: persist `client_id`→redirect URIs + cleaned name (no secrets), cap, exact-match check for `/authorize` |
| `src/oauth/store/presence.rs` | who is on the host: `authenticate` (bearer hash → `Agent`, noting last use at most once a minute per client), `active_agents` (used a bearer within `PRESENCE_WINDOW`, 15 min), `logged_in_agents` (for `mcp-ssh sessions`). Never reads a token |
| `src/oauth/store/sweep.rs` | `revoke_all` (`sessions --revoke`, `set-auth`) and the reaper's `sweep_expired_access`/`sweep_expired_refresh`/`sweep_stale_clients` — counts only, never a token value |
| `src/jobs/mod.rs` | job engine facade: `JobStore` wiring (`new` spawns the reaper), public types (`JobState`, `JobMeta` — title + owning `Agent`, `RunResult`, `JobSummary`, `JobList`, `JobError`), `list` (paged, ≤200), `owner` and `kill` |
| `src/jobs/run.rs` | launching: reserve id + 0600 log (`create_new`), spawn in its own process group (`sh -c` or `bash -ic`), hand to the supervisor, wait out the inline window |
| `src/jobs/supervise.rs` | the supervisor task: owns a job from spawn to its recorded final state (survives request cancellation), enforces the 256 MiB live log cap (`LOG_CAP`; `SIGKILL` past it) — on leftovers too, until the group is gone — maps exit/signal to `JobState` |
| `src/jobs/poll.rs` | poll: status + one newest-first page from the live log, else the tail saved in the row |
| `src/jobs/id.rs` | `JobId` newtype: `<title>-HH-MM-SS` (normalized agent `title`, else `job`; `-<seq>` on collision); free of command text so secrets can't leak into an id, log line, or filename |
| `src/jobs/log.rs` | bounded backward log reads: `read_tail`/`tail_of_text` → `TailPage { lines, older }` (byte-offset cursor), under line/byte/scan budgets; long lines clamped to 8 KiB |
| `src/jobs/pgid.rs` | `ProcessGroupId`: holds only `2..=i32::MAX`, the only way a group id reaches a signal |
| `src/jobs/signal.rs` | process-group signalling: `kill(2)` via `libc`, errno tri-state `Probe` (`Alive` incl. `EPERM` / `Gone` = `ESRCH` / `Unknown`), boot-id gate on persisted pgids, `escalate` (TERM → 2s → KILL, optional leader `SIGSTOP`) |
| `src/jobs/signal/kill.rs` | **the one kill semantics**: `kill_job` (live handle) / `kill_persisted` (a row's pgid — running-check, corrupt-pgid gate, `failed` transition) → `KillOutcome`, which the engine and `mcp-ssh job kill` only render |
| `src/jobs/reaper.rs` | reaper (startup + hourly), **never kills**: reconcile untracked `running` rows (→ `failed` only when the group probes gone), age out finished jobs 24h after they *finished* (running never; leftovers keep the row), orphan log files by mtime, OAuth sweeps on the same pass |
| `src/jobs/reaper/compact.rs` | log compaction: trim finished jobs' logs to their last 5000 lines (<3h since finish) / 500 after, ≤4 MiB, idempotent marker |
| `src/jobs/shell.rs` | `Shell`: how a user command is launched — bare `sh -c` (default) vs. interactive `bash -ic` (sources `~/.bashrc` for aliases/version managers) |
| `src/jobs/status.rs` | `JobStatus`: the bare `jobs.status` word (`running`/`exited`/`failed`); the one place the on-disk strings are spelled |
| `src/jobs/store.rs` | `JobRepo`: the `jobs` repository the engine, reaper and admin CLI call (insert with the owner, finish with the saved tail — 500 lines, ≤256 KiB — find, owner, paged summaries, listing). No `cmd` text reaches a query or a log line |
| `src/jobs/store/row.rs` | row⇄domain mapping: typed rows, `JobState` onto `(status, code, error)` and back |
| `src/jobs/store/fail.rs` | recording `failed` for a job this writer may not own (kill, reconcile) — only while the row still reads `running`, or replacing a bare `killed by signal N` the engine recorded; plus re-recording a finished job `failed` when the supervisor killed its leftovers over the log cap |
| `src/jobs/store/reap.rs` | the reaper's and reconcile's queries: untracked `running` rows, compaction/eviction candidates, deleting aged-out rows |
| `src/tools/mod.rs` | MCP tool surface (`#[tool_router]`/`#[tool]` from rmcp): 3 tools (`bash`/`job`/`file`) dispatching on `action`, their descriptions, one tracing span per dispatch (with the caller's `agent` label). Thin adapters over jobs + files |
| `src/tools/caller.rs` | the calling `Agent` (from the HTTP parts rmcp hands each tool), its span label, and `foreign_owner` — whose job `id` is, when not the caller's |
| `src/tools/args.rs` | arg structs for the 3 tools (`BashArgs`/`JobArgs`/`FileArgs`, all `deny_unknown_fields`; `JobAction`/`FileAction`) + `lenient_action` (non-string/missing `action` → `list`, unknown string → error) — schema shape only |
| `src/tools/render.rs` | agent-facing wording for `file`: `render_file`/`render_file_error` turn `FileOutcome`/`FileError` into sentences, markers and next-step hints |
| `src/tools/reject.rs` | the operator's log line for a `tools/call` refused before dispatch (args `Parameters<T>` rejects, or an unknown tool): tool + fixed reason word + `request_id` — never a key, value or serde message |
| `src/tools/job_render.rs` | agent-facing wording for `bash`/`job`: run/poll replies + the newest-first paging footer, list page (presence line, `by` per job) + cursor, each `KillOutcome`, the `[started by another agent: …]` note, `JobError` |
| `src/tools/files.rs` | file operations' shared types (`FileOutcome`, typed `FileError` — a failed shell op keeps its `ShError`) + `list`/`grep` (shelled-out `ls`/`find -maxdepth 20`/`grep`). Returns facts, never a sentence |
| `src/tools/files/read.rs` | `read`: regular files only, forward by line, ≤1000 lines / 64 KiB a page, 8 KiB line clamp, 30s deadline |
| `src/tools/files/write.rs` | `write` (atomic temp sibling + rename, keeps mode, writes through a symlink) and `append` |
| `src/tools/files/remove.rs` | `delete`: never follows a symlink (even `link/`); a non-empty directory only with `recursive=true` |
| `src/tools/files/rename.rs` | `move`: never overwrites, no stray dirs from a doomed move, cross-device copy for regular files only |
| `src/tools/files/shell.rs` | bounded runner behind `ls`/`find`/`grep`: streams combined output under a byte cap + 30s wall-clock deadline, killing the child on either |
| `src/sentry.rs` | GlitchTip (Sentry-protocol) error tracking: init + the fail-closed `before_send` credential scrubber. Unset DSN ⇒ no-op |

Files ≤300 **non-test** LOC; a module's tests go in a sibling `tests.rs` (`#[cfg(test)] mod tests;`, or `#[path]` for a top-level file) rather than counting against it. One responsibility per module — split when it grows a second reason to change.

## Execution model

- `bash` finishing within `MCP_SSH_INLINE_TIMEOUT_SECS` (default 2, clamped ≤60; per-call `timeout` clamped ≤600) returns its output inline; slower, or `bg=true`, auto-backgrounds to a **job id** whose live output streams to a per-job log file (0600).
- `job(action="poll")` pages that log **newest-first**: no cursor = the latest page (≤`limit` lines, default 200, ≤64 KiB); the footer `[latest N lines shown; earlier output: job(action="poll", id="…", cursor=C)]` names the older page, where `C` is the **byte offset** that page starts at (absolute, so a growing log never re-serves lines). Reads walk backwards under a line/byte/scan budget — never the whole log.
- Live log gone (a finished job after a restart) → poll falls back to the output tail saved in SQLite (500 lines, ≤256 KiB). Bounded output is the whole point: no context blowups, no server memory blowups.
- A live log is capped at 256 MiB (const) while its process group holds a process — the leader, or what it left running (`server &`); past it the group is `SIGKILL`ed and the job recorded `failed`, even one that had already `exited`. The cap is the supervisor's and ends with the server process: a group that outlives a restart is not watched.
- **Hybrid persistence:** structured state in SQLite (`src/db.rs`) — OAuth tokens + job metadata + output tail, durable across restarts; the append-heavy live output stays a per-job log file. SQLite only sees low-frequency writes (token issue/validate, job create/finish), so one serialized connection is ample.
- **Reaper** (startup + hourly) **never kills:** finished jobs (row + log) age out 24h after they *finished*; running jobs are never aged out; a finished job whose group still holds a process keeps its row until it exits; untracked `running` rows (left by a previous process) flip to `failed` only once their group probes gone.

### Kill semantics

One implementation, `src/jobs/signal/kill.rs` over `src/jobs/signal.rs`, behind both `job(action="kill")` and `mcp-ssh job kill`:

- Signals via `kill(2)` (`libc`) — never an external `kill` binary (the Docker image has none).
- Tri-state probe: delivered or `EPERM` = alive, `ESRCH` = gone, any other errno = **unknown** — never read as gone.
- Escalation: `SIGTERM`, 2s grace, `SIGKILL`, then probe; an interactive job's leader is `SIGSTOP`ped first — on both paths (the row's `interactive` column tells `kill_persisted`). A kill ending `Survived`/`NotSignalled` withdraws its reason, so a later exit is recorded as what it was. Success = the group probed gone, never "the leader exited".
- A persisted pgid outside `2..=i32::MAX` is refused; one recorded under another boot id is never signalled.
- Every path returns a `KillOutcome` — `Killed`, `KilledLeftovers`, `NotRunning`, `Unknown`, `NoProcessGroup`, `CorruptProcessGroup`, `Survived`, `NotSignalled` — that callers only render (`src/tools/job_render.rs`, `src/admin.rs`). `kill_persisted` writes `failed` only on `Killed`, and only while the row still reads `running` (a real exit recorded meanwhile wins) — except that a bare `killed by signal N` the engine recorded is replaced by the kill's reason, so the waiter/kill race can't decide whether the row says why.

## MCP tool design

**Constant, heavily-parametrized surface — 3 resource-oriented tools.** Group by resource; push composition into params (`action`, `cursor`, `limit`, `recursive`, `timeout`, `bg`, `interactive`) — do **NOT** add more tools; new capability = a new param or `action`, almost always.

Args are strict: `#[serde(deny_unknown_fields)]` on all three; a `job` `action` *string* naming no action is an error listing `poll|list|kill` (only a non-string placeholder or a missing `action` falls back to `list`). Agent-facing wording lives only in `src/tools/` (`render.rs`, `job_render.rs`, the tool descriptions in `mod.rs`).

Current tools (three, constant):

| Tool | Params | Does |
|---|---|---|
| `bash` | `cmd`, `cwd?`, `timeout?`, `bg?`, `interactive?`, `title?` | run a command; inline if fast, else a job id (`bg` backgrounds at once; `timeout` ≤600; `interactive` sources `~/.bashrc` via `bash -ic` for aliases/version managers, default fast `sh -c`; `title` labels the job id as `<title>-HH-MM-SS`) |
| `job` | `action?`, `id?`, `cursor?`, `limit?` | jobs by `action`: `poll` (newest-first page, byte cursor), `list` (default; presence line of agents active in 15 min, then newest first with `by` = `you`/agent label, ≤200 per page, offset cursor), `kill` (typed outcome). Another agent's job is pollable/killable; the reply names its owner |
| `file` | `action`, `path?`, `content?`, `pattern?`, `recursive?`, `src?`, `dest?`, `cursor?`, `limit?` | file ops by `action`: `read` (by line, ≤1000/64 KiB a page)/`write` (atomic)/`append`/`delete` (non-empty dir needs `recursive=true`; symlinks never followed)/`list`/`grep`/`move` (never overwrites) |

## Conventions

The bar: idiomatic, boring, readable Rust — SOLID, SRP, tested code; no spaghetti, no premature abstraction; a function reads top to bottom without chasing state; equally-correct options → the one easier to delete; `clippy -D warnings` is the floor, not the ceiling.

- Errors typed: `thiserror` for domain, `anyhow` only at the `main.rs` boundary. No `unwrap`/`expect` outside `main` and tests — panic in the request path crashes every client; propagate with `?`; branch with `match`/`if let`/`let ... else`.
- Newtype over bare primitives when a value has meaning (`JobId(String)`, not `String`); make illegal states unrepresentable — `enum` over contradictory `bool`+`Option`; validate input into a type once at the edge.
- Borrow by default (`&str` over `String`, `&[T]` over `Vec<T>`); `.clone()` only when ownership must move — non-obvious clone gets a one-line `// why`; `Arc<T>` for shared read-only; lock only when mutating shared state, critical section tiny.
- Functions do one thing — need "and" to describe it → split. Concrete first; a trait when the **second** impl arrives. No abstractions for single-use code; 200 lines that could be 50 → write 50.
- Async end-to-end: no `std::sync::Mutex` on the request path — use `tokio::sync`; never hold a `std::sync` guard across `.await`; no `block_on` — offload blocking I/O with `spawn_blocking`.
- `tracing`, not `println`: every tool dispatch is a span with `request_id`, tool name and the caller's `agent` label (never a `client_id`); a call refused before dispatch logs one `rejected` WARN (tool + reason word) instead.
- Derive, don't hand-roll (`Debug`, `Clone`, serde); every public type derives `Debug`; keep the `pub` surface minimal.
- Comment the non-obvious *why*, never the *what* — rename until the code doesn't need the *what*.
- Think before coding: state assumptions; uncertain → ask, don't guess; multiple interpretations → present them, don't pick silently; a simpler approach exists → say so.
- Surgical changes: touch only what the task requires; no drive-by refactors/reformatting; match existing style; every changed line traces to the request; pre-existing dead code — flag, don't delete.
- Goal-driven: bug → reproducing test first, then the fix; new param → test the new behavior first; refactor → tests green before AND after.

## Testing

- Unit: pure logic (config validation, job pagination, path handling) under `#[cfg(test)]` — in the file, or a sibling `tests.rs` once that keeps the file under the LOC limit.
- In-process: `src/app_tests.rs` drives the axum router (auth, host guard, OAuth, MCP JSON-RPC) with `tower::ServiceExt::oneshot`, no socket.
- End-to-end: `tests/e2e/` (`bin/e2e`) boots the real `mcp-ssh serve` binary per test on an ephemeral port with tempdir state, driven over HTTP — one scenario per top audit finding. `tests/` stays at the repo root pending its own move: until then `cargo test` compiles unit + in-process tests only.
- Public tree: `tests/public_tree.rs` scans every tracked file (minus `.dz/`) for facts about our own infrastructure; a failure names the file, never the value.
- Tested code is the default, not the exception. New behavior ships with the test that proves it. Never an unbounded writer (`yes`) in a test — use sparse files.

## Changelog

`CHANGELOG.md` is the index; each release's notes are `docs/changelog/<version>.md`. A pull request with a user-visible change adds its line to `docs/changelog/unreleased.md`.

## Context (not in code)

- One host, one service user. Targets the box it runs on; not a fan-out / multi-host tool.
- Many agents may share the host at once (one OAuth client each). They see each other (presence, job owners) but are not isolated from each other: awareness, not multi-tenancy.
- TLS, multi-host routing, and rate limiting live in the reverse proxy, not here.
- NOT building: an SSH client, a fleet orchestrator, a job scheduler, a secrets vault.

## Note

No git worktrees — work directly in this checkout. A task big enough for subagents runs them as a team in this same checkout, split into disjoint pieces so no two agents touch the same files.
