# 🏗️ Architecture

mcp-ssh is **one binary**: an [rmcp](https://github.com/modelcontextprotocol/rust-sdk) MCP server mounted on an axum router, behind an auth middleware. It exposes a tool surface that runs shell commands and file ops **locally, as the service user**. No SSH client, no remote hosts, no gateway.

## 🧬 Stack

| Layer | Choice |
|---|---|
| Language | Rust 2024 |
| Async runtime | tokio |
| HTTP server | axum 0.8 |
| MCP transport | rmcp 1.7 — **Streamable HTTP** at `/mcp` |
| Durable state | SQLite via `rusqlite` (**bundled** — compiled into the binary) |
| License | MIT |

TLS is deliberately **not** in the binary (keeps deps minimal). A reverse proxy terminates HTTPS → [deploy.md](deploy.md).

## 🗺️ Module map

| Module | Responsibility |
|---|---|
| `main.rs` | Entry point. Boots tracing, loads config, builds the JobStore, mounts the rmcp `StreamableHttpService` at `/mcp`, wraps it in the auth middleware, serves it. |
| `config.rs` | Runtime config from env + file. Fails fast at boot if credentials are missing. |
| `db.rs` | The **SQLite durable-state layer** (`rusqlite`, bundled): OAuth tokens + job metadata + output tail. Created on first run at `/var/lib/mcp-ssh/mcp-ssh.db`, WAL mode; one serialized connection driven via `spawn_blocking`. |
| `auth.rs` | The auth middleware — **bearer-only** guard on `/mcp`; rejects everything that is not a valid OAuth 2.1 access token. |
| `oauth` | A minimal **OAuth 2.1 authorization server** (per the MCP authorization spec) so GUI clients can log in. Tokens are persisted in SQLite, so logins survive a restart. |
| `jobs/mod.rs` | The job engine: run a command, return inline-or-background (incl. immediate `bg`), paginated logs, and job metadata persisted to SQLite so history survives restarts. |
| `jobs/id.rs` | JobId newtype: human-readable ids — neutral `job` prefix + local `HH-MM-SS` (e.g., `job-23-30-07`); free of command text so secrets can't leak. |
| `jobs/log.rs` | Job log pagination: read log files by page (cursor + limit). |
| `jobs/reaper.rs` | Reaper (startup + hourly): deletes jobs >24h old (DB rows + log files), trims finished jobs' logs to a tail, mtime-ages orphaned files, marks jobs stuck in `running` across a restart as failed; process-group kill helpers (TERM→KILL escalation). |
| `tools/mod.rs` | The MCP tool surface — **three tools** (`bash`/`job`/`file`) dispatching on an `action` param; thin adapters over `jobs` and `files`. |
| `tools/files.rs` | File operations (read/write/append/delete/list/grep/move). |

## 🔌 Request flow

```
client ──HTTPS──▶ reverse proxy ──HTTP──▶ axum (127.0.0.1:1337)
                                            │
                                  auth middleware  ── Basic | OAuth 2.1
                                            │
                                  /mcp  StreamableHttpService (rmcp)
                                            │
                                       Tools (tool_router)
                                     bash · job · file (action)
                                          ╱        ╲
                                  jobs/mod.rs    tools/files.rs
                                  (bash, job)     (file ops)
```

Every MCP request passes the auth middleware first; only authenticated requests reach the tool router.

## 🏃 The execution model — inline, then background

This is the core design idea. `bash` must serve both *"echo hello"* and *"a 20-minute build"* without either blocking the agent or flooding its context.

When a command starts (`JobStore::run`):

1. The command is spawned via a bare `sh -c` by default — fast, no rc files. Pass `interactive=true` to `bash` and it runs through an **interactive bash** (`bash -ic`) instead, sourcing the service user's `~/.bashrc` so aliases and version managers (`mise`, `nvm`, `rbenv`) resolve exactly as in a real shell. Either way stdout **and** stderr are merged into a single per-job **log file** (`MCP_SSH_JOB_DIR/<id>.log`): the child's own stdio goes to `/dev/null` and the command re-points stdout+stderr at the log after startup, so bash's "no job control" warnings (no controlling TTY under systemd) never reach the log. Logging to a file — not memory — is what lets long output be paginated later without holding it all in RAM.
2. A background task owns the child process, waits for it to exit, and records the final `JobState` (`Running` / `Exited{code}` / `Failed{error}`).
3. The caller waits for **either** completion **or** the inline window (`MCP_SSH_INLINE_TIMEOUT_SECS`, default 2s; overridable per call via `bash`'s `timeout`). Passing `bg=true` skips the wait entirely and backgrounds at once:
   - **Finished in time** → `RunResult::Inline` — status + first page of the log, returned now.
   - **Still running (or `bg`)** → `RunResult::Backgrounded { id }` — the agent gets a job id to poll.

Job **metadata** (id, title, status, exit code/error, start time, and a bounded output tail) is persisted to SQLite, so `job(action="list")` shows history across restarts; the live, append-heavy output streams to per-job **log files** on disk under the job dir (see [Durable state](#-durable-state-sqlite) below). The **reaper** runs on startup and hourly: it drops any job whose age exceeds 24h — deleting both its DB row and its log file — and trims finished jobs' logs to a tail, so the database and the job dir stay bounded without manual cleanup. Job ids are a neutral `job` prefix + local `HH-MM-SS` — deliberately free of command text so a secret on the command line can't leak into an id, log line, or filename. For the same reason `job(action="list")` exposes only non-sensitive metadata — the id and status — never the command text (which could carry a pasted secret).

```
JobState = Running | Exited { code } | Failed { error }
RunResult = Inline { state, page } | Backgrounded { id }
```

## 💾 Durable state (SQLite)

State splits along a **hybrid** seam — structured rows in SQLite, streaming output on the filesystem — each on the side it's good at.

A single SQLite database at `/var/lib/mcp-ssh/mcp-ssh.db` (sibling of the job-log dir `/var/lib/mcp-ssh/logs/jobs`) is created automatically on first run, in WAL mode, by `db.rs`. SQLite is **compiled into the binary** via `rusqlite`'s `bundled` feature — still a single-binary deploy, no system `libsqlite` dependency. The reads/writes are blocking, so they run on `spawn_blocking`; one serialized connection is ample because the DB only sees **low-frequency** writes.

**In SQLite — durable, survives restarts:**

- **OAuth tokens** — `access_tokens` and `refresh_tokens` (token + expiry). Logins now survive a service restart instead of forcing a re-auth every time — which matters because the agent can self-update and restart itself. Access tokens last 24h; refresh tokens 1 year, rotated on use.
- **Job metadata** — id, title, status (`running`/`exited`/`failed`), exit code or error, start time, and a saved **output tail**. This is what makes `job(action="list")` show history across restarts.

**On the filesystem — the high-frequency path:**

- Live job output streams to per-job log files in the job dir via a plain shell redirect. That append-heavy, high-frequency write pattern is exactly what SQLite is poor at, so it stays a file. SQLite only sees the bracketing events (token issue/validate, job create/finish).

`job(action="poll")` reads the live **log file** if it's present, else falls back to the bounded **output tail** saved in SQLite — e.g. for a finished job whose log was trimmed or whose process is gone after a restart.

The **reaper** (`jobs/reaper.rs`) keeps both halves bounded — see the execution model above: startup + hourly, it deletes jobs >24h old (DB rows **and** log files), trims finished jobs' logs to a tail (≈5000 lines while under 3h old, ≈500 after — bounded disk over long uptime), uses log-file mtime to age orphaned files left by a previous run, and marks any job still `running` after a restart as failed (its process is gone).

## 📄 The pagination model

Long output is the enemy of a context window, so anything potentially large is **paginated by line**.

- A `Page` carries `lines`, `next_cursor`, `total_lines`, and `has_more`.
- `job(action="poll", id, cursor, limit)` and `file(action="read", path, cursor, limit)` both read a window `[cursor, cursor+limit)` (default limit 200) and report where to continue.
- The agent walks forward — `cursor = next_cursor` — until `has_more` is false.

`read_page` re-reads the log file each call and slices by line. Simple and correct for typical logs; byte-offset seeking is the upgrade path if logs ever get huge.

## 🧰 The tool surface

`tools/mod.rs` defines one `Tools` struct whose methods are the MCP tools, registered via rmcp's `#[tool_router]`. Each method is a **thin adapter**: parse params → call `jobs` or `files` → wrap the result as MCP content. No business logic lives in the tool layer.

The surface is **three resource-oriented tools** — `bash`, `job`, `file` — grouped by resource, with composition pushed into params. `job` and `file` dispatch on an `action` param (`poll`/`list`/`kill`; `read`/`write`/`append`/`delete`/`list`/`grep`/`move`) rather than splitting into a tool per operation, keeping the surface constant. See [usage.md](usage.md) for each one's params and examples.

## 🔐 Auth

The middleware in `auth.rs` guards `/mcp` with a **bearer-only** check: every request must carry an
`Authorization: Bearer <token>` header containing a valid OAuth 2.1 access token issued by the
`oauth` module. There is no HTTP Basic fallback on `/mcp`.

The `oauth` module is a minimal authorization server implementing the MCP authorization spec:
discovery metadata (`/.well-known/oauth-authorization-server`), dynamic client registration,
`/authorize` (with HTTP Basic login for the resource-owner credential grant + PKCE), and
`/token`. All MCP clients — Claude, Cursor, or any spec-compliant GUI — drive this flow
automatically; the user logs in once with the username/password set via `mcp-ssh set-auth`.

Credentials are a single username/password, set once with `mcp-ssh set-auth <user>` and read
from config/env at boot. Missing credentials = the server refuses to start.

## ⚠️ Trust boundary

The tool surface is **arbitrary shell + filesystem access** as the service user — that *is* the product. Containment is operational, not in-code:

- Run as a **dedicated low-privilege user**.
- **TLS-only**, via the reverse proxy; never expose `127.0.0.1:1337` directly.
- `MCP_SSH_ALLOWED_HOSTS` set to the public hostname — rmcp rejects mismatched `Host` headers, the **DNS-rebinding guard**.

Hardening details → [deploy.md](deploy.md).
