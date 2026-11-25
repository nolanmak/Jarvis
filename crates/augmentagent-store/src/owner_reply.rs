//! #2284 — one cross-channel answer to "has the owner replied in this
//! conversation since T?".
//!
//! The evidence lives in four places, all keyed by conversation:
//!
//! * `message_index` rows with `from_me = 1` (iMessage, WhatsApp, Discord,
//!   LinkedIn, Slack and the rest of what `emails` holds). Rows whose time is
//!   only the ingest time (`ts_fallback = 1`) can't be ordered against an
//!   inbound and never count.
//! * `owner_messages`, a small ledger for channels whose owner messages never
//!   become `emails` rows: SocialAPI (Instagram and friends) DMs the owner
//!   sent, seen in polled history or the webhook.
//! * `outbound_thread_log`: Gmail replies the SENT-folder observer saw the
//!   owner write by hand.
//! * `self_sent_messages`: sends Jarvis made on the owner's behalf.
//!
//! A card's `threadId` and the index's `conversation_id` are the same string
//! on every channel today except where the live and history sides of one
//! channel disagree on a prefix; [`conversation_ids_for`] owns that mapping.

use rusqlite::{params, Connection, OptionalExtension};

use crate::{Store, StoreResult};

/// `message_index.conv_kind` values that are conversations someone can be
/// waiting on the owner in. Channels, notes and meetings are not.
const CONVERSATION_KINDS: &str = "('dm', 'group', 'email')";

/// iMessage's history threads carry this prefix; so do the card thread ids.
const IMESSAGE_PREFIX: &str = "imessage:";
/// The WhatsApp history export's thread prefix (`augmentagent-channel-whatsapp-history`).
/// The live sidecar's cards use the bare JID instead.
const WHATSAPP_HISTORY_PREFIX: &str = "whatsapp-history:";

/// A conversation whose newest inbound message has no owner message after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnansweredConversation {
    pub platform: String,
    pub conversation_id: String,
    /// `dm` | `group` | `email`.
    pub conv_kind: String,
    pub conversation_title: Option<String>,
    pub last_inbound_message_id: String,
    pub last_inbound_ms: i64,
}

pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS owner_messages (
            platform        TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            message_id      TEXT NOT NULL,
            sent_at_ms      INTEGER NOT NULL,
            source          TEXT NOT NULL,
            recorded_at_ms  INTEGER NOT NULL,
            PRIMARY KEY (platform, message_id));
         CREATE INDEX IF NOT EXISTS idx_owner_messages_conv
            ON owner_messages(platform, conversation_id, sent_at_ms);",
    )
}

/// The conversation ids a card on `platform` with `thread_id` may be stored
/// under, the card's own id first. Empty for a blank id.
pub fn conversation_ids_for(platform: &str, thread_id: &str) -> Vec<String> {
    let id = thread_id.trim();
    if id.is_empty() {
        return Vec::new();
    }
    let mut ids = vec![id.to_string()];
    match platform {
        // The history export prefixes the JID; the live sidecar does not.
        "whatsapp" => match id.strip_prefix(WHATSAPP_HISTORY_PREFIX) {
            Some(bare) if !bare.is_empty() => ids.push(bare.to_string()),
            Some(_) => {}
            None => ids.push(format!("{WHATSAPP_HISTORY_PREFIX}{id}")),
        },
        // Every iMessage writer prefixes today; tolerate a bare handle.
        "imessage" if !id.starts_with(IMESSAGE_PREFIX) => {
            ids.push(format!("{IMESSAGE_PREFIX}{id}"));
        }
        // Gmail thread ids, SocialAPI conversation UUIDs, LinkedIn URNs,
        // Slack/Discord channel ids, Telegram chat ids, X conversation ids and
        // Instagram thread ids are the same string on the card and the index.
        _ => {}
    }
    ids
}

