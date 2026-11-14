# 🚀 Deploy

mcp-ssh binds `127.0.0.1:1337` and serves `/mcp` **plus the OAuth 2.1 login routes** over plain HTTP. **A reverse proxy terminates TLS** and exposes them at `https://your-host`. TLS is not in the binary by design.

> **Golden rule:** bind to loopback, proxy `https://your-host → 127.0.0.1:1337` (the `/mcp` tool endpoint **and** the OAuth routes), never expose `:1337` to the internet.

The login flow needs more than `/mcp` on the public origin: both the browser OAuth flow (Claude desktop/web) and the headless [`bin/mcp-token`](../bin/mcp-token) flow call `/.well-known/oauth-authorization-server`, `/.well-known/oauth-protected-resource`, `/authorize`, `/token`, and `/register`. The proxy snippets below forward every path to the backend so these don't 404 — see [Connect a client](#-connect-a-client).

## ⚡ Install (Debian/Ubuntu)

### One-liner (recommended)

Downloads the latest release for your arch (amd64 or arm64), verifies it against the release's `SHA256SUMS`, prompts for a username + password, asks which OS user to run as and the public hostname (validated; sets `MCP_SSH_ALLOWED_HOSTS` and `MCP_SSH_PUBLIC_URL`), writes the config + a systemd drop-in, and (re)starts the service:

```bash
curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/apps/mcp-ssh/deploy/install.sh | sudo bash
```

Re-running it updates the binary, re-applies your answers and restarts the service so they take effect. Source: [`deploy/install.sh`](../apps/mcp-ssh/deploy/install.sh).

### By hand

```bash
# 1. install from the .deb (GitHub releases)
sudo dpkg -i mcp-ssh_*.deb

# 2. set the single username/password (prompts for password); sudo because
#    /etc/mcp-ssh is not world-writable — the file is handed to the service user
sudo mcp-ssh set-auth admin

# 3. enable + start the systemd service
sudo systemctl enable --now mcp-ssh

# 4. check it's up on loopback
sudo systemctl status mcp-ssh
curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server
```

