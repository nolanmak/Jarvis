//! #1296 — one record per Slack message, whichever path saw it first.
//!
//! A subscribed Slack message can arrive three ways: the Composio poll, a
//! live Socket Mode event (where the app is a member), or the bounded
//! catch-up after the host slept. They share one identity, the
//! [`SlackMessageKey`] `(team, channel, ts)`, whose stored form is the
//! `<channel>:<ts>` message ID every Slack row has always had
//! (`emails.messageId`, the durable inbox `event_id`, the reply target). No
//! existing ID or poll cursor changes.
//!
//! `slack_ingest_ledger` has one row per message ever seen:
//!
//! * [`Store::record_slack_message`] inserts the ledger row **and** the
//!   `emails` row in one `IMMEDIATE` transaction. Only the first caller gets
//!   [`SlackRecordOutcome::Stored`]; every later sighting is a
//!   [`SlackRecordOutcome::Duplicate`]. A message stored before this table
//!   existed (an `emails` row with no ledger row) is adopted as
//!   [`SlackIngestSource::Legacy`] and is never stored again.
//! * [`Store::claim_slack_triage`] is the single triage decision: it succeeds
//!   for one caller while the message is unprocessed, and again only after
//!   `stale_after_ms` (a daemon that died mid-triage).
//! * Edits ([`Store::apply_slack_edit`]) apply once per Slack `edited.ts`;
//!   deletes ([`Store::apply_slack_delete`]) remove the stored text once and
//!   leave a tombstone, so a later poll or catch-up of the same message never
//!   stores it again.
//! * A thread reply keeps its parent `thread_ts`, so it attaches to the
//!   parent (`<channel>:<thread_ts>`) whichever path stored either one.
//! * [`Store::rename_slack_conversation`] updates display names (the
//!   subscription, reply targets, and the search index title) and never an
//!   ID.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::models::Email;
use crate::store::{Store, StoreError, StoreResult};

/// Which path saw a message first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackIngestSource {
    /// A Socket Mode event.
    Live,
    /// The Composio poll.
    Poll,
    /// History fetched after a sleep or reconnect.
    CatchUp,
    /// Stored before the ledger existed.
    Legacy,
}

impl SlackIngestSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Poll => "poll",
            Self::CatchUp => "catch_up",
            Self::Legacy => "legacy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "live" => Self::Live,
            "poll" => Self::Poll,
            "catch_up" => Self::CatchUp,
            "legacy" => Self::Legacy,
            _ => return None,
        })
    }
}

/// `(team, channel, ts)`: the identity both ingestion paths share.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SlackMessageKey {
    team_id: String,
    channel_id: String,
    ts: String,
}

impl SlackMessageKey {
    pub fn new(team_id: &str, channel_id: &str, ts: &str) -> StoreResult<Self> {
        for (v, name) in [
            (team_id, "slack team id"),
            (channel_id, "slack channel id"),
            (ts, "slack ts"),
        ] {
            if v.trim().is_empty() || v.contains(':') {
                return Err(StoreError::InvalidInput(format!("{name} `{v}` is invalid")));
            }
        }
        if ts_order(ts).is_none() {
            return Err(StoreError::InvalidInput(format!(
                "slack ts `{ts}` is invalid"
            )));
        }
        Ok(Self {
            team_id: team_id.to_string(),
            channel_id: channel_id.to_string(),
            ts: ts.to_string(),
        })
    }

