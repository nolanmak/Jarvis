//! D3 tamper detection at decision time: the config file's hash is checked
//! before every gate decision (cheap: one read + sha256). On a change the
//! config is reloaded and reconciled against the store (new or changed
//! contacts enter `pending`), and a "Trusted contacts changed" notice is
//! queued for the broker to post.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use augmentagent_approval_discord::autonomy::TrustNotice;
use augmentagent_store::Store;

use augmentagent_store::autonomy::TrustState;

use crate::config::{load_config, LoadReport};
use crate::gate::TrustSource;
use crate::trust::{reconcile, TrustSnapshot};

pub struct TrustWatch {
    store: Arc<Store>,
    path: PathBuf,
    people_dir: Option<PathBuf>,
    /// The last reconciled load, keyed by its file hash.
    cache: Mutex<Option<LoadReport>>,
    /// Notices waiting to be posted.
    notices: Mutex<Vec<TrustNotice>>,
}

impl TrustWatch {
    pub fn new(store: Arc<Store>, path: PathBuf, people_dir: Option<PathBuf>) -> Self {
        Self {
            store,
            path,
            people_dir,
            cache: Mutex::new(None),
            notices: Mutex::new(Vec::new()),
        }
    }

    /// Notices queued by reloads since the last call.
    pub fn take_notices(&self) -> Vec<TrustNotice> {
        std::mem::take(&mut *self.notices.lock().expect("trust notices poisoned"))
    }
}

/// Trust without writing anything: active = contacts whose current entry
/// is confirmed in the store (for `autonomy explain`, and between reloads).
pub fn snapshot_read_only(store: &Store, report: LoadReport) -> TrustSnapshot {
    let mut snap = TrustSnapshot::default();
    for contact in &report.contacts {
        match store.trust_state(&contact.slug, &contact.entry_hash) {
            Ok(Some(TrustState::Active)) => snap.active.push(contact.clone()),
            _ => snap.pending.push(contact.clone()),
        }
    }
    snap.report = report;
    snap
}

impl TrustSource for TrustWatch {
    fn snapshot(&self, now_ms: i64) -> TrustSnapshot {
        let report = load_config(&self.path);
        let mut cache = self.cache.lock().expect("trust cache poisoned");
        let changed = cache
            .as_ref()
            .is_none_or(|c| c.file_hash != report.file_hash);
        if !changed {
            // Same file: trust state may still have changed (Confirm,
            // `trust confirm`), so read it — but nothing is written.
            return snapshot_read_only(&self.store, report);
        }
        match reconcile(
            &self.store,
            report.clone(),
            self.people_dir.as_deref(),
            now_ms,
        ) {
            Ok(snap) => {
                let newly_pending: Vec<(String, String)> = snap
                    .newly_pending
                    .iter()
                    .filter_map(|slug| {
                        report
                            .contact(slug)
                            .map(|c| (slug.clone(), c.entry_hash.clone()))
                    })
                    .collect();
                // Problems are alerted once per change of the file, not on
                // every restart.
                if snap.file_changed || !newly_pending.is_empty() || !snap.slug_notices.is_empty() {
                    let mut alerts = if snap.file_changed {
                        report.alerts()
                    } else {
                        Vec::new()
                    };
                    alerts.extend(snap.slug_notices.iter().cloned());
                    self.notices
                        .lock()
                        .expect("trust notices poisoned")
                        .push(TrustNotice {
                            newly_pending,
                            alerts,
                        });
                }
                *cache = Some(report);
                snap
            }
            Err(e) => {
                tracing::error!(
                    "autonomy: reconciling trusted contacts failed; trusting nobody: {e}"
                );
                TrustSnapshot::default()
            }
        }
    }
}
