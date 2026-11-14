# 08 — Docs drift, module map, file-size splits

> Part of [`overview.md`](overview.md). Depends on: 01–07 (run last; document the *fixed* behaviour, and move code only after the fixes land so nothing is lost in a rebase).

## Findings
### Docs that contradict the code
| ID | Where | Says | Reality |
|---|---|---|---|
| X1 | `CLAUDE.md` stack table, `README.md:140`, `docs/architecture.md:12` | rmcp 1.7 | 2.2 (`Cargo.toml`) |
| X2 | `CLAUDE.md` Testing + `/planx` template | integration tests in `tests/` | none until 07; `src/app_tests.rs` via `#[path]` |
| X3 | `CLAUDE.md` module map | — | missing `src/app.rs`, `src/app_tests.rs`, `src/sentry.rs`, `src/jobs/status.rs`, `src/oauth/{flow,metadata,store}.rs`, `src/tools/files/tests.rs`; `db.rs` row omits `jobs` |
| X4 | `CLAUDE.md` stack: "Lang … pinned via `rust-toolchain.toml`" | pinned | `channel = "stable"` (fixed in 07) |
| X5 | `CLAUDE.md` reaper row: kills any still-`Running` group first | — | untracked running rows are deleted unkilled (fixed in 02) |
| X6 | `docs/usage.md:17-33,44-48,79-88`, `docs/architecture.md:95-99`, `README.md:60` | poll walks **forward** from cursor 0; footer `[lines ..200 of 540; next_cursor=200]`; `total_lines`/`has_more` fields | newest-first; `[N of M lines shown]` + "older output remains — page back…" |
| X7 | `docs/usage.md:54,77,19` | no `title`; `action` required; "Poll it with" | `title` exists; `action` optional; "Monitor it with" |
| X8 | `docs/connect-claude.md:78`, `bin/mcp-token:15`, `.claude/skills/mcp-token/SKILL.md:13` | tokens reset on restart | persisted in SQLite ([E2E]: 2 tokens survived a crash + restart) |
| X9 | `bin/mcp-token:14`, `.claude/agents/oauth-auth.md:30` | refresh token 30 days | 365 days (`src/oauth/store.rs:26`); `docs/deploy.md:261` is right |
| X10 | `docs/architecture.md:30,89` | restart-failing + kill helpers live in `reaper.rs`; a job `running` after restart is failed because "its process is gone" | `signal.rs`/`store.rs`; survivors stay `running`. No rows for `signal.rs`, `store.rs`, `status.rs` |
| X11 | `docs/deploy.md:223` | DB path has "no config or env var" | `MCP_SSH_DB` / `db_path` exist; missing from the env tables (also `MCP_SSH_PUBLIC_URL`) |
| X12 | `docs/deploy.md` hardening checklist vs Docker section | "bound to 127.0.0.1" | container needs `0.0.0.0` (06) |
| X13 | `deploy/README.md` "NOTE: required Cargo.toml metadata … (not added here)" | deb metadata missing | present in `Cargo.toml` |
| X14 | `src/cli.rs:1` module doc | `serve` + `set-auth` only | 5 subcommands |
| X15 | `src/jobs/mod.rs:351` doc names `persist_final`; `src/jobs/mod.rs:1260` test id `ghost-01:00:00`; `src/db.rs:1-5` doc omits `clients` | — | `finish`; dashed ids; table exists |
| X16 | `docs/plans/2026/07/20/101-mcp-ssh-audit-fixes/status.yml` | `not_started`, 0 % | its slices are visibly implemented (`JobRepo`, `/healthz`, typed `FileError`, token sweep, redirect binding…) — stale tracker |
| X17 | `Dockerfile:17` comment | "shells out to sh/ls/find/grep/kill" | `kill` isn't in the image; after 02 it isn't needed |

