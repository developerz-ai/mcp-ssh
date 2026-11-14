//! Local admin subcommands: inspect and manage the server's durable state from a
//! shell on the host — `mcp-ssh jobs`, `mcp-ssh job kill <id>`, `mcp-ssh sessions`.
//!
//! They read/write the same SQLite database the running server uses (concurrent
//! WAL access is safe) and deliberately never construct a [`JobStore`]: doing so
//! would start a reaper and a startup reconcile that flips the live server's
//! `running` rows to `failed`. Here we only ever touch the database directly.
use crate::db::{Db, now_unix};
use crate::jobs::{JobId, JobRepo, JobRow, JobStatus, KillOutcome, kill_persisted};

/// Open the database the running server uses — only if it exists and carries this
/// binary's schema: no file is created and nothing is migrated, so a typo'd
/// `MCP_SSH_DB` or a `sudo` run before the server's first start changes nothing.
fn open_db() -> anyhow::Result<Db> {
    Ok(Db::open_existing(&crate::config::db_path()?)?)
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
            printable(&r.id),
            printable(&r.status),
            code,
            fmt_time(r.started_unix),
            printable(note),
        ));
    }
    out
}

/// A DB string made safe for the operator's terminal (often a root one): control
/// characters — escape sequences, a newline that would forge a row — become `?`.
/// New rows hold a normalized title already; rows written before that did not.
fn printable(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

// ---- job kill ----

/// A `mcp-ssh job kill` that did not kill anything. Returned as the command's
/// error so the process exits non-zero; its message is the operator sentence.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct KillNotDone(String);

/// `mcp-ssh job kill <id>` — signal a job's process group dead. Exits zero only
/// when something was killed (the running job, or what a finished one left
/// running); every other outcome is a [`KillNotDone`] error.
pub async fn kill(id: &str) -> anyhow::Result<()> {
    let (done, message) = kill_job(&open_repo()?, id).await?;
    if !done {
        return Err(KillNotDone(message).into());
    }
    println!("{message}");
    Ok(())
}

/// The CLI is pure rendering: the kill itself is [`kill_persisted`], the same code
/// path the server takes for a job it doesn't track, so both agree on what a kill
/// means. Returns whether something was killed, and the sentence saying what.
async fn kill_job(repo: &JobRepo, id: &str) -> anyhow::Result<(bool, String)> {
    let outcome = kill_persisted(repo, &JobId::from(id), "killed via mcp-ssh kill").await?;
    let id = printable(id);
    Ok(match outcome {
        KillOutcome::Killed => (true, format!("killed {id}")),
        KillOutcome::KilledLeftovers => (
            true,
            format!("job {id} had already finished; killed the processes it left running"),
        ),
        KillOutcome::Unknown => (false, format!("no such job: {id}")),
        KillOutcome::NotRunning { status } => (
            false,
            format!("job {id} is not running (status: {})", printable(&status)),
        ),
        KillOutcome::NoProcessGroup => (
            false,
            format!(
                "job {id} has no recorded process group (started before pgid tracking) — cannot kill from the CLI"
            ),
        ),
        KillOutcome::CorruptProcessGroup { pgid } => (
            false,
            format!("job {id} has a corrupt pgid ({pgid}) — refusing to signal"),
        ),
        KillOutcome::Survived { pgid } => (
            false,
            format!(
                "job {id} (pgid {}) outlived TERM then KILL — left running",
                pgid.get()
            ),
        ),
        KillOutcome::NotSignalled { error } => (
            false,
            format!("could not signal job {id}: {error} — it may still be running"),
        ),
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
///
/// `--revoke` ends every login instead (all access + refresh tokens), printing only
/// the count. Both open the database with `Db::open_existing` (no create, no
/// migration), so running this before the server's first start creates nothing.
pub async fn sessions(revoke: bool) -> anyhow::Result<()> {
    let path = crate::config::db_path()?;
    let db = Db::open_existing(&path)?;
    if revoke {
        let revoked = crate::oauth::revoke_all(&db).await?;
        println!("revoked {revoked} tokens: every client must log in again");
        return Ok(());
    }
    let now = now_unix();
    let access = token_stats(&db, "access_tokens", now).await?;
    let refresh = token_stats(&db, "refresh_tokens", now).await?;
    print!(
        "{}",
        render_sessions(&path.display().to_string(), &access, &refresh, now)
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
mod tests;
