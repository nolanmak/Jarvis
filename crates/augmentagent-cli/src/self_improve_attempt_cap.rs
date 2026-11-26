//! #2467 — close out issues that reached their attempt or spend cap.
//!
//! Model-free and run from the worker's sweep tick, so it keeps working while
//! the global model budget is spent. Each due issue gets exactly one comment
//! through the existing `agent-gave-up` path (reason, what was tried, the last
//! blockers and the calls spent), the label, and then retirement of its
//! workflow, which releases its native sessions and leases. The decision is
//! recorded before the comment, so a restart resumes the close-out instead of
//! commenting again; a comment whose reply was lost is looked for by its
//! marker before it is sent again.
//!
//! An issue edit or the label's removal clears a give-up: the label comes
//! off if it is still on, the caps count afresh and the workflow starts again
//! from intake. Both are read from the one open-issue listing per pass; a
//! pass with nothing due makes no GitHub call at all.
use super::*;
use crate::autoship_workflow::attempt_cap::{self, Caps, GiveUp, GiveUpState};
use crate::autoship_workflow::WorkflowStore;

/// Close-outs per pass. Each costs a few GitHub writes; the rest wait.
const PER_PASS: usize = 4;
/// Minimum time between passes.
const INTERVAL_MS: i64 = 4 * 60 * 1000;
static LAST_PASS: std::sync::Mutex<i64> = std::sync::Mutex::new(i64::MIN);

