//! Offline tests for the fast conversational layer (#1546). Every model is a
//! scripted fake; nothing here makes a network call or needs credentials.
use async_trait::async_trait;
use augmentagent_phone::{
    fast::{
        FastBackend, FastContext, FastEvent, FastLayerConfig, FastMessage, FastModel,
        FastModelError, FastRequest, FastStream, TaskEvents, ToolCall, VoiceConversationAgent,
    },
    ingress::{AgentError, ConversationAgent, SpeechEvent, SpeechSender},
    relay::{RelayInput, RelaySession},
};
use augmentagent_store::phone::{CallState, PhoneCall};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;

// ---------- scripted fast model ----------

#[derive(Clone)]
enum Step {
    Token(&'static str),
    Tool(&'static str, Value),
    Wait(Arc<Notify>),
    Sleep(Duration),
    Fail,
}
/// One scripted round per model request; requests are recorded.
struct ScriptedModel {
    rounds: Mutex<std::collections::VecDeque<Vec<Step>>>,
    requests: Mutex<Vec<RecordedRequest>>,
}
struct RecordedRequest {
    system: String,
    messages: Vec<FastMessage>,
    tools: Vec<String>,
}
impl ScriptedModel {
    fn new(rounds: Vec<Vec<Step>>) -> Arc<Self> {
        Arc::new(Self {
            rounds: Mutex::new(rounds.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> std::sync::MutexGuard<'_, Vec<RecordedRequest>> {
        self.requests.lock().unwrap()
    }
}
#[async_trait]
impl FastModel for ScriptedModel {
    async fn stream(&self, request: FastRequest) -> Result<FastStream, FastModelError> {
        self.requests.lock().unwrap().push(RecordedRequest {
            system: request.system,
            messages: request.messages,
            tools: request.tools.iter().map(|t| t.name.to_string()).collect(),
        });
        let round = self
            .rounds
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![Step::Token("Okay.")]);
        let (tx, rx) = mpsc::channel(8);
        tokio::spawn(async move {
            for (n, step) in round.into_iter().enumerate() {
                let event = match step {
                    Step::Token(t) => Ok(FastEvent::Token(t.into())),
                    Step::Tool(name, input) => Ok(FastEvent::ToolCall(ToolCall {
                        id: format!("tool-{n}"),
                        name: name.into(),
                        input,
                    })),
                    Step::Wait(gate) => {
                        gate.notified().await;
                        continue;
                    }
                    Step::Sleep(d) => {
                        tokio::time::sleep(d).await;
                        continue;
                    }
                    Step::Fail => Err(FastModelError("scripted_failure")),
                };
                if tx.send(event).await.is_err() {
                    return;
                }
            }
        });
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })))
    }
}

// ---------- fake originating-conversation backend ----------

type DispatchHook = Box<dyn Fn(TaskEvents) + Send + Sync>;
#[derive(Default)]
struct FakeBackend {
    revoked: AtomicBool,
    authorizations: AtomicUsize,
    mirrors: Mutex<Vec<(String, String)>>,
    dispatches: Mutex<Vec<(String, String)>>,
    lookups: Mutex<Vec<String>>,
    lookup_delay: Mutex<Duration>,
    on_dispatch: Mutex<Option<DispatchHook>>,
    chat_summary: String,
    agenda: Option<String>,
}
#[async_trait]
impl FastBackend for FakeBackend {
    fn authorize_call(&self, _: &PhoneCall) -> Result<(), AgentError> {
        self.authorizations.fetch_add(1, Ordering::SeqCst);
        if self.revoked.load(Ordering::SeqCst) {
            Err(AgentError)
        } else {
            Ok(())
        }
    }
    async fn context(&self, _: &PhoneCall) -> Result<FastContext, AgentError> {
        Ok(FastContext {
            owner_profile: None,
            chat_summary: self.chat_summary.clone(),
            agenda: self.agenda.clone(),
        })
    }
    async fn mirror(&self, _: &PhoneCall, key: &str, text: &str) -> Result<(), AgentError> {
        self.mirrors.lock().unwrap().push((key.into(), text.into()));
        Ok(())
    }
    async fn dispatch(
        &self,
        _: &PhoneCall,
        description: &str,
        events: TaskEvents,
    ) -> Result<(), AgentError> {
        self.dispatches
            .lock()
            .unwrap()
            .push((events.task_id().into(), description.into()));
        if let Some(hook) = self.on_dispatch.lock().unwrap().as_ref() {
            hook(events);
        }
        Ok(())
    }
    async fn lookup(&self, _: &PhoneCall, query: &str) -> Result<String, AgentError> {
        self.lookups.lock().unwrap().push(query.into());
        let delay = *self.lookup_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        Ok("Dentist at 9 a.m. tomorrow.".into())
    }
}

fn call() -> PhoneCall {
    PhoneCall {
        id: "call-1".into(),
        owner: "owner".into(),
        request_id: "r".into(),
        conversation: "chat".into(),
        provider_sid: Some("CA-test".into()),
        state: CallState::Connected,
        created_ms: 0,
        answered_ms: Some(0),
        ended_ms: None,
        max_duration_seconds: 600,
        stop_requested: false,
        verification: false,
    }
}

/// A relay session with an accepted setup; prompts become turns.
struct Relay {
    session: RelaySession,
    tx: mpsc::Sender<SpeechEvent>,
    rx: mpsc::Receiver<SpeechEvent>,
}
impl Relay {
    fn new() -> Self {
        let mut session = RelaySession::new("call-1", "AC-test", "CA-test");
        session
            .receive(
                r#"{"type":"setup","accountSid":"AC-test","callSid":"CA-test","sessionId":"VX"}"#,
            )
            .unwrap();
        let (tx, rx) = mpsc::channel(64);
        Self { session, tx, rx }
    }
    fn turn(&mut self, text: &str) -> (String, SpeechSender) {
        let frame = json!({"type":"prompt","voicePrompt":text,"last":true}).to_string();
        let RelayInput::Turn(turn) = self.session.receive(&frame).unwrap() else {
            panic!("prompt must open a turn")
        };
        let id = turn.id.clone();
        (id, SpeechSender::new(self.tx.clone(), Arc::new(turn)))
    }
    fn drain(&mut self) -> Vec<SpeechEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
        out
    }
}
fn tokens(events: &[SpeechEvent]) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            SpeechEvent::Token { text, last, .. } => Some((text.clone(), *last)),
            _ => None,
        })
        .collect()
}

fn agent(
    model: Arc<ScriptedModel>,
    backend: Arc<FakeBackend>,
    config: FastLayerConfig,
) -> Arc<VoiceConversationAgent> {
    Arc::new(VoiceConversationAgent::new(model, backend, config))
}

