//! Recovery intent durability only. No core transfer, execution lease or body.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{PreparationScan,TransferReason as R};

/// Current-authority durability receipt. No raw request/lease getter, Clone,
/// serialization or conversion into a core transfer intent is provided.
pub(crate) struct RecoveryNativeIntent<'a> {pending:PendingNativeHandoff,_claim:&'a RecoveryLease}
impl RecoveryNativeIntent<'_> {
    pub(crate) fn transfer_id(&self)->&str {&self.pending.request().transfer_id}
    pub(crate) fn generation(&self)->i64 {self.pending.request().destination.owner.lease_generation}
}
pub(crate) enum RecoveryNativeIntentPersistence<'a> {
    Durable(RecoveryNativeIntent<'a>),
    NotCommitted {reason:R},
    Unknown {reason:R,attempted_transfer:Option<String>},
}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRecord {
    version:u32,request_id:String,intent:super::super::owner_publication::Reference,
    claim:RecordRef,generation:i64,completion:RecordRef,pending:RecordRef,created_at:i64,
}
fn original_record(conn:&Connection,original:&Original)->Result<(crate::self_improve::OwnerPublicationRecord,String)> {
    let (raw,len):(String,i64)=conn.query_row("SELECT substr(record,1,4227073),length(CAST(record AS BLOB)) FROM workflow_owner_publication_intent WHERE workflow=?1 AND request_id=?2",params![original.workflow,original.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    ensure!((1..=4227072).contains(&len) && raw.len() as i64==len,"recovery native intent original exceeds bound");
    super::super::owner_publication::require(conn,&original.request_id,&original.intent,&raw)?;
    let record:crate::self_improve::OwnerPublicationRecord=serde_json::from_str(&raw)?;
    record.validate_history(conn)?;Ok((record,raw))
}
fn completed_reference(conn:&Connection,original:&Original)->Result<RecordRef> {
    let (record,raw)=original_record(conn,original)?;
    ensure!(super::super::owner_publication::completion(conn,&original.workflow,&original.request_id,&record,&raw)?.is_some(),
        "forward native intent requires original completed publication");
    let (sequence,sha256):(i64,String)=conn.query_row("SELECT sequence,record_sha256 FROM workflow_owner_publication_completion WHERE workflow=?1 AND request_id=?2",params![original.workflow,original.request_id],|r|Ok((r.get(0)?,r.get(1)?)))?;
    Ok(RecordRef {sequence,sha256})
}
fn history(conn:&Connection,workflow:&str)->Result<Vec<(ContextRecord,PendingNativeHandoff)>> {
    let indexed:i64=conn.query_row("SELECT count(*) FROM workflow_owner_recovery_native_intent WHERE workflow=?1",[workflow],|r|r.get(0))?;
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_recovery_native_intent_recorded'",[workflow],|r|r.get(0))?;
    ensure!(indexed==events && indexed<=1024,"recovery native context history index missing or oversized");
    if indexed==0 {return Ok(Vec::new());}
    let claims=journal(conn,workflow)?;
    let mut query=conn.prepare("SELECT request_id,transfer_id,generation,pending_sequence,sequence,record_sha256,substr(record,1,16385),length(CAST(record AS BLOB)) FROM workflow_owner_recovery_native_intent WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let rows=query.query_map([workflow],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,i64>(7)?)))?;
    let mut result=Vec::new();
    for row in rows {
        let (request,id,generation,pending_sequence,sequence,sha,raw,len)=row?;
        ensure!(result.len()<1024 && (1..=16384).contains(&len) && raw.len() as i64==len && digest(&raw)==sha,"recovery native context exceeds bound or changed");
        let context:ContextRecord=serde_json::from_str(&raw)?;
        let index=claims.iter().position(|(_,reference)|*reference==context.claim).context("recovery native context claim missing")?;
        let claim=&claims[index].0;let original=&claim.original;
        ensure!(context.version==1 && serde_json::to_string(&context)?==raw && context.request_id==request && request==original.request_id
            && context.intent==original.intent && generation==context.generation && generation==claim.generation
            && context.pending.sequence==pending_sequence && context.claim.sequence<pending_sequence && pending_sequence<sequence
            && claims.get(index+1).is_none_or(|(_,next)|sequence<next.sequence)
            && context.created_at>=claim.claimed_at && context.created_at<claim.lease_until
            && context.completion==completed_reference(conn,original)? && context.completion.sequence<pending_sequence,
            "recovery native context lineage changed");
        let pending=native_pending::recorded_pending(conn,workflow,&id)?;
        ensure!(pending.request().destination.owner.lease_generation==generation
            && pending.request().source_owners.len()==1 && pending.request().source_owners[0].owner.lease_generation==original.activated_generation,
            "recovery native context original namespace changed");
        let source=&pending.request().source_owners[0];
        let (audit,binding):(String,String)=conn.query_row("SELECT substr(o.audit_path,1,4097),substr(b.binding,1,16385) FROM workflow_native_owner o JOIN workflow_native_owner_binding b ON b.workflow=o.workflow AND b.lease_generation=o.lease_generation WHERE o.workflow=?1 AND o.lease_generation=?2",params![workflow,original.activated_generation],|r|Ok((r.get(0)?,r.get(1)?)))?;
        ensure!(audit.len()<=4096 && binding.len()<=16384 && source.audit_path==audit && serde_json::from_str::<augmentagent_channel_core::native_vm::ownership::TransferBinding>(&binding)?==*pending.original_binding(),"recovery native context source registration changed");
        let (record,_)=original_record(conn,original)?;record.validate_native_binding(pending.original_binding())?;
        ensure!(pending.workflow_binding()==record.recovery_binding(),"recovery native context workflow binding changed");
        let (kind,event_generation,payload,at):(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,pending_sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(kind=="native_handoff_prepared" && event_generation==generation && at==context.created_at
            && serde_json::from_str::<serde_json::Value>(&payload)?==serde_json::json!({"transfer_id":id,"record_sha256":context.pending.sha256}),"recovery native pending reference changed");
        let pending_sha=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(&pending)?)?));
        ensure!(pending_sha==context.pending.sha256,"recovery native pending digest changed");
        let (kind,event_generation,payload,at):(String,i64,String,i64)=conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(kind=="owner_recovery_native_intent_recorded" && event_generation==generation && at==context.created_at
            && serde_json::from_str::<serde_json::Value>(&payload)?==serde_json::json!({"version":1,"request_id":request,"transfer_id":id,"record_sha256":sha}),"recovery native context event changed");
        result.push((context,pending));
    }
    Ok(result)
}
fn read(conn:&Connection,lease:&RecoveryLease,authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64)->Result<Option<PendingNativeHandoff>> {
    require_authorized_claim(conn,lease,authorization,now)?;
    let contexts=history(conn,&lease.claim.original.workflow)?;
    let selected=contexts.into_iter().find(|(context,_)|context.request_id==lease.request_id());
    let active:Option<String>=conn.query_row("SELECT transfer_id FROM workflow_native_handoff_pending WHERE workflow=?1",[&lease.claim.original.workflow],|r|r.get(0)).optional()?;
    ensure!(active.as_ref().is_none_or(|id|selected.as_ref().is_some_and(|(_,pending)|pending.request().transfer_id==*id)),"another native intent owns original recovery; reconcile it first");
    ensure!(!native_pending::orphaned(conn,&lease.claim.original.workflow)?,"native intent persistence is unknown");
    Ok(selected.map(|(_,pending)|pending))
}
impl WorkflowStore {
    pub(crate) fn read_owner_recovery_native_intent<'a>(&mut self,lease:&'a RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64)->Result<Option<RecoveryNativeIntent<'a>>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let pending=read(&tx,lease,authorization,now)?;tx.commit()?;
        Ok(pending.map(|pending|RecoveryNativeIntent {pending,_claim:lease}))
    }
    pub(crate) fn prepare_owner_recovery_native_intent<'a>(&mut self,lease:&'a RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,basis:&super::super::owner_publication::OwnerPublicationNativeFacts,
        scan:&PreparationScan<'_>,now:i64)->RecoveryNativeIntentPersistence<'a> {
        let mut attempted=None;
        let reported=self.write_owner_recovery_native_intent(lease,authorization,basis,scan,now,&mut attempted);
        let expected=reported.as_ref().ok().cloned().or_else(||attempted.as_ref().map(|pending:&PendingNativeHandoff|pending.request().transfer_id.clone()));
        let reason=reported.as_ref().err().and_then(|error|error.downcast_ref::<R>()).copied().unwrap_or(R::PersistRetry);
        let inspected=self.read_owner_recovery_native_intent(lease,authorization,chrono::Utc::now().timestamp_millis());
        match inspected {
            Ok(Some(receipt)) if expected.as_ref()==Some(&receipt.pending.request().transfer_id)=>RecoveryNativeIntentPersistence::Durable(receipt),
            Ok(None) if reported.is_err() && attempted.is_none()=>RecoveryNativeIntentPersistence::NotCommitted {reason},
            _=>RecoveryNativeIntentPersistence::Unknown {reason,attempted_transfer:expected},
        }
    }
    fn write_owner_recovery_native_intent(&mut self,lease:&RecoveryLease,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,basis:&super::super::owner_publication::OwnerPublicationNativeFacts,
        scan:&PreparationScan<'_>,now:i64,attempted:&mut Option<PendingNativeHandoff>)->Result<String> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(pending)=read(&tx,lease,authorization,now)? {
            ensure!(scan.binding()==pending.original_binding() && scan.source_owners()==pending.request().source_owners
                && scan.destination()==&pending.request().destination,"existing recovery native intent cannot be retargeted");
            let id=pending.request().transfer_id.clone();tx.commit()?;return Ok(id);
        }
        let completion=completed_reference(&tx,&lease.claim.original)?;
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed')) OR EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![lease.claim.original.workflow,lease.generation()],|r|r.get(0))?;
        ensure!(!blocked,"recovery native intent has occupied destination or unsettled remote mutation");
        let snapshot=capture_recovery_snapshot_tx(&tx,lease,authorization,basis,scan,now)?;
        ensure!(snapshot.observation.certain,R::ConversationUncertain);
        let request=augmentagent_channel_core::native_vm::ownership::TransferRequest {version:1,transfer_id:uuid::Uuid::new_v4().to_string(),workflow_id:lease.claim.original.workflow.clone(),
            source_owners:snapshot.source_owners,inventory:snapshot.inventory,destination:scan.destination().clone()};request.validate()?;
        let pending=PendingNativeHandoff {version:1,request,original_binding:snapshot.original_binding,workflow_binding:basis.snapshot_facts().workflow_binding.clone(),
            observation_revision:snapshot.observation.revision,observation_sha256:snapshot.observation.sha256,observation_certain:true};
        let (sequence,sha256)=native_pending::append_pending(&tx,&pending,now)?;
        let context=ContextRecord {version:1,request_id:lease.request_id().into(),intent:lease.claim.original.intent.clone(),claim:lease.reference.clone(),generation:lease.generation(),
            completion,pending:RecordRef {sequence,sha256},created_at:now};
        let raw=serde_json::to_string(&context)?;ensure!(raw.len()<=16384,"recovery native context exceeds bound");let sha=digest(&raw);
        event(&tx,&lease.claim.original.workflow,lease.generation(),"owner_recovery_native_intent_recorded",&serde_json::json!({"version":1,"request_id":lease.request_id(),"transfer_id":pending.request().transfer_id,"record_sha256":sha}).to_string(),now)?;
        let context_sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&lease.claim.original.workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_owner_recovery_native_intent VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![lease.claim.original.workflow,lease.request_id(),pending.request().transfer_id,lease.generation(),sequence,context_sequence,sha,raw])?;
        history(&tx,&lease.claim.original.workflow)?;
        let current=chrono::Utc::now().timestamp_millis();require_authorized_claim(&tx,lease,authorization,current)?;
        let checked=recovery_native_facts_tx(&tx,lease,authorization,current)?;
        ensure!(checked.fingerprint()?==basis.fingerprint()?,"recovery native intent facts changed before commit");
        let id=pending.request().transfer_id.clone();*attempted=Some(pending);tx.commit()?;Ok(id)
    }
}

