//! Failed verifier correction escalates to independently observable children.
//! Parent requirements and original receipts are retained; no passing evidence,
//! reduced contract, owner grant or builder permission is manufactured here.
use super::*;
use crate::autoship_workflow::acceptance_escalation::{Material, ACTION};
use crate::autoship_workflow::retry_policy::{self, Strategy};
use augmentagent_channel_core::ProviderKind;

#[async_trait::async_trait]
trait Backend: Sync {
    fn ready(&self, provider: ProviderKind) -> bool;
    async fn children(
        &self,
        repo: &Path,
        root: &Path,
        issue: &Issue,
        criteria: &[String],
        detail: &str,
        guard: &mut workflow::WorkflowGuard,
        provider: ProviderKind,
        strategy: Option<(u32, Strategy)>,
    ) -> Result<Vec<u64>>;
    /// `Some(at)` while this issue's rolling daily model budget is spent.
    fn budget_reopens_at(&self, guard: &workflow::WorkflowGuard) -> Result<Option<i64>>;
    async fn summary(
        &self,
        repo: &Path,
        issue: &Issue,
        guard: &workflow::WorkflowGuard,
        key: &str,
        body: &str,
    ) -> Result<()>;
}
struct Production;
#[async_trait::async_trait]
impl Backend for Production {
    fn ready(&self, p: ProviderKind) -> bool {
        matches!(reviewer_status(p), ReviewerStatus::Ready)
    }
    async fn children(
        &self,
        repo: &Path,
        root: &Path,
        issue: &Issue,
        criteria: &[String],
        detail: &str,
        guard: &mut workflow::WorkflowGuard,
        p: ProviderKind,
        strategy: Option<(u32, Strategy)>,
    ) -> Result<Vec<u64>> {
        acceptance::readiness::decompose_unverifiable(
            repo, root, issue, criteria, detail, guard, p, strategy,
        )
        .await
    }
    fn budget_reopens_at(&self, guard: &workflow::WorkflowGuard) -> Result<Option<i64>> {
        guard.daily_budget_reopens_at()
    }
    async fn summary(
        &self,
        repo: &Path,
        issue: &Issue,
        guard: &workflow::WorkflowGuard,
        key: &str,
        body: &str,
    ) -> Result<()> {
        publish_summary(repo, issue, guard, key, body).await
    }
}
fn strategy_key(material: &Material, provider: ProviderKind, cycle: u32) -> Result<String> {
    // Cycle 0 keeps #2281's keys so its recorded outcomes still replay.
    if cycle == 0 {
        return Ok(amendment::digest(&serde_json::to_vec(&(
            "acceptance-strategy-failed-v1",
            &material.original.binding,
            provider.name(),
        ))?));
    }
    Ok(amendment::digest(&serde_json::to_vec(&(
        "acceptance-strategy-failed-v2",
        &material.original.binding,
        provider.name(),
        cycle,
        retry_policy::strategy(cycle).name(),
    ))?))
}
fn cycle_key(id: &str, material: &Material, cycle: u32) -> Result<String> {
    Ok(amendment::digest(&serde_json::to_vec(&(
        "acceptance-strategy-cycle-exhausted-v1",
        id,
        &material.original.binding,
        cycle,
    ))?))
}
fn failure_category(material: &Material) -> &'static str {
    if material
        .diagnostics
        .iter()
        .filter_map(|d| d["preview"].as_str())
        .any(|d| d.contains("error[E") || d.contains("E0308"))
    {
        "generated-verifier-compile-failure"
    } else {
        "incomplete-or-invalid-independent-acceptance"
    }
}
fn requirements(issue: &Issue) -> Result<Vec<String>> {
    // Require lossless coverage of ALL original request lines, not only the
    // scoper's possibly incomplete acceptance checklist.
    Ok(issue
        .body
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect())
}
pub(in crate::self_improve) async fn execute(
    repo: &Path,
    root: &Path,
    issue: &Issue,
    guard: &mut workflow::WorkflowGuard,
) -> Result<Option<RunReport>> {
    execute_with(repo, root, issue, guard, &reviewer_pool(), &Production).await
}
pub(in crate::self_improve) async fn hold_issue_error(
    repo: &Path,
    root: &Path,
    issue: &Issue,
    guard: &mut workflow::WorkflowGuard,
    error: &anyhow::Error,
) -> Result<Option<RunReport>> {
    let report = acceptance::hold_initial_error(guard, error).await?;
    if report.is_some() {
        if let Some(next) = execute(repo, root, issue, guard).await? {
            return Ok(Some(next));
        }
    }
    Ok(report)
}
async fn execute_with(
    repo: &Path,
    root: &Path,
    issue: &Issue,
    guard: &mut workflow::WorkflowGuard,
    providers: &[ProviderKind],
    backend: &impl Backend,
) -> Result<Option<RunReport>> {
    let Some(material) = guard.begin_acceptance_escalation().await? else {
        return Ok(None);
    };
    let request = serde_json::to_string(&(issue.number, &issue.title, &issue.body))?;
    anyhow::ensure!(
        guard.original_owner_request()? == request,
        "escalation cannot replace the original owner request"
    );
    let mut criteria = requirements(issue)?;
    criteria.extend(material.criteria.clone());
    let prepared = guard.retained_preparation()?;
    let context = if let Some(prepared) = &prepared {
        criteria.extend(
            prepared
                .proposal
                .contract
                .criteria
                .iter()
                .map(|c| c.requirement.clone()),
        );
        retained_initial_scope_context(guard, prepared)?
    } else {
        let (ok, base, _) = run("git", &["merge-base", "origin/main", "HEAD"], root).await?;
        anyhow::ensure!(
            ok && !base.trim().is_empty(),
            "escalation original base unavailable"
        );
        initial_scope_context(root, guard, base.trim())?
    };
    if let Some(saved) =
        read_initial_scope(&context, |key| guard.recorded_scope_stage_response(key))?
    {
        if let Some(scope) = saved.scope {
            criteria.extend(scope.criteria);
        }
    }
    let detail = serde_json::to_string(&material)?;
    // #2289: strategies never run out. Each exhausted cycle advances one rung
    // of the ladder and schedules the next after a capped backoff.
    let mut cycle = 0u32;
    while cycle < 100_000 && guard.recorded_scope_stage_response(&cycle_key(guard.id(), &material, cycle)?)?.is_some() {
        cycle += 1;
    }
    let rung = retry_policy::strategy(cycle);
    let mut order = providers.to_vec();
    if rung == Strategy::SwitchProvider {
        order.reverse();
    }
    let strategy = (cycle > 0).then_some((cycle, rung));
    let mut pending = false;
    for provider in order.iter().copied() {
        pending |= guard
            .recorded_scope_stage_response(&strategy_key(&material, provider, cycle)?)?
            .is_none();
    }
    if pending {
        if let Some(at) = backend.budget_reopens_at(guard)? {
            guard
                .transition(
                    crate::autoship_workflow::Stage::Scope,
                    "acceptance_strategy_budget_wait",
                    ACTION,
                    at,
                )
                .await?;
            return Ok(Some(RunReport::held(format!(
                "Acceptance strategy {} waits for this issue's daily model budget; automatic retry queued",
                rung.name()
            ))));
        }
    }
    // Only cached, bounded completed-response failures consume a strategy.
    // Capacity, unknown native outcomes and remote publication failure retry
    // the same durable strategy without resetting its protected model budget.
    let mut waiting = false;
    for provider in order.iter().copied() {
        let key = strategy_key(&material, provider, cycle)?;
        if guard.recorded_scope_stage_response(&key)?.is_some() {
            continue;
        }
        if !backend.ready(provider) {
            waiting = true;
            continue;
        }
        match backend
            .children(repo, root, issue, &criteria, &detail, guard, provider, strategy)
            .await
        {
            Ok(children) if !children.is_empty() => {
                guard
                    .transition(
                        crate::autoship_workflow::Stage::Scope,
                        "acceptance_decomposed_into_observable_children",
                        "await_child_completion",
                        0,
                    )
                    .await?;
                return Ok(Some(RunReport::triage(format!("issue #{}: acceptance recovery published {} observable children; parent evidence preserved",issue.number,children.len()))));
            }
            Ok(_) => {
                guard.record_scope_stage_response(&key,&serde_json::json!({"version":1,"provider":provider.name(),"reason":"already_a_bounded_child"}).to_string())?;
            }
            Err(error) if scope_authority::completed_exhaustion(&error) => {
                guard.record_scope_stage_response(&key,&serde_json::json!({"version":1,"provider":provider.name(),"reason":"completed_response_exhaustion","diagnostic":format!("{error:#}")}).to_string())?;
            }
            Err(error) => {
                // Missing/uncertain original native evidence remains under its
                // existing recovery fence; it is never a fresh strategy slot.
                if error.downcast_ref::<initial_repair::Hold>().is_some() {
                    let report = if let Some(prepared) = guard.retained_preparation()? {
                        let accounting = guard.capacity_accounting(false)?;
                        hold_qualification_error(guard, &prepared, &error, &accounting).await?
                    } else {
                        hold_initial_error(guard, &error).await?
                    };
                    if let Some(report) = report {
                        return Ok(Some(report));
                    }
                }
                // Keep the full diagnostic private; public status is typed.
                let key = amendment::digest(&serde_json::to_vec(&(
                    "acceptance-strategy-retry-v1",
                    guard.binding(),
                    provider.name(),
                    format!("{error:#}"),
                ))?);
                guard.record_scope_stage_response(
                    &key,
                    &serde_json::json!({"version":1,"diagnostic":format!("{error:#}")}).to_string(),
                )?;
                guard
                    .transition(
                        crate::autoship_workflow::Stage::Scope,
                        "acceptance_strategy_retry",
                        ACTION,
                        chrono::Utc::now().timestamp_millis() + 300_000,
                    )
                    .await?;
                return Ok(Some(RunReport::held("Acceptance strategy retry queued; original evidence and native charges preserved".into())));
            }
        }
    }
    if waiting || providers.is_empty() {
        guard
            .transition(
                crate::autoship_workflow::Stage::Scope,
                "acceptance_strategy_reviewer_capacity",
                ACTION,
                chrono::Utc::now().timestamp_millis() + 300_000,
            )
            .await?;
        return Ok(Some(RunReport::held(
            "Acceptance strategy waiting for independent provider capacity; automatic retry queued"
                .into(),
        )));
    }
    if cycle == 0 {
        let key = amendment::digest(&serde_json::to_vec(&(
            "acceptance-escalation-summary-v1",
            guard.id(),
            &material.original.binding,
        ))?);
        let category = failure_category(&material);
        let body=format!("Autoship acceptance strategy summary\n\nBounded verifier correction and observable-child decomposition did not yet succeed with the configured independent providers. Original requirements, candidate, failure evidence and charges are preserved. No acceptance pass or approval was inferred, and no criterion was removed. Autoship keeps retrying automatically with escalating strategies (re-scope with the failure evidence, switch provider, shrink to the smallest observable behavior, fresh attempt from current main) and capped backoff; no owner action is needed.\n\nReason: {}. Failure category: {category}. Evidence reference: {}.\n\n<!-- jarvis-acceptance-escalation:{key} -->",material.original.next.reason,key);
        // Intent before RPC; exact-body reconciliation makes a lost reply safe.
        // An intent recorded before #2289 keeps its exact original body.
        let body = match guard.scope_stage_response(&key)? {
            Some(recorded) => recorded,
            None => {
                guard.record_scope_stage_response(&key, &body)?;
                body
            }
        };
        if backend
            .summary(repo, issue, guard, &key, &body)
            .await
            .is_err()
        {
            guard
                .transition(
                    crate::autoship_workflow::Stage::Scope,
                    "acceptance_summary_retry",
                    ACTION,
                    chrono::Utc::now().timestamp_millis() + 300_000,
                )
                .await?;
            return Ok(Some(RunReport::held(
                "Acceptance strategy summary outcome unconfirmed; automatic reconciliation queued"
                    .into(),
            )));
        }
    }
    let delay = retry_policy::backoff_ms(cycle);
    let next = retry_policy::strategy(cycle.saturating_add(1));
    guard.record_scope_stage_response(
        &cycle_key(guard.id(), &material, cycle)?,
        &serde_json::json!({"version":1,"kind":"acceptance_strategy_cycle_exhausted","cycle":cycle,"strategy":rung.name(),"next_strategy":next.name(),"backoff_ms":delay}).to_string(),
    )?;
    guard
        .transition(
            crate::autoship_workflow::Stage::Scope,
            "acceptance_strategy_backoff",
            ACTION,
            chrono::Utc::now().timestamp_millis() + delay,
        )
        .await?;
    Ok(Some(RunReport::held(format!(
        "Acceptance strategy {} exhausted (cycle {cycle}); {} retries automatically in {} minutes",
        rung.name(),
        next.name(),
        delay / 60_000
    ))))
}
async fn publish_summary(
    repo: &Path,
    issue: &Issue,
    guard: &workflow::WorkflowGuard,
    key: &str,
    body: &str,
) -> Result<()> {
    let receipt = amendment::digest(format!("acceptance-summary-sent:{key}").as_bytes());
    if guard.recorded_scope_stage_response(&receipt)?.is_some() {
        return Ok(());
    }
    augmentagent_channel_core::public_report::validate("", body)?;
    guard.require_fresh_issue(repo).await?;
    let mut found = false;
    for page in 1..=10 {
        guard.validate_note_dispatch()?;
        let (ok, raw, _) = run(
            &gh_bin(),
            &[
                "api",
                &format!(
                    "repos/{}/issues/{}/comments?per_page=100&page={page}",
                    guard.repository(),
                    issue.number
                ),
            ],
            repo,
        )
        .await?;
        anyhow::ensure!(ok, "acceptance summary reconciliation unavailable");
        let rows: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
        if rows.iter().any(|r| r["body"].as_str() == Some(body)) {
            found = true;
            break;
        }
        if rows.len() < 100 {
            break;
        }
        anyhow::ensure!(page < 10, "acceptance summary reconciliation exceeds bound");
    }
    if !found {
        use std::io::Write;
        let mut file = tempfile::NamedTempFile::new()?;
        file.write_all(body.as_bytes())?;
        guard.validate_note_dispatch()?;
        let (ok, _, _) = run(
            &gh_bin(),
            &[
                "issue",
                "comment",
                &issue.number.to_string(),
                "--repo",
                guard.repository(),
                "--body-file",
                file.path().to_str().context("summary path encoding")?,
            ],
            repo,
        )
        .await?;
        anyhow::ensure!(
            ok,
            "acceptance summary publication outcome unknown; reconcile before retry"
        );
    }
    guard.record_scope_stage_response(&receipt, "observed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    };
    #[derive(Clone, Copy)]
    enum ResultKind {
        Invalid,
        Transient,
        Children,
    }
    struct Fake {
        outcomes: Mutex<Vec<ResultKind>>,
        calls: Mutex<Vec<ProviderKind>>,
        ready: AtomicBool,
        summary_fail: AtomicBool,
        posts: Mutex<Vec<String>>,
        strategies: Mutex<Vec<(u32, &'static str, ProviderKind)>>,
        budget: Mutex<Option<i64>>,
    }
    impl Fake {
        fn new(outcomes: Vec<ResultKind>) -> Self {
            Self {
                outcomes: Mutex::new(outcomes),
                calls: Mutex::new(vec![]),
                ready: AtomicBool::new(true),
                summary_fail: AtomicBool::new(false),
                posts: Mutex::new(vec![]),
                strategies: Mutex::new(vec![]),
                budget: Mutex::new(None),
            }
        }
    }
    #[async_trait::async_trait]
    impl Backend for Fake {
        fn ready(&self, _: ProviderKind) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
        async fn children(
            &self,
            _: &Path,
            _: &Path,
            issue: &Issue,
            criteria: &[String],
            detail: &str,
            _: &mut workflow::WorkflowGuard,
            p: ProviderKind,
            strategy: Option<(u32, Strategy)>,
        ) -> Result<Vec<u64>> {
            let (cycle, rung) = strategy.unwrap_or((0, Strategy::SplitObservableChildren));
            self.strategies.lock().unwrap().push((cycle, rung.name(), p));
            assert!(issue
                .body
                .lines()
                .filter(|l| !l.is_empty())
                .all(|l| criteria.contains(&l.to_owned())));
            assert!(
                detail.contains("PendingActionRow"),
                "failure evidence was not supplied to new strategy"
            );
            self.calls.lock().unwrap().push(p);
            match self.outcomes.lock().unwrap().remove(0) {
                ResultKind::Invalid => {
                    Err(scope_authority::CompletedInvalid("scripted invalid plan".into()).into())
                }
                ResultKind::Transient => Err(anyhow::anyhow!("scripted provider outage")),
                ResultKind::Children => Ok(vec![11, 12]),
            }
        }
        fn budget_reopens_at(&self, _: &workflow::WorkflowGuard) -> Result<Option<i64>> {
            Ok(*self.budget.lock().unwrap())
        }
        async fn summary(
            &self,
            _: &Path,
            _: &Issue,
            _: &workflow::WorkflowGuard,
            _: &str,
            body: &str,
        ) -> Result<()> {
            assert!(!body.contains("PendingActionRow"));
            assert!(!body.contains("PRIVATE"));
            let mut posts = self.posts.lock().unwrap();
            if !posts.iter().any(|p| p == body) {
                posts.push(body.into());
            }
            if self.summary_fail.swap(false, Ordering::SeqCst) {
                bail!("lost reply after commit")
            };
            Ok(())
        }
    }
    struct Fixture {
        repo: tempfile::TempDir,
        control: tempfile::TempDir,
        issue: Issue,
    }
    impl Fixture {
        async fn new() -> Self {
            let repo = tempfile::tempdir().unwrap();
            let control = tempfile::tempdir_in("/tmp").unwrap();
            for args in [
                vec!["init", "-q", "-b", "candidate"],
                vec![
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.test",
                    "-c",
                    "commit.gpgsign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "original",
                ],
                vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
            ] {
                let output = tokio::process::Command::new("/usr/bin/git")
                    .args(args)
                    .current_dir(repo.path())
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .output()
                    .await
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Self {
                repo,
                control,
                issue: Issue {
                    number: 1766,
                    title: "Preserve coverage safeguards".into(),
                    body:
                        "- Reject direct threads\n- Preserve unknown identity and hand-back guards"
                            .into(),
                    author: "owner".into(),
                    author_trusted: true,
                    research_filed: false,
                },
            }
        }
        async fn acquire(&self) -> workflow::WorkflowGuard {
            self.acquire_at(chrono::Utc::now().timestamp_millis()).await
        }
        async fn acquire_at(&self, now: i64) -> workflow::WorkflowGuard {
            let request =
                serde_json::to_string(&(self.issue.number, &self.issue.title, &self.issue.body))
                    .unwrap();
            workflow::WorkflowGuard::acquire_fixture_at(
                &self
                    .control
                    .path()
                    .canonicalize()
                    .unwrap()
                    .join("workflow.db"),
                workflow::Start {
                    repo_root: self.repo.path(),
                    worktree: self.repo.path(),
                    repository: "owner/repo",
                    source_issue: Some(self.issue.number),
                    pr: None,
                    branch: "candidate",
                    contract: &request,
                    environment: "fixture",
                },
                now,
            )
            .await
            .unwrap()
            .unwrap()
        }
        async fn held(&self) -> workflow::WorkflowGuard {
            let mut guard = self.acquire().await;
            guard
                .hold_initial_acceptance(
                    "acceptance_verifier_repair_exhausted",
                    "await_acceptance_input_change",
                )
                .await
                .unwrap();
            guard
                .record_scope_stage_response(
                    &"f".repeat(64),
                    &serde_json::json!({"detail":"PRIVATE E0308 tuple versus PendingActionRow"})
                        .to_string(),
                )
                .unwrap();
            guard
        }
    }
    const PROVIDERS: [ProviderKind; 2] = [ProviderKind::Claude, ProviderKind::Codex];
    #[tokio::test]
    async fn failed_rescope_switches_independent_provider_then_publishes_observable_children() {
        let f = Fixture::new().await;
        let mut g = f.held().await;
        let fake = Fake::new(vec![ResultKind::Invalid, ResultKind::Children]);
        assert!(execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake
        )
        .await
        .unwrap()
        .is_some());
        assert_eq!(*fake.calls.lock().unwrap(), PROVIDERS);
        assert!(fake.posts.lock().unwrap().is_empty());
        let id = g.id().to_owned();
        g.finish().unwrap();
        let s = crate::autoship_workflow::WorkflowStore::open(
            &f.control.path().canonicalize().unwrap().join("workflow.db"),
        )
        .unwrap()
        .snapshot(&id)
        .unwrap()
        .unwrap();
        assert_eq!(s.checkpoint.next.action, "await_child_completion");
        assert_eq!(s.model_calls, 0);
    }
    #[tokio::test]
    async fn capacity_wait_automatically_resumes_and_transient_failure_does_not_burn_a_strategy() {
        let f = Fixture::new().await;
        let mut g = f.held().await;
        let fake = Fake::new(vec![ResultKind::Transient, ResultKind::Children]);
        fake.ready.store(false, Ordering::SeqCst);
        execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake,
        )
        .await
        .unwrap();
        assert!(fake.calls.lock().unwrap().is_empty());
        fake.ready.store(true, Ordering::SeqCst);
        execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake,
        )
        .await
        .unwrap();
        execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake,
        )
        .await
        .unwrap();
        assert_eq!(
            *fake.calls.lock().unwrap(),
            vec![ProviderKind::Claude, ProviderKind::Claude]
        );
        assert!(fake.posts.lock().unwrap().is_empty());
        g.finish().unwrap();
    }
    #[tokio::test]
    async fn all_completed_strategies_fail_then_restart_reconciles_one_public_summary_without_more_models(
    ) {
        let f = Fixture::new().await;
        let mut g = f.held().await;
        let fake = Fake::new(vec![ResultKind::Invalid, ResultKind::Invalid]);
        fake.summary_fail.store(true, Ordering::SeqCst);
        execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake,
        )
        .await
        .unwrap();
        g.finish().unwrap();
        let mut g = f
            .acquire_at(chrono::Utc::now().timestamp_millis() + 300_001)
            .await;
        execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake,
        )
        .await
        .unwrap();
        let id = g.id().to_owned();
        g.finish().unwrap();
        assert_eq!(fake.calls.lock().unwrap().len(), 2);
        assert_eq!(fake.posts.lock().unwrap().len(), 1);
        let s = crate::autoship_workflow::WorkflowStore::open(
            &f.control.path().canonicalize().unwrap().join("workflow.db"),
        )
        .unwrap()
        .snapshot(&id)
        .unwrap()
        .unwrap();
        // #2289: exhaustion is never terminal. The summary is posted once and
        // the next strategy cycle is scheduled after a capped backoff.
        assert_eq!(s.checkpoint.next.reason, "acceptance_strategy_backoff");
        assert_eq!(s.checkpoint.next.action, ACTION);
        assert!(!crate::autoship_workflow::awaits_initial_input(
            &s.checkpoint
        ));
        assert!(s.checkpoint.next.eligible_at_ms > chrono::Utc::now().timestamp_millis());
        assert_eq!(s.model_calls, 0);
    }
    fn snapshot(f: &Fixture, id: &str) -> crate::autoship_workflow::Snapshot {
        crate::autoship_workflow::WorkflowStore::open(
            &f.control.path().canonicalize().unwrap().join("workflow.db"),
        )
        .unwrap()
        .snapshot(id)
        .unwrap()
        .unwrap()
    }
    #[tokio::test]
    async fn repair_exhaustion_escalates_strategy_with_capped_backoff_and_never_parks() {
        let f = Fixture::new().await;
        let mut g = f.held().await;
        let id = g.id().to_owned();
        let cycles = 7u32;
        let fake = Fake::new(vec![ResultKind::Invalid; 2 * cycles as usize]);
        for cycle in 0..cycles {
            let before = chrono::Utc::now().timestamp_millis();
            execute_with(f.repo.path(), f.repo.path(), &f.issue, &mut g, &PROVIDERS, &fake)
                .await
                .unwrap()
                .expect("an exhausted cycle must report its scheduled retry");
            let after = chrono::Utc::now().timestamp_millis();
            g.finish().unwrap();
            let s = snapshot(&f, &id);
            assert_eq!(s.checkpoint.next.reason, "acceptance_strategy_backoff", "cycle {cycle}");
            assert_eq!(s.checkpoint.next.action, ACTION);
            assert!(!crate::autoship_workflow::awaits_initial_input(&s.checkpoint));
            let wait = s.checkpoint.next.eligible_at_ms;
            assert!(
                wait >= before + retry_policy::backoff_ms(cycle)
                    && wait <= after + retry_policy::backoff_ms(cycle),
                "cycle {cycle} backoff is not capped exponential"
            );
            assert!(wait - after <= retry_policy::BACKOFF_CAP_MS);
            // Not claimable early; claimable once the backoff elapses.
            g = f.acquire_at(wait).await;
        }
        g.finish().unwrap();
        let seen = fake.strategies.lock().unwrap().clone();
        assert_eq!(seen.len(), 2 * cycles as usize, "every cycle made new attempts");
        for cycle in 0..cycles {
            let rungs: Vec<_> = seen.iter().filter(|(c, _, _)| *c == cycle).collect();
            assert_eq!(rungs.len(), 2);
            assert!(rungs.iter().all(|(_, name, _)| *name == retry_policy::strategy(cycle).name()));
            let order: Vec<_> = rungs.iter().map(|(_, _, p)| *p).collect();
            if retry_policy::strategy(cycle) == Strategy::SwitchProvider {
                assert_eq!(order, vec![ProviderKind::Codex, ProviderKind::Claude]);
            } else {
                assert_eq!(order, PROVIDERS.to_vec());
            }
        }
        assert_eq!(fake.posts.lock().unwrap().len(), 1, "one public summary, not one per cycle");
    }
    #[tokio::test]
    async fn daily_model_budget_defers_the_next_cycle_without_model_calls_then_resumes() {
        let f = Fixture::new().await;
        let mut g = f.held().await;
        let id = g.id().to_owned();
        let fake = Fake::new(vec![ResultKind::Invalid, ResultKind::Invalid, ResultKind::Children]);
        execute_with(f.repo.path(), f.repo.path(), &f.issue, &mut g, &PROVIDERS, &fake)
            .await
            .unwrap();
        g.finish().unwrap();
        let wait = snapshot(&f, &id).checkpoint.next.eligible_at_ms;
        let reopen = wait + 3 * retry_policy::DAY_MS / 4;
        *fake.budget.lock().unwrap() = Some(reopen);
        let mut g = f.acquire_at(wait).await;
        execute_with(f.repo.path(), f.repo.path(), &f.issue, &mut g, &PROVIDERS, &fake)
            .await
            .unwrap()
            .unwrap();
        g.finish().unwrap();
        assert_eq!(fake.strategies.lock().unwrap().len(), 2, "spent budget made a model call");
        let s = snapshot(&f, &id);
        assert_eq!(s.checkpoint.next.reason, "acceptance_strategy_budget_wait");
        assert_eq!(s.checkpoint.next.action, ACTION);
        assert_eq!(s.checkpoint.next.eligible_at_ms, reopen);
        assert!(!crate::autoship_workflow::awaits_initial_input(&s.checkpoint));
        *fake.budget.lock().unwrap() = None;
        let mut g = f.acquire_at(reopen).await;
        execute_with(f.repo.path(), f.repo.path(), &f.issue, &mut g, &PROVIDERS, &fake)
            .await
            .unwrap()
            .unwrap();
        g.finish().unwrap();
        assert_eq!(fake.strategies.lock().unwrap()[2].0, 1, "budget wait resumed the next cycle");
        assert_eq!(snapshot(&f, &id).checkpoint.next.action, "await_child_completion");
    }
    #[test]
    fn decomposition_cannot_reduce_owner_requirements_to_a_smaller_acceptance() {
        let issue = Issue {
            number: 1,
            title: "Original".into(),
            body: "Description\n- First constraint\n- Second constraint".into(),
            author: String::new(),
            author_trusted: true,
            research_filed: false,
        };
        assert_eq!(
            requirements(&issue).unwrap(),
            vec!["Description", "- First constraint", "- Second constraint"]
        );
    }
    #[tokio::test]
    async fn registered_acceptance_failure_keeps_its_original_context_when_acquired_for_planning() {
        use crate::autoship_workflow::{NextAction, RecoveryContext, Stage, WorkflowStore};
        let f = Fixture::new().await;
        let g = f.held().await;
        let id = g.id().to_owned();
        g.finish().unwrap();
        let database = f.control.path().canonicalize().unwrap().join("workflow.db");
        let mut store = WorkflowStore::open(&database).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let lease = store
            .claim(&id, "fixture-registration", now, 60_000)
            .unwrap()
            .unwrap();
        let mut cp = store.snapshot(&id).unwrap().unwrap().checkpoint;
        cp.binding.contract = "a".repeat(64);
        cp.stage = Stage::Build;
        cp.next = NextAction {
            reason: "acceptance_failed".into(),
            action: "repair_implementation".into(),
            eligible_at_ms: 0,
        };
        let source =
            serde_json::to_string(&(f.issue.number, &f.issue.title, &f.issue.body)).unwrap();
        let context = RecoveryContext {
            version: 1,
            workflow: id.clone(),
            repository: "owner/repo".into(),
            worktree: f.repo.path().canonicalize().unwrap(),
            source_request: source,
            source_pr: None,
            contract: format!("registered:{}", cp.binding.contract),
            environment: "fixture".into(),
            binding: cp.binding.clone(),
        };
        store
            .checkpoint_with_execution_context(&lease, &cp, &context, now + 1)
            .unwrap();
        store.release(&lease, now + 2).unwrap();
        drop(store);
        let mut g = f.acquire_at(now + 3).await;
        let material = g
            .begin_acceptance_escalation()
            .await
            .unwrap()
            .expect("registered acceptance failure was reset instead of queued for planning");
        assert_eq!(material.original, cp);
        let fake = Fake::new(vec![ResultKind::Children]);
        assert!(execute_with(
            f.repo.path(),
            f.repo.path(),
            &f.issue,
            &mut g,
            &PROVIDERS,
            &fake
        )
        .await
        .unwrap()
        .is_some());
        assert_eq!(g.binding(), &cp.binding);
        g.finish().unwrap();
    }
}
