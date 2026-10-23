//! #1292 — owner commands on the real Slack dispatch path: Socket Mode →
//! durable inbound log → owner gate → command (private lane) or harness →
//! durable outbox → Web API.
//!
//! In-memory duplex WebSocket, `RecordingSlackWebApi`, temporary store and
//! model-selection file, and a probe agent that joins the native session
//! like the Claude/Codex adapters and reports the provider and session it
//! ran in. Identifiers and tokens are synthetic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, LoopRunner, LoopScheduler, QueryHandler};
use augmentagent_channel_core::model_selection::conversation_selection;
use augmentagent_channel_core::native_session::{Launch, CURRENT};
use augmentagent_channel_slack::commands::{
    slack_loop_owner, slack_selection, SlackCommandDeps, SlackCommands, SlackLoopPoster,
    SurfaceLoopPoster,
};
use augmentagent_channel_slack::harness::{
    SlackConversationHarness, CANCELLED_REPLY, SESSION_UNCERTAIN_REPLY,
};
use augmentagent_channel_slack::interactive::{
    SlackInteractiveSurface, SlackSurfaceConfig, SlackWorkspaceRuntime,
};
use augmentagent_channel_slack::owner::{SlackBotIdentity, REJECTION_REPLY};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::socket::{
    AsyncIo, BoxedWebSocket, ConnectError, SocketConnector, SocketModeConfig,
};
use augmentagent_channel_slack::transport::web::{
    PostEphemeral, PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi,
};
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::{NativeConversation, Store};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const STRANGER: &str = "U00000002";
const BOT_USER: &str = "U0000000B";
const DM: &str = "D00000001";
const STRANGER_DM: &str = "D00000002";
const CONTROL: &str = "C00000001";
const T0: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

type ServerSocket = WebSocketStream<DuplexStream>;

struct DuplexConnector {
    servers: mpsc::UnboundedSender<ServerSocket>,
}

#[async_trait]
impl SocketConnector for DuplexConnector {
    async fn connect(&self, _cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError> {
        let (client_half, server_half) = tokio::io::duplex(256 * 1024);
        let server = WebSocketStream::from_raw_socket(server_half, Role::Server, None).await;
        if self.servers.send(server).is_err() {
            return Err(ConnectError::Transient("test stopped listening".into()));
        }
        let boxed: Box<dyn AsyncIo> = Box::new(client_half);
        Ok(
            WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(boxed), Role::Client, None)
                .await,
        )
    }
}

/// Joins the turn's native session with the session's own provider, like
/// the CLI adapters, and says what it ran in. Outside a native session (the
/// legacy route for qwen/glm) it reports the conversation's selection.
#[derive(Default)]
struct ProbeAgent {
    prompts: Mutex<Vec<String>>,
    started: Notify,
    release: Notify,
}

#[async_trait]
impl QueryHandler for ProbeAgent {
    async fn answer(&self, _ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        self.prompts.lock().unwrap().push(question.to_string());
        let Ok(session) = CURRENT.try_with(Arc::clone) else {
            return Ok(format!(
                "legacy selection={:?}",
                conversation_selection().flatten().map(|k| k.name())
            ));
        };
        let provider = session.provider();
        let mut lease = session.begin(provider)?;
        let (id, resumed) = match lease.launch() {
            Launch::Resume { id } => (id, true),
            Launch::Create {
                requested_id: Some(id),
            } => (id, false),
            Launch::Create { requested_id: None } => (
                format!("{}-{}", provider.name(), uuid::Uuid::new_v4()),
                false,
            ),
        };
        lease.observe(&id)?;
        if question.contains("slow") {
            self.started.notify_one();
            self.release.notified().await;
        }
        lease.finish()?;
        Ok(format!(
            "provider={} session={id} resumed={resumed}",
            provider.name()
        ))
    }
}

