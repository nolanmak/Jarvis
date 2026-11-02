//! Durable context for read-only phone answers produced outside native reasoning.
use super::*;
use crate::SurfaceConversationRef;
fn attention_authority(tx: &Transaction<'_>, call_id: &str) -> PhoneResult<()> {
    let (encoded, revision): (String, i64) = tx.query_row(
        "SELECT o.origin_json,a.revision FROM phone_origins o JOIN phone_origin_authority a ON a.call_id=o.call_id WHERE o.call_id=?1",
        [call_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    let origin: PhoneOrigin =
        serde_json::from_str(&encoded).map_err(|_| PhoneError::InvalidInput)?;
    let valid = crate::owner::load_binding(tx, origin.owner.account())?.is_some_and(|a| {
        a.revision == revision
            && a.owner == origin.owner
            && a.is_control_conversation(origin.conversation.conversation_id())
    });
    if !valid {
        return Err(PhoneError::Conflict);
    }
    Ok(())
}
impl Store {
    pub fn phone_attention_notice(
        &self,
        call_id: &str,
        task_id: &str,
        notice: &str,
    ) -> PhoneResult<bool> {
        if notice.trim().is_empty() || notice.len() > 1_000 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            attention_authority(tx, call_id)?;
            Ok(tx.execute(
                "INSERT OR IGNORE INTO phone_attention_notices(call_id,task_id,notice)
                SELECT j.call_id,j.task_id,?3 FROM phone_dispatch_jobs j
                JOIN phone_live_dispatches d ON d.call_id=j.call_id AND d.task_id=j.task_id
                JOIN phone_live_sessions s ON s.call_id=j.call_id
                JOIN phone_calls c ON c.id=j.call_id
                WHERE j.call_id=?1 AND j.task_id=?2 AND j.state='running' AND s.active=1
                AND c.state='connected' AND c.ended_ms IS NULL AND c.stop_requested=0",
                params![call_id, task_id, notice],
            )? == 1)
        })
    }
    /// Persist the claim and erase its private text before transport I/O.
    /// A restart cannot replay an ambiguously transmitted progress message.
    pub fn phone_claim_attention_notice(
        &self,
        call_id: &str,
        session_id: &str,
        task_id: &str,
    ) -> PhoneResult<Option<String>> {
        transaction(self, |tx| {
            attention_authority(tx, call_id)?;
            if !live_binding(tx, call_id, session_id)? {
                return Ok(None);
            }
            let text: Option<String> = tx
                .query_row(
                    "SELECT n.notice FROM phone_attention_notices n
                JOIN phone_dispatch_jobs j ON j.call_id=n.call_id AND j.task_id=n.task_id
                JOIN phone_calls c ON c.id=n.call_id
                WHERE n.call_id=?1 AND n.task_id=?2 AND n.claimed=0 AND j.state='running'
                AND c.state='connected' AND c.ended_ms IS NULL AND c.stop_requested=0",
                    params![call_id, task_id],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if text.is_some() {
                tx.execute("UPDATE phone_attention_notices SET claimed=1,notice=NULL WHERE call_id=?1 AND task_id=?2",params![call_id,task_id])?;
            }
            Ok(text)
        })
    }
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
                tx.execute(
                    "DELETE FROM phone_attention_notices WHERE call_id=?1 AND task_id=?2",
                    params![call_id, task_id],
                )?;
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
