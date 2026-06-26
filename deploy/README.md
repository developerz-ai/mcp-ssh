# Deployment

## The `.deb` package

Built with `cargo deb` (see the metadata note below). Installing it:

- places the binary at `/usr/bin/mcp-ssh`
- installs the systemd unit at `/lib/systemd/system/mcp-ssh.service`
- registers the unit with systemd (`systemd-units` integration)

After install:

```sh
sudo systemctl enable --now mcp-ssh
```

## Configuration

- Config file: `/etc/mcp-ssh/config.toml`
- Env overrides: `/etc/mcp-ssh/mcp-ssh.env` (read by the systemd unit via
  `EnvironmentFile=`). See `.env.example` in the repo root for the full list
  of `MCP_SSH_*` vars.
- Set auth credentials: `mcp-ssh set-auth <user>`

## TLS / reverse proxy

The server binds `127.0.0.1:1337` and serves MCP at `/mcp`. Put a reverse
proxy in front for TLS:

- Caddy (auto-HTTPS): see `Caddyfile.example`
- nginx + certbot: see `nginx-mcp-ssh.conf.example`

## NOTE: required Cargo.toml metadata

`cargo deb` needs a `[package.metadata.deb]` section in `Cargo.toml`. The
maintainer must add it (not added here). Suggested contents:

```toml
[package.metadata.deb]
maintainer = "developerz-ai <admin@venom.is>"
depends = "$auto"
assets = [
    ["target/release/mcp-ssh", "usr/bin/", "755"],
    ["deploy/mcp-ssh.service", "lib/systemd/system/", "644"],
]
systemd-units = { unit-name = "mcp-ssh", enable = false, start = false }
```
