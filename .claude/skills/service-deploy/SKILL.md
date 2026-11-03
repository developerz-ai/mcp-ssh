---
description: Installing and running mcp-ssh as a systemd service with TLS. Use for deploy, systemd, install.sh, caddy, nginx, certbot, hardening.
applies: [rust]
---

> House skill — established by dz-runner setup mode from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Build and install the package on the target Debian/Ubuntu host: `sudo dpkg -i mcp-ssh_*.deb` (the one-liner `curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/deploy/install.sh | sudo bash` does this plus the service setup).
2. Set the single credential: `mcp-ssh set-auth admin` — it prompts for the password and writes `/etc/mcp-ssh/config.toml` mode 0600. Fail -> stop; the server refuses to start without credentials.
3. Set the DNS-rebinding guard before exposing anything: `MCP_SSH_ALLOWED_HOSTS` must list the public hostname(s). Off-loopback bind with the guard unset or empty is refused at boot.
4. Start the unit: `sudo systemctl enable --now mcp-ssh`. The unit runs as the dedicated `mcp-ssh` user with `StateDirectory=mcp-ssh`; credentials live in `/etc/mcp-ssh/config.toml`, optional env overrides in `/etc/mcp-ssh/mcp-ssh.env`.
5. Verify on loopback: `curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server` must return the discovery JSON, and `curl -fsS http://127.0.0.1:1337/healthz` must return `ok`.
6. Confirm `/mcp` is bearer-only: `curl -i http://127.0.0.1:1337/mcp` without credentials must return 401.
7. Put TLS in front — Caddy (`deploy/Caddyfile.example`) or nginx + certbot (`deploy/nginx-mcp-ssh.conf.example`). Never expose `:1337` directly to the internet.
8. Check the service's own state with `mcp-ssh jobs --all`, `mcp-ssh job kill <id>` and `mcp-ssh sessions` — these read the same SQLite the server uses and need no auth.
9. Lock down if the agent must not self-manage the host: set `NoNewPrivileges=true` in the unit and remove `/etc/sudoers.d/mcp-ssh`, then `sudo systemctl daemon-reload && sudo systemctl restart mcp-ssh`.
