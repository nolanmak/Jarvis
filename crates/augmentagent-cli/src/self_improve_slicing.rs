//! Restart-safe issue slicing. The admitted plan is immutable; remote child
//! identity is reconciled before creation and partial plans never retire a parent.
use super::*;
use rusqlite::OptionalExtension;

#[async_trait::async_trait]
trait Github {
    async fn call(&mut self, args: &[&str]) -> Result<String>;
}
struct Transport<'a>(&'a Path, &'a workflow::WorkflowGuard);
#[async_trait::async_trait]
impl Github for Transport<'_> {
    async fn call(&mut self, args: &[&str]) -> Result<String> {
        if args.starts_with(&["issue", "create"]) || args.starts_with(&["issue", "edit"]) {
            self.1.validate_before_side_effect()?;
        }
        let (ok, out, error) = run(&gh_bin(), args, self.0).await?;
        anyhow::ensure!(ok, "slice GitHub call failed: {}", truncate(&error, 300));
        Ok(out)
    }
}

fn admitted_plan(
    path: &Path,
    repository: &str,
    issue: &Issue,
    plan: &[Slice],
) -> Result<Vec<Slice>> {
    let _ = crate::autoship_workflow::WorkflowStore::open(path)?;
    let mut conn = rusqlite::Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS slice_plan(repository TEXT NOT NULL,parent INTEGER NOT NULL,source TEXT NOT NULL,plan TEXT NOT NULL,PRIMARY KEY(repository,parent));")?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let prior: Option<String> = tx
        .query_row(
            "SELECT plan FROM slice_plan WHERE repository=?1 AND parent=?2",
            rusqlite::params![repository, issue.number],
            |r| r.get(0),
        )
        .optional()?;
    let result = if let Some(prior) = prior {
        serde_json::from_str(&prior)?
    } else {
        anyhow::ensure!(
            (MIN_SLICES..=MAX_SLICES).contains(&plan.len()),
            "invalid slice count"
        );
        for slice in plan {
            augmentagent_channel_core::public_report::validate(&slice.title, &slice.body)?;
        }
        tx.execute(
            "INSERT INTO slice_plan VALUES (?1,?2,?3,?4)",
            rusqlite::params![
                repository,
                issue.number,
                crate::autoship_review::digest(
                    format!("{}\n{}", issue.title, issue.body).as_bytes()
                ),
                serde_json::to_string(plan)?
            ],
        )?;
        plan.to_vec()
    };
    tx.commit()?;
    Ok(result)
}

async fn inventory(github: &mut impl Github) -> Result<Vec<serde_json::Value>> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for page in 1..=MAX_ISSUE_PAGES {
        let raw=github.call(&["api",&format!("repos/{{owner}}/{{repo}}/issues?state=all&per_page=100&sort=created&direction=asc&page={page}")]).await?;
        let rows: Vec<serde_json::Value> =
            serde_json::from_str(&raw).context("slice issue inventory incomplete")?;
        if rows.is_empty() {
            return Ok(out);
        }
        for row in rows {
            let number = row["number"]
                .as_u64()
                .context("slice inventory has missing issue identity")?;
            if row.get("pull_request").is_none() && seen.insert(number) {
                out.push(row);
            }
        }
    }
    anyhow::bail!("slice issue inventory exceeded safe pagination bound")
}
fn existing(
    rows: &[serde_json::Value],
    parent: u64,
    index: usize,
    total: usize,
) -> Result<Option<u64>> {
    let marker = format!("{SLICE_MARKER_PREFIX}#{parent} index={index}/{total} -->");
    let matches: Vec<_> = rows
        .iter()
        .filter(|r| {
            matches!(
                r["author_association"].as_str(),
                Some("OWNER" | "MEMBER" | "COLLABORATOR")
            )
        })
        .filter(|r| {
            r["body"]
                .as_str()
                .is_some_and(|body| body.contains(&marker))
        })
        .filter_map(|r| r["number"].as_u64())
        .collect();
    anyhow::ensure!(
        matches.len() <= 1,
        "ambiguous existing child for parent #{parent} slice {index}"
    );
    Ok(matches.first().copied())
}
async fn reconcile(
    github: &mut impl Github,
    issue: &Issue,
    plan: &[Slice],
    labels: &str,
) -> Result<Vec<u64>> {
    let mut rows = inventory(github).await?;
    let mut children = Vec::new();
    for (index, slice) in plan.iter().enumerate() {
        let index = index + 1;
        if let Some(number) = existing(&rows, issue.number, index, plan.len())? {
            children.push(number);
            continue;
        }
        let body = slice_issue_body(
            issue.number,
            index,
            plan.len(),
            &slice.body,
            issue.research_filed,
        );
        augmentagent_channel_core::public_report::validate(&slice.title, &body)?;
        let result = github
            .call(&[
                "issue",
                "create",
                "--title",
                &slice.title,
                "--body",
                &body,
                "--label",
                labels,
            ])
            .await;
        // Query even after a lost response: a transport error is not proof that
        // GitHub did not create the child. Closed children remain discoverable.
        rows = inventory(github).await?;
        let found = existing(&rows, issue.number, index, plan.len())?;
        match found {
            Some(number) => children.push(number),
            None => {
                return Err(result.err().unwrap_or_else(|| {
                    anyhow::anyhow!(
                        "slice creation was not remotely observed; retry reconciliation"
                    )
                }))
            }
        }
    }
    anyhow::ensure!(
        children.len() == plan.len(),
        "partial slice plan cannot retire parent"
    );
    github
        .call(&[
            "issue",
            "edit",
            &issue.number.to_string(),
            "--add-label",
            SLICED_LABEL,
        ])
        .await?;
    Ok(children)
}

