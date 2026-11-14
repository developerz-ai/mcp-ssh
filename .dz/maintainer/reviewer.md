# Reviewer instructions — developerz-ai/mcp-ssh

> Read by the developerz.ai reviewer at EVERY review of this repository, at the reviewed
> commit, as this repo's own layer of the reviewer's prompt — established by the developerz.ai maintainer agent for the maintainers of this repository.
> It refines the platform rules (a finding is cited or withheld; the bot always discloses)
> and never overrides them. Edit freely; keep it under 12,000 characters — past that it is cut.

## Stack

Rust 2024 + Cargo, toolchain pinned in `rust-toolchain.toml`. The gate is `bin/check` (`cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, the e2e suite included): a change that does not pass it is not proven, whatever its description says.

## Review conventions

- The password and every token never appear in a response, an error, a log line or `Debug` output. Read the error and `Debug` paths, not just the happy path. Tokens are stored hashed.
- `/mcp` is bearer-only behind `auth::require_auth`, and every route but `/healthz` sits behind the `Host` allowlist guard. A change that weakens either is must-fix.
- Exactly three tools: `bash`, `job`, `file`. New capability is a param or an `action`, never a fourth tool. Args are `#[serde(deny_unknown_fields)]`.
- Output is bounded everywhere: poll pages newest-first under line/byte budgets, the live log is capped at 256 MiB, file reads page. An unbounded read or buffer is a finding.
- Kills: `kill(2)` only, the tri-state probe (unknown is never gone), success means the group probed gone, a pgid from another boot is never signalled. Callers only render a `KillOutcome`.
- The reaper never kills, and never ages out a running job.
- No `unwrap`/`expect` outside `main` and tests; no `std::sync::Mutex` on the request path; typed errors with `thiserror`.
- Files ≤300 non-test LOC; a new or removed `src` file updates CLAUDE.md's module map (`tests/module_map.rs` fails otherwise).
- A user-visible change adds its line to `docs/changelog/unreleased.md` in the same PR.
- `rust-toolchain.toml`, `Cargo.toml` `rust-version` and the Dockerfile base move together, in one PR.
- A security-path finding (auth, OAuth, config/credentials, command execution, signalling) is must-fix: there is no human merge behind the bot, so the review is the gate.

## Gates that matter

- Required by the `main-protection` ruleset: `fmt` and `check`. `audit` (cargo audit) and `shellcheck` also run in `ci.yml`; a red one is a finding even though it does not block.
- The image job builds and smokes the container on every PR (`.github/scripts/smoke.sh`: healthz, OAuth, bash, job kill); the `deb` job installs the package in a clean Debian. Both prove the shipping artefacts, not just the code.
- `tests/public_tree.rs` scans every tracked file (minus `.dz/`) for facts about our own infrastructure.

## Never flag

- Style rustfmt and clippy own.
- The SHA-pinned GitHub Actions and the pinned toolchain: deliberate. Flag drift between the three toolchain pins, not the pinning.
- The `lukemathwalker/cargo-chef` Dependabot ignore: the toolchain moves by hand, in step.
- `docs/changelog/<version>.md` pages: release notes, not code.
