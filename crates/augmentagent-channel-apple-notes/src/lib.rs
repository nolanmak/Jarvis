//! Read-only Apple Notes bundle → searchable history and incremental wiki
//! capture (#1062). The bundle is produced by `scripts/apple-notes/` and kept
//! in a private git checkout the operator points `AUGMENTAGENT_APPLE_NOTES_REPO_DIR`
//! at. This crate never writes to that checkout and never touches Notes.app.
//!
//! Notes are mutable documents, so unlike the iMessage/WhatsApp bundles the
//! cursor is a per-note content hash rather than an append-only entry count:
//! an edit replaces the note's `emails` row (stable `message_id`), a
//! tombstone in `notes/index.json` deletes it.
use anyhow::{bail, Context, Result};
use augmentagent_store::{Email, Store};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tracing::warn;

pub const ENV_DIR: &str = "AUGMENTAGENT_APPLE_NOTES_REPO_DIR";
pub const POLL_INTERVAL: Duration = Duration::from_secs(30 * 60);
pub const PLATFORM: &str = "apple_notes";
/// Wiki capture sends at most this much note text per edit; the full note
/// stays readable through the memory tool.
pub const CAPTURE_MAX_CHARS: usize = 6_000;

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
            .context("Apple Notes bundle directory does not exist")?;
        if !directory.is_dir() {
            bail!("Apple Notes bundle path must be a directory");
        }
        Ok(Some(Self { directory }))
    }
}

/// Pull the bundle checkout. A plain directory is already current; a pull
/// failure is returned so the caller can still read the last on-disk state.
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
        .context("Apple Notes bundle pull timed out")??;
    if !output.status.success() {
        bail!("Apple Notes bundle pull failed ({})", output.status);
    }
    Ok(true)
}

/// One entry of `notes/index.json`, keyed by the note's UUID.
#[derive(Clone, Debug, Deserialize)]
pub struct IndexEntry {
    pub title: String,
    #[serde(default)]
    pub folder: String,
    /// Absent for tombstones.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub modified: String,
    #[serde(default)]
    pub deleted: Option<String>,
}

/// A parsed note file: frontmatter fields plus the body text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteDoc {
    pub identifier: String,
    pub title: String,
    pub folder: String,
    pub account: String,
    pub created: String,
    pub modified: String,
    pub attachments: Vec<String>,
    pub redactions: Vec<String>,
    /// Body after the frontmatter and its blank separator line.
    pub text: String,
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    // Values are written as JSON strings (valid YAML double-quoted scalars).
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.trim_matches('\'').to_string())
}

/// Parse a note file written by `scripts/apple-notes/apple_notes_sync.py`.
pub fn read_note(path: &Path) -> Result<NoteDoc> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_note(&raw)
}

pub fn parse_note(raw: &str) -> Result<NoteDoc> {
    let rest = raw
        .strip_prefix("---\n")
        .context("note has no frontmatter")?;
    let (front, body) = rest
        .split_once("\n---\n")
        .context("note frontmatter is unterminated")?;
    let mut doc = NoteDoc::default();
    let mut list: Option<&mut Vec<String>> = None;
    for line in front.lines() {
        if let Some(item) = line.strip_prefix("  - ") {
            if let Some(target) = list.as_deref_mut() {
                target.push(unquote(item));
            }
            continue;
        }
        list = None;
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim() {
            "identifier" => doc.identifier = unquote(value),
            "title" => doc.title = unquote(value),
            "folder" => doc.folder = unquote(value),
            "account" => doc.account = unquote(value),
            "created" => doc.created = unquote(value),
            "modified" => doc.modified = unquote(value),
            "attachments" => list = Some(&mut doc.attachments),
            "redactions" => list = Some(&mut doc.redactions),
            _ => {}
        }
    }
    doc.text = body.strip_prefix('\n').unwrap_or(body).to_string();
    Ok(doc)
}

