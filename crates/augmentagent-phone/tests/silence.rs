//! #1556 silence and idle policy. Every timing rule runs on a paused clock;
//! the frame-level scenarios use a real WebSocket against the phone router.
use async_trait::async_trait;
use augmentagent_phone::{
    config::Settings,
    ingress::{AgentError, CallLine, ConversationAgent, SpeechSender, TaskSpeech},
    silence::{
        SilenceAction, SilenceConfigError, SilenceMonitor, SilencePolicy, CHECK_IN_1_LINE,
        CHECK_IN_1_SECS, CHECK_IN_2_LINE, CHECK_IN_2_SECS, END_SECS, GOODBYE_LINE,
    },
    Clock, DialRequest, Error, OwnerLink, PhoneConfig, PhoneService, ProviderError, Telephony,
};
use augmentagent_store::{
    owner::ControlConversationKind,
    phone::{CallState, PhoneCall},
    Store, SurfaceAccountRef, SurfaceConversationRef, SurfaceOwnerRef, SurfacePlatform,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_util::sync::CancellationToken;

const MS: fn(u64) -> Duration = Duration::from_millis;
const SECS: fn(u64) -> Duration = Duration::from_secs;

// ---------- step 4 / AC4: configuration ----------

fn values() -> HashMap<&'static str, String> {
    [
        ("AUGMENTAGENT_PHONE_ENABLED", "1"),
        ("AUGMENTAGENT_PHONE_PUBLIC_URL", "https://phone.example.test"),
        ("AUGMENTAGENT_PHONE_VERIFICATION_KEY", "00000000000000000000000000000000"),
        ("TWILIO_ACCOUNT_SID", "AC00000000000000000000000000000000"),
        ("TWILIO_AUTH_TOKEN", "test-token"),
        ("TWILIO_PHONE_NUMBER", "+12025550100"),
        ("AUGMENTAGENT_PHONE_OWNER_LINKS", r#"[{"platform":"slack","account":"T-fixture","surface_owner":"U-fixture","owner":"canonical-owner"}]"#),
    ]
    .into_iter()
    .map(|(k, v)| (k, v.to_owned()))
    .collect()
}
fn settings_error(pairs: &[(&'static str, &str)]) -> Option<Error> {
    let mut env = values();
    for (key, value) in pairs {
        env.insert(key, (*value).into());
    }
    Settings::from_lookup(|k| env.get(k).cloned()).err()
}

#[test]
fn step4_defaults_are_12_25_45_seconds() {
    let policy = SilencePolicy::default();
    assert_eq!(policy.check_in_1(), SECS(12));
    assert_eq!(policy.check_in_2(), SECS(25));
    assert_eq!(policy.end(), SECS(45));
    assert_eq!(SilencePolicy::new(12, 25, 45), Ok(policy));
    let env = values();
    let settings = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap();
    assert_eq!(settings.silence(), policy);
    let custom = [
        (CHECK_IN_1_SECS, "10"),
        (CHECK_IN_2_SECS, "20"),
        (END_SECS, "30"),
    ];
    let mut env = values();
    for (k, v) in custom {
        env.insert(k, v.into());
    }
    let settings = Settings::from_lookup(|k| env.get(k).cloned())
        .unwrap()
        .unwrap();
    assert_eq!(settings.silence(), SilencePolicy::new(10, 20, 30).unwrap());
}

#[test]
fn step4_out_of_range_and_non_increasing_thresholds_fail_with_a_typed_error() {
    use SilenceConfigError::*;
    for (args, expected) in [
        ((4, 25, 45), OutOfRange(CHECK_IN_1_SECS)),
        ((12, 25, 301), OutOfRange(END_SECS)),
        ((12, 0, 45), OutOfRange(CHECK_IN_2_SECS)),
        ((12, 12, 45), NotIncreasing),
        ((25, 12, 45), NotIncreasing),
        ((12, 25, 25), NotIncreasing),
        ((12, 46, 45), NotIncreasing),
    ] {
        assert_eq!(
            SilencePolicy::new(args.0, args.1, args.2),
            Err(expected),
            "{args:?}"
        );
    }
    // Inclusive bounds.
    assert!(SilencePolicy::new(5, 6, 300).is_ok());
    for (pairs, expected) in [
        (vec![(CHECK_IN_1_SECS, "4")], OutOfRange(CHECK_IN_1_SECS)),
        (vec![(END_SECS, "301")], OutOfRange(END_SECS)),
        (vec![(CHECK_IN_2_SECS, "12")], NotIncreasing),
        (vec![(CHECK_IN_1_SECS, "30")], NotIncreasing),
        (vec![(END_SECS, "forty")], Invalid(END_SECS)),
        (vec![(CHECK_IN_2_SECS, "")], Invalid(CHECK_IN_2_SECS)),
        (vec![(CHECK_IN_1_SECS, "-12")], Invalid(CHECK_IN_1_SECS)),
    ] {
        match settings_error(&pairs) {
            Some(Error::Silence(error)) => assert_eq!(error, expected, "{pairs:?}"),
            other => panic!("{pairs:?}: expected a typed silence error, got {other:?}"),
        }
    }
    // Errors carry only the variable name, never the configured value.
    let message = format!("{}", Error::Silence(Invalid(END_SECS)));
    assert!(!message.contains("forty"));
}

// ---------- monitor rules on a paused clock ----------

#[tokio::test(start_paused = true)]
async fn monitor_check_ins_never_reset_the_clock_and_any_activity_does() {
    let start = Instant::now();
    let mut monitor = SilenceMonitor::new(SilencePolicy::default(), start);
    let mut fired = Vec::new();
    while fired.len() < 3 && Instant::now() - start < SECS(60) {
        tokio::time::advance(MS(100)).await;
        if let Some(action) = monitor.poll(Instant::now()) {
            // A tokens-played event for a check-in is not owner activity.
            monitor.played(Instant::now());
            fired.push((Instant::now() - start, action));
        }
    }
    assert_eq!(
        fired,
        [
            (SECS(12), SilenceAction::CheckIn(CHECK_IN_1_LINE)),
            (SECS(25), SilenceAction::CheckIn(CHECK_IN_2_LINE)),
            (SECS(45), SilenceAction::Goodbye(GOODBYE_LINE)),
        ]
    );
    assert_eq!(monitor.poll(start + SECS(600)), None, "goodbye is terminal");

    let mut monitor = SilenceMonitor::new(SilencePolicy::default(), start);
    assert!(monitor.poll(start + SECS(12)).is_some());
    monitor.activity(start + SECS(13));
    assert_eq!(monitor.poll(start + SECS(24)), None);
    assert_eq!(
        monitor.poll(start + SECS(25)),
        Some(SilenceAction::CheckIn(CHECK_IN_1_LINE)),
        "activity restarts from the first check-in"
    );
}

#[tokio::test(start_paused = true)]
async fn monitor_progress_holds_the_clock_for_15_seconds_until_the_result() {
    let start = Instant::now();
    let mut monitor = SilenceMonitor::new(SilencePolicy::default(), start);
    monitor.task_spoken(&TaskSpeech::Progress("t1".into()), start + SECS(10));
    for at in (10..30).map(SECS) {
        assert_eq!(monitor.poll(start + at), None, "{at:?}");
    }
    monitor.task_spoken(&TaskSpeech::Result("t1".into()), start + SECS(30));
    assert_eq!(monitor.poll(start + MS(41_900)), None);
    assert_eq!(
        monitor.poll(start + SECS(42)),
        Some(SilenceAction::CheckIn(CHECK_IN_1_LINE))
    );
    // Without a result, the hold lapses 15 s after the last progress line and
    // the clock restarts there.
    let mut monitor = SilenceMonitor::new(SilencePolicy::default(), start);
    monitor.task_spoken(&TaskSpeech::Progress("t2".into()), start + SECS(10));
    assert_eq!(monitor.poll(start + SECS(36)), None);
    assert_eq!(
        monitor.poll(start + SECS(37)),
        Some(SilenceAction::CheckIn(CHECK_IN_1_LINE))
    );
}

// ---------- real WebSocket fixture ----------

struct FakeClock(AtomicI64);
impl Clock for FakeClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}
struct FakeProvider;
#[async_trait]
impl Telephony for FakeProvider {
    async fn create(&self, _: DialRequest) -> Result<String, ProviderError> {
        Ok("CA-test".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// Answers every prompt at once. Optionally runs a scripted dispatched task
/// on the call line after the first answer. Records chat notices by key, as
/// the durable outbox would.
#[derive(Default)]
struct Owner {
    notices: Mutex<Vec<(String, String)>>,
    interrupted: Mutex<Vec<String>>,
    task: Option<Vec<(Duration, TaskSpeech, &'static str)>>,
    task_started: AtomicBool,
    fail_notice: AtomicBool,
}
#[async_trait]
impl ConversationAgent for Owner {
    async fn answer(
        &self,
        _: &PhoneCall,
        _: &str,
        _: &str,
        speech: SpeechSender,
        _: CancellationToken,
    ) -> Result<String, AgentError> {
        speech.speak_partial("Sure.").await?;
        speech.finish().await?;
        if let Some(script) = self.task.clone() {
            if !self.task_started.swap(true, Ordering::SeqCst) {
                let line: CallLine = speech.line();
                let started = Instant::now();
                tokio::spawn(async move {
                    for (at, kind, text) in script {
                        tokio::time::sleep_until(started + at).await;
                        assert!(line.announce_task(text, kind));
                    }
                });
            }
        }
        Ok("Sure.".into())
    }
    async fn interrupted(&self, _: &PhoneCall, heard: &str) -> Result<(), AgentError> {
        self.interrupted.lock().unwrap().push(heard.into());
        Ok(())
    }
    async fn call_ended(&self, _: &PhoneCall, key: &str, text: &str) -> Result<(), AgentError> {
        if self.fail_notice.load(Ordering::SeqCst) {
            return Err(AgentError);
        }
        let mut notices = self.notices.lock().unwrap();
        // The outbox rejects a replayed idempotency key.
        if !notices.iter().any(|(k, _)| k == key) {
            notices.push((key.into(), text.into()));
        }
        Ok(())
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
struct Live {
    socket: Socket,
    server: tokio::task::JoinHandle<()>,
    service: Arc<PhoneService>,
    store: Arc<Store>,
    clock: Arc<FakeClock>,
    call_id: String,
    _dir: tempfile::TempDir,
}

/// Twilio request signature: URL followed by the sorted form fields.
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
impl Live {
    /// A tokens-played event for the latest speech.
    async fn played(&mut self) {
        send(
            &mut self.socket,
            json!({"type":"info","name":"tokensPlayed","value":"x","last":true}),
        )
        .await;
    }
    /// The signed relay-ended (`<Connect action>`) callback after `end`.
    fn relay_ended(&self, handoff: &str) -> String {
        let fields = vec![
            ("AccountSid".to_string(), "AC-test".to_string()),
            ("CallSid".into(), "CA-test".into()),
            ("SessionStatus".into(), "completed".into()),
            ("HandoffData".into(), handoff.into()),
        ];
        let url = format!(
            "https://phone.example.test/phone/relay-ended/{}",
            self.call_id
        );
        let auth = augmentagent_phone::webhook::WebhookAuth::new("AC-test", "callback-secret");
        self.service
            .relay_ended_callback(
                &auth,
                &self.call_id,
                &fields,
                &signed("callback-secret", &url, &fields),
            )
            .unwrap()
    }
}

fn service(store: Arc<Store>, clock: Arc<FakeClock>, owner: &SurfaceOwnerRef) -> PhoneService {
    let config = PhoneConfig::new(
        true,
        "https://phone.example.test",
        "test-secret-for-verification-32bytes",
        vec![OwnerLink {
            surface_owner: owner.clone(),
            owner_id: "canonical-owner".into(),
        }],
    )
    .unwrap()
    // Tokens-played events (#1551) confirm the goodbye before `end`.
    .with_debug_events(true);
    PhoneService::new(store, Arc::new(FakeProvider), clock, config)
}

/// Paused-clock driver for real sockets. Left alone, a paused runtime jumps
/// to the next timer whenever it looks idle, including while a loopback
/// frame is still in flight, which skews frame times. This task keeps the
/// runtime busy (so I/O is polled without time passing) and moves the clock
/// forward in 1 ms steps.
fn drive_clock() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        loop {
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(MS(1)).await;
        }
    })
}

async fn live(agent: Arc<Owner>) -> Live {
    drive_clock();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("test.db")).unwrap());
    let account =
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "account").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let conversation = SurfaceConversationRef::new(account, "chat", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&conversation, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("canonical-owner", "+12025550123", 0)
        .unwrap();
    let clock = Arc::new(FakeClock(AtomicI64::new(100)));
    let service = Arc::new(service(store.clone(), clock.clone(), &owner));
    let grant = service.authorize(&owner, &conversation).unwrap();
    let call = service
        .tool(&grant, "call_owner", json!({"request_id":"r1"}))
        .await
        .unwrap();
    let call_id = call["id"].as_str().unwrap().to_string();
    store
        .phone_provider_event(&call_id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    store.phone_accept(&call_id, "CA-test", 101).unwrap();
    let router = augmentagent_phone::ingress::router(
        service.clone(),
        Arc::new(augmentagent_phone::webhook::WebhookAuth::new(
            "AC-test",
            "callback-secret",
        )),
        agent,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut request = format!("ws://{addr}/phone/relay/{call_id}")
        .into_client_request()
        .unwrap();
    let signature = signed(
        "callback-secret",
        &format!("wss://phone.example.test/phone/relay/{call_id}"),
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
    Live {
        socket,
        server,
        service,
        store,
        clock,
        call_id,
        _dir: dir,
    }
}

async fn send(socket: &mut Socket, frame: Value) {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .unwrap();
}
async fn next_frame(socket: &mut Socket, within: Duration) -> Option<Value> {
    let frame = tokio::time::timeout(within, socket.next())
        .await
        .ok()??
        .ok()?;
    serde_json::from_str(frame.to_text().ok()?).ok()
}
/// Ask a question and wait for the answer's closing frame; returns its time.
async fn ask(socket: &mut Socket, text: &str) -> Instant {
    send(
        socket,
        json!({"type":"prompt","voicePrompt":text,"last":true}),
    )
    .await;
    loop {
        let frame = next_frame(socket, SECS(5)).await.expect("answer frame");
        if frame["type"] == "text" && frame["last"] == true {
            return Instant::now();
        }
    }
}
/// Every frame until `until` after `since`, with its offset from `since`.
async fn frames_until(
    socket: &mut Socket,
    since: Instant,
    until: Duration,
) -> Vec<(Duration, Value)> {
    let mut frames = Vec::new();
    loop {
        let left = (since + until).saturating_duration_since(Instant::now());
        if left.is_zero() {
            return frames;
        }
        match tokio::time::timeout(left, socket.next()).await {
            Err(_) | Ok(None) => return frames,
            Ok(Some(Ok(Message::Text(text)))) => {
                frames.push((Instant::now() - since, serde_json::from_str(&text).unwrap()))
            }
            Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => return frames,
            Ok(Some(Ok(_))) => {}
        }
    }
}
fn near(actual: Duration, expected_ms: u64) -> bool {
    let actual = actual.as_millis() as i64;
    (actual - expected_ms as i64).abs() <= 100
}
fn token(frame: &Value) -> &str {
    frame["token"].as_str().unwrap_or_default()
}
async fn wait_for(mut done: impl FnMut() -> bool) {
    for _ in 0..200 {
        if done() {
            return;
        }
        tokio::time::sleep(MS(50)).await;
    }
    panic!("condition not reached");
}

// ---------- step 1 / AC1 + AC5 ----------

#[tokio::test(start_paused = true)]
async fn step1_silence_checks_in_twice_then_says_goodbye_and_ends() {
    let agent = Arc::new(Owner::default());
    let mut live = live(agent.clone()).await;
    let answered = ask(&mut live.socket, "what's next").await;
    let mut frames = frames_until(&mut live.socket, answered, MS(45_020)).await;
    // The provider reports the goodbye played; `end` follows at once (#1557).
    live.played().await;
    frames.extend(frames_until(&mut live.socket, answered, SECS(50)).await);
    let summary: Vec<_> = frames
        .iter()
        .map(|(at, f)| (at.as_millis(), f.clone()))
        .collect();
    assert_eq!(frames.len(), 4, "{summary:?}");
    let (at, first) = &frames[0];
    assert!(near(*at, 12_000), "{summary:?}");
    assert_eq!(
        (first["type"].as_str(), token(first)),
        (Some("text"), CHECK_IN_1_LINE)
    );
    assert_eq!(
        (
            &first["last"],
            &first["interruptible"],
            &first["preemptible"]
        ),
        (&json!(true), &json!(true), &json!(true))
    );
    let (at, second) = &frames[1];
    assert!(near(*at, 25_000), "{summary:?}");
    assert_eq!(token(second), CHECK_IN_2_LINE);
    assert_eq!(
        (&second["interruptible"], &second["preemptible"]),
        (&json!(true), &json!(true))
    );
    let (at, goodbye) = &frames[2];
    assert!(near(*at, 45_000), "{summary:?}");
    assert_eq!(
        (goodbye["type"].as_str(), token(goodbye)),
        (Some("text"), GOODBYE_LINE)
    );
    let (at, end) = &frames[3];
    assert!(near(*at, 45_000), "{summary:?}");
    assert_eq!(end["type"], "end");
    let data = end["handoffData"].as_str().unwrap();
    let handoff: Value = serde_json::from_str(data).unwrap();
    assert_eq!(handoff, json!({"reason":"silence","call_id":live.call_id}));
    assert_eq!(
        augmentagent_phone::end_call::end_reason_from_handoff(data, &live.call_id),
        "silence"
    );

    // AC5: persisted reason and exactly one chat notice.
    wait_for(|| !agent.notices.lock().unwrap().is_empty()).await;
    assert_eq!(
        live.store
            .phone_end_reason(&live.call_id)
            .unwrap()
            .as_deref(),
        Some("silence")
    );
    let key = format!("{}:silence_end", live.call_id);
    let notices = agent.notices.lock().unwrap().clone();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, key);
    assert!(notices[0].1.contains("silence"), "{}", notices[0].1);
    assert!(
        live.store
            .phone_provider_call(&live.call_id)
            .unwrap()
            .stop_requested
    );
    // The background pass and a simulated restart find nothing left to send.
    assert_eq!(
        live.service.run_end_notices(agent.as_ref()).await.unwrap(),
        0
    );
    let owner = SurfaceOwnerRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "account").unwrap(),
        "owner",
    )
    .unwrap();
    let restarted = service(live.store.clone(), live.clock.clone(), &owner);
    live.clock.0.fetch_add(3_600_000, Ordering::SeqCst);
    assert_eq!(restarted.run_end_notices(agent.as_ref()).await.unwrap(), 0);
    assert_eq!(agent.notices.lock().unwrap().len(), 1);
    // Twilio's relay-ended callback carries the handoff; the reason stays.
    assert!(live.relay_ended(data).contains("<Hangup/>"));
    assert_eq!(
        live.store
            .phone_end_reason(&live.call_id)
            .unwrap()
            .as_deref(),
        Some("silence")
    );
    live.server.abort();
}

// Without a playback report the goodbye still gets the bounded wait before
// `end`, so it is not cut off.
#[tokio::test(start_paused = true)]
async fn step1_goodbye_without_playback_report_ends_after_the_bounded_wait() {
    let mut live = live(Arc::new(Owner::default())).await;
    let answered = ask(&mut live.socket, "what's next").await;
    let frames = frames_until(&mut live.socket, answered, SECS(55)).await;
    let summary: Vec<_> = frames
        .iter()
        .map(|(at, f)| (at.as_millis(), f["type"].clone(), token(f).to_owned()))
        .collect();
    assert_eq!(frames.len(), 4, "{summary:?}");
    assert!(
        near(frames[2].0, 45_000) && token(&frames[2].1) == GOODBYE_LINE,
        "{summary:?}"
    );
    let wait = augmentagent_phone::end_call::PLAYBACK_WAIT.as_millis() as u64;
    assert!(
        near(frames[3].0, 45_000 + wait) && frames[3].1["type"] == "end",
        "{summary:?}"
    );
    live.server.abort();
}

// ---------- step 2 / AC2: any prompt resets the clock ----------

#[tokio::test(start_paused = true)]
async fn step2_partial_prompt_at_11s_moves_the_check_in_to_23s() {
    let mut live = live(Arc::new(Owner::default())).await;
    let answered = ask(&mut live.socket, "hello").await;
    assert!(frames_until(&mut live.socket, answered, SECS(11))
        .await
        .is_empty());
    send(
        &mut live.socket,
        json!({"type":"prompt","voicePrompt":"um","last":false}),
    )
    .await;
    let frames = frames_until(&mut live.socket, answered, SECS(24)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(near(frames[0].0, 23_000), "{frames:?}");
    assert_eq!(token(&frames[0].1), CHECK_IN_1_LINE);
    live.server.abort();
}

#[tokio::test(start_paused = true)]
async fn step2_final_prompt_after_a_check_in_restarts_from_the_first_check_in() {
    let mut live = live(Arc::new(Owner::default())).await;
    let answered = ask(&mut live.socket, "hello").await;
    let frames = frames_until(&mut live.socket, answered, SECS(20)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(token(&frames[0].1), CHECK_IN_1_LINE);
    let answered = ask(&mut live.socket, "yes, still here").await;
    let frames = frames_until(&mut live.socket, answered, SECS(13)).await;
    assert_eq!(frames.len(), 1, "no second check-in at 25 s: {frames:?}");
    assert!(near(frames[0].0, 12_000), "{frames:?}");
    assert_eq!(token(&frames[0].1), CHECK_IN_1_LINE);
    live.server.abort();
}

// ---------- step 3 / AC3: dispatched work suspends silence handling ----------

#[tokio::test(start_paused = true)]
async fn step3_progress_suspends_check_ins_until_the_result_is_spoken() {
    let agent = Arc::new(Owner {
        task: Some(vec![
            (
                SECS(10),
                TaskSpeech::Progress("t1".into()),
                "Still drafting.",
            ),
            (
                SECS(30),
                TaskSpeech::Result("t1".into()),
                "Update on your task: done.",
            ),
        ]),
        ..Default::default()
    });
    let mut live = live(agent).await;
    let answered = ask(&mut live.socket, "draft the reply").await;
    let frames = frames_until(&mut live.socket, answered, SECS(43)).await;
    let summary: Vec<_> = frames
        .iter()
        .map(|(at, f)| (at.as_millis(), token(f).to_owned()))
        .collect();
    assert_eq!(frames.len(), 3, "{summary:?}");
    assert!(
        near(frames[0].0, 10_000) && token(&frames[0].1) == "Still drafting.",
        "{summary:?}"
    );
    assert!(near(frames[1].0, 30_000), "{summary:?}");
    assert!(near(frames[2].0, 42_000), "{summary:?}");
    assert_eq!(token(&frames[2].1), CHECK_IN_1_LINE);
    let check_ins = frames
        .iter()
        .filter(|(at, f)| *at < SECS(30) && [CHECK_IN_1_LINE, CHECK_IN_2_LINE].contains(&token(f)))
        .count();
    assert_eq!(check_ins, 0);
    live.server.abort();
}

// ---------- step 5: owner speaks over a check-in ----------

#[tokio::test(start_paused = true)]
async fn step5_interrupting_a_check_in_is_handled_and_resets_the_clock() {
    let agent = Arc::new(Owner::default());
    let mut live = live(agent.clone()).await;
    let answered = ask(&mut live.socket, "hello").await;
    let frames = frames_until(&mut live.socket, answered, MS(12_500)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(token(&frames[0].1), CHECK_IN_1_LINE);
    send(
        &mut live.socket,
        json!({"type":"interrupt","utteranceUntilInterrupt":"Still","durationUntilInterruptMs":300}),
    )
    .await;
    // Nothing is spoken over the owner; the clock restarted at the interrupt.
    assert!(frames_until(&mut live.socket, answered, SECS(14))
        .await
        .is_empty());
    let answered = ask(&mut live.socket, "yes I'm here").await;
    wait_for(|| !agent.interrupted.lock().unwrap().is_empty()).await;
    let frames = frames_until(&mut live.socket, answered, SECS(13)).await;
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(near(frames[0].0, 12_000), "{frames:?}");
    assert_eq!(token(&frames[0].1), CHECK_IN_1_LINE);
    live.server.abort();
}

// ---------- AC5: durable, idempotent end notice ----------

#[tokio::test(start_paused = true)]
async fn ac5_end_notice_survives_a_crash_and_is_never_duplicated() {
    let agent = Arc::new(Owner::default());
    let live = live(agent.clone()).await;
    live.server.abort();
    let id = live.call_id.clone();
    assert_eq!(live.store.phone_end_reason(&id).unwrap(), None);
    // Crash between persisting the reason and delivering the notice.
    assert!(live
        .store
        .phone_record_end_with_notice(&id, "silence", 200)
        .unwrap());
    assert!(!live
        .store
        .phone_record_end_with_notice(&id, "silence", 300)
        .unwrap());
    agent.fail_notice.store(true, Ordering::SeqCst);
    assert_eq!(
        live.service.run_end_notices(agent.as_ref()).await.unwrap(),
        0
    );
    agent.fail_notice.store(false, Ordering::SeqCst);
    // The failed attempt holds a lease; it is retried once the lease expires.
    assert_eq!(
        live.service.run_end_notices(agent.as_ref()).await.unwrap(),
        0
    );
    live.clock.0.fetch_add(600_000, Ordering::SeqCst);
    assert_eq!(
        live.service.run_end_notices(agent.as_ref()).await.unwrap(),
        1
    );
    assert_eq!(
        live.service.run_end_notices(agent.as_ref()).await.unwrap(),
        0
    );
    let owner = SurfaceOwnerRef::new(
        SurfaceAccountRef::new(SurfacePlatform::new("discord").unwrap(), "account").unwrap(),
        "owner",
    )
    .unwrap();
    let restarted = service(live.store.clone(), live.clock.clone(), &owner);
    live.clock.0.fetch_add(3_600_000, Ordering::SeqCst);
    assert_eq!(restarted.run_end_notices(agent.as_ref()).await.unwrap(), 0);
    let notices = agent.notices.lock().unwrap().clone();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, format!("{id}:silence_end"));
    assert_eq!(
        live.store.phone_end_reason(&id).unwrap().as_deref(),
        Some("silence")
    );
    // Reasons are fixed category codes, never free text.
    assert!(live
        .store
        .phone_record_end_with_notice(&id, "Owner said bye!", 400)
        .is_err());
}
