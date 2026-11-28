//! #2610 — a stale red-main issue and a deterministic refusal must not take
//! every tick. Synthetic fixtures only: no providers or production state.
use super::*;
use stale_loop::{Backoff, Decision, StaleCloser};
use std::sync::{Arc, Mutex};

const UNPARSED: &str = "owner acceptance criteria contain an unparsed line; refusing partial requirements";

fn red(sha: &str) -> RedMain {
    RedMain {
        sha: sha.into(),
        failing: vec![],
        packages: vec![],
        gate_err: Some("GitHub Actions on main failed:\nCheck: cli | job 1 | https://example.invalid/job/1\n\
            Failed-step diagnostics unavailable; failure remains confirmed.\n\n".into()),
    }
}

/// The body autoship itself files, shaped exactly like #2590's.
fn red_main_issue_body() -> String {
    red_main_body(&red("c21f171eca47e24bbcf8847b7c1469639cb2c25e"),
        "c21f171e Merge pull request #2589 from owner/fix\n8f40d1ae fix: main is red at 41f746d: required CI checks failed (#2580) (#2581)\n")
}

// ---------------------------------------------------------------------------
// Auto-filed red-main body round-trips as acceptance criteria
// ---------------------------------------------------------------------------

#[test]
fn auto_filed_red_main_body_round_trips_through_the_pr_criteria_section() {
    let issue = Issue { number: 2590, title: "main is red at c21f171: required CI checks failed".into(),
        body: red_main_issue_body(), author: "owner".into(), author_trusted: true, research_filed: false };
    let owner_request = acceptance_owner_request(&issue);
    // #2609's shape: the whole owner request as the single criterion.
    let criteria = vec![owner_request.clone()];
    let body = format!("Automated self-improvement for #2590.\n\n## Summary\ns\n\n## Independent review\n\n{}\n\n\
        ## Verification\n- complexity (scoping pass): hard\n\nDraft — a human must review and merge. Fixes #2590\n\n\
        <!-- This is an auto-generated comment: release notes by coderabbit.ai -->\n## Summary by CodeRabbit\n* x",
        criteria_pr_section(&criteria));
    assert_eq!(required_criteria_from_pr_body(&body).unwrap(), criteria,
        "the multi-line owner request must survive the PR body exactly");

    // The plan lane no longer uses diagnostics as the requirement at all.
    let fallback = plan_fallback_criterion(&issue, &owner_request);
    assert!(!fallback.contains('\n') && fallback.contains("#2590") && fallback.contains("green"), "{fallback}");
    let body = format!("## Summary\ns{}\n\n## Verification\n- gate: passed", criteria_pr_section(std::slice::from_ref(&fallback)));
    assert_eq!(required_criteria_from_pr_body(&body).unwrap(), vec![fallback]);

    // An ordinary issue keeps its owner request verbatim.
    let plain = Issue { body: "Preserve the audit journal.".into(), ..issue };
    assert_eq!(plan_fallback_criterion(&plain, "x\n\ny"), "x\n\ny");
}

#[test]
fn single_line_criteria_keep_their_existing_pr_section_byte_for_byte() {
    let criteria = vec!["C1: plain".to_string(), "C2: `a | b`".to_string()];
    assert_eq!(criteria_pr_section(&criteria),
        "\n\n## Acceptance criteria (from the scoping pass)\n- C1: plain\n- C2: `a | b`");
}

#[test]
fn encoded_criteria_refuse_truncation_instead_of_using_a_prefix() {
    let section = criteria_pr_section(&["a\nb".to_string()]);
    let truncated = section.replace(" -->", "");
    assert!(required_criteria_from_pr_body(&truncated).is_err(), "{truncated}");
    // An encoded list under an earlier, quoted heading is not authoritative.
    let quoted = format!("## Summary{section}\n\n## Acceptance criteria (from the scoping pass)\n- controller criterion");
    assert_eq!(required_criteria_from_pr_body(&quoted).unwrap(), vec!["controller criterion".to_string()]);
}

// ---------------------------------------------------------------------------
// Stale red-main issues are closed, not worked
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeCloser { own: Option<u64>, calls: Vec<String>, fail_pr: bool }

#[async_trait::async_trait]
impl StaleCloser for FakeCloser {
    fn own_pr(&self, _issue: u64) -> Option<u64> { self.own }
    async fn close_pr(&mut self, pr: u64, issue: u64, comment: &str) -> Result<()> {
        assert!(comment.contains("green"), "{comment}");
        self.calls.push(format!("close-pr {pr} for {issue}"));
        anyhow::ensure!(!self.fail_pr, "scripted failure");
        Ok(())
    }
    async fn close_issue(&mut self, issue: u64, comment: &str) -> Result<()> {
        assert!(comment.contains("resolved"), "{comment}");
        self.calls.push(format!("close-issue {issue}"));
        Ok(())
    }
}

