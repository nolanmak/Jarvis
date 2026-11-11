//! Resolve explicit command targets through the same repository and author
//! policy as unattended execution. Resolution is read-only and spends no models.
use super::*;
use crate::autoship_workflow::submission::{Request, Target};

pub(super) enum Resolved {
    Issue(Issue),
    Pr(adoption::AdoptedPr),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExecutionMode {
    Background,
    Local,
}

tokio::task_local! { pub(super) static COMMAND_LANE: Lane; }

async fn read(repo: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, error) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        run(&gh_bin(), args, repo),
    )
    .await
    .context("target lookup timed out")??;
    anyhow::ensure!(ok, "target lookup failed: {}", truncate(&error, 400));
    Ok(out)
}

async fn authorized_repository(repo: &Path, request: &Request) -> Result<String> {
    let request = request.normalized()?;
    authorized_repository_name(repo,&request.repository).await
}

async fn authorized_repository_name(repo:&Path,requested:&str)->Result<String> {
    let requested=crate::autoship_workflow::submission::normalized_repository(requested)?;
    let repository = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        workflow::canonical_repository(repo),
    )
    .await
    .context("repository identity lookup timed out")??;
    anyhow::ensure!(
        repository.eq_ignore_ascii_case(&requested),
        "requested repository differs from checkout"
    );
    let permission = read(
        repo,
        &[
            "repo",
            "view",
            "--json",
            "viewerPermission",
            "--jq",
            ".viewerPermission",
        ],
    )
    .await?;
    anyhow::ensure!(
        matches!(permission.trim(), "ADMIN" | "MAINTAIN" | "WRITE"),
        "repository write access required"
    );
    Ok(repository)
}

/// Authenticate before looking up a replay; closed targets may replay their
/// original admission, but a revoked repository grant may not.
pub(crate) async fn authorize_command(repo: &Path, request: &Request) -> Result<String> {
    let request=request.normalized()?;
    authorize_repository(repo,&request.repository).await
}

