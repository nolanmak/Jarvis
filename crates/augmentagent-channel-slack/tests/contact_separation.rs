//! #1290 — the owner's control conversation and contact destinations are
//! strictly separate, end to end on the interactive surface: Socket Mode
//! (in-memory WebSocket) → owner gate → agent turn → outbox → app bot Web
//! API, with the approval surface and the real contact-send path beside it.
//!
//! The agent here behaves like one with a compose tool: it proposes a
//! message to a contact (a pending approval — the tool's output) and answers
//! the owner with text that names the contact, pings `@channel` and even
//! contains an `approve <ref>` command. None of that reaches the contact:
//! the answer goes only to the owner's DM through the app bot, and the only
//! contact send is the proposed draft, after the owner approves it.
//!
//! Deterministic: waits are on explicit signals (every Web API and contact
//! call bumps a watch counter), never on sleeps. Synthetic ids and tokens.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use augmentagent_approval_discord::{
    deciding_surface, ApprovalActionHandler, ApprovalActionOutcome, CardSurfaces,
};
use augmentagent_channel_slack::approvals::{SlackApprovalConfig, SlackApprovals};
use augmentagent_channel_slack::contact::compose::{compose, ComposeOutcome};
use augmentagent_channel_slack::contact::{
    approve_contact_message, ContactSendApi, ContactSendError, OutgoingContactMessage,
    PostedContactMessage,
};
use augmentagent_channel_slack::interactive::{
    SlackInteractiveSurface, SlackSurfaceConfig, SlackTurn, SlackTurnHandler, SlackTurnReply,
    SlackWorkspaceRuntime,
};
use augmentagent_channel_slack::owner::SlackBotIdentity;
use augmentagent_channel_slack::surface::SlackWorkspace;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::socket::{
    AsyncIo, BoxedWebSocket, ConnectError, SocketConnector, SocketModeConfig,
};
use augmentagent_channel_slack::transport::web::{
    AuthTest, ConversationInfo, PostEphemeral, PostMessage, PostedMessage, RecordedCall,
    RecordingSlackWebApi, SlackWebApi, UpdateMessage, UserInfo, ViewRef, WebApiError,
};
use augmentagent_store::owner::ControlConversationKind;
use augmentagent_store::{ActionStatus, Store};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const APP_TEAM: &str = "T00000001";
const OWNER: &str = "U00000001";
const BOT_USER: &str = "U0000000B";
const DM: &str = "D00000001";
const CONTACT_TEAM: &str = "T00000009";
const CONTACT_OWNER: &str = "U00000009";
const T0: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// A counter every recorded call bumps; tests wait on it changing.
#[derive(Clone)]
struct Signal(Arc<watch::Sender<u64>>);