// SQLite's WAL hook runs after durable commit; returning IOERR reproduces a
// lost acknowledgement without changing the production persistence path.
#[cfg(test)]
static LOST_COMMIT_ACKS: std::sync::atomic::AtomicUsize=std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
unsafe extern "C" fn lose_commit_ack(_: *mut std::ffi::c_void,_: *mut rusqlite::ffi::sqlite3,_: *const std::ffi::c_char,_: std::ffi::c_int)->std::ffi::c_int {
    LOST_COMMIT_ACKS.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
    rusqlite::ffi::SQLITE_IOERR
}
#[cfg(test)]
impl WorkflowStore {
    pub(crate) fn test_lose_recovery_commit_ack(&mut self) {
        LOST_COMMIT_ACKS.store(0,std::sync::atomic::Ordering::SeqCst);
        // Static callback state remains valid even if a fixture assertion panics.
        unsafe {rusqlite::ffi::sqlite3_wal_hook(self.conn.handle(),Some(lose_commit_ack),std::ptr::null_mut());}
    }
    pub(crate) fn test_finish_recovery_commit_ack_loss(&mut self)->usize {
        unsafe {rusqlite::ffi::sqlite3_wal_hook(self.conn.handle(),None,std::ptr::null_mut());}
        LOST_COMMIT_ACKS.load(std::sync::atomic::Ordering::SeqCst)
    }
}
