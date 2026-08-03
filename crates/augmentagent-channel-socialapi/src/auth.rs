//! SocialAPI.ai credential loading.
//!
//! A single bearer API key fronts the whole account. Resolution order:
//!   1. `SOCIALAPI_API_KEY` environment variable.
//!   2. The shared keyring vault slot `augmentagent/socialapi/default`
//!      (via [`augmentagent_auth`]).
//!   3. The sqlite `config` table under `socialapi_api_key` — where the
//!      dashboard's paste-your-key card writes (#525).
//!
//! Step 3 needs a [`Store`], so it lives in [`SocialApiAuth::load_with_store`];
//! [`SocialApiAuth::load`] covers the first two for callers that have no store
//! handle. **Prefer `load_with_store`** — the dashboard flow is the documented
//! primary setup path, and a plain `load()` cannot see a key set that way.
//!
//! If no source yields a non-empty key, loading errors so the daemon can
//! disable the channel cleanly with an actionable message.

use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError, DEFAULT_ACCOUNT};
use augmentagent_store::Store;

/// `config` table key the Express dashboard writes the pasted API key to
/// (`setConfig("socialapi_api_key", …)` in `src/dashboard.ts`).
pub const CONFIG_KEY: &str = "socialapi_api_key";

/// Keychain platform namespace; combined with [`DEFAULT_ACCOUNT`] to form the
/// single SocialAPI.ai credential slot (`augmentagent/socialapi/default`).
pub const KEYCHAIN_PLATFORM: &str = "socialapi";

/// Environment variable holding the SocialAPI.ai bearer key.
pub const ENV_VAR: &str = "SOCIALAPI_API_KEY";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("keychain: {0}")]
    Keychain(#[from] KeychainError),
    #[error("config store: {0}")]
    Store(String),
    #[error(
        "no SocialAPI.ai key found — set {ENV_VAR}, store one in the keyring \
         slot augmentagent/{KEYCHAIN_PLATFORM}/{DEFAULT_ACCOUNT}, or paste it \
         into the dashboard's SocialAPI.ai card (sqlite config.{CONFIG_KEY})"
    )]
    Missing,
}

/// Loaded SocialAPI.ai credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocialApiAuth {
    pub api_key: String,
}

