//! Local admin subcommands: inspect and manage the server's durable state from a
//! shell on the host — `mcp-ssh jobs`, `mcp-ssh job kill <id>`, `mcp-ssh sessions`.
//!
//! They read/write the same SQLite database the running server uses (concurrent
//! WAL access is safe) and deliberately never construct a [`JobStore`]: doing so
//! would start a reaper and a startup reconcile that flips the live server's
//! `running` rows to `failed`. Here we only ever touch the database directly.
use crate::db::{Db, now_unix};

/// Open the database the running server uses. Idempotent — applies the same schema
/// and pragmas, safe to run alongside the daemon (SQLite WAL allows it).
fn open_db() -> anyhow::Result<Db> {
    Db::open(&crate::config::db_path()?)
}

// ---- jobs ----

struct JobRow {
    id: String,
    status: String,
    code: Option<i64>,
    error: Option<String>,
    started_unix: i64,
    title: Option<String>,
}

/// `mcp-ssh jobs [--all]` — list running jobs (or every job, most-recent first).
pub async fn jobs(all: bool) -> anyhow::Result<()> {
    let db = open_db()?;
    let rows = fetch_jobs(&db, all).await?;
    if rows.is_empty() {
        println!("{}", if all { "no jobs" } else { "no active jobs" });
    } else {
        print!("{}", render_jobs(&rows));
    }
    Ok(())
}

