//! Explicit archive/read commands. No broker or reasoner is constructed.
use anyhow::Result;
use augmentagent_channel_linkedin::{
    archive::ArchiveReader, archive_store as db, archive_sync, LinkedInAuth,
};
use augmentagent_store::Store;
use std::{collections::BTreeSet, sync::Arc};
use tokio_util::sync::CancellationToken;

fn reader() -> Result<ArchiveReader> {
    let auth = LinkedInAuth::load_with_migration(&std::env::current_dir()?)?;
    Ok(ArchiveReader::new(auth)?)
}
pub async fn recent(
    limit: usize,
    json: bool,
    category: &str,
    cursor: Option<&str>,
    max_pages: usize,
) -> Result<()> {
    let api = reader()?;
    let result = augmentagent_channel_linkedin::archive_listing::recent(
        &api, limit, category, cursor, max_pages,
    )
    .await?;
    let complete = result.complete;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        for c in &result.conversations {
            println!(
                "{}\n  {} [{}]\n  participants: {}\n",
                c.urn,
                c.title,
                if c.group { "group" } else { "direct" },
                c.participants
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        println!(
            "{} conversations; complete={}; category={}",
            result.conversations.len(),
            complete,
            category
        );
        if let Some(cursor) = result.next_cursor {
            println!("next_cursor: {cursor}");
        }
    }
    Ok(())
}

pub async fn thread(urn: &str, cursor: Option<&str>, max_pages: usize, json: bool) -> Result<()> {
    let api = reader()?;
    let conversation = api.conversation(urn).await?;
    anyhow::ensure!(
        conversation
            .participants
            .iter()
            .any(|p| p.urn == api.mailbox()),
        "conversation is not in this account"
    );
    let mut next = cursor.map(str::to_string);
    let mut messages = Vec::new();
    let mut ids = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut pages = 0;
    let mut complete = false;
    while pages < max_pages {
        let page = api.messages(urn, next.as_deref()).await?;
        pages += 1;
        for m in page.items {
            if ids.insert(m.urn.clone()) {
                messages.push(m);
            }
        }
        next = page.next;
        if next.is_none() {
            complete = true;
            break;
        }
        anyhow::ensure!(cursors.insert(next.clone()), "pagination cursor loop");
    }
    messages.sort_by(|a, b| (a.delivered_at_ms, &a.urn).cmp(&(b.delivered_at_ms, &b.urn)));
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"schema_version":1,"account_id":format!("linkedin:{}",api.mailbox()),"conversation":conversation,"messages":messages,"next_cursor":next,"complete":complete,"pages":pages})
            )?
        );
    } else {
        for m in &messages {
            println!(
                "{} {} {}\n{}\n",
                m.delivered_at_ms,
                if m.from_me { "owner" } else { "inbound" },
                m.sender,
                m.body
            );
        }
        println!(
            "complete={complete}; next_cursor={}",
            next.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

pub async fn backfill(
    store: Arc<Store>,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
) -> Result<()> {
    run_backfill(store, category, max_pages, refresh, false).await
}

pub async fn dry_run(category: Option<&str>, max_pages: usize, refresh: bool) -> Result<()> {
    // This entry point runs before normal CLI bootstrap opens the disk store.
    run_backfill(
        Arc::new(Store::open(":memory:")?),
        category,
        max_pages,
        refresh,
        true,
    )
    .await
}

async fn run_backfill(
    store: Arc<Store>,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    dry_run: bool,
) -> Result<()> {
    if max_pages == 0 {
        println!(
            "{}",
            serde_json::json!({"schema_version":1,"dry_run":dry_run,"persistent_writes":!dry_run,"archive_only":true,"scope":"all_accessible_history","categories":category.into_iter().collect::<Vec<_>>(),"complete":false,"pages":0,"stopped":"page_budget","reports":[]})
        );
        return Ok(());
    }
    let api = if dry_run {
        ArchiveReader::new(LinkedInAuth::load_read_only(&std::env::current_dir()?)?)?
    } else {
        reader()?
    };
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let result =
        backfill_with_reader(&api, &store, category, max_pages, refresh, dry_run, &cancel).await;
    task.abort();
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

/// Shared command implementation; callers choose durable or in-memory storage.
pub async fn backfill_with_reader(
    api: &ArchiveReader,
    store: &Store,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    dry_run: bool,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    let mut remaining = max_pages;
    let categories = match category {
        Some(c) => vec![c.to_string()],
        None if remaining == 0 => Vec::new(),
        None => {
            let categories = archive_sync::categories(api, store, cancel).await?
                .ok_or_else(|| anyhow::anyhow!("LinkedIn account reads deferred or cancelled; inspect archive-status before retrying"))?;
            remaining -= 1;
            categories
        }
    };
    let mut reports = Vec::new();
    for category in &categories {
        if refresh && remaining > 0 && !cancel.is_cancelled() {
            db::refresh_mailbox(store, api.mailbox(), category)?;
        }
        let report = archive_sync::sync(api, store, category, remaining, cancel).await?;
        remaining = remaining.saturating_sub(report.pages);
        let deferred = report.account_deferred;
        reports.push(report);
        if deferred || cancel.is_cancelled() {
            break;
        }
    }
    augmentagent_messages::index::drain(store, 100, std::time::Duration::from_millis(10))?;
    for (report, category) in reports.iter_mut().zip(&categories) {
        report.coverage = db::coverage(store, api.mailbox(), category)?;
    }
    Ok(
        serde_json::json!({"schema_version":1,"dry_run":dry_run,"persistent_writes":!dry_run,"archive_only":true,"scope":"all_accessible_history","categories":categories,"complete":!categories.is_empty() && reports.len()==categories.len() && reports.iter().all(|r|r.coverage.complete),"pages":max_pages-remaining,"reports":reports}),
    )
}

pub fn status(store: &Store, category: &str) -> Result<()> {
    let mut coverage = store.with_conn(|c| {
        augmentagent_store::linkedin_archive::search_coverage(
            c,
            augmentagent_messages::EXTRACTOR_VERSION,
        )
    })?;
    coverage.scopes.retain(|scope| scope.category == category);
    if coverage.scopes.is_empty() {
        coverage.state = "not_synced";
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema_version":1,"category":category,"coverage":coverage})
        )?
    );
    Ok(())
}
