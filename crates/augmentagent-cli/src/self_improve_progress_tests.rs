//! Recovery regressions use a local scripted GitHub transport, never live PRs.
use super::*;
use std::os::unix::fs::PermissionsExt;

#[test]
fn different_review_findings_do_not_mean_the_same_failure() {
    let mut rec = AttemptRecord::unspecified();
    rec.kind = FailureKind::ReviewReject;
    rec.stage = "resume:review".into();
    rec.detail = "CODEX-REVIEW: changes-requested\nPersist the child issue links".into();
    let original = fingerprint(&rec);
    rec.detail = "CODEX-REVIEW: changes-requested\n  Persist the child issue links  ".into();
    assert_eq!(original, fingerprint(&rec));
    rec.detail = "CODEX-REVIEW: changes-requested\nRetry a failed parent comment".into();
    let revised = fingerprint(&rec);
    assert_ne!(original, revised);
    assert_eq!(give_up_verdict(&[Some(original)], &revised, rec.kind, 2), None);
    assert_eq!(give_up_verdict(&[Some(revised.clone())], &revised, rec.kind, 20), None,
        "a bounded revision cycle parks work for retry rather than closing it");
}

#[test]
fn ci_repair_uses_failed_checks_only_and_bounds_work() {
    let checks = serde_json::json!([
        {"name":"passing","bucket":"pass"},
        {"name":"running","bucket":"pending"},
        {"name":"cancelled","bucket":"cancel"},
        {"name":"audit","bucket":"fail","link":"https://github.com/fixture/project/actions/runs/1/job/42"},
        {"name":"external","bucket":"fail","link":"https://example.invalid/status"}
    ]);
    assert_eq!(failed_ci_checks(&checks.to_string()), vec![("audit".into(), Some(42)), ("external".into(), None)]);
    assert!(failed_ci_checks("not JSON").is_empty());
    assert!(failed_ci_checks("{}").is_empty());
    let many = vec![serde_json::json!({"name":"failed","bucket":"fail"}); 30];
    assert_eq!(failed_ci_checks(&serde_json::to_string(&many).unwrap()).len(), 5);
}

#[test]
fn publication_records_precede_draft_returns_and_never_cover_post_review_edits() {
    let src = include_str!("self_improve.rs");
    let fresh = &src[src.find("pub async fn run_once(").unwrap()..];
    let published = &fresh[fresh.find("let pr_url = stdout.trim()").unwrap()..];
    let record = published.find("record_opened_pr(").unwrap();
    assert!(record < published.find("if let Some(reason) = &park").unwrap());
    assert!(record < published.find("if !automerge").unwrap());
    assert!(published[..record].contains("qa_ok && park.is_none() && !repaired_after_review"));
    assert!(published[record..record+130].contains("published_head"));
    let resume = &src[src.find("async fn resume_draft_pr(").unwrap()..src.find("async fn reclaim_worktree(").unwrap()];
    assert!(resume.find("record_opened_pr(").unwrap() < resume.find("if checks_green(").unwrap());
    assert!(resume.contains("\"--match-head-commit\", reviewed_head"));
}

#[test]
fn ready_managed_prs_are_resumable_but_forks_and_unmanaged_prs_are_not() {
    let prs = serde_json::json!([
        {"number":1,"headRefName":"agent-fix/issue-1","isDraft":false,"isCrossRepository":true},
        {"number":2,"headRefName":"feature/human","isDraft":false,"isCrossRepository":false},
        {"number":3,"headRefName":"agent-fix/issue-3","isDraft":false,"isCrossRepository":false}
    ]);
    let mut ledger = AttemptLedger::default();
    let gave_up = std::collections::HashSet::new();
    assert_eq!(resumable_from(&prs, &ledger, 1, &gave_up, None, None),
        Some((3, 3, "agent-fix/issue-3".into())));
    ledger.mark(1, 3);
    assert_eq!(resumable_from(&prs, &ledger, 1, &gave_up, None, None), None);
}

#[test]
fn resumed_reviews_recover_latest_findings_without_minting_approval() {
    let old = "Auto-resume: no double LGTM after 3 rounds. Latest findings:\n\nFix missing links".into();
    assert_eq!(parse_independent_findings(&[old]), Some("Fix missing links".into()));
    let comments = vec![independent_findings_comment("Earlier finding"),
        "Unrelated comment".into(), independent_findings_comment("Latest finding")];
    assert_eq!(parse_independent_findings(&comments), Some("Latest finding".into()));
    assert_eq!(parse_independent_findings(&["CODEX-REVIEW: lgtm".into()]), None);
    assert!(!recorded_approval(None, 3, Some("head")).reviewed);
}

