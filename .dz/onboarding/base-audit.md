# Base audit — developerz-ai/mcp-ssh @ current main

> One new file, authored by this audit run. Every maintainer-authored base file was read and is
> reported on below; none was rewritten. `.dz/onboarding/scorecard.md` belongs to the onboarding
> runs that rewrite it whole and was left alone.

## Scope & method

- Work starts from **current main**, after the later onboarding run that established the base:
  `.dz/onboarding/scorecard.md:5-8` — run `run_d9c8969d57374e4787c2c649e16e35cf`, computed
  2026-11-14, proof **proved**, 13/15 ok.
- **PR #40 was read from GitHub** — head `a476972c00ff`, branch `dz/onboarding`, opened 2026-11-03,
  **closed unmerged** 2026-11-06 ("GitHub refused the merge 3 time(s) on this head"). Its roster
  (`job-engine`/`oauth-flow`/`mcp-tool-surface`), skills (`plan-writing`/`service-deploy`) and
  `.githooks/pre-commit` do not exist at main; main's names (`jobs-engine`/`oauth-auth`/`mcp-tools`,
  `mcp-token` skill) prove the base was re-established afterwards. **Nothing was pushed to
  `dz/onboarding`** by this audit; the branch and the PR were not touched.
- Method: read-only survey of the checkout, plus exactly one scoped command, `./bin/setup`
  (§Verification). No base file modified. Secret names only below, never values.

## The four base elements

| Element | Verdict | Evidence |
|---|---|---|
| Project brain | **PRESENT — drifted; policy thin** | CLAUDE.md (170 lines) + AGENTS.md:5-8 pointer; roster 4, commands 6, skills 3, policy 2 — all in place. Drift: module map / Where-to-look omit 7 of 26 src files, `rmcp 1.7` vs Cargo.toml `2.2`, command table missing `bin/setup`/`bin/stop`, `tests/` claim false. Policy: reviewer.md has 3 TODO(setup) holes → F1 |
| One-command DX scripts | **PRESENT** | `bin/setup:1-19`, `bin/dev:1-17`, `bin/check:1-6`, `bin/stop:1-5`, `bin/mcp-token:1-76` — all real, and bin/check mirrors CI exactly. Gaps: no exit-75/79 gate contract, hook claim holds only inside Claude Code → F2 |
| Managed MCP servers | **PRESENT — partially connected** | `.mcp.json:3-28`: sentry-mcp, codegraph, developerz. Secrets landed 2 of 12 (scorecard.md:49-52); `DEVELOPERZ_API_KEY` unlanded → services table |
| Code index | **PRESENT — drifted** | CLAUDE.md:52-71 + :135-149 map 19 of 26 src files; docs/README.md:5-11 indexes only directories that don't exist → F3; plan tracker contradicts the tree → F4 |

### 1. Project brain — present; drifted; policy thin

Present:

- `CLAUDE.md` (170 lines): stack :16-31, commands :33-44, module map :48-73, execution model
  :75-79, 3-tool design :81-91, conventions :93-105, coding rules :107-127, testing :129-133,
  where-to-look :135-149, NEVER :151-160.
- `AGENTS.md:1-8` — "two names, ONE brain" pointer to CLAUDE.md.
- Roster — `.claude/agents/`: `jobs-engine.md` (src/jobs; rules :28-34), `oauth-auth.md`
  (src/oauth + auth.rs; :28-34), `mcp-tools.md` (src/tools; :28-33), `sqlite-state.md`
  (db + both stores; :28-33).
- Commands — `.claude/commands/`: `deploy`, `feature` (143-line hive workflow), `implement`,
  `plan`, `planx` (104; defines the `status.yml` tracker shape :65-85), `qa`.
- Skills — `.claude/skills/`: `verify-change` (46; gate + exit-75/79 contract :28-45),
  `release-cut` (13), `mcp-token` (13).
- Policy — `.dz/maintainer/maintainer.yml` (114: `services: []` :12; autonomous PR posture,
  `auto_merge: true` :34, `require_review: true` :37) + `.dz/maintainer/reviewer.md` (22).
  Schedule briefs `.dz/schedules/{devops,hygiene}.md` exist for the commented block at
  maintainer.yml:98-114.
- Claude-layer hooks: `.claude/settings.json:17-38` wires `.claude/hooks/pre-commit.sh`
  (fmt + clippy) to `git commit` and `.claude/hooks/pre-push.sh` (cargo test) to `git push` —
  PreToolUse hooks for Claude Code sessions, **not** git hooks (→ F2).

