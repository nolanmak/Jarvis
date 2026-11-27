//! #2458: finish an approved PR whose only blocker is that main moved.
//!
//! The merge sweep runs beside the worker, never in its single slot. When a
//! loop-owned, approved PR has green required checks but GitHub reports it
//! BEHIND, the sweep asks GitHub to merge main in (`update-branch`, merge
//! method, pinned to the approved head), waits for checks on the new head,
//! and merges that head once it is green. The approval carries only to a
//! commit that is provably GitHub's clean merge of main into the approved
//! head: see `verify_main_merge` and `autoship_main_update.rs`.
use super::*;

/// GitHub's `mergeStateStatus`, as far as finalizing needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MergeState {
    /// Strict protection: the head must contain current main first.
    Behind,
    /// Anything else, including absent, defers to the existing merge path.
    Other,
}

pub(super) fn merge_state(row: &serde_json::Value) -> MergeState {
    match row.get("mergeStateStatus").and_then(serde_json::Value::as_str) {
        Some("BEHIND") => MergeState::Behind,
        _ => MergeState::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Required checks are not green on this head yet: look again next tick.
    AwaitChecks,
    /// Green but behind: merge main in, then wait for checks again.
    UpdateBranch,
    /// Green and current: ready the PR and merge it.
    Merge,
}

pub(super) fn step(checks_green: Option<bool>, state: MergeState) -> Step {
    match (checks_green, state) {
        (Some(true), MergeState::Behind) => Step::UpdateBranch,
        (Some(true), MergeState::Other) => Step::Merge,
        _ => Step::AwaitChecks,
    }
}

/// #2583: nightly review mode only. A draft whose required checks went red
/// while it sat BEHIND main may be red only because the main it merged was
/// red; main may since have been fixed. Merging current main in is the one
/// step that can heal it, and it never merges the PR itself: checks rerun on
/// the update and a later tick merges only if they come up green. Without
/// this, a red-main episode stranded every draft that pulled it in.
pub(super) fn heal_by_main_update(nightly: bool, checks_green: Option<bool>, state: MergeState) -> bool {
    nightly && checks_green == Some(false) && state == MergeState::Behind
}

/// A main update the sweep may finish for this PR: the approval recorded for
/// `origin_head` reaches the current PR head through it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Carry {
    pub origin_head: String,
    pub reviewed_head: String,
    pub main: String,
    /// GitHub has merged main in, but the approval is not carried yet.
    pub pending: bool,
}

/// Read-only: the durable update (if any) that connects the loop's recorded
/// approval at `origin_head` to the PR head GitHub reports now.
pub(super) fn carried(path: &Path, repository: &str, pr: u64, origin_head: &str, pr_head: &str) -> Option<Carry> {
    let store = crate::autoship_workflow::WorkflowStore::open_read_only(path).ok()??;
    let id = store.resolve_pr(repository, pr).ok()??;
    let update = store.main_update(&id).ok()??;
    if update.origin_head != origin_head {
        return None;
    }
    let pending = match update.updated_head.as_deref() {
        // Carried already: only the verified merge commit is reached.
        Some(updated) if updated == pr_head => false,
        Some(_) => return None,
        // Requested: the head GitHub reports now must still be verified.
        None if update.reviewed_head != pr_head => true,
        None => return None,
    };
    Some(Carry { origin_head: update.origin_head, reviewed_head: update.reviewed_head, main: update.main, pending })
}

