use augmentagent_store::{Store, WhatsappOwnerConfig};

#[test]
fn owner_and_control_chat_survive_restart_and_unlink_with_device() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.db");
    let store = Store::open(&path).unwrap();
    store
        .upsert_whatsapp_device(
            "15551234567",
            "15551234567:2@s.whatsapp.net",
            "15551234567@s.whatsapp.net",
        )
        .unwrap();
    store
        .set_whatsapp_owner_config(&WhatsappOwnerConfig {
            phone: "15551234567".into(),
            owner_jid: "15551234567@s.whatsapp.net".into(),
            control_chat_jid: "15551234567@s.whatsapp.net".into(),
            mode: "self_chat".into(),
        })
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    let self_chat = store.whatsapp_owner_config("15551234567").unwrap().unwrap();
    assert_eq!(self_chat.mode, "self_chat");
    store
        .set_whatsapp_owner_config(&WhatsappOwnerConfig {
            phone: "15551234567".into(),
            owner_jid: "15557654321@s.whatsapp.net".into(),
            control_chat_jid: "15557654321@s.whatsapp.net".into(),
            mode: "dedicated".into(),
        })
        .unwrap();
    assert_eq!(
        store
            .whatsapp_owner_config("15551234567")
            .unwrap()
            .unwrap()
            .mode,
        "dedicated"
    );
    store.delete_whatsapp_device("15551234567").unwrap();
    assert!(store
        .whatsapp_owner_config("15551234567")
        .unwrap()
        .is_none());
}
