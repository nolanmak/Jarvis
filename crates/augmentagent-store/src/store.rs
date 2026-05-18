use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;
use uuid::Uuid;

use crate::models::{
    Account, ActionRecord, ActionStatus, ChannelSubscription, DriveAccount, Email, LearnedPattern,
    LinkedInConnectionSync, PhoneIdentity, RateAuditRow, RateHalt, RateWarmup, SlackWorkspace,
    SubscriptionMode, TelegramBot, ToneExample, ToneProfile, TriageResult,
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
        // Weekly Orchid invoice automation: a tiny key/value bag for the
        // recipient email, the sequential invoice counter (starts at 35 — the
        // backlog #29-#34 was generated by hand), the Composio sending entity,
        // and a marker of the last week already billed (idempotency guard).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS invoice_config (\
                 key        TEXT PRIMARY KEY,\
                 value      TEXT NOT NULL,\
                 updated_at INTEGER NOT NULL\
             )",
            [],
        )?;
        // `last_billed_week_end` is seeded to 2026-05-17 — the manual backlog
        // (#29-#34) covered through that Sunday, so on a fresh db the first
        // automated send is the NEXT Sunday (05/24 → #35). Prevents an
        // immediate duplicate of the already-billed 05/10–05/17 week.
        // `auto_send_enabled` is the master kill switch — seeded 'false' so a
        // fresh deploy NEVER auto-sends until it's explicitly turned on
        // (dashboard / `!invoice autosend on` / `invoice set-auto-send`).
        // INSERT OR IGNORE is per-row, so existing deployed dbs that already
        // have the other keys still pick up this new row as 'false'.
        conn.execute(
            "INSERT OR IGNORE INTO invoice_config (key, value, updated_at) VALUES \
                 ('recipient_email', 'REDACTED', ?1),\
                 ('invoice_counter', '35', ?1),\
                 ('from_entity', '', ?1),\
                 ('last_billed_week_end', '2026-05-17', ?1),\
                 ('auto_send_enabled', 'false', ?1)",
            params![now_millis()],
        )?;

        // ----------------------------------------------------------------
        // Wave-A foundation: tables for the parallel feature PRs branching
        // off `foundation/swarm-v1`. Schemas are pulled verbatim from each
        // research issue body. Tables are independent — order doesn't matter.
        // Every CREATE is `IF NOT EXISTS`, so re-running migrate is a no-op.
        // ----------------------------------------------------------------

        // #73 — per-recipient tone-mirroring v1.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tone_profiles (\
                 id                       TEXT PRIMARY KEY,\
                 scope_kind               TEXT NOT NULL CHECK (scope_kind IN ('global','domain','recipient')),\
                 scope_value              TEXT NOT NULL,\
                 account_entity_id        TEXT,\
                 summary                  TEXT NOT NULL,\
                 exemplar_ids             TEXT NOT NULL DEFAULT '[]',\
                 sample_count             INTEGER NOT NULL DEFAULT 0,\
                 sample_count_at_refresh  INTEGER NOT NULL DEFAULT 0,\
                 last_refreshed_at        INTEGER NOT NULL,\
                 created_at_ms            INTEGER NOT NULL,\
                 updated_at_ms            INTEGER NOT NULL,\
                 UNIQUE(scope_kind, scope_value, account_entity_id)\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tone_profiles_scope \
                ON tone_profiles(scope_kind, scope_value)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tone_examples (\
                 id                  TEXT PRIMARY KEY,\
                 source              TEXT NOT NULL CHECK (source IN ('sent_backfill','user_edit','approved_clean')),\
                 action_id           TEXT,\
                 message_id          TEXT,\
                 account_entity_id   TEXT NOT NULL,\
                 recipient_email     TEXT NOT NULL,\
                 recipient_domain    TEXT NOT NULL,\
                 subject             TEXT,\
                 body                TEXT NOT NULL,\
                 body_chars          INTEGER NOT NULL,\
                 sent_at_ms          INTEGER NOT NULL,\
                 ingested_at_ms      INTEGER NOT NULL,\
                 weight              REAL NOT NULL DEFAULT 1.0,\
                 FOREIGN KEY (action_id) REFERENCES actions(id) ON DELETE SET NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tone_examples_recipient \
                ON tone_examples(recipient_email, sent_at_ms DESC)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tone_examples_domain \
                ON tone_examples(recipient_domain, sent_at_ms DESC)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tone_examples_account_recent \
                ON tone_examples(account_entity_id, sent_at_ms DESC)",
            [],
        )?;

        // #37 — draft revision history for tone learning + eval.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS draft_revisions (\
                 id                 TEXT PRIMARY KEY,\
                 actionId           TEXT NOT NULL REFERENCES actions(id) ON DELETE CASCADE,\
                 iteration          INTEGER NOT NULL,\
                 draftBody          TEXT NOT NULL,\
                 feedbackText       TEXT,\
                 presetId           TEXT,\
                 outcome            TEXT NOT NULL,\
                 modelId            TEXT NOT NULL,\
                 promptTokens       INTEGER,\
                 completionTokens   INTEGER,\
                 createdAt          INTEGER NOT NULL,\
                 UNIQUE(actionId, iteration)\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_draft_revisions_outcome \
                ON draft_revisions(outcome, createdAt)",
            [],
        )?;

        // #83 — RateGovernor (per-platform rate events, halts, warmup).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS rate_events (\
                 id              TEXT PRIMARY KEY,\
                 platform        TEXT NOT NULL,\
                 action_kind     TEXT NOT NULL,\
                 account_id      TEXT NOT NULL,\
                 occurred_at_ms  INTEGER NOT NULL,\
                 status          TEXT NOT NULL,\
                 cause           TEXT NOT NULL,\
                 target_id       TEXT,\
                 meta_json       TEXT\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_rate_events_window \
                ON rate_events(platform, action_kind, account_id, occurred_at_ms)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_rate_events_audit \
                ON rate_events(platform, occurred_at_ms)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS rate_halts (\
                 platform               TEXT PRIMARY KEY,\
                 paused_until_ms        INTEGER NOT NULL,\
                 reason                 TEXT NOT NULL,\
                 triggered_by_event_id  TEXT,\
                 created_at_ms          INTEGER NOT NULL,\
                 acknowledged_at_ms     INTEGER\
             )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS rate_warmup (\
                 platform              TEXT NOT NULL,\
                 account_id            TEXT NOT NULL,\
                 warmup_started_at_ms  INTEGER NOT NULL,\
                 PRIMARY KEY (platform, account_id)\
             )",
            [],
        )?;

        // #74 — Telegram Bot API per-bot state (long-poll cursor).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS telegram_bots (\
                 id              TEXT PRIMARY KEY,\
                 bot_id          INTEGER NOT NULL UNIQUE,\
                 bot_username    TEXT NOT NULL,\
                 owner_chat_id   INTEGER NOT NULL,\
                 last_update_id  INTEGER NOT NULL DEFAULT 0,\
                 active          INTEGER NOT NULL DEFAULT 1,\
                 created_at_ms   INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_telegram_bots_active \
                ON telegram_bots(active)",
            [],
        )?;

        // #74 — WhatsApp linked devices + per-chat outbound/inbound allowlists.
        // Inbound allowlist comes from review feedback: even reading a chat
        // requires explicit opt-in for ban-risk reasons.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS whatsapp_devices (\
                 id                TEXT PRIMARY KEY,\
                 phone             TEXT NOT NULL UNIQUE,\
                 device_jid        TEXT NOT NULL,\
                 user_jid          TEXT NOT NULL,\
                 paired_at_ms      INTEGER NOT NULL,\
                 last_event_at_ms  INTEGER NOT NULL DEFAULT 0,\
                 session_status    TEXT NOT NULL DEFAULT 'paired',\
                 active            INTEGER NOT NULL DEFAULT 1,\
                 created_at_ms     INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_whatsapp_devices_active \
                ON whatsapp_devices(active)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS whatsapp_outbound_allowlist (\
                 chat_jid       TEXT PRIMARY KEY,\
                 enabled_at_ms  INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS whatsapp_inbound_allowlist (\
                 chat_jid       TEXT PRIMARY KEY,\
                 enabled_at_ms  INTEGER NOT NULL\
             )",
            [],
        )?;

        // #81 — Proactive CRM signals + per-scan run cursor.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS proactive_signals (\
                 id                     TEXT PRIMARY KEY,\
                 kind                   TEXT NOT NULL,\
                 person_slug            TEXT,\
                 urgency                TEXT NOT NULL,\
                 headline               TEXT NOT NULL,\
                 detail                 TEXT NOT NULL,\
                 suggested_action_json  TEXT,\
                 status                 TEXT NOT NULL,\
                 snooze_until_ms        INTEGER,\
                 dedup_key              TEXT NOT NULL,\
                 created_at_ms          INTEGER NOT NULL,\
                 dispatched_at_ms       INTEGER\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_proactive_signals_status_created \
                ON proactive_signals(status, created_at_ms)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_proactive_signals_dedup_recent \
                ON proactive_signals(dedup_key, created_at_ms)",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_proactive_signals_person \
                ON proactive_signals(person_slug)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS proactive_scan_runs (\
                 scan_id         TEXT PRIMARY KEY,\
                 last_run_at_ms  INTEGER NOT NULL\
             )",
            [],
        )?;

        // #79 — Twitter/X GraphQL queryId cache (rotated by X every 2-6 wk).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS twitter_query_ids (\
                 operation      TEXT PRIMARY KEY,\
                 query_id       TEXT NOT NULL,\
                 last_seen_at   INTEGER NOT NULL\
             )",
            [],
        )?;

        // #77 — LinkedIn outbound action audit log (post / comment / like /
        // connection_invite / dm / profile_view), drives daily/hourly caps.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS linkedin_action_log (\
                 id              TEXT PRIMARY KEY,\
                 action_kind     TEXT NOT NULL,\
                 target_urn      TEXT,\
                 status          TEXT NOT NULL,\
                 occurred_at_ms  INTEGER NOT NULL,\
                 meta_json       TEXT\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_linkedin_action_log_window \
                ON linkedin_action_log(action_kind, occurred_at_ms)",
            [],
        )?;

        // #58 / #74-engagement — scheduled outbound posts (cross-platform).
        conn.execute(
            "CREATE TABLE IF NOT EXISTS scheduled_posts (\
                 id              TEXT PRIMARY KEY,\
                 platform        TEXT NOT NULL,\
                 body            TEXT NOT NULL,\
                 media_paths     TEXT,\
                 fire_at_ms      INTEGER NOT NULL,\
                 status          TEXT NOT NULL,\
                 approval_msg    TEXT,\
                 posted_at_ms    INTEGER,\
                 external_id     TEXT,\
                 thread_parent   TEXT REFERENCES scheduled_posts(id) ON DELETE SET NULL,\
                 created_at_ms   INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_scheduled_posts_fire \
                ON scheduled_posts(status, fire_at_ms)",
            [],
        )?;
        // Multi-tenant Google Drive (Composio). Inert in prod: empty + unread
        // unless a tenant connects a Drive account. Same proven-safe pattern
        // as the dormant wave-A tables already shipping in prod.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS drive_accounts (\
                 id            TEXT PRIMARY KEY,\
                 connection_id TEXT NOT NULL,\
                 entity_id     TEXT NOT NULL,\
                 email         TEXT,\
                 label         TEXT,\
                 active        INTEGER NOT NULL DEFAULT 1,\
                 created_at_ms INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_drive_accounts_active \
                ON drive_accounts(active)",
            [],
        )?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS drive_sync_state (\
                 entity_id     TEXT PRIMARY KEY,\
                 page_token    TEXT NOT NULL,\
                 updated_at_ms INTEGER NOT NULL\
             )",
            [],
        )?;

        // ---------------------------------------------------------------
        // CRM ingestion (#61 LinkedIn connections / #62 contacts / #64
        // signature backfill). All additive + dormant in prod until the
        // respective CLI command runs; same proven-safe pattern as the
        // wave-A tables above.
        // ---------------------------------------------------------------

        // #61 — LinkedIn 1st-degree connection sync cursor. One row keyed by
        // the user's own member urn (`account_id`); `last_full_sync_ms`
        // gates full-vs-delta mode, `cursor_start` resumes a paginated full
        // sync that was interrupted mid-run.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS linkedin_connection_sync (\
                 account_id          TEXT PRIMARY KEY,\
                 last_full_sync_ms   INTEGER,\
                 last_delta_sync_ms  INTEGER,\
                 cursor_start        INTEGER NOT NULL DEFAULT 0,\
                 last_synced_count   INTEGER NOT NULL DEFAULT 0,\
                 updated_at_ms       INTEGER NOT NULL\
             )",
            [],
        )?;

        // #62 — generic contacts sync token (Google People `syncToken` or
        // CardDAV `getctag`), keyed by `(backend, account_id)`.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS contacts_sync_state (\
                 backend       TEXT NOT NULL,\
                 account_id    TEXT NOT NULL,\
                 sync_token    TEXT,\
                 updated_at_ms INTEGER NOT NULL,\
                 PRIMARY KEY (backend, account_id)\
             )",
            [],
        )?;

        // #62 — phone→person reverse index consulted by message-triage
        // before creating a new wiki page. `phone` is E.164-normalized;
        // unique so re-ingest is an upsert, not a duplicate.
        conn.execute(
            "CREATE TABLE IF NOT EXISTS identity_phone (\
                 phone         TEXT PRIMARY KEY,\
                 person_slug   TEXT NOT NULL,\
                 display_name  TEXT,\
                 source        TEXT NOT NULL,\
                 updated_at_ms INTEGER NOT NULL\
             )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_identity_phone_slug \
                ON identity_phone(person_slug)",
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

    /// True iff there's already an in-flight action for this message — either
    /// `pending` (awaiting Discord approval) or `error` (will be picked up by
    /// the retry tick). The poll loop uses this to avoid spawning duplicate
    /// actions for the same email while one is still mid-flight.
    pub fn has_open_action(&self, message_id: &str) -> StoreResult<bool> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row: Option<i64> = guard
            .query_row(
                "SELECT 1 FROM actions \
                 WHERE messageId = ?1 AND status IN ('pending', 'error') \
                 LIMIT 1",
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

    /// Persist a (draft, feedback, revised_draft) triple for action `action_id`
    /// (#37 — draft-quality feedback loop).
    ///
    /// Writes two rows to `draft_revisions` in a single transaction:
    /// 1. The pre-Revise draft as iteration N with `outcome = 'superseded'`
    ///    and `feedbackText = NULL`.
    /// 2. The post-Revise draft as iteration N+1 with `outcome = 'pending'`
    ///    and `feedbackText = feedback`.
    ///
    /// Iteration numbering is contiguous per action: it picks `MAX(iteration)+1`
    /// from existing rows, defaulting to 0 when this is the first revise on a
    /// fresh action. Using two rows keeps the schema's `(actionId, iteration)`
    /// invariant clean and lets downstream consumers read the chronology
    /// without inferring it.
    ///
    /// Returns the id of the **revised** (newest) row — that's the one
    /// downstream tone-mirror / clusterer code refers to.
    pub fn record_revision_triple(
        &self,
        action_id: &str,
        original_draft: &str,
        feedback: &str,
        revised_draft: &str,
    ) -> StoreResult<String> {
        let now = now_millis();
        let revised_id = Uuid::new_v4().to_string();
        let mut guard = self.conn.lock().expect("store mutex poisoned");
        let tx = guard.transaction()?;
        // Pick the next iteration. If no prior rows exist for this action the
        // pre-Revise draft is iteration 0 and the revised one is iteration 1.
        let max_iter: Option<i64> = tx
            .query_row(
                "SELECT MAX(iteration) FROM draft_revisions WHERE actionId = ?1",
                params![action_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        let next_iter = max_iter.map(|i| i + 1).unwrap_or(0);
        // The pre-Revise draft is only inserted when there is no existing row
        // at iteration `next_iter - 0` for this action — i.e. on the first
        // Revise. On subsequent revises the previous iteration's row is
        // already there (with outcome = 'pending'); we just flip it to
        // 'superseded' so the chain stays consistent.
        if next_iter == 0 {
            let original_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO draft_revisions \
                   (id, actionId, iteration, draftBody, feedbackText, presetId, \
                    outcome, modelId, promptTokens, completionTokens, createdAt) \
                 VALUES (?1, ?2, 0, ?3, NULL, NULL, 'superseded', '', NULL, NULL, ?4)",
                params![original_id, action_id, original_draft, now],
            )?;
        } else {
            tx.execute(
                "UPDATE draft_revisions \
                    SET outcome = 'superseded' \
                  WHERE actionId = ?1 AND iteration = ?2 AND outcome = 'pending'",
                params![action_id, next_iter - 1],
            )?;
        }
        let revised_iter = if next_iter == 0 { 1 } else { next_iter };
        tx.execute(
            "INSERT INTO draft_revisions \
               (id, actionId, iteration, draftBody, feedbackText, presetId, \
                outcome, modelId, promptTokens, completionTokens, createdAt) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'pending', '', NULL, NULL, ?6)",
            params![revised_id, action_id, revised_iter, revised_draft, feedback, now],
        )?;
        tx.commit()?;
        Ok(revised_id)
    }

    /// All revision rows for `action_id`, oldest iteration first. Backs the
    /// downstream tone-mirror corpus (#73) and the recurring-feedback
    /// clusterer (#37 Phase 3).
    pub fn list_revisions_for_action(
        &self,
        action_id: &str,
    ) -> StoreResult<Vec<RevisionRecord>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, actionId, iteration, draftBody, feedbackText, presetId, \
                    outcome, modelId, promptTokens, completionTokens, createdAt \
               FROM draft_revisions \
              WHERE actionId = ?1 \
              ORDER BY iteration ASC",
        )?;
        let rows = stmt.query_map(params![action_id], row_to_revision_record)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Revision rows whose `feedbackText` is non-NULL, created within the last
    /// `since_ms` milliseconds. Backs the recurring-feedback clusterer
    /// (`augmentagent drafts feedback-clusters`).
    pub fn list_recent_feedback(
        &self,
        since_ms: i64,
    ) -> StoreResult<Vec<RevisionRecord>> {
        let cutoff = now_millis() - since_ms;
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, actionId, iteration, draftBody, feedbackText, presetId, \
                    outcome, modelId, promptTokens, completionTokens, createdAt \
               FROM draft_revisions \
              WHERE feedbackText IS NOT NULL \
                AND createdAt >= ?1 \
              ORDER BY createdAt DESC",
        )?;
        let rows = stmt.query_map(params![cutoff], row_to_revision_record)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
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

    // --- Multi-tenant Google Drive (Composio) ---------------------------
    // Inert in prod (no rows) — only a tenant that connects Drive uses these.

    /// Insert/replace a connected Drive account (dedup by `connection_id`).
    pub fn add_drive_account(
        &self,
        connection_id: &str,
        entity_id: &str,
        email: Option<&str>,
        label: Option<&str>,
    ) -> StoreResult<String> {
        let id = format!("drive-{connection_id}");
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO drive_accounts \
                 (id, connection_id, entity_id, email, label, active, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6) \
             ON CONFLICT(id) DO UPDATE SET \
                 entity_id = excluded.entity_id, email = excluded.email, \
                 label = excluded.label, active = 1",
            params![id, connection_id, entity_id, email, label, now_millis()],
        )?;
        Ok(id)
    }

    pub fn get_active_drive_accounts(&self) -> StoreResult<Vec<DriveAccount>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, connection_id, entity_id, email, label, active \
               FROM drive_accounts WHERE active = 1 ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DriveAccount {
                id: r.get(0)?,
                connection_id: r.get(1)?,
                entity_id: r.get(2)?,
                email: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                label: r.get::<_, Option<String>>(4)?,
                active: r.get::<_, i64>(5)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Persisted Drive `changes.list` cursor for an entity (None on first poll).
    pub fn get_drive_sync_token(&self, entity_id: &str) -> StoreResult<Option<String>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let tok: Option<String> = guard
            .query_row(
                "SELECT page_token FROM drive_sync_state WHERE entity_id = ?1",
                params![entity_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(tok)
    }

    pub fn set_drive_sync_token(&self, entity_id: &str, page_token: &str) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO drive_sync_state (entity_id, page_token, updated_at_ms) \
                 VALUES (?1, ?2, ?3) \
             ON CONFLICT(entity_id) DO UPDATE SET \
                 page_token = excluded.page_token, updated_at_ms = excluded.updated_at_ms",
            params![entity_id, page_token, now_millis()],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // #61 — LinkedIn connection-sync cursor.
    // ---------------------------------------------------------------

    /// Read the connection-sync cursor for `account_id` (the user's own
    /// member urn). `None` means this account has never synced — caller
    /// should run a full sync.
    pub fn get_linkedin_connection_sync(
        &self,
        account_id: &str,
    ) -> StoreResult<Option<LinkedInConnectionSync>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT account_id, last_full_sync_ms, last_delta_sync_ms, \
                        cursor_start, last_synced_count \
                   FROM linkedin_connection_sync WHERE account_id = ?1",
                params![account_id],
                |r| {
                    Ok(LinkedInConnectionSync {
                        account_id: r.get(0)?,
                        last_full_sync_ms: r.get(1)?,
                        last_delta_sync_ms: r.get(2)?,
                        cursor_start: r.get(3)?,
                        last_synced_count: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Upsert the connection-sync cursor. Pass the full desired state — this
    /// is a blind overwrite of the mutable columns (the caller owns the
    /// full-vs-delta decision).
    pub fn upsert_linkedin_connection_sync(
        &self,
        s: &LinkedInConnectionSync,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO linkedin_connection_sync \
                 (account_id, last_full_sync_ms, last_delta_sync_ms, \
                  cursor_start, last_synced_count, updated_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT(account_id) DO UPDATE SET \
                 last_full_sync_ms = excluded.last_full_sync_ms, \
                 last_delta_sync_ms = excluded.last_delta_sync_ms, \
                 cursor_start = excluded.cursor_start, \
                 last_synced_count = excluded.last_synced_count, \
                 updated_at_ms = excluded.updated_at_ms",
            params![
                s.account_id,
                s.last_full_sync_ms,
                s.last_delta_sync_ms,
                s.cursor_start,
                s.last_synced_count,
                now_millis(),
            ],
        )?;
        Ok(())
    }

    // ---------------------------------------------------------------
    // #62 — contacts sync token + phone reverse index.
    // ---------------------------------------------------------------

    /// Read the sync token (Google People `syncToken` / CardDAV `getctag`)
    /// for `(backend, account_id)`. `None` → full sync on next run.
    pub fn get_contacts_sync_token(
        &self,
        backend: &str,
        account_id: &str,
    ) -> StoreResult<Option<String>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let tok: Option<String> = guard
            .query_row(
                "SELECT sync_token FROM contacts_sync_state \
                   WHERE backend = ?1 AND account_id = ?2",
                params![backend, account_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(tok)
    }

    pub fn set_contacts_sync_token(
        &self,
        backend: &str,
        account_id: &str,
        token: &str,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO contacts_sync_state \
                 (backend, account_id, sync_token, updated_at_ms) \
                 VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(backend, account_id) DO UPDATE SET \
                 sync_token = excluded.sync_token, \
                 updated_at_ms = excluded.updated_at_ms",
            params![backend, account_id, token, now_millis()],
        )?;
        Ok(())
    }

    /// Reverse-lookup a person by E.164 phone. The message-triage path calls
    /// this *before* creating a new wiki page so a known phone resolves to an
    /// existing contact instead of fragmenting identity.
    pub fn lookup_person_by_phone(
        &self,
        phone_e164: &str,
    ) -> StoreResult<Option<PhoneIdentity>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT phone, person_slug, display_name, source \
                   FROM identity_phone WHERE phone = ?1",
                params![phone_e164],
                |r| {
                    Ok(PhoneIdentity {
                        phone: r.get(0)?,
                        person_slug: r.get(1)?,
                        display_name: r.get(2)?,
                        source: r.get(3)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Upsert a phone→person index row. Idempotent: re-ingesting the same
    /// contact rewrites the (slug, name) for that phone rather than
    /// duplicating.
    pub fn upsert_phone_identity(&self, p: &PhoneIdentity) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO identity_phone \
                 (phone, person_slug, display_name, source, updated_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5) \
             ON CONFLICT(phone) DO UPDATE SET \
                 person_slug = excluded.person_slug, \
                 display_name = excluded.display_name, \
                 source = excluded.source, \
                 updated_at_ms = excluded.updated_at_ms",
            params![
                p.phone,
                p.person_slug,
                p.display_name,
                p.source,
                now_millis(),
            ],
        )?;
        Ok(())
    }

    /// Backfill the human-readable Gmail address for a connected account.
    /// The OAuth connect flow never captured it (Composio doesn't return it
    /// on the connection), so the dashboard + entity picker show opaque IDs
    /// until this is populated from a `GMAIL_GET_PROFILE` lookup.
    pub fn update_gmail_account_email(&self, id: &str, email: &str) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE gmail_accounts SET email = ?2 WHERE id = ?1",
            params![id, email],
        )?;
        Ok(())
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

    /// #64 — `(message_id, from_email, body)` for emails first seen on/after
    /// `since_ms`, newest first. Backs `backfill signatures`: it mines each
    /// body's signature block for role/title/company/phone. Idempotent at
    /// the call site (the wiki merge is fill-blanks-only).
    pub fn email_bodies_since(
        &self,
        since_ms: i64,
        limit: i64,
    ) -> StoreResult<Vec<(String, String, String)>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT messageId, fromEmail, body \
             FROM emails \
             WHERE firstSeenAt >= ?1 \
             ORDER BY firstSeenAt DESC \
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![since_ms, limit], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
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
    /// row with `nudgeCount = 0`. Returns None when the backlog is empty.
    ///
    /// The 6h `nextNudgeAtMs` interval only throttles *re-nudges* of the
    /// already-active card (see `find_active_nudge` + `record_nudge`). Initial
    /// promotion has no throttle — when the user resolves a card we want the
    /// next one surfaced immediately, regardless of its age. `now_ms` is
    /// retained for API stability and possible future filters.
    pub fn find_next_to_promote(&self, _now_ms: i64) -> StoreResult<Option<PendingNudge>> {
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
                 ORDER BY a.createdAt ASC \
                 LIMIT 1",
                [],
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

    /// Read a single invoice-config value (None if the key is absent).
    pub fn get_invoice_config(&self, key: &str) -> StoreResult<Option<String>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let v: Option<String> = guard
            .query_row(
                "SELECT value FROM invoice_config WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Upsert a single invoice-config value.
    pub fn set_invoice_config(&self, key: &str, value: &str) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO invoice_config (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                 updated_at = excluded.updated_at",
            params![key, value, now_millis()],
        )?;
        Ok(())
    }

    /// The next invoice number to use (defaults to 35 if unset/corrupt).
    pub fn invoice_counter(&self) -> StoreResult<u32> {
        Ok(self
            .get_invoice_config("invoice_counter")?
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(35))
    }

    /// Atomically take the current invoice number and advance the counter.
    /// Returns the number to use for the invoice being generated now.
    pub fn next_invoice_number(&self) -> StoreResult<u32> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let cur: u32 = guard
            .query_row(
                "SELECT value FROM invoice_config WHERE key = 'invoice_counter'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .and_then(|s| s.parse().ok())
            .unwrap_or(35);
        guard.execute(
            "INSERT INTO invoice_config (key, value, updated_at) \
                 VALUES ('invoice_counter', ?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
                 updated_at = excluded.updated_at",
            params![(cur + 1).to_string(), now_millis()],
        )?;
        Ok(cur)
    }

    // --- telegram_bots (#74) ---

    /// Insert a fresh `telegram_bots` row, or — if a row with this `bot_id`
    /// already exists — update its `bot_username` / `owner_chat_id` and
    /// re-activate it. `last_update_id` is preserved on update so a re-login
    /// doesn't reset the long-poll cursor.
    pub fn upsert_telegram_bot(
        &self,
        bot_id: i64,
        bot_username: &str,
        owner_chat_id: i64,
    ) -> StoreResult<TelegramBot> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        let existing: Option<String> = guard
            .query_row(
                "SELECT id FROM telegram_bots WHERE bot_id = ?1",
                params![bot_id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                guard.execute(
                    "UPDATE telegram_bots \
                        SET bot_username = ?2, owner_chat_id = ?3, active = 1 \
                      WHERE id = ?1",
                    params![id, bot_username, owner_chat_id],
                )?;
            }
            None => {
                let id = Uuid::new_v4().to_string();
                guard.execute(
                    "INSERT INTO telegram_bots \
                        (id, bot_id, bot_username, owner_chat_id, last_update_id, \
                         active, created_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, 0, 1, ?5)",
                    params![id, bot_id, bot_username, owner_chat_id, now],
                )?;
            }
        };
        drop(guard);
        self.get_telegram_bot_by_id(bot_id)?
            .ok_or_else(|| StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    pub fn list_active_telegram_bots(&self) -> StoreResult<Vec<TelegramBot>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, bot_id, bot_username, owner_chat_id, last_update_id, \
                    active, created_at_ms \
               FROM telegram_bots \
              WHERE active = 1 \
              ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map([], row_to_telegram_bot)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn get_telegram_bot_by_id(&self, bot_id: i64) -> StoreResult<Option<TelegramBot>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT id, bot_id, bot_username, owner_chat_id, last_update_id, \
                        active, created_at_ms \
                   FROM telegram_bots \
                  WHERE bot_id = ?1",
                params![bot_id],
                row_to_telegram_bot,
            )
            .optional()?;
        Ok(row)
    }

    pub fn get_telegram_bot_by_username(
        &self,
        bot_username: &str,
    ) -> StoreResult<Option<TelegramBot>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT id, bot_id, bot_username, owner_chat_id, last_update_id, \
                        active, created_at_ms \
                   FROM telegram_bots \
                  WHERE bot_username = ?1 \
                  ORDER BY created_at_ms DESC \
                  LIMIT 1",
                params![bot_username],
                row_to_telegram_bot,
            )
            .optional()?;
        Ok(row)
    }

    /// Bump the long-poll cursor. Called once per successful `getUpdates`
    /// batch with the largest `update_id` returned in that batch.
    pub fn update_telegram_bot_last_update_id(
        &self,
        bot_id: i64,
        last_update_id: i64,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE telegram_bots \
                SET last_update_id = ?2 \
              WHERE bot_id = ?1 AND last_update_id < ?2",
            params![bot_id, last_update_id],
        )?;
        Ok(())
    }

    /// Hard delete + soft-deactivate all subscriptions tied to this bot, so
    /// the poll loop stops trying to read with credentials we just nuked.
    pub fn delete_telegram_bot(&self, bot_id: i64) -> StoreResult<()> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        let bot_id_str = bot_id.to_string();
        guard.execute(
            "UPDATE channel_subscriptions \
                SET active = 0, updated_at_ms = ?2 \
              WHERE platform = 'telegram' AND account_id = ?1",
            params![bot_id_str, now],
        )?;
        guard.execute(
            "DELETE FROM telegram_bots WHERE bot_id = ?1",
            params![bot_id],
        )?;
        Ok(())
    }

    // --- tone profiles & examples (issue #73) ---

    /// Insert one tone example. Returns the new row's `id` (uuid).
    ///
    /// All filtering — empty body, too short, no-reply recipient — is the
    /// caller's responsibility (see `tone::should_keep_for_tone` in
    /// `augmentagent-channel-email`). This helper is a dumb writer.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_tone_example(
        &self,
        source: &str,
        action_id: Option<&str>,
        message_id: Option<&str>,
        account_entity_id: &str,
        recipient_email: &str,
        recipient_domain: &str,
        subject: Option<&str>,
        body: &str,
        sent_at_ms: i64,
        weight: f64,
    ) -> StoreResult<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_millis();
        let body_chars = body.chars().count() as i64;
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO tone_examples \
                (id, source, action_id, message_id, account_entity_id, \
                 recipient_email, recipient_domain, subject, body, body_chars, \
                 sent_at_ms, ingested_at_ms, weight) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                id,
                source,
                action_id,
                message_id,
                account_entity_id,
                recipient_email,
                recipient_domain,
                subject,
                body,
                body_chars,
                sent_at_ms,
                now,
                weight,
            ],
        )?;
        Ok(id)
    }

    /// Look up a tone profile by `(scope_kind, scope_value, account_entity_id)`.
    /// `account_entity_id = None` matches the `NULL` cross-account row used
    /// for the global scope when no per-account split is needed.
    pub fn get_tone_profile(
        &self,
        scope_kind: &str,
        scope_value: &str,
        account_entity_id: Option<&str>,
    ) -> StoreResult<Option<ToneProfile>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        // `IS` is NULL-safe (`= NULL` would never match), matching the same
        // pattern `upsert_subscription` uses for the optional account_id key.
        let row = guard
            .query_row(
                "SELECT id, scope_kind, scope_value, account_entity_id, summary, \
                        exemplar_ids, sample_count, sample_count_at_refresh, \
                        last_refreshed_at, created_at_ms, updated_at_ms \
                   FROM tone_profiles \
                  WHERE scope_kind = ?1 AND scope_value = ?2 \
                    AND account_entity_id IS ?3",
                params![scope_kind, scope_value, account_entity_id],
                row_to_tone_profile,
            )
            .optional()?;
        Ok(row)
    }

    // -----------------------------------------------------------------
    // #83 — RateGovernor helpers (rate_events / rate_halts / rate_warmup).
    //
    // These are the store-side primitives the SqliteGovernor in
    // augmentagent-channel-core leans on. Kept here (and not on a separate
    // RateStore facade) so the single Mutex<Connection> still serializes
    // all rate writes against everything else hitting the same `data.db`.
    // The governor module owns the cap math; this layer just talks to SQL.
    // -----------------------------------------------------------------

    /// Insert a rate-event row. `status` is the snake-case form of the
    /// outcome (`ok` | `failed` | `rolled_back` | `suspicion`).
    /// `RolledBack` rows are still persisted (audit) but the count helpers
    /// below filter them out so they don't burn quota.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_rate_event(
        &self,
        id: &str,
        platform: &str,
        action_kind: &str,
        account_id: &str,
        occurred_at_ms: i64,
        status: &str,
        cause: &str,
        target_id: Option<&str>,
        meta_json: Option<&str>,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO rate_events \
                 (id, platform, action_kind, account_id, occurred_at_ms, \
                  status, cause, target_id, meta_json) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                platform,
                action_kind,
                account_id,
                occurred_at_ms,
                status,
                cause,
                target_id,
                meta_json,
            ],
        )?;
        Ok(())
    }

    /// Sliding-window count of "quota-burning" events for a (platform,
    /// action, account) tuple in `[since_ms, now_ms]`. Excludes
    /// `rolled_back` rows by definition (the action never executed).
    pub fn rate_event_count_in_window(
        &self,
        platform: &str,
        action_kind: &str,
        account_id: &str,
        since_ms: i64,
        now_ms: i64,
    ) -> StoreResult<u32> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n: i64 = guard.query_row(
            "SELECT COUNT(*) FROM rate_events \
              WHERE platform = ?1 \
                AND action_kind = ?2 \
                AND account_id = ?3 \
                AND occurred_at_ms >= ?4 \
                AND occurred_at_ms <= ?5 \
                AND status != 'rolled_back'",
            params![platform, action_kind, account_id, since_ms, now_ms],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u32)
    }

    /// Most recent quota-burning event timestamp for the (platform, action,
    /// account) tuple — drives min-gap enforcement. Returns `None` when no
    /// such event has ever happened.
    pub fn rate_last_event_at(
        &self,
        platform: &str,
        action_kind: &str,
        account_id: &str,
    ) -> StoreResult<Option<i64>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let v: Option<i64> = guard
            .query_row(
                "SELECT MAX(occurred_at_ms) FROM rate_events \
                  WHERE platform = ?1 \
                    AND action_kind = ?2 \
                    AND account_id = ?3 \
                    AND status != 'rolled_back'",
                params![platform, action_kind, account_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten();
        Ok(v)
    }

    /// Read the active halt row for a platform, if any. Caller compares
    /// `paused_until_ms` against the clock to decide whether the halt is
    /// still in effect.
    pub fn rate_halt_state(&self, platform: &str) -> StoreResult<Option<RateHalt>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT platform, paused_until_ms, reason, triggered_by_event_id, \
                        created_at_ms, acknowledged_at_ms \
                   FROM rate_halts WHERE platform = ?1",
                params![platform],
                |r| {
                    Ok(RateHalt {
                        platform: r.get(0)?,
                        paused_until_ms: r.get(1)?,
                        reason: r.get(2)?,
                        triggered_by_event_id: r.get(3)?,
                        created_at_ms: r.get(4)?,
                        acknowledged_at_ms: r.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Upsert a tone profile keyed on `(scope_kind, scope_value, account_entity_id)`.
    ///
    /// On insert: stores all fields and stamps timestamps.
    /// On update: refreshes summary/exemplar_ids/sample_count plus the
    /// `sample_count_at_refresh` snapshot so the staleness predicate
    /// (`sample_count - sample_count_at_refresh >= threshold`) resets.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_tone_profile(
        &self,
        scope_kind: &str,
        scope_value: &str,
        account_entity_id: Option<&str>,
        summary: &str,
        exemplar_ids: &str,
        sample_count: i64,
    ) -> StoreResult<ToneProfile> {
        let now = now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        let existing: Option<String> = guard
            .query_row(
                "SELECT id FROM tone_profiles \
                 WHERE scope_kind = ?1 AND scope_value = ?2 \
                   AND account_entity_id IS ?3",
                params![scope_kind, scope_value, account_entity_id],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                guard.execute(
                    "UPDATE tone_profiles \
                        SET summary = ?2, exemplar_ids = ?3, sample_count = ?4, \
                            sample_count_at_refresh = ?4, last_refreshed_at = ?5, \
                            updated_at_ms = ?5 \
                      WHERE id = ?1",
                    params![id, summary, exemplar_ids, sample_count, now],
                )?;
            }
            None => {
                let id = Uuid::new_v4().to_string();
                guard.execute(
                    "INSERT INTO tone_profiles \
                        (id, scope_kind, scope_value, account_entity_id, summary, \
                         exemplar_ids, sample_count, sample_count_at_refresh, \
                         last_refreshed_at, created_at_ms, updated_at_ms) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8, ?8, ?8)",
                    params![
                        id,
                        scope_kind,
                        scope_value,
                        account_entity_id,
                        summary,
                        exemplar_ids,
                        sample_count,
                        now,
                    ],
                )?;
            }
        }
        drop(guard);
        self.get_tone_profile(scope_kind, scope_value, account_entity_id)?
            .ok_or_else(|| StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows))
    }

    /// Capture the post-edit body of a `Sent` action as a tone example.
    ///
    /// This is the gold-standard signal: the user's voice corrected on top of
    /// the model's draft. Called from the email channel adapter right after
    /// `send_draft` succeeds. Source is `user_edit`, weight=1.5 to bias the
    /// summarizer toward these over backfilled history. No-op (Ok) if the
    /// action doesn't exist or is missing `draftBody`.
    pub fn record_user_edit_as_tone_example(&self, action_id: &str) -> StoreResult<Option<String>> {
        let row: Option<(
            Option<String>,
            String,
            Option<String>,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<i64>,
        )>;
        {
            let guard = self.conn.lock().expect("store mutex poisoned");
            row = guard
                .query_row(
                    "SELECT a.draftBody, a.fromEmail, a.threadId, a.messageId, a.subject, \
                            e.body, e.accountEntityId, e.firstSeenAt \
                       FROM actions a \
                       LEFT JOIN emails e ON a.messageId = e.messageId \
                      WHERE a.id = ?1 AND a.status = 'sent'",
                    params![action_id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<String>>(6)?,
                            r.get::<_, Option<i64>>(7)?,
                        ))
                    },
                )
                .optional()?;
        }
        let Some((
            draft_body,
            from_email,
            _thread_id,
            message_id,
            subject,
            _orig_body,
            account_entity_id,
            received_at_ms,
        )) = row
        else {
            return Ok(None);
        };
        let Some(body) = draft_body.filter(|b| !b.trim().is_empty()) else {
            return Ok(None);
        };
        let Some(account) = account_entity_id else {
            // Fallback for actions without account context — skip silently;
            // the per-account scoping invariant on tone_examples is intentional.
            return Ok(None);
        };
        // Recipient is the row we replied TO. `actions.fromEmail` is the
        // sender of the inbound mail; that IS the address we just replied to.
        let recipient_email = bare_lower(&from_email);
        let recipient_domain = recipient_email
            .split_once('@')
            .map(|(_, d)| d.to_string())
            .unwrap_or_default();
        if recipient_email.is_empty() || recipient_domain.is_empty() {
            return Ok(None);
        }
        let sent_at_ms = received_at_ms.unwrap_or_else(now_millis);
        let id = self.insert_tone_example(
            "user_edit",
            Some(action_id),
            Some(&message_id),
            &account,
            &recipient_email,
            &recipient_domain,
            Some(&subject),
            &body,
            sent_at_ms,
            1.5,
        )?;
        Ok(Some(id))
    }

    /// Pull the most-recent N example bodies for a scope, oldest→newest.
    /// Used by the summarizer to assemble the corpus prompt.
    pub fn recent_tone_examples(
        &self,
        scope_kind: &str,
        scope_value: &str,
        account_entity_id: Option<&str>,
        limit: i64,
    ) -> StoreResult<Vec<ToneExample>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        // Branch on scope_kind to use the right index. All three queries
        // return the same column order so they share `row_to_tone_example`.
        let sql = match scope_kind {
            "recipient" => {
                "SELECT id, source, action_id, message_id, account_entity_id, \
                        recipient_email, recipient_domain, subject, body, body_chars, \
                        sent_at_ms, ingested_at_ms, weight \
                   FROM tone_examples \
                  WHERE recipient_email = ?1 AND account_entity_id IS ?2 \
                  ORDER BY sent_at_ms DESC LIMIT ?3"
            }
            "domain" => {
                "SELECT id, source, action_id, message_id, account_entity_id, \
                        recipient_email, recipient_domain, subject, body, body_chars, \
                        sent_at_ms, ingested_at_ms, weight \
                   FROM tone_examples \
                  WHERE recipient_domain = ?1 AND account_entity_id IS ?2 \
                  ORDER BY sent_at_ms DESC LIMIT ?3"
            }
            // global / anything else: filter only by account.
            _ => {
                "SELECT id, source, action_id, message_id, account_entity_id, \
                        recipient_email, recipient_domain, subject, body, body_chars, \
                        sent_at_ms, ingested_at_ms, weight \
                   FROM tone_examples \
                  WHERE account_entity_id IS ?2 \
                  ORDER BY sent_at_ms DESC LIMIT ?3"
            }
        };
        let mut stmt = guard.prepare(sql)?;
        let rows = stmt.query_map(
            params![scope_value, account_entity_id, limit],
            row_to_tone_example,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List every tone profile, ordered by `last_refreshed_at` ascending so
    /// the staleness scan in `tone refresh-stale` walks the oldest first.
    pub fn list_tone_profiles(&self) -> StoreResult<Vec<ToneProfile>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT id, scope_kind, scope_value, account_entity_id, summary, \
                    exemplar_ids, sample_count, sample_count_at_refresh, \
                    last_refreshed_at, created_at_ms, updated_at_ms \
               FROM tone_profiles \
              ORDER BY last_refreshed_at ASC",
        )?;
        let rows = stmt.query_map([], row_to_tone_profile)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Count of tone_examples rows currently keyed against a scope. Used by
    /// the staleness predicate to refresh `sample_count` to ground truth
    /// before comparing against the snapshot.
    pub fn count_tone_examples(
        &self,
        scope_kind: &str,
        scope_value: &str,
        account_entity_id: Option<&str>,
    ) -> StoreResult<i64> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n: i64 = match scope_kind {
            "recipient" => guard.query_row(
                "SELECT COUNT(*) FROM tone_examples \
                  WHERE recipient_email = ?1 AND account_entity_id IS ?2",
                params![scope_value, account_entity_id],
                |r| r.get(0),
            )?,
            "domain" => guard.query_row(
                "SELECT COUNT(*) FROM tone_examples \
                  WHERE recipient_domain = ?1 AND account_entity_id IS ?2",
                params![scope_value, account_entity_id],
                |r| r.get(0),
            )?,
            _ => guard.query_row(
                "SELECT COUNT(*) FROM tone_examples WHERE account_entity_id IS ?1",
                params![account_entity_id],
                |r| r.get(0),
            )?,
        };
        Ok(n)
    }

    /// Upsert a halt for `platform`. Replaces any existing row — `permit()`
    /// only ever cares about the most recent halt window per platform.
    pub fn rate_set_halt(
        &self,
        platform: &str,
        paused_until_ms: i64,
        reason: &str,
        triggered_by_event_id: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO rate_halts \
                 (platform, paused_until_ms, reason, triggered_by_event_id, \
                  created_at_ms, acknowledged_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL) \
             ON CONFLICT(platform) DO UPDATE SET \
                 paused_until_ms = excluded.paused_until_ms, \
                 reason = excluded.reason, \
                 triggered_by_event_id = excluded.triggered_by_event_id, \
                 created_at_ms = excluded.created_at_ms, \
                 acknowledged_at_ms = NULL",
            params![
                platform,
                paused_until_ms,
                reason,
                triggered_by_event_id,
                now_ms
            ],
        )?;
        Ok(())
    }

    /// Mark the active halt as acknowledged by the user (Discord button /
    /// dashboard). Doesn't lift the halt — the halt stays until
    /// `paused_until_ms` passes — but suppresses re-pinging the user.
    pub fn rate_acknowledge_halt(&self, platform: &str, now_ms: i64) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "UPDATE rate_halts SET acknowledged_at_ms = ?2 WHERE platform = ?1",
            params![platform, now_ms],
        )?;
        Ok(())
    }

    /// Read the warmup-start timestamp for a (platform, account) pair, if
    /// it has been seeded.
    pub fn rate_get_warmup(
        &self,
        platform: &str,
        account_id: &str,
    ) -> StoreResult<Option<RateWarmup>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row = guard
            .query_row(
                "SELECT platform, account_id, warmup_started_at_ms \
                   FROM rate_warmup WHERE platform = ?1 AND account_id = ?2",
                params![platform, account_id],
                |r| {
                    Ok(RateWarmup {
                        platform: r.get(0)?,
                        account_id: r.get(1)?,
                        warmup_started_at_ms: r.get(2)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Idempotently seed (platform, account) → `warmup_started_at_ms = now`.
    /// Existing rows are left alone so warmup math doesn't get reset by a
    /// repeated `permit()` call on a known account.
    pub fn rate_seed_warmup(
        &self,
        platform: &str,
        account_id: &str,
        warmup_started_at_ms: i64,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO rate_warmup (platform, account_id, warmup_started_at_ms) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(platform, account_id) DO NOTHING",
            params![platform, account_id, warmup_started_at_ms],
        )?;
        Ok(())
    }

    /// Override a warmup start time. Used by the dashboard's
    /// "skip warmup, this account is well-aged" button (sets the timestamp
    /// 28 days into the past so the multiplier reads 1.0).
    pub fn rate_override_warmup(
        &self,
        platform: &str,
        account_id: &str,
        warmup_started_at_ms: i64,
    ) -> StoreResult<()> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        guard.execute(
            "INSERT INTO rate_warmup (platform, account_id, warmup_started_at_ms) \
             VALUES (?1, ?2, ?3) \
             ON CONFLICT(platform, account_id) DO UPDATE SET \
                 warmup_started_at_ms = excluded.warmup_started_at_ms",
            params![platform, account_id, warmup_started_at_ms],
        )?;
        Ok(())
    }

    /// `[since_ms, until_ms]` audit dump for one account. Optional
    /// `platform` filter narrows to a single platform; `None` returns all
    /// platforms for the account (useful for an "everything I did" query).
    /// Ordered newest-first so the dashboard table renders without a sort.
    pub fn rate_audit_query(
        &self,
        account_id: &str,
        platform: Option<&str>,
        since_ms: i64,
        until_ms: i64,
    ) -> StoreResult<Vec<RateAuditRow>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let rows = match platform {
            Some(p) => {
                let mut stmt = guard.prepare(
                    "SELECT id, platform, action_kind, account_id, occurred_at_ms, \
                            status, cause, target_id, meta_json \
                       FROM rate_events \
                      WHERE account_id = ?1 \
                        AND platform = ?2 \
                        AND occurred_at_ms >= ?3 \
                        AND occurred_at_ms <= ?4 \
                      ORDER BY occurred_at_ms DESC",
                )?;
                let it = stmt.query_map(params![account_id, p, since_ms, until_ms], |r| {
                    Ok(RateAuditRow {
                        id: r.get(0)?,
                        platform: r.get(1)?,
                        action_kind: r.get(2)?,
                        account_id: r.get(3)?,
                        occurred_at_ms: r.get(4)?,
                        status: r.get(5)?,
                        cause: r.get(6)?,
                        target_id: r.get(7)?,
                        meta_json: r.get(8)?,
                    })
                })?;
                it.collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = guard.prepare(
                    "SELECT id, platform, action_kind, account_id, occurred_at_ms, \
                            status, cause, target_id, meta_json \
                       FROM rate_events \
                      WHERE account_id = ?1 \
                        AND occurred_at_ms >= ?2 \
                        AND occurred_at_ms <= ?3 \
                      ORDER BY occurred_at_ms DESC",
                )?;
                let it = stmt.query_map(params![account_id, since_ms, until_ms], |r| {
                    Ok(RateAuditRow {
                        id: r.get(0)?,
                        platform: r.get(1)?,
                        action_kind: r.get(2)?,
                        account_id: r.get(3)?,
                        occurred_at_ms: r.get(4)?,
                        status: r.get(5)?,
                        cause: r.get(6)?,
                        target_id: r.get(7)?,
                        meta_json: r.get(8)?,
                    })
                })?;
                it.collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    /// Housekeeping helper — prune rate_events older than `older_than_ms`
    /// (90d retention per #83). Returns rows deleted for logging.
    pub fn rate_prune_events(&self, older_than_ms: i64) -> StoreResult<usize> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n = guard.execute(
            "DELETE FROM rate_events WHERE occurred_at_ms < ?1",
            params![older_than_ms],
        )?;
        Ok(n)
    }
}

fn row_to_tone_profile(r: &rusqlite::Row) -> rusqlite::Result<ToneProfile> {
    Ok(ToneProfile {
        id: r.get(0)?,
        scope_kind: r.get(1)?,
        scope_value: r.get(2)?,
        account_entity_id: r.get::<_, Option<String>>(3)?,
        summary: r.get(4)?,
        exemplar_ids: r.get(5)?,
        sample_count: r.get(6)?,
        sample_count_at_refresh: r.get(7)?,
        last_refreshed_at: r.get(8)?,
        created_at_ms: r.get(9)?,
        updated_at_ms: r.get(10)?,
    })
}

fn row_to_tone_example(r: &rusqlite::Row) -> rusqlite::Result<ToneExample> {
    Ok(ToneExample {
        id: r.get(0)?,
        source: r.get(1)?,
        action_id: r.get::<_, Option<String>>(2)?,
        message_id: r.get::<_, Option<String>>(3)?,
        account_entity_id: r.get(4)?,
        recipient_email: r.get(5)?,
        recipient_domain: r.get(6)?,
        subject: r.get::<_, Option<String>>(7)?,
        body: r.get(8)?,
        body_chars: r.get(9)?,
        sent_at_ms: r.get(10)?,
        ingested_at_ms: r.get(11)?,
        weight: r.get(12)?,
    })
}

fn bare_lower(raw: &str) -> String {
    let s = if let (Some(open), Some(close)) = (raw.find('<'), raw.rfind('>')) {
        if open < close {
            &raw[open + 1..close]
        } else {
            raw
        }
    } else {
        raw
    };
    s.trim().to_ascii_lowercase()
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

fn row_to_telegram_bot(r: &rusqlite::Row) -> rusqlite::Result<TelegramBot> {
    Ok(TelegramBot {
        id: r.get(0)?,
        bot_id: r.get(1)?,
        bot_username: r.get(2)?,
        owner_chat_id: r.get(3)?,
        last_update_id: r.get(4)?,
        active: r.get::<_, i64>(5)? != 0,
        created_at_ms: r.get(6)?,
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

/// One row from `draft_revisions` (#37). The full chain for a given action is
/// `iteration = 0, 1, 2, ...` with `outcome ∈ { superseded, pending, approved,
/// skipped }`. `feedbackText` is the user's Revise feedback for this draft;
/// it's NULL on the iteration-0 (auto-generated) draft and Some on every
/// revised draft thereafter.
#[derive(Debug, Clone)]
pub struct RevisionRecord {
    pub id: String,
    pub action_id: String,
    pub iteration: i64,
    pub draft_body: String,
    pub feedback_text: Option<String>,
    pub preset_id: Option<String>,
    pub outcome: String,
    pub model_id: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub created_at_ms: i64,
}

fn row_to_revision_record(r: &rusqlite::Row) -> rusqlite::Result<RevisionRecord> {
    Ok(RevisionRecord {
        id: r.get(0)?,
        action_id: r.get(1)?,
        iteration: r.get(2)?,
        draft_body: r.get(3)?,
        feedback_text: r.get::<_, Option<String>>(4)?,
        preset_id: r.get::<_, Option<String>>(5)?,
        outcome: r.get(6)?,
        model_id: r.get(7)?,
        prompt_tokens: r.get::<_, Option<i64>>(8)?,
        completion_tokens: r.get::<_, Option<i64>>(9)?,
        created_at_ms: r.get(10)?,
    })
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
    fn find_next_to_promote_returns_oldest_unpromoted_immediately() {
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
        // Initial promotion is no longer gated by the 6h timer — fresh
        // backlog rows are eligible immediately. Oldest createdAt wins.
        let nxt = s
            .find_next_to_promote(now_millis())
            .unwrap()
            .expect("expected next");
        assert_eq!(nxt.action.action.id, id1, "oldest createdAt wins");
        assert_eq!(nxt.nudge_count, 0);
    }

    #[test]
    fn find_next_to_promote_skips_promoted_rows() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        s.upsert_email(&sample_email("m2")).unwrap();
        let id1 = s
            .log_action("m1", None, "a@b.com", "s1", None, Some("d1"), ActionStatus::Pending)
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = s
            .log_action("m2", None, "a@b.com", "s2", None, Some("d2"), ActionStatus::Pending)
            .unwrap();
        // m1 is the active card (nudgeCount=1); next promotion should pick m2.
        s.record_nudge(&id1, now_millis() + NUDGE_INTERVAL_MS).unwrap();
        let nxt = s
            .find_next_to_promote(now_millis())
            .unwrap()
            .expect("expected next");
        assert_eq!(nxt.action.action.id, id2);
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

    // --- draft_revisions (#37) ---

    #[test]
    fn record_revision_triple_writes_two_rows_first_time() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let action_id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("v0"), ActionStatus::Pending)
            .unwrap();
        let revised_id = s
            .record_revision_triple(&action_id, "v0", "less formal please", "v1")
            .unwrap();
        let rows = s.list_revisions_for_action(&action_id).unwrap();
        assert_eq!(rows.len(), 2, "first revise writes original + revised rows");
        assert_eq!(rows[0].iteration, 0);
        assert_eq!(rows[0].draft_body, "v0");
        assert_eq!(rows[0].feedback_text, None);
        assert_eq!(rows[0].outcome, "superseded");
        assert_eq!(rows[1].iteration, 1);
        assert_eq!(rows[1].draft_body, "v1");
        assert_eq!(rows[1].feedback_text.as_deref(), Some("less formal please"));
        assert_eq!(rows[1].outcome, "pending");
        assert_eq!(rows[1].id, revised_id);
    }

    #[test]
    fn record_revision_triple_chains_subsequent_revises() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let action_id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("v0"), ActionStatus::Pending)
            .unwrap();
        s.record_revision_triple(&action_id, "v0", "less formal", "v1")
            .unwrap();
        s.record_revision_triple(&action_id, "v1", "shorter", "v2")
            .unwrap();
        let rows = s.list_revisions_for_action(&action_id).unwrap();
        assert_eq!(rows.len(), 3, "second revise appends one row, supersedes prior pending");
        assert_eq!(rows[0].outcome, "superseded");
        assert_eq!(rows[1].outcome, "superseded", "prior pending flips to superseded");
        assert_eq!(rows[1].draft_body, "v1");
        assert_eq!(rows[2].outcome, "pending");
        assert_eq!(rows[2].draft_body, "v2");
        assert_eq!(rows[2].feedback_text.as_deref(), Some("shorter"));
        // Iterations stay contiguous + UNIQUE.
        assert_eq!(rows[0].iteration, 0);
        assert_eq!(rows[1].iteration, 1);
        assert_eq!(rows[2].iteration, 2);
    }

    #[test]
    fn list_recent_feedback_filters_by_age_and_skips_iteration_zero() {
        let (s, _f) = fresh_store();
        s.upsert_email(&sample_email("m1")).unwrap();
        let action_id = s
            .log_action("m1", None, "a@b.com", "s", None, Some("v0"), ActionStatus::Pending)
            .unwrap();
        s.record_revision_triple(&action_id, "v0", "less formal", "v1")
            .unwrap();
        let recent = s.list_recent_feedback(60_000).unwrap();
        assert_eq!(recent.len(), 1, "only the iteration-1 row has feedback text");
        assert_eq!(recent[0].feedback_text.as_deref(), Some("less formal"));
        // A zero-window query excludes everything.
        let none = s.list_recent_feedback(0).unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn telegram_bot_upsert_preserves_last_update_id_on_relogin() {
        let (s, _f) = fresh_store();
        let bot = s
            .upsert_telegram_bot(99, "triage_bot", 5555)
            .unwrap();
        assert_eq!(bot.last_update_id, 0);
        s.update_telegram_bot_last_update_id(99, 4242).unwrap();
        // Re-login should not reset the cursor.
        let bot2 = s
            .upsert_telegram_bot(99, "triage_bot_renamed", 5555)
            .unwrap();
        assert_eq!(bot2.last_update_id, 4242);
        assert_eq!(bot2.bot_username, "triage_bot_renamed");
    }

    #[test]
    fn telegram_bot_update_last_update_id_is_monotonic() {
        let (s, _f) = fresh_store();
        s.upsert_telegram_bot(7, "b", 1).unwrap();
        s.update_telegram_bot_last_update_id(7, 100).unwrap();
        // A stale write (lower id) must not move the cursor backward.
        s.update_telegram_bot_last_update_id(7, 50).unwrap();
        let bot = s.get_telegram_bot_by_id(7).unwrap().unwrap();
        assert_eq!(bot.last_update_id, 100);
    }

    #[test]
    fn telegram_bot_delete_deactivates_subscriptions() {
        let (s, _f) = fresh_store();
        s.upsert_telegram_bot(7, "b", 1).unwrap();
        s.upsert_subscription("telegram", "12345", "alice", SubscriptionMode::Priority, Some("7"))
            .unwrap();
        s.delete_telegram_bot(7).unwrap();
        let subs = s.list_active_subscriptions("telegram").unwrap();
        assert!(subs.is_empty());
        assert!(s.get_telegram_bot_by_id(7).unwrap().is_none());
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

    // --- tone profiles & examples (issue #73) ---

    #[test]
    fn insert_and_query_tone_example() {
        let (s, _f) = fresh_store();
        let id = s
            .insert_tone_example(
                "sent_backfill",
                None,
                Some("gmail-msg-1"),
                "acc1",
                "jeremy@acme.com",
                "acme.com",
                Some("Re: stuff"),
                "Hey — quick reply.",
                1_700_000_000_000,
                1.0,
            )
            .unwrap();
        assert!(!id.is_empty(), "insert returns a non-empty uuid");
        let recents = s
            .recent_tone_examples("recipient", "jeremy@acme.com", Some("acc1"), 10)
            .unwrap();
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].source, "sent_backfill");
        assert_eq!(recents[0].body, "Hey — quick reply.");
        // body_chars is character count, not byte count.
        assert_eq!(recents[0].body_chars, "Hey — quick reply.".chars().count() as i64);
    }

    #[test]
    fn get_tone_profile_returns_none_for_missing() {
        let (s, _f) = fresh_store();
        let p = s.get_tone_profile("global", "*", Some("acc1")).unwrap();
        assert!(p.is_none());
    }

    #[test]
    fn upsert_tone_profile_inserts_then_updates_in_place() {
        let (s, _f) = fresh_store();
        let p1 = s
            .upsert_tone_profile(
                "global",
                "*",
                Some("acc1"),
                "{\"register\":\"casual\"}",
                "[]",
                10,
            )
            .unwrap();
        assert_eq!(p1.scope_kind, "global");
        assert_eq!(p1.sample_count, 10);
        assert_eq!(p1.sample_count_at_refresh, 10);

        // Re-upsert with new summary and higher sample_count: SAME id, fields refresh.
        let p2 = s
            .upsert_tone_profile(
                "global",
                "*",
                Some("acc1"),
                "{\"register\":\"professional\"}",
                "[\"x\"]",
                25,
            )
            .unwrap();
        assert_eq!(p1.id, p2.id, "upsert is in-place keyed by (scope,scope_value,acct)");
        assert_eq!(p2.summary, "{\"register\":\"professional\"}");
        assert_eq!(p2.sample_count, 25);
        // Snapshot is reset to current sample_count on refresh — caller can
        // then compare future inserts against this for staleness.
        assert_eq!(p2.sample_count_at_refresh, 25);
    }

    #[test]
    fn upsert_tone_profile_keys_account_distinctly() {
        let (s, _f) = fresh_store();
        let p_a = s
            .upsert_tone_profile("global", "*", Some("acc-A"), "A", "[]", 1)
            .unwrap();
        let p_b = s
            .upsert_tone_profile("global", "*", Some("acc-B"), "B", "[]", 1)
            .unwrap();
        assert_ne!(p_a.id, p_b.id, "different accounts → different rows");
        let all = s.list_tone_profiles().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn recent_tone_examples_orders_newest_first_and_limits() {
        let (s, _f) = fresh_store();
        for (i, ts) in [1_000_i64, 3_000, 2_000].iter().enumerate() {
            s.insert_tone_example(
                "sent_backfill",
                None,
                Some(&format!("m{i}")),
                "acc1",
                "x@y.com",
                "y.com",
                None,
                "body",
                *ts,
                1.0,
            )
            .unwrap();
        }
        let recents = s
            .recent_tone_examples("recipient", "x@y.com", Some("acc1"), 2)
            .unwrap();
        assert_eq!(recents.len(), 2);
        assert_eq!(recents[0].sent_at_ms, 3000);
        assert_eq!(recents[1].sent_at_ms, 2000);
    }

    #[test]
    fn count_tone_examples_per_scope() {
        let (s, _f) = fresh_store();
        for to in ["a@acme.com", "b@acme.com", "c@other.com"] {
            let domain = to.split('@').nth(1).unwrap();
            s.insert_tone_example(
                "sent_backfill",
                None,
                None,
                "acc1",
                to,
                domain,
                None,
                "body body body",
                1_000,
                1.0,
            )
            .unwrap();
        }
        assert_eq!(
            s.count_tone_examples("recipient", "a@acme.com", Some("acc1")).unwrap(),
            1
        );
        assert_eq!(
            s.count_tone_examples("domain", "acme.com", Some("acc1")).unwrap(),
            2
        );
        assert_eq!(
            s.count_tone_examples("global", "*", Some("acc1")).unwrap(),
            3
        );
    }

    #[test]
    fn record_user_edit_as_tone_example_captures_sent_draft() {
        let (s, _f) = fresh_store();
        // Seed an email + an action that's been transitioned to Sent.
        let mut email = sample_email("m-edit");
        email.from = "Alex <alex@startup.io>".into();
        email.subject = "Re: launch".into();
        s.upsert_email(&email).unwrap();
        let action_id = s
            .log_action(
                "m-edit",
                None,
                "Alex <alex@startup.io>",
                "Re: launch",
                Some("inbound body"),
                Some("This is the post-edit draft the user actually sent."),
                ActionStatus::Pending,
            )
            .unwrap();
        s.update_action_status(&action_id, ActionStatus::Sent, None, None)
            .unwrap();

        let new_id = s.record_user_edit_as_tone_example(&action_id).unwrap();
        assert!(new_id.is_some(), "expected a tone example to be recorded");
        let recents = s
            .recent_tone_examples("recipient", "alex@startup.io", Some("acc"), 10)
            .unwrap();
        assert_eq!(recents.len(), 1);
        assert_eq!(recents[0].source, "user_edit");
        assert!((recents[0].weight - 1.5).abs() < f64::EPSILON);
        assert_eq!(recents[0].recipient_domain, "startup.io");
        assert_eq!(
            recents[0].body,
            "This is the post-edit draft the user actually sent."
        );
    }

    #[test]
    fn record_user_edit_skips_non_sent_or_empty_draft() {
        let (s, _f) = fresh_store();
        let email = sample_email("m-pending");
        s.upsert_email(&email).unwrap();
        let id = s
            .log_action(
                "m-pending",
                None,
                "a@b.com",
                "subj",
                None,
                Some("draft"),
                ActionStatus::Pending,
            )
            .unwrap();
        // Status is Pending, not Sent → no-op.
        assert!(s.record_user_edit_as_tone_example(&id).unwrap().is_none());
        // Now flip to Sent but with no draftBody on a separate action.
        let id2 = s
            .log_action(
                "m-pending",
                None,
                "a@b.com",
                "subj",
                None,
                None,
                ActionStatus::Pending,
            )
            .unwrap();
        s.update_action_status(&id2, ActionStatus::Sent, None, None)
            .unwrap();
        assert!(
            s.record_user_edit_as_tone_example(&id2).unwrap().is_none(),
            "missing draftBody → skip"
        );
    }

    #[test]
    fn linkedin_connection_sync_roundtrips() {
        let (s, _f) = fresh_store();
        assert!(s
            .get_linkedin_connection_sync("urn:li:fsd_profile:ME")
            .unwrap()
            .is_none());
        let cur = LinkedInConnectionSync {
            account_id: "urn:li:fsd_profile:ME".into(),
            last_full_sync_ms: Some(1_700_000_000_000),
            last_delta_sync_ms: None,
            cursor_start: 80,
            last_synced_count: 562,
        };
        s.upsert_linkedin_connection_sync(&cur).unwrap();
        let got = s
            .get_linkedin_connection_sync("urn:li:fsd_profile:ME")
            .unwrap()
            .unwrap();
        assert_eq!(got.cursor_start, 80);
        assert_eq!(got.last_full_sync_ms, Some(1_700_000_000_000));
        assert_eq!(got.last_synced_count, 562);
        // Upsert overwrites mutable columns.
        let cur2 = LinkedInConnectionSync {
            cursor_start: 0,
            last_delta_sync_ms: Some(1_700_100_000_000),
            ..cur
        };
        s.upsert_linkedin_connection_sync(&cur2).unwrap();
        let got2 = s
            .get_linkedin_connection_sync("urn:li:fsd_profile:ME")
            .unwrap()
            .unwrap();
        assert_eq!(got2.cursor_start, 0);
        assert_eq!(got2.last_delta_sync_ms, Some(1_700_100_000_000));
    }

    #[test]
    fn contacts_sync_token_roundtrips() {
        let (s, _f) = fresh_store();
        assert!(s
            .get_contacts_sync_token("google_people", "acc1")
            .unwrap()
            .is_none());
        s.set_contacts_sync_token("google_people", "acc1", "tok-abc")
            .unwrap();
        assert_eq!(
            s.get_contacts_sync_token("google_people", "acc1").unwrap(),
            Some("tok-abc".to_string())
        );
        // Distinct (backend, account) key is independent.
        assert!(s
            .get_contacts_sync_token("carddav", "acc1")
            .unwrap()
            .is_none());
        s.set_contacts_sync_token("google_people", "acc1", "tok-def")
            .unwrap();
        assert_eq!(
            s.get_contacts_sync_token("google_people", "acc1").unwrap(),
            Some("tok-def".to_string())
        );
    }

    #[test]
    fn phone_identity_reverse_lookup() {
        let (s, _f) = fresh_store();
        assert!(s.lookup_person_by_phone("+14155550100").unwrap().is_none());
        s.upsert_phone_identity(&PhoneIdentity {
            phone: "+14155550100".into(),
            person_slug: "jane_doe".into(),
            display_name: Some("Jane Doe".into()),
            source: "google_people".into(),
        })
        .unwrap();
        let p = s.lookup_person_by_phone("+14155550100").unwrap().unwrap();
        assert_eq!(p.person_slug, "jane_doe");
        assert_eq!(p.display_name.as_deref(), Some("Jane Doe"));
        // Re-ingest same phone → upsert, not duplicate; slug can move.
        s.upsert_phone_identity(&PhoneIdentity {
            phone: "+14155550100".into(),
            person_slug: "jane_d".into(),
            display_name: Some("Jane D.".into()),
            source: "carddav".into(),
        })
        .unwrap();
        let p2 = s.lookup_person_by_phone("+14155550100").unwrap().unwrap();
        assert_eq!(p2.person_slug, "jane_d");
        assert_eq!(p2.source, "carddav");
    }
}
