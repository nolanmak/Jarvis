//! #1699 — first-class To/Cc recipients on `emails`.
//!
//! `toAddrs` / `ccAddrs` hold JSON arrays of normalized bare addresses
//! (lowercased, deduplicated, header order kept). `NULL` means *unknown* —
//! a non-email platform, a Gmail row with no recipient headers, or a row
//! persisted before recipient context was saved (#472) — and is never
//! confused with a known-empty list. Consumers that decide something from
//! recipients (#1700) must fail closed on `NULL`.

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::{Store, StoreResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
}

/// Bare, lowercased addresses from a raw `To:`/`Cc:` header.
pub fn parse_addresses(raw: &str) -> Vec<String> {
    let _ = raw;
    Vec::new()
}

/// Column values for a row, or `None`s when the recipients are unknown.
pub(crate) fn columns(platform: &str, to: &str, cc: &str) -> (Option<String>, Option<String>) {
    let _ = (platform, to, cc);
    (None, None)
}

/// Add the columns and backfill from `retryGuardContext`. Runs once: the
/// backfill only happens in the migration that adds the columns.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let _ = conn;
    Ok(())
}

impl Store {
    /// Normalized recipients of a stored message; `None` = unknown.
    pub fn email_recipients(&self, message_id: &str) -> StoreResult<Option<Recipients>> {
        let _ = message_id;
        Ok(None)
    }
}
