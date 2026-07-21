# mcp-ssh

A single Rust binary that acts as an MCP server, giving an AI agent remote shell and file access to **one** host — the box the service runs on. It speaks authenticated MCP Streamable HTTP at `/mcp` and executes commands locally as the service user, running as a systemd service. The pitch in the repo's own words: "ssh, but over `/mcp` from any MCP client." It serves AI agents (Claude and other MCP clients) that need durable, auditable shell access to a remote machine without handing out SSH keys.

- **Stack:** Rust 2024 (pinned via `rust-toolchain.toml`), tokio, axum 0.8, rmcp 1.7 for MCP Streamable HTTP, SQLite via `rusqlite` (bundled) for durable OAuth tokens + job metadata, thiserror/anyhow, tracing. Auth is OAuth 2.1 plus HTTP Basic; TLS is terminated by a reverse proxy, not the binary. Deployed as a systemd service (Dockerfile and `deploy/` present).
- **Key commands:**
  - `cargo build` — build
  - `bin/dev` — run locally with watch + reload, loads `.env`
  - `bin/check` — full gate: `fmt --check` + `clippy -D warnings` + tests
  - `cargo test` / `cargo test <pattern>` — tests
  - `cargo run -- serve` — run the server
  - `bin/mcp-token` — mint a bearer for a headless client via OAuth PKCE
- **Layout:**
  - `src/main.rs`, `src/cli.rs`, `src/config.rs` — entry point, clap CLI (`serve`/`set-auth`/`jobs`/`job kill`/`sessions`), config loading
  - `src/jobs/` — job engine: run commands, stream output to per-job log files, persist metadata, plus `reaper.rs` for cleanup and `id.rs` for secret-free job ids
  - `src/oauth/` — minimal OAuth 2.1 server (discovery, dynamic client registration, PKCE authorize/token, bearer validation)
  - `src/tools/` — the MCP tool surface: three fat tools (`bash`/`job`/`file`) dispatching on `action`
  - `src/db.rs`, `src/auth.rs`, `src/admin.rs` — SQLite state layer, Basic-auth middleware, local admin subcommands
  - `deploy/`, `docs/`, `bin/` — deployment assets, documentation, dev scripts
- **State as of 2026-07-21:** on branch `task/16-admin-kill-signalling-test`; working tree was clean when this note was written.
