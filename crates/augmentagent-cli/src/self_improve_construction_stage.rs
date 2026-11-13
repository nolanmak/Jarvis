//! Connect construction intent to the existing original executor and witness.
//! This service is not yet enrolled by the held-work selector or adjudicator.
use super::*;

pub(super) fn authenticate(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement)->Result<()> {
    episode.require_current(root,workflow)?;
    ensure!(measurement.episode==episode.key(),initial_repair::Hold::EvidenceUnknown);
    let saved:Measurement=read(workflow,&intent_key(episode.key(),measurement.slot))?.context(initial_repair::Hold::EvidenceUnknown)?;
    ensure!(saved.identity()?==measurement.identity()? && read::<Rejection>(workflow,&rejection_key(episode.key(),measurement.slot))?.is_none(),initial_repair::Hold::EvidenceUnknown);
    let turn:Turn=read(workflow,&turn_key(episode.key(),measurement.round))?.context(initial_repair::Hold::EvidenceUnknown)?;
    ensure!(turn.version==1 && turn.episode==episode.key() && turn.round==measurement.round
        && turn.input.identity()?==measurement.input_sha256,initial_repair::Hold::EvidenceUnknown);
    let reply=constructor_native::retained(root,workflow,episode,&turn.input)?;
    ensure!(digest(reply.text.as_bytes())==measurement.response_sha256
        && reply.provider==measurement.provider && reply.native_session==measurement.native_session,initial_repair::Hold::EvidenceUnknown);
    let supplied:serde_json::Value=serde_json::from_str(turn.input.request_text())?;
    let adjudication=match &supplied["construction"]["final_adjudication"] {
        serde_json::Value::Null=>None,serde_json::Value::String(text)=>Some(text.as_str()),
        _=>anyhow::bail!(initial_repair::Hold::EvidenceUnknown),
    };
    let old=constructor_enrollment(root,episode.control(),episode.source)?;
    let Output::Proposal(draft)=decode(&reply.text,&old,&environment_id(root)?,adjudication)? else {
        anyhow::bail!(initial_repair::Hold::EvidenceUnknown);
    };
    ensure!(serde_json::to_vec(&draft)?==serde_json::to_vec(&measurement.draft)?,initial_repair::Hold::EvidenceUnknown);
    Ok(())
}

/// A live, controller-owned stage, not deserializable approval data. Keeping
/// its original checkout alive permits fresh integrity/qualification checks
/// without installing unapproved adapters into the ordinary candidate.
pub(in crate::self_improve::acceptance::amendment) struct StagedCorrection {
    _directory:tempfile::TempDir,
    original:PathBuf,
    workflow:String,
    binding:crate::autoship_workflow::Binding,
    old_contract:String,
    candidate_environment:String,
    measurement:Measurement,
    original_plan:String,
    original_result:String,
    original_data_identity:String,
    prepared:PreparedAcceptance,
}
impl StagedCorrection {
    pub(in crate::self_improve::acceptance::amendment) fn prepared(&self)->&PreparedAcceptance {&self.prepared}
    pub(in crate::self_improve::acceptance::amendment) fn constructor_session(&self)->(&str,&str) {(&self.measurement.provider,&self.measurement.native_session)}
    pub(in crate::self_improve::acceptance::amendment) async fn require_current(&self,root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode)->Result<()> {
        authenticate(root,workflow,episode,&self.measurement)?;
        ensure!(self.workflow==workflow.id() && self.binding==*workflow.binding()
            && self.old_contract==episode.registered_contract
            && actual_head(root).await?==self.binding.head
            && environment_id(root)?==self.candidate_environment,initial_repair::Hold::EvidenceUnknown);
        ensure!(actual_head(&self.original).await?==episode.original_base()
            && environment_id(&self.original)?==episode.original_environment()
            && self.prepared.baseline.registration.baseline_head()==episode.original_base()
            && self.prepared.baseline.registration.baseline_result(&self.prepared.baseline.receipts)?==BaselineResult::Reproduced,
            initial_repair::Hold::EvidenceUnknown);
        let data=workflow.exact_original_result_data(&self.original_plan,&self.original_result)?;
        ensure!(data.identity==self.original_data_identity && data.binding==self.binding
            && serde_json::to_vec(&data.prepared)?==serde_json::to_vec(&self.prepared)?,initial_repair::Hold::EvidenceUnknown);
        let (plan,body)=verifier_execution::plan_input(workflow,self.measurement.proposal(),episode.original_base(),episode.original_environment(),&verifier_executor_id()?)?;
        ensure!(plan==self.original_plan && accounting::owned(root,episode.control(),workflow,&plan,&body)?,initial_repair::Hold::EvidenceUnknown);
        baseline_witness::assert_current(&self.original,episode.control(),&self.prepared)
    }
    pub(in crate::self_improve::acceptance::amendment) fn validate_decision(&self,decision:&Decision)->Result<()> {
        if !decision.approved {return Ok(());}
        ensure!(decision.previous_contract_hash==self.old_contract
            && decision.proposal.as_ref().map(serde_json::to_vec).transpose()?==Some(serde_json::to_vec(self.measurement.proposal())?)
            && decision.approve_additional_files==self.measurement.additional_files()
            && decision.approve_environment_transition==self.measurement.proposes_environment_transition(),
            "authority rewrote the measured correction or proposed environment mode");
        Ok(())
    }
}

