# 🚀 Deploy

mcp-ssh binds `127.0.0.1:1337` and serves `/mcp` **plus the OAuth 2.1 login routes** over plain HTTP. **A reverse proxy terminates TLS** and exposes them at `https://your-host`. TLS is not in the binary by design.

> **Golden rule:** bind to loopback, proxy `https://your-host → 127.0.0.1:1337` (the `/mcp` tool endpoint **and** the OAuth routes), never expose `:1337` to the internet.

The login flow needs more than `/mcp` on the public origin: both the browser OAuth flow (Claude desktop/web) and the headless [`bin/mcp-token`](../bin/mcp-token) flow call `/.well-known/oauth-authorization-server`, `/.well-known/oauth-protected-resource`, `/authorize`, `/token`, and `/register`. The proxy snippets below forward every path to the backend so these don't 404 — see [Connect a client](#-connect-a-client).

## ⚡ Install (Debian/Ubuntu)

### One-liner (recommended)

Downloads the latest release for your arch, prompts for a username + password, asks which OS user to run as, writes the config + a systemd drop-in, and starts the service:

```bash
curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/deploy/install.sh | sudo bash
```

Re-running it just updates the binary and re-applies your answers. Source: [`deploy/install.sh`](../deploy/install.sh).

### By hand

```bash
# 1. install from the .deb (GitHub releases)
sudo dpkg -i mcp-ssh_*.deb

# 2. set the single username/password (prompts for password)
mcp-ssh set-auth admin

# 3. enable + start the systemd service
sudo systemctl enable --now mcp-ssh

# 4. check it's up on loopback
sudo systemctl status mcp-ssh
curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server
```

The `.deb` installs the binary, a systemd unit, and `/etc/mcp-ssh/config.toml`.

## 🩺 Verify / debug with curl

All checks hit the loopback bind (`127.0.0.1:1337`); swap in `https://your-host` once the proxy is up.

```bash
# 1. OAuth discovery returns JSON ⇒ server is up
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

The packaged unit runs `mcp-ssh serve`. Run it as a **dedicated low-privilege user** and lock it down:

```ini
# /etc/systemd/system/mcp-ssh.service
[Unit]
Description=mcp-ssh — remote shell + file access over MCP-HTTP
After=network.target

[Service]
User=mcp-ssh
Group=mcp-ssh
ExecStart=/usr/bin/mcp-ssh serve
Restart=on-failure
Environment=MCP_SSH_BIND=127.0.0.1:1337
Environment=MCP_SSH_ALLOWED_HOSTS=your-host.example.com

[Install]
WantedBy=multi-user.target
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

**To lock it down** (no root for the agent):

```bash
sudo rm -f /etc/sudoers.d/mcp-ssh
sudo install -d -m 755 /etc/systemd/system/mcp-ssh.service.d
printf '[Service]\nNoNewPrivileges=true\n' \
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

Done — `https://your-host.example.com/mcp` is live with a valid cert.

## 🔒 TLS — option B: nginx + certbot

```bash
# get a cert
sudo certbot --nginx -d your-host.example.com
```

```nginx
# /etc/nginx/sites-available/mcp-ssh
server {
    listen 443 ssl;
    server_name your-host.example.com;

    ssl_certificate     /etc/letsencrypt/live/your-host.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/your-host.example.com/privkey.pem;

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

> `proxy_buffering off` and a long `proxy_read_timeout` keep streaming responses and long polls working.

## 🐳 Docker

```bash
docker run -d --name mcp-ssh \
  -p 127.0.0.1:1337:1337 \
  -e MCP_SSH_USER=admin \
  -e MCP_SSH_PASS=secret \
  -e MCP_SSH_ALLOWED_HOSTS=your-host.example.com \
  ghcr.io/developerz-ai/mcp-ssh:latest
```

Then point Caddy/nginx at `127.0.0.1:1337` exactly as above. Note: inside a container the "host" the agent controls is **the container**, not the VPS.

## 🛠️ Build from source

```bash
cargo build --release          # binary at target/release/mcp-ssh
./target/release/mcp-ssh serve
```

Requires Rust 1.85+ (edition 2024).

## ⚙️ Config & env

File: `/etc/mcp-ssh/config.toml` (or override the path with `$MCP_SSH_CONFIG`). **Env vars override the file** — handy in systemd units and containers.

| Env var | Default | Meaning |
|---|---|---|
| `MCP_SSH_BIND` | `127.0.0.1:1337` | bind address — keep on loopback behind the proxy |
| `MCP_SSH_USER` / `MCP_SSH_PASS` | — | credentials (prefer `mcp-ssh set-auth`) |
| `MCP_SSH_INLINE_TIMEOUT_SECS` | `2` | inline window before `bash` backgrounds |
| `MCP_SSH_JOB_DIR` | `/var/lib/mcp-ssh/logs/jobs` | per-job log files |
| `MCP_SSH_ALLOWED_HOSTS` | `localhost,127.0.0.1` | hostnames accepted in `Host` — **set to your public hostname** |

### 💾 Durable state (SQLite)

OAuth tokens and job history are persisted to a **SQLite database** at
`/var/lib/mcp-ssh/mcp-ssh.db` (sibling of the job-log dir, under the systemd
`StateDirectory`). It's **auto-created on first run** in WAL mode — no config or
env var; SQLite is compiled into the binary (`rusqlite` bundled), so there's no
system `libsqlite` to install. Because tokens survive a restart, clients stay
logged in across service restarts and self-updates.

**Backup:** it's a normal SQLite file — `sqlite3 /var/lib/mcp-ssh/mcp-ssh.db '.backup /path/backup.db'`
(or just copy the file while the service is stopped). Losing it only forces a
re-login and drops job history; nothing else depends on it.

## ⚠️ Hardening checklist

- [ ] Runs as a **dedicated low-privilege user** (not root).
- [ ] Bound to `127.0.0.1` — `:1337` is **not** reachable from the internet.
- [ ] **TLS** in front; clients only ever hit `https://your-host/mcp`.
- [ ] **Strong password** set via `mcp-ssh set-auth`.
- [ ] `MCP_SSH_ALLOWED_HOSTS` = your public hostname (DNS-rebinding guard).
- [ ] Firewall allows only 443 inbound.

## 🔗 Connect a client

Once `https://your-host/mcp` is live:

- **GUI MCP clients** (Claude desktop/web) — add the URL; the client discovers
  `/.well-known/oauth-authorization-server`, drives the **OAuth 2.1** flow in a browser, and
  logs in with your `set-auth` credentials. `/mcp` is **bearer-only**; there is no HTTP Basic
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

  Access tokens last 24h; the `/token` response also returns a 1-year
  `refresh_token` (rotated on use) for silent renewal. Tokens are persisted in
  SQLite, so they **survive a service restart** — no need to re-run `bin/mcp-token`
  after every restart. For a stable long-lived setup, prefer a GUI client's OAuth flow.

Tool reference → [usage.md](usage.md).
