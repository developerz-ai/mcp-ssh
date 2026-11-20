---
description: Prove a change in developerz-ai/mcp-ssh before committing or opening a pull request — the repo's own install, run and check commands. Use before every commit.
applies: [rust]
---

# Verify a change in developerz-ai/mcp-ssh

> House skill — established by the developerz.ai maintainer agent for the maintainers of this repository from this repo’s own
> detected commands. Correct it in place; setup never overwrites a skill that exists.

## The one gate

`bin/check` is the local gate, used by the pre-commit hook. CI independently runs
the same three Rust checks, plus audit, shellcheck and packaging checks. Local
success proves this gate's coverage, not every CI job or environment.

| Step | Command | What it is |
|------|---------|------------|
| install | `bin/setup` | cd to the repo root, require cargo, seed .env from .env.example only when .env is absent (chmod 600, never overwrite real credentials), point git at `.githooks` (pre-commit runs `bin/check`), then `cargo build --locked` |
| run | `bin/dev` | loads `.env`, writes `.local/dev.pid`, then `cargo watch -x 'run -- serve'` (or `exec cargo run -- serve`) — state under `.local/` per `.env.example`; on developerz.ai runs, start it with code_bash_background when you need a running stack |
| prove | `bin/check` | `cargo fmt --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked` (unit + the `tests/e2e` suite) |
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

## Concrete preflight and coverage

The exit contract above is authoritative. Keep exactly three grading commands:
`cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and
`cargo test --locked`. Do not add audit or shellcheck to manufacture partial coverage.

- Before any grading command, require usable Cargo, rustc, rustfmt and Clippy for
  the selected toolchain. A successful `command -v` alone is insufficient:
  rustup proxies can exist while the selected toolchain or component is absent.
  Probe usability without grading the source; an unmet prerequisite yields 75.
- Treat `PUBLIC_TREE_PRIVATE_WORDS` as comma- or newline-separated entries,
  trimmed with empty entries dropped, matching `tests/public_tree.rs`.
  Unset, blank and delimiter-only input all mean no private-word coverage.
  When `PUBLIC_TREE_PRIVATE_WORDS_REQUIRED=1`, this is a prerequisite refusal
  (75), before grading. Never print the input or matched private values.
- When that coverage is optional and absent, run all three grading commands.
  Return 79 only if all three pass. Other public-tree rules still run; do not
  weaken or bypass the public-tree test's own required-input enforcement.
- Normalize every executed grading failure to 1, including a child's 75 or 79.
  Optional coverage cannot hide a failure. Conversely, missing prerequisites
  take precedence over a latent source failure: no grading command runs then.
- Return 0 only after all three checks pass with private-word coverage supplied.
  Hooks may permit 0 and 79, but must preserve refusal and failure statuses.

## Status mapping in the current gate

The exit table above is the contract source; this section maps it onto what
`bin/check` ships today.

| Condition | `bin/check` today |
|-----------|-------------------|
| Usable prerequisites, supplied coverage, all checks pass | 0 |
| Cargo or rustc missing | 75 before grading |
| Proxy present, selected toolchain or component unusable | 75 before grading |
| Required private-word input empty, blank or delimiter-only | 75 before grading |
| Optional private-word input empty, checks pass | 79 |
| Executed fmt, clippy or test fails | 1, even for a child's 75 or 79 |
| Missing prerequisite plus latent check failure | 75, no grading command runs |
| Optional coverage absent plus executed failure | 1, never 79 |

The hooks translate that mapping: they accept 0 and 79 as a pass, and pass 75
and every other non-zero straight through, so a refused or red gate never
reaches a commit.

What pins each row:

- **Prerequisites** (`bin/check:7-16`): one `require_tool` probe runs
  `cargo --version`, `rustc --version`, `cargo fmt --version` and
  `cargo clippy --version` before any grading command, and exits 75 on the
  first that fails. A `command -v` hit is not enough: a rustup proxy on PATH
  proves a proxy exists, not that the selected toolchain or component runs.
- **Private-word coverage** (`bin/check:18-49`): commas and Rust
  `str::trim`'s Unicode White_Space set are stripped from
  `PUBLIC_TREE_PRIVATE_WORDS`, matching `tests/public_tree.rs:109-127`, so
  unset, blank, delimiter-only and whitespace-only input all mean no coverage.
  The reduction set is built from raw octal bytes one whole character at a
  time, not a bracket class or `$'\u00a0'`: both of those are locale-dependent,
  so where the locale is not UTF-8 the value never reduced to empty and a run
  without coverage graded as a full pass. With
  `PUBLIC_TREE_PRIVATE_WORDS_REQUIRED=1` an empty reduction is a 75 before
  grading; otherwise the run is marked partial.
- **Grading** (`bin/check:51-53`): exactly the three commands named above, and
  every executed failure is normalized to 1 (`|| exit 1`).
- **Deferral** (`bin/check:55-58`): 79 is reachable only after all three pass
  with the run marked partial, and never on a failure.
- **Regression coverage** (`tests/dev_scripts.rs`): `:274-291` fakes each
  probe and asserts 75 with no grading command run, `:293-318` the
  coverage/required mapping, `:320-340` normalization, `:356-384` the hooks.

### The `command -v` finding (PR #83)

PR #83 is closed and unmerged; leave it and its branch untouched. Its
`bin/check` guarded with `command -v`, which leaves the missing-component
case open: installed proxies do not prove usable tools, so a present rustc,
rustfmt or Clippy component that cannot run used to surface as a child
failure instead of a refusal. The current gate answers that finding by running
each tool (`bin/check:7-16`), and `tests/dev_scripts.rs:274-291` keeps the
answer honest: with every probe faked to fail, the gate exits 75 and invokes
no grading command.

`docs/changelog/unreleased.md` carries the user-visible entry for this
gate behaviour.
