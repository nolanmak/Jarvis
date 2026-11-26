//! Explicit archive/read commands. No broker or reasoner is constructed.
use anyhow::Result;
use augmentagent_channel_linkedin::{
    archive::ArchiveReader,
    archive_store as db, archive_sync,
    backfill_pace::{Admit, Pacer, Pacing},
    LinkedInAuth,
};
use augmentagent_store::Store;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;

pub(crate) fn reader() -> Result<ArchiveReader> {
    let auth = LinkedInAuth::load_with_migration(&std::env::current_dir()?)?;
    Ok(ArchiveReader::new(auth)?)
}
pub async fn recent_output(
    api: &ArchiveReader,
    limit: usize,
    json: bool,
    category: &str,
    cursor: Option<&str>,
    max_pages: usize,
) -> Result<String> {
    let result = augmentagent_channel_linkedin::archive_listing::recent(
        api, limit, category, cursor, max_pages,
    )
    .await?;
    if json {
        return Ok(format!("{}\n", serde_json::to_string_pretty(&result)?));
    }
    let mut output = String::new();
    for c in &result.conversations {
        output.push_str(&format!(
            "{}\n  {} [{}]\n  participants: {}\n\n",
            c.urn,
            c.title,
            if c.group { "group" } else { "direct" },
            c.participants
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    output.push_str(&format!(
        "{} conversations; complete={}; category={}\n",
        result.conversations.len(),
        result.complete,
        category
    ));
    if let Some(cursor) = result.next_cursor {
        output.push_str(&format!("next_cursor: {cursor}\n"));
    }
    Ok(output)
}

pub async fn thread_output(
    api: &ArchiveReader,
    urn: &str,
    cursor: Option<&str>,
    max_pages: usize,
    json: bool,
) -> Result<String> {
    let conversation = api.conversation(urn).await?;
    anyhow::ensure!(
        conversation
            .participants
            .iter()
            .any(|p| p.urn == api.mailbox()),
        "conversation is not in this account"
    );
    let mut next = cursor.map(str::to_string);
    let mut messages = BTreeMap::new();
    let mut cursors = BTreeSet::new();
    let mut pages = 0;
    let mut complete = false;
    while pages < max_pages {
        let page = api.messages(urn, next.as_deref()).await?;
        pages += 1;
        for m in page.items {
            if let Some(previous) = messages.get(&m.urn) {
                anyhow::ensure!(
                    previous == &m,
                    "conflicting message copies across history pages; restart the read"
                );
            } else {
                messages.insert(m.urn.clone(), m);
            }
        }
        next = page.next;
        if next.is_none() {
            complete = true;
            break;
        }
        anyhow::ensure!(cursors.insert(next.clone()), "pagination cursor loop");
    }
    let mut messages: Vec<_> = messages.into_values().collect();
    messages.sort_by(|a, b| (a.delivered_at_ms, &a.urn).cmp(&(b.delivered_at_ms, &b.urn)));
    if json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(
                &serde_json::json!({"schema_version":1,"account_id":format!("linkedin:{}",api.mailbox()),"conversation":conversation,"messages":messages,"next_cursor":next,"complete":complete,"pages":pages})
            )?
        ));
    }
    let mut output = String::new();
    for m in &messages {
        output.push_str(&format!(
            "{} {} {}\n{}\n\n",
            m.delivered_at_ms,
            if m.from_me { "owner" } else { "inbound" },
            m.sender,
            m.body
        ));
    }
    output.push_str(&format!(
        "complete={complete}; next_cursor={}\n",
        next.as_deref().unwrap_or("")
    ));
    Ok(output)
}

/// Name the archived participant behind a `/messaging/thread/<id>/` URL
/// (#2503). Offline and read-only; no request is issued and nothing is written.
///
/// Off unless `enabled`, and the credential is read only once it is: a
/// default-off run must explain the opt-in even on a host that never logged in.
pub fn thread_url_output(store: &Store, url: &str, json: bool, enabled: bool) -> Result<String> {
    let account = if enabled {
        // The member urn only names the archive scope to search, so it is read
        // without promoting or repairing the session it came from.
        LinkedInAuth::load_read_only(&std::env::current_dir()?)?.member_urn
    } else {
        String::new()
    };
    augmentagent_channel_linkedin::resolve_thread_url_report(store, &account, url, json, enabled)
}

/// Command-line pacing. Jitter spans one more delay, so requests land
/// between one and two delays apart.
pub fn pacing(page_delay_secs: u32, hourly_requests: u32, daily_requests: u32) -> Pacing {
    let delay_ms = i64::from(page_delay_secs) * 1000;
    Pacing {
        delay_ms,
        jitter_ms: delay_ms,
        hourly: i64::from(hourly_requests),
        daily: i64::from(daily_requests),
    }
}

pub async fn backfill(
    store: Arc<Store>,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    pacing: Pacing,
) -> Result<()> {
    run_backfill(store, category, max_pages, refresh, false, pacing).await
}

