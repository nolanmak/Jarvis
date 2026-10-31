use async_trait::async_trait;
use augmentagent_approval_discord::{
    imessage_owner::OwnerSurface, interaction, ApprovalActionHandler, ApprovalActionOutcome,
    AuditCtx, QueryHandler,
};
use augmentagent_store::{
    imessage_owner::{OwnerContext, OwnerEvent},
    ActionStatus, Email, ImessageSendOutcome, NewImessageOutboxItem, Store,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Runtime {
    store: Arc<Store>,
    effects: AtomicUsize,
    queries: AtomicUsize,
}
#[async_trait]
impl ApprovalActionHandler for Runtime {
    async fn approve(&self, id: &str) -> ApprovalActionOutcome {
        let context = interaction::current().expect("native actor metadata");
        assert_eq!(context.surface, "imessage");
        assert_eq!(context.actor, "owner@example.invalid");
        let seq = self
            .store
            .begin_approval(
                id,
                &context,
                "approve",
                "fixture",
                "imessage:compose",
                "Synthetic action",
            )
            .unwrap()
            .unwrap();
        assert!(self
            .store
            .claim_action_for_send(id, ActionStatus::Pending, "imessage-owner")
            .unwrap());
        self.store
            .enqueue_imessage_outbox(&NewImessageOutboxItem {
                action_id: id,
                target: "peer@example.invalid",
                target_kind: augmentagent_store::ImessageTargetKind::Handle,
                service: "iMessage",
                body: "hello",
            })
            .unwrap();
        self.effects.fetch_add(1, Ordering::SeqCst);
        self.store
            .finish_approval(seq, "completed", "Synthetic connector accepted")
            .unwrap();
        ApprovalActionOutcome::Approved
    }
    async fn revise(&self, id: &str, feedback: &str) -> ApprovalActionOutcome {
        let a = self.store.get_action_with_email(id).unwrap().unwrap();
        assert!(self.store.revise_pending_action(id, feedback).unwrap());
        ApprovalActionOutcome::Revised {
            email: a.email,
            draft: feedback.into(),
        }
    }
    async fn skip(&self, id: &str) -> ApprovalActionOutcome {
        assert!(self
            .store
            .try_resolve_action(id, ActionStatus::Rejected, "imessage-owner", None)
            .unwrap());
        ApprovalActionOutcome::Skipped
    }
    async fn schedule(&self, id: &str, at_ms: i64) -> ApprovalActionOutcome {
        assert!(self
            .store
            .schedule_action(id, at_ms, "imessage-owner")
            .unwrap());
        ApprovalActionOutcome::Scheduled {
            at_ms,
            local: "Synthetic schedule".into(),
        }
    }
    async fn back_to_queue(&self, id: &str) -> ApprovalActionOutcome {
        assert!(self.store.unschedule_action(id, "imessage-owner").unwrap());
        let row = self.store.get_action_with_email(id).unwrap().unwrap();
        ApprovalActionOutcome::Revised {
            email: row.email,
            draft: row.action.draft_body.unwrap(),
        }
    }
    async fn is_resolved(&self, _: &str) -> bool {
        false
    }
}
#[async_trait]
impl QueryHandler for Runtime {
    async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
        assert!(augmentagent_approval_discord::imessage_owner::owner_chat_active());
        self.queries.fetch_add(1, Ordering::SeqCst);
        Ok("Synthetic agent answer".into())
    }
}
fn fixture() -> (Arc<Store>, OwnerSurface, Runtime, String, Email) {
    let s = Arc::new(Store::open(":memory:").unwrap());
    let b = s
        .bind_imessage_owner("owner-chat", "owner@example.invalid", false, 0)
        .unwrap();
    s.configure_imessage_owner_context(&OwnerContext {
        revision: b.revision,
        config_digest: "fixture".into(),
        guild_id: 20,
        channel_id: 10,
        approval_channel_id: 11,
        owner_id: 42,
        bot_id: 99,
    })
    .unwrap();
    let e = Email {
        message_id: "imessage-compose:fixture".into(),
        thread_id: Some("imessage:peer@example.invalid".into()),
        from: "peer@example.invalid".into(),
        to: "peer@example.invalid".into(),
        cc: String::new(),
        attachments: vec![],
        subject: "Synthetic draft".into(),
        body: "hello".into(),
        date: "2026-10-31T00:00:00Z".into(),
        account_entity_id: Some("imessage".into()),
        platform: "imessage".into(),
        kind: "compose".into(),
    };
    let a = s.create_imessage_compose(&e, "hello").unwrap().0;
    let surface = OwnerSurface::for_origin(s.clone(), "fixture".into());
    let runtime = Runtime {
        store: s.clone(),
        effects: AtomicUsize::new(0),
        queries: AtomicUsize::new(0),
    };
    (s, surface, runtime, a, e)
}
fn deliver(s: &Store) -> String {
    let mut last = String::new();
    while let Some(item) = s.claim_imessage_outbox().unwrap() {
        last = format!("native-{}", item.id);
        s.complete_imessage_outbox(
            item.id,
            &ImessageSendOutcome::Sent {
                message_guid: Some(last.clone()),
            },
        )
        .unwrap();
    }
    last
}
fn input(s: &Store, row: i64, kind: &str, body: &str, target: Option<String>) -> OwnerEvent {
    let b = s.imessage_owner_binding().unwrap().unwrap();
    let e = OwnerEvent {
        binding_revision: b.revision,
        rowid: row,
        created_at_ms: chrono::Utc::now().timestamp_millis() + 1000,
        guid: format!("owner-input-{row}"),
        chat_guid: b.chat_guid,
        sender: b.owner_handle,
        from_me: false,
        kind: kind.into(),
        body: body.into(),
        target_guid: target,
    };
    s.accept_imessage_owner_events(b.revision, row, std::slice::from_ref(&e), row)
        .unwrap();
    e
}

