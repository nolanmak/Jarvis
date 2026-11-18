//! #2179 acceptance: real selectors (primary REST and configured-repository
//! inventory), disposable state, a scripted GitHub boundary, no model calls.
use super::*;
use std::os::unix::fs::PermissionsExt;

const CHILD:&str="AUTOSHIP_EPIC_SELECTION_FIXTURE";

/// Runs `test` in a child process per route; returns each route's stderr.
async fn exercise(case:&str,test:&str)->Vec<(String,String)> {
    let mut logs=Vec::new();
    for route in ["primary","configured"] {
        let root=tempfile::tempdir().unwrap();
        let root_path=root.path().canonicalize().unwrap();
        let gh=root_path.join("github.py");
        augmentagent_test_support::write_executable(&gh,r#"#!/usr/bin/python3
import json, os, sys, subprocess
args = sys.argv[1:]
with open(os.environ['AUTOSHIP_EPIC_CALLS'], 'a') as out:
    out.write(json.dumps(args) + '\n')
inv = os.environ['AUTOSHIP_EPIC_INVENTORY']
if args[:2] == ['repo', 'view']:
    print('Fixture-Owner/Repo'); sys.exit(0)
if args[:1] == ['api'] and args[1].endswith('/git/ref/heads/main'):
    print(subprocess.check_output(['/usr/bin/git', 'rev-parse', 'HEAD'], text=True).strip()); sys.exit(0)
if args[:2] == ['pr', 'list']:
    print('[]'); sys.exit(0)
if args[:2] == ['issue', 'list'] or (args[:1] == ['api'] and '/issues?' in args[1]):
    rows = json.load(open(inv))
    if args[:1] == ['api'] and not args[1].endswith('page=1'):
        rows = []
    print(json.dumps(rows)); sys.exit(0)
if args[:2] == ['label', 'create']:
    sys.exit(0)
if args[:2] == ['issue', 'edit'] and len(args) == 5 and args[3] in ('--add-label', '--remove-label'):
    rows = json.load(open(inv))
    for row in rows:
        if str(row['number']) == args[2]:
            names = [l['name'] for l in row['labels'] if l['name'] != args[4]]
            if args[3] == '--add-label':
                names.append(args[4])
            row['labels'] = [{'name': n} for n in names]
    json.dump(rows, open(inv, 'w')); sys.exit(0)
if args[:2] == ['issue', 'comment'] and len(args) == 5 and args[3] == '--body':
    sys.exit(0)
raise AssertionError('Unexpected GitHub call: ' + repr(args))
"#).unwrap();
        std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
        let repo=root_path.join("repo");std::fs::create_dir(&repo).unwrap();
        std::fs::write(repo.join("code.txt"),"original\n").unwrap();
        for args in [vec!["init","-q"],vec!["remote","add","origin","https://github.com/fixture-owner/repo.git"],
            vec!["add","code.txt"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid",
                "-c","core.hooksPath=/dev/null","-c","commit.gpgsign=false","commit","-qm","original"],
            vec!["update-ref","refs/remotes/origin/main","HEAD"]] {
            assert!(std::process::Command::new("/usr/bin/git").args(args).current_dir(&repo)
                .env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
        }
        let out=tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact",&format!("self_improve::epic_selection_tests::{test}"),"--nocapture"])
            .env_clear().env("PATH","/usr/bin:/bin").env("HOME",&root_path)
            .env("XDG_STATE_HOME",root_path.join("state"))
            .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",root_path.join("workflow.db"))
            .env("GH_BIN",&gh).env("AUGMENTAGENT_GH_OWNER","fixture-owner")
            .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","fixture-owner")
            .env("AUTOSHIP_EPIC_CASE",case).env("AUTOSHIP_EPIC_ROUTE",route)
            .env("AUTOSHIP_EPIC_CALLS",root_path.join("calls")).env("AUTOSHIP_EPIC_INVENTORY",root_path.join("inventory.json"))
            .env("CODEX_CLI","/bin/false").env("CLAUDE_CLI","/bin/false")
            .env(CHILD,&repo).current_dir(&repo).output().await.unwrap();
        let stderr=String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(),"{case}/{route}: {}\n{stderr}",String::from_utf8_lossy(&out.stdout));
        logs.push((route.to_string(),stderr));
    }
    logs
}

fn route()->String {std::env::var("AUTOSHIP_EPIC_ROUTE").unwrap()}
async fn select(repo:&Path)->Option<u64> {
    let picked=if route()=="primary" {pick_issue(repo,false).await} else {
        let product=Store::open(repo.parent().unwrap().join("product.db")).unwrap();
        let configured=AgentRepo {id:"fixture".into(),full_name:"Fixture-Owner/Repo".into(),base_branch:"main".into(),
            build_cmd:String::new(),blast_radius_extra:String::new(),max_diff_lines:600,enabled:true,created_at_ms:0,updated_at_ms:0};
        pick_issue_for_repo(&configured,repo,&product).await
    };
    picked.unwrap().map(|issue|issue.number)
}
/// Primary lists newest-first; the configured inventory is worked oldest-first.
/// Returns numbers in the order each selector considers them.
fn considered(count:u64)->Vec<u64> {
    let mut numbers:Vec<u64>=(40..40+count).collect();
    if route()=="primary" {numbers.reverse();}
    numbers
}
/// The configured route only sees `agent-fixable` work.
fn labels(extra:&[&str])->serde_json::Value {
    let mut names:Vec<&str>=extra.to_vec();
    if route()=="configured" || extra.is_empty() {names.push("agent-fixable");}
    serde_json::Value::Array(names.into_iter().map(|n|serde_json::json!({"name":n})).collect())
}
fn write(rows:&[(u64,String,String,serde_json::Value)]) {
    // Listed newest-first, as both GitHub listings return them.
    let mut rows=rows.to_vec();rows.sort_by(|a,b|b.0.cmp(&a.0));
    let json:Vec<_>=rows.into_iter().map(|(number,title,body,labels)|serde_json::json!({"number":number,"title":title,"body":body,
        "labels":labels,"user":{"login":"fixture-owner"},"author":{"login":"fixture-owner"},"author_association":"OWNER"})).collect();
    std::fs::write(std::env::var_os("AUTOSHIP_EPIC_INVENTORY").unwrap(),serde_json::to_vec(&json).unwrap()).unwrap();
}
fn calls()->Vec<Vec<String>> {
    std::fs::read_to_string(std::env::var_os("AUTOSHIP_EPIC_CALLS").unwrap()).unwrap_or_default().lines()
        .map(|l|serde_json::from_str(l).unwrap()).collect()
}
fn mutations()->Vec<Vec<String>> {
    calls().into_iter().filter(|c|matches!((c[0].as_str(),c.get(1).map(String::as_str)),("issue",Some("edit"|"comment"))|("label",Some("create")))).collect()
}
fn init_logs() {
    let _=tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false).with_max_level(tracing::Level::INFO).try_init();
}

