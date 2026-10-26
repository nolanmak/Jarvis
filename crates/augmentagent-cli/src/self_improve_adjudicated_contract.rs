//! Translate final Codex contract rulings into an independently registered new
//! verifier revision. Owner criteria are never replaced by reviewer wording.
use super::*;
use crate::autoship_review::{ReviewEvidence, ReviewRole, Ruling};
fn directives(
    review: &ReviewEvidence,
    prepared: &PreparedAcceptance,
) -> Result<Vec<serde_json::Value>> {
    let Some(receipt) = &review.adjudication else {
        return Ok(vec![]);
    };
    if !receipt
        .rulings
        .values()
        .any(|ruling| matches!(ruling, Ruling::Amend { .. }))
    {
        return Ok(vec![]);
    }
    review.binding.validate()?;
    ensure!(
        review.binding.contract_hash == prepared.baseline.registration.contract_hash(),
        "adjudicated amendment registered contract mismatch"
    );
    review.origin.validate(&review.binding.head)?;
    ensure!(
        !matches!(
            review.origin,
            crate::autoship_review::ReviewOrigin::DiagnosticCanary { .. }
        ),
        "diagnostic canary cannot amend shipping contract"
    );
    ensure!(
        receipt.binding == review.binding
            && receipt.actor.provider == "codex"
            && receipt.actor.backend == "native"
            && !receipt.prompt_hash.is_empty()
            && !receipt.response_hash.is_empty(),
        "adjudicated amendment lacks native final authority"
    );
    receipt
        .actor
        .independent_for(&review.builders, &review.origin)?;
    ensure!(
        review.passes.len() == 3,
        "adjudicated amendment lacks complete review roles"
    );
    let mut sessions = std::collections::BTreeSet::new();
    for role in [ReviewRole::Qa, ReviewRole::Diff, ReviewRole::System] {
        ensure!(
            review
                .passes
                .iter()
                .filter(|pass| pass.role == role)
                .count()
                == 1,
            "adjudicated amendment duplicate/missing review role"
        );
    }
    let mut findings = BTreeMap::new();
    let expected_coverage = &review.passes[0].coverage;
    for pass in &review.passes {
        ensure!(
            pass.coverage.required_paths == expected_coverage.required_paths
                && pass.coverage.supplied_files == expected_coverage.supplied_files,
            "adjudicated amendment reviews supplied different candidate evidence"
        );
        ensure!(
            pass.completed
                && pass.binding == review.binding
                && pass.coverage.binding == review.binding
                && pass.coverage.complete(),
            "adjudicated amendment incomplete/stale review"
        );
        pass.actor
            .independent_for(&review.builders, &review.origin)?;
        ensure!(
            sessions.insert(pass.actor.session_id.clone()),
            "adjudicated amendment shared review session"
        );
        for session in &pass.additional_sessions {
            ensure!(
                sessions.insert(session.clone()),
                "adjudicated amendment reused shard session"
            );
        }
        for finding in &pass.findings {
            finding.validate()?;
            ensure!(
                receipt.finding_fingerprints.get(&finding.id) == Some(&finding.fingerprint()?),
                "adjudicated amendment finding evidence changed"
            );
            if let Some(prior) = findings.insert(finding.id.clone(), finding) {
                ensure!(
                    prior == finding,
                    "adjudicated amendment duplicate inconsistent finding"
                );
            }
        }
    }
    ensure!(
        !sessions.contains(&receipt.actor.session_id),
        "final amendment authority reused reviewer session"
    );
    let mut directives = vec![];
    for (id, ruling) in &receipt.rulings {
        let finding = findings
            .get(id)
            .context("adjudicator ruled on absent finding")?;
        if let Ruling::Amend {
            evidence,
            replacement_criterion,
            rerun_checks,
        } = ruling
        {
            ensure!(
                !evidence.trim().is_empty()
                    && !replacement_criterion.trim().is_empty()
                    && !rerun_checks.is_empty()
                    && rerun_checks.iter().all(|check| !check.trim().is_empty()),
                "adjudicated amendment lacks evidence or named checks"
            );
            let owner = prepared
                .proposal
                .contract
                .criteria
                .iter()
                .find(|criterion| criterion.id == finding.criterion_id)
                .context("adjudication references unknown owner criterion")?;
            directives.push(serde_json::json!({"finding_id":id,"owner_criterion_id":owner.id,"immutable_owner_requirement":owner.requirement,
                "derived_interpretation":replacement_criterion,"evidence":evidence,"named_checks_as_data_only":rerun_checks}));
        }
    }
    Ok(directives)
}
pub(crate) async fn apply(
    reasoner: &FallbackReasoner,
    source: AcceptanceSource,
    title: &str,
    worktree: &Path,
    prepared: &mut PreparedAcceptance,
    control: &Path,
    review: &ReviewEvidence,
    workflow: &mut workflow::WorkflowGuard,
) -> Result<bool> {
    let interpretations = directives(review, prepared)?;
    if interpretations.is_empty() {
        return Ok(false);
    }
    ensure!(
        prepared.proposal.contract.source()? == source
            && actual_head(worktree).await? == review.binding.head,
        "adjudicated amendment source/head changed"
    );
    let bound =
        serde_json::to_string(&serde_json::json!({"review":review,"directives":interpretations}))?;
    ensure!(
        bound.len() <= 2 * 1024 * 1024,
        "adjudicated amendment evidence exceeds bound"
    );
    // Preserve the exact final authority before any contract mutation; resumed
    // loops discard in-memory review state after the revision changes.
    let artifact = control.join(format!(
        "adjudicated-contract-{}.json",
        amendment::digest(bound.as_bytes())
    ));
    if artifact.exists() {
        ensure!(
            std::fs::read(&artifact)? == bound.as_bytes(),
            "final adjudication artifact changed"
        );
    } else {
        use std::io::Write;
        let mut staged = tempfile::NamedTempFile::new_in(control)?;
        staged.write_all(bound.as_bytes())?;
        staged.as_file().sync_all()?;
        staged
            .persist_noclobber(&artifact)
            .map_err(|error| error.error)?;
        std::fs::File::open(control)?.sync_all()?;
    }
    let result = amendment::try_amend(
        reasoner,
        source,
        title,
        worktree,
        prepared,
        control,
        workflow,
        "Final native Codex adjudication requests a derived contract interpretation change",
        true,
        Some(&bound),
    )
    .await;
    match result {
        Ok(Some(next)) => {
            *prepared = next;
            workflow.bind_acceptance(prepared)?;
            workflow
                .transition(
                    crate::autoship_workflow::Stage::Verify,
                    "adjudicated_contract_amended",
                    "verify_new_revision",
                    0,
                )
                .await?;
            Ok(true)
        }
        Ok(None) => bail!("adjudicated amendment produced no executable revision"),
        Err(error) => {
            let detail = format!("{error:#}");
            let (outcome, action) = if detail.contains("immutable owner requirements")
                || detail.contains("weakened or dropped requirement")
            {
                (
                    "owner_requirement_conflict",
                    "dependency_owner_requirement_decision",
                )
            } else if detail.contains("epoch budget exhausted") {
                (
                    "adjudicated_amendment_budget",
                    "dependency_new_adjudication_evidence",
                )
            } else {
                ("adjudicated_amendment_retry", "retry_adjudicated_amendment")
            };
            let mut evidence = vec![detail.clone()];
            if action.starts_with("dependency_") {
                if let Some(pr) = workflow.pr()? {
                    if let Ok((true, raw, _)) = run(
                        "gh",
                        &["pr", "view", &pr.to_string(), "--json", "headRefOid,body"],
                        worktree,
                    )
                    .await
                    {
                        if let Ok(remote) = serde_json::from_str::<serde_json::Value>(&raw) {
                            let request_hash = if let AcceptanceSource::Issue(issue) = source {
                                match run(
                                    "gh",
                                    &["issue", "view", &issue.to_string(), "--json", "title,body"],
                                    worktree,
                                )
                                .await
                                {
                                    Ok((true, request, _)) => {
                                        Some(amendment::digest(request.as_bytes()))
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };
                            evidence.push(serde_json::json!({"kind":"adjudicated_dependency","remote_head":remote["headRefOid"],
                                "pr_body_hash":amendment::digest(remote["body"].as_str().unwrap_or("").as_bytes()),"request_hash":request_hash}).to_string());
                        }
                    }
                }
            }
            workflow
                .checkpoint(
                    crate::autoship_workflow::Stage::Scope,
                    outcome,
                    action,
                    chrono::Utc::now().timestamp_millis() + 300_000,
                    &evidence,
                )
                .await?;
            Err(error).context(format!("final contract ruling action: {action}"))
        }
    }
}
