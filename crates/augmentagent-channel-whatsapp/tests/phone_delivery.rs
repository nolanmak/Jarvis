use augmentagent_channel_whatsapp::phone_delivery::{
    deliver_one, enqueue, Payload, SendError, Transport,
};
use augmentagent_store::{
    delivery::SendStatus, owner::ControlConversationKind, phone::PhoneOrigin, Store,
    SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use std::sync::Mutex;
struct Fake {
    calls: Mutex<Vec<Payload>>,
    fail: Mutex<Option<SendError>>,
}
#[async_trait::async_trait]
impl Transport for Fake {
    async fn lookup(&self, payload: &Payload) -> Option<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.delivery_key == payload.delivery_key)
            .then(|| "55".into())
    }
    async fn send(&self, payload: &Payload) -> Result<String, SendError> {
        assert!(payload.content.chars().count() <= 3500);
        self.calls.lock().unwrap().push(payload.clone());
        match self.fail.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok("55".into()),
        }
    }
}
fn origin(store: &Store) -> PhoneOrigin {
    let account = SurfaceAccountRef::new(
        SurfacePlatform::new("whatsapp").unwrap(),
        "device:12025550111",
    )
    .unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "12025550111@s.whatsapp.net").unwrap();
    let conversation =
        SurfaceConversationRef::new(account, "12025550111@s.whatsapp.net", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&conversation, ControlConversationKind::Direct, 0)
        .unwrap();
    PhoneOrigin {
        owner,
        conversation,
    }
}
#[tokio::test]
async fn transcript_parts_survive_restart_retry_and_never_resend_uncertain_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let origin = origin(&store);
    let text = "hello ".repeat(4000);
    enqueue(&store, &origin, "turn:answer", &text, 0).unwrap();
    enqueue(&store, &origin, "turn:answer", &text, 0).unwrap();
    assert!(enqueue(
        &store,
        &origin,
        "turn:answer",
        &(text.clone() + "changed"),
        0
    )
    .is_err());
    let rows = store
        .outbound_sends_with_key_prefix(origin.owner.account(), "phone-mirror:", &[])
        .unwrap();
    assert!(rows.len() > 1);
    drop(store);
    let store = Store::open(&path).unwrap();
    let transport = Fake {
        calls: Mutex::new(vec![]),
        fail: Mutex::new(Some(SendError::Retryable)),
    };
    assert!(deliver_one(&store, origin.owner.account(), &transport, 0)
        .await
        .unwrap());
    assert!(!deliver_one(&store, origin.owner.account(), &transport, 1)
        .await
        .unwrap());
    assert!(
        deliver_one(&store, origin.owner.account(), &transport, 2000)
            .await
            .unwrap()
    );
    {
        let calls = transport.calls.lock().unwrap();
        assert_eq!(calls[0].delivery_key, calls[1].delivery_key);
    }
    *transport.fail.lock().unwrap() = Some(SendError::Uncertain);
    assert!(
        deliver_one(&store, origin.owner.account(), &transport, 2000)
            .await
            .unwrap()
    );
    assert!(
        !deliver_one(&store, origin.owner.account(), &transport, 100000)
            .await
            .unwrap()
    );
    assert_eq!(
        store.outbound_send(rows[1].id).unwrap().unwrap().status,
        SendStatus::Reconcile
    );
    assert_eq!(transport.calls.lock().unwrap().len(), 3);
    assert!(
        augmentagent_channel_whatsapp::phone_delivery::reconcile_one(
            &store,
            origin.owner.account(),
            &transport,
            100000
        )
        .await
        .unwrap()
    );
    assert_eq!(
        store.outbound_send(rows[1].id).unwrap().unwrap().status,
        SendStatus::Sent
    );
    assert_eq!(
        transport.calls.lock().unwrap().len(),
        3,
        "reconciliation must not resend"
    );
}
#[tokio::test]
async fn changed_authority_blocks_queued_private_transcripts() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).unwrap();
    let origin = origin(&store);
    enqueue(&store, &origin, "turn", "private text", 0).unwrap();
    store.bind_surface_owner(&origin.owner, 0).unwrap();
    let transport = Fake {
        calls: Mutex::new(vec![]),
        fail: Mutex::new(None),
    };
    assert!(deliver_one(&store, origin.owner.account(), &transport, 0)
        .await
        .unwrap());
    assert!(transport.calls.lock().unwrap().is_empty());
    let rows = store
        .outbound_sends_with_key_prefix(origin.owner.account(), "phone-mirror:", &[])
        .unwrap();
    assert_eq!(rows[0].status, SendStatus::DeadLetter);
}

