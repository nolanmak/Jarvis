//! WhatsApp-native types + JID parsing + conversion into the shared
//! `augmentagent_store::Email`.
//!
//! The store + broker + wiki pipeline are channel-agnostic — they consume
//! `Email` regardless of source. We repurpose the fields:
//!
//! - `message_id` ← `wa:<chat_jid>:<message_id>`
//! - `thread_id`  ← `<chat_jid>` (so the approver can route the send back)
//! - `from`       ← "<Push Name> <whatsapp:<sender_jid>>"
//! - `subject`    ← "" (WhatsApp has no subject; the card title is derived)
//! - `body`       ← message text (conversation / extendedTextMessage)
//! - `date`       ← RFC3339 from the message unix timestamp
//! - `account_entity_id` ← "whatsapp:device:<phone>"
//!
//! The `whatsapp:device:` prefix on `account_entity_id` is how the approver
//! in `augmentagent-cli` knows to route send requests back through the
//! whatsmeow sidecar for the right linked device.

use serde::{Deserialize, Serialize};

use augmentagent_store::Email;

use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// A WhatsApp JID (Jabber ID). Personal contacts are `<number>@s.whatsapp.net`,
/// groups are `<id>@g.us`, broadcast lists `<id>@broadcast`. We only triage
/// 1:1 (`s.whatsapp.net`) chats in v1; groups are surfaced so the channel can
/// drop them explicitly.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct Jid(pub String);

impl Jid {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// The user/number part before the `@`. For `15551234567@s.whatsapp.net`
    /// this is `15551234567`. JIDs with a device suffix (`:12@...`) get the
    /// device stripped so the bare user is stable across linked devices.
    pub fn user(&self) -> &str {
        let at = self.0.find('@').unwrap_or(self.0.len());
        let user = &self.0[..at];
        match user.find(':') {
            Some(colon) => &user[..colon],
            None => user,
        }
    }

    /// The server part after the `@` (`s.whatsapp.net`, `g.us`, `broadcast`).
    pub fn server(&self) -> &str {
        match self.0.find('@') {
            Some(at) => &self.0[at + 1..],
            None => "",
        }
    }

    /// True for a 1:1 personal chat (`<number>@s.whatsapp.net`). Groups and
    /// broadcast lists are excluded — v1 doesn't draft for those.
    pub fn is_personal(&self) -> bool {
        self.server() == "s.whatsapp.net"
    }

    pub fn is_group(&self) -> bool {
        self.server() == "g.us"
    }

    /// Normalized bare JID with any device suffix stripped
    /// (`15551234567:3@s.whatsapp.net` → `15551234567@s.whatsapp.net`). Used
    /// as the stable dedup / allowlist key.
    pub fn bare(&self) -> String {
        format!("{}@{}", self.user(), self.server())
    }
}

/// A single inbound WhatsApp message decoded from a sidecar `received-message`
/// event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaMessage {
    /// whatsmeow message id (server-assigned, stable).
    pub id: String,
    /// The chat the message belongs to. For a 1:1 this equals `sender` unless
    /// it's our own outbound (then chat = peer, sender = us).
    pub chat: Jid,
    /// Who actually authored the message.
    pub sender: Jid,
    /// Display / push name of the sender, when whatsmeow surfaced one.
    #[serde(default)]
    pub push_name: String,
    /// Decoded text body (conversation or extendedTextMessage). Empty for
    /// media-only messages — those are dropped before triage.
    #[serde(default)]
    pub text: String,
    /// Message unix timestamp (seconds).
    pub timestamp: i64,
    /// True if whatsmeow reports the message as sent by us (our linked
    /// device). These are filtered before triage.
    #[serde(default)]
    pub from_me: bool,
}

impl WaMessage {
    /// True if this is our own outbound message — never triaged.
    pub fn is_outbound(&self) -> bool {
        self.from_me
    }

