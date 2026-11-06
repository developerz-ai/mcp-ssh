# AI-first scorecard

> Written by the developerz.ai maintainer agent — established by dz-runner setup mode. Each onboarding run rewrites this file whole; the latest run wins, and the same card is stored on the platform.

- Run: `run_883496399aa342b8885ae9eebc00958f`
- Computed: 2026-11-06T23:47:31.207Z
- Proof: **unprovable** — `bin/check` did not run green at head on this box. Nothing in the repository’s own code was changed to force it; the excerpt below is the record.
- Score: 10 of 15 items ok

## `policy` — ok

- .dz/maintainer/maintainer.yml established
- .dz/maintainer/reviewer.md established

## `brain` — ok

- CLAUDE.md already present, left alone

## `roster` — ok

- .claude/agents/job-supervision.md established
- .claude/agents/oauth-pkce.md established
- .claude/agents/sqlite-persistence.md established
- .claude/agents/mcp-tools.md established

## `skills` — ok

- .claude/skills/verify-change/SKILL.md established
- .claude/skills/sqlite-migrations/SKILL.md established
- .claude/skills/release-cut/SKILL.md established
- .claude/skills/bearer-token-mint/SKILL.md established

## `scripts` — unprovable

- bin/setup already present, left alone
- bin/dev already present, left alone
- bin/check already present, left alone
- bin/stop established
- down: ./bin/stop (bin)
- ./bin/setup at head: red, exit 1 (0.0s)

## `wiring` — ok

- .mcp.json established

## `commands` — ok

- .claude/commands already present, left alone

## `access` — gap

- landed 0 of 9 asked
- still missing: MCP_SSH_USER, MCP_SSH_PASS, SENTRY_DSN, SENTRY_ENVIRONMENT, MCP_SSH_ALLOWED_HOSTS, MCP_SSH_BIND, MCP_SSH_JOB_DIR, MCP_SSH_INLINE_TIMEOUT_SECS, RUST_LOG

## `layout` — gap

- migration to the house layout filed as tasks
- apps/mcp-ssh: 10 move(s) planned as one task

## `smoke_test` — unprovable

- src/app_tests.rs in the repository
- ./bin/check at head: red, exit 1 (0.0s)

## `env_example` — ok

- .env.example already present, left alone

## `agents_md` — ok

- AGENTS.md established

## `per_workspace_brains` — ok

- single-package repository: 0 workspaces
- CLAUDE.md already present, left alone

## `docs_readme` — ok

- docs/README.md established

## `gate_proved` — unprovable

- ./bin/check at head: red, exit 1 (0.0s)
