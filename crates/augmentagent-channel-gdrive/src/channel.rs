use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{Email, Store};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use crate::composio::ComposioClient;
use crate::drive::{get_start_page_token, list_changes};
use crate::PLATFORM;

/// Drive changes are low-volume; 30 min keeps Composio calls + Discord noise low.
pub const DEFAULT_POLL_SECS: u64 = 30 * 60;
/// Safety cap on pages walked per account per tick (avoids an unbounded
/// backlog walk pinning a tick).
const MAX_PAGES: usize = 20;

pub struct GDriveChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
}

impl Default for GDriveChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct PollOutcome {
    pub accounts_polled: usize,
    pub baselined: usize,
    pub changes_seen: usize,
    pub new_changes: usize,
    pub digests_posted: usize,
    pub errors: usize,
}

pub struct GDriveChannel {
    store: Arc<Store>,
    composio: Arc<ComposioClient>,
    approvals: Arc<dyn ApprovalBroker>,
    config: GDriveChannelConfig,
}

impl GDriveChannel {
    pub fn new(
        store: Arc<Store>,
        composio: Arc<ComposioClient>,
        approvals: Arc<dyn ApprovalBroker>,
        config: GDriveChannelConfig,
    ) -> Self {
        Self {
            store,
            composio,
            approvals,
            config,
        }
    }

    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.config.poll_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("gdrive channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "gdrive poll complete"),
                        Err(e) => error!("gdrive poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let accounts = self.store.get_active_drive_accounts()?;
        for acct in accounts {
            outcome.accounts_polled += 1;
            let entity = &acct.entity_id;
            let label = if acct.email.is_empty() {
                entity.clone()
            } else {
                acct.email.clone()
            };

            // Establish a baseline cursor on first sight — no historical flood.
            let token = match self.store.get_drive_sync_token(entity)? {
                Some(t) => t,
                None => {
                    match get_start_page_token(&self.composio, entity).await {
                        Ok(t) => {
                            if self.config.dry_run {
                                info!(account = %label, "gdrive dry-run: would baseline at {t}");
                            } else {
                                self.store.set_drive_sync_token(entity, &t)?;
                                info!(account = %label, "gdrive: baselined at {t}");
                            }
                            outcome.baselined += 1;
                        }
                        Err(e) => {
                            error!(account = %label, "gdrive baseline failed: {e:#}");
                            outcome.errors += 1;
                        }
                    }
                    continue;
                }
            };

            let mut cursor = token;
            let mut fresh: Vec<String> = Vec::new();
            let mut final_token: Option<String> = None;
            let mut pages = 0usize;
            loop {
                let page = match list_changes(&self.composio, entity, &cursor).await {
                    Ok(p) => p,
                    Err(e) => {
                        error!(account = %label, "gdrive list_changes failed: {e:#}");
                        outcome.errors += 1;
                        break;
                    }
                };
                for ch in &page.changes {
                    outcome.changes_seen += 1;
                    if ch.removed || ch.file_id.is_empty() {
                        continue;
                    }
                    if self.config.dry_run {
                        fresh.push(render_change(&ch.name, &ch.web_view_link));
                        continue;
                    }
                    let email = Email {
                        message_id: format!(
                            "gdrive:{entity}:{}:{}",
                            ch.file_id, ch.modified_time
                        ),
                        thread_id: Some(entity.clone()),
                        from: label.clone(),
                        subject: if ch.name.is_empty() {
                            ch.file_id.clone()
                        } else {
                            ch.name.clone()
                        },
                        body: format!(
                            "{} ({})\n{}",
                            ch.name, ch.mime_type, ch.web_view_link
                        ),
                        date: ch.modified_time.clone(),
                        account_entity_id: Some(entity.clone()),
                        platform: PLATFORM.to_string(),
                        kind: "file_change".to_string(),
                    };
                    if self.store.upsert_email(&email)? {
                        outcome.new_changes += 1;
                        fresh.push(render_change(&ch.name, &ch.web_view_link));
                    }
                }
                pages += 1;
                if let Some(next) = page.next_page_token {
                    if pages >= MAX_PAGES {
                        final_token = Some(next);
                        break;
                    }
                    cursor = next;
                } else {
                    final_token = page.new_start_page_token.or(Some(cursor));
                    break;
                }
            }

            if self.config.dry_run {
                if !fresh.is_empty() {
                    info!(account = %label, count = fresh.len(), "gdrive dry-run: would announce");
                }
                continue;
            }
            if let Some(tok) = final_token {
                if let Err(e) = self.store.set_drive_sync_token(entity, &tok) {
                    error!(account = %label, "gdrive: persist page token failed: {e:#}");
                    outcome.errors += 1;
                }
            }
            if fresh.is_empty() {
                continue;
            }
            let title = format!("Drive: {label}");
            let body = format!(
                "**{} file change(s)**\n\n{}",
                fresh.len(),
                fresh.join("\n")
            );
            match self.approvals.post_digest(&title, &body).await {
                Ok(()) => outcome.digests_posted += 1,
                Err(e) => {
                    error!(account = %label, "gdrive post_digest failed: {e}");
                    outcome.errors += 1;
                }
            }
        }
        Ok(outcome)
    }
}

fn render_change(name: &str, link: &str) -> String {
    let n = if name.is_empty() { "(unnamed)" } else { name };
    if link.is_empty() {
        format!("• {n}")
    } else {
        format!("• {n} — {link}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_change_with_and_without_link() {
        assert_eq!(
            render_change("Plan.doc", "https://x/1"),
            "• Plan.doc — https://x/1"
        );
        assert_eq!(render_change("", ""), "• (unnamed)");
    }
}
