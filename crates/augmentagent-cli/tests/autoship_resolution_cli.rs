use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::{
    path::Path,
    process::{Command, Output},
};

fn fixture(response: Option<Value>, merged: bool, remote_head: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let db = root.join("workflow.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE workflow(id TEXT PRIMARY KEY,branch TEXT,pr INTEGER,checkpoint TEXT,owner TEXT,
        generation INTEGER,lease_until INTEGER,heartbeat_at INTEGER,progress_at INTEGER,attempts INTEGER,
        wall_ms INTEGER,model_calls INTEGER,tokens INTEGER);
        CREATE TABLE operation(id TEXT PRIMARY KEY,workflow TEXT,kind TEXT,binding TEXT,receipt TEXT,created_at INTEGER);
        CREATE TABLE merge_allowance_dispatch(operation TEXT PRIMARY KEY,repository TEXT,pr INTEGER,pr_node_id TEXT,
        head TEXT,generation INTEGER,reserved_at_ms INTEGER,dispatched_at_ms INTEGER,merged_at_ms INTEGER,resolution TEXT,state TEXT);
        CREATE TABLE merge_transport_response(operation TEXT PRIMARY KEY,response TEXT,observed_at_ms INTEGER);").unwrap();
    let binding = json!({"head":"a".repeat(40),"contract":"contract","evidence_hash":"proof"});
    let checkpoint = json!({"stage":"held","binding":binding,"evidence":[],"next":{"reason":"merge-reconciliation-required","action":"reconcile_merge","eligible_at_ms":0}});
    conn.execute(
        "INSERT INTO workflow VALUES('owner/repo#12','feature/fix',34,?1,NULL,1,0,10,10,1,0,0,0)",
        [checkpoint.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO operation VALUES('op-pending','owner/repo#12','\"merge_pr\"',?1,NULL,10)",
        [binding.to_string()],
    )
    .unwrap();
    conn.execute("INSERT INTO merge_allowance_dispatch VALUES('op-pending','owner/repo',34,'PR_34',?1,1,10,11,NULL,NULL,'dispatched')",["a".repeat(40)]).unwrap();
    let mut manifest = json!({"schema_version":1,"operation":"op-pending","repository":"owner/repo","pr":34,"head":"a".repeat(40)});
    if let Some(response) = response {
        let raw = response.to_string();
        manifest["response_sha256"] = format!("{:x}", Sha256::digest(raw.as_bytes())).into();
        conn.execute(
            "INSERT INTO merge_transport_response VALUES('op-pending',?1,12)",
            [raw],
        )
        .unwrap();
    }
    drop(conn);
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
    if merged {
        manifest["remote_head"] = remote_head.into();
        manifest["merge_commit"] = "c".repeat(40).into();
        manifest["merged_at"] = "2026-10-31T00:00:00Z".into();
    }
    std::fs::write(root.join("evidence.json"), manifest.to_string()).unwrap();
    std::fs::set_permissions(
        root.join("evidence.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(root.join("remote.json"),json!({"number":34,"node_id":"PR_34","merged":merged,"head":{"sha":remote_head},
        "merge_commit_sha":"c".repeat(40),"merged_at":if merged {Some("2026-10-31T00:00:00Z")} else {None},
        "merged_by":{"login":"owner"},"base":{"repo":{"full_name":"owner/repo"}}}).to_string()).unwrap();
    std::fs::write(root.join("gh"),r#"#!/usr/bin/env python3
import json,pathlib,sys
root=pathlib.Path(__file__).parent
a=sys.argv[1:]
with (root/'calls').open('a') as f: f.write(json.dumps(a)+'\n')
if a==['api','user']: print(json.dumps({'login':'owner'}))
elif a==['api','repos/owner/repo']: print(json.dumps({'full_name':'owner/repo','permissions':{'admin':not (root/'deny').exists()}}))
elif a==['api','repos/owner/repo/pulls/34']: print((root/'remote.json').read_text())
elif a==['api','repos/owner/repo/git/commits/'+'c'*40]: print((root/'commit.json').read_text())
else: raise Exception('unexpected/mutating GitHub call '+repr(a))
"#).unwrap();
    std::fs::set_permissions(root.join("gh"), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}
fn invoke(root: &Path, classification: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(root)
        .env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB", root.join("workflow.db"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("GH_BIN", root.join("gh"))
        .args([
            "autopr",
            "resolve-merge",
            "--operation",
            "op-pending",
            "--classification",
            classification,
            "--evidence",
            "evidence.json",
            "--json",
        ])
        .output()
        .unwrap()
}
fn assert_code(out: &Output, code: i32) -> Value {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("structured resolution result")
}
fn request_error() -> Value {
    json!({"errors":[{"message":"Unknown field","locations":[{"line":1,"column":1}]}]})
}
fn accepted() -> Value {
    json!({"data":{"mergePullRequest":{"clientMutationId":"op-pending","pullRequest":{"id":"PR_34","number":34,"state":"MERGED","headRefOid":"a".repeat(40),"mergedAt":"2026-10-31T00:00:00Z","mergeCommit":{"oid":"c".repeat(40)},"mergedBy":{"login":"owner"}}}}})
}

#[test]
fn cli_recovers_lost_response_from_immutable_commit_without_inventing_echo() {
    for scenario in ["valid","partial","execution-error","pre-execution-error","wrong-digest","wrong-marker","wrong-commit","legacy","oversized","invalid-time"] {
        let response = match scenario {
            "partial" | "wrong-digest" => Some(json!({"data":{"mergePullRequest":null}})),
            "execution-error" => Some(json!({"data":null,"errors":[{"message":"execution interrupted"}]})),
            "pre-execution-error" => Some(request_error()),
            _ => None,
        };
        let dir = fixture(response.clone(),true,&"a".repeat(40));
        let root = dir.path();
        let marker = format!("Autoship-Receipt: v1:op-pending:{}","f".repeat(64));
        if scenario != "legacy" {
            let conn = rusqlite::Connection::open(root.join("workflow.db")).unwrap();
            conn.execute_batch("CREATE TABLE merge_private_marker(operation TEXT PRIMARY KEY,protocol INTEGER,nonce TEXT);").unwrap();
            conn.execute("INSERT INTO merge_private_marker VALUES('op-pending',1,?1)",["f".repeat(64)]).unwrap();
        }
        std::fs::write(root.join("commit.json"),json!({
            "sha":if scenario=="wrong-commit" {"d".repeat(40)} else {"c".repeat(40)},
            "message":if scenario=="wrong-marker" {"manual merge".into()}
                else if scenario=="oversized" { format!("{}\n{marker}","x".repeat(20_000)) } else {marker}
        }).to_string()).unwrap();
        if scenario == "invalid-time" {
            for file in ["remote.json","evidence.json"] {
                let path = root.join(file);
                let mut value:Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                value["merged_at"] = "invalid-time".into();
                std::fs::write(path,value.to_string()).unwrap();
            }
        }
        if scenario == "wrong-digest" {
            let path = root.join("evidence.json");
            let mut value:Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["response_sha256"] = "0".repeat(64).into();
            std::fs::write(path,value.to_string()).unwrap();
        }
        let before = Sha256::digest(std::fs::read(root.join("workflow.db")).unwrap());
        if !matches!(scenario,"valid"|"partial"|"execution-error") {
            assert_code(&invoke(root,"autonomous"),3);
            assert_eq!(Sha256::digest(std::fs::read(root.join("workflow.db")).unwrap()),before,"{scenario}");
            continue;
        }
        for _ in 0..2 { assert_code(&invoke(root,"autonomous"),0); }
        let conn = rusqlite::Connection::open(root.join("workflow.db")).unwrap();
        let raw:String = conn.query_row("SELECT receipt FROM merge_mutation_receipt",[],|r|r.get(0)).unwrap();
        let proof:Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(proof["commit_marker_operation"],"op-pending");
        assert!(proof.get("client_mutation_id").is_none());
        let decision:String = conn.query_row("SELECT resolution FROM merge_allowance_dispatch",[],|r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&decision).unwrap()["proof_kind"],"immutable_commit_marker");
        let checkpoint:String = conn.query_row("SELECT checkpoint FROM workflow",[],|r|r.get(0)).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&checkpoint).unwrap()["stage"],"merged");
        let n:u32 = conn.query_row("SELECT count(*) FROM workflow_event WHERE kind='autonomous_merge_confirmed'",[],|r|r.get(0)).unwrap();
        assert_eq!(n,1);
        let journals:u32 = conn.query_row("SELECT count(*) FROM merge_transport_response",[],|r|r.get(0)).unwrap();
        assert_eq!(journals,u32::from(response.is_some()),"recovery must never fabricate an original response");
        if let Some(response) = response {
            let journal:String = conn.query_row("SELECT response FROM merge_transport_response",[],|r|r.get(0)).unwrap();
            assert_eq!(journal,response.to_string(),"original response must remain immutable");
        }
        if scenario == "valid" {
            let mut late = accepted();
            late["data"]["mergePullRequest"]["pullRequest"]["mergeCommit"]["message"] = proof["merge_message"].clone();
            let late = late.to_string();
            conn.execute("INSERT INTO merge_transport_response VALUES('op-pending',?1,99)",[&late]).unwrap();
            let path = root.join("evidence.json");
            let mut manifest:Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            manifest["response_sha256"] = format!("{:x}",Sha256::digest(late.as_bytes())).into();
            std::fs::write(path,manifest.to_string()).unwrap();
            drop(conn);
            assert_code(&invoke(root,"autonomous"),0);
            let conn = rusqlite::Connection::open(root.join("workflow.db")).unwrap();
            let canonical:String = conn.query_row("SELECT receipt FROM merge_mutation_receipt",[],|r|r.get(0)).unwrap();
            assert_eq!(serde_json::from_str::<Value>(&canonical).unwrap(),proof);
            let original_audit:String = conn.query_row("SELECT resolution FROM merge_allowance_dispatch",[],|r|r.get(0)).unwrap();
            assert_eq!(original_audit,decision,"late evidence must not rewrite the first audit");
        }
    }
}

