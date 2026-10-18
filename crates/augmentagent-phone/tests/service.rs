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
    .unwrap();
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
async fn ac05_signed_start_then_accept_is_the_only_path_to_relay() {
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
    assert!(greeting.contains("Gather"));
    assert!(!greeting.contains("ConversationRelay"));
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
    assert!(relay.contains("ConversationRelay"));
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
    assert!(response.text().await.unwrap().contains("Gather"));
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
    let setup =
        json!({"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX-test"})
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
            assert!(reply.contains("Gather"));
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
