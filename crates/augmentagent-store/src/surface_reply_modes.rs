//! #1297 — how answers are delivered in one interactive-surface
//! conversation (`text` or `spoken`), set by the owner's `voice on|off`
//! command and read for every answer, so the choice survives a restart.
//!
//! Keyed by the transport-neutral conversation storage key
//! ([`SurfaceConversationRef::storage_key`]); the mode is an opaque,
//! surface-defined word so a new mode needs no schema change. No row means
//! the surface default (text).

use rusqlite::{params, Connection, OptionalExtension};

use crate::store::{Store, StoreError, StoreResult};
use crate::surface::SurfaceConversationRef;

/// Additive, idempotent schema. Called from `Store::migrate`.
pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS surface_reply_modes (
            conversation_key TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(length(mode) > 0),
            updated_at_ms INTEGER NOT NULL
        );"#,
    )?;
    Ok(())
}

impl Store {
    /// Set (`Some`) or clear (`None`) the reply mode of `conversation`.
    pub fn set_surface_reply_mode(
        &self,
        conversation: &SurfaceConversationRef,
        mode: Option<&str>,
        now_ms: i64,
    ) -> StoreResult<()> {
        let key = conversation.storage_key();
        match mode.map(str::trim) {
            Some("") => Err(StoreError::InvalidInput("reply mode is empty".into())),
            Some(mode) => self.with_conn(|conn| {
                conn.execute(
                    "INSERT INTO surface_reply_modes (conversation_key, mode, updated_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(conversation_key) DO UPDATE SET
                       mode = excluded.mode, updated_at_ms = excluded.updated_at_ms",
                    params![key, mode, now_ms],
                )
                .map(|_| ())
            }),
            None => self.with_conn(|conn| {
                conn.execute(
                    "DELETE FROM surface_reply_modes WHERE conversation_key = ?1",
                    params![key],
                )
                .map(|_| ())
            }),
        }
    }

    /// The reply mode stored for exactly `conversation` (no inheritance).
    pub fn surface_reply_mode(
        &self,
        conversation: &SurfaceConversationRef,
    ) -> StoreResult<Option<String>> {
        let key = conversation.storage_key();
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT mode FROM surface_reply_modes WHERE conversation_key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()
        })
    }
}