    pub fn team_id(&self) -> &str {
        &self.team_id
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    pub fn ts(&self) -> &str {
        &self.ts
    }

    /// `<channel>:<ts>`: the stored message ID (unchanged from before the
    /// ledger existed).
    pub fn message_id(&self) -> String {
        format!("{}:{}", self.channel_id, self.ts)
    }
}

/// `"1700000000.000100"` → `(1700000000, 100)`, so timestamps compare
/// exactly (a float would lose the microseconds).
pub fn ts_order(ts: &str) -> Option<(u64, u64)> {
    let (secs, frac) = ts.split_once('.').unwrap_or((ts, "0"));
    if secs.is_empty() || frac.len() > 9 {
        return None;
    }
    let secs: u64 = secs.parse().ok()?;
    let micros: u64 = format!("{frac:0<6}").get(..6)?.parse().ok()?;
    Some((secs, micros))
}

/// `"1700000000.000100"` → milliseconds since the epoch.
pub fn ts_to_ms(ts: &str) -> Option<i64> {
    let (secs, micros) = ts_order(ts)?;
    i64::try_from(secs)
        .ok()?
        .checked_mul(1000)?
        .checked_add((micros / 1000) as i64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackRecordOutcome {
    /// First sighting: the `emails` row was written now.
    Stored,
    /// Already stored, by `first_source`.
    Duplicate { first_source: SlackIngestSource },
    /// The message was deleted; nothing is stored.
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackEditOutcome {
    Applied,
    /// This edit (or a newer one) is already applied.
    AlreadyApplied,
    /// No such message is stored (the edit is not a new message).
    Unknown,
    /// The message was deleted.
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackDeleteOutcome {
    /// The stored text was removed now.
    Applied,
    AlreadyDeleted,
    /// Never stored: a tombstone keeps a later sighting from storing it.
    Tombstoned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackLedgerRow {
    pub message_id: String,
    pub team_id: String,
    pub channel_id: String,
    pub ts: String,
    /// Parent `ts` for a thread reply.
    pub thread_ts: Option<String>,
    pub first_source: SlackIngestSource,
    pub edit_ts: Option<String>,
    pub edit_count: i64,
    pub deleted: bool,
    pub triage_claimed_at_ms: Option<i64>,
    pub triage_claims: i64,
}

impl SlackLedgerRow {
    /// `<channel>:<thread_ts>` of the parent, for a thread reply.
    pub fn parent_message_id(&self) -> Option<String> {
        self.thread_ts
            .as_deref()
            .filter(|t| *t != self.ts)
            .map(|t| format!("{}:{t}", self.channel_id))
    }
}

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    let upgrading: bool = !conn
        .prepare(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'slack_ingest_ledger'",
        )?
        .exists([])?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS slack_ingest_ledger (\
             message_id           TEXT PRIMARY KEY,\
             team_id              TEXT NOT NULL,\
             channel_id           TEXT NOT NULL,\
             ts                   TEXT NOT NULL,\
             thread_ts            TEXT,\
             first_source         TEXT NOT NULL \
                                  CHECK(first_source IN ('live', 'poll', 'catch_up', 'legacy')),\
             edit_ts              TEXT,\
             edit_count           INTEGER NOT NULL DEFAULT 0,\
             deleted_at_ms        INTEGER,\
             triage_claimed_at_ms INTEGER,\
             triage_claims        INTEGER NOT NULL DEFAULT 0,\
             created_at_ms        INTEGER NOT NULL,\
             updated_at_ms        INTEGER NOT NULL\
         );\
         CREATE INDEX IF NOT EXISTS idx_slack_ingest_thread \
         ON slack_ingest_ledger(team_id, channel_id, thread_ts);",
    )?;
    if upgrading {
        // Once, on upgrade: re-index the Slack rows already stored so search
        // gets their channel names, DM/channel kinds and Slack times. IDs are
        // untouched; the daemon's index drain does the work.
        conn.execute(
            "INSERT OR IGNORE INTO message_index_queue(message_id) \
             SELECT messageId FROM emails WHERE platform = 'slack'",
            [],
        )?;
    }
    Ok(())
}

fn tx<T>(store: &Store, f: impl FnOnce(&Connection) -> StoreResult<T>) -> StoreResult<T> {
    store.with_conn(|conn| {
        Ok((|| {
            let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })())
    })?
}

const LEDGER_COLUMNS: &str = "message_id, team_id, channel_id, ts, thread_ts, first_source, \
     edit_ts, edit_count, deleted_at_ms, triage_claimed_at_ms, triage_claims";

fn ledger_row(c: &Connection, message_id: &str) -> rusqlite::Result<Option<SlackLedgerRow>> {
    c.query_row(
        &format!("SELECT {LEDGER_COLUMNS} FROM slack_ingest_ledger WHERE message_id = ?1"),
        params![message_id],
        |r| {
            let source: String = r.get(5)?;
            Ok(SlackLedgerRow {
                message_id: r.get(0)?,
                team_id: r.get(1)?,
                channel_id: r.get(2)?,
                ts: r.get(3)?,
                thread_ts: r.get(4)?,
                first_source: SlackIngestSource::parse(&source).ok_or_else(|| {
                    rusqlite::Error::InvalidColumnType(5, source, rusqlite::types::Type::Text)
                })?,
                edit_ts: r.get(6)?,
                edit_count: r.get(7)?,
                deleted: r.get::<_, Option<i64>>(8)?.is_some(),
                triage_claimed_at_ms: r.get(9)?,
                triage_claims: r.get(10)?,
            })
        },
    )
    .optional()
}

fn email_exists(c: &Connection, message_id: &str) -> rusqlite::Result<bool> {
    c.prepare("SELECT 1 FROM emails WHERE messageId = ?1")?
        .exists(params![message_id])
}

fn insert_ledger(
    c: &Connection,
    key: &SlackMessageKey,
    thread_ts: Option<&str>,
    source: SlackIngestSource,
    deleted_at_ms: Option<i64>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    c.execute(
        "INSERT INTO slack_ingest_ledger \
             (message_id, team_id, channel_id, ts, thread_ts, first_source, deleted_at_ms, \
              created_at_ms, updated_at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
        params![
            key.message_id(),
            key.team_id,
            key.channel_id,
            key.ts,
            thread_ts.filter(|t| *t != key.ts),
            source.as_str(),
            deleted_at_ms,
            now_ms
        ],
    )?;
    Ok(())
}

/// The ledger row for a stored message, adopting a pre-ledger `emails` row
/// as `legacy`. `None` when the message was never stored.
fn known_row(
    c: &Connection,
    key: &SlackMessageKey,
    now_ms: i64,
) -> rusqlite::Result<Option<SlackLedgerRow>> {
    let id = key.message_id();
    if let Some(row) = ledger_row(c, &id)? {
        return Ok(Some(row));
    }
    if email_exists(c, &id)? {
        insert_ledger(c, key, None, SlackIngestSource::Legacy, None, now_ms)?;
        return ledger_row(c, &id);
    }
    Ok(None)
}

impl Store {
    /// Store `email` for `key` unless any path already did. `email` must be
    /// the row for this key (`email.message_id == key.message_id()`).
    pub fn record_slack_message(
        &self,
        key: &SlackMessageKey,
        thread_ts: Option<&str>,
        email: &Email,
        source: SlackIngestSource,
        now_ms: i64,
    ) -> StoreResult<SlackRecordOutcome> {
        if email.message_id != key.message_id() {
            return Err(StoreError::InvalidInput(format!(
                "email {} is not the row for slack message {}",
                email.message_id,
                key.message_id()
            )));
        }
        if source == SlackIngestSource::Legacy {
            return Err(StoreError::InvalidInput(
                "legacy is not an ingestion source".into(),
            ));
        }
        tx(self, |c| {
            if let Some(row) = known_row(c, key, now_ms)? {
                return Ok(if row.deleted {
                    SlackRecordOutcome::Deleted
                } else {
                    SlackRecordOutcome::Duplicate {
                        first_source: row.first_source,
                    }
                });
            }
            let body = crate::redact::mask(&email.body);
            c.execute(
                "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, \
                     accountEntityId, firstSeenAt, platform, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    email.message_id,
                    email.thread_id,
                    email.from,
                    email.subject,
                    body.as_ref(),
                    email.date,
                    email.account_entity_id,
                    now_ms,
                    email.platform,
                    email.kind,
                ],
            )?;
            insert_ledger(c, key, thread_ts, source, None, now_ms)?;
            Ok(SlackRecordOutcome::Stored)
        })
    }

    /// Take the one triage decision for `message_id`. `true` for exactly one
    /// caller while the message is unprocessed; a claim older than
    /// `stale_after_ms` (the daemon died mid-triage) may be taken again.
    pub fn claim_slack_triage(
        &self,
        message_id: &str,
        now_ms: i64,
        stale_after_ms: i64,
    ) -> StoreResult<bool> {
        tx(self, |c| {
            let n = c.execute(
                "UPDATE slack_ingest_ledger \
                    SET triage_claimed_at_ms = ?2, triage_claims = triage_claims + 1, \
                        updated_at_ms = ?2 \
                  WHERE message_id = ?1 AND deleted_at_ms IS NULL \
                    AND (triage_claimed_at_ms IS NULL OR triage_claimed_at_ms <= ?3) \
                    AND NOT EXISTS (SELECT 1 FROM emails \
                                     WHERE messageId = ?1 AND agentProcessedAt IS NOT NULL)",
                params![message_id, now_ms, now_ms.saturating_sub(stale_after_ms)],
            )?;
            Ok(n == 1)
        })
    }

    /// Give a claim back after a triage that failed before deciding, so the
    /// next sighting can try again.
    pub fn release_slack_triage(&self, message_id: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE slack_ingest_ledger SET triage_claimed_at_ms = NULL \
                  WHERE message_id = ?1",
                params![message_id],
            )
        })?;
        Ok(())
    }

