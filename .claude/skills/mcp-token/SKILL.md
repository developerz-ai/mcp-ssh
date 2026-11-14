---
description: Mint an OAuth bearer for headless MCP clients against a running server. Use for mcp-token, bearer, headless, claude cli, auth, pkce
applies: [rust]
---

> House skill — established by the developerz.ai maintainer agent for the maintainers of this repository from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Start the server locally: `MCP_SSH_USER=<dev-user> MCP_SSH_PASS=<dev-pass> MCP_SSH_DB="$PWD/.local/mcp-ssh.db" MCP_SSH_JOB_DIR="$PWD/.local/jobs" bin/dev` (binds 127.0.0.1:1337; no root dirs needed).
2. Verify up: `curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server`.
3. Mint the bearer: `bin/mcp-token` (reads MCP_SSH_USER/MCP_SSH_PASS from .env or the environment; set MCP_SSH_URL to target a non-local server).
4. Hand it to a headless client: `claude mcp add --transport http mcp-ssh http://localhost:1337/mcp --header "Authorization: Bearer $(bin/mcp-token)"`.
fail -> "client registration failed (server down?)" or "no consent form from /authorize" means the server is not up; "login refused (HTTP 401)" means wrong MCP_SSH_USER/MCP_SSH_PASS.
The access token lasts 24h and survives server restarts (stored hashed in SQLite); re-run after it expires, or after `mcp-ssh sessions --revoke` / `set-auth`. Never echo the token into logs or commits.
