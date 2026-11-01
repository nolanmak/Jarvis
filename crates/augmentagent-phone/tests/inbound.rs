use async_trait::async_trait;
use augmentagent_phone::{
    inbound::InboundConfig,
    ingress::{AgentError, ConversationAgent, SpeechSender},
    webhook::WebhookAuth,
    Clock, DialRequest, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneCall},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc,
};
use tokio_util::sync::CancellationToken;

const ORIGIN: &str = "https://phone.example.test";
const SID: &str = "CA00000000000000000000000000000001";
const HANGUP: &str = "<Response><Hangup/></Response>";

struct TestClock(AtomicI64);
impl Clock for TestClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct NoDial;
#[async_trait]
impl Telephony for NoDial {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        panic!("incoming calls must never dial")
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
        Ok("hello".into())
    }
    async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
        Ok(())
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    service: Arc<PhoneService>,
    clock: Arc<TestClock>,
    owner: SurfaceOwnerRef,
    conversation: SurfaceConversationRef,
    auth: Arc<WebhookAuth>,
}
fn fixture(enabled: bool, inbound: bool, live: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "dm:bot").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let conversation = SurfaceConversationRef::new(account, "private-chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&conversation, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("canonical", "+12025550123", 0)
        .unwrap();
    let mut config = PhoneConfig::new(
        enabled,
        ORIGIN,
        "test-verification-secret-32-bytes-long",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical".into(),
        }],
    )
    .unwrap()
    .with_inbound(
        inbound.then(|| InboundConfig::new("+12025550456", "discord", "dm:bot", "4831").unwrap()),
    )
    .unwrap();
    if live {
        config = config.with_live(Some(
            augmentagent_phone::live_config::LiveConfig::new("synthetic-key".into()).unwrap(),
        ));
    }
    let clock = Arc::new(TestClock(AtomicI64::new(1000)));
    let service = Arc::new(PhoneService::new(
        store.clone(),
        Arc::new(NoDial),
        clock.clone(),
        config,
    ));
    Fixture {
        _dir: dir,
        store,
        service,
        clock,
        owner,
        conversation,
        auth: Arc::new(WebhookAuth::new("AC-test", "test-token")),
    }
}
fn fields(sid: &str) -> Vec<(String, String)> {
    [
        ("AccountSid", "AC-test"),
        ("CallSid", sid),
        ("From", "+12025550123"),
        ("To", "+12025550456"),
        ("Direction", "inbound"),
    ]
    .into_iter()
    .map(|(k, v)| (k.into(), v.into()))
    .collect()
}
fn sign(path: &str, fields: &[(String, String)]) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut payload = format!("{ORIGIN}{path}");
    let mut fields = fields.to_vec();
    fields.sort();
    for (k, v) in fields {
        payload.push_str(&k);
        payload.push_str(&v);
    }
    let mut mac = Hmac::<Sha1>::new_from_slice(b"test-token").unwrap();
    mac.update(payload.as_bytes());
    STANDARD.encode(mac.finalize().into_bytes())
}
fn incoming(f: &Fixture, fields: &[(String, String)]) -> String {
    f.service
        .incoming_callback(&f.auth, fields, &sign("/phone/incoming", fields))
        .unwrap()
}
fn pin(f: &Fixture, id: &str, digits: &str) -> String {
    let mut data = fields(SID);
    data.push(("Digits".into(), digits.into()));
    f.service
        .incoming_pin_callback(
            &f.auth,
            id,
            &data,
            &sign(&format!("/phone/incoming/{id}"), &data),
        )
        .unwrap()
}

#[test]
fn incoming_requires_pin_and_reuses_chat_and_both_voice_backends() {
    for live in [false, true] {
        let f = fixture(true, true, live);
        let data = fields(SID);
        let xml = incoming(&f, &data);
        assert!(xml.contains("<Gather") && xml.contains("numDigits=\"4\""));
        assert!(!xml.contains("4831"));
        let call = f.store.phone_inbound_by_sid(SID).unwrap();
        assert_eq!(call.state, CallState::AwaitingAcceptance);
        assert!(f.service.authorize_call(&call.id).is_err());
        assert_eq!(incoming(&f, &data), xml); // delivery retry preserves reservation
        let origin = f.store.phone_origin(&call.id).unwrap();
        assert_eq!(origin.owner, f.owner);
        assert_eq!(origin.conversation, f.conversation);
        let xml = pin(&f, &call.id, "4831");
        assert!(xml.contains(if live {
            "<Stream "
        } else {
            "<ConversationRelay "
        }));
        f.service.authorize_call(&call.id).unwrap();
        assert_eq!(pin(&f, &call.id, "4831"), HANGUP); // one attempt per SID
        assert_eq!(incoming(&f, &data), HANGUP); // cannot restart connected call
    }
}

