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
    assert_eq!(resumable_from(&prs, &ledger, 1, &gave_up, None, None, 0),
        Some((3, 3, "agent-fix/issue-3".into())));
    ledger.mark(1, 3);
    assert_eq!(resumable_from(&prs, &ledger, 1, &gave_up, None, None, 0), None);
}

/// #1532 — the starvation axis: with more eligible drafts than the daily cap,
/// a persisted cursor that is NOT reset by the UTC-day rollover must give
/// every draft a turn before any draft gets a second one. On `main` the
/// lowest-numbered drafts were re-served every single day and 102-104 never
/// ran at all.
#[test]
fn fair_rotation_covers_backlog_larger_than_daily_cap_across_days() {
    let prs = serde_json::Value::Array(
        (100..105u64)
            .map(|pr| serde_json::json!({
                "number": pr,
                "headRefName": format!("agent-fix/issue-{}", pr - 90),
                "isDraft": true,
                "isCrossRepository": false,
            }))
            .collect(),
    );
    let gave_up = std::collections::HashSet::new();
    let mut fair = FairSched::default();
    let mut order = Vec::new();
    for day in 1..=3u64 {
        // A fresh ledger per simulated day: the within-day exclusion resets,
        // the rotation does not.
        let mut ledger = AttemptLedger::default();
        for _ in 0..2 {
            let (pr, issue, _) =
                resumable_from(&prs, &ledger, day, &gave_up, None, None, fair.resume_cursor)
                    .expect("a draft is always eligible");
            fair.advance(pr);
            ledger.mark(day, issue);
            order.push(pr);
        }
    }
    assert_eq!(order, vec![100, 101, 102, 103, 104, 100],
        "every draft gets a turn before any draft gets a second one");
}

/// #1532 — the other starvation axis, and the fallbacks. Three simulated days
/// of three billed slots each against five non-converging drafts and two new
/// issues: every draft is resumed and BOTH new issues are built, none of
/// which happens when sitting drafts outrank new work absolutely.
#[test]
fn a_combined_lane_day_spends_slots_on_both_drafts_and_new_issues() {
    let prs = serde_json::Value::Array(
        (100..105u64)
            .map(|pr| serde_json::json!({
                "number": pr,
                "headRefName": format!("agent-fix/issue-{}", pr - 90),
                "isDraft": true,
                "isCrossRepository": false,
            }))
            .collect(),
    );
    let gave_up = std::collections::HashSet::new();
    const CAP: u32 = 3;
    const PER_BUILD: u32 = 2;
    let mut fair = FairSched::default();
    let mut pickable = vec![20u64, 21];
    let (mut resumed, mut built) = (Vec::new(), Vec::new());
    // `run_once`'s order of operations, as one selection both the day loop and
    // the fallback cases below drive: the preference decides which kind is
    // tried first, the other kind is the fallback.
    type Slot = Option<Result<(u64, u64, String), u64>>;
    let select = |fair: &FairSched, draft: Option<(u64, u64, String)>, pick: Option<u64>| {
        let prefer_build = prefer_build_now(Lane::Combined, true, fair, PER_BUILD);
        let slot: Slot = if prefer_build {
            pick.map(Err).or_else(|| draft.map(Ok))
        } else {
            draft.map(Ok).or_else(|| pick.map(Err))
        };
        (prefer_build, slot)
    };
    let mut reserved_slots_spent_on_drafts = 0;
    for day in 1..=3u64 {
        let mut ledger = AttemptLedger::default();
        let mut billed = 0;
        while billed < CAP {
            let draft = resumable_from(&prs, &ledger, day, &gave_up, None, None, fair.resume_cursor);
            let pick = pickable.iter().copied().find(|i| !ledger.attempted_today(day, *i));
            let (prefer_build, slot) = select(&fair, draft, pick);
            match slot {
                Some(Ok((pr, issue, _))) => {
                    fair.advance(pr);
                    ledger.mark(day, issue);
                    resumed.push(pr);
                    fair.record_billed(true);
                    reserved_slots_spent_on_drafts += u32::from(prefer_build);
                }
                Some(Err(issue)) => {
                    ledger.mark(day, issue);
                    built.push(issue);
                    pickable.retain(|i| *i != issue);
                    fair.record_billed(false);
                }
                None => break,
            }
            billed += 1;
        }
        assert!(billed <= CAP, "day {day} must not exceed the shared daily budget");
    }
    assert_eq!(built, vec![20, 21], "new issues are not starved by the draft backlog");
    let mut distinct = resumed.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct, vec![100, 101, 102, 103, 104], "{resumed:?}");
    assert!(reserved_slots_spent_on_drafts > 0, "a reserved slot with no issue left resumed");

    // Fallback, explicitly, through that same selection. A slot that prefers a
    // resume but has no eligible draft left buys a build...
    let mut spent = AttemptLedger::default();
    for issue in 10..15u64 {
        spent.mark(4, issue);
    }
    let fresh = FairSched::default();
    let draft = resumable_from(&prs, &spent, 4, &gave_up, None, None, fresh.resume_cursor);
    assert_eq!(draft, None, "every draft was already attempted today");
    assert_eq!(select(&fresh, draft, Some(22)), (false, Some(Err(22))));

    // ...and a slot reserved for a build with nothing to build resumes,
    // rather than idling the tick away.
    let owing = FairSched { resume_cursor: 0, resumes_since_build: PER_BUILD };
    let draft = resumable_from(&prs, &AttemptLedger::default(), 5, &gave_up, None, None, 0);
    let (prefer_build, slot) = select(&owing, draft, None);
    assert!(prefer_build);
    assert_eq!(slot.map(|s| s.map(|(pr, _, _)| pr)), Some(Ok(100)));
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
import json, os, pathlib, sys, time
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
    print(json.dumps([dict(name='rust',state=s['checks']),dict(name='CodeRabbit',state='FAILURE')])); sys.exit(1)
