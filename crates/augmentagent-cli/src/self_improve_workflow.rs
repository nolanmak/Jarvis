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
struct OwnerBodyAdmission {
    dispatch:Arc<durable::owner_dispatch::OwnerBodyDispatch>,
    preparation:Arc<acceptance::owner::PreparedOwnerAcceptance>,
    provider:durable::submission::Provider,
}
impl OwnerBodyAdmission {
    fn validate(&self,store:&mut WorkflowStore,lease:&Lease,now:i64)->Result<()> {
        let preparation=self.preparation.activation_binding()?;
        store.validate_prepared_owner_body_dispatch(lease,&self.dispatch,&preparation,self.provider,now)
    }
}

const LEASE_MS: i64 = 10 * 60 * 1000;
const RENEW_SECS: u64 = 60;

pub(super) struct OwnerBodyResult {
    dispatch:Arc<durable::owner_dispatch::OwnerBodyDispatch>,
    response:String,
    observed:augmentagent_channel_core::native_session::ObservedConversation,
}

/// Supply registered intent and product paths without exposing private oracle
/// data or adapter code. Formatting a prompt grants no execution authority.
fn owner_body_prompt(prepared:&acceptance::PreparedAcceptance,contract:&str,body:&str)->Result<String> {
    prepared.verify_registration()?;
    anyhow::ensure!(prepared.baseline.registration.contract_hash()==contract,"owner builder acceptance contract changed");
    anyhow::ensure!(!body.trim().is_empty() && body.len()<=16*1024,"owner builder input exceeds bound");
    let criteria=prepared.proposal.contract.criteria.iter().map(|c|serde_json::json!({"id":c.id,"requirement":c.requirement})).collect::<Vec<_>>();
    let paths=prepared.proposal.contract.allowed_paths.iter().filter(|scope| {
        let path=match scope {crate::autoship_contract::PathScope::File(path)|crate::autoship_contract::PathScope::Directory(path)=>path};
        !prepared.proposal.files.contains_key(path)
    }).collect::<Vec<_>>();
    let prompt=format!("Work on this guarded candidate under registered acceptance {contract}. Preserve every registered requirement and use only the declared product paths and admitted tools. Verifier adapters and oracles remain in private controller storage; do not create verifier copies or change protected control files. Do not publish, push or merge. Return a concise private summary; a model response does not prove tests passed or that shipping occurred.\nRegistered requirements:{}\nDeclared product paths:{}\n\nOwner instruction:\n{}",serde_json::to_string(&criteria)?,serde_json::to_string(&paths)?,body);
    anyhow::ensure!(prompt.len()<=512*1024,"owner builder context requires scoped decomposition");
    Ok(prompt)
}

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
/// Constructed only on a failure path before this module publishes a guard.
/// This attests no provider dispatch in this setup, not native quiescence.
pub(crate) struct UnstartedOwnerTurnSetup {lease:Lease}
impl UnstartedOwnerTurnSetup {pub(crate) fn lease(&self)->&Lease {&self.lease}}

/// Only a guard whose actual intake contract is still unregistered may mint
/// this capability. It cannot be reconstructed from serialized workflow data.
pub(crate) struct InitialOwnerAcceptance {lease:Lease,binding:Binding}
impl InitialOwnerAcceptance {
    pub(crate) fn matches(&self,turn:&durable::owner_turn::OwnerTurn)->bool {
        self.lease.workflow==turn.lease.workflow && self.lease.owner==turn.lease.owner
            && self.lease.generation==turn.lease.generation && self.binding==turn.binding
    }
}

pub(super) struct WorkflowGuard {
    state:PathBuf,
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
    lease: Lease,
    calls_before: u64,
    resumed: bool,
}
impl CapacityAccounting {
    pub(super) fn settle_ci(&self, outcome: Result<RunReport>) -> Result<RunReport> {
        let Err(error) = &outcome else { return outcome; };
        let Some(deferred) = error.downcast_ref::<CiInfrastructureDeferred>() else { return outcome; };
        anyhow::ensure!(deferred.workflow == self.lease.workflow, "CI deferral belongs to another workflow");
        let checkpoint = self.store.lock().map_err(|_|anyhow::anyhow!("retry accounting store poisoned"))?
            .snapshot(&self.lease.workflow)?.context("retry workflow missing")?.checkpoint;
        anyhow::ensure!(checkpoint.next.reason == "verification_infrastructure_retry"
            && checkpoint.next.action == "retry_verification"
            && checkpoint.next.eligible_at_ms == deferred.eligible_at_ms,
            "CI deferral has no matching durable schedule");
        self.retry_report(deferred.to_string())
    }

    pub(super) fn retry_report(&self,message:String)->Result<RunReport> {
        let calls=self.store.lock().map_err(|_|anyhow::anyhow!("retry accounting store poisoned"))?
            .snapshot(&self.lease.workflow)?.context("retry workflow missing")?.model_calls;
        let paid=calls.checked_sub(self.calls_before).context("durable admissions decreased")?;
        // Actual spend and productive progress are independent. A retry must
        // remain deferred even if a builder ran earlier in the same attempt.
        Ok(RunReport{message,billed:paid>0,held:true,resumed:self.resumed})
    }
    pub(super) fn report(&self) -> Result<Option<RunReport>> {
        let store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
        let paused=store.owner_control(&self.lease.workflow)?.is_some_and(|c|c.paused);
        let input_waiting=store.pending_owner_messages(&self.lease.workflow)?>0 && !store.lease_has_owner_turn(&self.lease)?;
        if !paused && !input_waiting && !self.audit.resource_capacity_seen() { return Ok(None); }
        let calls_after = store.snapshot(&self.lease.workflow)?.context("capacity workflow missing")?.model_calls;
        let paid = calls_after.checked_sub(self.calls_before).context("durable model admissions decreased")?;
        Ok(Some(RunReport {
            message: if paused {format!("Autoship owner pause requested; candidate and evidence preserved ({paid} model admissions this run)")}
                else if input_waiting {format!("Autoship awaiting owner input; candidate and evidence preserved ({paid} model admissions this run)")}
                else {format!("Autoship awaiting declared run capacity; candidate and evidence preserved ({paid} model admissions this run)")},
            billed: paid > 0,
            held: input_waiting || paid == 0,
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

fn audit_generation_paths(state:&Path,id:&str,generation:i64)->Result<(PathBuf,PathBuf,PathBuf)> {
    let parent=state.parent().context("workflow state has no parent")?;
    let parent=if parent.as_os_str().is_empty(){Path::new(".")}else{parent}.canonicalize()?;
    let root=parent.join("autoship-audit");let key=format!("{:x}",Sha256::digest(id.as_bytes()));
    let key=if generation==0 {key}else{format!("{key}-generation-{generation}")};
    Ok((root.clone(),root.join("logs").join(format!("{key}.jsonl")),root.join("anchors").join(format!("{key}.json"))))
}
/// Existing registered namespaces are never recreated or health-appended here.
/// Only known generated paths are supported; legacy/vanished authority holds.
fn existing_native_audit(state:&Path,authority:&augmentagent_channel_core::native_vm::ownership::Authority,worktree:Option<&Path>,original:&augmentagent_channel_core::native_vm::ownership::TransferBinding)->Result<Arc<augmentagent_channel_core::AuditLogger>> {
    use std::os::unix::fs::MetadataExt;
    let mut selected=None;
    for generation in [authority.owner.lease_generation,0] {
        let (root,log,anchor)=audit_generation_paths(state,&authority.owner.workflow_id,generation)?;
        if Path::new(&authority.audit_path)!=log {continue;}
        for directory in [&root,&root.join("logs"),&root.join("anchors"),&anchor.with_extension("pending")] {
            let info=std::fs::symlink_metadata(directory)?;
            anyhow::ensure!(info.is_dir() && !info.file_type().is_symlink() && info.uid()==unsafe{libc::geteuid()} && info.mode()&0o077==0,"native audit directory authority unavailable");
        }
        for file in [&log,&anchor,&anchor.with_extension("lock")] {
            let info=std::fs::symlink_metadata(file)?;
            anyhow::ensure!(info.is_file() && !info.file_type().is_symlink() && info.uid()==unsafe{libc::geteuid()} && info.nlink()==1 && info.mode()&0o077==0,"native audit file authority unavailable");
        }
        selected=Some(generation);break;
    }
    let generation=selected.context(augmentagent_channel_core::native_vm::ownership::TransferReason::AuthorityConflict)?;
    let audit=open_audit_generation(state,&authority.owner.workflow_id,worktree,generation)?;
    audit.set_native_runtime_owner(authority.owner.clone())?;audit.set_native_store_binding(original.clone())?;Ok(audit)
}
fn existing_native_sources(state:&Path,authorities:&[augmentagent_channel_core::native_vm::ownership::Authority],worktree:Option<&Path>,original:&augmentagent_channel_core::native_vm::ownership::TransferBinding)->Result<Vec<Arc<augmentagent_channel_core::AuditLogger>>> {
    let mut grouped=std::collections::BTreeMap::new();
    for authority in authorities {
        let old=grouped.entry(authority.audit_path.clone()).or_insert_with(||authority.clone());
        if authority.owner.lease_generation>old.owner.lease_generation {*old=authority.clone();}
    }
    grouped.values().map(|authority|existing_native_audit(state,authority,worktree,original)).collect()
}
fn open_audit_generation(
    state: &Path, id: &str, worktree: Option<&Path>, generation: i64,
) -> Result<Arc<augmentagent_channel_core::AuditLogger>> {
    use std::os::unix::fs::PermissionsExt;
    let (root,log,anchor)=audit_generation_paths(state,id,generation)?;
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
    let logger=augmentagent_channel_core::AuditLogger::protected(log,anchor,&writes)?;
    Ok(Arc::new(logger))
}

fn acquire_audit_generation(state:&Path,store:&mut WorkflowStore,lease:&Lease,worktree:Option<&Path>,binding:&mut Binding,now:i64)->Result<Arc<augmentagent_channel_core::AuditLogger>> {
    match acquire_audit_generation_inner(state,store,lease,worktree,binding,now) {
        Ok(audit)=>Ok(audit),Err(error)=>{store.release(lease,millis()).context("failed to release refused audit acquisition")?;Err(error)}
    }
}
fn acquire_audit_generation_inner(
    state: &Path, store: &mut WorkflowStore, lease: &Lease,
    worktree: Option<&Path>, binding: &mut Binding, now: i64,
) -> Result<Arc<augmentagent_channel_core::AuditLogger>> {
    reconcile_runtime_retirements_in(store, now)?;
    if std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").is_some() {
        // This store is controller-local: no shared workflow mutex exists yet.
        // Each proof keeps original native/session/audit locks while its short
        // workflow transaction resolves only that exact retired namespace.
        durable::native_reconcile::resolve_retired_aborted(store,lease,binding,&millis,&mut |pending,namespace| {
            existing_native_audit(state,namespace,worktree,pending.original_binding())
        })?;
        if let Some((audit,original))=store.native_owner_context(&lease.workflow,lease.generation)? {
            let authority=augmentagent_channel_core::native_vm::ownership::Authority {owner:augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:lease.workflow.clone(),lease_generation:lease.generation},audit_path:audit};
            return existing_native_audit(state,&authority,worktree,&original);
        }
        if let Some(pending)=store.native_pending_request(lease,binding,now)? {
            if pending.request().destination.owner.lease_generation==lease.generation {
                return existing_native_audit(state,&pending.request().destination,worktree,pending.original_binding());
            }
        }
        if !store.initial_native_owner_available(&lease.workflow)? {
            return open_audit_generation(state,&lease.workflow,worktree,lease.generation);
        }
    }

    let generation = store.audit_generation(&lease.workflow)?;
    let old = open_audit_generation(state, &lease.workflow, worktree, generation)?;
    if old.protected_health().is_ok() { return Ok(old); }
    // No model-authored evidence or elapsed-time heuristic authorizes recovery.
    // This verifies the anchored old chain and kernel quiescence independently.
    let proof = old.recovery_evidence(lease.generation)?;
    store.validate_lease(lease, binding, now)?;
    let recovery = serde_json::json!({"workflow":lease.workflow,
        "old_audit_generation":generation,"old_audit_path":old.path(),"new_audit_generation":lease.generation,
        "lease_generation":lease.generation,"interrupted":proof});
    let next = open_audit_generation(state, &lease.workflow, worktree, lease.generation)?;
    next.protected_health()?;
    next.record_controller(serde_json::json!({"actor":"controller",
        "kind":"audit_generation_recovery","generation":lease.generation,
        "workflow":lease.workflow,"recovery":recovery}))?;
    *binding = store.recover_audit_generation(lease,generation,&recovery,now)?;
    reconcile_runtime_retirements_in(store,now)?;
    Ok(next)
}

/// Free, bounded reconciliation; durable intent survives terminal workflows and
/// crash between audit-generation commit and filesystem retirement.
pub(super) fn reconcile_runtime_retirements()->Result<usize> {reconcile_runtime_retirements_at(&path())}
fn reconcile_runtime_retirements_at(state:&Path)->Result<usize> {
    if !state.exists(){return Ok(0);}
    let mut store=WorkflowStore::open(state)?;
    let completed=reconcile_runtime_retirements_in(&mut store,millis())?;
    // Only this free worker entry creates the shared handle. Acquisition/finish
    // callers of the inner helper may already hold their workflow mutex.
    let shared=Arc::new(Mutex::new(store));
    let terminal=durable::native_terminal_cleanup::sweep(&shared,&millis,&mut |pending,namespace| {
        existing_native_audit(state,namespace,None,pending.original_binding())
    })?;
    Ok(completed+terminal)
}
fn reconcile_runtime_retirements_in(store:&mut WorkflowStore,now:i64)->Result<usize> {
    let rows=store.pending_native_retirements(now,16)?;
    let mut completed=0;
    for (workflow,generation,audit_path) in rows {
        let owner=augmentagent_channel_core::native_vm::RuntimeOwner{workflow_id:workflow.clone(),lease_generation:generation};
        let binding=match store.native_owner_binding(&workflow,generation) {
            Ok(Some(binding))=>binding,
            Ok(None)=>{warn!("native runtime retirement retained: original binding is missing");continue;},
            Err(error)=>{warn!("native runtime retirement retained: original binding is unreadable: {error:#}");continue;},
        };
        let current=|| {
            use augmentagent_channel_core::native_vm::ownership::TransferFenceError;
            match store.native_retirement_is_current(&workflow,generation,&audit_path,&binding) {
                Ok(true)=>Ok(()),Ok(false)=>Err(TransferFenceError::NotCurrent),Err(_)=>Err(TransferFenceError::Unavailable),
            }
        };
        match augmentagent_channel_core::native_vm::retire_owned_runtimes(&binding,Path::new(&audit_path),&owner,&current) {
            Ok(_)=>{store.complete_bound_native_retirement(&workflow,generation,&audit_path,&binding,now)?; completed+=1;}
            Err(error)=>warn!("native runtime retirement retained for retry: {error:#}"),
        }
    }
    store.settle_owner_execution_retries(now,16)?;
    Ok(completed)
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
        let mut binding = existing
            .as_ref()
            .map(|s| s.checkpoint.binding.clone())
            .unwrap_or(Binding {
                head: head.into(),
                contract: format!("{:x}", Sha256::digest(recorded.as_bytes())),
                evidence_hash: format!("{:x}", Sha256::digest(recorded.as_bytes())),
            });
        store.register(&id, branch, binding.clone(), now)?;
        let Some(lease) = store.claim_reconciliation(
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
        let reviews=store.current_review_history(&id)?;
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
        let reviewed_binding=binding.clone();
        let audit=acquire_audit_generation(path,&mut store,&lease,None,&mut binding,now)?;
        // Recovery changes evidence and clears prior review. Re-enter the normal
        // verification/review loop before this caller can authorize any merge.
        if binding!=reviewed_binding {
            store.release(&lease,now)?;
            return Ok(None);
        }
        audit.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
            workflow_id:lease.workflow.clone(),lease_generation:lease.generation,
        })?;
        let mut inner = WorkflowGuard {
            state:path.to_path_buf(),
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
        let setup=inner.register_native_context()
            .and_then(|_|inner.install_model_accounting())
            .and_then(|_|inner.validate_before_side_effect())
            .and_then(|_|inner.audit_event("sweep_claimed",serde_json::json!({})));
        if let Err(error)=setup {
            inner.release_unstarted_native_registration(false)?;
            return Err(error);
        }
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
        let worktree = prepare_resume_worktree_at_head(repo, pr, &self.branch,Some(&expected)).await?;
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
                inner,
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

    pub(super) fn merge_lease(&self) -> Result<Lease> {
        self.inner.as_ref().context("released sweep lease")?.merge_lease()
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

    /// Exercise the complete admission path against caller-owned disposable
    /// state without touching environment variables or the daemon database.
    #[cfg(test)]
    pub(super) async fn acquire_fixture(state: &Path, start: Start<'_>) -> Result<Option<Self>> {
        Self::acquire_at(state, start, millis()).await
    }

    pub(super) async fn acquire_owner_turn(start:Start<'_>)->Result<Option<Self>> {
        Self::acquire_mode_at(&path(),start,millis(),true).await
    }
    async fn acquire_at(state: &Path, start: Start<'_>, now: i64) -> Result<Option<Self>> {
        Self::acquire_mode_at(state,start,now,false).await
    }
    async fn acquire_mode_at(state:&Path,start:Start<'_>,now:i64,owner_turn:bool)->Result<Option<Self>> {
        if owner_turn {anyhow::ensure!(std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").is_some(),"owner-turn-native-configuration-unavailable");}
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
        let mut existing = store.snapshot(&id)?;
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
        if owner_turn {
            if existing.is_none() {
                if !store.register_owner_candidate(&id,start.repository,start.branch,binding.clone(),now)? {
                    return Ok(None);
                }
                existing=store.snapshot(&id)?;
            }
            anyhow::ensure!(existing.as_ref().is_some_and(|s|s.checkpoint.binding==binding),"owner turn requires the current owned candidate binding");
        } else {store.register(&id,start.branch,binding.clone(),now)?;}
        let owner=format!("{}:{}",std::process::id(),uuid::Uuid::new_v4());
        let lease=if owner_turn {store.claim_owner_turn_for_setup(&id,&owner,now,LEASE_MS)?.map(|turn|turn.lease)}
            else {store.claim_productive(&id,&owner,now,LEASE_MS)?};
        let Some(lease)=lease else {return Ok(None);};
        let prepared=(||->Result<Arc<augmentagent_channel_core::AuditLogger>> {
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
        let audit = acquire_audit_generation_inner(state, &mut store, &lease,
            Some(start.worktree), &mut binding, now)?;
        audit.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
            workflow_id: lease.workflow.clone(), lease_generation: lease.generation,
        })?;
            Ok(audit)
        })();
        let audit=match prepared {
            Ok(audit)=>audit,
            Err(error)=>{
                if owner_turn {store.release_unstarted_owner_turn(&UnstartedOwnerTurnSetup {lease:lease.clone()},millis())?;}
                else {store.release(&lease,millis())?;}
                return Err(error);
            }
        };
        let mut guard = Self {
            state:state.to_path_buf(),
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
        let setup=guard.register_native_context()
            .and_then(|_|guard.install_model_accounting())
            .and_then(|_|guard.validate_before_side_effect())
            .and_then(|_|guard.audit_event("workflow_claimed",serde_json::json!({})));
        if let Err(error)=setup {
            guard.release_unstarted_native_registration(owner_turn)?;
            return Err(error);
        }
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
            lease:self.lease.clone(), calls_before, resumed })
    }
    pub(super) fn audit_logger(&self) -> Arc<augmentagent_channel_core::AuditLogger> {
        self.audit.clone()
    }
    /// No native transport can escape acquisition before this registration.
    /// Missing selection leaves no unbound owner to strand later setup recovery.
    fn register_native_context(&self)->Result<()> {
        use augmentagent_channel_core::native_vm::ownership::{self,TransferFenceError,TransferReason};
        use durable::native_prepare::NativePreparationRun;
        use durable::native_reconcile::{self,Mode,ReconciliationRun};
        if std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").is_none() {return Ok(());}
        let owner=augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:self.id().into(),lease_generation:self.lease.generation};
        let current=|| {
            self.healthy_lease().map_err(|_|TransferFenceError::Unavailable)?;
            {
                let mut store=self.store.lock().map_err(|_|TransferFenceError::Unavailable)?;
                store.validate_dispatch(&self.lease,&self.binding,millis()).map_err(|_|TransferFenceError::NotCurrent)?;
                if let Some((path,_))=store.native_owner_context(self.id(),self.lease.generation).map_err(|_|TransferFenceError::Unavailable)? {
                    if Path::new(&path)!=self.audit.path() {return Err(TransferFenceError::NotCurrent);}
                }
            }
            self.audit.handoff_destination_ready(&owner).map_err(|_|TransferFenceError::Unavailable)
        };
        let (registered,available,pending)={
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("native registration store poisoned"))?;
            (store.native_owner_context(self.id(),self.lease.generation)?,store.initial_native_owner_available(self.id())?,store.native_pending_request(&self.lease,&self.binding,millis())?)
        };
        if registered.is_none() && pending.as_ref().is_none_or(|p|p.request().destination.owner.lease_generation!=self.lease.generation) {
            // This is a leased, provisional controller namespace, not promotion.
            self.audit.require_run_resources()?;
            self.audit.record_controller(serde_json::json!({"actor":"controller","kind":"native_destination_prepared","workflow":self.id(),"generation":self.lease.generation}))?;
        }
        let original=if let Some((path,original))=registered {
            anyhow::ensure!(Path::new(&path)==self.audit.path(),TransferReason::AuthorityConflict);original
        } else if available {
            ownership::with_initial_owner_binding(self.id(),&current,&mut |original| {
                self.store.lock().map_err(|_|anyhow::anyhow!("native registration store poisoned"))?
                    .register_initial_native_owner(&self.lease,&self.binding,self.audit.path().to_str().context("native audit is not UTF-8")?,original,millis())
            })?
        } else {
            let state=self.state.clone();
            let worktree=(!self.worktree.as_os_str().is_empty()).then_some(self.worktree.as_path());
            let mut completed=false;
            for _ in 0..3 {
                let pending=self.store.lock().map_err(|_|anyhow::anyhow!("native recovery store poisoned"))?.native_pending_request(&self.lease,&self.binding,millis())?;
                if let Some(pending)=pending {
                    let mode=Mode::Lease {lease:&self.lease,binding:&self.binding};
                    let mut result=native_reconcile::run(&self.store,mode,&pending,None,&millis)?;
                    if matches!(result,ReconciliationRun::Retained {state:Some(ownership::NativeTransferState::Prepared|ownership::NativeTransferState::Absent),..}) {
                        let sources=existing_native_sources(&state,&pending.request().source_owners,worktree,pending.original_binding())?;
                        let destination=existing_native_audit(&state,&pending.request().destination,worktree,pending.original_binding())?;
                        result=native_reconcile::run(&self.store,mode,&pending,Some((&sources,&destination)),&millis)?;
                    }
                    match result {
                        ReconciliationRun::Settled(settlement) if settlement.current_context=>{completed=true;break;}
                        ReconciliationRun::KnownUnstarted|ReconciliationRun::Historical|ReconciliationRun::Settled(_)=>continue,
                        ReconciliationRun::KnownAborted {..}=>anyhow::bail!(TransferReason::AuthorityConflict),
                        ReconciliationRun::Retained {reason,..}=>anyhow::bail!(reason),
                    }
                }
                let recovery=self.store.lock().map_err(|_|anyhow::anyhow!("native recovery census store poisoned"))?
                    .closed_native_recovery_facts(&self.lease,&self.binding,self.audit.path().to_str().context("native audit is not UTF-8")?,millis())?;
                if let Some(basis)=recovery {
                    self.audit.set_native_store_binding(basis.original.clone())?;
                    let audits=existing_native_sources(&state,&basis.plan.namespaces,worktree,&basis.original)?;
                    ownership::with_closed_workflow(&basis.original,&basis.plan,&audits,&self.audit,&current,&mut |proof| {
                        self.store.lock().map_err(|_|anyhow::anyhow!("native recovery registration store poisoned"))?
                            .register_closed_native_owner(&self.lease,&self.binding,&basis,proof,millis())
                    }).map_err(anyhow::Error::new)?;
                    completed=true;break;
                }
                let sources=self.store.lock().map_err(|_|anyhow::anyhow!("native facts store poisoned"))?.native_handoff_sources(&self.lease,&self.binding,millis());
                let sources=match sources {
                    Ok((authorities,original))=>{self.audit.set_native_store_binding(original.clone())?;existing_native_sources(&state,&authorities,worktree,&original)?}
                    Err(_)=>Vec::new(),
                };
                let result=durable::native_prepare::run(&self.store,&self.lease,&self.binding,&sources,&self.audit,&millis)?;
                match result {
                    NativePreparationRun::Pending {settlement:Some(settlement),settlement_failed:false,..} if settlement.current_context=>{completed=true;break;}
                    NativePreparationRun::Registered {..}=>{completed=true;break;}
                    NativePreparationRun::Reconciled {result:ReconciliationRun::KnownUnstarted|ReconciliationRun::Historical|ReconciliationRun::Settled(_),..}=>continue,
                    NativePreparationRun::NoPending {failure,..}=>anyhow::bail!(failure.reason()),
                    NativePreparationRun::PersistenceUnknown {reason,..}=>anyhow::bail!(reason),
                    _=>anyhow::bail!(TransferReason::PendingUnknown),
                }
            }
            anyhow::ensure!(completed,TransferReason::PendingUnknown);
            let (path,original)=self.store.lock().map_err(|_|anyhow::anyhow!("native destination store poisoned"))?.native_owner_context(self.id(),self.lease.generation)?.context(TransferReason::InventoryMissing)?;
            anyhow::ensure!(Path::new(&path)==self.audit.path(),TransferReason::AuthorityConflict);original
        };
        self.audit.set_native_store_binding(original.clone())?;
        ownership::validate_owner_binding(&original,&current)?;
        let pending=self.store.lock().map_err(|_|anyhow::anyhow!("native promotion store poisoned"))?.current_native_handoff(&self.lease,&self.binding,millis())?;
        if let Some(pending)=pending {
            let proof=pending.verified_receipt(&current).map_err(|_|anyhow::anyhow!(TransferReason::PendingUnknown))?.context(TransferReason::PendingUnknown)?;
            self.store.lock().map_err(|_|anyhow::anyhow!("native promotion store poisoned"))?.promote_native_handoff_audit(&self.lease,&self.binding,&proof,millis())?;
        }
        current().map_err(anyhow::Error::new)
    }

    /// Acquisition has not returned and no provider/observer can have executed.
    /// This narrowly releases a refused setup, never a running turn.
    fn release_unstarted_native_registration(&mut self,owner_turn:bool)->Result<()> {
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("native registration store poisoned"))?;
        let now=millis();charge_elapsed(&mut store,&self.lease,self.started,&self.accounted_ms,now)?;
        if owner_turn {store.release_unstarted_owner_turn(&UnstartedOwnerTurnSetup {lease:self.lease.clone()},now)?;}
        else {store.release(&self.lease,now)?;}
        self.released=true;Ok(())
    }

