use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::models::{
    Account, ActionRecord, ActionStatus, ChannelSubscription, Email, LearnedPattern,
    SlackWorkspace, SubscriptionMode, TriageResult,
};

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
        if !column_exists(conn, "actions", "nudgeCount")? {
            conn.execute(
                "ALTER TABLE actions ADD COLUMN nudgeCount INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        if !column_exists(conn, "actions", "nextNudgeAtMs")? {
            conn.execute("ALTER TABLE actions ADD COLUMN nextNudgeAtMs INTEGER", [])?;
            // One-shot backfill: rows still in 'pending' from before the
            // nudge-loop ship need a timer or they'll never get reminded.
            // Seeded at createdAt + 6h, matching the steady-state invariant
            // log_action maintains for fresh rows. Old backlog items will
            // therefore fire on the first scheduler tick after upgrade.
            conn.execute(
                "UPDATE actions \
                    SET nextNudgeAtMs = createdAt + ?1 \
                  WHERE status = 'pending' AND nextNudgeAtMs IS NULL",
                params![NUDGE_INTERVAL_MS],
            )?;
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
        // Issue #27: per-channel subscription registry (platform-agnostic).
        // Rows control which Discord/Slack/etc channels the poller watches and
        // which mode (priority / digest / store_only) they route through.
        // Uniqueness is enforced at the upsert layer (platform, channel_id,
        // account_id) rather than via a SQL UNIQUE so multi-workspace Slack
        // can carry the same channel id across teams.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS channel_subscriptions (\
                 id                   TEXT PRIMARY KEY,\
                 platform             TEXT NOT NULL,\
                 channel_id           TEXT NOT NULL,\
                 display_name         TEXT NOT NULL,\
                 mode                 TEXT NOT NULL,\
                 active               INTEGER NOT NULL DEFAULT 1,\
                 account_id           TEXT,\
                 last_seen_message_id TEXT,\
                 last_digest_at_ms    INTEGER,\
                 created_at_ms        INTEGER NOT NULL,\
                 updated_at_ms        INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_channel_subs_active_mode \
                ON channel_subscriptions(active, mode)",
            [],
        )?;
        // Multi-workspace Slack: each subscription may belong to a specific
        // account (for Slack, the workspace `team_id`). Nullable so existing
        // Discord rows migrate cleanly.
        if !column_exists(conn, "channel_subscriptions", "account_id")? {
            conn.execute(
                "ALTER TABLE channel_subscriptions ADD COLUMN account_id TEXT",
                [],
            )?;
        }
        // Older DBs still carry the legacy UNIQUE(platform, channel_id). Detect
        // it and rebuild without the constraint so multi-workspace rows can
        // coexist. SQLite can't ALTER away a constraint in place.
        if table_has_unique(conn, "channel_subscriptions", "platform", "channel_id")? {
            conn.execute_batch(
                "BEGIN TRANSACTION;\n\
                 CREATE TABLE channel_subscriptions_new (\
                   id                   TEXT PRIMARY KEY,\
                   platform             TEXT NOT NULL,\
                   channel_id           TEXT NOT NULL,\
                   display_name         TEXT NOT NULL,\
                   mode                 TEXT NOT NULL,\
                   active               INTEGER NOT NULL DEFAULT 1,\
                   account_id           TEXT,\
                   last_seen_message_id TEXT,\
                   last_digest_at_ms    INTEGER,\
                   created_at_ms        INTEGER NOT NULL,\
                   updated_at_ms        INTEGER NOT NULL\
                 );\n\
                 INSERT INTO channel_subscriptions_new \
                   (id, platform, channel_id, display_name, mode, active, \
                    account_id, last_seen_message_id, last_digest_at_ms, \
                    created_at_ms, updated_at_ms) \
                   SELECT id, platform, channel_id, display_name, mode, active, \
                          account_id, last_seen_message_id, last_digest_at_ms, \
                          created_at_ms, updated_at_ms \
                     FROM channel_subscriptions;\n\
                 DROP TABLE channel_subscriptions;\n\
                 ALTER TABLE channel_subscriptions_new RENAME TO channel_subscriptions;\n\
                 CREATE INDEX IF NOT EXISTS idx_channel_subs_active_mode \
                   ON channel_subscriptions(active, mode);\n\
                 COMMIT;",
            )?;
        }
        conn.execute(
            "CREATE TABLE IF NOT EXISTS slack_workspaces (\
                 id              TEXT PRIMARY KEY,\
                 team_id         TEXT NOT NULL UNIQUE,\
                 team_name       TEXT NOT NULL,\
                 entity_id       TEXT NOT NULL,\
                 connection_id   TEXT NOT NULL,\
                 user_id         TEXT NOT NULL,\
                 active          INTEGER NOT NULL DEFAULT 1,\
                 created_at_ms   INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_slack_workspaces_active \
                ON slack_workspaces(active)",
            [],
        )?;
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
        let next_nudge_at_ms = match status {
            ActionStatus::Pending => Some(now + NUDGE_INTERVAL_MS),
            _ => None,
        };
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO actions (id, messageId, threadId, fromEmail, subject, originalBody, draftBody, status, errorMessage, createdAt, updatedAt, nudgeCount, nextNudgeAtMs) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?9, 0, ?10)",
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
                next_nudge_at_ms,
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

    // --- pending-action nudge loop (serial queue) ---

    /// Count of pending actions currently in the nudge queue: rows whose
    /// `nextNudgeAtMs` has fired (i.e. they're either active or due to be
    /// promoted). Used by the scheduler to compute the X/Y queue counter.
    pub fn count_pending_overdue(&self, now_ms: i64) -> StoreResult<i64> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n: i64 = guard.query_row(
            "SELECT COUNT(*) FROM actions \
              WHERE status = 'pending' \
                AND nextNudgeAtMs IS NOT NULL \
                AND nextNudgeAtMs <= ?1",
            params![now_ms],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// The currently-active card in the nudge queue, if any. "Active" means a
    /// pending row that has already been promoted (`nudgeCount > 0`) — the
    /// card the user is currently looking at. There is at most one.
    pub fn find_active_nudge(&self) -> StoreResult<Option<PendingNudge>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT \
                   a.id, a.messageId, a.threadId, a.fromEmail, a.subject, \
                   a.originalBody, a.draftBody, a.status, a.errorMessage, \
                   a.createdAt, a.updatedAt, COALESCE(a.retryCount, 0), a.draftId, \
                   COALESCE(a.nudgeCount, 0), a.nextNudgeAtMs, \
                   e.body, e.receivedAt, e.accountEntityId, e.platform, e.kind \
                 FROM actions a \
                 JOIN emails e ON a.messageId = e.messageId \
                 WHERE a.status = 'pending' AND COALESCE(a.nudgeCount, 0) > 0 \
                 ORDER BY a.createdAt ASC \
                 LIMIT 1",
                [],
                row_to_pending_nudge,
            )
            .optional()?;
        Ok(row)
    }

    /// The next pending row eligible for promotion to active: oldest pending
    /// row with `nudgeCount = 0` whose `nextNudgeAtMs` has fired. Returns None
    /// when the backlog is empty.
    pub fn find_next_to_promote(&self, now_ms: i64) -> StoreResult<Option<PendingNudge>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT \
                   a.id, a.messageId, a.threadId, a.fromEmail, a.subject, \
                   a.originalBody, a.draftBody, a.status, a.errorMessage, \
                   a.createdAt, a.updatedAt, COALESCE(a.retryCount, 0), a.draftId, \
                   COALESCE(a.nudgeCount, 0), a.nextNudgeAtMs, \
                   e.body, e.receivedAt, e.accountEntityId, e.platform, e.kind \
                 FROM actions a \
                 JOIN emails e ON a.messageId = e.messageId \
                 WHERE a.status = 'pending' \
                   AND COALESCE(a.nudgeCount, 0) = 0 \
                   AND a.nextNudgeAtMs IS NOT NULL \
                   AND a.nextNudgeAtMs <= ?1 \
                 ORDER BY a.createdAt ASC \
                 LIMIT 1",
                params![now_ms],
                row_to_pending_nudge,
            )
            .optional()?;
        Ok(row)
    }

    /// Mark a nudge as delivered: bump `nudgeCount` and schedule the next
    /// reminder at `next_at_ms`. Caller computes the next time (typically
    /// `now + NUDGE_INTERVAL_MS`). Used both for initial promotion (count
    /// goes 0 → 1) and re-nudges of the active card (1 → 2 → ...).
    pub fn record_nudge(&self, action_id: &str, next_at_ms: i64) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE actions \
                SET nudgeCount = COALESCE(nudgeCount, 0) + 1, \
                    nextNudgeAtMs = ?2, \
                    updatedAt = ?3 \
              WHERE id = ?1",
            params![action_id, next_at_ms, now],
        )?;
        Ok(())
    }

    /// Defer the next nudge when the user engages mid-flow (e.g. revises).
    /// Pushes `nextNudgeAtMs` out by one full interval but **does not** zero
    /// `nudgeCount` — under serial-queue mode that would kick the card back
    /// into the backlog and yank the user between drafts. The card stays the
    /// active one until the user finally approves or skips.
    pub fn reset_nudge_schedule(&self, action_id: &str) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE actions \
                SET nextNudgeAtMs = ?2, \
                    updatedAt = ?3 \
              WHERE id = ?1",
            params![action_id, now + NUDGE_INTERVAL_MS, now],
        )?;
        Ok(())
    }

    // --- channel_subscriptions (issue #27) ---

    /// Create or update a subscription. Keyed on `(platform, channel_id, account_id)`
    /// so the same channel id can coexist across Slack workspaces. Re-running
    /// with the same triple upserts in place.
    pub fn upsert_subscription(
        &self,
        platform: &str,
        channel_id: &str,
        display_name: &str,
        mode: SubscriptionMode,
        account_id: Option<&str>,
    ) -> StoreResult<ChannelSubscription> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        // NULLs don't equate in SQL; IS is NULL-safe. Use it so lookup matches
        // existing rows whose account_id is NULL (Discord, pre-migration).
        let existing: Option<String> = guard
            .query_row(
                "SELECT id FROM channel_subscriptions \
                 WHERE platform = ?1 AND channel_id = ?2 AND account_id IS ?3",
                params![platform, channel_id, account_id],
                |r| r.get(0),
            )
            .optional()?;
        let id = match existing {
            Some(id) => {
                guard.execute(
                    "UPDATE channel_subscriptions \
                        SET display_name = ?2, mode = ?3, active = 1, updated_at_ms = ?4 \
                      WHERE id = ?1",
                    params![id, display_name, mode.as_str(), now],
                )?;
                id
            }
            None => {
                let id = Uuid::new_v4().to_string();
                guard.execute(
                    "INSERT INTO channel_subscriptions \
                        (id, platform, channel_id, display_name, mode, active, \
                         account_id, created_at_ms, updated_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7, ?7)",
                    params![
                        id,
                        platform,
                        channel_id,
                        display_name,
                        mode.as_str(),
                        account_id,
                        now,
                    ],
                )?;
                id
            }
        };
        drop(guard);
        self.get_subscription(&id)?
            .ok_or_else(|| StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn get_subscription(&self, id: &str) -> StoreResult<Option<ChannelSubscription>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row: Option<ChannelSubscription> = guard
            .query_row(
                "SELECT id, platform, channel_id, display_name, mode, active, \
                        account_id, last_seen_message_id, last_digest_at_ms, \
                        created_at_ms, updated_at_ms \
                   FROM channel_subscriptions \
                  WHERE id = ?1",
                params![id],
                row_to_subscription,
            )
            .optional()?;
        Ok(row)
    }

    /// List active subscriptions for a platform. Callers iterate these per
    /// poll tick. Returns deterministic order by `created_at_ms ASC` so tests
    /// are stable.
    pub fn list_active_subscriptions(
        &self,
        platform: &str,
    ) -> StoreResult<Vec<ChannelSubscription>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, platform, channel_id, display_name, mode, active, \
                    account_id, last_seen_message_id, last_digest_at_ms, \
                    created_at_ms, updated_at_ms \
               FROM channel_subscriptions \
              WHERE active = 1 AND platform = ?1 \
              ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![platform], row_to_subscription)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn update_subscription_mode(
        &self,
        id: &str,
        mode: SubscriptionMode,
    ) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE channel_subscriptions \
                SET mode = ?2, updated_at_ms = ?3 \
              WHERE id = ?1",
            params![id, mode.as_str(), now],
        )?;
        Ok(())
    }

    /// Soft delete — flips `active = 0`. Kept around for audit + to prevent
    /// the unique-pair constraint blocking a later re-subscribe.
    pub fn delete_subscription(&self, id: &str) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE channel_subscriptions \
                SET active = 0, updated_at_ms = ?2 \
              WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    /// Update `last_seen_message_id` after a successful poll. Snowflakes are
    /// time-sortable, so the caller passes the newest message id seen this
    /// tick.
    pub fn update_last_seen_message(
        &self,
        id: &str,
        message_id: &str,
    ) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE channel_subscriptions \
                SET last_seen_message_id = ?2, updated_at_ms = ?3 \
              WHERE id = ?1",
            params![id, message_id, now],
        )?;
        Ok(())
    }

    /// Fetch (from, subject, body) rows for messages in a thread since
    /// `since_ms`. Used by the digest scheduler to aggregate one channel's
    /// recent activity. Oldest first so the prompt reads top-down.
    pub fn recent_emails_for_thread(
        &self,
        thread_id: &str,
        since_ms: i64,
    ) -> StoreResult<Vec<(String, String, String)>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT fromEmail, subject, COALESCE(body, '') \
               FROM emails \
              WHERE threadId = ?1 AND firstSeenAt >= ?2 \
              ORDER BY firstSeenAt ASC",
        )?;
        let rows = stmt.query_map(params![thread_id, since_ms], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn mark_digest_posted(&self, id: &str, at_ms: i64) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE channel_subscriptions \
                SET last_digest_at_ms = ?2, updated_at_ms = ?3 \
              WHERE id = ?1",
            params![id, at_ms, now],
        )?;
        Ok(())
    }

    // --- slack_workspaces ---

    pub fn upsert_slack_workspace(
        &self,
        team_id: &str,
        team_name: &str,
        entity_id: &str,
        connection_id: &str,
        user_id: &str,
    ) -> StoreResult<SlackWorkspace> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        let existing: Option<String> = guard
            .query_row(
                "SELECT id FROM slack_workspaces WHERE team_id = ?1",
                params![team_id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                guard.execute(
                    "UPDATE slack_workspaces \
                        SET team_name = ?2, entity_id = ?3, connection_id = ?4, \
                            user_id = ?5, active = 1 \
                      WHERE id = ?1",
                    params![id, team_name, entity_id, connection_id, user_id],
                )?;
            }
            None => {
                let id = Uuid::new_v4().to_string();
                guard.execute(
                    "INSERT INTO slack_workspaces \
                        (id, team_id, team_name, entity_id, connection_id, \
                         user_id, active, created_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                    params![
                        id,
                        team_id,
                        team_name,
                        entity_id,
                        connection_id,
                        user_id,
                        now,
                    ],
                )?;
            }
        };
        drop(guard);
        self.get_slack_workspace_by_team(team_id)?
            .ok_or_else(|| StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn list_active_slack_workspaces(&self) -> StoreResult<Vec<SlackWorkspace>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, team_id, team_name, entity_id, connection_id, \
                    user_id, active, created_at_ms \
               FROM slack_workspaces \
              WHERE active = 1 \
              ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map([], row_to_slack_workspace)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_slack_workspace_by_team(
        &self,
        team_id: &str,
    ) -> StoreResult<Option<SlackWorkspace>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT id, team_id, team_name, entity_id, connection_id, \
                        user_id, active, created_at_ms \
                   FROM slack_workspaces \
                  WHERE team_id = ?1",
                params![team_id],
                row_to_slack_workspace,
            )
            .optional()?;
        Ok(row)
    }

    pub fn deactivate_slack_workspace(&self, team_id: &str) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE slack_workspaces SET active = 0 WHERE team_id = ?1",
            params![team_id],
        )?;
        Ok(())
    }

    /// Hard delete — used by Disconnect so a subsequent OAuth re-creates a
    /// fresh row instead of reactivating a stale one. We also soft-delete
    /// any subscriptions tied to this workspace so the poll loop stops
    /// trying to read them with credentials that just got nuked.
    pub fn delete_slack_workspace(&self, team_id: &str) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE channel_subscriptions \
                SET active = 0, updated_at_ms = ?2 \
              WHERE platform = 'slack' AND account_id = ?1",
            params![team_id, now],
        )?;
        guard.execute(
            "DELETE FROM slack_workspaces WHERE team_id = ?1",
            params![team_id],
        )?;
        Ok(())
    }
}

