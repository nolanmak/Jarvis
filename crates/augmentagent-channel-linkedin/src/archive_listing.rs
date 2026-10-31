//! Bounded live conversation listing used by the CLI.
use crate::archive::{ArchiveReader, Conversation};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cursor {
    account: String,
    category: String,
    seen: BTreeSet<String>,
    visited: BTreeSet<String>,
    page_ids: Vec<String>,
    provider: Option<String>,
    offset: usize,
}
#[derive(Serialize)]
pub struct ConversationResults {
    pub account_id: String,
    pub schema_version: u32,
    pub conversations: Vec<Conversation>,
    pub next_cursor: Option<String>,
    pub complete: bool,
    pub category: String,
    pub pages: usize,
}

pub async fn recent(
    api: &ArchiveReader,
    limit: usize,
    category: &str,
    cursor: Option<&str>,
    max_pages: usize,
) -> Result<ConversationResults> {
    let mut position = cursor
        .map(serde_json::from_str::<Cursor>)
        .transpose()
        .context("invalid continuation")?
        .unwrap_or_else(|| Cursor {
            account: api.mailbox().into(),
            category: category.into(),
            ..Default::default()
        });
    anyhow::ensure!(
        position.category == category,
        "cursor belongs to another category"
    );
    anyhow::ensure!(
        position.account == api.mailbox(),
        "cursor belongs to another account"
    );
    let mut rows = Vec::new();
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
        let page_ids = page.items.iter().map(|c| c.urn.clone()).collect::<Vec<_>>();
        anyhow::ensure!(
            position.offset == 0 || position.page_ids == page_ids,
            "page changed; restart the listing"
        );
        position.page_ids = page_ids;
        for (i, conv) in page.items.iter().enumerate().skip(position.offset) {
            position.offset = i + 1;
            if position.seen.insert(conv.urn.clone()) {
                rows.push(conv.clone());
            }
            if rows.len() == limit {
                break;
            }
        }
        if position.offset < page.items.len() {
            break;
        }
        position.provider = page.next;
        position.offset = 0;
        position.page_ids.clear();
        if position.provider.is_none() {
            complete = true;
            break;
        }
        anyhow::ensure!(
            position.visited.insert(
                position
                    .provider
                    .clone()
                    .expect("checked nonterminal cursor")
            ),
            "pagination cursor loop"
        );
    }
    let next_cursor = if complete {
        None
    } else {
        Some(serde_json::to_string(&position)?)
    };
    let result = ConversationResults {
        account_id: format!("linkedin:{}", api.mailbox()),
        schema_version: 2,
        conversations: rows,
        next_cursor,
        complete,
        category: category.into(),
        pages,
    };
    Ok(result)
}
