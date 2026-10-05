//! iMessage send outbox and conversation allowlists (#1301).
//!
//! The agent never talks to Messages directly. An approved reply becomes one
//! outbox row; the Mac-side sender claims it, sends through Messages, checks
//! `chat.db` for the outcome and reports back. Rows move
//! `queued → claimed → sent | failed`. A claim that is never reported becomes
//! `unknown`, never `queued`: re-sending a message that may already have gone
//! out is worse than asking the operator to look.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::store::{now_millis, Store, StoreError, StoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImessageOutboxStatus {
    Queued,
    Claimed,
    Sent,
    Failed,
    Unknown,
}

impl ImessageOutboxStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Claimed => "claimed",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    fn parse(s: &str) -> rusqlite::Result<Self> {
        Ok(match s {
            "queued" => Self::Queued,
            "claimed" => Self::Claimed,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "unknown" => Self::Unknown,
            other => {
                return Err(rusqlite::Error::InvalidColumnType(
                    0,
                    format!("imessage outbox status {other}"),
                    rusqlite::types::Type::Text,
                ))
            }
        })
    }
}

/// How the sender addresses the conversation. A handle is a phone number or
/// email for a 1:1 chat; a chat guid is copied verbatim from `chat.db` and is
/// the only way to address a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImessageTargetKind {
    Handle,
    ChatGuid,
}

impl ImessageTargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handle => "handle",
            Self::ChatGuid => "chat_guid",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "handle" => Some(Self::Handle),
            "chat_guid" => Some(Self::ChatGuid),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewImessageOutboxItem<'a> {
    pub action_id: &'a str,
    pub target: &'a str,
    pub target_kind: ImessageTargetKind,
    pub service: &'a str,
    pub body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImessageOutboxItem {
    pub id: i64,
    pub action_id: String,
    pub target: String,
    pub target_kind: ImessageTargetKind,
    pub service: String,
    pub body: String,
    pub status: ImessageOutboxStatus,
    pub created_at_ms: i64,
    pub claimed_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub error_code: Option<i64>,
    pub error_detail: Option<String>,
    pub message_guid: Option<String>,
}

/// What the sender observed in `chat.db`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImessageSendOutcome {
    Unknown { reason: String },
    Sent {
        message_guid: Option<String>,
    },
    Failed {
        error_code: Option<i64>,
        reason: String,
    },
}

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS imessage_outbox (\
             id              INTEGER PRIMARY KEY AUTOINCREMENT,\
             action_id       TEXT NOT NULL UNIQUE,\
             target          TEXT NOT NULL,\
             target_kind     TEXT NOT NULL CHECK(target_kind IN ('handle', 'chat_guid')),\
             service         TEXT NOT NULL,\
             body            TEXT NOT NULL,\
             status          TEXT NOT NULL DEFAULT 'queued' \
                             CHECK(status IN ('queued', 'claimed', 'sent', 'failed', 'unknown')),\
             created_at_ms   INTEGER NOT NULL,\
             claimed_at_ms   INTEGER,\
             completed_at_ms INTEGER,\
             error_code      INTEGER,\
             error_detail    TEXT,\
             message_guid    TEXT,\
             notified_at_ms  INTEGER\
         );\
         CREATE INDEX IF NOT EXISTS idx_imessage_outbox_status \
             ON imessage_outbox(status, id);\
         CREATE INDEX IF NOT EXISTS idx_imessage_outbox_target \
             ON imessage_outbox(target, status);\
         CREATE TABLE IF NOT EXISTS imessage_outbound_allowlist (\
             identifier    TEXT PRIMARY KEY,\
             enabled_at_ms INTEGER NOT NULL\
         );\
         CREATE TABLE IF NOT EXISTS imessage_inbound_allowlist (\
             identifier    TEXT PRIMARY KEY,\
             enabled_at_ms INTEGER NOT NULL\
         );",
    )?;
    Ok(())
}

pub(crate) const COLUMNS: &str = "id, action_id, target, target_kind, service, body, status, \
     created_at_ms, claimed_at_ms, completed_at_ms, error_code, error_detail, message_guid";

pub(crate) fn row_to_item(r: &Row<'_>) -> rusqlite::Result<ImessageOutboxItem> {
    let kind: String = r.get(3)?;
    let status: String = r.get(6)?;
    Ok(ImessageOutboxItem {
        id: r.get(0)?,
        action_id: r.get(1)?,
        target: r.get(2)?,
        target_kind: ImessageTargetKind::parse(&kind).ok_or_else(|| {
            rusqlite::Error::InvalidColumnType(3, kind.clone(), rusqlite::types::Type::Text)
        })?,
        service: r.get(4)?,
        body: r.get(5)?,
        status: ImessageOutboxStatus::parse(&status)?,
        created_at_ms: r.get(7)?,
        claimed_at_ms: r.get(8)?,
        completed_at_ms: r.get(9)?,
        error_code: r.get(10)?,
        error_detail: r.get(11)?,
        message_guid: r.get(12)?,
    })
}

