use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::models::{Account, ActionStatus, Email, LearnedPattern, TriageResult};

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
        Ok(Self { conn: Mutex::new(conn) })
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
                "INSERT INTO emails (messageId, threadId, fromEmail, subject, body, receivedAt, accountEntityId, firstSeenAt) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    email.message_id,
                    email.thread_id,
                    email.from,
                    email.subject,
                    email.body,
                    email.date,
                    email.account_entity_id,
                    now,
                ],
            )?;
            Ok(true)
        }
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

    pub fn mark_email_processed(
        &self,
        message_id: &str,
        triage: TriageResult,
    ) -> StoreResult<()> {
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
                    agentProcessedAt INTEGER
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
            .log_action("m1", None, "a@b.com", "subj", None, None, ActionStatus::Pending)
            .unwrap();
        s.update_action_status(&id, ActionStatus::Sent, Some("draft"), None).unwrap();
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
        s.log_action("m1", None, "a@b.com", "s", None, None, ActionStatus::DryRun).unwrap();
        assert!(s.is_message_processed("m1").unwrap());
    }
}
