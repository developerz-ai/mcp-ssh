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
| install | `bin/setup` | cd to the repo root, require cargo, seed .env from .env.example only when .env is absent (chmod 600, never overwrite real credentials), point git at `.githooks` (pre-commit runs `bin/check`), then `cargo build --locked` |
| run | `bin/dev` | loads `.env`, writes `.local/dev.pid`, then `cargo watch -x 'run -- serve'` (or `exec cargo run -- serve`) — state under `.local/` per `.env.example`; on developerz.ai runs, start it with code_bash_background when you need a running stack |
| prove | `bin/check` | `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked` (unit + the `tests/e2e` suite); plus CI's shellcheck and cargo audit gates (ci.yml audit + shellcheck jobs) — skipped with a loud notice naming the ci.yml job when the tool is not installed locally |
| e2e | `bin/e2e` | just the end-to-end suite: boots the real binary per test and drives it over HTTP |
| stop | `bin/stop` | stops only the server `bin/dev` started (pid in `.local/dev.pid`) — on developerz.ai runs, the platform runs this when your session ends — do not stop the stack yourself |

## Rules

- Run every command from the repo root — the scripts `cd` there themselves.
- `bin/check` red is not a pull request. Fix the cause, never silence the gate.

## Exit codes `bin/check` must use

A coding box runs this gate before it commits, and it has to tell a FAILING change
from a gate it could not run — otherwise correct work is thrown away as if it were
red. Exit 1 cannot carry that difference, so two codes are reserved:

| Exit | Means | What the platform does |
|------|-------|------------------------|
| 0 | the change passes — including a CI-parity tool missing: the gate prints a loud `SKIP:` line naming the ci.yml job it could not run and hands that gate to CI | commit it; CI grades the skipped gate |
| 75 | a resource a cargo step NEEDS is unreachable (cargo itself, a database, a service, a credential) — the unguarded core refused to grade | refuse the commit, and blame the environment, never the change |
| 79 | every check the gate ran PASSED, but it cannot grade the whole change here (a scoped gate meeting a change whose dependents are the whole suite) — not the missing-CI-tool case, which is exit 0 above | commit it and open the pull request; CI grades the rest |
| any other non-zero | the gate ran and the change is red | refuse the commit, and blame the change |

- The CI-parity gates (`shellcheck`, `cargo audit`) have their own precondition
  rule: tool missing → loud `SKIP:` on stderr naming the ci.yml job, `exit 0` —
  never `exit 75`. `exit 75` is reserved for a precondition of the cargo steps
  themselves; guard those FIRST and `exit 75` when one is unmet. `exit 1` there
  tells the platform your change is broken when it is your environment that is.
- `exit 79` only when every check you ran passed. Anything failed is `exit 1`;
  a deferral never hides a red.
- Changed a dependency? Re-run `bin/setup` before `bin/check`, or you are proving a stale tree.
- Add the test that fails first, then the fix — a green run that never went red proves nothing.
