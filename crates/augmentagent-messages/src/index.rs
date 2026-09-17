//! Queue draining, backfill enqueueing and health checks for `message_index`.
//!
//! Triggers on `emails` (created in `Store::migrate`) put every inserted,
//! updated or deleted messageId into `message_index_queue`. [`drain`] turns a
//! batch of queued ids into index rows inside one short `IMMEDIATE`
//! transaction, so a concurrent writer in another process either lands
//! before the batch (and is included) or re-queues after it (with a new
//! `seq`, so it is not lost).

use std::collections::BTreeMap;

use augmentagent_store::rusqlite::{params, Connection, OptionalExtension};
use augmentagent_store::Store;
use serde::Serialize;

use crate::extract::{extract, EmailRowView, OwnerHandles, EXTRACTOR_VERSION};
use crate::handles;

#[derive(Debug, Default, Serialize, Clone, PartialEq, Eq)]
pub struct DrainReport {
    pub indexed: usize,
    pub removed: usize,
    pub ts_fallback: usize,
    pub by_platform: BTreeMap<String, usize>,
    pub remaining: i64,
}

impl DrainReport {
    fn absorb(&mut self, other: DrainReport) {
        self.indexed += other.indexed;
        self.removed += other.removed;
        self.ts_fallback += other.ts_fallback;
        for (k, v) in other.by_platform {
            *self.by_platform.entry(k).or_default() += v;
        }
        self.remaining = other.remaining;
    }
}

#[derive(Debug, Default, Serialize, Clone, PartialEq, Eq)]
pub struct IndexHealth {
    pub emails: i64,
    pub indexed: i64,
    pub missing: i64,
    pub stale: i64,
    pub queued: i64,
    /// Index rows with no full-text entry.
    pub fts_missing: i64,
}

impl IndexHealth {
    pub fn is_complete(&self) -> bool {
        self.missing == 0 && self.stale == 0 && self.queued == 0 && self.fts_missing == 0
    }
}

/// Owner handles: connected mailbox addresses. Platform-specific owner ids
/// (e.g. Discord) are read per row by the extractor.
pub fn load_owner(conn: &Connection) -> augmentagent_store::rusqlite::Result<OwnerHandles> {
    let mut stmt = conn.prepare(
        "SELECT email FROM gmail_accounts WHERE email IS NOT NULL AND TRIM(email) <> ''",
    )?;
    let handles = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(Result::ok)
        .filter_map(|e| handles::identity("email", &e))
        .collect();
    Ok(OwnerHandles { handles })
}

/// Queue every `emails` row with no index row or an index row from an older
/// extractor. Optional platform filter. Returns how many ids were queued.
/// Existing queue entries keep their position.
///
/// Works through `emails` in rowid ranges of [`ENQUEUE_CHUNK`] rows, one short
/// statement each, pausing `pause` between them. SQLite's busy handler backs
/// off up to ~100 ms between retries, so a writer that re-takes the lock
/// immediately starves other processes; pass [`YIELD_PAUSE`] when anything
/// else may be writing.
pub fn enqueue_stale(
    store: &Store,
    platform: Option<&str>,
    pause: std::time::Duration,
) -> anyhow::Result<usize> {
    let max_rowid: i64 = store.with_conn(|c| {
        c.query_row("SELECT COALESCE(MAX(rowid), 0) FROM emails", [], |r| {
            r.get(0)
        })
    })?;
    let mut queued = 0usize;
    let mut start = 0i64;
    while start < max_rowid {
        let end = start + ENQUEUE_CHUNK;
        queued += store.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO message_index_queue(message_id) \
                 SELECT e.messageId FROM emails e \
                 LEFT JOIN message_index mi ON mi.message_id = e.messageId \
                 WHERE e.rowid > ?3 AND e.rowid <= ?4 \
                   AND (mi.message_id IS NULL OR mi.extractor_version < ?1) \
                   AND (?2 IS NULL OR e.platform = ?2) \
                 ORDER BY e.rowid",
                params![EXTRACTOR_VERSION, platform, start, end],
            )
        })?;
        start = end;
        if start < max_rowid && !pause.is_zero() {
            std::thread::sleep(pause);
        }
    }
    Ok(queued)
}

