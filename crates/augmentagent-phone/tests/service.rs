use async_trait::async_trait;
use augmentagent_phone::{
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind, Store, SurfaceAccountRef, SurfaceConversationRef,
    SurfaceOwnerRef, SurfacePlatform,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc, Mutex,
};

struct FakeClock(AtomicI64);
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
#[derive(Default)]
struct FakeProvider {
    calls: Mutex<Vec<DialRequest>>,
    stops: Mutex<Vec<String>>,
    uncertain: bool,
    usage: Mutex<Option<augmentagent_phone::FinalUsage>>,
    usage_reads: Mutex<Vec<String>>,
}
#[async_trait]
impl Telephony for FakeProvider {
    async fn create(&self, r: DialRequest) -> Result<String, ProviderError> {
        let mut calls = self.calls.lock().unwrap();
        calls.push(r);
        if self.uncertain {
            Err(ProviderError::Uncertain)
        } else {
            Ok(if calls.len() == 1 {
                "CA-test".into()
            } else {
                format!("CA-test-{}", calls.len())
            })
        }
    }
    async fn final_usage(
        &self,
        sid: &str,
    ) -> Result<Option<augmentagent_phone::FinalUsage>, ProviderError> {
        self.usage_reads.lock().unwrap().push(sid.into());
        Ok(*self.usage.lock().unwrap())
    }
    async fn hangup(&self, sid: &str) -> Result<(), ProviderError> {
        self.stops.lock().unwrap().push(sid.into());
        Ok(())
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    clock: Arc<FakeClock>,
    provider: Arc<FakeProvider>,
    service: PhoneService,
    owner: SurfaceOwnerRef,
    conversation: SurfaceConversationRef,
}
fn fixture(platform: &str, enabled: bool, private: bool, uncertain: bool) -> Fixture {
    fixture_with_voice(
        platform,
        enabled,
        private,
        uncertain,
        augmentagent_phone::voice::VoiceConfig::default(),
        None,
    )
}
fn fixture_with_voice(
    platform: &str,
    enabled: bool,
    private: bool,
    uncertain: bool,
    voice: augmentagent_phone::voice::VoiceConfig,
    owner_first_name: Option<&str>,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new(platform).unwrap(), "account").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let conversation = SurfaceConversationRef::new(account, "chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(
            &conversation,
            if private {
                ControlConversationKind::Direct
            } else {
                ControlConversationKind::Channel
            },
            0,
        )
        .unwrap();
    store
        .phone_import_destination("canonical-owner", "+12025550123", 0)
        .unwrap();
    let provider = Arc::new(FakeProvider {
        uncertain,
        ..Default::default()
    });
    let clock = Arc::new(FakeClock(AtomicI64::new(100)));
    let config = PhoneConfig::new(
        enabled,
        "https://phone.example.test",
        "test-secret-for-verification-32bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical-owner".into(),
        }],
    )
    .unwrap()
    .with_voice(voice)
    .with_owner_first_name(owner_first_name.map(str::to_owned));
    let service = PhoneService::new(store.clone(), provider.clone(), clock.clone(), config);
    Fixture {
        _dir: dir,
        store,
        clock,
        provider,
        service,
        owner,
        conversation,
    }
}

#[tokio::test]
async fn ac01_disabled_never_dials() {
    let f = fixture("discord", false, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    assert_eq!(
        f.service
            .tool(&grant, "call_owner", json!({"request_id":"r1"}))
            .await
            .unwrap_err()
            .code(),
        "disabled"
    );
    assert!(f.provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ac02_target_spoofing_expiry_nonowner_and_revocation_never_dial() {
    let f = fixture("slack", true, true, false);
    let outsider = SurfaceOwnerRef::new(f.owner.account().clone(), "outsider").unwrap();
    assert!(f.service.authorize(&outsider, &f.conversation).is_err());
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    assert!(f
        .service
        .tool(
            &grant,
            "call_owner",
            json!({"request_id":"r1","to":"+12025550999"})
        )
        .await
        .is_err());
    f.clock.0.store(300_101, Ordering::SeqCst);
    assert!(f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .is_err());
    let fresh = f.service.authorize(&f.owner, &f.conversation).unwrap();
    f.store.unbind_surface_owner(f.owner.account()).unwrap();
    assert!(f
        .service
        .tool(&fresh, "call_owner", json!({"request_id":"r1"}))
        .await
        .is_err());
    assert!(f.provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ac03_uncertain_create_is_not_redialed() {
    let f = fixture("discord", true, true, true);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let result = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    assert_eq!(result["state"], "uncertain");
    let retry = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    assert_eq!(result["id"], retry["id"]);
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn ac08_three_surfaces_share_call_status_and_stop_contract() {
    for platform in ["discord", "whatsapp", "slack"] {
        let f = fixture(platform, true, true, false);
        let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
        let call = f
            .service
            .tool(&grant, "call_owner", json!({"request_id":"r1"}))
            .await
            .unwrap();
        let status = f
            .service
            .tool(&grant, "phone_status", json!({"call_id":call["id"]}))
            .await
            .unwrap();
        assert_eq!(status["id"], call["id"]);
        assert!(!status.to_string().contains("+12025550123"));
        f.service
            .tool(&grant, "phone_hangup", json!({"call_id":call["id"]}))
            .await
            .unwrap();
        assert_eq!(f.provider.stops.lock().unwrap().len(), 1);
        let requests = f.provider.calls.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].destination.expose(), "+12025550123");
        assert!(requests[0].twiml.contains("Redirect"));
        assert!(requests[0].twiml.contains("/phone/start/"));
        assert!(!requests[0].twiml.contains("ConversationRelay"));
    }
}

#[tokio::test]
async fn ac13_shared_channel_setup_does_not_store_or_dial() {
    let f = fixture("slack", true, false, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let err = f
        .service
        .set_number(&grant, "setup-1", "+12025550456")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "private_setup_required");
    assert!(f.provider.calls.lock().unwrap().is_empty());
    assert!(
        !f.store
            .phone_destination_status("canonical-owner")
            .unwrap()
            .pending
    );
}

#[tokio::test]
async fn ac13_private_setup_verifies_then_calls_new_number_on_each_surface() {
    use augmentagent_store::phone::CallState;
    for platform in ["discord", "whatsapp", "slack"] {
        let f = fixture(platform, true, true, false);
        let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
        let pending = f
            .service
            .set_number(&grant, "setup", "+12025550456")
            .await
            .unwrap();
        assert_eq!(pending["pending"], true);
        assert_eq!(pending["masked"], "••••0123");
        let code = {
            let calls = f.provider.calls.lock().unwrap();
            calls[0]
                .twiml
                .split(" is ")
                .nth(1)
                .unwrap()
                .split('.')
                .next()
                .unwrap()
                .replace(' ', "")
        };
        assert!(f.service.verify_number(&grant, &code).unwrap());
        assert!(!f.service.verify_number(&grant, &code).unwrap());
        assert_eq!(
            f.service.number_status(&grant).unwrap()["masked"],
            "••••0456"
        );
        // A call that ends before a new start frees the single-owner slot.
        let receipt = f
            .store
            .phone_reserve_call(augmentagent_store::phone::ReserveCall {
                owner: "canonical-owner",
                request_id: "setup",
                conversation: &f.conversation.storage_key(),
                now_ms: 101,
            });
        assert!(receipt.is_err()); // Cannot turn a verification request into a normal call.
        let id = f
            .store
            .with_conn(|c| {
                c.query_row(
                    "SELECT id FROM phone_calls WHERE request_id='setup'",
                    [],
                    |r| r.get::<_, String>(0),
                )
            })
            .unwrap();
        f.store
            .phone_provider_event(&id, "CA-test", CallState::Completed, 102)
            .unwrap();
        let started = f
            .service
            .tool(&grant, "call_owner", json!({"request_id":"normal"}))
            .await
            .unwrap();
        assert_eq!(started["state"], "queued");
        let calls = f.provider.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].destination.expose(), "+12025550456");
    }
}

fn signed(token: &str, url: &str, fields: &[(String, String)]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut payload = url.to_string();
    let mut fields = fields.to_vec();
    fields.sort();
    for (k, v) in fields {
        payload.push_str(&k);
        payload.push_str(&v);
    }
    let mut h = Hmac::<Sha1>::new_from_slice(token.as_bytes()).unwrap();
    h.update(payload.as_bytes());
    STANDARD.encode(h.finalize().into_bytes())
}
#[tokio::test]
async fn ac04_signed_callbacks_reject_account_and_sid_spoofing_before_mutation() {
    use augmentagent_phone::webhook::WebhookAuth;
    let f = fixture("discord", true, true, false);
    let auth = WebhookAuth::new("AC-test", "callback-secret");
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap();
    let path = format!("/phone/status/{id}");
    let mut fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
        ("CallStatus".into(), "ringing".into()),
    ];
    assert!(f
        .service
        .status_callback(&auth, id, &fields, "invalid")
        .is_err());
    assert_eq!(
        f.store
            .phone_call("canonical-owner", id)
            .unwrap()
            .state
            .as_str(),
        "queued"
    );
    let sig = signed(
        "callback-secret",
        &format!("https://phone.example.test{path}"),
        &fields,
    );
    f.service.status_callback(&auth, id, &fields, &sig).unwrap();
    assert_eq!(
        f.store
            .phone_call("canonical-owner", id)
            .unwrap()
            .state
            .as_str(),
        "ringing"
    );
    fields[0].1 = "AC-other".into();
    let sig = signed(
        "callback-secret",
        &format!("https://phone.example.test{path}"),
        &fields,
    );
    assert!(f.service.status_callback(&auth, id, &fields, &sig).is_err());
    fields[0].1 = "AC-test".into();
    fields[1].1 = "CA-other".into();
    let sig = signed(
        "callback-secret",
        &format!("https://phone.example.test{path}"),
        &fields,
    );
    assert!(f.service.status_callback(&auth, id, &fields, &sig).is_err());
    assert_eq!(
        f.store
            .phone_call("canonical-owner", id)
            .unwrap()
            .state
            .as_str(),
        "ringing"
    );
}

#[tokio::test]
async fn ac05_signed_owner_start_connects_without_second_acceptance() {
    use augmentagent_phone::webhook::WebhookAuth;
    let f = fixture("discord", true, true, false);
    let auth = WebhookAuth::new("AC-test", "callback-secret");
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap();
    let mut fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
    ];
    let sig = signed(
        "callback-secret",
        &format!("https://phone.example.test/phone/start/{id}"),
        &fields,
    );
    let greeting = f.service.start_callback(&auth, id, &fields, &sig).unwrap();
    assert!(!greeting.contains("Gather"));
    assert!(greeting.contains("ConversationRelay"));
    assert!(!greeting.contains("+12025550123"));
    fields.push(("Digits".into(), "1".into()));
    let sig = signed(
        "callback-secret",
        &format!("https://phone.example.test/phone/accept/{id}"),
        &fields,
    );
    let relay = f
        .service
        .accept_callback(&auth, id, &fields, &sig)
        .await
        .unwrap();
    assert!(relay.contains("Hangup"));
    assert_eq!(
        f.store
            .phone_call("canonical-owner", id)
            .unwrap()
            .state
            .as_str(),
        "connected"
    );
    f.service
        .tool(&grant, "phone_hangup", json!({"call_id":id}))
        .await
        .unwrap();
    assert_eq!(
        f.service
            .accept_callback(&auth, id, &fields, &sig)
            .await
            .unwrap(),
        "<Response><Hangup/></Response>"
    );
}

