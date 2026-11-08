//! Scheduling observations for original metadata recovery; never native grants.
use super::*;
use crate::autoship_workflow::owner_publication::OwnerPublicationNativeFacts;
use augmentagent_channel_core::native_vm::ownership::{
    canonical_bytes, NoPendingPreparationFailure, PreparationFingerprintInput, TransferReason,
};
const KIND: &str = "owner_recovery_native_preparation_retry";
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreparationRetryGate {
    Ready,
    Held,
    Deferred { eligible_at_ms: i64 },
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Environment {
    policy_sha256: Option<String>,
    capacity_fingerprint: Option<String>,
}
/// Created only by current bounded private-policy and native-capacity reads.
/// No Clone/Deserialize. Capture outside workflow/native locks. Unknown capacity
/// cannot by itself wake a prior schedule; a gate never grants native authority.
pub(crate) struct PreparationEnvironment {
    value: Environment,
    observed_at_ms: i64,
}
impl PreparationEnvironment {
    pub(crate) fn observe() -> Result<Self> {
        use augmentagent_channel_core::native_vm::{ownership, state_capacity};
        let before = ownership::preparation_policy_sha256();
        let report = state_capacity::inspect();
        let after = ownership::preparation_policy_sha256();
        ensure!(
            before == after,
            "preparation policy changed during observation"
        );
        Ok(Self {
            value: Environment {
                policy_sha256: after,
                capacity_fingerprint: report.observed_fingerprint().map(str::to_owned),
            },
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }
    fn require_recent(&self, now: i64) -> Result<()> {
        ensure!(
            now >= self.observed_at_ms && now - self.observed_at_ms <= 30_000,
            "preparation environment observation expired"
        );
        Ok(())
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    version: u32,
    request_id: String,
    intent: super::super::owner_publication::Reference,
    claim: RecordRef,
    previous: Option<RecordRef>,
    facts_sha256: String,
    environment: Environment,
    // Last known observation survives a later unavailable capacity read. It is
    // used only for change detection and never asserted to be current capacity.
    known_capacity: Option<String>,
    failure_input: PreparationFingerprintInput,
    fingerprint: String,
    reason: String,
    deferred: bool,
    failures: u32,
    failed_at_ms: i64,
    eligible_at_ms: Option<i64>,
}
fn hash_valid(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f'))
}
fn delay(n: u32) -> i64 {
    super::super::capacity_retry::delay(n, true)
}
fn reason(code: &str) -> Result<TransferReason> {
    use TransferReason::*;
    [
        LockBusy,
        StateCapacity,
        HistoryCapacity,
        InventoryMissing,
        RuntimeUnavailable,
        ConversationUncertain,
        InvalidRequest,
        AuthorityConflict,
        MetadataInvalid,
        AuditCorrupt,
        PreparedCorrupt,
        PersistRetry,
        PendingUnknown,
    ]
    .into_iter()
    .find(|r| r.code() == code)
    .context("preparation retry reason invalid")
}
fn capacity_matches(known: Option<&str>, observed: Option<&str>) -> bool {
    observed.is_none_or(|current| known == Some(current))
}
fn same_dependencies(
    prior: &Retry,
    original: &Original,
    facts: &str,
    environment: &Environment,
) -> bool {
    prior.request_id == original.request_id
        && prior.intent == original.intent
        && prior.facts_sha256 == facts
        && prior.environment.policy_sha256 == environment.policy_sha256
        && capacity_matches(
            prior.known_capacity.as_deref(),
            environment.capacity_fingerprint.as_deref(),
        )
}
fn known_capacity(
    prior: Option<&Retry>,
    original: &Original,
    facts: &str,
    env: &Environment,
) -> Option<String> {
    env.capacity_fingerprint.clone().or_else(|| {
        prior
            .filter(|p| {
                p.request_id == original.request_id
                    && p.intent == original.intent
                    && p.facts_sha256 == facts
                    && p.environment.policy_sha256 == env.policy_sha256
            })
            .and_then(|p| p.known_capacity.clone())
    })
}
fn gate(record: &Retry, now: i64) -> PreparationRetryGate {
    gate_at(record.eligible_at_ms, now)
}
fn gate_at(eligible_at_ms: Option<i64>, now: i64) -> PreparationRetryGate {
    match eligible_at_ms {
        None => PreparationRetryGate::Held,
        Some(at) if now < at => PreparationRetryGate::Deferred { eligible_at_ms: at },
        Some(_) => PreparationRetryGate::Ready,
    }
}
fn history(conn: &Connection, workflow: &str, now: i64) -> Result<Option<(Retry, RecordRef)>> {
    let event_count: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_event WHERE workflow=?1 AND kind=?2",
        params![workflow, KIND],
        |r| r.get(0),
    )?;
    let indexed: i64 = conn.query_row(
        "SELECT count(*) FROM workflow_owner_recovery_preparation_retry WHERE workflow=?1",
        [workflow],
        |r| r.get(0),
    )?;
    ensure!(
        event_count == indexed && indexed <= 1024,
        "preparation retry history index missing or oversized"
    );
    let claims = journal(conn, workflow)?;
    let mut q=conn.prepare("SELECT e.seq,e.generation,substr(e.payload,1,8193),length(CAST(e.payload AS BLOB)),e.at_ms,substr(i.record_sha256,1,65),e.workflow,e.kind FROM workflow_owner_recovery_preparation_retry i JOIN workflow_event e ON e.seq=i.sequence WHERE i.workflow=?1 ORDER BY e.seq LIMIT 1025")?;
    let rows = q
        .query_map([workflow], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() as i64 == indexed,
        "preparation retry event missing"
    );
    let mut prior: Option<(Retry, RecordRef)> = None;
    for (seq, generation, raw, len, at, hash, scope, kind) in rows {
        ensure!(
            scope == workflow
                && kind == KIND
                && (1..=8192).contains(&len)
                && raw.len() as i64 == len
                && hash == digest(&raw),
            "preparation retry event/index changed"
        );
        let r: Retry = serde_json::from_str(&raw)?;
        let index = claims
            .iter()
            .position(|(_, reference)| *reference == r.claim)
            .context("preparation retry original claim missing")?;
        let (claim, reference) = &claims[index];
        let failure =
            NoPendingPreparationFailure::new(r.failure_input.clone(), reason(&r.reason)?)?;
        ensure!(
            r.version == 1
                && serde_json::to_string(&r)? == raw
                && r.request_id == claim.original.request_id
                && r.intent == claim.original.intent
                && generation == claim.generation
                && seq > reference.sequence
                && claims
                    .get(index + 1)
                    .is_none_or(|(_, next)| seq < next.sequence)
                && r.failed_at_ms == at
                && at >= claim.claimed_at
                && at < claim.lease_until
                && at <= now
                && r.previous.as_ref() == prior.as_ref().map(|(_, reference)| reference)
                && hash_valid(&r.facts_sha256)
                && r.environment
                    .policy_sha256
                    .as_deref()
                    .is_none_or(hash_valid)
                && r.environment
                    .capacity_fingerprint
                    .as_deref()
                    .is_none_or(hash_valid)
                && r.known_capacity.as_deref().is_none_or(hash_valid)
                && failure.workflow_id() == workflow
                && failure.workflow_binding() == &serde_json::to_value(&claim.checkpoint.binding)?
                && failure.fingerprint_input().latest_input_sha256.as_deref()
                    == Some(r.facts_sha256.as_str())
                && failure.fingerprint_input().selected_policy_sha256
                    == r.environment.policy_sha256
                && failure.fingerprint() == r.fingerprint
                && failure.is_deferred() == r.deferred
                && r.failures > 0
                && r.eligible_at_ms
                    == if r.deferred {
                        Some(
                            at.checked_add(delay(r.failures))
                                .context("preparation retry time overflow")?,
                        )
                    } else {
                        None
                    },
            "preparation retry journal changed"
        );
        let unchanged = prior.as_ref().filter(|(p, _)| {
            same_dependencies(p, &claim.original, &r.facts_sha256, &r.environment)
                && p.fingerprint == r.fingerprint
        });
        ensure!(
            r.failures
                == unchanged
                    .map(|(p, _)| p
                        .failures
                        .checked_add(1)
                        .context("preparation retry count overflow"))
                    .transpose()?
                    .unwrap_or(1)
                && prior.as_ref().is_none_or(|(p, _)| at >= p.failed_at_ms)
                && unchanged.is_none_or(|(p, _)| p.eligible_at_ms.is_some_and(|due| at >= due))
                && r.known_capacity
                    == known_capacity(
                        prior.as_ref().map(|(p, _)| p),
                        &claim.original,
                        &r.facts_sha256,
                        &r.environment
                    ),
            "preparation retry predecessor changed"
        );
        prior = Some((
            r,
            RecordRef {
                sequence: seq,
                sha256: hash,
            },
        ));
    }
    Ok(prior)
}
fn validate(
    tx: &Transaction<'_>,
    lease: &RecoveryLease,
    authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
    facts: &OwnerPublicationNativeFacts,
    environment: &PreparationEnvironment,
    now: i64,
) -> Result<String> {
    environment.require_recent(now)?;
    require_authorized_claim(tx, lease, authorization, now)?;
    let original = &lease.claim.original;
    let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",[&original.workflow],|r|r.get(0))?;
    ensure!(
        !pending
            && !native_pending::orphaned(tx, &original.workflow)?
            && native_intent::forward(tx, original)?.is_none(),
        "preparation retry cannot hide a pending, historical or uncertain transfer"
    );
    native_pending::require_clear_abort_barriers(tx, &original.workflow)?;
    let actual = recovery_native_facts_tx(tx, lease, authorization, now)?.fingerprint()?;
    ensure!(
        actual == facts.fingerprint()?
            && facts.namespace().owner.lease_generation < lease.generation(),
        "preparation retry original native facts changed"
    );
    Ok(actual)
}
pub(super) fn scheduling_status(
    conn: &Connection,
    workflow: &str,
    environment: &PreparationEnvironment,
    now: i64,
) -> Result<Option<PreparationRetryGate>> {
    environment.require_recent(now)?;
    let Some(plan) = plan(conn, workflow)? else {
        return Ok(None);
    };
    let prior = history(conn, workflow, now)?;
    ensure!(
        !native_pending::orphaned(conn, workflow)?,
        "preparation status has uncertain native persistence"
    );
    let pending:bool=conn.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL)",[workflow],|r|r.get(0))?;
    if pending || native_intent::forward(conn, &plan.original)?.is_some() {
        return Ok(Some(PreparationRetryGate::Ready));
    }
    let (record, _) = native_intent::original_record(conn, &plan.original)?;
    let context = RecoveryNativeContext {
        conn: conn,
        workflow,
        binding: record.recovery_binding().clone(),
        namespace: native_intent::frontier(conn, &plan.original)?,
    };
    let actual = super::super::native_handoff::read_recovery_metadata_facts(&context)?;
    let facts = OwnerPublicationNativeFacts::from_recovery_metadata(actual).fingerprint()?;
    let state = prior
        .filter(|(r, _)| same_dependencies(r, &plan.original, &facts, &environment.value))
        .map_or(PreparationRetryGate::Ready, |(r, _)| gate(&r, now));
    Ok(Some(state))
}
impl WorkflowStore {
    /// Read-only scheduling preview before claiming. No owner/repository/native
    /// authority is returned; dispatch must independently acquire all of it.
    /// Known pending effects take precedence over a preparation-only schedule.
    pub(crate) fn owner_recovery_native_preparation_status(
        &mut self,
        workflow: &str,
        environment: &PreparationEnvironment,
        now: i64,
    ) -> Result<Option<PreparationRetryGate>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let result = scheduling_status(&tx, workflow, environment, now)?;
        tx.commit()?;
        Ok(result)
    }

    /// A current metadata claim may read scheduling data. This neither renews
    /// that claim nor establishes native absence, completion or availability.
    pub(crate) fn owner_recovery_native_retry_gate(
        &mut self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        facts: &OwnerPublicationNativeFacts,
        environment: &PreparationEnvironment,
        now: i64,
    ) -> Result<PreparationRetryGate> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let actual = validate(&tx, lease, authorization, facts, environment, now)?;
        let state = history(&tx, &lease.claim.original.workflow, now)?
            .filter(|(r, _)| {
                same_dependencies(r, &lease.claim.original, &actual, &environment.value)
            })
            .map_or(PreparationRetryGate::Ready, |(r, _)| gate(&r, now));
        tx.commit()?;
        Ok(state)
    }
    /// The failure is diagnostic only. Independent current SQL checks reject
    /// any pending/unknown transfer or remote operation before scheduling it.
    pub(crate) fn defer_owner_recovery_native_preparation(
        &mut self,
        lease: &RecoveryLease,
        authorization: &crate::self_improve::targeted::AuthorizedOwnerRecovery<'_>,
        facts: &OwnerPublicationNativeFacts,
        environment: &PreparationEnvironment,
        failure: &NoPendingPreparationFailure,
        now: i64,
    ) -> Result<PreparationRetryGate> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual = validate(&tx, lease, authorization, facts, environment, now)?;
        let input = failure.fingerprint_input();
        let binding_hash = format!(
            "{:x}",
            Sha256::digest(canonical_bytes(&serde_json::to_value(
                facts.original_binding()
            )?)?)
        );
        ensure!(
            failure.workflow_id() == lease.claim.original.workflow
                && failure.workflow_binding()
                    == &serde_json::to_value(&lease.claim.checkpoint.binding)?
                && input.latest_input_sha256.as_deref() == Some(actual.as_str())
                && input.original_binding_sha256.as_deref() == Some(binding_hash.as_str())
                && input.selected_policy_sha256 == environment.value.policy_sha256,
            "preparation retry failure scope changed"
        );
        let prior = history(&tx, &lease.claim.original.workflow, now)?;
        let unchanged = prior.as_ref().filter(|(r, _)| {
            same_dependencies(r, &lease.claim.original, &actual, &environment.value)
                && r.fingerprint == failure.fingerprint()
        });
        if let Some((r, _)) = unchanged {
            if gate(r, now) != PreparationRetryGate::Ready {
                let state = gate(r, now);
                tx.commit()?;
                return Ok(state);
            }
        }
        let failures = unchanged
            .map(|(r, _)| {
                r.failures
                    .checked_add(1)
                    .context("preparation retry count overflow")
            })
            .transpose()?
            .unwrap_or(1);
        let at = if failure.is_deferred() {
            Some(
                now.checked_add(delay(failures))
                    .context("preparation retry time overflow")?,
            )
        } else {
            None
        };
        let retry = Retry {
            version: 1,
            request_id: lease.request_id().into(),
            intent: lease.claim.original.intent.clone(),
            claim: lease.reference.clone(),
            previous: prior.as_ref().map(|(_, r)| r.clone()),
            facts_sha256: actual.clone(),
            environment: environment.value.clone(),
            known_capacity: known_capacity(
                prior.as_ref().map(|(r, _)| r),
                &lease.claim.original,
                &actual,
                &environment.value,
            ),
            failure_input: input.clone(),
            fingerprint: failure.fingerprint().into(),
            reason: failure.reason().code().into(),
            deferred: failure.is_deferred(),
            failures,
            failed_at_ms: now,
            eligible_at_ms: at,
        };
        let raw = serde_json::to_string(&retry)?;
        ensure!(raw.len() <= 8192, "preparation retry record exceeds bound");
        event(
            &tx,
            &lease.claim.original.workflow,
            lease.generation(),
            KIND,
            &raw,
            now,
        )?;
        let seq: i64 = tx.query_row(
            "SELECT max(seq) FROM workflow_event WHERE workflow=?1",
            [&lease.claim.original.workflow],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO workflow_owner_recovery_preparation_retry VALUES(?1,?2,?3)",
            params![lease.claim.original.workflow, seq, digest(&raw)],
        )?;
        ensure!(
            history(&tx, &lease.claim.original.workflow, now)?
                .is_some_and(|(_, r)| r.sha256 == digest(&raw)),
            "preparation retry append changed"
        );
        let state = gate(&retry, now);
        tx.commit()?;
        Ok(state)
    }
}

