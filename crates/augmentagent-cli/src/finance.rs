use anyhow::{bail, Context, Result};
use augmentagent_finance::{
    complete, connect, export_wiki, import_items, poll_session, Client, ConnectOptions,
    FinanceStore, KeyringVault, SessionOutcome,
};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};
use tracing::{info, warn};

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
    /// One completion-worker pass; `serve` runs this on a timer.
    #[command(hide = true)]
    WorkerTick,
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
fn environment() -> Result<String> {
    let env = std::env::var("PLAID_ENV").unwrap_or_else(|_| "production".into());
    if !matches!(env.as_str(), "production" | "sandbox") {
        bail!("PLAID_ENV must be production or sandbox");
    }
    Ok(env)
}
fn lock_file(db: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    Ok(opts.open(db.with_extension("finance.lock"))?)
}
pub async fn run(op: &FinanceCommand, db: &Path, wiki: Option<&Path>) -> Result<()> {
    let env = environment()?;
    // The worker attempts the same lock itself and skips a busy tick instead
    // of failing, so it must not go through the blocking acquisition below.
    if matches!(op, FinanceCommand::WorkerTick) {
        println!(
            "{}",
            serde_json::to_string_pretty(&worker_tick(db, wiki).await?)?
        );
        return Ok(());
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
        let f = lock_file(db)?;
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
            let client = Client::from_env(&env)?;
            let mut result = complete(&client, &store, &KeyringVault, &env, session).await?;
            // A new connection is worthless until its history is in the
            // database, so import before reporting success.
            let linked = items_of(&result);
            if !linked.is_empty() {
                let failed = import(&client, &mut store, &env, wiki, Some(&linked)).await?;
                if failed > 0 {
                    bail!("{failed} finance connection(s) failed; see finance status");
                }
                report_duplicates(&store, &env, &linked)?;
                result["freshness"] = json!(store.status(&env)?);
            }
            result
        }
        FinanceCommand::Sync => {
            let failed = import(&Client::from_env(&env)?, &mut store, &env, wiki, None).await?;
            if failed > 0 {
                bail!("{failed} finance connection(s) failed; see finance status");
            }
            json!({"environment":env,"items":store.status(&env)?})
        }
        FinanceCommand::WorkerTick => unreachable!("worker tick runs before the finance lock"),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
fn reindex(store: &FinanceStore, root: &Path) -> Result<()> {
    augmentagent_wiki::rebuild_index(root, &|id| store.source_time(id))?;
    Ok(())
}
fn export(store: &FinanceStore, env: &str, root: &Path) -> Result<usize> {
    let pages = export_wiki(store, env, root)?;
    reindex(store, root)?;
    Ok(pages)
}
fn items_of(result: &Value) -> Vec<String> {
    result["items"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}
/// The shared sync/export loop, bound to this crate's wiki index rebuild.
async fn import(
    client: &Client,
    store: &mut FinanceStore,
    env: &str,
    wiki: Option<&Path>,
    only: Option<&[String]>,
) -> Result<usize> {
    import_items(client, store, &KeyringVault, env, wiki, &reindex, only).await
}
/// Second connections to an already linked institution are kept and reported;
/// nothing is ever unlinked on our own initiative.
fn report_duplicates(store: &FinanceStore, env: &str, items: &[String]) -> Result<()> {
    for (item, existing) in store.duplicates(env)? {
        if items.contains(&item) {
            warn!("finance item {item} duplicates already linked connection {existing}; kept for owner review");
        }
    }
    Ok(())
}
const WORKER_INTERVAL: Duration = Duration::from_secs(15);
/// One pass of the completion worker: retire expired sessions, poll the due
/// ones, then import whatever linked (plus any history re-sync now due).
/// Skipped while a `finance` command holds the lock.
pub async fn worker_tick(db: &Path, wiki: Option<&Path>) -> Result<Value> {
    let env = environment()?;
    let handle = lock_file(db)?;
    if handle.try_lock().is_err() {
        return Ok(json!({"status":"skipped","reason":"another finance operation is running"}));
    }
    let _lock = handle;
    let mut store = FinanceStore::open(db)?;
    let now = chrono::Utc::now();
    for id in store.expire_sessions(&env, now)? {
        info!("finance session {id} expired before the bank was authorized");
    }
    let due = store.due_sessions(&env, now)?;
    let mut items = store.history_retries_due(&env, now)?;
    if due.is_empty() && items.is_empty() {
        return Ok(json!({"status":"idle"}));
    }
    let client = Client::from_env(&env)?;
    let polled = due.len();
    for session in due {
        // Marked before the call so a failing session still waits its turn.
        store.mark_polled(&env, &session.id, now)?;
        match poll_session(&client, &store, &KeyringVault, &env, &session.id).await {
            Ok(SessionOutcome::Linked { items: linked }) => {
                info!("finance session {} linked {} item(s)", session.id, linked.len());
                items.extend(linked);
            }
            Ok(SessionOutcome::Expired) => {
                info!("finance session {} expired", session.id)
            }
            Ok(SessionOutcome::Exited | SessionOutcome::Pending) => {}
            Err(e) => warn!("finance session {} could not be polled: {e:#}", session.id),
        }
    }
    items.sort();
    items.dedup();
    if items.is_empty() {
        return Ok(json!({"status":"polled","sessions":polled}));
    }
    let failed = import(&client, &mut store, &env, wiki, Some(&items)).await?;
    if failed > 0 {
        warn!("{failed} finance connection(s) failed to import; see finance status");
    }
    report_duplicates(&store, &env, &items)?;
    Ok(json!({"status":"imported","sessions":polled,"items":items,"failed":failed}))
}
/// The daemon's loop. Every failure only logs: a bank, keyring or network
/// problem must not end `serve`. A tick runs on the blocking pool with its
/// own current-thread runtime because the SQLite connection it holds across
/// awaits is not `Sync`.
pub async fn worker_loop(
    db: PathBuf,
    wiki: Option<PathBuf>,
    shutdown: tokio_util::sync::CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return Ok(()),
            _ = tokio::time::sleep(WORKER_INTERVAL) => {}
        }
        let (db, wiki) = (db.clone(), wiki.clone());
        let tick = tokio::task::spawn_blocking(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(worker_tick(&db, wiki.as_deref()))
        })
        .await;
        match tick {
            Ok(Ok(result)) if result["status"] == "imported" => {
                info!("finance completion worker: {result}")
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!("finance completion worker tick failed: {e:#}"),
            Err(e) => warn!("finance completion worker task failed: {e}"),
        }
    }
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