### Files over the 300 LOC rule (CLAUDE.md: "Files ≤300 LOC")
| File | Total | Non-test (approx) |
|---|---|---|
| `src/jobs/mod.rs` | 1430 | ~500 |
| `src/jobs/store.rs` | 767 | ~424 |
| `src/oauth/flow.rs` | 675 | ~244 |
| `src/oauth/store.rs` | 649 | ~324 |
| `src/config.rs` | 592 | ~262 |
| `src/tools/mod.rs` | 588 | ~283 |
| `src/app_tests.rs` | 532 | test |
| `src/jobs/reaper.rs` | 503 | — |
| `src/admin.rs` | 488 | — |
| `src/tools/files/tests.rs` | 475 | test |
| `src/sentry.rs` | 462 | ~235 |
| `src/jobs/signal.rs` | 387 | — |
| `src/jobs/log.rs` | 332 | — |
| `src/tools/files.rs` | 322 | — |

## Files to change
- `CLAUDE.md`, `README.md`, `docs/architecture.md`, `docs/usage.md`, `docs/deploy.md`, `docs/connect-claude.md`, `deploy/README.md`.
- `bin/mcp-token`, `.claude/skills/mcp-token/SKILL.md`, `.claude/agents/oauth-auth.md`, `.claude/commands/planx.md` (module list still names files that moved).
- `docs/prompts/system-prompt.md`, `docs/prompts/skill.md` — the agent-facing guidance must describe newest-first paging, `job list` paging, `recursive` on delete, kill outcomes.
- Source splits listed below.
- `docs/plans/2026/07/20/101-mcp-ssh-audit-fixes/status.yml`.

## Steps
1. **Decide the LOC rule's scope first** (owner): either "≤300 LOC of non-test code; tests go in a sibling `tests.rs` via `#[path]`" (the pattern `src/app_tests.rs` and `src/tools/files/tests.rs` already use) or the literal rule. Recommend the former and write it into CLAUDE.md — it makes 8 of the 14 files compliant by moving tests out, with no logic moved.
2. **Mechanical test extraction** (no logic change): for each over-limit file, move `#[cfg(test)] mod tests` to `<mod>/tests.rs`. `bin/check` green before and after each move; one commit per file.
3. **Real splits** (one reason to change each), after step 2:
   - `src/jobs/mod.rs` → keep `JobStore` wiring + public types; move launch/waiter into `src/jobs/run.rs`, poll into `src/jobs/poll.rs`, `ProcessGroupId` into `src/jobs/signal.rs` (or `pgid.rs`).
   - `src/jobs/store.rs` → row⇄`JobState` mapping into `src/jobs/store/row.rs`.
   - `src/oauth/store.rs` → tokens vs clients (`store/tokens.rs`, `store/clients.rs`) — natural after 04 adds families + client sweep.
   - `src/sentry.rs` → `sentry/scrub.rs` (pure redaction) + init.
4. **Docs** (X1–X15, X17): rewrite the poll sections of `docs/usage.md`/`docs/architecture.md` from the actual render functions (`src/tools/mod.rs`); copy the example output from the snapshot test added in 05 so it can't drift silently. Update every table/claim in the list. Rebuild the CLAUDE.md module map and "Where to look" from `ls src/**` after the splits. Document `MCP_SSH_DB`, `MCP_SSH_PUBLIC_URL`, token lifetimes, `sessions --revoke`, the consent page, Docker bind/volume.
5. **Guard against re-drift:** a tiny test (or CI shell step) asserting every `src/**/*.rs` path named in CLAUDE.md's module map exists and every non-test `src` file appears in it. Cheap, and this map is the repo's stated navigation aid.
6. **Old tracker** (X16): set the July plan's `status.yml` to `complete` (or `superseded` by this plan for anything unfinished) with evidence PR numbers from `git log`; verify slice by slice before marking — don't assume.
7. Release notes for the version that ships this plan: one-time logout (token hashing), `delete` needs `recursive=true`, unknown job action is an error, consent page, Docker bind/volume, new file modes.

## Tests
- Module-map check from step 5.
- Poll/list render snapshot (from 05) is the source for doc examples.
- Refactors: `bin/check` green before AND after each move; no behaviour change in step 2/3 commits (diff is moves only).

## Done when
- Every claim in CLAUDE.md, README and `docs/` matches the code at HEAD; the module map lists every source file.
- No non-test source file exceeds the agreed limit.
- `docs/prompts/*` teach an agent the real paging and safety semantics.
- Both plan trackers reflect reality.