/// Pause between write batches that lets other processes' busy handlers win
/// the lock (their backoff tops out around 100 ms).
pub const YIELD_PAUSE: std::time::Duration = std::time::Duration::from_millis(120);

/// Body bytes read per drain transaction before the batch ends early.
pub const BATCH_BODY_BUDGET: usize = 2 * 1024 * 1024;

/// Rows scanned per enqueue statement.
pub const ENQUEUE_CHUNK: i64 = 2_000;

/// Process up to `batch` queued ids. Call repeatedly until `remaining == 0`.
pub fn drain_batch(store: &Store, batch: usize) -> anyhow::Result<DrainReport> {
    Ok(store.with_conn(|c| drain_batch_conn(c, batch))?)
}

/// Drain the whole queue in batches, releasing the store lock and pausing
/// `pause` between them so other writers (daemon, dashboard) get in.
pub fn drain(
    store: &Store,
    batch: usize,
    pause: std::time::Duration,
) -> anyhow::Result<DrainReport> {
    let mut total = DrainReport::default();
    loop {
        let r = drain_batch(store, batch)?;
        let done = r.indexed + r.removed == 0 || r.remaining == 0;
        total.absorb(r);
        if done {
            return Ok(total);
        }
        if !pause.is_zero() {
            std::thread::sleep(pause);
        }
    }
}

