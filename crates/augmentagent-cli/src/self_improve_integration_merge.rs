//! One local integration operation. Publication, CI, review and the final merge
//! remain the responsibility of the existing resumed-workflow pipeline.
use super::*;

/// Preserve the existing harness accounting distinction from semantic or Git
/// failures, even though both return through the same integration boundary.
#[derive(Debug)]
pub(super) struct ResolverFailure;
impl std::fmt::Display for ResolverFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("integration resolver failed")
    }
}
impl std::error::Error for ResolverFailure {}

/// Resolve the ref once so an advancing tracking ref cannot change the tree
/// during a paid resolution. The resolver runs only for a proven Git conflict.
pub(super) async fn integrate<F, Fut>(
    worktree: &Path,
    guard: &workflow::WorkflowGuard,
    target: &str,
    resolve: F,
) -> Result<Option<String>>
where
    F: FnOnce(Vec<String>, String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    guard.validate_before_side_effect()?;
    let (ok, base, error) = run(
        "git",
        &["rev-parse", "--verify", &format!("{target}^{{commit}}")],
        worktree,
    )
    .await?;
    anyhow::ensure!(ok, "integration base unavailable: {error}");
    let base = base.trim();
    let (ok, starting_head, _) = run("git", &["rev-parse", "HEAD"], worktree).await?;
    anyhow::ensure!(ok, "integration head unavailable");
    let git_name =
        std::env::var("AUGMENTAGENT_GIT_AUTHOR_NAME").unwrap_or_else(|_| "AugmentAgent".into());
    let git_email = std::env::var("AUGMENTAGENT_GIT_AUTHOR_EMAIL")
        .unwrap_or_else(|_| "augmentagent@localhost".into());
    let name = format!("user.name={git_name}");
    let email = format!("user.email={git_email}");
    guard.validate_before_side_effect()?;
    let (merged, _, error) = run(
        "git",
        &["-c", &name, "-c", &email, "merge", "--no-edit", base],
        worktree,
    )
    .await?;
    if merged {
        return Ok(None);
    }
    let (ok, status, _) = run("git", &["status", "--porcelain"], worktree).await?;
    anyhow::ensure!(ok, "cannot inspect integration conflict");
    let files = conflicted_files(&status);
    // A failed hook, dirty index or unavailable object is not a conflict and
    // cannot authorize the builder to edit arbitrary files.
    anyhow::ensure!(
        !files.is_empty(),
        "integration failed without a conflict: {error}"
    );
    if let ConflictPlan::Refuse(why) = conflict_plan(&files) {
        bail!("{why}");
    }
    let (_, hunks, _) = run("git", &["diff"], worktree).await?;
    guard.validate_before_side_effect()?;
    let summary = resolve(files, hunks).await.context(ResolverFailure)?;
    guard.validate_before_side_effect()?;
    let (ok, actual, _) = run("git", &["rev-parse", "HEAD"], worktree).await?;
    anyhow::ensure!(
        ok && actual.trim() == starting_head.trim(),
        "integration head changed during resolution"
    );
    let _ = drop_root_scratch(worktree).await;
    let (ok, _, error) = run("git", &["add", "-A"], worktree).await?;
    anyhow::ensure!(ok, "cannot stage integration: {error}");
    let diff = recovered_revision_diff(worktree).await?;
    anyhow::ensure!(
        blast_radius_hit_in_diff(&diff).is_none(),
        "integration resolution touches a guarded path"
    );
    let (ok, names, _) = run("git", &["diff", "--cached", "--name-only"], worktree).await?;
    anyhow::ensure!(ok, "cannot inspect staged integration");
    for file in names.lines().filter(|s| !s.is_empty()) {
        if let Ok(contents) = tokio::fs::read_to_string(worktree.join(file)).await {
            anyhow::ensure!(
                !has_conflict_markers(&contents),
                "conflict markers remain in {file}"
            );
        }
    }
    let (ok, status, _) = run("git", &["status", "--porcelain"], worktree).await?;
    anyhow::ensure!(
        ok && conflicted_files(&status).is_empty(),
        "integration remains unmerged"
    );
    guard.validate_before_side_effect()?;
    let (ok, _, error) = run(
        "git",
        &[
            "-c",
            &name,
            "-c",
            &email,
            "commit",
            "--allow-empty",
            "-m",
            "autoship: integrate current base",
        ],
        worktree,
    )
    .await?;
    anyhow::ensure!(ok, "cannot commit integration: {error}");
    let (ok, _, _) = run(
        "git",
        &["merge-base", "--is-ancestor", base, "HEAD"],
        worktree,
    )
    .await?;
    anyhow::ensure!(ok, "integration commit does not include selected base");
    Ok(Some(summary))
}
