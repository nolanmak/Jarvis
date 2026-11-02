//! Durable context for read-only phone answers produced outside native reasoning.
use super::*;
use crate::SurfaceConversationRef;
impl Store {
    /// Facts and final result commit together before either chat or voice I/O.
    pub fn phone_finish_attention_dispatch(
        &self,
        call_id: &str,
        task_id: &str,
        request: &str,
        result: &str,
    ) -> PhoneResult<()> {
        if request.len() > 1_200 || result.len() > 8_100 {
            return Err(PhoneError::InvalidInput);
        }
        let origin = self.phone_origin(call_id)?;
        let binding = self.surface_conversation(&origin.conversation)?;
        let session = binding.as_ref().map(|b| b.native_session_id.as_str());
        let content = serde_json::json!({"request":request,"answer":result}).to_string();
        transaction(self, |tx| {
            let revision: i64 = tx.query_row(
                "SELECT revision FROM phone_origin_authority WHERE call_id=?1",
                [call_id],
                |r| r.get(0),
            )?;
            let authority = crate::owner::load_binding(tx, origin.owner.account())?
                .ok_or(PhoneError::Conflict)?;
            if authority.revision != revision
                || authority.owner != origin.owner
                || !authority.is_control_conversation(origin.conversation.conversation_id())
            {
                return Err(PhoneError::Conflict);
            }
            let changed=tx.execute("UPDATE phone_dispatch_jobs SET state='ready',result=?3 WHERE call_id=?1 AND task_id=?2 AND state='running'",params![call_id,task_id,result])?;
            if changed == 1 {
                tx.execute("INSERT INTO phone_attention_context(call_id,task_id,conversation_key,native_session_id,content) VALUES(?1,?2,?3,?4,?5)",params![call_id,task_id,origin.conversation.storage_key(),session,content])?;
                tx.execute("UPDATE phone_live_dispatches SET result=?3 WHERE call_id=?1 AND task_id=?2 AND delivered=0 AND EXISTS(SELECT 1 FROM phone_live_sessions s WHERE s.call_id=?1 AND s.active=1)",params![call_id,task_id,result])?;
            }
            Ok(())
        })
    }
    /// Called under the existing native conversation mutex, after its durable
    /// turn claim. Data is attached to that turn once; it cannot dispatch tools.
    pub fn phone_claim_attention_context(
        &self,
        chat: &SurfaceConversationRef,
        native_id: Option<&str>,
        turn_id: &str,
    ) -> PhoneResult<String> {
        transaction(self, |tx| {
            let mut stmt=tx.prepare("SELECT c.call_id,c.task_id,c.native_session_id,c.content,o.origin_json,a.revision FROM phone_attention_context c JOIN phone_origins o ON o.call_id=c.call_id JOIN phone_origin_authority a ON a.call_id=c.call_id WHERE c.conversation_key=?1 AND c.consumed_turn_id IS NULL ORDER BY c.rowid LIMIT 16")?;
            let rows = stmt
                .query_map([chat.storage_key()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut context = Vec::new();
            let mut bytes = 0;
            for (call, task, session, content, encoded, revision) in rows {
                let origin: PhoneOrigin =
                    serde_json::from_str(&encoded).map_err(|_| PhoneError::InvalidInput)?;
                let valid =
                    crate::owner::load_binding(tx, origin.owner.account())?.is_some_and(|a| {
                        a.revision == revision
                            && a.owner == origin.owner
                            && a.is_control_conversation(chat.conversation_id())
                    }) && origin.conversation == *chat
                        && session.as_deref().is_none_or(|s| Some(s) == native_id);
                if valid && bytes + content.len() > 16_000 {
                    break;
                }
                tx.execute("UPDATE phone_attention_context SET consumed_turn_id=?3,content=NULL WHERE call_id=?1 AND task_id=?2 AND consumed_turn_id IS NULL",params![call,task,if valid {turn_id}else{"discarded"}])?;
                if valid {
                    bytes += content.len();
                    context.push(content);
                }
            }
            Ok(context.join("\n"))
        })
    }
}
