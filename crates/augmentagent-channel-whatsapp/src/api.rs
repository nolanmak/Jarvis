//! JSON-RPC client over a Unix domain socket talking to the whatsmeow Go
//! sidecar (`augmentagent-wa-sidecar`).
//!
//! ## Wire protocol (mirrors `sidecars/browser/sidecar.py` §6 NDJSON/UDS)
//!
//! The sidecar listens on `${XDG_RUNTIME_DIR}/augmentagent/wa.sock`. Two
//! frame families share the single connection, one JSON object per line
//! (`\n`-terminated, compact):
//!
//! **Request / response (methods):**
//!
//! ```text
//! Request  : {"version":1,"request_id":"<uuid>","op":"send_text","params":{...}}
//! Success  : {"version":1,"request_id":"...","ok":true,"result":{...}}
//! Failure  : {"version":1,"request_id":"...","ok":false,
//!             "error":{"kind":"NotPaired"|"NotConnected"|"SendFailed"
//!                            |"BadRequest"|"Internal","message":"..."}}
//! ```
//!
//! **Events (sidecar-initiated, no `request_id`):**
//!
//! ```text
//! {"version":1,"event":"received-message","id":"...","chat":"...","sender":"...",
//!  "push_name":"...","text":"...","timestamp":1776630000,"from_me":false}
//! {"version":1,"event":"qr","code":"2@..."}
//! {"version":1,"event":"pair-success","device_jid":"...","user_jid":"..."}
//! {"version":1,"event":"connected"}
//! {"version":1,"event":"logged-out","reason":"..."}
//! ```
//!
//! The reader task demultiplexes: frames with a `request_id` wake the matching
//! oneshot; frames with an `event` discriminator are pushed onto the event
//! `mpsc` the channel drains. This is the *single* WhatsApp client — both the
//! DM channel (#12) and the control surface (#102) consume the same
//! [`WaClient`] handle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::codec::{FramedRead, LinesCodec};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::types::{WaContact, WaEvent, WaMessage};

const PROTOCOL_VERSION: u32 = 1;
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Default UDS path. Linux uses `${XDG_RUNTIME_DIR}/augmentagent/wa.sock`,
/// then `/run/user/<uid>/...` or a private `/tmp/augmentagent-<uid>/...`.
/// macOS uses the short private `/tmp/augmentagent-<uid>/wa.sock` path.
/// Overridable via `AUGMENTAGENT_WA_SOCK` (parity with the browser sidecar's
/// `AUGMENTAGENT_BROWSER_SOCK`).
pub fn default_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("AUGMENTAGENT_WA_SOCK") {
        if !custom.is_empty() {
            return PathBuf::from(custom);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // Darwin's Unix socket path limit is short. Keep this path stable
        // across terminal and launchd environments, even with a long HOME.
        return macos_socket_path(users_uid());
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(runtime) = std::env::var("XDG_RUNTIME_DIR") {
            if !runtime.is_empty() {
                return PathBuf::from(runtime).join("augmentagent").join("wa.sock");
            }
        }
        let uid = users_uid();
        let runtime = PathBuf::from(format!("/run/user/{uid}"));
        if runtime.is_dir() {
            runtime.join("augmentagent").join("wa.sock")
        } else {
            PathBuf::from(format!("/tmp/augmentagent-{uid}")).join("wa.sock")
        }
    }
}

#[cfg(any(target_os = "macos", test))]
fn macos_socket_path(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/augmentagent-{uid}/wa.sock"))
}

fn users_uid() -> u32 {
    // SAFETY: geteuid has no inputs or side effects and is available on the
    // Unix platforms that support this Unix-domain socket client.
    unsafe { libc::geteuid() }
}

#[derive(Debug, Error)]
pub enum WaError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sidecar not running (connect {path}: {source})")]
    NotConnected {
        path: String,
        source: std::io::Error,
    },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    /// Typed error returned by the sidecar (`ok:false`).
    #[error("sidecar {kind}: {message}")]
    Sidecar { kind: String, message: String },
    #[error("sidecar closed the connection before responding")]
    ChannelClosed,
    #[error("config: {0}")]
    Config(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("sidecar request timed out")]
    Timeout,
}

