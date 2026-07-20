# 03 — Token GC + DB durability

> Part of [`overview.md`](overview.md). Depends on: none.

Expired OAuth tokens are only ever deleted lazily when re-presented — so every refresh leaks the old access-token row forever. `issue()` isn't transactional. New post-v1 columns aren't driven through the migration mechanism.

Security note: this slice touches token storage. Never log or return token values. The sweep is a bulk `DELETE ... WHERE expires_unix <= ?` — it must not select, log, or echo token strings.

## Files to change
- `src/oauth/store.rs:129-150` (`issue`) + `src/jobs/reaper.rs` (reaper pass) — MEDIUM. Expired `access_tokens`/`refresh_tokens` rows accumulate unboundedly: `validate` (`store.rs:187`) / `refresh` (`store.rs:111`) delete only the exact token re-presented, and after a refresh the old access token is never presented again. Admin's "expired" bucket (`admin.rs:184-204`) counts the growing dead rows. Add a periodic + startup sweep `DELETE FROM access_tokens WHERE expires_unix <= ?` and same for `refresh_tokens`, invoked from the existing reaper pass (`reaper.rs` `reaper_pass`/`reap_once`) alongside the job reaping. The in-memory `codes` map is already swept (`store.rs:61`) — mirror that discipline for the durable tables.
- `src/oauth/store.rs:135-148` (`issue`) — LOW. The two `INSERT`s (access, then refresh) run in autocommit; a mid-failure leaves an orphan access token with no refresh. Wrap both in one `conn.transaction()`.
- `src/db.rs:104-117` (`ensure_column`) + `:25-34,55` — MEDIUM (latent). Only `pgid` (`:55`) is forward-migrated; `title` and `output_tail` exist solely in `CREATE TABLE` (`:27,32`). A DB created before those columns existed would break every `INSERT`/`UPDATE` touching them (`jobs/mod.rs:356,78`). Drive all post-v1 columns through `ensure_column` (or add a `PRAGMA user_version` ledger). Since they may have shipped from day one, this is fragility-hardening — confirm before adding churn; at minimum add the `ensure_column` calls for `title`/`output_tail` so the pattern is uniform.

## Steps
1. Add repo methods (or, pre-05, free functions in `store.rs`) `sweep_expired_access(now)` / `sweep_expired_refresh(now)` doing a single bound `DELETE`. Return the deleted count for a `tracing::debug!` (count only — never token values).
2. Call both from the reaper pass in `reaper.rs` (startup + hourly) next to the job reap, using the same `now_unix()` source the store uses.
3. Wrap `issue()`'s two inserts in a transaction; commit once.
4. Route `title` + `output_tail` through `ensure_column` in `db.rs` open path, matching the `pgid` precedent (`:55`). Keep it idempotent (`ensure_column` already is — tested at `db.rs:176`).

## Tests (colocated `#[cfg(test)]`)
- `store.rs`: issue tokens with a past `expires_unix`, run the sweep, assert both tables emptied of expired rows and live rows retained; assert no token string is returned/logged (sweep returns a count).
- `store.rs`: `issue()` transactionality — no orphan access row on a simulated refresh-insert failure (if practically testable; otherwise assert both rows present after success).
- `db.rs`: open a connection with a **legacy** `jobs` table missing `title`/`output_tail`, run the migration, assert the columns are added and a subsequent insert succeeds (fills the `Db::open` real-path test gap noted in the audit).
- Gate: `bin/check`.

## Done when
- Expired access/refresh tokens are swept on startup and hourly; DB does not grow one dead row per refresh.
- `issue()` is atomic. Legacy-DB job columns are migrated uniformly, proven by a migration test. `bin/check` green.
