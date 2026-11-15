//! Positive observation of an existing Push, without claiming a work turn,
//! dispatching a command, changing its binding or granting another operation.
use super::*;

/// Protected idle snapshot, not serializable input or an execution grant.
pub(crate) struct PendingPush {
    workflow:String,branch:String,generation:i64,operation:String,binding:Binding,
}
impl PendingPush {
    pub(crate) fn workflow(&self)->&str {&self.workflow}
    pub(crate) fn branch(&self)->&str {&self.branch}
    pub(crate) fn head(&self)->&str {&self.binding.head}
}
impl WorkflowStore {
    pub(crate) fn pending_push_observations(&mut self,repository:&str,now:i64,limit:usize)->Result<Vec<PendingPush>> {
        ensure!((1..=64).contains(&limit),"pending Push observation batch exceeds bound");
        let repository=repository.to_ascii_lowercase();workflow_id(&repository,1)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Derived scheduling state only: it never claims a workflow or alters
        // its original accounting, checkpoint, controls or uncertain effects.
        tx.execute_batch("CREATE TABLE IF NOT EXISTS pending_push_observation_cursor(repository TEXT PRIMARY KEY,created_at INTEGER NOT NULL,operation TEXT NOT NULL);")?;
        let after:Option<(i64,String)>=tx.query_row("SELECT created_at,operation FROM pending_push_observation_cursor WHERE repository=?1",[&repository],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let (after_time,after_id)=after.unwrap_or((i64::MIN,String::new()));
        let mut statement=tx.prepare("SELECT w.id,w.branch,w.generation,w.checkpoint,o.id,o.binding,o.created_at FROM workflow w JOIN operation o ON o.workflow=w.id
            WHERE o.kind=?1 AND o.receipt IS NULL AND w.lease_until<=?2
            AND (instr(w.id,?3||'#')=1 OR instr(w.id,?3||'/pr/')=1)
            ORDER BY CASE WHEN o.created_at>?4 OR (o.created_at=?4 AND o.id>?5) THEN 0 ELSE 1 END,o.created_at,o.id LIMIT ?6")?;
        let rows=statement.query_map(params![serde_json::to_string(&OperationKind::Push)?,now,repository,after_time,after_id,limit as i64],|row|
            Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?,row.get::<_,i64>(6)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if let Some((_,_,_,_,operation,_,created))=rows.last() {
            tx.execute("INSERT INTO pending_push_observation_cursor(repository,created_at,operation) VALUES(?1,?2,?3)
                ON CONFLICT(repository) DO UPDATE SET created_at=excluded.created_at,operation=excluded.operation",params![repository,created,operation])?;
        }
        let mut result=Vec::new();
        for (workflow,branch,generation,checkpoint,operation,binding,_) in rows {
            let (Ok(checkpoint),Ok(binding))=(serde_json::from_str::<Checkpoint>(&checkpoint),serde_json::from_str::<Binding>(&binding)) else {
                tracing::warn!(workflow,"pending Push metadata unavailable; original effect retained");continue;
            };
            if binding.validate().is_err() || checkpoint.binding!=binding || matches!(checkpoint.stage,Stage::Merged|Stage::Retired) {continue;}
            let controls=owner_control::read(&tx,&workflow);
            let inbox=owner_inbox::pending(&tx,&workflow);
            let (Ok(controls),Ok(inbox))=(controls,inbox) else {continue;};
            if controls.is_some_and(|control|control.paused) || inbox>0 {continue;}
            result.push(PendingPush {workflow,branch,generation,operation,binding});
        }
        tx.commit()?;Ok(result)
    }

    /// A controller observed the exact original head on the exact remote ref.
    /// An immediate transaction fences that read against a changed generation,
    /// active lease, branch, binding or owner control. No turn is claimed and
    /// no original attempt, charge, checkpoint or uncertain peer effect changes.
    pub(crate) fn observe_idle_push(&mut self,pending:&PendingPush,remote_head:&str,now:i64)->Result<bool> {
        if remote_head!=pending.binding.head {return Ok(false);}
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current:Option<(String,i64,i64,String)>=tx.query_row("SELECT branch,generation,lease_until,checkpoint FROM workflow WHERE id=?1",[&pending.workflow],|row|
            Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        let Some((branch,generation,until,checkpoint))=current else {return Ok(false)};
        let checkpoint:Checkpoint=serde_json::from_str(&checkpoint)?;
        if branch!=pending.branch || generation!=pending.generation || until>now || checkpoint.binding!=pending.binding
            || matches!(checkpoint.stage,Stage::Merged|Stage::Retired)
            || owner_control::read(&tx,&pending.workflow)?.is_some_and(|control|control.paused)
            || owner_inbox::pending(&tx,&pending.workflow)?>0 {return Ok(false);}
        let operation:Option<(String,String,Option<String>)>=tx.query_row("SELECT kind,binding,receipt FROM operation WHERE id=?1 AND workflow=?2",
            params![pending.operation,pending.workflow],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        let Some((kind,binding,receipt))=operation else {return Ok(false)};
        if serde_json::from_str::<OperationKind>(&kind)?!=OperationKind::Push || serde_json::from_str::<Binding>(&binding)?!=pending.binding || receipt.is_some() {return Ok(false);}
        tx.execute("UPDATE operation SET receipt=?2 WHERE id=?1 AND receipt IS NULL",params![pending.operation,format!("git-head:{remote_head}")])?;
        event(&tx,&pending.workflow,generation,"operation_completed",&pending.operation,now)?;
        tx.commit()?;Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_observation_advances_past_retained_rows_without_claiming_work() {
        for paused in [true,false] {
            let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
            let mut before=Vec::new();
            for issue in 1..=33 {
                let id=format!("owner/repo#{issue}");let binding=Binding {head:if issue==33 {"a".repeat(40)}else{"b".repeat(40)},contract:"contract".into(),evidence_hash:"evidence".into()};
                store.register(&id,&format!("fixture-{issue}"),binding.clone(),1).unwrap();let lease=store.claim(&id,"original",2,100).unwrap().unwrap();
                store.charge(&lease,"past-work",ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},3).unwrap();
                store.begin_operation(&lease,OperationKind::Push,&binding,issue).unwrap();store.release(&lease,100).unwrap();
                if paused && issue<33 {store.set_owner_pause(&id,"github:owner",0,true,101).unwrap();}
                before.push((id.clone(),store.snapshot(&id).unwrap(),store.events(&id).unwrap()));
            }
            let mut found=false;
            for _ in 0..3 {
                let rows=store.pending_push_observations("owner/repo",200,32).unwrap();assert!(rows.len()<=32);
                found|=rows.iter().any(|row|row.workflow()=="owner/repo#33" && row.head()=="a".repeat(40));
            }
            assert!(found,"32 retained {} rows starved a later observable Push",if paused {"paused"}else{"mismatching-head"});
            for (id,snapshot,events) in before {assert_eq!(store.snapshot(&id).unwrap(),snapshot);assert_eq!(store.events(&id).unwrap(),events);}
        }
    }

    #[test]
    fn positive_push_observation_is_idle_fenced_and_never_claims_or_spends_a_turn() {
        for fault in ["none","foreign-head","binding","generation","active","branch","pause","terminal","foreign-repo"] {
            let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
            let id="owner/repo#7";let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
            store.register(id,"fixture",binding.clone(),1).unwrap();let lease=store.claim(id,"original",2,100).unwrap().unwrap();
            store.charge(&lease,"past-work",ResourceCharge {model_calls:9,tokens:1234,wall_ms:700},3).unwrap();
            let operation=store.begin_operation(&lease,OperationKind::Push,&binding,4).unwrap();
            let peer=store.begin_operation(&lease,OperationKind::MergePr,&binding,5).unwrap();
            store.release(&lease,6).unwrap();
            if fault=="foreign-repo" {assert!(store.pending_push_observations("foreign/repo",7,32).unwrap().is_empty());continue;}
            let pending=store.pending_push_observations("owner/repo",7,32).unwrap().pop().unwrap();
            if fault=="generation" {
                let lease=store.claim(id,"next",8,100).unwrap().unwrap();store.release(&lease,10).unwrap();
            }
            // Isolate each protected-fact fence in this disposable database;
            // changing generation too would mask a missing branch/binding check.
            if matches!(fault,"binding"|"terminal") {
                let mut checkpoint=store.snapshot(id).unwrap().unwrap().checkpoint;
                if fault=="binding" {checkpoint.binding.head="b".repeat(40);}else {checkpoint.stage=Stage::Retired;}
                store.conn.execute("UPDATE workflow SET checkpoint=?2 WHERE id=?1",params![id,serde_json::to_string(&checkpoint).unwrap()]).unwrap();
            }
            if fault=="branch" {store.conn.execute("UPDATE workflow SET branch='foreign' WHERE id=?1",[id]).unwrap();}
            if fault=="active" {store.conn.execute("UPDATE workflow SET lease_until=1000 WHERE id=?1",[id]).unwrap();}
            if fault=="pause" {store.set_owner_pause(id,"github:owner",0,true,8).unwrap();}
            let before=store.snapshot(id).unwrap();let events=store.events(id).unwrap();
            let observed=store.observe_idle_push(&pending,if fault=="foreign-head" {"foreign"}else{&binding.head},11).unwrap();
            assert_eq!(observed,fault=="none","{fault}");
            assert_eq!(store.snapshot(id).unwrap(),before,"{fault}: observation changed original workflow or accounting");
            assert!(store.pending_operations(id).unwrap().iter().any(|operation|operation.id==peer.id),"{fault}: observation erased an uncertain merge");
            if observed {
                let after=store.events(id).unwrap();assert_eq!(after.len(),events.len()+1);
                assert!(!store.pending_operations(id).unwrap().iter().any(|row|row.id==operation.id));
                assert!(!store.observe_idle_push(&pending,&binding.head,12).unwrap());assert_eq!(store.events(id).unwrap(),after);
                assert_eq!(store.conn.query_row("SELECT COUNT(*) FROM operation WHERE workflow=?1",[id],|row|row.get::<_,i64>(0)).unwrap(),2,"observation fabricated another operation");
            }else {assert_eq!(store.events(id).unwrap(),events,"{fault}: failed observation left a receipt/event");}
        }
    }
}
