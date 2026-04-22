//! Discord REST response types. Partial — we deserialize only fields the
//! channel pipeline needs; `#[serde(default)]` + `allow unknown fields` lets
//! Discord add keys without breaking us.

use serde::Deserialize;

/// A DM or group-DM channel (`GET /users/@me/channels`).
#[derive(Debug, Clone, Deserialize)]
pub struct DmChannel {
    pub id: String,
    /// `1` = 1:1 DM, `3` = group DM.
    #[serde(rename = "type")]
    pub channel_type: u8,
    #[serde(default)]
    pub recipients: Vec<User>,
    #[serde(default)]
    pub last_message_id: Option<String>,
}

impl DmChannel {
    /// Human-friendly label — recipients joined by `, ` (or "group DM" for
    /// unnamed type-3 channels with no recipients in the payload).
    pub fn display_name(&self) -> String {
        if self.recipients.is_empty() {
            return format!("channel {}", self.id);
        }
        self.recipients
            .iter()
            .map(|r| r.display_label())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn is_one_to_one(&self) -> bool {
        self.channel_type == 1
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Guild {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuildChannel {
    pub id: String,
    pub name: String,
    /// `0` = text channel (only kind we read from).
    #[serde(rename = "type")]
    pub channel_type: u8,
    #[serde(default)]
    pub parent_id: Option<String>,
}

impl GuildChannel {
    pub fn is_text(&self) -> bool {
        self.channel_type == 0
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(default)]
    pub global_name: Option<String>,
    #[serde(default)]
    pub bot: bool,
}

impl User {
    pub fn display_label(&self) -> String {
        self.global_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| self.username.clone())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub id: String,
    pub channel_id: String,
    pub author: User,
    pub content: String,
    pub timestamp: String,
    #[serde(default)]
    pub edited_timestamp: Option<String>,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Discord message type. `0` = default user message; other types (system
    /// messages like "user pinned a message", "user joined") we skip.
    #[serde(rename = "type", default)]
    pub message_type: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub url: String,
    pub size: u64,
}

impl Message {
    /// `true` iff this is a normal user-sent text message (not a system event).
    pub fn is_default_user_message(&self) -> bool {
        self.message_type == 0 && !self.author.bot
    }
}
