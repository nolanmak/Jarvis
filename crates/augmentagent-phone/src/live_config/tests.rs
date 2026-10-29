use super::*;
use crate::{Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony};
use async_trait::async_trait;
use augmentagent_store::{
    owner::ControlConversationKind, Store, SurfaceAccountRef, SurfaceConversationRef,
    SurfaceOwnerRef, SurfacePlatform,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use tokio::net::TcpListener;
use tokio_tungstenite::{accept_async, tungstenite::Message};
struct Time;
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        100
    }
}
struct Phone(Arc<AtomicUsize>, Option<Arc<AtomicBool>>);
#[async_trait]
impl Telephony for Phone {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        if let Some(ready) = &self.1 {
            assert!(
                ready.load(Ordering::SeqCst),
                "Twilio dial preceded Live finalization"
            );
        }
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("CA-test".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
async fn service_call(
    config: LiveConfig,
    ready: Option<Arc<AtomicBool>>,
) -> (Result<Value, Error>, usize) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "account").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("canonical", "+12025550123", 0)
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let service = PhoneService::new(
        store.clone(),
        Arc::new(Phone(calls.clone(), ready)),
        Arc::new(Time),
        PhoneConfig::new(
            true,
            "https://phone.example.test",
            "synthetic-secret-at-least-32-bytes",
            vec![OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "canonical".into(),
            }],
        )
        .unwrap()
        .with_live(Some(config)),
    );
    let grant = service.authorize(&owner, &chat).unwrap();
    let result = service
        .tool(&grant, "call_owner", json!({"request_id":"one"}))
        .await;
    if result.is_err() {
        let retry = service
            .tool(&grant, "call_owner", json!({"request_id":"one"}))
            .await
            .unwrap();
        assert_eq!(
            retry["state"], "failed",
            "failed preflight must retire reservation without replaying dial"
        );
    }
    let usage = store.phone_live_usage(1).unwrap();
    assert_eq!(
        usage.len(),
        1,
        "readiness probe must retain its own usage receipt"
    );
    assert_eq!(usage[0].phase, "preflight");
    if result.is_ok() {
        assert_eq!(usage[0].seconds, Some(0.1));
        assert_eq!(usage[0].reason.as_deref(), Some("close_requested"));
    } else {
        assert_eq!(usage[0].seconds, None);
        assert_eq!(usage[0].error.as_deref(), Some("authentication"));
    }
    (result, calls.load(Ordering::SeqCst))
}
#[tokio::test]
async fn provider_rejection_prevents_twilio_dial_and_does_not_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        ws.send(Message::Text(json!({"type":"error","event_id":"evt","error":{"type":"authentication_error","message":"private credential rejected"}}).to_string().into())).await.unwrap();
    });
    let (result, calls) = service_call(LiveConfig::for_test(endpoint), None).await;
    assert!(result.is_err());
    assert_eq!(calls, 0);
    assert!(!result.err().unwrap().to_string().contains("private"));
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn preflight_finalizes_before_dialing_and_uses_no_owner_context() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
    let ready = Arc::new(AtomicBool::new(false));
    let server_ready = ready.clone();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let start: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(start["session"]["model"], "gpt-live-1");
        assert!(
            start["session"].get("input").is_none(),
            "readiness probes must not greet"
        );
        assert!(!start.to_string().contains("canonical"));
        ws.send(Message::Text(json!({"type":"session.started","event_id":"evt-start","session":{"id":"preflight-test","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}).to_string().into())).await.unwrap();
        let close: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(close["type"], "session.close");
        ws.send(Message::Text(json!({"type":"session.closed","event_id":"evt-close","reason":"close_requested","session":{"id":"preflight-test"},"usage":{"seconds":0.1}}).to_string().into())).await.unwrap();
        server_ready.store(true, Ordering::SeqCst);
    });
    let (result, calls) = service_call(LiveConfig::for_test(endpoint), Some(ready)).await;
    assert!(result.is_ok());
    assert_eq!(calls, 1);
    tokio::time::timeout(std::time::Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn configured_voice_reaches_probe_and_call_connections() {
    for (name, description) in [
        ("willow", None),
        ("voice_synthetic", Some("Warm adult feminine voice.")),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/", listener.local_addr().unwrap());
        let voice = crate::live_voice::LiveVoice::parse(name, description).unwrap();
        let expected = voice.wire();
        let server = tokio::spawn(async move {
            for greet in [false, true] {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(tcp).await.unwrap();
                let start: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                assert_eq!(start["session"]["audio"]["output"]["voice"], expected);
                assert_eq!(start["session"]["input"].is_array(), greet);
                let mut session = start["session"].clone();
                session["id"] = json!("synthetic");
                ws.send(Message::Text(
                    json!({"type":"session.started","session":session})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            }
        });
        let config = LiveConfig::for_test(endpoint).with_voice(voice);
        assert_eq!(
            config.voice_label(),
            if description.is_some() {
                "custom"
            } else {
                name
            }
        );
        drop(config.connect("Be helpful.").await.unwrap());
        drop(config.connect_for_call("Be helpful.").await.unwrap());
        server.await.unwrap();
    }
}
