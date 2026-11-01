//! Retry failed infrastructure without changing the candidate or owner holds.
use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    version: u32,
    binding: Binding,
    failure_code: String,
    dependency_sha256: String,
    generation: i64,
    failures: u32,
    failed_at_ms: i64,
    eligible_at_ms: i64,
}
impl Retry {
    fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        ensure!(
            self.version == 1
                && self.generation > 0
                && self.failures > 0
                && self.failed_at_ms >= 0
                && self.eligible_at_ms > self.failed_at_ms,
            "invalid verification retry history"
        );
        ensure!(
            !self.failure_code.is_empty()
                && self.failure_code.len() <= 80
                && self
                    .failure_code
                    .bytes()
                    .all(|b| b.is_ascii_graphic() || b == b' '),
            "invalid verification infrastructure code"
        );
        ensure!(
            self.dependency_sha256.len() == 64
                && self
                    .dependency_sha256
                    .bytes()
                    .all(|b| matches!(b,b'0'..=b'9'|b'a'..=b'f')),
            "invalid retry dependency digest"
        );
        let delay = 300_000_i64
            .saturating_mul(1 << (self.failures - 1).min(4))
            .min(3_600_000);
        ensure!(
            self.eligible_at_ms == self.failed_at_ms.saturating_add(delay),
            "verification retry history has invalid backoff"
        );
        Ok(())
    }
}