#[tokio::test]
async fn thumbs_up_uses_the_shared_handler_once_and_records_real_native_authority() {
    let (s, surface, runtime, a, email) = fixture();
    surface.post_card(&a, &email, "hello").unwrap();
    let native = deliver(&s);
    let e = input(&s, 1, "like_add", "", Some(native));
    assert!(surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap());
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        s.accept_imessage_owner_events(e.binding_revision, 1, &[e], 2)
            .unwrap(),
        0
    );
    assert!(!surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap());
    assert_eq!(
        s.list_imessage_outbox(100)
            .unwrap()
            .iter()
            .filter(|o| o.action_id == a)
            .count(),
        1
    );
    let audit = s.approval_history(None, 10, Some(&a)).unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].surface, "imessage");
}

#[tokio::test]
async fn revise_keeps_action_identity_invalidates_old_card_and_needs_fresh_approval() {
    let (s, surface, runtime, a, email) = fixture();
    surface.post_card(&a, &email, "hello").unwrap();
    let old = deliver(&s);
    input(&s, 1, "text", "revise: shorter", Some(old.clone()));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(
        s.get_action_with_email(&a)
            .unwrap()
            .unwrap()
            .action
            .draft_body
            .as_deref(),
        Some("shorter")
    );
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    deliver(&s);
    input(&s, 2, "like_add", "", Some(old));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    let reference = s.imessage_owner_card_ref(&a).unwrap().unwrap();
    input(&s, 3, "text", &format!("skip {reference}"), None);
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "rejected"
    );
}

#[tokio::test]
async fn unscoped_text_and_removing_a_reaction_never_approve() {
    let (s, surface, runtime, _, _) = fixture();
    for (row, kind, body) in [
        (1, "text", "👍"),
        (2, "text", "approve"),
        (3, "like_remove", ""),
    ] {
        input(&s, row, kind, body, None);
        surface
            .dispatch_one(&runtime, &runtime, AuditCtx::empty())
            .await
            .unwrap();
    }
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.queries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unresolved_material_input_stays_visible_and_cannot_be_approved_by_reaction() {
    let (s, surface, runtime, a, email) = fixture();
    let draft = augmentagent_approval_discord::append_needs_input_marker(
        "hello",
        &[("context".into(), "Missing referenced email".into())],
    );
    s.revise_pending_action(&a, &draft).unwrap();
    surface.post_card(&a, &email, &draft).unwrap();
    assert!(s.list_imessage_outbox(100).unwrap()[0]
        .body
        .contains("Missing referenced email"));
    let native = deliver(&s);
    input(&s, 1, "like_add", "", Some(native));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "pending"
    );
}