    /// Convert into the store's generic `Email`. `phone` is the linked
    /// device's own number; it's stored in `account_entity_id` under a
    /// `whatsapp:device:` prefix so the approver routes sends back through
    /// the right sidecar device.
    pub fn into_email(self, phone: &str) -> Email {
        let display = if self.push_name.trim().is_empty() {
            self.chat.user().to_string()
        } else {
            self.push_name.clone()
        };
        let from = format!("{} <whatsapp:{}>", display, self.sender.bare());
        Email {
            message_id: format!("wa:{}:{}", self.chat.bare(), self.id),
            thread_id: Some(self.chat.bare()),
            from,
            subject: String::new(),
            body: self.text,
            date: secs_to_rfc3339(self.timestamp),
            account_entity_id: Some(format!("{ACCOUNT_ENTITY_ID_PREFIX}:device:{phone}")),
            platform: PLATFORM.to_string(),
            kind: "dm".into(),
        }
    }
}

/// A WhatsApp contact / chat summary returned by `list_chats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaContact {
    pub jid: Jid,
    #[serde(default)]
    pub name: String,
    /// Unix seconds of the last message in the chat, when known.
    #[serde(default)]
    pub last_message_at: i64,
}

/// An event pushed by the sidecar over the UDS event stream. `received-message`
/// is the only kind the channel acts on today; `pair-success` /
/// `connected` / `logged-out` are lifecycle signals surfaced for logging and
/// the `whatsapp login` flow.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum WaEvent {
    /// QR code string to render during pairing (`whatsapp login`).
    Qr { code: String },
    /// Device paired successfully; carries the linked device + user JIDs.
    PairSuccess {
        device_jid: String,
        user_jid: String,
    },
    /// Socket connected / authenticated.
    Connected,
    /// Server logged the device out — creds are dead, re-pair required.
    LoggedOut { reason: String },
    /// A new inbound (or our own outbound) message.
    ReceivedMessage {
        #[serde(flatten)]
        message: WaMessage,
    },
}

/// `account_entity_id` prefix that distinguishes WhatsApp-sourced rows.
pub const ACCOUNT_PREFIX: &str = "whatsapp:device:";

/// Parse the linked-device phone back out of `Email::account_entity_id`
/// (`whatsapp:device:<phone>`). Used by the approver to pick the right
/// sidecar at send time.
pub fn extract_device_phone(account_entity_id: &str) -> Option<String> {
    account_entity_id
        .strip_prefix(ACCOUNT_PREFIX)
        .map(str::to_string)
}

/// Parse the WhatsApp sender JID out of the `from` field shape
/// `"<display> <whatsapp:<jid>>"`.
pub fn extract_sender_jid(from: &str) -> Option<String> {
    let start = from.rfind("<whatsapp:")? + "<whatsapp:".len();
    let end = from[start..].find('>')?;
    Some(from[start..start + end].to_string())
}

/// True iff the email row came from this channel.
pub fn is_whatsapp_email(email: &Email) -> bool {
    email
        .account_entity_id
        .as_deref()
        .is_some_and(|a| a.starts_with(ACCOUNT_PREFIX))
}

