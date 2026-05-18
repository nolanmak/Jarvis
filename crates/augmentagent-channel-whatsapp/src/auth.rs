//! WhatsApp linked-device credentials.
//!
//! Unlike LinkedIn/Telegram, the actual whatsmeow noise session lives in the
//! **sidecar's** own SQLite store (`whatsmeow.SQLStore`) — that's the only
//! place the protobuf device keys can be safely re-loaded. What we persist in
//! the keyring is a small *bundle* that lets the Rust side know which device
//! is paired and route sends back to it:
//!
//! ```json
//! {
//!   "phone": "15559998888",
//!   "device_jid": "15559998888:5@s.whatsapp.net",
//!   "user_jid": "15559998888@s.whatsapp.net",
//!   "paired_at_ms": 1776600000000
//! }
//! ```
//!
//! One slot per phone: `augmentagent/whatsapp/<phone>`. The `whatsapp_devices`
//! SQLite table is the index; the keyring slot holds the bundle so a fresh
//! daemon boot can confirm a pairing without re-reading the sidecar store.
//!
//! For headless / CI loops where the Secret Service backend is unavailable,
//! `AUGMENTAGENT_WHATSAPP_AUTH` may point at a JSON file with the same shape
//! — consulted only on `Keychain(NotFound)` (parity with telegram-bot).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError};

/// Keychain platform namespace. Combined with `<phone>` to form
/// `augmentagent/whatsapp/<phone>`.
pub const KEYCHAIN_PLATFORM: &str = "whatsapp";

/// Env override: file path holding the JSON bundle for fallback / CI use.
pub const ENV_AUTH_OVERRIDE: &str = "AUGMENTAGENT_WHATSAPP_AUTH";

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
pub struct WhatsappAuth {
    /// E.164-ish digits of the linked device's number (no `+`). Used as the
    /// keyring slot key and the `whatsapp_devices.phone` index.
    pub phone: String,
    /// Full device JID including the `:<device>` suffix.
    pub device_jid: String,
    /// Bare user JID (`<number>@s.whatsapp.net`).
    pub user_jid: String,
    #[serde(default)]
    pub paired_at_ms: i64,
}

impl WhatsappAuth {
    pub fn validate(&self) -> Result<(), AuthError> {
        if self.phone.is_empty() {
            return Err(AuthError::Invalid("empty phone".into()));
        }
        if !self.phone.chars().all(|c| c.is_ascii_digit()) {
            return Err(AuthError::Invalid(
                "phone must be digits only (E.164 without '+')".into(),
            ));
        }
        if self.device_jid.is_empty() {
            return Err(AuthError::Invalid("empty device_jid".into()));
        }
        if !self.user_jid.contains('@') {
            return Err(AuthError::Invalid("user_jid must contain '@'".into()));
        }
        Ok(())
    }

    pub fn load_from_keychain(phone: &str) -> Result<Self, AuthError> {
        let bytes = KeychainAuth::get(KEYCHAIN_PLATFORM, phone)?;
        let parsed: WhatsappAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        KeychainAuth::put(KEYCHAIN_PLATFORM, &self.phone, &bytes)?;
        Ok(())
    }

    pub fn delete_from_keychain(phone: &str) -> Result<(), AuthError> {
        KeychainAuth::delete(KEYCHAIN_PLATFORM, phone)?;
        Ok(())
    }

    /// Keychain first; fall back to the `AUGMENTAGENT_WHATSAPP_AUTH` file
    /// only when the keychain entry is missing (a corrupt-but-present entry
    /// surfaces loudly instead of being papered over).
    pub fn load_with_file_fallback(phone: &str) -> Result<Self, AuthError> {
        match Self::load_from_keychain(phone) {
            Ok(a) => Ok(a),
            Err(AuthError::Keychain(KeychainError::NotFound { .. })) => {
                if let Some(path) = file_fallback_path() {
                    let raw = std::fs::read_to_string(&path)?;
                    let parsed: WhatsappAuth = serde_json::from_str(&raw)?;
                    parsed.validate()?;
                    Ok(parsed)
                } else {
                    Err(AuthError::Invalid(format!(
                        "no whatsapp keychain entry for {phone} and \
                         {ENV_AUTH_OVERRIDE} not set"
                    )))
                }
            }
            Err(e) => Err(e),
        }
    }

    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: WhatsappAuth = serde_json::from_str(&raw)?;
        parsed.validate()?;
        Ok(parsed)
    }
}

pub fn file_fallback_path() -> Option<PathBuf> {
    std::env::var(ENV_AUTH_OVERRIDE).ok().map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> WhatsappAuth {
        WhatsappAuth {
            phone: "15559998888".into(),
            device_jid: "15559998888:5@s.whatsapp.net".into(),
            user_jid: "15559998888@s.whatsapp.net".into(),
            paired_at_ms: 1776600000000,
        }
    }

    #[test]
    fn validate_accepts_populated() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_non_digit_phone() {
        let mut a = sample();
        a.phone = "+1 555".into();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_device_jid() {
        let mut a = sample();
        a.device_jid.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_user_jid_without_at() {
        let mut a = sample();
        a.user_jid = "15559998888".into();
        assert!(a.validate().is_err());
    }

    #[test]
    fn json_round_trip() {
        let a = sample();
        let s = serde_json::to_string(&a).unwrap();
        let p: WhatsappAuth = serde_json::from_str(&s).unwrap();
        assert_eq!(p.phone, a.phone);
        assert_eq!(p.device_jid, a.device_jid);
        assert_eq!(p.user_jid, a.user_jid);
    }

    #[test]
    fn load_from_file_round_trip() {
        let a = sample();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wa-auth.json");
        std::fs::write(&path, serde_json::to_string(&a).unwrap()).unwrap();
        let loaded = WhatsappAuth::load_from_file(&path).unwrap();
        assert_eq!(loaded.phone, a.phone);
    }
}