    /// Apply Slack edit `edit_ts` (the message's `edited.ts`) once: the
    /// stored text becomes `text`. Never a new message and never re-triaged.
    pub fn apply_slack_edit(
        &self,
        key: &SlackMessageKey,
        edit_ts: &str,
        text: &str,
        now_ms: i64,
    ) -> StoreResult<SlackEditOutcome> {
        let Some(edit_order) = ts_order(edit_ts) else {
            return Err(StoreError::InvalidInput(format!(
                "slack edit ts `{edit_ts}` is invalid"
            )));
        };
        tx(self, |c| {
            let Some(row) = known_row(c, key, now_ms)? else {
                return Ok(SlackEditOutcome::Unknown);
            };
            if row.deleted {
                return Ok(SlackEditOutcome::Deleted);
            }
            if row
                .edit_ts
                .as_deref()
                .and_then(ts_order)
                .is_some_and(|applied| applied >= edit_order)
            {
                return Ok(SlackEditOutcome::AlreadyApplied);
            }
            let body = crate::redact::mask(text);
            c.execute(
                "UPDATE emails SET body = ?2 WHERE messageId = ?1",
                params![row.message_id, body.as_ref()],
            )?;
            c.execute(
                "UPDATE slack_ingest_ledger \
                    SET edit_ts = ?2, edit_count = edit_count + 1, updated_at_ms = ?3 \
                  WHERE message_id = ?1",
                params![row.message_id, edit_ts, now_ms],
            )?;
            Ok(SlackEditOutcome::Applied)
        })
    }

