---
description: Prove a change in developerz-ai/mcp-ssh before committing or opening a pull request — the repo's own install, run and check commands. Use before every commit.
applies: [rust]
---

# Verify a change in developerz-ai/mcp-ssh

> House skill — established by dz-runner setup mode from this repo’s own
> detected commands. Correct it in place; setup never overwrites a skill that exists.

## The one gate

`bin/check` is the gate. CI runs it and the pre-commit hook runs it, so a change that
passes it locally passes everywhere.

| Step | Command | What it is |
|------|---------|------------|
| install | `bin/setup` | export PATH="$HOME/.cargo/bin:$PATH" && (dpkg -s build-essential ca-certificates coreutils findutils grep >/dev/null 2>&1 \|\| (sudo -n apt-get update && sudo -n apt-get install -y --no-install-recommends build-essential ca-certificates coreutils findutils grep)) && cargo build |
| run | `bin/dev` | export PATH="$HOME/.cargo/bin:$PATH" && { [ -f .env ] \|\| cp .env.example .env; } && set -a && . ./.env && set +a && export MCP_SSH_JOB_DIR="$PWD/.local/logs/jobs" MCP_SSH_DB="$PWD/.local/mcp-ssh.db" && if command -v cargo-watch >/dev/null 2>&1; then exec cargo watch -x "run -- serve"; else exec cargo run -- serve; fi — on developerz.ai runs, start it with code_bash_background when you need a running stack |
| prove | `bin/check` | `cargo fmt --check` then `cargo clippy --all-targets -- -D warnings` then `cargo test` (`bin/check:4-6`) — the repo's own file, run unchanged. Its `set -euo pipefail` means it exits with the FIRST failing step's status; see the exit-code table below |
| stop | `bin/stop` | pkill -f '[t]arget/debug/mcp-ssh serve' \|\| true; pkill -f '[c]argo watch -x run -- serve' \|\| true; pkill -f '[c]argo run -- serve' \|\| true — on developerz.ai runs, the platform runs this when your session ends — do not stop the stack yourself |

## Rules

- Run every command from the repo root — the scripts `cd` there themselves.
- `bin/check` red is not a pull request. Fix the cause, never silence the gate.

## Exit codes: what `bin/check` actually returns

`bin/check` is `set -euo pipefail` followed by three commands (`bin/check:4-6`), so it
exits with the FIRST failing step's own status. It reserves no codes and cannot: the gate
belongs to the repository, not to this skill. The codes this box observes:

| Exit | First step that stopped | Blame |
|------|-------------------------|-------|
| 0 | none — all three passed | commit it |
| 1 | `cargo fmt --check` — the diff is printed | the change |
| 101 | `cargo clippy --all-targets -- -D warnings`, or `cargo test` | the change |
| 127 | `cargo: command not found` — no toolchain on `PATH` | the environment |
| 1 with `No version is set for shim: cargo` in the output | the `mise` shim refusing to grade | the environment |

- Read the FIRST failing step's output, not the code alone. A formatting diff, a clippy
  lint or a failing test is the change; `command not found` or a `mise` shim message is
  the environment. Exit 1 is ambiguous between fmt and a shim, so the message decides —
  never assume the change is red because the number is 1.
- Preflight the environment before blaming a change: `command -v cargo` must print a path.
  When `mise` answers `No version is set for shim: cargo`, prefix
  `export PATH="$HOME/.cargo/bin:$PATH"` (rustup's `cargo` lives there, 1.99.1 on this
  box) or set the version with `mise use rust`, then re-run the gate unchanged.
- Never record a pass you did not see. If a step could not run, the pull request says
  which step and why; a partial grade is not a green gate.
- Changed a dependency? Re-run `bin/setup` before `bin/check`, or you are proving a stale tree.
- Add the test that fails first, then the fix — a green run that never went red proves nothing.
