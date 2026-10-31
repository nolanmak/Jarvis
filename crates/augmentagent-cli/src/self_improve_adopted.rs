//! Execution for authorized PRs with no source issue. PR and issue namespaces
//! stay separate throughout acceptance, ownership and remote mutation receipts.
use super::*;
use crate::autoship_contract::AcceptanceSource;
use crate::autoship_workflow::{OperationKind, Stage, WorkflowStore};

async fn discover(repo: &Path, dry_run: bool) -> Result<Vec<adoption::AdoptedPr>> {
    let repository = workflow::canonical_repository(repo).await?;
    let raw = complete_repository_inventory(repo, "pr", None,
        "number,isDraft,headRefName,headRefOid,baseRefName,isCrossRepository,author,title,body,closingIssuesReferences")
        .await.context("source-less PR inventory must be complete before fair selection")?;
    let mut allowed = trusted_authors(repo).await;
    if let Some(owner) = repository.split('/').next() {
        allowed.push(owner.into());
    }
    let candidates = adoption::authorized(&serde_json::from_str(&raw)?, &repository, &allowed);
    if !dry_run {
        adoption::persist(&workflow::path(), &candidates)?;
    }
    for candidate in &candidates {
        if !candidate.ready_for_review() {
            if let Err(error) = reconcile_parent(repo, candidate, dry_run).await {
                warn!(
                    pr = candidate.pr,
                    "parent reconciliation deferred: {error:#}"
                );
            }
        }
    }
    Ok(candidates)
}

fn publication_review(
    store: &WorkflowStore,
    candidate: &adoption::AdoptedPr,
) -> Result<Option<crate::autoship_workflow::Review>> {
    let Some(id) = store.resolve_pr(&candidate.repository, candidate.pr)? else {
        return Ok(None);
    };
    let Some(snapshot) = store.snapshot(&id)? else {
        return Ok(None);
    };
    let review=store.current_review_history(&id)?.into_iter()
        .find(|r| {
            r.binding == snapshot.checkpoint.binding
                && r.binding.head == candidate.head
                && r.reviewer.starts_with("adopted-publication-")
        })
        .filter(|r| r.approved);
    Ok(review)
}

fn approved(store:&WorkflowStore,candidate:&adoption::AdoptedPr)->Result<Option<RecordedVerdict>> {
    Ok(publication_review(store,candidate)?
        .and_then(|r| serde_json::from_str::<RecordedVerdict>(&r.evidence).ok())
        .filter(|v| v.structured.is_some() && approval_of(Some(v)).reviewed))
}

fn workflow_available(snapshot: Option<&crate::autoship_workflow::Snapshot>, now: i64) -> bool {
    snapshot.is_none_or(|state| {
        state.lease_until_ms <= now && state.checkpoint.next.eligible_at_ms <= now
    })
}

