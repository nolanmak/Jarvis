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
#[derive(Default)]
struct TelephonyFixture {
    creates: AtomicUsize,
}
#[async_trait]
impl Telephony for TelephonyFixture {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Ok("CA00000000000000000000000000000001".into())
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
    slow: bool,
    started: Arc<Notify>,
    release: Notify,
    calls: AtomicUsize,
    provider: RecordingNativeProvider,
}
#[async_trait]
impl QueryHandler for Native {
    async fn answer(&self, ctx: &AuditCtx, prompt: &str) -> anyhow::Result<String> {
        assert!(ctx.owner_authorized);
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        // Existing behavior sends the ordinary attention check through this
        // slow path. A passing implementation bypasses it for the first answer.
        if (self.slow && index == 0) || (!self.slow && !prompt.contains("Tuesday at 5 PM")) {
            self.release.notified().await;
        }
        assert!(
            self.slow || prompt.contains("Tuesday at 5 PM"),
            "the native follow-up must receive the facts already supplied by the fast read path"
        );
        let mut opts = ReasonerOpts::pinned(
            augmentagent_channel_core::ModelTier::Fast,
            "Synthetic native fixture",
        );
        opts.session_id = Some(ctx.session_id.clone());
        self.provider.call(&opts, prompt).await?;
        Ok(if self.slow && index == 0 {
            "The synthetic deadline is Tuesday at 5 PM. ".repeat(42)
        } else {
            "The deadline is Tuesday at 5 PM.".into()
        })
    }
}
fn signature(path: &str, fields: &[(String, String)]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(b"synthetic-token").unwrap();
    let origin = if path.starts_with("/phone/relay/") {
        PUBLIC.replacen("https://", "wss://", 1)
    } else {
        PUBLIC.into()
    };
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
async fn exercise_call(slow: bool, inbound: bool, busy: bool) {
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
    let busy_provider = Arc::new(
        RecordingNativeProvider::new(ProviderKind::Claude).holding_on("Existing blocked work"),
    );
    let busy_task = if busy {
        let task_store = store.clone();
        let task_chat = chat.clone();
        let provider = busy_provider.clone();
        let cwd = dir.path().to_string_lossy().into_owned();
        let task = tokio::spawn(async move {
            let turn = augmentagent_store::SurfaceTurnRef::new(task_chat, "busy-text").unwrap();
            augmentagent_channel_core::surface_turn::run_surface_turn(
                &task_store,
                augmentagent_channel_core::surface_turn::SurfaceTurnRequest {
                    turn: &turn,
                    history: "",
                    current: "Existing blocked work",
                    cwd: &cwd,
                },
                || Ok(Some(ProviderKind::Claude)),
                None,
                |prompt| async move {
                    let opts = ReasonerOpts::pinned(
                        augmentagent_channel_core::ModelTier::Fast,
                        "Synthetic held native turn",
                    );
                    provider.call(&opts, &prompt).await
                },
            )
            .await
            .unwrap();
        });
        busy_provider.started.notified().await;
        Some(task)
    } else {
        None
    };
    let peer = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", peer.local_addr().unwrap());
    let config = PhoneConfig::new(
        true,
        PUBLIC,
        "synthetic-verification-key-at-least-32-bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical".into(),
        }],
    )
    .unwrap()
    .with_inbound(Some(
        InboundConfig::new("+12025550456", "slack", "synthetic-account", "4831").unwrap(),
    ))
    .unwrap()
    .with_live(Some(LiveConfig::for_test(endpoint)));
    let telephony = Arc::new(TelephonyFixture::default());
    let service = Arc::new(PhoneService::new(
        store.clone(),
        telephony.clone(),
        Arc::new(ClockFixture),
        config,
    ));
    let started = Arc::new(Notify::new());
    let source = Arc::new(Source {
        started: started.clone(),
        reads: AtomicUsize::new(0),
    });
    let native = Arc::new(Native {
        slow,
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
        if !inbound {
            let (tcp, _) = peer.accept().await.unwrap();
            let mut preflight = accept_async(tcp).await.unwrap();
            let _ = preflight.next().await.unwrap().unwrap();
            preflight.send(wire(json!({"type":"session.started","session":{"id":"preflight","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}))).await.unwrap();
            let close = preflight.next().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(close.to_text().unwrap()).unwrap()["type"],
                "session.close"
            );
            preflight.send(wire(json!({"type":"session.closed","reason":"close_requested","session":{"id":"preflight"},"usage":{"seconds":0.5}}))).await.unwrap();
        }
        let (tcp, _) = peer.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await.unwrap().unwrap();
        ws.send(wire(json!({"type":"session.started","session":{"id":"live-integration","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}))).await.unwrap();
        for v in [
            json!({"type":"session.input_transcript.delta","event_id":"greeting-user","delta":"What's up Jarvis?","start_ms":0,"end_ms":10}),
            json!({"type":"session.output_transcript.delta","event_id":"greeting-assistant","delta":"Hi.","start_ms":10,"end_ms":20}),
            json!({"type":"session.input_transcript.delta","event_id":"user-1a","delta":if slow {"Research the synthetic "}else{"Anything important "},"start_ms":30,"end_ms":80}),
            json!({"type":"session.input_transcript.delta","event_id":"user-1b","delta":if slow {"project deadlines."}else{"I'm missing right now?"},"start_ms":80,"end_ms":130}),
            json!({"type":"session.delegation.created","event_id":"delegated-1","offset_ms":140,"delegation":{"id":"del-1","type":"delegation","target":"client"}}),
        ] {
            ws.send(wire(v)).await.unwrap();
        }
        let mut answered = 0;
        let mut rejected = false;
        let mut queued_acks = 0;
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let v: Value = serde_json::from_str(&text).unwrap();
            if v["type"] == "session.close" {
                ws.send(wire(json!({"type":"session.closed","reason":"close_requested","session":{"id":"live-integration"},"usage":{"seconds":3.}}))).await.unwrap();
                break;
            }
            if v["type"] == "session.thinking.append" {
                queued_acks += 1;
                assert_eq!(v["content"], "");
                continue;
            }
            if v["type"] != "session.commentary.append" {
                continue;
            }
            let useful = v["content"].as_str().unwrap().contains("Tuesday at 5 PM");
            if slow && useful && !rejected {
                rejected = true;
                ws.send(wire(json!({"type":"error","error":{"type":"invalid_request_error","code":null,"param":"delegation_id","client_event_id":v["event_id"],"message":"Unknown client or Decision delegation."}}))).await.unwrap();
                continue;
            }
            ws.send(wire(
                json!({"type":"session.commentary.appended","client_event_id":v["event_id"]}),
            ))
            .await
            .unwrap();
            // Progress and later pieces of the same result cannot stand in
            // for the second request's actual answer.
            if !useful || (answered > 0 && v["delegation_id"] != "del-2") {
                continue;
            }
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
        assert_eq!(
            queued_acks, 2,
            "exactly one application-owned quiet acknowledgment per delegation"
        );
        assert_eq!(rejected, slow);
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
    let (call, response) = if inbound {
        let incoming = client
            .post(format!("{local}/phone/incoming"))
            .header("x-twilio-signature", signature("/phone/incoming", &fields))
            .form(&fields)
            .send()
            .await
            .unwrap();
        assert!(incoming.status().is_success());
        let call = store
            .phone_inbound_by_sid("CA00000000000000000000000000000001")
            .unwrap();
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
        (call, response)
    } else {
        let grant = service.authorize(&owner, &chat).unwrap();
        let receipt = service
            .tool(
                &grant,
                "call_owner",
                json!({"request_id":"outbound-variant"}),
            )
            .await
            .unwrap();
        let call = store
            .phone_provider_call(receipt["id"].as_str().unwrap())
            .unwrap();
        let path = format!("/phone/start/{}", call.id);
        let mut outbound = fields.clone();
        for (k, v) in &mut outbound {
            match k.as_str() {
                "Direction" => *v = "outbound-api".into(),
                "From" => *v = "+12025550456".into(),
                "To" => *v = "+12025550123".into(),
                _ => {}
            }
        }
        let response = client
            .post(format!("{local}{path}"))
            .header("x-twilio-signature", signature(&path, &outbound))
            .form(&outbound)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        (call, response)
    };
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
    tokio::time::advance(Duration::from_secs(if slow { 94 } else { 2 })).await;
    if slow {
        native.release.notify_one();
    }
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
        if expected == 140 && !slow {
            assert!(start.elapsed() < Duration::from_secs(3));
            if busy {
                busy_provider.release.notify_one();
            }
        }
    }
    assert_eq!(
        source.reads.load(Ordering::SeqCst),
        if slow { 0 } else { 1 }
    );
    assert_eq!(
        native.calls.load(Ordering::SeqCst),
        if slow { 2 } else { 1 },
        "only the follow-up needs native reasoning"
    );
    assert_eq!(
        telephony.creates.load(Ordering::SeqCst),
        if inbound { 0 } else { 1 }
    );
    if slow {
        let ids = native.provider.observed_ids();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }
    assert!(!store.phone_provider_call(&call.id).unwrap().stop_requested);
    ws.send(wire(json!({"event":"stop","sequenceNumber":"2","streamSid":"MZ-test","stop":{"accountSid":"AC-test","callSid":"CA00000000000000000000000000000001"}}))).await.unwrap();
    peer_task.await.unwrap();
    if let Some(task) = busy_task {
        task.await.unwrap();
    }
    runtime.shutdown().await.unwrap();
    assert!(io
        .mirrors
        .lock()
        .unwrap()
        .iter()
        .any(|(_, s)| s.contains("Tuesday at 5 PM")));
}

#[tokio::test]
async fn incoming_attention_check_speaks_facts_before_slow_native_work_and_keeps_followup_context()
{
    exercise_call(false, true, false).await;
}
#[tokio::test]
async fn incoming_slow_native_result_recovers_rejected_context_and_answers_the_next_question() {
    exercise_call(true, true, false).await;
}
#[tokio::test]
async fn outbound_attention_check_uses_the_same_result_and_native_followup_path() {
    exercise_call(false, false, false).await;
}

#[tokio::test]
async fn attention_facts_and_history_handoff_bypass_an_active_shared_native_turn() {
    exercise_call(false, true, true).await;
}
