# CLAUDE.md

`mcp-ssh` — a single Rust binary. An MCP server giving an AI agent remote shell + file access to **one** host (the box it runs on), over authenticated MCP Streamable HTTP at `/mcp`. Executes commands **locally** as the service user. Runs as a systemd service. "ssh, but over `/mcp` from any MCP client."

## Response Rules

- Execute. No preamble. No "I'll start by…". No restating the task.
- Lead with action or answer. Reasoning after, only if non-obvious.
- Parallel tool calls when independent.
- Read before speculating.
- Disagree when user is wrong. State the correction.
- Terse. Fragments OK. Drop articles, filler, hedging.
- Code/commands/paths: verbatim. Only prose gets compressed.
- End-of-turn summary: 1–2 sentences. Nothing else.

## Stack

| Concern | Choice |
|---|---|
| Lang | Rust 2024, toolchain pinned to 1.100.0 (`rust-toolchain.toml` = `Cargo.toml` `rust-version` = Dockerfile base; bump together) |
| Runtime | tokio |
| HTTP | axum 0.8 |
| MCP | rmcp 2.2 (Streamable HTTP server transport) |
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
| Build | `cargo build` |
| Run locally (watch + reload, loads `.env`) | `bin/dev` |
| Full gate (fmt --check + clippy -D warnings + test, e2e included; all `--locked`) | `bin/check` |
| End-to-end suite only (real binary over HTTP) | `bin/e2e` |
| Test | `cargo test` |
| Single test by pattern | `cargo test <pattern>` |
| Format + lint | `cargo fmt && cargo clippy` |
| Run the binary | `cargo run -- serve` |
| Mint a bearer for a headless client (OAuth PKCE) | `bin/mcp-token` |

Secrets and local config: copy `.env.example` → `.env`. `.env` is gitignored.

## Module map

Keep this accurate — it's the navigation aid. One row per non-test file (`tests/module_map.rs` enforces it); extracted tests (`tests.rs`, `*_tests.rs`) need no row.

