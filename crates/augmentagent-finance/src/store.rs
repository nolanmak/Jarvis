use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rusqlite::{params, Connection};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path, str::FromStr};

pub struct FinanceStore {
    pub(crate) conn: Connection,
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
        Ok(Self { conn })
    }
    pub fn add_item(&self, env: &str, id: &str, alias: &str) -> Result<()> {
        self.conn.execute("INSERT INTO finance_items(env,id,alias) VALUES(?1,?2,?3) ON CONFLICT(env,id) DO UPDATE SET alias=excluded.alias",params![env,id,alias])?;
        Ok(())
    }
    pub fn cursor(&self, env: &str, id: &str) -> Result<String> {
        Ok(self.conn.query_row(
            "SELECT cursor FROM finance_items WHERE env=?1 AND id=?2",
            params![env, id],
            |r| r.get(0),
        )?)
    }
    pub fn status(&self, env: &str) -> Result<Vec<Value>> {
        let mut q=self.conn.prepare("SELECT id,alias,status,synced_at,history_status FROM finance_items WHERE env=?1 ORDER BY alias,id")?;
        let result=q.query_map([env],|r|Ok(json!({"item_id":r.get::<_,String>(0)?,"alias":r.get::<_,String>(1)?,"status":r.get::<_,String>(2)?,"synced_at":r.get::<_,Option<String>>(3)?,"history_status":r.get::<_,String>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
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
        tx.execute("UPDATE finance_items SET cursor=?3,status='ok',synced_at=?4,history_status=?5 WHERE env=?1 AND id=?2",params![env,item,field(last,"next_cursor")?,chrono::Utc::now().to_rfc3339(),last["transactions_update_status"].as_str().unwrap_or("UNKNOWN")])?;
        tx.commit()?;
        Ok(())
    }
}
