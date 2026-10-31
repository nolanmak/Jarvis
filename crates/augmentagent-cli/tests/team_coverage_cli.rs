//! #1702 — `augmentagent team-coverage report|review|gate` against an
//! isolated store. Synthetic data only.
use std::process::Command;

use augmentagent_store::team_coverage::TeamCoverageDecision;
use augmentagent_store::Store;

fn cli(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_augmentagent"));
    c.env_clear().env("HOME", home).env("PATH", "/usr/bin:/bin").current_dir(home);
    c
}

#[test]
fn report_review_and_gate_round_trip() {
    let home = tempfile::tempdir().unwrap();
    {
        let store = Store::open(home.path().join("data.db")).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        store
            .record_team_coverage_decision(&TeamCoverageDecision {
                action_id: "act-1".into(),
                mode: "shadow".into(),
                teammate: "Ana".into(),
                covering_message_id: "m-1".into(),
                thread_id: "T1".into(),
                covered_at_ms: now - 1_000,
                decided_at_ms: now,
            })
            .unwrap();
    }
    let out = cli(home.path()).args(["team-coverage", "report", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["rows"][0]["action_id"], "act-1");
    assert_eq!(v["rows"][0]["teammate"], "Ana");
    assert_eq!(v["rows"][0]["review"], serde_json::Value::Null);
    assert_eq!(v["summary"]["decisions"], 1);
    assert!(v.get("gate").is_none());

    // Unknown decision / verdict fail loudly.
    assert!(!cli(home.path()).args(["team-coverage", "review", "nope", "correct"]).output().unwrap().status.success());
    assert!(!cli(home.path()).args(["team-coverage", "review", "act-1", "maybe"]).output().unwrap().status.success());

    let ok = cli(home.path()).args(["team-coverage", "review", "act-1", "correct"]).output().unwrap();
    assert!(ok.status.success());
    let text = cli(home.path()).args(["team-coverage", "report"]).output().unwrap();
    let text = String::from_utf8_lossy(&text.stdout);
    assert!(text.contains("act-1"), "{text}");
    assert!(text.contains("team-coverage: 1 decision(s), 1 reviewed, 0 wrong, 0 replied-anyway"), "{text}");

    // The rollout gate is gone: the rule is on by default.
    assert!(!cli(home.path()).args(["team-coverage", "gate"]).output().unwrap().status.success());
}
