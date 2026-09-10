//! Read-only bank ingestion with durable cursors and derived knowledge pages.
mod client;
mod store;
use anyhow::{bail, Context, Result};
pub use client::{ApiError, Client};
use serde_json::{json, Value};
pub use store::{date, FinanceStore};

pub async fn sync_item(
    client: &Client,
    store: &mut FinanceStore,
    env: &str,
    item: &str,
    access: &str,
) -> Result<()> {
    let result = sync_inner(client, store, env, item, access).await;
    if let Err(e) = &result {
        store.error(
            env,
            item,
            e.downcast_ref::<ApiError>()
                .map(|e| e.code.as_str())
                .unwrap_or("SYNC_FAILED"),
        )?;
    }
    result
}
async fn sync_inner(
    client: &Client,
    store: &mut FinanceStore,
    env: &str,
    item: &str,
    access: &str,
) -> Result<()> {
    let original = store.cursor(env, item)?;
    for _ in 0..3 {
        let mut cursor = original.clone();
        let mut pages: Vec<Value> = vec![];
        loop {
            let result = client
                .call(
                    "/transactions/sync",
                    json!({"access_token":access,"cursor":cursor,"count":500}),
                )
                .await;
            let page = match result {
                Err(e)
                    if e.downcast_ref::<ApiError>().is_some_and(|e| {
                        e.code == "TRANSACTIONS_SYNC_MUTATION_DURING_PAGINATION"
                    }) =>
                {
                    break
                }
                other => other?,
            };
            let next = store::field(&page, "next_cursor")?.to_owned();
            let more = page["has_more"].as_bool().context("missing has_more")?;
            if more && (next == cursor || pages.len() >= 1000) {
                bail!("Plaid pagination did not converge");
            }
            cursor = next;
            pages.push(page);
            if !more {
                return store.apply(env, item, &pages);
            }
        }
    }
    bail!("Plaid pagination kept changing; retry next sync")
}

mod connect;
pub use connect::{access_token, complete, connect, ConnectOptions, KeyringVault, Vault};

mod export;
pub use export::{atomic_write, export_wiki};

mod statements;
pub use statements::{refresh_statements, sync_statements};
