//! Read-only, source-preserving conversation windows for drafting (#1691).
//! This layer selects stored messages; it neither decides relevance nor sends.

use crate::{rusqlite::params, Store, StoreResult};
use serde::{Deserialize, Serialize};

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
            let mut stmt = conn.prepare(
                "SELECT messageId, platform, accountEntityId, threadId, fromEmail,
                        subject, COALESCE(body, ''), receivedAt,
                        COALESCE(CAST(ROUND((julianday(receivedAt) - 2440587.5) * 86400000) AS INTEGER), firstSeenAt) AS ts
                 FROM emails
                 WHERE platform = ?1 AND accountEntityId IS ?2 AND threadId = ?3
                   AND kind != 'compose'
                   AND COALESCE(CAST(ROUND((julianday(receivedAt) - 2440587.5) * 86400000) AS INTEGER), firstSeenAt) <= ?4
                 ORDER BY ts DESC, messageId DESC LIMIT ?5",
            )?;
            let mut rows = stmt.query_map(
                params![scope.platform, scope.account_id, scope.thread_id, at_ms, limit.min(100) as i64],
                |r| Ok(ConversationMessage {
                    message_id: r.get(0)?, platform: r.get(1)?, account_id: r.get(2)?,
                    thread_id: r.get(3)?, sender: r.get(4)?, subject: r.get(5)?,
                    body: r.get(6)?, timestamp: r.get(7)?, timestamp_ms: r.get(8)?,
                }),
            )?.collect::<Result<Vec<_>, _>>()?;
            rows.reverse();
            Ok(rows)
        })
    }
}
