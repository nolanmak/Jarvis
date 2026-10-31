use augmentagent_store::{imessage_owner::OwnerEvent, Store};

fn event(rev: i64, row: i64) -> OwnerEvent {
    OwnerEvent {
        binding_revision: rev,
        rowid: row,
        created_at_ms: chrono::Utc::now().timestamp_millis() + 1000,
        guid: format!("fixture-{row}"),
        chat_guid: "iMessage;-;owner@example.invalid".into(),
        sender: "owner@example.invalid".into(),
        from_me: false,
        kind: "text".into(),
        target_guid: None,
        body: "approve abcdef01".into(),
    }
}
fn bind(s: &Store, cursor: i64) -> i64 {
    s.bind_imessage_owner(
        "iMessage;-;owner@example.invalid",
        "owner@example.invalid",
        false,
        cursor,
    )
    .unwrap()
    .revision
}

#[test]
fn capture_is_atomic_scoped_and_starts_after_enrollment() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 100);
    assert!(s
        .accept_imessage_owner_events(rev, 100, &[event(rev, 99)], 1)
        .is_err());
    let mut stranger = event(rev, 102);
    stranger.sender = "stranger@example.invalid".into();
    assert!(s
        .accept_imessage_owner_events(rev, 102, &[event(rev, 101), stranger], 2)
        .is_err());
    assert_eq!(s.imessage_owner_binding().unwrap().unwrap().cursor, 100);
    assert!(claim(&s, 3).unwrap().is_none());
    assert_eq!(
        s.accept_imessage_owner_events(rev, 102, &[event(rev, 101), event(rev, 102)], 4)
            .unwrap(),
        2
    );
    assert_eq!(claim(&s, 5).unwrap().unwrap().rowid, 101);
    assert!(claim(&s, 6).unwrap().is_none());
    assert!(s.finish_imessage_owner_event(&event(rev, 101), 7).unwrap());
    assert_eq!(claim(&s, 8).unwrap().unwrap().rowid, 102);
}

#[test]
fn replay_and_body_compaction_never_repeat_a_completed_command() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let e = event(rev, 1);
    assert_eq!(
        s.accept_imessage_owner_events(rev, 1, std::slice::from_ref(&e), 1)
            .unwrap(),
        1
    );
    assert_eq!(
        s.accept_imessage_owner_events(rev, 1, std::slice::from_ref(&e), 2)
            .unwrap(),
        0
    );
    let mut changed = e.clone();
    changed.body = "skip abcdef01".into();
    assert!(s
        .accept_imessage_owner_events(rev, 1, &[changed.clone()], 3)
        .is_err());
    assert_eq!(claim(&s, 4).unwrap(), Some(e.clone()));
    assert!(s.finish_imessage_owner_event(&e, 5).unwrap());
    assert_eq!(s.compact_imessage_owner_event_bodies(6).unwrap(), 1);
    assert_eq!(
        s.accept_imessage_owner_events(rev, 1, &[changed], 7)
            .unwrap(),
        0
    );
    assert!(claim(&s, 8).unwrap().is_none());
}

#[test]
fn restart_parks_claimed_work_and_blocks_later_dispatch_or_rebinding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let s = Store::open(&path).unwrap();
    let rev = bind(&s, 0);
    s.accept_imessage_owner_events(rev, 2, &[event(rev, 1), event(rev, 2)], 1)
        .unwrap();
    claim(&s, 2).unwrap().unwrap();
    drop(s);
    let s = Store::open(&path).unwrap();
    assert_eq!(s.interrupt_imessage_owner_events(3).unwrap(), 1);
    assert!(claim(&s, 4).unwrap().is_none());
    assert!(s
        .bind_imessage_owner("different", "owner@example.invalid", false, 2)
        .is_err());
    s.disable_imessage_owner().unwrap();
    assert!(s
        .accept_imessage_owner_events(rev, 3, &[event(rev, 3)], 5)
        .is_err());
    assert!(claim(&s, 6).unwrap().is_none());
}

#[test]
fn self_chat_is_quarantined_until_native_echo_reconciliation() {
    let s = Store::open(":memory:").unwrap();
    let b = s
        .bind_imessage_owner(
            "iMessage;-;owner@example.invalid",
            "owner@example.invalid",
            true,
            0,
        )
        .unwrap();
    let mut e = event(b.revision, 1);
    e.from_me = true;
    e.body = "approve abcdef01".into();
    s.accept_imessage_owner_events(b.revision, 1, &[e], 1)
        .unwrap();
    assert!(claim(&s, 2).unwrap().is_none());
}

