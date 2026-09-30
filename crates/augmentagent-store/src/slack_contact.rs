//! Slack contact sends (#1290).
//!
//! Two small tables:
//!
//! * `slack_send_targets` — where a reply to an ingested Slack message (or a
//!   new message the owner composed) goes: workspace, conversation and, when
//!   the reply belongs in a thread, the parent `thread_ts`. Written at
//!   ingest / compose time, read at approve time, so the destination the
//!   owner saw on the card is the one that is used.
//! * `slack_contact_sends` — one row per approved action: the destination
//!   and sender identity actually used, the text sent, every attempt and its
//!   outcome. It is what makes a retry after a failure send at most once: a
//!   `sent` row is never attempted again, and an attempt whose outcome is
//!   unknown must be checked against the conversation before a resend.
//!
//! Rows move `sending → sent | failed | unknown | unverified`, and back to
//! `sending` only through [`Store::start_slack_contact_send`].

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::store::{now_millis, Store, StoreError, StoreResult};

/// What kind of conversation a target is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackConversationKind {
    /// A 1:1 DM (`D…`).
    Dm,
    /// A multi-person DM.
    GroupDm,
    /// A public or private channel.
    Channel,
    /// A user id (`U…`): Slack opens (or reuses) the owner's DM with them.
    User,
}

impl SlackConversationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dm => "dm",
            Self::GroupDm => "group_dm",
            Self::Channel => "channel",
            Self::User => "user",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "dm" => Self::Dm,
            "group_dm" => Self::GroupDm,
            "channel" => Self::Channel,
            "user" => Self::User,
            _ => return None,
        })
    }
}

/// Where a Slack contact message goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackSendTarget {
    /// The `emails.messageId` of the row the action belongs to.
    pub message_id: String,
    pub team_id: String,
    /// Conversation id (`C…`/`G…`/`D…`), or a user id for a new DM.
    pub channel_id: String,
    /// Parent message `ts` when the reply belongs in a thread.
    pub thread_ts: Option<String>,
    pub kind: SlackConversationKind,
    /// Human label shown on the card (`#general`, `Alice Example`).
    pub label: Option<String>,
}

/// Which Slack identity a contact message is sent as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackSendIdentity {
    /// The owner's own Slack account, through the Composio user connection.
    OwnerUser,
}

impl SlackSendIdentity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnerUser => "owner_user",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "owner_user" => Some(Self::OwnerUser),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackSendStatus {
    /// An attempt is in flight (or the daemon died during one).
    Sending,
    Sent,
    /// Slack or Composio refused it: nothing was posted.
    Failed,
    /// The request may have reached Slack (timeout, lost response).
    Unknown,
    /// A retry could not check whether an unknown attempt landed; the next
    /// approval sends anyway.
    Unverified,
}

impl SlackSendStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Unverified => "unverified",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sending" => Self::Sending,
            "sent" => Self::Sent,
            "failed" => Self::Failed,
            "unknown" => Self::Unknown,
            "unverified" => Self::Unverified,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSlackContactSend<'a> {
    pub action_id: &'a str,
    pub team_id: &'a str,
    pub channel_id: &'a str,
    pub thread_ts: Option<&'a str>,
    pub identity: SlackSendIdentity,
    pub sender_user_id: &'a str,
    /// The text as sent (already converted to mrkdwn).
    pub body: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackContactSend {
    pub action_id: String,
    pub team_id: String,
    pub channel_id: String,
    pub thread_ts: Option<String>,
    pub identity: SlackSendIdentity,
    pub sender_user_id: String,
    pub body: String,
    pub status: SlackSendStatus,
    pub attempts: i64,
    /// The conversation Slack reported (a `D…` id for a send to a user id).
    pub remote_channel: Option<String>,
    pub remote_ts: Option<String>,
    /// The `user` Slack attributed the posted message to, when it said.
    pub observed_user: Option<String>,
    /// A `bot_id` on the posted message: Slack attributed it to an app.
    pub observed_bot_id: Option<String>,
    pub error: Option<String>,
    pub attempt_started_ms: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// What [`Store::start_slack_contact_send`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackSendStart {
    /// Go ahead: `row` is now `sending`. `previous` is the row as it was
    /// before this attempt (`None` for a first attempt); a previous
    /// `unknown`/`sending` outcome must be checked before posting.
    Attempt {
        previous: Option<Box<SlackContactSend>>,
        row: SlackContactSend,
    },
    /// Already delivered: do not post.
    AlreadySent(SlackContactSend),
}

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS slack_send_targets (\
             message_id    TEXT PRIMARY KEY,\
             team_id       TEXT NOT NULL,\
             channel_id    TEXT NOT NULL,\
             thread_ts     TEXT,\
             kind          TEXT NOT NULL \
                           CHECK(kind IN ('dm', 'group_dm', 'channel', 'user')),\
             label         TEXT,\
             created_at_ms INTEGER NOT NULL\
         );\
         CREATE TABLE IF NOT EXISTS slack_contact_sends (\
             action_id          TEXT PRIMARY KEY,\
             team_id            TEXT NOT NULL,\
             channel_id         TEXT NOT NULL,\
             thread_ts          TEXT,\
             identity           TEXT NOT NULL CHECK(identity IN ('owner_user')),\
             sender_user_id     TEXT NOT NULL,\
             body               TEXT NOT NULL,\
             status             TEXT NOT NULL CHECK(status IN \
                                ('sending', 'sent', 'failed', 'unknown', 'unverified')),\
             attempts           INTEGER NOT NULL DEFAULT 1,\
             remote_channel     TEXT,\
             remote_ts          TEXT,\
             observed_user      TEXT,\
             observed_bot_id    TEXT,\
             error              TEXT,\
             attempt_started_ms INTEGER NOT NULL,\
             created_at_ms      INTEGER NOT NULL,\
             updated_at_ms      INTEGER NOT NULL\
         );",
    )?;
    // #1290 — which platform a daemon send belongs to. Rows written before
    // this column existed were all Gmail sends (#449).
    let has_platform: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('self_sent_messages') WHERE name = 'platform'")?
        .exists([])?;
    if !has_platform {
        conn.execute(
            "ALTER TABLE self_sent_messages ADD COLUMN platform TEXT NOT NULL DEFAULT 'gmail'",
            [],
        )?;
    }
    Ok(())
}

