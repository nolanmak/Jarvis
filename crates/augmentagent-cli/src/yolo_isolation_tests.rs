//! #2218 — a turn that is not a yolo turn never runs in a yolo thread.
use augmentagent_store::yolo_threads::{override_path_for_this_thread, Registry};

fn ctx(channel: u64, guild: Option<u64>) -> augmentagent_approval_discord::AuditCtx {
    let mut ctx = augmentagent_approval_discord::AuditCtx::empty();
    ctx.channel_id = Some(serenity::model::id::ChannelId::new(channel));
    ctx.guild_id = guild;
    ctx
}

#[tokio::test]
async fn only_a_yolo_turn_may_run_in_a_yolo_thread() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(dir.path().join("yolo-threads.json"));
    registry.record(1001, 77, 1).unwrap();
    override_path_for_this_thread(Some(registry.path().to_path_buf()));

    // Voice start, the voice bridge, phone and the iMessage owner surface all
    // build their own context and never set the yolo flag.
    let refusal = crate::yolo_thread_refusal(&ctx(1001, Some(5)));
    assert_eq!(refusal.as_deref(), Some("Not available in a yolo thread."));
    // The message handler's yolo turn is let through.
    let allowed = augmentagent_approval_discord::interaction::yolo_turn(true, async {
        crate::yolo_thread_refusal(&ctx(1001, Some(5)))
    })
    .await;
    assert_eq!(allowed, None);
    // Other channels, DMs and turns with no channel are untouched.
    assert_eq!(crate::yolo_thread_refusal(&ctx(77, Some(5))), None);
    assert_eq!(crate::yolo_thread_refusal(&ctx(42, None)), None);
    assert_eq!(
        crate::yolo_thread_refusal(&augmentagent_approval_discord::AuditCtx::empty()),
        None
    );

    // An unreadable registry closes guild channels and leaves DMs alone.
    std::fs::write(registry.path(), b"{broken").unwrap();
    assert!(crate::yolo_thread_refusal(&ctx(77, Some(5))).is_some());
    assert_eq!(crate::yolo_thread_refusal(&ctx(42, None)), None);
    override_path_for_this_thread(None);
}