#[tokio::test]
async fn ac09_watchdog_hangs_up_unaccepted_calls_and_restart_never_redials() {
    use augmentagent_store::phone::CallState;
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap();
    f.store
        .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.clock.0.store(15_099, Ordering::SeqCst);
    f.service.maintenance(false).await.unwrap();
    assert!(f.provider.stops.lock().unwrap().is_empty());
    f.clock.0.store(15_100, Ordering::SeqCst);
    f.service.maintenance(false).await.unwrap();
    assert_eq!(f.provider.stops.lock().unwrap().len(), 1);
    assert!(
        f.store
            .phone_call("canonical-owner", id)
            .unwrap()
            .stop_requested
    );
    f.service.maintenance(true).await.unwrap();
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
}

struct ScriptedAgent;
#[async_trait]
impl augmentagent_phone::ingress::ConversationAgent for ScriptedAgent {
    async fn answer(
        &self,
        _call: &augmentagent_store::phone::PhoneCall,
        _turn_id: &str,
        text: &str,
        _speech: augmentagent_phone::ingress::SpeechSender,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, augmentagent_phone::ingress::AgentError> {
        Ok(format!("Answer to: {text}"))
    }
    async fn interrupted(
        &self,
        _call: &augmentagent_store::phone::PhoneCall,
        _heard: &str,
    ) -> Result<(), augmentagent_phone::ingress::AgentError> {
        Ok(())
    }
}

#[tokio::test]
async fn ac01_real_http_router_rejects_unsigned_callbacks_and_serves_signed_start() {
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap();
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        Arc::new(ScriptedAgent),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let http = reqwest::Client::new();
    let url = format!("http://{addr}/phone/start/{id}");
    let fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
    ];
    let response = http.post(&url).form(&fields).send().await.unwrap();
    assert_eq!(response.status().as_u16(), 403);
    let signature = signed(
        "callback-secret",
        &format!("https://phone.example.test/phone/start/{id}"),
        &fields,
    );
    let response = http
        .post(&url)
        .header("x-twilio-signature", signature)
        .header("forwarded", "host=attacker.invalid;proto=http")
        .form(&fields)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(response.text().await.unwrap().contains("ConversationRelay"));
    server.abort();
}

