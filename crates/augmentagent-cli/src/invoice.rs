//! Weekly invoice automation. All personal/business data is read from
//! runtime config (gitignored DB + .env) — nothing is hardcoded.
//!
//! A thin Rust layer over `scripts/invoice/send_invoice.py`. Rust owns durable
//! state (recipient, sequential counter from #35, Composio sending entity,
//! last-billed week) in the `invoice_config` store table; Python does PDF
//! generation + the Composio-SDK send (real PDF attachment via auto-upload).
//!
//! The scheduler ticks hourly and only acts on Sundays: it generates the
//! Sun→Sun week's PDF and posts a Discord approval card with the PDF
//! attached. A human clicks Approve to actually send, Reject to discard.
//! Idempotency: one pending draft per `week_end`.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, Local, NaiveDate, Weekday};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use augmentagent_store::Store;

/// Trait the Sunday scheduler uses to post a weekly invoice draft. The CLI
/// wires the concrete impl that bridges store + PDF generation + Discord
/// post; tests pass a no-op so the timing logic can be exercised without a
/// live serenity client.
#[async_trait::async_trait]
pub trait InvoiceDraftPoster: Send + Sync {
    /// Generate the PDF for `week_end`, post it as a Discord card with
    /// Approve/Reject, and record the draft row. Returns a one-line summary
    /// for the scheduler log. Implementations must be idempotent against
    /// re-runs — the scheduler also guards via
    /// `Store::get_pending_invoice_draft_for_week`.
    async fn dispatch_draft(&self, week_end: NaiveDate) -> Result<String>;
}

// No hardcoded recipient — the real value lives in the gitignored
// `invoice_config` DB table (set via dashboard / `!invoice recipient`).
// Empty fallback means "unconfigured": the send path errors loudly rather
// than mailing a baked-in personal address.
const DEFAULT_RECIPIENT: &str = "";

/// Where the vendored Python tooling lives. Override with `INVOICE_SCRIPTS_DIR`.
///
/// Resolution order:
/// 1. `INVOICE_SCRIPTS_DIR` env var (explicit override; tests, packaged installs).
///    A blank/whitespace-only value is treated as unset so it can't short-circuit
///    to an invalid `/send_invoice.py` path — resolution falls through to (2).
/// 2. Walk up from the running binary (`target/release/augmentagent`) and pick the
///    first ancestor containing `scripts/invoice/send_invoice.py`. This is what
///    makes wiki-ask and other sub-CLI invocations work — the spawned CLI's cwd
///    is pinned to the wiki root (so Write/Edit can't escape the wiki), but
///    `scripts/invoice/` lives at the repo root. Before this fallback, the
///    relative lookup resolved to `<wiki>/scripts/invoice/send_invoice.py`,
///    which doesn't exist → the script appeared to be "missing" (see #316).
/// 3. Last-resort relative path, preserving the original cwd-relative behavior
///    for unit tests that run from arbitrary tmpdirs.
fn scripts_dir() -> String {
    if let Ok(v) = std::env::var("INVOICE_SCRIPTS_DIR") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1) {
            let candidate = ancestor.join("scripts").join("invoice");
            if candidate.join("send_invoice.py").is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "scripts/invoice".to_string()
}

pub struct InvoiceScheduler {
    pub store: Arc<Store>,
    /// How often to wake and check whether it's draft time. 1h — cheap; all
    /// non-Sunday ticks and already-drafted Sundays short-circuit immediately.
    pub tick_interval: Duration,
    /// Optional poster. `None` ⇒ scheduler still runs but logs and skips the
    /// post step. Useful for non-Discord deploys; absent in dry-run.
    pub poster: Option<Arc<dyn InvoiceDraftPoster>>,
}