// Step 1 / AC1: speech starts with the first sentence, before the model
// finishes, and within 1,000 ms of the final prompt on a paused clock.
#[tokio::test(start_paused = true)]
async fn step1_first_sentence_is_spoken_before_the_model_completes() {
    let gate = Arc::new(Notify::new());
    let model = ScriptedModel::new(vec![vec![
        Step::Sleep(Duration::from_millis(300)),
        Step::Token("Morning! "),
        Step::Wait(gate.clone()),
        Step::Token("Your day is clear. "),
        Step::Token("Anything else?"),
    ]]);
    let backend = Arc::new(FakeBackend::default());
    let fast = agent(model, backend.clone(), FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("good morning");
    let prompt_at = tokio::time::Instant::now();
    let task = {
        let fast = fast.clone();
        let speech = speech.clone();
        tokio::spawn(async move {
            fast.answer(
                &call(),
                &turn_id,
                "good morning",
                speech,
                CancellationToken::new(),
            )
            .await
        })
    };
    let first = tokio::time::timeout(Duration::from_secs(5), relay.rx.recv())
        .await
        .expect("first sentence must be spoken while the model is still streaming")
        .unwrap();
    assert!(
        prompt_at.elapsed() < Duration::from_millis(1_000),
        "first text after {:?}",
        prompt_at.elapsed()
    );
    match first {
        SpeechEvent::Token { text, last, .. } => {
            assert_eq!(text, "Morning! ");
            assert!(!last);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(!task.is_finished(), "model has not completed yet");
    gate.notify_one();
    let answer = task.await.unwrap().unwrap();
    assert_eq!(answer, "Morning! Your day is clear. Anything else?");
    let rest = tokens(&relay.drain());
    assert!(rest.last().unwrap().1, "talk cycle closes with last:true");
    let spoken: String = rest.iter().map(|(t, _)| t.as_str()).collect();
    assert_eq!(spoken, "Your day is clear. Anything else?");
}

// Step 3 / AC5: authority is re-checked before each tool; a revoked call
// gets a typed refusal and no dispatch.
#[tokio::test]
async fn step3_authorize_call_runs_before_every_tool_and_blocks_revoked_calls() {
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"draft the reply"}),
        )],
        vec![Step::Token("Started.")],
    ]);
    let backend = Arc::new(FakeBackend::default());
    let fast = agent(model, backend.clone(), FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("draft it");
    fast.answer(
        &call(),
        &turn_id,
        "draft it",
        speech,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(backend.dispatches.lock().unwrap().len(), 1);
    // Turn start, the tool, and the final transcript check.
    assert_eq!(backend.authorizations.load(Ordering::SeqCst), 3);

    // Authority revoked after the turn started: the check before the tool
    // refuses it and nothing is dispatched.
    let model = ScriptedModel::new(vec![
        vec![
            Step::Sleep(Duration::from_millis(20)),
            Step::Tool("dispatch_task", json!({"description":"draft the reply"})),
        ],
        vec![Step::Token("I can't right now.")],
    ]);
    let revoking = Arc::new(FakeBackend::default());
    let fast = agent(model.clone(), revoking.clone(), FastLayerConfig::default());
    let (turn_id, speech) = relay.turn("draft it again");
    let task = {
        let fast = fast.clone();
        tokio::spawn(async move {
            fast.answer(
                &call(),
                &turn_id,
                "draft it again",
                speech,
                CancellationToken::new(),
            )
            .await
        })
    };
    tokio::time::sleep(Duration::from_millis(2)).await;
    revoking.revoked.store(true, Ordering::SeqCst);
    let _ = task.await.unwrap();
    assert!(
        revoking.dispatches.lock().unwrap().is_empty(),
        "revoked call dispatched"
    );
    let requests = model.requests();
    assert_eq!(requests.len(), 2, "the model hears the refusal");
    assert!(requests[1].messages.iter().any(|m| matches!(m,
        FastMessage::ToolResult { content, is_error: true, .. } if content.contains("unauthorized"))));
}

// Step 4 / AC4: the model sees exactly three tools (#1557 adds end_call); any
// other name gets a typed refusal and the turn still completes.
#[tokio::test]
async fn step4_tool_surface_is_exactly_three_and_unknown_tools_are_refused() {
    let model = ScriptedModel::new(vec![
        vec![Step::Tool("send_email", json!({"to":"someone"}))],
        vec![Step::Token("I can't send email from the call.")],
    ]);
    let backend = Arc::new(FakeBackend::default());
    let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("email Sam");
    let answer = fast
        .answer(
            &call(),
            &turn_id,
            "email Sam",
            speech,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(answer, "I can't send email from the call.");
    let requests = model.requests();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.tools, ["dispatch_task", "lookup_fast", "end_call"]);
        // #1547 sentence marker: the chunker splits on it and strips it.
        assert!(request.system.contains("Put • between sentences."));
    }
    assert!(requests[1].messages.iter().any(|m| matches!(m,
        FastMessage::ToolResult { content, is_error: true, .. } if content.contains("tool_not_allowed"))));
    assert!(backend.dispatches.lock().unwrap().is_empty());
    assert!(tokens(&relay.drain()).last().unwrap().1);
    assert_eq!(
        augmentagent_phone::fast::TOOL_NAMES[..],
        ["dispatch_task", "lookup_fast", "end_call"]
    );
}

