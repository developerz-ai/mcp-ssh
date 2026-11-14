//! End-to-end: the real `mcp-ssh serve` binary, booted per test on an ephemeral
//! port with tempdir state, driven over HTTP exactly as an MCP client drives it.
//! One scenario per top audit finding, each asserting the *fixed* behaviour.
//! Run: `bin/e2e` (or `cargo test --test e2e`).
mod client;
mod harness;
mod oauth;

mod auth;
mod files;
mod jobs;
mod lifecycle;
