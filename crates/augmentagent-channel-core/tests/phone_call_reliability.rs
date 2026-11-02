//! Incident acceptance through signed incoming HTTP/PIN, actual phone runtime,
//! AgentRouter and PhoneAgent. Only the source/native/provider peers are fake.
use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::{
    phone_agent::{ConversationIo, PhoneAgent},
    phone_attention::AttentionProvider,
    providers::ProviderKind,
    surface_conformance::RecordingNativeProvider,
    Reasoner, ReasonerOpts,
};
use augmentagent_phone::{
    inbound::InboundConfig, live_config::LiveConfig, router::AgentRouter, runtime::PhoneRuntime,
    webhook::WebhookAuth, Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError,
    Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind, phone::PhoneOrigin, Store, SurfaceAccountRef,
    SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha1::Sha1;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::Notify};
use tokio_tungstenite::{
    accept_async, connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
const PUBLIC: &str = "https://phone.example.test";
struct ClockFixture;
impl Clock for ClockFixture {
    fn now_ms(&self) -> i64 {
        1000
    }
}
struct TelephonyFixture;
#[async_trait]
impl Telephony for TelephonyFixture {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("incoming fixture must not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
#[derive(Default)]
struct Io {
    mirrors: Mutex<Vec<(String, String)>>,
}
#[async_trait]
impl ConversationIo for Io {
    async fn history(&self, _: &PhoneOrigin) -> anyhow::Result<String> {
        Ok(String::new())
    }
    async fn mirror(&self, _: &PhoneOrigin, key: &str, text: &str) -> anyhow::Result<()> {
        self.mirrors.lock().unwrap().push((key.into(), text.into()));
        Ok(())
    }
    fn audit_context(&self, _: &PhoneOrigin, id: &str) -> AuditCtx {
        let mut ctx = AuditCtx::empty();
        ctx.owner_authorized = true;
        ctx.session_id = id.into();
        ctx
    }
}
struct Source {
    started: Arc<Notify>,
    reads: AtomicUsize,
}
#[async_trait]
impl AttentionProvider for Source {
    async fn check(&self, origin: &PhoneOrigin) -> anyhow::Result<Option<String>> {
        assert_eq!(origin.conversation.conversation_id(), "owner-chat");
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        tokio::time::sleep(Duration::from_secs(2)).await;
        Ok(Some("The synthetic project deadline is Tuesday at 5 PM. Calendar and inbox checked just now; calendar covers the next three days.".into()))
    }
}
struct Native {
    started: Arc<Notify>,
    release: Notify,
    calls: AtomicUsize,
    provider: RecordingNativeProvider,
}
#[async_trait]
impl QueryHandler for Native {
    async fn answer(&self, ctx: &AuditCtx, prompt: &str) -> anyhow::Result<String> {
        assert!(ctx.owner_authorized);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        // Existing behavior sends the ordinary attention check through this
        // slow path. A passing implementation bypasses it for the first answer.
        if !prompt.contains("Tuesday at 5 PM") {
            self.release.notified().await;
        }
        assert!(
            prompt.contains("Tuesday at 5 PM"),
            "the native follow-up must receive the facts already supplied by the fast read path"
        );
        let mut opts = ReasonerOpts::pinned(
            augmentagent_channel_core::ModelTier::Fast,
            "Synthetic native fixture",
        );
        opts.session_id = Some(ctx.session_id.clone());
        self.provider.call(&opts, prompt).await?;
        Ok("The deadline is Tuesday at 5 PM.".into())
    }
}
fn signature(path: &str, fields: &[(String, String)]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(b"synthetic-token").unwrap();
    let origin=if path.starts_with("/phone/relay/") {PUBLIC.replacen("https://","wss://",1)} else {PUBLIC.into()};
    mac.update(format!("{origin}{path}").as_bytes());
    let mut fields = fields.to_vec();
    fields.sort();
    for (k, v) in fields {
        mac.update(k.as_bytes());
        mac.update(v.as_bytes());
    }
    STANDARD.encode(mac.finalize().into_bytes())
}
fn wire(value: Value) -> Message {
    Message::Text(value.to_string().into())
}
#[tokio::test]
async fn incoming_attention_check_speaks_facts_before_slow_native_work_and_keeps_followup_context()
{
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "synthetic-account")
            .unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "owner-chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("canonical", "+12025550123", 0)
        .unwrap();
    let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", peer.local_addr().unwrap());
    let config = PhoneConfig::new(
        true,
        PUBLIC,
        "synthetic-verification-key-at-least-32-bytes",
        vec![OwnerLink {
            surface_owner: owner,
            owner_id: "canonical".into(),
        }],
    )
    .unwrap()
    .with_inbound(Some(
        InboundConfig::new("+12025550456", "slack", "synthetic-account", "4831").unwrap(),
    ))
    .unwrap()
    .with_live(Some(LiveConfig::for_test(endpoint)));
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(TelephonyFixture),
        Arc::new(ClockFixture),
        config,
    ));
    let started = Arc::new(Notify::new());
    let source = Arc::new(Source {
        started: started.clone(),
        reads: AtomicUsize::new(0),
    });
    let native = Arc::new(Native {
        started: started.clone(),
        release: Notify::new(),
        calls: AtomicUsize::new(0),
        provider: RecordingNativeProvider::new(ProviderKind::Claude),
    });
    let io = Arc::new(Io::default());
    let bridge = Arc::new(
        PhoneAgent::new(
            store.clone(),
            service.clone(),
            native.clone(),
            io.clone(),
            dir.path().into(),
            Arc::new(|_| Ok(Some(ProviderKind::Claude))),
        )
        .with_attention_provider(source.clone()),
    );
    let router = AgentRouter::new(store.clone());
    let _registration = router.register("slack", bridge).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = format!("http://{}", listener.local_addr().unwrap());
    let runtime = PhoneRuntime::start(
        service.clone(),
        Arc::new(WebhookAuth::new("AC-test", "synthetic-token")),
        router,
        listener,
    )
    .await
    .unwrap();
    let peer_task = tokio::spawn(async move {
        let (tcp, _) = peer.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await.unwrap().unwrap();
        ws.send(wire(json!({"type":"session.started","session":{"id":"live-integration","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}))).await.unwrap();
        for v in [
            json!({"type":"session.input_transcript.delta","event_id":"user-1a","delta":"Anything important ","start_ms":0,"end_ms":50}),
            json!({"type":"session.input_transcript.delta","event_id":"user-1b","delta":"I'm missing right now?","start_ms":50,"end_ms":100}),
            json!({"type":"session.delegation.created","event_id":"delegated-1","offset_ms":110,"delegation":{"id":"del-1","type":"delegation","target":"client"}}),
        ] {
            ws.send(wire(v)).await.unwrap();
        }
        let mut answered = 0;
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let v: Value = serde_json::from_str(&text).unwrap();
            if v["type"] == "session.close" {
                ws.send(wire(json!({"type":"session.closed","reason":"close_requested","session":{"id":"live-integration"},"usage":{"seconds":3.}}))).await.unwrap();
                break;
            }
            if v["type"] != "session.commentary.append" {
                continue;
            }
            assert!(
                v["content"].as_str().unwrap().contains("Tuesday at 5 PM"),
                "only useful verified facts count as the result"
            );
            ws.send(wire(
                json!({"type":"session.commentary.appended","client_event_id":v["event_id"]}),
            ))
            .await
            .unwrap();
            ws.send(wire(json!({"type":"session.output_audio.delta","delta":STANDARD.encode([140+answered;160])}))).await.unwrap();
            answered += 1;
            if answered == 1 {
                for v in [
                    json!({"type":"session.input_transcript.delta","event_id":"user-2","delta":"Which day was that deadline?","start_ms":200,"end_ms":300}),
                    json!({"type":"session.delegation.created","event_id":"delegated-2","offset_ms":310,"delegation":{"id":"del-2","type":"delegation","target":"client"}}),
                ] {
                    ws.send(wire(v)).await.unwrap();
                }
            }
        }
        assert_eq!(answered, 2);
    });
    let client = reqwest::Client::new();
    let fields: Vec<(String, String)> = [
        ("AccountSid", "AC-test"),
        ("CallSid", "CA00000000000000000000000000000001"),
        ("From", "+12025550123"),
        ("To", "+12025550456"),
        ("Direction", "inbound"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect();
    let incoming = client
        .post(format!("{local}/phone/incoming"))
        .header("x-twilio-signature", signature("/phone/incoming", &fields))
        .form(&fields)
        .send()
        .await
        .unwrap();
    assert!(incoming.status().is_success());
    let call = store.phone_inbound_by_sid("CA00000000000000000000000000000001").unwrap();
    let path = format!("/phone/incoming/{}", call.id);
    let mut pin = fields.clone();
    pin.push(("Digits".into(), "4831".into()));
    let response = client
        .post(format!("{local}{path}"))
        .header("x-twilio-signature", signature(&path, &pin))
        .form(&pin)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(response.contains("<Stream "));
    let address = local.replacen("http://", "ws://", 1) + &format!("/phone/relay/{}", call.id);
    let mut request = address.into_client_request().unwrap();
    let sig = signature(&format!("/phone/relay/{}", call.id), &[]);
    request
        .headers_mut()
        .insert("x-twilio-signature", sig.parse().unwrap());
    let (mut ws, _) = connect_async(request).await.unwrap();
    ws.send(wire(
        json!({"event":"connected","protocol":"Call","version":"1.0.0"}),
    ))
    .await
    .unwrap();
    ws.send(wire(json!({"event":"start","sequenceNumber":"1","streamSid":"MZ-test","start":{"accountSid":"AC-test","callSid":"CA00000000000000000000000000000001","streamSid":"MZ-test","tracks":["inbound"],"mediaFormat":{"encoding":"audio/x-mulaw","sampleRate":8000,"channels":1}}}))).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), started.notified())
        .await
        .expect("source or native work must start");
    let start = tokio::time::Instant::now();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::time::resume();
    for expected in [140, 141] {
        let frame=tokio::time::timeout(Duration::from_secs(1),ws.next()).await.expect("useful attention facts must reach the call by three seconds without waiting for full native completion").unwrap().unwrap();
        assert!(
            matches!(frame, Message::Text(_)),
            "call must remain connected"
        );
        let v: Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
        assert_eq!(v["media"]["payload"], STANDARD.encode([expected; 160]));
        let _ = ws.next().await.unwrap().unwrap();
        if expected == 140 {
            assert!(start.elapsed() < Duration::from_secs(3));
        }
    }
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        native.calls.load(Ordering::SeqCst),
        1,
        "only the follow-up needs native reasoning"
    );
    assert!(!store.phone_provider_call(&call.id).unwrap().stop_requested);
    ws.send(wire(json!({"event":"stop","sequenceNumber":"2","streamSid":"MZ-test","stop":{"accountSid":"AC-test","callSid":"CA00000000000000000000000000000001"}}))).await.unwrap();
    peer_task.await.unwrap();
    runtime.shutdown().await.unwrap();
    assert!(io
        .mirrors
        .lock()
        .unwrap()
        .iter()
        .any(|(_, s)| s.contains("Tuesday at 5 PM")));
}
