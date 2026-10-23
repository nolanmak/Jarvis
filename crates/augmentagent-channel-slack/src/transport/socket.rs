//! Socket Mode client: one outbound WebSocket, typed envelopes, acks after
//! hand-off, heartbeat-based dead-socket detection and bounded-jitter
//! reconnects. Runs inside the daemon process; no sidecar.
//!
//! Lifecycle of one envelope:
//!
//! 1. A text frame arrives and is parsed with [`parse_envelope`].
//! 2. `hello` updates [`ConnectionState`]; `disconnect` starts a drain and
//!    then a reconnect; anything with an `envelope_id` becomes a
//!    [`SlackDelivery`].
//! 3. The delivery is handed to the [`SlackEventSink`] concurrently with the
//!    read loop (so pings keep flowing while a slow consumer works), bounded
//!    by [`SocketModeConfig::handoff_timeout`].
//! 4. Only when the sink returns `Ok(ack)` is `{"envelope_id": ...}` written
//!    back. A rejected or timed-out hand-off is never acknowledged, so Slack
//!    redelivers it (Slack retry behaviour is documented for HTTP delivery
//!    and not yet verified for Socket Mode; see `docs/SLACK-TRANSPORT.md`).
//!
//! Dead sockets: the client sends a WebSocket ping every
//! [`SocketModeConfig::ping_interval`] and reconnects when no inbound frame
//! of any kind has arrived for [`SocketModeConfig::dead_after`]. Because the
//! monotonic clock does not advance while a Mac sleeps, the client also
//! compares wall-clock progress against monotonic progress on every tick and
//! reconnects at once when the wall clock jumped by more than
//! [`SocketModeConfig::suspend_threshold`].
//!
//! Sizing: `dead_after` should exceed `handoff_timeout` and `ping_interval`
//! comfortably; the defaults are 45 s / 2.5 s / 15 s.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::backoff::{random_jitter, BackoffConfig};
use super::event::{parse_envelope, DisconnectReason, Envelope, EventEnvelope};
use super::token::{redact, AppLevelToken};

// ---------------------------------------------------------------------------
// Connector
// ---------------------------------------------------------------------------

/// Object-safe I/O bound so tests can hand the client an in-memory pipe.
pub trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncIo for T {}

pub type BoxedWebSocket = WebSocketStream<MaybeTlsStream<Box<dyn AsyncIo>>>;

#[derive(Debug, Error)]
pub enum ConnectError {
    /// Retrying will not help (bad token, wrong token type, plain `ws://`
    /// URL in production). The client stops.
    #[error("fatal: {0}")]
    Fatal(String),
    /// Network or server trouble; the client backs off and retries.
    #[error("transient: {0}")]
    Transient(String),
}

/// Produces a fresh, handshaken WebSocket each time the client (re)connects.
#[async_trait]
pub trait SocketConnector: Send + Sync {
    async fn connect(&self, cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError>;
}

/// Production connector: `apps.connections.open` with the app-level token,
/// then a TLS WebSocket handshake to the returned `wss://` URL.
pub struct SlackConnector {
    app_token: AppLevelToken,
    api_base_url: String,
    http: reqwest::Client,
    connect_timeout: Duration,
    allow_insecure_ws: bool,
}

impl fmt::Debug for SlackConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackConnector")
            .field("app_token", &self.app_token)
            .field("api_base_url", &self.api_base_url)
            .field("connect_timeout", &self.connect_timeout)
            .field("allow_insecure_ws", &self.allow_insecure_ws)
            .finish()
    }
}

impl SlackConnector {
    pub fn new(
        app_token: AppLevelToken,
        api_base_url: impl Into<String>,
    ) -> Result<Self, ConnectError> {
        let connect_timeout = Duration::from_secs(15);
        let http = reqwest::Client::builder()
            .timeout(connect_timeout)
            .build()
            .map_err(|e| ConnectError::Fatal(format!("http client: {e}")))?;
        Ok(Self {
            app_token,
            api_base_url: api_base_url.into(),
            http,
            connect_timeout,
            allow_insecure_ws: false,
        })
    }