impl InvoiceScheduler {
    pub fn new(store: Arc<Store>, poster: Option<Arc<dyn InvoiceDraftPoster>>) -> Self {
        Self {
            store,
            tick_interval: Duration::from_secs(3600),
            poster,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        info!("invoice scheduler: started (weekly drafts, Sundays)");
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

    /// One pass. No-op unless today is Sunday, the week isn't already billed,
    /// and there isn't already a pending draft for it.
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
        // the dashboard, `!invoice autodraft on`, or `invoice set-auto-draft`.
        // This is what makes a fresh production deploy safe — the scheduler
        // spawns, ticks, and finds nothing to do until a human enables it.
        if self.store.get_invoice_config("auto_draft_enabled")?.as_deref() != Some("true") {
            info!(
                "invoice scheduler: auto-draft disabled — skipping {week_end} \
                 (enable via dashboard or `!invoice autodraft on`)"
            );
            return Ok(());
        }
        // Idempotency guard: one pending draft per week. Re-running a tick on
        // a Sunday the scheduler already posted for must not double-post.
        if self
            .store
            .get_pending_invoice_draft_for_week(&week_end.to_string())?
            .is_some()
        {
            info!(
                "invoice scheduler: a pending draft already exists for {week_end}; skipping"
            );
            return Ok(());
        }
        let Some(poster) = self.poster.as_ref() else {
            warn!(
                "invoice scheduler: auto-draft on but no poster wired; skipping {week_end}"
            );
            return Ok(());
        };
        match poster.dispatch_draft(week_end).await {
            Ok(msg) => info!("invoice scheduler: {msg}"),
            Err(e) => warn!(
                "invoice scheduler: draft post failed, will retry next tick: {e:#}"
            ),
        }
        Ok(())
    }
}

/// Generated-PDF metadata returned by [`generate_pdf`] so callers (the
/// Discord post helper, manual CLI runs) can attach it to a card.
#[derive(Debug, Clone)]
pub struct GeneratedPdf {
    pub number: u32,
    pub week_start: NaiveDate,
    pub week_end: NaiveDate,
    pub pdf_path: PathBuf,
    pub recipient: String,
}

/// Generate the PDF for `week_end` (defaults to the most recent Sunday) via
/// the python script in dry-run mode. Does NOT mutate the counter or the
/// `invoice_drafts` table — that's the caller's job, ensuring the counter
/// only advances on a real send and one draft row exists per
/// (recipient, week_end).
pub async fn generate_pdf(
    store: &Store,
    week_end: Option<NaiveDate>,
) -> Result<GeneratedPdf> {
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
    if recipient.trim().is_empty() {
        anyhow::bail!(
            "invoice recipient not configured — set it via the dashboard or \
             `!invoice recipient <email>` (no address is hardcoded)"
        );
    }
    let from_entity = store
        .get_invoice_config("from_entity")?
        .unwrap_or_default();
    let number = store.invoice_counter()?;
    let pdf_path = pdf_path_for(number);
    if let Some(parent) = pdf_path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }

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
        .arg("--out").arg(&pdf_path)
        .arg("--dry-run").arg("true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd
        .output()
        .await
        .with_context(|| format!("spawning {script} (is python3 on PATH?)"))?;
    if !out.status.success() {
        anyhow::bail!(
            "invoice script failed (status {}):\n{}\n{}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
    }
    if !pdf_path.exists() {
        anyhow::bail!(
            "invoice script reported success but PDF not found at {}",
            pdf_path.display()
        );
    }
    Ok(GeneratedPdf {
        number,
        week_start: start,
        week_end: end,
        pdf_path,
        recipient,
    })
}

/// Deterministic PDF path for `send_invoice.py`. Mirrors the script's
/// default output (`INVOICE_OUT_DIR` || /tmp/invoices / `Orchid_Invoice_N.pdf`)
/// so Rust can pass `--out` explicitly and then read the file back to attach
/// it to the Discord card.
pub fn pdf_path_for(number: u32) -> PathBuf {
    let dir =
        std::env::var("INVOICE_OUT_DIR").unwrap_or_else(|_| "/tmp/invoices".to_string());
    PathBuf::from(dir).join(format!("Orchid_Invoice_{number}.pdf"))
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
    if recipient.trim().is_empty() {
        anyhow::bail!(
            "invoice recipient not configured — set it via the dashboard or \
             `!invoice recipient <email>` (no address is hardcoded)"
        );
    }
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

    #[test]
    fn blank_scripts_dir_env_is_ignored() {
        // A blank/whitespace INVOICE_SCRIPTS_DIR must not short-circuit to an
        // invalid path (regression for the `/send_invoice.py` foot-gun flagged
        // in review). With it blank, scripts_dir() falls through to the
        // binary-ancestor search or the relative fallback — either way the
        // result is a non-empty path that is NOT the blank value.
        // Only this test touches INVOICE_SCRIPTS_DIR, so the env write is
        // isolated from other tests' reads.
        std::env::set_var("INVOICE_SCRIPTS_DIR", "   ");
        let dir = scripts_dir();
        std::env::remove_var("INVOICE_SCRIPTS_DIR");
        assert!(
            !dir.trim().is_empty(),
            "blank env must not yield a blank scripts dir, got {dir:?}"
        );
        assert_ne!(dir.trim(), "", "scripts dir should never be the blank override");
    }

    #[test]
    fn pdf_path_matches_python_default_layout() {
        // Pin the env to a fixed dir so the assertion is hermetic regardless
        // of any ambient INVOICE_OUT_DIR the caller set.
        // SAFETY: tests run single-threaded enough for this and we don't
        // assert other env state below.
        // Using a sentinel rather than removing it so a parallel test that
        // also touches INVOICE_OUT_DIR doesn't see a missing value.
        std::env::set_var("INVOICE_OUT_DIR", "/tmp/aa-invoice-test");
        let p = pdf_path_for(42);
        assert_eq!(
            p,
            std::path::PathBuf::from("/tmp/aa-invoice-test/Orchid_Invoice_42.pdf")
        );
    }
}