#[test]
fn reconfiguration_invalidates_old_events_without_resetting_dedup() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 10);
    s.accept_imessage_owner_events(rev, 11, &[event(rev, 11)], 1)
        .unwrap();
    // Repeating an enabled identical binding never resets its watermark.
    assert_eq!(bind(&s, 0), rev);
    assert_eq!(s.imessage_owner_binding().unwrap().unwrap().cursor, 11);
    s.disable_imessage_owner().unwrap();
    let next = bind(&s, 20);
    assert!(next > rev);
    assert!(claim(&s, 2).unwrap().is_none());
    assert!(s
        .accept_imessage_owner_events(rev, 21, &[event(rev, 21)], 3)
        .is_err());
    assert_eq!(
        s.accept_imessage_owner_events(next, 21, &[event(next, 21)], 4)
            .unwrap(),
        1
    );
}

fn action(s: &Store) -> String {
    let e = augmentagent_store::Email {
        message_id: "imessage-compose:owner-fixture".into(),
        thread_id: Some("imessage:peer@example.invalid".into()),
        from: "peer@example.invalid".into(),
        to: "peer@example.invalid".into(),
        cc: String::new(),
        attachments: vec![],
        subject: "hello".into(),
        body: "hello".into(),
        date: "2026-10-31T00:00:00Z".into(),
        account_entity_id: Some("imessage".into()),
        platform: "imessage".into(),
        kind: "compose".into(),
    };
    s.create_imessage_compose(&e, "hello").unwrap().0
}

fn deliver(s: &Store) {
    while let Some(item) = s.claim_imessage_outbox().unwrap() {
        s.complete_imessage_outbox(
            item.id,
            &augmentagent_store::ImessageSendOutcome::Sent {
                message_guid: Some(format!("native-{}", item.id)),
            },
        )
        .unwrap();
    }
}

#[test]
fn mirrored_output_is_durable_and_disabled_bindings_cancel_queued_parts() {
    let s = Store::open(":memory:").unwrap();
    bind(&s, 0);
    assert_eq!(output(&s, "notice", "hello", None, 1).unwrap(), 1);
    assert_eq!(output(&s, "notice", "hello", None, 2).unwrap(), 0);
    assert!(output(&s, "notice", "different", None, 3).is_err());
    s.disable_imessage_owner().unwrap();
    assert!(s.claim_imessage_outbox().unwrap().is_none());
}

#[test]
fn incomplete_or_changed_presentations_cannot_authorize_an_action() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let a = action(&s);
    let r = s.imessage_owner_card_ref(&a).unwrap().unwrap();
    output(
        &s,
        "card",
        &"x".repeat(3000),
        Some((
            &a,
            "hello",
            &s.imessage_approval_digest(&a).unwrap().unwrap(),
        )),
        1,
    )
    .unwrap();
    let e = event(rev, 1);
    s.accept_imessage_owner_events(rev, 1, std::slice::from_ref(&e), 2)
        .unwrap();
    claim(&s, 3).unwrap().unwrap();
    assert!(s.lease_imessage_owner_action(&e, Some(&r), None).is_err());
    let first = s.claim_imessage_outbox().unwrap().unwrap();
    s.complete_imessage_outbox(
        first.id,
        &augmentagent_store::ImessageSendOutcome::Sent {
            message_guid: Some("part-one".into()),
        },
    )
    .unwrap();
    assert!(s
        .lease_imessage_owner_action(&e, Some(&r), Some("part-one"))
        .is_err());
    deliver(&s);
    s.revise_pending_action(&a, "new wording").unwrap();
    assert!(s.lease_imessage_owner_action(&e, Some(&r), None).is_err());
    assert_ne!(s.imessage_owner_card_ref(&a).unwrap().unwrap(), r);
}

