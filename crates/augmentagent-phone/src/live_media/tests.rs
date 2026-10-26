use crate::{
    ingress::{AgentError, ConversationAgent, SpeechSender},
    live_config::LiveConfig,
    webhook::WebhookAuth,
    *,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{PhoneCall, PhoneOrigin, ReserveCall},
    SurfaceAccountRef, SurfacePlatform,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha1::Sha1;
use tokio::net::TcpListener;
use tokio_tungstenite::{
    accept_async, connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tokio_util::sync::CancellationToken;
struct Time;
impl Clock for Time {
    fn now_ms(&self) -> i64 {
        100
    }
}
struct Phone;
#[async_trait]
impl Telephony for Phone {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("fixture does not dial")
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}
struct Agent;
#[async_trait]
impl ConversationAgent for Agent {
    async fn answer(
        &self,
        _: &PhoneCall,
        _: &str,
        _: &str,
        _: SpeechSender,
        _: CancellationToken,
    ) -> Result<String, AgentError> {
        panic!("Live cannot invoke the legacy voice path")
    }
    async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
        Ok(())
    }
}
fn signature(url: &str, fields: &[(String, String)]) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(b"synthetic-token").unwrap();
    mac.update(url.as_bytes());
    let mut sorted = fields.to_vec();
    sorted.sort();
    for (key, value) in sorted {
        mac.update(key.as_bytes());
        mac.update(value.as_bytes());
    }
    STANDARD.encode(mac.finalize().into_bytes())
}
struct Fixture {
    _dir: tempfile::TempDir,
    service: Arc<PhoneService>,
    id: String,
    twiml: String,
    url: String,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture(endpoint: String) -> Fixture {
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
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "synthetic-secret-at-least-32-bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical".into(),
        }],
    )
    .unwrap()
    .with_live(Some(LiveConfig::for_test(endpoint)));
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(Phone),
        Arc::new(Time),
        config,
    ));
    let grant = service.authorize(&owner, &chat).unwrap();
    let call = store
        .phone_reserve_call(ReserveCall {
            owner: "canonical",
            request_id: "one",
            conversation: &chat.storage_key(),
            now_ms: 100,
        })
        .unwrap()
        .call;
    store
        .phone_set_voice_backend(&call.id, "openai_live")
        .unwrap();
    store
        .phone_register_origin(
            &call.id,
            &PhoneOrigin {
                owner,
                conversation: chat,
            },
            grant.binding_revision,
        )
        .unwrap();
    store.phone_bind_provider(&call.id, "CA-test").unwrap();
    let auth = Arc::new(WebhookAuth::new("AC-test", "synthetic-token"));
    let fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
    ];
    let signed = signature(
        &format!("https://phone.example.test/phone/start/{}", call.id),
        &fields,
    );
    let twiml = service
        .start_callback(&auth, &call.id, &fields, &signed)
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://{}/phone/relay/{}",
        listener.local_addr().unwrap(),
        call.id
    );
    let router = ingress::router(service.clone(), auth, Arc::new(Agent));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Fixture {
        _dir: dir,
        service,
        id: call.id,
        twiml,
        url,
        server,
    }
}
fn request(f: &Fixture, valid: bool) -> tokio_tungstenite::tungstenite::http::Request<()> {
    let mut req = f.url.clone().into_client_request().unwrap();
    let sig = if valid {
        signature(
            &format!("wss://phone.example.test/phone/relay/{}", f.id),
            &[],
        )
    } else {
        "invalid".into()
    };
    req.headers_mut()
        .insert("x-twilio-signature", sig.parse().unwrap());
    req
}
fn start(call: &str) -> Message {
    Message::Text(json!({"event":"start","sequenceNumber":"1","streamSid":"MZ-test","start":{"accountSid":"AC-test","callSid":call,"streamSid":"MZ-test","tracks":["inbound"],"mediaFormat":{"encoding":"audio/x-mulaw","sampleRate":8000,"channels":1}}}).to_string().into())
}
fn connected() -> Message {
    Message::Text(
        json!({"event":"connected","protocol":"Call","version":"1.0.0"})
            .to_string()
            .into(),
    )
}
#[tokio::test]
async fn signed_answer_selects_media_stream_without_dtmf_or_legacy_relay() {
    let f = fixture("ws://127.0.0.1:1/".into()).await;
    assert!(f.twiml.contains("<Stream "));
    assert!(!f.twiml.contains("ConversationRelay"));
    assert!(!f.twiml.contains("Gather"));
}
#[tokio::test]
async fn unsigned_or_cross_call_stream_cannot_open_live() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = fixture(format!("ws://{}/", provider.local_addr().unwrap())).await;
    assert!(connect_async(request(&f, false)).await.is_err());
    let (mut ws, _) = connect_async(request(&f, true)).await.unwrap();
    ws.send(connected()).await.unwrap();
    ws.send(start("CA-other")).await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), provider.accept())
            .await
            .is_err()
    );
    assert!(f
        .service
        .store
        .phone_pending_dispatches()
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn signed_media_round_trips_audio_and_replayed_relay_opens_no_second_provider() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = fixture(format!("ws://{}/", provider.local_addr().unwrap())).await;
    let (replayed_tx, replayed_rx) = tokio::sync::oneshot::channel();
    let backend = tokio::spawn(async move {
        let (tcp, _) = provider.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let start = ws.next().await.unwrap().unwrap();
        let start: serde_json::Value = serde_json::from_str(start.to_text().unwrap()).unwrap();
        assert_eq!(start["session"]["input"][0]["role"], "developer");
        assert!(start["session"]["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Greet"));
        ws.send(Message::Text(json!({"type":"session.started","event_id":"evt-start","session":{"id":"live-test","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(
            json!({"type":"session.output_audio.delta","delta":STANDARD.encode([128;160])})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        loop {
            let Some(Ok(Message::Text(text))) = ws.next().await else {
                break;
            };
            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
            if value["type"] == "session.input_audio.append" {
                ws.send(Message::Text(json!({"type":"session.input_transcript.delta","event_id":"evt-request","delta":"Synthetic lookup","start_ms":0,"end_ms":20}).to_string().into())).await.unwrap();
                ws.send(Message::Text(json!({"type":"session.delegation.created","event_id":"evt-work","offset_ms":20,"delegation":{"id":"del-work","type":"delegation","target":"client"}}).to_string().into())).await.unwrap();
                ws.send(Message::Text(
                    json!({"type":"session.output_audio.delta","delta":value["audio"]})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
                let reply = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let reply: serde_json::Value =
                    serde_json::from_str(reply.to_text().unwrap()).unwrap();
                assert_eq!(reply["type"], "session.commentary.append");
                assert_eq!(reply["delegation_id"], "del-work");
                assert_eq!(reply["content"], "Synthetic lookup result");
                ws.send(Message::Text(json!({"type":"session.commentary.appended","event_id":"evt-accepted","client_event_id":reply["event_id"],"start_ms":30,"end_ms":30}).to_string().into())).await.unwrap();
                replayed_rx.await.unwrap();
                assert!(tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    provider.accept()
                )
                .await
                .is_err());
                break;
            }
        }
    });
    let (mut ws, _) = connect_async(request(&f, true)).await.unwrap();
    ws.send(connected()).await.unwrap();
    ws.send(start("CA-test")).await.unwrap();
    // The first audible frame must arrive without any caller audio.
    let greeting = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let greeting: serde_json::Value = serde_json::from_str(greeting.to_text().unwrap()).unwrap();
    assert_eq!(greeting["media"]["payload"], STANDARD.encode([128; 160]));
    let mark = ws.next().await.unwrap().unwrap();
    let mark: serde_json::Value = serde_json::from_str(mark.to_text().unwrap()).unwrap();
    ws.send(Message::Text(
        json!({"event":"mark","streamSid":"MZ-test","sequenceNumber":"2","mark":mark["mark"]})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(json!({"event":"media","sequenceNumber":"3","streamSid":"MZ-test","media":{"track":"inbound","chunk":"1","timestamp":"0","payload":STANDARD.encode([255;160])}}).to_string().into())).await.unwrap();
    let text = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let frame: serde_json::Value = serde_json::from_str(text.to_text().unwrap()).unwrap();
    assert_eq!(frame["event"], "media");
    assert_eq!(frame["media"]["payload"], STANDARD.encode([255; 160]));
    let jobs = f.service.store.phone_pending_dispatches().unwrap();
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0].description.contains("Synthetic lookup"));
    assert_eq!(
        jobs[0].state, "queued",
        "audio continues before backend work finishes"
    );
    f.service
        .store
        .phone_start_dispatch(&f.id, &jobs[0].task_id)
        .unwrap();
    f.service
        .store
        .phone_finish_dispatch(&f.id, &jobs[0].task_id, "Synthetic lookup result")
        .unwrap();
    f.service
        .store
        .phone_dispatch_delivered(&f.id, &jobs[0].task_id)
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !f
            .service
            .store
            .phone_live_results(&f.id, "live-test")
            .unwrap()
            .is_empty()
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let (mut replay, _) = connect_async(request(&f, true)).await.unwrap();
    replay.send(connected()).await.unwrap();
    replay.send(start("CA-test")).await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), replay.next())
        .await
        .unwrap();
    assert!(
        !f.service
            .store
            .phone_provider_call(&f.id)
            .unwrap()
            .stop_requested,
        "replayed stream must not stop the valid call"
    );
    replayed_tx.send(()).unwrap();
    backend.await.unwrap();
}

#[tokio::test]
async fn revoked_owner_cannot_open_live_even_with_valid_provider_signature() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let f = fixture(format!("ws://{}/", provider.local_addr().unwrap())).await;
    let origin = f.service.store.phone_origin(&f.id).unwrap();
    f.service
        .store
        .unbind_surface_owner(origin.owner.account())
        .unwrap();
    assert!(connect_async(request(&f, true)).await.is_err());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(30), provider.accept())
            .await
            .is_err()
    );
    assert!(f
        .service
        .store
        .phone_pending_dispatches()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn finalized_provider_failure_keeps_usage_and_is_not_an_owner_hangup() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", provider.local_addr().unwrap());
    let backend = tokio::spawn(async move {
        let (tcp, _) = provider.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        ws.send(Message::Text(json!({"type":"session.started","event_id":"evt-start","session":{"id":"live-test","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"type":"session.closed","event_id":"evt-close","reason":"connection_lost","session":{"id":"live-test"},"usage":{"seconds":0.25}}).to_string().into())).await.unwrap();
    });
    let f = fixture(endpoint).await;
    let (mut ws, _) = connect_async(request(&f, true)).await.unwrap();
    ws.send(connected()).await.unwrap();
    ws.send(start("CA-test")).await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap();
    backend.await.unwrap();
    let rows = f.service.store.phone_live_usage(1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].phase, "conversation");
    assert_eq!(rows[0].seconds, Some(0.25));
    assert_eq!(rows[0].reason.as_deref(), Some("connection_lost"));
    assert_eq!(rows[0].trigger.as_deref(), Some("provider_closed"));
    assert_eq!(rows[0].error.as_deref(), Some("connection_lost"));
    assert!(rows[0].finalization_error.is_none());
}

#[tokio::test]
async fn lost_provider_socket_has_unknown_usage_and_incomplete_finalization() {
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("ws://{}/", provider.local_addr().unwrap());
    let backend = tokio::spawn(async move {
        let (tcp, _) = provider.accept().await.unwrap();
        let mut ws = accept_async(tcp).await.unwrap();
        let _ = ws.next().await;
        ws.send(Message::Text(json!({"type":"session.started","event_id":"evt-start","session":{"id":"live-test","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}).to_string().into())).await.unwrap();
        // No session.closed: the wire closing must not imply zero usage.
        ws.close(None).await.unwrap();
    });
    let f = fixture(endpoint).await;
    let (mut ws, _) = connect_async(request(&f, true)).await.unwrap();
    ws.send(connected()).await.unwrap();
    ws.send(start("CA-test")).await.unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap();
    backend.await.unwrap();
    let rows = f.service.store.phone_live_usage(1).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].seconds, None);
    assert_eq!(rows[0].reason, None);
    assert_eq!(rows[0].trigger.as_deref(), Some("error"));
    assert_eq!(rows[0].error.as_deref(), Some("connection_lost"));
    assert!(rows[0].finalization_error.is_some());
}

#[tokio::test]
async fn owner_goodbye_closes_twilio_before_provider_finalization_and_keeps_tasks() {
    let provider=TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("ws://{}/",provider.local_addr().unwrap());
    let (finalize_tx,finalize_rx)=tokio::sync::oneshot::channel();
    let backend=tokio::spawn(async move {
        let (tcp,_)=provider.accept().await.unwrap();
        let mut ws=accept_async(tcp).await.unwrap();
        let _=ws.next().await;
        ws.send(Message::Text(json!({"type":"session.started","event_id":"evt-start","session":{"id":"live-test","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}).to_string().into())).await.unwrap();
        ws.send(Message::Text(json!({"type":"session.output_audio.delta","delta":STANDARD.encode([128;160])}).to_string().into())).await.unwrap();
        for event in [
            json!({"type":"session.input_transcript.delta","event_id":"task","delta":"Look up the weather.","start_ms":0,"end_ms":100}),
            json!({"type":"session.delegation.created","event_id":"delegate","delegation":{"type":"delegation","target":"client","id":"task-one"},"offset_ms":110}),
            json!({"type":"session.input_transcript.delta","event_id":"bye-one","delta":"That's all,","start_ms":2000,"end_ms":2300}),
            json!({"type":"session.input_transcript.delta","event_id":"bye-two","delta":" thanks.","start_ms":2300,"end_ms":2500}),
        ] { ws.send(Message::Text(event.to_string().into())).await.unwrap(); }
        let close=ws.next().await.unwrap().unwrap();
        let close:serde_json::Value=serde_json::from_str(close.to_text().unwrap()).unwrap();
        assert_eq!(close["type"],"session.close");
        finalize_rx.await.unwrap();
        ws.send(Message::Text(json!({"type":"session.closed","event_id":"evt-close","reason":"close_requested","session":{"id":"live-test"},"usage":{"seconds":2}}).to_string().into())).await.unwrap();
    });
    let f=fixture(endpoint).await;
    let (mut ws,_)=connect_async(request(&f,true)).await.unwrap();
    ws.send(connected()).await.unwrap();
    ws.send(start("CA-test")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3),async {
        loop { match ws.next().await {Some(Ok(Message::Close(_)))|None=>break,Some(Ok(_))=>{},other=>panic!("unexpected close: {other:?}") } }
    }).await.expect("goodbye must close Twilio without waiting for slow OpenAI finalization");
    assert!(f.service.store.phone_provider_call(&f.id).unwrap().stop_requested);
    assert_eq!(f.service.store.phone_end_reason(&f.id).unwrap().as_deref(),Some("owner_goodbye"));
    assert_eq!(f.service.store.phone_pending_dispatches().unwrap().len(),1);
    assert!(f.service.store.phone_live_usage(1).unwrap()[0].trigger.is_none());
    finalize_tx.send(()).unwrap();
    backend.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2),async {
        loop {
            let rows=f.service.store.phone_live_usage(1).unwrap();
            if rows[0].trigger.is_some() { assert_eq!(rows[0].trigger.as_deref(),Some("owner_goodbye")); break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    assert_eq!(f.service.store.phone_pending_dispatches().unwrap().len(),1);
    let playback=f.service.store.phone_live_playback(1).unwrap();
    assert!(playback[0].1);
    assert_eq!(playback[0].0.output_sent_bytes,160);
    assert_eq!(playback[0].0.played_bytes,0);
    assert_eq!(playback[0].0.clear_requested_bytes,160);
    assert_eq!(playback[0].0.cleared_receipt_bytes,0);
    assert_eq!(playback[0].0.first_playback_receipt_ms,None);

}

#[tokio::test]
async fn full_duplex_keeps_input_flowing_during_unacknowledged_output() {
    let provider=TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("ws://{}/",provider.local_addr().unwrap());
    let backend=tokio::spawn(async move {
        let (tcp,_)=provider.accept().await.unwrap();
        let mut ws=accept_async(tcp).await.unwrap();
        let _=ws.next().await;
        ws.send(Message::Text(json!({"type":"session.started","event_id":"evt-start","session":{"id":"live-test","model":"gpt-live-1","audio":{"format":{"type":"audio/pcmu","rate":8000},"output":{"voice":"gleam"}},"delegation":{"type":"client"}}}).to_string().into())).await.unwrap();
        for (index,text) in ["mm-hmm","yeah","stop","Actually, use tomorrow instead"].iter().enumerate() {
            // This fixture models provider behavior, not speech recognition or
            // interruption quality. The bridge must keep both directions live.
            let input=tokio::time::timeout(std::time::Duration::from_secs(1),ws.next()).await.unwrap().unwrap().unwrap();
            let input:serde_json::Value=serde_json::from_str(input.to_text().unwrap()).unwrap();
            assert_eq!(input["type"],"session.input_audio.append");
            assert_eq!(input["audio"],STANDARD.encode([128+index as u8;160]));
            ws.send(Message::Text(json!({"type":"session.input_transcript.delta","event_id":format!("input-{index}"),"delta":text,"start_ms":index*1000,"end_ms":index*1000+100}).to_string().into())).await.unwrap();
            ws.send(Message::Text(json!({"type":"session.output_audio.delta","delta":STANDARD.encode([132+index as u8;160])}).to_string().into())).await.unwrap();
        }
        let close=ws.next().await.unwrap().unwrap();
        let close:serde_json::Value=serde_json::from_str(close.to_text().unwrap()).unwrap();
        assert_eq!(close["type"],"session.close");
        ws.send(Message::Text(json!({"type":"session.closed","event_id":"closed","reason":"close_requested","session":{"id":"live-test"},"usage":{"seconds":4}}).to_string().into())).await.unwrap();
    });
    let f=fixture(endpoint).await;
    let (mut ws,_)=connect_async(request(&f,true)).await.unwrap();
    ws.send(connected()).await.unwrap();
    ws.send(start("CA-test")).await.unwrap();
    let mut marks=Vec::new();
    for index in 0..4 {
        ws.send(Message::Text(json!({"event":"media","sequenceNumber":(index+2).to_string(),"streamSid":"MZ-test","media":{"track":"inbound","chunk":(index+1).to_string(),"timestamp":(index*20).to_string(),"payload":STANDARD.encode([128+index as u8;160])}}).to_string().into())).await.unwrap();
        let output=tokio::time::timeout(std::time::Duration::from_secs(1),ws.next()).await.unwrap().unwrap().unwrap();
        let output:serde_json::Value=serde_json::from_str(output.to_text().unwrap()).unwrap();
        assert_eq!(output["event"],"media","no local clear/truncate or hangup inferred from a transcript");
        assert_eq!(output["media"]["payload"],STANDARD.encode([132+index as u8;160]));
        let mark=ws.next().await.unwrap().unwrap();
        let mark:serde_json::Value=serde_json::from_str(mark.to_text().unwrap()).unwrap();
        marks.push(mark["mark"].clone());
        // Deliberately defer all playback receipts while sending more input.
    }
    for (index,mark) in marks.into_iter().enumerate() {
        ws.send(Message::Text(json!({"event":"mark","sequenceNumber":(index+6).to_string(),"streamSid":"MZ-test","mark":mark}).to_string().into())).await.unwrap();
    }
    assert!(!f.service.store.phone_provider_call(&f.id).unwrap().stop_requested);
    ws.send(Message::Text(json!({"event":"stop","sequenceNumber":"10","streamSid":"MZ-test","stop":{"accountSid":"AC-test","callSid":"CA-test"}}).to_string().into())).await.unwrap();
    backend.await.unwrap();
    let playback=f.service.store.phone_live_playback(1).unwrap();
    assert_eq!(playback.len(),1);
    assert!(playback[0].1);
    let stats=&playback[0].0;
    assert_eq!(stats.input_bytes,640);
    assert_eq!(stats.output_received_bytes,640);
    assert_eq!(stats.output_sent_bytes,640);
    assert_eq!(stats.played_bytes,640);
    assert_eq!(stats.cleared_receipt_bytes,0);
    assert!(stats.first_playback_receipt_ms.is_some());
}

#[tokio::test]
async fn live_duration_starts_at_answer_and_expires_at_exact_boundary() {
    let f = fixture("ws://127.0.0.1:1".into()).await;
    let mut call = f.service.store.phone_provider_call(&f.id).unwrap();
    call.created_ms = 0;
    call.answered_ms = Some(60_000);
    call.max_duration_seconds = 60;
    assert!(!super::duration_expired(&call, 60_000));
    assert!(!super::duration_expired(&call, 119_999));
    assert!(super::duration_expired(&call, 120_000));
    assert!(!super::duration_expired(&call, 59_000));
}
