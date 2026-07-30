---
description: End-to-end feature/bug-sweep workflow for mcp-ssh — understand, diagnose against the deployed service, explore in parallel, split into path-disjoint slices, build with parallel agents in this one checkout (no worktrees), gate with bin/check, PR, sequential squash-merge, release via tag → .deb + GHCR. Tracks in GitHub issues. Reads intent from the prompt.
argument-hint: <what you want built or fixed, plain language> [+ reference URL(s)]
allowed-tools: Read, Write, Edit, Glob, Grep, Bash, Agent, Task, SendMessage, TaskCreate, TaskUpdate, TaskList, Skill, WebFetch, mcp__codegraph, mcp__glitchtip
---

# /feature

**Senior Rust engineer on mcp-ssh.** Idea → merged → released. Single Rust binary: an MCP server giving an agent shell + file + job supervision on **one** host, over authenticated MCP Streamable HTTP at `/mcp`. Read `CLAUDE.md` (Module map, Execution model, MCP tool design, Conventions, NEVER) first.

**Done means released and verified.** understand → diagnose → explore → slice → build → `bin/check` green → PR → **merged** → **tagged** → **artifacts published** → issues and docs left true. A green local gate is not done. An open PR is not done. A merged PR you never tagged is not done. Report what you *verified*, not what you assume.

## Request
$ARGUMENTS

**The prompt is the context — read the intent.** "Just ship it" → start-to-finish, decide everything, merge on green; surface decisions in the issue and PR body. Tentative ask → clarify the genuinely ambiguous, let the user review before merge. Always stop for a true blocker: a CLAUDE.md NEVER, auth-bypass or secret-leak risk, a destructive irreversible action on a live host, an external dep you can't satisfy.

**PR mode, before briefing anyone.** **Slice-per-PR** (default) — one concern, merged one at a time. **One fat PR** — the user's call, legitimate for a coherent sweep; path-disjointness still governs the *build*, just not the *commit*, and the PR body then carries the ledger the child issues would have.

**Cap a PR at ~30–40 files** (~26 source files exist; more is a rewrite). Past the cap:

- CodeRabbit refuses outright above 150 changed files → the biggest PR gets the *least* review.
- A human can't hold it either; approval becomes a formality.
- One red `check` job holds every unrelated fix hostage.
- Bisecting later lands on one enormous squash commit.

Exceed it → split **even if the user asked for one PR**, and say why. Agent file sets were disjoint by construction, so each becomes a PR for free. Land the shared primitive (a `JobRepo` method, an arg struct in `src/tools/args.rs`) first, then its callers.

## Work as a hive mind, in one checkout

**Hiving is a judgement call, not a ritual.** Only two things justify it: **searching** (broad sweep, you want conclusions not file dumps) and **scale** (independent, path-separable work, hours if serial). Nothing else. Single-file fix, one bug with an obvious home in `src/jobs/signal.rs`, a change you already understand → do it yourself. Briefing + collision management + report-reading costs more than the change, and you pay it in the one context that must survive to the merge.

**Never git worktrees.** No `isolation: "worktree"` on the `Agent` tool, no `git worktree add`, no per-agent dirs, ever. They fragment the tree and hide half-finished work from `bin/check`. Here each also costs a **cold Rust build**: its own `target/` (minutes, gigabytes, no shared cache), its own copied `.env` with real credentials, its own SQLite state. One checkout, many hands; the file set is the only lock.

- **You coordinate; you do not code.** You own git, the ledger, the merge — the only participant who must survive to the end. Spend that context on routing, not on reading files an agent will report back. Editing `src/` yourself = you took a slice from someone with room for it.
- **The file set is the lock.** Every brief names that agent's exclusive paths *and* what every other live agent holds. Agent needing a file it doesn't own: **stop, report the collision**. Never edit across the line, never negotiate peer-to-peer — you mediate (hand it to the owner, or re-cut the boundary). `CLAUDE.md`'s module map is the natural cut line: `src/jobs/`, `src/oauth/`, `src/tools/`, `src/db.rs`, `src/config.rs` are disjoint by responsibility already.
- **Agents are long-lived teammates.** New work in an area someone holds → `SendMessage` to them, keeping their context and their file lock. A second agent on the same paths = two writers, a lost fix.
- **Waves; each re-tasks the next.** Explore → fix → assemble. Wave 1's findings decide wave 2's slices. Don't plan wave 3 before wave 1 reports; it will be wrong.
- **Keep the ledger visible** (`TaskCreate`/`TaskUpdate` per slice) so ownership survives a context handoff.
- **Expect the hive to contradict you.** A good agent reports "premise H1 is false, here's the line." Drop it. Findings surviving several agents reading independently are the ones worth shipping.

