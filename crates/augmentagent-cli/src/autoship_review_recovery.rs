//! A review request is the existing fenced workflow's checkpoint, not a second
//! executor. Reconciliation never grants review, changes binding, or clears an
//! owner hold. The normal resume path still verifies, reviews and adjudicates.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract as contract;

#[derive(Clone, Copy)]
pub(crate) enum ReviewRecoveryReadiness {
    Ready,
    CapacityWait,
    AdoptionWait,
}

/// #2383: held review reasons the reconciler re-admits on its own. Capacity
/// and latch waits are already `waiting`; unknown provenance stands down.
pub(crate) const AUTO_RESUMED_HOLDS: &[&str] = &["review-recovery-required", "review-evidence-budget"];
const AUTO_RESUME_PURPOSE: &str = "review-recovery-auto-resume";
/// Paid review re-runs per reviewed binding before the daily resume owns it.
const AUTO_RESUME_LIMIT: u32 = 3;
/// First retry after a repeated failure waits this long, doubling after.
const AUTO_RESUME_BACKOFF_MS: i64 = 15 * 60 * 1000;

impl WorkflowStore {
    /// Called only after matching a controller publication record to the remote
    /// PR/head. Unknown or superseded workflow custody requires adoption (#1537).
    /// The event and checkpoint are committed together; an unchanged poll does
    /// not create another request or move the meaningful-progress timestamp.
    pub(crate) fn reconcile_review_recovery(
        &mut self,
        repository: &str,
        pr: u64,
        branch: &str,
        head: &str,
        readiness: ReviewRecoveryReadiness,
        now: i64,
    ) -> Result<Option<bool>> {
        let Some(id) = self.resolve_pr(repository, pr)? else {
            return Ok(None);
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (raw, actual_branch, actual_pr, leased, generation, progress): (String,String,Option<u64>,i64,i64,i64) =
            tx.query_row("SELECT checkpoint,branch,pr,lease_until,generation,progress_at FROM workflow WHERE id=?1", [&id],
                |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        if actual_branch != branch
            || actual_pr != Some(pr)
            || checkpoint.binding.head != head
            || leased > now
            || checkpoint.next.eligible_at_ms > now
            || owner_control::read(&tx, &id)?.is_some_and(|control| control.paused)
            || owner_inbox::pending(&tx, &id)? > 0
        {
            return Ok(None);
        }
        let waiting = checkpoint.stage == Stage::Held
            && matches!(
                checkpoint.next.reason.as_str(),
                "no-reviewer-capacity" | "reviewer-latched" | "dependency-adoption-provenance"
            );
        let parked =
            checkpoint.stage == Stage::Reconcile && checkpoint.next.reason == "draft_parked";
        let pending = checkpoint.stage == Stage::Review
            && checkpoint.next.reason == "review-recovery-required";
        // #2383: a review the executor could not finish (format/evidence
        // recovery, evidence budget) parks on Held. Nothing else resumes it,
        // so re-admit it here on a bounded backoff per reviewed binding. Past
        // the bound it is left to the daily adopted-draft resume.
        let recovering = checkpoint.stage == Stage::Held
            && checkpoint.next.action == "resume_preserved_work"
            && AUTO_RESUMED_HOLDS.contains(&checkpoint.next.reason.as_str());
        if !(waiting || parked || pending || recovering) {
            return Ok(None);
        }
        let operations: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",
            [&id],
            |r| r.get(0),
        )?;
        let uncertain_merge: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state='dispatched')", [&id], |r|r.get(0))?;
        if operations || uncertain_merge {
            return Ok(None);
        }
        if recovering {
            let key = serde_json::to_string(&checkpoint.binding)?;
            let used: u32 = tx
                .query_row(
                    "SELECT attempts FROM bounded_attempt WHERE workflow=?1 AND purpose=?2 AND binding=?3",
                    params![id, AUTO_RESUME_PURPOSE, key],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(0);
            if used >= AUTO_RESUME_LIMIT {
                return Ok(None);
            }
            if used > 0 {
                let held_at: i64 = tx.query_row(
                    "SELECT COALESCE(MAX(at_ms),0) FROM workflow_event WHERE workflow=?1 AND kind='checkpoint'",
                    [&id],
                    |r| r.get(0),
                )?;
                if now < held_at.saturating_add(AUTO_RESUME_BACKOFF_MS.saturating_mul(1 << (used - 1).min(8))) {
                    return Ok(None);
                }
            }
            if matches!(readiness, ReviewRecoveryReadiness::Ready) {
                tx.execute(
                    "INSERT INTO bounded_attempt(workflow,purpose,binding,attempts) VALUES(?1,?2,?3,1)
                     ON CONFLICT(workflow,purpose,binding) DO UPDATE SET attempts=attempts+1",
                    params![id, AUTO_RESUME_PURPOSE, key],
                )?;
            }
        }
        // A provenance wait is not cleansed by a provider becoming available.
        let readiness = if checkpoint.next.reason == "dependency-adoption-provenance" {
            ReviewRecoveryReadiness::AdoptionWait
        } else {
            readiness
        };
        let (stage, outcome, reason, action, due) = match readiness {
            ReviewRecoveryReadiness::Ready => (
                Stage::Review,
                contract::WorkflowOutcome::Progressed,
                "review-recovery-required",
                "complete_review_or_adjudication",
                true,
            ),
            ReviewRecoveryReadiness::CapacityWait => (
                Stage::Held,
                contract::WorkflowOutcome::CapacityWait,
                "no-reviewer-capacity",
                "resume_preserved_work",
                false,
            ),
            ReviewRecoveryReadiness::AdoptionWait => (
                Stage::Held,
                contract::WorkflowOutcome::DependencyWait,
                "dependency-adoption-provenance",
                "await_provenance_adoption",
                false,
            ),
        };
        let scheduled: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='review_recovery_scheduled' AND json_extract(payload,'$.binding.head')=?2 AND json_extract(payload,'$.binding.contract_version')=?3 AND json_extract(payload,'$.binding.evidence_version')=?4)",
            params![id,checkpoint.binding.head,checkpoint.binding.contract,checkpoint.binding.evidence_hash],|r|r.get(0))?;
        let previous: Option<String> = tx.query_row(
            "SELECT payload FROM workflow_event WHERE workflow=?1 AND kind IN ('review_recovery_scheduled','review_recovery_wait_changed') ORDER BY seq DESC LIMIT 1",
            [&id], |r|r.get(0)).optional()?;
        let previous = previous
            .map(|raw| serde_json::from_str::<contract::WorkflowEvent>(&raw))
            .transpose()?;
        let public_stage = if due {
            contract::WorkflowStage::Review
        } else {
            contract::WorkflowStage::Held
        };
        // The executor may already have checkpointed an outage. Compare with
        // the last emitted review event so that this real wait is not lost.
        // A legacy stage-start clock cannot grant shared meaningful progress.
        let state_changed = previous.as_ref().is_some_and(|prior| {
            prior.stage != public_stage
                || prior.outcome != outcome
                || prior.reason_code != reason
                || prior.next.action != action
        });
        let previous_stage = checkpoint.stage;
        checkpoint.stage = stage;
        checkpoint.next = NextAction {
            reason: reason.into(),
            action: action.into(),
            eligible_at_ms: 0,
        };
        let updated = serde_json::to_string(&checkpoint)?;
        if !scheduled || updated != raw || state_changed {
            let sequence: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq),0)+1 FROM workflow_event WHERE workflow=?1",
                [&id],
                |r| r.get(0),
            )?;
            let target = if let Some(issue) = id
                .strip_prefix(&format!("{repository}#"))
                .and_then(|v| v.parse().ok())
            {
                contract::WorkflowTarget::Issue(issue)
            } else {
                contract::WorkflowTarget::PullRequest(pr)
            };
            let event_record = contract::WorkflowEvent {
                schema_version: contract::SCHEMA_VERSION,
                workflow_id: id.clone(),
                sequence,
                occurred_at_ms: now,
                repository: repository.into(),
                target,
                pull_request: Some(pr),
                binding: contract::Binding {
                    head: checkpoint.binding.head.clone(),
                    contract_version: checkpoint.binding.contract.clone(),
                    evidence_version: checkpoint.binding.evidence_hash.clone(),
                },
                stage: public_stage,
                outcome,
                transition: if previous
                    .as_ref()
                    .map_or(stage == previous_stage, |prior| prior.stage == public_stage)
                {
                    contract::ProgressTransition::Observation
                } else {
                    contract::ProgressTransition::StageChanged
                },
                reason_code: reason.into(),
                last_meaningful_progress_at_ms: previous
                    .as_ref()
                    .map_or(progress, |prior| prior.last_meaningful_progress_at_ms),
                next: contract::NextAction {
                    action: action.into(),
                    eligible_at_ms: None,
                },
                actor: contract::Actor::Controller,
                origin: contract::Origin::Reconciliation,
                compute_usage: tx.query_row(
                    "SELECT model_calls,tokens,wall_ms FROM workflow WHERE id=?1",
                    [&id],
                    |r| {
                        Ok(contract::ComputeUsage {
                            model_calls: r.get(0)?,
                            tokens: r.get(1)?,
                            wall_ms: r.get(2)?,
                        })
                    },
                )?,
            };
            if let Some(previous) = &previous {
                event_record.validate_after(previous)?;
            } else {
                event_record.validate()?;
            }
            // No progress credit for merely scheduling/rechecking a request.
            tx.execute(
                "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
                params![id, updated],
            )?;
            event(
                &tx,
                &id,
                generation,
                if scheduled {
                    "review_recovery_wait_changed"
                } else {
                    "review_recovery_scheduled"
                },
                &serde_json::to_string(&event_record)?,
                now,
            )?;
        }
        tx.commit()?;
        Ok(Some(due))
    }
}
