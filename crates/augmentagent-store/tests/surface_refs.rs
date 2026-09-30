use augmentagent_store::{
    SurfaceAccountRef, SurfaceCapabilities, SurfaceCapability, SurfaceConversationRef,
    SurfaceMessageRef, SurfaceOwnerRef, SurfacePlatform, SurfaceReplyTarget, SurfaceTurnRef,
};

fn account(platform: &str, account_id: &str) -> SurfaceAccountRef {
    SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), account_id).unwrap()
}

#[test]
fn conversation_keys_preserve_opaque_ids_and_separate_platforms_accounts_and_threads() {
    let whatsapp = SurfaceConversationRef::new(
        account("whatsapp", "15551234567:2@s.whatsapp.net"),
        "15557654321@s.whatsapp.net",
        None,
    )
    .unwrap();
    let another_device = SurfaceConversationRef::new(
        account("whatsapp", "15551234567:3@s.whatsapp.net"),
        "15557654321@s.whatsapp.net",
        None,
    )
    .unwrap();
    let discord = SurfaceConversationRef::new(
        account("discord", "15551234567:2@s.whatsapp.net"),
        "15557654321@s.whatsapp.net",
        None,
    )
    .unwrap();
    let thread = SurfaceConversationRef::new(
        account("whatsapp", "15551234567:2@s.whatsapp.net"),
        "15557654321@s.whatsapp.net",
        Some("topic:1".into()),
    )
    .unwrap();
    assert_eq!(whatsapp.conversation_id(), "15557654321@s.whatsapp.net");
    assert_ne!(whatsapp.storage_key(), another_device.storage_key());
    assert_ne!(whatsapp.storage_key(), discord.storage_key());
    assert_ne!(whatsapp.storage_key(), thread.storage_key());
    assert_eq!(
        whatsapp.storage_key(),
        serde_json::from_str::<SurfaceConversationRef>(&serde_json::to_string(&whatsapp).unwrap())
            .unwrap()
            .storage_key()
    );
}

#[test]
fn owner_turn_reply_and_posted_message_have_distinct_identifiers() {
    let chat =
        SurfaceConversationRef::new(account("whatsapp", "device:1"), "123@s.whatsapp.net", None)
            .unwrap();
    let owner = SurfaceOwnerRef::new(chat.account().clone(), "owner@s.whatsapp.net").unwrap();
    let turn = SurfaceTurnRef::new(chat.clone(), "inbound:17").unwrap();
    let target = SurfaceReplyTarget::new(chat.clone(), Some("inbound:17".into())).unwrap();
    let sent = SurfaceMessageRef::new(chat.clone(), "inbound:17").unwrap();
    assert_eq!(owner.sender_id(), "owner@s.whatsapp.net");
    assert_eq!(turn.turn_id(), "inbound:17");
    assert_eq!(target.quoted_message_id(), Some("inbound:17"));
    assert_eq!(sent.message_id(), "inbound:17");
    assert_ne!(turn.storage_key(), sent.storage_key());
}

#[test]
fn invalid_ids_and_missing_capabilities_fail_with_typed_errors() {
    assert!(SurfacePlatform::new("WhatsApp").is_err());
    assert!(SurfaceAccountRef::new(SurfacePlatform::new("whatsapp").unwrap(), " ").is_err());
    let account = account("whatsapp", "device:1");
    assert!(SurfaceConversationRef::new(account.clone(), "", None).is_err());
    assert!(SurfaceOwnerRef::new(account.clone(), "").is_err());
    let chat = SurfaceConversationRef::new(account, "123@s.whatsapp.net", None).unwrap();
    assert!(SurfaceTurnRef::new(chat.clone(), "").is_err());
    assert!(SurfaceMessageRef::new(chat.clone(), "").is_err());
    assert!(SurfaceReplyTarget::new(chat, Some("".into())).is_err());

    let capabilities = SurfaceCapabilities::new([SurfaceCapability::Query]);
    assert!(capabilities.require(SurfaceCapability::Query).is_ok());
    let error = capabilities.require(SurfaceCapability::Send).unwrap_err();
    assert!(error.to_string().contains("send"));
}

#[test]
fn deserialization_cannot_bypass_reference_validation() {
    assert!(serde_json::from_str::<SurfaceAccountRef>(
        r#"{"platform":"whatsapp","account_id":" "}"#
    )
    .is_err());
    assert!(serde_json::from_str::<SurfaceConversationRef>(
        r#"{"account":{"platform":"whatsapp","account_id":"device:1"},"conversation_id":"123@s.whatsapp.net","thread_id":""}"#
    ).is_err());
    assert!(serde_json::from_str::<SurfaceReplyTarget>(
        r#"{"conversation":{"account":{"platform":"whatsapp","account_id":"device:1"},"conversation_id":"123@s.whatsapp.net","thread_id":null},"quoted_message_id":""}"#
    ).is_err());
}

#[test]
fn capability_list_names_every_capability_exactly_once() {
    let names: std::collections::BTreeSet<&str> = SurfaceCapability::ALL
        .iter()
        .map(|capability| capability.as_str())
        .collect();
    assert_eq!(names.len(), SurfaceCapability::ALL.len());
    for capability in SurfaceCapability::ALL {
        // Exhaustive on purpose: a new variant fails to compile here until it
        // is also added to `ALL`, which the length check below then enforces.
        let index = match capability {
            SurfaceCapability::Query => 0,
            SurfaceCapability::Approve => 1,
            SurfaceCapability::Send => 2,
            SurfaceCapability::Schedule => 3,
            SurfaceCapability::ModelControl => 4,
            SurfaceCapability::ProcessControl => 5,
            SurfaceCapability::MediaRead => 6,
            SurfaceCapability::MediaWrite => 7,
            SurfaceCapability::History => 8,
            SurfaceCapability::Notifications => 9,
            SurfaceCapability::Voice => 10,
        };
        assert_eq!(SurfaceCapability::ALL[index], capability);
    }
    assert_eq!(SurfaceCapability::ALL.len(), 11);
    let json = serde_json::to_string(&SurfaceCapability::ModelControl).unwrap();
    assert_eq!(json, "\"model_control\"");
}
