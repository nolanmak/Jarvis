//! Reconcile the source issue before spending on, or merging, managed work.
//! A publication record proves ownership independently of a stale approval.
use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SourceAction {
    Continue,
    Hold(&'static str),
    Retired,
    WouldRetire,
}

#[derive(Debug, PartialEq, Eq)]
enum SourceState {
    Open,
    Completed,
    Cancelled,
    Unknown,
}

fn source_state(value: &serde_json::Value) -> SourceState {
    match (value["state"].as_str(), value["state_reason"].as_str()) {
        (Some("open"), _) => SourceState::Open,
        (Some("closed"), Some("completed")) => SourceState::Completed,
        (Some("closed"), Some("not_planned")) => SourceState::Cancelled,
        _ => SourceState::Unknown,
    }
}

#[async_trait::async_trait]
trait Github {
    async fn call(&mut self, args: &[&str]) -> Result<serde_json::Value>;
}

struct Transport<'a>(&'a Path);
#[async_trait::async_trait]
impl Github for Transport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<serde_json::Value> {
        let (ok, out, _) = run(&gh_bin(), args, self.0).await?;
        anyhow::ensure!(ok, "source reconciliation GitHub call failed");
        if out.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        // Mutation responses are not evidence for another decision.
        if args.starts_with(&["pr", "close"]) {
            return Ok(serde_json::Value::Null);
        }
        Ok(serde_json::from_str(&out)?)
    }
}

pub(super) async fn reconcile(root: &Path, pr: u64, issue: u64, dry_run: bool) -> SourceAction {
    let recorded = opened_prs::get(issue).is_some_and(|r| r.pr == pr);
    match reconcile_with(&mut Transport(root), pr, issue, recorded, dry_run).await {
        Ok(action) => action,
        Err(error) => {
            warn!(pr, issue, "source reconciliation deferred: {error:#}");
            SourceAction::Hold("source-reconciliation-unavailable")
        }
    }
}

