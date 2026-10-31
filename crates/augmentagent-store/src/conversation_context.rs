//! Read-only, source-preserving conversation windows for drafting (#1691).
//! This layer selects stored messages; it neither decides relevance nor sends.

use crate::{
    rusqlite::{params, OptionalExtension},
    Email, Store, StoreResult,
};
use serde::{Deserialize, Serialize};

use chrono::{DateTime, NaiveDate, NaiveDateTime};

/// Shared with message indexing so source chronology has one parser.
pub fn parse_source_timestamp_ms(raw: &str) -> Option<i64> {
    let r = raw.trim();
    if r.is_empty() {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(r) {
        return Some(t.timestamp_millis());
    }
    if let Ok(t) = DateTime::parse_from_rfc2822(r) {
        return Some(t.timestamp_millis());
    }
    if let Ok(t) = NaiveDateTime::parse_from_str(r, "%Y-%m-%d %H:%M:%S") {
        return Some(t.and_utc().timestamp_millis());
    }
    if let Ok(d) = NaiveDate::parse_from_str(r, "%Y-%m-%d") {
        return d
            .and_hms_opt(12, 0, 0)
            .map(|t| t.and_utc().timestamp_millis());
    }
    r.parse::<i64>().ok().filter(|n| *n > 1_000_000_000_000)
}

pub fn conversation_timestamp_ms(platform: &str, raw: &str) -> Option<i64> {
    parse_source_timestamp_ms(raw).or_else(|| {
        (platform == "slack")
            .then(|| crate::slack_ingest::ts_to_ms(raw.trim()))
            .flatten()
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMessage {
    pub message_id: String,
    pub platform: String,
    pub account_id: Option<String>,
    pub thread_id: String,
    pub sender: String,
    pub subject: String,
    pub body: String,
    pub timestamp: Option<String>,
    pub timestamp_ms: i64,
}

/// Account is an exact scope: None means legacy rows with no account, not all
/// accounts. Callers must not broaden a failed lookup to another account.
pub struct ConversationScope<'a> {
    pub platform: &'a str,
    pub account_id: Option<&'a str>,
    pub thread_id: &'a str,
}

impl Store {
    /// Read an exact source for owner-requested context preparation. Restore
    /// persisted envelope/attachment metadata when available.
    pub fn context_source(&self, message_id: &str) -> StoreResult<Option<Email>> {
        let mut source = self.with_conn(|c| c.query_row(
            "SELECT messageId,threadId,fromEmail,subject,COALESCE(body,''),COALESCE(receivedAt,''),accountEntityId,platform,kind FROM emails WHERE messageId=?1",
            [message_id], |r| Ok(Email {
                message_id:r.get(0)?,thread_id:r.get(1)?,from:r.get(2)?,subject:r.get(3)?,body:r.get(4)?,
                date:r.get(5)?,account_entity_id:r.get(6)?,platform:r.get(7)?,kind:r.get(8)?,
                to:String::new(),cc:String::new(),attachments:Vec::new(),
            })).optional())?;
        if let Some(email) = source.as_mut() {
            if let Some((to, cc, attachments)) = self.email_retry_guard_context(message_id)? {
                email.to = to;
                email.cc = cc;
                email.attachments = attachments;
            }
            if let Some(recipients) = self.email_recipients(message_id)? {
                email.to = recipients.to.join(", ");
                email.cc = recipients.cc.join(", ");
            }
        }
        Ok(source)
    }

    /// Latest real messages at or before a trigger, returned oldest first.
    /// Synthetic compose rows are intentions, even when their action is sent;
    /// the transport's ingested message is the authoritative history record.
    pub fn conversation_window(
        &self,
        scope: &ConversationScope<'_>,
        at_ms: i64,
        limit: usize,
    ) -> StoreResult<Vec<ConversationMessage>> {
        self.with_conn(|conn| {
            conn.create_scalar_function(
                "aa_context_timestamp_ms",
                2,
                crate::rusqlite::functions::FunctionFlags::SQLITE_UTF8
                    | crate::rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                |ctx| {
                    let platform: String = ctx.get(0)?;
                    let raw: Option<String> = ctx.get(1)?;
                    Ok(raw
                        .as_deref()
                        .and_then(|raw| conversation_timestamp_ms(&platform, raw)))
                },
            )?;
            let mut stmt = conn.prepare(
                "SELECT messageId, platform, accountEntityId, threadId, fromEmail,
                        subject, COALESCE(body, ''), receivedAt,
                        COALESCE(aa_context_timestamp_ms(platform, receivedAt), firstSeenAt) AS ts
                 FROM emails
                 WHERE platform = ?1 AND accountEntityId IS ?2 AND threadId = ?3
                   AND kind != 'compose'
                   AND COALESCE(aa_context_timestamp_ms(platform, receivedAt), firstSeenAt) <= ?4
                 ORDER BY ts DESC, messageId DESC LIMIT ?5",
            )?;
            let mut rows = stmt
                .query_map(
                    params![
                        scope.platform,
                        scope.account_id,
                        scope.thread_id,
                        at_ms,
                        limit.min(100) as i64
                    ],
                    |r| {
                        Ok(ConversationMessage {
                            message_id: r.get(0)?,
                            platform: r.get(1)?,
                            account_id: r.get(2)?,
                            thread_id: r.get(3)?,
                            sender: r.get(4)?,
                            subject: r.get(5)?,
                            body: r.get(6)?,
                            timestamp: r.get(7)?,
                            timestamp_ms: r.get(8)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            rows.reverse();
            Ok(rows)
        })
    }
}