// lookup_fast answers from the backend, and is cut off at 1.5 s.
#[tokio::test(start_paused = true)]
async fn lookup_fast_returns_results_and_is_bounded_to_one_and_a_half_seconds() {
    for (delay, expect_result) in [
        (Duration::from_millis(100), true),
        (Duration::from_secs(5), false),
    ] {
        let model = ScriptedModel::new(vec![
            vec![Step::Tool(
                "lookup_fast",
                json!({"query":"calendar tomorrow"}),
            )],
            vec![Step::Token("Done.")],
        ]);
        let backend = Arc::new(FakeBackend::default());
        *backend.lookup_delay.lock().unwrap() = delay;
        let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
        let mut relay = Relay::new();
        let (turn_id, speech) = relay.turn("what's tomorrow");
        let started = tokio::time::Instant::now();
        fast.answer(
            &call(),
            &turn_id,
            "what's tomorrow",
            speech,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() <= Duration::from_millis(1_600),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(*backend.lookups.lock().unwrap(), ["calendar tomorrow"]);
        let requests = model.requests();
        let result = requests[1]
            .messages
            .iter()
            .find_map(|m| match m {
                FastMessage::ToolResult {
                    content, is_error, ..
                } => Some((content.clone(), *is_error)),
                _ => None,
            })
            .unwrap();
        if expect_result {
            assert!(!result.1);
            assert!(result.0.contains("Dentist at 9 a.m."));
        } else {
            assert!(result.1);
            assert!(result.0.contains("lookup_timeout"));
        }
    }
}

// ---------- websocket ingress harness ----------

struct Live {
    _dir: tempfile::TempDir,
    store: Arc<augmentagent_store::Store>,
    service: Arc<augmentagent_phone::PhoneService>,
    call_id: String,
    server: tokio::task::JoinHandle<()>,
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
}
fn signed(token: &str, url: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use hmac::{Hmac, Mac};
    let mut h = Hmac::<sha1::Sha1>::new_from_slice(token.as_bytes()).unwrap();
    h.update(url.as_bytes());
    STANDARD.encode(h.finalize().into_bytes())
}
struct NoDial;
#[async_trait]
impl augmentagent_phone::Telephony for NoDial {
    async fn create(
        &self,
        _: augmentagent_phone::DialRequest,
    ) -> Result<String, augmentagent_phone::ProviderError> {
        Ok("CA-test".into())
    }
    async fn hangup(&self, _: &str) -> Result<(), augmentagent_phone::ProviderError> {
        Ok(())
    }
}
struct Clock100;
impl augmentagent_phone::Clock for Clock100 {
    fn now_ms(&self) -> i64 {
        100
    }
}
/// A connected call with an accepted relay socket answered by `agent`.
async fn live(agent: Arc<dyn ConversationAgent>, fast: Option<FastLayerConfig>) -> Live {
    live_with(agent, fast, false).await
}
/// [`live`] with Relay debug events (tokens-played) optionally requested.
async fn live_with(
    agent: Arc<dyn ConversationAgent>,
    fast: Option<FastLayerConfig>,
    debug_events: bool,
) -> Live {
    use augmentagent_store::{
        owner::ControlConversationKind, Store, SurfaceAccountRef, SurfaceConversationRef,
        SurfaceOwnerRef, SurfacePlatform,
    };
    use futures::SinkExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path().join("db")).unwrap());
    let account = SurfaceAccountRef::new(SurfacePlatform::new("slack").unwrap(), "team").unwrap();
    let owner = SurfaceOwnerRef::new(account.clone(), "owner").unwrap();
    let chat = SurfaceConversationRef::new(account, "dm", None).unwrap();
    store.bind_surface_owner(&owner, 0).unwrap();
    store
        .set_surface_control_conversation(&chat, ControlConversationKind::Direct, 0)
        .unwrap();
    store
        .phone_import_destination("owner", "+12025550123", 0)
        .unwrap();
    let service = Arc::new(augmentagent_phone::PhoneService::new(
        store.clone(),
        Arc::new(NoDial),
        Arc::new(Clock100),
        augmentagent_phone::PhoneConfig::new(
            true,
            "https://phone.example.test",
            "test-verification-key-separate-32bytes",
            vec![augmentagent_phone::OwnerLink {
                surface_owner: owner.clone(),
                owner_id: "owner".into(),
            }],
        )
        .unwrap()
        .with_fast_layer(fast)
        .with_debug_events(debug_events),
    ));
    let grant = service.authorize(&owner, &chat).unwrap();
    let receipt = service
        .tool(&grant, "call_owner", json!({"request_id":"live"}))
        .await
        .unwrap();
    let call_id = receipt["id"].as_str().unwrap().to_owned();
    store
        .phone_provider_event(&call_id, "CA-test", CallState::AwaitingAcceptance, 100)
        .unwrap();
    store.phone_accept(&call_id, "CA-test", 100).unwrap();
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
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut request = format!("ws://{addr}/phone/relay/{call_id}")
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "x-twilio-signature",
        signed(
            "callback-secret",
            &format!("wss://phone.example.test/phone/relay/{call_id}"),
        )
        .parse()
        .unwrap(),
    );
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
        _dir: dir,
        store,
        service,
        call_id,
        server,
        socket,
    }
}
impl Live {
    async fn prompt(&mut self, text: &str) {
        use futures::SinkExt;
        self.socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"type":"prompt","voicePrompt":text,"last":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }
    /// Next JSON frame from the server, or `None` on close/timeout.
    async fn frame(&mut self, wait: Duration) -> Option<Value> {
        use futures::StreamExt;
        match tokio::time::timeout(wait, self.socket.next()).await {
            Ok(Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t)))) => {
                Some(serde_json::from_str(&t).unwrap())
            }
            _ => None,
        }
    }
    /// Frames up to and including the next `last:true` text frame.
    async fn talk_cycle(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Some(frame) = self.frame(Duration::from_secs(3)).await {
            let done = frame["type"] == "text" && frame["last"] == true;
            frames.push(frame);
            if done {
                return frames;
            }
        }
        panic!("talk cycle did not close: {frames:?}");
    }
}
fn spoken(frames: &[Value]) -> String {
    frames
        .iter()
        .filter(|f| f["type"] == "text")
        .map(|f| f["token"].as_str().unwrap())
        .collect()
}

// AC4 end to end: a refused tool never closes the socket and the next
// utterance is answered.
#[tokio::test]
async fn ac4_refused_tool_keeps_the_socket_open_and_the_next_utterance_is_answered() {
    let model = ScriptedModel::new(vec![
        vec![Step::Tool("run_shell", json!({"cmd":"rm -rf /"}))],
        vec![Step::Token("I can't do that on a call.")],
        vec![Step::Token("It's sunny today.")],
    ]);
    let backend = Arc::new(FakeBackend::default());
    let fast = agent(model, backend, FastLayerConfig::default());
    let mut live = live(fast, Some(FastLayerConfig::default())).await;
    live.prompt("run a shell command").await;
    assert_eq!(
        spoken(&live.talk_cycle().await),
        "I can't do that on a call."
    );
    live.prompt("how's the weather").await;
    assert_eq!(spoken(&live.talk_cycle().await), "It's sunny today.");
    let call = live.store.phone_provider_call(&live.call_id).unwrap();
    assert!(!call.stop_requested, "socket and call stay up");
    live.server.abort();
}

/// Backend whose dispatched task finishes when `release` fires, delivering
/// to the (fake) outbox first and then reporting completion.
fn completing_backend(release: Arc<Notify>, spoken: Arc<Mutex<Vec<bool>>>) -> Arc<FakeBackend> {
    let backend = Arc::new(FakeBackend::default());
    let weak = Arc::downgrade(&backend);
    *backend.on_dispatch.lock().unwrap() = Some(Box::new(move |events: TaskEvents| {
        let release = release.clone();
        let spoken = spoken.clone();
        let weak = weak.clone();
        tokio::spawn(async move {
            release.notified().await;
            if let Some(backend) = weak.upgrade() {
                backend.mirrors.lock().unwrap().push((
                    events.task_id().into(),
                    "Phone task — Jarvis: The draft is in your chat.".into(),
                ));
            }
            spoken
                .lock()
                .unwrap()
                .push(events.completed("The draft is in your chat."));
        });
    }));
    backend
}
fn dispatch_script() -> Arc<ScriptedModel> {
    ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"draft the reply"}),
        )],
        vec![Step::Token("On it; I'll tell you when it's done.")],
    ])
}