const SEND_COLUMNS: &str = "action_id, team_id, channel_id, thread_ts, identity, sender_user_id, \
     body, status, attempts, remote_channel, remote_ts, observed_user, observed_bot_id, error, \
     attempt_started_ms, created_at_ms, updated_at_ms";

fn bad_column(i: usize, v: String) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(i, v, rusqlite::types::Type::Text)
}

fn row_to_send(r: &Row<'_>) -> rusqlite::Result<SlackContactSend> {
    let identity: String = r.get(4)?;
    let status: String = r.get(7)?;
    Ok(SlackContactSend {
        action_id: r.get(0)?,
        team_id: r.get(1)?,
        channel_id: r.get(2)?,
        thread_ts: r.get(3)?,
        identity: SlackSendIdentity::parse(&identity).ok_or_else(|| bad_column(4, identity))?,
        sender_user_id: r.get(5)?,
        body: r.get(6)?,
        status: SlackSendStatus::parse(&status).ok_or_else(|| bad_column(7, status))?,
        attempts: r.get(8)?,
        remote_channel: r.get(9)?,
        remote_ts: r.get(10)?,
        observed_user: r.get(11)?,
        observed_bot_id: r.get(12)?,
        error: r.get(13)?,
        attempt_started_ms: r.get(14)?,
        created_at_ms: r.get(15)?,
        updated_at_ms: r.get(16)?,
    })
}

fn get_send(c: &Connection, action_id: &str) -> rusqlite::Result<Option<SlackContactSend>> {
    c.query_row(
        &format!("SELECT {SEND_COLUMNS} FROM slack_contact_sends WHERE action_id = ?1"),
        params![action_id],
        row_to_send,
    )
    .optional()
}

fn check(value: &str, what: &str) -> StoreResult<()> {
    if value.trim().is_empty() || value.contains(['\n', '\r', '\0']) {
        return Err(StoreError::InvalidInput(format!("invalid Slack {what}")));
    }
    Ok(())
}

