//! #1288 — Slack owner turns through the shared agent harness, on the real
//! dispatch path: Socket Mode client → durable inbound log → owner gate →
//! inbound files → [`SlackConversationHarness`] → shared native-session turn
//! (`augmentagent_channel_core::surface_turn`) → durable outbox → Web API.
//!
//! The socket is an in-memory duplex WebSocket, the Web API is
//! `RecordingSlackWebApi`, the store is a temporary file (spaces and
//! non-ASCII in its path), and the agent is a fake query handler whose
//! provider joins the native session like the Claude CLI adapter does and
//! answers `session=<id> turn=<n>`. The dispatcher's fallback poll is an
//! hour, so everything here is event-driven. Identifiers are synthetic.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use augmentagent_approval_discord::{AuditCtx, QueryHandler};
use augmentagent_channel_core::providers::ProviderKind;
use augmentagent_channel_core::surface_conformance::{
    native_session_conformance, ConformanceAdapter, RecordingNativeProvider,
};
use augmentagent_channel_core::surface_turn::turn_env;
use augmentagent_channel_slack::delivery::ProgressConfig;
use augmentagent_channel_slack::harness::{
    SlackConversationHarness, CANCELLED_REPLY, INTERRUPTED_REPLY, SLACK_INBOUND_DIR_ENV,
};
use augmentagent_channel_slack::inbound::InboundOptions;
use augmentagent_channel_slack::interactive::{
    slack_turn_id, SlackInteractiveSurface, SlackSurfaceConfig, SlackTurn, SlackTurnHandler,
    SlackWorkspaceRuntime, NOTHING_TO_CANCEL_REPLY,
};
use augmentagent_channel_slack::owner::{OwnerInputSource, SlackBotIdentity};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::event::{parse_envelope, Envelope};
use augmentagent_channel_slack::transport::socket::{
    AsyncIo, BoxedWebSocket, ConnectError, SocketConnector, SocketModeConfig,
};
use augmentagent_channel_slack::transport::web::{
    PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi,
};
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::{
    Store, SurfaceConversationRef, SurfaceTurnRef, SurfaceTurnResolution, SurfaceTurnStatus,
};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const BOT_USER: &str = "U0000000B";
const DM: &str = "D00000001";
const CONTROL: &str = "C00000001";
const T0: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

type ServerSocket = WebSocketStream<DuplexStream>;

struct DuplexConnector {
    servers: mpsc::UnboundedSender<ServerSocket>,
}

impl DuplexConnector {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<ServerSocket>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(Self { servers: tx }), rx)
    }
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

/// What the agent saw of one turn.
#[derive(Debug, Clone)]
struct AgentCall {
    audit_session_id: String,
    owner_authorized: bool,
    discord_context: bool,
    /// For a turn with files: (Codex bridge allowance granted, real scope
    /// guard allows Read of each attached text file), checked mid-turn.
    file_access: Option<(bool, Vec<bool>)>,
}

/// Run the real `scripts/aa-wiki-scope-guard.sh` on a Read of `path` with
/// the turn's environment. `None` when `jq` is missing.
fn guard_allows_read(path: &str, env: &[(String, String)]) -> Option<bool> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    Command::new("jq").arg("--version").output().ok()?;
    let guard = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../scripts/aa-wiki-scope-guard.sh"
    );
    let wiki = tempfile::tempdir().unwrap();
    let mut command = Command::new("bash");
    command
        .arg(guard)
        .env_clear()
        .env("WIKI_ROOT", wiki.path())
        .env("PATH", std::env::var_os("PATH").unwrap())
        .envs(env.iter().map(|(k, v)| (k, v)));
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"tool_name": "Read", "tool_input": {"file_path": path}})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    Some(output.status.success() && !String::from_utf8_lossy(&output.stdout).contains("\"block\""))
}

/// The shared agent seam (`QueryHandler::answer`, what Discord's
/// `WikiQuerier` implements) with the recording native provider behind it.
struct FakeAgent {
    provider: Arc<RecordingNativeProvider>,
    seen: Mutex<Vec<AgentCall>>,
}

