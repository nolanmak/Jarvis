//! #2488: an own PR that reaches `rescope_remaining_findings` is rescoped
//! from the findings it recorded, instead of resuming the same contract.
//!
//! The checkpoint is written when the bounded paths that could change the
//! contract are spent: the CI scope amendment refused or ran out of rounds
//! (#2454), adjudicated amendment rounds ran out (#2452), the review-attempt
//! cap or an episode rescope. Before this, the next tick resumed the same
//! contract, reached the same refusal and wrote the same checkpoint, forever
//! (PR #2378 at `ci_scope_amendment_refused` with
//! `ci-scope-path:crates/augmentagent-cli/src/notify.rs`).
//!
//! A rescope builds the next contract revision for the same source issue:
//! the recorded `ci-scope-path` files join `allowed_paths` (protected,
//! controller and verifier files never do), criteria, verifiers and harness
//! stay as they are, and review findings an adjudication upheld travel with
//! it as repair notes. The revision is registered through the amendment
//! journal (red measured again at the original base, qualified, activated),
//! bound to the existing branch and head, and the run continues into repair,
//! verification and review in the new scope. At most [`MAX_RESCOPES`] per
//! source issue and original base; after that the workflow stops at one
//! owner-attention checkpoint with one issue comment saying what blocks it.
use super::*;
use crate::autoship_workflow::review_binding_cap::adjudication_round_key;
use crate::autoship_workflow::{Checkpoint, Stage};

/// The checkpoint action this module consumes.
pub(crate) const ACTION: &str = "rescope_remaining_findings";
/// Rescopes per source issue and original base.
pub(crate) const PURPOSE: &str = "own-pr-rescope";
pub(crate) const MAX_RESCOPES: u32 = 2;
/// Registration tries of one rescope (same contract and head): a failed
/// registration that leaves a hold retries it without spending another
/// rescope. Infrastructure refusals (#2508) spend neither.
const TRY_PURPOSE: &str = "own-pr-rescope-try";
const MAX_TRIES: u32 = 3;
/// #2510: registrations started for one rescope (same contract and head),
/// admitted before `register` and handed back only on an infrastructure
/// refusal, so a run that dies mid-registration still counts toward
/// [`MAX_TRIES`].
const STARTED_PURPOSE: &str = "own-pr-rescope-started";
/// One owner comment per blocked head.
const COMMENT_PURPOSE: &str = "own-pr-rescope-comment";
/// Top-level marker of a controller-built rescope request.
pub(crate) const ORIGIN: &str = "own_pr_rescope";
/// The single stop checkpoint. #2371: never an owner-only park. It is a
/// bounded wait autoship revisits: daily, or when main or the issue changes.
pub(crate) const EXHAUSTED_REASON: &str = "own_pr_rescope_exhausted";
pub(crate) const EXHAUSTED_ACTION: &str = "await_request_or_main_change";
const REQUEST_EVIDENCE: &str = "request:";
const MAIN_EVIDENCE: &str = "main:";
const PATH_EVIDENCE: &str = "ci-scope-path:";
/// #2508: written before registration so the rescope stays pending when an
/// infrastructure refusal rewrites the checkpoint to `await_capacity`.
const REGISTERING_EVIDENCE: &str = "own-pr-rescope-registering:";
const NOTES_BYTES: usize = 8000;
const BLOCKER_BYTES: usize = 600;

/// What a `rescope_remaining_findings` checkpoint recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Findings {
    pub reason: String,
    pub paths: Vec<String>,
    pub evidence: Vec<String>,
    pub head: String,
    pub contract: String,
}

/// The findings of a pending rescope, or `None` for any other checkpoint.
/// A rescope whose registration was refused for capacity is still pending
/// once that backoff is due (#2508): it retries the same revision.
pub(crate) fn pending(checkpoint: &Checkpoint) -> Option<Findings> {
    if checkpoint.stage != Stage::Scope {
        return None;
    }
    let reason = if checkpoint.next.action == ACTION {
        checkpoint.next.reason.clone()
    } else if checkpoint.next.reason == "resource_capacity" && checkpoint.next.action == "await_capacity" {
        checkpoint.evidence.iter().find_map(|item| item.strip_prefix(REGISTERING_EVIDENCE)).filter(|reason| !reason.is_empty())?.to_owned()
    } else {
        return None;
    };
    let mut paths: Vec<String> = Vec::new();
    for path in checkpoint.evidence.iter().filter_map(|item| item.strip_prefix(PATH_EVIDENCE)) {
        if !path.is_empty() && !paths.iter().any(|seen| seen == path) {
            paths.push(path.to_owned());
        }
    }
    Some(Findings {
        reason,
        paths,
        evidence: checkpoint.evidence.clone(),
        head: checkpoint.binding.head.clone(),
        contract: checkpoint.binding.contract.clone(),
    })
}

/// How long an unchanged stop waits before autoship re-evaluates it.
pub(crate) const REVISIT_MS: i64 = 24 * 3_600_000;

/// The inputs a stop waits on: the issue's request and canonical main.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Inputs {
    pub request: String,
    pub main: String,
}

/// Whether a stop is still waiting: unchanged main and request, and not yet
/// due for its daily re-evaluation. Anything else re-runs the resume.
pub(crate) fn parked(checkpoint: &Checkpoint, now: i64, inputs: &Inputs) -> bool {
    exhausted(checkpoint)
        && now < checkpoint.next.eligible_at_ms
        && checkpoint.evidence.contains(&format!("{REQUEST_EVIDENCE}{}", inputs.request))
        && checkpoint.evidence.contains(&format!("{MAIN_EVIDENCE}{}", inputs.main))
}

/// Whether the workflow already stopped for the owner.
pub(crate) fn exhausted(checkpoint: &Checkpoint) -> bool {
    checkpoint.stage == Stage::Scope
        && checkpoint.next.reason == EXHAUSTED_REASON
        && checkpoint.next.action == EXHAUSTED_ACTION
}

/// The next revision: the old proposal with each file in `add` as a literal
/// `File` scope. Criteria, verifiers and harness files are unchanged.
pub(crate) fn next_proposal(old: &ContractProposal, add: &[String]) -> Result<ContractProposal> {
    let mut next = old.clone();
    next.contract.revision = old.contract.revision.checked_add(1).context("contract revision overflow")?;
    for path in add {
        if !next.contract.allowed_paths.iter().any(|scope| scope.contains(path)) {
            next.contract.allowed_paths.push(crate::autoship_contract::PathScope::File(path.clone()));
        }
    }
    Ok(next)
}

