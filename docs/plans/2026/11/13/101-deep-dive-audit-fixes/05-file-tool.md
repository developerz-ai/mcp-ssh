# 05 — `file` tool safety + agent-facing quality

> Part of [`overview.md`](overview.md). Depends on: 01 (read bounds live there), 02 (kill outcome rendering lives there).

Surface stays `bash`/`job`/`file`. Everything here is behaviour on existing `action`s or an existing param (`recursive`).

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| F1 | HIGH | `delete path="link/"` (symlink to dir, trailing slash): recurses into the **target**, empties it, then errors `Not a directory (os error 20)` | [E2E] `target/keep.txt` gone. `src/tools/files.rs:246-250`; existing test covers only the no-slash form (`files/tests.rs:211`) |
| F2 | MED | `delete` removes whole trees with no opt-in although `recursive` is already a param (documented `[list, grep]`, ignored by delete) | [E2E] populated dir deleted in one call. `src/tools/mod.rs:181-186`, `src/tools/args.rs:118` |
| F3 | MED | `move` onto an existing **directory** says "delete it or choose another path" → following the hint `remove_dir_all`s e.g. `/srv/releases` | [E2E] message reproduced. `src/tools/files.rs:261-265`, `src/tools/mod.rs:260-262` |
| F4 | MED | Failed `move` leaves stray dirs: `ensure_parent(dest)` runs before `rename` (missing src; dir into itself) | [E2E] `q/r` created by a failed move. `src/tools/files.rs:266-267` |
| F5 | MED | Cross-filesystem `move` → bare `Invalid cross-device link (os error 18)`; `docs/usage.md:119`'s own example (`/tmp` → `/srv`) is that case | [E2E] |
| F6 | MED | Partial success flagged as error: recursive `list` with one unreadable subdir → `isError=true`, body `exit status 1: <full listing>`; same for `grep -r` exit 2 | [E2E] `src/tools/files/shell.rs:176-185`, `src/tools/files.rs:300,312-318` |
| F7 | MED | I/O errors carry no path/op: `read` missing file, `write` onto a dir, `path=""`, failed move, bad `bash cwd` all → bare errno | [E2E] `src/tools/mod.rs:264`, `src/tools/files.rs:47-48` |
| F8 | MED | No-clobber `move` is check-then-rename (TOCTOU) — the comment at `files.rs:258` promises otherwise | [READ] |
| F9 | LOW-MED | Unknown `job` action **string** silently becomes `list` (`"stop"`, `"cancel"`, `"KILL"`) — pinned by a test (`src/tools/mod.rs:367`). Leniency was meant for non-string placeholders | [E2E] `action="frobnicate"` → job list. `src/tools/args.rs:64-69` |
| F10 | LOW | Unknown/irrelevant params silently ignored (`recurse=true`, `offset=200`) — no `deny_unknown_fields` | [E2E] `src/tools/args.rs:7,75,108` |
| F11 | LOW | `read`: empty file and cursor-past-EOF both return `""`; file of exactly `limit` lines emits a spurious next page | [E2E] `src/tools/files.rs:115-117,158-164` |
| F12 | LOW | Agent wording outside `src/tools/mod.rs`: advice inside `ShError` Display (`files/shell.rs:39`), truncation sentence pushed into `Ok(String)` (`:154-157`), read footer (`files.rs:75,173-175`); output of exactly 65 536 bytes mislabelled truncated (`shell.rs:195`, `>=`) | [READ] |
| F13 | LOW | Schema advertises job ids as `<title>-HH:MM:SS`; real format is `HH-MM-SS` | [READ] `src/tools/mod.rs:40`, `src/tools/args.rs:22-23` |
| F14 | LOW | Dispatch span lacks `action` and outcome; args rejected by rmcp before the body log nothing | [READ] `src/tools/mod.rs:102,145,158,211`; rejection timing [PLAUS] |
| F15 | LOW | `write` is truncate-then-write (ENOSPC/crash ⇒ empty `.env`) | [READ] `src/tools/files.rs:218` |
| F16 | LOW | Wording nits: `grep` on a dir without `recursive` gives no hint; description doesn't say the pattern is a basic regex; `bg=true` says "still running after the inline window"; empty `cmd` accepted and creates a job; recursive `list` guards only a leading `-` (`(`, `!` still parse as `find` expressions); `bash timeout` uncapped | [E2E] first four; rest [READ] |

