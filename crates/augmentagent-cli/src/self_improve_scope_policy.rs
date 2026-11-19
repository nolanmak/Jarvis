//! Scheduling adapters for the one-time safe scope policy admission.
use super::*;
use crate::autoship_workflow::WorkflowStore;
pub(super) async fn migrate(root:&Path,dry_run:bool)->Result<usize> {
    if dry_run {return Ok(0);}
    let state=workflow::path();
    if WorkflowStore::open_read_only(&state)?.is_none() {return Ok(0);}
    let repository=workflow::canonical_repository(root).await?;
    let count=WorkflowStore::open(&state)?.migrate_scope_policy(&repository,chrono::Utc::now().timestamp_millis())?;
    if count>0 {info!(count,"reconsidered legacy scope holds under safe-default policy without model calls");}
    Ok(count)
}
pub(super) fn pending(state: &Path, repository: &str, number: u64) -> Result<bool> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(false);
    };
    store.scope_policy_pending(&crate::autoship_workflow::workflow_id(repository, number)?)
}
pub(super) fn retries(state: &Path, repository: &str) -> Result<std::collections::BTreeSet<u64>> {
    let Some(store) = WorkflowStore::open_read_only(state)? else {
        return Ok(Default::default());
    };
    store.scope_policy_retries(repository)
}
pub(super) async fn readiness_blocked(
    root: &Path,
    repository: &str,
    issue: &Issue,
) -> Result<bool> {
    if pending(&workflow::path(), repository, issue.number)? {
        return Ok(false);
    }
    owner_checkout_recovery::readiness_blocked(root, repository, issue).await
}

pub(super) const SCOPING_POLICY:&str="\nThe owner authorizes conservative reversible scope assumptions. Preserve the independently recorded assumed decisions below as implementation constraints. Unknown external capability/qualification means unavailable: prefer the existing local path with optional integration disabled. Never invent successful external qualification, spend, credentials, sensitive-data exposure, or production changes. A routine unresolved choice is not a reason to stop; independent readiness escalates only all-sensitive choices.\n";

/// Carry already-recorded choices into builder and contract guidance even if
/// the spec model refuses or is unavailable. Original requirements are intact;
/// this cannot create a qualification witness or change guarded-path policy.
pub(super) fn constrain_scope(
    scope: Option<ScopeOutcome>,
    owner: &str,
    readiness: Option<&str>,
) -> Result<Option<ScopeOutcome>> {
    let Some(readiness) = readiness else {
        return Ok(scope);
    };
    let value: serde_json::Value = serde_json::from_str(readiness)?;
    let Some(values) = value
        .get("assumed_decisions")
        .and_then(serde_json::Value::as_array)
        .filter(|v| !v.is_empty())
    else {
        return Ok(scope);
    };
    let guidance=format!("\n\nRecorded assumed decisions (implementation constraints, not proof of external qualification):\n{}",serde_json::to_string(values)?);
    let mut scope = scope.unwrap_or(ScopeOutcome {
        fixable: true,
        complexity: Complexity::Hard,
        est_diff_lines: None,
        guarded_paths: false,
        body: owner.into(),
        criteria: vec![],
        relevance: Relevance::Current,
    });
    if !scope.body.contains(&guidance) {
        scope.body.push_str(&guidance);
    }
    Ok(Some(scope))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_or_replayed_scope_keeps_recorded_assumptions_and_original_requirements() {
        let readiness=serde_json::json!({"assumed_decisions":[{"decision":"Keep optional integration disabled","constraints":"No new credentials or spend"}]}).to_string();
        let scope = constrain_scope(None, "Original complete owner request", Some(&readiness))
            .unwrap()
            .unwrap();
        assert!(scope.body.starts_with("Original complete owner request"));
        assert!(scope.body.contains("Keep optional integration disabled"));
        assert_eq!(scope.complexity, Complexity::Hard);
        let first = scope.body.clone();
        let again = constrain_scope(
            Some(scope),
            "Original complete owner request",
            Some(&readiness),
        )
        .unwrap()
        .unwrap();
        assert_eq!(again.body, first);
    }
}

pub(super) async fn finish_prediction(repo:&Path,issue:&Issue,reason:&str,_scope_body:&str,plan:&[Slice],
    guard:&mut workflow::WorkflowGuard,accounting:&workflow::CapacityAccounting,_automatic:bool)->Result<Option<RunReport>> {
    if let Some(report)=accounting.report()? {return Ok(Some(report));}
    if plan.len()>=MIN_SLICES {
        match file_slices(repo,issue,plan,guard).await {
            Ok(children) if !children.is_empty()=> {
                guard.transition(crate::autoship_workflow::Stage::Scope,"scope_deferred","retry_scope",chrono::Utc::now().timestamp_millis()+300_000).await?;
                if let Some(report)=accounting.report()? {return Ok(Some(report));}
                return Ok(Some(RunReport::triage(format!("issue #{}: oversized — sliced into {} child issues ({})",issue.number,children.len(),
                    children.iter().map(|n|format!("#{n}")).collect::<Vec<_>>().join(", ")))));
            },
            Ok(_)=>(),
            Err(error)=> {
                if let Some(report)=linked_claims::report(&error) {return Ok(Some(report));}
                warn!(issue=issue.number,"filing slices failed: {error:#}");
            },
        }
    }
    // A model's path/size forecast supplies no authority to stop the issue.
    // Actual diff, owner, review and CI guards remain enforced after building.
    warn!(issue=issue.number,prediction=reason,"scope forecast is advisory; continue within normal build limits");
    Ok(None)
}
