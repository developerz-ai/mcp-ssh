# 🧰 Usage

Three tools — `bash`, `job`, `file` — each heavily parametrized. Their parameters and a short example follow. All tools execute **locally on the host running mcp-ssh, as the service user.**

## 🏃 Execution model — inline, then background

`bash` does not block. It waits a short **inline window** (default 2s) for the command to finish:

| Outcome | What you get back |
|---|---|
| Finishes within the window | Status + the first page of output, **inline** |
| Still running after the window | A **job id** (e.g., `job-23:30`) — monitor it with `job(action="poll")` |

This is what makes a 20-minute build safe: it backgrounds automatically, and you stream the log a page at a time instead of dumping it all into context.

```text
bash("cargo build --release")
→ "job job-23:30 still running after the inline window. Poll it with job(action=\"poll\", id=\"job-23:30\")."

job(action="poll", id="job-23:30")
→ {"status":"running"}
  Compiling mcp-ssh v0.1.0
  ...200 lines...
  [lines ..200 of 540; next_cursor=200]

job(action="poll", id="job-23:30", cursor=200)
→ ...next 200 lines...
  [lines ..400 of 540; next_cursor=400]

job(action="poll", id="job-23:30", cursor=400)
→ {"status":"exited","code":0}
  Finished `release` profile [optimized] target(s)
```

Override the window with `timeout` when you *want* to wait longer inline (e.g. a command you know takes ~5s), or skip waiting entirely with `bg=true`:

```
bash("npm test", timeout=10)     # wait up to 10s before backgrounding
bash("./deploy.sh", bg=true)     # background immediately, return the job id now
```

## 📄 Pagination model

Anything that can produce a lot of text — job logs (`job(action="poll")`) and `file(action="read")` — is **paginated by line** with `cursor` + `limit` (default 200 lines):

- `cursor` — line offset to start from (default 0).
- `limit` — max lines to return (default 200).
- Responses report `next_cursor`, `total_lines`, and `has_more` so you walk forward until `has_more` is false.

Output and stderr are merged into one stream, terminal-style.

## 🐚 Shell & jobs

### `bash(cmd, cwd?, timeout?, bg?, interactive?)`
Run a shell command. By default it's a fast bare `sh -c`. Pass `interactive=true` to run it in an **interactive bash** (`bash -ic`) that sources your `~/.bashrc`, so aliases and version managers (`mise`, `nvm`, `rbenv`) work just like a normal shell. Returns output inline if it finishes within the inline window, else a job id to monitor with `job`.

| Param | Required | Default | Meaning |
|---|---|---|---|
| `cmd` | yes | — | the shell command |
| `cwd` | no | process cwd | working directory |
| `timeout` | no | 2s | seconds to wait inline before backgrounding |
| `bg` | no | false | `true` backgrounds immediately, returning the job id without waiting |
| `interactive` | no | false | `true` sources `~/.bashrc` (aliases, mise/nvm/rbenv) via `bash -ic` |

```
bash("ls -la /var/www")
bash("./deploy.sh", cwd="/srv/app")
bash("claude -p 'fix the failing test and push'")    # agent runs an agent
bash("./long-build.sh", bg=true)                     # don't wait, just hand back the id
```

### `job(action, id?, cursor?, limit?)`
Manage jobs created by `bash`. `action` is one of `poll`, `list`, `kill`.

| Param | Required | Default | Meaning |
|---|---|---|---|
| `action` | yes | — | `poll` \| `list` \| `kill` |
| `id` | for `poll`/`kill` | — | the job id |
| `cursor` | no | 0 | (poll) line offset to start from |
| `limit` | no | 200 | (poll) max lines to return |

`action="poll"` returns the job's status plus **one page** of merged stdout+stderr, with `next_cursor`/`has_more` so you walk long logs forward without flooding context. `action="list"` returns all jobs with their status (`running` / `exited` / `failed`). `action="kill"` kills a running job.

```
job(action="poll", id="job-23:30")                       # first page
job(action="poll", id="job-23:30", cursor=200)           # next page
job(action="poll", id="job-23:30", cursor=400, limit=500)
job(action="list")                                                # all jobs + status
job(action="kill", id="job-23:30")                       # kill a running job
```

## 📁 Files

All paths are on the host's local filesystem. One tool, `file`, with an `action` selecting the operation.

### `file(action, path?, content?, pattern?, recursive?, src?, dest?, cursor?, limit?)`

| `action` | Params used | What it does |
|---|---|---|
| `read` | `path`, `cursor?`, `limit?` | Read a file, paginated by line (default 200). |
| `write` | `path`, `content` | Create or **truncate** a file with `content`. |
| `append` | `path`, `content` | Append to a file, creating it if absent. |
| `delete` | `path` | Delete a file **or directory** (recursive for dirs). |
| `list` | `path`, `recursive?` | `ls -la` a directory, or the full tree (`find`) when `recursive=true`. |
| `grep` | `pattern`, `path`, `recursive?` | Grep with line numbers; `recursive=true` searches under a directory. |
| `move` | `src`, `dest` | Move or rename a file or directory. |

```
file(action="read", path="/etc/nginx/nginx.conf")
file(action="read", path="/var/log/app.log", cursor=1000, limit=500)
file(action="write", path="/srv/app/.env", content="PORT=3000\n")
file(action="append", path="/var/log/deploy.log", content="deploy started\n")
file(action="delete", path="/tmp/build-cache")
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
| `MCP_SSH_USER` | — | Basic/OAuth username (set via `mcp-ssh set-auth`) |
| `MCP_SSH_PASS` | — | password (set via `mcp-ssh set-auth`) |
| `MCP_SSH_INLINE_TIMEOUT_SECS` | `2` | inline window before `bash` backgrounds |
| `MCP_SSH_JOB_DIR` | `/var/lib/mcp-ssh/jobs` | where per-job log files live |
| `MCP_SSH_ALLOWED_HOSTS` | `localhost,127.0.0.1` | hostnames accepted in the `Host` header (DNS-rebinding guard) — **set this to your public hostname** |

### Listen port

Default `1337`. Three ways to change it, in precedence order:

```bash
mcp-ssh serve --port 8080        # or -p 8080
MCP_SSH_PORT=8080 mcp-ssh serve  # env, port only
MCP_SSH_BIND=127.0.0.1:8080      # full bind address (or config key `bind`)
```

`--port` / `MCP_SSH_PORT` override **only the port** of the bind address, leaving the host intact; `MCP_SSH_BIND` / config `bind` sets the whole thing.

### Job retention

Jobs and their log files are **auto-pruned**: an hourly reaper drops any job older than 24 hours. No manual cleanup, no unbounded log growth under the job dir.

Set credentials (don't write the password into the file by hand):

```bash
mcp-ssh set-auth admin     # prompts for the password
```

## 🔐 Authenticating

`/mcp` requires a bearer token — obtain one via the **OAuth 2.1** flow:

1. Add `https://your-host/mcp` as a remote MCP server in your client (Claude, Cursor, etc.).
2. The client discovers `/.well-known/oauth-authorization-server` and opens the `/authorize` page.
3. Log in with the username/password you set via `mcp-ssh set-auth`.
4. The client receives a bearer token and uses it for all subsequent `/mcp` requests.