Drift (evidence for queued-five #1):

- Module map CLAUDE.md:52-71 and Where-to-look :137-149 omit 7 of src/'s 26 files:
  `src/app.rs` (router wiring, per feature.md:57), `src/app_tests.rs` (integration tests),
  `src/sentry.rs` (error tracking; named by Cargo.toml:59-61), `src/jobs/status.rs`,
  `src/oauth/flow.rs`, `src/oauth/metadata.rs`, `src/tools/files/tests.rs`.
- Stack: CLAUDE.md:23 says `rmcp 1.7`; Cargo.toml:18 pins `rmcp = "2.2"` (README.md:140 repeats 1.7).
- Commands table CLAUDE.md:35-44 has no `bin/setup` or `bin/stop` row.
- Testing CLAUDE.md:132 puts integration tests in `tests/` — no `tests/` directory exists; tests
  are colocated `#[cfg(test)]` plus `src/app_tests.rs` / `src/tools/files/tests.rs`
  (feature.md:64 already flags this falsification).
- Thin: `reviewer.md:14` (Review conventions), `:18` (Gates that matter), `:22` (Never flag) are
  TODO(setup) placeholders, and `:14` cites a `CONTRIBUTING` file that does not exist at the root
  (13 root files, none of them it) → **F1**.
- Observed tension, noted not filed: `maintainer.yml:55-57` sets `release.manager: none` while
  ci.yml:41-58 ships a tag-driven release job and :60-91 a docker job (release-cut SKILL:11
  documents both). For the maintainers to reconcile.

### 2. One-command DX scripts — present

| Script | Does | Anchor |
|---|---|---|
| `bin/setup` | fails fast if cargo is missing (:4-7), `cargo build` (:9), prints next steps (:11-19). Installs nothing: no `.env` copy, no hooks | bin/setup:1-19 |
| `bin/dev` | loads `.env` (:6-11); `cargo watch -x 'run -- serve'` when cargo-watch exists (:13-14), else `cargo run -- serve` (:16) | bin/dev:1-17 |
| `bin/check` | `cargo fmt --check` :4 / `cargo clippy --all-targets -- -D warnings` :5 / `cargo test` :6 — exactly CI's check job | bin/check:1-6; ci.yml:27,38-39 |
| `bin/stop` | `pkill -f '[m]cp-ssh serve'` — bracket trick, no self-match | bin/stop:5 |
| `bin/mcp-token` | headless OAuth: DCR :43-46, `/authorize` Basic via `curl -K -` stdin so the password is never on argv (:50-52), PKCE exchange :68-74, token to stdout only | bin/mcp-token:1-76 |

- Gate ↔ CI alignment holds: same three commands (bin/check:4-6 == ci.yml:27,38-39), `stable`
  on both sides (rust-toolchain.toml:2; ci.yml:24,34).
- Gaps: no exit-75/79 contract though verify-change SKILL.md:28-45 requires one, and its
  "the pre-commit hook runs it" claim (:13) is true only inside Claude Code sessions → **F2**.
  Mise-shim PATH hazard on other boxes → **F5**.

### 3. Managed MCP servers — present; partially connected

`.mcp.json` — three servers, no secret values inline:

- `sentry-mcp` :3-11 — `bunx @sentry/mcp-server`; env `SENTRY_ACCESS_TOKEN` / `SENTRY_HOST`
  (:8-9). Both **landed** (scorecard.md:51).
- `codegraph` :12-21 — `codegraph serve --mcp`, telemetry off (:17-19). No secret needed.
- `developerz` :22-28 — http `https://mcp.developerz.ai/v1/mcp`, `Bearer ${DEVELOPERZ_API_KEY}`
  (:26). The key is **not landed** (scorecard.md:49-52).

The scorecard's own `access` gap (:49-52) matches: 2 of 12 asked landed. The missing ten are
runtime env for the binary (`MCP_SSH_*`, `RUST_LOG`, `SENTRY_DSN`, `SENTRY_ENVIRONMENT`), not
MCP-server wiring — still unwired.

### 4. Code index — present; drifted

- CLAUDE.md module map :52-71 (18 rows) + Where-to-look :137-149 (11 rows) cover 19 of src/'s
  26 files; the 7 listed under §1 are unmapped.
- `docs/README.md:5-11` indexes only `idea/` + `standards/`, both "*(not created yet)*" (:7-8),
  and misses the six real pages (`docs/architecture.md`, `docs/connect-claude.md`,
  `docs/deploy.md`, `docs/usage.md`, `docs/prompts/system-prompt.md`, `docs/prompts/skill.md`)
  that root README.md:126-136 indexes; its command footer (:10-11) omits `bin/mcp-token` → **F3**.
- codegraph is wired (.mcp.json:12-21) and referenced by the workflow commands (feature.md:73,
  planx.md:18); `.codegraph/` is gitignored (.gitignore:21) and holds no index in this checkout.
- Plan tracker `docs/plans/2026/07/20/101-mcp-ssh-audit-fixes/status.yml:3` says `not_started`,
  `percent: 0` (:7), `last_updated: 2026-07-20` (:30) — but slice 01's deliverable already exists
  in the tree: `src/tools/files/shell.rs:12` (`MAX_SHELL_OUTPUT_BYTES`) + `:18`
  (`MAX_SHELL_RUN_SECS = 30`) + the timeout variant (:36-40) implement 01-file-op-bounds.md
  step 1 verbatim, with its tests at `src/tools/files/tests.rs` → **F4**.

## Services to connect

| Service | Wired | Secret state | Why |
|---|---|---|---|
| Sentry / GlitchTip | .mcp.json:3-11 | `SENTRY_ACCESS_TOKEN` + `SENTRY_HOST` landed | the binary ships Sentry-protocol error tracking to GlitchTip (Cargo.toml:63-72, src/sentry.rs); production diagnosis routes through `mcp__sentry-mcp` scrubbed events (feature.md:67) |
| codegraph | .mcp.json:12-21 | none needed | structural lookup before grepping on every survey (feature.md:73, planx.md:18) — cheap navigation for agents on a 26-file crate |
| developerz | .mcp.json:22-28 | `DEVELOPERZ_API_KEY` **not landed** | platform self-integration; without the key the server entry is dead weight |
| GitHub | platform layer (git/gh owned by the harness), not .mcp.json | none in repo | the whole loop lives there: triage (maintainer.yml:72-79), review (:63-68), tag → .deb + GHCR release (ci.yml:41-58,60-91; release-cut SKILL:3-4) |

## Queued five — reconciliation

Queued platform tasks reconciled against current main. In-repo records: scorecard.md:54-57
(layout) and PR #40's "First tasks" list, which names four of the five (dev-scripts verification,
CLAUDE.md refinement, bin/check-vs-CI, apps/mcp-ssh move); workflow hardening has no in-repo
record. None of the five is duplicated by F1–F5 below.

