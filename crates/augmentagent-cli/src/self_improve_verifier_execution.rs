//! Original verifier execution custody. Evidence data never grants a witness.
use super::*;
#[path="self_improve_verifier_capture.rs"]
mod capture;

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
    #[serde(default,skip_serializing_if="Option::is_none")]
    capture:Option<capture::Capture>,
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
    #[serde(default,skip_serializing_if="BTreeMap::is_empty")]
    command_sources:BTreeMap<String,String>,
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

/// This reference is checked against the protected original plan by the hold
/// transaction. It retains recovery after a completed result cannot be indexed;
/// deriving its identity neither creates a plan nor permits another execution.
pub(super) fn result_retention_failure(workflow:&workflow::WorkflowGuard,proposal:&ContractProposal,
    base:&str,environment:&str,executor:&str,error:anyhow::Error)->anyhow::Error {
    let error=error.context(initial_repair::Hold::EvidenceUnknown);
    match plan_input(workflow,proposal,base,environment,executor) {
        Ok((plan,_))=>error.context(OriginalExecutionFailed {plan}),
        Err(_)=>error,
    }
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
    control:PathBuf,
    command_sources:BTreeMap<String,String>,
}

impl<'a> OriginalExecution<'a> {
    pub(super) fn begin(workflow:&'a workflow::WorkflowGuard,control:&Path,proposal:&ContractProposal,base:&str,environment:&str,executor:&str)->Result<Self> {
        let (key,body)=plan_input(workflow,proposal,base,environment,executor)?;
        workflow.record_verifier_evidence(&key,"original_verifier_plan",&body)?;
        let (resume,mut command_sources)=Self::quiesce_previous(workflow).and_then(|_|Self::settle_previous_results(workflow,&key))
            .map_err(|error| {
                let error=if error.downcast_ref::<initial_repair::Hold>().is_some(){error}else{error.context(initial_repair::Hold::EvidenceUnknown)};
                error.context(OriginalExecutionFailed {plan:key.clone()})
            })?;
        let attempt=if let Some(attempt)=resume {attempt}else {
            let attempt=amendment::digest(uuid::Uuid::new_v4().as_bytes());
            let record=serde_json::json!({"version":1,"kind":"original_verifier_attempt","workflow":workflow.id(),"plan":key,"attempt":attempt});
            let attempt_key=amendment::digest(&serde_json::to_vec(&("original-verifier-attempt-v1",&key,&attempt))?);
            workflow.record_verifier_evidence(&attempt_key,"original_verifier_attempt",&record.to_string())?;attempt
        };
        command_sources.retain(|_,source|source!=&attempt);
        Ok(Self {workflow,plan:key,attempt,verifiers:proposal.contract.verifiers.iter().map(|v|v.id.clone()).collect(),
            base:base.into(),environment:environment.into(),contract:proposal.contract.clone(),
            files:proposal.files.iter().map(|(path,body)|(path.clone(),amendment::digest(body.as_bytes()))).collect(),control:control.canonicalize()?,command_sources})
    }

    fn settle_previous_results(workflow:&workflow::WorkflowGuard,current_plan:&str)->Result<(Option<String>,BTreeMap<String,String>)> {
        let mut cursor=0;let mut unfinished:BTreeMap<(String,String),Vec<(String,bool,String)>>=BTreeMap::new();
        let mut completed=BTreeMap::new();
        loop {
            let rows=workflow.verifier_evidence_page("original_verifier_command_admission",cursor)?;
            if rows.is_empty(){break;}
            for row in rows {
                cursor=row.sequence;
                let admitted:Admission=serde_json::from_str(&row.body)?;
                if let Some(saved)=workflow.verifier_evidence(&result_key(&admitted.plan,&admitted.attempt),"original_verifier_result")? {
                    let result:RecordedResult=serde_json::from_str(&saved)?;
                    ensure!(result.version==1 && result.kind=="original_verifier_result" && result.workflow==workflow.id()
                        && result.plan==admitted.plan && result.attempt==admitted.attempt,"original result identity changed");
                    result.baseline.registration.baseline_result(&result.baseline.receipts)?;
                    Self::require_result_commands(workflow,&admitted.plan,&admitted.attempt,&result.baseline,&result.command_sources)?;
                    ensure!(result.baseline.receipts.iter().any(|r|r.verifier_id==admitted.verifier),"original result omitted an admitted command");
                    if !result.baseline.receipts.iter().any(|receipt|receipt.result==crate::autoship_contract::CheckResult::InfrastructureFailure) {continue;}
                }
                let captured=admitted.capture.as_ref().context("original command result was not retained")?
                    .read(&row.key,&admitted.identity)?.context("original command result is not yet published")?;
                // Stream bytes have already been authenticated. Settlement
                // needs only terminal status and the manifest reference.
                let interrupted=captured.observation.interrupted || !captured.observation.output_complete;
                if admitted.plan==current_plan && !interrupted {
                    let previous=completed.insert(admitted.verifier.clone(),admitted.attempt.clone());
                    ensure!(previous.is_none_or(|source|source==admitted.attempt),"multiple completed commands for one original verifier");
                }
                unfinished.entry((admitted.plan,admitted.attempt)).or_default().push((row.key,interrupted,captured.manifest_sha256));
            }
        }
        let mut resume=None;
        for ((plan,attempt),commands) in unfinished {
            if commands.iter().any(|(_,interrupted,_)|*interrupted) {
                let key=amendment::digest(&serde_json::to_vec(&("original-verifier-attempt-interrupted-v1",&plan,&attempt))?);
                let references:Vec<_>=commands.iter().map(|(key,_,manifest)|serde_json::json!({"command":key,"manifest_sha256":manifest})).collect();
                workflow.record_verifier_evidence(&key,"original_verifier_attempt_interrupted",&serde_json::json!({"version":1,
                    "kind":"original_verifier_attempt_interrupted","workflow":workflow.id(),"plan":plan,"attempt":attempt,"commands":references}).to_string())?;
            } else if plan==current_plan {
                ensure!(resume.is_none(),"multiple original attempts have unreconciled results");resume=Some(attempt);
            }
        }
        Ok((resume,completed))
    }

