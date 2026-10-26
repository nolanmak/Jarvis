//! Production bridge between the autoship lanes and durable workflow state.
//! Included beneath self_improve so Git commands use the same sanitized runner.
use super::*;
use crate::autoship_workflow::{
    self as durable, Binding, Checkpoint, Lease, NextAction, Operation, OperationKind,
    ResourceCharge, Review, Stage, WorkflowStore,
};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Mutex,
};

#[derive(Clone)]
struct BoundedNativeCall {
    workflow:String,
    purpose:String,
    binding:Binding,
    limit:u32,
}
tokio::task_local! {
    static BOUNDED_NATIVE_CALL: BoundedNativeCall;
}

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

/// Separate optional-observation budget; never consumes the shipping attempt
/// reservation. Twelve calls permits a complete bounded canary plus replay.
fn observation_call_cap() -> u64 {
    std::env::var("AUGMENTAGENT_AUTOPR_OBSERVATION_CALL_CAP").ok()
        .and_then(|s|s.parse().ok()).unwrap_or(12)
}
pub(super) fn observation_available(required: u64) -> Result<bool> {
    WorkflowStore::open(&path())?.observation_available(millis(),required,observation_call_cap())
}

pub(super) fn path() -> PathBuf {
    durable::default_path()
}

/// Read GitHub's current base, never a possibly stale local tracking ref.
pub(super) async fn remote_main_head(repo: &Path, repository: &str) -> Result<String> {
    let (ok, out, error) = run(
        &gh_bin(),
        &[
            "api",
            &format!("repos/{repository}/git/ref/heads/main"),
            "--jq",
            ".object.sha",
        ],
        repo,
    )
    .await?;
    let head = out.trim();
    anyhow::ensure!(
        ok && matches!(head.len(), 40 | 64) && head.bytes().all(|c| c.is_ascii_hexdigit()),
        "current remote main unavailable: {}",
        truncate(&error, 240)
    );
    Ok(head.to_ascii_lowercase())
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
    capacity_exhausted: Arc<AtomicBool>,
    released: bool,
    audit: Arc<augmentagent_channel_core::AuditLogger>,
}

/// A run-local view of durable admissions. It survives guard consumption on
/// normal completion, so every early `?` can still settle capacity accurately.
pub(super) struct CapacityAccounting {
    store: Arc<Mutex<WorkflowStore>>,
    audit: Arc<augmentagent_channel_core::AuditLogger>,
    workflow: String,
    calls_before: u64,
    resumed: bool,
}
impl CapacityAccounting {
    pub(super) fn report(&self) -> Result<Option<RunReport>> {
        if !self.audit.resource_capacity_seen() { return Ok(None); }
        let calls_after = self.store.lock().map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .snapshot(&self.workflow)?.context("capacity workflow missing")?.model_calls;
        let paid = calls_after.checked_sub(self.calls_before).context("durable model admissions decreased")?;
        Ok(Some(RunReport {
            message: format!("Autoship awaiting declared run capacity; candidate and evidence preserved ({paid} model admissions this run)"),
            billed: paid > 0,
            held: paid == 0,
            resumed: self.resumed,
        }))
    }
}

pub(super) fn workflow_audit(
    state: &Path,
    id: &str,
    worktree: Option<&Path>,
) -> Result<Arc<augmentagent_channel_core::AuditLogger>> {
    let generation = WorkflowStore::open(state)?.audit_generation(id)?;
    let logger = open_audit_generation(state, id, worktree, generation)?;
    logger.protected_health()?;
    Ok(logger)
}

fn open_audit_generation(
    state: &Path, id: &str, worktree: Option<&Path>, generation: i64,
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
    let key = if generation == 0 { key } else { format!("{key}-generation-{generation}") };
    let logger = augmentagent_channel_core::AuditLogger::protected(
        root.join("logs").join(format!("{key}.jsonl")),
        root.join("anchors").join(format!("{key}.json")),
        &writes,
    )?;
    Ok(Arc::new(logger))
}

fn acquire_audit_generation(
    state: &Path, store: &mut WorkflowStore, lease: &Lease,
    worktree: Option<&Path>, binding: &mut Binding, now: i64,
) -> Result<Arc<augmentagent_channel_core::AuditLogger>> {
    let generation = store.audit_generation(&lease.workflow)?;
    let old = open_audit_generation(state, &lease.workflow, worktree, generation)?;
    if old.protected_health().is_ok() { return Ok(old); }
    // No model-authored evidence or elapsed-time heuristic authorizes recovery.
    // This verifies the anchored old chain and kernel quiescence independently.
    let proof = old.recovery_evidence(lease.generation)?;
    store.validate_lease(lease, binding, now)?;
    let recovery = serde_json::json!({"workflow":lease.workflow,
        "old_audit_generation":generation,"new_audit_generation":lease.generation,
        "lease_generation":lease.generation,"interrupted":proof});
    let next = open_audit_generation(state, &lease.workflow, worktree, lease.generation)?;
    next.protected_health()?;
    next.record_controller(serde_json::json!({"actor":"controller",
        "kind":"audit_generation_recovery","generation":lease.generation,
        "workflow":lease.workflow,"recovery":recovery}))?;
    *binding = store.recover_audit_generation(lease,generation,&recovery,now)?;
    Ok(next)
}

