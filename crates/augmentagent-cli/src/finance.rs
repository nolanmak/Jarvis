use anyhow::{bail, Context, Result};
use augmentagent_finance::{
    access_token, complete, connect, export_wiki, refresh_statements, sync_item, sync_statements,
    Client, ConnectOptions, FinanceStore, KeyringVault,
};
use clap::{Args, Subcommand};
use serde_json::json;
use std::{fs::OpenOptions, path::Path};

#[derive(Subcommand)]
pub enum FinanceCommand {
    /// Verify production/sandbox application credentials without linking a bank.
    Check,
    /// Create a Hosted Link URL; authorize your bank there, then run complete.
    Connect {
        #[arg(long)]
        alias: String,
        #[arg(long, value_delimiter = ',', default_value = "US,CA")]
        countries: Vec<String>,
        /// Reauthorize an existing Item without creating another connection.
        #[arg(long)]
        update_item: Option<String>,
        /// Also request bank PDF statements; requires institution support.
        #[arg(long)]
        statements: bool,
    },
    /// Recover a successful Hosted Link result and persist its credentials.
    Complete {
        #[arg(long)]
        session: String,
    },
    /// Fetch changes for every linked account and export when --wiki-dir is set.
    Sync,
    /// Regenerate KB records from the local database without contacting Plaid.
    Export,
    /// Show account connection health and data freshness. No credentials required.
    Status,
    /// Query local transaction records (JSON); inclusive date bounds.
    Transactions(Query),
    /// Compute exact totals by currency, excluding pending and classified transfers.
    Summary(Query),
}
#[derive(Args)]
pub struct Query {
    #[arg(long)]
    pub account: Option<String>,
    #[arg(long)]
    pub start: Option<String>,
    #[arg(long)]
    pub end: Option<String>,
}
pub async fn run(op: &FinanceCommand, db: &Path, wiki: Option<&Path>) -> Result<()> {
    let env = std::env::var("PLAID_ENV").unwrap_or_else(|_| "production".into());
    if !matches!(env.as_str(), "production" | "sandbox") {
        bail!("PLAID_ENV must be production or sandbox");
    }
    // Local read commands do not load bank credentials or touch the keyring.
    let read = matches!(
        op,
        FinanceCommand::Status
            | FinanceCommand::Transactions(_)
            | FinanceCommand::Summary(_)
            | FinanceCommand::Check
    );
    let _lock = if !read {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let f = opts.open(db.with_extension("finance.lock"))?;
        f.try_lock()
            .context("another finance operation is running")?;
        Some(f)
    } else {
        None
    };
    let mut store = FinanceStore::open(db)?;
    let value = match op {
        FinanceCommand::Status => json!({"environment":env,"items":store.status(&env)?}),
        FinanceCommand::Transactions(q) => {
            json!({"environment":env,"freshness":store.status(&env)?,"transactions":store.transactions(&env,q.account.as_deref(),q.start.as_deref(),q.end.as_deref())?})
        }
        FinanceCommand::Summary(q) => {
            json!({"environment":env,"freshness":store.status(&env)?,"totals":store.summary(&env,q.account.as_deref(),q.start.as_deref(),q.end.as_deref())?,"policy":"Exclude pending, TRANSFER_IN, TRANSFER_OUT and LOAN_PAYMENTS; unclassified transfers may remain. Positive net_outflow means net money out."})
        }
        FinanceCommand::Export => {
            json!({"pages":export(&store,&env,wiki.context("--wiki-dir is required for finance export")?)?})
        }
        FinanceCommand::Check => {
            Client::from_env(&env)?
                .call(
                    "/institutions/get",
                    json!({"count":1,"offset":0,"country_codes":["US","CA"]}),
                )
                .await?;
            json!({"environment":env,"credentials":"valid","note":"This does not verify bank connection or product access."})
        }
        FinanceCommand::Connect {
            alias,
            countries,
            update_item,
            statements,
        } => {
            connect(
                &Client::from_env(&env)?,
                &store,
                &KeyringVault,
                &env,
                ConnectOptions {
                    alias,
                    countries,
                    update_item: update_item.as_deref(),
                    statements: *statements,
                },
            )
            .await?
        }
        FinanceCommand::Complete { session } => {
            complete(
                &Client::from_env(&env)?,
                &store,
                &KeyringVault,
                &env,
                session,
            )
            .await?
        }
        FinanceCommand::Sync => {
            let items = store.status(&env)?;
            let client = Client::from_env(&env)?;
            let mut failed = 0;
            for item in items {
                let id = item["item_id"].as_str().context("missing item id")?;
                let result = match access_token(&KeyringVault, &env, id) {
                    Ok(access) => sync_item(&client, &mut store, &env, id, &access).await,
                    Err(e) => {
                        store.error(&env, id, "CREDENTIAL_UNAVAILABLE")?;
                        Err(e)
                    }
                };
                if let Err(e) = result {
                    failed += 1;
                    eprintln!("finance sync failed: {e}");
                }
            }
            if let Some(root) = wiki {
                for id in store.statement_items(&env)? {
                    let result = async {
                        let access = access_token(&KeyringVault, &env, &id)?;
                        sync_statements(&client, &store, &env, &id, &access, root).await?;
                        refresh_statements(&client, &store, &env, &id, &access).await?;
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    if let Err(e) = result {
                        failed += 1;
                        eprintln!("finance statements failed: {e}");
                    }
                }
                export(&store, &env, root)?;
            }
            if failed > 0 {
                bail!("{failed} finance connection(s) failed; see finance status");
            }
            json!({"environment":env,"items":store.status(&env)?})
        }
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
fn export(store: &FinanceStore, env: &str, root: &Path) -> Result<usize> {
    let pages = export_wiki(store, env, root)?;
    augmentagent_wiki::rebuild_index(root, &|id| store.source_time(id))?;
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        op: FinanceCommand,
    }
    #[test]
    fn cli_parses_read_filters_and_reconnect() {
        assert!(Cli::try_parse_from([
            "finance",
            "summary",
            "--start",
            "2026-09-01",
            "--end",
            "2026-09-30"
        ])
        .is_ok());
        assert!(Cli::try_parse_from([
            "finance",
            "connect",
            "--alias",
            "Household",
            "--update-item",
            "item"
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["finance", "complete"]).is_err());
    }
    #[tokio::test]
    async fn local_queries_work_without_api_keys_and_reject_bad_dates() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("data.db");
        run(&FinanceCommand::Status, &db, None).await.unwrap();
        let bad = FinanceCommand::Summary(Query {
            account: None,
            start: Some("invalid".into()),
            end: None,
        });
        assert!(run(&bad, &db, None).await.is_err());
    }
}
