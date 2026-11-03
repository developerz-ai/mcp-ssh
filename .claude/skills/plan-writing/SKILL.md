---
description: Writing an execution plan under docs/plans for another agent. Use for plan, planx, docs/plans, overview, slice.
applies: [rust]
---

> House skill — established by dz-runner setup mode from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Resolve the path: run `date +%Y`, `date +%m`, `date +%d`. Dir = `docs/plans/<YYYY>/<MM>/<DD>/`.
2. Find the next number: `Glob docs/plans/<YYYY>/<MM>/<DD>/1*` — next number = highest existing `1NN-*` + 1, else `101`. Slug = kebab-case title, max 5 words.
3. Explore before writing: read the modules the task touches, starting from the "Where to look" table in `CLAUDE.md` (`src/tools/`, `src/jobs/`, `src/oauth/`, `src/auth.rs`, `src/db.rs`, `src/config.rs`, `src/app.rs`). Note `file:line` refs and the `Module#method` symbols involved.
4. Write multiple files in the plan dir, never one big `plan.md`: always an `overview.md` index plus one `<NN>-<aspect>.md` per separable area (e.g. `01-tool-surface.md`, `02-jobs.md`, `03-db-schema.md`, `04-auth.md`, `05-tests.md`).
5. In `overview.md` state the goal, the file list with `file:line` refs, one verification per step (`step -> verify: check`), and the assumptions worth challenging.
6. Keep each aspect file independently executable and short — terse fragments, tables, `file:line` refs, `Module#method` symbol refs.
7. Name the verification for every step as `check` (the repo's gate) unless a specific test proves it: `cargo test <pattern>`.
8. Do not implement, and do not edit anything outside the plan dir while planning.
