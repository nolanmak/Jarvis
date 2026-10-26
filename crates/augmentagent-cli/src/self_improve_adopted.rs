//! Execution for authorized PRs with no source issue. PR and issue namespaces
//! stay separate throughout acceptance, ownership and remote mutation receipts.
use super::*;
use crate::autoship_contract::AcceptanceSource;
use crate::autoship_workflow::{OperationKind, Stage, WorkflowStore};

async fn discover(repo: &Path, dry_run: bool) -> Result<Vec<adoption::AdoptedPr>> {
    let repository = workflow::canonical_repository(repo).await?;
    let (ok, raw, error) = run(&gh_bin(), &["pr", "list", "--state", "open", "--limit", "1000",
        "--search", "sort:created-asc", "--json",
        "number,isDraft,headRefName,headRefOid,baseRefName,isCrossRepository,author,title,body,closingIssuesReferences"], repo).await?;
    anyhow::ensure!(ok, "cannot discover adopted PRs: {error}");
    let mut allowed = trusted_authors(repo).await;
    if let Some(owner) = repository.split('/').next() {
        allowed.push(owner.into());
    }
    let candidates = adoption::authorized(&serde_json::from_str(&raw)?, &repository, &allowed);
    if !dry_run {
        adoption::persist(&workflow::path(), &candidates)?;
    }
    Ok(candidates)
}

fn approved(
    store: &WorkflowStore,
    candidate: &adoption::AdoptedPr,
) -> Result<Option<RecordedVerdict>> {
    let Some(id) = store.resolve_pr(&candidate.repository, candidate.pr)? else {
        return Ok(None);
    };
    let review = store
        .current_reviews(&id)?
        .into_iter()
        .filter(|r| {
            r.binding.head == candidate.head
                && r.approved
                && r.reviewer.starts_with("adopted-publication-")
        })
        .last();
    Ok(review
        .and_then(|r| serde_json::from_str::<RecordedVerdict>(&r.evidence).ok())
        .filter(|v| v.structured.is_some() && approval_of(Some(v)).reviewed))
}

pub(super) async fn next(repo: &Path, dry_run: bool) -> Result<Option<adoption::AdoptedPr>> {
    let candidates = discover(repo, dry_run).await?;
    let store = WorkflowStore::open(&workflow::path())?;
    let now = chrono::Utc::now().timestamp_millis();
    for candidate in candidates {
        if candidate.source_issue.is_some() || !candidate.ready_for_review() {
            continue;
        }
        let id = store
            .resolve_pr(&candidate.repository, candidate.pr)?
            .unwrap_or(candidate.workflow_id()?);
        if let Some(snapshot) = store.snapshot(&id)? {
            if snapshot.checkpoint.next.eligible_at_ms > now {
                continue;
            }
        }
        if let Some(verdict) = approved(&store, &candidate)? {
            let (ok, base, _) =
                run("git", &["ls-remote", "origin", "refs/heads/main"], repo).await?;
            if ok
                && verdict.structured.as_ref().is_some_and(|e| {
                    base.split_whitespace().next() == Some(e.binding.base.as_str())
                })
            {
                continue;
            }
        }
        return Ok(Some(candidate));
    }
    Ok(None)
}

