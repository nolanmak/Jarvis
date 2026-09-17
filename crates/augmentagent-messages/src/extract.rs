//! Per-platform extraction: one `emails` row → normalized index fields.
//!
//! Pure functions. Each channel packs sender, conversation and direction
//! into `fromEmail` / `threadId` / `subject` differently; this module is the
//! single place that knows those shapes. Unknown platforms get conservative
//! defaults instead of guesses, and nothing here can panic on odd input.

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use serde::Serialize;

use crate::handles::{self, ME};

/// Bump when extraction output changes; `enqueue_stale` re-extracts rows
/// indexed by an older version.
pub const EXTRACTOR_VERSION: i64 = 2;

/// The columns of an `emails` row the extractor reads.
#[derive(Debug, Clone, Default)]
pub struct EmailRowView {
    pub message_id: String,
    pub thread_id: Option<String>,
    pub from: String,
    pub subject: String,
    pub body: String,
    pub received_at: Option<String>,
    pub account_entity_id: Option<String>,
    pub first_seen_ms: i64,
    pub platform: String,
    pub kind: String,
}

/// Handles that identify the owner, loaded from the store per batch.
#[derive(Debug, Clone, Default)]
pub struct OwnerHandles {
    /// Canonical handles of connected mailboxes and platform accounts.
    pub handles: Vec<String>,
}

