use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use rusqlite::{params, Connection};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, str::FromStr};

// A pending Hosted Link session is polled tightly while the owner is likely
// still in the flow, then slowly until the Link token expires.
const POLL_FAST_SECS: i64 = 15;
const POLL_FAST_WINDOW_SECS: i64 = 30 * 60;
const POLL_SLOW_SECS: i64 = 120;
// Plaid reports history readiness asynchronously, so a new Item's first sync
// is re-run on this schedule until it completes, then never again.
const HISTORY_RETRY_MINUTES: [i64; 3] = [2, 10, 30];

pub struct FinanceStore {
    pub(crate) conn: Connection,
}
/// Hosted Link session lifecycle, durable so the CLI and the daemon worker
/// agree on what is still worth polling.
#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub alias: String,
    pub update_item: Option<String>,
    pub complete: bool,
    pub state: String,
    pub created_at: Option<String>,
    pub expires_at: Option<String>,
    pub request_id: Option<String>,
    pub last_polled_at: Option<String>,
    pub notified_state: Option<String>,
}
const SESSION_COLUMNS: &str =
    "id,alias,update_item,complete,state,created_at,expires_at,request_id,last_polled_at,notified_state";
fn session(row: &rusqlite::Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        alias: row.get(1)?,
        update_item: row.get(2)?,
        complete: row.get(3)?,
        state: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        request_id: row.get(7)?,
        last_polled_at: row.get(8)?,
        notified_state: row.get(9)?,
    })
}
/// Fixed-width UTC text so SQLite's string comparison orders timestamps.
pub(crate) fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
pub(crate) fn instant(value: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid finance timestamp {value}"))?
        .into())
}
fn due(s: &Session, now: DateTime<Utc>) -> Result<bool> {
    let Some(polled) = s.last_polled_at.as_deref() else {
        return Ok(true);
    };
    let fresh = match s.created_at.as_deref() {
        Some(created) => (now - instant(created)?).num_seconds() < POLL_FAST_WINDOW_SECS,
        None => false,
    };
    let interval = if fresh { POLL_FAST_SECS } else { POLL_SLOW_SECS };
    Ok((now - instant(polled)?).num_seconds() >= interval)
}
pub fn date(value: &str) -> Result<()> {
    NaiveDate::parse_from_str(value, "%Y-%m-%d").context("expected date YYYY-MM-DD")?;
    if value.len() != 10 {
        bail!("expected date YYYY-MM-DD");
    }
    Ok(())
}
pub(crate) fn field<'a>(v: &'a Value, name: &str) -> Result<&'a str> {
    v[name]
        .as_str()
        .with_context(|| format!("Plaid response missing {name}"))
}
pub(crate) fn money(v: &Value) -> Result<Decimal> {
    Decimal::from_str_exact(
        v.as_str()
            .context("amount was not parsed as an exact decimal")?,
    )
    .context("invalid decimal amount")
}
impl FinanceStore {
    pub fn open(path: &Path) -> Result<Self> {
        // New databases contain financial records; SQLite sidecars inherit this mode.
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Self::init(Connection::open(path)?)
    }
    pub fn memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }
    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch("PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS finance_items(env TEXT NOT NULL,id TEXT NOT NULL,alias TEXT NOT NULL,cursor TEXT NOT NULL DEFAULT '',status TEXT NOT NULL DEFAULT 'linked',synced_at TEXT,history_status TEXT NOT NULL DEFAULT 'NOT_READY', PRIMARY KEY(env,id));
        CREATE TABLE IF NOT EXISTS finance_accounts(env TEXT NOT NULL,item_id TEXT NOT NULL,id TEXT NOT NULL,name TEXT NOT NULL,mask TEXT, PRIMARY KEY(env,item_id,id));
        CREATE TABLE IF NOT EXISTS finance_transactions(env TEXT NOT NULL,item_id TEXT NOT NULL,id TEXT NOT NULL,account_id TEXT NOT NULL,date TEXT NOT NULL,amount TEXT NOT NULL,currency TEXT NOT NULL,pending INTEGER NOT NULL,removed INTEGER NOT NULL DEFAULT 0,payload TEXT NOT NULL, PRIMARY KEY(env,item_id,id));
        CREATE INDEX IF NOT EXISTS finance_date ON finance_transactions(env,date);
        CREATE TABLE IF NOT EXISTS finance_sessions(env TEXT NOT NULL,id TEXT NOT NULL,alias TEXT NOT NULL,update_item TEXT,complete INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(env,id));
        CREATE TABLE IF NOT EXISTS finance_statement_items(env TEXT NOT NULL,item_id TEXT NOT NULL,last_refresh TEXT, PRIMARY KEY(env,item_id));
        CREATE TABLE IF NOT EXISTS finance_statements(env TEXT NOT NULL,item_id TEXT NOT NULL,id TEXT NOT NULL,filename TEXT NOT NULL,sha256 TEXT NOT NULL,account_id TEXT NOT NULL,year INTEGER NOT NULL,month INTEGER NOT NULL, PRIMARY KEY(env,item_id,id));")?;
        // Session lifecycle and per-Item provenance, added in place. Rows from
        // before this migration keep a NULL expires_at and are never polled.
        for (table, column, definition) in [
            ("finance_sessions", "created_at", "TEXT"),
            ("finance_sessions", "expires_at", "TEXT"),
            (
                "finance_sessions",
                "state",
                "TEXT NOT NULL DEFAULT 'pending'",
            ),
            ("finance_sessions", "request_id", "TEXT"),
            ("finance_sessions", "last_polled_at", "TEXT"),
            ("finance_sessions", "notified_state", "TEXT"),
            ("finance_items", "institution_id", "TEXT"),
            ("finance_items", "duplicate_of", "TEXT"),
            (
                "finance_items",
                "history_attempt",
                "INTEGER NOT NULL DEFAULT 0",
            ),
            ("finance_items", "history_due_at", "TEXT"),
        ] {
            let present: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name=?2",
                params![table, column],
                |r| r.get(0),
            )?;
            if !present {
                conn.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )?;
            }
        }
        Ok(Self { conn })
    }
    pub fn add_item(&self, env: &str, id: &str, alias: &str) -> Result<()> {
        self.conn.execute("INSERT INTO finance_items(env,id,alias) VALUES(?1,?2,?3) ON CONFLICT(env,id) DO UPDATE SET alias=excluded.alias",params![env,id,alias])?;
        Ok(())
    }
    pub fn add_session(
        &self,
        env: &str,
        id: &str,
        alias: &str,
        update_item: Option<&str>,
        expires_at: DateTime<Utc>,
        request_id: Option<&str>,
    ) -> Result<()> {
        self.conn.execute("INSERT INTO finance_sessions(env,id,alias,update_item,created_at,expires_at,state,request_id) VALUES(?1,?2,?3,?4,?5,?6,'pending',?7)",params![env,id,alias,update_item,stamp(Utc::now()),stamp(expires_at),request_id])?;
        Ok(())
    }
    pub fn session(&self, env: &str, id: &str) -> Result<Session> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM finance_sessions WHERE env=?1 AND id=?2"),
                params![env, id],
                session,
            )
            .context("unknown finance session")
    }
    /// Pending sessions whose Link token is still valid and whose polling
    /// interval has elapsed. Sessions without a recorded expiry are excluded.
    pub fn due_sessions(&self, env: &str, now: DateTime<Utc>) -> Result<Vec<Session>> {
        let mut q = self.conn.prepare(&format!("SELECT {SESSION_COLUMNS} FROM finance_sessions WHERE env=?1 AND complete=0 AND state='pending' AND expires_at IS NOT NULL AND expires_at>?2 ORDER BY created_at,id"))?;
        let rows = q
            .query_map(params![env, stamp(now)], session)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter_map(|s| match due(&s, now) {
                Ok(true) => Some(Ok(s)),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }
    pub fn mark_polled(&self, env: &str, id: &str, now: DateTime<Utc>) -> Result<()> {
        self.conn.execute(
            "UPDATE finance_sessions SET last_polled_at=?3 WHERE env=?1 AND id=?2",
            params![env, id, stamp(now)],
        )?;
        Ok(())
    }
    pub fn finish_session(&self, env: &str, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE finance_sessions SET complete=1,state='linked' WHERE env=?1 AND id=?2",
            params![env, id],
        )?;
        Ok(())
    }
    pub fn expire_session(&self, env: &str, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE finance_sessions SET state='expired' WHERE env=?1 AND id=?2 AND complete=0",
            params![env, id],
        )?;
        Ok(())
    }
    /// Mark every pending session past its expiry; returns the ids retired.
    pub fn expire_sessions(&self, env: &str, now: DateTime<Utc>) -> Result<Vec<String>> {
        let mut q=self.conn.prepare("SELECT id FROM finance_sessions WHERE env=?1 AND complete=0 AND state='pending' AND expires_at IS NOT NULL AND expires_at<=?2")?;
        let ids = q
            .query_map(params![env, stamp(now)], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in &ids {
            self.expire_session(env, id)?;
        }
        Ok(ids)
    }
    pub fn set_institution(&self, env: &str, item: &str, institution: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE finance_items SET institution_id=?3 WHERE env=?1 AND id=?2",
            params![env, item, institution],
        )?;
        Ok(())
    }
    /// Items already linked to this institution, excluding `item` itself.
    pub fn institution_items(&self, env: &str, institution: &str, item: &str) -> Result<Vec<String>> {
        let mut q=self.conn.prepare("SELECT id FROM finance_items WHERE env=?1 AND institution_id=?2 AND id<>?3 ORDER BY id")?;
        let ids = q
            .query_map(params![env, institution, item], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }
    /// Record that an Item duplicates an existing connection. Flagged only —
    /// the owner decides, and nothing is ever unlinked automatically.
    pub fn flag_duplicate(&self, env: &str, item: &str, existing: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE finance_items SET duplicate_of=?3 WHERE env=?1 AND id=?2",
            params![env, item, existing],
        )?;
        Ok(())
    }
    /// Items flagged as duplicating an existing connection, as (item, existing).
    pub fn duplicates(&self, env: &str) -> Result<Vec<(String, String)>> {
        let mut q=self.conn.prepare("SELECT id,duplicate_of FROM finance_items WHERE env=?1 AND duplicate_of IS NOT NULL ORDER BY id")?;
        let rows = q
            .query_map([env], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
    /// Persisted history-readiness retry state: attempts scheduled so far and
    /// when the next re-sync is due.
    pub fn history_retry(&self, env: &str, item: &str) -> Result<(i64, Option<String>)> {
        Ok(self.conn.query_row(
            "SELECT history_attempt,history_due_at FROM finance_items WHERE env=?1 AND id=?2",
            params![env, item],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
    pub fn history_retries_due(&self, env: &str, now: DateTime<Utc>) -> Result<Vec<String>> {
        let mut q=self.conn.prepare("SELECT id FROM finance_items WHERE env=?1 AND history_due_at IS NOT NULL AND history_due_at<=?2 ORDER BY id")?;
        let ids = q
            .query_map(params![env, stamp(now)], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }
    pub fn cursor(&self, env: &str, id: &str) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT cursor FROM finance_items WHERE env=?1 AND id=?2",
            params![env, id],
            |r| r.get(0),
        )?)
    }
    pub fn status(&self, env: &str) -> Result<Vec<Value>> {
        let mut q=self.conn.prepare("SELECT id,alias,status,synced_at,history_status,institution_id,duplicate_of FROM finance_items WHERE env=?1 ORDER BY alias,id")?;
        let result=q.query_map([env],|r|Ok(json!({"item_id":r.get::<_,String>(0)?,"alias":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"synced_at":r.get::<_,Option<String>>(3)?,"history_status":r.get::<_,String>(4)?,"institution_id":r.get::<_,Option<String>>(5)?,"duplicate_of":r.get::<_,Option<String>>(6)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(result)
    }
    pub fn error(&self, env: &str, id: &str, code: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE finance_items SET status=?3 WHERE env=?1 AND id=?2",
            params![env, id, code],
        )?;
        Ok(())
    }
    pub fn transactions(
        &self,
        env: &str,
        account: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Vec<Value>> {
        if let Some(s) = start {
            date(s)?;
        }
        if let Some(s) = end {
            date(s)?;
        }
        if start.zip(end).is_some_and(|(s, e)| s > e) {
            bail!("start date exceeds end date");
        }
        let mut q=self.conn.prepare("SELECT payload,item_id,amount,currency FROM finance_transactions WHERE env=?1 AND removed=0 AND (?2 IS NULL OR account_id=?2) AND (?3 IS NULL OR date>=?3) AND (?4 IS NULL OR date<=?4) ORDER BY date,id")?;
        let raw = q
            .query_map(params![env, account, start, end], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        raw.into_iter()
            .map(|(s, item, amount, currency)| {
                let mut v: Value = serde_json::from_str(&s)?;
                v["item_id"] = json!(item);
                v["amount"] = json!(amount);
                v["currency"] = json!(currency);
                Ok(v)
            })
            .collect()
    }
    pub fn summary(
        &self,
        env: &str,
        account: Option<&str>,
        start: Option<&str>,
        end: Option<&str>,
    ) -> Result<Vec<Value>> {
        let rows = self.transactions(env, account, start, end)?;
        let mut sums: BTreeMap<String, (Decimal, Decimal, usize, usize)> = BTreeMap::new();
        for row in rows {
            let sum = sums.entry(field(&row, "currency")?.to_owned()).or_default();
            if row["pending"].as_bool() == Some(true) {
                sum.3 += 1;
                continue;
            }
            let primary = row
                .pointer("/personal_finance_category/primary")
                .and_then(Value::as_str)
                .unwrap_or("");
            if matches!(primary, "TRANSFER_IN" | "TRANSFER_OUT" | "LOAN_PAYMENTS") {
                sum.2 += 1;
                continue;
            }
            let value = Decimal::from_str(field(&row, "amount")?)?;
            if value >= Decimal::ZERO {
                sum.0 = sum.0.checked_add(value).context("amount overflow")?;
            } else {
                sum.1 = sum.1.checked_add(-value).context("amount overflow")?;
            }
        }
        sums.into_iter().map(|(currency,(outflow,inflow,excluded,pending))|Ok(json!({"currency":currency,"outflow":outflow.normalize().to_string(),"inflow":inflow.normalize().to_string(),"net_outflow":outflow.checked_sub(inflow).context("amount overflow")?.normalize().to_string(),"excluded_transfer_or_loan_payment_count":excluded,"pending_count":pending}))).collect()
    }
    pub(crate) fn apply(&mut self, env: &str, item: &str, pages: &[Value]) -> Result<()> {
        let tx = self.conn.transaction()?;
        for page in pages {
            for a in page["accounts"].as_array().context("missing accounts")? {
                tx.execute("INSERT INTO finance_accounts VALUES(?1,?2,?3,?4,?5) ON CONFLICT(env,item_id,id) DO UPDATE SET name=excluded.name,mask=excluded.mask",params![env,item,field(a,"account_id")?,field(a,"name")?,a["mask"].as_str()])?;
            }
            for key in ["added", "modified"] {
                for t in page[key]
                    .as_array()
                    .with_context(|| format!("missing {key}"))?
                {
                    let id = field(t, "transaction_id")?;
                    let account = field(t, "account_id")?;
                    let d = field(t, "date")?;
                    date(d)?;
                    let amount = money(&t["amount"])?;
                    let currency = t["iso_currency_code"]
                        .as_str()
                        .or(t["unofficial_currency_code"].as_str())
                        .context("missing currency")?;
                    let pending = t["pending"].as_bool().context("missing pending flag")?;
                    if !pending {
                        if let Some(old) = t["pending_transaction_id"].as_str() {
                            tx.execute("UPDATE finance_transactions SET removed=1 WHERE env=?1 AND item_id=?2 AND id=?3",params![env,item,old])?;
                        }
                    }
                    tx.execute("INSERT INTO finance_transactions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0,?9) ON CONFLICT(env,item_id,id) DO UPDATE SET account_id=excluded.account_id,date=excluded.date,amount=excluded.amount,currency=excluded.currency,pending=excluded.pending,removed=0,payload=excluded.payload",params![env,item,id,account,d,amount.normalize().to_string(),currency,pending,t.to_string()])?;
                }
            }
            for t in page["removed"].as_array().context("missing removed")? {
                tx.execute("UPDATE finance_transactions SET removed=1 WHERE env=?1 AND item_id=?2 AND id=?3",params![env,item,field(t,"transaction_id")?])?;
            }
        }
        // Page order is not a lifecycle guarantee: a posted row can precede
        // its pending predecessor in a later page or replay. Reconcile after
        // the entire batch, in the same transaction as the cursor.
        tx.execute("UPDATE finance_transactions SET removed=1 WHERE env=?1 AND item_id=?2 AND pending=1 AND id IN (SELECT json_extract(payload,'$.pending_transaction_id') FROM finance_transactions WHERE env=?1 AND item_id=?2 AND pending=0)",params![env,item])?;
        let last = pages.last().context("empty sync")?;
        let history = last["transactions_update_status"]
            .as_str()
            .unwrap_or("UNKNOWN");
        // History readiness is asynchronous: keep the retry schedule in SQLite
        // so a restart resumes it instead of losing or repeating a slot.
        let attempt: i64 = tx.query_row(
            "SELECT history_attempt FROM finance_items WHERE env=?1 AND id=?2",
            params![env, item],
            |r| r.get(0),
        )?;
        let (attempt, due_at) = if history == "HISTORICAL_UPDATE_COMPLETE" {
            (0, None)
        } else {
            match HISTORY_RETRY_MINUTES.get(usize::try_from(attempt).unwrap_or(usize::MAX)) {
                Some(minutes) => (
                    attempt + 1,
                    Some(stamp(Utc::now() + Duration::minutes(*minutes))),
                ),
                None => (attempt, None),
            }
        };
        tx.execute("UPDATE finance_items SET cursor=?3,status='ok',synced_at=?4,history_status=?5,history_attempt=?6,history_due_at=?7 WHERE env=?1 AND id=?2",params![env,item,field(last,"next_cursor")?,chrono::Utc::now().to_rfc3339(),history,attempt,due_at])?;
        tx.commit()?;
        Ok(())
    }
}