#[test]
fn incoming_uses_an_existing_authorized_control_channel_when_no_dm_is_bound() {
    let f = fixture(true, true, false);
    f.store
        .remove_surface_control_conversation(&f.conversation)
        .unwrap();
    f.store
        .set_surface_control_conversation(&f.conversation, ControlConversationKind::Channel, 1000)
        .unwrap();
    assert!(incoming(&f, &fields(SID)).contains("Gather"));
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    assert!(pin(&f, &call.id, "4831").contains("ConversationRelay"));
    assert_eq!(
        f.store.phone_origin(&call.id).unwrap().conversation,
        f.conversation
    );
}

#[test]
fn pin_attempt_and_original_chat_survive_database_reopen() {
    let f = fixture(true, true, false);
    incoming(&f, &fields(SID));
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    assert_eq!(pin(&f, &call.id, "000000"), HANGUP);
    let reopened = Store::open(f._dir.path().join("db")).unwrap();
    assert_eq!(reopened.phone_inbound_by_sid(SID).unwrap().id, call.id);
    assert_eq!(
        reopened.phone_origin(&call.id).unwrap().conversation,
        f.conversation
    );
    assert!(!reopened
        .phone_accept_inbound(&call.id, SID, true, 1001)
        .unwrap());
}

#[test]
fn concurrent_pin_callbacks_grant_only_one_connection() {
    let f = Arc::new(fixture(true, true, false));
    incoming(&f, &fields(SID));
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let jobs: Vec<_> = (0..2)
        .map(|_| {
            let f = f.clone();
            let id = call.id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                pin(&f, &id, "4831")
            })
        })
        .collect();
    let responses: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
    assert_eq!(
        responses
            .iter()
            .filter(|xml| xml.contains("ConversationRelay"))
            .count(),
        1
    );
    assert_eq!(
        responses
            .iter()
            .filter(|xml| xml.as_str() == HANGUP)
            .count(),
        1
    );
}

#[tokio::test]
async fn outbound_acceptance_endpoints_cannot_bypass_inbound_pin() {
    let f = fixture(true, true, false);
    let data = fields(SID);
    incoming(&f, &data);
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    assert!(f.store.phone_accept(&call.id, SID, 1000).is_err());
    let start = format!("/phone/start/{}", call.id);
    assert_eq!(
        f.service
            .start_callback(&f.auth, &call.id, &data, &sign(&start, &data))
            .unwrap(),
        HANGUP
    );
    let mut data = data;
    data.push(("Digits".into(), "1".into()));
    let accept = format!("/phone/accept/{}", call.id);
    assert_eq!(
        f.service
            .accept_callback(&f.auth, &call.id, &data, &sign(&accept, &data))
            .await
            .unwrap(),
        HANGUP
    );
    assert!(f.service.authorize_call(&call.id).is_err());
    assert!(pin(&f, &call.id, "4831").contains("ConversationRelay"));
}

#[test]
fn failed_expired_stopped_or_revoked_calls_never_connect() {
    for scenario in ["wrong", "empty", "expired", "stopped", "revoked", "removed"] {
        let f = fixture(true, true, false);
        incoming(&f, &fields(SID));
        let call = f.store.phone_inbound_by_sid(SID).unwrap();
        match scenario {
            "expired" => {
                f.clock.0.store(16000, Ordering::SeqCst);
            }
            "stopped" => {
                f.store.phone_request_stop("canonical", &call.id).unwrap();
            }
            "revoked" => {
                f.store.unbind_surface_owner(f.owner.account()).unwrap();
            }
            "removed" => {
                f.store.phone_remove_destination("canonical", 1000).unwrap();
            }
            _ => {}
        }
        assert_eq!(
            pin(
                &f,
                &call.id,
                match scenario {
                    "wrong" => "999999",
                    "empty" => "",
                    _ => "4831",
                }
            ),
            HANGUP,
            "{scenario}"
        );
        assert_eq!(pin(&f, &call.id, "4831"), HANGUP, "{scenario}");
        assert!(f.service.authorize_call(&call.id).is_err());
    }
}

#[test]
fn rejected_callers_and_bad_signatures_cannot_create_state() {
    for (key, value) in [
        ("From", "+12025550999"),
        ("From", "anonymous"),
        ("To", "+12025550999"),
        ("Direction", "outbound-api"),
        ("AccountSid", "AC-other"),
        ("CallSid", "CA-invalid"),
    ] {
        let f = fixture(true, true, false);
        let mut data = fields(SID);
        data.iter_mut().find(|(k, _)| k == key).unwrap().1 = value.into();
        let result = f
            .service
            .incoming_callback(&f.auth, &data, &sign("/phone/incoming", &data));
        assert!(result.is_err() || result.unwrap().contains("Reject"));
        assert!(f.store.phone_latest_call("canonical").unwrap().is_none());
    }
    for (enabled, inbound) in [(false, true), (true, false)] {
        let f = fixture(enabled, inbound, false);
        let data = fields(SID);
        assert!(f.service.incoming_callback(&f.auth, &data, "bad").is_err());
        let _ = f
            .service
            .incoming_callback(&f.auth, &data, &sign("/phone/incoming", &data));
        assert!(f.store.phone_latest_call("canonical").unwrap().is_none());
    }
    let f = fixture(true, true, false);
    f.store
        .phone_import_destination("ambiguous-owner", "+12025550123", 0)
        .unwrap();
    assert!(incoming(&f, &fields(SID)).contains("Reject"));
}