    /// Apply a Slack delete once. The stored text is removed (the row itself
    /// is dropped unless an action still refers to it, in which case its body
    /// is emptied), and a tombstone keeps the message from being stored
    /// again.
    pub fn apply_slack_delete(
        &self,
        key: &SlackMessageKey,
        now_ms: i64,
    ) -> StoreResult<SlackDeleteOutcome> {
        tx(self, |c| {
            let Some(row) = known_row(c, key, now_ms)? else {
                insert_ledger(c, key, None, SlackIngestSource::Live, Some(now_ms), now_ms)?;
                return Ok(SlackDeleteOutcome::Tombstoned);
            };
            if row.deleted {
                return Ok(SlackDeleteOutcome::AlreadyDeleted);
            }
            let referenced: bool = c
                .prepare("SELECT 1 FROM actions WHERE messageId = ?1")
                .and_then(|mut s| s.exists(params![row.message_id]))
                .unwrap_or(false);
            if referenced {
                c.execute(
                    "UPDATE emails SET body = '' WHERE messageId = ?1",
                    params![row.message_id],
                )?;
            } else {
                c.execute(
                    "DELETE FROM emails WHERE messageId = ?1",
                    params![row.message_id],
                )?;
            }
            c.execute(
                "UPDATE slack_ingest_ledger SET deleted_at_ms = ?2, updated_at_ms = ?2 \
                  WHERE message_id = ?1",
                params![row.message_id, now_ms],
            )?;
            Ok(SlackDeleteOutcome::Applied)
        })
    }