fn row_to_slack_workspace(r: &rusqlite::Row) -> rusqlite::Result<SlackWorkspace> {
    Ok(SlackWorkspace {
        id: r.get(0)?,
        team_id: r.get(1)?,
        team_name: r.get(2)?,
        entity_id: r.get(3)?,
        connection_id: r.get(4)?,
        user_id: r.get(5)?,
        active: r.get::<_, i64>(6)? != 0,
        created_at_ms: r.get(7)?,
    })
}

fn row_to_subscription(r: &rusqlite::Row) -> rusqlite::Result<ChannelSubscription> {
    let mode_str: String = r.get(4)?;
    let mode = SubscriptionMode::parse(&mode_str).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            format!("unknown subscription mode: {mode_str}").into(),
        )
    })?;
    Ok(ChannelSubscription {
        id: r.get(0)?,
        platform: r.get(1)?,
        channel_id: r.get(2)?,
        display_name: r.get(3)?,
        mode,
        active: r.get::<_, i64>(5)? != 0,
        account_id: r.get::<_, Option<String>>(6)?,
        last_seen_message_id: r.get::<_, Option<String>>(7)?,
        last_digest_at_ms: r.get::<_, Option<i64>>(8)?,
        created_at_ms: r.get(9)?,
        updated_at_ms: r.get(10)?,
    })
}

