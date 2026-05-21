//! `augmentagent setup …` — operator-onboarding helpers.
//!
//! Issue #8 wires the first op: `setup harvest <channel>` — a schema emitter
//! for cookie-based channels (Discord, Twitter, LinkedIn, Instagram) so the
//! `/setup` skill can drive credential capture via `AskUserQuestion` instead
//! of shelling out to an interactive `read` loop.
//!
//! Future variants land here (alphabetically inside `SetupOp`):
//!   - Oauth   (#10 — Gmail/Calendar/Slack OAuth orchestrator)
//!
//! Each op stays small enough to live in its own sibling module
//! (`mod harvest;`, `mod oauth;`, …) so this file is just the dispatcher.

use anyhow::Result;
use clap::Subcommand;

pub mod harvest;

/// Verbs under `augmentagent setup …`. Alphabetical; future ops slot in
/// alphabetically (`Doctor` is *not* here — it's a top-level subcommand
/// under `setup+maintenance` in `main.rs`).
#[derive(Subcommand, Debug, Clone)]
pub enum SetupOp {
    /// Issue #8 — print the cookie-harvest field schema for a channel as JSON
    /// (so the `/setup` skill can drive credential capture via
    /// `AskUserQuestion`), or shell through to the existing
    /// `scripts/<channel>-harvest.sh` for power users.
    Harvest(harvest::HarvestArgs),
}

/// Dispatch entrypoint called from `main.rs`.
pub async fn run_setup(op: &SetupOp) -> Result<()> {
    match op {
        SetupOp::Harvest(args) => harvest::run_harvest(args).await,
    }
}
