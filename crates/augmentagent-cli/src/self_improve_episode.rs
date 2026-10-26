//! Bounded repair episodes over verified facts, with immutable Git recovery refs.
use super::*;
use crate::autoship_episode::{Candidate, Decision, Observation};
use crate::autoship_review::{Finding, Ruling};
use sha2::{Digest, Sha256};

pub(super) struct Evaluated {
    pub decision: Decision,
    pub epoch: String,
    pub remaining: Vec<Finding>,
}

fn effective_findings(review: &crate::autoship_review::ReviewEvidence) -> Result<Vec<Finding>> {
    let mut findings = std::collections::BTreeMap::new();
    for finding in review.passes.iter().flat_map(|pass| &pass.findings) {
        let dismissed = review.adjudication.as_ref().is_some_and(|receipt| {
            receipt.binding == review.binding
                && receipt.finding_fingerprints.get(&finding.id)
                    == finding.fingerprint().ok().as_ref()
                && matches!(
                    receipt.rulings.get(&finding.id),
                    Some(Ruling::Dismiss { .. })
                )
        });
        if !dismissed {
            findings.insert(finding.id.clone(), finding.clone());
        }
    }
    Ok(findings.into_values().collect())
}

pub(super) async fn evaluate(
    workflow: &mut workflow::WorkflowGuard,
    review: &IndependentReview,
    verified_criteria: usize,
    lines: usize,
) -> Result<Evaluated> {
    let structured = review.structured.as_ref();
    let complete = structured.is_some_and(|r| {
        r.passes.len() == 3
            && r.passes
                .iter()
                .all(|p| p.completed && p.coverage.complete())
    });
    if let Some(review) = structured {
        anyhow::ensure!(
            review.binding.head == workflow.binding().head,
            "episode review is stale"
        );
    }
    if let Some(evidence) = structured {
        workflow.record_review(
            &format!("episode-evidence-{}", uuid::Uuid::new_v4()),
            review.approved(),
            &serde_json::to_string(evidence)?,
        )?;
    }
    let remaining = structured
        .map(effective_findings)
        .transpose()?
        .unwrap_or_default();
    let epoch = match structured {
        Some(r) => format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &r.binding.contract_hash,
                &r.binding.base,
                &r.binding.environment_id
            ))?)
        ),
        None => workflow
            .prior_episode_epoch()?
            .unwrap_or_else(|| format!("uninitialized:{}", workflow.binding().contract)),
    };
    let review_receipt = serde_json::to_vec(&structured)?;
    let observation = Observation {
        receipt: format!(
            "review:{:x}",
            Sha256::digest(serde_json::to_vec(&(
                workflow.binding(),
                &review_receipt,
                review.available
            ))?)
        ),
        epoch: epoch.clone(),
        candidate: Candidate {
            binding: workflow.binding().clone(),
            tests_passed: true,
            guards_passed: true,
            verified_criteria: verified_criteria
                .try_into()
                .context("criterion count overflow")?,
            open_findings: remaining.iter().map(|finding| finding.id.clone()).collect(),
            lines_changed: lines.try_into().unwrap_or(u32::MAX),
        },
        review_complete: complete,
        contradictory_requirements: structured.is_some_and(|r| {
            r.adjudication.is_none() && remaining.iter().any(|f| !f.conflicts_with.is_empty())
        }),
        infrastructure_failed: !review.available,
        autonomous_merge_observed: false,
    };
    let mut decision = workflow.observe_episode(&observation)?;
    if complete && review.available {
        for (action, phase, payload) in workflow.episode_dispatch(&epoch)? {
            let mut details: serde_json::Value = serde_json::from_str(&payload)?;
            if matches!(phase.as_str(), "repair_dispatch" | "uncertain_legacy")
                || (remaining.is_empty()
                    && matches!(
                        phase.as_str(),
                        "prepared" | "scope_dispatch" | "scope_complete"
                    ))
            {
                workflow.checkpoint_episode_dispatch(&epoch,&action,"reconciled",&serde_json::json!({"review_receipt":observation.receipt,"head":workflow.binding().head}).to_string())?;
                continue;
            }
            let resumable = if phase == "scope_dispatch" {
                // A read-only scoper's response was lost. Never call it again;
                // the newly verified candidate and remaining facts are sufficient
                // to resume its still-unlaunched repair intent honestly.
                details["plan"] = serde_json::json!("Prior scoper outcome is uncertain; use the reverified contract and findings, without assuming an unobserved plan.");
                workflow.checkpoint_episode_dispatch(
                    &epoch,
                    &action,
                    "scope_complete",
                    &details.to_string(),
                )?;
                true
            } else {
                matches!(phase.as_str(), "prepared" | "scope_complete")
            };
            if resumable && !remaining.is_empty() {
                let best_head = details["best_head"]
                    .as_str()
                    .unwrap_or(&observation.candidate.binding.head)
                    .to_string();
                decision = if action == "fresh_session" {
                    Decision::FreshSession { best_head }
                } else {
                    Decision::Rescope { best_head }
                };
                break;
            }
        }
    }
    Ok(Evaluated {
        decision,
        epoch,
        remaining,
    })
}

