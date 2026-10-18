use async_trait::async_trait;
use augmentagent_channel_whatsapp::{
    Jid, WaClient, WaMessage, WhatsappChannel, WhatsappChannelConfig, WhatsappControlConfig,
    WhatsappControlSurface,
};
use augmentagent_phone::{
    tools::ToolServer, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError,
    SystemClock, Telephony,
};
use augmentagent_store::{
    phone::CallState, Store, SurfaceAccountRef, SurfaceOwnerRef, SurfacePlatform,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[derive(Default)]
struct TextHandler(
    std::sync::atomic::AtomicUsize,
    Mutex<Option<(Arc<Store>, SurfaceOwnerRef)>>,
);
#[async_trait]
impl augmentagent_approval_discord::QueryHandler for TextHandler {
    async fn answer(
        &self,
        _: &augmentagent_approval_discord::AuditCtx,
        _: &str,
    ) -> anyhow::Result<String> {
        panic!("self-chat skipped authenticated answer_turn");
    }
    async fn answer_turn(
        &self,
        ctx: &augmentagent_approval_discord::AuditCtx,
        history: &str,
        current: &str,
    ) -> anyhow::Result<String> {
        let origin = augmentagent_phone::context::current()
            .expect("missing trusted phone origin")
            .origin;
        assert_eq!(
            origin.conversation.account().platform().as_str(),
            "whatsapp"
        );
        assert_eq!(
            origin.owner.sender_id(),
            origin.conversation.conversation_id()
        );
        assert!(
            !ctx.owner_authorized,
            "must not inherit Discord-specific authority"
        );
        assert!(history.is_empty());
        assert_eq!(current, "can you call me?");
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some((store, owner)) = self.1.lock().unwrap().take() {
            store.bind_surface_owner(&owner, 0)?;
        }
        Ok("ordinary native reply".into())
    }
}
struct NoReasoner;
#[async_trait]
impl augmentagent_channel_core::Reasoner for NoReasoner {
    async fn call(
        &self,
        _: &augmentagent_channel_core::ReasonerOpts,
        _: &str,
    ) -> anyhow::Result<String> {
        panic!("private phone command reached triage/model");
    }
}
#[derive(Default)]
struct Phone(Mutex<Vec<DialRequest>>);
#[async_trait]
impl Telephony for Phone {
    async fn create(&self, request: DialRequest) -> Result<String, ProviderError> {
        let mut calls = self.0.lock().unwrap();
        calls.push(request);
        Ok(format!("CA-fixture-{}", calls.len()))
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
fn message(id: &str, text: &str) -> WaMessage {
    serde_json::from_value(json!({"id":id,"chat":"12025550111@s.whatsapp.net","sender":"12025550111:2@s.whatsapp.net","text":text,"timestamp":1,"from_me":true,"agent_outbound":false})).unwrap()
}
struct Env(Option<String>);
impl Drop for Env {
    fn drop(&mut self) {
        match &self.0 {
            Some(v) => std::env::set_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", v),
            None => std::env::remove_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED"),
        }
    }
}

#[tokio::test]
async fn self_chat_phone_setup_verify_call_and_replay_use_shared_service() {
    let _env = Env(std::env::var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED").ok());
    std::env::set_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", "1");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let linked = Jid::new("12025550111:3@s.whatsapp.net");
    let acct = SurfaceAccountRef::new(
        SurfacePlatform::new("whatsapp").unwrap(),
        "device:12025550111",
    )
    .unwrap();
    let owner = SurfaceOwnerRef::new(acct.clone(), linked.bare()).unwrap();
    store.allow_whatsapp_outbound(&linked.bare()).unwrap();
    let provider = Arc::new(Phone::default());
    let service = Arc::new(PhoneService::new(
        store.clone(),
        provider.clone(),
        Arc::new(SystemClock),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "fixture-secret-at-least-32-bytes",
            vec![OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "owner".into(),
            }],
        )
        .unwrap(),
    ));
    let tools = ToolServer::start(service).await.unwrap();
    let listener = tokio::net::UnixListener::bind(dir.path().join("wa.sock")).unwrap();
    let (events, _rx) = tokio::sync::mpsc::channel(8);
    let client = WaClient::connect(&dir.path().join("wa.sock"), events)
        .await
        .unwrap();
    let (peer, _) = listener.accept().await.unwrap();
    let replies = Arc::new(Mutex::new(Vec::<String>::new()));
    let output = replies.clone();
    let server = tokio::spawn(async move {
        let (read, mut write) = peer.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "send_text");
            output
                .lock()
                .unwrap()
                .push(request["params"]["text"].as_str().unwrap().into());
            write.write_all(format!("{}\n",json!({"version":1,"request_id":request["request_id"],"ok":true,"result":{"message_id":"reply"}})).as_bytes()).await.unwrap();
        }
    });
    let text_handler = Arc::new(TextHandler::default());
    let make_control = |store: Arc<Store>| {
        WhatsappControlSurface::new(
            client.clone(),
            store,
            WhatsappControlConfig {
                control_chat_jid: linked.bare(),
            },
            None,
            Some(text_handler.clone()),
        )
        .with_phone_tools(linked.clone(), tools.clone())
        .unwrap()
    };
    let control = make_control(store.clone());
    let setup = message("setup", "phone number set +12025550123");
    for mode in 0..5 {
        let mut invalid = setup.clone();
        match mode {
            0 => invalid.metadata.agent_outbound = Some(true),
            1 => invalid.metadata.agent_outbound = None,
            2 => invalid.sender = Jid::new("12025550112@s.whatsapp.net"),
            3 => invalid.metadata.is_edit = true,
            _ => invalid.from_me = false,
        }
        control.handle_control_message(&invalid).await.unwrap();
    }
    assert!(provider.0.lock().unwrap().is_empty());
    std::env::set_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", "0");
    control.handle_control_message(&setup).await.unwrap();
    assert!(provider.0.lock().unwrap().is_empty());
    std::env::set_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", "1");
    store.deny_whatsapp_outbound(&linked.bare()).unwrap();
    control.handle_control_message(&setup).await.unwrap();
    assert!(provider.0.lock().unwrap().is_empty());
    store.allow_whatsapp_outbound(&linked.bare()).unwrap();
    let dry_channel = WhatsappChannel::new(
        store.clone(),
        Arc::new(NoReasoner),
        Arc::new(control.clone()),
        Some(client.clone()),
        WhatsappChannelConfig {
            dry_run: true,
            phone: linked.user().into(),
            ..Default::default()
        },
    )
    .with_control(Arc::new(control.clone()));
    dry_channel.inbox_handle().lock().await.push(setup.clone());
    dry_channel.poll_once().await.unwrap();
    assert!(provider.0.lock().unwrap().is_empty(), "dry run dialed");
    dry_channel
        .inbox_handle()
        .lock()
        .await
        .push(message("dry-text", "can you call me?"));
    dry_channel.poll_once().await.unwrap();
    assert_eq!(text_handler.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let channel = WhatsappChannel::new(
        store.clone(),
        Arc::new(NoReasoner),
        Arc::new(control.clone()),
        Some(client.clone()),
        WhatsappChannelConfig {
            dry_run: false,
            phone: linked.user().into(),
            ..Default::default()
        },
    )
    .with_control(Arc::new(control.clone()));
    assert!(store
        .surface_owner_binding(owner.account())
        .unwrap()
        .is_none());
    channel.inbox_handle().lock().await.push(setup.clone());
    let polled = channel.poll_once().await.unwrap();
    assert_eq!(polled.errors, 0);
    assert_eq!(polled.messages_dispatched, 1);
    assert_eq!(provider.0.lock().unwrap().len(), 1);
    let text = message("native-text", "can you call me?");
    channel.inbox_handle().lock().await.push(text.clone());
    assert_eq!(channel.poll_once().await.unwrap().messages_dispatched, 1);
    assert_eq!(text_handler.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let mut echo = text.clone();
    echo.metadata.agent_outbound = Some(true);
    control.handle_control_message(&echo).await.unwrap();
    assert_eq!(text_handler.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let code = provider.0.lock().unwrap()[0]
        .twiml
        .split(" is ")
        .nth(1)
        .unwrap()
        .split('.')
        .next()
        .unwrap()
        .replace(' ', "");
    control
        .handle_control_message(&message("verify", &format!("phone number verify {code}")))
        .await
        .unwrap();
    assert!(replies.lock().unwrap().last().unwrap().contains("verified"));
    let verification = store.phone_latest_call("owner").unwrap().unwrap();
    store
        .phone_provider_event(&verification.id, "CA-fixture-1", CallState::Completed, 1)
        .unwrap();
    let call = message("call", "phone call");
    control.handle_control_message(&call).await.unwrap();
    assert_eq!(provider.0.lock().unwrap().len(), 2);
    let reopened = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let restarted = make_control(reopened);
    restarted.handle_control_message(&call).await.unwrap();
    restarted.handle_control_message(&setup).await.unwrap();
    assert_eq!(provider.0.lock().unwrap().len(), 2);
    let remove = message("remove", "phone number remove");
    restarted.handle_control_message(&remove).await.unwrap();
    restarted.handle_control_message(&remove).await.unwrap();
    assert!(replies
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("already received"));
    let before = replies.lock().unwrap().len();
    *text_handler.1.lock().unwrap() = Some((store.clone(), owner.clone()));
    restarted
        .handle_control_message(&message("late-text", "can you call me?"))
        .await
        .unwrap();
    assert_eq!(
        replies.lock().unwrap().len(),
        before,
        "late reply escaped changed authority"
    );
    store.unbind_surface_owner(owner.account()).unwrap();
    restarted
        .handle_control_message(&message("revoked", "phone number set +12025550123"))
        .await
        .unwrap();
    assert_eq!(provider.0.lock().unwrap().len(), 2);
    restarted
        .handle_control_message(&message("revoked-text", "can you call me?"))
        .await
        .unwrap();
    assert_eq!(text_handler.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    for reply in replies.lock().unwrap().iter() {
        assert!(!reply.contains("12025550123") && !reply.contains(&code));
    }
    let receipts: i64 = store
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM surface_private_command_receipts",
                [],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(
        receipts, 4,
        "only setup, verify, call and remove were consumed"
    );
    server.abort();
}
