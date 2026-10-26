//! Production bridge between the autoship lanes and durable workflow state.
//! Included beneath self_improve so Git commands use the same sanitized runner.
use super::*;
use crate::autoship_workflow::{
    self as durable, Binding, Checkpoint, Lease, NextAction, Operation, OperationKind,
    ResourceCharge, Review, Stage, WorkflowStore,
};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};

const LEASE_MS: i64 = 10 * 60 * 1000;
const RENEW_SECS: u64 = 60;

pub(super) struct Start<'a> {
    pub repo_root: &'a Path,
    pub worktree: &'a Path,
    /// Canonical trusted GitHub owner/name; never inferred from issue prose.
    pub repository: &'a str,
    pub source_issue: Option<u64>,
    pub pr: Option<u64>,
    pub branch: &'a str,
    pub contract: &'a str,
    /// The actual verifier/toolchain/environment identity, excluding secrets.
    pub environment: &'a str,
}

pub(super) fn path() -> PathBuf {
    durable::default_path()
}

fn millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Acquire after trusted candidate resolution. Dropping the guard stops lease
/// renewal but does not delete work or declare subprocess teardown complete.
/// Call finish only after child processes have stopped and artifacts are safe.
pub(super) struct WorkflowGuard {
    store: Arc<Mutex<WorkflowStore>>,
    lease: Lease,
    repository: String,
    worktree: PathBuf,
    contract: String,
    environment: String,
    binding: Binding,
    heartbeat: tokio_util::sync::CancellationToken,
    lease_failure: Arc<Mutex<Option<String>>>,
    started: std::time::Instant,
    accounted_ms: Arc<AtomicU64>,
    released: bool,
    audit: Arc<augmentagent_channel_core::AuditLogger>,
}

fn workflow_audit(
    state: &Path,
    id: &str,
    worktree: Option<&Path>,
) -> Result<Arc<augmentagent_channel_core::AuditLogger>> {
    use std::os::unix::fs::PermissionsExt;
    let root = state
        .parent()
        .context("workflow state has no parent")?
        .join("autoship-audit");
    for path in [&root, &root.join("logs"), &root.join("anchors")] {
        std::fs::create_dir_all(path)?;
        anyhow::ensure!(
            !std::fs::symlink_metadata(path)?.file_type().is_symlink(),
            "workflow audit directory is a symlink"
        );
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut writes = Vec::new();
    if let Some(worktree) = worktree {
        writes.push(worktree.to_path_buf());
    }
    for cache in [
        PathBuf::from(gate_target_dir()),
        crate::build_storage::effective_target(worktree.unwrap_or(Path::new("."))),
    ] {
        if cache.exists() {
            writes.push(cache);
        }
    }
    // Native worker runtime directories are independently allocated outside
    // protected control state; scope admission rejects overlapping roots.
    let key = format!("{:x}", Sha256::digest(id.as_bytes()));
    let logger = augmentagent_channel_core::AuditLogger::protected(
        root.join("logs").join(format!("{key}.jsonl")),
        root.join("anchors").join(format!("{key}.json")),
        &writes,
    )?;
    logger.protected_health()?;
    Ok(Arc::new(logger))
}

/// Free reconciliation shares the builder lease but never hashes the deploy
/// checkout as if it were the reviewed PR. Completed sweep scopes release;
/// a cancelled remote operation retains its durable intent for reconciliation.
pub(super) struct SweepGuard {
    inner: Option<WorkflowGuard>,
}
impl SweepGuard {
    pub(super) async fn acquire(
        repository: &str,
        issue: u64,
        pr: u64,
        branch: &str,
        head: &str,
        recorded: &str,
    ) -> Result<Option<Self>> {
        Self::acquire_at(
            &path(),
            repository,
            issue,
            pr,
            branch,
            head,
            recorded,
            millis(),
        )
    }

    fn acquire_at(
        path: &Path,
        repository: &str,
        issue: u64,
        pr: u64,
        branch: &str,
        head: &str,
        recorded: &str,
        now: i64,
    ) -> Result<Option<Self>> {
        let mut store = WorkflowStore::open(path)?;
        let issue_id = durable::workflow_id(repository, issue)?;
        let id = match store.resolve_pr(repository, pr)? {
            Some(id) => id,
            None => match store.snapshot(&issue_id)? {
                Some(snapshot)
                    if snapshot.branch == branch && snapshot.pr.is_none_or(|p| p == pr) =>
                {
                    issue_id
                }
                _ => durable::pr_workflow_id(repository, pr)?,
            },
        };
        let existing = store.snapshot(&id)?;
        let binding = existing
            .as_ref()
            .map(|s| s.checkpoint.binding.clone())
            .unwrap_or(Binding {
                head: head.into(),
                contract: format!("{:x}", Sha256::digest(recorded.as_bytes())),
                evidence_hash: format!("{:x}", Sha256::digest(recorded.as_bytes())),
            });
        store.register(&id, branch, binding.clone(), now)?;
        let Some(lease) = store.claim(
            &id,
            &format!("sweep:{}", uuid::Uuid::new_v4()),
            now,
            LEASE_MS,
        )?
        else {
            return Ok(None);
        };
        // Validate after claim, never modify another worker's checkpoint.
        let authorized = if existing.is_some() {
            binding.head == head
                && store
                    .events(&id)?
                    .into_iter()
                    .rev()
                    .filter(|(_, kind, _, _)| kind == "review_recorded")
                    .filter_map(|(_, _, body, _)| serde_json::from_str::<Review>(&body).ok())
                    .find(|review| review.binding == binding)
                    .is_some_and(|review| review.approved)
        } else {
            true
        };
        if !authorized {
            store.release(&lease, now)?;
            return Ok(None);
        }
        store.attach_pr_in_repo(&lease, repository, pr, now)?;
        if existing.is_none() {
            store.record_review(
                &lease,
                &Review {
                    reviewer: "legacy-recorded-approval".into(),
                    binding: binding.clone(),
                    approved: true,
                    evidence: recorded.into(),
                },
                now,
            )?;
        }
        let audit = workflow_audit(path, &id, None)?;
        let inner = WorkflowGuard {
            audit,
            store: Arc::new(Mutex::new(store)),
            lease,
            repository: repository.into(),
            worktree: PathBuf::new(),
            contract: String::new(),
            environment: String::new(),
            binding,
            heartbeat: tokio_util::sync::CancellationToken::new(),
            lease_failure: Arc::new(Mutex::new(None)),
            started: std::time::Instant::now(),
            accounted_ms: Arc::new(AtomicU64::new(0)),
            released: false,
        };
        inner.audit_event("sweep_claimed", serde_json::json!({}))?;
        inner.spawn_renewal();
        Ok(Some(Self { inner: Some(inner) }))
    }

    pub(super) async fn validate_remote(&self, repo: &Path, pr: u64, expected: &str) -> Result<()> {
        let inner = self.inner.as_ref().context("released sweep lease")?;
        inner.validate_before_side_effect()?;
        anyhow::ensure!(
            inner.binding.head == expected,
            "sweep approval binding changed"
        );
        let (ok, head, _) = run(
            &gh_bin(),
            &[
                "pr",
                "view",
                &pr.to_string(),
                "--json",
                "headRefOid",
                "--jq",
                ".headRefOid",
            ],
            repo,
        )
        .await?;
        anyhow::ensure!(
            ok && head.trim() == expected,
            "remote PR head changed since review"
        );
        // A network read can span lease expiry; revalidate immediately before mutation.
        inner.validate_before_side_effect()
    }

    pub(super) fn merge_intent(&self) -> Result<Operation> {
        self.inner
            .as_ref()
            .context("released sweep lease")?
            .intent(OperationKind::MergePr)
    }

    pub(super) fn merged(&self, operation: &Operation, pr: u64) -> Result<()> {
        let inner = self.inner.as_ref().context("released sweep lease")?;
        inner.observe_remote(
            operation,
            &format!("github:pr:{pr}:merged:{}", operation.binding.head),
            Some(pr),
        )?;
        let mut store = inner
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?;
        let mut cp = store
            .snapshot(inner.id())?
            .context("missing workflow")?
            .checkpoint;
        cp.stage = Stage::Merged;
        cp.next = NextAction {
            reason: "remote_merge_observed".into(),
            action: "complete".into(),
            eligible_at_ms: millis(),
        };
        store.checkpoint(&inner.lease, &cp, millis())
    }
}
impl Drop for SweepGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            if let Err(error) = inner.finish() {
                warn!("sweep lease release failed: {error:#}");
            }
        }
    }
}

