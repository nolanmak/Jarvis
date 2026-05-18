//! Persisted Instagram session state.
//!
//! Instagram's private web API takes session cookies harvested once from a
//! logged-in browser (`scripts/instagram-harvest.sh`) plus a small set of
//! fingerprint headers (`x-ig-app-id`, `x-asbd-id`, `x-mid`). We store these
//! in the keychain under `augmentagent/instagram/<ds_user_id>` and a legacy
//! JSON file fallback, mirroring the LinkedIn channel.
//!
//! On 401 / `checkpoint_required` we surface a clear error so the user knows
//! to re-harvest.
//!
//! File shape:
//!
//! ```json
//! {
//!   "ds_user_id": "456",
//!   "username": "nolanmak",
//!   "cookies": {
//!     "sessionid": "456%3A...%3A...",
//!     "csrftoken": "abc...",
//!     "ds_user_id": "456",
//!     "mid": "Z...",
//!     "ig_did": "0000-..."
//!   },
//!   "user_agent": "Mozilla/5.0 ...",
//!   "harvested_at_ms": 1776600000000
//! }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Web app id — the single most load-bearing non-cookie header. Stable for
/// years; documented in docs/instagram-protocol.md. Env override for the day
/// Instagram finally rotates it without a recompile.
pub const DEFAULT_IG_APP_ID: &str = "936619743392459";

/// Anti-scraping bot-detection id. Rotates rarely; env-overridable.
pub const DEFAULT_ASBD_ID: &str = "129477";

pub const DEFAULT_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

/// Keychain platform namespace. Combined with the account id (`ds_user_id`)
/// to form the credential slot `augmentagent/instagram/<ds_user_id>`.
pub const KEYCHAIN_PLATFORM: &str = "instagram";

/// Cookies that aren't strictly required to make a request but whose absence
/// materially raises `checkpoint_required` risk (the web client always sends
/// them). The validation harness warns on these; it does not hard-fail.
pub const RECOMMENDED_COOKIES: &[&str] = &["ds_user_id", "mid", "ig_did", "rur"];

/// A harvested session older than this is likely close to expiry / drift —
/// the harness flags it so the operator re-harvests proactively rather than
/// discovering it via a mid-poll `login_required`. IG web sessions are
/// long-lived but not indefinite; 60d is a conservative re-harvest nudge.
pub const SESSION_STALE_MS: i64 = 60 * 24 * 3600 * 1000;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstagramAuth {
    /// Your own numeric account id. Also the keychain account slot.
    pub ds_user_id: String,
    /// Your @handle — informational; used in log lines + the harvest probe.
    #[serde(default)]
    pub username: String,
    /// Name → value map. Must include `sessionid` and `csrftoken`.
    pub cookies: BTreeMap<String, String>,
    #[serde(default = "default_user_agent")]
    pub user_agent: String,
    #[serde(default)]
    pub harvested_at_ms: i64,
}

fn default_user_agent() -> String {
    DEFAULT_USER_AGENT.to_string()
}

/// Soft session-hygiene report (see [`InstagramAuth::health`]). Serialized
/// into the `instagram validate` JSON output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthHealth {
    /// `validate()` passed — a request can actually be constructed.
    pub hard_ok: bool,
    /// `sessionid` numeric prefix matches `ds_user_id`.
    pub sessionid_matches: bool,
    /// Recommended-but-not-required cookies that are absent.
    pub missing_recommended: Vec<String>,
    /// Age of the harvest in ms (0 if `harvested_at_ms` was unset).
    pub age_ms: i64,
    /// Harvest is older than [`SESSION_STALE_MS`].
    pub stale: bool,
    /// Human-readable advisories (empty ⇒ clean bill of health).
    pub warnings: Vec<String>,
}