pub(super) async fn resume(
    repo: &Path,
    reasoner: &Arc<FallbackReasoner>,
    pr: adoption::AdoptedPr,
    dry_run: bool,
) -> Result<RunReport> {
    if dry_run {
        return Ok(RunReport::held(format!(
            "PR #{}: would execute PR-scoped acceptance and review",
            pr.pr
        )));
    }
    anyhow::ensure!(
        pr.source_issue.is_none() && pr.ready_for_review(),
        "PR-scoped execution requires independent source identity"
    );
    adoption::preflight(repo, pr.pr, &pr.branch).await?;
    let lock_path = run_lock_path().with_file_name(format!("self-improve-pr-{}.lock", pr.pr));
    let Some(_lock) = RunLock::try_acquire(&lock_path)? else {
        return Ok(RunReport::held("adopted PR worktree busy".into()));
    };
    let worktree = workflow::prepare_resume_worktree(repo, pr.pr, &pr.branch).await?;
    let request = format!("Pull request #{}: {}\n{}", pr.pr, pr.title, pr.body);
    let environment = recovery::environment_identity(&worktree).await?;
    let Some(mut guard) = workflow::WorkflowGuard::acquire(workflow::Start {
        repo_root: repo,
        worktree: &worktree,
        repository: &pr.repository,
        source_issue: None,
        pr: Some(pr.pr),
        branch: &pr.branch,
        contract: &request,
        environment: &environment,
    })
    .await?
    else {
        return Ok(RunReport::held(
            "adopted PR workflow busy or deferred".into(),
        ));
    };
    guard.bind_reasoner(reasoner.as_ref())?;
    reasoner.track_review_history(repo, &pr.branch, true)?;
    let result = execute(
        repo,
        reasoner,
        &pr,
        &worktree,
        &request,
        &environment,
        &mut guard,
    )
    .await;
    match result {
        Ok(report) => {
            guard.finish()?;
            Ok(report)
        }
        Err(error) => {
            guard
                .transition(
                    Stage::Held,
                    "adopted_execution_retry",
                    "resume_preserved_pr",
                    chrono::Utc::now().timestamp_millis() + 300_000,
                )
                .await?;
            guard.finish()?;
            Err(error)
        }
    }
}

