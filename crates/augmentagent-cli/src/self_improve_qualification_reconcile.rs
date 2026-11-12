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
    let Some(store) = WorkflowStore::open_read_only(&workflow::path())? else {
        return Ok(Default::default());
    };
    let prefix = format!("{}#", repository.to_ascii_lowercase());
    let mut retries = std::collections::BTreeSet::new();
    for id in store.due_qualification_recovery(Some(&repository), now)? {
        if store
            .snapshot(&id)?
            .is_some_and(|snapshot| snapshot.pr.is_none())
        {
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
    recovery::existing_checkout(root, &context.worktree).await?;
    anyhow::ensure!(
        git(&context.worktree, &["symbolic-ref", "--short", "HEAD"]).await? == snapshot.branch,
        "original recovery branch changed"
    );
    let binding = workflow::git_binding(
        &context.worktree,
        &context.contract,
        &context.environment,
        &snapshot.checkpoint.evidence,
    )
    .await?;
    anyhow::ensure!(
        binding == context.binding && binding == snapshot.checkpoint.binding,
        "original recovery inputs changed"
    );
    if let Some(pr) = &pr {
        if pr.head != binding.head {
            let store = WorkflowStore::open(&workflow::path())?;
            anyhow::ensure!(
                store
                    .retained_candidate_head(
                        repository,
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
    // A completed semantic Unknown is negative source data. Authenticate it
    // under the original workflow namespace before queuing normal repair.
    let source=match issue_number {
        Some(number)=>crate::autoship_contract::AcceptanceSource::Issue(number),
        None=>crate::autoship_contract::AcceptanceSource::PullRequest(snapshot.pr.context("semantic recovery source missing")?),
    };
    let control=acceptance::control_directories(repository,&snapshot.workflow)?.workflow_acceptance;
    let mut store=WorkflowStore::open(&workflow::path())?;
    let owner_revision=store.owner_control(&snapshot.workflow)?.map(|control|control.revision);
    if store.recover_semantic_qualification(&snapshot,owner_revision,
        &acceptance::semantic_reconsideration_policy(),chrono::Utc::now().timestamp_millis(),
        |custody,evidence|acceptance::inspect_retained_semantic(&context.worktree,&control,source,&snapshot.checkpoint.binding,custody,evidence))? {
        return Ok(());
    }
    drop(store);
    // Physical native custody can be checked without a native allocation,
    // execution claim, model call or any change to the attempt counters.
    if WorkflowStore::open(&workflow::path())?.recover_native_qualification(
        &snapshot.workflow,
        &binding,
        &context.worktree,
        acceptance::require_retained_native_custody,
        chrono::Utc::now().timestamp_millis(),
    )? {
        return Ok(());
    }
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
