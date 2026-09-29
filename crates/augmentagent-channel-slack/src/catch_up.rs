//! #1296 — bounded catch-up of subscribed conversations after the host slept
//! or the daemon restarted, through the shared #1318 planner.
//!
//! Each subscribed conversation the app can read has a `surface_cursors` row
//! (seeded the first time it is seen, moved by every live top-level message).
//! [`SubscribedCatchUp::run_once`] plans the gap with
//! [`augmentagent_store::delivery::plan_catch_up`] and fetches it with
//! [`catch_up_conversation`] through [`SlackHistorySource`]
//! (`conversations.history` with the app's bot token), which records each
//! message in the durable inbox under the same `<channel>:<ts>` event ID the
//! live path uses. The inbox dispatcher then hands them to
//! [`crate::ingest::LiveIngest`] like live events, so a message seen live,
//! by the catch-up and by the Composio poll is stored and triaged once.
//!
//! Bounds (see [`default_policy`]): at most `max_window_ms` of history, at
//! most `max_pages` pages of `page_size` per conversation per run. A rate
//! limit stops that conversation until Slack's `Retry-After` has passed; an
//! error (for example `not_in_channel` when the app is not a member) is
//! reported and the Composio poll remains the path for that conversation.
//!
//! Slack returns history newest first. When a gap is larger than the page
//! budget, the newest messages are recorded and the cursor moves past the
//! older ones; the poll (reading forward from its own cursor) still stores
//! those. Records are dispatched in `occurred_at` order whatever order they
//! were fetched in.
//!
//! The owner's bound DM and control channel are never caught up here: their
//! messages are owner turns, not contact messages.
//!
//! [`SubscribedCatchUp::run`] runs once at start (after a restart), then
//! every `tick`; a wall-clock jump larger than the monotonic time that passed
//! (a laptop that slept) triggers a run at once, as does
//! [`SubscribedCatchUp::sweep_every`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use augmentagent_store::delivery::{
    catch_up_conversation, CatchUpPlan, CatchUpPolicy, CatchUpReport, HistoryMessage, HistoryPage,
    HistorySource,
};
use augmentagent_store::slack_ingest::{ts_order, ts_to_ms};
use augmentagent_store::{Store, SurfaceConversationRef};
use serde_json::{json, Value};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::ingest::{Clock, CATCH_UP_ENVELOPE_PREFIX};
use crate::surface::SlackWorkspace;
use crate::transport::web::{HistoryQuery, SlackHistoryMessage, SlackWebApi, WebApiError};

/// Slack errors meaning the app cannot read a conversation (it is not a
/// member, or cannot see it); the poll remains that conversation's path.
const UNREADABLE: &[&str] = &["not_in_channel", "channel_not_found", "missing_scope"];

/// How long an unreadable conversation is left alone before trying again
/// (the app may have been invited meanwhile).
pub const UNREADABLE_BACKOFF_MS: i64 = 6 * 60 * 60 * 1000;

/// 24 hours of history, 5 pages of 100 per conversation per run.
pub fn default_policy() -> CatchUpPolicy {
    CatchUpPolicy {
        max_window_ms: 24 * 60 * 60 * 1000,
        page_size: 100,
        max_pages: 5,
    }
}

/// `conversations.history` as a #1318 [`HistorySource`].
pub struct SlackHistorySource {
    pub web: Arc<dyn SlackWebApi>,
    pub workspace: SlackWorkspace,
}

impl HistorySource for SlackHistorySource {
    async fn fetch_page(
        &self,
        conversation: &SurfaceConversationRef,
        plan: &CatchUpPlan,
        page_cursor: Option<&str>,
    ) -> Result<HistoryPage, String> {
        let channel = conversation.conversation_id();
        // `oldest` is exclusive: after the last message seen, or from the
        // window floor when the gap was truncated.
        let oldest = plan
            .after_message_id
            .clone()
            .filter(|id| ts_order(id).is_some())
            .unwrap_or_else(|| ms_to_ts(plan.oldest_ms));
        let query = HistoryQuery {
            channel: channel.to_string(),
            thread_ts: None,
            oldest: Some(oldest),
            limit: plan.page_size,
            cursor: page_cursor.map(str::to_string),
            include_all_metadata: false,
        };
        match self.web.conversations_history(query).await {
            Err(WebApiError::RateLimited { retry_after }) => Ok(HistoryPage::RateLimited {
                retry_after_ms: i64::try_from(retry_after.as_millis()).unwrap_or(i64::MAX),
            }),
            Err(e) => Err(format!("conversations.history {channel}: {e}")),
            Ok(page) => {
                let mut messages: Vec<HistoryMessage> = page
                    .messages
                    .iter()
                    .filter_map(|m| history_record(&self.workspace, channel, m))
                    .collect();
                messages.sort_by_key(|m| m.occurred_at_ms);
                Ok(HistoryPage::Page {
                    messages,
                    next: page.next_cursor.filter(|c| page.has_more && !c.is_empty()),
                })
            }
        }
    }
}

