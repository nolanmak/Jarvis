use augmentagent_store::Store;

#[test]
fn inbound_message_commit_survives_restart_and_deduplicates_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Mac Home ü with spaces").join("agent.db");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let event = r#"{"id":"m1","chat":"1@s.whatsapp.net","text":"hello"}"#;
    {
        let store = Store::open(&path).unwrap();
        assert!(store
            .record_whatsapp_inbound_event("15550001111", "1@s.whatsapp.net", "m1", 7, event)
            .unwrap());
    }
    let store = Store::open(&path).unwrap();
    assert!(!store
        .record_whatsapp_inbound_event("15550001111", "1@s.whatsapp.net", "m1", 7, event)
        .unwrap());
    let pending = store.list_pending_whatsapp_inbound("15550001111", 10).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].seq, 7);
    assert_eq!(pending[0].message_id, "m1");
    assert_eq!(pending[0].payload_json, event);
    store
        .mark_whatsapp_inbound_processed("15550001111", "1@s.whatsapp.net", "m1")
        .unwrap();
    assert!(store
        .list_pending_whatsapp_inbound("15550001111", 10)
        .unwrap()
        .is_empty());
    store
        .record_whatsapp_inbound_event("15550001111", "1@s.whatsapp.net", "m2", 8, event)
        .unwrap();
    store.delete_whatsapp_device("15550001111").unwrap();
    assert!(store
        .list_pending_whatsapp_inbound("15550001111", 10)
        .unwrap()
        .is_empty());
}