impl WorkflowGuard {
    pub(super) async fn acquire(start: Start<'_>) -> Result<Option<Self>> {
        Self::acquire_at(&path(), start, millis()).await
    }

    async fn acquire_at(state: &Path, start: Start<'_>, now: i64) -> Result<Option<Self>> {
        anyhow::ensure!(
            !start.repository.trim().is_empty(),
            "canonical repository required"
        );
        anyhow::ensure!(
            !start.environment.trim().is_empty(),
            "verifier environment identity required"
        );
        let mut store = WorkflowStore::open(state)?;
        let id = if let Some(pr) = start.pr {
            store
                .resolve_pr(start.repository, pr)?
                .unwrap_or(durable::pr_workflow_id(start.repository, pr)?)
        } else {
            durable::workflow_id(
                start.repository,
                start
                    .source_issue
                    .context("issue or PR identity required")?,
            )?
        };
        let existing = store.snapshot(&id)?;
        let receipts = existing
            .as_ref()
            .map(|s| s.checkpoint.evidence.clone())
            .unwrap_or_default();
        let mut binding =
            git_binding(start.worktree, start.contract, start.environment, &receipts).await?;
        if existing
            .as_ref()
            .is_some_and(|s| s.checkpoint.binding != binding)
        {
            binding = git_binding(start.worktree, start.contract, start.environment, &[]).await?;
        }
        store.register(&id, start.branch, binding.clone(), now)?;
        let owner = format!("{}:{}", std::process::id(), uuid::Uuid::new_v4());
        let Some(lease) = store.claim(&id, &owner, now, LEASE_MS)? else {
            return Ok(None);
        };
        if let Some(pr) = start.pr {
            store.attach_pr_in_repo(&lease, start.repository, pr, now)?;
        }
        let snapshot = store
            .snapshot(&id)?
            .context("registered workflow missing")?;
        // A new commit/contract invalidates approvals, not workflow ownership.
        // Preserve historical evidence in append-only events, not as current
        // receipts that a new build could accidentally reuse.
        if snapshot.checkpoint.binding != binding {
            store.checkpoint(
                &lease,
                &Checkpoint {
                    stage: Stage::Reconcile,
                    binding: binding.clone(),
                    evidence: vec![],
                    next: NextAction {
                        reason: "inputs_changed".into(),
                        action: "reverify".into(),
                        eligible_at_ms: now,
                    },
                },
                now,
            )?;
        }
        let audit = workflow_audit(state, &id, Some(start.worktree))?;
        let guard = Self {
            audit,
            store: Arc::new(Mutex::new(store)),
            lease,
            repository: start.repository.into(),
            worktree: start.worktree.into(),
            contract: start.contract.into(),
            environment: start.environment.into(),
            binding,
            heartbeat: tokio_util::sync::CancellationToken::new(),
            lease_failure: Arc::new(Mutex::new(None)),
            started: std::time::Instant::now(),
            accounted_ms: Arc::new(AtomicU64::new(0)),
            released: false,
        };
        guard.audit_event("workflow_claimed", serde_json::json!({}))?;
        guard.spawn_renewal();
        let _ = start.repo_root; // Kept explicit: caller owns repository/worktree provenance validation.
        Ok(Some(guard))
    }

