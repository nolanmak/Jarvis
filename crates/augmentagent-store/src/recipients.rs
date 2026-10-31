//! #1699 — first-class To/Cc recipients on `emails`.
//!
//! `toAddrs` / `ccAddrs` hold JSON arrays of normalized bare addresses
//! (lowercased, deduplicated, header order kept). `NULL` means *unknown* —
//! a non-email platform, a Gmail row with no recipient headers, or a row
//! persisted before recipient context was saved (#472) — and is never
//! confused with a known-empty list. Consumers that decide something from
//! recipients (#1700) must fail closed on `NULL`.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{Store, StoreError, StoreResult};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipients {
    pub to: Vec<String>,
    pub cc: Vec<String>,
}

/// Bare, lowercased addresses from a raw `To:`/`Cc:` header.
pub fn parse_addresses(raw: &str) -> Vec<String> {
    // Split on commas outside quotes and angle brackets, so
    // `"Doe, Jane" <jane@x>` stays one mailbox.
    let mut parts = Vec::new();
    let (mut start, mut quoted, mut angle) = (0, false, false);
    for (i, ch) in raw.char_indices() {
        match ch {
            '"' if !angle => quoted = !quoted,
            '<' if !quoted => angle = true,
            '>' if !quoted => angle = false,
            ',' if !quoted && !angle => {
                parts.push(&raw[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&raw[start..]);

    let mut out: Vec<String> = Vec::new();
    for part in parts {
        let part = part.trim();
        let addr = match (part.rfind('<'), part.rfind('>')) {
            (Some(open), Some(close)) if open < close => &part[open + 1..close],
            _ => part,
        };
        let addr = addr.trim().trim_start_matches("mailto:").to_lowercase();
        let valid = addr
            .split_once('@')
            .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
            && !addr.contains(char::is_whitespace);
        if valid && !out.contains(&addr) {
            out.push(addr);
        }
    }
    out
}

/// Column values for a row, or `None`s when the recipients are unknown.
pub(crate) fn columns(platform: &str, to: &str, cc: &str) -> (Option<String>, Option<String>) {
    if platform != "gmail" || (to.trim().is_empty() && cc.trim().is_empty()) {
        return (None, None);
    }
    let json = |raw: &str| serde_json::to_string(&parse_addresses(raw)).ok();
    (json(to), json(cc))
}

/// Add the columns and backfill from `retryGuardContext`. Runs once: the
/// backfill only happens in the migration that adds the columns.
pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info('emails') WHERE name = 'toAddrs'",
        [],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "ALTER TABLE emails ADD COLUMN toAddrs TEXT; ALTER TABLE emails ADD COLUMN ccAddrs TEXT;",
    )?;
    let rows: Vec<(String, String, String)> = {
        let mut stmt = tx.prepare(
            "SELECT messageId, platform, retryGuardContext FROM emails \
              WHERE retryGuardContext IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut update =
        tx.prepare("UPDATE emails SET toAddrs = ?2, ccAddrs = ?3 WHERE messageId = ?1")?;
    for (id, platform, context) in rows {
        // Malformed context stays unknown rather than failing the migration.
        let Ok((to, cc, _)) = serde_json::from_str::<(String, String, Vec<String>)>(&context) else {
            continue;
        };
        let (to, cc) = columns(&platform, &to, &cc);
        if to.is_some() {
            update.execute(params![id, to, cc])?;
        }
    }
    drop(update);
    tx.commit()
}

impl Store {
    /// Normalized recipients of a stored message; `None` = unknown.
    pub fn email_recipients(&self, message_id: &str) -> StoreResult<Option<Recipients>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let row: Option<(Option<String>, Option<String>)> = guard
            .query_row(
                "SELECT toAddrs, ccAddrs FROM emails WHERE messageId = ?1",
                params![message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(to), Some(cc))) = row else {
            return Ok(None);
        };
        let parse = |raw: &str| {
            serde_json::from_str::<Vec<String>>(raw).map_err(|e| StoreError::InvalidInput(e.to_string()))
        };
        Ok(Some(Recipients { to: parse(&to)?, cc: parse(&cc)? }))
    }
}
