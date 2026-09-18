//! Conversation-window chunking (#1129): what actually gets embedded.
//!
//! Single chat lines carry little retrievable meaning, so chat is embedded
//! as windows of consecutive messages in one conversation, split on a time
//! gap and capped by message count and character budget. Email, notes and
//! meetings are one chunk per item.
//!
//! Boundaries depend only on earlier messages, so appending to a
//! conversation never moves an existing boundary: every chunk except the
//! open tail is sealed and keeps its id and text. Deleting a message
//! re-chunks only its conversation.

use std::collections::BTreeSet;
use std::time::Duration;

use augmentagent_store::rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkParams {
    /// A gap longer than this starts a new chunk.
    pub gap_ms: i64,
    pub max_messages: usize,
    pub max_chars: usize,
}

impl Default for ChunkParams {
    fn default() -> Self {
        Self {
            gap_ms: 30 * 60 * 1000,
            max_messages: 40,
            max_chars: 4_000,
        }
    }
}

/// Fingerprint of the tunables; stored with every chunk so a changed setting
/// is detected by `check` instead of silently mixing chunkings.
impl ChunkParams {
    pub fn version(&self) -> String {
        format!(
            "v1:{}:{}:{}",
            self.gap_ms, self.max_messages, self.max_chars
        )
    }
}

/// One indexed message, as the chunker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    pub message_id: String,
    pub ts_ms: i64,
    pub speaker: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Chunk {
    pub chunk_id: String,
    pub conversation_id: String,
    pub first_message_id: String,
    pub last_message_id: String,
    pub message_count: usize,
    pub start_ts_ms: i64,
    pub end_ts_ms: i64,
    pub text: String,
    pub text_hash: String,
    /// `false` only for the last chunk of a conversation, which may still grow.
    pub sealed: bool,
}

pub fn chunk_id(conversation_id: &str, first_message_id: &str) -> String {
    format!("{conversation_id}\u{1f}{first_message_id}")
}

fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Kinds embedded one item per chunk.
pub fn is_item_kind(conv_kind: &str) -> bool {
    matches!(conv_kind, "email" | "note" | "meeting" | "other")
}

/// Pure chunker over one conversation's messages (oldest first).
/// `header` (title / container) is prefixed once per chunk.
pub fn chunk_messages(
    conversation_id: &str,
    header: &str,
    conv_kind: &str,
    msgs: &[Msg],
    p: &ChunkParams,
) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current: Vec<&Msg> = Vec::new();
    let mut chars = 0usize;
    let flush = |current: &mut Vec<&Msg>, chunks: &mut Vec<Chunk>| {
        if current.is_empty() {
            return;
        }
        let mut text = String::new();
        if !header.is_empty() {
            text.push_str(header);
            text.push('\n');
        }
        for m in current.iter() {
            text.push_str(&m.speaker);
            text.push_str(": ");
            text.push_str(m.text.trim());
            text.push('\n');
        }
        let text = text.trim_end().to_string();
        chunks.push(Chunk {
            chunk_id: chunk_id(conversation_id, &current[0].message_id),
            conversation_id: conversation_id.to_string(),
            first_message_id: current[0].message_id.clone(),
            last_message_id: current[current.len() - 1].message_id.clone(),
            message_count: current.len(),
            start_ts_ms: current[0].ts_ms,
            end_ts_ms: current[current.len() - 1].ts_ms,
            text_hash: hash(&text),
            text,
            sealed: true,
        });
        current.clear();
    };
    for m in msgs {
        let len = m.text.len() + m.speaker.len() + 3;
        let split = if is_item_kind(conv_kind) {
            !current.is_empty()
        } else if let Some(last) = current.last() {
            m.ts_ms - last.ts_ms > p.gap_ms
                || current.len() >= p.max_messages
                || chars + len > p.max_chars
        } else {
            false
        };
        if split {
            flush(&mut current, &mut chunks);
            chars = 0;
        }
        current.push(m);
        chars += len;
    }
    flush(&mut current, &mut chunks);
    if let Some(last) = chunks.last_mut() {
        // The tail may still grow (chat only; items are complete).
        last.sealed = is_item_kind(conv_kind);
    }
    chunks
}