    fn spawn_renewal(&self) {
        let store = self.store.clone();
        let lease = self.lease.clone();
        let stop = self.heartbeat.clone();
        let failure = self.lease_failure.clone();
        let started = self.started;
        let accounted = self.accounted_ms.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=stop.cancelled()=>break,
                    _=tokio::time::sleep(std::time::Duration::from_secs(RENEW_SECS))=>{}
                }
                let store = store.clone();
                let lease = lease.clone();
                let accounted = accounted.clone();
                let result = tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut store = store
                        .lock()
                        .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
                    let now = millis();
                    store.heartbeat(&lease, now, LEASE_MS)?;
                    charge_elapsed(&mut store, &lease, started, &accounted, now)
                })
                .await;
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(e) => Some(e.to_string()),
                };
                if let Some(error) = error {
                    if let Ok(mut slot) = failure.lock() {
                        *slot = Some(error);
                    }
                    // Do not keep a lost lease alive or report success. The
                    // next checkpoint/side-effect validation must fail closed.
                    break;
                }
            }
        });
    }

    pub(super) fn audit_logger(&self) -> Arc<augmentagent_channel_core::AuditLogger> {
        self.audit.clone()
    }
    pub(super) fn bind_reasoner(&self, reasoner: &FallbackReasoner) -> Result<()> {
        let store = self.store.clone();
        let lease = self.lease.clone();
        self.audit
            .set_model_call_observer(Arc::new(move |_provider| {
                store
                    .lock()
                    .map_err(|_| anyhow::anyhow!("workflow accounting lock poisoned"))?
                    .charge(
                        &lease,
                        &format!("model-call:{}", uuid::Uuid::new_v4()),
                        ResourceCharge {
                            model_calls: 1,
                            ..Default::default()
                        },
                        millis(),
                    )
            }))?;
        reasoner.set_workflow_audit(self.audit.clone())
    }
    pub(super) async fn audit_health(&self) -> Result<()> {
        self.audit.drain_checked().await
    }
    fn audit_event(&self, kind: &str, details: serde_json::Value) -> Result<()> {
        self.audit.record_controller(serde_json::json!({
            "actor": "controller", "workflow": self.lease.workflow, "generation": self.lease.generation,
            "kind": kind, "binding": self.binding, "details": details, "at_ms": millis(),
        }))
    }

    pub(super) fn admit_episode_action(&self, epoch: &str, action: &str) -> Result<bool> {
        self.validate_before_side_effect()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .admit_episode_action(&self.lease, epoch, action, millis())
    }

    pub(super) fn prior_episode_epoch(&self) -> Result<Option<String>> {
        Ok(self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .events(self.id())?
            .into_iter()
            .rev()
            .filter(|(_, kind, _, _)| kind == "episode_observed")
            .filter_map(|(_, _, raw, _)| {
                serde_json::from_str::<crate::autoship_episode::Observation>(&raw).ok()
            })
            .find(|observation| observation.candidate.binding.contract == self.binding.contract)
            .map(|observation| observation.epoch))
    }

    pub(super) fn observe_episode(
        &self,
        observation: &crate::autoship_episode::Observation,
    ) -> Result<crate::autoship_episode::Decision> {
        self.validate_before_side_effect()?;
        let decision = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .observe_episode(&self.lease, observation, millis())?;
        self.audit_event(
            "episode_decision",
            serde_json::json!({"receipt":observation.receipt,"decision":decision}),
        )?;
        Ok(decision)
    }

    pub(super) fn fresh_review_origin_receipt(&self) -> Result<String> {
        self.validate_before_side_effect()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .issue_fresh_review_origin(&self.lease, &self.binding, millis())
    }

    pub(super) fn repository(&self) -> &str {
        &self.repository
    }
    pub(super) fn pr(&self) -> Result<Option<u64>> {
        Ok(self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .snapshot(self.id())?
            .context("workflow missing")?
            .pr)
    }

    pub(super) fn id(&self) -> &str {
        &self.lease.workflow
    }
    pub(super) fn binding(&self) -> &Binding {
        &self.binding
    }

    pub(super) fn bind_acceptance(
        &mut self,
        prepared: &acceptance::PreparedAcceptance,
    ) -> Result<()> {
        self.healthy_lease()?;
        self.contract = format!(
            "registered:{}",
            prepared.baseline.registration.contract_hash()
        );
        self.environment = prepared
            .baseline
            .registration
            .binding(prepared.baseline.registration.baseline_head())?
            .environment_id;
        Ok(())
    }

    fn healthy_lease(&self) -> Result<()> {
        let failure = self
            .lease_failure
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow failure lock poisoned"))?;
        if let Some(error) = failure.as_ref() {
            anyhow::bail!("workflow lease renewal failed: {error}");
        }
        Ok(())
    }

    /// Checkpoint after committed work or a completed, independently verified
    /// stage. Receipt references must describe current inputs; stale bindings
    /// force re-verification rather than replaying an old approval.
    pub(super) async fn checkpoint(
        &mut self,
        stage: Stage,
        reason: &str,
        action: &str,
        eligible_at_ms: i64,
        evidence: &[String],
    ) -> Result<()> {
        self.healthy_lease()?;
        self.audit_health().await?;
        let binding =
            git_binding(&self.worktree, &self.contract, &self.environment, evidence).await?;
        let now = millis();
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
        store.checkpoint(
            &self.lease,
            &Checkpoint {
                stage,
                binding: binding.clone(),
                evidence: evidence.to_vec(),
                next: NextAction {
                    reason: reason.into(),
                    action: action.into(),
                    eligible_at_ms,
                },
            },
            now,
        )?;
        charge_elapsed(
            &mut store,
            &self.lease,
            self.started,
            &self.accounted_ms,
            now,
        )?;
        self.binding = binding;
        self.audit_event(
            "stage_checkpoint",
            serde_json::json!({"stage":stage,"reason":reason,"next_action":action}),
        )?;
        Ok(())
    }

    /// Check immediately before gh/git side effects. Also pass GitHub the
    /// exact expected SHA; this local check cannot fence an in-flight RPC.
    pub(super) fn validate_before_side_effect(&self) -> Result<()> {
        self.healthy_lease()?;
        self.audit.protected_health()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .validate_lease(&self.lease, &self.binding, millis())
    }

    pub(super) fn intent(&self, kind: OperationKind) -> Result<Operation> {
        self.validate_before_side_effect()?;
        let operation = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .begin_operation(&self.lease, kind, &self.binding, millis())?;
        self.audit_event(
            "operation_intent",
            serde_json::json!({"operation_id":operation.id,"operation_kind":kind}),
        )?;
        Ok(operation)
    }

    /// Pending intents mean remote outcome unknown, never "safe to repeat".
    pub(super) fn pending_operations(&self) -> Result<Vec<Operation>> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .pending_operations(&self.lease.workflow)
    }

    pub(super) fn observe_remote(
        &self,
        operation: &Operation,
        receipt: &str,
        pr: Option<u64>,
    ) -> Result<()> {
        self.healthy_lease()?;
        anyhow::ensure!(
            operation.workflow == self.lease.workflow,
            "operation belongs to another workflow"
        );
        let now = millis();
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
        // Alias first: a crash between these two transactions leaves an uncertain
        // operation that reconciles to the already-bound PR, never another create.
        if let Some(pr) = pr {
            store.attach_pr_in_repo(&self.lease, &self.repository, pr, now)?;
        }
        store.complete_operation(&self.lease, &operation.id, receipt, now)?;
        if operation.kind == OperationKind::MergePr {
            let latest = store
                .events(self.id())?
                .into_iter()
                .rev()
                .filter(|(_, kind, _, _)| kind == "episode_observed")
                .filter_map(|(_, _, raw, _)| {
                    serde_json::from_str::<crate::autoship_episode::Observation>(&raw).ok()
                })
                .find(|observation| observation.candidate.binding == operation.binding);
            if let Some(mut observation) = latest.filter(|o| {
                o.review_complete
                    && o.candidate.tests_passed
                    && o.candidate.guards_passed
                    && o.candidate.open_findings.is_empty()
            }) {
                observation.receipt = format!("confirmed-own-merge:{}", operation.id);
                observation.autonomous_merge_observed = true;
                store.observe_episode(&self.lease, &observation, now)?;
            }
        }
        self.audit_event(
            "remote_observed",
            serde_json::json!({"operation_id":operation.id,
            "receipt_sha256":format!("{:x}", Sha256::digest(receipt.as_bytes())), "pr":pr}),
        )
    }

    pub(super) fn record_review(
        &self,
        decision_id: &str,
        approved: bool,
        receipt: &str,
    ) -> Result<()> {
        self.validate_before_side_effect()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .record_review(
                &self.lease,
                &Review {
                    reviewer: decision_id.into(),
                    binding: self.binding.clone(),
                    approved,
                    evidence: receipt.into(),
                },
                millis(),
            )?;
        self.audit_event(
            "review_recorded",
            serde_json::json!({"decision_id":decision_id,"approved":approved,
            "receipt_sha256":format!("{:x}", Sha256::digest(receipt.as_bytes()))}),
        )
    }

    pub(super) fn admit_attempt(&self, purpose: &str, limit: u32) -> Result<bool> {
        self.validate_before_side_effect()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .admit_attempt(&self.lease, purpose, &self.binding, limit, millis())
    }

    pub(super) async fn validate_binding(&self, expected: &Binding) -> Result<()> {
        self.validate_before_side_effect()?;
        let evidence = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .snapshot(&self.lease.workflow)?
            .context("workflow missing")?
            .checkpoint
            .evidence;
        let actual =
            git_binding(&self.worktree, &self.contract, &self.environment, &evidence).await?;
        anyhow::ensure!(
            &actual == expected,
            "workflow evidence changed; fresh verification and review required"
        );
        Ok(())
    }

    /// Reserve only immediately before a real adjudicator call. Each slot is
    /// transactionally persisted, including attempts whose response is lost.
    pub(super) fn reserve_adjudication(&self) -> Result<u32> {
        for prior in 0..crate::autoship_review::MAX_ADJUDICATION_ATTEMPTS {
            if self.admit_attempt(&format!("adjudication-slot-{prior}"), 1)? {
                return Ok(prior);
            }
        }
        anyhow::bail!("adjudication attempts exhausted for this immutable evidence; reconcile infrastructure before retry")
    }

    pub(super) fn latest_review_receipt(&self, prefix: &str) -> Result<Option<String>> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
        Ok(store
            .current_reviews(&self.lease.workflow)?
            .into_iter()
            .filter(|r| r.reviewer.starts_with(prefix))
            .last()
            .map(|r| r.evidence))
    }

    /// Transition stage while retaining the receipt set that reviewers saw.
    pub(super) async fn transition(
        &mut self,
        stage: Stage,
        reason: &str,
        action: &str,
        eligible_at_ms: i64,
    ) -> Result<()> {
        let evidence = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .snapshot(&self.lease.workflow)?
            .context("workflow missing")?
            .checkpoint
            .evidence;
        self.checkpoint(stage, reason, action, eligible_at_ms, &evidence)
            .await
    }

    /// Publish detached resumed work with a remote-head precondition. A lost
    /// response is reconciled against ls-remote before any retry. No force push
    /// is needed; ancestry and the lease both protect intervening remote edits.
    pub(super) async fn push(&mut self, branch: &str) -> Result<String> {
        self.transition(Stage::Publish, "publishing_revision", "push", millis())
            .await?;
        let operation = self.intent(OperationKind::Push)?;
        let target = format!("refs/heads/{branch}");
        let (ok, out, _) = run(
            "git",
            &["ls-remote", "--heads", "origin", &target],
            &self.worktree,
        )
        .await?;
        anyhow::ensure!(ok, "cannot reconcile remote revision before push");
        // Empty expected SHA is a create-if-absent compare-and-swap.
        let remote = out.split_whitespace().next().unwrap_or("");
        if remote == operation.binding.head {
            self.observe_remote(&operation, &format!("git-head:{remote}"), None)?;
            return Ok(remote.to_string());
        }
        if !remote.is_empty() {
            let (ancestor, _, _) = run(
                "git",
                &["merge-base", "--is-ancestor", remote, "HEAD"],
                &self.worktree,
            )
            .await?;
            anyhow::ensure!(
                ancestor,
                "remote PR changed; reconcile before publishing preserved work"
            );
        }
        self.validate_before_side_effect()?;
        self.validate_binding(&operation.binding).await?;
        let expected = format!("--force-with-lease={target}:{remote}");
        let refspec = format!("HEAD:{target}");
        let (pushed, _, _) = run(
            "git",
            &["push", &expected, "origin", &refspec],
            &self.worktree,
        )
        .await?;
        let (observed, out, _) = run(
            "git",
            &["ls-remote", "--heads", "origin", &target],
            &self.worktree,
        )
        .await?;
        let observed_head = out.split_whitespace().next().unwrap_or("");
        anyhow::ensure!(
            observed && observed_head == operation.binding.head,
            "push outcome not verified; preserved pending intent (command success: {pushed})"
        );
        self.observe_remote(&operation, &format!("git-head:{observed_head}"), None)?;
        Ok(observed_head.to_string())
    }

    pub(super) async fn publish_round(
        &mut self,
        branch: &str,
        repo_root: &Path,
        pr: u64,
        round: u32,
        kind: &str,
        findings: &str,
        dry_run: bool,
    ) -> Result<()> {
        if dry_run {
            return Ok(());
        }
        let head = self.push(branch).await?;
        let _ = write_optional_pr_note(
            repo_root,
            &pr.to_string(),
            &round_comment(round, kind, findings, &head),
        )
        .await;
        Ok(())
    }

    /// Caller has waited for worker teardown. Stage/evidence must already be
    /// checkpointed; releasing here never deletes the durable worktree.
    pub(super) fn finish(mut self) -> Result<()> {
        self.audit.protected_health()?;
        self.audit_event("workflow_released", serde_json::json!({}))?;
        self.heartbeat.cancel();
        self.healthy_lease()?;
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
        let now = millis();
        charge_elapsed(
            &mut store,
            &self.lease,
            self.started,
            &self.accounted_ms,
            now,
        )?;
        store.release(&self.lease, now)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for WorkflowGuard {
    fn drop(&mut self) {
        self.heartbeat.cancel();
        if !self.released {
            // Best-effort final usage accounting, never release/cleanup here:
            // dropping an async task is not proof its descendants are gone.
            if let Ok(mut store) = self.store.lock() {
                if let Err(error) = charge_elapsed(
                    &mut store,
                    &self.lease,
                    self.started,
                    &self.accounted_ms,
                    millis(),
                ) {
                    warn!(workflow=%self.lease.workflow,"autoship interrupted usage accounting failed: {error}");
                }
            }
        }
    }
}

fn charge_elapsed(
    store: &mut WorkflowStore,
    lease: &Lease,
    started: std::time::Instant,
    accounted: &AtomicU64,
    now: i64,
) -> Result<()> {
    let elapsed =
        u64::try_from(started.elapsed().as_millis()).context("elapsed runtime overflow")?;
    let prior = accounted.load(Ordering::Relaxed);
    if elapsed <= prior {
        return Ok(());
    }
    store.charge(
        lease,
        &format!("lease:{}:wall:{prior}:{elapsed}", lease.generation),
        ResourceCharge {
            wall_ms: elapsed - prior,
            ..Default::default()
        },
        now,
    )?;
    accounted.store(elapsed, Ordering::Relaxed);
    Ok(())
}

/// Hash only canonical non-secret evidence. Inputs include both committed tree
/// and tracked dirty changes, so a failed stage cannot reuse the previous head's
/// receipt while a builder has left edits in its worktree.
pub(super) async fn git_binding(
    worktree: &Path,
    contract: &str,
    environment: &str,
    receipts: &[String],
) -> Result<Binding> {
    anyhow::ensure!(
        !contract.trim().is_empty() && !environment.trim().is_empty(),
        "contract/environment required"
    );
    let head = git_evidence(worktree, &["rev-parse", "HEAD"]).await?;
    let tree = git_evidence(worktree, &["rev-parse", "HEAD^{tree}"]).await?;
    let base = git_evidence(worktree, &["merge-base", "origin/main", "HEAD"]).await?;
    let diff = git_evidence(worktree, &["diff", "--binary", "HEAD", "--"]).await?;
    let mut receipts = receipts.to_vec();
    receipts.sort();
    receipts.dedup();
    let untracked = untracked_evidence(worktree).await?;
    let evidence = serde_json::to_vec(&(tree, base, environment, diff, untracked, receipts))?;
    Ok(Binding {
        head,
        contract: contract
            .strip_prefix("registered:")
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{:x}", Sha256::digest(contract.as_bytes()))),
        evidence_hash: format!("{:x}", Sha256::digest(evidence)),
    })
}

/// Canonical trusted repository identity shared by both execution lanes.
pub(super) async fn canonical_repository(repo_root: &Path) -> Result<String> {
    let (ok, out, _) = run(
        &gh_bin(),
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner",
            "--jq",
            ".nameWithOwner",
        ],
        repo_root,
    )
    .await?;
    let name = out.trim();
    anyhow::ensure!(
        ok && name.split('/').count() == 2
            && name.split('/').all(|v| !v.is_empty()
                && v.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))),
        "canonical GitHub repository identity unavailable"
    );
    Ok(name.to_ascii_lowercase())
}

