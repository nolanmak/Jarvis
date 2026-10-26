//! #1214 C5 — `augmentagent autopr readmit --reason <code> --dry-run` lists
//! exactly the labelled issues whose recorded reason matches, and changes
//! nothing on GitHub. The `gh` it runs is a script that logs every call.
//!
//! #1517 — and `--reset-history` takes the label off, comments the reset
//! marker, and drops exactly the matched issues from the local attempt
//! history. The `gh` it runs is a script that logs every call.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const RESET_MARKER: &str = "<!-- self-improve-attempts-reset -->";

/// A `gh` that logs every call and serves the #1214 labelled-issue fixture:
/// #1082 and #921 gave up on `gate-red`, #1143 on a scoper verdict, #1071 on
/// `review-reject`. Mutating calls succeed silently.
fn fake_gh(dir: &Path) -> std::path::PathBuf {
    let gh = dir.join("fake-gh.py");
    std::fs::write(
        &gh,
        r#"#!/usr/bin/env python3
import json, os, sys
args = sys.argv[1:]
with open(os.path.join(os.environ['JARVIS_1214_ROOT'], 'gh-calls.jsonl'), 'a') as log:
    log.write(json.dumps(args) + '\n')
if args[:2] == ['issue', 'list']:
    print(json.dumps([
        {'number': 1082, 'labels': [{'name':'agent-gave-up'}], 'comments': [{'body': 'unrelated discussion'}]},
        {'number': 921, 'labels': [{'name':'agent-gave-up'}], 'comments': []},
        {'number': 1143, 'labels': [{'name':'agent-gave-up'}], 'comments': [{'body': '<!-- self-improve-gave-up reason=scoper:not-fixable -->\nAuto-PR gave up.'}]},
        {'number': 1071, 'labels': [{'name':'agent-gave-up'}], 'comments': []},
    ]))
elif args and args[0] == 'api':
    assert '--paginate' not in args and '--slurp' not in args
    page=int(args[-1].split('&page=')[1])
    issue=int(args[-1].split('/issues/')[1].split('/')[0])
    bodies={1082:['unrelated discussion'],1143:['<!-- self-improve-gave-up reason=scoper:not-fixable -->\\nAuto-PR gave up.']}.get(issue,[])
    if issue==1082 and os.path.exists(os.path.join(os.environ['JARVIS_1214_ROOT'],'late-comments')):
        bodies=['<!-- self-improve-gave-up reason=gate-red -->'] + ['discussion']*99
        bodies+=['<!-- self-improve-attempts-reset -->','<!-- self-improve-gave-up reason=review-reject -->']
    rows=[dict(id=i+1,body=b) for i,b in enumerate(bodies)]
    print(json.dumps(rows[(page-1)*100:page*100]))
elif args[:2] == ['issue', 'edit'] or args[:2] == ['issue', 'comment']:
    pass
else:
    sys.stderr.write('unexpected gh call: ' + json.dumps(args) + '\n')
    sys.exit(1)
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    gh
}

/// The #1214 history fixture: #1082 gave up on `gate-red`, #921 on
/// `gate-red` after a `review-reject`, #1071 on `review-reject` alone.
fn fake_history(path: &Path) {
    let rec = |kind: &str| {
        // Every field of the record, so a rewrite that preserves the
        // unmatched entries is byte-identical rather than default-filled.
        serde_json::json!({"at": 1, "kind": kind, "stage": "gate", "detail": "", "diffstat": "",
            "diff_lines": 0, "wall_secs": 0, "providers": "", "reasoner_calls": 0})
    };
    let history = serde_json::json!({"issues": {
        "1082": [rec("gate-red")],
        "921": [rec("review-reject"), rec("gate-red")],
        "1071": [rec("review-reject")],
    }});
    std::fs::write(path, history.to_string()).unwrap();
}

fn readmit(dir: &Path, gh: &Path, history: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    cmd.current_dir(dir)
        .args(["autopr", "readmit", "--reason", "gate-red"])
        .args(extra)
        .env("GH_BIN", gh)
        .env("JARVIS_1214_ROOT", dir)
        .env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("AUGMENTAGENT_AUTOPR_HISTORY_FILE", history);
    cmd.output().unwrap()
}

#[test]
fn readmit_dry_run_lists_matching_issues_and_mutates_nothing() {
    for extra in [vec!["--dry-run"], vec!["--dry-run", "--reset-history"]] {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        let gh = fake_gh(dir);
        let history_path = dir.join("history.json");
        fake_history(&history_path);
        let before = std::fs::read(&history_path).unwrap();

        let out = readmit(dir, &gh, &history_path, &extra);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        let mut listed: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
        listed.sort();
        assert_eq!(listed, vec!["1082", "921"], "exactly the gate-red issues: {stdout}");
        let calls = std::fs::read_to_string(dir.join("gh-calls.jsonl")).unwrap();
        for mutating in ["\"edit\"", "\"comment\"", "\"close\"", "--remove-label", "--add-label"] {
            assert!(!calls.contains(mutating), "dry run {extra:?} made a mutating call: {calls}");
        }
        assert_eq!(std::fs::read(&history_path).unwrap(), before, "dry run {extra:?} rewrote the history");
    }
}

/// #1517 — a real `--reset-history` run unlabels the matched issues, posts
/// the reset marker on each (so the marker-derived attempt count starts
/// over), and drops only their records from the history file.
#[test]
fn readmit_reset_history_unlabels_comments_and_clears_only_the_matched_records() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    let gh = fake_gh(dir);
    let history_path = dir.join("history.json");
    fake_history(&history_path);
    let before: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();

    let out = readmit(dir, &gh, &history_path, &["--reset-history"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    let mut listed: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    listed.sort();
    assert_eq!(listed, vec!["readmitted #1082", "readmitted #921"], "{stdout}");

    let calls: Vec<Vec<String>> = std::fs::read_to_string(dir.join("gh-calls.jsonl"))
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    for issue in ["1082", "921"] {
        let unlabel = calls.iter().position(|c| {
            *c == vec![
                "issue".to_string(),
                "edit".into(),
                issue.into(),
                "--remove-label".into(),
                "agent-gave-up".into(),
            ]
        });
        let comment = calls.iter().position(|c| {
            c.starts_with(&["issue".into(), "comment".into(), issue.into()])
                && c.last().is_some_and(|b| b.contains(RESET_MARKER))
        });
        let unlabel = unlabel.unwrap_or_else(|| panic!("#{issue} was not unlabelled: {calls:?}"));
        let comment =
            comment.unwrap_or_else(|| panic!("#{issue} got no reset-marker comment: {calls:?}"));
        assert!(
            comment < unlabel,
            "#{issue}: the reset marker must land before the label comes off, or a comment \
             failure would readmit the issue with its old charged markers: {calls:?}"
        );
    }
    for untouched in ["1143", "1071"] {
        assert!(
            !calls.iter().any(|c| c.len() > 2 && c[1] != "list" && c[2] == untouched),
            "#{untouched} should not have been touched: {calls:?}"
        );
    }

    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&history_path).unwrap()).unwrap();
    assert_eq!(
        after["issues"].as_object().unwrap().keys().collect::<Vec<_>>(),
        vec!["1071"],
        "only the unmatched issue's records survive: {after}"
    );
    assert_eq!(
        after["issues"]["1071"], before["issues"]["1071"],
        "the unmatched issue's records must survive the rewrite unchanged, field for field"
    );
    assert!(dir.join("history.json.bak").exists(), "the atomic save keeps a .bak");
}

#[test]
fn readmit_uses_changed_reason_after_comment_one_hundred_without_mutation() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    let gh = fake_gh(dir);
    let history = dir.join("history.json");
    fake_history(&history);
    std::fs::write(dir.join("late-comments"),"yes").unwrap();
    let out = readmit(dir,&gh,&history,&["--dry-run"]);
    assert!(out.status.success(),"{}",String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(),"921");
    let calls = std::fs::read_to_string(dir.join("gh-calls.jsonl")).unwrap();
    assert!(calls.contains("comments?per_page=100&page=2"));
    assert!(!calls.contains("--paginate") && !calls.contains("--slurp"));
    assert!(!calls.contains("--remove-label") && !calls.contains("\"comment\""));
}