async fn child() {
    init_logs();
    let repo=PathBuf::from(std::env::var_os(CHILD).unwrap());
    match std::env::var("AUTOSHIP_EPIC_CASE").unwrap().as_str() {
        "label"=>{
            let order=considered(2);let (epic,child)=(order[0],order[1]);
            write(&[(epic,"Ship the widget platform".into(),"Tracking work.".into(),labels(&["epic"])),
                (child,"Correct widget rounding".into(),format!("Part of #{epic}\n\nRound widget values consistently."),labels(&[]))]);
            assert_eq!(select(&repo).await,Some(child),"the epic itself was selected or its child was not");
            assert!(mutations().is_empty(),"an epic with an open child was flagged: {:?}",mutations());
        }
        "titles"=>{
            let order=considered(4);
            write(&[(order[0],"[EPIC] widget platform".into(),"Tracking.".into(),labels(&[])),
                (order[1],"EPIC: widget platform".into(),"Tracking.".into(),labels(&[])),
                (order[2],"Epic: widget platform".into(),"Tracking.".into(),labels(&[])),
                (order[3],"Fix the epic rounding bug".into(),"Round widget values consistently.".into(),labels(&[]))]);
            assert_eq!(select(&repo).await,Some(order[3]),"title-prefixed epics must be skipped; a mid-sentence 'epic' must not");
        }
        "childless"=>{
            let order=considered(2);let (epic,peer)=(order[0],order[1]);
            write(&[(epic,"Ship the widget platform".into(),"Tracking work; Part of #4200.".into(),labels(&["epic"])),
                (peer,"Correct widget rounding".into(),"Round widget values consistently.".into(),labels(&[]))]);
            for tick in 1..=3 {assert_eq!(select(&repo).await,Some(peer),"tick {tick}");}
            let all=mutations();
            let added:Vec<_>=all.iter().filter(|c|c[1]=="edit" && c[3]=="--add-label").collect();
            let comments:Vec<_>=all.iter().filter(|c|c[1]=="comment").collect();
            assert_eq!(added.len(),1,"exactly one label across three ticks: {all:?}");
            assert_eq!(added[0][2..],[epic.to_string(),"--add-label".into(),"needs-breakdown".into()]);
            assert_eq!(comments.len(),1,"exactly one comment across three ticks: {all:?}");
            assert_eq!(comments[0][2],epic.to_string());
            assert_eq!(comments[0][4],format!("This epic has no open child issues, so autoship can't work it. Break it into scoped child issues (`Part of #{epic}`) with testable acceptance criteria."));
            assert!(all.iter().all(|c|c[1]=="create" || c[2]==epic.to_string()),"only the epic is touched: {all:?}");
        }
        other=>panic!("unknown case {other}"),
    }
}

fn assert_skip_logged(logs:&[(String,String)],epics:&[&[u64]]) {
    for ((route,stderr),numbers) in logs.iter().zip(epics) {
        for number in *numbers {
            assert!(stderr.lines().any(|l|l.contains("skip: epic; autoship works its children") && l.contains(&format!("issue={number}"))),
                "{route}: skip reason not logged for #{number}\n{stderr}");
        }
    }
}

#[tokio::test]
async fn labelled_epic_is_never_selected_and_its_child_is() {
    if std::env::var_os(CHILD).is_some() {child().await;return;}
    let logs=exercise("label","labelled_epic_is_never_selected_and_its_child_is").await;
    assert_skip_logged(&logs,&[&[41],&[40]]);
}
#[tokio::test]
async fn epic_title_prefixes_are_skipped_but_mid_sentence_epic_is_not() {
    if std::env::var_os(CHILD).is_some() {child().await;return;}
    let logs=exercise("titles","epic_title_prefixes_are_skipped_but_mid_sentence_epic_is_not").await;
    assert_skip_logged(&logs,&[&[43,42,41],&[40,41,42]]);
}
#[tokio::test]
async fn childless_epic_is_flagged_once_across_three_ticks() {
    if std::env::var_os(CHILD).is_some() {child().await;return;}
    exercise("childless","childless_epic_is_flagged_once_across_three_ticks").await;
}
