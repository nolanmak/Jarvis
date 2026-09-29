//! #1296 — live ingestion of subscribed Slack conversations, reconciled with
//! the Composio poll.
//!
//! ```text
//! Socket Mode ─► durable inbox ─► owner::admit ─ Ignored/Rejected ─► LiveIngest ─┐
//! catch-up (after sleep) ─► durable inbox ──────────────────────────────────────┤
//! Composio poll ───────────────────────────────────────────────► SlackChannel ─┤
//!                                                                              ▼
//!                             Store::record_slack_message (one row, first path wins)
//!                             Store::claim_slack_triage   (one triage decision)
//! ```
//!
//! Identity: [`SlackMessageKey`] `(team, channel, ts)`, stored as the
//! unchanged `<channel>:<ts>` message ID. A subscribed message produces one
//! `emails` row and at most one triage decision whichever path sees it
//! first; the other paths get a duplicate and do nothing (an edit carried by
//! a later sighting is still applied, once).
//!
//! Only conversations the app is a member of produce live events; everything
//! else (for example the owner's DMs with other people) keeps arriving
//! through the poll, which stays the fallback.
//!
//! Live triage runs off the inbox dispatcher ([`SubscribedEventSink::observe`]
//! records synchronously, then spawns the triage), so a slow model never
//! holds an owner's turn.

use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_channel_core::Reasoner;
use augmentagent_store::slack_ingest::{
    SlackDeleteOutcome, SlackEditOutcome, SlackIngestSource, SlackMessageKey, SlackRecordOutcome,
};
use augmentagent_store::{ChannelSubscription, Email, Store};
use serde_json::Value;
use tracing::{debug, warn};

use crate::channel::{PollOutcome, SlackChannel};
use crate::transport::event::{EventEnvelope, SlackEvent};
use crate::types::SlackMessage;

/// A triage claim older than this belongs to a daemon that stopped
/// mid-triage and may be taken again.
pub const TRIAGE_STALE_AFTER_MS: i64 = 30 * 60 * 1000;

/// Envelope-ID prefix of history records written by the catch-up.
pub const CATCH_UP_ENVELOPE_PREFIX: &str = "catchup:";

/// What an event means for subscribed-conversation ingestion.
#[derive(Debug, Clone)]
pub enum SubscribedEvent {
    Message {
        team: String,
        channel: String,
        message: SlackMessage,
    },
    Edited {
        team: String,
        channel: String,
        ts: String,
        /// `message.edited.ts`; `None` for changes that are not edits (link
        /// unfurls), which are ignored.
        edit_ts: Option<String>,
        text: String,
    },
    Deleted {
        team: String,
        channel: String,
        ts: String,
    },
    Renamed {
        team: String,
        channel: String,
        name: String,
    },
}

/// Receives every event the owner gate did not turn into an agent turn.
/// Must return quickly: it runs on the inbox dispatcher.
#[async_trait]
pub trait SubscribedEventSink: Send + Sync {
    async fn observe(&self, envelope: &EventEnvelope);
}

/// `catch_up` for records the catch-up wrote, `live` otherwise.
pub fn source_of(envelope: &EventEnvelope) -> SlackIngestSource {
    if envelope.envelope_id.starts_with(CATCH_UP_ENVELOPE_PREFIX) {
        SlackIngestSource::CatchUp
    } else {
        SlackIngestSource::Live
    }
}

fn non_empty(v: Option<&str>) -> Option<String> {
    v.filter(|s| !s.trim().is_empty()).map(str::to_string)
}