#[tokio::test]
async fn native_owner_decision_excludes_racing_discord_writes_across_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let s = Store::open(&path).unwrap();
    let rival = Store::open(&path).unwrap();
    let rev = bind(&s, 0);
    let a = action(&s);
    let r = s.imessage_owner_card_ref(&a).unwrap().unwrap();
    output(
        &s,
        "card",
        "hello",
        Some((
            &a,
            "hello",
            &s.imessage_approval_digest(&a).unwrap().unwrap(),
        )),
        1,
    )
    .unwrap();
    deliver(&s);
    let e = event(rev, 1);
    s.accept_imessage_owner_events(rev, 1, std::slice::from_ref(&e), 2)
        .unwrap();
    claim(&s, 3).unwrap().unwrap();
    assert_eq!(
        s.lease_imessage_owner_action(&e, Some(&r), None).unwrap(),
        a
    );
    assert!(rival
        .claim_action_for_send(&a, augmentagent_store::ActionStatus::Pending, "discord")
        .is_err());
    assert!(rival.revise_pending_action(&a, "racing edit").is_err());
    assert!(rival
        .with_conn(|c| c.execute(
            "UPDATE actions SET recipientEmail='other@example.invalid' WHERE id=?1",
            [&a]
        ))
        .is_err());
    assert!(rival
        .with_conn(|c| c.execute("DELETE FROM actions WHERE id=?1", [&a]))
        .is_err());
    assert!(rival.with_conn(|c|c.execute("UPDATE emails SET accountEntityId='different' WHERE messageId=(SELECT messageId FROM actions WHERE id=?1)",[&a])).is_err());
    augmentagent_store::imessage_owner::with_owner_decision(&e, async {
        assert_eq!(
            s.get_action_with_email(&a)
                .unwrap()
                .unwrap()
                .action
                .draft_body
                .as_deref(),
            Some("hello")
        );
        assert!(s
            .claim_action_for_send(
                &a,
                augmentagent_store::ActionStatus::Pending,
                "imessage-owner"
            )
            .unwrap());
    })
    .await;
    assert!(s.finish_imessage_owner_event(&e, 4).unwrap());
    assert_eq!(
        rival
            .get_action_with_email(&a)
            .unwrap()
            .unwrap()
            .action
            .status,
        "sending"
    );
    assert!(!rival
        .claim_action_for_send(&a, augmentagent_store::ActionStatus::Pending, "discord")
        .unwrap());
}

#[test]
fn sent_guid_reconciliation_ignores_agent_echo_and_releases_real_owner_input() {
    let s = Store::open(":memory:").unwrap();
    let b = s
        .bind_imessage_owner(
            "iMessage;-;owner@example.invalid",
            "owner@example.invalid",
            true,
            0,
        )
        .unwrap();
    output(&s, "notice", "approve abcdef01", None, 1).unwrap();
    let mut echo = event(b.revision, 1);
    echo.from_me = true;
    echo.guid = "agent-echo".into();
    let mut owner = event(b.revision, 2);
    owner.from_me = true;
    s.accept_imessage_owner_events(b.revision, 2, &[echo, owner.clone()], 2)
        .unwrap();
    assert_eq!(s.reconcile_imessage_owner_echoes(3).unwrap(), 0);
    let item = s.claim_imessage_outbox().unwrap().unwrap();
    s.complete_imessage_outbox(
        item.id,
        &augmentagent_store::ImessageSendOutcome::Sent {
            message_guid: Some("agent-echo".into()),
        },
    )
    .unwrap();
    assert_eq!(s.reconcile_imessage_owner_echoes(4).unwrap(), 2);
    assert_eq!(claim(&s, 5).unwrap(), Some(owner));
}

fn output(
    s: &Store,
    key: &str,
    body: &str,
    card: Option<(&str, &str, &str)>,
    now: i64,
) -> augmentagent_store::StoreResult<usize> {
    s.enqueue_imessage_owner_output(
        s.imessage_owner_binding()?.unwrap().revision,
        key,
        body,
        card,
        now,
    )
}

#[test]
fn independent_controls_bypass_a_claimed_or_uncertain_chat_without_reordering_chat() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let mut chat = event(rev, 1);
    chat.body = "research this".into();
    let decision = event(rev, 2);
    let mut later = event(rev, 3);
    later.body = "follow up".into();
    s.accept_imessage_owner_events(rev, 3, &[chat.clone(), decision.clone(), later], 1)
        .unwrap();
    s.classify_imessage_owner_control(&decision).unwrap();
    assert_eq!(claim(&s, 2).unwrap(), Some(chat.clone()));
    assert!(claim(&s, 3).unwrap().is_none());
    assert_eq!(
        s.claim_imessage_owner_control(4).unwrap(),
        Some(decision.clone())
    );
    s.finish_imessage_owner_event(&decision, 5).unwrap();
    s.interrupt_imessage_owner_events(6).unwrap();
    assert!(claim(&s, 7).unwrap().is_none());
    assert_eq!(s.discard_imessage_owner_input("fixture-1").unwrap(), 1);
    assert_eq!(claim(&s, 8).unwrap().unwrap().rowid, 3);
    assert_eq!(
        s.accept_imessage_owner_events(rev, 3, &[chat], 9).unwrap(),
        0
    );
}

