---
description: Mint an OAuth bearer for a headless MCP client against a running server. Use for mcp-token, bearer, PKCE, headless client, claude mcp add, 401.
applies: [rust]
---

> House skill — established by dz-runner setup mode from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Start a server first — `/mcp` is bearer-only and `bin/mcp-token` needs a live one. Use `bin/dev` (it seeds `.env` and serves on `127.0.0.1:1337`).
2. Confirm it is up: `curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server`.
3. Credentials come from `.env` (`MCP_SSH_USER` / `MCP_SSH_PASS`) or the environment — `bin/mcp-token` sources `.env` itself. It errors `set MCP_SSH_USER (or put it in .env)` if neither is set.
4. Mint the token: `bin/mcp-token` → prints the access token to stdout. Override the base URL with `MCP_SSH_URL` when the server is not on the local bind.
5. Use it: `claude mcp add --transport http mcp-ssh http://localhost:1337/mcp --header "Authorization: Bearer $(bin/mcp-token)"`.
6. The script runs the real flow: `/register` (dynamic client registration) → `/authorize` with HTTP Basic + PKCE S256 → `/token` code exchange. The Basic header goes through a curl config on stdin (`-K -`), never argv — keep it that way so the password cannot surface in the process list.
7. Tokens and client registrations are persisted in SQLite (`src/db.rs`, `src/oauth/store.rs`), so they survive a server restart; re-run `bin/mcp-token` anyway for a fresh 24h token.
8. Failure branches: `client registration failed (server down?)` -> server is not listening, go to step 1; `no auth code (bad credentials or server down?)` -> wrong `MCP_SSH_USER`/`MCP_SSH_PASS`.
9. Never print or commit the token.
