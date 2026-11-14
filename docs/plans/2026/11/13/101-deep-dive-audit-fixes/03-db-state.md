# 03 — Durable state (SQLite, stores, admin CLI)

> Part of [`overview.md`](overview.md). Depends on: none (02's boot-id column and all of 04's schema changes depend on step 2 here).

## Findings
| ID | Sev | Finding | Evidence |
|---|---|---|---|
| D1 | HIGH | DB, `-wal`, `-shm` created with the process umask; state dir via plain `create_dir_all`. Holds plaintext bearer + 1-year refresh tokens; service user has `NOPASSWD:ALL` ⇒ local user → root. Job logs (command output, may contain secrets) likewise | [E2E] `mcp.db*` 0644, job dir 0775, logs 0664. `src/db.rs:57`, `src/main.rs:85-88`, `src/jobs/mod.rs:196,363-367` |
| D2 | MED | No schema version: `CREATE IF NOT EXISTS` + `ensure_column` only. Can't express a non-additive change (needed by 04), an older binary opens a newer DB silently, two processes race `ensure_column` | [READ] `src/db.rs:61-67,116-129` |
| D3 | MED | DB errors on the auth path swallowed silently: `validate` → `unwrap_or(false)`; others `map_err(\|_\| ServerError)`. A full/locked/corrupt DB looks like "invalid token" → every client re-auths, journal empty | [READ] `src/oauth/store.rs:95,116,190,227,273` |
| D4 | MED | Store errors reach the agent as "no jobs"/"no such job" (`list` → `[]`, `poll_persisted` → `Ok(None)`); one bad row fails the whole listing (`collect::<Result<Vec>>`) | [READ] `src/jobs/mod.rs:431-434,451-457`, `src/jobs/store.rs:197` |
| D5 | MED | Bulk writes commit per row under the connection mutex (`fail_running`, `delete`) — blocks bearer validation; partial batches on failure | [READ] `src/jobs/store.rs:243-260,345-354` |
| D6 | MED | Raw agent-supplied `title` persisted (only the id is normalized) and printed raw by `mcp-ssh jobs` → terminal escape / row forging in the operator's (often root) terminal | [READ] `src/jobs/store.rs:116-124`, `src/admin.rs:45-56` |
| D7 | LOW | Admin commands aren't read-only: `Db::open` creates the file + may `ALTER`. Typo'd `MCP_SSH_DB` prints "no active jobs" for an empty new DB; `sudo mcp-ssh sessions` before first start creates a root-owned DB | [READ] `src/admin.rs:13-15` |
| D8 | LOW | `mcp-ssh job kill` exits 0 on every outcome | [E2E] `job kill nope` → `no such job`, exit 0 |
| D9 | LOW | `busy_timeout` set after `journal_mode` → first pragma can hit "database is locked" when CLI + server start together | [PLAUS] `src/db.rs:58-59` |
| D10 | LOW | No constraints behind the row mapping: `status` unchecked, nullable PK, `c as i32` wraps | [READ] `src/db.rs:34-43`, `src/jobs/store.rs:398` |
| D11 | LOW | `now_unix()` falls back to `0` on clock error → every token unexpired (fails open) | [PLAUS] `src/db.rs:133-138` |

Not a finding: no index on `expires_unix`/`started_unix` — irrelevant at these table sizes. `foreign_keys=ON` is inert (no FKs).

## Files to change
- `src/db.rs` — open flags/mode, pragma order, `user_version` migrations, `open_existing`, `now_unix`.
- `src/main.rs:85-88` — state dir 0700.
- `src/jobs/mod.rs:196,363-367` — job dir 0700, logs 0600.
- `src/oauth/store.rs` — log store errors (never the token), distinguishable error.
- `src/auth.rs` — store failure ⇒ 503, not 401.
- `src/jobs/store.rs` — batched transactions, per-row mapping, normalized title, `i32::try_from`.
- `src/jobs/mod.rs` — `list`/`poll` return `Result`; `src/tools/mod.rs` renders "job store unavailable".
- `src/admin.rs` — `open_existing`, exit codes, control-char escaping.
- `deploy/mcp-ssh.service` — `UMask=0077`, `StateDirectoryMode=0700` (unit edit itself lands in slice 06; note the dependency).

## Steps
1. **Modes** (D1): create the DB file 0600 before SQLite opens it (same approach as `write_secret_file`, `src/config.rs:209-220`), chmod existing DB/`-wal`/`-shm` to 0600 on open, state + job dirs 0700, job logs opened with mode 0600. Tighten pre-existing loose files loudly-on-failure like `set_auth_in` does.
2. **Versioned migrations** (D2): ordered `&[fn(&Transaction)]` keyed on `PRAGMA user_version`; each step in its own `BEGIN IMMEDIATE` txn; refuse to open when `user_version` > binary max (typed `DbError::NewerSchema`). Step 1 = adopt the current schema (idempotent for existing DBs). Removes the `ensure_column` race.
3. **Pragma order** (D9): `busy_timeout` first.
4. **Error surfacing** (D3, D4): `oauth::Store::validate` returns `Result<bool, StoreError>`; `tracing::warn!` at the store boundary with the rusqlite error only; middleware maps `Err` → 503. `JobStore::list`/`poll` return `Result`; map rows individually and skip+warn a corrupt one.
5. **Transactions** (D5): one txn per batch in `fail_running` and `delete`.
6. **Title** (D6): persist the normalized label (`src/jobs/id.rs` normalizer) — or keep raw and escape control chars + cap length in `render_jobs`. Recommend persisting normalized: one representation, nothing to escape.
7. **Admin** (D7, D8): `Db::open_existing` (no `SQLITE_OPEN_CREATE`, no migrations, error if missing; refuse a newer schema). `admin::kill` returns a non-zero exit for anything but `Killed` — map in `main.rs` (anyhow boundary).
8. **Constraints** (D10): migration step adding `CHECK(status IN …)`/`NOT NULL` (table rebuild inside the txn); `i32::try_from` in the row mapping, out-of-range ⇒ `Failed{corrupt}`.
9. **Clock** (D11): `now_unix` returns `Result` or `i64::MAX` on error — fail closed.

## Tests
- `Db::open` on a tempdir under umask 022 → file mode 0600 (and `-wal` after a write); reopening a 0644 DB tightens it. Job dir/log modes.
- Migrations: legacy v0 fixture (real old schema, not a hand-copied INSERT — fix the stale `JobStore::spawn` reference at `src/db.rs:245-253`) reaches max and reopen is a no-op; `user_version = max+1` → error; failing step rolls back.
- Two connections on one file (server + CLI model): CLI `open_existing` while the server holds a write txn succeeds within `busy_timeout`.
- `validate` with `access_tokens` dropped → `Err` + a warn event with no token text; middleware → 503.
- `listing` with one row `started_unix='x'` still returns the good rows.
- `render_jobs` with title `"a\n\x1b[2Jb"` → no control bytes.
- `open_existing` on a missing path errors and creates nothing; `job kill` on a missing id → non-zero.
- These need file-backed DBs — today every store test uses `Db::memory()` (`src/db.rs:75`), which skips WAL/`busy_timeout`/migrations. Gate: `bin/check`.

## Done when
- Fresh install: `stat` shows DB 0600, dirs 0700, logs 0600.
- Schema changes are a new migration fn, nothing else.
- A DB outage is a 503 with a log line, not a 401 or an empty list.
- Admin read commands never create or alter a database.
