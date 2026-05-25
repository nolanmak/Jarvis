//! Just-in-time secret loader for the wiki-query call boundary (#128).
//!
//! The Discord wiki-query agent is the most prompt-injectable surface in
//! the system (free-form Discord input + ingested email bodies). Until
//! 2026-05-25 the daemon process held `GROQ_API_KEY`, `CEREBRAS_API_KEY`,
//! and `COMPOSIO_API_KEY` in its env (loaded from `.env`), which the
//! spawned `claude` CLI inherited and the `Read` tool could exfiltrate.
//!
//! This module looks up provider keys in the Linux Secret Service
//! (`gnome-keyring` via the `augmentagent-auth` crate) at the call
//! boundary, with `std::env::var` as a development-time fallback. The
//! caller then forwards only the specific keys a given sub-CLI needs
//! via the spawned process's explicit env list (`Command::envs`),
//! rather than letting it inherit the full daemon env.
//!
//! ## Keyring layout
//!
//! Keys are stored under `service=augmentagent/api-key`, `user=<KEY_NAME>`:
//!
//! ```text
//! augmentagent/api-key  COMPOSIO_API_KEY     <secret>
//! augmentagent/api-key  GROQ_API_KEY         <secret>
//! augmentagent/api-key  CEREBRAS_API_KEY     <secret>
//! ```
//!
//! Seed via the one-shot `augmentagent migrate-secrets-to-keyring` CLI
//! command (reads `.env` and writes each key into the keyring slot).

use augmentagent_auth::{Auth, AuthError};
use tracing::{debug, warn};

/// Service slot in the Linux Secret Service for provider API keys.
/// Distinct from per-channel credential slots so a stray `auth_delete`
/// for one platform can't take down all the LLM/Composio keys.
pub const SECRET_SERVICE_API_KEY: &str = "api-key";

/// Resolve a provider key by name. Keyring first; `std::env::var`
/// fallback (logged at warn so silent .env reliance is visible).
///
/// Returns `None` when neither source has the key — callers decide
/// whether the absence is fatal or just degrades a feature (e.g. the
/// wiki-query agent can still answer wiki-only questions when
/// `COMPOSIO_API_KEY` is missing; the `gmail search` sub-CLI will just
/// error and the LLM moves on per the "tool errors are not full stops"
/// rule in `schema/wiki-ask.md`).
pub fn load_provider_key(name: &str) -> Option<String> {
    match Auth::get(SECRET_SERVICE_API_KEY, name) {
        Ok(bytes) => {
            debug!(key = name, source = "keyring", "secret loaded");
            match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(_) => {
                    warn!(
                        key = name,
                        "keyring entry is not valid UTF-8; ignoring \
                         and falling through to env var"
                    );
                    std::env::var(name).ok()
                }
            }
        }
        Err(AuthError::NotFound { .. }) => match std::env::var(name) {
            Ok(v) => {
                warn!(
                    key = name,
                    "secret not in keyring; falling back to env var. \
                     Run `augmentagent migrate-secrets-to-keyring` to \
                     move {name} out of .env into Linux Secret Service"
                );
                Some(v)
            }
            Err(_) => None,
        },
        Err(e) => {
            // Keyring backend itself failed — log and try env. A broken
            // keyring shouldn't take the daemon offline.
            warn!(
                key = name,
                error = %e,
                "keyring read failed; falling back to env var"
            );
            std::env::var(name).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `load_provider_key` falls back to the env when the keyring entry
    /// is absent. We can't easily seed the real keyring in CI, so this
    /// test only exercises the env-fallback path (which is what dev/CI
    /// machines without a working Secret Service backend will hit).
    #[test]
    fn falls_back_to_env_when_keyring_absent() {
        // Pick a key the keyring is overwhelmingly unlikely to hold.
        let key = format!(
            "AUGMENTAGENT_TEST_KEY_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        // Without the env var, returns None.
        assert!(load_provider_key(&key).is_none());

        // With the env var, returns the value.
        std::env::set_var(&key, "from-env");
        let got = load_provider_key(&key);
        std::env::remove_var(&key);
        assert_eq!(got.as_deref(), Some("from-env"));
    }
}