pub(crate) async fn authorize_repository(repo:&Path,repository:&str)->Result<String> {
    authorized_repository_name(repo,repository).await?;
    // Repository identity and permissions above already use authenticated
    // GraphQL. REST has a separate quota; exhausting it must not turn a valid
    // owner control into a misleading authentication refusal.
    let raw = read(repo, &["api", "graphql", "-f",
        "query=query AutoshipViewer { viewer { login } }"]).await?;
    let identity: serde_json::Value = serde_json::from_str(&raw)
        .context("invalid authenticated GitHub identity response")?;
    let actor = identity["data"]["viewer"]["login"].as_str()
        .context("authenticated GitHub actor unavailable")?.trim();
    anyhow::ensure!(!actor.is_empty() && actor.len() <= 100 && actor.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid authenticated GitHub actor");
    Ok(format!("github:{}", actor.to_ascii_lowercase()))
}

pub(crate) async fn validate_command(repo: &Path, request: &Request) -> Result<()> {
    resolve(repo, request).await.map(|_| ())
}

/// Resume is a control request for the deployed worker, not native execution
/// by the operator's terminal. Every productive dispatch still passes the full
/// execution preflight inside that worker, including provider/resource gates.
pub(crate) async fn command_readiness(repo:&Path,request:&Request)->Result<()> {
    request.normalized()?;
    control_readiness(repo).await
}

pub(crate) async fn control_readiness(repo:&Path)->Result<()> {
    crate::autoship_deployment::inspect(repo).await.preflight()
}

pub(super) async fn execution_preflight(repo:&Path,target:Option<Target>,mode:ExecutionMode,
    selected_pr:Option<&adoption::AdoptedPr>)->Result<()> {
    let readiness:Result<()>=async {
        tokio::task::spawn_blocking(augmentagent_channel_core::native_vm::readiness::runtime::preflight)
            .await.context("native runtime preflight task failed")??;
        if let Some(target)=target {
            let builder=workflow_builder(repo,target).await?;
            provider_readiness::inspect_builder(builder.as_ref()).preflight()?;
        } else {provider_readiness::inspect().preflight()?;}
        let resources=augmentagent_channel_core::run_resources::readiness().await;
        preflight_selected_resources(&workflow::path(),selected_pr,&resources,chrono::Utc::now().timestamp_millis())?;
        if mode==ExecutionMode::Background {crate::autoship_deployment::inspect(repo).await.preflight()?;}
        Ok(())
    }.await;
    readiness.context(CommandFailure {code:3,reason:"execution-readiness-blocked"})
}

/// An observation is not an execution reservation. Launch-time R admission
/// remains mandatory independently of this initial workflow preflight.
fn preflight_selected_resources(state:&Path,pr:Option<&adoption::AdoptedPr>,
    resources:&augmentagent_channel_core::run_resources::Readiness,now:i64)->Result<()> {
    use crate::autoship_workflow::{Stage,WorkflowStore};
    if let Some(pr)=pr.filter(|pr|pr.ready_for_review()) {
        if let Some(store)=WorkflowStore::open_read_only(state)? {
            if let Some(id)=store.resolve_pr(&pr.repository,pr.pr)? {
                if let Some(snapshot)=store.snapshot(&id)? {
                    if snapshot.pr==Some(pr.pr) && snapshot.checkpoint.binding.head==pr.head
                        && snapshot.checkpoint.stage==Stage::Verify
                        && matches!(snapshot.checkpoint.next.action.as_str(),"run_verifier"|"retry_verification")
                        && snapshot.checkpoint.next.eligible_at_ms<=now && snapshot.lease_until_ms<=now
                        && !store.owner_control(&id)?.is_some_and(|control|control.paused)
                        && store.pending_owner_messages(&id)?==0
                        && store.pending_operations(&id)?.is_empty()
                        && !store.merge_reconciliation_needed(&id)? {
                        // Shared CI acquires its build-only R reservation at
                        // launch. A subsequent native repair independently
                        // acquires the full native+build reservation before spend.
                        return observed_resource_preflight(resources, resources.declaration_preflight());
                    }
                }
            }
        }
    }
    observed_resource_preflight(resources, resources.preflight())
}

fn observed_resource_preflight(resources: &augmentagent_channel_core::run_resources::Readiness, result: Result<()>) -> Result<()> {
    result.map_err(|error| match crate::autoship_workflow::resource_refusal::Observation::capture(resources) {
        Some(observation) => error.context(observation),
        None => error,
    })
}

#[cfg(test)]
#[path="self_improve_targeted_resource_tests.rs"]
mod resource_tests;

/// Final read-only observation for PR milestone waiters. Closed PRs remain
/// observable on replay, but the repository grant and exact PR identity must hold.
pub(crate) async fn published_pr_head(repo: &Path, repository: &str, pr: u64) -> Result<String> {
    let request=Request {repository:repository.into(),target:Target::Pr(pr),provider:None,
        until:crate::autoship_workflow::submission::Milestone::Pr};
    authorized_repository(repo,&request).await?;
    let raw=read(repo,&["pr","view",&pr.to_string(),"--repo",repository,"--json","number,url,headRefOid"]).await?;
    let value:serde_json::Value=serde_json::from_str(&raw)?;
    let head=value["headRefOid"].as_str().context("published head missing")?;
    anyhow::ensure!(value["number"].as_u64()==Some(pr)
        && value["url"].as_str().is_some_and(|url|url.eq_ignore_ascii_case(&format!("https://github.com/{repository}/pull/{pr}")))
        && matches!(head.len(),40|64) && head.bytes().all(|b|b.is_ascii_hexdigit()),"published PR identity differs");
    Ok(head.to_ascii_lowercase())
}

pub(super) async fn resolve(repo: &Path, request: &Request) -> Result<Resolved> {
    let request = request.normalized()?;
    let repository = authorized_repository(repo, &request).await?;
    let mut allowed = trusted_authors(repo).await;
    if let Some(owner) = repository.split('/').next() {
        allowed.push(owner.into());
    }
    match request.target {
        Target::Issue(number) => {
            let raw = read(
                repo,
                &[
                    "api",
                    &format!("repos/{}/issues/{number}", request.repository),
                ],
            )
            .await?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                value["number"].as_u64() == Some(number)
                    && value["state"] == "open"
                    && value.get("pull_request").is_none_or(|v| v.is_null())
                    && value["html_url"]
                        .as_str()
                        .is_some_and(|url| url.eq_ignore_ascii_case(&format!(
                            "https://github.com/{}/issues/{number}",
                            request.repository
                        ))),
                "issue is closed or its repository/target identity differs"
            );
            let author = value["user"]["login"].as_str().unwrap_or("");
            anyhow::ensure!(
                author_is_trusted(
                    author,
                    value["author_association"].as_str().unwrap_or(""),
                    &allowed
                ),
                "issue author is not authorized for autoship"
            );
            let body = value["body"].as_str().unwrap_or("").to_owned();
            Ok(Resolved::Issue(Issue {
                number,
                title: value["title"]
                    .as_str()
                    .context("issue title missing")?
                    .into(),
                research_filed: is_research_filed(&body),
                body,
                author: author.into(),
                author_trusted: true,
            }))
        }
        Target::Pr(number) => {
            let raw=read(repo,&["pr","view",&number.to_string(),"--repo",&request.repository,"--json",
                "number,state,url,headRefName,headRefOid,baseRefName,isCrossRepository,author,title,body,closingIssuesReferences"]).await?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                value["number"].as_u64() == Some(number)
                    && value["state"] == "OPEN"
                    && value["url"]
                        .as_str()
                        .is_some_and(|url| url.eq_ignore_ascii_case(&format!(
                            "https://github.com/{}/pull/{number}",
                            request.repository
                        ))),
                "PR is closed or its repository/target identity differs"
            );
            let mut candidates =
                adoption::authorized(&serde_json::json!([value]), &request.repository, &allowed);
            let pr = candidates
                .pop()
                .context("PR author or source repository is not authorized for autoship")?;
            anyhow::ensure!(
                matches!(pr.head.len(), 40 | 64) && pr.head.bytes().all(|b| b.is_ascii_hexdigit()),
                "PR head identity missing"
            );
            Ok(Resolved::Pr(pr))
        }
    }
}

/// Remote lifecycle of a command's current target. Identity stays strict: a
/// foreign or renumbered target is an error, never a lifecycle state.
#[derive(Debug, PartialEq, Eq)]
enum Lifecycle {
    Open,
    Merged(String),
    Closed,
}

async fn target_lifecycle(repo: &Path, repository: &str, target: Target) -> Result<Lifecycle> {
    match target {
        Target::Issue(number) => {
            let raw = read(repo, &["api", &format!("repos/{repository}/issues/{number}")]).await?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                value["number"].as_u64() == Some(number)
                    && value.get("pull_request").is_none_or(|v| v.is_null())
                    && value["html_url"].as_str().is_some_and(|url| url
                        .eq_ignore_ascii_case(&format!("https://github.com/{repository}/issues/{number}"))),
                "issue repository/target identity differs"
            );
            match value["state"].as_str() {
                Some("open") => Ok(Lifecycle::Open),
                Some("closed") => Ok(Lifecycle::Closed),
                _ => anyhow::bail!("issue state unknown"),
            }
        }
        Target::Pr(number) => {
            let raw = read(repo, &["pr", "view", &number.to_string(), "--repo", repository,
                "--json", "number,state,url,mergeCommit"]).await?;
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                value["number"].as_u64() == Some(number)
                    && value["url"].as_str().is_some_and(|url| url
                        .eq_ignore_ascii_case(&format!("https://github.com/{repository}/pull/{number}"))),
                "PR repository/target identity differs"
            );
            match value["state"].as_str() {
                Some("OPEN") => Ok(Lifecycle::Open),
                Some("CLOSED") => Ok(Lifecycle::Closed),
                Some("MERGED") => {
                    let commit = value["mergeCommit"]["oid"].as_str()
                        .filter(|sha| matches!(sha.len(), 40 | 64) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
                        .context("merged PR has no merge commit identity")?;
                    Ok(Lifecycle::Merged(commit.to_ascii_lowercase()))
                }
                _ => anyhow::bail!("PR state unknown"),
            }
        }
    }
}

