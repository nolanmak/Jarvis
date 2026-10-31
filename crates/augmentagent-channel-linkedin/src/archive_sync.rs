//! Bounded, resumable archival reads. Intentionally no triage/send dependency.
use crate::{
    archive::{ArchiveError, ArchiveReader},
    archive_store::{self as db, Coverage},
};
use augmentagent_store::Store;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Serialize)]
pub struct SyncReport {
    pub pages: usize,
    pub stopped: String,
    pub account_deferred: bool,
    pub coverage: Coverage,
}
fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}
fn retry_at(error: &ArchiveError, now: i64) -> i64 {
    let seconds = match error {
        ArchiveError::RateLimited(s) => *s,
        _ => 60,
    };
    now.saturating_add(seconds.saturating_mul(1000).min(i64::MAX as u64) as i64)
}

pub async fn sync(
    reader: &ArchiveReader,
    store: &Store,
    category: &str,
    max_pages: usize,
    cancel: &CancellationToken,
) -> anyhow::Result<SyncReport> {
    sync_with_clock(reader, store, category, max_pages, cancel, &now_ms).await
}

/// Clock injection for deterministic retry/recovery tests; production uses UTC.
#[doc(hidden)]
pub async fn sync_with_clock(
    reader: &ArchiveReader,
    store: &Store,
    category: &str,
    max_pages: usize,
    cancel: &CancellationToken,
    now: &(dyn Fn() -> i64 + Sync),
) -> anyhow::Result<SyncReport> {
    let account = reader.mailbox();
    let mut pages = 0;
    let mut account_deferred = false;
    let mut stopped = "page_budget".to_string();
    while pages < max_pages {
        if cancel.is_cancelled() {
            stopped = "cancelled".into();
            break;
        }
        let request_started = now();
        if db::account_retry_at(store, account)? > request_started {
            stopped = "retry_deferred".into();
            account_deferred = true;
            break;
        }
        let cp = db::checkpoint(store, account, category)?;
        if !cp.complete {
            if cp.retry_at_ms > now() {
                stopped = "retry_deferred".into();
                break;
            }
            pages += 1;
            let fetched = tokio::select! {
                _=cancel.cancelled()=>{stopped="cancelled".into();break;},
                p=reader.conversations(category,cp.cursor.as_deref())=>p,
            };
            match fetched {
                Ok(page) => {
                    db::clear_account_failure(store, account, request_started)?;
                    db::commit_conversations(store, account, category, &cp, &page, now())?;
                }
                Err(e) => {
                    account_deferred = defer_account(store, account, &e, now())?;
                    // A cursor-bearing request rejected as bad/gone gets one
                    // bounded reconciliation. This is deliberately not applied
                    // to auth, throttling, or malformed successful responses.
                    if matches!(e, ArchiveError::Status(400 | 410))
                        && db::restart_cursor(store, account, category, false, &cp, now())?
                    {
                        continue;
                    }
                    db::record_failure(
                        store,
                        account,
                        category,
                        false,
                        &e.to_string(),
                        retry_at(&e, now()),
                    )?;
                    stopped = e.to_string();
                    break;
                }
            }
        } else {
            let pending = db::pending_threads(store, account, category)?;
            if pending.is_empty() {
                stopped = "history_exhausted".into();
                break;
            }
            let Some((thread, cp)) = pending.into_iter().find(|(_, p)| p.retry_at_ms <= now())
            else {
                stopped = "retry_deferred".into();
                break;
            };
            pages += 1;
            let fetched = tokio::select! {
                _=cancel.cancelled()=>{stopped="cancelled".into();break;},
                p=reader.messages(&thread,cp.cursor.as_deref())=>p,
            };
            match fetched {
                Ok(page) => {
                    db::clear_account_failure(store, account, request_started)?;
                    db::commit_messages(store, account, &thread, &cp, &page, now())?;
                }
                Err(e) => {
                    account_deferred = defer_account(store, account, &e, now())?;
                    if matches!(e, ArchiveError::Status(400 | 410))
                        && db::restart_cursor(store, account, &thread, true, &cp, now())?
                    {
                        continue;
                    }
                    db::record_failure(
                        store,
                        account,
                        &thread,
                        true,
                        &e.to_string(),
                        retry_at(&e, now()),
                    )?;
                    // Session/throttle failures are account-wide. Other failures
                    // leave this thread partial while allowing other threads.
                    if account_deferred {
                        stopped = e.to_string();
                        break;
                    }
                }
            }
        }
    }
    Ok(SyncReport {
        pages,
        stopped,
        account_deferred,
        coverage: db::coverage(store, account, category)?,
    })
}

fn defer_account(
    store: &Store,
    account: &str,
    error: &ArchiveError,
    now: i64,
) -> anyhow::Result<bool> {
    let wide = matches!(
        error,
        ArchiveError::Auth | ArchiveError::Redirect | ArchiveError::RateLimited(_)
    );
    if wide {
        db::record_account_failure(store, account, &error.to_string(), retry_at(error, now))?;
    }
    Ok(wide)
}

/// Inventory obeys the same durable account deferral as mailbox/history reads.
/// None means deferred or cancelled, never an empty accessible mailbox.
pub async fn categories(
    reader: &ArchiveReader,
    store: &Store,
    cancel: &CancellationToken,
) -> anyhow::Result<Option<Vec<String>>> {
    let started = now_ms();
    if cancel.is_cancelled() || db::account_retry_at(store, reader.mailbox())? > started {
        return Ok(None);
    }
    let fetched = tokio::select! {
        _=cancel.cancelled()=>return Ok(None),
        result=reader.categories()=>result,
    };
    match fetched {
        Ok(categories) => {
            db::clear_account_failure(store, reader.mailbox(), started)?;
            Ok(Some(categories))
        }
        Err(error) => {
            // Inventory is required for every implicit-scope poll. Persist all
            // its failures, including malformed responses and transient I/O.
            db::record_account_failure(
                store,
                reader.mailbox(),
                &error.to_string(),
                retry_at(&error, now_ms()),
            )?;
            Err(error.into())
        }
    }
}
