//! Durable integration admission. This schedules the existing fenced executor;
//! metadata and dependency observations never grant review or merge permission.
use super::*;
use augmentagent_channel_core::autoship_workflow_contract as public;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IntegrationReason {
    Unknown,
    Conflict,
    BaseChanged,
    Parent,
    Cycle,
    Unavailable,
    Resolved,
    ReviewerCapacity,
    Provenance,
}
impl IntegrationReason {
    fn code(self) -> &'static str {
        match self {
            Self::Unknown => "mergeability_unknown",
            Self::Conflict => "integration_conflict",
            Self::BaseChanged => "integration_base_changed",
            Self::Parent => "integration_parent_pending",
            Self::Cycle => "integration_dependency_cycle",
            Self::Unavailable => "integration_parent_unavailable",
            Self::Resolved => "integration_metadata_resolved",
            Self::ReviewerCapacity => "integration_reviewer_capacity",
            Self::Provenance => "dependency-adoption-provenance",
        }
    }
    fn due(self) -> bool {
        matches!(self, Self::Conflict | Self::BaseChanged)
    }
}
impl WorkflowStore {
    /// Free routing hint for admission crashes whose checkpoint no longer
    /// carries integration's reason. This grants no claim or model authority;
    /// reconcile_integration rechecks the current lease/controls transactionally.
    pub(crate) fn integration_admission_interrupted(
        &self,
        workflow: &str,
        head: &str,
        base: &str,
    ) -> Result<bool> {
        let row: Option<(String, String)> = self.conn.query_row(
            "SELECT w.checkpoint,r.binding FROM workflow w JOIN integration_recovery r ON r.workflow=w.id
             WHERE w.id=?1 AND r.base=?2 AND r.kind IN ('integration_conflict','integration_base_changed')",
            params![workflow, base], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()?;
        let Some((checkpoint, original)) = row else {
            return Ok(false);
        };
        let checkpoint: Checkpoint = serde_json::from_str(&checkpoint)?;
        Ok(checkpoint.binding.head == head
            && checkpoint.stage == Stage::Reconcile
            && checkpoint.next.reason == "resume_started"
            && checkpoint.next.action == "reconcile_main"
            && serde_json::from_str::<Binding>(&original).is_ok_and(|binding| binding.head == head))
    }
    pub(crate) fn defer_integration_review(
        &mut self,
        repository: &str,
        pr: u64,
        branch: &str,
        reason: IntegrationReason,
        now: i64,
    ) -> Result<bool> {
        ensure!(
            matches!(
                reason,
                IntegrationReason::ReviewerCapacity | IntegrationReason::Provenance
            ),
            "invalid integration review wait"
        );
        let Some(id) = self.resolve_pr(repository, pr)? else {
            return Ok(false);
        };
        let Some(snapshot) = self.snapshot(&id)? else {
            return Ok(false);
        };
        if !matches!(
            snapshot.checkpoint.next.reason.as_str(),
            "integration_conflict" | "integration_base_changed" | "integration_reviewer_capacity"
        ) {
            return Ok(false);
        }
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT base,fingerprint FROM integration_recovery WHERE workflow=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((base, fingerprint)) = row else {
            return Ok(false);
        };
        let _ = self.reconcile_integration(
            repository,
            pr,
            branch,
            &snapshot.checkpoint.binding.head,
            &base,
            &fingerprint,
            reason,
            now,
        )?;
        // A concurrent lease or owner hold keeps its stronger authority. The
        // caller must still return a wait, never legacy stand-down/closure.
        Ok(true)
    }
    /// Exact current ownership, controls and in-flight effects are checked in
    /// the same transaction as the request/event/checkpoint. No lease is stolen.
    pub(crate) fn reconcile_integration(
        &mut self,
        repository: &str,
        pr: u64,
        branch: &str,
        head: &str,
        base: &str,
        fingerprint: &str,
        reason: IntegrationReason,
        now: i64,
    ) -> Result<Option<bool>> {
        let Some(id) = self.resolve_pr(repository, pr)? else {
            return Ok(None);
        };
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (raw,actual_branch,actual_pr,until,generation,progress):(String,String,Option<u64>,i64,i64,i64)=tx.query_row(
            "SELECT checkpoint,branch,pr,lease_until,generation,progress_at FROM workflow WHERE id=?1",[&id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        let mut cp: Checkpoint = serde_json::from_str(&raw)?;
        if cp.next.action == "await_merge_authorization" {
            return Ok(None);
        }
        if actual_branch != branch
            || actual_pr != Some(pr)
            || cp.binding.head != head
            || until > now
            || owner_control::read(&tx, &id)?.is_some_and(|c| c.paused)
            || owner_inbox::pending(&tx, &id)? > 0
        {
            return Ok(None);
        }
        let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state='dispatched')",[&id],|r|r.get(0))?;
        if pending {
            return Ok(None);
        }
        let old:Option<(String,String,String,String,u32,i64,String)>=tx.query_row(
            "SELECT binding,base,fingerprint,kind,attempts,next_at,original FROM integration_recovery WHERE workflow=?1",[&id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?))).optional()?;
        // Admission checkpoints resume_started before doing integration work.
        // A crash there must not hide the existing job behind today's ledger.
        // Only the persisted due job for this same source/base can restore the
        // integration cue. Current inputs remain bound by normal admission;
        // no historical review or CI evidence is restored.
        let interrupted = reason.due()
            && cp.stage == Stage::Reconcile
            && cp.next.reason == "resume_started"
            && cp.next.action == "reconcile_main"
            && old.as_ref().is_some_and(|row| {
                row.1 == base
                    && matches!(
                        row.3.as_str(),
                        "integration_conflict" | "integration_base_changed"
                    )
                    && serde_json::from_str::<Binding>(&row.0)
                        .is_ok_and(|binding| binding.head == cp.binding.head)
            });
        let ours =
            cp.next.reason.starts_with("integration_") || cp.next.reason == "mergeability_unknown";
        let admissible = ours
            || interrupted
            || (cp.stage == Stage::Reconcile
                && matches!(
                    cp.next.reason.as_str(),
                    "draft_parked" | "review_main_changed"
                ))
            || (cp.stage == Stage::Reconcile && cp.next.reason == "parent_merged_child_retargeted")
            || cp.stage == Stage::AwaitChecks
            || (cp.stage == Stage::Review && cp.next.reason == "review-recovery-required")
            || (cp.stage == Stage::Held
                && matches!(
                    cp.next.reason.as_str(),
                    "no-reviewer-capacity" | "reviewer-latched"
                ));
        if !admissible {
            return Ok(None);
        }
        if cp.next.reason == "integration_resolution_deferred"
            && cp.next.eligible_at_ms > now
            && old.as_ref().is_some_and(|o| o.1 == base)
        {
            return Ok(Some(false));
        }
        let binding = serde_json::to_string(&cp.binding)?;
        let same = old.as_ref().is_some_and(|o| o.0 == binding && o.1 == base);
        let unchanged = same
            && old
                .as_ref()
                .is_some_and(|o| o.2 == fingerprint && o.3 == reason.code());
        if unchanged
            && !interrupted
            && (reason != IntegrationReason::Unknown || old.as_ref().unwrap().5 > now)
        {
            return Ok(if reason == IntegrationReason::Resolved {
                None
            } else {
                Some(reason.due() && cp.next.eligible_at_ms <= now)
            });
        }
        if !ours && cp.next.eligible_at_ms > now {
            return Ok(None);
        }
        let original = old
            .as_ref()
            .filter(|o| {
                o.0 == binding
                    && (interrupted
                        || reason == IntegrationReason::Resolved
                        || (reason == IntegrationReason::Unknown && o.3 == "mergeability_unknown"))
            })
            .map_or(raw.clone(), |o| o.6.clone());
        let attempts = if reason == IntegrationReason::Unknown {
            if unchanged {
                old.as_ref().unwrap().4.saturating_add(1)
            } else {
                1
            }
        } else {
            0
        };
        let next_at = if reason == IntegrationReason::Unknown {
            now.saturating_add(
                300_000i64
                    .saturating_mul(1i64 << attempts.saturating_sub(1).min(4))
                    .min(3_600_000),
            )
        } else {
            0
        };
        if reason == IntegrationReason::Resolved {
            if !old.as_ref().is_some_and(|o| o.0 == binding) || !ours {
                return Ok(None);
            }
            cp = serde_json::from_str(&original)?;
        } else {
            cp.stage = if reason.due() {
                Stage::Reconcile
            } else {
                Stage::Held
            };
            cp.next = NextAction {
                reason: reason.code().into(),
                action: match reason {
                    IntegrationReason::Unknown => "retry_mergeability",
                    IntegrationReason::Conflict | IntegrationReason::BaseChanged => {
                        "reconcile_main"
                    }
                    IntegrationReason::ReviewerCapacity => "await_independent_reviewer",
                    IntegrationReason::Provenance => "await_provenance_adoption",
                    _ => "await_integration_parent",
                }
                .into(),
                eligible_at_ms: next_at,
            };
        }
        if reason.due() && !unchanged {
            // Old approvals/CI-ready milestones depend on the integrated base.
            // Append-only findings remain available; ownership/usage is retained.
            cp.evidence.clear();
            let floor: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq),0) FROM workflow_event WHERE workflow=?1",
                [&id],
                |r| r.get(0),
            )?;
            tx.execute("INSERT INTO integration_review_floor VALUES(?1,?2) ON CONFLICT(workflow) DO UPDATE SET after_seq=excluded.after_seq",params![id,floor])?;
            tx.execute("DELETE FROM review WHERE workflow=?1", [&id])?;
            tx.execute("DELETE FROM review_fragment WHERE workflow=?1", [&id])?;
            tx.execute("DELETE FROM pr_ready_receipt WHERE workflow=?1", [&id])?;
        }
        tx.execute("INSERT INTO integration_recovery VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(workflow) DO UPDATE SET binding=excluded.binding,base=excluded.base,fingerprint=excluded.fingerprint,kind=excluded.kind,attempts=excluded.attempts,next_at=excluded.next_at,original=excluded.original",
            params![id,binding,base,fingerprint,reason.code(),attempts,next_at,original])?;
        tx.execute(
            "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
            params![id, serde_json::to_string(&cp)?],
        )?;
        let previous:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind IN ('integration_recovery_scheduled','integration_recovery_changed') ORDER BY seq DESC LIMIT 1",[&id],|r|r.get(0)).optional()?;
        let previous = previous
            .map(|s| serde_json::from_str::<public::WorkflowEvent>(&s))
            .transpose()?;
        let stage: public::WorkflowStage = serde_json::from_value(serde_json::to_value(cp.stage)?)?;
        let sequence = tx.query_row(
            "SELECT COALESCE(MAX(seq),0)+1 FROM workflow_event WHERE workflow=?1",
            [&id],
            |r| r.get(0),
        )?;
        let target = if let Some(issue) = id
            .strip_prefix(&format!("{repository}#"))
            .and_then(|s| s.parse().ok())
        {
            public::WorkflowTarget::Issue(issue)
        } else {
            public::WorkflowTarget::PullRequest(pr)
        };
        let record = public::WorkflowEvent {
            schema_version: public::SCHEMA_VERSION,
            workflow_id: id.clone(),
            sequence,
            occurred_at_ms: now,
            repository: repository.into(),
            target,
            pull_request: Some(pr),
            binding: public::Binding {
                head: cp.binding.head.clone(),
                contract_version: cp.binding.contract.clone(),
                evidence_version: cp.binding.evidence_hash.clone(),
            },
            stage,
            outcome: if reason.due() || reason == IntegrationReason::Resolved {
                public::WorkflowOutcome::Progressed
            } else if reason == IntegrationReason::ReviewerCapacity {
                public::WorkflowOutcome::CapacityWait
            } else if reason == IntegrationReason::Unknown {
                public::WorkflowOutcome::RetryableInfrastructure
            } else {
                public::WorkflowOutcome::DependencyWait
            },
            transition: if previous.as_ref().is_some_and(|p| p.stage != stage) {
                public::ProgressTransition::StageChanged
            } else {
                public::ProgressTransition::Observation
            },
            reason_code: reason.code().into(),
            last_meaningful_progress_at_ms: previous
                .as_ref()
                .map_or(progress, |p| p.last_meaningful_progress_at_ms),
            next: public::NextAction {
                action: cp.next.action.clone(),
                eligible_at_ms: Some(cp.next.eligible_at_ms),
            },
            actor: public::Actor::Controller,
            origin: public::Origin::Reconciliation,
            compute_usage: tx.query_row(
                "SELECT model_calls,tokens,wall_ms FROM workflow WHERE id=?1",
                [&id],
                |r| {
                    Ok(public::ComputeUsage {
                        model_calls: r.get(0)?,
                        tokens: r.get(1)?,
                        wall_ms: r.get(2)?,
                    })
                },
            )?,
        };
        if let Some(previous) = previous {
            record.validate_after(&previous)?;
        } else {
            record.validate()?;
        }
        event(
            &tx,
            &id,
            generation,
            if same {
                "integration_recovery_changed"
            } else {
                "integration_recovery_scheduled"
            },
            &serde_json::to_string(&record)?,
            now,
        )?;
        tx.commit()?;
        Ok(if reason == IntegrationReason::Resolved {
            (cp.stage == Stage::Reconcile && cp.next.reason.starts_with("integration_"))
                .then_some(true)
        } else {
            Some(reason.due())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(path: &std::path::Path) -> WorkflowStore {
        let mut store = WorkflowStore::open(path).unwrap();
        let binding = Binding {
            head: "candidate".into(),
            contract: "contract".into(),
            evidence_hash: "tree".into(),
        };
        store
            .register("owner/repo#1", "candidate", binding.clone(), 1)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "publisher", 2, 100)
            .unwrap()
            .unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 1606, 3)
            .unwrap();
        store
            .record_review(
                &lease,
                &Review {
                    reviewer: "independent".into(),
                    binding: binding.clone(),
                    approved: true,
                    evidence: "old-base".into(),
                },
                4,
            )
            .unwrap();
        store
            .checkpoint(
                &lease,
                &Checkpoint {
                    stage: Stage::Reconcile,
                    binding,
                    evidence: vec![],
                    next: NextAction {
                        reason: "draft_parked".into(),
                        action: "resume".into(),
                        eligible_at_ms: 0,
                    },
                },
                5,
            )
            .unwrap();
        store.release(&lease, 6).unwrap();
        store
    }
    fn observe(
        store: &mut WorkflowStore,
        reason: IntegrationReason,
        base: &str,
        now: i64,
    ) -> Option<bool> {
        store
            .reconcile_integration(
                "owner/repo",
                1606,
                "candidate",
                "candidate",
                base,
                "fixture",
                reason,
                now,
            )
            .unwrap()
    }
    #[test]
    fn integration_metadata_retry_is_durable_deduplicated_and_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = fixture(&path);
        assert_eq!(
            observe(&mut store, IntegrationReason::Unknown, "base", 10),
            Some(false)
        );
        let events = store.events("owner/repo#1").unwrap();
        for now in [11, 12, 300_009] {
            assert_eq!(
                observe(&mut store, IntegrationReason::Unknown, "base", now),
                Some(false)
            );
        }
        assert_eq!(store.events("owner/repo#1").unwrap(), events);
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        let mut now = 300_010;
        for delay in [600_000, 1_200_000, 2_400_000, 3_600_000, 3_600_000] {
            assert_eq!(
                observe(&mut store, IntegrationReason::Unknown, "base", now),
                Some(false)
            );
            let cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
            assert_eq!(cp.next.eligible_at_ms, now + delay);
            now += delay;
        }
        assert_eq!(
            observe(&mut store, IntegrationReason::Resolved, "base", now),
            None
        );
        let snapshot = store.snapshot("owner/repo#1").unwrap().unwrap();
        assert_eq!(snapshot.checkpoint.next.reason, "draft_parked");
        assert_eq!(snapshot.model_calls, 0);
        assert_eq!(
            store.current_reviews("owner/repo#1").unwrap().len(),
            1,
            "metadata observation invalidated valid evidence"
        );
    }
    #[test]
    fn integration_failed_resolution_respects_backoff_even_after_tree_binding_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 10),
            Some(true)
        );
        let lease = store
            .claim("owner/repo#1", "resolver", 11, 100)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        cp.binding.evidence_hash = "unresolved-preserved-tree".into();
        cp.stage = Stage::Held;
        cp.next = NextAction {
            reason: "integration_resolution_deferred".into(),
            action: "inspect_integration_conflict".into(),
            eligible_at_ms: 3_600_020,
        };
        store.checkpoint(&lease, &cp, 12).unwrap();
        store.release(&lease, 13).unwrap();
        let events = store.events("owner/repo#1").unwrap();
        for now in [14, 1_000_000, 3_600_019] {
            assert_eq!(
                observe(&mut store, IntegrationReason::Conflict, "base", now),
                Some(false)
            );
        }
        assert_eq!(store.events("owner/repo#1").unwrap(), events);
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 3_600_020),
            Some(true)
        );
    }
    #[test]
    fn integration_unknown_after_conflict_keeps_reverification_and_capacity_can_wake() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 10),
            Some(true)
        );
        assert_eq!(
            observe(&mut store, IntegrationReason::Unknown, "base", 11),
            Some(false)
        );
        assert_eq!(
            observe(&mut store, IntegrationReason::Resolved, "base", 12),
            Some(true)
        );
        assert_eq!(
            store
                .snapshot("owner/repo#1")
                .unwrap()
                .unwrap()
                .checkpoint
                .next
                .reason,
            "integration_conflict"
        );
        assert!(store
            .current_review_history("owner/repo#1")
            .unwrap()
            .is_empty());
        assert!(store
            .defer_integration_review(
                "owner/repo",
                1606,
                "candidate",
                IntegrationReason::ReviewerCapacity,
                13
            )
            .unwrap());
        assert_eq!(
            observe(&mut store, IntegrationReason::ReviewerCapacity, "base", 14),
            Some(false)
        );
        let events = store.events("owner/repo#1").unwrap();
        assert_eq!(
            observe(&mut store, IntegrationReason::ReviewerCapacity, "base", 15),
            Some(false)
        );
        assert_eq!(store.events("owner/repo#1").unwrap(), events);
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 16),
            Some(true)
        );
        assert_eq!(
            store
                .snapshot("owner/repo#1")
                .unwrap()
                .unwrap()
                .checkpoint
                .next
                .reason,
            "integration_conflict"
        );
    }
    #[test]
    fn integration_changed_base_invalidates_evidence_once_without_stealing_live_lease() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        let lease = store
            .claim("owner/repo#1", "worker", 10, 100)
            .unwrap()
            .unwrap();
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 11),
            None
        );
        assert_eq!(store.current_reviews("owner/repo#1").unwrap().len(), 1);
        store.release(&lease, 12).unwrap();
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 13),
            Some(true)
        );
        assert!(store.current_reviews("owner/repo#1").unwrap().is_empty());
        assert!(store
            .current_review_history("owner/repo#1")
            .unwrap()
            .is_empty());
        assert!(
            store
                .events("owner/repo#1")
                .unwrap()
                .iter()
                .any(|(_, kind, _, _)| kind == "review_recorded"),
            "historical findings were deleted"
        );
        let events = store.events("owner/repo#1").unwrap();
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 14),
            Some(true)
        );
        assert_eq!(store.events("owner/repo#1").unwrap(), events);
        assert_eq!(
            observe(&mut store, IntegrationReason::BaseChanged, "new-base", 15),
            Some(true)
        );
        assert!(store.events("owner/repo#1").unwrap().len() > events.len());
        let snapshot = store.snapshot("owner/repo#1").unwrap().unwrap();
        assert_eq!(snapshot.checkpoint.next.reason, "integration_base_changed");
        assert_eq!(snapshot.model_calls, 0);
    }
    #[test]
    fn integration_crashed_resume_obeys_expiry_and_does_not_restore_approval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = fixture(&path);
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 10),
            Some(true)
        );
        let lease = store
            .claim_productive("owner/repo#1", "doomed", 100, 100)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "resume_started".into(),
            action: "reconcile_main".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 101).unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&path).unwrap();
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 199),
            None
        );
        assert_eq!(
            store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint,
            cp
        );
        assert_eq!(
            observe(&mut store, IntegrationReason::Conflict, "base", 201),
            Some(true)
        );
        let restored = store.snapshot("owner/repo#1").unwrap().unwrap();
        assert_eq!(restored.checkpoint.next.reason, "integration_conflict");
        assert_eq!(restored.model_calls, 0);
        assert!(store.current_reviews("owner/repo#1").unwrap().is_empty());
        let next = store
            .claim_productive("owner/repo#1", "replacement", 202, 100)
            .unwrap()
            .unwrap();
        assert_ne!(next.generation, lease.generation);
        assert!(
            store.checkpoint(&lease, &cp, 203).is_err(),
            "dead executor regained write authority"
        );
    }

    #[test]
    fn integration_crashed_resume_keeps_current_controls_and_source_binding() {
        for case in [
            "no-job",
            "unknown-job",
            "paused",
            "pending-push",
            "new-head",
            "scope-hold",
            "changed-base",
            "new-inputs",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut store = fixture(&dir.path().join("workflow.db"));
            if case != "no-job" {
                observe(
                    &mut store,
                    if case == "unknown-job" {
                        IntegrationReason::Unknown
                    } else {
                        IntegrationReason::Conflict
                    },
                    "base",
                    10,
                );
            }
            let lease = store
                .claim("owner/repo#1", "doomed", 300_011, 100)
                .unwrap()
                .unwrap();
            let mut cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
            cp.stage = Stage::Reconcile;
            cp.next = NextAction {
                reason: "resume_started".into(),
                action: "reconcile_main".into(),
                eligible_at_ms: 0,
            };
            if case == "new-head" {
                cp.binding.head = "owner-updated-head".into();
            }
            if case == "new-inputs" {
                cp.binding.evidence_hash = "new-environment".into();
                cp.binding.contract = "updated-contract".into();
            }
            if case == "scope-hold" {
                cp.stage = Stage::Scope;
                cp.next.reason = "acceptance_contract_invalid".into();
                cp.next.action = "await_acceptance_input_change".into();
            }
            store.checkpoint(&lease, &cp, 300_012).unwrap();
            if case == "paused" {
                store
                    .set_owner_pause("owner/repo#1", "github:owner", 0, true, 300_013)
                    .unwrap();
            }
            if case == "pending-push" {
                store
                    .begin_operation(&lease, OperationKind::Push, &cp.binding, 300_013)
                    .unwrap();
            }
            let before = store.snapshot("owner/repo#1").unwrap().unwrap();
            let head = cp.binding.head.clone();
            let result = store
                .reconcile_integration(
                    "owner/repo",
                    1606,
                    "candidate",
                    &head,
                    if case == "changed-base" {
                        "new-base"
                    } else {
                        "base"
                    },
                    "fixture",
                    IntegrationReason::Conflict,
                    300_112,
                )
                .unwrap();
            assert_eq!(
                result,
                if case == "new-inputs" {
                    Some(true)
                } else {
                    None
                },
                "{case}"
            );
            if case != "new-inputs" {
                assert_eq!(
                    store.snapshot("owner/repo#1").unwrap().unwrap(),
                    before,
                    "{case}"
                );
            } else {
                let resumed = store.snapshot("owner/repo#1").unwrap().unwrap();
                assert_eq!(resumed.checkpoint.binding, cp.binding);
                assert!(resumed.checkpoint.evidence.is_empty());
                assert!(store.current_reviews("owner/repo#1").unwrap().is_empty());
            }
        }
    }

    #[test]
    fn integration_admission_hint_requires_current_source_and_checkpoint_without_claiming() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = fixture(&dir.path().join("workflow.db"));
        assert!(!store
            .integration_admission_interrupted("owner/repo#1", "candidate", "base")
            .unwrap());
        assert_eq!(
            observe(&mut store, IntegrationReason::BaseChanged, "base", 10),
            Some(true)
        );
        assert!(!store
            .integration_admission_interrupted("owner/repo#1", "candidate", "base")
            .unwrap());
        let lease = store
            .claim_productive("owner/repo#1", "live-worker", 11, 100)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "resume_started".into(),
            action: "reconcile_main".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 12).unwrap();
        assert!(store
            .integration_admission_interrupted("owner/repo#1", "candidate", "base")
            .unwrap());
        assert!(!store
            .integration_admission_interrupted("owner/repo#1", "owner-new-head", "base")
            .unwrap());
        assert!(!store
            .integration_admission_interrupted("owner/repo#1", "candidate", "new-base")
            .unwrap());
        assert_eq!(
            observe(&mut store, IntegrationReason::BaseChanged, "base", 13),
            None,
            "a routing hint bypassed the live worker"
        );
        assert_eq!(
            store.snapshot("owner/repo#1").unwrap().unwrap().checkpoint,
            cp
        );
        cp.stage = Stage::Scope;
        cp.next.reason = "acceptance_contract_invalid".into();
        store.checkpoint(&lease, &cp, 14).unwrap();
        assert!(!store
            .integration_admission_interrupted("owner/repo#1", "candidate", "base")
            .unwrap());
    }
}

/// Read-only stores from before integration scheduling have no floor yet.
pub(super) fn review_floor(conn: &Connection, workflow: &str) -> Result<i64> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='integration_review_floor')",[],|r|r.get(0))?;
    if !exists {
        return Ok(0);
    }
    Ok(conn
        .query_row(
            "SELECT after_seq FROM integration_review_floor WHERE workflow=?1",
            [workflow],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}