    /// Accept `ws://` URLs. Only for tests against a loopback fake.
    pub fn allow_insecure_ws(mut self, allow: bool) -> Self {
        self.allow_insecure_ws = allow;
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// `apps.connections.open` → one-time WebSocket URL. The URL carries a
    /// ticket and must be treated as a secret.
    pub async fn open_connection_url(&self) -> Result<String, ConnectError> {
        let url = format!(
            "{}/apps.connections.open",
            self.api_base_url.trim_end_matches('/')
        );
        let resp = self
            .http
            .post(&url)
            .bearer_auth(self.app_token.expose_secret())
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .send()
            .await
            .map_err(|e| ConnectError::Transient(redact(&e.without_url().to_string())))?;
        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ConnectError::Transient(
                "apps.connections.open rate limited".into(),
            ));
        }
        let body: Value = resp.json().await.map_err(|_| {
            ConnectError::Transient(format!("apps.connections.open: http {status}"))
        })?;
        if body.get("ok") == Some(&Value::Bool(true)) {
            return body
                .get("url")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| ConnectError::Transient("apps.connections.open: no url".into()));
        }
        let error = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        match error {
            "invalid_auth"
            | "not_authed"
            | "account_inactive"
            | "token_revoked"
            | "token_expired"
            | "not_allowed_token_type"
            | "invalid_token"
            | "missing_scope"
            | "access_denied" => Err(ConnectError::Fatal(format!(
                "apps.connections.open: {error}"
            ))),
            other => Err(ConnectError::Transient(format!(
                "apps.connections.open: {other}"
            ))),
        }
    }
}

