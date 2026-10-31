//! Owner-request-bound activation of registered contract metadata.
use super::*;
use super::owner_turn::OwnerTurn;
use crate::autoship_contract::RegisteredContract;
impl WorkflowStore {
    /// Controller supplies an independently prepared registration. This records
    /// metadata activation only: baseline/candidate checks and review still gate shipping.
    pub fn activate_owner_revision(&mut self,turn:&OwnerTurn,previous:&RegisteredContract,next:&RegisteredContract,binding:&Binding,root:&Path,now:i64)->Result<OwnerTurn> {
        previous.verify_metadata()?;next.verify_integrity(root)?;binding.validate()?;
        ensure!(previous.contract().source()?==next.contract().source()? && next.contract().revision==previous.contract().revision.checked_add(1).context("contract revision overflow")?,"owner contract source or revision mismatch");
        ensure!(previous.baseline_head()==next.baseline_head(),"owner revision changed the registered baseline");
        ensure!(binding.head==turn.binding.head && binding.contract==next.contract_hash(),"owner revision binding mismatch");
        for prior in &previous.contract().criteria {
            ensure!(next.contract().criteria.iter().any(|criterion|criterion.id==prior.id && criterion.requirement==prior.requirement),"owner revision dropped or changed a prior requirement");
        }
        for scope in &next.contract().allowed_paths {
            let path=match scope {crate::autoship_contract::PathScope::File(path)|crate::autoship_contract::PathScope::Directory(path)=>path};
            ensure!(!crate::autoship_review::immutable_control_path(path),"owner revision touches protected controls");
            if matches!(scope,crate::autoship_contract::PathScope::Directory(_)) {ensure!(previous.contract().allowed_paths.contains(scope),"owner revision adds a broad directory scope");}
        }
        let registration=serde_json::to_string(next)?;ensure!(registration.len()<=4*1024*1024,"owner revision registration exceeds bound");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let body_hash=format!("{:x}",Sha256::digest(turn.body.as_bytes()));
        let prior:Option<(String,String,String,String)>=tx.query_row("SELECT previous_contract,next_contract,binding,owner_message_sha256 FROM workflow_owner_revision WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if let Some((old,new,recorded,body))=prior {
            ensure!(old==previous.contract_hash() && new==next.contract_hash() && recorded==serde_json::to_string(binding)? && body==body_hash,"owner revision replay changed");
            let fence:Option<(i64,i64)>=tx.query_row("SELECT previous_generation,next_generation FROM workflow_owner_revision_fence WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let (before,after)=fence.context("unfenced legacy owner revision requires reconciliation")?;
            ensure!(turn.lease.generation==before || turn.lease.generation==after,"owner revision lease changed");
            let revised=OwnerTurn {lease:Lease {generation:after,..turn.lease.clone()},binding:binding.clone(),..turn.clone()};
            super::owner_turn::validate_turn(&tx,&revised,now)?;return Ok(revised);
        }
        super::owner_turn::validate_turn(&tx,turn,now)?;
        ensure!(turn.binding.contract==previous.contract_hash(),"previous contract differs from claimed owner turn");
        tx.execute("INSERT INTO workflow_owner_revision VALUES(?1,?2,?3,?4,?5,?6,?7)",params![turn.request_id,previous.contract_hash(),next.contract_hash(),serde_json::to_string(binding)?,body_hash,registration,now])?;
        let generation=turn.lease.generation.checked_add(1).context("owner revision generation overflow")?;
        let revised=OwnerTurn {lease:Lease {generation,..turn.lease.clone()},binding:binding.clone(),..turn.clone()};
        tx.execute("INSERT INTO workflow_owner_revision_fence VALUES(?1,?2,?3)",params![turn.request_id,turn.lease.generation,generation])?;
        let checkpoint=Checkpoint {stage:Stage::Scope,binding:binding.clone(),evidence:Vec::new(),next:NextAction {reason:"owner_contract_revised".into(),action:"verify_owner_revision".into(),eligible_at_ms:now}};
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3,generation=?4 WHERE id=?1",params![turn.lease.workflow,serde_json::to_string(&checkpoint)?,now,generation])?;
        tx.execute("UPDATE workflow_owner_turn SET binding=?2,lease_generation=?3 WHERE request_id=?1",params![turn.request_id,serde_json::to_string(binding)?,generation])?;
        super::review_epoch::invalidate(&tx,&turn.lease.workflow,&turn.request_id)?;
        event(&tx,&turn.lease.workflow,generation,"owner_contract_revised",&serde_json::json!({"request_id":turn.request_id,"previous_contract":previous.contract_hash(),"next_contract":next.contract_hash(),"revision":next.contract().revision,"owner_message_sha256":body_hash}).to_string(),now)?;
        event(&tx,&turn.lease.workflow,generation,"checkpoint",&serde_json::to_string(&checkpoint)?,now)?;
        tx.commit()?;Ok(revised)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_contract::*;
    fn setup(root:&Path)->(WorkflowStore,OwnerTurn,RegisteredContract,RegisteredContract,Binding) {
        std::fs::write(root.join("verifier.rs"),"synthetic verifier fixture").unwrap();
        let contract=AcceptanceContract {schema_version:SCHEMA_VERSION,revision:1,issue:None,pull_request:Some(42),criteria:vec![Criterion {id:"C1".into(),requirement:"Original requirement".into(),verifier_ids:vec!["ready".into()]}],allowed_paths:vec![PathScope::File("implementation.rs".into())],verifiers:vec![Verifier {id:"ready".into(),command:VerifierCommand::BlackBox {invocation:BehaviorInvocation::CargoBinary {package:"fixture".into(),bin:"behavior".into(),args:vec![]},oracle:OutputOracle::Json {value:serde_json::json!(true)}},pinned_files:vec!["verifier.rs".into()],failure_witness:"expected ready".into()}]};
        let previous=RegisteredContract::register(root,contract.clone(),"a".repeat(40),"environment".into()).unwrap();
        let mut revised=contract;revised.revision=2;revised.criteria.push(Criterion {id:"C2".into(),requirement:"Additional owner requirement".into(),verifier_ids:vec!["ready".into()]});
        let next=RegisteredContract::register(root,revised,"a".repeat(40),"environment".into()).unwrap();
        let mut store=WorkflowStore::open(&root.join("state.db")).unwrap();
        let origin=discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=submission::Request {repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),1).unwrap().admission.workflow;
        let binding=Binding {head:"b".repeat(40),contract:previous.contract_hash().into(),evidence_hash:"evidence".into()};store.register(&id,"candidate",binding.clone(),2).unwrap();
        store.enqueue_owner_message(&discord_ingress::DiscordOrigin {message:45,..origin},&id,"Additional owner requirement",None,3).unwrap();
        let turn=store.claim_owner_turn(&id,"consumer",4,100).unwrap().unwrap();let binding=Binding {contract:next.contract_hash().into(),..binding};
        (store,turn,previous,next,binding)
    }
    #[test]
    fn owner_revision_rebinds_once_without_applying_message_or_granting_shipping() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let revised=store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),5).expect("registered owner revision cannot activate");
        assert_eq!(revised.binding,binding);assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().checkpoint.binding,binding);
        assert_eq!(store.pending_owner_messages(&turn.lease.workflow).unwrap(),1);
        assert!(store.validate_owner_turn(&turn,6).is_err());store.validate_owner_turn(&revised,6).unwrap();
        assert_eq!(store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),7).unwrap().binding,binding);
        assert!(store.begin_operation(&revised.lease,OperationKind::Push,&binding,8).is_err());
    }
    #[test]
    fn owner_revision_rolls_back_checkpoint_turn_and_receipt_on_audit_failure() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        store.conn.execute_batch("CREATE TRIGGER deny_revision BEFORE INSERT ON workflow_event WHEN NEW.kind='owner_contract_revised' BEGIN SELECT RAISE(ABORT,'unavailable'); END;").unwrap();
        assert!(store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),5).is_err());
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().checkpoint.binding,turn.binding);store.validate_owner_turn(&turn,6).unwrap();
        assert_eq!(store.conn.query_row("SELECT count(*) FROM workflow_owner_revision",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert!(store.events(&turn.lease.workflow).unwrap().iter().all(|(_,kind,_,_)|kind!="owner_contract_revised"));
    }
    #[test]
    fn owner_revision_refuses_tampered_registration_changed_requirements_and_new_intent() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let mut tampered=serde_json::to_value(&previous).unwrap();tampered["contract"]["criteria"][0]["requirement"]="changed".into();
        let tampered:RegisteredContract=serde_json::from_value(tampered).unwrap();
        assert!(store.activate_owner_revision(&turn,&tampered,&next,&binding,dir.path(),5).is_err());
        let mut changed=previous.contract().clone();changed.revision=2;changed.criteria[0].requirement="Drop original behavior".into();
        let changed=RegisteredContract::register(dir.path(),changed,"a".repeat(40),"environment".into()).unwrap();
        let changed_binding=Binding {contract:changed.contract_hash().into(),..binding.clone()};
        assert!(store.activate_owner_revision(&turn,&previous,&changed,&changed_binding,dir.path(),5).is_err());
        std::fs::write(dir.path().join("verifier.rs"),"tampered verifier").unwrap();
        assert!(store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),5).is_err());
        std::fs::write(dir.path().join("verifier.rs"),"synthetic verifier fixture").unwrap();
        store.set_owner_pause(&turn.lease.workflow,"owner",turn.pause_revision,true,6).unwrap();
        assert!(store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),7).is_err());
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().checkpoint.binding,turn.binding);
    }
    #[test]
    fn owner_revision_replay_survives_reopen_and_cannot_change_its_binding() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),5).unwrap();drop(store);
        let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let revised=store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),6).unwrap();
        assert_eq!(store.owner_turn_for_lease(&revised.lease,7).unwrap().binding,revised.binding);
        let altered=Binding {evidence_hash:"changed".into(),..binding};assert!(store.activate_owner_revision(&turn,&previous,&next,&altered,dir.path(),8).is_err());
        assert_eq!(store.events(&turn.lease.workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="owner_contract_revised").count(),1);
    }

    #[test]
    fn owner_revision_fences_pre_activation_dispatch_callbacks() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let revised=store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),5).unwrap();
        assert!(store.validate_dispatch(&turn.lease,&binding,6).is_err(),"old guard callback can dispatch against the new contract");
        assert!(store.charge(&turn.lease,"old-native-call",ResourceCharge {model_calls:1,..Default::default()},6).is_err());
        store.validate_dispatch(&revised.lease,&binding,6).unwrap();
    }

}
