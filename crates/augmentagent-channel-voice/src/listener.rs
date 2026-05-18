//! Long-poll loop for the voice-capture bot.
//!
//! Hard security model:
//! - The bot token lives ONLY in the keyring slot
//!   `augmentagent/telegram-capture` (never an env var, never the db).
//! - Inbound chats are gated by a hard allowlist file
//!   `~/.config/augmentagent/telegram-allowed-chats.json` — a JSON array of
//!   integer chat ids. A message from any other chat is dropped without
//!   transcription (we don't even download the audio). Missing/empty file =
//!   deny-all.
//! - The last acked `update_id` is persisted in `voice_capture_state` so a
//!   daemon restart never re-ingests an already-handled memo.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use augmentagent_auth::Auth;
use augmentagent_channel_core::reasoner::Reasoner;
use augmentagent_store::Store;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::channel::ingest_memo;
use crate::confirm::send_confirmation;
use crate::extract::extract;
use crate::telegram::{VoiceTelegramClient, DEFAULT_LONG_POLL_SECS};
use crate::transcribe::Transcriber;

/// Keyring slot for the capture bot token (see issue #80).
pub const KEYRING_PLATFORM: &str = "telegram-capture";
/// Logical key for the `voice_capture_state` cursor row.
pub const BOT_KEY: &str = "default";

/// Resolve the allowlist path: `$XDG_CONFIG_HOME` or `~/.config`, then
/// `augmentagent/telegram-allowed-chats.json`.
pub fn default_allowlist_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        });
    base.join("augmentagent/telegram-allowed-chats.json")
}

#[derive(Debug, Deserialize)]
struct AllowFile(Vec<i64>);

/// Load the chat-id allowlist. Missing / unparseable file = empty (deny-all).
pub fn load_allowlist(path: &Path) -> Vec<i64> {
    match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<AllowFile>(&s) {
            Ok(a) => a.0,
            Err(e) => {
                warn!(path = %path.display(), "allowlist parse failed: {e}; deny-all");
                Vec::new()
            }
        },
        Err(_) => {
            warn!(path = %path.display(), "allowlist file missing; deny-all");
            Vec::new()
        }
    }
}

/// Read the capture bot token from the keyring. `None` ⇒ channel disabled.
pub fn load_token() -> Option<String> {
    match Auth::get(KEYRING_PLATFORM, augmentagent_auth::DEFAULT_ACCOUNT) {
        Ok(bytes) => Some(String::from_utf8_lossy(&bytes).trim().to_string()),
        Err(_) => None,
    }
}

pub struct VoiceListener<R: Reasoner + 'static, T: Transcriber> {
    pub client: VoiceTelegramClient,
    pub store: Arc<Store>,
    pub reasoner: Arc<R>,
    pub transcriber: T,
    pub allowed_chats: Vec<i64>,
    pub wiki_root: PathBuf,
    pub wiki_schema: String,
    pub dry_run: bool,
}

impl<R: Reasoner + 'static, T: Transcriber> VoiceListener<R, T> {
    /// Run one long-poll batch. Returns the number of memos ingested.
    /// Pure of side effects beyond ingest + cursor advance.
    pub async fn poll_once(&self) -> anyhow::Result<usize> {
        let offset = self
            .store
            .voice_capture_offset(BOT_KEY)
            .ok()
            .flatten()
            .map(|v| v + 1);
        let timeout = if self.dry_run {
            0
        } else {
            DEFAULT_LONG_POLL_SECS
        };
        let updates = self.client.get_updates(offset, timeout).await?;
        let mut ingested = 0usize;
        let mut max_update = offset.map(|o| o - 1).unwrap_or(0);

        for up in &updates {
            max_update = max_update.max(up.update_id);
            let Some(msg) = &up.message else {
                continue;
            };
            let chat_id = match &msg.chat {
                Some(c) => c.id,
                None => continue,
            };
            // Hard allowlist gate — drop before any download.
            if !self.allowed_chats.contains(&chat_id) {
                warn!(chat_id, "voice capture: chat not in allowlist; dropped");
                continue;
            }
            let voice = msg.voice.as_ref().or(msg.audio.as_ref());
            let Some(v) = voice else {
                debug!(chat_id, "non-voice message ignored");
                continue;
            };

            if let Err(e) = self
                .handle_voice(chat_id, msg.message_id, &v.file_id)
                .await
            {
                warn!(chat_id, "voice memo handling failed: {e:#}");
                continue;
            }
            ingested += 1;
        }

        // Advance the cursor even if some messages were dropped — we never
        // want to re-see a denied chat's traffic.
        if !updates.is_empty() {
            let _ = self.store.set_voice_capture_offset(BOT_KEY, max_update);
        }
        Ok(ingested)
    }

    async fn handle_voice(
        &self,
        chat_id: i64,
        message_id: i64,
        file_id: &str,
    ) -> anyhow::Result<()> {
        let meta = self.client.get_file(file_id).await?;
        let file_path = meta
            .file_path
            .ok_or_else(|| anyhow::anyhow!("getFile returned no file_path"))?;
        let bytes = self.client.download_file(&file_path).await?;

        // whisper.cpp wants a file on disk. Use a temp file with the source
        // extension so the decoder sniffs the container correctly.
        let ext = Path::new(&file_path)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("ogg");
        let tmp_dir = std::env::temp_dir();
        let audio_path = tmp_dir.join(format!("voice-{chat_id}-{message_id}.{ext}"));
        tokio::fs::write(&audio_path, &bytes).await?;

        let transcript = self.transcriber.transcribe(&audio_path).await;
        let _ = tokio::fs::remove_file(&audio_path).await;
        let transcript = transcript?;

        let rec = extract(&self.reasoner, &transcript).await;

        if self.dry_run {
            info!(chat_id, title = %rec.title, "voice capture (dry-run): not ingested");
        } else {
            ingest_memo(
                Arc::clone(&self.reasoner),
                self.wiki_root.clone(),
                self.wiki_schema.clone(),
                &rec,
                chat_id,
                message_id,
            );
            send_confirmation(&self.client, chat_id, &rec).await;
        }
        Ok(())
    }

    /// Long-poll loop until shutdown.
    pub async fn run(&self, shutdown: CancellationToken) -> anyhow::Result<()> {
        info!(
            allowed = self.allowed_chats.len(),
            dry_run = self.dry_run,
            "voice-capture listener started"
        );
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("voice-capture listener: shutdown");
                    return Ok(());
                }
                r = self.poll_once() => {
                    if let Err(e) = r {
                        warn!("voice poll error: {e:#}; backing off 5s");
                        tokio::select! {
                            _ = shutdown.cancelled() => return Ok(()),
                            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn allowlist_missing_is_deny_all() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("nope.json");
        assert!(load_allowlist(&p).is_empty());
    }

    #[test]
    fn allowlist_parses_array() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("chats.json");
        std::fs::write(&p, "[111, 222, 333]").unwrap();
        let a = load_allowlist(&p);
        assert_eq!(a, vec![111, 222, 333]);
    }

    #[test]
    fn allowlist_garbage_is_deny_all() {
        let d = TempDir::new().unwrap();
        let p = d.path().join("bad.json");
        std::fs::write(&p, "not json").unwrap();
        assert!(load_allowlist(&p).is_empty());
    }

    #[test]
    fn default_path_ends_with_expected() {
        std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdgtest");
        let p = default_allowlist_path();
        assert!(p.ends_with("augmentagent/telegram-allowed-chats.json"));
        std::env::remove_var("XDG_CONFIG_HOME");
    }
}