async fn execute(
    repo: &Path,
    reasoner: &FallbackReasoner,
    pr: &adoption::AdoptedPr,
    worktree: &Path,
    request: &str,
    environment: &str,
    guard: &mut workflow::WorkflowGuard,
) -> Result<RunReport> {
    let source = AcceptanceSource::PullRequest(pr.pr);
    let control = acceptance::control_directory(&pr.repository)?;
    let (ok, base, error) = run("git", &["merge-base", "origin/main", "HEAD"], worktree).await?;
    anyhow::ensure!(ok, "cannot resolve adopted acceptance baseline: {error}");
    let prepared = acceptance::prepare_acceptance_for(
        repo,
        worktree,
        &control,
        source,
        base.trim(),
        request,
        reasoner,
        &acceptance::worker_roots(worktree),
        &[],
    )
    .await?;
    guard.bind_acceptance(&prepared)?;
    // Preserve interrupted edits before integrating either upstream. The pinned
    // verifier and full gate judge every recovered or conflict-resolution edit.
    recovery::commit_candidate_named(
        worktree,
        &format!("autoship: preserve PR #{} revision", pr.pr),
    )
    .await?;
    for upstream in [format!("origin/{}", pr.branch), "origin/main".into()] {
        let (ok, _, error) = run("git", &["merge", "--no-edit", &upstream], worktree).await?;
        if !ok {
            let (_, diff, _) = run("git", &["diff"], worktree).await?;
            adoption::revise(reasoner,guard,&fix_opts(worktree.to_path_buf()),&format!("Resolve this merge while preserving the PR contract. Do not weaken any verifier.\n{request}\n{error}\n{diff}")).await?;
            recovery::commit_candidate_named(worktree, "autoship: reconcile PR upstream").await?;
        }
    }
    let criteria: Vec<String> = prepared
        .proposal
        .contract
        .criteria
        .iter()
        .map(|c| c.requirement.clone())
        .collect();
    let mut previous = None;
    for round in 0..=revise_rounds() {
        let receipt = acceptance::verify_with_repair_for(
            reasoner, source, &pr.title, worktree, &prepared, &control, guard,
        )
        .await?;
        verification_gate(worktree).await?;
        guard
            .transition(Stage::Review, "adopted_review", "collect_reviews", 0)
            .await?;
        let review = aligned_review::run_for_subject(
            reasoner,
            &aligned_review::ReviewSubject {
                title: &pr.title,
                body: &pr.body,
            },
            worktree.to_path_buf(),
            &criteria,
            aligned_review::ReviewContext {
                qa_ok: true,
                qa_notes: "Independent executable acceptance and full verification gate passed.",
                repair_round: round,
                previous: previous.as_ref(),
                environment_id: environment,
                adjudication_attempts_already_made: 0,
                workflow: Some(guard),
                acceptance_evidence: Some((&prepared, &receipt)),
            },
        )
        .await;
        if let Some(structured) = &review.structured {
            previous = Some(structured.clone());
        }
        if let Some(verdict) = review.recorded_verdict() {
            guard.record_review(
                &format!("adopted-publication-{}", uuid::Uuid::new_v4()),
                review.approved(),
                &serde_json::to_string(&verdict)?,
            )?;
        }
        if review.approved() {
            guard.push(&pr.branch).await?;
            acceptance::assert_receipt_current(worktree, &prepared, &receipt).await?;
            let (_, names, _) = run(
                "git",
                &["diff", "--name-only", "origin/main...HEAD"],
                worktree,
            )
            .await?;
            let allowed = may_automerge(&MergePolicy {
                automerge_enabled: automerge_enabled_value(
                    std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE")
                        .ok()
                        .as_deref(),
                ),
                complexity: complexity_from_pr_body(&pr.body),
                codex_approved: review.codex_approved(),
                reviews_approved: true,
                research_filed: is_research_filed(&pr.body),
                receipt_gated_file: touches_verify_gated_path(&names),
                lgtm_overrides_receipt: std::env::var("AUGMENTAGENT_AUTOPR_LGTM_OVERRIDES_RECEIPT")
                    .ok(),
                issue_author: pr.author.clone(),
                repo_owner: merge_policy_owner(repo).await,
                automerge_authors: std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE_AUTHORS").ok(),
            });
            if !allowed || checks_green(repo, pr.pr).await != Some(true) {
                guard
                    .transition(
                        Stage::Held,
                        "adopted_approved_pending_policy_or_ci",
                        "reconcile_approved_pr",
                        chrono::Utc::now().timestamp_millis() + 300_000,
                    )
                    .await?;
                return Ok(RunReport::built(format!(
                    "PR #{}: LGTM, waiting for merge policy/checks",
                    pr.pr
                )));
            }
            let binding = guard.binding().clone();
            guard.validate_binding(&binding).await?;
            let (ok, raw, _) = run(
                &gh_bin(),
                &[
                    "pr",
                    "view",
                    &pr.pr.to_string(),
                    "--json",
                    "headRefOid,isDraft",
                ],
                repo,
            )
            .await?;
            anyhow::ensure!(ok, "PR merge state unavailable");
            let row: serde_json::Value = serde_json::from_str(&raw)?;
            anyhow::ensure!(
                row["headRefOid"].as_str() == Some(binding.head.as_str()),
                "PR head changed after review"
            );
            if row["isDraft"].as_bool() == Some(true) {
                let intent = guard.intent(OperationKind::ReadyPr)?;
                guard.validate_binding(&binding).await?;
                let (ok, _, error) =
                    run(&gh_bin(), &["pr", "ready", &pr.pr.to_string()], repo).await?;
                anyhow::ensure!(ok, "cannot ready approved PR: {error}");
                guard.observe_remote(
                    &intent,
                    &format!("github:pr:{}:ready:{}", pr.pr, binding.head),
                    Some(pr.pr),
                )?;
            }
            guard.validate_binding(&binding).await?;
            let intent = guard.intent(OperationKind::MergePr)?;
            let (ok, _, error) = run(
                &gh_bin(),
                &[
                    "pr",
                    "merge",
                    &pr.pr.to_string(),
                    "--squash",
                    "--match-head-commit",
                    &binding.head,
                ],
                repo,
            )
            .await?;
            anyhow::ensure!(
                sweep_merged_exact(repo, pr.pr, &binding.head).await,
                "merge not observed (command={ok}): {error}"
            );
            guard.observe_remote(
                &intent,
                &format!("github:pr:{}:merged:{}", pr.pr, binding.head),
                Some(pr.pr),
            )?;
            guard
                .transition(Stage::Merged, "adopted_merge_observed", "complete", 0)
                .await?;
            return Ok(RunReport::built(format!(
                "PR #{}: verified, LGTM and merged",
                pr.pr
            )));
        }
        if !review.available || round == revise_rounds() {
            break;
        }
        adoption::revise(reasoner,guard,&fix_opts(worktree.to_path_buf()),&format!("Address verified review findings for this PR. Preserve pinned acceptance; do not weaken verifiers.\n{request}\n{}",review.notes)).await?;
    }
    guard.push(&pr.branch).await?;
    guard
        .transition(
            Stage::Review,
            "adopted_review_pending",
            "resume_preserved_pr",
            chrono::Utc::now().timestamp_millis() + 300_000,
        )
        .await?;
    Ok(RunReport::built(format!(
        "PR #{}: revisions preserved; review retry scheduled",
        pr.pr
    )))
}

