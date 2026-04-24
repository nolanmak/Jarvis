//! Pending-action nudge scheduler — serial queue mode.
//!
//! Surfaces pending approval cards one at a time. While there's an "active"
//! card (a pending row with `nudgeCount > 0`) the user is looking at, no new
//! cards are promoted from the backlog. When the user resolves the active
//! card via approve/skip (status → terminal), the next backlog item is
//! promoted instantly via `post_next_if_idle()` — no waiting for the next
//! tick. The 60s tick is the safety net for restart and for re-posting an
//! ignored active card after each 6h interval.
//!
//! ## Display
//!
//! Posted cards prepend a header: `🔔 reminder · 3/6 · pending since {ts}`.
//! The X/Y session counter lives in-process: `shown` increments on each new
//! promotion (not on re-posts of the same active card); `total` updates to
//! `max(total, shown + remaining_backlog)` so cards arriving mid-session
//! expand Y rather than overflowing X past Y. State resets on daemon restart
//! — known limitation, acceptable for v1.
//!
//! ## Revise
//!
//! Revise on a nudge card runs through the standard `Revised` outcome
//! (instant new draft posted by the broker's event handler) and calls
//! `Store::reset_nudge_schedule` to push the timer 6h out — but **does
//! not** zero `nudgeCount`. The card therefore stays the active one until
//! the user finally approves or skips.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use augmentagent_store::{PendingNudge, Store, NUDGE_INTERVAL_MS};

use crate::ApprovalBroker;

/// In-memory session counter. `total` is the queue size at the moment the
/// session started; `shown` is the number of distinct cards we've promoted to
/// active so far in this session. Both reset to None when the queue empties.
#[derive(Debug, Clone, Copy, Default)]
struct Session {
    shown: i64,
    total: i64,
}

pub struct NudgeScheduler {
    store: Arc<Store>,
    approvals: Arc<dyn ApprovalBroker>,
    /// How often the scheduler wakes to recheck. 60s — cheap query, mainly
    /// matters at startup; the resolve path triggers immediate posts.
    tick_interval: Duration,
    session: Mutex<Option<Session>>,
}

