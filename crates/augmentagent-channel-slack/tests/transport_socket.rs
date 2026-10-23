//! #1283 — Socket Mode client against an in-process fake Slack.
//!
//! Most tests drive the client over an in-memory duplex pipe under tokio's
//! paused clock, so heartbeat and backoff timing is deterministic and takes
//! no real time. One test goes through the real `SlackConnector` against a
//! loopback `apps.connections.open` mock and a loopback WebSocket listener.
//! Tokens and IDs are synthetic (`xapp-test-000`, `T00000001`).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use augmentagent_channel_slack::transport::backoff::BackoffConfig;
use augmentagent_channel_slack::transport::event::{DisconnectReason, SlackEvent};
use augmentagent_channel_slack::transport::socket::{
    Ack, AsyncIo, BoxedWebSocket, ConnectError, ConnectionState, HandoffError, SlackConnector,
    SlackDelivery, SlackEventSink, SocketConnector, SocketModeClient, SocketModeConfig,
    SocketModeError, WallClock,
};
use augmentagent_channel_slack::transport::token::AppLevelToken;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::DuplexStream;
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

const APP_LEVEL: &str = "xapp-test-000";

/// `Result::unwrap_err` needs `T: Debug`, which a live socket is not.
async fn connect_err(connector: &SlackConnector) -> ConnectError {
    match connector.connect(&CancellationToken::new()).await {
        Err(e) => e,
        Ok(_) => panic!("connect unexpectedly succeeded"),
    }
}

type ServerSocket = WebSocketStream<DuplexStream>;

/// Hands the client one in-memory WebSocket per `connect`, and the matching
/// server half to the test. `fail_first` connects fail transiently.
struct DuplexConnector {
    servers: mpsc::UnboundedSender<ServerSocket>,
    connects: AtomicU32,
    fail_first: u32,
    fatal: bool,
    connect_times: Mutex<Vec<tokio::time::Instant>>,
}

impl DuplexConnector {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<ServerSocket>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                servers: tx,
                connects: AtomicU32::new(0),
                fail_first: 0,
                fatal: false,
                connect_times: Mutex::new(Vec::new()),
            }),
            rx,
        )
    }
}

#[async_trait]
impl SocketConnector for DuplexConnector {
    async fn connect(&self, _cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError> {
        let n = self.connects.fetch_add(1, Ordering::SeqCst) + 1;
        self.connect_times
            .lock()
            .unwrap()
            .push(tokio::time::Instant::now());
        if self.fatal {
            return Err(ConnectError::Fatal("invalid_auth".into()));
        }
        if n <= self.fail_first {
            return Err(ConnectError::Transient(format!("simulated failure {n}")));
        }
        let (client_half, server_half) = tokio::io::duplex(256 * 1024);
        let server = WebSocketStream::from_raw_socket(server_half, Role::Server, None).await;
        self.servers.send(server).expect("test still listening");
        let boxed: Box<dyn AsyncIo> = Box::new(client_half);
        Ok(
            WebSocketStream::from_raw_socket(MaybeTlsStream::Plain(boxed), Role::Client, None)
                .await,
        )
    }
}

/// Records deliveries; optionally holds each one until `release` is notified.
struct RecordingSink {
    deliveries: Mutex<Vec<SlackDelivery>>,
    gate: Option<Arc<Notify>>,
    ack_payload: Mutex<Option<Value>>,
    fail: std::sync::atomic::AtomicBool,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            deliveries: Mutex::new(Vec::new()),
            gate: None,
            ack_payload: Mutex::new(None),
            fail: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn gated() -> (Arc<Self>, Arc<Notify>) {
        let gate = Arc::new(Notify::new());
        (
            Arc::new(Self {
                deliveries: Mutex::new(Vec::new()),
                gate: Some(gate.clone()),
                ack_payload: Mutex::new(None),
                fail: std::sync::atomic::AtomicBool::new(false),
            }),
            gate,
        )
    }