fn secs_to_rfc3339(secs: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jid_parses_user_and_server() {
        let j = Jid::new("15551234567@s.whatsapp.net");
        assert_eq!(j.user(), "15551234567");
        assert_eq!(j.server(), "s.whatsapp.net");
        assert!(j.is_personal());
        assert!(!j.is_group());
    }

    #[test]
    fn jid_strips_device_suffix() {
        let j = Jid::new("15551234567:12@s.whatsapp.net");
        assert_eq!(j.user(), "15551234567");
        assert_eq!(j.bare(), "15551234567@s.whatsapp.net");
    }

    #[test]
    fn jid_detects_group() {
        let j = Jid::new("120363001234567890@g.us");
        assert!(j.is_group());
        assert!(!j.is_personal());
    }

    #[test]
    fn message_roundtrips_to_email() {
        let m = WaMessage {
            id: "3EB0ABCDEF".into(),
            chat: Jid::new("15551234567@s.whatsapp.net"),
            sender: Jid::new("15551234567@s.whatsapp.net"),
            push_name: "Tony Siu".into(),
            text: "hey, free thursday?".into(),
            timestamp: 1776630000,
            from_me: false,
        };
        let email = m.into_email("15559998888");
        assert_eq!(email.message_id, "wa:15551234567@s.whatsapp.net:3EB0ABCDEF");
        assert_eq!(
            email.thread_id.as_deref(),
            Some("15551234567@s.whatsapp.net")
        );
        assert!(email.from.contains("Tony Siu"));
        assert!(email.from.contains("whatsapp:15551234567@s.whatsapp.net"));
        assert_eq!(email.body, "hey, free thursday?");
        assert_eq!(
            email.account_entity_id.as_deref(),
            Some("whatsapp:device:15559998888")
        );
        assert_eq!(email.platform, "whatsapp");
        assert!(is_whatsapp_email(&email));
    }

    #[test]
    fn message_uses_jid_user_when_no_push_name() {
        let m = WaMessage {
            id: "m1".into(),
            chat: Jid::new("15551234567@s.whatsapp.net"),
            sender: Jid::new("15551234567@s.whatsapp.net"),
            push_name: "".into(),
            text: "hi".into(),
            timestamp: 0,
            from_me: false,
        };
        let email = m.into_email("15559998888");
        assert!(email.from.starts_with("15551234567 <whatsapp:"));
    }

    #[test]
    fn extract_helpers_roundtrip() {
        assert_eq!(
            extract_device_phone("whatsapp:device:15559998888").as_deref(),
            Some("15559998888")
        );
        assert_eq!(extract_device_phone("slack:team:T1"), None);
        assert_eq!(
            extract_sender_jid("Tony Siu <whatsapp:15551234567@s.whatsapp.net>").as_deref(),
            Some("15551234567@s.whatsapp.net")
        );
        assert_eq!(extract_sender_jid("no tag here"), None);
    }

    #[test]
    fn is_whatsapp_email_rejects_other_platforms() {
        let email = Email {
            message_id: "m".into(),
            thread_id: None,
            from: "a@b.com".into(),
            subject: "s".into(),
            body: "b".into(),
            date: "d".into(),
            account_entity_id: Some("composio-acc1".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        };
        assert!(!is_whatsapp_email(&email));
    }

    #[test]
    fn outbound_is_flagged() {
        let m = WaMessage {
            id: "m".into(),
            chat: Jid::new("15551234567@s.whatsapp.net"),
            sender: Jid::new("15559998888@s.whatsapp.net"),
            push_name: String::new(),
            text: "sent by me".into(),
            timestamp: 0,
            from_me: true,
        };
        assert!(m.is_outbound());
    }

    #[test]
    fn event_deserializes_received_message() {
        let raw = serde_json::json!({
            "event": "received-message",
            "id": "X1",
            "chat": "15551234567@s.whatsapp.net",
            "sender": "15551234567@s.whatsapp.net",
            "push_name": "Tony",
            "text": "yo",
            "timestamp": 1776630000,
            "from_me": false
        });
        let ev: WaEvent = serde_json::from_value(raw).unwrap();
        match ev {
            WaEvent::ReceivedMessage { message } => {
                assert_eq!(message.id, "X1");
                assert_eq!(message.text, "yo");
                assert!(message.chat.is_personal());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn event_deserializes_qr_and_pair() {
        let qr: WaEvent =
            serde_json::from_value(serde_json::json!({"event":"qr","code":"2@abc"})).unwrap();
        assert!(matches!(qr, WaEvent::Qr { code } if code == "2@abc"));
        let pair: WaEvent = serde_json::from_value(serde_json::json!({
            "event": "pair-success",
            "device_jid": "15559998888:5@s.whatsapp.net",
            "user_jid": "15559998888@s.whatsapp.net"
        }))
        .unwrap();
        assert!(matches!(pair, WaEvent::PairSuccess { .. }));
    }
}