pub(super) fn requires_reverification(repository: &str, pr: u64) -> Result<bool> {
    requires_reverification_at(&path(), repository, pr)
}
fn requires_reverification_at(path: &Path, repository: &str, pr: u64) -> Result<bool> {
    requires_reverification_at_time(path, repository, pr, millis())
}
fn requires_reverification_at_time(path: &Path, repository: &str, pr: u64, now: i64) -> Result<bool> {
    let store = WorkflowStore::open(path)?;
    let Some(id) = store.resolve_pr(repository, pr)? else {
        return Ok(false);
    };
    Ok(store
        .snapshot(&id)?
        .is_some_and(|snapshot| snapshot.checkpoint.next.action == "reverify_candidate"
            || (snapshot.checkpoint.next.action == "await_capacity"
                && snapshot.checkpoint.next.eligible_at_ms <= now
                && snapshot.lease_until_ms <= now)))
}

fn structured_review_base(raw: &str, expected_head: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let binding = value
        .pointer("/verdict/structured/binding")
        .or_else(|| value.pointer("/structured/binding"))
        .or_else(|| value.get("binding"))?;
    if binding.get("head")?.as_str()? != expected_head {
        return None;
    }
    binding
        .get("base")?
        .as_str()
        .filter(|base| !base.is_empty())
        .map(str::to_owned)
}

