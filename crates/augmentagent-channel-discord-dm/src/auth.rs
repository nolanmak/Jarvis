//! Discord user-token auth persisted in macOS Keychain via `augmentagent-auth`.
//!
//! Stored payload shape (JSON, serde):
//!
//! ```json
//! {
//!   "user_id": "<YOUR_USER_ID>",
//!   "token": "MTE5...rI7FQECJj6iNi8",
//!   "super_properties_b64": "eyJvcyI6...",
//!   "user_agent": "Mozilla/5.0 (Macintosh; ...) Chrome/147.0.0.0 ..."
//! }
//! ```
//!
//! Keychain key: `service=augmentagent/discord`, `account=default`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError, DEFAULT_ACCOUNT};

pub const KEYCHAIN_PLATFORM: &str = "discord";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("keychain: {0}")]
    Keychain(#[from] KeychainError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid auth: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordAuth {
    pub user_id: String,
    pub token: String,
    /// `x-super-properties` header value — base64-encoded JSON fingerprint
    /// harvested from a real browser session.
    pub super_properties_b64: String,
    /// `user-agent` header value — must match the `browser_user_agent` field
    /// inside the decoded super_properties, or Discord flags the session.
    pub user_agent: String,
}

impl DiscordAuth {
    pub fn validate(&self) -> Result<(), AuthError> {
        if self.user_id.is_empty() {
            return Err(AuthError::Invalid("empty user_id".into()));
        }
        if self.token.is_empty() {
            return Err(AuthError::Invalid("empty token".into()));
        }
        if self.super_properties_b64.is_empty() {
            return Err(AuthError::Invalid("empty super_properties_b64".into()));
        }
        if self.user_agent.is_empty() {
            return Err(AuthError::Invalid("empty user_agent".into()));
        }
        Ok(())
    }

    pub fn load_from_keychain() -> Result<Self, AuthError> {
        let bytes = KeychainAuth::get(KEYCHAIN_PLATFORM, DEFAULT_ACCOUNT)?;
        let parsed: DiscordAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        KeychainAuth::put(KEYCHAIN_PLATFORM, DEFAULT_ACCOUNT, &bytes)?;
        Ok(())
    }

    /// Keychain-first, optional legacy-file fallback for a one-time import.
    /// No legacy file exists today (this is a new integration), but keeping
    /// the same shape as LinkedIn's `load_with_migration` makes the CLI
    /// callers uniform and leaves the door open for future bulk-imports.
    pub fn load_with_migration(file_fallback: Option<&Path>) -> Result<Self, AuthError> {
        match Self::load_from_keychain() {
            Ok(a) => Ok(a),
            Err(AuthError::Keychain(KeychainError::NotFound { .. })) => {
                if let Some(path) = file_fallback {
                    let raw = std::fs::read_to_string(path)?;
                    let a: DiscordAuth = serde_json::from_str(&raw)?;
                    a.validate()?;
                    if let Err(e) = a.save_to_keychain() {
                        tracing::warn!(error = %e, "file->keychain promote failed");
                    }
                    Ok(a)
                } else {
                    Err(AuthError::Keychain(KeychainError::NotFound {
                        platform: KEYCHAIN_PLATFORM.into(),
                        account: DEFAULT_ACCOUNT.into(),
                    }))
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Default path used by `augmentagent discord login --creds-json <path>` —
/// the user-facing harvested file. Not automatically consumed; the CLI
/// explicitly passes this path.
pub fn default_creds_path(repo_root: &Path) -> PathBuf {
    if let Ok(custom) = std::env::var("AUGMENTAGENT_DISCORD_CREDS") {
        return PathBuf::from(custom);
    }
    repo_root.join("discord-creds.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DiscordAuth {
        DiscordAuth {
            user_id: "<YOUR_USER_ID>".into(),
            token: "<YOUR_DISCORD_USER_TOKEN>".into(),
            super_properties_b64: "eyJvcyI6Ik1hYyBPUyBYIn0=".into(),
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/147.0.0.0".into(),
        }
    }

    #[test]
    fn validate_accepts_populated() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_token() {
        let mut a = sample();
        a.token.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_super_props() {
        let mut a = sample();
        a.super_properties_b64.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_user_agent() {
        let mut a = sample();
        a.user_agent.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn json_round_trip() {
        let a = sample();
        let json = serde_json::to_string(&a).unwrap();
        let parsed: DiscordAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user_id, a.user_id);
        assert_eq!(parsed.token, a.token);
        assert_eq!(parsed.super_properties_b64, a.super_properties_b64);
        assert_eq!(parsed.user_agent, a.user_agent);
    }

    #[test]
    fn default_creds_path_honors_env() {
        let repo = tempfile::tempdir().unwrap();
        std::env::set_var("AUGMENTAGENT_DISCORD_CREDS", "/tmp/custom-discord.json");
        assert_eq!(
            default_creds_path(repo.path()),
            PathBuf::from("/tmp/custom-discord.json"),
        );
        std::env::remove_var("AUGMENTAGENT_DISCORD_CREDS");
    }
}