    fn install_model_accounting(&self) -> Result<()> {
        self.audit.require_run_resources()?;
        if self.audit.native_runtime_owner()?.is_some() {
            let original=self.store.lock().map_err(|_|anyhow::anyhow!("native original binding store poisoned"))?
                .native_owner_binding(&self.lease.workflow,self.lease.generation)?;
            if let Some(original)=original {self.audit.set_native_store_binding(original)?;}
            let workflow=self.lease.workflow.clone();
            durable::native_callbacks::install(&self.store,&self.lease,&self.audit,Arc::new(move || {
                if let Ok(call)=BOUNDED_NATIVE_CALL.try_with(Clone::clone) {
                    anyhow::ensure!(call.workflow==workflow,"native callback used another workflow scope");Ok(Some(call.binding))
                } else {Ok(None)}
            }))?;
        }
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
        self.install_model_observers(None)
    }
    /// Once a body is reserved, narrow both observers to its captured original
    /// authority. Keep that fence after success/failure and across thread hops;
    /// only a new guarded generation can install an ordinary observer again.
    fn install_model_observers(&self,body:Option<Arc<OwnerBodyAdmission>>) -> Result<()> {
        let dispatch_body=body.clone();
        let dispatch_store=self.store.clone();let dispatch_lease=self.lease.clone();
        let dispatch_failure=self.lease_failure.clone();
        self.audit.set_dispatch_observer(Arc::new(move || {
            let mut store=dispatch_store.lock().map_err(|_|anyhow::anyhow!("workflow dispatch lock poisoned"))?;
            anyhow::ensure!(dispatch_failure.lock().map_err(|_|anyhow::anyhow!("workflow failure lock poisoned"))?.is_none(),"workflow guard fenced; fresh guarded recovery required");
            let binding=store.snapshot(&dispatch_lease.workflow)?.context("workflow missing")?.checkpoint.binding;
            store.validate_dispatch(&dispatch_lease,&binding,millis())?;
            if let Some(body)=&dispatch_body {body.validate(&mut store,&dispatch_lease,millis())?;}
            Ok(())
        }))?;
        let capacity_flag = self.capacity_exhausted.clone();
        let model_failure=self.lease_failure.clone();
        let store = self.store.clone();
        let lease = self.lease.clone();
        let observational = self.repository.starts_with("autoship-observation/") || self.repository.starts_with("autoship-canary/");
        self.audit.set_model_call_observer(Arc::new(move |provider| {
            anyhow::ensure!(!capacity_flag.load(Ordering::SeqCst), "resource_capacity: await_capacity checkpoint retained");
            let mut store = store.lock().map_err(|_| anyhow::anyhow!("workflow accounting lock poisoned"))?;
            anyhow::ensure!(model_failure.lock().map_err(|_|anyhow::anyhow!("workflow failure lock poisoned"))?.is_none(),"workflow guard fenced; fresh guarded recovery required");
            anyhow::ensure!(!matches!(store.snapshot(&lease.workflow)?.context("workflow missing")?.checkpoint.stage,Stage::Merged|Stage::Retired),"terminal workflow cannot admit another model call");
            let receipt = format!("model-call:{}", uuid::Uuid::new_v4());
            let bounded=BOUNDED_NATIVE_CALL.try_with(Clone::clone).ok();
            if let Some(bounded)=&bounded {
                anyhow::ensure!(bounded.workflow==lease.workflow,"bounded native call used a different workflow audit sink");
            }
            let actual=match provider {"codex"=>Some(durable::submission::Provider::Codex),"claude"=>Some(durable::submission::Provider::Claude),_=>None};
            if let Some(body)=&body {
                anyhow::ensure!(!augmentagent_channel_core::native_session::is_fresh_review(),"owner coding cannot use reviewer attribution");
                let actual=actual.context("owner coding requires its native provider")?;
                anyhow::ensure!(actual==body.provider,"owner coding provider changed");
                let preparation=body.preparation.activation_binding()?;
                return store.charge_prepared_owner_model(&lease,&receipt,millis(),
                    bounded.as_ref().map(|b|(b.purpose.as_str(),&b.binding,b.limit)),actual,&body.dispatch,&preparation);
            }
            store.charge_admitted_model(&lease,&receipt,millis(),observational.then(observation_call_cap),
                bounded.as_ref().map(|b|(b.purpose.as_str(),&b.binding,b.limit)),actual,
                augmentagent_channel_core::native_session::is_fresh_review())
        }))
    }
    /// Builder identity is checked independently of audit binding so reviewer
    /// and diagnostic reasoners are never forced onto the builder's provider.
    pub(super) fn bind_builder(&self, reasoner: &FallbackReasoner) -> Result<()> {
        use crate::autoship_workflow::submission::Provider;
        let actual = match reasoner.builder_provider_pin() {
            Some(augmentagent_channel_core::providers::ProviderKind::Claude) => Some(Provider::Claude),
            Some(augmentagent_channel_core::providers::ProviderKind::Codex) => Some(Provider::Codex),
            _ => None,
        };
        self.store.lock().map_err(|_| anyhow::anyhow!("workflow store poisoned"))?
            .require_command_builder(self.id(), actual)?;
        self.bind_reasoner(reasoner)
    }

    pub(super) fn bind_reasoner(&self, reasoner: &FallbackReasoner) -> Result<()> {
        // The guard owns one accounting observer. Binding a reasoner reuses
        // it; the owner-body boundary narrows it without adding a second charge.
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
        let mut guard = Self { state:state.clone(),audit,store:Arc::new(Mutex::new(store)),lease,repository:id.clone(),worktree:PathBuf::new(),contract:"observation-v1".into(),environment:"diagnostic".into(),binding,
            heartbeat:tokio_util::sync::CancellationToken::new(),lease_failure:Arc::new(Mutex::new(None)),started:std::time::Instant::now(),accounted_ms:Arc::new(AtomicU64::new(0)),capacity_exhausted:Arc::new(AtomicBool::new(false)),released:false };
        let setup=guard.register_native_context()
            .and_then(|_|guard.install_model_accounting())
            .and_then(|_|guard.validate_before_side_effect())
            .and_then(|_|guard.audit_event("observation_claimed",serde_json::json!({"key":key})));
        if let Err(error)=setup {
            guard.release_unstarted_native_registration(false)?;
            return Err(error);
        }
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

    fn native_builder_owner(&self) -> Result<augmentagent_channel_core::native_vm::RuntimeOwner> {
        self.validate_before_side_effect()?;
        self.audit.validate_native_store_binding()?;
        let owner=self.audit.native_runtime_owner()?.context("workflow native owner missing")?;
        anyhow::ensure!(owner.workflow_id==self.lease.workflow && owner.lease_generation==self.lease.generation,"workflow native owner changed");
        Ok(owner)
    }

    pub(super) fn fresh_native_builder_sessions(&self) -> Result<Arc<augmentagent_channel_core::native_session::FreshBuilderSessions>> {
        augmentagent_channel_core::native_session::FreshBuilderSessions::for_workflow(self.native_builder_owner()?)
    }

    /// Owner follow-ups continue the actual observed builder conversation.
    /// A new episode deliberately uses the separate fresh factory above.
    /// Neither scope is native dispatch authority: transport preparation and
    /// the installed admission callback still validate the original store,
    /// current generation, owner intent and requested provider on every call.
    pub(super) fn owner_turn_native_builder_sessions(
        &self,
        provider: augmentagent_channel_core::ProviderKind,
    ) -> Result<Arc<augmentagent_channel_core::native_session::FreshBuilderSessions>> {
        let owner=self.native_builder_owner()?;
        let observed={
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store poisoned"))?;
            let now=millis();
            let turn=store.owner_turn_for_lease(&self.lease,now)?;
            store.owner_turn_conversations(&turn,provider,now)?
        };
        augmentagent_channel_core::native_session::FreshBuilderSessions::from_observed(owner,&observed)
    }

    /// Authenticated private input for the independent acceptance producer.
    /// Reading it grants neither provider dispatch nor contract activation.
    pub(super) fn owner_acceptance_turn(&self,candidate:&Path,source:crate::autoship_contract::AcceptanceSource,
        previous:Option<&crate::autoship_contract::RegisteredContract>)->Result<durable::owner_turn::OwnerTurn> {
        self.native_builder_owner()?;
        anyhow::ensure!(candidate.canonicalize()?==self.worktree.canonicalize()?,"owner acceptance candidate changed");
        let expected=match source {crate::autoship_contract::AcceptanceSource::Issue(n)=>durable::workflow_id(&self.repository,n)?,
            crate::autoship_contract::AcceptanceSource::PullRequest(n)=>durable::pr_workflow_id(&self.repository,n)?};
        anyhow::ensure!(expected==self.id(),"owner acceptance source changed");
        if let Some(previous)=previous {
            previous.verify_metadata()?;
            anyhow::ensure!(self.contract==format!("registered:{}",previous.contract_hash()) && self.binding.contract==previous.contract_hash(),"owner acceptance previous contract changed");
        }else {
            anyhow::ensure!(!self.contract.starts_with("registered:") && self.binding.contract==format!("{:x}",Sha256::digest(self.contract.as_bytes())),"initial owner acceptance requires unregistered intake");
        }
        self.store.lock().map_err(|_|anyhow::anyhow!("owner acceptance store poisoned"))?.owner_turn_for_lease(&self.lease,millis())
    }
    pub(super) fn validate_original_owner_preparation(&self,turn:&durable::owner_turn::OwnerTurn,preparation:&durable::OwnerPreparationBinding)->Result<()> {
        anyhow::ensure!(turn.lease.workflow==self.lease.workflow && turn.lease.owner==self.lease.owner
            && turn.lease.generation==self.lease.generation,"owner preparation workflow changed");
        self.store.lock().map_err(|_|anyhow::anyhow!("owner preparation store poisoned"))?
            .validate_owner_preparation_for_turn(turn,preparation,millis())
    }

    /// Preparation is neither installation nor activation. The current request
    /// and its independently reproduced private baseline are rechecked before
    /// consuming the old guard. Adapter files are never installed in product.
    pub(super) async fn activate_prepared_owner_acceptance(self,prepared:&acceptance::owner::PreparedOwnerAcceptance,
        previous:Option<&crate::autoship_contract::RegisteredContract>)->Result<Self> {
        prepared.validate_current(&self,&self.worktree,previous)?;
        let owner_prepared=prepared;
        let prepared=prepared.prepared();
        acceptance::validate_candidate_harness(&self.worktree,prepared).await?;
        self.audit_health().await?;self.native_builder_owner()?;
        let next=&prepared.baseline.registration;let old=self.binding.contract.clone();let contract=format!("registered:{}",next.contract_hash());
        let environment=next.binding(&self.binding.head)?.environment_id;
        let binding=git_binding(&self.worktree,&contract,&environment,&[]).await?;
        // Recheck after all asynchronous candidate inspection, then again
        // across native settlement before changing its registered scope.
        owner_prepared.validate_current(&self,&self.worktree,previous)?;
        let preparation=owner_prepared.activation_binding()?;
        let initial=previous.is_none().then(||InitialOwnerAcceptance {lease:self.lease.clone(),binding:self.binding.clone()});
        let revised={
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("private owner acceptance store poisoned"))?;
            let now=millis();let turn=store.owner_turn_for_lease(&self.lease,now)?;
            charge_elapsed(&mut store,&self.lease,self.started,&self.accounted_ms,now)?;
            store.activate_private_owner_acceptance(initial.as_ref(),&turn,previous,next,&prepared.baseline.receipts,&prepared.proposal.files,&preparation,&binding,now)?
        };
        let guard=self.finish_owner_contract_change(revised,contract,environment,"owner_private_acceptance_guard_ready",&old,next.contract_hash())?;
        anyhow::ensure!(owner_prepared.activation_binding()?==preparation,"owner preparation changed during native settlement");
        acceptance::bind_write_scope(&guard.worktree,prepared,previous)?;
        anyhow::ensure!(owner_prepared.activation_binding()?==preparation,"owner preparation changed during scope registration");
        Ok(guard)
    }

    /// The independently prepared registration changes the owner turn's
    /// generation. Consume this guard so its old audit/admission callbacks can
    /// never be reused for the new contract. Native ownership must transfer
    /// through the same original-store service before a new guard escapes.
    /// This does not apply the input, unpause, approve acceptance or ship.
    #[cfg(test)]
    pub(super) async fn activate_owner_revision(
        self,
        previous: &crate::autoship_contract::RegisteredContract,
        next: &crate::autoship_contract::RegisteredContract,
    ) -> Result<Self> {
        anyhow::ensure!(std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").is_some(),"owner-turn-native-configuration-unavailable");
        self.audit_health().await?;
        self.native_builder_owner()?;
        let contract=format!("registered:{}",next.contract_hash());
        let environment=next.binding(&self.binding.head)?.environment_id;
        let binding=git_binding(&self.worktree,&contract,&environment,&[]).await?;
        let revised={
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("owner revision store poisoned"))?;
            let now=millis();
            let turn=store.owner_turn_for_lease(&self.lease,now)?;
            charge_elapsed(&mut store,&self.lease,self.started,&self.accounted_ms,now)?;
            store.activate_owner_revision(&turn,previous,next,&binding,&self.worktree,now)?
        };
        self.finish_owner_contract_change(revised,contract,environment,"owner_revision_guard_ready",previous.contract_hash(),next.contract_hash())
    }