#[tokio::test]
async fn red_main_issue_with_green_main_is_closed_with_its_draft_not_worked() {
    let rows = serde_json::json!([
        {"number": 2600, "state": "open", "title": "Ordinary work", "body": "plain issue"},
        {"number": 2590, "state": "open", "title": "main is red at c21f171", "body": red_main_issue_body()},
        {"number": 2609, "state": "open", "title": "fix", "body": red_main_issue_body(), "pull_request": {}},
    ]);
    assert_eq!(stale_loop::open_red_main_issues(&rows), vec![2590], "pull requests and plain issues are not red-main issues");

    let mut closer = FakeCloser { own: Some(2609), ..Default::default() };
    let closed = stale_loop::close_stale_with(&mut closer, &[2590], Some("4fe9cac6abcdef0123")).await;
    assert_eq!(closed, vec![2590]);
    assert_eq!(closer.calls, ["close-pr 2609 for 2590", "close-issue 2590"], "the draft closes before its issue");

    // A draft that cannot be closed keeps its issue open (still skipped).
    let mut closer = FakeCloser { own: Some(2609), fail_pr: true, ..Default::default() };
    assert!(stale_loop::close_stale_with(&mut closer, &[2590], None).await.is_empty());
    assert_eq!(closer.calls, ["close-pr 2609 for 2590"]);

    assert!(stale_loop::stale_red_main(&red_main_issue_body(), true));
    assert!(!stale_loop::stale_red_main(&red_main_issue_body(), false), "a red main still works its issue");
    assert!(!stale_loop::stale_red_main("plain issue", true));
}

#[test]
fn green_main_observation_closes_red_main_before_any_resume_or_pick() {
    let src = include_str!("self_improve.rs");
    let start = src.find("async fn run_selected(").unwrap();
    let body = &src[start..start + src[start..].find("\n}\n").unwrap()];
    let reset = body.find("stale_loop::note_main_green(false)").expect("each run starts unobserved");
    let green = body.find("stale_loop::note_main_green(true)").expect("a green main is recorded");
    let close = body.find("stale_loop::close_stale_red_main(repo_root, dry_run)").expect("stale red-main issues are closed");
    let resume = body.find("find_resumable_draft(repo_root, None").expect("resume path");
    let pick = body.find("select_issue_with_fallback(").expect("picker");
    assert!(reset < green && green < close && close < resume && close < pick);
    let picker = &src[src.find("async fn pick_issue(").unwrap()..];
    let picker = &picker[..picker.find("\n}\n").unwrap()];
    assert!(picker.contains("stale_loop::skip_as_stale_red_main(&iss.body)"));
    assert!(picker.contains("stale_loop::holds(iss.number"));
}

// ---------------------------------------------------------------------------
// Deterministic refusals back off, then park
// ---------------------------------------------------------------------------

#[test]
fn deterministic_refusal_backs_off_growing_and_parks_after_n() {
    let mut state = Backoff::default();
    let t0 = 1_800_000_000_000_i64;
    let minute = 60_000;
    assert_eq!(state.record(2590, &format!("{UNPARSED}"), t0), Decision::BackOff { until_ms: t0 + 30 * minute, count: 1 });
    assert!(state.holds(2590, t0 + 40_000), "the next tick (~40 s later) must not select it again");
    assert!(!state.holds(2600, t0 + 40_000), "peers are unaffected");
    assert!(!state.holds(2590, t0 + 30 * minute));
    let t1 = t0 + 31 * minute;
    assert_eq!(state.record(2590, "recovered edits touch a guarded path", t1), Decision::BackOff { until_ms: t1 + 60 * minute, count: 2 });
    let t2 = t1 + 61 * minute;
    match state.record(2590, UNPARSED, t2) {
        Decision::Park { count, reason } => { assert_eq!(count, stale_loop::PARK_AFTER); assert!(reason.contains("unparsed line")); }
        other => panic!("not parked after {} refusals: {other:?}", stale_loop::PARK_AFTER),
    }
    assert!(state.clear(2590) && !state.holds(2590, t2));
}

