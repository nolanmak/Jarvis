//! `augmentagent heartbeat run-once|status|items|done|snooze` (#1317, #2285).
//!
//! `run-once` drives the same gates as the daemon's daily sweep, so it is
//! both the manual "sweep now" and the way to try a checklist before
//! enabling the loop (`--dry-run --force`). `status --check` is the
//! liveness probe: it exits 1 when an enabled heartbeat has gone quiet, for
//! cron or an external healthcheck to alert on. `done` and `snooze` take the
//! short ref printed on a notice item.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use augmentagent_heartbeat::store_ext::{item_state, HeartbeatItem, HeartbeatRun, HeartbeatStore};
use augmentagent_heartbeat::{checklist, HeartbeatConfig, HeartbeatRunner, RunOptions, RunReport};
use augmentagent_store::Store;
use clap::Subcommand;
use serde::Serialize;

#[derive(Debug, Clone, Subcommand)]
pub enum HeartbeatOp {
    /// Run one sweep now through the normal gates.
    RunOnce {
        /// Ignore the daily schedule and active hours. The checklist, notice
        /// cap, item dedup and lease still apply.
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
        /// Exit 1 when the heartbeat is enabled but has not run for over a
        /// day inside active hours.
        #[arg(long, default_value_t = false)]
        check: bool,
    },
    /// List items the sweep surfaced, and their state.
    Items {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Mark a surfaced item done (by the ref on the notice, or its key).
    Done { id: String },
    /// Hide a surfaced item, then show it again after `for` (`3d`, `12h`).
    Snooze {
        id: String,
        #[arg(long = "for", default_value = "3d")]
        duration: String,
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
    sweep_at: String,
    max_items: usize,
    active_hours: Option<String>,
    tz: Option<String>,
    daily_cap: u32,
    timeout_secs: u64,
    warnings: Vec<String>,
    checklist: ChecklistView,
    stale: bool,
    last_run: Option<HeartbeatRun>,
    runs: Vec<HeartbeatRun>,
    items: Vec<HeartbeatItem>,
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
            // #1833 D16 — outreach only with both switches on; a dry run
            // shows the proposals and the gate's decision for each, with no
            // side effects.
            let outreach = crate::autonomy_cmd::heartbeat_outreach(cli, &store, dry_run);
            let mut runner =
                HeartbeatRunner::new(store, broker, crate::build_reasoner(), wiki_root, config);
            if let Some(outreach) = outreach {
                runner = runner.with_outreach(outreach);
            }
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
                    eprintln!("heartbeat is stale: no sweep attempted in over a day");
                }
                std::process::exit(1);
            }
            Ok(())
        }
        HeartbeatOp::Items { limit, json } => {
            let items = store.recent_items(limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if items.is_empty() {
                println!("no surfaced items yet");
            } else {
                for item in &items {
                    println!("{}", describe_item(item));
                }
            }
            Ok(())
        }
        HeartbeatOp::Done { ref id } => {
            let item = one_item(&store, id)?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            store.set_item_state(&item.item_key, item_state::DONE, None, now_ms)?;
            println!(
                "done: {}",
                item.summary.as_deref().unwrap_or(&item.item_key)
            );
            Ok(())
        }
        HeartbeatOp::Snooze {
            ref id,
            ref duration,
        } => {
            let secs = augmentagent_approval_discord::parse_interval(duration)
                .filter(|s| *s > 0)
                .with_context(|| format!("`{duration}` is not a duration like 3d or 12h"))?;
            let item = one_item(&store, id)?;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let until = now_ms + secs * 1000;
            store.set_item_state(&item.item_key, item_state::SNOOZED, Some(until), now_ms)?;
            println!(
                "snoozed until {}: {}",
                local_time(until),
                item.summary.as_deref().unwrap_or(&item.item_key)
            );
            Ok(())
        }
    }
}

/// The one item `id` (a ref or a key) names.
fn one_item(store: &Store, id: &str) -> Result<HeartbeatItem> {
    let mut found = store.find_items(id.trim())?;
    match found.len() {
        0 => anyhow::bail!("no heartbeat item `{id}`; see `augmentagent heartbeat items`"),
        1 => Ok(found.remove(0)),
        n => anyhow::bail!(
            "`{id}` matches {n} items; use the full key from `heartbeat items --json`"
        ),
    }
}

fn describe_item(item: &HeartbeatItem) -> String {
    let mut line = format!(
        "{}  {:<8} {}",
        item.item_ref,
        item.state,
        item.summary.as_deref().unwrap_or(&item.item_key)
    );
    if let Some(until) = item
        .snooze_until_ms
        .filter(|_| item.state == item_state::SNOOZED)
    {
        line.push_str(&format!("  (until {})", local_time(until)));
    }
    line
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
        sweep_at: config.sweep_at_label(),
        max_items: config.max_items,
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
        items: store.recent_items(10)?,
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
    if let Some(n) = report.candidates {
        line.push_str(&format!(" · {n} candidates"));
    }
    if let Some(message) = &report.message {
        line.push_str(&format!("\n{message}"));
    }
    for o in &report.outreach {
        line.push_str(&format!("\noutreach: {} → {}", o.contact, o.status));
        if let Some(reason) = &o.reason {
            line.push_str(&format!(" ({reason})"));
        }
        if let Some(detail) = &o.detail {
            line.push_str(&format!(" · {detail}"));
        }
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
        "heartbeat: {} · daily sweep at {} · up to {} items · {window} · cap {}/day · timeout {}s",
        if view.enabled { "enabled" } else { "disabled" },
        view.sweep_at,
        view.max_items,
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
        println!("STALE: no sweep attempted in over a day");
    }
    if !view.items.is_empty() {
        println!("recent items (heartbeat done|snooze <ref>):");
        for item in &view.items {
            println!("  {}", describe_item(item));
        }
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
        let cli = crate::Cli::try_parse_from([
            "augmentagent",
            "heartbeat",
            "snooze",
            "a1b2c3",
            "--for",
            "7d",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            crate::Cmd::Heartbeat {
                op: HeartbeatOp::Snooze { ref id, ref duration }
            } if id == "a1b2c3" && duration == "7d"
        ));
        let cli =
            crate::Cli::try_parse_from(["augmentagent", "heartbeat", "done", "a1b2c3"]).unwrap();
        assert!(matches!(
            cli.cmd,
            crate::Cmd::Heartbeat {
                op: HeartbeatOp::Done { ref id }
            } if id == "a1b2c3"
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
