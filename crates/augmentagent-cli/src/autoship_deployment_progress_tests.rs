use super::*;
use super::super::discord_ingress::DiscordOrigin;
use std::process::Command;

const DAY:i64=86_400_000;
const MINUTE:i64=60_000;

/// Real temporary Git repository: base, then one merge commit per PR.
fn repo(path:&Path,merges:usize)->(String,Vec<String>) {
    let git=|args:&[&str]|{
        let out=Command::new("git").args(args).current_dir(path)
            .env("GIT_AUTHOR_NAME","t").env("GIT_AUTHOR_EMAIL","t@example.invalid")
            .env("GIT_COMMITTER_NAME","t").env("GIT_COMMITTER_EMAIL","t@example.invalid")
            .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_NOSYSTEM","1").output().unwrap();
        assert!(out.status.success(),"git {args:?}: {}",String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init","-q","-b","main"]);
    git(&["commit","-q","--allow-empty","-m","base"]);
    let base=git(&["rev-parse","HEAD"]);
    let merges=(0..merges).map(|i|{git(&["commit","-q","--allow-empty","-m",&format!("merge {i}")]);git(&["rev-parse","HEAD"])}).collect();
    (base,merges)
}

/// Confirmed autonomous merge through the production allowance path.
fn merge(store:&mut WorkflowStore,repository:&str,pr:u64,commit:&str,now:i64)->String {
    let id=format!("{repository}#{pr}");
    let binding=Binding{head:"a".repeat(40),contract:"contract".into(),evidence_hash:"verified-tree".into()};
    store.register(&id,&format!("agent-fix/issue-{pr}"),binding.clone(),now).unwrap();
    let lease=store.claim(&id,"fixture",now,DAY).unwrap().unwrap();
    store.attach_pr(&lease,pr,now).unwrap();
    let op=store.begin_operation(&lease,OperationKind::MergePr,&binding,now).unwrap();
    store.reserve_merge(&lease,&op,repository,pr,&format!("PR_{pr}"),now,5).unwrap();
    store.dispatch_merge(&lease,&op,now,5).unwrap();
    let receipt=serde_json::json!({"client_mutation_id":op.id,"expected_head":op.binding.head,
        "pr_number":pr,"pr_node_id":format!("PR_{pr}"),"merge_commit":commit,
        "merged_by":"owner","merged_at":chrono::DateTime::from_timestamp_millis(now).unwrap().to_rfc3339()});
    store.confirm_merge_dispatch(&op,&receipt,now).unwrap();
    store.release(&lease,now).unwrap();
    id
}
fn last(store:&WorkflowStore,id:&str)->progress::Progress {store.progress_events(id).unwrap().pop().unwrap()}
fn receipt(revision:&str,outcome:DeploymentOutcome,verified:bool)->DeploymentReceipt {
    DeploymentReceipt{repository:"owner/repo".into(),revision:revision.into(),outcome,artifact_verified:verified}
}

#[test]
fn merged_is_not_deployed_until_a_verified_revision_contains_the_merge() {
    let root=tempfile::tempdir().unwrap();let checkout=root.path().join("checkout");std::fs::create_dir(&checkout).unwrap();
    let (base,commits)=repo(&checkout,2);
    let path=root.path().join("state.db");let mut store=WorkflowStore::open(&path).unwrap();
    store.set_deployment_applicability("owner/repo",true).unwrap();
    let now=100*DAY;
    let first=merge(&mut store,"owner/repo",1,&commits[0],now);
    let second=merge(&mut store,"owner/repo",2,&commits[1],now+1);
    for id in [&first,&second] {
        let merged=last(&store,id);
        assert_eq!(merged.kind,"merged");assert_eq!(merged.merge_state,"observed-autonomous-merge");
        assert_eq!(merged.deployment_state,"pending","a deployable merge must report deployment pending, not unknown or deployed");
    }
    let merged_events=store.progress_events(&first).unwrap();

    // A running revision that predates the merge proves nothing about it.
    assert!(store.record_deployment_receipt(&checkout,&receipt(&base,DeploymentOutcome::Deployed,true),now+2).unwrap().is_empty());
    assert_eq!(store.progress_events(&first).unwrap(),merged_events);

    // Updater OOM on the batch revision: merged + deployment failed, never deployed.
    let mut updated=store.record_deployment_receipt(&checkout,&receipt(&commits[1],DeploymentOutcome::Failed,false),now+3).unwrap();
    updated.sort();assert_eq!(updated,vec![first.clone(),second.clone()]);
    for id in [&first,&second] {
        let failed=last(&store,id);
        assert_eq!(failed.kind,"deployment-failed");assert_eq!(failed.deployment_state,"failed");
        assert_eq!(failed.merge_state,"observed-autonomous-merge");assert_eq!(failed.pr.is_some(),true);
    }
    let failed_seq=last(&store,&first).sequence;

    // A success claim without verifying the running artifact is refused.
    assert!(store.record_deployment_receipt(&checkout,&receipt(&commits[1],DeploymentOutcome::Deployed,false),now+4).is_err());
    assert_eq!(last(&store,&first).sequence,failed_seq);

    // A later verified containing revision resolves both merges in one build.
    let mut updated=store.record_deployment_receipt(&checkout,&receipt(&commits[1],DeploymentOutcome::Deployed,true),now+5).unwrap();
    updated.sort();assert_eq!(updated,vec![first.clone(),second.clone()]);
    for id in [&first,&second] {
        let deployed=last(&store,id);
        assert_eq!(deployed.kind,"deployed");assert_eq!(deployed.deployment_state,"deployed");
        assert_eq!(deployed.next_action,"complete");
    }
    assert_eq!(last(&store,&first).sequence,failed_seq+1,"monotonic per-workflow sequence");
    let settled=store.progress_events(&first).unwrap();

    // Replays and stale observations cannot regress a confirmed deployment.
    assert!(store.record_deployment_receipt(&checkout,&receipt(&commits[1],DeploymentOutcome::Deployed,true),now+6).unwrap().is_empty());
    assert!(store.record_deployment_receipt(&checkout,&receipt(&commits[1],DeploymentOutcome::Failed,false),now+7).unwrap().is_empty());
    drop(store);let store=WorkflowStore::open(&path).unwrap();
    assert_eq!(store.progress_events(&first).unwrap(),settled);
}

#[test]
fn partially_contained_batch_only_advances_merges_in_the_running_revision() {
    let root=tempfile::tempdir().unwrap();let checkout=root.path().join("checkout");std::fs::create_dir(&checkout).unwrap();
    let (_,commits)=repo(&checkout,2);
    let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    store.set_deployment_applicability("owner/repo",true).unwrap();
    let first=merge(&mut store,"owner/repo",1,&commits[0],100*DAY);
    let second=merge(&mut store,"owner/repo",2,&commits[1],100*DAY);
    assert_eq!(store.record_deployment_receipt(&checkout,&receipt(&commits[0],DeploymentOutcome::Deployed,true),100*DAY+1).unwrap(),vec![first.clone()]);
    assert_eq!(last(&store,&first).deployment_state,"deployed");
    assert_eq!(last(&store,&second).deployment_state,"pending");
}

#[test]
fn non_deployable_repository_reports_not_applicable_and_never_deployed() {
    let root=tempfile::tempdir().unwrap();let checkout=root.path().join("checkout");std::fs::create_dir(&checkout).unwrap();
    let (_,commits)=repo(&checkout,1);
    let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    store.set_deployment_applicability("owner/repo",false).unwrap();
    let id=merge(&mut store,"owner/repo",1,&commits[0],100*DAY);
    assert_eq!(last(&store,&id).deployment_state,"not-applicable");
    let events=store.progress_events(&id).unwrap();
    assert!(store.record_deployment_receipt(&checkout,&receipt(&commits[0],DeploymentOutcome::Deployed,true),100*DAY+1).is_err(),
        "a deployment receipt for a non-deployable repository is a configuration fault");
    assert_eq!(store.progress_events(&id).unwrap(),events);
}

#[test]
fn unconfigured_applicability_stays_unknown_rather_than_guessing() {
    let root=tempfile::tempdir().unwrap();let checkout=root.path().join("checkout");std::fs::create_dir(&checkout).unwrap();
    let (_,commits)=repo(&checkout,1);
    let mut store=WorkflowStore::open(&root.path().join("state.db")).unwrap();
    let id=merge(&mut store,"owner/repo",1,&commits[0],100*DAY);
    assert_eq!(last(&store,&id).deployment_state,"unknown");
    assert!(store.record_deployment_receipt(&checkout,&receipt(&commits[0],DeploymentOutcome::Deployed,true),100*DAY+1).is_err());
}

#[test]
fn undelivered_progress_degrades_then_dead_letters_without_discarding_events() {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("state.db");
    let mut store=WorkflowStore::open(&path).unwrap();
    let request=submission::Request{repository:"owner/repo".into(),target:submission::Target::Pr(42),provider:None,until:submission::Milestone::Merged};
    let id=store.submit_authorized("cli-request","github:owner",&request,1).unwrap().workflow;
    let created=store.conn.query_row("SELECT min(created_at) FROM workflow_progress_event WHERE workflow=?1",[&id],|r|r.get::<_,i64>(0)).unwrap();
    // Unconfigured destination: retained and visibly unconfigured, never degraded into a guess.
    assert_eq!(store.progress_delivery_at(&id,created+2*DAY).unwrap().state,"delivery-unconfigured");
    assert!(store.bind_delivery(&id,"github:owner",super::super::delivery::Destination{bot:11,owner:22,channel:33},created).unwrap());
    let status=|store:&WorkflowStore,at:i64|store.progress_delivery_at(&id,at).unwrap();
    assert_eq!(status(&store,created+15*MINUTE-1).state,"delivery-pending");
    assert_eq!(status(&store,created+15*MINUTE).state,"delivery-degraded");
    let dead=status(&store,created+DAY);
    assert_eq!(dead.state,"delivery-dead-letter");assert!(dead.pending_events>0,"dead letter keeps the retained events visible");
    drop(store);let mut store=WorkflowStore::open(&path).unwrap();
    let pending=store.pending_progress(11,22).unwrap();
    assert_eq!(pending.len(),1,"a dead-lettered event remains retryable after restart");
    let origin=DiscordOrigin{bot:11,owner:22,channel:33,message:1};
    store.claim_progress(&origin,&id,pending[0].2,created+DAY+1).unwrap().unwrap();
    assert_eq!(store.progress_delivery_at(&id,created+DAY+2).unwrap().state,"delivery-uncertain");
    store.confirm_progress(&origin,&id,pending[0].2,99,created+DAY+3).unwrap();
    assert_eq!(store.progress_delivery_at(&id,created+2*DAY).unwrap().state,"delivery-confirmed");
}
