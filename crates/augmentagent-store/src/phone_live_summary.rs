//! Bounded, restartable, read-only summarization of completed native results.
use super::*;
const FALLBACK: &str =
    "I couldn't prepare a spoken summary. Please check our chat for the full result.";
pub struct PhoneLiveSummaryJob {
    pub call_id: String,
    pub session_id: String,
    pub delegation_id: String,
    pub lease_token: String,
    pub result: String,
}
impl Store {
    pub fn phone_claim_live_summary(
        &self,
        now_ms: i64,
    ) -> PhoneResult<Option<PhoneLiveSummaryJob>> {
        transaction(self, |tx| {
            // Very large results stay intact in the chat outbox. Do not truncate
            // their contents into a misleading partial model summary.
            tx.execute("UPDATE phone_live_dispatches SET result=?1 WHERE delivered=0 AND length(CAST(result AS BLOB))>64000",[FALLBACK])?;
            tx.execute("INSERT OR IGNORE INTO phone_live_summary_jobs(call_id,delegation_id) SELECT d.call_id,d.delegation_id FROM phone_live_dispatches d JOIN phone_live_sessions s ON s.call_id=d.call_id WHERE s.active=1 AND d.delivered=0 AND length(CAST(d.result AS BLOB))>500",[])?;
            tx.execute("UPDATE phone_live_dispatches SET result=?1 WHERE delivered=0 AND length(CAST(result AS BLOB))>500 AND EXISTS(SELECT 1 FROM phone_live_summary_jobs j WHERE j.call_id=phone_live_dispatches.call_id AND j.delegation_id=phone_live_dispatches.delegation_id AND j.attempts>=2 AND j.next_ms<=?2)",params![FALLBACK,now_ms])?;
            let candidate:Option<(String,String,String,String)>=tx.query_row("SELECT d.call_id,s.session_id,d.delegation_id,d.result FROM phone_live_dispatches d JOIN phone_live_sessions s ON s.call_id=d.call_id JOIN phone_live_summary_jobs j ON j.call_id=d.call_id AND j.delegation_id=d.delegation_id WHERE s.active=1 AND d.delivered=0 AND length(CAST(d.result AS BLOB))>500 AND j.attempts<2 AND j.next_ms<=?1 ORDER BY j.next_ms,j.rowid LIMIT 1",[now_ms],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
            let Some((call_id, session_id, delegation_id, result)) = candidate else {
                return Ok(None);
            };
            let lease_token = uuid::Uuid::new_v4().to_string();
            tx.execute("UPDATE phone_live_summary_jobs SET attempts=attempts+1,lease_token=?3,next_ms=?4 WHERE call_id=?1 AND delegation_id=?2",params![call_id,delegation_id,lease_token,now_ms.saturating_add(30_000)])?;
            Ok(Some(PhoneLiveSummaryJob {
                call_id,
                session_id,
                delegation_id,
                lease_token,
                result,
            }))
        })
    }
    pub fn phone_finish_live_summary(
        &self,
        job: &PhoneLiveSummaryJob,
        summary: Option<&str>,
        now_ms: i64,
    ) -> PhoneResult<bool> {
        if summary.is_some_and(|s| s.trim().is_empty() || s.len() > 500) {
            return Err(PhoneError::InvalidInput);
        }
        transaction(self, |tx| {
            if !live_binding(tx, &job.call_id, &job.session_id)? {
                return Ok(false);
            }
            let attempt:Option<i64>=tx.query_row("SELECT attempts FROM phone_live_summary_jobs WHERE call_id=?1 AND delegation_id=?2 AND lease_token=?3 AND next_ms>?4",params![job.call_id,job.delegation_id,job.lease_token,now_ms],|r|r.get(0)).optional()?;
            let Some(attempt) = attempt else {
                return Ok(false);
            };
            if let Some(text) = summary.or((attempt >= 2).then_some(FALLBACK)) {
                tx.execute("UPDATE phone_live_dispatches SET result=?3 WHERE call_id=?1 AND delegation_id=?2 AND delivered=0 AND result=?4",params![job.call_id,job.delegation_id,text,job.result])?;
            }
            tx.execute("UPDATE phone_live_summary_jobs SET lease_token=NULL,next_ms=?3 WHERE call_id=?1 AND delegation_id=?2",params![job.call_id,job.delegation_id,now_ms.saturating_add(1000)])?;
            Ok(true)
        })
    }
}
