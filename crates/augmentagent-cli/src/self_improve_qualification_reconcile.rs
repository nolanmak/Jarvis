//! Bounded restoration of original evidence, independent of model/merge lanes.
use super::*;
use crate::autoship_workflow::submission::{Milestone, Request, Target};
use crate::autoship_workflow::{RecoveryCandidate, WorkflowStore};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

/// A completed free inspection rejoins ordinary issue selection. The legacy
/// per-day refusal ledger must not suppress its newly available evidence.
pub(super) async fn issue_retries(root: &Path) -> Result<std::collections::BTreeSet<u64>> {
    let now = chrono::Utc::now().timestamp_millis();
    let pending = {
        let Some(store) = WorkflowStore::open_read_only(&workflow::path())? else {
            return Ok(Default::default());
        };
        store.has_due_qualification_recovery(None, now)?
    };
    if !pending {
        return Ok(Default::default());
    }
    let repository = workflow::canonical_repository(root).await?;
    let store = WorkflowStore::open_read_only(&workflow::path())?
        .context("observed qualification recovery history disappeared during repository inspection")?;
    let prefix = format!("{}#", repository.to_ascii_lowercase());
    let mut retries = std::collections::BTreeSet::new();
    for id in store.due_qualification_recovery(Some(&repository), now)? {
        let snapshot=match store.snapshot(&id) {
            Ok(snapshot)=>snapshot,
            // The complete due-view query succeeded. A bad individual row
            // supplies no hint; the normal per-issue reader preserves its hold.
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some()
                || error.downcast_ref::<rusqlite::Error>().is_some_and(WorkflowStore::is_row_data_error)=>continue,
            Err(error)=>return Err(error),
        };
        if snapshot.is_some_and(|snapshot| snapshot.pr.is_none()) {
            if let Some(number) = id.strip_prefix(&prefix).and_then(|s| s.parse().ok()) {
                retries.insert(number);
            }
        }
    }
    Ok(retries)
}

fn source_request(
    issue: Option<&Issue>,
    pr: Option<&adoption::AdoptedPr>,
    source_pr: Option<u64>,
) -> Result<String> {
    match (issue, source_pr) {
        (Some(issue), None) => Ok(serde_json::to_string(&(
            issue.number,
            &issue.title,
            &issue.body,
        ))?),
        (Some(issue), Some(number)) => {
            let pr = pr
                .filter(|pr| pr.pr == number)
                .context("original source PR differs")?;
            Ok(format!("{}\n{}\n{}", issue.title, issue.body, pr.body))
        }
        (None, Some(number)) => {
            let pr = pr
                .filter(|pr| pr.pr == number)
                .context("original source PR unavailable")?;
            Ok(format!(
                "Pull request #{}: {}\n{}",
                pr.pr, pr.title, pr.body
            ))
        }
        _ => bail!("original request unavailable"),
    }
}

async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let (ok, out, _) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        super::run("git", args, repo),
    )
    .await
    .context("qualification recovery Git lookup timed out")??;
    anyhow::ensure!(ok, "qualification recovery Git evidence unavailable");
    Ok(out.trim().into())
}

/// The caller retains refreshed source authorization and the lane lock. Read
/// the original checkout and ownership proof without rewriting candidate state.
async fn validate_recovery_checkout(
    state:&Path,root:&Path,snapshot:&crate::autoship_workflow::Snapshot,
    context:&crate::autoship_workflow::RecoveryContext,pr:Option<&adoption::AdoptedPr>,
)->Result<crate::autoship_workflow::Binding> {
    recovery::existing_checkout(root, &context.worktree).await?;
    let reference=git(&context.worktree,&["rev-parse","--verify","--symbolic-full-name","HEAD"]).await?;
    if reference=="HEAD" {
        let pr=pr.filter(|pr|snapshot.pr==Some(pr.pr) && snapshot.branch==pr.branch)
            .context("detached recovery requires its original PR identity")?;
        let expected=workflow::resume_worktree_path(root,pr.pr).await?;
        anyhow::ensure!(context.worktree.canonicalize()?==expected.canonicalize()?,
            "detached recovery is not the original resume checkout");
    }else {
        anyhow::ensure!(reference==format!("refs/heads/{}",snapshot.branch),
            "original recovery branch changed");
    }
    let binding = workflow::git_binding(
        &context.worktree,
        &context.contract,
        &context.environment,
        &snapshot.checkpoint.evidence,
    )
    .await?;
    anyhow::ensure!(context.binding==snapshot.checkpoint.binding,"original recovery context changed");
    if binding!=snapshot.checkpoint.binding {
        let store=WorkflowStore::open_read_only(state)?.context("recovery workflow store unavailable")?;
        anyhow::ensure!(store.retained_setup_candidate(snapshot)?.as_ref()==Some(&binding),"original recovery inputs changed without an owned failure observation");
    }
    if let Some(pr) = pr {
        if pr.head != binding.head {
            let store = WorkflowStore::open_read_only(state)?.context("recovery workflow store unavailable")?;
            anyhow::ensure!(
                store
                    .retained_candidate_head(
                        &context.repository,
                        pr.pr,
                        &pr.branch,
                        &pr.head,
                        &binding.head,
                        chrono::Utc::now().timestamp_millis()
                    )?
                    .as_deref()
                    == Some(binding.head.as_str()),
                "retained recovery head is not owned"
            );
            git(
                &context.worktree,
                &["merge-base", "--is-ancestor", &pr.head, &binding.head],
            )
            .await?;
        }
    }
    Ok(binding)
}

