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
    let account = reader.mailbox();
    let mut pages = 0;
    let mut stopped = "page_budget".to_string();
    while pages < max_pages {
        if cancel.is_cancelled() {
            stopped = "cancelled".into();
            break;
        }
        let cp = db::checkpoint(store, account, category)?;
        if !cp.complete {
            if cp.retry_at_ms > now_ms() {
                stopped = "retry_deferred".into();
                break;
            }
            let fetched = tokio::select! {
                _=cancel.cancelled()=>{stopped="cancelled".into();break;},
                p=reader.conversations(category,cp.cursor.as_deref())=>p,
            };
            pages += 1;
            match fetched {
                Ok(page) => {
                    db::commit_conversations(store, account, category, &cp, &page, now_ms())?;
                }
                Err(e) => {
                    // A cursor-bearing request rejected as bad/gone gets one
                    // bounded reconciliation. This is deliberately not applied
                    // to auth, throttling, or malformed successful responses.
                    if matches!(e, ArchiveError::Status(400 | 410))
                        && db::restart_cursor(store, account, category, false, &cp, now_ms())?
                    {
                        continue;
                    }
                    db::record_failure(
                        store,
                        account,
                        category,
                        false,
                        &e.to_string(),
                        retry_at(&e, now_ms()),
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
            let Some((thread, cp)) = pending.into_iter().find(|(_, p)| p.retry_at_ms <= now_ms())
            else {
                stopped = "retry_deferred".into();
                break;
            };
            let fetched = tokio::select! {
                _=cancel.cancelled()=>{stopped="cancelled".into();break;},
                p=reader.messages(&thread,cp.cursor.as_deref())=>p,
            };
            pages += 1;
            match fetched {
                Ok(page) => {
                    db::commit_messages(store, account, &thread, &cp, &page, now_ms())?;
                }
                Err(e) => {
                    if matches!(e, ArchiveError::Status(400 | 410))
                        && db::restart_cursor(store, account, &thread, true, &cp, now_ms())?
                    {
                        continue;
                    }
                    db::record_failure(
                        store,
                        account,
                        &thread,
                        true,
                        &e.to_string(),
                        retry_at(&e, now_ms()),
                    )?;
                    // Session/throttle failures are account-wide. Other failures
                    // leave this thread partial while allowing other threads.
                    if matches!(
                        e,
                        ArchiveError::Auth | ArchiveError::Redirect | ArchiveError::RateLimited(_)
                    ) {
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
        coverage: db::coverage(store, account, category)?,
    })
}
