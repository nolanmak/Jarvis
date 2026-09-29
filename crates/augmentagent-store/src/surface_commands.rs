//! #1292 — store operations behind owner commands on the interactive
//! surfaces (Slack first):
//!
//! * [`Store::reset_surface_conversation`] — `reset` / `new`: forget the
//!   conversation's native session so the next turn starts a fresh one. A
//!   turn a provider failure left unfinished is resolved as interrupted: it
//!   stays consumed (never re-run) and no longer blocks the conversation.
//!   This is the way out of a top-level DM left "uncertain", which has no
//!   new thread to move to.
//! * [`Store::drop_queued_inbound_events`] — `cancel all`: the messages
//!   still waiting in a conversation are settled without a turn.
//! * [`Store::pause_user_loop`] / [`Store::resume_user_loop`] — owner-scoped
//!   loop controls next to the existing `stop_user_loop`.
//!
//! Discord text channels keep their original tables (the compatibility API
//! in `store.rs`); resetting one through this surface API is refused.

use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::store::{now_millis, NativeConversation, Store, StoreError, StoreResult};
use crate::surface::SurfaceConversationRef;

/// What [`Store::reset_surface_conversation`] changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceConversationReset {
    /// The binding that was removed, if the conversation had one.
    pub previous: Option<NativeConversation>,
    /// Unfinished turns resolved as interrupted.
    pub resolved_turns: usize,
}

fn key(chat: &SurfaceConversationRef) -> (&str, &str, &str, &str) {
    (
        chat.account().platform().as_str(),
        chat.account().account_id(),
        chat.conversation_id(),
        chat.thread_id().unwrap_or(""),
    )
}

fn write_tx<T>(
    store: &Store,
    f: impl FnOnce(&Transaction<'_>) -> StoreResult<T>,
) -> StoreResult<T> {
    store.with_conn(|conn| {
        Ok((|| {
            let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })())
    })?
}

impl Store {
    /// Start a new native session for `chat`: remove its binding and resolve
    /// every unfinished, unresolved turn as interrupted, in one transaction.
    /// A no-op (with `previous: None`) for a conversation with no session.
    pub fn reset_surface_conversation(
        &self,
        chat: &SurfaceConversationRef,
    ) -> StoreResult<SurfaceConversationReset> {
        if chat.account().platform().as_str() == "discord" {
            return Err(StoreError::InvalidInput(
                "Discord conversations are not reset through the surface API".into(),
            ));
        }
        let (platform, account, conversation, thread) = key(chat);
        let now = now_millis();
        write_tx(self, |tx| {
            let previous = tx
                .query_row(
                    "SELECT provider, native_session_id, cwd, uncertain FROM surface_conversations
                     WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3
                       AND thread_id = ?4",
                    params![platform, account, conversation, thread],
                    |row| {
                        Ok(NativeConversation {
                            conversation: chat.clone(),
                            provider: row.get(0)?,
                            native_session_id: row.get(1)?,
                            cwd: row.get(2)?,
                            uncertain: row.get::<_, i64>(3)? != 0,
                        })
                    },
                )
                .optional()?;
            let unfinished: Vec<String> = {
                let mut stmt = tx.prepare(
                    "SELECT t.turn_id FROM surface_native_turns t
                     WHERE t.platform = ?1 AND t.account_id = ?2 AND t.conversation_id = ?3
                       AND t.thread_id = ?4 AND t.status != 'complete'
                       AND NOT EXISTS (SELECT 1 FROM surface_turn_resolutions r
                           WHERE r.platform = t.platform AND r.account_id = t.account_id
                             AND r.conversation_id = t.conversation_id
                             AND r.thread_id = t.thread_id AND r.turn_id = t.turn_id)",
                )?;
                let rows = stmt
                    .query_map(params![platform, account, conversation, thread], |r| {
                        r.get(0)
                    })?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for turn in &unfinished {
                tx.execute(
                    "UPDATE surface_native_turns SET status = 'uncertain', finished_at_ms = ?6
                     WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3
                       AND thread_id = ?4 AND turn_id = ?5 AND status = 'pending'",
                    params![platform, account, conversation, thread, turn, now],
                )?;
                tx.execute(
                    "INSERT INTO surface_turn_resolutions
                     (platform, account_id, conversation_id, thread_id, turn_id, resolution,
                      resolved_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'interrupted', ?6)
                     ON CONFLICT(platform, account_id, conversation_id, thread_id, turn_id)
                     DO NOTHING",
                    params![platform, account, conversation, thread, turn, now],
                )?;
            }
            tx.execute(
                "DELETE FROM surface_conversations
                 WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3
                   AND thread_id = ?4",
                params![platform, account, conversation, thread],
            )?;
            Ok(SurfaceConversationReset {
                previous,
                resolved_turns: unfinished.len(),
            })
        })
    }

    /// Inbound events still waiting (not yet claimed) in `conversation`.
    pub fn queued_inbound_events(&self, conversation: &SurfaceConversationRef) -> StoreResult<i64> {
        let (platform, account, conversation, thread) = key(conversation);
        self.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM surface_inbound_events
                 WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3
                   AND thread_id = ?4 AND status = 'received'",
                params![platform, account, conversation, thread],
                |r| r.get(0),
            )
        })
    }

    /// Settle every event still waiting in `conversation` without running
    /// it (`reason` is kept as its last error). A claimed (running) event is
    /// untouched. Returns how many were dropped.
    pub fn drop_queued_inbound_events(
        &self,
        conversation: &SurfaceConversationRef,
        reason: &str,
        now_ms: i64,
    ) -> StoreResult<usize> {
        let (platform, account, conversation, thread) = key(conversation);
        write_tx(self, |tx| {
            Ok(tx.execute(
                "UPDATE surface_inbound_events
                 SET status = 'handled', handled_at_ms = ?5, last_error = ?6
                 WHERE platform = ?1 AND account_id = ?2 AND conversation_id = ?3
                   AND thread_id = ?4 AND status = 'received'",
                params![platform, account, conversation, thread, now_ms, reason],
            )?)
        })
    }

    /// Pause an active loop the owner owns. Returns true if a row changed.
    pub fn pause_user_loop(&self, owner: &str, id: &str) -> StoreResult<bool> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE user_loops SET status = 'paused', updated_at_ms = ?3
                 WHERE id = ?1 AND owner = ?2 AND status = 'active'",
                params![id, owner, now_millis()],
            )
        })
        .map(|n| n == 1)
    }

    /// Resume a paused loop the owner owns (manually or after repeated
    /// failures), with its failure count cleared. Returns true if a row
    /// changed.
    pub fn resume_user_loop(&self, owner: &str, id: &str) -> StoreResult<bool> {
        self.with_conn(|conn| {
            conn.execute(
                "UPDATE user_loops SET status = 'active', fail_count = 0, updated_at_ms = ?3
                 WHERE id = ?1 AND owner = ?2 AND status = 'paused'",
                params![id, owner, now_millis()],
            )
        })
        .map(|n| n == 1)
    }
}
