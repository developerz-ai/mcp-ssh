# AI-first scorecard

> This card is the repository's own — established by the developerz.ai maintainer agent for the maintainers of this repository; each onboarding run rewrites the file whole, the latest run wins, and the same card is stored on the platform.

- Run: `run_d9c8969d57374e4787c2c649e16e35cf`
- Computed: 2026-11-14T00:39:18.914Z
- Proof: **proved** — every command ran green at head.
- Score: 13 of 15 items ok

## `policy` — ok

- .dz/maintainer/maintainer.yml established
- .dz/maintainer/reviewer.md established

## `brain` — ok

- CLAUDE.md already present, left alone

## `roster` — ok

- .claude/agents/jobs-engine.md established
- .claude/agents/oauth-auth.md established
- .claude/agents/mcp-tools.md established
- .claude/agents/sqlite-state.md established

## `skills` — ok

- .claude/skills/verify-change/SKILL.md established
- .claude/skills/release-cut/SKILL.md established
- .claude/skills/mcp-token/SKILL.md established

## `scripts` — ok

- bin/setup already present, left alone
- bin/dev already present, left alone
- bin/check already present, left alone
- bin/stop established
- down: ./bin/stop (bin)
- ./bin/setup exited 0 at head (0.4s)

## `wiring` — ok

- .mcp.json established

## `commands` — ok

- .claude/commands already present, left alone

## `access` — gap

- landed 2 of 12 asked: SENTRY_ACCESS_TOKEN, SENTRY_HOST
- still missing: MCP_SSH_USER, MCP_SSH_PASS, MCP_SSH_DB, MCP_SSH_JOB_DIR, MCP_SSH_BIND, MCP_SSH_ALLOWED_HOSTS, MCP_SSH_INLINE_TIMEOUT_SECS, RUST_LOG, SENTRY_DSN, SENTRY_ENVIRONMENT

## `layout` — gap

- migration to the house layout filed as tasks
- apps/mcp-ssh: 10 move(s) planned as one task

## `smoke_test` — ok

- src/tools/files/tests.rs in the repository
- run by ./bin/check exited 0 at head (3.7s)

## `env_example` — ok

- .env.example already present, left alone

## `agents_md` — ok

- AGENTS.md established

## `per_workspace_brains` — ok

- single-package repository: 0 workspaces
- CLAUDE.md already present, left alone

## `docs_readme` — ok

- docs/README.md established

## `gate_proved` — ok

- ./bin/check exited 0 at head (3.7s)
