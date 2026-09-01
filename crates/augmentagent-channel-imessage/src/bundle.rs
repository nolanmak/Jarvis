//! Read-only parser for the external iMessage OKF v0.2 bundle.
//!
//! Bundle shape (produced by the operator's sync job, refreshed every
//! ~30 min; this crate never writes to it):
//!
//! ```text
//! <root>/conversations/index.json      { "<ident>": { title, participants,
//!                                        service, dir, path, .. }, .. }
//! <root>/conversations/<dir>/messages.md
//!   ---  YAML frontmatter  ---
//!   ### [2026-08-26T14:32:05-04:00] me
//!   body lines until the next header; lines that would collide with the
//!   header pattern are escaped with a leading backslash
//!   [attachment: image/jpeg IMG_001.jpeg s3://bucket/key]
//! ```
//!
//! Entries are append-only, so a per-conversation "entries seen" count is a
//! stable incremental cursor.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use augmentagent_store::Email;
use serde::Deserialize;

/// One conversation from `conversations/index.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Conversation {
    /// Stable chat identifier (phone/email for DMs, `chatNNN…` for groups).
    pub identifier: String,
    /// Directory name under `conversations/` (contact name when resolved).
    pub dir: String,
    pub title: String,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub service: String,
}

impl Conversation {
    /// DMs have a single participant; groups have several (or a `chatNNN…`
    /// identifier when membership metadata is missing).
    pub fn is_group(&self) -> bool {
        self.identifier.starts_with("chat") || self.participants.len() > 1
    }
}

/// One parsed message entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageEntry {
    /// ISO-8601 timestamp with offset, verbatim from the header.
    pub timestamp: String,
    /// `me` for the operator; otherwise a raw handle (phone or email).
    pub sender: String,
    /// Body text with header-escapes removed. Empty for attachment-only
    /// entries.
    pub body: String,
    /// `[attachment: …]` lines, verbatim (mime, name, optional s3 uri).
    pub attachments: Vec<String>,
}

pub struct Bundle {
    root: PathBuf,
}

impl Bundle {
    pub fn open(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Conversations from `index.json`, keyed by identifier, sorted for
    /// deterministic iteration.
    pub fn conversations(&self) -> Result<Vec<Conversation>> {
        let path = self.root.join("conversations").join("index.json");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let map: BTreeMap<String, Conversation> = serde_json::from_str(&raw)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(map.into_values().collect())
    }

    /// All entries of one conversation, oldest first.
    pub fn entries(&self, conv: &Conversation) -> Result<Vec<MessageEntry>> {
        let path = self
            .root
            .join("conversations")
            .join(&conv.dir)
            .join("messages.md");
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(parse_entries(&raw))
    }
}

const HEADER_PREFIX: &str = "### [";

/// Parse the entry list out of a `messages.md`, skipping the frontmatter.
pub fn parse_entries(md: &str) -> Vec<MessageEntry> {
    let body = skip_frontmatter(md);
    let mut entries: Vec<MessageEntry> = Vec::new();

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(HEADER_PREFIX) {
            // `### [<ts>] <sender>`
            if let Some(close) = rest.find(']') {
                let timestamp = rest[..close].to_string();
                let sender = rest[close + 1..].trim().to_string();
                entries.push(MessageEntry {
                    timestamp,
                    sender,
                    body: String::new(),
                    attachments: Vec::new(),
                });
                continue;
            }
        }
        let Some(current) = entries.last_mut() else {
            continue; // preamble noise before the first header
        };
        if line.starts_with("[attachment: ") && line.ends_with(']') {
            current.attachments.push(line.to_string());
        } else {
            // un-escape body lines that collide with the header pattern
            let unescaped = line.strip_prefix('\\').filter(|r| r.starts_with(HEADER_PREFIX));
            let text = unescaped.unwrap_or(line);
            if !current.body.is_empty() {
                current.body.push('\n');
            }
            current.body.push_str(text);
        }
    }

    for e in &mut entries {
        e.body = e.body.trim().to_string();
    }
    entries
}

fn skip_frontmatter(src: &str) -> &str {
    let Some(after) = src.strip_prefix("---\n") else {
        return src;
    };
    match after.find("\n---\n") {
        Some(end) => &after[end + 5..],
        None => src,
    }
}