/// Settle a workflow whose target finished outside the loop through the
/// ordinary lease/checkpoint path. No merge operation or allowance dispatch is
/// created, so an observed manual merge never debits the autonomous allowance.
/// Returns false when another owner holds the workflow lease.
fn settle_outside_target(path: &Path, id: &str, stage: crate::autoship_workflow::Stage,
    reason: &str, evidence: String) -> Result<bool> {
    use crate::autoship_workflow::{Binding, NextAction, WorkflowStore};
    let now = chrono::Utc::now().timestamp_millis();
    let mut store = WorkflowStore::open(path)?;
    if store.snapshot(id)?.is_none() {
        // Admitted but never started: give the command a terminal record.
        store.register(id, "unbound", Binding {head:"unbound".into(),
            contract:"target-settled-outside-autoship".into(), evidence_hash:"none".into()}, now)?;
    }
    let Some(lease) = store.claim(id, "autoship-target-lifecycle", now, 60_000)? else { return Ok(false); };
    let mut checkpoint = store.snapshot(id)?.context("workflow disappeared")?.checkpoint;
    checkpoint.stage = stage;
    checkpoint.next = NextAction {reason: reason.into(), action: "complete".into(), eligible_at_ms: 0};
    if checkpoint.evidence.len() < 128 { checkpoint.evidence.push(evidence); }
    let settled = store.checkpoint(&lease, &checkpoint, now);
    store.release(&lease, now)?;
    settled?;
    info!(workflow = id, ?stage, reason, "autoship command target settled outside the loop");
    Ok(true)
}

#[derive(Debug)]
pub(crate) struct CommandFailure {
    pub code: i32,
    pub reason: &'static str,
}
impl std::fmt::Display for CommandFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.reason) }
}
impl std::error::Error for CommandFailure {}

/// A local operator executes the same durable job without depending on a
/// daemon/worker process pair. It still passes every runtime and workflow gate.
pub(crate) async fn work_once(repo: &Path, id: &str) -> Result<RunReport> {
    work_once_mode(repo, id, ExecutionMode::Local).await
}

pub(super) async fn work_once_mode(repo: &Path, id: &str, mode: ExecutionMode) -> Result<RunReport> {
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())?
        .context("no durable autoship state")?;
    let mut request = store
        .command_request(id)?
        .context("no queued command for workflow")?;
    authorize_command(repo, &request).await.context(CommandFailure {code:2, reason:"target-authorization-failed"})?;
    // Pending merge accounting remains readable below; no new worker dispatch
    // may start while the owner pause is set.
    let paused=store.owner_control(id)?.is_some_and(|c|c.paused);
    let mut waiting_for_checks = false;
    if let Some(snapshot) = store.snapshot(id)? {
        use crate::autoship_workflow::Stage;
        if snapshot.lease_until_ms > chrono::Utc::now().timestamp_millis() {
            return Ok(RunReport::held("Workflow has an active owner lease".into()));
        }
        if store.merge_reconciliation_needed(id)? || store.pending_operations(id)?.iter().any(|op| op.kind == crate::autoship_workflow::OperationKind::MergePr) {
            drop(store);
            publication::reconcile_command_merge(repo, &request.repository, id).await?;
            let observed = crate::autoship_workflow::WorkflowStore::open_read_only(&workflow::path())?
                .context("workflow state disappeared during reconciliation")?;
            if observed.snapshot(id)?.is_some_and(|s| s.checkpoint.stage == Stage::Merged)
                && !observed.merge_reconciliation_needed(id)?
                && !observed.pending_operations(id)?.iter().any(|op| op.kind == crate::autoship_workflow::OperationKind::MergePr) {
                return Ok(RunReport::triage("Confirmed merge reconciled without another mutation".into()));
            }
            return Ok(RunReport::held("Merge reconciliation remains pending; no mutation replayed".into()));
        }
        if paused {return Ok(RunReport::held("Owner pause requested; no new dispatch".into()));}
        if snapshot.checkpoint.stage == Stage::Merged {
            return Ok(RunReport::triage("Workflow is already merged".into()));
        }
        if matches!(snapshot.checkpoint.stage, Stage::Held | Stage::Retired | Stage::Merge) {
            return Ok(RunReport::held(format!("Preserved {:?}: {}", snapshot.checkpoint.stage, snapshot.checkpoint.next.reason)));
        }
        if snapshot.checkpoint.next.eligible_at_ms > chrono::Utc::now().timestamp_millis() {
            return Ok(RunReport::held("Workflow retry is not eligible yet".into()));
        }
        if snapshot.checkpoint.stage == Stage::AwaitChecks {
            if snapshot.pr.is_none() || request.until == crate::autoship_workflow::submission::Milestone::Pr {
                return Ok(RunReport::held("Awaiting a bound PR and merge authorization".into()));
            }
            waiting_for_checks = true;
        }
    }
    if paused {return Ok(RunReport::held("Owner pause requested; no new dispatch".into()));}
    let source_issue = if let Target::Issue(n) = request.target {
        Some(n)
    } else {
        // A PR may already belong to an issue workflow even after a branch
        // rename or edited closing references. Preserve that durable source.
        id.rsplit_once('#')
            .map(|(_, number)| number.parse::<u64>())
            .transpose()?
    };
    if let Some(pr) = store.snapshot(id)?.and_then(|state| state.pr) {
        request.target = Target::Pr(pr);
    }
    // Humans may merge or close the target outside the loop. Settle those
    // terminally instead of rejecting (and retrying) forever; a foreign or
    // renumbered identity is still an invalid target.
    match target_lifecycle(repo, &request.repository, request.target).await
        .context(CommandFailure {code:2, reason:"invalid-target"})? {
        Lifecycle::Open => {}
        Lifecycle::Merged(commit) => {
            drop(store);
            let Target::Pr(pr) = request.target else { anyhow::bail!("merged target is not a PR") };
            return Ok(if settle_outside_target(&workflow::path(), id, crate::autoship_workflow::Stage::Merged,
                "external_merge_reconciled", format!("github:pr:{pr}:merged-externally:{commit}"))? {
                RunReport::triage(format!("PR #{pr} was merged outside autoship; recorded as a manual merge"))
            } else {
                RunReport::held("Workflow lease unavailable; external merge reconciliation deferred".into())
            });
        }
        Lifecycle::Closed => {
            drop(store);
            let evidence = match request.target {
                Target::Pr(n) => format!("github:pr:{n}:closed-unmerged"),
                Target::Issue(n) => format!("github:issue:{n}:closed"),
            };
            return Ok(if settle_outside_target(&workflow::path(), id, crate::autoship_workflow::Stage::Retired,
                "target_closed", evidence)? {
                RunReport::triage("Command target was closed outside autoship; workflow retired".into())
            } else {
                RunReport::held("Workflow lease unavailable; closed-target retirement deferred".into())
            });
        }
    }
    let mut selected = resolve(repo, &request).await.context(CommandFailure {code:2, reason:"invalid-target"})?;
    if let (Some(issue), Resolved::Pr(pr)) = (source_issue, &mut selected) {
        anyhow::ensure!(
            pr.source_issue.is_none_or(|n| n == issue),
            "PR source changed from submitted issue"
        );
        pr.source_issue = Some(issue);
    }
    if let Resolved::Issue(issue) = &selected {
        if has_open_agent_pr(repo, issue.number).await.map_err(linked_claims::command_error)? {
            return Err(anyhow::anyhow!("target issue has an unbound or unverified PR; reconcile or select that PR")
                .context(CommandFailure {code:3, reason:"target-pr-reconciliation-required"}));
        }
    }
    drop(store);
    if waiting_for_checks {
        let Resolved::Pr(pr) = selected else { anyhow::bail!("waiting workflow has no bound PR"); };
        if !pr.ready_for_review() {
            return Ok(RunReport::held("PR still has an unresolved base dependency".into()));
        }
        let controller = workspace::prepare(repo, "sweep").await
            .context(CommandFailure {code:3, reason:"reconciliation-workspace-unavailable"})?;
        let merged = merge_sweep_selected(&controller.root, false, Some(&pr)).await;
        return Ok(if merged > 0 {
            RunReport::triage("Selected PR reconciled using recorded approval".into())
        } else {
            RunReport::held("Selected PR awaits current CI, review or merge admission".into())
        });
    }
    COMMAND_LANE
        .scope(
            Lane::Combined,
            run_selected(repo, false, Some(selected), mode),
        )
        .await.map_err(linked_claims::command_error)
}

