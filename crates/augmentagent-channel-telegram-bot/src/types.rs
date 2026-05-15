//! Wire shapes for the Telegram Bot API responses we consume.
//!
//! Reference: <https://core.telegram.org/bots/api>
//!
//! Only the fields the channel actually uses today are decoded explicitly;
//! everything else is left out and `serde(default)` shields us from schema
//! drift on the optional bits. Voice / File land here in stub form so the
//! voice-memo follow-up (#66) can flesh them out without touching anything
//! else.

use serde::{Deserialize, Serialize};

/// `getMe` response payload (`result` field of the envelope).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Me {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub first_name: String,
    /// Bot username without the leading `@`. Required field on bots per
    /// the API spec, but kept optional in deserialization to avoid hard
    /// failures on partial responses.
    #[serde(default)]
    pub username: String,
}

/// One update from `getUpdates` — the polling-loop primary type.
///
/// We only model `message` updates today; `edited_message`, `channel_post`,
/// `callback_query`, etc. all parse with their respective fields set to
/// `None` and get filtered at the channel layer.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub edited_message: Option<Message>,
    #[serde(default)]
    pub channel_post: Option<Message>,
}

/// A Telegram `Message` object. We carry only the slice the channel reads;
/// extra fields like `entities`, `photo`, `document`, etc. are dropped on
/// deserialize.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Message {
    pub message_id: i64,
    /// Unix timestamp of when the message was sent.
    pub date: i64,
    pub chat: Chat,
    /// Sender. Optional in the API (channel posts have no `from`); we only
    /// triage messages with a `from` set.
    #[serde(default)]
    pub from: Option<User>,
    #[serde(default)]
    pub text: Option<String>,
    /// Caption used when the user sends media + a text caption together.
    /// We surface it as text on best-effort.
    #[serde(default)]
    pub caption: Option<String>,
    /// Voice memo metadata — not handled by this PR (#66 follow-up), but
    /// surfaced so the channel can decide whether to skip or queue.
    #[serde(default)]
    pub voice: Option<Voice>,
    /// `message_id` of the message this is a reply to (Telegram's quoted
    /// reply primitive).
    #[serde(default)]
    pub reply_to_message_id: Option<i64>,
    /// Sometimes the API sends the full original message rather than just
    /// `reply_to_message_id`; we accept both shapes but only the id is used.
    #[serde(default)]
    pub reply_to_message: Option<Box<Message>>,
}

impl Message {
    /// Single source of truth for "what's the user-visible body of this
    /// message?". Falls back to caption when the user posted media + caption.
    pub fn body_text(&self) -> &str {
        self.text
            .as_deref()
            .or(self.caption.as_deref())
            .unwrap_or("")
    }

    /// Effective `reply_to_message_id`, preferring the explicit field but
    /// falling back to the embedded full message's id when present.
    pub fn effective_reply_to(&self) -> Option<i64> {
        self.reply_to_message_id
            .or_else(|| self.reply_to_message.as_deref().map(|m| m.message_id))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub id: i64,
    #[serde(default)]
    pub is_bot: bool,
    #[serde(default)]
    pub first_name: String,
    #[serde(default)]
    pub last_name: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
}

impl User {
    /// Best-effort display name: prefer `@username`, fall back to first/last,
    /// then to the numeric id.
    pub fn display_label(&self) -> String {
        if let Some(u) = self.username.as_deref().filter(|s| !s.is_empty()) {
            return format!("@{u}");
        }
        let mut name = self.first_name.clone();
        if let Some(last) = self.last_name.as_deref().filter(|s| !s.is_empty()) {
            if !name.is_empty() {
                name.push(' ');
            }
            name.push_str(last);
        }
        if name.is_empty() {
            self.id.to_string()
        } else {
            name
        }
    }
}

/// `Chat` — `private` is the DM case, `group`/`supergroup`/`channel` are the
/// rest. The channel today only acts on `private` and explicit-subscription
/// `group`/`supergroup`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chat {
    pub id: i64,
    /// One of `private`, `group`, `supergroup`, `channel`.
    #[serde(default, rename = "type")]
    pub chat_type: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub last_name: Option<String>,
}