elif a[0]=='api': print('[]')
elif a[:2]==['pr','edit']: time.sleep(30)
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
    let started = std::time::Instant::now();
    assert!(!write_optional_pr_note(&root, "20", "Optional reviewer observation").await);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    assert_eq!(merge_sweep(&root, false).await, 1);
    assert_eq!(std::fs::read_to_string(root.join("merged")).unwrap(), head);
}

#[test]
fn coderabbit_never_controls_ci_or_internal_review_approval() {
    for state in ["PENDING", "FAILURE", "ERROR", "SUCCESS"] {
        let rows = serde_json::json!([
            {"name":"rust", "state":"SUCCESS"},
            {"name":"CodeRabbit", "state":state}
        ]);
        assert_eq!(ci_checks_green(&rows.to_string()), Some(true));
    }
    assert_eq!(ci_checks_green(r#"[{"name":"CodeRabbit","state":"SUCCESS"}]"#), None);
    assert_eq!(ci_checks_green(r#"[{"name":"rust","state":"PENDING"}]"#), Some(false));
    assert_eq!(ci_checks_green(r#"[{"name":"rust","state":"FAILURE"}]"#), Some(false));
    assert_eq!(ci_checks_green("{}"), None);
    assert_eq!(ci_checks_green("[]"), None);
    assert!(failed_ci_checks(r#"[{"name":"CodeRabbit","bucket":"fail"}]"#).is_empty());
    assert!(resume_may_merge(true, true));
    assert!(!resume_may_merge(false, true));
    assert!(!resume_may_merge(true, false));
    let src = include_str!("self_improve.rs");
    let production = &src[..src.find("mod tests {").unwrap()];
    for removed in ["wait_for_rabbit(", "rabbit_blocks", "@coderabbitai review", "AUTOPR_REQUIRE_CODERABBIT"] {
        assert!(!production.contains(removed), "retired dependency: {removed}");
    }
}