// Step 5: completion while the socket is open is spoken once as an injected
// system turn (`last:true`); after peer_closed nothing is spoken and the
// outbox still holds exactly one result.
#[tokio::test]
async fn step5_completion_is_spoken_once_while_live_and_only_mirrored_after_hangup() {
    for hang_up_first in [false, true] {
        let release = Arc::new(Notify::new());
        let spoken_flags = Arc::new(Mutex::new(Vec::new()));
        let backend = completing_backend(release.clone(), spoken_flags.clone());
        let fast = agent(
            dispatch_script(),
            backend.clone(),
            FastLayerConfig::default(),
        );
        let mut live = live(fast, Some(FastLayerConfig::default())).await;
        live.prompt("draft a reply to the last Linear comment")
            .await;
        assert_eq!(
            spoken(&live.talk_cycle().await),
            "On it; I'll tell you when it's done."
        );
        if hang_up_first {
            live.socket.close(None).await.unwrap();
            // The relay loop ends on peer_closed and requests a stop.
            tokio::time::timeout(Duration::from_secs(3), async {
                while !live
                    .store
                    .phone_provider_call(&live.call_id)
                    .unwrap()
                    .stop_requested
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), async {
            while spoken_flags.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let task_id = backend.dispatches.lock().unwrap()[0].0.clone();
        let results = backend
            .mirrors
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| *k == task_id)
            .count();
        assert_eq!(results, 1, "one outbox result");
        if hang_up_first {
            assert_eq!(
                *spoken_flags.lock().unwrap(),
                [false],
                "nothing spoken after hangup"
            );
        } else {
            assert_eq!(*spoken_flags.lock().unwrap(), [true]);
            let frame = live
                .frame(Duration::from_secs(3))
                .await
                .expect("one injected frame");
            assert_eq!(frame["type"], "text");
            assert_eq!(frame["last"], true);
            assert!(frame["token"]
                .as_str()
                .unwrap()
                .contains("The draft is in your chat."));
            assert!(
                live.frame(Duration::from_millis(400)).await.is_none(),
                "exactly one frame"
            );
        }
        live.server.abort();
    }
}

/// Collects every log line written while it is the default subscriber.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// AC7: fast-model diagnostics never carry credentials, phone numbers or chat
// text, even when the model fails and tools are refused.
#[tokio::test]
async fn ac7_fast_layer_diagnostics_are_redacted() {
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(logs.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "exfiltrate",
            json!({"key":"sk-ant-api03-SECRETKEY"}),
        )],
        vec![Step::Token("Call +12025550123 now. "), Step::Fail],
        vec![Step::Fail],
    ]);
    let backend = Arc::new(FakeBackend {
        chat_summary: "owner: my number is +12025550123 and token sk-ant-api03-SECRETKEY".into(),
        ..Default::default()
    });
    let fast = agent(model, backend, FastLayerConfig::default());
    let mut relay = Relay::new();
    for utterance in [
        "private: wire $5,000 to account 99887766",
        "another private sentence",
    ] {
        let (turn_id, speech) = relay.turn(utterance);
        assert!(fast
            .answer(
                &call(),
                &turn_id,
                utterance,
                speech,
                CancellationToken::new()
            )
            .await
            .is_err());
    }
    let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("scripted_failure"),
        "failures are diagnosable: {text}"
    );
    for private in [
        "SECRETKEY",
        "sk-ant",
        "2025550123",
        "99887766",
        "private",
        "wire",
        "exfiltrate",
        "Call +1",
    ] {
        assert!(!text.contains(private), "{private} leaked: {text}");
    }
    let error = augmentagent_phone::fast::FastModelError("http_4xx");
    assert_eq!(
        format!("{error} {error:?}"),
        "fast model failed: http_4xx FastModelError(\"http_4xx\")"
    );
}

// #1551 integration: fast-layer turns are recorded with provider `fast`; a
// turn that hands work off is `dispatched`; tool time is recorded.
#[tokio::test]
async fn metrics_record_fast_provider_dispatched_outcome_and_tool_time() {
    use augmentagent_store::phone::{TurnOutcome, TurnProvider};
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"draft the reply"}),
        )],
        vec![Step::Token("Started.")],
        vec![Step::Tool("lookup_fast", json!({"query":"tomorrow"}))],
        vec![Step::Token("Dentist at nine.")],
    ]);
    let backend = Arc::new(FakeBackend::default());
    *backend.lookup_delay.lock().unwrap() = Duration::from_millis(60);
    let fast = agent(model, backend, FastLayerConfig::default());
    let mut live = live(fast, Some(FastLayerConfig::default())).await;
    live.prompt("draft a reply").await;
    live.talk_cycle().await;
    live.prompt("what's tomorrow").await;
    live.talk_cycle().await;
    live.socket.close(None).await.unwrap();
    let records = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let records = live.store.phone_turn_metrics(&live.call_id).unwrap();
            if records.len() == 2 {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(records.iter().all(|r| r.provider == TurnProvider::Fast));
    assert_eq!(records[0].outcome, TurnOutcome::Dispatched);
    assert_eq!(records[1].outcome, TurnOutcome::Answered);
    assert!(
        records[1].tool_ms.is_some_and(|ms| ms >= 60),
        "{:?}",
        records[1].tool_ms
    );
    live.server.abort();
}

