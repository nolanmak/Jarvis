//! D3 tamper detection at decision time: the config file's hash is checked
//! before every gate decision (cheap: one read + sha256). On a change the
//! config is reloaded and reconciled against the store (new or changed
//! contacts enter `pending`), and a "Trusted contacts changed" notice is
//! queued for the broker to post.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use augmentagent_approval_discord::autonomy::TrustNotice;
use augmentagent_store::Store;

use crate::config::LoadReport;
use crate::gate::TrustSource;
use crate::trust::TrustSnapshot;

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
/// is confirmed in the store (for `autonomy explain`).
pub fn snapshot_read_only(store: &Store, report: LoadReport) -> TrustSnapshot {
    let _ = (store, report);
    todo!("C7: read-only snapshot")
}

impl TrustSource for TrustWatch {
    fn snapshot(&self, now_ms: i64) -> TrustSnapshot {
        let _ = (now_ms, &self.store, &self.path, &self.people_dir, &self.cache);
        todo!("C7: trust watch")
    }
}
