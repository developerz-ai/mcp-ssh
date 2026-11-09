# Reviewer instructions — developerz-ai/mcp-ssh

> Read by the developerz.ai reviewer at EVERY review of this repository, at the reviewed
> commit, as this repo's own layer of the reviewer's prompt — established by dz-runner setup mode.
> It refines the platform rules (a finding is cited or withheld; the bot always discloses)
> and never overrides them. Edit freely; keep it under 12,000 characters — past that it is cut.

## Stack

Rust + Cargo. The gate is `bin/check` (`export PATH="$HOME/.cargo/bin:$PATH" && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`): a change that does not pass it is not proven, whatever its description says.

## Review conventions

Hold a change to these. Each is a rule this repo already implements — cite the line that
breaks it, and the existing line that shows the intended shape.

- **`/mcp` is bearer-only.** HTTP Basic survives solely as the human login inside
  `/authorize`. Never accept a patch that adds a Basic path to `/mcp`
  (`src/auth.rs:1-5`; the rejection is pinned by `basic_on_mcp_now_401s`,
  `src/app_tests.rs:198`).
- **A 401 from `/mcp` points at the OAuth metadata** through `WWW-Authenticate`
  `resource_metadata_url` so a GUI client can start the flow (`src/auth.rs:78-94`).
- **`/authorize` redeems a `redirect_uri` only when it is exactly one the named
  `client_id` registered** — no prefix, wildcard or host match; unknown client,
  unregistered URI and DB error all deny, inline, before any redirect
  (`src/oauth/flow.rs:49-86`, `src/oauth/store.rs:99-114`; pinned by
  `a_client_is_bound_to_exactly_the_uris_it_registered`, `src/oauth/store.rs:368`).
- **PKCE is S256 only** — a missing challenge or any other method is `invalid_request`
  (`src/oauth/flow.rs:81-86`), and the discovery document must stay honest:
  `code_challenge_methods_supported: ["S256"]`, nothing more (`src/oauth/metadata.rs:57`).
- **Token, OAuth client and job rows live in SQLite** (`src/db.rs`, `src/oauth/store.rs`),
  never in memory: logins must survive a restart. Token sweeps are one bulk bound `DELETE`
  returning a count only (`src/oauth/store.rs:293-303`).
- **A schema change is an idempotent migration, not a rewrite.** Add a durable column by
  extending `ensure_column` (`src/db.rs:116`), and ship the test that opens a *legacy*
  on-disk database missing the column and proves it is migrated in place
  (`open_migrates_a_legacy_jobs_table_missing_new_columns`, `src/db.rs:206`).
- **No secret reaches a response, a log line, an error or `argv`.** Not a token value
  (`src/oauth/store.rs:293-303`, `src/admin.rs:1-7`), not the password — `bin/mcp-token`
  feeds the Basic header through a curl config on stdin, never on the command line
  (`bin/mcp-token:49-56`). This repo is an auth boundary; treat a leak here as critical.
- **No `unwrap`/`expect` outside `main` and tests.** A panic in the request path kills
  every connected client. Propagate with `?`, branch with `match`/`if let`/`let ... else`
  (`CLAUDE.md:98`). Same for `std::sync::Mutex` on the request path and for holding a
  `std::sync` guard across `.await`.
- **Errors are typed.** `thiserror` for domain errors, `anyhow` only at the `main.rs`
  boundary (`src/config.rs:42-57` is the shape to match).
- **A newtype over a bare primitive** when the value has meaning — `JobId`, not `String`;
  an `enum` over contradictory `bool`+`Option` (`src/jobs/id.rs`, `src/tools/args.rs:43-50`).
- **Every tool dispatch is a `tracing` span carrying `request_id` and the tool name** —
  `tracing`, never `println` (`src/tools/mod.rs:89`, `:145`).
- **All `jobs` SQL lives in `src/jobs/store.rs` (`JobRepo`)** — never inline in the engine,
  the reaper or `src/admin.rs`. Kill semantics live only in `src/jobs/signal.rs`
  (`kill_job` / `kill_persisted`); callers render `KillOutcome` instead of re-implementing
  TERM→KILL escalation.