pub fn ensure_tables(c: &Connection) -> augmentagent_store::rusqlite::Result<()> {
    c.execute_batch(
        "CREATE TABLE IF NOT EXISTS message_chunks (
             chunk_id         TEXT PRIMARY KEY,
             conversation_id  TEXT NOT NULL,
             first_message_id TEXT NOT NULL,
             last_message_id  TEXT NOT NULL,
             message_count    INTEGER NOT NULL,
             start_ts_ms      INTEGER NOT NULL,
             end_ts_ms        INTEGER NOT NULL,
             text_hash        TEXT NOT NULL,
             sealed           INTEGER NOT NULL,
             params_version   TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_chunks_conv ON message_chunks(conversation_id, start_ts_ms);
         CREATE INDEX IF NOT EXISTS idx_chunks_end ON message_chunks(end_ts_ms);",
    )
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
pub struct ChunkReport {
    pub conversations: usize,
    pub chunks: usize,
    /// New chunks or chunks whose text changed: these need (re-)embedding.
    pub changed: Vec<String>,
    pub removed: usize,
}

impl ChunkReport {
    fn absorb(&mut self, o: ChunkReport) {
        self.conversations += o.conversations;
        self.chunks += o.chunks;
        self.changed.extend(o.changed);
        self.removed += o.removed;
    }
}

/// Load one conversation's messages from the index + full-text tables.
fn load_conversation(
    c: &Connection,
    conversation_id: &str,
) -> augmentagent_store::rusqlite::Result<(String, String, Vec<Msg>)> {
    let mut stmt = c.prepare(
        "SELECT mi.message_id, mi.ts_ms, mi.from_me, mi.sender_label, mi.sender_handle,
                mi.conv_kind, mi.conversation_title, mi.container, COALESCE(f.body, '')
           FROM message_index mi LEFT JOIN message_fts f ON f.rowid = mi.rowid
          WHERE mi.conversation_id = ?1 ORDER BY mi.ts_ms, mi.rowid",
    )?;
    let mut kind = String::from("other");
    let mut header = String::new();
    let mut msgs = Vec::new();
    for row in stmt.query_map([conversation_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)? != 0,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, Option<String>>(6)?,
            r.get::<_, Option<String>>(7)?,
            r.get::<_, String>(8)?,
        ))
    })? {
        let (id, ts, from_me, label, handle, k, title, container, body) = row?;
        kind = k;
        if header.is_empty() {
            header = [
                title.as_deref().unwrap_or(""),
                container.as_deref().unwrap_or(""),
            ]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
        }
        let speaker = if from_me {
            "me".to_string()
        } else {
            label.unwrap_or(handle)
        };
        msgs.push(Msg {
            message_id: id,
            ts_ms: ts,
            speaker,
            text: body,
        });
    }
    Ok((kind, header, msgs))
}

