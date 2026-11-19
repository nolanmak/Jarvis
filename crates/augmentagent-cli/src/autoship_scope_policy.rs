//! One-time scheduling re-evaluation of model-authored scope holds. This never
//! replaces checkout custody, original verifier evidence, owner control or leases.
use super::*;
pub(crate) const POLICY: &str = "safe-scope-repair-v1";
/// v6 (#2289): readiness never parks for a decision or on validation
/// exhaustion. The bump changes the verifier environment identity, so holds
/// parked under v5 are re-evaluated without issue edits.
pub(crate) const READINESS_POLICY: &str = "readiness-never-park-v6";
fn old_policy(conn: &Connection, id: &str, cp: &Checkpoint) -> Result<bool> {
    Ok(
        qualification_schedule::context(conn, id, &cp.binding)?.is_some_and(|context| {
            context
                .environment
                .contains("readiness-policy:readiness-grounding-")
                && !context
                    .environment
                    .contains(&format!("readiness-policy:{READINESS_POLICY}"))
        }),
    )
}
fn target(cp: &Checkpoint) -> bool {
    cp.stage == Stage::Scope
        && matches!(
            cp.next.reason.as_str(),
            "material_external_decision_missing"
                | "acceptance_contract_invalid"
                | "readiness_assessment_invalid"
        )
}
fn enrolled(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='scope_policy_reconsidered' AND CASE WHEN json_valid(payload) THEN json_extract(payload,'$.policy')=?2 ELSE 0 END)",params![id,POLICY],|r|r.get(0))?)
}
pub(crate) fn fresh_epoch(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND kind='scope_policy_reconsidered' AND CASE WHEN json_valid(payload) THEN json_extract(payload,'$.policy')=?2 AND json_extract(payload,'$.fresh_scope')=1 ELSE 0 END)",params![id,POLICY],|r|r.get(0))?)
}
fn schedule_reconsideration(
    tx: &Transaction<'_>,
    id: &str,
    generation: i64,
    cp: &Checkpoint,
    now: i64,
) -> Result<()> {
    let cutover = owner_checkout_recovery::cutover_seq(tx, id)?;
    let original:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND seq>?2 AND kind='verifier_evidence_recorded') OR EXISTS(SELECT 1 FROM workflow_event e JOIN scope_stage_response s ON s.workflow=e.workflow AND s.key=json_extract(e.payload,'$.key') WHERE e.workflow=?1 AND e.seq>?2 AND e.kind='scope_stage_response_recorded' AND json_valid(e.payload) AND CASE WHEN json_valid(s.response) THEN json_extract(s.response,'$.kind')='original_verifier_plan' ELSE 0 END)",params![id,cutover],|r|r.get(0))?;
    let payload = serde_json::json!({"version":1,"policy":POLICY,"prior_checkpoint":serde_json::to_string(cp)?,"fresh_scope":!original,"original_evidence":"preserved"});
    event(
        tx,
        id,
        generation,
        "scope_policy_reconsidered",
        &payload.to_string(),
        now,
    )?;
    let mut retry = cp.clone();
    retry.next = NextAction {
        reason: "scope_policy_changed".into(),
        action: "rescope_executable_acceptance".into(),
        eligible_at_ms: now,
    };
    let next = serde_json::to_string(&retry)?;
    tx.execute(
        "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
        params![id, next],
    )?;
    event(tx, id, generation, "checkpoint", &next, now)?;
    Ok(())
}