#[derive(Debug, Clone)]
pub struct RetryableReply {
    pub action: ActionRecord,
    pub retry_count: i64,
    pub email: Email,
}

/// Fixed interval between nudges for a pending approval card. 6 hours.
pub const NUDGE_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

/// A pending action in the nudge queue, packaged with the email body the
/// approval card was rendered from. `nudge_count` is how many times this card
/// has been surfaced (0 = still in backlog, ≥1 = currently active);
/// `next_nudge_at_ms` is when it next becomes eligible for posting/re-posting.
#[derive(Debug, Clone)]
pub struct PendingNudge {
    pub action: ActionWithEmail,
    pub nudge_count: i64,
    pub next_nudge_at_ms: Option<i64>,
}

fn row_to_pending_nudge(r: &rusqlite::Row) -> rusqlite::Result<PendingNudge> {
    Ok(PendingNudge {
        action: ActionWithEmail {
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
                body: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
                date: r.get::<_, Option<String>>(16)?.unwrap_or_default(),
                account_entity_id: r.get::<_, Option<String>>(17)?,
                platform: r.get::<_, Option<String>>(18)?.unwrap_or_else(|| "gmail".into()),
                kind: r.get::<_, Option<String>>(19)?.unwrap_or_else(|| "dm".into()),
            },
        },
        nudge_count: r.get::<_, i64>(13)?,
        next_nudge_at_ms: r.get::<_, Option<i64>>(14)?,
    })
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

