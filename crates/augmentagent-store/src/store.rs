use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::models::{Account, ActionRecord, ActionStatus, Email, LearnedPattern, TriageResult};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type StoreResult<T> = Result<T, StoreError>;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Additive, idempotent schema migrations. Safe to run against databases
    /// that were created by the original Node daemon (they just lack some of
    /// the newer columns).
    fn migrate(conn: &Connection) -> StoreResult<()> {
        if !column_exists(conn, "actions", "retryCount")? {
            conn.execute("ALTER TABLE actions ADD COLUMN retryCount INTEGER DEFAULT 0", [])?;
        }
        if !column_exists(conn, "actions", "draftId")? {
            conn.execute("ALTER TABLE actions ADD COLUMN draftId TEXT", [])?;
        }
        if !column_exists(conn, "emails", "platform")? {
            conn.execute(
                "ALTER TABLE emails ADD COLUMN platform TEXT NOT NULL DEFAULT 'gmail'",
                [],
            )?;
            // One-shot backfill for pre-platform-column rows: any row whose
            // accountEntityId looks like a LinkedIn URN is tagged 'linkedin'.
            // Safe to run once at column-add time — fresh rows from the channels
            // write their platform directly.
            conn.execute(
                "UPDATE emails SET platform = 'linkedin' WHERE accountEntityId LIKE 'urn:li:%'",
                [],
            )?;
        }
        if !column_exists(conn, "emails", "kind")? {
            conn.execute(
                "ALTER TABLE emails ADD COLUMN kind TEXT NOT NULL DEFAULT 'dm'",
                [],
            )?;
        }
        Ok(())
    }

    /// Insert email if new. Returns `true` when the row did not previously exist.
    /// Matches Node `upsertEmail` behavior in src/db.ts: preserves firstSeenAt on re-seen messages.
    pub fn upsert_email(&self, email: &Email) -> StoreResult<bool> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let existed: Option<i64> = guard
            .query_row(
                "SELECT 1 FROM emails WHERE messageId = ?1",
                params![email.message_id],
                |r| r.get(0),
            )
            .optional()?;
        if existed.is_some() {
            guard.execute(
                "UPDATE emails SET threadId = ?2, fromEmail = ?3, subject = ?4, body = ?5, receivedAt = ?6 WHERE messageId = ?1",
                params![
                    email.message_id,
                    email.thread_id,
                    email.from,
                    email.subject,
                    email.body,
                    email.date,
                ],
            )?;
            Ok(false)
        } else {
            let now = now_millis();
            guard.execute(
                "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, accountEntityId, firstSeenAt, platform, kind) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    email.message_id,
                    email.thread_id,
                    email.from,
                    email.subject,
                    email.body,
                    email.date,
                    email.account_entity_id,
                    now,
                    email.platform,
                    email.kind,
                ],
            )?;
            Ok(true)
        }
    }

    /// True iff the email has been carried to a terminal outcome — skip, flag,
    /// dry-run reply, successful send, or an explicit rejection/timeout from
    /// the approver. Transient errors leave `agentProcessedAt = NULL`, which
    /// makes them retryable.
    pub fn is_email_complete(&self, message_id: &str) -> StoreResult<bool> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row: Option<Option<i64>> = guard
            .query_row(
                "SELECT agentProcessedAt FROM emails WHERE messageId = ?1",
                params![message_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(matches!(row, Some(Some(_))))
    }

    pub fn is_message_processed(&self, message_id: &str) -> StoreResult<bool> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row: Option<i64> = guard
            .query_row(
                "SELECT 1 FROM actions WHERE messageId = ?1 LIMIT 1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.is_some())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log_action(
        &self,
        message_id: &str,
        thread_id: Option<&str>,
        from_email: &str,
        subject: &str,
        original_body: Option<&str>,
        draft_body: Option<&str>,
        status: ActionStatus,
    ) -> StoreResult<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO actions (id, messageId, threadId, fromEmail, subject, originalBody, draftBody, status, errorMessage, createdAt, updatedAt) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9)",
            params![
                id,
                message_id,
                thread_id,
                from_email,
                subject,
                original_body,
                draft_body,
                status.as_str(),
                now,
            ],
        )?;
        Ok(id)
    }

    pub fn update_action_status(
        &self,
        action_id: &str,
        status: ActionStatus,
        draft_body: Option<&str>,
        error_message: Option<&str>,
    ) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE actions SET status = ?2, draftBody = COALESCE(?3, draftBody), errorMessage = COALESCE(?4, errorMessage), updatedAt = ?5 WHERE id = ?1",
            params![action_id, status.as_str(), draft_body, error_message, now],
        )?;
        Ok(())
    }

    pub fn mark_email_processed(&self, message_id: &str, triage: TriageResult) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE emails SET triageResult = ?2, agentProcessedAt = ?3 WHERE messageId = ?1",
            params![message_id, triage.as_str(), now],
        )?;
        Ok(())
    }

    pub fn get_active_gmail_accounts(&self) -> StoreResult<Vec<Account>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, connectionId, entityId, email, active FROM gmail_accounts WHERE active = 1",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Account {
                id: r.get(0)?,
                connection_id: r.get::<_, Option<String>>(1)?,
                entity_id: r.get(2)?,
                email: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                active: r.get::<_, i64>(4)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn save_learned_pattern(&self, _pattern: &LearnedPattern) -> StoreResult<()> {
        // Node writes these as JSON under skills/email-triage/learned/*.json, not sqlite.
        // Phase 3 decides final home. For Phase 1 this is a no-op; channel adapter logs instead.
        Ok(())
    }

    /// Count actions grouped by status for rows created in the last `since_ms`
    /// milliseconds. Pairs the `actions.status` text with its count.
    pub fn action_counts_since(
        &self,
        since_ms: i64,
    ) -> StoreResult<Vec<(String, i64)>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT status, COUNT(*) FROM actions WHERE createdAt >= ?1 GROUP BY status",
        )?;
        let rows = stmt.query_map(params![since_ms], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Most recently processed emails. Each row: (from, subject, triageResult).
    /// `triageResult` is `None` for rows still awaiting processing.
    pub fn recent_emails_since(
        &self,
        since_ms: i64,
        limit: i64,
    ) -> StoreResult<Vec<(String, String, Option<String>)>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT fromEmail, subject, triageResult \
             FROM emails \
             WHERE firstSeenAt >= ?1 \
             ORDER BY firstSeenAt DESC \
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since_ms, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// How many reply actions are currently sitting in `pending` status
    /// (awaiting the user's Discord click). Useful as a digest metric.
    pub fn pending_reply_count(&self) -> StoreResult<i64> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n: i64 = guard.query_row(
            "SELECT COUNT(*) FROM actions WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Load a single action row plus its email body. Used by the Discord
    /// event handler on approve/revise/skip clicks to reconstruct context.
    pub fn get_action_with_email(&self, action_id: &str) -> StoreResult<Option<ActionWithEmail>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row: Option<ActionWithEmail> = guard
            .query_row(
                "SELECT \
                   a.id, a.messageId, a.threadId, a.fromEmail, a.subject, \
                   a.originalBody, a.draftBody, a.status, a.errorMessage, \
                   a.createdAt, a.updatedAt, COALESCE(a.retryCount, 0), a.draftId, \
                   e.body, e.receivedAt, e.accountEntityId, e.platform, e.kind \
                 FROM actions a \
                 LEFT JOIN emails e ON a.messageId = e.messageId \
                 WHERE a.id = ?1",
                params![action_id],
                |r| {
                    Ok(ActionWithEmail {
                        action: ActionRecord {
                            id: r.get(0)?,
                            message_id: r.get(1)?,
                            thread_id: r.get(2)?,
                            from_email: r.get(3)?,
                            subject: r.get(4)?,
                            original_body: r.get(5)?,
                            draft_body: r.get(6)?,
                            status: r.get(7)?,
                            error_message: r.get(8)?,
                            created_at: ms_to_rfc3339(r.get::<_, i64>(9)?),
                            updated_at: ms_to_rfc3339(r.get::<_, i64>(10)?),
                        },
                        retry_count: r.get::<_, i64>(11)?,
                        draft_id: r.get::<_, Option<String>>(12)?,
                        email: Email {
                            message_id: r.get(1)?,
                            thread_id: r.get(2)?,
                            from: r.get(3)?,
                            subject: r.get(4)?,
                            body: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                            date: r.get::<_, Option<String>>(14)?.unwrap_or_default(),
                            account_entity_id: r.get::<_, Option<String>>(15)?,
                            platform: r.get::<_, Option<String>>(16)?.unwrap_or_else(|| "gmail".into()),
                            kind: r.get::<_, Option<String>>(17)?.unwrap_or_else(|| "dm".into()),
                        },
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Store the Gmail-side draft id alongside an action. Called right after
    /// create_draft succeeds.
    pub fn set_action_draft_id(&self, action_id: &str, draft_id: &str) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE actions SET draftId = ?2, updatedAt = ?3 WHERE id = ?1",
            params![action_id, draft_id, now],
        )?;
        Ok(())
    }

    /// Find reply-intent actions that errored out and deserve another try.
    ///
    /// Criteria:
    /// - `actions.status = 'error'` (not `permanent_error`, not terminal)
    /// - `actions.createdAt` within `max_age_ms` (don't retry ancient errors forever)
    /// - `actions.updatedAt` older than `min_gap_ms` ago (space attempts out)
    /// - `actions.retryCount < max_attempts`
    /// - Joined with `emails` so the caller has the email body to retry with
    pub fn list_retryable_replies(
        &self,
        now_ms: i64,
        max_age_ms: i64,
        min_gap_ms: i64,
        max_attempts: i64,
        limit: i64,
    ) -> StoreResult<Vec<RetryableReply>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT \
               a.id, a.messageId, a.threadId, a.fromEmail, a.subject, \
               a.originalBody, a.draftBody, a.status, a.errorMessage, \
               a.createdAt, a.updatedAt, COALESCE(a.retryCount, 0), \
               e.body, e.receivedAt, e.accountEntityId, e.platform, e.kind \
             FROM actions a \
             JOIN emails e ON a.messageId = e.messageId \
             WHERE a.status = 'error' \
               AND a.createdAt >= ?1 \
               AND a.updatedAt <= ?2 \
               AND COALESCE(a.retryCount, 0) < ?3 \
             ORDER BY a.createdAt ASC \
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![
                now_ms - max_age_ms,
                now_ms - min_gap_ms,
                max_attempts,
                limit,
            ],
            |r| {
                Ok(RetryableReply {
                    action: ActionRecord {
                        id: r.get(0)?,
                        message_id: r.get(1)?,
                        thread_id: r.get(2)?,
                        from_email: r.get(3)?,
                        subject: r.get(4)?,
                        original_body: r.get(5)?,
                        draft_body: r.get(6)?,
                        status: r.get(7)?,
                        error_message: r.get(8)?,
                        created_at: ms_to_rfc3339(r.get::<_, i64>(9)?),
                        updated_at: ms_to_rfc3339(r.get::<_, i64>(10)?),
                    },
                    retry_count: r.get::<_, i64>(11)?,
                    email: Email {
                        message_id: r.get(1)?,
                        thread_id: r.get(2)?,
                        from: r.get(3)?,
                        subject: r.get(4)?,
                        body: r.get::<_, Option<String>>(12)?.unwrap_or_default(),
                        date: r.get::<_, Option<String>>(13)?.unwrap_or_default(),
                        account_entity_id: r.get::<_, Option<String>>(14)?,
                        platform: r.get::<_, Option<String>>(15)?.unwrap_or_else(|| "gmail".into()),
                        kind: r.get::<_, Option<String>>(16)?.unwrap_or_else(|| "dm".into()),
                    },
                })
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Increment an action's retry counter. When it crosses `max_attempts`,
    /// flip the status to `permanent_error` so the retry loop stops touching it.
    pub fn increment_retry_count(
        &self,
        action_id: &str,
        max_attempts: i64,
    ) -> StoreResult<i64> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE actions SET retryCount = COALESCE(retryCount, 0) + 1, updatedAt = ?2 WHERE id = ?1",
            params![action_id, now],
        )?;
        let new_count: i64 = guard.query_row(
            "SELECT COALESCE(retryCount, 0) FROM actions WHERE id = ?1",
            params![action_id],
            |r| r.get(0),
        )?;
        if new_count >= max_attempts {
            guard.execute(
                "UPDATE actions SET status = 'permanent_error', updatedAt = ?2 WHERE id = ?1",
                params![action_id, now],
            )?;
        }
        Ok(new_count)
    }
}

#[derive(Debug, Clone)]
pub struct RetryableReply {
    pub action: ActionRecord,
    pub retry_count: i64,
    pub email: Email,
}

#[derive(Debug, Clone)]
pub struct ActionWithEmail {
    pub action: ActionRecord,
    pub retry_count: i64,
    pub draft_id: Option<String>,
    pub email: Email,
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> StoreResult<bool> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(1))?;
    for name in rows {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ms_to_rfc3339(ms: i64) -> String {
    use time::{format_description::well_known::Rfc3339, OffsetDateTime};
    OffsetDateTime::from_unix_timestamp(ms / 1000)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_default()
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn fresh_store() -> (Store, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        {
            let conn = Connection::open(file.path()).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY,
                    messageId TEXT NOT NULL,
                    threadId TEXT,
                    fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    originalBody TEXT,
                    draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending',
                    errorMessage TEXT,
                    createdAt INTEGER NOT NULL,
                    updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY,
                    threadId TEXT,
                    fromEmail TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    body TEXT,
                    receivedAt TEXT,
                    accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL,
                    triageResult TEXT,
                    agentProcessedAt INTEGER,
                    platform TEXT NOT NULL DEFAULT 'gmail',
                    kind TEXT NOT NULL DEFAULT 'dm'
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY,
                    connectionId TEXT NOT NULL,
                    email TEXT,
                    label TEXT,
                    entityId TEXT NOT NULL,
                    active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        (Store::open(file.path()).unwrap(), file)
    }

    fn sample_email(message_id: &str) -> Email {
        Email {
            message_id: message_id.into(),
            thread_id: None,
            from: "a@b.com".into(),
            subject: "hi".into(),
            body: "hello".into(),
            date: "2026-04-13T12:00:00Z".into(),
            account_entity_id: Some("acc".into()),
            platform: "gmail".into(),
            kind: "dm".into(),
        }
    }

    #[test]
    fn upsert_email_returns_is_new() {
        let (s, _f) = fresh_store();
        let e = sample_email("m1");
        assert!(s.upsert_email(&e).unwrap());
        assert!(!s.upsert_email(&e).unwrap());
    }

    #[test]
    fn log_and_update_action_status() {
        let (s, _f) = fresh_store();
        let id = s
            .log_action(
                "m1",
                None,
                "a@b.com",
                "subj",
                None,
                None,
                ActionStatus::Pending,
            )
            .unwrap();
        s.update_action_status(&id, ActionStatus::Sent, Some("draft"), None)
            .unwrap();
    }

    #[test]
    fn mark_email_processed_sets_triage() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        s.mark_email_processed("m1", TriageResult::Skip).unwrap();
    }

    #[test]
    fn is_message_processed_reflects_action_existence() {
        let (s, _f) = fresh_store();
        assert!(!s.is_message_processed("nope").unwrap());
        s.log_action("m1", None, "a@b.com", "s", None, None, ActionStatus::DryRun)
            .unwrap();
        assert!(s.is_message_processed("m1").unwrap());
    }
}
