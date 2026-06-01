//! Poll loop modeled on `augmentagent-channel-github`'s channel + the
//! `discord-dm` digest scheduler. Digest-only: new events are deduped via
//! `Store::upsert_email` (returns `true` only the first time a message_id is
//! seen) and announced once via `ApprovalBroker::post_digest`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use augmentagent_approval_discord::ApprovalBroker;
use augmentagent_store::{Email, Store, SubscriptionMode};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::client::{MeetupClient, MeetupError, MeetupEvent};
use crate::PLATFORM;

/// Events change slowly; 6h keeps Discord quiet and Node spawns rare.
pub const DEFAULT_POLL_SECS: u64 = 6 * 3600;
/// Per-group cap so one huge calendar can't flood a tick.
const DEFAULT_MAX_EVENTS: usize = 25;

pub struct MeetupChannelConfig {
    pub poll_interval: Duration,
    pub dry_run: bool,
    pub max_events: usize,
}

impl Default for MeetupChannelConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_SECS),
            dry_run: true,
            max_events: DEFAULT_MAX_EVENTS,
        }
    }
}

#[derive(Debug, Default)]
pub struct PollOutcome {
    pub groups_polled: usize,
    pub events_seen: usize,
    pub new_events: usize,
    pub digests_posted: usize,
    pub stale_hash_groups: usize,
    pub errors: usize,
}

pub struct MeetupChannel {
    store: Arc<Store>,
    client: Arc<MeetupClient>,
    approvals: Arc<dyn ApprovalBroker>,
    config: MeetupChannelConfig,
}

impl MeetupChannel {
    pub fn new(
        repo_root: PathBuf,
        store: Arc<Store>,
        approvals: Arc<dyn ApprovalBroker>,
        config: MeetupChannelConfig,
    ) -> Self {
        Self {
            store,
            client: Arc::new(MeetupClient::new(&repo_root)),
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
                    info!("meetup channel: shutdown signal received");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    match self.poll_once().await {
                        Ok(out) => info!(?out, "meetup poll complete"),
                        Err(e) => error!("meetup poll failed: {e:#}"),
                    }
                }
            }
        }
    }

    pub async fn poll_once(&self) -> anyhow::Result<PollOutcome> {
        let mut outcome = PollOutcome::default();
        let subs = self.store.list_active_subscriptions(PLATFORM)?;
        for sub in subs {
            outcome.groups_polled += 1;
            let events = match self
                .client
                .upcoming_events(&sub.channel_id, self.config.max_events)
                .await
            {
                Ok(e) => e,
                Err(MeetupError::StalePersistedQuery) => {
                    warn!(group = %sub.channel_id, "meetup hash stale — skipping (refresh via /intercept)");
                    outcome.stale_hash_groups += 1;
                    continue;
                }
                Err(e) => {
                    error!(group = %sub.channel_id, "meetup fetch failed: {e:#}");
                    outcome.errors += 1;
                    continue;
                }
            };

            let mut fresh: Vec<String> = Vec::new();
            for ev in &events {
                outcome.events_seen += 1;
                if self.config.dry_run {
                    // Preview only: no persistence, no announce.
                    fresh.push(render_event(ev));
                    continue;
                }
                let email = Email {
                    message_id: format!("meetup:{}", ev.id),
                    thread_id: Some(sub.channel_id.clone()),
                    from: sub.display_name.clone(),
                    subject: ev.title.clone(),
                    body: render_event(ev),
                    date: ev.date_time.clone().unwrap_or_default(),
                    account_entity_id: None,
                    platform: PLATFORM.to_string(),
                    kind: "event".to_string(),
                };
                let is_new = self.store.upsert_email(&email)?;
                if is_new {
                    outcome.new_events += 1;
                    if sub.mode != SubscriptionMode::StoreOnly {
                        fresh.push(render_event(ev));
                    }
                }
            }

            if fresh.is_empty() {
                continue;
            }
            if self.config.dry_run {
                info!(group = %sub.channel_id, count = fresh.len(), "meetup dry-run: would announce");
                continue;
            }
            let title = format!("Meetup: {}", sub.display_name);
            let body = format!(
                "**{} new event(s) — {}**\n\n{}",
                fresh.len(),
                sub.display_name,
                fresh.join("\n\n")
            );
            match self.approvals.post_digest(&title, &body).await {
                Ok(()) => outcome.digests_posted += 1,
                Err(e) => {
                    error!(group = %sub.channel_id, "meetup post_digest failed: {e}");
                    outcome.errors += 1;
                }
            }
        }
        Ok(outcome)
    }
}

pub fn render_event(ev: &MeetupEvent) -> String {
    let when = ev.date_time.as_deref().unwrap_or("(date TBD)");
    let where_ = if ev.is_online {
        "online".to_string()
    } else if let Some(v) = &ev.venue {
        let mut parts: Vec<&str> = Vec::new();
        if !v.name.is_empty() {
            parts.push(&v.name);
        }
        if !v.city.is_empty() {
            parts.push(&v.city);
        }
        if parts.is_empty() {
            "in person".to_string()
        } else {
            parts.join(", ")
        }
    } else {
        "in person".to_string()
    };
    let going = ev
        .going
        .map(|n| format!(" · {n} going"))
        .unwrap_or_default();
    format!(
        "• **{}**\n  {} · {}{}\n  {}",
        ev.title, when, where_, going, ev.url
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_event_includes_title_and_url() {
        let ev = MeetupEvent {
            id: "1".into(),
            title: "Coffee & Code".into(),
            url: "https://meetup.com/x/events/1".into(),
            status: "ACTIVE".into(),
            date_time: Some("2026-06-01T18:00:00-04:00".into()),
            is_online: false,
            going: Some(12),
            venue: Some(crate::client::MeetupVenue {
                name: "Cafe".into(),
                city: "Philadelphia".into(),
                state: "PA".into(),
            }),
        };
        let s = render_event(&ev);
        assert!(s.contains("Coffee & Code"));
        assert!(s.contains("https://meetup.com/x/events/1"));
        assert!(s.contains("12 going"));
        assert!(s.contains("Cafe, Philadelphia"));
    }

    #[test]
    fn render_event_handles_online_and_missing_fields() {
        let ev = MeetupEvent {
            id: "2".into(),
            title: "Virtual Standup".into(),
            url: "https://meetup.com/x/events/2".into(),
            status: "ACTIVE".into(),
            date_time: None,
            is_online: true,
            going: None,
            venue: None,
        };
        let s = render_event(&ev);
        assert!(s.contains("online"));
        assert!(s.contains("(date TBD)"));
        assert!(!s.contains("going"));
    }
}
