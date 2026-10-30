//! Ephemeral conversation-bound capabilities for native agent speech tools.
//! The model chooses only text and an utterance ID; guild/channel/generation
//! come from an owner-authorized server grant and are checked on every call.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result};
use dashmap::DashMap;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Mutex, Semaphore};

use crate::voice_bridge::{VoiceBinding, VoiceBridge};

const MAX_TOOL_FRAME: u64 = 32_768;
const MAX_TOOL_CLIENTS: usize = 32;
const MAX_TURN_SPEECH: usize = 100;

struct GrantState {
    binding: VoiceBinding,
    turn_id: String,
    final_spoken: AtomicBool,
    revoked: AtomicBool,
    speech: Mutex<std::collections::HashMap<String, SpeechEntry>>,
}

struct SpeechEntry {
    text: String,
    receipt: Value,
    mirrored: bool,
}

pub struct VoiceToolService {
    bridge: Weak<VoiceBridge>,
    socket_path: PathBuf,
    _directory: tempfile::TempDir,
    grants: DashMap<String, Arc<GrantState>>,
    clients: Arc<Semaphore>,
}

/// Dropping the turn's handle revokes its authority, including after a stop
/// and rejoin. A grant never survives a daemon restart.
pub struct VoiceToolGrant {
    service: Arc<VoiceToolService>,
    token: String,
    state: Arc<GrantState>,
}

impl VoiceToolGrant {
    pub fn token(&self) -> &str {
        &self.token
    }
    pub fn final_spoken(&self) -> bool {
        self.state.final_spoken.load(Ordering::SeqCst)
    }
}

impl Drop for VoiceToolGrant {
    fn drop(&mut self) {
        self.state.revoked.store(true, Ordering::SeqCst);
        self.service.grants.remove(&self.token);
    }
}