pub async fn dry_run(
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    pacing: Pacing,
) -> Result<()> {
    // This entry point runs before normal CLI bootstrap opens the disk store.
    // Its request ledger is in memory too: the delay and budget hold within
    // this run only.
    run_backfill(
        Arc::new(Store::open(":memory:")?),
        category,
        max_pages,
        refresh,
        true,
        pacing,
    )
    .await
}

async fn run_backfill(
    store: Arc<Store>,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    dry_run: bool,
    pacing: Pacing,
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
    let pacer = Pacer::new(pacing);
    let result = backfill_paced(
        &api,
        &store,
        category,
        max_pages,
        refresh,
        dry_run,
        &cancel,
        Some(&pacer),
    )
    .await;
    task.abort();
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

/// Shared command implementation; callers choose durable or in-memory storage.
/// Unpaced: the `backfill` command itself always goes through a [`Pacer`].
pub async fn backfill_with_reader(
    api: &ArchiveReader,
    store: &Store,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    dry_run: bool,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    backfill_paced(api, store, category, max_pages, refresh, dry_run, cancel, None).await
}

/// With a pacer every request, category discovery included, waits for the
/// page delay and the account's request budget. A spent budget ends the run
/// with `stopped: "request_budget"` and the time it may resume.
#[allow(clippy::too_many_arguments)]
pub async fn backfill_paced(
    api: &ArchiveReader,
    store: &Store,
    category: Option<&str>,
    max_pages: usize,
    refresh: bool,
    dry_run: bool,
    cancel: &CancellationToken,
    pacer: Option<&Pacer>,
) -> Result<serde_json::Value> {
    let mut remaining = max_pages;
    let mut stopped: Option<String> = None;
    let mut resume_at_ms: Option<i64> = None;
    let discovery = match (category, pacer) {
        (None, Some(pacer)) if remaining > 0 => pacer.admit(store, api.mailbox(), cancel).await?,
        _ => Admit::Go,
    };
    let categories = match category {
        Some(c) => vec![c.to_string()],
        None if remaining == 0 => Vec::new(),
        None if discovery != Admit::Go => {
            if let Admit::Budget { resume_at_ms: at } = discovery {
                resume_at_ms = Some(at);
                stopped = Some("request_budget".into());
            } else {
                stopped = Some("cancelled".into());
            }
            Vec::new()
        }
        None => {
            let discovered = match pacer {
                Some(pacer) => {
                    archive_sync::categories_paced(api, store, cancel, pacer).await
                }
                None => archive_sync::categories(api, store, cancel).await,
            };
            let categories = discovered?
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
        let report = match pacer {
            Some(pacer) => {
                archive_sync::sync_paced(api, store, category, remaining, cancel, pacer).await?
            }
            None => archive_sync::sync(api, store, category, remaining, cancel).await?,
        };
        remaining = remaining.saturating_sub(report.pages);
        let deferred = report.account_deferred;
        stopped = Some(report.stopped.clone());
        resume_at_ms = report.resume_at_ms;
        reports.push(report);
        if deferred || resume_at_ms.is_some() || cancel.is_cancelled() {
            break;
        }
    }
    // The shared queue can include a large backlog from unrelated channels.
    // Keep this command finite even while other writers continuously enqueue.
    if max_pages > 0 {
        for _ in 0..10 {
            tokio::task::yield_now().await;
            if cancel.is_cancelled() {
                break;
            }
            let indexed = augmentagent_messages::index::drain_batch(store, 100)?;
            if indexed.remaining == 0 || indexed.indexed + indexed.removed == 0 {
                break;
            }
        }
    }
    for (report, category) in reports.iter_mut().zip(&categories) {
        report.coverage = db::coverage(store, api.mailbox(), category)?;
    }
    let mut receipt = serde_json::json!({"schema_version":1,"dry_run":dry_run,"persistent_writes":!dry_run,"archive_only":true,"scope":"all_accessible_history","categories":categories,"complete":!categories.is_empty() && reports.len()==categories.len() && reports.iter().all(|r|r.coverage.complete),"pages":max_pages-remaining,"reports":reports});
    // Why the run ended, at the top level so a budget stop before category
    // discovery (no reports) is still explained.
    if let Some(stopped) = stopped {
        receipt["stopped"] = stopped.into();
    }
    if let Some(at) = resume_at_ms {
        receipt["resume_at_ms"] = at.into();
        if let Ok(utc) = time_utc(at) {
            receipt["resume_at"] = utc.into();
        }
    }
    Ok(receipt)
}

fn time_utc(ms: i64) -> Result<String> {
    Ok(chrono::DateTime::from_timestamp_millis(ms)
        .ok_or_else(|| anyhow::anyhow!("timestamp out of range"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
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
    let threads = store.with_conn(|c| {
        augmentagent_store::linkedin_archive::thread_status(
            c,
            category,
            augmentagent_messages::EXTRACTOR_VERSION,
        )
    })?;
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"schema_version":1,"category":category,"coverage":coverage,"threads":threads})
        )?
    );
    Ok(())
}