#[test]
fn cli_resolves_original_accepted_response_once_without_mutation() {
    let dir = fixture(Some(accepted()), true, &"a".repeat(40));
    for _ in 0..2 {
        let body = assert_code(&invoke(dir.path(), "autonomous"), 0);
        assert_eq!(body["classification"], "autonomous");
    }
    let conn = rusqlite::Connection::open(dir.path().join("workflow.db")).unwrap();
    let state: String = conn
        .query_row("SELECT state FROM merge_allowance_dispatch", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(state, "confirmed");
    let n: u64 = conn
        .query_row("SELECT count(*) FROM merge_mutation_receipt", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(n, 1);
    let checkpoint:String = conn.query_row("SELECT checkpoint FROM workflow",[],|r|r.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&checkpoint).unwrap()["stage"],"merged");
    let status = Command::new(env!("CARGO_BIN_EXE_augmentagent"))
        .current_dir(dir.path()).env("AUGMENTAGENT_AUTOPR_WORKFLOW_DB",dir.path().join("workflow.db"))
        .args(["autopr","status","--workflow","owner/repo#12","--json"]).output().unwrap();
    assert_eq!(assert_code(&status,0)["complete"],true);
    assert!(
        !dir.path().join("state").exists(),
        "no unrelated application initialization"
    );
}

#[test]
fn cli_recovers_marked_journal_response_without_losing_proof() {
    let marker = format!("Autoship-Receipt: v1:op-pending:{}", "f".repeat(64));
    let mut response = accepted();
    response["data"]["mergePullRequest"]["pullRequest"]["mergeCommit"]["message"] = marker.clone().into();
    let dir = fixture(Some(response), true, &"a".repeat(40));
    {
        let conn = rusqlite::Connection::open(dir.path().join("workflow.db")).unwrap();
        conn.execute_batch("CREATE TABLE merge_private_marker(operation TEXT PRIMARY KEY,protocol INTEGER,nonce TEXT);").unwrap();
        conn.execute("INSERT INTO merge_private_marker VALUES('op-pending',1,?1)", ["f".repeat(64)]).unwrap();
    }
    for _ in 0..2 { assert_code(&invoke(dir.path(), "autonomous"), 0); }
    let conn = rusqlite::Connection::open(dir.path().join("workflow.db")).unwrap();
    let raw: String = conn.query_row("SELECT receipt FROM merge_mutation_receipt", [], |r| r.get(0)).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&raw).unwrap()["merge_message"], marker);
    let confirmed: u32 = conn.query_row("SELECT count(*) FROM merge_allowance_dispatch WHERE state='confirmed'", [], |r| r.get(0)).unwrap();
    assert_eq!(confirmed, 1);
}
#[test]
fn cli_releases_only_proven_unexecuted_or_external_merges() {
    for (response, merged, head, classification) in [
        (Some(request_error()), false, "a".repeat(40), "not-merged"),
        (None, true, "b".repeat(40), "manual"),
    ] {
        let dir = fixture(response, merged, &head);
        for _ in 0..2 {
            assert_code(&invoke(dir.path(), classification), 0);
        }
        let conn = rusqlite::Connection::open(dir.path().join("workflow.db")).unwrap();
        let state: String = conn
            .query_row("SELECT state FROM merge_allowance_dispatch", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "released");
        let n: u64 = conn
            .query_row("SELECT count(*) FROM merge_mutation_receipt", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
        let events: u64 = conn
            .query_row(
                "SELECT count(*) FROM workflow_event WHERE kind='merge_capacity_resolved'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
        if classification == "manual" {
            drop(conn);
            let before = Sha256::digest(std::fs::read(dir.path().join("workflow.db")).unwrap());
            for (file,key) in [("evidence.json","merge_commit"),("remote.json","merge_commit_sha")] {
                let path = dir.path().join(file);
                let mut value:Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                value[key] = "d".repeat(40).into();
                std::fs::write(path,value.to_string()).unwrap();
            }
            assert_code(&invoke(dir.path(),"manual"),3);
            assert_eq!(Sha256::digest(std::fs::read(dir.path().join("workflow.db")).unwrap()),before);
        }
    }
}
#[test]
fn cli_repeats_controller_resolution_without_rewriting_audit_or_live_work() {
    let dir = fixture(Some(request_error()), false, &"a".repeat(40));
    assert_code(&invoke(dir.path(),"not-merged"),0);
    {
        let conn = rusqlite::Connection::open(dir.path().join("workflow.db")).unwrap();
        let raw:String = conn.query_row("SELECT resolution FROM merge_allowance_dispatch",[],|r|r.get(0)).unwrap();
        let mut decision:Value = serde_json::from_str(&raw).unwrap();
        decision["actor"] = "autoship".into();
        conn.execute("UPDATE merge_allowance_dispatch SET resolution=?1",[decision.to_string()]).unwrap();
        conn.execute("UPDATE operation SET receipt=?1",[decision.to_string()]).unwrap();
        conn.execute("UPDATE workflow SET owner='new-worker',lease_until=?1,generation=generation+1",
            [chrono::Utc::now().timestamp_millis()+60_000]).unwrap();
    }
    let before = Sha256::digest(std::fs::read(dir.path().join("workflow.db")).unwrap());
    let body = assert_code(&invoke(dir.path(),"not-merged"),0);
    assert_eq!(body["reason"],"merge-resolution-already-recorded");
    assert_eq!(body["actor"],"owner");
    assert_eq!(body["resolution_actor"],"autoship");
    assert_eq!(Sha256::digest(std::fs::read(dir.path().join("workflow.db")).unwrap()),before);
    let path = dir.path().join("evidence.json");
    let mut manifest:Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["response_sha256"] = "0".repeat(64).into();
    std::fs::write(path,manifest.to_string()).unwrap();
    assert_code(&invoke(dir.path(),"not-merged"),3);
    assert_eq!(Sha256::digest(std::fs::read(dir.path().join("workflow.db")).unwrap()),before);
}

#[test]
fn cli_cannot_resolve_ambiguity_from_open_state_or_same_account_alone() {
    for (merged, classification) in [
        (false, "not-merged"),
        (true, "manual"),
        (true, "autonomous"),
    ] {
        let dir = fixture(None, merged, &"a".repeat(40));
        let before = std::fs::read(dir.path().join("workflow.db")).unwrap();
        assert_code(&invoke(dir.path(), classification), 3);
        assert_eq!(
            std::fs::read(dir.path().join("workflow.db")).unwrap(),
            before
        );
    }
}
#[test]
fn cli_requires_operator_authority_and_matching_private_evidence() {
    let dir = fixture(Some(request_error()), false, &"a".repeat(40));
    let before = std::fs::read(dir.path().join("workflow.db")).unwrap();
    std::fs::write(dir.path().join("deny"), b"deny").unwrap();
    assert_code(&invoke(dir.path(), "not-merged"), 2);
    std::fs::remove_file(dir.path().join("deny")).unwrap();
    std::fs::set_permissions(
        dir.path().join("evidence.json"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert_code(&invoke(dir.path(), "not-merged"), 2);
    assert_eq!(
        std::fs::read(dir.path().join("workflow.db")).unwrap(),
        before
    );
}

#[test]
fn cli_rejects_stale_or_contradictory_proof_without_writing_state() {
    for scenario in [
        "wrong-digest",
        "wrong-node",
        "wrong-commit",
        "contradictory-head",
        "active-worker",
        "missing-marker",
    ] {
        let raw = if scenario == "active-worker" {
            request_error()
        } else {
            accepted()
        };
        let head = if scenario == "contradictory-head" {
            "b".repeat(40)
        } else {
            "a".repeat(40)
        };
        let dir = fixture(Some(raw), scenario != "active-worker", &head);
        let root = dir.path();
        if scenario == "missing-marker" {
            let conn = rusqlite::Connection::open(root.join("workflow.db")).unwrap();
            conn.execute_batch("CREATE TABLE merge_private_marker(operation TEXT PRIMARY KEY,protocol INTEGER,nonce TEXT);
                INSERT INTO merge_private_marker VALUES('op-pending',1,'private-marker');").unwrap();
        }
        if scenario == "wrong-digest" {
            let path = root.join("evidence.json");
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            value["response_sha256"] = "0".repeat(64).into();
            std::fs::write(path, value.to_string()).unwrap();
        }
        if matches!(scenario, "wrong-node" | "wrong-commit") {
            let path = root.join("remote.json");
            let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            if scenario == "wrong-node" {
                value["node_id"] = "OTHER_PR".into();
            } else {
                value["merge_commit_sha"] = "d".repeat(40).into();
            }
            std::fs::write(path, value.to_string()).unwrap();
        }
        if scenario == "active-worker" {
            let conn = rusqlite::Connection::open(root.join("workflow.db")).unwrap();
            conn.execute(
                "UPDATE workflow SET owner='active-worker',lease_until=?1",
                [chrono::Utc::now().timestamp_millis() + 60_000],
            )
            .unwrap();
        }
        let before = std::fs::read(root.join("workflow.db")).unwrap();
        let classification = match scenario {
            "active-worker" => "not-merged",
            "contradictory-head" => "manual",
            _ => "autonomous",
        };
        assert_code(&invoke(root, classification), 3);
        assert_eq!(
            Sha256::digest(std::fs::read(root.join("workflow.db")).unwrap()),
            Sha256::digest(before),
            "{scenario}: invalid proof must not initialize a writer"
        );
    }
}