async fn git(root: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, error) = run("git", args, root).await?;
    anyhow::ensure!(ok, "episode Git operation failed: {error}");
    Ok(out.trim().to_string())
}

/// Create-only content-addressed refs preserve all candidates before a reset.
async fn preserve(root: &Path, workflow: &str, head: &str) -> Result<()> {
    let namespace = format!("{:x}", Sha256::digest(workflow.as_bytes()));
    let reference = format!("refs/autoship/{namespace}/candidates/{head}");
    let (exists, actual, _) = run("git", &["rev-parse", "--verify", &reference], root).await?;
    if exists {
        anyhow::ensure!(actual.trim() == head, "immutable candidate ref changed");
        return Ok(());
    }
    git(
        root,
        &["update-ref", &reference, head, &"0".repeat(head.len())],
    )
    .await?;
    Ok(())
}

pub(super) async fn restore_best(root: &Path, workflow: &str, best: &str) -> Result<()> {
    anyhow::ensure!(
        git(root, &["status", "--porcelain", "--untracked-files=all"])
            .await?
            .is_empty(),
        "checkpoint unfinished edits before resetting repair episode"
    );
    let head = git(root, &["rev-parse", "HEAD"]).await?;
    preserve(root, workflow, &head).await?;
    preserve(root, workflow, best).await?;
    git(root, &["reset", "--hard", best]).await?;
    if head != best {
        // Keep the best tree while retaining the published candidate as an
        // ancestor. Recovery must not require rewriting a remote PR branch.
        git(
            root,
            &[
                "-c",
                "user.name=AugmentAgent",
                "-c",
                "user.email=augmentagent@localhost",
                "merge",
                "--strategy=ours",
                "--no-ff",
                "-m",
                "autoship: restore best verified candidate",
                &head,
            ],
        )
        .await?;
    }
    Ok(())
}

pub(super) struct RepairSession {
    pub opts: augmentagent_channel_core::ReasonerOpts,
    pub prompt: String,
    pub epoch: String,
    pub action: String,
}

pub(super) async fn session(
    reasoner: &FallbackReasoner,
    issue: &Issue,
    root: &Path,
    workflow: &mut workflow::WorkflowGuard,
    prepared: &acceptance::PreparedAcceptance,
    evaluated: &Evaluated,
) -> Result<Option<RepairSession>> {
    session_for(
        reasoner,
        crate::autoship_contract::AcceptanceSource::Issue(issue.number),
        &issue.title,
        &issue.body,
        root,
        workflow,
        prepared,
        evaluated,
    )
    .await
}

