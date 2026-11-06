//! Owner-request-bound activation of registered contract metadata.
use super::*;
use super::owner_turn::OwnerTurn;
use crate::autoship_contract::{RegisteredContract,VerifierReceipt,BaselineResult,AcceptanceSource};
#[path="autoship_owner_revision_history.rs"]
mod history;
pub(super) fn validate_history(conn:&Connection,workflow:&str)->Result<()> {history::validate(conn,workflow)}
/// Historical binding data only. Current turn/lease validation is separate;
/// absence never grants a legacy exception to productive owner coding.
pub(super) fn preparation_for_turn(conn:&Connection,turn:&owner_turn::OwnerTurn)->Result<Option<OwnerPreparationBinding>> {
    validate_history(conn,&turn.lease.workflow)?;
    let raw:Option<String>=conn.query_row("SELECT p.record FROM workflow_owner_revision r JOIN workflow_owner_preparation p ON p.request_id=r.request_id JOIN workflow_owner_turn t ON t.request_id=r.request_id WHERE r.request_id=?1 AND r.binding=?2 AND r.next_contract=?3 AND t.workflow=?4 AND t.owner=?5 AND t.lease_generation=?6 AND t.pause_revision=?7",
        params![turn.request_id,serde_json::to_string(&turn.binding)?,turn.binding.contract,turn.lease.workflow,turn.lease.owner,turn.lease.generation,turn.pause_revision],|r|r.get(0)).optional()?;
    raw.map(|raw|Ok(serde_json::from_str(&raw)?)).transpose()
}
pub(super) fn require_preparation(conn:&Connection,turn:&owner_turn::OwnerTurn,expected:&OwnerPreparationBinding)->Result<()> {
    ensure!(preparation_for_turn(conn,turn)?.as_ref()==Some(expected),"original owner preparation missing or changed");Ok(())
}
fn baseline_digest(receipts:&[VerifierReceipt])->Result<String> {
    Ok(format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(receipts)?)?)))
}
/// Binding data for the original protected preparation, never activation or
/// cleanup authority on its own. The owning guard reads and checks the file.
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OwnerPreparationBinding {
    version:u32,
    artifact_sha256:String,
    context_sha256:String,
    registration_sha256:String,
    baseline_receipts_sha256:String,
}
impl OwnerPreparationBinding {
    pub(crate) fn from_artifact(artifact_sha256:String,context_sha256:String,
        registration:&RegisteredContract,receipts:&[VerifierReceipt])->Result<Self> {
        let value=Self {version:1,artifact_sha256,context_sha256,
            registration_sha256:Self::registration_digest(registration)?,baseline_receipts_sha256:baseline_digest(receipts)?};
        value.validate(registration,receipts)?;Ok(value)
    }
    fn registration_digest(registration:&RegisteredContract)->Result<String> {
        Ok(format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(registration)?)?)))
    }
    fn validate(&self,registration:&RegisteredContract,receipts:&[VerifierReceipt])->Result<()> {
        registration.verify_metadata()?;
        ensure!(self.version==1 && [&self.artifact_sha256,&self.context_sha256,&self.registration_sha256,&self.baseline_receipts_sha256].into_iter()
            .all(|value|value.len()==64 && value.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),"invalid owner preparation binding");
        ensure!(self.registration_sha256==Self::registration_digest(registration)?
            && self.baseline_receipts_sha256==baseline_digest(receipts)?
            && registration.baseline_result(receipts)?==BaselineResult::Reproduced,"owner preparation registration or baseline changed");
        Ok(())
    }
}
enum VerifierAuthority<'a> {
    Private(&'a std::collections::BTreeMap<String,String>,&'a OwnerPreparationBinding,&'a [VerifierReceipt]),
    #[cfg(test)]
    Candidate(&'a Path),
}
impl WorkflowStore {
    /// Controller supplies an independently prepared registration. This records
    /// metadata activation only: baseline/candidate checks and review still gate shipping.
    #[cfg(test)]
    pub fn activate_owner_revision(&mut self,turn:&OwnerTurn,previous:&RegisteredContract,next:&RegisteredContract,binding:&Binding,root:&Path,now:i64)->Result<OwnerTurn> {
        self.activate_owner_contract(turn,Some(previous),next,None,binding,VerifierAuthority::Candidate(root),now)
    }
    /// Only the owning guard can attest that its current contract is still
    /// unregistered intake. Registration is not owner-input completion.
    #[cfg(test)]
    pub(crate) fn activate_initial_owner_acceptance(&mut self,proof:&crate::self_improve::InitialOwnerAcceptance,
        turn:&OwnerTurn,next:&RegisteredContract,baseline:&[VerifierReceipt],binding:&Binding,root:&Path,now:i64)->Result<OwnerTurn> {
        ensure!(proof.matches(turn),"initial owner acceptance guard changed");
        let request=self.command_request(&turn.lease.workflow)?.context("initial owner acceptance command missing")?;
        let expected=match next.contract().source()? {
            AcceptanceSource::Issue(n)=>workflow_id(&request.repository,n)?,
            AcceptanceSource::PullRequest(n)=>pr_workflow_id(&request.repository,n)?,
        };
        ensure!(expected==turn.lease.workflow,"initial owner acceptance source changed");
        self.activate_owner_contract(turn,None,next,Some(baseline),binding,VerifierAuthority::Candidate(root),now)
    }
    /// Trusted controller metadata only. Adapter authority comes from the
    /// protected registration map, never from worker files or ignored copies.
    pub(crate) fn activate_private_owner_acceptance(&mut self,proof:Option<&crate::self_improve::InitialOwnerAcceptance>,
        turn:&OwnerTurn,previous:Option<&RegisteredContract>,next:&RegisteredContract,baseline:&[VerifierReceipt],files:&std::collections::BTreeMap<String,String>,preparation:&OwnerPreparationBinding,binding:&Binding,now:i64)->Result<OwnerTurn> {
        ensure!(next.baseline_result(baseline)?==BaselineResult::Reproduced,"private owner acceptance lacks intended-red baseline");
        if previous.is_none() {
            ensure!(proof.context("initial owner acceptance guard missing")?.matches(turn),"initial owner acceptance guard changed");
            let request=self.command_request(&turn.lease.workflow)?.context("initial owner acceptance command missing")?;
            let expected=match next.contract().source()? {AcceptanceSource::Issue(n)=>workflow_id(&request.repository,n)?,AcceptanceSource::PullRequest(n)=>pr_workflow_id(&request.repository,n)?};
            ensure!(expected==turn.lease.workflow,"initial owner acceptance source changed");
        }else {ensure!(proof.is_none(),"revised owner acceptance cannot reuse intake proof");}
        self.activate_owner_contract(turn,previous,next,previous.is_none().then_some(baseline),binding,VerifierAuthority::Private(files,preparation,baseline),now)
    }
    fn activate_owner_contract(&mut self,turn:&OwnerTurn,previous:Option<&RegisteredContract>,next:&RegisteredContract,
        baseline:Option<&[VerifierReceipt]>,binding:&Binding,authority:VerifierAuthority<'_>,now:i64)->Result<OwnerTurn> {
        let preparation=match authority {VerifierAuthority::Private(files,preparation,receipts)=>{
                next.verify_files(files)?;preparation.validate(next,receipts)?;
                let raw=serde_json::to_string(receipts)?;ensure!(raw.len()<=4*1024*1024,"owner preparation baseline exceeds bound");
                Some((preparation.clone(),raw))
            },
            #[cfg(test)]
            VerifierAuthority::Candidate(root)=>{next.verify_integrity(root)?;None},
        };
        binding.validate()?;
        ensure!(binding.head==turn.binding.head && binding.contract==next.contract_hash(),"owner contract binding mismatch");
        let (old,kind)=if let Some(previous)=previous {
            previous.verify_metadata()?;
            ensure!(baseline.is_none() && previous.contract().source()?==next.contract().source()?
                && next.contract().revision==previous.contract().revision.checked_add(1).context("contract revision overflow")?,"owner contract source or revision mismatch");
            ensure!(previous.baseline_head()==next.baseline_head(),"owner revision changed the registered baseline");
            for prior in &previous.contract().criteria {ensure!(next.contract().criteria.iter().any(|criterion|criterion.id==prior.id && criterion.requirement==prior.requirement),"owner revision dropped or changed a prior requirement");}
            (previous.contract_hash(),"owner_contract_revised")
        }else{
            ensure!(next.contract().revision==1 && next.baseline_result(baseline.context("initial owner baseline missing")?)?==BaselineResult::Reproduced,
                "initial owner acceptance requires revision one and reproduced baseline");
            (turn.binding.contract.as_str(),"owner_acceptance_registered")
        };
        for scope in &next.contract().allowed_paths {
            let path=match scope {crate::autoship_contract::PathScope::File(path)|crate::autoship_contract::PathScope::Directory(path)=>path};
            ensure!(!crate::autoship_review::immutable_control_path(path),"owner contract touches protected controls");
            if let Some(previous)=previous {if matches!(scope,crate::autoship_contract::PathScope::Directory(_)) {ensure!(previous.contract().allowed_paths.contains(scope),"owner revision adds a broad directory scope");}}
        }
        let baseline_json=baseline.map(serde_json::to_string).transpose()?;
        ensure!(baseline_json.as_ref().is_none_or(|raw|raw.len()<=4*1024*1024),"initial owner baseline exceeds bound");
        let baseline_hash=baseline.map(baseline_digest).transpose()?;
        let registration=serde_json::to_string(next)?;ensure!(registration.len()<=4*1024*1024,"owner revision registration exceeds bound");
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        validate_history(&tx,&turn.lease.workflow)?;
        let body_hash=format!("{:x}",Sha256::digest(turn.body.as_bytes()));
        let expected_old=old;
        let prior:Option<(String,String,String,String)>=tx.query_row("SELECT previous_contract,next_contract,binding,owner_message_sha256 FROM workflow_owner_revision WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if let Some((old,new,recorded,body))=prior {
            ensure!(old==expected_old && new==next.contract_hash() && recorded==serde_json::to_string(binding)? && body==body_hash,"owner revision replay changed");
            let recorded_baseline:Option<String>=tx.query_row("SELECT receipts FROM workflow_owner_initial_acceptance WHERE request_id=?1",[&turn.request_id],|r|r.get(0)).optional()?;
            ensure!(recorded_baseline==baseline_json,"owner contract activation kind or baseline replay changed");
            let recorded_preparation:Option<(String,String)>=tx.query_row("SELECT record,receipts FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            ensure!(recorded_preparation==preparation.as_ref().map(|(binding,receipts)|serde_json::to_string(binding).map(|record|(record,receipts.clone()))).transpose()?,"owner preparation replay changed");
            let fence:Option<(i64,i64)>=tx.query_row("SELECT previous_generation,next_generation FROM workflow_owner_revision_fence WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let (before,after)=fence.context("unfenced legacy owner revision requires reconciliation")?;
            ensure!(turn.lease.generation==before || turn.lease.generation==after,"owner revision lease changed");
            let revised=OwnerTurn {lease:Lease {generation:after,..turn.lease.clone()},binding:binding.clone(),..turn.clone()};
            super::owner_turn::validate_turn(&tx,&revised,now)?;return Ok(revised);
        }
        super::owner_turn::validate_turn(&tx,turn,now)?;
        ensure!(turn.binding.contract==expected_old,"previous contract differs from claimed owner turn");
        tx.execute("INSERT INTO workflow_owner_revision VALUES(?1,?2,?3,?4,?5,?6,?7)",params![turn.request_id,expected_old,next.contract_hash(),serde_json::to_string(binding)?,body_hash,registration,now])?;
        if let Some((record,receipts))=&preparation {
            tx.execute("INSERT INTO workflow_owner_preparation VALUES(?1,?2,?3)",params![turn.request_id,serde_json::to_string(record)?,receipts])?;
        }
        if let Some(raw)=baseline_json {tx.execute("INSERT INTO workflow_owner_initial_acceptance VALUES(?1,?2)",params![turn.request_id,raw])?;}
        let generation=turn.lease.generation.checked_add(1).context("owner revision generation overflow")?;
        let revised=OwnerTurn {lease:Lease {generation,..turn.lease.clone()},binding:binding.clone(),..turn.clone()};
        tx.execute("INSERT INTO workflow_owner_revision_fence VALUES(?1,?2,?3)",params![turn.request_id,turn.lease.generation,generation])?;
        let checkpoint=Checkpoint {stage:Stage::Scope,binding:binding.clone(),evidence:Vec::new(),next:NextAction {reason:kind.into(),action:if baseline.is_some(){"verify_owner_acceptance".into()}else{"verify_owner_revision".into()},eligible_at_ms:now}};
        tx.execute("UPDATE workflow SET checkpoint=?2,progress_at=?3,generation=?4 WHERE id=?1",params![turn.lease.workflow,serde_json::to_string(&checkpoint)?,now,generation])?;
        tx.execute("UPDATE workflow_owner_turn SET binding=?2,lease_generation=?3 WHERE request_id=?1",params![turn.request_id,serde_json::to_string(binding)?,generation])?;
        super::review_epoch::invalidate(&tx,&turn.lease.workflow,&turn.request_id)?;
        let mut activation=serde_json::json!({"request_id":turn.request_id,"previous_contract":expected_old,"next_contract":next.contract_hash(),"revision":next.contract().revision,"owner_message_sha256":body_hash});
        if let Some(hash)=baseline_hash {activation["baseline_receipts_sha256"]=hash.into();}
        if let Some((record,_))=&preparation {activation["preparation"]=serde_json::to_value(record)?;}
        event(&tx,&turn.lease.workflow,generation,kind,&activation.to_string(),now)?;
        event(&tx,&turn.lease.workflow,generation,"checkpoint",&serde_json::to_string(&checkpoint)?,now)?;
        validate_history(&tx,&turn.lease.workflow)?;tx.commit()?;Ok(revised)
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
    // Synthetic baseline observations exercise metadata atomicity only.
    fn private_preparation(root:&Path,next:&RegisteredContract)->(Vec<VerifierReceipt>,std::collections::BTreeMap<String,String>,OwnerPreparationBinding) {
        let receipt=next.observe(root,"ready",next.binding(next.baseline_head()).unwrap(),&crate::autoship_contract::Observation {
            exit_code:Some(0),stdout:"false".into(),stderr:String::new(),interrupted:false,output_complete:true}).unwrap();
        let receipts=vec![receipt];
        let files=std::collections::BTreeMap::from([("verifier.rs".into(),"synthetic verifier fixture".into())]);
        let preparation=OwnerPreparationBinding::from_artifact("a".repeat(64),"b".repeat(64),next,&receipts).unwrap();
        (receipts,files,preparation)
    }
    #[test]
    fn owner_coding_admission_cannot_treat_missing_preparation_as_legacy_authority() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        let revised=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
        assert!(store.has_owner_acceptance_for_turn(&revised).unwrap());
        store.conn.execute("DELETE FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id]).unwrap();
        store.conn.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.preparation') WHERE workflow=?1 AND kind='owner_contract_revised'",[&turn.lease.workflow]).unwrap();
        drop(store);let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        // Legacy-shaped history remains inspectable, but cannot grant coding.
        store.validate_owner_turn(&revised,6).unwrap();
        let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
        assert!(!store.has_owner_acceptance_for_turn(&revised).unwrap(),"revision row granted coding after both preparation projections disappeared");
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
        assert_eq!(store.pending_owner_messages(&turn.lease.workflow).unwrap(),1);assert_eq!(before.model_calls,0);
    }
    #[test]
    fn prepared_owner_body_admission_and_charge_are_atomic_and_retain_original_binding() {
        for fault in ["intact","activation-missing","dispatch-downgrade","artifact","provider","pause","expired","audit"] {
            let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
            let (receipts,files,preparation)=private_preparation(dir.path(),&next);
            let turn=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
            let dispatch=store.begin_prepared_owner_body_dispatch(&turn,submission::Provider::Codex,&preparation,6).unwrap();
            let raw:String=store.conn.query_row("SELECT record FROM workflow_owner_body_dispatch WHERE request_id=?1",[&turn.request_id],|r|r.get(0)).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(&raw).unwrap()["preparation"],serde_json::to_value(&preparation).unwrap());
            let mut expected=preparation.clone();let mut provider=submission::Provider::Codex;let mut now=7;
            match fault {
                "activation-missing"=>{
                    store.conn.execute("DELETE FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id]).unwrap();
                    store.conn.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.preparation') WHERE workflow=?1 AND kind='owner_contract_revised'",[&turn.lease.workflow]).unwrap();
                },
                "dispatch-downgrade"=>{
                    store.conn.execute("UPDATE workflow_owner_body_dispatch SET record=json_remove(record,'$.preparation') WHERE request_id=?1",[&turn.request_id]).unwrap();
                    store.conn.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.preparation') WHERE workflow=?1 AND kind='owner_body_dispatch_started'",[&turn.lease.workflow]).unwrap();
                },
                "artifact"=>{let mut value=serde_json::to_value(&expected).unwrap();value["artifact_sha256"]="e".repeat(64).into();expected=serde_json::from_value(value).unwrap();},
                "provider"=>provider=submission::Provider::Claude,
                "pause"=>{store.set_owner_pause(&turn.lease.workflow,"owner",turn.pause_revision,true,7).unwrap();},
                "expired"=>now=1000,
                "audit"=>store.conn.execute_batch("CREATE TRIGGER deny_owner_model_admission BEFORE INSERT ON workflow_event WHEN NEW.kind='native_model_admitted' BEGIN SELECT RAISE(ABORT,'injected owner model admission failure'); END;").unwrap(),
                _=>{},
            }
            let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
            let result=store.charge_prepared_owner_model(&turn.lease,"owner-body-call",now,Some(("owner-body-fixture",&binding,1)),provider,&dispatch,&expected);
            if fault=="intact" {
                result.unwrap();store.charge_prepared_owner_model(&turn.lease,"owner-body-call",8,Some(("owner-body-fixture",&binding,1)),provider,&dispatch,&expected).unwrap();
                assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().model_calls,1);
                assert_eq!(store.conn.query_row("SELECT attempts FROM bounded_attempt WHERE workflow=?1",[&turn.lease.workflow],|r|r.get::<_,i64>(0)).unwrap(),1);
            }else {
                assert!(result.is_err(),"{fault}: invalid owner body charged a model");
                assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
                assert_eq!(store.conn.query_row("SELECT count(*) FROM bounded_attempt WHERE workflow=?1",[&turn.lease.workflow],|r|r.get::<_,i64>(0)).unwrap(),0);
                assert_eq!(store.conn.query_row("SELECT count(*) FROM resource_charge WHERE workflow=?1",[&turn.lease.workflow],|r|r.get::<_,i64>(0)).unwrap(),0);
                if fault=="audit" {
                    store.conn.execute_batch("DROP TRIGGER deny_owner_model_admission").unwrap();
                    store.charge_prepared_owner_model(&turn.lease,"owner-body-call",8,Some(("owner-body-fixture",&binding,1)),provider,&dispatch,&expected).unwrap();
                }
            }
            assert_eq!(store.pending_owner_messages(&turn.lease.workflow).unwrap(),1);
            assert_eq!(store.owner_control(&turn.lease.workflow).unwrap().unwrap().paused,fault=="pause");
            assert_eq!(store.merge_allowance(8,5).unwrap().confirmed,0);
        }
    }
    #[test]
    fn prepared_owner_model_admission_is_once_after_reopen_and_generic_paths_refuse() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        let turn=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
        let dispatch=store.begin_prepared_owner_body_dispatch(&turn,submission::Provider::Codex,&preparation,6).unwrap();
        store.charge_prepared_owner_model(&turn.lease,"first-body",7,None,submission::Provider::Codex,&dispatch,&preparation).unwrap();
        let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
        drop(store);let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        // Retain the actual in-process capability; reopening the database does
        // not manufacture another dispatch. Same-receipt metadata is idempotent.
        store.charge_prepared_owner_model(&turn.lease,"first-body",8,None,submission::Provider::Codex,&dispatch,&preparation).unwrap();
        assert!(store.charge_prepared_owner_model(&turn.lease,"second-body",8,None,submission::Provider::Codex,&dispatch,&preparation).is_err());
        assert!(store.charge_admitted_model(&turn.lease,"ordinary-bypass",8,None,None,Some(submission::Provider::Codex),false).is_err());
        assert!(store.charge(&turn.lease,"raw-bypass",ResourceCharge {model_calls:1,..Default::default()},8).is_err());
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
        assert_eq!(store.pending_owner_messages(&turn.lease.workflow).unwrap(),1);
        assert_eq!(store.merge_allowance(8,5).unwrap().confirmed,0);
    }
    #[test]
    fn prepared_owner_model_admission_projection_failure_rolls_back_all_charges() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        let turn=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
        let dispatch=store.begin_prepared_owner_body_dispatch(&turn,submission::Provider::Codex,&preparation,6).unwrap();
        let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
        store.conn.execute_batch("CREATE TRIGGER deny_body_slot BEFORE INSERT ON workflow_owner_body_model_admission BEGIN SELECT RAISE(ABORT,'injected body slot failure'); END;").unwrap();
        assert!(store.charge_prepared_owner_model(&turn.lease,"first-body",7,Some(("body-model",&binding,3)),submission::Provider::Codex,&dispatch,&preparation).is_err());
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
        for table in ["resource_charge","bounded_attempt","workflow_owner_body_model_admission"] {
            assert_eq!(store.conn.query_row(&format!("SELECT count(*) FROM {table} WHERE workflow=?1"),[&turn.lease.workflow],|row|row.get::<_,i64>(0)).unwrap(),0);
        }
        store.conn.execute_batch("DROP TRIGGER deny_body_slot").unwrap();
        store.charge_prepared_owner_model(&turn.lease,"first-body",8,Some(("body-model",&binding,3)),submission::Provider::Codex,&dispatch,&preparation).unwrap();
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().model_calls,1);
    }
    #[test]
    fn prepared_owner_model_admission_history_damage_cannot_release_another_call() {
        for fault in ["index","event","payload","charge","legacy-pair"] {
            let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
            let (receipts,files,preparation)=private_preparation(dir.path(),&next);
            let turn=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
            let dispatch=store.begin_prepared_owner_body_dispatch(&turn,submission::Provider::Codex,&preparation,6).unwrap();
            store.charge_prepared_owner_model(&turn.lease,"first-body",7,None,submission::Provider::Codex,&dispatch,&preparation).unwrap();
            match fault {
                "index"=>{store.conn.execute("DELETE FROM workflow_owner_body_model_admission",[]).unwrap();},
                "event"=>{store.conn.execute("UPDATE workflow_event SET kind='damaged-admission' WHERE kind='native_model_admitted'",[]).unwrap();},
                "payload"=>{store.conn.execute("UPDATE workflow_owner_body_model_admission SET record='{}'",[]).unwrap();},
                "charge"=>{store.conn.execute("UPDATE resource_charge SET model_calls=0 WHERE id='first-body'",[]).unwrap();},
                "legacy-pair"=>{
                    store.conn.execute("DELETE FROM workflow_owner_body_model_admission",[]).unwrap();
                    store.conn.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.owner_body') WHERE kind='native_model_admitted'",[]).unwrap();
                },
                _=>unreachable!(),
            }
            let before=store.events(&turn.lease.workflow).unwrap();
            assert!(store.charge_prepared_owner_model(&turn.lease,"second-body",8,None,submission::Provider::Codex,&dispatch,&preparation).is_err(),"{fault}: damaged history authorized replay");
            assert_eq!(store.events(&turn.lease.workflow).unwrap(),before);
        }
    }
    #[test]
    fn prepared_owner_model_admission_competing_connections_charge_only_one() {
        use std::sync::{Arc,Barrier};
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        let turn=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
        let dispatch=Arc::new(store.begin_prepared_owner_body_dispatch(&turn,submission::Provider::Codex,&preparation,6).unwrap());
        let peers=[WorkflowStore::open(&dir.path().join("state.db")).unwrap(),WorkflowStore::open(&dir.path().join("state.db")).unwrap()];
        let barrier=Arc::new(Barrier::new(2));
        let workers=peers.into_iter().enumerate().map(|(i,mut peer)| {
            let barrier=barrier.clone();let dispatch=dispatch.clone();let turn=turn.clone();let preparation=preparation.clone();
            std::thread::spawn(move || {barrier.wait();peer.charge_prepared_owner_model(&turn.lease,&format!("racing-body-{i}"),7,None,submission::Provider::Codex,&dispatch,&preparation).is_ok()})
        }).collect::<Vec<_>>();
        assert_eq!(workers.into_iter().map(|worker|worker.join().unwrap()).filter(|ok|*ok).count(),1);
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().model_calls,1);
        store.validate_owner_turn(&turn,8).unwrap();
    }
    #[test]
    fn private_owner_revision_retains_its_exact_preparation_and_revised_baseline_after_reopen() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        let revised=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
        drop(store);let mut store=WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        store.validate_owner_turn(&revised,6).unwrap();
        let (record,baseline):(String,String)=store.conn.query_row("SELECT record,receipts FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(serde_json::from_str::<OwnerPreparationBinding>(&record).unwrap(),preparation);
        assert_eq!(serde_json::from_str::<serde_json::Value>(&baseline).unwrap(),serde_json::to_value(&receipts).unwrap());
        assert_eq!(store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,7).unwrap().binding,revised.binding);
        assert_eq!(store.conn.query_row("SELECT count(*) FROM workflow_owner_initial_acceptance",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(store.events(&turn.lease.workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="owner_contract_revised").count(),1);
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().model_calls,0);
    }
    #[test]
    fn private_owner_revision_replay_cannot_rebind_changed_artifact_context_or_valid_baseline() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
        let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
        for field in ["artifact_sha256","context_sha256"] {
            let mut changed=serde_json::to_value(&preparation).unwrap();changed[field]="e".repeat(64).into();
            let changed:OwnerPreparationBinding=serde_json::from_value(changed).unwrap();
            assert!(store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&changed,&binding,6).is_err(),"{field} changed on replay");
        }
        let changed=vec![next.observe(dir.path(),"ready",next.binding(next.baseline_head()).unwrap(),&crate::autoship_contract::Observation {
            exit_code:Some(0),stdout:"false\n".into(),stderr:String::new(),interrupted:false,output_complete:true}).unwrap()];
        let changed_preparation=OwnerPreparationBinding::from_artifact("a".repeat(64),"b".repeat(64),&next,&changed).unwrap();
        assert_ne!(preparation,changed_preparation);
        assert!(store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&changed,&files,&changed_preparation,&binding,6).is_err(),"a different valid red replaced the original activation baseline");
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
    }
    #[test]
    fn private_owner_preparation_history_damage_fences_dispatch_without_repair() {
        for corruption in ["missing","projection","event","baseline","orphan","version","unknown"] {
            let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
            let (receipts,files,preparation)=private_preparation(dir.path(),&next);
            let revised=store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).unwrap();
            match corruption {
                "missing"=>{store.conn.execute("DELETE FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "projection"=>{store.conn.execute("UPDATE workflow_owner_preparation SET record=json_set(record,'$.artifact_sha256',?2) WHERE request_id=?1",params![turn.request_id,"e".repeat(64)]).unwrap();},
                "event"=>{store.conn.execute("UPDATE workflow_event SET payload=json_set(payload,'$.preparation.context_sha256',?2) WHERE workflow=?1 AND kind='owner_contract_revised'",params![turn.lease.workflow,"e".repeat(64)]).unwrap();},
                "baseline"=>{store.conn.execute("UPDATE workflow_owner_preparation SET receipts='[]' WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "orphan"=>{store.conn.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.preparation') WHERE workflow=?1 AND kind='owner_contract_revised'",[&turn.lease.workflow]).unwrap();},
                "version"=>{store.conn.execute("UPDATE workflow_owner_preparation SET record=json_set(record,'$.version',2) WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "unknown"=>{store.conn.execute("UPDATE workflow_owner_preparation SET record=json_set(record,'$.unexpected',1) WHERE request_id=?1",[&turn.request_id]).unwrap();},
                _=>unreachable!(),
            }
            let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
            assert!(store.validate_owner_turn(&revised,6).is_err(),"{corruption}");
            assert!(store.charge(&revised.lease,"must-not-spend",ResourceCharge {model_calls:1,..Default::default()},6).is_err(),"{corruption}");
            assert!(store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,6).is_err(),"{corruption}");
            assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
            assert_eq!(store.pending_owner_messages(&turn.lease.workflow).unwrap(),1);assert!(!store.owner_control(&turn.lease.workflow).unwrap().unwrap().paused);
        }
    }
    #[test]
    fn private_owner_preparation_rolls_back_with_failed_activation_audit() {
        let dir=tempfile::tempdir().unwrap();let (mut store,turn,previous,next,binding)=setup(dir.path());
        let (receipts,files,preparation)=private_preparation(dir.path(),&next);
        store.conn.execute_batch("CREATE TRIGGER deny_private_revision BEFORE INSERT ON workflow_event WHEN NEW.kind='owner_contract_revised' BEGIN SELECT RAISE(ABORT,'injected audit failure'); END;").unwrap();
        assert!(store.activate_private_owner_acceptance(None,&turn,Some(&previous),&next,&receipts,&files,&preparation,&binding,5).is_err());
        assert_eq!(store.conn.query_row("SELECT count(*) FROM workflow_owner_preparation",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap().checkpoint.binding,turn.binding);store.validate_owner_turn(&turn,6).unwrap();
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

    #[test]
    fn owner_revision_dispatch_requires_intact_activation_history() {
        for corruption in ["projection","fence","event","registration","binding","fence-generation","orphan-event","claim","origin","all-history"] {
            let dir=tempfile::tempdir().unwrap();let path=dir.path().join("state.db");
            let (mut store,turn,previous,next,binding)=setup(dir.path());
            let revised=store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),5).unwrap();
            store.validate_dispatch(&revised.lease,&binding,6).unwrap();
            // Simulate lost/corrupt protected state through a separate connection,
            // not through a new production editing or recovery interface.
            let db=rusqlite::Connection::open(&path).unwrap();db.pragma_update(None,"foreign_keys",false).unwrap();
            match corruption {
                "projection"=>{db.execute("DELETE FROM workflow_owner_revision WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "fence"=>{db.execute("DELETE FROM workflow_owner_revision_fence WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "event"=>{db.execute("DELETE FROM workflow_event WHERE workflow=?1 AND kind='owner_contract_revised'",[&turn.lease.workflow]).unwrap();},
                "registration"=>{db.execute("UPDATE workflow_owner_revision SET registration=json_set(registration,'$.contract.criteria[0].requirement','changed') WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "binding"=>{db.execute("UPDATE workflow_owner_revision SET binding=json_set(binding,'$.unexpected_scope','unrecognized') WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "fence-generation"=>{db.execute("UPDATE workflow_owner_revision_fence SET next_generation=next_generation+1 WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "orphan-event"=>{db.execute("DELETE FROM workflow_owner_revision_fence WHERE request_id=?1",[&turn.request_id]).unwrap();db.execute("DELETE FROM workflow_owner_revision WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "claim"=>{db.execute("DELETE FROM workflow_event WHERE workflow=?1 AND kind='owner_turn_claimed'",[&turn.lease.workflow]).unwrap();},
                "origin"=>{db.execute("DELETE FROM discord_control_request WHERE request_id=?1",[&turn.request_id]).unwrap();},
                "all-history"=>{db.execute("DELETE FROM workflow_owner_revision_fence WHERE request_id=?1",[&turn.request_id]).unwrap();db.execute("DELETE FROM workflow_owner_revision WHERE request_id=?1",[&turn.request_id]).unwrap();db.execute("DELETE FROM workflow_event WHERE workflow=?1 AND kind='owner_contract_revised'",[&turn.lease.workflow]).unwrap();},
                _=>unreachable!(),
            }
            drop(db);drop(store);
            let mut store=WorkflowStore::open(&path).unwrap();
            let before=store.snapshot(&turn.lease.workflow).unwrap().unwrap();let events=store.events(&turn.lease.workflow).unwrap();
            assert!(store.validate_owner_turn(&revised,7).is_err(),"{corruption}: owner turn accepted damaged activation history");
            assert!(store.validate_dispatch(&revised.lease,&binding,7).is_err(),"{corruption}: native dispatch accepted damaged activation history");
            assert!(store.charge(&revised.lease,"must-not-admit",ResourceCharge {model_calls:1,..Default::default()},7).is_err(),"{corruption}: charged a model admission");
            assert!(store.activate_owner_revision(&turn,&previous,&next,&binding,dir.path(),7).is_err(),"{corruption}: replay accepted damaged activation history");
            assert_eq!(store.pending_owner_messages(&turn.lease.workflow).unwrap(),1);assert!(!store.owner_control(&turn.lease.workflow).unwrap().unwrap().paused);
            assert_eq!(store.snapshot(&turn.lease.workflow).unwrap().unwrap(),before);assert_eq!(store.events(&turn.lease.workflow).unwrap(),events);
        }
    }

}