/// What `envelope` means for subscribed-conversation ingestion, if
/// anything. `app_mention` is not a message of its own (the same message
/// also arrives as `message`).
pub fn subscribed_event(envelope: &EventEnvelope) -> Option<SubscribedEvent> {
    let team = non_empty(
        envelope
            .events_api
            .as_ref()
            .and_then(|m| m.team_id.as_deref()),
    )?;
    match &envelope.event {
        SlackEvent::Message(m) | SlackEvent::ThreadReply(m) => {
            if m.channel.is_empty() || m.ts.is_empty() {
                return None;
            }
            let mut subtype = m.subtype.clone();
            if subtype.is_none() && m.raw.get("hidden").and_then(Value::as_bool) == Some(true) {
                subtype = Some("hidden".into());
            }
            Some(SubscribedEvent::Message {
                team,
                channel: m.channel.clone(),
                message: SlackMessage {
                    message_type: "message".into(),
                    subtype,
                    ts: m.ts.clone(),
                    user: m.user.clone(),
                    username: None,
                    text: m.text.clone(),
                    thread_ts: m.thread_ts.clone(),
                    bot_id: m.bot_id.clone().or_else(|| {
                        m.raw
                            .get("app_id")
                            .and_then(Value::as_str)
                            .map(|a| format!("app:{a}"))
                    }),
                    edited: None,
                },
            })
        }
        SlackEvent::MessageEdited(x) if !x.channel.is_empty() && !x.ts.is_empty() => {
            Some(SubscribedEvent::Edited {
                team,
                channel: x.channel.clone(),
                ts: x.ts.clone(),
                edit_ts: non_empty(x.raw.pointer("/message/edited/ts").and_then(Value::as_str)),
                text: x.text.clone().unwrap_or_default(),
            })
        }
        SlackEvent::MessageDeleted(d) if !d.channel.is_empty() && !d.deleted_ts.is_empty() => {
            Some(SubscribedEvent::Deleted {
                team,
                channel: d.channel.clone(),
                ts: d.deleted_ts.clone(),
            })
        }
        // `channel_rename` / `group_rename` (Events API reference, read
        // 2026-09-29): `{"channel": {"id", "name", "created"}}`.
        SlackEvent::Unknown { kind, raw } if kind == "channel_rename" || kind == "group_rename" => {
            let channel = non_empty(raw.pointer("/channel/id").and_then(Value::as_str))?;
            let name = non_empty(raw.pointer("/channel/name").and_then(Value::as_str))?;
            Some(SubscribedEvent::Renamed {
                team,
                channel,
                name: format!("#{}", name.trim_start_matches('#')),
            })
        }
        _ => None,
    }
}

/// The active subscription for `channel` in `team` (a legacy row with no
/// workspace matches any team).
pub fn find_subscription(
    store: &Store,
    team: &str,
    channel: &str,
) -> anyhow::Result<Option<ChannelSubscription>> {
    let subs = store.list_active_subscriptions(crate::PLATFORM)?;
    let mut matching = subs.into_iter().filter(|s| s.channel_id == channel);
    let all: Vec<_> = matching.by_ref().collect();
    Ok(all
        .iter()
        .find(|s| s.account_id.as_deref() == Some(team))
        .or_else(|| all.iter().find(|s| s.account_id.is_none()))
        .cloned())
}

/// Slack users whose own messages are never ingested in `team`: the bound
/// owner and the Composio user.
fn own_user_ids(store: &Store, team: &str) -> Vec<String> {
    let mut ids = Vec::new();
    if let Ok(Some(binding)) = crate::owner_setup::find_binding(store, team) {
        ids.push(binding.owner.sender_id().to_string());
    }
    if let Ok(Some(ws)) = store.get_slack_workspace_by_team(team) {
        ids.push(ws.user_id);
    }
    ids
}

/// A stored priority message waiting for its (claimed) triage.
#[derive(Debug, Clone)]
pub struct TriageJob {
    pub email: Box<Email>,
}

#[derive(Debug)]
pub enum LiveIngestOutcome {
    /// Not a subscribed conversation, or nothing to ingest.
    NotSubscribed,
    /// Subscribed, but not a message that is stored (own, bot, empty…).
    Skipped(&'static str),
    Recorded {
        outcome: SlackRecordOutcome,
        triage: Option<TriageJob>,
    },
    Edited(SlackEditOutcome),
    Deleted(SlackDeleteOutcome),
    Renamed(usize),
}

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The live half of ingestion, over the same [`SlackChannel`] the poll runs.
pub struct LiveIngest<R: Reasoner + 'static> {
    channel: Arc<SlackChannel<R>>,
    clock: Clock,
}