#[tokio::test]
async fn approved_ready_pr_survives_pending_checks_failed_merge_and_head_change() {
    const CHILD: &str = "AUTOSHIP_PROGRESS_FIXTURE";
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let gh = root.join("gh.py");
        std::fs::write(&gh, r#"#!/usr/bin/env python3
import json, os, pathlib, sys
root=pathlib.Path(os.environ['AUTOSHIP_PROGRESS_FIXTURE'])
a=sys.argv[1:]
with (root/'calls').open('a') as f: f.write(json.dumps(a)+'\n')
s=json.loads((root/'scenario').read_text())
if a[:2]==['pr','list']:
    assert 'is:draft' not in a
    print(json.dumps([dict(number=20,headRefName='agent-fix/issue-10',headRefOid=s['head'],isDraft=s['draft'],isCrossRepository=False,mergeable='MERGEABLE',body='Automated self-improvement for #10.\n- complexity (scoping pass): medium')]))
elif a[:2]==['issue','view']: print(json.dumps(dict(body='Fix synthetic addition',author=dict(login='fixture-owner'))))
elif a[:2]==['pr','diff']: print('src/synthetic.rs')
elif a[:2]==['pr','checks']:
    print(json.dumps([dict(state=s['checks'])])); sys.exit(0 if s['checks']=='SUCCESS' else 8)
elif a[0]=='api': print('[]')
elif a[:2]==['pr','ready']:
    assert s['draft'], 'already-ready PR must retain its original state'
elif a[:2]==['pr','merge']:
    assert a[a.index('--match-head-commit')+1]==s['head']
    if s['fail_merge']: sys.exit(1)
    (root/'merged').write_text(s['head'])
else: raise Exception('unexpected call '+repr(a))
"#).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "self_improve::progress_tests::approved_ready_pr_survives_pending_checks_failed_merge_and_head_change", "--nocapture"])
            .env(CHILD, root).env("GH_BIN", gh)
            .env("AUGMENTAGENT_GH_OWNER", "fixture-owner")
            .env("AUGMENTAGENT_AUTOPR_AUTOMERGE", "1")
            .env("AUGMENTAGENT_AUTOPR_OPENED_FILE", root.join("opened.json"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env_remove("DISCORD_WEBHOOK_URL")
            .output().unwrap();
        assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        assert!(root.join("merged").exists(), "child scenario must actually execute");
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    for args in [vec!["init", "-q"], vec!["remote", "add", "origin", "https://example.invalid/fixture-owner/project.git"]] {
        assert!(std::process::Command::new("git").args(args).current_dir(&root).status().unwrap().success());
    }
    let head = "a".repeat(40);
    let scenario = |checks: &str, draft: bool, fail_merge: bool, sha: &str| {
        std::fs::write(root.join("scenario"), serde_json::to_vec(&serde_json::json!({
            "checks":checks,"draft":draft,"fail_merge":fail_merge,"head":sha
        })).unwrap()).unwrap();
    };
    let verdict = || Some(RecordedVerdict {provider:"codex".into(),backend:"native".into(),model:"synthetic".into(),diff_ok:true,system_ok:true});
    // Opening a rejected draft records ownership, but never authorizes merge.
    record_opened_pr(&opened_prs_path(), 10, 20, &head, None);
    scenario("SUCCESS", true, false, &head);
    assert_eq!(merge_sweep(&root, false).await, 0);
    // A resumed LGTM survives a delayed check and a subsequent merge failure.
    record_opened_pr(&opened_prs_path(), 10, 20, &head, verdict());
    scenario("PENDING", false, false, &head);
    assert_eq!(merge_sweep(&root, false).await, 0);
    scenario("SUCCESS", false, true, &head);
    assert_eq!(merge_sweep(&root, false).await, 0);
    scenario("SUCCESS", true, true, &head);
    assert_eq!(merge_sweep(&root, false).await, 0);
    let calls = std::fs::read_to_string(root.join("calls")).unwrap();
    assert!(calls.lines().any(|line| line.contains("--undo")), "failed draft merge restores draft state");
    assert_eq!(ready_for_merge(&root, 20, None).await, None);
    scenario("SUCCESS", false, false, &"b".repeat(40));
    assert_eq!(merge_sweep(&root, false).await, 0);
    scenario("SUCCESS", false, false, &head);
    assert_eq!(merge_sweep(&root, true).await, 0);
    assert!(!root.join("merged").exists());
    assert_eq!(merge_sweep(&root, false).await, 1);
    assert_eq!(std::fs::read_to_string(root.join("merged")).unwrap(), head);
}
