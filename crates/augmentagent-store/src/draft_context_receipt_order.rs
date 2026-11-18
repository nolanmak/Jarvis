//! Durable append order for receipts; wall clocks and UUIDs are not chronology.
use crate::rusqlite::{Connection, Transaction, TransactionBehavior};

pub(super) fn migrate(conn: &Connection) -> crate::rusqlite::Result<()> {
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    if !has_order(&tx)? {
        // Legacy rowids retain the available insertion history. Copy it once
        // into an explicit column so VACUUM/reopen cannot change chronology.
        tx.execute_batch(
            "ALTER TABLE draft_context_receipts ADD COLUMN receipt_order INTEGER;
             UPDATE draft_context_receipts SET receipt_order=rowid;",
        )?;
    }
    tx.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS draft_context_receipts_order
           ON draft_context_receipts(receipt_order);
         CREATE TRIGGER IF NOT EXISTS draft_context_receipts_append_order
         AFTER INSERT ON draft_context_receipts WHEN NEW.receipt_order IS NULL
         BEGIN
           UPDATE draft_context_receipts
           SET receipt_order=(SELECT COALESCE(MAX(receipt_order),0)+1 FROM draft_context_receipts)
           WHERE rowid=NEW.rowid;
         END;",
    )?;
    // The insert trigger runs under SQLite's writer transaction. It also
    // covers inserts from older binaries that know only the original columns.
    tx.commit()
}

/// Read-only diagnostics may open a pre-migration database; never migrate it.
pub(super) fn has_order(conn: &Connection) -> crate::rusqlite::Result<bool> {
    let mut columns = conn.prepare("PRAGMA table_info(draft_context_receipts)")?;
    let names = columns
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|name| name == "receipt_order"))
}
