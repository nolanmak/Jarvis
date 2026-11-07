//! One durable native transfer for the actual live successor setup role.
//! Completion retains setup-only purpose; no body or coding guard is minted.
use super::*;
use augmentagent_channel_core::native_vm::ownership::{Authority,PreparationScan,PreparedHandoffIntent,TransferReason as R,TransferRequest};

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextRecord {
    version:u32,workflow:String,request_id:String,generation:i64,setup:RecordRef,ready:RecordRef,
    original_intent:super::super::super::owner_publication::Reference,
    source:Authority,facts_sha256:String,pending:RecordRef,at_ms:i64,
}
/// Borrowed live reservation plus exact durable purpose, never a coding lease.
pub(crate) struct SuccessorNativeIntent<'a> {pending:PendingNativeHandoff,_setup:&'a SuccessorSetup}
impl SuccessorNativeIntent<'_> {
    pub(crate) fn transfer_id(&self)->&str {&self.pending.request().transfer_id}
    pub(crate) fn core_intent(&self)->PreparedHandoffIntent {
        PreparedHandoffIntent {original_binding:self.pending.original_binding().clone(),request:self.pending.request().clone(),observation:self.pending.observation()}
    }
}
pub(crate) enum SuccessorNativePersistence<'a> {
    Durable(SuccessorNativeIntent<'a>),NotCommitted {reason:R},Unknown {reason:R,attempted_transfer:Option<String>},
}
fn original_source(conn:&Connection,workflow:&str,setup:&Record)->Result<(Original,Authority)> {
    let (claim,reference)=source(conn,workflow,setup.ready_generation)?;
    ensure!(reference==setup.recovery,"successor native original claim changed");
    let authority=native_intent::frontier(conn,&claim.original)?;
    Ok((claim.original,authority))
}
fn destination(conn:&Connection,workflow:&str,generation:i64)->Result<Authority> {
    let state=conn.path().context("successor native setup needs original file database")?;
    ensure!(!state.is_empty(),"successor native setup needs original file database");
    let (_,audit,_)=native_audit_generation_paths(Path::new(state),workflow,generation)?;
    Ok(Authority {owner:augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:workflow.into(),lease_generation:generation},audit_path:audit.to_str().context("successor audit path encoding")?.into()})
}
/// Bounded descriptive joins only. Does not depend on fresh facts or live auth,
/// so native facts/terminal cleanup can validate this same immutable ancestry.
fn history(conn:&Connection,workflow:&str)->Result<Option<(ContextRecord,PendingNativeHandoff)>> {
    let rows:Vec<(i64,i64,String,i64,i64,String,String,i64)>={let mut q=conn.prepare("SELECT generation,setup_sequence,transfer_id,pending_sequence,sequence,substr(record_sha256,1,65),substr(record,1,16385),length(CAST(record AS BLOB)) FROM workflow_owner_successor_native_intent WHERE workflow=?1 LIMIT 2")?;
        let rows=q.query_map([workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?)))?.collect::<rusqlite::Result<_>>()?;rows};
    let events:i64=conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_successor_native_intent_recorded'",[workflow],|r|r.get(0))?;
    ensure!(rows.len()<=1 && events==rows.len() as i64,"successor native context missing or unsupported chain");
    let setup=read(conn,workflow)?;
    let Some((generation,setup_sequence,id,pending_sequence,sequence,sha,raw,len))=rows.into_iter().next() else {
        if let Some((setup,_))=setup {
            let orphan:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind='native_handoff_prepared') OR EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2)",params![workflow,setup.setup.generation],|r|r.get(0))?;
            ensure!(!orphan,"successor native purpose context missing");
        }
        return Ok(None);
    };
    let (setup,setup_ref)=setup.context("successor native setup edge missing")?;
    ensure!((1..=16384).contains(&len) && raw.len() as i64==len && digest(&raw)==sha,"successor native context exceeds bound or changed");
    let record:ContextRecord=serde_json::from_str(&raw)?;
    let (original,source)=original_source(conn,workflow,&setup)?;
    ensure!(serde_json::to_string(&record)?==raw && record.version==1 && record.workflow==workflow && record.request_id==setup.request_id
        && record.generation==generation && generation==setup.setup.generation && record.setup==setup_ref && setup_sequence==setup_ref.sequence
        && record.ready==setup.ready && record.original_intent==original.intent && record.source==source
        && record.setup.sequence<pending_sequence && pending_sequence<sequence && record.pending.sequence==pending_sequence
        && record.at_ms>=setup.at_ms && record.at_ms<setup.lease_until
        && record.facts_sha256.len()==64 && record.facts_sha256.bytes().all(|b|matches!(b,b'0'..=b'9'|b'a'..=b'f')),"successor native context lineage changed");
    let pending=native_pending::recorded_pending(conn,workflow,&id)?;
    let expected_sha=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(&pending)?)?));
    let (intent,_)=native_intent::original_record(conn,&original)?;
    intent.validate_native_binding(pending.original_binding())?;
    ensure!(expected_sha==record.pending.sha256 && pending.request().source_owners==vec![source]
        && pending.request().destination==destination(conn,workflow,generation)? && pending.workflow_binding()==&setup.checkpoint.binding,"successor native pending scope changed");
    let (kind,event_generation,payload,at,event_len):(String,i64,String,i64,i64)=conn.query_row("SELECT kind,generation,substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
    ensure!((1..=4096).contains(&event_len) && payload.len() as i64==event_len && kind=="owner_successor_native_intent_recorded" && event_generation==generation && at==record.at_ms
        && serde_json::from_str::<serde_json::Value>(&payload)?==serde_json::json!({"version":1,"request_id":record.request_id,"transfer_id":id,"record_sha256":sha}),"successor native context event changed");
    let pending_at:i64=conn.query_row("SELECT at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",params![workflow,pending_sequence],|r|r.get(0))?;
    ensure!(pending_at==record.at_ms,"successor native pending time changed");
    Ok(Some((record,pending)))
}
pub(super) fn frontier(conn:&Connection,setup:&SuccessorSetup)->Result<Authority> {
    if let Some((_,pending))=history(conn,&setup.record.workflow)? {
        let completed=native_pending::settlement::History::load(conn,&setup.record.workflow,u64::MAX)?;
        if completed.current_pending(setup.generation(),&setup.record.checkpoint.binding)?.as_ref()==Some(&pending) {return Ok(pending.request().destination.clone());}
    }
    Ok(original_source(conn,&setup.record.workflow,&setup.record)?.1)
}
pub(super) fn require_context(conn:&Connection,setup:&SuccessorSetup,
    authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,pending:&PendingNativeHandoff,now:i64)->Result<()> {
    require_authorized_setup(conn,setup,authorization,now)?;
    let (context,actual)=history(conn,&setup.record.workflow)?.context("successor native intent missing")?;
    ensure!(context.setup==setup.reference && actual==*pending,"successor native transfer purpose changed");
    native_pending::require_clear_abort_barriers(conn,&setup.record.workflow)?;Ok(())
}
pub(super) fn require_terminal(conn:&Connection,pending:&PendingNativeHandoff)->Result<()> {
    if read(conn,&pending.request().workflow_id)?.is_some_and(|(setup,_)|setup.setup.generation==pending.request().destination.owner.lease_generation) {
        ensure!(history(conn,&pending.request().workflow_id)?.is_some_and(|(_,actual)|actual==*pending),"successor terminal transfer context missing");
    }
    Ok(())
}
fn selected(conn:&Connection,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64)->Result<Option<PendingNativeHandoff>> {
    require_authorized_setup(conn,setup,authorization,now)?;
    let context=history(conn,&setup.record.workflow)?;
    let active:Option<String>=conn.query_row("SELECT transfer_id FROM workflow_native_handoff_pending WHERE workflow=?1",[&setup.record.workflow],|r|r.get(0)).optional()?;
    ensure!(active.as_ref().is_none_or(|id|context.as_ref().is_some_and(|(_,p)|&p.request().transfer_id==id)),"another pending transfer owns successor setup");
    ensure!(!native_pending::orphaned(conn,&setup.record.workflow)?,"successor native persistence unknown");
    Ok(context.map(|(_,pending)|pending))
}
/// Only this typed transaction can use the shared descriptive snapshot writer.
pub(crate) struct SnapshotContext<'tx,'db> {tx:&'tx Transaction<'db>,generation:i64,facts:&'tx crate::autoship_workflow::native_handoff::NativeHandoffFacts}
impl<'db> SnapshotContext<'_,'db> {
    pub(crate) fn transaction(&self)->&Transaction<'db> {self.tx}
    pub(crate) fn generation(&self)->i64 {self.generation}
    pub(crate) fn facts(&self)->&crate::autoship_workflow::native_handoff::NativeHandoffFacts {self.facts}
}
impl WorkflowStore {
    pub(crate) fn owner_successor_pending_transfer(&mut self,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64)->Result<Option<PendingNativeHandoff>> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        let pending=selected(&tx,setup,authorization,now)?;
        let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1)",[&setup.record.workflow],|r|r.get(0))?;
        tx.commit()?;Ok(pending.filter(|_|active))
    }
    pub(crate) fn validate_owner_successor_transfer(&self,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,pending:&PendingNativeHandoff,now:i64)->Result<()> {
        require_context(&self.conn,setup,authorization,pending,now)
    }
    pub(crate) fn owner_successor_transfer_replay_is_current(&mut self,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,pending:&PendingNativeHandoff,now:i64)->Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Deferred)?;
        require_context(&tx,setup,authorization,pending,now)?;
        let row:(String,i64,u64,String)=tx.query_row("SELECT transfer_id,sequence,snapshot_sequence,record FROM workflow_native_handoff_pending WHERE workflow=?1",[&setup.record.workflow],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(native_pending::decode(&tx,&setup.record.workflow,row)?==*pending,"successor active pending changed");
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",params![setup.record.workflow,setup.generation()],|r|r.get(0))?;
        ensure!(!blocked,"successor replay has occupied destination or unsettled mutation");
        for source in &pending.request().source_owners {
            ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_owner WHERE workflow=?1 AND lease_generation=?2 AND audit_path=?3 AND retirement_reason IS NULL AND retired_at IS NULL)",params![setup.record.workflow,source.owner.lease_generation,source.audit_path],|r|r.get::<_,bool>(0))?,"successor replay source changed");
        }
        tx.commit()?;Ok(())
    }
    pub(crate) fn prepare_owner_successor_native_intent<'a>(&mut self,setup:&'a SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,
        basis:&crate::autoship_workflow::owner_publication::OwnerPublicationNativeFacts,scan:&PreparationScan<'_>,now:i64)->SuccessorNativePersistence<'a> {
        let mut attempted=None;
        let reported=self.write_successor_intent(setup,authorization,basis,scan,now,&mut attempted);
        let expected=reported.as_ref().ok().cloned().or(attempted);
        let reason=reported.as_ref().err().and_then(|e|e.downcast_ref::<R>()).copied().unwrap_or(R::PersistRetry);
        match selected(&self.conn,setup,authorization,chrono::Utc::now().timestamp_millis()) {
            Ok(Some(pending)) if Some(&pending.request().transfer_id)==expected.as_ref()=>SuccessorNativePersistence::Durable(SuccessorNativeIntent {pending,_setup:setup}),
            Ok(None) if reported.is_err() && expected.is_none()=>SuccessorNativePersistence::NotCommitted {reason},
            _=>SuccessorNativePersistence::Unknown {reason,attempted_transfer:expected},
        }
    }
    fn write_successor_intent(&mut self,setup:&SuccessorSetup,authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,
        basis:&crate::autoship_workflow::owner_publication::OwnerPublicationNativeFacts,scan:&PreparationScan<'_>,now:i64,attempted:&mut Option<String>)->Result<String> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(pending)=selected(&tx,setup,authorization,now)? {
            ensure!(scan.binding()==pending.original_binding() && scan.source_owners()==pending.request().source_owners && scan.destination()==&pending.request().destination,"successor native intent cannot be retargeted");
            let id=pending.request().transfer_id.clone();tx.commit()?;return Ok(id);
        }
        let fresh=native::facts(&tx,setup,now)?;
        ensure!(fresh.fingerprint()?==basis.fingerprint()? && scan.binding()==fresh.original_binding()
            && scan.source_owners()==fresh.snapshot_facts().source_owners && scan.destination()==&destination(&tx,&setup.record.workflow,setup.generation())?,"successor native scan scope changed");
        let blocked:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[&setup.record.workflow],|r|r.get(0))?;
        ensure!(!blocked,"successor native intent has unsettled remote mutation");
        let rows=crate::autoship_workflow::native_handoff::selected_rows(&tx,fresh.snapshot_facts(),scan.inventory(),scan.retired())?;
        let context=SnapshotContext {tx:&tx,generation:setup.generation(),facts:fresh.snapshot_facts()};
        let snapshot=crate::autoship_workflow::native_handoff::capture_successor_selected(&context,rows,now)?;
        ensure!(snapshot.observation.certain,R::ConversationUncertain);
        let request=TransferRequest {version:1,transfer_id:uuid::Uuid::new_v4().to_string(),workflow_id:setup.record.workflow.clone(),source_owners:snapshot.source_owners,inventory:snapshot.inventory,destination:scan.destination().clone()};request.validate()?;
        let pending=PendingNativeHandoff {version:1,request,original_binding:snapshot.original_binding,workflow_binding:setup.record.checkpoint.binding.clone(),observation_revision:snapshot.observation.revision,observation_sha256:snapshot.observation.sha256,observation_certain:true};
        let (sequence,sha256)=native_pending::append_pending(&tx,&pending,now)?;
        let (original,source)=original_source(&tx,&setup.record.workflow,&setup.record)?;
        let context=ContextRecord {version:1,workflow:setup.record.workflow.clone(),request_id:setup.request_id().into(),generation:setup.generation(),setup:setup.reference.clone(),ready:setup.record.ready.clone(),original_intent:original.intent,source,facts_sha256:fresh.fingerprint()?,pending:RecordRef {sequence,sha256},at_ms:now};
        let raw=serde_json::to_string(&context)?;ensure!(raw.len()<=16384,"successor native context exceeds bound");let sha=digest(&raw);
        event(&tx,&setup.record.workflow,setup.generation(),"owner_successor_native_intent_recorded",&serde_json::json!({"version":1,"request_id":setup.request_id(),"transfer_id":pending.request().transfer_id,"record_sha256":sha}).to_string(),now)?;
        let context_sequence:i64=tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1",[&setup.record.workflow],|r|r.get(0))?;
        tx.execute("INSERT INTO workflow_owner_successor_native_intent VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",params![setup.record.workflow,setup.generation(),setup.reference.sequence,pending.request().transfer_id,sequence,context_sequence,sha,raw])?;
        history(&tx,&setup.record.workflow)?;
        let current=chrono::Utc::now().timestamp_millis();
        require_authorized_setup(&tx,setup,authorization,current)?;
        ensure!(native::facts(&tx,setup,current)?.fingerprint()?==basis.fingerprint()?,"successor native facts changed before commit");
        let id=pending.request().transfer_id.clone();*attempted=Some(id.clone());tx.commit()?;Ok(id)
    }
}
