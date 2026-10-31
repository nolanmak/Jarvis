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

#[test]
fn readiness_without_policy_is_read_only_and_never_claims_live_ready() {
    let root=tempfile::tempdir().unwrap();
    let output=Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path()).env_remove("AUGMENTAGENT_NATIVE_VM_POLICY")
        .env("XDG_STATE_HOME",root.path().join("state"))
        .args(["autopr","readiness","--json"]).output().unwrap();
    assert_eq!(output.status.code(),Some(3),"{}",String::from_utf8_lossy(&output.stderr));
    let body:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["ready"],false);
    for provider in ["claude","codex"] {
        assert_eq!(body["credentials"][provider]["status"],"policy-missing");
        assert_eq!(body["credentials"][provider]["live_verified"],false);
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(),0);
}

#[test]
fn readiness_reports_expired_credentials_without_refreshing_or_leaking_them() {
    let root=tempfile::tempdir().unwrap();
    let auth=root.path().join("auth.json");
    let policy=root.path().join("policy.json");
    let raw=r#"{"claudeAiOauth":{"accessToken":"private-access-fixture","refreshToken":"private-refresh-fixture","expiresAt":1}}"#;
    std::fs::write(&auth,raw).unwrap();
    std::fs::set_permissions(&auth,std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&policy,serde_json::json!({"version":1,"runtime_config":"/missing/runtime.json","guest_runtime":"/missing/guest",
        "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
        "providers":{"claude":{"gateway":{},"auth_file":auth,"auth_relative":".claude/.credentials.json","refresh_auth":false}}}).to_string()).unwrap();
    std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
    let digest=Sha256::digest(std::fs::read(&policy).unwrap());
    let output=Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root.path()).env("AUGMENTAGENT_NATIVE_VM_POLICY",&policy)
        .env("XDG_STATE_HOME",root.path().join("state"))
        .args(["autopr","readiness","--json"]).output().unwrap();
    assert_eq!(output.status.code(),Some(3));
    let body:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["ready"],false);
    assert_eq!(body["credentials"]["claude"]["status"],"credential-expired");
    assert_eq!(body["credentials"]["codex"]["status"],"route-undeclared");
    assert_eq!(body["credentials"]["claude"]["policy_sha256"],format!("{digest:x}"));
    for bytes in [&output.stdout,&output.stderr] {assert!(!String::from_utf8_lossy(bytes).contains("private-access-fixture"));}
    assert_eq!(std::fs::read_to_string(&auth).unwrap(),raw);
    assert_eq!(Sha256::digest(std::fs::read(&policy).unwrap()),digest);
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(),2);
}

#[test]
fn readiness_rechecks_guest_artifacts_and_keeps_missing_build_runtime_explicit() {
    let root=tempfile::tempdir().unwrap();let guest=root.path().join("guest");
    std::fs::create_dir(&guest).unwrap();std::fs::set_permissions(&guest,std::fs::Permissions::from_mode(0o700)).unwrap();
    let artifact=guest.join("fixture");std::fs::write(&artifact,b"runtime").unwrap();
    std::fs::write(guest.join("manifest.json"),serde_json::json!({"version":1,"files":[{"path":"fixture","sha256":format!("{:x}",Sha256::digest(b"runtime"))}]}).to_string()).unwrap();
    let policy=root.path().join("policy.json");
    std::fs::write(&policy,serde_json::json!({"version":1,"runtime_config":root.path().join("missing-runtime.json"),"guest_runtime":guest,
        "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,"providers":{}}).to_string()).unwrap();
    std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
    for changed in [false,true] {
        if changed {std::fs::write(&artifact,b"changed").unwrap();}
        let output=Command::new(env!("CARGO_BIN_EXE_augmentagent")).current_dir(root.path())
            .env("AUGMENTAGENT_NATIVE_VM_POLICY",&policy).env("XDG_STATE_HOME",root.path().join("state"))
            .args(["autopr","readiness","--json"]).output().unwrap();
        assert_eq!(output.status.code(),Some(3));
        let body:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["runtime"]["status"],if changed {"guest-artifact-digest-mismatch"} else {"native-guest-verified"});
        assert_eq!(body["runtime"]["build_status"],"build-runtime-missing");
        assert_eq!(body["ready"],false);
        assert!(!root.path().join("state").exists());
    }
    assert_eq!(std::fs::read(artifact).unwrap(),b"changed","inspection must not repair artifacts");
}

#[test]
fn readiness_detects_provider_binary_updates_against_runtime_companions() {
    let root=tempfile::tempdir().unwrap();let guest=root.path().join("guest");
    std::fs::create_dir(&guest).unwrap();std::fs::set_permissions(&guest,std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable=root.path().join("codex");std::fs::write(&executable,b"native fixture").unwrap();
    std::fs::set_permissions(&executable,std::fs::Permissions::from_mode(0o700)).unwrap();
    let digest=format!("{:x}",Sha256::digest(b"native fixture"));
    std::fs::write(guest.join("companion"),b"companion fixture").unwrap();
    let hash=format!("{:x}",Sha256::digest(b"companion fixture"));
    let mut manifest=serde_json::json!({"version":1,"files":[{"path":"companion","sha256":hash}]});
    let manifest_path=guest.join("manifest.json");
    let policy=root.path().join("policy.json");
    std::fs::write(&policy,serde_json::json!({"version":1,"runtime_config":root.path().join("missing-runtime.json"),"guest_runtime":guest,
        "state_root":root.path(),"memory_mb":1024,"max_state_bytes":8589934592u64,"max_sessions":16,
        "providers":{"codex":{"gateway":{},"auth_relative":".codex/auth.json"}}}).to_string()).unwrap();
    std::fs::set_permissions(&policy,std::fs::Permissions::from_mode(0o600)).unwrap();
    for step in 0..3 {
        if step==1 {manifest["native_companions"]=serde_json::json!({digest.clone():[{"name":"codex-code-mode-host","path":"companion","sha256":hash}]});}
        if step==2 {std::fs::write(&executable,b"updated native fixture").unwrap();}
        std::fs::write(&manifest_path,manifest.to_string()).unwrap();
        std::fs::set_permissions(&manifest_path,std::fs::Permissions::from_mode(0o600)).unwrap();
        let output=Command::new(env!("CARGO_BIN_EXE_augmentagent")).current_dir(root.path())
            .env("AUGMENTAGENT_NATIVE_VM_POLICY",&policy).env("CODEX_CLI",&executable)
            .env("XDG_STATE_HOME",root.path().join("state")).args(["autopr","readiness","--json"]).output().unwrap();
        assert_eq!(output.status.code(),Some(3));
        let body:serde_json::Value=serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(body["runtime"]["providers"]["codex"]["status"],if step==1 {"executable-compatible"} else {"provider-runtime-companion-missing"});
        assert_eq!(body["ready"],false);
        assert!(!root.path().join("state").exists());
    }
}
