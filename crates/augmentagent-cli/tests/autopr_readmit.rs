//! #1214 C5 — `augmentagent autopr readmit --reason <code> --dry-run` lists
//! exactly the labelled issues whose recorded reason matches, and changes
//! nothing on GitHub. The `gh` it runs is a script that logs every call.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn readmit_dry_run_lists_matching_issues_and_mutates_nothing() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
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
        {'number': 1082, 'comments': [{'body': 'unrelated discussion'}]},
        {'number': 921, 'comments': []},
        {'number': 1143, 'comments': [{'body': '<!-- self-improve-gave-up reason=scoper:not-fixable -->\nAuto-PR gave up.'}]},
        {'number': 1071, 'comments': []},
    ]))
else:
    sys.stderr.write('unexpected gh call: ' + json.dumps(args) + '\n')
    sys.exit(1)
"#,
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let rec = |kind: &str| {
        serde_json::json!({"at": 1, "kind": kind, "stage": "gate", "detail": "", "diffstat": "",
            "diff_lines": 0, "wall_secs": 0})
    };
    let history = serde_json::json!({"issues": {
        "1082": [rec("gate-red")],
        "921": [rec("review-reject"), rec("gate-red")],
        "1071": [rec("review-reject")],
    }});
    let history_path = dir.join("history.json");
    std::fs::write(&history_path, history.to_string()).unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(dir)
        .args(["autopr", "readmit", "--reason", "gate-red", "--dry-run"])
        .env("GH_BIN", &gh)
        .env("JARVIS_1214_ROOT", dir)
        .env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("AUGMENTAGENT_AUTOPR_HISTORY_FILE", &history_path)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
    let mut listed: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    listed.sort();
    assert_eq!(listed, vec!["1082", "921"], "exactly the gate-red issues: {stdout}");
    let calls = std::fs::read_to_string(dir.join("gh-calls.jsonl")).unwrap();
    for mutating in ["\"edit\"", "\"comment\"", "\"close\"", "--remove-label", "--add-label"] {
        assert!(!calls.contains(mutating), "dry run made a mutating call: {calls}");
    }
}
