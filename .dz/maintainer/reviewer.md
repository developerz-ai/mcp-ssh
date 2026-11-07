# Reviewer instructions — developerz-ai/mcp-ssh

> Read by the developerz.ai reviewer at EVERY review of this repository, at the reviewed
> commit, as this repo's own layer of the reviewer's prompt — established by dz-runner setup mode.
> It refines the platform rules (a finding is cited or withheld; the bot always discloses)
> and never overrides them. Edit freely; keep it under 12,000 characters — past that it is cut.

## Stack

Rust + Cargo. The gate is `bin/check`, verbatim in the repo: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` (`bin/check:4-6`) — the same three steps CI runs (`.github/workflows/ci.yml`, jobs `fmt` and `check`). A change that does not pass it is not proven, whatever its description says. If `cargo` is not on `PATH` (a `mise` shim answering `No version is set for shim: cargo`), that is an environment gap, not a red change: prefix `export PATH="$HOME/.cargo/bin:$PATH"` and re-run.

## Review conventions

Hold a change to `CLAUDE.md`'s rules, each as a flag when broken:

- **Constant tool surface.** Three tools — `bash`, `job`, `file` — grouped by resource and
  parametrized by `action`/params. New capability = a new param or `action`; a fourth tool
  is a defect. Agent-facing wording lives only in `src/tools/mod.rs` (`files` returns facts,
  never a rendered sentence).
- **Typed errors.** `thiserror` for domain errors, `anyhow` only at the `main.rs` boundary;
  no `unwrap`/`expect` outside `main` and tests (a panic in the request path kills every
  client).
- **Make illegal states unrepresentable.** Newtype over a bare primitive when the value has
  meaning (`JobId`, not `String`); `enum` over contradictory `bool`+`Option`; validate at
  the edge.
- **Async end-to-end.** No `std::sync::Mutex` on the request path, never a `std::sync`
  guard held across `.await`; blocking I/O through `spawn_blocking`; `tracing`, never
  `println`, and a tool dispatch is a span carrying `request_id` and `tool`.
- **Borrow by default**, `.clone()` only when ownership must move.
- **Module size.** Files ≤300 LOC, one responsibility each; a module with a second reason to
  change gets split.
- **Tests ship with the behaviour.** Unit tests colocated in `#[cfg(test)]`; integration
  tests in `tests/` boot the real server and speak MCP over HTTP. A new behaviour without the
  test that proves it is incomplete.
- **House wrappers are law**: `bin/check`, `bin/setup`, `bin/dev`, `bin/stop` and the
  `.claude/skills/*` procedures. A change that contradicts one names why in the body.

## Gates that matter

- `bin/check` green at the reviewed commit — fmt, `clippy --all-targets -- -D warnings`,
  `cargo test`. Not the description's claim of it; the run.
- Auth changes: `/mcp` stays bearer-only (HTTP Basic exists solely as the human login inside
  `/authorize`), `redirect_uri` matching stays exact against the registered `client_id`, PKCE
  stays S256, and `src/app_tests.rs` gains a passing case AND a rejection case.
- Schema changes ship the `ensure_column` migration in `Db::open` plus a legacy-database test
  (`.claude/skills/sqlite-migrations`).
- Secrets: no token, password or auth code in a response, an error, a log line or an argv.
- "Proven" for a behaviour change is the command, request or output the pull request shows —
  a diff alone is not evidence, and a 200 status is not an answer.

## Never flag

- Anything `cargo fmt` owns (formatting, line breaks, import order) or `clippy -D warnings`
  already rejects — the machine says it better.
- `Cargo.lock`, `target/`, and binary assets under `docs/assets/`.
- Choices `CLAUDE.md` states as deliberate: TLS terminated by the reverse proxy rather than
  in the binary, Streamable HTTP rather than stdio, one host rather than a fleet, SQLite via
  `rusqlite` `bundled` rather than a system library.
- The prose in `.dz/**`, `.claude/**`, `AGENTS.md` or `docs/` for existing on purpose. Stale
  or placeholder text in those files IS fair game — flag it like any other drift.