/// Dispatch hook: the heavy task emits `count` progress events `every`
/// apart, then delivers its result and reports completion.
fn progressing_backend(count: usize, every: Duration, done: Arc<Notify>) -> Arc<FakeBackend> {
    let backend = Arc::new(FakeBackend::default());
    let weak = Arc::downgrade(&backend);
    *backend.on_dispatch.lock().unwrap() = Some(Box::new(move |events: TaskEvents| {
        let weak = weak.clone();
        let done = done.clone();
        tokio::spawn(async move {
            for i in 0..count {
                events.progress(&format!(
                    "Working on step {i} of the draft. {}",
                    "x".repeat(200)
                ));
                tokio::time::sleep(every).await;
            }
            if let Some(backend) = weak.upgrade() {
                backend.mirrors.lock().unwrap().push((
                    events.task_id().into(),
                    "Phone task — Jarvis: Draft ready.".into(),
                ));
            }
            events.completed("Draft ready.");
            done.notify_one();
        });
    }));
    backend
}
fn system_events(events: &[SpeechEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SpeechEvent::System { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// Step 8 / AC9: 10 progress events in 10 s reach the fast layer twice (one per
// 5 s), one is spoken (one per 15 s), none are mirrored; the final result is
// mirrored once.
#[tokio::test(start_paused = true)]
async fn step8_progress_is_rate_limited_spoken_sparingly_and_never_mirrored() {
    let done = Arc::new(Notify::new());
    let backend = progressing_backend(10, Duration::from_secs(1), done.clone());
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"draft the reply"}),
        )],
        vec![Step::Token("Started.")],
        vec![Step::Token("Still going.")],
    ]);
    let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("draft it");
    fast.answer(
        &call(),
        &turn_id,
        "draft it",
        speech,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    done.notified().await;
    let announced = system_events(&relay.drain());
    let progress_spoken: Vec<_> = announced
        .iter()
        .filter(|t| t.contains("Working on step"))
        .collect();
    assert_eq!(progress_spoken.len(), 1, "{announced:?}");
    assert!(progress_spoken[0].chars().count() <= 140);
    assert_eq!(
        announced
            .iter()
            .filter(|t| t.contains("Draft ready."))
            .count(),
        1
    );
    // The fast layer received exactly two progress events.
    let (turn_id, speech) = relay.turn("how's it going");
    fast.answer(
        &call(),
        &turn_id,
        "how's it going",
        speech,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let system = model.requests().last().unwrap().system.clone();
    assert_eq!(system.matches("Working on step").count(), 2, "{system}");
    let mirrors = backend.mirrors.lock().unwrap();
    assert!(mirrors
        .iter()
        .all(|(_, text)| !text.contains("Working on step")));
    assert_eq!(
        mirrors
            .iter()
            .filter(|(k, _)| k.ends_with(":dispatch"))
            .count(),
        1
    );
}

// AC9: no task delivers more than six progress events.
#[tokio::test(start_paused = true)]
async fn progress_is_capped_at_six_per_task() {
    let done = Arc::new(Notify::new());
    let backend = progressing_backend(10, Duration::from_secs(6), done.clone());
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"long task"}),
        )],
        vec![Step::Token("Started.")],
        vec![Step::Token("Okay.")],
    ]);
    let fast = agent(model.clone(), backend, FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("do it");
    fast.answer(&call(), &turn_id, "do it", speech, CancellationToken::new())
        .await
        .unwrap();
    done.notified().await;
    let announced = system_events(&relay.drain());
    // 60 s of progress: one spoken line per 15 s window at most.
    let spoken = announced
        .iter()
        .filter(|t| t.contains("Working on step"))
        .count();
    assert!((1..=4).contains(&spoken), "{announced:?}");
    let (turn_id, speech) = relay.turn("status");
    fast.answer(
        &call(),
        &turn_id,
        "status",
        speech,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let system = model.requests().last().unwrap().system.clone();
    assert_eq!(system.matches("Working on step").count(), 6, "{system}");
}

fn lookup_step(n: usize) -> Step {
    Step::Tool("lookup_fast", json!({"query": format!("query {n}")}))
}
fn tool_results(messages: &[FastMessage]) -> Vec<(String, bool)> {
    messages
        .iter()
        .filter_map(|m| match m {
            FastMessage::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect()
}

// Step 9 / AC10: at most three tool calls per utterance; the fourth is
// refused, the model answers in words and the talk cycle closes.
#[tokio::test]
async fn step9_fourth_tool_call_in_one_utterance_is_refused_and_the_turn_completes() {
    let model = ScriptedModel::new(vec![
        vec![
            lookup_step(1),
            lookup_step(2),
            lookup_step(3),
            lookup_step(4),
        ],
        vec![Step::Token("Here's what I found.")],
    ]);
    let backend = Arc::new(FakeBackend::default());
    let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("look up four things");
    let answer = fast
        .answer(
            &call(),
            &turn_id,
            "look up four things",
            speech,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(answer, "Here's what I found.");
    assert_eq!(backend.lookups.lock().unwrap().len(), 3);
    let results = tool_results(&model.requests()[1].messages);
    assert_eq!(results.len(), 4);
    assert!(results[..3].iter().all(|(_, error)| !error), "{results:?}");
    assert!(
        results[3].1 && results[3].0.contains("tool_step_limit"),
        "{results:?}"
    );
    assert!(tokens(&relay.drain()).last().unwrap().1);
}

// AC10: a model that keeps asking for tools is stopped and the turn still
// closes with last:true; the cap follows AUGMENTAGENT_PHONE_MAX_TOOL_STEPS.
#[tokio::test]
async fn tool_step_cap_is_configurable_and_bounds_model_rounds() {
    let model = ScriptedModel::new((0..20).map(|n| vec![lookup_step(n)]).collect());
    let backend = Arc::new(FakeBackend::default());
    let config = FastLayerConfig {
        max_tool_steps: 2,
        ..FastLayerConfig::default()
    };
    let fast = agent(model.clone(), backend.clone(), config);
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("loop forever");
    fast.answer(
        &call(),
        &turn_id,
        "loop forever",
        speech,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(backend.lookups.lock().unwrap().len(), 2);
    assert!(
        model.requests().len() <= 4,
        "rounds: {}",
        model.requests().len()
    );
    assert!(tokens(&relay.drain()).last().unwrap().1);
}

// AC10 end to end: socket stays open and the next utterance is answered.
#[tokio::test]
async fn ac10_capped_turn_keeps_the_socket_open() {
    let model = ScriptedModel::new(vec![
        vec![
            lookup_step(1),
            lookup_step(2),
            lookup_step(3),
            lookup_step(4),
        ],
        vec![Step::Token("Three lookups done.")],
        vec![Step::Token("Next answer.")],
    ]);
    let fast = agent(
        model,
        Arc::new(FakeBackend::default()),
        FastLayerConfig::default(),
    );
    let mut live = live(fast, Some(FastLayerConfig::default())).await;
    live.prompt("look up four things").await;
    assert_eq!(spoken(&live.talk_cycle().await), "Three lookups done.");
    live.prompt("and then").await;
    assert_eq!(spoken(&live.talk_cycle().await), "Next answer.");
    live.server.abort();
}

const SOUND: &str = "https://phone.example.test/sounds/thinking.mp3";
fn plays(events: &[SpeechEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SpeechEvent::Play { source, .. } => Some(source.clone()),
            _ => None,
        })
        .collect()
}
async fn thinking_run(sound: Option<&str>, lookup: Duration, preamble: bool) -> Vec<SpeechEvent> {
    let mut first = Vec::new();
    if preamble {
        first.push(Step::Token("Let me check. "));
    }
    first.push(Step::Tool("lookup_fast", json!({"query":"tomorrow"})));
    first.push(Step::Tool("lookup_fast", json!({"query":"next week"})));
    let model = ScriptedModel::new(vec![first, vec![Step::Token("Dentist at nine.")]]);
    let backend = Arc::new(FakeBackend::default());
    *backend.lookup_delay.lock().unwrap() = lookup;
    let config = FastLayerConfig {
        thinking_sound_url: sound.map(str::to_owned),
        ..FastLayerConfig::default()
    };
    let fast = agent(model, backend, config);
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("what's tomorrow");
    fast.answer(
        &call(),
        &turn_id,
        "what's tomorrow",
        speech,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    relay.drain()
}

// Step 10 / AC11: a tool in flight past 700 ms with nothing spoken sends one
// play request; a fast tool, earlier speech or an unset flag sends none.
#[tokio::test(start_paused = true)]
async fn step10_thinking_sound_plays_once_only_for_slow_silent_tools() {
    let events = thinking_run(Some(SOUND), Duration::from_millis(1_000), false).await;
    assert_eq!(plays(&events), [SOUND], "two slow tools, one sound");
    // The sound precedes the answer's text.
    let first_text = events
        .iter()
        .position(|e| matches!(e, SpeechEvent::Token { .. }))
        .unwrap();
    let play = events
        .iter()
        .position(|e| matches!(e, SpeechEvent::Play { .. }))
        .unwrap();
    assert!(play < first_text);
    assert!(plays(&thinking_run(Some(SOUND), Duration::from_millis(400), false).await).is_empty());
    assert!(plays(&thinking_run(Some(SOUND), Duration::from_millis(1_000), true).await).is_empty());
    assert!(plays(&thinking_run(None, Duration::from_millis(1_000), false).await).is_empty());
}

// AC11 on the wire: one `play` frame with preemptible:true, then the answer.
#[tokio::test]
async fn ac11_thinking_sound_frame_is_preemptible_and_sent_once() {
    let model = ScriptedModel::new(vec![
        vec![Step::Tool("lookup_fast", json!({"query":"tomorrow"}))],
        vec![Step::Token("Dentist at nine.")],
    ]);
    let backend = Arc::new(FakeBackend::default());
    *backend.lookup_delay.lock().unwrap() = Duration::from_millis(900);
    let config = FastLayerConfig {
        thinking_sound_url: Some(SOUND.into()),
        ..FastLayerConfig::default()
    };
    let fast = agent(model, backend, config.clone());
    let mut live = live(fast, Some(config)).await;
    live.prompt("what's tomorrow").await;
    let frames = live.talk_cycle().await;
    let play: Vec<_> = frames.iter().filter(|f| f["type"] == "play").collect();
    assert_eq!(play.len(), 1, "{frames:?}");
    assert_eq!(
        *play[0],
        json!({"type":"play","source":SOUND,"loop":1,"preemptible":true})
    );
    assert_eq!(frames[0]["type"], "play");
    assert_eq!(spoken(&frames), "Dentist at nine.");
    live.server.abort();
}

// ---------- #1557 agent-initiated end_call ----------

/// Sign a form callback the way Twilio does: URL followed by sorted fields.
fn signed_form(token: &str, url: &str, fields: &[(String, String)]) -> String {
    let mut payload = url.to_string();
    let mut fields = fields.to_vec();
    fields.sort();
    for (k, v) in fields {
        payload.push_str(&k);
        payload.push_str(&v);
    }
    signed(token, &payload)
}
impl Live {
    /// The signed `<Connect action>` callback Twilio sends after `end`.
    fn relay_ended(&self, handoff: Option<&str>) -> String {
        let mut fields = vec![
            ("AccountSid".to_string(), "AC-test".to_string()),
            ("CallSid".into(), "CA-test".into()),
            ("SessionStatus".into(), "completed".into()),
        ];
        if let Some(handoff) = handoff {
            fields.push(("HandoffData".into(), handoff.into()));
        }
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
                &signed_form("callback-secret", &url, &fields),
            )
            .unwrap()
    }
    async fn played(&mut self) {
        use futures::SinkExt;
        self.socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({"type":"info","name":"tokensPlayed","value":"x","last":true})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }
}
fn end_script(reason: &str, goodbye: &str) -> Arc<ScriptedModel> {
    let input = json!({"reason": reason, "goodbye": goodbye});
    ScriptedModel::new(vec![vec![Step::Tool(
        "end_call",
        input,
    )]])
}
/// Parsed `handoffData` of an `end` frame; panics on anything else.
fn handoff(frame: &Value) -> Value {
    assert_eq!(frame["type"], "end", "{frame}");
    let data = frame["handoffData"].as_str().expect("handoffData is a string");
    serde_json::from_str(data).expect("handoffData is JSON")
}

// TDD 1 / AC1: end_call(task_complete) produces one goodbye text frame with
// last:true, then exactly one `end` frame carrying {reason, call_id}; the
// relay-ended callback persists end_reason = task_complete.
#[tokio::test]
async fn end_call_step1_goodbye_then_one_end_frame_and_reason_is_persisted() {
    let model = end_script("task_complete", "All set. Talk soon!");
    let backend = Arc::new(FakeBackend::default());
    let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
    let mut live = live_with(fast, Some(FastLayerConfig::default()), true).await;
    live.prompt("great, that covers everything").await;
    let goodbye = live.talk_cycle().await;
    assert_eq!(goodbye.len(), 1, "exactly one goodbye frame: {goodbye:?}");
    assert_eq!(goodbye[0]["token"], "All set. Talk soon!");
    assert_eq!(goodbye[0]["last"], true);
    live.played().await;
    let end = live
        .frame(Duration::from_secs(3))
        .await
        .expect("end frame after the goodbye was played");
    assert_eq!(
        handoff(&end),
        json!({"reason":"task_complete","call_id": live.call_id})
    );
    assert!(
        live.frame(Duration::from_millis(300)).await.is_none(),
        "nothing after end"
    );
    assert_eq!(model.requests().len(), 1);
    let data = end["handoffData"].as_str().unwrap().to_owned();
    assert!(live.relay_ended(Some(&data)).contains("<Hangup/>"));
    assert_eq!(
        live.store.phone_end_reason(&live.call_id).unwrap().as_deref(),
        Some("task_complete")
    );
    // The goodbye is part of the mirrored transcript.
    assert!(backend
        .mirrors
        .lock()
        .unwrap()
        .iter()
        .any(|(_, text)| text == "Phone — Jarvis: All set. Talk soon!"));
    live.server.abort();
}

// TDD 2 / AC2: `end` follows the goodbye at 4,000 ms ±50 when no tokens-played
// event arrives, and as soon as one arrives when it is earlier. Real clock: a
// paused clock auto-advances while loopback socket reads are pending, which
// skews client-side timing by whole timer periods.
#[tokio::test]
async fn end_call_step2_end_waits_for_tokens_played_or_four_seconds() {
    for played_after in [None, Some(Duration::from_millis(1_200))] {
        let model = end_script("owner_request", "Okay, hanging up now. Bye!");
        let fast = agent(
            model,
            Arc::new(FakeBackend::default()),
            FastLayerConfig::default(),
        );
        let mut live = live_with(fast, Some(FastLayerConfig::default()), true).await;
        live.prompt("please hang up on me now").await;
        let goodbye = live.talk_cycle().await;
        assert_eq!(goodbye.len(), 1);
        let sent = tokio::time::Instant::now();
        if let Some(after) = played_after {
            tokio::time::sleep(after).await;
            live.played().await;
        }
        let end = live
            .frame(Duration::from_secs(10))
            .await
            .expect("end frame");
        let waited = sent.elapsed();
        assert_eq!(handoff(&end)["reason"], "owner_request");
        match played_after {
            None => assert!(
                waited >= Duration::from_millis(3_950) && waited <= Duration::from_millis(4_050),
                "fallback end after {waited:?}"
            ),
            Some(after) => assert!(
                waited >= after && waited < after + Duration::from_millis(50),
                "played-event end after {waited:?}"
            ),
        }
        live.server.abort();
    }
}

/// The `End` request queued for the relay loop, if any.
fn end_events(events: &[SpeechEvent]) -> Vec<(String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            SpeechEvent::End {
                goodbye, reason, ..
            } => Some((goodbye.clone(), reason.as_str().to_owned())),
            _ => None,
        })
        .collect()
}

// TDD 3 / AC3: each fixed goodbye phrase, as the whole final prompt, ends the
// call as owner_goodbye with zero model calls; near misses go to the model.
#[tokio::test]
async fn end_call_step3_goodbye_phrases_end_without_a_model_call() {
    let phrases = [
        "Bye.",
        "Goodbye!",
        "bye bye",
        "Talk later.",
        "That's all.",
        "That’s it, thanks.",
        "Hang up",
        "End the call.",
    ];
    assert_eq!(
        augmentagent_phone::end_call::GOODBYE_PHRASES.len(),
        phrases.len()
    );
    for phrase in phrases {
        let model = ScriptedModel::new(vec![]);
        let backend = Arc::new(FakeBackend::default());
        let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
        let mut relay = Relay::new();
        let (turn_id, speech) = relay.turn(phrase);
        fast.answer(&call(), &turn_id, phrase, speech, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(model.requests().len(), 0, "{phrase}: no model call");
        let ends = end_events(&relay.drain());
        assert_eq!(ends.len(), 1, "{phrase}");
        assert_eq!(ends[0].1, "owner_goodbye", "{phrase}");
        assert!(!ends[0].0.is_empty() && ends[0].0.chars().count() <= 120);
    }
    for near_miss in [
        "goodbye for now, one more thing",
        "don't hang up yet",
        "that's all for the first item",
        "bye the way, what's next",
        "can you end the call summary with a list",
    ] {
        let model = ScriptedModel::new(vec![vec![Step::Token("Sure.")]]);
        let fast = agent(
            model.clone(),
            Arc::new(FakeBackend::default()),
            FastLayerConfig::default(),
        );
        let mut relay = Relay::new();
        let (turn_id, speech) = relay.turn(near_miss);
        fast.answer(&call(), &turn_id, near_miss, speech, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(model.requests().len(), 1, "{near_miss}: answered by the model");
        assert!(end_events(&relay.drain()).is_empty(), "{near_miss}");
    }
}

// TDD 4 / AC4: with a dispatched task in flight a goodbye phrase is not a
// shortcut; the model's end_call speaks the fixed hand-off line first, `end`
// follows, and the heavy result still reaches the chat outbox exactly once.
#[tokio::test]
async fn end_call_step4_in_flight_task_gets_the_handoff_line_and_one_chat_result() {
    let release = Arc::new(Notify::new());
    let spoken_flags = Arc::new(Mutex::new(Vec::new()));
    let backend = completing_backend(release.clone(), spoken_flags.clone());
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"draft the reply"}),
        )],
        vec![Step::Token("On it.")],
        vec![Step::Tool(
            "end_call",
            json!({"reason":"owner_goodbye","goodbye":"Bye for now!"}),
        )],
    ]);
    let fast = agent(model.clone(), backend.clone(), FastLayerConfig::default());
    let mut live = live_with(fast, Some(FastLayerConfig::default()), true).await;
    live.prompt("draft a reply to the last Linear comment").await;
    assert_eq!(spoken(&live.talk_cycle().await), "On it.");
    live.prompt("bye").await;
    let goodbye = live.talk_cycle().await;
    assert_eq!(goodbye.len(), 1, "{goodbye:?}");
    let text = goodbye[0]["token"].as_str().unwrap();
    assert!(
        text.starts_with("I'll text you the result."),
        "hand-off line first: {text}"
    );
    assert_eq!(model.requests().len(), 3, "the in-flight guard skips the shortcut");
    live.played().await;
    let end = live.frame(Duration::from_secs(3)).await.expect("end frame");
    assert_eq!(handoff(&end)["reason"], "owner_goodbye");
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), async {
        while spoken_flags.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let task_id = backend.dispatches.lock().unwrap()[0].0.clone();
    let results = backend
        .mirrors
        .lock()
        .unwrap()
        .iter()
        .filter(|(k, _)| *k == task_id)
        .count();
    assert_eq!(results, 1, "one outbox result");
    assert_eq!(*spoken_flags.lock().unwrap(), [false], "not spoken after end");
    live.server.abort();
}

