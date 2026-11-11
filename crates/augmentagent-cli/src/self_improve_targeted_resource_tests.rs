//! Real durable bindings at the initial resource-observation boundary.
use super::*;
use crate::autoship_workflow::{Binding,Stage,WorkflowStore,submission::{Request,Milestone}};
use augmentagent_channel_core::run_resources::Readiness;

fn builder_capacity() -> Readiness {
    Readiness {status:"capacity-exhausted".into(),permits_attempt:false,live_verified:false,
        policy:Some(serde_json::json!({"memory_bytes":6_u64*1024*1024*1024,"pids":512,"cpu_percent":200})),
        observation:Some(serde_json::json!({"status":"capacity-exhausted","permits_attempt":false,"live_verified":false,
            "refusal":{"phase":"admission_failed","exhausted":true,"refusal_code":"host_headroom",
                "facts":{"requested_bytes":6442450944_u64,"available_bytes":6140776448_u64,
                    "required_bytes":6710886400_u64,"margin_bytes":268435456_u64,"reservation_slack_bytes":0}}}))}
}

fn fixture(path:&Path,stage:Stage,action:&str)->(WorkflowStore,adoption::AdoptedPr) {
    let mut store=WorkflowStore::open(path).unwrap();
    let request=Request {repository:"owner/repo".into(),target:Target::Issue(7),provider:None,until:Milestone::Merged};
    let id=store.submit_authorized("fixture","github:owner",&request,1).unwrap().workflow;
    let binding=Binding {head:"a".repeat(40),contract:"original-contract".into(),evidence_hash:"original-evidence".into()};
    store.register(&id,"agent-fix/issue-7",binding,1).unwrap();
    let lease=store.claim(&id,"fixture",2,10000).unwrap().unwrap();
    store.attach_pr_in_repo(&lease,"owner/repo",42,2).unwrap();
    assert_eq!(store.resolve_pr("owner/repo",42).unwrap().as_deref(),Some(id.as_str()));
    let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
    checkpoint.stage=stage;checkpoint.next.action=action.into();checkpoint.next.reason="verification_infrastructure_retry".into();
    checkpoint.next.eligible_at_ms=3;
    store.checkpoint(&lease,&checkpoint,3).unwrap();store.release(&lease,4).unwrap();
    let pr=adoption::AdoptedPr {repository:"owner/repo".into(),pr:42,source_issue:Some(7),branch:"agent-fix/issue-7".into(),
        head:"a".repeat(40),base:"main".into(),author:"owner".into(),title:"Fixture".into(),body:"Fixture".into(),
        dependency:adoption::Dependency::Main};
    (store,pr)
}

#[test]
fn exact_head_verification_does_not_require_builder_capacity_or_mutate_accounting() {
    let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
    let (store,pr)=fixture(&path,Stage::Verify,"retry_verification");drop(store);
    let before=std::fs::read(&path).unwrap();
    let resources=builder_capacity();
    assert!(resources.preflight().is_err(),"the normal builder still lacks capacity");
    preflight_selected_resources(&path,Some(&pr),&resources,5).unwrap();
    assert!(resources.preflight().is_err(),"verification did not create builder capacity");
    assert_eq!(std::fs::read(&path).unwrap(),before,"resource observation changed protected history");
    let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
    let snapshot=store.snapshot("owner/repo#7").unwrap().unwrap();
    assert_eq!(snapshot.checkpoint.stage,Stage::Verify);
    assert_eq!(snapshot.model_calls,0);
    assert_eq!(store.merge_allowance(5,5).unwrap().confirmed,0);
}