impl WorkflowStore {
    pub fn defer_verification_infrastructure(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        failure_code: &str,
        dependency_sha256: &str,
        now: i64,
    ) -> Result<i64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        let (raw, progress_at): (String, i64) = tx.query_row(
            "SELECT checkpoint,progress_at FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        ensure!(
            checkpoint.stage == Stage::Verify,
            "only active verification can be deferred"
        );
        let previous: Option<String> = tx.query_row(
            "SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry' ORDER BY seq DESC LIMIT 1",
            [&lease.workflow], |r| r.get(0)).optional()?;
        let previous = previous
            .map(|raw| -> Result<Retry> {
                ensure!(
                    raw.len() <= 8192,
                    "verification retry history exceeds bound"
                );
                let prior: Retry = serde_json::from_str(&raw)?;
                prior.validate()?;
                ensure!(
                    prior.generation <= lease.generation && prior.failed_at_ms <= now,
                    "verification retry history is newer than its current authority"
                );
                ensure!(
                    serde_json::to_value(&prior)?
                        == serde_json::from_str::<serde_json::Value>(&raw)?,
                    "verification retry contains unknown binding fields"
                );
                Ok(prior)
            })
            .transpose()?;
        let same = |prior: &Retry| {
            prior.binding == *binding
                && prior.failure_code == failure_code
                && prior.dependency_sha256 == dependency_sha256
        };
        if let Some(prior) = previous
            .as_ref()
            .filter(|p| p.generation == lease.generation && progress_at <= p.failed_at_ms)
        {
            ensure!(
                same(prior)
                    && checkpoint.next.reason == "verification_infrastructure_retry"
                    && checkpoint.next.action == "retry_verification"
                    && checkpoint.next.eligible_at_ms == prior.eligible_at_ms,
                "verification retry replay changed within its owner generation"
            );
            return Ok(prior.eligible_at_ms);
        }
        let failures = previous
            .as_ref()
            .filter(|p| same(p) && progress_at <= p.failed_at_ms)
            .map_or(1, |p| p.failures.saturating_add(1));
        let delay = 300_000_i64
            .saturating_mul(1 << (failures - 1).min(4))
            .min(3_600_000);
        let retry = Retry {
            version: 1,
            binding: binding.clone(),
            failure_code: failure_code.into(),
            dependency_sha256: dependency_sha256.into(),
            generation: lease.generation,
            failures,
            failed_at_ms: now,
            eligible_at_ms: now.saturating_add(delay),
        };
        retry.validate()?;
        checkpoint.next = NextAction {
            reason: "verification_infrastructure_retry".into(),
            action: "retry_verification".into(),
            eligible_at_ms: retry.eligible_at_ms,
        };
        let checkpoint = serde_json::to_string(&checkpoint)?;
        // Candidate, evidence, progress clock, model charges and merge allowance
        // are preserved. Both events and the retry schedule commit together.
        tx.execute(
            "UPDATE workflow SET checkpoint=?2 WHERE id=?1",
            params![lease.workflow, checkpoint],
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "checkpoint",
            &checkpoint,
            now,
        )?;
        event(
            &tx,
            &lease.workflow,
            lease.generation,
            "verification_infrastructure_retry",
            &serde_json::to_string(&retry)?,
            now,
        )?;
        tx.commit()?;
        Ok(retry.eligible_at_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup(path: &Path) -> (WorkflowStore, Lease, Binding) {
        let mut store = WorkflowStore::open(path).unwrap();
        let binding = Binding {
            head: "head".into(),
            contract: "contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register("owner/repo#1", "fix", binding.clone(), 10)
            .unwrap();
        let lease = store
            .claim("owner/repo#1", "a", 20, 60_000)
            .unwrap()
            .unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.stage = Stage::Verify;
        checkpoint.evidence = vec!["protected-receipt".into()];
        store.checkpoint(&lease, &checkpoint, 21).unwrap();
        (store, lease, binding)
    }
    #[test]
    fn verification_infrastructure_retry_persists_backoff_and_idempotent_replay() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let (mut store, mut lease, binding) = setup(&path);
        let mut now = 30;
        for delay in [300_000, 600_000, 1_200_000, 2_400_000, 3_600_000, 3_600_000] {
            let before = store.snapshot(&lease.workflow).unwrap().unwrap();
            let allowance = store.merge_allowance(now, 5).unwrap();
            let due = store
                .defer_verification_infrastructure(
                    &lease,
                    &binding,
                    "network",
                    &"a".repeat(64),
                    now,
                )
                .unwrap();
            assert_eq!(due, now + delay);
            let count = store.events(&lease.workflow).unwrap().len();
            assert_eq!(
                store
                    .defer_verification_infrastructure(
                        &lease,
                        &binding,
                        "network",
                        &"a".repeat(64),
                        now + 1
                    )
                    .unwrap(),
                due
            );
            assert_eq!(store.events(&lease.workflow).unwrap().len(), count);
            let after = store.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
            assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
            assert_eq!(after.progress_at_ms, before.progress_at_ms);
            assert_eq!(after.model_calls, before.model_calls);
            assert_eq!(after.attempts, before.attempts);
            assert_eq!(store.merge_allowance(now + 1, 5).unwrap(), allowance);
            store.release(&lease, now + 2).unwrap();
            drop(store);
            store = WorkflowStore::open(&path).unwrap();
            assert!(store
                .claim_productive(&lease.workflow, "b", due - 1, 60_000)
                .unwrap()
                .is_none());
            lease = store
                .claim_productive(&lease.workflow, "b", due, 60_000)
                .unwrap()
                .unwrap();
            now = due + 1;
        }
        assert_eq!(
            store
                .defer_verification_infrastructure(
                    &lease,
                    &binding,
                    "network",
                    &"b".repeat(64),
                    now
                )
                .unwrap(),
            now + 300_000
        );
        let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Review;
        store.checkpoint(&lease, &cp, now + 2).unwrap();
        cp.stage = Stage::Verify;
        store.checkpoint(&lease, &cp, now + 3).unwrap();
        assert_eq!(
            store
                .defer_verification_infrastructure(
                    &lease,
                    &binding,
                    "network",
                    &"b".repeat(64),
                    now + 4
                )
                .unwrap(),
            now + 4 + 300_000,
            "real stage progress did not reset the consecutive failure backoff"
        );
    }
    #[test]
    fn verification_infrastructure_retry_cannot_clear_holds_or_use_stale_authority() {
        for fault in [
            "held",
            "pause",
            "stale",
            "binding",
            "history",
            "future-generation",
            "future-clock",
            "changed-replay",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut store, mut lease, mut binding) = setup(&dir.path().join("workflow.db"));
            match fault {
                "held" => {
                    let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
                    cp.stage = Stage::Held;
                    store.checkpoint(&lease, &cp, 22).unwrap();
                }
                "pause" => {
                    store
                        .set_owner_pause(&lease.workflow, "github:owner", 0, true, 22)
                        .unwrap();
                }
                "stale" => lease.generation += 1,
                "binding" => binding.head = "another-head".into(),
                "future-generation" | "future-clock" => {
                    let failed_at = if fault == "future-clock" { 50 } else { 22 };
                    let retry = Retry {
                        version: 1,
                        binding: binding.clone(),
                        failure_code: "network".into(),
                        dependency_sha256: "a".repeat(64),
                        generation: lease.generation
                            + if fault == "future-generation" { 1 } else { 0 },
                        failures: 1,
                        failed_at_ms: failed_at,
                        eligible_at_ms: failed_at + 300_000,
                    };
                    store.conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,1,'verification_infrastructure_retry',?2,22)",params![lease.workflow,serde_json::to_string(&retry).unwrap()]).unwrap();
                }
                "changed-replay" => {
                    store
                        .defer_verification_infrastructure(
                            &lease,
                            &binding,
                            "network",
                            &"a".repeat(64),
                            22,
                        )
                        .unwrap();
                    let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
                    cp.next.action = "another-action".into();
                    store.checkpoint(&lease, &cp, 23).unwrap();
                }
                "history" => {
                    store.conn.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,1,'verification_infrastructure_retry','{}',22)",[&lease.workflow]).unwrap();
                }
                _ => unreachable!(),
            }
            let before = store.snapshot(&lease.workflow).unwrap().unwrap();
            let count = store.events(&lease.workflow).unwrap().len();
            assert!(
                store
                    .defer_verification_infrastructure(
                        &lease,
                        &binding,
                        "network",
                        &"a".repeat(64),
                        30
                    )
                    .is_err(),
                "accepted {fault}"
            );
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(), before);
            assert_eq!(store.events(&lease.workflow).unwrap().len(), count);
        }
    }
}