fn commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `updated` is exactly GitHub's clean merge of `main` into `reviewed`: its
/// parents are `[reviewed, main]` in that order and its tree is the tree a
/// clean merge of the two produces. Anything else carries nothing.
pub(super) async fn verify_main_merge(repo: &Path, reviewed: &str, updated: &str, main: &str) -> Result<()> {
    anyhow::ensure!([reviewed, updated, main].iter().all(|id| commit_id(id)), "main merge needs commit identities");
    let (ok, parents, _) = run("git", &["--no-optional-locks", "rev-list", "--parents", "-n", "1", updated], repo).await?;
    anyhow::ensure!(ok, "merge commit {updated} is not available locally");
    let parents: Vec<String> = parents.split_whitespace().map(str::to_ascii_lowercase).collect();
    let expected: Vec<String> = [updated, reviewed, main].iter().map(|id| id.to_ascii_lowercase()).collect();
    anyhow::ensure!(parents == expected, "{updated} is not a merge of main {main} into the approved head {reviewed}");
    // merge-tree exits 0 only for a clean merge and prints the tree first.
    let (clean, tree, _) = run("git", &["--no-optional-locks", "merge-tree", "--write-tree", "--no-messages", reviewed, main], repo).await?;
    let tree = tree.lines().next().unwrap_or_default().trim().to_ascii_lowercase();
    anyhow::ensure!(clean && commit_id(&tree), "main does not merge cleanly into the approved head");
    let (ok, actual, _) = run("git", &["--no-optional-locks", "rev-parse", "--verify", &format!("{updated}^{{tree}}")], repo).await?;
    anyhow::ensure!(ok && actual.trim().eq_ignore_ascii_case(&tree), "merge commit {updated} changes more than merging main");
    Ok(())
}

/// Ask GitHub to merge main into the PR with a merge commit, refusing if the
/// PR head is no longer the approved head.
pub(super) async fn update_branch(root: &Path, repository: &str, pr: u64, expected_head: &str) -> Result<()> {
    update_branch_with(&gh_bin(), root, repository, pr, expected_head).await
}

