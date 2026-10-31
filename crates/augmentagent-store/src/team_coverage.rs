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
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS team_coverage_decisions (
            action_id           TEXT PRIMARY KEY,
            mode                TEXT NOT NULL,
            teammate            TEXT NOT NULL,
            covering_message_id TEXT NOT NULL,
            thread_id           TEXT NOT NULL,
            covered_at_ms       INTEGER NOT NULL,
            decided_at_ms       INTEGER NOT NULL);
         CREATE INDEX IF NOT EXISTS idx_team_coverage_decided
            ON team_coverage_decisions(decided_at_ms);
         CREATE TABLE IF NOT EXISTS team_coverage_reviews (
            action_id      TEXT PRIMARY KEY,
            verdict        TEXT NOT NULL,
            reviewed_at_ms INTEGER NOT NULL);",
    )
}

fn decision(r: &rusqlite::Row) -> rusqlite::Result<TeamCoverageDecision> {
    Ok(TeamCoverageDecision {
        action_id: r.get(0)?,
        mode: r.get(1)?,
        teammate: r.get(2)?,
        covering_message_id: r.get(3)?,
        thread_id: r.get(4)?,
        covered_at_ms: r.get(5)?,
        decided_at_ms: r.get(6)?,
    })
}

impl Store {
    /// Every stored message on `thread_id` in mailbox `account_entity_id`.
    /// A mailbox is an address, not an entity id: a legacy connector id and
    /// the direct-Google id that replaced it (`gmail_accounts`, active or
    /// not) are the same mailbox.
    pub fn thread_messages_in_mailbox(
        &self,
        thread_id: &str,
        account_entity_id: &str,
    ) -> StoreResult<Vec<ThreadMessage>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT messageId, fromEmail, COALESCE(receivedAt, ''), COALESCE(body, ''), \
                    toAddrs, ccAddrs \
               FROM emails \
              WHERE threadId = ?1 \
                AND (accountEntityId = ?2 \
                     OR accountEntityId IN ( \
                        SELECT entityId FROM gmail_accounts \
                         WHERE email <> '' AND email = ( \
                            SELECT email FROM gmail_accounts WHERE entityId = ?2)))",
        )?;
        let rows = stmt.query_map(params![thread_id, account_entity_id], |r| {
            let to: Option<String> = r.get(4)?;
            let cc: Option<String> = r.get(5)?;
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, to, cc))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (message_id, from, received_at, body, to, cc) = row?;
            // Unparseable recipient JSON is unknown, never empty.
            let parse = |raw: Option<String>| {
                raw.and_then(|r| serde_json::from_str::<Vec<String>>(&r).ok())
            };
            let recipients = match (parse(to), parse(cc)) {
                (Some(to), Some(cc)) => Some(Recipients { to, cc }),
                _ => None,
            };
            out.push(ThreadMessage { message_id, from, received_at, body, recipients });
        }
        Ok(out)
    }

    /// Record a decision once per action; a later tick never overwrites the
    /// first decision. Returns whether a row was inserted.
    pub fn record_team_coverage_decision(&self, d: &TeamCoverageDecision) -> StoreResult<bool> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n = guard.execute(
            "INSERT OR IGNORE INTO team_coverage_decisions \
                 (action_id, mode, teammate, covering_message_id, thread_id, \
                  covered_at_ms, decided_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                d.action_id,
                d.mode,
                d.teammate,
                d.covering_message_id,
                d.thread_id,
                d.covered_at_ms,
                d.decided_at_ms
            ],
        )?;
        Ok(n > 0)
    }

    /// Decisions made at or after `since_ms`, oldest first.
    pub fn team_coverage_decisions(&self, since_ms: i64) -> StoreResult<Vec<TeamCoverageDecision>> {
        let guard = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = guard.prepare(
            "SELECT action_id, mode, teammate, covering_message_id, thread_id, \
                    covered_at_ms, decided_at_ms \
               FROM team_coverage_decisions WHERE decided_at_ms >= ?1 \
              ORDER BY decided_at_ms, action_id",
        )?;
        let rows = stmt.query_map(params![since_ms], decision)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// #1702 — one row of the shadow report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamCoverageReportRow {
    pub decision: TeamCoverageDecision,
    /// The card's platform (`emails.platform`; empty if the inbound is gone).
    pub platform: String,
    /// The owner sent on the thread after the teammate's covering message —
    /// the objective "the owner still needed to answer" false-positive signal.
    pub owner_replied_anyway: bool,
    /// `correct` / `wrong`, or `None` until the owner reviews it.
    pub review: Option<String>,
}

pub const REVIEW_VERDICTS: [&str; 2] = ["correct", "wrong"];

impl Store {
    /// Decisions made at or after `since_ms`, with platform, the
    /// replied-anyway signal and the owner's review.
    pub fn team_coverage_report(&self, since_ms: i64) -> StoreResult<Vec<TeamCoverageReportRow>> {
        let rows: Vec<(TeamCoverageDecision, String, Option<String>)> = {
            let guard = self.conn.lock().expect("store mutex poisoned");
            let mut stmt = guard.prepare(
                "SELECT d.action_id, d.mode, d.teammate, d.covering_message_id, d.thread_id, \
                        d.covered_at_ms, d.decided_at_ms, COALESCE(e.platform, ''), r.verdict \
                   FROM team_coverage_decisions d \
                   LEFT JOIN actions a ON a.id = d.action_id \
                   LEFT JOIN emails e ON e.messageId = a.messageId \
                   LEFT JOIN team_coverage_reviews r ON r.action_id = d.action_id \
                  WHERE d.decided_at_ms >= ?1 \
                  ORDER BY d.decided_at_ms, d.action_id",
            )?;
            let rows = stmt.query_map(params![since_ms], |r| Ok((decision(r)?, r.get(7)?, r.get(8)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        // The store mutex is released: thread_has_owner_send_after locks it.
        rows.into_iter()
            .map(|(decision, platform, review)| {
                let owner_replied_anyway =
                    self.thread_has_owner_send_after(&decision.thread_id, decision.covered_at_ms)?;
                Ok(TeamCoverageReportRow { decision, platform, owner_replied_anyway, review })
            })
            .collect()
    }

    /// Record the owner's verdict on a decision. `Ok(false)` when there is no
    /// such decision; an unknown verdict is an error. Re-reviewing replaces
    /// the earlier verdict.
    pub fn review_team_coverage(&self, action_id: &str, verdict: &str) -> StoreResult<bool> {
        if !REVIEW_VERDICTS.contains(&verdict) {
            return Err(crate::StoreError::InvalidInput(format!(
                "verdict must be one of {REVIEW_VERDICTS:?}, got {verdict:?}"
            )));
        }
        let guard = self.conn.lock().expect("store mutex poisoned");
        let n = guard.execute(
            "INSERT INTO team_coverage_reviews (action_id, verdict, reviewed_at_ms) \
             SELECT action_id, ?2, ?3 FROM team_coverage_decisions WHERE action_id = ?1 \
             ON CONFLICT(action_id) DO UPDATE SET verdict = excluded.verdict, \
                                                 reviewed_at_ms = excluded.reviewed_at_ms",
            params![action_id, verdict, chrono::Utc::now().timestamp_millis()],
        )?;
        Ok(n > 0)
    }
}