- **The MCP surface stays three tools** — `bash`, `job`, `file`, parametrized. New
  capability is a new `action` or param, never a fourth tool; adding one fails
  `tool_surface_is_exactly_bash_job_file` (`src/app_tests.rs:396`).
- **Output stays bounded at every layer** — a byte cap plus wall-clock deadline on the
  shelled-out file ops (`src/tools/files/shell.rs:69`), a line+byte cap per `job poll`
  page, newest-first. Returning a whole log is the context blow-up this server exists to
  prevent (`CLAUDE.md:Execution model`).
- **A job id carries no command text** (`src/jobs/id.rs`), so a secret cannot leak through
  an id, a log line or a filename.
- **Off-loopback binding requires `MCP_SSH_ALLOWED_HOSTS`** — the config fails fast with
  `AllowedHostsRequired` rather than serving with no Host allowlist
  (`src/config.rs:140-144`). Never weaken the auth middleware or that check.
- **Modules stay ≤300 LOC with one reason to change** (`CLAUDE.md:73`). A module that grew
  a second responsibility is a finding; a file at 299 lines is not.

## Gates that matter

- **`bin/check` decides.** It is `cargo fmt --check`, then
  `cargo clippy --all-targets -- -D warnings`, then `cargo test` (`bin/check:4-6`), run
  unchanged. CI runs the same three commands (`.github/workflows/ci.yml:27,38,39`) and so
  does the pre-commit hook (`.githooks/pre-commit:7`). A gate change is not a review
  suggestion: it is the thing that must not happen in the change under review.
- **"Proven" means**: the gate green at the reviewed head, plus a test that fails without
  the change — colocated `#[cfg(test)]` for pure logic, `tests/` for real MCP requests
  over HTTP (`CLAUDE.md:Testing`). A behaviour change to auth also needs its rejection case,
  not only its happy path.
- **A behaviour change that can run must be shown running** — `bin/dev`, then the real
  request or the real CLI on real input (`bin/mcp-token` drives
  `/register` → `/authorize` → `/token` end to end). A green suite alone is not proof.
- **Never suggest silencing the gate** — no `#[allow(...)]` to clear a clippy error, no
  `--no-verify`, no skipped or `#[ignore]`d test, no weakened assertion. The guard is the
  asset; pin known-bad behaviour with a test instead.
- **A red `bin/check` names the change, not the environment**, unless the output says
  otherwise (`cargo: command not found`, a `mise` shim refusing to grade). Say which one
  you concluded, and from what output.

## Never flag

- **Style the formatter owns.** `cargo fmt --check` decides line breaks, import order and
  trailing commas. A hand-formatting nit is not a finding.
- **`#[allow(dead_code)]` on a macro-read field.** `tool_router` (`src/tools/mod.rs:24-27`)
  and `/authorize`'s accepted `resource` (`src/oauth/flow.rs:25-29`) are read by the rmcp
  macro or reserved by RFC 8707; dead-code analysis cannot see either. Both are commented.
- **Generated, vendored or ignored paths.** `target/`, `*.deb`, `.env*` (except
  `.env.example`) and `tmp/` are gitignored (`.gitignore:1-33`); `Cargo.lock` is tracked on
  purpose — a lockfile diff is a dependency change to be read, not noise.
- **Deliberate repo conventions.** `sh -c` by default with `bash -ic` as opt-in
  (`interactive`, which sources `~/.bashrc`, `src/jobs/shell.rs:26-36`); the
  `lenient_action` shim that maps a malformed `action` to `list` for clients that mishandle
  the schema (`src/tools/args.rs:52-79`); one serialized SQLite connection with live output
  kept in a per-job log file (`CLAUDE.md:Hybrid persistence`); the "not building" list —
  no SSH client, fleet orchestration, scheduler or secrets vault (`CLAUDE.md:Context`).
- **TLS, rate limiting and multi-host routing.** They belong to the reverse proxy. Their
  absence from this binary is the design, not a gap.
- **Docs or comments that restate the code's `what`** in a place this repo wrote on
  purpose — but do flag a doc that names a path, command or flag the code no longer has.