    /// A conversation was renamed: update every display name (the
    /// subscription, reply-target labels) and queue its messages for
    /// re-indexing so search finds them under the new name. IDs never
    /// change. Returns how many subscriptions were renamed.
    pub fn rename_slack_conversation(
        &self,
        team_id: &str,
        channel_id: &str,
        display_name: &str,
        now_ms: i64,
    ) -> StoreResult<usize> {
        if display_name.trim().is_empty() {
            return Err(StoreError::InvalidInput("a new name is required".into()));
        }
        tx(self, |c| {
            let renamed = c.execute(
                "UPDATE channel_subscriptions SET display_name = ?3, updated_at_ms = ?4 \
                  WHERE platform = 'slack' AND channel_id = ?2 \
                    AND (account_id = ?1 OR account_id IS NULL) AND display_name <> ?3",
                params![team_id, channel_id, display_name, now_ms],
            )?;
            c.execute(
                "UPDATE slack_send_targets SET label = ?3 \
                  WHERE team_id = ?1 AND channel_id = ?2",
                params![team_id, channel_id, display_name],
            )?;
            c.execute(
                "INSERT OR REPLACE INTO message_index_queue(message_id) \
                 SELECT messageId FROM emails WHERE platform = 'slack' AND threadId = ?1",
                params![channel_id],
            )?;
            Ok(renamed)
        })
    }

    pub fn slack_ledger_row(&self, message_id: &str) -> StoreResult<Option<SlackLedgerRow>> {
        self.with_conn(|c| ledger_row(c, message_id))
    }

    /// Stored (not deleted) replies to the thread under `parent_ts`, oldest
    /// first, as message IDs.
    pub fn slack_thread_replies(
        &self,
        team_id: &str,
        channel_id: &str,
        parent_ts: &str,
    ) -> StoreResult<Vec<String>> {
        let mut rows: Vec<(String, String)> = self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT message_id, ts FROM slack_ingest_ledger \
                  WHERE team_id = ?1 AND channel_id = ?2 AND thread_ts = ?3 \
                    AND deleted_at_ms IS NULL",
            )?;
            let rows = stmt
                .query_map(params![team_id, channel_id, parent_ts], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        rows.sort_by_key(|(_, ts)| ts_order(ts));
        Ok(rows.into_iter().map(|(id, _)| id).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_timestamps_order_exactly() {
        assert!(ts_order("1700000000.000100") < ts_order("1700000000.000101"));
        assert!(ts_order("999999999.999999") < ts_order("1000000000.000000"));
        assert_eq!(ts_to_ms("1700000000.123456"), Some(1_700_000_000_123));
        assert_eq!(ts_order("nope"), None);
        assert_eq!(ts_order("1.2.3"), None);
    }

    #[test]
    fn keys_keep_the_stored_message_id() {
        let k = SlackMessageKey::new("T1", "C1", "1700000000.000100").unwrap();
        assert_eq!(k.message_id(), "C1:1700000000.000100");
        assert!(SlackMessageKey::new("T1", "C:1", "1.0").is_err());
        assert!(SlackMessageKey::new("", "C1", "1.0").is_err());
    }
}
