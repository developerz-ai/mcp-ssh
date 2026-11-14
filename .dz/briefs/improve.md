# Brief: monthly improvement

Fired every Tuesday by `schedules: improve`. **Act only on the first Tuesday of the month:**
`[ "$(date -u +%-d)" -le 7 ]`. On any other Tuesday stop here, file nothing, open nothing.

Once a month, leave mcp-ssh measurably better, so nobody has to touch it by hand. `CLAUDE.md` binds every change, and `non_goals` in the policy file are off limits. A red `main`, a missing publish and a stuck Dependabot PR belong to `health`.

## Signals

| Signal | Command |
|---|---|
| the gate on fresh `main` | `bin/check` |
| advisories | `cargo audit` |
| dependency majors | `cargo outdated` if installed, else the Dependabot PRs closed red |
| the Rust toolchain | newest stable Rust at least 14 days old, against `rust-toolchain.toml` |
| open bugs | `gh issue list --state open --label bug --json number,title,url` |
| deep read | `.dz/schedules/hygiene.md` and `.dz/schedules/devops.md`: what to look for, and the evidence rules |

Read a failing job's log before deciding anything. A red run whose log names a missing secret or a repository setting is the owner's to fix: escalate once, never delete the check or turn it into a green skip.

## Pick at most three, in this order

1. Anything the gate found broken or flaky: the root cause, never a retry or a skip.
2. Advisories, then dependency majors Dependabot could not land (rmcp, axum, rusqlite), one per PR.
3. The toolchain, when more than one stable release behind: `rust-toolchain.toml`, `Cargo.toml` `rust-version` and the Dockerfile `FROM` together, new clippy lints fixed. Dependabot cannot do this (it ignores the cargo-chef base on purpose).
4. Open bugs, each starting from a test that fails on `main`.
5. Docs drift: README, CLAUDE.md (the module map, the tool table), `docs/`; quote the doc line and the code.
6. Hidden failures: an `|| true`, a `2>/dev/null`, a `continue-on-error`, an `.ok()` or `let _ =` that drops an error. Make it fail, or handle it visibly. A documented, deliberate fallback stays.
7. Simplification: dead code, duplicated logic, a file past the 300-LOC cap, slow or flaky tests.

## Ship

One small PR per improvement, its evidence in the body, `bin/check` green first, a line in `docs/changelog/unreleased.md` when users see it. Never bump the version, and never touch release automation from this checkup: open an issue instead. A breaking change is never merged by this lane (`escalate.ask_before_acting: [breaking_change]`): open the PR or issue and leave it for a maintainer.
Check no open PR or issue already covers it. If nothing is worth changing, do nothing: no empty PRs, no churn.

## Rows

- `gate`: `bin/check` on fresh `main` finds something broken or flaky; root cause, never a retry or a skip.
- `advisories`: `cargo audit` finds advisories; one per PR, before majors.
- `dependency-majors`: majors Dependabot could not land; one per PR.
- `toolchain`: the three Rust pins bumped together when behind.
- `open-bugs`: each from a test that fails on `main`.
- `docs-drift`: a doc line the code contradicts.
- `hidden-failures`: swallowed errors and CI skips, made visible.
- `simplification`: dead code, duplicated logic, files past 300 LOC, slow or flaky tests.
