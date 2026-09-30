//! #1289 — durable approval-card pointers per surface, and explicit short
//! action references for text commands. Temporary stores and synthetic
//! identifiers only.

use augmentagent_store::approval_cards::{ActionRefMatch, ApprovalCardState};
use augmentagent_store::{
    ActionStatus, Email, Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceMessageRef,
    SurfacePlatform,
};

const T0: i64 = 1_700_000_000_000;

fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let store = Store::open(&path).unwrap();
    (dir, path, store)
}

fn platform(name: &str) -> SurfacePlatform {
    SurfacePlatform::new(name).unwrap()
}

fn message(platform_name: &str, channel: &str, ts: &str) -> SurfaceMessageRef {
    let account = SurfaceAccountRef::new(platform(platform_name), "team:T00000001").unwrap();
    let conversation = SurfaceConversationRef::new(account, channel, None).unwrap();
    SurfaceMessageRef::new(conversation, ts).unwrap()
}

fn pending_action(store: &Store, n: u32) -> String {
    let email = Email {
        message_id: format!("slack:C00000009:1700000000.{n:06}"),
        thread_id: Some("C00000009".into()),
        from: "Contact Example".into(),
        to: String::new(),
        cc: String::new(),
        attachments: Vec::new(),
        subject: "Lunch next week?".into(),
        body: "Are you free for lunch next week?".into(),
        date: String::new(),
        account_entity_id: Some("slack:team:T00000009".into()),
        platform: "slack".into(),
        kind: "dm".into(),
    };
    store.upsert_email(&email).unwrap();
    store
        .log_action(
            &email.message_id,
            email.thread_id.as_deref(),
            &email.from,
            &email.subject,
            Some(&email.body),
            Some("Sure, Tuesday works."),
            ActionStatus::Pending,
        )
        .unwrap()
}

#[test]
fn pointers_are_recorded_per_surface_and_survive_a_reopen() {
    let (_dir, path, store) = temp_store();
    let action = pending_action(&store, 1);
    let first = message("slack", "D00000001", "1700000000.000100");
    let second = message("slack", "D00000001", "1700000000.000200");
    let discord = message("discord", "123456789", "987654321");
    store
        .record_approval_card(&first, &action, "d1", "pending", T0)
        .unwrap();
    store
        .record_approval_card(&second, &action, "d1", "pending", T0 + 10)
        .unwrap();
    store
        .record_approval_card(&discord, &action, "d1", "pending", T0 + 20)
        .unwrap();
    drop(store);

    // A restarted daemon reads the same pointers: nothing lives in memory.
    let store = Store::open(&path).unwrap();
    let cards = store
        .approval_cards_for_action(&platform("slack"), &action)
        .unwrap();
    assert_eq!(
        cards.iter().map(|c| c.message.clone()).collect::<Vec<_>>(),
        vec![second.clone(), first.clone()],
        "newest first, and only this surface's cards"
    );
    assert!(cards.iter().all(|c| c.state == ApprovalCardState::Live));
    assert_eq!(cards[0].draft_digest, "d1");
    assert_eq!(cards[0].rendered_status, "pending");
    assert_eq!(cards[0].posted_at_ms, T0 + 10);
    let found = store.approval_card(&first).unwrap().unwrap();
    assert_eq!(found.action_id, action);
    assert!(store
        .approval_card(&message("slack", "D00000001", "1700000000.000999"))
        .unwrap()
        .is_none());
}

#[test]
fn marking_a_card_updates_its_state_and_what_it_shows() {
    let (_dir, _path, store) = temp_store();
    let action = pending_action(&store, 2);
    let card = message("slack", "D00000001", "1700000000.000100");
    store
        .record_approval_card(&card, &action, "d1", "pending", T0)
        .unwrap();
    assert!(store
        .mark_approval_card(&card, ApprovalCardState::Settled, "d1", "sent", T0 + 5)
        .unwrap());
    let got = store.approval_card(&card).unwrap().unwrap();
    assert_eq!(got.state, ApprovalCardState::Settled);
    assert_eq!(got.rendered_status, "sent");
    assert_eq!(got.updated_at_ms, T0 + 5);
    // An unknown card is reported, not invented.
    assert!(!store
        .mark_approval_card(
            &message("slack", "D00000001", "1700000000.000555"),
            ApprovalCardState::Settled,
            "d1",
            "sent",
            T0
        )
        .unwrap());
    // Re-recording the same message (a redraw of a replaced card coming
    // back) makes it live again without duplicating it.
    store
        .record_approval_card(&card, &action, "d2", "pending", T0 + 9)
        .unwrap();
    let cards = store
        .approval_cards_for_action(&platform("slack"), &action)
        .unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0].state, ApprovalCardState::Live);
    assert_eq!(cards[0].draft_digest, "d2");
}