/// sha256 over title and text, independent of `modified` and of the sync
/// job's own state file, so a re-save that changes nothing is not an edit.
pub fn content_hash(doc: &NoteDoc) -> String {
    let mut h = Sha256::new();
    h.update(doc.title.as_bytes());
    h.update(b"\n");
    h.update(doc.text.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn message_id(identifier: &str) -> String {
    format!("apple-notes:{identifier}")
}

/// Stable per note: an edit upserts over the previous version so search
/// only ever sees the latest text.
pub fn note_email(doc: &NoteDoc) -> Email {
    Email {
        message_id: message_id(&doc.identifier),
        thread_id: Some(message_id(&doc.identifier)),
        from: "me".into(),
        to: String::new(),
        cc: String::new(),
        attachments: doc.attachments.clone(),
        subject: format!("Apple Note: {} [{}]", doc.title, doc.folder),
        body: doc.text.clone(),
        date: doc.modified.clone(),
        account_entity_id: Some("apple-notes".into()),
        platform: PLATFORM.into(),
        kind: "note".into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Change {
    New,
    Updated,
}

#[derive(Debug)]
pub struct Delta {
    pub doc: NoteDoc,
    pub change: Change,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub new: usize,
    pub updated: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub skipped: usize,
    /// The state table was empty: a full-history pass callers must not fan
    /// out to LLM capture.
    pub first_run: bool,
    #[serde(skip)]
    pub deltas: Vec<Delta>,
}

fn resolve(root: &Path, rel: &str) -> Result<PathBuf> {
    let rel_path = Path::new(rel);
    if rel_path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        bail!("note path escapes bundle");
    }
    let base = root.join("notes").canonicalize()?;
    let path = base
        .join(rel_path.strip_prefix("notes").unwrap_or(rel_path))
        .canonicalize()?;
    if !path.starts_with(&base) {
        bail!("note path escapes bundle");
    }
    Ok(path)
}

fn ensure_table(store: &Store) -> Result<()> {
    store.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS apple_notes_sync_state (
                identifier TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );",
        )
    })?;
    Ok(())
}

/// Compare every indexed note against `apple_notes_sync_state` and bring
/// the `emails` table in line. Read-only on the bundle.
pub fn poll_once(root: &Path, store: &Store) -> Result<Report> {
    poll(root, store, false)
}