pub(super) async fn execute_pr(
    repo: &Path,
    pr: adoption::AdoptedPr,
    dry_run: bool,
) -> Result<RunReport> {
    if !pr.ready_for_review() {
        return Ok(RunReport::held(format!(
            "PR #{}: base dependency {:?}",
            pr.pr, pr.dependency
        )));
    }
    let builder = workflow_builder(repo, Target::Pr(pr.pr)).await?;
    if let Some(issue) = pr.source_issue {
        resume_draft_pr(repo, &builder, pr.pr, issue, &pr.branch, dry_run, Some(&pr.head)).await
    } else {
        adopted_execution::resume(repo, &builder, pr, dry_run).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::submission::Milestone;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn explicit_targets_are_authorized_and_bound_before_execution() {
        const CHILD: &str = "AUTOSHIP_TARGET_RESOLUTION_FIXTURE";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let request = Request {
                repository: "owner/repo".into(),
                target: Target::Issue(7),
                provider: None,
                until: Milestone::Merged,
            };
            let scenario = |value: serde_json::Value| {
                std::fs::write(root.join("scenario.json"), value.to_string()).unwrap()
            };
            let issue = serde_json::json!({"number":7,"state":"open","html_url":"https://github.com/owner/repo/issues/7","title":"Fix addition","body":"Correct arithmetic","user":{"login":"owner"},"author_association":"OWNER"});
            scenario(serde_json::json!({"permission":"WRITE","issue":issue}));
            assert_eq!(authorize_command(&root, &request).await.unwrap(), "github:owner",
                "REST quota exhaustion must not invalidate authenticated GraphQL identity");
            for actor in [serde_json::json!(""), serde_json::Value::Null, serde_json::json!("owner/other"), serde_json::json!("owner\nother")] {
                scenario(serde_json::json!({"permission":"WRITE","issue":issue,"actor":actor}));
                assert!(authorize_command(&root, &request).await.is_err(), "invalid actor {actor:?}");
            }
            scenario(serde_json::json!({"permission":"READ","issue":issue}));
            assert!(authorize_command(&root, &request).await.is_err(), "read-only access cannot authorize controls");
            scenario(serde_json::json!({"permission":"WRITE","issue":issue}));
            let Resolved::Issue(selected) = resolve(&root, &request).await.unwrap() else {
                panic!("wrong target kind")
            };
            assert_eq!(selected.number, 7);
            assert!(selected.author_trusted);
            for invalid in [
                serde_json::json!({"permission":"READ","issue":issue}),
                serde_json::json!({"permission":"WRITE","issue":{"number":8,"state":"open"}}),
                serde_json::json!({"permission":"WRITE","issue":{"number":7,"state":"open","html_url":"https://github.com/foreign/repo/issues/7"}}),
            ] {
                scenario(invalid);
                assert!(resolve(&root, &request).await.is_err());
            }
            let mut untrusted = issue.clone();
            untrusted["user"]["login"] = "stranger".into();
            untrusted["author_association"] = "NONE".into();
            scenario(serde_json::json!({"permission":"WRITE","issue":untrusted}));
            assert!(resolve(&root, &request).await.is_err());
            let mut pr_request = request.clone();
            pr_request.target = Target::Pr(42);
            let pr = serde_json::json!({"number":42,"state":"OPEN","url":"https://github.com/owner/repo/pull/42","headRefName":"repair","headRefOid":"a".repeat(40),"baseRefName":"main","isCrossRepository":false,"author":{"login":"owner"},"title":"Repair addition","body":"Fix arithmetic","closingIssuesReferences":[]});
            scenario(serde_json::json!({"permission":"WRITE","pr":pr}));
            let Resolved::Pr(selected) = resolve(&root, &pr_request).await.unwrap() else {
                panic!("wrong target kind")
            };
            assert_eq!(selected.pr, 42);
            assert!(selected.source_issue.is_none());
            for key in ["state", "isCrossRepository"] {
                let mut invalid = pr.clone();
                invalid[key] = if key == "state" {
                    "CLOSED".into()
                } else {
                    true.into()
                };
                scenario(serde_json::json!({"permission":"WRITE","pr":invalid}));
                assert!(resolve(&root, &pr_request).await.is_err());
            }
            assert!(
                !root.join("state").exists(),
                "resolution created control state"
            );
            assert!(
                !root.join(".self-improve-worktrees").exists(),
                "resolution prepared a checkout"
            );
            // The worker persists a readiness deferral even when no executing
            // workflow was created; a fresh status reader must explain it.
            scenario(serde_json::json!({"permission":"WRITE","issue":issue}));
            let id=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap()
                .submit_authorized("worker-readiness","github:owner",&request,1).unwrap().workflow;
            let error=run_once(&root,false).await.err().expect("missing runtime should defer");
            assert_eq!(error.downcast_ref::<CommandFailure>().unwrap().reason,"execution-readiness-blocked");
            let (code,status)=crate::autoship_commands::status(&workflow::path(),&id,chrono::Utc::now().timestamp_millis(),5);
            assert_eq!(code,3);assert_eq!(status["reason"],"execution-readiness-blocked");
            assert_eq!(status["last_attempt"]["outcome"],"deferred");
            assert_eq!(status["worker"]["state"],"unknown");
            assert!(status["head"].is_null());
            assert!(!root.join(".self-improve-worktrees").exists());
            let context=crate::autoship_discord_command::Context {origin:crate::autoship_workflow::discord_ingress::DiscordOrigin {
                bot:11,owner:22,channel:33,message:44},guild:None,owner_dm_verified:true};
            let policy=crate::autoship_discord_command::OwnerPolicy {bot:11,owner:22};
            scenario(serde_json::json!({"permission":"WRITE","pr":pr}));
            let text="!autoship https://github.com/owner/repo/pull/42 --provider codex";
            // Exercise the same dispatcher and CLI adapter used by WikiQuerier,
            // with a scripted transport and real durable workflow storage.
            use augmentagent_approval_discord::{autoship as gateway,QueryHandler};
            struct Adapter {repo:PathBuf,path:PathBuf,progress:std::sync::atomic::AtomicBool}
            #[async_trait::async_trait]
            impl QueryHandler for Adapter {
                async fn answer(&self,_:&augmentagent_approval_discord::AuditCtx,_:&str)->Result<String> {panic!("autoship reached ordinary chat")}
                fn autoship_ack_uncertain(&self,b:u64,o:u64)->Result<Vec<gateway::UncertainAcknowledgement>> {crate::autoship_discord_command::ack_uncertain(&self.path,b,o)}
                fn autoship_progress_uncertain(&self,b:u64,o:u64)->Result<Vec<gateway::UncertainProgress>> {if self.progress.load(std::sync::atomic::Ordering::SeqCst) {crate::autoship_discord_command::progress_uncertain(&self.path,b,o)} else {Ok(Vec::new())}}
                fn autoship_progress_pending(&self,b:u64,o:u64)->Result<Vec<gateway::ProgressDelivery>> {if self.progress.load(std::sync::atomic::Ordering::SeqCst) {crate::autoship_discord_command::progress_pending(&self.path,b,o)} else {Ok(Vec::new())}}
                fn autoship_progress_claim(&self,d:&gateway::ProgressDelivery)->Result<Option<gateway::Reply>> {crate::autoship_discord_command::progress_claim(&self.path,d)}
                fn autoship_progress_confirm(&self,d:&gateway::ProgressDelivery,m:u64)->Result<()> {crate::autoship_discord_command::progress_confirm(&self.path,d,m)}
                fn autoship_bound(&self,c:&gateway::Context)->Result<bool> {crate::autoship_discord_command::gateway_bound(&self.path,c)}
                async fn autoship_command(&self,c:&gateway::Context,t:&str)->Result<Option<gateway::Reply>> {
                    crate::autoship_discord_command::gateway_command(&self.repo,&self.path,c,t).await
                }
                fn autoship_acknowledged(&self,c:&gateway::Context,m:u64)->Result<()> {crate::autoship_discord_command::gateway_acknowledged(&self.path,c,m)}
                fn autoship_pending(&self,b:u64,o:u64)->Result<Vec<gateway::Context>> {crate::autoship_discord_command::gateway_pending(&self.path,b,o)}
                fn autoship_claim_ack(&self,c:&gateway::Context)->Result<Option<gateway::Reply>> {crate::autoship_discord_command::gateway_claim_ack(&self.path,c)}
            }
            struct Wire(std::sync::atomic::AtomicUsize,std::sync::atomic::AtomicBool,std::sync::atomic::AtomicBool);
            #[async_trait::async_trait]
            impl gateway::Transport for Wire {
                async fn verify_owner_dm(&self,_:&gateway::Context)->Result<bool> {Ok(true)}
                async fn find_acknowledgement(&self,_:&gateway::Context,_:&gateway::Reply)->Result<Option<u64>> {Ok(self.2.load(std::sync::atomic::Ordering::SeqCst).then_some(55))}
                async fn find_progress(&self,_:&gateway::Context,_:&str,_:&str)->Result<Option<u64>> {Ok(self.2.load(std::sync::atomic::Ordering::SeqCst).then_some(55))}
                async fn send(&self,_:&gateway::Context,r:&gateway::Reply)->Result<u64> {
                    self.0.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                    assert!(r.content.contains("Completion has not been observed") || r.content.contains("has not edited the PR") || r.content.starts_with("Autoship"));
                    anyhow::ensure!(!self.1.load(std::sync::atomic::Ordering::SeqCst),"lost response");Ok(55)
                }
            }
            let adapter=Adapter {repo:root.clone(),path:workflow::path(),progress:std::sync::atomic::AtomicBool::new(false)};
            let wire=Wire(std::sync::atomic::AtomicUsize::new(0),std::sync::atomic::AtomicBool::new(false),std::sync::atomic::AtomicBool::new(false));
            let gateway_context=gateway::Context {bot:11,owner:Some(22),author:22,channel:33,message:44,guild:None};
            assert!(gateway::dispatch(&adapter,&wire,&gateway_context,text).await.unwrap());
            assert!(gateway::dispatch(&adapter,&wire,&gateway_context,text).await.unwrap());
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),1);
            let discord=crate::autoship_discord_command::admit(&root,&workflow::path(),policy,&context,text).await.unwrap();
            assert!(discord.admission.accepted);assert!(!discord.admission.complete);
            let mut closed=pr.clone();closed["state"]="CLOSED".into();
            scenario(serde_json::json!({"permission":"WRITE","pr":closed}));
            assert_eq!(crate::autoship_discord_command::admit(&root,&workflow::path(),policy,&context,text).await.unwrap(),discord);
            assert!(gateway::dispatch(&adapter,&wire,&gateway_context,text).await.unwrap());
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),1);
            scenario(serde_json::json!({"permission":"READ","pr":closed}));
            assert!(gateway::dispatch(&adapter,&wire,&gateway::Context {message:45,..gateway_context.clone()},"fix the test").await.unwrap());
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),2);
            scenario(serde_json::json!({"permission":"READ","pr":closed}));
            assert!(crate::autoship_discord_command::admit(&root,&workflow::path(),policy,&context,text).await.is_err());
            scenario(serde_json::json!({"permission":"WRITE","pr":pr}));
            let restart_context=crate::autoship_discord_command::Context {origin:crate::autoship_workflow::discord_ingress::DiscordOrigin {message:46,..context.origin.clone()},guild:None,owner_dm_verified:true};
            crate::autoship_discord_command::admit(&root,&workflow::path(),policy,&restart_context,text).await.unwrap();
            // No gateway event replay: the recovery adapter reopens persisted
            // queued acknowledgements and uses the same atomic dispatch claim.
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),1);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),3);
            let control_context=|message|gateway::Context {message,..gateway_context.clone()};
            assert!(gateway::dispatch(&adapter,&wire,&control_context(100),"!autoship pause").await.unwrap());
            assert!(gateway::dispatch(&adapter,&wire,&control_context(100),"!autoship pause").await.unwrap());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            let paused=store.owner_control(&discord.admission.workflow).unwrap().unwrap();
            assert!(paused.paused);assert_eq!(paused.revision,1);drop(store);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),4);
            assert!(gateway::dispatch(&adapter,&wire,&control_context(101),"!autoship resume").await.unwrap());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            let digest=|text:&str| {use sha2::{Digest,Sha256};format!("{:x}",Sha256::digest(text.as_bytes()))};
            let origin=|message|crate::autoship_workflow::discord_ingress::DiscordOrigin {message,..context.origin.clone()};
            assert_eq!(store.replay_discord_control(&origin(101),&digest("!autoship resume")).unwrap().unwrap().reason,"execution-readiness-blocked");
            assert_eq!(store.owner_control(&discord.admission.workflow).unwrap().unwrap(),paused);drop(store);
            assert!(gateway::dispatch(&adapter,&wire,&control_context(102),"!autoship status").await.unwrap());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            let status=store.replay_discord_control(&origin(102),&digest("!autoship status")).unwrap().unwrap();
            assert!(status.reply.contains("owner-pause-requested"));assert!(status.reply.contains("Deployment: unknown"));drop(store);
            let select=format!("!autoship use {}",discord.admission.workflow);
            assert!(gateway::dispatch(&adapter,&wire,&control_context(103),&select).await.unwrap());
            scenario(serde_json::json!({"permission":"READ","pr":pr}));
            assert!(gateway::dispatch(&adapter,&wire,&control_context(104),"!autoship pause").await.unwrap());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert_eq!(store.replay_discord_control(&origin(104),&digest("!autoship pause")).unwrap().unwrap().reason,"target-authorization-failed");
            assert_eq!(store.owner_control(&discord.admission.workflow).unwrap().unwrap(),paused);drop(store);
            scenario(serde_json::json!({"permission":"WRITE","pr":pr}));
            // A permission recovery cannot turn the same refused message into a mutation.
            assert!(gateway::dispatch(&adapter,&wire,&control_context(104),"!autoship pause").await.unwrap());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert_eq!(store.owner_control(&discord.admission.workflow).unwrap().unwrap(),paused);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),8);
            drop(store);adapter.progress.store(true,std::sync::atomic::Ordering::SeqCst);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),1);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            let mut store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert_eq!(store.progress_delivery(&discord.admission.workflow).unwrap().state,"delivery-confirmed");
            store.set_owner_pause(&discord.admission.workflow,"owner",1,true,200).unwrap();
            // An unchanged pause dedupes; a real new resume request advances progress.
            store.set_owner_pause(&discord.admission.workflow,"owner",2,false,201).unwrap();drop(store);
            wire.1.store(true,std::sync::atomic::Ordering::SeqCst);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),10);
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert_eq!(store.progress_delivery(&discord.admission.workflow).unwrap().state,"delivery-uncertain");
            drop(store);wire.2.store(true,std::sync::atomic::Ordering::SeqCst);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),1);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),10,"reconciliation must never send another message");
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert_eq!(store.progress_delivery(&discord.admission.workflow).unwrap().state,"delivery-confirmed");
            drop(store);adapter.progress.store(false,std::sync::atomic::Ordering::SeqCst);
            wire.2.store(false,std::sync::atomic::Ordering::SeqCst);
            assert!(gateway::dispatch(&adapter,&wire,&control_context(105),"!autoship pause").await.is_err());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            let revision=store.owner_control(&discord.admission.workflow).unwrap().unwrap().revision;
            assert_eq!(store.uncertain_discord_acks(11,22).unwrap().len(),1);drop(store);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            wire.2.store(true,std::sync::atomic::Ordering::SeqCst);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),1);
            assert_eq!(gateway::drain_queued(&adapter,&wire,11,22).await.unwrap(),0);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),11);
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert!(store.uncertain_discord_acks(11,22).unwrap().is_empty());
            assert_eq!(store.owner_control(&discord.admission.workflow).unwrap().unwrap().revision,revision);
            drop(store);wire.1.store(false,std::sync::atomic::Ordering::SeqCst);
            let followup="Please fix the failing test";
            assert!(gateway::dispatch(&adapter,&wire,&control_context(106),followup).await.unwrap());
            assert!(gateway::dispatch(&adapter,&wire,&control_context(106),followup).await.unwrap());
            let store=crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
            assert_eq!(store.pending_owner_messages(&discord.admission.workflow).unwrap(),0,"unavailable native consumer must not queue messages or strand a workflow");
            assert_eq!(store.owner_control(&discord.admission.workflow).unwrap().unwrap().revision,revision);
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst),13);





            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
                .status
                .success())
        };
        git(&["init", "-q"]);
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let gh = root.join("gh.py");
        std::fs::write(
            &gh,
            r#"#!/usr/bin/env python3
import json,os,pathlib,sys
root=pathlib.Path(os.environ['AUTOSHIP_TARGET_RESOLUTION_FIXTURE'])
a=sys.argv[1:];s=json.loads((root/'scenario.json').read_text())
if a[:2]==['repo','view']:
 print(s['permission'] if 'viewerPermission' in a else 'owner/repo')
elif a[:2]==['api','user']:
 print('API rate limit exceeded',file=sys.stderr);sys.exit(1)
elif a[:2]==['api','graphql']: print(json.dumps({'data':{'viewer':{'login':s.get('actor','owner')}}}))
elif a[:2]==['pr','list']: print('[]')
elif a[:1]==['api'] and a[1]=='repos/owner/repo/issues/7':print(json.dumps(s['issue']))
elif a[:2]==['pr','view'] and a[2]=='42':print(json.dumps(s['pr']))
else:raise Exception('unexpected request '+repr(a))
"#,
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","self_improve::targeted::tests::explicit_targets_are_authorized_and_bound_before_execution","--nocapture"])
            .env_remove("AUGMENTAGENT_NATIVE_VM_POLICY")
            .env(CHILD,root).env("GH_BIN",gh).env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS","owner")
            .env("XDG_STATE_HOME",root.join("state")).env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",root.join("state/workflow.db"))
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    /// A command job's bound target can change state outside the loop (a human
    /// merges or closes the PR, or closes the issue). Those settle the workflow
    /// terminally; a genuine identity mismatch still refuses, but only that job.
    #[tokio::test]
    async fn bound_target_lifecycle_settles_without_failing_the_tick() {
        const CHILD: &str = "AUTOSHIP_TARGET_LIFECYCLE_FIXTURE";
        if let Some(root) = std::env::var_os(CHILD) {
            use crate::autoship_workflow::{Binding, NextAction, Stage, WorkflowStore};
            let root = PathBuf::from(root);
            let issue = |n: u64, state: &str| serde_json::json!({"number":n,"state":state,
                "html_url":format!("https://github.com/owner/repo/issues/{n}"),"title":"Fix","body":"Fix it",
                "user":{"login":"owner"},"author_association":"OWNER"});
            let pr = |n: u64, state: &str, repository: &str| serde_json::json!({"number":n,"state":state,
                "url":format!("https://github.com/{repository}/pull/{n}"),"headRefName":format!("agent/{n}"),
                "headRefOid":"a".repeat(40),"baseRefName":"main","isCrossRepository":false,"author":{"login":"owner"},
                "title":"Fix","body":"Fix it","closingIssuesReferences":[],
                "mergeCommit":if state=="MERGED" {serde_json::json!({"oid":"c".repeat(40)})} else {serde_json::Value::Null}});
            std::fs::write(root.join("scenario.json"), serde_json::json!({"permission":"WRITE",
                "issues":{"7":issue(7,"open"),"8":issue(8,"open"),"9":issue(9,"closed"),"10":issue(10,"open"),"11":issue(11,"open")},
                "prs":{"42":pr(42,"MERGED","owner/repo"),"43":pr(43,"CLOSED","owner/repo"),"44":pr(44,"OPEN","foreign/repo")}}).to_string()).unwrap();
            let path = workflow::path();
            let mut store = WorkflowStore::open(&path).unwrap();
            let mut ids = Vec::new();
            // (issue, bound PR): merged by hand, closed unmerged, closed issue,
            // foreign PR identity, and an ordinary healthy job queued last.
            for (at, (number, bound)) in [(7, Some(42)), (8, Some(43)), (9, None), (10, Some(44)), (11, None)].into_iter().enumerate() {
                let request = Request {repository:"owner/repo".into(),target:Target::Issue(number),provider:None,until:Milestone::Merged};
                let id = store.submit_authorized(&format!("job-{number}"), "github:owner", &request, at as i64 + 1).unwrap().workflow;
                if number != 11 {
                    store.register(&id, &format!("agent/{number}"), Binding {head:"b".repeat(40),contract:"c1".into(),evidence_hash:"e1".into()}, 10).unwrap();
                    let lease = store.claim(&id, "setup", 20, 1000).unwrap().unwrap();
                    if let Some(pr) = bound { store.attach_pr(&lease, pr, 21).unwrap(); }
                    let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
                    checkpoint.stage = Stage::Verify;
                    checkpoint.next = NextAction {reason:"verification_infrastructure_retry".into(),action:"retry_verification".into(),eligible_at_ms:0};
                    store.checkpoint(&lease, &checkpoint, 22).unwrap();
                    store.release(&lease, 23).unwrap();
                }
                ids.push(id);
            }
            drop(store);
            let mut reports = Vec::new();
            for _ in &ids {
                // One command turn, then the alternating automatic-lane turn
                // (which defers on the fixture's missing runtime).
                reports.push(run_once(&root, false).await.map(|r| r.to_string()).map_err(|e| format!("{e:#}")));
                let _ = run_once(&root, false).await;
            }
            let store = WorkflowStore::open(&path).unwrap();
            let stage = |id: &str| store.snapshot(id).unwrap().unwrap().checkpoint;
            let now = chrono::Utc::now().timestamp_millis();
            // Merged outside the loop: complete, recorded as a manual merge,
            // never debiting the autonomous allowance.
            assert!(reports[0].is_ok(), "manual merge must not fail the tick: {:?}", reports[0]);
            assert_eq!(stage(&ids[0]).stage, Stage::Merged);
            assert_eq!(stage(&ids[0]).next.reason, "external_merge_reconciled");
            assert!(stage(&ids[0]).evidence.iter().any(|e| e == &format!("github:pr:42:merged-externally:{}", "c".repeat(40))));
            let (_, status) = crate::autoship_commands::status(&path, &ids[0], now, 5);
            assert_eq!(status["complete"], true, "{status}");
            assert_eq!(status["milestones"]["merged"], true);
            let allowance = store.merge_allowance(now, 5).unwrap();
            assert_eq!((allowance.confirmed, allowance.available), (0, 5), "manual merge debited the autonomous allowance");
            let progress=store.progress_events(&ids[0]).unwrap();
            let merged=progress.last().unwrap();
            assert_eq!(merged.kind,"merged","manual completion must not look like unresolved merge reconciliation");
            assert_eq!(merged.merge_state,"observed-external-merge");
            assert_eq!(merged.deployment_state,"unknown","merge observation is not deployment proof");
            // Closed unmerged PR and closed issue: terminal, never retried.
            for id in [&ids[1], &ids[2]] {
                assert_eq!(stage(id).stage, Stage::Retired, "{id}");
                assert_eq!(stage(id).next.reason, "target_closed", "{id}");
                let events = store.progress_events(id).unwrap();
                let terminal = events.last().expect("closed target has durable progress");
                assert_eq!(terminal.kind, "retired");
                assert_eq!(terminal.reason, "target_closed", "private updates preserve the actual terminal blocker");
            }
            assert!(reports[1].is_ok() && reports[2].is_ok(), "{reports:?}");
            // A genuinely foreign identity still refuses, but only that job.
            assert!(reports[3].is_ok(), "one invalid job failed the whole tick: {:?}", reports[3]);
            assert_eq!(stage(&ids[3]).stage, Stage::Verify);
            let rejected = store.command_attempt(&ids[3]).unwrap().unwrap();
            assert_eq!(rejected.reason, "invalid-target");
            assert_eq!(rejected.outcome, crate::autoship_workflow::submission::AttemptOutcome::Rejected);
            assert!(rejected.eligible_at_ms > rejected.selected_at_ms, "rejection must back off");
            // The healthy job behind it was still selected and passed target
            // resolution (it only defers on the fixture's missing runtime).
            assert_eq!(store.command_attempt(&ids[4]).unwrap().unwrap().reason, "execution-readiness-blocked", "{reports:?}");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for args in [&["init", "-q"][..], &["remote", "add", "origin", "https://github.com/owner/repo.git"]] {
            assert!(std::process::Command::new("git").args(args).current_dir(root).output().unwrap().status.success());
        }
        let gh = root.join("gh.py");
        std::fs::write(&gh, r#"#!/usr/bin/env python3
import json,os,pathlib,sys
root=pathlib.Path(os.environ['AUTOSHIP_TARGET_LIFECYCLE_FIXTURE'])
a=sys.argv[1:];s=json.loads((root/'scenario.json').read_text())
if a[:2]==['repo','view']: print(s['permission'] if 'viewerPermission' in a else 'owner/repo')
elif a[:2]==['api','user']: print('owner')
elif a[:2]==['api','graphql']: print(json.dumps({'data':{'viewer':{'login':'owner'}}}))
elif a[:2]==['pr','list']: print('[]')
elif a[:1]==['api'] and a[1].startswith('repos/owner/repo/issues/'): print(json.dumps(s['issues'][a[1].rsplit('/',1)[1]]))
elif a[:2]==['pr','view']: print(json.dumps(s['prs'][a[2]]))
else: raise Exception('unexpected request '+repr(a))
"#).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "self_improve::targeted::tests::bound_target_lifecycle_settles_without_failing_the_tick", "--nocapture"])
            .env_remove("AUGMENTAGENT_NATIVE_VM_POLICY")
            .env(CHILD, root).env("GH_BIN", gh).env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner")
            .env("XDG_STATE_HOME", root.join("state")).env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("state/workflow.db"))
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }

    #[tokio::test]
    async fn local_execution_cannot_skip_deployment_without_an_explicit_target() {
        let dir = tempfile::tempdir().unwrap();
        let error = run_selected(dir.path(), false, None, ExecutionMode::Local)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "local execution requires an authorized target"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    #[ignore = "read-only admission probe requiring the normally deployed worker"]
    async fn live_resume_uses_worker_readiness_from_a_nondelegated_operator() {
        let repo=PathBuf::from(std::env::var_os("AUGMENTAGENT_LIVE_REPO").expect("production repository required"));
        crate::autoship_deployment::inspect(&repo).await.preflight().expect("live worker must match its deployment configuration");
        assert!(!augmentagent_channel_core::run_resources::readiness().await.permits_attempt,
            "this regression must run from a nondelegated operator context");
        let request=Request {repository:"nolanmak/jarvis".into(),target:Target::Pr(1669),provider:None,
            until:crate::autoship_workflow::submission::Milestone::Merged};
        let result=command_readiness(&repo,&request).await;
        assert!(result.is_ok(),"a control request for the deployed worker must not require the operator's native execution delegation: {result:?}");
    }

    #[tokio::test]
    async fn command_lane_scope_restores_the_background_lane() {
        let original = lane_from_env();
        COMMAND_LANE
            .scope(Lane::Resume, async {
                assert_eq!(lane_from_env(), Lane::Resume);
                COMMAND_LANE
                    .scope(Lane::Combined, async {
                        assert_eq!(lane_from_env(), Lane::Combined);
                    })
                    .await;
                assert_eq!(lane_from_env(), Lane::Resume);
            })
            .await;
        assert_eq!(lane_from_env(), original);
    }
}
