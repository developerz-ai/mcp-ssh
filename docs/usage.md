# 🧰 Usage

Every tool, its parameters, and a short example. All tools execute **locally on the host running mcp-ssh, as the service user.**

## 🏃 Execution model — inline, then background

`bash` does not block. It waits a short **inline window** (default 2s) for the command to finish:

| Outcome | What you get back |
|---|---|
| Finishes within the window | Status + the first page of output, **inline** |
| Still running after the window | A **job id** (`j1`, `j2`, …) — poll it with `job_poll` |

This is what makes a 20-minute build safe: it backgrounds automatically, and you stream the log a page at a time instead of dumping it all into context.

```
bash("cargo build --release")
→ "job j7 still running after the inline window. Poll it with job_poll(id=\"j7\")."

job_poll("j7")
→ {"status":"running"}
  Compiling mcp-ssh v0.1.0
  ...200 lines...
  [lines ..200 of 540; next_cursor=200]

job_poll("j7", cursor=200)
→ ...next page...

job_poll("j7", cursor=540)
→ {"status":"exited","code":0}
  Finished `release` profile [optimized] target(s)
```

Override the window with `timeout` when you *want* to wait longer inline (e.g. a command you know takes ~5s):

```
bash("npm test", timeout=10)     # wait up to 10s before backgrounding
```

## 📄 Pagination model

Anything that can produce a lot of text — job logs and `file_read` — is **paginated by line** with `cursor` + `limit` (default 200 lines):

- `cursor` — line offset to start from (default 0).
- `limit` — max lines to return (default 200).
- Responses report `next_cursor`, `total_lines`, and `has_more` so you walk forward until `has_more` is false.

Output and stderr are merged into one stream, terminal-style.

## 🐚 Shell & jobs

### `bash(cmd, cwd?, timeout?)`
Run a shell command (`sh -c`). Returns output inline if it finishes within the inline window, else a job id.

| Param | Required | Default | Meaning |
|---|---|---|---|
| `cmd` | yes | — | the shell command |
| `cwd` | no | process cwd | working directory |
| `timeout` | no | 2s | seconds to wait inline before backgrounding |

```
bash("ls -la /var/www")
bash("./deploy.sh", cwd="/srv/app")
bash("claude -p 'fix the failing test and push'")    # agent runs an agent
```

### `job_poll(id, cursor?, limit?)`
Status + one page of a job's output.

```
job_poll("j7")                  # first page
job_poll("j7", cursor=200)      # next page
job_poll("j7", cursor=400, limit=500)
```

### `job_list()`
All jobs with their status (`running` / `exited` / `failed`).

```
job_list()
```

### `job_kill(id)`
Kill a running job.

```
job_kill("j7")
```

## 📁 Files

All paths are on the host's local filesystem.

### `file_read(path, cursor?, limit?)`
Read a file, paginated by line (default 200).

```
file_read("/etc/nginx/nginx.conf")
file_read("/var/log/app.log", cursor=1000, limit=500)
```

### `file_write(path, content)`
Create or **truncate** a file with `content`.

```
file_write("/srv/app/.env", "PORT=3000\n")
```

### `file_append(path, content)`
Append to a file, creating it if absent.

```
file_append("/var/log/deploy.log", "deploy started\n")
```

### `file_delete(path)`
Delete a file **or directory** (recursive for dirs).

```
file_delete("/tmp/build-cache")
```

### `file_list(path, recursive?)`
`ls -la` a directory, or the full tree (`find`) when `recursive=true`.

```
file_list("/srv/app")
file_list("/srv/app", recursive=true)
```

### `file_grep(pattern, path, recursive?)`
Grep with line numbers. `recursive=true` searches under a directory.

```
file_grep("TODO", "/srv/app/src/main.rs")
file_grep("password", "/srv/app", recursive=true)
```

### `file_move(src, dest)`
Move or rename a file or directory.

```
file_move("/tmp/out.tar.gz", "/srv/releases/out.tar.gz")
```

## ⚙️ Configuration

Config lives at `/etc/mcp-ssh/config.toml` (or `$XDG_CONFIG_HOME/mcp-ssh/config.toml`). **Env vars override the file.**

| Env var | Default | Meaning |
|---|---|---|
| `MCP_SSH_BIND` | `127.0.0.1:1337` | address to bind |
| `MCP_SSH_USER` | — | Basic/OAuth username (set via `mcp-ssh set-auth`) |
| `MCP_SSH_PASS` | — | password (set via `mcp-ssh set-auth`) |
| `MCP_SSH_INLINE_TIMEOUT_SECS` | `2` | inline window before `bash` backgrounds |
| `MCP_SSH_JOB_DIR` | `/tmp/mcp-ssh-jobs` | where per-job log files live |
| `MCP_SSH_ALLOWED_HOSTS` | `localhost,127.0.0.1` | hostnames accepted in the `Host` header (DNS-rebinding guard) — **set this to your public hostname** |

Set credentials (don't write the password into the file by hand):

```bash
mcp-ssh set-auth admin     # prompts for the password
```

## 🔐 Authenticating

| Client | Mode |
|---|---|
| Claude, GUI MCP clients | **OAuth 2.1** — driven automatically by the client; log in with your username/password |
| curl, scripts | **HTTP Basic** — `-u user:pass` |

```bash
curl -u admin:secret https://your-host/mcp -d @request.json
```
