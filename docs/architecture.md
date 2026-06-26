# 🏗️ Architecture

mcp-ssh is **one binary**: an [rmcp](https://github.com/modelcontextprotocol/rust-sdk) MCP server mounted on an axum router, behind an auth middleware. It exposes a tool surface that runs shell commands and file ops **locally, as the service user**. No SSH client, no remote hosts, no gateway.

## 🧬 Stack

| Layer | Choice |
|---|---|
| Language | Rust 2024 |
| Async runtime | tokio |
| HTTP server | axum 0.8 |
| MCP transport | rmcp 1.7 — **Streamable HTTP** at `/mcp` |
| License | MIT |

TLS is deliberately **not** in the binary (keeps deps minimal). A reverse proxy terminates HTTPS → [deploy.md](deploy.md).

## 🗺️ Module map

| Module | Responsibility |
|---|---|
| `main.rs` | Entry point. Boots tracing, loads config, builds the JobStore, mounts the rmcp `StreamableHttpService` at `/mcp`, wraps it in the auth middleware, serves it. |
| `config.rs` | Runtime config from env + file. Fails fast at boot if credentials are missing. |
| `auth.rs` | The auth middleware — **HTTP Basic** and **OAuth 2.1** in front of `/mcp`. |
| `oauth` | A minimal **OAuth 2.1 authorization server** (per the MCP authorization spec) so GUI clients can log in. |
| `jobs.rs` | The job engine: run a command, return inline-or-background, paginated logs. |
| `tools/mod.rs` | The MCP tool surface — thin adapters over `jobs` and `files`. |
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
                                          ╱        ╲
                                    jobs.rs       files.rs
                                  (bash, jobs)   (file_* ops)
```

Every MCP request passes the auth middleware first; only authenticated requests reach the tool router.

## 🏃 The execution model — inline, then background

This is the core design idea. `bash` must serve both *"echo hello"* and *"a 20-minute build"* without either blocking the agent or flooding its context.

When a command starts (`JobStore::run`):

1. The command is spawned via `sh -c`, with stdout **and** stderr merged into a single per-job **log file** (`MCP_SSH_JOB_DIR/<id>.log`). Logging to a file — not memory — is what lets long output be paginated later without holding it all in RAM.
2. A background task owns the child process, waits for it to exit, and records the final `JobState` (`Running` / `Exited{code}` / `Failed{error}`).
3. The caller waits for **either** completion **or** the inline window (`MCP_SSH_INLINE_TIMEOUT_SECS`, default 2s; overridable per call via `bash`'s `timeout`):
   - **Finished in time** → `RunResult::Inline` — status + first page of the log, returned now.
   - **Still running** → `RunResult::Backgrounded { id }` — the agent gets a job id to poll.

Jobs live in an in-memory map keyed by id (`j1`, `j2`, …); the log files persist on disk under the job dir.

```
JobState = Running | Exited { code } | Failed { error }
RunResult = Inline { state, page } | Backgrounded { id }
```

## 📄 The pagination model

Long output is the enemy of a context window, so anything potentially large is **paginated by line**.

- A `Page` carries `lines`, `next_cursor`, `total_lines`, and `has_more`.
- `job_poll(id, cursor, limit)` and `file_read(path, cursor, limit)` both read a window `[cursor, cursor+limit)` (default limit 200) and report where to continue.
- The agent walks forward — `cursor = next_cursor` — until `has_more` is false.

`read_page` re-reads the log file each call and slices by line. Simple and correct for typical logs; byte-offset seeking is the upgrade path if logs ever get huge.

## 🧰 The tool surface

`tools/mod.rs` defines one `Tools` struct whose methods are the MCP tools, registered via rmcp's `#[tool_router]`. Each method is a **thin adapter**: parse params → call `jobs` or `files` → wrap the result as MCP content. No business logic lives in the tool layer.

The set is intentionally small and heavily parametrized: `bash` + 3 job tools + 7 file tools. See [usage.md](usage.md) for each one's params and examples.

## 🔐 Auth

The middleware in `auth.rs` guards `/mcp` with two interchangeable modes against **one** credential set:

| Mode | Who uses it | How |
|---|---|---|
| **HTTP Basic** | curl, scripts, simple clients | `Authorization: Basic <base64(user:pass)>`, compared against the configured credentials |
| **OAuth 2.1** | Claude & GUI MCP clients (they require it) | the `oauth` module is a minimal authorization server implementing the MCP authorization spec; the client drives the flow, the user logs in with the same username/password |

Credentials are a single username/password, set once with `mcp-ssh set-auth <user>` and read from config/env at boot. Missing credentials = the server refuses to start.

## ⚠️ Trust boundary

The tool surface is **arbitrary shell + filesystem access** as the service user — that *is* the product. Containment is operational, not in-code:

- Run as a **dedicated low-privilege user**.
- **TLS-only**, via the reverse proxy; never expose `127.0.0.1:1337` directly.
- `MCP_SSH_ALLOWED_HOSTS` set to the public hostname — rmcp rejects mismatched `Host` headers, the **DNS-rebinding guard**.

Hardening details → [deploy.md](deploy.md).
