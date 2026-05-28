//! SocialAPI.ai credential loading.
//!
//! A single bearer API key fronts the whole account. Resolution order:
//!   1. `SOCIALAPI_API_KEY` environment variable.
//!   2. The shared keyring vault slot `augmentagent/socialapi/default`
//!      (via [`augmentagent_auth`]).
//!
//! If neither yields a non-empty key, [`SocialApiAuth::load`] errors so the
//! daemon can disable the channel cleanly with an actionable message.

use thiserror::Error;

use augmentagent_auth::{Auth as KeychainAuth, AuthError as KeychainError, DEFAULT_ACCOUNT};

/// Keychain platform namespace; combined with [`DEFAULT_ACCOUNT`] to form the
/// single SocialAPI.ai credential slot (`augmentagent/socialapi/default`).
pub const KEYCHAIN_PLATFORM: &str = "socialapi";

/// Environment variable holding the SocialAPI.ai bearer key.
pub const ENV_VAR: &str = "SOCIALAPI_API_KEY";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("keychain: {0}")]
    Keychain(#[from] KeychainError),
    #[error(
        "no SocialAPI.ai key found — set {ENV_VAR} or store one in the \
         keyring slot augmentagent/{KEYCHAIN_PLATFORM}/{DEFAULT_ACCOUNT}"
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
    pub fn load() -> Result<Self, AuthError> {
        if let Ok(key) = std::env::var(ENV_VAR) {
            let key = key.trim();
            if !key.is_empty() {
                return Ok(Self::new(key));
            }
        }
        match KeychainAuth::get(KEYCHAIN_PLATFORM, DEFAULT_ACCOUNT) {
            Ok(bytes) => {
                let key = String::from_utf8_lossy(&bytes).trim().to_string();
                if key.is_empty() {
                    Err(AuthError::Missing)
                } else {
                    Ok(Self::new(key))
                }
            }
            Err(KeychainError::NotFound { .. }) => Err(AuthError::Missing),
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
}