#[test]
fn history_is_bounded_paginatable_and_keeps_delivery_state_explicit() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let e = event(rev, 1);
    s.accept_imessage_owner_events(rev, 1, &[e.clone()], 1)
        .unwrap();
    claim(&s, 2).unwrap();
    s.set_imessage_owner_event_conversation(&e, "10").unwrap();
    let body = "response ".repeat(1000);
    output(&s, &format!("result:{rev}:{}", e.guid), &body, None, 3).unwrap();
    s.finish_imessage_owner_event(&e, 4).unwrap();
    let page = s.imessage_owner_history(Some(&e.guid), 0, 300).unwrap();
    assert_eq!(page["body"].as_str().unwrap().chars().count(), 300);
    assert_eq!(page["next_offset"], 300);
    let continuation = s.imessage_owner_history(Some(&e.guid), 300, 8000).unwrap();
    assert!(continuation["body"].as_str().unwrap().contains("response"));
    let h = s.imessage_owner_chat_history("10").unwrap();
    assert!(h.contains("prepared; delivery unconfirmed"));
    deliver(&s);
    assert!(s
        .imessage_owner_chat_history("10")
        .unwrap()
        .contains("delivered"));
    for item in s.list_imessage_outbox(100).unwrap() {
        assert!(item.body.chars().count() <= 2000);
    }
}

#[test]
fn a_recipient_change_or_a_rebinding_cannot_reuse_the_previous_presentation() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let a = action(&s);
    let old = s.imessage_owner_presentation(&a).unwrap().unwrap();
    s.with_conn(|c| {
        c.execute(
            "UPDATE actions SET recipientEmail='changed@example.invalid' WHERE id=?1",
            [&a],
        )
    })
    .unwrap();
    assert!(s
        .enqueue_imessage_owner_output(
            rev,
            "stale-render",
            "hello",
            Some((&a, &old.draft, &old.digest)),
            1
        )
        .is_err());
    let current = s.imessage_owner_presentation(&a).unwrap().unwrap();
    assert_ne!(old.reference, current.reference);
    s.disable_imessage_owner().unwrap();
    bind(&s, 0);
    assert_eq!(
        s.enqueue_imessage_owner_output(rev, "stale-binding", "private", None, 2)
            .unwrap(),
        0
    );
}

// Store contract tests choose the lane explicitly. Production uses the shared
// deterministic parser; the store has no dependency on approval grammar.
fn claim(s: &Store, now: i64) -> augmentagent_store::StoreResult<Option<OwnerEvent>> {
    for event in s.ready_imessage_owner_events()? {
        s.classify_imessage_owner_event(&event, false)?;
    }
    s.claim_imessage_owner_event(now)
}

#[test]
fn a_stale_status_caption_cannot_follow_a_newer_action_state() {
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let a = action(&s);
    let p = s.imessage_owner_presentation(&a).unwrap().unwrap();
    s.update_action_status(&a, augmentagent_store::ActionStatus::Sent, None, None)
        .unwrap();
    assert_eq!(
        s.enqueue_imessage_owner_state(rev, &a, "pending", &p.digest, "still pending", 1)
            .unwrap(),
        0
    );
    assert!(s.list_imessage_outbox(100).unwrap().is_empty());
}

#[tokio::test]
async fn discarding_a_reserved_control_requires_its_exact_finished_operation_receipt() {
    use augmentagent_store::approval_history::DecisionContext;
    let s = Store::open(":memory:").unwrap();
    let rev = bind(&s, 0);
    let a = action(&s);
    let p = s.imessage_owner_presentation(&a).unwrap().unwrap();
    output(&s, "card", "hello", Some((&a, "hello", &p.digest)), 1).unwrap();
    deliver(&s);
    let e = event(rev, 1);
    s.accept_imessage_owner_events(rev, 1, &[e.clone()], 2)
        .unwrap();
    claim(&s, 3).unwrap();
    s.lease_imessage_owner_action(&e, Some(&p.reference), None)
        .unwrap();
    let seq = augmentagent_store::imessage_owner::with_owner_decision(&e, async {
        let ctx = DecisionContext {
            surface: "imessage".into(),
            actor: e.sender.clone(),
            conversation: e.chat_guid.clone(),
            interaction_id: format!("{}:{}:{}", rev, e.guid, e.kind),
            revision: None,
        };
        s.begin_approval(
            &a,
            &ctx,
            "revise",
            "fixture",
            "fixture",
            "Synthetic revision",
        )
        .unwrap()
        .unwrap()
    })
    .await;
    s.interrupt_imessage_owner_events(4).unwrap();
    assert!(s.discard_imessage_owner_input(&e.guid).is_err());
    s.finish_approval(seq, "completed", "Synthetic handler completed before crash")
        .unwrap();
    assert_eq!(s.discard_imessage_owner_input(&e.guid).unwrap(), 1);
    assert!(s
        .revise_pending_action(&a, "fresh owner-authorized revision")
        .unwrap());
    assert!(
        claim(&s, 5).unwrap().is_none(),
        "the retired command never replays"
    );
}