/// The durable-inbox record for one history message: the same event ID as
/// the live event (`<channel>:<ts>`) and an events-API envelope the
/// dispatcher parses like a live one. `None` for messages without a usable
/// `ts`.
pub fn history_record(
    workspace: &SlackWorkspace,
    channel: &str,
    message: &SlackHistoryMessage,
) -> Option<HistoryMessage> {
    let occurred_at_ms = ts_to_ms(&message.ts)?;
    let event_id = format!("{channel}:{}", message.ts);
    let mut event = match &message.raw {
        Value::Object(_) => message.raw.clone(),
        _ => json!({}),
    };
    event["type"] = json!("message");
    event["channel"] = json!(channel);
    event["ts"] = json!(message.ts);
    for (key, value) in [
        ("user", &message.user),
        ("text", &message.text),
        ("thread_ts", &message.thread_ts),
        ("bot_id", &message.bot_id),
    ] {
        if let (Some(v), true) = (value, event.get(key).is_none()) {
            event[key] = json!(v);
        }
    }
    let mut payload = json!({
        "type": "event_callback",
        "team_id": workspace.team_id(),
        "event_id": format!("{CATCH_UP_ENVELOPE_PREFIX}{event_id}"),
        "event": event,
    });
    if let Some(enterprise) = workspace.enterprise_id() {
        payload["enterprise_id"] = json!(enterprise);
    }
    let frame = json!({
        "type": "events_api",
        "envelope_id": format!("{CATCH_UP_ENVELOPE_PREFIX}{event_id}"),
        "accepts_response_payload": false,
        "payload": payload,
    });
    Some(HistoryMessage {
        event_id,
        message_id: message.ts.clone(),
        kind: "message".into(),
        occurred_at_ms,
        payload: frame.to_string(),
    })
}

/// Milliseconds → a Slack `ts`.
pub fn ms_to_ts(ms: i64) -> String {
    let ms = ms.max(0);
    format!("{}.{:06}", ms / 1000, (ms % 1000) * 1000)
}

/// `true` when the wall clock moved further than the monotonic clock by more
/// than `slack_ms`: the host was suspended in between.
pub fn suspended(
    prev_wall_ms: i64,
    now_wall_ms: i64,
    elapsed_mono: Duration,
    slack_ms: i64,
) -> bool {
    let wall = now_wall_ms.saturating_sub(prev_wall_ms);
    let mono = i64::try_from(elapsed_mono.as_millis()).unwrap_or(i64::MAX);
    wall.saturating_sub(mono) > slack_ms
}

/// One conversation's outcome in a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationCatchUp {
    pub team_id: String,
    pub channel_id: String,
    pub outcome: CatchUpOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatchUpOutcome {
    /// First time this conversation was seen: the cursor starts now.
    Seeded,
    /// Still inside a rate limit from an earlier run.
    RateLimited {
        until_ms: i64,
    },
    Ran(CatchUpReport),
}

pub struct SubscribedCatchUp {
    store: Arc<Store>,
    workspaces: Vec<(SlackWorkspace, Arc<dyn SlackWebApi>)>,
    policy: CatchUpPolicy,
    clock: Clock,
    tick: Duration,
    sweep_every: Duration,
    rate_limited: Mutex<HashMap<String, i64>>,
}

