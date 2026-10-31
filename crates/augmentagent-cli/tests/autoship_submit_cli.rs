use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
};
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(root.path())
        .status()
        .unwrap()
        .success());
    let gh = root.path().join("gh");
    std::fs::write(&gh, r#"#!/usr/bin/env python3
import json,os,pathlib,sys
r=pathlib.Path(os.environ['FIXTURE']);a=sys.argv[1:]
s=json.loads((r/'scenario.json').read_text())
if a[:2]==['repo','view']: print(s.get('permission','WRITE') if 'viewerPermission' in a else 'owner/repo')
elif a[:2]==['api','user']: print(s.get('actor','owner'))
elif a[:2]==['api','repos/owner/repo/issues/7']:
 print(json.dumps({'number':7,'state':s.get('state','open'),'html_url':'https://github.com/owner/repo/issues/7','title':'Fix arithmetic','body':'Fix addition','user':{'login':'owner'},'author_association':'OWNER'}))
elif a[:2]==['pr','list']: print(json.dumps([{'number':42,'isCrossRepository':False,'headRepositoryOwner':{'login':'owner'}}] if s.get('open_pr') else []))
elif a[:3]==['pr','view','42']:
 print(json.dumps({'number':42,'state':s.get('pr_state','OPEN'),'url':'https://github.com/owner/repo/pull/42','headRefName':'feature/fix','headRefOid':'a'*40,'baseRefName':'main','isCrossRepository':s.get('fork',False),'author':{'login':'owner'},'title':'Fix arithmetic','body':'Fix addition','closingIssuesReferences':[]}))
else: raise Exception('unexpected request '+repr(a))
"#).unwrap();
    std::fs::set_permissions(gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(root.path().join("scenario.json"), "{}").unwrap();
    root
}
fn run(root: &Path, args: &[&str]) -> Output {
    let mut flags = vec!["--issue", "7"];
    flags.extend_from_slice(args);
    invoke(root, &flags)
}
fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root)
        .env("FIXTURE", root)
        .env("GH_BIN", root.join("gh"))
        .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner")
        .env("XDG_STATE_HOME", root.join("state"))
        .env(
            "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
            root.join("state/workflow.db"),
        )
        .args(["autopr", "run", "--repo", "owner/repo", "--json"])
        .args(args)
        .output()
        .unwrap()
}
fn body(output: Output, code: i32) -> serde_json::Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
#[test]
fn dry_run_and_unauthorized_targets_create_no_state() {
    let root = fixture();
    let result = body(run(root.path(), &["--dry-run"]), 0);
    assert_eq!(result["dry_run"], true);
    assert_eq!(result["accepted"], false);
    assert_eq!(result["complete"], false);
    assert!(!root.path().join("state").exists());
    std::fs::write(
        root.path().join("scenario.json"),
        r#"{"permission":"READ"}"#,
    )
    .unwrap();
    let result = body(run(root.path(), &[]), 2);
    assert_eq!(result["accepted"], false);
    assert!(!root.path().join("state").exists());
    assert!(!root.path().join("data.db").exists());
    assert!(!root.path().join(".self-improve-worktrees").exists());
}
#[test]
fn enqueue_replay_survives_closed_target_and_rejects_actor_or_payload_changes() {
    let root = fixture();
    let first = body(
        run(
            root.path(),
            &[
                "--request-id",
                "test-request",
                "--provider",
                "codex",
                "--until",
                "pr",
            ],
        ),
        0,
    );
    assert_eq!(first["accepted"], true);
    assert_eq!(first["complete"], false);
    assert_eq!(first["workflow"], "owner/repo#7");
    assert_eq!(first["reason"], "queued");
    std::fs::write(root.path().join("scenario.json"), r#"{"state":"closed"}"#).unwrap();
    let replay = body(
        run(
            root.path(),
            &[
                "--request-id",
                "test-request",
                "--provider",
                "codex",
                "--until",
                "pr",
            ],
        ),
        0,
    );
    assert_eq!(first, replay);
    body(
        run(
            root.path(),
            &["--request-id", "test-request", "--provider", "codex"],
        ),
        2,
    );
    std::fs::write(root.path().join("scenario.json"), r#"{"actor":"other"}"#).unwrap();
    body(
        run(
            root.path(),
            &[
                "--request-id",
                "test-request",
                "--provider",
                "codex",
                "--until",
                "pr",
            ],
        ),
        2,
    );
    let conn = rusqlite::Connection::open(root.path().join("state/workflow.db")).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM command_job", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM workflow", [], |r| r.get::<_, u32>(0))
            .unwrap(),
        0
    );
    assert!(!root.path().join("data.db").exists());
}