/// Re-chunk one conversation, replacing its rows. Returns the chunk ids that
/// are new or whose text changed. Reads happen before the write transaction
/// so a large conversation never holds the write lock while loading.
pub fn chunk_conversation(
    c: &Connection,
    conversation_id: &str,
    p: &ChunkParams,
) -> augmentagent_store::rusqlite::Result<ChunkReport> {
    let (kind, header, msgs) = load_conversation(c, conversation_id)?;
    let chunks = chunk_messages(conversation_id, &header, &kind, &msgs, p);
    c.execute_batch("BEGIN IMMEDIATE")?;
    match write_chunks_in_tx(c, conversation_id, &chunks, p) {
        Ok(r) => {
            c.execute_batch("COMMIT")?;
            Ok(r)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Replace a conversation's chunk rows; caller owns the transaction.
fn write_chunks_in_tx(
    c: &Connection,
    conversation_id: &str,
    chunks: &[Chunk],
    p: &ChunkParams,
) -> augmentagent_store::rusqlite::Result<ChunkReport> {
    let existing: Vec<(String, String)> = {
        let mut s =
            c.prepare("SELECT chunk_id, text_hash FROM message_chunks WHERE conversation_id = ?1")?;
        let rows: Vec<(String, String)> = s
            .query_map([conversation_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        rows
    };
    let keep: BTreeSet<&str> = chunks.iter().map(|k| k.chunk_id.as_str()).collect();
    let mut report = ChunkReport {
        conversations: 1,
        chunks: chunks.len(),
        ..Default::default()
    };
    for (id, _) in existing
        .iter()
        .filter(|(id, _)| !keep.contains(id.as_str()))
    {
        c.execute("DELETE FROM message_chunks WHERE chunk_id = ?1", [id])?;
        report.removed += 1;
    }
    let version = p.version();
    let mut upsert = c.prepare(
        "INSERT INTO message_chunks (chunk_id, conversation_id, first_message_id, last_message_id,
             message_count, start_ts_ms, end_ts_ms, text_hash, sealed, params_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(chunk_id) DO UPDATE SET last_message_id = excluded.last_message_id,
             message_count = excluded.message_count, end_ts_ms = excluded.end_ts_ms,
             text_hash = excluded.text_hash, sealed = excluded.sealed,
             params_version = excluded.params_version",
    )?;
    for k in chunks {
        let prev = existing
            .iter()
            .find(|(id, _)| id == &k.chunk_id)
            .map(|(_, h)| h.as_str());
        if prev != Some(k.text_hash.as_str()) {
            report.changed.push(k.chunk_id.clone());
        }
        upsert.execute(params![
            k.chunk_id,
            k.conversation_id,
            k.first_message_id,
            k.last_message_id,
            k.message_count as i64,
            k.start_ts_ms,
            k.end_ts_ms,
            k.text_hash,
            k.sealed,
            version,
        ])?;
    }
    Ok(report)
}

/// Conversations touched by these message ids (for incremental re-chunking).
pub fn conversations_of(
    c: &Connection,
    message_ids: &[String],
) -> augmentagent_store::rusqlite::Result<BTreeSet<String>> {
    let mut stmt = c.prepare("SELECT conversation_id FROM message_index WHERE message_id = ?1")?;
    let mut out = BTreeSet::new();
    for id in message_ids {
        if let Some(cid) = stmt.query_row([id], |r| r.get::<_, String>(0)).optional()? {
            out.insert(cid);
        }
    }
    Ok(out)
}

/// Chunk every conversation, one short transaction each, pausing between
/// them so other writers are never starved.
pub fn chunk_all(
    store: &augmentagent_store::Store,
    p: &ChunkParams,
    pause: Duration,
) -> anyhow::Result<ChunkReport> {
    store.with_conn(ensure_tables)?;
    let convs: Vec<String> = store.with_conn(|c| {
        let mut s = c.prepare("SELECT DISTINCT conversation_id FROM message_index")?;
        let rows: Vec<String> = s.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
        Ok(rows)
    })?;
    // Many tiny transactions are worse for other writers than a few short
    // ones: every collision costs them a busy-handler backoff (up to ~100 ms).
    // So: compute chunks outside any transaction, write ~250 rows per
    // IMMEDIATE transaction, then pause so waiting writers get in.
    let mut report = ChunkReport::default();
    let mut pending: Vec<(String, Vec<Chunk>)> = Vec::new();
    let mut pending_rows = 0usize;
    let flush =
        |pending: &mut Vec<(String, Vec<Chunk>)>, report: &mut ChunkReport| -> anyhow::Result<()> {
            if pending.is_empty() {
                return Ok(());
            }
            let batch = std::mem::take(pending);
            let r = store.with_conn(|c| {
                c.execute_batch("BEGIN IMMEDIATE")?;
                let mut acc = ChunkReport::default();
                for (cid, chunks) in &batch {
                    match write_chunks_in_tx(c, cid, chunks, p) {
                        Ok(r) => acc.absorb(r),
                        Err(e) => {
                            let _ = c.execute_batch("ROLLBACK");
                            return Err(e);
                        }
                    }
                }
                c.execute_batch("COMMIT")?;
                Ok(acc)
            })?;
            report.absorb(r);
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
            Ok(())
        };
    for cid in &convs {
        let chunks = store.with_conn(|c| {
            let (kind, header, msgs) = load_conversation(c, cid)?;
            Ok(chunk_messages(cid, &header, &kind, &msgs, p))
        })?;
        pending_rows += chunks.len().max(1);
        pending.push((cid.clone(), chunks));
        if pending_rows >= WRITE_BATCH_ROWS {
            flush(&mut pending, &mut report)?;
            pending_rows = 0;
        }
    }
    flush(&mut pending, &mut report)?;
    // Chunks for conversations that no longer exist in the index.
    report.removed += store.with_conn(|c| {
        c.execute(
            "DELETE FROM message_chunks WHERE conversation_id NOT IN (SELECT DISTINCT conversation_id FROM message_index)",
            [],
        )
    })?;
    Ok(report)
}

/// Chunk rows written per transaction during a full run.
pub const WRITE_BATCH_ROWS: usize = 250;

/// Chunks stored under a different tunable version than the active one.
pub fn stale_params_count(
    c: &Connection,
    p: &ChunkParams,
) -> augmentagent_store::rusqlite::Result<i64> {
    c.query_row(
        "SELECT COUNT(*) FROM message_chunks WHERE params_version <> ?1",
        [p.version()],
        |r| r.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_messages::index::drain;
    use augmentagent_store::{Email, Store};

    fn m(id: &str, ts: i64, speaker: &str, text: &str) -> Msg {
        Msg {
            message_id: id.into(),
            ts_ms: ts,
            speaker: speaker.into(),
            text: text.into(),
        }
    }

    const MIN: i64 = 60_000;

    #[test]
    fn gap_threshold_splits_conversations() {
        let p = ChunkParams::default();
        let msgs = vec![
            m("a", 0, "me", "hi"),
            m("b", 5 * MIN, "pat", "hey"),
            m("c", 5 * MIN + 31 * MIN, "me", "later"),
        ];
        let out = chunk_messages("c1", "Pat", "dm", &msgs, &p);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].message_count, 2);
        assert_eq!(out[1].first_message_id, "c");
        assert!(out[0].sealed && !out[1].sealed, "only the tail is open");
    }

    #[test]
    fn message_cap_and_char_budget_split_long_runs() {
        let p = ChunkParams {
            gap_ms: i64::MAX / 2,
            max_messages: 3,
            max_chars: 100_000,
        };
        let msgs: Vec<Msg> = (0..7)
            .map(|i| m(&format!("m{i}"), i * MIN, "me", "x"))
            .collect();
        let out = chunk_messages("c1", "", "group", &msgs, &p);
        assert_eq!(
            out.iter().map(|c| c.message_count).collect::<Vec<_>>(),
            [3, 3, 1]
        );
        let p = ChunkParams {
            gap_ms: i64::MAX / 2,
            max_messages: 1000,
            max_chars: 60,
        };
        let msgs: Vec<Msg> = (0..5)
            .map(|i| m(&format!("m{i}"), i * MIN, "me", &"word ".repeat(5)))
            .collect();
        let out = chunk_messages("c1", "", "dm", &msgs, &p);
        assert!(out.len() >= 2);
        assert!(
            out.iter().all(|c| c.text.len() <= 60 + 40),
            "roughly within budget: {:?}",
            out.iter().map(|c| c.text.len()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn email_and_notes_are_one_chunk_each_and_sealed() {
        let p = ChunkParams::default();
        let msgs = vec![
            m("e1", 0, "pat", "quarterly numbers"),
            m("e2", 1, "pat", "follow-up"),
        ];
        for kind in ["email", "note", "meeting"] {
            let out = chunk_messages("t", "Subject", kind, &msgs, &p);
            assert_eq!(out.len(), 2, "{kind}");
            assert!(out.iter().all(|c| c.sealed), "{kind}");
        }
    }

    #[test]
    fn chunk_ids_are_stable_as_a_chunk_grows_and_sealed_chunks_never_change() {
        let p = ChunkParams::default();
        let base = vec![
            m("a", 0, "me", "hi"),
            m("b", MIN, "pat", "hey"),
            m("c", 60 * MIN, "me", "later"),
        ];
        let before = chunk_messages("c1", "Pat", "dm", &base, &p);
        let mut grown = base.clone();
        grown.push(m("d", 61 * MIN, "pat", "ok"));
        let after = chunk_messages("c1", "Pat", "dm", &grown, &p);
        assert_eq!(before[0], after[0], "sealed chunk identical");
        assert_eq!(before[1].chunk_id, after[1].chunk_id, "tail keeps its id");
        assert_ne!(before[1].text_hash, after[1].text_hash, "tail text changed");
        assert_eq!(after[1].message_count, 2);
    }

    #[test]
    fn chunk_text_carries_header_and_speakers_and_is_deterministic() {
        let p = ChunkParams::default();
        let msgs = vec![m("a", 0, "me", " hi "), m("b", MIN, "Pat", "hey")];
        let out = chunk_messages("c1", "#general Acme", "channel", &msgs, &p);
        assert_eq!(out[0].text, "#general Acme\nme: hi\nPat: hey");
        assert_eq!(
            out,
            chunk_messages("c1", "#general Acme", "channel", &msgs, &p)
        );
    }

    #[test]
    fn every_message_belongs_to_exactly_one_chunk() {
        let mut seed: u64 = 7;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let n = (next() % 120) as usize;
            let mut ts = 0i64;
            let msgs: Vec<Msg> = (0..n)
                .map(|i| {
                    ts += (next() % (90 * MIN as u64)) as i64;
                    m(
                        &format!("m{i}"),
                        ts,
                        if next() % 2 == 0 { "me" } else { "pat" },
                        &"w ".repeat((next() % 200) as usize),
                    )
                })
                .collect();
            let p = ChunkParams {
                gap_ms: 30 * MIN,
                max_messages: 1 + (next() % 20) as usize,
                max_chars: 200 + (next() % 3000) as usize,
            };
            let kind = if next() % 3 == 0 { "email" } else { "dm" };
            let chunks = chunk_messages("c", "", kind, &msgs, &p);
            let mut covered: Vec<&str> = Vec::new();
            for c in &chunks {
                let first = msgs
                    .iter()
                    .position(|x| x.message_id == c.first_message_id)
                    .unwrap();
                let last = msgs
                    .iter()
                    .position(|x| x.message_id == c.last_message_id)
                    .unwrap();
                assert_eq!(last + 1 - first, c.message_count);
                covered.extend(msgs[first..=last].iter().map(|x| x.message_id.as_str()));
            }
            let expected: Vec<&str> = msgs.iter().map(|x| x.message_id.as_str()).collect();
            assert_eq!(
                covered, expected,
                "chunks must tile the conversation in order"
            );
            let ids: BTreeSet<&str> = chunks.iter().map(|c| c.chunk_id.as_str()).collect();
            assert_eq!(ids.len(), chunks.len(), "chunk ids unique");
        }
    }

    // ---- store-backed ----

    fn store() -> (tempfile::TempDir, Store) {
        let d = tempfile::tempdir().unwrap();
        let s = Store::open(d.path().join("t.db")).unwrap();
        (d, s)
    }

    fn put(s: &Store, id: &str, thread: &str, from: &str, body: &str, ts: &str) {
        s.upsert_email(&Email {
            message_id: id.into(),
            thread_id: Some(thread.into()),
            from: from.into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: "iMessage: Pat".into(),
            body: body.into(),
            date: ts.into(),
            account_entity_id: None,
            platform: "imessage".into(),
            kind: "dm".into(),
        })
        .unwrap();
        drain(s, 100, Duration::ZERO).unwrap();
    }

    #[test]
    fn new_message_extends_the_tail_and_unchanged_conversations_produce_no_work() {
        let (_d, s) = store();
        let p = ChunkParams::default();
        put(
            &s,
            "a",
            "imessage:+15555550100",
            "+15555550100",
            "hey there",
            "2026-01-01T10:00:00Z",
        );
        put(
            &s,
            "b",
            "imessage:+15555550100",
            "me",
            "hi!",
            "2026-01-01T10:01:00Z",
        );
        let r = chunk_all(&s, &p, Duration::ZERO).unwrap();
        assert_eq!((r.conversations, r.chunks, r.changed.len()), (1, 1, 1));
        let again = chunk_all(&s, &p, Duration::ZERO).unwrap();
        assert!(
            again.changed.is_empty(),
            "no text changed → nothing to re-embed"
        );
        put(
            &s,
            "c",
            "imessage:+15555550100",
            "+15555550100",
            "lunch?",
            "2026-01-01T10:05:00Z",
        );
        let r = s
            .with_conn(|c| chunk_conversation(c, "imessage:+15555550100", &p))
            .unwrap();
        assert_eq!(r.changed.len(), 1, "tail re-embeds");
        assert_eq!(r.chunks, 1);
        let text_uses_speakers: String = s
            .with_conn(|c| {
                c.query_row("SELECT message_count FROM message_chunks", [], |r| {
                    r.get::<_, i64>(0)
                })
            })
            .unwrap()
            .to_string();
        assert_eq!(text_uses_speakers, "3");
    }

    #[test]
    fn deleting_a_message_rechunks_only_its_conversation_and_orphans_are_removed() {
        let (_d, s) = store();
        let p = ChunkParams {
            gap_ms: 30 * MIN,
            max_messages: 2,
            max_chars: 10_000,
        };
        for (i, t) in ["10:00", "10:01", "10:02", "10:03"].iter().enumerate() {
            put(
                &s,
                &format!("a{i}"),
                "imessage:+15555550100",
                "me",
                "x",
                &format!("2026-01-01T{t}:00Z"),
            );
        }
        put(
            &s,
            "z",
            "imessage:+15555550199",
            "me",
            "other",
            "2026-01-01T10:00:00Z",
        );
        chunk_all(&s, &p, Duration::ZERO).unwrap();
        let count = |s: &Store| -> i64 {
            s.with_conn(|c| c.query_row("SELECT COUNT(*) FROM message_chunks", [], |r| r.get(0)))
                .unwrap()
        };
        assert_eq!(count(&s), 3);
        s.with_conn(|c| c.execute("DELETE FROM emails WHERE messageId = 'a1'", []))
            .unwrap();
        drain(&s, 10, Duration::ZERO).unwrap();
        let touched = s
            .with_conn(|c| conversations_of(c, &["a0".to_string(), "z".to_string()]))
            .unwrap();
        assert_eq!(touched.len(), 2);
        let r = s
            .with_conn(|c| chunk_conversation(c, "imessage:+15555550100", &p))
            .unwrap();
        assert_eq!(r.removed, 1, "one old chunk id no longer exists");
        assert_eq!(
            count(&s),
            3,
            "conversation now 2 chunks + the other conversation"
        );
        s.with_conn(|c| c.execute("DELETE FROM emails WHERE messageId = 'z'", []))
            .unwrap();
        drain(&s, 10, Duration::ZERO).unwrap();
        let r = chunk_all(&s, &p, Duration::ZERO).unwrap();
        assert_eq!(r.removed, 1, "orphaned conversation's chunk removed");
    }

    #[test]
    fn changed_tunables_are_detected_as_stale() {
        let (_d, s) = store();
        put(
            &s,
            "a",
            "imessage:+15555550100",
            "me",
            "x",
            "2026-01-01T10:00:00Z",
        );
        let p = ChunkParams::default();
        chunk_all(&s, &p, Duration::ZERO).unwrap();
        assert_eq!(s.with_conn(|c| stale_params_count(c, &p)).unwrap(), 0);
        let p2 = ChunkParams {
            gap_ms: 5 * MIN,
            ..p
        };
        assert_eq!(s.with_conn(|c| stale_params_count(c, &p2)).unwrap(), 1);
    }
}
