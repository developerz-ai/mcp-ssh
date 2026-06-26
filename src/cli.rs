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
    Serve,
    /// Set the HTTP username/password used for auth. Prompts for the password.
    SetAuth {
        /// Username.
        user: String,
    },
}