    fn stable_ids(&self) -> Vec<String> {
        self.deliveries
            .lock()
            .unwrap()
            .iter()
            .map(|d| d.envelope.stable_id())
            .collect()
    }
}

#[async_trait]
impl SlackEventSink for RecordingSink {
    async fn deliver(&self, delivery: SlackDelivery) -> Result<Ack, HandoffError> {
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        self.deliveries.lock().unwrap().push(delivery);
        if self.fail.load(Ordering::SeqCst) {
            return Err(HandoffError::Rejected("simulated store failure".into()));
        }
        match self.ack_payload.lock().unwrap().clone() {
            Some(p) => Ok(Ack::with_payload(p)),
            None => Ok(Ack::empty()),
        }
    }
}

/// Wall clock the test can jump forward to simulate a laptop sleeping.
struct FakeWallClock {
    offset_ms: AtomicU64,
}

impl WallClock for FakeWallClock {
    fn now(&self) -> SystemTime {
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(1_800_000_000)
            + Duration::from_millis(self.offset_ms.load(Ordering::SeqCst))
    }
}

fn fast_config() -> SocketModeConfig {
    SocketModeConfig {
        ping_interval: Duration::from_secs(10),
        dead_after: Duration::from_secs(30),
        handoff_timeout: Duration::from_secs(2),
        drain_timeout: Duration::from_secs(3),
        max_inflight: 8,
        backoff: BackoffConfig {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(16),
        },
        suspend_threshold: Duration::from_secs(60),
        recent_ids: 128,
    }
}

fn hello() -> Message {
    Message::Text(
        json!({"type": "hello", "connection_info": {"app_id": "A00000001"}, "num_connections": 1,
            "debug_info": {"host": "applink-test", "approximate_connection_time": 3600}})
        .to_string(),
    )
}

fn message_envelope(envelope_id: &str, event_id: &str, retry_attempt: u32) -> Message {
    Message::Text(
        json!({
            "envelope_id": envelope_id,
            "type": "events_api",
            "accepts_response_payload": false,
            "retry_attempt": retry_attempt,
            "retry_reason": if retry_attempt > 0 { "timeout" } else { "" },
            "payload": {
                "team_id": "T00000001", "api_app_id": "A00000001", "type": "event_callback",
                "event_id": event_id, "event_time": 1_700_000_000,
                "event": {"type": "message", "channel": "C00000001", "user": "U00000001",
                    "text": "hi", "ts": "1700000000.000100"}
            }
        })
        .to_string(),
    )
}

fn disconnect(reason: &str) -> Message {
    Message::Text(
        json!({"type": "disconnect", "reason": reason, "debug_info": {"host": "wss-test"}})
            .to_string(),
    )
}

/// Next text frame from the server side, or `None` if nothing arrives within
/// `wait` (virtual time under the paused clock).
async fn next_text(server: &mut ServerSocket, wait: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(wait, server.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => return serde_json::from_str(&t).ok(),
            Ok(Some(Ok(Message::Ping(p)))) => {
                let _ = server.send(Message::Pong(p)).await;
            }
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(_))) | Ok(None) | Err(_) => return None,
        }
    }
}

/// Wait for the client's next ping and answer it.
async fn answer_one_ping(server: &mut ServerSocket) {
    loop {
        match tokio::time::timeout(Duration::from_secs(30), server.next()).await {
            Ok(Some(Ok(Message::Ping(p)))) => {
                server.send(Message::Pong(p)).await.unwrap();
                return;
            }
            Ok(Some(Ok(_))) => continue,
            other => panic!("no ping: {other:?}"),
        }
    }
}

async fn wait_for_state(
    rx: &mut tokio::sync::watch::Receiver<ConnectionState>,
    pred: impl Fn(&ConnectionState) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            if pred(&rx.borrow()) {
                return;
            }
            rx.changed().await.expect("client alive");
        }
    })
    .await
    .unwrap_or_else(|_| panic!("state never matched; last = {:?}", *rx.borrow()));
}

