//! Production selectors with disposable GitHub/state; no native model calls.
use super::*;
use crate::autoship_workflow::WorkflowStore;
use std::os::unix::fs::PermissionsExt;

const CHILD:&str="AUTOSHIP_MANUAL_PR_FIXTURE";

#[tokio::test]
async fn automatic_pr_routes_skip_manual_work_before_ownership_and_advance_peer() {
    if let Some(root)=std::env::var_os(CHILD) {
        let root=PathBuf::from(root);
        let case=std::env::var("MANUAL_PR_CASE").unwrap();
        let path=workflow::path();let mut initial=WorkflowStore::open(&path).unwrap();
        if case=="late-mutation" {
            let source=(std::env::var("MANUAL_PR_ROUTE").unwrap()=="linked").then_some(7);
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:source,pr:Some(9),
                branch:"feat/repair-9",contract:"Retained ordinary request",environment:"fixture",
            }).await.unwrap().unwrap();
            assert!(manual_routing::permit_mutation(&root,9,source,true,&mut guard).await.unwrap());
            let before=initial.snapshot(guard.id()).unwrap().unwrap();
            std::fs::write(root.join("manual-after-admission"),b"late owner routing").unwrap();
            assert!(!manual_routing::permit_mutation(&root,9,source,true,&mut guard).await.unwrap(),
                "an automatic publication boundary ignored a manual label applied after admission");
            let held=initial.snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(held.model_calls,before.model_calls);assert_eq!(held.tokens,before.tokens);
            assert_eq!(held.checkpoint.binding,before.checkpoint.binding,"routing observation changed candidate evidence");
            assert_eq!(held.checkpoint.next.reason,"automatic_pr_routing_held");
            assert!(initial.pending_operations(guard.id()).unwrap().is_empty());
            assert!(manual_routing::permit_mutation(&root,9,source,false,&mut guard).await.unwrap(),
                "automatic routing revoked explicit dispatch authorization");
            assert_eq!(initial.snapshot(guard.id()).unwrap().unwrap(),held,
                "explicit routing observation cleared a durable hold or changed charges");
            std::fs::write(root.join("actual-head"),&guard.binding().head).unwrap();
            manual_routing::restore_draft(&root,9,false,&guard).await;
            assert!(!root.join("draft-restored").exists(),"undo touched a PR this turn did not ready");
            manual_routing::restore_draft(&root,9,true,&guard).await;
            assert!(root.join("draft-restored").exists(),"late routing denial left this turn's exact-head PR ready");
            std::fs::remove_file(root.join("draft-restored")).unwrap();
            std::fs::write(root.join("actual-head"),"foreign-head").unwrap();
            manual_routing::restore_draft(&root,9,true,&guard).await;
            assert!(!root.join("draft-restored").exists(),"draft undo touched a changed remote candidate");
            guard.finish().unwrap();return;
        }
        if case.starts_with("paused-") {
            let id=if case=="paused-pr" {"owner/repo/pr/9"}else{"owner/repo#7"};
            let binding=crate::autoship_workflow::Binding {head:format!("{:040x}",9),contract:"contract".into(),evidence_hash:"evidence".into()};
            initial.register(id,"feat/repair-9",binding,1).unwrap();
            let lease=initial.claim(id,"original",2,100).unwrap().unwrap();
            initial.attach_pr_in_repo(&lease,"owner/repo",9,3).unwrap();
            initial.charge(&lease,"original-work",crate::autoship_workflow::ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},4).unwrap();
            initial.release(&lease,5).unwrap();initial.set_owner_pause(id,"github:owner",0,true,6).unwrap();
            let rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            let mut owned=adoption::authorized(&rows,"owner/repo",&["owner".into()]);owned.retain(|pr|pr.pr==9);
            owned[0].body="Previous retained owner request".into();adoption::persist(&path,&owned).unwrap();
        }
        let ids=["owner/repo#7","owner/repo#8","owner/repo/pr/9","owner/repo/pr/10"];
        let before=ids.map(|id|(initial.snapshot(id).unwrap(),initial.owner_control(id).unwrap(),initial.events(id).unwrap()));
        let prior_adoption=initial.adopted_pr("owner/repo",9).unwrap();drop(initial);
        let selected=if std::env::var("MANUAL_PR_ROUTE").unwrap()=="linked" {
            find_resumable_draft(&root,None,false).await.map(|p|p.0)
        }else {
            adopted_execution::next(&root,false).await.unwrap().map(|p|p.pr)
        };
        assert_eq!(selected,Some(10),"manual/unknown routing occupied the ordinary peer's turn");
        let store=WorkflowStore::open(&workflow::path()).unwrap();
        assert_eq!(store.adopted_pr("owner/repo",9).unwrap(),prior_adoption,"manual ownership was persisted or replaced before routing was checked");
        assert!(store.adopted_pr("owner/repo",10).unwrap().is_some());
        for (id,prior) in ids.into_iter().zip(before) {
            assert_eq!((store.snapshot(id).unwrap(),store.owner_control(id).unwrap(),store.events(id).unwrap()),prior,
                "selection changed execution, owner controls or accounting");
        }
        if matches!(case.as_str(),"pr-label"|"explicit-revoked") {
            let request=crate::autoship_workflow::submission::Request {repository:"owner/repo".into(),
                target:crate::autoship_workflow::submission::Target::Pr(9),provider:None,
                until:crate::autoship_workflow::submission::Milestone::Merged};
            let resolved=targeted::resolve(&root,&request).await;
            if case=="explicit-revoked" {assert!(resolved.is_err(),"routing label supplied repository authorization");}
            else {assert!(matches!(resolved,Ok(targeted::Resolved::Pr(pr)) if pr.pr==9),"explicit authorized targeting was blocked by an automatic routing label");}
        }
        if case.starts_with("paused-") {
            let mut rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            rows[0]["labels"]=serde_json::json!([]);
            std::fs::write(root.join("prs.json"),serde_json::to_vec(&rows).unwrap()).unwrap();
            assert!(manual_routing::allowed(&root,"owner/repo",9,None).await);
            let id=if case=="paused-pr" {"owner/repo/pr/9"}else{"owner/repo#7"};
            assert!(store.require_owner_dispatch(id).is_err(),"label removal cleared the durable owner pause");
            assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,4);
        }
        return;
    }
    for route in ["linked","source-less"] {
        for case in ["late-mutation","pr-label","source-label","unknown-pr","unknown-source","race","malformed-pr","malformed-source","malformed-reference","saturated-pr-labels","saturated-references","explicit-revoked","paused-pr","paused-issue"] {
            for draft in [false,true] {
                let dir=tempfile::tempdir_in("/tmp").unwrap();let root=dir.path().join("repo");
                std::fs::create_dir(&root).unwrap();
                for args in [vec!["init","-q"],vec!["remote","add","origin","https://github.com/owner/repo.git"]] {
                    assert!(std::process::Command::new("/usr/bin/git").args(args).current_dir(&root)
                        .env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                }
                if case=="late-mutation" {
                    assert!(std::process::Command::new("/usr/bin/git").args(["-c","user.name=Fixture","-c","user.email=fixture@example.invalid",
                        "-c","commit.gpgsign=false","commit","--quiet","--allow-empty","-m","Original fixture"])
                        .current_dir(&root).env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                    assert!(std::process::Command::new("/usr/bin/git").args(["update-ref","refs/remotes/origin/main","HEAD"])
                        .current_dir(&root).env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                    std::fs::write(root.join(".git/info/exclude"),"prs.json\nissues.json\ncalls\nmanual-after-admission\nactual-head\ndraft-restored\n").unwrap();
                }
                let references=|number|serde_json::json!([{"number":number,"url":format!("https://github.com/owner/repo/issues/{number}")}]);
                let mut manual_refs=if route=="linked" {references(7)}else{serde_json::json!([])};
                if route=="source-less" && matches!(case,"source-label"|"unknown-source"|"malformed-source") {
                    manual_refs=serde_json::json!([{"number":7,"url":"https://github.com/owner/repo/issues/7"},
                        {"number":8,"url":"https://github.com/owner/repo/issues/8"}]);
                }
                if case=="malformed-reference" {manual_refs=serde_json::json!([{"number":7,"url":"https://github.com/owner/repo/issues/not-7"}]);}
                let row=|number,refs,labels|serde_json::json!({"number":number,"isDraft":draft,
                    "headRefName":format!("feat/repair-{number}"),"headRefOid":format!("{number:040x}"),
                    "state":"OPEN","url":format!("https://github.com/owner/repo/pull/{number}"),
                    "baseRefName":"main","isCrossRepository":false,"headRepositoryOwner":{"login":"owner"},
                    "author":{"login":"owner"},"title":"Repair a widget","body":"Synthetic owner work",
                    "closingIssuesReferences":refs,"labels":labels,"files":[],"mergeable":"MERGEABLE"});
                let prs=serde_json::json!([row(9,manual_refs,if matches!(case,"pr-label"|"explicit-revoked"|"paused-pr"|"paused-issue") {serde_json::json!([{"name":"autoship-manual"}])}else{serde_json::json!([])}),
                    row(10,if route=="linked" {references(8)}else{serde_json::json!([])},serde_json::json!([]))]);
                let issues=serde_json::json!({"7":{"number":7,"state":"open","labels":if case=="source-label" {serde_json::json!([{"name":"autoship-manual"}])}else{serde_json::json!([])}},
                    "8":{"number":8,"state":"open","labels":[]}});
                std::fs::write(root.join("prs.json"),serde_json::to_vec(&prs).unwrap()).unwrap();
                std::fs::write(root.join("issues.json"),serde_json::to_vec(&issues).unwrap()).unwrap();
                let gh=dir.path().join("github.py");
                std::fs::write(&gh,r#"#!/usr/bin/python3
import json,os,pathlib,sys
a=sys.argv[1:];root=pathlib.Path(os.environ['AUTOSHIP_MANUAL_PR_FIXTURE'])
prs=json.loads((root/'prs.json').read_text());issues=json.loads((root/'issues.json').read_text())
case=os.environ['MANUAL_PR_CASE']
with (root/'calls').open('a') as out:out.write(json.dumps(a)+'\n')
if a[:2]==['repo','view']: print(('READ' if case=='explicit-revoked' else 'ADMIN') if 'viewerPermission' in a else 'owner/repo')
elif a[:2]==['pr','list']: print(json.dumps(prs))
elif a[:2]==['issue','list']: print(json.dumps(list(issues.values())))
elif a[:2]==['pr','view']:
 if case=='unknown-pr' and a[2]=='9':sys.exit(1)
 row=next(p for p in prs if p['number']==int(a[2]))
 if row['number']==9 and case=='race':row['labels']=[dict(name='autoship-manual')]
 if row['number']==9 and (root/'manual-after-admission').exists():row['labels']=[dict(name='autoship-manual')]
 if row['number']==9 and (root/'actual-head').exists():row['headRefOid']=(root/'actual-head').read_text()
 if row['number']==9 and case=='malformed-pr':row.pop('labels')
 if row['number']==9 and case=='saturated-pr-labels':row['labels']=[dict(name=f'ordinary-{n}') for n in range(100)]
 if row['number']==9 and case=='saturated-references':row['closingIssuesReferences']=[dict(number=7,url='https://github.com/owner/repo/issues/7') for _ in range(100)]
 print(json.dumps(row))
elif a[:1]==['api'] and '/issues/' in a[1]:
 n=a[1].split('/issues/',1)[1]
 if case=='unknown-source' and n=='7':sys.exit(1)
 row=issues[n]
 if case=='malformed-source' and n=='7':row.pop('labels')
 print(json.dumps(row))
elif a[:2]==['api','user']:print('owner')
elif a[:2]==['pr','ready'] and case=='late-mutation' and '--undo' in a:(root/'draft-restored').write_text('restored')
else:raise AssertionError('Unexpected or mutating GitHub call: '+repr(a))
"#).unwrap();
                std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
                let output=tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact","self_improve::manual_pr_routing_tests::automatic_pr_routes_skip_manual_work_before_ownership_and_advance_peer","--nocapture"])
                    .env_clear().env("PATH","/usr/bin:/bin").env("HOME",dir.path())
                    .env("XDG_STATE_HOME",dir.path().join("state"))
                    .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",dir.path().join("workflow.db"))
                    .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","owner")
                    .env("AUGMENTAGENT_GH_OWNER","owner").env("GH_BIN",gh)
                    .env("CODEX_CLI","/bin/false").env("CLAUDE_CLI","/bin/false")
                    .env(CHILD,&root).env("MANUAL_PR_ROUTE",route).env("MANUAL_PR_CASE",case)
                    .current_dir(&root).output().await.unwrap();
                assert!(output.status.success(),"{route}/{case}/draft={draft}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            }
        }
    }
}
