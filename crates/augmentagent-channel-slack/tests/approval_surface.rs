//! #1289 — the approval workflow on Slack, end to end on the real dispatch
//! path: Socket Mode (in-memory WebSocket) → durable inbound log (persist,
//! then ack) → owner gate → approval surface → shared approval handler →
//! Web API (`RecordingSlackWebApi`), with a temporary store.
//!
//! The approval handler is a store-backed stand-in for the daemon's
//! `ReplyApprover`: the same store compare-and-swap transitions (claim for
//! send, resolve, refresh draft, recompose) and a fake reasoner for
//! redrafts; its "send" is a recorded call. Discord is a recording card
//! surface plus the same `SyncingActionHandler` the Discord bot is given,
//! so a "Discord click" is exactly the call Discord's event handler makes.
//! Identifiers, tokens and people are synthetic.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use augmentagent_approval_discord::{
    append_needs_input_marker, ApprovalActionHandler, ApprovalActionOutcome, ApprovalBroker,
    ApprovalCardSurface, CardSurfaces, SyncingActionHandler, MAX_REDRAFT_ITERATIONS, PRESETS,
};
use augmentagent_channel_slack::approvals::card::{self, ControlRef};
use augmentagent_channel_slack::approvals::{
    SlackApprovalConfig, SlackApprovals, REFINE_LIMIT_REPLY, REVISED_REPLY, STALE_DRAFT_REPLY,
};
use augmentagent_channel_slack::interactive::{
    SlackInteractiveSurface, SlackSurfaceConfig, SlackTurn, SlackTurnHandler, SlackTurnReply,
    SlackWorkspaceRuntime,
};
use augmentagent_channel_slack::owner::{SlackBotIdentity, REJECTION_REPLY};
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::socket::{
    AsyncIo, BoxedWebSocket, ConnectError, SocketConnector, SocketModeConfig,
};
use augmentagent_channel_slack::transport::web::{
    PostEphemeral, PostMessage, RecordedCall, RecordingSlackWebApi, SlackWebApi, UpdateMessage,
};
use augmentagent_store::approval_cards::ApprovalCardState;
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::{ActionStatus, Email, Store, SurfacePlatform};
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
const CONTROL: &str = "C00000001";
const T0: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Socket Mode fake (same pattern as interactive_surface.rs)
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
        s.ws.send(Message::Text(
            json!({"type": "hello", "connection_info": {"app_id": "A00000001"}, "num_connections": 1})
                .to_string(),
        ))
        .await
        .unwrap();
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

    /// Send an envelope and wait for its ack (acked = persisted).
    async fn deliver(&mut self, frame: &Value) {
        self.send(frame).await;
        let id = frame["envelope_id"].as_str().unwrap().to_string();
        assert_eq!(self.ack().await, id);
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

// ---------------------------------------------------------------------------
// The shared approval handler, store-backed
// ---------------------------------------------------------------------------

/// The daemon's handler semantics over the real store: every decision is a
/// compare-and-swap, so a lost race runs no side effect. The redraft is a
/// fake reasoner; a send is a recorded call.
struct StoreHandler {
    store: Arc<Store>,
    sends: Mutex<Vec<(String, String)>>,
    revises: Mutex<Vec<(String, String)>>,
    fail_revise: AtomicBool,
    /// Hold a claimed send until `release` fires.
    hold_send: AtomicBool,
    held: Notify,
    release: Notify,
    broker: OnceLock<Weak<dyn ApprovalBroker>>,
}

impl StoreHandler {
    fn new(store: Arc<Store>) -> Arc<Self> {
        Arc::new(Self {
            store,
            sends: Mutex::new(Vec::new()),
            revises: Mutex::new(Vec::new()),
            fail_revise: AtomicBool::new(false),
            hold_send: AtomicBool::new(false),
            held: Notify::new(),
            release: Notify::new(),
            broker: OnceLock::new(),
        })
    }

    fn sends(&self) -> Vec<(String, String)> {
        self.sends.lock().unwrap().clone()
    }

    fn resolved(&self, id: &str) -> ApprovalActionOutcome {
        match self.store.get_action_with_email(id).unwrap() {
            Some(a) => ApprovalActionOutcome::AlreadyResolved {
                status: a.action.status,
                detail: a.action.error_message,
            },
            None => ApprovalActionOutcome::NotFound,
        }
    }

    #[allow(clippy::result_large_err)]
    fn pending_or(
        &self,
        id: &str,
    ) -> Result<augmentagent_store::ActionWithEmail, ApprovalActionOutcome> {
        match self.store.get_action_with_email(id).unwrap() {
            None => Err(ApprovalActionOutcome::NotFound),
            Some(a) if a.action.status != "pending" => {
                Err(ApprovalActionOutcome::AlreadyResolved {
                    status: a.action.status,
                    detail: a.action.error_message,
                })
            }
            Some(a) => Ok(a),
        }
    }
}

#[async_trait]
impl ApprovalActionHandler for StoreHandler {
    async fn approve(&self, id: &str) -> ApprovalActionOutcome {
        let row = match self.pending_or(id) {
            Ok(r) => r,
            Err(out) => return out,
        };
        if !self
            .store
            .claim_action_for_send(id, ActionStatus::Pending, "test")
            .unwrap()
        {
            return self.resolved(id);
        }
        if self.hold_send.load(Ordering::SeqCst) {
            self.held.notify_one();
            self.release.notified().await;
        }
        let body = row.action.draft_body.unwrap_or_default();
        self.sends.lock().unwrap().push((id.to_string(), body));
        self.store
            .update_action_status(id, ActionStatus::Sent, None, None)
            .unwrap();
        ApprovalActionOutcome::Approved
    }

    async fn revise(&self, id: &str, feedback: &str) -> ApprovalActionOutcome {
        let row = match self.pending_or(id) {
            Ok(r) => r,
            Err(out) => return out,
        };
        self.revises
            .lock()
            .unwrap()
            .push((id.to_string(), feedback.to_string()));
        if self.fail_revise.load(Ordering::SeqCst) {
            return ApprovalActionOutcome::Failed {
                message: "redraft call failed: provider unavailable".into(),
            };
        }
        let draft = format!(
            "Redrafted ({}).",
            feedback.chars().take(40).collect::<String>()
        );
        if !self.store.refresh_pending_draft(id, &draft).unwrap() {
            return self.resolved(id);
        }
        ApprovalActionOutcome::Revised {
            email: row.email,
            draft,
        }
    }

    async fn skip(&self, id: &str) -> ApprovalActionOutcome {
        if let Err(out) = self.pending_or(id) {
            return out;
        }
        if self
            .store
            .try_resolve_action(
                id,
                ActionStatus::Rejected,
                "test",
                Some("skipped by approver"),
            )
            .unwrap()
        {
            ApprovalActionOutcome::Skipped
        } else {
            self.resolved(id)
        }
    }

    async fn is_resolved(&self, id: &str) -> bool {
        self.pending_or(id).is_err()
    }

    async fn recompose(&self, id: &str) -> ApprovalActionOutcome {
        if !self.store.recompose_action(id, "test").unwrap() {
            return self.resolved(id);
        }
        let row = self.store.get_action_with_email(id).unwrap().unwrap();
        if let Some(broker) = self.broker.get().and_then(Weak::upgrade) {
            broker
                .post_approval_card(
                    id,
                    &row.email,
                    &row.action.draft_body.unwrap_or_default(),
                    0,
                )
                .await
                .unwrap();
        }
        ApprovalActionOutcome::Recomposed
    }
}

/// Stands in for Discord's card redraw.
#[derive(Default)]
struct DiscordCards {
    redraws: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl ApprovalCardSurface for DiscordCards {
    fn surface_name(&self) -> &'static str {
        "discord"
    }
    async fn redraw_cards(&self, action_id: &str, origin: &str) {
        self.redraws
            .lock()
            .unwrap()
            .push((action_id.into(), origin.into()));
    }
}

/// The agent: approval commands must never reach it.
#[derive(Default)]
struct Agent {
    texts: Mutex<Vec<String>>,
    calls: AtomicU32,
}

#[async_trait]
impl SlackTurnHandler for Agent {
    async fn handle_turn(&self, turn: &SlackTurn) -> anyhow::Result<Option<SlackTurnReply>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.texts.lock().unwrap().push(turn.text.clone());
        if turn.text.is_empty() {
            return Ok(None);
        }
        Ok(Some(SlackTurnReply {
            text: format!("agent: {}", turn.text),
            files: Vec::new(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn workspace() -> SlackWorkspace {
    SlackWorkspace::new(TEAM, None).unwrap()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// A Slack `action_ts` for "now" (the click just happened).
fn click_ts(offset_ms: i64) -> String {
    let ms = now_ms() + offset_ms;
    format!("{}.{:06}", ms / 1000, (ms % 1000) * 1000)
}

struct Harness {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Arc<Store>,
    web: Arc<RecordingSlackWebApi>,
    handler: Arc<StoreHandler>,
    surfaces: CardSurfaces,
    discord: Arc<DiscordCards>,
    _discord_dyn: Arc<dyn ApprovalCardSurface>,
    agent: Arc<Agent>,
    /// Added to the approval surface's clock (a suspended Mac).
    skew_ms: Arc<AtomicI64>,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state dir ü").join("data.db");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
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
        let surfaces = CardSurfaces::new();
        let discord = Arc::new(DiscordCards::default());
        let discord_dyn: Arc<dyn ApprovalCardSurface> = discord.clone();
        surfaces.register(&discord_dyn);
        Harness {
            _dir: dir,
            path,
            handler: StoreHandler::new(Arc::clone(&store)),
            store,
            web: Arc::new(RecordingSlackWebApi::default()),
            surfaces,
            discord,
            _discord_dyn: discord_dyn,
            agent: Arc::new(Agent::default()),
            skew_ms: Arc::new(AtomicI64::new(0)),
        }
    }

    fn approvals(&self, store: Arc<Store>, channel: &str) -> Arc<SlackApprovals> {
        let skew = Arc::clone(&self.skew_ms);
        let approvals = Arc::new(
            SlackApprovals::new(
                store,
                Arc::clone(&self.web) as Arc<dyn SlackWebApi>,
                SlackApprovalConfig {
                    workspace: workspace(),
                    channel: channel.into(),
                },
                self.surfaces.clone(),
            )
            .with_clock(Arc::new(move || now_ms() + skew.load(Ordering::SeqCst))),
        );
        approvals.set_handler(self.handler.clone());
        approvals.register();
        let broker: Arc<dyn ApprovalBroker> = approvals.clone();
        let _ = self.handler.broker.set(Arc::downgrade(&broker));
        approvals
    }

    fn surface(
        &self,
        store: Arc<Store>,
        connector: Arc<dyn SocketConnector>,
        approvals: &Arc<SlackApprovals>,
    ) -> SlackInteractiveSurface {
        SlackInteractiveSurface::new(
            store,
            vec![SlackWorkspaceRuntime {
                workspace: workspace(),
                web: Arc::clone(&self.web) as Arc<dyn SlackWebApi>,
                bot: SlackBotIdentity {
                    bot_user_id: Some(BOT_USER.into()),
                    bot_id: Some("B00000001".into()),
                    app_id: Some("A00000001".into()),
                },
            }],
            vec![connector],
            self.agent.clone(),
            config(),
        )
        .with_approvals(Arc::clone(approvals))
    }

    fn pending(&self, subject: &str, draft: &str) -> (String, Email) {
        let n = now_ms();
        let email = Email {
            message_id: format!("slack:C00000009:{subject}:{n}"),
            thread_id: Some("C00000009".into()),
            from: "Contact Example".into(),
            to: String::new(),
            cc: String::new(),
            attachments: Vec::new(),
            subject: subject.into(),
            body: "Are you free for lunch next week?".into(),
            date: String::new(),
            account_entity_id: Some("slack:team:T00000009".into()),
            platform: "slack".into(),
            kind: "dm".into(),
        };
        self.store.upsert_email(&email).unwrap();
        let id = self
            .store
            .log_action(
                &email.message_id,
                email.thread_id.as_deref(),
                &email.from,
                &email.subject,
                Some(&email.body),
                Some(draft),
                ActionStatus::Pending,
            )
            .unwrap();
        (id, email)
    }

    fn calls(&self) -> Vec<RecordedCall> {
        self.web.calls()
    }

    fn posts(&self) -> Vec<PostMessage> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostMessage(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn updates(&self) -> Vec<UpdateMessage> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::UpdateMessage(u) => Some(u),
                _ => None,
            })
            .collect()
    }

    fn ephemerals(&self) -> Vec<PostEphemeral> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostEphemeral(p) => Some(p),
                _ => None,
            })
            .collect()
    }

    fn modals(&self) -> Vec<(String, Value)> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::OpenModal { trigger_id, view } => Some((trigger_id, view)),
                _ => None,
            })
            .collect()
    }

    fn status(&self, id: &str) -> String {
        self.store
            .get_action_with_email(id)
            .unwrap()
            .unwrap()
            .action
            .status
    }

    fn card_states(&self, id: &str) -> Vec<ApprovalCardState> {
        self.store
            .approval_cards_for_action(&SurfacePlatform::new("slack").unwrap(), id)
            .unwrap()
            .into_iter()
            .map(|c| c.state)
            .collect()
    }
}