#[tokio::test(start_paused = true)]
async fn envelope_is_acknowledged_only_after_the_sink_accepts_it() {
    let (connector, mut servers) = DuplexConnector::new();
    let (sink, gate) = RecordingSink::gated();
    let client = SocketModeClient::new(connector.clone(), sink.clone(), fast_config());
    let mut state = client.state();
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.expect("first connection");
    server.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    server
        .send(message_envelope("env-0001", "Ev00000001", 0))
        .await
        .unwrap();

    // The sink has not accepted it yet: no ack may be on the wire.
    assert!(
        next_text(&mut server, Duration::from_millis(500))
            .await
            .is_none(),
        "acked before handoff"
    );
    assert_eq!(metrics.acked(), 0);
    assert!(sink.deliveries.lock().unwrap().is_empty());

    gate.notify_one();
    let ack = next_text(&mut server, Duration::from_secs(5))
        .await
        .expect("ack after handoff");
    assert_eq!(ack["envelope_id"], "env-0001");
    assert!(ack.get("payload").is_none());
    assert_eq!(metrics.acked(), 1);
    assert_eq!(sink.stable_ids(), vec!["Ev00000001".to_string()]);

    shutdown.cancel();
    assert!(run.await.unwrap().is_ok());
    assert!(matches!(
        *state.borrow(),
        ConnectionState::Stopped { fatal: None }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_rejected_handoff_is_not_acknowledged() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    sink.fail.store(true, Ordering::SeqCst);
    let client = SocketModeClient::new(connector, sink.clone(), fast_config());
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server
        .send(message_envelope("env-0001", "Ev00000001", 0))
        .await
        .unwrap();
    assert!(
        next_text(&mut server, Duration::from_secs(5))
            .await
            .is_none(),
        "must not ack a rejected handoff"
    );
    assert_eq!(metrics.handoff_failures(), 1);
    assert_eq!(metrics.acked(), 0);
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn slow_handoff_past_the_deadline_is_not_acknowledged() {
    let (connector, mut servers) = DuplexConnector::new();
    let (sink, _gate) = RecordingSink::gated(); // never released
    let client = SocketModeClient::new(connector, sink.clone(), fast_config());
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server
        .send(message_envelope("env-0001", "Ev00000001", 0))
        .await
        .unwrap();
    assert!(next_text(&mut server, Duration::from_secs(5))
        .await
        .is_none());
    assert_eq!(
        metrics.handoff_failures(),
        1,
        "handoff must time out, not hang"
    );
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn redelivery_is_surfaced_with_a_stable_event_id() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let client = SocketModeClient::new(connector, sink.clone(), fast_config());
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server
        .send(message_envelope("env-0001", "Ev00000001", 0))
        .await
        .unwrap();
    assert_eq!(
        next_text(&mut server, Duration::from_secs(5))
            .await
            .unwrap()["envelope_id"],
        "env-0001"
    );
    // Slack redelivers under a new envelope id but the same event id.
    server
        .send(message_envelope("env-0002", "Ev00000001", 1))
        .await
        .unwrap();
    assert_eq!(
        next_text(&mut server, Duration::from_secs(5))
            .await
            .unwrap()["envelope_id"],
        "env-0002"
    );

    {
        let deliveries = sink.deliveries.lock().unwrap();
        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].envelope.stable_id(), "Ev00000001");
        assert_eq!(deliveries[1].envelope.stable_id(), "Ev00000001");
        assert!(!deliveries[0].seen_before);
        assert!(deliveries[1].seen_before, "second delivery must be flagged");
        assert!(deliveries[1].envelope.is_redelivery());
        assert_eq!(
            deliveries[1].envelope.retry_reason.as_deref(),
            Some("timeout")
        );
    }
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn unknown_and_malformed_frames_do_not_drop_the_connection() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let client = SocketModeClient::new(connector.clone(), sink.clone(), fast_config());
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server
        .send(Message::Text("{not json".into()))
        .await
        .unwrap();
    server
        .send(Message::Text(
            json!({"type": "events_api", "payload": {}}).to_string(),
        ))
        .await
        .unwrap();
    server.send(Message::Binary(vec![1, 2, 3])).await.unwrap();
    // Unknown envelope kind with an id: ack it, surface it as Unknown.
    server
        .send(Message::Text(
            json!({"envelope_id": "env-0001", "type": "future_kind", "payload": {"x": 1}})
                .to_string(),
        ))
        .await
        .unwrap();
    // Known envelope kind with an unknown event type.
    server
        .send(Message::Text(
            json!({"envelope_id": "env-0002", "type": "events_api", "payload": {
                "team_id": "T00000001", "event_id": "Ev00000002", "event": {"type": "never_seen"}}})
            .to_string(),
        ))
        .await
        .unwrap();
    server
        .send(message_envelope("env-0003", "Ev00000003", 0))
        .await
        .unwrap();

    let mut acked = Vec::new();
    for _ in 0..3 {
        acked.push(
            next_text(&mut server, Duration::from_secs(5))
                .await
                .expect("ack")["envelope_id"]
                .clone(),
        );
    }
    acked.sort_by_key(|v| v.as_str().unwrap().to_string());
    assert_eq!(
        acked,
        vec![json!("env-0001"), json!("env-0002"), json!("env-0003")]
    );
    assert_eq!(metrics.malformed(), 2);
    assert_eq!(
        connector.connects.load(Ordering::SeqCst),
        1,
        "connection must survive"
    );
    {
        let deliveries = sink.deliveries.lock().unwrap();
        assert!(
            matches!(deliveries[0].envelope.event, SlackEvent::Unknown { ref kind, .. } if kind == "future_kind")
        );
        assert!(
            matches!(deliveries[1].envelope.event, SlackEvent::Unknown { ref kind, .. } if kind == "never_seen")
        );
    }
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn ack_carries_the_response_payload_when_slack_accepts_one() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    *sink.ack_payload.lock().unwrap() = Some(json!({"text": "working on it"}));
    let client = SocketModeClient::new(connector, sink.clone(), fast_config());
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server
        .send(Message::Text(
            json!({"envelope_id": "env-0001", "type": "slash_commands", "accepts_response_payload": true,
                "payload": {"command": "/jarvis", "text": "status", "user_id": "U00000001",
                    "team_id": "T00000001", "channel_id": "C00000001", "trigger_id": "1.2.abc"}})
            .to_string(),
        ))
        .await
        .unwrap();
    let ack = next_text(&mut server, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(ack["envelope_id"], "env-0001");
    assert_eq!(ack["payload"]["text"], "working on it");

    // Payload is dropped when the envelope does not accept one.
    server
        .send(message_envelope("env-0002", "Ev00000002", 0))
        .await
        .unwrap();
    let ack = next_text(&mut server, Duration::from_secs(5))
        .await
        .unwrap();
    assert!(ack.get("payload").is_none(), "{ack}");
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn refresh_request_flushes_pending_acks_then_reconnects() {
    let (connector, mut servers) = DuplexConnector::new();
    let (sink, gate) = RecordingSink::gated();
    let client = SocketModeClient::new(connector.clone(), sink.clone(), fast_config());
    let mut state = client.state();
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server
        .send(message_envelope("env-0001", "Ev00000001", 0))
        .await
        .unwrap();
    server.send(disconnect("refresh_requested")).await.unwrap();
    // Handoff still in flight; the client must wait for it before leaving.
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.notify_one();
    let ack = next_text(&mut server, Duration::from_secs(5))
        .await
        .expect("ack flushed on old link");
    assert_eq!(ack["envelope_id"], "env-0001");
    // Old link closes, new one opens.
    assert!(next_text(&mut server, Duration::from_secs(10))
        .await
        .is_none());
    let mut server2 = servers.recv().await.expect("reconnect");
    server2.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { connection: 2, .. })
    })
    .await;
    assert_eq!(metrics.reconnects(), 1);
    assert_eq!(
        metrics.last_disconnect(),
        Some(DisconnectReason::RefreshRequested)
    );
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn forced_close_reconnects_with_bounded_jitter() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let client =
        SocketModeClient::new(connector.clone(), sink, fast_config()).with_jitter(Arc::new(|| 0.5));
    let mut state = client.state();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    server.close(None).await.unwrap();
    drop(server);
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Backoff { attempt: 1, .. })
    })
    .await;
    let mut server2 = servers.recv().await.expect("reconnect");
    let times = connector.connect_times.lock().unwrap().clone();
    let gap = times[1] - times[0];
    // jitter 0.5 with initial 1s: 0.75s, bounded by [0.5s, 1s].
    assert!(
        gap >= Duration::from_millis(500) && gap <= Duration::from_secs(1),
        "{gap:?}"
    );
    server2.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { connection: 2, .. })
    })
    .await;
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn transient_connect_failures_back_off_and_stay_bounded() {
    let (mut connector, mut servers) = DuplexConnector::new();
    Arc::get_mut(&mut connector).unwrap().fail_first = 6;
    let sink = RecordingSink::new();
    let mut config = fast_config();
    config.backoff = BackoffConfig {
        initial: Duration::from_secs(1),
        max: Duration::from_secs(8),
    };
    let client =
        SocketModeClient::new(connector.clone(), sink, config).with_jitter(Arc::new(|| 0.999));
    let mut state = client.state();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.expect("eventually connects");
    server.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    let times = connector.connect_times.lock().unwrap().clone();
    assert_eq!(times.len(), 7);
    let gaps: Vec<Duration> = times.windows(2).map(|w| w[1] - w[0]).collect();
    for (i, gap) in gaps.iter().enumerate() {
        let cap = Duration::from_secs(1 << i).min(Duration::from_secs(8));
        assert!(
            *gap <= cap && *gap >= cap / 2,
            "gap {i} = {gap:?}, cap {cap:?}"
        );
    }
    assert_eq!(
        gaps[5],
        Duration::from_secs(8).mul_f64(0.9995),
        "capped at max"
    );
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn heartbeat_detects_a_dead_socket_without_tcp_timeout() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let client = SocketModeClient::new(connector.clone(), sink, fast_config());
    let mut state = client.state();
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    // The far side goes silent: the pipe stays open but nothing answers pings.
    // (`server` is kept alive and never polled, so no pong is ever written.)
    let started = tokio::time::Instant::now();
    wait_for_state(&mut state, |s| matches!(s, ConnectionState::Backoff { .. })).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(30) && elapsed <= Duration::from_secs(45),
        "dead socket must be detected within dead_after + one ping interval; took {elapsed:?}"
    );
    assert_eq!(metrics.heartbeat_timeouts(), 1);
    drop(server);
    let mut server2 = servers.recv().await.expect("reconnect after dead socket");
    server2.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { connection: 2, .. })
    })
    .await;
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_healthy_socket_is_kept_alive_by_pings_and_pongs() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let client = SocketModeClient::new(connector.clone(), sink, fast_config());
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    // Answer pings for two minutes of virtual time; no reconnect may happen.
    let mut pings = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(15), server.next()).await {
            Ok(Some(Ok(Message::Ping(p)))) => {
                pings += 1;
                server.send(Message::Pong(p)).await.unwrap();
            }
            Ok(Some(Ok(_))) => {}
            Ok(_) => panic!("socket closed"),
            Err(_) => panic!("no ping within 15s"),
        }
    }
    assert!(pings >= 10, "pings = {pings}");
    assert_eq!(connector.connects.load(Ordering::SeqCst), 1);
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn wall_clock_jump_after_sleep_forces_an_immediate_reconnect() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let wall = Arc::new(FakeWallClock {
        offset_ms: AtomicU64::new(0),
    });
    let client =
        SocketModeClient::new(connector.clone(), sink, fast_config()).with_wall_clock(wall.clone());
    let mut state = client.state();
    let metrics = client.metrics();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    // Answer one ping so the link is provably healthy, then "sleep" 3 hours:
    // the monotonic clock barely moves, the wall clock jumps.
    answer_one_ping(&mut server).await;
    wall.offset_ms.store(3 * 3600 * 1000, Ordering::SeqCst);
    let started = tokio::time::Instant::now();
    wait_for_state(&mut state, |s| matches!(s, ConnectionState::Backoff { .. })).await;
    assert!(
        started.elapsed() <= Duration::from_secs(11),
        "must not wait for dead_after: {:?}",
        started.elapsed()
    );
    assert_eq!(metrics.suspend_detections(), 1);
    drop(server);
    let mut server2 = servers.recv().await.expect("reconnect after wake");
    server2.send(hello()).await.unwrap();
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { connection: 2, .. })
    })
    .await;
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn link_disabled_and_fatal_connect_errors_stop_the_client() {
    let (connector, mut servers) = DuplexConnector::new();
    let sink = RecordingSink::new();
    let client = SocketModeClient::new(connector, sink.clone(), fast_config());
    let mut state = client.state();
    let run = tokio::spawn(client.run(CancellationToken::new()));
    let mut server = servers.recv().await.unwrap();
    server.send(hello()).await.unwrap();
    server.send(disconnect("link_disabled")).await.unwrap();
    let err = run.await.unwrap().unwrap_err();
    assert!(
        matches!(err, SocketModeError::Fatal(ref r) if r.contains("link_disabled")),
        "{err:?}"
    );
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Stopped { fatal: Some(_) })
    })
    .await;

    let (mut connector, _servers) = DuplexConnector::new();
    Arc::get_mut(&mut connector).unwrap().fatal = true;
    let client = SocketModeClient::new(connector, sink, fast_config());
    let err = client.run(CancellationToken::new()).await.unwrap_err();
    assert!(
        matches!(err, SocketModeError::Fatal(ref r) if r.contains("invalid_auth")),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// Real connector over loopback: apps.connections.open + WebSocket handshake.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn slack_connector_fetches_the_url_and_completes_the_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!(
        "ws://{}/link/?ticket=test-ticket-0001&app_id=A00000001",
        listener.local_addr().unwrap()
    );
    let mut api = mockito::Server::new_async().await;
    let open = api
        .mock("POST", "/apps.connections.open")
        .match_header("authorization", format!("Bearer {APP_LEVEL}").as_str())
        .with_body(json!({"ok": true, "url": ws_url}).to_string())
        .expect(1)
        .create_async()
        .await;

    let server_task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.send(hello()).await.unwrap();
        ws.send(message_envelope("env-0001", "Ev00000001", 0))
            .await
            .unwrap();
        let mut ack = None;
        while let Some(Ok(msg)) = ws.next().await {
            if let Message::Text(t) = msg {
                ack = Some(serde_json::from_str::<Value>(&t).unwrap());
                break;
            }
        }
        ack
    });

    let connector = SlackConnector::new(AppLevelToken::new(APP_LEVEL), api.url())
        .unwrap()
        .allow_insecure_ws(true);
    assert!(!format!("{connector:?}").contains(APP_LEVEL));
    let sink = RecordingSink::new();
    let mut config = fast_config();
    config.ping_interval = Duration::from_millis(50);
    config.dead_after = Duration::from_millis(500);
    let client = SocketModeClient::new(Arc::new(connector), sink.clone(), config);
    let mut state = client.state();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));

    let ack = tokio::time::timeout(Duration::from_secs(10), server_task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ack.expect("ack")["envelope_id"], "env-0001");
    wait_for_state(&mut state, |s| {
        matches!(s, ConnectionState::Connected { .. })
    })
    .await;
    assert_eq!(sink.stable_ids(), vec!["Ev00000001".to_string()]);
    open.assert_async().await;
    shutdown.cancel();
    run.await.unwrap().unwrap();
}