    #[cfg(test)]
    pub(super) async fn activate_initial_owner_acceptance(self,prepared:&acceptance::PreparedAcceptance)->Result<Self> {
        anyhow::ensure!(std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").is_some(),"owner-turn-native-configuration-unavailable");
        self.audit_health().await?;self.native_builder_owner()?;
        anyhow::ensure!(!self.contract.starts_with("registered:")
            && self.binding.contract==format!("{:x}",Sha256::digest(self.contract.as_bytes())),
            "initial owner acceptance requires the original unregistered intake");
        let next=&prepared.baseline.registration;
        anyhow::ensure!(prepared.proposal.contract==*next.contract(),"initial owner proposal differs from registration");
        let old=self.binding.contract.clone();let contract=format!("registered:{}",next.contract_hash());
        let environment=next.binding(&self.binding.head)?.environment_id;
        let binding=git_binding(&self.worktree,&contract,&environment,&[]).await?;
        let proof=InitialOwnerAcceptance {lease:self.lease.clone(),binding:self.binding.clone()};
        let revised={
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("initial owner acceptance store poisoned"))?;
            let now=millis();let turn=store.owner_turn_for_lease(&self.lease,now)?;
            charge_elapsed(&mut store,&self.lease,self.started,&self.accounted_ms,now)?;
            store.activate_initial_owner_acceptance(&proof,&turn,next,&prepared.baseline.receipts,&binding,&self.worktree,now)?
        };
        self.finish_owner_contract_change(revised,contract,environment,"owner_initial_acceptance_guard_ready",&old,next.contract_hash())
    }

