//! Real selectors, disposable owner controls, and no model or live GitHub work.
use super::*;
use crate::autoship_workflow::{workflow_id, ResourceCharge, WorkflowStore};
use std::os::unix::fs::PermissionsExt;

const CHILD:&str="AUTOSHIP_PAUSED_SELECTION_FIXTURE";
const REPOSITORY:&str="fixture-owner/repo";

async fn exercise(case:&str,test:&str) {
    if std::env::var_os(CHILD).is_some() {child(case).await;return;}
    let routes:&[&str]=if case=="race" || case.starts_with("trust-") {&["primary"]}else{&["primary","configured"]};
    for &route in routes {
        let root=tempfile::tempdir_in("/tmp").unwrap();
        let gh=root.path().join("github.py");
        std::fs::write(&gh,r#"#!/usr/bin/python3
import json, os, sys, subprocess
args = sys.argv[1:]
with open(os.environ['AUTOSHIP_PAUSED_CALLS'], 'a') as out:
    out.write(json.dumps(args) + '\n')
if args[:2] == ['repo', 'view']:
    if os.environ.get('AUTOSHIP_PAUSED_CASE') == 'trust-identity':
        print('identity unavailable', file=sys.stderr); sys.exit(1)
    print('Fixture-Owner/Repo'); sys.exit(0)
if args[:1] == ['api'] and args[1].endswith('/git/ref/heads/main'):
    print(subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip()); sys.exit(0)
if args[:2] == ['pr', 'list']:
    print('[]'); sys.exit(0)
if args[:2] == ['issue', 'list'] or (args[:1] == ['api'] and '/issues?' in args[1]):
    rows = json.load(open(os.environ['AUTOSHIP_PAUSED_INVENTORY']))
    if isinstance(rows, dict):
        rows = rows.get(args[1].rsplit('page=', 1)[-1], [])
    elif args[:1] == ['api'] and not args[1].endswith('page=1'):
        rows = []
    print(json.dumps(rows)); sys.exit(0)
raise AssertionError('Unexpected or mutating GitHub call: ' + repr(args))
"#).unwrap();
        std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
        let repo=root.path().join("repo");std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("code.txt"),"original candidate\n").unwrap();
        for args in [vec!["init","-q"],vec!["remote","add","origin","https://github.com/fixture-owner/repo.git"],
            vec!["add","code.txt"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid",
                "-c","core.hooksPath=/dev/null","-c","commit.gpgsign=false","commit","-qm","original"],
            vec!["update-ref","refs/remotes/origin/main","HEAD"]] {
            assert!(std::process::Command::new("/usr/bin/git").args(args).current_dir(&repo)
                .env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
        }
        let out=tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact",&format!("self_improve::paused_selection_tests::{test}"),"--nocapture"])
            .env_clear().env("PATH","/usr/bin:/bin").env("HOME",root.path())
            .env("XDG_STATE_HOME",root.path().join("state"))
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",root.path().join("workflow.db"))
            .env("GH_BIN",gh).env("AUGMENTAGENT_GH_OWNER","fixture-owner")
            .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","fixture-owner,explicit-author")
            .env("AUTOSHIP_PAUSED_CASE",case).env("AUTOSHIP_PAUSED_ROUTE",route).env("AUTOSHIP_PAUSED_CALLS",root.path().join("calls"))
            .env("AUTOSHIP_PAUSED_INVENTORY",root.path().join("inventory.json"))
            .env("CODEX_CLI","/bin/false").env("CLAUDE_CLI","/bin/false")
            .env(CHILD,&repo).current_dir(&repo).output().await.unwrap();
        assert!(out.status.success(),"{case}/{route}: {}\n{}",String::from_utf8_lossy(&out.stdout),String::from_utf8_lossy(&out.stderr));
    }
}

async fn select(repo:&Path)->Result<Option<Issue>> {
    if std::env::var("AUTOSHIP_PAUSED_ROUTE").unwrap()=="primary" {return pick_issue(repo,false).await;}
    let product=Store::open(repo.parent().unwrap().join("product.db")).unwrap();
    let configured=AgentRepo {id:"fixture".into(),full_name:"Fixture-Owner/Repo".into(),base_branch:"main".into(),
        build_cmd:String::new(),blast_radius_extra:String::new(),max_diff_lines:600,enabled:true,created_at_ms:0,updated_at_ms:0};
    pick_issue_for_repo(&configured,repo,&product).await
}

fn inventory(numbers:&[u64]) {
    let rows=numbers.iter().map(|number|serde_json::json!({"number":number,"title":"Correct widget rounding",
        "body":"Round widget values consistently.","labels":[{"name":"agent-fixable"}],
        "user":{"login":"fixture-owner"},"author":{"login":"fixture-owner"},"author_association":"OWNER"})).collect::<Vec<_>>();
    std::fs::write(std::env::var_os("AUTOSHIP_PAUSED_INVENTORY").unwrap(),serde_json::to_vec(&rows).unwrap()).unwrap();
}

fn ordered_numbers()->[u64;3] {
    // Preserve primary newest-first and configured-repository oldest-first.
    if std::env::var("AUTOSHIP_PAUSED_ROUTE").unwrap()=="primary" {[42,41,40]}else{[40,41,42]}
}

async fn child(case:&str) {
    let repo=PathBuf::from(std::env::var_os(CHILD).unwrap());
    let path=workflow::path();
    if case.starts_with("trust-") {trust_child(&repo,&path,case).await;return;}
    let [first,second,peer_number]=ordered_numbers();
    if case=="all-paused" {inventory(&[first,second]);}else{inventory(&[42,41,40]);}
    if case=="missing" {
        assert!(!path.exists());
        assert_eq!(select(&repo).await.unwrap().unwrap().number,first);
        assert!(!path.exists(),"selection implicitly registered or initialized workflow state");
        return;
    }
    if case=="unreadable" {
        std::fs::write(&path,b"not a SQLite database").unwrap();
        std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o600)).unwrap();
        let error=select(&repo).await.err().expect("unknown owner controls authorized fresh work");
        assert_eq!(error.to_string(),"owner-control-unavailable");
        assert!(error.downcast_ref::<workflow::OwnerControlUnavailable>().is_some(),"control uncertainty lost its typed hold");
        return;
    }
    let binding=workflow::git_binding(&repo,"original request","fixture-environment",&[]).await.unwrap();
    let mut store=WorkflowStore::open(&path).unwrap();
    for number in [first,second] {
        let id=workflow_id(REPOSITORY,number).unwrap();
        store.register(&id,"fixture",binding.clone(),1).unwrap();
        let lease=store.claim(&id,"past-owner",2,100).unwrap().unwrap();
        store.charge(&lease,"past-work",ResourceCharge {wall_ms:700,model_calls:9,tokens:1234},3).unwrap();
        store.release(&lease,4).unwrap();
        // Persisted controls are deliberately old: neither today's UTC date
        // nor a reopened reader may expire a genuine owner pause.
        store.set_owner_pause(&id,"github:fixture-owner",0,true,5).unwrap();
    }
    let before=[first,second].map(|number| {
        let id=workflow_id(REPOSITORY,number).unwrap();
        (store.snapshot(&id).unwrap().unwrap(),store.owner_control(&id).unwrap().unwrap(),store.events(&id).unwrap())
    });
    drop(store);
    let selected=select(&repo).await.unwrap();
    assert_eq!(selected.as_ref().map(|issue|issue.number),if case=="all-paused" {None}else{Some(peer_number)},
        "paused first issue starved its ordinary peer");
    let mut store=WorkflowStore::open(&path).unwrap();
    for (number,(snapshot,control,events)) in [first,second].into_iter().zip(&before) {
        let id=workflow_id(REPOSITORY,number).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().as_ref(),Some(snapshot));
        assert_eq!(store.owner_control(&id).unwrap().as_ref(),Some(control));
        assert_eq!(&store.events(&id).unwrap(),events,"selection mutated paused history");
        assert_eq!(snapshot.model_calls,9);
    }
    let peer=workflow_id(REPOSITORY,peer_number).unwrap();
    assert!(store.snapshot(&peer).unwrap().is_none(),"free selection implicitly registered its peer");
    if case=="resume" {
        store.set_owner_pause(&workflow_id(REPOSITORY,first).unwrap(),"github:fixture-owner",1,false,10).unwrap();
        drop(store);
        assert_eq!(select(&repo).await.unwrap().unwrap().number,first,"supported owner resume did not restore eligibility");
    }else if case=="race" {
        store.register(&peer,"fixture",binding,10).unwrap();
        store.set_owner_pause(&peer,"github:fixture-owner",0,true,11).unwrap();
        let prior=store.snapshot(&peer).unwrap().unwrap();drop(store);
        let acquired=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {repo_root:&repo,worktree:&repo,
            repository:REPOSITORY,source_issue:Some(peer_number),pr:None,branch:"fixture",contract:"original request",environment:"fixture-environment"}).await.unwrap();
        assert!(acquired.is_none(),"pause arriving after selection reached productive claim");
        let store=WorkflowStore::open(&path).unwrap();
        let after=store.snapshot(&peer).unwrap().unwrap();
        assert_eq!(after.model_calls,prior.model_calls);assert_eq!(after.generation,prior.generation);
        assert!(store.owner_control(&peer).unwrap().unwrap().paused);
    }else {drop(store);}
    assert_eq!(std::fs::read_to_string(repo.join("code.txt")).unwrap(),"original candidate\n");
    assert!(!repo.join(".self-improve-worktrees").exists(),"selector created a per-issue checkout");
}

