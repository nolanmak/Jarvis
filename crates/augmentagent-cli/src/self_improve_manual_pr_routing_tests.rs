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
        if case=="cross-repository-source" {
            let mut rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            rows[0]["closingIssuesReferences"]=serde_json::json!([{"number":70,"url":"https://github.com/other/repo/issues/70"}]);
            std::fs::write(root.join("prs.json"),serde_json::to_vec(&rows).unwrap()).unwrap();
            assert!(!manual_routing::allowed(&root,"owner/repo",9,Some(7)).await,
                "automatic routing silently ignored an unsupported cross-repository source");
            return;
        }
        if case=="push-failed-recovery" {
            let _=tracing_subscriber::fmt().with_writer(std::io::stderr).with_max_level(tracing::Level::WARN).try_init();
            let source=(std::env::var("MANUAL_PR_ROUTE").unwrap()=="linked").then_some(7);
            let candidate=root.join("candidate");
            assert!(std::process::Command::new("/usr/bin/git").args(["worktree","add","--quiet","--detach"])
                .arg(&candidate).arg("HEAD").current_dir(&root).status().unwrap().success());
            if source.is_some() {
                let binding=workflow::git_binding(&candidate,"Original request","fixture",&[]).await.unwrap();
                let now=chrono::Utc::now().timestamp_millis();
                initial.register("owner/repo#7","feat/repair-9",binding,now).unwrap();
                let lease=initial.claim("owner/repo#7","original-publisher",now,1000).unwrap().unwrap();
                initial.attach_pr_in_repo(&lease,"owner/repo",9,now).unwrap();
                initial.release(&lease,now+1).unwrap();
            }
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&candidate,repository:"owner/repo",source_issue:source,pr:Some(9),
                branch:"feat/repair-9",contract:"Original request",environment:"fixture",
            }).await.unwrap().unwrap();
            let hook=root.join(".git/hooks/pre-push");
            std::fs::write(&hook,"#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&hook,std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(guard.push("feat/repair-9").await.is_err(),"transport failure fixture did not fail");
            let id=guard.id().to_owned();let head=guard.binding().head.clone();guard.finish().unwrap();
            assert_eq!(initial.pending_operations(&id).unwrap().len(),1);
            let before=initial.snapshot(&id).unwrap().unwrap();
            assert!(initial.retryable_bound_push(&id).unwrap(),"failed Push lost its bound identity");
            assert!(initial.recovery_context(&id,&before.checkpoint.binding).unwrap().is_some(),"failed Push lost its original context");
            std::fs::remove_file(&hook).unwrap();
            let mut rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            rows[0]["headRefOid"]=head.clone().into();
            std::fs::write(root.join("prs.json"),serde_json::to_vec(&rows).unwrap()).unwrap();
            let mut candidates=adoption::authorized(&rows,"owner/repo",&["owner".into()]);
            if source.is_some() {
                let mut issues:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("issues.json")).unwrap()).unwrap();
                issues["7"]["state"]="closed".into();issues["7"]["state_reason"]="completed".into();
                std::fs::write(root.join("issues.json"),serde_json::to_vec(&issues).unwrap()).unwrap();
                let mut closed=candidates.clone();manual_routing::filter(&root,&mut closed).await;
                assert_eq!(initial.pending_operations(&id).unwrap().len(),1,"already completed source still replayed its pending Push");
                assert_eq!(initial.snapshot(&id).unwrap().unwrap().attempts,before.attempts,"closed source admitted another attempt");
                issues["7"]["state"]="open".into();issues["7"]["state_reason"]=serde_json::Value::Null;
                std::fs::write(root.join("issues.json"),serde_json::to_vec(&issues).unwrap()).unwrap();
            }
            manual_routing::filter(&root,&mut candidates).await;
            assert!(initial.pending_operations(&id).unwrap().is_empty(),"automatic inventory never recovered the failed bound Push");
            let out=std::process::Command::new("/usr/bin/git").args(["ls-remote","--heads","origin","refs/heads/feat/repair-9"])
                .current_dir(&root).output().unwrap();assert!(out.status.success());
            assert!(String::from_utf8(out.stdout).unwrap().starts_with(&head));
            let after=initial.snapshot(&id).unwrap().unwrap();
            assert_eq!(after.model_calls,before.model_calls,"Push recovery spent model work");
            assert_eq!(after.tokens,before.tokens);assert_eq!(after.attempts,before.attempts+1,"count exactly one native transport recovery attempt");
            assert!(adoption::episode_retries(&path,&candidates,chrono::Utc::now().timestamp_millis()).unwrap().contains(&9),
                "recovered Push remained hidden by the same-day attempt marker");
            return;
        }
        if case.starts_with("create-manual-") {
            let automatic=case=="create-manual-auto";
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:None,
                branch:"feat/repair-9",contract:"Original request",environment:"fixture",
            }).await.unwrap().unwrap();
            let head=guard.push_fresh(&root,"feat/repair-9",automatic).await.unwrap();
            std::fs::write(root.join("actual-head"),&head).unwrap();
            std::fs::write(root.join("manual-after-admission"),"owner routed source manually after the successful Push").unwrap();
            let args=["pr","create","--head","feat/repair-9","--base","main"];
            let result=publication::publish(&guard,&root,"feat/repair-9",&head,7,&args,automatic).await;
            if automatic {
                assert!(!root.join("create-writes").exists(),"source routed manually after Push still dispatched PR creation");
                assert!(result.unwrap_err().downcast_ref::<manual_routing::Hold>().is_some());
            }else {
                assert_eq!(result.unwrap().number,9,"explicit authorized creation was blocked by routing alone");
                let count=||std::fs::read_to_string(root.join("calls")).unwrap().lines()
                    .filter(|line|serde_json::from_str::<Vec<String>>(line).unwrap().starts_with(&["pr".into(),"create".into()])).count();
                assert_eq!(count(),1);
                // Observing a known publication is allowed even after the
                // source is manual; it must not grant a second creation.
                assert_eq!(publication::publish(&guard,&root,"feat/repair-9",&head,7,&args,true).await.unwrap().number,9);
                assert_eq!(count(),1,"known publication recovery created another PR");
            }
            guard.finish().unwrap();return;
        }
        if matches!(case.as_str(),"merge-label-during-policy"|"ready-pause-during-routing") {
            let out=std::process::Command::new("/usr/bin/git").args(["rev-parse","HEAD"]).current_dir(&root).output().unwrap();
            assert!(out.status.success());let head=String::from_utf8(out.stdout).unwrap().trim().to_owned();
            std::fs::write(root.join("actual-head"),&head).unwrap();
            let guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:None,pr:Some(9),
                branch:"feat/repair-9",contract:"Original request",environment:"fixture",
            }).await.unwrap().unwrap();
            if case=="ready-pause-during-routing" {
                guard.validate_note_dispatch().unwrap();
                assert!(manual_routing::allowed(&root,"owner/repo",9,None).await);
                let result=ready_for_merge(&root,9,Some(true),||guard.validate_note_dispatch()).await;
                assert!(!root.join("ready-writes").exists(),"owner pause accepted during routing still dispatched ready");
                assert_eq!(result,None);guard.finish().unwrap();return;
            }
            assert!(manual_routing::allowed(&root,"owner/repo",9,None).await);
            let operation=guard.intent(crate::autoship_workflow::OperationKind::MergePr).unwrap();
            let before=initial.snapshot(guard.id()).unwrap();
            let result=publication::merge_exact(&root,"owner/repo",9,&head,&operation,&guard.merge_lease().unwrap(),
                publication::MergeRouting {automatic:true,source_issue:None}).await;
            assert!(root.join("manual-after-admission").exists(),"fixture never changed routing during policy read");
            assert!(!root.join("merge-writes").exists(),"late manual routing still dispatched a merge mutation");
            assert!(result.unwrap_err().downcast_ref::<manual_routing::Hold>().is_some());
            assert!(initial.merge_dispatch(&operation.id).unwrap().is_none(),"routing hold reserved merge allowance");
            assert!(initial.pending_operations(guard.id()).unwrap().is_empty(),"known unused merge intent permanently blocks retry");
            assert_eq!(initial.snapshot(guard.id()).unwrap(),before,"routing check changed original work accounting");
            // Explicit authorized targeting bypasses only the routing label.
            let operation=guard.intent(crate::autoship_workflow::OperationKind::MergePr).unwrap();
            let _=publication::merge_exact(&root,"owner/repo",9,&head,&operation,&guard.merge_lease().unwrap(),
                publication::MergeRouting {automatic:false,source_issue:None}).await;
            assert!(root.join("merge-writes").exists(),"manual label blocked explicit authorized targeting");
            assert_eq!(initial.merge_dispatch(&operation.id).unwrap().unwrap().state,"dispatched","lost mutation response must stay uncertain");
            guard.finish().unwrap();return;
        }
        if case=="source-reopened-during-close-routing" {
            let mut rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            rows[0]["headRefName"]=format!("{BRANCH_PREFIX}7").into();
            rows[0]["body"]=format!("{SELF_IMPROVE_BODY_MARKER}7.").into();
            std::fs::write(root.join("prs.json"),serde_json::to_vec(&rows).unwrap()).unwrap();
            let mut issues:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("issues.json")).unwrap()).unwrap();
            issues["7"]["state"]="closed".into();issues["7"]["state_reason"]="completed".into();
            std::fs::write(root.join("issues.json"),serde_json::to_vec(&issues).unwrap()).unwrap();
            opened_prs::record(7,9,&"a".repeat(40),None);
            assert_eq!(source_reconcile::reconcile(&root,9,7,false,true).await,source_reconcile::SourceAction::Hold("source-state-changed"),"source reopened while routing was read, but its PR was retired");
            assert!(!root.join("close-writes").exists(),"reopened source still dispatched PR close");
            return;
        }
        if matches!(case.as_str(),"push-before-config-change"|"push-after-config-change"|"push-hook-composition"|"push-rewrite-once"|"push-missing-ccat"|"push-shadowed-ccat"|"push-valid-ccat"|"push-push-instead-of"|"push-nonbare-destination"|"push-empty-destination"|"push-multiple-destination") {
            let git=|args:&[&str]| {
                let out=std::process::Command::new("/usr/bin/git").args(args).current_dir(&root).output().unwrap();
                assert!(out.status.success(),"fixture Git failed: {}",String::from_utf8_lossy(&out.stderr));
                String::from_utf8(out.stdout).unwrap().trim().to_owned()
            };
            let mut first=git(&["remote","get-url","--push","origin"]);let other=root.parent().unwrap().join("other.git");
            git(&["init","--bare","--quiet",other.to_str().unwrap()]);
            let hook=root.join(".git/hooks/pre-push");
            if case=="push-after-config-change" {
                std::fs::write(&hook,format!("#!/usr/bin/python3\nimport subprocess\nfor args in [['remote','set-url','origin',{:?}],['remote','set-url','--push','origin',{:?}]]:subprocess.run(['/usr/bin/git',*args],check=True)\n",other.to_str().unwrap(),other.to_str().unwrap())).unwrap();
                std::fs::set_permissions(&hook,std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            if matches!(case.as_str(),"push-hook-composition"|"push-valid-ccat") {
                std::fs::write(&hook,format!("#!/usr/bin/python3\n# ccat-public-push-gate.sh fixture marker; no native gate execution claimed\nimport pathlib,sys,json,subprocess\np=subprocess.run(['/usr/bin/git','config','--get','core.hooksPath'],capture_output=True,text=True)\npathlib.Path({:?}).write_text(json.dumps(dict(args=sys.argv[1:],input=sys.stdin.read(),configuration=p.stdout)))\n",root.join("hook-observation"))).unwrap();
                std::fs::set_permissions(&hook,std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            if matches!(case.as_str(),"push-empty-destination"|"push-multiple-destination") {
                git(&["config","--add","remote.origin.pushurl",if case=="push-empty-destination" {""}else{other.to_str().unwrap()}]);
                git(&["config","--add","remote.origin.pushurl",&first]);
            }
            if case=="push-nonbare-destination" {
                let nonbare=root.parent().unwrap().join("nonbare");git(&["init","--quiet",nonbare.to_str().unwrap()]);
                git(&["remote","set-url","--push","origin",nonbare.to_str().unwrap()]);
                first=git(&["remote","get-url","--push","origin"]);
            }
            if case=="push-shadowed-ccat" {
                std::fs::write(&hook,"#!/bin/sh\n# ccat-public-push-gate.sh fixture marker\nexit 0\n").unwrap();
                std::fs::set_permissions(&hook,std::fs::Permissions::from_mode(0o700)).unwrap();
                let actual=root.parent().unwrap().join("actual-hooks");std::fs::create_dir(&actual).unwrap();
                std::fs::write(actual.join("pre-push"),"#!/bin/sh\nexit 0\n").unwrap();
                std::fs::set_permissions(actual.join("pre-push"),std::fs::Permissions::from_mode(0o700)).unwrap();
                git(&["config","core.hooksPath",actual.to_str().unwrap()]);
            }
            if case=="push-push-instead-of" {
                git(&["config",&format!("url.{}.pushInsteadOf",other.display()),&first]);
                first=git(&["remote","get-url","--push","origin"]);
            }
            if case=="push-rewrite-once" {
                git(&["push","--quiet",other.to_str().unwrap(),"HEAD:refs/heads/feat/repair-9"]);
                git(&["config",&format!("url.{}.insteadOf",first),"https://github.com/owner/repo.git"]);
                git(&["config",&format!("url.{}.insteadOf",other.display()),&first]);
                git(&["remote","set-url","--push","origin","https://github.com/owner/repo.git"]);
            }
            if case=="push-before-config-change" {
                let frozen=push_destination::Resolved::resolve(&root,"owner/repo").await.unwrap();
                let private=root.parent().unwrap().join("private-hook-control");std::fs::create_dir(&private).unwrap();
                let guarded=frozen.hook(&root,&private).await.unwrap();
                git(&["remote","set-url","--push","origin",other.to_str().unwrap()]);
                let (ok,_,_)=run("git",&["-c",guarded.config(),"push","origin","HEAD:refs/heads/feat/repair-9"],&root).await.unwrap();
                assert!(!ok,"configuration change bypassed the actual transport destination fence");
                for remote in [&first,other.to_str().unwrap()] {assert!(git(&["ls-remote","--heads",remote,"refs/heads/feat/repair-9"]).is_empty());}
                return;
            }
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:Some(9),
                branch:"feat/repair-9",contract:"Published original request",environment:"fixture",
            }).await.unwrap().unwrap();
            let empty_is_effective=if case=="push-empty-destination" {
                // Git 2.43 retains empty entries; Git 2.56 clears the list.
                // https://github.com/git/git/blob/v2.56.1/remote.c#L83
                // The contract concerns effective destinations, not raw config.
                let out=std::process::Command::new("/usr/bin/git").args(["remote","get-url","--push","--all","origin"])
                    .current_dir(&root).output().unwrap();assert!(out.status.success());
                String::from_utf8(out.stdout).unwrap()!=format!("{first}\n")
            }else {false};
            let head=git(&["rev-parse","HEAD"]);let result=guard.push("feat/repair-9").await;
            if empty_is_effective || matches!(case.as_str(),"push-missing-ccat"|"push-shadowed-ccat"|"push-multiple-destination") {
                assert!(result.is_err(),"invalid effective destination or missing required CCAT hook allowed a Push");
                assert!(initial.pending_operations(guard.id()).unwrap().is_empty(),"known invalid push configuration left an un-dispatched uncertain intent");
                guard.finish().unwrap();return;
            }
            assert_eq!(result.unwrap(),head);
            // Read the actual bare repository without applying another URL rewrite.
            let actual=git(&["-C",&first,"rev-parse","refs/heads/feat/repair-9"]);
            assert_eq!(actual,head,"the pinned actual transport was not published");
            if case=="push-after-config-change" {assert!(git(&["ls-remote","--heads",other.to_str().unwrap(),"refs/heads/feat/repair-9"]).is_empty());}
            if matches!(case.as_str(),"push-hook-composition"|"push-valid-ccat") {
                let proof:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("hook-observation")).unwrap()).unwrap();
                assert_eq!(proof["args"],serde_json::json!(["origin",first]));
                assert!(proof["input"].as_str().unwrap().contains(&head),"original hook lost Git's stdin");
                assert_eq!(proof["configuration"],"","temporary hooksPath leaked into the original hook");
            }
            assert!(initial.pending_operations(guard.id()).unwrap().is_empty());
            guard.finish().unwrap();return;
        }
        if case=="push-separate-remotes" {
            let git=|args:&[&str]| {
                let out=std::process::Command::new("/usr/bin/git").args(args).current_dir(&root).output().unwrap();
                assert!(out.status.success(),"fixture Git failed: {}",String::from_utf8_lossy(&out.stderr));
                String::from_utf8(out.stdout).unwrap().trim().to_owned()
            };
            let head=git(&["rev-parse","HEAD"]);
            git(&["push","--quiet","origin","HEAD:refs/heads/feat/repair-9"]);
            let other=root.parent().unwrap().join("other.git");git(&["init","--bare","--quiet",other.to_str().unwrap()]);
            git(&["remote","set-url","--push","origin",other.to_str().unwrap()]);
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:Some(9),
                branch:"feat/repair-9",contract:"Published original request",environment:"fixture",
            }).await.unwrap().unwrap();
            assert_eq!(guard.push("feat/repair-9").await.unwrap(),head);
            let pushed=git(&["ls-remote","--heads",other.to_str().unwrap(),"refs/heads/feat/repair-9"]);
            assert_eq!(pushed.split_whitespace().next(),Some(head.as_str()),"matching fetch head falsely completed a Push without publishing to its different push destination");
            assert!(initial.pending_operations(guard.id()).unwrap().is_empty());
            guard.finish().unwrap();return;
        }
        if case.starts_with("post-rabbit-") {
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:Some(9),
                branch:"feat/repair-9",contract:"Published original request",environment:"fixture",
            }).await.unwrap().unwrap();
            let automatic=std::env::var("MANUAL_PR_ROUTE").unwrap()=="linked";
            let number=(case!="post-rabbit-unknown").then_some(9);
            let result=record_published_rabbit_observation(&root,&mut guard,number,&"a".repeat(40),
                RABBIT_NOT_WAITED_FOR,"feat/repair-9","https://github.com/owner/repo/pull/9",7,automatic).await;
            let allowed=case=="post-rabbit-ordinary" || (case=="post-rabbit-label" && !automatic);
            assert_eq!(result.unwrap_or(false),allowed,"late observation control did not fence publication: {case}/automatic={automatic}");
            assert_eq!(root.join("note-writes").exists(),allowed,"late observation wrote a note without current authority");
            if case!="post-rabbit-unknown" {assert!(std::fs::read_to_string(root.join("calls")).unwrap().contains("/pulls/9/"),"fixture never read actual review metadata");}
            guard.finish().unwrap();return;
        }
        if case.starts_with("post-control-") {
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:Some(9),
                branch:"feat/repair-9",contract:"Published original request",environment:"fixture",
            }).await.unwrap().unwrap();
            let now=chrono::Utc::now().timestamp_millis();
            if case=="post-control-pause" {
                initial.set_owner_pause(guard.id(),"github:owner",0,true,now).unwrap();
            }else {
                let origin=crate::autoship_workflow::discord_ingress::DiscordOrigin {bot:1,owner:2,channel:3,message:4};
                let request=crate::autoship_workflow::submission::Request {repository:"owner/repo".into(),
                    target:crate::autoship_workflow::submission::Target::Pr(9),provider:None,
                    until:crate::autoship_workflow::submission::Milestone::Merged};
                let admitted=initial.submit_discord_authorized(&origin,&request,&"a".repeat(64),now).unwrap();
                assert_eq!(admitted.admission.workflow,guard.id());
                let message=crate::autoship_workflow::discord_ingress::DiscordOrigin {message:5,..origin};
                assert!(initial.enqueue_owner_message(&message,guard.id(),"Please reconsider this proposal",None,now+1).unwrap().accepted);
                // Isolate the inbox fence from the pause fence in this disposable
                // fixture only. Production never clears pauses behind input.
                rusqlite::Connection::open(&path).unwrap().execute("UPDATE workflow_owner_control SET paused=0 WHERE workflow=?1",[guard.id()]).unwrap();
                assert_eq!(initial.pending_owner_messages(guard.id()).unwrap(),1);
            }
            let snapshot=initial.snapshot(guard.id()).unwrap();let events=initial.events(guard.id()).unwrap();
            let automatic=std::env::var("MANUAL_PR_ROUTE").unwrap()=="linked";
            for target in [manual_routing::NoteTarget::Pr,manual_routing::NoteTarget::Issue] {
                let result=manual_routing::published_note(&root,&mut guard,9,7,automatic,target,"Synthetic controlled-work note").await;
                assert!(!result.unwrap_or(false),"late owner control permitted a note: {case}/automatic={automatic}");
            }
            assert!(!root.join("note-writes").exists(),"late owner control sent a PR/source note");
            assert_eq!(initial.snapshot(guard.id()).unwrap(),snapshot);assert_eq!(initial.events(guard.id()).unwrap(),events);
            guard.finish().unwrap();return;
        }
        if case.starts_with("round-") {
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:Some(9),
                branch:"feat/repair-9",contract:"Retained original request",environment:"fixture",
            }).await.unwrap().unwrap();
            let hook=root.join(".git/hooks/pre-push");
            let script=if case=="round-pause-during-push" {
                // The actual Git hook fault-injects a late pause into this
                // disposable fixture DB only; production state is untouched.
                format!("#!/usr/bin/python3\nimport sqlite3\nc=sqlite3.connect({:?})\nc.execute(\"INSERT OR REPLACE INTO workflow_owner_control(workflow,revision,paused,principal,updated_at) VALUES(?,1,1,'github:owner',1)\",({:?},))\nc.commit()\n",path.to_str().unwrap(),guard.id())
            }else {format!("#!/usr/bin/python3\nfrom pathlib import Path\nPath({:?}).write_text('owner changed routing during actual push')\n",root.join("manual-after-admission"))};
            std::fs::write(&hook,script).unwrap();
            std::fs::set_permissions(&hook,std::fs::Permissions::from_mode(0o700)).unwrap();
            let result=guard.publish_round("feat/repair-9",&root,9,1,"gate repair","Synthetic verified findings",false,true,Some(7)).await;
            if case=="round-label-during-push" {assert!(root.join("manual-after-admission").exists(),"hook never changed routing during the push");}
            else {assert!(initial.owner_control(guard.id()).unwrap().unwrap().paused,"hook never paused the workflow during actual Push");}
            assert!(!root.join("note-writes").exists(),"a successful Push was followed by a note despite the new manual route");
            assert!(!result.unwrap_or(false),"round publication continued after late routing or owner control");
            let remote=std::process::Command::new("/usr/bin/git").args(["ls-remote","--heads","origin","refs/heads/feat/repair-9"]).current_dir(&root).output().unwrap();
            assert!(remote.status.success() && !remote.stdout.is_empty(),"fixture never made a successful real Push");
            assert!(initial.pending_operations(guard.id()).unwrap().is_empty(),"routing denial lost the already-observed Push outcome");
            guard.finish().unwrap();return;
        }
        if case.starts_with("post-note-") {
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:Some(9),
                branch:"feat/repair-9",contract:"Published original request",environment:"fixture",
            }).await.unwrap().unwrap();
            if case=="post-note-between" {
                assert!(manual_routing::published_note(&root,&mut guard,9,7,true,manual_routing::NoteTarget::Pr,"Synthetic initial review note").await.unwrap());
            }else {std::fs::write(root.join("manual-after-admission"),b"late owner routing").unwrap();}
            let prior=std::fs::read(root.join("note-writes")).ok();
            for target in [manual_routing::NoteTarget::Pr,manual_routing::NoteTarget::Issue] {
                assert!(!manual_routing::published_note(&root,&mut guard,9,7,true,target,"Synthetic parked-work note").await.unwrap(),
                    "fresh publication wrote a note after current routing changed");
            }
            assert_eq!(std::fs::read(root.join("note-writes")).ok(),prior,"denied notes changed the PR or its source issue");
            assert!(manual_routing::published_note(&root,&mut guard,9,7,false,manual_routing::NoteTarget::Pr,"Explicit owner note").await.unwrap());
            guard.finish().unwrap();return;
        }
        if matches!(case.as_str(),"late-fresh-claim"|"fresh-uncertain"|"fresh-uncertain-unobserved"|"fresh-uncertain-unbound"|"fresh-uncertain-other-fetch"|"fresh-uncertain-destination-changed"|"fresh-uncertain-branch-changed"|"fresh-uncertain-access-denied"|"fresh-uncertain-missing-peer"|"late-fresh-recheck") {
            let git=|args:&[&str]| {
                let out=std::process::Command::new("/usr/bin/git").args(args).current_dir(&root).output().unwrap();
                assert!(out.status.success(),"fixture Git failed: {}",String::from_utf8_lossy(&out.stderr));
                String::from_utf8(out.stdout).unwrap().trim().to_owned()
            };
            let published=git(&["rev-parse","HEAD"]);git(&["push","--quiet","origin","HEAD:refs/heads/main"]);git(&["push","--quiet","origin","HEAD:refs/heads/feat/repair-9"]);
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:Some(7),pr:None,
                branch:"feat/repair-9",contract:"Original request",environment:"fixture",
            }).await.unwrap().unwrap();
            guard.require_fresh_issue(&root).await.unwrap();
            git(&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
                "commit","--quiet","--allow-empty","-m","New local fresh revision"]);
            guard.transition(crate::autoship_workflow::Stage::Build,"revision_created","publish_revision",0).await.unwrap();
            let before=initial.snapshot(guard.id()).unwrap().unwrap();
            std::fs::write(root.join("fresh-claim-appeared"),b"owner opened manual PR after earlier absence observation").unwrap();
            if case.starts_with("fresh-uncertain") {
                guard.transition(crate::autoship_workflow::Stage::Publish,"publishing_revision","push",0).await.unwrap();
                let id=guard.id().to_owned();let binding=guard.binding().clone();guard.finish().unwrap();
                let now=chrono::Utc::now().timestamp_millis();
                let lease=initial.claim(&id,"lost-response-controller",now,1000).unwrap().unwrap();
                let missing_peer=if matches!(case.as_str(),"fresh-uncertain-missing-peer"|"fresh-uncertain-access-denied") {
                    let peer="owner/repo#8";
                    initial.register(peer,"feat/missing-8",binding.clone(),now-20).unwrap();
                    let peer_lease=initial.claim(peer,"prior-controller",now-19,1000).unwrap().unwrap();
                    initial.charge(&peer_lease,"original-work",crate::autoship_workflow::ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},now-18).unwrap();
                    initial.begin_bound_push(&peer_lease,&binding,"https://github.com/owner/repo.git","refs/heads/feat/missing-8",now-17).unwrap();
                    initial.release(&peer_lease,now-16).unwrap();
                    Some((initial.snapshot(peer).unwrap(),initial.events(peer).unwrap()))
                }else {None};
                let operation=if case=="fresh-uncertain-unbound" {
                    initial.begin_operation(&lease,crate::autoship_workflow::OperationKind::Push,&binding,now).unwrap()
                }else {initial.begin_bound_push(&lease,&binding,"https://github.com/owner/repo.git","refs/heads/feat/repair-9",now).unwrap()};
                // Simulate a genuinely dispatched push whose response was lost.
                if matches!(case.as_str(),"fresh-uncertain"|"fresh-uncertain-unbound"|"fresh-uncertain-other-fetch"|"fresh-uncertain-destination-changed"|"fresh-uncertain-branch-changed"|"fresh-uncertain-access-denied"|"fresh-uncertain-missing-peer") {git(&["push","--quiet","origin","HEAD:refs/heads/feat/repair-9"]);}
                if matches!(case.as_str(),"fresh-uncertain-unbound"|"fresh-uncertain-other-fetch"|"fresh-uncertain-destination-changed") {
                    let other=root.parent().unwrap().join("other.git");git(&["init","--bare","--quiet",other.to_str().unwrap()]);
                    if case=="fresh-uncertain-other-fetch" {git(&["remote","set-url","origin",other.to_str().unwrap()]);}
                }
                // Network I/O is served by the existing scripted GitHub API;
                // effective push destination parsing is the actual Git command.
                git(&["remote","set-url","--push","origin",if matches!(case.as_str(),"fresh-uncertain-unbound"|"fresh-uncertain-destination-changed") {"https://github.com/owner/other.git"}else{"https://github.com/owner/repo.git"}]);
                initial.release(&lease,now+1).unwrap();
                if case=="fresh-uncertain-branch-changed" {
                    // Isolate retained-effect identity from the mutable workflow
                    // branch in this fixture; production state is never edited.
                    rusqlite::Connection::open(&path).unwrap().execute("UPDATE workflow SET branch='feat/renamed-after-dispatch' WHERE id=?1",[&id]).unwrap();
                }
                let recovery_before=initial.snapshot(&id).unwrap();let events_before=initial.events(&id).unwrap();
                assert!(publication::claimed_after_reconcile(&root,7,false).await.unwrap());
                if matches!(case.as_str(),"fresh-uncertain-unobserved"|"fresh-uncertain-unbound"|"fresh-uncertain-access-denied") {
                    assert!(initial.pending_operations(&id).unwrap().iter().any(|row|row.id==operation.id),"unknown push was cleared without its exact remote head");
                    assert_eq!(initial.snapshot(&id).unwrap(),recovery_before);assert_eq!(initial.events(&id).unwrap(),events_before);
                    if case=="fresh-uncertain-access-denied" {
                        let calls=std::fs::read_to_string(root.join("calls")).unwrap();
                        assert!(calls.contains("/git/ref/heads/main"),"ref404 was called absence without proving Git-data access");
                        assert!(!calls.contains("/git/ref/heads/feat/repair-9"),"inaccessible Git API fanned out to a later pending ref");
                        assert_eq!(initial.pending_operations("owner/repo#8").unwrap().len(),1);
                    }
                    return;
                }
                assert!(initial.pending_operations(&id).unwrap().is_empty(),"observable Push remained pending: expected={}, actual={}, calls={}, read={}",binding.head,git(&["ls-remote","--heads",root.parent().unwrap().join("remote.git").to_str().unwrap(),"refs/heads/feat/repair-9"]),std::fs::read_to_string(root.join("calls")).unwrap(),std::fs::read_to_string(root.join("push-read-output")).unwrap_or_default());
                if let Some((snapshot,events))=missing_peer {
                    assert_eq!(initial.snapshot("owner/repo#8").unwrap(),snapshot);
                    assert_eq!(initial.events("owner/repo#8").unwrap(),events);
                    assert_eq!(initial.pending_operations("owner/repo#8").unwrap().len(),1);
                }
                let conn=rusqlite::Connection::open(&path).unwrap();
                assert_eq!(conn.query_row("SELECT COUNT(*) FROM operation WHERE workflow=?1",[&id],|r|r.get::<_,i64>(0)).unwrap(),1,"reconciliation manufactured another Push");
                assert_eq!(conn.query_row("SELECT receipt FROM operation WHERE id=?1",[&operation.id],|r|r.get::<_,String>(0)).unwrap(),format!("git-head:{}",binding.head));
                assert_eq!(initial.snapshot(&id).unwrap().unwrap().model_calls,before.model_calls);
                assert_eq!(initial.snapshot(&id).unwrap(),recovery_before,"free push observation changed attempts, lease generation, checkpoint or original charges");
                return;
            }
            let result=guard.push_fresh(&root,"feat/repair-9",true).await;
            assert_eq!(git(&["ls-remote","--heads","origin","refs/heads/feat/repair-9"]).split_whitespace().next(),Some(published.as_str()),
                "fresh publication updated a newly claimed manual PR before checking routing");
            assert_eq!(result.unwrap_err().downcast_ref::<linked_claims::Hold>(),Some(&linked_claims::Hold::Claimed));
            let after=initial.snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.model_calls,before.model_calls);
            assert!(initial.pending_operations(guard.id()).unwrap().is_empty(),"late claim created an un-dispatched Push intent");
            guard.finish().unwrap();return;
        }
        if case=="late-adopted-retry" {
            let git=|args:&[&str]| {
                let out=std::process::Command::new("/usr/bin/git").args(args).current_dir(&root).output().unwrap();
                assert!(out.status.success(),"fixture Git failed: {}",String::from_utf8_lossy(&out.stderr));
                String::from_utf8(out.stdout).unwrap().trim().to_owned()
            };
            let published=git(&["rev-parse","HEAD"]);
            // This is the owner's pre-existing PR, not an automation Push.
            git(&["push","--quiet","origin","HEAD:refs/heads/feat/repair-9"]);
            let mut rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            rows[0]["headRefOid"]=published.clone().into();
            std::fs::write(root.join("prs.json"),serde_json::to_vec(&rows).unwrap()).unwrap();
            let mut candidates=adoption::authorized(&rows,"owner/repo",&["owner".into()]);
            candidates.retain(|p|p.pr==9);adoption::persist(&path,&candidates).unwrap();
            let source=candidates[0].source_issue;
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:source,pr:Some(9),
                branch:"feat/repair-9",contract:"Retained ordinary request",environment:"fixture",
            }).await.unwrap().unwrap();
            git(&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
                "commit","--quiet","--allow-empty","-m","Preserved local revision"]);
            guard.transition(crate::autoship_workflow::Stage::Build,"revision_created","publish_revision",0).await.unwrap();
            let before=initial.snapshot(guard.id()).unwrap().unwrap();
            std::fs::write(root.join("manual-after-admission"),b"late owner routing").unwrap();
            assert!(!guard.publish_round("feat/repair-9",&root,9,1,"gate repair","Retained findings",false,true,source).await.unwrap());
            assert_eq!(git(&["ls-remote","--heads","origin","refs/heads/feat/repair-9"]).split_whitespace().next(),Some(published.as_str()));
            let held=initial.snapshot(guard.id()).unwrap().unwrap();let id=guard.id().to_owned();guard.finish().unwrap();
            std::fs::remove_file(root.join("manual-after-admission")).unwrap();
            assert!(manual_routing::allowed(&root,"owner/repo",9,source).await);
            let now=held.checkpoint.next.eligible_at_ms+1;
            let snapshot=initial.snapshot(&id).unwrap();let events=initial.events(&id).unwrap();
            assert_eq!(initial.retained_candidate_head("owner/repo",9,"feat/repair-9",&published,&held.checkpoint.binding.head,now).unwrap(),
                Some(held.checkpoint.binding.head.clone()),"newly adopted local work lost its admitted published-head custody");
            let retries=adoption::episode_retries(&path,&candidates,now).unwrap();
            assert!(retries.contains(&9),"label removal stranded the due retained revision");
            if let Some(issue)=source {
                let mut ledger=AttemptLedger::default();ledger.mark(1,issue);
                assert_eq!(adoption::select_linked_with_retries(&candidates,&ledger,1,&Default::default(),None,0,&retries).unwrap().0,9,
                    "same-day marker hid the newly adopted retained revision after label removal");
                assert!(ledger.attempted_today(1,issue),"recovery erased the original attempt marker");
            }
            assert_eq!(initial.snapshot(&id).unwrap(),snapshot);assert_eq!(initial.events(&id).unwrap(),events);
            assert_eq!(held.model_calls,before.model_calls);assert_eq!(held.tokens,before.tokens);
            let conn=rusqlite::Connection::open(&path).unwrap();
            assert_eq!(conn.query_row("SELECT COUNT(*) FROM operation WHERE workflow=?1",[&id],|r|r.get::<_,i64>(0)).unwrap(),0,
                "admission or recovery manufactured an automation Push receipt");
            return;
        }
        if case=="late-mutation" {
            let source=(std::env::var("MANUAL_PR_ROUTE").unwrap()=="linked").then_some(7);
            let mut guard=workflow::WorkflowGuard::acquire_fixture(&path,workflow::Start {
                repo_root:&root,worktree:&root,repository:"owner/repo",source_issue:source,pr:Some(9),
                branch:"feat/repair-9",contract:"Retained ordinary request",environment:"fixture",
            }).await.unwrap().unwrap();
            assert!(manual_routing::permit_mutation(&root,9,source,true,&mut guard).await.unwrap());
            let before=initial.snapshot(guard.id()).unwrap().unwrap();
            std::fs::write(root.join("manual-after-admission"),b"late owner routing").unwrap();
            let published=guard.publish_round("feat/repair-9",&root,9,1,"gate repair","Retained repair findings",false,true,source).await.unwrap();
            let remote=std::process::Command::new("/usr/bin/git").args(["ls-remote","--heads","origin","refs/heads/feat/repair-9"])
                .current_dir(&root).output().unwrap();
            assert!(remote.status.success() && remote.stdout.is_empty(),
                "production resume round pushed after a manual label was applied following admission");
            assert!(!published,"production resume round continued after a late routing denial");
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
        if matches!(case.as_str(),"changed-owned-source"|"missing-owned-source") {
            let binding=crate::autoship_workflow::Binding {head:format!("{:040x}",9),contract:"contract".into(),evidence_hash:"evidence".into()};
            initial.register("owner/repo#7","feat/repair-9",binding,1).unwrap();
            let lease=initial.claim("owner/repo#7","original",2,100).unwrap().unwrap();
            initial.attach_pr_in_repo(&lease,"owner/repo",9,3).unwrap();
            initial.charge(&lease,"original-work",crate::autoship_workflow::ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},4).unwrap();
            initial.release(&lease,5).unwrap();
            let mut rows:serde_json::Value=serde_json::from_slice(&std::fs::read(root.join("prs.json")).unwrap()).unwrap();
            rows[0]["closingIssuesReferences"]=serde_json::json!([{"number":7,"url":"https://github.com/owner/repo/issues/7"}]);
            let mut owned=adoption::authorized(&rows,"owner/repo",&["owner".into()]);owned.retain(|pr|pr.pr==9);
            adoption::persist(&path,&owned).unwrap();
            AttemptLedger::mark_persist(&attempt_ledger_path(),utc_day_now(),7);
            assert!(!AttemptLedger::load(&attempt_ledger_path()).attempted_today(utc_day_now(),70),"new source must remain unattempted");
        }
        if case.starts_with("paused-") || case=="pending-pr-unlabelled" {
            let id=if matches!(case.as_str(),"paused-pr"|"pending-pr-unlabelled") {"owner/repo/pr/9"}else{"owner/repo#7"};
            let binding=crate::autoship_workflow::Binding {head:format!("{:040x}",9),contract:"contract".into(),evidence_hash:"evidence".into()};
            initial.register(id,"feat/repair-9",binding,1).unwrap();
            let lease=initial.claim(id,"original",2,100).unwrap().unwrap();
            initial.attach_pr_in_repo(&lease,"owner/repo",9,3).unwrap();
            initial.charge(&lease,"original-work",crate::autoship_workflow::ResourceCharge {model_calls:4,tokens:1234,wall_ms:700},4).unwrap();
            if case=="pending-pr-unlabelled" {initial.begin_operation(&lease,crate::autoship_workflow::OperationKind::Push,&initial.snapshot(id).unwrap().unwrap().checkpoint.binding,5).unwrap();}
            initial.release(&lease,5).unwrap();
            if case.starts_with("paused-") {initial.set_owner_pause(id,"github:owner",0,true,6).unwrap();}
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
        for case in ["cross-repository-source","push-failed-recovery","create-manual-auto","create-manual-explicit","merge-label-during-policy","ready-pause-during-routing","source-reopened-during-close-routing","fresh-uncertain-branch-changed","push-empty-destination","push-multiple-destination","push-nonbare-destination","fresh-uncertain-access-denied","fresh-uncertain-destination-changed","push-valid-ccat","push-shadowed-ccat","push-push-instead-of","push-missing-ccat","push-before-config-change","push-after-config-change","push-hook-composition","push-rewrite-once","push-separate-remotes","post-rabbit-label","post-rabbit-pause","post-rabbit-unknown","post-rabbit-ordinary","round-pause-during-push","post-control-pause","post-control-followup","fresh-uncertain-missing-peer","fresh-uncertain-other-fetch","round-label-during-push","fresh-uncertain-unbound","paused-unlabelled","pending-pr-unlabelled","fresh-uncertain-unobserved","late-fresh-recheck","post-note-manual","post-note-unknown","post-note-between","changed-owned-source","missing-owned-source","fresh-uncertain","late-fresh-claim","late-adopted-retry","late-mutation","pr-label","source-label","unknown-pr","unknown-source","race","malformed-pr","malformed-source","malformed-reference","saturated-pr-labels","saturated-references","explicit-revoked","paused-pr","paused-issue"] {
            if std::env::var("MANUAL_PR_FOCUS").is_ok_and(|focus|focus!=case) {continue;}
            for draft in [false,true] {
                let dir=tempfile::tempdir_in("/tmp").unwrap();let root=dir.path().join("repo");
                std::fs::create_dir(&root).unwrap();
                for args in [vec!["init","-q"],vec!["remote","add","origin","https://github.com/owner/repo.git"]] {
                    assert!(std::process::Command::new("/usr/bin/git").args(args).current_dir(&root)
                        .env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                }
                if matches!(case,"late-mutation"|"late-adopted-retry"|"late-fresh-claim"|"fresh-uncertain"|"fresh-uncertain-unobserved"|"fresh-uncertain-unbound"|"fresh-uncertain-other-fetch"|"fresh-uncertain-destination-changed"|"fresh-uncertain-branch-changed"|"fresh-uncertain-access-denied"|"fresh-uncertain-missing-peer"|"late-fresh-recheck") || case.starts_with("create-") || case.starts_with("merge-") || case.starts_with("ready-") || case.starts_with("post-note-") || case.starts_with("post-control-") || case.starts_with("post-rabbit-") || case.starts_with("round-") || case.starts_with("push-") {
                    assert!(std::process::Command::new("/usr/bin/git").args(["-c","user.name=Fixture","-c","user.email=fixture@example.invalid",
                        "-c","commit.gpgsign=false","commit","--quiet","--allow-empty","-m","Original fixture"])
                        .current_dir(&root).env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                    assert!(std::process::Command::new("/usr/bin/git").args(["update-ref","refs/remotes/origin/main","HEAD"])
                        .current_dir(&root).env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                    std::fs::write(root.join(".git/info/exclude"),"prs.json\nissues.json\ncalls\nmanual-after-admission\nactual-head\ndraft-restored\nfresh-claim-appeared\nfresh-observations\nnote-writes\npush-read-output\nhook-observation\nmerge-writes\nready-writes\ncreate-writes\n").unwrap();
                    let remote=dir.path().join("remote.git");
                    assert!(std::process::Command::new("/usr/bin/git").args(["init","--bare","--quiet"])
                        .arg(&remote).env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                    assert!(std::process::Command::new("/usr/bin/git").args(["remote","set-url","origin"])
                        .arg(remote).current_dir(&root).env_clear().env("PATH","/usr/bin:/bin").status().unwrap().success());
                }
                let references=|number|serde_json::json!([{"number":number,"url":format!("https://github.com/owner/repo/issues/{number}")}]);
                let mut manual_refs=if route=="linked" || matches!(case,"late-fresh-claim"|"fresh-uncertain"|"fresh-uncertain-unobserved"|"fresh-uncertain-unbound"|"fresh-uncertain-other-fetch"|"fresh-uncertain-destination-changed"|"fresh-uncertain-branch-changed"|"fresh-uncertain-access-denied"|"fresh-uncertain-missing-peer"|"late-fresh-recheck") {references(7)}else{serde_json::json!([])};
                if route=="source-less" && matches!(case,"source-label"|"unknown-source"|"malformed-source") {
                    manual_refs=serde_json::json!([{"number":7,"url":"https://github.com/owner/repo/issues/7"},
                        {"number":8,"url":"https://github.com/owner/repo/issues/8"}]);
                }
                if case=="changed-owned-source" {manual_refs=references(70);}
                if case=="missing-owned-source" {manual_refs=serde_json::json!([]);}
                if case=="malformed-reference" {manual_refs=serde_json::json!([{"number":7,"url":"https://github.com/owner/repo/issues/not-7"}]);}
                let row=|number,refs,labels|serde_json::json!({"id":format!("PR_fixture_{number}"),"number":number,"isDraft":draft,
                    "headRefName":format!("feat/repair-{number}"),"headRefOid":format!("{number:040x}"),
                    "state":"OPEN","url":format!("https://github.com/owner/repo/pull/{number}"),
                    "baseRefName":"main","isCrossRepository":false,"headRepositoryOwner":{"login":"owner"},
                    "author":{"login":"owner"},"title":"Repair a widget","body":"Synthetic owner work",
                    "closingIssuesReferences":refs,"labels":labels,"files":[],"mergeable":"MERGEABLE"});
                let prs=serde_json::json!([row(9,manual_refs,if matches!(case,"pr-label"|"explicit-revoked"|"paused-pr"|"paused-issue"|"late-fresh-claim"|"fresh-uncertain"|"fresh-uncertain-unobserved"|"fresh-uncertain-unbound"|"fresh-uncertain-other-fetch"|"fresh-uncertain-destination-changed"|"fresh-uncertain-branch-changed"|"fresh-uncertain-access-denied"|"fresh-uncertain-missing-peer"|"late-fresh-recheck") {serde_json::json!([{"name":"autoship-manual"}])}else{serde_json::json!([])}),
                    row(10,if route=="linked" {references(8)}else{serde_json::json!([])},serde_json::json!([]))]);
                let issues=serde_json::json!({"7":{"number":7,"state":"open","labels":if case=="source-label" {serde_json::json!([{"name":"autoship-manual"}])}else{serde_json::json!([])}},
                    "8":{"number":8,"state":"open","labels":[]},"70":{"number":70,"state":"open","labels":[]}});
                std::fs::write(root.join("prs.json"),serde_json::to_vec(&prs).unwrap()).unwrap();
                std::fs::write(root.join("issues.json"),serde_json::to_vec(&issues).unwrap()).unwrap();
                let gh=dir.path().join("github.py");
                std::fs::write(&gh,r#"#!/usr/bin/python3
import json,os,pathlib,sys,subprocess,urllib.parse,sqlite3
a=sys.argv[1:];root=pathlib.Path(os.environ['AUTOSHIP_MANUAL_PR_FIXTURE'])
prs=json.loads((root/'prs.json').read_text());issues=json.loads((root/'issues.json').read_text())
case=os.environ['MANUAL_PR_CASE']
with (root/'calls').open('a') as out:out.write(json.dumps(a)+'\n')
if a[:2]==['repo','view']: print(('READ' if case=='explicit-revoked' else 'ADMIN') if 'viewerPermission' in a else 'owner/repo')
elif a[:2]==['pr','list']:
 if case.startswith('create-manual-'):print('[]');sys.exit(0)
 if case=='late-fresh-recheck':
  p=root/'fresh-observations';n=int(p.read_text())+1 if p.exists() else 1;p.write_text(str(n));print(json.dumps([] if n<=2 else prs))
 else: print(json.dumps([] if case in ('late-fresh-claim','fresh-uncertain','fresh-uncertain-unobserved','fresh-uncertain-unbound','fresh-uncertain-other-fetch','fresh-uncertain-destination-changed','fresh-uncertain-branch-changed','fresh-uncertain-access-denied','fresh-uncertain-missing-peer') and not (root/'fresh-claim-appeared').exists() else prs))
elif a[:2]==['issue','list']: print(json.dumps(list(issues.values())))
elif a[:2]==['pr','view']:
 if (case=='unknown-pr' or (case=='post-note-unknown' and (root/'manual-after-admission').exists())) and a[2]=='9':sys.exit(1)
 row=next(p for p in prs if p['number']==int(a[2]))
 if case=='ready-pause-during-routing' and 'number,labels,closingIssuesReferences' in a:
  c=sqlite3.connect(os.environ['AUGMENTAGENT_AUTOPR_WORKFLOW_DB']);c.execute("INSERT OR REPLACE INTO workflow_owner_control(workflow,revision,paused,principal,updated_at) VALUES('owner/repo/pr/9',1,1,'github:owner',1)");c.commit();c.close()
 if case=='source-reopened-during-close-routing' and 'number,labels,closingIssuesReferences' in a:
  issues['7']['state']='open';issues['7']['state_reason']=None;(root/'issues.json').write_text(json.dumps(issues))
 if row['number']==9 and case=='race':row['labels']=[dict(name='autoship-manual')]
 if row['number']==9 and (root/'manual-after-admission').exists():row['labels']=[dict(name='autoship-manual')]
 if row['number']==9 and (root/'actual-head').exists():row['headRefOid']=(root/'actual-head').read_text()
 if row['number']==9 and case=='malformed-pr':row.pop('labels')
 if row['number']==9 and case=='saturated-pr-labels':row['labels']=[dict(name=f'ordinary-{n}') for n in range(100)]
 if row['number']==9 and case=='saturated-references':row['closingIssuesReferences']=[dict(number=7,url='https://github.com/owner/repo/issues/7') for _ in range(100)]
 print(json.dumps(row))
elif case=='merge-label-during-policy' and a[:2]==['api','graphql']:
 if any('AutoshipDirectMergePolicy' in x for x in a):print(json.dumps(dict(data=dict(repository=dict(mergeQueue=None)))))
 else:(root/'merge-writes').write_text('dispatched');print('synthetic lost response',file=sys.stderr);sys.exit(1)
elif case=='merge-label-during-policy' and a[:1]==['api'] and a[1].endswith('/rules/branches/main'):print('[]')
elif case=='merge-label-during-policy' and a[:1]==['api'] and a[1].endswith('/branches/main/protection'):
 (root/'manual-after-admission').write_text('owner changed route during policy read');print(json.dumps(dict(required_status_checks=dict(strict=True,contexts=['privacy']),enforce_admins=dict(enabled=True))))
elif case=='ready-pause-during-routing' and a[:2]==['pr','ready']:(root/'ready-writes').write_text('dispatched');print('ok')
elif a[:1]==['api'] and '/pulls/9/' in a[1]:
 if case=='post-rabbit-label':(root/'manual-after-admission').write_text('owner changed route during review read')
 if case=='post-rabbit-pause':
  # Late-control fault injection is restricted to the disposable fixture DB.
  c=sqlite3.connect(os.environ['AUGMENTAGENT_AUTOPR_WORKFLOW_DB']);c.execute("INSERT OR REPLACE INTO workflow_owner_control(workflow,revision,paused,principal,updated_at) VALUES('owner/repo/pr/9',1,1,'github:owner',1)");c.commit();c.close()
 print('[]')
elif a[:1]==['api'] and a[1].startswith('https://api.github.com/repos/') and '/git/ref/heads/' in a[1]:
 branch=urllib.parse.unquote(a[1].split('/git/ref/heads/',1)[1])
 remote=root.parent/('other.git' if '/owner/other/' in a[1] else 'remote.git')
 out=subprocess.run(['/usr/bin/git','ls-remote','--heads',str(remote),'refs/heads/'+branch],capture_output=True,text=True)
 (root/'push-read-output').write_text(json.dumps(dict(code=out.returncode,stdout=out.stdout,stderr=out.stderr)))
 if out.returncode:sys.exit(1)
 words=[] if case=='fresh-uncertain-access-denied' else out.stdout.split()
 if '--include' in a:print('HTTP/2.0 '+('200 OK' if len(words)==2 else '404 Not Found')+'\nContent-Type: application/json\n')
 if len(words)!=2:print('{"message":"Not Found"}');sys.exit(1)
 print(words[0])
elif case.startswith('create-manual-') and a[:2]==['issue','view']:print(json.dumps(dict(state='OPEN')))
elif case.startswith('create-manual-') and a[:1]==['api'] and '/git/ref/heads/' in a[1]:print(json.dumps(dict(object=dict(sha=(root/'actual-head').read_text()))))
elif case.startswith('create-manual-') and a[:2]==['pr','create']:(root/'create-writes').write_text('dispatched');print('https://github.com/owner/repo/pull/9')
elif a[:1]==['api'] and '/issues/' in a[1]:
 n=a[1].split('/issues/',1)[1]
 if case=='unknown-source' and n=='7':sys.exit(1)
 row=issues[n]
 if case.startswith('create-manual-') and (root/'manual-after-admission').exists():row['labels']=[dict(name='autoship-manual')]
 if case=='malformed-source' and n=='7':row.pop('labels')
 print(json.dumps(row))
elif a[:2] in (['pr','comment'],['issue','comment']) and (case.startswith('post-note-') or case.startswith('post-control-') or case.startswith('post-rabbit-') or case.startswith('round-')):
 with (root/'note-writes').open('a') as out:out.write(json.dumps(a)+'\n')
 (root/'manual-after-admission').write_text('owner changed routing during publication note');print('ok')
elif a[:2]==['pr','close'] and case=='source-reopened-during-close-routing':(root/'close-writes').write_text('closed');print('')
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
                    .env("AUGMENTAGENT_CCAT_ENABLED",if matches!(case,"push-missing-ccat"|"push-shadowed-ccat"|"push-valid-ccat") {"true"}else{"false"})
                    .env("AUGMENTAGENT_CCAT_PUBLIC_REMOTES","origin")
                    .env(CHILD,&root).env("MANUAL_PR_ROUTE",route).env("MANUAL_PR_CASE",case)
                    .current_dir(&root).output().await.unwrap();
                assert!(output.status.success(),"{route}/{case}/draft={draft}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));
            }
        }
    }
}