impl FakeAgent {
    fn new(provider: Arc<RecordingNativeProvider>) -> Arc<Self> {
        Arc::new(Self {
            provider,
            seen: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl QueryHandler for FakeAgent {
    async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
        let env = turn_env();
        let file_access = env
            .iter()
            .find(|(k, _)| k == SLACK_INBOUND_DIR_ENV)
            .map(|(_, dir)| {
                let codex =
                    augmentagent_channel_core::codex_tools::slack_inbound_dir(dir).is_some();
                let guard = question
                    .lines()
                    .filter_map(|line| line.strip_prefix("- "))
                    .map(|path| {
                        guard_allows_read(path.split("  (").next().unwrap(), &env).unwrap_or(true)
                    })
                    .collect();
                (codex, guard)
            });
        self.seen.lock().unwrap().push(AgentCall {
            audit_session_id: ctx.session_id.clone(),
            owner_authorized: ctx.owner_authorized,
            discord_context: ctx.http.is_some()
                || ctx.channel_id.is_some()
                || ctx.guild_id.is_some(),
            file_access,
        });
        let mut opts = augmentagent_channel_core::reasoner::triage_opts(None);
        opts.session_id = Some(ctx.session_id.clone());
        self.provider.answer(&opts, question, turn_env()).await
    }
}

fn harness_for(
    store: &Arc<Store>,
    agent: Arc<FakeAgent>,
    wiki: &Path,
) -> Arc<SlackConversationHarness> {
    Arc::new(
        SlackConversationHarness::new(Arc::clone(store), agent, wiki.to_path_buf())
            .with_selection(Arc::new(|_| Ok(Some(ProviderKind::Claude)))),
    )
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Harness {
    dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
    wiki: PathBuf,
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

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("state dir \u{fc}");
        let path = root.join("data.db");
        std::fs::create_dir_all(&root).unwrap();
        let wiki = root.join("wiki");
        std::fs::create_dir_all(&wiki).unwrap();
        let store = Arc::new(Store::open(&path).unwrap());
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
        Harness {
            dir,
            path,
            store,
            web: Arc::new(RecordingSlackWebApi::default()),
            wiki,
        }
    }

    fn reopen(&self) -> Arc<Store> {
        Arc::new(Store::open(&self.path).unwrap())
    }

    fn inbound_root(&self) -> PathBuf {
        self.dir
            .path()
            .join("state dir \u{fc}")
            .join("slack-inbound")
    }

    fn config(&self, dry_run: bool, progress: bool) -> SlackSurfaceConfig {
        SlackSurfaceConfig {
            dry_run,
            idle_poll: Duration::from_secs(3600),
            heartbeat: Duration::from_secs(3600),
            socket: SocketModeConfig {
                backoff: BackoffConfig {
                    initial: Duration::from_millis(50),
                    max: Duration::from_millis(200),
                },
                ..SocketModeConfig::default()
            },
            inbound: Some(InboundOptions::new(self.inbound_root())),
            progress: progress.then(|| ProgressConfig {
                min_interval: Duration::from_millis(10),
                ..ProgressConfig::default()
            }),
            ..SlackSurfaceConfig::default()
        }
    }

    fn surface(
        &self,
        store: Arc<Store>,
        connector: Arc<dyn SocketConnector>,
        handler: Arc<dyn SlackTurnHandler>,
        config: SlackSurfaceConfig,
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
            config,
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

    /// Answers (outbox posts, which carry an idempotency key) in one place.
    fn answers(&self, channel: &str, thread: Option<&str>) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|p| p.metadata.is_some())
            .filter(|p| p.channel == channel && p.thread_ts.as_deref() == thread)
            .map(|p| p.text)
            .collect()
    }

    fn live_calls(&self) -> Vec<RecordedCall> {
        self.web
            .calls()
            .into_iter()
            .filter(|c| {
                matches!(
                    c,
                    RecordedCall::PostMessage(_)
                        | RecordedCall::PostEphemeral(_)
                        | RecordedCall::UpdateMessage(_)
                )
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

fn frame(envelope_id: &str, event: Value) -> Value {
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

fn channel_message(envelope_id: &str, text: &str, ts: &str) -> Value {
    frame(
        envelope_id,
        json!({"type": "message", "channel": CONTROL, "channel_type": "group",
            "user": OWNER, "text": text, "ts": ts}),
    )
}

fn thread_reply(envelope_id: &str, channel: &str, text: &str, ts: &str, thread: &str) -> Value {
    let kind = if channel.starts_with('D') {
        "im"
    } else {
        "group"
    };
    frame(
        envelope_id,
        json!({"type": "message", "channel": channel, "channel_type": kind,
            "user": OWNER, "text": text, "ts": ts, "thread_ts": thread}),
    )
}

fn owner_dm(envelope_id: &str, text: &str, ts: &str) -> Value {
    frame(
        envelope_id,
        json!({"type": "message", "channel": DM, "channel_type": "im",
            "user": OWNER, "text": text, "ts": ts}),
    )
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

fn thread_turn(channel: &str, thread: Option<&str>, event_ts: &str) -> SurfaceTurnRef {
    let conversation = workspace().conversation(channel, thread).unwrap();
    let event_id = format!("{channel}:{event_ts}");
    SurfaceTurnRef::new(
        conversation,
        slack_turn_id(&workspace().account(), &event_id),
    )
    .unwrap()
}

fn session_of(answer: &str) -> String {
    answer
        .split_whitespace()
        .find_map(|w| w.strip_prefix("session="))
        .unwrap_or_else(|| panic!("no session in {answer:?}"))
        .to_string()
}

// ---------------------------------------------------------------------------
// Conversation mapping and native session continuity
// ---------------------------------------------------------------------------

const A: &str = "1700000100.000100";
const B: &str = "1700000200.000100";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_follow_up_continues_its_native_session_and_a_new_thread_starts_another() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let agent = FakeAgent::new(Arc::clone(&provider));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, agent.clone(), &h.wiki),
        h.config(false, false),
    ));
    let mut server = Server::accept(&mut servers).await;

    // A control-channel top-level message starts thread A...
    server
        .deliver(&channel_message("e1", "plan my week", A))
        .await;
    eventually("A answered", || h.answers(CONTROL, Some(A)).len() == 1).await;
    // ...a reply in thread A continues it...
    server
        .deliver(&thread_reply(
            "e2",
            CONTROL,
            "and friday?",
            "1700000100.000200",
            A,
        ))
        .await;
    eventually("A follow-up answered", || {
        h.answers(CONTROL, Some(A)).len() == 2
    })
    .await;
    // ...and another top-level message is a different conversation.
    server.deliver(&channel_message("e3", "unrelated", B)).await;
    eventually("B answered", || h.answers(CONTROL, Some(B)).len() == 1).await;

    let a = h.answers(CONTROL, Some(A));
    let b = h.answers(CONTROL, Some(B));
    assert_eq!(session_of(&a[0]), session_of(&a[1]));
    assert!(
        a[0].ends_with("turn=1") && a[1].ends_with("turn=2"),
        "{a:?}"
    );
    assert_ne!(session_of(&a[0]), session_of(&b[0]));
    assert!(b[0].ends_with("turn=1"), "{b:?}");
    let calls = provider.calls();
    assert!(!calls[0].resumed && calls[1].resumed && !calls[2].resumed);

    // The binding is keyed by the Slack thread; the turn claims completed.
    let thread_a = workspace().conversation(CONTROL, Some(A)).unwrap();
    assert_eq!(
        h.store
            .surface_conversation(&thread_a)
            .unwrap()
            .unwrap()
            .native_session_id,
        session_of(&a[0])
    );
    assert_eq!(
        h.store
            .surface_turn_state(&thread_turn(CONTROL, Some(A), "1700000100.000200"))
            .unwrap()
            .unwrap()
            .status,
        SurfaceTurnStatus::Complete
    );

    // Owner authority from `owner::admit`, no Discord context, and a
    // globally namespaced per-turn audit ID.
    let seen = agent.seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|c| c.owner_authorized && !c.discord_context),
        "{seen:?}"
    );
    assert_eq!(
        seen[0].audit_session_id,
        format!("slack:{TEAM}:{CONTROL}:{A}")
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_dm_is_one_conversation_and_a_dm_thread_is_its_own() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
        h.config(false, false),
    ));
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&owner_dm("e1", "hello", "1700000300.000100"))
        .await;
    eventually("first", || h.answers(DM, None).len() == 1).await;
    server
        .deliver(&owner_dm("e2", "again", "1700000300.000200"))
        .await;
    eventually("second", || h.answers(DM, None).len() == 2).await;
    server
        .deliver(&thread_reply(
            "e3",
            DM,
            "side topic",
            "1700000300.000300",
            "1700000300.000100",
        ))
        .await;
    eventually("dm thread", || {
        h.answers(DM, Some("1700000300.000100")).len() == 1
    })
    .await;
    let dm = h.answers(DM, None);
    let side = h.answers(DM, Some("1700000300.000100"));
    assert_eq!(session_of(&dm[0]), session_of(&dm[1]));
    assert_ne!(session_of(&dm[0]), session_of(&side[0]));
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_threads_run_at_once_without_sharing_context() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("slow"));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
        h.config(false, false),
    ));
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&channel_message("e1", "slow research", A))
        .await;
    eventually("A running", || provider.observed_ids().len() == 1).await;
    server.deliver(&channel_message("e2", "quick one", B)).await;
    eventually("B answered while A runs", || {
        h.answers(CONTROL, Some(B)).len() == 1
    })
    .await;
    assert!(h.answers(CONTROL, Some(A)).is_empty());
    provider.release.notify_one();
    eventually("A answered", || h.answers(CONTROL, Some(A)).len() == 1).await;
    assert_ne!(
        session_of(&h.answers(CONTROL, Some(A))[0]),
        session_of(&h.answers(CONTROL, Some(B))[0])
    );
    let calls = provider.calls();
    assert!(calls.iter().all(|c| !c.resumed));
    assert!(!calls
        .iter()
        .any(|c| c.prompt.contains("slow") && c.prompt.contains("quick")));
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Queueing and cancellation
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_during_a_running_turn_waits_and_then_continues_the_session() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("slow"));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
        h.config(false, false),
    ));
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&channel_message("e1", "slow first", A))
        .await;
    eventually("first running", || provider.observed_ids().len() == 1).await;
    server
        .deliver(&thread_reply(
            "e2",
            CONTROL,
            "then this",
            "1700000100.000200",
            A,
        ))
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        provider.observed_ids().len(),
        1,
        "queued behind the running turn"
    );
    assert_eq!(
        h.inbound_status(&format!("{CONTROL}:1700000100.000200")),
        "received"
    );
    provider.release.notify_one();
    eventually("both answered", || h.answers(CONTROL, Some(A)).len() == 2).await;
    let a = h.answers(CONTROL, Some(A));
    assert!(
        a[0].ends_with("turn=1") && a[1].ends_with("turn=2"),
        "{a:?}"
    );
    assert_eq!(session_of(&a[0]), session_of(&a[1]));
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_stops_the_running_turn_reports_it_and_the_thread_continues() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("slow"));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
        h.config(false, true),
    ));
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&channel_message("e1", "slow report", A))
        .await;
    eventually("running", || provider.observed_ids().len() == 1).await;
    // The status line says how to stop it.
    eventually("progress posted", || {
        h.posts()
            .iter()
            .any(|p| p.metadata.is_none() && p.thread_ts.as_deref() == Some(A))
    })
    .await;
    let progress = h
        .posts()
        .into_iter()
        .find(|p| p.metadata.is_none() && p.thread_ts.as_deref() == Some(A))
        .unwrap();
    assert!(progress.text.contains("cancel"), "{}", progress.text);

    // `cancel` in the same thread is handled at once, not queued behind it.
    let sent = Instant::now();
    server
        .deliver(&thread_reply(
            "e2",
            CONTROL,
            "cancel",
            "1700000100.000200",
            A,
        ))
        .await;
    eventually("cancel reported", || {
        h.answers(CONTROL, Some(A))
            .iter()
            .any(|t| t == CANCELLED_REPLY)
    })
    .await;
    assert!(sent.elapsed() < Duration::from_secs(2));
    assert_eq!(
        provider.dropped_mid_turn(),
        1,
        "the provider call was torn down"
    );
    assert!(provider.calls().is_empty());
    let state = h
        .store
        .surface_turn_state(&thread_turn(CONTROL, Some(A), A))
        .unwrap()
        .unwrap();
    assert_eq!(state.resolution, Some(SurfaceTurnResolution::Cancelled));
    eventually("progress says stopped", || {
        h.web.calls().iter().any(|c| {
            matches!(c, RecordedCall::UpdateMessage(u) if u.text.to_lowercase().contains("stopped"))
        })
    })
    .await;

    // The next message in the thread continues the same session.
    server
        .deliver(&thread_reply(
            "e3",
            CONTROL,
            "shorter version please",
            "1700000100.000300",
            A,
        ))
        .await;
    eventually("continued", || {
        h.answers(CONTROL, Some(A))
            .iter()
            .any(|t| t.starts_with("session="))
    })
    .await;
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].resumed);
    assert_eq!(calls[0].native_session_id, provider.observed_ids()[0]);

    // Nothing running: `stop` says so and how cancel works.
    server
        .deliver(&owner_dm("e4", "stop", "1700000400.000100"))
        .await;
    eventually("nothing to cancel", || {
        h.answers(DM, None)
            .iter()
            .any(|t| t == NOTHING_TO_CANCEL_REPLY)
    })
    .await;
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Redelivery and restart
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redelivered_event_never_starts_a_second_turn_even_across_restart() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let msg = owner_dm("e1", "only once", "1700000500.000100");
    for round in 0..2 {
        let (connector, mut servers) = DuplexConnector::new();
        let running = start(h.surface(
            h.reopen(),
            connector,
            harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
            h.config(false, false),
        ));
        let mut server = Server::accept(&mut servers).await;
        let mut again = msg.clone();
        again["envelope_id"] = json!(format!("e1-retry-{round}"));
        again["retry_attempt"] = json!(round + 1);
        server.deliver(&msg).await;
        server.deliver(&again).await;
        eventually("answered", || h.answers(DM, None).len() == 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        running.stop().await;
    }
    assert_eq!(provider.calls().len(), 1);
    assert_eq!(h.answers(DM, None).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_mid_turn_reports_the_interruption_once_and_the_thread_keeps_its_session() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude).holding_on("slow"));
    let second = "1700000100.000200";
    let first_session;
    // First daemon: turn 1 completes, turn 2 is mid-flight when it dies.
    {
        let (connector, mut servers) = DuplexConnector::new();
        let surface = h.surface(
            h.reopen(),
            connector,
            harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
            h.config(false, false),
        );
        let task = tokio::spawn(surface.run(CancellationToken::new()));
        let mut server = Server::accept(&mut servers).await;
        server
            .deliver(&channel_message("e1", "draft the plan", A))
            .await;
        eventually("turn 1", || h.answers(CONTROL, Some(A)).len() == 1).await;
        first_session = session_of(&h.answers(CONTROL, Some(A))[0]);
        server
            .deliver(&thread_reply(
                "e2",
                CONTROL,
                "slow: now expand it",
                second,
                A,
            ))
            .await;
        eventually("turn 2 running", || provider.observed_ids().len() == 2).await;
        task.abort();
        let _ = task.await;
    }
    // The claim was persisted before the agent ran: the store says so.
    let interrupted = thread_turn(CONTROL, Some(A), second);
    assert_eq!(
        h.store
            .surface_turn_state(&interrupted)
            .unwrap()
            .unwrap()
            .status,
        SurfaceTurnStatus::Pending
    );
    assert_eq!(h.inbound_status(&format!("{CONTROL}:{second}")), "claimed");

    // Second daemon: the interrupted turn is reported, not re-run.
    {
        let (connector, mut servers) = DuplexConnector::new();
        let running = start(h.surface(
            h.reopen(),
            connector,
            harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
            h.config(false, false),
        ));
        let mut server = Server::accept(&mut servers).await;
        eventually("recovery message", || {
            h.answers(CONTROL, Some(A))
                .iter()
                .any(|t| t == INTERRUPTED_REPLY)
        })
        .await;
        assert_eq!(provider.observed_ids().len(), 2, "not re-run after restart");
        let state = h.store.surface_turn_state(&interrupted).unwrap().unwrap();
        assert_eq!(state.resolution, Some(SurfaceTurnResolution::Interrupted));
        // The thread continues the same native session.
        server
            .deliver(&thread_reply(
                "e3",
                CONTROL,
                "where were we?",
                "1700000100.000300",
                A,
            ))
            .await;
        eventually("continued", || h.answers(CONTROL, Some(A)).len() == 3).await;
        running.stop().await;
    }
    let last = h.answers(CONTROL, Some(A)).pop().unwrap();
    assert_eq!(session_of(&last), first_session);
    assert!(provider.calls().last().unwrap().resumed);

    // Third daemon: nothing left to do.
    let before = h.posts().len();
    {
        let (connector, mut servers) = DuplexConnector::new();
        let running = start(h.surface(
            h.reopen(),
            connector,
            harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
            h.config(false, false),
        ));
        let _server = Server::accept(&mut servers).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        running.stop().await;
    }
    assert_eq!(h.posts().len(), before);
    assert_eq!(
        h.answers(CONTROL, Some(A))
            .iter()
            .filter(|t| *t == INTERRUPTED_REPLY)
            .count(),
        1
    );
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn file_url(id: &str, name: &str) -> String {
    format!("https://files.slack.com/files-pri/{TEAM}-{id}/download/{name}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_owner_file_reaches_the_agent_as_discords_attachment_input_and_is_readable_by_the_guard_path(
) {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    h.web.add_file(
        &file_url("F00000001", "notes.txt"),
        b"synthetic notes body\n".to_vec(),
    );
    let agent = FakeAgent::new(Arc::clone(&provider));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, agent.clone(), &h.wiki),
        h.config(false, false),
    ));
    let mut server = Server::accept(&mut servers).await;
    let event = json!({
        "type": "message", "subtype": "file_share", "channel": DM, "channel_type": "im",
        "user": OWNER, "text": "summarize this", "ts": "1700000600.000100",
        "files": [
            {"id": "F00000001", "name": "notes.txt", "mimetype": "text/plain", "size": 21,
             "mode": "hosted", "url_private_download": file_url("F00000001", "notes.txt")},
            {"id": "F00000002", "name": "tool.exe", "mimetype": "application/octet-stream",
             "size": 10, "mode": "hosted", "url_private_download": file_url("F00000002", "tool.exe")}
        ]
    });
    server.deliver(&frame("e1", event)).await;
    eventually("answered", || h.answers(DM, None).len() == 1).await;

    let call = provider.calls()[0].clone();
    let dir = call
        .env
        .iter()
        .find(|(k, _)| k == SLACK_INBOUND_DIR_ENV)
        .map(|(_, v)| PathBuf::from(v))
        .expect("the turn names its inbound dir");
    assert_eq!(
        dir.parent().unwrap(),
        h.inbound_root().canonicalize().unwrap()
    );
    assert!(dir
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("msg-"));
    let path = dir.join("00-notes.txt");
    // Exactly the shared (Discord) attachment prompt.
    let expected = augmentagent_docs::inbound::build_prompt(
        "summarize this",
        &[],
        &[augmentagent_docs::inbound::TextAttachment {
            path: path.clone(),
            truncated: false,
            original_size: 21,
            note: None,
        }],
    );
    assert_eq!(call.prompt, expected);
    // While the turn ran, both enforcement layers let the agent Read the
    // file: the Codex bridge allowance and the real Claude scope guard.
    let access = agent.seen.lock().unwrap()[0].file_access.clone();
    assert_eq!(access, Some((true, vec![true])), "{access:?}");
    // The refused file is reported with the answer; the accepted one is
    // removed once the turn is over.
    let answer = &h.answers(DM, None)[0];
    assert!(answer.contains("tool.exe"), "{answer}");
    eventually("cleaned up", || !dir.exists()).await;
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dry_run_runs_the_turn_but_makes_zero_live_sends_including_progress() {
    let h = Harness::new();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(
        Arc::clone(&h.store),
        connector,
        harness_for(&h.store, FakeAgent::new(Arc::clone(&provider)), &h.wiki),
        h.config(true, true),
    ));
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&owner_dm("e1", "dry question", "1700000700.000100"))
        .await;
    server
        .deliver(&owner_dm("e2", "cancel", "1700000700.000200"))
        .await;
    eventually("handled", || {
        h.inbound_status(&format!("{DM}:1700000700.000100")) == "handled"
            && h.inbound_status(&format!("{DM}:1700000700.000200")) == "handled"
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    running.stop().await;
    assert_eq!(provider.calls().len(), 1, "the turn still runs in dry-run");
    assert!(
        h.live_calls().is_empty(),
        "dry-run sent live: {:?}",
        h.live_calls()
    );
}

// ---------------------------------------------------------------------------
// Shared conformance (same scenario as Discord's conversation path)
// ---------------------------------------------------------------------------

/// Drives the Slack harness with the turns the dispatcher builds: a
/// conversation label is a control-channel thread, a turn label its message.
struct SlackAdapter {
    store_path: PathBuf,
    wiki: PathBuf,
    provider: Arc<RecordingNativeProvider>,
    harness: Mutex<Arc<SlackConversationHarness>>,
}

impl SlackAdapter {
    fn harness(&self) -> Arc<SlackConversationHarness> {
        let store = Arc::new(Store::open(&self.store_path).unwrap());
        Arc::new(
            SlackConversationHarness::new(
                store,
                FakeAgent::new(Arc::clone(&self.provider)),
                self.wiki.clone(),
            )
            .with_selection(Arc::new(|_| Ok(None))),
        )
    }
}

fn thread_ts(conversation: &str) -> &'static str {
    if conversation == "A" {
        "1700000800.000000"
    } else {
        "1700000900.000000"
    }
}

