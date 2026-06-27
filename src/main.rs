//! mcp-ssh — remote shell + file access for AI agents over authenticated MCP-HTTP.
mod admin;
mod app;
mod auth;
mod cli;
mod config;
mod db;
mod jobs;
mod oauth;
mod tools;

use std::sync::Arc;

use clap::Parser;

use cli::{Cli, Command, JobCommand};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,mcp_ssh=debug".into()),
        )
        .init();

    match Cli::parse()
        .command
        .unwrap_or(Command::Serve { port: None })
    {
        Command::Serve { port } => serve(port).await,
        Command::SetAuth { user } => set_auth(user),
        Command::Jobs { all } => admin::jobs(all).await,
        Command::Job {
            action: JobCommand::Kill { id },
        } => admin::kill(&id).await,
        Command::Sessions => admin::sessions().await,
    }
}

/// `mcp-ssh set-auth <user>` — prompt for a password and write it to the config.
fn set_auth(user: String) -> anyhow::Result<()> {
    let pass = rpassword::prompt_password("Password: ")?;
    if pass.is_empty() {
        anyhow::bail!("password must not be empty");
    }
    let path = config::set_auth(&user, &pass)?;
    println!("Saved credentials for {user} to {}", path.display());
    Ok(())
}

async fn serve(port: Option<u16>) -> anyhow::Result<()> {
    let mut cfg = config::Config::load()?;
    // `--port` (or MCP_SSH_PORT) overrides just the port of the bind address.
    if let Some(p) = port.or_else(|| {
        std::env::var("MCP_SSH_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
    }) {
        cfg.bind.set_port(p);
    }
    // The single SQLite database (OAuth tokens + job metadata) under the systemd
    // StateDirectory. Ensure its parent dir exists before opening it.
    if let Some(parent) = cfg.db_path.parent() {
        std::fs::create_dir_all(parent)?;
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
    };

    let app = app::build(auth_state, store, cfg.allowed_hosts.clone());

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "mcp-ssh listening on /mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
mod tests {
    use std::time::Duration;

    use tokio::signal::unix::{SignalKind, signal};
    use tokio::time::timeout;

    use super::*;

    // SIGTERM (what systemd sends on stop) must resolve `shutdown_signal` so
    // axum begins draining instead of the process being killed outright.
    #[tokio::test]
    async fn sigterm_resolves_shutdown_signal() {
        // Install the global SIGTERM handler up front: once tokio owns the
        // signal, raising it can never fall through to the default disposition
        // (which would terminate the whole test process).
        let _guard = signal(SignalKind::terminate()).expect("install SIGTERM guard");

        let shutdown = tokio::spawn(shutdown_signal());
        // Give the spawned task a chance to register its own SIGTERM stream.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let status = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -TERM failed");

        timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("shutdown_signal did not resolve within 5s")
            .expect("shutdown task panicked");
    }
}
