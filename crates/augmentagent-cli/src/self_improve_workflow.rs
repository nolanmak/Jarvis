//! Production bridge between the autoship lanes and durable workflow state.
//! Included beneath self_improve so Git commands use the same sanitized runner.
use super::*;
#[path = "self_improve_stage_activity.rs"]
mod stage_activity;
#[path = "self_improve_family_dispatch_control.rs"]
mod family_dispatch_control;
#[path = "self_improve_family_wait_settlement.rs"]
mod family_wait_settlement;
#[cfg(test)]
#[path = "self_improve_family_wait_settlement_tests.rs"]
mod family_wait_settlement_tests;
#[path = "self_improve_family_budget_hold.rs"]
mod family_budget_hold;
#[cfg(test)]
#[path = "self_improve_family_budget_hold_tests.rs"]
mod family_budget_hold_tests;
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

#[derive(Debug)]
pub(super) struct OwnerControlUnavailable;
impl std::fmt::Display for OwnerControlUnavailable {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("owner-control-unavailable")}
}
impl std::error::Error for OwnerControlUnavailable {}

#[derive(Debug)]
pub(super) struct SharedIssueHistoryUnavailable;
impl std::fmt::Display for SharedIssueHistoryUnavailable {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("shared-issue-history-unavailable")}
}
impl std::error::Error for SharedIssueHistoryUnavailable {}
fn classify_issue_history_error(error:anyhow::Error)->anyhow::Error {
    if error.downcast_ref::<SharedIssueHistoryUnavailable>().is_none()
        && error.downcast_ref::<rusqlite::Error>().is_some_and(|error|!WorkflowStore::is_row_data_error(error)) {
        error.context(SharedIssueHistoryUnavailable)
    }else {error}
}

/// Free selection reads existing owner and workflow history without registering
/// a workflow or initializing storage. The productive claim still rechecks pause
/// and terminal state atomically; this is a fairness filter, never admission.
pub(super) struct FreshIssueControls {store:Option<WorkflowStore>,state:PathBuf}
impl FreshIssueControls {
    pub(super) fn open()->Result<Self> {Self::open_at(&path())}
    fn open_at(state:&Path)->Result<Self> {
        WorkflowStore::open_read_only(state).and_then(|store| {
            if let Some(store)=store.as_ref() {
                // Empty is not a canonical workflow. Reuse the real reader
                // queries to check structural capabilities even with no issues,
                // without loading candidate data, registering or migrating.
                store.snapshot("")?;
                store.owner_control("")?;
            }
            Ok(Self {store,state:state.to_path_buf()})
        }).map_err(|error|error.context(OwnerControlUnavailable))
    }
    /// A per-issue preview already observed existing protected history. Its
    /// fallback must not reinterpret a disappeared store as a fresh install.
    pub(super) fn require_existing(&self)->Result<()> {
        anyhow::ensure!(self.store.is_some(),OwnerControlUnavailable);Ok(())
    }
    fn reopen(state:&Path,observed_existing:bool)->Result<Self> {
        let current=Self::open_at(state).map_err(|error|error.context(SharedIssueHistoryUnavailable))?;
        if observed_existing {current.require_existing().map_err(|error|error.context(SharedIssueHistoryUnavailable))?;}
        Ok(current)
    }
    /// Close asynchronous selection gaps without turning an observed store
    /// disappearance into a new installation. This never claims or writes.
    pub(super) fn require_current_history(&self)->Result<()> {
        Self::reopen(&self.state,self.store.is_some()).map(|_|())
    }
    pub(super) fn selection_snapshot(&self,repository:&str,issue:u64)->Result<Option<durable::Snapshot>> {
        (||->Result<Option<durable::Snapshot>> {
            let id=durable::workflow_id(repository,issue)?;
            self.store.as_ref().map(|store|store.snapshot(&id)).transpose().map(Option::flatten)
        })().map_err(|error|classify_issue_history_error(error).context(OwnerControlUnavailable))
    }
    /// Read existing protected execution paths; this cannot mint a receipt or
    /// admit a replacement when a retained checkout has disappeared.
    pub(super) fn selection_checkout(&self, repository:&str, issue:u64)->Result<()> {
        (|| -> Result<()> {
            let Some(store)=self.store.as_ref() else {return Ok(())};
            let id=durable::workflow_id(repository,issue)?;
            let Some(snapshot)=store.snapshot(&id)? else {return Ok(())};
            let Some(context)=store.recovery_context(&id,&snapshot.checkpoint.binding)? else {return Ok(())};
            anyhow::ensure!(context.workflow==id && context.repository.eq_ignore_ascii_case(repository)
                && context.binding==snapshot.checkpoint.binding,"selection checkout context differs");
            let Err(error)=checkout_identity::check_recorded(&self.state,&id,&context.worktree,repository,&snapshot.branch) else {return Ok(())};
            // #2349 — scope-stage work whose recorded checkout path is gone is
            // selectable; productive preparation re-establishes continuity.
            if checkout_continuity::selectable(store,&self.state,&id,repository,&error) {
                info!(issue,reason=%error,"checkout continuity lost on scope-stage work; select to re-establish from current main");
                return Ok(());
            }
            Err(error)
        })().map_err(classify_issue_history_error)
    }
    /// #851/#2432 — the daily ledger holds a fresh start only. An issue whose
    /// workflow is already in flight resumes; an unreadable history keeps the
    /// ledger's hold, since the spend guard is the safe side.
    pub(super) fn daily_ledger_holds(&self,ledger:&AttemptLedger,today:u64,repository:&str,issue:u64,retry_due:bool)->bool {
        ledger.attempted_today(today,issue) && !retry_due
            && !self.selection_snapshot(repository,issue).ok().flatten()
                .is_some_and(|snapshot|durable::resumes_existing_work(&snapshot))
    }
    pub(super) fn terminal(&self,repository:&str,issue:u64)->Result<bool> {
        Ok(self.selection_snapshot(repository,issue)?.is_some_and(|snapshot|
            matches!(snapshot.checkpoint.stage,Stage::Merged|Stage::Retired)))
    }
    /// Fairness preview only: unchanged initial holds are skipped unless exact
    /// native custody has recovered. Read the same context/environment/binding inputs,
    /// without register, claim, capacity refresh or checkout preparation.
    pub(super) fn unchanged_initial_hold(&self,root:&Path,repository:&str,issue:&Issue)
        ->impl std::future::Future<Output=Result<bool>>+Send+'static {
        // Keep SQLite borrows entirely in this synchronous read. Only immutable
        // data crosses awaits; reopen the same protected store for the final
        // race check instead of retaining a connection or transaction.
        let inspected=(||->Result<Option<(durable::Snapshot,durable::RecoveryContext,String)>> {
            let Some(store)=self.store.as_ref() else{return Ok(None)};
            let id=durable::workflow_id(repository,issue.number)?;
            let Some(snapshot)=store.snapshot(&id)? else{return Ok(None)};
            if store.scope_policy_pending(&id)? || store.acceptance_escalation_pending(&id)? {return Ok(None);}
            if snapshot.pr.is_some() || !durable::awaits_initial_input(&snapshot.checkpoint) {return Ok(None);}
            let context=store.recovery_context(&id,&snapshot.checkpoint.binding)?
                .context("held issue execution context unavailable")?;
            anyhow::ensure!(context.workflow==id && context.repository.eq_ignore_ascii_case(repository)
                && context.binding==snapshot.checkpoint.binding && context.source_pr.is_none(),"held issue context differs");
            let request=serde_json::to_string(&(issue.number,&issue.title,&issue.body))?;
            if context.source_request!=request {return Ok(None);}
            Ok(Some((snapshot,context,request)))
        })();
        let root=root.to_path_buf();let repository=repository.to_owned();let number=issue.number;let state=self.state.clone();
        async move {
            let result=async {
                let Some((snapshot,context,request))=inspected? else{return Ok(false)};
                let (expected,expected_branch)=owner_checkout_recovery::layout(&root,&state,&repository,number).await?;
                recovery::existing_checkout(&root,&context.worktree).await?;
                anyhow::ensure!(context.worktree.canonicalize()?==expected.canonicalize()?
                    && snapshot.branch==expected_branch,"held issue checkout differs from execution lane");
                let (ok,branch,_)=run("git",&["symbolic-ref","--quiet","--short","HEAD"],&context.worktree).await?;
                anyhow::ensure!(ok && branch.trim()==snapshot.branch,"held issue branch differs");
                let upstream=remote_main_head(&root,&repository).await?;
                let current_environment=owner_checkout_recovery::environment(&state,&repository,number,recovery::environment_identity(&context.worktree,&upstream).await?)?;
                if !recovery_context_matches(&context,&request,&expected,&current_environment,true) {return Ok(false);}
                let binding=existing_candidate_binding(&context.worktree,&context.contract,&context.environment,Some(&snapshot)).await?;
                // A genuine recovery cue arriving during Git inspection must
                // rejoin selection. Productive claim still checks atomically.
                let reopened=Self::reopen(&state,true)?;
                let store=reopened.store.as_ref().expect("existing history was required above");
                // Standalone self-improve has no background inspector: restored
                // physical custody can be recovered by the ordinary atomic claim
                // even when the binding and checkpoint have not changed yet.
                let custody_recovered=store.native_custody_recovery_available(
                    &snapshot,&context.worktree,acceptance::require_retained_native_custody)?;
                let current=store.snapshot(&snapshot.workflow)?.context("held issue history disappeared during inspection")?;
                Ok(binding==snapshot.checkpoint.binding && current==snapshot && !custody_recovered)
            }.await;
            result.map_err(|error:anyhow::Error|classify_issue_history_error(error).context(OwnerControlUnavailable))
        }
    }
    pub(super) fn paused(&self,repository:&str,issue:u64)->Result<bool> {
        (||->Result<bool> {
            let id=durable::workflow_id(repository,issue)?;
            let control=self.store.as_ref().map(|store|store.owner_control(&id)).transpose()?.flatten();
            Ok(control.is_some_and(|control|control.paused))
        })().map_err(|error|classify_issue_history_error(error).context(OwnerControlUnavailable))
    }
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
    state: PathBuf,
    checkout_identity: Arc<Mutex<Option<checkout_identity::Identity>>>,
    store: Arc<Mutex<WorkflowStore>>,
    lease: Lease,
    repository: String,
    worktree: PathBuf,
    contract: String,
    environment: String,
    source_request:Option<(String,Option<u64>)>,
    binding: Binding,
    heartbeat: tokio_util::sync::CancellationToken,
    lease_failure: Arc<Mutex<Option<String>>>,
    started: std::time::Instant,
    accounted_ms: Arc<AtomicU64>,
    capacity_exhausted: Arc<AtomicBool>,
    family_budget_wait: Arc<AtomicBool>,
    family_wait_released: Arc<AtomicBool>,
    released: bool,
    audit: Arc<augmentagent_channel_core::AuditLogger>,
    acceptance_authority:Arc<Mutex<AcceptanceAuthority>>,
    acceptance_source:Option<crate::autoship_contract::AcceptanceSource>,
}

#[derive(Debug)]
struct SemanticSourceUnavailable;
impl std::fmt::Display for SemanticSourceUnavailable {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"Original verification evidence is unavailable; waiting for its restoration")}
}
impl std::error::Error for SemanticSourceUnavailable {}

fn record_semantic_source_failure(store:&mut WorkflowStore,lease:&Lease,binding:&Binding,cue:&str,error:&anyhow::Error)->Result<()> {
    if let Some(setup)=acceptance::retained_setup_failure(error) {store.hold_retained_setup_failure(lease,binding,cue,setup,acceptance::retained_setup_candidate(error),millis())?;}
    else {store.hold_semantic_source_unavailable(lease,binding,cue,millis())?;}
    let detail=format!("{error:#}");
    let key=format!("{:x}",Sha256::digest(serde_json::to_vec(&("semantic-source-diagnostic-v2",&lease.workflow,cue,binding,&detail))?));
    let body=serde_json::json!({"version":1,"kind":"semantic_source_diagnostic","workflow":lease.workflow,"binding":binding,"diagnostic":detail});
    store.record_scope_stage_response(lease,binding,&key,&body.to_string(),millis()).map_err(|_|anyhow::Error::from(SemanticSourceUnavailable))?;
    Ok(())
}

/// Source data is rechecked at preparation and both launch gates. This has
/// no model or verifier execution and never substitutes an old positive approval.
fn semantic_preparation_source(store:&mut WorkflowStore,lease:&Lease,root:&Path,repository:&str)->Result<Option<acceptance::PreparedAcceptance>> {
    let snapshot=store.snapshot(&lease.workflow)?.context("semantic workflow missing")?;
    let binding=&snapshot.checkpoint.binding;
    let (cue,settled):(durable::SemanticReconsideration,bool)=match store.semantic_reconsideration(&lease.workflow,binding)? {
        Some(cue)=>(cue,false),
        None=>match store.retained_qualification_cue(&lease.workflow,binding)? {
            Some(cue)=>(cue,true),None=>return Ok(None),
        },
    };
    store.validate_dispatch(lease,binding,millis())?;
    if store.semantic_source_waiting(&lease.workflow,binding)? {return Err(SemanticSourceUnavailable.into());}
    let source=(||->Result<Option<acceptance::PreparedAcceptance>> {
        let source=source_from_workflow(&lease.workflow).context("semantic source identity missing")?;
        let control=acceptance::control_directories(repository,&lease.workflow)?.workflow_acceptance;
        if let Some(reference)=cue.retained_witness() {
            if acceptance::retained_qualification_superseded(&control,source,reference)? {return Ok(None);}
            if settled {
                acceptance::check_settled_retained_preparation(root,&control,source,reference,&store.inspect_verifier_evidence(&lease.workflow))?;
                return Ok(None);
            }
            return acceptance::retained_qualification_source(root,&control,source,reference,
                &store.inspect_verifier_evidence(&lease.workflow)).map(Some);
        }
        let custody=store.baseline_native_custody(lease,binding,millis())?;
        let (prepared,identity)=acceptance::retained_semantic_source(root,&control,source,binding,&custody,&store.inspect_verifier_evidence(&lease.workflow))?
            .context("original semantic prepared source is unavailable")?;
        anyhow::ensure!(identity==cue.judgment,"semantic source differs from its queued judgment");
        Ok(Some(prepared))
    })();
    match source {
        Ok(prepared)=>Ok(prepared),
        Err(error)=> {
            record_semantic_source_failure(store,lease,binding,&cue.key,&error)?;
            Err(SemanticSourceUnavailable.into())
        }
    }
}

enum AcceptanceAuthority {
    Unbound,
    RecoveryOnly,
    Qualified {prepared:acceptance::PreparedAcceptance,control:PathBuf},
}

#[derive(Clone,Copy,PartialEq,Eq)]
enum AcquireMode {Productive,OwnerTurn,VerifierRecovery}

fn source_from_workflow(id:&str)->Option<crate::autoship_contract::AcceptanceSource> {
    use crate::autoship_contract::AcceptanceSource;
    if let Some((_,number))=id.rsplit_once('#') {
        return number.parse::<u64>().ok().filter(|n|*n>0).map(AcceptanceSource::Issue);
    }
    id.rsplit_once("/pr/").and_then(|(_,number)|number.parse::<u64>().ok()).filter(|n|*n>0).map(AcceptanceSource::PullRequest)
}

/// A run-local view of durable admissions. It survives guard consumption on
/// normal completion, so every early `?` can still settle capacity accurately.
pub(super) struct CapacityAccounting {
    store: Arc<Mutex<WorkflowStore>>,
    audit: Arc<augmentagent_channel_core::AuditLogger>,
    workflow: String,
    calls_before: u64,
    resumed: bool,
    generation: i64,
    family_budget_wait: Arc<AtomicBool>,
    lease: Lease,
    heartbeat: tokio_util::sync::CancellationToken,
    started: std::time::Instant,
    accounted_ms: Arc<AtomicU64>,
    released: Arc<AtomicBool>,
}
impl CapacityAccounting {
    pub(super) fn settle_ci(&self, outcome: Result<RunReport>) -> Result<RunReport> {
        if self.family_budget_wait.load(Ordering::SeqCst) {
            return self.report()?.context("family budget refusal has no durable status report");
        }
        let source_waiting={
            let store=self.store.lock().map_err(|_|anyhow::anyhow!("source accounting store poisoned"))?;
            let snapshot=store.snapshot(&self.workflow)?.context("source accounting workflow missing")?;
            store.semantic_source_waiting(&self.workflow,&snapshot.checkpoint.binding)?
        };
        if source_waiting {return self.report()?.context("source hold has no status report");}
        let error=match &outcome {
            // A routing hold is still idle, but it cannot erase admissions
            // already made in this run. Historical spend never bills a retry.
            Ok(report) if report.held=>return self.retry_report(report.message.clone()),
            Ok(_)=>return outcome,
            Err(error)=>error,
        };
        let Some(deferred) = error.downcast_ref::<CiInfrastructureDeferred>() else {
            let checkpoint=self.store.lock().map_err(|_|anyhow::anyhow!("retry accounting store poisoned"))?
                .snapshot(&self.workflow)?.context("retry workflow missing")?.checkpoint;
            if durable::retry_recovery::class(&checkpoint).is_some() {
                return self.retry_report(format!("Autoship deferred {} turn; preserved work will retry automatically",checkpoint.next.reason));
            }
            return outcome;
        };
        anyhow::ensure!(deferred.workflow == self.workflow, "CI deferral belongs to another workflow");
        let checkpoint = self.store.lock().map_err(|_|anyhow::anyhow!("retry accounting store poisoned"))?
            .snapshot(&self.workflow)?.context("retry workflow missing")?.checkpoint;
        if checkpoint.next.reason=="retry_cycle_exhausted" {
            return self.retry_report("Autoship retry cycle exhausted; preserved request scheduled for automatic rescoping".into());
        }
        anyhow::ensure!(checkpoint.next.reason == "verification_infrastructure_retry"
            && checkpoint.next.action == "retry_verification"
            && checkpoint.next.eligible_at_ms == deferred.eligible_at_ms,
            "CI deferral has no matching durable schedule");
        self.retry_report(deferred.to_string())
    }

