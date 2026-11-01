//! WhatsApp archive → searchable history, incremental wiki capture, and reply
//! cards (#1764). No linked device, live channel or automated send: Approve
//! hands the owner the text to paste, and the archive confirms it (#1765).
pub mod reply;

use anyhow::{bail, Context, Result};
use augmentagent_channel_imessage::{parse_entries, Conversation, MessageEntry};
use augmentagent_store::{rusqlite::OptionalExtension, Email, Store};
use chrono::DateTime;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tracing::warn;

pub const ENV_DIR: &str = "AUGMENTAGENT_WHATSAPP_HISTORY_DIR";
pub const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Seconds between archive polls; reply cards want minutes, not half-hours.
pub const ENV_POLL_SECS: &str = "AUGMENTAGENT_WHATSAPP_HISTORY_POLL_SECS";
/// `emails.accountEntityId` of every archive row and reply card.
pub const ACCOUNT: &str = "whatsapp-history";
pub const THREAD_PREFIX: &str = "whatsapp-history:";

/// [`ENV_POLL_SECS`], floored at 60s; [`POLL_INTERVAL`] when unset or invalid.
pub fn poll_interval() -> Duration {
    std::env::var(ENV_POLL_SECS)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|secs| Duration::from_secs(secs.max(60)))
        .unwrap_or(POLL_INTERVAL)
}

pub fn is_group(conv: &Conversation) -> bool {
    conv.identifier.ends_with("@g.us")
}

/// `Name <+phone>` when the exporter knows the name, else the best handle:
/// an `@lid` privacy id resolves through the bundle's identity map (#1762)
/// and stays raw when unmapped — never guessed.
pub fn display_sender(conv: &Conversation, sender: &str) -> String {
    if sender == "me" {
        return sender.to_string();
    }
    let handle = conv.identity.get(sender).map(String::as_str).unwrap_or(sender);
    let name = conv.participant_names.get(sender).map(String::as_str).or_else(|| {
        (!is_group(conv) && !conv.title.is_empty() && conv.title != handle && conv.title != sender)
            .then_some(conv.title.as_str())
    });
    match name {
        Some(name) => format!("{name} <{handle}>"),
        None => handle.to_string(),
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub directory: PathBuf,
}

impl Config {
    pub fn load() -> Result<Option<Self>> {
        Self::from_path(std::env::var(ENV_DIR).ok().as_deref())
    }

    pub fn from_path(value: Option<&str>) -> Result<Option<Self>> {
        let Some(value) = value.map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(None);
        };
        let directory = PathBuf::from(value)
            .canonicalize()
            .context("WhatsApp history directory does not exist")?;
        if !directory.is_dir() {
            bail!("WhatsApp history path must be a directory");
        }
        Ok(Some(Self { directory }))
    }
}