async fn recover(root: &Path, candidate: RecoveryCandidate) -> Result<()> {
    let RecoveryCandidate { snapshot, context } = candidate;
    let repository = &context.repository;
    let issue_number = snapshot
        .workflow
        .strip_prefix(&format!("{}#", repository.to_ascii_lowercase()))
        .map(str::parse::<u64>)
        .transpose()?;
    let request = |target| Request {
        repository: repository.clone(),
        target,
        provider: None,
        until: Milestone::Merged,
    };
    // Refresh remote identity, author, write permission, lifecycle and source.
    // No old context or inspection reservation substitutes for authorization.
    let issue = if let Some(number) = issue_number {
        match targeted::resolve_for_recovery(root, &request(Target::Issue(number))).await? {
            targeted::Resolved::Issue(issue) => Some(issue),
            _ => bail!("source issue identity differs"),
        }
    } else {
        None
    };
    let pr = if let Some(number) = snapshot.pr {
        match targeted::resolve_for_recovery(root, &request(Target::Pr(number))).await? {
            targeted::Resolved::Pr(pr) => {
                anyhow::ensure!(
                    pr.branch == snapshot.branch
                        && pr.ready_for_review()
                        && pr.source_issue == issue_number,
                    "recovery PR source or dependency changed"
                );
                Some(pr)
            }
            _ => bail!("source PR identity differs"),
        }
    } else {
        None
    };
    anyhow::ensure!(
        source_request(issue.as_ref(), pr.as_ref(), context.source_pr)? == context.source_request,
        "owner request changed; original recovery remains held"
    );
    let lock_name = snapshot
        .pr
        .map(|pr| format!("self-improve-pr-{pr}.lock"))
        .unwrap_or_else(|| format!("self-improve-issue-{}.lock", issue_number.unwrap_or(0)));
    let Some(_lock) = RunLock::try_acquire(&run_lock_path().with_file_name(lock_name))? else {
        return Ok(());
    };
    let binding=validate_recovery_checkout(&workflow::path(),root,&snapshot,&context,pr.as_ref()).await?;
    // A completed semantic Unknown is negative source data. Authenticate it
    // under the original workflow namespace before queuing normal repair.
    let source=match issue_number {
        Some(number)=>crate::autoship_contract::AcceptanceSource::Issue(number),
        None=>crate::autoship_contract::AcceptanceSource::PullRequest(snapshot.pr.context("semantic recovery source missing")?),
    };
    let control=acceptance::control_directories(repository,&snapshot.workflow)?.workflow_acceptance;
    let mut store=WorkflowStore::open(&workflow::path())?;
    let owner_revision=store.owner_control(&snapshot.workflow)?.map(|control|control.revision);
    let exact_inputs=binding==snapshot.checkpoint.binding;
    if exact_inputs && store.recover_semantic_qualification(&snapshot,owner_revision,
        &acceptance::semantic_policy_for_hold(&snapshot.checkpoint),chrono::Utc::now().timestamp_millis(),
        |custody,evidence|acceptance::inspect_semantic_hold(&context.worktree,&control,source,&snapshot.checkpoint,custody,evidence))? {
        return Ok(());
    }
    // Preserve the existing free physical-custody route before an optional
    // retained-measurement inspection can discover incomplete original data.
    if exact_inputs && store.recover_native_qualification(&snapshot.workflow,&binding,&context.worktree,
        acceptance::require_retained_native_custody,chrono::Utc::now().timestamp_millis())? {
        return Ok(());
    }
    let retained=store.retained_qualification_history(&snapshot.workflow,&snapshot.checkpoint.binding)?;
    match store.recover_retained_qualification_with_setup(&snapshot,owner_revision,chrono::Utc::now().timestamp_millis(),|evidence,failed|
        acceptance::inspect_retained_recovery_ready_with_setup(&context.worktree,&control,source,&snapshot,&context,evidence,retained.as_ref(),failed)) {
        Ok(true)=>return Ok(()),
        Ok(false)=>{},
        Err(error) if retained.is_some()=>return Err(error),
        Err(_)=>warn!("retained source unavailable; existing original-verifier recovery authority remains required"),
    }
    if !exact_inputs {return Ok(());}
    drop(store);
    // No retained cue authorizes re-execution. The existing original-plan
    // descriptor, precise hold, owner and lease checks still decide fallback.
    let Some(mut guard) = workflow::WorkflowGuard::acquire_verifier_recovery(workflow::Start {
        repo_root: root,
        worktree: &context.worktree,
        repository,
        source_issue: issue_number,
        pr: snapshot.pr,
        branch: &snapshot.branch,
        contract: &context.source_request,
        environment: &context.environment,
    })
    .await?
    else {
        return Ok(());
    };
    let Some((plan, _)) = guard.original_verifier_recovery_plan()? else {
        return guard.finish();
    };
    let control = acceptance::control_directories(repository, guard.id())?.workflow_acceptance;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(45 * 60),
        acceptance::recover_original_verifier(&context.worktree, &control, &guard),
    )
    .await
    .context("original verifier recovery deadline elapsed")
    .and_then(|result| result);
    if let Err(error) = &result {
        acceptance::hold_recovery_error(&mut guard, error, &plan).await?;
    }
    guard.finish()?;
    result.map(|_| ())
}