fn config() -> SlackSurfaceConfig {
    SlackSurfaceConfig {
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

/// The posted card for `id`: its channel, ts and control block ID.
fn card_of(h: &Harness, id: &str) -> (String, String, String) {
    let card = h
        .store
        .approval_cards_for_action(&SurfacePlatform::new("slack").unwrap(), id)
        .unwrap()
        .into_iter()
        .next()
        .expect("a card was recorded");
    let post = h
        .posts()
        .into_iter()
        .rev()
        .find(|p| {
            p.blocks
                .as_ref()
                .is_some_and(|b| b.to_string().contains(&format!("aa|{id}|")))
        })
        .expect("the card was posted");
    let block_id = post
        .blocks
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "actions")
        .map(|b| b["block_id"].as_str().unwrap().to_string())
        .expect("the card has controls");
    (
        card.message.conversation().conversation_id().to_string(),
        card.message.message_id().to_string(),
        block_id,
    )
}

/// The control block ID a (redrawn) card carries.
fn actions_block_id(update: &UpdateMessage) -> String {
    update
        .blocks
        .as_ref()
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "actions")
        .unwrap()["block_id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn block_action(
    envelope_id: &str,
    user: &str,
    channel: &str,
    message_ts: &str,
    action_id: &str,
    block_id: &str,
    selected: Option<&str>,
    action_ts: &str,
) -> Value {
    let mut action = json!({
        "action_id": action_id, "block_id": block_id, "type": "button",
        "value": block_id, "action_ts": action_ts,
    });
    if let Some(v) = selected {
        action["type"] = json!("static_select");
        action["selected_option"] = json!({"text": {"type": "plain_text", "text": v}, "value": v});
    }
    json!({
        "type": "interactive",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "block_actions",
            "trigger_id": format!("trigger-{envelope_id}"),
            "team": {"id": TEAM},
            "user": {"id": user, "team_id": TEAM},
            "channel": {"id": channel},
            "container": {"type": "message", "message_ts": message_ts, "channel_id": channel},
            "message": {"ts": message_ts},
            "response_url": "https://hooks.slack.invalid/actions/x",
            "actions": [action],
        }
    })
}

