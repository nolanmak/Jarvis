//! Original execution ownership within the already charged constructor budget.
//! No initial-repair budget, new model admission, or verifier approval is granted.
use super::*;

const KIND:&str="construction_original_plan_owner";
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {version:u32,kind:String,workflow:String,plan:String,episode:String,slot:u32,intent_sha256:String}
fn owner_key(plan:&str)->String {digest(&serde_json::to_vec(&("construction-original-plan-owner-v1",plan)).unwrap())}
fn owner(workflow:&workflow::WorkflowGuard,plan:&str)->Result<Option<Owner>> {
    let key=owner_key(plan);
    let raw=workflow.recorded_scope_stage_response(&key)?;
    let indexed=workflow.verifier_evidence(&key,KIND)?;
    ensure!(raw==indexed,initial_repair::Hold::EvidenceUnknown);
    raw.map(|raw|->Result<Owner> {
        let value:Owner=serde_json::from_str(&raw)?;
        ensure!(value.version==1 && value.kind==KIND && value.workflow==workflow.id()
            && value.plan==plan && value.slot<SLOTS,initial_repair::Hold::EvidenceUnknown);
        Ok(value)
    }).transpose()
}

/// Historical ownership only. Rejected intents remain charged and owned; no
/// current enrollment or positive witness is imported by this reader.
pub(super) fn owned(root:&Path,control:&Path,workflow:&workflow::WorkflowGuard,key:&str,body:&str)->Result<bool> {
    (||->Result<bool> {
        let Some(owner)=owner(workflow,key)? else{return Ok(false)};
        let plan=verifier_execution::Plan::checked(workflow.id(),key,body)?;
        let (binding,base,environment,proposal)=plan.inputs();
        ensure!(binding.contract.len()==64 && binding.contract.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),initial_repair::Hold::EvidenceUnknown);
        let expected=digest(&serde_json::to_vec(&("verifier-construction-episode-v1",workflow.id(),proposal.contract.source()?,base,&binding.contract,environment))?);
        ensure!(owner.episode==expected,initial_repair::Hold::EvidenceUnknown);
        let measurement:Measurement=read(workflow,&intent_key(&owner.episode,owner.slot))?.context(initial_repair::Hold::EvidenceUnknown)?;
        ensure!(measurement.version==1 && measurement.episode==owner.episode && measurement.slot==owner.slot
            && measurement.round<TURNS && measurement.identity()?==owner.intent_sha256
            && serde_json::to_vec(measurement.proposal())?==serde_json::to_vec(proposal)?,initial_repair::Hold::EvidenceUnknown);
        let turn:Turn=read(workflow,&turn_key(&owner.episode,measurement.round))?.context(initial_repair::Hold::EvidenceUnknown)?;
        ensure!(turn.version==1 && turn.episode==owner.episode && turn.round==measurement.round
            && turn.input.identity()?==measurement.input_sha256,initial_repair::Hold::EvidenceUnknown);
        turn.input.require_history(workflow.id(),base,&owner.episode)?;
        let reply=constructor_native::retained_in(root,control,workflow,&turn.input)?;
        ensure!(digest(reply.text.as_bytes())==measurement.response_sha256 && reply.provider==measurement.provider
            && reply.native_session==measurement.native_session,initial_repair::Hold::EvidenceUnknown);
        let raw=reply.text.trim();let raw=raw.strip_prefix("```json\n").and_then(|s|s.strip_suffix("```")).unwrap_or(raw);
        let Output::Proposal(draft)=serde_json::from_str::<Output>(raw)? else {anyhow::bail!(initial_repair::Hold::EvidenceUnknown)};
        ensure!(serde_json::to_vec(&draft)?==serde_json::to_vec(&measurement.draft)?,initial_repair::Hold::EvidenceUnknown);
        Ok(true)
    })().map_err(|error|error.context(initial_repair::Hold::EvidenceUnknown))
}

