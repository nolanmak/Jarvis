#[path = "autoship_work_command.rs"]
pub(crate) mod work_command;
#[path = "autoship_run_command.rs"]
pub(crate) mod run_command;
#[path = "autoship_merge_recovery.rs"]
pub(crate) mod merge_recovery;
// Shared command projections. Read paths never create state or launch work.
use crate::autoship_workflow::{OperationKind, Stage, WorkflowStore};
use anyhow::{ensure, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

fn target(workflow: &str) -> Result<(String, Value)> {
    ensure!(workflow.len() <= 512, "workflow identity exceeds bound");
    let (repository, kind, number) = if let Some((repo, number)) = workflow.split_once('#') {
        (repo, "issue", number)
    } else if let Some((repo, number)) = workflow.rsplit_once("/pr/") {
        (repo, "pr", number)
    } else {
        anyhow::bail!("expected OWNER/REPO#ISSUE or OWNER/REPO/pr/PR");
    };
    let components: Vec<_> = repository.split('/').collect();
    ensure!(
        components.len() == 2
            && components.iter().all(|part| !part.is_empty()
                && !matches!(*part, "." | "..")
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))),
        "invalid repository identity"
    );
    let number: u64 = number.parse()?;
    ensure!(number > 0 && number <= i64::MAX as u64, "target number out of range");
    Ok((repository.to_ascii_lowercase(), json!({"kind":kind,"number":number})))
}

fn error(workflow: &str, reason: &str, detail: &str) -> Value {
    json!({"schema_version":1,"workflow":workflow,"accepted":false,"complete":false,
        "reason":reason,"error":detail})
}

/// Common read model for CLI and owner-authorized channel adapters. Unknown
/// provider/deployment evidence stays unknown rather than inferred from main.
pub fn status(path: &Path, workflow: &str, now: i64, cap: u32) -> (i32, Value) {
    let (repository, target) = match target(workflow) {
        Ok(target) => target,
        Err(e) => return (2, error(workflow, "invalid-workflow", &e.to_string())),
    };
    let observed = (|| -> Result<Option<(i32, Value)>> {
        let Some(store) = WorkflowStore::open_read_only(path)? else {
            return Ok(None);
        };
        let canonical = if target["kind"] == "pr" {
            store.resolve_pr(&repository, target["number"].as_u64().unwrap())?
                .unwrap_or(crate::autoship_workflow::pr_workflow_id(&repository, target["number"].as_u64().unwrap())?)
        } else {
            crate::autoship_workflow::workflow_id(&repository, target["number"].as_u64().unwrap())?
        };
        let workflow = canonical.as_str();
        let command = store.command_request(workflow)?;
        let attempt = store.command_attempt(workflow)?;
        let Some(snapshot) = store.snapshot(workflow)? else {
            let Some(command) = command else { return Ok(None); };
            // Admission is durable before a worker claims the job. There is no
            // execution/head/worker-liveness evidence at this point.
            // A failed attempt is still retryable. Only authoritative workflow
            // state may turn it into a terminal workflow failure.
            let code=if attempt.is_some() {3} else {0};
            return Ok(Some((code, json!({"schema_version":1,"workflow":workflow,
                "repository":command.repository,"target":command.target,
                "accepted":true,"complete":false,"stage":"queued",
                "reason":attempt.as_ref().map_or("awaiting-worker-claim",|a|a.reason.as_str()),
                "next_action":if attempt.is_some() {"retry_worker_attempt"} else {"claim_workflow"},
                "eligible_at_ms":attempt.as_ref().map(|a|a.eligible_at_ms),"head":null,"last_attempt":attempt,
                "provider":command.provider,"workflow_until":command.until,"pr_url":null,
                "worker":{"state":"unknown"},"merge":{"state":"not-confirmed"},
                "deployment":{"state":"unknown"},"allowance":null,
                "pending_merge_operations":[],"pending_merge_evidence":[],
                "resources":null,"lease":{"active":false},"progress_at_ms":null}))));
        };
        let allowance = store.merge_allowance(now, cap)?;
        let mut pending = Vec::new();
        let mut pending_evidence = Vec::new();
        let recovery_needed = store.merge_reconciliation_needed(workflow)?;
        let mut operations: Vec<_> = store.pending_operations(workflow)?.into_iter()
            .filter(|op| op.kind == OperationKind::MergePr).collect();
        for record in store.pending_merge_dispatches(&repository)?.into_iter()
            .filter(|record| record.operation.workflow == workflow) {
            if !operations.iter().any(|op| op.id == record.operation.id) {
                operations.push(record.operation);
            }
        }
        for op in operations {
            if let Some(record) = store.merge_dispatch(&op.id)? {
                let digest = store
                    .merge_response(&op, &record.repository, record.pr)?
                    .map(|(raw, _)| format!("{:x}", Sha256::digest(raw.as_bytes())));
                pending_evidence.push(
                    json!({"schema_version":1,"operation":op.id,"repository":record.repository,
                    "pr":record.pr,"head":op.binding.head,"response_sha256":digest}),
                );
            }
            pending.push(op.id);
        }
        let code = if recovery_needed || matches!(snapshot.checkpoint.stage, Stage::Held | Stage::AwaitChecks) {
            3
        } else {
            0
        };
        let complete = snapshot.checkpoint.stage == Stage::Merged && !recovery_needed;
        let pr_url = snapshot
            .pr
            .map(|pr| format!("https://github.com/{repository}/pull/{pr}"));
        Ok(Some((
            code,
            json!({"schema_version":1,"workflow":workflow,"repository":repository,"target":target,
            "accepted":true,"complete":complete,"head":snapshot.checkpoint.binding.head,
            "stage":snapshot.checkpoint.stage,"reason":snapshot.checkpoint.next.reason,
            "next_action":snapshot.checkpoint.next.action,"eligible_at_ms":snapshot.checkpoint.next.eligible_at_ms,
            "provider":command.as_ref().and_then(|c| c.provider),"workflow_until":command.as_ref().map(|c| c.until),"pr_url":pr_url,
            "merge":{"state":if complete {"observed-merged"} else if recovery_needed || !pending.is_empty() {"pending"} else {"not-confirmed"}},
            "deployment":{"state":"unknown"},"allowance":allowance,"pending_merge_operations":pending,"pending_merge_evidence":pending_evidence,
            "resources":{"attempts":snapshot.attempts,"model_calls":snapshot.model_calls,"tokens":snapshot.tokens,"wall_ms":snapshot.wall_ms},
            "lease":{"active":snapshot.lease_until_ms > now,"generation":snapshot.generation},
            "progress_at_ms":snapshot.progress_at_ms,"last_attempt":attempt}),
        )))
    })();
    match observed {
        Ok(Some(result)) => result,
        Ok(None) => (
            2,
            error(
                workflow,
                "workflow-not-found",
                "No matching durable workflow exists",
            ),
        ),
        Err(e) => (
            4,
            error(workflow, "workflow-state-unavailable", &format!("{e:#}")),
        ),
    }
}

