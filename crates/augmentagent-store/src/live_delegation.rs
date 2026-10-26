//! Durable bindings for client-delegated Live work.
//!
//! A delegation event identifies work but deliberately does not carry its
//! instructions. The adapter may create a row only after it has selected a
//! bounded, transcript-derived task for the authenticated origin.
use crate::{Store, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension};

pub(crate) fn migrate(conn: &Connection) -> StoreResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS live_delegations (
            session_id TEXT NOT NULL,
            delegation_id TEXT NOT NULL,
            origin_account TEXT NOT NULL,
            origin_owner TEXT NOT NULL,
            origin_conversation TEXT NOT NULL,
            transcript_until_ms INTEGER NOT NULL,
            task TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'queued'
                CHECK(state IN ('queued','running','ready','done','uncertain','cancelled')),
            result TEXT,
            PRIMARY KEY(session_id, delegation_id)
        );
        CREATE INDEX IF NOT EXISTS idx_live_delegations_pending
        ON live_delegations(state, session_id, delegation_id);",
    )?;
    Ok(())
}

/// Metadata authenticated by the Live client before the acknowledgement is
/// emitted. `task` is transcript-derived context, never a delegation ID.
#[derive(Clone, Copy)]
pub struct NewLiveDelegation<'a> {
    pub session_id: &'a str,
    pub delegation_id: &'a str,
    pub origin_account: &'a str,
    pub origin_owner: &'a str,
    pub origin_conversation: &'a str,
    pub transcript_until_ms: i64,
    pub task: &'a str,
}

/// Private durable work record. It intentionally does not implement Debug or
/// Serialize because task context and output belong only to the origin chat.
pub struct LiveDelegation {
    pub session_id: String,
    pub delegation_id: String,
    pub origin_account: String,
    pub origin_owner: String,
    pub origin_conversation: String,
    pub transcript_until_ms: i64,
    pub task: String,
    pub state: String,
    pub result: Option<String>,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

impl Store {
    /// Bind a delegation before acknowledging it. Replayed metadata is safe;
    /// conflicting metadata is rejected rather than redirected to another chat.
    pub fn live_enqueue_delegation(&self, item: NewLiveDelegation<'_>) -> StoreResult<bool> {
        if !valid_id(item.session_id)
            || !valid_id(item.delegation_id)
            || !valid_id(item.origin_account)
            || !valid_id(item.origin_owner)
            || !valid_id(item.origin_conversation)
            || item.transcript_until_ms < 0
            || item.task.trim().is_empty()
            || item.task.len() > 12_000
        {
            return Err(StoreError::InvalidInput("invalid live delegation".into()));
        }
        self.with_conn(|conn| {
            let existing: Option<(String, String, String, String, i64)> = conn
                .query_row(
                    "SELECT origin_account,origin_owner,origin_conversation,task,transcript_until_ms
                     FROM live_delegations WHERE session_id=?1 AND delegation_id=?2",
                    params![item.session_id, item.delegation_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?;
            if let Some((account, owner, conversation, task, until)) = existing {
                return Ok(account == item.origin_account
                    && owner == item.origin_owner
                    && conversation == item.origin_conversation
                    && task == item.task
                    && until == item.transcript_until_ms);
            }
            conn.execute(
                "INSERT INTO live_delegations(session_id,delegation_id,origin_account,origin_owner,origin_conversation,transcript_until_ms,task)
                 VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![item.session_id,item.delegation_id,item.origin_account,item.origin_owner,item.origin_conversation,item.transcript_until_ms,item.task],
            )?;
            Ok(true)
        })
    }

    pub fn live_delegation(
        &self,
        session_id: &str,
        delegation_id: &str,
    ) -> StoreResult<Option<LiveDelegation>> {
        self.with_conn(|conn| conn.query_row(
            "SELECT session_id,delegation_id,origin_account,origin_owner,origin_conversation,transcript_until_ms,task,state,result
             FROM live_delegations WHERE session_id=?1 AND delegation_id=?2",
            params![session_id, delegation_id],
            |r| Ok(LiveDelegation {
                session_id: r.get(0)?,
                delegation_id: r.get(1)?,
                origin_account: r.get(2)?,
                origin_owner: r.get(3)?,
                origin_conversation: r.get(4)?,
                transcript_until_ms: r.get(5)?,
                task: r.get(6)?,
                state: r.get(7)?,
                result: r.get(8)?,
            }),
        ).optional().map_err(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_delegation_cannot_replace_the_bound_origin_or_task() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let item = NewLiveDelegation {
            session_id: "session-1", delegation_id: "delegation-1",
            origin_account: "account", origin_owner: "owner",
            origin_conversation: "conversation", transcript_until_ms: 100,
            task: "Summarize the selected transcript.",
        };
        assert!(store.live_enqueue_delegation(item).unwrap());
        assert!(store.live_enqueue_delegation(item).unwrap());
        let conflicting = NewLiveDelegation { task: "Do something else.", ..item };
        assert!(!store.live_enqueue_delegation(conflicting).unwrap());
        assert_eq!(store.live_delegation("session-1", "delegation-1").unwrap().unwrap().task,
            "Summarize the selected transcript.");
    }

    #[test]
    fn missing_transcript_task_creates_no_delegation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).unwrap();
        let item = NewLiveDelegation {
            session_id: "session-1", delegation_id: "delegation-1",
            origin_account: "account", origin_owner: "owner",
            origin_conversation: "conversation", transcript_until_ms: 100, task: " ",
        };
        assert!(store.live_enqueue_delegation(item).is_err());
        assert!(store.live_delegation("session-1", "delegation-1").unwrap().is_none());
    }
}
