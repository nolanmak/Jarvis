//! #2370 guard: autoship never parks a row on an await only an issue edit or
//! the owner can clear. Owner-initiated waits (explicit pause, merge
//! authorization, spend) are the only human waits allowed.
use super::*;

/// Waits only the owner can clear, by design: an explicit `autopr pause`, an
/// owner-chosen PR milestone, owner approval, or a spend decision.
const OWNER_INITIATED: &[&str] = &[
    "await_owner_resume",
    "await_quiescence_or_owner_resume",
    "await_owner_approval",
    "await_owner_reply",
    "await_merge_authorization",
    "await_owner_budget_revision",
    "await_owner_budget_policy",
    "await_compatible_budget_policy",
];

/// Waits an automatic mechanism clears (retry, sweep, ladder, observation).
/// The initial-input waits are proven below, not trusted from this list.
const AUTOMATIC: &[&str] = &[
    "await_acceptance_input_change",
    "await_readiness_input_change",
    "await_original_evidence_recovery",
    "await_verifier_infrastructure_recovery",
    "await_new_execution_inputs",
    "await_capacity",
    "await_quota",
    "await_checks",
    "await_child_completion",
    "await_worker",
    "await_inactivity_retry",
    "await_publication_reconcile",
    "await_request_or_main_change",
    "await_protected_execution_scope",
    "await_authoritative_usage_evidence",
    "await_authoritative_family_state",
    "await_verified_lineage",
    "await_provenance_adoption",
    "await_independent_reviewer",
    "await_integration_parent",
    "await_ci_dependency",
    "await_hosted_evidence",
    "await_baseline",
    "await_deployment_retry",
    "await_input",
];

fn source_await_actions() -> std::collections::BTreeSet<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (at, _) in text.match_indices("\"await_") {
            let rest = &text[at + 1..];
            let end = rest.find('"').unwrap_or(0);
            let word = &rest[..end];
            if word.len() > "await_".len() && word.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
                found.insert(word.to_owned());
            }
        }
    }
    found
}

#[test]
fn every_await_action_is_classified_and_initial_holds_are_never_owner_only() {
    for action in source_await_actions() {
        assert!(
            OWNER_INITIATED.contains(&action.as_str()) || AUTOMATIC.contains(&action.as_str()),
            "{action} is a new await: give it an automatic exit (retry/ladder/sweep) and list it in AUTOMATIC, \
             or, only if the owner explicitly started it, in OWNER_INITIATED"
        );
    }
    for (reason, action) in INITIAL_INPUT_WAITS {
        assert!(!OWNER_INITIATED.contains(action), "{reason} parks on owner-only {action}");
    }
}

fn park(store: &mut WorkflowStore, id: &str, reason: &str, action: &str) -> Binding {
    let binding = Binding {
        head: "c".repeat(40),
        contract: format!("original owner request {id}"),
        evidence_hash: "evidence".into(),
    };
    store.register(id, "candidate", binding.clone(), 1).unwrap();
    let lease = store.claim(id, "first", 2, 100).unwrap().unwrap();
    let mut cp = store.snapshot(id).unwrap().unwrap().checkpoint;
    cp.stage = Stage::Scope;
    cp.evidence = vec![format!("amendment-input:{}", "e".repeat(64))];
    cp.next = NextAction { reason: reason.into(), action: action.into(), eligible_at_ms: 0 };
    store.checkpoint(&lease, &cp, 3).unwrap();
    store.release(&lease, 4).unwrap();
    binding
}

/// Every reason → await mapping an initial verifier hold can produce leaves
/// the park automatically: the next productive claim admits it, or a free
/// maintenance sweep moves it, without any issue edit or owner action.
#[test]
fn no_initial_hold_reason_maps_to_a_human_only_await() {
    for (reason, action) in INITIAL_INPUT_WAITS {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WorkflowStore::open(&dir.path().join("workflow.db")).unwrap();
        let id = "owner/repo#2361";
        let binding = park(&mut store, id, reason, action);
        assert!(awaits_initial_input(&store.snapshot(id).unwrap().unwrap().checkpoint));
        let later = 10 + terminal_parks::EVIDENCE_HOLD_TTL_MS;
        store.migrate_repair_exhausted_to_ladder("owner/repo", later).unwrap();
        store.migrate_readiness_invalid_to_ladder("owner/repo", later).unwrap();
        store.reverify_unrecoverable_original_evidence("owner/repo", later, |p| p.exists()).unwrap();
        let after = store.snapshot(id).unwrap().unwrap();
        let moved = !awaits_initial_input(&after.checkpoint);
        let admitted = store
            .claim_productive_for_binding(id, "automatic", later + 1, 100, &binding)
            .unwrap()
            .is_some();
        assert!(
            moved || admitted,
            "{reason} → {action} is a human-only park: no claim or sweep moves it"
        );
    }
}
