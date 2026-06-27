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
| Errors | thiserror (domain) + anyhow (main boundary) |
| Logging | tracing |
| Auth | OAuth 2.1 (MCP spec, for Claude) + HTTP Basic (simple clients) |
| TLS | reverse proxy — **not** in the binary |
| CLI | `mcp-ssh serve`, `mcp-ssh set-auth <user>` |

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
| `src/main.rs` | entry: CLI parse, config load, build axum router, serve |
| `src/config.rs` | env + TOML file config; fails fast if auth creds missing |
| `src/auth.rs` | HTTP Basic auth middleware |
| `src/oauth/` | minimal OAuth 2.1 server: discovery metadata, dynamic client registration, authorize + token with PKCE, bearer validation |
| `src/jobs/mod.rs` | job engine: run a command, return inline if fast (<2s) else a job id (or immediately when `bg`); output streams to a per-job log file, polled paginated |
| `src/jobs/id.rs` | JobId newtype: human-readable ids — neutral `job` prefix + local `HH:MM` (e.g., `job-23:30`); free of command text so secrets can't leak into an id, log line, or filename |
| `src/jobs/log.rs` | job log pagination: read per-job log files by page (cursor + limit) |
| `src/jobs/reaper.rs` | hourly reaper drops jobs >24h old (killing any still-`Running` group first); process-group kill helpers (TERM→KILL escalation), shared with `job(action="kill")` |
| `src/tools/mod.rs` | MCP tool surface (`#[tool_router]`/`#[tool]` from rmcp): 3 tools (`bash`/`job`/`file`) dispatching on `action`. Thin adapters over jobs + files |
| `src/tools/files.rs` | file operations (`tokio::fs`; `ls`/`find`/`grep` shelled out) |

Files ≤300 LOC. One responsibility per module (SRP). Split when a module grows a second reason to change.

## Execution model

`bash` runs a command. Finishes within `MCP_SSH_INLINE_TIMEOUT_SECS` (default 2) → output returns inline. Slower (or `bg=true`) → auto-backgrounds to a **job id**; output streams to a per-job log file. `job(action="poll")` paginates that log (cursor/limit) so a chatty command never floods the agent's context. This is the whole point: bounded output, no context blowups. Jobs >24h old are reaped hourly.

## MCP tool design

**Constant, heavily-parametrized surface — 3 resource-oriented tools.** Group by resource; push composition into params (`action`, `cursor`, `limit`, `recursive`, `timeout`, `bg`, `interactive`) — do **NOT** add more tools. New capability = a new param or `action` on an existing tool, almost always.

Current tools (three, constant):

| Tool | Params | Does |
|---|---|---|
| `bash` | `cmd`, `cwd?`, `timeout?`, `bg?`, `interactive?` | run a command; inline if fast, else a job id (`bg` backgrounds at once; `interactive` sources `~/.bashrc` via `bash -ic` for aliases/version managers, default fast `sh -c`) |
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
| HTTP Basic auth | `src/auth.rs` |
| OAuth 2.1 (discovery, registration, PKCE, bearer) | `src/oauth/` |
| Running commands, backgrounding, job logs | `src/jobs/mod.rs` |
| Reaper eviction + process-group kill helpers | `src/jobs/reaper.rs` |
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
