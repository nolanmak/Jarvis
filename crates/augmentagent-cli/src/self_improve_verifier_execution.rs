//! Original verifier execution custody. Evidence data never grants a witness.
use super::*;

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version:u32,kind:String,workflow:String,binding:crate::autoship_workflow::Binding,
    base:String,environment:String,executor:String,proposal:ContractProposal,
}

#[derive(Debug)]
pub(super) struct OriginalExecutionFailed {pub(super) plan:String}
impl std::fmt::Display for OriginalExecutionFailed {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"original verifier operation requires recovery")}
}
impl std::error::Error for OriginalExecutionFailed {}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    version:u32,kind:String,workflow:String,plan:String,attempt:String,verifier:String,
    identity:serde_json::Value,
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Quiescence {
    version:u32,kind:String,workflow:String,admission_sha256:String,evidence:serde_json::Value,
}

#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedResult {
    version:u32,kind:String,workflow:String,plan:String,attempt:String,
    baseline:BaselineEvidence,
}

fn result_key(plan:&str,attempt:&str)->String {
    amendment::digest(&serde_json::to_vec(&("original-verifier-result-v1",plan,attempt)).unwrap())
}

fn plan_input(workflow:&workflow::WorkflowGuard,proposal:&ContractProposal,base:&str,environment:&str,executor:&str)->Result<(String,String)> {
    let plan=Plan {version:1,kind:"original_verifier_plan".into(),workflow:workflow.id().into(),binding:workflow.binding().clone(),
        base:base.into(),environment:environment.into(),executor:executor.into(),proposal:proposal.clone()};
    let body=serde_json::to_string(&plan)?;
    let key=amendment::digest(&serde_json::to_vec(&("original-verifier-plan-v1",&body))?);
    Ok((key,body))
}

fn command_key(plan:&str,attempt:&str,verifier:&str)->String {
    amendment::digest(&serde_json::to_vec(&("original-verifier-command-v1",plan,attempt,verifier)).unwrap())
}
fn quiet_key(command:&str)->String {
    amendment::digest(&serde_json::to_vec(&("original-verifier-command-quiescent-v1",command)).unwrap())
}

/// Borrows the existing fenced workflow; cannot be deserialized from model data.
#[derive(Clone)]
pub(super) struct OriginalExecution<'a> {
    workflow:&'a workflow::WorkflowGuard,
    plan:String,
    attempt:String,
    verifiers:Vec<String>,
    base:String,
    environment:String,
    contract:AcceptanceContract,
    files:BTreeMap<String,String>,
}

impl<'a> OriginalExecution<'a> {
    pub(super) fn begin(workflow:&'a workflow::WorkflowGuard,proposal:&ContractProposal,base:&str,environment:&str,executor:&str)->Result<Self> {
        let (key,body)=plan_input(workflow,proposal,base,environment,executor)?;
        workflow.record_verifier_evidence(&key,"original_verifier_plan",&body)?;
        Self::quiesce_previous(workflow).map_err(|e|e.context(OriginalExecutionFailed {plan:key.clone()}))?;
        let attempt=amendment::digest(uuid::Uuid::new_v4().as_bytes());
        let record=serde_json::json!({"version":1,"kind":"original_verifier_attempt","workflow":workflow.id(),"plan":key,"attempt":attempt});
        let attempt_key=amendment::digest(&serde_json::to_vec(&("original-verifier-attempt-v1",&key,&attempt))?);
        workflow.record_verifier_evidence(&attempt_key,"original_verifier_attempt",&record.to_string())?;
        Ok(Self {workflow,plan:key,attempt,verifiers:proposal.contract.verifiers.iter().map(|v|v.id.clone()).collect(),
            base:base.into(),environment:environment.into(),contract:proposal.contract.clone(),
            files:proposal.files.iter().map(|(path,body)|(path.clone(),amendment::digest(body.as_bytes()))).collect()})
    }