The `.deb` installs the binary and a systemd unit, creates the `mcp-ssh` system user/group the unit runs as (home `/home/mcp-ssh`, shell `bash` so `interactive=true` finds a `~/.bashrc`), and creates `/etc/mcp-ssh` (`0750`, owned by `mcp-ssh`). `set-auth` writes `/etc/mcp-ssh/config.toml` (`0600`). The by-hand path grants **no** sudo — see [Self-management](#self-management-the-agent-has-sudo-by-default).

## 🩺 Verify / debug with curl

All checks hit the loopback bind (`127.0.0.1:1337`); swap in `https://your-host` once the proxy is up.

```bash
# 1. liveness ⇒ server is up; discovery JSON ⇒ the OAuth login surface is up
curl -fsS http://127.0.0.1:1337/healthz && echo # → ok
curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server | jq .

# 2. /mcp is bearer-only — no creds ⇒ 401
curl -s -o /dev/null -w '%{http_code}\n' -X POST http://127.0.0.1:1337/mcp   # → 401

# 3. Mint a bearer from your username/password (runs the OAuth PKCE flow).
#    `bin/mcp-token` ships in the source checkout, NOT the .deb — run it from a
#    repo clone with MCP_SSH_USER/MCP_SSH_PASS in env or .env, or skip to a GUI
#    client's browser OAuth (see Connect a client).
read -rp 'MCP_SSH_USER: ' MCP_SSH_USER
read -rsp 'MCP_SSH_PASS: ' MCP_SSH_PASS; echo
TOKEN="$(MCP_SSH_USER="$MCP_SSH_USER" MCP_SSH_PASS="$MCP_SSH_PASS" bin/mcp-token)"
unset MCP_SSH_PASS

# 4. initialize a session — look for the `mcp-session-id:` response header
curl -sS -D - -o /dev/null -X POST http://127.0.0.1:1337/mcp \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -H 'Host: localhost' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"curl","version":"0"}}}'
```

Discovery 404s behind your proxy ⇒ it isn't forwarding `/.well-known/*`, `/authorize`, `/token`, `/register` (see [Connect a client](#-connect-a-client)). Logs: `journalctl -u mcp-ssh -e`.

## 🔧 systemd

The packaged unit ([`deploy/mcp-ssh.service`](../apps/mcp-ssh/deploy/mcp-ssh.service)) runs `mcp-ssh serve` as the dedicated `mcp-ssh` user. Notable settings:

- `StateDirectory=mcp-ssh` + `StateDirectoryMode=0700` — `/var/lib/mcp-ssh` is private to the service user, and the server creates the DB and job logs `0600` itself. There is deliberately no `UMask=`: agent commands (and their `sudo` children) inherit the service's umask, so a restrictive one would make files the agent writes elsewhere owner-only.
- `KillMode=control-group` — stopping or restarting the service kills **every** process it started, background jobs included. Jobs do not outlive a restart.
- `TimeoutStopSec=20` — the server drains in-flight requests for up to 10 s on SIGTERM, then exits; systemd SIGKILLs at 20 s.
- `NoNewPrivileges=false`, no `ProtectSystem` — so the agent can self-manage via `sudo` (below).

Put overrides in `/etc/mcp-ssh/mcp-ssh.env` (read via `EnvironmentFile=`), e.g.:

```bash
MCP_SSH_ALLOWED_HOSTS=localhost,127.0.0.1,your-host.example.com
MCP_SSH_PUBLIC_URL=https://your-host.example.com
```

```bash
sudo systemctl daemon-reload
sudo systemctl restart mcp-ssh
journalctl -u mcp-ssh -f          # follow logs
```

> ⚠️ The service user **is** the agent's shell user. Whatever it can do, the agent can do. Give it the least privilege the job needs.

### Self-management: the agent has `sudo` by default

**By design, the agent's shell is full-power.** The shipped unit sets
`NoNewPrivileges=false` and the installer grants the run user
`NOPASSWD:ALL` in `/etc/sudoers.d/mcp-ssh`, so the agent can install its own
updates, restart itself, and manage the host as root. That means **anyone who
authenticates to `/mcp` can run anything as root** — keep the password strong and
TLS in front.

Verify it's active: `sudo -n true` succeeds inside the shell, and
`grep NoNewPrivs /proc/$(systemctl show mcp-ssh -p MainPID --value)/status` reads `0`.

The default unit sets no `ProtectSystem`: it mounts `/usr`, `/boot` and `/etc` read-only in the service's mount namespace, and `sudo` children inherit that namespace — so `sudo apt install` or editing `/etc` from the tool would fail even as root.

**To lock it down** (no root for the agent, read-only system dirs):

```bash
sudo rm -f /etc/sudoers.d/mcp-ssh
sudo install -d -m 755 /etc/systemd/system/mcp-ssh.service.d
printf '[Service]\nNoNewPrivileges=true\nProtectSystem=full\n' \
  | sudo tee /etc/systemd/system/mcp-ssh.service.d/20-lockdown.conf
sudo systemctl daemon-reload && sudo systemctl restart mcp-ssh
```

Or scope the sudoers line to just the commands the agent needs instead of
`NOPASSWD:ALL`.

## 🔒 TLS — option A: Caddy (recommended, auto-HTTPS)

Caddy fetches and renews certificates automatically. Smallest possible config:

```caddyfile
# /etc/caddy/Caddyfile
your-host.example.com {
    # Proxy every path: mcp-ssh serves /mcp plus the OAuth 2.1 endpoints
    # (/.well-known/oauth-*, /authorize, /token, /register) the login flow needs.
    reverse_proxy 127.0.0.1:1337
}
```

```bash
sudo systemctl reload caddy
```

Done — `https://your-host.example.com/mcp` is live with a valid cert. Caddy only answers for the site address, so stray `Host`s never reach mcp-ssh.

> **Rate limiting:** mcp-ssh leaves throttling of the login endpoints (`/authorize`, `/token`, `/register`) to the proxy. Stock Caddy has no rate limiter; build it with the [`caddy-ratelimit`](https://github.com/mholt/caddy-ratelimit) plugin (`xcaddy`), or use nginx (option B), whose config below is the reference.

## 🔒 TLS — option B: nginx + certbot

```bash
# get a cert
sudo certbot --nginx -d your-host.example.com
```

```nginx
# /etc/nginx/sites-available/mcp-ssh
# Throttle the login endpoints per client IP (password guessing on /authorize).
limit_req_zone $binary_remote_addr zone=mcp_ssh_auth:10m rate=10r/m;

# Catch-all: requests for any other Host never reach mcp-ssh.
server {
    listen 443 ssl default_server;
    server_name _;
    ssl_reject_handshake on;
}

server {
    listen 443 ssl;
    server_name your-host.example.com;

    ssl_certificate     /etc/letsencrypt/live/your-host.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/your-host.example.com/privkey.pem;

    client_max_body_size 16m;     # tool calls carry file content

    location ~ ^/(authorize|token|register)$ {
        limit_req zone=mcp_ssh_auth burst=5 nodelay;
        limit_req_status 429;
        proxy_pass http://127.0.0.1:1337;
        proxy_set_header Host $host;
    }

    # Proxy every path: mcp-ssh serves /mcp plus the OAuth 2.1 endpoints
    # (/.well-known/oauth-*, /authorize, /token, /register) the login flow needs.
    location / {
        proxy_pass http://127.0.0.1:1337;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Connection "";
        proxy_buffering off;          # stream MCP responses
        proxy_read_timeout 3600s;     # long-running jobs
    }
}
```

```bash
sudo ln -s /etc/nginx/sites-available/mcp-ssh /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
```

> `proxy_buffering off` and a long `proxy_read_timeout` keep streaming responses and long polls working. Full example: [`deploy/nginx-mcp-ssh.conf.example`](../apps/mcp-ssh/deploy/nginx-mcp-ssh.conf.example).

## 🐳 Docker

```bash
docker run -d --name mcp-ssh \
  -p 127.0.0.1:1337:1337 \
  -v mcp-ssh-state:/var/lib/mcp-ssh \
  -e MCP_SSH_USER=admin \
  -e MCP_SSH_PASS=secret \
  -e MCP_SSH_ALLOWED_HOSTS=localhost,127.0.0.1,your-host.example.com \
  -e MCP_SSH_PUBLIC_URL=https://your-host.example.com \
  ghcr.io/developerz-ai/mcp-ssh:latest
```

- The image binds `0.0.0.0:1337` **inside** the container (`MCP_SSH_BIND`) — loopback there is unreachable from `-p`. That non-loopback bind makes `MCP_SSH_ALLOWED_HOSTS` **required**: the server refuses to start without it. Keep `-p 127.0.0.1:…` so the host only exposes it on loopback.
- `-v mcp-ssh-state:/var/lib/mcp-ssh` keeps OAuth tokens, registered clients and job history across `docker rm` / image upgrades. Without it every recreate logs clients out.
- The image has a `HEALTHCHECK` on `/healthz` (`docker ps` shows `healthy`).
- Images are multi-arch (`linux/amd64`, `linux/arm64`).

Then point Caddy/nginx at `127.0.0.1:1337` exactly as above. Note: inside a container the "host" the agent controls is **the container**, not the VPS.

## 🛠️ Build from source

```bash
cd apps/mcp-ssh && cargo build --release   # binary lands at the repo-root target/release/mcp-ssh
../target/release/mcp-ssh serve
```

The toolchain is pinned in [`rust-toolchain.toml`](../apps/mcp-ssh/rust-toolchain.toml) (Rust 1.100.0); `rustup` picks it up automatically. Releases are built with `--locked`.

## ⚙️ Config & env

File: `/etc/mcp-ssh/config.toml` (or override the path with `$MCP_SSH_CONFIG`). **Env vars override the file** — handy in systemd units and containers.

| Env var | Default | Meaning |
|---|---|---|
| `MCP_SSH_CONFIG` | `/etc/mcp-ssh/config.toml` | config file path |
| `MCP_SSH_BIND` | `127.0.0.1:1337` (image: `0.0.0.0:1337`) | bind address — keep on loopback behind the proxy |
| `MCP_SSH_PORT` | — | overrides just the port of the bind address (same as `serve --port`) |
| `MCP_SSH_USER` / `MCP_SSH_PASS` | — | credentials (prefer `mcp-ssh set-auth`) |
| `MCP_SSH_INLINE_TIMEOUT_SECS` | `2` | inline window before `bash` backgrounds |
| `MCP_SSH_JOB_DIR` | `/var/lib/mcp-ssh/logs/jobs` | per-job log files |
| `MCP_SSH_DB` | `/var/lib/mcp-ssh/mcp-ssh.db` | SQLite state DB (file key `db_path`) |
| `MCP_SSH_PUBLIC_URL` | — | public origin behind the proxy, e.g. `https://your-host.example.com` — the OAuth issuer/endpoint base advertised in discovery (file key `public_url`) |
| `MCP_SSH_ALLOWED_HOSTS` | `localhost,127.0.0.1` (loopback binds only) | hostnames accepted in `Host` — **set to your public hostname**. **Required** when `MCP_SSH_BIND` is non-loopback (e.g. `0.0.0.0`): startup fails without it |
| `SENTRY_DSN` | — | GlitchTip ingest DSN; injected via the systemd EnvironmentFile (`/etc/mcp-ssh/mcp-ssh.env`). Unset/empty ⇒ error tracking disabled |
| `SENTRY_ENVIRONMENT` | `development` | GlitchTip environment tag; injected via the same env file |

### 💾 Durable state (SQLite)

OAuth tokens and job history are persisted to a **SQLite database** at
`/var/lib/mcp-ssh/mcp-ssh.db` by default (sibling of the job-log dir, under the
systemd `StateDirectory`, mode `0700`); override with `MCP_SSH_DB` or `db_path`.
It's **auto-created on first run** in WAL mode; SQLite is compiled into the
binary (`rusqlite` bundled), so there's no system `libsqlite` to install. Because tokens survive a restart, clients stay
logged in across service restarts and self-updates.

**Backup:** it's a normal SQLite file — `sqlite3 /var/lib/mcp-ssh/mcp-ssh.db '.backup /path/backup.db'`
(or just copy the file while the service is stopped). Losing it only forces a
re-login and drops job history; nothing else depends on it.

## ⚠️ Hardening checklist

- [ ] Runs as a **dedicated low-privilege user** (not root).
- [ ] Bound to `127.0.0.1` — `:1337` is **not** reachable from the internet. (Docker: `0.0.0.0` inside the container is required; publish with `-p 127.0.0.1:1337:1337`.)
- [ ] **TLS** in front; clients only ever hit `https://your-host/mcp`.
- [ ] **Strong password** set via `mcp-ssh set-auth`.
- [ ] `MCP_SSH_ALLOWED_HOSTS` = your public hostname (DNS-rebinding guard).
- [ ] **Rate limiting** on `/authorize`, `/token`, `/register` at the proxy (nginx `limit_req`, or Caddy with the rate-limit plugin).
- [ ] Firewall allows only 443 inbound.

## 🔗 Connect a client

Once `https://your-host/mcp` is live:

- **GUI MCP clients** (Claude desktop/web) — add the URL; the client discovers
  `/.well-known/oauth-authorization-server`, drives the **OAuth 2.1** flow in a browser, and
  you log in with your `set-auth` credentials on mcp-ssh's consent page (it shows where the
  code will be sent; a code is issued only when that form is submitted). `/mcp` is **bearer-only**; there is no HTTP Basic
  fallback on this endpoint.

- **Headless / CLI clients** (the `claude` CLI, curl, scripts) have no browser to run the
  flow. Mint a bearer token non-interactively with [`bin/mcp-token`](../bin/mcp-token) — it
  runs the same Authorization-Code + PKCE flow against a running server using your
  `MCP_SSH_USER`/`MCP_SSH_PASS` — then pass it as a header:

  ```bash
  claude mcp add --transport http mcp-ssh https://your-host/mcp \
    --header "Authorization: Bearer $(MCP_SSH_URL=https://your-host bin/mcp-token)"

  claude mcp list          # mcp-ssh: ... ✔ Connected
  ```

  Access tokens last 24h; the `/token` response also returns a 30-day
  `refresh_token` (rotated on use; a login's refresh chain ends after 90 days)
  for silent renewal. Tokens are persisted in
  SQLite, so they **survive a service restart** — no need to re-run `bin/mcp-token`
  after every restart. For a stable long-lived setup, prefer a GUI client's OAuth flow.

- **Logging everyone out** — `sudo mcp-ssh sessions` summarises live logins (counts and
  expiries, never token values); `sudo mcp-ssh sessions --revoke` ends every one of them.
  Every `set-auth` run revokes every session too (restart the service to apply the new credentials).

Tool reference → [usage.md](usage.md).
