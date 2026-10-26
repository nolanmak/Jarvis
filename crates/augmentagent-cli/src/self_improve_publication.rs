//! Recover uncertain GitHub creation before attempting another publication.
use super::*;
use crate::autoship_workflow::OperationKind;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Published {
    pub number: u64,
    pub url: String,
}

#[async_trait::async_trait]
trait Github {
    async fn call(&mut self, args: &[&str]) -> Result<String>;
}
struct Transport<'a>(&'a Path, &'a workflow::WorkflowGuard);
#[async_trait::async_trait]
impl Github for Transport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<String> {
        if args.starts_with(&["pr", "create"]) {
            self.1.validate_binding(self.1.binding()).await?;
        }
        let (ok, out, err) = run(&gh_bin(), args, self.0).await?;
        anyhow::ensure!(ok, "GitHub publication call failed: {err}");
        Ok(out)
    }
}

async fn find(github: &mut impl Github, branch: &str, head: &str) -> Result<Option<Published>> {
    let mut limit = 100usize;
    let rows: Vec<serde_json::Value> = loop {
        let raw = github
            .call(&[
                "pr",
                "list",
                "--head",
                branch,
                "--state",
                "all",
                "--limit",
                &limit.to_string(),
                "--json",
                "number,url,headRefOid,headRefName,isCrossRepository,state",
            ])
            .await?;
        let page: Vec<serde_json::Value> = serde_json::from_str(&raw)?;
        if page.len() < limit {
            break page;
        }
        anyhow::ensure!(limit < 12800, "publication history exceeds bounded reconciliation; refusing creation from incomplete history");
        limit *= 2;
    };
    let mut found = None;
    for row in rows {
        if row["headRefName"] != branch || row["isCrossRepository"] != false {
            continue;
        }
        let state = row["state"].as_str().context("missing PR state")?;
        anyhow::ensure!(
            state != "CLOSED",
            "prior publication was closed; reconcile that PR before creating another"
        );
        if state == "OPEN" {
            anyhow::ensure!(
                row["headRefOid"] == head,
                "open PR head changed; reconcile before publication"
            );
        } else if state != "MERGED" || row["headRefOid"] != head {
            continue;
        }
        let number = row["number"].as_u64().context("missing PR number")?;
        let url = row["url"].as_str().context("missing PR URL")?.to_owned();
        anyhow::ensure!(found.is_none(), "ambiguous publication identity");
        found = Some(Published { number, url });
    }
    Ok(found)
}

async fn create_or_reconcile(
    github: &mut impl Github,
    branch: &str,
    head: &str,
    args: &[&str],
) -> Result<Published> {
    if let Some(found) = find(github, branch, head).await? {
        return Ok(found);
    }
    // Neither an error nor a successful CLI response establishes remote state.
    // Always query GitHub after the request, including a lost response.
    let result = github.call(args).await;
    if let Some(found) = find(github, branch, head).await? {
        return Ok(found);
    }
    match result {
        Err(error) => Err(error.context("publication not observed; intent retained for retry")),
        Ok(_) => anyhow::bail!("publication response not confirmed; intent retained for retry"),
    }
}

pub(super) async fn publish(
    guard: &workflow::WorkflowGuard,
    worktree: &Path,
    branch: &str,
    head: &str,
    args: &[&str],
) -> Result<Published> {
    anyhow::ensure!(
        head == guard.binding().head,
        "publication head is not the checkpointed candidate"
    );
    let operation = guard.intent(OperationKind::CreatePr)?;
    guard.validate_binding(guard.binding()).await?;
    let result = if operation.receipt.is_some() {
        // A completed identity can never create another PR, even if it was
        // subsequently closed or its head changed.
        find(&mut Transport(worktree, guard), branch, head)
            .await?
            .context("published workflow needs PR reconciliation; refusing duplicate creation")?
    } else {
        create_or_reconcile(&mut Transport(worktree, guard), branch, head, args).await?
    };
    guard.observe_remote(
        &operation,
        &format!("github-pr:{}", result.number),
        Some(result.number),
    )?;
    Ok(result)
}

pub(super) async fn merged_exact(root: &Path, number: u64, head: &str) -> bool {
    let Ok((ok, raw, _)) = run(
        &gh_bin(),
        &[
            "pr",
            "view",
            &number.to_string(),
            "--json",
            "state,headRefOid",
        ],
        root,
    )
    .await
    else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return false;
    };
    ok && value["state"] == "MERGED" && value["headRefOid"] == head
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    struct Fake {
        replies: VecDeque<Result<String>>,
        creates: usize,
    }
    #[async_trait::async_trait]
    impl Github for Fake {
        async fn call(&mut self, args: &[&str]) -> Result<String> {
            if args.starts_with(&["pr", "create"]) {
                self.creates += 1;
            }
            self.replies.pop_front().expect("unexpected GitHub call")
        }
    }
    fn row(head: &str) -> String {
        serde_json::json!([{"number":12,"url":"https://github.com/example/repo/pull/12","headRefOid":head,"headRefName":"agent-fix/issue-1","isCrossRepository":false,"state":"OPEN"}]).to_string()
    }
    #[tokio::test]
    async fn lost_create_response_recovers_exactly_one_pr() {
        let mut gh = Fake {
            replies: VecDeque::from([
                Ok("[]".into()),
                Err(anyhow::anyhow!("lost response")),
                Ok(row("abc")),
                Ok(row("abc")),
            ]),
            creates: 0,
        };
        let first = create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
            .await
            .unwrap();
        let second = create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(gh.creates, 1);
    }
    #[tokio::test]
    async fn unknown_state_and_changed_head_never_create_another_pr() {
        for response in [
            Err(anyhow::anyhow!("API unavailable")),
            Ok(row("other-head")),
        ] {
            let mut gh = Fake {
                replies: VecDeque::from([response]),
                creates: 0,
            };
            assert!(
                create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
                    .await
                    .is_err()
            );
            assert_eq!(gh.creates, 0);
        }
    }
    #[tokio::test]
    async fn closed_publication_is_not_recreated_after_lost_receipt() {
        let closed = row("abc").replace("OPEN", "CLOSED");
        let mut gh = Fake {
            replies: VecDeque::from([Ok(closed)]),
            creates: 0,
        };
        assert!(
            create_or_reconcile(&mut gh, "agent-fix/issue-1", "abc", &["pr", "create"])
                .await
                .is_err()
        );
        assert_eq!(gh.creates, 0);
    }
}
