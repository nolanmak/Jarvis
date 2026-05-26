//! `augmentagent-mcp-memory` — stdio MCP server binary entrypoint.
//!
//! Spawned by Claude Code via the `mcp.json` config wired in by issue #110
//! (see `augmentagent-channel-core::mcp`). The binary takes one positional
//! arg or env var:
//!
//! - `AUGMENTAGENT_DB` (env) — absolute path to the daemon's sqlite db.
//!   Falls back to `data.db` in the cwd if unset, which matches the
//!   convention used everywhere else in the workspace.
//!
//! All logging goes to stderr (stdout is the JSON-RPC channel — anything
//! we print there poisons the protocol).

use std::env;

use anyhow::Context;
use augmentagent_mcp_memory::{serve_stdio, Server};
use tracing_subscriber::EnvFilter;

fn main() -> anyhow::Result<()> {
    // tracing → stderr so it doesn't collide with the JSON-RPC channel
    // on stdout. `RUST_LOG` is the standard knob; default INFO to keep
    // boot info visible without flooding the log.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .init();

    let db_path = env::var("AUGMENTAGENT_DB").unwrap_or_else(|_| "data.db".to_string());
    tracing::info!(db = %db_path, "starting augmentagent-mcp-memory");
    let server = Server::open(&db_path)
        .with_context(|| format!("open server backed by {db_path}"))?;
    serve_stdio(server)
}