fn drain_batch_conn(
    c: &Connection,
    batch: usize,
) -> augmentagent_store::rusqlite::Result<DrainReport> {
    c.execute_batch("BEGIN IMMEDIATE")?;
    match drain_in_tx(c, batch) {
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

fn drain_in_tx(c: &Connection, batch: usize) -> augmentagent_store::rusqlite::Result<DrainReport> {
    let owner = load_owner(c)?;
    let queued: Vec<(i64, String)> = {
        let mut stmt =
            c.prepare("SELECT seq, message_id FROM message_index_queue ORDER BY seq LIMIT ?1")?;
        let rows = stmt.query_map([batch as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut report = DrainReport::default();
    let mut body_bytes = 0usize;
    let mut load = c.prepare(
        "SELECT messageId, threadId, fromEmail, subject, COALESCE(body, ''), receivedAt, \
                accountEntityId, firstSeenAt, platform, kind \
           FROM emails WHERE messageId = ?1",
    )?;
    let mut upsert = c.prepare(
        "INSERT INTO message_index (message_id, platform, conv_kind, conversation_id, \
            conversation_title, container, sender_handle, sender_label, counterpart_handle, \
            from_me, ts_ms, ts_fallback, has_attachment, extractor_version) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT(message_id) DO UPDATE SET platform = excluded.platform, \
            conv_kind = excluded.conv_kind, conversation_id = excluded.conversation_id, \
            conversation_title = excluded.conversation_title, container = excluded.container, \
            sender_handle = excluded.sender_handle, sender_label = excluded.sender_label, \
            counterpart_handle = excluded.counterpart_handle, from_me = excluded.from_me, \
            ts_ms = excluded.ts_ms, ts_fallback = excluded.ts_fallback, \
            has_attachment = excluded.has_attachment, \
            extractor_version = excluded.extractor_version",
    )?;
    let mut delete_index = c.prepare("DELETE FROM message_index WHERE message_id = ?1")?;
    let mut rowid_of = c.prepare("SELECT rowid FROM message_index WHERE message_id = ?1")?;
    let mut dequeue = c.prepare("DELETE FROM message_index_queue WHERE seq = ?1")?;

    for (seq, id) in queued {
        // Large HTML mail makes a row-count batch hold the write lock far
        // longer; stop early once the batch has read enough text.
        if body_bytes >= BATCH_BODY_BUDGET {
            break;
        }
        let row = load
            .query_row([&id], |r| {
                Ok(EmailRowView {
                    message_id: r.get(0)?,
                    thread_id: r.get(1)?,
                    from: r.get(2)?,
                    subject: r.get(3)?,
                    body: r.get(4)?,
                    received_at: r.get(5)?,
                    account_entity_id: r.get(6)?,
                    first_seen_ms: r.get(7)?,
                    platform: r.get(8)?,
                    kind: r.get(9)?,
                })
            })
            .optional()?;
        match row {
            None => {
                if let Some(rowid) = rowid_of
                    .query_row([&id], |r| r.get::<_, i64>(0))
                    .optional()?
                {
                    crate::fts::delete(c, rowid)?;
                }
                delete_index.execute([&id])?;
                report.removed += 1;
            }
            Some(row) => {
                body_bytes += row.body.len();
                let f = extract(&row, &owner);
                upsert.execute(params![
                    row.message_id,
                    f.platform,
                    f.conv_kind,
                    f.conversation_id,
                    f.conversation_title,
                    f.container,
                    f.sender_handle,
                    f.sender_label,
                    f.counterpart_handle,
                    f.from_me,
                    f.ts_ms,
                    f.ts_fallback,
                    f.has_attachment,
                    EXTRACTOR_VERSION,
                ])?;
                let rowid: i64 = rowid_of.query_row([&row.message_id], |r| r.get(0))?;
                let doc = crate::fts::prepare(
                    &row.platform,
                    f.conversation_title.as_deref(),
                    f.container.as_deref(),
                    &row.subject,
                    &row.body,
                );
                crate::fts::upsert(c, rowid, &doc)?;
                report.indexed += 1;
                if f.ts_fallback {
                    report.ts_fallback += 1;
                }
                *report.by_platform.entry(f.platform).or_default() += 1;
            }
        }
        dequeue.execute([seq])?;
    }
    report.remaining = c.query_row("SELECT COUNT(*) FROM message_index_queue", [], |r| r.get(0))?;
    Ok(report)
}

pub fn check(store: &Store) -> anyhow::Result<IndexHealth> {
    Ok(store.with_conn(|c| {
        Ok(IndexHealth {
            emails: c.query_row("SELECT COUNT(*) FROM emails", [], |r| r.get(0))?,
            indexed: c.query_row("SELECT COUNT(*) FROM message_index", [], |r| r.get(0))?,
            missing: c.query_row(
                "SELECT COUNT(*) FROM emails e \
                 LEFT JOIN message_index mi ON mi.message_id = e.messageId \
                 WHERE mi.message_id IS NULL",
                [],
                |r| r.get(0),
            )?,
            stale: c.query_row(
                "SELECT COUNT(*) FROM message_index WHERE extractor_version < ?1",
                [EXTRACTOR_VERSION],
                |r| r.get(0),
            )?,
            queued: c.query_row("SELECT COUNT(*) FROM message_index_queue", [], |r| r.get(0))?,
            fts_missing: c.query_row(
                "SELECT COUNT(*) FROM message_index mi \
                 WHERE NOT EXISTS (SELECT 1 FROM message_fts f WHERE f.rowid = mi.rowid)",
                [],
                |r| r.get(0),
            )?,
        })
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use augmentagent_store::Email;

    const Z: std::time::Duration = std::time::Duration::ZERO;

    #[test]
    fn large_bodies_end_a_batch_early_without_losing_rows() {
        let (_d, s) = store();
        // Each body is just over half the budget: two rows exhaust it.
        let big = "x ".repeat(BATCH_BODY_BUDGET / 4 + 10);
        for i in 0..5 {
            let mut e = email(&format!("m{i}"), "gmail", "a@example.com", "t", "s");
            e.body = big.clone();
            s.upsert_email(&e).unwrap();
        }
        let first = drain_batch(&s, 100).unwrap();
        assert_eq!(
            (first.indexed, first.remaining),
            (2, 3),
            "budget ends the batch after 2 big rows"
        );
        drain(&s, 100, Z).unwrap();
        assert!(check(&s).unwrap().is_complete());
    }

    #[test]
    fn enqueue_covers_every_rowid_chunk_boundary() {
        let (_d, s) = store();
        let n = (ENQUEUE_CHUNK * 2 + 3) as usize;
        s.with_conn(|c| {
            c.execute_batch("BEGIN")?;
            for i in 0..n {
                c.execute(
                    "INSERT INTO emails (messageId, fromEmail, subject, firstSeenAt, platform, kind) \
                     VALUES (?1, 'x@example.com', 's', 1, 'gmail', 'dm')",
                    [format!("m{i}")],
                )?;
            }
            c.execute_batch("DELETE FROM message_index_queue; COMMIT")
        })
        .unwrap();
        assert_eq!(enqueue_stale(&s, None, Z).unwrap(), n);
        drain(&s, 1000, Z).unwrap();
        assert!(check(&s).unwrap().is_complete());
    }

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("t.db")).unwrap();
        (dir, s)
    }

    fn email(id: &str, platform: &str, from: &str, thread: &str, subject: &str) -> Email {
        Email {
            message_id: id.into(),
            thread_id: Some(thread.into()),
            from: from.into(),
            to: String::new(),
            cc: String::new(),
            attachments: vec![],
            subject: subject.into(),
            body: "hello".into(),
            date: "2026-08-26T14:32:05-04:00".into(),
            account_entity_id: None,
            platform: platform.into(),
            kind: "dm".into(),
        }
    }

    fn one(s: &Store, sql: &str) -> i64 {
        s.with_conn(|c| c.query_row(sql, [], |r| r.get(0))).unwrap()
    }

    #[test]
    fn migration_is_idempotent_and_additive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        {
            let s = Store::open(&path).unwrap();
            s.upsert_email(&email("a", "gmail", "x@example.com", "t", "s"))
                .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let s2 = Store::open(&path).unwrap();
        drop(s2);
        assert_eq!(one(&s, "SELECT COUNT(*) FROM emails"), 1);
        assert_eq!(
            one(
                &s,
                "SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'trg_emails_message_index_%'"
            ),
            3
        );
    }

    #[test]
    fn inserts_updates_and_deletes_are_queued_by_triggers() {
        let (_d, s) = store();
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "s"))
            .unwrap();
        s.upsert_email(&email("b", "gmail", "y@example.com", "t", "s"))
            .unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index_queue"), 2);
        drain(&s, 100, Z).unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index_queue"), 0);
        // Triage marking doesn't touch indexed columns → not re-queued.
        s.mark_email_processed("a", augmentagent_store::TriageResult::DigestOnly)
            .unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index_queue"), 0);
        // Re-writing identical content (the email poller re-upserts unread
        // mail every tick) must not re-queue.
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "s"))
            .unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index_queue"), 0);
        // Content update re-queues.
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "new subject"))
            .unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index_queue"), 1);
        s.with_conn(|c| c.execute("DELETE FROM emails WHERE messageId='b'", []))
            .unwrap();
        let r = drain(&s, 100, Z).unwrap();
        assert_eq!((r.indexed, r.removed, r.remaining), (1, 1, 0));
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index"), 1);
        let title: String = s
            .with_conn(|c| {
                c.query_row(
                    "SELECT conversation_title FROM message_index WHERE message_id='a'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(title, "new subject");
    }

    #[test]
    fn second_drain_refreshes_not_duplicates() {
        let (_d, s) = store();
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "s"))
            .unwrap();
        drain(&s, 10, Z).unwrap();
        s.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO message_index_queue(message_id) VALUES ('a')",
                [],
            )
        })
        .unwrap();
        drain(&s, 10, Z).unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index"), 1);
    }

    #[test]
    fn owner_mailboxes_mark_sent_mail_from_me() {
        let (_d, s) = store();
        s.with_conn(|c| {
            c.execute(
                "INSERT INTO gmail_accounts (id, connectionId, email, entityId, createdAt) \
                 VALUES ('g1', 'c1', 'Owner@Example.com', 'e1', 1)",
                [],
            )
        })
        .unwrap();
        s.upsert_email(&email("a", "gmail", "Owner <owner@example.com>", "t", "s"))
            .unwrap();
        drain(&s, 10, Z).unwrap();
        assert_eq!(
            one(&s, "SELECT from_me FROM message_index WHERE message_id='a'"),
            1
        );
    }

    #[test]
    fn enqueue_stale_backfills_rows_that_predate_the_triggers_and_old_versions() {
        let (_d, s) = store();
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "s"))
            .unwrap();
        s.upsert_email(&email(
            "b",
            "imessage",
            "me",
            "imessage:+14155550123",
            "iMessage: J",
        ))
        .unwrap();
        // Simulate rows written before this feature: no queue entries.
        s.with_conn(|c| c.execute("DELETE FROM message_index_queue", []))
            .unwrap();
        let h = check(&s).unwrap();
        assert_eq!((h.emails, h.indexed, h.missing), (2, 0, 2));
        assert!(!h.is_complete());

        assert_eq!(enqueue_stale(&s, Some("imessage"), Z).unwrap(), 1);
        drain(&s, 1, Z).unwrap();
        assert_eq!(check(&s).unwrap().missing, 1);
        assert_eq!(enqueue_stale(&s, None, Z).unwrap(), 1);
        let r = drain(&s, 1, Z).unwrap();
        assert_eq!(r.indexed, 1);
        assert!(check(&s).unwrap().is_complete());
        // Idempotent: nothing left to queue.
        assert_eq!(enqueue_stale(&s, None, Z).unwrap(), 0);

        s.with_conn(|c| c.execute("UPDATE message_index SET extractor_version = 0", []))
            .unwrap();
        assert_eq!(check(&s).unwrap().stale, 2);
        assert_eq!(enqueue_stale(&s, None, Z).unwrap(), 2);
        drain(&s, 500, Z).unwrap();
        assert!(check(&s).unwrap().is_complete());
    }

    #[test]
    fn update_before_drain_collapses_to_one_queue_entry_with_latest_content() {
        let (_d, s) = store();
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "old"))
            .unwrap();
        // A second writer updates the row after it was queued but before the
        // drain: the trigger's REPLACE gives it a new seq, still one entry.
        s.upsert_email(&email("a", "gmail", "x@example.com", "t", "newer"))
            .unwrap();
        assert_eq!(one(&s, "SELECT COUNT(*) FROM message_index_queue"), 1);
        drain(&s, 10, Z).unwrap();
        let title: String = s
            .with_conn(|c| {
                c.query_row("SELECT conversation_title FROM message_index", [], |r| {
                    r.get(0)
                })
            })
            .unwrap();
        assert_eq!(title, "newer");
    }

    #[test]
    fn index_rows_never_block_email_writes() {
        // Garbage in every column still indexes (no panic, no failed insert).
        let (_d, s) = store();
        let mut e = email("z", "discord", "<<<>>>", "", "Discord: [[[ #");
        e.date = "not a date".into();
        assert!(s.upsert_email(&e).unwrap());
        let r = drain(&s, 10, Z).unwrap();
        assert_eq!((r.indexed, r.ts_fallback), (1, 1));
    }
}

