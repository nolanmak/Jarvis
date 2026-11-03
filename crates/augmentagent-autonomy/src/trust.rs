//! D3 — trust state: pending until the owner confirms, keyed by
//! `(slug, sha256(entry))` in the store, plus tamper detection by file hash.

use std::path::Path;

use augmentagent_store::autonomy::TrustState;
use augmentagent_store::{Store, StoreError};

use crate::config::{LoadReport, TrustedContact};
use crate::handle::Handle;

#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("no valid contact `{0}` in the trusted-contacts config")]
    UnknownContact(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// The config as loaded, split by trust state.
#[derive(Debug, Clone, Default)]
pub struct TrustSnapshot {
    pub report: LoadReport,
    /// Confirmed contacts: the only ones autonomy may act for.
    pub active: Vec<TrustedContact>,
    /// Loaded but unconfirmed (new, or entry changed since confirmation).
    pub pending: Vec<TrustedContact>,
    /// Slugs that entered pending in this reconcile (for the "Trusted
    /// contacts changed" notice with Confirm).
    pub newly_pending: Vec<String>,
    /// The file hash differs from the one stored at the last load —
    /// including edits made while the daemon was down.
    pub file_changed: bool,
    /// One-time "slug missing, run `trust sync`" notices (D3). The contact
    /// stays loaded and keeps its state.
    pub slug_notices: Vec<String>,
}

impl TrustSnapshot {
    /// The active contact whose handles contain `handle` (D4 inbound trust).
    pub fn active_contact_for(&self, handle: &Handle) -> Option<&TrustedContact> {
        self.active.iter().find(|c| c.has_handle(handle))
    }

    pub fn active_contact(&self, slug: &str) -> Option<&TrustedContact> {
        self.active.iter().find(|c| c.slug == slug)
    }

    pub fn is_active(&self, slug: &str) -> bool {
        self.active_contact(slug).is_some()
    }

    pub fn state_of(&self, slug: &str) -> Option<TrustState> {
        if self.is_active(slug) {
            Some(TrustState::Active)
        } else if self.pending.iter().any(|c| c.slug == slug) {
            Some(TrustState::Pending)
        } else {
            None
        }
    }
}

/// Reconcile a load against the store: record the file hash (flagging a
/// change), make every contact without a row for its current entry hash
/// pending, and split contacts into active and pending. With `people_dir`,
/// contacts whose `people/<slug>.md` is missing produce a one-time notice.
pub fn reconcile(
    store: &Store,
    report: LoadReport,
    people_dir: Option<&Path>,
    now_ms: i64,
) -> Result<TrustSnapshot, TrustError> {
    let _ = (store, report, people_dir, now_ms);
    todo!()
}

/// `trust confirm <slug>`: activate the contact's *current* entry. Returns
/// true when it was not already active.
pub fn confirm_contact(
    store: &Store,
    report: &LoadReport,
    slug: &str,
    now_ms: i64,
) -> Result<bool, TrustError> {
    let _ = (store, report, slug, now_ms);
    todo!()
}
