# Deep-Dive Audit Fixes (all areas)

## Goal
Fix the defects found by a full audit of `main` @ `109ac78` (4 area audits by reading + a live end-to-end run against a booted server): server-killing memory/disk blowups, kill paths that lie, auth/token weaknesses, file-tool footguns, shutdown/deploy/CI/DX gaps, doc drift.

## Context
- Stack: Rust 2024, tokio, axum 0.8, rmcp **2.2** (docs still say 1.7), SQLite via `rusqlite` bundled, OAuth 2.1 + Basic (login only). One binary, one host, one service user.
- Baseline: `bin/check` green — 191 tests, 2.3s. All tests colocated; `tests/` does not exist.
- Evidence tags used in slices: **[E2E]** reproduced against a running server · **[READ]** confirmed by reading the path · **[PLAUS]** plausible, verify first.
- Tool surface stays 3 tools (`bash`/`job`/`file`). Every fix here is a param/`action`/behaviour change, never a 4th tool.
- Agent-facing wording lives only in `src/tools/mod.rs` (`render_file`/`render_file_error` pattern) — new typed outcomes get rendered there.
- Reference patterns:
  - Bounded streamed read: `src/tools/files.rs:98-150` (`append_capped`) — the model for log reads in slice 01.
  - Bounded child runner (cap + deadline + kill/reap): `src/tools/files/shell.rs`.
  - Typed kill outcome: `KillOutcome` in `src/jobs/signal.rs:87-112` — extend to the live-handle path (slice 02).
  - Transactional store op: `oauth::Store::issue` in `src/oauth/store.rs:216-227`.
  - Secret-file creation 0600: `write_secret_file` in `src/config.rs:209-220`.
  - `EnvSource`/`MapEnv` test seam: `src/config.rs:62-74,271-287`.
- Security invariants (CLAUDE.md NEVER): no password/token/code in a log line, span, error body, id or filename; job ids/log names derive only from the normalized `title` (`src/jobs/id.rs`); never weaken `auth::require_auth`; TLS + rate limiting stay in the proxy (document proxy config, don't add to `src/`).

## Top findings (why this order)
| # | Finding | Evidence |
|---|---|---|
| 1 | `job(action="poll")` on a large live log → server RSS 50 GB → OOM-killed. Running-job log unbounded (150 GB in ~60 s) | [E2E] |
| 2 | Docker image has no `kill` binary; a failed `kill` spawn reads as "group dead" → every kill reports success, nothing signalled | [E2E] image, [READ] logic |
| 3 | `file(read, path="/dev/zero")` never returns, pins a core after client disconnect | [E2E] |
| 4 | `file(delete, path="link/")` empties the symlink's target dir | [E2E] |
| 5 | SIGTERM with one open SSE stream: no exit within 30 s | [E2E] |
| 6 | DB with plaintext bearer/refresh tokens created 0644; job logs 0664 | [E2E] |
| 7 | `MCP_SSH_PASS=` (empty) boots and serves | [E2E] |
| 8 | `/authorize` issues a code on a bare GET with cached Basic creds (no consent); no token revocation | [READ] |

## Plan files (execute in order)
1. [`01-bounded-output.md`](01-bounded-output.md) — HIGH: bound every log/file read, the saved tail, the live log size, `job list`.
2. [`02-kill-and-lifecycle.md`](02-kill-and-lifecycle.md) — HIGH: `kill(2)` tri-state, one kill loop, survivors/descendants, reaper, typed outcomes.
3. [`03-db-state.md`](03-db-state.md) — file modes, versioned migrations, transactions, error surfacing, admin CLI. Prerequisite for 04's schema changes.
4. [`04-auth-oauth.md`](04-auth-oauth.md) — consent step, hashed tokens, revocation, empty creds, redirect/PKCE validation, metadata.
5. [`05-file-tool.md`](05-file-tool.md) — delete/move safety, partial success, error context, strict args, wording locality.
6. [`06-runtime-deploy.md`](06-runtime-deploy.md) — shutdown drain, Dockerfile, systemd unit, `.deb`, installer, proxy examples, Sentry scrub.
7. [`07-ci-dx.md`](07-ci-dx.md) — CI correctness/supply chain/arch coverage, e2e harness, dev setup, hooks.
8. [`08-docs-structure.md`](08-docs-structure.md) — doc drift, module map, >300 LOC splits, stale tracker. Run last; rebase moves onto fixed code.

## Done when
- A job emitting ≥1 GB, polled while running and after exit, keeps server RSS flat (bounded read) and its log under the cap — proven by test and by `bin/e2e`.
- No kill path reports success unless the process group is verifiably gone; works in the Docker image.
- `file read` on a device/FIFO errors promptly; `delete` never follows a symlink; non-empty dir delete needs `recursive=true`.
- No row in `access_tokens`/`refresh_tokens` equals a usable token; DB 0600, state dir 0700; password change revokes sessions; empty creds refuse to start.
- `/authorize` never returns `code=` without an explicit confirmed POST.
- SIGTERM exits within the drain deadline with streams open.
- CI builds with `--locked`, runs an audit, ships amd64+arm64 `.deb`, smoke-tests the image; `bin/e2e` exists and runs in CI.
- Docs/CLAUDE.md match the code; `bin/check` green after every slice.

## Risks / open questions
- **Behaviour changes needing owner sign-off:** `delete` requiring `recursive=true` for non-empty dirs (05); unknown `job` action string → error instead of `list` (05); `deny_unknown_fields` (05); 24h reaper killing running jobs — keep, exempt, or document (02); atomic `write` via temp+rename changes inode/hardlink semantics (05).
- **Consent page (04):** replaces the Basic-prompt GET; must keep headless `bin/mcp-token` working (it becomes a 2-step GET+POST) and Claude's connector flow. Biggest-risk change — test against a real Claude client before release.
- **Password at rest (04):** argon2id adds a dependency + changes `config.toml` format (`pass` → `pass_hash`); env `MCP_SSH_PASS` stays plaintext input. Needs a migration note; decide before starting.
- **Token hashing (04) logs everyone out** once (old plaintext rows unusable). Acceptable; say so in release notes.
- **`ProtectSystem=full` vs "agent self-manages host via sudo" (06):** [PLAUS] sudo children inherit the read-only `/usr` `/etc` mount namespace, so `sudo apt install` would fail inside the service. Verify on a box before changing the unit.
- **Ordering:** 03 (migration framework) before 04 (schema changes). 08 moves code — last. 01 and 02 both touch `src/jobs/`; run sequentially, not in parallel.
- **Test safety:** never run an unbounded generator (`yes`) against a dev machine when reproducing #1 — use a sparse/truncate-created file.