impl Chat {
    pub fn is_private(&self) -> bool {
        self.chat_type == "private"
    }

    pub fn display_label(&self) -> String {
        if let Some(t) = self.title.as_deref().filter(|s| !s.is_empty()) {
            return t.to_string();
        }
        if let Some(u) = self.username.as_deref().filter(|s| !s.is_empty()) {
            return format!("@{u}");
        }
        let mut name = self.first_name.clone().unwrap_or_default();
        if let Some(last) = self.last_name.as_deref().filter(|s| !s.is_empty()) {
            if !name.is_empty() {
                name.push(' ');
            }
            name.push_str(last);
        }
        if name.is_empty() {
            format!("chat {}", self.id)
        } else {
            name
        }
    }
}

/// Voice memo metadata. Stub — voice transcription wiring lives in #66.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Voice {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: String,
    #[serde(default)]
    pub duration: i64,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_size: Option<i64>,
}

/// `getFile` response payload — used by #66 to download voice memos.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct File {
    pub file_id: String,
    #[serde(default)]
    pub file_unique_id: String,
    #[serde(default)]
    pub file_size: Option<i64>,
    /// Relative path under `https://api.telegram.org/file/bot<token>/<file_path>`.
    #[serde(default)]
    pub file_path: Option<String>,
}

/// `sendMessage` response payload (subset).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SentMessage {
    pub message_id: i64,
    pub date: i64,
    pub chat: Chat,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_dm_update() {
        let raw = serde_json::json!({
            "update_id": 100001,
            "message": {
                "message_id": 42,
                "date": 1747200000,
                "chat": { "id": 12345, "type": "private", "username": "alice" },
                "from": { "id": 12345, "is_bot": false, "first_name": "Alice", "username": "alice" },
                "text": "hey, got a sec?"
            }
        });
        let u: Update = serde_json::from_value(raw).unwrap();
        assert_eq!(u.update_id, 100001);
        let m = u.message.unwrap();
        assert_eq!(m.body_text(), "hey, got a sec?");
        assert!(m.chat.is_private());
        assert_eq!(m.from.unwrap().display_label(), "@alice");
    }

    #[test]
    fn body_text_falls_back_to_caption() {
        let m: Message = serde_json::from_value(serde_json::json!({
            "message_id": 1,
            "date": 0,
            "chat": { "id": 1, "type": "private" },
            "caption": "look at this"
        }))
        .unwrap();
        assert_eq!(m.body_text(), "look at this");
    }

    #[test]
    fn effective_reply_to_prefers_explicit_id() {
        let m: Message = serde_json::from_value(serde_json::json!({
            "message_id": 2,
            "date": 0,
            "chat": { "id": 1, "type": "private" },
            "reply_to_message_id": 99
        }))
        .unwrap();
        assert_eq!(m.effective_reply_to(), Some(99));
    }

    #[test]
    fn user_display_label_prefers_username() {
        let u = User {
            id: 7,
            is_bot: false,
            first_name: "Bob".into(),
            last_name: Some("Smith".into()),
            username: Some("bob_smith".into()),
        };
        assert_eq!(u.display_label(), "@bob_smith");
    }

    #[test]
    fn user_display_label_falls_back_to_full_name() {
        let u = User {
            id: 7,
            is_bot: false,
            first_name: "Bob".into(),
            last_name: Some("Smith".into()),
            username: None,
        };
        assert_eq!(u.display_label(), "Bob Smith");
    }

    #[test]
    fn chat_display_label_titled_group() {
        let c = Chat {
            id: -100,
            chat_type: "supergroup".into(),
            title: Some("Engineering".into()),
            username: None,
            first_name: None,
            last_name: None,
        };
        assert_eq!(c.display_label(), "Engineering");
        assert!(!c.is_private());
    }

    #[test]
    fn ignores_unknown_fields() {
        // Telegram regularly adds new optional fields. We must keep parsing.
        let raw = serde_json::json!({
            "update_id": 1,
            "totally_new_2026_field": "ignore me",
            "message": {
                "message_id": 5,
                "date": 1,
                "chat": { "id": 1, "type": "private", "totally_new_chat_field": 7 },
                "text": "hi"
            }
        });
        let _: Update = serde_json::from_value(raw).unwrap();
    }
}
