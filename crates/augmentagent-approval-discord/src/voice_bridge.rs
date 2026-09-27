//! Private, versioned IPC between the Rust-owned Discord gateway and the
//! Linux voice sidecar. The sidecar never receives the bot token.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde_json::{json, Value};
use serenity::gateway::ShardMessenger;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tracing::warn;

const MAX_FRAME_BYTES: usize = 32_768;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceBinding {
    pub guild_id: String,
    pub conversation_id: String,
    pub voice_channel_id: String,
    pub owner_id: String,
    pub bot_user_id: String,
    pub generation: u64,
}

pub struct VoiceBridge {
    writer: Mutex<OwnedWriteHalf>,
    pending: DashMap<String, oneshot::Sender<Result<Value, String>>>,
    active: DashMap<String, VoiceBinding>,
    shard: RwLock<Option<ShardMessenger>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl VoiceBridge {
    pub async fn connect(path: &Path) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("connect Discord voice sidecar at {}", path.display()))?;
        let (reader, writer) = stream.into_split();
        let bridge = Arc::new(Self {
            writer: Mutex::new(writer),
            pending: DashMap::new(),
            active: DashMap::new(),
            shard: RwLock::new(None),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        tokio::spawn(Arc::clone(&bridge).read_loop(reader));
        Ok(bridge)
    }

    pub async fn set_shard(&self, shard: ShardMessenger) {
        *self.shard.write().await = Some(shard);
    }

    pub fn binding(&self, guild_id: &str) -> Option<VoiceBinding> {
        self.active.get(guild_id).map(|item| item.clone())
    }

    pub async fn start(&self, binding: VoiceBinding) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("Voice sidecar is disconnected");
        }
        match self.active.entry(binding.guild_id.clone()) {
            Entry::Occupied(existing) => {
                if existing.get() == &binding {
                    bail!("Voice start is already pending or active");
                }
                bail!(
                    "Guild voice is already bound to {}",
                    existing.get().conversation_id
                );
            }
            Entry::Vacant(slot) => {
                slot.insert(binding.clone());
            }
        }
        let result = self
            .request(json!({
                "version": 1, "kind": "start", "guildId": binding.guild_id,
                "channelId": binding.voice_channel_id, "conversationId": binding.conversation_id,
                "ownerId": binding.owner_id, "botUserId": binding.bot_user_id,
                "generation": binding.generation,
            }))
            .await;
        if result.is_err() {
            self.active.remove(&binding.guild_id);
        }
        result.map(|_| ())
    }

    pub async fn stop(&self, guild_id: &str, conversation_id: &str) -> Result<()> {
        let binding = self
            .binding(guild_id)
            .context("No active voice binding in this guild")?;
        if binding.conversation_id != conversation_id {
            bail!("Voice is bound to another conversation");
        }
        let result = self
            .request(json!({
                "version": 1, "kind": "stop", "conversationId": conversation_id,
                "generation": binding.generation,
            }))
            .await;
        self.active.remove(guild_id);
        if result.is_err() {
            self.send_leave(guild_id).await;
        }
        result.map(|_| ())
    }

    pub async fn interrupt(&self, guild_id: &str, conversation_id: &str) -> Result<()> {
        let binding = self
            .binding(guild_id)
            .context("No active voice binding in this guild")?;
        if binding.conversation_id != conversation_id {
            bail!("Voice is bound to another conversation");
        }
        self.request(json!({
            "version": 1, "kind": "interrupt", "conversationId": conversation_id,
            "generation": binding.generation,
        }))
        .await
        .map(|_| ())
    }

    pub async fn forward_voice_state(
        &self,
        guild_id: &str,
        user_id: &str,
        channel_id: Option<&str>,
        session_id: &str,
    ) -> Result<()> {
        let Some(binding) = self.binding(guild_id) else {
            return Ok(());
        };
        if user_id != binding.owner_id && user_id != binding.bot_user_id {
            return Ok(());
        }
        self.send(&json!({
            "version": 1, "kind": "voice_state", "conversationId": binding.conversation_id,
            "generation": binding.generation, "guildId": guild_id, "userId": user_id,
            "channelId": channel_id, "sessionId": session_id,
        }))
        .await
    }

    pub async fn forward_voice_server(
        &self,
        guild_id: &str,
        endpoint: &str,
        token: &str,
    ) -> Result<()> {
        let Some(binding) = self.binding(guild_id) else {
            return Ok(());
        };
        self.send(&json!({
            "version": 1, "kind": "voice_server", "conversationId": binding.conversation_id,
            "generation": binding.generation, "guildId": guild_id,
            "endpoint": endpoint, "token": token,
        }))
        .await
    }

    async fn request(&self, mut frame: Value) -> Result<Value> {
        let request_id = self.next_id.fetch_add(1, Ordering::SeqCst).to_string();
        frame["requestId"] = json!(request_id);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(request_id.clone(), tx);
        if let Err(error) = self.send(&frame).await {
            self.pending.remove(&request_id);
            return Err(error);
        }
        let result = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
        self.pending.remove(&request_id);
        match result {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(error))) => bail!("Voice sidecar: {error}"),
            Ok(Err(_)) => bail!("Voice sidecar disconnected"),
            Err(_) => bail!("Voice sidecar request timed out"),
        }
    }

    async fn send(&self, frame: &Value) -> Result<()> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("Voice sidecar is disconnected");
        }
        let mut data = serde_json::to_vec(frame)?;
        if data.len() > MAX_FRAME_BYTES {
            bail!("Voice IPC frame exceeds size limit");
        }
        data.push(b'\n');
        self.writer
            .lock()
            .await
            .write_all(&data)
            .await
            .context("write voice IPC frame")
    }

    async fn read_loop(self: Arc<Self>, mut reader: OwnedReadHalf) {
        let mut pending = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            for byte in &chunk[..read] {
                if *byte == b'\n' {
                    if let Ok(frame) = serde_json::from_slice::<Value>(&pending) {
                        self.handle_frame(frame).await;
                    }
                    pending.clear();
                } else {
                    pending.push(*byte);
                    if pending.len() > MAX_FRAME_BYTES {
                        warn!("voice IPC frame exceeded size limit");
                        self.close().await;
                        return;
                    }
                }
            }
        }
        self.close().await;
    }

    async fn handle_frame(&self, frame: Value) {
        if frame.get("version") != Some(&json!(1)) {
            return;
        }
        match frame.get("kind").and_then(Value::as_str) {
            Some("reply") => {
                let Some(id) = frame.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                if let Some((_, tx)) = self.pending.remove(id) {
                    let outcome = if frame.get("ok") == Some(&json!(true)) {
                        Ok(frame)
                    } else {
                        Err(frame
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("request failed")
                            .to_string())
                    };
                    let _ = tx.send(outcome);
                }
            }
            Some("gateway_send") => {
                let Some(guild_id) = frame.get("guildId").and_then(Value::as_str) else {
                    return;
                };
                let Some(binding) = self.binding(guild_id) else {
                    return;
                };
                if frame.get("conversationId").and_then(Value::as_str)
                    != Some(binding.conversation_id.as_str())
                    || frame.get("generation").and_then(Value::as_u64) != Some(binding.generation)
                {
                    return;
                }
                let Some(payload) = frame.get("payload") else {
                    return;
                };
                if !valid_gateway_send(payload, &binding) {
                    return;
                }
                if let Some(shard) = self.shard.read().await.as_ref() {
                    shard.websocket_message(WebSocketMessage::Text(payload.to_string()));
                }
            }
            _ => {}
        }
    }

    async fn send_leave(&self, guild_id: &str) {
        if let Some(shard) = self.shard.read().await.as_ref() {
            shard.websocket_message(WebSocketMessage::Text(
                json!({
                    "op": 4, "d": { "guild_id": guild_id, "channel_id": null,
                        "self_deaf": false, "self_mute": false }
                })
                .to_string(),
            ));
        }
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let guilds: Vec<_> = self
            .active
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for guild in guilds {
            self.send_leave(&guild).await;
        }
        self.active.clear();
        let ids: Vec<_> = self
            .pending
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        for id in ids {
            if let Some((_, tx)) = self.pending.remove(&id) {
                let _ = tx.send(Err("Voice sidecar disconnected".into()));
            }
        }
        let _ = self.writer.lock().await.shutdown().await;
    }
}