/// `poll_once` with a `dry_run` switch: when set, the report and deltas are
/// computed against the store's real state but nothing is written.
pub fn poll(root: &Path, store: &Store, dry_run: bool) -> Result<Report> {
    let index_path = root.join("notes/index.json");
    let index: BTreeMap<String, IndexEntry> = serde_json::from_str(
        &std::fs::read_to_string(&index_path)
            .with_context(|| format!("reading {}", index_path.display()))?,
    )
    .with_context(|| format!("parsing {}", index_path.display()))?;
    ensure_table(store)?;
    let known: BTreeMap<String, String> = store.with_conn(|conn| {
        let mut stmt =
            conn.prepare("SELECT identifier, content_hash FROM apple_notes_sync_state")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        rows.collect()
    })?;
    let mut report = Report {
        first_run: known.is_empty(),
        ..Report::default()
    };
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Retire notes that are tombstoned or gone from the index entirely.
    for identifier in known.keys() {
        let live = index
            .get(identifier)
            .map(|e| e.deleted.is_none() && e.path.is_some())
            .unwrap_or(false);
        if live {
            continue;
        }
        if !dry_run {
            store.with_conn(|conn| {
                conn.execute(
                    "DELETE FROM emails WHERE messageId = ?1",
                    [message_id(identifier)],
                )?;
                conn.execute(
                    "DELETE FROM apple_notes_sync_state WHERE identifier = ?1",
                    [identifier],
                )
            })?;
        }
        report.deleted += 1;
    }

    for (identifier, entry) in &index {
        let (Some(rel), None) = (&entry.path, &entry.deleted) else {
            continue;
        };
        let doc = match resolve(root, rel).and_then(|p| read_note(&p)) {
            Ok(doc) => doc,
            Err(e) => {
                report.skipped += 1;
                warn!(error = %e, "skipping unreadable Apple Note");
                continue;
            }
        };
        let hash = content_hash(&doc);
        let change = match known.get(identifier) {
            Some(prev) if *prev == hash => {
                report.unchanged += 1;
                continue;
            }
            Some(_) => Change::Updated,
            None => Change::New,
        };
        // Don't rewrite an unknown timestamp as today's history.
        let ts = DateTime::parse_from_rfc3339(&doc.modified)
            .map(|t| t.timestamp_millis())
            .unwrap_or(now_ms);
        let mut doc = doc;
        if doc.identifier.is_empty() {
            doc.identifier = identifier.clone();
        }
        if !dry_run {
            store.upsert_email_backfill(&note_email(&doc), ts)?;
            store.with_conn(|conn| {
                conn.execute(
                    "INSERT INTO apple_notes_sync_state (identifier, content_hash, updated_at_ms) VALUES (?1, ?2, ?3)
                     ON CONFLICT(identifier) DO UPDATE SET content_hash = excluded.content_hash, updated_at_ms = excluded.updated_at_ms",
                    augmentagent_store::rusqlite::params![identifier, hash, now_ms],
                )
            })?;
        }
        match change {
            Change::New => report.new += 1,
            Change::Updated => report.updated += 1,
        }
        report.deltas.push(Delta { doc, change });
    }
    Ok(report)
}

/// Bounded note text for wiki capture; the subject says whether the note is
/// new or edited so the ingest prompt can phrase it accordingly.
pub fn capture_email(delta: &Delta) -> Email {
    let mut email = note_email(&delta.doc);
    email.message_id = format!("{}:capture:{}", email.message_id, delta.doc.modified);
    email.subject = match delta.change {
        Change::New => format!("Apple Note (new): {}", delta.doc.title),
        Change::Updated => format!("Apple Note (edited): {}", delta.doc.title),
    };
    if email.body.chars().count() > CAPTURE_MAX_CHARS {
        let cut: String = email.body.chars().take(CAPTURE_MAX_CHARS).collect();
        email.body =
            format!("{cut}\n[… truncated; full note available via search_conversation_history]");
    }
    email
}

/// What the daemon hands to wiki capture after a poll: nothing when capture
/// is off or when this was the first full-bundle pass.
pub fn capture_emails(report: &Report, wiki_capture: bool) -> Vec<Email> {
    if !wiki_capture || report.first_run {
        return Vec::new();
    }
    report.deltas.iter().map(capture_email).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_note_handles_quoted_title_and_body_dashes() {
        let doc = parse_note("---\ntype: \"Apple Note\"\nidentifier: \"ID\"\ntitle: \"A: \\\"b\\\" #c\"\nfolder: \"F\"\naccount: \"iCloud\"\ncreated: \"c\"\nmodified: \"m\"\n---\n\nx\n---\ny\n").unwrap();
        assert_eq!(doc.title, "A: \"b\" #c");
        assert_eq!(doc.text, "x\n---\ny\n");
    }

    #[test]
    fn parse_note_rejects_missing_frontmatter() {
        assert!(parse_note("just text").is_err());
        assert!(parse_note("---\ntitle: x\n").is_err());
    }

    #[test]
    fn capture_is_bounded_on_char_boundaries() {
        let doc = NoteDoc {
            title: "T".into(),
            text: "é".repeat(CAPTURE_MAX_CHARS + 10),
            ..Default::default()
        };
        let email = capture_email(&Delta {
            doc,
            change: Change::New,
        });
        assert!(email.body.contains("truncated"));
        assert!(email.body.chars().count() < CAPTURE_MAX_CHARS + 100);
    }
}
