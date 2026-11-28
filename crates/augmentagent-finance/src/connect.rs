use crate::{
    store::{field, instant},
    Client, FinanceStore,
};
use anyhow::{bail, Context, Result};
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
    let mut body = json!({"client_name":"Jarvis","user":{"client_user_id":"personal-owner"},"language":"en","country_codes":countries,"hosted_link":{},"products":["transactions"],"transactions":{"days_requested":730}});
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
    // Without a usable expiry the session could be polled forever, so refuse
    // to create it rather than record one that never retires.
    let expires_at = instant(field(&response, "expiration")?)?;
    vault.put(
        &format!("{env}/session/{id}"),
        &json!({"link_token":link,"statements":statements}).to_string(),
    )?;
    store.add_session(
        env,
        &id,
        alias,
        update,
        expires_at,
        response["request_id"].as_str(),
    )?;
    Ok(
        json!({"session_id":id,"url":url,"expiration":response["expiration"],"next":"Open the URL, authorize your bank, then run finance complete --session <session_id> within six hours."}),
    )
}
/// What a Hosted Link session is currently worth doing something about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    /// Not authorized yet; poll again until the Link token expires.
    Pending,
    /// Hosted Link closed without successful consent; the URL still works.
    Exited,
    /// Consent succeeded; these Items now have credentials in the vault.
    Linked { items: Vec<String> },
    /// The Link token expired; no public token is exchanged.
    Expired,
}
pub async fn poll_session(
    client: &Client,
    store: &FinanceStore,
    vault: &dyn Vault,
    env: &str,
    session: &str,
) -> Result<SessionOutcome> {
    let record = store.session(env, session)?;
    if record.complete {
        return Ok(SessionOutcome::Linked { items: Vec::new() });
    }
    // Expiry decides before any token is loaded, so an expired session never
    // reaches the exchange endpoint.
    if let Some(expires) = record.expires_at.as_deref() {
        if chrono::Utc::now() >= instant(expires)? {
            store.expire_session(env, session)?;
            return Ok(SessionOutcome::Expired);
        }
    }
    let saved: Value = serde_json::from_str(&vault.get(&format!("{env}/session/{session}"))?)?;
    let link = field(&saved, "link_token")?;
    let response = client
        .call("/link/token/get", json!({"link_token":link}))
        .await?;
    let Some(sessions) = response["link_sessions"].as_array() else {
        return Ok(SessionOutcome::Pending);
    };
    let mut items = Vec::new();
    if let Some(item) = record.update_item.as_deref() {
        // An exit or a finished timestamp alone is not proof of successful consent.
        let succeeded = sessions
            .iter()
            .any(|s| s.get("on_success").is_some_and(|v| v.is_object()));
        if !succeeded {
            return Ok(unauthorized(sessions));
        }
        store.error(env, item, "linked")?;
        items.push(item.to_owned());
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
                store.add_item(env, item, &record.alias)?;
                if let Some(institution) = result
                    .pointer("/institution/institution_id")
                    .and_then(Value::as_str)
                {
                    store.set_institution(env, item, institution)?;
                    // A second connection to the same bank is kept and flagged;
                    // only the owner may unlink one.
                    if let Some(existing) = store
                        .institution_items(env, institution, item)?
                        .first()
                        .map(String::as_str)
                    {
                        store.flag_duplicate(env, item, existing)?;
                    }
                }
                items.push(item.into());
            }
        }
        if items.is_empty() {
            return Ok(unauthorized(sessions));
        }
    }
    if saved["statements"].as_bool() == Some(true) {
        for item in &items {
            store.enable_statements(env, item)?;
        }
    }
    store.finish_session(env, session)?;
    Ok(SessionOutcome::Linked { items })
}
// Hosted Link that closed on its own is distinguishable from one never
// opened; neither is terminal, so the session stays pending either way.
fn unauthorized(sessions: &[Value]) -> SessionOutcome {
    if sessions
        .iter()
        .any(|s| s.get("on_exit").is_some() || s.get("finished_at").is_some())
    {
        SessionOutcome::Exited
    } else {
        SessionOutcome::Pending
    }
}
pub async fn complete(
    client: &Client,
    store: &FinanceStore,
    vault: &dyn Vault,
    env: &str,
    session: &str,
) -> Result<Value> {
    if store.session(env, session)?.complete {
        return Ok(json!({"status":"already_completed"}));
    }
    match poll_session(client, store, vault, env, session).await? {
        SessionOutcome::Linked { items } => Ok(json!({"status":"linked","items":items})),
        SessionOutcome::Pending => {
            bail!("bank connection not complete; open Hosted Link and authorize your bank, then retry")
        }
        SessionOutcome::Exited => {
            bail!("bank connection not complete; Hosted Link closed without authorizing — reopen the URL and retry")
        }
        SessionOutcome::Expired => {
            bail!("finance session expired; run finance connect to start a new one")
        }
    }
}