#[tokio::test]
async fn ac07_real_websocket_routes_final_transcript_and_hangup_without_replay() {
    use augmentagent_store::phone::CallState;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 101).unwrap();
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        Arc::new(ScriptedAgent),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{id}"),
        &[],
    );
    request
        .headers_mut()
        .insert("x-twilio-signature", signature.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();
    socket.send(Message::Ping(vec![1, 2, 3].into())).await.unwrap();
    let pong = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next()).await.unwrap().unwrap().unwrap();
    assert!(matches!(pong, Message::Pong(ref bytes) if bytes.as_ref() == [1,2,3]));
    socket.send(Message::Pong(vec![4].into())).await.unwrap();
    let setup =
        json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test","parentCallSid":"","from":"+12025550123","to":"+12025550124","forwardedFrom":"","callType":"PSTN","callerName":"","direction":"outbound-api","callStatus":"IN_PROGRESS","customParameters":{}})
            .to_string();
    socket
        .send(Message::Text(setup.clone().into()))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"hel","last":false})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(75), socket.next())
            .await
            .is_err()
    );
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"hello","last":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
    assert_eq!(body["token"], "Answer to: hello");
    let (mut replay, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    replay.send(Message::Text(setup.into())).await.unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(2), replay.next())
        .await
        .unwrap();
    assert!(closed.is_none() || matches!(closed, Some(Ok(Message::Close(_)))));
    socket.close(None).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if f.store
                .phone_call("canonical-owner", &id)
                .unwrap()
                .state
                .terminal()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    assert_eq!(f.provider.stops.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn ac02_mcp_socket_grants_are_private_bound_revocable_and_reject_target_arguments() {
    use augmentagent_phone::tools::ToolServer;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let f = fixture("discord", true, true, false);
    let server = ToolServer::start(Arc::new(f.service)).await.unwrap();
    assert_eq!(
        std::fs::metadata(server.socket_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let grant = server.grant(&f.owner, &f.conversation, None).unwrap();
    async fn invoke(
        path: &std::path::Path,
        token: &str,
        args: serde_json::Value,
    ) -> serde_json::Value {
        let mut stream = tokio::net::UnixStream::connect(path).await.unwrap();
        stream
            .write_all(
                format!(
                    "{}\n",
                    json!({"version":1,"grant":token,"method":"call_owner","arguments":args})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }
    let rejected = invoke(
        server.socket_path(),
        grant.token(),
        json!({"request_id":"r","to":"+12025550999"}),
    )
    .await;
    assert_eq!(rejected["ok"], false);
    assert!(f.provider.calls.lock().unwrap().is_empty());
    let valid = invoke(
        server.socket_path(),
        grant.token(),
        json!({"request_id":"r"}),
    )
    .await;
    assert_eq!(valid["ok"], true);
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    let token = grant.token().to_string();
    drop(grant);
    assert_eq!(
        invoke(server.socket_path(), &token, json!({"request_id":"r"})).await["ok"],
        false
    );
}

#[tokio::test]
async fn ac13_number_commands_consume_codes_without_returning_them_to_history() {
    use augmentagent_phone::commands::{execute, recognizes, sensitive};
    let f = fixture("slack", true, true, false);
    assert!(recognizes("phone number verify 123456"));
    assert!(sensitive("/phone number set +12025550456"));
    assert!(sensitive("phone number verify invalid code"));
    assert!(!recognizes("telephone support"));
    let reply = execute(
        &f.service,
        &f.owner,
        &f.conversation,
        "event-1",
        "phone number set +12025550456",
    )
    .await
    .unwrap();
    assert!(!reply.contains("+12025550456"));
    let code = {
        let calls = f.provider.calls.lock().unwrap();
        calls[0]
            .twiml
            .split(" is ")
            .nth(1)
            .unwrap()
            .split('.')
            .next()
            .unwrap()
            .replace(' ', "")
    };
    let reply = execute(
        &f.service,
        &f.owner,
        &f.conversation,
        "event-2",
        &format!("phone number verify {code}"),
    )
    .await
    .unwrap();
    assert!(reply.contains("verified"));
    assert!(!reply.contains(&code));
    let status = execute(
        &f.service,
        &f.owner,
        &f.conversation,
        "event-3",
        "phone number status",
    )
    .await
    .unwrap();
    assert!(status.contains("0456"));
    assert!(!status.contains("+12025550456"));
}

#[tokio::test]
async fn phone_router_uses_persisted_origin_and_revocable_adapter_registration() {
    use augmentagent_phone::{
        ingress::{AgentError, ConversationAgent, SpeechSender},
        router::AgentRouter,
    };
    use augmentagent_store::phone::PhoneCall;
    struct Agent(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl ConversationAgent for Agent {
        async fn answer(
            &self,
            _: &PhoneCall,
            _: &str,
            _: &str,
            _: SpeechSender,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<String, AgentError> {
            Ok("answer".into())
        }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let f = fixture("slack", true, true, false);
    let router = AgentRouter::new(f.store.clone());
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let receipt = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"routed"}))
        .await
        .unwrap();
    let call = f
        .store
        .phone_provider_call(receipt["id"].as_str().unwrap())
        .unwrap();
    let agent = Arc::new(Agent(std::sync::atomic::AtomicUsize::new(0)));
    let wrong = router.register("discord", agent.clone()).unwrap();
    assert!(router.interrupted(&call, "heard").await.is_err());
    drop(wrong);
    let registration = router.register("slack", agent.clone()).unwrap();
    assert!(router.register("slack", agent.clone()).is_err());
    router.interrupted(&call, "heard").await.unwrap();
    let mut forged = call.clone();
    forged.conversation = "another conversation".into();
    assert!(router.interrupted(&forged, "secret").await.is_err());
    drop(registration);
    assert!(router.interrupted(&call, "heard").await.is_err());
    assert_eq!(agent.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ac04_revoked_owner_cannot_start_or_accept_but_terminal_status_still_records() {
    use augmentagent_phone::webhook::WebhookAuth;
    for revoke_before_start in [true, false] {
        let f = fixture("discord", true, true, false);
        let auth = WebhookAuth::new("AC-test", "callback-secret");
        let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
        let call = f
            .service
            .tool(&grant, "call_owner", json!({"request_id":"revoked"}))
            .await
            .unwrap();
        let id = call["id"].as_str().unwrap();
        let mut fields = vec![
            ("AccountSid".into(), "AC-test".into()),
            ("CallSid".into(), "CA-test".into()),
        ];
        if revoke_before_start {
            f.store.unbind_surface_owner(f.owner.account()).unwrap();
        }
        let sig = signed(
            "callback-secret",
            &format!("https://phone.example.test/phone/start/{id}"),
            &fields,
        );
        let reply = f.service.start_callback(&auth, id, &fields, &sig).unwrap();
        if revoke_before_start {
            assert_eq!(reply, "<Response><Hangup/></Response>");
        } else {
            assert!(reply.contains("ConversationRelay"));
            f.store.unbind_surface_owner(f.owner.account()).unwrap();
        }
        fields.push(("Digits".into(), "1".into()));
        let sig = signed(
            "callback-secret",
            &format!("https://phone.example.test/phone/accept/{id}"),
            &fields,
        );
        assert_eq!(
            f.service
                .accept_callback(&auth, id, &fields, &sig)
                .await
                .unwrap(),
            "<Response><Hangup/></Response>"
        );
        assert!(f.store.phone_provider_call(id).unwrap().stop_requested);
        fields.push(("CallStatus".into(), "completed".into()));
        let sig = signed(
            "callback-secret",
            &format!("https://phone.example.test/phone/status/{id}"),
            &fields,
        );
        f.service.status_callback(&auth, id, &fields, &sig).unwrap();
        assert!(f.store.phone_provider_call(id).unwrap().state.terminal());
        assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn ac04_same_timestamp_rebind_invalidates_existing_call_and_grant() {
    use augmentagent_store::phone::CallState;
    for connected in [false, true] {
        let f = fixture("discord", true, true, false);
        let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
        let call = f
            .service
            .tool(&grant, "call_owner", json!({"request_id":"old"}))
            .await
            .unwrap();
        let id = call["id"].as_str().unwrap();
        f.store
            .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
            .unwrap();
        if connected {
            f.store.phone_accept(id, "CA-test", 101).unwrap();
        }
        f.store.unbind_surface_owner(f.owner.account()).unwrap();
        f.store.bind_surface_owner(&f.owner, 0).unwrap();
        f.store
            .set_surface_control_conversation(&f.conversation, ControlConversationKind::Direct, 0)
            .unwrap();
        assert!(
            f.service
                .tool(&grant, "phone_status", json!({"call_id":id}))
                .await
                .is_err(),
            "old grants must not revive at the same timestamp"
        );
        assert!(f.service.authorize(&f.owner, &f.conversation).is_ok());
        if connected {
            assert!(f.service.authorize_call(id).is_err());
        } else {
            let auth = augmentagent_phone::webhook::WebhookAuth::new("AC-test", "callback-secret");
            let fields = vec![
                ("AccountSid".into(), "AC-test".into()),
                ("CallSid".into(), "CA-test".into()),
                ("Digits".into(), "1".into()),
            ];
            let sig = signed(
                "callback-secret",
                &format!("https://phone.example.test/phone/accept/{id}"),
                &fields,
            );
            assert_eq!(
                f.service
                    .accept_callback(&auth, id, &fields, &sig)
                    .await
                    .unwrap(),
                "<Response><Hangup/></Response>"
            );
        }
    }
}

#[tokio::test]
async fn late_speech_is_suppressed_and_call_stops_after_authority_changes() {
    use augmentagent_phone::ingress::{AgentError, ConversationAgent, SpeechSender};
    use augmentagent_store::phone::{CallState, PhoneCall};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    struct WaitingAgent {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ConversationAgent for WaitingAgent {
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            Ok(())
        }
        async fn answer(
            &self,
            _: &PhoneCall,
            _: &str,
            _: &str,
            speech: SpeechSender,
            _: tokio_util::sync::CancellationToken,
        ) -> Result<String, AgentError> {
            self.entered.notify_one();
            self.release.notified().await;
            let _ = speech.speak("late", "private late speech", false).await;
            Ok("private late final".into())
        }
    }
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"late"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_owned();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 100).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        Arc::new(WaitingAgent {
            entered: entered.clone(),
            release: release.clone(),
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "x-twilio-signature",
        signed(
            "callback-secret",
            &format!("wss://phone.example.test/phone/relay/{id}"),
            &[],
        )
        .parse()
        .unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket.send(Message::Text(json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"}).to_string().into())).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"private question","last":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    f.store.bind_surface_owner(&f.owner, 0).unwrap();
    release.notify_one();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap();
    assert!(
        result.is_none() || matches!(result, Some(Ok(Message::Close(_)))),
        "revoked call must close without another speech frame"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while f.provider.stops.lock().unwrap().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn revoked_signed_relay_upgrade_requests_stop_but_bad_signature_does_not() {
    use augmentagent_store::phone::CallState;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Error};
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(
            &grant,
            "call_owner",
            json!({"request_id":"revoked-upgrade"}),
        )
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_owned();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 100).unwrap();
    f.store.bind_surface_owner(&f.owner, 0).unwrap();
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        Arc::new(ScriptedAgent),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    for valid in [false, true] {
        let mut request = format!("ws://{addr}/phone/relay/{id}")
            .into_client_request()
            .unwrap();
        let signature = if valid {
            signed(
                "callback-secret",
                &format!("wss://phone.example.test/phone/relay/{id}"),
                &[],
            )
        } else {
            "invalid".into()
        };
        request
            .headers_mut()
            .insert("x-twilio-signature", signature.parse().unwrap());
        let error = tokio_tungstenite::connect_async(request).await.unwrap_err();
        assert!(
            matches!(error, Error::Http(ref response) if response.status().as_u16() == if valid {409} else {403})
        );
        assert_eq!(
            f.store.phone_provider_call(&id).unwrap().stop_requested,
            valid
        );
    }
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    server.abort();
}

#[test]
fn whatsapp_private_bootstrap_requires_explicit_link_and_never_restores_revocation() {
    for mode in [
        "valid",
        "disabled",
        "unlinked",
        "wrong-chat",
        "thread",
        "wrong-account",
        "wrong-sender",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
        let account = SurfaceAccountRef::new(
            SurfacePlatform::new("whatsapp").unwrap(),
            if mode == "wrong-account" {
                "device:garbage"
            } else {
                "device:12025550111"
            },
        )
        .unwrap();
        let owner = SurfaceOwnerRef::new(
            account.clone(),
            if mode == "wrong-sender" {
                "12025550112@s.whatsapp.net"
            } else {
                "12025550111@s.whatsapp.net"
            },
        )
        .unwrap();
        let chat = SurfaceConversationRef::new(
            account.clone(),
            if mode == "wrong-chat" {
                "12025550112@s.whatsapp.net"
            } else {
                "12025550111@s.whatsapp.net"
            },
            (mode == "thread").then(|| "thread".to_owned()),
        )
        .unwrap();
        let linked = if mode == "unlinked" {
            SurfaceOwnerRef::new(account.clone(), "12025550113@s.whatsapp.net").unwrap()
        } else {
            owner.clone()
        };
        let service = PhoneService::new(
            store.clone(),
            Arc::new(FakeProvider::default()),
            Arc::new(FakeClock(AtomicI64::new(100))),
            PhoneConfig::new(
                mode != "disabled",
                "https://phone.example.test",
                "fixture-secret-at-least-32-bytes",
                vec![OwnerLink {
                    surface_owner: linked,
                    owner_id: "owner".into(),
                }],
            )
            .unwrap(),
        );
        let result = service.initialize_whatsapp_private_owner(&owner, &chat);
        if mode != "valid" {
            assert!(result.is_err(), "{mode}");
            assert!(
                store.surface_owner_binding(&account).unwrap().is_none(),
                "{mode}"
            );
            continue;
        }
        result.unwrap();
        let revision = store
            .surface_owner_binding(&account)
            .unwrap()
            .unwrap()
            .revision;
        service
            .initialize_whatsapp_private_owner(&owner, &chat)
            .unwrap();
        assert_eq!(
            store
                .surface_owner_binding(&account)
                .unwrap()
                .unwrap()
                .revision,
            revision
        );
        assert!(service.authorize(&owner, &chat).is_ok());
        store.unbind_surface_owner(&account).unwrap();
        assert!(service
            .initialize_whatsapp_private_owner(&owner, &chat)
            .is_err());
        assert!(store.surface_owner_binding(&account).unwrap().is_none());
    }
}

#[tokio::test]
async fn final_duration_requires_signed_valid_terminal_fields_and_updates_after_cleanup() {
    use augmentagent_phone::webhook::WebhookAuth;
    use augmentagent_store::phone::{CallState, ProviderUsage};
    let f = fixture("discord", true, true, false);
    let auth = WebhookAuth::new("AC-test", "callback-secret");
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"duration"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap();
    let url = format!("https://phone.example.test/phone/status/{id}");
    f.clock.0.store(200_000, Ordering::SeqCst);
    let fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
        ("CallStatus".into(), "completed".into()),
        ("CallDuration".into(), "120".into()),
        ("Timestamp".into(), "Thu, 01 Jan 1970 00:02:00 +0000".into()),
    ];
    assert!(f
        .service
        .status_callback(&auth, id, &fields, "forged")
        .is_err());
    for mode in 0..7 {
        let mut bad = fields.clone();
        match mode {
            0 => bad[3].1 = "-1".into(),
            1 => bad[3].1 = "1.2".into(),
            2 => bad[3].1 = "99999999999999999999999".into(),
            3 => bad[4].1 = "not a timestamp".into(),
            4 => bad.push(("CallDuration".into(), "0".into())),
            5 => bad[2].1 = "ringing".into(),
            _ => {
                bad.pop();
            }
        }
        let sig = signed("callback-secret", &url, &bad);
        assert!(
            f.service.status_callback(&auth, id, &bad, &sig).is_err(),
            "invalid duration mode {mode}"
        );
        assert_eq!(f.store.phone_usage(id).unwrap(), None);
        assert_eq!(
            f.store.phone_provider_call(id).unwrap().state,
            CallState::Queued
        );
    }
    f.store
        .phone_provider_event(id, "CA-test", CallState::Completed, 150_000)
        .unwrap();
    let sig = signed("callback-secret", &url, &fields);
    for _ in 0..2 {
        f.service.status_callback(&auth, id, &fields, &sig).unwrap();
    }
    assert_eq!(
        f.store.phone_usage(id).unwrap(),
        Some(ProviderUsage {
            duration_seconds: 120,
            ended_ms: 120_000
        })
    );
    let mut conflicting = fields.clone();
    conflicting[3].1 = "0".into();
    let sig = signed("callback-secret", &url, &conflicting);
    assert!(f
        .service
        .status_callback(&auth, id, &conflicting, &sig)
        .is_err());
    assert_eq!(
        f.store.phone_usage(id).unwrap().unwrap().duration_seconds,
        120
    );
}

#[tokio::test]
async fn late_provider_usage_shortens_an_active_calls_daily_budget() {
    use augmentagent_store::phone::{CallState, ProviderUsage};
    let f = fixture("discord", true, true, false);
    let mut first = String::new();
    for i in 0..3 {
        let now = 100 + i * 1_000_000;
        f.clock.0.store(now, Ordering::SeqCst);
        let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
        let call = f
            .service
            .tool(
                &grant,
                "call_owner",
                json!({"request_id":format!("past-{i}")}),
            )
            .await
            .unwrap();
        let id = call["id"].as_str().unwrap();
        let sid = f
            .store
            .phone_provider_call(id)
            .unwrap()
            .provider_sid
            .unwrap();
        f.store
            .phone_provider_event(id, &sid, CallState::Completed, now + 100)
            .unwrap();
        if i == 0 {
            first = id.into();
        }
    }
    f.clock.0.store(3_000_100, Ordering::SeqCst);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"active"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap();
    f.store
        .phone_provider_event(id, "CA-test-4", CallState::AwaitingAcceptance, 3_000_100)
        .unwrap();
    f.store.phone_accept(id, "CA-test-4", 3_000_101).unwrap();
    // A provider overrun beyond the original limit must reduce this call's
    // remaining allowance, even though it was already dialed with a 900s cap.
    f.store
        .phone_provider_event_with_usage(
            &first,
            "CA-test",
            CallState::Completed,
            3_000_100,
            Some(ProviderUsage {
                duration_seconds: 1200,
                ended_ms: 1_200_100,
            }),
        )
        .unwrap();
    f.clock.0.store(3_600_100, Ordering::SeqCst);
    f.service.maintenance(false).await.unwrap();
    assert!(f
        .provider
        .stops
        .lock()
        .unwrap()
        .iter()
        .any(|sid| sid == "CA-test-4"));
}

#[tokio::test]
async fn lost_final_callback_recovers_usage_without_another_dial_or_hangup() {
    use augmentagent_phone::FinalUsage;
    use augmentagent_store::phone::{CallState, ProviderUsage, ReserveCall};
    for usage in [
        FinalUsage::Completed(ProviderUsage {
            duration_seconds: 120,
            ended_ms: 120_000,
        }),
        FinalUsage::NeverAnswered,
    ] {
        let f = fixture("slack", true, true, false);
        let call = f
            .store
            .phone_reserve_call(ReserveCall {
                owner: "canonical-owner",
                request_id: "lost-callback",
                conversation: "chat",
                now_ms: 0,
            })
            .unwrap()
            .call;
        f.store
            .phone_provider_event(&call.id, "CA-test", CallState::Canceled, 120_000)
            .unwrap();
        f.clock.0.store(120_000, Ordering::SeqCst);
        assert_eq!(
            f.store
                .phone_budget_remaining_ms("canonical-owner", 120_000)
                .unwrap(),
            2_700_000
        );
        *f.provider.usage.lock().unwrap() = Some(usage);
        f.service.maintenance(false).await.unwrap();
        let expected = if matches!(usage, FinalUsage::NeverAnswered) {
            3_600_000
        } else {
            3_480_000
        };
        assert_eq!(
            f.store
                .phone_budget_remaining_ms("canonical-owner", 120_000)
                .unwrap(),
            expected
        );
        f.service.maintenance(true).await.unwrap();
        assert_eq!(f.provider.usage_reads.lock().unwrap().len(), 1);
        assert!(f.provider.calls.lock().unwrap().is_empty());
        assert!(f.provider.stops.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn contradictory_unanswered_status_does_not_refund_a_locally_answered_call() {
    use augmentagent_store::phone::{CallState, ReserveCall};
    let f = fixture("slack", true, true, false);
    let call = f
        .store
        .phone_reserve_call(ReserveCall {
            owner: "canonical-owner",
            request_id: "answered",
            conversation: "chat",
            now_ms: 0,
        })
        .unwrap()
        .call;
    f.store
        .phone_provider_event(&call.id, "CA-test", CallState::AwaitingAcceptance, 1000)
        .unwrap();
    f.store
        .phone_provider_event(&call.id, "CA-test", CallState::Completed, 120_000)
        .unwrap();
    f.clock.0.store(120_000, Ordering::SeqCst);
    *f.provider.usage.lock().unwrap() = Some(augmentagent_phone::FinalUsage::NeverAnswered);
    f.service.maintenance(false).await.unwrap();
    assert!(f.store.phone_usage(&call.id).unwrap().is_none());
    assert_eq!(
        f.store
            .phone_budget_remaining_ms("canonical-owner", 120_000)
            .unwrap(),
        2_700_000
    );
}

#[tokio::test]
async fn ac09_status_exposes_persisted_retry_usage_without_provider_actions() {
    use augmentagent_store::phone::CallState;
    for (platform, uncertain) in ["discord", "slack", "whatsapp"].into_iter()
        .flat_map(|platform| [false, true].map(|uncertain| (platform, uncertain))) {
        let f = fixture(platform, true, true, uncertain);
        let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
        let call = f.service.tool(&grant, "call_owner", json!({"request_id":"status-recovery"})).await.unwrap();
        let id = call["id"].as_str().unwrap();
        if !uncertain {
            f.store.phone_request_stop("canonical-owner", id).unwrap();
            for now in [100, 1100, 3100] {
                assert!(f.store.phone_claim_cleanup(id, now).unwrap());
            }
        }
        let status = f.service.tool(&grant, "phone_status", json!({"call_id":id})).await.unwrap();
        assert_eq!(status["maintenance"]["provider_sid_known"], !uncertain);
        assert_eq!(status["maintenance"]["cleanup_attempts"], if uncertain {0} else {3});
        assert_eq!(status["maintenance"]["usage_attempts"], 0);
        assert_eq!(status["maintenance"]["final_usage_recorded"], false);
        let reply = augmentagent_phone::commands::execute(&f.service, &f.owner, &f.conversation,
            "inspect", "phone status").await.unwrap();
        assert!(reply.contains(if uncertain { "provider confirmation" } else { "cleanup attempts" }), "{reply}");
        assert!(!reply.contains("+12025550123"));
        if !uncertain {
            f.store.phone_provider_event(id, "CA-test", CallState::Canceled, 4000).unwrap();
            for now in [4000, 5000, 7000] {
                assert!(f.store.phone_claim_usage_reconciliation(id, now).unwrap());
            }
            let status = f.service.tool(&grant, "phone_status", json!({"call_id":id})).await.unwrap();
            assert_eq!(status["maintenance"]["usage_attempts"], 3);
            let reply = augmentagent_phone::commands::execute(&f.service, &f.owner, &f.conversation,
                "inspect-usage", "phone status").await.unwrap();
            assert!(reply.contains("final usage"), "{reply}");
            let reopened = Store::open(f.store.db_path()).unwrap();
            let persisted = reopened.phone_maintenance_status("canonical-owner", id).unwrap();
            assert_eq!(persisted.cleanup_attempts, 3);
            assert_eq!(persisted.usage_attempts, 3);
            assert!(reopened.phone_maintenance_status("another-owner", id).is_err());
            f.store.phone_provider_event_with_usage(id, "CA-test", CallState::Canceled, 8000,
                Some(augmentagent_store::phone::ProviderUsage { duration_seconds: 0, ended_ms: 4000 })).unwrap();
            assert!(reopened.phone_maintenance_status("canonical-owner", id).unwrap().final_usage_recorded);
            let reply = augmentagent_phone::commands::execute(&f.service, &f.owner, &f.conversation,
                "inspect-final", "phone status").await.unwrap();
            assert!(!reply.contains("final usage"), "{reply}");
        }
        assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
        assert!(f.provider.stops.lock().unwrap().is_empty());
        assert!(f.provider.usage_reads.lock().unwrap().is_empty());
        assert!(!status.to_string().contains("+12025550123"));
    }
}

#[tokio::test]
async fn ac07_disconnect_during_tool_work_cancels_without_replaying_queued_turns() {
    use augmentagent_phone::ingress::{AgentError, ConversationAgent, SpeechSender};
    use augmentagent_store::phone::{CallState, PhoneCall};
    use futures::SinkExt;
    use std::sync::atomic::AtomicUsize;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    #[derive(Default)]
    struct WorkingAgent {
        effects: AtomicUsize,
        entered: tokio::sync::Notify,
        cancelled: tokio::sync::Notify,
    }
    #[async_trait]
    impl ConversationAgent for WorkingAgent {
        async fn answer(
            &self,
            _: &PhoneCall,
            _: &str,
            _: &str,
            _: SpeechSender,
            cancel: tokio_util::sync::CancellationToken,
        ) -> Result<String, AgentError> {
            // A synthetic completed external effect followed by ongoing work.
            // Disconnect must neither repeat this effect nor start queued work.
            self.effects.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            cancel.cancelled().await;
            self.cancelled.notify_one();
            Ok("late result after disconnect".into())
        }
        async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
            Ok(())
        }
    }
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let receipt = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"disconnect"}))
        .await
        .unwrap();
    let id = receipt["id"].as_str().unwrap();
    f.store
        .phone_provider_event(id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(id, "CA-test", 100).unwrap();
    let service = Arc::new(f.service);
    let auth = Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
        "AC-test",
        "callback-secret",
    ));
    let agent = Arc::new(WorkingAgent::default());
    let router = augmentagent_phone::ingress::router(service.clone(), auth, agent.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "x-twilio-signature",
        signed(
            "callback-secret",
            &format!("wss://phone.example.test/phone/relay/{id}"),
            &[],
        )
        .parse()
        .unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();
    socket.send(Message::Text(json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"}).to_string().into())).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"start synthetic tool","last":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), agent.entered.notified())
        .await
        .unwrap();
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"queued second turn","last":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    socket.close(None).await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        agent.cancelled.notified(),
    )
    .await
    .expect("disconnect must cancel ongoing work");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !f.store.phone_provider_call(id).unwrap().state.terminal() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(agent.effects.load(Ordering::SeqCst), 1);
    assert_eq!(*f.provider.stops.lock().unwrap(), vec!["CA-test"]);
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    let reconnect = tokio_tungstenite::connect_async(request).await.unwrap_err();
    assert!(
        matches!(reconnect, tokio_tungstenite::tungstenite::Error::Http(response) if response.status().as_u16() == 409)
    );
    // Restart cleanup and redelivery of the same dial request cannot redial.
    service.maintenance(true).await.unwrap();
    let repeated = service
        .tool(&grant, "call_owner", json!({"request_id":"disconnect"}))
        .await
        .unwrap();
    assert_eq!(repeated["id"], receipt["id"]);
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
    assert_eq!(agent.effects.load(Ordering::SeqCst), 1);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn call_me_routes_directly_and_explains_missing_setup() {
    use augmentagent_phone::commands::{execute, recognizes};
    let f = fixture("discord", true, true, false);
    for text in ["call me", "Call me!", "please call me", "call me please."] {
        assert!(recognizes(text), "{text}");
    }
    for text in ["don't call me", "if I say call me", "call me tomorrow"] {
        assert!(!recognizes(text), "{text}");
    }
    f.store.phone_remove_destination("canonical-owner", 100).unwrap();
    let reply = execute(&f.service, &f.owner, &f.conversation, "missing", "call me").await.unwrap();
    assert!(reply.contains("private DM"), "{reply}");
    assert!(reply.contains("my number is"), "{reply}");
    assert!(f.provider.calls.lock().unwrap().is_empty());
    f.store.unbind_surface_owner(f.owner.account()).unwrap();
    let reply = execute(&f.service, &f.owner, &f.conversation, "unbound", "call me").await.unwrap();
    assert!(reply.contains("my number is"), "{reply}");
    assert!(f.provider.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn call_me_dials_verified_destination_once_on_replay() {
    let f = fixture("discord", true, true, false);
    for _ in 0..2 {
        let reply = augmentagent_phone::commands::execute(&f.service, &f.owner, &f.conversation, "call-me-event", "call me").await.unwrap();
        assert!(reply.contains("queued"), "{reply}");
    }
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn conversational_number_setup_defaults_to_us_and_completes_pending_call() {
    use augmentagent_phone::commands::{execute, recognizes, sensitive};
    let f = fixture("discord", true, false, false);
    f.store.phone_remove_destination("canonical-owner", 100).unwrap();
    let reply=execute(&f.service,&f.owner,&f.conversation,"ask","call me").await.unwrap();
    assert!(reply.contains("What phone number"),"{reply}");
    assert!(reply.contains("US"),"{reply}");
    assert!(recognizes("my number is (202) 555-0123"));
    assert!(sensitive("my number is (202) 555-0123"));
    let reply=execute(&f.service,&f.owner,&f.conversation,"number","my number is (202) 555-0123").await.unwrap();
    assert!(reply.contains("queued"),"{reply}");
    assert!(!reply.contains("2025550123"));
    assert!(!reply.contains("(202) 555-0123"));
    assert_eq!(f.provider.calls.lock().unwrap()[0].destination.expose(),"+12025550123");
    execute(&f.service,&f.owner,&f.conversation,"number","my number is (202) 555-0123").await.unwrap();
    assert_eq!(f.provider.calls.lock().unwrap().len(),1);
}

#[tokio::test]
async fn conversational_setup_validates_country_and_rejects_untrusted_requests() {
    use augmentagent_phone::commands::execute;
    let f=fixture("discord",true,false,false);
    let reply=execute(&f.service,&f.owner,&f.conversation,"bad","my number is 020 7946 0958").await.unwrap();
    assert!(reply.contains("country code"),"{reply}");
    assert!(f.provider.calls.lock().unwrap().is_empty());
    let reply=execute(&f.service,&f.owner,&f.conversation,"uk","my number is +44 20 7946 0958\ncall me").await.unwrap();
    assert!(reply.contains("queued"),"{reply}");
    assert_eq!(f.provider.calls.lock().unwrap()[0].destination.expose(),"+442079460958");
    f.store.unbind_surface_owner(f.owner.account()).unwrap();
    let reply=execute(&f.service,&f.owner,&f.conversation,"forged","my number is 2025550199").await.unwrap();
    assert!(reply.contains("authorization"));
    assert_eq!(f.provider.calls.lock().unwrap().len(),1);
}

#[tokio::test]
async fn conversational_setup_expires_and_old_messages_cannot_replace_new_number() {
    use augmentagent_phone::commands::execute;
    let f=fixture("discord",true,false,false);
    f.store.phone_remove_destination("canonical-owner",100).unwrap();
    execute(&f.service,&f.owner,&f.conversation,"ask-expire","call me").await.unwrap();
    f.clock.0.store(900_101,Ordering::SeqCst);
    let reply=execute(&f.service,&f.owner,&f.conversation,"first","2025550123").await.unwrap();
    assert!(reply.contains("Saved"),"{reply}");
    assert!(f.provider.calls.lock().unwrap().is_empty());
    execute(&f.service,&f.owner,&f.conversation,"second","my number is 2025550199").await.unwrap();
    execute(&f.service,&f.owner,&f.conversation,"first","2025550123").await.unwrap();
    execute(&f.service,&f.owner,&f.conversation,"dial","call me").await.unwrap();
    assert_eq!(f.provider.calls.lock().unwrap()[0].destination.expose(),"+12025550199");
}

#[tokio::test]
async fn failed_relay_reports_failure_instead_of_silent_hangup() {
    use augmentagent_phone::webhook::WebhookAuth;
    let f=fixture("discord",true,true,false);
    let grant=f.service.authorize(&f.owner,&f.conversation).unwrap();
    let call=f.service.tool(&grant,"call_owner",json!({"request_id":"relay-failure"})).await.unwrap();
    let id=call["id"].as_str().unwrap();
    let auth=WebhookAuth::new("AC-test","secret");
    let fields=vec![("AccountSid".into(),"AC-test".into()),("CallSid".into(),"CA-test".into()),("SessionStatus".into(),"failed".into()),("ErrorCode".into(),"64102".into())];
    assert!(f.service.relay_ended_callback(&auth,id,&fields,"invalid").is_err());
    let sig=signed("secret",&format!("https://phone.example.test/phone/relay-ended/{id}"),&fields);
    let reply=f.service.relay_ended_callback(&auth,id,&fields,&sig).unwrap();
    assert!(reply.contains("could not connect"),"{reply}");
    assert!(reply.contains("<Say "));
}

#[tokio::test]
async fn relay_startup_bounds_control_frames_and_keeps_one_deadline() {
    for scenario in ["flood", "deadline", "binary", "malformed"] {
    use augmentagent_store::phone::CallState;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 101).unwrap();
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        Arc::new(ScriptedAgent),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{id}"),
        &[],
    );
    request
        .headers_mut()
        .insert("x-twilio-signature", signature.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();

        let started = std::time::Instant::now();
        if scenario == "flood" {
            for _ in 0..33 { let _ = socket.send(Message::Pong(vec![1].into())).await; }
        } else if scenario == "binary" {
            socket.send(Message::Binary(vec![1].into())).await.unwrap();
        } else if scenario == "malformed" {
            socket.send(Message::Text("not json".into())).await.unwrap();
        }
        let closed = tokio::time::timeout(std::time::Duration::from_secs(6), async {
            loop {
                tokio::select! {
                    frame = socket.next() => match frame {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                        _ => {},
                    },
                    _ = tokio::time::sleep(std::time::Duration::from_millis(250)), if scenario == "deadline" => {
                        if socket.send(Message::Pong(vec![1].into())).await.is_err() { break; }
                    }
                }
            }
        }).await;
        assert!(closed.is_ok(), "{scenario} must close within the original deadline");
        if scenario == "deadline" { assert!(started.elapsed() >= std::time::Duration::from_secs(4)); }
        else { assert!(started.elapsed() < std::time::Duration::from_secs(2)); }
        assert_eq!(f.store.with_conn(|c| c.query_row("SELECT count(*) FROM phone_relay_claims",[],|r|r.get::<_,i64>(0))).unwrap(),0);
        server.abort();
    }
}

#[tokio::test]
async fn owner_requested_call_connects_directly_with_female_voice() {
    let f = fixture("discord", true, true, false);
    let (id, reply) = start_owner_call(&f).await;
    assert!(reply.contains("ConversationRelay"));
    assert!(!reply.contains("Gather"));
    let relay = relay_attributes(&reply);
    assert_eq!(relay["language"], "en-US");
    assert_eq!(
        f.store
            .phone_call("canonical-owner", &id)
            .unwrap()
            .state
            .as_str(),
        "connected"
    );
    let fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
    ];
    let auth = augmentagent_phone::webhook::WebhookAuth::new("AC-test", "secret");
    let sig = signed(
        "secret",
        &format!("https://phone.example.test/phone/start/{id}"),
        &fields,
    );
    assert!(f
        .service
        .start_callback(&auth, &id, &fields, &sig)
        .unwrap()
        .contains("Hangup"));
    assert_eq!(f.provider.calls.lock().unwrap().len(), 1);
}

/// Signed owner start callback; returns the call ID and the TwiML reply.
async fn start_owner_call(f: &Fixture) -> (String, String) {
    use augmentagent_phone::webhook::WebhookAuth;
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"direct-answer"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_owned();
    let fields = vec![
        ("AccountSid".into(), "AC-test".into()),
        ("CallSid".into(), "CA-test".into()),
    ];
    let auth = WebhookAuth::new("AC-test", "secret");
    let sig = signed(
        "secret",
        &format!("https://phone.example.test/phone/start/{id}"),
        &fields,
    );
    let reply = f.service.start_callback(&auth, &id, &fields, &sig).unwrap();
    (id, reply)
}
/// Parses the TwiML as XML (not substring matching) and returns the decoded
/// attributes of the single ConversationRelay element.
fn relay_attributes(twiml: &str) -> std::collections::BTreeMap<String, String> {
    let doc = roxmltree::Document::parse(twiml).expect("TwiML must be well-formed XML");
    let mut relays = doc
        .descendants()
        .filter(|n| n.has_tag_name("ConversationRelay"));
    let relay = relays.next().expect("one ConversationRelay element");
    assert!(relays.next().is_none(), "exactly one ConversationRelay");
    relay
        .attributes()
        .map(|a| (a.name().to_owned(), a.value().to_owned()))
        .collect()
}
fn voice_from(pairs: &[(&str, &str)]) -> augmentagent_phone::voice::VoiceConfig {
    let env: std::collections::HashMap<&str, String> = pairs
        .iter()
        .map(|(k, v)| (*k, (*v).to_owned()))
        .collect();
    augmentagent_phone::voice::VoiceConfig::from_lookup(|k| env.get(k).cloned()).unwrap()
}
const DEFAULT_HINTS: &str = "Jarvis, Discord, WhatsApp, Slack, Linear, Notion";

#[tokio::test]
async fn ac01_default_twiml_pins_deepgram_nova3_and_elevenlabs_flash() {
    let f = fixture("discord", true, true, false);
    let (_, reply) = start_owner_call(&f).await;
    let relay = relay_attributes(&reply);
    assert_eq!(relay["transcriptionProvider"], "Deepgram");
    assert_eq!(relay["speechModel"], "nova-3-general");
    assert_eq!(relay["ttsProvider"], "ElevenLabs");
    assert_eq!(relay["voice"], "UgBBYS2sOqTuMpoF3BR0-flash_v2_5");
    assert_eq!(relay["language"], "en-US");
    assert_eq!(relay["elevenlabsTextNormalization"], "on");
    assert_eq!(relay["hints"], DEFAULT_HINTS);
    // Still an owner-authorized direct connect with the existing turn contract.
    assert_eq!(relay["interruptible"], "any");
    assert_eq!(relay["partialPrompts"], "false");
    assert_eq!(
        relay["url"],
        format!(
            "wss://phone.example.test/phone/relay/{}",
            f.store.phone_active_calls().unwrap()[0].id
        )
    );
}

#[tokio::test]
async fn ac02_each_provider_variable_changes_only_its_own_attribute() {
    let defaults = fixture("discord", true, true, false);
    let (_, reply) = start_owner_call(&defaults).await;
    let baseline = relay_attributes(&reply);
    // (env overrides, expected attribute differences from the baseline; None = absent)
    type Overrides<'a> = Vec<(&'a str, &'a str)>;
    type Expected<'a> = Vec<(&'a str, Option<&'a str>)>;
    let matrix: Vec<(Overrides, Expected)> = vec![
        (
            vec![("AUGMENTAGENT_PHONE_STT_MODEL", "nova-2-phonecall")],
            vec![("speechModel", Some("nova-2-phonecall"))],
        ),
        (
            vec![("AUGMENTAGENT_PHONE_STT_PROVIDER", "Google")],
            vec![
                ("transcriptionProvider", Some("Google")),
                ("speechModel", Some("telephony")),
            ],
        ),
        (
            vec![
                ("AUGMENTAGENT_PHONE_STT_PROVIDER", "Google"),
                ("AUGMENTAGENT_PHONE_STT_MODEL", "long"),
            ],
            vec![
                ("transcriptionProvider", Some("Google")),
                ("speechModel", Some("long")),
            ],
        ),
        (
            vec![("AUGMENTAGENT_PHONE_TTS_VOICE", "UgBBYS2sOqTuMpoF3BR0-flash_v2_5-1.0_0.5_0.75")],
            vec![("voice", Some("UgBBYS2sOqTuMpoF3BR0-flash_v2_5-1.0_0.5_0.75"))],
        ),
        (
            vec![("AUGMENTAGENT_PHONE_TTS_PROVIDER", "Amazon")],
            vec![
                ("ttsProvider", Some("Amazon")),
                ("voice", Some("Joanna")),
                ("elevenlabsTextNormalization", None),
            ],
        ),
        (
            vec![
                ("AUGMENTAGENT_PHONE_TTS_PROVIDER", "Amazon"),
                ("AUGMENTAGENT_PHONE_TTS_VOICE", "Matthew-Generative"),
            ],
            vec![
                ("ttsProvider", Some("Amazon")),
                ("voice", Some("Matthew-Generative")),
                ("elevenlabsTextNormalization", None),
            ],
        ),
    ];
    assert_eq!(matrix.len(), 6);
    for (env, expected) in matrix {
        let f = fixture_with_voice("discord", true, true, false, voice_from(&env), None);
        let (_, reply) = start_owner_call(&f).await;
        let relay = relay_attributes(&reply);
        let mut want = baseline.clone();
        for (name, value) in &expected {
            match value {
                Some(v) => {
                    want.insert((*name).to_owned(), (*v).to_owned());
                }
                None => {
                    want.remove(*name);
                }
            }
        }
        // Call IDs differ per fixture; everything else must match exactly.
        want.remove("url");
        let mut got = relay.clone();
        got.remove("url");
        assert_eq!(got, want, "overrides {env:?}");
    }
}

#[test]
fn ac04_relay_twiml_reads_only_startup_configuration() {
    // Source-level guard: the TwiML builder may read `self.config` and the
    // call ID only. Chat text, provider fields, tool arguments and model output
    // never reach it, so no attribute can be influenced by them.
    let source = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/webhook.rs"
    ))
    .unwrap();
    let start = source
        .find("fn relay_twiml(&self, id: &str) -> String {")
        .expect("relay_twiml signature unchanged");
    let body = &source[start..];
    let end = body.find("\n    }\n").expect("end of relay_twiml") + 7;
    let body = &body[..end];
    assert!(body.contains("ConversationRelay"), "{body}");
    for forbidden in [
        "fields",
        "args",
        "call.",
        "store",
        "Value",
        "serde_json",
        "std::env",
        "env::var",
        "prompt",
        "text",
    ] {
        assert!(!body.contains(forbidden), "relay_twiml must not read {forbidden}: {body}");
    }
    let mut cursor = body;
    let mut reads = 0;
    while let Some(i) = cursor.find("self.") {
        assert!(
            cursor[i..].starts_with("self.config."),
            "relay_twiml reads something other than self.config: {body}"
        );
        reads += 1;
        cursor = &cursor[i + 5..];
    }
    assert!(reads >= 4, "expected provider reads through self.config");
    // The only other binding read is the call identifier parameter.
    let other_user = source.contains("fn relay_twiml(&self, id: &str)");
    assert!(other_user);
}

#[tokio::test]
async fn ac05_hints_include_stored_first_name_only_when_present_and_are_escaped_and_capped() {
    let plain = fixture_with_voice(
        "discord",
        true,
        true,
        false,
        augmentagent_phone::voice::VoiceConfig::default(),
        None,
    );
    let (_, reply) = start_owner_call(&plain).await;
    assert_eq!(relay_attributes(&reply)["hints"], DEFAULT_HINTS);

    let named = fixture_with_voice(
        "discord",
        true,
        true,
        false,
        augmentagent_phone::voice::VoiceConfig::default(),
        Some("Nolan"),
    );
    let (_, reply) = start_owner_call(&named).await;
    assert_eq!(
        relay_attributes(&reply)["hints"],
        format!("Nolan, {DEFAULT_HINTS}")
    );

    // Markup in a stored name is removed before emission, apostrophes survive
    // through XML escaping, and the raw characters never appear in the TwiML.
    let hostile = fixture_with_voice(
        "discord",
        true,
        true,
        false,
        augmentagent_phone::voice::VoiceConfig::default(),
        Some("D'Arcy <b>&amp;\" x=\"y"),
    );
    let (_, reply) = start_owner_call(&hostile).await;
    assert!(!reply.contains("<b>"));
    assert!(!reply.contains("x=\"y"));
    assert_eq!(
        relay_attributes(&reply)["hints"],
        format!("D'Arcy bamp xy, {DEFAULT_HINTS}")
    );
    assert_eq!(
        augmentagent_phone::voice::hints(Some("O'Brien")),
        format!("O'Brien, {DEFAULT_HINTS}")
    );
    assert_eq!(augmentagent_phone::voice::hints(Some("   ")), DEFAULT_HINTS);
    // Oversized names are dropped rather than truncated mid-word, and the
    // whole attribute is capped at 500 characters.
    assert_eq!(
        augmentagent_phone::voice::hints(Some(&"N".repeat(65))),
        DEFAULT_HINTS
    );
    assert!(augmentagent_phone::voice::hints(Some(&"N".repeat(64))).chars().count() <= 500);
    const { assert!(augmentagent_phone::voice::HINTS_MAX_CHARS == 500) };
}

struct SlowPhoneAgent {
    permits: tokio::sync::Semaphore,
}
#[async_trait]
impl augmentagent_phone::ingress::ConversationAgent for SlowPhoneAgent {
    async fn answer(
        &self,
        _: &augmentagent_store::phone::PhoneCall,
        _: &str,
        _: &str,
        _: augmentagent_phone::ingress::SpeechSender,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, augmentagent_phone::ingress::AgentError> {
        tokio::select! {
            _ = cancel.cancelled() => Err(augmentagent_phone::ingress::AgentError),
            permit = self.permits.acquire() => { permit.unwrap().forget(); Ok("Here is the result.".into()) }
        }
    }
    async fn interrupted(
        &self,
        _: &augmentagent_store::phone::PhoneCall,
        _: &str,
    ) -> Result<(), augmentagent_phone::ingress::AgentError> {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        Err(augmentagent_phone::ingress::AgentError)
    }
}
#[tokio::test]
async fn slow_work_acknowledges_and_slow_interruption_mirror_does_not_hang_up() {
    let agent = Arc::new(SlowPhoneAgent {
        permits: tokio::sync::Semaphore::new(0),
    });
    use augmentagent_store::phone::CallState;
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 101).unwrap();
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        agent.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{id}"),
        &[],
    );
    request
        .headers_mut()
        .insert("x-twilio-signature", signature.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();

    socket.send(Message::Text(json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"}).to_string().into())).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"look up something","last":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let ack = tokio::time::timeout(std::time::Duration::from_millis(500), socket.next())
        .await
        .expect("ack must not wait for model/tools")
        .unwrap()
        .unwrap();
    assert!(ack.into_text().unwrap().contains("Got it"));
    socket.send(Message::Text(json!({"type":"interrupt","utteranceUntilInterrupt":"Got it","durationUntilInterruptMs":100}).to_string().into())).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"prompt","voicePrompt":"actually something else","last":true})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    let ack = tokio::time::timeout(std::time::Duration::from_millis(500), socket.next())
        .await
        .expect("slow transcript mirror must not block speech")
        .unwrap()
        .unwrap();
    assert!(ack.into_text().unwrap().contains("Got it"));
    agent.permits.add_permits(2);
    let answer = tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(answer.into_text().unwrap().contains("Here is the result"));
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    socket.send(Message::Ping(vec![1].into())).await.unwrap();
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Pong(_)
    ));
    assert!(
        !f.store
            .phone_call("canonical-owner", &id)
            .unwrap()
            .stop_requested
    );
    server.abort();
}

#[test]
fn ac06_twiml_debug_attribute_present_iff_debug_events_enabled() {
    use augmentagent_phone::webhook::WebhookAuth;
    for debug in [false, true] {
        let f = fixture("discord", true, true, false);
        let config = PhoneConfig::new(
            true,
            "https://phone.example.test",
            "test-secret-for-verification-32bytes",
            vec![OwnerLink {
                surface_owner: f.owner.clone(),
                owner_id: "canonical-owner".into(),
            }],
        )
        .unwrap()
        .with_debug_events(debug);
        let service = PhoneService::new(f.store.clone(), f.provider.clone(), f.clock.clone(), config);
        assert_eq!(service.debug_events(), debug);
        let grant = service.authorize(&f.owner, &f.conversation).unwrap();
        let call = futures::executor::block_on(service.tool(&grant, "call_owner", json!({"request_id":"debug"}))).unwrap();
        let id = call["id"].as_str().unwrap();
        let fields = vec![
            ("AccountSid".into(), "AC-test".into()),
            ("CallSid".into(), "CA-test".into()),
        ];
        let auth = WebhookAuth::new("AC-test", "secret");
        let sig = signed("secret", &format!("https://phone.example.test/phone/start/{id}"), &fields);
        let reply = service.start_callback(&auth, id, &fields, &sig).unwrap();
        assert!(reply.contains("<ConversationRelay "), "{reply}");
        assert_eq!(
            reply.contains(r#" debug="debugging tokens-played speaker-events""#),
            debug,
            "{reply}"
        );
    }
}

/// Captures every tracing event so #1551 tests can count `phone.turn` records
/// and prove raw debug frame bodies never reach the logs.
mod capture {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex, OnceLock},
    };
    use tracing_subscriber::layer::SubscriberExt;
    #[derive(Default)]
    pub struct Captured {
        pub events: Mutex<Vec<(String, BTreeMap<String, String>)>>,
    }
    struct Layer(Arc<Captured>);
    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Layer {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Fields(BTreeMap<String, String>);
            impl tracing::field::Visit for Fields {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    self.0.insert(field.name().into(), format!("{value:?}"));
                }
            }
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            self.0
                .events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((event.metadata().target().into(), fields.0));
        }
    }
    static CAPTURED: OnceLock<Arc<Captured>> = OnceLock::new();
    pub fn install() -> Arc<Captured> {
        CAPTURED
            .get_or_init(|| {
                let captured = Arc::new(Captured::default());
                tracing::subscriber::set_global_default(
                    tracing_subscriber::registry().with(Layer(captured.clone())),
                )
                .expect("only this module installs a global subscriber");
                captured
            })
            .clone()
    }
}

struct MixedOutcomeAgent {
    release: tokio::sync::Semaphore,
}
#[async_trait]
impl augmentagent_phone::ingress::ConversationAgent for MixedOutcomeAgent {
    async fn answer(
        &self,
        _: &augmentagent_store::phone::PhoneCall,
        _: &str,
        text: &str,
        speech: augmentagent_phone::ingress::SpeechSender,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, augmentagent_phone::ingress::AgentError> {
        match text {
            "fail please" => Err(augmentagent_phone::ingress::AgentError),
            "slow lookup" => tokio::select! {
                _ = cancel.cancelled() => Err(augmentagent_phone::ingress::AgentError),
                permit = self.release.acquire() => { permit.unwrap().forget(); Ok("Late result.".into()) }
            },
            _ => {
                speech.note_tool_time(std::time::Duration::from_millis(40));
                Ok(format!("Answer to: {text}"))
            }
        }
    }
    async fn interrupted(
        &self,
        _: &augmentagent_store::phone::PhoneCall,
        _: &str,
    ) -> Result<(), augmentagent_phone::ingress::AgentError> {
        Ok(())
    }
}

#[tokio::test]
async fn ac01_every_turn_records_one_metrics_row_and_event_with_debug_played_timing() {
    use augmentagent_store::phone::{CallState, TurnOutcome};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let captured = capture::install();
    let agent = Arc::new(MixedOutcomeAgent {
        release: tokio::sync::Semaphore::new(0),
    });
    let f = fixture("discord", true, true, false);
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "test-secret-for-verification-32bytes",
        vec![OwnerLink {
            surface_owner: f.owner.clone(),
            owner_id: "canonical-owner".into(),
        }],
    )
    .unwrap()
    .with_debug_events(true);
    let service = Arc::new(PhoneService::new(
        f.store.clone(),
        f.provider.clone(),
        f.clock.clone(),
        config,
    ));
    let grant = service.authorize(&f.owner, &f.conversation).unwrap();
    let call = service
        .tool(&grant, "call_owner", json!({"request_id":"metrics"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 101).unwrap();
    let router = augmentagent_phone::ingress::router(
        service,
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        agent.clone(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{id}"),
        &[],
    );
    request
        .headers_mut()
        .insert("x-twilio-signature", signature.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket.send(Message::Text(json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"}).to_string().into())).await.unwrap();
    async fn next_text(
        socket: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> String {
        tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .expect("reply")
            .unwrap()
            .unwrap()
            .into_text()
            .unwrap()
            .to_string()
    }
    // Turn 1: answered, then provider reports tokens played (debug events).
    socket.send(Message::Text(json!({"type":"prompt","voicePrompt":"hello","last":true}).to_string().into())).await.unwrap();
    assert!(next_text(&mut socket).await.contains("Answer to: hello"));
    socket.send(Message::Text(json!({"type":"info","name":"tokensPlayed","value":"raw-debug-marker-one","last":false}).to_string().into())).await.unwrap();
    socket.send(Message::Text(json!({"type":"info","name":"tokensPlayed","value":"raw-debug-marker-two","last":true}).to_string().into())).await.unwrap();
    socket.send(Message::Text(json!({"type":"info","name":"speakerEvent","value":"raw-debug-marker-three"}).to_string().into())).await.unwrap();
    socket.send(Message::Text(json!({"type":"unknownFutureFrame","body":"raw-debug-marker-four"}).to_string().into())).await.unwrap();
    // Turn 2: the agent fails; the spoken failure notice closes the turn.
    socket.send(Message::Text(json!({"type":"prompt","voicePrompt":"fail please","last":true}).to_string().into())).await.unwrap();
    assert!(next_text(&mut socket).await.contains("Sorry"));
    // Turn 3: interrupted while the agent is still working (after the ack).
    socket.send(Message::Text(json!({"type":"prompt","voicePrompt":"slow lookup","last":true}).to_string().into())).await.unwrap();
    assert!(next_text(&mut socket).await.contains("Got it"));
    socket.send(Message::Text(json!({"type":"interrupt","utteranceUntilInterrupt":"Got it","durationUntilInterruptMs":100}).to_string().into())).await.unwrap();
    // Turn 4: answered after the stalled turn releases.
    socket.send(Message::Text(json!({"type":"prompt","voicePrompt":"hello again","last":true}).to_string().into())).await.unwrap();
    // The ack proves the interrupt and new prompt were processed before the
    // stalled turn is released; its late output is suppressed, not spoken.
    assert!(next_text(&mut socket).await.contains("Got it"));
    agent.release.add_permits(1);
    let reply = next_text(&mut socket).await;
    assert!(reply.contains("Answer to: hello again"), "{reply}");
    socket.close(None).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if f.store.phone_call("canonical-owner", &id).unwrap().state.terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.abort();

    let rows = f.store.phone_turn_metrics(&id).unwrap();
    assert_eq!(rows.len(), 4, "{rows:?}");
    let outcomes: Vec<_> = rows.iter().map(|r| r.outcome).collect();
    assert_eq!(
        outcomes,
        [
            TurnOutcome::Answered,
            TurnOutcome::Failed,
            TurnOutcome::Interrupted,
            TurnOutcome::Answered
        ]
    );
    assert!(rows.iter().all(|r| r.turn_id.starts_with(&format!("phone:{id}:"))));
    // Debug on: the played turn carries audible timing; others have speech only.
    let first = &rows[0];
    assert!(first.t_first_speech_ms.is_some() && first.t_last_speech_ms.is_some());
    assert!(first.t_first_played_ms.is_some(), "{first:?}");
    assert!(first.t_last_played_ms >= first.t_first_played_ms);
    assert_eq!(
        first.audible_delta_ms,
        Some(first.t_first_played_ms.unwrap() - first.t_first_speech_ms.unwrap())
    );
    assert_eq!(first.tool_ms, Some(40));
    assert!(first.queue_wait_ms.is_some() && first.t_first_token_ms.is_some());
    assert!(rows[1].t_first_speech_ms.is_some() && rows[1].t_first_played_ms.is_none());
    assert!(rows[2].t_first_speech_ms.is_none(), "{:?}", rows[2]);
    assert!(rows[3].t_first_speech_ms.is_some());
    assert!(rows[3].queue_wait_ms.unwrap() >= rows[0].queue_wait_ms.unwrap());
    // Exactly one structured event per turn, and no raw debug bodies anywhere.
    let events = captured
        .events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let turn_events: Vec<_> = events
        .iter()
        .filter(|(target, fields)| target == "phone.turn" && fields.get("call_id").is_some_and(|c| c.contains(&id)))
        .collect();
    assert_eq!(turn_events.len(), 4, "{turn_events:?}");
    for (_, fields) in &turn_events {
        assert!(fields.contains_key("outcome") && fields.contains_key("turn_id"), "{fields:?}");
    }
    // Absent (null) fields are omitted by tracing; the played turn carries all of them.
    let played = &turn_events[0].1;
    for key in ["t_first_speech_ms", "t_last_speech_ms", "t_first_played_ms", "audible_delta_ms", "tool_ms", "queue_wait_ms"] {
        assert!(played.contains_key(key), "{key} missing from {played:?}");
    }
    let everything = format!("{events:?}");
    for private in ["raw-debug-marker", "hello again", "fail please", "voicePrompt"] {
        assert!(!everything.contains(private), "{private} leaked into logs");
    }
}

#[tokio::test]
async fn ac06_without_debug_events_played_fields_are_null_and_others_unchanged() {
    use augmentagent_store::phone::{CallState, TurnOutcome};
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"metrics-off"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 101).unwrap();
    assert!(!f.service.debug_events());
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        Arc::new(ScriptedAgent),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{id}"),
        &[],
    );
    request
        .headers_mut()
        .insert("x-twilio-signature", signature.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket.send(Message::Text(json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"}).to_string().into())).await.unwrap();
    socket.send(Message::Text(json!({"type":"prompt","voicePrompt":"hello","last":true}).to_string().into())).await.unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(reply.into_text().unwrap().contains("Answer to: hello"));
    // Info frames are tolerated but attach no played timing when debug is off.
    socket.send(Message::Text(json!({"type":"info","name":"tokensPlayed","value":"x","last":true}).to_string().into())).await.unwrap();
    socket.close(None).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if f.store.phone_call("canonical-owner", &id).unwrap().state.terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
    let rows = f.store.phone_turn_metrics(&id).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].outcome, TurnOutcome::Answered);
    assert!(rows[0].t_first_speech_ms.is_some() && rows[0].t_last_speech_ms.is_some());
    assert!(rows[0].queue_wait_ms.is_some() && rows[0].t_first_token_ms.is_some());
    assert_eq!(rows[0].t_first_played_ms, None);
    assert_eq!(rows[0].t_last_played_ms, None);
    assert_eq!(rows[0].audible_delta_ms, None);
}

// ---- #1549: recoverable conditions never close the relay socket ----

/// Answers `Answer to: <text>` once a permit is available, so turns can be
/// held in the queue deterministically.
struct GatedAgent {
    permits: tokio::sync::Semaphore,
    entered: tokio::sync::Notify,
}
#[async_trait]
impl augmentagent_phone::ingress::ConversationAgent for GatedAgent {
    async fn answer(
        &self,
        _: &augmentagent_store::phone::PhoneCall,
        _: &str,
        text: &str,
        _: augmentagent_phone::ingress::SpeechSender,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, augmentagent_phone::ingress::AgentError> {
        self.entered.notify_one();
        tokio::select! {
            _ = cancel.cancelled() => Err(augmentagent_phone::ingress::AgentError),
            permit = self.permits.acquire() => { permit.unwrap().forget(); Ok(format!("Answer to: {text}")) }
        }
    }
    async fn interrupted(
        &self,
        _: &augmentagent_store::phone::PhoneCall,
        _: &str,
    ) -> Result<(), augmentagent_phone::ingress::AgentError> {
        Ok(())
    }
}

type RelaySocket = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;
struct RelayHarness {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    provider: Arc<FakeProvider>,
    id: String,
    socket: RelaySocket,
    server: tokio::task::JoinHandle<()>,
}
/// Signed upgrade plus accepted setup on a connected owner call.
async fn connect_relay(
    agent: Arc<dyn augmentagent_phone::ingress::ConversationAgent>,
) -> RelayHarness {
    use augmentagent_store::phone::CallState;
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let f = fixture("discord", true, true, false);
    let grant = f.service.authorize(&f.owner, &f.conversation).unwrap();
    let call = f
        .service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let id = call["id"].as_str().unwrap().to_string();
    f.store
        .phone_provider_event(&id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    f.store.phone_accept(&id, "CA-test", 101).unwrap();
    let router = augmentagent_phone::ingress::router(
        Arc::new(f.service),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        agent,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{id}"),
        &[],
    );
    request
        .headers_mut()
        .insert("x-twilio-signature", signature.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    socket
        .send(Message::Text(
            json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
    RelayHarness {
        _dir: f._dir,
        store: f.store,
        provider: f.provider,
        id,
        socket,
        server,
    }
}
fn prompt(text: &str) -> tokio_tungstenite::tungstenite::Message {
    tokio_tungstenite::tungstenite::Message::Text(
        json!({"type":"prompt","voicePrompt":text,"last":true})
            .to_string()
            .into(),
    )
}
/// Reads text frames until one carries `token` with `last:true`, returning
/// every token seen on the way. Panics on close or timeout.
async fn read_until_answer(socket: &mut RelaySocket, token: &str, context: &str) -> Vec<String> {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;
    let mut seen = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if body["type"] == "end" {
                        panic!("{context}: relay sent end before answering");
                    }
                    let spoken = body["token"].as_str().unwrap().to_string();
                    let last = body["last"] == true;
                    seen.push(spoken.clone());
                    if spoken == token {
                        assert!(last, "{context}: answer must be a final text frame");
                        return;
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                other => panic!("{context}: socket closed before answer: {other:?}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{context}: no answer within deadline"));
    seen
}

#[tokio::test]
async fn ac01_recoverable_frames_keep_relay_open_and_next_prompt_is_answered() {
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let scenarios: [(&str, Message); 5] = [
        ("binary", Message::Binary(vec![0xde, 0xad].into())),
        ("malformed", Message::Text("{not json".into())),
        (
            "unknown_type",
            Message::Text(json!({"type":"mystery","payload":"x"}).to_string().into()),
        ),
        (
            "provider_64107",
            Message::Text(
                json!({"type":"error","code":64107,"description":"invalid message"})
                    .to_string()
                    .into(),
            ),
        ),
        (
            "provider_unknown",
            Message::Text(
                json!({"type":"error","code":64199,"description":"unknown"})
                    .to_string()
                    .into(),
            ),
        ),
    ];
    for (name, frame) in scenarios {
        let mut relay = connect_relay(Arc::new(ScriptedAgent)).await;
        relay.socket.send(prompt("before")).await.unwrap();
        read_until_answer(&mut relay.socket, "Answer to: before", name).await;
        relay.socket.send(frame).await.unwrap();
        relay.socket.send(prompt("after")).await.unwrap();
        read_until_answer(&mut relay.socket, "Answer to: after", name).await;
        assert!(
            !relay
                .store
                .phone_call("canonical-owner", &relay.id)
                .unwrap()
                .stop_requested,
            "{name}: recoverable condition must not request a stop"
        );
        assert!(relay.provider.stops.lock().unwrap().is_empty(), "{name}");
        relay.server.abort();
    }
}

#[tokio::test]
async fn ac02_queue_overflow_drops_oldest_turn_speaks_recovery_once_and_answers_newest() {
    use futures::SinkExt;
    let agent = Arc::new(GatedAgent {
        permits: tokio::sync::Semaphore::new(0),
        entered: tokio::sync::Notify::new(),
    });
    let mut relay = connect_relay(agent.clone()).await;
    // Turn one is in progress; four queue; the sixth overflows the queue.
    relay.socket.send(prompt("one")).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), agent.entered.notified())
        .await
        .unwrap();
    for text in ["two", "three", "four", "five", "six"] {
        relay.socket.send(prompt(text)).await.unwrap();
    }
    agent.permits.add_permits(6);
    let seen = read_until_answer(&mut relay.socket, "Answer to: six", "overflow").await;
    let recovery = seen
        .iter()
        .filter(|t| t.as_str() == "Sorry, say that again?")
        .count();
    assert_eq!(recovery, 1, "exactly one recovery line: {seen:?}");
    for answered in ["one", "three", "four", "five", "six"] {
        assert!(
            seen.iter().any(|t| t == &format!("Answer to: {answered}")),
            "{answered} must still be answered: {seen:?}"
        );
    }
    assert!(
        !seen.iter().any(|t| t == "Answer to: two"),
        "the oldest queued turn is dropped: {seen:?}"
    );
    assert!(
        !relay
            .store
            .phone_call("canonical-owner", &relay.id)
            .unwrap()
            .stop_requested
    );
    relay.server.abort();
}

#[tokio::test]
async fn ac03_ten_consecutive_invalid_frames_send_end_then_close_and_a_valid_frame_resets() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let malformed = || Message::Text("{bad".into());
    // Exactly ten in a row: one `end` frame, then the socket closes.
    let mut relay = connect_relay(Arc::new(ScriptedAgent)).await;
    for _ in 0..10 {
        relay.socket.send(malformed()).await.unwrap();
    }
    let mut ends = 0;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            match relay.socket.next().await {
                Some(Ok(Message::Text(text))) => {
                    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(body["type"], "end", "only an end frame may follow: {text}");
                    ends += 1;
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break,
                other => panic!("unexpected frame {other:?}"),
            }
        }
    })
    .await
    .expect("socket must close after the invalid frame limit");
    assert_eq!(ends, 1, "exactly one end frame");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !relay.store.phone_provider_call(&relay.id).unwrap().state.terminal() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    relay.server.abort();
    // Nine, one valid frame, nine more: the counter reset keeps the socket open.
    let mut relay = connect_relay(Arc::new(ScriptedAgent)).await;
    for _ in 0..9 {
        relay.socket.send(malformed()).await.unwrap();
    }
    relay.socket.send(prompt("reset")).await.unwrap();
    read_until_answer(&mut relay.socket, "Answer to: reset", "reset").await;
    for _ in 0..9 {
        relay.socket.send(malformed()).await.unwrap();
    }
    relay.socket.send(prompt("still open")).await.unwrap();
    read_until_answer(&mut relay.socket, "Answer to: still open", "still open").await;
    assert!(
        !relay
            .store
            .phone_call("canonical-owner", &relay.id)
            .unwrap()
            .stop_requested
    );
    relay.server.abort();
}

#[test]
fn ac04_terminal_end_reasons_are_exactly_the_documented_set() {
    use augmentagent_phone::ingress::EndReason;
    // A wildcard-free match fails to compile if a variant is added or removed.
    let name = |reason: EndReason| match reason {
        EndReason::PeerClosed => "peer_closed",
        EndReason::TransportError => "transport_error",
        EndReason::OwnerHangup => "owner_hangup",
        EndReason::Revoked => "revoked",
        EndReason::DurationLimit => "duration_limit",
        EndReason::WorkerFailed => "worker_failed",
        EndReason::InvalidFrameLimit => "invalid_frame_limit",
    };
    assert_eq!(EndReason::ALL.len(), 7);
    let mut names: Vec<&str> = EndReason::ALL.iter().map(|r| r.as_str()).collect();
    names.sort_unstable();
    let mut expected = vec![
        "peer_closed",
        "transport_error",
        "owner_hangup",
        "revoked",
        "duration_limit",
        "worker_failed",
        "invalid_frame_limit",
    ];
    expected.sort_unstable();
    assert_eq!(names, expected);
    for reason in EndReason::ALL {
        assert_eq!(reason.as_str(), name(reason));
    }
}