Sound (no action): no shell interpolation (argv via `Command::args`), `--` before grep pattern/ls path, `find` doesn't follow symlinks, `-maxdepth 20`, runner kills+reaps on cap/timeout, no `cmd`/content/path in spans, no request-path panics.

## Files to change
- `src/tools/files.rs` — delete (F1, F2), move (F3, F4, F5, F8), read outcomes (F11), `FileError::Io { op, path, source }` (F7), atomic write (F15).
- `src/tools/files/shell.rs` — typed `Output { text, truncated }`, partial-success outcome, prose-free `ShError` (F6, F12).
- `src/tools/mod.rs` — all rendering; empty-string arg rejection; `action` in span; description fixes (F7, F9, F13, F14, F16).
- `src/tools/args.rs` — `JobAction::Unknown(String)` via `lenient_action`; `deny_unknown_fields`; `recursive` doc (`[list, grep, delete]`).
- `src/jobs/mod.rs:326-330` — clamp `timeout`.

## Steps
1. **F1:** strip trailing slashes before `symlink_metadata`; symlink ⇒ `remove_file`. Ship with the regression test first (it currently destroys data).
2. **F2** (owner decision — recommended): non-empty directory without `recursive=true` ⇒ `FileError::DirectoryNotEmpty`; hint rendered in `mod.rs`. Empty dir and files unchanged.
3. **Move** (F3, F4, F5, F8): order = `symlink_metadata(src)` (typed `SourceMissing`) → reject dest under src (typed) → dest exists? `DestinationIsDirectory` (hint: pass the full path incl. filename; never suggest delete) or `DestinationExists` → `ensure_parent` → `renameat2(RENAME_NOREPLACE)` (via `libc`, added in 02; map `EEXIST`) → on `ErrorKind::CrossesDevices` for a regular file: copy to a temp sibling of dest + rename + remove src; for a directory: typed error hinting `bash mv`.
4. **F6:** non-zero exit + non-empty stdout ⇒ `FileOutcome::Partial { text, note }`, rendered as success with a trailing note (`some entries unreadable`). Exit non-zero + empty ⇒ error as today.
5. **F7:** `FileError::Io { op: &'static str, path, source }`; adapter rejects empty `path`/`src`/`dest`/`pattern`/`cmd` up front (extend `file_dispatch_rejects_missing_arguments`).
6. **F9, F10** (owner decision): keep null/bool/number → `list`; unrecognised string ⇒ `JobAction::Unknown(s)` ⇒ error naming `poll|list|kill`. Add `#[serde(deny_unknown_fields)]` to the three arg structs. Update the test at `mod.rs:367` to the new contract.
7. **F11:** after the window fills, peek once for EOF; typed `Empty` / `PastEnd { total }` outcomes.
8. **F12, F13, F16:** move every sentence into `render_file`/`render_file_error`; fix `>=` → `>`; correct id format strings; `bg` reply wording; grep-on-dir hint; say "basic regex"; clamp `bash timeout` to a sane max (e.g. 600 s) and reject empty `cmd`.
9. **F14:** add `action` (enum name only — never path/content/cmd) to the span; one outcome event (`ok`/`error`).
10. **F15** (owner decision): temp file in the same dir + `rename`, preserving mode and following an existing symlink target. Skip if hardlink/inode stability matters more.

## Tests
- `delete("link/")` ⇒ `target/keep.txt` survives. Populated dir without `recursive` errors and is intact; with it, removed.
- Move: onto dir ⇒ message lacks "delete it"; missing src ⇒ dest parent not created, message names src; dir into itself ⇒ typed error, src unchanged; `EEXIST`/`CrossesDevices` mapping unit tests.
- Recursive list over a `chmod 000` subdir ⇒ not an error, contains readable entries + note. (Restore the mode in the test.)
- `{"action":"stop","id":"x"}` ⇒ error naming valid actions; `{"action":null}` still lists; `recurse=true` ⇒ deserialize error.
- 2-line file `limit=2` ⇒ no footer; past-EOF ⇒ mentions line count.
- `ShError::Timeout` Display has no advice; rendered string does. Schema strings contain `HH-MM-SS`, not `HH:MM:SS`.
- Tracing test extended to `file` and `job`: `action=` present, content text absent. Gate: `bin/check`.

## Done when
- No `file` action can destroy data the agent didn't name (symlink target, non-empty tree without opt-in, clobbered dest).
- Every error names the operation and path and, where there is one, the next step.
- All agent-facing sentences are in `src/tools/mod.rs`.
