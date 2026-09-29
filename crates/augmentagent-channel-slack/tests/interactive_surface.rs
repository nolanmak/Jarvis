//! #1287 — the interactive Slack surface on its real dispatch path: Socket
//! Mode client → durable inbound log (persist, then ack) → owner gate →
//! turn handler → durable outbox → Web API.
//!
//! The socket is an in-memory duplex WebSocket (same pattern as
//! `transport_socket.rs`), the Web API is `RecordingSlackWebApi`, the turn
//! handler is a recording fake standing in for the reasoner, and the store
//! is a temporary file. The dispatcher's fallback poll is set to an hour,
//! so anything handled quickly was driven by the event itself, not by a
//! cadence. Nothing here needs Discord or WhatsApp state. Identifiers and
//! tokens are synthetic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use augmentagent_channel_slack::interactive::{
    report_inactive, SlackInteractiveSurface, SlackSurfaceConfig, SlackTurn, SlackTurnHandler,
    SlackTurnReply, SlackWorkspaceRuntime, SurfaceState,
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
use augmentagent_store::delivery::{InboundRecordOutcome, NewInboundEvent};
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::{Store, SurfacePlatform};
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

/// One in-memory WebSocket per connect; the server half goes to the test.
struct DuplexConnector {
    servers: mpsc::UnboundedSender<ServerSocket>,
    fatal: bool,
}

impl DuplexConnector {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<ServerSocket>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                servers: tx,
                fatal: false,
            }),
            rx,
        )
    }

    fn fatal() -> Arc<Self> {
        let (tx, _rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            servers: tx,
            fatal: true,
        })
    }
}