impl InstagramAuth {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, AuthError> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: InstagramAuth = serde_json::from_str(&raw)?;
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
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), AuthError> {
        if self.ds_user_id.is_empty() {
            return Err(AuthError::Invalid("empty ds_user_id".into()));
        }
        if !self.cookies.contains_key("sessionid") {
            return Err(AuthError::MissingCookie("sessionid"));
        }
        if !self.cookies.contains_key("csrftoken") {
            return Err(AuthError::MissingCookie("csrftoken"));
        }
        Ok(())
    }

    /// Non-fatal session hygiene report consumed by `instagram validate`.
    /// `validate()` is the hard gate (request can't be built without it);
    /// this layers the soft signals (recommended-cookie coverage, harvest
    /// staleness, ds_user_id↔sessionid consistency) so the operator can
    /// re-harvest *before* a mid-poll `login_required`.
    pub fn health(&self, now_ms: i64) -> AuthHealth {
        let mut warnings = Vec::new();
        let hard_ok = self.validate().is_ok();
        if let Err(e) = self.validate() {
            warnings.push(format!("HARD: {e}"));
        }

        let missing_recommended: Vec<String> = RECOMMENDED_COOKIES
            .iter()
            .filter(|c| !self.cookies.contains_key(**c))
            .map(|c| c.to_string())
            .collect();
        if !missing_recommended.is_empty() {
            warnings.push(format!(
                "missing recommended cookies {missing_recommended:?} — raises checkpoint risk; \
                 re-harvest with a full cookie jar"
            ));
        }

        // `sessionid` is URL-encoded `<ds_user_id>%3A...`; the prefix must
        // match the declared account id or the cookie is for another login.
        let sessionid_matches = self
            .cookies
            .get("sessionid")
            .map(|s| {
                let head: String =
                    s.chars().take_while(|c| c.is_ascii_digit()).collect();
                !head.is_empty() && head == self.ds_user_id
            })
            .unwrap_or(false);
        if self.cookies.contains_key("sessionid") && !sessionid_matches {
            warnings.push(
                "sessionid prefix does not match ds_user_id — cookie jar is \
                 for a different account or malformed"
                    .into(),
            );
        }

        let age_ms = if self.harvested_at_ms > 0 {
            (now_ms - self.harvested_at_ms).max(0)
        } else {
            warnings.push(
                "harvested_at_ms is 0 — cannot assess session age; \
                 re-run `instagram login` to stamp it"
                    .into(),
            );
            0
        };
        let stale = self.harvested_at_ms > 0 && age_ms > SESSION_STALE_MS;
        if stale {
            warnings.push(format!(
                "session harvested {}d ago (>{}d) — re-harvest proactively",
                age_ms / (24 * 3600 * 1000),
                SESSION_STALE_MS / (24 * 3600 * 1000)
            ));
        }

        AuthHealth {
            hard_ok,
            sessionid_matches,
            missing_recommended,
            age_ms,
            stale,
            warnings,
        }
    }

    /// The `x-csrftoken` header value Instagram expects — the raw value of
    /// the `csrftoken` cookie (no transformation, unlike LinkedIn's
    /// JSESSIONID quote-strip).
    pub fn csrf_token(&self) -> Result<String, AuthError> {
        self.cookies
            .get("csrftoken")
            .cloned()
            .ok_or(AuthError::MissingCookie("csrftoken"))
    }

    /// `x-mid` header value — mirrors the `mid` cookie when present.
    pub fn machine_id(&self) -> Option<String> {
        self.cookies.get("mid").cloned()
    }

    /// `_uuid` form field used by write endpoints — the `ig_did` device id.
    /// Falls back to a stable derived value if the cookie is absent so we
    /// never send an empty `_uuid`.
    pub fn device_uuid(&self) -> String {
        self.cookies
            .get("ig_did")
            .cloned()
            .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".into())
    }

    /// Serialize the cookie jar as a `Cookie:` header value.
    pub fn cookie_header(&self) -> String {
        self.cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Read from keychain at `augmentagent/instagram/<ds_user_id>`. The
    /// account slot is the numeric id so multi-account is a future no-op.
    pub fn load_from_keychain(ds_user_id: &str) -> Result<Self, AuthError> {
        let bytes = augmentagent_auth::Auth::get(KEYCHAIN_PLATFORM, ds_user_id)?;
        let parsed: InstagramAuth = serde_json::from_slice(&bytes)?;
        parsed.validate()?;
        Ok(parsed)
    }

    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        let bytes = serde_json::to_vec(self)?;
        augmentagent_auth::Auth::put(KEYCHAIN_PLATFORM, &self.ds_user_id, &bytes)?;
        Ok(())
    }

    /// Load from keychain first; fall back to the legacy auth file under
    /// [`default_auth_path`]. On a file-fallback hit, promote into keychain
    /// so subsequent loads skip the file. The `ds_user_id` is read out of the
    /// file when keychain has no entry (we don't know the account slot up
    /// front on a cold box).
    pub fn load_with_migration(repo_root: &Path) -> Result<Self, AuthError> {
        // We can't ask the keychain "any instagram entry" — the slot is the
        // ds_user_id. So consult the file first to learn the id, then prefer
        // the keychain copy if one exists for that id.
        let path = default_auth_path(repo_root);
        match Self::load(&path) {
            Ok(file_auth) => match Self::load_from_keychain(&file_auth.ds_user_id) {
                Ok(kc) => {
                    tracing::debug!("instagram auth loaded from keychain");
                    Ok(kc)
                }
                Err(AuthError::Keychain(augmentagent_auth::AuthError::NotFound {
                    ..
                })) => {
                    match file_auth.save_to_keychain() {
                        Ok(()) => tracing::info!(
                            from = %path.display(),
                            "instagram auth migrated to keychain from legacy file",
                        ),
                        Err(e) => tracing::warn!(
                            error = %e,
                            "instagram auth loaded from file but keychain write failed",
                        ),
                    }
                    Ok(file_auth)
                }
                Err(e) => Err(e),
            },
            Err(AuthError::Io(_)) => {
                // No file. The keychain path needs a known account id; without
                // a file we can't derive it, so this is a clean "not
                // configured" — bubble a clear error the CLI maps to disabled.
                Err(AuthError::Invalid(format!(
                    "no instagram auth file at {} and no ds_user_id to probe keychain — run `augmentagent instagram login`",
                    path.display()
                )))
            }
            Err(e) => Err(e),
        }
    }
}

