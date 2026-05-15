//! GitHub PAT auth persisted via `augmentagent-auth` (Linux Secret Service /
//! macOS Keychain).
//!
//! One slot per connected GitHub user, keyed by login:
//! `augmentagent/github/<login>`. The shared SQLite DB has no GitHub-specific
//! index; the keyring is the source of truth (one PAT per machine in v1).
//!
//! Stored payload shape (JSON):
//!
//! ```json
//! {
//!   "username":      "nolanmak",
//!   "token":         "ghp_xxxxxxxxxxxxxxxxxxxx",
//!   "fetched_at_ms": 1776600000000
//! }
//! ```
//!
//! Required PAT scopes: `notifications` (poll the user's notification feed)
//! and `repo` (post review/issue comments on Approve). Validated at
//! `augmentagent github login` time by hitting `GET /user`.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError};

/// Keychain platform namespace. Combined with the user's login to form
/// `augmentagent/github/<login>`.
pub const KEYCHAIN_PLATFORM: &str = "github";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("keychain: {0}")]
    Keychain(#[from] KeychainError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid auth: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuth {
    /// GitHub login (e.g. `"nolanmak"`). Doubles as the keychain account slot
    /// so the credential file is self-describing.
    pub username: String,
    /// Personal-access token. `gho_` (fine-grained) or `ghp_` (classic) both
    /// work — only the `notifications` + `repo` scopes are required.
    pub token: String,
    /// Wall-clock ms when the token was last persisted. Surfaced in
    /// `augmentagent github subscriptions` so the user can spot stale rotations.
    #[serde(default)]
    pub fetched_at_ms: i64,
}

impl GithubAuth {
    pub fn validate(&self) -> Result<(), AuthError> {
        if self.username.is_empty() {
            return Err(AuthError::Invalid("empty username".into()));
        }
        if self.token.is_empty() {
            return Err(AuthError::Invalid("empty token".into()));
        }
        Ok(())
    }

    /// Load by login. Caller is expected to know the login — typically passed
    /// on the CLI or stamped on `Email::account_entity_id` (`github:<login>`).
    pub fn load_for_user(username: &str) -> Result<Self, AuthError> {
        let bytes = KeychainAuth::get(KEYCHAIN_PLATFORM, username)?;
        let parsed: GithubAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Persist into the user-keyed slot derived from `self.username`.
    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        KeychainAuth::put(KEYCHAIN_PLATFORM, &self.username, &bytes)?;
        Ok(())
    }

    pub fn delete_from_keychain(username: &str) -> Result<(), AuthError> {
        KeychainAuth::delete(KEYCHAIN_PLATFORM, username)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GithubAuth {
        GithubAuth {
            username: "nolanmak".into(),
            token: "ghp_AAAABBBBCCCCDDDDEEEE".into(),
            fetched_at_ms: 1776600000000,
        }
    }

    #[test]
    fn validate_accepts_populated() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_username() {
        let mut a = sample();
        a.username.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_token() {
        let mut a = sample();
        a.token.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn json_round_trip() {
        let a = sample();
        let json = serde_json::to_string(&a).unwrap();
        let back: GithubAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(back.username, a.username);
        assert_eq!(back.token, a.token);
    }
}