    /// Retain the actual executor result before semantic classification. A
    /// conclusive non-red observation must survive recovery too; it supplies
    /// correction feedback, never passing acceptance or a witness judgment.
    pub(super) fn retain_result(&self,root:&Path,baseline:&BaselineEvidence)->Result<()> {
        self.require_registration(root,&baseline.registration)?;
        baseline.registration.baseline_result(&baseline.receipts)?;
        Self::require_result_commands(self.workflow,&self.plan,&self.attempt,baseline)?;
        let record=RecordedResult {version:1,kind:"original_verifier_result".into(),workflow:self.workflow.id().into(),
            plan:self.plan.clone(),attempt:self.attempt.clone(),baseline:baseline.clone()};
        self.workflow.record_verifier_evidence(&result_key(&self.plan,&self.attempt),"original_verifier_result",&serde_json::to_string(&record)?)
    }

    fn require_result_commands(workflow:&workflow::WorkflowGuard,plan:&str,attempt:&str,baseline:&BaselineEvidence)->Result<()> {
        for receipt in &baseline.receipts {
            let key=command_key(plan,attempt,&receipt.verifier_id);
            let raw=workflow.verifier_evidence(&key,"original_verifier_command_admission")?.context("original result lacks command admission")?;
            let admitted:Admission=serde_json::from_str(&raw)?;
            ensure!(admitted.version==1 && admitted.kind=="original_verifier_command_admission" && admitted.workflow==workflow.id()
                && admitted.plan==plan && admitted.attempt==attempt && admitted.verifier==receipt.verifier_id,"result belongs to another command");
            let raw_proof=workflow.verifier_evidence(&quiet_key(&key),"original_verifier_command_quiescent")?.context("original result lacks command quiescence")?;
            let proof:Quiescence=serde_json::from_str(&raw_proof)?;
            ensure!(proof.version==1 && proof.kind=="original_verifier_command_quiescent" && proof.workflow==workflow.id()
                && proof.admission_sha256==amendment::digest(raw.as_bytes()) && proof.evidence["identity"]==admitted.identity
                && matches!(proof.evidence["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),"result lacks matching kernel quiescence");
        }
        Ok(())
    }

    pub(super) fn retained_result(root:&Path,workflow:&workflow::WorkflowGuard,proposal:&ContractProposal,base:&str,environment:&str,executor:&str)->Result<Option<(String,BaselineEvidence)>> {
        let (plan,body)=plan_input(workflow,proposal,base,environment,executor)?;
        (||->Result<Option<(String,BaselineEvidence)>> {
            let saved=workflow.verifier_evidence(&plan,"original_verifier_plan")?;
            if let Some(saved)=&saved {ensure!(saved==&body,"original verifier plan changed");}
            let mut cursor=0;
            let mut conclusive=None;
            loop {
                let rows=workflow.verifier_evidence_page("original_verifier_result",cursor)?;
                if rows.is_empty() {break;}
                for row in rows {
                    cursor=row.sequence;
                    let record:RecordedResult=serde_json::from_str(&row.body)?;
                    ensure!(record.version==1 && record.kind=="original_verifier_result" && record.workflow==workflow.id()
                        && result_key(&record.plan,&record.attempt)==row.key,"invalid original verifier result identity");
                    if record.plan!=plan {continue;}
                    ensure!(saved.is_some(),"original result lost its input plan");
                    install_harness(root,proposal)?;
                    let registered=RegisteredContract::register(root,proposal.contract.clone(),base.into(),environment.into())?;
                    ensure!(registered==record.baseline.registration,"retained result differs from original registration");
                    record.baseline.registration.baseline_result(&record.baseline.receipts)?;
                    Self::require_result_commands(workflow,&plan,&record.attempt,&record.baseline)?;
                    if record.baseline.receipts.iter().any(|receipt|receipt.result==crate::autoship_contract::CheckResult::InfrastructureFailure) {continue;}
                    if let Some((_,previous))=&conclusive {
                        ensure!(serde_json::to_vec(previous)?==serde_json::to_vec(&record.baseline)?,"original operation has conflicting conclusive results");
                    } else {conclusive=Some((row.key,record.baseline));}
                }
            }
            if conclusive.is_some() {Self::quiesce_previous(workflow)?;}
            Ok(conclusive)
        })().map_err(|error| {
            let error=if error.downcast_ref::<initial_repair::Hold>().is_some(){error}else{error.context(initial_repair::Hold::EvidenceUnknown)};
            error.context(OriginalExecutionFailed {plan})
        })
    }

    pub(super) fn require_registration(&self,root:&Path,registration:&RegisteredContract)->Result<()> {
        ensure!(registration.baseline_head()==self.base && registration.contract()==&self.contract
            && registration.binding(&self.base)?.environment_id==self.environment,"executor registration differs from the frozen original plan");
        registration.verify_integrity(root)?;
        for (path,hash) in &self.files {
            ensure!(amendment::digest(&std::fs::read(root.join(path))?)==*hash,"original adapter differs from its frozen plan");
        }
        Ok(())
    }

    /// Every earlier admitted command in this workflow must be settled, even
    /// if a new candidate binding creates a different measurement plan.
    fn quiesce_previous(workflow:&workflow::WorkflowGuard)->Result<()> {
        let mut cursor=0;
        loop {
            let rows=workflow.verifier_evidence_page("original_verifier_command_admission",cursor)
                .map_err(|error|error.context(initial_repair::Hold::EvidenceUnknown))?;
            if rows.is_empty() {return Ok(());}
            for row in rows {
                cursor=row.sequence;
                ensure!(row.body.len()<=65536,initial_repair::Hold::EvidenceUnknown);
                let admitted:Admission=serde_json::from_str(&row.body).map_err(|e|anyhow::Error::from(e).context(initial_repair::Hold::EvidenceUnknown))?;
                ensure!(admitted.version==1 && admitted.kind=="original_verifier_command_admission" && admitted.workflow==workflow.id()
                    && command_key(&admitted.plan,&admitted.attempt,&admitted.verifier)==row.key,initial_repair::Hold::EvidenceUnknown);
                let key=quiet_key(&row.key);
                if let Some(saved)=workflow.verifier_evidence(&key,"original_verifier_command_quiescent")? {
                    let proof:Quiescence=serde_json::from_str(&saved)?;
                    ensure!(proof.version==1 && proof.kind=="original_verifier_command_quiescent" && proof.workflow==workflow.id()
                        && proof.admission_sha256==amendment::digest(row.body.as_bytes()) && proof.evidence["identity"]==admitted.identity
                        && matches!(proof.evidence["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),initial_repair::Hold::EvidenceUnknown);
                }else {
                    let proof=augmentagent_channel_core::run_resources::verify_quiescent(&admitted.identity)
                        .map_err(|e|e.context(initial_repair::Hold::InfrastructureUnavailable))?;
                    Self::retain_quiescence(workflow,&row.key,&row.body,proof)?;
                }
            }
        }
    }

    pub(super) fn admitted(&self,verifier:&str,admission:&augmentagent_channel_core::run_resources::CommandAdmission)->Result<()> {
        self.retain_admission(verifier,admission.identity())
    }

    fn retain_admission(&self,verifier:&str,identity:&serde_json::Value)->Result<()> {
        ensure!(self.verifiers.iter().any(|id|id==verifier),"command is not in the original verifier plan");
        let record=Admission {version:1,kind:"original_verifier_command_admission".into(),workflow:self.workflow.id().into(),
            plan:self.plan.clone(),attempt:self.attempt.clone(),verifier:verifier.into(),identity:identity.clone()};
        let raw=serde_json::to_string(&record)?;
        ensure!(raw.len()<=65536,"verifier admission exceeds bound");
        self.workflow.record_verifier_evidence(&command_key(&self.plan,&self.attempt,verifier),"original_verifier_command_admission",&raw)
    }

    pub(super) fn quiescent(&self,verifier:&str,proof:&augmentagent_channel_core::run_resources::CommandQuiescence)->Result<()> {
        let key=command_key(&self.plan,&self.attempt,verifier);
        let raw=self.workflow.verifier_evidence(&key,"original_verifier_command_admission")?.context("original command admission missing")?;
        Self::retain_quiescence(self.workflow,&key,&raw,proof.evidence().clone())
    }

    fn retain_quiescence(workflow:&workflow::WorkflowGuard,key:&str,admission:&str,evidence:serde_json::Value)->Result<()> {
        let original:Admission=serde_json::from_str(admission)?;
        ensure!(evidence["identity"]==original.identity && matches!(evidence["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),"quiescence belongs to another command or lacks a kernel result");
        let record=Quiescence {version:1,kind:"original_verifier_command_quiescent".into(),workflow:workflow.id().into(),
            admission_sha256:amendment::digest(admission.as_bytes()),evidence};
        workflow.record_verifier_evidence(&quiet_key(key),"original_verifier_command_quiescent",&serde_json::to_string(&record)?)
    }

    pub(super) fn failed(&self,error:anyhow::Error)->anyhow::Error {
        let error=if matches!(error.downcast_ref::<augmentagent_channel_core::reasoner::ReasonerError>(),Some(augmentagent_channel_core::reasoner::ReasonerError::ResourceCapacity {..})) {
            error.context(initial_repair::Hold::InfrastructureUnavailable)
        }else {error};
        error.context(OriginalExecutionFailed {plan:self.plan.clone()})
    }
}

/// Execute only the protected original recipe. Qualification and correction
/// remain ordinary paid pipeline work after this inspection lease is released.
pub(in crate::self_improve) async fn recover_original_verifier(repo:&Path,control:&Path,workflow:&workflow::WorkflowGuard)->Result<bool> {
    let Some((key,body))=workflow.original_verifier_recovery_plan()? else {return Ok(false)};
    let plan:Plan=serde_json::from_str(&body).map_err(|e|anyhow::Error::from(e).context(initial_repair::Hold::EvidenceUnknown))?;
    ensure!(plan.version==1 && plan.kind=="original_verifier_plan" && plan.workflow==workflow.id()
        && plan.binding==*workflow.binding() && plan.executor==verifier_executor_id()?,initial_repair::Hold::EvidenceUnknown);
    let temporary=tempfile::Builder::new().prefix("original-verifier-recovery-").tempdir_in(control)?;
    let checkout=temporary.path().join("checkout");
    let path=checkout.to_str().context("original verifier checkout UTF8")?;
    trusted_git(repo,&["worktree","add","--detach",path,&plan.base]).await?;
    let recovered=async {
        ensure!(actual_head(&checkout).await?==plan.base && environment_id(&checkout)?==plan.environment,initial_repair::Hold::EvidenceUnknown);
        ensure!(plan_input(workflow,&plan.proposal,&plan.base,&plan.environment,&plan.executor)?.0==key,initial_repair::Hold::EvidenceUnknown);
        let result=retain_initial_measurement(&checkout,control,&plan.proposal,workflow,|original|
            register_and_reproduce_original(&checkout,plan.proposal.clone(),workflow.audit_logger(),original)).await;
        if let Err(error)=result {
            // An actual conclusive non-red result returns to the existing
            // coordinator, which records the original slot before correction.
            if !error.downcast_ref::<BaselineNotRed>().is_some_and(|failure|failure.kind.is_some()) {return Err(error);}
        }
        let (result,_)=OriginalExecution::retained_result(&checkout,workflow,&plan.proposal,&plan.base,&plan.environment,&plan.executor)?
            .context(initial_repair::Hold::EvidenceUnknown)?;
        workflow.complete_original_verifier_recovery(&key,&result).await?;
        Ok::<_,anyhow::Error>(true)
    }.await;
    // Verifiers consume a separate managed export. This staging worktree is
    // never their writable runtime or live command source.
    let cleanup=trusted_git(repo,&["worktree","remove","--force",path]).await;
    let recovered=recovered?;cleanup?;Ok(recovered)
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    use std::cell::Cell;

    async fn fixture()->(tempfile::TempDir,tempfile::TempDir,PreparedAcceptance,workflow::WorkflowGuard) {
        let (root,control,prepared)=baseline_witness::prepared_fixture();
        let guard=workflow::WorkflowGuard::acquire_fixture(&control.path().join("workflow.db"),workflow::Start {
            repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
            branch:"fixture",contract:"original scope",environment:"test",
        }).await.unwrap().unwrap();
        (root,control,prepared,guard)
    }

    // Script only historical admission metadata. The kernel checker is real,
    // but a nonexistent random scope is not evidence of a live command test.
    fn gone_scope()->serde_json::Value {
        use sha2::{Digest,Sha256};
        serde_json::json!({"host_sha256":format!("{:x}",Sha256::digest(std::fs::read("/etc/machine-id").unwrap())),
            "boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
            "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1,"limits":{"memory.max":67108864}})
    }

    #[tokio::test]
    async fn quiescent_original_without_result_cannot_start_another_attempt() {
        let (_root,_control,prepared,guard)=fixture().await;
        let original=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor").unwrap();
        original.retain_admission("observe",&gone_scope()).unwrap();
        OriginalExecution::quiesce_previous(&guard).unwrap();
        let retry=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor");
        let failure=retry.err().expect("quiescence was used as a result receipt and admitted another execution");
        assert_eq!(failure.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
        assert_eq!(guard.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1);
        drop(original);guard.finish().unwrap();
    }

    #[tokio::test]
    async fn retained_original_verifier_result_reenters_without_executing_again() {
        for reproduced in [true,false] {
            let (root,control,prepared,guard)=fixture().await;
            let base=prepared.baseline.registration.baseline_head();
            let environment=environment_id(root.path()).unwrap();
            let executor=amendment::digest(&serde_json::to_vec(&(amendment::digest(include_bytes!("autoship_contract.rs")),amendment::digest(include_bytes!("self_improve_contract.rs")))).unwrap());
            let original=OriginalExecution::begin(&guard,&prepared.proposal,base,&environment,&executor).unwrap();
            original.retain_admission("observe",&gone_scope()).unwrap();
            OriginalExecution::quiesce_previous(&guard).unwrap();
            let mut baseline=prepared.baseline.clone();
            if !reproduced {
                let observed=Observation {exit_code:Some(0),stdout:"true\n".into(),stderr:String::new(),interrupted:false,output_complete:true};
                baseline.receipts=vec![baseline.registration.observe(root.path(),"observe",baseline.registration.binding(base).unwrap(),&observed).unwrap()];
            }
            let raw=serde_json::json!({"version":1,"kind":"original_verifier_result","workflow":guard.id(),"plan":original.plan,"attempt":original.attempt,"baseline":baseline});
            let key=amendment::digest(&serde_json::to_vec(&("original-verifier-result-v1",&original.plan,&original.attempt)).unwrap());
            guard.record_verifier_evidence(&key,"original_verifier_result",&raw.to_string()).unwrap();
            let executed=Cell::new(false);
            let result=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {
                executed.set(true);Ok(prepared.baseline.clone())
            }).await;
            assert!(!executed.get(),"a retained conclusive original result launched the executor again");
            if reproduced {assert!(result.is_ok());}
            else {assert_eq!(result.unwrap_err().downcast_ref::<BaselineNotRed>().unwrap().kind,Some(initial_repair::FailureKind::AlreadyPasses));}
            assert_eq!(crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
            drop(original);guard.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn original_verifier_recovery_hands_off_only_conclusive_results_without_native_ownership() {
        for (reproduced,hold) in [(true,initial_repair::Hold::InfrastructureUnavailable),
            (false,initial_repair::Hold::InfrastructureUnavailable),(true,initial_repair::Hold::EvidenceUnknown)] {
            let (root,control,prepared,mut guard)=fixture().await;
            let database=control.path().join("workflow.db");
            let base=prepared.baseline.registration.baseline_head();
            let environment=environment_id(root.path()).unwrap();
            let original=OriginalExecution::begin(&guard,&prepared.proposal,base,&environment,&verifier_executor_id().unwrap()).unwrap();
            original.retain_admission("observe",&gone_scope()).unwrap();
            OriginalExecution::quiesce_previous(&guard).unwrap();
            let mut baseline=prepared.baseline.clone();
            if !reproduced {
                let observation=Observation {exit_code:Some(0),stdout:"true\n".into(),stderr:String::new(),interrupted:false,output_complete:true};
                baseline.receipts=vec![baseline.registration.observe(root.path(),"observe",baseline.registration.binding(base).unwrap(),&observation).unwrap()];
            }
            original.retain_result(root.path(),&baseline).unwrap();
            let plan=original.plan.clone();
            let error=original.failed(anyhow::Error::from(hold));
            drop(original);
            hold_initial_error(&mut guard,&error).await.unwrap().unwrap();
            let id=guard.id().to_owned();guard.finish().unwrap();
            let before=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
            let conn=rusqlite::Connection::open(&database).unwrap();
            let owners:i64=conn.query_row("SELECT count(*) FROM workflow_native_owner WHERE workflow=?1",[&id],|r|r.get(0)).unwrap();
            let guard=workflow::WorkflowGuard::acquire_verifier_recovery_fixture(&database,workflow::Start {
                repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
                branch:"fixture",contract:"original scope",environment:"test",
            }).await.unwrap().expect("original verifier inspection was not claimable");
            assert_eq!(guard.original_verifier_recovery_plan().unwrap().unwrap().0,plan);
            // Exercise the actual replayer against retained synthetic receipt
            // data. No live verifier or native-model execution is claimed here.
            conn.execute_batch("CREATE TRIGGER deny_recovered_result BEFORE INSERT ON workflow_event WHEN NEW.kind='original_verifier_result_recovered' BEGIN SELECT RAISE(ABORT,'recovery event unavailable'); END;").unwrap();
            assert!(recover_original_verifier(root.path(),control.path(),&guard).await.is_err());
            let failed=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
            assert_eq!(failed.checkpoint,before.checkpoint,"failed handoff partially released the original hold");
            conn.execute_batch("DROP TRIGGER deny_recovered_result;").unwrap();
            assert!(recover_original_verifier(root.path(),control.path(),&guard).await.unwrap());
            let after=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
            assert_eq!(after.attempts,before.attempts);assert_eq!(after.model_calls,before.model_calls);
            assert_eq!(after.checkpoint.binding,before.checkpoint.binding);
            assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
            assert_eq!(after.checkpoint.next.action,if reproduced {"qualify_original_measurement"}else{"correct_original_verifier"});
            assert_eq!(conn.query_row("SELECT count(*) FROM workflow_native_owner WHERE workflow=?1",[&id],|r|r.get::<_,i64>(0)).unwrap(),owners,"inspection allocated another native runtime owner");
            assert!(guard.audit_logger().model_call_admitted("codex").is_err(),"handoff enabled a model inside the inspection generation");
            assert!(guard.intent(crate::autoship_workflow::OperationKind::Push).is_err(),"handoff enabled publication inside the inspection generation");
            assert!(baseline_witness::assert_current(root.path(),control.path(),&prepared).is_err(),"raw result manufactured a qualification witness");
            guard.finish().unwrap();
        }
    }

    #[test]
    #[ignore = "requires isolated delegated service and no enclosing managed build lease; executor owns normal build admission"]
    fn original_verifier_recovery_executes_the_frozen_recipe_without_a_model() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let (root,control,prepared,mut guard)=fixture().await;
            let database=control.path().join("workflow.db");
            let original=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),
                &environment_id(root.path()).unwrap(),&verifier_executor_id().unwrap()).unwrap();
            // Inject an interruption after recipe retention but before command
            // admission. Recovery must execute this exact original proposal.
            let error=original.failed(anyhow::Error::from(initial_repair::Hold::InfrastructureUnavailable));
            drop(original);hold_initial_error(&mut guard,&error).await.unwrap().unwrap();
            let id=guard.id().to_owned();guard.finish().unwrap();
            let before=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
            let guard=workflow::WorkflowGuard::acquire_verifier_recovery_fixture(&database,workflow::Start {
                repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
                branch:"fixture",contract:"original scope",environment:"test",
            }).await.unwrap().unwrap();
            assert!(recover_original_verifier(root.path(),control.path(),&guard).await.unwrap());
            let after=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
            assert_eq!(after.attempts,before.attempts);assert_eq!(after.model_calls,0);
            assert_eq!(after.checkpoint.next.action,"qualify_original_measurement");
            assert_eq!(guard.verifier_evidence_page("original_verifier_command_admission",0).unwrap().len(),1);
            assert_eq!(guard.verifier_evidence_page("original_verifier_command_quiescent",0).unwrap().len(),1);
            let results=guard.verifier_evidence_page("original_verifier_result",0).unwrap();assert_eq!(results.len(),1);
            let actual:RecordedResult=serde_json::from_str(&results[0].body).unwrap();
            assert_eq!(actual.baseline.registration.baseline_result(&actual.baseline.receipts).unwrap(),BaselineResult::Reproduced);
            assert!(baseline_witness::assert_current(root.path(),control.path(),&prepared).is_err());
            guard.finish().unwrap();
        });
    }

    #[tokio::test]
    async fn verifier_command_custody_requires_the_frozen_registration_and_adapter() {
        let (root,_control,prepared,guard)=fixture().await;
        let base=prepared.baseline.registration.baseline_head();
        let environment=prepared.baseline.registration.binding(base).unwrap().environment_id;
        let original=OriginalExecution::begin(&guard,&prepared.proposal,base,&environment,"executor").unwrap();
        original.require_registration(root.path(),&prepared.baseline.registration).unwrap();
        let wrong_base=OriginalExecution::begin(&guard,&prepared.proposal,&"f".repeat(40),&environment,"executor").unwrap();
        assert!(wrong_base.require_registration(root.path(),&prepared.baseline.registration).is_err());
        let wrong_environment=OriginalExecution::begin(&guard,&prepared.proposal,base,"another-environment","executor").unwrap();
        assert!(wrong_environment.require_registration(root.path(),&prepared.baseline.registration).is_err());
        let mut changed=prepared.proposal.clone();
        changed.files.values_mut().next().unwrap().push_str("\nprint('different input')\n");
        let changed_adapter=OriginalExecution::begin(&guard,&changed,base,&environment,"executor").unwrap();
        assert!(changed_adapter.require_registration(root.path(),&prepared.baseline.registration).is_err(),"a valid registration for different adapter bytes was used");
        drop((original,wrong_base,wrong_environment,changed_adapter));guard.finish().unwrap();
    }

    #[tokio::test]
    async fn verifier_command_custody_settles_the_exact_prior_scope_and_preserves_its_input() {
        let (root,control,prepared,guard)=fixture().await;
        let first=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","original-executor").unwrap();
        let identity=gone_scope();first.retain_admission("observe",&identity).unwrap();
        let command=command_key(&first.plan,&first.attempt,"observe");
        assert!(first.retain_admission("foreign-verifier",&identity).is_err());
        let mut foreign=identity.clone();foreign["inode"]=2.into();
        assert!(first.retain_admission("observe",&foreign).is_err(),"original command admission was overwritten");
        let raw=guard.verifier_evidence(&command,"original_verifier_command_admission").unwrap().unwrap();
        assert!(OriginalExecution::retain_quiescence(&guard,&command,&raw,serde_json::json!({"identity":foreign,"verified_state":"removed_empty_subtree"})).is_err());
        let next=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","changed-executor").unwrap();
        assert_ne!(first.plan,next.plan,"fixture did not change the current plan");
        let proof:Quiescence=serde_json::from_str(&guard.verifier_evidence(&quiet_key(&command),"original_verifier_command_quiescent").unwrap().unwrap()).unwrap();
        assert_eq!(proof.evidence["identity"],identity);assert_eq!(proof.admission_sha256,amendment::digest(raw.as_bytes()));
        assert_eq!(crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        drop((first,next));guard.finish().unwrap();drop(root);
    }

    #[tokio::test]
    async fn verifier_command_custody_unknown_scope_blocks_the_real_executor_boundary() {
        let (root,control,prepared,guard)=fixture().await;
        let first=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","original-executor").unwrap();
        let mut identity=gone_scope();identity["host_sha256"]="another-host".into();
        first.retain_admission("observe",&identity).unwrap();
        let executed=Cell::new(false);
        let result=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {
            executed.set(true);Ok(prepared.baseline.clone())
        }).await;
        assert_eq!(result.unwrap_err().downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::InfrastructureUnavailable));
        assert!(!executed.get(),"a new recipe bypassed an unsettled original command");
        assert_eq!(guard.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1,"unknown scope admitted another measurement attempt");
        drop(first);guard.finish().unwrap();
    }

    #[tokio::test]
    async fn verifier_command_custody_rejects_lost_or_modified_original_admission() {
        for lost in [true,false] {
            let (root,control,prepared,guard)=fixture().await;
            let original=OriginalExecution::begin(&guard,&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor").unwrap();
            original.retain_admission("observe",&gone_scope()).unwrap();
            let key=command_key(&original.plan,&original.attempt,"observe");
            let conn=rusqlite::Connection::open(control.path().join("workflow.db")).unwrap();
            if lost {conn.execute("DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",rusqlite::params![guard.id(),key]).unwrap();}
            else {conn.execute("UPDATE scope_stage_response SET response='{}' WHERE workflow=?1 AND key=?2",rusqlite::params![guard.id(),key]).unwrap();}
            let executed=Cell::new(false);
            let result=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,|_original|async {executed.set(true);Ok(prepared.baseline.clone())}).await;
            assert_eq!(result.unwrap_err().downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
            assert!(!executed.get());drop(original);guard.finish().unwrap();
        }
    }
}
