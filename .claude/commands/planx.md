---
description: Write a concise, self-contained execution plan to docs/plans/<YYYY>/<MM>/<DD>/<1NN>-<slug>/ for another AI to implement
argument-hint: [what you want done]
allowed-tools: Write, Read, Glob, Grep, Task, Bash
---

# /planx

Produce a concise plan another AI can execute with zero extra context. Plan only — no implementation, no code execution, no edits outside the plan dir. For a small single-file change use `/plan` instead; `/planx` is for multi-slice work worth tracking.

## Goal
$ARGUMENTS

## Steps

1. **Resolve path.** Run `date +%Y`, `date +%m`, `date +%d`. Dir = `docs/plans/<YYYY>/<MM>/<DD>/`. `Glob docs/plans/<YYYY>/<MM>/<DD>/1*` → next number = highest existing `1NN-*` + 1, else `101`. Slug = kebab-case title, max 5 words. Final plan dir: `docs/plans/<YYYY>/<MM>/<DD>/<1NN>-<slug>/`.

2. **Explore.** `Task` (subagent_type=Explore, thoroughness="very thorough"): existing patterns + files to touch (`file:line`), the right module(s) under `src/` (start from the "Where to look" / "Module map" tables in `CLAUDE.md` — `src/tools/`, `src/jobs/`, `src/oauth/`, `src/auth.rs`, `src/db.rs`, `src/config.rs`, `src/app.rs`), tests (colocated `#[cfg(test)]` unit vs `tests/` integration), the MCP tool surface (`src/tools/mod.rs` — 3 tools, parametrized), durable-state shape (`src/db.rs`), gotchas. Prefer `codegraph_explore` for structural lookups. Skip only for trivial asks.

3. **Write the plan as multiple files** in the plan dir — never one big `plan.md`. Always produce an `overview.md` index plus one `<NN>-<aspect>.md` per separable area (e.g. `01-tool-surface.md`, `02-jobs.md`, `03-db-schema.md`, `04-auth.md`, `05-tests.md`). Split by area of work so each file is independently executable and stays short. Match the house style — terse fragments, `file:line` refs, tables, `Module#method` symbol refs.

   **`overview.md`** — the map. Sections:

```markdown
# <Title>

## Goal
1-2 sentences: what + why.

## Context
- Stack facts the executor needs (Rust 2024, tokio, axum 0.8, rmcp Streamable HTTP MCP at `/mcp`, SQLite via `rusqlite` bundled, OAuth 2.1 + HTTP Basic auth — only what's relevant).
- Reference patterns: `src/<mod>.rs:12` — follow this for Z.

## Plan files (execute in order)
1. [`01-<aspect>.md`](01-<aspect>.md) — one line: what it covers.
2. [`02-<aspect>.md`](02-<aspect>.md) — ...

## Done when
- Verifiable acceptance criteria spanning the whole feature.

## Risks / open questions
- Anything the executor must decide or watch.
```

   **Each `<NN>-<aspect>.md`** — one slice of work. Sections:

```markdown
# <NN> — <Aspect>

> Part of [`overview.md`](overview.md). Depends on: <NN-prior or "none">.

## Files to change
- `path:line` — what changes, why. (Keep files ≤300 LOC; split by responsibility.)

## Steps
1. Ordered, concrete actions. Reference `Type#method` / `file:line`, don't restate.

## Tests
- What to add/run. Tests written with the code — colocated `#[cfg(test)]` for logic, `tests/` for MCP-over-HTTP integration. Gate: `bin/check` (fmt --check + clippy -D warnings + test).

## Done when
- Verifiable acceptance criteria for this slice.
```

4. **Write a `status.yml`** in the plan dir (alongside `overview.md`) — the live tracker for this plan. New plans start `not_started` / `0%`. Get `created_by` + `owner` from `git config user.name` (the person running /planx). Leave `worked_by` empty — the executor sets it to their own `git config user.name` when they pick the plan up. Shape:

```yaml
plan: <1NN>-<slug>
title: <human title from overview.md>
status: not_started        # not_started | in_progress | blocked | complete | superseded
created_by: <git config user.name>   # who authored the plan
worked_by: ""              # who is executing it; empty = unclaimed; executor fills with their git user.name
owner: <git config user.name>
percent: 0                 # 0–100, overall completion
current_focus: ""          # where it's at right now / next slice to pick up
slices:                    # one row per <NN>-<aspect>.md slice
  - file: 01-<aspect>.md
    status: not_started      # not_started | in_progress | complete
    percent: 0
evidence: []               # commits/PRs proving progress, e.g. ["#18", "abc1234"]
notes: ""
last_updated: <YYYY-MM-DD>
```

   Keep `status.yml` machine-readable (valid YAML, the enums above). It's the one file in the plan dir that IS a tracker — the `.md` slices stay reference maps (no checkboxes there).

## Rules
- Compact English. Fragments over sentences. `file:line` and `Type#method` symbol refs over prose. Tables for structured data.
- Reference-only: point at code, don't paste it or re-explain it ("follow `src/jobs/mod.rs` but ...").
- No checkboxes (`[ ]`). Plain bullets. The plan is a reference map, not a tracker.
- Multiple files always: `overview.md` + `<NN>-<aspect>.md` slices. Never a single `plan.md`.
- Self-contained: executor reads only `overview.md`, the slice it's on, and the files those cite.
- Respect `CLAUDE.md` + `docs/architecture.md`: typed errors (`thiserror` domain, `anyhow` only at the `main.rs` boundary); no `unwrap`/`expect` outside `main`+tests; newtype over bare primitives, make illegal states unrepresentable; borrow by default; async end-to-end (no `std::sync::Mutex` on the request path, no `block_on`, offload blocking I/O with `spawn_blocking`); `tracing` not `println`; files ≤300 LOC, one reason to change; concrete before abstract (introduce a trait at the second impl); default to deletion. `clippy -D warnings` is the floor.
- **Never add an MCP tool to dodge a param** — the surface is a constant 3 tools (`bash`/`job`/`file`); new capability = a new `action`/param on an existing tool. A plan that adds a 4th tool is wrong unless the goal explicitly demands it.
- Security invariants (from CLAUDE.md NEVER): never log/return password or token; never run as root; always behind a TLS reverse proxy; `MCP_SSH_ALLOWED_HOSTS` always set; never weaken the auth middleware. Any slice touching auth/`src/oauth/`/secrets/job-id or filename generation must call out that secrets can't leak into an id, log line, or filename.
- Infra note: TLS, multi-host routing, and rate limiting live in the reverse proxy, not the binary — a plan that needs them documents the proxy config, it doesn't add them to `src/`.

## Output
```
✓ docs/plans/<YYYY>/<MM>/<DD>/<1NN>-<slug>/overview.md
  + 01-<aspect>.md, 02-<aspect>.md, … (one per area)
  + status.yml (tracker — status/owner/percent/current_focus)
Next: run an executor on overview.md.
```
