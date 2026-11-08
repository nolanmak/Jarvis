//! Closed preparation journal for the live, authenticated successor setup.
//! Neither persisted records nor this controller grant model/body/input release.
use super::*;
use std::sync::{Arc,Mutex};
use augmentagent_channel_core::native_vm::ownership::{Authority,TransferBinding};
use crate::autoship_workflow::native_launch::{self,NativeLaunchRecord,VerifiedLaunchAppend};

const KIND:&str="owner_successor_bootstrap_launch_recorded";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapRecord {
    version:u32,workflow:String,request_id:String,generation:i64,setup:RecordRef,
    intent_id:String,native_sequence:i64,native_sha256:String,at_ms:i64,
}
fn hash(value:&str)->String {format!("{:x}",Sha256::digest(value.as_bytes()))}
fn recorded(conn:&Connection,setup:&SuccessorSetup)->Result<Option<NativeLaunchRecord>> {
    let rows:Vec<(i64,String,i64,i64)>={
        let mut q=conn.prepare("SELECT seq,substr(payload,1,16385),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE workflow=?1 AND generation=?2 AND kind=?3 LIMIT 2")?;
        let rows=q.query_map(params![setup.workflow_id(),setup.generation(),KIND],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;rows
    };
    ensure!(rows.len()<=1,"duplicate successor bootstrap launch context");
    let launches:i64=conn.query_row("SELECT count(*) FROM workflow_native_launch WHERE workflow=?1 AND lease_generation=?2",params![setup.workflow_id(),setup.generation()],|r|r.get(0))?;
    let Some((sequence,raw,at,len))=rows.into_iter().next() else {
        ensure!(launches==0,"successor bootstrap launch lost its setup ancestry");return Ok(None);
    };
    ensure!(launches==1,"successor bootstrap has unclassified native launches");
    ensure!((1..=16384).contains(&len) && raw.len() as i64==len,"bootstrap launch context exceeds bound");
    let record:BootstrapRecord=serde_json::from_str(&raw)?;
    ensure!(serde_json::to_value(&record)?==serde_json::from_str::<serde_json::Value>(&raw)?
        && record.version==1 && record.workflow==setup.workflow_id() && record.request_id==setup.request_id()
        && record.generation==setup.generation() && record.setup==setup.reference && record.at_ms==at
        && record.at_ms>=setup.record.at_ms && sequence>record.native_sequence
        && record.native_sequence>setup.reference.sequence,"bootstrap launch context changed");
    let launch=native_launch::recorded_launch(conn,&record.intent_id)?;
    ensure!(launch.sequence==record.native_sequence && hash(&serde_json::to_string(&launch)?)==record.native_sha256
        && launch.source_owner.workflow_id==setup.workflow_id() && launch.source_owner.lease_generation==setup.generation()
        && launch.workflow_binding==setup.record.checkpoint.binding,"bootstrap launch ancestry changed");
    let event:(String,i64,String,String,i64,i64)=conn.query_row("SELECT workflow,generation,kind,substr(payload,1,4097),at_ms,length(CAST(payload AS BLOB)) FROM workflow_event WHERE seq=?1",[launch.sequence],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
    let provider=match launch.intent.provider {submission::Provider::Codex=>"codex",submission::Provider::Claude=>"claude"};
    ensure!((1..=4096).contains(&event.5) && event.3.len() as i64==event.5 && event.0==setup.workflow_id() && event.1==setup.generation() && event.2=="native_launch_intent_recorded"
        && event.4==launch.recorded_at_ms && event.4==record.at_ms
        && serde_json::from_str::<serde_json::Value>(&event.3)?==serde_json::json!({"provider":provider,"launch_sha256":launch.intent.launch_sha256}),
        "bootstrap launch original event changed");
    Ok(Some(launch))
}

/// One immutable authorization observation. A new observation never changes
/// the prepared attempt, principal, configured identity or provider selection.
struct BootstrapAuthorization {
    issued:i64,expires:i64,configuration:Option<Arc<dyn Fn()->Result<()>+Send+Sync>>,
}
impl BootstrapAuthorization {
    /// Enforce the issuer deadline without extending the maximum snapshot age.
    fn require_time(&self,now:i64)->Result<()> {
        ensure!(now>=self.issued && now<=self.expires && self.expires<=self.issued.saturating_add(30_000),
            "successor bootstrap authorization expired");Ok(())
    }
}
/// Captured only from actual live setup. Fresh authorization can be replaced
/// under the same controller; its one-shot native constructor cannot be reset.
pub(crate) struct SuccessorBootstrapPersistence {
    setup:Arc<SuccessorSetup>,authorization:std::sync::RwLock<Arc<BootstrapAuthorization>>,
    principal:String,configuration_binding:Option<String>,
    namespace:Authority,original_binding:TransferBinding,provider:Option<submission::Provider>,
}
/// One checked journal call tied to its exact live authorization observation.
pub(crate) struct CheckedBootstrapJournal<'a> {
    authority:&'a SuccessorBootstrapPersistence,snapshot:Arc<BootstrapAuthorization>,at_ms:i64,
}
/// Consumed only after a fresh original-owner check outside workflow/native locks.
pub(crate) struct CheckedBootstrapRenewal<'a,'plan> {
    authority:&'a SuccessorBootstrapPersistence,
    authorization:&'a crate::self_improve::targeted::AuthorizedOwnerSuccessor<'plan>,
    prior:Arc<BootstrapAuthorization>,next:Arc<BootstrapAuthorization>,at_ms:i64,
}
impl SuccessorBootstrapPersistence {
    /// Retain this exact immutable observation without holding a lock over IO.
    fn snapshot(&self)->Result<Arc<BootstrapAuthorization>> {
        Ok(self.authorization.read().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?.clone())
    }
    /// Scheduling information only; reading this cannot extend freshness.
    pub(crate) fn authorization_deadline_ms(&self)->Result<i64> {Ok(self.snapshot()?.expires)}
    /// Call outside workflow/native locks. Replacing authorization during this
    /// external check refuses instead of splicing two different observations.
    pub(crate) fn check_external(&self)->Result<CheckedBootstrapJournal<'_>> {
        let snapshot=self.snapshot()?;
        if let Some(current)=&snapshot.configuration {current()?;}
        let at_ms=chrono::Utc::now().timestamp_millis();snapshot.require_time(at_ms)?;
        let current=self.authorization.read().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?;
        ensure!(Arc::ptr_eq(&current,&snapshot),"bootstrap authorization changed during check");
        Ok(CheckedBootstrapJournal {authority:self,snapshot,at_ms})
    }
    /// Actual fresh owner/configuration checks happen before taking the store
    /// mutex. Reusing a snapshot or changing its source/identity/pin refuses.
    pub(crate) fn check_authorization_renewal<'a,'plan>(&'a self,
        authorization:&'a crate::self_improve::targeted::AuthorizedOwnerSuccessor<'plan>,
    )->Result<CheckedBootstrapRenewal<'a,'plan>> {
        let prior=self.snapshot()?;let at_ms=chrono::Utc::now().timestamp_millis();
        authorization.require(authorization.plan(),at_ms)?;
        ensure!(authorization.principal()==self.principal
            && authorization.configuration_binding()==self.configuration_binding
            && authorization.plan().command().provider==self.provider,"bootstrap authorization identity or provider changed");
        let next=Arc::new(BootstrapAuthorization {issued:authorization.issued_at(),expires:authorization.expires_at(),
            configuration:authorization.configuration_fence()});
        next.require_time(at_ms)?;
        ensure!(next.issued>prior.issued && next.expires>prior.expires,"bootstrap renewal requires a newer owner authorization and deadline");
        let current=self.authorization.read().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?;
        ensure!(Arc::ptr_eq(&current,&prior),"bootstrap authorization changed during renewal check");
        Ok(CheckedBootstrapRenewal {authority:self,authorization,prior,next,at_ms})
    }
    /// Validate current SQL ancestry under the current authorization observation.
    fn require_current(&self,conn:&Connection,now:i64)->Result<()> {
        let snapshot=self.authorization.read().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?;
        self.require_current_with(conn,now,&snapshot)
    }
    /// Check a pinned observation while its publication read guard remains held.
    fn require_current_with(&self,conn:&Connection,now:i64,snapshot:&BootstrapAuthorization)->Result<()> {
        snapshot.require_time(now)?;self.require_setup_current(conn,now)
    }
    /// Check SQL-only setup invariants independently of an older snapshot deadline.
    fn require_setup_current(&self,conn:&Connection,now:i64)->Result<()> {
        require_current_setup(conn,&self.setup,now)?;
        let pin:String=conn.query_row("SELECT substr(provider,1,65) FROM command_job WHERE workflow=?1",[self.setup.workflow_id()],|r|r.get(0))?;
        ensure!(pin==serde_json::to_string(&self.provider)?,"successor bootstrap provider policy changed");
        native_pending::require_clear_abort_barriers(conn,self.setup.workflow_id())?;
        let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1)",[self.setup.workflow_id()],|r|r.get(0))?;
        ensure!(!pending && !native_pending::orphaned(conn,self.setup.workflow_id())?,"successor bootstrap native transfer is unsettled");
        ensure!(super::transfer::frontier(conn,&self.setup)?==self.namespace,"successor bootstrap native frontier changed");
        let (audit,digest)=native_launch::original_context(conn,self.setup.workflow_id(),self.setup.generation(),true)?;
        ensure!(audit==self.namespace.audit_path && digest==native_launch::binding_digest(&self.original_binding)?,"successor bootstrap original native binding changed");
        recorded(conn,&self.setup)?;Ok(())
    }
    pub(crate) fn native_fence(self:&Arc<Self>,store:Arc<Mutex<WorkflowStore>>)->Arc<dyn Fn()->Result<()>+Send+Sync> {
        let original=self.clone();Arc::new(move || {
            let checked=original.check_external()?;
            let store=store.lock().map_err(|_|anyhow::anyhow!("successor bootstrap store poisoned"))?;
            let snapshot=original.authorization.read().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?;
            ensure!(Arc::ptr_eq(&snapshot,&checked.snapshot),"bootstrap authorization changed before native check");
            original.require_current_with(&store.conn,chrono::Utc::now().timestamp_millis(),&snapshot)
        })
    }
}

