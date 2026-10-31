//! Public submission adapter. Execution is owned by the shared durable job,
//! never by the submitting client's lifetime.
use crate::autoship_workflow::{
    submission::{Admission, Milestone, Provider, Request, Target},
    WorkflowStore,
};
use serde_json::{json, Value};
use std::path::Path;

#[derive(clap::Args)]
pub(crate) struct RunArgs {
    #[arg(long)]
    repo: String,
    #[arg(long, required_unless_present = "pr", conflicts_with = "pr")]
    issue: Option<u64>,
    #[arg(long, required_unless_present = "issue", conflicts_with = "issue")]
    pr: Option<u64>,
    #[arg(long, value_parser=["codex", "claude"])]
    provider: Option<String>,
    #[arg(long, default_value="merged", value_parser=["pr", "merged"])]
    until: String,
    #[arg(long)]
    request_id: Option<String>,
    #[arg(long, conflicts_with="wait")]
    dry_run: bool,
    #[arg(long)]
    wait: bool,
    #[arg(long, requires="wait", value_parser=clap::value_parser!(u64).range(0..=604800))]
    timeout_secs: Option<u64>,
    #[arg(long)]
    json: bool,
}

fn failure(code: i32, reason: &str, error: impl std::fmt::Display) -> (i32, Value) {
    (
        code,
        json!({"schema_version":1,"accepted":false,"complete":false,"reason":reason,"error":error.to_string()}),
    )
}
fn response(id: &str, request: &Request, admission: Admission) -> (i32, Value) {
    let code = if admission.accepted { 0 } else { 2 };
    let mut value = serde_json::to_value(&admission).expect("serializable admission");
    value["request_id"] = id.into();
    value["repository"] = request.repository.clone().into();
    value["target"] = serde_json::to_value(request.target).unwrap();
    value["stage"] = if admission.reason == "queued" {
        "queued".into()
    } else {
        Value::Null
    };
    value["head"] = Value::Null;
    value["pr_url"] = Value::Null;
    value["next_action"] = "inspect_workflow".into();
    value["eligible_at_ms"] = Value::Null;
    value["merge"] = json!({"state":"unknown"});
    value["deployment"] = json!({"state":"unknown"});
    value["worker"] = json!({"state":"unknown"});
    value["allowance"] = Value::Null;
    (code, value)
}
async fn submit(repo: &Path, path: &Path, args: &RunArgs) -> (i32, Value) {
    let request = Request {
        repository: args.repo.clone(),
        target: match (args.issue, args.pr) {
            (Some(n), None) => Target::Issue(n),
            (None, Some(n)) => Target::Pr(n),
            _ => return failure(2, "invalid-target", "exactly one issue or PR is required"),
        },
        provider: args.provider.as_deref().map(|p| {
            if p == "codex" {
                Provider::Codex
            } else {
                Provider::Claude
            }
        }),
        until: if args.until == "pr" {
            Milestone::Pr
        } else {
            Milestone::Merged
        },
    };
    let request = match request.normalized() {
        Ok(r) => r,
        Err(e) => return failure(2, "invalid-target", e),
    };
    if let Some(id) = args.request_id.as_deref() {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        {
            return failure(
                2,
                "invalid-request-id",
                "request ID must be 1..128 safe ASCII characters",
            );
        }
    }
    let actor = match crate::self_improve::targeted::authorize_command(repo, &request).await {
        Ok(actor) => actor,
        Err(e) => return failure(2, "target-authorization-failed", e),
    };
    // Dry runs inspect current eligibility and never return a cached acceptance.
    if !args.dry_run {
        if let Some(id) = args.request_id.as_deref() {
            let store = match WorkflowStore::open_read_only(path) {
                Ok(s) => s,
                Err(e) => return failure(4, "workflow-state-unavailable", e),
            };
            if let Some(store) = store {
                match store.replay_authorized(id, &actor, &request) {
                    Ok(Some(admission)) => return response(id, &request, admission),
                    Ok(None) => {}
                    Err(e) => {
                        return failure(
                            if e.to_string() == "request-id-conflict" {
                                2
                            } else {
                                4
                            },
                            "submission-replay-failed",
                            e,
                        )
                    }
                }
            }
        }
    }
    if let Err(e) = crate::self_improve::targeted::validate_command(repo, &request).await {
        return failure(2, "invalid-target", e);
    }
    if args.dry_run {
        return (
            0,
            json!({"schema_version":1,"accepted":false,"complete":false,"dry_run":true,
            "repository":request.repository,"target":request.target,"provider":request.provider,
            "requested_until":request.until,"reason":"target-validated","execution_readiness":"unverified"}),
        );
    }
    let mut store = match WorkflowStore::open(path) {
        Ok(s) => s,
        Err(e) => return failure(4, "workflow-state-unavailable", e),
    };
    let now = chrono::Utc::now().timestamp_millis();
    let id = match args.request_id.clone() {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            if let Err(e) = store.persist_client_intent(&id, &actor, &request, now) {
                return failure(4, "client-intent-not-persisted", e);
            }
            id
        }
    };
    match store.submit_authorized(&id, &actor, &request, now) {
        Ok(admission) => response(&id, &request, admission),
        Err(e) => failure(
            if e.to_string() == "request-id-conflict" {
                2
            } else {
                4
            },
            "submission-failed",
            e,
        ),
    }
}

pub(crate) async fn run(args: &RunArgs) -> i32 {
    let repo=std::env::current_dir();
    let (mut code, mut body) = match &repo {
        Ok(repo) => submit(repo, &crate::autoship_workflow::default_path(), args).await,
        Err(e) => failure(4, "checkout-unavailable", e),
    };
    if args.wait && code==0 && body["accepted"]==true {
        (code,body)=super::wait::observe(repo.as_deref().expect("accepted submission has checkout"),&crate::autoship_workflow::default_path(),body,
            if args.until=="pr" {Milestone::Pr} else {Milestone::Merged},
            std::time::Duration::from_secs(args.timeout_secs.unwrap_or(3600))).await;
    }
    if args.json {
        println!("{body}");
    } else {
        println!(
            "{}: {}",
            body["workflow"].as_str().unwrap_or("autoship"),
            body["reason"].as_str().unwrap_or("unknown")
        );
        if let Some(id) = body["request_id"].as_str() {
            println!("Request ID: {id}");
        }
        if let Some(e) = body["error"].as_str() {
            println!("{e}");
        }
        if body["complete"] == true {
            println!("Requested milestone complete.");
        } else if body["accepted"] == true {
            println!(
                "Accepted; execution is incomplete. Inspect with autopr status --workflow {}",
                body["workflow"].as_str().unwrap_or("unknown")
            );
        }
    }
    code
}
