//! Retry failed infrastructure without changing the candidate or owner holds.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retry {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage: Option<Stage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    domain:Option<VerificationDomain>,
    binding: Binding,
    failure_code: String,
    dependency_sha256: String,
    generation: i64,
    failures: u32,
    failed_at_ms: i64,
    eligible_at_ms: i64,
}
impl Retry {
    fn stage(&self) -> Stage {
        self.stage.unwrap_or(Stage::Verify)
    }
    fn domain(&self)->VerificationDomain {self.domain.unwrap_or_else(||VerificationDomain::for_failure(&self.failure_code))}
    fn validate(&self) -> Result<()> {
        self.binding.validate()?;
        ensure!(
            ((self.version == 1 && self.stage.is_none() && self.domain.is_none())
                || (self.version == 2 && self.stage.is_some() && self.domain.is_none())
                || (self.version == 3 && self.stage.is_some() && self.domain==Some(VerificationDomain::NativeHandoffPreparation)))
                && retryable_domain_stage(self.domain(),self.stage())
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

/// A completed verification gate is meaningful progress for its own domain.
/// Starting a stage, editing a head, or passing CI cannot clear acceptance debt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationDomain { Ci, Acceptance, NativeHandoffPreparation }
impl VerificationDomain {
    fn key(self)->&'static str {match self {Self::Ci=>"ci",Self::Acceptance=>"acceptance",Self::NativeHandoffPreparation=>"native_handoff_preparation"}}
    fn for_failure(code: &str) -> Self {
        if code=="acceptance verifier infrastructure" {Self::Acceptance} else {Self::Ci}
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatePassed { version: u32, domain: VerificationDomain, binding: Binding }

fn gate_passed_since(tx: &Transaction<'_>, lease: &Lease, after_seq: i64, after_ms: i64,
    domain: VerificationDomain, now: i64) -> Result<bool> {
    let mut statement=tx.prepare("SELECT payload,generation,at_ms FROM workflow_event WHERE workflow=?1 AND kind='verification_gate_passed' AND seq>?2 ORDER BY seq LIMIT 1025")?;
    let rows=statement.query_map(params![lease.workflow,after_seq],|row|
        Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?)))?;
    let mut recovered=false;
    for (index,row) in rows.enumerate() {
        let (raw,generation,at)=row?;
        ensure!(index<1024 && raw.len()<=8192,"verification success history exceeds bound");
        let passed:GatePassed=serde_json::from_str(&raw)?;
        passed.binding.validate()?;
        ensure!(passed.version==1 && generation>0 && generation<=lease.generation && at>=after_ms && at<=now
            && serde_json::to_value(&passed)?==serde_json::from_str::<serde_json::Value>(&raw)?,
            "verification success history is invalid or newer than current authority");
        recovered|=passed.domain==domain;
    }
    Ok(recovered)
}

fn retryable_stage(stage: Stage) -> bool {
    matches!(
        stage,
        Stage::Scope
            | Stage::Build
            | Stage::Verify
            | Stage::Review
            | Stage::Reconcile
            | Stage::AwaitChecks
    )
}

fn retryable_domain_stage(domain:VerificationDomain,stage:Stage)->bool {
    if domain==VerificationDomain::NativeHandoffPreparation { !matches!(stage,Stage::Held|Stage::Merged|Stage::Retired) }
    else {retryable_stage(stage)}
}
impl WorkflowStore {
    pub fn defer_native_handoff_preparation(&mut self,lease:&Lease,binding:&Binding,
        failure:&augmentagent_channel_core::native_vm::ownership::NoPendingPreparationFailure,now:i64)->Result<i64> {
        ensure!(failure.workflow_id()==lease.workflow && failure.workflow_binding()==&serde_json::to_value(binding)?,"native preparation retry scope differs from current workflow");
        self.defer_retry(lease,binding,failure.reason().code(),failure.fingerprint(),VerificationDomain::NativeHandoffPreparation,now)
    }
    /// Called after the controller's actual successful gate, never at stage
    /// entry. Completion remains recordable after an owner pause; it cannot
    /// clear that pause, schedule productive work, or change any accounting.
    pub fn record_verification_gate_passed(&mut self, lease: &Lease, binding: &Binding,
        domain: VerificationDomain, now: i64) -> Result<()> {
        let tx=self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx,lease,binding,now)?;
        event(&tx,&lease.workflow,lease.generation,"verification_gate_passed",
            &serde_json::to_string(&GatePassed {version:1,domain,binding:binding.clone()})?,now)?;
        tx.commit()?;
        Ok(())
    }

    pub fn defer_verification_infrastructure(
        &mut self,
        lease: &Lease,
        binding: &Binding,
        failure_code: &str,
        dependency_sha256: &str,
        now: i64,
    ) -> Result<i64> {
        self.defer_retry(lease,binding,failure_code,dependency_sha256,VerificationDomain::for_failure(failure_code),now)
    }
    fn defer_retry(&mut self,lease:&Lease,binding:&Binding,failure_code:&str,dependency_sha256:&str,domain:VerificationDomain,now:i64)->Result<i64> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_binding(&tx, lease, binding, now)?;
        owner_control::require_dispatch(&tx, &lease.workflow)?;
        if domain==VerificationDomain::NativeHandoffPreparation {
            // No-pending diagnostics are descriptive, never proof that a
            // possibly committed request vanished. Fence absence in this write.
            let orphan=native_pending::orphaned(&tx,&lease.workflow)?;
            let unresolved:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM workflow_native_handoff_pending WHERE workflow=?1) OR EXISTS(SELECT 1 FROM operation WHERE workflow=?1 AND receipt IS NULL) OR EXISTS(SELECT 1 FROM merge_allowance_dispatch d JOIN operation o ON o.id=d.operation WHERE o.workflow=?1 AND d.state IN ('reserved','dispatched','confirmed'))",[&lease.workflow],|r|r.get(0))?;
            ensure!(!unresolved && !orphan,"native preparation retry cannot replace unresolved durable intent");
        }
        let (retry_reason,retry_action)=if domain==VerificationDomain::NativeHandoffPreparation {("native_handoff_preparation_retry","retry_native_handoff_preparation")} else {("verification_infrastructure_retry","retry_verification")};
        let (raw, progress_at): (String, i64) = tx.query_row(
            "SELECT checkpoint,progress_at FROM workflow WHERE id=?1",
            [&lease.workflow],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut checkpoint: Checkpoint = serde_json::from_str(&raw)?;
        ensure!(
            retryable_domain_stage(domain,checkpoint.stage),
            "only an active pre-merge CI stage can be deferred"
        );
        let decode = |raw: &str| -> Result<Retry> {
            ensure!(raw.len()<=8192,"verification retry history exceeds bound");
            let prior:Retry=serde_json::from_str(raw)?;
            prior.validate()?;
            ensure!(prior.generation<=lease.generation && prior.failed_at_ms<=now,
                "verification retry history is newer than its current authority");
            ensure!(serde_json::to_value(&prior)?==serde_json::from_str::<serde_json::Value>(raw)?,
                "verification retry contains unknown binding fields");
            Ok(prior)
        };
        // Domain isolation must not hide malformed history or let a second
        // domain overwrite an unchanged schedule in the same owner generation.
        let latest:Option<String>=tx.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|row|row.get(0)).optional()?;
        let latest=latest.map(|raw|decode(&raw)).transpose()?;
        if let Some(prior)=latest.as_ref().filter(|p|p.generation==lease.generation && progress_at<=p.failed_at_ms) {
            ensure!(prior.domain()==domain,"retry domains cannot overwrite unchanged debt in the same generation");
        }
        let previous:Option<(i64,String)>=tx.query_row(
            "SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry' AND coalesce(json_extract(payload,'$.domain'),CASE WHEN json_extract(payload,'$.failure_code')='acceptance verifier infrastructure' THEN 'acceptance' ELSE 'ci' END)=?2 ORDER BY seq DESC LIMIT 1",
            params![lease.workflow,domain.key()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
        let previous=previous.map(|(seq,raw)|decode(&raw).map(|retry|(seq,retry))).transpose()?;
        let same = |prior: &Retry| {
            prior.domain()==domain && prior.binding == *binding
                && prior.stage() == checkpoint.stage
                && prior.failure_code == failure_code
                && prior.dependency_sha256 == dependency_sha256
        };
        if let Some(prior) = previous
            .as_ref()
            .map(|(_,retry)|retry)
            .filter(|p| p.generation == lease.generation && progress_at <= p.failed_at_ms)
        {
            ensure!(
                same(prior)
                    && checkpoint.next.reason == retry_reason
                    && checkpoint.next.action == retry_action
                    && checkpoint.next.eligible_at_ms == prior.eligible_at_ms,
                "verification retry replay changed within its owner generation"
            );
            return Ok(prior.eligible_at_ms);
        }
        // Count the failure/dependency epoch, not attempts to work on it.
        // Looking up its own history also prevents alternating failure classes
        // or dependency toggles from erasing an unchanged failure's backoff.
        let matching=if previous.as_ref().is_some_and(|(_,prior)|
            prior.failure_code==failure_code && prior.dependency_sha256==dependency_sha256) {
            previous.clone()
        } else {
            let raw:Option<(i64,String)>=tx.query_row(
                "SELECT seq,payload FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry' AND coalesce(json_extract(payload,'$.domain'),CASE WHEN json_extract(payload,'$.failure_code')='acceptance verifier infrastructure' THEN 'acceptance' ELSE 'ci' END)=?4 AND json_extract(payload,'$.failure_code')=?2 AND json_extract(payload,'$.dependency_sha256')=?3 ORDER BY seq DESC LIMIT 1",
                params![lease.workflow,failure_code,dependency_sha256,domain.key()],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            raw.map(|(seq,raw)|decode(&raw).map(|retry|(seq,retry))).transpose()?
        };
        let failures=if let Some((seq,prior))=matching {
            if gate_passed_since(&tx,lease,seq,prior.failed_at_ms,domain,now)? {1}
            else {prior.failures.saturating_add(1)}
        } else {1};
        let delay = 300_000_i64
            .saturating_mul(1 << (failures - 1).min(4))
            .min(3_600_000);
        let retry = Retry {
            version:if domain==VerificationDomain::NativeHandoffPreparation {3}else{2},
            stage: Some(checkpoint.stage),
            domain:if domain==VerificationDomain::NativeHandoffPreparation {Some(domain)}else{None},
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
            reason:retry_reason.into(),
            action:retry_action.into(),
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
    fn preparation_failure(lease:&Lease,binding:&Binding,source:char)->augmentagent_channel_core::native_vm::ownership::NoPendingPreparationFailure {
        use augmentagent_channel_core::native_vm::ownership::*;
        NoPendingPreparationFailure::new(PreparationFingerprintInput {workflow_id:lease.workflow.clone(),workflow_binding:serde_json::to_value(binding).unwrap(),original_binding_sha256:Some("a".repeat(64)),source_registration_sha256:Some(source.to_string().repeat(64)),latest_input_sha256:Some("c".repeat(64)),selected_policy_sha256:None,capacity_class:PreparationCapacity::default()},TransferReason::LockBusy).unwrap()
    }
    #[test]
    fn native_preparation_retry_persists_capped_backoff_without_charges_or_fake_requests() {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");let (mut store,mut lease,binding)=setup(&path);let mut now=30;
        let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Intake;store.checkpoint(&lease,&checkpoint,22).unwrap();
        for delay in [300_000,600_000,1_200_000,2_400_000,3_600_000,3_600_000] {
            let failure=preparation_failure(&lease,&binding,'b');let before=store.snapshot(&lease.workflow).unwrap().unwrap();let allowance=store.merge_allowance(now,5).unwrap();
            let due=store.defer_native_handoff_preparation(&lease,&binding,&failure,now).unwrap();assert_eq!(due,now+delay);
            let events=store.events(&lease.workflow).unwrap();assert_eq!(store.defer_native_handoff_preparation(&lease,&binding,&failure,now+1).unwrap(),due);assert_eq!(store.events(&lease.workflow).unwrap(),events);
            let after=store.snapshot(&lease.workflow).unwrap().unwrap();assert_eq!(after.attempts,before.attempts);assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.progress_at_ms,before.progress_at_ms);assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(store.merge_allowance(now+1,5).unwrap(),allowance);
            assert_eq!(after.checkpoint.next.action,"retry_native_handoff_preparation");
            let raw:String=store.conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|r|r.get(0)).unwrap();let payload:serde_json::Value=serde_json::from_str(&raw).unwrap();assert_eq!(payload["domain"],"native_handoff_preparation");assert!(payload.get("transfer_id").is_none() && payload.get("observation_revision").is_none());
            store.release(&lease,now+2).unwrap();drop(store);store=WorkflowStore::open(&path).unwrap();assert!(store.claim_productive(&lease.workflow,"early",due-1,60_000).unwrap().is_none());lease=store.claim_productive(&lease.workflow,"next",due,60_000).unwrap().unwrap();now=due+1;
        }
    }
    #[test]
    fn native_preparation_retry_is_separate_from_ci_acceptance_epochs_and_gate_success() {
        let directory=tempfile::tempdir().unwrap();let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"));let failure=preparation_failure(&lease,&binding,'b');
        let due=store.defer_native_handoff_preparation(&lease,&binding,&failure,30).unwrap();store.release(&lease,31).unwrap();let lease=store.claim_productive(&lease.workflow,"ci",due,60_000).unwrap().unwrap();
        store.record_verification_gate_passed(&lease,&binding,VerificationDomain::Ci,due+1).unwrap();store.record_verification_gate_passed(&lease,&binding,VerificationDomain::Acceptance,due+2).unwrap();
        let ci_due=store.defer_verification_infrastructure(&lease,&binding,failure.reason().code(),failure.fingerprint(),due+3).unwrap();assert_eq!(ci_due,due+3+300_000,"native epoch leaked into CI");
        store.release(&lease,due+4).unwrap();let lease=store.claim_productive(&lease.workflow,"native",ci_due,60_000).unwrap().unwrap();
        assert_eq!(store.defer_native_handoff_preparation(&lease,&binding,&failure,ci_due+1).unwrap(),ci_due+1+600_000,"CI or acceptance success cleared native preparation debt");
    }
    #[test]
    fn native_preparation_retry_changed_inputs_reset_epoch_but_wrong_scope_cannot_schedule() {
        let directory=tempfile::tempdir().unwrap();let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"));let failure=preparation_failure(&lease,&binding,'b');let due=store.defer_native_handoff_preparation(&lease,&binding,&failure,30).unwrap();store.release(&lease,31).unwrap();
        let lease=store.claim(&lease.workflow,"changed-input",due,60_000).unwrap().unwrap();let changed=preparation_failure(&lease,&binding,'d');assert_eq!(store.defer_native_handoff_preparation(&lease,&binding,&changed,due+1).unwrap(),due+1+300_000);
        let mut foreign=lease.clone();foreign.workflow="another/repo#9".into();let wrong=preparation_failure(&foreign,&binding,'d');let events=store.events(&lease.workflow).unwrap();assert!(store.defer_native_handoff_preparation(&lease,&binding,&wrong,due+2).is_err());assert_eq!(store.events(&lease.workflow).unwrap(),events);
    }
    #[test]
    fn native_preparation_retry_cannot_overwrite_same_generation_debt_or_hide_unknown_domain() {
        for fault in ["same-generation","unknown-domain"] {
            let directory=tempfile::tempdir().unwrap();let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"));let failure=preparation_failure(&lease,&binding,'b');
            store.defer_native_handoff_preparation(&lease,&binding,&failure,30).unwrap();
            if fault=="unknown-domain" {store.conn.execute("UPDATE workflow_event SET payload=json_set(payload,'$.domain','unknown') WHERE workflow=?1 AND kind='verification_infrastructure_retry'",[&lease.workflow]).unwrap();}
            let events=store.events(&lease.workflow).unwrap();let before=store.snapshot(&lease.workflow).unwrap().unwrap();
            assert!(store.defer_verification_infrastructure(&lease,&binding,"network",&"e".repeat(64),31).is_err(),"{fault}");
            assert_eq!(store.events(&lease.workflow).unwrap(),events);assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint.next,before.checkpoint.next);
        }
    }
    #[test]
    fn unchanged_dependency_keeps_backoff_across_new_heads_stage_starts_and_restarts() {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
        let (mut store,mut lease,mut binding)=setup(&path);
        let mut due=store.defer_verification_infrastructure(&lease,&binding,
            "acceptance verifier infrastructure",&"a".repeat(64),30).unwrap();
        store.release(&lease,31).unwrap();
        for (attempt,delay) in [600_000,1_200_000,2_400_000,3_600_000].into_iter().enumerate() {
            drop(store);store=WorkflowStore::open(&path).unwrap();
            lease=store.claim_productive(&lease.workflow,&format!("worker-{attempt}"),due,60_000).unwrap().unwrap();
            let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            binding.head=format!("changed-head-{attempt}");binding.evidence_hash=format!("changed-context-{attempt}");
            checkpoint.binding=binding.clone();checkpoint.stage=Stage::Build;
            checkpoint.next=NextAction {reason:"repair_started".into(),action:"repair".into(),eligible_at_ms:0};
            store.checkpoint(&lease,&checkpoint,due+1).unwrap();
            checkpoint.stage=Stage::Verify;checkpoint.next.reason="verification_started".into();
            store.checkpoint(&lease,&checkpoint,due+2).unwrap();
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();
            let allowance=store.merge_allowance(due+3,5).unwrap();
            let next=store.defer_verification_infrastructure(&lease,&binding,
                "acceptance verifier infrastructure",&"a".repeat(64),due+3).unwrap();
            assert_eq!(next,due+3+delay,"source edits and stage-start checkpoints reset an unchanged failure");
            let after=store.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(after.progress_at_ms,before.progress_at_ms);
            assert_eq!(after.checkpoint.binding,before.checkpoint.binding);
            assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
            assert_eq!(after.model_calls,before.model_calls);
            assert_eq!(store.merge_allowance(due+4,5).unwrap(),allowance);
            store.release(&lease,due+4).unwrap();due=next;
        }
    }

    #[test]
    fn only_a_successful_gate_in_the_affected_domain_resets_backoff() {
        let directory=tempfile::tempdir().unwrap();let path=directory.path().join("workflow.db");
        let (mut store,lease,binding)=setup(&path);
        let due=store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),30).unwrap();
        store.release(&lease,31).unwrap();
        let lease=store.claim_productive(&lease.workflow,"next",due,60_000).unwrap().unwrap();
        let before=store.snapshot(&lease.workflow).unwrap().unwrap();
        store.record_verification_gate_passed(&lease,&binding,VerificationDomain::Ci,due+1).unwrap();
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before,"gate evidence mutated scheduling/accounting");
        let next=store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),due+2).unwrap();
        assert_eq!(next,due+2+600_000,"CI success cleared unresolved acceptance verification");
        store.release(&lease,due+3).unwrap();
        let lease=store.claim_productive(&lease.workflow,"recovered",next,60_000).unwrap().unwrap();
        store.record_verification_gate_passed(&lease,&binding,VerificationDomain::Acceptance,next+1).unwrap();
        drop(store);let mut store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),next+2).unwrap(),next+2+300_000);
    }

    #[test]
    fn alternating_failure_keys_cannot_erase_an_unchanged_epochs_backoff() {
        let directory=tempfile::tempdir().unwrap();let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"));
        let first=store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),30).unwrap();
        store.release(&lease,31).unwrap();
        let lease=store.claim_productive(&lease.workflow,"different-failure",first,60_000).unwrap().unwrap();
        let different=store.defer_verification_infrastructure(&lease,&binding,"network",&"b".repeat(64),first+1).unwrap();
        assert_eq!(different,first+1+300_000);
        store.release(&lease,first+2).unwrap();
        let lease=store.claim_productive(&lease.workflow,"same-failure",different,60_000).unwrap().unwrap();
        assert_eq!(store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),different+1).unwrap(),different+1+600_000);
    }

    #[test]
    fn gate_completion_is_fenced_preserves_holds_and_cannot_credit_corrupt_history() {
        for fault in ["stale","wrong-binding","paused","held","version","unknown-field","binding-field","future-generation","future-clock"] {
            let directory=tempfile::tempdir().unwrap();let (mut store,lease,binding)=setup(&directory.path().join("workflow.db"));
            let due=store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),30).unwrap();
            store.release(&lease,31).unwrap();
            let lease=store.claim_productive(&lease.workflow,"next",due,60_000).unwrap().unwrap();
            let mut passed_lease=lease.clone();let mut passed_binding=binding.clone();
            match fault {
                "stale"=>passed_lease.generation+=1,
                "wrong-binding"=>passed_binding.head="other-head".into(),
                "paused"=>{store.set_owner_pause(&lease.workflow,"github:owner",0,true,due+1).unwrap();},
                "held"=>{let mut checkpoint=store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
                    checkpoint.stage=Stage::Held;store.checkpoint(&lease,&checkpoint,due+1).unwrap();},
                _=>{},
            }
            let before=store.snapshot(&lease.workflow).unwrap().unwrap();let events=store.events(&lease.workflow).unwrap();
            let result=store.record_verification_gate_passed(&passed_lease,&passed_binding,VerificationDomain::Acceptance,due+2);
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
            if matches!(fault,"stale"|"wrong-binding") {
                assert!(result.is_err());assert_eq!(store.events(&lease.workflow).unwrap(),events);continue;
            }
            result.unwrap();
            if matches!(fault,"paused"|"held") {
                assert!(store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),due+3).is_err());
                assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);continue;
            }
            let raw:String=store.conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='verification_gate_passed' ORDER BY seq DESC LIMIT 1",[&lease.workflow],|row|row.get(0)).unwrap();
            let mut value:serde_json::Value=serde_json::from_str(&raw).unwrap();
            match fault {
                "version"=>value["version"]=2.into(),
                "unknown-field"=>value["unrecognized"]=true.into(),
                "binding-field"=>value["binding"]["unrecognized"]=true.into(),
                "future-generation"=>{store.conn.execute("UPDATE workflow_event SET generation=?2 WHERE workflow=?1 AND kind='verification_gate_passed'",params![lease.workflow,lease.generation+1]).unwrap();},
                "future-clock"=>{store.conn.execute("UPDATE workflow_event SET at_ms=?2 WHERE workflow=?1 AND kind='verification_gate_passed'",params![lease.workflow,due+100]).unwrap();},
                _=>unreachable!(),
            }
            store.conn.execute("UPDATE workflow_event SET payload=?2 WHERE workflow=?1 AND kind='verification_gate_passed'",params![lease.workflow,value.to_string()]).unwrap();
            let events=store.events(&lease.workflow).unwrap();
            assert!(store.defer_verification_infrastructure(&lease,&binding,"acceptance verifier infrastructure",&"a".repeat(64),due+3).is_err(),"{fault}");
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(),before);
            assert_eq!(store.events(&lease.workflow).unwrap(),events);
        }
    }

    #[test]
    fn ci_infrastructure_retry_preserves_build_and_review_stages_without_new_spend() {
        for stage in [
            Stage::Scope,
            Stage::Build,
            Stage::Verify,
            Stage::Review,
            Stage::Reconcile,
            Stage::AwaitChecks,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut store, lease, binding) = setup(&dir.path().join("workflow.db"));
            let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            checkpoint.stage = stage;
            store.checkpoint(&lease, &checkpoint, 22).unwrap();
            store
                .charge(
                    &lease,
                    "prior-builder",
                    ResourceCharge {
                        model_calls: 2,
                        ..Default::default()
                    },
                    23,
                )
                .unwrap();
            let before = store.snapshot(&lease.workflow).unwrap().unwrap();
            let allowance = store.merge_allowance(30, 5).unwrap();
            let eligible = store.defer_verification_infrastructure(&lease, &binding, "network", &"a".repeat(64), 30)
                .expect("infrastructure during a revision must defer without inventing a Verify transition");
            let after = store.snapshot(&lease.workflow).unwrap().unwrap();
            assert_eq!(eligible, 300_030);
            assert_eq!(after.checkpoint.stage, stage);
            assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
            assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
            assert_eq!(after.progress_at_ms, before.progress_at_ms);
            assert_eq!(after.model_calls, before.model_calls);
            assert_eq!(after.attempts, before.attempts);
            assert_eq!(store.merge_allowance(30, 5).unwrap(), allowance);
        }
    }
    #[test]
    fn ci_retry_rejects_non_ci_stages_without_mutation() {
        for stage in [
            Stage::Intake,
            Stage::Publish,
            Stage::Merge,
            Stage::Held,
            Stage::Retired,
            Stage::Merged,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut store, lease, binding) = setup(&dir.path().join("workflow.db"));
            let mut cp = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
            cp.stage = stage;
            store.checkpoint(&lease, &cp, 22).unwrap();
            let before = store.snapshot(&lease.workflow).unwrap().unwrap();
            let events = store.events(&lease.workflow).unwrap();
            assert!(store
                .defer_verification_infrastructure(&lease, &binding, "network", &"a".repeat(64), 30)
                .is_err());
            assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(), before);
            assert_eq!(store.events(&lease.workflow).unwrap(), events);
        }
    }

    #[test]
    fn legacy_verify_retry_keeps_its_backoff_after_stage_aware_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let (mut store, lease, binding) = setup(&dir.path().join("workflow.db"));
        let due = store
            .defer_verification_infrastructure(&lease, &binding, "network", &"a".repeat(64), 30)
            .unwrap();
        let legacy = Retry {
            domain:None,
            version: 1,
            stage: None,
            binding: binding.clone(),
            failure_code: "network".into(),
            dependency_sha256: "a".repeat(64),
            generation: lease.generation,
            failures: 1,
            failed_at_ms: 30,
            eligible_at_ms: due,
        };
        store.conn.execute("UPDATE workflow_event SET payload=?2 WHERE workflow=?1 AND kind='verification_infrastructure_retry'",
            params![lease.workflow, serde_json::to_string(&legacy).unwrap()]).unwrap();
        assert_eq!(
            store
                .defer_verification_infrastructure(&lease, &binding, "network", &"a".repeat(64), 31)
                .unwrap(),
            due
        );
        store.release(&lease, 32).unwrap();
        let resumed = store
            .claim(&lease.workflow, "b", due, 60_000)
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .defer_verification_infrastructure(
                    &resumed,
                    &binding,
                    "network",
                    &"a".repeat(64),
                    due + 1
                )
                .unwrap(),
            due + 1 + 600_000
        );
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
        store.record_verification_gate_passed(&lease,&binding,VerificationDomain::Ci,now+3).unwrap();
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
            "a successful verification gate did not reset its failure backoff"
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
            "wrong-stage",
            "version-stage",
            "unknown-field",
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
                "wrong-stage" | "version-stage" | "unknown-field" => {
                    store
                        .defer_verification_infrastructure(
                            &lease,
                            &binding,
                            "network",
                            &"a".repeat(64),
                            22,
                        )
                        .unwrap();
                    let raw: String = store.conn.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='verification_infrastructure_retry' ORDER BY seq DESC LIMIT 1",
                        [&lease.workflow], |row| row.get(0)).unwrap();
                    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
                    match fault {
                        "wrong-stage" => {
                            value["stage"] = serde_json::to_value(Stage::Build).unwrap()
                        }
                        "version-stage" => value["version"] = 1.into(),
                        _ => value["unexpected"] = true.into(),
                    }
                    store.conn.execute("UPDATE workflow_event SET payload=?2 WHERE workflow=?1 AND kind='verification_infrastructure_retry'",
                        params![lease.workflow, value.to_string()]).unwrap();
                }
                "future-generation" | "future-clock" => {
                    let failed_at = if fault == "future-clock" { 50 } else { 22 };
                    let retry = Retry {
                        domain:None,
                        version: 1,
                        stage: None,
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