#[test]
fn live_cards_come_back_with_the_actions_current_truth() {
    let (_dir, _path, store) = temp_store();
    let resolved = pending_action(&store, 3);
    let pending = pending_action(&store, 4);
    let settled = pending_action(&store, 5);
    let a = message("slack", "D00000001", "1700000000.000100");
    let b = message("slack", "D00000001", "1700000000.000200");
    let c = message("slack", "D00000001", "1700000000.000300");
    let gone = message("slack", "D00000001", "1700000000.000400");
    store
        .record_approval_card(&a, &resolved, "d1", "pending", T0)
        .unwrap();
    store
        .record_approval_card(&b, &pending, "d1", "pending", T0)
        .unwrap();
    store
        .record_approval_card(&c, &settled, "d1", "pending", T0)
        .unwrap();
    store
        .record_approval_card(&gone, "no-such-action", "d1", "pending", T0)
        .unwrap();
    store
        .mark_approval_card(&c, ApprovalCardState::Settled, "d1", "sent", T0)
        .unwrap();
    store
        .mark_pending_superseded_by_ids(std::slice::from_ref(&resolved), "superseded: stale")
        .unwrap();

    let live = store.live_approval_cards(&platform("slack")).unwrap();
    let by_msg = |m: &SurfaceMessageRef| live.iter().find(|l| &l.card.message == m);
    assert!(by_msg(&c).is_none(), "settled cards are not live");
    let a_live = by_msg(&a).unwrap();
    assert_eq!(a_live.action_status.as_deref(), Some("superseded"));
    let b_live = by_msg(&b).unwrap();
    assert_eq!(b_live.action_status.as_deref(), Some("pending"));
    assert_eq!(b_live.draft_body.as_deref(), Some("Sure, Tuesday works."));
    let gone_live = by_msg(&gone).unwrap();
    assert_eq!(
        gone_live.action_status, None,
        "a deleted action reads as gone"
    );
    assert!(store
        .live_approval_cards(&platform("discord"))
        .unwrap()
        .is_empty());
}

#[test]
fn short_action_references_resolve_only_when_unambiguous() {
    let (_dir, _path, store) = temp_store();
    let id = pending_action(&store, 6);
    let short = &id[..8];
    assert_eq!(
        store.resolve_action_ref(short).unwrap(),
        ActionRefMatch::One(id.clone())
    );
    assert_eq!(
        store.resolve_action_ref(&short.to_uppercase()).unwrap(),
        ActionRefMatch::One(id.clone()),
        "references are case-insensitive"
    );
    assert_eq!(
        store.resolve_action_ref(&id).unwrap(),
        ActionRefMatch::One(id.clone())
    );
    assert_eq!(
        store.resolve_action_ref("00000000").unwrap(),
        ActionRefMatch::None
    );
    // Too short to be a deliberate reference, or not an id at all: never a
    // match, so ordinary words are not mistaken for actions.
    assert_eq!(
        store.resolve_action_ref(&id[..3]).unwrap(),
        ActionRefMatch::None
    );
    assert_eq!(
        store.resolve_action_ref("the%").unwrap(),
        ActionRefMatch::None
    );
    assert_eq!(
        store.resolve_action_ref("budget").unwrap(),
        ActionRefMatch::None
    );

    // Two actions that share a prefix are reported as ambiguous with both.
    store
        .with_conn(|c| {
            c.execute(
                "INSERT INTO actions (id, messageId, fromEmail, subject, status, createdAt, updatedAt) \
                 VALUES ('abcdef01-0000-4000-8000-000000000001', 'm1', 'x', 's', 'pending', 1, 1), \
                        ('abcdef01-0000-4000-8000-000000000002', 'm2', 'x', 's', 'pending', 1, 1)",
                [],
            )
        })
        .unwrap();
    match store.resolve_action_ref("abcdef01").unwrap() {
        ActionRefMatch::Ambiguous(ids) => assert_eq!(ids.len(), 2),
        other => panic!("expected ambiguous, got {other:?}"),
    }
}
