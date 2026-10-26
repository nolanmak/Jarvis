//! Durable scheduled occurrence results and evidence-backed reported findings.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoopFinding {
    pub fingerprint: String,
    pub finding: String,
    pub evidence: Vec<String>,
}
#[derive(Debug, Clone)]
pub struct LoopOccurrence {
    pub request_id: String,
    pub loop_id: String,
    pub prompt: String,
    pub answer: Option<String>,
    pub state: String,
    pub error: Option<String>,
    pub updated_at_ms: i64,
}

pub(super) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS loop_occurrences (
        request_id TEXT PRIMARY KEY, loop_id TEXT NOT NULL, prompt TEXT NOT NULL,
        answer TEXT, state TEXT NOT NULL, error TEXT, updated_at_ms INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_loop_occurrence_time ON loop_occurrences(updated_at_ms);
        CREATE TABLE IF NOT EXISTS loop_execution_prompts (request_id TEXT PRIMARY KEY, prompt TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS loop_coverage (
        loop_id TEXT NOT NULL, fingerprint TEXT NOT NULL, finding TEXT NOT NULL,
        evidence TEXT NOT NULL, request_id TEXT NOT NULL, completed_at_ms INTEGER NOT NULL,
        PRIMARY KEY(loop_id,fingerprint));
        CREATE TABLE IF NOT EXISTS loop_occurrence_failures (
        id INTEGER PRIMARY KEY, request_id TEXT NOT NULL, stage TEXT NOT NULL,
        detail TEXT NOT NULL, observed_at_ms INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS idx_loop_failure_retry ON loop_occurrence_failures(request_id,stage,observed_at_ms);",
    )?;
    Ok(())
}
fn occurrence(r: &rusqlite::Row<'_>) -> rusqlite::Result<LoopOccurrence> {
    Ok(LoopOccurrence {
        request_id: r.get(0)?,
        loop_id: r.get(1)?,
        prompt: r.get(2)?,
        answer: r.get(3)?,
        state: r.get(4)?,
        error: r.get(5)?,
        updated_at_ms: r.get(6)?,
    })
}
impl Store {
    pub fn prepare_loop_occurrence(
        &self,
        loop_id: &str,
        request_id: &str,
        prompt: &str,
    ) -> StoreResult<LoopOccurrence> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx=conn.transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO loop_occurrences VALUES (?1,?2,?3,NULL,'pending',NULL,?4)",
            params![request_id, loop_id, redact::mask(prompt), now_millis()],
        )?;
        let row = tx.query_row("SELECT request_id,loop_id,prompt,answer,state,error,updated_at_ms FROM loop_occurrences WHERE request_id=?1",params![request_id],occurrence)?;
        if row.loop_id != loop_id {
            return Err(StoreError::InvalidInput(
                "loop occurrence identity mismatch".into(),
            ));
        }
        // Execution data has the same private-store semantics as user_loops.prompt.
        // Keep it separate from the redacted audit rows exposed to digests/history.
        tx.execute("INSERT OR IGNORE INTO loop_execution_prompts VALUES (?1,?2)",params![request_id,prompt])?;
        tx.commit()?;
        Ok(row)
    }
    pub fn loop_execution_prompt(&self,request_id:&str)->StoreResult<String> {
        let conn=self.conn.lock().expect("store mutex poisoned");
        Ok(conn.query_row("SELECT prompt FROM loop_execution_prompts WHERE request_id=?1",params![request_id],|r|r.get(0))?)
    }
    /// Delivery-only retry delay preserves the answer and occurrence identity.
    /// Exponential delay starts at one minute and caps at one hour, persisted
    /// through the append-only failure journal rather than process memory.
    pub fn loop_delivery_retry_ready(&self,request_id:&str,now_ms:i64)->StoreResult<bool> {
        let conn=self.conn.lock().expect("store mutex poisoned");
        let (count,last):(i64,Option<i64>)=conn.query_row("SELECT COUNT(*),MAX(observed_at_ms) FROM loop_occurrence_failures WHERE request_id=?1 AND stage='delivery'",params![request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let delay=60_000_i64.saturating_mul(1_i64 << count.saturating_sub(1).clamp(0,6)).min(3_600_000);
        Ok(last.is_none_or(|last|now_ms>=last.saturating_add(delay)))
    }
    pub fn save_loop_answer(&self, request_id: &str, answer: &str) -> StoreResult<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let answer = redact::mask(answer);
        let prior: Option<String> = conn.query_row(
            "SELECT answer FROM loop_occurrences WHERE request_id=?1",
            params![request_id],
            |r| r.get(0),
        )?;
        if prior.as_deref().is_some_and(|p| p != answer.as_ref()) {
            return Err(StoreError::InvalidInput(
                "loop occurrence answer is immutable".into(),
            ));
        }
        conn.execute("UPDATE loop_occurrences SET answer=?2,state='answered',updated_at_ms=?3 WHERE request_id=?1 AND state!='delivered'",params![request_id,answer,now_millis()])?;
        Ok(())
    }
    pub fn loop_coverage(&self, loop_id: &str, limit: usize) -> StoreResult<Vec<LoopFinding>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare("SELECT fingerprint,finding,evidence FROM loop_coverage WHERE loop_id=?1 ORDER BY completed_at_ms DESC,fingerprint LIMIT ?2")?;
        let rows = stmt
            .query_map(params![loop_id, limit as i64], |r| {
                let raw: String = r.get(2)?;
                let evidence = serde_json::from_str(&raw).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(LoopFinding {
                    fingerprint: r.get(0)?,
                    finding: r.get(1)?,
                    evidence,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    pub fn loop_finding_covered(&self, loop_id: &str, fingerprint: &str) -> StoreResult<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM loop_coverage WHERE loop_id=?1 AND fingerprint=?2)",
            params![loop_id, fingerprint],
            |r| r.get(0),
        )?)
    }
    /// Finish delivery and advance the schedule in the same transaction. A
    /// restart can never advance the occurrence without persisting coverage.
    pub fn complete_loop_occurrence(
        &self,
        request_id: &str,
        findings: &[LoopFinding],
    ) -> StoreResult<()> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let (loop_id, state): (String, String) = tx.query_row(
            "SELECT loop_id,state FROM loop_occurrences WHERE request_id=?1 AND answer IS NOT NULL",
            params![request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if state == "delivered" {
            return Ok(());
        }
        let now = now_millis();
        for finding in findings {
            if finding.fingerprint.is_empty()
                || finding.finding.trim().is_empty()
                || finding.evidence.is_empty()
                || finding.evidence.iter().any(|e| e.trim().is_empty())
            {
                return Err(StoreError::InvalidInput(
                    "coverage requires concrete findings and evidence".into(),
                ));
            }
            let evidence = serde_json::to_string(&finding.evidence)
                .map_err(|e| StoreError::InvalidInput(e.to_string()))?;
            tx.execute(
                "INSERT OR IGNORE INTO loop_coverage VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    loop_id,
                    finding.fingerprint,
                    redact::mask(&finding.finding),
                    redact::mask(&evidence),
                    request_id,
                    now
                ],
            )?;
        }
        tx.execute("UPDATE loop_occurrences SET state='delivered',error=NULL,updated_at_ms=?2 WHERE request_id=?1",params![request_id,now])?;
        tx.execute("UPDATE user_loops SET last_run_ms=MAX(?2,COALESCE(last_run_ms,0)+1),last_status='ok',fail_count=0,updated_at_ms=?2 WHERE id=?1",params![loop_id,now])?;
        tx.commit()?;
        Ok(())
    }
    pub fn fail_loop_occurrence(
        &self,
        request_id: &str,
        error: &str,
        post_failed: bool,
        pause_at: i64,
    ) -> StoreResult<()> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let loop_id: String = tx.query_row(
            "SELECT loop_id FROM loop_occurrences WHERE request_id=?1",
            params![request_id],
            |r| r.get(0),
        )?;
        let now = now_millis();
        tx.execute("INSERT INTO loop_occurrence_failures(request_id,stage,detail,observed_at_ms) VALUES(?1,?2,?3,?4)",
            params![request_id,if post_failed { "delivery" } else { "execution" },redact::mask(error),now])?;
        tx.execute(
            "UPDATE loop_occurrences SET state=?2,error=?3,updated_at_ms=?4 WHERE request_id=?1",
            params![
                request_id,
                if post_failed { "post_failed" } else { "failed" },
                redact::mask(error),
                now
            ],
        )?;
        tx.execute("UPDATE user_loops SET last_run_ms=CASE WHEN ?4 THEN last_run_ms ELSE MAX(?2,COALESCE(last_run_ms,0)+1) END,last_status=?3,fail_count=fail_count+1,status=CASE WHEN fail_count+1>=?5 THEN 'paused' ELSE status END,updated_at_ms=?2 WHERE id=?1",
            params![loop_id,now,redact::mask(error),post_failed,pause_at])?;
        tx.commit()?;
        Ok(())
    }
    /// Append-only failures survive a later successful delivery of the occurrence.
    pub fn loop_occurrence_failures(
        &self,
        request_id: &str,
    ) -> StoreResult<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare("SELECT stage,detail,observed_at_ms FROM loop_occurrence_failures WHERE request_id=?1 ORDER BY id")?;
        let rows = stmt
            .query_map(params![request_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn loop_recent_occurrences(
        &self,
        loop_id: &str,
        limit: usize,
    ) -> StoreResult<Vec<LoopOccurrence>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare("SELECT request_id,loop_id,prompt,answer,state,error,updated_at_ms FROM loop_occurrences WHERE loop_id=?1 ORDER BY updated_at_ms DESC,request_id LIMIT ?2")?;
        let rows = stmt
            .query_map(params![loop_id, limit as i64], occurrence)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Full audit records; callers bound display context without deleting history.
    pub fn loop_occurrences_since(
        &self,
        since_ms: i64,
        limit: usize,
    ) -> StoreResult<Vec<LoopOccurrence>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare("SELECT request_id,loop_id,prompt,answer,state,error,updated_at_ms FROM loop_occurrences WHERE updated_at_ms>=?1 ORDER BY updated_at_ms DESC,request_id LIMIT ?2")?;
        let rows = stmt
            .query_map(params![since_ms, limit as i64], occurrence)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn execution_prompt_survives_reopen_without_leaking_into_audit() {
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("db");
        let original="Use this literal fixture: {\"password\":\"synthetic-secret-shaped-value\"}";
        {let store=Store::open(&path).unwrap();let audit=store.prepare_loop_occurrence("loop","occurrence",original).unwrap();assert!(!audit.prompt.contains("synthetic-secret-shaped-value"));}
        let store=Store::open(&path).unwrap();
        store.prepare_loop_occurrence("loop","occurrence","changed retry context").unwrap();
        assert_eq!(store.loop_execution_prompt("occurrence").unwrap(),original);
        assert!(!store.loop_occurrences_since(0,10).unwrap()[0].prompt.contains("synthetic-secret-shaped-value"));
    }
    #[test]
    fn delivery_backoff_persists_and_caps_without_advancing_occurrence() {
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("db");
        let store=Store::open(&path).unwrap();
        store.prepare_loop_occurrence("loop","occurrence","prompt").unwrap();
        assert!(store.loop_delivery_retry_ready("occurrence",0).unwrap());
        store.save_loop_answer("occurrence","answer").unwrap();
        for _ in 0..10 {store.fail_loop_occurrence("occurrence","delivery unavailable",true,i64::MAX).unwrap();}
        let last=store.loop_occurrence_failures("occurrence").unwrap().last().unwrap().2;
        drop(store);let store=Store::open(&path).unwrap();
        assert!(!store.loop_delivery_retry_ready("occurrence",last+3_599_999).unwrap());
        assert!(store.loop_delivery_retry_ready("occurrence",last+3_600_000).unwrap());
        assert_eq!(store.prepare_loop_occurrence("loop","occurrence","prompt").unwrap().answer.as_deref(),Some("answer"));
    }
}