async fn reconcile_with(
    github: &mut impl Github,
    pr: u64,
    issue: u64,
    recorded: bool,
    dry_run: bool,
) -> Result<SourceAction> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/issues/{issue}");
    let source = github.call(&["api", &endpoint]).await?;
    let state = source_state(&source);
    if state == SourceState::Open {
        return Ok(SourceAction::Continue);
    }
    if state == SourceState::Unknown {
        return Ok(SourceAction::Hold("source-state-unknown"));
    }
    if !recorded {
        return Ok(SourceAction::Hold("source-closed-ownership-unverified"));
    }
    let number = pr.to_string();
    let pull = github
        .call(&[
            "pr",
            "view",
            &number,
            "--json",
            "state,body,headRefName,isCrossRepository",
        ])
        .await?;
    if pull["state"] == "CLOSED" {
        return Ok(SourceAction::Retired);
    }
    if pull["state"] != "OPEN" {
        return Ok(SourceAction::Hold("source-closed-pr-not-open"));
    }
    let branch = format!("{BRANCH_PREFIX}{issue}");
    let marker = format!("{SELF_IMPROVE_BODY_MARKER}{issue}.");
    if pull["isCrossRepository"] != false
        || pull["headRefName"] != branch
        || !pull["body"]
            .as_str()
            .is_some_and(|body| body.contains(&marker))
    {
        return Ok(SourceAction::Hold("source-closed-ownership-unverified"));
    }
    if dry_run {
        return Ok(SourceAction::WouldRetire);
    }
    // Reopening the source while we inspected the PR must cancel retirement.
    let current = github.call(&["api", &endpoint]).await?;
    if source_state(&current) != state {
        return Ok(SourceAction::Hold("source-state-changed"));
    }
    let reason = if state == SourceState::Completed {
        "completed"
    } else {
        "cancelled"
    };
    let comment = format!("Autoship retired this managed PR because its source issue #{issue} is {reason}. The branch is preserved. Reopen the source and PR to resume this work.");
    github
        .call(&["pr", "close", &number, "--comment", &comment])
        .await?;
    Ok(SourceAction::Retired)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Fixture {
        replies: VecDeque<Result<serde_json::Value>>,
        calls: Vec<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl Github for Fixture {
        async fn call(&mut self, args: &[&str]) -> Result<serde_json::Value> {
            self.calls
                .push(args.iter().map(|s| s.to_string()).collect());
            self.replies.pop_front().expect("unexpected GitHub call")
        }
    }
    fn closed(reason: &str) -> serde_json::Value {
        serde_json::json!({"state":"closed","state_reason":reason})
    }
    fn pull() -> serde_json::Value {
        serde_json::json!({"state":"OPEN","body":"Automated self-improvement for #10.","headRefName":"agent-fix/issue-10","isCrossRepository":false})
    }
    fn fixture(values: Vec<serde_json::Value>) -> Fixture {
        Fixture {
            replies: values.into_iter().map(Ok).collect(),
            calls: vec![],
        }
    }
    #[tokio::test]
    async fn completed_and_cancelled_sources_retire_without_deleting_the_branch() {
        for reason in ["completed", "not_planned"] {
            let mut f = fixture(vec![
                closed(reason),
                pull(),
                closed(reason),
                serde_json::Value::Null,
            ]);
            assert_eq!(
                reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
                SourceAction::Retired
            );
            assert_eq!(&f.calls.last().unwrap()[..3], ["pr", "close", "20"]);
            assert!(!f.calls.iter().flatten().any(|s| s == "--delete-branch"));
        }
    }
    #[tokio::test]
    async fn unknown_state_and_missing_ownership_never_close() {
        for (v, owned) in [
            (serde_json::Value::Null, true),
            (closed("completed"), false),
            (closed("unknown"), true),
        ] {
            let mut f = fixture(vec![v]);
            assert!(matches!(
                reconcile_with(&mut f, 20, 10, owned, false).await.unwrap(),
                SourceAction::Hold(_)
            ));
            assert_eq!(f.calls.len(), 1);
        }
    }
    #[tokio::test]
    async fn fork_and_reused_branch_are_not_retired() {
        for key in ["isCrossRepository", "body", "headRefName"] {
            let mut p = pull();
            p[key] = if key == "isCrossRepository" {
                true.into()
            } else {
                "unmanaged".into()
            };
            let mut f = fixture(vec![closed("completed"), p]);
            assert!(matches!(
                reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
                SourceAction::Hold(_)
            ));
            assert_eq!(f.calls.len(), 2);
        }
    }
    #[tokio::test]
    async fn dry_run_is_read_only_and_reopened_source_cancels_close() {
        let mut f = fixture(vec![closed("completed"), pull()]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, true).await.unwrap(),
            SourceAction::WouldRetire
        );
        assert_eq!(f.calls.len(), 2);
        let mut f = fixture(vec![
            closed("completed"),
            pull(),
            serde_json::json!({"state":"open"}),
        ]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
            SourceAction::Hold("source-state-changed")
        );
    }
    #[tokio::test]
    async fn failed_close_remains_retryable_and_open_sources_continue() {
        let mut f = fixture(vec![closed("completed"), pull(), closed("completed")]);
        f.replies
            .push_back(Err(anyhow::anyhow!("synthetic transport failure")));
        assert!(reconcile_with(&mut f, 20, 10, true, false).await.is_err());
        let mut f = fixture(vec![
            closed("completed"),
            pull(),
            closed("completed"),
            serde_json::Value::Null,
        ]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
            SourceAction::Retired
        );
        let mut f = fixture(vec![serde_json::json!({"state":"open"})]);
        assert_eq!(
            reconcile_with(&mut f, 20, 10, true, false).await.unwrap(),
            SourceAction::Continue
        );
    }
}