impl ProbeAgent {
    fn calls(&self) -> usize {
        self.prompts.lock().unwrap().len()
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
    wiki: PathBuf,
    selection: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wiki = dir.path().join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        let store = Arc::new(Store::open(dir.path().join("data.db")).unwrap());
        let ws = workspace();
        store
            .bind_surface_owner(&ws.owner(OWNER).unwrap(), T0)
            .unwrap();
        store
            .set_surface_control_conversation(
                &ws.conversation(DM, None).unwrap(),
                ControlConversationKind::Direct,
                T0,
            )
            .unwrap();
        store
            .set_surface_control_conversation(
                &ws.conversation(CONTROL, None).unwrap(),
                ControlConversationKind::Channel,
                T0,
            )
            .unwrap();
        let selection = dir.path().join("config").join("model-selection.json");
        Harness {
            _dir: dir,
            store,
            web: Arc::new(RecordingSlackWebApi::default()),
            wiki,
            selection,
        }
    }

    fn surface(
        &self,
        agent: Arc<ProbeAgent>,
        idle_poll: Duration,
    ) -> (
        SlackInteractiveSurface,
        mpsc::UnboundedReceiver<ServerSocket>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let harness =
            SlackConversationHarness::new(Arc::clone(&self.store), agent, self.wiki.clone())
                .with_selection(slack_selection(self.selection.clone()));
        let mut deps = SlackCommandDeps::new(self.selection.clone());
        // Every profile is ready here (no operator pause gate in tests).
        deps.model_ready = Arc::new(|_| Ok(()));
        let commands = SlackCommands::new(Arc::clone(&self.store), deps);
        let surface = SlackInteractiveSurface::new(
            Arc::clone(&self.store),
            vec![SlackWorkspaceRuntime {
                workspace: workspace(),
                web: Arc::clone(&self.web) as Arc<dyn SlackWebApi>,
                bot: SlackBotIdentity {
                    bot_user_id: Some(BOT_USER.into()),
                    bot_id: Some("B00000001".into()),
                    app_id: Some("A00000001".into()),
                },
            }],
            vec![Arc::new(DuplexConnector { servers: tx }) as Arc<dyn SocketConnector>],
            Arc::new(harness),
            SlackSurfaceConfig {
                idle_poll,
                heartbeat: Duration::from_secs(3600),
                socket: SocketModeConfig {
                    backoff: BackoffConfig {
                        initial: Duration::from_millis(50),
                        max: Duration::from_millis(200),
                    },
                    ..SocketModeConfig::default()
                },
                ..SlackSurfaceConfig::default()
            },
        )
        .with_commands(Arc::new(commands));
        (surface, rx)
    }

    fn posts(&self) -> Vec<PostMessage> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostMessage(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn ephemerals(&self) -> Vec<PostEphemeral> {
        self.web
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostEphemeral(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn answers(&self, channel: &str, thread: Option<&str>) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|p| p.channel == channel && p.thread_ts.as_deref() == thread)
            .map(|p| p.text)
            .collect()
    }

    fn selected(&self, channel: &str, thread: Option<&str>) -> Option<&'static str> {
        slack_selection(self.selection.clone())(&workspace().conversation(channel, thread).unwrap())
            .unwrap()
            .map(|k| k.name())
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

struct Server {
    ws: ServerSocket,
}

impl Server {
    async fn accept(servers: &mut mpsc::UnboundedReceiver<ServerSocket>) -> Self {
        let mut ws = tokio::time::timeout(Duration::from_secs(5), servers.recv())
            .await
            .expect("client connects")
            .expect("connector alive");
        ws.send(Message::Text(
            json!({"type": "hello", "connection_info": {"app_id": "A00000001"}, "num_connections": 1})
                .to_string(),
        ))
        .await
        .unwrap();
        Server { ws }
    }

    /// Send a frame and wait for its ack (acked = persisted).
    async fn deliver(&mut self, frame: &Value) {
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .unwrap();
        let want = frame["envelope_id"].as_str().unwrap().to_string();
        loop {
            let next = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("an ack in time")
                .expect("socket open")
                .unwrap();
            if let Message::Text(text) = next {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v["envelope_id"].as_str() == Some(want.as_str()) {
                    return;
                }
            }
        }
    }
}

struct Running {
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn start(surface: SlackInteractiveSurface) -> Running {
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    Running {
        shutdown,
        task: tokio::spawn(async move { surface.run(sd).await }),
    }
}

impl Running {
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("surface stops promptly")
            .expect("no panic")
            .expect("clean stop");
    }
}

fn event(envelope_id: &str, event: Value) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "event_callback", "team_id": TEAM, "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"), "event_time": 1_700_000_000,
            "event": event,
        }
    })
}