impl Store {
    /// Remember where a reply to `target.message_id` goes. Idempotent: a
    /// re-poll of the same message rewrites the same row.
    pub fn record_slack_send_target(&self, target: &SlackSendTarget) -> StoreResult<()> {
        check(&target.message_id, "message id")?;
        check(&target.team_id, "team id")?;
        check(&target.channel_id, "conversation id")?;
        if let Some(ts) = &target.thread_ts {
            check(ts, "thread ts")?;
        }
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO slack_send_targets \
                     (message_id, team_id, channel_id, thread_ts, kind, label, created_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(message_id) DO UPDATE SET \
                     team_id = excluded.team_id, channel_id = excluded.channel_id, \
                     thread_ts = excluded.thread_ts, kind = excluded.kind, \
                     label = excluded.label",
                params![
                    target.message_id,
                    target.team_id,
                    target.channel_id,
                    target.thread_ts,
                    target.kind.as_str(),
                    target.label,
                    now_millis()
                ],
            )
        })?;
        Ok(())
    }

    pub fn slack_send_target(&self, message_id: &str) -> StoreResult<Option<SlackSendTarget>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT message_id, team_id, channel_id, thread_ts, kind, label \
                   FROM slack_send_targets WHERE message_id = ?1",
                params![message_id],
                |r| {
                    let kind: String = r.get(4)?;
                    Ok(SlackSendTarget {
                        message_id: r.get(0)?,
                        team_id: r.get(1)?,
                        channel_id: r.get(2)?,
                        thread_ts: r.get(3)?,
                        kind: SlackConversationKind::parse(&kind)
                            .ok_or_else(|| bad_column(4, kind))?,
                        label: r.get(5)?,
                    })
                },
            )
            .optional()
        })
    }

    /// Begin one send attempt for an approved action, atomically.
    ///
    /// * No row: insert it (`sending`, attempt 1).
    /// * `sent`: [`SlackSendStart::AlreadySent`] — the caller must not post.
    /// * Anything else: back to `sending`, attempt + 1. The destination,
    ///   identity and body recorded by the first attempt are kept: a retry
    ///   goes where the owner approved, as whom the card said.
    ///
    /// The caller holds the action's `sending` claim, so two attempts for
    /// one action never run at once.
    pub fn start_slack_contact_send(
        &self,
        new: &NewSlackContactSend<'_>,
    ) -> StoreResult<SlackSendStart> {
        check(new.action_id, "action id")?;
        check(new.team_id, "team id")?;
        check(new.channel_id, "conversation id")?;
        check(new.sender_user_id, "sender user id")?;
        if new.body.trim().is_empty() {
            return Err(StoreError::InvalidInput("empty Slack message".into()));
        }
        let now = now_millis();
        self.with_conn(|c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| {
                let previous = get_send(c, new.action_id)?;
                if let Some(p) = &previous {
                    if p.status == SlackSendStatus::Sent {
                        return Ok(SlackSendStart::AlreadySent(p.clone()));
                    }
                    c.execute(
                        "UPDATE slack_contact_sends \
                            SET status = 'sending', attempts = attempts + 1, \
                                attempt_started_ms = ?2, updated_at_ms = ?2 \
                          WHERE action_id = ?1",
                        params![new.action_id, now],
                    )?;
                } else {
                    c.execute(
                        "INSERT INTO slack_contact_sends \
                             (action_id, team_id, channel_id, thread_ts, identity, \
                              sender_user_id, body, status, attempts, attempt_started_ms, \
                              created_at_ms, updated_at_ms) \
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'sending', 1, ?8, ?8, ?8)",
                        params![
                            new.action_id,
                            new.team_id,
                            new.channel_id,
                            new.thread_ts,
                            new.identity.as_str(),
                            new.sender_user_id,
                            new.body,
                            now
                        ],
                    )?;
                }
                let row =
                    get_send(c, new.action_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
                Ok(SlackSendStart::Attempt {
                    previous: previous.map(Box::new),
                    row,
                })
            })();
            match result {
                Ok(v) => {
                    c.execute_batch("COMMIT")?;
                    Ok(v)
                }
                Err(e) => {
                    let _ = c.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
    }

    /// `sending → sent`. `false` when no attempt was in flight.
    pub fn finish_slack_contact_sent(
        &self,
        action_id: &str,
        remote_channel: &str,
        remote_ts: &str,
        observed_user: Option<&str>,
        observed_bot_id: Option<&str>,
    ) -> StoreResult<bool> {
        let n = self.with_conn(|c| {
            c.execute(
                "UPDATE slack_contact_sends \
                    SET status = 'sent', remote_channel = ?2, remote_ts = ?3, \
                        observed_user = ?4, observed_bot_id = ?5, error = NULL, \
                        updated_at_ms = ?6 \
                  WHERE action_id = ?1 AND status = 'sending'",
                params![
                    action_id,
                    remote_channel,
                    remote_ts,
                    observed_user,
                    observed_bot_id,
                    now_millis()
                ],
            )
        })?;
        Ok(n == 1)
    }

    /// `sending → failed | unknown | unverified`. `false` when no attempt
    /// was in flight.
    pub fn finish_slack_contact_failed(
        &self,
        action_id: &str,
        status: SlackSendStatus,
        error: &str,
    ) -> StoreResult<bool> {
        if matches!(status, SlackSendStatus::Sending | SlackSendStatus::Sent) {
            return Err(StoreError::InvalidInput(format!(
                "{} is not a failure",
                status.as_str()
            )));
        }
        let n = self.with_conn(|c| {
            c.execute(
                "UPDATE slack_contact_sends \
                    SET status = ?2, error = ?3, updated_at_ms = ?4 \
                  WHERE action_id = ?1 AND status = 'sending'",
                params![action_id, status.as_str(), error, now_millis()],
            )
        })?;
        Ok(n == 1)
    }

    pub fn slack_contact_send(&self, action_id: &str) -> StoreResult<Option<SlackContactSend>> {
        self.with_conn(|c| get_send(c, action_id))
    }

    /// #449 for every platform: record a message the daemon itself sent, so
    /// no observer mistakes it for one the owner typed. Idempotent.
    pub fn record_platform_self_sent_message(
        &self,
        platform: &str,
        message_id: &str,
        thread_id: Option<&str>,
        entity_id: Option<&str>,
        action_id: Option<&str>,
    ) -> StoreResult<()> {
        check(platform, "platform")?;
        check(message_id, "message id")?;
        self.with_conn(|c| {
            c.execute(
                "INSERT OR IGNORE INTO self_sent_messages \
                     (message_id, thread_id, entity_id, action_id, sent_at_ms, platform) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    message_id,
                    thread_id,
                    entity_id,
                    action_id,
                    now_millis(),
                    platform
                ],
            )
        })?;
        Ok(())
    }

    /// The platform a recorded daemon send belongs to.
    pub fn self_sent_message_platform(&self, message_id: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT platform FROM self_sent_messages WHERE message_id = ?1",
                params![message_id],
                |r| r.get(0),
            )
            .optional()
        })
    }
}