#[tokio::test]
async fn ordinary_chat_invokes_the_existing_query_handler_once() {
    let (s, surface, runtime, _, _) = fixture();
    let e = input(&s, 1, "text", "What is next on the project?", None);
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(runtime.queries.load(Ordering::SeqCst), 1);
    assert_eq!(
        s.accept_imessage_owner_events(e.binding_revision, 1, &[e], 2)
            .unwrap(),
        0
    );
    assert!(!surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap());
    assert_eq!(runtime.queries.load(Ordering::SeqCst), 1);
    assert!(s
        .list_imessage_outbox(100)
        .unwrap()
        .iter()
        .any(|o| o.body == "Synthetic agent answer"));
}

#[test]
fn discord_output_is_owner_scoped_and_idempotent() {
    use serenity::all::{ChannelId, GuildId, Message, MessageId, UserId};
    let (s, surface, _, _, _) = fixture();
    let b = s.imessage_owner_binding().unwrap().unwrap();
    let mut m = Message::default();
    m.id = MessageId::new(((b.activated_at_ms as u64 + 1000 - 1_420_070_400_000) << 22) + 1);
    m.channel_id = ChannelId::new(15);
    m.guild_id = Some(GuildId::new(20));
    m.author.id = UserId::new(99);
    m.author.bot = true;
    m.content = "Owner-visible answer".into();
    surface.mirror_discord(&m, None, None).unwrap();
    assert!(s.list_imessage_outbox(100).unwrap().is_empty());
    surface.mirror_discord(&m, Some(10), None).unwrap();
    surface.mirror_discord(&m, Some(10), None).unwrap();
    assert_eq!(s.list_imessage_outbox(100).unwrap().len(), 1);
    m.author.id = UserId::new(100);
    m.id = MessageId::new(m.id.get() + 1);
    surface.mirror_discord(&m, Some(10), None).unwrap();
    assert_eq!(s.list_imessage_outbox(100).unwrap().len(), 1);
    m.author.id = UserId::new(99);
    m.id = MessageId::new(m.id.get() + 1);
    let health = serde_json::to_value(
        augmentagent_approval_discord::imessage_owner::health_message(1, "unknown"),
    )
    .unwrap();
    m.content = String::new();
    m.embeds = serde_json::from_value(health["embeds"].clone()).unwrap();
    surface.mirror_discord(&m, Some(10), None).unwrap();
    assert_eq!(
        s.list_imessage_outbox(100).unwrap().len(),
        1,
        "delivery health must not mirror into its own failing transport"
    );
}

#[tokio::test]
async fn scheduled_notices_require_their_own_explicit_command_and_new_reference() {
    let (s, surface, runtime, a, email) = fixture();
    surface.post_card(&a, &email, "hello").unwrap();
    let original = deliver(&s);
    input(&s, 1, "text", "schedule: in 1h", Some(original.clone()));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "scheduled"
    );
    deliver(&s);
    let scheduled=s.with_conn(|c|c.query_row("SELECT o.message_guid FROM imessage_owner_deliveries d JOIN imessage_outbox o ON o.id=d.outbox_id WHERE d.action_id=?1 AND d.origin_key LIKE 'state-card:%' ORDER BY o.id DESC LIMIT 1",[&a],|r|r.get::<_,String>(0))).unwrap();
    input(&s, 2, "like_add", "", Some(scheduled.clone()));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    input(&s, 3, "text", "back-to-queue", Some(scheduled));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "pending"
    );
    deliver(&s);
    // The first card's semantic snapshot is now identical again: its original
    // reference must still remain retired across a state round trip.
    input(&s, 4, "like_add", "", Some(original));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
}

