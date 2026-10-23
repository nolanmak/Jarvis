//! Private controls must never fall back to ordinary agent queries.
use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_whatsapp::{
    Jid, WaClient, WaMessage, WhatsappControlConfig, WhatsappControlSurface,
};
use augmentagent_store::Store;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Query(AtomicUsize);
#[async_trait]
impl QueryHandler for Query {
    async fn answer(&self, _: &AuditCtx, _: &str) -> anyhow::Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("ordinary answer".into())
    }
}

#[tokio::test]
async fn phone_controls_never_reach_query_handler_without_phone_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let listener = tokio::net::UnixListener::bind(dir.path().join("wa.sock")).unwrap();
    let (events, _receiver) = tokio::sync::mpsc::channel(8);
    let client = WaClient::connect(&dir.path().join("wa.sock"), events)
        .await
        .unwrap();
    let (_peer, _) = listener.accept().await.unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let query = Arc::new(Query(AtomicUsize::new(0)));
    // No outbound allowlist, so even an externally enabled control flag cannot
    // send to the fixture socket. No global environment mutation is needed.
    let control = WhatsappControlSurface::new(
        client,
        store,
        WhatsappControlConfig {
            control_chat_jid: "12025550111@s.whatsapp.net".into(),
        },
        None,
        Some(query.clone()),
    );
    for (index, body) in [
        "phone number set +12025550123",
        "phone number verify 123456",
        "/phone number remove",
        "phone call",
        "phone stop",
    ]
    .iter()
    .enumerate()
    {
        let message = WaMessage {
            id: format!("private-{index}"),
            chat: Jid::new("12025550111@s.whatsapp.net"),
            sender: Jid::new("12025550111@s.whatsapp.net"),
            push_name: String::new(),
            text: (*body).into(),
            timestamp: 1,
            from_me: false,
            metadata: Default::default(),
        };
        assert!(control.handle_control_message(&message).await.unwrap());
    }
    assert_eq!(query.0.load(Ordering::SeqCst), 0);
}
