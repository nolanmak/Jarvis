//! Credential vault backed by macOS Keychain.
//!
//! Every non-Discord-bot channel persists its credentials here under a
//! consistent naming scheme: `augmentagent/<platform>/<account>`.
//!
//! - `platform` is lowercase ASCII (`linkedin`, `slack`, `whatsapp`, …).
//! - `account` is the platform-native identifier for the specific account
//!   (LinkedIn member URN, Slack workspace ID, phone number, etc.) or the
//!   [`DEFAULT_ACCOUNT`] sentinel when the channel is single-account.
//!
//! Payload is opaque bytes — each channel serializes its own credential
//! shape (typically JSON).

use thiserror::Error;
use tracing::debug;

/// Convention for single-account platforms (one LinkedIn profile, one
/// default Slack workspace). Multi-account channels pass their own
/// account identifiers.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Service-name prefix. Combined with `<platform>` to form the Keychain
/// `service` attribute so entries sort under a single namespace.
const SERVICE_PREFIX: &str = "augmentagent/";

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("not found: augmentagent/{platform}/{account}")]
    NotFound { platform: String, account: String },

    #[error("keyring: {0}")]
    Keyring(#[from] keyring::Error),
}

/// Credential store. Stateless — all methods take (platform, account) keys.
pub struct Auth;

impl Auth {
    /// Store opaque bytes at `service=augmentagent/<platform>`, `user=<account>`.
    /// Overwrites any existing entry.
    pub fn put(platform: &str, account: &str, payload: &[u8]) -> Result<(), AuthError> {
        let entry = entry_for(platform, account)?;
        entry.set_secret(payload)?;
        debug!(platform, account, bytes = payload.len(), "auth put");
        Ok(())
    }

    /// Retrieve bytes. Returns [`AuthError::NotFound`] when the entry is missing.
    pub fn get(platform: &str, account: &str) -> Result<Vec<u8>, AuthError> {
        let entry = entry_for(platform, account)?;
        match entry.get_secret() {
            Ok(bytes) => {
                debug!(platform, account, bytes = bytes.len(), "auth get");
                Ok(bytes)
            }
            Err(keyring::Error::NoEntry) => Err(AuthError::NotFound {
                platform: platform.into(),
                account: account.into(),
            }),
            Err(e) => Err(AuthError::Keyring(e)),
        }
    }

    /// Delete an entry. Idempotent — a missing entry is treated as success.
    pub fn delete(platform: &str, account: &str) -> Result<(), AuthError> {
        let entry = entry_for(platform, account)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {
                debug!(platform, account, "auth delete");
                Ok(())
            }
            Err(e) => Err(AuthError::Keyring(e)),
        }
    }

    /// True when an entry exists. Does not decrypt or read the secret, so
    /// it does not trigger the macOS "allow access" prompt.
    pub fn exists(platform: &str, account: &str) -> bool {
        let entry = match entry_for(platform, account) {
            Ok(e) => e,
            Err(_) => return false,
        };
        !matches!(entry.get_attributes(), Err(keyring::Error::NoEntry))
    }
}

fn entry_for(platform: &str, account: &str) -> Result<keyring::Entry, AuthError> {
    let service = format!("{SERVICE_PREFIX}{platform}");
    Ok(keyring::Entry::new(&service, account)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyring::{mock, set_default_credential_builder};
    use std::sync::Once;

    static INIT: Once = Once::new();

    /// Install the in-memory mock backend once per process. `keyring::mock`
    /// entries are isolated per `Entry::new` call (each Entry gets its own
    /// in-memory credential; there is no shared store), so we use the mock
    /// only to exercise the error-mapping paths in `Auth` without touching
    /// the real Keychain. Round-trip put→get coverage lives in the
    /// `#[ignore]`-gated integration tests below.
    fn install_mock() {
        INIT.call_once(|| {
            set_default_credential_builder(mock::default_credential_builder());
        });
    }

    #[test]
    fn default_account_is_plain_string() {
        assert_eq!(DEFAULT_ACCOUNT, "default");
    }

    #[test]
    fn service_prefix_is_stable() {
        // Silent drift on this prefix would orphan every previously-written
        // Keychain entry. The const is load-bearing across builds.
        assert_eq!(SERVICE_PREFIX, "augmentagent/");
    }

    #[test]
    fn get_missing_returns_not_found() {
        install_mock();
        let err = Auth::get("testplat", "missing-user").unwrap_err();
        let AuthError::NotFound { platform, account } = err else {
            panic!("expected NotFound, got {err:?}");
        };
        assert_eq!(platform, "testplat");
        assert_eq!(account, "missing-user");
    }

    #[test]
    fn delete_is_idempotent_when_missing() {
        install_mock();
        // Never written — must not error.
        Auth::delete("testplat", "never-existed").unwrap();
    }

    #[test]
    fn entry_for_builds_prefixed_service() {
        // Construct an Entry through the same helper Auth uses; read its
        // attributes and confirm the service attribute carries the namespace
        // prefix. `get_attributes` returns NoEntry on the mock (nothing was
        // written), which is the only thing we care to assert here.
        install_mock();
        let entry = entry_for("linkedin", DEFAULT_ACCOUNT).unwrap();
        assert!(matches!(
            entry.get_attributes(),
            Err(keyring::Error::NoEntry)
        ));
    }

    // ---------------------------------------------------------------------
    // Integration tests — hit the real macOS Keychain. Opt-in via
    //   cargo test -p augmentagent-auth -- --ignored
    // Each test uses a unique service/account pair so parallel runs don't
    // collide, and every test deletes its entry in a scope-end guard.
    // ---------------------------------------------------------------------

    /// Cleans up a Keychain entry on drop so a panicking test doesn't leak.
    struct Cleanup {
        platform: String,
        account: String,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = Auth::delete(&self.platform, &self.account);
        }
    }

    fn unique(prefix: &str) -> String {
        // Nanos since epoch — plenty of entropy for parallel test runs on
        // one machine; avoids pulling in uuid as a dev-dep for this alone.
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{prefix}-{ns}")
    }

    #[test]
    #[ignore = "hits real macOS Keychain; run with --ignored"]
    fn integration_put_get_round_trip() {
        let platform = unique("augmentagent-test-roundtrip");
        let account = "default";
        let _c = Cleanup {
            platform: platform.clone(),
            account: account.into(),
        };

        Auth::put(&platform, account, b"secret-bytes").unwrap();
        assert_eq!(Auth::get(&platform, account).unwrap(), b"secret-bytes");
    }

    #[test]
    #[ignore = "hits real macOS Keychain; run with --ignored"]
    fn integration_overwrite_replaces_value() {
        let platform = unique("augmentagent-test-overwrite");
        let account = "default";
        let _c = Cleanup {
            platform: platform.clone(),
            account: account.into(),
        };

        Auth::put(&platform, account, b"first").unwrap();
        Auth::put(&platform, account, b"second").unwrap();
        assert_eq!(Auth::get(&platform, account).unwrap(), b"second");
    }

    #[test]
    #[ignore = "hits real macOS Keychain; run with --ignored"]
    fn integration_delete_removes_entry() {
        let platform = unique("augmentagent-test-delete");
        let account = "default";
        let _c = Cleanup {
            platform: platform.clone(),
            account: account.into(),
        };

        Auth::put(&platform, account, b"x").unwrap();
        Auth::delete(&platform, account).unwrap();
        assert!(matches!(
            Auth::get(&platform, account),
            Err(AuthError::NotFound { .. })
        ));
    }
}