#[cfg(test)]
mod bench {
    use augmentagent_store::{Email, Store};
    use std::time::Instant;

    const DROP: &str = "DROP TRIGGER trg_emails_message_index_insert; \
         DROP TRIGGER trg_emails_message_index_update; \
         DROP TRIGGER trg_emails_message_index_delete; DROP TABLE message_index_queue;";

    fn run(setup: &str, n: usize) -> f64 {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("b.db")).unwrap();
        s.with_conn(|c| c.execute_batch(setup)).unwrap();
        let body = "Hi team, notes from today's call follow. ".repeat(60);
        let t = Instant::now();
        for i in 0..n {
            s.upsert_email(&Email {
                message_id: format!("m{i}"),
                thread_id: Some(format!("t{}", i % 50)),
                from: "Pat <pat@example.com>".into(),
                to: String::new(),
                cc: String::new(),
                attachments: vec![],
                subject: "Weekly notes".into(),
                body: body.clone(),
                date: "2026-08-26T14:32:05-04:00".into(),
                account_entity_id: Some("acct".into()),
                platform: "gmail".into(),
                kind: "dm".into(),
            })
            .unwrap();
        }
        t.elapsed().as_secs_f64() / n as f64 * 1e6
    }

    /// `cargo test -p augmentagent-messages --release -- --ignored bench --nocapture`
    #[test]
    #[ignore]
    fn upsert_email_trigger_overhead() {
        let n = 4_000;
        let variants = [
            ("no triggers", DROP.to_string()),
            ("with message-index triggers", String::new()),
        ];
        let mut best = vec![f64::MAX; variants.len()];
        for round in 0..4 {
            // Interleave variants (and rotate the start) so disk / fsync
            // state doesn't bias whichever variant runs later.
            for k in 0..variants.len() {
                let i = (k + round) % variants.len();
                best[i] = best[i].min(run(&variants[i].1, n));
            }
        }
        for (i, (name, _)) in variants.iter().enumerate() {
            println!("{name:40} {:8.1} us/row", best[i]);
        }
    }
}