/// Identifiers become AppleScript arguments and SQL keys; a newline or an
/// empty value is never a real handle or chat guid.
fn check_identifier(value: &str, what: &str) -> StoreResult<()> {
    if value.trim().is_empty() || value.contains(['\n', '\r', '\0']) {
        return Err(StoreError::InvalidInput(format!("invalid iMessage {what}")));
    }
    Ok(())
}

fn allowlist_table(outbound: bool) -> &'static str {
    if outbound {
        "imessage_outbound_allowlist"
    } else {
        "imessage_inbound_allowlist"
    }
}

impl Store {
    /// Queue one approved reply. Returns `false` when the action already has
    /// a row, so a repeated approve can never queue a second send.
    pub fn enqueue_imessage_outbox(&self, item: &NewImessageOutboxItem<'_>) -> StoreResult<bool> {
        check_identifier(item.action_id, "action id")?;
        check_identifier(item.target, "target")?;
        check_identifier(item.service, "service")?;
        if item.body.trim().is_empty() {
            return Err(StoreError::InvalidInput("empty iMessage body".into()));
        }
        let n = self.with_conn(|c| {
            c.execute(
                "INSERT INTO imessage_outbox \
                     (action_id, target, target_kind, service, body, status, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6) \
                 ON CONFLICT(action_id) DO NOTHING",
                params![
                    item.action_id,
                    item.target,
                    item.target_kind.as_str(),
                    item.service,
                    item.body,
                    now_millis()
                ],
            )
        })?;
        Ok(n == 1)
    }

    /// Hand the oldest queued row to one sender. A single UPDATE … RETURNING
    /// so two processes polling the same database cannot both get it.
    pub fn claim_imessage_outbox(&self) -> StoreResult<Option<ImessageOutboxItem>> {
        let sql = format!(
            "UPDATE imessage_outbox SET status = 'claimed', claimed_at_ms = ?1 \
              WHERE id = (SELECT id FROM imessage_outbox WHERE status = 'queued' \
                          AND NOT EXISTS (SELECT 1 FROM owner_alert_texts WHERE outbox_id=imessage_outbox.id) \
                          ORDER BY id LIMIT 1) \
                AND status = 'queued' \
             RETURNING {COLUMNS}"
        );
        self.with_conn(|c| {
            c.query_row(&sql, params![now_millis()], row_to_item)
                .optional()
        })
    }

    /// Record the sender's verdict. Accepted from `claimed`, and from
    /// `unknown` so a late but verified report still lands; refused from
    /// every other status, which makes a second report a no-op.
    pub fn complete_imessage_outbox(
        &self,
        id: i64,
        outcome: &ImessageSendOutcome,
    ) -> StoreResult<bool> {
        let now = now_millis();
        let n = self.with_conn(|c| match outcome {
            ImessageSendOutcome::Unknown { reason } => c.execute(
                "UPDATE imessage_outbox SET status='unknown',error_detail=?2
                 WHERE id=?1 AND status='claimed'", params![id,reason],
            ),
            ImessageSendOutcome::Sent { message_guid } => c.execute(
                "UPDATE imessage_outbox \
                    SET status = 'sent', completed_at_ms = ?2, message_guid = ?3 \
                  WHERE id = ?1 AND status IN ('claimed', 'unknown')",
                params![id, now, message_guid],
            ),
            ImessageSendOutcome::Failed { error_code, reason } => c.execute(
                "UPDATE imessage_outbox \
                    SET status = 'failed', completed_at_ms = ?2, error_code = ?3, \
                        error_detail = ?4 \
                  WHERE id = ?1 AND status IN ('claimed', 'unknown')",
                params![id, now, error_code, reason],
            ),
        })?;
        Ok(n == 1)
    }