#[test]
fn incoming_shares_concurrency_rate_limits_and_daily_budget_with_outbound() {
    let f = fixture(true, true, false);
    incoming(&f, &fields(SID));
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    let request = || augmentagent_store::phone::ReserveCall {
        owner: "canonical",
        request_id: "outgoing",
        conversation: "chat",
        now_ms: 1000,
    };
    assert!(matches!(
        f.store.phone_reserve_call(request()),
        Err(augmentagent_store::phone::PhoneError::AlreadyActive)
    ));
    assert!(incoming(&f, &fields("CA00000000000000000000000000000002")).contains("busy"));
    f.store
        .phone_provider_event(&call.id, SID, CallState::Completed, 2000)
        .unwrap();
    for i in 2..=3 {
        let sid = format!("CA{i:032}");
        incoming(&f, &fields(&sid));
        let call = f.store.phone_inbound_by_sid(&sid).unwrap();
        f.store
            .phone_provider_event(&call.id, &sid, CallState::Completed, 2000)
            .unwrap();
    }
    assert!(incoming(&f, &fields("CA00000000000000000000000000000004")).contains("busy"));
    f.clock.0.store(602000, Ordering::SeqCst);
    let fourth = "CA00000000000000000000000000000004";
    assert!(incoming(&f, &fields(fourth)).contains("Gather"));
    let call = f.store.phone_inbound_by_sid(fourth).unwrap();
    f.store
        .phone_provider_event(&call.id, fourth, CallState::Completed, 603000)
        .unwrap();
    assert_eq!(
        f.store
            .phone_budget_remaining_ms("canonical", 603000)
            .unwrap(),
        0
    );
    f.clock.0.store(1203000, Ordering::SeqCst);
    assert!(incoming(&f, &fields("CA00000000000000000000000000000005")).contains("busy"));
}

#[test]
fn incoming_status_finishes_call_without_timestamp_and_never_resurrects_it() {
    let f = fixture(true, true, false);
    incoming(&f, &fields(SID));
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    pin(&f, &call.id, "4831");
    let mut data = fields(SID);
    data.extend([
        ("CallStatus".into(), "completed".into()),
        ("CallDuration".into(), "12".into()),
    ]);
    let path = "/phone/incoming/status";
    f.service
        .incoming_status_callback(&f.auth, &data, &sign(path, &data))
        .unwrap();
    assert_eq!(
        f.store.phone_provider_call(&call.id).unwrap().state,
        CallState::Completed
    );
    assert!(f.store.phone_usage(&call.id).unwrap().is_none()); // provider reconciliation owns exact end time
    assert_eq!(incoming(&f, &fields(SID)), HANGUP);
    assert_eq!(pin(&f, &call.id, "4831"), HANGUP);
}

#[tokio::test]
async fn real_http_ingress_enforces_signatures_forms_and_pin_callback() {
    let f = fixture(true, true, false);
    let router =
        augmentagent_phone::ingress::router(f.service.clone(), f.auth.clone(), Arc::new(Agent));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let path = "/phone/incoming";
    let url = format!("http://{addr}{path}");
    let data = fields(SID);
    assert_eq!(
        client.post(&url).form(&data).send().await.unwrap().status(),
        403
    );
    assert_eq!(
        client.post(&url).json(&data).send().await.unwrap().status(),
        415
    );
    assert_eq!(
        client
            .post(format!("{url}?x=1"))
            .form(&data)
            .send()
            .await
            .unwrap()
            .status(),
        400
    );
    assert!(f.store.phone_latest_call("canonical").unwrap().is_none());
    let response = client
        .post(&url)
        .header("x-twilio-signature", sign(path, &data))
        .form(&data)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("Gather"));
    let call = f.store.phone_inbound_by_sid(SID).unwrap();
    let path = format!("/phone/incoming/{}", call.id);
    let mut data = data;
    data.push(("Digits".into(), "4831".into()));
    let response = client
        .post(format!("http://{addr}{path}"))
        .header("x-twilio-signature", sign(&path, &data))
        .form(&data)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("ConversationRelay"));
    let path = "/phone/incoming/status";
    let mut data = fields(SID);
    data.extend([
        ("CallStatus".into(), "completed".into()),
        ("CallDuration".into(), "12".into()),
    ]);
    let response = client
        .post(format!("http://{addr}{path}"))
        .header("x-twilio-signature", sign(path, &data))
        .form(&data)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(
        f.store.phone_provider_call(&call.id).unwrap().state,
        CallState::Completed
    );
    server.abort();
}
