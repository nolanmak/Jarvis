//! Non-publishing transport observation. Never an operation or merge grant.
use super::*;

pub(super) async fn probe(root: &Path, repository: &str) -> Result<()> {
    let (ok, raw, _) = run("git", &["remote", "get-url", "--all", "origin"], root).await?;
    anyhow::ensure!(
        ok && raw.len() <= 16384,
        "publish source identity unavailable"
    );
    let origins = raw.lines().collect::<Vec<_>>();
    let [origin] = origins.as_slice() else {
        anyhow::bail!("publish source identity ambiguous")
    };
    let policy = repository_identity::Policy::from_env()?;
    anyhow::ensure!(
        policy.origin(origin)?.as_deref() == Some(policy.canonical(repository)?.as_str()),
        "publish source alias differs from protected repository"
    );
    let destination = push_destination::Resolved::resolve(root, repository).await?;
    let hooks = tempfile::tempdir()?;
    let hook = destination.hook(root, hooks.path()).await?;
    // Use the same exact transport fence and original pre-push hook as actual
    // publication, but never update a remote ref or grant a workflow operation.
    let reference = format!(
        "HEAD:refs/heads/augmentagent-readiness-{}",
        uuid::Uuid::new_v4()
    );
    let (ok, _, _) = run(
        "git",
        &[
            "-c",
            hook.config(),
            "-c",
            "push.followTags=false",
            "push",
            "--dry-run",
            "--porcelain",
            "origin",
            &reference,
        ],
        root,
    )
    .await?;
    anyhow::ensure!(ok, "publish dry-run admission refused");
    Ok(())
}
pub(super) async fn inspect(root: &Path) -> serde_json::Value {
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        let repository = workflow::canonical_repository(root).await?;
        probe(root, &repository).await
    })
    .await;
    let status = match result {
        Ok(Ok(())) => "publish-ready",
        Ok(Err(_)) => "publish-smoke-refused",
        Err(_) => "publish-smoke-timeout",
    };
    serde_json::json!({"status":status,"permits_attempt":status=="publish-ready","live_verified":false})
}