#[test]
fn verification_cannot_skip_invalid_or_unknown_resource_configuration() {
    let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
    let (store,pr)=fixture(&path,Stage::Verify,"run_verifier");drop(store);
    let before=std::fs::read(&path).unwrap();
    for case in ["missing","invalid","busy","unknown","missing-observation","wrong-code","mismatched-status","wrong-phase"] {
        let mut resources=builder_capacity();
        match case {
            "missing"=>resources.status="declaration-missing".into(),
            "invalid"=>resources.status="declaration-invalid".into(),
            "busy"=>resources.status="inspection-busy".into(),
            "unknown"=>resources.status="inspection-unavailable".into(),
            "missing-observation"=>resources.observation=None,
            "wrong-code"=>resources.observation.as_mut().unwrap()["refusal"]["refusal_code"]="invalid_policy".into(),
            "mismatched-status"=>resources.observation.as_mut().unwrap()["status"]="available".into(),
            "wrong-phase"=>resources.observation.as_mut().unwrap()["refusal"]["phase"]="completed".into(),
            _=>unreachable!(),
        }
        assert!(preflight_selected_resources(&path,Some(&pr),&resources,5).is_err(),"{case}");
        assert_eq!(std::fs::read(&path).unwrap(),before,"{case} changed protected state");
    }
    assert!(preflight_selected_resources(&path,None,&builder_capacity(),5).is_err());
    let absent=directory.path().join("missing.db");
    assert!(preflight_selected_resources(&absent,Some(&pr),&builder_capacity(),5).is_err());
    assert!(!absent.exists());
}

#[test]
fn stale_or_non_verification_work_retains_normal_builder_preflight() {
    for case in ["new-head","unknown-pr","builder","review","repair","paused","leased","pending-merge","future"] {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
        let stage=match case {"builder"=>Stage::Build,"review"=>Stage::Review,_=>Stage::Verify};
        let action=if case=="repair" {"repair_candidate"} else {"retry_verification"};
        let (mut store,mut pr)=fixture(&path,stage,action);
        if case=="new-head" {pr.head="b".repeat(40);}
        if case=="unknown-pr" {pr.pr=43;}
        if case=="paused" {store.set_owner_pause("owner/repo#7","github:owner",0,true,5).unwrap();}
        if matches!(case,"leased"|"pending-merge"|"future") {
            let lease=store.claim("owner/repo#7","other",5,10000).unwrap().unwrap();
            if case=="pending-merge" {
                let binding=store.snapshot("owner/repo#7").unwrap().unwrap().checkpoint.binding;
                store.begin_operation(&lease,crate::autoship_workflow::OperationKind::MergePr,&binding,5).unwrap();
                store.release(&lease,5).unwrap();
            }
            if case=="future" {
                let mut checkpoint=store.snapshot("owner/repo#7").unwrap().unwrap().checkpoint;
                checkpoint.next.eligible_at_ms=100;store.checkpoint(&lease,&checkpoint,5).unwrap();store.release(&lease,5).unwrap();
            }
        }
        drop(store);let before=std::fs::read(&path).unwrap();
        assert!(preflight_selected_resources(&path,Some(&pr),&builder_capacity(),6).is_err(),"{case}");
        assert_eq!(std::fs::read(&path).unwrap(),before,"{case} changed protected state");
    }
}

/// Read-only live preflight: starts no workflow, model, build or merge attempt.
#[tokio::test]
#[ignore="requires declared native runtime, delegated resources and an existing authorized verification PR"]
async fn live_verification_preflight_under_builder_pressure_is_read_only()->Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let repo=PathBuf::from(std::env::var("AUTOSHIP_PREFLIGHT_LIVE_REPO")?).canonicalize()?;
    let evidence=PathBuf::from(std::env::var("AUTOSHIP_PREFLIGHT_LIVE_EVIDENCE")?);
    anyhow::ensure!(evidence.is_absolute() && !evidence.starts_with(&repo),"private evidence required");
    std::fs::create_dir(&evidence)?;
    std::fs::set_permissions(&evidence,std::fs::Permissions::from_mode(0o700))?;
    let number=std::env::var("AUTOSHIP_PREFLIGHT_LIVE_PR")?.parse::<u64>()?;
    let request=Request {repository:workflow::canonical_repository(&repo).await?,target:Target::Pr(number),provider:None,until:Milestone::Merged};
    let Resolved::Pr(pr)=resolve(&repo,&request).await? else {anyhow::bail!("expected authorized PR")};
    let path=workflow::path();let store=WorkflowStore::open_read_only(&path)?.context("existing workflow missing")?;
    let id=store.resolve_pr(&pr.repository,pr.pr)?.context("canonical PR binding missing")?;
    let before=store.snapshot(&id)?.context("existing snapshot missing")?;drop(store);
    let resources=augmentagent_channel_core::run_resources::readiness().await;
    anyhow::ensure!(resources.status=="capacity-exhausted" && resources.preflight().is_err(),
        "live capacity-pressure precondition absent");
    // The exact same real observation must defer to CI's own launch admission.
    preflight_selected_resources(&path,Some(&pr),&resources,chrono::Utc::now().timestamp_millis())?;
    execution_preflight(&repo,Some(request.target),ExecutionMode::Local,Some(&pr)).await?;
    let store=WorkflowStore::open_read_only(&path)?.context("workflow disappeared")?;
    let after=store.snapshot(&id)?.context("snapshot disappeared")?;
    anyhow::ensure!(before.checkpoint==after.checkpoint && before.attempts==after.attempts
        && before.model_calls==after.model_calls && before.tokens==after.tokens,
        "workflow changed concurrently during read-only observation");
    std::fs::write(evidence.join("receipt.json"),serde_json::to_vec_pretty(&serde_json::json!({
        "version":1,"kind":"read_only_live_verification_resource_preflight","workflow":id,"head":pr.head,
        "builder_observation":resources,"initial_local_preflight":"passed","workflow_started":false,
        "model_calls_before":before.model_calls,"model_calls_after":after.model_calls,
        "attempts_before":before.attempts,"attempts_after":after.attempts,
        "production_runtime_changed":false,"shipping_authority":false,"workflow_complete":false
    }))?)?;
    Ok(())
}


