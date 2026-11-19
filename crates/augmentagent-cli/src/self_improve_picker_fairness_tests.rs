//! Synthetic end-to-end picker fixtures: no providers or production state.
use super::*;
use crate::autoship_workflow::{Binding, NextAction, RecoveryContext, Stage, WorkflowStore};

fn git(root:&Path,args:&[&str])->String {
    let out=std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().into()
}

async fn fixture(case:&str,test:&str) {
    const CHILD:&str="AUTOSHIP_PICKER_FAIRNESS_ROOT";
    if std::env::var_os(CHILD).is_none() {
        let tmp=tempfile::tempdir().unwrap();let repo=tmp.path().join("repo");std::fs::create_dir(&repo).unwrap();
        git(&repo,&["init","-q","-b","main"]);
        git(&repo,&["remote","add","origin","https://github.com/fixture-owner/repo.git"]);
        std::fs::write(repo.join("fixture.txt"),"synthetic input\n").unwrap();
        git(&repo,&["add","."]);git(&repo,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","fixture"]);
        let head=git(&repo,&["rev-parse","HEAD"]);
        let gh=tmp.path().join("gh.py");
        augmentagent_test_support::write_executable(&gh,r#"#!/usr/bin/python3
import json,os,sys
args=sys.argv[1:]
if args[:2]==['repo','view']:print('fixture-owner/repo')
elif args[:2]==['pr','list']:print('[]')
elif args[:1]==['api'] and '/git/ref/heads/main' in args[1]:print(os.environ['AUTOSHIP_FAIRNESS_HEAD'])
elif args[:1]==['api'] and '/issues?' in args[1]:
 def issue(n):return dict(number=n,title='Correct widget rounding',body=('Original synthetic requirement.' if os.environ['AUTOSHIP_CHECKOUT_CASE']=='policy-upgrade' else 'Updated synthetic requirement.') if n==2 else 'Preserve synthetic rounding.',labels=[],user={'login':'fixture-owner'},author_association='OWNER')
 print(json.dumps([issue(3),issue(2)] if args[1].endswith('page=1') else []))
else:raise AssertionError(args)
"#).unwrap();
        let out=tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact",test,"--nocapture"]).env_clear()
            .env("PATH","/usr/bin:/bin").env("HOME",tmp.path()).env("XDG_STATE_HOME",tmp.path().join("state"))
            .env("GH_BIN",gh).env("AUGMENTAGENT_GH_OWNER","fixture-owner")
            .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","fixture-owner")
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",tmp.path().join("workflow.db"))
            .env("AUTOSHIP_CHECKOUT_CASE",case).env("AUTOSHIP_FAIRNESS_HEAD",head).env(CHILD,&repo).current_dir(&repo).output().await.unwrap();
        assert!(out.status.success(),"{case}: {}\n{}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));
        return;
    }
    let repo=PathBuf::from(std::env::var_os(CHILD).unwrap());let now=chrono::Utc::now().timestamp_millis();
    let mut store=WorkflowStore::open(&workflow::path()).unwrap();
    let binding=Binding {head:std::env::var("AUTOSHIP_FAIRNESS_HEAD").unwrap(),contract:"synthetic-contract".into(),evidence_hash:"synthetic-environment".into()};
    let peer="fixture-owner/repo#2";let retry="fixture-owner/repo#3";
    for (number,id) in [(2,peer),(3,retry)] {
        store.register(id,&format!("agent-fix/issue-{number}"),binding.clone(),now-20_000).unwrap();
        let attempts=if number==3 && case=="rotation" {3}else if number==3 && case=="initial-order" {2}else{1};
        for round in 0..attempts {
            let at=now-10_000+round*10+number;
            let lease=store.claim(id,"fixture-worker",at,1_000).unwrap().unwrap();
            let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;cp.stage=Stage::Scope;
            cp.next=NextAction {reason:if number==2 && case=="policy-upgrade" {"material_external_decision_missing"}else if number==2 {"readiness_assessment_invalid"}else{"acceptance_registration_retry"}.into(),action:if number==2 {"await_readiness_input_change"}else{"rescope_executable_acceptance"}.into(),eligible_at_ms:if number==3 && case=="future" {now+300_000}else{0}};
            if number==2 {
                let request=serde_json::to_string(&(2,"Correct widget rounding","Original synthetic requirement.")).unwrap();
                store.record_recovery_context(&lease,&binding,&RecoveryContext {version:1,workflow:id.into(),repository:"fixture-owner/repo".into(),worktree:repo.clone(),source_request:request.clone(),source_pr:None,contract:request.clone(),environment:if case=="policy-upgrade" {"synthetic-environment;readiness-policy:readiness-grounding-v4"}else{"synthetic-environment"}.into(),binding:binding.clone()},at+1).unwrap();
            }
            if number==2 && case=="policy-upgrade" {
                let request=serde_json::to_string(&(2,"Correct widget rounding","Original synthetic requirement.")).unwrap();
                store.set_readiness_dependency(&lease,&binding,"fixture-owner/repo",2,Some((&amendment::digest(request.as_bytes()),&binding.head,"external_decision","synthetic preserved assessment")),at+1).unwrap();
            }
            store.checkpoint(&lease,&cp,at+2).unwrap();store.release(&lease,at+3).unwrap();
        }
    }
    if matches!(case,"lost-checkout"|"replaced-checkout") {
        git(&repo,&["update-ref","refs/remotes/origin/main",&binding.head]);
        let candidate=workspace::candidate_parent(&repo).await.unwrap().join("current-issue-3");
        let branch="agent-fix/issue-3";
        recovery::prepare(&repo,&candidate,branch).await.unwrap();
        let lease=store.claim(retry,"fixture-enrollment",now-1000,500).unwrap().unwrap();
        let identity=checkout_identity::Identity::observe(&workflow::path(),&repo,&candidate,retry,"fixture-owner/repo",branch).await.unwrap();
        identity.enroll(&mut store,&lease,&binding,now-999).unwrap();
        store.record_recovery_context(&lease,&binding,&RecoveryContext {version:1,workflow:retry.into(),repository:"fixture-owner/repo".into(),worktree:candidate.clone(),source_request:"synthetic-request".into(),source_pr:None,contract:"synthetic-contract".into(),environment:"synthetic-environment".into(),binding:binding.clone()},now-998).unwrap();
        store.release(&lease,now-997).unwrap();
        if case=="lost-checkout" {
            git(&repo,&["worktree","remove","--force",candidate.to_str().unwrap()]);
        } else {
            let original=candidate.with_file_name("preserved-original");
            git(&repo,&["worktree","move",candidate.to_str().unwrap(),original.to_str().unwrap()]);
            git(&repo,&["worktree","add","--force",candidate.to_str().unwrap(),branch]);
        }
    }
    if case=="leased" {store.claim(retry,"still-owned",now,600_000).unwrap().unwrap();}
    let before=[store.snapshot(peer).unwrap(),store.snapshot(retry).unwrap()];
    let events=[store.events(peer).unwrap(),store.events(retry).unwrap()];
    let picked=pick_issue(&repo,true).await.unwrap().unwrap();
    assert_eq!(picked.number,if case=="initial-order" {3}else{2},"{case}: picker did not preserve due-work fairness and initial newest-first order");
    assert_eq!([store.snapshot(peer).unwrap(),store.snapshot(retry).unwrap()],before,"preview changed protected workflow state");
    assert_eq!([store.events(peer).unwrap(),store.events(retry).unwrap()],events,"preview wrote events or accounting");
    if case=="rotation" {
        // Simulate real productive claims between otherwise read-only picker
        // calls. Both remain in scope, so the most recent retry must yield.
        for round in 0..6 {
            let number=if round%2==0 {2}else{3};let id=if number==2 {peer}else{retry};
            // The peer starts with one attempt: bring it to the same bounded
            // retry tier, then keep using actual durable claim timestamps.
            let count=if round==0 {2}else{1};
            for inner in 0..count {
                let at=now+round*10+inner;
                let lease=store.claim(id,"fixture-turn",at,1_000).unwrap().unwrap();
                let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;
                cp.next=NextAction {reason:"acceptance_registration_retry".into(),action:"rescope_executable_acceptance".into(),eligible_at_ms:0};
                store.checkpoint(&lease,&cp,at).unwrap();store.release(&lease,at).unwrap();
            }
            let next=pick_issue(&repo,true).await.unwrap().unwrap();
            assert_eq!(next.number,if number==2 {3}else{2},"repeated scope turns failed to rotate");
        }
    }
}

#[tokio::test]
async fn future_retry_yields_to_changed_hold_peer() {
    fixture("future","self_improve::picker_fairness_tests::future_retry_yields_to_changed_hold_peer").await;
}
#[tokio::test]
async fn retained_lease_yields_to_changed_hold_peer() {
    fixture("leased","self_improve::picker_fairness_tests::retained_lease_yields_to_changed_hold_peer").await;
}
#[tokio::test]
async fn repeated_scope_attempts_rotate_between_eligible_issues() {
    fixture("rotation","self_improve::picker_fairness_tests::repeated_scope_attempts_rotate_between_eligible_issues").await;
}

#[tokio::test]
async fn first_scope_attempts_preserve_existing_newest_first_order() {
    fixture("initial-order","self_improve::picker_fairness_tests::first_scope_attempts_preserve_existing_newest_first_order").await;
}

#[tokio::test]
async fn invalid_checkout_yields_to_eligible_peer_without_claiming() {
    const TEST:&str="self_improve::picker_fairness_tests::invalid_checkout_yields_to_eligible_peer_without_claiming";
    if std::env::var_os("AUTOSHIP_PICKER_FAIRNESS_ROOT").is_some() {
        fixture(&std::env::var("AUTOSHIP_CHECKOUT_CASE").unwrap(),TEST).await;
    } else {
        for case in ["lost-checkout","replaced-checkout"] { fixture(case,TEST).await; }
    }
}

#[tokio::test]
async fn safe_scope_policy_selects_legacy_dependency_without_owner_or_main_edits() {
    fixture("policy-upgrade","self_improve::picker_fairness_tests::safe_scope_policy_selects_legacy_dependency_without_owner_or_main_edits").await;
}
