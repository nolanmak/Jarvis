//! #2394: interrupted-edit recovery inside an interrupted merge with main.
use super::*;

fn git(root: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
        .args(args).current_dir(root).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}
fn write(root: &Path, file: &str, text: &str) {
    std::fs::write(root.join(file), text).unwrap();
}

/// Production shape (#2350 / PR #2358, 2026-11-21 04:13Z): the preserved
/// resume worktree holds an interrupted `git merge` of main (MERGE_HEAD, 73
/// staged paths including main's Cargo.lock). Main's incoming changes are not
/// the interrupted editor's edits; only what differs from MERGE_HEAD is.
#[tokio::test]
async fn interrupted_main_merge_charges_only_non_main_edits_to_the_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "candidate"]);
    write(root, "Cargo.lock", "v1\n");
    write(root, "conflict.rs", "base\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    git(root, &["checkout", "-q", "-b", "main"]);
    write(root, "Cargo.lock", "v2\n");
    write(root, "conflict.rs", "main\n");
    git(root, &["commit", "-qam", "main"]);
    let main = git(root, &["rev-parse", "HEAD"]);
    git(root, &["update-ref", "refs/remotes/origin/main", &main]);
    git(root, &["checkout", "-q", "candidate"]);
    write(root, "conflict.rs", "candidate\n");
    git(root, &["commit", "-qam", "candidate"]);
    git(root, &["merge", "--no-edit", "main"]);
    assert!(root.join(".git/MERGE_HEAD").exists(), "fixture must stop mid-merge");
    write(root, "conflict.rs", "resolved\n");
    git(root, &["add", "-A"]);
    let diff = recovered_revision_diff(root).await.unwrap();
    assert_eq!(blast_radius_hit_in_diff(&diff), None, "main's Cargo.lock is not a recovered edit");
    assert!(diff.contains("conflict.rs"), "the conflict resolution is still reviewed: {diff}");
    // An edit to a guarded path beyond what main brought stays guarded.
    write(root, "Cargo.lock", "v3\n");
    git(root, &["add", "-A"]);
    let diff = recovered_revision_diff(root).await.unwrap();
    assert!(blast_radius_hit_in_diff(&diff).is_some(), "editor change to a guarded path: {diff}");
    // A MERGE_HEAD that main does not contain grants nothing.
    write(root, "Cargo.lock", "v2\n");
    git(root, &["add", "-A"]);
    git(root, &["update-ref", "refs/remotes/origin/main", &base]);
    let diff = recovered_revision_diff(root).await.unwrap();
    assert!(blast_radius_hit_in_diff(&diff).is_some(), "unverified merge source: {diff}");
}