impl SocialApiAuth {
    /// Construct directly from a key (tests / explicit wiring).
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }

    /// Resolve a key from the environment, falling back to the keyring vault.
    /// Errors with [`AuthError::Missing`] if neither source yields a
    /// non-empty key.
    ///
    /// This does **not** see a key pasted into the dashboard — that lands in
    /// sqlite. Use [`SocialApiAuth::load_with_store`] wherever a `Store` is in
    /// hand; this exists for the few callers that have none.
    pub fn load() -> Result<Self, AuthError> {
        if let Some(auth) = Self::from_env() {
            return Ok(auth);
        }
        match Self::from_keychain() {
            Ok(Some(auth)) => Ok(auth),
            Ok(None) => Err(AuthError::Missing),
            Err(e) => Err(e),
        }
    }

    /// Full resolution: env var, then keyring vault, then the sqlite `config`
    /// table (`socialapi_api_key`) the dashboard writes to (#525).
    ///
    /// Env stays first so an operator can always override a stale stored key
    /// for one run without touching the keyring or the dashboard.
    ///
    /// A keyring *backend* failure (no Secret Service on a headless box, a
    /// locked or unavailable D-Bus session) does NOT abort the walk — it
    /// falls through to sqlite, because that box is exactly where the
    /// dashboard paste-key flow is the only practical setup path. The keyring
    /// error is still surfaced if sqlite has nothing either, since it is the
    /// more informative diagnosis at that point.
    pub fn load_with_store(store: &Store) -> Result<Self, AuthError> {
        if let Some(auth) = Self::from_env() {
            return Ok(auth);
        }
        let keychain_err = match Self::from_keychain() {
            Ok(Some(auth)) => return Ok(auth),
            Ok(None) => None,
            Err(e) => Some(e),
        };
        match store.get_config(CONFIG_KEY) {
            // `get_config` already trims and treats blank as absent.
            Ok(Some(key)) => Ok(Self::new(key)),
            Ok(None) => Err(keychain_err.unwrap_or(AuthError::Missing)),
            // A store read failure must not masquerade as "no key
            // configured" — that would send the operator chasing setup for a
            // key they already set.
            Err(e) => Err(AuthError::Store(e.to_string())),
        }
    }

    /// Step 1: the environment variable. `None` when unset or blank.
    fn from_env() -> Option<Self> {
        let key = std::env::var(ENV_VAR).ok()?;
        let key = key.trim();
        (!key.is_empty()).then(|| Self::new(key))
    }

    /// Step 2: the keyring vault. `Ok(None)` means "looked, not there";
    /// `Err` means the keyring backend itself failed, which callers may choose
    /// to treat as non-fatal.
    fn from_keychain() -> Result<Option<Self>, AuthError> {
        match KeychainAuth::get(KEYCHAIN_PLATFORM, DEFAULT_ACCOUNT) {
            Ok(bytes) => {
                let key = String::from_utf8_lossy(&bytes).trim().to_string();
                if key.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(Self::new(key)))
                }
            }
            Err(KeychainError::NotFound { .. }) => Ok(None),
            Err(e) => Err(AuthError::Keychain(e)),
        }
    }

    /// Persist the key into the keyring vault.
    pub fn save_to_keychain(&self) -> Result<(), AuthError> {
        KeychainAuth::put(
            KEYCHAIN_PLATFORM,
            DEFAULT_ACCOUNT,
            self.api_key.as_bytes(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `SOCIALAPI_API_KEY` is process-global; serialize the env-touching tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn load_reads_env_var() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_VAR, "sk_test_123");
        let auth = SocialApiAuth::load().unwrap();
        assert_eq!(auth.api_key, "sk_test_123");
        std::env::remove_var(ENV_VAR);
    }

    #[test]
    fn load_trims_whitespace() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_VAR, "  sk_padded  ");
        let auth = SocialApiAuth::load().unwrap();
        assert_eq!(auth.api_key, "sk_padded");
        std::env::remove_var(ENV_VAR);
    }

    #[test]
    fn new_stores_key() {
        assert_eq!(SocialApiAuth::new("k").api_key, "k");
    }

    /// True when this machine has a real `augmentagent/socialapi/default`
    /// keyring entry. The keyring is process-global and not injectable, so a
    /// developer box with a live key would shadow the sqlite step these tests
    /// exist to exercise. Skip rather than assert something environment
    /// -dependent.
    fn keychain_has_real_key() -> bool {
        matches!(SocialApiAuth::from_keychain(), Ok(Some(_)))
    }

    /// #525: the dashboard's paste-key card writes to sqlite, so the daemon
    /// must resolve from there when env and keyring are empty.
    #[test]
    fn load_with_store_falls_back_to_sqlite_config() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_VAR);
        if keychain_has_real_key() {
            eprintln!("skipping: live socialapi keyring entry shadows the sqlite step");
            return;
        }
        let f = tempfile::NamedTempFile::new().unwrap();
        let store = Store::open(f.path()).unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO config (key, value, updatedAt) VALUES (?1, ?2, 0)",
                    [CONFIG_KEY, "  sk_from_dashboard  "],
                )
            })
            .unwrap();
        let auth = SocialApiAuth::load_with_store(&store).unwrap();
        assert_eq!(auth.api_key, "sk_from_dashboard");
    }

    /// Env still wins so an operator can override a stored key for one run.
    #[test]
    fn load_with_store_prefers_env_over_sqlite() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let f = tempfile::NamedTempFile::new().unwrap();
        let store = Store::open(f.path()).unwrap();
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO config (key, value, updatedAt) VALUES (?1, ?2, 0)",
                    [CONFIG_KEY, "sk_from_dashboard"],
                )
            })
            .unwrap();
        std::env::set_var(ENV_VAR, "sk_from_env");
        let auth = SocialApiAuth::load_with_store(&store).unwrap();
        std::env::remove_var(ENV_VAR);
        assert_eq!(auth.api_key, "sk_from_env");
    }

    /// Nothing anywhere is still a clean, actionable Missing.
    #[test]
    fn load_with_store_missing_when_no_source_has_a_key() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_VAR);
        if keychain_has_real_key() {
            eprintln!("skipping: live socialapi keyring entry means a key IS resolvable");
            return;
        }
        let f = tempfile::NamedTempFile::new().unwrap();
        let store = Store::open(f.path()).unwrap();
        // A blank config value must read as unset, not as a key of spaces.
        store
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO config (key, value, updatedAt) VALUES (?1, ?2, 0)",
                    [CONFIG_KEY, "   "],
                )
            })
            .unwrap();
        let err = SocialApiAuth::load_with_store(&store).unwrap_err();
        // On a box with a working-but-empty keyring this is Missing; on one
        // with no keyring backend at all the keychain error is surfaced
        // instead. Either way it must NOT be a success.
        assert!(matches!(err, AuthError::Missing | AuthError::Keychain(_)), "got {err:?}");
    }

    #[test]
    fn from_env_trims_and_treats_blank_as_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_VAR, "   ");
        assert!(SocialApiAuth::from_env().is_none());
        std::env::set_var(ENV_VAR, "  sk_x  ");
        assert_eq!(SocialApiAuth::from_env().unwrap().api_key, "sk_x");
        std::env::remove_var(ENV_VAR);
    }
}