#[test]
fn preworkflow_resource_refusal_survives_worker_completion_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let request = Request {repository:"owner/repo".into(), target:Target::Pr(42),
        provider:None, until:Milestone::Merged};
    let id = store.submit_authorized("resource-observation", "github:owner", &request, 1).unwrap().workflow;
    assert_eq!(store.next_command_job("owner/repo", 2).unwrap().as_deref(), Some(id.as_str()));
    let resources = builder_capacity();
    let error = preflight_selected_resources(&path, None, &resources, 3)
        .context(CommandFailure {code:3, reason:"execution-readiness-blocked"}).unwrap_err();
    assert!(format!("{error:#}").contains("workflow-resource:capacity-exhausted"));
    let result = Err(error);
    assert!(super::super::finish_selected_command_attempt(&store, &id, 2, &result, 4).unwrap());
    drop(store);
    let store = WorkflowStore::open_read_only(&path).unwrap().unwrap();
    assert!(store.snapshot(&id).unwrap().is_none(), "reporting registered execution");
    let (code, status) = crate::autoship_commands::status(&path, &id, 5, 5);
    assert_eq!(code, 3);
    assert_eq!(status["reason"], "execution-readiness-blocked");
    assert_eq!(status["last_attempt"]["resource_refusal"]["refusal"]["refusal_code"],
        "host_headroom", "worker completion lost the original resource metric: {status}");
    assert_eq!(status["last_attempt"]["resource_refusal"]["refusal"]["facts"]["required_bytes"],
        6710886400_u64);
    let progress = serde_json::to_value(store.progress_events(&id).unwrap().last().unwrap()).unwrap();
    assert_eq!(progress["resource_refusal"]["observation"], status["last_attempt"]["resource_refusal"]);
    assert_eq!(progress["resource_refusal"]["selected_at_ms"], 2);
}

#[test]
fn resource_receipt_cannot_override_pause_or_authorization_priority() {
    for unauthorized in [false,true] {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let request=Request {repository:"owner/repo".into(),target:Target::Pr(42),provider:None,until:Milestone::Merged};
        let id=store.submit_authorized("priority","github:owner",&request,1).unwrap().workflow;
        store.next_command_job("owner/repo",2).unwrap().unwrap();
        store.set_owner_pause(&id,"github:owner",0,true,3).unwrap();
        let error=preflight_selected_resources(&path,None,&builder_capacity(),4).unwrap_err()
            .context(CommandFailure {code:if unauthorized {2}else{3},reason:if unauthorized {"target-authorization-failed"}else{"execution-readiness-blocked"}});
        super::super::finish_selected_command_attempt(&store,&id,2,&Err(error),5).unwrap();
        let attempt=store.command_attempt(&id).unwrap().unwrap();
        assert!(attempt.resource_refusal.is_none());
        assert_eq!(attempt.reason,if unauthorized {"target-authorization-failed"}else{"owner-pause-requested"});
        let (_,status)=crate::autoship_commands::status(&path,&id,6,5);
        assert!(status["resource_refusal"].is_null());
        let progress=store.progress_events(&id).unwrap();assert!(progress.last().unwrap().resource_refusal.is_none());
    }
}
