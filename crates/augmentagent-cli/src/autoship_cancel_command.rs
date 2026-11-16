//! Owner cancel: authenticated, model-free durable retirement. Exit 6 means the
//! workflow is canceled; active work is refused, never torn down by this path.
use serde_json::{json,Value};
use std::path::Path;

#[derive(clap::Args)]
pub(crate) struct Args {
    #[arg(long)] workflow:String,
    #[arg(long)] json:bool,
}

pub(crate) const CANCELED_EXIT:i32=6;

pub(crate) async fn apply(repo:&Path,path:&Path,id:&str)->anyhow::Result<Value> {
    let (repository,target)=super::target(id)?;
    let number=target["number"].as_u64().unwrap();
    let canonical=match crate::autoship_workflow::WorkflowStore::open_read_only(path)? {
        Some(store) if target["kind"]=="pr"=>store.resolve_pr(&repository,number)?
            .unwrap_or(crate::autoship_workflow::pr_workflow_id(&repository,number)?),
        Some(_)=>crate::autoship_workflow::workflow_id(&repository,number)?,
        None=>return Err(anyhow::anyhow!("no durable workflow state")
            .context(crate::self_improve::targeted::CommandFailure {code:2,reason:"workflow-not-found"})),
    };
    anyhow::ensure!(super::target(&canonical)?.0==repository,"workflow alias repository differs");
    crate::self_improve::targeted::cancel_workflow(repo,path,&canonical,&repository).await?;
    let cap=std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP").ok().and_then(|v|v.parse::<u32>().ok()).unwrap_or(5).max(1);
    let (_,mut status)=super::status(path,&canonical,chrono::Utc::now().timestamp_millis(),cap);
    status["canceled"]=true.into();
    status["accepted"]=true.into();
    status["native_retirement"]="queued-or-existing-unverified".into();
    Ok(status)
}

pub(crate) async fn run(args:&Args)->i32 {
    let result=match std::env::current_dir() {
        Ok(repo)=>apply(&repo,&crate::autoship_workflow::default_path(),&args.workflow).await,
        Err(error)=>Err(error.into()),
    };
    let (code,value)=match result {
        Ok(value)=>(CANCELED_EXIT,value),
        Err(error)=>{
            let (code,reason)=error.downcast_ref::<crate::self_improve::targeted::CommandFailure>()
                .map(|failure|(failure.code,failure.reason)).unwrap_or((4,"workflow-state-unavailable"));
            (code,json!({"schema_version":1,"workflow":args.workflow,"accepted":false,"complete":false,
                "canceled":false,"reason":reason,"error":format!("{error:#}")}))
        },
    };
    if args.json {println!("{value}");} else {
        println!("{}: {}",args.workflow,value["reason"].as_str().unwrap_or("unknown"));
        if let Some(error)=value["error"].as_str() {println!("{error}");}
    }
    code
}
