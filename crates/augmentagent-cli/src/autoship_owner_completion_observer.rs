//! Unpaid, authenticated late-completion observation. No executable authority.
use super::*;
use std::sync::{Arc, Mutex};

/// A live observation scope, never reconstructible from saved history. Its only
/// transition retains the exact claim committed by its own proof-bearing writer.
pub(crate) struct CompletionObservation {
    expected: RecoveryPlan,
    settled: Mutex<Option<Arc<RecoveryLease>>>,
}

fn require_preclaim(conn:&Connection,expected:&RecoveryPlan,now:i64)->Result<()> {
    let current=plan(conn,&expected.original.workflow)?.context("completion observation no longer pending")?;
    ensure!(current==*expected && current.predecessor.lease_until<=now,
        "completion observation predecessor changed or still live");
    ensure!(blocked::blocked_state(conn,&current,now)?==Some(RecoveryBlockedState::LegacyInputWriter),
        "completion observation requires unchanged legacy input debt");
    let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",[&expected.original.workflow],|r|r.get(0))?;
    ensure!(!pending && !native_pending::orphaned(conn,&expected.original.workflow)?,
        "completion observation cannot hide unsettled effects");
    Ok(())
}

fn require_phase(conn:&Connection,observation:&CompletionObservation,now:i64)->Result<()> {
    let settled=observation.settled.lock().map_err(|_|anyhow::anyhow!("completion observation poisoned"))?;
    match settled.as_ref() {
        Some(lease)=>require_current_claim(conn,lease,now),
        None=>require_preclaim(conn,&observation.expected,now),
    }
}

pub(crate) fn completion_observation_fence(store:Arc<Mutex<WorkflowStore>>,
    observation:Arc<CompletionObservation>,
    authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64,
)->Result<Arc<dyn Fn()->Result<()>+Send+Sync>> {
    store.lock().map_err(|_|anyhow::anyhow!("completion observation store poisoned"))?
        .validate_owner_completion_observation(&observation,authorization,now)?;
    let issued=authorization.issued_at();let expires=authorization.expires_at();
    let configuration=authorization.configuration_fence();
    Ok(Arc::new(move || {
        if let Some(configuration)=&configuration {configuration()?;}
        let now=chrono::Utc::now().timestamp_millis();
        ensure!(now>=issued && now<=expires,"completion observation authorization expired");
        let store=store.lock().map_err(|_|anyhow::anyhow!("completion observation store poisoned"))?;
        require_phase(&store.conn,&observation,now)
    }))
}

impl WorkflowStore {
    pub(crate) fn begin_owner_completion_observation(&self,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64,
    )->Result<Option<Arc<CompletionObservation>>> {
        authorization.require(authorization.plan(),now)?;
        let tx=self.conn.unchecked_transaction()?;
        let expected=authorization.plan();
        let Some(current)=plan(&tx,&expected.original.workflow)? else {return Ok(None)};
        if current!=*expected || current.predecessor.lease_until>now
            || blocked::blocked_state(&tx,&current,now)?!=Some(RecoveryBlockedState::LegacyInputWriter) {
            return Ok(None);
        }
        require_preclaim(&tx,expected,now)?;
        tx.commit()?;
        Ok(Some(Arc::new(CompletionObservation {expected:expected.clone(),settled:Mutex::new(None)})))
    }

    pub(crate) fn validate_owner_completion_observation(&self,observation:&CompletionObservation,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64,
    )->Result<()> {
        authorization.require(&observation.expected,now)?;
        require_phase(&self.conn,observation,now)
    }

    pub(crate) fn owner_completion_observation_native_facts(&self,observation:&CompletionObservation,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,now:i64,
    )->Result<super::super::owner_publication::OwnerPublicationNativeFacts> {
        self.validate_owner_completion_observation(observation,authorization,now)?;
        let tx=self.conn.unchecked_transaction()?;
        require_phase(&tx,observation,now)?;
        let original=&observation.expected.original;
        let (record,_)=native_intent::original_record(&tx,original)?;
        let context=RecoveryNativeContext {conn:&tx,workflow:&original.workflow,
            binding:record.recovery_binding().clone(),namespace:native_intent::frontier(&tx,original)?};
        let facts=super::super::native_handoff::read_recovery_metadata_facts(&context)?;
        tx.commit()?;
        Ok(super::super::owner_publication::OwnerPublicationNativeFacts::from_recovery_metadata(facts))
    }

    /// The caller retains actual original native locks and the borrowed proof.
    /// No callback/native I/O occurs in this transaction. An unchanged outcome,
    /// failed import or remaining blocker rolls back the candidate claim too.
    pub(crate) fn claim_with_owner_native_completion(&mut self,observation:&CompletionObservation,
        authorization:&crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        proof:&augmentagent_channel_core::native_vm::ownership::MetadataNativeCompletionProof<'_>,
        owner:&str,now:i64,ttl:i64,
    )->Result<Option<Arc<RecoveryLease>>> {
        let mut settled=observation.settled.lock().map_err(|_|anyhow::anyhow!("completion observation poisoned"))?;
        ensure!(settled.is_none(),"completion observation already settled");
        authorization.require(&observation.expected,now)?;
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_preclaim(&tx,&observation.expected,now)?;
        let original=&observation.expected.original;
        let (record,_)=native_intent::original_record(&tx,original)?;
        record.validate_native_binding(proof.binding())?;
        let namespace=native_intent::frontier(&tx,original)?;
        ensure!(namespace.owner==proof.prepared().intent.source_owner
            && Path::new(&namespace.audit_path)==proof.prepared().intent.source_audit_path,
            "completion observation requires original active launch namespace");
        let Some(lease)=append_claim_tx(&tx,authorization,&observation.expected,owner,now,ttl)? else {return Ok(None)};
        let import=CompletionImport::for_lease(&lease);
        super::super::native_observation::record_metadata_completion_tx(&tx,record.recovery_binding(),proof,&import,now)?;
        let current=plan(&tx,&original.workflow)?.context("completion observation lost pending request")?;
        if blocked::blocked_state(&tx,&current,now)?.is_some() {return Ok(None);}
        require_authorized_claim(&tx,&lease,authorization,chrono::Utc::now().timestamp_millis())?;
        ensure!(native_intent::frontier(&tx,original)?==namespace,"completion observation frontier changed");
        tx.commit()?;
        let lease=Arc::new(lease);
        *settled=Some(lease.clone());
        Ok(Some(lease))
    }
}