/// The window is concurrent so one blocked build lease cannot starve physical
/// custody checks. Routine upgrades stop selection and drain existing work;
/// operator shutdown drops guarded work with its durable capture intact.
async fn window<C, F>(
    shutdown: CancellationToken,
    stop_new: CancellationToken,
    mut select: C,
    execute: F,
) -> Result<()>
where
    C: FnMut(usize, &std::collections::BTreeSet<String>) -> Result<Vec<RecoveryCandidate>>,
    F: Fn(RecoveryCandidate) -> BoxFuture<'static, Result<()>>,
{
    let mut timer = tokio::time::interval(std::time::Duration::from_secs(300));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut running: FuturesUnordered<BoxFuture<'static, (String, Result<()>)>> =
        FuturesUnordered::new();
    let mut active = std::collections::BTreeSet::new();
    loop {
        if stop_new.is_cancelled() && running.is_empty() {
            return Ok(());
        }
        tokio::select! {
            biased;
            _=shutdown.cancelled()=>return Ok(()),
            _=stop_new.cancelled(),if !stop_new.is_cancelled()=>{},
            result=running.next(),if !running.is_empty()=>{
                if let Some((id,result))=result {
                    active.remove(&id);
                    if result.is_err() {warn!("original qualification recovery deferred; held evidence preserved");}
                }
            },
            _=timer.tick(),if !stop_new.is_cancelled()=>{
                if running.len()<4 {
                    match select(4-running.len(),&active) {
                        Ok(candidates)=>for candidate in candidates {
                            let id=candidate.snapshot.workflow.clone();
                            if !active.insert(id.clone()) {continue;}
                            let execution=execute(candidate);
                            running.push(Box::pin(async move {(id,execution.await)}));
                        },
                        Err(_)=>warn!("qualification recovery selection unavailable"),
                    }
                }
            }
        }
    }
}