| # | Queued task | Status at main | Evidence |
|---|---|---|---|
| 1 | Refine CLAUDE.md into a tight project brain | **still needed** | the drift list in §1: 7 unmapped files, rmcp version, missing `bin/setup`/`bin/stop` rows, false `tests/` claim |
| 2 | Align bin/check with the existing CI pipeline | **satisfied** | bin/check:4-6 == ci.yml:27,38-39; `stable` both sides (rust-toolchain.toml:2; ci.yml:24,34). The remaining gap is the gate contract, filed as F2 — not an alignment gap |
| 3 | Verify the existing dev scripts run clean | **satisfied** | this audit's script reads (§2) plus this audit's own `./bin/setup` run, exit 0 (§Verification); scorecard.md:39,62 recorded setup 0.4s / check 3.7s green at head. The residual box-dependence is F5 |
| 4 | Workflow hardening | **still needed** | fmt/check jobs carry no `permissions:` block (ci.yml:17-39) while release (:46-47) and docker (:65-67) do; 15 `uses:` across 9 distinct actions, all tag-pinned not SHA-pinned (ci.yml:23-24,33-34,37,49-52,56,69-71,76,84) |
| 5 | Move the application into apps/mcp-ssh | **still open** | scorecard.md:54-57 (layout gap, "10 move(s) planned as one task"); tree is still root-layout: Cargo.toml:12-14 `path = "src/main.rs"`, 26 files under src/ |

## Follow-up work items (F1–F5)

Anchored, none duplicating the queued five. Every target is a maintainer-authored file —
reported here, never rewritten by this audit.

**F1 — fill reviewer.md's three TODO(setup) sections; resolve its CONTRIBUTING reference.**
`.dz/maintainer/reviewer.md:14` (Review conventions), `:18` (Gates that matter), `:22` (Never
flag) are placeholders; `:14` cites CONTRIBUTING, which does not exist at the root. Source of
truth for all three: CLAUDE.md conventions :93-127, gate = `bin/check` (CLAUDE.md:39), NEVER
:151-160.