    fn prepare_capture(&self,verifier:&str)->Result<capture::Capture> {
        ensure!(self.verifiers.iter().any(|id|id==verifier),"capture is not in the original verifier plan");
        ensure!(!self.command_sources.contains_key(verifier),"completed original verifier cannot admit another command");
        capture::Capture::prepare(&self.control,self.workflow.publication_inputs().1,&command_key(&self.plan,&self.attempt,verifier))
    }

    pub(super) fn capture_path(&self,verifier:&str)->Result<PathBuf> {
        self.prepare_capture(verifier).map(|capture|capture.path().to_owned())
            .map_err(|error|error.context(initial_repair::Hold::InfrastructureUnavailable))
    }

    pub(super) fn captured_observation(&self,verifier:&str)->Result<Option<Observation>> {
        self.read_captured_observation(verifier).map_err(|error|error.context(initial_repair::Hold::EvidenceUnknown))
    }

    fn read_captured_observation(&self,verifier:&str)->Result<Option<Observation>> {
        let attempt=self.command_sources.get(verifier).unwrap_or(&self.attempt);
        let key=command_key(&self.plan,attempt,verifier);
        let Some(raw)=self.workflow.verifier_evidence(&key,"original_verifier_command_admission")? else {return Ok(None)};
        let admitted:Admission=serde_json::from_str(&raw)?;
        ensure!(admitted.workflow==self.workflow.id() && admitted.plan==self.plan && admitted.attempt==*attempt && admitted.verifier==verifier,"original capture belongs to another operation");
        let proof=self.workflow.verifier_evidence(&quiet_key(&key),"original_verifier_command_quiescent")?.context("original result has no quiescence proof")?;
        let proof:Quiescence=serde_json::from_str(&proof)?;
        ensure!(proof.version==1 && proof.kind=="original_verifier_command_quiescent" && proof.workflow==self.workflow.id()
            && proof.admission_sha256==amendment::digest(raw.as_bytes()) && proof.evidence["identity"]==admitted.identity
            && matches!(proof.evidence["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),"original capture quiescence changed");
        let result=admitted.capture.context("original command lacks durable output")?.read(&key,&admitted.identity)?
            .context("original command output unavailable")?;
        if attempt!=&self.attempt {ensure!(!result.observation.interrupted && result.observation.output_complete,"reused original command is not complete");}
        Ok(Some(result.observation))
    }

    /// Retain the actual executor result before semantic classification. A
    /// conclusive non-red observation must survive recovery too; it supplies
    /// correction feedback, never passing acceptance or a witness judgment.
    pub(super) fn retain_result(&self,root:&Path,baseline:&BaselineEvidence)->Result<()> {
        self.require_registration(root,&baseline.registration)?;
        baseline.registration.baseline_result(&baseline.receipts)?;
        Self::require_result_commands(self.workflow,&self.plan,&self.attempt,baseline,&self.command_sources)?;
        let record=RecordedResult {version:1,kind:"original_verifier_result".into(),workflow:self.workflow.id().into(),
            plan:self.plan.clone(),attempt:self.attempt.clone(),baseline:baseline.clone(),command_sources:self.command_sources.clone()};
        self.workflow.record_verifier_evidence(&result_key(&self.plan,&self.attempt),"original_verifier_result",&serde_json::to_string(&record)?)
    }

    fn require_result_commands(workflow:&workflow::WorkflowGuard,plan:&str,attempt:&str,baseline:&BaselineEvidence,sources:&BTreeMap<String,String>)->Result<()> {
        Self::require_result_commands_read(workflow.id(),|key,kind|workflow.verifier_evidence(key,kind),plan,attempt,baseline,sources)
    }

    fn require_result_commands_read<F>(workflow:&str,mut read:F,plan:&str,attempt:&str,baseline:&BaselineEvidence,sources:&BTreeMap<String,String>)->Result<()>
    where F:FnMut(&str,&str)->Result<Option<String>> {
        ensure!(sources.keys().all(|id|baseline.receipts.iter().any(|receipt|&receipt.verifier_id==id)),"result references an unrelated original verifier");
        for receipt in &baseline.receipts {
            let source=sources.get(&receipt.verifier_id).map_or(attempt,String::as_str);
            if source!=attempt {ensure!(source.len()==64 && source.bytes().all(|byte|byte.is_ascii_hexdigit()),"invalid original command attempt reference");}
            let key=command_key(plan,source,&receipt.verifier_id);
            let raw=read(&key,"original_verifier_command_admission")?.context("original result lacks command admission")?;
            let admitted:Admission=serde_json::from_str(&raw)?;
            ensure!(admitted.version==1 && admitted.kind=="original_verifier_command_admission" && admitted.workflow==workflow
                && admitted.plan==plan && admitted.attempt==source && admitted.verifier==receipt.verifier_id,"result belongs to another command");
            let raw_proof=read(&quiet_key(&key),"original_verifier_command_quiescent")?.context("original result lacks command quiescence")?;
            let proof:Quiescence=serde_json::from_str(&raw_proof)?;
            ensure!(proof.version==1 && proof.kind=="original_verifier_command_quiescent" && proof.workflow==workflow
                && proof.admission_sha256==amendment::digest(raw.as_bytes()) && proof.evidence["identity"]==admitted.identity
                && matches!(proof.evidence["verified_state"].as_str(),Some("host_rebooted"|"removed_empty_subtree"|"kernel_empty_subtree_removed")),"result lacks matching kernel quiescence");
            if source!=attempt {
                let captured=admitted.capture.context("reused command lacks original capture")?.read(&key,&admitted.identity)?.context("reused command output unavailable")?;
                ensure!(!captured.observation.interrupted && captured.observation.output_complete
                    && amendment::digest(format!("{}\n{}",captured.observation.stdout,captured.observation.stderr).as_bytes())==receipt.output_digest,
                    "reused command differs from its original complete observation");
            }
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
                    record.baseline.registration.baseline_result(&record.baseline.receipts)?;
                    Self::require_result_commands(workflow,&plan,&record.attempt,&record.baseline,&record.command_sources)?;
                    if record.baseline.receipts.iter().any(|receipt|receipt.result==crate::autoship_contract::CheckResult::InfrastructureFailure) {continue;}
                    install_harness(root,proposal)?;
                    let registered=RegisteredContract::register(root,proposal.contract.clone(),base.into(),environment.into())?;
                    ensure!(registered==record.baseline.registration,"retained result differs from original registration");
                    if let Some((_,previous))=&conclusive {
                        ensure!(serde_json::to_vec(previous)?==serde_json::to_vec(&record.baseline)?,"original operation has conflicting conclusive results");
                    } else {conclusive=Some((row.key,record.baseline));}
                }
            }
            if conclusive.is_some() {Self::quiesce_previous(workflow)?;Self::settle_previous_results(workflow,&plan)?;}
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
        self.retain_admission_with_capture(verifier,admission.identity(),Some(self.prepare_capture(verifier)?))
    }

    fn retain_admission(&self,verifier:&str,identity:&serde_json::Value)->Result<()> {
        self.retain_admission_with_capture(verifier,identity,None)
    }

    fn retain_admission_with_capture(&self,verifier:&str,identity:&serde_json::Value,capture:Option<capture::Capture>)->Result<()> {
        ensure!(self.verifiers.iter().any(|id|id==verifier),"command is not in the original verifier plan");
        ensure!(!self.command_sources.contains_key(verifier),"completed original verifier cannot admit another command");
        let record=Admission {version:1,kind:"original_verifier_command_admission".into(),workflow:self.workflow.id().into(),
            plan:self.plan.clone(),attempt:self.attempt.clone(),verifier:verifier.into(),identity:identity.clone(),capture};
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
        let error=if error.downcast_ref::<initial_repair::Hold>().is_some() || error.downcast_ref::<BaselineNotRed>().is_some() {
            error
        }else if error.downcast_ref::<std::io::Error>().is_some()
            || matches!(error.downcast_ref::<augmentagent_channel_core::reasoner::ReasonerError>(),Some(augmentagent_channel_core::reasoner::ReasonerError::ResourceCapacity {..})) {
            error.context(initial_repair::Hold::InfrastructureUnavailable)
        }else {error.context(initial_repair::Hold::EvidenceUnknown)};
        error.context(OriginalExecutionFailed {plan:self.plan.clone()})
    }
}

/// Every retained original plan must have a matching accounting context before
/// a missing-scope frontend may start fresh work for a different trusted input.
pub(super) fn retained_original_plan_budget_contexts(workflow:&workflow::WorkflowGuard)->Result<std::collections::BTreeSet<String>> {
    let mut cursor=0;let mut contexts=std::collections::BTreeSet::new();let mut count=0usize;
    loop {
        let rows=workflow.verifier_evidence_page("original_verifier_plan",cursor)?;
        if rows.is_empty() {return Ok(contexts);}
        for row in rows {
            count+=1;ensure!(count<=8192,"original plan accounting exceeds inspection bound");cursor=row.sequence;
            ensure!(row.body.len()<=2*1024*1024,"original plan exceeds evidence envelope");
            let plan:Plan=serde_json::from_str(&row.body)?;
            ensure!(plan.version==1 && plan.kind=="original_verifier_plan" && plan.workflow==workflow.id()
                && !plan.executor.is_empty() && amendment::digest(&serde_json::to_vec(&("original-verifier-plan-v1",&row.body))?)==row.key,
                "original plan accounting identity changed");
            contexts.insert(initial_plan_budget_context(workflow.id(),&plan.binding.contract,plan.proposal.contract.source()?,&plan.base,&plan.environment)?);
        }
    }
}

/// Read original data without claiming, replaying, installing or remeasuring.
pub(super) fn retained_original_prepared(root:&Path,source:AcceptanceSource,
    binding:&crate::autoship_workflow::Binding,evidence:&crate::autoship_workflow::VerifierEvidenceInspection<'_>)
    ->Result<Vec<PreparedAcceptance>> {
    let mut cursor=0;let mut prepared=Vec::new();
    loop {
        let rows=evidence.page("original_verifier_result",cursor)?;
        if rows.is_empty() {break;}
        for row in rows {
            cursor=row.sequence;
            let result:RecordedResult=serde_json::from_str(&row.body)?;
            ensure!(result.version==1 && result.kind=="original_verifier_result" && result.workflow==evidence.workflow()
                && result_key(&result.plan,&result.attempt)==row.key,"retained original result identity changed");
            let body=evidence.record(&result.plan,"original_verifier_plan")?.context("retained original result lost its plan")?;
            ensure!(amendment::digest(&serde_json::to_vec(&("original-verifier-plan-v1",&body))?)==result.plan,"retained original plan identity changed");
            let plan:Plan=serde_json::from_str(&body)?;
            ensure!(plan.version==1 && plan.kind=="original_verifier_plan" && plan.workflow==evidence.workflow() && !plan.executor.is_empty(),"retained original plan owner or role changed");
            if plan.binding!=*binding || plan.proposal.contract.source()?!=source {continue;}
            let registration=&result.baseline.registration;
            ensure!(registration.contract()==&plan.proposal.contract && registration.baseline_head()==plan.base
                && registration.binding(&plan.base)?.environment_id==plan.environment,"retained original result differs from frozen plan");
            registration.verify_source_files(root,&plan.proposal.files)?;
            OriginalExecution::require_result_commands_read(evidence.workflow(),|key,kind|evidence.record(key,kind),
                &result.plan,&result.attempt,&result.baseline,&result.command_sources)?;
            if registration.baseline_result(&result.baseline.receipts)?!=BaselineResult::Reproduced {continue;}
            prepared.push(PreparedAcceptance {proposal:plan.proposal,baseline:result.baseline});
        }
    }
    Ok(prepared)
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
pub(super) fn retain_scripted_original_data(root:&Path,control:&Path,prepared:&PreparedAcceptance,workflow:&workflow::WorkflowGuard)->Result<()> {
    let original=OriginalExecution::begin(workflow,control,&prepared.proposal,prepared.baseline.registration.baseline_head(),&environment_id(root)?,"scripted old executor")?;
    for verifier in &prepared.proposal.contract.verifiers {original.retain_admission(&verifier.id,&tests::gone_scope())?;}
    OriginalExecution::quiesce_previous(workflow)?;
    original.retain_result(root,&prepared.baseline)
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
    pub(super) fn gone_scope()->serde_json::Value {
        use sha2::{Digest,Sha256};
        serde_json::json!({"host_sha256":format!("{:x}",Sha256::digest(std::fs::read("/etc/machine-id").unwrap())),
            "boot_id":std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim(),
            "path":format!("/sys/fs/cgroup/run-{}",uuid::Uuid::new_v4().simple()),"inode":1,"limits":{"memory.max":67108864}})
    }

    #[tokio::test]
    async fn unenrolled_original_result_is_read_only_source_without_candidate_harness_or_current_executor() {
        let (root,control,prepared,guard)=fixture().await;
        let base=prepared.baseline.registration.baseline_head();let environment=environment_id(root.path()).unwrap();
        let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"old executor data").unwrap();
        original.retain_admission("observe",&gone_scope()).unwrap();OriginalExecution::quiesce_previous(&guard).unwrap();
        original.retain_result(root.path(),&prepared.baseline).unwrap();
        let binding=guard.binding().clone();let workflow=guard.id().to_owned();drop(original);guard.finish().unwrap();
        for path in prepared.proposal.files.keys() {std::fs::remove_file(root.path().join(path)).unwrap();}
        let path=control.path().join("workflow.db");
        let store=crate::autoship_workflow::WorkflowStore::open_read_only(&path).unwrap().unwrap();
        let before=store.snapshot(&workflow).unwrap().unwrap();let events=store.events(&workflow).unwrap();
        let evidence=store.inspect_verifier_evidence(&workflow);
        let loaded=retained_original_prepared(root.path(),AcceptanceSource::PullRequest(840),&binding,&evidence).unwrap();
        assert_eq!(loaded.len(),1,"completed unenrolled data has no read-only recovery producer");
        assert_eq!(serde_json::to_vec(&loaded[0]).unwrap(),serde_json::to_vec(&prepared).unwrap());
        assert!(prepared.baseline.registration.verify_integrity(root.path()).is_err(),"source inspection manufactured candidate qualification");
        assert!(!root.path().join(prepared.proposal.files.keys().next().unwrap()).exists(),"free inspection installed an adapter");
        assert_eq!(store.snapshot(&workflow).unwrap().unwrap(),before);assert_eq!(store.events(&workflow).unwrap(),events);
        assert!(retained_original_prepared(root.path(),AcceptanceSource::PullRequest(841),&binding,&evidence).unwrap().is_empty());
        let mut changed=binding;changed.head="b".repeat(40);
        assert!(retained_original_prepared(root.path(),AcceptanceSource::PullRequest(840),&changed,&evidence).unwrap().is_empty());
    }

    #[tokio::test]
    async fn unenrolled_original_source_refuses_lost_or_corrupt_protected_custody() {
        for fault in ["plan","result","admission","quiescence","adapter"] {
            let (root,control,prepared,guard)=fixture().await;
            let base=prepared.baseline.registration.baseline_head();let environment=environment_id(root.path()).unwrap();
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"old executor data").unwrap();
            original.retain_admission("observe",&gone_scope()).unwrap();OriginalExecution::quiesce_previous(&guard).unwrap();
            original.retain_result(root.path(),&prepared.baseline).unwrap();
            let command=command_key(&original.plan,&original.attempt,"observe");
            let key=match fault {"plan"=>original.plan.clone(),"result"=>result_key(&original.plan,&original.attempt),"admission"=>command.clone(),"quiescence"=>quiet_key(&command),"adapter"=>String::new(),_=>unreachable!()};
            let binding=guard.binding().clone();let workflow=guard.id().to_owned();drop(original);guard.finish().unwrap();
            let path=control.path().join("workflow.db");
            if fault=="adapter" {std::fs::write(root.path().join(prepared.proposal.files.keys().next().unwrap()),"changed adapter").unwrap();}
            else {let conn=rusqlite::Connection::open(&path).unwrap();conn.execute("DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",rusqlite::params![workflow,key]).unwrap();}
            let store=crate::autoship_workflow::WorkflowStore::open_read_only(&path).unwrap().unwrap();
            let before=store.snapshot(&workflow).unwrap().unwrap();let events=store.events(&workflow).unwrap();
            assert!(retained_original_prepared(root.path(),AcceptanceSource::PullRequest(840),&binding,&store.inspect_verifier_evidence(&workflow)).is_err(),"{fault} was accepted as original source");
            assert_eq!(store.snapshot(&workflow).unwrap().unwrap(),before);assert_eq!(store.events(&workflow).unwrap(),events);
        }
    }

    #[tokio::test]
    async fn quiescent_original_without_result_cannot_start_another_attempt() {
        let (_root,control,prepared,guard)=fixture().await;
        let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor").unwrap();
        original.retain_admission("observe",&gone_scope()).unwrap();
        OriginalExecution::quiesce_previous(&guard).unwrap();
        let retry=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor");
        let failure=retry.err().expect("quiescence was used as a result receipt and admitted another execution");
        assert_eq!(failure.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
        assert_eq!(guard.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1);
        drop(original);guard.finish().unwrap();
    }

    fn captured_fixture(original:&OriginalExecution<'_>,identity:&serde_json::Value,interrupted:bool)->PathBuf {
        captured_verifier_fixture(original,"observe",identity,interrupted)
    }

    fn captured_verifier_fixture(original:&OriginalExecution<'_>,verifier:&str,identity:&serde_json::Value,interrupted:bool)->PathBuf {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let capture=original.prepare_capture(verifier).unwrap();
        original.retain_admission_with_capture(verifier,identity,Some(capture.clone())).unwrap();
        let write=|name:&str,bytes:&[u8]| {
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(capture.path().join(name)).unwrap();
            file.write_all(bytes).unwrap();file.sync_all().unwrap();
        };
        write("stdout",b"false\n");write("stderr",b"");
        write("result.json",serde_json::json!({"version":1,"command":command_key(&original.plan,&original.attempt,verifier),
            "identity":identity,"exit_code":if interrupted {None}else{Some(0)},"interrupted":interrupted,"output_complete":true,
            "terminal_cause":if interrupted {"controller_stopped"}else{"completed"},
            "stdout_bytes":6,"stderr_bytes":0,"stdout_sha256":amendment::digest(b"false\n"),"stderr_sha256":amendment::digest(b"")}).to_string().as_bytes());
        capture.path().to_owned()
    }

    #[tokio::test]
    async fn interrupted_peer_reuses_completed_commands_with_original_admission_provenance() {
        for retained_interrupted_result in [false,true] {
        let (root,control,mut prepared,guard)=fixture().await;
        let base=prepared.baseline.registration.baseline_head().to_owned();let environment=environment_id(root.path()).unwrap();
        let mut second=prepared.proposal.contract.verifiers[0].clone();second.id="second".into();
        prepared.proposal.contract.verifiers.push(second);prepared.proposal.contract.criteria[0].verifier_ids.push("second".into());
        let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,&base,&environment,"executor").unwrap();
        captured_verifier_fixture(&original,"observe",&gone_scope(),false);
        captured_verifier_fixture(&original,"second",&gone_scope(),true);
        OriginalExecution::quiesce_previous(&guard).unwrap();
        let registration=RegisteredContract::register(root.path(),prepared.proposal.contract.clone(),base.clone(),environment.clone()).unwrap();
        if retained_interrupted_result {
            let receipts=execute_registered_with_custody(root.path(),&registration,&guard.audit_logger(),Some(&original)).await.unwrap();
            original.retain_result(root.path(),&BaselineEvidence {registration:registration.clone(),receipts}).unwrap();
        }
        let resumed=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,&base,&environment,"executor").unwrap();
        assert_ne!(resumed.attempt,original.attempt,"interruption did not create a successor attempt");
        assert_eq!(resumed.captured_observation("observe").unwrap().expect("completed peer was lost and would execute again").stdout,"false\n");
        assert!(resumed.captured_observation("second").unwrap().is_none());
        assert!(resumed.prepare_capture("observe").is_err(),"completed peer admitted a new command");
        assert!(guard.verifier_evidence(&command_key(&resumed.plan,&resumed.attempt,"observe"),"original_verifier_command_admission").unwrap().is_none(),"recovery copied an old admission under a new identity");
        captured_verifier_fixture(&resumed,"second",&gone_scope(),false);OriginalExecution::quiesce_previous(&guard).unwrap();
        let receipts=execute_registered_with_custody(root.path(),&registration,&guard.audit_logger(),Some(&resumed)).await.unwrap();
        let baseline=BaselineEvidence {registration,receipts};resumed.retain_result(root.path(),&baseline).unwrap();
        let retained=OriginalExecution::retained_result(root.path(),&guard,&prepared.proposal,&base,&environment,"executor").unwrap().unwrap().1;
        assert_eq!(serde_json::to_vec(&retained).unwrap(),serde_json::to_vec(&baseline).unwrap());
        let command=command_key(&original.plan,&original.attempt,"observe");
        let conn=rusqlite::Connection::open(control.path().join("workflow.db")).unwrap();
        conn.execute("DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",rusqlite::params![guard.id(),command]).unwrap();
        assert!(OriginalExecution::retained_result(root.path(),&guard,&prepared.proposal,&base,&environment,"executor").is_err(),"cross-attempt result ignored loss of its original admission");
        drop((original,resumed));guard.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn captured_original_resumes_its_attempt_and_interruption_is_explicit() {
        for interrupted in [false,true] {
            let (root,control,prepared,guard)=fixture().await;
            let base=prepared.baseline.registration.baseline_head();let environment=environment_id(root.path()).unwrap();
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"executor").unwrap();
            captured_fixture(&original,&gone_scope(),interrupted);
            let resumed=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"executor").unwrap();
            let attempts=guard.verifier_evidence_page("original_verifier_attempt",0).unwrap().len();
            if interrupted {
                assert_ne!(resumed.attempt,original.attempt);assert_eq!(attempts,2);
                assert_eq!(guard.verifier_evidence_page("original_verifier_attempt_interrupted",0).unwrap().len(),1);
                assert!(resumed.captured_observation("observe").unwrap().is_none());
            }else {
                assert_eq!(resumed.attempt,original.attempt);assert_eq!(attempts,1);
                assert_eq!(resumed.captured_observation("observe").unwrap().unwrap().stdout,"false\n");
                let receipts=execute_registered_with_custody(root.path(),&prepared.baseline.registration,&guard.audit_logger(),Some(&resumed)).await.unwrap();
                assert_eq!(receipts,prepared.baseline.receipts,"captured output did not reconstruct the original registered receipt");
            }
            drop((resumed,original));guard.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn changed_captured_output_or_scope_cannot_authorize_another_attempt() {
        for changed in ["stdout","scope","request","directory"] {
            let (root,control,prepared,guard)=fixture().await;
            let base=prepared.baseline.registration.baseline_head();let environment=environment_id(root.path()).unwrap();
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"executor").unwrap();
            let path=captured_fixture(&original,&gone_scope(),false);
            match changed {
                "stdout"=>std::fs::write(path.join("stdout"),b"true\n").unwrap(),
                "scope"=>{
                    let file=path.join("result.json");let mut value:serde_json::Value=serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
                    value["identity"]["inode"]=2.into();std::fs::write(file,value.to_string()).unwrap();
                },
                "request"=>std::fs::write(path.join("request.json"),"{}").unwrap(),
                "directory"=>{
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o755)).unwrap();
                },
                _=>unreachable!(),
            }
            OriginalExecution::quiesce_previous(&guard).unwrap();
            assert_eq!(original.captured_observation("observe").err().unwrap().downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
            let retry=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"executor");
            assert_eq!(retry.err().expect("changed capture authorized execution").downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
            assert_eq!(guard.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1);
            drop(original);guard.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn raw_measurement_failures_retain_the_plan_and_enter_a_durable_recovery_hold() {
        for infrastructure in [true,false] {
            let (root,control,prepared,mut guard)=fixture().await;
            let failure=if infrastructure {anyhow::Error::from(std::io::Error::from_raw_os_error(libc::ENOSPC))}
                else {anyhow::anyhow!("unclassified raw executor outcome")};
            let error=retain_initial_measurement(root.path(),control.path(),&prepared.proposal,&guard,
                |_original|std::future::ready(Err::<BaselineEvidence,_>(failure))).await.unwrap_err();
            let expected=if infrastructure {initial_repair::Hold::InfrastructureUnavailable}else {initial_repair::Hold::EvidenceUnknown};
            assert_eq!(error.downcast_ref::<initial_repair::Hold>(),Some(&expected),"raw executor escaped its durable hold: {error:#}");
            assert!(error.downcast_ref::<OriginalExecutionFailed>().is_some(),"hold lost the original plan");
            assert!(hold_initial_error(&mut guard,&error).await.unwrap().is_some());
            let store=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
            let snapshot=store.snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(snapshot.checkpoint.next.reason,expected.reason());
            assert_eq!(snapshot.model_calls,0);assert_eq!(snapshot.attempts,1);
            drop(store);guard.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn completed_measurement_storage_failure_retains_its_recovery_plan() {
        let (root,control,prepared,mut guard)=fixture().await;let database=control.path().join("workflow.db");
        let base=actual_head(root.path()).await.unwrap();let environment=environment_id(root.path()).unwrap();let executor=verifier_executor_id().unwrap();
        let key=amendment::digest(&serde_json::to_vec(&("initial-baseline-measurement-v1",&base,&environment,&prepared.proposal,&executor)).unwrap());
        let conn=rusqlite::Connection::open(&database).unwrap();
        conn.execute_batch(&format!("CREATE TRIGGER refuse_measurement_index BEFORE INSERT ON scope_stage_response WHEN NEW.key='{key}' BEGIN SELECT RAISE(ABORT,'measurement index unavailable'); END;")).unwrap();
        let baseline=prepared.baseline.clone();let source=root.path();
        let error=retain_initial_measurement(source,control.path(),&prepared.proposal,&guard,|original|async move {
            original.retain_admission("observe",&gone_scope())?;
            OriginalExecution::quiesce_previous(original.workflow)?;
            original.retain_result(source,&baseline)?;
            Ok(baseline)
        }).await.unwrap_err();
        assert_eq!(error.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown),"completed-result persistence escaped its hold: {error:#}");
        assert!(error.downcast_ref::<OriginalExecutionFailed>().is_some(),"completed-result persistence lost its original plan");
        hold_initial_error(&mut guard,&error).await.unwrap().unwrap();
        let id=guard.id().to_owned();guard.finish().unwrap();
        conn.execute_batch("DROP TRIGGER refuse_measurement_index;").unwrap();drop(conn);
        let recovery=workflow::WorkflowGuard::acquire_verifier_recovery_fixture(&database,workflow::Start {
            repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
            branch:"fixture",contract:"original scope",environment:"test",
        }).await.unwrap().unwrap();
        assert!(recover_original_verifier(root.path(),control.path(),&recovery).await.unwrap());
        assert_eq!(recovery.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1,"saved result created a second measurement attempt");
        recovery.finish().unwrap();
        let after=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.reason,"original_verifier_measurement_recovered");
        assert_eq!(after.attempts,1);assert_eq!(after.model_calls,0);
    }

    #[tokio::test]
    async fn partial_measurement_file_write_leaves_unpublished_data_and_recovers_without_execution() {
        use std::io::Write;
        let (root,control,prepared,mut guard)=fixture().await;let database=control.path().join("workflow.db");
        let base=actual_head(root.path()).await.unwrap();let environment=environment_id(root.path()).unwrap();let executor=verifier_executor_id().unwrap();
        let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,&base,&environment,&executor).unwrap();
        original.retain_admission("observe",&gone_scope()).unwrap();OriginalExecution::quiesce_previous(&guard).unwrap();
        original.retain_result(root.path(),&prepared.baseline).unwrap();
        let key=amendment::digest(&serde_json::to_vec(&("initial-baseline-measurement-v1",&base,&environment,&prepared.proposal,&executor)).unwrap());
        let hash=amendment::digest(&serde_json::to_vec(&prepared.baseline).unwrap());
        let file=control.path().join(format!("baseline-measurement-{key}-{hash}.json"));
        let failure=persist_baseline_with_writer(&file,&prepared.baseline,&worker_roots(root.path()),|file,bytes| {
            file.write_all(&bytes[..16])?;Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into())
        }).unwrap_err();
        assert!(!file.exists(),"partial baseline was published at the final identity and would permanently block recovery");
        let failure=result_retention_failure(&guard,&prepared.proposal,&base,&environment,&executor,failure);
        drop(original);hold_initial_error(&mut guard,&failure).await.unwrap().unwrap();
        let id=guard.id().to_owned();guard.finish().unwrap();
        let recovery=workflow::WorkflowGuard::acquire_verifier_recovery_fixture(&database,workflow::Start {
            repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
            branch:"fixture",contract:"original scope",environment:"test",
        }).await.unwrap().unwrap();
        assert!(recover_original_verifier(root.path(),control.path(),&recovery).await.unwrap());
        assert_eq!(recovery.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1);
        assert_eq!(baseline_witness::private_bytes(&file,4*1024*1024).unwrap(),serde_json::to_vec(&prepared.baseline).unwrap());
        recovery.finish().unwrap();let state=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
        assert_eq!(state.checkpoint.next.reason,"original_verifier_measurement_recovered");assert_eq!(state.model_calls,0);assert_eq!(state.attempts,1);
    }

    #[tokio::test]
    async fn measurement_file_crash_never_publishes_partial_final_name() {
        use std::io::Write;
        const CHILD:&str="AUTOSHIP_MEASUREMENT_PUBLICATION_CRASH";
        if let Some(path)=std::env::var_os(CHILD) {
            let path=PathBuf::from(path);
            let evidence:BaselineEvidence=serde_json::from_slice(&std::fs::read(path.with_extension("input")).unwrap()).unwrap();
            let _=persist_baseline_with_writer(&path,&evidence,&[],|file,bytes| {
                file.write_all(&bytes[..16])?;std::process::exit(79)
            });
            panic!("crash injection did not exit");
        }
        let (root,control,prepared)=baseline_witness::prepared_fixture();let file=control.path().join("baseline.json");
        std::fs::write(file.with_extension("input"),serde_json::to_vec(&prepared.baseline).unwrap()).unwrap();
        let status=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::acceptance::verifier_execution::tests::measurement_file_crash_never_publishes_partial_final_name","--nocapture"])
            .env(CHILD,&file).status().unwrap();
        assert_eq!(status.code(),Some(79));
        assert!(!file.exists(),"process exit published an incomplete final baseline");
        persist_baseline(&file,&prepared.baseline,&worker_roots(root.path())).unwrap();
        let bytes=baseline_witness::private_bytes(&file,4*1024*1024).unwrap();assert_eq!(bytes,serde_json::to_vec(&prepared.baseline).unwrap());
        assert!(persist_baseline(&file,&prepared.baseline,&worker_roots(root.path())).is_err(),"exclusive baseline publication overwrote existing evidence");
        assert_eq!(baseline_witness::private_bytes(&file,4*1024*1024).unwrap(),bytes);
    }

    #[tokio::test]
    async fn unsupported_atomic_publication_never_links_final_evidence() {
        const CHILD:&str="AUTOSHIP_MEASUREMENT_RENAME_UNSUPPORTED";
        if let Some(path)=std::env::var_os(CHILD) {
            let path=PathBuf::from(path);
            let evidence:BaselineEvidence=serde_json::from_slice(&std::fs::read(path.with_extension("input")).unwrap()).unwrap();
            assert!(persist_baseline(&path,&evidence,&[]).is_err(),"unsupported atomic rename published evidence through a non-atomic fallback");
            assert!(!path.exists(),"failed publication left a final hard link");return;
        }
        let (root,control,prepared)=baseline_witness::prepared_fixture();let path=control.path().join("baseline.json");
        std::fs::write(path.with_extension("input"),serde_json::to_vec(&prepared.baseline).unwrap()).unwrap();
        // Exercise the real pinned library/kernel boundary in a disposable
        // child: force the unavailable-rename path and a failed link cleanup.
        let filter=r#"import ctypes, errno, os, sys
s=ctypes.CDLL('libseccomp.so.2',use_errno=True)
s.seccomp_init.argtypes=[ctypes.c_uint32];s.seccomp_init.restype=ctypes.c_void_p
s.seccomp_syscall_resolve_name.argtypes=[ctypes.c_char_p];s.seccomp_syscall_resolve_name.restype=ctypes.c_int
s.seccomp_rule_add.argtypes=[ctypes.c_void_p,ctypes.c_uint32,ctypes.c_int,ctypes.c_uint]
s.seccomp_load.argtypes=[ctypes.c_void_p];s.seccomp_release.argtypes=[ctypes.c_void_p]
c=s.seccomp_init(0x7fff0000)
assert c
for name,error in [(b'renameat2',errno.ENOSYS),(b'unlink',errno.EACCES),(b'unlinkat',errno.EACCES)]:
    number=s.seccomp_syscall_resolve_name(name)
    assert number>=0 and s.seccomp_rule_add(c,0x00050000|error,number,0)==0
libc=ctypes.CDLL(None,use_errno=True)
assert libc.prctl(38,1,0,0,0)==0 and s.seccomp_load(c)==0
s.seccomp_release(c)
os.execv(sys.argv[1],sys.argv[1:])
"#;
        let status=std::process::Command::new("/usr/bin/python3").args(["-I","-c",filter])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::acceptance::verifier_execution::tests::unsupported_atomic_publication_never_links_final_evidence","--nocapture"])
            .env(CHILD,&path).status().unwrap();
        assert!(status.success(),"unsupported publication child failed");assert!(!path.exists());
        persist_baseline(&path,&prepared.baseline,&worker_roots(root.path())).unwrap();
        assert_eq!(baseline_witness::private_bytes(&path,4*1024*1024).unwrap(),serde_json::to_vec(&prepared.baseline).unwrap());
    }

    #[tokio::test]
    async fn infrastructure_only_result_does_not_install_a_harness_before_retry() {
        let (root,control,prepared,guard)=fixture().await;
        let base=prepared.baseline.registration.baseline_head();let environment=environment_id(root.path()).unwrap();let executor=verifier_executor_id().unwrap();
        let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,&executor).unwrap();
        original.retain_admission("observe",&gone_scope()).unwrap();OriginalExecution::quiesce_previous(&guard).unwrap();
        let observation=Observation {exit_code:None,stdout:String::new(),stderr:"interrupted".into(),interrupted:true,output_complete:false};
        let mut baseline=prepared.baseline.clone();
        baseline.receipts=vec![baseline.registration.observe(root.path(),"observe",baseline.registration.binding(base).unwrap(),&observation).unwrap()];
        assert_eq!(baseline.receipts[0].result,crate::autoship_contract::CheckResult::InfrastructureFailure);
        original.retain_result(root.path(),&baseline).unwrap();drop(original);
        for file in prepared.proposal.files.keys() {std::fs::remove_file(root.path().join(file)).unwrap();}
        assert!(OriginalExecution::retained_result(root.path(),&guard,&prepared.proposal,base,&environment,&executor).unwrap().is_none());
        for file in prepared.proposal.files.keys() {assert!(!root.path().join(file).exists(),"unusable receipt dirtied the new measurement checkout");}
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn invalid_recovery_plan_releases_the_unpaid_inspection_lease() {
        for missing in [false,true] {
            let (root,control,prepared,mut guard)=fixture().await;let database=control.path().join("workflow.db");
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor").unwrap();
            let plan=original.plan.clone();drop(original);
            guard.hold_initial_acceptance_verifier("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery",false,Some(&plan)).await.unwrap();
            let id=guard.id().to_owned();guard.finish().unwrap();
            let conn=rusqlite::Connection::open(&database).unwrap();
            if missing {conn.execute("DELETE FROM scope_stage_response WHERE workflow=?1 AND key=?2",rusqlite::params![id,plan]).unwrap();}
            else {conn.execute("UPDATE scope_stage_response SET response='corrupt' WHERE workflow=?1 AND key=?2",rusqlite::params![id,plan]).unwrap();}
            drop(conn);
            let retry=workflow::WorkflowGuard::acquire_verifier_recovery_fixture(&database,workflow::Start {
                repo_root:root.path(),worktree:root.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
                branch:"fixture",contract:"original scope",environment:"test",
            }).await;
            assert!(retry.is_err(),"invalid evidence was admitted");
            let after=crate::autoship_workflow::WorkflowStore::open(&database).unwrap().snapshot(&id).unwrap().unwrap();
            assert!(after.lease_until_ms<=chrono::Utc::now().timestamp_millis(),"failed validation stranded a ten-minute lease");
            assert_eq!(after.attempts,1);assert_eq!(after.model_calls,0);
        }
    }

    #[tokio::test]
    async fn retained_original_verifier_result_reenters_without_executing_again() {
        for reproduced in [true,false] {
            let (root,control,prepared,guard)=fixture().await;
            let base=prepared.baseline.registration.baseline_head();
            let environment=environment_id(root.path()).unwrap();
            let executor=verifier_executor_id().unwrap();
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,&executor).unwrap();
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
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,&verifier_executor_id().unwrap()).unwrap();
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
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),
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

