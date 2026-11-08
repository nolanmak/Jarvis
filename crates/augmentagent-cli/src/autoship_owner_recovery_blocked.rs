//! Descriptive recovery debt. Never completion, teardown or execution authority.
use super::*;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Obligations {
    transfer_id: String,
    abort_sequence: i64,
    namespaces: Vec<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Blocked {
    version: u32,
    request_id: String,
    intent: super::super::owner_publication::Reference,
    claim: RecordRef,
    generation: i64,
    previous: Option<RecordRef>,
    obligations: Obligations,
    control: owner_control::OwnerControl,
    observed_at_ms: i64,
    failures: u32,
    eligible_at_ms: Option<i64>,
}
/// Scheduler/status data only. None of these variants grants native authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryBlockedState {
    LegacyInputWriter,
    NamespaceObligations { eligible_at_ms: i64 },
    OriginalPublicationUnavailable,
    ForwardNativeFrontierUnavailable,
    HistoryCapacity,
}
fn deadline(observed_at: i64, failures: u32, obligations: &Obligations) -> Result<Option<i64>> {
    if obligations.namespaces.is_empty() { return Ok(None); }
    Ok(Some(observed_at.checked_add(delay(failures)).context("recovery blocked deadline overflow")?))
}
fn delay(failures: u32) -> i64 {
    300_000_i64.saturating_mul(1 << failures.saturating_sub(1).min(4)).min(3_600_000)
}
// Called only after the shared immutable native disposition validator. The
// sequence cut reconstructs historical obligations; later cleanup is retained.
fn obligations(conn: &Connection, workflow: &str, transfer: &str, before: i64) -> Result<Obligations> {
    let abort_sequence: i64 = conn.query_row(
        "SELECT sequence FROM workflow_native_handoff_aborted WHERE workflow=?1 AND transfer_id=?2 AND sequence<?3",
        params![workflow, transfer, before], |r| r.get(0))?;
    let mut query = conn.prepare("SELECT namespace_sha256 FROM workflow_native_abort_namespace n WHERE workflow=?1 AND transfer_id=?2 AND NOT EXISTS(SELECT 1 FROM workflow_native_abort_resolution r WHERE r.workflow=n.workflow AND r.transfer_id=n.transfer_id AND r.namespace_sha256=n.namespace_sha256 AND r.sequence<?3) ORDER BY namespace_sha256 LIMIT 66")?;
    let namespaces = query.query_map(params![workflow, transfer, before], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(namespaces.len() <= 65, "recovery obligations exceed bound");
    Ok(Obligations { transfer_id: transfer.into(), abort_sequence, namespaces })
}
fn load(conn: &Connection, workflow: &str) -> Result<Vec<(Blocked, RecordRef)>> {
    let indexed: i64 = conn.query_row("SELECT count(*) FROM workflow_owner_recovery_blocked WHERE workflow=?1", [workflow], |r| r.get(0))?;
    let events: i64 = conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_recovery_blocked'", [workflow], |r| r.get(0))?;
    ensure!(indexed == events && indexed <= 1024, "blocked recovery history index missing or oversized");
    if indexed == 0 { return Ok(Vec::new()); }
    let claims = journal(conn, workflow)?;
    native_pending::validate_dispositions(conn, workflow)?;
    let mut query = conn.prepare("SELECT sequence,record_sha256,substr(record,1,32769),length(CAST(record AS BLOB)) FROM workflow_owner_recovery_blocked WHERE workflow=?1 ORDER BY sequence LIMIT 1025")?;
    let rows = query.query_map([workflow], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?;
    let mut result: Vec<(Blocked, RecordRef)> = Vec::new();
    for row in rows {
        let (sequence, sha256, raw, len) = row?;
        ensure!(result.len() < 1024 && (1..=32768).contains(&len) && raw.len() as i64 == len && digest(&raw) == sha256,
            "recovery blocked history exceeds bound or changed");
        let record: Blocked = serde_json::from_str(&raw)?;
        let index = claims.iter().position(|(_, reference)| *reference == record.claim).context("blocked recovery claim missing")?;
        let claim = &claims[index].0;
        ensure!(record.version == 1 && serde_json::to_string(&record)? == raw
            && record.request_id == claim.original.request_id && record.intent == claim.original.intent
            && record.generation == claim.generation && record.claim.sequence < sequence
            && claims.get(index+1).is_none_or(|(_, next)| sequence < next.sequence)
            && record.observed_at_ms >= claim.claimed_at && record.observed_at_ms < claim.lease_until
            && record.failures > 0 && record.eligible_at_ms == deadline(record.observed_at_ms, record.failures, &record.obligations)?
            && record.previous == result.last().map(|(_, reference)| reference.clone()),
            "recovery blocked lineage changed");
        ensure!(record.obligations == obligations(conn, workflow, &record.obligations.transfer_id, sequence)?, "recovery blocked obligations changed");
        let pending=native_pending::recorded_pending(conn,workflow,&record.obligations.transfer_id)?;
        if pending.request().destination.owner.lease_generation==claim.original.activated_generation {
            let (original,_) = super::native_intent::original_record(conn,&claim.original)?;
            original.require_recovery_transfer(&pending)?;
        }else {super::native_intent::require_forward(conn,&claim.original,&pending)?;}
        let expected_failures = result.iter().rev().find(|(prior,_)| prior.request_id == record.request_id && prior.obligations == record.obligations)
            .map_or(1, |(prior,_)| prior.failures.saturating_add(1));
        ensure!(record.failures == expected_failures, "blocked recovery debt changed");
        let event: (String,i64,String,i64) = conn.query_row("SELECT kind,generation,payload,at_ms FROM workflow_event WHERE workflow=?1 AND seq=?2",
            params![workflow, sequence], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        ensure!(event.0 == "owner_publication_recovery_blocked" && event.1 == record.generation && event.3 == record.observed_at_ms
            && serde_json::from_str::<serde_json::Value>(&event.2)? == serde_json::json!({"version":1,"request_id":record.request_id,"record_sha256":sha256}),
            "blocked recovery event changed");
        result.push((record, RecordRef { sequence, sha256 }));
    }
    let count: i64 = conn.query_row("SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind='owner_publication_recovery_blocked'", [workflow], |r| r.get(0))?;
    ensure!(count == result.len() as i64, "blocked recovery history index missing");
    Ok(result)
}
pub(super) fn retained_count(conn:&Connection,workflow:&str)->Result<usize> {Ok(load(conn,workflow)?.len())}
// Shares claim acquisition's immediate transaction. This is metadata admission
// only; existing current claims can inspect and settle cleanup during backoff.
pub(super) fn blocked_state(conn: &Connection, plan: &RecoveryPlan, now: i64) -> Result<Option<RecoveryBlockedState>> {
    let history = load(conn, &plan.original.workflow)?;
    if let Some(state)=super::input_blocked::state(conn,plan,now,history.len())? {return Ok(Some(state));}
    let Some((record,_)) = history.last() else { return Ok(None); };
    ensure!(record.request_id == plan.original.request_id && record.intent == plan.original.intent
        && record.generation <= plan.predecessor.generation && record.observed_at_ms <= now,
        "recovery blocked observation differs from current lineage or clock");
    if history.len() == 1024 { return Ok(Some(RecoveryBlockedState::HistoryCapacity)); }
    let current = obligations(conn, &plan.original.workflow, &record.obligations.transfer_id, i64::MAX)?;
    if current != record.obligations { return Ok(None); }
    match record.eligible_at_ms {
        None => {
            let pending=native_pending::recorded_pending(conn,&plan.original.workflow,&record.obligations.transfer_id)?;
            Ok(Some(if pending.request().destination.owner.lease_generation==plan.original.activated_generation {
                RecoveryBlockedState::OriginalPublicationUnavailable
            }else {
                super::native_intent::require_forward(conn,&plan.original,&pending)?;
                RecoveryBlockedState::ForwardNativeFrontierUnavailable
            }))
        },
        Some(eligible_at_ms) => Ok((now < eligible_at_ms).then_some(RecoveryBlockedState::NamespaceObligations { eligible_at_ms })),
    }
}
impl WorkflowStore {
    /// Read-only scheduler/status observation, never a lease or permission.
    pub(crate) fn owner_publication_recovery_blocked_state(&self, workflow: &str, now: i64) -> Result<Option<RecoveryBlockedState>> {
        let tx = self.conn.unchecked_transaction()?;
        let Some(current) = plan(&tx, workflow)? else { return Ok(None); };
        let state = blocked_state(&tx, &current, now)?;
        tx.commit()?;
        Ok(state)
    }

    /// Observes validated unresolved namespace obligations. Does not assert
    /// that a process exists, perform cleanup, release input or complete work.
    pub(crate) fn record_owner_recovery_abort_blocked(&mut self, lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        pending: &PendingNativeHandoff, now: i64) -> Result<Option<i64>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_transfer_context(&tx, lease, authorization, pending, now)?;
        native_pending::require_aborted_reference(&tx, pending)?;
        let prior = load(&tx, &lease.claim.original.workflow)?;
        let current = obligations(&tx, &lease.claim.original.workflow, &pending.request().transfer_id, i64::MAX)?;
        if let Some((record,_)) = prior.last().filter(|(record,_)| record.claim == lease.reference && record.obligations == current) {
            return Ok(record.eligible_at_ms);
        }
        ensure!(prior.len()+super::input_blocked::retained_count(&tx,&lease.claim.original.workflow)? < 1024, "recovery blocked history capacity reached");
        let failures = prior.iter().rev().find(|(record,_)| record.request_id == lease.request_id() && record.obligations == current)
            .map_or(1, |(record,_)| record.failures.saturating_add(1));
        let eligible_at_ms = deadline(now, failures, &current)?;
        let record = Blocked { version:1, request_id:lease.request_id().into(), intent:lease.claim.original.intent.clone(),
            claim:lease.reference.clone(), generation:lease.generation(), previous:prior.last().map(|(_,reference)|reference.clone()),
            obligations:current, control:owner_control::read(&tx, &lease.claim.original.workflow)?.context("recovery owner control missing")?,
            observed_at_ms:now, failures, eligible_at_ms };
        let raw = serde_json::to_string(&record)?;
        ensure!(raw.len() <= 32768, "recovery blocked record exceeds bound");
        let sha = digest(&raw);
        event(&tx, &lease.claim.original.workflow, lease.generation(), "owner_publication_recovery_blocked",
            &serde_json::json!({"version":1,"request_id":record.request_id,"record_sha256":sha}).to_string(), now)?;
        let seq: i64 = tx.query_row("SELECT max(seq) FROM workflow_event WHERE workflow=?1", [&lease.claim.original.workflow], |r| r.get(0))?;
        tx.execute("INSERT INTO workflow_owner_recovery_blocked VALUES(?1,?2,?3,?4)",params![lease.claim.original.workflow,seq,sha,raw])?;
        load(&tx, &lease.claim.original.workflow)?;
        tx.commit()?;
        Ok(eligible_at_ms)
    }
}
