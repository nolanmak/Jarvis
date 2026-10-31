//! Authenticated adapters for owner dispatch controls. No checkpoint or hold is
//! rewritten, and an accepted pause is never presented as completed teardown.
use crate::autoship_workflow::WorkflowStore;
use serde_json::{json,Value};
use std::path::Path;

#[derive(clap::Args)]
pub(crate) struct Args {
    #[arg(long)] workflow:String,
    #[arg(long)] json:bool,
}
fn failure(id:&str,code:i32,reason:&str,error:impl std::fmt::Display)->(i32,Value) {
    (code,json!({"schema_version":1,"workflow":id,"accepted":false,"complete":false,"reason":reason,"error":error.to_string()}))
}

pub(crate) async fn apply(repo:&Path,path:&Path,id:&str,paused:bool)->(i32,Value) {
    let (repository,target)=match super::target(id) {Ok(value)=>value,Err(e)=>return failure(id,2,"invalid-workflow",e)};
    let store=match WorkflowStore::open_read_only(path) {
        Ok(Some(store))=>store,
        Ok(None)=>return failure(id,2,"workflow-not-found","no durable workflow state"),
        Err(e)=>return failure(id,4,"workflow-state-unavailable",e),
    };
    let number=target["number"].as_u64().unwrap();
    let canonical=(||->anyhow::Result<String> {
        if target["kind"]=="pr" {Ok(store.resolve_pr(&repository,number)?.unwrap_or(crate::autoship_workflow::pr_workflow_id(&repository,number)?))}
        else {crate::autoship_workflow::workflow_id(&repository,number)}
    })();
    let id=match canonical {Ok(id)=>id,Err(e)=>return failure(id,4,"workflow-state-unavailable",e)};
    let request=match store.command_request(&id) {
        Ok(Some(request))=>request,
        Ok(None)=>return failure(&id,2,"workflow-not-found","no durable command job"),
        Err(e)=>return failure(&id,4,"workflow-state-unavailable",e),
    };
    let revision=match store.owner_control(&id) {Ok(control)=>control.map_or(0,|c|c.revision),Err(e)=>return failure(&id,4,"workflow-state-unavailable",e)};
    drop(store);
    let principal=match crate::self_improve::targeted::authorize_command(repo,&request).await {
        Ok(actor)=>actor,Err(e)=>return failure(&id,2,"target-authorization-failed",e),
    };
    if !paused {
        if let Err(e)=crate::self_improve::targeted::command_readiness(repo,&request).await {
            return failure(&id,3,"execution-readiness-blocked",format!("{e:#}"));
        }
    }
    let changed=(||->anyhow::Result<_> {
        WorkflowStore::open(path)?.set_owner_pause(&id,&principal,revision,paused,chrono::Utc::now().timestamp_millis())
    })();
    match changed {
        Ok(control)=>(0,json!({"schema_version":1,"workflow":id,"accepted":true,"complete":false,
            "reason":if paused {"owner-pause-requested"} else {"owner-resume-accepted"},
            "owner_control":control,"worker_enforcement":"unverified","quiescence":"unverified","execution":"not-confirmed"})),
        Err(e) if matches!(e.to_string().as_str(),"owner-followup-pending"|"owner-control-revision-conflict"|"terminal workflow cannot change owner control"|"unknown workflow")=>failure(&id,3,"owner-control-rejected",e),
        Err(e)=>failure(&id,4,"workflow-state-unavailable",e),
    }
}

pub(crate) async fn run(args:&Args,paused:bool)->i32 {
    let (code,value)=match std::env::current_dir() {
        Ok(repo)=>apply(&repo,&crate::autoship_workflow::default_path(),&args.workflow,paused).await,
        Err(e)=>failure(&args.workflow,4,"checkout-unavailable",e),
    };
    if args.json {println!("{value}");} else {
        println!("{}: {}",args.workflow,value["reason"].as_str().unwrap_or("unknown"));
        if value["accepted"]==true {println!("Control recorded. Live-worker enforcement, active-work teardown and workflow completion are not confirmed.");}
        if let Some(error)=value["error"].as_str() {println!("{error}");}
    }
    code
}