    #[test]
    #[ignore = "requires isolated delegated service and no enclosing managed build lease; executor owns normal build admission"]
    fn original_verifier_recovery_reconciles_captured_output_without_reexecution() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            for partial in [false,true] {
            let (root,control,prepared,mut guard)=fixture().await;
            let database=control.path().join("workflow.db");let base=prepared.baseline.registration.baseline_head();
            let mut proposal=prepared.proposal.clone();
            if partial {
                let mut remaining=proposal.contract.verifiers[0].clone();remaining.id="observe_remaining".into();
                proposal.contract.criteria[0].verifier_ids.push(remaining.id.clone());proposal.contract.verifiers.push(remaining);
            }
            let original=OriginalExecution::begin(&guard,control.path(),&proposal,base,
                &environment_id(root.path()).unwrap(),&verifier_executor_id().unwrap()).unwrap();
            let attempt=original.attempt.clone();
            let stage=control.path().join("original-stage");let stage_path=stage.to_str().unwrap();
            trusted_git(root.path(),&["worktree","add","--detach",stage_path,base]).await.unwrap();
            let actual=if partial {
                install_harness(&stage,&proposal).unwrap();
                let registration=RegisteredContract::register(&stage,proposal.contract.clone(),base.into(),environment_id(&stage).unwrap()).unwrap();
                original.require_registration(&stage,&registration).unwrap();
                let target=protected_verifier_target(&stage,&registration).unwrap();
                let _lease=crate::build_storage::admit(&stage,&target).await.unwrap();
                let committed=verifier_checkout::materialize(&stage,base,&registration,&target).await.unwrap();
                let verifier=&registration.contract().verifiers[0];
                let observation=isolated_observation_with_custody(committed.path(),&target,verifier.command.invocation().unwrap(),
                    &guard.audit_logger(),Some((&original,&verifier.id))).await.unwrap();
                vec![registration.observe(&stage,&verifier.id,registration.binding(base).unwrap(),&observation).unwrap()]
            }else {register_and_observe_with_custody(&stage,proposal,&guard.audit_logger(),Some(&original)).await.unwrap().receipts};
            trusted_git(root.path(),&["worktree","remove","--force",stage_path]).await.unwrap();
            assert!(guard.verifier_evidence_page("original_verifier_result",0).unwrap().is_empty());
            // Cut the production path after real command capture/quiescence,
            // before the controller could retain the whole registered result.
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
            assert_eq!(guard.verifier_evidence_page("original_verifier_attempt",0).unwrap().len(),1,"recovery created another raw attempt");
            assert_eq!(guard.verifier_evidence_page("original_verifier_command_admission",0).unwrap().len(),if partial{2}else{1},"recovery executed a completed verifier again");
            let results=guard.verifier_evidence_page("original_verifier_result",0).unwrap();assert_eq!(results.len(),1);
            let recovered:RecordedResult=serde_json::from_str(&results[0].body).unwrap();
            assert_eq!(recovered.attempt,attempt);assert!(recovered.baseline.receipts.starts_with(&actual));
            assert!(baseline_witness::assert_current(root.path(),control.path(),&prepared).is_err());
            guard.finish().unwrap();
            }
        });
    }

    #[tokio::test]
    async fn verifier_command_custody_requires_the_frozen_registration_and_adapter() {
        let (root,control,prepared,guard)=fixture().await;
        let base=prepared.baseline.registration.baseline_head();
        let environment=prepared.baseline.registration.binding(base).unwrap().environment_id;
        let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,&environment,"executor").unwrap();
        original.require_registration(root.path(),&prepared.baseline.registration).unwrap();
        let wrong_base=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,&"f".repeat(40),&environment,"executor").unwrap();
        assert!(wrong_base.require_registration(root.path(),&prepared.baseline.registration).is_err());
        let wrong_environment=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,base,"another-environment","executor").unwrap();
        assert!(wrong_environment.require_registration(root.path(),&prepared.baseline.registration).is_err());
        let mut changed=prepared.proposal.clone();
        changed.files.values_mut().next().unwrap().push_str("\nprint('different input')\n");
        let changed_adapter=OriginalExecution::begin(&guard,control.path(),&changed,base,&environment,"executor").unwrap();
        assert!(changed_adapter.require_registration(root.path(),&prepared.baseline.registration).is_err(),"a valid registration for different adapter bytes was used");
        drop((original,wrong_base,wrong_environment,changed_adapter));guard.finish().unwrap();
    }

    #[tokio::test]
    async fn verifier_command_custody_settles_the_exact_prior_scope_and_preserves_its_input() {
        let (root,control,prepared,guard)=fixture().await;
        let environment=environment_id(root.path()).unwrap();
        let first=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),&environment,"original-executor").unwrap();
        let identity=gone_scope();first.retain_admission("observe",&identity).unwrap();
        let command=command_key(&first.plan,&first.attempt,"observe");
        assert!(first.retain_admission("foreign-verifier",&identity).is_err());
        let mut foreign=identity.clone();foreign["inode"]=2.into();
        assert!(first.retain_admission("observe",&foreign).is_err(),"original command admission was overwritten");
        let raw=guard.verifier_evidence(&command,"original_verifier_command_admission").unwrap().unwrap();
        assert!(OriginalExecution::retain_quiescence(&guard,&command,&raw,serde_json::json!({"identity":foreign,"verified_state":"removed_empty_subtree"})).is_err());
        let blocked=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),&environment,"changed-executor");
        assert_eq!(blocked.err().unwrap().downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
        first.retain_result(root.path(),&prepared.baseline).unwrap();
        let next=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),&environment,"changed-executor").unwrap();
        assert_ne!(first.plan,next.plan,"fixture did not change the current plan");
        let proof:Quiescence=serde_json::from_str(&guard.verifier_evidence(&quiet_key(&command),"original_verifier_command_quiescent").unwrap().unwrap()).unwrap();
        assert_eq!(proof.evidence["identity"],identity);assert_eq!(proof.admission_sha256,amendment::digest(raw.as_bytes()));
        assert_eq!(crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        drop((first,next));guard.finish().unwrap();drop(root);
    }

    #[tokio::test]
    async fn verifier_command_custody_unknown_scope_blocks_the_real_executor_boundary() {
        let (root,control,prepared,guard)=fixture().await;
        let first=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","original-executor").unwrap();
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
            let original=OriginalExecution::begin(&guard,control.path(),&prepared.proposal,prepared.baseline.registration.baseline_head(),"fixture","executor").unwrap();
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