pub(super) async fn session_for(
    reasoner: &FallbackReasoner,
    source: crate::autoship_contract::AcceptanceSource,
    title: &str,
    body: &str,
    root: &Path,
    workflow: &mut workflow::WorkflowGuard,
    prepared: &acceptance::PreparedAcceptance,
    evaluated: &Evaluated,
) -> Result<Option<RepairSession>> {
    let (best, rescope) = match &evaluated.decision {
        Decision::FreshSession { best_head } => (best_head, false),
        Decision::Rescope { best_head } => (best_head, true),
        _ => anyhow::bail!("episode does not request a new repair context"),
    };
    let action = if rescope { "rescope" } else { "fresh_session" };
    workflow.admit_episode_action(&evaluated.epoch, action)?;
    let (_, phase, payload) = workflow
        .episode_dispatch(&evaluated.epoch)?
        .into_iter()
        .find(|(a, _, _)| a == action)
        .context("episode dispatch missing")?;
    if !matches!(phase.as_str(), "prepared" | "scope_complete") {
        return Ok(None);
    }
    let mut details: serde_json::Value = serde_json::from_str(&payload)?;
    details["best_head"] = serde_json::json!(best);
    workflow.checkpoint_episode_dispatch(&evaluated.epoch, action, &phase, &details.to_string())?;
    workflow
        .transition(
            crate::autoship_workflow::Stage::Reconcile,
            "episode_checkpoint",
            "restore_best_verified",
            0,
        )
        .await?;
    restore_best(root, workflow.id(), best).await?;
    // Reset is never a verifier waiver. Verify the exact restored commit again.
    verification_gate(root).await?;
    acceptance::verify_candidate(root, &prepared.baseline).await?;
    workflow
        .transition(
            crate::autoship_workflow::Stage::Build,
            "best_candidate_reverified",
            if rescope {
                "rescope_repair"
            } else {
                "fresh_repair_session"
            },
            0,
        )
        .await?;
    let best_review = workflow
        .episode_review_at_head(best)?
        .context("best candidate review evidence missing")?;
    let best_findings = effective_findings(&best_review)?;
    let facts: Vec<_> = best_findings.iter().map(|f| serde_json::json!({
        "id":f.id,"criterion":f.criterion_id,"claim":f.claim,"required_change":f.requested_change,"evidence_hash":f.evidence_hash
    })).collect();
    let mut prompt = serde_json::to_string(&serde_json::json!({
        "instruction":"Repair this committed candidate against the unchanged executable acceptance contract. These are the remaining verified findings. Do not consult prior builder transcripts or prior LGTM claims. Preserve pinned verifier bytes and scope.",
        "source":{"identity":source,"title":title,"body":body},
        "contract":prepared.proposal.contract,"remaining_facts":facts
    }))?;
    if rescope {
        let plan = if phase == "scope_complete" {
            details["plan"]
                .as_str()
                .context("completed scope plan missing")?
                .to_string()
        } else {
            let mut opts = scope_opts(root.to_path_buf());
            opts.session_id = Some(uuid::Uuid::new_v4().to_string());
            workflow.checkpoint_episode_dispatch(
                &evaluated.epoch,
                action,
                "scope_dispatch",
                &details.to_string(),
            )?;
            let plan = augmentagent_channel_core::native_session::FreshBuilderSessions::new().scope(reasoner.call(&opts,&format!("Re-scope repair for this immutable contract and candidate. Read source and return the smallest implementation plan; do not weaken verifiers.\n{prompt}"))).await?;
            details["plan"] = serde_json::json!(truncate(&plan, 16000));
            workflow.checkpoint_episode_dispatch(
                &evaluated.epoch,
                action,
                "scope_complete",
                &details.to_string(),
            )?;
            plan
        };
        prompt.push_str("\nIndependent re-scoping plan (unchanged contract):\n");
        prompt.push_str(&truncate(&plan, 16000));
    }
    let digest =
        Sha256::digest(format!("{}:{}:fresh-repair", workflow.id(), evaluated.epoch).as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let mut opts = fix_opts(root.to_path_buf());
    // Audit correlation only. Actual transport identity comes from FreshBuilderSessions.
    opts.session_id = Some(uuid::Uuid::from_bytes(bytes).to_string());
    Ok(Some(RepairSession {
        opts,
        prompt,
        epoch: evaluated.epoch.clone(),
        action: action.into(),
    }))
}

/// Dispatch is journaled before provider admission. A lost response remains
/// uncertain until completed candidate verification/review reconciles it.
pub(super) async fn dispatch<T>(
    workflow: &workflow::WorkflowGuard,
    intent: Option<(String, String)>,
    call: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    if let Some((epoch, action)) = &intent {
        let (_, _, payload) = workflow
            .episode_dispatch(epoch)?
            .into_iter()
            .find(|(a, _, _)| a == action)
            .context("episode dispatch missing")?;
        workflow.checkpoint_episode_dispatch(epoch, action, "repair_dispatch", &payload)?;
    }
    let result = call.await;
    if result.is_ok() {
        if let Some((epoch, action)) = &intent {
            workflow.checkpoint_episode_dispatch(epoch, action, "repair_complete", "{}")?;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reset_preserves_both_commits_and_refuses_uncommitted_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]).await.unwrap();
        git(root, &["config", "user.name", "Fixture"])
            .await
            .unwrap();
        git(root, &["config", "user.email", "fixture@example.invalid"])
            .await
            .unwrap();
        std::fs::write(root.join("code"), "best").unwrap();
        git(root, &["add", "code"]).await.unwrap();
        git(root, &["commit", "-qm", "best"]).await.unwrap();
        let best = git(root, &["rev-parse", "HEAD"]).await.unwrap();
        std::fs::write(root.join("code"), "later").unwrap();
        git(root, &["commit", "-qam", "later"]).await.unwrap();
        let later = git(root, &["rev-parse", "HEAD"]).await.unwrap();
        restore_best(root, "owner/repo#7", &best).await.unwrap();
        assert_eq!(
            git(root, &["rev-parse", "HEAD^{tree}"]).await.unwrap(),
            git(root, &["rev-parse", &format!("{best}^{{tree}}")])
                .await
                .unwrap()
        );
        assert!(
            run(
                "git",
                &["merge-base", "--is-ancestor", &later, "HEAD"],
                root
            )
            .await
            .unwrap()
            .0,
            "restored tree retains published ancestry"
        );
        let refs = git(
            root,
            &["for-each-ref", "--format=%(objectname)", "refs/autoship"],
        )
        .await
        .unwrap();
        assert!(refs.contains(&best) && refs.contains(&later));
        std::fs::write(root.join("code"), "unfinished").unwrap();
        assert!(restore_best(root, "owner/repo#7", &later).await.is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("code")).unwrap(),
            "unfinished"
        );
    }
}