/// A transaction-borrowed insertion context. Only this module constructs it.
pub(crate) struct SuccessorBootstrapJournalContext<'tx,'db,'authority> {
    tx:&'tx Transaction<'db>,authority:&'authority SuccessorBootstrapPersistence,
}
impl<'db> SuccessorBootstrapJournalContext<'_,'db,'_> {
    pub(crate) fn connection(&self)->&Connection {self.tx}
    pub(crate) fn transaction(&self)->&Transaction<'db> {self.tx}
    pub(crate) fn namespace(&self)->&Authority {&self.authority.namespace}
    pub(crate) fn original_binding(&self)->&TransferBinding {&self.authority.original_binding}
    pub(crate) fn binding(&self)->&Binding {&self.authority.setup.record.checkpoint.binding}
}
impl WorkflowStore {
    pub(crate) fn owner_successor_bootstrap_persistence(&mut self,setup:Arc<SuccessorSetup>,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerSuccessor<'_>,now:i64,
    )->Result<SuccessorBootstrapPersistence> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_authorized_setup(&tx,&setup,authorization,now)?;
        ensure!(super::native::original(&tx,&setup)?.body_dispatch.is_none(),"reserved owner body requires its reservation-preserving constructor");
        ensure!(recorded(&tx,&setup)?.is_none(),"successor bootstrap already has an original launch; reconcile it");
        let facts=super::native::facts(&tx,&setup,now)?;
        ensure!(facts.namespace().owner.lease_generation==setup.generation(),"successor bootstrap needs completed native ownership setup");
        let result=SuccessorBootstrapPersistence {authorization:std::sync::RwLock::new(Arc::new(BootstrapAuthorization {
                issued:authorization.issued_at(),expires:authorization.expires_at(),configuration:authorization.configuration_fence()})),
            principal:authorization.principal().into(),configuration_binding:authorization.configuration_binding(),
            namespace:facts.namespace().clone(),original_binding:facts.original_binding().clone(),
            provider:authorization.plan().command().provider,setup};
        result.require_current(&tx,now)?;tx.commit()?;
        // No native effects have begun. One original setup object can issue
        // only one controller, even when callers race before journal commit.
        ensure!(result.setup.bootstrap_controller_issued.compare_exchange(false,true,
            std::sync::atomic::Ordering::AcqRel,std::sync::atomic::Ordering::Acquire).is_ok(),
            "successor bootstrap preparation controller already issued");
        Ok(result)
    }
    /// Replace only the live authorization observation, with no native effect,
    /// progress event, lease generation change or constructor-claim reset.
    pub(crate) fn renew_successor_bootstrap_authorization(&mut self,checked:CheckedBootstrapRenewal<'_,'_>,now:i64)->Result<()> {
        let authority=checked.authority;ensure!(now>=checked.at_ms,"bootstrap renewal check is newer than its write");
        // All paths take the SQL writer before this lock, including separate
        // connections. Journal checks hold a read guard through SQL commit.
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut snapshot=authority.authorization.write().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?;
        ensure!(Arc::ptr_eq(&snapshot,&checked.prior),"bootstrap authorization changed before renewal");
        checked.next.require_time(now)?;
        ensure!(checked.next.issued>snapshot.issued && checked.next.expires>snapshot.expires,"bootstrap renewal did not advance actual authorization");
        require_setup_plan(&tx,&authority.setup,checked.authorization.plan(),now)?;
        ensure!(checked.authorization.principal()==authority.principal
            && checked.authorization.configuration_binding()==authority.configuration_binding
            && checked.authorization.plan().command().provider==authority.provider,"bootstrap renewal identity or provider changed");
        authority.require_setup_current(&tx,now)?;
        tx.commit()?;
        *snapshot=checked.next;Ok(())
    }
    /// `append` was verified against the protected audit before taking the
    /// workflow mutex. No native/audit lock is acquired in this transaction.
    pub(crate) fn record_successor_bootstrap_launch(&mut self,checked:CheckedBootstrapJournal<'_>,
        append:&VerifiedLaunchAppend,now:i64,
    )->Result<NativeLaunchRecord> {
        let authority=checked.authority;ensure!(now>=checked.at_ms,"bootstrap journal check is newer than its write");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let snapshot=authority.authorization.read().map_err(|_|anyhow::anyhow!("bootstrap authorization poisoned"))?;
        ensure!(Arc::ptr_eq(&snapshot,&checked.snapshot),"bootstrap journal authorization changed after external check");
        authority.require_current_with(&tx,now,&snapshot)?;
        let access=SuccessorBootstrapJournalContext {tx:&tx,authority};
        let prior=recorded(&tx,&authority.setup)?;
        let result=native_launch::append_successor_bootstrap(&access,append,now)?;
        ensure!(authority.provider.is_none_or(|p|p==result.intent.provider),"successor bootstrap provider pin changed");
        if let Some(prior)=prior {ensure!(prior==result,"successor bootstrap cannot replace its original launch");}
        else {
            let count:i64=tx.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind=?2",params![authority.setup.workflow_id(),KIND],|r|r.get(0))?;
            ensure!(count<1024,"successor bootstrap journal capacity reached");
            let record=BootstrapRecord {version:1,workflow:authority.setup.workflow_id().into(),request_id:authority.setup.request_id().into(),
                generation:authority.setup.generation(),setup:authority.setup.reference.clone(),intent_id:result.intent.intent_id.clone(),
                native_sequence:result.sequence,native_sha256:hash(&serde_json::to_string(&result)?),at_ms:now};
            let raw=serde_json::to_string(&record)?;ensure!(raw.len()<=16384,"bootstrap context exceeds bound");
            event(&tx,authority.setup.workflow_id(),authority.setup.generation(),KIND,&raw,now)?;
        }
        ensure!(recorded(&tx,&authority.setup)?.as_ref()==Some(&result),"successor bootstrap journal append changed");
        authority.require_current_with(&tx,chrono::Utc::now().timestamp_millis(),&snapshot)?;
        tx.commit()?;Ok(result)
    }
}