#[cfg(test)]
pub(super) fn fixture_encoding(raw: &str) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::from_str::<Retry>(raw)?)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_timing_never_turns_an_integrity_hold_into_a_clock_retry() {
        for now in [0, 300_000, 3_600_000, i64::MAX] {
            assert_eq!(gate_at(None, now), PreparationRetryGate::Held);
        }
        assert_eq!(
            gate_at(Some(300_000), 299_999),
            PreparationRetryGate::Deferred {
                eligible_at_ms: 300_000
            }
        );
        assert_eq!(gate_at(Some(300_000), 300_000), PreparationRetryGate::Ready);
    }
    #[test]
    fn repeated_transient_failures_use_the_required_capped_backoff() {
        assert_eq!(
            (1..=7).map(delay).collect::<Vec<_>>(),
            vec![300_000, 600_000, 1_200_000, 2_400_000, 3_600_000, 3_600_000, 3_600_000]
        );
        assert_eq!(delay(u32::MAX), 3_600_000);
    }
    #[test]
    fn unavailable_capacity_cannot_fake_a_dependency_change() {
        assert!(capacity_matches(Some("known"), None));
        assert!(capacity_matches(None, None));
        assert!(capacity_matches(Some("known"), Some("known")));
        assert!(!capacity_matches(Some("known"), Some("changed")));
        assert!(!capacity_matches(None, Some("first-real-observation")));
    }
}
