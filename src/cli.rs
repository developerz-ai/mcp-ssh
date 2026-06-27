//! Command-line surface: `serve` (run the server) and `set-auth` (configure creds).
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "mcp-ssh",
    version,
    about = "Remote shell + file access for AI agents over MCP-HTTP"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run the MCP server (default).
    Serve {
        /// Listen port (default 1337; overrides MCP_SSH_BIND / config).
        #[arg(long, short)]
        port: Option<u16>,
    },
    /// Set the HTTP username/password used for auth. Prompts for the password.
    SetAuth {
        /// Username.
        user: String,
    },
    /// List jobs (running only by default; `--all` includes finished ones).
    Jobs {
        /// Include finished/failed jobs, not just the running ones.
        #[arg(long)]
        all: bool,
    },
    /// Manage a background job.
    Job {
        #[command(subcommand)]
        action: JobCommand,
    },
    /// Summarise OAuth sessions (durable access/refresh tokens).
    Sessions,
}

#[derive(Subcommand)]
pub enum JobCommand {
    /// Kill a running job by id (SIGTERM, then SIGKILL after a short grace).
    Kill {
        /// Job id, as shown by `mcp-ssh jobs`.
        id: String,
    },
}