pub fn run_status(workflow: &str, json_output: bool) -> i32 {
    let cap = std::env::var("AUGMENTAGENT_AUTOPR_DAILY_CAP")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .unwrap_or(5)
        .max(1);
    let (code, body) = status(
        &crate::autoship_workflow::default_path(),
        workflow,
        chrono::Utc::now().timestamp_millis(),
        cap,
    );
    if json_output {
        println!("{}", body);
    } else if let Some(error) = body["error"].as_str() {
        println!(
            "{workflow}: {} — {error}",
            body["reason"].as_str().unwrap_or("unknown")
        );
    } else {
        println!(
            "{workflow}: {} — {}",
            body["stage"].as_str().unwrap_or("unknown"),
            body["reason"].as_str().unwrap_or("unknown")
        );
        println!(
            "Next: {} (eligible at {} ms UTC)",
            body["next_action"].as_str().unwrap_or("unknown"),
            body["eligible_at_ms"]
        );
        if body["allowance"].is_object() {
            println!("Confirmed auto-merges today: {}; remaining: {}; pending capacity holds: {}; available: {}",
                body["allowance"]["confirmed"],body["allowance"]["remaining"],body["allowance"]["pending"],body["allowance"]["available"]);
        } else {
            println!("Merge allowance: unobserved; worker status: unknown");
        }
        if let Some(url) = body["pr_url"].as_str() {
            println!("PR: {url}");
        }
        for op in body["pending_merge_operations"]
            .as_array()
            .into_iter()
            .flatten()
        {
            println!(
                "Pending merge operation: {}",
                op.as_str().unwrap_or("unknown")
            );
        }
        println!("Deployment: unknown");
    }
    code
}

/// Credential declaration evidence is deliberately separate from role/runtime
/// readiness. The latter requires controller-bound acceptance evidence.
pub async fn run_readiness(json_output:bool) -> i32 {
    let claude=augmentagent_channel_core::native_vm::readiness::credentials("claude");
    let codex=augmentagent_channel_core::native_vm::readiness::credentials("codex");
    let runtime=augmentagent_channel_core::native_vm::readiness::runtime::inspect();
    let deployment=crate::autoship_deployment::inspect(&std::env::current_dir().unwrap_or_default()).await;
    let mut unverified=vec!["live-guarded-probe"];
    if !deployment.matches { unverified.push("deployed-policy-equality"); }
    let body=json!({"schema_version":1,"ready":false,"reason":"workflow-readiness-unverified","runtime":runtime,
        "credentials":{"claude":claude,"codex":codex},
        "roles":crate::self_improve::provider_readiness::inspect(),
        "resources":augmentagent_channel_core::run_resources::readiness().await,
        "deployment":deployment,
        "unverified":unverified});
    if json_output {println!("{body}");} else {
        println!("Autoship readiness is unverified.");
        for provider in ["claude","codex"] {
            println!("{provider}: {}",body["credentials"][provider]["status"].as_str().unwrap_or("unknown"));
        }
        println!("Native guest: {}; build runtime: {}",body["runtime"]["status"],body["runtime"]["build_status"]);
        for role in ["builder","independent_reviewer","adjudicator"] {
            println!("{role}: {}",body["roles"][role]["status"]);
        }
        println!("Resources: {}",body["resources"]["status"]);
        println!("Deployment: {}",body["deployment"]["status"]);
        println!("Live-probe evidence remains unverified.");
    }
    3
}