/// Sidecar request frame.
#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    version: u32,
    request_id: String,
    op: &'a str,
    params: Value,
}

/// Sidecar response frame (method replies only — events are a separate shape).
#[derive(Debug, Deserialize)]
struct RpcResponse {
    version: u32,
    request_id: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    message: String,
}

type Pending = Arc<StdMutex<HashMap<String, oneshot::Sender<Result<RpcResponse, WaError>>>>>;

/// An aborted caller must not leave a waiter in the shared request map.
struct PendingRequest {
    pending: Pending,
    id: String,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

/// Connected JSON-RPC client + a background reader task that splits responses
/// from events. Cheap to `clone()` — the inner write half and pending map are
/// `Arc`-shared so the DM channel and the control surface share one socket.
#[derive(Clone)]
pub struct WaClient {
    write: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    pending: Pending,
}

impl WaClient {
    /// Connect to the sidecar UDS and spawn the demux reader. `events` is the
    /// channel inbound `WaEvent`s are pushed onto; the caller (channel /
    /// control surface) drains it.
    pub async fn connect(
        socket_path: impl AsRef<Path>,
        events: mpsc::Sender<WaEvent>,
    ) -> Result<Self, WaError> {
        let path = socket_path.as_ref();
        let stream = UnixStream::connect(path)
            .await
            .map_err(|source| WaError::NotConnected {
                path: path.display().to_string(),
                source,
            })?;
        let (read, write) = stream.into_split();
        let pending: Pending = Arc::new(StdMutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);

        tokio::spawn(async move {
            let mut lines = FramedRead::new(read, LinesCodec::new_with_max_length(4 * 1024 * 1024));
            loop {
                match lines.next().await {
                    Some(Ok(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        Self::dispatch_frame(&line, &pending_reader, &events).await;
                    }
                    None => {
                        debug!("wa sidecar closed the connection");
                        break;
                    }
                    Some(Err(e)) => {
                        warn!("wa sidecar read error: {e}");
                        break;
                    }
                }
            }
            // Drain pending waiters so callers get ChannelClosed instead of
            // hanging forever once the sidecar dies.
            let mut guard = pending_reader.lock().unwrap();
            guard.clear();
        });

        Ok(Self {
            write: Arc::new(Mutex::new(write)),
            pending,
        })
    }

    /// Route one decoded line: response frames (have `request_id`) wake the
    /// matching oneshot; everything else is parsed as an event.
    async fn dispatch_frame(line: &str, pending: &Pending, events: &mpsc::Sender<WaEvent>) {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                warn!("wa sidecar sent unparseable frame: {e}");
                return;
            }
        };
        if value.get("request_id").is_some() {
            let request_id = value
                .get("request_id")
                .and_then(Value::as_str)
                .map(str::to_string);
            match serde_json::from_value::<RpcResponse>(value) {
                Ok(resp) => {
                    let id = resp.request_id.clone();
                    if let Some(tx) = pending.lock().unwrap().remove(&id) {
                        let reply = if resp.version == PROTOCOL_VERSION {
                            Ok(resp)
                        } else {
                            Err(WaError::Protocol(format!(
                                "unsupported response version {}",
                                resp.version
                            )))
                        };
                        let _ = tx.send(reply);
                    } else {
                        debug!(request_id = %id, "wa response with no waiter (timed out?)");
                    }
                }
                Err(e) => {
                    if let Some(id) = request_id {
                        if let Some(tx) = pending.lock().unwrap().remove(&id) {
                            let _ =
                                tx.send(Err(WaError::Protocol(format!("invalid response: {e}"))));
                        }
                    }
                }
            }
            return;
        }
        if value.get("version").and_then(Value::as_u64) != Some(u64::from(PROTOCOL_VERSION)) {
            warn!("wa event has unsupported protocol version");
            return;
        }
        match serde_json::from_value::<WaEvent>(value) {
            Ok(ev) => {
                if events.send(ev).await.is_err() {
                    debug!("wa event receiver dropped");
                }
            }
            Err(e) => debug!("wa frame is neither response nor known event: {e}"),
        }
    }