/// Pin the existing constructor charge to its original plan before the plan or
/// any command is admitted. Never retroactively relabel an initial/orphan plan.
pub(super) fn reserve(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,key:&str,body:&str)->Result<()> {
    staging::authenticate(root,workflow,episode,measurement)?;
    let plan=verifier_execution::Plan::checked(workflow.id(),key,body)?;
    let (binding,base,environment,proposal)=plan.inputs();
    ensure!(binding==workflow.binding() && base==episode.original_base() && environment==episode.original_environment()
        && serde_json::to_vec(proposal)?==serde_json::to_vec(measurement.proposal())?,initial_repair::Hold::EvidenceUnknown);
    let existing=owner(workflow,key)?;
    let indexed=workflow.verifier_evidence(key,"original_verifier_plan")?;
    let recorded=workflow.recorded_scope_stage_response(key)?;
    ensure!(indexed==recorded && indexed.as_deref().is_none_or(|old|old==body),initial_repair::Hold::EvidenceUnknown);
    let ownership=if existing.is_some() {
        workflow.verifier_evidence(&owner_key(key),KIND)?.context(initial_repair::Hold::EvidenceUnknown)?
    }else {
        ensure!(indexed.is_none(),"existing original plan cannot be reclassified as construction ownership");
        serde_json::to_string(&Owner {version:1,kind:KIND.into(),workflow:workflow.id().into(),plan:key.into(),episode:episode.key().into(),slot:measurement.slot,intent_sha256:measurement.identity()?})?
    };
    workflow.record_constructed_original_plan(key,body,&owner_key(key),&ownership)
        .map_err(|error|error.context(initial_repair::Hold::EvidenceUnknown))?;
    ensure!(owned(root,episode.control(),workflow,key,body)?,initial_repair::Hold::EvidenceUnknown);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn constructed_plan_publication_failure_does_not_orphan_ownership_or_spend_another_charge() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let measurement=super::super::staging::tests::owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
        let executor=verifier_executor_id().unwrap();
        let (key,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        let conn=rusqlite::Connection::open(control.path().join("workflow.db")).unwrap();
        // Fault injection touches only this temporary fixture database. Fail
        // the second logical write, after ownership would have been inserted.
        conn.execute_batch(&format!("CREATE TRIGGER deny_constructed_plan BEFORE INSERT ON scope_stage_response WHEN NEW.key='{key}' BEGIN SELECT RAISE(ABORT,'scripted original-plan publication failure'); END;")).unwrap();
        let store=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
        let before=store.snapshot(guard.id()).unwrap().unwrap();let events=store.events(guard.id()).unwrap();
        let outcome=(||->Result<()> {
            reserve(root.path(),&guard,&episode,&measurement,&key,&body)?;
            verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor)?;
            Ok(())
        })();
        assert!(outcome.is_err(),"publication fault did not reach the actual constructed-plan path");
        assert!(owner(&guard,&key).unwrap().is_none(),"failed plan publication left an orphan ownership receipt");
        assert!(guard.verifier_evidence(&key,"original_verifier_plan").unwrap().is_none());
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap(),before);
        assert_eq!(store.events(guard.id()).unwrap(),events,"rolled-back publication fabricated durable progress");
        conn.execute_batch("DROP TRIGGER deny_constructed_plan;").unwrap();
        reserve(root.path(),&guard,&episode,&measurement,&key,&body).unwrap();
        verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        assert!(owned(root.path(),episode.control(),&guard,&key,&body).unwrap());
        assert_eq!(guard.verifier_evidence_page(KIND,0).unwrap().len(),1);
        assert_eq!(guard.verifier_evidence_page("original_verifier_plan",0).unwrap().len(),1);
        assert!(guard.verifier_evidence_page("original_verifier_command_admission",0).unwrap().is_empty());
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,before.model_calls,"publication retry added a native charge");
        drop(store);drop(conn);guard.finish().unwrap();
    }

    #[tokio::test]
    async fn constructed_original_plan_is_not_misclassified_as_unowned_initial_budget() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let measurement=super::super::staging::tests::owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
        let executor=verifier_executor_id().unwrap();
        let (plan,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap();
        verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        let contexts=verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard).unwrap();
        assert!(contexts.is_empty(),"constructor-owned measurement was assigned an unowned initial-repair budget: {contexts:?}");
        assert_eq!(guard.verifier_evidence_page(KIND,0).unwrap().len(),1);
        reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap();
        assert_eq!(guard.verifier_evidence_page(KIND,0).unwrap().len(),1,"replay wrote another ownership fact");
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn constructor_ownership_cannot_relabel_an_existing_initial_or_orphan_plan() {
        let (root,control,old,guard,episode)=super::super::tests::fixture().await;
        let measurement=super::super::staging::tests::owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
        let executor=verifier_executor_id().unwrap();
        let (plan,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        let error=reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap_err();
        assert!(error.to_string().contains("cannot be reclassified"));
        assert!(owner(&guard,&plan).unwrap().is_none());
        assert_eq!(verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard).unwrap().len(),1);
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn rejected_and_historical_constructor_plan_ownership_preserves_original_charge() {
        let (root,control,old,mut guard,episode)=super::super::tests::fixture().await;
        let measurement=super::super::staging::tests::owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
        let executor=verifier_executor_id().unwrap();
        let (plan,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap();
        verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
        reject(&guard,&measurement,&BaselineNotRed {kind:Some(initial_repair::FailureKind::NotReproduced),detail:"scripted completed semantic rejection".into()}).unwrap();
        let mut later=old.clone();later.proposal.contract.revision+=1;
        later.baseline.registration=RegisteredContract::register(root.path(),later.proposal.contract.clone(),episode.original_base().into(),episode.original_environment().into()).unwrap();
        guard.bind_acceptance_for_recovery(&later).unwrap();
        guard.checkpoint(crate::autoship_workflow::Stage::Scope,"new_registered_context","qualify_original",0,&[]).await.unwrap();
        assert!(episode.require_current(root.path(),&guard).is_err());
        assert!(verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard).unwrap().is_empty());
        assert_eq!(guard.verifier_evidence_page(KIND,0).unwrap().len(),1);
        let snapshot=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap();
        assert_eq!(snapshot.model_calls,1,"historical ownership reset or added a charge");
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn invalid_or_incomplete_constructor_ownership_holds_instead_of_refilling_initial_budget() {
        for fault in ["forged-marker","missing-role-index","lost-native-proof"] {
            let (root,control,old,guard,episode)=super::super::tests::fixture().await;
            let measurement=super::super::staging::tests::owned_measurement(root.path(),&guard,&episode,&old,super::super::tests::proposal(&old)).await.unwrap();
            let executor=verifier_executor_id().unwrap();
            let (plan,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
            match fault {
                "forged-marker"=>{
                    let value=Owner {version:1,kind:KIND.into(),workflow:guard.id().into(),plan:plan.clone(),episode:"a".repeat(64),slot:0,intent_sha256:measurement.identity().unwrap()};
                    guard.record_verifier_evidence(&owner_key(&plan),KIND,&serde_json::to_string(&value).unwrap()).unwrap();
                }
                "missing-role-index"=>{
                    let value=Owner {version:1,kind:KIND.into(),workflow:guard.id().into(),plan:plan.clone(),episode:episode.key().into(),slot:0,intent_sha256:measurement.identity().unwrap()};
                    guard.record_scope_stage_response(&owner_key(&plan),&serde_json::to_string(&value).unwrap()).unwrap();
                }
                _=>{
                    reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap();
                    let observation=guard.construction_native_custody().unwrap().pop().unwrap();
                    let reference:serde_json::Value=serde_json::from_str(&observation.provenance).unwrap();
                    std::fs::remove_file(reference["artifact"].as_str().unwrap()).unwrap();
                }
            }
            verifier_execution::OriginalExecution::begin(&guard,control.path(),measurement.proposal(),episode.original_base(),episode.original_environment(),&executor).unwrap();
            assert!(verifier_execution::retained_original_plan_budget_contexts(root.path(),control.path(),&guard).is_err(),"{fault} manufactured a fresh initial budget context");
            let snapshot=crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(snapshot.model_calls,1);
            guard.finish().unwrap();
        }
    }
}