// Unix socket paths have a small fixed kernel limit. Build TMPDIR can be
// arbitrarily deep; socket-only state uses a fresh private short directory.
fn private_tool_listener() -> std::io::Result<(tempfile::TempDir, PathBuf, UnixListener)> {
    let directory = tempfile::Builder::new().prefix("jarvis-voice-tool-").tempdir_in("/tmp")?;
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
    let socket = directory.path().join("control.sock");
    let listener = UnixListener::bind(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    Ok((directory, socket, listener))
}

impl VoiceToolService {
    pub async fn start(bridge: &Arc<VoiceBridge>) -> Result<Arc<Self>> {
        let (directory, socket_path, listener) = private_tool_listener()?;
        let service = Arc::new(Self {
            bridge: Arc::downgrade(bridge),
            socket_path,
            _directory: directory,
            grants: DashMap::new(),
            clients: Arc::new(Semaphore::new(MAX_TOOL_CLIENTS)),
        });
        let server = Arc::clone(&service);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let Ok(permit) = Arc::clone(&server.clients).try_acquire_owned() else {
                    drop(socket);
                    continue;
                };
                let current = Arc::clone(&server);
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ =
                        tokio::time::timeout(Duration::from_secs(7), current.handle(socket)).await;
                });
            }
        });
        Ok(service)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn grant(
        self: &Arc<Self>,
        guild_id: &str,
        conversation_id: &str,
        turn_id: &str,
    ) -> Option<VoiceToolGrant> {
        if turn_id.is_empty() || turn_id.len() > 100 {
            return None;
        }
        let bridge = self.bridge.upgrade()?;
        let binding = bridge.binding(guild_id)?;
        if binding.conversation_id != conversation_id {
            return None;
        }
        let token = uuid::Uuid::new_v4().to_string();
        let state = Arc::new(GrantState {
            binding,
            turn_id: turn_id.into(),
            final_spoken: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            speech: Mutex::new(std::collections::HashMap::new()),
        });
        self.grants.insert(token.clone(), Arc::clone(&state));
        Some(VoiceToolGrant {
            service: Arc::clone(self),
            token,
            state,
        })
    }

    async fn handle(&self, socket: UnixStream) {
        let (read, mut write) = socket.into_split();
        let mut data = Vec::new();
        let read_result = BufReader::new(read)
            .take(MAX_TOOL_FRAME + 1)
            .read_until(b'\n', &mut data)
            .await;
        let result = match read_result {
            Ok(_) if data.len() as u64 <= MAX_TOOL_FRAME && data.last() == Some(&b'\n') => {
                match serde_json::from_slice::<Value>(&data) {
                    Ok(request) => self.dispatch(&request).await,
                    Err(_) => Err(anyhow::anyhow!("Malformed voice tool request")),
                }
            }
            _ => Err(anyhow::anyhow!("Voice tool frame is missing or too large")),
        };
        let reply = match result {
            Ok(value) => value,
            Err(error) => json!({"version":1,"ok":false,"error":error.to_string()}),
        };
        if let Ok(mut encoded) = serde_json::to_vec(&reply) {
            encoded.push(b'\n');
            let _ = write.write_all(&encoded).await;
        }
    }

    async fn dispatch(&self, request: &Value) -> Result<Value> {
        let fields = request
            .as_object()
            .context("Voice tool request must be an object")?;
        anyhow::ensure!(
            fields.len() == 4
                && ["version", "grant", "method", "arguments"]
                    .iter()
                    .all(|key| fields.contains_key(*key)),
            "Voice tool request contains unexpected fields"
        );
        anyhow::ensure!(request["version"] == 1, "Unsupported voice tool version");
        let token = request["grant"]
            .as_str()
            .context("Voice grant is required")?;
        let state = self
            .grants
            .get(token)
            .context("Voice grant is invalid or expired")?
            .clone();
        anyhow::ensure!(
            !state.revoked.load(Ordering::SeqCst),
            "Voice grant is expired"
        );
        let bridge = self
            .bridge
            .upgrade()
            .context("Voice bridge is unavailable")?;
        anyhow::ensure!(
            bridge.binding(&state.binding.guild_id).as_ref() == Some(&state.binding),
            "Voice binding has changed or stopped"
        );
        let args = request
            .get("arguments")
            .and_then(Value::as_object)
            .context("Voice tool arguments must be an object")?;
        let guild = &state.binding.guild_id;
        let conversation = &state.binding.conversation_id;
        match request["method"].as_str().unwrap_or("") {
            "speak" => {
                anyhow::ensure!(
                    args.len() == 2
                        && args.contains_key("text")
                        && args.contains_key("utterance_id"),
                    "speak requires text and utterance_id only"
                );
                let text = args["text"]
                    .as_str()
                    .context("Speech text must be a string")?;
                let id = args["utterance_id"]
                    .as_str()
                    .context("Speech utterance_id must be a string")?;
                anyhow::ensure!(
                    !text.trim().is_empty() && text.len() <= 12_000,
                    "Invalid speech text"
                );
                anyhow::ensure!(
                    !id.is_empty()
                        && id.len() <= 64
                        && id.bytes().all(|byte| byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.')),
                    "Invalid speech utterance_id"
                );
                // The server mints the sidecar ID; model input cannot select a
                // different turn or cross a conversation boundary.
                let actual_id = format!("{token}:{id}");
                let mut speech = state.speech.lock().await;
                if let Some(entry) = speech.get_mut(id) {
                    anyhow::ensure!(
                        entry.text == text,
                        "Speech ID was already used with different text"
                    );
                    if !entry.mirrored {
                        entry.mirrored = bridge
                            .mirror_tool_speech(&state.binding, &state.turn_id, id, text)
                            .await
                            .is_ok();
                    }
                    return Ok(json!({"version":1,"ok":true,"receipt":entry.receipt,
                        "mirrored":entry.mirrored}));
                }
                anyhow::ensure!(
                    speech.len() < MAX_TURN_SPEECH,
                    "Speech limit reached for this agent turn"
                );
                anyhow::ensure!(
                    !state.revoked.load(Ordering::SeqCst),
                    "Voice grant is expired"
                );
                let receipt = bridge.speak(guild, conversation, &actual_id, text).await?;
                if id == "final" {
                    state.final_spoken.store(true, Ordering::SeqCst);
                }
                let mirrored = bridge
                    .mirror_tool_speech(&state.binding, &state.turn_id, id, text)
                    .await
                    .is_ok();
                speech.insert(
                    id.into(),
                    SpeechEntry {
                        text: text.into(),
                        receipt: receipt.clone(),
                        mirrored,
                    },
                );
                Ok(json!({"version":1,"ok":true,"receipt":receipt,"mirrored":mirrored}))
            }
            "speech_status" => {
                anyhow::ensure!(
                    args.len() == 1 && args.contains_key("utterance_id"),
                    "speech_status requires utterance_id only"
                );
                let id = args["utterance_id"]
                    .as_str()
                    .context("Speech utterance_id must be a string")?;
                anyhow::ensure!(
                    state.speech.lock().await.contains_key(id),
                    "Unknown speech receipt"
                );
                let receipt = bridge
                    .speech_status(guild, conversation, &format!("{token}:{id}"))
                    .await?;
                Ok(json!({"version":1,"ok":true,"receipt":receipt}))
            }
            "voice_status" => {
                anyhow::ensure!(args.is_empty(), "voice_status accepts no arguments");
                let status = bridge.status(guild, conversation).await?;
                Ok(json!({"version":1,"ok":true,"state":status,"turnId":state.turn_id}))
            }
            "voice_interrupt" => {
                anyhow::ensure!(args.is_empty(), "voice_interrupt accepts no arguments");
                bridge.interrupt(guild, conversation).await?;
                Ok(json!({"version":1,"ok":true,"state":"interrupted"}))
            }
            _ => anyhow::bail!("Unknown voice tool method"),
        }
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;
    #[tokio::test]
    async fn long_build_temp_root_does_not_break_private_tool_socket() {
        const CHILD: &str = "JARVIS_SHORT_TOOL_SOCKET_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir_in("/tmp").unwrap();
            let long = root.path().join("nested-build-cache-".repeat(10));
            std::fs::create_dir(&long).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "voice_tool::socket_tests::long_build_temp_root_does_not_break_private_tool_socket", "--nocapture"])
                .env(CHILD, "1").env("TMPDIR", &long).output().unwrap();
            assert!(output.status.success(), "{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"), "socket fixture must execute");
            return;
        }
        assert!(std::env::temp_dir().as_os_str().len() > 108);
        let (first, path, listener) = private_tool_listener().unwrap();
        let (second, other, _other_listener) = private_tool_listener().unwrap();
        assert_ne!(path, other);
        assert!(path.as_os_str().len() < 104);
        assert_eq!(std::fs::metadata(first.path()).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        drop(listener); drop(first);
        assert!(!path.exists());
        assert!(other.exists());
        drop(second);
    }
}
