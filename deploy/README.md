# Deployment

Full guide: [`docs/deploy.md`](../docs/deploy.md). This directory holds the files it references.

| File | What |
|---|---|
| `install.sh` | one-shot installer (`curl … \| sudo bash`): verifies the release `.deb` against `SHA256SUMS`, installs, writes config, restarts the service |
| `mcp-ssh.service` | systemd unit shipped in the `.deb` |
| `debian/postinst` | `.deb` maintainer script: creates the `mcp-ssh` user/group and `/etc/mcp-ssh` (0750) |
| `Caddyfile.example` | Caddy reverse proxy (auto-HTTPS) |
| `nginx-mcp-ssh.conf.example` | nginx reverse proxy with login-endpoint rate limiting — the reference proxy config |

## The `.deb` package

Built with `cargo deb --locked` from the `[package.metadata.deb]` section of
`Cargo.toml`. CI builds and smoke-tests amd64 and arm64 on every run and publishes them on `v*` tags. Installing it:

- places the binary at `/usr/bin/mcp-ssh`
- installs the systemd unit at `/lib/systemd/system/mcp-ssh.service`
- creates the `mcp-ssh` system user/group and `/etc/mcp-ssh` (`postinst`)

After install:

```sh
sudo mcp-ssh set-auth admin
sudo systemctl enable --now mcp-ssh
```

## Configuration

- Config file: `/etc/mcp-ssh/config.toml`
- Env overrides: `/etc/mcp-ssh/mcp-ssh.env` (read by the systemd unit via
  `EnvironmentFile=`). See `.env.example` in the repo root and the env table in
  `docs/deploy.md` for the full list of `MCP_SSH_*` vars.

## TLS / reverse proxy

The server binds `127.0.0.1:1337` and serves MCP at `/mcp`. Put a reverse
proxy in front for TLS:

- Caddy (auto-HTTPS): see `Caddyfile.example`
- nginx + certbot: see `nginx-mcp-ssh.conf.example`