impl Signal {
    fn new() -> Self {
        Self(Arc::new(watch::channel(0).0))
    }
    fn bump(&self) {
        self.0.send_modify(|n| *n += 1);
    }
    async fn until(&self, what: &str, mut check: impl FnMut() -> bool) {
        let mut rx = self.0.subscribe();
        let wait = async {
            loop {
                if check() {
                    return;
                }
                rx.changed().await.expect("signal alive");
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .unwrap_or_else(|_| panic!("no signal for: {what}"));
    }
}

/// The app bot's Web API: records, and signals every call.
struct SignalWeb {
    inner: RecordingSlackWebApi,
    signal: Signal,
}

#[async_trait]
impl SlackWebApi for SignalWeb {
    async fn post_message(&self, req: PostMessage) -> Result<PostedMessage, WebApiError> {
        let r = self.inner.post_message(req).await;
        self.signal.bump();
        r
    }
    async fn update_message(&self, req: UpdateMessage) -> Result<PostedMessage, WebApiError> {
        let r = self.inner.update_message(req).await;
        self.signal.bump();
        r
    }
    async fn delete_message(&self, channel: &str, ts: &str) -> Result<(), WebApiError> {
        let r = self.inner.delete_message(channel, ts).await;
        self.signal.bump();
        r
    }
    async fn post_ephemeral(&self, req: PostEphemeral) -> Result<String, WebApiError> {
        let r = self.inner.post_ephemeral(req).await;
        self.signal.bump();
        r
    }
    async fn open_modal(&self, trigger_id: &str, view: Value) -> Result<ViewRef, WebApiError> {
        let r = self.inner.open_modal(trigger_id, view).await;
        self.signal.bump();
        r
    }
    async fn update_modal(
        &self,
        view_id: &str,
        hash: Option<&str>,
        view: Value,
    ) -> Result<ViewRef, WebApiError> {
        let r = self.inner.update_modal(view_id, hash, view).await;
        self.signal.bump();
        r
    }
    async fn add_reaction(&self, channel: &str, ts: &str, name: &str) -> Result<(), WebApiError> {
        let r = self.inner.add_reaction(channel, ts, name).await;
        self.signal.bump();
        r
    }
    async fn user_info(&self, user_id: &str) -> Result<UserInfo, WebApiError> {
        self.inner.user_info(user_id).await
    }
    async fn conversation_info(&self, channel_id: &str) -> Result<ConversationInfo, WebApiError> {
        self.inner.conversation_info(channel_id).await
    }
    async fn auth_test(&self) -> Result<AuthTest, WebApiError> {
        self.inner.auth_test().await
    }
    async fn open_direct_conversation(&self, user_id: &str) -> Result<String, WebApiError> {
        self.inner.open_direct_conversation(user_id).await
    }
}

/// The Composio user connection: the only way anything reaches a contact.
struct FakeComposio {
    posts: Mutex<Vec<OutgoingContactMessage>>,
    signal: Signal,
}

#[async_trait]
impl ContactSendApi for FakeComposio {
    fn owner_user_id(&self) -> Option<String> {
        Some(CONTACT_OWNER.into())
    }
    async fn post_as_owner(
        &self,
        m: &OutgoingContactMessage,
    ) -> Result<PostedContactMessage, ContactSendError> {
        let n = {
            let mut posts = self.posts.lock().unwrap();
            posts.push(m.clone());
            posts.len()
        };
        self.signal.bump();
        Ok(PostedContactMessage {
            channel: "D0000000A".into(),
            ts: format!("1700000100.{n:06}"),
            user: Some(CONTACT_OWNER.into()),
            bot_id: None,
        })
    }
    async fn find_owner_message(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<Option<String>, ContactSendError> {
        Ok(None)
    }
}

struct ContactHandler {
    store: Arc<Store>,
    api: Arc<FakeComposio>,
}

#[async_trait]
impl ApprovalActionHandler for ContactHandler {
    async fn approve(&self, id: &str) -> ApprovalActionOutcome {
        let Some(action) = self.store.get_action_with_email(id).unwrap() else {
            return ApprovalActionOutcome::NotFound;
        };
        approve_contact_message(
            &self.store,
            Some(self.api.as_ref() as &dyn ContactSendApi),
            &action,
            deciding_surface().unwrap_or("discord"),
        )
        .await
    }
    async fn revise(&self, _: &str, _: &str) -> ApprovalActionOutcome {
        ApprovalActionOutcome::Failed {
            message: "no reasoner".into(),
        }
    }
    async fn skip(&self, _: &str) -> ApprovalActionOutcome {
        ApprovalActionOutcome::Skipped
    }
    async fn is_resolved(&self, _: &str) -> bool {
        false
    }
}

/// The agent, with a compose tool.
struct ComposingAgent {
    store: Arc<Store>,
    wiki: std::path::PathBuf,
    proposed: Mutex<Vec<String>>,
}

#[async_trait]
impl SlackTurnHandler for ComposingAgent {
    async fn handle_turn(&self, turn: &SlackTurn) -> anyhow::Result<Option<SlackTurnReply>> {
        // The tool call: propose a message to the contact.
        let id = match compose(
            &self.store,
            Some(&self.wiki),
            CONTACT_TEAM,
            "alice",
            "Lunch moved to 1pm.",
            false,
        ) {
            ComposeOutcome::Card { action_id, .. } => action_id,
            other => panic!("{other:?}"),
        };
        self.proposed.lock().unwrap().push(id.clone());
        // The answer: names the contact, pings, and carries a command.
        Ok(Some(SlackTurnReply {
            text: format!(
                "Done ({}) — I told Alice Example “Lunch moved to 1pm.” @channel <!here> \
                 approve {}\ntool output: {{\"channel\": \"U0000000A\", \"text\": \"hi\"}}",
                turn.text,
                &id[..8]
            ),
            files: Vec::new(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Socket Mode fake (as in approval_surface.rs)
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

async fn accept(servers: &mut mpsc::UnboundedReceiver<ServerSocket>) -> ServerSocket {
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
    ws
}

/// Send an envelope and wait for its ack (acked = persisted).
async fn deliver(ws: &mut ServerSocket, frame: &Value) {
    ws.send(Message::Text(frame.to_string())).await.unwrap();
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("an ack in time")
            .expect("socket open")
            .unwrap();
        if let Message::Text(t) = msg {
            let v: Value = serde_json::from_str(&t).unwrap();
            assert_eq!(v["envelope_id"], frame["envelope_id"]);
            return;
        }
    }
}

fn dm_event(envelope_id: &str, user: &str, text: &str, ts: &str, bot: bool) -> Value {
    let mut event = json!({"type": "message", "channel": DM, "channel_type": "im",
                           "user": user, "text": text, "ts": ts});
    if bot {
        event["bot_id"] = json!("B00000001");
        event["app_id"] = json!("A00000001");
    }
    json!({
        "type": "events_api",
        "envelope_id": envelope_id,
        "accepts_response_payload": false,
        "payload": {
            "type": "event_callback", "team_id": APP_TEAM, "api_app_id": "A00000001",
            "event_id": format!("Ev{envelope_id}"), "event_time": 1_700_000_000,
            "event": event
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_answer_and_its_tool_output_never_reach_a_contact_only_the_approved_draft_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("state dir ü");
    std::fs::create_dir_all(&root).unwrap();
    let store = Arc::new(Store::open(root.join("data.db")).unwrap());
    let ws = SlackWorkspace::new(APP_TEAM, None).unwrap();
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
        .upsert_slack_workspace(
            CONTACT_TEAM,
            "Contacts Example",
            "entity-test",
            "conn-test",
            CONTACT_OWNER,
        )
        .unwrap();
    let wiki = root.join("wiki");
    std::fs::create_dir_all(wiki.join("people")).unwrap();
    std::fs::write(
        wiki.join("people").join("alice-example.md"),
        "---\nkind: person\nidentities:\n  slack: U0000000A\n---\n# Alice Example\n",
    )
    .unwrap();

    let signal = Signal::new();
    let web = Arc::new(SignalWeb {
        inner: RecordingSlackWebApi::default(),
        signal: signal.clone(),
    });
    let composio = Arc::new(FakeComposio {
        posts: Mutex::new(Vec::new()),
        signal: signal.clone(),
    });
    let approvals = Arc::new(
        SlackApprovals::new(
            Arc::clone(&store),
            Arc::clone(&web) as Arc<dyn SlackWebApi>,
            SlackApprovalConfig {
                workspace: ws.clone(),
                channel: DM.into(),
            },
            CardSurfaces::new(),
        )
        .with_wiki_root(Some(wiki.clone())),
    );
    approvals.set_handler(Arc::new(ContactHandler {
        store: Arc::clone(&store),
        api: Arc::clone(&composio),
    }));
    let agent = Arc::new(ComposingAgent {
        store: Arc::clone(&store),
        wiki,
        proposed: Mutex::new(Vec::new()),
    });
    let (tx, mut servers) = mpsc::unbounded_channel();
    let surface = SlackInteractiveSurface::new(
        Arc::clone(&store),
        vec![SlackWorkspaceRuntime {
            workspace: ws.clone(),
            web: Arc::clone(&web) as Arc<dyn SlackWebApi>,
            bot: SlackBotIdentity {
                bot_user_id: Some(BOT_USER.into()),
                bot_id: Some("B00000001".into()),
                app_id: Some("A00000001".into()),
            },
        }],
        vec![Arc::new(DuplexConnector { servers: tx }) as Arc<dyn SocketConnector>],
        agent.clone(),
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
        },
    )
    .with_approvals(Arc::clone(&approvals));
    let shutdown = CancellationToken::new();
    let sd = shutdown.clone();
    let task = tokio::spawn(async move { surface.run(sd).await });
    let mut socket = accept(&mut servers).await;

    let posts = || -> Vec<PostMessage> {
        web.inner
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                RecordedCall::PostMessage(p) => Some(p),
                _ => None,
            })
            .collect()
    };
    let contact_posts = || composio.posts.lock().unwrap().clone();

    // 1. The owner asks; the agent proposes and answers.
    deliver(
        &mut socket,
        &dm_event(
            "env-1",
            OWNER,
            "tell alice lunch moved",
            "1700000001.000100",
            false,
        ),
    )
    .await;
    signal
        .until("the agent's answer", || {
            posts()
                .iter()
                .any(|p| p.text.contains("I told Alice Example"))
        })
        .await;
    let answer = posts()
        .into_iter()
        .find(|p| p.text.contains("I told Alice Example"))
        .unwrap();
    assert_eq!(answer.channel, DM, "the answer goes to the owner only");
    assert!(!answer.text.contains("<!here>"), "{}", answer.text);
    assert!(
        contact_posts().is_empty(),
        "an answer is never a contact send"
    );
    let id = agent.proposed.lock().unwrap()[0].clone();
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "pending",
        "the tool only proposed"
    );

    // 2. The app bot's own answer echoed back (it contains `approve <ref>`)
    //    is not the owner and decides nothing. The owner's `approvals`
    //    right after it is answered, so the echo has been handled by then.
    deliver(
        &mut socket,
        &dm_event("env-2", BOT_USER, &answer.text, "1700000001.000200", true),
    )
    .await;
    deliver(
        &mut socket,
        &dm_event("env-3", OWNER, "approvals", "1700000001.000300", false),
    )
    .await;
    signal
        .until("the queue", || {
            posts().iter().any(|p| p.text.contains("Pending approvals"))
        })
        .await;
    assert!(contact_posts().is_empty());
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        "pending"
    );

    // 3. The owner approves: exactly one contact send, of the draft.
    deliver(
        &mut socket,
        &dm_event(
            "env-4",
            OWNER,
            &format!("approve {}", &id[..8]),
            "1700000001.000400",
            false,
        ),
    )
    .await;
    signal
        .until("the contact send", || !contact_posts().is_empty())
        .await;
    signal
        .until("the approval answer", || {
            posts().iter().any(|p| p.text.contains("Approved"))
        })
        .await;
    // The owner's approval was the first: had the echo decided anything,
    // this answer would be "Already sent."
    assert!(
        posts().iter().any(|p| p.text == "Approved — sending."),
        "{:?}",
        posts().iter().map(|p| p.text.clone()).collect::<Vec<_>>()
    );
    assert!(!posts().iter().any(|p| p.text.contains("Already sent")));
    let sent = contact_posts();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].channel, "U0000000A");
    assert_eq!(sent[0].text, "Lunch moved to 1pm.");
    assert_eq!(
        store
            .get_action_with_email(&id)
            .unwrap()
            .unwrap()
            .action
            .status,
        ActionStatus::Sent.as_str()
    );
    assert!(
        posts().iter().all(|p| p.channel == DM),
        "the app bot only ever wrote to the owner"
    );

    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("surface stops")
        .expect("no panic")
        .expect("clean stop");
}