#[tokio::test]
async fn crash_inflight_stays_parked_when_delivery_cannot_be_proven() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = Store::open(&path).unwrap();
    let origin = origin(&store);
    enqueue(&store, &origin, "crash", "private transcript", 0).unwrap();
    let send = store
        .claim_next_outbound_send_for_prefix(origin.owner.account(), "phone-mirror:", 0)
        .unwrap()
        .unwrap();
    drop(store);
    let store = Store::open(&path).unwrap();
    store.recover_surface_delivery(1).unwrap();
    let transport = Fake {
        calls: Mutex::new(vec![]),
        fail: Mutex::new(None),
    };
    assert!(
        augmentagent_channel_whatsapp::phone_delivery::reconcile_one(
            &store,
            origin.owner.account(),
            &transport,
            1
        )
        .await
        .unwrap()
    );
    assert!(
        !augmentagent_channel_whatsapp::phone_delivery::reconcile_one(
            &store,
            origin.owner.account(),
            &transport,
            2
        )
        .await
        .unwrap()
    );
    assert!(
        !deliver_one(&store, origin.owner.account(), &transport, 100000)
            .await
            .unwrap()
    );
    assert_eq!(
        store.outbound_send(send.id).unwrap().unwrap().status,
        SendStatus::Reconcile
    );
    assert!(transport.calls.lock().unwrap().is_empty());
}

struct NoDial;
#[async_trait::async_trait]
impl augmentagent_phone::Telephony for NoDial {
    async fn create(
        &self,
        _: augmentagent_phone::DialRequest,
    ) -> Result<String, augmentagent_phone::ProviderError> {
        panic!("transcript delivery must not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), augmentagent_phone::ProviderError> {
        panic!("transcript delivery must not dial")
    }
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
async fn actual_transport_checks_gates_and_parks_unknown_sidecar_results() {
    use augmentagent_channel_whatsapp::{phone_delivery::WhatsappTransport, Jid, WaClient};
    use augmentagent_phone::{
        tools::ToolServer, OwnerLink, PhoneConfig, PhoneService, SystemClock,
    };
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let _env = Env(std::env::var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED").ok());
    std::env::set_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", "0");
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let origin = origin(&store);
    store
        .allow_whatsapp_outbound(origin.owner.sender_id())
        .unwrap();
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(NoDial),
        Arc::new(SystemClock),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "fixture-secret-longer-than-32-bytes",
            vec![OwnerLink {
                surface_owner: origin.owner.clone(),
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
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = calls.clone();
    let server = tokio::spawn(async move {
        let (read, mut write) = peer.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(req["params"]["account"], "12025550111@s.whatsapp.net");
            assert!(req["params"]["delivery_key"]
                .as_str()
                .is_some_and(|key| !key.is_empty()));
            if req["op"] == "lookup_delivery" {
                let response = serde_json::json!({"version":1,"request_id":req["request_id"],"ok":true,"result":{"sent":true,"message_id":"delivered-two"}});
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
                continue;
            }
            assert_eq!(req["op"], "send_text");
            assert_eq!(req["params"]["chat_jid"], "12025550111@s.whatsapp.net");
            let index = count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response = if index == 0 {
                serde_json::json!({"version":1,"request_id":req["request_id"],"ok":true,"result":{"message_id":"delivered-one"}})
            } else {
                serde_json::json!({"version":1,"request_id":req["request_id"],"ok":false,"error":{"kind":"SendFailed","message":"outcome unknown"}})
            };
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let transport = WhatsappTransport::new(
        client,
        store.clone(),
        tools,
        Jid::new(origin.owner.sender_id()),
    )
    .unwrap();
    enqueue(&store, &origin, "one", "Phone — you: hello", 0).unwrap();
    let rows = store
        .outbound_sends_with_key_prefix(origin.owner.account(), "phone-mirror:", &[])
        .unwrap();
    let payload: Payload = serde_json::from_str(&rows[0].payload).unwrap();
    assert!(matches!(
        transport.send(&payload).await,
        Err(SendError::Retryable)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    std::env::set_var("AUGMENTAGENT_WHATSAPP_CONTROL_ENABLED", "1");
    assert!(deliver_one(&store, origin.owner.account(), &transport, 0)
        .await
        .unwrap());
    assert_eq!(
        store.outbound_send(rows[0].id).unwrap().unwrap().status,
        SendStatus::Sent
    );
    enqueue(&store, &origin, "two", "Phone — Jarvis: reply", 1).unwrap();
    assert!(deliver_one(&store, origin.owner.account(), &transport, 1)
        .await
        .unwrap());
    assert!(
        !deliver_one(&store, origin.owner.account(), &transport, 100000)
            .await
            .unwrap()
    );
    assert!(
        augmentagent_channel_whatsapp::phone_delivery::reconcile_one(
            &store,
            origin.owner.account(),
            &transport,
            100000
        )
        .await
        .unwrap()
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let rows = store
        .outbound_sends_with_key_prefix(origin.owner.account(), "phone-mirror:", &[])
        .unwrap();
    assert!(rows.iter().all(|row| row.status == SendStatus::Sent));
    store
        .deny_whatsapp_outbound(origin.owner.sender_id())
        .unwrap();
    assert!(matches!(
        transport.send(&payload).await,
        Err(SendError::Rejected)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    server.abort();
}