fn owner_dm(envelope_id: &str, text: &str, ts: &str) -> Value {
    event(
        envelope_id,
        json!({"type": "message", "channel": DM, "channel_type": "im",
            "user": OWNER, "text": text, "ts": ts}),
    )
}

fn control_message(envelope_id: &str, text: &str, ts: &str) -> Value {
    event(
        envelope_id,
        json!({"type": "message", "channel": CONTROL, "channel_type": "group",
            "user": OWNER, "text": text, "ts": ts}),
    )
}

fn control_reply(envelope_id: &str, text: &str, ts: &str, thread: &str) -> Value {
    event(
        envelope_id,
        json!({"type": "message", "channel": CONTROL, "channel_type": "group",
            "user": OWNER, "text": text, "ts": ts, "thread_ts": thread}),
    )
}

fn slash(envelope_id: &str, user: &str, channel: &str, text: &str) -> Value {
    json!({
        "type": "slash_commands",
        "envelope_id": envelope_id,
        "accepts_response_payload": true,
        "payload": {
            "command": "/jarvis", "text": text, "team_id": TEAM, "user_id": user,
            "user_name": "someone", "channel_id": channel,
            "trigger_id": format!("1.{envelope_id}.trigger"),
            "response_url": "https://hooks.example.invalid/commands/1",
        }
    })
}

