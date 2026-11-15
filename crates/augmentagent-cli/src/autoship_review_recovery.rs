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
        if !(waiting || parked || pending) {
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
        checkpoint.stage = stage;
        checkpoint.next = NextAction {
            reason: reason.into(),
            action: action.into(),
            eligible_at_ms: 0,
        };
        let updated = serde_json::to_string(&checkpoint)?;
        if !scheduled || updated != raw {
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
                stage: if due {
                    contract::WorkflowStage::Review
                } else {
                    contract::WorkflowStage::Held
                },
                outcome,
                transition: contract::ProgressTransition::Observation,
                reason_code: reason.into(),
                last_meaningful_progress_at_ms: progress,
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
            event_record.validate()?;
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
