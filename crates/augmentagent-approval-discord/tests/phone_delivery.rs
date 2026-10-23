use augmentagent_approval_discord::phone_delivery::{
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
            .any(|p| p.nonce == payload.nonce)
            .then(|| "55".into())
    }
    async fn send(&self, payload: &Payload) -> Result<String, SendError> {
        let request = serde_json::to_value(augmentagent_approval_discord::phone_delivery::request(
            payload,
        ))
        .unwrap();
        assert_eq!(request["nonce"], payload.nonce);
        assert!(payload.nonce.len() <= 25);
        assert!(payload.content.chars().count() <= 2000);
        assert_eq!(request["enforce_nonce"], true);
        assert_eq!(request["allowed_mentions"]["parse"], serde_json::json!([]));
        self.calls.lock().unwrap().push(payload.clone());
        match self.fail.lock().unwrap().take() {
            Some(e) => Err(e),
            None => Ok("55".into()),
        }
    }
}
fn origin(store: &Store) -> PhoneOrigin {
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "dm:11").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "33").unwrap();
    let conversation = SurfaceConversationRef::new(account, "22", None).unwrap();
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
    let text = "hello ".repeat(700);
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
        assert_eq!(calls[0].nonce, calls[1].nonce);
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
        augmentagent_approval_discord::phone_delivery::reconcile_one(
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
        augmentagent_approval_discord::phone_delivery::reconcile_one(
            &store,
            origin.owner.account(),
            &transport,
            1
        )
        .await
        .unwrap()
    );
    assert!(
        !augmentagent_approval_discord::phone_delivery::reconcile_one(
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
