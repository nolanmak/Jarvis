//! #1296 — `augmentagent slack subscribe | set-mode | unsubscribe`: the CLI
//! over [`augmentagent_channel_slack::subscriptions::SubscriptionManager`],
//! the same API the Slack owner command registry (#1292) calls through
//! `subscriptions::run_command`.
//!
//! Targets are a conversation ID, `#name`, a group DM's name, or a person
//! (resolved through `--wiki-dir` people pages). Names are looked up in the
//! workspace's conversation list through its Composio connection; without
//! one, IDs and already-subscribed names still work. An ambiguous or unknown
//! target exits non-zero and changes nothing.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use augmentagent_channel_slack::subscriptions::{ConversationDirectory, SubscriptionManager};
use augmentagent_store::{Store, SubscriptionMode};

/// `--team-id`, or the only connected workspace.
pub fn resolve_team(store: &Store, team_id: Option<String>) -> Result<String> {
    if let Some(t) = team_id {
        return Ok(t);
    }
    let workspaces = store
        .list_active_slack_workspaces()
        .context("list slack workspaces")?;
    match workspaces.as_slice() {
        [w] => Ok(w.team_id.clone()),
        [] => anyhow::bail!(
            "no slack workspaces connected — run `augmentagent slack login` or connect via dashboard"
        ),
        _ => anyhow::bail!("multiple slack workspaces connected — pass --team-id <T...>"),
    }
}

fn parse_mode(mode: &str) -> Result<SubscriptionMode> {
    SubscriptionMode::parse(mode).ok_or_else(|| anyhow::anyhow!("invalid mode: {mode}"))
}

async fn with_manager<T>(
    store: &Store,
    team_id: Option<String>,
    wiki_root: Option<PathBuf>,
    f: impl AsyncFnOnce(&SubscriptionManager<'_>) -> Result<T>,
) -> Result<T> {
    let team = resolve_team(store, team_id)?;
    let client = crate::load_single_slack_client(store, Some(&team));
    let manager = SubscriptionManager::new(store, &team).with_wiki_root(wiki_root);
    match &client {
        Some(c) => {
            let directory: &dyn ConversationDirectory = c.as_ref();
            f(&manager.with_directory(directory)).await
        }
        None => f(&manager).await,
    }
}

pub async fn subscribe(
    store: Arc<Store>,
    target: String,
    mode: String,
    name: Option<String>,
    team_id: Option<String>,
    wiki_root: Option<PathBuf>,
) -> Result<()> {
    let mode = parse_mode(&mode)?;
    let change = with_manager(&store, team_id, wiki_root, async |m| {
        Ok(m.subscribe(&target, mode, name.as_deref()).await?)
    })
    .await?;
    println!("{}", change.describe());
    let s = change.subscription();
    println!(
        "subscription id={} platform={} channel_id={} mode={} name={} account_id={}",
        s.id,
        s.platform,
        s.channel_id,
        s.mode.as_str(),
        s.display_name,
        s.account_id.as_deref().unwrap_or("-"),
    );
    Ok(())
}

pub async fn set_mode(
    store: Arc<Store>,
    target: String,
    mode: String,
    team_id: Option<String>,
    wiki_root: Option<PathBuf>,
) -> Result<()> {
    let mode = parse_mode(&mode)?;
    let change = with_manager(&store, team_id, wiki_root, async |m| {
        Ok(m.set_mode(&target, mode).await?)
    })
    .await?;
    println!("{}", change.describe());
    Ok(())
}

pub async fn unsubscribe(
    store: Arc<Store>,
    target: String,
    team_id: Option<String>,
    wiki_root: Option<PathBuf>,
) -> Result<()> {
    // A subscription row ID (the original form of this command) works
    // without a workspace, as before.
    if let Some(sub) = store
        .get_subscription(target.trim())
        .context("read subscription")?
        .filter(|s| s.platform == augmentagent_channel_slack::PLATFORM)
    {
        store
            .delete_subscription(&sub.id)
            .context("delete subscription")?;
        println!("subscription {} deactivated", sub.id);
        return Ok(());
    }
    let change = with_manager(&store, team_id, wiki_root, async |m| {
        Ok(m.unsubscribe(&target).await?)
    })
    .await?;
    println!("{}", change.describe());
    Ok(())
}