// end_call arguments are validated: an unknown reason is a typed refusal and
// the turn completes in words; an over-long goodbye is cut to 120 chars.
#[tokio::test]
async fn end_call_rejects_unknown_reasons_and_bounds_the_goodbye() {
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "end_call",
            json!({"reason":"because","goodbye":"Bye."}),
        )],
        vec![Step::Token("Still here.")],
    ]);
    let fast = agent(
        model.clone(),
        Arc::new(FakeBackend::default()),
        FastLayerConfig::default(),
    );
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("you can go");
    let answer = fast
        .answer(&call(), &turn_id, "you can go", speech, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(answer, "Still here.");
    assert!(end_events(&relay.drain()).is_empty());
    assert!(model.requests()[1].messages.iter().any(|m| matches!(m,
        FastMessage::ToolResult { content, is_error: true, .. } if content.contains("invalid_arguments"))));

    let long = "Thanks so much for the chat today. ".repeat(10);
    let model = ScriptedModel::new(vec![vec![Step::Tool(
        "end_call",
        json!({"reason":"owner_goodbye","goodbye": long}),
    )]]);
    let fast = agent(
        model,
        Arc::new(FakeBackend::default()),
        FastLayerConfig::default(),
    );
    let (turn_id, speech) = relay.turn("ok we're done");
    fast.answer(&call(), &turn_id, "ok we're done", speech, CancellationToken::new())
        .await
        .unwrap();
    let ends = end_events(&relay.drain());
    assert_eq!(ends.len(), 1);
    assert!(ends[0].0.chars().count() <= 120, "{}", ends[0].0);
    assert!(ends[0].0.starts_with("Thanks so much for the chat today."));
}

