//! Authenticated, model-free maintenance for targets completed outside autoship.
use crate::autoship_workflow::{submission::Target,WorkflowStore};
use serde_json::{json,Value};
use std::path::Path;

#[derive(clap::Args)]
pub(crate) struct Args {
    #[arg(long)] workflow:String,
    #[arg(long)] json:bool,
}

async fn apply(repo:&Path,path:&Path,id:&str)->anyhow::Result<Value> {
    let (repository,target)=super::target(id)?;
    let number=target["number"].as_u64().unwrap();
    let store=WorkflowStore::open_read_only(path)?.ok_or_else(||anyhow::anyhow!("workflow store unavailable"))?;
    let (canonical,target)=if target["kind"]=="pr" {
        (store.resolve_pr(&repository,number)?.unwrap_or(crate::autoship_workflow::pr_workflow_id(&repository,number)?),Target::Pr(number))
    } else {(crate::autoship_workflow::workflow_id(&repository,number)?,Target::Issue(number))};
    anyhow::ensure!(super::target(&canonical)?.0==repository,"workflow alias repository differs");
    drop(store);
    crate::self_improve::targeted::reconcile_terminal_target(repo,path,&canonical,&repository,target).await?;
    let cap=std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP").ok().and_then(|value|value.parse::<u32>().ok()).unwrap_or(5).max(1);
    let (_,mut status)=super::status(path,&canonical,chrono::Utc::now().timestamp_millis(),cap);
    status["reconciled"]=true.into();
    status["execution"]="not-dispatched".into();
    status["native_retirement"]="queued-or-existing-unverified".into();
    Ok(status)
}