fn session_of(answer: &str) -> String {
    answer
        .split_whitespace()
        .find_map(|w| w.strip_prefix("session="))
        .unwrap_or_else(|| panic!("no session in {answer:?}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// Help, unknown input, owner-only
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slash_help_and_an_unknown_command_are_answered_from_the_registry() {
    let h = Harness::new();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server.deliver(&slash("s1", OWNER, DM, "help")).await;
    eventually("help", || h.answers(DM, None).len() == 1).await;
    server
        .deliver(&owner_dm(
            "m1",
            "!frobnicate the thing",
            "1700000001.000100",
        ))
        .await;
    eventually("unknown", || h.answers(DM, None).len() == 2).await;
    server.deliver(&slash("s2", OWNER, DM, "")).await;
    eventually("empty slash", || h.answers(DM, None).len() == 3).await;

    let answers = h.answers(DM, None);
    assert!(
        answers[0].contains("Jarvis owner commands"),
        "{}",
        answers[0]
    );
    assert!(answers[0].contains("#1298"), "{}", answers[0]);
    assert!(answers[1].contains("Unknown command"), "{}", answers[1]);
    assert!(
        answers[1].contains("Jarvis owner commands"),
        "{}",
        answers[1]
    );
    assert!(
        answers[2].contains("Jarvis owner commands"),
        "{}",
        answers[2]
    );
    assert_eq!(agent.calls(), 0, "commands never reach the agent");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_owner_slash_command_is_rejected_and_changes_nothing() {
    let h = Harness::new();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&slash(
            "s1",
            STRANGER,
            STRANGER_DM,
            "model set codex scope:default",
        ))
        .await;
    server
        .deliver(&slash("s2", STRANGER, CONTROL, "reset"))
        .await;
    eventually("rejection in their DM", || h.posts().len() == 1).await;
    eventually("ephemeral rejection", || h.ephemerals().len() == 1).await;
    let post = &h.posts()[0];
    // Top level in their DM, never a reply into the private dispatch lane.
    assert_eq!(
        (
            post.channel.as_str(),
            post.thread_ts.as_deref(),
            post.text.as_str()
        ),
        (STRANGER_DM, None, REJECTION_REPLY)
    );
    let eph = &h.ephemerals()[0];
    assert_eq!(
        (eph.channel.as_str(), eph.user.as_str()),
        (CONTROL, STRANGER)
    );
    assert_eq!(h.selected(DM, None), None);
    assert!(
        !h.selection.exists(),
        "the selection file was never written"
    );
    assert_eq!(agent.calls(), 0);
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Private lane, cancel all
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commands_never_queue_behind_a_running_turn_and_cancel_all_drops_the_queue() {
    let h = Harness::new();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&owner_dm("m1", "a slow task", "1700000001.000100"))
        .await;
    tokio::time::timeout(Duration::from_secs(5), agent.started.notified())
        .await
        .expect("the slow turn started");
    server
        .deliver(&owner_dm("m2", "a queued question", "1700000002.000100"))
        .await;
    // `status` in the same DM is answered while the turn still runs.
    server
        .deliver(&owner_dm("m3", "status", "1700000003.000100"))
        .await;
    eventually("status answered", || h.answers(DM, None).len() == 1).await;
    let status = h.answers(DM, None)[0].clone();
    assert!(status.contains("Running: yes"), "{status}");
    assert!(status.contains("Queued: 1"), "{status}");
    assert_eq!(agent.calls(), 1, "still only the slow turn");

    server
        .deliver(&owner_dm("m4", "cancel all", "1700000004.000100"))
        .await;
    eventually("cancel all answered and the turn stopped", || {
        h.answers(DM, None).len() == 3
    })
    .await;
    let answers = h.answers(DM, None);
    assert!(
        answers
            .iter()
            .any(|a| a.contains("Stopped the running request and dropped 1 queued")),
        "{answers:?}"
    );
    assert!(
        answers
            .iter()
            .any(|a| a.starts_with(&CANCELLED_REPLY[..20])),
        "{answers:?}"
    );
    // The queued question never ran and never will.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent.calls(), 1, "{:?}", agent.prompts.lock().unwrap());
    assert_eq!(h.answers(DM, None).len(), 3);
    let dm = workspace().conversation(DM, None).unwrap();
    assert_eq!(h.store.queued_inbound_events(&dm).unwrap(), 0);
    running.stop().await;
}

// ---------------------------------------------------------------------------
// reset
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_starts_a_new_native_session_for_the_conversation() {
    let h = Harness::new();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    for (i, text) in ["first", "second"].iter().enumerate() {
        server
            .deliver(&owner_dm(
                &format!("m{i}"),
                text,
                &format!("170000000{i}.000100"),
            ))
            .await;
        eventually("answered", || h.answers(DM, None).len() == i + 1).await;
    }
    let before = h.answers(DM, None);
    let s1 = session_of(&before[0]);
    assert_eq!(session_of(&before[1]), s1);
    assert!(before[1].contains("resumed=true"), "{before:?}");

    server
        .deliver(&owner_dm("m2", "reset", "1700000002.000100"))
        .await;
    eventually("reset answered", || h.answers(DM, None).len() == 3).await;
    assert!(
        h.answers(DM, None)[2].contains(&s1),
        "{:?}",
        h.answers(DM, None)
    );

    server
        .deliver(&owner_dm("m3", "third", "1700000003.000100"))
        .await;
    eventually("third answered", || h.answers(DM, None).len() == 4).await;
    let third = h.answers(DM, None)[3].clone();
    assert_ne!(session_of(&third), s1, "{third}");
    assert!(third.contains("resumed=false"), "{third}");
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dm_left_uncertain_is_recovered_with_reset() {
    let h = Harness::new();
    h.store
        .bind_surface_conversation(&NativeConversation {
            conversation: workspace().conversation(DM, None).unwrap(),
            provider: "claude".into(),
            native_session_id: "stuck-session".into(),
            cwd: "/wiki".into(),
            uncertain: true,
        })
        .unwrap();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&owner_dm("m1", "hello", "1700000001.000100"))
        .await;
    eventually("refused", || h.answers(DM, None).len() == 1).await;
    assert_eq!(h.answers(DM, None)[0], SESSION_UNCERTAIN_REPLY);
    assert!(SESSION_UNCERTAIN_REPLY.contains("`reset`"));
    server
        .deliver(&owner_dm("m2", "reset", "1700000002.000100"))
        .await;
    eventually("reset", || h.answers(DM, None).len() == 2).await;
    let reset = h.answers(DM, None)[1].clone();
    assert!(
        reset.contains("stuck-session") && reset.contains("part-way"),
        "{reset}"
    );
    server
        .deliver(&owner_dm("m3", "hello again", "1700000003.000100"))
        .await;
    eventually("answered", || h.answers(DM, None).len() == 3).await;
    let answer = h.answers(DM, None)[2].clone();
    assert!(answer.contains("provider=claude"), "{answer}");
    assert_ne!(session_of(&answer), "stuck-session");
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Model selection used by the next turn, across restart
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_selection_is_used_by_the_next_turn_and_survives_restart() {
    let h = Harness::new();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    server
        .deliver(&slash("s1", OWNER, DM, "model set codex"))
        .await;
    eventually("model set", || h.answers(DM, None).len() == 1).await;
    assert!(h.answers(DM, None)[0].contains("Model set to codex"));
    server
        .deliver(&owner_dm("m1", "hello", "1700000001.000100"))
        .await;
    eventually("answered", || h.answers(DM, None).len() == 2).await;
    let first = h.answers(DM, None)[1].clone();
    assert!(first.contains("provider=codex"), "{first}");

    // A control-channel message starts a thread; `model qwen` there applies
    // to that thread only, on the legacy (non-native) route.
    const A: &str = "1700000100.000100";
    server
        .deliver(&control_message("c1", "model qwen", A))
        .await;
    eventually("thread model set", || {
        h.answers(CONTROL, Some(A)).len() == 1
    })
    .await;
    server
        .deliver(&control_reply("c2", "hello thread", "1700000100.000200", A))
        .await;
    eventually("thread answered", || h.answers(CONTROL, Some(A)).len() == 2).await;
    assert_eq!(
        h.answers(CONTROL, Some(A))[1],
        "legacy selection=Some(\"qwen\")"
    );
    running.stop().await;

    // Restart: a new surface and harness over the same store and file.
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_secs(3600));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&owner_dm("m2", "after restart", "1700000002.000100"))
        .await;
    eventually("answered after restart", || h.answers(DM, None).len() == 3).await;
    let after = h.answers(DM, None)[2].clone();
    assert!(after.contains("provider=codex"), "{after}");
    assert_eq!(session_of(&after), session_of(&first));
    assert!(after.contains("resumed=true"), "{after}");
    server.deliver(&slash("s2", OWNER, DM, "model")).await;
    eventually("model shown", || h.answers(DM, None).len() == 4).await;
    let show = h.answers(DM, None)[3].clone();
    assert!(
        show.contains("codex") && show.contains("this conversation"),
        "{show}"
    );
    assert_eq!(h.selected(DM, None), Some("codex"));
    running.stop().await;
}

// ---------------------------------------------------------------------------
// A loop created on Slack fires into its Slack thread
// ---------------------------------------------------------------------------

struct CountingRunner(AtomicUsize);

#[async_trait]
impl LoopRunner for CountingRunner {
    async fn run_prompt(
        &self,
        _request_id: &str,
        _owner: &str,
        prompt: &str,
        _model: Option<&str>,
    ) -> anyhow::Result<String> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!("answer for {prompt}"))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_loop_created_in_a_thread_posts_its_result_into_that_thread() {
    let h = Harness::new();
    let agent = Arc::new(ProbeAgent::default());
    let (surface, mut servers) = h.surface(agent.clone(), Duration::from_millis(50));
    let running = start(surface);
    let mut server = Server::accept(&mut servers).await;

    const A: &str = "1700000200.000100";
    server
        .deliver(&control_message("c1", "loop 10m check the build", A))
        .await;
    eventually("loop created", || h.answers(CONTROL, Some(A)).len() == 1).await;
    assert!(h.answers(CONTROL, Some(A))[0].contains("created"));
    let owner = workspace().owner(OWNER).unwrap();
    assert_eq!(
        h.store
            .list_user_loops(&slack_loop_owner(&owner))
            .unwrap()
            .len(),
        1
    );

    let runner = Arc::new(CountingRunner(AtomicUsize::new(0)));
    let scheduler = Arc::new(LoopScheduler::new(
        Arc::clone(&h.store),
        runner.clone(),
        Arc::new(SurfaceLoopPoster {
            slack: Some(Arc::new(SlackLoopPoster::new(Arc::clone(&h.store)))),
            other: None,
        }),
    ));
    let stop = CancellationToken::new();
    let task = tokio::spawn(scheduler.run(stop.clone()));
    eventually("loop result posted in the thread", || {
        h.answers(CONTROL, Some(A)).len() == 2
    })
    .await;
    stop.cancel();
    task.await.unwrap().unwrap();
    let result = h.answers(CONTROL, Some(A))[1].clone();
    assert!(result.contains("answer for check the build"), "{result}");
    assert_eq!(runner.0.load(Ordering::SeqCst), 1);
    assert_eq!(agent.calls(), 0);
    running.stop().await;
}
