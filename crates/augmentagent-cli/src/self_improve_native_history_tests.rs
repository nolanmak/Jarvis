use super::*;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

fn proof(prepared: &PreparedAcceptance, purpose: Option<&str>) -> serde_json::Value {
    serde_json::json!({"version":1,"head":prepared.baseline.registration.baseline_head(),
        "provider":"codex","native_session":"synthetic-controller-session",
        "supplied_request_hash":"a".repeat(64),"response_hash":"b".repeat(64),
        "purpose":purpose,"scope_context_hash":null,"native_input_hash":null,
        "scope_request_hash":null,"scope_policy_hash":null})
}
fn write_private(path: &Path, value: &[u8]) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().create_new(true).write(true).mode(0o600).open(path).unwrap();
    file.write_all(value).unwrap();
}

#[test]
fn prior_readiness_and_scope_proofs_do_not_invent_a_missing_baseline_admission() {
    for purpose in ["readiness", "specification", "executable_contract", "source_selection"] {
        let (root, control, prepared) = super::tests::fixture();
        let database = control.path().join("workflow.sqlite3");
        let mut store = crate::autoship_workflow::WorkflowStore::open(&database).unwrap();
        let id = "owner/repo#840";
        let binding = crate::autoship_workflow::Binding {head:prepared.baseline.registration.baseline_head().into(),
            contract:prepared.baseline.registration.contract_hash().into(),evidence_hash:"e".repeat(64)};
        store.register(id, "fixture", binding, 0).unwrap();
        let lease = store.claim(id, "fixture-owner", 1, 100).unwrap().unwrap();
        store.charge(&lease, "prior-ordinary-scoping", crate::autoship_workflow::ResourceCharge {
            wall_ms:10, model_calls:6, tokens:20}, 2).unwrap();
        store.release(&lease, 3).unwrap();
        let path = control.path().join(format!("primary-scope-{}.json", uuid::Uuid::new_v4()));
        let bytes = serde_json::to_vec(&proof(&prepared, Some(purpose))).unwrap();
        write_private(&path, &bytes);
        let before = store.snapshot(id).unwrap().unwrap();
        assert_eq!(before.model_calls, 6);
        let events = store.events(id).unwrap();
        let runs = retained_controller_slots(root.path(), control.path(), &prepared, &store.inspect_verifier_evidence(id))
            .unwrap_or_else(|error| panic!("ordinary {purpose} proof blocked the FIRST baseline witness: {error:#}"));
        assert!(runs.is_empty(), "ordinary scope output became a baseline judgment");
        assert_eq!(store.snapshot(id).unwrap().unwrap(), before);
        assert_eq!(store.events(id).unwrap(), events);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!qualification_destination(control.path(), &prepared).unwrap().exists());
    }
}

#[test]
fn unattributed_baseline_and_ambiguous_native_history_still_refuse() {
    for fault in ["baseline", "bounded_source", "bounded_readiness", "legacy", "unknown", "malformed", "symlink", "hardlink", "public", "oversize", "qualification"] {
        let (root, control, prepared) = super::tests::fixture();
        let mut value = proof(&prepared, Some("readiness"));
        match fault {
            "baseline" => value["purpose"] = "baseline_witness".into(),
            "bounded_source" | "bounded_readiness" => {
                if fault == "bounded_source" { value["purpose"] = "source_selection".into(); }
                value["native_input_hash"] = "c".repeat(64).into();
                value["scope_context_hash"] = "d".repeat(64).into();
            }
            "legacy" => value["purpose"] = serde_json::Value::Null,
            "unknown" => value["purpose"] = "unknown_new_role".into(),
            _ => {},
        }
        let name = if fault == "qualification" {"baseline-witness-existing.json".into()}
            else {format!("primary-scope-{}.json", uuid::Uuid::new_v4())};
        let path = control.path().join(name);
        let bytes = if fault == "malformed" {b"not json".to_vec()}
            else if fault == "oversize" {vec![b' '; 65537]}
            else {serde_json::to_vec(&value).unwrap()};
        let elsewhere = tempfile::tempdir_in(control.path().parent().unwrap()).unwrap();
        if matches!(fault, "symlink" | "hardlink") {
            let external = elsewhere.path().join("proof");
            write_private(&external, &bytes);
            if fault == "symlink" {std::os::unix::fs::symlink(&external, &path).unwrap();}
            else {std::fs::hard_link(&external, &path).unwrap();}
        } else {write_private(&path, &bytes);}
        if fault == "public" {std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();}
        let store = crate::autoship_workflow::WorkflowStore::open(&control.path().join("workflow.sqlite3")).unwrap();
        assert!(retained_controller_slots(root.path(), control.path(), &prepared,
            &store.inspect_verifier_evidence("owner/repo#840")).is_err(), "{fault} authorized fresh witness dispatch");
    }
}
