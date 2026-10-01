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

#[test]
fn control_claim_and_reply_survive_restart_without_overwriting_an_answer() {
    let dir=tempfile::tempdir().unwrap();let path=dir.path().join("agent.db");
    {
        let store=Store::open(&path).unwrap();
        assert!(store.claim_whatsapp_control_turn("account","owner","m1").unwrap());
        assert!(!store.claim_whatsapp_control_turn("account","owner","m1").unwrap());
        assert_eq!(store.whatsapp_control_reply("account","owner","m1").unwrap(),Some(None));
        store.finish_whatsapp_control_turn("account","owner","m1","original").unwrap();
    }
    let store=Store::open(&path).unwrap();
    assert_eq!(store.whatsapp_control_reply("account","owner","m1").unwrap(),Some(Some("original".into())));
    store.finish_whatsapp_control_turn("account","owner","m1","changed").unwrap();
    assert_eq!(store.whatsapp_control_reply("account","owner","m1").unwrap(),Some(Some("original".into())));
    assert_eq!(store.whatsapp_control_reply("other","owner","m1").unwrap(),None);
    assert_eq!(store.whatsapp_control_reply("account","other","m1").unwrap(),None);
    for (chat,id,seq) in [("stranger","m2",2),("owner","m3",3)] {
        store.record_whatsapp_inbound_event("account",chat,id,seq,"{}").unwrap();
    }
    let pending=store.pending_whatsapp_control("account","owner").unwrap();
    assert_eq!(pending.len(),1);assert_eq!(pending[0].message_id,"m3");
}
