use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneOrigin, ReserveCall},
    NativeConversation, Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef,
    SurfacePlatform,
};
fn fixture(bound: bool) -> (tempfile::TempDir, Store, String, PhoneOrigin) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "synthetic-account")
            .unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "owner-chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    let revision = store
        .surface_owner_binding(owner.account())
        .unwrap()
        .unwrap()
        .revision;
    let origin = PhoneOrigin {
        owner,
        conversation: chat,
    };
    store
        .phone_import_destination("canonical", "+12025550123", 0)
        .unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner: "canonical",
            request_id: "one",
            conversation: &origin.conversation.storage_key(),
            now_ms: 0,
        })
        .unwrap()
        .call;
    store
        .phone_register_origin(&call.id, &origin, revision)
        .unwrap();
    store.phone_bind_provider(&call.id, "CA-test").unwrap();
    store
        .phone_provider_event(&call.id, "CA-test", CallState::AwaitingAcceptance, 1)
        .unwrap();
    store.phone_accept(&call.id, "CA-test", 2).unwrap();
    if bound {
        store
            .bind_surface_conversation(&NativeConversation {
                conversation: origin.conversation.clone(),
                provider: "claude".into(),
                native_session_id: "session-one".into(),
                cwd: "/fixture".into(),
                uncertain: false,
            })
            .unwrap();
    }
    store
        .phone_enqueue_dispatch(&call.id, "synthetic-task", "What needs my attention?")
        .unwrap();
    assert!(store
        .phone_start_dispatch(&call.id, "synthetic-task")
        .unwrap());
    store
        .phone_finish_attention_dispatch(
            &call.id,
            "synthetic-task",
            "What needs my attention?",
            "Synthetic deadline Tuesday at 5 PM.",
        )
        .unwrap();
    (dir, store, call.id, origin)
}
#[test]
fn attention_context_survives_restart_and_is_attached_only_once_to_its_conversation() {
    let (dir, store, id, origin) = fixture(false);
    drop(store);
    let store = Store::open(dir.path().join("db")).unwrap();
    let other = SurfaceConversationRef::new(
        origin.conversation.account().clone(),
        "different-chat",
        None,
    )
    .unwrap();
    assert!(store
        .phone_claim_attention_context(&other, None, "wrong-turn")
        .unwrap()
        .is_empty());
    let context = store
        .phone_claim_attention_context(&origin.conversation, None, "native-follow-up")
        .unwrap();
    assert!(context.contains("Tuesday at 5 PM"));
    assert!(store
        .phone_claim_attention_context(&origin.conversation, None, "another-turn")
        .unwrap()
        .is_empty());
    let jobs = store.phone_pending_dispatches().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, "ready");
    assert_eq!(jobs[0].call_id, id);
}
#[test]
fn a_rebound_owner_cannot_receive_prior_pending_attention_context() {
    let (_dir, store, _id, origin) = fixture(false);
    store.unbind_surface_owner(origin.owner.account()).unwrap();
    store.bind_surface_owner(&origin.owner, 10).unwrap();
    store
        .set_surface_control_conversation(&origin.conversation, ControlConversationKind::Direct, 10)
        .unwrap();
    assert!(store
        .phone_claim_attention_context(&origin.conversation, None, "new-authority-turn")
        .unwrap()
        .is_empty());
}
#[test]
fn a_replaced_native_session_cannot_receive_context_from_an_old_binding() {
    let (_dir, store, _id, origin) = fixture(true);
    assert!(store
        .phone_claim_attention_context(
            &origin.conversation,
            Some("session-two"),
            "new-session-turn"
        )
        .unwrap()
        .is_empty());
    assert!(
        store
            .phone_claim_attention_context(
                &origin.conversation,
                Some("session-one"),
                "old-session-turn"
            )
            .unwrap()
            .is_empty(),
        "discarded context cannot be resurrected"
    );
}
