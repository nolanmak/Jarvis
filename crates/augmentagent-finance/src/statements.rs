use crate::{atomic_write, store::field, Client, FinanceStore};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

/// Request at most one refresh per seven days. The next list/sync can pick
/// up results after Plaid finishes asynchronously; no completion is assumed.
pub async fn refresh_statements(
    client: &Client,
    store: &FinanceStore,
    env: &str,
    item: &str,
    access: &str,
) -> Result<bool> {
    let now = chrono::Utc::now().date_naive();
    let today = now.to_string();
    let last: Option<String> = store
        .conn
        .query_row(
            "SELECT last_refresh FROM finance_statement_items WHERE env=?1 AND item_id=?2",
            params![env, item],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if last
        .and_then(|s| chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").ok())
        .is_some_and(|last| (now - last).num_days() < 7)
    {
        return Ok(false);
    }
    let start = now
        .checked_sub_months(chrono::Months::new(2))
        .context("date overflow")?;
    client.call("/statements/refresh",json!({"access_token":access,"start_date":start.to_string(),"end_date":now.to_string()})).await?;
    store.conn.execute("INSERT INTO finance_statement_items VALUES(?1,?2,?3) ON CONFLICT(env,item_id) DO UPDATE SET last_refresh=excluded.last_refresh",params![env,item,today])?;
    Ok(true)
}
pub async fn sync_statements(
    client: &Client,
    store: &FinanceStore,
    env: &str,
    item: &str,
    access: &str,
    root: &Path,
) -> Result<usize> {
    if !matches!(env, "sandbox" | "production") {
        bail!("invalid environment");
    }
    let dir = root.join("finance");
    if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("refusing symlink finance directory");
    }
    fs::create_dir_all(&dir)?;
    let data = client
        .call("/statements/list", json!({"access_token":access}))
        .await?;
    let mut count = 0;
    for account in data["accounts"]
        .as_array()
        .context("missing statement accounts")?
    {
        let aid = field(account, "account_id")?;
        for statement in account["statements"]
            .as_array()
            .context("missing statements")?
        {
            let id = field(statement, "statement_id")?;
            let year = statement["year"]
                .as_i64()
                .context("missing statement year")?;
            let month = statement["month"]
                .as_i64()
                .context("missing statement month")?;
            if !(1900..=9999).contains(&year) || !(1..=12).contains(&month) {
                bail!("invalid statement period");
            }
            let hash = format!("{:x}", Sha256::digest(format!("{env}/{item}/{id}")));
            let filename = format!("statement-{env}-{hash}.pdf");
            let dest = dir.join(&filename);
            let known: Option<String> = store
                .conn
                .query_row(
                    "SELECT sha256 FROM finance_statements WHERE env=?1 AND item_id=?2 AND id=?3",
                    params![env, item, id],
                    |r| r.get(0),
                )
                .optional()?;
            if fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_symlink()) {
                bail!("refusing symlink statement target");
            }
            let intact = known.as_ref().is_some_and(|digest| {
                fs::read(&dest).is_ok_and(|b| format!("{:x}", Sha256::digest(b)) == *digest)
            });
            if !intact {
                let bytes = client.download_statement(access, id).await?;
                let digest = format!("{:x}", Sha256::digest(&bytes));
                atomic_write(&dest, &bytes)?;
                store.conn.execute("INSERT INTO finance_statements VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(env,item_id,id) DO UPDATE SET sha256=excluded.sha256,filename=excluded.filename",params![env,item,id,filename,digest,aid,year,month])?;
                count += 1;
            }
        }
    }
    // Derive the archive index from durable state, including earlier periods.
    let mut body=format!("# Bank statement archive ({env})\n\nOriginal bank PDF copies. This archive contains private financial data.\n\n");
    let mut q=store.conn.prepare("SELECT filename,year,month,sha256 FROM finance_statements WHERE env=?1 ORDER BY year,month,filename")?;
    for row in q.query_map([env], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })? {
        let (file, year, month, digest) = row?;
        body.push_str(&format!(
            "- [{year}-{month:02}]({file}) — SHA-256 `{digest}`\n"
        ));
    }
    atomic_write(&dir.join(format!("statements-{env}.md")), body.as_bytes())?;
    Ok(count)
}
impl FinanceStore {
    pub fn enable_statements(&self, env: &str, item: &str) -> Result<()> {
        self.cursor(env, item)?;
        self.conn.execute(
            "INSERT INTO finance_statement_items(env,item_id) VALUES(?1,?2) ON CONFLICT DO NOTHING",
            params![env, item],
        )?;
        Ok(())
    }
    pub fn statement_items(&self, env: &str) -> Result<Vec<String>> {
        let mut q = self
            .conn
            .prepare("SELECT item_id FROM finance_statement_items WHERE env=?1 ORDER BY item_id")?;
        let rows = q
            .query_map([env], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
