//! Private exact original feedback. Data only; never an adjudication or cue.
use super::*;

#[derive(Clone,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {key:String,sha256:String}

const PAYLOAD_LIMIT:usize=384*1024;
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version:u32,workflow:String,episode:String,slot:u32,intent_sha256:String,
    plan:String,result:String,original_identity:String,diagnosis:Option<String>,payload_sha256:String,
}
fn record_key(episode:&str,slot:u32)->String {key("construction-exact-original-feedback-v1",episode,slot)}

fn payload(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,
    plan:&str,result:&str,identity:&str,diagnosis:Option<&str>,select_diagnosis:bool)->Result<(serde_json::Value,Option<String>)> {
    let original=workflow.exact_original_result_data(plan,result)?;
    ensure!(original.identity==identity
        && original.prepared.baseline.registration.baseline_head()==episode.original_base()
        && serde_json::to_vec(&original.prepared.proposal)?==serde_json::to_vec(measurement.proposal())?,initial_repair::Hold::EvidenceUnknown);
    let body=workflow.verifier_evidence(plan,"original_verifier_plan")?.context(initial_repair::Hold::EvidenceUnknown)?;
    let parsed=verifier_execution::Plan::checked(workflow.id(),plan,&body)?;
    let (binding,base,environment,proposal)=parsed.inputs();
    ensure!(binding==&original.binding && binding.contract==episode.registered_contract && base==episode.original_base() && environment==episode.original_environment()
        && serde_json::to_vec(proposal)?==serde_json::to_vec(measurement.proposal())?
        && accounting::owned(root,episode.control(),workflow,plan,&body)?,initial_repair::Hold::EvidenceUnknown);
    let negative=if original.prepared.baseline.registration.baseline_result(&original.prepared.baseline.receipts)?==BaselineResult::Reproduced {
        ensure!(select_diagnosis || diagnosis.is_some(),initial_repair::Hold::EvidenceUnknown);
        Some(baseline_witness::retained_proposal_diagnosis_in(root,episode.control(),&original.prepared,
            &workflow.baseline_native_custody()?,diagnosis)?.context(initial_repair::Hold::EvidenceUnknown)?)
    }else {ensure!(diagnosis.is_none(),initial_repair::Hold::EvidenceUnknown);None};
    let diagnosis_identity=negative.as_ref().map(|(identity,_)|identity.clone());
    let value=serde_json::json!({"role":"untrusted_original_measurement_data","original":original,
        "negative_witness":negative.map(|(_,raw)|serde_json::from_str::<serde_json::Value>(&raw)).transpose()?});
    ensure!(serde_json::to_vec(&value)?.len()<=PAYLOAD_LIMIT,initial_repair::Hold::EvidenceUnknown);
    Ok((value,diagnosis_identity))
}

pub(super) fn retain(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,plan:&str,result:&str,identity:&str)->Result<Reference> {
    staging::authenticate(root,workflow,episode,measurement)?;
    let (value,diagnosis)=payload(root,workflow,episode,measurement,plan,result,identity,None,true)?;
    let record=Record {version:1,workflow:workflow.id().into(),episode:episode.key().into(),slot:measurement.slot,
        intent_sha256:measurement.identity()?,plan:plan.into(),result:result.into(),original_identity:identity.into(),
        diagnosis,payload_sha256:digest(&serde_json::to_vec(&value)?)};
    let key=record_key(episode.key(),measurement.slot);
    let raw=serde_json::to_string(&record)?;
    workflow.record_scope_stage_response(&key,&raw)?;
    Ok(Reference {key,sha256:digest(raw.as_bytes())})
}
pub(super) fn load(root:&Path,workflow:&workflow::WorkflowGuard,episode:&ConstructionEpisode,measurement:&Measurement,reference:&Reference)->Result<serde_json::Value> {
    episode.require_current(root,workflow)?;
    ensure!(reference.key==record_key(episode.key(),measurement.slot),initial_repair::Hold::EvidenceUnknown);
    let raw=workflow.recorded_scope_stage_response(&reference.key)?.context(initial_repair::Hold::EvidenceUnknown)?;
    ensure!(raw.len()<=LIMIT && digest(raw.as_bytes())==reference.sha256,initial_repair::Hold::EvidenceUnknown);
    let record:Record=serde_json::from_str(&raw)?;
    ensure!(record.version==1 && record.workflow==workflow.id() && record.episode==episode.key()
        && record.slot==measurement.slot && record.intent_sha256==measurement.identity()?,initial_repair::Hold::EvidenceUnknown);
    let (value,diagnosis)=payload(root,workflow,episode,measurement,&record.plan,&record.result,&record.original_identity,record.diagnosis.as_deref(),false)?;
    ensure!(diagnosis==record.diagnosis && digest(&serde_json::to_vec(&value)?)==record.payload_sha256,initial_repair::Hold::EvidenceUnknown);
    Ok(value)
}

