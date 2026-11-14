# 🧰 Usage

Three tools — `bash`, `job`, `file` — each heavily parametrized. Their parameters and a short example follow. All tools execute **locally on the host running mcp-ssh, as the service user.**

## 🏃 Execution model — inline, then background

`bash` does not block. It waits a short **inline window** (default 2s) for the command to finish:

| Outcome | What you get back |
|---|---|
| Finishes within the window | The final status + the **newest** page of output, inline |
| Still running after the window | A **job id** (e.g. `job-23-30-07`, or `build-23-30-07` with `title="build"`) — monitor it with `job(action="poll")` |

This is what makes a 20-minute build safe: it backgrounds automatically, and you look at the latest output a page at a time instead of dumping the whole log into context.

```text
bash(cmd="cargo build --release", title="build")
→ job build-23-30-07 still running after the inline window. Monitor it with job(action="poll", id="build-23-30-07").

job(action="poll", id="build-23-30-07")
→ {"status":"running"}
     Compiling serde v1.0.228
  ...the latest 200 lines...
     Compiling mcp-ssh v1.4.0
  [latest 200 lines shown; earlier output: job(action="poll", id="build-23-30-07", cursor=48213)]

job(action="poll", id="build-23-30-07", cursor=48213)
→ {"status":"running"}
  ...the 200 lines before those, still in chronological order...
  [latest 200 lines shown; earlier output: job(action="poll", id="build-23-30-07", cursor=20977)]

job(action="poll", id="build-23-30-07")          # no cursor: back to the newest output
→ {"status":"exited","code":0}
      Finished `release` profile [optimized] target(s) in 4m 12s
```

The first line is always the job's state as JSON: `{"status":"running"}`, `{"status":"exited","code":N}`, or `{"status":"failed","error":"..."}` (killed, killed by a signal, output past the log cap, or lost in a restart). The footer appears only when older output exists; on the oldest page there is none.

Override the window with `timeout` when you *want* to wait longer inline (e.g. a command you know takes ~5s), or skip waiting entirely with `bg=true`:

```
bash(cmd="npm test", timeout=10)     # wait up to 10s before backgrounding (at most 600)
bash(cmd="./deploy.sh", bg=true)     # background immediately, return the job id now
```

## 📄 Pagination model

Nothing that can be large is returned whole. Stdout and stderr are merged into one stream, terminal-style.

**Job output (`job(action="poll")`) pages newest-first:**

- No `cursor` → the **latest** output: the last `limit` lines (default 200), at most 64 KiB, in chronological order. A single line longer than 8 KiB keeps its newest 8 KiB behind a `[+N bytes]…` marker.
- When older output exists, the footer `[latest N lines shown; earlier output: job(action="poll", id="…", cursor=C)]` names the exact next call. `C` is a **byte offset** into the log (where the page you just got starts) — pass it back verbatim, never compute one. It is absolute, so a running job that keeps appending never re-serves lines as you page back.
- Once the live log is gone (trimmed or reaped), `poll` serves the output tail saved when the job finished: its last 500 lines, at most 256 KiB.

**Job history (`job(action="list")`)** opens with a presence line — the agents that used this host in the last 15 minutes, you first — then a JSON array of `{"id": …, "state": {"status": …}, "by": …}`, newest first, at most `limit` (default and maximum 200) per page. When older jobs exist the reply ends with `[older jobs: job(action="list", cursor=N)]`.

**Files (`file(action="read")`) page forward by line:** `cursor` is the line to start at (default 0), `limit` the line count (default 200, at most 1000), and a page also stops at 64 KiB. When more remains the reply ends with `[lines A..B of ≥T; next_cursor=B]` — the total is a lower bound, since the file is never scanned to the end. An empty file reads `[empty file]`; a cursor past the end says how many lines the file has.

## 🐚 Shell & jobs

### `bash(cmd, cwd?, timeout?, bg?, interactive?, title?)`
Run a shell command. By default it's a fast bare `sh -c`. Pass `interactive=true` to run it in an **interactive bash** (`bash -ic`) that sources your `~/.bashrc`, so aliases and version managers (`mise`, `nvm`, `rbenv`) work just like a normal shell. Returns output inline if it finishes within the inline window, else a job id to monitor with `job`.

| Param | Required | Default | Meaning |
|---|---|---|---|
| `cmd` | yes | — | the shell command (must not be empty) |
| `cwd` | no | the server's cwd | working directory; must be an existing directory |
| `timeout` | no | 2s | seconds to wait inline before backgrounding (clamped to 600) |
| `bg` | no | false | `true` backgrounds immediately, returning the job id without waiting |
| `interactive` | no | false | `true` sources `~/.bashrc` (aliases, mise/nvm/rbenv) via `bash -ic` |
| `title` | no | `job` | short label that becomes the job id prefix: `<title>-HH-MM-SS`. Spaces, `/`, `.` and other characters collapse to `-`. Keep it secret-free — ids appear in replies, `job(action="list")` and log filenames |

