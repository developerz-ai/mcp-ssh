# 🏗️ Architecture

mcp-ssh is **one binary**: an [rmcp](https://github.com/modelcontextprotocol/rust-sdk) MCP server mounted on an axum router, behind an auth middleware. It exposes a tool surface that runs shell commands and file ops **locally, as the service user**. No SSH client, no remote hosts, no gateway.

## 🧬 Stack

| Layer | Choice |
|---|---|
| Language | Rust 2024, toolchain pinned to 1.100.0 (`apps/mcp-ssh/rust-toolchain.toml`) |
| Async runtime | tokio |
| HTTP server | axum 0.8 |
| MCP transport | rmcp 3.9 — **Streamable HTTP** at `/mcp` |
| Durable state | SQLite via `rusqlite` (**bundled** — compiled into the binary) |
| License | MIT |

TLS is deliberately **not** in the binary (keeps deps minimal). A reverse proxy terminates HTTPS → [deploy.md](deploy.md).

## 🗺️ Module map

By area — the file-by-file map (kept exact, and checked by `tests/module_map.rs`) is in [`CLAUDE.md`](../CLAUDE.md#module-map).

| Area | Responsibility |
|---|---|
| `main.rs`, `cli.rs`, `app.rs` | Entry point and CLI (`serve`, `set-auth`, `jobs`, `job kill`, `sessions [--revoke]`). `app.rs` builds the router: `/mcp` (rmcp `StreamableHttpService` behind the auth middleware) and the OAuth routes, both behind the `Host` allowlist, plus `/healthz` outside it. On SIGTERM every MCP stream is cancelled and the server drains for at most 10s. |
| `config.rs` | Config from `/etc/mcp-ssh/config.toml` overlaid by `MCP_SSH_*` env. Refuses to start without non-empty credentials, or with a non-loopback bind and no `MCP_SSH_ALLOWED_HOSTS`. The password's `Debug` is redacted. |
| `db.rs` | The **SQLite durable-state layer** (`rusqlite`, bundled): schema + forward-only migrations (`PRAGMA user_version`). Created on first run (file 0600), WAL mode; one serialized connection driven via `spawn_blocking`. Queries live in the repositories, not here. |
| `auth.rs` | The auth middleware — **bearer-only** guard on `/mcp`; and the constant-time credential check the consent form uses. |
| `oauth/` | A minimal **OAuth 2.1 authorization server** (per the MCP authorization spec): discovery metadata, dynamic client registration, the `/authorize` consent page, `/token` with PKCE S256 and refresh rotation. Tokens are stored hashed in SQLite, so logins survive a restart. |
| `jobs/` | The job engine: launch (own process group, output to a per-job log), inline-or-background, newest-first log paging, the live log cap, kill semantics (`signal`), the `jobs` repository (`store`), and the reaper. |
| `tools/` | The MCP tool surface — **three tools** (`bash`/`job`/`file`) dispatching on an `action` param; arg schemas; file operations; and all agent-facing wording (`render`, `job_render`). |
| `admin.rs` | The local admin commands; they open the existing DB read/write through the same repositories and never start a reaper. |
| `sentry.rs` | Optional GlitchTip error tracking with a fail-closed credential scrubber. |

## 🔌 Request flow

```
client ──HTTPS──▶ reverse proxy ──HTTP──▶ axum (127.0.0.1:1337)
                                            │
                                  auth middleware  ── OAuth 2.1 bearer
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

1. The command is spawned via a bare `sh -c` by default — fast, no rc files. Pass `interactive=true` to `bash` and it runs through an **interactive bash** (`bash -ic`) instead, sourcing the service user's `~/.bashrc` so aliases and version managers (`mise`, `nvm`, `rbenv`) resolve exactly as in a real shell. Either way stdout **and** stderr are merged into a single per-job **log file** (`MCP_SSH_JOB_DIR/<id>.log`, mode 0600): the child's own stdio goes to `/dev/null` and the command re-points stdout+stderr at the log after startup, so bash's "no job control" warnings (no controlling TTY under systemd) never reach the log. Logging to a file — not memory — is what lets long output be paged later without holding it in RAM.
2. The child leads its **own process group**, so a kill reaches everything it spawned. A supervisor task owns it from the moment it spawns (a cancelled request can't orphan it): it records the job, enforces the **256 MiB live log cap** (past it the group is `SIGKILL`ed and the job finishes `failed`), reaps the child, and records the final `JobState` (`Running` / `Exited{code}` / `Failed{error}`); if the job left processes in its group, it keeps the cap on them until the group is gone, and a breach re-records the job `failed`.
3. The caller waits for **either** completion **or** the inline window (`MCP_SSH_INLINE_TIMEOUT_SECS`, default 2s, at most 60; overridable per call via `bash`'s `timeout`, at most 600s). Passing `bg=true` skips the wait entirely and backgrounds at once:
   - **Finished in time** → `RunResult::Inline` — status + the newest page of the log, returned now.
   - **Still running (or `bg`)** → `RunResult::Backgrounded { id }` — the agent gets a job id to poll.

Job **metadata** (id, title, status, exit code/error, start + finish time, process group, boot id, and a bounded output tail) is persisted to SQLite, so `job(action="list")` shows history across restarts; the live, append-heavy output streams to per-job **log files** on disk under the job dir (see [Durable state](#-durable-state-sqlite) below). Job ids are the agent's normalized `title` (or the neutral `job`) + local `HH-MM-SS` — deliberately free of command text so a secret on the command line can't leak into an id, log line, or filename. For the same reason `job(action="list")` exposes only the id and state — never the command text.

```
JobState = Running | Exited { code } | Failed { error }
RunResult = Inline { id, state, page } | Backgrounded { id }
```

### Kills

One kill semantics lives in `jobs/signal.rs` + `jobs/signal/kill.rs`, shared by `job(action="kill")` and `mcp-ssh job kill`. It signals with `kill(2)` directly — no external `kill` binary, so it works in the minimal Docker image — and reads the errno tri-state: delivered or `EPERM` = alive, `ESRCH` = gone, anything else = **unknown**, never mistaken for "gone". A kill is `SIGTERM`, a 2s grace, then `SIGKILL` (an interactive job's leader is `SIGSTOP`ped first so its shell can't start the next command), and it reports success only once a probe finds the whole group gone — never from the leader's exit alone. A row recorded under an earlier boot (`/proc/sys/kernel/random/boot_id`) is never signalled: its pgid may since name an unrelated group. Every outcome is a typed `KillOutcome` (`Killed`, `KilledLeftovers`, `NotRunning`, `Unknown`, `NoProcessGroup`, `CorruptProcessGroup`, `Survived`, `NotSignalled`) that the tool and the CLI only render.

## 💾 Durable state (SQLite)

State splits along a **hybrid** seam — structured rows in SQLite, streaming output on the filesystem — each on the side it's good at.

A single SQLite database at `/var/lib/mcp-ssh/mcp-ssh.db` (sibling of the job-log dir `/var/lib/mcp-ssh/logs/jobs`) is created automatically on first run, in WAL mode, by `db.rs`. SQLite is **compiled into the binary** via `rusqlite`'s `bundled` feature — still a single-binary deploy, no system `libsqlite` dependency. The reads/writes are blocking, so they run on `spawn_blocking`; one serialized connection is ample because the DB only sees **low-frequency** writes.

**In SQLite — durable, survives restarts:**

- **OAuth tokens** — `access_tokens` and `refresh_tokens`, stored only as SHA-256 hashes (a copy of the DB yields no usable bearer), each bound to its client and login family. Logins survive a service restart — which matters because the agent can self-update and restart itself. Access tokens last 24h; refresh tokens 30 days, rotated on use, within a 90-day family cap; a replayed (already-rotated) refresh token ends its whole family.
- **Registered clients** — `clients` (`client_id` → redirect URIs, the cleaned `client_name` it registered, when it last used a bearer; no secrets), capped at 1000; a client at least 7 days old that holds no live token is swept. The last-use time is rewritten at most once a minute per client, so a busy agent's requests stay reads; it is what `job(action="list")`'s presence line and `mcp-ssh sessions` read.
- **Job metadata** — id, title, status (`running`/`exited`/`failed`), exit code or error, start/finish time, process group + boot id, the OAuth client that started it (id + name, so the owner still reads right after the client is swept), and a saved **output tail** (last 500 lines, ≤256 KiB). This is what makes `job(action="list")` show history across restarts.

**On the filesystem — the high-frequency path:**

- Live job output streams to per-job log files in the job dir via a plain shell redirect. That append-heavy, high-frequency write pattern is exactly what SQLite is poor at, so it stays a file. SQLite only sees the bracketing events (token issue/validate, job create/finish).

`job(action="poll")` reads the live **log file** if it's present, else falls back to the bounded **output tail** saved in SQLite — e.g. for a finished job whose log was trimmed or whose process is gone after a restart.

The **reaper** (`jobs/reaper.rs`) keeps both halves bounded. It runs at startup (its first pass is the startup reconcile) and hourly, and **never kills anything**:

- **Reconcile** — a `running` row this process doesn't track (left by a previous process) is flipped to `failed` only once its process group is really gone. A group that outlived the restart (possible outside systemd's `KillMode=control-group`) stays `running`, truthfully, and stays killable from its persisted pgid.
- **Age-out** — a finished job's row and log go 24h after it **finished**. A running job is never aged out, and a finished one whose group still holds a process (`server &`) keeps its row until that process is gone.
- **Compaction** — finished jobs' logs are trimmed to their last 5000 lines while under 3h since finishing, 500 after (never more than 4 MiB).
- **Orphans** — log files with no row are aged out by mtime.
- **OAuth** — expired access/refresh tokens and stale clients are swept on the same pass (counts only, never a token value).

## 📄 The pagination model

Long output is the enemy of a context window, so nothing potentially large is ever read or returned whole — every read is bounded by its budget, never by the file's size.

- **Job logs page newest-first.** `jobs/log.rs` walks a log *backwards* from a byte offset in fixed 64 KiB chunks and stops at the line budget (`limit`, default 200), the byte budget (64 KiB), or a scan ceiling — whichever comes first. A page is a `TailPage { lines, older }`: lines in chronological order, plus `older`, the **byte offset** where the page starts, which the agent passes back as `cursor` for the previous page. An absolute offset (not "N lines from the end") means a live job that keeps appending never re-serves lines. The finished job's saved tail is paged by the same code over the in-memory text.
- **Files page forward by line.** `file(action="read")` streams from the start, stops once `limit` lines (≤1000) or 64 KiB are taken, and reports `[lines A..B of ≥T; next_cursor=B]` — a lower bound, since it never scans to EOF. A 30s deadline guards slow filesystems.
- **Shelled-out `list`/`grep`** run under a byte cap and a 30s wall-clock deadline (`tools/files/shell.rs`), killing the child on either.

The exact reply wording (footers, markers) is in [usage.md](usage.md#-pagination-model).

## 🧰 The tool surface

`tools/mod.rs` defines one `Tools` struct whose methods are the MCP tools, registered via rmcp's `#[tool_router]`. Each method is a **thin adapter**: parse params → call `jobs` or `files` → render the typed result as MCP content. The domain returns facts (`RunResult`, `TailPage`, `KillOutcome`, `FileOutcome`, typed errors); every sentence the agent reads is phrased in `tools/render.rs` (file) and `tools/job_render.rs` (bash/job). Args are strict: unknown fields are rejected (`deny_unknown_fields`), and a `job` `action` string naming no action is an error — only a non-string placeholder (`null`, `true`) or a missing `action` falls back to the read-only `list`.

The surface is **three resource-oriented tools** — `bash`, `job`, `file` — grouped by resource, with composition pushed into params. `job` and `file` dispatch on an `action` param (`poll`/`list`/`kill`; `read`/`write`/`append`/`delete`/`list`/`grep`/`move`) rather than splitting into a tool per operation, keeping the surface constant. See [usage.md](usage.md) for each one's params and examples.

## 🔐 Auth

The middleware in `auth.rs` guards `/mcp` with a **bearer-only** check: every request must carry an
`Authorization: Bearer <token>` header containing a valid OAuth 2.1 access token issued by the
`oauth` module. There is no HTTP Basic fallback on `/mcp`.

The `oauth` module is a minimal authorization server implementing the MCP authorization spec:
discovery metadata (`/.well-known/oauth-authorization-server`), dynamic client registration,
`/authorize` (a consent page), and `/token`. All MCP clients — Claude, Cursor, or any
spec-compliant GUI — drive this flow automatically; the user logs in once with the
username/password set via `mcp-ssh set-auth`.

`GET /authorize` validates the request (registered `client_id` + exact `redirect_uri`, PKCE
S256) and renders a login form showing **where the code will be sent**; it never issues a code.
The code is issued only by the form's `POST`, carrying a single-use CSRF token (10 minutes) that
names the request held server-side — nothing in the posted form can change where the code goes,
and nothing ambient (a cached Basic header, a cookie) can make a page load mint one. The page is
`no-store`, un-frameable, and a failed login costs a flat delay (rate limiting itself belongs to
the proxy).

Registration is not a formality: `/register` persists the `client_id` → `redirect_uri` binding,
and `/authorize` serves the form only for a URI that exact `client_id` registered. An `/authorize`
link that pairs a real client's `client_id` with someone else's `redirect_uri` therefore gets a
`400`, not a login page.

This is defense in depth, not a complete anti-phishing control. Dynamic client registration is
unauthenticated by design — it bootstraps auth — so an attacker can register their own
`client_id` for their own callback and lure a victim to a *self-consistent* `/authorize` link.
The consent page's destination line is what exposes that lure: a login page you didn't expect,
or one sending you somewhere you don't recognise, should be treated as hostile.

Tokens are opaque 256-bit random values stored as SHA-256 hashes. `/token` answers `no-store`;
a token-store outage is a `503` (retry), never an `invalid_grant` that would make a client
discard a good login.

Credentials are a single username/password, set once with `mcp-ssh set-auth <user>` and read
from config/env at boot. Missing or empty credentials = the server refuses to start. `set-auth`
revokes every existing session; `mcp-ssh sessions --revoke` does the same on demand.

## ⚠️ Trust boundary

The tool surface is **arbitrary shell + filesystem access** as the service user — that *is* the product. Containment is operational, not in-code:

- Run as a **dedicated low-privilege user**.
- **TLS-only**, via the reverse proxy; never expose `127.0.0.1:1337` directly.
- `MCP_SSH_ALLOWED_HOSTS` set to the public hostname — rmcp rejects mismatched `Host` headers, the **DNS-rebinding guard**.

Hardening details → [deploy.md](deploy.md).
