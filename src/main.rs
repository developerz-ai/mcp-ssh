//! mcp-ssh — remote shell + file access for AI agents over authenticated MCP-HTTP.
mod app;
mod auth;
mod cli;
mod config;
mod jobs;
mod oauth;
mod tools;

use std::sync::Arc;

use clap::Parser;

use cli::{Cli, Command};

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
    let store = jobs::JobStore::new(cfg.job_dir.clone(), cfg.inline_timeout)?;

    let auth_state = oauth::AuthState {
        creds: auth::Credentials {
            user: cfg.user.clone(),
            pass: cfg.pass.clone(),
        },
        store: Arc::new(oauth::Store::default()),
        public_url: cfg.public_url.clone(),
    };

    let app = app::build(auth_state, store, cfg.allowed_hosts.clone());

    let listener = tokio::net::TcpListener::bind(cfg.bind).await?;
    tracing::info!(addr = %cfg.bind, "mcp-ssh listening on /mcp");
    axum::serve(listener, app).await?;
    Ok(())
}
