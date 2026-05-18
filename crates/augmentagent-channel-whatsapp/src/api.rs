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
//! Request  : {"request_id":"<uuid>","op":"send_text","params":{...}}
//! Success  : {"request_id":"...","ok":true,"result":{...}}
//! Failure  : {"request_id":"...","ok":false,
//!             "error":{"kind":"NotPaired"|"NotConnected"|"SendFailed"
//!                            |"BadRequest"|"Internal","message":"..."}}
//! ```
//!
//! **Events (sidecar-initiated, no `request_id`):**
//!
//! ```text
//! {"event":"received-message","id":"...","chat":"...","sender":"...",
//!  "push_name":"...","text":"...","timestamp":1776630000,"from_me":false}
//! {"event":"qr","code":"2@..."}
//! {"event":"pair-success","device_jid":"...","user_jid":"..."}
//! {"event":"connected"}
//! {"event":"logged-out","reason":"..."}
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

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, warn};
use uuid::Uuid;

use crate::types::{WaContact, WaEvent, WaMessage};

/// Default UDS path. `${XDG_RUNTIME_DIR}/augmentagent/wa.sock`, falling back to
/// `/run/user/<uid>/...` then `/tmp/...` so headless / CI hosts still work.
/// Overridable via `AUGMENTAGENT_WA_SOCK` (parity with the browser sidecar's
/// `AUGMENTAGENT_BROWSER_SOCK`).
pub fn default_socket_path() -> PathBuf {
    if let Ok(custom) = std::env::var("AUGMENTAGENT_WA_SOCK") {
        return PathBuf::from(custom);
    }
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| {
        // SAFETY: getuid is always-available libc; fall back to /tmp.
        match std::fs::metadata("/run/user") {
            Ok(_) => format!("/run/user/{}", users_uid()),
            Err(_) => "/tmp".to_string(),
        }
    });
    PathBuf::from(runtime).join("augmentagent").join("wa.sock")
}

fn users_uid() -> u32 {
    // `id -u` without pulling the `users`/`libc` crate. Falls back to 1000.
    std::fs::read_to_string("/proc/self/loginuid")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&u| u != u32::MAX)
        .unwrap_or(1000)
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
}

/// Sidecar request frame.
#[derive(Debug, Serialize)]
struct RpcRequest<'a> {
    request_id: String,
    op: &'a str,
    params: Value,
}

/// Sidecar response frame (method replies only — events are a separate shape).
#[derive(Debug, Deserialize)]
struct RpcResponse {
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

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<RpcResponse>>>>;

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
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pending_reader = Arc::clone(&pending);

        tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        Self::dispatch_frame(&line, &pending_reader, &events).await;
                    }
                    Ok(None) => {
                        debug!("wa sidecar closed the connection");
                        break;
                    }
                    Err(e) => {
                        warn!("wa sidecar read error: {e}");
                        break;
                    }
                }
            }
            // Drain pending waiters so callers get ChannelClosed instead of
            // hanging forever once the sidecar dies.
            let mut guard = pending_reader.lock().await;
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
                warn!("wa sidecar sent unparseable frame: {e}; line={line}");
                return;
            }
        };
        if value.get("request_id").is_some() {
            match serde_json::from_value::<RpcResponse>(value) {
                Ok(resp) => {
                    let id = resp.request_id.clone();
                    if let Some(tx) = pending.lock().await.remove(&id) {
                        let _ = tx.send(resp);
                    } else {
                        debug!(request_id = %id, "wa response with no waiter (timed out?)");
                    }
                }
                Err(e) => warn!("wa response decode failed: {e}"),
            }
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
        let request_id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(request_id.clone(), tx);

        let frame = RpcRequest {
            request_id: request_id.clone(),
            op,
            params,
        };
        let mut line = serde_json::to_vec(&frame)?;
        line.push(b'\n');
        {
            let mut w = self.write.lock().await;
            if let Err(e) = w.write_all(&line).await {
                self.pending.lock().await.remove(&request_id);
                return Err(e.into());
            }
            w.flush().await?;
        }

        let resp = rx.await.map_err(|_| WaError::ChannelClosed)?;
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
        let chats = v
            .get("chats")
            .cloned()
            .unwrap_or(Value::Array(Vec::new()));
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
                    write.write_all(out.as_bytes()).await.unwrap();
                    write.write_all(b"\n").await.unwrap();
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
            assert_eq!(req["params"]["chat_jid"], "15551234567@s.whatsapp.net");
            vec![serde_json::json!({
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
    async fn sidecar_error_surfaces_typed() {
        let dir = tempfile::tempdir().unwrap();
        let path = sock(&dir, "wa.sock");
        mock_sidecar(path.clone(), |req| {
            let id = req["request_id"].as_str().unwrap().to_string();
            vec![serde_json::json!({
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
        let err = client.send_text("x@s.whatsapp.net", "hi").await.unwrap_err();
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
                    "request_id": id, "ok": true, "result": { "chats": [] }
                })
                .to_string(),
                serde_json::json!({
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

    #[test]
    fn default_socket_path_respects_env_override() {
        std::env::set_var("AUGMENTAGENT_WA_SOCK", "/tmp/custom-wa.sock");
        assert_eq!(
            default_socket_path(),
            PathBuf::from("/tmp/custom-wa.sock")
        );
        std::env::remove_var("AUGMENTAGENT_WA_SOCK");
    }
}