async fn fetch_jobs(db: &Db, all: bool) -> anyhow::Result<Vec<JobRow>> {
    let rows = db
        .call(move |conn| {
            // Active-only by default; `--all` includes finished jobs (capped, newest
            // first, so a long-lived box's history can't flood the terminal).
            let sql = if all {
                "SELECT id, status, code, error, started_unix, title \
                 FROM jobs ORDER BY started_unix DESC LIMIT 200"
            } else {
                "SELECT id, status, code, error, started_unix, title \
                 FROM jobs WHERE status = 'running' ORDER BY started_unix DESC"
            };
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(JobRow {
                    id: r.get(0)?,
                    status: r.get(1)?,
                    code: r.get(2)?,
                    error: r.get(3)?,
                    started_unix: r.get(4)?,
                    title: r.get(5)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    Ok(rows)
}

fn render_jobs(rows: &[JobRow]) -> String {
    let id_w = rows.iter().map(|r| r.id.len()).max().unwrap_or(2).max(2);
    let mut out = format!(
        "{:<id_w$}  {:<8}  {:>4}  {:<19}  {}\n",
        "ID", "STATUS", "CODE", "STARTED", "TITLE / ERROR"
    );
    for r in rows {
        let code = r.code.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
        // For a failure the error is the useful column; otherwise the title.
        let note = match (r.status.as_str(), r.error.as_deref()) {
            ("failed", Some(e)) => e,
            _ => r.title.as_deref().unwrap_or("-"),
        };
        out.push_str(&format!(
            "{:<id_w$}  {:<8}  {:>4}  {:<19}  {}\n",
            r.id,
            r.status,
            code,
            fmt_time(r.started_unix),
            note,
        ));
    }
    out
}

// ---- job kill ----

/// `mcp-ssh job kill <id>` — signal a running job's process group dead.
pub async fn kill(id: &str) -> anyhow::Result<()> {
    let db = open_db()?;
    println!("{}", kill_job(&db, id).await?);
    Ok(())
}

async fn kill_job(db: &Db, id: &str) -> anyhow::Result<String> {
    let lookup = id.to_string();
    let row: Option<(String, Option<i64>)> = db
        .call(move |conn| {
            use rusqlite::OptionalExtension;
            conn.query_row(
                "SELECT status, pgid FROM jobs WHERE id = ?1",
                [lookup],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()
        })
        .await?;

    let Some((status, pgid)) = row else {
        return Ok(format!("no such job: {id}"));
    };
    if status != "running" {
        return Ok(format!("job {id} is not running (status: {status})"));
    }
    let Some(pgid) = pgid else {
        return Ok(format!(
            "job {id} has no recorded process group (started before pgid tracking) — cannot kill from the CLI"
        ));
    };

    // A pgid outside u32 is a corrupt row; a raw `as` cast would wrap it into a
    // real (wrong) process group and signal that instead.
    let Ok(pgid) = u32::try_from(pgid) else {
        return Ok(format!(
            "job {id} has a corrupt pgid ({pgid}) — refusing to signal"
        ));
    };
    let killed = crate::jobs::kill_group(pgid).await;
    // Record the kill only if the row is still `running`: when the server owns the
    // job, its waiter may already have written the real exit as the group died.
    let lookup = id.to_string();
    db.call(move |conn| {
        conn.execute(
            "UPDATE jobs SET status = 'failed', error = 'killed via mcp-ssh kill' \
             WHERE id = ?1 AND status = 'running'",
            [lookup],
        )
    })
    .await?;

    Ok(if killed {
        format!("killed {id}")
    } else {
        format!("signalled {id} (pgid {pgid}); the group may already be gone")
    })
}

// ---- sessions ----

/// Active/expired token counts for one table, plus the soonest upcoming expiry.
/// Token *values* are never read — only counts and expiry timestamps.
struct TokenStats {
    active: i64,
    expired: i64,
    next_expiry: Option<i64>,
}

/// `mcp-ssh sessions` — summarise the durable OAuth login state (access + refresh
/// tokens). Never prints token material, only counts and expiries.
pub async fn sessions() -> anyhow::Result<()> {
    let db = open_db()?;
    let now = now_unix();
    let access = token_stats(&db, "access_tokens", now).await?;
    let refresh = token_stats(&db, "refresh_tokens", now).await?;
    print!(
        "{}",
        render_sessions(
            &crate::config::db_path()?.display().to_string(),
            &access,
            &refresh,
            now
        )
    );
    Ok(())
}

async fn token_stats(db: &Db, table: &'static str, now: i64) -> anyhow::Result<TokenStats> {
    // `table` is a compile-time constant from the caller, never user input.
    let sql = format!(
        "SELECT COALESCE(SUM(expires_unix > ?1), 0), \
                COALESCE(SUM(expires_unix <= ?1), 0), \
                MIN(CASE WHEN expires_unix > ?1 THEN expires_unix END) \
         FROM {table}"
    );
    let stats = db
        .call(move |conn| {
            conn.query_row(&sql, [now], |r| {
                Ok(TokenStats {
                    active: r.get(0)?,
                    expired: r.get(1)?,
                    next_expiry: r.get(2)?,
                })
            })
        })
        .await?;
    Ok(stats)
}

fn render_sessions(db_path: &str, access: &TokenStats, refresh: &TokenStats, now: i64) -> String {
    let line = |label: &str, s: &TokenStats| {
        let next = match s.next_expiry {
            Some(exp) => format!(
                "; next expiry {} (in {})",
                fmt_time(exp),
                humanize_duration(exp - now)
            ),
            None => String::new(),
        };
        format!(
            "  {label}: {} active, {} expired{next}\n",
            s.active, s.expired
        )
    };
    let mut out = format!("OAuth sessions (durable tokens in {db_path}):\n");
    out.push_str(&line("access tokens ", access));
    out.push_str(&line("refresh tokens", refresh));
    out.push_str("(authorization codes are short-lived and held in memory — not shown)\n");
    out
}

// ---- shared formatting ----

/// A Unix timestamp as local `YYYY-MM-DD HH:MM:SS`; the raw number if it's out of range.
fn fmt_time(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| ts.to_string())
}

/// A positive duration in seconds, rendered compactly: `45s`, `12m`, `1h 12m`, `3d 4h`.
fn humanize_duration(secs: i64) -> String {
    if secs <= 0 {
        return "0s".into();
    }
    let (d, h, m, s) = (
        secs / 86_400,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60,
    );
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_duration_scales_units() {
        assert_eq!(humanize_duration(0), "0s");
        assert_eq!(humanize_duration(-5), "0s");
        assert_eq!(humanize_duration(45), "45s");
        assert_eq!(humanize_duration(12 * 60), "12m");
        assert_eq!(humanize_duration(3_600 + 12 * 60), "1h 12m");
        assert_eq!(humanize_duration(3 * 86_400 + 4 * 3_600), "3d 4h");
    }

    #[test]
    fn render_jobs_aligns_and_prefers_error_for_failures() {
        let rows = vec![
            JobRow {
                id: "minime-v1-07-16-59".into(),
                status: "running".into(),
                code: None,
                error: None,
                started_unix: 1_700_000_000,
                title: Some("minime-v1".into()),
            },
            JobRow {
                id: "job-01".into(),
                status: "failed".into(),
                code: None,
                error: Some("server restarted".into()),
                started_unix: 1_700_000_100,
                title: None,
            },
        ];
        let out = render_jobs(&rows);
        assert!(out.contains("ID"), "has a header");
        // Column width is driven by the longest id, so the short id is padded.
        assert!(
            out.contains("job-01             "),
            "short id padded to the widest id:\n{out}"
        );
        assert!(
            out.contains("server restarted"),
            "failure shows its error, not its (absent) title:\n{out}"
        );
        assert!(
            out.contains("minime-v1"),
            "running row shows its title:\n{out}"
        );
    }

    #[tokio::test]
    async fn fetch_jobs_filters_active_unless_all() {
        let db = Db::memory();
        db.call(|conn| {
            conn.execute_batch(
                "INSERT INTO jobs (id, status, started_unix) VALUES ('a', 'running', 10);\
                 INSERT INTO jobs (id, status, code, started_unix) VALUES ('b', 'exited', 0, 20);",
            )
        })
        .await
        .unwrap();

        let active = fetch_jobs(&db, false).await.unwrap();
        assert_eq!(active.len(), 1, "only the running job");
        assert_eq!(active[0].id, "a");

        let all = fetch_jobs(&db, true).await.unwrap();
        assert_eq!(all.len(), 2, "every job");
        // Newest first.
        assert_eq!(all[0].id, "b");
    }

    #[tokio::test]
    async fn kill_job_reports_unknown_and_finished_without_signalling() {
        let db = Db::memory();
        assert_eq!(kill_job(&db, "ghost").await.unwrap(), "no such job: ghost");

        db.call(|conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, code, started_unix) VALUES ('done', 'exited', 0, 1)",
                [],
            )
        })
        .await
        .unwrap();
        assert_eq!(
            kill_job(&db, "done").await.unwrap(),
            "job done is not running (status: exited)"
        );
    }

    #[tokio::test]
    async fn kill_job_without_pgid_cannot_signal() {
        let db = Db::memory();
        // A running row from before pgid tracking (pgid is NULL).
        db.call(|conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix) VALUES ('legacy', 'running', 1)",
                [],
            )
        })
        .await
        .unwrap();
        let msg = kill_job(&db, "legacy").await.unwrap();
        assert!(
            msg.contains("no recorded process group"),
            "must refuse to guess a pgid: {msg}"
        );
        // The row is untouched — we didn't fabricate a kill.
        let status: String = db
            .call(|conn| {
                conn.query_row("SELECT status FROM jobs WHERE id = 'legacy'", [], |r| {
                    r.get(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(status, "running");
    }

    #[tokio::test]
    async fn render_sessions_counts_without_leaking_tokens() {
        let db = Db::memory();
        let now = now_unix();
        let (active_exp, expired_exp) = (now + 3_600, now - 1);
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO access_tokens (token, expires_unix) VALUES ('AAA', ?1)",
                [active_exp],
            )?;
            conn.execute(
                "INSERT INTO access_tokens (token, expires_unix) VALUES ('BBB', ?1)",
                [expired_exp],
            )?;
            conn.execute(
                "INSERT INTO refresh_tokens (token, expires_unix) VALUES ('CCC', ?1)",
                [active_exp],
            )
        })
        .await
        .unwrap();

        let access = token_stats(&db, "access_tokens", now).await.unwrap();
        let refresh = token_stats(&db, "refresh_tokens", now).await.unwrap();
        assert_eq!((access.active, access.expired), (1, 1));
        assert_eq!((refresh.active, refresh.expired), (1, 0));

        let out = render_sessions("/tmp/x.db", &access, &refresh, now);
        assert!(out.contains("access tokens : 1 active, 1 expired"), "{out}");
        assert!(out.contains("refresh tokens: 1 active, 0 expired"), "{out}");
        // The whole point: token values never appear.
        for secret in ["AAA", "BBB", "CCC"] {
            assert!(
                !out.contains(secret),
                "token value leaked into output: {out}"
            );
        }
    }
}