    /// Issue one method call and await the typed result.
    async fn call(&self, op: &str, params: Value) -> Result<Value, WaError> {
        self.call_with_timeout(op, params, REQUEST_TIMEOUT).await
    }

    async fn call_with_timeout(
        &self,
        op: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, WaError> {
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(request_id.clone(), tx);
        let _pending_request = PendingRequest {
            pending: Arc::clone(&self.pending),
            id: request_id.clone(),
        };

        let frame = RpcRequest {
            version: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            op,
            params,
        };
        let mut line = serde_json::to_vec(&frame)?;
        line.push(b'\n');
        {
            let mut w = self.write.lock().await;
            if let Err(e) = w.write_all(&line).await {
                return Err(e.into());
            }
            w.flush().await?;
        }

        let resp = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => reply?,
            Ok(Err(_)) => return Err(WaError::ChannelClosed),
            Err(_) => return Err(WaError::Timeout),
        };
        if resp.ok {
            Ok(resp.result.unwrap_or(Value::Null))
        } else {
            let err = resp.error.unwrap_or(RpcError {
                kind: "Internal".into(),
                message: "sidecar returned ok=false with no error body".into(),
            });
            Err(WaError::Sidecar {
                kind: err.kind,
                message: err.message,
            })
        }
    }

    /// `list_chats` — recent 1:1 chats the linked device knows about.
    pub async fn list_chats(&self, limit: u32) -> Result<Vec<WaContact>, WaError> {
        let v = self
            .call("list_chats", serde_json::json!({ "limit": limit }))
            .await?;
        let chats = v.get("chats").cloned().unwrap_or(Value::Array(Vec::new()));
        Ok(serde_json::from_value(chats)?)
    }

    /// `fetch_history` — last `limit` messages of one chat (used by the
    /// control surface to give the reasoner conversation context).
    pub async fn fetch_chat_history(
        &self,
        chat_jid: &str,
        limit: u32,
    ) -> Result<Vec<WaMessage>, WaError> {
        let v = self
            .call(
                "fetch_history",
                serde_json::json!({ "chat_jid": chat_jid, "limit": limit }),
            )
            .await?;
        let msgs = v
            .get("messages")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
        Ok(serde_json::from_value(msgs)?)
    }