    pub(super) fn retry_report(&self,message:String)->Result<RunReport> {
        let calls=self.store.lock().map_err(|_|anyhow::anyhow!("retry accounting store poisoned"))?
            .snapshot(&self.workflow)?.context("retry workflow missing")?.model_calls;
        let paid=calls.checked_sub(self.calls_before).context("durable admissions decreased")?;
        // Actual spend and productive progress are independent. A retry must
        // remain deferred even if a builder ran earlier in the same attempt.
        Ok(RunReport{message,billed:paid>0,held:true,resumed:self.resumed})
    }
    pub(super) fn report(&self) -> Result<Option<RunReport>> {
        if let Some(report)=family_budget_hold::report(self)? {return Ok(Some(report));}
        let store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
        let paused=store.owner_control(&self.workflow)?.is_some_and(|c|c.paused);
        let snapshot=store.snapshot(&self.workflow)?.context("capacity workflow missing")?;
        let source_waiting=store.semantic_source_waiting(&self.workflow,&snapshot.checkpoint.binding)?;
        if !paused && !source_waiting && !self.audit.resource_capacity_seen() { return Ok(None); }
        let calls_after = snapshot.model_calls;
        let paid = calls_after.checked_sub(self.calls_before).context("durable model admissions decreased")?;
        Ok(Some(RunReport {
            message: if paused {format!("Autoship owner pause requested; candidate and evidence preserved ({paid} model admissions this run)")}
                else if source_waiting {format!("Autoship waiting for original verification evidence; candidate and prior charges preserved ({paid} model admissions this run)")}
                else {format!("Autoship awaiting declared run capacity; candidate and evidence preserved ({paid} model admissions this run)")},
            billed: paid > 0,
            held: true,
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
    reconcile_runtime_retirements_in(store, now)?;
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
pub(super) fn reconcile_runtime_retirements()->Result<usize> {
    reconcile_runtime_retirements_at(&path(),millis())
}
fn reconcile_runtime_retirements_at(state:&Path,now:i64)->Result<usize> {
    if !state.exists(){return Ok(0);}
    let mut store=WorkflowStore::open(state)?;
    let retired=reconcile_runtime_retirements_in(&mut store,now)?;
    augmentagent_channel_core::native_vm::reconcile_retired_runtimes()?;
    let report=augmentagent_channel_core::native_vm::state_capacity::inspect();
    let woken=store.reconcile_native_capacity(&report,now,16)?;
    if woken>0 {info!(woken,"native capacity wait schedules eligible; normal admission remains required");}
    Ok(retired)
}
fn reconcile_runtime_retirements_in(store:&mut WorkflowStore,now:i64)->Result<usize> {
    if std::env::var_os("AUGMENTAGENT_NATIVE_VM_POLICY").is_none(){store.settle_owner_execution_retries(now,16)?;return Ok(0);}
    let rows=store.pending_native_retirements(now,16)?;
    let mut completed=0;
    for (workflow,generation,audit_path) in rows {
        let owner=augmentagent_channel_core::native_vm::RuntimeOwner{workflow_id:workflow.clone(),lease_generation:generation};
        match augmentagent_channel_core::native_vm::retire_owned_runtimes(Path::new(&audit_path),&owner) {
            Ok(_)=>{store.complete_native_retirement(&workflow,generation,&audit_path,now)?; completed+=1;}
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
    let mut store = WorkflowStore::open(path)?;
    let Some(id) = store.resolve_pr(repository, pr)? else {
        return Ok(false);
    };
    refresh_native_capacity(&mut store,&id,now)?;
    Ok(store
        .snapshot(&id)?
        .is_some_and(|snapshot| integration::requires_reverification(&snapshot.checkpoint) || snapshot.checkpoint.next.action == "reverify_candidate"
            || (snapshot.checkpoint.next.action == "await_capacity"
                && snapshot.checkpoint.next.eligible_at_ms <= now
                && snapshot.lease_until_ms <= now)))
}

/// Observe native storage outside the SQL transaction. A wake changes only
/// eligibility; the subsequent original claim/admission checks remain mandatory.
fn refresh_native_capacity(store:&mut WorkflowStore,id:&str,now:i64)->Result<()> {
    if store.snapshot(id)?.is_some_and(|s|s.checkpoint.next.reason=="resource_capacity" && s.checkpoint.next.action=="await_capacity"
        && s.checkpoint.next.eligible_at_ms>now && s.lease_until_ms<=now) {
        let report=augmentagent_channel_core::native_vm::state_capacity::inspect();
        store.wake_native_capacity(id,&report,now)?;
    }
    Ok(())
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
    pub(super) fn legacy_notice_suppressed(&self) -> bool {
        self.inner.as_ref().is_some_and(WorkflowGuard::legacy_notice_suppressed)
    }
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
        let audit = workflow_audit(path, &id, None)?;
        let inner = WorkflowGuard {
            state: path.into(), checkout_identity: Arc::new(Mutex::new(None)),
            audit,
            store: Arc::new(Mutex::new(store)),
            lease,
            repository: repository.into(),
            worktree: PathBuf::new(),
            contract: String::new(),
            environment: String::new(),
            source_request:None,
            binding,
            heartbeat: tokio_util::sync::CancellationToken::new(),
            lease_failure: Arc::new(Mutex::new(None)),
            started: std::time::Instant::now(),
            accounted_ms: Arc::new(AtomicU64::new(0)),
            capacity_exhausted: Arc::new(AtomicBool::new(false)),
            family_budget_wait: Arc::new(AtomicBool::new(false)),
            acceptance_authority:Arc::new(Mutex::new(AcceptanceAuthority::Unbound)),
            acceptance_source:source_from_workflow(&id),
            family_wait_released: Arc::new(AtomicBool::new(false)),
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
        let worktree = prepare_resume_worktree_at_head(repo, pr, &self.branch,Some(&expected)).await?;
        let identity=checkout_identity::Identity::observe(&inner.state,repo,&worktree,inner.id(),&inner.repository,&self.branch).await?;
        identity.enroll(&mut *inner.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?,&inner.lease,&inner.binding,millis())?;
        *inner.checkout_identity.lock().map_err(|_|anyhow::anyhow!("checkout identity lock poisoned"))?=Some(identity);
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

    pub(super) fn validate_note_dispatch(&self)->Result<()> {
        self.inner.as_ref().context("released sweep lease")?.validate_note_dispatch()
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

/// Recover a failed transport before selectors exclude its pending effect.
/// Reuses protected execution context and the original bound Push; no model is
/// bound or called, and no unrelated pending operation becomes selectable.
pub(super) async fn recover_pending_push(repo:&Path,candidate:&adoption::AdoptedPr)->Result<()> {
    let Some(store)=WorkflowStore::open_read_only(&path())? else {return Ok(())};
    let Some(id)=store.resolve_pr(&candidate.repository,candidate.pr)? else {return Ok(())};
    let Some(snapshot)=store.snapshot(&id)? else {return Ok(())};
    let now=millis();
    if snapshot.branch!=candidate.branch || snapshot.lease_until_ms>now
        || snapshot.checkpoint.next.eligible_at_ms>now || snapshot.checkpoint.stage!=Stage::Publish
        || store.require_owner_dispatch(&id).is_err() || !store.retryable_bound_push(&id)? {return Ok(());}
    let source=id.rsplit_once('#').map(|(_,number)|number.parse::<u64>()).transpose()?;
    if !(source==candidate.source_issue && source.is_some()
        || durable::pr_workflow_id(&candidate.repository,candidate.pr)?==id) {return Ok(());}
    if let Some(issue)=candidate.source_issue {
        if source_reconcile::reconcile(repo,candidate.pr,issue,true,true).await!=source_reconcile::SourceAction::Continue {
            return Ok(());
        }
    }
    let Some(context)=store.recovery_context(&id,&snapshot.checkpoint.binding)? else {return Ok(())};
    anyhow::ensure!(context.repository.eq_ignore_ascii_case(&candidate.repository),"Push recovery repository changed");
    recovery::existing_checkout(repo,&context.worktree).await?;
    let binding=existing_candidate_binding(&context.worktree,&context.contract,&context.environment,Some(&snapshot)).await?;
    anyhow::ensure!(binding==snapshot.checkpoint.binding,"pending Push candidate changed");
    drop(store);
    let Some(mut guard)=WorkflowGuard::acquire(Start {repo_root:repo,worktree:&context.worktree,
        repository:&candidate.repository,source_issue:source,pr:Some(candidate.pr),branch:&snapshot.branch,
        contract:&context.source_request,environment:&context.environment}).await? else {return Ok(())};
    let result=async {
        anyhow::ensure!(guard.binding()==&binding && guard.store.lock().map_err(|_|anyhow::anyhow!("Push recovery store poisoned"))?
            .retryable_bound_push(guard.id())?,"pending Push recovery changed");
        // A failed routing observation leaves the original pending Push in
        // Publish so a later inventory pass can retry it without a reset.
        if !manual_routing::allowed(repo,&candidate.repository,candidate.pr,source).await {return Ok(());}
        guard.push(&snapshot.branch).await?;
        guard.transition(Stage::Publish,"bound_push_recovered","resume_preserved_work",0).await?;
        Ok::<(),anyhow::Error>(())
    }.await;
    guard.finish()?;
    result
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

    /// Same acquisition path with an explicit fixture clock for crash/expiry tests.
    #[cfg(test)]
    pub(super) async fn acquire_fixture_at(state:&Path,start:Start<'_>,now:i64)->Result<Option<Self>> {
        Self::acquire_at(state,start,now).await
    }

    pub(super) async fn acquire_owner_turn(start:Start<'_>)->Result<Option<Self>> {
        Self::acquire_mode_at(&path(),start,millis(),AcquireMode::OwnerTurn).await
    }
    pub(super) async fn acquire_verifier_recovery(start:Start<'_>)->Result<Option<Self>> {
        Self::acquire_mode_at(&path(),start,millis(),AcquireMode::VerifierRecovery).await
    }
    #[cfg(test)]
    pub(super) async fn acquire_verifier_recovery_fixture(state:&Path,start:Start<'_>)->Result<Option<Self>> {
        Self::acquire_mode_at(state,start,millis(),AcquireMode::VerifierRecovery).await
    }
    async fn acquire_at(state: &Path, start: Start<'_>, now: i64) -> Result<Option<Self>> {
        Self::acquire_mode_at(state,start,now,AcquireMode::Productive).await
    }
    async fn acquire_mode_at(state:&Path,start:Start<'_>,now:i64,mode:AcquireMode)->Result<Option<Self>> {
        // #2354: never claim the next workflow with a deploy pending.
        crate::autoship_worker::safe_point().await;
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
        let checkout_identity = checkout_identity::Identity::observe(state, start.repo_root, start.worktree,
            &id, start.repository, start.branch).await?;
        let existing = store.snapshot(&id)?;
        let semantic=if let Some(snapshot)=&existing {store.semantic_reconsideration(&id,&snapshot.checkpoint.binding)?}else{None};
        let retained=if let Some(snapshot)=&existing {store.retained_qualification_history(&id,&snapshot.checkpoint.binding)?}else{None};
        anyhow::ensure!(!existing.as_ref().is_some_and(|snapshot|snapshot.checkpoint.next.reason=="original_semantic_witness_recovered")
            || semantic.is_some() || retained.is_some(),"semantic recovery source reference is missing");
        let context=if let Some(snapshot)=&existing {
            // #2279 — a verified candidate whose push failed before dispatch
            // keeps its scoped contract; the raw request must not rebind it
            // to `inputs_changed` and send finished work back to scope.
            let relevant=snapshot.checkpoint.stage==Stage::Scope || snapshot.checkpoint.next.reason=="original_native_custody_recovered"
                || (snapshot.checkpoint.stage==Stage::Publish && snapshot.checkpoint.next.reason=="publishing_revision"
                    && snapshot.checkpoint.next.action=="push")
                || snapshot.checkpoint.next.reason=="original_verifier_measurement_recovered"
                || snapshot.checkpoint.next.reason=="original_semantic_witness_recovered" || semantic.is_some()
                || store.acceptance_escalation_pending(&id)?;
            if relevant || store.retryable_bound_push(&id)? {store.recovery_context(&id,&snapshot.checkpoint.binding)?}else{None}
        }else{None};
        let initial_wait=existing.as_ref().is_some_and(|snapshot|durable::awaits_initial_input(&snapshot.checkpoint));
        let context=context.filter(|context|recovery_context_matches(context,start.contract,start.worktree,start.environment,initial_wait));
        let contract=context.as_ref().map_or(start.contract,|c|c.contract.as_str());
        let environment=context.as_ref().map_or(start.environment,|c|c.environment.as_str());
        let mut binding=existing_candidate_binding(start.worktree,contract,environment,existing.as_ref()).await?;
        if mode==AcquireMode::OwnerTurn {
            anyhow::ensure!(existing.as_ref().is_some_and(|s|s.checkpoint.binding==binding),"owner turn requires the current owned candidate binding");
        } else if mode==AcquireMode::VerifierRecovery {
            if !existing.as_ref().is_some_and(|s|s.checkpoint.binding==binding && s.branch==start.branch
                && s.checkpoint.stage==Stage::Scope && matches!((s.checkpoint.next.reason.as_str(),s.checkpoint.next.action.as_str()),
                    ("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery")|
                    ("acceptance_verifier_evidence_unknown","await_original_evidence_recovery"))) {return Ok(None);}
        } else {store.register(&id,start.branch,binding.clone(),now)?;}
        if mode!=AcquireMode::VerifierRecovery {refresh_native_capacity(&mut store,&id,now)?;}
        let owner=format!("{}:{}",std::process::id(),uuid::Uuid::new_v4());
        let lease=match mode {
            AcquireMode::OwnerTurn=>store.claim_owner_turn(&id,&owner,now,LEASE_MS)?.map(|turn|turn.lease),
            AcquireMode::VerifierRecovery=>store.claim_reconciliation(&id,&owner,now,LEASE_MS)?,
            AcquireMode::Productive=>store.claim_productive_recovering_custody(&id,&owner,now,LEASE_MS,&binding,start.worktree,acceptance::require_retained_native_custody)?,
        };
        let Some(lease)=lease else {return Ok(None);};
        if mode==AcquireMode::VerifierRecovery {
            match store.original_verifier_recovery_plan(&lease,&binding,now) {
                Ok(Some(_))=>{},
                Ok(None)=>{store.release(&lease,now)?;return Ok(None);},
                Err(error)=>{store.release(&lease,now)?;return Err(error);},
            }
        }
        if let Some(pr) = start.pr {
            store.attach_pr_in_repo(&lease, start.repository, pr, now)?;
        }
        let snapshot = store
            .snapshot(&id)?
            .context("registered workflow missing")?;
        // Only productive acquisition may migrate old scope holds. Owner
        // turns intentionally run native work under a paused owner grant;
        // reconciliation must preserve its original evidence and authority.
        if mode==AcquireMode::Productive {store.reconsider_scope_policy(&lease,now)?;}
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
        if let Err(error)=checkout_identity.enroll(&mut store,&lease,&binding,now) {
            store.release(&lease,now)?;return Err(error);
        }
        if mode==AcquireMode::Productive {
            if let Some(pr)=start.pr {
                if let Err(error)=store.record_adopted_publication(&lease,&binding,start.repository,pr,start.branch,now) {
                    store.release(&lease,now)?;return Err(error);
                }
            }
        }
        let audit = acquire_audit_generation(state, &mut store, &lease,
            Some(start.worktree), &mut binding, now)?;
        if mode!=AcquireMode::VerifierRecovery {
            audit.set_native_runtime_owner(augmentagent_channel_core::native_vm::RuntimeOwner {
                workflow_id: lease.workflow.clone(), lease_generation: lease.generation,
            })?;
            store.register_native_owner(&lease,audit.path().to_str().context("native audit path is not UTF-8")?,now)?;
        }
        let guard = Self {
            state: state.into(), checkout_identity: Arc::new(Mutex::new(Some(checkout_identity))),
            audit,
            store: Arc::new(Mutex::new(store)),
            lease,
            repository: start.repository.into(),
            worktree: start.worktree.into(),
            contract: contract.into(),
            environment: environment.into(),
            source_request:Some((start.contract.into(),context.as_ref().map_or(start.pr,|context|context.source_pr))),
            binding,
            heartbeat: tokio_util::sync::CancellationToken::new(),
            lease_failure: Arc::new(Mutex::new(None)),
            started: std::time::Instant::now(),
            accounted_ms: Arc::new(AtomicU64::new(0)),
            capacity_exhausted: Arc::new(AtomicBool::new(false)),
            family_budget_wait: Arc::new(AtomicBool::new(false)),
            family_wait_released: Arc::new(AtomicBool::new(false)),
            released: false,
            acceptance_authority:Arc::new(Mutex::new(AcceptanceAuthority::RecoveryOnly)),
            acceptance_source:source_from_workflow(&id),
        };
        if let Some(context)=guard.execution_context(&guard.binding)? {
            let result=guard.store.lock().map_err(|_|anyhow::anyhow!("workflow context lock poisoned"))?
                .record_recovery_context(&guard.lease,&guard.binding,&context,now);
            if let Err(error)=result {guard.finish()?;return Err(error);}
        }
        // A retained source belongs to the original request, not an old head.
        // Publish the authorized current context before resolving that source
        // and before installing any model admission observer.
        let semantic=guard.store.lock().map_err(|_|anyhow::anyhow!("workflow context lock poisoned"))?
            .semantic_reconsideration(&id,&guard.binding)?;
        let retained=guard.store.lock().map_err(|_|anyhow::anyhow!("workflow context lock poisoned"))?
            .retained_qualification_history(&id,&guard.binding)?;
        let acceptance_authority=if mode==AcquireMode::VerifierRecovery || snapshot.checkpoint.next.reason=="original_native_custody_recovered" || semantic.is_some() || retained.is_some() {
            AcceptanceAuthority::RecoveryOnly
        }else {AcceptanceAuthority::Unbound};
        *guard.acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?=acceptance_authority;
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
            workflow:self.id().into(), calls_before, resumed, generation:self.lease.generation,
            family_budget_wait:self.family_budget_wait.clone(), lease:self.lease.clone(),
            heartbeat:self.heartbeat.clone(), started:self.started,
            accounted_ms:self.accounted_ms.clone(), released:self.family_wait_released.clone() })
    }
    pub(super) fn audit_logger(&self) -> Arc<augmentagent_channel_core::AuditLogger> {
        self.audit.clone()
    }
    fn install_model_accounting(&self) -> Result<()> {
        self.audit.require_run_resources()?;
        self.install_native_tool_progress()?;
        let dispatch_store=self.store.clone();let dispatch_lease=self.lease.clone();
        let dispatch_root=self.worktree.clone();let dispatch_repository=self.repository.clone();
        let dispatch_identity=self.checkout_identity.clone();
        let dispatch_family=self.family_budget_wait.clone();
        self.audit.set_dispatch_observer(Arc::new(move || {
            checkout_identity::validate(&dispatch_identity)?;
            let mut store=dispatch_store.lock().map_err(|_|anyhow::anyhow!("workflow dispatch lock poisoned"))?;
            let binding=store.snapshot(&dispatch_lease.workflow)?.context("workflow missing")?.checkpoint.binding;
            family_dispatch_control::validate_native(&mut store,&dispatch_lease,&binding,&dispatch_family)?;
            semantic_preparation_source(&mut store,&dispatch_lease,&dispatch_root,&dispatch_repository)?;
            Ok(())
        }))?;
        let capacity_store = self.store.clone();
        let capacity_lease = self.lease.clone();
        let capacity_flag = self.capacity_exhausted.clone();
        self.audit.set_resource_capacity_observer(Arc::new(move |observation| {
            // Refuse further work even if persistence itself fails. This
            // controller observation never grants permission to continue.
            capacity_flag.store(true, Ordering::SeqCst);
            let mut store = capacity_store.lock().map_err(|_| anyhow::anyhow!("workflow capacity lock poisoned"))?;
            let fingerprint=if observation["phase"]=="native_runtime_state_capacity" && observation["exhausted"]==true
                && observation["native_state"]["status"]=="capacity-exhausted" {
                observation["native_state"]["capacity_fingerprint"].as_str()
            }else{None};
            let tool_refusal=durable::tool_capacity::ToolCapacityRefusal::capture(observation);
            store.defer_run_capacity_observation(&capacity_lease,fingerprint,tool_refusal.as_ref(),millis())?;
            Ok(())
        }))?;
        let capacity_flag = self.capacity_exhausted.clone();
        let store = self.store.clone();
        let lease = self.lease.clone();
        let observational = self.repository.starts_with("autoship-observation/") || self.repository.starts_with("autoship-canary/");
        let acceptance_authority=self.acceptance_authority.clone();let worktree=self.worktree.clone();
        let model_identity=self.checkout_identity.clone();
        let family_budget_flag=self.family_budget_wait.clone();
        self.audit.set_correlated_model_call_observer(Arc::new(move |provider| {
            family_budget_hold::require_active(&family_budget_flag)?;
            checkout_identity::validate(&model_identity)?;
            anyhow::ensure!(!capacity_flag.load(Ordering::SeqCst), "resource_capacity: await_capacity checkpoint retained");
            let mut store = store.lock().map_err(|_| anyhow::anyhow!("workflow accounting lock poisoned"))?;
            let current=store.snapshot(&lease.workflow)?.context("workflow missing")?;
            anyhow::ensure!(!matches!(current.checkpoint.stage,Stage::Merged|Stage::Retired),"terminal workflow cannot admit another model call");
            let custody=store.baseline_native_custody(&lease,&current.checkpoint.binding,millis())?;
            acceptance::require_retained_native_custody(&worktree,&custody)?;
            if !augmentagent_channel_core::native_session::is_fresh_review() {
                let state=store.snapshot(&lease.workflow)?.context("workflow missing")?;
                let authority=acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?;
                match &*authority {
                    AcceptanceAuthority::Qualified {prepared,control}=> {
                        acceptance::require_qualified_baseline(&worktree,control,prepared)?;
                        if state.checkpoint.binding.contract.starts_with("registered:") {
                            anyhow::ensure!(state.checkpoint.binding.contract==format!("registered:{}",prepared.baseline.registration.contract_hash()),"builder qualification belongs to another registered contract");
                        }
                    },
                    AcceptanceAuthority::RecoveryOnly=>anyhow::bail!("baseline_witness_unqualified: recovery metadata cannot authorize a builder"),
                    AcceptanceAuthority::Unbound if state.checkpoint.binding.contract.starts_with("registered:") || matches!(state.checkpoint.stage,Stage::Build|Stage::Verify|Stage::Review)=>anyhow::bail!("baseline_witness_unqualified: builder admission requires protected original-production qualification"),
                    AcceptanceAuthority::Unbound=>{},
                }
            }
            let receipt = format!("model-call:{}", uuid::Uuid::new_v4());
            let bounded=BOUNDED_NATIVE_CALL.try_with(Clone::clone).ok();
            if let Some(bounded)=&bounded {
                anyhow::ensure!(bounded.workflow==lease.workflow,"bounded native call used a different workflow audit sink");
            }
            let actual=match provider {"codex"=>Some(durable::submission::Provider::Codex),"claude"=>Some(durable::submission::Provider::Claude),_=>None};
            let admission=store.charge_admitted_model(&lease,&receipt,millis(),observational.then(observation_call_cap),
                bounded.as_ref().map(|b|(b.purpose.as_str(),&b.binding,b.limit)),actual,
                augmentagent_channel_core::native_session::is_fresh_review());
            family_budget_hold::settle_admission(&mut store,&lease,&current.checkpoint.binding,
                &family_budget_flag,admission,millis())?;
            Ok(receipt)
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
        store.register_native_owner(&lease,audit.path().to_str().context("native audit path is not UTF-8")?,now)?;
        let guard = Self { state:state.clone(),checkout_identity:Arc::new(Mutex::new(None)),audit,store:Arc::new(Mutex::new(store)),lease,repository:id.clone(),worktree:PathBuf::new(),contract:"observation-v1".into(),environment:"diagnostic".into(),source_request:None,binding,
            acceptance_authority:Arc::new(Mutex::new(AcceptanceAuthority::Unbound)),
            acceptance_source:None,
            heartbeat:tokio_util::sync::CancellationToken::new(),lease_failure:Arc::new(Mutex::new(None)),started:std::time::Instant::now(),accounted_ms:Arc::new(AtomicU64::new(0)),capacity_exhausted:Arc::new(AtomicBool::new(false)),family_budget_wait:Arc::new(AtomicBool::new(false)),family_wait_released:Arc::new(AtomicBool::new(false)),released:false };
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
    /// #2430: the PR branch this workflow publishes to.
    pub(super) fn branch(&self) -> Result<String> {
        Ok(self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow lock poisoned"))?
            .snapshot(self.id())?
            .context("workflow missing")?
            .branch)
    }
    /// #1714: when the durable outbox routes this workflow, legacy
    /// `notify_review` must stay silent so the owner is not told twice.
    pub(super) fn legacy_notice_suppressed(&self) -> bool {
        self.store.lock().ok().and_then(|store|store.durable_route_owns_updates(self.id()).ok()).unwrap_or(false)
    }
    pub(super) fn binding(&self) -> &Binding {
        &self.binding
    }

    pub(super) fn bind_acceptance(
        &mut self,
        prepared: &acceptance::PreparedAcceptance,
        control:&Path,
    ) -> Result<()> {
        self.validate_acceptance_source(prepared)?;
        let retained=self.store.lock().map_err(|_|anyhow::anyhow!("baseline qualification store poisoned"))?
            .semantic_reconsideration(self.id(),&self.binding)?.is_some_and(|cue|cue.retained_witness().is_some());
        if retained {
            *self.acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?=AcceptanceAuthority::RecoveryOnly;
        }
        acceptance::require_qualified_baseline(&self.worktree,control,prepared)?;
        self.bind_acceptance_identity(prepared)?;
        if retained {
            let source=self.acceptance_source.context("retained qualification source identity missing")?;
            let native_control=acceptance::control_directories(&self.repository,self.id())?.workflow_acceptance;
            let settled=self.store.lock().map_err(|_|anyhow::anyhow!("baseline qualification store poisoned"))?
                .settle_retained_qualification(&self.lease,&self.binding,millis(),|reference,evidence| {
                    let original=acceptance::retained_qualification_source(&self.worktree,&native_control,source,reference,evidence)?;
                    let expected=serde_json::to_vec(prepared)?;
                    anyhow::ensure!(serde_json::to_vec(&original)?==expected,"qualified preparation differs from its retained source");
                    let published=acceptance::retained_prepared(&self.worktree,control,source)?.context("qualified prepared evidence is not published")?;
                    anyhow::ensure!(serde_json::to_vec(&published)?==expected,"published prepared evidence differs from qualified preparation");
                    acceptance::require_qualified_baseline(&self.worktree,control,prepared)
                })?;
            anyhow::ensure!(settled,"retained qualification handoff changed before settlement");
        }
        *self.acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?=AcceptanceAuthority::Qualified {prepared:prepared.clone(),control:control.canonicalize()?};
        Ok(())
    }

    /// This metadata is needed by independent repair of an invalid legacy
    /// verifier. It deliberately grants no builder/verification/merge proof.
    pub(super) fn bind_acceptance_for_recovery(&mut self,prepared:&acceptance::PreparedAcceptance)->Result<()> {
        self.bind_acceptance_identity(prepared)?;
        *self.acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?=AcceptanceAuthority::RecoveryOnly;
        Ok(())
    }

    fn bind_acceptance_identity(&mut self,prepared:&acceptance::PreparedAcceptance)->Result<()> {
        self.healthy_lease()?;
        self.validate_acceptance_source(prepared)?;
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

    pub(super) fn validate_acceptance_source(&self,prepared:&acceptance::PreparedAcceptance)->Result<()> {
        anyhow::ensure!(self.acceptance_source==Some(prepared.proposal.contract.source()?),"baseline qualification belongs to a different issue or PR");
        Ok(())
    }

    fn healthy_lease(&self) -> Result<()> {
        family_budget_hold::require_active(&self.family_budget_wait)?;
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
        let context=self.execution_context(&binding)?;
        let now = millis();
        let mut store = self
            .store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?;
        let checkpoint=
            Checkpoint {
                stage,
                binding: binding.clone(),
                evidence: evidence.to_vec(),
                next: NextAction {
                    reason: reason.into(),
                    action: action.into(),
                    eligible_at_ms,
                },
            };
        if let Some(context)=&context {store.checkpoint_with_execution_context(&self.lease,&checkpoint,context,now)?;}
        else {store.checkpoint(&self.lease,&checkpoint,now)?;}
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

    /// Metadata identifying enrolled recovery, never positive qualification.
    pub(super) fn has_registered_acceptance_context(&self)->Result<bool> {
        self.healthy_lease()?;
        Ok(self.contract.strip_prefix("registered:").is_some_and(|hash|hash.len()==64 && hash.bytes().all(|b|b.is_ascii_hexdigit())))
    }

    /// Original protected frontend request, never a replacement inferred from a title.
    pub(super) fn original_owner_request(&self)->Result<&str> {
        self.healthy_lease()?;
        self.source_request.as_ref().map(|(request,_)|request.as_str())
            .filter(|request|!request.trim().is_empty())
            .context("original owner request unavailable")
    }

    fn execution_context(&self,binding:&Binding)->Result<Option<durable::RecoveryContext>> {
        let Some((request,pr))=&self.source_request else{return Ok(None)};
        Ok(Some(durable::RecoveryContext {version:1,workflow:self.id().into(),repository:self.repository.clone(),
            worktree:self.worktree.canonicalize()?,source_request:request.clone(),source_pr:*pr,
            contract:self.contract.clone(),environment:self.environment.clone(),binding:binding.clone()}))
    }

    /// Check immediately before gh/git side effects. Also pass GitHub the
    /// exact expected SHA; this local check cannot fence an in-flight RPC.
    pub(super) fn validate_before_side_effect(&self) -> Result<()> {
        self.validate_control_state()?;
        checkout_identity::validate(&self.checkout_identity)
    }

    fn validate_mutation_dispatch(&self)->Result<()> {
        self.validate_before_side_effect()?;
        family_dispatch_control::validate_mutation(self)
    }

    // A completed remote mutation must remain recordable if the checkout was
    // replaced while its RPC was in flight. Preserve the old lease/audit gate.
    fn validate_control_state(&self) -> Result<()> {
        self.healthy_lease()?;
        self.audit.protected_health()?;
        self.store
            .lock()
            .map_err(|_| anyhow::anyhow!("workflow store lock poisoned"))?
            .validate_lease(&self.lease, &self.binding, millis())
    }

    /// New notes and ready transitions must honor controls accepted after earlier reads or
    /// Push dispatch. Completion receipts use the weaker lease/binding guard.
    pub(super) fn validate_note_dispatch(&self)->Result<()> {
        self.validate_mutation_dispatch()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
            .require_owner_dispatch(self.id())
    }

    pub(super) fn intent(&self, kind: OperationKind) -> Result<Operation> {
        self.validate_mutation_dispatch()?;
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

    fn bound_push_intent(&self,destination:&str,reference:&str)->Result<Operation> {
        self.validate_mutation_dispatch()?;
        let operation=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
            .begin_bound_push(&self.lease,&self.binding,destination,reference,millis())?;
        self.audit_event("operation_intent",serde_json::json!({"operation_id":operation.id,"operation_kind":operation.kind}))?;
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
        self.validate_mutation_dispatch()?;
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
        self.validate_control_state()?;
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

    pub(super) fn require_independent_native_session(&self,provider:&str,session:&str)->Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .require_independent_native_session(provider,session)
    }

    /// #2451: decide whether this binding may be reviewed again; consumes an
    /// attempt only when admitted. Refusals are audited with their route.
    pub(super) fn admit_review_binding(&self, key: &str, per_attempt_bound: u64, has_findings: bool,
        repair_round_left: bool) -> Result<crate::autoship_workflow::review_binding_cap::Route> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .admit_review_binding(&self.lease,key,per_attempt_bound,has_findings,repair_round_left,millis())
    }
    pub(super) fn charge_review_binding_calls(&self, key: &str, calls: u64) -> Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .charge_review_binding_calls(&self.lease,key,calls,millis())
    }
    /// Durable model admissions charged to this workflow so far.
    pub(super) fn model_calls(&self) -> Result<u64> {
        Ok(self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .snapshot(self.id())?.context("workflow missing")?.model_calls)
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

    /// `Some(at)` while this issue's rolling daily retry model budget is spent.
    pub(super) fn daily_budget_reopens_at(&self)->Result<Option<i64>> {
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
            .daily_budget_reopens_at(self.id(),millis(),durable::retry_policy::daily_model_calls())
    }
    pub(super) async fn begin_acceptance_escalation(&mut self)->Result<Option<durable::acceptance_escalation::Material>> {
        self.validate_before_side_effect()?;self.audit_health().await?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
            .begin_acceptance_escalation(&self.lease,&self.binding,millis())
    }
    pub(super) async fn hold_initial_acceptance(&mut self,reason:&str,action:&str)->Result<()> {
        self.hold_initial_acceptance_custody(reason,action,false).await
    }

    pub(super) async fn hold_initial_acceptance_custody(&mut self,reason:&str,action:&str,physical:bool)->Result<()> {
        self.hold_initial_acceptance_verifier(reason,action,physical,None).await
    }

    pub(super) async fn hold_initial_acceptance_verifier(&mut self,reason:&str,action:&str,physical:bool,plan:Option<&str>)->Result<()> {
        self.healthy_lease()?;
        self.audit_health().await?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
        let mut checkpoint=store.snapshot(self.id())?.context("initial acceptance workflow missing")?.checkpoint;
        let readiness_reassessment=reason=="readiness_assessment_invalid" && action=="await_readiness_input_change"
            && checkpoint.stage==Stage::Reconcile && checkpoint.next.reason=="inputs_changed";
        anyhow::ensure!(checkpoint.binding==self.binding && (matches!(checkpoint.stage,Stage::Intake|Stage::Scope) || readiness_reassessment),
            "initial acceptance hold requires unchanged unenrolled scope");
        checkpoint.stage=Stage::Scope;
        checkpoint.next=crate::autoship_workflow::NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:0};
        store.checkpoint_verifier_recovery_hold(&self.lease,&checkpoint,millis(),physical,plan)?;
        Ok(())
    }

    /// Qualification recovery holds already-enrolled work without turning its
    /// candidate binding into an unenrolled initial scope or granting a builder.
    pub(super) async fn hold_enrolled_acceptance(&mut self,prepared:&acceptance::PreparedAcceptance,reason:&str,action:&str)->Result<()> {
        self.hold_enrolled_acceptance_custody(prepared,reason,action,false).await
    }

    pub(super) async fn hold_enrolled_acceptance_custody(&mut self,prepared:&acceptance::PreparedAcceptance,reason:&str,action:&str,physical:bool)->Result<()> {
        self.healthy_lease()?;
        self.validate_acceptance_source(prepared)?;
        self.audit_health().await?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
        let mut checkpoint=store.snapshot(self.id())?.context("enrolled acceptance workflow missing")?.checkpoint;
        anyhow::ensure!(checkpoint.binding==self.binding && matches!(checkpoint.stage,
            Stage::Intake|Stage::Scope|Stage::Reconcile|Stage::Build|Stage::Verify|Stage::Review),"enrolled acceptance hold binding/stage changed");
        *self.acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?=AcceptanceAuthority::RecoveryOnly;
        checkpoint.stage=Stage::Scope;
        checkpoint.next=crate::autoship_workflow::NextAction {reason:reason.into(),action:action.into(),eligible_at_ms:0};
        if reason=="acceptance_verifier_infrastructure_unavailable" && action=="await_verifier_infrastructure_recovery" {
            if let Some(cue)=store.retained_qualification_cue(self.id(),&self.binding)? {
                store.hold_retained_infrastructure(&self.lease,&self.binding,&cue.key,millis())?;
                return Ok(());
            }
        }
        if reason=="acceptance_verifier_evidence_unknown" && action=="await_original_evidence_recovery" {
            if let Some(cue)=store.retained_qualification_cue(self.id(),&self.binding)? {
                store.hold_semantic_source_unavailable(&self.lease,&self.binding,&cue.key,millis())?;
                return Ok(());
            }
        }
        store.checkpoint_native_custody_hold(&self.lease,&checkpoint,millis(),physical)?;
        Ok(())
    }

    pub(super) async fn hold_enrolled_setup_failure(&mut self,prepared:&acceptance::PreparedAcceptance,setup:&durable::RetainedSetupDependency)->Result<()> {
        self.healthy_lease()?;self.validate_acceptance_source(prepared)?;self.audit_health().await?;
        let candidate=self.failed_setup_candidate().await.ok();
        *self.acceptance_authority.lock().map_err(|_|anyhow::anyhow!("baseline qualification lock poisoned"))?=AcceptanceAuthority::RecoveryOnly;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("enrolled setup store poisoned"))?;
        let cue=store.retained_qualification_cue(self.id(),&self.binding)?.context("retained setup cue missing")?;
        store.hold_retained_setup_failure(&self.lease,&self.binding,&cue.key,setup,candidate.as_ref(),millis())
    }

    /// A fresh issue must not keep spending once an authorized linked PR exists.
    /// PR-bound recovery uses its original independent execution authority.
    pub(super) async fn require_fresh_issue(&self,repo:&Path)->Result<()> {
        self.validate_before_side_effect()?;
        if self.pr()?.is_some() {return Ok(());}
        let Some((_,number))=self.id().rsplit_once('#') else {return Ok(())};
        let issue=number.parse::<u64>()?;
        if let Err(error)=linked_claims::require_absence(repo,self.repository(),issue).await {
            if let Some(hold)=error.downcast_ref::<linked_claims::Hold>() {
                self.hold_linked_claim(*hold).await?;
            }
            return Err(error);
        }
        Ok(())
    }

    async fn hold_linked_claim(&self,hold:linked_claims::Hold)->Result<()> {
        self.healthy_lease()?;
        self.audit_health().await?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?;
        let mut checkpoint=store.snapshot(self.id())?.context("fresh issue workflow missing")?.checkpoint;
        anyhow::ensure!(checkpoint.binding==self.binding
            && !matches!(checkpoint.stage,Stage::Merged|Stage::Retired),"fresh claim hold binding/stage changed");
        if checkpoint.next.reason==hold.reason() && checkpoint.next.action==hold.action() {return Ok(());}
        checkpoint.next=crate::autoship_workflow::NextAction {
            reason:hold.reason().into(),action:hold.action().into(),eligible_at_ms:millis()+300_000,
        };
        store.checkpoint(&self.lease,&checkpoint,millis())?;
        Ok(())
    }

    pub(super) fn check_resource_policy(&self) -> Result<()> {
        self.validate_before_side_effect()?;
        augmentagent_channel_core::run_resources::check_requirement("workflow_action", Some(&self.audit))
    }

    pub(super) fn semantic_preparation_source(&self)->Result<Option<acceptance::PreparedAcceptance>> {
        self.validate_before_side_effect()?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("semantic preparation store poisoned"))?;
        semantic_preparation_source(&mut store,&self.lease,&self.worktree,&self.repository)
    }

    pub(super) fn retained_preparation(&self)->Result<Option<acceptance::PreparedAcceptance>> {
        self.validate_before_side_effect()?;
        let reference=self.store.lock().map_err(|_|anyhow::anyhow!("retained preparation store poisoned"))?
            .retained_qualification_history(self.id(),&self.binding)?;
        let Some(reference)=reference else{return Ok(None)};
        let checked=(||->Result<Option<acceptance::PreparedAcceptance>> {
            let source=self.acceptance_source.context("retained preparation source identity missing")?;
            let control=acceptance::control_directories(&self.repository,self.id())?.workflow_acceptance;
            if acceptance::retained_qualification_superseded(&control,source,&reference)? {return Ok(None);}
            let prepared=match self.semantic_preparation_source()? {
                Some(prepared)=>prepared,
                None=> {
                    if acceptance::retained_qualification_superseded(&control,source,&reference)? {return Ok(None);}
                    match acceptance::retained_prepared(&self.worktree,&control,source) {
                        Ok(Some(prepared))=>prepared,
                        Ok(None)=>anyhow::bail!("retained prepared evidence is unavailable"),
                        Err(error)=>return Err(error),
                    }
                },
            };
            anyhow::ensure!(self.retained_qualification_reference(&prepared)?.as_ref()==Some(&reference),"retained preparation source changed");
            Ok(Some(prepared))
        })();
        match checked {
            Ok(prepared)=>Ok(prepared),
            Err(error) if self.semantic_source_waiting()?=>Err(error),
            Err(error)=>Err(self.retained_preparation_failure(error)?),
        }
    }

    pub(super) fn preparation_failure(&self,error:anyhow::Error)->Result<anyhow::Error> {
        self.validate_before_side_effect()?;
        let retained=self.store.lock().map_err(|_|anyhow::anyhow!("retained preparation store poisoned"))?
            .retained_qualification_cue(self.id(),&self.binding)?.is_some();
        if !retained {return Ok(error);}
        if self.semantic_source_waiting()? {return Ok(SemanticSourceUnavailable.into());}
        self.retained_preparation_failure(error)
    }

    async fn failed_setup_candidate(&self)->Result<Binding> {
        self.validate_before_side_effect()?;
        let checkpoint=self.store.lock().map_err(|_|anyhow::anyhow!("failed setup store poisoned"))?
            .snapshot(self.id())?.context("failed setup workflow missing")?.checkpoint;
        anyhow::ensure!(checkpoint.binding==self.binding,"failed setup checkpoint changed");
        let observed=git_binding(&self.worktree,&self.contract,&self.environment,&checkpoint.evidence).await?;
        anyhow::ensure!(observed.head==self.binding.head && observed.contract==self.binding.contract,"failed setup head/contract changed");
        self.validate_before_side_effect()?;Ok(observed)
    }

    pub(super) async fn preparation_failure_at_current(&self,error:anyhow::Error)->Result<anyhow::Error> {
        let error=if acceptance::retained_setup_failure(&error).is_some() && !self.semantic_source_waiting()? {
            match self.failed_setup_candidate().await {
                Ok(candidate)=>acceptance::stamp_retained_setup_candidate(error,candidate),
                Err(_)=>error,
            }
        }else{error};
        self.preparation_failure(error)
    }

    pub(super) fn retained_preparation_failure(&self,error:anyhow::Error)->Result<anyhow::Error> {
        self.validate_before_side_effect()?;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("retained preparation store poisoned"))?;
        store.validate_dispatch(&self.lease,&self.binding,millis())?;
        let cue=store.retained_qualification_cue(self.id(),&self.binding)?.context("retained preparation source reference missing")?;
        record_semantic_source_failure(&mut store,&self.lease,&self.binding,&cue.key,&error)?;
        Ok(SemanticSourceUnavailable.into())
    }

    /// A completed handoff stops overriding preparation, but remains proof
    /// that these exact prepared bytes already had an original witness.
    pub(super) fn retained_qualification_reference(&self,prepared:&acceptance::PreparedAcceptance)->Result<Option<durable::RetainedWitnessReference>> {
        self.validate_before_side_effect()?;self.validate_acceptance_source(prepared)?;
        let expected=format!("{:x}",Sha256::digest(serde_json::to_vec(prepared)?));
        let reference=self.store.lock().map_err(|_|anyhow::anyhow!("retained qualification store poisoned"))?
            .retained_qualification_history(self.id(),&self.binding)?.filter(|reference|reference.prepared==expected);
        if let Some(reference)=&reference {
            anyhow::ensure!(reference.contract==prepared.baseline.registration.contract_hash(),"retained qualification contract reference changed");
        }
        Ok(reference)
    }

    pub(super) fn semantic_source_waiting(&self)->Result<bool> {
        self.store.lock().map_err(|_|anyhow::anyhow!("semantic preparation store poisoned"))?
            .semantic_source_waiting(self.id(),&self.binding)
    }

    pub(super) async fn checkpoint_qualification_source(&mut self,prepared:&acceptance::PreparedAcceptance)->Result<()> {
        let evidence=self.store.lock().map_err(|_|anyhow::anyhow!("qualification store poisoned"))?
            .snapshot(self.id())?.context("qualification workflow missing")?.checkpoint.evidence;
        self.bind_acceptance_for_recovery(prepared)?;
        self.checkpoint(Stage::Scope,"baseline_qualification_started","qualify_original_baseline",0,&evidence).await
    }

    pub(super) fn epoch_attempts(&self,purpose:&str,epoch:&str)->Result<u32> {
        self.healthy_lease()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .epoch_attempts(&self.lease,purpose,epoch,millis())
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
    pub(super) fn recorded_scope_stage_response(&self,key:&str)->Result<Option<String>> {
        self.validate_before_side_effect()?;self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .recorded_scope_stage_response(&self.lease,&self.binding,key,millis())
    }
    pub(super) fn scope_stage_response_keys(&self,after:&str)->Result<Vec<String>> {
        self.validate_before_side_effect()?;self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .scope_stage_response_keys(&self.lease,&self.binding,after,millis())
    }
    /// #2373: audited, once per key. True when this call recorded it.
    pub(super) fn record_reviewer_fallback(&self,key:&str,payload:&serde_json::Value)->Result<bool> {
        self.validate_before_side_effect()?;self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.record_reviewer_fallback(&self.lease,&self.binding,key,payload,millis())
    }
    pub(super) fn record_scope_stage_response(&self,key:&str,response:&str)->Result<()> {
        self.validate_before_side_effect()?;self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?.record_scope_stage_response(&self.lease,&self.binding,key,response,millis())
    }

    pub(super) fn with_qualification_evidence<T>(&self,
        read:impl FnOnce(&Path,&durable::VerifierEvidenceInspection<'_>)->Result<T>)->Result<T> {
        self.healthy_lease()?;self.audit.protected_health()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow evidence lock poisoned"))?
            .with_qualification_evidence(&self.lease,&self.binding,millis(),read)
    }

    pub(super) fn record_verifier_evidence(&self,key:&str,kind:&str,body:&str)->Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .record_verifier_evidence(&self.lease,&self.binding,key,kind,body,millis())
    }

    pub(super) fn original_verifier_recovery_plan(&self)->Result<Option<(String,String)>> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .original_verifier_recovery_plan(&self.lease,&self.binding,millis())
    }

    pub(super) async fn complete_original_verifier_recovery(&self,plan:&str,result:&str)->Result<()> {
        self.validate_before_side_effect()?;self.audit_health().await?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .complete_original_verifier_recovery(&self.lease,&self.binding,plan,result,millis())
    }

    pub(super) fn verifier_evidence_page(&self,kind:&str,after:i64)->Result<Vec<durable::VerifierEvidence>> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .verifier_evidence_page(&self.lease,&self.binding,kind,after,millis())
    }

    pub(super) fn verifier_evidence(&self,key:&str,kind:&str)->Result<Option<String>> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .verifier_evidence(&self.lease,&self.binding,key,kind,millis())
    }

    pub(super) fn scope_stage_response_pair(&self,key:&str,provenance_key:&str)->Result<Option<(String,String)>> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .scope_stage_response_pair(&self.lease,&self.binding,key,provenance_key,millis())
    }

    pub(super) fn initial_correction_evidence(&self,root:&Path,source:crate::autoship_contract::AcceptanceSource,base:&str,proposal_sha256:&str)->Result<serde_json::Value> {
        self.validate_before_side_effect()?;
        let control=acceptance::control_directories(self.repository(),self.id())?.workflow_acceptance;
        let mut store=self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?;
        let custody=store.baseline_native_custody(&self.lease,&self.binding,millis())?;
        acceptance::initial_correction_evidence(root,&control,source,base,proposal_sha256,&self.binding,&custody,&store.inspect_verifier_evidence(self.id()))
    }

    pub(super) fn baseline_native_custody(&self)->Result<Vec<durable::NativeScopeCustody>> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .baseline_native_custody(&self.lease,&self.binding,millis())
    }

    pub(super) fn record_scope_stage_response_pair(&self,key:&str,response:&str,provenance_key:&str,provenance:&str)->Result<()> {
        self.validate_before_side_effect()?;
        self.store.lock().map_err(|_|anyhow::anyhow!("workflow lock poisoned"))?
            .record_scope_stage_response_pair(&self.lease,&self.binding,key,response,provenance_key,provenance,millis())
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

    pub(super) async fn defer_main_policy_reconciliation(&mut self, main: &str) -> Result<i64> {
        anyhow::ensure!(
            matches!(main.len(), 40 | 64) && main.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid main policy dependency"
        );
        self.validate_before_side_effect()?;
        self.audit_health().await?;
        anyhow::ensure!(
            self.pending_operations()?.is_empty(),
            "pending publication must reconcile before main integration"
        );
        let eligible = millis().saturating_add(300_000);
        self.audit_event(
            "hosted_ci_main_dependency",
            serde_json::json!({"observed_main":main,"shipping_authority":false}),
        )?;
        self.checkpoint(
            Stage::Reconcile,
            "review_main_changed",
            "reconcile_main",
            eligible,
            &[],
        )
        .await?;
        Ok(eligible)
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

    /// #2315: a hosted CI code failure takes a repair round now or waits on
    /// the #2301 backoff ladder, on the main-reconciliation cue. Never the
    /// infrastructure retry, never terminal.
    pub(super) async fn code_repair_round(&mut self,reason:&str,immediate:bool)
        ->Result<crate::autoship_workflow::ci_code_repair::Ladder> {
        self.validate_before_side_effect()?;
        self.audit_health().await?;
        let ladder=self.store.lock().map_err(|_|anyhow::anyhow!("workflow code repair store poisoned"))?
            .code_repair_round(&self.lease,&self.binding,reason,immediate,millis())?;
        self.audit_event("hosted_ci_code_failure",match ladder {
            crate::autoship_workflow::ci_code_repair::Ladder::Repair(round)=>serde_json::json!({
                "reason":reason,"round":round,"action":"merge_main_rebuild_and_repair"}),
            crate::autoship_workflow::ci_code_repair::Ladder::Wait(eligible)=>serde_json::json!({
                "reason":reason,"action":"reconcile_main","eligible_at_ms":eligible}),
        })?;
        Ok(ladder)
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
            .await?;
        // #2354: the checkpoint is durable and the next step is not yet
        // claimed, so a pending deploy may restart the worker here. Waiting
        // and terminal stages end their tick normally instead.
        if !matches!(stage,Stage::Merged|Stage::Retired|Stage::Held|Stage::AwaitChecks) {
            crate::autoship_worker::safe_point().await;
        }
        Ok(())
    }

    /// Publish detached resumed work with a remote-head precondition. A lost
    /// response is reconciled against ls-remote before any retry. No force push
    /// is needed; ancestry and the lease both protect intervening remote edits.
    /// The actual fresh publication entry, separately callable by its
    /// disposable-remote regression without invoking a model.
    pub(super) async fn push_fresh(&mut self,repo:&Path,branch:&str,automatic:bool)->Result<String> {
        self.push_checked(branch,Some((repo,automatic))).await
    }

    async fn require_fresh_publication_route(&mut self,repo:&Path,automatic:bool)->Result<()> {
        self.require_fresh_issue(repo).await?;
        let source=self.id().rsplit_once('#').map(|(_,number)|number.parse::<u64>()).transpose()?;
        if let Some(pr)=self.pr()? {
            if !manual_routing::permit_mutation(repo,pr,source,automatic,self).await? {return Err(manual_routing::Hold.into());}
        }else {
            let issue=source.context("fresh publication source unavailable")?;
            if automatic && !manual_routing::issue_allowed(repo,self.repository(),issue).await {
                self.transition(Stage::Held,"automatic_pr_routing_held","retry_automatic_pr_routing",millis()+300_000).await?;
                return Err(manual_routing::Hold.into());
            }
        }
        Ok(())
    }

    pub(super) async fn push(&mut self, branch: &str) -> Result<String> {
        self.push_checked(branch,None).await
    }

    async fn push_checked(&mut self,branch:&str,fresh:Option<(&Path,bool)>)->Result<String> {
        if let Some((repo,automatic))=fresh {
            let pending_push=self.store.lock().map_err(|_|anyhow::anyhow!("workflow store lock poisoned"))?
                .pending_operations(self.id())?.iter().any(|operation|operation.kind==OperationKind::Push);
            // Existing uncertain pushes may first observe their remote outcome.
            // Every newly dispatched push still rechecks below.
            if !pending_push {self.require_fresh_publication_route(repo,automatic).await?;}
        }
        self.transition(Stage::Publish, "publishing_revision", "push", millis())
            .await?;
        let destination=push_destination::Resolved::resolve(&self.worktree,&self.repository).await?;
        let target=format!("refs/heads/{branch}");
        let remote=destination.head(&self.worktree,&self.repository,&target).await?.unwrap_or_default();
        if remote==self.binding.head {
            let operation=self.bound_push_intent(destination.destination(),&target)?;
            self.observe_remote(&operation,&format!("git-head:{remote}"),None)?;
            return Ok(remote);
        }
        if !remote.is_empty() {
            let (ancestor,_,_)=run("git",&["merge-base","--is-ancestor",&remote,&self.binding.head],&self.worktree).await?;
            anyhow::ensure!(ancestor,"remote PR changed; reconcile before publishing preserved work");
        }
        // The actual transport still uses origin, preserving its credentials,
        // pushInsteadOf rules and installed CCAT hook. Its pre-push wrapper
        // refuses a destination change before any pack can be sent.
        let hook=destination.hook(&self.worktree,self.audit.path().parent().context("private push control unavailable")?).await?;
        self.validate_before_side_effect()?;
        self.validate_binding(&self.binding.clone()).await?;
        if let Some((repo,automatic))=fresh {self.require_fresh_publication_route(repo,automatic).await?;}
        let operation=self.bound_push_intent(destination.destination(),&target)?;
        let expected=format!("--force-with-lease={target}:{remote}");
        let refspec=format!("{}:{target}",operation.binding.head);
        let (pushed,_,_)=run("git",&["-c",hook.config(),"push",&expected,"origin",&refspec],&self.worktree).await?;
        let observed=destination.head(&self.worktree,&self.repository,&target).await?;
        anyhow::ensure!(observed.as_deref()==Some(operation.binding.head.as_str()),
            "push outcome not verified; preserved pending intent (command success: {pushed})");
        let head=observed.context("Push observation unavailable")?;
        self.observe_remote(&operation,&format!("git-head:{head}"),None)?;
        Ok(head)
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
        automatic: bool,
        source: Option<u64>,
    ) -> Result<bool> {
        if dry_run {
            return Ok(true);
        }
        if !manual_routing::permit_mutation(repo_root,pr,source,automatic,self).await? {
            return Ok(false);
        }
        let head = self.push(branch).await?;
        if !manual_routing::permit_mutation(repo_root,pr,source,automatic,self).await? {return Ok(false);}
        self.validate_note_dispatch()?;
        let _ = write_optional_pr_note(
            repo_root,
            &pr.to_string(),
            &round_comment(round, kind, findings, &head),
        )
        .await;
        Ok(true)
    }

    /// Caller has waited for worker teardown. Stage/evidence must already be
    /// checkpointed; releasing here never deletes the durable worktree.
    pub(super) fn finish(mut self) -> Result<()> {
        if !self.released && !self.family_wait_released.load(Ordering::SeqCst) { self.release_after_teardown()?; }
        Ok(())
    }

    pub(super) fn finish_ci_deferral(&mut self) -> Result<()> {
        let checkpoint = self.store.lock().map_err(|_|anyhow::anyhow!("workflow retry store poisoned"))?
            .snapshot(self.id())?.context("retry workflow missing")?.checkpoint;
        anyhow::ensure!(matches!((checkpoint.next.reason.as_str(),checkpoint.next.action.as_str()),
            ("verification_infrastructure_retry","retry_verification") | ("review_main_changed","reconcile_main")),
            "no durable CI deferral to release");
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
        if !self.released && !self.family_wait_released.load(Ordering::SeqCst) {
            // Best-effort final usage and retry scheduling, never release or
            // cleanup: dropping a task is not descendant teardown evidence.
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
                if let Err(error)=store.record_retry_completion(&self.lease,millis()) {
                    warn!(workflow=%self.lease.workflow,"autoship retry schedule retained without new authority: {error}");
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
/// Shared with free selection. A preview cannot invent a different definition
/// of changed input from productive acquisition or normalize an unknown shape.
fn recovery_context_matches(context:&durable::RecoveryContext,request:&str,worktree:&Path,environment:&str,initial_wait:bool)->bool {
    context.source_request==request
        && context.worktree.canonicalize().ok()==worktree.canonicalize().ok()
        && (context.contract.starts_with("registered:") || context.environment==environment
            || initial_wait && recovery::same_initial_verifier_environment(&context.environment,environment))
}
async fn existing_candidate_binding(worktree:&Path,contract:&str,environment:&str,existing:Option<&durable::Snapshot>)->Result<Binding> {
    let receipts=existing.map(|snapshot|snapshot.checkpoint.evidence.as_slice()).unwrap_or(&[]);
    let binding=git_binding(worktree,contract,environment,receipts).await?;
    if existing.is_some_and(|snapshot|snapshot.checkpoint.binding!=binding) {
        git_binding(worktree,contract,environment,&[]).await
    }else {Ok(binding)}
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
    // #2444: the repository identity cannot change while the process runs.
    let name = gh_budget::repository_name(repo_root, || async {
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
    Ok(name.to_owned())
    }).await?;
    repository_identity::Policy::from_env()?.canonical(&name)
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

/// Read-only identity shared by the resume producer and recovery inspector.
pub(super) async fn resume_worktree_path(repo_root:&Path,pr:u64)->Result<PathBuf> {
    anyhow::ensure!(pr>0,"PR identity required");
    let primary=workspace::primary_root(repo_root).await?;
    let path=primary.join(".self-improve-worktrees").join(format!("resume-pr-{pr}"));
    recovery::checkout_path(&primary,&path)?;
    Ok(path)
}

pub(super) async fn prepare_resume_worktree_at_head(
    repo_root:&Path, pr:u64, branch:&str, expected_head:Option<&str>,
)->Result<PathBuf> {
    let path=resume_worktree_path(repo_root,pr).await?;
    let exists = path.try_exists()?;
    if exists {
        recovery::existing_checkout(repo_root, &path).await?;
    }
    let (valid, _, _) = run("git", &["check-ref-format", "--branch", branch], repo_root).await?;
    anyhow::ensure!(valid, "invalid resumed PR branch");
    let (ok, _, _) = workspace::preparation_git(repo_root, &["fetch", "--no-write-fetch-head", "origin", branch]).await?;
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
    let (ok, _, error) = workspace::preparation_git(
        repo_root,
        &[
            "worktree",
            "add",
            "--detach",
            &path.to_string_lossy(),
            &fetched,
        ],
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
    let now = millis();
    if let Some(recorded) = store.retained_candidate_head(repository, pr, branch,
        &mismatch.published, &mismatch.actual, now)? {
        // Exact-head binding: only the recorded candidate, and only when it
        // extends the publication this workflow itself confirmed.
        let (descendant, _, _) = run("git", &["merge-base", "--is-ancestor", &mismatch.published, &recorded], &mismatch.path).await?;
        if descendant && git_evidence(&mismatch.path, &["rev-parse", "HEAD"]).await? == recorded {
            return Ok(mismatch.path.clone());
        }
    }
    // #2279 — an unrecorded or non-descendant candidate is never published.
    // Retrying the same refusal every tick held the PR forever; once the
    // workflow is idle and due, set the candidate aside under a durable ref
    // and resume from the authorized PR head, which re-verifies on main.
    if !store.retained_candidate_due(repository, pr, branch, now)? { return Err(error); }
    drop(store);
    rebuild_from_published(mismatch, pr).await
}

async fn rebuild_from_published(mismatch: &RetainedHeadMismatch, pr: u64) -> Result<PathBuf> {
    let path = &mismatch.path;
    let (clean, status, _) = run("git", &["status", "--porcelain", "--untracked-files=no"], path).await?;
    anyhow::ensure!(clean && status.trim().is_empty(),
        "retained candidate has uncommitted edits; candidate preserved");
    anyhow::ensure!(git_evidence(path, &["rev-parse", "HEAD"]).await?.eq_ignore_ascii_case(&mismatch.actual),
        "retained candidate changed during rebuild; candidate preserved");
    let preserved = format!("refs/autoship/preserved/pr-{pr}/{}", mismatch.actual);
    let (saved, _, _) = run("git", &["update-ref", &preserved, &mismatch.actual], path).await?;
    anyhow::ensure!(saved, "cannot preserve retained candidate; candidate preserved");
    let (switched, _, error) = run("git", &["checkout", "--quiet", "--detach", &mismatch.published], path).await?;
    anyhow::ensure!(switched, "cannot resume from authorized PR head: {error}; candidate preserved at {preserved}");
    anyhow::ensure!(git_evidence(path, &["rev-parse", "HEAD"]).await?.eq_ignore_ascii_case(&mismatch.published),
        "rebuild did not reach the authorized PR head; candidate preserved at {preserved}");
    warn!(pr, preserved = %preserved, published = %mismatch.published,
        "retained candidate is not a recorded descendant of its publication; set aside and resuming from the authorized PR head");
    Ok(path.clone())
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

    #[test]
    fn daily_ledger_holds_fresh_starts_but_not_in_flight_issues() {
        let data=tempfile::tempdir().unwrap();let path=data.path().join("workflow.db");
        {
            let mut store=WorkflowStore::open(&path).unwrap();
            // #7: #2360's shape on 2026-11-21. #8: a scope-stage start with no PR.
            for (issue,stage,pr) in [(7,Stage::Verify,Some(2372)),(8,Stage::Scope,None)] {
                let id=durable::workflow_id("owner/repo",issue).unwrap();
                let binding=Binding {head:"h".into(),contract:"c".into(),evidence_hash:"e".into()};
                store.register(&id,"fixture",binding.clone(),0).unwrap();
                let lease=store.claim(&id,"fixture",0,1).unwrap().unwrap();
                if let Some(pr)=pr {store.attach_pr_in_repo(&lease,"owner/repo",pr,0).unwrap();}
                store.checkpoint(&lease,&durable::Checkpoint {stage,binding,evidence:vec![],
                    next:durable::NextAction {reason:"integration_checks_required".into(),action:"verify_integrated_head".into(),eligible_at_ms:0}},0).unwrap();
                store.release(&lease,0).unwrap();
            }
        }
        let controls=FreshIssueControls::open_at(&path).unwrap();
        let ledger=AttemptLedger {day:100,issues:vec![7,8,9]};
        assert!(!controls.daily_ledger_holds(&ledger,100,"owner/repo",7,false),"in-flight verify sat behind the ledger");
        assert!(controls.daily_ledger_holds(&ledger,100,"owner/repo",8,false),"a second fresh start the same day is still capped");
        assert!(controls.daily_ledger_holds(&ledger,100,"owner/repo",9,false),"no workflow is a fresh start");
        assert!(!controls.daily_ledger_holds(&ledger,100,"owner/repo",9,true),"a due retry keeps its existing exemption");
        assert!(!controls.daily_ledger_holds(&ledger,100,"owner/repo",10,false),"unlisted issues are untouched");
        assert!(!controls.daily_ledger_holds(&ledger,101,"owner/repo",8,false),"the ledger resets each UTC day");
    }

    #[test]
    fn source_failure_diagnostics_survive_owned_binding_rotation_and_deduplicate_each_binding() {
        let data=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&data.path().join("workflow.db")).unwrap();
        let id="owner/repo#7";let first=Binding {head:"original-head".into(),contract:"original-contract".into(),evidence_hash:"first-evidence".into()};
        store.register(id,"fixture",first.clone(),millis()).unwrap();let lease=store.claim(id,"fixture",millis(),60_000).unwrap().unwrap();
        let context=|binding:&Binding|durable::RecoveryContext {version:1,workflow:id.into(),repository:"owner/repo".into(),worktree:data.path().into(),
            source_request:"original owner request".into(),source_pr:None,contract:"original owner request".into(),environment:"fixture".into(),binding:binding.clone()};
        store.record_recovery_context(&lease,&first,&context(&first),millis()).unwrap();
        let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;cp.stage=Stage::Scope;
        cp.next=durable::NextAction {reason:"acceptance_contract_invalid".into(),action:"await_acceptance_input_change".into(),eligible_at_ms:0};
        store.checkpoint(&lease,&cp,millis()).unwrap();store.release(&lease,millis()).unwrap();
        let reference=durable::RetainedWitnessReference {plan:"a".repeat(64),result:"b".repeat(64),data:"c".repeat(64),prepared:"d".repeat(64),contract:"e".repeat(64)};
        let held=store.snapshot(id).unwrap().unwrap();
        assert!(store.recover_retained_qualification(&held,None,millis(),|_|Ok(Some(reference.clone()))).unwrap());
        let cue=store.semantic_reconsideration(id,&first).unwrap().unwrap();
        for turn in 0..2 {
            let lease=store.claim_productive_for_binding(id,"fixture",millis(),60_000,&first).unwrap().unwrap();
            let binding=if turn==0 {first.clone()}else{Binding {evidence_hash:"second-evidence".into(),..first.clone()}};
            if turn==1 {
                let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;cp.binding=binding.clone();
                cp.next=durable::NextAction {reason:"baseline_qualification_started".into(),action:"qualify_original_baseline".into(),eligible_at_ms:0};
                store.checkpoint_with_execution_context(&lease,&cp,&context(&binding),millis()).unwrap();
            }
            let error=anyhow::anyhow!("identical unavailable original source");
            record_semantic_source_failure(&mut store,&lease,&binding,&cue.key,&error)
                .expect("same private cause on a new owned binding collided with the earlier immutable diagnostic");
            let events=store.events(id).unwrap();
            record_semantic_source_failure(&mut store,&lease,&binding,&cue.key,&error).unwrap();
            assert_eq!(store.events(id).unwrap(),events,"duplicate diagnostic changed workflow history");
            store.release(&lease,millis()).unwrap();
            if turn==0 {
                let held=store.snapshot(id).unwrap().unwrap();
                assert!(store.recover_retained_qualification(&held,None,millis(),|_|Ok(Some(reference.clone()))).unwrap());
            }
        }
        assert_eq!(store.snapshot(id).unwrap().unwrap().model_calls,0);
        assert_eq!(store.events(id).unwrap().iter().filter(|(_,kind,_,_)|kind=="original_semantic_witness_recovered").count(),1);
    }
    #[tokio::test]
    async fn retained_recovery_restart_after_settlement_keeps_qualification_gate() {
        let (repo,control,prepared)=acceptance::qualification_fixture();let state=control.path().join("workflow.db");
        let start=||Start {repo_root:repo.path(),worktree:repo.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
            branch:"fixture",contract:"original owner request",environment:"frontend environment"};
        let mut original=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().unwrap();
        original.transition(Stage::Scope,"acceptance_contract_invalid","await_acceptance_input_change",0).await.unwrap();
        let id=original.id().to_owned();original.finish().unwrap();
        let reference=durable::RetainedWitnessReference {plan:"a".repeat(64),result:"b".repeat(64),data:"c".repeat(64),
            prepared:format!("{:x}",Sha256::digest(serde_json::to_vec(&prepared).unwrap())),contract:prepared.baseline.registration.contract_hash().into()};
        {
            let mut store=WorkflowStore::open(&state).unwrap();let held=store.snapshot(&id).unwrap().unwrap();
            assert!(store.recover_retained_qualification(&held,None,millis(),|_|Ok(Some(reference))).unwrap());
        }
        let guard=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().unwrap();
        {
            let mut store=guard.store.lock().unwrap();
            // Script only the store completion boundary, without qualifying
            // the guard. Model admission must still recheck real evidence.
            assert!(store.settle_retained_qualification(&guard.lease,&guard.binding,millis(),|_,_|Ok(())).unwrap());
            assert!(store.semantic_reconsideration(&id,&guard.binding).unwrap().is_none());
            assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.next.reason,"original_semantic_witness_recovered");
        }
        guard.finish().unwrap();
        let restarted=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().expect("settled recovery checkpoint was parked after restart");
        assert!(matches!(*restarted.acceptance_authority.lock().unwrap(),AcceptanceAuthority::RecoveryOnly),"historical completion bypassed current qualification after restart");
        let store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls,0);
        assert_eq!(store.events(&id).unwrap().iter().filter(|(_,kind,_,_)|kind=="original_semantic_witness_recovered").count(),1);
        restarted.finish().unwrap();
    }

    #[tokio::test]
    async fn retained_recovery_keeps_recovery_only_authority_across_a_changed_candidate_head() {
        for (registered,owner_changed) in [(false,false),(true,false),(false,true),(true,true)] {
            let (repo,control,prepared)=acceptance::qualification_fixture();let state=control.path().join("workflow.db");
            let start=||Start {repo_root:repo.path(),worktree:repo.path(),repository:"owner/repo",source_issue:None,pr:Some(840),
                branch:"fixture",contract:"original owner request",environment:"frontend environment"};
            let mut original=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().unwrap();
            if registered {original.bind_acceptance_for_recovery(&prepared).unwrap();}
            original.checkpoint(Stage::Scope,"acceptance_contract_invalid","await_acceptance_input_change",0,&[]).await.unwrap();
            let workflow=original.id().to_owned();let binding=original.binding().clone();original.finish().unwrap();
            // Synthetic data identities isolate binding/authority assembly;
            // this callback neither validates a witness nor grants approval.
            let reference=durable::RetainedWitnessReference {plan:"a".repeat(64),result:"b".repeat(64),data:"c".repeat(64),
                prepared:"d".repeat(64),contract:prepared.baseline.registration.contract_hash().into()};
            let cue={
                let mut store=WorkflowStore::open(&state).unwrap();let held=store.snapshot(&workflow).unwrap().unwrap();
                assert!(store.recover_retained_qualification(&held,None,millis(),|_|Ok(Some(reference.clone()))).unwrap());
                store.semantic_reconsideration(&workflow,&binding).unwrap().unwrap()
            };
            git_evidence(repo.path(),&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false",
                "-c","core.hooksPath=/dev/null","commit","--allow-empty","-qm","candidate integration"]).await.unwrap();
            let mut next=start();if owner_changed {next.contract="changed owner request";}
            let resumed=WorkflowGuard::acquire_fixture(&state,next).await.unwrap().unwrap();
            assert_ne!(resumed.binding().head,binding.head);
            assert_eq!(matches!(*resumed.acceptance_authority.lock().unwrap(),AcceptanceAuthority::RecoveryOnly),!owner_changed,
                "current owner request did not determine recovery-only authority; registered={registered}, owner_changed={owner_changed}");
            let store=WorkflowStore::open(&state).unwrap();
            assert_eq!(store.semantic_reconsideration(&workflow,resumed.binding()).unwrap(),(!owner_changed).then_some(cue));
            let context=store.recovery_context(&workflow,resumed.binding()).unwrap().unwrap();
            assert_eq!(context.source_request,if owner_changed {"changed owner request"}else{"original owner request"});assert_eq!(context.binding,*resumed.binding());
            let snapshot=store.snapshot(&workflow).unwrap().unwrap();assert_eq!(snapshot.model_calls,0);assert_eq!(snapshot.tokens,0);
            assert_eq!(store.events(&workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="original_semantic_witness_recovered").count(),1);
            resumed.finish().unwrap();
        }
    }

    #[tokio::test]
    async fn semantic_recovery_reuses_registered_context_without_builder_authority() {
        let (repo,control,prepared)=acceptance::qualification_fixture();let state=control.path().join("workflow.db");
        let start=||Start {repo_root:repo.path(),worktree:repo.path(),repository:"owner/repo",source_issue:None,pr:Some(840),branch:"fixture",contract:"original owner request",environment:"frontend environment"};
        let mut original=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().unwrap();
        original.bind_acceptance_for_recovery(&prepared).unwrap();
        original.checkpoint(Stage::Scope,"acceptance_verifier_evidence_unknown","await_original_evidence_recovery",0,&[]).await.unwrap();
        let binding=original.binding().clone();let workflow=original.id().to_owned();original.finish().unwrap();
        {
            let mut store=WorkflowStore::open(&state).unwrap();let held=store.snapshot(&workflow).unwrap().unwrap();
            assert!(store.recover_semantic_qualification(&held,None,&"b".repeat(64),millis(),|_,_|Ok(Some("c".repeat(64)))).unwrap());
        }
        let mut resumed=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().unwrap();
        assert_eq!(resumed.binding(),&binding,"semantic recovery lost its original registered binding");
        assert!(matches!(*resumed.acceptance_authority.lock().unwrap(),AcceptanceAuthority::RecoveryOnly),"reconsideration entered with ordinary builder authority");
        assert!(resumed.audit_logger().model_call_admitted("codex").is_err(),"negative qualification metadata admitted a builder");
        let store=WorkflowStore::open(&state).unwrap();assert_eq!(store.snapshot(&workflow).unwrap().unwrap().model_calls,0);
        drop(store);
        resumed.checkpoint(Stage::Reconcile,"resume_started","inspect_candidate",0,&[]).await.unwrap();
        resumed.finish().unwrap();
        let restarted=WorkflowGuard::acquire_fixture(&state,start()).await.unwrap().unwrap();
        assert_eq!(restarted.binding(),&binding,"crash after resume_started discarded the queued source binding");
        assert!(matches!(*restarted.acceptance_authority.lock().unwrap(),AcceptanceAuthority::RecoveryOnly),"restart discarded semantic recovery-only authority");
        assert!(restarted.audit_logger().model_call_admitted("codex").is_err(),"restart admitted a builder from negative source data");
        assert_eq!(WorkflowStore::open(&state).unwrap().snapshot(&workflow).unwrap().unwrap().model_calls,0);
        restarted.finish().unwrap();
    }

    #[test]
    fn baseline_source_uses_the_resolved_workflow_not_a_pr_alias() {
        use crate::autoship_contract::AcceptanceSource;
        assert_eq!(source_from_workflow("owner/repo#2014"),Some(AcceptanceSource::Issue(2014)));
        assert_eq!(source_from_workflow("owner/repo/pr/2027"),Some(AcceptanceSource::PullRequest(2027)));
        assert_eq!(source_from_workflow("autoship-observation/probe"),None);
        assert_eq!(source_from_workflow("owner/repo#0"),None);
    }
    pub(super) async fn scripted_independent_admission(guard:&WorkflowGuard) {
        use augmentagent_channel_core::{native_session,ProviderKind};
        native_session::fresh_review_scope(ProviderKind::Codex,async {
            let session=native_session::current_for(ProviderKind::Codex)?.unwrap();let mut native=session.begin(ProviderKind::Codex)?;
            guard.audit_logger().model_call_admitted("codex")?;native.observe("scripted-independent-accounting-fixture")?;native.finish()?;
            Ok::<_,anyhow::Error>(())
        }).await.unwrap();
    }
    pub(super) fn control_dir() -> tempfile::TempDir {
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

    pub(super) fn repo() -> tempfile::TempDir {
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
        let unrelated_head = git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap();
        // #2279: an unrecorded local head never inherits the old recovery
        // authority. An idle, due workflow sets it aside under a durable ref
        // and resumes from the authorized PR head instead of holding forever.
        let rebuilt = prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await
            .expect("an unrecorded head must rebuild from the authorized PR head, not hold");
        assert_eq!(rebuilt, candidate);
        assert_eq!(git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap(), published,
            "an unrecorded local head cannot inherit the old recovery authority");
        assert_eq!(git_evidence(dir.path(), &["rev-parse", &format!("refs/autoship/preserved/pr-42/{unrelated_head}")]).await.unwrap(),
            unrelated_head, "the set-aside candidate must be preserved as evidence");
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(), before, "rebuild preparation must not rewrite workflow state");
        assert_eq!(git_evidence(dir.path(), &["ls-remote", "origin", "refs/heads/fixture"]).await.unwrap(),
            format!("{published}\trefs/heads/fixture"), "rebuild must not publish the set-aside candidate");
        assert!(run("git", &["checkout", "--quiet", "--detach", &unrelated_head], &candidate).await.unwrap().0);
        let mut unrelated = retained;
        unrelated.head = unrelated_head.clone();
        let next_lease = store.claim(&lease.workflow, "fixture-authority", millis(), 600_000).unwrap().unwrap();
        checkpoint.binding = unrelated;
        store.checkpoint(&next_lease, &checkpoint, 41).unwrap();
        // While another worker holds the lease the candidate stays in place.
        let leased = prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await;
        assert!(leased.is_err(), "a leased workflow's candidate must be preserved, not rebuilt");
        assert_eq!(git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap(), unrelated_head);
        store.release(&next_lease, 42).unwrap();
        let rebuilt = prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await
            .expect("a recorded non-descendant rebuilds from the authorized PR head");
        assert_eq!(rebuilt, candidate);
        assert_eq!(git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap(), published,
            "matching protected metadata cannot replace publication ancestry");
        assert_eq!(git_evidence(dir.path(), &["ls-remote", "origin", "refs/heads/fixture"]).await.unwrap(),
            format!("{published}\trefs/heads/fixture"));
    }

    /// #2279 — built and verified, then the push failed before any dispatch:
    /// the workflow sits at `publish / publishing_revision / push`. Resuming
    /// it must keep the recorded candidate (a verified descendant of the
    /// workflow's own confirmed publication) instead of refusing every tick.
    #[tokio::test]
    async fn push_failed_publish_candidate_resumes_as_recorded_descendant() {
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
        store.register("owner/repo/pr/42", "fixture", original.clone(), 10).unwrap();
        let lease = store.claim("owner/repo/pr/42", "original-worker", 20, 60_000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease, "owner/repo", 42, 21).unwrap();
        let pushed = store.begin_operation(&lease, OperationKind::Push, &original, 22).unwrap();
        store.complete_operation(&lease, &pushed.id, &format!("git-head:{published}"), 23).unwrap();
        std::fs::write(candidate.join("code.txt"), "verified revision\n").unwrap();
        assert!(run("git", &["add", "code.txt"], &candidate).await.unwrap().0);
        assert!(run("git", &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "verified revision"], &candidate).await.unwrap().0);
        let verified = git_binding(&candidate, "C1", "rust-a", &[]).await.unwrap();
        let mut checkpoint = store.snapshot(&lease.workflow).unwrap().unwrap().checkpoint;
        checkpoint.binding = verified.clone();
        checkpoint.stage = Stage::Publish;
        checkpoint.next = NextAction { reason: "publishing_revision".into(), action: "push".into(), eligible_at_ms: 30 };
        store.checkpoint(&lease, &checkpoint, 24).unwrap();
        store.release(&lease, 25).unwrap();
        let before = store.snapshot(&lease.workflow).unwrap().unwrap();
        let resumed = prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await
            .expect("a verified push-failed candidate must resume for publication");
        assert_eq!(resumed, candidate);
        assert_eq!(git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap(), verified.head);
        assert_eq!(store.snapshot(&lease.workflow).unwrap().unwrap(), before);
        // An unconfirmed Push intent still fences resume (outcome unknown).
        let fenced = store.claim(&lease.workflow, "fence", 40, 60_000).unwrap().unwrap();
        store.begin_operation(&fenced, OperationKind::Push, &verified, 41).unwrap();
        store.release(&fenced, 42).unwrap();
        assert!(prepare_resume_candidate_at(&database, dir.path(), "owner/repo", 42, "fixture", Some(&published)).await.is_err(),
            "an unknown Push outcome must not be resumed or rebuilt");
        assert_eq!(git_evidence(&candidate, &["rev-parse", "HEAD"]).await.unwrap(), verified.head);
    }

    #[tokio::test]
    async fn claimed_owner_guard_admits_native_work_without_granting_shipping() {
        let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");let now=millis();
        let mut store=WorkflowStore::open(&path).unwrap();
        let origin=durable::discord_ingress::DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let request=durable::submission::Request {repository:"owner/repo".into(),target:durable::submission::Target::Pr(42),provider:Some(durable::submission::Provider::Codex),until:durable::submission::Milestone::Merged};
        let id=store.submit_discord_authorized(&origin,&request,&"a".repeat(64),now).unwrap().admission.workflow;
        let binding=git_binding(dir.path(),"C1","rust-a",&[]).await.unwrap();store.register(&id,"candidate",binding,now).unwrap();
        store.enqueue_owner_message(&durable::discord_ingress::DiscordOrigin {message:45,..origin},&id,"Fix the test",None,now).unwrap();drop(store);
        let guard=WorkflowGuard::acquire_mode_at(&path,Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:None,pr:Some(42),branch:"candidate",contract:"C1",environment:"rust-a"},millis(),AcquireMode::OwnerTurn).await.unwrap().unwrap();
        guard.audit_logger().dispatch_allowed().unwrap();guard.audit_logger().model_call_admitted("codex").unwrap();
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

    /// #2354: a productive stage transition is a restart-safe point. With a
    /// deploy pending, the checkpoint is durably written first and only then
    /// does the tick park, before the next step starts. Waiting and terminal
    /// transitions finish their tick normally.
    #[tokio::test]
    async fn a_productive_transition_parks_for_a_deploy_after_its_checkpoint() {
        let dir = repo();
        // A canonical (non-symlinked) private directory, so the fixture also
        // runs where /tmp is a symlink.
        let state = tempfile::tempdir().unwrap();
        let path = state.path().join("workflow.db");
        let binary = state.path().join("worker");
        std::fs::write(&binary, "same executable").unwrap();
        let stamp = state.path().join("built-commit");
        std::fs::write(&stamp, "aaaa\n").unwrap();
        let points = crate::autoship_worker::SafePoints::new(
            Arc::new(crate::autoship_worker::UpgradeWatch::at(&binary).with_deployment_stamp(&stamp)),
            tokio_util::sync::CancellationToken::new());
        let start = || Start {
            repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo",
            source_issue: Some(7), pr: None, branch: "fix/seven", contract: "C1", environment: "rust-a",
        };
        let mut guard = WorkflowGuard::acquire_at(&path, start(), millis()).await.unwrap().unwrap();
        let id = guard.id().to_owned();
        points.scope(guard.transition(Stage::Build, "scoped", "build", 0)).await
            .expect("no deploy pending: the transition completes");
        std::fs::write(&stamp, "bbbb\n").unwrap();
        points.scope(guard.transition(Stage::Held, "provider_capacity", "retry", 0)).await
            .expect("a waiting transition ends its tick normally");
        let parked = tokio::time::timeout(std::time::Duration::from_secs(2),
            points.scope(guard.transition(Stage::Review, "candidate_verified", "review", 0))).await;
        assert!(parked.is_err(), "a productive transition with a deploy pending must park the tick");
        tokio::time::timeout(std::time::Duration::from_secs(1), points.parked()).await
            .expect("the worker is told the tick parked");
        let store = WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(&id).unwrap().unwrap().checkpoint.stage, Stage::Review,
            "the checkpoint is durable before the tick parks");
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
        let frozen=push_destination::Resolved::resolve(dir.path(),"owner/repo").await.unwrap();
        guard.bound_push_intent(frozen.destination(),"refs/heads/fix/seven").unwrap();
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
        let store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls,2);
        let progress=store.progress_events(guard.id()).unwrap();
        assert_eq!(progress.last().unwrap().requested_provider,None);
        assert_eq!(progress.last().unwrap().actual_provider.as_deref(),Some("codex"),"native admission must project the provider that actually ran, independently of the requested pin");
    }

    #[tokio::test]
    async fn workflow_restart_cannot_spend_a_second_witness_call_for_changed_inputs() {
        let dir=repo();let control=control_dir();let path=control.path().join("workflow.db");
        let start=||Start {repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,branch:"fixture",contract:"C1",environment:"test"};
        let guard=WorkflowGuard::acquire_at(&path,start(),millis()).await.unwrap().unwrap();
        let original=format!("{}{}",durable::BASELINE_NATIVE_PURPOSE_PREFIX,"a".repeat(64));
        let result=guard.bounded_native_calls(original,1,
            augmentagent_channel_core::native_session::fresh_review_scope(
                augmentagent_channel_core::ProviderKind::Codex,async {
                    guard.audit_logger().model_call_admitted("codex")?;
                    Err::<(),_>(anyhow::anyhow!("scripted original response loss after admission"))
                })).await;
        assert!(result.is_err());
        assert!(guard.baseline_native_custody().is_err());
        let workflow=guard.id().to_owned();guard.finish().unwrap();
        let next=WorkflowGuard::acquire_at(&path,start(),millis()).await.unwrap().unwrap();
        let changed=format!("{}{}",durable::BASELINE_NATIVE_PURPOSE_PREFIX,"b".repeat(64));
        let refused=next.bounded_native_calls(changed,1,
            augmentagent_channel_core::native_session::fresh_review_scope(
                augmentagent_channel_core::ProviderKind::Codex,async {
                    next.audit_logger().model_call_admitted("codex")
                })).await.unwrap_err();
        assert!(refused.is::<durable::NativeScopeOutcomeUnresolved>(),"native admission lost its durable custody refusal: {refused:#}");
        let store=WorkflowStore::open(&path).unwrap();
        assert_eq!(store.snapshot(&workflow).unwrap().unwrap().model_calls,1);
        assert_eq!(store.events(&workflow).unwrap().iter().filter(|(_,kind,_,_)|kind=="native_model_admitted").count(),1);
        next.finish().unwrap();
    }

    #[tokio::test]
    async fn model_admission_audit_names_the_committed_accounting_receipt() {
        let dir = repo();
        let control = control_dir();
        let path = control.path().join("workflow.db");
        let guard = WorkflowGuard::acquire_at(&path, Start {
            repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo",
            source_issue: Some(77), pr: None, branch: "fixture", contract: "C1",
            environment: "test",
        }, millis()).await.unwrap().unwrap();
        let audit = guard.audit_logger();
        // Script the existing admission boundary; no provider is launched.
        guard.bounded_native_calls("original-witness-input".into(),1,async {
            audit.model_call_admitted("codex")
        }).await.unwrap();
        let store = WorkflowStore::open(&path).unwrap();
        let admissions: Vec<serde_json::Value> = store.events(guard.id()).unwrap()
            .into_iter().filter(|(_, kind, _, _)| kind == "native_model_admitted")
            .map(|(_, _, payload, _)| serde_json::from_str(&payload).unwrap()).collect();
        assert_eq!(admissions.len(), 1);
        assert_eq!(store.snapshot(guard.id()).unwrap().unwrap().model_calls, 1);
        assert_eq!(admissions[0]["bounded"],serde_json::json!({
            "purpose":"original-witness-input","binding":guard.binding(),"limit":1
        }),"the existing admission observer must bind the charged receipt to its native input");
        let receipt = admissions[0]["receipt"].as_str().unwrap();
        assert!(receipt.starts_with("model-call:"));
        let records: Vec<serde_json::Value> = std::fs::read_to_string(audit.path()).unwrap()
            .lines().map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|record| record["kind"] == "model_call_admitted").collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["actual_provider"], "codex");
        assert_eq!(records[0]["accounting_receipt"].as_str(), Some(receipt),
            "protected admission must identify the original committed charge");
        audit.protected_health().unwrap();
    }

    /// #2408 production shape (#2329 at 11:54:52Z): "alignment reviewer
    /// unavailable: origin: candidate is not a descendant of trusted adopted
    /// head" was deferred as acceptance infrastructure and retried forever.
    /// It is provenance: reroute to the PR lane, never an infrastructure wait.
    #[tokio::test]
    async fn provenance_divergence_reroutes_instead_of_infrastructure_retry() {
        let directory=repo();let state=control_dir();let path=state.path().join("workflow.db");
        let mut guard=WorkflowGuard::acquire_at(&path,Start {
            repo_root:directory.path(),worktree:directory.path(),repository:"owner/repo",
            source_issue:Some(2329),pr:Some(2378),branch:"fixture",contract:"C1",environment:"rust-a"
        },millis()).await.unwrap().unwrap();
        guard.checkpoint(Stage::Verify,"verification_started","run_verifier",0,&["receipt".into()]).await.unwrap();
        let error=anyhow::Error::new(adoption::CandidateProvenanceDiverged {
            adopted_head:"1801".repeat(10),candidate_head:"1027".repeat(10)})
            .context("alignment reviewer unavailable: origin")
            .context("acceptance infrastructure unavailable");
        let outcome=CiGateResult(Err(error)).for_workflow(&mut guard).await;
        let error=match outcome { Err(error)=>error, Ok(other)=>panic!("provenance became a gate result: {other:?}") };
        assert!(error.downcast_ref::<CiInfrastructureDeferred>().is_none(), "provenance misfiled as infrastructure: {error:#}");
        let store=WorkflowStore::open(&path).unwrap();
        let cp=store.snapshot(guard.id()).unwrap().unwrap().checkpoint;
        assert_eq!((cp.stage,cp.next.reason.as_str(),cp.next.action.as_str()),
            (Stage::Reconcile,"candidate_provenance_diverged","resume_preserved_pr"));
        assert!(!store.events(guard.id()).unwrap().iter().any(|(_,kind,_,_)|kind=="verification_infrastructure_retry"));
        assert_eq!(durable::retry_recovery::class(&cp),Some("provenance"),"a bounded automatic retry class, not a park");
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
                let (dir,state,prepared)=acceptance::qualification_fixture();
                acceptance::qualify_fixture(dir.path(),state.path(),&prepared).unwrap();
                let path = state.path().join("workflow.db");
                let mut guard = WorkflowGuard::acquire_at(&path, Start {
                    repo_root: dir.path(), worktree: dir.path(), repository: "owner/repo",
                    source_issue: None, pr: Some(840), branch: "fixture", contract: "C1",
                    environment: "rust-a",
                }, millis()).await.unwrap().unwrap();
                guard.bind_acceptance(&prepared,state.path()).unwrap();
                guard.checkpoint(stage, "candidate_ready", "run_verifier", 0,
                    &["protected-receipt".into()]).await.unwrap();
                let accounting = guard.capacity_accounting(true).unwrap();
                if already_paid { scripted_independent_admission(&guard).await; }
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
    async fn routing_hold_settlement_reports_only_this_runs_actual_admissions() {
        for resumed in [false,true] {
            for admission in ["none","historical","current"] {
                let dir=repo();let control=control_dir();let path=control.path().join("workflow.db");
                let guard=WorkflowGuard::acquire_at(&path,Start {
                    repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(77),pr:None,
                    branch:"fixture",contract:"C1",environment:"test",
                },millis()).await.unwrap().unwrap();
                if admission=="historical" {scripted_independent_admission(&guard).await;}
                let accounting=guard.capacity_accounting(resumed).unwrap();
                if admission=="current" {scripted_independent_admission(&guard).await;}
                let store=WorkflowStore::open(&path).unwrap();
                let before=store.snapshot(guard.id()).unwrap();let events=store.events(guard.id()).unwrap();
                let allowance=store.merge_allowance(millis(),5).unwrap();
                let message="candidate retained after owner changed automatic routing";
                let report=accounting.settle_ci(Ok(RunReport::held(message.into()))).unwrap();
                assert_eq!(report.billed,admission=="current","late hold lost actual spend: {admission}/resumed={resumed}");
                assert!(report.held && report.is_idle(),"a routing hold became productive work");
                assert_eq!(report.resumed,resumed);assert_eq!(report.message,message);
                assert_eq!(store.snapshot(guard.id()).unwrap(),before,"reporting changed workflow or original charges");
                assert_eq!(store.events(guard.id()).unwrap(),events,"reporting fabricated progress");
                assert_eq!(store.merge_allowance(millis(),5).unwrap(),allowance,"reporting changed merge allowance");
                guard.finish().unwrap();
            }
        }
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
        scripted_independent_admission(&guard).await;
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

    #[test]
    fn native_capacity_reconciliation_prunes_only_completed_retired_sessions_before_wake() {
        use std::os::unix::fs::{OpenOptionsExt,PermissionsExt};
        use std::os::fd::AsRawFd;
        const CHILD:&str="JARVIS_TEST_NATIVE_RETIREMENT_WAKE";
        if std::env::var_os(CHILD).is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","self_improve::workflow::tests::native_capacity_reconciliation_prunes_only_completed_retired_sessions_before_wake","--nocapture"])
                .env_clear().env("PATH","/usr/bin:/bin").env(CHILD,"1").output().unwrap();
            let stdout=String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(),"{stdout} {}",String::from_utf8_lossy(&output.stderr));
            assert!(stdout.contains("test result: ok. 1 passed; 0 failed;"),"child regression did not run: {stdout}");return;
        }
        let root=control_dir();let state=root.path().join("native");let provider=state.join("codex");
        std::fs::create_dir_all(&provider).unwrap();
        for path in [&state,&provider] {std::fs::set_permissions(path,std::fs::Permissions::from_mode(0o700)).unwrap();}
        let write=|path:&Path,bytes:&[u8]| {
            use std::io::Write;
            let mut file=std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap();file.write_all(bytes).unwrap();file.sync_all().unwrap();
        };
        write(&state.join("state.lock"),b"");
        let known=provider.join("session-completed");let unknown=provider.join("session-interrupted");
        for session in [&known,&unknown] {
            std::fs::create_dir(session).unwrap();std::fs::set_permissions(session,std::fs::Permissions::from_mode(0o700)).unwrap();
            write(&session.join("owned.json"),br#"{"version":1,"audit_path":"/original/audit","owner":{"workflow_id":"original","lease_generation":1}}"#);
            write(&session.join("retired.json"),br#"{"version":1,"reason":"native_scope_retired"}"#);
            write(&session.join("lock"),b"");
        }
        write(&known.join("complete.json"),br#"{"version":1,"generation":"generation-a"}"#);
        let unknown_owned=std::fs::read(unknown.join("owned.json")).unwrap();
        let aliases=provider.join("aliases");std::fs::create_dir(&aliases).unwrap();
        write(&aliases.join("completed-fixture"),br#""session-completed""#);
        write(&aliases.join("interrupted-fixture"),br#""session-interrupted""#);
        let policy=root.path().join("policy.json");
        write(&policy,&serde_json::to_vec(&serde_json::json!({"version":1,"runtime_config":"/declared/runtime","guest_runtime":"/declared/guest","state_root":state,"memory_mb":512,"max_state_bytes":2147483648u64,"max_sessions":3,"providers":{}})).unwrap());
        std::env::set_var("AUGMENTAGENT_NATIVE_VM_POLICY",&policy);
        let full=augmentagent_channel_core::native_vm::state_capacity::inspect();
        assert!(full.available_fingerprint().is_none());assert!(known.exists(),"read-only inspection deleted runtime state");
        let fingerprint=serde_json::to_value(full).unwrap()["capacity_fingerprint"].as_str().unwrap().to_owned();
        let db=root.path().join("workflow.db");let mut store=WorkflowStore::open(&db).unwrap();let id="owner/repo#914";
        let binding=Binding{head:"head".into(),contract:"contract".into(),evidence_hash:"receipt".into()};
        store.register(id,"fix",binding,10).unwrap();
        let request=durable::submission::Request{repository:"owner/repo".into(),target:durable::submission::Target::Issue(914),provider:None,until:durable::submission::Milestone::Merged};
        store.submit_authorized("native-retirement-job","owner",&request,10).unwrap();
        let lease=store.claim(id,"worker",20,60_000).unwrap().unwrap();
        let due=store.defer_run_capacity(&lease,Some(&fingerprint),30).unwrap();store.release(&lease,31).unwrap();
        let before=store.snapshot(id).unwrap().unwrap();
        let lock=std::fs::OpenOptions::new().read(true).write(true).open(known.join("lock")).unwrap();
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)},0);
        reconcile_runtime_retirements_at(&db,32).unwrap();
        assert!(known.exists());assert!(!store.command_queue_due("owner/repo",32).unwrap());
        assert_eq!(store.snapshot(id).unwrap().unwrap().checkpoint.next.eligible_at_ms,due);
        assert_eq!(unsafe{libc::flock(lock.as_raw_fd(),libc::LOCK_UN)},0);drop(lock);
        reconcile_runtime_retirements_at(&db,33).unwrap();
        assert!(store.command_queue_due("owner/repo",33).unwrap(),"retired completed capacity did not wake the deferred command job");
        assert!(!known.exists());assert!(!aliases.join("completed-fixture").exists());
        assert!(unknown.exists());assert!(aliases.join("interrupted-fixture").exists());
        assert_eq!(std::fs::read(unknown.join("owned.json")).unwrap(),unknown_owned);
        assert!(!unknown.join("retired-owner.json").exists());
        let after=store.snapshot(id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.eligible_at_ms,33);assert_eq!(after.progress_at_ms,before.progress_at_ms);
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.generation,before.generation);
    }

    #[tokio::test]
    async fn native_capacity_observer_persists_retry_without_progress_or_paid_admission() {
        let dir=repo();let control=control_dir();let path=control.path().join("workflow.db");
        let start=Start{repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(711),pr:None,branch:"fixture",contract:"C1",environment:"test"};
        let guard=WorkflowGuard::acquire_at(&path,start,millis()).await.unwrap().unwrap();let id=guard.id().to_owned();
        let before=WorkflowStore::open(&path).unwrap().snapshot(&id).unwrap().unwrap();let now=millis();
        // Protected observer metadata fixture; native storage production itself
        // is covered by channel-core's real admission/inspection regressions.
        let observation=serde_json::json!({"phase":"native_runtime_state_capacity","exhausted":true,
            "native_state":{"status":"capacity-exhausted","capacity_fingerprint":"a".repeat(64)}});
        guard.audit_logger().resource_capacity_exhausted(&observation).unwrap();
        let mut store=WorkflowStore::open(&path).unwrap();let after=store.snapshot(&id).unwrap().unwrap();
        let retry=store.events(&id).unwrap().into_iter().find(|e|e.1=="resource_capacity_retry");
        assert!(retry.is_some());assert_eq!(after.checkpoint.next.action,"await_capacity");
        assert!(after.checkpoint.next.eligible_at_ms>=now+300_000 && after.checkpoint.next.eligible_at_ms<=millis()+300_000);
        let events=store.events(&id).unwrap().len();guard.audit_logger().resource_capacity_exhausted(&observation).unwrap();
        assert_eq!(store.events(&id).unwrap().len(),events);
        assert_eq!(after.progress_at_ms,before.progress_at_ms);assert_eq!(after.model_calls,before.model_calls);
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert!(guard.audit_logger().model_call_admitted("codex").is_err());assert!(guard.validate_before_side_effect().is_err());
        assert!(guard.finish().is_err());
        assert!(store.claim_productive(&id,"too-early",after.checkpoint.next.eligible_at_ms-1,60_000).unwrap().is_none());
    }

    #[tokio::test]
    async fn published_policy_dependency_defers_before_the_model_repair_boundary() {
        let dir = repo();
        let control = control_dir();
        let path = control.path().join("workflow.db");
        let mut guard = WorkflowGuard::acquire_at(
            &path,
            Start {
                repo_root: dir.path(),
                worktree: dir.path(),
                repository: "owner/repo",
                source_issue: Some(713),
                pr: Some(42),
                branch: "fixture",
                contract: "C1",
                environment: "test",
            },
            millis(),
        )
        .await
        .unwrap()
        .unwrap();
        guard.audit_logger().model_call_admitted("claude").unwrap();
        let id = guard.id().to_owned();
        let before = WorkflowStore::open(&path)
            .unwrap()
            .snapshot(&id)
            .unwrap()
            .unwrap();
        let error = CiGateResult(Err(hosted_ci::MainPolicyReconciliation {
            main: "b".repeat(40),
        }
        .into()))
        .for_workflow(&mut guard)
        .await
        .unwrap_err();
        assert!(
            error.is::<CiInfrastructureDeferred>(),
            "policy drift leaked into the repairable inner result: {error:#}"
        );
        let mut store = WorkflowStore::open(&path).unwrap();
        let after = store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.stage, Stage::Reconcile);
        assert_eq!(after.checkpoint.next.reason, "review_main_changed");
        assert_eq!(after.checkpoint.next.action, "reconcile_main");
        assert!(after.checkpoint.next.eligible_at_ms > millis());
        assert_eq!(after.model_calls, before.model_calls);
        assert_eq!(after.tokens, before.tokens);
        assert!(store
            .claim_productive(
                &id,
                "too-early",
                after.checkpoint.next.eligible_at_ms - 1,
                60_000
            )
            .unwrap()
            .is_none());
        guard.finish().unwrap();
    }

    #[tokio::test]
    async fn native_tool_capacity_observer_keeps_precise_blocker_after_restart() {
        let dir=repo();let control=control_dir();let path=control.path().join("workflow.db");
        let start=Start{repo_root:dir.path(),worktree:dir.path(),repository:"owner/repo",source_issue:Some(712),pr:None,branch:"fixture",contract:"C1",environment:"test"};
        let guard=WorkflowGuard::acquire_at(&path,start,millis()).await.unwrap().unwrap();
        guard.audit_logger().model_call_admitted("claude").unwrap();
        let before=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
        let observation=serde_json::json!({"phase":"native_tool_build_storage_reserve","exhausted":true,
            "tool_readiness":{"category":"build_storage_reserve","execution_started":false},
            "available_bytes":25,"required_bytes":37,"private":"MUST_NOT_ESCAPE"});
        guard.audit_logger().resource_capacity_exhausted(&observation).unwrap();
        let id=guard.id().to_owned();assert!(guard.finish().is_err());
        let mut store=WorkflowStore::open(&path).unwrap();let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.next.reason,"resource_capacity");assert_eq!(after.checkpoint.next.action,"await_capacity");
        assert_eq!(after.model_calls,before.model_calls);assert_eq!(after.progress_at_ms,before.progress_at_ms);
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert!(store.claim_productive(&id,"early",after.checkpoint.next.eligible_at_ms-1,60_000).unwrap().is_none());
        let events=store.progress_events(&id).unwrap();let progress=serde_json::to_value(events.last().unwrap()).unwrap();
        assert_eq!(progress["tool_capacity_refusal"],serde_json::json!({"category":"build_storage_reserve","execution_started":false,"available_bytes":25,"required_bytes":37}));
        assert!(!progress.to_string().contains("MUST_NOT_ESCAPE"));
        let (code,status)=crate::autoship_commands::status(&path,&id,millis(),5);
        assert_eq!(code,3);assert_eq!(status["tool_capacity_refusal"],progress["tool_capacity_refusal"]);
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

    #[tokio::test]
    async fn linked_claim_hold_preserves_current_stage_evidence_progress_and_first_eligibility() {
        for stage in [Stage::Scope,Stage::Build,Stage::Publish] {
            for hold in [linked_claims::Hold::Claimed,linked_claims::Hold::Unknown] {
                let dir=repo();let state=control_dir();let path=state.path().join("workflow.db");
                let mut guard=WorkflowGuard::acquire_at(&path,Start{repo_root:dir.path(),worktree:dir.path(),
                    repository:"owner/repo",source_issue:Some(77),pr:None,branch:"fixture",contract:"C1",environment:"rust-a"},millis()).await.unwrap().unwrap();
                guard.checkpoint(stage,"fixture_dispatch","fixture",0,&["original-evidence".into()]).await.unwrap();
                let before=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
                let first=millis();guard.hold_linked_claim(hold).await.unwrap();
                let after=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
                assert_eq!(after.checkpoint.stage,before.checkpoint.stage);
                assert_eq!(after.checkpoint.binding,before.checkpoint.binding);
                assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
                assert_eq!(after.progress_at_ms,before.progress_at_ms);
                assert_eq!(after.model_calls,before.model_calls);
                assert_eq!(after.checkpoint.next.reason,hold.reason());
                assert_eq!(after.checkpoint.next.action,hold.action());
                assert!(after.checkpoint.next.eligible_at_ms>=first+300_000);
                guard.hold_linked_claim(hold).await.unwrap();
                let repeated=WorkflowStore::open(&path).unwrap().snapshot(guard.id()).unwrap().unwrap();
                assert_eq!(repeated.checkpoint,after.checkpoint);
                assert_eq!(repeated.progress_at_ms,after.progress_at_ms);
                guard.finish().unwrap();
            }
        }
    }

}


#[cfg(test)]
mod initial_acceptance_hold_tests {
    use super::*;
    #[tokio::test]
    async fn upstream_only_change_cannot_reclaim_an_exhausted_initial_verifier() {
        upstream_wait_fixture(true).await;
    }
    #[tokio::test]
    async fn upstream_movement_still_wakes_other_readiness_dependencies() {
        upstream_wait_fixture(false).await;
    }
    async fn upstream_wait_fixture(input_wait:bool) {
        async fn git(root:&Path,args:&[&str])->String {
            let out=tokio::process::Command::new("/usr/bin/git").args(args).current_dir(root)
                .env_clear().env("PATH","/usr/bin:/bin").output().await.unwrap();
            assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }
        let root=tempfile::tempdir().unwrap();let control=tempfile::tempdir_in("/tmp").unwrap();
        let upstream=root.path().join("upstream");let candidate=root.path().join("candidate");
        std::fs::create_dir(&upstream).unwrap();
        std::fs::write(upstream.join("implementation.py"),"def add(a,b): return a-b\n").unwrap();
        std::fs::write(upstream.join("Cargo.lock"),"unchanged compiler inputs\n").unwrap();
        for args in [vec!["init","-q"],vec!["add","."],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","initial"],vec!["update-ref","refs/remotes/origin/main","HEAD"]] {git(&upstream,&args).await;}
        let initial=git(&upstream,&["rev-parse","HEAD"]).await;
        git(&upstream,&["worktree","add","--detach",candidate.to_str().unwrap(),&initial]).await;
        let environment=recovery::environment_identity(&candidate,&initial).await.unwrap();
        let path=control.path().join("workflow.db");let now=millis();
        let mut guard=WorkflowGuard::acquire_at(&path,Start {repo_root:&upstream,worktree:&candidate,repository:"owner/repo",
            source_issue:Some(7),pr:None,branch:"fixture",contract:"unchanged owner criteria",environment:&environment},now).await.unwrap().unwrap();
        guard.store.lock().unwrap().charge(&guard.lease,"past-scope-work",crate::autoship_workflow::ResourceCharge {model_calls:3,tokens:1234,wall_ms:700},millis()).unwrap();
        if input_wait {
            guard.hold_initial_acceptance("acceptance_verifier_repair_exhausted","await_acceptance_input_change").await.unwrap();
        }else {
            guard.checkpoint(Stage::Scope,"scope_deferred","resolve_scope_dependencies",0,&[]).await.unwrap();
        }
        let id=guard.id().to_owned();guard.finish().unwrap();
        let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();drop(store);
        std::fs::write(upstream.join("unrelated.md"),"manual intake routing changed elsewhere\n").unwrap();
        git(&upstream,&["add","unrelated.md"]).await;
        git(&upstream,&["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","unrelated main advance"]).await;
        let advanced=git(&upstream,&["rev-parse","HEAD"]).await;
        git(&upstream,&["update-ref","refs/remotes/origin/main",&advanced]).await;
        let current=recovery::environment_identity(&candidate,&advanced).await.unwrap();
        assert_ne!(environment,current,"fixture did not exercise real upstream-dependent environment");
        assert_eq!(git(&candidate,&["rev-parse","HEAD"]).await,initial);
        assert!(git(&candidate,&["status","--porcelain"]).await.is_empty());
        let next=WorkflowGuard::acquire_at(&path,Start {repo_root:&upstream,worktree:&candidate,repository:"owner/repo",
            source_issue:Some(7),pr:None,branch:"fixture",contract:"unchanged owner criteria",environment:&current},now+1000).await.unwrap();
        if !input_wait {
            let resumed=next.expect("ordinary readiness dependency lost its upstream wake");
            let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();let after=store.snapshot(&id).unwrap().unwrap();
            assert_eq!(after.attempts,before.attempts+1);assert_eq!(after.model_calls,3);assert_eq!(after.tokens,before.tokens);
            resumed.finish().unwrap();return;
        }
        // A planning lease may now escalate to children. The original verifier
        // remains exhausted: its binding, evidence and charges are unchanged.
        let mut resumed=next.expect("exhausted verifier could not escalate planning");
        let original=resumed.begin_acceptance_escalation().await.unwrap().unwrap();
        assert_eq!(original.original,before.checkpoint);
        let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();let after=store.snapshot(&id).unwrap().unwrap();
        assert_eq!(after.checkpoint.binding,before.checkpoint.binding);assert_eq!(after.checkpoint.evidence,before.checkpoint.evidence);
        assert_eq!(after.attempts,before.attempts+1);assert_eq!(after.model_calls,3);assert_eq!(after.tokens,before.tokens);
        drop(store);resumed.finish().unwrap();
    }
    #[tokio::test]
    async fn unchanged_initial_acceptance_waits_preserve_admission_and_accounting_after_restart() {
        for (reason,action) in [
            ("acceptance_verifier_amendment_declined","await_acceptance_input_change"),
            ("acceptance_verifier_repair_exhausted","await_acceptance_input_change"),
            ("readiness_assessment_invalid","await_readiness_input_change"),
            ("acceptance_verifier_evidence_unknown","await_original_evidence_recovery"),
            ("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery"),
        ] {
            for (issue,pr) in [(Some(7),None),(Some(7),Some(42)),(None,Some(42))] {
                let repo=tempfile::tempdir().unwrap();let control=tempfile::tempdir_in("/tmp").unwrap();
                std::fs::write(repo.path().join("implementation.py"),"def add(a,b): return a-b\n").unwrap();
                for args in [vec!["init","-q"],vec!["add","."],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","initial"],vec!["update-ref","refs/remotes/origin/main","HEAD"]] {
                    let output=tokio::process::Command::new("/usr/bin/git").args(args).current_dir(repo.path()).env_clear().env("PATH","/usr/bin:/bin").output().await.unwrap();
                    assert!(output.status.success());
                }
                // SQLite NOFOLLOW rejects macOS /tmp symlinks; exercise the same
                // admission/accounting assertions on both platforms.
                let path=control.path().canonicalize().unwrap().join("workflow.db");
                let start=||Start {repo_root:repo.path(),worktree:repo.path(),repository:"owner/repo",
                    source_issue:issue,pr,branch:"fixture",contract:"unchanged owner criteria",environment:"fixture"};
                let now=millis();
                let mut guard=WorkflowGuard::acquire_at(&path,start(),now).await.unwrap().unwrap();
                guard.store.lock().unwrap().charge(&guard.lease,"original-verifier-work",
                    crate::autoship_workflow::ResourceCharge {model_calls:3,tokens:111,wall_ms:700},now).unwrap();
                guard.hold_initial_acceptance(reason,action).await.unwrap();
                let id=guard.id().to_owned();guard.finish().unwrap();
                let before=WorkflowStore::open_read_only(&path).unwrap().unwrap();
                let original=before.snapshot(&id).unwrap().unwrap();
                let events=before.events(&id).unwrap();drop(before);
                let mut planning_claims = 0;
                for offset in [1000,86_400_000,2*86_400_000] {
                    let next=WorkflowGuard::acquire_at(&path,start(),now+offset).await.unwrap();
                    // #2370: a declined amendment enters the same ladder.
                    if matches!(reason,"acceptance_verifier_repair_exhausted"|"acceptance_verifier_amendment_declined") && pr.is_none() {
                        let planning=next.expect("unpublished exhausted hold did not admit planning");
                        // A lease is an attempt even when no model work is done.
                        // Recovery must retain all paid work and the exhausted
                        // verifier checkpoint across every restart/day change.
                        planning_claims += 1;
                        let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();let after=store.snapshot(&id).unwrap().unwrap();
                        assert_eq!(after.attempts,original.attempts+planning_claims);
                        assert_eq!(after.checkpoint,original.checkpoint);assert_eq!(after.model_calls,original.model_calls);assert_eq!(after.tokens,original.tokens);
                        assert!(after.wall_ms>=original.wall_ms,"elapsed work accounting was reset");
                        drop(store);planning.finish().unwrap();continue;
                    }
                    assert!(next.is_none(),"unchanged acceptance-input wait was reclaimed: issue={issue:?}, pr={pr:?}");
                    let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
                    let after=store.snapshot(&id).unwrap().unwrap();
                    assert_eq!(after.attempts,original.attempts);
                    assert_eq!(after.model_calls,original.model_calls);
                    assert_eq!(after.tokens,original.tokens);
                    assert_eq!(after.checkpoint,original.checkpoint);
                    assert_eq!(store.events(&id).unwrap(),events);
                }
                // A real contract revision can re-enter ordinary admission, but
                // it cannot override the owner's pause or reuse old approval.
                let mut store=WorkflowStore::open(&path).unwrap();
                store.set_owner_pause(&id,"github:owner",0,true,now+3*86_400_000).unwrap();
                let mut revised=start();revised.contract="changed owner criteria";
                assert!(WorkflowGuard::acquire_at(&path,revised,now+3*86_400_000).await.unwrap().is_none());
                store.set_owner_pause(&id,"github:owner",1,false,now+3*86_400_000+1).unwrap();
                drop(store);
                let mut revised=start();revised.contract="changed owner criteria";
                let mut changed=WorkflowGuard::acquire_at(&path,revised,now+3*86_400_000+2).await.unwrap().unwrap();
                let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
                let after=store.snapshot(&id).unwrap().unwrap();
                assert_eq!(after.attempts,original.attempts+planning_claims+1);
                assert_eq!(after.checkpoint.stage,Stage::Reconcile);
                assert_eq!(after.checkpoint.next.reason,"inputs_changed");
                assert_ne!(after.checkpoint.binding.contract,original.checkpoint.binding.contract);
                assert_eq!(after.model_calls,original.model_calls);
                assert_eq!(after.tokens,original.tokens);
                assert!(store.current_review_history(&id).unwrap().is_empty());
                drop(store);
                if reason=="readiness_assessment_invalid" {
                    changed.hold_initial_acceptance(reason,action).await.unwrap();
                }
                changed.finish().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn all_initial_source_families_keep_precise_hold_without_false_progress() {
        for (issue,pr) in [(Some(7),None),(Some(7),Some(42)),(None,Some(42))] {
            let repo=tempfile::tempdir().unwrap();let control=tempfile::tempdir_in("/tmp").unwrap();
            std::fs::write(repo.path().join("implementation.py"),"def add(a,b): return a-b\n").unwrap();
            for args in [vec!["init","-q"],vec!["add","."],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null","commit","-qm","initial"],vec!["update-ref","refs/remotes/origin/main","HEAD"]] {
                let output=tokio::process::Command::new("/usr/bin/git").args(args).current_dir(repo.path()).env_clear().env("PATH","/usr/bin:/bin").output().await.unwrap();
                assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
            }
            let path=control.path().join("workflow.db");
            let mut guard=WorkflowGuard::acquire_fixture(&path,Start {repo_root:repo.path(),worktree:repo.path(),
                repository:"owner/repo",source_issue:issue,pr,branch:"fixture",contract:"unchanged owner criteria",environment:"fixture"}).await.unwrap().unwrap();
            guard.hold_initial_acceptance("acceptance_verifier_repair_exhausted","await_acceptance_input_change").await.unwrap();
            let first=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
            let events=guard.store.lock().unwrap().events(guard.id()).unwrap().len();
            guard.hold_initial_acceptance("acceptance_verifier_repair_exhausted","await_acceptance_input_change").await.unwrap();
            let repeated=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(repeated.progress_at_ms,first.progress_at_ms);
            assert_eq!(repeated.checkpoint.binding,first.checkpoint.binding);
            assert_eq!(repeated.checkpoint.next.reason,"acceptance_verifier_repair_exhausted");
            assert_eq!(guard.store.lock().unwrap().events(guard.id()).unwrap().len(),events);
            assert_eq!(repeated.model_calls,0);
            assert_eq!(repeated.tokens,0);
            guard.hold_initial_acceptance("acceptance_verifier_evidence_unknown","await_original_evidence_recovery").await.unwrap();
            let unknown=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(unknown.checkpoint.next.action,"await_original_evidence_recovery");
            guard.hold_initial_acceptance("acceptance_verifier_infrastructure_unavailable","await_verifier_infrastructure_recovery").await.unwrap();
            let infrastructure=guard.store.lock().unwrap().snapshot(guard.id()).unwrap().unwrap();
            assert_eq!(infrastructure.checkpoint.next.reason,"acceptance_verifier_infrastructure_unavailable");
            assert_eq!(infrastructure.checkpoint.next.action,"await_verifier_infrastructure_recovery");
            assert_eq!(infrastructure.model_calls,0);
        }
    }
}

#[cfg(test)]
#[path = "self_improve_hold_outcome_tests.rs"]
mod hold_outcome_tests;

#[cfg(test)]
#[path="self_improve_retry_drop_tests.rs"]
mod retry_drop_tests;