/// Controller-only references to a completed semantic failure. Private raw
/// output is loaded separately; its contents never become an error message.
#[derive(Debug)]
pub(super) struct MeasuredFailure {pub(super) plan:String,pub(super) result:String,pub(super) identity:String}
impl std::fmt::Display for MeasuredFailure {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("constructed original measurement rejected; exact feedback retained privately")}
}
impl std::error::Error for MeasuredFailure {}
fn measured_failure(error:anyhow::Error,plan:&str,result:&str,identity:&str)->anyhow::Error {
    if error.downcast_ref::<BaselineNotRed>().is_some_and(|failure|failure.kind.is_some()) {
        error.context(MeasuredFailure {plan:plan.into(),result:result.into(),identity:identity.into()})
    }else {error}
}

pub(in crate::self_improve::acceptance::amendment) async fn stage(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement)->Result<StagedCorrection> {
    stage_with(root,workflow,episode,measurement,|original,control,prepared,workflow| {
        Box::pin(async move {baseline_witness::qualify(original,control,prepared,workflow).await})
    }).await
}

async fn stage_with<Q>(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,qualify:Q)->Result<StagedCorrection>
where Q:for<'a> FnOnce(&'a Path,&'a Path,&'a PreparedAcceptance,&'a workflow::WorkflowGuard)->std::pin::Pin<Box<dyn std::future::Future<Output=Result<()>>+'a>> {
    authenticate(root,workflow,episode,measurement)?;
    let binding=workflow.binding().clone();
    ensure!(actual_head(root).await?==binding.head,initial_repair::Hold::EvidenceUnknown);
    let candidate_environment=environment_id(root)?;
    let directory=tempfile::Builder::new().prefix("constructor-original-").tempdir_in(episode.control())?;
    let original=directory.path().join("checkout");
    // This is a disposable source checkout, not an executor. Actual commands
    // still run through the existing pinned exporter and isolated executor.
    let cloned=Command::new("/usr/bin/git")
        .args(["--no-replace-objects","-c","core.hooksPath=/dev/null","clone","--shared","--no-checkout","--"])
        .arg(root).arg(&original).env_clear().env("PATH","/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM","1").env("GIT_CONFIG_GLOBAL","/dev/null").output().await?;
    ensure!(cloned.status.success(),"constructor original object clone unavailable");
    trusted_git(&original,&["checkout","--detach",episode.original_base()]).await?;
    let environment=environment_id(&original)?;
    ensure!(actual_head(&original).await?==episode.original_base() && environment==episode.original_environment(),initial_repair::Hold::EvidenceUnknown);
    let executor=verifier_executor_id()?;
    let proposal=measurement.proposal();
    let (plan,body)=verifier_execution::plan_input(workflow,proposal,episode.original_base(),&environment,&executor)?;
    accounting::reserve(root,workflow,episode,measurement,&plan,&body)?;
    // OriginalExecution retains every conclusive result, including already
    // green and unexpected failures. Reuse those exact observations too.
    let (original_result,baseline)=if let Some((result,baseline))=verifier_execution::OriginalExecution::retained_result(&original,workflow,proposal,episode.original_base(),&environment,&executor)? {
        (result,baseline)
    }else{
        let execution=verifier_execution::OriginalExecution::begin(workflow,episode.control(),proposal,episode.original_base(),&environment,&executor)?;
        let baseline=register_and_observe_with_custody(&original,proposal.clone(),&workflow.audit_logger(),Some(&execution)).await.map_err(|error|execution.failed(error))?;
        execution.retain_result(&original,&baseline).map_err(|error|execution.failed(error))?;
        (execution.result_reference(),baseline)
    };
    let prepared=PreparedAcceptance {proposal:proposal.clone(),baseline};
    let original_data=workflow.exact_original_result_data(&plan,&original_result)?;
    ensure!(serde_json::to_vec(&original_data.prepared)?==serde_json::to_vec(&prepared)?,initial_repair::Hold::EvidenceUnknown);
    reproduce_observation(prepared.baseline.clone()).map_err(|error|measured_failure(error,&plan,&original_result,&original_data.identity))?;
    qualify(&original,episode.control(),&prepared,workflow).await
        .map_err(|error|measured_failure(error,&plan,&original_result,&original_data.identity))?;
    // A callback's success, including an eventual cached qualifier, is not
    // enough: independently re-read the protected current-policy witness.
    baseline_witness::assert_current(&original,episode.control(),&prepared)?;
    let staged=StagedCorrection {_directory:directory,original,workflow:workflow.id().into(),binding,
        old_contract:episode.registered_contract.clone(),candidate_environment,measurement:measurement.clone(),
        original_plan:plan,original_result,original_data_identity:original_data.identity,prepared};
    staged.require_current(root,workflow,episode).await?;
    Ok(staged)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[tokio::test]
    async fn unowned_constructor_text_cannot_start_original_staging() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let measurement=next_with(root.path(),&guard,super::super::tests::request(&episode,&old,"test-model"),|_| {
            std::future::ready(Ok(constructor_native::Reply {text:super::super::tests::proposal(&old),provider:"codex".into(),native_session:"unowned-model-string".into()}))
        }).await.unwrap();
        let before=std::fs::read_dir(control.path()).unwrap().count();
        let error=stage(root.path(),&guard,&episode,&measurement).await.err().unwrap();
        assert_eq!(error.downcast_ref::<initial_repair::Hold>(),Some(&initial_repair::Hold::EvidenceUnknown));
        assert_eq!(std::fs::read_dir(control.path()).unwrap().count(),before,"unowned output created a staging checkout");
        assert!(guard.verifier_evidence_page("original_verifier_plan",0).unwrap().is_empty());
        guard.finish().unwrap();
    }

    pub(in super::super) async fn owned_measurement(root:&Path,guard:&workflow::WorkflowGuard,episode:&ConstructionEpisode,old:&PreparedAcceptance,text:String)->Result<Measurement> {
        next_with(root,guard,super::super::tests::request(episode,old,"test-model"),|input| {
            let text=text.clone();
            async move {constructor_native::scripted_reply(root,guard,episode,&input,text).await}
        }).await
    }

    #[tokio::test]
    async fn completed_constructor_data_replays_after_its_execution_grant_is_stale() {
        let (root,_control,old,mut guard,episode)=super::super::tests::fixture().await;
        let measurement=owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
        let turn:Turn=read(&guard,&turn_key(episode.key(),measurement.round)).unwrap().unwrap();
        let before=constructor_native::retained(root.path(),&guard,&episode,&turn.input).unwrap();
        let mut later=old.clone();later.proposal.contract.revision+=1;
        later.baseline.registration=RegisteredContract::register(root.path(),later.proposal.contract.clone(),episode.original_base().into(),episode.original_environment().into()).unwrap();
        guard.bind_acceptance_for_recovery(&later).unwrap();
        guard.checkpoint(crate::autoship_workflow::Stage::Scope,"changed_registered_context","qualify_original",0,&[]).await.unwrap();
        assert!(episode.require_current(root.path(),&guard).is_err());
        let after=constructor_native::retained(root.path(),&guard,&episode,&turn.input).unwrap();
        assert_eq!(before.text,after.text);assert_eq!(before.native_session,after.native_session);
        assert_eq!(guard.construction_native_custody().unwrap().len(),1);
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn constructor_actual_admission_refuses_an_unsettled_original_command() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let _execution=verifier_execution::OriginalExecution::begin(&guard,episode.control(),&old.proposal,episode.original_base(),episode.original_environment(),&verifier_executor_id().unwrap()).unwrap();
        let plan=guard.verifier_evidence_page("original_verifier_plan",0).unwrap().pop().unwrap();
        let attempt="a".repeat(64);let verifier=&old.proposal.contract.verifiers[0].id;
        let key=digest(&serde_json::to_vec(&("original-verifier-command-v1",&plan.key,&attempt,verifier)).unwrap());
        // Supported immutable fixture admission with no published quiescence or
        // output. No command was actually launched and no production row changes.
        let admission=serde_json::json!({"version":1,"kind":"original_verifier_command_admission","workflow":guard.id(),
            "plan":plan.key,"attempt":attempt,"verifier":verifier,"identity":{}});
        guard.record_verifier_evidence(&key,"original_verifier_command_admission",&admission.to_string()).unwrap();
        let result=owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await;
        assert!(result.is_err(),"unsettled original command admitted a new native constructor conversation");
        let store=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,0,"unsettled command spent a model admission");
        drop(store);guard.finish().unwrap();
    }

    #[tokio::test]
    async fn fresh_scoper_cannot_bypass_unsettled_original_execution_by_changing_purpose() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let _execution=verifier_execution::OriginalExecution::begin(&guard,episode.control(),&old.proposal,episode.original_base(),episode.original_environment(),&verifier_executor_id().unwrap()).unwrap();
        let plan=guard.verifier_evidence_page("original_verifier_plan",0).unwrap().pop().unwrap();
        let attempt="b".repeat(64);let verifier=&old.proposal.contract.verifiers[0].id;
        let key=digest(&serde_json::to_vec(&("original-verifier-command-v1",&plan.key,&attempt,verifier)).unwrap());
        guard.record_verifier_evidence(&key,"original_verifier_command_admission",&serde_json::json!({"version":1,
            "kind":"original_verifier_command_admission","workflow":guard.id(),"plan":plan.key,"attempt":attempt,
            "verifier":verifier,"identity":{}}).to_string()).unwrap();
        // A fresh scoper/reviewer has no constructor-purpose prefix. Its
        // actual model admission must still retain the old command's hold.
        let provider=augmentagent_channel_core::ProviderKind::Codex;
        let result=augmentagent_channel_core::native_session::fresh_review_scope(provider,async {
            let session=augmentagent_channel_core::native_session::current_for(provider)?.unwrap();
            let mut native=session.begin(provider)?;
            guard.audit_logger().model_call_admitted(provider.name())?;
            native.observe("test-fresh-scoper-admission")?;native.finish()?;
            Ok(())
        }).await;
        assert!(result.is_err(),"fresh scoper admitted a model call around unsettled original execution");
        let snapshot=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap();
        assert_eq!(snapshot.model_calls,0,"changing model purpose bypassed the execution hold");
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn native_json_imitating_original_evidence_remains_untrusted_data_not_a_custody_hold() {
        let (root,control,_old,guard,episode)=super::super::tests::fixture().await;
        let input=constructor_native::Input::for_episode(&episode,augmentagent_channel_core::ProviderKind::Codex,"test-model",
            "Untrusted constructor, no tools","Preserve every requirement and return the requested JSON proposal").unwrap();
        constructor_native::scripted_reply(root.path(),&guard,&episode,&input,
            r#"{"version":1,"kind":"original_verifier_plan","workflow":"model-invented-metadata"}"#.into()).await.unwrap();
        let store=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
        let result=verifier_execution::require_correction_commands(&store.inspect_verifier_evidence(guard.id()));
        assert!(result.is_ok(),"untrusted native JSON poisoned controller custody instead of reaching bounded format correction: {result:?}");
        assert!(guard.verifier_evidence_page("original_verifier_plan",0).unwrap().is_empty());
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,1);
        drop(store);guard.finish().unwrap();
    }

    #[cfg(target_os="linux")]
    #[tokio::test]
    #[ignore="actual isolated original execution owns managed storage; run built test outside cargo-managed lease; constructor and witness replies are scripted"]
    async fn isolated_constructed_stage_requires_real_red_current_witness_and_reuses_original_result()->Result<()> {
        // The native transport/accounting is exercised with a scripted provider
        // reply. Original behavior executes for real in the pinned sandbox;
        // qualification data is explicitly test-only, never live acceptance.
        for qualification in ["positive","absent","negative","already-green"] {
            let (root,control,old,guard,episode)=super::super::tests::fixture().await;
            let mut text=super::super::tests::proposal(&old);
            if qualification=="already-green" {
                let mut raw:serde_json::Value=serde_json::from_str(&text)?;
                raw["proposal"]["contract"]["verifiers"][0]["command"]["oracle"]["value"]=false.into();
                text=serde_json::to_string(&raw)?;
            }
            let measurement=owned_measurement(root.path(),&guard,&episode,&old,text).await?;
            let head=actual_head(root.path()).await?;
            let result=stage_with(root.path(),&guard,&episode,&measurement,move |original,control,prepared,_| {
                Box::pin(async move {
                    match qualification {
                        "positive"=>baseline_witness::persist_scripted(original,control,prepared,true),
                        "negative"=>{
                            baseline_witness::persist_scripted(original,control,prepared,false)?;
                            baseline_witness::assert_current(original,control,prepared)
                        }
                        "absent"=>Ok(()),
                        _=>anyhow::bail!("already-green original reached witness qualification"),
                    }
                })
            }).await;
            if guard.audit_logger().resource_capacity_seen() {
                // Report the original refusal before fenced guard inspection
                // replaces it with the generic await-capacity checkpoint.
                let audit=std::fs::read_to_string(guard.audit_logger().path())?;
                eprintln!("Isolated constructed-stage capacity evidence ({qualification}):\n{audit}");
                return match result {Err(error)=>Err(error.context("isolated stage resource admission")),Ok(_)=>Err(anyhow::anyhow!("isolated stage retained resource capacity refusal"))};
            }
            ensure!(actual_head(root.path()).await?==head,"staging edited the ordinary candidate");
            for (path,body) in &old.proposal.files {ensure!(std::fs::read_to_string(root.path().join(path))?==*body,"staging installed unapproved adapters in candidate");}
            let commands=guard.verifier_evidence_page("original_verifier_command_admission",0)?;
            ensure!(commands.len()==1,"original measurement was not admitted exactly once");
            ensure!(guard.verifier_evidence_page("original_verifier_command_quiescent",0)?.len()==1,"original command was not quiescent");
            ensure!(guard.verifier_evidence_page("original_verifier_result",0)?.len()==1,"original result was not retained before qualification");
            ensure!(verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard)?.is_empty(),"actual constructed measurement leaked into initial-repair accounting");
            match qualification {
                "positive"=>{
                    let staged=result.map_err(|e|e.context("actual isolated constructed stage"))?;
                    staged.require_current(root.path(),&guard,&episode).await?;
                    ensure!(staged.prepared().baseline.receipts.iter().all(|r|r.result==crate::autoship_contract::CheckResult::IntendedFailure),"stage did not measure intended original red");
                    ensure!(!staged.constructor_session().1.is_empty(),"stage lacks native constructor ownership");
                    let mut decision=Decision {approved:true,previous_contract_hash:old.baseline.registration.contract_hash().into(),approve_environment_transition:false,
                        approve_additional_files:vec![],reason:"Independent review of exact staged proposal".into(),proposal:Some(measurement.proposal().clone())};
                    staged.validate_decision(&decision)?;
                    decision.proposal.as_mut().unwrap().files.values_mut().next().unwrap().push_str("\n# authority rewrite\n");
                    ensure!(staged.validate_decision(&decision).is_err(),"unmeasured authority rewrite accepted");
                    decision.approved=false;staged.validate_decision(&decision)?;
                    drop(staged);
                    let replay=stage_with(root.path(),&guard,&episode,&measurement,|original,control,prepared,_| {
                        Box::pin(async move {baseline_witness::assert_current(original,control,prepared)})
                    }).await?;
                    ensure!(guard.verifier_evidence_page("original_verifier_command_admission",0)?.len()==1,"restart reran unchanged original command");
                    ensure!(guard.verifier_evidence_page("original_verifier_result",0)?.len()==1,"restart manufactured another original measurement");
                    let admitted:serde_json::Value=serde_json::from_str(&commands[0].body)?;
                    let stdout=PathBuf::from(admitted["capture"]["path"].as_str().context("actual original capture absent")?).join("stdout");
                    let actual=std::fs::read(&stdout)?;std::fs::remove_file(&stdout)?;
                    ensure!(replay.require_current(root.path(),&guard,&episode).await.is_err(),"lost original capture still admitted review with a cached positive witness");
                    {
                        use std::io::Write;use std::os::unix::fs::OpenOptionsExt;
                        let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(stdout)?;
                        file.write_all(&actual)?;file.sync_all()?;
                    }
                    replay.require_current(root.path(),&guard,&episode).await?;
                    let file=std::fs::read_dir(control.path())?.map(|e|e.map(|e|e.path())).collect::<std::io::Result<Vec<_>>>()?
                        .into_iter().find(|p|p.file_name().is_some_and(|n|n.to_string_lossy().starts_with("baseline-witness-") && n.to_string_lossy().ends_with(".json")))
                        .context("current witness file missing")?;
                    std::fs::remove_file(file)?;
                    ensure!(replay.require_current(root.path(),&guard,&episode).await.is_err(),"deleted current witness still admitted review");
                }
                "negative"=>{
                    let error=result.err().context("negative witness produced a staged correction")?;
                    ensure!(error.downcast_ref::<BaselineNotRed>().is_some(),"negative semantic result lost its classification: {error:#}");
                }
                "absent"=>{ensure!(result.is_err(),"qualifier success without protected witness produced a stage");}
                _=>{
                    let error=result.err().context("already-green original produced a staged correction")?;
                    ensure!(matches!(error.downcast_ref::<BaselineNotRed>().and_then(|f|f.kind),Some(initial_repair::FailureKind::AlreadyPasses)),"already-green result lost classification: {error:#}");
                    let measured=error.downcast_ref::<MeasuredFailure>().context("already-green result lost exact feedback references")?;
                    guard.exact_original_result_data(&measured.plan,&measured.result)?;
                    let feedback=feedback::retain(root.path(),&guard,&episode,&measurement,&measured.plan,&measured.result,&measured.identity)?;
                    ensure!(feedback::load(root.path(),&guard,&episode,&measurement,&feedback)?["original"]["commands"]["observe"]["stdout"].is_string(),"actual original output absent from feedback");
                    let replay=stage_with(root.path(),&guard,&episode,&measurement,|_,_,_,_|Box::pin(async {anyhow::bail!("already-green replay reached qualifier")})).await;
                    ensure!(replay.is_err() && guard.verifier_evidence_page("original_verifier_command_admission",0)?.len()==1,"already-green original was executed again");
                }
            }
            guard.finish()?;
        }
        println!("Actual isolated original execution: red/witness gates, retained non-red and red replay, exact proposal review binding; scripted provider/witness data only, no production workflow acceptance");
        Ok(())
    }
}