/// Synthesize the universal message envelope. `idx` is the entry's position
/// in the conversation file — stable because entries are append-only.
pub fn synthetic_imessage_email(conv: &Conversation, idx: usize, entry: &MessageEntry) -> Email {
    let mut body = entry.body.clone();
    for a in &entry.attachments {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(a);
    }
    Email {
        message_id: format!("imessage:{}:{}", conv.identifier, idx),
        thread_id: Some(format!("imessage:{}", conv.identifier)),
        from: entry.sender.clone(),
        to: String::new(),
        cc: String::new(),
        attachments: entry.attachments.clone(),
        subject: format!("iMessage: {}", conv.title),
        body,
        date: entry.timestamp.clone(),
        account_entity_id: Some("imessage".into()),
        platform: "imessage".into(),
        kind: "dm".into(),
    }
}

/// `2026-08-26T14:32:05-04:00` → `2026-08-26`. Timestamps are written by the
/// bundle producer; a malformed one yields `None` rather than a bogus date.
pub fn entry_date(timestamp: &str) -> Option<&str> {
    let date = timestamp.get(..10)?;
    let ok = date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date.chars().enumerate().all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit());
    ok.then_some(date)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const SAMPLE_MD: &str = "---\ntype: iMessage Conversation\ntitle: 'John Smith'\nchat_identifier: '+14155550123'\n---\n\n### [2026-08-26T14:32:05-04:00] me\nsee you at 7\n\n### [2026-08-26T14:33:10-04:00] +14155550123\nsounds good\n\\### [not a real header] escaped line\n\n### [2026-08-26T14:35:00-04:00] +14155550123\n[attachment: image/jpeg IMG_001.jpeg s3://b/conversations/John_Smith/attachments/9-IMG_001.jpeg]\n";

    #[test]
    fn parses_entries_with_escapes_and_attachments() {
        let entries = parse_entries(SAMPLE_MD);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].sender, "me");
        assert_eq!(entries[0].timestamp, "2026-08-26T14:32:05-04:00");
        assert_eq!(entries[0].body, "see you at 7");
        // escaped header line is body text, unescaped
        assert_eq!(
            entries[1].body,
            "sounds good\n### [not a real header] escaped line"
        );
        // attachment-only entry
        assert_eq!(entries[2].body, "");
        assert_eq!(entries[2].attachments.len(), 1);
        assert!(entries[2].attachments[0].contains("s3://"));
    }

    #[test]
    fn file_without_frontmatter_still_parses() {
        let entries = parse_entries("### [2026-01-01T00:00:00+00:00] me\nhi\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].body, "hi");
    }

    #[test]
    fn bundle_reads_index_and_entries() {
        let dir = TempDir::new().unwrap();
        let conv_dir = dir.path().join("conversations").join("John_Smith");
        std::fs::create_dir_all(&conv_dir).unwrap();
        std::fs::write(
            dir.path().join("conversations").join("index.json"),
            r#"{"+14155550123": {"identifier": "+14155550123", "dir": "John_Smith",
                "title": "John Smith", "participants": ["+14155550123"],
                "service": "iMessage", "path": "conversations/John_Smith/messages.md"}}"#,
        )
        .unwrap();
        std::fs::write(conv_dir.join("messages.md"), SAMPLE_MD).unwrap();

        let bundle = Bundle::open(dir.path());
        let convs = bundle.conversations().unwrap();
        assert_eq!(convs.len(), 1);
        assert!(!convs[0].is_group());
        let entries = bundle.entries(&convs[0]).unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn group_detection() {
        let group = Conversation {
            identifier: "chat0001".into(),
            dir: "chat0001".into(),
            title: "Ski Trip".into(),
            participants: vec!["+1".into(), "+2".into()],
            service: "iMessage".into(),
        };
        assert!(group.is_group());
    }

    #[test]
    fn synthetic_email_shape() {
        let conv = Conversation {
            identifier: "+14155550123".into(),
            dir: "John_Smith".into(),
            title: "John Smith".into(),
            participants: vec!["+14155550123".into()],
            service: "iMessage".into(),
        };
        let entries = parse_entries(SAMPLE_MD);
        let email = synthetic_imessage_email(&conv, 1, &entries[1]);
        assert_eq!(email.message_id, "imessage:+14155550123:1");
        assert_eq!(email.thread_id.as_deref(), Some("imessage:+14155550123"));
        assert_eq!(email.from, "+14155550123");
        assert_eq!(email.platform, "imessage");
        assert_eq!(email.kind, "dm");
        assert_eq!(email.date, "2026-08-26T14:33:10-04:00");
    }

    #[test]
    fn entry_date_extraction() {
        assert_eq!(entry_date("2026-08-26T14:32:05-04:00"), Some("2026-08-26"));
        assert_eq!(entry_date("unknown-time"), None);
        assert_eq!(entry_date(""), None);
    }
}