async fn untracked_evidence(worktree: &Path) -> Result<Vec<(String, String)>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let (ok, paths, _) = run(
        "git",
        &["ls-files", "--others", "--exclude-standard", "-z"],
        worktree,
    )
    .await?;
    anyhow::ensure!(ok, "cannot enumerate untracked review evidence");
    let root = worktree.canonicalize()?;
    let mut evidence = Vec::new();
    let mut total = 0usize;
    for name in paths.split('\0').filter(|s| !s.is_empty()) {
        anyhow::ensure!(evidence.len() < 4096, "too many untracked evidence files");
        let relative = Path::new(name);
        anyhow::ensure!(
            relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
            "invalid untracked evidence path"
        );
        let mut path = root.clone();
        for component in relative.components() {
            path.push(component);
            anyhow::ensure!(
                !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
                "symlink cannot supply untracked review evidence"
            );
        }
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "untracked evidence must be a regular file"
        );
        let mut digest = Sha256::new();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            total = total
                .checked_add(n)
                .context("untracked evidence size overflow")?;
            anyhow::ensure!(total <= 64 * 1024 * 1024, "untracked review evidence exceeds 64 MiB; commit or remove build artifacts before review");
            digest.update(&buffer[..n]);
        }
        evidence.push((name.to_string(), format!("{:x}", digest.finalize())));
    }
    evidence.sort();
    Ok(evidence)
}