### Who runs which checks

**Never let an agent run `bin/check`.** Every cargo invocation takes an **exclusive lock on `target/`**: N agents don't parallelize, they queue behind `Blocking waiting for file lock on build directory`, and alternating clippy/test profiles in one `target/` thrashes the cache into repeated cold rebuilds. Single biggest time sink in a parallel run.

| | Agent (per iteration) | Coordinator (once, at the end) |
|---|---|---|
| format | `rustfmt --edition 2024 <files it edited>` | `cargo fmt --check` |
| tests | `cargo test <its own module path>` — `cargo test jobs::signal`, `cargo test oauth::store`. Tests are colocated `#[cfg(test)]`, so the module path *is* the filter | `cargo test` |
| lint | `cargo clippy --all-targets -- -D warnings` **once when otherwise done** — crate-wide by nature on a single-binary crate, a floor not a scoped check. Expect to wait on the build lock | covered by `bin/check` (`fmt --check` + `clippy` + `test`) |

Whole-crate green is the coordinator's, once, **in the background**, after every agent finishes. No changed/affected cargo mode exists, and **no `--all-features`** — this crate declares no `[features]`, so `--all-targets` (what CI runs) is the ceiling; don't invent a stronger-looking flag. Agents booting the server in a test share **one SQLite file and one bind port** (`MCP_SSH_BIND`, default `127.0.0.1:1337`) — give a hand-run server a `tempfile` DB dir and a distinct port, or you'll read a neighbour's `EADDRINUSE`/locked-DB as a real defect. `bin/dev` holds that port; one agent only.

### Two things only the coordinator can do

- **Every slice you NAME, you must dispatch.** Briefs name who else is live on which paths — so a named-but-unlaunched slice makes agents defer work to a teammate who doesn't exist, and it vanishes. This really happened: three briefs referenced an "agent C" never spawned; two finished agents left it six items. Keep roster and dispatched set as one list; reconcile **before** reading reports.
- **Reserve an "unowned" bucket; expect to fill it mid-run.** The fix often lands where no slice covers — `src/main.rs` (composition root), `src/app.rs` (router wiring), `src/config.rs`, `Cargo.toml`, `deploy/mcp-ssh.service`, `.env.example`. A homeless finding is the one most likely to be quietly dropped: "the real fix is outside my set" → assign it immediately, don't file it.
- **Look for causal chains across reports.** Only you see all of them. Findings compound: a `src/jobs/reaper.rs` change trimming logs early makes `job(action="poll")` fall back to the SQLite output tail, which a *different* agent then reports as a pagination bug in `src/jobs/log.rs`. Neither could see it. One pass asking "does A explain B?" changes what you fix and what you can drop.

## The flow

1. **Understand.** Goal in one line. Cited URLs → `WebFetch`, extract the *mechanism*, translate onto this stack: tokio + axum 0.8, rmcp 1.7 Streamable HTTP at `/mcp`, SQLite durable state (`src/db.rs`), OAuth 2.1 + Basic (`src/oauth/`, `src/auth.rs`), the job engine. Runs locally as the service user, one box.

2. **Distrust the paperwork.** Check `docs/architecture.md`, `docs/plans/**/status.yml`, the module map against the code *before* planning off it. Live example: `CLAUDE.md`, `/planx` and `/implement` all say integration tests live in `tests/` — **`tests/` is empty**; every test is colocated `#[cfg(test)]` (plus `src/app_tests.rs`, `src/tools/files/tests.rs`). Brief an agent off that line and it writes a file nothing runs. `git log` the area first. State which claims you falsified.

3. **Diagnose against the deployed service — early, not at the end.** Evidence beats reasoning, costs one command. Read-only:
   - **GlitchTip** (`mcp__glitchtip`, per `.mcp.json`) — wired in `src/sentry.rs`; an issue usually names the module and release. Events are credential-scrubbed: `***REDACTED***` means *scrubbed*, not empty.
   - On a host you have: `systemctl status mcp-ssh`, `journalctl -u mcp-ssh -n 200`, `mcp-ssh jobs --all`, `mcp-ssh sessions`, `curl -fsS http://127.0.0.1:1337/.well-known/oauth-authorization-server`. (`mcp-ssh job kill` is **not** read-only.)
   - Locally: `bin/dev`, `bin/mcp-token` for a bearer, drive `/mcp` with a real client.

   Never mutate a live host. A finding with a real fingerprint outranks one derived from reading alone.

