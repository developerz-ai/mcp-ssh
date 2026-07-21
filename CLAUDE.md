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
| Lang | Rust 2024 (pinned via `rust-toolchain.toml`) |
| Runtime | tokio |
| HTTP | axum 0.8 |
| MCP | rmcp 1.7 (Streamable HTTP server transport) |
| Durable state | SQLite via `rusqlite` (`bundled` — compiled in, no system libsqlite). OAuth tokens + job metadata durable across restarts |
| Errors | thiserror (domain) + anyhow (main boundary) |
| Logging | tracing |
| Auth | OAuth 2.1 (MCP spec, for Claude) + HTTP Basic (simple clients) |
| TLS | reverse proxy — **not** in the binary |
| CLI | `mcp-ssh serve`, `mcp-ssh set-auth <user>`, `mcp-ssh jobs [--all]`, `mcp-ssh job kill <id>`, `mcp-ssh sessions` |

Deps pinned to latest stable at implementation time.

## Commands

| Task | Command |
|---|---|
| Build | `cargo build` |
| Run locally (watch + reload, loads `.env`) | `bin/dev` |
| Full gate (fmt --check + clippy -D warnings + test) | `bin/check` |
| Test | `cargo test` |
| Single test by pattern | `cargo test <pattern>` |
| Format + lint | `cargo fmt && cargo clippy` |
| Run the binary | `cargo run -- serve` |
| Mint a bearer for a headless client (OAuth PKCE) | `bin/mcp-token` |

Secrets and local config: copy `.env.example` → `.env`. `.env` is gitignored.

## Module map

Keep this accurate — it's the navigation aid.