fn exists(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<bool> {
    Ok(conn.query_row(sql, p, |_| Ok(())).optional()?.is_some())
}

fn replied_in(
    conn: &Connection,
    platform: &str,
    conversation_id: &str,
    since_ms: i64,
) -> rusqlite::Result<bool> {
    if !platform.is_empty()
        && (exists(
            conn,
            "SELECT 1 FROM message_index WHERE conversation_id = ?1 AND platform = ?2 \
               AND from_me = 1 AND ts_fallback = 0 AND ts_ms > ?3 LIMIT 1",
            params![conversation_id, platform, since_ms],
        )? || exists(
            conn,
            "SELECT 1 FROM owner_messages WHERE platform = ?1 AND conversation_id = ?2 \
               AND sent_at_ms > ?3 LIMIT 1",
            params![platform, conversation_id, since_ms],
        )?)
    {
        return Ok(true);
    }
    // Thread-keyed Gmail ledgers, unscoped by platform as the reconcile
    // sweep's Rule 1 always read them.
    Ok(exists(
        conn,
        "SELECT 1 FROM outbound_thread_log WHERE thread_id = ?1 AND sent_at_ms > ?2 LIMIT 1",
        params![conversation_id, since_ms],
    )? || exists(
        conn,
        "SELECT 1 FROM self_sent_messages WHERE thread_id = ?1 AND sent_at_ms > ?2 LIMIT 1",
        params![conversation_id, since_ms],
    )?)
}

impl Store {
    /// Has the owner sent anything in this conversation strictly after
    /// `since_ms`, by any route: typed on another device, sent from the
    /// platform's own app or web, or approved and sent by Jarvis?
    ///
    /// `conversation_id` may be a card's `actions.threadId` or a
    /// `message_index.conversation_id`; [`conversation_ids_for`] maps between
    /// them. `platform` is `emails.platform`; an empty platform only consults
    /// the thread-keyed Gmail ledgers.
    pub fn owner_replied_since(
        &self,
        platform: &str,
        conversation_id: &str,
        since_ms: i64,
    ) -> StoreResult<bool> {
        let ids = conversation_ids_for(platform, conversation_id);
        let guard = self.conn.lock().expect("store mutex poisoned");
        for id in &ids {
            if replied_in(&guard, platform, id, since_ms)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Record one message the owner sent. Idempotent per
    /// `(platform, message_id)`; returns true when the row is new.
    pub fn record_owner_message(
        &self,
        platform: &str,
        conversation_id: &str,
        message_id: &str,
        sent_at_ms: i64,
        source: &str,
    ) -> StoreResult<bool> {
        let now = crate::store::now_millis();
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n = guard.execute(
            "INSERT OR IGNORE INTO owner_messages \
               (platform, conversation_id, message_id, sent_at_ms, source, recorded_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![platform, conversation_id, message_id, sent_at_ms, source, now],
        )?;
        Ok(n > 0)
    }

    /// Conversations whose newest inbound message is at or after `since_ms`
    /// and has no owner message after it, newest first, at most `limit`.
    /// Read from `message_index`, so a conversation is listed only once its
    /// inbound has been indexed.
    pub fn unanswered_conversations(
        &self,
        since_ms: i64,
        limit: usize,
    ) -> StoreResult<Vec<UnansweredConversation>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let guard = self.conn.lock().expect("store mutex poisoned");
        let sql = format!(
            "SELECT platform, conversation_id, MAX(ts_ms) AS last_ms \
               FROM message_index \
              WHERE from_me = 0 AND ts_fallback = 0 AND ts_ms >= ?1 \
                AND conv_kind IN {CONVERSATION_KINDS} \
              GROUP BY platform, conversation_id \
              ORDER BY last_ms DESC"
        );
        let mut stmt = guard.prepare(&sql)?;
        let mut rows = stmt.query(params![since_ms])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            let platform: String = r.get(0)?;
            let conversation_id: String = r.get(1)?;
            let last_ms: i64 = r.get(2)?;
            let answered = conversation_ids_for(&platform, &conversation_id)
                .iter()
                .map(|id| replied_in(&guard, &platform, id, last_ms))
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .any(|b| b);
            if answered {
                continue;
            }
            let (message_id, conv_kind, title) = guard.query_row(
                "SELECT message_id, conv_kind, conversation_title FROM message_index \
                  WHERE platform = ?1 AND conversation_id = ?2 AND from_me = 0 \
                    AND ts_fallback = 0 AND ts_ms = ?3 \
                  ORDER BY message_id DESC LIMIT 1",
                params![platform, conversation_id, last_ms],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get(2)?)),
            )?;
            out.push(UnansweredConversation {
                platform,
                conversation_id,
                conv_kind,
                conversation_title: title,
                last_inbound_message_id: message_id,
                last_inbound_ms: last_ms,
            });
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }
}