/// #2508: an infrastructure refusal says nothing about the rescope itself.
/// Native runtime or declared run capacity, deferred CI infrastructure, the
/// disk floor, build admission and verifier infrastructure all retry the
/// same revision after their own backoff, spending neither a rescope nor a
/// try. Registration rejected on its merits, an unreproduced baseline or a
/// refused authority are verdicts, and spend the budget.
pub(super) fn infrastructure_refusal(error: &anyhow::Error, capacity_seen: bool) -> bool {
    capacity_seen
        || error.downcast_ref::<CiInfrastructureDeferred>().is_some()
        || matches!(error.downcast_ref::<initial_repair::Hold>(), Some(initial_repair::Hold::InfrastructureUnavailable))
        || error.chain().any(|cause| {
            matches!(cause.downcast_ref::<augmentagent_channel_core::reasoner::ReasonerError>(),
                Some(augmentagent_channel_core::reasoner::ReasonerError::ResourceCapacity { .. }))
                || cause.downcast_ref::<crate::autoship_workflow::resource_refusal::Observation>().is_some()
                || cause.downcast_ref::<augmentagent_channel_core::storage_reservations::Busy>().is_some()
                || {
                    let text = cause.to_string();
                    text.contains(crate::disk::LOW_DISK_REFUSAL) || text.contains("build admission deferred")
                }
        })
}

/// Spend this rescope's budget: the rescope itself on its first try, and
/// the try. Only a registration that ran to a verdict reaches here.
fn spend(workflow: &workflow::WorkflowGuard, key: &str, attempt: &str, first: bool) -> Result<()> {
    if first {
        ensure!(workflow.admit_epoch_attempt(PURPOSE, key, MAX_RESCOPES)?, "own-PR rescope budget spent concurrently");
    }
    ensure!(workflow.admit_epoch_attempt(TRY_PURPOSE, attempt, MAX_TRIES)?, "own-PR rescope tries spent concurrently");
    Ok(())
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// The rescope budget: per source issue and original base, and per owner
/// request, so an issue edit earns a fresh budget. Main changes do not.
fn key(prepared: &PreparedAcceptance, inputs: &Inputs) -> Result<String> {
    let source = prepared.proposal.contract.source()?;
    let round = adjudication_round_key(&source.key(), prepared.baseline.registration.baseline_head());
    Ok(amendment::digest(&serde_json::to_vec(&("own-pr-rescope-budget-v1", round, &inputs.request))?))
}

/// The paid and remote steps, behind a seam so the controller logic is
/// testable without a native witness or GitHub.
#[async_trait::async_trait]
pub(crate) trait Rescope: Sync {
    /// Register and activate the next revision adding exactly `add`.
    async fn register(
        &self,
        prepared: &PreparedAcceptance,
        workflow: &mut workflow::WorkflowGuard,
        bound: &str,
        add: &[String],
    ) -> Result<PreparedAcceptance>;
    /// Post the owner-attention comment on the source issue.
    async fn notify(&self, workflow: &mut workflow::WorkflowGuard, body: &str) -> Result<bool>;
}

/// Production: the amendment journal's registration and the issue comment.
pub(crate) struct Controller<'a> {
    pub repo_root: &'a Path,
    pub worktree: &'a Path,
    pub control: &'a Path,
    pub pr: u64,
    pub issue: u64,
    pub automatic: bool,
}
#[async_trait::async_trait]
impl Rescope for Controller<'_> {
    async fn register(
        &self,
        prepared: &PreparedAcceptance,
        workflow: &mut workflow::WorkflowGuard,
        bound: &str,
        add: &[String],
    ) -> Result<PreparedAcceptance> {
        amendment::rescope_revision(self.worktree, prepared, self.control, workflow, bound, add).await
    }
    async fn notify(&self, workflow: &mut workflow::WorkflowGuard, body: &str) -> Result<bool> {
        manual_routing::published_note(self.repo_root, workflow, self.pr, self.issue, self.automatic,
            manual_routing::NoteTarget::Issue, body).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The new scope is bound; repair, verify and review continue in it.
    Rescoped { revision: u64, added: Vec<String>, notes: Option<String> },
    /// Stopped at the owner-attention checkpoint, for this reason.
    NeedsOwner(String),
}

