//! Read-only WhatsApp archive → searchable history and incremental wiki capture.
//! No linked device, live channel, reply drafting, or outbound access is enabled.
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

fn email(conv: &Conversation, idx: usize, entry: &MessageEntry) -> Email {
    let mut email = augmentagent_channel_imessage::synthetic_imessage_email(conv, idx, entry);
    email.message_id = format!("whatsapp-history:{}:{idx}", conv.identifier);
    email.thread_id = Some(format!("whatsapp-history:{}", conv.identifier));
    email.platform = "whatsapp".into();
    email.account_entity_id = Some("whatsapp-history".into());
    email.kind = if conv.identifier.ends_with("@g.us") {
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
