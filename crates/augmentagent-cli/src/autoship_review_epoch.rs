//! Current review authority has a durable owner-input revision boundary.
use super::*;
pub(super) fn invalidate(tx:&Transaction<'_>,workflow:&str,request:&str)->Result<()> {
    let floor:i64=tx.query_row("SELECT COALESCE(max(seq),0) FROM workflow_event WHERE workflow=?1",[workflow],|r|r.get(0))?;
    tx.execute("INSERT INTO workflow_review_floor VALUES(?1,?2,?3) ON CONFLICT(workflow) DO UPDATE SET after_seq=excluded.after_seq,request_id=excluded.request_id",params![workflow,floor,request])?;
    tx.execute("DELETE FROM review WHERE workflow=?1",[workflow])?;
    tx.execute("DELETE FROM review_fragment WHERE workflow=?1",[workflow])?;
    tx.execute("DELETE FROM pr_ready_receipt WHERE workflow=?1",[workflow])?;
    Ok(())
}
impl WorkflowStore {
    /// Newest-first receipts for the exact current binding and review epoch.
    /// Historical events remain available through events(), never as approval.
    pub fn current_review_history(&self,workflow:&str)->Result<Vec<Review>> {
        if self.pending_owner_messages(workflow)?>0 {return Ok(Vec::new());}
        let Some(snapshot)=self.snapshot(workflow)? else {return Ok(Vec::new());};
        let has_floor:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workflow_review_floor')",[],|r|r.get(0))?;
        let floor=if has_floor {self.conn.query_row("SELECT after_seq FROM workflow_review_floor WHERE workflow=?1",[workflow],|r|r.get::<_,i64>(0)).optional()?.unwrap_or(0)} else {0};
        let mut stmt=self.conn.prepare("SELECT kind,payload FROM workflow_event WHERE workflow=?1 AND seq>?2 ORDER BY seq DESC")?;
        let rows=stmt.query_map(params![workflow,floor],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?;
        let mut reviews=Vec::new();
        for row in rows {
            let (kind,payload)=row?;
            if matches!(kind.as_str(),"audit_generation_recovered"|"owner_turn_claimed"|"owner_contract_revised") {break;}
            if kind=="review_recorded" {
                if let Ok(review)=serde_json::from_str::<Review>(&payload) {
                    if review.binding==snapshot.checkpoint.binding {reviews.push(review);}
                }
            }
        }
        Ok(reviews)
    }
}