impl SubscribedCatchUp {
    pub fn new(store: Arc<Store>, workspaces: Vec<(SlackWorkspace, Arc<dyn SlackWebApi>)>) -> Self {
        Self {
            store,
            workspaces,
            policy: default_policy(),
            clock: Arc::new(crate::ingest::system_now_ms),
            tick: Duration::from_secs(60),
            sweep_every: Duration::from_secs(30 * 60),
            rate_limited: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_policy(mut self, policy: CatchUpPolicy) -> Self {
        self.policy = policy;
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_timing(mut self, tick: Duration, sweep_every: Duration) -> Self {
        self.tick = tick;
        self.sweep_every = sweep_every;
        self
    }

    fn workspace_for(
        &self,
        account_id: Option<&str>,
    ) -> Option<&(SlackWorkspace, Arc<dyn SlackWebApi>)> {
        match account_id {
            Some(team) => self.workspaces.iter().find(|(w, _)| w.team_id() == team),
            // A legacy subscription with no workspace: only when there is
            // exactly one.
            None if self.workspaces.len() == 1 => self.workspaces.first(),
            None => None,
        }
    }

    /// The owner's bound DM and control channel are owner turns, never
    /// contact history.
    fn is_owner_conversation(&self, team: &str, channel: &str) -> bool {
        match crate::owner_setup::find_binding(&self.store, team) {
            Ok(Some(binding)) => binding.is_control_conversation(channel),
            Ok(None) => false,
            Err(e) => {
                warn!(team, "slack catch-up: cannot read the owner binding: {e}");
                true
            }
        }
    }

    /// Catch up every subscribed conversation once.
    pub async fn run_once(&self) -> Vec<ConversationCatchUp> {
        let now = (self.clock)();
        let subs = match self.store.list_active_subscriptions(crate::PLATFORM) {
            Ok(s) => s,
            Err(e) => {
                warn!("slack catch-up: cannot list subscriptions: {e}");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for sub in subs {
            let Some((workspace, web)) = self.workspace_for(sub.account_id.as_deref()) else {
                continue;
            };
            let team = workspace.team_id().to_string();
            // The app is never a member of the owner's DMs with other people
            // (only its own DM): the poll is their path.
            if sub.channel_id.starts_with('D') || self.is_owner_conversation(&team, &sub.channel_id)
            {
                continue;
            }
            let Ok(conversation) = workspace.conversation(&sub.channel_id, None) else {
                continue;
            };
            let key = conversation.storage_key();
            let result = |outcome| ConversationCatchUp {
                team_id: team.clone(),
                channel_id: sub.channel_id.clone(),
                outcome,
            };
            let limited = self.rate_limited.lock().unwrap().get(&key).copied();
            if let Some(until_ms) = limited.filter(|until| *until > now) {
                out.push(result(CatchUpOutcome::RateLimited { until_ms }));
                continue;
            }
            match self.store.surface_cursor(&conversation) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    // Never seen: start from now, no backfill.
                    if let Err(e) =
                        self.store
                            .advance_surface_cursor(&conversation, &ms_to_ts(now), now, now)
                    {
                        warn!(channel = %sub.channel_id, "slack catch-up: cannot seed the cursor: {e}");
                        continue;
                    }
                    out.push(result(CatchUpOutcome::Seeded));
                    continue;
                }
                Err(e) => {
                    warn!(channel = %sub.channel_id, "slack catch-up: cannot read the cursor: {e}");
                    continue;
                }
            }
            let source = SlackHistorySource {
                web: Arc::clone(web),
                workspace: workspace.clone(),
            };
            match catch_up_conversation(&self.store, &conversation, &source, &self.policy, now)
                .await
            {
                Ok(report) => {
                    let mut limits = self.rate_limited.lock().unwrap();
                    let unreadable = report
                        .error
                        .as_deref()
                        .is_some_and(|e| UNREADABLE.iter().any(|code| e.contains(code)));
                    match report.rate_limited_until_ms {
                        Some(until) => {
                            limits.insert(key, until);
                        }
                        // The app is not in that conversation: do not ask
                        // again on every sweep.
                        None if unreadable => {
                            limits.insert(key, now + UNREADABLE_BACKOFF_MS);
                        }
                        None => {
                            limits.remove(&key);
                        }
                    }
                    if let Some(error) = &report.error {
                        warn!(channel = %sub.channel_id, %error, "slack catch-up failed; the poll remains the path for this conversation");
                    } else if report.accepted > 0 || report.truncated {
                        info!(channel = %sub.channel_id, accepted = report.accepted, duplicates = report.duplicates, pages = report.pages, truncated = report.truncated, more_pending = report.more_pending, "slack catch-up");
                    }
                    out.push(result(CatchUpOutcome::Ran(report)));
                }
                Err(e) => warn!(channel = %sub.channel_id, "slack catch-up: store error: {e}"),
            }
        }
        out
    }

    /// Run at start, then again after a suspension, every
    /// [`Self::with_timing`] sweep, or on the next tick while a page budget
    /// left work pending.
    pub async fn run(self, shutdown: CancellationToken) {
        let slack_ms = i64::try_from(self.tick.as_millis()).unwrap_or(i64::MAX);
        loop {
            let results = self.run_once().await;
            let pending = results.iter().any(|r| {
                matches!(&r.outcome, CatchUpOutcome::Ran(rep)
                    if rep.more_pending && rep.error.is_none() && rep.rate_limited_until_ms.is_none())
            });
            let since_run = Instant::now();
            loop {
                let (wall, mono) = ((self.clock)(), Instant::now());
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = tokio::time::sleep(self.tick) => {}
                }
                if pending {
                    break;
                }
                if suspended(wall, (self.clock)(), mono.elapsed(), slack_ms) {
                    info!("slack catch-up: the host was suspended; catching up");
                    break;
                }
                if since_run.elapsed() >= self.sweep_every {
                    break;
                }
            }
        }
    }
}
