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
                // #2370: a declined amendment is a planning failure too.
                | "acceptance_verifier_amendment_declined"
                | "acceptance_contract_invalid"
                | "acceptance_failed"
                // #2289: the old terminal summary state is never permanent.
                | "acceptance_strategies_exhausted"
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
/// Cycle-0 escalation material: the original hold, bounded private
/// diagnostics and the receipted original criteria. Nothing is relaxed.
fn start_material(tx: &Connection, id: &str, cp: &Checkpoint) -> Result<Material> {
    let mut q=tx.prepare("SELECT key,response FROM scope_stage_response WHERE workflow=?1 AND json_valid(response) AND (json_type(response,'$.detail')='text' OR json_type(response,'$.diagnostic')='text') ORDER BY rowid DESC LIMIT 12")?;
    let rows = q.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
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
    Ok(Material {
        version: 1,
        original: cp.clone(),
        diagnostics,
        criteria: retained_criteria(tx, id, &cp.binding)?,
    })
}

/// #2317 one-time audit tag for holds parked before #2301.
pub(crate) const LADDER_MIGRATION: &str = "repair-exhausted-ladder-v1";
/// #2370 one-time audit tag for declined-amendment holds.
pub(crate) const AMENDMENT_LADDER_MIGRATION: &str = "amendment-declined-ladder-v1";
/// Bounded per sweep, like #2274's scope-policy migration.
const LADDER_MIGRATION_BATCH: usize = 8;

