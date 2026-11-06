---
description: Add or change a durable column in the bundled SQLite schema. Use for schema, migration, ensure_column, db.rs, jobs table, token table.
applies: [rust]
---

> House skill — established by dz-runner setup mode from this repo's own evidence, then owned by its maintainers. Correct it in place; setup never overwrites a skill that exists.

1. Read `src/db.rs` — `SCHEMA` is the create-path, `Db::open` runs the forward-only migrations.
2. For a fresh column, append an idempotent call in `Db::open` beside the existing ones:
   `ensure_column(&conn, "jobs", "<column>", "<SQL decl>")?;`
   `CREATE TABLE IF NOT EXISTS` will NOT add a column to a table that already exists — the `ensure_column` call is the migration.
3. Keep `table`/`column`/`decl` as compile-time constants; they are interpolated into DDL and `ensure_column` assumes they are not user input.
4. Add the column to `SCHEMA` as well, so a fresh database and a migrated one agree.
5. Put the query itself in the owning repository — `src/jobs/store.rs` (`JobRepo`) or `src/oauth/store.rs` — never in `db.rs`.
6. Every access goes through `Db::call(|conn| ...)`; the guard stays inside the closure, never across an `.await`.
7. Prove it with a legacy-database test modelled on `open_migrates_a_legacy_jobs_table_missing_new_columns` in `src/db.rs`: build the old narrower table on a real file, open it, assert the migrated table accepts an insert.
8. Run `bin/check` — `fail -> fix before continuing`, do not weaken the test.

Note: SQLite is `rusqlite` with `bundled`; it is compiled into the binary. Never add a system libsqlite dependency.
