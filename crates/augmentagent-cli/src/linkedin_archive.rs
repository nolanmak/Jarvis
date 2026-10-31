//! Explicit archive/read commands. No broker or reasoner is constructed.
use anyhow::{Context, Result};
use augmentagent_channel_linkedin::{
    archive::{ArchiveReader, Conversation},
    archive_store as db, archive_sync, LinkedInAuth,
};
use augmentagent_store::Store;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, sync::Arc};
use tokio_util::sync::CancellationToken;

fn reader() -> Result<ArchiveReader> {
    let auth = LinkedInAuth::load_with_migration(&std::env::current_dir()?)?;
    Ok(ArchiveReader::new(auth)?)
}
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    category: String,
    provider: Option<String>,
    offset: usize,
}
#[derive(Serialize)]
struct ConversationResults {
    schema_version: u32,
    conversations: Vec<Conversation>,
    next_cursor: Option<String>,
    complete: bool,
    category: String,
    pages: usize,
}

pub async fn recent(
    limit: usize,
    json: bool,
    category: &str,
    cursor: Option<&str>,
    max_pages: usize,
) -> Result<()> {
    let api = reader()?;
    let mut position = cursor
        .map(serde_json::from_str::<Cursor>)
        .transpose()
        .context("invalid continuation")?
        .unwrap_or_else(|| Cursor {
            category: category.into(),
            ..Default::default()
        });
    anyhow::ensure!(
        position.category == category,
        "cursor belongs to another category"
    );
    let mut rows = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut pages = 0;
    let mut complete = false;
    while rows.len() < limit && pages < max_pages {
        let page = api
            .conversations(category, position.provider.as_deref())
            .await?;
        pages += 1;
        anyhow::ensure!(
            position.offset <= page.items.len(),
            "page changed; restart the listing"
        );
        for (i, conv) in page.items.iter().enumerate().skip(position.offset) {
            position.offset = i + 1;
            if seen.insert(conv.urn.clone()) {
                rows.push(conv.clone());
            }
            if rows.len() == limit {
                break;
            }
        }
        if position.offset < page.items.len() {
            break;
        }
        position = Cursor {
            category: category.into(),
            provider: page.next,
            offset: 0,
        };
        if position.provider.is_none() {
            complete = true;
            break;
        }
        anyhow::ensure!(
            cursors.insert(position.provider.clone()),
            "pagination cursor loop"
        );
    }
    let next_cursor = if complete {
        None
    } else {
        Some(serde_json::to_string(&position)?)
    };
    let result = ConversationResults {
        schema_version: 2,
        conversations: rows,
        next_cursor,
        complete,
        category: category.into(),
        pages,
    };
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
                &serde_json::json!({"schema_version":1,"conversation":conversation,"messages":messages,"next_cursor":next,"complete":complete,"pages":pages})
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
    let api = reader()?;
    let categories = match category {
        Some(c) => vec![c.to_string()],
        None => api.categories().await?,
    };
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let task = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal.cancel();
        }
    });
    let mut reports = Vec::new();
    let mut remaining = max_pages;
    let result:Result<()>=async{
        for category in &categories{
            if refresh{db::refresh_mailbox(&store,api.mailbox(),category)?;}
            let report=archive_sync::sync(&api,&store,category,remaining,&cancel).await?;
            remaining=remaining.saturating_sub(report.pages);reports.push(report);
        }
        augmentagent_messages::index::drain(&store,100,std::time::Duration::from_millis(10))?;
        for (report,category) in reports.iter_mut().zip(&categories){report.coverage=db::coverage(&store,api.mailbox(),category)?;}
        println!("{}",serde_json::to_string_pretty(&serde_json::json!({"schema_version":1,"archive_only":true,"categories":categories,"complete":reports.iter().all(|r|r.coverage.complete),"reports":reports}))?);
        Ok(())
    }.await;
    task.abort();
    result
}

pub fn status(store: &Store, category: &str) -> Result<()> {
    let api = reader()?;
    println!(
        "{}",
        serde_json::to_string_pretty(&db::coverage(store, api.mailbox(), category)?)?
    );
    Ok(())
}