/// Free reconciliation shares the builder lease but never hashes the deploy
/// checkout as if it were the reviewed PR. Completed sweep scopes release;
/// a cancelled remote operation retains its durable intent for reconciliation.
pub(super) struct SweepGuard {
    inner: Option<WorkflowGuard>,
    reviewed_base: Option<String>,
    source: crate::autoship_contract::AcceptanceSource,
    branch: String,
    worktree_lock: Mutex<Option<RunLock>>,
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
        Self::acquire_source_at(
            path,
            repository,
            Some(issue),
            pr,
            branch,
            head,
            recorded,
            now,
        )
    }

    pub(super) fn acquire_pr(
        repository: &str,
        pr: u64,
        branch: &str,
        head: &str,
        recorded: &str,
    ) -> Result<Option<Self>> {
        Self::acquire_source_at(
            &path(),
            repository,
            None,
            pr,
            branch,
            head,
            recorded,
            millis(),
        )
    }

    fn acquire_source_at(
        path: &Path,
        repository: &str,
        issue: Option<u64>,
        pr: u64,
        branch: &str,
        head: &str,
        recorded: &str,
        now: i64,
    ) -> Result<Option<Self>> {
        let mut store = WorkflowStore::open(path)?;
        let issue_id = match issue {
            Some(issue) => durable::workflow_id(repository, issue)?,
            None => durable::pr_workflow_id(repository, pr)?,
        };

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
        if issue.is_none() && existing.is_none() {
            return Ok(None);
        }
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
        // Bind merge metadata to the protected review that granted authority,
        // not a different cached OpenedPr supplied by a reconciliation caller.
        let reviews: Vec<Review> = store
            .events(&id)?
            .into_iter()
            .rev()
            .take_while(|(_, kind, _, _)| kind != "audit_generation_recovered")
            .filter(|(_, kind, _, _)| kind == "review_recorded")
            .filter_map(|(_, _, body, _)| serde_json::from_str::<Review>(&body).ok())
            .filter(|review| review.binding == binding)
            .collect();
        let structured = reviews.iter().find_map(|review| {
            structured_review_base(&review.evidence, head).map(|base| (review.approved, base))
        });
        let reviewed_base = if existing.is_some() {
            structured.as_ref().map(|(_, base)| base.clone())
        } else {
            structured_review_base(recorded, head)
        };
        let authorized = if existing.is_some() {
            binding.head == head
                && reviews.first().is_some_and(|review| review.approved)
                && structured.as_ref().is_none_or(|(approved, _)| *approved)
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
            capacity_exhausted: Arc::new(AtomicBool::new(false)),
            released: false,
        };
        inner.install_model_accounting()?;
        inner.audit_event("sweep_claimed", serde_json::json!({}))?;
        inner.spawn_renewal();
        Ok(Some(Self {
            inner: Some(inner),
            reviewed_base,
            source: issue
                .map(crate::autoship_contract::AcceptanceSource::Issue)
                .unwrap_or(crate::autoship_contract::AcceptanceSource::PullRequest(pr)),
            branch: branch.into(),
            worktree_lock: Mutex::new(None),
        }))
    }

    pub(super) async fn validate_remote(&self, repo: &Path, pr: u64, expected: &str) -> Result<()> {
        let inner = self.inner.as_ref().context("released sweep lease")?;
        inner.validate_before_side_effect()?;
        anyhow::ensure!(
            inner.binding.head == expected,
            "sweep approval binding changed"
        );
        let reviewed_base = self
            .reviewed_base
            .as_deref()
            .context("recorded approval lacks a structured integration base; refresh review")?;
        anyhow::ensure!(
            remote_main_head(repo, &inner.repository).await? == reviewed_base,
            "main changed since review; integrate and verify the candidate again"
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
        {
            let mut held = self
                .worktree_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("sweep worktree lock poisoned"))?;
            if held.is_none() {
                let path = run_lock_path().with_file_name(format!("self-improve-pr-{pr}.lock"));
                *held = Some(
                    RunLock::try_acquire(&path)?
                        .context("PR checkout is currently being prepared by another worker")?,
                );
            }
        }
        let worktree = prepare_resume_worktree(repo, pr, &self.branch).await?;
        let reference = {
            let store = inner
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?;
            store
                .snapshot(inner.id())?
                .context("sweep workflow missing")?
                .checkpoint
                .evidence
                .into_iter()
                .filter_map(|raw| {
                    serde_json::from_str::<acceptance::CandidateReceiptReference>(&raw).ok()
                })
                .last()
        };
        let checked = async {
            let reference = reference.context("protected acceptance checkpoint missing")?;
            acceptance::assert_sweep_receipt(
                &worktree,
                &acceptance::control_directories(&inner.repository, inner.id())?
                    .workflow_acceptance,
                self.source,
                &reference,
                expected,
            )
            .await
        }
        .await;
        if let Err(error) = checked {
            inner.record_review(
                &format!("sweep-invalidation-{}", uuid::Uuid::new_v4()),
                false,
                &serde_json::json!({"reason":format!("{error:#}"),"head":expected}).to_string(),
            )?;
            let mut store = inner
                .store
                .lock()
                .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?;
            let mut checkpoint = store
                .snapshot(inner.id())?
                .context("sweep workflow missing")?
                .checkpoint;
            checkpoint.stage = Stage::Verify;
            checkpoint.next = NextAction {
                reason: "protected_acceptance_stale".into(),
                action: "reverify_candidate".into(),
                eligible_at_ms: millis(),
            };
            store.checkpoint(&inner.lease, &checkpoint, millis())?;
            return Err(error).context("sweep approval invalidated; verification repair queued");
        }
        // Network and artifact reads can span lease expiry; fence immediately before mutation.
        inner.validate_before_side_effect()
    }

    pub(super) fn record_merge_mutation(
        &self,
        operation: &Operation,
        receipt: &serde_json::Value,
    ) -> Result<()> {
        self.inner
            .as_ref()
            .context("released sweep lease")?
            .record_merge_mutation(operation, receipt)
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
        let audit = acquire_audit_generation(state, &mut store, &lease,
            Some(start.worktree), &mut binding, now)?;
        audit.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
            workflow_id: lease.workflow.clone(), lease_generation: lease.generation,
        })?;
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
            capacity_exhausted: Arc::new(AtomicBool::new(false)),
            released: false,
        };
        guard.install_model_accounting()?;
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

    pub(super) fn capacity_accounting(&self, resumed: bool) -> Result<CapacityAccounting> {
        let calls_before = self.store.lock().map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .snapshot(self.id())?.context("capacity workflow missing")?.model_calls;
        Ok(CapacityAccounting { store:self.store.clone(), audit:self.audit.clone(),
            workflow:self.id().into(), calls_before, resumed })
    }
    pub(super) fn audit_logger(&self) -> Arc<augmentagent_channel_core::AuditLogger> {
        self.audit.clone()
    }
    fn install_model_accounting(&self) -> Result<()> {
        self.audit.require_run_resources()?;
        let capacity_store = self.store.clone();
        let capacity_lease = self.lease.clone();
        let capacity_flag = self.capacity_exhausted.clone();
        self.audit.set_resource_capacity_observer(Arc::new(move |_observation| {
            // Refuse further work even if persistence itself fails. The protected
            // audit already contains the full independently observed resource event.
            capacity_flag.store(true, Ordering::SeqCst);
            let mut store = capacity_store.lock().map_err(|_| anyhow::anyhow!("workflow capacity lock poisoned"))?;
            let mut checkpoint = store.snapshot(&capacity_lease.workflow)?
                .context("capacity workflow missing")?.checkpoint;
            // Resource observations live in the protected audit, not in the
            // acceptance evidence vector whose hash binds the verified candidate.
            checkpoint.next = NextAction {
                reason: "resource_capacity".into(),
                action: "await_capacity".into(),
                eligible_at_ms: millis().saturating_add(300_000),
            };
            // Keep the current stage, verified binding, findings and receipts;
            // capacity exhaustion is not a code defect or a convergence round.
            store.checkpoint(&capacity_lease, &checkpoint, millis())
        }))?;
        let capacity_flag = self.capacity_exhausted.clone();
        let store = self.store.clone();
        let lease = self.lease.clone();
        let observational = self.repository.starts_with("autoship-observation/") || self.repository.starts_with("autoship-canary/");
        self.audit.set_model_call_observer(Arc::new(move |_provider| {
            anyhow::ensure!(!capacity_flag.load(Ordering::SeqCst), "resource_capacity: await_capacity checkpoint retained");
            let mut store = store.lock().map_err(|_| anyhow::anyhow!("workflow accounting lock poisoned"))?;
            let receipt = format!("model-call:{}", uuid::Uuid::new_v4());
            let charge = ResourceCharge { model_calls:1, ..Default::default() };
            if let Ok(bounded)=BOUNDED_NATIVE_CALL.try_with(Clone::clone) {
                anyhow::ensure!(bounded.workflow==lease.workflow,"bounded native call used a different workflow audit sink");
                return store.charge_bounded_call(&lease,&receipt,millis(),observational.then(observation_call_cap),
                    &bounded.purpose,&bounded.binding,bounded.limit);
            }
            if observational {
                store.charge_observation(&lease,&receipt,charge,millis(),observation_call_cap())
            } else {
                store.charge(&lease,&receipt,charge,millis())
            }
        }))
    }
    pub(super) fn bind_reasoner(&self, reasoner: &FallbackReasoner) -> Result<()> {
        // The guard installs exactly one observer at acquisition. Rebinding a
        // builder or attaching a separately constructed reviewer cannot double charge.
        reasoner.set_workflow_audit(self.audit.clone())
    }

    pub(super) fn reserve_canary_calls(&self,calls:u64,allow_shipping:bool)->Result<bool> {
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store poisoned"))?
            .reserve_observation(&self.lease,calls,observation_call_cap(),millis(),allow_shipping)
    }
    pub(super) async fn acquire_observation(key: &str) -> Result<Option<Self>> {
        let state = path();
        let mut store = WorkflowStore::open(&state)?;
        let now = millis();
        if !store.observation_available(now,1,observation_call_cap())? { return Ok(None); }
        let hash = format!("{:x}",Sha256::digest(key.as_bytes()));
        let id = format!("autoship-observation/{hash}");
        let mut binding = Binding {head:hash.clone(),contract:"observation-v1".into(),evidence_hash:hash};
        store.register(&id,"observational",binding.clone(),now)?;
        let Some(lease) = store.claim(&id,&format!("observation:{}",uuid::Uuid::new_v4()),now,LEASE_MS)? else { return Ok(None); };
        // A recovered observation retains its generation binding across later leases.
        binding = store.snapshot(&id)?.context("observation workflow missing")?.checkpoint.binding;
        let audit = acquire_audit_generation(&state,&mut store,&lease,None,&mut binding,now)?;
        audit.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
            workflow_id: lease.workflow.clone(), lease_generation: lease.generation,
        })?;
        let guard = Self { audit,store:Arc::new(Mutex::new(store)),lease,repository:id.clone(),worktree:PathBuf::new(),contract:"observation-v1".into(),environment:"diagnostic".into(),binding,
            heartbeat:tokio_util::sync::CancellationToken::new(),lease_failure:Arc::new(Mutex::new(None)),started:std::time::Instant::now(),accounted_ms:Arc::new(AtomicU64::new(0)),capacity_exhausted:Arc::new(AtomicBool::new(false)),released:false };
        guard.install_model_accounting()?;
        guard.audit_event("observation_claimed",serde_json::json!({"key":key}))?;
        guard.spawn_renewal();
        Ok(Some(guard))
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

    pub(super) fn record_episode_sessions(
        &self,
        sessions: &augmentagent_channel_core::native_session::FreshBuilderSessions,
    ) -> Result<()> {
        self.validate_before_side_effect()?;
        let observations: Vec<_> = sessions.observed().into_iter().map(|s|serde_json::json!({"provider":s.provider.name(),"native_session":s.id,"uncertain":s.uncertain})).collect();
        self.audit_event(
            "episode_native_conversations",
            serde_json::json!({"observed":observations}),
        )
    }

    pub(super) fn episode_review_at_head(
        &self,
        head: &str,
    ) -> Result<Option<crate::autoship_review::ReviewEvidence>> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?;
        for (_, kind, body, _) in store.events(self.id())?.into_iter().rev() {
            if kind != "review_recorded" {
                continue;
            }
            if let Ok(review) = serde_json::from_str::<Review>(&body) {
                if review.binding.head == head && review.reviewer.starts_with("episode-evidence-") {
                    return Ok(Some(serde_json::from_str(&review.evidence)?));
                }
            }
        }
        Ok(None)
    }

    pub(super) fn episode_dispatch(&self, epoch: &str) -> Result<Vec<(String, String, String)>> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .episode_dispatch(self.id(), epoch)
    }
    pub(super) fn checkpoint_episode_dispatch(
        &self,
        epoch: &str,
        action: &str,
        phase: &str,
        payload: &str,
    ) -> Result<()> {
        self.validate_before_side_effect()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .checkpoint_episode_dispatch(&self.lease, epoch, action, phase, payload, millis())
    }
    pub(super) fn admit_episode_action(&self, epoch: &str, action: &str) -> Result<bool> {
        self.check_resource_policy()?;
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
        anyhow::ensure!(!self.capacity_exhausted.load(Ordering::SeqCst),
            "resource_capacity: await_capacity checkpoint retained; reacquire after eligible retry");
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
    pub(super) fn publication_inputs(&self)->(&str,&Path,&str,&str) {
        (&self.repository,&self.worktree,&self.contract,&self.environment)
    }
    pub(super) fn save_publication(&self,body:&str)->Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .save_publication(&self.lease,&self.binding,&self.repository,body,millis())
    }
    pub(super) fn publication_dispatch(&self,operation:&Operation)->Result<Option<(i64,i64,Option<u64>)>> {
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.publication_dispatch(&operation.id)
    }
    pub(super) fn reserve_publication_dispatch(&self,operation:&Operation)->Result<i64> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.reserve_publication_dispatch(&self.lease,operation,millis())
    }
    pub(super) fn record_publication_identity(&self,operation:&Operation,pr:u64)->Result<()> {
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.record_publication_identity(&self.lease,operation,pr,millis())
    }
    pub(super) fn forget_publication(&self)->Result<()> {
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.forget_publication(&self.lease,millis())
    }

    pub(super) fn pending_operations(&self) -> Result<Vec<Operation>> {
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .pending_operations(&self.lease.workflow)
    }

    pub(super) fn record_merge_mutation(
        &self,
        operation: &Operation,
        receipt: &serde_json::Value,
    ) -> Result<()> {
        self.validate_before_side_effect()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .record_merge_mutation(&self.lease, &operation.id, receipt, millis())?;
        self.audit_event(
            "merge_mutation_accepted",
            serde_json::json!({"operation":operation.id,"receipt":receipt}),
        )
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
        if operation.kind == OperationKind::MergePr && store.has_merge_mutation(&operation.id)? {
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

    pub(super) fn review_fragment(&self, key: &str) -> Result<Option<String>> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .review_fragment(&self.lease,&self.binding,key,millis())
    }
    pub(super) fn record_review_fragment(&self, key: &str, payload: &str) -> Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .record_review_fragment(&self.lease,&self.binding,key,payload,millis())?;
        self.audit_event("review_fragment_completed",serde_json::json!({"key":key,"payload_sha256":format!("{:x}",Sha256::digest(payload.as_bytes()))}))
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

    pub(super) fn check_resource_policy(&self) -> Result<()> {
        self.validate_before_side_effect()?;
        augmentagent_channel_core::run_resources::check_requirement("workflow_action", Some(&self.audit))
    }

    pub(super) fn admit_epoch_attempt(
        &self,
        purpose: &str,
        epoch: &str,
        limit: u32,
    ) -> Result<bool> {
        self.check_resource_policy()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .admit_epoch_attempt(&self.lease, purpose, epoch, limit, millis())
    }

    pub(super) fn scope_stage_response(&self,key:&str)->Result<Option<String>> {
        self.validate_before_side_effect()?;self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.scope_stage_response(&self.lease,&self.binding,key,millis())
    }
    pub(super) fn record_scope_stage_response(&self,key:&str,response:&str)->Result<()> {
        self.validate_before_side_effect()?;self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.record_scope_stage_response(&self.lease,&self.binding,key,response,millis())
    }

    pub(super) fn set_pr_readiness_dependency(&self,pr:u64,record:Option<(&str,&str,&str)>)->Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?.set_pr_readiness_dependency(&self.lease,&self.binding,&self.repository,pr,record,millis())
    }

    pub(super) fn set_readiness_dependency(&self,issue:u64,record:Option<(&str,&str,&str,&str)>)->Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?.set_readiness_dependency(&self.lease,&self.binding,&self.repository,issue,record,millis())
    }

    pub(super) async fn bounded_native_calls<F:std::future::Future>(&self,purpose:String,limit:u32,future:F)->F::Output {
        BOUNDED_NATIVE_CALL.scope(BoundedNativeCall{workflow:self.id().to_owned(),purpose,binding:self.binding.clone(),limit},future).await
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
        self.healthy_lease()?;
        self.audit_event("workflow_released", serde_json::json!({}))?;
        self.heartbeat.cancel();
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
    fn control_dir() -> tempfile::TempDir {
        // Managed Cargo puts TMPDIR inside the writable build cache. Control
        // evidence must live outside every worker write root even in fixtures.
        let dir = tempfile::Builder::new()
            .prefix("autoship-control-test-")
            .tempdir_in("/tmp")
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

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
        let state = control_dir();
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
        let state = control_dir();
        let path = state.path().join("workflow.db");
        let guard = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(7),
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
        let reasoner = FallbackReasoner::for_tests(
            vec![],
            augmentagent_channel_core::CooldownLatch::at(state.path().join("cooldowns.json")),
        );
        guard.bind_reasoner(&reasoner).unwrap();
        assert_eq!(guard.audit_logger().native_runtime_owner().unwrap(),
            Some(augmentagent_channel_core::native_vm::RuntimeOwner {
                workflow_id: guard.id().to_owned(), lease_generation: guard.lease.generation,
            }), "runtime retirement must bind to the actual fenced claim, not audit path alone");
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
        assert!(
            !store.has_merge_mutation(&op.id).unwrap(),
            "pending intent alone is not autonomous attribution"
        );
        guard.record_merge_mutation(&op,&serde_json::json!({"client_mutation_id":op.id,"expected_head":op.binding.head,"pr_number":42,"pr_node_id":"PR_fixture","merge_commit":"merged-sha","merged_by":"owner"})).unwrap();
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
        let state = control_dir();
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

    #[test]
    fn stale_artifact_checkpoint_reengages_verification_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = durable::pr_workflow_id("owner/repo", 42).unwrap();
        store
            .register(
                &id,
                "fix/child",
                Binding {
                    head: "head".into(),
                    contract: "contract".into(),
                    evidence_hash: "evidence".into(),
                },
                10,
            )
            .unwrap();
        let lease = store.claim(&id, "worker", 11, 1000).unwrap().unwrap();
        store
            .attach_pr_in_repo(&lease, "owner/repo", 42, 12)
            .unwrap();
        assert!(!requires_reverification_at(&path, "owner/repo", 42).unwrap());
        let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.stage = Stage::Verify;
        checkpoint.next.action = "reverify_candidate".into();
        store.checkpoint(&lease, &checkpoint, 13).unwrap();
        drop(store);
        assert!(requires_reverification_at(&path, "owner/repo", 42).unwrap());
        assert!(!requires_reverification_at(&path, "owner/repo", 43).unwrap());
    }

    #[test]
    fn interrupted_audit_generation_reopens_without_erasing_failure_or_old_approval() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let state = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&state).unwrap();
        let id = durable::pr_workflow_id("owner/repo",42).unwrap();
        let mut binding = Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,"fix/recovery",binding.clone(),10).unwrap();
        let old_lease = store.claim(&id,"old",11,1000).unwrap().unwrap();
        store.record_review(&old_lease,&Review {reviewer:"prior".into(),binding:binding.clone(),approved:true,evidence:"old".into()},12).unwrap();
        let old = open_audit_generation(&state,&id,None,0).unwrap();
        old.require_run_resources().unwrap();
        old.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed",
            "workflow":id,"generation":old_lease.generation})).unwrap();
        drop(old.begin_write().unwrap());
        assert!(old.protected_health().is_err());
        let lease = store.claim(&id,"new",1012,1000).unwrap().unwrap();
        let current = acquire_audit_generation(&state,&mut store,&lease,None,&mut binding,1013).unwrap();
        current.protected_health().unwrap();
        assert!(old.protected_health().is_err(),"the failed sink must remain failed");
        assert!(store.current_reviews(&id).unwrap().is_empty());
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.next.action,"reverify_candidate");
        drop(current);
        drop(store);
        workflow_audit(&state,&id,None).unwrap().protected_health().unwrap();
    }

    /// Run centrally inside a disposable delegated user service, never the daemon.
    #[tokio::test]
    #[ignore = "requires isolated delegated cgroup service and declared run resources"]
    async fn cancelled_real_worker_recovers_under_new_lease_and_executes_again() {
        use tokio::io::AsyncBufReadExt;
        assert_eq!(std::env::var("AUGMENTAGENT_RUN_RESOURCES").unwrap(),"1");
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let state = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&state).unwrap();
        let id = durable::pr_workflow_id("owner/repo",42).unwrap();
        let mut binding = Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,"fix/recovery",binding.clone(),10).unwrap();
        let old_lease = store.claim(&id,"old",11,1000).unwrap().unwrap();
        let old = open_audit_generation(&state,&id,None,0).unwrap();
        old.require_run_resources().unwrap();
        old.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed",
            "workflow":id,"generation":old_lease.generation})).unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c","printf 'ready\n'; exec /bin/sleep 60"])
            .env_clear().stdout(Stdio::piped()).stderr(Stdio::piped());
        let (mut child, resources) = augmentagent_channel_core::run_resources::spawn_command(&command,Some(&old)).unwrap();
        resources.admit("recovery_fixture",Some(&old)).await.unwrap();
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(output.next_line().await.unwrap().as_deref(),Some("ready"));
        drop(old.begin_write().unwrap());
        assert!(old.recovery_evidence(old_lease.generation+1).is_err(),"live descendants prohibit recovery");
        drop(resources); // Actual supervisor cancellation kills/reaps the sleep subtree.
        let status = child.wait().await.unwrap();
        assert!(!status.success());
        drop(store);
        let mut store = WorkflowStore::open(&state).unwrap();
        let lease = store.claim(&id,"new",1012,1000).unwrap().unwrap();
        let recovered = acquire_audit_generation(&state,&mut store,&lease,None,&mut binding,1013).unwrap();
        recovered.require_run_resources().unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c","printf 'verified\n'"]).env_clear()
            .stdout(Stdio::piped()).stderr(Stdio::piped());
        let (mut child, resources) = augmentagent_channel_core::run_resources::spawn_command(&command,Some(&recovered)).unwrap();
        resources.admit("recovery_fixture",Some(&recovered)).await.unwrap();
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        assert_eq!(output.next_line().await.unwrap().as_deref(),Some("verified"));
        assert!(child.wait().await.unwrap().success());
        resources.record_outcome("recovery_fixture",Some(&recovered)).unwrap();
        recovered.protected_health().unwrap();
        assert!(old.protected_health().is_err());
        assert_eq!(store.audit_generation(&id).unwrap(),lease.generation);
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.next.action,"reverify_candidate");
    }

    #[test]
    fn abandoned_recovery_sink_does_not_poison_later_lease() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let state = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&state).unwrap();
        let id = durable::pr_workflow_id("owner/repo",42).unwrap();
        let mut binding = Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,"fix/recovery",binding.clone(),10).unwrap();
        let first = store.claim(&id,"first",11,1000).unwrap().unwrap();
        let old = open_audit_generation(&state,&id,None,0).unwrap();
        old.require_run_resources().unwrap();
        old.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed",
            "workflow":id,"generation":first.generation})).unwrap();
        drop(old.begin_write().unwrap());
        let abandoned = store.claim(&id,"abandoned",1012,1000).unwrap().unwrap();
        old.recovery_evidence(abandoned.generation).unwrap();
        let prepared = open_audit_generation(&state,&id,None,abandoned.generation).unwrap();
        prepared.protected_health().unwrap();
        drop(prepared.begin_write().unwrap()); // crash before pointer commit
        let successor = store.claim(&id,"successor",2013,1000).unwrap().unwrap();
        let recovered = acquire_audit_generation(&state,&mut store,&successor,None,&mut binding,2014).unwrap();
        recovered.protected_health().unwrap();
        assert_eq!(store.audit_generation(&id).unwrap(), successor.generation);
        assert!(prepared.protected_health().is_err());
    }

    #[test]
    fn audit_generation_recovery_does_not_accept_tampered_history() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let state = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&state).unwrap();
        let id = durable::pr_workflow_id("owner/repo",42).unwrap();
        let mut binding = Binding {head:"head".into(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,"fix/recovery",binding.clone(),10).unwrap();
        let old_lease = store.claim(&id,"old",11,1000).unwrap().unwrap();
        let old = open_audit_generation(&state,&id,None,0).unwrap();
        old.require_run_resources().unwrap();
        old.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed",
            "workflow":id,"generation":old_lease.generation})).unwrap();
        drop(old.begin_write().unwrap());
        let key = format!("{:x}",Sha256::digest(id.as_bytes()));
        std::fs::write(dir.path().join("autoship-audit/logs").join(format!("{key}.jsonl")),b"forged").unwrap();
        let lease = store.claim(&id,"new",1012,1000).unwrap().unwrap();
        assert!(acquire_audit_generation(&state,&mut store,&lease,None,&mut binding,1013).is_err());
        assert_eq!(store.audit_generation(&id).unwrap(),0);
    }

    #[test]
    fn capacity_retry_is_admitted_only_when_due_and_unleased_after_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = durable::pr_workflow_id("owner/repo",42).unwrap();
        store.register(&id,"fix/capacity",Binding {head:"head".into(),
            contract:"contract".into(),evidence_hash:"evidence".into()},10).unwrap();
        let lease = store.claim(&id,"worker",11,1000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease,"owner/repo",42,12).unwrap();
        let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.next = NextAction { reason:"resource_capacity".into(),
            action:"await_capacity".into(),eligible_at_ms:500 };
        store.checkpoint(&lease,&checkpoint,13).unwrap();
        drop(store);
        assert!(!requires_reverification_at_time(&path,"owner/repo",42,499).unwrap());
        assert!(!requires_reverification_at_time(&path,"owner/repo",42,500).unwrap(),"live lease excludes repair");
        assert!(requires_reverification_at_time(&path,"owner/repo",42,1012).unwrap());
        let mut reopened = WorkflowStore::open(&path).unwrap();
        let next = reopened.claim(&id,"next",1012,1000).unwrap().unwrap();
        assert!(!requires_reverification_at_time(&path,"owner/repo",42,1013).unwrap());
        reopened.release(&next,1014).unwrap();
        assert!(requires_reverification_at_time(&path,"owner/repo",42,1015).unwrap());
    }

    #[test]
    fn structured_merge_base_is_bound_to_same_reviewed_head() {
        for raw in [
            serde_json::json!({"binding":{"head":"candidate","base":"main-a"}}),
            serde_json::json!({"structured":{"binding":{"head":"candidate","base":"main-a"}}}),
            serde_json::json!({"verdict":{"structured":{"binding":{"head":"candidate","base":"main-a"}}}}),
        ] {
            assert_eq!(
                structured_review_base(&raw.to_string(), "candidate"),
                Some("main-a".into())
            );
            assert!(structured_review_base(&raw.to_string(), "changed-candidate").is_none());
        }
        assert!(
            structured_review_base(r#"{"head":"candidate","approved":true}"#, "candidate")
                .is_none()
        );
    }

    #[tokio::test]
    async fn sweep_excludes_active_builder_releases_and_fences_lost_lease() {
        let dir = repo();
        let state = control_dir();
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
        let state = control_dir();
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
        let state = control_dir();
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
        let state = control_dir();
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
        let state = control_dir();
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
        let state = control_dir();
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
    #[tokio::test]
    async fn unadmitted_verification_keeps_unpaid_capacity_checkpoint() {
        const CHILD: &str = "AUTOSHIP_GATE_CAPACITY_FIXTURE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let isolated = control_dir();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "self_improve::workflow::tests::unadmitted_verification_keeps_unpaid_capacity_checkpoint", "--nocapture"])
                .env(CHILD, isolated.path())
                .env("XDG_STATE_HOME", isolated.path().join("state"))
                .env_remove("AUGMENTAGENT_RUN_RESOURCES")
                .output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            assert!(isolated.path().join("executed").exists());
            return;
        }
        let dir = repo();
        let control = control_dir();
        let path = control.path().join("workflow.db");
        let mut guard = WorkflowGuard::acquire_at(&path, Start {
            repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo",
            source_issue: Some(77), pr: None, branch: "fixture", contract: "C1", environment: "test",
        }, millis()).await.unwrap().unwrap();
        guard.checkpoint(Stage::Verify, "candidate_committed", "verify", 0, &["protected-candidate".into()]).await.unwrap();
        let before = WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        let accounting = guard.capacity_accounting(true).unwrap();
        assert!(guard.admit_epoch_attempt("amendment", "unchanged-inputs", 1).is_err());
        // The production wrapper refused before the durable reservation. The
        // direct store assertion is a test probe, never a production bypass.
        assert!(guard.store.lock().unwrap().admit_epoch_attempt(&guard.lease,
            "amendment", "unchanged-inputs", 1, millis()).unwrap());
        let error = super::super::run_sandboxed("/bin/sh", &["-c", "touch forbidden-execution"], dir.path(),
            &[("PATH".into(), "/usr/bin:/bin".into())], &guard.audit_logger()).await.unwrap_err();
        assert!(error.chain().any(|cause| matches!(cause.downcast_ref::<augmentagent_channel_core::reasoner::ReasonerError>(),
            Some(augmentagent_channel_core::reasoner::ReasonerError::ResourceCapacity { .. }))));
        assert!(!dir.path().join("forbidden-execution").exists());
        let held = accounting.report().unwrap().unwrap();
        assert!(!held.billed && held.held && held.resumed);
        assert_eq!(super::super::baseline_infra_reason(&format!("{error:#}")), Some("declared run resource capacity"));
        let after = WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.action, "await_capacity");
        assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
        assert_eq!(after.model_calls, before.model_calls);
        assert!(guard.validate_before_side_effect().is_err());
        std::fs::write(PathBuf::from(std::env::var_os(CHILD).unwrap()).join("executed"), "ok").unwrap();
    }

    #[tokio::test]
    async fn component_budget_is_consumed_only_at_actual_native_admission() {
        let dir=repo();let control=control_dir();let path=control.path().join("workflow.db");
        let guard=WorkflowGuard::acquire_at(&path,Start{repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,branch:"fixture",contract:"C1",environment:"test"},millis()).await.unwrap().unwrap();
        for _ in 0..5 {
            let result=guard.bounded_native_calls("component".into(),2,async {anyhow::bail!("fixture setup refused before native admission");#[allow(unreachable_code)] Ok::<(),anyhow::Error>(())}).await;
            assert!(result.is_err());
        }
        assert_eq!(WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        for _ in 0..2 {
            let result=guard.bounded_native_calls("component".into(),2,async {
                guard.audit_logger().model_call_admitted("codex")?;
                anyhow::bail!("fixture response lost after actual admission");
                #[allow(unreachable_code)] Ok::<(),anyhow::Error>(())
            }).await;
            assert!(result.is_err());
        }
        assert!(guard.bounded_native_calls("component".into(),2,async {guard.audit_logger().model_call_admitted("codex")}).await.is_err());
        assert_eq!(WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,2);
    }

    #[tokio::test]
    async fn capacity_exhaustion_preserves_evidence_and_blocks_paid_or_publication_work() {
        let dir=repo(); let control=control_dir(); let path=control.path().join("workflow.db");
        let start = || Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,branch:"fixture",contract:"C1",environment:"test"};
        let mut guard=WorkflowGuard::acquire_at(&path,start(),millis()).await.unwrap().unwrap();
        guard.checkpoint(Stage::Review,"verified","review",0,&["protected-acceptance-receipt".into()]).await.unwrap();
        let id=guard.id().to_owned();
        let paid_run = guard.capacity_accounting(false).unwrap();
        // Deterministic observer fixture: one protected admission in this run,
        // then a resumed run sees that admission only as historical spend.
        guard.audit_logger().model_call_admitted("codex").unwrap();
        let subsequent_run = guard.capacity_accounting(true).unwrap();
        let before=WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();
        guard.audit_logger().resource_capacity_exhausted(&serde_json::json!({"phase":"completed","exhausted":true,"memory_events":{"oom_kill":1}})).unwrap();
        assert!(paid_run.report().unwrap().unwrap().billed, "actual admission remains billed after capacity exhaustion");
        assert!(!subsequent_run.report().unwrap().unwrap().billed, "prior workflow admissions do not bill an unadmitted retry");
        assert!(guard.validate_before_side_effect().is_err());
        assert!(guard.audit_logger().model_call_admitted("codex").is_err());
        assert!(guard.checkpoint(Stage::Build,"generic_failure","retry_builder",0,&[]).await.is_err());
        let after=WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage,before.checkpoint.stage);
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);
        assert!(after.checkpoint.evidence.contains(&"protected-acceptance-receipt".to_owned()));
        assert_eq!(after.checkpoint.next.action,"await_capacity");
        assert_eq!(after.model_calls,before.model_calls);
        assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        let eligible=after.checkpoint.next.eligible_at_ms;
        let reclaim_after=eligible.max(after.lease_until_ms)+1;
        assert!(guard.finish().is_err(), "capacity failure cannot falsely record a normal release");
        assert!(WorkflowGuard::acquire_at(&path,start(),eligible-1).await.unwrap().is_none());
        let resumed=WorkflowGuard::acquire_at(&path,start(),reclaim_after).await.unwrap().unwrap();
        assert!(!resumed.capacity_exhausted.load(Ordering::SeqCst));
        let recovered=WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();
        assert_eq!(recovered.checkpoint.binding,before.checkpoint.binding);
        assert_eq!(recovered.checkpoint.evidence,before.checkpoint.evidence);
    }

    #[tokio::test]
    async fn acquired_guard_accounts_direct_reviewers_without_builder_binding() {
        let dir=repo(); let control=control_dir(); let path=control.path().join("workflow.db");
        let guard=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(7),pr:None,branch:"fixture",contract:"C1",environment:"test"},millis()).await.unwrap().unwrap();
        guard.audit_logger().model_call_admitted("codex").unwrap();
        assert_eq!(WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,1);
        let reasoner=FallbackReasoner::for_tests(vec![],augmentagent_channel_core::CooldownLatch::at(control.path().join("cooldowns.json")));
        guard.bind_reasoner(&reasoner).unwrap(); guard.bind_reasoner(&reasoner).unwrap();
        guard.audit_logger().model_call_admitted("codex").unwrap();
        assert_eq!(WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,2,"rebinding cannot duplicate observer charging");
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        let id=guard.id().to_owned(); guard.finish().unwrap();
        let record=WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();
        assert!(record.wall_ms>0,"actual observed guard lifetime must be accounted");
    }

}