pub(super) async fn file(
    repo: &Path,
    issue: &Issue,
    plan: &[Slice],
    guard: &workflow::WorkflowGuard,
) -> Result<Vec<u64>> {
    guard.validate_before_side_effect()?;
    let repository = workflow::canonical_repository(repo).await?;
    let plan = admitted_plan(&workflow::path(), &repository, issue, plan)?;
    ensure_label(
        repo,
        FIXABLE_LABEL,
        "0E8A16",
        "Eligible for the auto-PR pipeline",
    )
    .await;
    ensure_label(
        repo,
        SLICED_LABEL,
        "BFD4F2",
        "Parent decomposed into admitted child issues",
    )
    .await;
    let mut labels = vec![FIXABLE_LABEL];
    if issue.research_filed {
        ensure_label(
            repo,
            RESEARCH_LABEL,
            "D4C5F9",
            "Filed by the research pipeline",
        )
        .await;
        labels.push(RESEARCH_LABEL);
    }
    let children = reconcile(&mut Transport(repo, guard), issue, &plan, &labels.join(",")).await?;
    // Status comment is advisory. Losing it never loses child identity/plan.
    let list = children
        .iter()
        .zip(&plan)
        .map(|(n, s)| format!("- #{n} — {}", s.title))
        .collect::<Vec<_>>()
        .join("\n");
    let _=backoff_comment(repo,issue.number,&format!("Auto-fix triage: all {} admitted slices exist. Parent is labelled `{SLICED_LABEL}`.\n\n{list}",children.len())).await;
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fake {
        rows: Vec<serde_json::Value>,
        creates: usize,
        labelled: bool,
        fail_at: Option<usize>,
        lose_response: bool,
    }
    #[async_trait::async_trait]
    impl Github for Fake {
        async fn call(&mut self, args: &[&str]) -> Result<String> {
            if args[0] == "api" {
                return Ok(if args[1].ends_with("page=1") {
                    serde_json::to_string(&self.rows)?
                } else {
                    "[]".into()
                });
            }
            if args[1] == "edit" {
                self.labelled = true;
                return Ok(String::new());
            }
            self.creates += 1;
            if self.fail_at == Some(self.creates) {
                anyhow::bail!("transient create failure");
            }
            let value = |flag: &str| args[args.iter().position(|a| *a == flag).unwrap() + 1];
            self.rows.push(serde_json::json!({"number":100+self.creates,"title":value("--title"),"body":value("--body"),"state":"closed","author_association":"OWNER"}));
            if self.lose_response {
                anyhow::bail!("response lost after creation");
            }
            Ok(format!(
                "https://github.com/fixture/project/issues/{}",
                100 + self.creates
            ))
        }
    }
    fn issue() -> Issue {
        Issue {
            number: 7,
            title: "Synthetic epic".into(),
            body: "Separate two independent changes".into(),
            author: "fixture".into(),
            author_trusted: true,
            research_filed: false,
        }
    }
    fn plan() -> Vec<Slice> {
        vec![
            Slice {
                title: "First synthetic change".into(),
                body: "Implement the first checkable behavior.".into(),
            },
            Slice {
                title: "Second synthetic change".into(),
                body: "Implement the second checkable behavior.".into(),
            },
        ]
    }
    #[tokio::test]
    async fn partial_plan_retries_only_missing_child_and_lost_response_recovers() {
        let mut github = Fake {
            rows: vec![],
            creates: 0,
            labelled: false,
            fail_at: Some(2),
            lose_response: true,
        };
        assert!(reconcile(&mut github, &issue(), &plan(), FIXABLE_LABEL)
            .await
            .is_err());
        assert_eq!(github.rows.len(), 1);
        assert!(!github.labelled);
        github.fail_at = None;
        let children = reconcile(&mut github, &issue(), &plan(), FIXABLE_LABEL)
            .await
            .unwrap();
        assert_eq!(children, vec![101, 103]);
        assert!(github.labelled);
        assert_eq!(github.creates, 3);
        assert_eq!(
            reconcile(&mut github, &issue(), &plan(), FIXABLE_LABEL)
                .await
                .unwrap(),
            children
        );
        assert_eq!(
            github.creates, 3,
            "restart must not recreate closed or open children"
        );
    }
    #[test]
    fn untrusted_marker_cannot_replace_an_admitted_child() {
        let body = slice_issue_body(7, 1, 2, "Synthetic child", false);
        let rows = vec![serde_json::json!({"number":90,"body":body,"author_association":"NONE"})];
        assert_eq!(existing(&rows, 7, 1, 2).unwrap(), None);
    }

    #[test]
    fn admitted_plan_survives_restart_and_replanning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workflow.db");
        let original = plan();
        assert_eq!(
            admitted_plan(&path, "fixture/repo", &issue(), &original).unwrap(),
            original
        );
        let mut changed = plan();
        changed[0].title = "Different planner wording".into();
        assert_eq!(
            admitted_plan(&path, "fixture/repo", &issue(), &changed).unwrap(),
            original
        );
    }
}