    fn finish_owner_contract_change(mut self,revised:durable::owner_turn::OwnerTurn,
        contract:String,environment:String,kind:&str,previous_contract:&str,next_contract:&str)->Result<Self> {
        self.heartbeat.cancel();
        self.lease=revised.lease;
        self.binding=revised.binding;
        self.contract=contract;
        self.environment=environment;
        self.heartbeat=tokio_util::sync::CancellationToken::new();
        self.lease_failure=Arc::new(Mutex::new(None));
        self.started=std::time::Instant::now();
        self.accounted_ms=Arc::new(AtomicU64::new(0));
        self.capacity_exhausted=Arc::new(AtomicBool::new(false));
        // Use a controller-local connection for native/audit I/O. No shared
        // workflow mutex may span the state/session/audit lock acquisition.
        let mut store=WorkflowStore::open(&self.state)?;
        self.audit=acquire_audit_generation_inner(&self.state,&mut store,&self.lease,
            Some(&self.worktree),&mut self.binding,millis())?;
        self.audit.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
            workflow_id:self.id().into(),lease_generation:self.lease.generation,
        })?;
        self.register_native_context()?;
        self.install_model_accounting()?;
        self.native_builder_owner()?;
        self.audit_event(kind,serde_json::json!({
            "request_id":revised.request_id,"previous_contract":previous_contract,
            "next_contract":next_contract,
        }))?;
        // Unlike initial constructor refusal, this turn has already escaped a
        // guard and may have performed work. Failed setup stays claimed; it
        // must never be relabelled as an unstarted, automatically replayable
        // request. Drop charges elapsed usage and stops renewal without release.
        self.spawn_renewal();
        Ok(self)
    }

    /// The caller supplies a builder with its original authorship history bound,
    /// as in the ordinary worker lane; imported unknown history stays unknown.
    /// Execute the authenticated pending body exactly once, inside the real
    /// guarded native builder scope. This is not input completion or permission
    /// to publish: the returned private result still needs locked completion
    /// and an atomic per-request reply/owner-turn transaction.
    pub(super) async fn run_owner_body(&self,reasoner:&FallbackReasoner,owner_prepared:&Arc<acceptance::owner::PreparedOwnerAcceptance>)->Result<OwnerBodyResult> {
        owner_prepared.validate_activated(self,&self.worktree)?;
        let prepared=owner_prepared.prepared();
        self.native_builder_owner()?;
        anyhow::ensure!(self.contract==format!("registered:{}",self.binding.contract),
            "owner body requires independently registered acceptance");
        let provider=reasoner.builder_provider_pin().context("owner body requires pinned native builder")?;
        let selected=match provider {augmentagent_channel_core::ProviderKind::Codex=>durable::submission::Provider::Codex,
            augmentagent_channel_core::ProviderKind::Claude=>durable::submission::Provider::Claude,
            _=>anyhow::bail!("owner body requires native Codex or Claude")};
        self.bind_builder(reasoner)?;
        let sessions=self.owner_turn_native_builder_sessions(provider)?;
        let expected_turn=self.store.lock().map_err(|_|anyhow::anyhow!("owner prompt store poisoned"))?.owner_turn_for_lease(&self.lease,millis())?;
        // Reject malformed or oversized context before the once-only body
        // reservation. No provider submission has begun at this point.
        let prompt=owner_body_prompt(prepared,&self.binding.contract,&expected_turn.body)?;
        let origin=adoption::origin_for_candidate(&self.worktree,Some(self)).await?;
        let authors=reasoner.review_authors()?;
        let first=!origin.is_imported() && authors.as_ref().is_some_and(|authors|authors.is_empty());
        anyhow::ensure!(origin.is_imported() || authors.as_ref().is_some_and(|authors|!authors.is_empty()) || first,"owner body requires known builder provenance or authorized imported history");
        // Revalidate after asynchronous provenance inspection and before the
        // once-only reservation, so a known invalid artifact cannot strand it.
        let preparation=owner_prepared.validate_activated(self,&self.worktree)?;
        let (turn,dispatch)=if first {
            anyhow::ensure!(sessions.observed().is_empty(),"first builder cannot replace an observed conversation");
            let main=remote_main_head(&self.worktree,&self.repository).await?;
            let dispatch=self.reserve_prepared_first_owner_body(owner_prepared,selected,&main).await?;
            let turn=self.store.lock().map_err(|_|anyhow::anyhow!("owner dispatch store poisoned"))?.owner_turn_for_lease(&self.lease,millis())?;
            (turn,dispatch)
        }else {
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("owner dispatch store poisoned"))?;
            let now=millis();let turn=store.owner_turn_for_lease(&self.lease,now)?;
            let dispatch=store.begin_prepared_owner_body_dispatch(&turn,selected,&preparation,now)?;
            (turn,dispatch)
        };
        let mut opts=fix_opts(self.worktree.clone());opts.audit_logger=Some(self.audit.clone());
        anyhow::ensure!(turn.request_id==expected_turn.request_id && turn.body==expected_turn.body && turn.binding==expected_turn.binding,"owner prompt request changed before dispatch");
        let dispatch=Arc::new(dispatch);
        let admission=Arc::new(OwnerBodyAdmission {dispatch:dispatch.clone(),preparation:owner_prepared.clone(),provider:selected});
        self.install_model_observers(Some(admission))?;
        let outcome=sessions.scope(async {
            if first {reasoner.call_with_storage(&opts,&prompt).await}
            else {adoption::revise(reasoner,self,&opts,&prompt).await}
        }).await;
        self.audit_health().await?;
        self.record_episode_sessions(&sessions)?;
        let response=outcome?;
        anyhow::ensure!(response.len()<=1024*1024,"owner body response exceeds private result bound");
        let observed=sessions.observed();
        anyhow::ensure!(observed.len()==1 && observed[0].provider==provider && !observed[0].uncertain,
            "owner body has no unique certain native conversation");
        let result=OwnerBodyResult {dispatch,response,observed:observed.into_iter().next().unwrap()};
        // The first fresh check catches stale observations before returning a
        // result. Completion must repeat this check in its atomic visitor.
        self.with_owner_body_completion(&result,&mut |_|Ok(()))?;
        Ok(result)
    }
    /// A first builder may start only on a real linked candidate still equal
    /// to its original main baseline, using protected private verifier bytes.
    /// Actual native absence is checked under original-store locks at reservation.
    async fn reserve_prepared_first_owner_body(&self,owner_prepared:&acceptance::owner::PreparedOwnerAcceptance,provider:durable::submission::Provider,canonical_main:&str)->Result<durable::owner_dispatch::OwnerBodyDispatch> {
        let preparation=owner_prepared.validate_activated(self,&self.worktree)?;
        let prepared=owner_prepared.prepared();
        self.native_builder_owner()?;
        let registration=&prepared.baseline.registration;
        anyhow::ensure!(self.contract==format!("registered:{}",registration.contract_hash())
            && prepared.proposal.contract==*registration.contract(),"first builder acceptance changed");
        prepared.verify_registration()?;
        acceptance::validate_candidate_harness(&self.worktree,prepared).await?;
        let base=registration.baseline_head();
        anyhow::ensure!(matches!(canonical_main.len(),40|64) && canonical_main.bytes().all(|b|b.is_ascii_hexdigit()),"first builder canonical main identity missing");
        let primary=workspace::primary_root(&self.worktree).await?;
        recovery::existing_checkout(&primary,&self.worktree).await?;
        anyhow::ensure!(self.worktree.canonicalize()?.starts_with(workspace::candidate_parent(&primary).await?),"first builder requires an owned candidate checkout");
        let actual=first_builder_git(&self.worktree,&["rev-parse","HEAD"]).await?;
        anyhow::ensure!(actual.trim()==base,"first builder candidate is not the original baseline");
        first_builder_git(&self.worktree,&["merge-base","--is-ancestor",base,canonical_main]).await?;
        for args in [vec!["diff","--cached","--no-ext-diff","--no-textconv","--name-only","-z",base,"--"],vec!["diff","--no-ext-diff","--no-textconv","--name-only","-z",base,"--"]] {
            anyhow::ensure!(first_builder_git(&self.worktree,&args).await?.is_empty(),"first builder candidate has staged or tracked owner changes");
        }
        let untracked=first_builder_git(&self.worktree,&["ls-files","--others","-z","--"]).await?;
        anyhow::ensure!(untracked.len()<=512*1024,"first builder untracked inventory exceeds bound");
        anyhow::ensure!(untracked.is_empty(),"first builder has untracked or ignored owner data");
        let current=||->Result<()> {self.healthy_lease()?;self.store.lock().map_err(|_|anyhow::anyhow!("first builder store poisoned"))?.first_owner_body_turn(&self.lease,provider,base,millis())?;Ok(())};
        let mut dispatch=None;
        augmentagent_channel_core::native_vm::ownership::with_unused_native_owner(&self.audit,&current,&mut |proof| {
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("first builder store poisoned"))?;
            let turn=store.first_owner_body_turn(&self.lease,provider,base,millis())?;
            dispatch=Some(store.begin_prepared_first_owner_body(&turn,provider,base,canonical_main,proof,&preparation,millis())?);Ok(())
        })?;
        dispatch.context("first builder reservation missing")
    }
    /// Completion applies only proven input. Explicit holds remain independent;
    /// normal verification/review/CI/publication still gate any shipping.
    pub(super) async fn finish_owner_body(mut self,result:OwnerBodyResult)->Result<()> {
        self.audit_health().await?;
        let binding=git_binding(&self.worktree,&self.contract,&self.environment,&[]).await?;
        self.with_owner_body_completion(&result,&mut |proof| {
            self.store.lock().map_err(|_|anyhow::anyhow!("owner completion store poisoned"))?
                .complete_owner_body(&result.dispatch,proof,&binding,&result.response,millis())
        })?;
        self.binding=binding;
        self.finish()
    }
    pub(super) fn with_owner_body_completion(&self,result:&OwnerBodyResult,
        visitor:&mut dyn FnMut(&augmentagent_channel_core::native_vm::ownership::NativeCompletionProof<'_>)->Result<()>,
    )->Result<()> {
        self.healthy_lease()?;
        let (prepared,reference)={
            self.store.lock().map_err(|_|anyhow::anyhow!("owner completion store poisoned"))?
                .owner_body_completion_reference(&result.dispatch,&result.observed,millis())?
        };
        let current=||->Result<()> {
            self.healthy_lease()?;
            // Metadata only. Full audit health would invert the retained
            // native/session/audit -> short workflow metadata lock order.
            self.store.lock().map_err(|_|anyhow::anyhow!("owner completion store poisoned"))?
                .validate_owner_body_dispatch(&result.dispatch,millis())
        };
        augmentagent_channel_core::native_vm::ownership::with_current_native_completion(
            &self.audit,&prepared,&reference,&current,visitor)
    }

    pub(super) fn record_episode_sessions(
        &self,
        sessions: &augmentagent_channel_core::native_session::FreshBuilderSessions,
    ) -> Result<()> {
        self.validate_before_side_effect()?;
        let observed=sessions.observed();
        let observations: Vec<_> = observed.iter().map(|s|serde_json::json!({"provider":s.provider.name(),"native_session":s.id,"uncertain":s.uncertain})).collect();
        self.audit_event(
            "episode_native_conversations",
            serde_json::json!({"observed":observations}),
        )?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store poisoned"))?
            .record_builder_conversations(&self.lease,&self.binding,&observed,millis())
    }

    pub(super) fn episode_review_at_head(
        &self,
        head: &str,
    ) -> Result<Option<crate::autoship_review::ReviewEvidence>> {
        let store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?;
        for review in store.current_review_history(self.id())? {
            if review.binding.head==head && review.reviewer.starts_with("episode-evidence-") {
                return Ok(Some(serde_json::from_str(&review.evidence)?));
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
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("acceptance store poisoned"))?
            .require_owner_dispatch(self.id()).context("owner acceptance requires a guarded contract transition")?;
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
            anyhow::bail!("workflow guard fenced: {error}");
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

    /// Bounded synchronous acceptance-store I/O, serialized with owner controls
    /// and workflow claims. No native/audit I/O or subprocess enters the visitor.
    pub(super) fn with_acceptance_metadata<T>(&self,
        operation:impl FnOnce(&durable::AcceptanceMetadata<'_>)->Result<T>)->Result<T> {
        self.with_acceptance_metadata_at(&millis,operation)
    }
    fn with_acceptance_metadata_at<T>(&self,clock:&dyn Fn()->i64,
        operation:impl FnOnce(&durable::AcceptanceMetadata<'_>)->Result<T>)->Result<T> {
        self.visit_acceptance_metadata_at(clock,true,operation)
    }
    /// Inspection gets no writer token and cannot claim a refused read changed files.
    pub(super) fn read_acceptance_metadata<T>(&self,operation:impl FnOnce()->Result<T>)->Result<T> {
        self.visit_acceptance_metadata_at(&millis,false,|_access|operation())
    }
    fn visit_acceptance_metadata_at<T>(&self,clock:&dyn Fn()->i64,mutating:bool,
        operation:impl FnOnce(&durable::AcceptanceMetadata<'_>)->Result<T>)->Result<T> {
        self.healthy_lease()?;
        self.audit.protected_health()?;
        let entered=std::cell::Cell::new(false);
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("acceptance metadata store poisoned"))?;
        let outcome=store.with_acceptance_metadata(&self.lease,&self.binding,clock,|access| {
            self.healthy_lease()?;
            entered.set(true);operation(access)
        });
        // Publish the local failure fence before releasing the shared store.
        // A model/dispatch callback already queued there must see it too.
        if outcome.is_err() && mutating && entered.get() {
            let mut failure=self.lease_failure.lock().map_err(|_|anyhow::anyhow!("workflow failure lock poisoned"))?;
            if failure.is_none() {*failure=Some("acceptance metadata outcome uncertain; retain durable files for fresh guarded recovery".into());}
        }
        outcome
    }

    /// Check immediately before gh/git side effects. Also pass GitHub the
    /// exact expected SHA; this local check cannot fence an in-flight RPC.
    pub(super) fn validate_before_side_effect(&self) -> Result<()> {
        self.healthy_lease()?;
        self.audit.protected_health()?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
        self.healthy_lease()?;
        store.validate_dispatch(&self.lease, &self.binding, millis())
    }

    pub(super) fn intent(&self, kind: OperationKind) -> Result<Operation> {
        self.validate_before_side_effect()?;
        let operation = {
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
            self.healthy_lease()?;
            store.begin_operation(&self.lease, kind, &self.binding, millis())?
        };
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

    pub(super) fn merge_lease(&self) -> Result<Lease> {
        self.validate_before_side_effect()?;
        Ok(self.lease.clone())
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

    /// Complete the caller's PR milestone only after the validated revision
    /// and independent approval have both been published under this binding.
    pub(super) async fn published_pr_ready(&mut self, pr: u64, review: &IndependentReview) -> Result<bool> {
        anyhow::ensure!(review.approved(), "PR milestone requires independent approval");
        let structured=review.structured.as_ref().context("PR milestone requires structured review evidence")?;
        anyhow::ensure!(structured.binding.head==self.binding.head && structured.binding.contract_hash==self.binding.contract,
            "PR milestone review differs from current candidate or contract");
        let verdict=review.recorded_verdict().context("PR milestone review evidence missing")?;
        let encoded=serde_json::to_string(&verdict)?;
        self.record_review(&format!("pr-ready-{}",uuid::Uuid::new_v4()),true,&encoded)?;
        let pr_only={
            let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?;
            store.record_pr_ready(&self.lease,&self.binding,pr,&encoded,millis())?;
            store.command_request(&self.lease.workflow)?.is_some_and(|r|r.until==durable::submission::Milestone::Pr)
        };
        if pr_only {
            self.transition(Stage::AwaitChecks,"pr_milestone_reached","await_merge_authorization",0).await?;
        }
        Ok(pr_only)
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

    pub(super) fn defer_observation_at_call_limit(&self,limit:u64,reset_at_ms:i64)->Result<bool> {
        anyhow::ensure!(self.repository.starts_with("autoship-observation/"),"observation checkpoint requires diagnostic namespace");
        self.validate_before_side_effect()?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?;
        let snapshot=store.snapshot(self.id())?.context("observation workflow missing")?;
        if snapshot.model_calls<limit {return Ok(false);}
        let mut checkpoint=snapshot.checkpoint;
        checkpoint.next=NextAction{reason:"calibration_admitted_call_budget".into(),action:"resume_calibration_after_budget_reset".into(),eligible_at_ms:reset_at_ms};
        store.checkpoint(&self.lease,&checkpoint,millis())?;
        Ok(true)
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
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
            .validate_dispatch(&self.lease,expected,millis())?;
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

    pub(super) async fn verification_gate_passed(&mut self,domain:durable::VerificationDomain)->Result<()> {
        if self.audit.resource_capacity_seen() {return Ok(());}
        self.audit_health().await?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow retry store poisoned"))?
            .record_verification_gate_passed(&self.lease,&self.binding,domain,millis())?;
        self.audit_event("verification_gate_passed",serde_json::json!({"domain":domain}))?;
        Ok(())
    }

    pub(super) async fn defer_verification_infrastructure(&mut self,error:&anyhow::Error)->Result<Option<i64>> {
        let message=format!("{error:#}");
        let Some(code)=infra_failure_reason(&message) else {return Ok(None);};
        self.validate_before_side_effect()?;
        self.audit_health().await?;
        let dependency=format!("{:x}",Sha256::digest(self.environment.as_bytes()));
        let eligible=self.store.lock().map_err(|_|anyhow::anyhow!("workflow retry store poisoned"))?
            .defer_verification_infrastructure(&self.lease,&self.binding,code,&dependency,millis())?;
        self.audit_event("verification_infrastructure_retry",serde_json::json!({
            "failure_code":code,"dependency_sha256":dependency,"eligible_at_ms":eligible,
            "failure_sha256":format!("{:x}",Sha256::digest(message.as_bytes()))}))?;
        Ok(Some(eligible))
    }

    /// Transition stage while retaining the receipt set that reviewers saw.
    pub(super) async fn transition(
        &mut self,
        stage: Stage,
        reason: &str,
        action: &str,
        eligible_at_ms: i64,
    ) -> Result<()> {
        // Completion and waiting receipts remain recordable after a pause;
        // entering another productive stage does not.
        if !matches!(stage,Stage::Merged|Stage::Retired|Stage::Held|Stage::AwaitChecks) {
            self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
                .require_owner_dispatch(self.id())?;
        }
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
        if !self.released { self.release_after_teardown()?; }
        Ok(())
    }

    pub(super) fn finish_ci_deferral(&mut self) -> Result<()> {
        let checkpoint = self.store.lock().map_err(|_|anyhow::anyhow!("workflow retry store poisoned"))?
            .snapshot(self.id())?.context("retry workflow missing")?.checkpoint;
        anyhow::ensure!(checkpoint.next.reason == "verification_infrastructure_retry"
            && checkpoint.next.action == "retry_verification", "no durable CI deferral to release");
        self.release_after_teardown()
    }

    fn release_after_teardown(&mut self) -> Result<()> {
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
        reconcile_runtime_retirements_in(&mut store,now)?;
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
// Preserve NUL-separated filenames exactly; trimming or lossy UTF-8 decoding
// could make an owner file look like a registered verifier path.
async fn first_builder_git(root:&Path,args:&[&str])->Result<String> {
    let result=Command::new("/usr/bin/git").args(["--no-replace-objects","-c","core.hooksPath=/dev/null","-c","core.fsmonitor=false"])
        .args(args).current_dir(root).env_clear().env("PATH","/usr/bin:/bin").env("GIT_OPTIONAL_LOCKS","0").env("GIT_GRAFT_FILE","/dev/null")
        .stdin(Stdio::null()).kill_on_drop(true).output().await?;
    anyhow::ensure!(result.status.success(),"first builder Git source check failed");Ok(String::from_utf8(result.stdout)?)
}

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
    prepare_resume_worktree_at_head(repo_root,pr,branch,None).await
}

pub(super) async fn prepare_resume_worktree_at_head(
    repo_root:&Path, pr:u64, branch:&str, expected_head:Option<&str>,
)->Result<PathBuf> {
    anyhow::ensure!(pr > 0, "PR identity required");
    let primary = workspace::primary_root(repo_root).await?;
    let path = primary.join(".self-improve-worktrees").join(format!("resume-pr-{pr}"));
    recovery::checkout_path(&primary, &path)?;
    let exists = path.try_exists()?;
    if exists {
        recovery::existing_checkout(repo_root, &path).await?;
    }
    let (valid, _, _) = run("git", &["check-ref-format", "--branch", branch], repo_root).await?;
    anyhow::ensure!(valid, "invalid resumed PR branch");
    let (ok, _, _) = run("git", &["fetch", "origin", branch], repo_root).await?;
    anyhow::ensure!(ok, "cannot refresh resumed PR branch");
    let fetched=git_evidence(repo_root,&["rev-parse","--verify",&format!("origin/{branch}^{{commit}}")]).await?;
    if let Some(expected)=expected_head {
        anyhow::ensure!(matches!(expected.len(),40|64) && expected.bytes().all(|b|b.is_ascii_hexdigit())
            && fetched.eq_ignore_ascii_case(expected),"resolved PR head changed before execution; candidate preserved");
        if exists {
            let actual = git_evidence(&path, &["rev-parse", "HEAD"]).await?;
            if !actual.eq_ignore_ascii_case(expected) {
                return Err(RetainedHeadMismatch { path, actual, published: fetched }.into());
            }
        }
    }
    if exists {
        return Ok(path);
    }
    let (ok, _, error) = run(
        "git",
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            &fetched,
        ],
        repo_root,
    )
    .await?;
    anyhow::ensure!(ok, "cannot create resumed PR worktree: {error}");
    Ok(path)
}

/// Prepare retained execution for fresh verification. Merge admission continues
/// to use the strict exact-head helper above.
pub(super) async fn prepare_resume_candidate(
    repo_root: &Path, repository: &str, pr: u64, branch: &str, expected_head: Option<&str>,
) -> Result<PathBuf> {
    prepare_resume_candidate_at(&path(), repo_root, repository, pr, branch, expected_head).await
}

async fn prepare_resume_candidate_at(
    state: &Path, repo_root: &Path, repository: &str, pr: u64, branch: &str, expected_head: Option<&str>,
) -> Result<PathBuf> {
    let error = match prepare_resume_worktree_at_head(repo_root, pr, branch, expected_head).await {
        Ok(path) => return Ok(path),
        Err(error) => error,
    };
    let Some(mismatch) = error.downcast_ref::<RetainedHeadMismatch>() else { return Err(error); };
    let Some(store) = WorkflowStore::open_read_only(state)? else { return Err(error); };
    let Some(recorded) = store.retained_candidate_head(repository, pr, branch,
        &mismatch.published, &mismatch.actual, millis())? else { return Err(error); };
    let (descendant, _, _) = run("git", &["merge-base", "--is-ancestor", &mismatch.published, &recorded], &mismatch.path).await?;
    anyhow::ensure!(descendant && git_evidence(&mismatch.path, &["rev-parse", "HEAD"]).await? == recorded,
        "retained candidate does not extend its recorded publication; candidate preserved");
    Ok(mismatch.path.clone())
}

#[derive(Debug)]
struct RetainedHeadMismatch { path: PathBuf, actual: String, published: String }
impl std::fmt::Display for RetainedHeadMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("resumed candidate differs from authorized PR head; candidate preserved")
    }
}
impl std::error::Error for RetainedHeadMismatch {}

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
    async fn retained_recorded_candidate_resumes_after_unpublished_integration() {
        let dir = repo();
        let state = control_dir();
        let database = state.path().join("workflow.db");
        let remote = state.path().join("remote.git");
        assert!(run("git", &["init", "--bare", "-q", remote.to_str().unwrap()], dir.path()).await.unwrap().0);
        assert!(run("git", &["remote", "add", "origin", remote.to_str().unwrap()], dir.path()).await.unwrap().0);
        assert!(run("git", &["push", "origin", "HEAD:fixture"], dir.path()).await.unwrap().0);
        let published = git_evidence(dir.path(), &["rev-parse", "HEAD"]).await.unwrap();
        let candidate = prepare_resume_worktree_at_head(dir.path(), 42, "fixture", Some(&published)).await.unwrap();
        let original = git_binding(&candidate, "C1", "rust-a", &[]).await.unwrap();
        let mut store = WorkflowStore::open(&database).unwrap();
        store.register("owner/repo#77", "fixture", original.clone(), 10).unwrap();
        let lease = store.claim("owner/repo#77", "original-worker", 20, 60_000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease, "owner/repo", 42, 21).unwrap();
        let pushed = store.begin_operation(&lease, OperationKind::Push, &original, 22).unwrap();
        store.complete_operation(&lease, &pushed.id, &format!("git-head:{published}"), 23).unwrap();
        std::fs::write(candidate.join("code.txt"), "retained repair\n").unwrap();
        assert!(run("git", &["add", "code.txt"], &candidate).await.unwrap().0);
        assert!(run("git", &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "retained candidate"], &candidate).await.unwrap().0);
        let retained = git_binding(&candidate, "C1", "rust-a", &[]).await.unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.binding = retained.clone();
        checkpoint.stage = Stage::Verify;
        checkpoint.next = NextAction { reason: "verification_infrastructure_retry".into(), action: "retry_verification".into(), eligible_at_ms: 30 };
        store.checkpoint(&lease, &checkpoint, 24).unwrap();
        store.release(&lease, 25).unwrap();
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        let events = store.events(&lease.workflow).unwrap();
        let recovered = prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await
            .expect("recorded unpublished candidate must remain executable for fresh verification");
        assert_eq!(recovered, candidate);
        assert_eq!(git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap(), retained.head);
        assert_eq!(std::fs::read_to_string(candidate.join("code.txt")).unwrap(), "retained repair\n");
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(), before);
        assert_eq!(store.events(&lease.workflow).unwrap(), events);
        assert_eq!(git_evidence(dir.path(), &["ls-remote", "origin", "refs/heads/fixture"]).await.unwrap(),
            format!("{published}\trefs/heads/fixture"), "recovery must not publish the retained candidate");
        assert!(prepare_resume_worktree_at_head(dir.path(), 42, "fixture", Some(&published)).await.is_err(),
            "the exact-head merge helper must keep rejecting the unpublished head");
        assert!(prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&"f".repeat(40))).await.is_err(),
            "a changed remote resolution must not use retained-candidate recovery");
        assert!(run("git", &["checkout", "--orphan", "unrelated-fixture"], &candidate).await.unwrap().0);
        assert!(run("git", &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--allow-empty", "-qm", "unrelated root"], &candidate).await.unwrap().0);
        assert!(prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await.is_err(),
            "an unrecorded local head cannot inherit the old recovery authority");
        let mut unrelated = retained;
        unrelated.head = git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap();
        let next_lease = store.claim(&lease.workflow, "fixture-authority", 40, 60_000).unwrap().unwrap();
        checkpoint.binding = unrelated;
        store.checkpoint(&next_lease, &checkpoint, 41).unwrap();
        store.release(&next_lease, 42).unwrap();
        assert!(prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await.is_err(),
            "matching protected metadata cannot replace publication ancestry");
    }

    #[tokio::test]
    async fn claimed_owner_guard_admits_native_work_without_granting_shipping() {
        const CHILD:&str="AUTOSHIP_CONFIGURED_OWNER_GUARD_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let control=control_dir();let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","self_improve::workflow::tests::claimed_owner_guard_admits_native_work_without_granting_shipping","--nocapture","--test-threads=1"])
                .env(CHILD,control.path()).env("AUGMENTAGENT_NATIVE_VM_POLICY",control.path().join("policy.json")).output().unwrap();
            assert!(output.status.success(),"{} {}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));return;
        }
        use std::os::unix::fs::{MetadataExt,PermissionsExt};
        let control=PathBuf::from(std::env::var_os(CHILD).unwrap());let native=control.join("native");std::fs::create_dir(&native).unwrap();std::fs::set_permissions(&native,std::fs::Permissions::from_mode(0o700)).unwrap();let metadata=native.metadata().unwrap();
        let identity=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::json!({"device":metadata.dev(),"inode":metadata.ino()})).unwrap()));
        let bootstrap=native.join("authority-bootstrap.json");std::fs::write(&bootstrap,serde_json::to_vec(&serde_json::json!({"version":1,"epoch":uuid::Uuid::new_v4().to_string(),"root_identity_sha256":identity})).unwrap()).unwrap();std::fs::set_permissions(&bootstrap,std::fs::Permissions::from_mode(0o600)).unwrap();
        let policy=control.join("policy.json");std::fs::write(&policy,serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/unused-runtime","guest_runtime":"/unused-guest","state_root":native,"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,"providers":{}})).unwrap()).unwrap();std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");let now=millis();
        let mut store=WorkflowStore::open(&path).unwrap();
        let origin=durable::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=durable::submission::Request {repository:"owner/repo".into(),target:durable::submission::Target::Pr(42),provider:Some(durable::submission::Provider::Codex),until:durable::submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),now).unwrap().admission.workflow;
        let binding=git_binding(dir.path(),"C1","rust-a",&[]).await.unwrap();store.register(&id,"candidate",binding,now).unwrap();
        store.enqueue_owner_message(&durable::discord_ingress::DiscordOrigin {message:45,..origin},&id,"Fix the test",None,now).unwrap();drop(store);
        let guard=WorkflowGuard::acquire_mode_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:None,pr:Some(42),branch:"candidate",contract:"C1",environment:"rust-a"},millis(),true).await.unwrap().unwrap();
        let accounting=guard.capacity_accounting(false).unwrap();
        assert!(accounting.report().unwrap().is_none(),"pending input hid an active owner's provider failure");
        guard.audit_logger().dispatch_allowed().unwrap();guard.audit_logger().model_call_admitted("codex").unwrap();
        assert!(accounting.report().unwrap().is_none(),"paid owner turn was mistaken for an interrupted ordinary run");
        assert!(guard.intent(OperationKind::Push).is_err());assert!(guard.intent(OperationKind::MergePr).is_err());
        let mut store=WorkflowStore::open(&path).unwrap();let revision=store.owner_control(&id).unwrap().unwrap().revision;
        store.set_owner_pause(&id,"owner",revision,true,millis()).unwrap();
        assert!(guard.audit_logger().dispatch_allowed().is_err());assert!(guard.audit_logger().model_call_admitted("codex").is_err());
        assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,1);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
        assert_eq!(std::fs::read_to_string(dir.path().join("code.txt")).unwrap(),"baseline\n");guard.finish().unwrap();
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
    async fn acceptance_metadata_uncertain_file_effect_fences_original_native_observers() {
        for failure in ["visitor-error","lease-expiry-after-write"] {
            let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
            let guard=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,branch:"candidate",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
            let inspected:Result<()>=guard.read_acceptance_metadata(||anyhow::bail!("missing completion requires metadata replay"));
            assert!(inspected.is_err());guard.audit_logger().dispatch_allowed().unwrap();
            let canonical=state.path().join("metadata-written-before-error.json");
            let clock_calls=std::cell::Cell::new(0);let initial=millis();
            let clock=|| {let n=clock_calls.get();clock_calls.set(n+1);if n==0 {initial}else{initial+LEASE_MS+1}};
            let selected_clock:&dyn Fn()->i64=if failure=="visitor-error" {&millis}else{&clock};
            let result:Result<()>=guard.with_acceptance_metadata_at(selected_clock,|_access| {
                std::fs::write(&canonical,b"authorized durable file effect")?;
                std::fs::File::open(&canonical)?.sync_all()?;
                anyhow::ensure!(failure!="visitor-error","injected failure after durable file publication");Ok(())
            });
            assert!(result.is_err());assert_eq!(std::fs::read(&canonical).unwrap(),b"authorized durable file effect");
            assert!(guard.audit_logger().dispatch_allowed().is_err(),"failed metadata admission left its old native dispatch callback usable: {failure}");
            assert!(guard.audit_logger().model_call_admitted("codex").is_err(),"failed metadata admission charged another model call");
            let mut store=WorkflowStore::open(&path).unwrap();assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,0);
            assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
            assert!(guard.finish().is_err(),"uncertain metadata did not require fresh guarded recovery");
        }
    }
    #[tokio::test]
    async fn acceptance_metadata_waiting_model_callback_is_fenced_before_accounting() {
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
        let guard=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,branch:"candidate",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
        std::thread::scope(|scope| {
            let (started,ready)=std::sync::mpsc::channel();let (finished,observed)=std::sync::mpsc::channel();
            let mut worker=None;
            let outcome:Result<()>=guard.with_acceptance_metadata(|_access| {
                let audit=guard.audit_logger();
                worker=Some(scope.spawn(move || {started.send(()).unwrap();finished.send(audit.model_call_admitted("codex").is_ok()).unwrap();}));
                ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                assert!(matches!(observed.recv_timeout(std::time::Duration::from_millis(100)),Err(std::sync::mpsc::RecvTimeoutError::Timeout)),"model callback did not wait for metadata store ownership");
                anyhow::bail!("injected metadata mutation acknowledgement failure")
            });
            assert!(outcome.is_err());
            assert!(!observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),"model callback waiting before metadata failure escaped its fence and was charged");
            worker.take().unwrap().join().unwrap();
        });
        assert_eq!(WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        assert!(guard.finish().is_err());
    }
    #[tokio::test]
    async fn acceptance_metadata_commit_serializes_owner_pause_and_refuses_stale_writers() {
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
        let guard=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,branch:"candidate",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
        let mut owner=WorkflowStore::open(&path).unwrap();let id=guard.id().to_owned();
        let before=owner.snapshot(&id).unwrap().unwrap();let canonical=state.path().join("canonical-metadata.json");
        std::thread::scope(|scope| {
            let (started,ready)=std::sync::mpsc::channel();let (committed,observed)=std::sync::mpsc::channel();
            let mut owner_thread=None;
            guard.with_acceptance_metadata(|_access| {
                owner_thread=Some(scope.spawn(move || {
                    started.send(()).unwrap();
                    let control=owner.set_owner_pause(&id,"github:owner",0,true,millis()).unwrap();
                    committed.send(control).unwrap();
                }));
                ready.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                assert!(matches!(observed.recv_timeout(std::time::Duration::from_millis(100)),Err(std::sync::mpsc::RecvTimeoutError::Timeout)),"owner pause committed between metadata admission and its file write");
                std::fs::write(&canonical,b"durable canonical metadata")?;
                std::fs::File::open(&canonical)?.sync_all()?;Ok(())
            }).unwrap();
            assert!(observed.recv_timeout(std::time::Duration::from_secs(2)).unwrap().paused);
            owner_thread.take().unwrap().join().unwrap();
        });
        assert_eq!(std::fs::read(&canonical).unwrap(),b"durable canonical metadata");
        assert!(guard.with_acceptance_metadata(|_access| {std::fs::write(&canonical,b"stale overwrite")?;Ok(())}).is_err());
        assert_eq!(std::fs::read(&canonical).unwrap(),b"durable canonical metadata");
        let mut store=WorkflowStore::open(&path).unwrap();let after=store.snapshot(guard.id()).unwrap().unwrap();
        assert_eq!(after.checkpoint,before.checkpoint);assert_eq!(after.model_calls,0);assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
        guard.finish().unwrap();
    }
    #[tokio::test]
    async fn owner_input_defers_an_interrupted_ordinary_run_without_changing_its_checkpoint() {
        for paid in [false,true] {
            let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
            let origin=durable::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
            let request=durable::submission::Request {repository:"owner/repo".into(),target:durable::submission::Target::Pr(42),provider:None,until:durable::submission::Milestone::Merged};
            let id=WorkflowStore::open(&path).unwrap().submit_discord_authorized(&origin,&request,&"a".repeat(64),millis()).unwrap().admission.workflow;
            let mut guard=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:None,pr:Some(42),branch:"candidate",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
            guard.checkpoint(Stage::Review,"verified","review",0,&["preserved-evidence".into()]).await.unwrap();
            let accounting=guard.capacity_accounting(false).unwrap();
            if paid {guard.audit_logger().model_call_admitted("codex").unwrap();}
            let before=WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();
            WorkflowStore::open(&path).unwrap().enqueue_owner_message(&durable::discord_ingress::DiscordOrigin {message:45,..origin},&id,"Private clarification arriving during ordinary work",None,millis()).unwrap();
            assert!(guard.audit_logger().dispatch_allowed().is_err());
            assert!(!guard.audit_logger().resource_capacity_seen());
            let report=accounting.report().unwrap().expect("pending owner input lost its ordinary-run deferral");
            assert!(report.message.contains("owner input"));assert_eq!(report.billed,paid);assert!(report.held,"pending input was reported as productive progress");
            assert_eq!(WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap().checkpoint,before.checkpoint);
            guard.finish().unwrap();
            let mut store=WorkflowStore::open(&path).unwrap();assert!(!store.owner_control(&id).unwrap().unwrap().paused);
            assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,u64::from(paid));
            assert!(store.claim_owner_turn_for_setup(&id,"owner-consumer",millis(),60_000).unwrap().is_some(),"ordinary deferral stranded owner input in a permanent hold");
            assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
        }
    }
    #[tokio::test]
    async fn owner_pause_fences_live_guard_dispatch_without_losing_completion_receipts() {
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
        let mut guard=WorkflowGuard::acquire_at(&path,Start {
            repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(7),pr:Some(42),
            branch:"fix/seven",contract:"C1",environment:"rust-a",
        },millis()).await.unwrap().unwrap();
        let operation=guard.intent(OperationKind::Push).unwrap();
        let accounting=guard.capacity_accounting(false).unwrap();
        let before=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        WorkflowStore::open(&path).unwrap().set_owner_pause(guard.id(),"github:owner",0,true,millis()).unwrap();
        assert!(guard.transition(Stage::Review,"review","review",0).await.is_err());
        assert!(guard.audit_logger().model_call_admitted("claude").is_err());
        assert!(augmentagent_channel_core::run_resources::check_requirement("verification",Some(&guard.audit_logger())).unwrap_err().to_string().contains("owner-paused"));
        assert!(!guard.audit_logger().resource_capacity_seen(),"owner pause is not resource exhaustion");
        assert!(accounting.report().unwrap().unwrap().message.contains("owner pause"));
        assert!(guard.validate_binding(guard.binding()).await.is_err());
        guard.observe_remote(&operation,"published-head",None).unwrap();
        let after=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        assert_eq!(after.checkpoint,before.checkpoint);assert_eq!(after.model_calls,before.model_calls);
        assert!(WorkflowStore::open(&path).unwrap().pending_operations(guard.id()).unwrap().is_empty());
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
    async fn sweep_recovered_audit_requires_new_review() {
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
        let builder=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(7),pr:None,branch:"fix/seven",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
        let head=builder.binding.head.clone();let id=builder.id().to_owned();let audit=builder.audit_logger();
        builder.record_review("approved",true,"prior approval").unwrap();builder.finish().unwrap();
        drop(audit.begin_write().unwrap());assert!(audit.protected_health().is_err());
        assert!(SweepGuard::acquire_at(&path,"owner/repo",7,42,"fix/seven",&head,"old approval",millis()).unwrap().is_none(),"audit recovery invalidates the reviewed binding before merge sweep admission");
        let store=WorkflowStore::open(&path).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().lease_until_ms,0);assert!(store.current_review_history(&id).unwrap().is_empty());
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
    async fn published_pr_milestone_stops_pr_only_work_after_real_push_and_bound_review() {
        let dir=repo();let remote=tempfile::tempdir().unwrap();
        assert!(run("git",&["init","--bare","-q"],remote.path()).await.unwrap().0);
        assert!(run("git",&["remote","add","origin",remote.path().to_str().unwrap()],dir.path()).await.unwrap().0);
        let state=control_dir();let path=state.path().join("workflow.db");
        let mut store=WorkflowStore::open(&path).unwrap();
        let id=store.submit_authorized("pr-only","owner",&durable::submission::Request {
            repository:"owner/repo".into(),target:durable::submission::Target::Pr(42),provider:None,until:durable::submission::Milestone::Pr
        },millis()).unwrap().workflow;drop(store);
        let mut guard=WorkflowGuard::acquire_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",
            source_issue:None,pr:Some(42),branch:"fix/pr",contract:"contract",environment:"rust"},millis()).await.unwrap().unwrap();
        let mut review=super::super::progress_tests::synthetic_aligned_review(&guard.binding.head,true);
        let evidence=review.structured.as_mut().unwrap();evidence.binding.contract_hash=guard.binding.contract.clone();
        for pass in &mut evidence.passes {pass.binding=evidence.binding.clone();pass.coverage.binding=evidence.binding.clone();}
        evidence.adjudication.as_mut().unwrap().binding=evidence.binding.clone();
        assert!(guard.published_pr_ready(42,&review).await.is_err(),"approval without publication cannot finish");
        guard.push("fix/pr").await.unwrap();
        let mut wrong=super::super::progress_tests::synthetic_aligned_review("other-head",true);
        assert!(guard.published_pr_ready(42,&wrong).await.is_err());
        wrong.structured=None;
        assert!(guard.published_pr_ready(42,&wrong).await.is_err(),"legacy booleans are not a fresh receipt");
        assert!(guard.published_pr_ready(42,&review).await.unwrap());
        guard.finish().unwrap();
        let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
        let snapshot=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(snapshot.checkpoint.stage,Stage::AwaitChecks);
        assert_eq!(snapshot.checkpoint.next.reason,"pr_milestone_reached");
        assert!(store.pr_ready(&id).unwrap().is_some());
        assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
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

    #[test]
    fn owner_body_receives_registered_requirements_without_private_verifier_code_or_oracles() {
        use crate::autoship_contract::*;
        let files=std::collections::BTreeMap::from([("autoship_verifiers/private.py".to_owned(),"print('PRIVATE_ADAPTER_FIXTURE_ONLY')\n".to_owned())]);
        let contract=AcceptanceContract {schema_version:1,revision:2,issue:Some(77),pull_request:None,
            criteria:vec![Criterion {id:"C1".into(),requirement:"Preserve the existing owner settings".into(),verifier_ids:vec!["observe".into()]}],
            allowed_paths:vec![PathScope::File("src/shipping.rs".into()),PathScope::File("autoship_verifiers/private.py".into())],
            verifiers:vec![Verifier {id:"observe".into(),command:VerifierCommand::BlackBox {invocation:BehaviorInvocation::PythonScript {file:"autoship_verifiers/private.py".into(),args:vec![]},oracle:OutputOracle::Text {value:"PRIVATE_ORACLE_FIXTURE_ONLY".into()}},pinned_files:vec!["autoship_verifiers/private.py".into()],failure_witness:"behavior differs".into()}]};
        let registration=RegisteredContract::register_from_files(contract.clone(),"a".repeat(40),"test".into(),&files).unwrap();
        let mut prepared=acceptance::PreparedAcceptance {proposal:acceptance::ContractProposal {contract,files},baseline:acceptance::BaselineEvidence {registration,receipts:vec![]}};
        let prompt=owner_body_prompt(&prepared,prepared.baseline.registration.contract_hash(),"Fix the reported bug").unwrap();
        assert!(prompt.contains("Preserve the existing owner settings"),"registered owner requirement was omitted from builder prompt");
        assert!(prompt.contains("src/shipping.rs"));assert!(prompt.contains("Fix the reported bug"));
        assert!(!prompt.contains("PRIVATE_ORACLE_FIXTURE_ONLY"));assert!(!prompt.contains("PRIVATE_ADAPTER_FIXTURE_ONLY"));
        assert!(!prompt.contains("autoship_verifiers/private.py"),"private verifier path was offered as a product edit path");
        assert!(owner_body_prompt(&prepared,"wrong-contract","Fix").is_err());
        prepared.proposal.files.insert("autoship_verifiers/private.py".into(),"changed adapter".into());
        assert!(owner_body_prompt(&prepared,prepared.baseline.registration.contract_hash(),"Fix").is_err());
    }

    #[tokio::test]
    async fn owner_body_admission_rechecks_original_preparation_in_actual_callbacks() {
        let control=control_dir();
        let status=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::workflow::tests::workflow_guard_initial_native_owner_registers_before_returning","--nocapture","--test-threads=1"])
            .env("AUTOSHIP_INITIAL_GUARD_CHILD",control.path())
            .env("AUTOSHIP_INITIAL_GUARD_MODE","owner-early-acceptance-producer-body-admission")
            .env("XDG_STATE_HOME",control.path().join("operator-state"))
            .env("AUGMENTAGENT_NATIVE_VM_POLICY",control.path().join("policy.json"))
            .status().unwrap();
        assert!(status.success(),"original owner preparation admission callback fixture failed");
    }
    async fn check_prepared_owner_body_callbacks(guard:&WorkflowGuard,prepared:Arc<acceptance::owner::PreparedOwnerAcceptance>,artifact:&Path) {
        let preparation=prepared.validate_activated(guard,&guard.worktree).unwrap();
        let turn=guard.store.lock().unwrap().owner_turn_for_lease(&guard.lease,millis()).unwrap();
        let dispatch=Arc::new(guard.store.lock().unwrap().begin_prepared_owner_body_dispatch(&turn,durable::submission::Provider::Codex,&preparation,millis()).unwrap());
        let admission=Arc::new(OwnerBodyAdmission {dispatch:dispatch.clone(),preparation:prepared.clone(),provider:durable::submission::Provider::Codex});
        guard.install_model_observers(Some(admission)).unwrap();
        let db=rusqlite::Connection::open(&guard.state).unwrap();
        let (original_preparation,original_receipts):(String,String)=db.query_row("SELECT record,receipts FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        let activation:String=db.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='owner_acceptance_registered'",[guard.id()],|r|r.get(0)).unwrap();
        let reservation:String=db.query_row("SELECT record FROM workflow_owner_body_dispatch WHERE request_id=?1",[&turn.request_id],|r|r.get(0)).unwrap();
        let bytes=std::fs::read(artifact).unwrap();
        for fault in ["preparation-pair","reservation-downgrade","artifact","provider","intact"] {
            match fault {
                "preparation-pair"=>{
                    db.execute("DELETE FROM workflow_owner_preparation WHERE request_id=?1",[&turn.request_id]).unwrap();
                    db.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.preparation') WHERE workflow=?1 AND kind='owner_acceptance_registered'",[guard.id()]).unwrap();
                },
                "reservation-downgrade"=>{
                    db.execute("UPDATE workflow_owner_body_dispatch SET record=json_remove(record,'$.preparation') WHERE request_id=?1",[&turn.request_id]).unwrap();
                    db.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.preparation') WHERE workflow=?1 AND kind='owner_body_dispatch_started'",[guard.id()]).unwrap();
                },
                "artifact"=>{let mut changed=bytes.clone();changed.push(b' ');std::fs::write(artifact,changed).unwrap();},
                _=>{},
            }
            let before=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
            let events=guard.store.lock().unwrap().events(guard.id()).unwrap();
            if matches!(fault,"preparation-pair"|"artifact") {
                let reasoner=FallbackReasoner::for_tests(vec![],augmentagent_channel_core::CooldownLatch::at(artifact.parent().unwrap().join("fixture-cooldowns.json")));
                let error=guard.run_owner_body(&reasoner,&prepared).await.err().expect("damaged preparation reached owner body execution");
                assert!(format!("{error:#}").contains("owner preparation"),"refusal occurred after original preparation admission: {error:#}");
                assert_eq!(reasoner.calls(),0);
            }
            // A plain thread has no Tokio task-local context. Admission must
            // still use the captured preparation and reservation, never fallback.
            let audit=guard.audit.clone();
            std::thread::spawn(move|| {
                let healthy=matches!(fault,"provider"|"intact");
                assert_eq!(audit.dispatch_allowed().is_ok(),healthy,"{fault}: actual dispatch observer");
                let provider=if fault=="provider" {"claude"}else{"codex"};
                assert_eq!(audit.model_call_admitted(provider).is_ok(),fault=="intact","{fault}: actual model admission observer");
            }).join().unwrap();
            if fault!="intact" {
                assert_eq!(guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap(),before);
                assert_eq!(guard.store.lock().unwrap().events(guard.id()).unwrap(),events);
            }
            match fault {
                "preparation-pair"=>{
                    db.execute("INSERT INTO workflow_owner_preparation VALUES(?1,?2,?3)",rusqlite::params![turn.request_id,original_preparation,original_receipts]).unwrap();
                    db.execute("UPDATE workflow_event SET payload=?2 WHERE workflow=?1 AND kind='owner_acceptance_registered'",rusqlite::params![guard.id(),activation]).unwrap();
                },
                "reservation-downgrade"=>{
                    db.execute("UPDATE workflow_owner_body_dispatch SET record=?2 WHERE request_id=?1",rusqlite::params![turn.request_id,reservation]).unwrap();
                    db.execute("UPDATE workflow_event SET payload=?2 WHERE workflow=?1 AND kind='owner_body_dispatch_started'",rusqlite::params![guard.id(),reservation]).unwrap();
                },
                "artifact"=>std::fs::write(artifact,&bytes).unwrap(),
                _=>{},
            }
            prepared.validate_activated(guard,&guard.worktree).unwrap();
        }
        // Even losing both slot metadata and its new event field must not
        // broaden a previously used captured callback into legacy admission.
        db.execute("DELETE FROM workflow_owner_body_model_admission WHERE workflow=?1",[guard.id()]).unwrap();
        db.execute("UPDATE workflow_event SET payload=json_remove(payload,'$.owner_body') WHERE workflow=?1 AND kind='native_model_admitted'",[guard.id()]).unwrap();
        let before=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
        assert!(guard.audit.model_call_admitted("codex").is_err(),
            "owner body reservation admitted a second native call that could replay the original input");
        assert_eq!(guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap(),before);
        let mut store=guard.store.lock().unwrap();
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,1);
        assert_eq!(store.pending_owner_messages(guard.id()).unwrap(),1);assert!(!store.owner_control(guard.id()).unwrap().unwrap().paused);
        assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
    }

    #[tokio::test]
    async fn owner_preparation_refuses_replaced_checkout_with_identical_git_binding() {
        let control=control_dir();
        let status=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::workflow::tests::workflow_guard_initial_native_owner_registers_before_returning","--nocapture","--test-threads=1"])
            .env("AUTOSHIP_INITIAL_GUARD_CHILD",control.path())
            .env("AUTOSHIP_INITIAL_GUARD_MODE","owner-early-acceptance-producer-replaced-checkout")
            .env("XDG_STATE_HOME",control.path().join("operator-state"))
            .env("AUGMENTAGENT_NATIVE_VM_POLICY",control.path().join("policy.json"))
            .status().unwrap();
        assert!(status.success(),"owner preparation accepted a substituted checkout identity");
    }

    #[tokio::test]
    async fn workflow_guard_initial_native_owner_registers_before_returning() {
        const CHILD:&str="AUTOSHIP_INITIAL_GUARD_CHILD";
        const MODE:&str="AUTOSHIP_INITIAL_GUARD_MODE";
        if std::env::var_os(CHILD).is_none() {
            for mode in ["fresh","missing-policy","unconfigured","legacy-owner","legacy-call","legacy-workflow","physical-prior-state","configuration-recovery","healthy-handoff","lost-source-audit","pending-settlement-recovery","promotion-failure-recovery","lost-native-authority","native-sweep","native-sweep-lost-authority","resolved-abort-acquisition","closed-recovery-rollback","closed-recovery-history-loss","terminal-abort-cleanup","owner-setup-healthy","owner-setup-policy-retry","owner-setup-history-loss","owner-setup-write-failure","owner-setup-unconfigured","owner-setup-index-corruption","owner-setup-codex-session","owner-setup-claude-session","owner-setup-codex-revision-session","owner-setup-claude-revision-session","owner-setup-codex-revision-setup-failure-session","owner-setup-codex-revision-unconfigured-session","owner-early-fresh","owner-early-explicit-pause","owner-early-no-input","owner-early-corrupt-input","owner-early-history","owner-early-policy-retry","owner-early-register-write-failure","owner-early-initial-acceptance","owner-early-initial-acceptance-green-baseline","owner-early-initial-acceptance-write-failure","owner-early-initial-acceptance-history-loss","owner-early-initial-acceptance-setup-failure","owner-early-initial-acceptance-baseline-tamper","owner-early-initial-acceptance-unconfigured","owner-early-initial-acceptance-repeat","owner-early-initial-acceptance-wrong-source","owner-early-initial-acceptance-first-build","owner-early-initial-acceptance-first-build-dirty","owner-early-initial-acceptance-first-build-staged","owner-early-initial-acceptance-first-build-ignored","owner-early-initial-acceptance-first-build-off-base","owner-early-initial-acceptance-first-build-orphan-main","owner-early-initial-acceptance-first-build-runtime","owner-early-initial-acceptance-first-build-legacy-call","owner-early-initial-acceptance-first-build-workflow-call","owner-early-initial-acceptance-first-build-readonly-call","owner-early-initial-acceptance-first-build-lost-call-history","owner-early-initial-acceptance-first-build-policy-lost","owner-early-initial-acceptance-first-build-leading-space","owner-early-initial-acceptance-first-build-counter-loss","owner-early-initial-acceptance-first-build-new-verifier","owner-early-initial-acceptance-first-build-retired-native","owner-early-initial-acceptance-first-build-grafted-main","owner-early-acceptance-producer","owner-early-acceptance-producer-fenced","owner-early-acceptance-producer-replaced-checkout","owner-early-acceptance-producer-body-admission"] {
                if mode.ends_with("-session") && !cfg!(target_os="linux") {continue;}
                let control=control_dir();let mut child=std::process::Command::new(std::env::current_exe().unwrap());
                child.args(["--exact","self_improve::workflow::tests::workflow_guard_initial_native_owner_registers_before_returning","--nocapture","--test-threads=1"])
                    .env(CHILD,control.path()).env(MODE,mode).env("XDG_STATE_HOME",control.path().join("operator-state"));
                if matches!(mode,"unconfigured"|"configuration-recovery") {child.env_remove("AUGMENTAGENT_NATIVE_VM_POLICY");}
                else {child.env("AUGMENTAGENT_NATIVE_VM_POLICY",control.path().join("policy.json"));}
                assert!(child.status().unwrap().success(),"native registration guard fixture failed: {mode}");
            }
            return;
        }
        use std::os::unix::fs::{MetadataExt,PermissionsExt};
        let control=PathBuf::from(std::env::var_os(CHILD).unwrap());let native=control.join("native");
        std::fs::create_dir(&native).unwrap();std::fs::set_permissions(&native,std::fs::Permissions::from_mode(0o700)).unwrap();
        let info=native.metadata().unwrap();
        let root_identity=format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(
            &serde_json::json!({"device":info.dev(),"inode":info.ino()})).unwrap()));
        // Explicit fresh-root provisioning intent; no provider, credentials or VM.
        let bootstrap=native.join("authority-bootstrap.json");
        std::fs::write(&bootstrap,serde_json::to_vec(&serde_json::json!({"version":1,"epoch":uuid::Uuid::new_v4().to_string(),"root_identity_sha256":root_identity})).unwrap()).unwrap();
        std::fs::set_permissions(&bootstrap,std::fs::Permissions::from_mode(0o600)).unwrap();
        let policy=control.join("policy.json");
        std::fs::write(&policy,serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/unused-runtime","guest_runtime":"/unused-guest",
            "state_root":native,"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,"providers":{}})).unwrap()).unwrap();
        std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
        let mode=std::env::var(MODE).unwrap();
        struct GuardRepo {root:tempfile::TempDir,candidate:Option<PathBuf>}
        impl GuardRepo {fn path(&self)->&Path {self.candidate.as_deref().unwrap_or_else(||self.root.path())}}
        let root=repo();
        let candidate=if mode.contains("-first-build") {
            let candidate=root.path().join(".self-improve-worktrees/first-owner");
            recovery::prepare(root.path(),&candidate,"fixture").await.unwrap();
            git_evidence(root.path(),&["config","user.name","Fixture"]).await.unwrap();git_evidence(root.path(),&["config","user.email","fixture@example.invalid"]).await.unwrap();Some(candidate)
        }else{None};
        let dir=GuardRepo {root,candidate};let state=control.join("workflow.db");
        let id=durable::workflow_id("owner/repo",77).unwrap();
        if mode.starts_with("owner-setup-") || mode.starts_with("owner-early-") {
            let origin=durable::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
            let request=durable::submission::Request {repository:"owner/repo".into(),target:durable::submission::Target::Issue(77),provider:Some(if mode.contains("-claude-") {durable::submission::Provider::Claude}else{durable::submission::Provider::Codex}),until:durable::submission::Milestone::Merged};
            WorkflowStore::open(&state).unwrap().submit_discord_authorized(&origin,&request,&"a".repeat(64),millis()).unwrap();
        }
        if mode=="legacy-workflow" {
            let binding=git_binding(dir.path(),"C1","test",&[]).await.unwrap();
            // No live WorkflowStore connection exists while this fixture
            // recreates a pre-feature row and generic registered event.
            drop(WorkflowStore::open(&state).unwrap());
            let db=augmentagent_store::rusqlite::Connection::open(&state).unwrap();
            let checkpoint=Checkpoint {stage:Stage::Intake,binding,evidence:vec![],next:NextAction {reason:"legacy".into(),action:"scope".into(),eligible_at_ms:0}};
            db.execute("INSERT INTO workflow(id,branch,checkpoint,heartbeat_at,progress_at) VALUES(?1,'fixture',?2,1,1)",augmentagent_store::rusqlite::params![id,serde_json::to_string(&checkpoint).unwrap()]).unwrap();
            db.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,0,'registered','fixture',1)",[&id]).unwrap();
        }
        if matches!(mode.as_str(),"legacy-owner"|"legacy-call") {
            let binding=git_binding(dir.path(),"C1","test",&[]).await.unwrap();let now=millis();
            let mut store=WorkflowStore::open(&state).unwrap();store.register(&id,"fixture",binding,now).unwrap();
            let lease=store.claim(&id,"legacy",now,LEASE_MS).unwrap().unwrap();
            if mode=="legacy-owner" {store.register_native_owner(&lease,"/private/legacy-audit",now).unwrap();}
            else {store.charge(&lease,"legacy-call",ResourceCharge {model_calls:1,..Default::default()},now).unwrap();}
            store.release(&lease,millis()).unwrap();
        }
        if mode=="missing-policy" {std::fs::remove_file(&policy).unwrap();}
        if mode=="physical-prior-state" {
            let provider=native.join("codex");std::fs::create_dir(&provider).unwrap();
            std::fs::set_permissions(&provider,std::fs::Permissions::from_mode(0o700)).unwrap();
            let session=provider.join("session-prior");std::fs::create_dir(&session).unwrap();
            std::fs::set_permissions(&session,std::fs::Permissions::from_mode(0o700)).unwrap();
            let owned=session.join("owned.json");std::fs::write(&owned,serde_json::to_vec(&serde_json::json!({
                "version":1,"audit_path":"/private/prior-audit","owner":{"workflow_id":id,"lease_generation":1}})).unwrap()).unwrap();
            std::fs::set_permissions(&owned,std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let private_first_files=mode.contains("-first-build").then(|| {
            let mut files=std::collections::BTreeMap::from([("autoship_verifiers/first.py".to_owned(),"print(False)\n".to_owned())]);
            if mode.ends_with("-new-verifier") {files.insert("autoship_verifiers/second.py".into(),"print(False)\n".into());}files
        });
        let revision_contracts=if mode.contains("-revision-") || mode.contains("-initial-acceptance") {
            use crate::autoship_contract::*;
            let mut contract=AcceptanceContract {schema_version:SCHEMA_VERSION,revision:1,issue:Some(77),pull_request:None,
                criteria:vec![Criterion {id:"C1".into(),requirement:"Original requirement".into(),verifier_ids:vec!["ready".into()]}],
                allowed_paths:vec![PathScope::File("implementation.txt".into())],
                verifiers:vec![Verifier {id:"ready".into(),command:VerifierCommand::BlackBox {
                    invocation:BehaviorInvocation::CargoBinary {package:"fixture".into(),bin:"behavior".into(),args:vec![]},
                    oracle:OutputOracle::Json {value:serde_json::json!(true)}},pinned_files:vec!["code.txt".into()],failure_witness:"expected ready".into()}]};
            if let Some(files)=&private_first_files {
                contract.verifiers[0].command=VerifierCommand::BlackBox {invocation:BehaviorInvocation::PythonScript {file:"autoship_verifiers/first.py".into(),args:vec![]},oracle:OutputOracle::Json {value:serde_json::json!(true)}};
                contract.verifiers[0].pinned_files=files.keys().cloned().collect();
                contract.allowed_paths.extend(files.keys().cloned().map(PathScope::File));
            }
            let base=git_evidence(dir.path(),&["rev-parse","HEAD"]).await.unwrap();
            let register=|contract|if let Some(files)=&private_first_files {RegisteredContract::register_from_files(contract,base.clone(),"test".into(),files).unwrap()}
                else {RegisteredContract::register(dir.path(),contract,base.clone(),"test".into()).unwrap()};
            let previous=register(contract.clone());let mut contract=contract;contract.revision=2;
            contract.criteria.push(Criterion {id:"C2".into(),requirement:"Additional owner requirement".into(),verifier_ids:vec!["ready".into()]});
            let next=register(contract);Some((previous,next))
        }else{None};
        let contract_label=if mode.contains("-initial-acceptance") {"C1".into()}else{revision_contracts.as_ref().map(|(previous,_)|format!("registered:{}",previous.contract_hash())).unwrap_or_else(||"C1".into())};
        let start=||Start{repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",
            source_issue:Some(77),pr:None,branch:"fixture",contract:&contract_label,environment:"test"};
        if mode.starts_with("owner-early-") {
            let mut store=WorkflowStore::open(&state).unwrap();
            assert!(store.snapshot(&id).unwrap().is_none(),"fixture must start before workflow candidate registration");
            if mode=="owner-early-no-input" {
                assert!(WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.unwrap().is_none());
                assert!(store.snapshot(&id).unwrap().is_none());return;
            }
            let origin=durable::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:45};
            store.enqueue_owner_message(&origin,&id,"Early synthetic clarification",None,millis()).unwrap();
            if mode=="owner-early-explicit-pause" {
                let revision=store.owner_control(&id).unwrap().unwrap().revision;
                let paused=store.set_owner_pause(&id,"fixture-owner",revision,true,millis()).unwrap();
                drop(store);
                assert!(WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.unwrap().is_none(),"explicit pause started owner setup");
                let mut store=WorkflowStore::open(&state).unwrap();
                assert!(store.snapshot(&id).unwrap().is_none(),"explicit pause created a native candidate");
                assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
                let resumed=store.set_owner_pause(&id,"fixture-owner",paused.revision,false,millis()).unwrap();
                assert!(store.require_owner_dispatch(&id).is_err(),"resume erased the pending-input barrier");
                drop(store);
                let guard=WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.unwrap().expect("explicit resume must permit the queued owner turn");
                guard.validate_before_side_effect().unwrap();
                let mut store=WorkflowStore::open(&state).unwrap();
                assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);
                assert!(store.native_owner_binding(&id,guard.lease.generation).unwrap().is_some());
                assert!(store.claim_productive(&id,"ordinary-must-wait",millis(),LEASE_MS).unwrap().is_none());
                store.set_owner_pause(&id,"fixture-owner",resumed.revision,true,millis()).unwrap();
                assert!(guard.validate_before_side_effect().is_err(),"new explicit pause did not fence the active owner guard");
                assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
                assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
                return;
            }
            let control_before=store.owner_control(&id).unwrap();drop(store);
            let policy_bytes=std::fs::read(&policy).unwrap();
            if mode=="owner-early-corrupt-input" {rusqlite::Connection::open(&state).unwrap().execute("UPDATE workflow_owner_message SET body='Different instruction' WHERE workflow=?1",[&id]).unwrap();}
            if mode=="owner-early-history" {let db=rusqlite::Connection::open(&state).unwrap();db.pragma_update(None,"foreign_keys",false).unwrap();db.execute("INSERT INTO workflow_event(workflow,generation,kind,payload,at_ms) VALUES(?1,4,'native_model_admitted','lost candidate history',1)",[&id]).unwrap();}
            if mode=="owner-early-policy-retry" {std::fs::remove_file(&policy).unwrap();}
            if mode=="owner-early-register-write-failure" {rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_early_owner_registration BEFORE INSERT ON workflow_event WHEN NEW.kind='registered' BEGIN SELECT RAISE(ABORT,'injected early registration failure'); END;").unwrap();}
            let result=WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await;
            if matches!(mode.as_str(),"owner-early-corrupt-input"|"owner-early-history") {
                assert!(result.is_err());let store=WorkflowStore::open(&state).unwrap();assert!(store.snapshot(&id).unwrap().is_none(),"refused first owner input created a fresh origin");assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),control_before);return;
            }
            let mut first=if mode=="owner-early-register-write-failure" {
                assert!(result.is_err());let store=WorkflowStore::open(&state).unwrap();assert!(store.snapshot(&id).unwrap().is_none());assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),control_before);assert!(store.events(&id).unwrap().is_empty());drop(store);
                let db=rusqlite::Connection::open(&state).unwrap();let origins:i64=db.query_row("SELECT count(*) FROM workflow_native_registration_origin WHERE workflow=?1",[&id],|r|r.get(0)).unwrap();assert_eq!(origins,0);db.execute_batch("DROP TRIGGER deny_early_owner_registration").unwrap();drop(db);
                WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.unwrap().expect("rolled-back first registration must retry without a new origin")
            }else if mode=="owner-early-policy-retry" {
                assert!(result.is_err());let store=WorkflowStore::open(&state).unwrap();assert!(store.snapshot(&id).unwrap().is_some(),"legitimate first-candidate binding was lost during native setup refusal");drop(store);
                std::fs::write(&policy,&policy_bytes).unwrap();std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
                WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.unwrap().expect("early owner turn must retry only its unstarted setup")
            }else{result.expect("early owner input must acquire its first candidate without unpausing").unwrap()};
            if mode.starts_with("owner-early-acceptance-producer") {
                let before=WorkflowStore::open(&state).unwrap().snapshot(&id).unwrap().unwrap();
                let original=first.audit.native_store_binding().unwrap().unwrap();
                let prepared=acceptance::owner::test_cached_preparation(dir.path(),&control.join("owner-producer-control"),&first).await.unwrap();
                assert_eq!(WorkflowStore::open(&state).unwrap().snapshot(&id).unwrap().unwrap(),before,"preparation changed workflow state");
                if mode.ends_with("-replaced-checkout") {
                    for replacement in ["git-directory","worktree-root"] {
                        let source=if replacement=="git-directory" {dir.path().join(".git")}else{dir.path().to_path_buf()};
                        let retained=tempfile::tempdir_in(dir.path().parent().unwrap()).unwrap();
                        let retained_source=retained.path().join("original");
                        std::fs::rename(&source,&retained_source).unwrap();
                        assert!(std::process::Command::new("cp").arg("-a").arg(&retained_source).arg(&source).status().unwrap().success());
                        assert_eq!(git_binding(dir.path(),&contract_label,"test",&[]).await.unwrap(),first.binding,
                            "replacement fixture must retain exact head/tree/product evidence");
                        assert!(prepared.validate_current(&first,dir.path(),None).is_err(),
                            "{replacement}: owner preparation accepted a substituted physical checkout");
                        assert!(prepared.activation_binding().is_err(),"{replacement}: substituted checkout acquired activation binding");
                        assert!(prepared.test_reload_cached(dir.path(),&first).await.is_err(),"{replacement}: cached factory rebound the original request to a substitute");
                        assert_eq!(WorkflowStore::open(&state).unwrap().snapshot(&id).unwrap().unwrap(),before);
                        std::fs::remove_dir_all(&source).unwrap();std::fs::rename(&retained_source,&source).unwrap();
                        prepared.validate_current(&first,dir.path(),None).unwrap();prepared.activation_binding().unwrap();
                        prepared.test_reload_cached(dir.path(),&first).await.unwrap();
                    }
                    first.finish().unwrap();
                }else if mode.ends_with("-fenced") {
                    let mut store=WorkflowStore::open(&state).unwrap();let revision=store.owner_control(&id).unwrap().unwrap().revision;
                    store.set_owner_pause(&id,"fixture-owner",revision,true,millis()).unwrap();drop(store);
                    assert!(prepared.validate_current(&first,dir.path(),None).is_err(),"stale prepared acceptance escaped owner fence");
                    assert!(first.activate_prepared_owner_acceptance(&prepared,None).await.is_err());
                }else {
                    assert!(!dir.path().join("autoship_verifiers/observe.py").exists());
                    first=first.activate_prepared_owner_acceptance(&prepared,None).await.expect("private owner acceptance must activate without installing adapters into product");first.validate_before_side_effect().unwrap();
                    assert!(!dir.path().join("autoship_verifiers/observe.py").exists(),"owner activation installed a private adapter into product");
                    assert!(prepared.validate_current(&first,dir.path(),None).is_err(),"old producer proof survived activation");
                    let db=rusqlite::Connection::open(&state).unwrap();
                    let raw:String=db.query_row("SELECT payload FROM workflow_event WHERE workflow=?1 AND kind='owner_acceptance_registered'",[&id],|row|row.get(0)).unwrap();
                    let activation:serde_json::Value=serde_json::from_str(&raw).unwrap();
                    let artifacts=std::fs::read_dir(control.join("owner-producer-control/owner-preparations")).unwrap().collect::<std::io::Result<Vec<_>>>().unwrap();
                    assert_eq!(artifacts.len(),1);
                    let bytes=std::fs::read(artifacts[0].path()).unwrap();
                    assert_eq!(activation["preparation"]["artifact_sha256"],format!("{:x}",Sha256::digest(&bytes)),"owner activation must retain the exact protected preparation bytes");
                    drop(db);
                    if mode.ends_with("-body-admission") {
                        check_prepared_owner_body_callbacks(&first,Arc::new(prepared),&artifacts[0].path()).await;
                    }
                    assert_eq!(first.audit.native_store_binding().unwrap(),Some(original));first.finish().unwrap();
                }
                let store=WorkflowStore::open(&state).unwrap();assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap().unwrap().paused,mode.ends_with("-fenced"));
                assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,if mode.ends_with("-body-admission"){1}else{0});return;
            }
            if mode.contains("-initial-acceptance") {
                let mut registration=revision_contracts.as_ref().unwrap().0.clone();
                if mode.ends_with("-wrong-source") {
                    let mut contract=registration.contract().clone();contract.issue=Some(88);
                    registration=crate::autoship_contract::RegisteredContract::register(dir.path(),contract,registration.baseline_head().into(),"test".into()).unwrap();
                }
                let fixture=tempfile::tempdir().unwrap();
                if let Some(files)=&private_first_files {acceptance::install_harness(fixture.path(),&acceptance::ContractProposal {contract:registration.contract().clone(),files:files.clone()}).unwrap();}
                let receipt=registration.observe(if private_first_files.is_some(){fixture.path()}else{dir.path()},"ready",registration.binding(registration.baseline_head()).unwrap(),&crate::autoship_contract::Observation {
                    exit_code:Some(0),stdout:if mode.ends_with("-green-baseline"){"true".into()}else{"false".into()},stderr:String::new(),interrupted:false,output_complete:true,
                }).unwrap();
                let mut prepared=acceptance::PreparedAcceptance {proposal:acceptance::ContractProposal {contract:registration.contract().clone(),files:private_first_files.clone().unwrap_or_else(||std::collections::BTreeMap::from([("code.txt".into(),"baseline\n".into())]))},baseline:acceptance::BaselineEvidence {registration,receipts:vec![receipt]}};
                assert!(first.bind_acceptance(&prepared).is_err(),"ordinary binding must not poison an owner turn");
                let old_lease=first.lease.clone();let old_binding=first.binding.clone();
                if mode.ends_with("-unconfigured") {unsafe {std::env::remove_var("AUGMENTAGENT_NATIVE_VM_POLICY");}}
                if mode.ends_with("-write-failure") {rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_initial_acceptance BEFORE INSERT ON workflow_event WHEN NEW.kind='owner_acceptance_registered' BEGIN SELECT RAISE(ABORT,'injected initial acceptance write failure'); END;").unwrap();}
                if mode.ends_with("-setup-failure") {rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_initial_handoff BEFORE INSERT ON workflow_native_handoff_pending BEGIN SELECT RAISE(ABORT,'injected initial handoff failure'); END;").unwrap();}
                let private=private_first_files.as_ref().map(|_|acceptance::owner::test_from_prepared(dir.path(),&first,&prepared).unwrap());
                let activated=if let Some(private)=private.as_ref() {
                    first.activate_prepared_owner_acceptance(private,None).await
                }else {first.activate_initial_owner_acceptance(&prepared).await};
                if mode.ends_with("-green-baseline") || mode.ends_with("-write-failure") || mode.ends_with("-wrong-source") || mode.ends_with("-unconfigured") {
                    assert!(activated.is_err());let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();
                    assert_eq!(snapshot.generation,old_lease.generation);assert_eq!(snapshot.checkpoint.binding,old_binding);assert_eq!(snapshot.model_calls,0);
                    assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),control_before);
                    let count:i64=rusqlite::Connection::open(&state).unwrap().query_row("SELECT count(*) FROM workflow_owner_initial_acceptance",[],|r|r.get(0)).unwrap();assert_eq!(count,0);assert!(!store.events(&id).unwrap().iter().any(|(_,kind,_,_)|kind=="owner_acceptance_registered"));return;
                }
                if mode.ends_with("-setup-failure") {
                    assert!(activated.is_err());let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();
                    assert_eq!(snapshot.generation,old_lease.generation+1);assert_eq!(snapshot.checkpoint.binding.contract,prepared.baseline.registration.contract_hash());assert_eq!(snapshot.model_calls,0);
                    assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),control_before);
                    assert!(store.claim_owner_turn(&id,"must-not-replay-initial-owner-work",millis()+LEASE_MS+1,LEASE_MS).unwrap().is_none());
                    assert!(store.native_owner_binding(&id,snapshot.generation).unwrap().is_none());return;
                }
                first=activated.expect("initial acceptance must rebind its owner turn before dispatch");
                first.validate_before_side_effect().unwrap();
                assert_eq!(first.lease.generation,old_lease.generation+1);assert_eq!(first.binding.contract,prepared.baseline.registration.contract_hash());
                assert!(WorkflowStore::open(&state).unwrap().validate_dispatch(&old_lease,&old_binding,millis()).is_err());
                if mode.ends_with("-history-loss") || mode.ends_with("-baseline-tamper") {
                    let db=rusqlite::Connection::open(&state).unwrap();
                    if mode.ends_with("-history-loss") {db.execute("DELETE FROM workflow_owner_initial_acceptance",[]).unwrap();}
                    else {db.execute("UPDATE workflow_owner_initial_acceptance SET receipts=json_set(receipts,'$[0].result','passed')",[]).unwrap();}
                    drop(db);assert!(first.validate_before_side_effect().is_err());assert!(first.owner_turn_native_builder_sessions(augmentagent_channel_core::ProviderKind::Codex).is_err());
                    let mut store=WorkflowStore::open(&state).unwrap();assert!(store.charge(&first.lease,"must-not-admit-initial-corruption",ResourceCharge {model_calls:1,..Default::default()},millis()).is_err());assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),control_before);return;
                }
                if mode.ends_with("-repeat") {
                    let generation=first.lease.generation;
                    assert!(first.activate_initial_owner_acceptance(&prepared).await.is_err(),"registered contract was reinterpreted as raw intake");
                    let store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().generation,generation);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);return;
                }
                if mode.contains("-first-build") {
                    let mut canonical=prepared.baseline.registration.baseline_head().to_owned();
                    if mode.ends_with("-dirty") {std::fs::write(dir.path().join("code.txt"),"owner dirty change").unwrap();}
                    if mode.ends_with("-staged") {std::fs::write(dir.path().join("code.txt"),"owner staged change").unwrap();git_evidence(dir.path(),&["add","code.txt"]).await.unwrap();std::fs::write(dir.path().join("code.txt"),"baseline\n").unwrap();}
                    if mode.ends_with("-leading-space") {std::fs::write(dir.path().join(" code.txt"),"preserve whitespace owner file").unwrap();}
                    if mode.ends_with("-retired-native") {let binding=first.audit.native_store_binding().unwrap().unwrap();augmentagent_channel_core::native_vm::retire_owned_runtimes(&binding,first.audit.path(),&first.audit.native_runtime_owner().unwrap().unwrap(),&||Ok(())).unwrap();}
                    if mode.ends_with("-ignored") {std::fs::write(dir.root.path().join(".git/info/exclude"),"ignored-owner-file\n").unwrap();std::fs::write(dir.path().join("ignored-owner-file"),"preserve owner data").unwrap();}
                    if mode.ends_with("-off-base") {git_evidence(dir.path(),&["commit","--allow-empty","-m","preserve owner commit"]).await.unwrap();}
                    if mode.ends_with("-orphan-main") {canonical=git_evidence(dir.path(),&["commit-tree","HEAD^{tree}","-m","unrelated main"]).await.unwrap();}
                    if mode.ends_with("-grafted-main") {canonical=git_evidence(dir.path(),&["commit-tree","HEAD^{tree}","-m","unrelated main"]).await.unwrap();std::fs::write(dir.root.path().join(".git/info/grafts"),format!("{} {}\n",canonical,prepared.baseline.registration.baseline_head())).unwrap();}
                    if mode.ends_with("-runtime") {
                        let provider=native.join("codex");std::fs::create_dir(&provider).unwrap();std::fs::set_permissions(&provider,std::fs::Permissions::from_mode(0o700)).unwrap();
                        let path=provider.join("session-stray");std::fs::create_dir(&path).unwrap();std::fs::set_permissions(&path,std::fs::Permissions::from_mode(0o700)).unwrap();
                        let owned=path.join("owned.json");std::fs::write(&owned,serde_json::to_vec(&serde_json::json!({"version":1,"owner":first.audit.native_runtime_owner().unwrap().unwrap(),"audit_path":first.audit.path()})).unwrap()).unwrap();std::fs::set_permissions(&owned,std::fs::Permissions::from_mode(0o600)).unwrap();
                        let lock=path.join("lock");std::fs::write(&lock,"").unwrap();std::fs::set_permissions(&lock,std::fs::Permissions::from_mode(0o600)).unwrap();
                    }
                    if mode.ends_with("-legacy-call") {first.store.lock().unwrap().charge(&first.lease,"legacy",ResourceCharge {model_calls:1,..Default::default()},millis()).unwrap();}
                    if mode.ends_with("-workflow-call") || mode.ends_with("-readonly-call") || mode.ends_with("-lost-call-history") || mode.ends_with("-counter-loss") {
                        first.store.lock().unwrap().charge_admitted_model(&first.lease,"synthetic-classified-admission",millis(),None,None,Some(durable::submission::Provider::Codex),!mode.ends_with("-workflow-call")).unwrap();
                        if mode.ends_with("-lost-call-history") {rusqlite::Connection::open(&state).unwrap().execute("DELETE FROM workflow_event WHERE kind='native_model_admitted'",[]).unwrap();}
                        if mode.ends_with("-counter-loss") {rusqlite::Connection::open(&state).unwrap().execute("UPDATE workflow SET model_calls=0 WHERE id=?1",[&id]).unwrap();}
                    }
                    if mode.ends_with("-policy-lost") {unsafe {std::env::remove_var("AUGMENTAGENT_NATIVE_VM_POLICY");}}
                    let before=WorkflowStore::open(&state).unwrap().snapshot(&id).unwrap().unwrap();let original=first.audit.native_store_binding().unwrap().unwrap();
                    let result=first.reserve_prepared_first_owner_body(private.as_ref().unwrap(),durable::submission::Provider::Codex,&canonical).await;
                    let allowed=mode.ends_with("-first-build") || mode.ends_with("-readonly-call") || mode.ends_with("-new-verifier");
                    assert_eq!(result.is_ok(),allowed,"first owner body decision: {mode}: {:?}",result.as_ref().err());
                    if allowed {let dispatch=result.unwrap();first.store.lock().unwrap().validate_owner_body_dispatch(&dispatch,millis()).unwrap();assert!(first.reserve_prepared_first_owner_body(private.as_ref().unwrap(),durable::submission::Provider::Codex,&canonical).await.is_err(),"first body was reserved twice");}
                    let mut store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert!(!store.owner_control(&id).unwrap().unwrap().paused);
                    assert_eq!(first.audit.native_store_binding().unwrap(),Some(original));
                    let count:i64=rusqlite::Connection::open(&state).unwrap().query_row("SELECT count(*) FROM workflow_owner_body_dispatch WHERE workflow=?1",[&id],|r|r.get(0)).unwrap();assert_eq!(count,i64::from(allowed));
                    if mode.ends_with("-staged") {assert_eq!(git_evidence(dir.path(),&["show",":code.txt"]).await.unwrap(),"owner staged change");}
                    if mode.ends_with("-ignored") {assert_eq!(std::fs::read_to_string(dir.path().join("ignored-owner-file")).unwrap(),"preserve owner data");}
                    return;
                }

            }
            first.validate_before_side_effect().unwrap();
            let sessions=first.owner_turn_native_builder_sessions(augmentagent_channel_core::ProviderKind::Codex).unwrap();
            sessions.scope(async {let session=augmentagent_channel_core::native_session::current_for(augmentagent_channel_core::ProviderKind::Codex).unwrap().unwrap();let turn=session.begin(augmentagent_channel_core::ProviderKind::Codex).unwrap();assert!(matches!(turn.launch(),augmentagent_channel_core::native_session::Launch::Create {..}));}).await;
            let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();
            assert!(store.native_owner_binding(&id,snapshot.generation).unwrap().is_some());assert_eq!(snapshot.model_calls,0);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),control_before);
            assert!(store.claim_productive(&id,"must-not-unpause",millis(),LEASE_MS).unwrap().is_none());assert!(store.begin_operation(&first.lease,OperationKind::Push,first.binding(),millis()).is_err());assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
            drop(store);first.finish().unwrap();return;
        }
        let result=WorkflowGuard::acquire_at(&state,start(),millis()).await;
        if matches!(mode.as_str(),"missing-policy"|"legacy-owner"|"legacy-call"|"legacy-workflow"|"physical-prior-state") {
            assert!(result.is_err(),"registration should have refused: {mode}");
            let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();
            assert_eq!(snapshot.lease_until_ms,0,"unstarted admission retained its lease: {mode}");
            assert_eq!(snapshot.model_calls,if mode=="legacy-call" {1} else {0});
            assert!(store.native_owner_binding(&id,snapshot.generation).unwrap().is_none());
            assert!(store.pending_native_retirements(millis(),16).unwrap().is_empty(),"refusal retired an older owner");
            println!("native registration guard case passed: {mode}");return;
        }
        let mut guard=result.unwrap().unwrap();
        if matches!(mode.as_str(),"unconfigured"|"configuration-recovery") {
            assert!(guard.audit_logger().native_store_binding().unwrap().is_none());
            assert!(guard.fresh_native_builder_sessions().is_err(),"builder scope accepted a missing original native store binding");
            assert!(guard.store.lock().unwrap().initial_native_owner_available(guard.id()).unwrap(),"missing configuration created an unbound native owner");
            guard.finish().unwrap();
            if mode=="unconfigured" {println!("native registration guard case passed: {mode}");return;}
            // This subprocess runs one test only; no process-global env race.
            unsafe {std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",&policy);}
            guard=WorkflowGuard::acquire_at(&state,start(),millis()).await.unwrap().unwrap();
        }
        let saved=guard.store.lock().unwrap().native_owner_binding(guard.id(),guard.lease.generation).unwrap();
        assert!(saved.is_some(),"fresh guard returned without its original native store binding");
        assert_eq!(guard.audit_logger().native_store_binding().unwrap(),saved);
        assert_eq!(guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        guard.audit_logger().validate_native_store_binding().unwrap();
        assert!(guard.fresh_native_builder_sessions().is_ok(),"bound builder scope was refused");
        if mode=="fresh" {
            let audit=guard.audit_logger();
            let original=saved.clone().unwrap();
            let intent=augmentagent_channel_core::native_vm::PreparedLaunchIntent {version:2,
                original_store_binding_sha256:format!("{:x}",Sha256::digest(augmentagent_channel_core::native_vm::ownership::canonical_bytes(&serde_json::to_value(original).unwrap()).unwrap())),
                intent_id:uuid::Uuid::new_v4().to_string(),source_owner:augmentagent_channel_core::native_vm::RuntimeOwner {
                    workflow_id:guard.id().into(),lease_generation:guard.lease.generation},source_audit_path:audit.path().into(),
                provider:"codex".into(),runtime_relative_path:"codex/session-unsubmitted".into(),
                native_session_id:Some("requested-only".into()),launch_sha256:"a".repeat(64),creation_receipt_sha256:"b".repeat(64)};
            let prepared=audit.native_launch_prepared(&intent).unwrap();
            let fact=augmentagent_channel_core::native_vm::ownership::observe_native_uncertainty(&audit,&prepared,None).unwrap();
            let rows=guard.store.lock().unwrap().native_observation_history(guard.id()).unwrap();
            assert_eq!(rows.len(),1,"the actual guard did not consume native outcome observations");
            assert!(rows[0].native_session_id.is_none(),"a requested ID became an actual provider observation");
            assert_eq!(rows[0].certainty,crate::autoship_workflow::native_observation::Certainty::Uncertain);
            assert_eq!(rows[0].protected_observation_reference.as_ref(),Some(fact.receipt()));
            assert_eq!(guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        }
        if mode.starts_with("owner-setup-") {
            let session_case=mode.ends_with("-session");
            let healthy=mode=="owner-setup-healthy" || session_case;
            let provider=if mode.contains("-claude-") {augmentagent_channel_core::ProviderKind::Claude}else{augmentagent_channel_core::ProviderKind::Codex};
            #[cfg(target_os="linux")]
            if session_case {
                let (prepared,session,resource)=durable::native_observation::tests::retained_launch_fixture(
                    &native,&guard.audit,provider.name(),|_|{});
                let fact=augmentagent_channel_core::native_vm::ownership::observe_native_completion(
                    &guard.audit,&prepared,&session,&resource).unwrap();
                assert_eq!(fact.native_session_id(),Some("actual-session"));
                let lock=native.join(format!("{}/session-fixture/lock",provider.name()));
                std::fs::write(&lock,b"").unwrap();std::fs::set_permissions(&lock,std::fs::Permissions::from_mode(0o600)).unwrap();
                // Saved provider-parser projection of the protected fixture
                // completion above. No live provider or model admission.
                guard.store.lock().unwrap().record_builder_conversations(
                    &guard.lease,&guard.binding,&[augmentagent_channel_core::native_session::ObservedConversation {
                        provider,id:fact.native_session_id().unwrap().into(),uncertain:false,
                    }],millis()).unwrap();
            }
            let original=saved.clone().unwrap();guard.finish().unwrap();let mut store=WorkflowStore::open(&state).unwrap();
            let origin=durable::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:45};let reply=store.enqueue_owner_message(&origin,&id,"Synthetic owner clarification",None,millis()).unwrap();assert!(reply.accepted);let control_before=store.owner_control(&id).unwrap().unwrap();let generation_before=store.snapshot(&id).unwrap().unwrap().generation;let policy_bytes=std::fs::read(&policy).unwrap();drop(store);
            if mode=="owner-setup-unconfigured" {unsafe{std::env::remove_var("AUGMENTAGENT_NATIVE_VM_POLICY");}}
            else if !healthy {std::fs::remove_file(&policy).unwrap();}
            if mode=="owner-setup-write-failure" {rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_owner_setup_refusal BEFORE INSERT ON workflow_owner_turn_setup_refusal BEGIN SELECT RAISE(ABORT,'injected owner setup refusal failure'); END;").unwrap();}
            let result=WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await;
            if mode=="owner-setup-unconfigured" {assert!(result.is_err());let store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().generation,generation_before);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),Some(control_before));return;}
            let mut recovered=if healthy {result.unwrap().expect("guarded native owner handoff must acquire the paused owner turn")}else{
                assert!(result.is_err());let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();assert_eq!(snapshot.model_calls,0);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),Some(control_before.clone()));assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
                let row_state:String=rusqlite::Connection::open(&state).unwrap().query_row("SELECT state FROM workflow_owner_turn WHERE request_id=?1",[&reply.request_id],|r|r.get(0)).unwrap();
                if mode=="owner-setup-write-failure" {assert_eq!(row_state,"claimed");assert!(snapshot.lease_until_ms>0);rusqlite::Connection::open(&state).unwrap().execute_batch("DROP TRIGGER deny_owner_setup_refusal").unwrap();assert!(store.claim_owner_turn(&id,"must-not-replay",millis()+LEASE_MS+1,LEASE_MS).unwrap().is_none());return;}
                assert_eq!(row_state,"setup_refused");assert_eq!(snapshot.lease_until_ms,0);assert!(store.native_owner_binding(&id,snapshot.generation).unwrap().is_none());drop(store);std::fs::write(&policy,&policy_bytes).unwrap();std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
                if matches!(mode.as_str(),"owner-setup-history-loss"|"owner-setup-index-corruption") {
                    let db=rusqlite::Connection::open(&state).unwrap();
                    if mode=="owner-setup-history-loss" {db.execute("DELETE FROM workflow_owner_turn_setup_refusal WHERE workflow=?1",[&id]).unwrap();}
                    else {db.execute("UPDATE workflow_owner_turn SET binding=json_set(binding,'$.unexpected_scope','unrecognized') WHERE workflow=?1",[&id]).unwrap();}
                    drop(db);assert!(WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.is_err(),"corrupt owner setup authority must remain held");assert_eq!(WorkflowStore::open(&state).unwrap().pending_owner_messages(&id).unwrap(),1);return;
                }
                WorkflowGuard::acquire_mode_at(&state,start(),millis(),true).await.unwrap().expect("same unstarted owner request must retry after configuration recovery")
            };
            if let Some((previous,next))=&revision_contracts {
                let old_lease=recovered.lease.clone();let old_binding=recovered.binding.clone();
                let old_native=recovered.audit.native_runtime_owner().unwrap().unwrap();
                if mode.contains("-revision-unconfigured-") {
                    unsafe {std::env::remove_var("AUGMENTAGENT_NATIVE_VM_POLICY");}
                    assert!(recovered.activate_owner_revision(previous,next).await.is_err(),"owner revision returned a guard without native configuration");
                    let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();
                    assert_eq!(snapshot.generation,old_lease.generation);assert_eq!(snapshot.checkpoint.binding,old_binding);
                    assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert!(!store.owner_control(&id).unwrap().unwrap().paused);
                    assert!(!store.events(&id).unwrap().iter().any(|(_,kind,_,_)|kind=="owner_contract_revised"));return;
                }
                if mode.contains("-setup-failure-") {
                    let before=recovered.store.lock().unwrap().snapshot(&id).unwrap().unwrap();
                    let input_before=WorkflowStore::open(&state).unwrap().owner_control(&id).unwrap();
                    rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_owner_revision_handoff BEFORE INSERT ON workflow_native_handoff_pending BEGIN SELECT RAISE(ABORT,'injected revision handoff failure'); END;").unwrap();
                    assert!(recovered.activate_owner_revision(previous,next).await.is_err());
                    let mut store=WorkflowStore::open(&state).unwrap();let after=store.snapshot(&id).unwrap().unwrap();
                    assert_eq!(after.generation,old_lease.generation+1);assert_eq!(after.checkpoint.binding.contract,next.contract_hash());
                    assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.attempts,before.attempts);
                    assert_eq!(store.owner_control(&id).unwrap(),input_before);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);
                    assert!(store.validate_dispatch(&old_lease,&old_binding,millis()).is_err());
                    let (turn_state,turn_generation):(String,i64)=rusqlite::Connection::open(&state).unwrap().query_row("SELECT state,lease_generation FROM workflow_owner_turn WHERE request_id=?1",[&reply.request_id],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
                    assert_eq!(turn_state,"claimed");assert_eq!(turn_generation,after.generation);
                    assert!(store.native_owner_binding(&id,after.generation).unwrap().is_none());
                    assert!(store.claim_owner_turn(&id,"must-not-repeat-owner-work",millis()+LEASE_MS+1,LEASE_MS).unwrap().is_none());
                    assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);return;
                }
                recovered=recovered.activate_owner_revision(previous,next).await.expect("owner revision must return a guard for the revised generation");
                assert_eq!(recovered.lease.generation,old_lease.generation+1);
                assert_eq!(recovered.binding.contract,next.contract_hash());assert_eq!(recovered.binding.head,old_binding.head);
                recovered.validate_before_side_effect().unwrap();
                assert_ne!(recovered.audit.native_runtime_owner().unwrap().unwrap(),old_native);
                let mut store=recovered.store.lock().unwrap();
                assert!(store.validate_dispatch(&old_lease,&old_binding,millis()).is_err(),"old callbacks survived owner contract activation");
                assert_eq!(store.owner_turn_for_lease(&recovered.lease,millis()).unwrap().request_id,reply.request_id);
                assert_eq!(store.merge_allowance(millis(),5).unwrap().confirmed,0);
                assert!(store.current_review_history(&id).unwrap().is_empty());
            }
            if session_case {
                // A new owner guard has reopened the workflow and transferred
                // native authority. It must resume the actual observed ID.
                let sessions=recovered.owner_turn_native_builder_sessions(provider).unwrap();
                sessions.scope(async {
                    let session=augmentagent_channel_core::native_session::current_for(provider).unwrap().unwrap();
                    let turn=session.begin(provider).unwrap();
                    assert_eq!(turn.launch(),augmentagent_channel_core::native_session::Launch::Resume {id:"actual-session".into()});
                }).await;
                let other=if provider==augmentagent_channel_core::ProviderKind::Codex {augmentagent_channel_core::ProviderKind::Claude}else{augmentagent_channel_core::ProviderKind::Codex};
                assert!(recovered.owner_turn_native_builder_sessions(other).is_err(),"owner restoration changed the requested provider");
                let uncertain=augmentagent_channel_core::native_session::ObservedConversation {provider,id:"actual-session".into(),uncertain:true};
                recovered.store.lock().unwrap().record_builder_conversations(&recovered.lease,&recovered.binding,&[uncertain],millis()).unwrap();
                assert!(recovered.owner_turn_native_builder_sessions(provider).is_err(),"uncertain conversation was resumed");
            }
            recovered.validate_before_side_effect().unwrap();assert_eq!(recovered.audit.native_store_binding().unwrap(),Some(original));let mut store=recovered.store.lock().unwrap();let turn=store.owner_turn_for_lease(&recovered.lease,millis()).unwrap();assert_eq!(turn.request_id,reply.request_id);assert_eq!(store.pending_owner_messages(&id).unwrap(),1);assert_eq!(store.owner_control(&id).unwrap(),Some(control_before));assert!(store.begin_operation(&recovered.lease,OperationKind::Push,&recovered.binding,millis()).is_err());assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);drop(store);recovered.finish().unwrap();assert!(WorkflowStore::open(&state).unwrap().claim_owner_turn(&id,"published-turn-must-not-replay",millis()+LEASE_MS+1,LEASE_MS).unwrap().is_none());return;
        }
        if mode=="terminal-abort-cleanup" {
            use augmentagent_channel_core::native_vm::ownership;
            let source=guard.audit_logger();let original=saved.clone().unwrap();guard.finish().unwrap();let mut store=WorkflowStore::open(&state).unwrap();let lease=store.claim(&id,"fixture-terminal-transfer",millis(),LEASE_MS).unwrap().unwrap();let binding=store.snapshot(&id).unwrap().unwrap().checkpoint.binding;
            let destination=open_audit_generation(&state,&id,None,lease.generation).unwrap();destination.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:id.clone(),lease_generation:lease.generation}).unwrap();destination.set_native_store_binding(original.clone()).unwrap();destination.require_run_resources().unwrap();destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":id,"generation":lease.generation})).unwrap();
            let pending=match store.prepare_or_load_native_handoff(&lease,&binding,destination.path().to_str().unwrap(),millis()).unwrap() {durable::HandoffPreparation::Prepared(p)=>p,_=>panic!("expected exact fixture intent")};let writer=source.begin_write().unwrap();assert!(matches!(ownership::transfer(&original,pending.request(),&pending.observation(),&[source],&destination,&||Ok(())),ownership::TransferOutcome::Deferred {..}));writer.complete();
            // Disposable fixture retirement creates actual Aborted native state.
            let old=&pending.request().source_owners[0];augmentagent_channel_core::native_vm::retire_owned_runtimes(&original,Path::new(&old.audit_path),&old.owner,&||Ok(())).unwrap();let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;checkpoint.stage=Stage::Retired;store.checkpoint(&lease,&checkpoint,millis()).unwrap();let before=store.snapshot(&id).unwrap().unwrap();let allowance=store.merge_allowance(millis(),5).unwrap();let epoch=std::fs::read(native.join("authority-epoch.json")).unwrap();drop(store);
            assert!(reconcile_runtime_retirements_at(&state).unwrap()>0);let mut store=WorkflowStore::open(&state).unwrap();let count:i64=rusqlite::Connection::open(&state).unwrap().query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&id],|r|r.get(0)).unwrap();assert_eq!(count,0,"worker must reconcile original terminal pending work");assert!(store.unresolved_native_abort_namespaces(&id).unwrap().is_empty());assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);assert_eq!(store.merge_allowance(millis(),5).unwrap(),allowance);assert!(store.native_owner_binding(&id,lease.generation).unwrap().is_none());let events=store.events(&id).unwrap();drop(store);assert_eq!(std::fs::read(native.join("authority-epoch.json")).unwrap(),epoch);assert_eq!(reconcile_runtime_retirements_at(&state).unwrap(),0);assert_eq!(WorkflowStore::open(&state).unwrap().events(&id).unwrap(),events);return;
        }
        if matches!(mode.as_str(),"resolved-abort-acquisition"|"closed-recovery-rollback"|"closed-recovery-history-loss") {
            use augmentagent_channel_core::native_vm::ownership;
            let source=guard.audit_logger();let original=saved.clone().unwrap();guard.finish().unwrap();
            let mut store=WorkflowStore::open(&state).unwrap();let lease=store.claim(&id,"fixture-interrupted-transfer",millis(),LEASE_MS).unwrap().unwrap();let binding=store.snapshot(&id).unwrap().unwrap().checkpoint.binding;
            let destination=open_audit_generation(&state,&id,None,lease.generation).unwrap();destination.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {workflow_id:id.clone(),lease_generation:lease.generation}).unwrap();destination.set_native_store_binding(original.clone()).unwrap();destination.require_run_resources().unwrap();destination.record_controller(serde_json::json!({"actor":"controller","kind":"workflow_claimed","workflow":id,"generation":lease.generation})).unwrap();
            let pending=match store.prepare_or_load_native_handoff(&lease,&binding,destination.path().to_str().unwrap(),millis()).unwrap() {durable::HandoffPreparation::Prepared(p)=>p,_=>panic!("expected exact fixture intent")};
            let writer=source.begin_write().unwrap();let outcome=ownership::transfer(&original,pending.request(),&pending.observation(),&[source.clone()],&destination,&||Ok(()));assert!(matches!(outcome,ownership::TransferOutcome::Deferred {..}),"{outcome:?}");writer.complete();
            // Disposable fixture cleanup; this does not create production owner authorization.
            for authority in pending.request().source_owners.iter().chain(std::iter::once(&pending.request().destination)) {augmentagent_channel_core::native_vm::retire_owned_runtimes(&original,Path::new(&authority.audit_path),&authority.owner,&||Ok(())).unwrap();}
            store.release(&lease,millis()).unwrap();let recovery=store.claim(&id,"fixture-abort-ack",millis(),LEASE_MS).unwrap().unwrap();
            ownership::with_transfer_inspection(&original,pending.request(),&pending.observation(),&||Ok(()),&mut |proof|store.acknowledge_native_aborted(&recovery,&binding,&pending,proof,millis())).unwrap();store.release(&recovery,millis()).unwrap();assert_eq!(store.unresolved_native_abort_namespaces(&id).unwrap().len(),2);let calls=store.snapshot(&id).unwrap().unwrap().model_calls;drop(store);
            let epoch=std::fs::read(native.join("authority-epoch.json")).unwrap();
            if mode=="closed-recovery-rollback" {
                rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_closed_recovery BEFORE INSERT ON workflow_native_recovery BEGIN SELECT RAISE(ABORT,'injected closed recovery failure'); END;").unwrap();
                assert!(WorkflowGuard::acquire_at(&state,start(),millis()).await.is_err());let store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();assert_eq!(snapshot.lease_until_ms,0);assert_eq!(snapshot.model_calls,calls);assert!(store.native_owner_binding(&id,snapshot.generation).unwrap().is_none());assert_eq!(store.audit_generation(&id).unwrap(),0);drop(store);
                rusqlite::Connection::open(&state).unwrap().execute_batch("DROP TRIGGER deny_closed_recovery").unwrap();
            }
            let recovered=WorkflowGuard::acquire_at(&state,start(),millis()).await.unwrap().unwrap();recovered.validate_before_side_effect().unwrap();assert!(recovered.fresh_native_builder_sessions().is_ok());let mut store=recovered.store.lock().unwrap();assert!(store.unresolved_native_abort_namespaces(&id).unwrap().is_empty());let snapshot=store.snapshot(&id).unwrap().unwrap();assert_eq!(snapshot.model_calls,calls);assert_eq!(snapshot.checkpoint.binding,binding);assert_eq!(store.native_owner_binding(&id,snapshot.generation).unwrap(),Some(original));assert!(!store.initial_native_owner_available(&id).unwrap());assert_eq!(store.audit_generation(&id).unwrap(),snapshot.generation);assert_eq!(store.native_aborted_handoff(&id,&pending.request().transfer_id).unwrap(),Some(pending));assert_eq!(std::fs::read(native.join("authority-epoch.json")).unwrap(),epoch);
            if mode=="closed-recovery-history-loss" {store.validate_dispatch(&recovered.lease,&binding,millis()).unwrap();rusqlite::Connection::open(&state).unwrap().execute("DELETE FROM workflow_native_recovery WHERE workflow=?1",[&id]).unwrap();assert!(store.validate_dispatch(&recovered.lease,&binding,millis()).is_err());assert!(store.initial_native_owner_available(&id).is_err());let events=store.events(&id).unwrap();assert!(store.begin_operation(&recovered.lease,OperationKind::CreatePr,&binding,millis()).is_err());assert_eq!(store.events(&id).unwrap(),events);drop(store);assert!(recovered.validate_before_side_effect().is_err());return;}
            drop(store);recovered.finish().unwrap();let mut store=WorkflowStore::open(&state).unwrap();assert!(!store.initial_native_owner_available(&id).unwrap());assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,calls);return;
        }
        if matches!(mode.as_str(),"native-sweep"|"native-sweep-lost-authority") {
            guard.record_review("fixture-independent-review",true,"native sweep fixture approval").unwrap();
            let head=guard.binding.head.clone();let original=saved.clone().unwrap();let old_audit=guard.audit.path().to_path_buf();
            let reviews=guard.store.lock().unwrap().current_review_history(&id).unwrap();guard.finish().unwrap();
            if mode=="native-sweep-lost-authority" {std::fs::remove_file(native.join("authority-epoch.json")).unwrap();}
            let result=SweepGuard::acquire_at(&state,"owner/repo",77,42,"fixture",&head,"untrusted caller approval",millis());
            if mode=="native-sweep-lost-authority" {
                assert!(result.is_err(),"sweep must refuse missing native authority");assert!(!native.join("authority-epoch.json").exists());
                let store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().lease_until_ms,0);assert_eq!(store.current_review_history(&id).unwrap(),reviews);return;
            }
            let sweep=result.unwrap().unwrap();let inner=sweep.inner.as_ref().unwrap();
            assert_ne!(inner.audit.path(),old_audit);assert_eq!(inner.audit.native_store_binding().unwrap(),Some(original));inner.validate_before_side_effect().unwrap();
            let store=inner.store.lock().unwrap();assert!(store.native_owner_context(&id,inner.lease.generation).unwrap().is_some());assert_eq!(store.audit_generation(&id).unwrap(),inner.lease.generation);assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);assert_eq!(store.current_review_history(&id).unwrap(),reviews);drop(store);drop(sweep);return;
        }
        if matches!(mode.as_str(),"healthy-handoff"|"lost-source-audit"|"pending-settlement-recovery"|"promotion-failure-recovery"|"lost-native-authority") {
            let old_generation=guard.lease.generation;let old_audit=guard.audit_logger().path().to_path_buf();let old_binding=saved.clone().unwrap();let old_checkpoint=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap().checkpoint;guard.finish().unwrap();
            let key=format!("{:x}",Sha256::digest(id.as_bytes()));let old_anchor=control.join("autoship-audit/anchors").join(format!("{key}.json"));if mode=="lost-source-audit" {std::fs::remove_file(&old_anchor).unwrap();}
            if mode=="pending-settlement-recovery" {rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_guard_settlement BEFORE INSERT ON workflow_native_handoff_completed BEGIN SELECT RAISE(ABORT,'injected guard settlement failure'); END;").unwrap();}
            if mode=="promotion-failure-recovery" {rusqlite::Connection::open(&state).unwrap().execute_batch("CREATE TRIGGER deny_guard_promotion BEFORE INSERT ON workflow_audit_generation BEGIN SELECT RAISE(ABORT,'injected guard promotion failure'); END;").unwrap();}
            if mode=="lost-native-authority" {std::fs::remove_file(native.join("authority-epoch.json")).unwrap();}
            let result=WorkflowGuard::acquire_at(&state,start(),millis()).await;
            if mode=="lost-native-authority" {assert!(result.is_err());assert!(!native.join("authority-epoch.json").exists());let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();assert_eq!(snapshot.lease_until_ms,0);assert_eq!(snapshot.model_calls,0);assert!(store.native_owner_binding(&id,snapshot.generation).unwrap().is_none());assert_eq!(store.audit_generation(&id).unwrap(),0);assert!(store.pending_native_retirements(millis(),16).unwrap().is_empty());return;}
            if matches!(mode.as_str(),"pending-settlement-recovery"|"promotion-failure-recovery") {
                assert!(result.is_err());let mut store=WorkflowStore::open(&state).unwrap();let snapshot=store.snapshot(&id).unwrap().unwrap();assert_eq!(snapshot.lease_until_ms,0);assert_eq!(snapshot.model_calls,0);assert_eq!(store.audit_generation(&id).unwrap(),0);
                let pending:i64=rusqlite::Connection::open(&state).unwrap().query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&id],|r|r.get(0)).unwrap();assert_eq!(pending,if mode=="pending-settlement-recovery" {1}else{0});
                rusqlite::Connection::open(&state).unwrap().execute_batch(if mode=="pending-settlement-recovery" {"DROP TRIGGER deny_guard_settlement"}else{"DROP TRIGGER deny_guard_promotion"}).unwrap();drop(store);
                let recovered=WorkflowGuard::acquire_at(&state,start(),millis()).await.unwrap().unwrap();recovered.validate_before_side_effect().unwrap();assert_eq!(recovered.audit_logger().native_store_binding().unwrap(),Some(old_binding));let mut store=recovered.store.lock().unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);assert_eq!(store.audit_generation(&id).unwrap(),recovered.lease.generation);let pending:i64=rusqlite::Connection::open(&state).unwrap().query_row("SELECT count(*) FROM workflow_native_handoff_pending WHERE workflow=?1",[&id],|r|r.get(0)).unwrap();assert_eq!(pending,0);assert!(store.pending_native_retirements(millis(),16).unwrap().is_empty());drop(store);recovered.finish().unwrap();return;
            }
            if mode=="lost-source-audit" {assert!(result.is_err());assert!(!old_anchor.exists(),"missing source authority was recreated");let mut store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().lease_until_ms,0);assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);assert_eq!(store.audit_generation(&id).unwrap(),0);assert!(store.pending_native_retirements(millis(),16).unwrap().is_empty());return;}
            let next=result.unwrap().unwrap();assert!(next.lease.generation>old_generation);assert_ne!(next.audit_logger().path(),old_audit);assert_eq!(next.audit_logger().native_store_binding().unwrap(),Some(old_binding));next.validate_before_side_effect().unwrap();let mut store=next.store.lock().unwrap();assert_eq!(store.audit_generation(&id).unwrap(),next.lease.generation);assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint,old_checkpoint);assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);assert!(store.pending_native_retirements(millis(),16).unwrap().is_empty());drop(store);next.finish().unwrap();
            let third=WorkflowGuard::acquire_at(&state,start(),millis()).await.unwrap().unwrap();assert_eq!(third.store.lock().unwrap().audit_generation(&id).unwrap(),third.lease.generation);third.validate_before_side_effect().unwrap();third.finish().unwrap();return;
        }
        guard.finish().unwrap();println!("native registration guard case passed: {mode}");
    }
    #[tokio::test]
    async fn workflow_guard_installs_native_launch_journal_before_provider_binding() {
        let dir=repo();let control=control_dir();let path=control.path().join("workflow.db");
        let guard=WorkflowGuard::acquire_at(&path,Start{repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",
            source_issue:Some(77),pr:None,branch:"fixture",contract:"C1",environment:"test"},millis()).await.unwrap().unwrap();
        assert!(guard.audit_logger().set_native_launch_observer(Arc::new(|_|Ok(()))).is_err(),
            "guard returned without installing its native launch journal");
        assert!(guard.store.lock().unwrap().native_owner_binding(guard.id(),guard.lease.generation).unwrap().is_none(),
            "legacy owner must not be backfilled from current policy");
        // Metadata fixture only: call the same protected preparation boundary
        // as NativeVM, without inventing a real runtime or submitting a prompt.
        let audit=guard.audit_logger();
        let prepared=augmentagent_channel_core::native_vm::PreparedLaunchIntent {version:2,original_store_binding_sha256:"a".repeat(64),
            intent_id:uuid::Uuid::new_v4().to_string(),source_owner:augmentagent_channel_core::native_vm::RuntimeOwner {
                workflow_id:guard.id().into(),lease_generation:guard.lease.generation},
            source_audit_path:audit.path().into(),provider:"codex".into(),runtime_relative_path:"codex/session-fixture".into(),
            native_session_id:None,launch_sha256:"a".repeat(64),creation_receipt_sha256:"b".repeat(64)};
        let error=audit.native_launch_prepared(&prepared).unwrap_err();
        assert!(error.to_string().contains("original") && error.to_string().contains("binding"),"{error:#}");
        let mut store=guard.store.lock().unwrap();
        assert!(store.native_launch_intents(guard.id()).unwrap().is_empty());
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,0);
        drop(store);
        guard.validate_before_side_effect().unwrap();
        let weak=Arc::downgrade(&audit);drop(audit);guard.finish().unwrap();
        assert!(weak.upgrade().is_none(),"native launch callback retained the logger after the guard finished");
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
        let store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,2);
        let progress=store.progress_events(guard.id()).unwrap();
        assert_eq!(progress.last().unwrap().requested_provider,None);
        assert_eq!(progress.last().unwrap().actual_provider.as_deref(),Some("codex"),"native admission must project the provider that actually ran, independently of the requested pin");
    }

    #[tokio::test]
    async fn ci_gate_reports_meaningful_success_only_after_a_passing_result() {
        for disposition in ["passed","hosted-pending","source-failed","infrastructure-failed"] {
            let directory=repo();let state=control_dir();let path=state.path().join("workflow.db");
            let mut guard=WorkflowGuard::acquire_at(&path,Start {
                repo_root:directory.path(),worktree:directory.path(),repository:"owner/repo",
                source_issue:Some(77),pr:Some(88),branch:"fixture",contract:"C1",environment:"rust-a"
            },millis()).await.unwrap().unwrap();
            guard.checkpoint(Stage::Review,"candidate_ready","run_verifier",0,&["receipt".into()]).await.unwrap();
            let before=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
            let result=match disposition {
                "passed"=>Ok(()),
                "hosted-pending"=>Err(hosted_ci::AwaitingHostedCi.into()),
                "source-failed"=>Err(anyhow::anyhow!("error[E0308]: mismatched types")),
                _=>Err(anyhow::anyhow!("shared CI prerequisite unavailable: network offline")),
            };
            let outcome=CiGateResult(result).for_workflow(&mut guard).await;
            match disposition {
                "passed" | "hosted-pending"=>assert!(matches!(outcome,Ok(Ok(())))),
                "source-failed"=>assert!(matches!(outcome,Ok(Err(_)))),
                _=>assert!(outcome.unwrap_err().downcast_ref::<CiInfrastructureDeferred>().is_some()),
            }
            let store=WorkflowStore::open(&path).unwrap();
            let passed:Vec<_>=store.events(guard.id()).unwrap().into_iter()
                .filter(|(_,kind,_,_)|kind=="verification_gate_passed").collect();
            assert_eq!(passed.len(),usize::from(disposition=="passed"));
            if let Some((_,_,raw,_))=passed.first() {
                let value:serde_json::Value=serde_json::from_str(raw).unwrap();
                assert_eq!(value["domain"],"ci");assert_eq!(value["binding"],serde_json::to_value(&before.checkpoint.binding).unwrap());
            }
            let after=store.snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.attempts,before.attempts);
            assert_eq!(after.progress_at_ms,before.progress_at_ms);
            assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
            drop(store);guard.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn ci_gate_consumption_defers_revisions_before_repair_and_preserves_paid_accounting() {
        for stage in [Stage::Build, Stage::Review, Stage::Reconcile] {
            for (infrastructure, already_paid) in [(true, false), (true, true), (false, false)] {
                let dir = repo();
                let state = control_dir();
                let path = state.path().join("workflow.db");
                let mut guard = WorkflowGuard::acquire_at(&path, Start {
                    repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo",
                    source_issue: Some(77), pr: Some(88), branch: "fixture", contract: "C1",
                    environment: "rust-a",
                }, millis()).await.unwrap().unwrap();
                guard.checkpoint(stage, "candidate_ready", "run_verifier", 0,
                    &["protected-receipt".into()]).await.unwrap();
                let accounting = guard.capacity_accounting(true).unwrap();
                if already_paid { guard.audit_logger().model_call_admitted("codex").unwrap(); }
                let before = WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
                let allowance = WorkflowStore::open(&path).unwrap().merge_allowance(millis(), 5).unwrap();
                let mut repairs = 0;
                let outcome: Result<RunReport> = async {
                    let error = if infrastructure {
                        anyhow::anyhow!("shared CI prerequisite unavailable: dependency network offline")
                    } else {
                        anyhow::anyhow!("error[E0308]: mismatched types")
                    };
                    if let Err(_code_error) = CiGateResult(Err(error)).for_workflow(&mut guard).await? {
                        // This is the paid code-repair branch used after a gate.
                        repairs += 1;
                        guard.audit_logger().model_call_admitted("codex")?;
                    }
                    Ok(RunReport::built("code repaired".into()))
                }.await;
                let report = accounting.settle_ci(outcome).unwrap();
                let after = WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
                assert_eq!(repairs, usize::from(!infrastructure));
                assert_eq!(after.model_calls, before.model_calls + u64::from(!infrastructure));
                assert_eq!(after.checkpoint.stage, stage);
                assert_eq!(after.checkpoint.binding, before.checkpoint.binding);
                assert_eq!(after.checkpoint.evidence, before.checkpoint.evidence);
                assert_eq!(after.progress_at_ms, before.progress_at_ms);
                assert_eq!(after.attempts, before.attempts);
                assert_eq!(WorkflowStore::open(&path).unwrap().merge_allowance(millis(), 5).unwrap(), allowance);
                if infrastructure {
                    assert_eq!(after.checkpoint.next.action, "retry_verification");
                    assert!(after.checkpoint.next.eligible_at_ms > millis());
                    assert_eq!(report.billed, already_paid);
                    assert!(report.is_idle(), "the command attempt must be deferred even when earlier work spent model calls");
                    assert!(report.resumed);
                    assert!(after.lease_until_ms <= millis(), "CI retry still waits for the abandoned ten-minute lease");
                    let eligible = after.checkpoint.next.eligible_at_ms;
                    let mut store = WorkflowStore::open(&path).unwrap();
                    assert!(store.claim_productive(guard.id(), "next-worker", eligible - 1, 60_000).unwrap().is_none());
                    assert!(store.claim_productive(guard.id(), "next-worker", eligible, 60_000).unwrap().is_some(),
                        "the persisted retry must run at five minutes, before the old ten-minute lease expires");
                } else {
                    assert_eq!(after.checkpoint.next, before.checkpoint.next);
                    assert!(report.billed);
                }
                guard.finish().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn verification_infrastructure_defers_without_repair_calls_or_permanent_hold() {
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
        let mut guard=WorkflowGuard::acquire_at(&path,Start{repo_root:dir.path(),worktree:dir.path(),
            repository:"owner/repo",source_issue:Some(77),pr:Some(88),branch:"fixture",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
        guard.checkpoint(Stage::Verify,"verification_started","run_verifier",0,&["receipt".into()]).await.unwrap();
        let before=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        let accounting=guard.capacity_accounting(true).unwrap();
        let code_error=anyhow::anyhow!("error[E0308]: mismatched types");
        assert!(guard.defer_verification_infrastructure(&code_error).await.unwrap().is_none());
        let now=millis();
        let infra=anyhow::anyhow!("ort-sys failed to download prebuilt binaries: Temporary failure in name resolution");
        let eligible=guard.defer_verification_infrastructure(&infra).await.unwrap().unwrap();
        let after=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage,Stage::Verify,"retryable infrastructure became a permanent hold");
        assert_eq!(after.checkpoint.next.reason,"verification_infrastructure_retry");
        assert!(eligible>=now+300_000 && eligible<=millis()+300_000);
        assert_eq!(after.checkpoint.next.eligible_at_ms,eligible);
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);
        assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert_eq!(after.model_calls,before.model_calls);
        assert_eq!(after.progress_at_ms,before.progress_at_ms,"a retry is not productive progress");
        let report=accounting.retry_report("retry".into()).unwrap();
        assert!(!report.billed && report.is_idle() && report.resumed);
        guard.finish().unwrap();
        assert!(WorkflowStore::open(&path).unwrap().claim_productive(&before.workflow,"other",eligible-1,60_000).unwrap().is_none());
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