pub(crate) async fn run(args:&Args)->i32 {
    let result=match std::env::current_dir() {
        Ok(repo)=>apply(&repo,&crate::autoship_workflow::default_path(),&args.workflow).await,
        Err(error)=>Err(error.into()),
    };
    let (code,value)=match result {
        Ok(value)=>(0,value),
        Err(error)=>{
            let (code,reason)=error.downcast_ref::<crate::self_improve::targeted::CommandFailure>()
                .map(|failure|(failure.code,failure.reason)).unwrap_or((4,"workflow-state-unavailable"));
            (code,json!({"schema_version":1,"workflow":args.workflow,"reconciled":false,"reason":reason,"error":format!("{error:#}")}))
        },
    };
    if args.json {println!("{value}");} else {println!("{}",serde_json::to_string_pretty(&value).unwrap());}
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::{Binding,NextAction,OperationKind,ResourceCharge,Stage,
        submission::{Request,Milestone}};
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn manual_target_reconciliation_uses_authenticated_production_adapter() {
        const CHILD:&str="AUTOSHIP_TERMINAL_FIXTURE";
        if let Some(root)=std::env::var_os(CHILD).map(std::path::PathBuf::from) {
            for command in [false,true] {
                for case in ["closed","merged","open","leased","unknown","foreign","denied"] {
                    let path=root.join(format!("{command}-{case}.db"));
                    let mut store=WorkflowStore::open(&path).unwrap();
                    let now=chrono::Utc::now().timestamp_millis();
                    let id=if command {
                        store.submit_authorized("request","github:owner",&Request {repository:"owner/repo".into(),
                            target:Target::Issue(7),provider:None,until:Milestone::Merged},now).unwrap().workflow
                    } else {crate::autoship_workflow::pr_workflow_id("owner/repo",42).unwrap()};
                    let binding=Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
                    store.register(&id,"fixture",binding.clone(),now).unwrap();
                    let lease=store.claim(&id,"original",now,60_000).unwrap().unwrap();
                    store.attach_pr_in_repo(&lease,"owner/repo",42,now).unwrap();
                    store.charge(&lease,"prior-paid",ResourceCharge {model_calls:3,tokens:700,wall_ms:5},now).unwrap();
                    store.register_native_owner(&lease,root.join("original-audit").to_str().unwrap(),now).unwrap();
                    if case=="unknown" {store.begin_operation(&lease,OperationKind::Push,&binding,now).unwrap();}
                    let mut cp=store.snapshot(&id).unwrap().unwrap().checkpoint;
                    cp.stage=Stage::Held;cp.next=NextAction {reason:"resume_deferred".into(),action:"resume_preserved_work".into(),eligible_at_ms:0};
                    store.checkpoint(&lease,&cp,now).unwrap();
                    if case!="leased" {store.release(&lease,now).unwrap();}
                    let pause=store.set_owner_pause(&id,"github:owner",0,true,now).unwrap();
                    let before=store.snapshot(&id).unwrap().unwrap();let events=store.events(&id).unwrap();
                    let state=if case=="merged" {"MERGED"} else if case=="open" {"OPEN"} else {"CLOSED"};
                    std::fs::write(root.join("scenario.json"),json!({"permission":if case=="denied" {"READ"}else{"ADMIN"},
                        "pr":{"number":42,"state":state,"url":if case=="foreign" {"https://github.com/foreign/repo/pull/42"}else{"https://github.com/owner/repo/pull/42"},
                            "mergeCommit":{"oid":"c".repeat(40)}}}).to_string()).unwrap();
                    let result=apply(&root,&path,"owner/repo/pr/42").await;
                    if matches!(case,"closed"|"merged") {
                        let result=result.unwrap();assert_eq!(result["reconciled"],true);
                        if case=="closed" {
                            assert_eq!(result["reason"],"target_closed","CLI status hid terminal retirement behind owner pause");
                            assert_eq!(result["next_action"],"complete");
                        }
                        let after=store.snapshot(&id).unwrap().unwrap();
                        assert_eq!(after.checkpoint.stage,if case=="closed" {Stage::Retired}else{Stage::Merged});
                        assert_eq!((after.model_calls,after.tokens),(3,700));
                        assert_eq!(store.owner_control(&id).unwrap(),Some(pause));
                        assert_eq!(store.command_request(&id).unwrap().is_some(),command);
                        let progress=store.progress_events(&id).unwrap();
                        let terminal=progress.last().unwrap();
                        assert_eq!(terminal.kind,if case=="closed" {"retired"}else{"merged"},
                            "preserved owner pause obscured actual terminal completion");
                        assert_eq!(terminal.reason,if case=="closed" {"target_closed"}else{"external-merge-observed"});
                        let terminal_events=store.events(&id).unwrap();
                        apply(&root,&path,"owner/repo/pr/42").await.unwrap();
                        assert_eq!(store.snapshot(&id).unwrap().unwrap(),after);
                        assert_eq!(store.events(&id).unwrap(),terminal_events);
                        let retirements=store.pending_native_retirements(chrono::Utc::now().timestamp_millis(),16).unwrap();
                        assert_eq!(retirements.len(),1);assert_eq!(retirements[0].0,id);
                    } else {
                        assert!(result.is_err(),"{command}/{case} unexpectedly reconciled");
                        assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);
                        assert_eq!(store.events(&id).unwrap(),events);
                        if case=="unknown" {assert_eq!(store.pending_operations(&id).unwrap().len(),1);}
                    }
                    let allowance=store.merge_allowance(now,5).unwrap();
                    assert_eq!((allowance.confirmed,allowance.available),(0,5));
                }
            }
            // The public pause adapter also supports a known, never-started PR
            // adoption without creating a command job or native workflow.
            std::fs::write(root.join("scenario.json"),json!({"permission":"ADMIN"}).to_string()).unwrap();
            let path=root.join("not-started.db");let mut store=WorkflowStore::open(&path).unwrap();
            store.observe_adoption("owner/repo",99,"head","{}",1).unwrap();
            let id="owner/repo/pr/99";
            let (code,reply)=crate::autoship_commands::control_command::apply(&root,&path,id,true).await;
            assert_eq!(code,0,"{reply}");assert!(store.owner_control(id).unwrap().unwrap().paused);
            assert!(store.snapshot(id).unwrap().is_none() && store.command_request(id).unwrap().is_none());
            return;
        }
        let dir=tempfile::tempdir().unwrap();let root=dir.path();let gh=root.join("gh.py");
        std::fs::write(&gh,r#"#!/usr/bin/env python3
import json,os,pathlib,sys
root=pathlib.Path(os.environ['AUTOSHIP_TERMINAL_FIXTURE']);s=json.loads((root/'scenario.json').read_text());a=sys.argv[1:]
if a[:2]==['repo','view']:print(s['permission'] if 'viewerPermission' in a else 'owner/repo')
elif a[:2]==['api','graphql']:print(json.dumps({'data':{'viewer':{'login':'owner'}}}))
elif a[:2]==['pr','view']:
 assert a[2]=='42' and a[a.index('--repo')+1]=='owner/repo';print(json.dumps(s['pr']))
else:raise Exception('unexpected GitHub call '+repr(a))
"#).unwrap();
        std::fs::set_permissions(&gh,std::fs::Permissions::from_mode(0o700)).unwrap();
        let result=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","autoship_commands::target_reconcile::tests::manual_target_reconciliation_uses_authenticated_production_adapter","--nocapture"])
            .env(CHILD,root).env("GH_BIN",gh).env("XDG_STATE_HOME",root.join("state"))
            .env("AUGMENTAGENT_NATIVE_VM_POLICY",root.join("must-not-be-opened-policy"))
            .output().unwrap();
        assert!(result.status.success(),"{}\n{}",String::from_utf8_lossy(&result.stdout),String::from_utf8_lossy(&result.stderr));
    }
}
