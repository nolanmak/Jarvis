//! #1296 — Slack conversation history for the agent harness.
//!
//! A provider without a native session (Qwen, GLM and the other
//! history-in-prompt profiles, see `surface_turn`) only keeps multi-turn
//! context if the transport hands it the earlier transcript. Discord builds
//! one from the channel (`fetch_conversation_context`); this is the Slack
//! equivalent, fetched with the app's bot token:
//!
//! * **Thread-aware.** A turn in a thread reads that thread
//!   (`conversations.replies`); a top-level DM turn reads the DM
//!   (`conversations.history`).
//! * **Bounded.** At most [`HistoryLimits::max_messages`] messages, none
//!   older than [`HistoryLimits::max_age_ms`], at most
//!   [`HistoryLimits::char_cap`] characters (the oldest are dropped first),
//!   one Web API call per turn, and only when the conversation has no
//!   native session yet (a native session already has its history).
//! * **Owner and agent only.** The owner's messages are `user:`, the app's
//!   own are `assistant:`; anyone else's (a channel member, another bot) is
//!   left out, like Discord's fail-closed rule, and nothing after the
//!   current message is included.
//! * **Best effort.** A failed fetch gives an empty history, never a failed
//!   turn.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use augmentagent_store::slack_ingest::{ts_order, ts_to_ms};
use serde_json::Value;
use tracing::warn;

use crate::ingest::Clock;
use crate::interactive::{SlackTurn, SlackWorkspaceRuntime};
use crate::owner::SlackBotIdentity;
use crate::surface::SlackWorkspace;
use crate::transport::event::SlackEvent;
use crate::transport::web::{HistoryQuery, SlackHistoryMessage, SlackWebApi};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimits {
    pub max_messages: u32,
    pub max_age_ms: i64,
    pub char_cap: usize,
}

impl Default for HistoryLimits {
    /// Discord's values: 30 messages, 2 hours, 10 000 characters.
    fn default() -> Self {
        Self {
            max_messages: 30,
            max_age_ms: 2 * 60 * 60 * 1000,
            char_cap: 10_000,
        }
    }
}

/// Earlier transcript for a turn; empty when there is none.
#[async_trait]
pub trait TurnHistory: Send + Sync {
    async fn history(&self, turn: &SlackTurn) -> String;
}

/// [`TurnHistory`] over each installed workspace's Web API.
pub struct SlackConversationHistory {
    workspaces: HashMap<String, (Arc<dyn SlackWebApi>, SlackBotIdentity)>,
    limits: HistoryLimits,
    clock: Clock,
}

impl SlackConversationHistory {
    pub fn new(workspaces: &[SlackWorkspaceRuntime]) -> Self {
        Self {
            workspaces: workspaces
                .iter()
                .map(|w| {
                    (
                        w.workspace.team_id().to_string(),
                        (Arc::clone(&w.web), w.bot.clone()),
                    )
                })
                .collect(),
            limits: HistoryLimits::default(),
            clock: Arc::new(crate::ingest::system_now_ms),
        }
    }

    pub fn with_limits(mut self, limits: HistoryLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The transcript before `before_ts` in `channel` (or its thread under
    /// `thread_ts`), as `<conversation_history>` text.
    pub async fn transcript(
        &self,
        team_id: &str,
        channel: &str,
        thread_ts: Option<&str>,
        before_ts: Option<&str>,
        owner_user_id: &str,
    ) -> String {
        let Some((web, bot)) = self.workspaces.get(team_id) else {
            return String::new();
        };
        let limit = self.limits.max_messages.max(1);
        let page = match thread_ts {
            // Replies come oldest first; one bounded page.
            Some(ts) => {
                web.conversations_replies(HistoryQuery {
                    channel: channel.to_string(),
                    thread_ts: Some(ts.to_string()),
                    oldest: None,
                    limit: 200,
                    cursor: None,
                    include_all_metadata: false,
                })
                .await
            }
            // History comes newest first: the current message plus the
            // `limit` before it.
            None => {
                web.conversations_history(HistoryQuery {
                    channel: channel.to_string(),
                    thread_ts: None,
                    oldest: None,
                    limit: limit + 1,
                    cursor: None,
                    include_all_metadata: false,
                })
                .await
            }
        };
        let messages = match page {
            Ok(page) => page.messages,
            Err(e) => {
                warn!(channel, "slack history for the agent unavailable: {e}");
                return String::new();
            }
        };
        let floor = (self.clock)().saturating_sub(self.limits.max_age_ms);
        let before = before_ts.and_then(ts_order);
        let mut turns: Vec<(i64, &'static str, String)> = messages
            .iter()
            .filter_map(|m| {
                let order = ts_order(&m.ts)?;
                if before.is_some_and(|b| order >= b) {
                    return None;
                }
                let at = ts_to_ms(&m.ts)?;
                if at < floor {
                    return None;
                }
                let role = role_of(m, bot, owner_user_id)?;
                let text = m.text.as_deref().unwrap_or("").trim().to_string();
                (!text.is_empty()).then_some((at, role, text))
            })
            .collect();
        turns.sort_by_key(|t| t.0);
        let skip = turns.len().saturating_sub(limit as usize);
        let kept: Vec<(&str, String)> = turns
            .into_iter()
            .skip(skip)
            .map(|(_, role, text)| (role, text))
            .collect();
        format_transcript(&kept, self.limits.char_cap)
    }
}

/// `assistant` for this app's own messages, `user` for the owner's, `None`
/// for everyone else and for system messages.
fn role_of(m: &SlackHistoryMessage, bot: &SlackBotIdentity, owner: &str) -> Option<&'static str> {
    let app_id = m.raw.get("app_id").and_then(Value::as_str);
    let own = (m.bot_id.is_some() && m.bot_id == bot.bot_id)
        || (m.user.is_some() && m.user == bot.bot_user_id)
        || (app_id.is_some() && app_id == bot.app_id.as_deref());
    if own {
        return Some("assistant");
    }
    let subtype = m.raw.get("subtype").and_then(Value::as_str);
    if m.bot_id.is_some() || subtype.is_some_and(|s| s != "thread_broadcast" && s != "file_share") {
        return None;
    }
    (m.user.as_deref() == Some(owner)).then_some("user")
}

/// Role-tagged transcript, dropping the oldest entries over `char_cap`.
pub fn format_transcript(turns: &[(&str, String)], char_cap: usize) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut total = 0usize;
    for (role, body) in turns.iter().rev() {
        let entry = format!("{role}: {body}\n");
        if total + entry.len() > char_cap {
            break;
        }
        total += entry.len();
        kept.push(entry);
    }
    if kept.is_empty() {
        return String::new();
    }
    kept.reverse();
    let mut out = String::from("<conversation_history>\n");
    for entry in kept {
        out.push_str(&entry);
    }
    out.push_str("</conversation_history>");
    out
}

#[async_trait]
impl TurnHistory for SlackConversationHistory {
    async fn history(&self, turn: &SlackTurn) -> String {
        let Some(session) = &turn.session else {
            return String::new();
        };
        let Ok(workspace) = SlackWorkspace::from_account(session.account()) else {
            return String::new();
        };
        let current_ts = match &turn.envelope.event {
            SlackEvent::Message(m) | SlackEvent::ThreadReply(m) | SlackEvent::AppMention(m) => {
                Some(m.ts.as_str())
            }
            _ => None,
        };
        self.transcript(
            workspace.team_id(),
            session.conversation_id(),
            session.thread_id(),
            current_ts,
            turn.owner.sender_id(),
        )
        .await
    }
}