Unknown parameters are rejected, not ignored. A job whose output passes **256 MiB** is killed and finishes `failed`.

```
bash(cmd="ls -la /var/www")
bash(cmd="./deploy.sh", cwd="/srv/app", title="deploy")
bash(cmd="claude -p 'fix the failing test and push'", bg=true)    # agent runs an agent
```

### `job(action?, id?, cursor?, limit?)`
Manage jobs created by `bash`. `action` is one of `poll`, `list`, `kill`.

| Param | Required | Default | Meaning |
|---|---|---|---|
| `action` | no | `list` | `poll` \| `list` \| `kill`. A string naming anything else (`"stop"`, `"KILL"`) is an error listing the valid ones |
| `id` | for `poll`/`kill` | — | the job id |
| `cursor` | no | latest / newest | (poll) the byte offset from the previous footer; (list) the offset from `[older jobs: …]` |
| `limit` | no | 200 | (poll) max lines per page; (list) max jobs per page (at most 200) |

```
job(action="poll", id="job-23-30-07")                  # the latest output
job(action="poll", id="job-23-30-07", cursor=48213)    # older output, cursor from the footer
job(action="poll", id="job-23-30-07", limit=50)        # just the last 50 lines
job(action="list")                                     # newest 200 jobs + status
job(action="list", cursor=200)                         # the next (older) page
job(action="kill", id="job-23-30-07")
```

**Several agents on one host.** Each MCP client logs in on its own and gets its own bearer; their calls run concurrently (a slow command or job of one never holds up another). An agent is shown by the `client_name` its client registered plus a 4-hex tag that tells two clients of the same name apart (`Claude Code#4f2a`), or `client#<tag>` when it registered none. Names are cleaned before anyone sees them: control and invisible characters are dropped, whitespace collapsed, at most 32 characters.

```
job(action="list")
→ 2 agents active (last 15 min): you (Claude Code#4f2a), cursor#9b1c
  [
    { "id": "deploy-10-12-03", "state": { "status": "running" }, "by": "cursor#9b1c" },
    { "id": "build-10-11-40", "state": { "status": "exited", "code": 0 }, "by": "you" }
  ]
```

- `by` is `you`, another agent's label, or `unknown` for a job started before owners were recorded.
- An agent counts as active once it has used its bearer in the last 15 minutes (tracked per client, to the minute).
- Jobs are shared, not isolated: every agent runs as the same service user, so any agent can `poll` or `kill` any job. A reply about another agent's job opens with `[started by another agent: cursor#9b1c]`.

`kill` signals the job's whole process group (`SIGTERM`, a 2s grace, then `SIGKILL`) and only reports success once the group is verifiably gone:

| Reply | Meaning |
|---|---|
| `killed <id>` | it was running; its group is gone and the job is recorded `failed` |
| `job <id> had already finished; killed the processes it left running` | a finished job's leftovers (`server &`) were still in its group; they are gone now |
| `job <id> is not running (status: …) — nothing to kill` | error: already finished, nothing left in its group |
| `no such job: <id>` | error: unknown id |
| `… survived TERM then KILL — still running` | error: the group outlived both signals |
| `could not signal job <id>: … — it may still be running` | error: the kernel refused or the probe failed; no claim either way |
| `… has no recorded process group` / `… has a corrupt process group id` | error: nothing that can safely be signalled |

Retention: a finished job (row + log) is kept **24 hours after it finished**; its log is trimmed to its last 5000 lines meanwhile, 500 once it finished over 3 hours ago. A running job is never reaped or trimmed, however old.

## 📁 Files

All paths are on the host's local filesystem. One tool, `file`, with an `action` selecting the operation.

### `file(action, path?, content?, pattern?, recursive?, src?, dest?, cursor?, limit?)`

| `action` | Params used | What it does |
|---|---|---|
| `read` | `path`, `cursor?`, `limit?` | Read a regular file, paginated by line (default 200, at most 1000, ≤64 KiB a page). A directory points you at `list`; a device, FIFO or socket is refused. |
| `write` | `path`, `content` | Replace a file with `content` **atomically** (temp file in the same dir + rename): a reader never sees half a file. An existing file keeps its mode; writing to a symlink writes its target. Parent dirs are created. |
| `append` | `path`, `content` | Append to a file, creating it (and its parent dirs) if absent. |
| `delete` | `path`, `recursive?` | Delete a file, a symlink (the link itself — never followed, even as `link/`), or an empty directory. A **non-empty directory needs `recursive=true`**; without it nothing is deleted. |
| `list` | `path`, `recursive?` | `ls -la` a directory, or the full tree (`find`, max depth 20) when `recursive=true`. |
| `grep` | `pattern`, `path`, `recursive?` | Basic-regex grep with line numbers; a directory needs `recursive=true`. No match reads `[grep: no matches]`. |
| `move` | `src`, `dest` | Move or rename. `dest` is the full new path: an existing `dest` (file or directory) is an error, never overwritten. A regular file can cross filesystems; a directory or symlink can't (use `mv` via `bash`). |

