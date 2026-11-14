---
description: Prove a change in developerz-ai/mcp-ssh before committing or opening a pull request — the repo's own install, run and check commands. Use before every commit.
applies: [rust]
---

# Verify a change in developerz-ai/mcp-ssh

> House skill — established by the developerz.ai maintainer agent for the maintainers of this repository from this repo’s own
> detected commands. Correct it in place; setup never overwrites a skill that exists.

## The one gate

`bin/check` is the gate. CI runs it and the pre-commit hook runs it, so a change that
passes it locally passes everywhere.

| Step | Command | What it is |
|------|---------|------------|
| install | `bin/setup` | (dpkg -s build-essential ca-certificates >/dev/null 2>&1 \|\| (sudo -n apt-get update && sudo -n apt-get install -y --no-install-recommends build-essential ca-certificates)) && (command -v cargo >/dev/null 2>&1 \|\| mise use -g rust@stable) && cargo build |
| run | `bin/dev` | if [ -f .env ]; then set -a; . ./.env; set +a; fi; exec cargo run -- serve — on developerz.ai runs, start it with code_bash_background when you need a running stack |
| prove | `bin/check` | cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test |
| stop | `bin/stop` | pkill -f '[m]cp-ssh serve' \|\| true — on developerz.ai runs, the platform runs this when your session ends — do not stop the stack yourself |

## Rules

- Run every command from the repo root — the scripts `cd` there themselves.
- `bin/check` red is not a pull request. Fix the cause, never silence the gate.

## Exit codes `bin/check` must use

A coding box runs this gate before it commits, and it has to tell a FAILING change
from a gate it could not run — otherwise correct work is thrown away as if it were
red. Exit 1 cannot carry that difference, so two codes are reserved:

| Exit | Means | What the platform does |
|------|-------|------------------------|
| 0 | the change passes | commit it |
| 75 | a resource the gate NEEDS is unreachable (a database, a service, a credential) — it refused to grade | refuse the commit, and blame the environment, never the change |
| 79 | every check the gate ran PASSED, but it cannot grade the whole change here (a scoped gate meeting a change whose dependents are the whole suite) | commit it and open the pull request; CI grades the rest |
| any other non-zero | the gate ran and the change is red | refuse the commit, and blame the change |

- Guard preconditions FIRST and `exit 75` when one is unmet. `exit 1` there tells the
  platform your change is broken when it is your environment that is.
- `exit 79` only when every check you ran passed. Anything failed is `exit 1`;
  a deferral never hides a red.
- Changed a dependency? Re-run `bin/setup` before `bin/check`, or you are proving a stale tree.
- Add the test that fails first, then the fix — a green run that never went red proves nothing.