fn view_submission(envelope_id: &str, user: &str, view: &Value, values: Value) -> Value {
    json!({
        "type": "interactive",
        "envelope_id": envelope_id,
        "accepts_response_payload": true,
        "payload": {
            "type": "view_submission",
            "trigger_id": format!("trigger-{envelope_id}"),
            "team": {"id": TEAM},
            "user": {"id": user, "team_id": TEAM},
            "view": {
                "id": "V00000001",
                "callback_id": view["callback_id"],
                "private_metadata": view["private_metadata"],
                "state": {"values": values},
            }
        }
    })
}

fn owner_dm(envelope_id: &str, text: &str, ts: &str) -> Value {
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "event_callback", "team_id": TEAM, "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"), "event_time": 1_700_000_000,
            "event": {"type": "message", "channel": DM, "channel_type": "im",
                      "user": OWNER, "text": text, "ts": ts}
        }
    })
}

async fn connected(h: &Harness, approvals: &Arc<SlackApprovals>) -> (Running, Server) {
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(Arc::clone(&h.store), connector, approvals));
    let server = Server::accept(&mut servers).await;
    (running, server)
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_card_is_posted_to_the_owner_dm_with_controls_and_text_commands() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals
        .post_approval(&id, &email, "Sure — Tuesday works.")
        .await
        .unwrap();

    let posts = h.posts();
    assert_eq!(posts.len(), 1);
    let post = &posts[0];
    assert_eq!(post.channel, DM);
    let blocks = post.blocks.as_ref().unwrap().to_string();
    for control in [card::APPROVE, card::REVISE, card::SKIP, card::REFINE] {
        assert!(blocks.contains(control), "{control} missing: {blocks}");
    }
    let r = &id[..8];
    assert!(blocks.contains(&format!("approve {r}")), "{blocks}");
    assert!(blocks.contains(&format!("revise {r}")), "{blocks}");
    // `<…>` is a link in mrkdwn: placeholders are escaped.
    assert!(blocks.contains("&lt;what to change&gt;"), "{blocks}");
    assert!(blocks.contains("Sure — Tuesday works."));
    assert!(blocks.contains("Lunch next week?"));
    let (channel, ts, block_id) = card_of(&h, &id);
    assert_eq!(channel, DM);
    assert!(!ts.is_empty());
    let control = ControlRef::parse(&block_id).unwrap();
    assert_eq!(control.action_id, id);
    assert_eq!(
        control.digest.as_deref(),
        Some(card::draft_digest("Sure — Tuesday works.").as_str())
    );
    assert_eq!(h.card_states(&id), vec![ApprovalCardState::Live]);
}

