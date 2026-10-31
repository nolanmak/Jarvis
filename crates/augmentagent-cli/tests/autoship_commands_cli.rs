use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::{
    path::Path,
    process::{Command, Output},
};

fn status(root: &Path, workflow: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root)
        .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("AUGMENTAGENT_AUTOPR_DAILY_CAP", "5")
        .args(["autopr", "status", "--workflow", workflow, "--json"])
        .output()
        .unwrap()
}

#[test]
fn missing_workflow_status_is_structured_and_does_not_create_state() {
    let root = tempfile::tempdir().unwrap();
    let output = status(root.path(), "owner/repo#12");
    assert_eq!(output.status.code(), Some(2));
    let body: serde_json::Value = serde_json::from_slice(&output.stdout)
        .expect("CLI must return structured status instead of a parser error");
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["reason"], "workflow-not-found");
    assert_eq!(
        std::fs::read_dir(root.path()).unwrap().count(),
        0,
        "status must not initialize workflow or unrelated application state"
    );
}

#[test]
fn status_reports_real_checkpoint_and_pending_capacity_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workflow.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE workflow(id TEXT PRIMARY KEY,branch TEXT,pr INTEGER,checkpoint TEXT,owner TEXT,
        generation INTEGER,lease_until INTEGER,heartbeat_at INTEGER,progress_at INTEGER,attempts INTEGER,
        wall_ms INTEGER,model_calls INTEGER,tokens INTEGER);
        CREATE TABLE operation(id TEXT PRIMARY KEY,workflow TEXT,kind TEXT,binding TEXT,receipt TEXT,created_at INTEGER);
        CREATE TABLE merge_allowance_dispatch(operation TEXT PRIMARY KEY,repository TEXT,pr INTEGER,pr_node_id TEXT,
        head TEXT,generation INTEGER,reserved_at_ms INTEGER,dispatched_at_ms INTEGER,merged_at_ms INTEGER,resolution TEXT,state TEXT);
        CREATE TABLE merge_transport_response(operation TEXT PRIMARY KEY,response TEXT,observed_at_ms INTEGER);").unwrap();
    let binding =
        serde_json::json!({"head":"a".repeat(40),"contract":"contract","evidence_hash":"evidence"});
    let checkpoint = serde_json::json!({"stage":"held","binding":binding,"evidence":[],
        "next":{"reason":"merge-reconciliation-required","action":"reconcile_merge","eligible_at_ms":42}});
    conn.execute(
        "INSERT INTO workflow VALUES('owner/repo#12','feature/fix',34,?1,NULL,2,0,10,9,8,100,3,50)",
        [checkpoint.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO operation VALUES('op-pending','owner/repo#12','\"merge_pr\"',?1,NULL,10)",
        [binding.to_string()],
    )
    .unwrap();
    conn.execute("INSERT INTO merge_allowance_dispatch VALUES('op-pending','owner/repo',34,'PR_34',?1,2,10,11,NULL,NULL,'dispatched')", ["a".repeat(40)]).unwrap();
    conn.execute(
        "INSERT INTO merge_transport_response VALUES('op-pending',?1,12)",
        [r#"{"data":null}"#],
    )
    .unwrap();
    drop(conn);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let before = std::fs::read(&path).unwrap();
    let output = status(root.path(), "owner/repo#12");
    assert_eq!(
        output.status.code(),
        Some(3),
        "blocked workflow is explicit: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["workflow"], "owner/repo#12");
    assert_eq!(body["stage"], "held");
    assert_eq!(body["reason"], "merge-reconciliation-required");
    assert_eq!(body["next_action"], "reconcile_merge");
    assert_eq!(body["pr_url"], "https://github.com/owner/repo/pull/34");
    assert_eq!(body["allowance"]["confirmed"], 0);
    assert_eq!(body["allowance"]["pending"], 1);
    assert_eq!(body["allowance"]["remaining"], 5);
    assert_eq!(body["allowance"]["available"], 4);
    assert_eq!(body["pending_merge_operations"][0], "op-pending");
    assert_eq!(body["deployment"]["state"], "unknown");
    let manifest = &body["pending_merge_evidence"][0];
    assert_eq!(manifest["operation"], "op-pending");
    assert_eq!(
        manifest["response_sha256"],
        format!("{:x}", Sha256::digest(br#"{"data":null}"#))
    );
    assert!(
        manifest.get("response").is_none(),
        "status exposes digest, never raw response"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn invalid_status_target_rejects_without_creating_state() {
    let root = tempfile::tempdir().unwrap();
    for workflow in [
        "../../other#1",
        "owner/repo#0",
        "owner/repo#bad",
        "owner/repo",
    ] {
        let output = status(root.path(), workflow);
        assert_eq!(output.status.code(), Some(2));
        let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["reason"], "invalid-workflow");
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn corrupt_status_store_reports_failure_without_reset() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workflow.db");
    std::fs::write(&path, b"corrupt state").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let output = status(root.path(), "owner/repo#12");
    assert_eq!(output.status.code(), Some(4));
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["reason"], "workflow-state-unavailable");
    assert_eq!(std::fs::read(path).unwrap(), b"corrupt state");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
}