/// Detect whether `table` has a UNIQUE index covering exactly `(col_a, col_b)`.
/// Uses sqlite's PRAGMA index_list + PRAGMA index_info to walk the schema.
fn table_has_unique(
    conn: &Connection,
    table: &str,
    col_a: &str,
    col_b: &str,
) -> StoreResult<bool> {
    let index_list_sql = format!("PRAGMA index_list({table})");
    let mut stmt = conn.prepare(&index_list_sql)?;
    // index_list columns: seq, name, unique, origin, partial
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (index_name, is_unique, _origin) = row?;
        if is_unique == 0 {
            continue;
        }
        let info_sql = format!("PRAGMA index_info({index_name})");
        let mut info = conn.prepare(&info_sql)?;
        let cols: Vec<String> = info
            .query_map([], |r| r.get::<_, String>(2))?
            .collect::<Result<Vec<_>, _>>()?;
        if cols.len() == 2
            && ((cols[0] == col_a && cols[1] == col_b)
                || (cols[0] == col_b && cols[1] == col_a))
        {
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

    // --- nudge loop ---

    #[test]
    fn pending_action_seeds_nudge_schedule() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("draft"), ActionStatus::Pending)
            .unwrap();
        // Next nudge is roughly 6h out — query directly to verify.
        let conn = Connection::open(_f.path()).unwrap();
        let next: Option<i64> = conn
            .query_row(
                "SELECT nextNudgeAtMs FROM actions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(next.is_some(), "pending action must have a nudge timer");
        let count: i64 = conn
            .query_row(
                "SELECT nudgeCount FROM actions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn non_pending_action_skips_nudge_schedule() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let id = s
            .log_action("m1", None, "a@b.com", "s", None, None, ActionStatus::DryRun)
            .unwrap();
        let conn = Connection::open(_f.path()).unwrap();
        let next: Option<i64> = conn
            .query_row(
                "SELECT nextNudgeAtMs FROM actions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(next.is_none(), "dry-run actions should never be nudged");
    }

    #[test]
    fn find_next_to_promote_returns_oldest_due_unpromoted() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        s.upsert_email(&sample_email("m2")).unwrap();
        let id1 = s
            .log_action("m1", None, "a@b.com", "s1", None, Some("d1"), ActionStatus::Pending)
            .unwrap();
        // Slight delay so m2's createdAt > m1's.
        std::thread::sleep(std::time::Duration::from_millis(2));
        let _id2 = s
            .log_action("m2", None, "a@b.com", "s2", None, Some("d2"), ActionStatus::Pending)
            .unwrap();
        // Not yet due — backfill is createdAt + 6h.
        let nxt = s.find_next_to_promote(now_millis()).unwrap();
        assert!(nxt.is_none(), "fresh actions shouldn't be due yet");
        // Far future → both due. Oldest first.
        let future = now_millis() + NUDGE_INTERVAL_MS + 1000;
        let nxt = s.find_next_to_promote(future).unwrap().expect("expected next");
        assert_eq!(nxt.action.action.id, id1, "oldest createdAt wins");
        assert_eq!(nxt.nudge_count, 0);
    }

    #[test]
    fn find_active_nudge_returns_promoted_pending() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("d"), ActionStatus::Pending)
            .unwrap();
        assert!(s.find_active_nudge().unwrap().is_none());
        s.record_nudge(&id, now_millis() + NUDGE_INTERVAL_MS).unwrap();
        let active = s.find_active_nudge().unwrap().expect("expected active card");
        assert_eq!(active.action.action.id, id);
        assert_eq!(active.nudge_count, 1);
        assert!(active.next_nudge_at_ms.is_some());
    }

    #[test]
    fn find_active_nudge_excludes_terminal_status() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("d"), ActionStatus::Pending)
            .unwrap();
        s.record_nudge(&id, now_millis() + NUDGE_INTERVAL_MS).unwrap();
        s.update_action_status(&id, ActionStatus::Approved, None, None)
            .unwrap();
        assert!(
            s.find_active_nudge().unwrap().is_none(),
            "approved card should no longer be active"
        );
    }

    #[test]
    fn count_pending_overdue_counts_all_due() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        s.upsert_email(&sample_email("m2")).unwrap();
        s.log_action("m1", None, "a@b.com", "s", None, None, ActionStatus::Pending)
            .unwrap();
        let id2 = s
            .log_action("m2", None, "a@b.com", "s", None, None, ActionStatus::Pending)
            .unwrap();
        // Promote m2 so it's active. Both should still count as overdue when
        // we query past the timer.
        s.record_nudge(&id2, now_millis() + NUDGE_INTERVAL_MS).unwrap();
        let future = now_millis() + 2 * NUDGE_INTERVAL_MS;
        assert_eq!(s.count_pending_overdue(future).unwrap(), 2);
    }

    #[test]
    fn record_nudge_bumps_count_and_pushes_next() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("d"), ActionStatus::Pending)
            .unwrap();
        let now = now_millis();
        s.record_nudge(&id, now + NUDGE_INTERVAL_MS).unwrap();
        s.record_nudge(&id, now + 2 * NUDGE_INTERVAL_MS).unwrap();
        let conn = Connection::open(_f.path()).unwrap();
        let (count, next): (i64, i64) = conn
            .query_row(
                "SELECT nudgeCount, nextNudgeAtMs FROM actions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(next, now + 2 * NUDGE_INTERVAL_MS);
    }

    #[test]
    fn reset_nudge_schedule_defers_without_zeroing_count() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("d"), ActionStatus::Pending)
            .unwrap();
        // Promote, then re-nudge once → count = 2.
        s.record_nudge(&id, now_millis()).unwrap();
        s.record_nudge(&id, now_millis()).unwrap();
        // Revise: defer timer 6h, KEEP count (card stays the active one).
        s.reset_nudge_schedule(&id).unwrap();
        let conn = Connection::open(_f.path()).unwrap();
        let (count, next): (i64, i64) = conn
            .query_row(
                "SELECT nudgeCount, nextNudgeAtMs FROM actions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(count, 2, "revise must not zero nudgeCount");
        let now = now_millis();
        assert!(next >= now + NUDGE_INTERVAL_MS - 5_000);
        assert!(next <= now + NUDGE_INTERVAL_MS + 5_000);
    }

    // --- channel_subscriptions ---

    #[test]
    fn upsert_subscription_creates_then_updates() {
        let (s, _f) = fresh_store();
        let sub = s
            .upsert_subscription(
                "discord",
                "ch1",
                "DM with alice",
                SubscriptionMode::Priority,
                None,
            )
            .unwrap();
        assert_eq!(sub.platform, "discord");
        assert_eq!(sub.channel_id, "ch1");
        assert_eq!(sub.mode, SubscriptionMode::Priority);
        assert!(sub.active);
        assert!(sub.last_seen_message_id.is_none());

        let updated = s
            .upsert_subscription(
                "discord",
                "ch1",
                "DM with alice (renamed)",
                SubscriptionMode::Digest,
                None,
            )
            .unwrap();
        assert_eq!(updated.id, sub.id, "same (platform, channel_id) re-upserts in place");
        assert_eq!(updated.display_name, "DM with alice (renamed)");
        assert_eq!(updated.mode, SubscriptionMode::Digest);
    }

    #[test]
    fn list_active_subscriptions_filters_by_platform_and_active() {
        let (s, _f) = fresh_store();
        let d1 = s
            .upsert_subscription("discord", "d1", "d1", SubscriptionMode::Priority, None)
            .unwrap();
        s.upsert_subscription("discord", "d2", "d2", SubscriptionMode::Digest, None)
            .unwrap();
        s.upsert_subscription("slack", "s1", "s1", SubscriptionMode::StoreOnly, Some("T1"))
            .unwrap();

        let discord_subs = s.list_active_subscriptions("discord").unwrap();
        assert_eq!(discord_subs.len(), 2);
        assert!(discord_subs.iter().all(|x| x.platform == "discord"));

        s.delete_subscription(&d1.id).unwrap();
        let after_delete = s.list_active_subscriptions("discord").unwrap();
        assert_eq!(after_delete.len(), 1, "soft-deleted subs excluded");
        assert_eq!(after_delete[0].channel_id, "d2");
    }

    #[test]
    fn update_subscription_mode_persists() {
        let (s, _f) = fresh_store();
        let sub = s
            .upsert_subscription("discord", "ch1", "dm", SubscriptionMode::Priority, None)
            .unwrap();
        s.update_subscription_mode(&sub.id, SubscriptionMode::StoreOnly)
            .unwrap();
        let reloaded = s.get_subscription(&sub.id).unwrap().unwrap();
        assert_eq!(reloaded.mode, SubscriptionMode::StoreOnly);
    }

    #[test]
    fn update_last_seen_message_persists() {
        let (s, _f) = fresh_store();
        let sub = s
            .upsert_subscription("discord", "ch1", "dm", SubscriptionMode::Priority, None)
            .unwrap();
        s.update_last_seen_message(&sub.id, "1234567890").unwrap();
        let reloaded = s.get_subscription(&sub.id).unwrap().unwrap();
        assert_eq!(reloaded.last_seen_message_id.as_deref(), Some("1234567890"));
    }

    #[test]
    fn mark_digest_posted_persists() {
        let (s, _f) = fresh_store();
        let sub = s
            .upsert_subscription("discord", "ch1", "dm", SubscriptionMode::Digest, None)
            .unwrap();
        s.mark_digest_posted(&sub.id, 1776806000000).unwrap();
        let reloaded = s.get_subscription(&sub.id).unwrap().unwrap();
        assert_eq!(reloaded.last_digest_at_ms, Some(1776806000000));
    }

    #[test]
    fn delete_then_reupsert_restores_same_row() {
        // Soft-delete preserves the (platform, channel_id, account_id) unique
        // triple; a subsequent upsert should flip active back to 1 and
        // overwrite fields.
        let (s, _f) = fresh_store();
        let sub = s
            .upsert_subscription("discord", "ch1", "first", SubscriptionMode::Priority, None)
            .unwrap();
        s.delete_subscription(&sub.id).unwrap();
        let restored = s
            .upsert_subscription("discord", "ch1", "second", SubscriptionMode::Digest, None)
            .unwrap();
        assert_eq!(restored.id, sub.id);
        assert_eq!(restored.display_name, "second");
        assert_eq!(restored.mode, SubscriptionMode::Digest);
        assert!(restored.active);
    }

    #[test]
    fn same_channel_distinct_workspaces_coexist() {
        let (s, _f) = fresh_store();
        let a = s
            .upsert_subscription("slack", "C_SAME", "general@A", SubscriptionMode::Priority, Some("T_A"))
            .unwrap();
        let b = s
            .upsert_subscription("slack", "C_SAME", "general@B", SubscriptionMode::Priority, Some("T_B"))
            .unwrap();
        assert_ne!(a.id, b.id, "same channel_id across workspaces yields distinct rows");
        let list = s.list_active_subscriptions("slack").unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn slack_workspace_upsert_is_idempotent() {
        let (s, _f) = fresh_store();
        let w1 = s
            .upsert_slack_workspace("T1", "Team1", "e1", "c1", "U1")
            .unwrap();
        let w2 = s
            .upsert_slack_workspace("T1", "Team1 renamed", "e1", "c1", "U1")
            .unwrap();
        assert_eq!(w1.id, w2.id);
        assert_eq!(w2.team_name, "Team1 renamed");
        let list = s.list_active_slack_workspaces().unwrap();
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn subscription_mode_parse_round_trip() {
        for m in [
            SubscriptionMode::Priority,
            SubscriptionMode::Digest,
            SubscriptionMode::StoreOnly,
        ] {
            assert_eq!(SubscriptionMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(SubscriptionMode::parse("garbage"), None);
    }
}
