//! Explicit owner recovery. Evidence manifests select existing controller proof;
//! they cannot import a made-up mutation response or infer authorship from login.
use crate::autoship_workflow::{merge_request_not_executed, MergeReleaseProof, WorkflowStore};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    schema_version: u32,
    operation: String,
    repository: String,
    pr: u64,
    head: String,
    #[serde(default)]
    response_sha256: Option<String>,
    #[serde(default)]
    remote_head: Option<String>,
    #[serde(default)]
    merge_commit: Option<String>,
    #[serde(default)]
    merged_at: Option<String>,
}
fn evidence(path: &Path) -> Result<Evidence> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.nlink() == 1
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "evidence must be an owner-private regular file (mode 600), not a link"
    );
    let mut raw = Vec::new();
    file.take(64 * 1024 + 1).read_to_end(&mut raw)?;
    ensure!(raw.len() <= 64 * 1024, "evidence manifest exceeds bound");
    let evidence: Evidence = serde_json::from_slice(&raw)?;
    ensure!(
        evidence.schema_version == 1,
        "unsupported evidence manifest version"
    );
    Ok(evidence)
}
fn failure(
    operation: &str,
    code: i32,
    reason: &str,
    error: impl std::fmt::Display,
) -> (i32, Value) {
    (
        code,
        json!({"schema_version":1,"operation":operation,"resolved":false,"reason":reason,"error":error.to_string()}),
    )
}
async fn github(endpoint: &str) -> Result<Value> {
    let bin = std::env::var_os("GH_BIN").unwrap_or_else(|| "gh".into());
    let mut command = tokio::process::Command::new(bin);
    command
        .args(["api", endpoint])
        .env("GH_PROMPT_DISABLED", "true")
        .env("GIT_TERMINAL_PROMPT", "0")
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .context("GitHub recovery read timed out")??;
    ensure!(
        output.status.success(),
        "GitHub recovery read failed: {}",
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(1024)
            .collect::<String>()
    );
    serde_json::from_slice(&output.stdout).context("invalid GitHub recovery response")
}

