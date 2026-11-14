//! mcp-ssh — remote shell + file access for AI agents over authenticated MCP-HTTP.
mod admin;
mod agent;
mod app;
mod auth;
mod cli;
mod config;
mod db;
mod jobs;
#[cfg(test)]
mod log_capture_tests;
mod oauth;
mod sentry;
mod tools;

use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio_util::sync::CancellationToken;

use cli::{Cli, Command, JobCommand};

fn main() -> anyhow::Result<()> {
    // Sentry MUST init before the tokio runtime: its reqwest transport spins up its
    // own runtime, and the guard has to live for the whole program so in-flight
    // events flush on exit. `let _sentry_guard` keeps the binding alive (a bare
    // `let _ =` would drop it immediately and disable capture). Unset/malformed
    // DSN → a disabled no-op client; the app runs regardless.
    let _sentry_guard = sentry::init();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mcp_ssh=debug".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // Capture run() failures (config/load/bind/serve) to Sentry before they leave
    // main — otherwise they propagate as an exit code and never reach error
    // tracking. The guard is still bound; this just reports on the way out.
    let result = runtime.block_on(run());
    if let Err(ref error) = result {
        let source: &dyn std::error::Error = error.as_ref();
        sentry::capture_error(source);
    }
    result
}

async fn run() -> anyhow::Result<()> {
    match Cli::parse()
        .command
        .unwrap_or(Command::Serve { port: None })
    {
        Command::Serve { port } => serve(port).await,
        Command::SetAuth { user } => set_auth(user).await,
        Command::Jobs { all } => admin::jobs(all).await,
        Command::Job {
            action: JobCommand::Kill { id },
        } => match admin::kill(&id).await {
            // Nothing was killed: an operator outcome, not a fault — exit 1 with
            // the sentence, and keep it out of error tracking.
            Err(error) if error.is::<admin::KillNotDone>() => {
                eprintln!("{error}");
                std::process::exit(1);
            }
            other => other,
        },
        Command::Sessions { revoke } => admin::sessions(revoke).await,
    }
}

/// How long in-flight requests get to finish after a shutdown signal before the
/// server stops waiting. The systemd unit's `TimeoutStopSec` sits above this.
const DRAIN_DEADLINE: Duration = Duration::from_secs(10);

/// `mcp-ssh set-auth <user>` — prompt for a password, write it to the config, and
/// end every existing login: a credential change must lock out whoever held the
/// old one, not just future logins.
async fn set_auth(user: String) -> anyhow::Result<()> {
    let pass = config::Password::new(rpassword::prompt_password("Password: ")?);
    let path = config::set_auth(&user, &pass)?;
    println!("Saved credentials for {user} to {}", path.display());
    match db::Db::open_existing(&config::db_path()?) {
        Ok(database) => {
            let revoked = oauth::revoke_all(&database).await?;
            println!("Revoked {revoked} tokens: every client must log in again.");
        }
        Err(db::DbError::Missing(_)) => println!("No session database yet: nothing to revoke."),
        Err(e) => anyhow::bail!(
            "credentials saved, but existing sessions were NOT revoked ({e}); \
             run `mcp-ssh sessions --revoke` once the server is up"
        ),
    }
    println!("Restart the service to apply the new credentials.");
    Ok(())
}

async fn serve(port: Option<u16>) -> anyhow::Result<()> {
    let mut cfg = config::Config::load()?;
    // `--port` (or MCP_SSH_PORT) overrides just the port of the bind address.
    if let Some(p) = match port {
        Some(p) => Some(p),
        None => env_port(std::env::var("MCP_SSH_PORT").ok())?,
    } {
        cfg.bind.set_port(p);
    }
    // The single SQLite database (OAuth tokens + job metadata) under the systemd
    // StateDirectory. Any directory created for it is owner-only; an existing one
    // keeps its mode (systemd's `StateDirectoryMode` sets the default one's).
    if let Some(parent) = cfg.db_path.parent() {
        create_private_dir(parent)?;
    }
    let database = db::Db::open(&cfg.db_path)?;

    let store = jobs::JobStore::new(
        cfg.job_dir.clone(),
        cfg.inline_timeout,
        jobs::Shell::interactive_bash(),
        database.clone(),
    )?;

    let auth_state = oauth::AuthState {
        creds: auth::Credentials {
            user: cfg.user.clone(),
            pass: cfg.pass.clone(),
        },
        store: Arc::new(oauth::Store::new(database)),
        public_url: cfg.public_url.clone(),
        revalidate_every: auth::STREAM_REVALIDATE,
    };

    let shutdown = CancellationToken::new();
    let app = app::build(
        auth_state,
        store,
        cfg.allowed_hosts.clone(),
        shutdown.clone(),
    );

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "mcp-ssh listening on /mcp");
    let signalled = shutdown.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        shutdown_signal().await;
        // Ends every MCP session's SSE stream, so the graceful drain isn't held
        // open by connected clients.
        signalled.cancel();
    });
    serve_until_drained(server.into_future(), shutdown, DRAIN_DEADLINE).await?;
    Ok(())
}

/// Run `server` to completion — but once `shutdown` fires, give it only
/// `deadline` to drain. A connection that outlives that (a client holding a
/// stream open) is dropped with the server rather than stalling the stop until
/// the service manager kills the process.
async fn serve_until_drained<F>(
    server: F,
    shutdown: CancellationToken,
    deadline: Duration,
) -> std::io::Result<()>
where
    F: Future<Output = std::io::Result<()>>,
{
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => return result,
        () = shutdown.cancelled() => {}
    }
    match tokio::time::timeout(deadline, server).await {
        Ok(result) => {
            tracing::info!("drained");
            result
        }
        Err(_) => {
            tracing::warn!(
                ?deadline,
                "drain deadline hit; dropping remaining connections"
            );
            Ok(())
        }
    }
}

/// `create_dir_all`, but every directory it creates is 0700.
fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// Parse the `MCP_SSH_PORT` value. Unset/empty → no override; anything else must
/// be a valid port. Silently ignoring `8080x` or `70000` (the old `.ok()` chain)
/// left the server listening on the config/default port while the operator
/// believed they'd moved it — fail fast instead, like an invalid `MCP_SSH_BIND`.
fn env_port(raw: Option<String>) -> anyhow::Result<Option<u16>> {
    match raw.as_deref() {
        None | Some("") => Ok(None),
        Some(v) => v
            .parse::<u16>()
            .map(Some)
            .map_err(|e| anyhow::anyhow!("invalid MCP_SSH_PORT {v:?}: {e}")),
    }
}

/// Resolves on Ctrl-C or (on Unix) SIGTERM, letting axum drain in-flight
/// requests before exit. systemd sends SIGTERM on `stop`/`restart`.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(%err, "failed to listen for Ctrl-C");
            // Don't resolve: a broken handler must not trigger a spurious shutdown.
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!(%err, "failed to install SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }

    tracing::info!("shutdown signal received, draining connections");
}

#[cfg(all(test, unix))]
#[path = "main_tests.rs"]
mod tests;