// ---------------------------------------------------------------------------
// Approve, second click, skip
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approve_sends_once_updates_the_card_in_place_and_a_second_click_is_already_sent() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;

    server
        .deliver(&block_action(
            "env-a1",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("the answer", || !h.ephemerals().is_empty()).await;
    assert_eq!(
        h.handler.sends(),
        vec![(id.clone(), "Sure — Tuesday works.".to_string())]
    );
    assert_eq!(h.status(&id), "sent");
    let eph = &h.ephemerals()[0];
    assert_eq!(eph.text, "Approved — sending.");
    assert_eq!(eph.user, OWNER);
    let update = h
        .updates()
        .into_iter()
        .find(|u| u.ts == ts)
        .expect("card updated in place");
    assert_eq!(update.channel, channel);
    let shown = update.blocks.as_ref().unwrap().to_string();
    assert!(shown.contains("✅ Sent."), "{shown}");
    assert!(!shown.contains(card::APPROVE), "no controls on a sent card");
    assert_eq!(h.card_states(&id), vec![ApprovalCardState::Settled]);
    assert_eq!(h.posts().len(), 1, "no new card: the card was edited");
    assert_eq!(
        h.discord.redraws.lock().unwrap().clone(),
        vec![(id.clone(), "slack".to_string())],
        "Discord was asked to redraw its card"
    );

    // Second click on the same (stale) card: no second send, the reason.
    server
        .deliver(&block_action(
            "env-a2",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("second answer", || h.ephemerals().len() == 2).await;
    assert_eq!(h.ephemerals()[1].text, "Already sent.");
    assert_eq!(h.handler.sends().len(), 1);
    assert_eq!(
        h.agent.calls.load(Ordering::SeqCst),
        0,
        "clicks never reach the agent"
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skip_discards_the_draft_and_the_card_says_so() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Newsletter", "Thanks!");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-s1",
            OWNER,
            &channel,
            &ts,
            card::SKIP,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("skip answer", || !h.ephemerals().is_empty()).await;
    assert_eq!(h.ephemerals()[0].text, "Skipped — draft discarded.");
    assert_eq!(h.status(&id), "rejected");
    assert!(h.handler.sends().is_empty());
    let update = h.updates().into_iter().find(|u| u.ts == ts).unwrap();
    assert!(update.blocks.unwrap().to_string().contains("Skipped"));
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Revise via modal, failed revise, presets, missing info
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revise_opens_a_modal_and_the_submission_redraws_the_same_card() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;

    server
        .deliver(&block_action(
            "env-r1",
            OWNER,
            &channel,
            &ts,
            card::REVISE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("modal opened", || !h.modals().is_empty()).await;
    let (trigger, view) = h.modals()[0].clone();
    assert_eq!(trigger, "trigger-env-r1");
    assert_eq!(view["callback_id"], card::REVISE_MODAL);

    server
        .deliver(&view_submission(
            "env-r2",
            OWNER,
            &view,
            json!({"feedback": {"feedback": {"type": "plain_text_input", "value": "Offer Wednesday instead"}}}),
        ))
        .await;
    eventually("revise answer", || !h.ephemerals().is_empty()).await;
    assert_eq!(h.ephemerals()[0].text, REVISED_REPLY);
    assert_eq!(
        h.handler.revises.lock().unwrap().clone(),
        vec![(id.clone(), "Offer Wednesday instead".to_string())]
    );
    let row = h.store.get_action_with_email(&id).unwrap().unwrap();
    assert_eq!(row.action.status, "pending");
    assert_eq!(
        row.action.draft_body.as_deref(),
        Some("Redrafted (Offer Wednesday instead).")
    );
    assert_eq!(h.store.redraft_count(&id).unwrap(), 1);
    assert_eq!(
        h.store.list_revisions_for_action(&id).unwrap().len(),
        2,
        "#37 triple captured"
    );
    let update = h
        .updates()
        .into_iter()
        .find(|u| u.ts == ts)
        .expect("same card redrawn");
    let shown = update.blocks.as_ref().unwrap().to_string();
    assert!(
        shown.contains("Redrafted (Offer Wednesday instead)."),
        "{shown}"
    );
    assert!(shown.contains("Revised draft"), "{shown}");
    assert!(shown.contains("draft v2"), "{shown}");
    assert!(shown.contains(card::APPROVE), "still actionable");
    assert_eq!(h.posts().len(), 1, "revised in place, not reposted");
    assert_eq!(h.card_states(&id), vec![ApprovalCardState::Live]);

    // The redrawn card carries the new draft's digest: approving it sends
    // the revised text.
    let new_block = actions_block_id(&update);
    server
        .deliver(&block_action(
            "env-r3",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &new_block,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("sent", || h.handler.sends().len() == 1).await;
    assert_eq!(
        h.handler.sends()[0].1,
        "Redrafted (Offer Wednesday instead)."
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_revise_keeps_the_card_and_leaves_a_durable_notice() {
    let h = Harness::new();
    h.handler.fail_revise.store(true, Ordering::SeqCst);
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Proposal", "Looks good.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-f1",
            OWNER,
            &channel,
            &ts,
            card::REVISE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("modal", || !h.modals().is_empty()).await;
    let view = h.modals()[0].1.clone();
    server
        .deliver(&view_submission(
            "env-f2",
            OWNER,
            &view,
            json!({"feedback": {"feedback": {"value": "shorter"}}}),
        ))
        .await;
    eventually("answer", || !h.ephemerals().is_empty()).await;
    assert_eq!(
        h.ephemerals()[0].text,
        "Failed: redraft call failed: provider unavailable"
    );
    let notice = h
        .posts()
        .into_iter()
        .find(|p| p.text.contains("Revise produced no new draft"))
        .expect("durable notice");
    assert_eq!(notice.channel, DM);
    assert!(notice.text.contains("Proposal"));
    assert_eq!(h.status(&id), "pending");
    assert_eq!(h.card_states(&id), vec![ApprovalCardState::Live]);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_preset_redrafts_with_its_feedback_and_stops_at_the_cap() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Intro", "Hello there, nice to meet you.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-p1",
            OWNER,
            &channel,
            &ts,
            card::REFINE,
            &block_id,
            Some("shorter"),
            &click_ts(0),
        ))
        .await;
    eventually("preset answer", || !h.ephemerals().is_empty()).await;
    let shorter = PRESETS.iter().find(|p| p.id == "shorter").unwrap();
    assert_eq!(
        h.handler.revises.lock().unwrap().clone(),
        vec![(id.clone(), shorter.feedback.to_string())]
    );
    assert_eq!(h.store.redraft_count(&id).unwrap(), 1);
    let preset: Option<String> = h
        .store
        .with_conn(|c| {
            c.query_row(
                "SELECT lastPresetId FROM actions WHERE id = ?1",
                [&id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(preset.as_deref(), Some("shorter"));

    // At the cap the preset is refused and nothing is redrafted.
    h.store
        .with_conn(|c| {
            c.execute(
                "UPDATE actions SET redraftCount = ?2 WHERE id = ?1",
                augmentagent_store::rusqlite::params![id, MAX_REDRAFT_ITERATIONS],
            )
        })
        .unwrap();
    let block_now = {
        let update = h.updates().into_iter().rfind(|u| u.ts == ts).unwrap();
        update
            .blocks
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "actions")
            .unwrap()["block_id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    server
        .deliver(&block_action(
            "env-p2",
            OWNER,
            &channel,
            &ts,
            card::REFINE,
            &block_now,
            Some("warmer"),
            &click_ts(0),
        ))
        .await;
    eventually("cap answer", || h.ephemerals().len() == 2).await;
    assert_eq!(
        h.ephemerals()[1].text,
        format!("Failed: {REFINE_LIMIT_REPLY}")
    );
    assert_eq!(h.handler.revises.lock().unwrap().len(), 1);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_info_opens_one_input_per_ask_and_rewrites_with_the_values() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let draft = append_needs_input_marker(
        "Happy to meet — how about [TIME]?",
        &[(
            "scheduling".to_string(),
            "Which time should I propose?".to_string(),
        )],
    );
    let (id, email) = h.pending("Meeting", &draft);
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    assert!(h.posts()[0]
        .blocks
        .as_ref()
        .unwrap()
        .to_string()
        .contains(card::FILL));
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-m1",
            OWNER,
            &channel,
            &ts,
            card::FILL,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("fill modal", || !h.modals().is_empty()).await;
    let view = h.modals()[0].1.clone();
    assert_eq!(view["callback_id"], card::FILL_MODAL);
    assert!(view.to_string().contains("Which time should I propose?"));
    server
        .deliver(&view_submission(
            "env-m2",
            OWNER,
            &view,
            json!({"ask_0": {"value": {"value": "Thursday 3pm"}}}),
        ))
        .await;
    eventually("rewrite", || !h.handler.revises.lock().unwrap().is_empty()).await;
    let feedback = h.handler.revises.lock().unwrap()[0].1.clone();
    assert!(feedback.contains("Thursday 3pm"), "{feedback}");
    assert!(feedback.contains("Proposed meeting time"), "{feedback}");
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Supersede, recompose, changed drafts
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_card_explains_why_and_recompose_restores_a_fresh_card() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Thread", "On it.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    h.store
        .mark_pending_superseded_by_ids(std::slice::from_ref(&id), "superseded by manual reply")
        .unwrap();
    // The reconcile sweep redraws the card nothing clicked.
    assert_eq!(approvals.reconcile().await, 1);
    let update = h.updates().into_iter().find(|u| u.ts == ts).unwrap();
    let shown = update.blocks.unwrap().to_string();
    assert!(shown.contains("newest card for this thread"), "{shown}");
    assert!(shown.contains(card::RECOMPOSE), "{shown}");
    assert!(!shown.contains(card::APPROVE));
    assert_eq!(approvals.reconcile().await, 0, "nothing left to redraw");

    let (running, mut server) = connected(&h, &approvals).await;
    // A click on the stale card gets the reason and the recovery button.
    server
        .deliver(&block_action(
            "env-x1",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("reason", || !h.ephemerals().is_empty()).await;
    let eph = h.ephemerals()[0].clone();
    assert_eq!(
        eph.text,
        "A newer version replaced this draft. Act on the newest card for this thread."
    );
    let recompose_block = eph
        .blocks
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "actions")
        .unwrap()["block_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(h.handler.sends().is_empty());

    server
        .deliver(&block_action(
            "env-x2",
            OWNER,
            &channel,
            &ts,
            card::RECOMPOSE,
            &recompose_block,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("recomposed", || h.ephemerals().len() == 2).await;
    assert!(h.ephemerals()[1].text.starts_with("Recomposed"));
    assert_eq!(h.status(&id), "pending");
    assert_eq!(h.posts().len(), 2, "a fresh card was posted");
    assert_eq!(
        h.card_states(&id),
        vec![ApprovalCardState::Live, ApprovalCardState::Replaced],
        "the new card is live and the old one points at it"
    );
    let old = h.updates().into_iter().rfind(|u| u.ts == ts).unwrap();
    assert!(old.blocks.unwrap().to_string().contains("Reposted below"));
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_on_a_card_showing_an_older_draft_is_refused_and_the_card_redrawn() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Budget", "First draft.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    // Another path replaced the draft (a revise elsewhere, update-draft).
    assert!(h.store.refresh_pending_draft(&id, "Second draft.").unwrap());
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-d1",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("refusal", || !h.ephemerals().is_empty()).await;
    assert_eq!(h.ephemerals()[0].text, STALE_DRAFT_REPLY);
    assert!(
        h.handler.sends().is_empty(),
        "never sends a draft the owner did not see"
    );
    assert_eq!(h.status(&id), "pending");
    let update = h.updates().into_iter().find(|u| u.ts == ts).unwrap();
    assert!(update.blocks.unwrap().to_string().contains("Second draft."));
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Text commands
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_commands_with_explicit_references_work_without_controls() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    let (other, _) = h.pending("Another", "Other draft.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (_, ts, _) = card_of(&h, &id);
    let r = id[..8].to_string();
    let (running, mut server) = connected(&h, &approvals).await;

    server
        .deliver(&owner_dm("env-c1", "approvals", "1700000001.000100"))
        .await;
    eventually("queue", || {
        h.posts()
            .iter()
            .any(|p| p.text.contains("Pending approvals"))
    })
    .await;
    let queue = h
        .posts()
        .into_iter()
        .find(|p| p.text.contains("Pending approvals"))
        .unwrap();
    // The answer is Markdown the outbox converts once: bold, not italic,
    // and `<`/`&` escaped exactly once.
    assert!(
        queue.text.starts_with("*Pending approvals* (2)"),
        "{}",
        queue.text
    );
    assert!(!queue.text.contains("&amp;"), "{}", queue.text);
    assert!(
        queue.text.contains(&r) && queue.text.contains(&other[..8]),
        "{}",
        queue.text
    );

    server
        .deliver(&owner_dm(
            "env-c2",
            &format!("approve {r}"),
            "1700000001.000200",
        ))
        .await;
    eventually("approved", || {
        h.posts().iter().any(|p| p.text == "Approved — sending.")
    })
    .await;
    assert_eq!(h.handler.sends().len(), 1);
    assert!(
        h.updates().iter().any(|u| u.ts == ts),
        "the card was updated in place"
    );

    server
        .deliver(&owner_dm(
            "env-c3",
            &format!("approve {r}"),
            "1700000001.000300",
        ))
        .await;
    eventually("already", || {
        h.posts().iter().any(|p| p.text == "Already sent.")
    })
    .await;
    assert_eq!(h.handler.sends().len(), 1);

    server
        .deliver(&owner_dm(
            "env-c4",
            &format!("revise {} make it warmer", &other[..8]),
            "1700000001.000400",
        ))
        .await;
    eventually("revised", || !h.handler.revises.lock().unwrap().is_empty()).await;
    assert_eq!(
        h.handler.revises.lock().unwrap()[0],
        (other.clone(), "make it warmer".to_string())
    );

    // Ordinary requests that start with a verb still go to the agent.
    server
        .deliver(&owner_dm(
            "env-c5",
            "send the report to finance",
            "1700000001.000500",
        ))
        .await;
    eventually("agent", || h.agent.calls.load(Ordering::SeqCst) == 1).await;
    assert_eq!(
        h.agent.texts.lock().unwrap()[0],
        "send the report to finance"
    );
    server
        .deliver(&owner_dm("env-c6", "approve 00000000", "1700000001.000600"))
        .await;
    eventually("no match", || {
        h.posts()
            .iter()
            .any(|p| p.text.contains("No approval matches"))
    })
    .await;
    assert_eq!(
        h.agent.calls.load(Ordering::SeqCst),
        1,
        "commands never reach the agent"
    );
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Cross-surface
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_decision_on_discord_redraws_the_slack_card_and_slack_then_says_already_sent() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);

    // What Discord's event handler calls on an Approve click.
    let discord_handler =
        SyncingActionHandler::new("discord", h.handler.clone(), h.surfaces.clone());
    assert!(matches!(
        discord_handler.approve(&id).await,
        ApprovalActionOutcome::Approved
    ));
    let update = h
        .updates()
        .into_iter()
        .find(|u| u.ts == ts)
        .expect("Slack card redrawn");
    assert!(update.blocks.unwrap().to_string().contains("✅ Sent."));
    assert!(
        h.discord.redraws.lock().unwrap().is_empty(),
        "Discord redraws its own card"
    );

    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-y1",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("already", || !h.ephemerals().is_empty()).await;
    assert_eq!(h.ephemerals()[0].text, "Already sent.");
    assert_eq!(h.handler.sends().len(), 1);
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_clicks_on_slack_and_discord_send_once_and_the_ack_does_not_wait() {
    let h = Harness::new();
    h.handler.hold_send.store(true, Ordering::SeqCst);
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;

    let discord_handler = Arc::new(SyncingActionHandler::new(
        "discord",
        h.handler.clone(),
        h.surfaces.clone(),
    ));
    let held = h.handler.held.notified();
    let d = Arc::clone(&discord_handler);
    let did = id.clone();
    let discord_click = tokio::spawn(async move { d.approve(&did).await });
    // The Slack click is acknowledged as soon as it is persisted, even
    // while a send is in flight.
    server
        .deliver(&block_action(
            "env-z1",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    held.await;
    // The loser answers without waiting for the winner.
    eventually("the losing click answered", || {
        discord_click.is_finished() || !h.ephemerals().is_empty()
    })
    .await;
    h.handler.release.notify_one();
    let discord_outcome = discord_click.await.unwrap();
    eventually("slack answered", || !h.ephemerals().is_empty()).await;
    let slack_text = h.ephemerals()[0].text.clone();

    assert_eq!(h.handler.sends().len(), 1, "one send across both surfaces");
    let approved = [
        matches!(discord_outcome, ApprovalActionOutcome::Approved),
        slack_text == "Approved — sending.",
    ];
    assert_eq!(
        approved.iter().filter(|a| **a).count(),
        1,
        "{discord_outcome:?} / {slack_text}"
    );
    assert!(
        matches!(&discord_outcome, ApprovalActionOutcome::AlreadyResolved { status, .. } if status == "sending")
            || slack_text == "Already sending — a send is in flight.",
        "{discord_outcome:?} / {slack_text}"
    );
    eventually("card shows sent", || {
        h.updates()
            .iter()
            .any(|u| u.ts == ts && u.blocks.as_ref().unwrap().to_string().contains("✅ Sent."))
    })
    .await;
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_non_owner_click_is_rejected_and_changes_nothing() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), CONTROL);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    assert_eq!(channel, CONTROL);
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-n1",
            STRANGER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("rejection", || !h.ephemerals().is_empty()).await;
    let eph = &h.ephemerals()[0];
    assert_eq!(eph.user, STRANGER);
    assert_eq!(eph.text, REJECTION_REPLY);
    assert!(h.handler.sends().is_empty());
    assert_eq!(h.status(&id), "pending");
    assert!(h.updates().is_empty(), "the card is untouched");
    let audited: i64 = h
        .store
        .with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM surface_auth_rejections WHERE actor_id = ?1",
                [STRANGER],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(audited, 1);
    running.stop().await;
}

// A refused click in a DM is answered in that DM, top level: the dispatch
// lane an interaction runs in is not a Slack thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_click_in_a_dm_is_answered_top_level_not_in_a_lane_thread() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    let (running, mut server) = connected(&h, &approvals).await;
    server
        .deliver(&block_action(
            "env-n2",
            STRANGER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("rejection", || {
        h.posts().iter().any(|p| p.text == REJECTION_REPLY)
    })
    .await;
    let rejection = h
        .posts()
        .into_iter()
        .find(|p| p.text == REJECTION_REPLY)
        .unwrap();
    assert_eq!(rejection.channel, DM);
    assert_eq!(rejection.thread_ts, None, "{rejection:?}");
    assert!(h.handler.sends().is_empty());
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Restart and late clicks
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_between_post_and_click_still_resolves_and_redraws() {
    let h = Harness::new();
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    {
        // The daemon that posted the card.
        let first = h.approvals(Arc::clone(&h.store), DM);
        first.post_approval(&id, &email, "x").await.unwrap();
    }
    let (channel, ts, block_id) = card_of(&h, &id);

    // A fresh process: new store handle, new surface, nothing in memory.
    let store = Arc::new(Store::open(&h.path).unwrap());
    let handler = StoreHandler::new(Arc::clone(&store));
    let approvals = Arc::new(SlackApprovals::new(
        Arc::clone(&store),
        Arc::clone(&h.web) as Arc<dyn SlackWebApi>,
        SlackApprovalConfig {
            workspace: workspace(),
            channel: DM.into(),
        },
        CardSurfaces::new(),
    ));
    approvals.set_handler(handler.clone());
    let (connector, mut servers) = DuplexConnector::new();
    let running = start(h.surface(Arc::clone(&store), connector, &approvals));
    let mut server = Server::accept(&mut servers).await;
    server
        .deliver(&block_action(
            "env-rs1",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("resolved after restart", || !h.ephemerals().is_empty()).await;
    assert_eq!(h.ephemerals()[0].text, "Approved — sending.");
    assert_eq!(handler.sends().len(), 1);
    assert!(
        h.updates().iter().any(|u| u.ts == ts),
        "the pre-restart card was updated"
    );
    running.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_click_handled_after_the_mac_slept_gets_a_fresh_message() {
    let h = Harness::new();
    let approvals = h.approvals(Arc::clone(&h.store), DM);
    let (id, email) = h.pending("Lunch next week?", "Sure — Tuesday works.");
    approvals.post_approval(&id, &email, "x").await.unwrap();
    let (channel, ts, block_id) = card_of(&h, &id);
    // The clicks were persisted and acked, then the Mac slept for ten
    // minutes before the dispatcher got to them.
    h.skew_ms.store(10 * 60 * 1000, Ordering::SeqCst);
    let (running, mut server) = connected(&h, &approvals).await;

    server
        .deliver(&block_action(
            "env-l1",
            OWNER,
            &channel,
            &ts,
            card::REVISE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("fresh message", || {
        h.posts().iter().any(|p| p.text.contains("could not open"))
    })
    .await;
    assert!(h.modals().is_empty(), "an expired trigger is never used");
    let fresh = h
        .posts()
        .into_iter()
        .find(|p| p.text.contains("could not open"))
        .unwrap();
    assert_eq!(fresh.channel, DM);
    assert!(
        !fresh.text.contains("<what"),
        "mrkdwn-escaped: {}",
        fresh.text
    );
    assert!(
        fresh.text.contains(&format!("revise {}", &id[..8])),
        "{}",
        fresh.text
    );

    server
        .deliver(&block_action(
            "env-l2",
            OWNER,
            &channel,
            &ts,
            card::APPROVE,
            &block_id,
            None,
            &click_ts(0),
        ))
        .await;
    eventually("late approve answered", || {
        h.posts()
            .iter()
            .any(|p| p.text.starts_with("Approved — sending."))
    })
    .await;
    assert_eq!(
        h.handler.sends().len(),
        1,
        "the late decision still runs exactly once"
    );
    assert!(
        h.ephemerals().is_empty(),
        "late answers are new messages, not ephemerals"
    );
    running.stop().await;
}