/// Reuse interrupted work. Never delete a per-PR worktree to make room for a
/// new attempt; a mismatched repository is a hard error, not reclaimable data.
pub(super) async fn prepare_resume_worktree(
    repo_root: &Path,
    pr: u64,
    branch: &str,
) -> Result<PathBuf> {
    anyhow::ensure!(pr > 0, "PR identity required");
    let path = repo_root
        .join(".self-improve-worktrees")
        .join(format!("resume-pr-{pr}"));
    let (ok, _, _) = run("git", &["fetch", "origin", branch], repo_root).await?;
    anyhow::ensure!(ok, "cannot refresh resumed PR branch");
    if path.exists() {
        anyhow::ensure!(
            !std::fs::symlink_metadata(&path)?.file_type().is_symlink(),
            "resume worktree path is a symlink"
        );
        let expected = git_evidence(
            repo_root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        let actual = git_evidence(
            &path,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .await?;
        anyhow::ensure!(
            Path::new(&expected).canonicalize()? == Path::new(&actual).canonicalize()?,
            "preserved resume worktree belongs to another repository"
        );
        return Ok(path);
    }
    let (ok, _, error) = run(
        "git",
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            &format!("origin/{branch}"),
        ],
        repo_root,
    )
    .await?;
    anyhow::ensure!(ok, "cannot create resumed PR worktree: {error}");
    Ok(path)
}

async fn git_evidence(worktree: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, _) = run("git", args, worktree).await?;
    anyhow::ensure!(ok, "cannot establish workflow Git evidence");
    Ok(out.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
                .status
                .success())
        };
        git(&["init", "-q"]);
        std::fs::write(dir.path().join("code.txt"), "baseline\n").unwrap();
        git(&["add", "code.txt"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "baseline",
        ]);
        git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
        dir
    }
    #[tokio::test]
    async fn real_git_binding_changes_with_dirty_code_environment_and_contract() {
        let dir = repo();
        let a = git_binding(dir.path(), "C1", "rust-version-a", &[])
            .await
            .unwrap();
        assert_eq!(
            a,
            git_binding(dir.path(), "C1", "rust-version-a", &[])
                .await
                .unwrap()
        );
        std::fs::write(dir.path().join("code.txt"), "changed\n").unwrap();
        let b = git_binding(dir.path(), "C1", "rust-version-a", &[])
            .await
            .unwrap();
        assert_eq!(a.head, b.head);
        assert_ne!(a.evidence_hash, b.evidence_hash);
        let c = git_binding(dir.path(), "C1", "rust-version-b", &[])
            .await
            .unwrap();
        assert_ne!(b.evidence_hash, c.evidence_hash);
        assert_ne!(
            c.contract,
            git_binding(dir.path(), "C2", "rust-version-b", &[])
                .await
                .unwrap()
                .contract
        );
    }
    #[tokio::test]
    async fn missing_review_does_not_replenish_fresh_episode_budget() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let mut guard = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        let mut observation = crate::autoship_episode::Observation {
            receipt: "review-0".into(),
            epoch: "contract/base/environment".into(),
            candidate: crate::autoship_episode::Candidate {
                binding: guard.binding().clone(),
                tests_passed: true,
                guards_passed: true,
                verified_criteria: 2,
                open_findings: std::collections::BTreeSet::from(["F1".into()]),
                lines_changed: 20,
            },
            review_complete: true,
            contradictory_requirements: false,
            infrastructure_failed: false,
            autonomous_merge_observed: false,
        };
        for round in 0..3 {
            observation.receipt = format!("review-{round}");
            guard.observe_episode(&observation).unwrap();
        }
        let missing = IndependentReview::not_run("review transport unavailable".into());
        let recovery = episode::evaluate(&mut guard, &missing, 2, 20)
            .await
            .unwrap();
        assert_eq!(recovery.epoch, "contract/base/environment");
        assert_eq!(
            recovery.decision,
            crate::autoship_episode::Decision::RepairInfrastructure
        );
        observation.receipt = "review-3".into();
        guard.observe_episode(&observation).unwrap();
        observation.receipt = "review-4".into();
        assert!(
            matches!(
                guard.observe_episode(&observation).unwrap(),
                crate::autoship_episode::Decision::Rescope { .. }
            ),
            "transient transport failure must not grant another fresh builder reset"
        );
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn episode_counts_only_own_confirmed_merge_and_model_calls_are_separate() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let guard = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        let reasoner = FallbackReasoner::for_tests(
            vec![],
            augmentagent_channel_core::CooldownLatch::at(state.path().join("cooldowns.json")),
        );
        guard.bind_reasoner(&reasoner).unwrap();
        guard.audit_logger().model_call_admitted("claude").unwrap();
        let observation = crate::autoship_episode::Observation {
            receipt: "review-verified".into(),
            epoch: "contract/base/environment".into(),
            candidate: crate::autoship_episode::Candidate {
                binding: guard.binding().clone(),
                tests_passed: true,
                guards_passed: true,
                verified_criteria: 2,
                open_findings: Default::default(),
                lines_changed: 20,
            },
            review_complete: true,
            contradictory_requirements: false,
            infrastructure_failed: false,
            autonomous_merge_observed: false,
        };
        assert_eq!(
            guard.observe_episode(&observation).unwrap(),
            crate::autoship_episode::Decision::ReadyToMerge
        );
        let store = WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls, 1);
        let merges = || {
            store
                .events(guard.id())
                .unwrap()
                .into_iter()
                .filter(|(_, kind, _, _)| kind == "episode_observed")
                .filter_map(|(_, _, raw, _)| {
                    serde_json::from_str::<crate::autoship_episode::Observation>(&raw).ok()
                })
                .filter(|o| o.autonomous_merge_observed)
                .count()
        };
        assert_eq!(merges(), 0, "LGTM does not count as shipping");
        let op = guard.intent(OperationKind::MergePr).unwrap();
        guard
            .observe_remote(&op, "github:pr:42:merged:fixture", Some(42))
            .unwrap();
        guard
            .observe_remote(&op, "github:pr:42:merged:fixture", Some(42))
            .unwrap();
        assert_eq!(
            merges(),
            1,
            "reconciled duplicate merge receipt counts once"
        );
        let logger = guard.audit_logger();
        guard.finish().unwrap();
        assert!(
            logger.model_call_admitted("claude").is_err(),
            "released workflow cannot admit unaccounted spend"
        );
    }

    #[tokio::test]
    async fn controller_audit_is_anchored_and_lost_rows_block_publication() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let mut guard = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        guard
            .transition(Stage::Review, "candidate_verified", "review", 0)
            .await
            .unwrap();
        let log = guard.audit_logger().path().to_path_buf();
        let records = std::fs::read_to_string(&log).unwrap();
        assert!(records
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|row| row["actor"] == "controller"
                && row["kind"] == "stage_checkpoint"
                && row.get("provider").is_none()));
        std::fs::write(&log, "").unwrap();
        assert!(guard.intent(OperationKind::Push).is_err());
        assert!(guard.record_review("LGTM", true, "approval").is_err());
        assert!(
            workflow_audit(&path, guard.id(), Some(dir.path())).is_err(),
            "restart cannot clear audit failure"
        );
    }

    #[test]
    fn audit_storage_cannot_be_inside_worker_source_root() {
        let worktree = tempfile::tempdir().unwrap();
        let state = worktree.path().join("state/workflow.db");
        std::fs::create_dir_all(state.parent().unwrap()).unwrap();
        assert!(workflow_audit(&state, "owner/repo#7", Some(worktree.path())).is_err());
    }

    #[tokio::test]
    async fn sweep_excludes_active_builder_releases_and_fences_lost_lease() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let now = millis();
        let builder = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
        let head = builder.binding().head.clone();
        builder
            .record_review("approved", true, "native approval")
            .unwrap();
        assert!(SweepGuard::acquire_at(
            &path,
            "owner/repo",
            7,
            42,
            "fix/seven",
            &head,
            "record",
            millis()
        )
        .unwrap()
        .is_none());
        builder.finish().unwrap();
        let sweep = SweepGuard::acquire_at(
            &path,
            "owner/repo",
            7,
            42,
            "fix/seven",
            &head,
            "record",
            millis(),
        )
        .unwrap()
        .unwrap();
        assert!(sweep.merge_intent().is_ok());
        assert!(SweepGuard::acquire_at(
            &path,
            "owner/repo",
            7,
            42,
            "fix/seven",
            &head,
            "record",
            millis()
        )
        .unwrap()
        .is_none());
        drop(sweep);
        let sweep = SweepGuard::acquire_at(
            &path,
            "owner/repo",
            7,
            42,
            "fix/seven",
            &head,
            "record",
            millis(),
        )
        .unwrap()
        .unwrap();
        let id = sweep.inner.as_ref().unwrap().id().to_string();
        let mut store = WorkflowStore::open(&path).unwrap();
        let successor = store
            .claim(&id, "replacement", millis() + LEASE_MS + 1, LEASE_MS)
            .unwrap()
            .unwrap();
        assert!(
            sweep.merge_intent().is_err(),
            "lost generation may not authorize a merge"
        );
        assert!(store
            .validate_lease(
                &successor,
                &store.snapshot(&id).unwrap().unwrap().checkpoint.binding,
                millis() + LEASE_MS + 2
            )
            .is_ok());
    }

    #[tokio::test]
    async fn sweep_never_reuses_approval_after_evidence_changes_at_same_head() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let mut builder = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        let head = builder.binding().head.clone();
        builder
            .record_review("approved", true, "native approval")
            .unwrap();
        std::fs::write(dir.path().join("code.txt"), "unreviewed revision\n").unwrap();
        builder
            .transition(Stage::Build, "revision_started", "verify", 0)
            .await
            .unwrap();
        builder.finish().unwrap();
        assert!(SweepGuard::acquire_at(
            &path,
            "owner/repo",
            7,
            42,
            "fix/seven",
            &head,
            "old record",
            millis()
        )
        .unwrap()
        .is_none());
    }

    #[tokio::test]
    async fn push_creates_absent_branch_and_reconciles_lost_receipt() {
        let dir = repo();
        let remote = tempfile::tempdir().unwrap();
        assert!(
            run("git", &["init", "--bare", "-q"], remote.path())
                .await
                .unwrap()
                .0
        );
        assert!(
            run(
                "git",
                &["remote", "add", "origin", &remote.path().to_string_lossy()],
                dir.path()
            )
            .await
            .unwrap()
            .0
        );
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let start = || Start {
            repo_root: dir.path(),
            worktree: dir.path(),
            repository: "owner/repo",
            source_issue: Some(7),
            pr: None,
            branch: "fix/seven",
            contract: "C1",
            environment: "rust-a",
        };
        let mut guard = WorkflowGuard::acquire_at(&path, start(), millis())
            .await
            .unwrap()
            .unwrap();
        let head = guard.binding().head.clone();
        assert_eq!(guard.push("fix/seven").await.unwrap(), head);
        assert!(guard.pending_operations().unwrap().is_empty());
        // Simulate push success followed by process death before its receipt.
        std::fs::write(dir.path().join("code.txt"), "new candidate\n").unwrap();
        assert!(
            run("git", &["add", "code.txt"], dir.path())
                .await
                .unwrap()
                .0
        );
        assert!(
            run(
                "git",
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "next"
                ],
                dir.path()
            )
            .await
            .unwrap()
            .0
        );
        guard
            .transition(Stage::Publish, "next_candidate", "push", 0)
            .await
            .unwrap();
        guard.intent(OperationKind::Push).unwrap();
        assert!(
            run(
                "git",
                &["push", "origin", "HEAD:refs/heads/fix/seven"],
                dir.path()
            )
            .await
            .unwrap()
            .0
        );
        guard.finish().unwrap();
        let mut guard = WorkflowGuard::acquire_at(&path, start(), millis())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(guard.pending_operations().unwrap().len(), 1);
        guard.push("fix/seven").await.unwrap();
        assert!(guard.pending_operations().unwrap().is_empty());
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn adjudication_reservations_and_receipts_survive_worker_restart() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let start = || Start {
            repo_root: dir.path(),
            worktree: dir.path(),
            repository: "owner/repo",
            source_issue: Some(7),
            pr: None,
            branch: "fix/seven",
            contract: "C1",
            environment: "rust-a",
        };
        let guard = WorkflowGuard::acquire_at(&path, start(), millis())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(guard.reserve_adjudication().unwrap(), 0);
        guard
            .record_review("resume-structured-1", false, "receipt-one")
            .unwrap();
        guard.finish().unwrap();
        let mut guard = WorkflowGuard::acquire_at(&path, start(), millis())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            guard
                .latest_review_receipt("resume-structured-")
                .unwrap()
                .as_deref(),
            Some("receipt-one")
        );
        assert_eq!(guard.reserve_adjudication().unwrap(), 1);
        assert!(guard.reserve_adjudication().is_err());
        std::fs::write(dir.path().join("file"), "changed evidence\n").unwrap();
        guard
            .transition(Stage::Review, "new_evidence", "review", 0)
            .await
            .unwrap();
        assert_eq!(guard.reserve_adjudication().unwrap(), 0);
        assert!(guard
            .latest_review_receipt("resume-structured-")
            .unwrap()
            .is_none());
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn created_pr_resumes_the_original_workflow_without_an_issue() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let start = Start {
            repo_root: dir.path(),
            worktree: dir.path(),
            repository: "owner/repo",
            source_issue: Some(7),
            pr: None,
            branch: "fix/seven",
            contract: "C1",
            environment: "rust-a",
        };
        let guard = WorkflowGuard::acquire_at(&path, start, millis())
            .await
            .unwrap()
            .unwrap();
        let id = guard.id().to_string();
        let op = guard.intent(OperationKind::CreatePr).unwrap();
        guard.observe_remote(&op, "github-pr:42", Some(42)).unwrap();
        guard.finish().unwrap();
        let adopted = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: None,
                pr: Some(42),
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(adopted.id(), id);
        assert!(adopted.pending_operations().unwrap().is_empty());
        adopted.finish().unwrap();
    }
    #[tokio::test]
    async fn interrupted_guard_preserves_pending_intent_and_lease_until_recovery() {
        let dir = repo();
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let guard = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
                pr: None,
                branch: "fix/seven",
                contract: "C1",
                environment: "rust-a",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        let id = guard.id().to_string();
        guard.intent(OperationKind::CreatePr).unwrap();
        drop(guard);
        let mut store = WorkflowStore::open(&path).unwrap();
        assert_eq!(store.pending_operations(&id).unwrap().len(), 1);
        assert!(store
            .claim(&id, "premature", millis(), LEASE_MS)
            .unwrap()
            .is_none());
        assert!(dir.path().join("code.txt").exists());
    }
    #[tokio::test]
    async fn untracked_contents_change_binding_and_symlinks_are_refused() {
        let dir = repo();
        let before = git_binding(dir.path(), "C1", "rust-a", &[]).await.unwrap();
        let new_file = dir.path().join("new_test.rs");
        std::fs::write(&new_file, "assert!(false);").unwrap();
        let first = git_binding(dir.path(), "C1", "rust-a", &[]).await.unwrap();
        std::fs::write(&new_file, "assert!(true);").unwrap();
        let second = git_binding(dir.path(), "C1", "rust-a", &[]).await.unwrap();
        assert_ne!(before.evidence_hash, first.evidence_hash);
        assert_ne!(first.evidence_hash, second.evidence_hash);
        std::fs::remove_file(&new_file).unwrap();
        std::os::unix::fs::symlink("code.txt", &new_file).unwrap();
        assert!(git_binding(dir.path(), "C1", "rust-a", &[]).await.is_err());
    }
}
