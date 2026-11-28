use crate::{
    access_token, export_wiki, refresh_statements, sync_item, sync_statements, Client,
    FinanceStore, Vault,
};
use anyhow::{Context, Result};
use std::path::Path;

/// Rebuilding the knowledge-base index lives in the CLI crate, so import
/// takes it as a hook rather than depending on it.
pub type Reindex<'a> = &'a dyn Fn(&FinanceStore, &Path) -> Result<()>;

/// Fetch and export everything for the selected Items, or for all of them
/// when `only` is None. Returns how many connections failed: each failure is
/// reported and recorded in `finance status` instead of aborting the rest.
pub async fn import_items(
    client: &Client,
    store: &mut FinanceStore,
    vault: &dyn Vault,
    env: &str,
    wiki: Option<&Path>,
    reindex: Reindex<'_>,
    only: Option<&[String]>,
) -> Result<usize> {
    let selected = |id: &str| only.is_none_or(|list| list.iter().any(|wanted| wanted == id));
    let ids = store
        .status(env)?
        .iter()
        .map(|item| {
            Ok(item["item_id"]
                .as_str()
                .context("missing item id")?
                .to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    let mut failed = 0;
    for id in ids.iter().filter(|id| selected(id)) {
        let result = match access_token(vault, env, id) {
            Ok(access) => sync_item(client, store, env, id, &access).await,
            Err(e) => {
                store.error(env, id, "CREDENTIAL_UNAVAILABLE")?;
                Err(e)
            }
        };
        if let Err(e) = result {
            failed += 1;
            eprintln!("finance sync failed: {e}");
        }
    }
    if let Some(root) = wiki {
        for id in store.statement_items(env)? {
            if !selected(&id) {
                continue;
            }
            let result = async {
                let access = access_token(vault, env, &id)?;
                sync_statements(client, store, env, &id, &access, root).await?;
                refresh_statements(client, store, env, &id, &access).await?;
                Ok::<(), anyhow::Error>(())
            }
            .await;
            if let Err(e) = result {
                failed += 1;
                store.error(env, &id, "STATEMENTS_FAILED")?;
                eprintln!("finance statements failed: {e}");
            }
        }
        export_wiki(store, env, root)?;
        reindex(store, root)?;
    }
    Ok(failed)
}