pub(crate) async fn run<R: Rescope>(
    rescope: &R,
    prepared: &mut PreparedAcceptance,
    control: &Path,
    findings: &Findings,
    notes: Option<String>,
    inputs: &Inputs,
    workflow: &mut workflow::WorkflowGuard,
) -> Result<Outcome> {
    let logger = workflow.audit_logger();
    let request = ci_scope::request(prepared, &findings.paths);
    let key = key(prepared, inputs)?;
    if request.add.len() > ci_scope::MAX_SCOPE_FILES {
        return needs_owner(rescope, workflow, findings, &key, inputs, format!(
            "the fix needs {} files outside the contract ({}), more than a rescope may add ({})",
            request.add.len(), request.add.join(", "), ci_scope::MAX_SCOPE_FILES)).await;
    }
    if request.add.is_empty() && !request.protected.is_empty() {
        return needs_owner(rescope, workflow, findings, &key, inputs, format!(
            "the fix needs protected, controller or verifier files that autoship never adds to a contract: {}",
            request.protected.join(", "))).await;
    }
    // One rescope per (contract, head): a registration that failed with a
    // hold retries it without spending another one. The budget is spent only
    // once registration reaches a verdict (#2508): an infrastructure refusal
    // spends nothing and retries the same revision after its backoff.
    let attempt = amendment::digest(&serde_json::to_vec(&("own-pr-rescope-try-v1", &key,
        prepared.baseline.registration.contract_hash(), &findings.head))?);
    let tries = workflow.epoch_attempts(TRY_PURPOSE, &attempt)?;
    let first = tries == 0;
    let used = workflow.epoch_attempts(PURPOSE, &key)?;
    if first && used >= MAX_RESCOPES {
        return needs_owner(rescope, workflow, findings, &key, inputs, format!(
            "all {MAX_RESCOPES} rescopes for this issue and original base are spent")).await;
    }
    if tries >= MAX_TRIES {
        return needs_owner(rescope, workflow, findings, &key, inputs, format!(
            "registering the rescoped contract failed {MAX_TRIES} times")).await;
    }
    let number = used + u32::from(first);
    let notes = notes.map(|notes| bounded(&notes, NOTES_BYTES));
    let previous = prepared.baseline.registration.contract_hash().to_owned();
    if !request.add.is_empty() {
        let bound = serde_json::to_string(&serde_json::json!({
            "origin": ORIGIN, "previous_contract_hash": previous, "head": findings.head,
            "reason": findings.reason, "rescope": number, "max_rescopes": MAX_RESCOPES,
            "protected": request.protected, "repair_notes": notes,
            "directives": [{"repair_target": {"kind": "contract_scope", "paths": request.add},
                "evidence": "Recorded findings name these files; the bounded amendments that could add them are spent."}],
        }))?;
        // Keep the rescope recoverable if a capacity refusal rewrites the
        // checkpoint during registration.
        let mut evidence: Vec<String> = findings.evidence.iter()
            .filter(|item| !item.starts_with(REGISTERING_EVIDENCE)).cloned().collect();
        evidence.push(format!("{REGISTERING_EVIDENCE}{}", findings.reason));
        workflow.checkpoint(Stage::Scope, &findings.reason, ACTION, 0, &evidence).await?;
        if !workflow.admit_epoch_attempt(STARTED_PURPOSE, &attempt, MAX_TRIES)? {
            return needs_owner(rescope, workflow, findings, &key, inputs, format!(
                "registering the rescoped contract failed or was interrupted {MAX_TRIES} times")).await;
        }
        let next = match rescope.register(prepared, workflow, &bound, &request.add).await {
            Ok(next) => {
                spend(workflow, &key, &attempt, first)?;
                next
            }
            Err(error) => {
                let capacity = logger.resource_capacity_seen();
                if infrastructure_refusal(&error, capacity) {
                    workflow.release_epoch_attempt(STARTED_PURPOSE, &attempt)?;
                    // Spend nothing. A capacity refusal already holds the
                    // workflow at `await_capacity` with the rescope still
                    // pending; any other keeps it pending for the retry.
                    if !capacity {
                        workflow.checkpoint(Stage::Scope, &findings.reason, ACTION,
                            chrono::Utc::now().timestamp_millis() + 300_000, &evidence).await?;
                    }
                    logger.record_controller(serde_json::json!({"kind":"own_pr_rescope_deferred","reason":findings.reason,
                        "rescope":number,"max_rescopes":MAX_RESCOPES,"capacity":capacity,"spent":false,
                        "head":findings.head,"shipping_authority":false})).ok();
                    return Err(error.context("own-PR rescope registration deferred for infrastructure; no rescope spent"));
                }
                spend(workflow, &key, &attempt, first)?;
                if let Some(hold) = persist_enrolled_acceptance_error(workflow, prepared, &error).await? {
                    return Err(hold.into());
                }
                return needs_owner(rescope, workflow, findings, &key, inputs,
                    format!("the rescoped contract could not be registered: {error:#}")).await;
            }
        };
        if next.proposal.contract.source()? != prepared.proposal.contract.source()?
            || next.proposal.contract.revision <= prepared.proposal.contract.revision
            || !ci_scope::covers(&next, &request.add)
        {
            return needs_owner(rescope, workflow, findings, &key, inputs,
                "the registered revision does not add the requested files".into()).await;
        }
        *prepared = next;
        workflow.bind_acceptance(prepared, control)?;
    } else {
        spend(workflow, &key, &attempt, first)?;
    }
    workflow.transition(Stage::Verify, "own_pr_rescoped", "verify_new_revision", 0).await?;
    logger.record_controller(serde_json::json!({"kind":"own_pr_rescoped","reason":findings.reason,
        "rescope":number,"max_rescopes":MAX_RESCOPES,"added":request.add,"protected":request.protected,
        "previous_contract_hash":previous,"contract_hash":prepared.baseline.registration.contract_hash(),
        "revision":prepared.proposal.contract.revision,"head":findings.head,"repair_notes":notes.is_some(),
        "shipping_authority":false}))?;
    Ok(Outcome::Rescoped { revision: prepared.proposal.contract.revision, added: request.add, notes })
}