/// Reason code the give-up comment records, grouped by `autopr health`.
pub(super) fn reason_code(reason: &str) -> String {
    match reason {
        attempt_cap::ATTEMPTS_REASON => "cap:attempts".into(),
        attempt_cap::CALLS_REASON => "cap:model-calls".into(),
        attempt_cap::DECOMPOSITION_SPENT_REASON => "cap:decomposition".into(),
        other => format!("cap:{}", other.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-")),
    }
}

/// The GitHub side of a pass, so the sequencing is testable without GitHub.
#[async_trait::async_trait]
pub(super) trait Github: Sync {
    async fn issues(&self) -> Option<gh_budget::OpenIssues>;
    /// The reason comment and the label, through [`label_gave_up`].
    async fn give_up(&self, give_up: &GiveUp, note: GaveUpNote<'_>) -> Result<()>;
    async fn comment_recorded(&self, issue: u64, marker: &str) -> Result<bool>;
    async fn unlabel(&self, issue: u64) -> Result<()>;
}

struct Production<'a> {
    root: &'a Path,
    repository: &'a str,
}
#[async_trait::async_trait]
impl Github for Production<'_> {
    async fn issues(&self) -> Option<gh_budget::OpenIssues> {
        gh_budget::OpenIssues::read(self.root, self.repository).await
    }
    async fn give_up(&self, give_up: &GiveUp, note: GaveUpNote<'_>) -> Result<()> {
        label_gave_up(self.root, give_up.issue, &reason_code(&give_up.reason), true, give_up.pr, note).await
    }
    async fn comment_recorded(&self, issue: u64, marker: &str) -> Result<bool> {
        let (ok, raw, _) = run(&gh_bin(), &["issue", "view", &issue.to_string(), "--repo", self.repository, "--json", "comments"], self.root).await?;
        anyhow::ensure!(ok, "give-up comment reconciliation unavailable");
        Ok(raw.contains(marker))
    }
    async fn unlabel(&self, issue: u64) -> Result<()> {
        let (ok, _, error) = run(&gh_bin(), &["issue", "edit", &issue.to_string(), "--repo", self.repository, "--remove-label", GAVE_UP_LABEL], self.root).await?;
        anyhow::ensure!(ok, "removing `{GAVE_UP_LABEL}` from #{issue} failed: {}", truncate(&error, 300));
        Ok(())
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Pass {
    pub gave_up: Vec<u64>,
    pub cleared: Vec<u64>,
}

/// One pass against `path` with an injected GitHub.
pub(super) async fn pass_with(path: &Path, repository: &str, now: i64, caps: Caps, github: &impl Github) -> Result<Pass> {
    let mut pass = Pass::default();
    let (due, clearable) = {
        let Some(store) = WorkflowStore::open_read_only(path)? else { return Ok(pass) };
        (store.give_ups_due(repository, now, caps)?, store.clearable_give_ups(repository)?)
    };
    if due.is_empty() && clearable.is_empty() {
        return Ok(pass);
    }
    // One listing serves both halves. Without it nothing is decided: a
    // closed or edited issue must not be commented on from stale state.
    let Some(issues) = github.issues().await else { return Ok(pass) };
    let mut store = WorkflowStore::open(path)?;
    for give_up in clearable {
        let number = give_up.issue;
        if !issues.is_open(number) {
            continue;
        }
        let labelled = issues.get(number).is_some_and(has_gave_up_label);
        let request = issues.request(number);
        let edited = matches!((&give_up.request, &request), (Some(then), Some(now)) if then != now);
        if give_up.request.is_none() {
            if let Some(request) = &request {
                store.note_give_up_request(&give_up.workflow, request)?;
            }
        }
        if labelled && !edited {
            continue;
        }
        if labelled {
            if let Err(error) = github.unlabel(number).await {
                warn!(issue = number, "give-up clear deferred: {error:#}");
                continue;
            }
        }
        if store.clear_give_up(&give_up.workflow, if edited { "issue_edited" } else { "label_removed" }, now)? {
            info!(issue = number, edited, "give-up cleared; the issue is back in the pool with a fresh count");
            pass.cleared.push(number);
        }
    }
    for (workflow, reason) in due.into_iter().take(PER_PASS) {
        let Some(number) = attempt_cap::issue_number(&workflow) else { continue };
        if !issues.is_open(number) {
            continue; // the closed-target sweep retires it
        }
        // A manually routed issue is a person's; its paid claims are already
        // refused, and the terminal routing check would refuse the comment.
        if issues.get(number).is_some_and(|row| has_label(row, MANUAL_ENGINEERING_LABEL)) {
            continue;
        }
        let give_up = store.decide_give_up(&workflow, &reason, issues.request(number).as_deref(), now)?;
        match close_out(&mut store, github, &give_up, caps, now).await {
            Ok(true) => {
                info!(issue = number, reason = %give_up.reason, "autoship gave up on the issue; workflow retired");
                pass.gave_up.push(number);
            }
            Ok(false) => tracing::debug!(issue = number, "give-up close-out waits for the workflow to drain"),
            Err(error) => warn!(issue = number, "give-up close-out deferred: {error:#}"),
        }
    }
    Ok(pass)
}

/// Drive one give-up from wherever its close-out stopped. `Ok(false)` while
/// a lease or an unresolved operation still holds the workflow.
async fn close_out(store: &mut WorkflowStore, github: &impl Github, give_up: &GiveUp, caps: Caps, now: i64) -> Result<bool> {
    match give_up.state {
        GiveUpState::Decided | GiveUpState::Commenting => {
            let detail = attempt_cap::comment_detail(give_up, caps);
            let posted = give_up.state == GiveUpState::Commenting
                && github.comment_recorded(give_up.issue, &attempt_cap::comment_marker(give_up)).await?;
            store.set_give_up_state(&give_up.workflow, GiveUpState::Commenting, now)?;
            let note = if posted { GaveUpNote::AlreadyPosted } else { GaveUpNote::Detailed(&detail) };
            github.give_up(give_up, note).await?;
            store.set_give_up_state(&give_up.workflow, GiveUpState::Labelled, now)?;
        }
        GiveUpState::Commented => {
            github.give_up(give_up, GaveUpNote::AlreadyPosted).await?;
            store.set_give_up_state(&give_up.workflow, GiveUpState::Labelled, now)?;
        }
        GiveUpState::Labelled | GiveUpState::Retired => {}
    }
    store.retire_given_up(&give_up.workflow, now)
}

/// Run from the worker's sweep tick, at most every [`INTERVAL_MS`].
pub(super) async fn sweep(root: &Path, path: &Path) -> Result<Pass> {
    let now = chrono::Utc::now().timestamp_millis();
    {
        let mut last = LAST_PASS.lock().unwrap_or_else(|e| e.into_inner());
        if now.saturating_sub(*last) < INTERVAL_MS {
            return Ok(Pass::default());
        }
        *last = now;
    }
    let repository = workflow::canonical_repository(root).await?;
    pass_with(path, &repository, now, Caps::configured(), &Production { root, repository: &repository }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autoship_workflow::{Binding, NextAction, ResourceCharge, Stage};
    use std::sync::Mutex;

    const CAPS: Caps = Caps { tries: 3, issue_calls: 300, global_calls: 1_500 };

    #[derive(Default)]
    struct Fake {
        rows: Mutex<Vec<serde_json::Value>>,
        comments: Mutex<Vec<(u64, String)>>,
        labels: Mutex<Vec<u64>>,
        unlabels: Mutex<Vec<u64>>,
        lose_label_reply: std::sync::atomic::AtomicBool,
    }
    impl Fake {
        fn issue(&self, number: u64, body: &str, labelled: bool) {
            let labels = if labelled { serde_json::json!([{"name": GAVE_UP_LABEL}]) } else { serde_json::json!([]) };
            let mut rows = self.rows.lock().unwrap();
            rows.retain(|r| r["number"] != number);
            rows.push(serde_json::json!({"number": number, "state": "OPEN", "title": "Widget", "body": body, "labels": labels}));
        }
    }
    #[async_trait::async_trait]
    impl Github for Fake {
        async fn issues(&self) -> Option<gh_budget::OpenIssues> {
            gh_budget::OpenIssues::from_rows(self.rows.lock().unwrap().clone())
        }
        async fn give_up(&self, give_up: &GiveUp, note: GaveUpNote<'_>) -> Result<()> {
            if let GaveUpNote::Detailed(detail) = note {
                self.comments.lock().unwrap().push((give_up.issue, gave_up_note(&reason_code(&give_up.reason), detail)));
            }
            if self.lose_label_reply.swap(false, std::sync::atomic::Ordering::SeqCst) {
                bail!("lost reply");
            }
            self.labels.lock().unwrap().push(give_up.issue);
            let body = self.rows.lock().unwrap().iter().find(|r| r["number"] == give_up.issue).unwrap()["body"].as_str().unwrap().to_owned();
            self.issue(give_up.issue, &body, true);
            Ok(())
        }
        async fn comment_recorded(&self, issue: u64, marker: &str) -> Result<bool> {
            Ok(self.comments.lock().unwrap().iter().any(|(n, body)| *n == issue && body.contains(marker)))
        }
        async fn unlabel(&self, issue: u64) -> Result<()> {
            self.unlabels.lock().unwrap().push(issue);
            let body = self.rows.lock().unwrap().iter().find(|r| r["number"] == issue).unwrap()["body"].as_str().unwrap().to_owned();
            self.issue(issue, &body, false);
            Ok(())
        }
    }

    fn over_spend(path: &Path, id: &str, calls: u64) {
        let mut store = WorkflowStore::open(path).unwrap();
        let binding = Binding { head: "a".repeat(40), contract: "contract".into(), evidence_hash: "env".into() };
        store.register(id, "agent-fix/issue-2329", binding, 1).unwrap();
        let lease = store.claim(id, "worker", 2, 1_000).unwrap().unwrap();
        store.register_native_owner(&lease, "/tmp/audit-2329", 2).unwrap();
        store.charge(&lease, "paid", ResourceCharge { model_calls: calls, tokens: 1, wall_ms: 1 }, 2).unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::Scope;
        cp.next = NextAction { reason: "own_pr_rescope_exhausted".into(), action: "rescope_wait_fixture".into(), eligible_at_ms: 10 * 3_600_000 };
        store.checkpoint(&lease, &cp, 3).unwrap();
        store.release(&lease, 4).unwrap();
    }

    /// The deploy case (#2329 at 803 calls): one comment with what was tried,
    /// the blockers and the calls, the label, retirement, and nothing more on
    /// later passes.
    #[tokio::test]
    async fn the_cap_is_reached_then_exactly_one_comment_the_label_and_retirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let id = "owner/repo#2329";
        over_spend(&path, id, 803);
        let fake = Fake::default();
        fake.issue(2329, "Fix the widget.", false);
        let first = pass_with(&path, "owner/repo", 100, CAPS, &fake).await.unwrap();
        assert_eq!(first.gave_up, vec![2329]);
        for later in [200, 300] {
            assert_eq!(pass_with(&path, "owner/repo", later, CAPS, &fake).await.unwrap(), Pass::default());
        }
        let comments = fake.comments.lock().unwrap().clone();
        assert_eq!(comments.len(), 1, "exactly one comment");
        let body = &comments[0].1;
        assert!(body.starts_with(&format!("{GAVE_UP_REASON_MARKER_PREFIX}cap:model-calls -->")), "{body}");
        assert!(body.contains("803 since the last reset") && body.contains("`own_pr_rescope_exhausted`"), "{body}");
        assert_eq!(*fake.labels.lock().unwrap(), vec![2329]);
        let mut store = WorkflowStore::open(&path).unwrap();
        let s = store.snapshot(id).unwrap().unwrap();
        assert_eq!((s.checkpoint.stage, s.checkpoint.next.action.as_str()), (Stage::Retired, "complete"));
        assert_eq!(s.lease_until_ms, 0);
        assert_eq!(store.pending_native_retirements(400, 16).unwrap().len(), 1);
        assert_eq!(gave_up_breakdown(&[(2329, vec![body.clone()])], &AttemptHistory::default()), vec![("cap:model-calls".into(), 1)]);
    }

    #[tokio::test]
    async fn a_lost_reply_resumes_the_close_out_without_a_second_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        over_spend(&path, "owner/repo#7", 300);
        let fake = Fake::default();
        fake.issue(7, "Fix it.", false);
        fake.lose_label_reply.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(pass_with(&path, "owner/repo", 100, CAPS, &fake).await.unwrap().gave_up.is_empty());
        assert_eq!(pass_with(&path, "owner/repo", 200, CAPS, &fake).await.unwrap().gave_up, vec![7]);
        assert_eq!(fake.comments.lock().unwrap().len(), 1);
        assert_eq!(*fake.labels.lock().unwrap(), vec![7]);
    }

    #[tokio::test]
    async fn an_issue_edit_or_label_removal_clears_the_give_up() {
        for edit in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("workflow.db");
            let id = "owner/repo#9";
            over_spend(&path, id, 400);
            let fake = Fake::default();
            fake.issue(9, "Fix it.", false);
            assert_eq!(pass_with(&path, "owner/repo", 100, CAPS, &fake).await.unwrap().gave_up, vec![9]);
            // An unrelated pass with the label still on changes nothing.
            assert_eq!(pass_with(&path, "owner/repo", 200, CAPS, &fake).await.unwrap(), Pass::default());
            if edit {
                fake.issue(9, "Fix it, and here is the missing detail.", true);
            } else {
                fake.issue(9, "Fix it.", false);
            }
            let cleared = pass_with(&path, "owner/repo", 300, CAPS, &fake).await.unwrap();
            assert_eq!(cleared.cleared, vec![9], "edit={edit}");
            assert_eq!(*fake.unlabels.lock().unwrap(), if edit { vec![9] } else { vec![] });
            let mut store = WorkflowStore::open(&path).unwrap();
            let s = store.snapshot(id).unwrap().unwrap();
            assert_eq!((s.checkpoint.stage, s.checkpoint.next.reason.as_str()), (Stage::Intake, attempt_cap::CLEARED_REASON));
            assert_eq!(store.attempt_usage(id).unwrap().calls, 0, "a fresh count");
            assert!(store.claim_productive(id, "worker", 301, 100).unwrap().is_some());
            assert_eq!(fake.comments.lock().unwrap().len(), 1);
        }
    }

    /// With the global budget spent no new paid turn is claimed and a run
    /// holds before selecting work, but merge reconciliation (finalize) still
    /// claims, and the give-up close-out still runs.
    #[tokio::test]
    async fn the_global_budget_exhausted_stops_paid_turns_but_finalize_still_runs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let now = chrono::Utc::now().timestamp_millis();
        let binding = Binding { head: "a".repeat(40), contract: "contract".into(), evidence_hash: "env".into() };
        let mut store = WorkflowStore::open(&path).unwrap();
        let spender = "owner/repo/pr/5";
        store.register(spender, "owner-branch", binding.clone(), now).unwrap();
        let lease = store.claim(spender, "worker", now, 1_000).unwrap().unwrap();
        let cap = attempt_cap::global_daily_model_calls();
        store.charge(&lease, "paid", ResourceCharge { model_calls: cap - 1, tokens: 1, wall_ms: 1 }, now).unwrap();
        store.release(&lease, now).unwrap();
        let fresh = "owner/repo#6";
        store.register(fresh, "agent-fix/issue-6", binding.clone(), now).unwrap();
        assert!(global_budget_hold(&path, now + 1).unwrap().is_none());
        let lease = store.claim_productive(fresh, "worker", now + 1, 1_000).unwrap().expect("budget left");
        store.charge(&lease, "last", ResourceCharge { model_calls: 1, tokens: 1, wall_ms: 1 }, now + 1).unwrap();
        store.release(&lease, now + 1).unwrap();
        let held = global_budget_hold(&path, now + 2).unwrap().expect("a run holds with the budget spent");
        assert!(held.message.contains("no new paid work"), "{}", held.message);
        assert!(store.claim_productive(fresh, "worker", now + 2, 1_000).unwrap().is_none(), "a paid turn started");
        let finalize = store.claim_reconciliation(fresh, "merge-sweep", now + 2, 1_000).unwrap();
        assert!(finalize.is_some(), "merge reconciliation must keep running");
        store.release(&finalize.unwrap(), now + 2).unwrap();
        // The model-free close-out is not paid work either.
        over_spend(&path, "owner/repo#8", 300);
        let fake = Fake::default();
        fake.issue(8, "Fix it.", false);
        assert_eq!(pass_with(&path, "owner/repo", now + 3, CAPS, &fake).await.unwrap().gave_up, vec![8]);
        assert!(global_budget_hold(&path, now + retry_policy_day() + 2).unwrap().is_none(), "the window reopens");
    }
    fn retry_policy_day() -> i64 {
        crate::autoship_workflow::retry_policy::DAY_MS
    }

    #[tokio::test]
    async fn nothing_due_makes_no_github_call() {
        struct Never;
        #[async_trait::async_trait]
        impl Github for Never {
            async fn issues(&self) -> Option<gh_budget::OpenIssues> { panic!("listing with nothing due") }
            async fn give_up(&self, _: &GiveUp, _: GaveUpNote<'_>) -> Result<()> { unreachable!() }
            async fn comment_recorded(&self, _: u64, _: &str) -> Result<bool> { unreachable!() }
            async fn unlabel(&self, _: u64) -> Result<()> { unreachable!() }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        assert_eq!(pass_with(&path, "owner/repo", 1, CAPS, &Never).await.unwrap(), Pass::default());
        over_spend(&path, "owner/repo#3", 12);
        assert_eq!(pass_with(&path, "owner/repo", 5, CAPS, &Never).await.unwrap(), Pass::default());
    }
}