| Module | Owns |
|---|---|
| `src/main.rs` | entry: CLI parse, Sentry + tracing init, config load, serve with a 10s graceful drain (`DRAIN_DEADLINE`); dispatches `set-auth` (writes creds, then revokes every session) and the admin subcommands. Tests in `src/main_tests.rs` |
| `src/cli.rs` | clap command definitions (`serve`/`set-auth`/`jobs`/`job kill`/`sessions [--revoke]`) |
| `src/app.rs` | builds the axum router: `/mcp` (rmcp `StreamableHttpService` behind `auth::require_auth`) + the OAuth routes, both behind the `Host` allowlist guard; `/healthz` outside it; the shutdown token that ends every SSE stream. In-process router tests in `src/app_tests.rs` |
| `src/admin.rs` | local admin subcommands (`jobs`/`job kill`/`sessions [--revoke]`): open the existing DB (`Db::open_existing` — no create, no migration) through `jobs::JobRepo` / `oauth` — never builds a `JobStore` (that would start a reaper + reconcile and fail the live server's running rows); `job kill` only renders `jobs::kill_persisted`'s `KillOutcome`. Never prints token values |
| `src/config.rs` | env + TOML file config; fails fast on missing/empty creds or a non-loopback bind without `MCP_SSH_ALLOWED_HOSTS`; clamps `inline_timeout` ≤60s; `Password` with redacted `Debug`; TOML parse errors scrubbed of the file's text. `db_path()` resolves the DB path alone for the cred-free admin commands |
| `src/config/set_auth.rs` | `set-auth`'s config write: atomic replace, 0600 from the first byte, owner adopted from the config dir |
| `src/db.rs` | SQLite connection (`rusqlite`, `bundled`): open/create (file 0600, WAL), `open_existing` for the admin CLI, one serialized connection driven via `spawn_blocking`, typed `DbError`. DB at `/var/lib/mcp-ssh/mcp-ssh.db`. Queries live in the repositories (`src/jobs/store.rs`, `src/oauth/store.rs`) |
| `src/db/migrations.rs` | the schema (`jobs`, hashed `access_tokens`/`refresh_tokens`/`refresh_tombstones`, `clients`) as ordered forward-only steps versioned by `PRAGMA user_version`, each in its own `BEGIN IMMEDIATE` transaction. Append only |
| `src/auth.rs` | bearer-only guard for `/mcp` (`require_auth`; store outage → 503, not 401) and `check_login`, the constant-time credential check the consent form uses. No HTTP Basic |
| `src/oauth/mod.rs` | minimal OAuth 2.1 server wiring: `AuthState`, the public routes (discovery, `/authorize` GET+POST, `/token`, `/register`) with a 64 KiB body limit |
| `src/oauth/metadata.rs` | RFC 8414 / RFC 9728 discovery documents; `base_url` from `MCP_SSH_PUBLIC_URL` else the allow-listed `Host` |
| `src/oauth/flow.rs` | `/authorize`: GET validates (registered `client_id` + exact `redirect_uri`, else `invalid_request`; PKCE S256) and renders the consent page — never a code; POST takes the single-use CSRF nonce + credentials and only then redirects with a code. Flat delay on a failed login; redirect-URI rule |
| `src/oauth/flow/token.rs` | `/token`: `authorization_code` (code + PKCE verifier) and `refresh_token` grants; `no-store`; store outage → 503 |
| `src/oauth/flow/register.rs` | `/register`: Dynamic Client Registration (RFC 7591), bounded URIs per client |
| `src/oauth/consent.rs` | the consent page HTML: static template, escaped values, `no-store`, CSP `frame-ancestors 'none'`, `X-Frame-Options: DENY` |
| `src/oauth/pending.rs` | `Pending<T>`: bounded, TTL'd, single-use in-memory map for consent requests and auth codes (10 min) |
| `src/oauth/secret.rs` | random 256-bit tokens, SHA-256 `hash_token` (how tokens are stored and looked up), PKCE S256 + verifier rules |
| `src/oauth/store.rs` | `Store`: the OAuth repository facade — lifetimes (access 24h, refresh 30d, family 90d, client grace 7d, ≤1000 clients), codes/consents, issue + validate; typed `GrantError`/`StoreUnavailable`/`RegisterError`. Only token hashes reach the DB |
| `src/oauth/store/pair.rs` | token pairs: mint + insert within a login family; refresh rotation with reuse detection (a replayed refresh token revokes its whole family via tombstones) |
| `src/oauth/store/clients.rs` | registered clients: persist `client_id`→redirect URIs (no secrets), cap, exact-match check for `/authorize` |
| `src/oauth/store/sweep.rs` | `revoke_all` (`sessions --revoke`, `set-auth`) and the reaper's `sweep_expired_access`/`sweep_expired_refresh`/`sweep_stale_clients` — counts only, never a token value |
| `src/jobs/mod.rs` | job engine facade: `JobStore` wiring (`new` spawns the reaper), public types (`JobState`, `RunResult`, `JobSummary`, `JobList`, `JobError`), `list` (paged, ≤200) and `kill` |
| `src/jobs/run.rs` | launching: reserve id + 0600 log (`create_new`), spawn in its own process group (`sh -c` or `bash -ic`), hand to the supervisor, wait out the inline window |
| `src/jobs/supervise.rs` | the supervisor task: owns a job from spawn to its recorded final state (survives request cancellation), enforces the 256 MiB live log cap (`LOG_CAP`; `SIGKILL` past it), maps exit/signal to `JobState` |
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
| `src/jobs/store.rs` | `JobRepo`: the `jobs` repository the engine, reaper and admin CLI call (insert, finish with the saved tail — 500 lines, ≤256 KiB — find, paged summaries, listing). No `cmd` text reaches a query or a log line |
| `src/jobs/store/row.rs` | row⇄domain mapping: typed rows, `JobState` onto `(status, code, error)` and back |
| `src/jobs/store/fail.rs` | recording `failed` for a job this writer may not own (kill, reconcile) — only while the row still reads `running`, or replacing a bare `killed by signal N` the engine recorded |
| `src/jobs/store/reap.rs` | the reaper's and reconcile's queries: untracked `running` rows, compaction/eviction candidates, deleting aged-out rows |
| `src/tools/mod.rs` | MCP tool surface (`#[tool_router]`/`#[tool]` from rmcp): 3 tools (`bash`/`job`/`file`) dispatching on `action`, their descriptions, one tracing span per dispatch. Thin adapters over jobs + files |
| `src/tools/args.rs` | arg structs for the 3 tools (`BashArgs`/`JobArgs`/`FileArgs`, all `deny_unknown_fields`; `JobAction`/`FileAction`) + `lenient_action` (non-string/missing `action` → `list`, unknown string → error) — schema shape only |
| `src/tools/render.rs` | agent-facing wording for `file`: `render_file`/`render_file_error` turn `FileOutcome`/`FileError` into sentences, markers and next-step hints |
| `src/tools/job_render.rs` | agent-facing wording for `bash`/`job`: run/poll replies + the newest-first paging footer, list page + cursor, each `KillOutcome`, `JobError` |
| `src/tools/files.rs` | file operations' shared types (`FileOutcome`, typed `FileError` — a failed shell op keeps its `ShError`) + `list`/`grep` (shelled-out `ls`/`find -maxdepth 20`/`grep`). Returns facts, never a sentence |
| `src/tools/files/read.rs` | `read`: regular files only, forward by line, ≤1000 lines / 64 KiB a page, 8 KiB line clamp, 30s deadline |
| `src/tools/files/write.rs` | `write` (atomic temp sibling + rename, keeps mode, writes through a symlink) and `append` |
| `src/tools/files/remove.rs` | `delete`: never follows a symlink (even `link/`); a non-empty directory only with `recursive=true` |
| `src/tools/files/rename.rs` | `move`: never overwrites, no stray dirs from a doomed move, cross-device copy for regular files only |
| `src/tools/files/shell.rs` | bounded runner behind `ls`/`find`/`grep`: streams combined output under a byte cap + 30s wall-clock deadline, killing the child on either |
| `src/sentry.rs` | GlitchTip (Sentry-protocol) error tracking: init + the fail-closed `before_send` credential scrubber. Unset DSN ⇒ no-op |

Files ≤300 **non-test** LOC; a module's tests go in a sibling `tests.rs` (`#[cfg(test)] mod tests;`, or `#[path]` for a top-level file) rather than counting against it. One responsibility per module (SRP). Split when a module grows a second reason to change. `tests/module_map.rs` fails when a `src` file is missing from this map or the map names one that doesn't exist.

## Execution model

`bash` runs a command. Finishes within `MCP_SSH_INLINE_TIMEOUT_SECS` (default 2, clamped ≤60; per-call `timeout` clamped ≤600) → output returns inline. Slower (or `bg=true`) → auto-backgrounds to a **job id**; live output streams to a per-job log file (0600). `job(action="poll")` pages that log **newest-first**: no cursor = the latest page (≤`limit` lines, default 200, ≤64 KiB); the footer `[latest N lines shown; earlier output: job(action="poll", id="…", cursor=C)]` names the older page, where `C` is the **byte offset** that page starts at (absolute, so a growing log never re-serves lines). Reads walk backwards under a line/byte/scan budget — never the whole log. When the live log is gone (e.g. a finished job after a restart), poll falls back to the output tail saved in SQLite (500 lines, ≤256 KiB). This is the whole point: bounded output, no context blowups — and no server memory blowups.

A job's live log is capped at 256 MiB (const): past it the group is `SIGKILL`ed and the job finishes `failed`.

**Hybrid persistence:** structured state lives in SQLite (`src/db.rs`) — OAuth tokens + job metadata + output tail, durable across restarts; the high-frequency, append-heavy live output stays a per-job log file on disk. SQLite only sees low-frequency writes (token issue/validate, job create/finish), so one serialized connection is ample.

**Reaper** (startup + hourly) **never kills.** A finished job (row + log) goes 24h after it *finished*; a running job is never aged out; a finished job whose group still holds a process keeps its row until it exits. Untracked `running` rows (left by a previous process) flip to `failed` only once their group probes gone.

### Kill semantics

One implementation, `src/jobs/signal/kill.rs` over `src/jobs/signal.rs`, behind both `job(action="kill")` and `mcp-ssh job kill`:

- Signals via `kill(2)` (`libc`) — never an external `kill` binary (the Docker image has none).
- Tri-state probe: delivered or `EPERM` = alive, `ESRCH` = gone, any other errno = **unknown**. Unknown is never read as gone.
- Escalation: `SIGTERM`, 2s grace, `SIGKILL`, then probe; an interactive job's leader is `SIGSTOP`ped first. Success = the group probed gone, never "the leader exited".
- A persisted pgid outside `2..=i32::MAX` is refused; one recorded under another boot id is never signalled.
- Every path returns a `KillOutcome` — `Killed`, `KilledLeftovers`, `NotRunning`, `Unknown`, `NoProcessGroup`, `CorruptProcessGroup`, `Survived`, `NotSignalled` — that callers only render (`src/tools/job_render.rs`, `src/admin.rs`). `kill_persisted` writes `failed` only on `Killed`, and only while the row still reads `running` (a real exit recorded meanwhile wins) — except that a bare `killed by signal N` the engine recorded is replaced by the kill's reason, so the waiter/kill race can't decide whether the row says why.

## MCP tool design

**Constant, heavily-parametrized surface — 3 resource-oriented tools.** Group by resource; push composition into params (`action`, `cursor`, `limit`, `recursive`, `timeout`, `bg`, `interactive`) — do **NOT** add more tools. New capability = a new param or `action` on an existing tool, almost always.

Args are strict: `#[serde(deny_unknown_fields)]` on all three; a `job` `action` *string* naming no action is an error listing `poll|list|kill` (only a non-string placeholder or a missing `action` falls back to `list`). Agent-facing wording lives only in `src/tools/` (`render.rs`, `job_render.rs`, the tool descriptions in `mod.rs`).

Current tools (three, constant):

| Tool | Params | Does |
|---|---|---|
| `bash` | `cmd`, `cwd?`, `timeout?`, `bg?`, `interactive?`, `title?` | run a command; inline if fast, else a job id (`bg` backgrounds at once; `timeout` ≤600; `interactive` sources `~/.bashrc` via `bash -ic` for aliases/version managers, default fast `sh -c`; `title` labels the job id as `<title>-HH-MM-SS`) |
| `job` | `action?`, `id?`, `cursor?`, `limit?` | jobs by `action`: `poll` (newest-first page, byte cursor), `list` (default; newest first, ≤200 per page, offset cursor), `kill` (typed outcome) |
| `file` | `action`, `path?`, `content?`, `pattern?`, `recursive?`, `src?`, `dest?`, `cursor?`, `limit?` | file ops by `action`: `read` (by line, ≤1000/64 KiB a page)/`write` (atomic)/`append`/`delete` (non-empty dir needs `recursive=true`; symlinks never followed)/`list`/`grep`/`move` (never overwrites) |

## Conventions

Non-negotiable: SOLID, SRP, tested code. The bar: idiomatic, boring, readable Rust. No spaghetti, no premature abstraction. A function reads top to bottom without chasing state. Equally-correct options → pick the one easier to delete. `clippy -D warnings` is the floor, not the ceiling.

- Errors typed. `thiserror` for domain; `anyhow` only at the `main.rs` boundary.
- No `unwrap`/`expect` outside `main` and tests. Panic in the request path crashes every client. Propagate with `?`; branch with `match`/`if let`/`let ... else`.
- Newtype over bare primitives when a value has meaning (`JobId(String)`, not `String`). Make illegal states unrepresentable — `enum` over contradictory `bool`+`Option`. Validate input into a type once at the edge.
- Borrow by default (`&str` over `String`, `&[T]` over `Vec<T>`). `.clone()` only when ownership must move — non-obvious clone gets a one-line `// why`. `Arc<T>` for shared read-only; lock only when you mutate shared state, keep the critical section tiny.
- Functions do one thing. Need "and" to describe it → split it. Concrete first; introduce a trait when the **second** impl arrives.
- Async end-to-end. No `std::sync::Mutex` on the request path — use `tokio::sync`. Never hold a `std::sync` guard across `.await`. No `block_on`; offload blocking I/O with `spawn_blocking`.
- `tracing`, not `println`. Every tool dispatch is a span with `request_id` and `tool` name.
- Derive, don't hand-roll (`Debug`, `Clone`, serde). Every public type derives `Debug`. Keep `pub` surface minimal.
- Comment the non-obvious *why*, never the *what*. Rename until the code doesn't need the *what*.

## Coding Rules

### Think before coding
- State assumptions explicitly. Uncertain → ask, don't guess.
- Multiple interpretations → present them, don't pick silently.
- Simpler approach exists → say so.

### Simplicity first
- Minimum code that solves the stated problem. Nothing speculative.
- No abstractions for single-use code. No unrequested config/flexibility.
- 200 lines that could be 50 → write 50.

### Surgical changes
- Touch only what the task requires. No drive-by refactors/reformatting.
- Match existing style. Every changed line traces to the request.
- Pre-existing dead code: flag, don't delete.

### Goal-driven execution
- "Fix the bug" → reproducing test → make it pass.
- "Add a param" → test the new behavior → make it pass.
- Refactor → tests green before AND after.

## Testing

- Unit: pure logic (config validation, job pagination, path handling) under `#[cfg(test)]` — in the file, or its sibling `tests.rs` once that keeps the file under the LOC limit.
- In-process: `src/app_tests.rs` drives the axum router (auth, host guard, OAuth, MCP JSON-RPC) with `tower::ServiceExt::oneshot`, no socket.
- End-to-end: `tests/e2e/` (`bin/e2e`, also part of `bin/check` and CI) boots the real `mcp-ssh serve` binary per test on an ephemeral port with tempdir state and drives it over HTTP — one scenario per top audit finding.
- Tested code is the default, not the exception. New behavior ships with the test that proves it. Never an unbounded writer (`yes`) in a test — use sparse files.

## Where to look

| Concern | File |
|---|---|
| Startup, CLI dispatch, graceful drain | `src/main.rs` |
| Router wiring, `Host` guard, `/healthz` | `src/app.rs` |
| Config / env / required creds | `src/config.rs` |
| SQLite connection | `src/db.rs` |
| Schema + migrations | `src/db/migrations.rs` |
| Every `jobs` query (metadata, output tail, reconcile) | `src/jobs/store.rs` + `src/jobs/store/` |
| Bearer validation on `/mcp`, consent-form credential check | `src/auth.rs` |
| OAuth 2.1 (discovery, registration, consent page, PKCE, tokens) | `src/oauth/` |
| Token lifetimes, hashing, rotation, revocation | `src/oauth/store.rs` + `src/oauth/store/` |
| Running commands, backgrounding | `src/jobs/run.rs`, `src/jobs/supervise.rs` |
| Job log paging (newest-first, byte cursor) | `src/jobs/log.rs`, `src/jobs/poll.rs` |
| Reaper (reconcile, retention, compaction) | `src/jobs/reaper.rs` |
| Kill semantics + process-group signalling | `src/jobs/signal/kill.rs`, `src/jobs/signal.rs` |
| Tool definitions / MCP surface | `src/tools/mod.rs` |
| Agent-facing wording | `src/tools/render.rs`, `src/tools/job_render.rs` |
| File operations | `src/tools/files.rs` + `src/tools/files/` |
| End-to-end tests | `tests/e2e/` |

## NEVER

- Log or return the password / token — not in responses, errors, or logs.
- Run as root. Dedicated service user, full stop.
- Serve without TLS. Always behind a TLS-terminating reverse proxy.
- Ship without `MCP_SSH_ALLOWED_HOSTS` set.
- Weaken or bypass the auth middleware.
- Add more MCP tools to dodge a param. Parametrize the existing surface.
- Force-push `main`.
- `--no-verify` on commits — fix the hook.

## Context (not in code)

- One host, one service user. Targets the box it runs on; not a fan-out / multi-host tool.
- TLS, multi-host routing, and rate limiting live in the reverse proxy, not here.
- NOT building: an SSH client, a fleet orchestrator, a job scheduler, a secrets vault.

## Note

Do not use git worktrees — work directly in this checkout. If a task is big enough to need subagents, run them as a team in this same checkout: split the work into disjoint pieces so no two agents touch the same files.
