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
        let plan=Plan {version:1,kind:"original_verifier_plan".into(),workflow:workflow.id().into(),binding:workflow.binding().clone(),
            base:base.into(),environment:environment.into(),executor:executor.into(),proposal:proposal.clone()};
        let body=serde_json::to_string(&plan)?;
        let key=amendment::digest(&serde_json::to_vec(&("original-verifier-plan-v1",&body))?);
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