#[tokio::test]
async fn slack_connector_treats_auth_errors_as_fatal_and_never_leaks_secrets() {
    let mut api = mockito::Server::new_async().await;
    api.mock("POST", "/apps.connections.open")
        .with_body(json!({"ok": false, "error": "invalid_auth"}).to_string())
        .create_async()
        .await;
    let connector = SlackConnector::new(AppLevelToken::new(APP_LEVEL), api.url()).unwrap();
    let err = connect_err(&connector).await;
    assert!(
        matches!(err, ConnectError::Fatal(ref r) if r.contains("invalid_auth")),
        "{err:?}"
    );
    assert!(!format!("{err:?}").contains(APP_LEVEL));

    // A URL that cannot be dialled: the error must not echo the ticket.
    api.reset();
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_url = format!(
        "ws://{}/link/?ticket=secret-ticket-0002",
        closed.local_addr().unwrap()
    );
    drop(closed);
    api.mock("POST", "/apps.connections.open")
        .with_body(json!({"ok": true, "url": dead_url}).to_string())
        .create_async()
        .await;
    let connector = SlackConnector::new(AppLevelToken::new(APP_LEVEL), api.url())
        .unwrap()
        .allow_insecure_ws(true);
    let err = connect_err(&connector).await;
    let text = format!("{err:?}");
    assert!(matches!(err, ConnectError::Transient(_)), "{text}");
    assert!(!text.contains("secret-ticket-0002"), "{text}");
    assert!(!text.contains(APP_LEVEL));

    // Plain ws:// is refused unless explicitly allowed (tests only).
    let connector = SlackConnector::new(AppLevelToken::new(APP_LEVEL), api.url()).unwrap();
    let err = connect_err(&connector).await;
    assert!(
        matches!(err, ConnectError::Fatal(ref r) if r.contains("wss")),
        "{err:?}"
    );
}

/// Opt-in live probe. Needs a test workspace app with Socket Mode enabled:
/// `SLACK_TEST_APP_TOKEN=xapp-... cargo test -p augmentagent-channel-slack --test transport_socket -- --ignored live_`
#[tokio::test]
#[ignore = "live probe against a Slack test workspace; set SLACK_TEST_APP_TOKEN"]
async fn live_socket_mode_hello_roundtrip() {
    let Ok(token) = std::env::var("SLACK_TEST_APP_TOKEN") else {
        eprintln!("SLACK_TEST_APP_TOKEN not set; skipping");
        return;
    };
    let connector =
        SlackConnector::new(AppLevelToken::new(token), "https://slack.com/api").unwrap();
    let sink = RecordingSink::new();
    let client = SocketModeClient::new(Arc::new(connector), sink, SocketModeConfig::default());
    let mut state = client.state();
    let shutdown = CancellationToken::new();
    let run = tokio::spawn(client.run(shutdown.clone()));
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let ConnectionState::Connected { ref app_id, .. } = *state.borrow() {
                eprintln!("connected; app_id={app_id:?}");
                return;
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .expect("connected within 30s");
    shutdown.cancel();
    run.await.unwrap().unwrap();
}
