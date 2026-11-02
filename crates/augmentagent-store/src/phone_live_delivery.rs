//! Minimal content-free voice send journal. Persist intent before socket I/O;
//! ambiguous sends never become automatic replays after a crash or timeout.
use super::*;

impl Store {
    pub fn phone_live_reply_intent(
        &self,
        call_id: &str,
        session_id: &str,
        delegation_id: &str,
        event_id: &str,
        end_offset: usize,
        complete: bool,
    ) -> PhoneResult<()> {
        identifier(event_id)?;
        if event_id.len() > 256 || end_offset == 0 || end_offset > 64_000 {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            if !live_binding(tx, call_id, session_id)? {
                return Err(PhoneError::Conflict);
            }
            let available: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM phone_live_dispatches d LEFT JOIN phone_live_result_receipts r USING(call_id,delegation_id) WHERE d.call_id=?1 AND d.delegation_id=?2 AND d.delivered=0 AND d.result IS NOT NULL AND (r.disposition IS NULL OR r.disposition='pending'))",params![call_id,delegation_id],|r|r.get(0))?;
            if !available {
                return Err(PhoneError::Conflict);
            }
            tx.execute("INSERT INTO phone_live_reply_intents(call_id,event_id,delegation_id,end_offset,complete,disposition) VALUES(?1,?2,?3,?4,?5,'sending')",params![call_id,event_id,delegation_id,end_offset,complete])?;
            tx.execute("INSERT INTO phone_live_result_receipts(call_id,delegation_id,disposition) VALUES(?1,?2,'sending') ON CONFLICT(call_id,delegation_id) DO UPDATE SET disposition='sending'",params![call_id,delegation_id])?;
            Ok(())
        })
    }
    /// Match only a pending locally generated command. A late acknowledgment
    /// cannot restart a publication already abandoned on timeout/cleanup.
    pub fn phone_live_reply_settle(
        &self,
        call_id: &str,
        session_id: &str,
        event_id: &str,
        accepted: bool,
    ) -> PhoneResult<bool> {
        transaction(self, |tx| {
            if !live_binding(tx, call_id, session_id)? {
                return Ok(false);
            }
            let row:Option<(String,bool)>=tx.query_row("SELECT delegation_id,complete FROM phone_live_reply_intents WHERE call_id=?1 AND event_id=?2 AND disposition='sending'",params![call_id,event_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let Some((delegation, complete)) = row else {
                return Ok(false);
            };
            tx.execute("UPDATE phone_live_reply_intents SET disposition=?3 WHERE call_id=?1 AND event_id=?2",params![call_id,event_id,if accepted {"accepted"} else {"rejected"}])?;
            tx.execute("UPDATE phone_live_result_receipts SET disposition=?3 WHERE call_id=?1 AND delegation_id=?2",params![call_id,delegation,if accepted && complete {"accepted"} else {"pending"}])?;
            if accepted && complete {
                tx.execute("UPDATE phone_live_dispatches SET delivered=1,result=NULL WHERE call_id=?1 AND delegation_id=?2",params![call_id,delegation])?;
            }
            Ok(true)
        })
    }
    pub fn phone_live_result_timeout(
        &self,
        call_id: &str,
        session_id: &str,
        delegation_id: &str,
    ) -> PhoneResult<()> {
        transaction(self, |tx| {
            live_binding(tx, call_id, session_id)?;
            tx.execute("UPDATE phone_live_reply_intents SET disposition='timed_out' WHERE call_id=?1 AND delegation_id=?2 AND disposition='sending'",params![call_id,delegation_id])?;
            tx.execute("UPDATE phone_live_result_receipts SET disposition='timed_out' WHERE call_id=?1 AND delegation_id=?2 AND disposition='sending'",params![call_id,delegation_id])?;
            tx.execute("UPDATE phone_live_dispatches SET delivered=1,result=NULL WHERE call_id=?1 AND delegation_id=?2",params![call_id,delegation_id])?;
            Ok(())
        })
    }
    pub fn phone_live_result_disposition(
        &self,
        call_id: &str,
        delegation_id: &str,
    ) -> PhoneResult<Option<String>> {
        Ok(self.with_conn(|c|c.query_row("SELECT disposition FROM phone_live_result_receipts WHERE call_id=?1 AND delegation_id=?2",params![call_id,delegation_id],|r|r.get(0)).optional())?)
    }
}