#[test]
fn generated_request_id_is_recoverable_and_policy_joins_do_not_upgrade() {
    let root = fixture();
    let first = body(run(root.path(), &["--until", "pr"]), 0);
    let id = first["request_id"].as_str().unwrap();
    let conn = rusqlite::Connection::open(root.path().join("state/workflow.db")).unwrap();
    let (actor, payload): (String, String) = conn
        .query_row(
            "SELECT principal,payload FROM command_client_intent WHERE request_id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(actor, "github:owner");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&payload).unwrap()["until"],
        "pr"
    );
    assert_eq!(
        first,
        body(run(root.path(), &["--until", "pr", "--request-id", id]), 0)
    );
    let rejected = body(run(root.path(), &["--request-id", "upgrade"]), 2);
    assert_eq!(rejected["reason"], "authorization-upgrade-required");
    assert_eq!(rejected["workflow_until"], "pr");
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM command_job", [], |r| r
            .get::<_, u32>(0))
            .unwrap(),
        1
    );
}
#[test]
fn source_less_pr_submits_and_closed_or_fork_targets_do_not_create_state() {
    let root = fixture();
    for scenario in [r#"{"pr_state":"CLOSED"}"#, r#"{"fork":true}"#] {
        std::fs::write(root.path().join("scenario.json"), scenario).unwrap();
        body(invoke(root.path(), &["--pr", "42"]), 2);
        assert!(!root.path().join("state").exists());
    }
    std::fs::write(root.path().join("scenario.json"), "{}").unwrap();
    let result = body(invoke(root.path(), &["--pr", "42"]), 0);
    assert_eq!(result["workflow"], "owner/repo/pr/42");
    assert_eq!(result["workflow_until"], "merged");
    assert_eq!(result["complete"], false);
}
#[test]
fn malformed_request_id_and_closed_issue_cannot_initialize_state() {
    let root = fixture();
    body(run(root.path(), &["--request-id", "bad/id"]), 2);
    assert!(!root.path().join("state").exists());
    std::fs::write(root.path().join("scenario.json"), r#"{"state":"closed"}"#).unwrap();
    body(run(root.path(), &[]), 2);
    assert!(!root.path().join("state").exists());
}

fn work(root: &Path, id: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root)
        .env("FIXTURE", root)
        .env("GH_BIN", root.join("gh"))
        .env_remove("AUGMENTAGENT_NATIVE_VM_POLICY")
        .env("AUGMENTAGENT_SELFIMPROVE_TRUSTED_AUTHORS", "owner")
        .env("XDG_STATE_HOME", root.join("state"))
        .env(
            "AUGMENTAGENT_AUTOPR_WORKFLOW_DB",
            root.join("state/workflow.db"),
        )
        .args(["autopr", "work", "--once", "--workflow", id, "--json"])
        .output()
        .unwrap()
}
#[test]
fn work_once_missing_job_is_invalid_and_missing_runtime_is_deferred_without_mutation() {
    let root = fixture();
    let missing = body(work(root.path(), "owner/repo#7"), 2);
    assert_eq!(missing["reason"], "workflow-not-found");
    assert!(!root.path().join("state").exists());
    body(run(root.path(), &["--request-id", "work"]), 0);
    let path = root.path().join("state/workflow.db");
    let before = std::fs::read(&path).unwrap();
    let result = body(work(root.path(), "owner/repo#7"), 3);
    assert_eq!(result["reason"], "execution-readiness-blocked");
    assert_eq!(result["accepted"], true);
    assert_eq!(result["complete"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!root.path().join(".self-improve-worktrees").exists());
    assert!(!root.path().join("data.db").exists());
}

#[test]
fn work_once_preserves_holds_live_ownership_and_terminal_checkpoints() {
    for (stage, lease_until, eligible, expected) in [
        ("held", 0, 0, 3),
        ("retired", 0, 0, 3),
        ("await_checks", 0, 0, 3),
        ("merge", 0, 0, 3),
        ("build", i64::MAX, 0, 3),
        ("build", 0, i64::MAX, 3),
        ("merged", 0, 0, 0),
    ] {
        let root = fixture();
        body(run(root.path(), &["--request-id", "held-job"]), 0);
        let path = root.path().join("state/workflow.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        let checkpoint = serde_json::json!({"stage":stage,"binding":{"head":"a".repeat(40),"contract":"contract","evidence_hash":"evidence"},"evidence":[],"next":{"reason":"preserved-controller-state","action":"inspect","eligible_at_ms":eligible}});
        conn.execute("INSERT INTO workflow(id,branch,checkpoint,heartbeat_at,progress_at,lease_until) VALUES('owner/repo#7','feature/fix',?1,10,10,?2)",rusqlite::params![checkpoint.to_string(),lease_until]).unwrap();
        drop(conn);
        std::fs::write(root.path().join("scenario.json"), r#"{"state":"closed"}"#).unwrap();
        let before = std::fs::read(&path).unwrap();
        let result = body(work(root.path(), "owner/repo#7"), expected);
        assert_eq!(result["stage"], stage);
        assert_eq!(result["reason"], "preserved-controller-state");
        assert_eq!(result["complete"], stage == "merged");
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!root.path().join(".self-improve-worktrees").exists());
    }
}

#[test]
fn work_once_does_not_duplicate_an_unbound_existing_pr() {
    let root = fixture();
    body(run(root.path(), &["--request-id", "dedup"]), 0);
    std::fs::write(root.path().join("scenario.json"), r#"{"open_pr":true}"#).unwrap();
    let path = root.path().join("state/workflow.db");
    let before = std::fs::read(&path).unwrap();
    let result = body(work(root.path(), "owner/repo#7"), 3);
    assert_eq!(result["reason"], "target-pr-reconciliation-required");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!root.path().join(".self-improve-worktrees").exists());
}

#[test]
fn work_once_rechecks_repository_authority_before_execution() {
    let root = fixture();
    body(run(root.path(), &["--request-id", "revoked"]), 0);
    std::fs::write(
        root.path().join("scenario.json"),
        r#"{"permission":"READ"}"#,
    )
    .unwrap();
    let path = root.path().join("state/workflow.db");
    let before = std::fs::read(&path).unwrap();
    let result = body(work(root.path(), "owner/repo#7"), 2);
    assert_eq!(result["reason"], "target-authorization-failed");
    assert_eq!(result["accepted"], false);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!root.path().join(".self-improve-worktrees").exists());
}