#[async_trait]
impl SocketConnector for SlackConnector {
    async fn connect(&self, cancel: &CancellationToken) -> Result<BoxedWebSocket, ConnectError> {
        let url = tokio::select! {
            _ = cancel.cancelled() => return Err(ConnectError::Transient("cancelled".into())),
            r = self.open_connection_url() => r?,
        };
        let request = url
            .as_str()
            .into_client_request()
            .map_err(|e| ConnectError::Transient(redact(&e.to_string())))?;
        let uri = request.uri().clone();
        let scheme = uri.scheme_str().unwrap_or("");
        let secure = match scheme {
            "wss" => true,
            "ws" if self.allow_insecure_ws => false,
            _ => {
                return Err(ConnectError::Fatal(format!(
                    "refusing non-wss socket URL (scheme `{scheme}`)"
                )))
            }
        };
        let host = uri
            .host()
            .ok_or_else(|| ConnectError::Transient("socket URL has no host".into()))?
            .to_string();
        let port = uri.port_u16().unwrap_or(if secure { 443 } else { 80 });
        let handshake = async {
            let tcp = tokio::net::TcpStream::connect((host.as_str(), port))
                .await
                .map_err(|e| {
                    ConnectError::Transient(format!("tcp connect to {host}:{port}: {e}"))
                })?;
            let _ = tcp.set_nodelay(true);
            let boxed: Box<dyn AsyncIo> = Box::new(tcp);
            let (ws, _response) =
                tokio_tungstenite::client_async_tls_with_config(request, boxed, None, None)
                    .await
                    .map_err(|e| {
                        ConnectError::Transient(redact(&format!("websocket handshake: {e}")))
                    })?;
            Ok::<_, ConnectError>(ws)
        };
        tokio::select! {
            _ = cancel.cancelled() => Err(ConnectError::Transient("cancelled".into())),
            r = tokio::time::timeout(self.connect_timeout, handshake) => match r {
                Ok(r) => r,
                Err(_) => Err(ConnectError::Transient("websocket connect timed out".into())),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Sink (consumer side)
// ---------------------------------------------------------------------------

/// What the consumer returns once it owns the event. Slack accepts an
/// optional response payload for interactions and slash commands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ack {
    pub payload: Option<Value>,
}

impl Ack {
    pub fn empty() -> Self {
        Self { payload: None }
    }

    pub fn with_payload(payload: Value) -> Self {
        Self {
            payload: Some(payload),
        }
    }
}

#[derive(Debug, Error)]
pub enum HandoffError {
    /// The consumer could not take ownership (e.g. store write failed). The
    /// envelope is left unacknowledged.
    #[error("handoff rejected: {0}")]
    Rejected(String),
}

/// One envelope handed to the consumer.
#[derive(Debug, Clone, PartialEq)]
pub struct SlackDelivery {
    pub envelope: EventEnvelope,
    /// `true` when this connection already saw the same
    /// [`EventEnvelope::stable_id`] recently. Durable dedupe is #1285's job;
    /// this is a hint, not a guarantee.
    pub seen_before: bool,
    /// 1-based index of the connection that received it.
    pub connection: u64,
    pub received_at: SystemTime,
}

/// Consumer of deliveries. Must be safe to call concurrently. Returning
/// `Ok` means "I own this now; acknowledge it to Slack".
#[async_trait]
pub trait SlackEventSink: Send + Sync {
    async fn deliver(&self, delivery: SlackDelivery) -> Result<Ack, HandoffError>;
}

/// Convenience: a bounded channel is a sink that acknowledges once the
/// delivery is queued.
#[async_trait]
impl SlackEventSink for tokio::sync::mpsc::Sender<SlackDelivery> {
    async fn deliver(&self, delivery: SlackDelivery) -> Result<Ack, HandoffError> {
        self.send(delivery)
            .await
            .map(|_| Ack::empty())
            .map_err(|_| HandoffError::Rejected("consumer channel closed".into()))
    }
}

// ---------------------------------------------------------------------------
// Clock, config, state, metrics
// ---------------------------------------------------------------------------

/// Wall clock, injectable so sleep/wake can be simulated.
pub trait WallClock: Send + Sync {
    fn now(&self) -> SystemTime;
}

#[derive(Debug, Default)]
pub struct SystemWallClock;

impl WallClock for SystemWallClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketModeConfig {
    /// How often to send a WebSocket ping.
    pub ping_interval: Duration,
    /// No inbound frame for this long → the socket is dead; reconnect.
    pub dead_after: Duration,
    /// Upper bound for one `SlackEventSink::deliver`; past it the envelope
    /// stays unacknowledged.
    pub handoff_timeout: Duration,
    /// After a `disconnect` frame, how long to wait for in-flight hand-offs
    /// so their acks go out on the old link.
    pub drain_timeout: Duration,
    /// Maximum concurrent hand-offs; reads pause when reached.
    pub max_inflight: usize,
    pub backoff: BackoffConfig,
    /// Wall clock advancing this much more than the monotonic clock between
    /// two ticks means the host was suspended; reconnect immediately.
    pub suspend_threshold: Duration,
    /// How many recent stable ids to remember for `seen_before`.
    pub recent_ids: usize,
}

impl Default for SocketModeConfig {
    fn default() -> Self {
        Self {
            ping_interval: Duration::from_secs(15),
            dead_after: Duration::from_secs(45),
            handoff_timeout: Duration::from_millis(2500),
            drain_timeout: Duration::from_secs(3),
            max_inflight: 64,
            backoff: BackoffConfig::default(),
            suspend_threshold: Duration::from_secs(60),
            recent_ids: 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Idle,
    Connecting {
        attempt: u32,
    },
    Connected {
        connection: u64,
        app_id: Option<String>,
    },
    /// Waiting `delay` before connection attempt `attempt`.
    Backoff {
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    Stopped {
        fatal: Option<String>,
    },
}

impl ConnectionState {
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }
}

/// Counters for status/doctor output. All monotonically increasing.
#[derive(Debug, Default)]
pub struct SocketModeMetrics {
    connections: AtomicU64,
    reconnects: AtomicU64,
    received: AtomicU64,
    acked: AtomicU64,
    handoff_failures: AtomicU64,
    malformed: AtomicU64,
    heartbeat_timeouts: AtomicU64,
    suspend_detections: AtomicU64,
    last_disconnect: Mutex<Option<DisconnectReason>>,
}

macro_rules! counter {
    ($name:ident) => {
        pub fn $name(&self) -> u64 {
            self.$name.load(Ordering::SeqCst)
        }
    };
}

impl SocketModeMetrics {
    counter!(connections);
    counter!(reconnects);
    counter!(received);
    counter!(acked);
    counter!(handoff_failures);
    counter!(malformed);
    counter!(heartbeat_timeouts);
    counter!(suspend_detections);

    pub fn last_disconnect(&self) -> Option<DisconnectReason> {
        self.last_disconnect.lock().unwrap().clone()
    }

    fn bump(&self, c: &AtomicU64) {
        c.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(Debug, Error)]
pub enum SocketModeError {
    #[error("socket mode stopped: {0}")]
    Fatal(String),
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub type JitterSource = Arc<dyn Fn() -> f64 + Send + Sync>;

pub struct SocketModeClient {
    connector: Arc<dyn SocketConnector>,
    sink: Arc<dyn SlackEventSink>,
    config: SocketModeConfig,
    wall_clock: Arc<dyn WallClock>,
    jitter: JitterSource,
    state_tx: watch::Sender<ConnectionState>,
    state_rx: watch::Receiver<ConnectionState>,
    metrics: Arc<SocketModeMetrics>,
}

impl fmt::Debug for SocketModeClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SocketModeClient")
            .field("config", &self.config)
            .field("state", &*self.state_rx.borrow())
            .finish()
    }
}

enum SessionEnd {
    Shutdown,
    Reconnect(String),
    Fatal(String),
}

type Handoff = std::pin::Pin<Box<dyn std::future::Future<Output = HandoffResult> + Send>>;

struct HandoffResult {
    envelope_id: String,
    accepts_payload: bool,
    outcome: Result<Ack, String>,
}

impl SocketModeClient {
    pub fn new(
        connector: Arc<dyn SocketConnector>,
        sink: Arc<dyn SlackEventSink>,
        config: SocketModeConfig,
    ) -> Self {
        let (state_tx, state_rx) = watch::channel(ConnectionState::Idle);
        Self {
            connector,
            sink,
            config,
            wall_clock: Arc::new(SystemWallClock),
            jitter: Arc::new(random_jitter),
            state_tx,
            state_rx,
            metrics: Arc::new(SocketModeMetrics::default()),
        }
    }

    pub fn with_wall_clock(mut self, clock: Arc<dyn WallClock>) -> Self {
        self.wall_clock = clock;
        self
    }

    pub fn with_jitter(mut self, jitter: JitterSource) -> Self {
        self.jitter = jitter;
        self
    }

    /// Live connection state for status/doctor and for callers that want to
    /// wait until the link is up.
    pub fn state(&self) -> watch::Receiver<ConnectionState> {
        self.state_rx.clone()
    }

    pub fn metrics(&self) -> Arc<SocketModeMetrics> {
        Arc::clone(&self.metrics)
    }

    fn set_state(&self, state: ConnectionState) {
        self.state_tx.send_replace(state);
    }

    /// Run until `shutdown` fires (`Ok`) or a fatal condition stops the
    /// client (`Err`). Transient failures reconnect forever with bounded
    /// jittered backoff.
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), SocketModeError> {
        let mut attempt: u32 = 0;
        let mut recent = RecentIds::new(self.config.recent_ids);
        loop {
            if shutdown.is_cancelled() {
                self.set_state(ConnectionState::Stopped { fatal: None });
                return Ok(());
            }
            self.set_state(ConnectionState::Connecting {
                attempt: attempt + 1,
            });
            let connected = tokio::select! {
                _ = shutdown.cancelled() => {
                    self.set_state(ConnectionState::Stopped { fatal: None });
                    return Ok(());
                }
                r = self.connector.connect(&shutdown) => r,
            };
            let end = match connected {
                Err(ConnectError::Fatal(reason)) => SessionEnd::Fatal(reason),
                Err(ConnectError::Transient(reason)) => SessionEnd::Reconnect(reason),
                Ok(ws) => {
                    let connection = self.metrics.connections.fetch_add(1, Ordering::SeqCst) + 1;
                    if connection > 1 {
                        self.metrics.bump(&self.metrics.reconnects);
                    }
                    self.run_session(ws, connection, &shutdown, &mut attempt, &mut recent)
                        .await
                }
            };
            match end {
                SessionEnd::Shutdown => {
                    self.set_state(ConnectionState::Stopped { fatal: None });
                    return Ok(());
                }
                SessionEnd::Fatal(reason) => {
                    warn!(reason = %reason, "slack socket mode stopped");
                    self.set_state(ConnectionState::Stopped {
                        fatal: Some(reason.clone()),
                    });
                    return Err(SocketModeError::Fatal(reason));
                }
                SessionEnd::Reconnect(reason) => {
                    attempt += 1;
                    let delay = self.config.backoff.delay(attempt, (self.jitter)());
                    info!(reason = %reason, attempt, ?delay, "slack socket mode reconnecting");
                    self.set_state(ConnectionState::Backoff {
                        attempt,
                        delay,
                        reason,
                    });
                    tokio::select! {
                        _ = shutdown.cancelled() => {
                            self.set_state(ConnectionState::Stopped { fatal: None });
                            return Ok(());
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
            }
        }
    }

    async fn run_session(
        &self,
        ws: BoxedWebSocket,
        connection: u64,
        shutdown: &CancellationToken,
        attempt: &mut u32,
        recent: &mut RecentIds,
    ) -> SessionEnd {
        let (mut writer, mut reader) = ws.split();
        let mut inflight: FuturesUnordered<Handoff> = FuturesUnordered::new();
        let mut ping_tick = tokio::time::interval(self.config.ping_interval);
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping_tick.tick().await; // first tick fires immediately; consume it
        let mut last_inbound = Instant::now();
        let mut last_tick_mono = Instant::now();
        let mut last_tick_wall = self.wall_clock.now();
        let mut drain_deadline: Option<Instant> = None;

        let end = loop {
            if drain_deadline.is_some() && inflight.is_empty() {
                break SessionEnd::Reconnect("disconnect requested by slack".into());
            }
            let drain_sleep = async {
                match drain_deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = shutdown.cancelled() => break SessionEnd::Shutdown,

                frame = reader.next(), if inflight.len() < self.config.max_inflight => {
                    last_inbound = Instant::now();
                    match frame {
                        None => break SessionEnd::Reconnect("socket closed".into()),
                        Some(Err(e)) => break SessionEnd::Reconnect(redact(&format!("socket error: {e}"))),
                        Some(Ok(Message::Close(_))) => break SessionEnd::Reconnect("close frame".into()),
                        Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
                        Some(Ok(Message::Binary(_))) => debug!("ignoring binary socket mode frame"),
                        Some(Ok(Message::Text(text))) => {
                            match self.handle_text(&text, connection, recent, &mut inflight) {
                                TextOutcome::Continue => {}
                                TextOutcome::Hello(app_id) => {
                                    *attempt = 0;
                                    self.set_state(ConnectionState::Connected { connection, app_id });
                                }
                                TextOutcome::Drain(reason) => {
                                    *self.metrics.last_disconnect.lock().unwrap() = Some(reason.clone());
                                    if reason == DisconnectReason::LinkDisabled {
                                        break SessionEnd::Fatal("slack disconnect: link_disabled".into());
                                    }
                                    info!(reason = reason.as_str(), "slack requested disconnect; draining");
                                    if drain_deadline.is_none() {
                                        drain_deadline = Some(Instant::now() + self.config.drain_timeout);
                                    }
                                }
                            }
                        }
                    }
                }

                Some(done) = inflight.next(), if !inflight.is_empty() => {
                    match done.outcome {
                        Ok(ack) => {
                            let mut frame = json!({"envelope_id": done.envelope_id});
                            if done.accepts_payload {
                                if let Some(p) = ack.payload {
                                    frame["payload"] = p;
                                }
                            }
                            if let Err(e) = writer.send(Message::Text(frame.to_string())).await {
                                break SessionEnd::Reconnect(redact(&format!("ack write failed: {e}")));
                            }
                            self.metrics.bump(&self.metrics.acked);
                        }
                        Err(reason) => {
                            self.metrics.bump(&self.metrics.handoff_failures);
                            warn!(envelope_id = %done.envelope_id, reason = %reason,
                                "slack envelope not acknowledged; slack may redeliver");
                        }
                    }
                }

                _ = ping_tick.tick() => {
                    let now = Instant::now();
                    let wall = self.wall_clock.now();
                    let mono_gap = now.saturating_duration_since(last_tick_mono);
                    let wall_gap = wall.duration_since(last_tick_wall).unwrap_or_default();
                    last_tick_mono = now;
                    last_tick_wall = wall;
                    if wall_gap > mono_gap + self.config.suspend_threshold {
                        self.metrics.bump(&self.metrics.suspend_detections);
                        break SessionEnd::Reconnect(format!(
                            "host suspend detected (wall clock advanced {wall_gap:?}, monotonic {mono_gap:?})"
                        ));
                    }
                    if now.saturating_duration_since(last_inbound) > self.config.dead_after {
                        self.metrics.bump(&self.metrics.heartbeat_timeouts);
                        break SessionEnd::Reconnect(format!(
                            "no frame for {:?}; socket presumed dead", self.config.dead_after
                        ));
                    }
                    if let Err(e) = writer.send(Message::Ping(Vec::new())).await {
                        break SessionEnd::Reconnect(redact(&format!("ping write failed: {e}")));
                    }
                }

                _ = drain_sleep => {
                    break SessionEnd::Reconnect(format!(
                        "disconnect requested; {} hand-off(s) still pending after drain timeout",
                        inflight.len()
                    ));
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(500), close(writer, reader)).await;
        end
    }

    fn handle_text(
        &self,
        text: &str,
        connection: u64,
        recent: &mut RecentIds,
        inflight: &mut FuturesUnordered<Handoff>,
    ) -> TextOutcome {
        match parse_envelope(text) {
            Err(e) => {
                self.metrics.bump(&self.metrics.malformed);
                warn!(error = %e, "ignoring malformed socket mode frame");
                TextOutcome::Continue
            }
            Ok(Envelope::Hello(h)) => {
                info!(app_id = ?h.app_id, num_connections = ?h.num_connections,
                    approximate_connection_time_secs = ?h.approximate_connection_time_secs,
                    "slack socket mode connected");
                TextOutcome::Hello(h.app_id)
            }
            Ok(Envelope::Disconnect(d)) => TextOutcome::Drain(d.reason),
            Ok(Envelope::Event(envelope)) => {
                self.metrics.bump(&self.metrics.received);
                let seen_before = recent.insert(envelope.stable_id());
                let delivery = SlackDelivery {
                    seen_before,
                    connection,
                    received_at: self.wall_clock.now(),
                    envelope: *envelope,
                };
                let envelope_id = delivery.envelope.envelope_id.clone();
                let accepts_payload = delivery.envelope.accepts_response_payload;
                let sink = Arc::clone(&self.sink);
                let timeout = self.config.handoff_timeout;
                inflight.push(Box::pin(async move {
                    let outcome = match tokio::time::timeout(timeout, sink.deliver(delivery)).await
                    {
                        Ok(Ok(ack)) => Ok(ack),
                        Ok(Err(e)) => Err(e.to_string()),
                        Err(_) => Err(format!("hand-off exceeded {timeout:?}")),
                    };
                    HandoffResult {
                        envelope_id,
                        accepts_payload,
                        outcome,
                    }
                }));
                TextOutcome::Continue
            }
        }
    }
}

enum TextOutcome {
    Continue,
    Hello(Option<String>),
    Drain(DisconnectReason),
}

async fn close(writer: SplitSink<BoxedWebSocket, Message>, reader: SplitStream<BoxedWebSocket>) {
    if let Ok(mut ws) = writer.reunite(reader) {
        let _ = ws.close(None).await;
    }
}

/// Fixed-size ring of recently seen stable ids.
struct RecentIds {
    ring: std::collections::VecDeque<String>,
    set: std::collections::HashSet<String>,
    cap: usize,
}

impl RecentIds {
    fn new(cap: usize) -> Self {
        Self {
            ring: std::collections::VecDeque::with_capacity(cap.min(4096)),
            set: std::collections::HashSet::new(),
            cap: cap.max(1),
        }
    }

    /// Returns `true` if `id` was already present.
    fn insert(&mut self, id: String) -> bool {
        if self.set.contains(&id) {
            return true;
        }
        if self.ring.len() >= self.cap {
            if let Some(old) = self.ring.pop_front() {
                self.set.remove(&old);
            }
        }
        self.set.insert(id.clone());
        self.ring.push_back(id);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_ids_evicts_oldest() {
        let mut r = RecentIds::new(2);
        assert!(!r.insert("a".into()));
        assert!(!r.insert("b".into()));
        assert!(r.insert("a".into()));
        assert!(!r.insert("c".into())); // evicts a
        assert!(!r.insert("a".into()));
        assert!(r.insert("c".into()));
    }
}
