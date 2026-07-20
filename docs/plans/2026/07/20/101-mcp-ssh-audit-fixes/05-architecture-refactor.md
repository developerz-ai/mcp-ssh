# 05 — Architecture refactor

> Part of [`overview.md`](overview.md). Depends on: 01–04 (execute **last**; rebase these moves onto the bug-fixed code so no fix is lost). Each step must leave `bin/check` green.

The jobs side has no persistence boundary — raw `jobs` SQL is hand-written across `jobs/mod.rs`, `reaper.rs`, and `admin.rs`, while the token side has a real `oauth::Store` repository. Two modules exceed the 300-LOC / one-reason-to-change rule. Status is a stringly-typed magic constant in 4 files. File-op errors are stringly-typed. Docs drift.

Security note: job ids/log filenames stay `title`-derived only (`src/jobs/id.rs:69-83`) — no `cmd` text into any new type, query, or log line during the move.

## Files to change
- **JobRepo (HIGH).** Raw `jobs` SQL lives in `src/jobs/mod.rs:65-87,353-364,460-538`, `src/jobs/reaper.rs:171-182,227-233,339-343,377-387`, `src/admin.rs:39-66,102-153`. Introduce `src/jobs/store.rs` (`JobRepo`) owning every `jobs` query + the row⇄`JobState` mapping (`state_columns`/`state_from_columns`/`persist_final`/`page_from_tail`, `mod.rs:38-95`) + the startup-reconcile query. jobs/reaper/admin call typed methods, never SQL. Mirror `oauth::Store`. This shrinks `mod.rs` and `reaper.rs` under 300.
- **`JobStatus` enum (HIGH).** `"running"`/`"exited"`/`"failed"` literals in `jobs/mod.rs:40-56`, `reaper.rs:200`, `admin.rs:78,119`, `db.rs:28` (comment). `state_from_columns` (`mod.rs:49-59`) silently coerces unknown → `Failed`. One `JobStatus` enum with `as_str`/`from_str` in a single module; compare variants, not strings. Keep the on-disk string values byte-identical so existing DBs read unchanged.
- **`src/jobs/mod.rs` (549 LOC → split).** Bundles `Shell` launcher (`:144-181`), `JobStore` engine (`:264-414`), row mapping (`:38-95`), reconcile closure (`:216-247`), persisted read/list (`:460-538`). Move `Shell` → `src/jobs/shell.rs`; mapping + persisted queries → `JobRepo`; reconcile → a named `JobRepo` method. Engine (run/poll/kill) stays, reads top-to-bottom.
- **`src/jobs/reaper.rs` (394 LOC → split).** Signalling (`kill_job`/`kill_group`/`group_alive`/`signal_group`/`exited_within`, `:44-134`) is a separate concern from retention/trim (`spawn_reaper`/`reaper_pass`/`reap_once`/`compact_once`/`reap_orphans`/`trim_log`, `:138-393`) and is shared with admin/`job kill`. Split into `src/jobs/signal.rs` (kill/group primitives) and `reaper.rs` (retention). Keep call-sites from slices 02/03 working.
- **Consolidate the two kill paths.** `JobStore::kill` (`mod.rs:542-547`, in-memory) and `admin::kill_job` (`admin.rs:102-153`, from persisted row) duplicate the running-check + failed-transition. After slice 02 both should route through `signal.rs` + `JobRepo` — one kill semantics, invoked with a live handle or a persisted pgid.
- **`FileError` (MEDIUM).** `src/tools/files.rs:26-201` returns `Result<String,String>` via `.map_err(|e| e.to_string())`, discarding the typed `ShError` (`:206-216`), and bakes presentation (`"wrote {n} bytes to {path}"`, move/`ls`-redirect hints) into the domain layer. Add a `FileError` (thiserror) + structured returns; move human-string rendering into `src/tools/mod.rs` next to `render`/`ok`/`err`.
- **`oauth::Store` error type (LOW).** `store.rs:74-149` returns `Result<T,&'static str>` (wire codes). Optional: a `GrantError` enum keeping the wire-string mapping in one place. Lowest priority.
- **`tools/mod.rs` (360 LOC, LOW).** Move arg structs + `lenient_action` (`:28-149`) to `src/tools/args.rs`, leaving the three dispatchers + `ServerHandler`. Optional.
- **Docs (LOW).** `src/auth.rs:1-5` + CLAUDE.md module map call `auth.rs` "HTTP Basic middleware" but it's bearer-only (`require_auth`, `:60-70`); Basic survives only in `/authorize`. The map's "db.rs owns job metadata" is false (SQL is scattered — now fixed by JobRepo). Update the map + header to match reality.

## Steps
1. `JobStatus` enum first (small, unblocks the rest): define it, replace literals across the 4 files, keep on-disk strings identical, `from_str` returns an error/`Failed` explicitly (not silent coerce).
2. `src/jobs/signal.rs`: move the signalling primitives out of `reaper.rs`; update slice-02 call-sites.
3. `src/jobs/store.rs` (`JobRepo`): move all `jobs` SQL + row mapping + reconcile here; jobs/reaper/admin call typed methods. Delete the inline SQL.
4. `src/jobs/shell.rs`: move `Shell`. Confirm `jobs/mod.rs` and `reaper.rs` now ≤300 LOC.
5. Consolidate kill onto `signal.rs` + `JobRepo`.
6. `FileError` + move presentation to `tools/mod.rs`.
7. Optional: `GrantError`, `tools/args.rs`.
8. Update CLAUDE.md module map + `auth.rs` header.

## Tests
- No behavior change intended — the full existing suite (job lifecycle, reconcile, reaper, admin, oauth, file ops) plus the slices 01–04 tests must stay green before and after each move.
- Add a `JobStatus::from_str` round-trip test asserting on-disk strings map exactly and an unknown string is handled explicitly.
- `JobRepo`: a focused test that its methods produce the same rows the old inline SQL did (spot-check insert→poll→list→reconcile).
- Gate: `bin/check` after **each** step, not just at the end.

## Done when
- All `jobs` SQL lives in `JobRepo`; no raw `jobs` query remains in `mod.rs`/`reaper.rs`/`admin.rs`.
- One `JobStatus` enum; no status string literals outside it.
- `jobs/mod.rs` and `reaper.rs` are ≤300 LOC and single-responsibility; `Shell` and signalling are their own modules.
- File ops return typed `FileError`; presentation lives in the tool adapter.
- CLAUDE.md module map + `auth.rs` header are accurate. Full suite + `bin/check` green.