/// Stop at the single owner-attention checkpoint and tell the owner once
/// per blocked head what blocks it. The branch and its commits stay.
async fn needs_owner<R: Rescope>(
    rescope: &R,
    workflow: &mut workflow::WorkflowGuard,
    findings: &Findings,
    key: &str,
    inputs: &Inputs,
    blocker: String,
) -> Result<Outcome> {
    let blocker = bounded(&blocker, BLOCKER_BYTES);
    let used = workflow.epoch_attempts(PURPOSE, key)?;
    let mut evidence = vec![format!("own-pr-rescope:{used}/{MAX_RESCOPES}"), format!("blocked-after:{}", findings.reason)];
    evidence.extend(findings.paths.iter().map(|path| format!("{PATH_EVIDENCE}{path}")));
    evidence.push(format!("blocker:{blocker}"));
    evidence.push(format!("{REQUEST_EVIDENCE}{}", inputs.request));
    evidence.push(format!("{MAIN_EVIDENCE}{}", inputs.main));
    workflow.checkpoint(Stage::Scope, EXHAUSTED_REASON, EXHAUSTED_ACTION,
        chrono::Utc::now().timestamp_millis() + REVISIT_MS, &evidence).await?;
    let comment = amendment::digest(&serde_json::to_vec(&("own-pr-rescope-comment-v1", key, &findings.head))?);
    let mut commented = workflow.epoch_attempts(COMMENT_PURPOSE, &comment)? > 0;
    if !commented {
        let files = if findings.paths.is_empty() { String::new() } else {
            format!("\n\nFiles the recorded findings need: {}.", findings.paths.iter().map(|path| format!("`{path}`")).collect::<Vec<_>>().join(", "))
        };
        let body = format!(
            "Autoship stopped working on this issue's open PR and needs a decision from you.\n\n\
             The PR still cannot ship. Blocking: {blocker}. \
             Rescopes used for this issue and original base: {used} of {MAX_RESCOPES}.\n\nLast checkpoint before the stop: `{}`.{files}\n\n\
             The branch and its commits are kept. Autoship looks again daily, or as soon as main or this issue changes; editing the issue starts a fresh rescope budget. You can also push a fix to the branch or close the PR.",
            findings.reason);
        if rescope.notify(workflow, &body).await? {
            workflow.admit_epoch_attempt(COMMENT_PURPOSE, &comment, 1)?;
            commented = true;
        }
    }
    workflow.audit_logger().record_controller(serde_json::json!({"kind":"own_pr_rescope_exhausted","reason":findings.reason,
        "rescopes":used,"max_rescopes":MAX_RESCOPES,"paths":findings.paths,"blocker":blocker,"commented":commented,
        "action":EXHAUSTED_ACTION,"shipping_authority":false}))?;
    Ok(Outcome::NeedsOwner(blocker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const NOTIFY: &str = "crates/augmentagent-cli/src/notify.rs";
    const DIGEST: &str = "crates/augmentagent-cli/src/digest.rs";
    const LEDGER: &str = "crates/augmentagent-cli/src/ledger.rs";
    const WORKFLOW_FILE: &str = ".github/workflows/rust.yml";
    const HARNESS: &str = "autoship_verifiers/observe.py";
    const UPHELD: &str = "- [READY] The digest producer is never registered\n  Required repair: register post_digest in notify::PRODUCERS";

    fn git(root: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("/usr/bin/git")
            .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(args).current_dir(root).env_clear().env("PATH", "/usr/bin:/bin").output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// Measure `proposal` at the original base the way the fixture's red
    /// baseline was measured, and qualify it with the scripted witness.
    fn measured(root: &Path, control: &Path, base: &str, proposal: ContractProposal) -> PreparedAcceptance {
        install_harness(root, &proposal).unwrap();
        let registration = RegisteredContract::register(root, proposal.contract.clone(), base.into(), environment_id(root).unwrap()).unwrap();
        let observed = crate::autoship_contract::Observation { exit_code: Some(0), stdout: "false\n".into(), stderr: String::new(), interrupted: false, output_complete: true };
        let receipts = vec![registration.observe(root, "observe", registration.binding(base).unwrap(), &observed).unwrap()];
        let prepared = PreparedAcceptance { proposal, baseline: BaselineEvidence { registration, receipts } };
        baseline_witness::persist_scripted(root, control, &prepared, true).unwrap();
        prepared
    }

    /// Scripted registration: the production revision shape, measured and
    /// qualified like the fixture's original, with no paid call.
    struct Scripted {
        root: PathBuf,
        control: PathBuf,
        base: String,
        registered: Mutex<Vec<(String, Vec<String>)>>,
        comments: Mutex<Vec<String>>,
        fail: Option<&'static str>,
        /// Registrations refused for native runtime capacity before any succeeds.
        capacity: std::sync::atomic::AtomicU32,
        /// Registrations that never return, like a run killed mid-activation.
        hang: std::sync::atomic::AtomicU32,
    }
    #[async_trait::async_trait]
    impl Rescope for Scripted {
        async fn register(&self, prepared: &PreparedAcceptance, workflow: &mut workflow::WorkflowGuard, bound: &str, add: &[String]) -> Result<PreparedAcceptance> {
            self.registered.lock().unwrap().push((bound.to_owned(), add.to_vec()));
            if self.capacity.fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |left| left.checked_sub(1)).is_ok() {
                // What activation's native run reports when the runtime is full
                // (#2329, 2026-11-23): the capacity observer, then the error.
                workflow.audit_logger().resource_capacity_exhausted(&serde_json::json!({"phase":"native_runtime_state_capacity","exhausted":true,
                    "native_state":{"status":"capacity-exhausted","capacity_fingerprint":"a".repeat(64)}}))?;
                bail!("native runtime capacity exhausted: 8 of 8 GiB reserved");
            }
            if self.hang.fetch_update(std::sync::atomic::Ordering::SeqCst, std::sync::atomic::Ordering::SeqCst, |left| left.checked_sub(1)).is_ok() {
                std::future::pending::<()>().await;
            }
            if let Some(error) = self.fail { bail!("{error}"); }
            Ok(measured(&self.root, &self.control, &self.base, next_proposal(&prepared.proposal, add)?))
        }
        async fn notify(&self, _: &mut workflow::WorkflowGuard, body: &str) -> Result<bool> {
            self.comments.lock().unwrap().push(body.to_owned());
            Ok(true)
        }
    }

    struct Lane {
        root: tempfile::TempDir,
        control: tempfile::TempDir,
        prepared: PreparedAcceptance,
        workflow: workflow::WorkflowGuard,
        rescope: Scripted,
        inputs: Inputs,
    }

    /// PR #2378's shape: issue #2329's own PR, bound to a qualified revision 1
    /// whose allowed paths miss the file its failing required check needs.
    async fn lane(fail: Option<&'static str>) -> Lane {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        let root = tempfile::tempdir_in(&tmp).unwrap();
        let control = tempfile::tempdir_in(&tmp).unwrap();
        std::fs::write(root.path().join("behavior.py"), "READY = False\n").unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-qm", "baseline"]);
        git(root.path(), &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        let base = git(root.path(), &["rev-parse", "HEAD"]);
        let source = AcceptanceSource::Issue(2329);
        let proposal = parse_proposal_for(&format!("```autoship-contract\n{}\n```", serde_json::json!({
            "contract":{"schema_version":1,"revision":1,"issue":2329,"criteria":[{"id":"READY","requirement":"Readiness becomes true","verifier_ids":["observe"]}],
                "allowed_paths":[{"kind":"file","path":"behavior.py"}],
                "verifiers":[{"id":"observe","command":{"kind":"black_box","invocation":{"kind":"python_script","file":HARNESS,"args":[]},"oracle":{"kind":"json","value":true}},"pinned_files":[HARNESS],"failure_witness":"readiness is false"}]},
            "files":{HARNESS:"import json,sys\nfrom pathlib import Path\nsys.path.insert(0,str(Path.cwd()))\nimport behavior\nprint(json.dumps(behavior.READY))\n"}
        })), source).unwrap();
        let prepared = measured(root.path(), control.path(), &base, proposal);
        // The PR's own commits on top of the base.
        std::fs::write(root.path().join("behavior.py"), "READY = True\n").unwrap();
        git(root.path(), &["commit", "-qam", "fix readiness"]);
        std::fs::write(root.path().join("notes.txt"), "digest producer\n").unwrap();
        git(root.path(), &["add", "notes.txt"]);
        git(root.path(), &["commit", "-qm", "add digest producer"]);
        // The issue's own workflow already owns the PR, as in production.
        {
            let mut store = crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.db")).unwrap();
            let id = crate::autoship_workflow::workflow_id("owner/repo", 2329).unwrap();
            let binding = crate::autoship_workflow::Binding { head: "opened".into(), contract: "contract".into(), evidence_hash: "evidence".into() };
            store.register(&id, "agent-fix/issue-2329", binding, 1).unwrap();
            let lease = store.claim(&id, "opener", 2, 100).unwrap().unwrap();
            store.attach_pr_in_repo(&lease, "owner/repo", 2378, 3).unwrap();
            store.release(&lease, 4).unwrap();
        }
        let mut workflow = workflow::WorkflowGuard::acquire_fixture(&control.path().join("workflow.db"), workflow::Start {
            repo_root: root.path(), worktree: root.path(), repository: "owner/repo", source_issue: Some(2329), pr: Some(2378),
            branch: "agent-fix/issue-2329", contract: prepared.baseline.registration.contract_hash(), environment: "environment-a",
        }).await.unwrap().unwrap();
        workflow.bind_acceptance(&prepared, control.path()).unwrap();
        let rescope = Scripted { root: root.path().to_owned(), control: control.path().to_owned(), base, registered: Mutex::new(vec![]), comments: Mutex::new(vec![]), fail, capacity: 0.into(), hang: 0.into() };
        Lane { root, control, prepared, workflow, rescope, inputs: Inputs { request: "request-a".into(), main: "main-a".into() } }
    }

    impl Lane {
        /// What `ci_scope::widen` writes when the amendment refuses `paths`.
        async fn refused(&mut self, paths: &[&str]) -> Findings {
            let mut evidence: Vec<String> = paths.iter().map(|path| format!("{PATH_EVIDENCE}{path}")).collect();
            evidence.push(format!("refusal_sha256:{}", "a".repeat(64)));
            self.workflow.checkpoint(Stage::Scope, "ci_scope_amendment_refused", ACTION, 0, &evidence).await.unwrap();
            pending(&self.workflow.current_checkpoint().unwrap()).expect("a refused CI scope request is a pending rescope")
        }
        async fn run(&mut self, findings: &Findings, notes: Option<&str>) -> Outcome {
            let control = self.control.path().to_owned();
            let inputs = self.inputs.clone();
            run(&self.rescope, &mut self.prepared, &control, findings, notes.map(str::to_owned), &inputs, &mut self.workflow).await.unwrap()
        }
        fn snapshot(&self) -> crate::autoship_workflow::Snapshot {
            crate::autoship_workflow::WorkflowStore::open(&self.control.path().join("workflow.db")).unwrap()
                .snapshot(self.workflow.id()).unwrap().unwrap()
        }
        fn rescopes(&self) -> u32 {
            self.workflow.epoch_attempts(PURPOSE, &key(&self.prepared, &self.inputs).unwrap()).unwrap()
        }
        fn tries(&self, head: &str) -> u32 {
            self.workflow.epoch_attempts(TRY_PURPOSE, &self.attempt(head)).unwrap()
        }
        fn started(&self, head: &str) -> u32 {
            self.workflow.epoch_attempts(STARTED_PURPOSE, &self.attempt(head)).unwrap()
        }
        fn attempt(&self, head: &str) -> String {
            amendment::digest(&serde_json::to_vec(&("own-pr-rescope-try-v1", key(&self.prepared, &self.inputs).unwrap(),
                self.prepared.baseline.registration.contract_hash(), head)).unwrap())
        }
        /// The next autoship tick: a fresh guard once the capacity backoff
        /// is due (the refused guard is unusable, as in production).
        async fn next_tick(&mut self, now: i64) {
            let root = self.root.path().to_owned();
            let fresh = workflow::WorkflowGuard::acquire_fixture_at(&self.control.path().join("workflow.db"), workflow::Start {
                repo_root: &root, worktree: &root, repository: "owner/repo", source_issue: Some(2329), pr: Some(2378),
                branch: "agent-fix/issue-2329", contract: self.prepared.baseline.registration.contract_hash(), environment: "environment-a",
            }, now).await.unwrap().expect("the workflow is claimable once its capacity backoff is due");
            drop(std::mem::replace(&mut self.workflow, fresh));
            self.workflow.bind_acceptance(&self.prepared, self.control.path()).unwrap();
        }
    }

    /// Workflow admissions require a declared run resource policy, which is
    /// process environment: run each lane test in its own child process.
    fn parent(test: &str) -> bool {
        if std::env::var_os("OWN_PR_RESCOPE_CHILD").is_some() {
            return false;
        }
        let home = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child.args(["--exact", &format!("self_improve::acceptance::own_pr_rescope::tests::{test}"), "--nocapture"])
            .env_clear().env("PATH", "/usr/bin:/bin").env("HOME", home.path())
            .env("AUGMENTAGENT_RUN_RESOURCES", "1").env("OWN_PR_RESCOPE_CHILD", "1")
            .env("XDG_STATE_HOME", home.path().join("state"));
        if let Some(tmp) = std::env::var_os("TMPDIR") {
            child.env("TMPDIR", tmp);
        }
        let output = child.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success() && stdout.contains("1 passed"), "{test}: {stdout}\n{}", String::from_utf8_lossy(&output.stderr));
        true
    }

    /// The acceptance case (PR #2378): the CI scope amendment refused
    /// `notify.rs`. The rescope adds exactly that file (never the CI workflow
    /// it also named), keeps every criterion, verifier and harness file,
    /// carries the upheld review finding, binds the new revision and sends the
    /// run on to verification. The CI repair then needs nothing outside the
    /// contract, so it repairs instead of refusing again.
    #[tokio::test]
    async fn a_ci_scope_refused_own_pr_rescopes_with_the_file_added_and_repairs() {
        if parent("a_ci_scope_refused_own_pr_rescopes_with_the_file_added_and_repairs") { return; }
        let mut lane = lane(None).await;
        let before = lane.prepared.clone();
        let binding = lane.snapshot().checkpoint.binding;
        let findings = lane.refused(&[NOTIFY, WORKFLOW_FILE]).await;
        let mut paths = findings.paths.clone();
        paths.sort();
        assert_eq!(paths, vec![WORKFLOW_FILE.to_string(), NOTIFY.to_string()]);
        assert_eq!(findings.reason, "ci_scope_amendment_refused");

        let outcome = lane.run(&findings, Some(UPHELD)).await;
        assert_eq!(outcome, Outcome::Rescoped { revision: 2, added: vec![NOTIFY.into()], notes: Some(UPHELD.into()) });

        let contract = &lane.prepared.proposal.contract;
        assert_eq!(contract.revision, 2, "no new contract revision");
        assert_eq!(contract.source().unwrap(), AcceptanceSource::Issue(2329), "the rescope changed the source issue");
        assert!(contract.allowed_paths.iter().any(|scope| scope.contains(NOTIFY)), "{:?}", contract.allowed_paths);
        assert!(contract.allowed_paths.iter().any(|scope| scope.contains("behavior.py")), "the old scope was dropped");
        assert!(!contract.allowed_paths.iter().any(|scope| scope.contains(WORKFLOW_FILE)), "a protected CI workflow joined the scope");
        assert_eq!((&contract.criteria, &contract.verifiers, &lane.prepared.proposal.files),
            (&before.proposal.contract.criteria, &before.proposal.contract.verifiers, &before.proposal.files),
            "the rescope changed what the PR is accepted against");

        let registered = lane.rescope.registered.lock().unwrap().clone();
        assert_eq!(registered.len(), 1);
        let (bound, add) = &registered[0];
        assert_eq!(add, &vec![NOTIFY.to_string()]);
        let request: serde_json::Value = serde_json::from_str(bound).unwrap();
        assert_eq!(request["origin"], ORIGIN);
        assert_eq!(request["previous_contract_hash"], before.baseline.registration.contract_hash());
        assert_eq!(request["protected"], serde_json::json!([WORKFLOW_FILE]));
        assert!(request["repair_notes"].as_str().unwrap().contains("notify::PRODUCERS"), "the upheld finding was dropped: {bound}");
        amendment::validate_bound_scope_files(Some(bound), &[NOTIFY.to_string()]).expect("the bound request names the file");

        let after = lane.snapshot();
        assert_eq!((after.checkpoint.stage, after.checkpoint.next.reason.as_str(), after.checkpoint.next.action.as_str()),
            (Stage::Verify, "own_pr_rescoped", "verify_new_revision"));
        assert_ne!(after.checkpoint.binding.contract, binding.contract, "the new revision is not bound");
        assert_eq!(lane.rescopes(), 1);
        assert_eq!(after.model_calls, 0);

        // The repair: the failing check's file is now in scope, so the CI
        // scope step lets the existing repair round run instead of refusing.
        let repair = ci_scope::request(&lane.prepared, &[NOTIFY.to_string()]);
        assert!(repair.add.is_empty() && repair.protected.is_empty(), "{repair:?}");
        assert!(pending(&after.checkpoint).is_none(), "the rescope left the workflow pending");
    }

    /// Bounded: two rescopes per source issue and original base. The third
    /// stops at one owner-attention checkpoint with one issue comment naming
    /// what blocks it; re-entering it neither rescopes nor comments again.
    #[tokio::test]
    async fn two_rescopes_then_the_terminal_checkpoint_and_one_comment() {
        if parent("two_rescopes_then_the_terminal_checkpoint_and_one_comment") { return; }
        let mut lane = lane(None).await;
        for (round, file) in [NOTIFY, DIGEST].into_iter().enumerate() {
            let findings = lane.refused(&[file]).await;
            let outcome = lane.run(&findings, None).await;
            assert_eq!(outcome, Outcome::Rescoped { revision: round as u64 + 2, added: vec![file.into()], notes: None });
            assert_eq!(lane.rescopes(), round as u32 + 1);
        }
        let findings = lane.refused(&[LEDGER]).await;
        let Outcome::NeedsOwner(blocker) = lane.run(&findings, None).await else { panic!("a third rescope ran") };
        assert!(blocker.contains("2 rescopes"), "{blocker}");
        let after = lane.snapshot();
        assert_eq!((after.checkpoint.stage, after.checkpoint.next.reason.as_str(), after.checkpoint.next.action.as_str()),
            (Stage::Scope, EXHAUSTED_REASON, EXHAUSTED_ACTION));
        assert!(after.checkpoint.evidence.contains(&format!("own-pr-rescope:{MAX_RESCOPES}/{MAX_RESCOPES}")), "{:?}", after.checkpoint.evidence);
        assert_eq!(EXHAUSTED_ACTION, "await_request_or_main_change", "#2371: the stop must be an automatic wait, never owner-only");
        let now = chrono::Utc::now().timestamp_millis();
        assert!(after.checkpoint.next.eligible_at_ms > now && after.checkpoint.next.eligible_at_ms <= now + REVISIT_MS,
            "the stop is not revisited within a day: {}", after.checkpoint.next.eligible_at_ms);
        assert!(exhausted(&after.checkpoint));
        assert!(parked(&after.checkpoint, now, &lane.inputs), "an unchanged stop was revisited at once");
        assert!(pending(&after.checkpoint).is_none(), "the terminal checkpoint is picked up as another rescope");
        assert_eq!(lane.prepared.proposal.contract.revision, 3, "the refused third rescope changed the contract");
        assert_eq!(lane.rescope.registered.lock().unwrap().len(), 2, "the third rescope registered a revision");

        let comments = lane.rescope.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1, "{comments:?}");
        for expected in ["2 of 2", LEDGER, "ci_scope_amendment_refused", "branch and its commits are kept"] {
            assert!(comments[0].contains(expected), "comment lacks {expected:?}: {}", comments[0]);
        }

        // Re-entry on the same head: still blocked, still one comment.
        let findings = lane.refused(&[LEDGER]).await;
        assert!(matches!(lane.run(&findings, None).await, Outcome::NeedsOwner(_)));
        assert_eq!(lane.rescope.comments.lock().unwrap().len(), 1, "the owner was commented on again");
        assert_eq!(lane.rescope.registered.lock().unwrap().len(), 2);
        assert_eq!(lane.rescopes(), MAX_RESCOPES);
        assert_eq!(lane.snapshot().model_calls, 0);
    }

    /// The rescope binds the existing branch and head: no reset, no rebuild,
    /// every PR commit kept, and the binding names the same head.
    #[tokio::test]
    async fn a_rescope_keeps_the_branch_and_its_commits() {
        if parent("a_rescope_keeps_the_branch_and_its_commits") { return; }
        let mut lane = lane(None).await;
        let head = git(lane.root.path(), &["rev-parse", "HEAD"]);
        let log = git(lane.root.path(), &["log", "--format=%H %s"]);
        let branch = lane.workflow.branch().unwrap();
        let findings = lane.refused(&[NOTIFY]).await;
        assert_eq!(findings.head, head);
        assert!(matches!(lane.run(&findings, Some(UPHELD)).await, Outcome::Rescoped { .. }));
        assert_eq!(git(lane.root.path(), &["rev-parse", "HEAD"]), head, "the rescope moved the PR head");
        assert_eq!(git(lane.root.path(), &["log", "--format=%H %s"]), log, "the rescope rewrote the PR's commits");
        assert_eq!(lane.workflow.branch().unwrap(), branch);
        let after = lane.snapshot();
        assert_eq!(after.checkpoint.binding.head, head, "the new scope is bound to another head");
        assert_eq!(after.pr, Some(2378));
        assert_eq!(after.branch, branch);
        let (bound, _) = lane.rescope.registered.lock().unwrap()[0].clone();
        assert_eq!(serde_json::from_str::<serde_json::Value>(&bound).unwrap()["head"], head);
    }

    /// A fix that needs only protected or controller files cannot be
    /// rescoped automatically: stop for the owner at once, spending nothing.
    #[tokio::test]
    async fn a_fix_needing_only_protected_files_stops_for_the_owner_at_once() {
        if parent("a_fix_needing_only_protected_files_stops_for_the_owner_at_once") { return; }
        let mut lane = lane(None).await;
        let findings = lane.refused(&[WORKFLOW_FILE, "crates/augmentagent-cli/src/self_improve_ci_repair.rs"]).await;
        let Outcome::NeedsOwner(blocker) = lane.run(&findings, None).await else { panic!("a protected file was rescoped") };
        assert!(blocker.contains(WORKFLOW_FILE), "{blocker}");
        assert_eq!(lane.rescopes(), 0);
        assert!(lane.rescope.registered.lock().unwrap().is_empty());
        assert_eq!(lane.rescope.comments.lock().unwrap().len(), 1);
        assert!(exhausted(&lane.snapshot().checkpoint));
    }

    /// A revision that cannot be registered stops for the owner with the
    /// reason, instead of resuming the old contract again.
    #[tokio::test]
    async fn a_registration_refusal_stops_for_the_owner_with_its_reason() {
        if parent("a_registration_refusal_stops_for_the_owner_with_its_reason") { return; }
        let mut lane = lane(Some("amended verifier failed intended-red baseline")).await;
        let findings = lane.refused(&[NOTIFY]).await;
        let Outcome::NeedsOwner(blocker) = lane.run(&findings, None).await else { panic!("an unregistered revision was bound") };
        assert!(blocker.contains("intended-red baseline"), "{blocker}");
        assert_eq!(lane.prepared.proposal.contract.revision, 1);
        assert!(exhausted(&lane.snapshot().checkpoint));
        assert_eq!(lane.rescope.comments.lock().unwrap().len(), 1);
    }

    /// #2371: the stop is a bounded wait autoship revisits itself: once a
    /// day, or as soon as main or the issue's request changes. An issue edit
    /// also earns a fresh rescope budget; the owner comment stays single.
    #[tokio::test]
    async fn the_stop_is_revisited_daily_or_when_main_or_the_issue_changes() {
        if parent("the_stop_is_revisited_daily_or_when_main_or_the_issue_changes") { return; }
        let mut lane = lane(None).await;
        for file in [NOTIFY, DIGEST] {
            let findings = lane.refused(&[file]).await;
            assert!(matches!(lane.run(&findings, None).await, Outcome::Rescoped { .. }));
        }
        let findings = lane.refused(&[LEDGER]).await;
        assert!(matches!(lane.run(&findings, None).await, Outcome::NeedsOwner(_)));
        let stop = lane.snapshot().checkpoint;
        let now = chrono::Utc::now().timestamp_millis();
        let main_moved = Inputs { request: lane.inputs.request.clone(), main: "main-b".into() };
        let issue_edited = Inputs { request: "request-b".into(), main: lane.inputs.main.clone() };
        assert!(parked(&stop, now, &lane.inputs));
        assert!(!parked(&stop, stop.next.eligible_at_ms, &lane.inputs), "the daily re-evaluation never comes");
        assert!(!parked(&stop, now, &main_moved), "a main change does not re-evaluate the stop");
        assert!(!parked(&stop, now, &issue_edited), "an issue edit does not re-evaluate the stop");

        // Main moved: re-evaluated, but the spent budget still holds.
        lane.inputs = main_moved;
        let findings = lane.refused(&[LEDGER]).await;
        assert!(matches!(lane.run(&findings, None).await, Outcome::NeedsOwner(_)));
        assert_eq!(lane.rescope.registered.lock().unwrap().len(), 2);
        // The issue was edited: a fresh budget, and the rescope runs.
        lane.inputs = issue_edited;
        let findings = lane.refused(&[LEDGER]).await;
        assert_eq!(lane.run(&findings, None).await, Outcome::Rescoped { revision: 4, added: vec![LEDGER.into()], notes: None });
        assert_eq!(lane.rescope.comments.lock().unwrap().len(), 1, "the owner was commented on again");
    }

    /// #2329 (PR #2378, 2026-11-23): revision 2 could not activate because
    /// native runtime capacity was exhausted, and those refusals alone spent
    /// the rescope budget. A capacity refusal is infrastructure, not a verdict
    /// on the rescope: any number of them spend neither a rescope nor a try,
    /// the rescope stays pending through the capacity backoff, and once
    /// capacity returns the same revision activates.
    #[tokio::test]
    async fn capacity_refusals_spend_no_rescope_and_the_same_revision_activates_after() {
        if parent("capacity_refusals_spend_no_rescope_and_the_same_revision_activates_after") { return; }
        const REFUSALS: u32 = MAX_TRIES + 2;
        let mut lane = lane(None).await;
        lane.rescope.capacity.store(REFUSALS, std::sync::atomic::Ordering::SeqCst);
        let mut findings = lane.refused(&[NOTIFY]).await;
        let head = findings.head.clone();
        let mut now = chrono::Utc::now().timestamp_millis();
        for refusal in 1..=REFUSALS {
            let control = lane.control.path().to_owned();
            let inputs = lane.inputs.clone();
            let error = run(&lane.rescope, &mut lane.prepared, &control, &findings, None, &inputs, &mut lane.workflow).await
                .expect_err("a capacity refusal stopped for the owner instead of waiting");
            assert!(format!("{error:#}").contains("capacity"), "{error:#}");
            let waiting = lane.snapshot().checkpoint;
            assert_eq!((waiting.stage, waiting.next.action.as_str()), (Stage::Scope, "await_capacity"), "refusal {refusal}");
            assert!(!exhausted(&waiting), "refusal {refusal} stopped the workflow for the owner");
            now = now.max(waiting.next.eligible_at_ms) + 2 * 3_600_000;
            lane.next_tick(now).await;
            assert_eq!(lane.rescopes(), 0, "capacity refusal {refusal} spent a rescope");
            assert_eq!(lane.tries(&head), 0, "capacity refusal {refusal} counted as a registration try");
            assert_eq!(lane.started(&head), 0, "capacity refusal {refusal} kept its started registration");
            findings = pending(&lane.workflow.current_checkpoint().unwrap())
                .unwrap_or_else(|| panic!("the rescope is no longer pending after capacity refusal {refusal}"));
            assert_eq!((findings.reason.as_str(), findings.paths.as_slice(), findings.head.as_str()),
                ("ci_scope_amendment_refused", [NOTIFY.to_string()].as_slice(), head.as_str()));
        }
        assert_eq!(lane.prepared.proposal.contract.revision, 1);
        assert!(lane.rescope.comments.lock().unwrap().is_empty(), "the owner was told a capacity refusal blocks the PR");

        // Capacity is back: the same revision 2 activates, spending one rescope.
        let outcome = lane.run(&findings, None).await;
        assert_eq!(outcome, Outcome::Rescoped { revision: 2, added: vec![NOTIFY.into()], notes: None });
        assert!(lane.prepared.proposal.contract.allowed_paths.iter().any(|scope| scope.contains(NOTIFY)));
        assert_eq!(lane.rescopes(), 1);
        let registered = lane.rescope.registered.lock().unwrap().clone();
        assert_eq!(registered.len() as u32, REFUSALS + 1);
        assert!(registered.iter().all(|(_, add)| add == &vec![NOTIFY.to_string()]), "a retry asked for another revision: {registered:?}");
        let after = lane.snapshot();
        assert_eq!((after.checkpoint.stage, after.checkpoint.next.reason.as_str()), (Stage::Verify, "own_pr_rescoped"));
    }

    /// #2510: a run that dies mid-registration reaches no verdict and no
    /// infrastructure refusal. Each such start still counts, so a revision
    /// whose registration always crashes stops after [`MAX_TRIES`] instead of
    /// retrying forever, and spends no rescope doing it.
    #[tokio::test]
    async fn registrations_interrupted_mid_flight_still_count_toward_the_try_cap() {
        if parent("registrations_interrupted_mid_flight_still_count_toward_the_try_cap") { return; }
        let mut lane = lane(None).await;
        lane.rescope.hang.store(u32::MAX, std::sync::atomic::Ordering::SeqCst);
        let findings = lane.refused(&[NOTIFY]).await;
        let head = findings.head.clone();
        let mut now = chrono::Utc::now().timestamp_millis();
        for crash in 1..=MAX_TRIES {
            let control = lane.control.path().to_owned();
            let inputs = lane.inputs.clone();
            let interrupted = tokio::time::timeout(std::time::Duration::from_millis(500),
                run(&lane.rescope, &mut lane.prepared, &control, &findings, None, &inputs, &mut lane.workflow)).await;
            assert!(interrupted.is_err(), "registration {crash} returned instead of hanging");
            now += 2 * 3_600_000;
            lane.next_tick(now).await;
            assert_eq!(lane.started(&head), crash, "interrupted registration {crash} was not counted");
            assert_eq!(lane.rescopes(), 0, "an interrupted registration spent a rescope");
            assert!(pending(&lane.workflow.current_checkpoint().unwrap()).is_some(), "the rescope is no longer pending after crash {crash}");
        }
        let Outcome::NeedsOwner(blocker) = lane.run(&findings, None).await else { panic!("a fourth interrupted registration was started") };
        assert!(blocker.contains(&format!("{MAX_TRIES} times")), "{blocker}");
        assert_eq!(lane.rescope.registered.lock().unwrap().len() as u32, MAX_TRIES);
        assert!(exhausted(&lane.snapshot().checkpoint));
    }

    #[test]
    fn only_a_rescope_checkpoint_is_pending() {
        let binding = crate::autoship_workflow::Binding { head: "h".into(), contract: "c".into(), evidence_hash: "e".into() };
        let at = |stage, reason: &str, action: &str| Checkpoint { stage, binding: binding.clone(),
            evidence: vec![format!("{PATH_EVIDENCE}{NOTIFY}"), "refusal_sha256:x".into()],
            next: crate::autoship_workflow::NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 } };
        for reason in ["ci_scope_amendment_refused", "ci_scope_rounds_exhausted", "adjudication_rounds_exhausted",
            "review_attempts_exhausted_rescope", "episode_rescope_pending"] {
            let findings = pending(&at(Stage::Scope, reason, ACTION)).unwrap_or_else(|| panic!("{reason} is not a pending rescope"));
            assert_eq!(findings.paths, vec![NOTIFY.to_string()]);
            assert_eq!((findings.head.as_str(), findings.contract.as_str()), ("h", "c"));
        }
        assert!(pending(&at(Stage::Verify, "own_pr_rescoped", "verify_new_revision")).is_none());
        assert!(pending(&at(Stage::Scope, EXHAUSTED_REASON, EXHAUSTED_ACTION)).is_none());
        assert!(exhausted(&at(Stage::Scope, EXHAUSTED_REASON, EXHAUSTED_ACTION)));
        assert!(!exhausted(&at(Stage::Scope, "ci_scope_amendment_refused", ACTION)));
    }

    /// The CI scope amendment answers no, every time, and counts its calls.
    struct Refuses(Mutex<u32>);
    #[async_trait::async_trait]
    impl ci_scope::Authority for Refuses {
        async fn amend(&self, _: &PreparedAcceptance, _: &mut workflow::WorkflowGuard, _: &str) -> Result<Option<PreparedAcceptance>> {
            *self.0.lock().unwrap() += 1;
            Ok(None)
        }
    }

    /// #2329 sequence (PR #2378, 2026-11-23): the required `targeted` check
    /// fails on `notify::PRODUCERS` and the fix needs `notify.rs`, outside the
    /// contract. The CI scope amendment refuses it; the own-PR rescope then
    /// runs from that refusal and adds the file; the CI scope step lets the
    /// repair run without asking the authority again; and the repair's edit
    /// to `notify.rs` is inside the new contract.
    #[tokio::test]
    async fn the_2329_sequence_refused_then_rescoped_then_repair_edits_the_file() {
        if parent("the_2329_sequence_refused_then_rescoped_then_repair_edits_the_file") { return; }
        let mut lane = lane(None).await;
        let control = lane.control.path().to_owned();
        let authority = Refuses(Mutex::new(0));
        let tests = vec!["notify_tests::registry_covers_every_producer_in_the_code".to_string()];
        let failure = ci_scope::Failure { tests: &tests, diagnostics: "add them to notify::PRODUCERS", panic_locations: &[],
            regression: ci_scope::Regression::of(false, Some("ci_selection_contract_required")), main: None };

        // 1. Refused: the CI scope step leaves a pending own-PR rescope.
        let widened = ci_scope::widen(&authority, &mut lane.prepared, &control, &[NOTIFY.to_string()], failure, &mut lane.workflow).await.unwrap();
        assert!(matches!(widened, ci_scope::Widen::Rescoped(_)), "{widened:?}");
        assert_eq!(*authority.0.lock().unwrap(), 1);
        let checkpoint = lane.workflow.current_checkpoint().unwrap();
        assert_eq!((checkpoint.stage, checkpoint.next.reason.as_str()), (Stage::Scope, "ci_scope_amendment_refused"));
        let findings = pending(&checkpoint).expect("a refused CI scope amendment must leave a pending own-PR rescope");
        assert_eq!(findings.paths, vec![NOTIFY.to_string()]);

        // 2. The rescope runs and adds the file.
        let outcome = lane.run(&findings, None).await;
        assert_eq!(outcome, Outcome::Rescoped { revision: 2, added: vec![NOTIFY.into()], notes: None });
        assert!(lane.prepared.proposal.contract.allowed_paths.iter().any(|scope| scope.contains(NOTIFY)));

        // 3. The CI scope step is now in scope: no second authority call.
        let widened = ci_scope::widen(&authority, &mut lane.prepared, &control, &[NOTIFY.to_string()], failure, &mut lane.workflow).await.unwrap();
        assert_eq!(widened, ci_scope::Widen::InScope);
        assert_eq!(*authority.0.lock().unwrap(), 1, "the authority was asked again for a file the rescope added");
        assert!(pending(&lane.workflow.current_checkpoint().unwrap()).is_none(), "the rescope is still pending after it ran");

        // 4. The repair edits notify.rs and every changed file is in scope.
        let root = lane.root.path();
        std::fs::create_dir_all(root.join("crates/augmentagent-cli/src")).unwrap();
        std::fs::write(root.join(NOTIFY), "pub const PRODUCERS: &[&str] = &[\"post_digest\"];\n").unwrap();
        git(root, &["add", NOTIFY]);
        git(root, &["commit", "-qm", "register post_digest in notify::PRODUCERS"]);
        let changed = git(root, &["diff", "--name-only", "HEAD~1", "HEAD"]);
        assert_eq!(changed, NOTIFY);
        for path in changed.lines() {
            assert!(lane.prepared.proposal.contract.allowed_paths.iter().any(|scope| scope.contains(path)),
                "the repair's edit to {path} is outside the rescoped contract");
        }
    }
}
