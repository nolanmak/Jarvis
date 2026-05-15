//! Integration test: deserialize a real-shape `getUpdates` response
//! captured from the Telegram Bot API. Guards against schema drift in
//! the wire types (`Update` / `Message` / `Chat` / `User`).

use augmentagent_channel_telegram_bot::channel::update_to_work_item;
use augmentagent_channel_telegram_bot::types::Update;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Envelope {
    ok: bool,
    result: Vec<Update>,
}

fn load_fixture() -> Envelope {
    let raw = include_str!("fixtures/telegram_update_text_dm.json");
    serde_json::from_str(raw).expect("fixture deserializes")
}

#[test]
fn fixture_envelope_parses() {
    let env = load_fixture();
    assert!(env.ok);
    assert_eq!(env.result.len(), 2);
    assert_eq!(env.result[0].update_id, 100001);
}

#[test]
fn first_update_yields_dm_work_item() {
    let env = load_fixture();
    let item = update_to_work_item(&env.result[0]).expect("first update has a message");
    assert_eq!(item.platform, "telegram");
    assert_eq!(item.kind, "dm");
    assert_eq!(item.external_id, "tg:12345:42");
    assert_eq!(
        item.payload["message"]["text"].as_str(),
        Some("hey, got 15 min tomorrow to talk through the brief?")
    );
}

#[test]
fn reply_chain_carries_reply_to_id() {
    let env = load_fixture();
    let msg = env.result[1]
        .message
        .as_ref()
        .expect("second update has a message");
    assert_eq!(msg.effective_reply_to(), Some(42));
}