#[test]
fn an_unknown_failure_backs_off_only_once_it_repeats_unchanged() {
    let mut state = Backoff::default();
    let t0 = 1_800_000_000_000_i64;
    assert_eq!(state.record(7, "gh api issues failed: HTTP 502 at 12:00:01", t0), Decision::Retry);
    assert!(!state.holds(7, t0 + 1));
    assert_eq!(state.record(7, "gh api issues failed: HTTP 502 at 12:00:41", t0 + 40_000),
        Decision::BackOff { until_ms: t0 + 40_000 + 30 * 60_000, count: 1 }, "the same refusal on the next tick is deterministic");
    assert_eq!(state.record(7, "a different failure", t0 + 50 * 60_000), Decision::Retry);
    // A refusal a day old no longer counts toward parking.
    let mut state = Backoff::default();
    state.record(8, UNPARSED, t0);
    state.record(8, UNPARSED, t0 + 31 * 60_000);
    assert_eq!(state.record(8, UNPARSED, t0 + 26 * 3_600_000), Decision::BackOff { until_ms: t0 + 26 * 3_600_000 + 30 * 60_000, count: 1 });
}

#[test]
fn backoff_state_survives_a_restart_and_corruption_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("backoff.json");
    stale_loop::update(&path, |state| state.record(5, UNPARSED, 1_000)).unwrap();
    assert!(stale_loop::load(&path).holds(5, 2_000));
    std::fs::write(&path, b"{not json").unwrap();
    assert!(!stale_loop::load(&path).holds(5, 2_000));
}

// ---------------------------------------------------------------------------
// End to end, in a child process (globals and env stay isolated)
// ---------------------------------------------------------------------------

const CHILD: &str = "AUTOSHIP_STALE_LOOP_ROOT";