/// Plain directories are already current; legacy private Git feeds pull in place.
/// Failure is returned to the caller, which can still import the last disk state.
pub async fn refresh(config: &Config) -> Result<bool> {
    if !config.directory.join(".git").exists() {
        return Ok(false);
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(&config.directory)
        .args(["pull", "--ff-only", "--quiet"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o ConnectTimeout=15",
        )
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .context("WhatsApp bundle pull timed out")??;
    if !output.status.success() {
        bail!("WhatsApp bundle pull failed ({})", output.status);
    }
    Ok(true)
}

#[derive(Default, Serialize)]
pub struct Report {
    pub inserted: usize,
    pub skipped: usize,
    pub conversations_with_new: usize,
    #[serde(skip)]
    pub deltas: Vec<Delta>,
}

pub struct Delta {
    pub conversation: Conversation,
    pub entries: Vec<(usize, MessageEntry)>,
    pub first_run: bool,
}

fn read_entries(root: &Path, conv: &Conversation) -> Result<Vec<MessageEntry>> {
    let dir = Path::new(&conv.dir);
    if dir.components().count() != 1
        || !matches!(dir.components().next(), Some(Component::Normal(_)))
    {
        bail!("invalid conversation directory");
    }
    let base = root.join("conversations").canonicalize()?;
    if !base.starts_with(root.canonicalize()?) {
        bail!("conversation root escapes bundle");
    }
    let path = base.join(dir).join("messages.md").canonicalize()?;
    if !path.starts_with(&base) {
        bail!("conversation path escapes bundle");
    }
    Ok(parse_entries(&std::fs::read_to_string(path)?))
}

pub fn email(conv: &Conversation, idx: usize, entry: &MessageEntry) -> Email {
    let mut email = augmentagent_channel_imessage::synthetic_imessage_email(conv, idx, entry);
    email.message_id = format!("{THREAD_PREFIX}{}:{idx}", conv.identifier);
    email.thread_id = Some(format!("{THREAD_PREFIX}{}", conv.identifier));
    email.platform = "whatsapp".into();
    email.account_entity_id = Some(ACCOUNT.into());
    email.from = display_sender(conv, &entry.sender);
    email.kind = if is_group(conv) {
        "group"
    } else {
        "dm"
    }
    .into();
    // Search snippets carry the speaker, including the owner's sent messages.
    email.subject = format!("WhatsApp: {} [{}]", conv.title, entry.sender);
    email
}

pub fn poll_once(root: &Path, store: &Store) -> Result<Report> {
    let index: BTreeMap<String, Conversation> = serde_json::from_str(&std::fs::read_to_string(
        root.join("conversations/index.json"),
    )?)
    .context("invalid WhatsApp conversation index")?;
    store.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS whatsapp_history_sync_state (
            conversation TEXT PRIMARY KEY, entries_seen INTEGER NOT NULL
        );",
        )
    })?;
    store.set_archive_newest_entry(
        "whatsapp",
        augmentagent_channel_imessage::newest_entry_of(
            index.values().filter_map(|c| c.newest_entry.as_deref()),
        )
        .as_deref(),
    )?;
    let mut report = Report::default();
    for (key, conv) in index {
        if key != conv.identifier || conv.service != "WhatsApp" {
            report.skipped += 1;
            continue;
        }
        let entries = match read_entries(root, &conv) {
            Ok(entries) => entries,
            Err(_) => {
                report.skipped += 1;
                warn!("skipping unreadable WhatsApp conversation");
                continue;
            }
        };
        let seen: Option<usize> = store.with_conn(|conn| {
            conn.query_row(
                "SELECT entries_seen FROM whatsapp_history_sync_state WHERE conversation=?1",
                [&conv.identifier],
                |row| row.get(0),
            )
            .optional()
        })?;
        let start = seen.unwrap_or(0);
        if entries.len() < start {
            report.skipped += 1;
            warn!("WhatsApp conversation shrank; preserving cursor and imported history");
            continue;
        }
        let mut delta = Delta {
            conversation: conv,
            entries: Vec::new(),
            first_run: seen.is_none(),
        };
        for (idx, entry) in entries.iter().enumerate().skip(start) {
            // Consume the archive position without storing private phone setup
            // or passing it into incremental wiki capture. Keep original indices
            // so skipping controls cannot renumber ordinary message identities.
            if augmentagent_phone::commands::sensitive(&entry.body) {
                continue;
            }
            // Don't silently rewrite an unknown timestamp as today's history.
            let ts = match DateTime::parse_from_rfc3339(&entry.timestamp) {
                Ok(ts) => ts.timestamp_millis(),
                Err(_) => continue,
            };
            let item = email(&delta.conversation, idx, entry);
            if store.upsert_email_backfill(&item, ts)? {
                report.inserted += 1;
                delta.entries.push((idx, entry.clone()));
            }
        }
        store.with_conn(|conn| {
            conn.execute(
                "INSERT INTO whatsapp_history_sync_state (conversation,entries_seen) VALUES (?1,?2)
             ON CONFLICT(conversation) DO UPDATE SET entries_seen=excluded.entries_seen",
                augmentagent_store::rusqlite::params![
                    delta.conversation.identifier,
                    entries.len() as i64
                ],
            )
        })?;
        if !delta.entries.is_empty() {
            report.conversations_with_new += 1;
            report.deltas.push(delta);
        }
    }
    Ok(report)
}

/// Bounded recent context; full history remains available through the memory tool.
pub fn capture_email(delta: &Delta) -> Email {
    let (idx, last) = delta.entries.last().expect("capture only nonempty deltas");
    let mut result = email(&delta.conversation, *idx, last);
    result.message_id = format!(
        "whatsapp-history:{}:batch:{idx}",
        delta.conversation.identifier
    );
    let text = delta
        .entries
        .iter()
        .map(|(_, entry)| {
            format!(
                "### [{}] {}\n{}\n{}",
                entry.timestamp,
                entry.sender,
                entry.body,
                entry.attachments.join("\n")
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let start = text
        .char_indices()
        .rev()
        .nth(7_999)
        .map(|(i, _)| i)
        .unwrap_or(0);
    result.body = text[start..].to_string();
    result
}

#[cfg(test)]
mod index_tests {
    use super::*;

    #[test]
    fn old_index_without_members_still_imports_and_new_fields_parse() {
        let old: Conversation = serde_json::from_str(
            r#"{"identifier":"1@g.us","dir":"g","title":"G","service":"WhatsApp","participants":[]}"#,
        )
        .unwrap();
        assert!(old.participant_names.is_empty() && old.identity.is_empty() && old.active.is_none());
        let new: Conversation = serde_json::from_str(
            r#"{"identifier":"1@g.us","dir":"g","title":"G","service":"WhatsApp",
                "participants":["+14155550999","111@lid","222@lid"],
                "participant_names":{"111@lid":"Ana"},"identity":{"111@lid":"+14155550777"},"active":false}"#, // pii-ok synthetic
        )
        .unwrap();
        assert_eq!(new.active, Some(false));
        assert_eq!(display_sender(&new, "111@lid"), "Ana <+14155550777>"); // pii-ok synthetic
        assert_eq!(display_sender(&new, "222@lid"), "222@lid", "unmapped lid stays raw");
        assert_eq!(display_sender(&new, "me"), "me");
    }
}
