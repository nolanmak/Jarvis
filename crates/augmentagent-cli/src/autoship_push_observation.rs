//! Positive observation of an existing Push, without claiming a work turn,
//! dispatching a command, changing its binding or granting another operation.
use super::*;

fn destination_identity(workflow:&str,binding:&Binding,destination:&str,reference:&str)->Result<String> {
    Ok(format!("{:x}",Sha256::digest(serde_json::to_vec(&("bound-push-v1",workflow,OperationKind::Push,binding,destination,reference))?)))
}

/// Protected idle snapshot, not serializable input or an execution grant.
pub(crate) struct PendingPush {
    workflow:String,branch:String,generation:i64,operation:String,binding:Binding,destination:String,reference:String,
    scan_repository:String,scan_token:String,created_at:i64,
}
impl PendingPush {
    pub(crate) fn workflow(&self)->&str {&self.workflow}
    pub(crate) fn head(&self)->&str {&self.binding.head}
    pub(crate) fn destination(&self)->&str {&self.destination}
    pub(crate) fn reference(&self)->&str {&self.reference}
}
impl WorkflowStore {
    /// Only one exact, controller-bound Push may be recovered. Other pending
    /// effects and unknown legacy Push intents retain their existing guards.
    /// Positive remote evidence that this exact head was pushed for the
    /// workflow. Absence includes a push that failed before any dispatch.
    pub(crate) fn pushed_head(&self,workflow:&str,head:&str)->Result<bool> {
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND kind=?2 AND receipt=?3)",
            params![workflow,serde_json::to_string(&OperationKind::Push)?,format!("git-head:{head}")],|r|r.get(0))?)
    }
    /// #2279 — a terminal target's unconfirmed Push intents can never publish.
    /// Settle them only when the remote branch positively shows a different
    /// head than every pending intent, preserving that observation as the
    /// receipt, so terminal reconciliation is not held forever.
    pub(crate) fn settle_unlanded_pushes(&mut self,lease:&Lease,remote_head:&str,evidence:&str,now:i64)->Result<usize> {
        ensure!(matches!(remote_head.len(),40|64) && remote_head.bytes().all(|b|b.is_ascii_hexdigit()),"remote head observation malformed");
        ensure!(!evidence.is_empty() && evidence.len()<=256 && !evidence.bytes().any(|b|b.is_ascii_control()),"bounded terminal evidence required");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx,lease,now)?;
        let pending:Vec<(String,String,String)>={
            let mut stmt=tx.prepare("SELECT id,kind,binding FROM operation WHERE workflow=?1 AND receipt IS NULL")?;
            let rows=stmt.query_map([&lease.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let push=serde_json::to_string(&OperationKind::Push)?;
        for (_,kind,binding) in &pending {
            ensure!(*kind==push,"terminal reconciliation refuses non-Push pending operations");
            let binding:Binding=serde_json::from_str(binding)?;
            ensure!(!binding.head.eq_ignore_ascii_case(remote_head),"pending Push head is present remotely; observe it instead");
        }
        let receipt=format!("terminal-target-unlanded:{evidence}:remote-head:{}",remote_head.to_ascii_lowercase());
        for (id,_,_) in &pending {
            tx.execute("UPDATE operation SET receipt=?2 WHERE id=?1 AND receipt IS NULL",params![id,receipt])?;
        }
        if !pending.is_empty() {
            event(&tx,&lease.workflow,lease.generation,"pending_push_settled_terminal_target",
                &serde_json::json!({"operations":pending.iter().map(|(id,_,_)|id).collect::<Vec<_>>(),"receipt":receipt}).to_string(),now)?;
        }
        tx.commit()?;Ok(pending.len())
    }
    pub(crate) fn retryable_bound_push(&self,workflow:&str)->Result<bool> {
        let pending=self.pending_operations(workflow)?;
        let [operation]=pending.as_slice() else {return Ok(false)};
        if operation.kind!=OperationKind::Push || self.merge_reconciliation_needed(workflow)? {return Ok(false);}
        let Some(snapshot)=self.snapshot(workflow)? else {return Ok(false)};
        let saved:Option<(String,String)>=self.conn.query_row(
            "SELECT destination,reference FROM push_dispatch_destination WHERE operation=?1",[&operation.id],
            |row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let Some((destination,reference))=saved else {return Ok(false)};
        Ok(snapshot.checkpoint.binding==operation.binding && reference==format!("refs/heads/{}",snapshot.branch)
            && destination_identity(workflow,&operation.binding,&destination,&reference)?==operation.id)
    }

    /// The exact dispatch destination is immutable evidence, committed with
    /// the intent. Unknown earlier Push effects cannot be relabelled or evaded.
    pub(crate) fn begin_bound_push(&mut self,lease:&Lease,binding:&Binding,destination:&str,reference:&str,now:i64)->Result<Operation> {
        ensure!(!destination.is_empty() && destination.len()<=16384
            && !destination.bytes().any(|b|b.is_ascii_control()),"invalid Push destination envelope");
        ensure!(reference.starts_with("refs/heads/") && reference.len()>11 && reference.len()<=2048
            && !reference.bytes().any(|b|b.is_ascii_control()),"invalid Push reference envelope");
        let kind=OperationKind::Push;
        let id=destination_identity(&lease.workflow,binding,destination,reference)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        let branch:String=tx.query_row("SELECT branch FROM workflow WHERE id=?1",[&lease.workflow],|r|r.get(0))?;
        ensure!(reference==format!("refs/heads/{branch}"),"Push reference differs from current workflow branch");
        owner_control::require_dispatch(&tx,&lease.workflow)?;
        family_budget::control::require_dispatch(&tx,lease)?;
        qualification_recovery::require_mutation_allowed(&tx,&lease.workflow)?;
        let other:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND kind=?2 AND receipt IS NULL AND id<>?3)",
            params![lease.workflow,serde_json::to_string(&kind)?,id],|r|r.get(0))?;
        ensure!(!other,"original uncertain Push must be reconciled before another destination or binding");
        let inserted=tx.execute("INSERT INTO operation(id,workflow,kind,binding,created_at) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO NOTHING",
            params![id,lease.workflow,serde_json::to_string(&kind)?,serde_json::to_string(binding)?,now])?;
        if inserted==1 {
            tx.execute("INSERT INTO push_dispatch_destination(operation,destination,reference) VALUES(?1,?2,?3)",params![id,destination,reference])?;
        }else {
            let original:Option<(String,String)>=tx.query_row("SELECT destination,reference FROM push_dispatch_destination WHERE operation=?1",[&id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            ensure!(original.as_ref().is_some_and(|(url,refname)|url==destination && refname==reference),"Push destination evidence missing or changed");
        }
        let (receipt,original):(Option<String>,String)=tx.query_row("SELECT receipt,binding FROM operation WHERE id=?1 AND workflow=?2",params![id,lease.workflow],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let original:Binding=serde_json::from_str(&original)?;
        ensure!(original==*binding,"Push binding changed");
        tx.commit()?;
        Ok(Operation {id,workflow:lease.workflow.clone(),kind,binding:original,receipt})
    }

    pub(crate) fn pending_push_observations(&mut self,repository:&str,now:i64,limit:usize)->Result<Vec<PendingPush>> {
        ensure!((1..=64).contains(&limit),"pending Push observation batch exceeds bound");
        let repository=repository.to_ascii_lowercase();workflow_id(&repository,1)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Derived scheduling state only: it never claims a workflow or alters
        // its original accounting, checkpoint, controls or uncertain effects.
        tx.execute_batch("CREATE TABLE IF NOT EXISTS pending_push_observation_cursor(repository TEXT PRIMARY KEY,created_at INTEGER NOT NULL,operation TEXT NOT NULL);")?;
        // Older development stores may predate the scan token. It fences
        // late failure receipts from concurrent scans, not execution authority.
        let has_token={
            let mut columns=tx.prepare("PRAGMA table_info(pending_push_observation_cursor)")?;
            let names=columns.query_map([],|row|row.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
            names.iter().any(|name|name=="scan_token")
        };
        if !has_token {tx.execute_batch("ALTER TABLE pending_push_observation_cursor ADD COLUMN scan_token TEXT NOT NULL DEFAULT '';")?;}
        let scan_token:String=tx.query_row("SELECT lower(hex(randomblob(16)))",[],|row|row.get(0))?;
        let after:Option<(i64,String)>=tx.query_row("SELECT created_at,operation FROM pending_push_observation_cursor WHERE repository=?1",[&repository],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let (after_time,after_id)=after.unwrap_or((i64::MIN,String::new()));
        let mut statement=tx.prepare("SELECT w.id,w.branch,w.generation,w.checkpoint,o.id,o.binding,o.created_at,d.destination,d.reference FROM workflow w JOIN operation o ON o.workflow=w.id LEFT JOIN push_dispatch_destination d ON d.operation=o.id
            WHERE o.kind=?1 AND o.receipt IS NULL AND w.lease_until<=?2
            AND (instr(w.id,?3||'#')=1 OR instr(w.id,?3||'/pr/')=1)
            ORDER BY CASE WHEN o.created_at>?4 OR (o.created_at=?4 AND o.id>?5) THEN 0 ELSE 1 END,o.created_at,o.id LIMIT ?6")?;
        let rows=statement.query_map(params![serde_json::to_string(&OperationKind::Push)?,now,repository,after_time,after_id,limit as i64],|row|
            Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,i64>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?,row.get::<_,i64>(6)?,row.get::<_,Option<String>>(7)?,row.get::<_,Option<String>>(8)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        if let Some((_,_,_,_,operation,_,created,_,_))=rows.last() {
            tx.execute("INSERT INTO pending_push_observation_cursor(repository,created_at,operation,scan_token) VALUES(?1,?2,?3,?4)
                ON CONFLICT(repository) DO UPDATE SET created_at=excluded.created_at,operation=excluded.operation,scan_token=excluded.scan_token",params![repository,created,operation,scan_token])?;
        }
        let mut result=Vec::new();
        for (workflow,branch,generation,checkpoint,operation,binding,created_at,destination,reference) in rows {
            let (Some(destination),Some(reference))=(destination,reference) else {continue};
            let (Ok(checkpoint),Ok(binding))=(serde_json::from_str::<Checkpoint>(&checkpoint),serde_json::from_str::<Binding>(&binding)) else {
                tracing::warn!(workflow,"pending Push metadata unavailable; original effect retained");continue;
            };
            if destination_identity(&workflow,&binding,&destination,&reference)?!=operation {continue;}
            if binding.validate().is_err() || checkpoint.binding!=binding || matches!(checkpoint.stage,Stage::Merged|Stage::Retired) {continue;}
            let controls=owner_control::read(&tx,&workflow);
            let inbox=owner_inbox::pending(&tx,&workflow);
            let (Ok(controls),Ok(inbox))=(controls,inbox) else {continue;};
            if controls.is_some_and(|control|control.paused) || inbox>0 {continue;}
            result.push(PendingPush {workflow,branch,generation,operation,binding,destination,reference,scan_repository:repository.clone(),scan_token:scan_token.clone(),created_at});
        }
        tx.commit()?;Ok(result)
    }

    /// A service failure stops remote reads for this batch. Resume the next
    /// batch after the failed attempt, rather than after the unattempted tail.
    /// A newer scan owns its own cursor and cannot be rewound by an old reader.
    pub(crate) fn defer_push_observation_after(&mut self,pending:&PendingPush)->Result<()> {
        self.conn.execute("UPDATE pending_push_observation_cursor SET created_at=?2,operation=?3 WHERE repository=?1 AND scan_token=?4",
            params![pending.scan_repository,pending.created_at,pending.operation,pending.scan_token])?;
        Ok(())
    }

    /// A controller observed the exact original head on the exact remote ref.
    /// An immediate transaction fences that read against a changed generation,
    /// active lease, branch, binding or owner control. No turn is claimed and
    /// no original attempt, charge, checkpoint or uncertain peer effect changes.
    pub(crate) fn observe_idle_push(&mut self,pending:&PendingPush,remote_head:&str,now:i64)->Result<bool> {
        if remote_head!=pending.binding.head || destination_identity(&pending.workflow,&pending.binding,&pending.destination,&pending.reference)?!=pending.operation {return Ok(false);}
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current:Option<(String,i64,i64,String)>=tx.query_row("SELECT branch,generation,lease_until,checkpoint FROM workflow WHERE id=?1",[&pending.workflow],|row|
            Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
        let Some((branch,generation,until,checkpoint))=current else {return Ok(false)};
        let checkpoint:Checkpoint=serde_json::from_str(&checkpoint)?;
        if branch!=pending.branch || generation!=pending.generation || until>now || checkpoint.binding!=pending.binding
            || matches!(checkpoint.stage,Stage::Merged|Stage::Retired)
            || owner_control::read(&tx,&pending.workflow)?.is_some_and(|control|control.paused)
            || owner_inbox::pending(&tx,&pending.workflow)?>0 {return Ok(false);}
        let operation:Option<(String,String,Option<String>)>=tx.query_row("SELECT o.kind,o.binding,o.receipt FROM operation o JOIN push_dispatch_destination d ON d.operation=o.id WHERE o.id=?1 AND o.workflow=?2 AND d.destination=?3 AND d.reference=?4",
            params![pending.operation,pending.workflow,pending.destination,pending.reference],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
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
    fn destination_and_intent_publish_atomically_and_preserve_unknown_legacy_effects() {
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id="owner/repo#7";let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(id,"fixture",binding.clone(),1).unwrap();let lease=store.claim(id,"original",2,100).unwrap().unwrap();
        let before=store.snapshot(id).unwrap();let events=store.events(id).unwrap();
        store.conn.execute_batch("CREATE TRIGGER refuse_destination BEFORE INSERT ON push_dispatch_destination BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
        assert!(store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git","refs/heads/fixture",3).is_err());
        assert!(store.pending_operations(id).unwrap().is_empty(),"failed destination publication left an unbound new Push");
        assert_eq!(store.snapshot(id).unwrap(),before);assert_eq!(store.events(id).unwrap(),events);
        store.conn.execute_batch("DROP TRIGGER refuse_destination;").unwrap();
        let original=store.begin_operation(&lease,OperationKind::Push,&binding,3).unwrap();
        assert!(store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git","refs/heads/fixture",4).is_err(),"unknown legacy Push was evaded by another intent");
        assert_eq!(store.pending_operations(id).unwrap().len(),1);
        store.complete_operation(&lease,&original.id,"git-head:legacy",5).unwrap();
        let bound=store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git","refs/heads/fixture",6).unwrap();
        assert_ne!(bound.id,original.id,"completed legacy evidence was relabelled");
        assert_eq!(store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git","refs/heads/fixture",7).unwrap().id,bound.id);
        assert!(store.begin_bound_push(&lease,&binding,"https://github.com/owner/other.git","refs/heads/fixture",8).is_err());
        store.release(&lease,9).unwrap();
        let observations=store.pending_push_observations("owner/repo",10,32).unwrap();
        assert_eq!(observations.len(),1);assert_eq!(observations[0].destination(),"https://github.com/owner/repo.git");
        assert_eq!(store.conn.query_row("SELECT receipt FROM operation WHERE id=?1",[original.id],|r|r.get::<_,String>(0)).unwrap(),"git-head:legacy");
        store.conn.execute("UPDATE push_dispatch_destination SET destination=?2 WHERE operation=?1",params![bound.id,"https://github.com/owner/other.git"]).unwrap();
        assert!(store.pending_push_observations("owner/repo",11,32).unwrap().is_empty(),"changed destination no longer matches its immutable operation identity");
    }

    #[test]
    fn bounded_observation_advances_past_retained_rows_without_claiming_work() {
        for paused in [true,false] {
            let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
            let mut before=Vec::new();
            for issue in 1..=33 {
                let id=format!("owner/repo#{issue}");let binding=Binding {head:if issue==33 {"a".repeat(40)}else{"b".repeat(40)},contract:"contract".into(),evidence_hash:"evidence".into()};
                store.register(&id,&format!("fixture-{issue}"),binding.clone(),1).unwrap();let lease=store.claim(&id,"original",2,100).unwrap().unwrap();
                store.charge(&lease,"past-work",ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},3).unwrap();
                store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git",&format!("refs/heads/fixture-{issue}"),issue).unwrap();store.release(&lease,100).unwrap();
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
    fn older_failed_scan_cannot_rewind_a_newer_scan_cursor() {
        let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
        for issue in 1..=3 {
            let id=format!("owner/repo#{issue}");let branch=format!("fixture-{issue}");
            let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
            store.register(&id,&branch,binding.clone(),1).unwrap();let lease=store.claim(&id,"original",2,100).unwrap().unwrap();
            store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git",&format!("refs/heads/{branch}"),issue+3).unwrap();store.release(&lease,10).unwrap();
        }
        let old=store.pending_push_observations("owner/repo",200,2).unwrap();
        let current=store.pending_push_observations("owner/repo",200,2).unwrap();
        let cursor=|store:&WorkflowStore|store.conn.query_row("SELECT created_at,operation,scan_token FROM pending_push_observation_cursor WHERE repository='owner/repo'",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).unwrap();
        let before=cursor(&store);store.defer_push_observation_after(&old[1]).unwrap();
        assert_eq!(cursor(&store),before,"stale remote failure rewound newer scan progress");
        store.defer_push_observation_after(&current[0]).unwrap();
        assert_eq!(cursor(&store).1,current[0].operation,"current failed attempt did not preserve its successor's turn");
    }

    #[test]
    fn positive_push_observation_is_idle_fenced_and_never_claims_or_spends_a_turn() {
        for fault in ["none","foreign-head","binding","generation","active","branch","pause","terminal","destination","reference","foreign-repo"] {
            let root=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&root.path().join("workflow.db")).unwrap();
            let id="owner/repo#7";let binding=Binding {head:"a".repeat(40),contract:"contract".into(),evidence_hash:"evidence".into()};
            store.register(id,"fixture",binding.clone(),1).unwrap();let lease=store.claim(id,"original",2,100).unwrap().unwrap();
            store.charge(&lease,"past-work",ResourceCharge {model_calls:9,tokens:1234,wall_ms:700},3).unwrap();
            let operation=store.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git","refs/heads/fixture",4).unwrap();
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
            if fault=="reference" {store.conn.execute("UPDATE push_dispatch_destination SET reference=?2 WHERE operation=?1",params![pending.operation,"refs/heads/another"]).unwrap();}
            if fault=="destination" {store.conn.execute("UPDATE push_dispatch_destination SET destination=?2 WHERE operation=?1",params![pending.operation,"https://github.com/owner/other.git"]).unwrap();}
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