impl<R: Reasoner + 'static> LiveIngest<R> {
    pub fn new(channel: Arc<SlackChannel<R>>) -> Self {
        Self {
            channel,
            clock: Arc::new(crate::ingest::system_now_ms),
        }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Record (or edit/delete/rename) what `envelope` carries. A returned
    /// [`TriageJob`] holds the claimed triage; run it with
    /// [`Self::run_triage`].
    pub async fn handle(&self, envelope: &EventEnvelope) -> anyhow::Result<LiveIngestOutcome> {
        let Some(event) = subscribed_event(envelope) else {
            return Ok(LiveIngestOutcome::NotSubscribed);
        };
        let store = &self.channel.store;
        let now = (self.clock)();
        match event {
            SubscribedEvent::Message {
                team,
                channel,
                message,
            } => {
                let Some(sub) = find_subscription(store, &team, &channel)? else {
                    return Ok(LiveIngestOutcome::NotSubscribed);
                };
                let own = own_user_ids(store, &team);
                if message
                    .user
                    .as_deref()
                    .is_some_and(|u| own.iter().any(|o| o == u))
                {
                    return Ok(LiveIngestOutcome::Skipped("own_message"));
                }
                if !message.is_default_user_message() {
                    return Ok(LiveIngestOutcome::Skipped("not_a_person_message"));
                }
                if message.text.trim().is_empty() {
                    return Ok(LiveIngestOutcome::Skipped("empty"));
                }
                let my_user_id = own.last().cloned().unwrap_or_default();
                let mut outcome = PollOutcome::default();
                let (recorded, job) = self.channel.record_message(
                    &sub,
                    &team,
                    &my_user_id,
                    &message,
                    source_of(envelope),
                    &mut outcome,
                )?;
                self.advance_cursor(envelope, &team, &channel, &message, now);
                debug!(message_id = %format!("{channel}:{}", message.ts), ?recorded, "slack live ingest");
                Ok(LiveIngestOutcome::Recorded {
                    outcome: recorded,
                    triage: job.map(|email| TriageJob {
                        email: Box::new(email),
                    }),
                })
            }
            SubscribedEvent::Edited {
                team,
                channel,
                ts,
                edit_ts,
                text,
            } => {
                if find_subscription(store, &team, &channel)?.is_none() {
                    return Ok(LiveIngestOutcome::NotSubscribed);
                }
                let Some(edit_ts) = edit_ts else {
                    return Ok(LiveIngestOutcome::Skipped("not_an_edit"));
                };
                let key = SlackMessageKey::new(&team, &channel, &ts)?;
                Ok(LiveIngestOutcome::Edited(
                    store.apply_slack_edit(&key, &edit_ts, &text, now)?,
                ))
            }
            SubscribedEvent::Deleted { team, channel, ts } => {
                if find_subscription(store, &team, &channel)?.is_none() {
                    return Ok(LiveIngestOutcome::NotSubscribed);
                }
                let key = SlackMessageKey::new(&team, &channel, &ts)?;
                Ok(LiveIngestOutcome::Deleted(
                    store.apply_slack_delete(&key, now)?,
                ))
            }
            SubscribedEvent::Renamed {
                team,
                channel,
                name,
            } => Ok(LiveIngestOutcome::Renamed(
                store.rename_slack_conversation(&team, &channel, &name, now)?,
            )),
        }
    }

    /// Top-level messages move the conversation's catch-up cursor, so the
    /// catch-up after a sleep starts where live delivery stopped.
    fn advance_cursor(
        &self,
        envelope: &EventEnvelope,
        team: &str,
        channel: &str,
        message: &SlackMessage,
        now: i64,
    ) {
        if message
            .thread_ts
            .as_deref()
            .is_some_and(|t| t != message.ts)
        {
            return;
        }
        let Some(seen_ms) = augmentagent_store::slack_ingest::ts_to_ms(&message.ts) else {
            return;
        };
        let enterprise = envelope
            .payload
            .get("enterprise_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let conversation = crate::surface::SlackWorkspace::new(team, enterprise)
            .and_then(|w| w.conversation(channel, None));
        match conversation {
            Ok(conversation) => {
                if let Err(e) = self.channel.store.advance_surface_cursor(
                    &conversation,
                    &message.ts,
                    seen_ms,
                    now,
                ) {
                    warn!(channel, "slack live ingest: cursor not advanced: {e}");
                }
            }
            Err(e) => debug!(
                channel,
                "slack live ingest: no cursor for this conversation: {e}"
            ),
        }
    }

    /// Run a claimed triage (the decision `observe` spawns).
    pub async fn run_triage(&self, job: TriageJob) -> PollOutcome {
        run_claimed(&self.channel, job).await
    }
}

async fn run_claimed<R: Reasoner + 'static>(
    channel: &SlackChannel<R>,
    job: TriageJob,
) -> PollOutcome {
    let mut outcome = PollOutcome::default();
    let message_id = job.email.message_id.clone();
    if let Err(e) = channel.triage_claimed(*job.email, &mut outcome).await {
        outcome.errors += 1;
        warn!(message_id = %message_id, "slack live triage failed: {e:#}");
    }
    outcome
}

#[async_trait]
impl<R: Reasoner + 'static> SubscribedEventSink for LiveIngest<R> {
    async fn observe(&self, envelope: &EventEnvelope) {
        match self.handle(envelope).await {
            Ok(LiveIngestOutcome::Recorded {
                triage: Some(job), ..
            }) => {
                let channel = Arc::clone(&self.channel);
                tokio::spawn(async move {
                    let out = run_claimed(&channel, job).await;
                    debug!(?out, "slack live triage complete");
                });
            }
            Ok(other) => debug!(outcome = ?other, "slack live ingest"),
            Err(e) => warn!("slack live ingest failed: {e:#}"),
        }
    }
}

pub(crate) fn system_now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
