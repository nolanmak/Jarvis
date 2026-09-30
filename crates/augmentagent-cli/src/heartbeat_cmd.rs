//! `augmentagent heartbeat run-once|status` (#1317).
//!
//! `run-once` drives the same gates as the daemon's loop, so it is both the
//! manual "wake now" and the way to try a checklist before enabling the
//! loop (`--dry-run --force`). `status --check` is the liveness probe: it
//! exits 1 when an enabled heartbeat has gone quiet, for cron or an
//! external healthcheck to alert on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use augmentagent_heartbeat::store_ext::{HeartbeatRun, HeartbeatStore};
use augmentagent_heartbeat::{checklist, HeartbeatConfig, HeartbeatRunner, RunOptions, RunReport};
use augmentagent_store::Store;
use clap::Subcommand;
use serde::Serialize;

#[derive(Debug, Clone, Subcommand)]
pub enum HeartbeatOp {
    /// Run one heartbeat now through the normal gates.
    RunOnce {
        /// Ignore the interval and active hours. The checklist, notice cap,
        /// duplicate check and lease still apply.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Ask the model but deliver nothing and record nothing.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Show configuration, the last run and recent runs.
    Status {
        #[arg(long, default_value_t = false)]
        json: bool,
        /// Exit 1 when the heartbeat is enabled but has not run for three
        /// intervals inside active hours.
        #[arg(long, default_value_t = false)]
        check: bool,
    },
}

/// Why `serve` does or doesn't start the heartbeat loop.
pub fn serve_decision(
    config: &HeartbeatConfig,
    wiki_dir: Option<&Path>,
    dry_run: bool,
) -> Result<PathBuf, &'static str> {
    if !config.enabled {
        return Err("AUGMENTAGENT_HEARTBEAT_ENABLED is not set");
    }
    let wiki = wiki_dir.ok_or("--wiki-dir is not set")?;
    if dry_run {
        return Err("dry run");
    }
    Ok(wiki.to_path_buf())
}

#[derive(Serialize)]
struct ChecklistView {
    path: Option<PathBuf>,
    present: bool,
}

#[derive(Serialize)]
struct StatusView {
    enabled: bool,
    interval_secs: u64,
    active_hours: Option<String>,
    tz: Option<String>,
    daily_cap: u32,
    timeout_secs: u64,
    warnings: Vec<String>,
    checklist: ChecklistView,
    stale: bool,
    last_run: Option<HeartbeatRun>,
    runs: Vec<HeartbeatRun>,
}

pub async fn run(cli: &crate::Cli, store: Arc<Store>, op: &HeartbeatOp) -> Result<()> {
    let config = HeartbeatConfig::from_env();
    match *op {
        HeartbeatOp::RunOnce {
            force,
            dry_run,
            json,
        } => {
            let wiki_root = cli
                .wiki_dir
                .clone()
                .context("heartbeat run-once needs --wiki-dir")?;
            let (broker, _) = crate::build_broker(cli, Arc::clone(&store), dry_run).await?;
            let runner =
                HeartbeatRunner::new(store, broker, crate::build_reasoner(), wiki_root, config);
            let report = runner.run_once(RunOptions { force, dry_run }).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", describe_report(&report));
            }
            Ok(())
        }
        HeartbeatOp::Status { json, check } => {
            let view = status_view(&store, &config, cli.wiki_dir.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&view)?);
            } else {
                print_status(&view);
            }
            if check && view.stale {
                if !json {
                    eprintln!("heartbeat is stale: no run in the last three intervals");
                }
                std::process::exit(1);
            }
            Ok(())
        }
    }
}

