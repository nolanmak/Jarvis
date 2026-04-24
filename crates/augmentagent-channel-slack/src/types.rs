//! Slack response types returned by Composio's SLACK_* tool executes.
//!
//! Composio wraps every Slack API response in `data.response_data` under
//! the top-level execute response. We deserialize the inner Slack payloads
//! here, accepting unknown fields so schema drift doesn't break us.

use serde::Deserialize;

/// Conversation (channel/DM/MPIM/group) listing entry.
///
/// Slack's `conversations.list` response item shape — only the fields the
/// channel poller cares about.
#[derive(Debug, Clone, Deserialize)]
pub struct Conversation {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// `true` for 1:1 direct messages.
    #[serde(default)]
    pub is_im: bool,
    /// `true` for multi-party IM (group DM).
    #[serde(default)]
    pub is_mpim: bool,
    /// `true` for private channels.
    #[serde(default)]
    pub is_private: bool,
    /// `true` for public channels.
    #[serde(default)]
    pub is_channel: bool,
    /// Other party's user id for 1:1 DMs (Slack calls this "user" on im).
    #[serde(default)]
    pub user: Option<String>,
}

impl Conversation {
    /// Human-friendly display label for dashboard pickers.
    pub fn display_name(&self) -> String {
        if self.is_im {
            format!("DM with {}", self.user.clone().unwrap_or_else(|| "user".into()))
        } else if self.is_mpim {
            format!("group DM {}", self.id)
        } else if !self.name.is_empty() {
            format!("#{}", self.name)
        } else {
            self.id.clone()
        }
    }
}

/// A Slack message from `conversations.history`.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackMessage {
    /// Message type — `"message"` for normal messages; bot-edit, channel_join,
    /// etc. use different subtypes we skip.
    #[serde(default, rename = "type")]
    pub message_type: String,
    /// Presence means this is a non-user system message; skip these.
    #[serde(default)]
    pub subtype: Option<String>,
    /// Slack timestamp (e.g., `"1234567890.123456"`). Used as the message id
    /// AND as the polling-cursor (`oldest=...`).
    pub ts: String,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// Slack marks app/bot messages with a bot_id.
    #[serde(default)]
    pub bot_id: Option<String>,
}

impl SlackMessage {
    /// Standard user message (not channel_join, not pinned, not a bot event).
    pub fn is_default_user_message(&self) -> bool {
        self.subtype.is_none() && self.bot_id.is_none() && self.message_type == "message"
    }
}

/// Result of a user lookup via `users.info` — mostly used to resolve DM
/// recipient display names.
#[derive(Debug, Clone, Deserialize)]
pub struct SlackUser {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub real_name: String,
    #[serde(default)]
    pub is_bot: bool,
}

impl SlackUser {
    pub fn display_label(&self) -> String {
        if !self.real_name.is_empty() {
            self.real_name.clone()
        } else {
            self.name.clone()
        }
    }
}
