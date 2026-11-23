use std::sync::Arc;

use augmentagent_approval_discord::imessage_owner::OwnerSurface;
use augmentagent_store::{
    imessage_owner::{OwnerContext, OwnerEvent},
    Store,
};

fn event(revision: i64) -> OwnerEvent {
    OwnerEvent {
        binding_revision: revision,
        rowid: 1,
        created_at_ms: 1,
        guid: "fixture-event".into(),
        chat_guid: "fixture-chat".into(),
        sender: "owner@example.invalid".into(),
        from_me: false,
        kind: "text".into(),
        target_guid: None,
        body: "fixture question".into(),
    }
}

fn main() {
    let store = Arc::new(Store::open(":memory:").unwrap());
    let first = store
        .bind_imessage_owner("fixture-chat", "owner@example.invalid", false, 0)
        .unwrap();
    store
        .enqueue_imessage_owner_output(first.revision, "fixture-output", "fixture output", None, 1)
        .unwrap();
    store.disable_imessage_owner().unwrap();
    let rejected = store
        .accept_imessage_owner_events(first.revision, 1, &[event(first.revision)], 2)
        .is_err();
    let unclaimable = store.claim_imessage_outbox().unwrap().is_none();

    let active = store
        .bind_imessage_owner("fixture-chat", "owner@example.invalid", false, 0)
        .unwrap();
    let long = "x".repeat(400);
    store.report_imessage_owner_capture_error(&long).unwrap();
    store.report_imessage_owner_runtime(true, Some(&long), 3).unwrap();
    store.report_imessage_owner_runtime(false, Some(&long), 3).unwrap();
    let status = store.imessage_owner_status(3).unwrap();
    let diagnostic_lengths = ["capture_error", "mirror_error", "dispatch_error"]
        .iter()
        .map(|key| status[*key].as_str().map(str::len).unwrap_or(0))
        .collect::<Vec<_>>();

    let context_a = OwnerContext {
        revision: active.revision,
        config_digest: "fixture-digest-a".into(),
        guild_id: 11,
        channel_id: 12,
        approval_channel_id: 13,
        owner_id: 14,
        bot_id: 15,
    };
    store.configure_imessage_owner_context(&context_a).unwrap();
    let context_b = OwnerContext {
        config_digest: "fixture-digest-b".into(),
        ..context_a.clone()
    };
    let drift_rejected = store.configure_imessage_owner_context(&context_b).is_err();
    let preserved = store.imessage_owner_context().unwrap().unwrap().config_digest == "fixture-digest-a";
    let drift_surface = OwnerSurface::for_origin(store.clone(), "fixture-digest-b".into());
    let inactive_before_reenrollment = !drift_surface.active();
    store.disable_imessage_owner().unwrap();
    let reenrolled = store
        .bind_imessage_owner("fixture-chat", "owner@example.invalid", false, 0)
        .unwrap();
    let context_b_reenrolled = OwnerContext {
        revision: reenrolled.revision,
        ..context_b
    };
    store.configure_imessage_owner_context(&context_b_reenrolled).unwrap();
    let active_after_reenrollment = drift_surface.active();

    let help = augmentagent_approval_discord::imessage_owner::HELP;
    let controls = ["status", "cancel", "reset", "help"]
        .into_iter()
        .filter(|control| help.contains(control))
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!([
            [rejected, unclaimable],
            diagnostic_lengths,
            [drift_rejected, preserved, inactive_before_reenrollment, active_after_reenrollment],
            controls,
        ])
    );
}
