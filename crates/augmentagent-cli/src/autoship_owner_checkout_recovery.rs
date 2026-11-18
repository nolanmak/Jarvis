//! Explicit owner supersession of lost checkout custody. Evidence tables are
//! append-only here; only the live scheduling projection advances to Intake.
use super::*;

pub(super) const SCHEMA:&str="CREATE TABLE IF NOT EXISTS owner_checkout_recovery(
 request_id TEXT PRIMARY KEY,workflow TEXT NOT NULL,principal TEXT NOT NULL,reason TEXT NOT NULL,
 original_checkpoint TEXT NOT NULL,original_snapshot TEXT NOT NULL,original_context TEXT NOT NULL,original_dependencies TEXT NOT NULL,original_scope_keys TEXT NOT NULL,original_bounded_attempts TEXT NOT NULL,
 receipt_path TEXT NOT NULL,receipt BLOB NOT NULL,worktree TEXT NOT NULL,branch TEXT NOT NULL,
 base TEXT NOT NULL,created_at_ms INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS active_checkout_recovery(workflow TEXT PRIMARY KEY,request_id TEXT NOT NULL REFERENCES owner_checkout_recovery(request_id));";

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OwnerCheckoutRecovery {
    pub request_id: String,
    pub workflow: String,
    pub principal: String,
    pub reason: String,
    pub original: Snapshot,
    pub original_context: RecoveryContext,
    pub receipt_path: std::path::PathBuf,
    pub receipt: Vec<u8>,
    pub worktree: std::path::PathBuf,
    pub branch: String,
    pub base: String,
}
impl WorkflowStore {
    pub(crate) fn pending_owner_checkout_recoveries(&self, prefix: &str) -> Result<Vec<u64>> {
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_checkout_recovery')",[],|r|r.get(0))?;
        if !exists {
            return Ok(vec![]);
        }
        let mut q=self.conn.prepare("SELECT w.id FROM workflow w JOIN active_checkout_recovery a ON a.workflow=w.id WHERE json_valid(w.checkpoint) AND json_extract(w.checkpoint,'$.stage')='intake' AND json_extract(w.checkpoint,'$.next.reason')='owner_checkout_recovered'")?;
        let ids = q
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids
            .iter()
            .filter_map(|id| id.strip_prefix(prefix)?.parse().ok())
            .collect())
    }
    pub(crate) fn recovery_snapshots(&self) -> Result<Vec<Snapshot>> {
        let mut query = self.conn.prepare("SELECT id FROM workflow ORDER BY id")?;
        let ids = query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.into_iter()
            .map(|id| {
                self.snapshot(&id)?
                    .context("workflow disappeared during recovery list")
            })
            .collect()
    }
    pub(crate) fn active_checkout_recovery(
        &self,
        id: &str,
    ) -> Result<Option<OwnerCheckoutRecovery>> {
        // A read-only picker must also work against stores created before this table.
        let exists:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_checkout_recovery')",[],|r|r.get(0))?;
        if !exists {
            return Ok(None);
        }
        self.conn.query_row("SELECT r.request_id,r.workflow,r.principal,r.reason,r.original_snapshot,r.original_context,r.receipt_path,r.receipt,r.worktree,r.branch,r.base FROM owner_checkout_recovery r JOIN active_checkout_recovery a USING(request_id) WHERE a.workflow=?1",[id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,Vec<u8>>(7)?,r.get::<_,String>(8)?,r.get::<_,String>(9)?,r.get::<_,String>(10)?))).optional()?
   .map(|(request_id,workflow,principal,reason,snapshot,context,receipt_path,receipt,worktree,branch,base)|Ok(OwnerCheckoutRecovery {request_id,workflow,principal,reason,original:serde_json::from_str(&snapshot)?,original_context:serde_json::from_str(&context)?,receipt_path:receipt_path.into(),receipt,worktree:worktree.into(),branch,base})).transpose()
    }
    /// The adapter authenticates GitHub authority and proves lost physical custody.
    /// This transaction fences competing leases/pauses and commits authority,
    /// immutable archive and the replacement projection together.
    pub(crate) fn recover_owner_checkout(
        &mut self,
        plan: &OwnerCheckoutRecovery,
        now: i64,
    ) -> Result<()> {
        ensure!(
            self.snapshot(&plan.workflow)?.as_ref() == Some(&plan.original),
            "workflow changed during owner recovery"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (raw, lease, generation, branch, pr): (String, i64, i64, String, Option<u64>) = tx
            .query_row(
                "SELECT checkpoint,lease_until,generation,branch,pr FROM workflow WHERE id=?1",
                [&plan.workflow],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
        ensure!(lease <= now, "live-workflow-lease");
        ensure!(
            generation == plan.original.generation
                && branch == plan.original.branch
                && pr.is_none()
                && serde_json::from_str::<Checkpoint>(&raw)? == plan.original.checkpoint,
            "workflow changed during owner recovery"
        );
        ensure!(
            owner_control::read(&tx, &plan.workflow)?.is_none_or(|c| !c.paused),
            "explicit-owner-hold"
        );
        ensure!(
            awaits_initial_input(&plan.original.checkpoint),
            "not-an-initial-input-hold"
        );
        ensure!(!tx.query_row("SELECT EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM pending_publication WHERE workflow=?1)",[&plan.workflow],|r|r.get::<_,bool>(0))?,"pending-workflow-operation");
        let issue = plan
            .workflow
            .rsplit_once('#')
            .context("issue identity missing")?
            .1
            .parse::<u64>()?;
        let dependency:Option<(String,String,String,String)>=tx.query_row("SELECT request_hash,base,kind,evidence FROM issue_readiness_dependencies WHERE repository=?1 AND issue=?2",params![plan.original_context.repository,issue],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        let keys = {
            let mut q =
                tx.prepare("SELECT key FROM scope_stage_response WHERE workflow=?1 ORDER BY key")?;
            let rows = q
                .query_map([&plan.workflow], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        let bounded = {
            let mut q=tx.prepare("SELECT purpose,binding FROM bounded_attempt WHERE workflow=?1 ORDER BY purpose,binding")?;
            let rows = q
                .query_map([&plan.workflow], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        tx.execute("INSERT INTO owner_checkout_recovery VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",params![plan.request_id,plan.workflow,plan.principal,plan.reason,raw,serde_json::to_string(&plan.original)?,serde_json::to_string(&plan.original_context)?,serde_json::to_string(&dependency)?,serde_json::to_string(&keys)?,serde_json::to_string(&bounded)?,plan.receipt_path.to_string_lossy(),plan.receipt,plan.worktree.to_string_lossy(),plan.branch,plan.base,now])?;
        let prior = serde_json::json!({"kind":"checkout_continuity_lost","archive_request_id":plan.request_id,"superseded_by_owner_recovery":plan.request_id,"original_binding":plan.original.checkpoint.binding});
        tx.execute(
            "INSERT INTO owner_readoption VALUES(?1,?2,'issue',?3,?4,?5,?6,'applied',?7,?7)",
            params![
                plan.request_id,
                plan.original_context.repository,
                plan.workflow
                    .rsplit_once('#')
                    .context("issue identity missing")?
                    .1
                    .parse::<u64>()?,
                plan.reason,
                plan.principal,
                prior.to_string(),
                now
            ],
        )?;
        tx.execute("INSERT INTO active_checkout_recovery VALUES(?1,?2) ON CONFLICT(workflow) DO UPDATE SET request_id=excluded.request_id",params![plan.workflow,plan.request_id])?;
        let binding = Binding {
            head: plan.base.clone(),
            contract: format!(
                "{:x}",
                Sha256::digest(format!("owner-recovery:{}", plan.request_id))
            ),
            evidence_hash: format!(
                "{:x}",
                Sha256::digest(format!("fresh-checkout:{}", plan.request_id))
            ),
        };
        let cp = Checkpoint {
            stage: Stage::Intake,
            binding,
            evidence: vec![],
            next: NextAction {
                reason: "owner_checkout_recovered".into(),
                action: "fresh_scope_and_review".into(),
                eligible_at_ms: now,
            },
        };
        tx.execute("UPDATE workflow SET branch=?2,checkpoint=?3,owner=NULL,generation=generation+1,lease_until=0,heartbeat_at=?4,progress_at=?4,attempts=0 WHERE id=?1",params![plan.workflow,plan.branch,serde_json::to_string(&cp)?,now])?;
        event(
            &tx,
            &plan.workflow,
            generation + 1,
            "superseded_by_owner_recovery",
            &prior.to_string(),
            now,
        )?;
        tx.commit()?;
        Ok(())
    }
}

/// Cutover affects current consumers, never the immutable evidence rows. In
/// particular, malformed/orphaned old repair history must not poison the new
/// owner-authorized episode. Missing evidence after cutover still fails closed.
fn scope_frontier(
    conn: &Connection,
    workflow: &str,
) -> Result<Option<(i64, std::collections::BTreeSet<String>)>> {
    let exists:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='active_checkout_recovery')",[],|r|r.get(0))?;
    if !exists {
        return Ok(None);
    }
    let row:Option<(String,String)>=conn.query_row("SELECT r.request_id,r.original_scope_keys FROM active_checkout_recovery a JOIN owner_checkout_recovery r USING(request_id) WHERE a.workflow=?1",[workflow],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((request, keys)) = row else {
        return Ok(None);
    };
    let seq:Option<i64>=conn.query_row("SELECT MAX(seq) FROM workflow_event WHERE workflow=?1 AND kind='superseded_by_owner_recovery' AND CASE WHEN json_valid(payload) THEN json_extract(payload,'$.archive_request_id')=?2 ELSE 0 END",params![workflow,request],|r|r.get(0))?;
    Ok(Some((
        seq.context("owner recovery supersession event missing")?,
        serde_json::from_str(&keys)?,
    )))
}
pub(super) fn scope_response(
    conn: &Connection,
    workflow: &str,
    key: &str,
) -> Result<Option<Option<String>>> {
    let Some((seq, old_keys)) = scope_frontier(conn, workflow)? else {
        return Ok(None);
    };
    let malformed:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND seq>?2 AND kind='scope_stage_response_recorded' AND NOT json_valid(payload))",params![workflow,seq],|r|r.get(0))?;
    ensure!(!malformed, MissingScopeResponse);
    let recorded:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_event WHERE workflow=?1 AND seq>?2 AND kind='scope_stage_response_recorded' AND json_extract(payload,'$.key')=?3)",params![workflow,seq,key],|r|r.get(0))?;
    let value: Option<String> = conn
        .query_row(
            "SELECT response FROM scope_stage_response WHERE workflow=?1 AND key=?2",
            params![workflow, key],
            |r| r.get(0),
        )
        .optional()?;
    if recorded {
        ensure!(value.is_some(), MissingScopeResponse);
        return Ok(Some(value));
    }
    if old_keys.contains(key) {
        return Ok(Some(None));
    }
    ensure!(value.is_none(), MissingScopeResponse);
    Ok(Some(None))
}
pub(super) fn scope_keys(
    conn: &Connection,
    workflow: &str,
    after: &str,
) -> Result<Option<Vec<String>>> {
    let Some((seq, old_keys)) = scope_frontier(conn, workflow)? else {
        return Ok(None);
    };
    let mut keys = std::collections::BTreeSet::new();
    let mut q=conn.prepare("SELECT payload FROM workflow_event WHERE workflow=?1 AND seq>?2 AND kind='scope_stage_response_recorded'")?;
    for raw in q.query_map(params![workflow, seq], |r| r.get::<_, String>(0))? {
        let v: serde_json::Value = serde_json::from_str(&raw?).map_err(|_| MissingScopeResponse)?;
        let key = v["key"].as_str().context(MissingScopeResponse)?;
        ensure!(
            key.len() == 64
                && key
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            MissingScopeResponse
        );
        keys.insert(key.to_owned());
    }
    let mut q = conn.prepare("SELECT key FROM scope_stage_response WHERE workflow=?1")?;
    for key in q.query_map([workflow], |r| r.get::<_, String>(0))? {
        let key = key?;
        if !old_keys.contains(&key) {
            keys.insert(key);
        }
    }
    ensure!(
        keys.iter().all(|key| key.len() == 64
            && key
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())),
        MissingScopeResponse
    );
    Ok(Some(
        keys.into_iter()
            .filter(|key| key.as_str() > after)
            .take(16)
            .collect(),
    ))
}

pub(super) fn cutover_seq(conn: &Connection, workflow: &str) -> Result<i64> {
    Ok(scope_frontier(conn, workflow)?.map_or(0, |(seq, _)| seq))
}
pub(super) fn native_attempts(conn: &Connection, workflow: &str) -> Result<Option<u64>> {
    if scope_frontier(conn, workflow)?.is_none() {
        return Ok(None);
    }
    let raw:String=conn.query_row("SELECT r.original_bounded_attempts FROM active_checkout_recovery a JOIN owner_checkout_recovery r USING(request_id) WHERE a.workflow=?1",[workflow],|r|r.get(0))?;
    let original: std::collections::BTreeSet<(String, String)> = serde_json::from_str(&raw)?;
    let mut q=conn.prepare("SELECT purpose,binding,attempts FROM bounded_attempt WHERE workflow=?1 AND substr(purpose,1,length(?2))=?2")?;
    let mut total = 0u64;
    for row in q.query_map(params![workflow, BASELINE_NATIVE_PURPOSE_PREFIX], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, u64>(2)?,
        ))
    })? {
        let (purpose, binding, attempts) = row?;
        if !original.contains(&(purpose, binding)) {
            total = total
                .checked_add(attempts)
                .context("native scope attempt overflow")?;
        }
    }
    Ok(Some(total))
}