**F2 — gate contract: make bin/check match what verify-change/SKILL.md promises.**

- SKILL.md:28-45 reserves exit 75 (a resource the gate needs is unreachable — blame the
  environment) and 79 (everything ran green but the gate is scoped — defer to CI); bin/check:1-6
  is a bare three-command script under `set -euo pipefail`, so any failure is cargo's own
  non-zero and a platform cannot tell a red change from an ungradable environment.
- SKILL.md:13 "CI runs it and the pre-commit hook runs it": the git-layer hook does not exist at
  main — no `.githooks/` in the root, bin/setup:1-19 installs none. What exists is a Claude Code
  PreToolUse hook (`.claude/hooks/pre-commit.sh`, fmt + clippy, wired at
  `.claude/settings.json:20-27`; `pre-push.sh` → cargo test, :28-36) that fires only when Claude
  Code itself runs git — a plain `git commit` bypasses it. PR #40 shipped `.githooks/pre-commit`;
  it died with the closed PR (→ F5).
- Skill-table drift, same file: SKILL.md:18 describes `bin/setup` as an apt/mise install chain —
  the real script is a cargo check + build (bin/setup:4-9); SKILL.md:19 describes `bin/dev` as a
  plain `cargo run` — the real script prefers cargo-watch (bin/dev:13-16).
- PR #40's `bin/test-dev-lifecycle` regression test never landed at main. Its recorded review
  findings stand as the spec for a future attempt: the fixture publishes its PID before `setsid`
  creates the session (bin/test-dev-lifecycle:111), and cleanup leaves the second workload's
  separate session alive (bin/dev:38 in the PR's tree; finding at :143).

**F3 — docs/README.md does not index its own real pages.**
docs/README.md:5-11 indexes `idea/` + `standards/` ("not created yet", :7-8) while the actual
pages — docs/architecture.md, docs/connect-claude.md, docs/deploy.md, docs/usage.md,
docs/prompts/system-prompt.md, docs/prompts/skill.md — are indexed only by root README.md:126-136,
and the footer's command list (:10-11) omits `bin/mcp-token`. Add the six pages and the token
minter to the docs index.

**F4 — plan tracker stale: 101-mcp-ssh-audit-fixes says not_started over shipped work.**
status.yml:3 `status: not_started`, :7 `percent: 0`, :30 `last_updated: 2026-07-20`, yet slice
01's deliverable exists: src/tools/files/shell.rs:12 + :18 + :36-40 implement 01-file-op-bounds.md
step 1 (streamed byte cap, wall-clock deadline, kill + marker), its tests file exists
(src/tools/files/tests.rs), and the no-clobber `move` behavior 01 step 4 specifies is documented
as shipped (CLAUDE.md:68). Reconcile the tracker (status/percent/evidence) or mark the plan
superseded.

**F5 — PR #40's script repairs never landed: the mise-shim PATH hazard is still open.**
PR #40 (read from GitHub, head `a476972c00ff`, closed unmerged 2026-11-06) diagnosed `bin/dev`
and `bin/check` failing on boxes where a mise cargo shim precedes `~/.cargo/bin` — its own
Verification shows `./bin/setup` exiting 1 at base AND at head with "mise ERROR No version is set
for shim: cargo" — and repaired both scripts with a PATH prepend, plus shipped
`.githooks/pre-commit`. None of it is at main: bin/check:1-6 and bin/dev:13-16 carry no PATH fix
and no `.githooks/` exists. It is environment-dependent, not universal — this box ran green
(scorecard.md:39,62; §Verification below) — which is exactly why it needs its own task with a
reproducing box, not a re-attempt of the dead PR.

## Verification

Editor's own scoped check, run at this audit's head — cold `target/`, so stderr is the full
dependency build. Every `Compiling <crate> …` line (hundreds, `proc-macro2 v1.0.106` first,
`mcp-ssh v1.4.0` last) is elided for length; nothing else was cut.

stderr, last lines verbatim:

```
   Compiling mcp-ssh v1.4.0 (/home/dz-agent/workspace-ai/developerz-ai/developerz-ai/mcp-ssh)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3m 03s
```

stdout verbatim:

```

Build complete.

Next steps:
  cp .env.example .env   # then edit credentials
  bin/dev                # run the server locally
  bin/check              # run the fmt/clippy/test gate
EXIT=0
```

Exit code **0** — the environment is green at this head, which supports queued-five #3
(dev scripts run clean) and bounds F5 as box-dependent, not universal.
