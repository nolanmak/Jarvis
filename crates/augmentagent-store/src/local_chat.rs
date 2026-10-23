//! Durable local conversation transcripts; authorization is enforced by the adapter.
use crate::{Store, StoreError, StoreResult, SurfaceConversationRef};
use rusqlite::{params, Connection};
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS local_chat_messages (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        conversation TEXT NOT NULL,
        event_id TEXT NOT NULL,
        text TEXT NOT NULL,
        UNIQUE(conversation,event_id)
    );",
    )?;
    Ok(())
}
impl Store {
    pub fn append_local_chat(
        &self,
        conversation: &SurfaceConversationRef,
        event_id: &str,
        text: &str,
    ) -> StoreResult<()> {
        if conversation.account().platform().as_str() != "cli"
            || event_id.is_empty()
            || event_id.len() > 256
            || text.len() > 1024 * 1024
        {
            return Err(StoreError::InvalidInput(
                "invalid local transcript entry".into(),
            ));
        }
        self.with_conn(|conn| {
            conn.execute("INSERT INTO local_chat_messages(conversation,event_id,text) VALUES(?1,?2,?3) ON CONFLICT(conversation,event_id) DO NOTHING", params![conversation.storage_key(), event_id, text])?;
            let existing: String = conn.query_row("SELECT text FROM local_chat_messages WHERE conversation=?1 AND event_id=?2", params![conversation.storage_key(), event_id], |row| row.get(0))?;
            Ok(existing == text)
        }).and_then(|same| if same {Ok(())} else {Err(StoreError::InvalidInput("conflicting local transcript event".into()))})
    }
    /// At most 50 entries and 16 KiB, newest entries retained in chronological order.
    pub fn local_chat_history(&self, conversation: &SurfaceConversationRef) -> StoreResult<String> {
        let rows = self.with_conn(|conn| {
            let mut query = conn.prepare("SELECT substr(text,1,16384) FROM local_chat_messages WHERE conversation=?1 ORDER BY seq DESC LIMIT 50")?;
            let rows = query.query_map([conversation.storage_key()], |row| row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })?;
        let mut selected = Vec::new();
        let mut bytes = 0;
        for mut text in rows {
            let remaining = 16384usize.saturating_sub(bytes + 1);
            if remaining == 0 {
                break;
            }
            if text.len() > remaining {
                let mut end = remaining;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            bytes += text.len() + 1;
            selected.push(text);
        }
        selected.reverse();
        Ok(selected.join("\n"))
    }
}
