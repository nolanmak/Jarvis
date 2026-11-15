use super::*;

const DAY: i64 = 86_400_000;
const MIN: i64 = 60_000;

fn fixture(store: &mut WorkflowStore, pr: u64, now: i64) -> (Lease, Operation) {
    let id = format!("owner/repo#{pr}");
    let binding = Binding { head: "a".repeat(40), contract: "contract".into(), evidence_hash: "verified-tree".into() };
    store.register(&id, &format!("agent-fix/issue-{pr}"), binding.clone(), now).unwrap();
    let lease = store.claim(&id, "fixture", now, DAY * 3).unwrap().unwrap();
    store.attach_pr(&lease, pr, now).unwrap();
    let op = store.begin_operation(&lease, OperationKind::MergePr, &binding, now).unwrap();
    (lease, op)
}

fn receipt(op: &Operation, pr: u64, at: i64) -> serde_json::Value {
    serde_json::json!({"client_mutation_id":op.id,"expected_head":op.binding.head,
        "pr_number":pr,"pr_node_id":format!("PR_{pr}"),"merge_commit":"b".repeat(40),
        "merged_by":"owner","merged_at":chrono::DateTime::from_timestamp_millis(at).unwrap().to_rfc3339()})
}

#[test]
fn only_dispatched_unconfirmed_merges_are_ambiguous_holds_and_survive_restart_and_midnight() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.db");
    let mut store = WorkflowStore::open(&path).unwrap();
    let t = 100 * DAY - 10 * MIN; // ten minutes before UTC midnight
    // Live reservation that never dispatched: capacity held, not ambiguous.
    let (l1, o1) = fixture(&mut store, 1, t);
    store.reserve_merge(&l1, &o1, "owner/repo", 1, "PR_1", t, 5).unwrap();
    // Dispatched long after it was reserved: ambiguity starts at dispatch.
    let (l2, o2) = fixture(&mut store, 2, t);
    store.reserve_merge(&l2, &o2, "owner/repo", 2, "PR_2", t, 5).unwrap();
    store.dispatch_merge(&l2, &o2, t + 9 * MIN, 5).unwrap();
    // Dispatched and confirmed: a shipment, not a hold.
    let (l3, o3) = fixture(&mut store, 3, t);
    store.reserve_merge(&l3, &o3, "owner/repo", 3, "PR_3", t, 5).unwrap();
    store.dispatch_merge(&l3, &o3, t + MIN, 5).unwrap();
    store.confirm_merge_dispatch(&o3, &receipt(&o3, 3, t + 2 * MIN), t + 2 * MIN).unwrap();
    drop(store);

    let store = WorkflowStore::open(&path).unwrap();
    let holds = store.unresolved_merge_holds().unwrap();
    assert_eq!(holds.len(), 1, "{holds:?}");
    let hold = &holds[0];
    assert_eq!((hold.operation.as_str(), hold.workflow.as_str(), hold.repository.as_str(), hold.pr),
        (o2.id.as_str(), "owner/repo#2", "owner/repo", 2));
    assert_eq!((hold.reserved_at_ms, hold.dispatched_at_ms), (t, t + 9 * MIN));
    assert_eq!(hold.head, "a".repeat(40));
    assert!(!hold.response_journaled);

    // Reserved twenty minutes ago but dispatched eleven: below the window.
    assert!(overdue_merge_holds(&holds, t + 20 * MIN).is_empty());
    // Crossing midnight cannot reset or hide it.
    assert_eq!(overdue_merge_holds(&holds, t + 9 * MIN + MERGE_AMBIGUITY_ESCALATION_MS).len(), 1);
    assert_eq!(overdue_merge_holds(&holds, t + DAY).len(), 1);
    // The allowance still counts it as pending in the next UTC day.
    assert!(store.merge_allowance(t + DAY, 5).unwrap().pending >= 1);
}

#[test]
fn journaled_response_is_reported_as_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
    let t = 100 * DAY;
    let (lease, op) = fixture(&mut store, 4, t);
    store.reserve_merge(&lease, &op, "owner/repo", 4, "PR_4", t, 5).unwrap();
    store.dispatch_merge(&lease, &op, t, 5).unwrap();
    store.record_merge_response(&op, r#"{"data":null,"errors":[{"message":"timeout"}]}"#, t + 1).unwrap();
    let holds = store.unresolved_merge_holds().unwrap();
    assert_eq!(holds.len(), 1);
    assert!(holds[0].response_journaled);
    let detail = merge_escalation_detail(&overdue_merge_holds(&holds, t + 16 * MIN), t + 16 * MIN);
    assert!(detail.contains("response journaled"), "{detail}");
}

