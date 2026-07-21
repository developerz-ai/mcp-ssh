//! Local admin subcommands: inspect and manage the server's durable state from a
//! shell on the host — `mcp-ssh jobs`, `mcp-ssh job kill <id>`, `mcp-ssh sessions`.
//!
//! They read/write the same SQLite database the running server uses (concurrent
//! WAL access is safe) and deliberately never construct a [`JobStore`]: doing so
//! would start a reaper and a startup reconcile that flips the live server's
//! `running` rows to `failed`. Here we only ever touch the database directly.
use crate::db::{Db, now_unix};
use crate::jobs::{JobId, JobRepo, JobRow, JobStatus};

/// Open the database the running server uses. Idempotent — applies the same schema
/// and pragmas, safe to run alongside the daemon (SQLite WAL allows it).
fn open_db() -> anyhow::Result<Db> {
    Db::open(&crate::config::db_path()?)
}

/// The `jobs` table, through the same typed repository the server uses — these
/// commands never write their own SQL.
fn open_repo() -> anyhow::Result<JobRepo> {
    Ok(JobRepo::new(open_db()?))
}

// ---- jobs ----

/// `mcp-ssh jobs [--all]` — list running jobs (or every job, most-recent first).
pub async fn jobs(all: bool) -> anyhow::Result<()> {
    let rows = open_repo()?.listing(all).await?;
    if rows.is_empty() {
        println!("{}", if all { "no jobs" } else { "no active jobs" });
    } else {
        print!("{}", render_jobs(&rows));
    }
    Ok(())
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
        let note = match (r.status.parse::<JobStatus>(), r.error.as_deref()) {
            (Ok(JobStatus::Failed), Some(e)) => e,
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
    println!("{}", kill_job(&open_repo()?, id).await?);
    Ok(())
}

async fn kill_job(repo: &JobRepo, id: &str) -> anyhow::Result<String> {
    let job_id = JobId::from(id);
    let Some(target) = repo.kill_target(&job_id).await? else {
        return Ok(format!("no such job: {id}"));
    };
    if target.status != JobStatus::Running.as_str() {
        return Ok(format!(
            "job {id} is not running (status: {})",
            target.status
        ));
    }
    let Some(pgid) = target.pgid else {
        return Ok(format!(
            "job {id} has no recorded process group (started before pgid tracking) — cannot kill from the CLI"
        ));
    };

    // Corrupt rows are refused, not signalled: outside `u32` a raw `as` cast would
    // wrap onto a real (wrong) group, and `0` is worse — `kill -- -0` signals *this*
    // process's own group. `ProcessGroupId` is the single gate for both.
    let Some(group) = crate::jobs::ProcessGroupId::from_persisted(pgid) else {
        return Ok(format!(
            "job {id} has a corrupt pgid ({pgid}) — refusing to signal"
        ));
    };
    let killed = crate::jobs::kill_group(group).await;
    // Record the kill only if the row is still `running`: when the server owns the
    // job, its waiter may already have written the real exit as the group died.
    repo.mark_killed(&job_id, "killed via mcp-ssh kill").await?;

    Ok(if killed {
        format!("killed {id}")
    } else {
        format!(
            "signalled {id} (pgid {}); the group may already be gone",
            group.get()
        )
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

    /// A repository over a fresh in-memory DB, plus the raw handle the tests use to
    /// plant rows the typed API can't produce (legacy or corrupt ones) and to read
    /// columns back.
    fn repo() -> (JobRepo, Db) {
        let db = Db::memory();
        (JobRepo::new(db.clone()), db)
    }

    #[tokio::test]
    async fn kill_job_reports_unknown_and_finished_without_signalling() {
        let (repo, db) = repo();
        assert_eq!(
            kill_job(&repo, "ghost").await.unwrap(),
            "no such job: ghost"
        );

        db.call(|conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, code, started_unix) VALUES ('done', 'exited', 0, 1)",
                [],
            )
        })
        .await
        .unwrap();
        assert_eq!(
            kill_job(&repo, "done").await.unwrap(),
            "job done is not running (status: exited)"
        );
    }

    #[tokio::test]
    async fn kill_job_without_pgid_cannot_signal() {
        let (repo, db) = repo();
        // A running row from before pgid tracking (pgid is NULL).
        db.call(|conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, started_unix) VALUES ('legacy', 'running', 1)",
                [],
            )
        })
        .await
        .unwrap();
        let msg = kill_job(&repo, "legacy").await.unwrap();
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

    /// True once `pid` is no longer signalable, polled up to `deadline`. A killed
    /// process stays signalable until its parent reaps it, so a single probe right
    /// after the kill would be a race; a bounded poll is the deterministic form.
    #[cfg(unix)]
    async fn gone_within(pid: u32, deadline: std::time::Duration) -> bool {
        let start = tokio::time::Instant::now();
        loop {
            let alive = tokio::process::Command::new("kill")
                .args(["-0", "--"])
                .arg(pid.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if !alive {
                return true;
            }
            if start.elapsed() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn kill_job_signals_a_real_process_group_and_updates_row() {
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("descendant.pid");
        // Leader of its own group (pgid == pid) with a background descendant in that
        // same group — the shape of a real job's shell. The descendant is what makes
        // this a *group* test: a pid-only signal would leave it running.
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "sleep 300 & echo $! > '{}'; wait",
                pidfile.display()
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i64::from(child.id().expect("child pid"));
        // Reap in the background so the signalled child leaves no zombie — mirrors
        // the real parent (server/init) reaping it.
        let waiter = tokio::spawn(async move { child.wait().await });
        let descendant = read_pid(&pidfile).await;

        let (repo, db) = repo();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO jobs (id, status, pgid, started_unix) VALUES ('real', 'running', ?1, 1)",
                [pid],
            )
        })
        .await
        .unwrap();

        let msg = kill_job(&repo, "real").await.unwrap();
        assert_eq!(msg, "killed real", "the whole group must be gone");
        let exit = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
            .await
            .expect("group leader should exit after kill_job")
            .expect("wait task should not panic")
            .expect("waiting on the leader should succeed");
        assert!(
            !exit.success(),
            "leader exits from the signal, not normally"
        );
        assert!(
            gone_within(descendant, std::time::Duration::from_secs(5)).await,
            "descendant {descendant} outlived the group kill"
        );

        let status: String = db
            .call(|conn| {
                conn.query_row("SELECT status FROM jobs WHERE id = 'real'", [], |r| {
                    r.get(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(
            status, "failed",
            "row transitions out of running once the group is signalled"
        );
    }

    /// The pid the shell wrote for its background child, once the write lands.
    #[cfg(unix)]
    async fn read_pid(path: &std::path::Path) -> u32 {
        for _ in 0..100 {
            if let Ok(text) = tokio::fs::read_to_string(path).await
                && let Ok(pid) = text.trim().parse()
            {
                return pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("shell never recorded its background child's pid");
    }

    #[tokio::test]
    async fn kill_job_rejects_a_corrupt_pgid_without_signalling() {
        let (repo, db) = repo();
        // Neither value can come from a real job's process group: negative is outside
        // u32, and `-0` is *this* process's own group. Both simulate a corrupt row
        // (hand-edited DB, or a future bug writing garbage into the column).
        for (id, pgid) in [("negative", -1), ("zero", 0)] {
            db.call(move |conn| {
                conn.execute(
                    "INSERT INTO jobs (id, status, pgid, started_unix) VALUES (?1, 'running', ?2, 1)",
                    rusqlite::params![id, pgid],
                )
            })
            .await
            .unwrap();

            let msg = kill_job(&repo, id).await.unwrap();
            assert!(
                msg.contains("corrupt pgid"),
                "must refuse to signal pgid {pgid}: {msg}"
            );
            let lookup = id.to_string();
            let status: String = db
                .call(move |conn| {
                    conn.query_row("SELECT status FROM jobs WHERE id = ?1", [lookup], |r| {
                        r.get(0)
                    })
                })
                .await
                .unwrap();
            assert_eq!(status, "running", "row untouched — no signal was sent");
        }
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
