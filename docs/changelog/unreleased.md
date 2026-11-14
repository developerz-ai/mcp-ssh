# Unreleased

### Added

- **Agents sharing a host can see each other.** `job(action="list")` opens with the agents (OAuth clients) that used the host in the last 15 minutes, the caller marked `you` — e.g. `2 agents active (last 15 min): you (Claude Code#4f2a), cursor#9b1c` — and each job gains `by`: `you`, the agent that started it, or `unknown` for older jobs. Polling or killing another agent's job still works (one service user), and the reply opens with `[started by another agent: …]`.
- `mcp-ssh jobs` shows a `BY` column; `mcp-ssh sessions` lists each logged-in agent with when it was last active.
- An end-to-end test proves two separately logged-in agents run commands and background jobs side by side without waiting on each other.

### Changed

- **Schema version 7.** `jobs` gains `client_id`/`client_name`; `clients` gains `client_name`/`last_used_unix`. An older binary, including its admin CLI, refuses a migrated database. A `client_name` from client registration is cleaned before it is stored or shown: control and invisible characters dropped, whitespace collapsed, at most 32 characters.
- `job(action="list")` is no longer bare JSON: the presence line comes first.