fn valid_gateway_send(payload: &Value, binding: &VoiceBinding) -> bool {
    let Some(outer) = payload.as_object() else {
        return false;
    };
    if outer.len() != 2 || outer.get("op") != Some(&json!(4)) {
        return false;
    }
    let Some(data) = outer.get("d").and_then(Value::as_object) else {
        return false;
    };
    if data.len() != 4
        || data.get("guild_id").and_then(Value::as_str) != Some(binding.guild_id.as_str())
    {
        return false;
    }
    let channel = data.get("channel_id");
    (channel == Some(&Value::Null)
        || channel.and_then(Value::as_str) == Some(binding.voice_channel_id.as_str()))
        && data.get("self_deaf").and_then(Value::as_bool).is_some()
        && data.get("self_mute").and_then(Value::as_bool).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, BufReader};

    fn binding() -> VoiceBinding {
        VoiceBinding {
            guild_id: "1".into(),
            conversation_id: "1:2".into(),
            voice_channel_id: "3".into(),
            owner_id: "4".into(),
            bot_user_id: "5".into(),
            generation: 6,
        }
    }

    #[test]
    fn gateway_send_is_bound_to_the_authorized_guild_and_channel() {
        let good = json!({"op":4,"d":{"guild_id":"1","channel_id":"3",
            "self_deaf":false,"self_mute":false}});
        assert!(valid_gateway_send(&good, &binding()));
        assert!(valid_gateway_send(
            &json!({"op":4,"d":{"guild_id":"1","channel_id":null,
            "self_deaf":false,"self_mute":false}}),
            &binding()
        ));
        assert!(!valid_gateway_send(
            &json!({"op":4,"d":{"guild_id":"1","channel_id":"9",
            "self_deaf":false,"self_mute":false}}),
            &binding()
        ));
        assert!(!valid_gateway_send(
            &json!({"op":2,"d":{"guild_id":"1","channel_id":"3",
            "self_deaf":false,"self_mute":false}}),
            &binding()
        ));
    }

    #[tokio::test]
    async fn private_sidecar_start_rejects_a_competing_conversation_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let start: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(start["kind"], "start");
            assert_eq!(start["conversationId"], "1:2");
            assert_eq!(start["channelId"], "3");
            write
                .write_all(
                    format!(
                        "{{\"version\":1,\"kind\":\"reply\",\"requestId\":\"{}\",\"ok\":true}}\n",
                        start["requestId"].as_str().unwrap()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let stop: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(stop["kind"], "stop");
            write
                .write_all(
                    format!(
                        "{{\"version\":1,\"kind\":\"reply\",\"requestId\":\"{}\",\"ok\":true}}\n",
                        stop["requestId"].as_str().unwrap()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        bridge.start(binding()).await.unwrap();
        let mut competing = binding();
        competing.conversation_id = "1:9".into();
        assert!(bridge
            .start(competing)
            .await
            .unwrap_err()
            .to_string()
            .contains("already bound"));
        bridge.stop("1", "1:2").await.unwrap();
        assert!(bridge.binding("1").is_none());
        server.await.unwrap();
    }
}