/// Default on-disk location for the cookie file. Ordered:
/// 1. `AUGMENTAGENT_INSTAGRAM_AUTH` env override
/// 2. `<repo_root>/instagram-auth.json` (dev fallback)
///
/// (No `/Volumes/...` vault branch — this is a Linux-only deploy per
/// CLAUDE.md; the vault-mount path is a macOS-only no-op here.)
pub fn default_auth_path(repo_root: &Path) -> PathBuf {
    if let Ok(custom) = std::env::var("AUGMENTAGENT_INSTAGRAM_AUTH") {
        return PathBuf::from(custom);
    }
    repo_root.join("instagram-auth.json")
}

/// Resolve the `x-ig-app-id` (env-overridable for the rotation day).
pub fn ig_app_id() -> String {
    std::env::var("AUGMENTAGENT_INSTAGRAM_APP_ID")
        .unwrap_or_else(|_| DEFAULT_IG_APP_ID.to_string())
}

/// Resolve the `x-asbd-id` (env-overridable).
pub fn asbd_id() -> String {
    std::env::var("AUGMENTAGENT_INSTAGRAM_ASBD_ID")
        .unwrap_or_else(|_| DEFAULT_ASBD_ID.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InstagramAuth {
        let mut cookies = BTreeMap::new();
        cookies.insert("sessionid".into(), "456%3Aabc%3A12".into());
        cookies.insert("csrftoken".into(), "tok123".into());
        cookies.insert("ds_user_id".into(), "456".into());
        cookies.insert("mid".into(), "ZmIDmid".into());
        cookies.insert("ig_did".into(), "DEAD-BEEF".into());
        InstagramAuth {
            ds_user_id: "456".into(),
            username: "nolanmak".into(),
            cookies,
            user_agent: DEFAULT_USER_AGENT.into(),
            harvested_at_ms: 1776600000000,
        }
    }

    #[test]
    fn csrf_is_raw_cookie_value() {
        assert_eq!(sample().csrf_token().unwrap(), "tok123");
    }

    #[test]
    fn machine_id_mirrors_mid_cookie() {
        assert_eq!(sample().machine_id().as_deref(), Some("ZmIDmid"));
        let mut a = sample();
        a.cookies.remove("mid");
        assert_eq!(a.machine_id(), None);
    }

    #[test]
    fn device_uuid_falls_back_when_absent() {
        let mut a = sample();
        a.cookies.remove("ig_did");
        assert_eq!(a.device_uuid(), "00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn validate_rejects_missing_cookies() {
        let mut a = sample();
        a.cookies.remove("sessionid");
        assert!(matches!(
            a.validate(),
            Err(AuthError::MissingCookie("sessionid"))
        ));
        let mut a = sample();
        a.cookies.remove("csrftoken");
        assert!(matches!(
            a.validate(),
            Err(AuthError::MissingCookie("csrftoken"))
        ));
    }

    #[test]
    fn cookie_header_joins_pairs() {
        let h = sample().cookie_header();
        assert!(h.contains("sessionid=456%3Aabc%3A12"));
        assert!(h.contains("csrftoken=tok123"));
        assert!(h.contains("; "));
    }

    #[test]
    fn round_trip_json() {
        let auth = sample();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ig.json");
        auth.save(&path).unwrap();
        let loaded = InstagramAuth::load(&path).unwrap();
        assert_eq!(loaded.ds_user_id, auth.ds_user_id);
        assert_eq!(loaded.cookies, auth.cookies);
    }

    #[test]
    fn app_id_env_override() {
        std::env::set_var("AUGMENTAGENT_INSTAGRAM_APP_ID", "999");
        assert_eq!(ig_app_id(), "999");
        std::env::remove_var("AUGMENTAGENT_INSTAGRAM_APP_ID");
        assert_eq!(ig_app_id(), DEFAULT_IG_APP_ID);
    }

    #[test]
    fn health_clean_session_has_no_warnings() {
        let mut a = sample();
        // sessionid prefix must match ds_user_id ("456") for a clean bill.
        a.cookies
            .insert("sessionid".into(), "456%3Aabc%3A12".into());
        a.cookies.insert("rur".into(), "EAG".into());
        let now = a.harvested_at_ms + 1000;
        let h = a.health(now);
        assert!(h.hard_ok);
        assert!(h.sessionid_matches);
        assert!(h.missing_recommended.is_empty());
        assert!(!h.stale);
        assert!(h.warnings.is_empty(), "warnings: {:?}", h.warnings);
    }

    #[test]
    fn health_flags_missing_recommended_cookies() {
        let mut a = sample();
        a.cookies.remove("mid");
        a.cookies.remove("ig_did");
        let h = a.health(a.harvested_at_ms + 1000);
        assert!(h.hard_ok); // sessionid + csrftoken still present
        assert!(h.missing_recommended.contains(&"mid".to_string()));
        assert!(h.missing_recommended.contains(&"ig_did".to_string()));
        assert!(h.warnings.iter().any(|w| w.contains("checkpoint risk")));
    }

    #[test]
    fn health_flags_stale_harvest_and_mismatched_sessionid() {
        let mut a = sample();
        a.cookies
            .insert("sessionid".into(), "999%3Awrong%3A1".into());
        a.harvested_at_ms = 1;
        let now = SESSION_STALE_MS + 10 * 24 * 3600 * 1000;
        let h = a.health(now);
        assert!(h.stale);
        assert!(!h.sessionid_matches);
        assert!(h.warnings.iter().any(|w| w.contains("re-harvest")));
        assert!(h
            .warnings
            .iter()
            .any(|w| w.contains("different account")));
    }

    #[test]
    fn health_flags_unstamped_harvest() {
        let mut a = sample();
        a.harvested_at_ms = 0;
        let h = a.health(1_000_000);
        assert_eq!(h.age_ms, 0);
        assert!(!h.stale);
        assert!(h.warnings.iter().any(|w| w.contains("cannot assess")));
    }

    #[test]
    fn health_reports_hard_failure_for_missing_sessionid() {
        let mut a = sample();
        a.cookies.remove("sessionid");
        let h = a.health(a.harvested_at_ms + 1);
        assert!(!h.hard_ok);
        assert!(h.warnings.iter().any(|w| w.starts_with("HARD:")));
    }

    #[test]
    fn default_auth_path_env_override() {
        std::env::set_var("AUGMENTAGENT_INSTAGRAM_AUTH", "/tmp/ig-x.json");
        assert_eq!(
            default_auth_path(Path::new("/repo")),
            PathBuf::from("/tmp/ig-x.json")
        );
        std::env::remove_var("AUGMENTAGENT_INSTAGRAM_AUTH");
        assert_eq!(
            default_auth_path(Path::new("/repo")),
            PathBuf::from("/repo/instagram-auth.json")
        );
    }
}
