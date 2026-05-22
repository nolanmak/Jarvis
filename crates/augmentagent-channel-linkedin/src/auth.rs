//! Persisted LinkedIn session state.
//!
//! Voyager (LinkedIn's internal web API) takes cookies + a `csrf-token`
//! header derived from the `JSESSIONID` cookie. We harvest these once from a
//! logged-in browser via the `linkedin login` CLI and store them in a JSON
//! file on disk. On 401/403 we surface a clear error so the user knows to
//! re-harvest.
//!
//! File shape:
//!
//! ```json
//! {
//!   "member_urn": "urn:li:fsd_profile:ACoAA...",
//!   "cookies": {
//!     "li_at": "...",
//!     "JSESSIONID": "\"ajax:0103...\"",
//!     "bcookie": "..."
//!   },
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

/// Keychain platform namespace. Combined with [`augmentagent_auth::DEFAULT_ACCOUNT`]
/// to form the single LinkedIn credential slot (`augmentagent/linkedin/default`).
pub const KEYCHAIN_PLATFORM: &str = "linkedin";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkedInAuth {
    /// Your own `urn:li:fsd_profile:...` — used as `mailboxUrn` on voyager calls.
    pub member_urn: String,
    /// Name → value map. Must include `li_at` and `JSESSIONID`.
    pub cookies: BTreeMap<String, String>,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default)]
    pub harvested_at_ms: i64,
}

fn default_user_agent() -> String {
    DEFAULT_USER_AGENT.to_string()
}

impl LinkedInAuth {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: LinkedInAuth = serde_json::from_str(&raw)?;
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
        if self.member_urn.is_empty() {
            return Err(AuthError::Invalid("empty member_urn".into()));
        }
        if !self.cookies.contains_key("li_at") {
            return Err(AuthError::MissingCookie("li_at"));
        }
        if !self.cookies.contains_key("JSESSIONID") {
            return Err(AuthError::MissingCookie("JSESSIONID"));
        }
        Ok(())
    }

    /// The csrf-token header value LinkedIn expects. LinkedIn's JS derives it
    /// from `JSESSIONID` by stripping the surrounding double quotes (the
    /// cookie is stored with literal quotes: `"ajax:..."`).
    pub fn csrf_token(&self) -> Result<String, AuthError> {
        let raw = self
            .cookies
            .get("JSESSIONID")
            .ok_or(AuthError::MissingCookie("JSESSIONID"))?;
        let trimmed = raw.trim();
        let stripped = trimmed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(trimmed);
        Ok(stripped.to_string())
    }

    /// Serialize the cookie jar as a `Cookie:` header value.
    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Read LinkedIn credentials from macOS Keychain at
    /// `augmentagent/linkedin/default`.
    pub fn load_from_keychain() -> Result<Self, AuthError> {
        let bytes =
            augmentagent_auth::Auth::get(KEYCHAIN_PLATFORM, augmentagent_auth::DEFAULT_ACCOUNT)?;
        let parsed: LinkedInAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    /// Write LinkedIn credentials to macOS Keychain at
    /// `augmentagent/linkedin/default`.
    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(self)?;
        augmentagent_auth::Auth::put(
            KEYCHAIN_PLATFORM,
            augmentagent_auth::DEFAULT_ACCOUNT,
            &bytes,
        )?;
        Ok(())
    }

    /// Load from Keychain first; fall back to the legacy auth file under
    /// [`default_auth_path`]. On a successful file-fallback hit, promote the
    /// credentials into Keychain so subsequent loads skip the file entirely.
    ///
    /// Callers should prefer this over [`load`] so existing single-user
    /// deployments migrate to Keychain on their first poll after this ships.
    pub fn load_with_migration(repo_root: &Path) -> Result<Self, AuthError> {
        match Self::load_from_keychain() {
            Ok(auth) => {
                tracing::debug!("linkedin auth loaded from keychain");
                Ok(auth)
            }
            Err(AuthError::Keychain(augmentagent_auth::AuthError::NotFound { .. })) => {
                let path = default_auth_path(repo_root);
                let auth = Self::load(&path)?;
                match auth.save_to_keychain() {
                    Ok(()) => tracing::info!(
                        from = %path.display(),
                        "linkedin auth migrated to keychain from legacy file",
                    ),
                    Err(e) => tracing::warn!(
                        error = %e,
                        "linkedin auth loaded from file but keychain write failed; will retry next boot",
                    ),
                }
                Ok(auth)
            }
            Err(e) => Err(e),
        }
    }
}

/// Default on-disk location for the cookie file. Ordered:
/// 1. `AUGMENTAGENT_LINKEDIN_AUTH` env override
/// 2. `/Volumes/augmentagent/linkedin-auth.json` (macOS encrypted vault) if mounted
/// 3. `<repo_root>/linkedin-auth.json` (dev fallback)
///
/// Callers pass the repo root so we don't have to guess the cwd.
pub fn default_auth_path(repo_root: &Path) -> PathBuf {
    if let Ok(custom) = std::env::var("AUGMENTAGENT_LINKEDIN_AUTH") {
        return PathBuf::from(custom);
    }
    let vault = PathBuf::from("/Volumes/augmentagent");
    if vault.is_dir() {
        return vault.join("linkedin-auth.json");
    }
    repo_root.join("linkedin-auth.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LinkedInAuth {
        let mut cookies = BTreeMap::new();
        cookies.insert("li_at".into(), "AQEFAHkBAAAA".into());
        cookies.insert("JSESSIONID".into(), "\"ajax:0103540587890015905\"".into());
        cookies.insert("bcookie".into(), "v=2&d9d8a9a3".into());
        LinkedInAuth {
            member_urn: "urn:li:fsd_profile:ACoAAB-7H5gB".into(),
            cookies,
            user_agent: DEFAULT_USER_AGENT.into(),
            harvested_at_ms: 1776600000000,
        }
    }

    #[test]
    fn csrf_strips_quotes_from_jsessionid() {
        assert_eq!(sample().csrf_token().unwrap(), "ajax:0103540587890015905");
    }

    #[test]
    fn csrf_passes_through_unquoted() {
        let mut a = sample();
        a.cookies.insert("JSESSIONID".into(), "ajax:123".into());
        assert_eq!(a.csrf_token().unwrap(), "ajax:123");
    }

    #[test]
    fn validate_rejects_missing_cookies() {
        let mut a = sample();
        a.cookies.remove("li_at");
        assert!(matches!(
            a.validate(),
            Err(AuthError::MissingCookie("li_at"))
        ));
        let mut a = sample();
        a.cookies.remove("JSESSIONID");
        assert!(matches!(
            a.validate(),
            Err(AuthError::MissingCookie("JSESSIONID"))
        ));
    }

    #[test]
    fn cookie_header_joins_pairs() {
        let header = sample().cookie_header();
        assert!(header.contains("li_at=AQEFAHkBAAAA"));
        assert!(header.contains("JSESSIONID=\"ajax:0103540587890015905\""));
        assert!(header.contains("; "));
    }

    #[test]
    fn round_trip_json() {
        let auth = sample();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.json");
        auth.save(&path).unwrap();
        let loaded = LinkedInAuth::load(&path).unwrap();
        assert_eq!(loaded.member_urn, auth.member_urn);
        assert_eq!(loaded.cookies, auth.cookies);
    }
}
