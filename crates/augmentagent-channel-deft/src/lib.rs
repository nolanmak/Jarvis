//! Deft (Deftform) channel — **spike scaffold (#116)**.
//!
//! "Deft" resolves to **Deftform** (`deftform.com`), a form builder with a
//! *documented public REST API* + a first-class submission webhook. A form
//! submission becomes an agent command-and-control instruction, routed
//! through the same triage→draft→approval pipeline every other channel uses.
//!
//! **There is no reverse-engineering here** — the sanctioned API exists, so
//! the #40-style ban-risk gate that governed WhatsApp does not apply. The
//! crate is nonetheless shipped **inert**: every network call is gated on
//! [`deft_enabled`] (`AUGMENTAGENT_DEFT_ENABLED`) so it is provably a no-op in
//! production until explicitly armed and a workspace token is persisted.
//!
//! See `docs/deft-protocol.md` for the full protocol + the open-question /
//! TODO map. Layout mirrors `augmentagent-channel-github`:
//! `lib` / `auth` / `api` / `channel` / `types`.

pub mod api;
pub mod auth;
pub mod channel;
pub mod types;

pub use api::{DeftApi, DeftClient, DeftError};
pub use auth::{DeftAuth, KEYCHAIN_PLATFORM};
pub use channel::{DeftChannel, DeftChannelConfig, DEFAULT_POLL_SECS};
pub use types::{
    command_from_submission, is_deft_email, webhook_submissions, DeftCommand, DeftForm,
    DeftSubmission,
};

/// Platform discriminator written to `Email::platform` and
/// `channel_subscriptions.platform`.
pub const PLATFORM: &str = "deft";

/// `account_entity_id` prefix so stored rows route back to the right Deftform
/// workspace at action time.
pub const ACCOUNT_ENTITY_ID_PREFIX: &str = "deft";

/// Arming gate. The crate performs **zero** network I/O unless this env var
/// is truthy. This is a least-privilege / blast-radius control for a
/// command-and-control surface — NOT a ban-risk gate (the Deftform API is
/// sanctioned; see `docs/deft-protocol.md` §1).
pub fn deft_enabled() -> bool {
    matches!(
        std::env::var("AUGMENTAGENT_DEFT_ENABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Test-only serialization guard. `AUGMENTAGENT_DEFT_ENABLED` is process-global
/// and Rust runs unit tests in parallel within one process, so every test that
/// mutates the gate MUST hold this lock for its duration. An *async* mutex is
/// used deliberately (it's designed to be held across `.await`, unlike a
/// `std::sync::MutexGuard` which trips `clippy::await_holding_lock`).
/// Production code never touches this — the gate there is purely env-driven,
/// which is the correct behavior.
#[cfg(test)]
pub(crate) async fn gate_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    use tokio::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::const_new(());
    LOCK.lock().await
}

/// Set/clear the gate env var. Test-only helper so the set+reset is in one
/// place and always paired with the lock.
#[cfg(test)]
pub(crate) fn set_gate(on: bool) {
    if on {
        std::env::set_var("AUGMENTAGENT_DEFT_ENABLED", "1");
    } else {
        std::env::remove_var("AUGMENTAGENT_DEFT_ENABLED");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gate_defaults_off() {
        let _g = gate_test_lock().await;
        // The default (env unset) MUST be inert.
        set_gate(false);
        assert!(!deft_enabled());
    }

    #[tokio::test]
    async fn gate_accepts_truthy() {
        let _g = gate_test_lock().await;
        std::env::set_var("AUGMENTAGENT_DEFT_ENABLED", "true");
        assert!(deft_enabled());
        std::env::set_var("AUGMENTAGENT_DEFT_ENABLED", "1");
        assert!(deft_enabled());
        std::env::set_var("AUGMENTAGENT_DEFT_ENABLED", "off");
        assert!(!deft_enabled());
        set_gate(false);
    }
}
