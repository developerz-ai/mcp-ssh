---
description: Prove a change in developerz-ai/mcp-ssh before committing or opening a pull request — the repo's own install, run and check commands. Use before every commit.
applies: [rust]
---

# Verify a change in developerz-ai/mcp-ssh

> House skill — established by the developerz.ai maintainer agent for the maintainers of this repository from this repo’s own
> detected commands. Correct it in place; setup never overwrites a skill that exists.

## The one gate

`bin/check` is the gate. It runs every check CI's `fmt`/`check`/`audit`/`shellcheck`
jobs run — `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked`, `shellcheck` over the scripts, `cargo audit` — and
`.githooks/pre-commit` runs it on every commit, so a change that passes it locally
passes CI.

| Step | Command | What it is |
|------|---------|------------|
| install | `bin/setup` | cd to the repo root, require cargo (the gate's 75 guard covers `rustfmt` and `cargo clippy` too — `rustup component add rustfmt clippy`), seed .env from .env.example only when .env is absent (chmod 600, never overwrite real credentials), point git at `.githooks` (pre-commit runs `bin/check`), then `cargo build --locked` |
| run | `bin/dev` | loads `.env`, writes `.local/dev.pid`, then `cargo watch -x 'run -- serve'` (or `exec cargo run -- serve`) — state under `.local/` per `.env.example`; on developerz.ai runs, start it with code_bash_background when you need a running stack |
| prove | `bin/check` | the five checks in order: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` (unit + `tests/e2e` + `tests/dev_scripts`), `cargo audit`, `shellcheck bin/* deploy/install.sh deploy/debian/postinst .githooks/pre-commit .claude/hooks/*.sh .github/scripts/*.sh`. The last two print a named `SKIP:` line when the tool is not installed; the other three are required (see exit codes) |
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
| 0 | every check the gate ran passed, and none was skipped | commit it, push, open the pull request |
| 75 | a tool the gate NEEDS is unreachable — `cargo`, `rustfmt` or `cargo clippy` is not installed, so it refused to grade | block the commit, and blame the environment, never the change; the diagnostic names the missing tool and the fix |
| 79 | every check the gate RAN passed, but `shellcheck` and/or `cargo-audit` is not installed here, so CI's two optional gates are deferred | commit it, push and open the pull request — a 79 must not block; CI grades the rest |
| 1 (or any other non-zero) | a check that ran failed — the change is red | block the commit, and blame the change; the diagnostic names the failed check |

- Order inside `bin/check`: precondition guards first (`exit 75`), then the checks,
  then the classification. A missing `cargo`/`rustfmt`/`cargo clippy` runs nothing and
  is `75`; `exit 1` there tells the platform your change is broken when it is your
  environment that is.
- `exit 79` only when every check that ran passed — a named `SKIP:` line per deferred
  tool. A present `shellcheck`/`cargo-audit` that fails is `exit 1`, never `79`, and
  a deferral never hides a red.
- The three hooks (`.githooks/pre-commit`, `.claude/hooks/pre-commit.sh`,
  `.claude/hooks/pre-push.sh`) translate that: `0` and `79` continue, `75` and `1`
  stop. With `set -euo pipefail`, capture the status as
  `status=0; bin/check || status=$?` — never a bare `bin/check` followed by `$?`.
- Changed a dependency? Re-run `bin/setup` before `bin/check`, or you are proving a stale tree.
- Add the test that fails first, then the fix — a green run that never went red proves nothing.