impl WorkflowStore {
    /// Free worker maintenance (#2317). A repair-exhausted hold parked before
    /// #2301 still waits on `await_acceptance_input_change`, which nothing will
    /// ever supply. Enter it into the never-give-up ladder at cycle 0 with an
    /// audit event holding the complete prior checkpoint. Binding, evidence,
    /// generation, attempts, spend, owner control and leases are untouched;
    /// this grants planning eligibility only, never acceptance. #2370: a
    /// declined-amendment hold is the same terminal park and migrates too.
    pub(crate) fn migrate_repair_exhausted_to_ladder(
        &mut self,
        repository: &str,
        now: i64,
    ) -> Result<usize> {
        let prefix = format!("{}#", repository.to_ascii_lowercase());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows: Vec<(String, String, i64)> = tx
            .prepare(
                "SELECT id,checkpoint,generation FROM workflow WHERE pr IS NULL AND lease_until<=?1
                 AND substr(id,1,length(?2))=?2 AND json_valid(checkpoint)
                 AND json_extract(checkpoint,'$.stage')='scope'
                 AND json_extract(checkpoint,'$.next.reason') IN ('acceptance_verifier_repair_exhausted','acceptance_verifier_amendment_declined')
                 AND json_extract(checkpoint,'$.next.action')='await_acceptance_input_change'
                 ORDER BY heartbeat_at,id LIMIT 64",
            )?
            .query_map(params![now, prefix], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut migrated = 0;
        for (id, raw, generation) in rows {
            // A malformed row keeps its typed per-issue hold; peers continue.
            let Ok(cp) = serde_json::from_str::<Checkpoint>(&raw) else {
                continue;
            };
            if owner_control::require_dispatch(&tx, &id).is_err()
                || target_fence::fenced(&tx, &id)?
                || !eligible(&tx, &id, &cp).unwrap_or(false)
            {
                continue;
            }
            let material = match start_material(&tx, &id, &cp) {
                Ok(material) => material,
                Err(error) => {
                    tracing::warn!(workflow=%id,error=%error,"repair-exhausted evidence unavailable; preserve hold and continue peers");
                    continue;
                }
            };
            let migration = if cp.next.reason == "acceptance_verifier_amendment_declined" {
                AMENDMENT_LADDER_MIGRATION
            } else {
                LADDER_MIGRATION
            };
            let audit = serde_json::json!({"version":1,"migration":migration,
                "prior_checkpoint":serde_json::to_string(&cp)?,"original_evidence":"preserved"});
            event(&tx, &id, generation, "acceptance_ladder_migrated", &audit.to_string(), now)?;
            event(
                &tx,
                &id,
                generation,
                "acceptance_strategy_started",
                &serde_json::to_string(&material)?,
                now,
            )?;
            let mut next = cp;
            next.next = NextAction {
                reason: "acceptance_strategy_escalation".into(),
                action: ACTION.into(),
                eligible_at_ms: now,
            };
            let next = serde_json::to_string(&next)?;
            tx.execute(
                "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
                params![id, next],
            )?;
            event(&tx, &id, generation, "checkpoint", &next, now)?;
            migrated += 1;
            if migrated == LADDER_MIGRATION_BATCH {
                break;
            }
        }
        tx.commit()?;
        Ok(migrated)
    }
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
            let m = start_material(&tx, &lease.workflow, &cp)?;
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
    fn legacy_terminal_strategy_exhaustion_is_readmitted_for_the_next_cycle() {
        // #2289: a row parked by the earlier terminal summary state is never
        // permanent; normal acquisition admits it for the next strategy cycle.
        let (_dir, mut store, id, binding) = held("acceptance_verifier_repair_exhausted");
        let lease = store.claim(&id, "legacy", 5, 100).unwrap().unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.next = NextAction {
            reason: "acceptance_strategies_exhausted".into(),
            action: "await_new_execution_inputs".into(),
            eligible_at_ms: 0,
        };
        store.checkpoint(&lease, &cp, 6).unwrap();
        store.release(&lease, 7).unwrap();
        assert!(store.acceptance_escalation_pending(&id).unwrap());
        assert!(store
            .claim_productive_for_binding(&id, "next-cycle", 8, 100, &binding)
            .unwrap()
            .is_some());
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
    fn park(store: &mut WorkflowStore, id: &str, reason: &str, at: i64) -> Binding {
        let binding = Binding {
            head: "b".repeat(40),
            contract: format!("original owner request {id}"),
            evidence_hash: "evidence".into(),
        };
        store.register(id, "candidate", binding.clone(), at).unwrap();
        let lease = store.claim(id, "first", at + 1, 100).unwrap().unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction {
            reason: reason.into(),
            action: "await_acceptance_input_change".into(),
            eligible_at_ms: 0,
        };
        cp.evidence = vec!["original-diagnostic".into()];
        store.checkpoint(&lease, &cp, at + 2).unwrap();
        store.release(&lease, at + 3).unwrap();
        binding
    }
    #[test]
    fn legacy_repair_exhausted_rows_migrate_once_into_the_strategy_ladder_with_audit() {
        // #2317: rows parked before #2301 still sat at the terminal-looking
        // await_acceptance_input_change action and were never selected.
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#1615".to_string();
        let binding = park(&mut store, &id, "acceptance_verifier_repair_exhausted", 1);
        let paused = "owner/repo#1898".to_string();
        park(&mut store, &paused, "acceptance_verifier_repair_exhausted", 1);
        let control = store.owner_control(&paused).unwrap().map_or(0, |c| c.revision);
        store.set_owner_pause(&paused, "owner", control, true, 5).unwrap();
        let other_repo = "else/repo#1".to_string();
        park(&mut store, &other_repo, "acceptance_verifier_repair_exhausted", 1);
        let unrelated = "owner/repo#2".to_string();
        park(&mut store, &unrelated, "acceptance_contract_invalid", 1);
        let before = store.snapshot(&id).unwrap().unwrap();
        let untouched: Vec<_> = [&paused, &other_repo, &unrelated]
            .iter()
            .map(|w| (store.snapshot(w).unwrap().unwrap(), store.events(w).unwrap()))
            .collect();

        assert_eq!(store.migrate_repair_exhausted_to_ladder("Owner/Repo", 50).unwrap(), 1);
        let after = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.action, ACTION);
        assert_eq!(after.checkpoint.next.reason, "acceptance_strategy_escalation");
        assert_eq!(after.checkpoint.next.eligible_at_ms, 50);
        assert!(!awaits_initial_input(&after.checkpoint), "migrated row still looks parked");
        // Only scheduling moved: binding, evidence, generation, attempts and spend stay.
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
        assert_eq!(
            (after.generation, after.attempts, after.model_calls, after.progress_at_ms),
            (before.generation, before.attempts, before.model_calls, before.progress_at_ms)
        );
        let events = store.events(&id).unwrap();
        let audit = events
            .iter()
            .find(|(_, kind, _, _)| kind == "acceptance_ladder_migrated")
            .expect("migration must leave an audit event");
        let audit: serde_json::Value = serde_json::from_str(&audit.2).unwrap();
        let prior: Checkpoint =
            serde_json::from_str(audit["prior_checkpoint"].as_str().unwrap()).unwrap();
        assert_eq!(prior, before.checkpoint, "audit lost the original hold");
        assert!(events.iter().any(|(_, kind, _, _)| kind == "acceptance_strategy_started"));
        // Peers outside scope are left exactly as they were.
        for (w, (snapshot, history)) in [&paused, &other_repo, &unrelated].iter().zip(untouched) {
            assert_eq!(store.snapshot(w).unwrap().unwrap(), snapshot, "{w} changed");
            assert_eq!(store.events(w).unwrap(), history, "{w} history changed");
        }
        // One-time: a second sweep finds nothing to do and writes nothing.
        let settled = store.events(&id).unwrap();
        assert_eq!(store.migrate_repair_exhausted_to_ladder("owner/repo", 60).unwrap(), 0);
        assert_eq!(store.events(&id).unwrap(), settled);

        // The migrated row is selectable and enters cycle 0 of the ladder
        // with its original hold as the escalation material.
        assert!(store.acceptance_escalation_retries("owner/repo").unwrap().contains(&1615));
        let lease = store
            .claim_productive_for_binding(&id, "ladder", 70, 100, &binding)
            .unwrap()
            .expect("migrated ladder row was not claimable");
        let material = store
            .begin_acceptance_escalation(&lease, &binding, 71)
            .unwrap()
            .expect("migrated row lost its ladder material");
        assert_eq!(material.original, before.checkpoint);
        let started = store
            .events(&id)
            .unwrap()
            .iter()
            .filter(|(_, kind, _, _)| kind == "acceptance_strategy_started")
            .count();
        assert_eq!(started, 1, "claim restarted the strategy instead of resuming it");
    }
    #[test]
    fn declined_verifier_amendment_enters_the_strategy_ladder_not_an_issue_edit_park() {
        // #2370 (nolanmak/jarvis#2361): a declined amendment parked forever on
        // await_acceptance_input_change. It is a planning failure like
        // repair exhaustion and must enter #2301's ladder on the next claim.
        let (_dir, mut store, id, binding) = held("acceptance_verifier_amendment_declined");
        let before = store.snapshot(&id).unwrap().unwrap();
        assert!(store.acceptance_escalation_pending(&id).unwrap(), "declined amendment is not ladder-eligible");
        assert!(store.acceptance_escalation_retries("owner/repo").unwrap().contains(&1766));
        let lease = store
            .claim_productive_for_binding(&id, "ladder", 5, 100, &binding)
            .unwrap()
            .expect("declined amendment still waits for a human issue edit");
        let material = store
            .begin_acceptance_escalation(&lease, &binding, 6)
            .unwrap()
            .expect("declined amendment did not start a strategy cycle");
        assert_eq!(material.original, before.checkpoint);
        let after = store.snapshot(&id).unwrap().unwrap().checkpoint;
        assert_eq!((after.next.reason.as_str(), after.next.action.as_str()), ("acceptance_strategy_escalation", ACTION));
        assert_eq!((after.binding, after.evidence), (before.checkpoint.binding, before.checkpoint.evidence));
    }
    #[test]
    fn existing_declined_amendment_rows_migrate_once_into_the_ladder_with_audit() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#2361".to_string();
        let binding = park(&mut store, &id, "acceptance_verifier_amendment_declined", 1);
        let paused = "owner/repo#2362".to_string();
        park(&mut store, &paused, "acceptance_verifier_amendment_declined", 1);
        let control = store.owner_control(&paused).unwrap().map_or(0, |c| c.revision);
        store.set_owner_pause(&paused, "owner", control, true, 5).unwrap();
        let held_before = (store.snapshot(&paused).unwrap().unwrap(), store.events(&paused).unwrap());
        let before = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(store.migrate_repair_exhausted_to_ladder("owner/repo", 50).unwrap(), 1);
        let after = store.snapshot(&id).unwrap().unwrap();
        assert_eq!((after.checkpoint.next.reason.as_str(), after.checkpoint.next.action.as_str()), ("acceptance_strategy_escalation", ACTION));
        assert!(!awaits_initial_input(&after.checkpoint));
        assert_eq!((after.generation, after.attempts, after.model_calls), (before.generation, before.attempts, before.model_calls));
        let audit = store.events(&id).unwrap().into_iter()
            .find(|(_, kind, _, _)| kind == "acceptance_ladder_migrated")
            .expect("migration must leave an audit event");
        let audit: serde_json::Value = serde_json::from_str(&audit.2).unwrap();
        assert_eq!(audit["migration"], AMENDMENT_LADDER_MIGRATION);
        let prior: Checkpoint = serde_json::from_str(audit["prior_checkpoint"].as_str().unwrap()).unwrap();
        assert_eq!(prior, before.checkpoint, "audit lost the original hold");
        // The owner's explicit pause is legitimate and stays untouched.
        assert_eq!((store.snapshot(&paused).unwrap().unwrap(), store.events(&paused).unwrap()), held_before);
        let settled = store.events(&id).unwrap();
        assert_eq!(store.migrate_repair_exhausted_to_ladder("owner/repo", 60).unwrap(), 0);
        assert_eq!(store.events(&id).unwrap(), settled);
        let lease = store.claim_productive_for_binding(&id, "ladder", 70, 100, &binding).unwrap().unwrap();
        let material = store.begin_acceptance_escalation(&lease, &binding, 71).unwrap().unwrap();
        assert_eq!(material.original, before.checkpoint);
    }
    #[test]
    fn ladder_migration_leaves_leased_and_published_rows_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let leased = "owner/repo#10".to_string();
        park(&mut store, &leased, "acceptance_verifier_repair_exhausted", 1);
        let lease = store.claim(&leased, "busy", 10, 1_000).unwrap().unwrap();
        let published = "owner/repo#11".to_string();
        park(&mut store, &published, "acceptance_verifier_repair_exhausted", 1);
        let pub_lease = store.claim(&published, "pub", 10, 100).unwrap().unwrap();
        store.attach_pr(&pub_lease, 2135, 11).unwrap();
        store.release(&pub_lease, 12).unwrap();
        let before: Vec<_> = [&leased, &published]
            .iter()
            .map(|w| store.snapshot(w).unwrap().unwrap())
            .collect();
        assert_eq!(store.migrate_repair_exhausted_to_ladder("owner/repo", 20).unwrap(), 0);
        for (w, snapshot) in [&leased, &published].iter().zip(before) {
            assert_eq!(store.snapshot(w).unwrap().unwrap(), snapshot, "{w} changed");
        }
        store.release(&lease, 30).unwrap();
        assert_eq!(store.migrate_repair_exhausted_to_ladder("owner/repo", 40).unwrap(), 1);
    }
}
