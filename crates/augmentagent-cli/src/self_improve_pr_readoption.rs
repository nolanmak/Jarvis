//! #1537 explicit PR readoption uses ordinary authenticated intake. It records
//! current provenance but never erases owner/security holds or review evidence.
use super::refuse;
use crate::autoship_workflow::{submission, WorkflowStore};
use anyhow::Result;
use serde_json::{json, Value};
use std::path::Path;

pub(crate) struct Request {
    pub repository: String,
    pub number: u64,
    pub reason: String,
    pub request_id: String,
    pub origin: String,
}

pub(crate) async fn apply(repo: &Path, path: &Path, input: &Request) -> Result<Value> {
    let request = submission::Request {
        repository: input.repository.clone(),
        target: submission::Target::Pr(input.number),
        provider: None,
        until: submission::Milestone::Merged,
    }
    .normalized()
    .map_err(|e| refuse(2, "invalid-request", e.to_string()))?;
    let reason = input.reason.trim();
    if reason.is_empty()
        || reason.len() > 500
        || reason.chars().any(char::is_control)
        || input.request_id.is_empty()
        || input.request_id.len() > 128
        || !input
            .request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
        || !matches!(input.origin.as_str(), "cli" | "discord")
    {
        return Err(refuse(
            2,
            "invalid-request",
            "bounded reason, request id and authenticated origin required",
        ));
    }
    let principal = super::targeted::authorize_command(repo, &request)
        .await
        .map_err(|e| refuse(2, "target-authorization-failed", e.to_string()))?;
    // Resolve before opening writable state: a fork/closed/foreign target must
    // not leave an authority record. A replay still checks the current target.
    let super::targeted::Resolved::Pr(pr) = super::targeted::resolve(repo, &request)
        .await
        .map_err(|e| refuse(2, "invalid-target", e.to_string()))?
    else {
        return Err(refuse(2, "invalid-target", "PR required"));
    };
    let provenance = json!({"schema_version":1,"origin":input.origin,"reason":reason,
        "head":pr.head,"base":pr.base,"branch":pr.branch,"source_issue":pr.source_issue,
        "author":pr.author,"repository":request.repository,"pr":pr.pr});
    let mut store = WorkflowStore::open(path)
        .map_err(|e| refuse(4, "workflow-state-unavailable", e.to_string()))?;
    let (admission, replayed) = store
        .readopt_pr_authorized(
            &input.request_id,
            &principal,
            &request,
            &provenance,
            chrono::Utc::now().timestamp_millis(),
        )
        .map_err(|e| {
            if e.to_string() == "request-id-conflict" {
                refuse(2, "request-id-conflict", e.to_string())
            } else {
                refuse(4, "workflow-state-unavailable", e.to_string())
            }
        })?;
    Ok(
        json!({"schema_version":1,"accepted":admission.accepted,"complete":false,
        "workflow":admission.workflow,"joined":admission.joined,"replayed":replayed,
        "repository":request.repository,"target":{"kind":"pr","number":input.number},
        "request_id":input.request_id,"principal":principal,"origin":input.origin,
        "reason_code":"owner_readopted","next_action":"fresh_review_and_ci",
        "hold_history":"preserved","execution":"not-dispatched"}),
    )
}