fn git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Re-run `test` in a child with a fake `gh` listing issues #3 (newer) and
/// #2. Returns `None` in the parent (after asserting the child passed).
async fn in_child(test: &str, case: &str) -> Option<(PathBuf, String)> {
    if let Some(root) = std::env::var_os(CHILD) {
        return Some((PathBuf::from(root), std::env::var("AUTOSHIP_STALE_CASE").unwrap()));
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["remote", "add", "origin", "https://github.com/fixture-owner/repo.git"]);
    std::fs::write(repo.join("fixture.txt"), "synthetic input\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]);
    let head = String::from_utf8(std::process::Command::new("git").args(["rev-parse", "HEAD"]).current_dir(&repo)
        .output().unwrap().stdout).unwrap().trim().to_string();
    let gh = tmp.path().join("gh.py");
    augmentagent_test_support::write_executable(&gh, r#"#!/usr/bin/python3
import json,os,sys
args=sys.argv[1:]
open(os.environ['AUTOSHIP_STALE_GH_LOG'],'a').write(json.dumps(args)+'\n')
red='<!-- red-main -->\nAuto-filed by the auto-PR loop: main at abc fails.'
if args[:2]==['repo','view']:print('fixture-owner/repo')
elif args[:2]==['pr','list']:print('[]')
elif args[:1]==['api'] and '/git/ref/heads/main' in args[1]:print(os.environ['AUTOSHIP_STALE_HEAD'])
elif args[:2] in (['issue','comment'],['issue','edit']):pass
elif args[:1]==['api'] and args[1].startswith('rate_limit'):print('4000')
elif args[:1]==['api'] and args[1].endswith('/issues/2590'):print(json.dumps(dict(number=2590,state='open',title='Synthetic',body='',labels=[],user={'login':'fixture-owner'},author_association='OWNER')))
elif args[:1]==['api'] and '/issues?' in args[1]:
 def issue(n):return dict(number=n,state='open',title='Synthetic %d'%n,body=red if (n==3 and os.environ['AUTOSHIP_STALE_CASE'].startswith('red-main')) else 'Synthetic requirement %d.'%n,labels=[],user={'login':'fixture-owner'},author_association='OWNER')
 print(json.dumps([issue(3),issue(2)] if args[1].endswith('page=1') else []))
else:raise AssertionError(args)
"#).unwrap();
    let out = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"]).env_clear()
        .env("PATH", "/usr/bin:/bin").env("HOME", tmp.path()).env("XDG_STATE_HOME", tmp.path().join("state"))
        .env("GH_BIN", &gh).env("AUGMENTAGENT_GH_OWNER", "fixture-owner")
        .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "fixture-owner")
        .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", tmp.path().join("workflow.db"))
        .env("AUGMENTAGENT_AUTOPR_REFUSAL_BACKOFF_FILE", tmp.path().join("refusal-backoff.json"))
        .env("AUTOSHIP_STALE_GH_LOG", tmp.path().join("gh.log"))
        .env("AUTOSHIP_STALE_HEAD", head).env("AUTOSHIP_STALE_CASE", case).env(CHILD, &repo).current_dir(&repo).output().await.unwrap();
    assert!(out.status.success(), "{case}: {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    None
}

async fn picked_in_child(test: &str, case: &str) {
    let Some((repo, case)) = in_child(test, case).await else { return };
    crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    match case.as_str() {
        "refusal" => {
            // Issue #3 refused on the tick just before this one.
            stale_loop::update(&stale_loop::path(), |state| state.record(3, UNPARSED, now - 40_000)).unwrap();
        }
        "red-main-green" => stale_loop::note_main_green(true),
        _ => stale_loop::note_main_green(false),
    }
    let picked = pick_issue(&repo, true).await.unwrap().unwrap();
    let expected = if matches!(case.as_str(), "refusal" | "red-main-green") { 2 } else { 3 };
    assert_eq!(picked.number, expected, "{case}: wrong issue selected");
}

#[tokio::test]
async fn picker_baseline_selects_newest_issue() {
    picked_in_child("self_improve::stale_loop_tests::picker_baseline_selects_newest_issue", "baseline").await;
}

#[tokio::test]
async fn refusing_issue_is_not_reselected_on_the_next_tick() {
    picked_in_child("self_improve::stale_loop_tests::refusing_issue_is_not_reselected_on_the_next_tick", "refusal").await;
}

#[tokio::test]
async fn picker_never_selects_red_main_issue_while_main_is_green() {
    picked_in_child("self_improve::stale_loop_tests::picker_never_selects_red_main_issue_while_main_is_green", "red-main-green").await;
}

#[tokio::test]
async fn picker_still_selects_red_main_issue_while_main_is_red() {
    picked_in_child("self_improve::stale_loop_tests::picker_still_selects_red_main_issue_while_main_is_red", "red-main-red").await;
}

/// The worker loop itself: three ticks that refuse identically back the
/// issue off on the first and park it on the third. Real time: the park's
/// `gh` calls run as subprocesses, which paused time would time out.
#[tokio::test]
async fn worker_backs_off_refusing_ticks_and_parks_after_n() {
    const TEST: &str = "self_improve::stale_loop_tests::worker_backs_off_refusing_ticks_and_parks_after_n";
    let Some((repo, _)) = in_child(TEST, "worker").await else { return };
    crate::autoship_workflow::WorkflowStore::open(&workflow::path()).unwrap();
    let state = tempfile::tempdir_in(repo.parent().unwrap().canonicalize().unwrap()).unwrap();
    let base = chrono::DateTime::parse_from_rfc3339("2026-11-28T00:04:00Z").unwrap().with_timezone(&chrono::Utc);
    let start = tokio::time::Instant::now();
    let mut worker = AutoPrLoop::for_tests(repo.clone(), 5, state.path());
    worker.interval = std::time::Duration::from_millis(20);
    worker.clock = Arc::new(move || base + chrono::Duration::from_std(start.elapsed()).unwrap());
    let backoff = worker.refusal_path.clone();
    let seen = Arc::new(Mutex::new(Vec::<stale_loop::Entry>::new()));
    let observed = (seen.clone(), backoff.clone());
    let stop = tokio_util::sync::CancellationToken::new();
    let done = stop.clone();
    let run = worker.run_builds(stop, move || {
        let (seen, backoff) = observed.clone();
        let done = done.clone();
        async move {
            let mut seen = seen.lock().unwrap();
            if let Some(entry) = stale_loop::load(&backoff).entry(2590) { seen.push(entry.clone()); }
            if seen.len() == stale_loop::PARK_AFTER as usize { done.cancel(); }
            note_current_issue(2590);
            Err(anyhow::anyhow!(UNPARSED))
        }
    });
    tokio::select! {
        result = run => result.unwrap(),
        _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => panic!("the worker did not reach a third refusal"),
    }
    let entry = stale_loop::load(&backoff).entry(2590).cloned().expect("refusals recorded");
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].count, 1, "the first refusal backs off at once");
    assert_eq!(seen[0].until_ms - seen[0].last_at_ms, 30 * 60_000);
    assert!(entry.count >= stale_loop::PARK_AFTER, "{entry:?}");
    let log = std::fs::read_to_string(std::env::var("AUTOSHIP_STALE_GH_LOG").unwrap()).unwrap_or_default();
    assert!(log.lines().any(|line| line.contains("--add-label") && line.contains(GAVE_UP_LABEL))
        && log.lines().any(|line| line.contains(stale_loop::GIVE_UP_REPEATED_REFUSAL)),
        "the third refusal parks the issue for its owner:\n{log}");
}
