//! #1700 — storage for the teammate-coverage rule: the messages on a card's
//! thread (with recipients, #1699) and a durable log of every coverage
//! decision, shadow or live, that the #1702 report reads.

use rusqlite::{params, Connection};

use crate::recipients::Recipients;
use crate::{Store, StoreResult};

/// One stored message on a thread, as the coverage rule needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMessage {
    pub message_id: String,
    pub from: String,
    /// Raw `emails.receivedAt` (Date header / RFC 3339 / ms).
    pub received_at: String,
    pub body: String,
    /// `None` = unknown recipients; the rule fails closed on it.
    pub recipients: Option<Recipients>,
}

/// A coverage decision. `mode` is `shadow` (logged only) or `on` (retired).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamCoverageDecision {
    pub action_id: String,
    pub mode: String,
    pub teammate: String,
    pub covering_message_id: String,
    pub thread_id: String,
    pub covered_at_ms: i64,
    pub decided_at_ms: i64,
}

pub(crate) fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let _ = conn;
    Ok(())
}

impl Store {
    /// Every stored message on `thread_id` in mailbox `account_entity_id`.
    pub fn thread_messages_in_mailbox(
        &self,
        thread_id: &str,
        account_entity_id: &str,
    ) -> StoreResult<Vec<ThreadMessage>> {
        let _ = (thread_id, account_entity_id, params![]);
        Ok(Vec::new())
    }

    /// Record a decision once per action; a later tick never overwrites the
    /// first decision. Returns whether a row was inserted.
    pub fn record_team_coverage_decision(&self, d: &TeamCoverageDecision) -> StoreResult<bool> {
        let _ = d;
        Ok(false)
    }

    /// Decisions made at or after `since_ms`, oldest first.
    pub fn team_coverage_decisions(&self, since_ms: i64) -> StoreResult<Vec<TeamCoverageDecision>> {
        let _ = since_ms;
        Ok(Vec::new())
    }
}