/// Reserve a turn before preflight/checkout so a persistent setup failure also
/// rotates. The cursor is scheduling state, never a billed model attempt.
fn choose_round_robin(
    path: &Path,
    candidates: Vec<adoption::AdoptedPr>,
    dry_run: bool,
) -> Result<Option<adoption::AdoptedPr>> {
    use rusqlite::OptionalExtension;
    let Some(first) = candidates.first() else {
        return Ok(None);
    };
    let repository = first.repository.clone();
    anyhow::ensure!(
        candidates
            .iter()
            .all(|candidate| candidate.repository == repository),
        "adoption cursor cannot mix repositories"
    );
    if dry_run {
        let cursor = if WorkflowStore::open_read_only(path)?.is_none() { 0 } else {
        let conn = rusqlite::Connection::open_with_flags(path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        let cursor = conn
            .query_row(
                "SELECT last_pr FROM adopted_execution_cursor WHERE repository=?1",
                [&repository],
                |row| row.get::<_, u64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or(0);
        cursor
        };
        return Ok(candidates
            .into_iter()
            .min_by_key(|candidate| (candidate.pr <= cursor, candidate.pr)));
    }
    let _ = WorkflowStore::open(path)?;
    let mut conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS adopted_execution_cursor(repository TEXT PRIMARY KEY,last_pr INTEGER NOT NULL)")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let cursor = tx
        .query_row(
            "SELECT last_pr FROM adopted_execution_cursor WHERE repository=?1",
            [&repository],
            |row| row.get::<_, u64>(0),
        )
        .optional()?
        .unwrap_or(0);
    let chosen = candidates
        .into_iter()
        .min_by_key(|candidate| (candidate.pr <= cursor, candidate.pr))
        .expect("nonempty candidate pool");
    tx.execute("INSERT INTO adopted_execution_cursor(repository,last_pr) VALUES(?1,?2) ON CONFLICT(repository) DO UPDATE SET last_pr=excluded.last_pr",rusqlite::params![repository,chosen.pr])?;
    tx.commit()?;
    Ok(Some(chosen))
}

fn reserve_source_less(
    path: &Path,
    fair_path: &Path,
    eligible: Vec<adoption::AdoptedPr>,
    dry_run: bool,
) -> Result<Option<adoption::AdoptedPr>> {
    let selected = choose_round_robin(path, eligible, dry_run)?;
    if selected.is_some() && !dry_run {
        FairSched::update_persist(fair_path, |fair| fair.reserve_resume());
    }
    Ok(selected)
}

pub(super) async fn next(repo: &Path, dry_run: bool) -> Result<Option<adoption::AdoptedPr>> {
    let candidates = discover(repo, dry_run).await?;
    select_from_inventory(repo,candidates,dry_run,&workflow::path(),&fair_sched_path()).await
}

async fn select_from_inventory(repo:&Path,candidates:Vec<adoption::AdoptedPr>,dry_run:bool,
    state:&Path,fair_path:&Path)->Result<Option<adoption::AdoptedPr>> {
    let store = if dry_run {WorkflowStore::open_read_only(state)?} else {Some(WorkflowStore::open(state)?)};
    let now = chrono::Utc::now().timestamp_millis();
    let mut eligible = Vec::new();
    for candidate in candidates {
        if candidate.source_issue.is_some() || !candidate.ready_for_review() {
            continue;
        }
        let id = store.as_ref().map(|store|store.resolve_pr(&candidate.repository,candidate.pr)).transpose()?.flatten()
            .unwrap_or(candidate.workflow_id()?);
        let snapshot=store.as_ref().map(|store|store.snapshot(&id)).transpose()?.flatten();
        if !workflow_available(snapshot.as_ref(), now) {
            continue;
        }
        if acceptance::readiness::pr_blocked(repo,state,&candidate).await? {
            continue;
        }
        if let Some(reason) = policy_hold::adjudicated_dependency_at(repo, &candidate, dry_run,state).await {
            info!(
                pr = candidate.pr,
                reason, "adopted PR dependency remains in free reconciliation"
            );
            continue;
        }
        if let Some(verdict) = store.as_ref().map(|store|approved(store,&candidate)).transpose()?.flatten() {
            if let policy_hold::ApprovalDisposition::Free(reason) =
                policy_hold::reconciliation_disposition(repo, &candidate, &verdict).await
            {
                info!(
                    pr = candidate.pr,
                    reason, "approved adopted PR stays in free reconciliation"
                );
                continue;
            }
        }
        eligible.push(candidate);
    }
    reserve_source_less(state, fair_path, eligible, dry_run)
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
    let worktree = workflow::prepare_resume_worktree_at_head(repo, pr.pr, &pr.branch,Some(&pr.head)).await?;
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
    guard.bind_builder(reasoner.as_ref())?;
    reasoner.track_review_history(repo, &pr.branch, true)?;
    let prior_checkpoint = WorkflowStore::open(&workflow::path())?
        .snapshot(guard.id())?
        .map(|snapshot| snapshot.checkpoint);
    let capacity_accounting = guard.capacity_accounting(true)?;
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
    if let Some(report) = capacity_accounting.report()? { return Ok(report); }
    match result {
        Ok(report) => {
            guard.finish()?;
            Ok(report)
        }
        Err(error) => {
            let latest = WorkflowStore::open(&workflow::path())?
                .snapshot(guard.id())?
                .map(|snapshot| snapshot.checkpoint);
            let action_preserved = latest.as_ref().is_some_and(|checkpoint| {
                Some(checkpoint) != prior_checkpoint.as_ref()
                    && (checkpoint.next.action.starts_with("dependency_")
                        || checkpoint.next.action.starts_with("retry_")
                        || matches!(
                            checkpoint.next.action.as_str(),
                            "verify_new_revision"
                                | "rescope_acceptance"
                                | "repair_verifier_definition"
                                | "complete_review_or_adjudication"
                        ))
            });
            if !action_preserved {
                guard
                    .transition(
                        Stage::Held,
                        "adopted_execution_retry",
                        "resume_preserved_pr",
                        chrono::Utc::now().timestamp_millis() + 300_000,
                    )
                    .await?;
            }
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
    let grounding=match acceptance::readiness::assess_pr(worktree,pr,guard).await? {
        acceptance::readiness::Decision::Ready(evidence)=>evidence,
        acceptance::readiness::Decision::Dependency(reason)=>return Ok(RunReport::held(format!("PR #{}: grounded external decision; re-evaluate on PR/main change: {}",pr.pr,truncate(&reason,1200)))),
    };
    let grounded_request=format!("{request}\nIndependent readiness evidence (grounding, not approval):\n{grounding}");
    let control = acceptance::control_directories(&pr.repository, guard.id())?.workflow_acceptance;
    let (ok, base, error) = run("git", &["merge-base", "origin/main", "HEAD"], worktree).await?;
    anyhow::ensure!(ok, "cannot resolve adopted acceptance baseline: {error}");
    let mut prepared = acceptance::prepare_acceptance_for(
        repo,
        worktree,
        &control,
        source,
        base.trim(),
        &grounded_request,
        reasoner,
        &acceptance::worker_roots(worktree),
        &[],
        guard,
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
    let mut criteria: Vec<String> = prepared
        .proposal
        .contract
        .criteria
        .iter()
        .map(|c| c.requirement.clone())
        .collect();
    let mut previous = None;
    let mut round = 0;
    let mut native_sessions = None;
    loop {
        let receipt = acceptance::verify_with_repair_for(
            reasoner,
            source,
            &pr.title,
            worktree,
            &mut prepared,
            &control,
            guard,
        )
        .await?;
        criteria = prepared
            .proposal
            .contract
            .criteria
            .iter()
            .map(|criterion| criterion.requirement.clone())
            .collect();
        verification_gate(worktree, &guard.audit_logger()).await?;
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
            if acceptance::apply_adjudicated_amendment(
                reasoner,
                source,
                &pr.title,
                worktree,
                &mut prepared,
                &control,
                structured,
                guard,
            )
            .await?
            {
                previous = None;
                criteria = prepared
                    .proposal
                    .contract
                    .criteria
                    .iter()
                    .map(|criterion| criterion.requirement.clone())
                    .collect();
                continue;
            }
            previous = Some(structured.clone());
        }
        if let Some(verdict) = review.recorded_verdict() {
            guard.record_review(
                &format!("adopted-publication-{}", uuid::Uuid::new_v4()),
                review.approved(),
                &serde_json::to_string(&verdict)?,
            )?;
        }
        let (_, diff, _) = run(
            "git",
            &["diff", "--numstat", "origin/main...HEAD"],
            worktree,
        )
        .await?;
        let changed_lines = diff
            .lines()
            .flat_map(|line| line.split_whitespace().take(2))
            .filter_map(|n| n.parse::<usize>().ok())
            .sum();
        let evaluated = episode::evaluate(
            guard,
            &review,
            prepared.proposal.contract.criteria.len(),
            changed_lines,
        )
        .await?;
        if review.approved() {
            guard.push(&pr.branch).await?;
            acceptance::assert_receipt_current(worktree, &prepared, &receipt).await?;
            if guard.published_pr_ready(pr.pr,&review).await? {
                return Ok(RunReport::built(format!("PR #{}: validated revision published; PR milestone complete",pr.pr)));
            }
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
                        if allowed { Stage::AwaitChecks } else { Stage::Held },
                        if allowed { "adopted_ci_pending" } else { "adopted_merge_policy_blocked" },
                        if allowed { "reconcile_approved_pr" } else { "await_merge_authorization" },
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
                let (observed, raw, _) = run(
                    &gh_bin(),
                    &[
                        "pr",
                        "view",
                        &pr.pr.to_string(),
                        "--json",
                        "isDraft,headRefOid",
                    ],
                    repo,
                )
                .await?;
                let ready: serde_json::Value = serde_json::from_str(&raw)?;
                anyhow::ensure!(
                    observed
                        && ready["isDraft"].as_bool() == Some(false)
                        && ready["headRefOid"].as_str() == Some(binding.head.as_str()),
                    "ready transition requires exact-head remote observation"
                );
                guard.observe_remote(
                    &intent,
                    &format!("github:pr:{}:ready:{}", pr.pr, binding.head),
                    Some(pr.pr),
                )?;
            }
            guard.validate_binding(&binding).await?;
            let intent = guard.intent(OperationKind::MergePr)?;
            let mutation =
                publication::merge_exact(repo, &pr.repository, pr.pr, &binding.head, &intent, &guard.merge_lease()?).await;
            if let Ok(Some(receipt)) = &mutation {
                guard.record_merge_mutation(&intent, &serde_json::to_value(receipt)?)?;
            }
            let ok = mutation.is_ok();
            let error = mutation.err().map(|e| format!("{e:#}")).unwrap_or_default();
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
        let strategy = match &evaluated.decision {
            crate::autoship_episode::Decision::FreshSession { .. }
            | crate::autoship_episode::Decision::Rescope { .. } => {
                let session = episode::session_for(
                    reasoner, source, &pr.title, &pr.body, worktree, guard, &prepared, &evaluated,
                )
                .await?;
                let Some(session) = session else {
                    break;
                };
                round = 0;
                previous = None;
                if native_sessions.is_none() {
                    native_sessions = Some(
                        augmentagent_channel_core::native_session::FreshBuilderSessions::new(),
                    );
                }
                Some(session)
            }
            crate::autoship_episode::Decision::CompleteEvidence
            | crate::autoship_episode::Decision::RepairInfrastructure
            | crate::autoship_episode::Decision::Adjudicate => break,
            _ => None,
        };
        if !review.available || round >= revise_rounds() {
            break;
        }
        let episode_dispatch = strategy
            .as_ref()
            .map(|session| (session.epoch.clone(), session.action.clone()));
        let (opts,prompt) = match strategy {
            Some(session) => (session.opts,session.prompt),
            None => (fix_opts(worktree.to_path_buf()),format!("Address verified review findings for this PR. Preserve pinned acceptance; do not weaken verifiers.\n{request}\n{}",review.notes)),
        };
        let call = adoption::revise(reasoner, guard, &opts, &prompt);
        let result = episode::dispatch(guard, episode_dispatch, async {
            match &native_sessions {
                Some(sessions) => sessions.scope(call).await,
                None => call.await,
            }
        })
        .await;
        if let Some(sessions) = &native_sessions {
            guard.record_episode_sessions(sessions)?;
        }
        result?;
        round += 1;
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
    sweep_candidates(repo, dry_run, candidates).await
}

pub(super) async fn sweep_candidates(repo: &Path, dry_run: bool, candidates: Vec<adoption::AdoptedPr>) -> Result<usize> {
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
        let mutation =
            publication::merge_exact(repo, &pr.repository, pr.pr, &pr.head, &intent, &lease.merge_lease()?).await;
        if let Ok(Some(receipt)) = &mutation {
            lease.record_merge_mutation(&intent, &serde_json::to_value(receipt)?)?;
        }
        if let Err(error) = &mutation {
            warn!(pr = pr.pr, "adopted merge awaits reconciliation: {error:#}");
        }
        if sweep_merged_exact(repo, pr.pr, &pr.head).await {
            lease.merged(&intent, pr.pr)?;
            merged += 1;
        }
    }
    Ok(merged)
}

/// A merged parent no longer blocks the child. Retarget only after Git proves
/// that the parent's merge commit is reachable from main, under the child's
/// own workflow lease. Full child verification runs after rediscovery.
async fn reconcile_parent(repo: &Path, pr: &adoption::AdoptedPr, dry_run: bool) -> Result<bool> {
    if pr.ready_for_review() {
        return Ok(false);
    }
    let (ok, raw, _) = run(
        &gh_bin(),
        &[
            "pr",
            "list",
            "--state",
            "all",
            "--head",
            &pr.base,
            "--limit",
            "100",
            "--json",
            "number,state,isCrossRepository,mergeCommit",
        ],
        repo,
    )
    .await?;
    if !ok {
        return Ok(false);
    }
    let rows: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
    // Ambiguous parent history is not authority to silently change a target.
    let parents: Vec<_> = rows
        .iter()
        .filter(|p| p["isCrossRepository"].as_bool() == Some(false))
        .collect();
    if parents.len() != 1 || parents[0]["state"].as_str() != Some("MERGED") {
        return Ok(false);
    }
    let Some(merge) = parents[0]["mergeCommit"]["oid"].as_str() else {
        return Ok(false);
    };
    let (ok, _, _) = run("git", &["fetch", "origin", "main"], repo).await?;
    if !ok
        || !run(
            "git",
            &["merge-base", "--is-ancestor", merge, "origin/main"],
            repo,
        )
        .await?
        .0
        || dry_run
    {
        return Ok(false);
    }
    let lock_path = run_lock_path().with_file_name(format!("self-improve-pr-{}.lock", pr.pr));
    let Some(_lock) = RunLock::try_acquire(&lock_path)? else {
        return Ok(false);
    };
    let worktree = workflow::prepare_resume_worktree_at_head(repo, pr.pr, &pr.branch,Some(&pr.head)).await?;
    let contract = format!(
        "Retarget authorized child PR #{} after merged parent {}",
        pr.pr, pr.base
    );
    let Some(mut guard) = workflow::WorkflowGuard::acquire(workflow::Start {
        repo_root: repo,
        worktree: &worktree,
        repository: &pr.repository,
        source_issue: pr.source_issue,
        pr: Some(pr.pr),
        branch: &pr.branch,
        contract: &contract,
        environment: "parent-reconciliation-v1",
    })
    .await?
    else {
        return Ok(false);
    };
    let result: Result<()> = async {
        let (ok, raw, _) = run(
            &gh_bin(),
            &[
                "pr",
                "view",
                &pr.pr.to_string(),
                "--json",
                "headRefOid,baseRefName,isCrossRepository",
            ],
            repo,
        )
        .await?;
        let row: serde_json::Value = serde_json::from_str(&raw)?;
        anyhow::ensure!(
            ok && row["headRefOid"].as_str() == Some(pr.head.as_str())
                && row["baseRefName"].as_str() == Some(pr.base.as_str())
                && row["isCrossRepository"].as_bool() == Some(false),
            "child changed before retarget"
        );
        let intent = guard.intent(OperationKind::RetargetPr)?;
        guard.validate_before_side_effect()?;
        let (ok, _, error) = run(
            &gh_bin(),
            &["pr", "edit", &pr.pr.to_string(), "--base", "main"],
            repo,
        )
        .await?;
        anyhow::ensure!(ok, "child retarget failed: {error}");
        let (ok, raw, _) = run(
            &gh_bin(),
            &[
                "pr",
                "view",
                &pr.pr.to_string(),
                "--json",
                "baseRefName,headRefOid",
            ],
            repo,
        )
        .await?;
        let row: serde_json::Value = serde_json::from_str(&raw)?;
        anyhow::ensure!(
            ok && row["baseRefName"].as_str() == Some("main")
                && row["headRefOid"].as_str() == Some(pr.head.as_str()),
            "child retarget needs reconciliation"
        );
        guard.observe_remote(
            &intent,
            &format!("github:pr:{}:base:main:head:{}", pr.pr, pr.head),
            Some(pr.pr),
        )?;
        guard
            .transition(
                Stage::Reconcile,
                "parent_merged_child_retargeted",
                "verify_against_main",
                0,
            )
            .await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        guard
            .transition(
                Stage::Reconcile,
                "parent_retarget_retry",
                "reconcile_child_base",
                chrono::Utc::now().timestamp_millis() + 300_000,
            )
            .await?;
    }
    guard.finish()?;
    result.map(|_| true)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_less_ownership_and_lgtm_text_do_not_authorize_merge() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let pr = adoption::AdoptedPr {
            repository: "owner/repo".into(),
            pr: 42,
            source_issue: None,
            branch: "worktree/recovered".into(),
            head: "candidate".into(),
            base: "main".into(),
            author: "owner".into(),
            title: "Repair behavior".into(),
            body: "LGTM".into(),
            dependency: adoption::Dependency::Main,
        };
        let id = pr.workflow_id().unwrap();
        assert_ne!(
            id,
            crate::autoship_workflow::workflow_id("owner/repo", 42).unwrap(),
            "PR42 must never alias issue42"
        );
        assert!(approved(&store, &pr).unwrap().is_none());
        let binding = crate::autoship_workflow::Binding {
            head: pr.head.clone(),
            contract: "pr-contract".into(),
            evidence_hash: "evidence".into(),
        };
        store
            .register(&id, &pr.branch, binding.clone(), 10)
            .unwrap();
        let lease = store.claim(&id, "fixture", 11, 1000).unwrap().unwrap();
        store
            .attach_pr_in_repo(&lease, &pr.repository, pr.pr, 12)
            .unwrap();
        let legacy = RecordedVerdict {
            provider: "codex".into(),
            backend: "native".into(),
            model: "fixture".into(),
            diff_ok: true,
            system_ok: true,
            structured: None,
        };
        store
            .record_review(
                &lease,
                &crate::autoship_workflow::Review {
                    reviewer: "adopted-publication-fixture".into(),
                    binding,
                    approved: true,
                    evidence: serde_json::to_string(&legacy).unwrap(),
                },
                13,
            )
            .unwrap();
        assert!(
            approved(&store, &pr).unwrap().is_none(),
            "source-less merges require complete structured authority, not booleans or comments"
        );
    }
}

#[cfg(test)]
mod fairness_tests {
    use super::*;
    fn candidate(pr: u64) -> adoption::AdoptedPr {
        adoption::AdoptedPr {
            repository: "owner/repo".into(),
            pr,
            source_issue: None,
            branch: format!("worktree/pr-{pr}"),
            head: format!("head-{pr}"),
            base: "main".into(),
            author: "owner".into(),
            title: "Synthetic repair".into(),
            body: "Preserve existing behavior".into(),
            dependency: adoption::Dependency::Main,
        }
    }
    #[test]
    fn owner_input_invalidates_adopted_publication_on_unchanged_head() {
        use crate::autoship_workflow::{Binding,Review,discord_ingress::DiscordOrigin,submission::{Request,Target,Milestone}};
        let dir=tempfile::tempdir().unwrap();let mut store=WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();let candidate=candidate(42);
        let origin=DiscordOrigin {bot:11,owner:22,channel:33,message:44};
        let id=store.submit_discord_authorized(&origin,&Request {repository:candidate.repository.clone(),target:Target::Pr(42),provider:None,until:Milestone::Merged},&"a".repeat(64),1).unwrap().admission.workflow;
        let binding=Binding {head:candidate.head.clone(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,&candidate.branch,binding.clone(),2).unwrap();let lease=store.claim(&id,"worker",3,100).unwrap().unwrap();store.attach_pr_in_repo(&lease,&candidate.repository,42,4).unwrap();
        store.record_review(&lease,&Review {reviewer:"adopted-publication-old".into(),binding,approved:true,evidence:"old structured receipt".into()},5).unwrap();assert!(publication_review(&store,&candidate).unwrap().is_some());
        store.enqueue_owner_message(&DiscordOrigin {message:45,..origin},&id,"Change the behavior",None,6).unwrap();
        assert!(publication_review(&store,&candidate).unwrap().is_none(),"historical review survived owner input despite unchanged head");
    }
    #[test]
    fn recovered_audit_cannot_revive_adopted_publication_approval() {
        use crate::autoship_workflow::{Binding,Review};
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("workflow.db");
        let mut store=WorkflowStore::open(&path).unwrap();let candidate=candidate(42);
        let id=candidate.workflow_id().unwrap();
        let binding=Binding {head:candidate.head.clone(),contract:"contract".into(),evidence_hash:"evidence".into()};
        store.register(&id,&candidate.branch,binding.clone(),10).unwrap();
        let first=store.claim(&id,"first",11,100).unwrap().unwrap();
        store.attach_pr_in_repo(&first,&candidate.repository,candidate.pr,12).unwrap();
        let review=Review {reviewer:"adopted-publication-old".into(),binding:binding.clone(),approved:true,evidence:"old structured receipt".into()};
        store.record_review(&first,&review,13).unwrap();
        assert!(publication_review(&store,&candidate).unwrap().is_some());
        let next=store.claim(&id,"next",112,100).unwrap().unwrap();
        store.recover_audit_generation(&next,0,&serde_json::json!({"fixture":"verified recovery"}),113).unwrap();
        let mut checkpoint=store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.binding=binding;
        store.checkpoint(&next,&checkpoint,114).unwrap();
        drop(store);
        let store=WorkflowStore::open_read_only(&path).unwrap().unwrap();
        assert!(publication_review(&store,&candidate).unwrap().is_none());
        assert!(approved(&store,&candidate).unwrap().is_none(),"old receipt cannot keep candidate in free hold");
        assert!(store.events(&id).unwrap().iter().any(|(_,kind,_,_)|kind=="review_recorded"),"history must be retained");
    }

    #[tokio::test]
    async fn full_dry_selector_never_creates_state_or_changes_existing_cursor() {
        let dir=tempfile::tempdir().unwrap();
        let state=dir.path().join("workflows.db");let fair=dir.path().join("fair.json");
        let first=select_from_inventory(dir.path(),vec![candidate(20),candidate(10)],true,&state,&fair).await.unwrap().unwrap();
        assert_eq!(first.pr,10);
        assert!(!state.exists() && !fair.exists());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(),0);
        choose_round_robin(&state,vec![candidate(10),candidate(20)],false).unwrap();
        let before=std::fs::read(&state).unwrap();
        let metadata=std::fs::metadata(&state).unwrap();
        for _ in 0..2 {
            let chosen=select_from_inventory(dir.path(),vec![candidate(10),candidate(20)],true,&state,&fair).await.unwrap().unwrap();
            assert_eq!(chosen.pr,20);
        }
        assert_eq!(std::fs::read(&state).unwrap(),before);
        assert_eq!(std::fs::metadata(&state).unwrap().modified().unwrap(),metadata.modified().unwrap());
        assert!(!fair.exists());
    }

    #[test]
    fn permanently_failing_oldest_rotates_to_two_new_prs_across_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflows.db");
        let candidates = vec![candidate(10), candidate(20), candidate(30)];
        let mut seen = vec![];
        for tick in 0..4 {
            let now = 1_000 + tick * 3_600_000;
            // Every tick opens fresh DB handles, exactly as daemon restart does.
            let mut store = WorkflowStore::open(&path).unwrap();
            let pool = candidates
                .iter()
                .filter(|candidate| {
                    workflow_available(
                        store
                            .snapshot(&candidate.workflow_id().unwrap())
                            .unwrap()
                            .as_ref(),
                        now,
                    )
                })
                .cloned()
                .collect();
            let chosen = choose_round_robin(&path, pool, false).unwrap().unwrap();
            seen.push(chosen.pr);
            let id = chosen.workflow_id().unwrap();
            store
                .register(
                    &id,
                    &chosen.branch,
                    crate::autoship_workflow::Binding {
                        head: chosen.head.clone(),
                        contract: "contract".into(),
                        evidence_hash: "evidence".into(),
                    },
                    now,
                )
                .unwrap();
            let lease = store
                .claim(&id, "failing-fixture", now, 100)
                .unwrap()
                .unwrap();
            let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
            checkpoint.stage = Stage::Held;
            checkpoint.next.action = "resume_preserved_pr".into();
            checkpoint.next.reason = "permanent synthetic failure".into();
            checkpoint.next.eligible_at_ms = now + 300_000;
            store.checkpoint(&lease, &checkpoint, now).unwrap();
            store.release(&lease, now).unwrap();
            assert_eq!(
                store.snapshot(&id).unwrap().unwrap().model_calls,
                0,
                "cursor reservation never bills a model call"
            );
        }
        assert_eq!(
            seen,
            vec![10, 20, 30, 10],
            "oldest failure must not monopolize subsequent ticks"
        );
    }
    #[test]
    fn source_less_failed_reservations_yield_reserved_build_slot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let fair = dir.path().join("fair.json");
        let mut turns = vec![];
        for _ in 0..4 {
            if prefer_build_now(Lane::Combined, true, &FairSched::load(&fair), 2) {
                turns.push("fresh-issue".to_owned());
                FairSched::update_persist(&fair, |state| state.record_billed(false));
            } else {
                let pr = reserve_source_less(
                    &path,
                    &fair,
                    vec![candidate(10), candidate(20), candidate(30)],
                    false,
                )
                .unwrap()
                .unwrap();
                turns.push(format!("pr-{}", pr.pr));
                // Model a failed preflight: no successful/billed RunReport.
            }
        }
        assert_eq!(turns, vec!["pr-10", "pr-20", "fresh-issue", "pr-30"]);
        let before = std::fs::read(&fair).unwrap();
        assert!(reserve_source_less(&path, &fair, vec![], false)
            .unwrap()
            .is_none());
        reserve_source_less(&path, &fair, vec![candidate(40)], true).unwrap();
        assert_eq!(
            std::fs::read(&fair).unwrap(),
            before,
            "held or dry-run heads cannot consume scheduling debt"
        );
    }
    #[test]
    fn held_and_leased_heads_are_not_admitted_or_charged_by_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflows.db");
        let blocked = candidate(10);
        let mut store = WorkflowStore::open(&path).unwrap();
        let id = blocked.workflow_id().unwrap();
        store
            .register(
                &id,
                &blocked.branch,
                crate::autoship_workflow::Binding {
                    head: blocked.head.clone(),
                    contract: "contract".into(),
                    evidence_hash: "evidence".into(),
                },
                1,
            )
            .unwrap();
        let lease = store.claim(&id, "active-worker", 2, 1000).unwrap().unwrap();
        assert!(!workflow_available(
            store.snapshot(&id).unwrap().as_ref(),
            3
        ));
        let mut checkpoint = store.snapshot(&id).unwrap().unwrap().checkpoint;
        checkpoint.next.eligible_at_ms = 5000;
        store.checkpoint(&lease, &checkpoint, 4).unwrap();
        store.release(&lease, 5).unwrap();
        assert!(!workflow_available(
            store.snapshot(&id).unwrap().as_ref(),
            6
        ));
        let pool = vec![blocked, candidate(20), candidate(30)]
            .into_iter()
            .filter(|candidate| {
                workflow_available(
                    store
                        .snapshot(&candidate.workflow_id().unwrap())
                        .unwrap()
                        .as_ref(),
                    6,
                )
            })
            .collect();
        assert_eq!(
            choose_round_robin(&path, pool, false).unwrap().unwrap().pr,
            20
        );
        assert_eq!(store.snapshot(&id).unwrap().unwrap().model_calls, 0);
        assert_eq!(
            choose_round_robin(&path, vec![candidate(20), candidate(30)], true)
                .unwrap()
                .unwrap()
                .pr,
            30
        );
        assert_eq!(
            choose_round_robin(&path, vec![candidate(20), candidate(30)], true)
                .unwrap()
                .unwrap()
                .pr,
            30,
            "dry runs must not consume the rotation"
        );
    }
}