#[test]
fn escalation_names_identity_dispatch_time_and_supported_resolution_command() {
    let hold = UnresolvedMergeHold { operation: "op-7".into(), workflow: "owner/repo#7".into(),
        repository: "owner/repo".into(), pr: 77, head: "c".repeat(40), reserved_at_ms: 0,
        dispatched_at_ms: 100 * DAY, response_journaled: false };
    let now = 100 * DAY + 16 * MIN;
    let detail = merge_escalation_detail(&[&hold], now);
    for expected in ["op-7", "owner/repo#77", "cccccccccccc", "dispatched 1970-04-11T00:00:00Z",
        "16 min", "no response journaled", "not confirmed shipments"] {
        assert!(detail.contains(expected), "missing {expected:?}: {detail}");
    }
    assert_eq!(merge_resolution_command("op-7", None),
        "augmentagent autopr resolve-merge --operation 'op-7' --classification <autonomous|manual|not-merged> --evidence <evidence.json>");
}

/// Review #2098: resolve-merge reads AUGMENTAGENT_AUTOPR_WORKFLOW_DB or the
/// caller's default, so the printed command must name the worker's store.
#[test]
fn resolution_command_targets_the_store_holding_the_dispatch_shell_quoted() {
    let store = std::path::Path::new("/var/lib/worker's state/autopr workflow.sqlite3");
    assert_eq!(merge_resolution_command("op-7", Some(store)),
        "AUGMENTAGENT_AUTOPR_WORKFLOW_DB='/var/lib/worker'\\''s state/autopr workflow.sqlite3' \
         augmentagent autopr resolve-merge --operation 'op-7' --classification <autonomous|manual|not-merged> --evidence <evidence.json>");
}

/// Review #2098: notifications clip the report at 1800 characters. Many
/// overdue 64-hex operations must not push the command or safety rule out.
#[test]
fn escalation_stays_within_budget_and_keeps_complete_command_and_safety_rule() {
    let holds: Vec<UnresolvedMergeHold> = (0..40).map(|n| UnresolvedMergeHold {
        operation: format!("{n:064x}"), workflow: format!("nolanmak/jarvis#{n}"),
        repository: "nolanmak/jarvis".into(), pr: 2000 + n, head: "d".repeat(40), reserved_at_ms: 0,
        dispatched_at_ms: n as i64, response_journaled: n % 2 == 0 }).collect();
    let store = std::path::Path::new("/srv/fixture-worker/autopr-workflow.sqlite3");
    let now = 100 * DAY;
    let overdue = overdue_merge_holds(&holds, now);
    let (detail, fix) = (merge_escalation_detail(&overdue, now), merge_escalation_fix(&overdue, Some(store)));
    assert!(detail.len() + fix.len() <= MERGE_ESCALATION_BUDGET, "{} chars:\n{detail}\n{fix}", detail.len() + fix.len());
    assert!(detail.starts_with("40 dispatched merge(s)") && detail.contains(&holds[0].operation) && detail.contains("more"), "{detail}");
    assert!(fix.contains(&merge_resolution_command(&holds[0].operation, Some(store))), "{fix}");
    assert!(fix.ends_with("Never replay an uncertain mutation, release its capacity without proof, or reset the ledger."), "{fix}");
}

#[test]
fn allowance_clock_line_shows_utc_day_and_local_time_with_offset() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-14T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
    let tz = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
    let merged = chrono::DateTime::parse_from_rfc3339("2026-09-14T03:30:00Z").unwrap().timestamp_millis();
    let line = allowance_clock_line(now, Some(merged), &tz);
    for expected in ["allowance day 2026-09-14 (UTC)", "resets 2026-09-14 19:00 -05:00",
        "last confirmed autonomous merge 2026-09-13 22:30 -05:00"] {
        assert!(line.contains(expected), "missing {expected:?}: {line}");
    }
    assert!(allowance_clock_line(now, None, &tz).contains("last confirmed autonomous merge: none"));
}