#[cfg(all(test,target_os="linux"))]
mod tests {
    use super::*;
    #[tokio::test]
    async fn next_constructor_request_contains_exact_captured_failure_not_only_summary() {
        let (root,control,old,mut guard,episode)=super::super::tests::fixture().await;
        let mut proposal=old.proposal.clone();proposal.contract.revision+=1;
        proposal.contract.verifiers[0].failure_witness.push_str(" under the proposed verifier setup");
        let reply=serde_json::json!({"kind":"proposal","proposal":proposal,"additional_files":[],"propose_environment_transition":false}).to_string();
        let measurement=staging::tests::owned_measurement(root.path(),&guard,&episode,&old,reply.clone()).await.unwrap();
        let (plan,body)=verifier_execution::plan_input(&guard,measurement.proposal(),episode.original_base(),episode.original_environment(),&verifier_executor_id().unwrap()).unwrap();
        accounting::reserve(root.path(),&guard,&episode,&measurement,&plan,&body).unwrap();
        let stderr=format!("{}COMPLETE ORIGINAL COMPILER DIAGNOSIS 🦀\n","error[E0599]: missing original API; quoted \"untrusted\" text\n".repeat(900));
        let observation=Observation {exit_code:Some(1),stdout:"exact setup stdout\n".into(),stderr:stderr.clone(),interrupted:false,output_complete:true};
        let (_,result)=verifier_execution::retain_captured_feedback_fixture(root.path(),control.path(),measurement.proposal(),episode.original_base(),&guard,&observation).unwrap();
        let original=guard.exact_original_result_data(&plan,&result).unwrap();
        let reference=retain(root.path(),&guard,&episode,&measurement,&plan,&result,&original.identity).unwrap();
        reject_with_reference(&guard,&measurement,&BaselineNotRed {kind:Some(initial_repair::FailureKind::UnexpectedFailure),detail:"unexpected setup failure; full evidence retained privately".into()},Some(reference.clone())).unwrap();
        let turn:Turn=read(&guard,&turn_key(episode.key(),0)).unwrap().unwrap();let previous=turn.input.identity().unwrap();
        let captured=std::cell::RefCell::new(None::<String>);
        let response=next_with(root.path(),&guard,super::super::tests::request(&episode,&old,"test-model"),|input| {
            let cached=input.identity().unwrap()==previous;let text=reply.clone();let captured=&captured;
            let root=root.path();let guard=&guard;let episode=&episode;
            async move {
                if cached {constructor_native::scripted_reply(root,guard,episode,&input,text).await}
                else {*captured.borrow_mut()=Some(input.request_text().into());anyhow::bail!("captured next constructor input before backend dispatch")}
            }
        }).await;
        assert!(response.is_err());
        let prompt:serde_json::Value=serde_json::from_str(captured.borrow().as_ref().expect("next input missing")).unwrap();
        assert_eq!(prompt["previous_steps"][0]["raw_original_feedback"]["original"]["commands"]["observe"]["stderr"].as_str(),Some(stderr.as_str()),"next native request lost complete original failure output");
        assert_eq!(crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,1);
        // A changed reference is a held read, with no repair of controller
        // records and no new constructor charge.
        let before=guard.recorded_scope_stage_response(&reference.key).unwrap();
        let forged=Reference {key:reference.key.clone(),sha256:"0".repeat(64)};
        assert!(load(root.path(),&guard,&episode,&measurement,&forged).is_err());
        assert_eq!(guard.recorded_scope_stage_response(&reference.key).unwrap(),before);
        assert!(load(root.path(),&guard,&episode,&measurement,&reference).is_ok());
        let previous_binding=guard.binding().clone();
        guard.checkpoint(crate::autoship_workflow::Stage::Scope,"changed_candidate_evidence","construct_verifier",0,&["new-candidate-context".into()]).await.unwrap();
        assert_ne!(*guard.binding(),previous_binding);
        assert!(load(root.path(),&guard,&episode,&measurement,&reference).is_ok(),"candidate binding rotation discarded authenticated original failure data");
        let command=guard.verifier_evidence_page("original_verifier_command_admission",0).unwrap().pop().unwrap();
        let admission:serde_json::Value=serde_json::from_str(&command.body).unwrap();
        let stdout=PathBuf::from(admission["capture"]["path"].as_str().unwrap()).join("stdout");
        std::fs::remove_file(stdout).unwrap();
        let held=next_with(root.path(),&guard,super::super::tests::request(&episode,&old,"test-model"),|input| {
            assert_eq!(input.identity().unwrap(),previous,"lost feedback dispatched another constructor input");
            let text=reply.clone();let root=root.path();let guard=&guard;let episode=&episode;
            async move {constructor_native::scripted_reply(root,guard,episode,&input,text).await}
        }).await;
        assert!(held.is_err(),"missing exact output became successful feedback");
        assert_eq!(guard.recorded_scope_stage_response(&reference.key).unwrap(),before);
        assert_eq!(crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,1);
        guard.finish().unwrap();
    }
}