fn status_view(
    store: &Store,
    config: &HeartbeatConfig,
    wiki_dir: Option<&Path>,
) -> Result<StatusView> {
    let runs = store.recent_runs(10)?;
    let now = chrono::Utc::now();
    Ok(StatusView {
        enabled: config.enabled,
        interval_secs: config.interval.as_secs(),
        active_hours: config.active_hours.map(|w| w.to_string()),
        tz: config.tz.map(|tz| tz.name().to_string()),
        daily_cap: config.daily_cap,
        timeout_secs: config.timeout.as_secs(),
        warnings: config.warnings.clone(),
        checklist: ChecklistView {
            path: wiki_dir.map(|w| w.join(checklist::FILE_NAME)),
            present: wiki_dir.is_some_and(|w| checklist::load(w).is_some()),
        },
        stale: config.is_stale(store.last_attempt_ms()?, now),
        last_run: runs.first().cloned(),
        runs,
    })
}

fn local_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| ms.to_string())
}

fn describe_run(run: &HeartbeatRun) -> String {
    let mut line = format!("{}  {}", local_time(run.started_at_ms), run.status);
    if let Some(reason) = &run.reason {
        line.push_str(&format!(" ({reason})"));
    }
    if let Some(message) = &run.message {
        let short: String = message.chars().take(80).collect();
        line.push_str(&format!("  {short}"));
    }
    line
}

fn describe_report(report: &RunReport) -> String {
    let mut line = format!("heartbeat: {}", report.status);
    if let Some(reason) = &report.reason {
        line.push_str(&format!(" ({reason})"));
    }
    if let Some(decision) = &report.decision {
        line.push_str(&format!(" · decision {decision}"));
    }
    if let Some(message) = &report.message {
        line.push_str(&format!("\n{message}"));
    }
    line
}

fn print_status(view: &StatusView) {
    let window = match (&view.active_hours, &view.tz) {
        (Some(w), Some(tz)) => format!("active {w} ({tz})"),
        (Some(w), None) => format!("active {w} (host time)"),
        (None, _) => "active all day".into(),
    };
    println!(
        "heartbeat: {} · every {}m · {window} · cap {}/day · timeout {}s",
        if view.enabled { "enabled" } else { "disabled" },
        view.interval_secs / 60,
        view.daily_cap,
        view.timeout_secs,
    );
    match &view.checklist.path {
        Some(path) => println!(
            "checklist: {} ({})",
            path.display(),
            if view.checklist.present {
                "present"
            } else {
                "missing or empty: runs are skipped"
            }
        ),
        None => println!("checklist: unknown (pass --wiki-dir)"),
    }
    for warning in &view.warnings {
        println!("warning: {warning}");
    }
    if view.stale {
        println!("STALE: no run in the last three intervals");
    }
    if view.runs.is_empty() {
        println!("no runs yet");
        return;
    }
    println!("recent runs:");
    for run in &view.runs {
        println!("  {}", describe_run(run));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled() -> HeartbeatConfig {
        HeartbeatConfig {
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn serve_starts_the_loop_only_when_enabled_with_a_wiki_and_live() {
        let wiki = Path::new("/srv/wiki");
        assert_eq!(
            serve_decision(&enabled(), Some(wiki), false),
            Ok(wiki.to_path_buf())
        );
        assert_eq!(
            serve_decision(&HeartbeatConfig::default(), Some(wiki), false),
            Err("AUGMENTAGENT_HEARTBEAT_ENABLED is not set")
        );
        assert_eq!(
            serve_decision(&enabled(), None, false),
            Err("--wiki-dir is not set")
        );
        assert_eq!(serve_decision(&enabled(), Some(wiki), true), Err("dry run"));
    }

    #[test]
    fn subcommands_parse() {
        use clap::Parser;
        let cli = crate::Cli::try_parse_from([
            "augmentagent",
            "heartbeat",
            "run-once",
            "--force",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            crate::Cmd::Heartbeat {
                op: HeartbeatOp::RunOnce {
                    force: true,
                    dry_run: true,
                    json: true
                }
            }
        ));
        let cli =
            crate::Cli::try_parse_from(["augmentagent", "heartbeat", "status", "--check"]).unwrap();
        assert!(matches!(
            cli.cmd,
            crate::Cmd::Heartbeat {
                op: HeartbeatOp::Status {
                    json: false,
                    check: true
                }
            }
        ));
    }
}
