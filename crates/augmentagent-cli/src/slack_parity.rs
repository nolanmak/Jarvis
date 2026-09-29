//! #1300 — `augmentagent slack parity`: the Slack parity matrix for the owner
//! and for CI.
//!
//! - `report [--json]` checks `docs/slack-parity-matrix.json` against the
//!   source tree and the Slack capability tables, then prints every row with
//!   its status and host evidence, and the open blockers (blocked rows,
//!   rows that need a real workspace or host, host blockers, missing
//!   real-host acceptance, shared-suite gaps). Exits 1 when the check fails.
//! - `commands` prints one `cargo test` line per test target the matrix
//!   names, plus the Discord and WhatsApp regression suites; the platform
//!   workflow runs them on Linux and macOS.
//!
//! Read-only: nothing touches Slack, the database or credentials. The
//! matrix is read at run time (from `./docs/` when run in a checkout,
//! otherwise from the checkout this binary was built from).

use std::path::{Path, PathBuf};

use anyhow::Result;
use augmentagent_channel_slack::parity::{
    cargo_commands, check, load, render_text, report, source_root_for, CheckContext, MATRIX_PATH,
};
use clap::{Args, Subcommand};

#[derive(Subcommand, Debug, Clone)]
pub enum SlackParityOp {
    /// Check the parity matrix and print every row, its status and the
    /// open blockers. Exits 1 when the matrix fails its check.
    Report(ReportArgs),
    /// Print the `cargo test` commands that run every named parity test and
    /// the Discord and WhatsApp regression suites.
    Commands(MatrixArgs),
}

#[derive(Args, Debug, Clone)]
pub struct MatrixArgs {
    /// Matrix file (default: docs/slack-parity-matrix.json in the checkout).
    #[arg(long)]
    pub matrix: Option<PathBuf>,
    /// Repository root the named tests are looked up in (default: the
    /// directory above the matrix's `docs/`).
    #[arg(long)]
    pub root: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct ReportArgs {
    #[command(flatten)]
    pub matrix: MatrixArgs,
    /// Print JSON instead of the table.
    #[arg(long)]
    pub json: bool,
}

fn default_matrix() -> PathBuf {
    let local = Path::new(MATRIX_PATH);
    if local.is_file() {
        return local.to_path_buf();
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(MATRIX_PATH)
}

/// Exit code: 0, or 1 when `report` found violations.
pub fn run(op: &SlackParityOp) -> Result<i32> {
    match op {
        SlackParityOp::Report(args) => {
            let path = args.matrix.matrix.clone().unwrap_or_else(default_matrix);
            let matrix = load(&path)?;
            let root = args
                .matrix
                .root
                .clone()
                .unwrap_or_else(|| source_root_for(&path));
            let violations = check(&matrix, &CheckContext::slack(&root));
            let r = report(&matrix, &violations);
            if args.json {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                print!("{}", render_text(&r));
            }
            Ok(if r.check.ok { 0 } else { 1 })
        }
        SlackParityOp::Commands(args) => {
            let path = args.matrix.clone().unwrap_or_else(default_matrix);
            for line in cargo_commands(&load(&path)?) {
                println!("{line}");
            }
            Ok(0)
        }
    }
}