/// Free reconciliation remains available while a different PR's builder runs.
pub(super) async fn sweep(repo: &Path, dry_run: bool) -> Result<usize> {
    let candidates = discover(repo, dry_run).await?;
    let store = WorkflowStore::open(&workflow::path())?;
    let mut merged = 0;
    for pr in candidates
        .into_iter()
        .filter(|p| p.source_issue.is_none() && p.ready_for_review())
    {
        let Some(verdict) = approved(&store, &pr)? else {
            continue;
        };
        let Some(evidence) = verdict.structured.as_ref() else {
            continue;
        };
        let (ok, base, _) = run("git", &["ls-remote", "origin", "refs/heads/main"], repo).await?;
        if !ok || base.split_whitespace().next() != Some(evidence.binding.base.as_str()) {
            continue;
        }
        let approval = approval_of(Some(&verdict));
        let (ok, raw, _) = run(
            &gh_bin(),
            &["pr", "view", &pr.pr.to_string(), "--json", "files,isDraft"],
            repo,
        )
        .await?;
        if !ok {
            continue;
        }
        let row: serde_json::Value = serde_json::from_str(&raw)?;
        let names = row["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["path"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if !may_automerge(&MergePolicy {
            automerge_enabled: automerge_enabled_value(
                std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE")
                    .ok()
                    .as_deref(),
            ),
            complexity: complexity_from_pr_body(&pr.body),
            codex_approved: approval.codex,
            reviews_approved: approval.reviewed,
            research_filed: is_research_filed(&pr.body),
            receipt_gated_file: touches_verify_gated_path(&names),
            lgtm_overrides_receipt: std::env::var("AUGMENTAGENT_AUTOPR_LGTM_OVERRIDES_RECEIPT")
                .ok(),
            issue_author: pr.author.clone(),
            repo_owner: merge_policy_owner(repo).await,
            automerge_authors: std::env::var("AUGMENTAGENT_AUTOPR_AUTOMERGE_AUTHORS").ok(),
        }) || checks_green(repo, pr.pr).await != Some(true)
            || dry_run
        {
            continue;
        }
        let Some(lease) = workflow::SweepGuard::acquire_pr(
            &pr.repository,
            pr.pr,
            &pr.branch,
            &pr.head,
            &serde_json::to_string(&verdict)?,
        )?
        else {
            continue;
        };
        lease.validate_remote(repo, pr.pr, &pr.head).await?;
        if ready_for_merge(repo, pr.pr, row["isDraft"].as_bool())
            .await
            .is_none()
        {
            continue;
        }
        lease.validate_remote(repo, pr.pr, &pr.head).await?;
        let intent = lease.merge_intent()?;
        let _ = run(
            &gh_bin(),
            &[
                "pr",
                "merge",
                &pr.pr.to_string(),
                "--squash",
                "--match-head-commit",
                &pr.head,
            ],
            repo,
        )
        .await?;
        if sweep_merged_exact(repo, pr.pr, &pr.head).await {
            lease.merged(&intent, pr.pr)?;
            merged += 1;
        }
    }
    Ok(merged)
}
