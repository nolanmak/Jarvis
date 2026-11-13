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
    pub(in crate::self_improve::acceptance::amendment) fn original_references(&self)->(&str,&str) {(&self.original_plan,&self.original_result)}
    pub(in crate::self_improve::acceptance::amendment) fn candidate_environment(&self)->&str {&self.candidate_environment}
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
    pub(in crate::self_improve::acceptance::amendment) async fn adjudication_data(&self,root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode)->Result<serde_json::Value> {
        self.require_current(root,workflow,episode).await?;
        Ok(serde_json::json!({"version":1,"workflow":self.workflow,"episode":episode.key(),
            "binding":self.binding,"old_contract":self.old_contract,"candidate_environment":self.candidate_environment,
            "measurement_intent":self.measurement.identity()?,"draft":self.measurement.draft,
            "original_plan":self.original_plan,"original_result":self.original_result,
            "original":workflow.exact_original_result_data(&self.original_plan,&self.original_result)?,
            "qualified_original_witness":baseline_witness::current_data(&self.original,episode.control(),&self.prepared)?}))
    }
    pub(in crate::self_improve::acceptance::amendment) async fn adjudication_identity(&self,root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode)->Result<String> {
        self.require_current(root,workflow,episode).await?;
        let witness=baseline_witness::current_data(&self.original,episode.control(),&self.prepared)?;
        // A routing binding, publication reference, qualification-policy hash
        // or rephrased reason is not new behavioral evidence. Keep the live
        // full-binding/custody checks above, but deduplicate the verdict using
        // the actual proposal, observation and immutable production attribution.
        let mut attribution=std::collections::BTreeMap::new();
        for verifier in witness["verifiers"].as_array().context(initial_repair::Hold::EvidenceUnknown)? {
            let id=verifier["verifier_id"].as_str().context(initial_repair::Hold::EvidenceUnknown)?;
            let mut citations=std::collections::BTreeSet::new();
            for citation in verifier["production_citations"].as_array().context(initial_repair::Hold::EvidenceUnknown)? {
                citations.insert(serde_json::to_string(citation)?);
            }
            ensure!(attribution.insert(id.to_string(),citations).is_none(),initial_repair::Hold::EvidenceUnknown);
        }
        let original=workflow.exact_original_result_data(&self.original_plan,&self.original_result)?;
        Ok(digest(&serde_json::to_vec(&("qualified-staged-correction-v1",&self.workflow,episode.key(),
            &self.old_contract,&self.candidate_environment,&self.measurement.draft,&self.prepared.baseline,&original.commands,attribution))?))
    }
    pub(in crate::self_improve::acceptance::amendment) async fn original_source(&self,root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,budget:usize)->Result<source_evidence::Evidence> {
        self.require_current(root,workflow,episode).await?;
        let witness=baseline_witness::current_data(&self.original,episode.control(),&self.prepared)?;
        let context=witness["context"].as_str().context(initial_repair::Hold::EvidenceUnknown)?;
        let custody=workflow.baseline_native_custody()?;
        let key=source_evidence::replay_key(&self.original,episode.control(),episode.original_base(),&custody,Some(context))?;
        let replay=key.map(|key|workflow.scope_stage_response(&key)).transpose()?.flatten();
        source_evidence::read_bounded(&self.original,episode.control(),episode.original_base(),&custody,Some(context),replay.as_deref(),budget).await
    }
    pub(in crate::self_improve::acceptance::amendment) fn validate_decision(&self,decision:&Decision)->Result<()> {
        ensure!(decision.previous_contract_hash==self.old_contract && !decision.reason.trim().is_empty(),"authority decision is not bound to the staged correction");
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

pub(super) async fn stage_replay(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement)->Result<StagedCorrection> {
    stage_with_mode(root,workflow,episode,measurement,true,|original,control,prepared,_|Box::pin(async move {
        baseline_witness::assert_current(original,control,prepared)
    })).await
}
async fn stage_with<Q>(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,qualify:Q)->Result<StagedCorrection>
where Q:for<'a> FnOnce(&'a Path,&'a Path,&'a PreparedAcceptance,&'a workflow::WorkflowGuard)->std::pin::Pin<Box<dyn std::future::Future<Output=Result<()>>+Send+'a>> {
    stage_with_mode(root,workflow,episode,measurement,false,qualify).await
}
async fn stage_with_mode<Q>(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,replay_only:bool,qualify:Q)->Result<StagedCorrection>
where Q:for<'a> FnOnce(&'a Path,&'a Path,&'a PreparedAcceptance,&'a workflow::WorkflowGuard)->std::pin::Pin<Box<dyn std::future::Future<Output=Result<()>>+Send+'a>> {
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
        ensure!(!replay_only,initial_repair::Hold::EvidenceUnknown);
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
    #[ignore="real isolated original commands plus native admission; requires delegated service outside enclosing managed build lease; model replies scripted"]
    async fn approved_transition_original_execution_retains_distinct_owned_plan_and_capture()->Result<()> {
        let (root,control,old,guard,episode,_namespace)=super::super::tests::native_namespace_fixture().await;
        let mut text:serde_json::Value=serde_json::from_str(&super::super::tests::proposal(&old))?;
        text["propose_environment_transition"]=true.into();
        let measurement=owned_measurement(root.path(),&guard,&episode,&old,serde_json::to_string(&text)?).await?;
        let staged=stage_with(root.path(),&guard,&episode,&measurement,|original,control,prepared,_|Box::pin(async move {
            baseline_witness::persist_scripted(original,control,prepared,true)
        })).await?;
        let original=guard.exact_original_result_data(&staged.original_plan,&staged.original_result)?;
        let original_identity=original.identity.clone();
        let decision=Decision {approved:true,previous_contract_hash:old.baseline.registration.contract_hash().into(),
            approve_environment_transition:true,approve_additional_files:vec![],
            reason:"Independent scripted reviewer approves only the exact measured proposal and its explicit environment mode".into(),
            proposal:Some(measurement.proposal().clone())};
        let raw=serde_json::to_string(&decision)?;let raw=&raw;let workflow=&guard;
        let reviewed=adjudication::review_with(root.path(),workflow,&episode,Some(&staged),|_|Ok(augmentagent_channel_core::ProviderKind::Codex),
            "Complete original request and exact qualified proposal","approved-mode-input","approved-mode-epoch",|provider,_,_|async move {
                let session=augmentagent_channel_core::native_session::current_for(provider)?.context("scripted authority scope absent")?;
                let mut native=session.begin(provider)?;workflow.audit_logger().model_call_admitted(provider.name())?;
                native.observe(&format!("scripted-approved-mode-review-{}",uuid::Uuid::new_v4()))?;native.finish()?;
                Ok(raw.clone())
            }).await?;
        staged.validate_decision(&reviewed.0)?;
        ensure!(reviewed.0.approved && reviewed.0.approve_environment_transition,"fixture lacks the exact approved mode");
        let candidate_environment=environment_id(root.path())?;
        let capability=reviewed.4.as_ref().context("native authority did not return a live approved capability")?;
        let mut rewritten=measurement.proposal().clone();rewritten.contract.revision+=1;
        ensure!(super::super::super::observe_approved_original(&staged.original,&rewritten,
            episode.original_base(),episode.original_environment(),&candidate_environment,capability,&guard).await.is_err(),
            "approved capability accepted a proposal the authority did not review");
        ensure!(super::super::super::observe_approved_original(&staged.original,measurement.proposal(),
            episode.original_base(),episode.original_environment(),"unreviewed candidate environment",capability,&guard).await.is_err(),
            "approved capability accepted another candidate environment");
        ensure!(guard.verifier_evidence_page("original_verifier_command_admission",0)?.len()==1,
            "rejected approved-registration inputs dispatched a command");
        let baseline=super::super::super::observe_approved_original(&staged.original,measurement.proposal(),
            episode.original_base(),episode.original_environment(),&candidate_environment,
            capability,&guard).await?;
        ensure!(baseline.registration.has_environment_transition(),"approved registration lost the transition mode");
        ensure!(baseline.registration.baseline_result(&baseline.receipts)?==BaselineResult::Reproduced,"actual approved original did not reproduce intended red");
        let plans=guard.verifier_evidence_page("original_verifier_plan",0)?;
        ensure!(plans.len()==2,"approved environment registration executed without its own original plan/capture ownership");
        let plan=plans.iter().find(|p|p.key!=staged.original_plan).context("approved execution relabeled its provisional plan")?;
        let results=guard.verifier_evidence_page("original_verifier_result",0)?;
        let result=results.iter().find(|r|serde_json::from_str::<serde_json::Value>(&r.body).ok().is_some_and(|v|v["plan"]==plan.key)).context("approved original result not retained")?;
        let retained=guard.exact_original_result_data(&plan.key,&result.key)?;
        ensure!(serde_json::to_vec(&retained.prepared.baseline)?==serde_json::to_vec(&baseline)?,"approved mode/capture/result binding differs");
        ensure!(guard.verifier_evidence_page("original_verifier_command_admission",0)?.len()==2
            && guard.verifier_evidence_page("original_verifier_command_quiescent",0)?.len()==2,"approved command did not retain separate actual custody");
        ensure!(guard.exact_original_result_data(&staged.original_plan,&staged.original_result)?.identity==original_identity,
            "approved execution changed its provisional original evidence");
        ensure!(verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard)?.is_empty(),
            "approved correction leaked into an initial-scoping grant");
        ensure!(guard.epoch_attempts("verifier-amend-total","approved-mode-epoch")?==1,"approved execution spent another authority strategy attempt");
        guard.finish()?;Ok(())
    }

    #[cfg(target_os="linux")]
    #[tokio::test]
    #[ignore="delegated original execution plus native admission accounting; run built binary outside cargo-managed lease; model replies scripted"]
    async fn qualified_authority_setup_refusal_retries_reserved_intent_after_restart_without_extra_attempt()->Result<()> {
        let (root,control,old,mut guard,episode,_namespace)=super::super::tests::native_namespace_fixture().await;
        let measurement=owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await?;
        let staged=stage_with(root.path(),&guard,&episode,&measurement,|original,control,prepared,_|Box::pin(async move {
            baseline_witness::persist_scripted(original,control,prepared,true)
        })).await?;
        let calls=std::sync::atomic::AtomicUsize::new(0);let calls=&calls;
        let first=adjudication::review_with(root.path(),&guard,&episode,Some(&staged),|_|Ok(augmentagent_channel_core::ProviderKind::Codex),
            "complete owner requirements and immutable original source","setup-input","setup-epoch",|_,_,_| {
                calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                async {anyhow::bail!("scripted runtime capacity refusal before any native admission")}
            }).await;
        ensure!(first.is_err() && calls.load(std::sync::atomic::Ordering::SeqCst)==1,"setup refusal fixture did not run once");
        ensure!(guard.epoch_attempts("verifier-amend-total","setup-epoch")?==1,"fixture lost its original strategy reservation");
        let repository=guard.repository().to_string();guard.finish()?;
        let mut guard=workflow::WorkflowGuard::acquire_fixture(&control.path().join("workflow.db"),workflow::Start {
            repo_root:root.path(),worktree:root.path(),repository:&repository,source_issue:None,pr:Some(840),branch:"fixture",
            contract:"Complete original owner request",environment:"fixture"}).await?.context("fixture workflow did not reacquire")?;
        guard.bind_acceptance_for_recovery(&old)?;
        let raw=serde_json::to_string(&Decision {approved:false,previous_contract_hash:old.baseline.registration.contract_hash().into(),
            approve_environment_transition:false,approve_additional_files:vec![],reason:"Independent scripted reviewer legitimately declines this correction".into(),proposal:None})?;
        let raw=&raw;let workflow=&guard;
        let second=adjudication::review_with(root.path(),workflow,&episode,Some(&staged),|_|Ok(augmentagent_channel_core::ProviderKind::Codex),
            "complete owner requirements and immutable original source","setup-input","setup-epoch",|provider,_,_|async move {
                calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                let session=augmentagent_channel_core::native_session::current_for(provider)?.context("fixture native scope absent")?;
                let mut native=session.begin(provider)?;workflow.audit_logger().model_call_admitted(provider.name())?;
                native.observe(&format!("scripted-setup-recovery-{}",uuid::Uuid::new_v4()))?;native.finish()?;
                Ok(raw.clone())
            }).await;
        ensure!(calls.load(std::sync::atomic::Ordering::SeqCst)==2,"known zero-admission setup refusal became a permanent unknown-outcome hold: {second:?}");
        ensure!(!second?.0.approved,"fixture changed a legitimate refusal into approval");
        ensure!(guard.epoch_attempts("verifier-amend-total","setup-epoch")?==1,"setup recovery spent a second total attempt");
        let state=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db"))?.snapshot(guard.id())?.context("fixture workflow missing")?;
        ensure!(state.model_calls==2,"setup refusal/recovery charged anything beyond constructor plus one native reviewer");
        guard.finish()?;Ok(())
    }

    #[cfg(target_os="linux")]
    #[tokio::test]
    #[ignore="actual isolated original execution owns managed storage; run built test outside cargo-managed lease; constructor and witness replies are scripted"]
    async fn isolated_constructed_stage_requires_real_red_current_witness_and_reuses_original_result()->Result<()> {
        // The native transport/accounting is exercised with a scripted provider
        // reply. Original behavior executes for real in the pinned sandbox;
        // qualification data is explicitly test-only, never live acceptance.
        for qualification in ["positive","absent","negative","already-green"] {
            let (root,control,old,mut guard,episode,_namespace)=super::super::tests::native_namespace_fixture().await;
            let mut text=super::super::tests::proposal(&old);
            if qualification=="already-green" {
                let mut raw:serde_json::Value=serde_json::from_str(&text)?;
                raw["proposal"]["contract"]["verifiers"][0]["command"]["oracle"]["value"]=false.into();
                text=serde_json::to_string(&raw)?;
            }
            let measurement=owned_measurement(root.path(),&guard,&episode,&old,text).await?;
            require_retained_constructor_custody(root.path(),guard.repository(),guard.id(),&guard.construction_native_custody()?)
                .map_err(|error|error.context("fixture constructor must satisfy the real cross-model namespace guard"))?;
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
                    // Exercise the real qualified authority boundary at its
                    // fourth and last attempt. The native reply is scripted;
                    // original execution and protected accounting are real.
                    for _ in 0..3 {ensure!(guard.admit_epoch_attempt("verifier-amend-total","fixture-authority",4)?,"fixture setup admission failed");}
                    let calls=std::sync::atomic::AtomicUsize::new(0);let calls=&calls;
                    let exact=Decision {approved:true,previous_contract_hash:old.baseline.registration.contract_hash().into(),
                        approve_environment_transition:false,approve_additional_files:vec![],reason:"Independent scripted authority approves only this exact measured correction".into(),proposal:Some(measurement.proposal().clone())};
                    let raw=serde_json::to_string(&exact)?;let raw=&raw;let workflow=&guard;
                    let context=authority_request_context("fixture",guard.original_owner_request()?,None)?;let context=&context;
                    // Fail the index publication after physical native bytes
                    // are durable. Only this disposable fixture DB is touched.
                    let fault=rusqlite::Connection::open(control.path().join("workflow.db"))?;
                    fault.execute_batch("CREATE TRIGGER refuse_authority_index BEFORE INSERT ON scope_stage_response WHEN json_valid(NEW.response) AND json_extract(NEW.response,'$.version')=2 AND json_extract(NEW.response,'$.response_sha256') IS NOT NULL BEGIN SELECT RAISE(ABORT,'scripted authority index publication failure'); END;")?;
                    let first=adjudication::review_with(root.path(),workflow,&episode,Some(&staged),|_|Ok(augmentagent_channel_core::ProviderKind::Codex),
                        context,"fixture-input","fixture-authority",|provider,_,request|async move {
                            ensure!(request.contains("qualified_original_witness") && request.contains("original_plan") && request.contains("READY = False"),"authority lost staged original evidence");
                            ensure!(request.contains(workflow.original_owner_request()?),"authority lost the complete original owner request");
                            calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                            let session=augmentagent_channel_core::native_session::current_for(provider)?.context("scripted authority scope absent")?;
                            let mut native=session.begin(provider)?;workflow.audit_logger().model_call_admitted(provider.name())?;
                            native.observe(&format!("scripted-independent-authority-{}",uuid::Uuid::new_v4()))?;native.finish()?;
                            Ok(raw.clone())
                        }).await;
                    fault.execute_batch("DROP TRIGGER refuse_authority_index;")?;
                    ensure!(calls.load(std::sync::atomic::Ordering::SeqCst)==1,"fixture did not make one fourth-attempt callback");
                    let first_error=first.err().context("fixture did not simulate post-completion caller failure")?;
                    ensure!(format!("{first_error:#}").contains("scripted authority index publication failure"),"authority fixture failed before durable output: {first_error:#}");
                    eprintln!("Staged authority fixture: fourth native output completed before simulated caller failure");
                    ensure!(guard.epoch_attempts("verifier-amend-total","fixture-authority")?==4,"qualified review did not use existing total allowance");
                    let replay=adjudication::review_with(root.path(),&guard,&episode,Some(&staged),|_|Err("scripted provider unavailable today".into()),
                        "changed transient defaults must not replace the frozen request","fixture-input","fixture-authority",|_,_,_|async {anyhow::bail!("completed fourth review dispatched another native call")}).await.map_err(|error|error.context("replay completed fourth authority output"))?;
                    eprintln!("Staged authority fixture: cached fourth decision replayed while provider unavailable");
                    ensure!(serde_json::to_vec(&replay.0)?==serde_json::to_vec(&exact)? && replay.1=="codex","replay changed the completed decision or observed provider");
                    ensure!(guard.epoch_attempts("verifier-amend-total","fixture-authority")?==4,"replay spent a fifth authority attempt");
                    let outputs=std::fs::read_dir(control.path())?.map(|e|e.map(|e|e.path())).collect::<std::io::Result<Vec<_>>>()?;
                    let output=outputs.into_iter().find(|p|p.file_name().is_some_and(|name|name.to_string_lossy().starts_with("authority-") && name.to_string_lossy().ends_with(".response"))).context("retained authority output absent")?;
                    let bytes=std::fs::read(&output)?;std::fs::remove_file(&output)?;
                    let missing=adjudication::review_with(root.path(),&guard,&episode,Some(&staged),|_|Ok(augmentagent_channel_core::ProviderKind::Codex),
                        "Complete unchanged owner requirements and authenticated source data","fixture-input","fixture-authority",|_,_,_|async {anyhow::bail!("missing authority output dispatched another native call")}).await;
                    ensure!(missing.is_err() && guard.epoch_attempts("verifier-amend-total","fixture-authority")?==4,"lost authority output replenished review allowance");
                    {
                        use std::io::Write;use std::os::unix::fs::OpenOptionsExt;
                        let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(output)?;file.write_all(&bytes)?;file.sync_all()?;
                    }
                    ensure!(calls.load(std::sync::atomic::Ordering::SeqCst)==1,"retained authority result was re-queried");
                    let counted=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db"))?.snapshot(guard.id())?.context("fixture workflow disappeared")?;
                    ensure!(counted.model_calls==2,"authority replay changed actual model accounting");
                    eprintln!("Staged authority fixture: missing native output held without new calls; restored original bytes");
                    drop(staged);
                    let replay=stage_with(root.path(),&guard,&episode,&measurement,|original,control,prepared,_| {
                        Box::pin(async move {baseline_witness::assert_current(original,control,prepared)})
                    }).await.map_err(|error|error.context("reconstruct original stage from retained result after authority"))?;
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
                    replay.require_current(root.path(),&guard,&episode).await.map_err(|error|error.context("restore original capture after staged refusal"))?;
                    eprintln!("Staged authority fixture: original capture loss refused; original bytes restored");
                    let previous_identity=replay.adjudication_identity(root.path(),&guard,&episode).await?;
                    guard.checkpoint(crate::autoship_workflow::Stage::Scope,"fixture_binding_metadata_changed","reconstruct_staged_correction",0,&["new-routing-evidence".into()]).await?;
                    ensure!(replay.require_current(root.path(),&guard,&episode).await.is_err(),"old stage ignored its full binding fence");
                    let replay=stage_with(root.path(),&guard,&episode,&measurement,|original,control,prepared,_|Box::pin(async move {
                        baseline_witness::assert_current(original,control,prepared)
                    })).await.map_err(|error|error.context("reconstruct qualified stage after routing binding rotation"))?;
                    ensure!(replay.adjudication_identity(root.path(),&guard,&episode).await?==previous_identity,"binding metadata replenished the same semantic review input");
                    let cached=adjudication::review_with(root.path(),&guard,&episode,Some(&replay),|_|Err("scripted unavailable provider".into()),
                        "new routing metadata remains context only","fixture-input","fixture-authority",|_,_,_|async {anyhow::bail!("binding metadata retried the completed fourth authority call")}).await.map_err(|error|error.context("replay same completed verdict across binding metadata rotation"))?;
                    ensure!(cached.0.approved && guard.epoch_attempts("verifier-amend-total","fixture-authority")?==4,"metadata rotation changed the completed verdict or allowance");
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