// ---------- #1548 shared conversation queue ----------

/// Native-path stand-in that waited 700 ms for the shared conversation queue:
/// it reports the wait, speaks the fixed notice, then answers.
struct QueuedNativeAgent;
#[async_trait]
impl ConversationAgent for QueuedNativeAgent {
    async fn answer(
        &self,
        _: &PhoneCall,
        _: &str,
        _: &str,
        speech: SpeechSender,
        _: CancellationToken,
    ) -> Result<String, AgentError> {
        assert!(speech.notice("queue_wait_1", "I'm still finishing something, one moment."));
        tokio::time::sleep(Duration::from_millis(60)).await;
        speech.note_queue_wait(Duration::from_millis(700));
        speech.speak_partial("All done.").await?;
        speech.finish().await?;
        Ok("All done.".into())
    }
    async fn interrupted(&self, _: &PhoneCall, _: &str) -> Result<(), AgentError> {
        Ok(())
    }
}

// AC3 / AC4 on the wire: the wait notice is its own spoken frame ahead of the
// answer, the socket stays open, the notice is not counted as the answer's
// first speech, and the queue wait lands in `queue_wait_ms`.
#[tokio::test]
async fn queue_wait_notice_is_spoken_on_the_socket_and_wait_is_recorded() {
    let mut live = live(Arc::new(QueuedNativeAgent), None).await;
    live.prompt("status?").await;
    let notice = live.talk_cycle().await;
    assert_eq!(
        spoken(&notice),
        "I'm still finishing something, one moment."
    );
    assert_eq!(spoken(&live.talk_cycle().await), "All done.");
    let call = live.store.phone_provider_call(&live.call_id).unwrap();
    assert!(!call.stop_requested, "socket and call stay up");
    live.socket.close(None).await.unwrap();
    let records = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let records = live.store.phone_turn_metrics(&live.call_id).unwrap();
            if !records.is_empty() {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert!(
        record.queue_wait_ms.is_some_and(|ms| ms >= 700),
        "{record:?}"
    );
    assert!(
        record.t_first_speech_ms.is_some_and(|ms| ms >= 60),
        "the notice is not the answer's first speech: {record:?}"
    );
    live.server.abort();
}

// #1553 AC5: an accepted proposal's agenda is in the first turn's prompt,
// with the instruction to open with it.
#[tokio::test]
async fn proposal_agenda_is_in_the_first_turn_prompt() {
    let model = ScriptedModel::new(vec![vec![Step::Token("I called about the venue.")]]);
    let backend = Arc::new(FakeBackend {
        agenda: Some("Reason: Two follow-ups are due\n- Reply to the venue\n- Confirm Friday dinner".into()),
        ..Default::default()
    });
    let fast = agent(model.clone(), backend, FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("hey");
    fast.answer(&call(), &turn_id, "hey", speech, CancellationToken::new())
        .await
        .unwrap();
    let system = model.requests()[0].system.clone();
    assert!(system.contains(augmentagent_phone::proposals::AGENDA_INSTRUCTION), "{system}");
    assert!(system.contains("- Reply to the venue\n- Confirm Friday dinner"), "{system}");
    // A long multi-byte agenda keeps its head: the reason and first item.
    let long = format!("Reason: Ünïcödé reason\n- first item\n{}", "- ✓✓✓✓✓✓✓✓✓✓\n".repeat(200));
    let wide = ScriptedModel::new(vec![vec![Step::Token("Ok.")]]);
    let fast = agent(
        wide.clone(),
        Arc::new(FakeBackend { agenda: Some(long), ..Default::default() }),
        FastLayerConfig::default(),
    );
    let (turn_id, speech) = relay.turn("hey");
    fast.answer(&call(), &turn_id, "hey", speech, CancellationToken::new())
        .await
        .unwrap();
    let system = wide.requests()[0].system.clone();
    assert!(system.contains("Reason: Ünïcödé reason\n- first item"), "{system}");
    // Without a proposal there is no agenda section.
    let plain = ScriptedModel::new(vec![vec![Step::Token("Hi.")]]);
    let fast = agent(plain.clone(), Arc::new(FakeBackend::default()), FastLayerConfig::default());
    let (turn_id, speech) = relay.turn("hey");
    fast.answer(&call(), &turn_id, "hey", speech, CancellationToken::new())
        .await
        .unwrap();
    assert!(!plain.requests()[0].system.contains("Agenda"));
}

// #1556: spoken task updates carry their task so the relay can hold its
// silence clock on progress and release it on the result.
#[tokio::test(start_paused = true)]
async fn spoken_task_updates_are_tagged_with_their_task_for_the_silence_clock() {
    use augmentagent_phone::ingress::TaskSpeech;
    let done = Arc::new(Notify::new());
    let backend = progressing_backend(1, Duration::from_secs(1), done.clone());
    let model = ScriptedModel::new(vec![
        vec![Step::Tool(
            "dispatch_task",
            json!({"description":"draft the reply"}),
        )],
        vec![Step::Token("Started.")],
    ]);
    let fast = agent(model, backend, FastLayerConfig::default());
    let mut relay = Relay::new();
    let (turn_id, speech) = relay.turn("draft it");
    fast.answer(&call(), &turn_id, "draft it", speech, CancellationToken::new())
        .await
        .unwrap();
    done.notified().await;
    let task = format!("{turn_id}:dispatch");
    let tags: Vec<_> = relay
        .drain()
        .into_iter()
        .filter_map(|e| match e {
            SpeechEvent::System { task, .. } => Some(task),
            _ => None,
        })
        .collect();
    assert_eq!(
        tags,
        [
            Some(TaskSpeech::Progress(task.clone())),
            Some(TaskSpeech::Result(task))
        ]
    );
}
