# AI-first scorecard

> Written by the developerz.ai maintainer agent — established by dz-runner setup mode. Each onboarding run rewrites this file whole; the latest run wins, and the same card is stored on the platform.

- Run: `run_d02686b693c14f48a68ed0e932a99159`
- Computed: 2026-11-03T16:32:20.631Z
- Proof: **proved with gaps** — `bin/check` ran green at head; the head commands that did not are named below.
- Score: 12 of 15 items ok

## `policy` — ok

- .dz/maintainer/maintainer.yml established
- .dz/maintainer/reviewer.md established

## `brain` — ok

- CLAUDE.md already present, left alone

## `roster` — ok

- .claude/agents/job-engine.md established
- .claude/agents/oauth-flow.md established
- .claude/agents/sqlite-state.md established
- .claude/agents/mcp-tool-surface.md established

## `skills` — ok

- .claude/skills/verify-change/SKILL.md established
- .claude/skills/release-cut/SKILL.md established
- .claude/skills/plan-writing/SKILL.md established
- .claude/skills/service-deploy/SKILL.md established

## `scripts` — unprovable

- bin/setup already present, left alone
- bin/dev repaired
- bin/check repaired
- bin/stop established
- down: ./bin/stop (bin)
- ./bin/setup at head: red, exit 1 (0.1s)

## `wiring` — ok

- .mcp.json established

## `commands` — ok

- .claude/commands already present, left alone

## `access` — gap

- landed 2 of 16 asked: SENTRY_ACCESS_TOKEN, SENTRY_HOST
- still missing: MCP_SSH_USER, MCP_SSH_PASS, MCP_SSH_BIND, MCP_SSH_PORT, MCP_SSH_CONFIG, MCP_SSH_DB, MCP_SSH_JOB_DIR, MCP_SSH_INLINE_TIMEOUT_SECS, MCP_SSH_ALLOWED_HOSTS, MCP_SSH_PUBLIC_URL, MCP_SSH_URL, RUST_LOG, SENTRY_DSN, SENTRY_ENVIRONMENT

## `layout` — gap

- migration to the house layout filed as tasks
- apps/mcp-ssh: 10 move(s) planned as one task

## `smoke_test` — ok

- src/app_tests.rs in the repository
- run by ./bin/check exited 0 at head (5.4s)

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

- ./bin/check exited 0 at head (5.4s)