`list`/`grep` output is capped and they time out after 30s; a capped or partial result says so in a trailing `[…]` note. Unknown parameters (`recurse`, `offset`) are rejected, not ignored.

```
file(action="read", path="/etc/nginx/nginx.conf")
file(action="read", path="/var/log/app.log", cursor=1000, limit=500)
file(action="write", path="/srv/app/.env", content="PORT=3000\n")
file(action="append", path="/var/log/deploy.log", content="deploy started\n")
file(action="delete", path="/tmp/build-cache", recursive=true)
file(action="list", path="/srv/app")
file(action="list", path="/srv/app", recursive=true)
file(action="grep", pattern="TODO", path="/srv/app/src/main.rs")
file(action="grep", pattern="password", path="/srv/app", recursive=true)
file(action="move", src="/tmp/out.tar.gz", dest="/srv/releases/out.tar.gz")
```

## ⚙️ Configuration

Config lives at `/etc/mcp-ssh/config.toml` (or override the path with `$MCP_SSH_CONFIG`). **Env vars override the file.**

| Env var | Default | Meaning |
|---|---|---|
| `MCP_SSH_BIND` | `127.0.0.1:1337` | full address to bind (config key `bind`) |
| `MCP_SSH_PORT` | `1337` | overrides just the port of the bind address |
| `MCP_SSH_CONFIG` | `/etc/mcp-ssh/config.toml` | config file path |
| `MCP_SSH_USER` | — | login username (set via `mcp-ssh set-auth`); empty counts as unset |
| `MCP_SSH_PASS` | — | login password (set via `mcp-ssh set-auth`); empty counts as unset — the server refuses to start without both |
| `MCP_SSH_INLINE_TIMEOUT_SECS` | `2` | inline window before `bash` backgrounds (clamped to 60) |
| `MCP_SSH_JOB_DIR` | `/var/lib/mcp-ssh/logs/jobs` | where per-job log files live |
| `MCP_SSH_DB` | `/var/lib/mcp-ssh/mcp-ssh.db` | SQLite state: OAuth tokens, registered clients, job history (config key `db_path`) |
| `MCP_SSH_PUBLIC_URL` | — | public origin behind the proxy (e.g. `https://your-host.example.com`), advertised as the OAuth issuer; unset ⇒ derived from the allow-listed `Host` (config key `public_url`) |
| `MCP_SSH_ALLOWED_HOSTS` | `localhost,127.0.0.1` (loopback binds only) | hostnames accepted in the `Host` header (DNS-rebinding guard) — **set this to your public hostname**. **Required** for a non-loopback bind: the server refuses to start without it |

### Listen port

Default `1337`. Three ways to change it, in precedence order:

```bash
mcp-ssh serve --port 8080        # or -p 8080
MCP_SSH_PORT=8080 mcp-ssh serve  # env, port only
MCP_SSH_BIND=127.0.0.1:8080      # full bind address (or config key `bind`)
```

`--port` / `MCP_SSH_PORT` override **only the port** of the bind address, leaving the host intact; `MCP_SSH_BIND` / config `bind` sets the whole thing.

### Job retention

Jobs and their log files are **auto-pruned**: a reaper (at startup, then hourly) drops a job 24 hours after it **finished** and trims finished jobs' logs before that. It never kills anything — a running job is never aged out — and a finished job whose leftover processes still run keeps its row until they exit. A live log is capped at 256 MiB (the job is killed past it), so one runaway command can't fill the disk.

Set credentials (don't write the password into the file by hand):

```bash
mcp-ssh set-auth admin     # prompts for the password
```

## 🔐 Authenticating

`/mcp` requires a bearer token — obtain one via the **OAuth 2.1** flow:

1. Add `https://your-host/mcp` as a remote MCP server in your client (Claude, Cursor, etc.).
2. The client discovers `/.well-known/oauth-authorization-server`, registers itself, and opens `/authorize` in your browser.
3. mcp-ssh's **login page** shows where the client will be sent. Check it's the client you started, then log in with the username/password you set via `mcp-ssh set-auth`.
4. The client receives an access token (24h) plus a refresh token (30 days, rotated on use) and renews silently; after 90 days it logs in again.

`mcp-ssh sessions --revoke` (or changing the password with `set-auth`) logs every client out. A stream that is already open closes within 30 seconds.
