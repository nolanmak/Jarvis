//! Native charge-boundary contract: pending lineage cannot create a fresh budget.
//! Fixture SQL represents T's future protected pending-family transaction; no
//! policy is fabricated and no provider/model process is launched by this test.
use super::*;

#[test]
fn pending_family_without_policy_fences_new_admission_and_retains_original_spend() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let root_id = "owner/repo#1725";
    let child_id = "owner/repo#1796";
    let other_id = "owner/repo#1797";
    let binding = Binding { head: "a".repeat(40), contract: "c".repeat(64), evidence_hash: "e".repeat(64) };
    for id in [root_id, child_id, other_id] {
        store.register(id, "fixture", binding.clone(), 0).unwrap();
    }
    let root = store.claim(root_id, "original-parent", 1, 1_000).unwrap().unwrap();
    let child = store.claim(child_id, "existing-child", 2, 1_000).unwrap().unwrap();
    for index in 0..12 {
        store.charge_admitted_model(&root, &format!("original-native-{index}"), 3 + index,
            None, None, Some(submission::Provider::Codex), false).unwrap();
    }
    store.charge_admitted_model(&child, "independent-child-before-join", 16,
        None, None, Some(submission::Provider::Codex), false).unwrap();
    let before = store.snapshot(child_id).unwrap().unwrap();
    assert_eq!(before.model_calls, 1, "fixture never reached the real admission boundary");

    // Pending state is written before T can expose inherited productive work.
    // This is contract fixture data, not an implemented T publication service.
    store.conn.execute_batch("CREATE TABLE workflow_family_budget (
        family TEXT PRIMARY KEY, root_workflow TEXT NOT NULL, repository TEXT NOT NULL,
        original_request_ref TEXT NOT NULL, expected_children INTEGER NOT NULL,
        state TEXT NOT NULL, manifest_ref TEXT, policy_ref TEXT);
        CREATE TABLE workflow_family_member (
        family TEXT NOT NULL, workflow TEXT NOT NULL, position INTEGER NOT NULL,
        PRIMARY KEY(family,workflow), UNIQUE(family,position));").unwrap();
    store.conn.execute("INSERT INTO workflow_family_budget VALUES ('fixture-family',?1,'owner/repo',
        'protected-original-request',2,'pending',NULL,NULL)", [root_id]).unwrap();
    for (position, id) in [root_id, child_id, other_id].iter().enumerate() {
        store.conn.execute("INSERT INTO workflow_family_member VALUES ('fixture-family',?1,?2)",
            params![id, position as i64]).unwrap();
    }
    let admission = store.charge_admitted_model(&child, "new-after-pending-family", 18,
        None, None, Some(submission::Provider::Codex), false);
    assert!(admission.is_err(), "pending family with no owner policy admitted a new model call");
    assert_eq!(store.snapshot(child_id).unwrap().unwrap().model_calls, 1,
        "refused admission created a charge or reset joined child spend");
    assert_eq!(store.snapshot(root_id).unwrap().unwrap().model_calls, 12,
        "decomposition discarded the original twelve admissions");
    assert_eq!(store.snapshot(other_id).unwrap().unwrap().model_calls, 0);
    assert_eq!(store.merge_allowance(18, 5).unwrap().available, 5);
}