struct WaitingQuery {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: AtomicUsize,
}
#[async_trait]
impl QueryHandler for WaitingQuery {
    async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        self.release.notified().await;
        Ok("Completed research".into())
    }
}
#[tokio::test]
async fn approvals_remain_available_during_a_long_query_and_a_large_chat_backlog() {
    let (s, surface, runtime, a, email) = fixture();
    surface.post_card(&a, &email, "hello").unwrap();
    let card = deliver(&s);
    input(&s, 1, "text", "research this", None);
    for row in 2..112 {
        input(&s, row, "text", "follow-up question", None);
    }
    input(&s, 112, "like_add", "", Some(card));
    let query = WaitingQuery {
        started: Default::default(),
        release: Default::default(),
        calls: AtomicUsize::new(0),
    };
    let chat = surface.dispatch_chat_one(&runtime, &query, AuditCtx::empty());
    tokio::pin!(chat);
    tokio::select! { _=query.started.notified()=>{}, result=&mut chat=>panic!("query should wait: {result:?}") }
    for _ in 0..3 {
        if surface
            .dispatch_control_one(&runtime, &query, AuditCtx::empty())
            .await
            .unwrap()
        {
            break;
        }
    }
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 1);
    assert_eq!(query.calls.load(Ordering::SeqCst), 1);
    query.release.notify_one();
    assert!(chat.await.unwrap());
}

#[tokio::test]
async fn native_send_completion_is_reconciled_without_a_discord_event() {
    let (s, surface, runtime, a, email) = fixture();
    surface.post_card(&a, &email, "hello").unwrap();
    let card = deliver(&s);
    input(&s, 1, "like_add", "", Some(card));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    surface.reconcile_action_states().await.unwrap();
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "sending"
    );
    deliver(&s);
    // The CLI completion adapter performs this transition after recording the
    // primitive outbox receipt; no Discord event accompanies it.
    s.update_action_status(&a, ActionStatus::Sent, None, None)
        .unwrap();
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "sent"
    );
    surface.reconcile_action_states().await.unwrap();
    let count = s.list_imessage_outbox(100).unwrap().len();
    surface.reconcile_action_states().await.unwrap();
    assert_eq!(s.list_imessage_outbox(100).unwrap().len(), count);
    let sent=s.with_conn(|c|c.query_row("SELECT o.body,d.action_id FROM imessage_owner_deliveries d JOIN imessage_outbox o ON o.id=d.outbox_id WHERE d.origin_key=?1",[format!("state:{a}:sent")],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?)))).unwrap();
    assert!(sent.0.ends_with(": sent."));
    assert!(
        sent.1.is_none(),
        "a resolution notice is not an approvable card"
    );
}

#[tokio::test]
async fn approval_files_keep_their_original_links_without_becoming_approval_targets() {
    use serenity::all::{ChannelId, GuildId, Message, MessageId, UserId};
    let (s, surface, runtime, a, email) = fixture();
    let b = s.imessage_owner_binding().unwrap().unwrap();
    let mut message = Message::default();
    message.id = MessageId::new(((b.activated_at_ms as u64 + 1000 - 1_420_070_400_000) << 22) + 1);
    message.channel_id = ChannelId::new(11);
    message.guild_id = Some(GuildId::new(20));
    message.author.id = UserId::new(99);
    message.author.bot = true;
    let card = serde_json::to_value(augmentagent_approval_discord::approval_message(
        &a, &email, "hello", 0,
    ))
    .unwrap();
    message.components = serde_json::from_value(card["components"].clone()).unwrap();
    message.attachments=serde_json::from_value(serde_json::json!([{"id":"123","filename":"review.pdf","size":10,
        "url":"https://cdn.example.invalid/review.pdf","proxy_url":"https://cdn.example.invalid/review.pdf"}])).unwrap();
    surface.mirror_discord(&message, None, None).unwrap();
    surface.mirror_discord(&message, None, None).unwrap();
    assert_eq!(s.list_imessage_outbox(100).unwrap().len(), 2);
    let file_notice = deliver(&s);
    let row = s
        .list_imessage_outbox(100)
        .unwrap()
        .into_iter()
        .find(|r| r.message_guid.as_deref() == Some(&file_notice))
        .unwrap();
    assert!(row.body.contains("https://cdn.example.invalid/review.pdf"));
    assert!(row.body.contains(&message.link()));
    input(&s, 1, "like_add", "", Some(file_notice));
    surface
        .dispatch_one(&runtime, &runtime, AuditCtx::empty())
        .await
        .unwrap();
    assert_eq!(runtime.effects.load(Ordering::SeqCst), 0);
    assert_eq!(
        s.get_action_with_email(&a).unwrap().unwrap().action.status,
        "pending"
    );
}
