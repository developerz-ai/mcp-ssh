# 06 — Server runtime, packaging, deploy

> Part of [`overview.md`](overview.md). Depends on: 02 (no external `kill`), 03 (file modes), 04 (`public_url`, revoke on `set-auth`).

TLS, multi-host routing and rate limiting stay in the reverse proxy — this slice documents proxy config, it adds none of them to `src/`.

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| R1 | HIGH | SIGTERM with one open SSE stream: process still alive after 30 s (`axum::serve(...).with_graceful_shutdown` waits for connections forever). Under systemd ⇒ every restart/self-update waits `TimeoutStopSec` (90 s) then SIGKILL — no clean drain, WAL not checkpointed | [E2E] idle: exits in 0 s; one `GET /mcp` stream: >30 s. `src/main.rs:110-112` |
| R2 | HIGH | Documented Docker run can't work: default bind is `127.0.0.1:1337` **inside** the container, `-p 127.0.0.1:1337:1337` publishes to nothing; no `MCP_SSH_BIND` in the image or the example | [READ] `Dockerfile`, `docs/deploy.md:185-190`, `src/config.rs:93` — confirm with slice 07's image smoke test |
| R3 | HIGH | Docker image: no `kill` binary (see 02 K1), no `VOLUME`/guidance for `/var/lib/mcp-ssh` (tokens + history lost on recreate), no `HEALTHCHECK` (though `/healthz` exists), service user has `--no-create-home` + `nologin` ⇒ `interactive=true` has no `~/.bashrc` | [E2E] `kill` absent in `debian:bookworm-slim`; rest [READ] |
| R4 | MED | "By hand" install in `docs/deploy.md:22-35` / `README.md:35-41` can't start: the `.deb` creates no `mcp-ssh` user (no maintainer scripts) ⇒ unit fails `217/USER`; `mcp-ssh set-auth` without sudo can't write `/etc/mcp-ssh`, with sudo writes root:root 0600 the service can't read | [READ] `Cargo.toml` deb metadata, `deploy/mcp-ssh.service:8-9`, `src/config.rs:188-204` |
| R5 | MED | Unit: no `UMask=0077`/`StateDirectoryMode=0700` (see 03 D1); `KillMode`/`TimeoutStopSec` implicit; `ProtectSystem=full` + the advertised "agent self-manages via sudo" — sudo children inherit the read-only `/usr` `/boot` `/etc` namespace, so `sudo apt install` / editing `/etc` from the tool likely fails | modes [E2E]; ProtectSystem [PLAUS] — verify on a box |
| R6 | MED | `install.sh`: `systemctl enable --now` doesn't restart a running service ⇒ rerun doesn't apply new creds/hosts (header says it does); arch from `dpkg --print-architecture` but releases ship **amd64 only** ⇒ arm64 dies with "no .deb asset"; `.deb` fetched with no checksum/signature check and installed as root; `PUBLIC_HOST` written to the env file unvalidated; never sets `public_url` | [READ]; release assets [E2E] `mcp-ssh_1.4.0-1_amd64.deb` only |
| R7 | MED | Proxy examples ship no rate limit on `/authorize` `/token` `/register` although the design assigns throttling to the proxy; nginx example doesn't restrict to the expected `Host` | [READ] `deploy/Caddyfile.example`, `deploy/nginx-mcp-ssh.conf.example` |
| R8 | LOW | No request timeout/body-limit layer on the OAuth router beyond axum's 2 MiB default; no limit visible on `/mcp` bodies | [READ] `src/app.rs`; `/mcp` limit [PLAUS] |
| R9 | LOW | Sentry scrubber covers message/exception/breadcrumbs/extra only — not `request`, `tags`, `contexts`, `user`, `logentry`, stack-frame `vars`. `server_name` (hostname) is sent by default. Only `main` errors + panics are captured; `tracing::error!` isn't | [READ] `src/sentry.rs:132-158` |
| R10 | LOW | `serve` hardcodes `Shell::interactive_bash()` into `JobStore::new` — fine if it's just the "interactive" variant holder; confirm it isn't the default (e2e shows default is `sh`) | [E2E] default `$0`=`sh`, interactive=`bash` — naming only |
| R11 | LOW | `set-auth` writes the config non-atomically (truncate + write) | [READ] `src/config.rs:209-220` |
| R12 | LOW | `inline_timeout` has no upper bound; `public_url` unvalidated (scheme, trailing slash) | [READ] `src/config.rs:100-105,148` |

## Files to change
- `src/main.rs:108-113,132-164` — bounded drain.
- `src/app.rs` — expose a shutdown handle for sessions if rmcp offers one; body-limit/timeout layers on the OAuth router.
- `src/config.rs` — validate `public_url` + `inline_timeout`; atomic `set_auth` write (temp + rename, 0600 from creation).
- `src/sentry.rs:132-158` — scrub remaining event fields; `server_name: None`.
- `Dockerfile` — bind, volume, healthcheck, home dir, drop the "kill" comment.
- `deploy/mcp-ssh.service` — `UMask`, `StateDirectoryMode`, explicit `KillMode`, `TimeoutStopSec`, the `ProtectSystem` decision.
- `Cargo.toml` `[package.metadata.deb]` + new `deploy/debian/postinst` — create user, config dir.
- `deploy/install.sh` — restart, arch guard, checksum, host validation, `public_url`.
- `deploy/Caddyfile.example`, `deploy/nginx-mcp-ssh.conf.example`, `docs/deploy.md` — rate limit + corrected steps.

