//! Immutable receipts for completed, individually approved document signatures.
use crate::{Store, StoreResult};
use rusqlite::{params, Connection, OptionalExtension};
pub(crate) fn migrate(c: &Connection) -> StoreResult<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS document_signatures (
        action_id TEXT PRIMARY KEY, receipt TEXT NOT NULL,
        created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));
        CREATE TRIGGER IF NOT EXISTS document_signatures_no_update BEFORE UPDATE ON document_signatures BEGIN SELECT RAISE(ABORT,'signature receipts are immutable'); END;
        CREATE TRIGGER IF NOT EXISTS document_signatures_no_delete BEFORE DELETE ON document_signatures BEGIN SELECT RAISE(ABORT,'signature receipts are immutable'); END;")?;
    Ok(())
}
impl Store {
    pub fn record_document_signature(&self, action_id: &str, receipt: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO document_signatures(action_id,receipt) VALUES (?1,?2)",
                params![action_id, receipt],
            )?;
            Ok(())
        })
    }
    pub fn document_signature(&self, action_id: &str) -> StoreResult<Option<String>> {
        self.with_conn(|c| {
            c.query_row(
                "SELECT receipt FROM document_signatures WHERE action_id=?1",
                [action_id],
                |r| r.get(0),
            )
            .optional()
        })
    }
}
