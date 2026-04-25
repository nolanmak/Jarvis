//! Slack auth persisted via `augmentagent-auth` (macOS Keychain).
//!
//! One slot per connected workspace, keyed by Slack `team_id`:
//! `augmentagent/slack/<team_id>`. The `slack_workspaces` table in the shared
//! SQLite DB is the index; Keychain only holds credentials.
//!
//! Stored payload shape (JSON):
//!
//! ```json
//! {
//!   "entity_id":     "composio-user-123",
//!   "connection_id": "slack_conn_abc",
//!   "team_id":       "T01234567",
//!   "team_name":     "Code & Coffee",
//!   "user_id":       "U0123ABCD",
//!   "composio_api_key": "<composio api key>"
//! }
//! ```

use serde::{Deserialize, Serialize};
use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError, DEFAULT_ACCOUNT};

pub const KEYCHAIN_PLATFORM: &str = "slack";

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
pub struct SlackAuth {
    /// Composio entity id (stable per-Composio-user). Routes API calls to
    /// this workspace's connected account.
    pub entity_id: String,
    /// Composio connection id — unique per connected workspace; used for
    /// lifecycle operations (not per-call).
    pub connection_id: String,
    /// Slack team (workspace) id, e.g. `T01234567`.
    pub team_id: String,
    /// Human-readable workspace name for CLI/UI listings.
    pub team_name: String,
    /// Authenticated user's Slack id within this workspace. Used to skip our
    /// own outbound messages on ingest.
    pub user_id: String,
    /// Composio API key. Stored with the connection so callers don't have to
    /// keep it in env. Rotate by re-running `augmentagent slack login`.
    pub composio_api_key: String,
}

impl SlackAuth {
    /// Full validation, called before persisting to Keychain. team_id is
    /// required since it's the slot key.
    pub fn validate(&self) -> Result<(), AuthError> {
        if self.entity_id.is_empty() {
            return Err(AuthError::Invalid("empty entity_id".into()));
        }
        if self.connection_id.is_empty() {
            return Err(AuthError::Invalid("empty connection_id".into()));
        }
        if self.team_id.is_empty() {
            return Err(AuthError::Invalid("empty team_id".into()));
        }
        if self.composio_api_key.is_empty() {
            return Err(AuthError::Invalid("empty composio_api_key".into()));
        }
        // user_id is optional (back-compat for accounts where Composio doesn't
        // expose authed_user). Only used to skip self-messages on ingest.
        Ok(())
    }

    /// Loose validation for callers that just need to make Composio calls
    /// (entity_id + composio_api_key are the only fields execute() actually
    /// reads). Used during OAuth callback to probe team info before we know
    /// the team_id.
    pub fn validate_for_execute(&self) -> Result<(), AuthError> {
        if self.entity_id.is_empty() {
            return Err(AuthError::Invalid("empty entity_id".into()));
        }
        if self.composio_api_key.is_empty() {
            return Err(AuthError::Invalid("empty composio_api_key".into()));
        }
        Ok(())
    }

    /// Load a workspace's auth by its Slack `team_id`. Each workspace lives in
    /// its own Keychain slot; the caller is expected to iterate known teams
    /// (from the `slack_workspaces` table) and call this once per team.
    pub fn load_for_team(team_id: &str) -> Result<Self, AuthError> {
        let bytes = KeychainAuth::get(KEYCHAIN_PLATFORM, team_id)?;
        let parsed: SlackAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Back-compat: loads the legacy single-account slot, used as a fallback
    /// the first time the multi-workspace code runs against a pre-migration
    /// Keychain. Callers should prefer `load_for_team`.
    pub fn load_from_default_slot() -> Result<Self, AuthError> {
        let bytes = KeychainAuth::get(KEYCHAIN_PLATFORM, DEFAULT_ACCOUNT)?;
        let parsed: SlackAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Persist into the workspace-keyed slot derived from `self.team_id`.
    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        KeychainAuth::put(KEYCHAIN_PLATFORM, &self.team_id, &bytes)?;
        Ok(())
    }

    pub fn delete_from_keychain(team_id: &str) -> Result<(), AuthError> {
        KeychainAuth::delete(KEYCHAIN_PLATFORM, team_id)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SlackAuth {
        SlackAuth {
            entity_id: "composio-user-123".into(),
            connection_id: "slack_conn_abc".into(),
            team_id: "T01234567".into(),
            team_name: "Code & Coffee".into(),
            user_id: "U0123ABCD".into(),
            composio_api_key: "ckak_abcdef".into(),
        }
    }

    #[test]
    fn validate_accepts_populated() {
        sample().validate().unwrap();
    }

    #[test]
    fn validate_rejects_empty_entity_id() {
        let mut a = sample();
        a.entity_id.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_composio_api_key() {
        let mut a = sample();
        a.composio_api_key.clear();
        assert!(a.validate().is_err());
    }

    #[test]
    fn json_round_trip() {
        let a = sample();
        let json = serde_json::to_string(&a).unwrap();
        let parsed: SlackAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.entity_id, a.entity_id);
        assert_eq!(parsed.team_id, a.team_id);
        assert_eq!(parsed.composio_api_key, a.composio_api_key);
    }
}
