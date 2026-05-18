//! Persisted X / Twitter session state.
//!
//! X's web app authenticates internal (`/i/api`, GraphQL) calls with a cookie
//! session bundle plus a CSRF echo header:
//!
//! - `auth_token` cookie — the session bearer (treat as a password).
//! - `ct0` cookie — CSRF token. The web client echoes it back in the
//!   `x-csrf-token` request header verbatim; mismatched/absent => HTTP 403.
//! - A **public web Bearer** token (`AAAA…`) shipped in `main.<hash>.js`.
//!   It's a static app-level token, not per-user, so we default a known-good
//!   value and expose an env override for the rare rotation.
//! - `x-client-transaction-id` — a per-request anti-automation header X added
//!   in 2023. Newer deploys reject some endpoints without it; we send a
//!   best-effort generated value (full client-side derivation REQUIRES LIVE
//!   OPERATOR VALIDATION — see `docs/twitter-protocol.md`).
//!
//! File shape:
//!
//! ```json
//! {
//!   "user_id": "1234567890",
//!   "screen_name": "you",
//!   "cookies": {
//!     "auth_token": "...",
//!     "ct0": "..."
//!   },
//!   "bearer": "AAAAAAAAAAAAAAAAAAAAA...",
//!   "user_agent": "Mozilla/5.0 ...",
//!   "harvested_at_ms": 1776600000000
//! }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

/// The public web app Bearer. This is the long-lived, app-level token X ships
/// in its bundled JS — it is NOT per-user and has been stable for years.
/// Override via `AUGMENTAGENT_TWITTER_BEARER` if X rotates it.
pub const DEFAULT_PUBLIC_BEARER: &str =
    "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs%3D1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("missing cookie: {0}")]
    MissingCookie(&'static str),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("keychain: {0}")]
    Keychain(#[from] augmentagent_auth::AuthError),
}

/// Keychain platform namespace. Combined with the screen_name to form a
/// per-account credential slot (`augmentagent/twitter/<screen_name>`), or the
/// shared default slot (`augmentagent/twitter/default`).
pub const KEYCHAIN_PLATFORM: &str = "twitter";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TwitterAuth {
    /// Numeric account id (string form). Used to filter self-authored tweets
    /// and as the `account_entity_id` suffix.
    pub user_id: String,
    /// `@handle` minus the `@`. Used for keyring slot + log lines.
    pub screen_name: String,
    /// Name → value cookie map. Must include `auth_token` and `ct0`.
    pub cookies: BTreeMap<String, String>,
    #[serde(default = "default_bearer")]
    pub bearer: String,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default)]
    pub harvested_at_ms: i64,
}

fn default_user_agent() -> String {
    DEFAULT_USER_AGENT.to_string()
}

fn default_bearer() -> String {
    std::env::var("AUGMENTAGENT_TWITTER_BEARER")
        .unwrap_or_else(|_| DEFAULT_PUBLIC_BEARER.to_string())
}

impl TwitterAuth {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: TwitterAuth = serde_json::from_str(&raw)?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), AuthError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), AuthError> {
        if self.user_id.is_empty() {
            return Err(AuthError::Invalid("empty user_id".into()));
        }
        if self.screen_name.is_empty() {
            return Err(AuthError::Invalid("empty screen_name".into()));
        }
        if !self.cookies.contains_key("auth_token") {
            return Err(AuthError::MissingCookie("auth_token"));
        }
        if !self.cookies.contains_key("ct0") {
            return Err(AuthError::MissingCookie("ct0"));
        }
        if self.bearer.is_empty() {
            return Err(AuthError::Invalid("empty bearer".into()));
        }
        Ok(())
    }

    /// Age of the harvested session in whole days relative to `now_ms`.
    /// `None` when `harvested_at_ms` is unset (0) or in the future. The
    /// validate harness surfaces this as a non-fatal advisory: X
    /// `auth_token`s don't carry a fixed TTL, but a months-old cookie is the
    /// likeliest cause of an otherwise-inexplicable 401 mid-run.
    pub fn session_age_days(&self, now_ms: i64) -> Option<i64> {
        if self.harvested_at_ms <= 0 || now_ms < self.harvested_at_ms {
            return None;
        }
        Some((now_ms - self.harvested_at_ms) / (24 * 60 * 60 * 1000))
    }

    /// Heuristic staleness flag for the auth probe. 60 days is well past a
    /// typical X web-session refresh; not authoritative (only a live call
    /// proves the cookie), just an advisory the runbook calls out.
    pub fn is_session_stale(&self, now_ms: i64) -> bool {
        self.session_age_days(now_ms).is_some_and(|d| d >= 60)
    }

    /// The `x-csrf-token` header value X expects: a verbatim echo of the
    /// `ct0` cookie.
    pub fn csrf_token(&self) -> Result<String, AuthError> {
        self.cookies
            .get("ct0")
            .cloned()
            .ok_or(AuthError::MissingCookie("ct0"))
    }

    /// Serialize the cookie jar as a `Cookie:` header value.
    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// `authorization` header value — the public Bearer with the `Bearer `
    /// scheme prefix.
    pub fn authorization(&self) -> String {
        format!("Bearer {}", self.bearer)
    }

    /// Read credentials from the OS keychain at
    /// `augmentagent/twitter/<account>`.
    pub fn load_from_keychain(account: &str) -> Result<Self, AuthError> {
        let bytes = augmentagent_auth::Auth::get(KEYCHAIN_PLATFORM, account)?;
        let parsed: TwitterAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Write credentials to the OS keychain at
    /// `augmentagent/twitter/<account>`.
    pub fn save_to_keychain(&self, account: &str) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(self)?;
        augmentagent_auth::Auth::put(KEYCHAIN_PLATFORM, account, &bytes)?;
        Ok(())
    }

    /// Load from Keychain first (default slot); fall back to the legacy auth
    /// file under [`default_auth_path`]. On a successful file-fallback hit,
    /// promote the credentials into Keychain so subsequent loads skip the
    /// file entirely. Mirrors the LinkedIn channel's migration path.
    pub fn load_with_migration(repo_root: &Path) -> Result<Self, AuthError> {
        match Self::load_from_keychain(augmentagent_auth::DEFAULT_ACCOUNT) {
            Ok(auth) => {
                tracing::debug!("twitter auth loaded from keychain");
                Ok(auth)
            }
            Err(AuthError::Keychain(augmentagent_auth::AuthError::NotFound { .. })) => {
                let path = default_auth_path(repo_root);
                let auth = Self::load(&path)?;
                match auth.save_to_keychain(augmentagent_auth::DEFAULT_ACCOUNT) {
                    Ok(()) => tracing::info!(
                        from = %path.display(),
                        "twitter auth migrated to keychain from legacy file",
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "twitter auth loaded from file but keychain write failed; will retry next boot",
                    ),
                }
                Ok(auth)
            }
            Err(e) => Err(e),
        }
    }
}