impl WorkflowStore {
    /// Free, bounded worker maintenance, independent of model/resource admission.
    /// Readers and owner-turn/reconciliation acquisition never call this.
    pub(crate) fn migrate_scope_policy(&mut self, repository: &str, now: i64) -> Result<usize> {
        let issue_prefix = format!("{}#", repository.to_ascii_lowercase());
        let pr_prefix = format!("{}/pr/", repository.to_ascii_lowercase());
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let rows:Vec<(String,String,i64)>=tx.prepare(
            "SELECT id,checkpoint,generation FROM workflow WHERE lease_until<=?1
             AND json_valid(checkpoint) AND json_extract(checkpoint,'$.stage')='scope'
             AND COALESCE(json_extract(checkpoint,'$.next.eligible_at_ms'),0)<=?1
             AND json_extract(checkpoint,'$.next.reason') IN ('material_external_decision_missing','acceptance_contract_invalid','readiness_assessment_invalid')
             AND (substr(id,1,length(?2))=?2 OR substr(id,1,length(?3))=?3)
             AND NOT EXISTS(SELECT 1 FROM workflow_event e WHERE e.workflow=workflow.id AND e.kind='scope_policy_reconsidered' AND CASE WHEN json_valid(e.payload) THEN json_extract(e.payload,'$.policy')=?4 ELSE 0 END)
             ORDER BY heartbeat_at,id LIMIT 64")?
            .query_map(params![now,issue_prefix,pr_prefix,POLICY],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut migrated = 0;
        for (id, raw, generation) in rows {
            let cp: Checkpoint = serde_json::from_str(&raw)?;
            let obsolete = match old_policy(&tx, &id, &cp) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(workflow=%id,error=%error,"scope policy history unavailable; preserve row and continue peers");
                    continue;
                }
            };
            if !obsolete
                || owner_control::require_dispatch(&tx, &id).is_err()
                || target_fence::fenced(&tx, &id)?
                || qualification_recovery::pending_mutation(&tx, &id)?
            {
                continue;
            }
            schedule_reconsideration(&tx, &id, generation, &cp, now)?;
            migrated += 1;
            if migrated == 4 {
                break;
            }
        }
        tx.commit()?;
        Ok(migrated)
    }

    pub(crate) fn scope_policy_pending(&self, id: &str) -> Result<bool> {
        let Some(s) = self.snapshot(id)? else {
            return Ok(false);
        };
        Ok(target(&s.checkpoint)
            && !enrolled(&self.conn, id)?
            && old_policy(&self.conn, id, &s.checkpoint)?)
    }
    pub(crate) fn scope_policy_retries(
        &self,
        repository: &str,
    ) -> Result<std::collections::BTreeSet<u64>> {
        let prefix = format!("{}#", repository.to_ascii_lowercase());
        let mut q=self.conn.prepare("SELECT id FROM workflow WHERE json_valid(checkpoint) AND json_extract(checkpoint,'$.stage')='scope' AND json_extract(checkpoint,'$.next.reason') IN ('material_external_decision_missing','acceptance_contract_invalid','readiness_assessment_invalid')")?;
        let ids = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = std::collections::BTreeSet::new();
        for id in ids {
            // These are advisory picker hints. A damaged row must not starve
            // peers; per-issue inspection still returns the typed history hold.
            if self.scope_policy_pending(&id).unwrap_or(false) {
                if let Some(n) = id.strip_prefix(&prefix).and_then(|s| s.parse().ok()) {
                    result.insert(n);
                }
            }
        }
        Ok(result)
    }
    pub(crate) fn reconsider_scope_policy(&mut self, lease: &Lease, now: i64) -> Result<bool> {
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
        let cp: Checkpoint = serde_json::from_str(&raw)?;
        if !target(&cp)
            || enrolled(&tx, &lease.workflow)?
            || !old_policy(&tx, &lease.workflow, &cp)?
        {
            return Ok(false);
        }
        ensure!(
            !qualification_recovery::pending_mutation(&tx, &lease.workflow)?,
            "scope policy cannot supersede pending publication"
        );
        schedule_reconsideration(&tx, &lease.workflow, lease.generation, &cp, now)?;
        tx.commit()?;
        Ok(true)
    }
    pub(crate) fn scope_policy_repair_key(&self, id: &str, key: String) -> Result<String> {
        Ok(if enrolled(&self.conn, id)? {
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(POLICY, "validator-repair", id, key))?)
            )
        } else {
            key
        })
    }
    pub(crate) fn scope_policy_key(&self, id: &str, key: String) -> Result<String> {
        Ok(if fresh_epoch(&self.conn, id)? {
            format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(&(POLICY, id, key))?)
            )
        } else {
            key
        })
    }
}

#[cfg(test)]
#[path = "autoship_scope_policy_tests.rs"]
mod tests;