pub(super) async fn run(
    root: PathBuf,
    dry_run: bool,
    shutdown: CancellationToken,
    stop_new: CancellationToken,
) -> Result<()> {
    if dry_run {
        stop_new.cancelled().await;
        return Ok(());
    }
    let mut retry = tokio::time::interval(std::time::Duration::from_secs(300));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let controller = loop {
        tokio::select! {
            biased;
            _=shutdown.cancelled()=>return Ok(()),
            _=stop_new.cancelled()=>return Ok(()),
            _=retry.tick()=>{},
        }
        let prepared = tokio::select! {
            _=shutdown.cancelled()=>return Ok(()),
            result=workspace::prepare(&root,"recovery")=>result,
        };
        match prepared {
            Ok(controller) => break controller,
            Err(_) => warn!("qualification recovery controller unavailable"),
        }
    };
    let repository = workflow::canonical_repository(&controller.root).await?;
    let execute_root = controller.root.clone();
    window(
        shutdown,
        stop_new,
        move |limit, active| {
            WorkflowStore::open(&workflow::path())?.reserve_qualification_recovery(
                &repository,
                chrono::Utc::now().timestamp_millis(),
                limit,
                active,
            )
        },
        move |candidate| {
            let root = execute_root.clone();
            Box::pin(async move {
                tokio::time::timeout(
                    std::time::Duration::from_secs(45 * 60),
                    recover(&root, candidate),
                )
                .await
                .context("qualification recovery deadline elapsed")?
            })
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_git(root:&Path,args:&[&str])->String {
        let out=std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
        assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().into()
    }

    #[tokio::test]
    async fn normal_detached_resume_checkout_reaches_qualification_recovery() {
        let root=tempfile::tempdir().unwrap();let data=tempfile::tempdir().unwrap();
        fixture_git(root.path(),&["init","-q","-b","main"]);
        fixture_git(root.path(),&["config","user.name","Fixture"]);
        fixture_git(root.path(),&["config","user.email","fixture@example.invalid"]);
        std::fs::write(root.path().join("code"),"original production\n").unwrap();
        fixture_git(root.path(),&["add","code"]);fixture_git(root.path(),&["commit","-qm","original"]);
        fixture_git(root.path(),&["remote","add","origin",root.path().to_str().unwrap()]);
        fixture_git(root.path(),&["fetch","-q","origin","main"]);
        let branch="agent-fix/issue-7";fixture_git(root.path(),&["branch",branch]);
        let published=fixture_git(root.path(),&["rev-parse","HEAD"]);
        let candidate=workflow::prepare_resume_worktree_at_head(root.path(),42,branch,Some(&published)).await.unwrap();
        assert_eq!(fixture_git(&candidate,&["rev-parse","--verify","--symbolic-full-name","HEAD"]),"HEAD","real resume producer no longer creates detached checkout");
        let binding=workflow::git_binding(&candidate,"owner request","fixture environment",&[]).await.unwrap();
        let state=data.path().join("workflow.db");let id="owner/repo#7";
        let mut store=WorkflowStore::open(&state).unwrap();let now=chrono::Utc::now().timestamp_millis();
        store.register(id,branch,binding.clone(),now).unwrap();
        let lease=store.claim(id,"fixture",now,60_000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease,"owner/repo",42,now).unwrap();
        let context=crate::autoship_workflow::RecoveryContext {version:1,workflow:id.into(),repository:"owner/repo".into(),worktree:candidate.clone(),source_request:"owner request".into(),source_pr:Some(42),contract:"owner request".into(),environment:"fixture environment".into(),binding:binding.clone()};
        store.record_recovery_context(&lease,&binding,&context,now).unwrap();
        let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage=crate::autoship_workflow::Stage::Scope;
        cp.next=crate::autoship_workflow::NextAction {reason:"acceptance_verifier_evidence_unknown".into(),action:"await_original_evidence_recovery".into(),eligible_at_ms:0};
        store.checkpoint(&lease,&cp,now).unwrap();store.release(&lease,now).unwrap();
        let snapshot=store.snapshot(id).unwrap().unwrap();let events=store.events(id).unwrap();
        let pr=adoption::AdoptedPr {repository:"owner/repo".into(),pr:42,source_issue:Some(7),branch:branch.into(),head:published.clone(),base:"main".into(),author:"owner".into(),title:"Fixture".into(),body:"Original request".into(),dependency:adoption::Dependency::Main};
        let result=validate_recovery_checkout(&state,root.path(),&snapshot,&context,Some(&pr)).await;
        assert!(result.is_ok(),"normal detached resume cannot reach free recovery: {result:?}");
        assert_eq!(store.snapshot(id).unwrap().unwrap(),snapshot);
        assert_eq!(store.events(id).unwrap(),events);
        assert_eq!(fixture_git(&candidate,&["rev-parse","--verify","--symbolic-full-name","HEAD"]),"HEAD","inspection reattached candidate");
        assert_eq!(fixture_git(&candidate,&["rev-parse","HEAD"]),published);
        assert!(fixture_git(&candidate,&["status","--porcelain=v1","--untracked-files=all"]).is_empty());
    }

    async fn retained_checkout_fixture(mode:&str)->(tempfile::TempDir,tempfile::TempDir,WorkflowStore,crate::autoship_workflow::Snapshot,crate::autoship_workflow::RecoveryContext,adoption::AdoptedPr) {
        let root=tempfile::tempdir().unwrap();let data=tempfile::tempdir().unwrap();
        fixture_git(root.path(),&["init","-q","-b","main"]);
        fixture_git(root.path(),&["config","user.name","Fixture"]);fixture_git(root.path(),&["config","user.email","fixture@example.invalid"]);
        std::fs::write(root.path().join("code"),"original production\n").unwrap();fixture_git(root.path(),&["add","code"]);fixture_git(root.path(),&["commit","-qm","original"]);
        fixture_git(root.path(),&["remote","add","origin",root.path().to_str().unwrap()]);fixture_git(root.path(),&["fetch","-q","origin","main"]);
        let branch="agent-fix/issue-7";let original=root.path().join("workers/original-issue");
        recovery::prepare(root.path(),&original,branch).await.unwrap();
        std::fs::write(original.join("code"),"published candidate\n").unwrap();fixture_git(&original,&["add","code"]);fixture_git(&original,&["commit","-qm","published candidate"]);
        let published=fixture_git(&original,&["rev-parse","HEAD"]);
        let candidate=workflow::prepare_resume_worktree_at_head(root.path(),42,branch,Some(&published)).await.unwrap();
        let original_binding=workflow::git_binding(&candidate,"owner request","fixture environment",&[]).await.unwrap();
        if matches!(mode,"owned"|"unowned"|"sibling") {
            if mode=="sibling" {
                let sibling=fixture_git(&candidate,&["commit-tree","HEAD^{tree}","-p","origin/main","-m","not a published descendant"]);
                fixture_git(&candidate,&["checkout","-q","--detach",&sibling]);
            }else {
                std::fs::write(candidate.join("code"),"retained unpublished candidate\n").unwrap();fixture_git(&candidate,&["add","code"]);fixture_git(&candidate,&["commit","-qm","retained candidate"]);
            }
        }
        let binding=workflow::git_binding(&candidate,"owner request","fixture environment",&[]).await.unwrap();
        let mut store=WorkflowStore::open(&data.path().join("workflow.db")).unwrap();let now=chrono::Utc::now().timestamp_millis();let id="owner/repo#7";
        store.register(id,branch,original_binding.clone(),now).unwrap();let lease=store.claim(id,"fixture",now,60_000).unwrap().unwrap();store.attach_pr_in_repo(&lease,"owner/repo",42,now).unwrap();
        if matches!(mode,"owned"|"sibling") {
            let operation=store.begin_operation(&lease,crate::autoship_workflow::OperationKind::Push,&original_binding,now).unwrap();store.complete_operation(&lease,&operation.id,&format!("git-head:{published}"),now).unwrap();
        }
        let context=crate::autoship_workflow::RecoveryContext {version:1,workflow:id.into(),repository:"owner/repo".into(),worktree:candidate,source_request:"owner request".into(),source_pr:Some(42),contract:"owner request".into(),environment:"fixture environment".into(),binding:binding.clone()};
        let mut cp=store.snapshot(id).unwrap().unwrap().checkpoint;cp.binding=binding.clone();cp.stage=crate::autoship_workflow::Stage::Scope;cp.next=crate::autoship_workflow::NextAction {reason:"acceptance_verifier_evidence_unknown".into(),action:"await_original_evidence_recovery".into(),eligible_at_ms:0};
        store.checkpoint(&lease,&cp,now).unwrap();store.record_recovery_context(&lease,&binding,&context,now).unwrap();store.release(&lease,now).unwrap();
        let snapshot=store.snapshot(id).unwrap().unwrap();
        let pr=adoption::AdoptedPr {repository:"owner/repo".into(),pr:42,source_issue:Some(7),branch:branch.into(),head:published,base:"main".into(),author:"owner".into(),title:"Fixture".into(),body:"Original request".into(),dependency:adoption::Dependency::Main};
        (root,data,store,snapshot,context,pr)
    }

    #[tokio::test]
    async fn recovery_checkout_preserves_path_binding_and_original_publication_fences() {
        let mut failures=vec![];
        for mode in ["published","owned","attached","wrong-attached","arbitrary-detached","canonical-alias","parent-alias","dangling-alias","foreign-repository","changed-input","changed-head","missing-pr","wrong-pr","wrong-branch","issue-only","unowned","sibling"] {
            let (root,data,store,mut snapshot,mut context,mut pr)=retained_checkout_fixture(mode).await;
            match mode {
                "attached"=>context.worktree=root.path().join("workers/original-issue"),
                "wrong-attached"=>{fixture_git(&context.worktree,&["checkout","-q","-b","wrong-owner-branch"]);},
                "arbitrary-detached"=>{
                    let other=root.path().join("workers/arbitrary-detached");fixture_git(root.path(),&["worktree","add","--detach",other.to_str().unwrap(),&pr.head]);context.worktree=other;
                },
                "canonical-alias"|"parent-alias"|"dangling-alias"=>{
                    let canonical=context.worktree.clone();let moved=root.path().join(if mode=="parent-alias" {"workers/resume-pr-42"}else{"workers/moved-resume"});
                    fixture_git(root.path(),&["worktree","move",canonical.to_str().unwrap(),moved.to_str().unwrap()]);context.worktree=moved.clone();
                    if mode=="parent-alias" {
                        std::fs::remove_dir(canonical.parent().unwrap()).unwrap();std::os::unix::fs::symlink(moved.parent().unwrap(),canonical.parent().unwrap()).unwrap();
                    }else {
                        std::os::unix::fs::symlink(if mode=="dangling-alias" {root.path().join("missing")}else{moved},canonical).unwrap();
                    }
                },
                "foreign-repository"=>{
                    let foreign=root.path().join("workers/foreign");fixture_git(root.path(),&["clone","-q","--no-hardlinks",root.path().to_str().unwrap(),foreign.to_str().unwrap()]);context.worktree=foreign;
                },
                "changed-input"=>std::fs::write(context.worktree.join("code"),"owner's uncommitted changes\n").unwrap(),
                "changed-head"=>{fixture_git(&context.worktree,&["commit","--allow-empty","-qm","unrecorded candidate"]);},
                "wrong-pr"=>pr.pr=43,
                "wrong-branch"=>pr.branch="other-published-branch".into(),
                "issue-only"=>{snapshot.pr=None;context.source_pr=None;},
                _=>{},
            }
            let identity=|path:&Path|(
                fixture_git(path,&["rev-parse","HEAD"]),fixture_git(path,&["rev-parse","--verify","--symbolic-full-name","HEAD"]),
                fixture_git(path,&["diff","--binary","HEAD","--"]),fixture_git(path,&["status","--porcelain=v1","--untracked-files=all"]));
            let before_checkout=identity(&context.worktree);let before_snapshot=store.snapshot(&snapshot.workflow).unwrap();let before_events=store.events(&snapshot.workflow).unwrap();
            let metadata=if matches!(mode,"missing-pr"|"issue-only") {None}else{Some(&pr)};
            let checked=validate_recovery_checkout(&data.path().join("workflow.db"),root.path(),&snapshot,&context,metadata).await;
            let expected=matches!(mode,"published"|"owned"|"attached");
            if checked.is_ok()!=expected {failures.push(format!("{mode}: expected allowed={expected}, got {checked:?}"));}
            if mode=="foreign-repository" {assert!(format!("{:?}",checked.unwrap_err()).contains("another repository"));}
            assert_eq!(identity(&context.worktree),before_checkout,"inspection changed {mode} checkout");
            assert_eq!(fixture_git(root.path(),&["rev-parse","refs/heads/agent-fix/issue-7"]),pr.head,"inspection rewrote {mode} published branch");
            assert_eq!(store.snapshot(&snapshot.workflow).unwrap(),before_snapshot,"inspection changed {mode} state");assert_eq!(store.events(&snapshot.workflow).unwrap(),before_events,"inspection changed {mode} events");
        }
        assert!(failures.is_empty(),"checkout boundary mismatches: {failures:#?}");
    }

    #[tokio::test]
    async fn recorded_setup_recovery_preserves_the_exact_failed_candidate_without_accepting_later_edits() {
        for mode in ["captured","missing","later-edit","later-head"] {
            let (root,data,mut store,before,context,pr)=retained_checkout_fixture("published").await;
            let now=chrono::Utc::now().timestamp_millis();let reference=crate::autoship_workflow::RetainedWitnessReference {
                plan:"a".repeat(64),result:"b".repeat(64),data:"c".repeat(64),prepared:"d".repeat(64),contract:"e".repeat(64)};
            assert!(store.recover_retained_qualification(&before,None,now,|_|Ok(Some(reference))).unwrap());
            let cue=store.semantic_reconsideration(&before.workflow,&before.checkpoint.binding).unwrap().unwrap();
            let lease=store.claim_productive_for_binding(&before.workflow,"fixture-setup",now,60_000,&before.checkpoint.binding).unwrap().unwrap();
            std::fs::create_dir_all(context.worktree.join("autoship_verifiers")).unwrap();
            std::fs::write(context.worktree.join("autoship_verifiers/case.py"),b"print('original measurement')\n").unwrap();
            let captured=workflow::git_binding(&context.worktree,&context.contract,&context.environment,&before.checkpoint.evidence).await.unwrap();
            assert_ne!(captured,before.checkpoint.binding);
            let setup=crate::autoship_workflow::RetainedSetupDependency {version:1,operation:"registration".into(),condition:"permission".into(),fingerprint:Some("a".repeat(64)),capacity:None,quotas:None,completed:false};
            store.hold_retained_setup_failure(&lease,&before.checkpoint.binding,&cue.key,&setup,(mode!="missing").then_some(&captured),now).unwrap();
            store.release(&lease,now).unwrap();let snapshot=store.snapshot(&before.workflow).unwrap().unwrap();let events=store.events(&before.workflow).unwrap();
            if mode=="later-edit" {std::fs::write(context.worktree.join("code"),b"unrecorded later edits\n").unwrap();}
            if mode=="later-head" {fixture_git(&context.worktree,&["commit","--allow-empty","-qm","unrecorded later commit"]);}
            let checked=validate_recovery_checkout(&data.path().join("workflow.db"),root.path(),&snapshot,&context,Some(&pr)).await;
            assert_eq!(checked.is_ok(),mode=="captured","recorded setup recovery boundary: {mode}: {checked:?}");
            assert_eq!(store.snapshot(&before.workflow).unwrap().unwrap(),snapshot);assert_eq!(store.events(&before.workflow).unwrap(),events);
        }
    }

    #[tokio::test]
    #[ignore = "explicit read-only retained recovery checkout compatibility; no model calls"]
    async fn production_retained_checkout_recovery_compatibility() {
        let state=PathBuf::from(std::env::var_os("AUTOSHIP_RECOVERY_STATE").expect("explicit read-only store"));
        let root=PathBuf::from(std::env::var_os("AUTOSHIP_RECOVERY_ROOT").expect("explicit controller repository"));
        let id=std::env::var("AUTOSHIP_RECOVERY_WORKFLOW").expect("explicit workflow identity");
        let store=WorkflowStore::open_read_only(&state).unwrap().unwrap();
        let before=store.snapshot(&id).unwrap().unwrap();let events=store.events(&id).unwrap();
        let context=store.recovery_context(&id,&before.checkpoint.binding).unwrap().expect("original protected context");
        let request=|target|Request {repository:context.repository.clone(),target,provider:None,until:Milestone::Merged};
        let issue_number=id.strip_prefix(&format!("{}#",context.repository)).map(str::parse::<u64>).transpose().unwrap();
        let issue=if let Some(number)=issue_number {
            let targeted::Resolved::Issue(issue)=targeted::resolve_for_recovery(&root,&request(Target::Issue(number))).await.unwrap() else {panic!("original issue did not resolve")};
            Some(issue)
        }else {None};
        let targeted::Resolved::Pr(pr)=targeted::resolve_for_recovery(&root,&request(Target::Pr(before.pr.unwrap()))).await.unwrap() else {panic!("original PR did not resolve")};
        assert!(pr.branch==before.branch && pr.ready_for_review() && pr.source_issue==issue_number,"fresh PR source or dependency changed");
        assert_eq!(source_request(issue.as_ref(),Some(&pr),context.source_pr).unwrap(),context.source_request,"original owner request changed");
        validate_recovery_checkout(&state,&root,&before,&context,Some(&pr)).await.expect("retained ordinary checkout cannot enter recovery");
        if crate::autoship_workflow::legacy_amendment_input(&before.checkpoint).is_some() {
            let source=issue_number.map(crate::autoship_contract::AcceptanceSource::Issue)
                .unwrap_or(crate::autoship_contract::AcceptanceSource::PullRequest(pr.pr));
            let control=acceptance::control_directories(&context.repository,&id).unwrap().workflow_acceptance;
            let custody=store.inspect_baseline_native_custody(&id).unwrap();
            assert!(acceptance::inspect_semantic_hold(&context.worktree,&control,source,&before.checkpoint,&custody,&store.inspect_verifier_evidence(&id)).unwrap().is_some(),"legacy ordinary decline is not eligible for corrected-evidence migration");
        }
        assert_eq!(store.snapshot(&id).unwrap().unwrap(),before);assert_eq!(store.events(&id).unwrap(),events);
        println!("Fresh remote authorization, original request, retained checkout and binding verified read-only; no branch rewrite, workflow mutation or model calls");
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_recovery_window_keeps_peers_moving_and_drains_on_upgrade() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("state.db")).unwrap();
        let mut candidates = std::collections::VecDeque::new();
        for issue in 1..=5 {
            let id = format!("owner/repo#{issue}");
            let binding = crate::autoship_workflow::Binding {
                head: issue.to_string(),
                contract: "contract".into(),
                evidence_hash: "evidence".into(),
            };
            store
                .register(&id, "candidate", binding.clone(), 1)
                .unwrap();
            candidates.push_back(RecoveryCandidate {
                snapshot: store.snapshot(&id).unwrap().unwrap(),
                context: crate::autoship_workflow::RecoveryContext {
                    version: 1,
                    workflow: id,
                    repository: "owner/repo".into(),
                    worktree: dir.path().into(),
                    source_request: "request".into(),
                    source_pr: None,
                    contract: "contract".into(),
                    environment: "environment".into(),
                    binding,
                },
            });
        }
        let shutdown = CancellationToken::new();
        let stop = CancellationToken::new();
        let starts = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let selections = Arc::new(AtomicUsize::new(0));
        let selected = selections.clone();
        let executing = starts.clone();
        let done = completed.clone();
        let runner = window(
            shutdown.clone(),
            stop.clone(),
            move |limit, active| {
                if selected.load(Ordering::SeqCst) > 0 {
                    assert!(active.contains("owner/repo#1"));
                }
                selected.fetch_add(1, Ordering::SeqCst);
                Ok((0..limit).filter_map(|_| candidates.pop_front()).collect())
            },
            move |candidate| {
                let starts = executing.clone();
                let completed = done.clone();
                Box::pin(async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    if candidate.snapshot.workflow.ends_with("#1") {
                        tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                    }
                    completed.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
            },
        );
        let observer = async {
            tokio::time::sleep(std::time::Duration::from_secs(301)).await;
            assert_eq!(
                starts.load(Ordering::SeqCst),
                5,
                "one slow verifier starved ready peers"
            );
            assert_eq!(completed.load(Ordering::SeqCst), 4);
            stop.cancel();
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            assert_eq!(
                completed.load(Ordering::SeqCst),
                5,
                "upgrade canceled admitted verification"
            );
            assert_eq!(
                selections.load(Ordering::SeqCst),
                2,
                "upgrade admitted new work while draining"
            );
            shutdown.cancel();
        };
        let (result, ()) = tokio::join!(runner, observer);
        result.unwrap();
    }
    #[test]
    fn recovery_request_reconstruction_preserves_the_original_frontend_shape() {
        let issue = Issue {
            number: 14,
            title: "Title".into(),
            body: "Body".into(),
            author: "owner".into(),
            author_trusted: true,
            research_filed: false,
        };
        let mut pr = adoption::AdoptedPr {
            repository: "owner/repo".into(),
            pr: 27,
            source_issue: Some(14),
            branch: "branch".into(),
            head: "a".repeat(40),
            base: "main".into(),
            author: "owner".into(),
            title: "PR title".into(),
            body: "PR body".into(),
            dependency: adoption::Dependency::Main,
        };
        assert_eq!(
            source_request(Some(&issue), Some(&pr), None).unwrap(),
            "[14,\"Title\",\"Body\"]"
        );
        assert_eq!(
            source_request(Some(&issue), Some(&pr), Some(27)).unwrap(),
            "Title\nBody\nPR body"
        );
        pr.source_issue = None;
        assert_eq!(
            source_request(None, Some(&pr), Some(27)).unwrap(),
            "Pull request #27: PR title\nPR body"
        );
        assert!(source_request(Some(&issue), Some(&pr), Some(28)).is_err());
        assert!(source_request(None, None, None).is_err());
    }
}