/// Default on-disk location for the cookie file. Ordered:
/// 1. `AUGMENTAGENT_TWITTER_AUTH` env override
/// 2. `<repo_root>/twitter-auth.json` (Linux-only deploy; no macOS vault)
///
/// Callers pass the repo root so we don't have to guess the cwd.
pub fn default_auth_path(repo_root: &Path) -> PathBuf {
    if let Ok(custom) = std::env::var("AUGMENTAGENT_TWITTER_AUTH") {
        return PathBuf::from(custom);
    }
    repo_root.join("twitter-auth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TwitterAuth {
        let mut cookies = BTreeMap::new();
        cookies.insert("auth_token".into(), "abc123sessiontoken".into());
        cookies.insert("ct0".into(), "deadbeefcsrf".into());
        TwitterAuth {
            user_id: "1450000000000000000".into(),
            screen_name: "nolanmak".into(),
            cookies,
            bearer: DEFAULT_PUBLIC_BEARER.into(),
            user_agent: DEFAULT_USER_AGENT.into(),
            harvested_at_ms: 1776600000000,
        }
    }

    #[test]
    fn csrf_echoes_ct0_cookie() {
        assert_eq!(sample().csrf_token().unwrap(), "deadbeefcsrf");
    }

    #[test]
    fn authorization_has_bearer_prefix() {
        assert!(sample().authorization().starts_with("Bearer AAAA"));
    }

    #[test]
    fn validate_rejects_missing_cookies() {
        let mut a = sample();
        a.cookies.remove("auth_token");
        assert!(matches!(
            a.validate(),
            Err(AuthError::MissingCookie("auth_token"))
        ));
        let mut a = sample();
        a.cookies.remove("ct0");
        assert!(matches!(a.validate(), Err(AuthError::MissingCookie("ct0"))));
    }

    #[test]
    fn validate_rejects_empty_identity() {
        let mut a = sample();
        a.user_id = String::new();
        assert!(matches!(a.validate(), Err(AuthError::Invalid(_))));
        let mut a = sample();
        a.screen_name = String::new();
        assert!(matches!(a.validate(), Err(AuthError::Invalid(_))));
    }

    #[test]
    fn cookie_header_joins_pairs() {
        let header = sample().cookie_header();
        assert!(header.contains("auth_token=abc123sessiontoken"));
        assert!(header.contains("ct0=deadbeefcsrf"));
        assert!(header.contains("; "));
    }

    #[test]
    fn round_trip_json() {
        let auth = sample();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("twitter-auth.json");
        auth.save(&path).unwrap();
        let loaded = TwitterAuth::load(&path).unwrap();
        assert_eq!(loaded.user_id, auth.user_id);
        assert_eq!(loaded.screen_name, auth.screen_name);
        assert_eq!(loaded.cookies, auth.cookies);
    }

    #[test]
    fn session_age_and_staleness() {
        let mut a = sample();
        a.harvested_at_ms = 1_000_000_000_000;
        // +10 days
        let now = a.harvested_at_ms + 10 * 24 * 60 * 60 * 1000;
        assert_eq!(a.session_age_days(now), Some(10));
        assert!(!a.is_session_stale(now));
        // +90 days → stale advisory
        let later = a.harvested_at_ms + 90 * 24 * 60 * 60 * 1000;
        assert_eq!(a.session_age_days(later), Some(90));
        assert!(a.is_session_stale(later));
        // unset harvest time → no opinion
        a.harvested_at_ms = 0;
        assert_eq!(a.session_age_days(now), None);
        assert!(!a.is_session_stale(now));
    }

    #[test]
    fn default_auth_path_honors_env_override() {
        std::env::set_var("AUGMENTAGENT_TWITTER_AUTH", "/tmp/custom-tw.json");
        let p = default_auth_path(Path::new("/repo"));
        assert_eq!(p, PathBuf::from("/tmp/custom-tw.json"));
        std::env::remove_var("AUGMENTAGENT_TWITTER_AUTH");
        let p = default_auth_path(Path::new("/repo"));
        assert_eq!(p, PathBuf::from("/repo/twitter-auth.json"));
    }
}
