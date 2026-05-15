//! Telegram bot credentials persisted via `augmentagent-auth` (Linux Secret
//! Service / macOS Keychain).
//!
//! One slot per connected bot, keyed by Telegram bot username:
//! `augmentagent/telegram-bot/<bot_username>`. The `telegram_bots` SQLite
//! table is the index; the keyring slot only holds the secret token.
//!
//! Stored payload shape (JSON):
//!
//! ```json
//! {
//!   "bot_token": "123456789:AAH-...",
//!   "bot_username": "nolan_triage_bot",
//!   "bot_id": 123456789,
//!   "owner_chat_id": 987654321
//! }
//! ```
//!
//! For dev / smoke-test loops where the keyring backend isn't available
//! (headless CI, missing Secret Service), the env var
//! `AUGMENTAGENT_TELEGRAM_BOT_AUTH` may point at a JSON file holding the same
//! payload — the loader falls through to that path when the keychain lookup
//! returns `NotFound`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError};

/// Keychain platform namespace. Combined with `bot_username` to form
/// `augmentagent/telegram-bot/<bot_username>`.
pub const KEYCHAIN_PLATFORM: &str = "telegram-bot";

/// Env override: file path holding the JSON payload for fallback / CI use.
pub const ENV_AUTH_OVERRIDE: &str = "AUGMENTAGENT_TELEGRAM_BOT_AUTH";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("keychain: {0}")]
    Keychain(#[from] KeychainError),
    #[error("invalid auth: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramBotAuth {
    /// Raw bot token from BotFather (`123456789:AAH-...`). Treated as a
    /// secret — never logged at info level.
    pub bot_token: String,
    /// Bot's @username (without the `@`). Used as the keyring slot key and
    /// as the human-readable label everywhere a bot id would be opaque.
    pub bot_username: String,
    /// Numeric Telegram bot id (the integer prefix of `bot_token`). Stored
    /// alongside the username so the dispatcher can route by id when it
    /// needs to be O(1).
    pub bot_id: i64,
    /// `chat_id` of the user who ran `telegram-bot login`. The bot is
    /// allowed to DM this chat for setup / status messages without being
    /// in any subscription list, and inbound messages from chats other
    /// than this one + the explicit subscription list are dropped at the
    /// channel boundary (see `channel.rs`).
    pub owner_chat_id: i64,
}

impl TelegramBotAuth {
    /// Reject obviously-broken payloads before they hit the keychain.
    pub fn validate(&self) -> Result<(), AuthError> {
        if self.bot_token.is_empty() {
            return Err(AuthError::Invalid("empty bot_token".into()));
        }
        if !self.bot_token.contains(':') {
            return Err(AuthError::Invalid(
                "bot_token must contain ':' (BotFather format `<id>:<secret>`)".into(),
            ));
        }
        if self.bot_username.is_empty() {
            return Err(AuthError::Invalid("empty bot_username".into()));
        }
        if self.bot_id == 0 {
            return Err(AuthError::Invalid("bot_id must be non-zero".into()));
        }
        if self.owner_chat_id == 0 {
            return Err(AuthError::Invalid("owner_chat_id must be non-zero".into()));
        }
        Ok(())
    }

    /// Load credentials from the per-bot keyring slot.
    pub fn load_from_keychain(bot_username: &str) -> Result<Self, AuthError> {
        let bytes = KeychainAuth::get(KEYCHAIN_PLATFORM, bot_username)?;
        let parsed: TelegramBotAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Persist into the per-bot keyring slot.
    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        KeychainAuth::put(KEYCHAIN_PLATFORM, &self.bot_username, &bytes)?;
        Ok(())
    }

    pub fn delete_from_keychain(bot_username: &str) -> Result<(), AuthError> {
        KeychainAuth::delete(KEYCHAIN_PLATFORM, bot_username)?;
        Ok(())
    }

    /// Try the keychain first; fall back to the file pointed at by
    /// `AUGMENTAGENT_TELEGRAM_BOT_AUTH` if present and the keychain entry
    /// is missing.
    ///
    /// The file fallback is intentionally only consulted on
    /// `Keychain(NotFound)` so a corrupt-but-present keychain entry still
    /// surfaces loudly instead of being papered over.
    pub fn load_with_file_fallback(bot_username: &str) -> Result<Self, AuthError> {
        match Self::load_from_keychain(bot_username) {
            Ok(auth) => Ok(auth),
            Err(AuthError::Keychain(KeychainError::NotFound { .. })) => {
                if let Some(path) = file_fallback_path() {
                    let raw = std::fs::read_to_string(&path)?;
                    let parsed: TelegramBotAuth = serde_json::from_str(&raw)?;
                    parsed.validate()?;
                    Ok(parsed)
                } else {
                    Err(AuthError::Invalid(format!(
                        "no telegram-bot keychain entry for {bot_username} and \
                         {ENV_AUTH_OVERRIDE} not set"
                    )))
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Read from a file (used by CLI and the env-fallback path).
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: TelegramBotAuth = serde_json::from_str(&raw)?;
        parsed.validate()?;
        Ok(parsed)
    }
}

/// Resolve the env-override path, if set.
pub fn file_fallback_path() -> Option<PathBuf> {
    std::env::var(ENV_AUTH_OVERRIDE).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TelegramBotAuth {
        TelegramBotAuth {
            bot_token: "123456789:AAH-fake-secret".into(),
            bot_username: "nolan_triage_bot".into(),
            bot_id: 123456789,
            owner_chat_id: 987654321,
        }
    }

    #[test]
    fn validate_accepts_populated() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_token_without_colon() {
        let mut a = sample();
        a.bot_token = "garbage".into();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_username() {
        let mut a = sample();
        a.bot_username.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_bot_id() {
        let mut a = sample();
        a.bot_id = 0;
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_owner_chat_id() {
        let mut a = sample();
        a.owner_chat_id = 0;
        assert!(a.validate().is_err());
    }

    #[test]
    fn json_round_trip() {
        let a = sample();
        let json = serde_json::to_string(&a).unwrap();
        let parsed: TelegramBotAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.bot_token, a.bot_token);
        assert_eq!(parsed.bot_username, a.bot_username);
        assert_eq!(parsed.bot_id, a.bot_id);
        assert_eq!(parsed.owner_chat_id, a.owner_chat_id);
    }

    #[test]
    fn load_from_file_round_trip() {
        let a = sample();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, serde_json::to_string(&a).unwrap()).unwrap();
        let loaded = TelegramBotAuth::load_from_file(&path).unwrap();
        assert_eq!(loaded.bot_id, a.bot_id);
    }
}