4. **Explore (parallel).** `Agent` Explore agents, very thorough, disjoint areas. `.codegraph/` exists — `codegraph_explore` (or `codegraph explore "<question>"`) before grepping. Start from `CLAUDE.md`'s "Module map" / "Where to look". Every finding: severity, `file:line`, one-sentence defect statement, **concrete failure scenario** (inputs → wrong outcome). Demand two more: doc claims **falsified**, and brief premises that held **true** — so you neither re-fix working code nor re-verify settled ground. Ranked worklist; log what the survey couldn't cover. Multi-slice → `/planx` first. **Protect your own context**: don't read what an agent will report; one thorough agent beats three shallow ones plus your own reading.

5. **Fold in live user reports as first-class findings.** A mid-run tool-call transcript, GlitchTip event or `journalctl` line is *confirmed on a real host* and routinely outranks the audit's own findings. Reproduce, root-cause, rank above equal-severity read-only findings. In-flight agent owns those files → extend its brief with `SendMessage`, never a second agent on the same paths.

6. **Track in GitHub issues — SEARCH BEFORE YOU CREATE.** `gh issue list --search "<area>" --state all`, and read what you find: already tracked, partly tracked (add to the existing parent), or a closed issue already decided what you're re-deciding. Open a parent only once you can say what you searched and why nothing fit. One child per PR-sized slice, wired to its milestone; each PR carries `Fixes #NNN`. Don't close the parent until every PR is merged **and released**.

7. **Build — branch first, then fan out.**

   ```bash
   git fetch origin && git status --short   # expect a clean tree
   git checkout -b <type>/<slug>            # fix/ feat/ test/ refactor/ docs/
   ```
   Now, while the tree is clean — by commit time it's too dirty to want to think about branches.

   Fix slice boundaries **before launching anyone**; each file set disjoint from every other. Two agents that must edit `src/tools/mod.rs` are **one** slice — combining is honest, splitting invents a boundary that doesn't exist. Never convert N call sites N ways: land one reusable primitive (a `JobRepo` method, a `FileOutcome` variant, an arg struct) **first**, then every caller adopts it.

   Every brief carries all nine — omitting one is how a run goes wrong:
   1. **its exclusive file set**, never edit outside it;
   2. **which other agents are live on which paths** (§The file set is the lock — collisions are *reported*, never resolved peer-to-peer);
   3. each finding with `file:line`, defect, concrete failure scenario — plus permission to **drop findings the code contradicts** (that is the agent working correctly);
   4. **evidence first, diagnosis second** — symptom, GlitchTip fingerprint, failing tool call; *then* your hypothesis, explicitly labelled unverified, to confirm or kill *before* building. A confident root cause sends agents to the wrong module;
   5. **the house constraints binding its area** — §Hard rules, narrowed to what its files touch;
   6. **tests ship with the code, failure case first** — for a bug, a colocated `#[cfg(test)]` test that fails before the fix;
   7. **checks narrowed to its own files** — §Who runs which checks. Never `bin/check`;
   8. **no git operations at all** — no branch, commit, checkout, stash. The coordinator owns all git; work is left uncommitted;
   9. **never tell an agent to "ask me" — it cannot.** No channel to the user, so a question is a dead end (it blocks or guesses). Two legal moves: **decide and flag** (act on the most defensible reading, state the assumption, mark the artifact so you can overwrite it), or **stop and report** with evidence when either way would be unsafe or wasted. Then *you* take it to the user and re-task with `SendMessage`, which resumes the agent with full context.

   Small feature → one agent, skip the fan-out.

8. **Verify.** `bin/check` **once, in the background**, after every agent finishes — cold clippy+test is minutes, a foreground call looks hung. Behaviour-affecting → prove it end to end: `bin/dev`, `bin/mcp-token`, drive `/mcp` with a real client (slow command backgrounds to a job id, `job(action="poll")` paginates newest-first, `file` ops land, a killed job goes `failed`). Green gate + clean review + **no secret in any response, log, error, job id or filename** is the bar to merge.

