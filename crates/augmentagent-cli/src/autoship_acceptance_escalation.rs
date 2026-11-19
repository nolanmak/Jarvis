//! Bounded planning recovery for failed acceptance, never acceptance approval.
use super::*;

pub(crate) const ACTION: &str = "escalate_acceptance";
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Material {
    pub version: u32,
    pub original: Checkpoint,
    pub diagnostics: Vec<serde_json::Value>,
    pub criteria: Vec<String>,
}
fn target(cp: &Checkpoint) -> bool {
    matches!(cp.stage, Stage::Scope | Stage::Build | Stage::Verify)
        && (matches!(
            cp.next.reason.as_str(),
            "acceptance_verifier_repair_exhausted"
                | "acceptance_contract_invalid"
                | "acceptance_failed"
        ) || cp.next.action == ACTION)
}
pub(super) fn eligible(c: &Connection, id: &str, cp: &Checkpoint) -> Result<bool> {
    if !target(cp) {
        return Ok(false);
    }
    let pr: Option<u64> = c.query_row("SELECT pr FROM workflow WHERE id=?1", [id], |r| r.get(0))?;
    Ok(pr.is_none()
        && !qualification_recovery::pending_mutation(c, id)?
        && baseline_native_custody(c, id).is_ok())
}
fn retained_criteria(c: &Connection, id: &str, binding: &Binding) -> Result<Vec<String>> {
    // Raw model replies share this cache. Only controller-recorded verifier
    // evidence, authenticated by its immutable receipt, supplies requirements.
    let mut q = c.prepare(
        "SELECT s.response,json_extract(e.payload,'$.sha256')
         FROM scope_stage_response s JOIN workflow_event e ON e.workflow=s.workflow
         WHERE s.workflow=?1 AND e.kind='verifier_evidence_recorded'
         AND CASE WHEN json_valid(e.payload) THEN
             json_extract(e.payload,'$.key')=s.key
             AND json_extract(e.payload,'$.kind')='original_verifier_plan' ELSE 0 END
         AND CASE WHEN json_valid(s.response) THEN
             json_extract(s.response,'$.kind')='original_verifier_plan'
             AND json_extract(s.response,'$.binding.head')=?2
             AND json_extract(s.response,'$.binding.contract')=?3
             AND json_extract(s.response,'$.binding.evidence_hash')=?4 ELSE 0 END",
    )?;
    let rows = q.query_map(
        params![id, binding.head, binding.contract, binding.evidence_hash],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )?;
    let mut criteria = Vec::new();
    for row in rows {
        let (raw, digest) = row?;
        ensure!(
            format!("{:x}", Sha256::digest(raw.as_bytes())) == digest,
            "original verifier plan receipt mismatch"
        );
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        if let Some(items) = value["proposal"]["contract"]["criteria"].as_array() {
            for item in items {
                let requirement = item["requirement"]
                    .as_str()
                    .context("original verifier criterion missing")?;
                ensure!(
                    requirement.len() <= 32 * 1024 && criteria.len() < 8192,
                    "original criteria exceed evidence envelope"
                );
                if !criteria.iter().any(|v| v == requirement) {
                    criteria.push(requirement.to_owned());
                }
            }
        }
    }
    Ok(criteria)
}
impl WorkflowStore {
    pub(crate) fn acceptance_escalation_pending(&self, id: &str) -> Result<bool> {
        let Some(s) = self.snapshot(id)? else {
            return Ok(false);
        };
        eligible(&self.conn, id, &s.checkpoint)
    }
    pub(crate) fn acceptance_escalation_retries(
        &self,
        repository: &str,
    ) -> Result<std::collections::BTreeSet<u64>> {
        let prefix = format!("{}#", repository.to_ascii_lowercase());
        let mut q = self
            .conn
            .prepare("SELECT id FROM workflow WHERE pr IS NULL")?;
        let ids = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out = std::collections::BTreeSet::new();
        for id in ids {
            if self.acceptance_escalation_pending(&id).unwrap_or(false) {
                if let Some(n) = id.strip_prefix(&prefix).and_then(|n| n.parse().ok()) {
                    out.insert(n);
                }
            }
        }
        Ok(out)
    }
    /// New planning is not a new verifier budget or a recovered native outcome.
    /// Preserve the original checkpoint and immutable failure records verbatim.
    pub(crate) fn begin_acceptance_escalation(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        now: i64,
    ) -> Result<Option<Material>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_lease(&tx, lease, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        let raw: String = tx.query_row(
            "SELECT checkpoint FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| r.get(0),
        )?;
        let mut cp: Checkpoint = serde_json::from_str(&raw)?;
        ensure!(
            cp.binding == *binding,
            "acceptance escalation binding changed"
        );
        if !eligible(&tx, &lease.workflow, &cp)? {
            return Ok(None);
        }
        let material = if cp.next.action == ACTION {
            let raw:String=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='acceptance_strategy_started' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|r|r.get(0))?;
            let m: Material = serde_json::from_str(&raw)?;
            ensure!(
                m.version == 1
                    && m.original.binding == cp.binding
                    && m.original.evidence == cp.evidence,
                "acceptance escalation evidence changed"
            );
            m
        } else {
            let mut q=tx.prepare("SELECT key,response FROM scope_stage_response WHERE workflow=?1 AND json_valid(response) AND (json_type(response,'$.detail')='text' OR json_type(response,'$.diagnostic')='text') ORDER BY rowid DESC LIMIT 12")?;
            let rows = q.query_map([&lease.workflow], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut diagnostics = Vec::new();
            for row in rows {
                let (key, raw) = row?;
                let value: serde_json::Value = serde_json::from_str(&raw)?;
                let detail = value["detail"]
                    .as_str()
                    .or(value["diagnostic"].as_str())
                    .unwrap_or("");
                diagnostics.push(serde_json::json!({"key":key,"sha256":format!("{:x}",Sha256::digest(raw.as_bytes())),"preview":detail.chars().take(8192).collect::<String>()}));
            }
            let m = Material {
                version: 1,
                original: cp.clone(),
                diagnostics,
                criteria: retained_criteria(&tx, &lease.workflow, &cp.binding)?,
            };
            event(
                &tx,
                &lease.workflow,
                lease.generation,
                "acceptance_strategy_started",
                &serde_json::to_string(&m)?,
                now,
            )?;
            m
        };
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: "acceptance_strategy_escalation".into(),
            action: ACTION.into(),
            eligible_at_ms: now,
        };
        tx.execute(
            "UPDATE workflow SET checkpoint=?2,progress_at=?3 WHERE id=?1",
            params![lease.workflow, serde_json::to_string(&cp)?, now],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "checkpoint",
            &serde_json::to_string(&cp)?,
            now,
        )?;
        tx.commit()?;
        Ok(Some(material))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn held(reason: &str) -> (tempfile::TempDir, WorkflowStore, String, Binding) {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#1766".to_string();
        let binding = Binding {
            head: "a".repeat(40),
            contract: "original owner request".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register(&id, "candidate", binding.clone(), 1)
            .unwrap();
        let lease = store.claim(&id, "first", 2, 100).unwrap().unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: reason.into(),
            action: "await_acceptance_input_change".into(),
            eligible_at_ms: 0,
        };
        cp.evidence = vec!["original-diagnostic".into()];
        store.checkpoint(&lease, &cp, 3).unwrap();
        store.release(&lease, 4).unwrap();
        (dir, store, id, binding)
    }
    #[test]
    fn exhausted_and_invalid_acceptance_automatically_admit_planning_without_issue_edits() {
        for reason in [
            "acceptance_verifier_repair_exhausted",
            "acceptance_contract_invalid",
            "acceptance_failed",
        ] {
            let (_dir, mut store, id, binding) = held(reason);
            let before = store.snapshot(&id).unwrap().unwrap();
            let lease = store
                .claim_productive_for_binding(&id, "recovery", 5, 100, &binding)
                .unwrap();
            assert!(
                lease.is_some(),
                "{reason} still requires a human issue edit"
            );
            let after = store.snapshot(&id).unwrap().unwrap();
            assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
            assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
            assert_eq!(after.model_calls, 0);
        }
    }
    #[test]
    fn restart_keeps_original_diagnostics_evidence_and_charges() {
        let (dir, mut store, id, binding) = held("acceptance_verifier_repair_exhausted");
        let lease = store
            .claim_productive_for_binding(&id, "planning", 5, 100, &binding)
            .unwrap()
            .unwrap();
        store
            .record_scope_stage_response(
                &lease,
                &binding,
                &"f".repeat(64),
                &serde_json::json!({"detail":"E0308: tuple versus PendingActionRow"}).to_string(),
                6,
            )
            .unwrap();
        store.record_verifier_evidence(&lease,&binding,&"e".repeat(64),"original_verifier_plan",&serde_json::json!({"version":1,"workflow":id,"kind":"original_verifier_plan","binding":binding,"proposal":{"contract":{"criteria":[{"requirement":"Retain structural mention safeguards"}]}}}).to_string(),6).unwrap();
        // Raw completed model text can imitate a plan's JSON shape but cannot
        // mint the controller's verifier_evidence_recorded custody receipt.
        store.record_scope_stage_response(&lease,&binding,&"d".repeat(64),&serde_json::json!({"version":1,"workflow":id,"kind":"original_verifier_plan","binding":binding,"proposal":{"contract":{"criteria":[{"requirement":"MODEL_ADDED_REQUIREMENT"}]}}}).to_string(),6).unwrap();
        store
            .charge(
                &lease,
                "original-verifiers",
                ResourceCharge {
                    model_calls: 3,
                    tokens: 111,
                    wall_ms: 9000,
                },
                6,
            )
            .unwrap();
        let first = store
            .begin_acceptance_escalation(&lease, &binding, 7)
            .unwrap()
            .unwrap();
        assert!(serde_json::to_string(&first.diagnostics)
            .unwrap()
            .contains("PendingActionRow"));
        assert_eq!(first.original.evidence, vec!["original-diagnostic"]);
        assert_eq!(first.criteria, vec!["Retain structural mention safeguards"]);
        store.release(&lease, 8).unwrap();
        drop(store);
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let next = store
            .claim_productive_for_binding(&id, "restart", 9, 100, &binding)
            .unwrap()
            .unwrap();
        assert!(
            store
                .begin_acceptance_escalation(&lease, &binding, 10)
                .is_err(),
            "late old lease changed planning"
        );
        let second = store
            .begin_acceptance_escalation(&next, &binding, 10)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(first).unwrap(),
            serde_json::to_value(second).unwrap()
        );
        let saved = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(
            (saved.model_calls, saved.tokens, saved.wall_ms),
            (3, 111, 9000)
        );
    }
    #[test]
    fn pause_publication_and_unknown_native_outcome_still_fence_planning() {
        for fault in ["pause", "publication", "native"] {
            let (_dir, mut store, id, binding) = held("acceptance_contract_invalid");
            if fault == "pause" {
                store
                    .set_owner_pause(&id, "github:owner", 0, true, 5)
                    .unwrap();
            }
            if fault == "publication" {
                let lease = store.claim(&id, "publisher", 5, 100).unwrap().unwrap();
                store
                    .begin_operation(&lease, OperationKind::Push, &binding, 6)
                    .unwrap();
                store.release(&lease, 7).unwrap();
            }
            if fault == "native" {
                store.conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,1,'native_model_admitted',?2,5)",params![id,serde_json::json!({"bounded":{"purpose":format!("{}{}",BASELINE_NATIVE_PURPOSE_PREFIX,"f".repeat(64)),"limit":1},"reviewer":true,"provider":"codex"}).to_string()]).unwrap();
            }
            assert!(
                store
                    .claim_productive_for_binding(&id, "unsafe", 10, 100, &binding)
                    .unwrap()
                    .is_none(),
                "{fault} bypassed"
            );
        }
    }
    #[test]
    fn input_wait_needs_no_policy_flag_and_enrolled_prs_are_excluded() {
        let (_dir, mut store, id, binding) = held("acceptance_contract_invalid");
        assert!(store
            .acceptance_escalation_retries("owner/repo")
            .unwrap()
            .contains(&1766));
        let lease = store.claim(&id, "attach", 5, 100).unwrap().unwrap();
        store.attach_pr(&lease, 42, 6).unwrap();
        store.release(&lease, 7).unwrap();
        assert!(!store.acceptance_escalation_pending(&id).unwrap());
        assert!(store
            .claim_productive_for_binding(&id, "unsafe", 8, 100, &binding)
            .unwrap()
            .is_none());
    }
    #[test]
    fn scheduled_capacity_and_retry_waits_honor_the_clock_and_export_typed_outcomes() {
        use augmentagent_channel_core::autoship_workflow_contract::WorkflowOutcome;
        for (reason, outcome) in [
            (
                "acceptance_strategy_reviewer_capacity",
                WorkflowOutcome::CapacityWait,
            ),
            (
                "acceptance_strategy_retry",
                WorkflowOutcome::RetryableInfrastructure,
            ),
            (
                "acceptance_summary_retry",
                WorkflowOutcome::RetryableInfrastructure,
            ),
        ] {
            let (_dir, mut store, id, binding) = held("acceptance_contract_invalid");
            let lease = store
                .claim_productive_for_binding(&id, "first", 5, 100, &binding)
                .unwrap()
                .unwrap();
            store
                .begin_acceptance_escalation(&lease, &binding, 6)
                .unwrap()
                .unwrap();
            let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
            cp.next = NextAction {
                reason: reason.into(),
                action: ACTION.into(),
                eligible_at_ms: 1000,
            };
            store.checkpoint(&lease, &cp, 7).unwrap();
            store.release(&lease, 8).unwrap();
            assert!(store
                .claim_productive_for_binding(&id, "early", 999, 100, &binding)
                .unwrap()
                .is_none());
            assert_eq!(
                store.activity_status(&id).unwrap().unwrap().outcome,
                outcome
            );
            assert!(store
                .claim_productive_for_binding(&id, "resumed", 1000, 100, &binding)
                .unwrap()
                .is_some());
        }
    }
}