impl NudgeScheduler {
    pub fn new(store: Arc<Store>, approvals: Arc<dyn ApprovalBroker>) -> Self {
        Self {
            store,
            approvals,
            tick_interval: Duration::from_secs(60),
            session: Mutex::new(None),
        }
    }

    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) -> anyhow::Result<()> {
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("nudge scheduler: shutdown");
                    return Ok(());
                }
                _ = ticker.tick() => {
                    if let Err(e) = self.post_next_if_idle().await {
                        error!("nudge tick failed: {e:#}");
                    }
                }
            }
        }
    }

    /// Surface the next card if we're idle. Three branches:
    /// - active card with timer-not-fired → wait
    /// - active card with timer fired → re-post same card (6h reminder)
    /// - no active card → promote head of backlog (or end session if empty)
    ///
    /// Returns `Ok(true)` if a card was posted this call.
    pub async fn post_next_if_idle(&self) -> anyhow::Result<bool> {
        let now = now_millis();

        // 1. Is there a card already active?
        if let Some(active) = self.store.find_active_nudge()? {
            let timer_fired = active
                .next_nudge_at_ms
                .map(|t| t <= now)
                .unwrap_or(true);
            if !timer_fired {
                debug!(
                    action_id = %active.action.action.id,
                    "nudge: active card not due yet"
                );
                return Ok(false);
            }
            // Re-nudge the same active card.
            return self.post_card(&active, /*new_promotion=*/ false, now).await;
        }

        // 2. No active. Try to promote head of backlog.
        match self.store.find_next_to_promote(now)? {
            None => {
                // Queue empty — close the session.
                let mut s = self.session.lock().expect("session mutex poisoned");
                if s.is_some() {
                    debug!("nudge: queue empty, ending session");
                }
                *s = None;
                Ok(false)
            }
            Some(next) => self.post_card(&next, /*new_promotion=*/ true, now).await,
        }
    }

    async fn post_card(
        &self,
        card: &PendingNudge,
        new_promotion: bool,
        now: i64,
    ) -> anyhow::Result<bool> {
        let action_id = card.action.action.id.clone();
        let draft = card
            .action
            .action
            .draft_body
            .clone()
            .unwrap_or_default();

        // Compute X/Y. Update session state for new promotions only — re-nudges
        // of the active card don't bump `shown`.
        let (x, y) = {
            let mut s = self.session.lock().expect("session mutex poisoned");
            let pending = self.store.count_pending_overdue(now)?;
            let mut session = s.unwrap_or(Session { shown: 0, total: 0 });
            if new_promotion {
                session.shown += 1;
            }
            // pending includes the just-promoted card (still pending), but
            // not yet-resolved past cards. shown is the running count of
            // promotions; remaining = pending - 1 if active else pending.
            // For total: shown + remaining_after_this >= shown.
            let remaining_after_this = (pending - 1).max(0);
            let projected_total = session.shown + remaining_after_this;
            session.total = session.total.max(projected_total);
            *s = Some(session);
            (session.shown, session.total)
        };

        let header = format_header(x, y, &card.action.action.created_at);
        let body = format!("{header}\n\n{draft}");

        match self
            .approvals
            .post_approval(&action_id, &card.action.email, &body)
            .await
        {
            Ok(()) => {
                if let Err(e) = self.store.record_nudge(&action_id, now + NUDGE_INTERVAL_MS) {
                    warn!(action_id, "record_nudge failed: {e}");
                    return Err(e.into());
                }
                info!(
                    action_id,
                    new_promotion, x, y, "nudge card posted"
                );
                Ok(true)
            }
            Err(e) => {
                warn!(action_id, "nudge post_approval failed: {e}");
                // On failure, still advance the timer so a broken broker
                // doesn't get hammered every tick. Do NOT bump nudgeCount —
                // a failed post shouldn't promote a backlog card to active.
                let _ = self.store.reset_nudge_schedule(&action_id);
                // Roll back the shown bump if this was a new promotion.
                if new_promotion {
                    let mut s = self.session.lock().expect("session mutex poisoned");
                    if let Some(session) = s.as_mut() {
                        session.shown = (session.shown - 1).max(0);
                    }
                }
                Ok(false)
            }
        }
    }
}

fn format_header(x: i64, y: i64, pending_since: &str) -> String {
    format!("🔔 reminder · {x}/{y} · pending since {pending_since}")
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use augmentagent_store::Email;
    use std::sync::Mutex as StdMutex;

    use crate::{ApprovalBroker, ApprovalError};

    #[derive(Default)]
    struct CountingBroker {
        posts: StdMutex<Vec<(String, String)>>,
    }

    #[async_trait]
    impl ApprovalBroker for CountingBroker {
        async fn post_approval(
            &self,
            action_id: &str,
            _email: &Email,
            draft: &str,
        ) -> Result<(), ApprovalError> {
            self.posts
                .lock()
                .unwrap()
                .push((action_id.to_string(), draft.to_string()));
            Ok(())
        }

        async fn post_flag_notice(
            &self,
            _email: &Email,
            _reason: &str,
        ) -> Result<(), ApprovalError> {
            Ok(())
        }
    }

    #[test]
    fn header_includes_xy_and_timestamp() {
        let h = format_header(3, 6, "2026-04-24T12:00:00Z");
        assert!(h.contains("3/6"), "expected `3/6` in header, got: {h}");
        assert!(h.contains("2026-04-24T12:00:00Z"));
    }

    #[test]
    fn nudge_interval_is_six_hours() {
        assert_eq!(NUDGE_INTERVAL_MS, 6 * 60 * 60 * 1000);
    }
}