pub(super) async fn update_branch_with(gh: &str, root: &Path, repository: &str, pr: u64, expected_head: &str) -> Result<()> {
    anyhow::ensure!(commit_id(expected_head) && pr > 0 && repository.split('/').count() == 2, "invalid update-branch target");
    let endpoint = format!("repos/{repository}/pulls/{pr}/update-branch");
    let pin = format!("expected_head_sha={expected_head}");
    let (ok, _, error) = run(gh, &["api", "--method", "PUT", &endpoint, "-f", &pin, "-f", "update_method=merge"], root).await?;
    anyhow::ensure!(ok, "update-branch refused: {}", truncate(&error, 300));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git").args(["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
            "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"]).args(args).current_dir(root).output().unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    /// main at `main`, the approved candidate at `reviewed`, both from `base`.
    fn repository() -> (tempfile::TempDir, String, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("shared.txt"), "one\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        let base = git(root, &["rev-parse", "HEAD"]);
        git(root, &["checkout", "-qb", "candidate"]);
        std::fs::write(root.join("candidate.txt"), "approved change\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "approved change"]);
        let reviewed = git(root, &["rev-parse", "HEAD"]);
        git(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("main.txt"), "another PR merged\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "main moved"]);
        let main = git(root, &["rev-parse", "HEAD"]);
        git(root, &["checkout", "-q", "--detach", &reviewed]);
        let _ = base;
        (dir, base, reviewed, main)
    }
    /// What GitHub's update-branch (merge method) produces.
    fn github_update(root: &Path, reviewed: &str, main: &str) -> String {
        git(root, &["checkout", "-q", "--detach", reviewed]);
        git(root, &["merge", "-q", "--no-ff", "--no-edit", main]);
        git(root, &["rev-parse", "HEAD"])
    }

    #[test]
    fn a_green_behind_pr_updates_then_waits_then_merges() {
        let row = |state: &str| serde_json::json!({"number": 2449, "mergeStateStatus": state});
        // PR #2449 at 07:45Z: approved head, every required check green, BEHIND.
        assert_eq!(step(Some(true), merge_state(&row("BEHIND"))), Step::UpdateBranch);
        // #2583: only a red, behind draft in nightly mode is healed by an update.
        assert!(heal_by_main_update(true, Some(false), merge_state(&row("BEHIND"))));
        assert!(!heal_by_main_update(false, Some(false), merge_state(&row("BEHIND"))), "pre-merge keeps waiting");
        assert!(!heal_by_main_update(true, None, merge_state(&row("BEHIND"))), "pending checks are not red");
        assert!(!heal_by_main_update(true, Some(false), merge_state(&row("CLEAN"))), "current with main: red is the PR's own");
        assert!(!heal_by_main_update(true, Some(true), merge_state(&row("BEHIND"))), "green already updates");
        // After update-branch, checks rerun on GitHub's merge commit.
        assert_eq!(step(Some(false), merge_state(&row("BLOCKED"))), Step::AwaitChecks);
        assert_eq!(step(None, merge_state(&row("BEHIND"))), Step::AwaitChecks, "unknown checks never update or merge");
        // Green on the merge commit: merge it.
        for state in ["CLEAN", "UNSTABLE", "HAS_HOOKS", "BLOCKED", "DRAFT"] {
            assert_eq!(step(Some(true), merge_state(&row(state))), Step::Merge, "{state}");
        }
        // A legacy inventory without the field keeps the existing merge path.
        assert_eq!(step(Some(true), merge_state(&serde_json::json!({"number": 1}))), Step::Merge);
    }

    #[tokio::test]
    async fn only_githubs_clean_merge_of_main_into_the_approved_head_carries() {
        let (dir, base, reviewed, main) = repository();
        let root = dir.path();
        let updated = github_update(root, &reviewed, &main);
        verify_main_merge(root, &reviewed, &updated, &main).await.unwrap();
        // Parents in the other order: main with the candidate merged in.
        git(root, &["checkout", "-q", "--detach", &main]);
        git(root, &["merge", "-q", "--no-ff", "--no-edit", &reviewed]);
        let swapped = git(root, &["rev-parse", "HEAD"]);
        assert!(verify_main_merge(root, &reviewed, &swapped, &main).await.is_err(), "parents must be [approved, main]");
        // The right parents, plus an unreviewed change folded into the merge.
        git(root, &["checkout", "-q", "--detach", &reviewed]);
        git(root, &["merge", "-q", "--no-ff", "--no-commit", &main]);
        std::fs::write(root.join("candidate.txt"), "unreviewed change\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "Merge main"]);
        let smuggled = git(root, &["rev-parse", "HEAD"]);
        assert!(verify_main_merge(root, &reviewed, &smuggled, &main).await.is_err(), "the tree must be the clean merge");
        // Merging something other than the recorded main.
        assert!(verify_main_merge(root, &reviewed, &updated, &base).await.is_err(), "another main was recorded");
        // A plain new commit on the approved head is not a main merge.
        git(root, &["checkout", "-q", "--detach", &reviewed]);
        std::fs::write(root.join("candidate.txt"), "follow-up\n").unwrap();
        git(root, &["commit", "-qam", "follow-up"]);
        let follow_up = git(root, &["rev-parse", "HEAD"]);
        assert!(verify_main_merge(root, &reviewed, &follow_up, &main).await.is_err());
        assert!(verify_main_merge(root, &reviewed, &"f".repeat(40), &main).await.is_err(), "unknown objects carry nothing");
    }

    #[tokio::test]
    async fn update_branch_merges_main_with_a_merge_commit_pinned_to_the_approved_head() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let gh = dir.path().join("gh");
        std::fs::write(&gh, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\necho '{{\"message\":\"Updating pull request branch.\"}}'\n",
            calls.display())).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let head = "a".repeat(40);
        update_branch_with(gh.to_str().unwrap(), dir.path(), "owner/repo", 2449, &head).await.unwrap();
        let args = std::fs::read_to_string(&calls).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args, ["api", "--method", "PUT", "repos/owner/repo/pulls/2449/update-branch",
            "-f", &format!("expected_head_sha={head}"), "-f", "update_method=merge"]);
        // GitHub refusing (head moved, conflict) is an error, never a success.
        std::fs::write(&gh, "#!/bin/sh\necho 'HTTP 422: expected head sha didn'\"'\"'t match current head ref' >&2\nexit 1\n").unwrap();
        assert!(update_branch_with(gh.to_str().unwrap(), dir.path(), "owner/repo", 2449, &head).await.is_err());
        assert!(update_branch_with(gh.to_str().unwrap(), dir.path(), "owner/repo", 2449, "not-a-head").await.is_err());
    }

    /// The whole light path for PR #2449's shape against the real durable
    /// store and real git, while another workflow holds the worker slot.
    #[tokio::test]
    async fn green_satisfied_and_behind_updates_then_merges_without_the_worker_slot() {
        use crate::autoship_workflow::{Binding, NextAction, OperationKind, Review, Stage, WorkflowStore};
        let (dir, base, reviewed, main) = repository();
        let root = dir.path();
        let path = root.join("workflow.db");
        let id = "owner/repo#2439";
        let approved = Binding { head: reviewed.clone(), contract: "contract".into(), evidence_hash: "evidence".into() };
        let mut store = WorkflowStore::open(&path).unwrap();
        store.register(id, "agent-fix/issue-2439", approved.clone(), 1).unwrap();
        store.register("owner/repo#2329", "agent-fix/issue-2329", Binding { head: base.clone(), ..approved.clone() }, 1).unwrap();
        let lease = store.claim(id, "publisher", 2, 1_000).unwrap().unwrap();
        store.attach_pr_in_repo(&lease, "owner/repo", 2449, 3).unwrap();
        store.record_review(&lease, &Review { reviewer: "publication-review".into(), binding: approved.clone(), approved: true,
            evidence: serde_json::json!({"structured":{"binding":{"head":reviewed,"base":base}}}).to_string() }, 4).unwrap();
        let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
        cp.stage = Stage::AwaitChecks;
        cp.next = NextAction { reason: "ci_pending".into(), action: "reconcile_checks".into(), eligible_at_ms: 0 };
        store.checkpoint(&lease, &cp, 5).unwrap();
        store.release(&lease, 6).unwrap();
        // #2329's repair holds the only productive slot for an hour.
        assert!(store.claim_productive("owner/repo#2329", "worker", 7, 3_600_000).unwrap().is_some());

        // Tick 1: green and BEHIND. The sweep records the request, then GitHub merges main in.
        assert_eq!(step(Some(true), MergeState::Behind), Step::UpdateBranch);
        let sweep = store.claim_reconciliation(id, "sweep:1", 10, 60_000).unwrap().expect("the sweep needs no worker slot");
        store.record_main_update_request(&sweep, &approved, &main, &base, 11).unwrap();
        store.release(&sweep, 12).unwrap();
        let updated = github_update(root, &reviewed, &main);
        let carry = carried(&path, "owner/repo", 2449, &reviewed, &updated).expect("the recorded approval reaches the merge commit");
        assert_eq!(carry, Carry { origin_head: reviewed.clone(), reviewed_head: reviewed.clone(), main: main.clone(), pending: true });
        assert_eq!(carried(&path, "owner/repo", 2449, &reviewed, &"e".repeat(40)).map(|c| c.pending), Some(true),
            "an unverified head only reaches the verification step");
        assert_eq!(carried(&path, "owner/repo", 2449, &base, &updated), None, "another recorded approval");
        assert_eq!(carried(&path, "owner/repo", 7, &reviewed, &updated), None, "another PR");

        // Tick 2: checks still running on the merge commit.
        assert_eq!(step(Some(false), MergeState::Other), Step::AwaitChecks);
        // Tick 3: green. Verify, carry, and bind the merge intent to the merge commit.
        assert_eq!(step(Some(true), MergeState::Other), Step::Merge);
        verify_main_merge(root, &reviewed, &updated, &main).await.unwrap();
        let sweep = store.claim_reconciliation(id, "sweep:3", 20, 60_000).unwrap().unwrap();
        let carried_binding = store.carry_main_update(&sweep, &approved, &updated, &main, 21).unwrap();
        let intent = store.begin_operation(&sweep, OperationKind::MergePr, &carried_binding, 22).unwrap();
        assert_eq!(intent.binding.head, updated, "the merge is pinned to GitHub's merge commit");
        assert!(store.begin_operation(&sweep, OperationKind::MergePr, &approved, 23).is_err(),
            "the superseded head can no longer be merged");
        assert_eq!(carried(&path, "owner/repo", 2449, &reviewed, &updated),
            Some(Carry { origin_head: reviewed.clone(), reviewed_head: reviewed.clone(), main: main.clone(), pending: false }));
        store.release(&sweep, 24).unwrap();
    }

    /// #2458 (PR #2449 at 08:14Z): re-integration of an approved PR drops its
    /// approval and re-enters acceptance preparation, which parked #2449 in
    /// scope. A clean, approved PR that is only BEHIND stays with the sweep.
    #[test]
    fn integration_leaves_a_clean_behind_approved_pr_to_the_sweep() {
        use crate::autoship_workflow::{Binding, Checkpoint, NextAction, Stage};
        let mut cp = Checkpoint { stage: Stage::AwaitChecks,
            binding: Binding { head: "h".into(), contract: "c".into(), evidence_hash: "e".into() },
            evidence: vec![], next: NextAction { reason: "ci_pending".into(), action: "reconcile_checks".into(), eligible_at_ms: 0 } };
        let row = |mergeable: &str, state: &str| serde_json::json!({"mergeable": mergeable, "mergeStateStatus": state});
        assert!(super::super::integration::sweep_finalizes(&cp, &row("MERGEABLE", "BEHIND")));
        assert!(!super::super::integration::sweep_finalizes(&cp, &row("CONFLICTING", "DIRTY")), "conflicts still integrate");
        assert!(!super::super::integration::sweep_finalizes(&cp, &row("MERGEABLE", "BLOCKED")), "red or pending checks still integrate");
        cp.stage = Stage::Reconcile;
        assert!(!super::super::integration::sweep_finalizes(&cp, &row("MERGEABLE", "BEHIND")), "unapproved work keeps the existing path");
    }

    /// The sweep wiring: carried approvals resolve before any gate reads
    /// approval, and a BEHIND update keeps every guard the merge path has,
    /// including the routing check at mutation time (#2447).
    #[test]
    fn the_sweep_finalizes_behind_prs_through_the_guarded_light_path() {
        let src = include_str!("self_improve.rs");
        let start = src.find("async fn merge_sweep_selected(").expect("the sweep");
        let sweep = &src[start..start + src[start..].find("\n}\n").expect("end")];
        let at = |needle: &str| sweep.find(needle).unwrap_or_else(|| panic!("the sweep lacks {needle}"));
        assert!(at("finalize::carried(") < at("sweep_local_skip("), "carried approval feeds the local gates");
        let update = at("finalize::update_branch(");
        assert!(at("SweepGuard::acquire(") < at("validate_for_main_update("));
        assert!(at("validate_for_main_update(") < at("request_main_update(") && at("request_main_update(") < update,
            "the request is durable before GitHub is asked");
        let routing = sweep[..update].rfind("manual_routing::allowed(").expect("routing check before update");
        assert!(routing > at("validate_for_main_update("), "routing is re-read at mutation time");
        assert!(at("carry_main_update(") < at("validate_remote("), "the carry precedes ready and merge");
        for forbidden in ["claim_productive", "reasoner"] {
            assert!(!sweep.contains(forbidden), "finalizing must not need the worker slot: {forbidden}");
        }
    }
}