    /// `send_text` — outbound text to a chat. Returns the server message id.
    pub async fn send_text(&self, chat_jid: &str, text: &str) -> Result<String, WaError> {
        let v = self
            .call(
                "send_text",
                serde_json::json!({ "chat_jid": chat_jid, "text": text }),
            )
            .await?;
        Ok(v.get("message_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    /// `status` — sidecar self-report (paired? connected? which device JID?).
    pub async fn status(&self) -> Result<Value, WaError> {
        self.call("status", serde_json::json!({})).await
    }

    /// Start or resume QR pairing on the sidecar's single linked-device store.
    pub async fn start_pairing(&self) -> Result<(), WaError> {
        self.call("start_pairing", serde_json::json!({})).await?;
        Ok(())
    }

    /// Remove exactly the linked device the operator selected. The sidecar
    /// rejects a stale or different device JID before contacting WhatsApp.
    pub async fn logout(&self, expected_device_jid: &str) -> Result<(), WaError> {
        if expected_device_jid.trim().is_empty() {
            return Err(WaError::Config("expected device JID is required".into()));
        }
        self.call(
            "logout",
            serde_json::json!({ "expected_device_jid": expected_device_jid }),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// Spin a one-connection mock sidecar on a tempfile UDS. `responder` is
    /// called with each request line and returns the response line(s) to
    /// write back (it may also emit unsolicited event frames).
    async fn mock_sidecar<F>(path: PathBuf, responder: F)
    where
        F: Fn(Value) -> Vec<String> + Send + 'static,
    {
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let req: Value = serde_json::from_str(&line).unwrap();
                for out in responder(req) {
                    if write.write_all(out.as_bytes()).await.is_err()
                        || write.write_all(b"\n").await.is_err()
                    {
                        return;
                    }
                }
            }
        });
    }

    fn sock(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        dir.path().join(name)
    }

    #[tokio::test]
    async fn send_text_round_trips_over_uds() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            let id = req["request_id"].as_str().unwrap().to_string();
            assert_eq!(req["op"], "send_text");
            assert_eq!(req["version"], PROTOCOL_VERSION);
            assert_eq!(req["params"]["chat_jid"], "15551234567@s.whatsapp.net");
            vec![serde_json::json!({
                "version": PROTOCOL_VERSION,
                "request_id": id,
                "ok": true,
                "result": { "message_id": "3EB0SENT" }
            })
            .to_string()]
        })
        .await;
        // Give the listener a beat to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let mid = client
            .send_text("15551234567@s.whatsapp.net", "hello")
            .await
            .unwrap();
        assert_eq!(mid, "3EB0SENT");
    }

    #[tokio::test]
    async fn pairing_and_logout_use_explicit_sidecar_operations() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            let id = req["request_id"].as_str().unwrap();
            assert!(matches!(req["op"].as_str(), Some("start_pairing" | "logout")));
            if req["op"] == "logout" {
                assert_eq!(req["params"]["expected_device_jid"], "15551234567:2@s.whatsapp.net");
            }
            vec![serde_json::json!({"version": PROTOCOL_VERSION, "request_id": id, "ok": true, "result": {}}).to_string()]
        }).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        client.start_pairing().await.unwrap();
        client.logout("15551234567:2@s.whatsapp.net").await.unwrap();
    }

    #[tokio::test]
    async fn sidecar_error_surfaces_typed() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            let id = req["request_id"].as_str().unwrap().to_string();
            vec![serde_json::json!({
                "version": PROTOCOL_VERSION,
                "request_id": id,
                "ok": false,
                "error": { "kind": "NotPaired", "message": "no linked device" }
            })
            .to_string()]
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let err = client
            .send_text("x@s.whatsapp.net", "hi")
            .await
            .unwrap_err();
        match err {
            WaError::Sidecar { kind, message } => {
                assert_eq!(kind, "NotPaired");
                assert!(message.contains("no linked device"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[tokio::test]
    async fn event_frames_are_routed_to_the_event_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            let id = req["request_id"].as_str().unwrap().to_string();
            // Reply to the call, then push an unsolicited inbound message.
            vec![
                serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "request_id": id, "ok": true, "result": { "chats": [] }
                })
                .to_string(),
                serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "event": "received-message",
                    "id": "INBOUND1",
                    "chat": "15551234567@s.whatsapp.net",
                    "sender": "15551234567@s.whatsapp.net",
                    "push_name": "Tony",
                    "text": "yo",
                    "timestamp": 1776630000,
                    "from_me": false
                })
                .to_string(),
            ]
        })
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let (tx, mut rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let chats = client.list_chats(10).await.unwrap();
        assert!(chats.is_empty());
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match ev {
            WaEvent::ReceivedMessage { message } => {
                assert_eq!(message.id, "INBOUND1");
                assert_eq!(message.text, "yo");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_fails_cleanly_when_no_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "absent.sock");
        let (tx, _rx) = mpsc::channel(8);
        match WaClient::connect(&path, tx).await {
            Ok(_) => panic!("expected NotConnected error, got a connected client"),
            Err(e) => assert!(matches!(e, WaError::NotConnected { .. })),
        }
    }

    #[tokio::test]
    async fn mismatched_response_version_fails_without_waiting_for_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            vec![serde_json::json!({
                "version": 999,
                "request_id": req["request_id"],
                "ok": true,
                "result": {"paired": false}
            })
            .to_string()]
        })
        .await;
        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let result = client.status().await;
        assert!(matches!(result, Err(WaError::Protocol(_))));
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn timed_out_and_cancelled_calls_remove_their_waiters() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |_req| Vec::new()).await;
        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let result = client
            .call_with_timeout(
                "status",
                serde_json::json!({}),
                std::time::Duration::from_millis(10),
            )
            .await;
        assert!(matches!(result, Err(WaError::Timeout)));
        assert!(client.pending.lock().unwrap().is_empty());

        let waiting = {
            let client = client.clone();
            tokio::spawn(async move { client.status().await })
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if !client.pending.lock().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        waiting.abort();
        let _ = waiting.await;
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_responses_are_matched_by_request_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let first: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let second: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            for req in [&second, &first] {
                let reply = serde_json::json!({
                    "version": PROTOCOL_VERSION,
                    "request_id": req["request_id"],
                    "ok": true,
                    "result": {"op": req["op"]}
                });
                write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let (first, second) = tokio::join!(
            client.call("first", serde_json::json!({})),
            client.call("second", serde_json::json!({}))
        );
        assert_eq!(first.unwrap()["op"], "first");
        assert_eq!(second.unwrap()["op"], "second");
    }

    #[tokio::test]
    async fn oversized_response_closes_pending_call_instead_of_allocating_unboundedly() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            vec![serde_json::json!({
                "version": PROTOCOL_VERSION,
                "request_id": req["request_id"],
                "ok": true,
                "result": {"padding": "x".repeat(4 * 1024 * 1024)}
            })
            .to_string()]
        })
        .await;
        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        assert!(matches!(client.status().await, Err(WaError::ChannelClosed)));
    }

    #[tokio::test]
    async fn socket_close_wakes_a_pending_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, _write) = stream.into_split();
            let _ = BufReader::new(read).lines().next_line().await;
            // Both halves close here, before a response is sent.
        });
        let (tx, _rx) = mpsc::channel(8);
        let client = WaClient::connect(&path, tx).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), client.status())
            .await
            .unwrap();
        assert!(matches!(result, Err(WaError::ChannelClosed)));
    }

    #[test]
    fn default_socket_path_respects_env_override() {
        std::env::set_var("AUGMENTAGENT_WA_SOCK", "/tmp/custom-wa.sock");
        assert_eq!(default_socket_path(), PathBuf::from("/tmp/custom-wa.sock"));
        std::env::remove_var("AUGMENTAGENT_WA_SOCK");
    }

    #[test]
    fn macos_socket_path_is_short_and_independent_of_home() {
        assert_eq!(
            macos_socket_path(501),
            PathBuf::from("/tmp/augmentagent-501/wa.sock")
        );
    }

    #[test]
    fn shared_wire_v1_fixture_decodes_and_preserves_media() {
        let lines: Vec<&str> = include_str!("../../../docs/fixtures/whatsapp/wire-v1.ndjson")
            .lines()
            .collect();
        assert_eq!(lines.len(), 3);
        let expected_request: Value = serde_json::from_str(lines[0]).unwrap();
        let actual_request = serde_json::to_value(RpcRequest {
            version: PROTOCOL_VERSION,
            request_id: "golden-status".into(),
            op: "status",
            params: serde_json::json!({}),
        })
        .unwrap();
        assert_eq!(actual_request, expected_request);
        let response: RpcResponse = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(response.version, PROTOCOL_VERSION);
        assert_eq!(response.result.unwrap()["paired"], false);
        let event: WaEvent = serde_json::from_str(lines[2]).unwrap();
        let WaEvent::ReceivedMessage { message } = event else {
            panic!("fixture must be a received message");
        };
        assert_eq!(message.metadata.quoted_message_id, "quoted-1");
        assert_eq!(message.metadata.mentioned_jids, ["2@s.whatsapp.net"]);
        let media = message.metadata.media.unwrap();
        assert_eq!(media.kind, "image");
        assert_eq!(media.mime_type, "image/png");
        assert_eq!(media.size, 123);
    }
}
