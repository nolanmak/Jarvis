use augmentagent_channel_whatsapp::{Jid, WaMessage};
use serde_json::json;

#[test]
fn self_chat_requires_transport_classification_and_exact_linked_owner() {
    let owner = Jid::new("12025550111:3@s.whatsapp.net");
    let base = json!({"id":"human-command", "chat":"12025550111@s.whatsapp.net",
        "sender":"12025550111:2@s.whatsapp.net", "text":"phone call", "timestamp":1,
        "from_me":true, "agent_outbound":false});
    let parse = |value| serde_json::from_value::<WaMessage>(value).unwrap();
    assert!(parse(base.clone()).is_verified_self_command(&owner));
    for (field, value) in [
        ("agent_outbound", json!(true)),
        ("agent_outbound", json!(null)),
        ("from_me", json!(false)),
        ("sender", json!("12025550112@s.whatsapp.net")),
        ("chat", json!("12025550112@s.whatsapp.net")),
        ("chat", json!("group@g.us")),
        ("id", json!("")),
        ("text", json!(" ")),
        ("is_edit", json!(true)),
        ("is_revoke", json!(true)),
    ] {
        let mut changed = base.clone();
        changed[field] = value;
        assert!(
            !parse(changed).is_verified_self_command(&owner),
            "accepted invalid {field}"
        );
    }
    let mut legacy = base.clone();
    legacy.as_object_mut().unwrap().remove("agent_outbound");
    assert!(!parse(legacy).is_verified_self_command(&owner));
    assert!(!parse(base).is_verified_self_command(&Jid::new("12025550111@g.us")));
}
