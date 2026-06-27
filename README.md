# 🔌 mcp-ssh

> **ssh, but you talk to it over `/mcp` from any MCP client.**
> A single Rust binary that gives an AI agent a remote shell + file access to **one host — the box it runs on.** No SSH client, no multi-server fan-out, no gateway. It runs commands **locally**, as the service user, and speaks MCP over HTTP.

You point Claude (or any MCP client) at `https://your-host/mcp`, it authenticates, and now the agent can `bash`, read/write files, and supervise long-running jobs on that machine — from anywhere.

## 🚗 The story

You're in your car. From your phone, you point Claude at `https://your-vps/mcp` and say *"run the deploy."*

- Claude calls `bash("./deploy.sh")`.
- The deploy takes 20 minutes — so it **auto-backgrounds** and hands back a job id instead of blocking.
- Claude polls `job(action="poll", id)` **page by page**, watching progress a few hundred lines at a time, so a 20-minute build never floods its context window.
- It finishes. Claude tells you it's done. You never touched a keyboard.

Want to go further? Run `bash("claude -p 'fix the failing test and push'")` — **one agent supervising another agent** on your VPS. mcp-ssh is just the shell; what you run through it is up to you.

## ⚡ Quickstart

**One-liner (Debian/Ubuntu)** — downloads the latest release, asks for a username + password, installs the service, and starts it:

```bash
curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/deploy/install.sh | sudo bash
```

<details><summary>Or do it by hand</summary>

```bash
# 1. install (Debian/Ubuntu — grab the .deb from releases)
sudo dpkg -i mcp-ssh_*.deb

# 2. set the single username/password (prompts for the password)
mcp-ssh set-auth admin

# 3. start it as a systemd service
sudo systemctl enable --now mcp-ssh

# 4. verify it's up on loopback
curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server
```

</details>

Then put TLS in front (see [docs/deploy.md](docs/deploy.md)) and connect from Claude.

mcp-ssh now listens on `127.0.0.1:1337` at `/mcp`. Expose it as `https://your-host/mcp` with a reverse proxy → **[docs/deploy.md](docs/deploy.md)**.

## 🧰 The tools

A small, heavily-parametrized surface — **three resource-oriented tools**, composition pushed into params. Everything runs locally as the service user.

| Tool | Params | What it does |
|---|---|---|
| `bash` | `cmd`, `cwd?`, `timeout?`, `bg?`, `interactive?`, `title?` | Run a shell command. Returns output inline if it finishes within the inline window (default 2s), else a **job id** to monitor with `job`. `timeout` overrides the inline window; `bg=true` backgrounds immediately; `interactive=true` sources `~/.bashrc`; `title` labels the job id (`<title>-HH:MM:SS`). Output is byte/line-capped per page. |
| `job` | `action`, `id?`, `cursor?`, `limit?` | Manage jobs. `action="poll"` → status + **one page** of merged stdout+stderr (default 200 lines, byte-capped, with `next_cursor`/`has_more`); `action="list"` → all jobs + status; `action="kill"` → kill running job `id`. |
| `file` | `action`, `path?`, `content?`, `pattern?`, `recursive?`, `src?`, `dest?`, `cursor?`, `limit?` | File operations by `action`: `read` (paginated), `write`, `append`, `delete`, `list` (`recursive` for the tree), `grep` (`pattern`, `recursive` under a dir), `move` (`src`→`dest`). |

Full reference with examples → **[docs/usage.md](docs/usage.md)**.

## 🔗 Connect from Claude

1. Deploy mcp-ssh behind TLS so it's reachable at `https://your-host/mcp`.
2. In Claude, add a remote MCP server with URL `https://your-host/mcp`.
3. Claude runs the **OAuth 2.1** flow (the spec-compliant auth GUI clients use); log in with the username/password you set via `mcp-ssh set-auth`.
4. The tools above appear. Say *"run the deploy."*

Headless client (the `claude` CLI, curl) with no browser? Mint a bearer with [`bin/mcp-token`](bin/mcp-token) and pass it as `Authorization: Bearer …` → **[docs/deploy.md](docs/deploy.md#-connect-a-client)**.

## 🔐 Auth

`/mcp` is **bearer-only** — all MCP clients must authenticate via OAuth 2.1. Claude and every
spec-compliant GUI client run this flow automatically; you just log in with the username/password
you set via `mcp-ssh set-auth`.

Set the credentials once:

```bash
mcp-ssh set-auth admin     # prompts for the password
```

## 🖥️ CLI

```bash
mcp-ssh serve              # run the server (this is the default)
mcp-ssh set-auth <user>    # configure the username/password
```

## ⚠️ Security

**This gives an agent full shell access — with `sudo` (root) by default.** The
unit ships `NoNewPrivileges=false` and the installer grants the run user
`NOPASSWD:ALL`, so the agent can self-manage the host (update + restart itself,
manage services). Anyone who authenticates to `/mcp` can run anything as root.
Treat it accordingly:

- Run it as a **dedicated user** (the installer defaults to `mcp-ssh`), not your login account.
- Always put it **behind TLS** (reverse proxy). Never expose `:1337` directly.
- Use a **strong password** — it's the only thing between the internet and root.
- Set `MCP_SSH_ALLOWED_HOSTS` to your public hostname — it's the DNS-rebinding guard.
- Don't want root? [Lock it down](docs/deploy.md#self-management-the-agent-has-sudo-by-default) — remove the sudoers file + set `NoNewPrivileges=true`.

## 📦 Install

| Method | How |
|---|---|
| **One-liner** | `curl -fsSL https://raw.githubusercontent.com/developerz-ai/mcp-ssh/main/deploy/install.sh \| sudo bash` — latest release, prompts for creds, installs + starts the service |
| **Debian/Ubuntu** | download `mcp-ssh_*.deb` from [releases](https://github.com/developerz-ai/mcp-ssh/releases) → `sudo dpkg -i mcp-ssh_*.deb` |
| **Docker** | pull the image and run it (see [docs/deploy.md](docs/deploy.md)) |
| **From source** | `cargo build --release` → binary at `target/release/mcp-ssh` |

## 📚 Docs

| Doc | What's in it |
|---|---|
| [docs/connect-claude.md](docs/connect-claude.md) | Connect from Claude Desktop & mobile — custom-connector setup, OAuth login, troubleshooting |
| [docs/usage.md](docs/usage.md) | Every tool with params + examples, the execution & pagination model, config & env vars |
| [docs/architecture.md](docs/architecture.md) | Module map, auto-backgrounding execution, the auth middleware, the stack |
| [docs/deploy.md](docs/deploy.md) | systemd, Caddy & nginx+certbot TLS, Docker, hardening |
| [docs/prompts/system-prompt.md](docs/prompts/system-prompt.md) | Ready-to-paste system prompt for driving the server from a chat LLM |
| [docs/prompts/skill.md](docs/prompts/skill.md) | Claude Code skill for autonomous server-side work |
| [`.coderabbit.yaml`](.coderabbit.yaml) | CodeRabbit AI review config; install the [GitHub App](https://github.com/apps/coderabbitai) on the repo |

## 🧬 Stack

Rust 2024 · tokio · axum 0.8 · [rmcp](https://github.com/modelcontextprotocol/rust-sdk) 1.7 (MCP Streamable HTTP).

## 📄 License

MIT. Repository: <https://github.com/developerz-ai/mcp-ssh>.
