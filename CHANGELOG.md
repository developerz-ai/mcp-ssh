# Changelog

All notable changes to mcp-ssh are recorded here. Format: [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versioning: [Semantic Versioning](https://semver.org/).

Each release's notes live in their own file under [`docs/changelog/`](docs/changelog/); this page is the index, newest first. Not released yet: [`docs/changelog/unreleased.md`](docs/changelog/unreleased.md) ([diff since the last release][Unreleased]). A pull request with a user-visible change adds its line there.

## [2.1.0] - 2026-11-14

rmcp 3.9, and the follow-ups from the 2.0.0 audit: kills from a persisted row, the log cap on leftover processes, logging of refused tool calls, and open streams ending when their token is revoked. The database schema moves to version 6. [Release notes](docs/changelog/2.1.0.md).

## [2.0.0] - 2026-11-14

A major release: the fixes from the deep-dive audit — bounded output, real kills, hashed tokens and a consent-page login, safe file ops. Several are deliberate behaviour changes; read **Upgrading** first. [Release notes](docs/changelog/2.0.0.md).

## [1.4.0] - 2026-07-21

Audit fixes across files, jobs, OAuth and auth, a `/healthz` endpoint, and the job engine split into focused modules. [Release notes](docs/changelog/1.4.0.md).

## [1.3.0] - 2026-07-02

GlitchTip error tracking and a round of correctness fixes in jobs, files, auth and config. [Release notes](docs/changelog/1.3.0.md).

## [1.2.4] - 2026-06-27

`job(action="poll")` is newest-first: the latest output is the first page. [Release notes](docs/changelog/1.2.4.md).

## [1.2.3] - 2026-06-27

Admin CLI: `mcp-ssh jobs`, `mcp-ssh job kill` and `mcp-ssh sessions`. [Release notes](docs/changelog/1.2.3.md).

## [1.2.2] - 2026-06-27

Clients no longer fail `job`/`file` with `invalid type ... expected string or map`. [Release notes](docs/changelog/1.2.2.md).

## [1.2.1] - 2026-06-27

A null or missing `job` action is tolerated. [Release notes](docs/changelog/1.2.1.md).

## [1.2.0] - 2026-06-27

Durable state: OAuth tokens and job metadata live in SQLite and survive a restart; refresh tokens. [Release notes](docs/changelog/1.2.0.md).

## [1.1.5] - 2026-06-27

File `write`, `append` and `move` create missing parent directories. [Release notes](docs/changelog/1.1.5.md).

## [1.1.4] - 2026-06-27

Tiered job-log compaction keeps disk use bounded over long uptime. [Release notes](docs/changelog/1.1.4.md).

## [1.1.3] - 2026-06-27

Job titles and bounded output everywhere. [Release notes](docs/changelog/1.1.3.md).

## [1.1.2] - 2026-06-27

The agent can self-manage the host through `sudo`. [Release notes](docs/changelog/1.1.2.md).

## [1.1.1] - 2026-06-27

The `job` action renders as a flat enum so Claude Desktop can call it. [Release notes](docs/changelog/1.1.1.md).

## [1.1.0] - 2026-06-27

Opt-in interactive bash and a one-shot installer. [Release notes](docs/changelog/1.1.0.md).

## [1.0.0] - 2026-06-26

First stable release: an MCP server giving an agent shell, file and job access to one host over authenticated Streamable HTTP. [Release notes](docs/changelog/1.0.0.md).

[Unreleased]: https://github.com/developerz-ai/mcp-ssh/compare/v2.1.0...HEAD
[2.1.0]: https://github.com/developerz-ai/mcp-ssh/compare/v2.0.0...v2.1.0
[2.0.0]: https://github.com/developerz-ai/mcp-ssh/compare/v1.4.0...v2.0.0
[1.4.0]: https://github.com/developerz-ai/mcp-ssh/compare/v1.3.0...v1.4.0
[1.3.0]: https://github.com/developerz-ai/mcp-ssh/compare/v1.2.4...v1.3.0
[1.2.4]: https://github.com/developerz-ai/mcp-ssh/compare/v1.2.3...v1.2.4
[1.2.3]: https://github.com/developerz-ai/mcp-ssh/compare/v1.2.2...v1.2.3
[1.2.2]: https://github.com/developerz-ai/mcp-ssh/compare/v1.2.1...v1.2.2
[1.2.1]: https://github.com/developerz-ai/mcp-ssh/compare/v1.2.0...v1.2.1
[1.2.0]: https://github.com/developerz-ai/mcp-ssh/compare/v1.1.5...v1.2.0
[1.1.5]: https://github.com/developerz-ai/mcp-ssh/compare/v1.1.4...v1.1.5
[1.1.4]: https://github.com/developerz-ai/mcp-ssh/compare/v1.1.3...v1.1.4
[1.1.3]: https://github.com/developerz-ai/mcp-ssh/compare/v1.1.2...v1.1.3
[1.1.2]: https://github.com/developerz-ai/mcp-ssh/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/developerz-ai/mcp-ssh/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/developerz-ai/mcp-ssh/compare/v1.0.0...v1.1.0
[1.0.0]: https://github.com/developerz-ai/mcp-ssh/releases/tag/v1.0.0
