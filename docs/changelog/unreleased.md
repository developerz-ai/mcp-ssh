# Unreleased

### Changed

- **rmcp 3.9.** The MCP SDK moves from 2.2 to 3.9.0, the first release patched for RUSTSEC-2026-0462; `cargo audit` runs with no ignores (#66).
- **The 256 MiB log cap covers what a job leaves running.** A process a job left in its group (`server &`) is still capped after the job itself has exited: past the cap the group is `SIGKILL`ed and the job is re-recorded `failed`, even one that had `exited`. The cap ends with the server process (#69).
- **Schema version 6.** `jobs` gains an `interactive` column. An older binary, including its `mcp-ssh jobs` CLI, refuses a migrated database.

### Fixed

- An interactive job that outlived a restart is killed like a live one: its bash leader is stopped first, so it cannot start its next command during the grace period (#69).
- A kill that did not take (`Survived`, `NotSignalled`) no longer marks the job `killed` when it later exits on its own (#69).
- A tool call refused before dispatch (unknown field, wrong type, unknown tool) logs one `rejected` warning naming the tool and a fixed reason word, never an argument (#69).
- An open event stream ends within 30 seconds of its token being revoked by `mcp-ssh sessions --revoke` or `set-auth` (#69).
- The Docker image creates `/var/lib/mcp-ssh/logs` owner-only (#69).
- A CI comment that described our own infrastructure is gone, and a test keeps tracked files free of such facts (#47).
