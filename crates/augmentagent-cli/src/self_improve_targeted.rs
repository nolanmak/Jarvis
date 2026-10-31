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
    let repository = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        workflow::canonical_repository(repo),
    )
    .await
    .context("repository identity lookup timed out")??;
    anyhow::ensure!(
        repository.eq_ignore_ascii_case(&request.repository),
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
    authorized_repository(repo, request).await?;
    let actor = read(repo, &["api", "user", "--jq", ".login"]).await?;
    let actor = actor.trim();
    anyhow::ensure!(!actor.is_empty() && actor.len() <= 100 && actor.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'), "invalid authenticated GitHub actor");
    Ok(format!("github:{}", actor.to_ascii_lowercase()))
}

pub(crate) async fn validate_command(repo: &Path, request: &Request) -> Result<()> {
    resolve(repo, request).await.map(|_| ())
}

/// Shared read-only execution admission, also used before clearing owner pause.
/// Background dispatch still verifies deployed-service equality independently.
pub(crate) async fn command_readiness(repo:&Path,request:&Request)->Result<()> {
    execution_preflight(repo,Some(request.target),ExecutionMode::Local).await
}

pub(super) async fn execution_preflight(repo:&Path,target:Option<Target>,mode:ExecutionMode)->Result<()> {
    let readiness:Result<()>=async {
        tokio::task::spawn_blocking(augmentagent_channel_core::native_vm::readiness::runtime::preflight)
            .await.context("native runtime preflight task failed")??;
        if let Some(target)=target {
            let builder=workflow_builder(repo,target).await?;
            provider_readiness::inspect_builder(builder.as_ref()).preflight()?;
        } else {provider_readiness::inspect().preflight()?;}
        augmentagent_channel_core::run_resources::readiness().await.preflight()?;
        if mode==ExecutionMode::Background {crate::autoship_deployment::inspect(repo).await.preflight()?;}
        Ok(())
    }.await;
    readiness.context(CommandFailure {code:3,reason:"execution-readiness-blocked"})
}

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
    let mut selected = resolve(repo, &request).await.context(CommandFailure {code:2, reason:"invalid-target"})?;
    if let (Some(issue), Resolved::Pr(pr)) = (source_issue, &mut selected) {
        anyhow::ensure!(
            pr.source_issue.is_none_or(|n| n == issue),
            "PR source changed from submitted issue"
        );
        pr.source_issue = Some(issue);
    }
    if let Resolved::Issue(issue) = &selected {
        if has_open_agent_pr(repo, issue.number).await? {
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
        .await
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
        resume_draft_pr(repo, &builder, pr.pr, issue, &pr.branch, dry_run).await
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
elif a[:2]==['api','user']: print('owner')
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
