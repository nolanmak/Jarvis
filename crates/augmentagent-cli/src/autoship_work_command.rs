//! One local execution attempt of an already admitted job. No independent
//! build/review/merge implementation lives in this adapter.
use crate::autoship_workflow::{submission::Request, WorkflowStore};
use serde_json::{json, Value};
use std::path::Path;

#[derive(clap::Args)]
pub(crate) struct WorkArgs {
    #[arg(long, required = true)]
    once: bool,
    #[arg(long)]
    workflow: String,
    #[arg(long)]
    json: bool,
}
fn failure(
    id: &str,
    code: i32,
    accepted: bool,
    reason: &str,
    error: impl std::fmt::Display,
) -> (i32, Value) {
    (
        code,
        json!({"schema_version":1,"workflow":id,"accepted":accepted,"complete":false,"reason":reason,"error":error.to_string()}),
    )
}
async fn work(repo: &Path, path: &Path, id: &str) -> (i32, Value) {
    let (repository, target) = match super::target(id) {
        Ok(t) => t,
        Err(e) => return failure(id, 2, false, "invalid-workflow", e),
    };
    let store = match WorkflowStore::open_read_only(path) {
        Ok(Some(s)) => s,
        Ok(None) => return failure(id, 2, false, "workflow-not-found", "No durable job exists"),
        Err(e) => return failure(id, 4, false, "workflow-state-unavailable", e),
    };
    let number = target["number"].as_u64().unwrap();
    let canonical = (|| -> anyhow::Result<String> {
        if target["kind"] == "pr" {
            Ok(store.resolve_pr(&repository, number)?.unwrap_or(
                crate::autoship_workflow::pr_workflow_id(&repository, number)?,
            ))
        } else {
            crate::autoship_workflow::workflow_id(&repository, number)
        }
    })();
    let id = match canonical {
        Ok(id) => id,
        Err(e) => return failure(id, 4, false, "workflow-state-unavailable", e),
    };
    let command: Option<Request> = match store.command_request(&id) {
        Ok(c) => c,
        Err(e) => return failure(&id, 4, false, "workflow-state-unavailable", e),
    };
    if command.is_none() {
        return failure(
            &id,
            2,
            false,
            "workflow-not-found",
            "No durable command job exists",
        );
    }
    drop(store);
    let report = match crate::self_improve::targeted::work_once(repo, &id).await {
        Ok(report) => report,
        Err(e) => {
            let (code, reason) = e
                .downcast_ref::<crate::self_improve::targeted::CommandFailure>()
                .map(|e| (e.code, e.reason))
                .unwrap_or((4, "workflow-execution-failed"));
            return failure(&id, code, code != 2, reason, format!("{e:#}"));
        }
    };
    let cap = std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(5)
        .max(1);
    let (status_code, mut status) =
        super::status(path, &id, chrono::Utc::now().timestamp_millis(), cap);
    status["attempt"] =
        json!({"held":report.is_idle(),"builder_ran":report.billed,"message":report.message});
    (
        if status_code == 0 && report.is_idle() && status["complete"] != true {
            3
        } else {
            status_code
        },
        status,
    )
}
pub(crate) async fn run(args: &WorkArgs) -> i32 {
    let (code, value) = match std::env::current_dir() {
        Ok(repo) => {
            work(
                &repo,
                &crate::autoship_workflow::default_path(),
                &args.workflow,
            )
            .await
        }
        Err(e) => failure(&args.workflow, 4, false, "checkout-unavailable", e),
    };
    if args.json {
        println!("{value}");
    } else {
        println!(
            "{}: {}",
            args.workflow,
            value["reason"].as_str().unwrap_or("unknown")
        );
        if let Some(error) = value["error"].as_str() {
            println!("{error}");
        }
        if let Some(message) = value["attempt"]["message"].as_str() {
            println!("{message}");
        }
    }
    code
}