pub async fn resolve(
    path: &Path,
    operation: &str,
    classification: &str,
    evidence_path: &Path,
) -> (i32, Value) {
    if !matches!(classification, "autonomous" | "manual" | "not-merged")
        || operation.is_empty()
        || operation.len() > 256
    {
        return failure(
            operation,
            2,
            "invalid-resolution",
            "unsupported classification or operation",
        );
    }
    let manifest = match evidence(evidence_path) {
        Ok(value) => value,
        Err(error) => return failure(operation, 2, "invalid-evidence", error),
    };
    let store = match WorkflowStore::open_read_only(path) {
        Ok(Some(store)) => store,
        Ok(None) => {
            return failure(
                operation,
                2,
                "operation-not-found",
                "workflow store does not exist",
            )
        }
        Err(error) => return failure(operation, 4, "workflow-state-unavailable", error),
    };
    let record = match store.merge_dispatch(operation) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return failure(
                operation,
                2,
                "operation-not-found",
                "no matching merge dispatch",
            )
        }
        Err(error) => return failure(operation, 4, "workflow-state-unavailable", error),
    };
    if manifest.operation != operation
        || manifest.repository != record.repository
        || manifest.pr != record.pr
        || manifest.head != record.operation.binding.head
        || super::target(&format!("{}#{}", record.repository, record.pr)).is_err()
    {
        return failure(
            operation,
            2,
            "evidence-identity-mismatch",
            "manifest must identify the exact reserved repository, PR and head",
        );
    }
    let remote = (async {
        let user = github("user").await?;
        let actor = user["login"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("authenticated operator identity missing")?
            .to_owned();
        let repository = github(&format!("repos/{}", record.repository)).await?;
        ensure!(
            repository["full_name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(&record.repository)),
            "GitHub repository identity differs"
        );
        if repository["permissions"]["admin"] != true {
            return Ok((actor, None));
        }
        let pr = github(&format!("repos/{}/pulls/{}", record.repository, record.pr)).await?;
        Ok::<_, anyhow::Error>((actor, Some(pr)))
    })
    .await;
    let (actor, remote) = match remote {
        Ok((_, None)) => {
            return failure(
                operation,
                2,
                "operator-not-authorized",
                "repository administrator authority is required",
            )
        }
        Ok((actor, Some(remote))) => (actor, remote),
        Err(error) => return failure(operation, 3, "merge-reconciliation-required", error),
    };
    if remote["number"].as_u64() != Some(record.pr)
        || remote["node_id"].as_str() != Some(record.node.as_str())
        || !remote["base"]["repo"]["full_name"]
            .as_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(&record.repository))
    {
        return failure(
            operation,
            3,
            "remote-identity-mismatch",
            "GitHub observation does not match the reserved immutable PR",
        );
    }
    let journal = match store.merge_response(&record.operation, &record.repository, record.pr) {
        Ok(value) => value,
        Err(error) => return failure(operation, 4, "workflow-state-unavailable", error),
    };
    // Validate all proof against read-only state and current GitHub evidence
    // before opening a writer, even when the operator is authorized.
    let validated = (|| -> Result<(Option<Value>, Option<MergeReleaseProof>)> {
        if classification == "autonomous" {
            let (raw, node) = journal
                .as_ref()
                .context("no controller-recorded response; mergedBy alone is not attribution")?;
            let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
            ensure!(
                manifest.response_sha256.as_deref() == Some(digest.as_str()),
                "response digest differs from controller journal"
            );
            let receipt = crate::self_improve::accepted_merge_response(
                raw,
                node,
                record.pr,
                &record.operation.binding.head,
                operation,
            )?;
            store.validate_merge_receipt_marker(operation, &receipt)?;
            ensure!(
                remote["merged"] == true
                    && remote["head"]["sha"] == receipt["expected_head"]
                    && remote["merge_commit_sha"] == receipt["merge_commit"]
                    && remote["merged_at"] == receipt["merged_at"]
                    && remote["merged_by"]["login"] == receipt["merged_by"],
                "remote merge contradicts accepted controller response"
            );
            return Ok((Some(receipt), None));
        }
        if record.state == "released" {
            let decision: Value = serde_json::from_str(
                record
                    .resolution
                    .as_deref()
                    .context("released reservation lacks resolution")?,
            )?;
            ensure!(
                decision["classification"] == classification
                    && decision["actor"].as_str().is_some_and(|actor| !actor.trim().is_empty()),
                "immutable prior resolution differs"
            );
            let proof: MergeReleaseProof = serde_json::from_value(decision["proof"].clone())?;
            // Revalidate the requested digest/remote identity below; do not
            // accept a changed manifest merely because a row is already settled.
            match &proof {
                MergeReleaseProof::NeverDispatched => return Ok((None, Some(proof))),
                MergeReleaseProof::RequestNotExecuted { response_sha256 } => {
                    ensure!(
                        manifest.response_sha256.as_deref() == Some(response_sha256.as_str()),
                        "resolution evidence changed"
                    );
                    let (raw,_) = journal.as_ref().context("recorded rejection journal missing")?;
                    ensure!(*response_sha256 == format!("{:x}",Sha256::digest(raw.as_bytes()))
                        && merge_request_not_executed(raw), "recorded rejection journal differs");
                    return Ok((None, Some(proof)));
                }
                _ => (),
            }
        }
        ensure!(
            remote["merged"].as_bool() == Some(classification == "manual"),
            "classification differs from remote merged state"
        );
        if record.state == "reserved" {
            return Ok((None, Some(MergeReleaseProof::NeverDispatched)));
        }
        if let Some((raw, _)) = &journal {
            let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));
            if merge_request_not_executed(raw) {
                ensure!(
                    manifest.response_sha256.as_deref() == Some(digest.as_str()),
                    "response digest differs from controller journal"
                );
                return Ok((
                    None,
                    Some(MergeReleaseProof::RequestNotExecuted {
                        response_sha256: digest,
                    }),
                ));
            }
        }
        ensure!(classification=="manual" && remote["head"]["sha"].as_str()!=Some(record.operation.binding.head.as_str()),
            "insufficient evidence: open state, same account or same-head merged state cannot release an uncertain dispatch");
        if let Some((raw, _)) = &journal {
            let value: Value = serde_json::from_str(raw)?;
            ensure!(
                value
                    .pointer("/data/mergePullRequest/pullRequest/state")
                    .and_then(|v| v.as_str())
                    != Some("MERGED"),
                "remote observation contradicts original accepted response"
            );
        }
        let head = remote["head"]["sha"]
            .as_str()
            .context("remote merged head missing")?;
        let commit = remote["merge_commit_sha"]
            .as_str()
            .context("remote merge commit missing")?;
        let at = remote["merged_at"]
            .as_str()
            .context("remote merge time missing")?;
        ensure!(
            manifest.remote_head.as_deref() == Some(head)
                && manifest.merge_commit.as_deref() == Some(commit)
                && manifest.merged_at.as_deref() == Some(at),
            "manifest does not match observed external merge"
        );
        Ok((
            None,
            Some(MergeReleaseProof::DifferentMergedHead {
                node: record.node.clone(),
                pr: record.pr,
                head: head.into(),
                merge_commit: commit.into(),
                merged_at: at.into(),
            }),
        ))
    })();
    let (receipt, proof) = match validated {
        Ok(value) => value,
        Err(error) => return failure(operation, 3, "merge-reconciliation-required", error),
    };
    if record.state == "released" && proof.is_some()
        && record.operation.receipt.is_some() && record.operation.receipt == record.resolution {
        // The authorized operator is reading an immutable prior decision, not
        // making a new one. Do not open a writer or touch a newer live lease.
        let decision:Value = match serde_json::from_str(record.resolution.as_deref().unwrap()) {
            Ok(value) => value,
            Err(error) => return failure(operation,4,"workflow-state-unavailable",error),
        };
        if serde_json::to_value(proof.as_ref().unwrap()).ok().as_ref() != Some(&decision["proof"]) {
            return failure(operation,3,"merge-reconciliation-required","immutable resolution evidence changed");
        }
        return (0,json!({"schema_version":1,"operation":operation,"workflow":record.operation.workflow,
            "resolved":true,"classification":classification,"actor":actor,"resolution_actor":decision["actor"],
            "reason":"merge-resolution-already-recorded"}));
    }
    // Non-confirming resolutions must not compete with a live workflow. Check
    // before writer initialization; the store repeats this under its write lock.
    if proof.is_some() {
        match store.snapshot(&record.operation.workflow) {
            Ok(Some(snapshot))
                if snapshot.lease_until_ms > chrono::Utc::now().timestamp_millis() =>
            {
                return failure(
                    operation,
                    3,
                    "workflow-active",
                    "stop or await the active workflow before resolving",
                )
            }
            Ok(_) => (),
            Err(error) => return failure(operation, 4, "workflow-state-unavailable", error),
        }
    }
    drop(store);
    let result = (|| -> Result<()> {
        let mut store = WorkflowStore::open(path)?;
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(receipt) = receipt {
            store.confirm_merge_dispatch(&record.operation, &receipt, now)?;
            store.record_confirmed_merge_resolution(
                &record.operation,
                &actor,
                manifest
                    .response_sha256
                    .as_deref()
                    .context("response digest missing")?,
                now,
            )?;
            store.reconcile_confirmed_workflow(&record.operation.id, now)?;
        } else {
            store.resolve_merge_without_debit(
                &record.operation,
                classification,
                &actor,
                &proof.context("release proof missing")?,
                now,
            )?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => (
            0,
            json!({"schema_version":1,"operation":operation,"workflow":record.operation.workflow,"resolved":true,
            "classification":classification,"actor":actor,"reason":"merge-resolution-recorded"}),
        ),
        Err(error) => failure(operation, 3, "merge-reconciliation-required", error),
    }
}

pub async fn run(operation: &str, classification: &str, evidence: &Path, json_output: bool) -> i32 {
    let (code, body) = resolve(
        &crate::autoship_workflow::default_path(),
        operation,
        classification,
        evidence,
    )
    .await;
    if json_output {
        println!("{body}");
    } else if code == 0 {
        println!("{operation}: resolved as {classification}");
    } else {
        println!(
            "{operation}: {} — {}",
            body["reason"].as_str().unwrap_or("unknown"),
            body["error"].as_str().unwrap_or("unknown")
        );
    }
    code
}
