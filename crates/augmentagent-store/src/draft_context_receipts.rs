//! Private evidence receipts, kept separately from public tool notices (#1694).
use crate::{
    rusqlite::{params, Connection},
    Store, StoreResult,
};

/// Sub-platform qualifiers a SocialAPI conversation id may carry, mirroring the
/// networks the channel labels. Only these are stripped, so two conversations
/// that merely happen to be colon-separated are never treated as one.
const SOCIALAPI_QUALIFIERS: &[&str] = &[
    "instagram",
    "ig",
    "x",
    "twitter",
    "linkedin",
    "li",
    "facebook",
    "fb",
    "threads",
    "tiktok",
    "youtube",
    "pinterest",
];

/// The bare conversation id behind a qualified SocialAPI reference, e.g.
/// `instagram:<conversation>` → `<conversation>`.
fn socialapi_unqualified(conversation: &str) -> Option<&str> {
    let (qualifier, rest) = conversation.split_once(':')?;
    (!rest.is_empty()
        && SOCIALAPI_QUALIFIERS
            .iter()
            .any(|known| qualifier.eq_ignore_ascii_case(known)))
    .then_some(rest)
}

/// #1830 — a `socialapi dm --post` card keys its conversation as
/// `<sub-platform>:<conversation>` while the ingested trigger message keeps the
/// bare conversation id. The two name the same thread, so compare the qualified
/// form against the bare one. A qualifier on both sides still has to match
/// exactly: `instagram:c` and `x:c` are different conversations.
fn socialapi_same_conversation(source: &str, target: &str) -> bool {
    source == target
        || socialapi_unqualified(source) == Some(target)
        || Some(source) == socialapi_unqualified(target)
}

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS draft_context_receipts (
        id TEXT PRIMARY KEY, message_id TEXT NOT NULL, action_id TEXT,
        created_at_ms INTEGER NOT NULL, receipt_json TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS draft_context_receipts_message ON draft_context_receipts(message_id, created_at_ms);
        CREATE INDEX IF NOT EXISTS draft_context_receipts_action ON draft_context_receipts(action_id);")?;
    Ok(())
}

impl Store {
    /// Caller supplies a metadata-only receipt (IDs, hashes, gaps), not another
    /// copy of source bodies. Apply the same secret masking as draft storage.
    pub fn save_draft_context_receipt(
        &self,
        message_id: &str,
        receipt: &serde_json::Value,
    ) -> StoreResult<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().timestamp_millis();
        let raw = receipt.to_string();
        let masked = crate::redact::mask(&raw);
        self.with_conn(|c| {
            let tx=c.unchecked_transaction()?;
            tx.execute("INSERT INTO draft_context_receipts(id,message_id,created_at_ms,receipt_json) VALUES (?1,?2,?3,?4)",params![id,message_id,now,masked.as_ref()])?;
            // Bound retention; receipts attached to unresolved approvals remain
            // inspectable until the owner acts, even beyond the normal horizon.
            tx.execute("DELETE FROM draft_context_receipts WHERE created_at_ms < ?1
                AND NOT EXISTS (SELECT 1 FROM actions a WHERE a.id=draft_context_receipts.action_id
                  AND a.status IN ('pending','approved','scheduled','sending'))",[now-30*24*60*60*1000])?;
            tx.commit()
        })?;
        Ok(id)
    }

    pub fn attach_draft_context_receipt(
        &self,
        receipt_id: &str,
        action_id: &str,
    ) -> StoreResult<bool> {
        self.with_conn(|c| {
            c.create_scalar_function(
                "aa_socialapi_same_conversation",
                2,
                crate::rusqlite::functions::FunctionFlags::SQLITE_UTF8
                    | crate::rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
                |ctx| {
                    let source: String = ctx.get(0)?;
                    let target: String = ctx.get(1)?;
                    Ok(socialapi_same_conversation(&source, &target))
                },
            )?;
            Ok(c.execute(
                "UPDATE draft_context_receipts SET action_id=?2
            WHERE id=?1 AND (action_id IS NULL OR action_id=?2)
              AND EXISTS (SELECT 1 FROM actions a WHERE a.id=?2 AND (
                  a.messageId=draft_context_receipts.message_id OR EXISTS (
                    SELECT 1 FROM emails source JOIN emails target ON target.messageId=a.messageId
                    WHERE source.messageId=draft_context_receipts.message_id
                      AND source.platform=target.platform
                      AND source.accountEntityId IS target.accountEntityId
                      AND source.threadId IS NOT NULL AND target.threadId IS NOT NULL
                      AND (source.threadId=target.threadId OR (source.platform='socialapi'
                        AND aa_socialapi_same_conversation(source.threadId,target.threadId)))
                  )))",
                params![receipt_id, action_id],
            )? == 1)
        })
    }

    pub fn draft_context_receipts(&self, action_id: &str) -> StoreResult<Vec<String>> {
        self.with_conn(|c| {
            let mut stmt=c.prepare("SELECT receipt_json FROM draft_context_receipts WHERE action_id=?1 ORDER BY created_at_ms,id")?;
            let rows=stmt.query_map([action_id],|r|r.get(0))?.collect();
            rows
        })
    }
}