#[async_trait]
impl SocketConnector for DuplexConnector {
    async fn connect(&self, _cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError> {
        if self.fatal {
            return Err(ConnectError::Fatal(
                "apps.connections.open: invalid_auth".into(),
            ));
        }
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

/// Stands in for the reasoner: records each turn, answers `answer: <text>`,
/// and can hold every turn until released, or fail.
#[derive(Default)]
struct FakeHandler {
    turns: Mutex<Vec<(SlackTurn, Instant)>>,
    hold: AtomicBool,
    release: Notify,
    fail: AtomicBool,
    started: Notify,
    calls: AtomicU32,
}

impl FakeHandler {
    fn holding() -> Arc<Self> {
        let h = Self::default();
        h.hold.store(true, Ordering::SeqCst);
        Arc::new(h)
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn texts(&self) -> Vec<String> {
        self.turns
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _)| t.text.clone())
            .collect()
    }
}

#[async_trait]
impl SlackTurnHandler for FakeHandler {
    async fn handle_turn(&self, turn: &SlackTurn) -> anyhow::Result<Option<SlackTurnReply>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.turns
            .lock()
            .unwrap()
            .push((turn.clone(), Instant::now()));
        self.started.notify_one();
        if self.hold.load(Ordering::SeqCst) {
            self.release.notified().await;
        }
        if self.fail.load(Ordering::SeqCst) {
            anyhow::bail!("reasoner exploded with an internal detail");
        }
        Ok(Some(SlackTurnReply {
            text: format!("answer: {}", turn.text),
        }))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
}

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

fn bot() -> SlackBotIdentity {
    SlackBotIdentity {
        bot_user_id: Some(BOT_USER.into()),
        bot_id: Some("B00000001".into()),
        app_id: Some("A00000001".into()),
    }
}

fn bind_owner(store: &Store) {
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
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // Spaces and non-ASCII in the database path.
        let path = dir.path().join("state dir ü").join("data.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let store = Arc::new(Store::open(&path).unwrap());
        bind_owner(&store);
        Harness {
            _dir: dir,
            path,
            store,
            web: Arc::new(RecordingSlackWebApi::default()),
        }
    }

    /// A second handle on the same database, as a restarted daemon would open.
    fn reopen(&self) -> Arc<Store> {
        Arc::new(Store::open(&self.path).unwrap())
    }

    fn surface(
        &self,
        store: Arc<Store>,
        connector: Arc<dyn SocketConnector>,
        handler: Arc<dyn SlackTurnHandler>,
        dry_run: bool,
    ) -> SlackInteractiveSurface {
        SlackInteractiveSurface::new(
            store,
            vec![SlackWorkspaceRuntime {
                workspace: workspace(),
                web: Arc::clone(&self.web) as Arc<dyn SlackWebApi>,
                bot: bot(),
            }],
            vec![connector],
            handler,
            config(dry_run),
        )
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

    fn inbound_status(&self, event_id: &str) -> String {
        rusqlite::Connection::open(&self.path)
            .unwrap()
            .query_row(
                "SELECT status FROM surface_inbound_events WHERE event_id = ?1",
                [event_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn outbox(&self) -> Vec<(String, String, Option<String>)> {
        let conn = rusqlite::Connection::open(&self.path).unwrap();
        let mut stmt = conn
            .prepare("SELECT idempotency_key, status, provider_message_id FROM surface_outbox ORDER BY id")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    fn health_state(&self) -> Option<String> {
        self.store
            .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
            .unwrap()
            .map(|h| h.state)
    }
}

fn config(dry_run: bool) -> SlackSurfaceConfig {
    SlackSurfaceConfig {
        dry_run,
        // A cadence this long never fires inside a test: anything handled
        // quickly was driven by the event itself.
        idle_poll: Duration::from_secs(3600),
        heartbeat: Duration::from_secs(3600),
        socket: SocketModeConfig {
            backoff: BackoffConfig {
                initial: Duration::from_millis(50),
                max: Duration::from_millis(200),
            },
            ..SocketModeConfig::default()
        },
        ..SlackSurfaceConfig::default()
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

fn hello() -> Message {
    Message::Text(
        json!({"type": "hello", "connection_info": {"app_id": "A00000001"}, "num_connections": 1})
            .to_string(),
    )
}

fn message(
    envelope_id: &str,
    channel: &str,
    channel_type: &str,
    user: &str,
    text: &str,
    ts: &str,
) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "event_callback",
            "team_id": TEAM,
            "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"),
            "event_time": 1_700_000_000,
            "event": {
                "type": "message", "channel": channel, "channel_type": channel_type,
                "user": user, "text": text, "ts": ts,
            }
        }
    })
}

fn owner_dm(envelope_id: &str, text: &str, ts: &str) -> Value {
    message(envelope_id, DM, "im", OWNER, text, ts)
}

struct Server {
    ws: ServerSocket,
}

impl Server {
    async fn accept(servers: &mut mpsc::UnboundedReceiver<ServerSocket>) -> Self {
        let ws = tokio::time::timeout(Duration::from_secs(5), servers.recv())
            .await
            .expect("client connects")
            .expect("connector alive");
        let mut s = Server { ws };
        s.ws.send(hello()).await.unwrap();
        s
    }

    async fn send(&mut self, frame: &Value) {
        self.ws
            .send(Message::Text(frame.to_string()))
            .await
            .unwrap();
    }

    /// The next ack frame's envelope id (pings and pongs skipped).
    async fn ack(&mut self) -> String {
        loop {
            let frame = tokio::time::timeout(Duration::from_secs(5), self.ws.next())
                .await
                .expect("an ack in time")
                .expect("socket open")
                .unwrap();
            if let Message::Text(text) = frame {
                let v: Value = serde_json::from_str(&text).unwrap();
                return v["envelope_id"].as_str().unwrap().to_string();
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
            .expect("surface stops promptly after shutdown")
            .expect("surface task did not panic")
            .expect("surface stops cleanly");
    }
}

fn not_persisted_yet(store: &Store, event_id: &str) -> bool {
    let conversation = workspace().conversation(DM, None).unwrap();
    matches!(
        store
            .record_inbound_event(
                &NewInboundEvent {
                    conversation,
                    event_id: event_id.into(),
                    kind: "probe".into(),
                    occurred_at_ms: T0,
                    payload: "{}".into(),
                },
                T0,
            )
            .unwrap(),
        InboundRecordOutcome::Accepted { .. }
    )
}

// ---------------------------------------------------------------------------
// Immediate owner turns
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_dm_begins_handling_within_one_second_and_the_answer_is_posted() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));

    let mut server = Server::accept(&mut servers).await;
    eventually("connected", || {
        h.health_state().as_deref() == Some("connected")
    })
    .await;

    let sent_at = Instant::now();
    server
        .send(&owner_dm("env-1", "what is on today?", "1700000000.000100"))
        .await;
    assert_eq!(server.ack().await, "env-1");
    // Acked means persisted: the durable log already has this message.
    assert!(!not_persisted_yet(
        &h.store,
        &format!("{DM}:1700000000.000100")
    ));

    eventually("handler called", || handler.calls() == 1).await;
    let started = handler.turns.lock().unwrap()[0].1;
    assert!(
        started.duration_since(sent_at) < Duration::from_secs(1),
        "turn began {:?} after local receipt",
        started.duration_since(sent_at)
    );
    let turn = handler.turns.lock().unwrap()[0].0.clone();
    assert_eq!(turn.text, "what is on today?");
    assert_eq!(turn.owner.sender_id(), OWNER);
    assert_eq!(turn.event_id, format!("{DM}:1700000000.000100"));
    assert_eq!(turn.attempt, 1);

    eventually("answer posted", || h.posts().len() == 1).await;
    let post = &h.posts()[0];
    assert_eq!(post.channel, DM);
    assert_eq!(post.text, "answer: what is on today?");
    assert_eq!(post.thread_ts, None, "a top-level DM is answered top-level");
    // Delivered by the shared outbox dispatcher (#1294), which tags every
    // post with its idempotency key so an uncertain send can be reconciled.
    assert_eq!(
        post.metadata.as_ref().unwrap()["event_payload"]["idempotency_key"],
        json!(format!("turn:{DM}:1700000000.000100:text:0"))
    );
    eventually("event handled", || {
        h.inbound_status(&format!("{DM}:1700000000.000100")) == "handled"
    })
    .await;

    let health = h
        .store
        .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(health.state, "connected");
    assert!(health.last_event_at_ms.is_some());
    eventually("last send recorded", || {
        h.store
            .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
            .unwrap()
            .unwrap()
            .last_send_at_ms
            .is_some()
    })
    .await;
    running.stop().await;
    assert_eq!(h.health_state().as_deref(), Some("stopped"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn control_channel_message_is_answered_in_its_thread() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));
    let mut server = Server::accept(&mut servers).await;

    server
        .send(&message(
            "env-1",
            CONTROL,
            "group",
            OWNER,
            "status?",
            "1700000000.000200",
        ))
        .await;
    server.ack().await;
    eventually("answer posted", || h.posts().len() == 1).await;
    let post = &h.posts()[0];
    assert_eq!(post.channel, CONTROL);
    assert_eq!(post.thread_ts.as_deref(), Some("1700000000.000200"));
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redelivered_envelope_produces_one_turn_and_one_answer() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));
    let mut server = Server::accept(&mut servers).await;

    let first = owner_dm("env-1", "once please", "1700000000.000100");
    server.send(&first).await;
    assert_eq!(server.ack().await, "env-1");
    // Slack redelivers under a new envelope id with a retry marker.
    let mut again = owner_dm("env-2", "once please", "1700000000.000100");
    again["retry_attempt"] = json!(1);
    again["retry_reason"] = json!("timeout");
    server.send(&again).await;
    assert_eq!(server.ack().await, "env-2", "a duplicate is still acked");

    eventually("answer posted", || h.posts().len() == 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handler.calls(), 1);
    assert_eq!(h.posts().len(), 1);
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_owner_gets_one_rejection_reply_and_never_reaches_the_handler() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));
    let mut server = Server::accept(&mut servers).await;

    // A stranger DMs the app: the reply goes back in that DM.
    server
        .send(&message(
            "env-1",
            STRANGER_DM,
            "im",
            STRANGER,
            "give me the owner's mail",
            "1700000000.000300",
        ))
        .await;
    server.ack().await;
    // A stranger in the private control channel: ephemeral, only they see it.
    server
        .send(&message(
            "env-2",
            CONTROL,
            "group",
            STRANGER,
            "me too",
            "1700000000.000400",
        ))
        .await;
    server.ack().await;

    eventually("rejection DM", || h.posts().len() == 1).await;
    eventually("rejection ephemeral", || h.ephemerals().len() == 1).await;
    let post = &h.posts()[0];
    assert_eq!(
        (post.channel.as_str(), post.text.as_str()),
        (STRANGER_DM, REJECTION_REPLY)
    );
    let eph = &h.ephemerals()[0];
    assert_eq!(
        (eph.channel.as_str(), eph.user.as_str(), eph.text.as_str()),
        (CONTROL, STRANGER, REJECTION_REPLY)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handler.calls(), 0, "no rejected input reaches the handler");
    assert_eq!(h.posts().len() + h.ephemerals().len(), 2);
    assert_eq!(
        h.store
            .surface_auth_rejection_count(&workspace().account())
            .unwrap(),
        2
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_agents_own_echo_is_acked_and_ignored() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));
    let mut server = Server::accept(&mut servers).await;

    let mut echo = message(
        "env-1",
        DM,
        "im",
        BOT_USER,
        "answer: hi",
        "1700000000.000500",
    );
    echo["payload"]["event"]["bot_id"] = json!("B00000001");
    server.send(&echo).await;
    server.ack().await;
    eventually("event settled", || {
        h.inbound_status(&format!("{DM}:1700000000.000500")) == "handled"
    })
    .await;
    assert_eq!(handler.calls(), 0);
    assert!(h.web.calls().is_empty());
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Failure, shutdown and restart
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_turn_tells_the_owner_without_leaking_the_error() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    handler.fail.store(true, Ordering::SeqCst);
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));
    let mut server = Server::accept(&mut servers).await;

    server
        .send(&owner_dm("env-1", "boom", "1700000000.000100"))
        .await;
    server.ack().await;
    eventually("failure notice", || h.posts().len() == 1).await;
    let text = &h.posts()[0].text;
    assert!(
        !text.contains("internal detail"),
        "error detail leaked: {text}"
    );
    assert!(text.to_lowercase().contains("went wrong"), "{text}");
    assert_eq!(
        h.inbound_status(&format!("{DM}:1700000000.000100")),
        "handled"
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_a_pending_turn_stops_cleanly_and_leaves_the_event_claimable() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = FakeHandler::holding();
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));
    let mut server = Server::accept(&mut servers).await;

    server
        .send(&owner_dm("env-1", "slow one", "1700000000.000100"))
        .await;
    server.ack().await;
    eventually("turn pending", || handler.calls() == 1).await;
    let event_id = format!("{DM}:1700000000.000100");
    assert_eq!(h.inbound_status(&event_id), "claimed");

    running.stop().await;
    assert!(h.web.calls().is_empty(), "no answer for an unfinished turn");
    assert_eq!(
        h.inbound_status(&event_id),
        "received",
        "released for the next run"
    );
    assert_eq!(h.health_state().as_deref(), Some("stopped"));
    let again = h
        .store
        .claim_next_inbound_event_for(&SurfacePlatform::new("slack").unwrap(), T0, 5)
        .unwrap()
        .expect("still claimable");
    assert_eq!(again.event_id, event_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_mid_turn_recovers_the_event_exactly_once() {
    let h = Harness::new();
    let event_id = format!("{DM}:1700000000.000100");

    // First daemon: the turn is in flight when the process dies (the task is
    // aborted, so nothing gets to release the claim).
    {
        let (connector, mut servers) = DuplexConnector::new();
        let handler = FakeHandler::holding();
        let surface = h.surface(h.reopen(), connector, handler.clone(), false);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(surface.run(shutdown.clone()));
        let mut server = Server::accept(&mut servers).await;
        server
            .send(&owner_dm("env-1", "survive a crash", "1700000000.000100"))
            .await;
        server.ack().await;
        eventually("turn pending", || handler.calls() == 1).await;
        task.abort();
        let _ = task.await;
        assert_eq!(h.inbound_status(&event_id), "claimed");
    }

    // Second daemon: recovery replays the event once.
    let handler = Arc::new(FakeHandler::default());
    {
        let (connector, mut servers) = DuplexConnector::new();
        let running = start(h.surface(h.reopen(), connector, handler.clone(), false));
        let _server = Server::accept(&mut servers).await;
        eventually("recovered answer", || h.posts().len() == 1).await;
        running.stop().await;
    }
    assert_eq!(handler.calls(), 1);
    assert_eq!(handler.turns.lock().unwrap()[0].0.attempt, 2);
    assert_eq!(handler.texts(), vec!["survive a crash".to_string()]);

    // Third daemon: nothing left to do.
    let later = Arc::new(FakeHandler::default());
    {
        let (connector, mut servers) = DuplexConnector::new();
        let running = start(h.surface(h.reopen(), connector, later.clone(), false));
        let _server = Server::accept(&mut servers).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        running.stop().await;
    }
    assert_eq!(later.calls(), 0);
    assert_eq!(h.posts().len(), 1);
    assert_eq!(h.inbound_status(&event_id), "handled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_after_the_answer_was_queued_sends_it_without_a_second_turn() {
    let h = Harness::new();
    let event_id = format!("{DM}:1700000000.000100");
    // Simulate a crash between queuing the answer and marking the event
    // handled: run once in dry-run to produce the rows, then rewind them.
    {
        let (connector, mut servers) = DuplexConnector::new();
        let handler = FakeHandler::holding();
        let running = start(h.surface(h.reopen(), connector, handler.clone(), false));
        let mut server = Server::accept(&mut servers).await;
        server
            .send(&owner_dm(
                "env-1",
                "queued before the crash",
                "1700000000.000100",
            ))
            .await;
        server.ack().await;
        eventually("turn pending", || handler.calls() == 1).await;
        running.stop().await;
    }
    // The event is back to `received`; queue its answer by hand as the
    // crashed run would have, before it could mark the event handled.
    {
        let conn = rusqlite::Connection::open(&h.path).unwrap();
        conn.execute(
            "INSERT INTO surface_outbox (platform, account_id, conversation_id, thread_id,
                idempotency_key, operation, payload, max_attempts, next_attempt_at_ms,
                created_at_ms, updated_at_ms)
             VALUES ('slack', 'team:T00000001', ?1, '', ?2, 'post', ?3, 5, 0, 0, 0)",
            rusqlite::params![
                DM,
                // The shared outbox dispatcher's key and payload (#1294).
                format!("turn:{event_id}:text:0"),
                json!({"text": "answer: queued before the crash", "part": 1, "parts": 1})
                    .to_string()
            ],
        )
        .unwrap();
    }
    let handler = Arc::new(FakeHandler::default());
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(h.reopen(), connector, handler.clone(), false));
    let _server = Server::accept(&mut servers).await;
    eventually("queued answer sent", || h.posts().len() == 1).await;
    eventually("event handled", || h.inbound_status(&event_id) == "handled").await;
    running.stop().await;
    assert_eq!(
        handler.calls(),
        0,
        "the answer already existed; no second turn"
    );
    assert_eq!(h.posts()[0].text, "answer: queued before the crash");
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_makes_zero_live_sends_and_records_them_as_dry_run() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), true));
    let mut server = Server::accept(&mut servers).await;

    server
        .send(&owner_dm("env-1", "dry question", "1700000000.000100"))
        .await;
    server.ack().await;
    server
        .send(&message(
            "env-2",
            STRANGER_DM,
            "im",
            STRANGER,
            "hi",
            "1700000000.000200",
        ))
        .await;
    server.ack().await;
    eventually("both sends settled", || {
        let rows = h.outbox();
        rows.len() == 2 && rows.iter().all(|(_, status, _)| status == "sent")
    })
    .await;
    running.stop().await;

    assert_eq!(handler.calls(), 1, "the turn still runs in dry-run");
    assert!(
        h.web.calls().iter().all(|c| !matches!(
            c,
            RecordedCall::PostMessage(_) | RecordedCall::PostEphemeral(_)
        )),
        "dry-run posted live: {:?}",
        h.web.calls()
    );
    for (key, _, provider_id) in h.outbox() {
        assert!(
            provider_id
                .as_deref()
                .is_some_and(|p| p.starts_with("dry-run:")),
            "{key} recorded as {provider_id:?}"
        );
    }
    let health = h
        .store
        .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
        .unwrap()
        .unwrap();
    assert!(health.dry_run);
}