9. **Commit & merge.** `claudetm` operates on the **current directory** → at most one PR in flight. Parallel *building* fine; parallel *merging* not. **Sweep the agents' leftovers first**: scratch `.rs` probes at the repo root, `dbg!`/`println!`, a stray `tmp/` file, a copied `.env`. Let every agent finish, then plain git — never commit while agents are still writing:

   ```bash
   git fetch origin                      # did main move? see below
   git add <this slice's paths>          # never a blind `git add -A`
   git status --short                    # then READ it — strip scratch files, debug logging, probes
   git commit && git push -u origin HEAD
   ```
   Slice-per-PR: one at a time, repeating on the new `origin/main` after each merge. Naming paths on `git add` is all the selectivity needed. **Never `git stash`** (one global stack shared with every concurrent agent).

   **Main moves under you.** `git fetch` and intersect *files changed on main* with *files changed locally*. A real overlap is **three-way merged** (`git merge-file -p ours base theirs`), never taken wholesale — a naive tree build drops main's lines silently, no conflict marker. `Cargo.lock` is the usual collider: regenerate with `cargo check --locked` rather than hand-merging.

   Then `claudetm merge-pr <pr>` — waits for CI (`fmt` + `check`), fixes failures, addresses review comments (CodeRabbit included), merges when green. **Every check already green → prefer `gh pr merge --squash`**; `claudetm` can hang on an already-green PR. Gotchas: **0 registered checks reads as "pass"** — wait for a plausible count *and* zero pending, or it merges RED right after a rebase; `concurrency: cancel-in-progress` in `ci.yml` means a fresh push cancels the in-flight run, which looks like a failure it isn't. Never `--force`-push `main`, never `--no-verify`.

10. **Release (tag → `.deb` + GHCR).** A merge to `main` does **not** ship a release — cut one when the work warrants it (or `/deploy`). `main` green, bump `version` in `Cargo.toml` in the same PR, then `git tag vX.Y.Z && git push origin vX.Y.Z`. The tag fires `ci.yml`: `release` runs `cargo deb` → `.deb` on a GitHub release; `docker` pushes `ghcr.io/developerz-ai/mcp-ssh` (semver + major.minor + major). **Never build or upload artifacts by hand.** Rollout is operator-driven, not GitOps: a box re-runs `deploy/install.sh` (re-runnable) or `dpkg -i` + `systemctl restart mcp-ssh`. New env var → `.env.example` **and** a PR-body callout so operators mirror it into `/etc/mcp-ssh/mcp-ssh.env`; never hard-coded, never a secret in `deploy/mcp-ssh.service`.

11. **Watch + close.** `.deb` attached for the tag, GHCR image published, no secret in the audit trail. **Re-check the original symptom** with the step-3 command that proved it. Verify each `Fixes #NNN` flipped; close stragglers by hand linking the merged PR, then close the **parent** yourself. Broken → forward-fix on a branch. Auth bypass, secret leak, or a running-as-root regression → stop and tell the user.

12. **Leave the trail straight.** Update what your change invalidated: the `CLAUDE.md` module map and "Where to look", `docs/architecture.md`, `docs/usage.md` if the tool surface moved, the plan's `status.yml`. A doc that lies costs the next person a full re-audit (step 2). Defect that could recur → land the guard in the same PR; a colocated test asserting the invariant is where a house rule becomes unbreakable.

## Hard rules (from CLAUDE.md — non-negotiable)

Never log or return the password/token — not in responses, errors, or logs; secrets never reach a job id, log line, or filename. **Never run as root.** Always behind a TLS-terminating reverse proxy. Never ship without `MCP_SSH_ALLOWED_HOSTS`. Never weaken or bypass the auth middleware. **Never add an MCP tool to dodge a param** — constant 3-tool surface (`bash`/`job`/`file`); new capability = a new `action` or param. Typed errors (`thiserror` domain, `anyhow` only at the `main.rs` boundary); no `unwrap`/`expect` outside `main`+tests — a panic in the request path crashes every client. Newtype over bare primitives, illegal states unrepresentable, borrow by default. Async end-to-end — no `std::sync::Mutex` on the request path, never a guard across `.await`, no `block_on`, `spawn_blocking` for blocking I/O. `tracing` not `println`, a span per tool dispatch. Files ≤300 LOC, SRP. Concrete before abstract — a trait arrives with the second impl; default to deletion. `clippy -D warnings` is the floor. No force-push `main`, no `--no-verify`, no `git stash`. TLS, multi-host routing and rate limiting live in the reverse proxy — one host, one service user; not a fleet orchestrator, SSH client, job scheduler or secrets vault.

## Output

A sweep that fixes 40 of 90 findings is a success only if the other 50 are named.

```
Root cause:  <the one-line mechanism, for a bug sweep>
Primitive:   <name> @ <path>  (PR #NNN, merged)          [sweeps only]
Fixed:       <n> findings across <m> PRs → #… #…
Deferred:    <n> — <what, and why not now>               [never omit this line]
Falsified:   <doc/CLAUDE.md/status.yml claims that were wrong, now corrected>
Guards:      <tests/invariants added, or none>
Verify:      bin/check green   MCP: <tool calls exercised over /mcp>
Release:     v<X.Y.Z> → .deb + ghcr.io/developerz-ai/mcp-ssh:<X.Y.Z>   env asks: <VAR… or none>
Verified:    <symptom re-checked>   GlitchTip: <clean?>   service: <up?>
Issues:      #<parent> closed (<k> children)
```
