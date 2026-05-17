//! Weekly Orchid invoice automation.
//!
//! A thin Rust layer over `scripts/invoice/send_invoice.py`. Rust owns durable
//! state (recipient, sequential counter from #35, Composio sending entity,
//! last-billed week) in the `invoice_config` store table; Python does PDF
//! generation + the Composio-SDK send (real PDF attachment via auto-upload).
//!
//! The scheduler ticks hourly and only acts on Sundays: it bills the
//! Sun→Sun week that just closed (`(today-7, today]`), once, idempotently.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, Weekday};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use augmentagent_store::Store;

const DEFAULT_RECIPIENT: &str = "REDACTED";

/// Where the vendored Python tooling lives. Override with `INVOICE_SCRIPTS_DIR`
/// (the daemon's cwd is the repo root, where `data.db` sits).
fn scripts_dir() -> String {
    std::env::var("INVOICE_SCRIPTS_DIR").unwrap_or_else(|_| "scripts/invoice".to_string())
}

pub struct InvoiceScheduler {
    pub store: Arc<Store>,
    /// How often to wake and check whether it's billing time. 1h — cheap; all
    /// non-Sunday ticks and already-billed Sundays short-circuit immediately.
    pub tick_interval: Duration,
}

impl InvoiceScheduler {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            tick_interval: Duration::from_secs(3600),
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!("invoice scheduler: started (weekly, Sundays)");
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("invoice scheduler: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.tick_once().await {
                        error!("invoice tick failed: {e:#}");
                    }
                }
            }
        }
    }

    /// One pass. No-op unless today is Sunday and this week isn't billed yet.
    pub async fn tick_once(&self) -> anyhow::Result<()> {
        let today = Local::now().date_naive();
        if today.weekday() != Weekday::Sun {
            return Ok(());
        }
        let week_end = today;
        if self.store.get_invoice_config("last_billed_week_end")?.as_deref()
            == Some(week_end.to_string().as_str())
        {
            return Ok(()); // already billed this week
        }
        // Master kill switch. Off by default (seeded 'false'); flip it on via
        // the dashboard, `!invoice autosend on`, or `invoice set-auto-send`.
        // This is what makes a fresh production deploy safe — the scheduler
        // spawns, ticks, and finds nothing to do until a human enables it.
        if self.store.get_invoice_config("auto_send_enabled")?.as_deref() != Some("true") {
            info!(
                "invoice scheduler: auto-send disabled — skipping {week_end} \
                 (enable via dashboard or `!invoice autosend on`)"
            );
            return Ok(());
        }
        match run_invoice(&self.store, Some(week_end), false).await {
            Ok(msg) => info!("invoice scheduler: {msg}"),
            Err(e) => warn!("invoice scheduler: send failed, will retry next tick: {e:#}"),
        }
        Ok(())
    }
}

/// Generate (and unless `dry_run`, send) the invoice for the Sun→Sun week
/// ending `week_end` (defaults to the most recent Sunday). On a successful
/// real send, advances the counter and records the billed week so it can't
/// double-fire. Returns a human-readable summary line.
pub async fn run_invoice(
    store: &Store,
    week_end: Option<NaiveDate>,
    dry_run: bool,
) -> Result<String> {
    let end = match week_end {
        Some(d) => d,
        None => most_recent_sunday(Local::now().date_naive()),
    };
    let start = end - ChronoDuration::days(7);
    let today = Local::now().date_naive();

    let recipient = store
        .get_invoice_config("recipient_email")?
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_RECIPIENT.to_string());
    let from_entity = store
        .get_invoice_config("from_entity")?
        .unwrap_or_default();
    // Peek (don't burn) the number — only commit it on a successful send.
    let number = store.invoice_counter()?;

    let dir = scripts_dir();
    let script = format!("{dir}/send_invoice.py");
    let mut cmd = Command::new("python3");
    cmd.arg(&script)
        .arg("--number").arg(number.to_string())
        .arg("--start").arg(start.to_string())
        .arg("--end").arg(end.to_string())
        .arg("--invoice-date").arg(today.to_string())
        .arg("--to").arg(&recipient)
        .arg("--from-entity").arg(&from_entity)
        .arg("--dry-run").arg(if dry_run { "true" } else { "false" })
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd
        .output()
        .await
        .with_context(|| format!("spawning {script} (is python3 on PATH?)"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        anyhow::bail!(
            "invoice script failed (status {}):\n{stderr}\n{stdout}",
            out.status
        );
    }

    if !dry_run {
        // Commit only after the send succeeded so a failure never burns a
        // number or marks the week billed.
        store.set_invoice_config("invoice_counter", &(number + 1).to_string())?;
        store.set_invoice_config("last_billed_week_end", &end.to_string())?;
    }
    Ok(format!(
        "invoice #{number} {start}→{end} {} ({})",
        if dry_run { "generated (dry-run)" } else { "SENT" },
        recipient
    ))
}

/// List Composio-connected Gmail accounts (email → entity) so the user can
/// pick the sending identity. Delegates to the Python helper.
pub async fn list_accounts() -> Result<String> {
    let dir = scripts_dir();
    let script = format!("{dir}/send_invoice.py");
    let out = Command::new("python3")
        .arg(&script)
        .arg("--list-accounts")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawning {script}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "list-accounts failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Most recent Sunday on or before `d` (so a Sunday returns itself).
fn most_recent_sunday(d: NaiveDate) -> NaiveDate {
    let back = d.weekday().num_days_from_sunday() as i64;
    d - ChronoDuration::days(back)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sunday_anchor() {
        // 2026-05-17 is a Sunday → itself.
        let sun = NaiveDate::from_ymd_opt(2026, 5, 17).unwrap();
        assert_eq!(most_recent_sunday(sun), sun);
        // 2026-05-20 (Wed) → back to 2026-05-17.
        let wed = NaiveDate::from_ymd_opt(2026, 5, 20).unwrap();
        assert_eq!(most_recent_sunday(wed), sun);
        // 2026-05-16 (Sat) → 2026-05-10.
        let sat = NaiveDate::from_ymd_opt(2026, 5, 16).unwrap();
        assert_eq!(
            most_recent_sunday(sat),
            NaiveDate::from_ymd_opt(2026, 5, 10).unwrap()
        );
    }
}