impl OwnerHandles {
    fn contains(&self, handle: &str) -> bool {
        handle == ME || self.handles.iter().any(|h| h == handle)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexFields {
    pub platform: String,
    /// `dm` | `group` | `channel` | `note` | `email` | `meeting` | `other`
    pub conv_kind: String,
    pub conversation_id: String,
    pub conversation_title: Option<String>,
    /// Server / workspace / folder, when the platform has one.
    pub container: Option<String>,
    pub sender_handle: String,
    pub sender_label: Option<String>,
    /// For 1:1 conversations, the other party when derivable from this row.
    pub counterpart_handle: Option<String>,
    pub from_me: bool,
    pub ts_ms: i64,
    /// `true` when `ts_ms` came from ingest time because `receivedAt` didn't parse.
    pub ts_fallback: bool,
    pub has_attachment: bool,
}

/// Split `"<title> [<label>]"` on the LAST bracket group, so titles that
/// themselves contain brackets survive.
fn split_label(s: &str) -> (&str, Option<&str>) {
    let t = s.trim_end();
    if t.ends_with(']') {
        if let Some(open) = t.rfind(" [") {
            let label = &t[open + 2..t.len() - 1];
            return (t[..open].trim(), Some(label));
        }
    }
    (t, None)
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// Display name of `Name <addr>`; `None` for a bare address.
fn display_name(from: &str) -> Option<String> {
    let open = from.rfind('<')?;
    non_empty(from[..open].trim().trim_matches('"'))
}

pub fn parse_ts_ms(raw: &str) -> Option<i64> {
    let r = raw.trim();
    if r.is_empty() {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(r) {
        return Some(t.timestamp_millis());
    }
    if let Ok(t) = DateTime::parse_from_rfc2822(r) {
        return Some(t.timestamp_millis());
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(r, "%Y-%m-%d %H:%M:%S") {
        return Some(t.and_utc().timestamp_millis());
    }
    if let Ok(d) = NaiveDate::parse_from_str(r, "%Y-%m-%d") {
        return d
            .and_hms_opt(12, 0, 0)
            .map(|t| t.and_utc().timestamp_millis());
    }
    r.parse::<i64>().ok().filter(|n| *n > 1_000_000_000_000)
}

fn has_attachment(body: &str) -> bool {
    body.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("[attachment:") || l.starts_with("IMAGE:")
    })
}

pub fn extract(row: &EmailRowView, owner: &OwnerHandles) -> IndexFields {
    let (ts_ms, ts_fallback) = match row.received_at.as_deref().and_then(parse_ts_ms) {
        Some(t) => (t, false),
        None => (row.first_seen_ms, true),
    };
    let conversation_id = row
        .thread_id
        .as_deref()
        .and_then(non_empty)
        .unwrap_or_else(|| row.message_id.clone());
    let mut f = IndexFields {
        platform: row.platform.clone(),
        conv_kind: "other".into(),
        conversation_id,
        conversation_title: non_empty(&row.subject),
        container: None,
        sender_handle: handles::canonical(&row.from),
        sender_label: display_name(&row.from),
        counterpart_handle: None,
        from_me: false,
        ts_ms,
        ts_fallback,
        has_attachment: has_attachment(&row.body),
    };
    f.from_me = owner.contains(&f.sender_handle);

    match row.platform.as_str() {
        "imessage" => imessage(row, &mut f),
        "whatsapp" => whatsapp(row, &mut f),
        "discord" => discord(row, owner, &mut f),
        "gmail" => {
            f.conv_kind = "email".into();
        }
        "apple_notes" => {
            f.conv_kind = "note".into();
            f.from_me = true;
            f.sender_handle = ME.into();
            let rest = row
                .subject
                .strip_prefix("Apple Note:")
                .unwrap_or(&row.subject);
            let (title, folder) = split_label(rest);
            f.conversation_title = non_empty(title);
            f.container = folder.and_then(non_empty);
        }
        "linkedin" | "socialapi" | "instagram" | "twitter" | "slack" | "telegram" => {
            f.conv_kind = if row.kind == "group" { "group" } else { "dm" }.into();
            // "[LinkedIn DM from Pat]" / "[Instagram DM from pat]"
            if let Some(name) = row
                .subject
                .trim()
                .strip_prefix('[')
                .and_then(|s| s.strip_suffix(']'))
                .and_then(|s| s.split_once(" from "))
                .map(|(_, n)| n)
            {
                f.conversation_title = non_empty(name);
            }
            if f.conv_kind == "dm" && !f.from_me {
                f.counterpart_handle = Some(f.sender_handle.clone());
            }
        }
        _ => {
            f.conv_kind = match row.kind.as_str() {
                "meeting" | "meeting_transcript" => "meeting",
                _ => "other",
            }
            .into();
            if f.conv_kind == "meeting" {
                if let Some(t) = row.subject.strip_prefix("Meeting:") {
                    f.conversation_title = non_empty(t);
                }
            }
        }
    }
    f
}

fn imessage(row: &EmailRowView, f: &mut IndexFields) {
    let ident = row
        .thread_id
        .as_deref()
        .and_then(|t| t.strip_prefix("imessage:"))
        .unwrap_or("");
    // Group chats carry a `chat…` identifier; rows store them as kind=dm.
    let group = ident.starts_with("chat");
    f.conv_kind = if group { "group" } else { "dm" }.into();
    let title = row
        .subject
        .strip_prefix("iMessage:")
        .unwrap_or(&row.subject);
    f.conversation_title = non_empty(title);
    if !group && !ident.is_empty() {
        f.counterpart_handle = Some(handles::canonical(ident));
    }
}

fn whatsapp(row: &EmailRowView, f: &mut IndexFields) {
    let jid = row
        .thread_id
        .as_deref()
        .and_then(|t| t.strip_prefix("whatsapp-history:"))
        .unwrap_or("");
    let group = row.kind == "group" || jid.ends_with("@g.us");
    f.conv_kind = if group { "group" } else { "dm" }.into();
    let rest = row
        .subject
        .strip_prefix("WhatsApp:")
        .unwrap_or(&row.subject);
    let (title, speaker) = split_label(rest);
    f.conversation_title = non_empty(title);
    if f.sender_label.is_none() {
        f.sender_label = speaker.and_then(non_empty);
    }
    if !group && !jid.is_empty() {
        f.counterpart_handle = Some(handles::whatsapp_jid(jid));
    }
}

fn discord(row: &EmailRowView, owner: &OwnerHandles, f: &mut IndexFields) {
    // History rows label the owner "me"; either way the owner's user id is
    // in accountEntityId as `discord:<id>`.
    let owner_id = row
        .account_entity_id
        .as_deref()
        .and_then(|a| a.strip_prefix("discord:"))
        .map(|id| format!("discord:{id}"));
    if owner_id.as_deref() == Some(f.sender_handle.as_str())
        || f.sender_label.as_deref() == Some(ME)
        || owner.contains(&f.sender_handle)
    {
        f.from_me = true;
    }
    let subject = row.subject.as_str();
    let (kind, rest) = if let Some(r) = subject.strip_prefix("Discord DM:") {
        ("dm", r)
    } else if let Some(r) = subject.strip_prefix("Discord group DM:") {
        ("group", r)
    } else if let Some(r) = subject.strip_prefix("Discord:") {
        ("channel", r)
    } else {
        (
            match row.kind.as_str() {
                "dm" => "dm",
                "group" => "group",
                _ => "channel",
            },
            subject,
        )
    };
    f.conv_kind = kind.into();
    let (title, _speaker) = split_label(rest);
    if kind == "channel" {
        // "<server> #<channel>"
        match title.rfind(" #") {
            Some(i) => {
                f.container = non_empty(&title[..i]);
                f.conversation_title = non_empty(&title[i + 1..]);
            }
            None => f.conversation_title = non_empty(title),
        }
    } else {
        f.conversation_title = non_empty(title);
    }
    if kind == "dm" && !f.from_me {
        f.counterpart_handle = Some(f.sender_handle.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(platform: &str, from: &str, thread: &str, subject: &str, kind: &str) -> EmailRowView {
        EmailRowView {
            message_id: "m1".into(),
            thread_id: (!thread.is_empty()).then(|| thread.to_string()),
            from: from.into(),
            subject: subject.into(),
            body: "hello".into(),
            received_at: Some("2026-08-26T14:32:05-04:00".into()),
            account_entity_id: None,
            first_seen_ms: 7,
            platform: platform.into(),
            kind: kind.into(),
        }
    }

    fn owner() -> OwnerHandles {
        OwnerHandles {
            handles: vec!["email:owner@example.com".into()],
        }
    }

    #[test]
    fn imessage_dm_inbound() {
        let f = extract(
            &row(
                "imessage",
                "+14155550123",
                "imessage:+14155550123",
                "iMessage: Jane Doe",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "dm");
        assert_eq!(f.sender_handle, "phone:+14155550123");
        assert!(!f.from_me);
        assert_eq!(f.conversation_title.as_deref(), Some("Jane Doe"));
        assert_eq!(f.counterpart_handle.as_deref(), Some("phone:+14155550123"));
        assert_eq!(f.conversation_id, "imessage:+14155550123");
    }

    #[test]
    fn imessage_me() {
        let f = extract(
            &row(
                "imessage",
                "me",
                "imessage:jane@example.com",
                "iMessage: Jane",
                "dm",
            ),
            &owner(),
        );
        assert!(f.from_me);
        assert_eq!(f.sender_handle, "me");
        assert_eq!(
            f.counterpart_handle.as_deref(),
            Some("email:jane@example.com")
        );
    }

    #[test]
    fn imessage_group_detected_from_chat_identifier() {
        let f = extract(
            &row(
                "imessage",
                "+14155550123",
                "imessage:chat123456",
                "iMessage: Ski Trip",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "group");
        assert_eq!(f.counterpart_handle, None);
    }

    #[test]
    fn whatsapp_jid_canonicalizes_to_number() {
        let f = extract(
            &row(
                "whatsapp",
                "+14155550123",
                "whatsapp-history:14155550123@s.whatsapp.net",
                "WhatsApp: Jane [+14155550123]",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "dm");
        assert_eq!(f.counterpart_handle.as_deref(), Some("phone:+14155550123"));
        assert_eq!(f.conversation_title.as_deref(), Some("Jane"));
    }

    #[test]
    fn whatsapp_group_title_and_sender_split_from_subject() {
        let f = extract(
            &row(
                "whatsapp",
                "me",
                "whatsapp-history:1203630001@g.us",
                "WhatsApp: Book [club] chat [me]",
                "group",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "group");
        assert!(f.from_me);
        assert_eq!(f.conversation_title.as_deref(), Some("Book [club] chat"));
        assert_eq!(f.counterpart_handle, None);
    }

    #[test]
    fn whatsapp_lid_dm_counterpart_stays_namespaced() {
        let f = extract(
            &row(
                "whatsapp",
                "me",
                "whatsapp-history:998877@lid",
                "WhatsApp: Kay [me]",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(f.counterpart_handle.as_deref(), Some("whatsapp:998877@lid"));
    }

    #[test]
    fn discord_dm_user_id_from_tag() {
        let mut r = row(
            "discord",
            "Alice <discord:500>",
            "10",
            "Discord DM: Alice [Alice]",
            "dm",
        );
        r.account_entity_id = Some("discord:900".into());
        let f = extract(&r, &owner());
        assert_eq!(f.conv_kind, "dm");
        assert_eq!(f.sender_handle, "discord:500");
        assert_eq!(f.sender_label.as_deref(), Some("Alice"));
        assert!(!f.from_me);
        assert_eq!(f.counterpart_handle.as_deref(), Some("discord:500"));
    }

    #[test]
    fn discord_owner_message_is_from_me() {
        let mut r = row(
            "discord",
            "me <discord:900>",
            "10",
            "Discord DM: Alice [me]",
            "dm",
        );
        r.account_entity_id = Some("discord:900".into());
        let f = extract(&r, &owner());
        assert!(f.from_me);
        assert_eq!(f.counterpart_handle, None);
    }

    #[test]
    fn discord_server_channel_sets_container_and_title() {
        let f = extract(
            &row(
                "discord",
                "Bob <discord:7>",
                "40",
                "Discord: Acme HQ #general [Bob]",
                "guild_channel",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "channel");
        assert_eq!(f.container.as_deref(), Some("Acme HQ"));
        assert_eq!(f.conversation_title.as_deref(), Some("#general"));
    }

    #[test]
    fn discord_live_row_without_subject_uses_kind() {
        let f = extract(
            &row("discord", "Bob <discord:7>", "40", "", "digest_item"),
            &owner(),
        );
        assert_eq!(f.conv_kind, "channel");
        assert_eq!(f.conversation_title, None);
    }

    #[test]
    fn gmail_from_me_when_sender_is_connected_account() {
        let f = extract(
            &row(
                "gmail",
                "Owner <OWNER@example.com>",
                "t1",
                "Quarterly plan",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "email");
        assert!(f.from_me);
        assert_eq!(f.conversation_title.as_deref(), Some("Quarterly plan"));
    }

    #[test]
    fn gmail_display_name_stripped_and_lowercased() {
        let f = extract(
            &row(
                "gmail",
                "\"Pat Lee\" <Pat.Lee@Example.org>",
                "t1",
                "Hi",
                "dm",
            ),
            &owner(),
        );
        assert!(!f.from_me);
        assert_eq!(f.sender_handle, "email:pat.lee@example.org");
        assert_eq!(f.sender_label.as_deref(), Some("Pat Lee"));
    }

    #[test]
    fn apple_note_is_note_from_me() {
        let f = extract(
            &row(
                "apple_notes",
                "me",
                "apple-notes:ABC",
                "Apple Note: Groceries [Home]",
                "note",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "note");
        assert!(f.from_me);
        assert_eq!(f.conversation_title.as_deref(), Some("Groceries"));
        assert_eq!(f.container.as_deref(), Some("Home"));
    }

    #[test]
    fn linkedin_and_instagram_dms() {
        let f = extract(
            &row(
                "linkedin",
                "Pat <linkedin:urn:li:fsd_profile:P1>",
                "urn:li:msg_conversation:x",
                "[LinkedIn DM from Pat]",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "dm");
        assert_eq!(f.sender_handle, "linkedin:urn:li:fsd_profile:P1");
        assert_eq!(f.conversation_title.as_deref(), Some("Pat"));
        assert_eq!(
            f.counterpart_handle.as_deref(),
            Some("linkedin:urn:li:fsd_profile:P1")
        );
        let g = extract(
            &row(
                "socialapi",
                "pat <socialapi:instagram:77>",
                "c1",
                "[Instagram DM from pat]",
                "dm",
            ),
            &owner(),
        );
        assert_eq!(g.sender_handle, "instagram:77");
    }

    #[test]
    fn meetings_strip_prefix() {
        let f = extract(
            &row(
                "gcal",
                "host@example.com",
                "",
                "Meeting: Planning sync",
                "meeting",
            ),
            &owner(),
        );
        assert_eq!(f.conv_kind, "meeting");
        assert_eq!(f.conversation_title.as_deref(), Some("Planning sync"));
        assert_eq!(
            f.conversation_id, "m1",
            "no thread → the message is its own conversation"
        );
    }

    #[test]
    fn unknown_platform_gets_conservative_defaults() {
        let f = extract(&row("carrier-pigeon", "Coo", "", "", "whatever"), &owner());
        assert_eq!(f.conv_kind, "other");
        assert_eq!(f.sender_handle, "raw:coo");
        assert!(!f.from_me);
        assert_eq!(f.counterpart_handle, None);
    }

    #[test]
    fn timestamps_parse_common_formats_and_fall_back() {
        assert_eq!(parse_ts_ms("2026-01-01T00:00:00Z"), Some(1_767_225_600_000));
        assert_eq!(
            parse_ts_ms("2026-01-01T00:00:00.500000+00:00"),
            Some(1_767_225_600_500)
        );
        assert_eq!(
            parse_ts_ms("Thu, 01 Jan 2026 00:00:00 +0000"),
            Some(1_767_225_600_000)
        );
        assert!(parse_ts_ms("2026-01-01").is_some());
        assert_eq!(parse_ts_ms("soon"), None);
        let mut r = row("gmail", "a@example.com", "t", "s", "dm");
        r.received_at = Some("garbage".into());
        let f = extract(&r, &owner());
        assert!(f.ts_fallback);
        assert_eq!(f.ts_ms, 7);
    }

    #[test]
    fn attachment_markers_detected() {
        let mut r = row("discord", "x <discord:1>", "1", "Discord DM: x [x]", "dm");
        r.body = "look\n[attachment: image/png a.png https://cdn.example.com/a.png]".into();
        assert!(extract(&r, &owner()).has_attachment);
    }

    #[test]
    fn split_label_uses_the_last_bracket_group() {
        assert_eq!(split_label("A [b] c [d]"), ("A [b] c", Some("d")));
        assert_eq!(split_label("no label"), ("no label", None));
        assert_eq!(split_label("[only]"), ("[only]", None));
    }

    #[test]
    fn extract_never_panics_on_arbitrary_text() {
        fn next(seed: &mut u64) -> u64 {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 7;
            *seed ^= *seed << 17;
            *seed
        }
        let alphabet: Vec<char> = "ab []<>:#@+-_.\u{00e9}\u{4e2d}\n\\\"0123456789"
            .chars()
            .collect();
        let mut seed: u64 = 0xDEAD_BEEF_1234_5678;
        let gen = |seed: &mut u64| -> String {
            let n = next(seed) % 30;
            (0..n)
                .map(|_| alphabet[(next(seed) as usize) % alphabet.len()])
                .collect()
        };
        let platforms = [
            "imessage",
            "whatsapp",
            "discord",
            "gmail",
            "apple_notes",
            "linkedin",
            "x",
        ];
        for i in 0..3000 {
            let r = EmailRowView {
                message_id: format!("m{i}"),
                thread_id: Some(gen(&mut seed)),
                from: gen(&mut seed),
                subject: gen(&mut seed),
                body: gen(&mut seed),
                received_at: Some(gen(&mut seed)),
                account_entity_id: Some(gen(&mut seed)),
                first_seen_ms: 1,
                platform: platforms[(next(&mut seed) % 7) as usize].into(),
                kind: gen(&mut seed),
            };
            let _ = extract(&r, &owner());
        }
    }
}