#[async_trait]
impl ConformanceAdapter for SlackAdapter {
    fn name(&self) -> &str {
        "slack"
    }

    async fn turn(&self, conversation: &str, turn: &str, text: &str) -> anyhow::Result<String> {
        let parent = thread_ts(conversation);
        let ts = format!("{}.00000{turn}", &parent[..10]);
        let event = json!({"type": "message", "channel": CONTROL, "channel_type": "group",
            "user": OWNER, "text": text, "ts": ts, "thread_ts": parent});
        let Ok(Envelope::Event(envelope)) = parse_envelope(&frame("c", event).to_string()) else {
            panic!("parse")
        };
        let event_id = format!("{CONTROL}:{ts}");
        let ws = workspace();
        let session: SurfaceConversationRef = ws.conversation(CONTROL, Some(parent)).unwrap();
        let turn = SlackTurn {
            event_id: event_id.clone(),
            attempt: 1,
            owner: ws.owner(OWNER).unwrap(),
            conversation: Some(session.clone()),
            source: OwnerInputSource::ThreadReply,
            text: text.into(),
            envelope: *envelope,
            session: Some(session),
            turn_id: slack_turn_id(&ws.account(), &event_id),
            prompt: text.into(),
            inbound_dir: None,
            cancel: CancellationToken::new(),
        };
        let harness = self.harness.lock().unwrap().clone();
        let reply = harness.handle_turn(&turn).await?;
        Ok(reply.map(|r| r.text).unwrap_or_default())
    }

    async fn restart(&self) {
        *self.harness.lock().unwrap() = self.harness();
    }
}

#[tokio::test]
async fn the_shared_session_conformance_scenario_passes_for_slack() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("state dir \u{fc}").join("data.db");
    std::fs::create_dir_all(store_path.parent().unwrap()).unwrap();
    let wiki = dir.path().join("wiki");
    std::fs::create_dir_all(&wiki).unwrap();
    let provider = Arc::new(RecordingNativeProvider::new(ProviderKind::Claude));
    let adapter = SlackAdapter {
        store_path: store_path.clone(),
        wiki: wiki.clone(),
        provider: Arc::clone(&provider),
        harness: Mutex::new(Arc::new(SlackConversationHarness::new(
            Arc::new(Store::open(&store_path).unwrap()),
            FakeAgent::new(Arc::clone(&provider)),
            wiki,
        ))),
    };
    adapter.restart().await;
    native_session_conformance(&adapter, &provider).await;
}
