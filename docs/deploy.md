# 🚀 Deploy

mcp-ssh binds `127.0.0.1:1337` and serves `/mcp` over plain HTTP. **A reverse proxy terminates TLS** and exposes `https://your-host/mcp`. TLS is not in the binary by design.

> **Golden rule:** bind to loopback, proxy `https://your-host/mcp → 127.0.0.1:1337`, never expose `:1337` to the internet.

## ⚡ Install (Debian/Ubuntu)

```bash
# 1. install from the .deb (GitHub releases)
sudo dpkg -i mcp-ssh_*.deb

# 2. set the single username/password (prompts for password)
mcp-ssh set-auth admin

# 3. enable + start the systemd service
sudo systemctl enable --now mcp-ssh

# 4. check it's up on loopback
sudo systemctl status mcp-ssh
curl -u admin:secret http://127.0.0.1:1337/mcp
```

The `.deb` installs the binary, a systemd unit, and `/etc/mcp-ssh/config.toml`.

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

## 🔒 TLS — option A: Caddy (recommended, auto-HTTPS)

Caddy fetches and renews certificates automatically. Smallest possible config:

```caddyfile
# /etc/caddy/Caddyfile
your-host.example.com {
    reverse_proxy /mcp* 127.0.0.1:1337
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

    location /mcp {
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
| `MCP_SSH_JOB_DIR` | `/var/lib/mcp-ssh/jobs` | per-job log files |
| `MCP_SSH_ALLOWED_HOSTS` | `localhost,127.0.0.1` | hostnames accepted in `Host` — **set to your public hostname** |

## ⚠️ Hardening checklist

- [ ] Runs as a **dedicated low-privilege user** (not root).
- [ ] Bound to `127.0.0.1` — `:1337` is **not** reachable from the internet.
- [ ] **TLS** in front; clients only ever hit `https://your-host/mcp`.
- [ ] **Strong password** set via `mcp-ssh set-auth`.
- [ ] `MCP_SSH_ALLOWED_HOSTS` = your public hostname (DNS-rebinding guard).
- [ ] Firewall allows only 443 inbound.

## 🔗 Connect a client

Once `https://your-host/mcp` is live:

- **Claude / GUI clients** — add a remote MCP server with that URL; it runs the **OAuth 2.1** flow, log in with your `set-auth` credentials.
- **curl / scripts** — **HTTP Basic**:

  ```bash
  curl -u admin:secret https://your-host.example.com/mcp -d @request.json
  ```

Tool reference → [usage.md](usage.md).