| Module | Owns |
|---|---|
| `src/main.rs` | entry: CLI parse, config load, build axum router, serve; dispatches admin subcommands |
| `src/cli.rs` | clap command definitions (`serve`/`set-auth`/`jobs`/`job kill`/`sessions`) |
| `src/admin.rs` | local admin subcommands (`jobs`/`job kill`/`sessions`): read/act on the same SQLite through `jobs::JobRepo` — never builds a `JobStore` (that would start a reaper + reconcile and fail the live server's running rows); `job kill` only renders `jobs::kill_persisted`'s outcome, it owns no kill logic. Never prints token values |
| `src/config.rs` | env + TOML file config; fails fast if auth creds missing. `db_path()` resolves the DB path alone for the cred-free admin commands |
| `src/db.rs` | SQLite durable-state layer (`rusqlite`, `bundled`): the schema + forward-only migrations for OAuth `access_tokens`/`refresh_tokens` + registered `clients` (`client_id`→`redirect_uri`, no secrets) + job metadata + output tail; the queries themselves live in the repositories (`src/jobs/store.rs`, `src/oauth/store.rs`). DB at `/var/lib/mcp-ssh/mcp-ssh.db`, WAL, auto-created; one serialized connection driven via `spawn_blocking` |
| `src/auth.rs` | HTTP Basic auth middleware |
| `src/oauth/` | minimal OAuth 2.1 server: discovery metadata, dynamic client registration, authorize + token with PKCE, bearer validation; tokens *and* client registrations persisted in SQLite (`src/db.rs`) so logins survive a restart; `/authorize` only issues a code to a `redirect_uri` the named `client_id` registered (exact match, else `invalid_request`); `sweep_expired_access`/`sweep_expired_refresh` bulk-delete dead rows (count only, never a token value) for the reaper to call |
| `src/jobs/mod.rs` | job engine: run a command, return inline if fast (<2s) else a job id (or immediately when `bg`); live output streams to a per-job log file (polled paginated), metadata + output tail persisted to SQLite so history survives restarts; startup reconcile flips rows left `running` by a previous process to `failed` only once their persisted process group is really dead (a survivor stays `running`) |
| `src/jobs/id.rs` | JobId newtype: human-readable ids — neutral `job` prefix + local `HH-MM-SS` (e.g., `job-23-30-07`); free of command text so secrets can't leak into an id, log line, or filename |
| `src/jobs/log.rs` | job log pagination: read per-job log files by page (cursor + limit) |
| `src/jobs/reaper.rs` | reaper (startup + hourly): drops jobs >24h old (DB rows + log files, killing any still-`Running` group first via `src/jobs/signal.rs`), trims finished jobs' logs to a trailing tail (5000 lines <3h old, 500 after), mtime-ages orphaned files from a previous run, sweeps expired OAuth tokens (`src/oauth/store.rs`) on the same pass |
| `src/jobs/shell.rs` | `Shell`: how a user command is launched — bare `sh -c` (default) vs. interactive `bash -ic` (sources `~/.bashrc` for aliases/version managers) |
| `src/jobs/signal.rs` | process-group signalling **and the one kill semantics**: `kill_job` (live handle) / `kill_persisted` (a row's pgid — running-check, corrupt-pgid gate, `failed` transition, returning a `KillOutcome` the engine and the `mcp-ssh job kill` CLI only render) over `kill_group`'s TERM→KILL escalation; `group_alive` liveness probe the startup reconcile and reaper's log-compaction gate also reuse |
| `src/jobs/store.rs` | `JobRepo`: the only place `jobs` SQL lives (engine, reaper, and admin CLI call typed methods) + the row⇄`JobState` mapping and the startup-reconcile query; mirrors `oauth::Store`. No `cmd` text reaches a query or a log line |
| `src/tools/mod.rs` | MCP tool surface (`#[tool_router]`/`#[tool]` from rmcp): 3 tools (`bash`/`job`/`file`) dispatching on `action`. Thin adapters over jobs + files, and **the only place agent-facing wording lives** — `render_file`/`render_file_error` turn `files`' typed outcome/error into the sentence (and the `list`-redirect / no-clobber hints, which name `file`'s own actions) |
| `src/tools/args.rs` | deserializable arg structs for the 3 tools (`BashArgs`/`JobArgs`/`FileArgs`, `JobAction`/`FileAction`) + `lenient_action` (the malformed-`action` leniency shim `JobArgs` uses) — schema shape only, no dispatch logic |
| `src/tools/files.rs` | file operations (`tokio::fs`; `ls`/`find`/`grep` shelled out). Returns facts — `FileOutcome` / typed `FileError` (a failed shell op keeps its `ShError`) — never a rendered sentence |
| `src/tools/files/shell.rs` | bounded runner behind the shelled-out file ops: streams a child's combined output under a byte cap + wall-clock deadline, killing it on either |

Files ≤300 LOC. One responsibility per module (SRP). Split when a module grows a second reason to change.

## Execution model

`bash` runs a command. Finishes within `MCP_SSH_INLINE_TIMEOUT_SECS` (default 2) → output returns inline. Slower (or `bg=true`) → auto-backgrounds to a **job id**; live output streams to a per-job log file. `job(action="poll")` paginates that log **newest-first** (cursor 0 = latest output; page back with cursor) so a chatty command never floods the agent's context and monitoring a long job shows what's happening now — falling back to the bounded output tail saved in SQLite when the live log is gone (e.g. a finished job after a restart). This is the whole point: bounded output, no context blowups.

**Hybrid persistence:** structured state lives in SQLite (`src/db.rs`) — OAuth tokens + job metadata + output tail, durable across restarts; the high-frequency, append-heavy live output stays a per-job log file on disk. SQLite only sees low-frequency writes (token issue/validate, job create/finish), so one serialized connection is ample. Jobs >24h old are reaped (startup + hourly).

## MCP tool design

**Constant, heavily-parametrized surface — 3 resource-oriented tools.** Group by resource; push composition into params (`action`, `cursor`, `limit`, `recursive`, `timeout`, `bg`, `interactive`) — do **NOT** add more tools. New capability = a new param or `action` on an existing tool, almost always.

Current tools (three, constant):

| Tool | Params | Does |
|---|---|---|
| `bash` | `cmd`, `cwd?`, `timeout?`, `bg?`, `interactive?`, `title?` | run a command; inline if fast, else a job id (`bg` backgrounds at once; `interactive` sources `~/.bashrc` via `bash -ic` for aliases/version managers, default fast `sh -c`; `title` labels the job id as `<title>-HH-MM-SS`) |
| `job` | `action`, `id?`, `cursor?`, `limit?` | jobs by `action`: `poll` (paginated output), `list` (jobs + status), `kill` |
| `file` | `action`, `path?`, `content?`, `pattern?`, `recursive?`, `src?`, `dest?`, `cursor?`, `limit?` | file ops by `action`: `read`/`write`/`append`/`delete`/`list`/`grep`/`move` |

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

- Unit: pure logic (config validation, job pagination, path handling), colocated `#[cfg(test)]`.
- Integration: server booted, real MCP requests over HTTP, in `tests/`.
- Tested code is the default, not the exception. New behavior ships with the test that proves it.

## Where to look

| Concern | File |
|---|---|
| Startup, router wiring, CLI | `src/main.rs` |
| Config / env / required creds | `src/config.rs` |
| SQLite connection, schema, migrations | `src/db.rs` |
| Every `jobs` query (metadata, output tail, reconcile) | `src/jobs/store.rs` |
| HTTP Basic auth | `src/auth.rs` |
| OAuth 2.1 (discovery, registration, PKCE, bearer) | `src/oauth/` |
| Running commands, backgrounding, job logs | `src/jobs/mod.rs` |
| Reaper (DB + log-file cleanup) | `src/jobs/reaper.rs` |
| Kill semantics + process-group signalling | `src/jobs/signal.rs` |
| Tool definitions / MCP surface | `src/tools/mod.rs` |
| File operations | `src/tools/files.rs` |

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
