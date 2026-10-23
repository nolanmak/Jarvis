//! Approval audit and durable execution fence. UI delivery never owns this state.
use crate::{Store, StoreResult};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub(crate) fn migrate(c: &Connection) -> StoreResult<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS approval_history (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        action_id TEXT NOT NULL, interaction_id TEXT NOT NULL,
        surface TEXT NOT NULL, actor TEXT NOT NULL, conversation TEXT NOT NULL,
        verb TEXT NOT NULL, revision TEXT NOT NULL, kind TEXT NOT NULL,
        summary TEXT NOT NULL, outcome TEXT NOT NULL DEFAULT 'in_progress',
        detail TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
        finished_at TEXT, UNIQUE(surface,interaction_id,action_id,verb));
        CREATE UNIQUE INDEX IF NOT EXISTS approval_history_inflight ON approval_history(action_id) WHERE finished_at IS NULL;
        CREATE INDEX IF NOT EXISTS approval_history_action ON approval_history(action_id,seq);")?;
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionContext {
    pub surface: String,
    pub actor: String,
    pub conversation: String,
    pub interaction_id: String,
    /// Optional fingerprint of the proposal actually shown on the card.
    pub revision: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub seq: i64,
    pub action_id: String,
    pub surface: String,
    pub actor: String,
    pub conversation: String,
    pub verb: String,
    pub revision: String,
    pub kind: String,
    pub summary: String,
    pub outcome: String,
    pub detail: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub current_action_status: Option<String>,
    pub provider_receipt: Option<String>,
    pub account: Option<String>,
}
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    Ok(ApprovalRecord {
        seq: r.get(0)?,
        action_id: r.get(1)?,
        surface: r.get(2)?,
        actor: r.get(3)?,
        conversation: r.get(4)?,
        verb: r.get(5)?,
        revision: r.get(6)?,
        kind: r.get(7)?,
        summary: r.get(8)?,
        outcome: r.get(9)?,
        detail: r.get(10)?,
        created_at: r.get(11)?,
        finished_at: r.get(12)?,
        current_action_status: r.get(13)?,
        provider_receipt: r.get(14)?,
        account: r.get(15)?,
    })
}
const COLS: &str = "seq,action_id,surface,actor,conversation,verb,revision,kind,summary,outcome,detail,created_at,finished_at,
    (SELECT status FROM actions WHERE id=approval_history.action_id),
    (SELECT message_id FROM self_sent_messages WHERE action_id=approval_history.action_id ORDER BY sent_at_ms DESC LIMIT 1),
    (SELECT e.accountEntityId FROM emails e JOIN actions a ON a.messageId=e.messageId WHERE a.id=approval_history.action_id)";
pub fn bounded(s: &str, max: usize) -> String {
    crate::redact::mask(s)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(max)
        .collect()
}
impl Store {
    /// Returns None for a replay or another operation already executing this action.
    /// The committed row is both audit evidence and a durable fence, even after a crash.
    pub fn begin_approval(
        &self,
        action: &str,
        ctx: &DecisionContext,
        verb: &str,
        revision: &str,
        kind: &str,
        summary: &str,
    ) -> StoreResult<Option<i64>> {
        self.with_conn(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO approval_history
                (action_id,interaction_id,surface,actor,conversation,verb,revision,kind,summary)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    action,
                    ctx.interaction_id,
                    ctx.surface,
                    ctx.actor,
                    ctx.conversation,
                    verb,
                    revision,
                    kind,
                    bounded(summary, 400)
                ],
            )?;
            Ok((n == 1).then(|| c.last_insert_rowid()))
        })
    }
    pub fn finish_approval(&self, seq: i64, outcome: &str, detail: &str) -> StoreResult<()> {
        self.with_conn(|c| {
            let updated=c.execute("UPDATE approval_history SET outcome=?2,detail=?3,finished_at=CASE WHEN ?2='unconfirmed' THEN NULL ELSE strftime('%Y-%m-%dT%H:%M:%fZ','now') END WHERE seq=?1 AND finished_at IS NULL",
                params![seq,outcome,bounded(detail,800)])?;
            if updated!=1 {return Err(rusqlite::Error::QueryReturnedNoRows);}
            Ok(())
        })
    }
    pub fn approval_history(
        &self,
        before: Option<i64>,
        limit: usize,
        action: Option<&str>,
    ) -> StoreResult<Vec<ApprovalRecord>> {
        self.with_conn(|c| {
            let sql=format!("SELECT {COLS} FROM approval_history WHERE (?1 IS NULL OR seq<?1) AND (?2 IS NULL OR action_id=?2) ORDER BY seq DESC LIMIT ?3");
            let mut st=c.prepare(&sql)?;
            let rows=st.query_map(params![before,action,limit.min(100) as i64],row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
    pub fn approval_inflight(&self, action: &str) -> StoreResult<Option<ApprovalRecord>> {
        self.with_conn(|c|c.query_row(&format!("SELECT {COLS} FROM approval_history WHERE action_id=?1 AND finished_at IS NULL"),[action],row).optional())
    }
}
