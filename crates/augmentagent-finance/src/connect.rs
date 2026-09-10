use crate::{store::field, Client, FinanceStore};
use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub trait Vault {
    fn get(&self, key: &str) -> Result<String>;
    fn put(&self, key: &str, value: &str) -> Result<()>;
}
pub struct KeyringVault;
impl Vault for KeyringVault {
    fn get(&self, key: &str) -> Result<String> {
        keyring::Entry::new("augmentagent/finance", key)?
            .get_password()
            .map_err(|_| {
                anyhow::anyhow!("finance credential unavailable; unlock the login keyring")
            })
    }
    fn put(&self, key: &str, value: &str) -> Result<()> {
        keyring::Entry::new("augmentagent/finance", key)?
            .set_password(value)
            .map_err(|_| {
                anyhow::anyhow!("cannot persist finance credential; unlock the login keyring")
            })
    }
}
pub fn access_token(vault: &dyn Vault, env: &str, item: &str) -> Result<String> {
    vault.get(&format!("{env}/item/{item}"))
}
pub struct ConnectOptions<'a> {
    pub alias: &'a str,
    pub countries: &'a [String],
    pub update_item: Option<&'a str>,
    pub statements: bool,
}
pub async fn connect(
    client: &Client,
    store: &FinanceStore,
    vault: &dyn Vault,
    env: &str,
    options: ConnectOptions<'_>,
) -> Result<Value> {
    let ConnectOptions {
        alias,
        countries,
        update_item: update,
        statements,
    } = options;
    if alias.trim().is_empty() || alias.len() > 100 {
        bail!("alias must contain 1–100 bytes");
    }
    if countries.is_empty() || countries.iter().any(|c| !matches!(c.as_str(), "US" | "CA")) {
        bail!("country must be US or CA");
    }
    let probe = uuid::Uuid::new_v4().to_string();
    vault.put("healthcheck", &probe)?;
    if vault.get("healthcheck")? != probe {
        bail!("finance credential store failed persistence check");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut body = json!({"client_name":"Personal finance agent","user":{"client_user_id":"personal-owner"},"language":"en","country_codes":countries,"hosted_link":{},"products":["transactions"],"transactions":{"days_requested":730}});
    if let Some(item) = update {
        store.cursor(env, item)?;
        body.as_object_mut().unwrap().remove("products");
        body.as_object_mut().unwrap().remove("transactions");
        body["access_token"] = json!(access_token(vault, env, item)?);
    }
    if statements {
        let now = chrono::Utc::now().date_naive();
        body["statements"] = json!({"start_date":(now-chrono::Duration::days(730)).to_string(),"end_date":now.to_string()});
        if update.is_some() {
            body["products"] = json!(["statements"]);
        } else {
            body["products"] = json!(["transactions", "statements"]);
        }
    }
    let response = client.call("/link/token/create", body).await?;
    let link = field(&response, "link_token")?;
    let url = field(&response, "hosted_link_url")?;
    let parsed = reqwest::Url::parse(url).context("invalid Hosted Link URL")?;
    if parsed.scheme() != "https" || parsed.host_str() != Some("secure.plaid.com") {
        bail!("unexpected Hosted Link host");
    }
    vault.put(
        &format!("{env}/session/{id}"),
        &json!({"link_token":link,"statements":statements}).to_string(),
    )?;
    store.conn.execute(
        "INSERT INTO finance_sessions(env,id,alias,update_item) VALUES(?1,?2,?3,?4)",
        params![env, id, alias, update],
    )?;
    Ok(
        json!({"session_id":id,"url":url,"expiration":response["expiration"],"next":"Open the URL, authorize your bank, then run finance complete --session <session_id> within six hours."}),
    )
}
pub async fn complete(
    client: &Client,
    store: &FinanceStore,
    vault: &dyn Vault,
    env: &str,
    session: &str,
) -> Result<Value> {
    let (alias, update, done): (String, Option<String>, bool) = store
        .conn
        .query_row(
            "SELECT alias,update_item,complete FROM finance_sessions WHERE env=?1 AND id=?2",
            params![env, session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .context("unknown finance session")?;
    if done {
        return Ok(json!({"status":"already_completed"}));
    }
    let saved: Value = serde_json::from_str(&vault.get(&format!("{env}/session/{session}"))?)?;
    let link = field(&saved, "link_token")?;
    let response = client
        .call("/link/token/get", json!({"link_token":link}))
        .await?;
    let sessions = response["link_sessions"]
        .as_array()
        .context("missing Link sessions")?;
    let mut items = Vec::new();
    if let Some(item) = update {
        // An exit or a finished timestamp alone is not proof of successful consent.
        let succeeded = sessions.iter().any(|s| {
            s.get("on_success").is_some_and(|v| v.is_object())
                || s.pointer("/results/item_update_results")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
        });
        if !succeeded {
            bail!("bank reconnection not complete; finish Hosted Link and retry");
        }
        store.error(env, &item, "linked")?;
        items.push(item);
    } else {
        for s in sessions {
            let Some(results) = s
                .pointer("/results/item_add_results")
                .and_then(Value::as_array)
            else {
                continue;
            };
            for result in results {
                let public = field(result, "public_token")?;
                // Receipt journals the exchange result before registering the Item in SQLite.
                // If DB persistence fails, retry completion without exchanging a consumed token.
                let receipt_id = format!("{:x}", Sha256::digest(public.as_bytes()));
                let receipt_key = format!("{env}/receipt/{session}/{receipt_id}");
                let receipt = match vault.get(&receipt_key) {
                    Ok(raw) => {
                        serde_json::from_str::<Value>(&raw).context("invalid credential receipt")?
                    }
                    Err(_) => {
                        let v = client
                            .call(
                                "/item/public_token/exchange",
                                json!({"public_token":public}),
                            )
                            .await?;
                        field(&v, "item_id")?;
                        field(&v, "access_token")?;
                        vault.put(&receipt_key, &v.to_string())?;
                        v
                    }
                };
                let item = field(&receipt, "item_id")?;
                vault.put(
                    &format!("{env}/item/{item}"),
                    field(&receipt, "access_token")?,
                )?;
                store.add_item(env, item, &alias)?;
                items.push(item.into());
            }
        }
        if items.is_empty() {
            bail!("bank connection not complete; finish Hosted Link and retry within six hours");
        }
    }
    if saved["statements"].as_bool() == Some(true) {
        for item in &items {
            store.enable_statements(env, item)?;
        }
    }
    store.conn.execute(
        "UPDATE finance_sessions SET complete=1 WHERE env=?1 AND id=?2",
        params![env, session],
    )?;
    Ok(json!({"status":"linked","items":items}))
}
