//! #1700 — thread messages for the teammate-coverage rule and the durable
//! decision log. Temporary stores and synthetic addresses only.

use augmentagent_store::team_coverage::TeamCoverageDecision;
use augmentagent_store::{Email, Recipients, Store};

fn temp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    (dir, store)
}

fn email(id: &str, thread: &str, account: &str, from: &str, to: &str) -> Email {
    Email {
        message_id: id.into(),
        thread_id: Some(thread.into()),
        from: from.into(),
        to: to.into(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "s".into(),
        body: format!("body of {id}"),
        date: "2026-01-02T00:00:00Z".into(),
        account_entity_id: Some(account.into()),
        platform: "gmail".into(),
        kind: "dm".into(),
    }
}

#[test]
fn thread_messages_are_scoped_to_thread_and_mailbox_with_recipients() {
    let (_d, store) = temp_store();
    store.upsert_email(&email("m1", "T1", "acc-a", "ana@example.net", "asker@example.org")).unwrap();
    store.upsert_email(&email("m2", "T1", "acc-b", "ana@example.net", "asker@example.org")).unwrap();
    store.upsert_email(&email("m3", "T2", "acc-a", "ana@example.net", "asker@example.org")).unwrap();
    store.upsert_email(&email("m4", "T1", "acc-a", "bo@example.net", "")).unwrap();

    let mut got = store.thread_messages_in_mailbox("T1", "acc-a").unwrap();
    got.sort_by(|a, b| a.message_id.cmp(&b.message_id));
    assert_eq!(got.iter().map(|m| m.message_id.as_str()).collect::<Vec<_>>(), ["m1", "m4"]);
    assert_eq!(got[0].from, "ana@example.net");
    assert_eq!(got[0].body, "body of m1");
    assert_eq!(got[0].received_at, "2026-01-02T00:00:00Z");
    assert_eq!(
        got[0].recipients,
        Some(Recipients { to: vec!["asker@example.org".into()], cc: vec![] })
    );
    assert_eq!(got[1].recipients, None, "no headers = unknown");
}

#[test]
fn coverage_decisions_record_once_and_list_since() {
    let (_d, store) = temp_store();
    let d = TeamCoverageDecision {
        action_id: "a1".into(),
        mode: "shadow".into(),
        teammate: "Ana".into(),
        covering_message_id: "m1".into(),
        thread_id: "T1".into(),
        covered_at_ms: 1_000,
        decided_at_ms: 5_000,
    };
    assert!(store.record_team_coverage_decision(&d).unwrap());
    // A later tick never overwrites the first decision.
    let later = TeamCoverageDecision { mode: "on".into(), decided_at_ms: 9_000, ..d.clone() };
    assert!(!store.record_team_coverage_decision(&later).unwrap());
    assert_eq!(store.team_coverage_decisions(0).unwrap(), vec![d.clone()]);
    assert!(store.team_coverage_decisions(5_001).unwrap().is_empty());
}

/// One mailbox can carry several entity ids — a legacy connector id and the
/// direct-Google id that replaced it. Messages stored under either id are the
/// same mailbox, so a card raised under the old id still sees teammate
/// replies stored under the new one.
#[test]
fn mailbox_spans_every_entity_id_of_the_same_address() {
    let (_d, store) = temp_store();
    store
        .with_conn(|c| {
            c.execute_batch(
                "INSERT INTO gmail_accounts (id, connectionId, email, entityId, active, createdAt) \
                 VALUES ('old', 'conn-old', 'owner@example.com', 'legacy-1', 0, 0);",
            )
        })
        .unwrap();
    store.upsert_google_gmail_account("owner@example.com").unwrap();
    store.upsert_email(&email("m-old", "T1", "legacy-1", "asker@example.org", "owner@example.com")).unwrap();
    store
        .upsert_email(&email("m-new", "T1", "google:owner@example.com", "ana@example.net", "asker@example.org"))
        .unwrap();
    // A different mailbox with a colliding thread id stays out.
    store.upsert_email(&email("m-x", "T1", "google:other@example.com", "ana@example.net", "asker@example.org")).unwrap();

    let mut ids: Vec<String> = store
        .thread_messages_in_mailbox("T1", "legacy-1")
        .unwrap()
        .into_iter()
        .map(|m| m.message_id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["m-new", "m-old"]);
}