    /// Move claims older than `timeout_ms` to `unknown` and return them.
    pub fn expire_imessage_outbox_claims(
        &self,
        timeout_ms: i64,
    ) -> StoreResult<Vec<ImessageOutboxItem>> {
        let sql = format!(
            "UPDATE imessage_outbox SET status = 'unknown' \
              WHERE status = 'claimed' AND claimed_at_ms < ?1 \
             RETURNING {COLUMNS}"
        );
        let cutoff = now_millis() - timeout_ms;
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params![cutoff], row_to_item)?;
            rows.collect()
        })
    }

    /// Fail queued rows older than `max_age_ms` and return them. A reply
    /// that waited that long for the Mac is stale; sending it late is worse
    /// than telling the operator it did not go.
    pub fn expire_imessage_outbox_queued(
        &self,
        max_age_ms: i64,
    ) -> StoreResult<Vec<ImessageOutboxItem>> {
        let sql = format!(
            "UPDATE imessage_outbox \
                SET status = 'failed', completed_at_ms = ?2, \
                    error_detail = 'expired before the Mac sender picked it up' \
              WHERE status = 'queued' AND created_at_ms < ?1 \
                AND NOT EXISTS (SELECT 1 FROM owner_alert_texts WHERE outbox_id=imessage_outbox.id) \
             RETURNING {COLUMNS}"
        );
        let now = now_millis();
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params![now - max_age_ms, now], row_to_item)?;
            rows.collect()
        })
    }

    /// Failed or unknown rows the operator has not been told about yet.
    pub fn unnotified_imessage_outbox_failures(&self) -> StoreResult<Vec<ImessageOutboxItem>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM imessage_outbox \
              WHERE status IN ('failed', 'unknown') AND notified_at_ms IS NULL \
              ORDER BY id"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], row_to_item)?;
            rows.collect()
        })
    }

    pub fn mark_imessage_outbox_notified(&self, id: i64) -> StoreResult<bool> {
        let n = self.with_conn(|c| {
            c.execute(
                "UPDATE imessage_outbox SET notified_at_ms = ?2 \
                  WHERE id = ?1 AND notified_at_ms IS NULL",
                params![id, now_millis()],
            )
        })?;
        Ok(n == 1)
    }

    pub fn get_imessage_outbox(&self, id: i64) -> StoreResult<Option<ImessageOutboxItem>> {
        let sql = format!("SELECT {COLUMNS} FROM imessage_outbox WHERE id = ?1");
        self.with_conn(|c| c.query_row(&sql, params![id], row_to_item).optional())
    }

    pub fn imessage_outbox_for_action(
        &self,
        action_id: &str,
    ) -> StoreResult<Option<ImessageOutboxItem>> {
        let sql = format!("SELECT {COLUMNS} FROM imessage_outbox WHERE action_id = ?1");
        self.with_conn(|c| {
            c.query_row(&sql, params![action_id], row_to_item)
                .optional()
        })
    }

    /// Newest first.
    pub fn list_imessage_outbox(&self, limit: usize) -> StoreResult<Vec<ImessageOutboxItem>> {
        let sql = format!("SELECT {COLUMNS} FROM imessage_outbox ORDER BY id DESC LIMIT ?1");
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params![limit as i64], row_to_item)?;
            rows.collect()
        })
    }

    /// Sent rows for one conversation completed at or after `since_ms`, so
    /// re-ingest can recognise the agent's own messages coming back.
    pub fn sent_imessage_outbox_for_target(
        &self,
        target: &str,
        since_ms: i64,
    ) -> StoreResult<Vec<ImessageOutboxItem>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM imessage_outbox \
              WHERE target = ?1 AND status = 'sent' AND completed_at_ms >= ?2 \
              ORDER BY id"
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map(params![target, since_ms], row_to_item)?;
            rows.collect()
        })
    }

    fn allow_imessage(&self, outbound: bool, identifier: &str) -> StoreResult<bool> {
        check_identifier(identifier, "conversation identifier")?;
        let sql = format!(
            "INSERT INTO {} (identifier, enabled_at_ms) VALUES (?1, ?2) \
             ON CONFLICT(identifier) DO NOTHING",
            allowlist_table(outbound)
        );
        Ok(self.with_conn(|c| c.execute(&sql, params![identifier, now_millis()]))? == 1)
    }

    fn deny_imessage(&self, outbound: bool, identifier: &str) -> StoreResult<bool> {
        let sql = format!(
            "DELETE FROM {} WHERE identifier = ?1",
            allowlist_table(outbound)
        );
        Ok(self.with_conn(|c| c.execute(&sql, params![identifier]))? == 1)
    }

    fn is_imessage_allowed(&self, outbound: bool, identifier: &str) -> StoreResult<bool> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE identifier = ?1",
            allowlist_table(outbound)
        );
        let n: i64 = self.with_conn(|c| c.query_row(&sql, params![identifier], |r| r.get(0)))?;
        Ok(n > 0)
    }

    /// Opt a conversation into sends. Returns `false` if it already was.
    pub fn allow_imessage_outbound(&self, identifier: &str) -> StoreResult<bool> {
        self.allow_imessage(true, identifier)
    }

    /// Returns `false` if the conversation was not on the list.
    pub fn deny_imessage_outbound(&self, identifier: &str) -> StoreResult<bool> {
        self.deny_imessage(true, identifier)
    }

    pub fn is_imessage_outbound_allowed(&self, identifier: &str) -> StoreResult<bool> {
        self.is_imessage_allowed(true, identifier)
    }

    /// Opt a conversation into drafting reply cards.
    pub fn allow_imessage_inbound(&self, identifier: &str) -> StoreResult<bool> {
        self.allow_imessage(false, identifier)
    }

    pub fn deny_imessage_inbound(&self, identifier: &str) -> StoreResult<bool> {
        self.deny_imessage(false, identifier)
    }

    pub fn is_imessage_inbound_allowed(&self, identifier: &str) -> StoreResult<bool> {
        self.is_imessage_allowed(false, identifier)
    }

    pub fn list_imessage_allowlist(&self, outbound: bool) -> StoreResult<Vec<String>> {
        let sql = format!(
            "SELECT identifier FROM {} ORDER BY identifier",
            allowlist_table(outbound)
        );
        self.with_conn(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            rows.collect()
        })
    }
}
