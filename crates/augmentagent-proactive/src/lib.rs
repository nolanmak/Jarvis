//! Proactive CRM scanner. Scheduled `ScheduledScan` impls walk the wiki +
//! sqlite, emit `ProactiveSignal`s, and the runner persists + dispatches them
//! through the existing `ApprovalBroker`. See issue #81.

pub mod person;
pub mod rules;
pub mod runner;
pub mod scan;
pub mod store_ext;
pub mod suppress;

pub use scan::{
    Cadence, ProactiveSignal, ScanCtx, ScheduledScan, SignalKind, SuggestedAction, Urgency,
};
pub use store_ext::{ProactiveGateStore, ProactiveStore, SignalStatus, StoredSignal};
pub use suppress::{ProactiveActionsStore, TableSuppression, UserAction};

/// Test-only helpers. The Rust `Store::migrate()` is *additive* over the
/// Node-owned base schema (`actions`, `emails`, … live in `src/db.ts`), so a
/// fresh sqlite file needs those tables seeded before `Store::open` will
/// migrate cleanly. Mirrors the seed other channel crates use in-tree.
#[cfg(test)]
pub(crate) mod testutil {
    use augmentagent_store::Store;
    use std::sync::Arc;

    /// Open a `Store` on a fresh temp db with the Node base schema seeded.
    pub fn test_store() -> (tempfile::TempDir, Arc<Store>) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("data.db");
        {
            let conn = augmentagent_store::rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE actions (
                    id TEXT PRIMARY KEY, messageId TEXT NOT NULL, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    originalBody TEXT, draftBody TEXT,
                    status TEXT NOT NULL DEFAULT 'pending', errorMessage TEXT,
                    createdAt INTEGER NOT NULL, updatedAt INTEGER NOT NULL
                );
                CREATE TABLE emails (
                    messageId TEXT PRIMARY KEY, threadId TEXT,
                    fromEmail TEXT NOT NULL, subject TEXT NOT NULL,
                    body TEXT, receivedAt TEXT, accountEntityId TEXT,
                    firstSeenAt INTEGER NOT NULL, triageResult TEXT,
                    agentProcessedAt INTEGER
                );
                CREATE TABLE gmail_accounts (
                    id TEXT PRIMARY KEY, connectionId TEXT NOT NULL, email TEXT,
                    label TEXT, entityId TEXT NOT NULL, active INTEGER DEFAULT 1,
                    createdAt INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
        }
        let store = Arc::new(Store::open(&path).unwrap());
        (d, store)
    }
}