#[tokio::test]
async fn paused_issues_do_not_starve_ordinary_peer() {exercise("skip","paused_issues_do_not_starve_ordinary_peer").await;}
#[tokio::test]
async fn all_paused_issues_are_idle_without_mutating_their_history() {exercise("all-paused","all_paused_issues_are_idle_without_mutating_their_history").await;}
#[tokio::test]
async fn supported_owner_resume_restores_fresh_eligibility() {exercise("resume","supported_owner_resume_restores_fresh_eligibility").await;}
#[tokio::test]
async fn primary_pause_after_selection_still_fences_actual_workflow_claim() {exercise("race","primary_pause_after_selection_still_fences_actual_workflow_claim").await;}
#[tokio::test]
async fn absent_control_state_allows_new_issue_without_initializing_state() {exercise("missing","absent_control_state_allows_new_issue_without_initializing_state").await;}
#[tokio::test]
async fn unreadable_control_state_is_a_free_typed_hold() {exercise("unreadable","unreadable_control_state_is_a_free_typed_hold").await;}


async fn trust_child(repo:&Path,path:&Path,case:&str) {
    let row=|number:u64,login:&str,association:&str|serde_json::json!({"number":number,"title":"Correct widget rounding",
        "body":"Round widget values consistently.","labels":[],"user":{"login":login},"author_association":association});
    let binding=workflow::git_binding(repo,"original request","fixture-environment",&[]).await.unwrap();
    let mut store=WorkflowStore::open(path).unwrap();
    for number in [42,41] {
        let id=workflow_id(REPOSITORY,number).unwrap();
        store.register(&id,"fixture",binding.clone(),1).unwrap();
        let lease=store.claim(&id,"past-owner",2,100).unwrap().unwrap();
        store.charge(&lease,"past-work",ResourceCharge {wall_ms:700,model_calls:9,tokens:1234},3).unwrap();
        store.release(&lease,4).unwrap();
    }
    if case=="trust-paused" {store.set_owner_pause(&workflow_id(REPOSITORY,42).unwrap(),"github:fixture-owner",0,true,5).unwrap();}
    let before=[42,41].map(|n| {
        let id=workflow_id(REPOSITORY,n).unwrap();
        (store.snapshot(&id).unwrap(),store.owner_control(&id).unwrap(),store.events(&id).unwrap())
    });
    drop(store);
    for (login,association) in [("another-owner","owner"),("member-login","MEMBER"),("collaborator-login","collaborator"),("FiXtUrE-OwNeR","NONE"),("ExPlIcIt-AuThOr","NONE")] {
        let mut a=if case=="trust-paused" {row(42,"fixture-owner","OWNER")}else{row(42,"github-actions[bot]","NONE")};
        if case=="trust-peers" {a["body"]="Background mentions .env but has no owner authorization".into();}
        let mut rows=vec![a,row(41,"outside-author","NONE")];
        if case!="trust-all" {rows.push(row(40,login,association));}
        let inventory=if case=="trust-pages" {
            serde_json::json!({"1":(901..1001).rev().map(|n|row(n,"outside-author","NONE")).collect::<Vec<_>>(),"2":[row(40,login,association)]})
        }else {serde_json::Value::Array(rows)};
        std::fs::write(std::env::var_os("AUTOSHIP_PAUSED_INVENTORY").unwrap(),serde_json::to_vec(&inventory).unwrap()).unwrap();
        let result=select(repo).await;
        if case=="trust-identity" {
            let error=result.err().expect("unavailable identity authorized selection");
            assert!(error.downcast_ref::<workflow::OwnerControlUnavailable>().is_some());
        }else {
            let selected=result.unwrap();
            assert_eq!(selected.as_ref().map(|issue|issue.number),if case=="trust-all" {None}else{Some(40)},
                "untrusted source consumed the eligible peer's selection turn");
            if let Some(issue)=selected {assert!(issue.author_trusted,"eligible peer lost its existing authorization");}
        }
        let store=WorkflowStore::open(path).unwrap();
        for (number,(snapshot,control,events)) in [42,41].into_iter().zip(&before) {
            let id=workflow_id(REPOSITORY,number).unwrap();
            assert_eq!(&store.snapshot(&id).unwrap(),snapshot);assert_eq!(&store.owner_control(&id).unwrap(),control);
            assert_eq!(&store.events(&id).unwrap(),events,"selection changed refused-source accounting or history");
        }
        assert!(store.snapshot(&workflow_id(REPOSITORY,40).unwrap()).unwrap().is_none());
    }
    let calls=std::fs::read_to_string(std::env::var_os("AUTOSHIP_PAUSED_CALLS").unwrap()).unwrap();
    for call in calls.lines() {
        let args:Vec<String>=serde_json::from_str(call).unwrap();
        if matches!(args.first().map(String::as_str),Some("issue"|"pr")) {
            assert_eq!(args.get(1).map(String::as_str),Some("list"),"selection attempted a public triage mutation");
        }
    }
    assert!(!repo.join(".self-improve-worktrees").exists());
    assert_eq!(std::fs::read_to_string(repo.join("code.txt")).unwrap(),"original candidate\n");
}

#[tokio::test]
async fn automatic_selector_skips_untrusted_sources_for_authorized_peer() {exercise("trust-peers","automatic_selector_skips_untrusted_sources_for_authorized_peer").await;}
#[tokio::test]
async fn automatic_selector_leaves_all_untrusted_inventory_idle() {exercise("trust-all","automatic_selector_leaves_all_untrusted_inventory_idle").await;}
#[tokio::test]
async fn automatic_selector_retains_owner_pauses_while_filtering_trust() {exercise("trust-paused","automatic_selector_retains_owner_pauses_while_filtering_trust").await;}
#[tokio::test]
async fn automatic_selector_completes_inventory_before_trusted_selection() {exercise("trust-pages","automatic_selector_completes_inventory_before_trusted_selection").await;}
#[tokio::test]
async fn automatic_selector_requires_canonical_identity_for_trusted_selection() {exercise("trust-identity","automatic_selector_requires_canonical_identity_for_trusted_selection").await;}