// ---------------------------------------------------------------------------
// Listener states
// ---------------------------------------------------------------------------

#[test]
fn not_configured_and_disabled_are_reported_with_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("data.db")).unwrap();
    report_inactive(
        &store,
        SurfaceState::NotConfigured,
        "no Slack app is installed",
        Some("Install it: augmentagent slack app install --stdin"),
        &[],
        false,
        T0,
    )
    .unwrap();
    let h = store
        .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(h.state, "not_configured");
    assert!(h.recovery.unwrap().contains("slack app install"));
    assert!(!SurfaceState::NotConfigured.is_healthy());

    report_inactive(
        &store,
        SurfaceState::Disabled,
        "disabled by AUGMENTAGENT_SLACK_INTERACTIVE=0",
        None,
        &[TEAM.to_string()],
        false,
        T0 + 1,
    )
    .unwrap();
    let h = store
        .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(h.state, "disabled");
    assert_eq!(h.workspaces, vec![TEAM.to_string()]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connecting_connected_reconnecting_and_stopped_are_live_states() {
    let h = Harness::new();
    let (connector, mut servers) = DuplexConnector::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(Arc::clone(&h.store), connector, handler.clone(), false));

    // Connected only after Slack's hello, not when the socket opens.
    let ws = tokio::time::timeout(Duration::from_secs(5), servers.recv())
        .await
        .unwrap()
        .unwrap();
    eventually("connecting reported", || {
        h.health_state().as_deref() == Some("connecting")
    })
    .await;
    let mut server = Server { ws };
    server.ws.send(hello()).await.unwrap();
    eventually("connected", || {
        h.health_state().as_deref() == Some("connected")
    })
    .await;
    assert!(SurfaceState::Connected.is_healthy());

    // The fake Slack drops the socket: reconnecting, never healthy, and the
    // operator is told what the daemon is doing about it.
    drop(server);
    eventually("reconnecting", || {
        h.health_state().as_deref() == Some("reconnecting")
    })
    .await;
    let report = h
        .store
        .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
        .unwrap()
        .unwrap();
    assert!(
        report
            .recovery
            .as_deref()
            .is_some_and(|r| r.contains("retries")),
        "{report:?}"
    );
    // It comes back.
    let _server = Server::accept(&mut servers).await;
    eventually("connected again", || {
        h.health_state().as_deref() == Some("connected")
    })
    .await;
    running.stop().await;
    assert_eq!(h.health_state().as_deref(), Some("stopped"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_app_token_is_disconnected_with_recovery_and_does_not_end_the_surface() {
    let h = Harness::new();
    let handler = Arc::new(FakeHandler::default());
    let running = start(h.surface(
        Arc::clone(&h.store),
        DuplexConnector::fatal(),
        handler.clone(),
        false,
    ));
    eventually("disconnected", || {
        h.health_state().as_deref() == Some("disconnected")
    })
    .await;
    let health = h
        .store
        .surface_listener_health(&SurfacePlatform::new("slack").unwrap())
        .unwrap()
        .unwrap();
    assert!(health.detail.unwrap().contains("invalid_auth"));
    let recovery = health.recovery.unwrap();
    assert!(recovery.contains("slack app rotate"), "{recovery}");
    assert!(
        !running.task.is_finished(),
        "a dead listener must not end the surface"
    );
    running.stop().await;
}

#[test]
fn surface_state_words_are_stable() {
    let words: Vec<&str> = [
        SurfaceState::NotConfigured,
        SurfaceState::Disabled,
        SurfaceState::Misconfigured,
        SurfaceState::Connecting,
        SurfaceState::Connected,
        SurfaceState::Reconnecting,
        SurfaceState::Disconnected,
        SurfaceState::Stopped,
    ]
    .iter()
    .map(|s| s.as_str())
    .collect();
    assert_eq!(
        words,
        [
            "not_configured",
            "disabled",
            "misconfigured",
            "connecting",
            "connected",
            "reconnecting",
            "disconnected",
            "stopped"
        ]
    );
    for w in words {
        assert_eq!(SurfaceState::parse(w).map(|s| s.as_str()), Some(w));
    }
}
