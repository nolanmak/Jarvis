//! Voice-capture channel: turn a structured memo into a synthetic `Email`
//! and hand it straight to the existing `spawn_ingest` pipeline as a
//! `DecisionKind::Capture` / `IngestTrigger::VoiceMemo`.
//!
//! IMPORTANT: this makes ZERO changes to the ingest pipeline. We synthesize
//! the same `Email` shape the gcal channel uses and call the public
//! `spawn_ingest` exactly as the email channel does on its capture path.

use std::path::PathBuf;
use std::sync::Arc;

use augmentagent_channel_core::decision::DecisionKind;
use augmentagent_channel_core::ingest::{spawn_ingest, IngestTrigger};
use augmentagent_channel_core::reasoner::Reasoner;
use augmentagent_store::Email;

use crate::extract::MemoRecord;
use crate::{ACCOUNT_ENTITY_ID_PREFIX, PLATFORM};

/// Build the synthetic email for a captured memo. `source_id` is the
/// Telegram `message_id` — stable, so a re-poll of the same update dedups on
/// the `emails` table exactly like every other channel.
pub fn synthetic_memo_email(rec: &MemoRecord, chat_id: i64, source_id: i64) -> Email {
    let title = if rec.title.trim().is_empty() {
        "Voice memo".to_string()
    } else {
        rec.title.trim().to_string()
    };
    let mut body = String::new();
    if !rec.summary.trim().is_empty() {
        body.push_str(rec.summary.trim());
        body.push('\n');
    }
    if !rec.people.is_empty() {
        body.push_str(&format!("\nPeople: {}", rec.people.join(", ")));
    }
    if !rec.commitments.is_empty() {
        body.push_str("\nCommitments:");
        for c in &rec.commitments {
            body.push_str(&format!("\n- {c}"));
        }
    }
    if !rec.topics.is_empty() {
        body.push_str(&format!("\nTopics: {}", rec.topics.join(", ")));
    }
    Email {
        attachments: Vec::new(),
        to: String::new(),
        cc: String::new(),
        message_id: format!("voice:{chat_id}:{source_id}"),
        thread_id: None,
        from: format!("{ACCOUNT_ENTITY_ID_PREFIX}:{chat_id}"),
        subject: format!("Voice memo: {title}"),
        body: body.trim().to_string(),
        date: String::new(),
        account_entity_id: Some(format!("{ACCOUNT_ENTITY_ID_PREFIX}:{chat_id}")),
        platform: PLATFORM.into(),
        kind: "voice_memo".into(),
    }
}

/// Fire the memo through the wiki ingest pipeline. Fire-and-forget: returns
/// once the background task is spawned (same contract as `spawn_ingest`).
#[allow(clippy::too_many_arguments)]
pub fn ingest_memo<R>(
    reasoner: Arc<R>,
    wiki_root: PathBuf,
    schema: String,
    rec: &MemoRecord,
    chat_id: i64,
    source_id: i64,
) where
    R: Reasoner + 'static,
{
    let email = synthetic_memo_email(rec, chat_id, source_id);
    spawn_ingest(
        reasoner,
        wiki_root,
        schema,
        email,
        DecisionKind::Capture,
        Some("voice memo capture".to_string()),
        None,
        IngestTrigger::VoiceMemo,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_email_is_capture_shaped() {
        let rec = MemoRecord {
            title: "Call Sam".into(),
            summary: "Need to call Sam about the Q3 deck".into(),
            people: vec!["Sam".into()],
            commitments: vec!["call Sam".into()],
            topics: vec!["deck".into()],
        };
        let e = synthetic_memo_email(&rec, 999, 7);
        assert_eq!(e.message_id, "voice:999:7");
        assert_eq!(e.platform, "voice");
        assert_eq!(e.kind, "voice_memo");
        assert!(e.subject.contains("Call Sam"));
        assert!(e.body.contains("call Sam"));
        assert!(e.body.contains("Sam"));
        assert_eq!(e.account_entity_id.as_deref(), Some("voice:999"));
    }

    #[test]
    fn empty_record_still_yields_valid_email() {
        let e = synthetic_memo_email(&MemoRecord::default(), 1, 2);
        assert_eq!(e.message_id, "voice:1:2");
        assert!(e.subject.contains("Voice memo"));
    }
}
