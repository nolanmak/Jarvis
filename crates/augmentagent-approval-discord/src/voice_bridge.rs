//! Private, versioned IPC between the Rust-owned Discord gateway and the
//! Linux voice sidecar. The sidecar never receives the bot token.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use serde_json::{json, Value};
use serenity::all::{ChannelId, CreateMessage, Http};
use serenity::gateway::ShardMessenger;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message as WebSocketMessage;
use tracing::warn;

use crate::{AuditCtx, QueryHandler};

const MAX_FRAME_BYTES: usize = 32_768;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const RECONNECT_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];
#[cfg(test)]
const RECONNECT_DELAYS: [Duration; 3] = [
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(40),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceBinding {
    pub guild_id: String,
    pub conversation_id: String,
    pub text_channel_id: String,
    pub voice_channel_id: String,
    pub owner_id: String,
    pub bot_user_id: String,
    pub generation: u64,
}

#[derive(Clone)]
struct TurnHandler {
    query: Arc<dyn QueryHandler>,
    http: Arc<Http>,
}

pub struct VoiceBridge {
    socket_path: PathBuf,
    writer: Mutex<Option<OwnedWriteHalf>>,
    pending: DashMap<String, oneshot::Sender<Result<Value, String>>>,
    active: DashMap<String, VoiceBinding>,
    seen_transcripts: DashMap<String, ()>,
    audio_states: DashMap<String, String>,
    shard: RwLock<Option<ShardMessenger>>,
    turn_handler: RwLock<Option<TurnHandler>>,
    next_id: AtomicU64,
    closed: AtomicBool,
    reconnecting: AtomicBool,
}

impl VoiceBridge {
    pub async fn connect(path: &Path) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(path).await;
        let connected = stream.is_ok();
        let (reader, writer) = match stream {
            Ok(stream) => {
                let (reader, writer) = stream.into_split();
                (Some(reader), Some(writer))
            }
            Err(error) => {
                warn!("Discord voice sidecar unavailable at {}: {error}; retrying", path.display());
                (None, None)
            }
        };
        let bridge = Arc::new(Self {
            socket_path: path.to_path_buf(),
            writer: Mutex::new(writer),
            pending: DashMap::new(),
            active: DashMap::new(),
            seen_transcripts: DashMap::new(),
            audio_states: DashMap::new(),
            shard: RwLock::new(None),
            turn_handler: RwLock::new(None),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(!connected),
            reconnecting: AtomicBool::new(!connected),
        });
        if let Some(reader) = reader {
            tokio::spawn(Arc::clone(&bridge).read_loop(reader));
        } else {
            tokio::spawn(Arc::clone(&bridge).recover_initial_connection());
        }
        Ok(bridge)
    }

    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    pub fn transport_status(&self) -> &'static str {
        if self.is_connected() {
            "connected"
        } else if self.reconnecting.load(Ordering::SeqCst) {
            "reconnecting"
        } else {
            "stopped"
        }
    }

    async fn recover_initial_connection(self: Arc<Self>) {
        if let Some(stream) = self.retry_connect().await {
            let (reader, writer) = stream.into_split();
            *self.writer.lock().await = Some(writer);
            self.closed.store(false, Ordering::SeqCst);
            self.reconnecting.store(false, Ordering::SeqCst);
            tokio::spawn(self.read_loop(reader));
            warn!("Discord voice sidecar connected after startup retry");
        }
    }

    async fn retry_connect(&self) -> Option<UnixStream> {
        for delay in RECONNECT_DELAYS {
            tokio::time::sleep(delay).await;
            match UnixStream::connect(&self.socket_path).await {
                Ok(stream) => return Some(stream),
                Err(error) => warn!("Discord voice sidecar reconnect failed: {error}"),
            }
        }
        warn!("Discord voice sidecar stopped after three reconnect attempts");
        self.reconnecting.store(false, Ordering::SeqCst);
        None
    }

    pub async fn set_shard(&self, shard: ShardMessenger) {
        *self.shard.write().await = Some(shard);
    }

    pub async fn set_turn_handler(&self, query: Arc<dyn QueryHandler>, http: Arc<Http>) {
        *self.turn_handler.write().await = Some(TurnHandler { query, http });
    }

    pub(crate) async fn mirror_tool_speech(
        &self,
        binding: &VoiceBinding,
        turn_id: &str,
        utterance_id: &str,
        text: &str,
    ) -> Result<()> {
        let Some(handler) = self.turn_handler.read().await.clone() else {
            bail!("Voice text mirror is unavailable");
        };
        let channel = ChannelId::new(binding.text_channel_id.parse()?);
        let mirror = format!("🔊 **Agent** (`{turn_id}/{utterance_id}`): {text}");
        for chunk in crate::event_handler::chunk_for_discord(&mirror) {
            channel
                .send_message(&handler.http, CreateMessage::new().content(chunk))
                .await?;
        }
        Ok(())
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
        if result.is_ok() {
            self.audio_states
                .insert(binding.guild_id.clone(), "connecting".into());
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
        self.audio_states.remove(guild_id);
        let prefix = format!("{}:{}:", binding.guild_id, binding.generation);
        self.seen_transcripts
            .retain(|key, _| !key.starts_with(&prefix));
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

    pub async fn status(&self, guild_id: &str, conversation_id: &str) -> Result<String> {
        let binding = self
            .binding(guild_id)
            .context("No active voice binding in this guild")?;
        if binding.conversation_id != conversation_id {
            bail!("Voice is bound to another conversation");
        }
        let reply = self
            .request(json!({
                "version": 1, "kind": "status", "conversationId": conversation_id,
                "generation": binding.generation,
            }))
            .await?;
        Ok(reply
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("connecting")
            .to_string())
    }

    pub async fn speak(
        &self,
        guild_id: &str,
        conversation_id: &str,
        utterance_id: &str,
        text: &str,
    ) -> Result<Value> {
        let binding = self
            .binding(guild_id)
            .context("No active voice binding in this guild")?;
        anyhow::ensure!(
            binding.conversation_id == conversation_id,
            "Voice is bound to another conversation"
        );
        anyhow::ensure!(
            !utterance_id.is_empty() && utterance_id.len() <= 128,
            "Invalid speech utterance ID"
        );
        anyhow::ensure!(
            !text.trim().is_empty() && text.len() <= 12_000,
            "Invalid speech text"
        );
        let reply = self
            .request(json!({
                "version": 1, "kind": "speak", "conversationId": conversation_id,
                "generation": binding.generation, "utteranceId": utterance_id,
                "text": text,
            }))
            .await?;
        let receipt = reply
            .get("receipt")
            .context("Voice sidecar omitted speech receipt")?;
        anyhow::ensure!(
            receipt.get("utteranceId").and_then(Value::as_str) == Some(utterance_id),
            "Voice sidecar returned another utterance receipt"
        );
        Ok(receipt.clone())
    }

    pub async fn speech_status(
        &self,
        guild_id: &str,
        conversation_id: &str,
        utterance_id: &str,
    ) -> Result<Option<Value>> {
        let binding = self
            .binding(guild_id)
            .context("No active voice binding in this guild")?;
        anyhow::ensure!(
            binding.conversation_id == conversation_id,
            "Voice is bound to another conversation"
        );
        anyhow::ensure!(
            !utterance_id.is_empty() && utterance_id.len() <= 128,
            "Invalid speech utterance ID"
        );
        let reply = self
            .request(json!({
                "version": 1, "kind": "speech_status", "conversationId": conversation_id,
                "generation": binding.generation, "utteranceId": utterance_id,
            }))
            .await?;
        match reply.get("receipt") {
            Some(Value::Null) => Ok(None),
            Some(receipt)
                if receipt.get("utteranceId").and_then(Value::as_str) == Some(utterance_id) =>
            {
                Ok(Some(receipt.clone()))
            }
            _ => bail!("Voice sidecar returned an invalid speech receipt"),
        }
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
        let mut writer = self.writer.lock().await;
        let writer = writer.as_mut().context("Voice sidecar is disconnected")?;
        writer.write_all(&data).await.context("write voice IPC frame")
    }

    async fn read_loop(self: Arc<Self>, mut reader: OwnedReadHalf) {
        let mut chunk = [0u8; 4096];
        loop {
            let mut pending = Vec::new();
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
            self.reconnecting.store(true, Ordering::SeqCst);
            let Some(stream) = self.retry_connect().await else {
                return;
            };
            let (next_reader, next_writer) = stream.into_split();
            *self.writer.lock().await = Some(next_writer);
            self.closed.store(false, Ordering::SeqCst);
            self.reconnecting.store(false, Ordering::SeqCst);
            reader = next_reader;
            warn!("Discord voice sidecar reconnected; start a new voice binding to resume");
        }
    }

    async fn handle_frame(self: &Arc<Self>, frame: Value) {
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
            Some("audio_status") | Some("audio_failure") => {
                let Some(binding) = self.binding_for_frame(&frame) else {
                    return;
                };
                if frame["kind"] == "audio_failure" {
                    self.active.remove(&binding.guild_id);
                    self.audio_states
                        .insert(binding.guild_id.clone(), "failed".into());
                    let prefix = format!("{}:{}:", binding.guild_id, binding.generation);
                    self.seen_transcripts
                        .retain(|key, _| !key.starts_with(&prefix));
                    self.send_leave(&binding.guild_id).await;
                    if let Some(handler) = self.turn_handler.read().await.clone() {
                        tokio::spawn(async move {
                            if let Ok(channel) = binding.text_channel_id.parse::<u64>() {
                                let _ = ChannelId::new(channel).send_message(&handler.http,
                                    CreateMessage::new().content("Voice audio stopped after a connection or provider failure. Text remains available; check the voice service before restarting.")).await;
                            }
                        });
                    }
                } else if let Some(state) = frame.get("state").and_then(Value::as_str) {
                    if matches!(state, "connecting" | "listening" | "reconnecting" | "stopped" | "failed") {
                        self.audio_states
                            .insert(binding.guild_id, state.to_string());
                    }
                }
            }
            Some("transcript") => {
                let Some(binding) = self.binding_for_frame(&frame) else {
                    return;
                };
                if frame.get("ownerId").and_then(Value::as_str) != Some(binding.owner_id.as_str()) {
                    return;
                }
                let Some(turn_id) = frame.get("turnId").and_then(Value::as_str) else {
                    return;
                };
                let Some(transcript) = frame.get("text").and_then(Value::as_str) else {
                    return;
                };
                if turn_id.is_empty()
                    || turn_id.len() > 128
                    || transcript.trim().is_empty()
                    || transcript.len() > 12_000
                {
                    return;
                }
                let seen_key = format!("{}:{}:{turn_id}", binding.guild_id, binding.generation);
                if self.seen_transcripts.insert(seen_key, ()).is_some() {
                    return;
                }
                let Some(handler) = self.turn_handler.read().await.clone() else {
                    return;
                };
                let bridge = Arc::clone(self);
                let turn_id = turn_id.to_string();
                let transcript = transcript.trim().to_string();
                tokio::spawn(async move {
                    bridge
                        .process_transcript(binding, handler, turn_id, transcript)
                        .await;
                });
            }
            _ => {}
        }
    }

    fn binding_for_frame(&self, frame: &Value) -> Option<VoiceBinding> {
        let guild_id = frame.get("guildId")?.as_str()?;
        let binding = self.binding(guild_id)?;
        (frame.get("conversationId")?.as_str()? == binding.conversation_id
            && frame.get("generation")?.as_u64()? == binding.generation)
            .then_some(binding)
    }

    async fn process_transcript(
        self: Arc<Self>,
        binding: VoiceBinding,
        handler: TurnHandler,
        turn_id: String,
        transcript: String,
    ) {
        let (Ok(guild_id), Ok(channel_id)) = (
            binding.guild_id.parse::<u64>(),
            binding.text_channel_id.parse::<u64>(),
        ) else {
            return;
        };
        let channel = ChannelId::new(channel_id);
        let mirror = format!("🎙️ **You:** {transcript}");
        for chunk in crate::event_handler::chunk_for_discord(&mirror) {
            if let Err(error) = channel
                .send_message(&handler.http, CreateMessage::new().content(chunk))
                .await
            {
                warn!("could not mirror Discord voice transcript: {error}");
                break;
            }
        }
        let audit = AuditCtx {
            session_id: turn_id.clone(),
            guild_id: Some(guild_id),
            http: Some(Arc::clone(&handler.http)),
            channel_id: Some(channel),
            owner_authorized: true,
        };
        match handler.query.answer_turn(&audit, "", &transcript).await {
            Ok(answer) => {
                for chunk in crate::event_handler::chunk_for_discord(&answer) {
                    if let Err(error) = channel
                        .send_message(&handler.http, CreateMessage::new().content(chunk))
                        .await
                    {
                        warn!("could not mirror Discord voice reply: {error}");
                        break;
                    }
                }
                let final_spoken = handler.query.take_final_spoken(&turn_id);
                if !final_spoken
                    && !answer.trim().is_empty()
                    && answer.len() <= 12_000
                    && self.binding(&binding.guild_id).as_ref() == Some(&binding)
                {
                    let result = self
                        .speak(
                            &binding.guild_id,
                            &binding.conversation_id,
                            &format!("{turn_id}:final"),
                            &answer,
                        )
                        .await;
                    if let Err(error) = result {
                        warn!("could not speak Discord voice reply: {error}");
                    }
                }
            }
            Err(error) => {
                let _ = channel
                    .send_message(
                        &handler.http,
                        CreateMessage::new()
                            .content(format!("Voice turn could not complete: {error}")),
                    )
                    .await;
            }
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
        self.audio_states.clear();
        self.seen_transcripts.clear();
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
        if let Some(mut writer) = self.writer.lock().await.take() {
            let _ = writer.shutdown().await;
        }
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, BufReader};

    struct FakeVoiceQuery(Arc<AtomicUsize>);

    struct FinalSpokenVoiceQuery(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl QueryHandler for FinalSpokenVoiceQuery {
        async fn answer(&self, _ctx: &AuditCtx, _question: &str) -> anyhow::Result<String> {
            Ok("already spoken through the bound tool".into())
        }

        fn take_final_spoken(&self, _turn_id: &str) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[async_trait::async_trait]
    impl QueryHandler for FakeVoiceQuery {
        async fn answer(&self, ctx: &AuditCtx, question: &str) -> anyhow::Result<String> {
            assert_eq!(ctx.session_id, "voice:6:0");
            assert_eq!(ctx.guild_id, Some(1));
            assert_eq!(ctx.channel_id, Some(ChannelId::new(2)));
            assert_eq!(question, "synthetic spoken request");
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok("synthetic spoken answer".into())
        }
    }

    fn binding() -> VoiceBinding {
        VoiceBinding {
            guild_id: "1".into(),
            conversation_id: "1:2".into(),
            text_channel_id: "2".into(),
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
    async fn sidecar_can_start_after_gateway_without_restarting_the_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        assert!(!bridge.is_connected());
        assert_eq!(bridge.transport_status(), "reconnecting");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (stream, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !bridge.is_connected() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(bridge.is_connected());
        assert_eq!(bridge.transport_status(), "connected");
        drop(stream);
    }

    #[tokio::test]
    async fn sidecar_restart_clears_old_binding_then_accepts_explicit_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        let (first, _) = listener.accept().await.unwrap();
        bridge.active.insert("1".into(), binding());
        drop(first);
        tokio::time::timeout(Duration::from_secs(1), async {
            while bridge.binding("1").is_some() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let (second, _) = tokio::time::timeout(Duration::from_secs(1), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let (read, mut write) = second.into_split();
        let server = tokio::spawn(async move {
            let mut lines = BufReader::new(read).lines();
            let start: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(start["kind"], "start");
            assert_eq!(start["generation"], 6);
            let reply = json!({"version":1,"kind":"reply","requestId":start["requestId"],"ok":true});
            write.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while bridge.closed.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        bridge.start(binding()).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn sidecar_reconnect_stops_after_three_failed_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        let (first, _) = listener.accept().await.unwrap();
        bridge.active.insert("1".into(), binding());
        drop(first);
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while bridge.binding("1").is_some() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(bridge.closed.load(Ordering::SeqCst));
        assert_eq!(bridge.transport_status(), "stopped");
        assert!(bridge.start(binding()).await.unwrap_err().to_string().contains("disconnected"));
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

    #[tokio::test]
    async fn speech_receipts_are_scoped_to_the_active_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            for expected in ["start", "speak", "speech_status"] {
                let frame: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(frame["kind"], expected);
                assert_eq!(frame["conversationId"], "1:2");
                assert_eq!(frame["generation"], 6);
                if expected != "start" {
                    assert_eq!(frame["utteranceId"], "turn-1:answer");
                }
                let receipt = json!({"utteranceId":"turn-1:answer","status":"queued"});
                let reply = json!({"version":1,"kind":"reply","requestId":frame["requestId"],
                    "ok":true,"receipt":receipt});
                write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        bridge.start(binding()).await.unwrap();
        assert!(bridge
            .speak("1", "1:9", "turn-1:answer", "hello")
            .await
            .is_err());
        let receipt = bridge
            .speak("1", "1:2", "turn-1:answer", "hello")
            .await
            .unwrap();
        assert_eq!(receipt["status"], "queued");
        assert!(bridge
            .speech_status("1", "1:9", "turn-1:answer")
            .await
            .is_err());
        let status = bridge
            .speech_status("1", "1:2", "turn-1:answer")
            .await
            .unwrap();
        assert_eq!(status.unwrap()["utteranceId"], "turn-1:answer");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn committed_transcript_routes_once_to_the_bound_text_session_and_speaks_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let speak: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(speak["kind"], "speak");
            assert_eq!(speak["conversationId"], "1:2");
            assert_eq!(speak["utteranceId"], "voice:6:0:final");
            assert_eq!(speak["text"], "synthetic spoken answer");
            let reply = json!({"version":1,"kind":"reply","requestId":speak["requestId"],
                "ok":true,"receipt":{"utteranceId":"voice:6:0:final","status":"queued"}});
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        });
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        bridge.active.insert("1".into(), binding());
        let calls = Arc::new(AtomicUsize::new(0));
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = local.local_addr().unwrap();
        drop(local);
        let http = Arc::new(
            serenity::http::HttpBuilder::new("test-token")
                .proxy(format!("http://{proxy}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        bridge
            .set_turn_handler(Arc::new(FakeVoiceQuery(Arc::clone(&calls))), http)
            .await;
        let transcript = json!({ "version": 1, "kind": "transcript", "conversationId": "1:2",
            "generation": 6, "guildId": "1", "ownerId": "4", "turnId": "voice:6:0",
            "text": "synthetic spoken request" });
        bridge.handle_frame(transcript.clone()).await;
        bridge.handle_frame(transcript).await;
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn final_output_delivered_by_tool_is_not_played_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("voice.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let bridge = VoiceBridge::connect(&path).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        bridge.active.insert("1".into(), binding());
        let checked = Arc::new(AtomicUsize::new(0));
        let local = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = local.local_addr().unwrap();
        drop(local);
        let http = Arc::new(
            serenity::http::HttpBuilder::new("test-token")
                .proxy(format!("http://{proxy}"))
                .ratelimiter_disabled(true)
                .build(),
        );
        bridge
            .set_turn_handler(Arc::new(FinalSpokenVoiceQuery(Arc::clone(&checked))), http)
            .await;
        bridge
            .handle_frame(
                json!({"version":1,"kind":"transcript","conversationId":"1:2",
            "generation":6,"guildId":"1","ownerId":"4","turnId":"voice:6:final-test",
            "text":"synthetic request"}),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(3), async {
            while checked.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let mut lines = BufReader::new(stream).lines();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), lines.next_line())
                .await
                .is_err(),
            "final answer was sent to speech twice"
        );
    }
}