## Steps
1. **Drain deadline** (R1): after `shutdown_signal()` fires, give in-flight requests N seconds (recommend 10), then drop the server future (`tokio::select!` between the graceful serve and a `sleep` started by the signal). Check whether rmcp 2.2's `StreamableHttpServerConfig`/`LocalSessionManager` exposes a cancellation token to close SSE streams proactively — if so, cancel it on signal so the drain usually finishes early. Log `drained` vs `drain deadline hit`. Set `TimeoutStopSec=20` in the unit to match.
2. **Docker** (R2, R3): `ENV MCP_SSH_BIND=0.0.0.0:1337`; this makes `MCP_SSH_ALLOWED_HOSTS` mandatory (existing `AllowedHostsRequired` check — good; document it); `VOLUME /var/lib/mcp-ssh`; `HEALTHCHECK` needs an HTTP client in the image — add a `mcp-ssh healthcheck`-free option: use `bash`'s `/dev/tcp` in the HEALTHCHECK (`bash -c 'exec 3<>/dev/tcp/127.0.0.1/1337'`), no new dependency or subcommand; `useradd --create-home --shell /bin/bash`. Fix `docs/deploy.md` Docker block (add `-v`, note the bind).
3. **`.deb`** (R4): `maintainer-scripts = "deploy/debian/"` with a `postinst` that creates the `mcp-ssh` system user/group if absent and `install -d -o mcp-ssh -g mcp-ssh -m 750 /etc/mcp-ssh`. `set_auth`: when run as root and the config dir is owned by another user, chown the written file to that owner (keeps 0600 readable by the service) — or document `sudo -u mcp-ssh mcp-ssh set-auth`. Recommend the chown: the documented command then just works. `docs/deploy.md` step 2 becomes `sudo mcp-ssh set-auth admin`.
4. **Unit** (R5): add `UMask=0077`, `StateDirectoryMode=0700`, `KillMode=control-group` (explicit — jobs die with the service; say so), `TimeoutStopSec=20`. `ProtectSystem`: first verify on a box (`bash("sudo touch /usr/local/x")` through the service). If it fails, choose: drop `ProtectSystem` in the sudo-enabled profile (honest with the stated design) and keep it in the documented lockdown drop-in (`docs/deploy.md:117-121`).
5. **Installer** (R6): `systemctl enable mcp-ssh && systemctl restart mcp-ssh`; fail early with a clear message on an arch with no asset (until 07 ships arm64); download `SHA256SUMS` from the release and verify before install (07 publishes it); validate `PUBLIC_HOST` against `^[A-Za-z0-9.-]+(:[0-9]+)?$`; write `MCP_SSH_PUBLIC_URL=https://$PUBLIC_HOST` to the env file.
6. **Proxy examples** (R7): nginx `limit_req_zone` + `limit_req` on `/authorize`, `/token`, `/register`; Caddy equivalent needs a plugin — document that and give the nginx form as the reference; explicit `server_name`/host match so stray Hosts never reach the app. Add a "rate limiting" line to the hardening checklist in `docs/deploy.md`.
7. **Limits** (R8): `DefaultBodyLimit` of 64 KiB on the OAuth router (register caps from 04 make this natural); verify rmcp's own body limit on `/mcp` and document the proxy `client_max_body_size`.
8. **Sentry** (R9): scrub `request` (drop headers/cookies/query/data outright), `tags`, `contexts`, `user`, `logentry`; set `server_name` to `None`. Don't wire `tracing` → Sentry unless asked.
9. **Config** (R11, R12): atomic `set_auth`; `public_url` must parse as `http(s)://host[:port]` with no path — normalise the trailing slash; clamp `inline_timeout` to ≤ 60 s.

## Tests
- `main.rs`: with a held-open connection, shutdown resolves within the deadline (pattern: existing `sigterm_resolves_shutdown_signal`, `src/main.rs:188-209`).
- `config.rs`: bad `public_url` shapes rejected; `set_auth` atomicity (pre-existing content intact when the temp write fails); root-chown path behind a seam.
- `sentry.rs`: event with `request.headers.authorization`, a secret tag, `user.ip_address` ⇒ all gone after `scrub_event`.
- Image/`.deb` behaviour is covered by 07's smoke tests (container answers `/healthz` on the published port; `.deb` installs and the unit starts in a container).
- `shellcheck deploy/install.sh bin/*`. Gate: `bin/check`.

## Done when
- `systemctl restart mcp-ssh` returns in seconds with clients connected.
- `docker run` exactly as documented yields a reachable server whose state survives `docker rm` + re-run with the volume.
- `dpkg -i` + the documented by-hand steps start the service.
- Installer rerun applies new settings; refuses unsupported arch clearly; verifies the download.
